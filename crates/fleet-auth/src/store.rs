// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Postgres-backed API key store with data-defined roles (ADR-0006).
//!
//! Construction does NOT run migrations — apply them via [`crate::migrate`]
//! from the operational tool that owns deploys (e.g. `fleet-admin migrate`).

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::Row as _;
use sqlx::postgres::{PgPool, PgRow};
use tokio::sync::Semaphore;

use crate::cache::{VerificationCache, VerificationCacheKey, VerificationCacheStats};
use crate::error::AuthError;
use crate::token;
use crate::types::{ApiKeyInfo, CreatedKey, PrincipalKind, Role, RolePermission, VerifiedKey};
use crate::validation::{validate_app_namespace, validate_permission, validate_role_name};

/// How many argon2id checks may run at once in this process (ADR-0054).
const ARGON2_CHECK_LIMIT: usize = 4;

static ARGON2_CHECKS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(ARGON2_CHECK_LIMIT)));

async fn hash_token_async(plaintext: &str) -> Result<String, AuthError> {
    let plaintext = plaintext.to_owned();
    on_blocking_pool(move || token::hash_token(&plaintext)).await
}

async fn verify_token_async(plaintext: &str, hash: &str) -> Result<bool, AuthError> {
    let plaintext = plaintext.to_owned();
    let hash = hash.to_owned();
    on_blocking_pool(move || token::verify_token(&plaintext, &hash)).await
}

/// Run the timing-equalization check of a prefix miss on the blocking pool.
///
/// [`token::DUMMY_HASH`] is dereferenced inside the closure, so its
/// first-use hash also runs on the blocking pool under a permit.
async fn verify_dummy_async(plaintext: &str) -> Result<bool, AuthError> {
    let plaintext = plaintext.to_owned();
    on_blocking_pool(move || token::verify_token(&plaintext, &token::DUMMY_HASH)).await
}

/// A worker that did not return answers fixed text. `JoinError`'s
/// `Display` quotes a panic payload, and the middleware logs this error on
/// the bearer request path, so the join error is never formatted.
async fn on_blocking_pool<T, F>(f: F) -> Result<T, AuthError>
where
    F: FnOnce() -> Result<T, AuthError> + Send + 'static,
    T: Send + 'static,
{
    // Nothing closes the semaphore, so the error arm is unreachable.
    let permit = Arc::clone(&ARGON2_CHECKS)
        .acquire_owned()
        .await
        .map_err(|_| AuthError::Hash("argon2 limit is closed".to_owned()))?;
    let step = move || {
        let _permit = permit;
        f()
    };
    tokio::task::spawn_blocking(step).await.map_err(|e| {
        let what = if e.is_panic() {
            "argon2 worker panicked"
        } else {
            "argon2 worker was cancelled"
        };
        AuthError::Hash(what.to_owned())
    })?
}

/// A transaction sqlx begins counts only once Postgres has answered its
/// `BEGIN`. A request dropped while that answer is on its way leaves the
/// session inside a transaction no one ends, and the pool's own return
/// check accepts it. So the pool closes a session still inside a
/// transaction.
///
/// A failed transaction refuses the query with `in_failed_sql_transaction`,
/// and is inside one too.
async fn released_outside_transaction(
    conn: &mut sqlx::postgres::PgConnection,
) -> Result<bool, sqlx::Error> {
    let outside = match sqlx::raw_sql("SELECT transaction_timestamp() = statement_timestamp()")
        .fetch_one(&mut *conn)
        .await
    {
        Ok(row) => row.try_get(0)?,
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("25P02") => false,
        Err(e) => return Err(e),
    };
    if !outside {
        tracing::warn!(
            event_type = "pool_connection_returned_in_transaction",
            pool = "keystore",
            "a connection came back to the pool inside a transaction; it is closed, not reused"
        );
    }
    Ok(outside)
}

#[derive(Clone, Debug)]
pub struct KeyStore {
    pool: PgPool,
    cache: VerificationCache,
    verifying: Arc<Semaphore>,
}

impl KeyStore {
    pub fn from_pool(pool: PgPool) -> Self {
        Self::with_cache(pool, VerificationCache::new())
    }

    pub(crate) fn with_cache(pool: PgPool, cache: VerificationCache) -> Self {
        let connections = pool.options().get_max_connections() as usize;
        Self {
            pool,
            cache,
            verifying: Arc::new(Semaphore::new(connections)),
        }
    }

    pub async fn connect(database_url: &str) -> Result<Self, AuthError> {
        const MAX_CONNECTIONS: u32 = 8;

        // KeyStore::connect is the named production pool owner for the
        // fleet keystore (ADR-0021 ruling 3).
        #[allow(clippy::disallowed_methods)]
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(MAX_CONNECTIONS)
            .after_release(|conn, _| Box::pin(released_outside_transaction(conn)))
            .connect(database_url)
            .await?;
        crate::validate_schema(&pool).await?;
        Ok(Self::from_pool(pool))
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub fn cache_stats(&self) -> VerificationCacheStats {
        self.cache.stats()
    }

    pub async fn ping(&self) -> Result<(), AuthError> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    pub async fn create_key(
        &self,
        name: &str,
        kind: PrincipalKind,
        role_names: &[String],
        expires_in: Option<Duration>,
    ) -> Result<CreatedKey, AuthError> {
        const MAX_RETRIES: usize = 3;

        for role in role_names {
            validate_role_name(role)?;
        }

        let mut seen = std::collections::HashSet::new();
        for role in role_names {
            if !seen.insert(role) {
                return Err(AuthError::RoleAlreadyAssigned {
                    prefix: "(new key)".to_owned(),
                    role: role.clone(),
                });
            }
        }

        let mut role_ids = Vec::with_capacity(role_names.len());
        for role in role_names {
            role_ids.push(self.get_role_id(role).await?);
        }

        // Set both timestamps client-side so they share the same wall-clock
        // reading. Without this, Postgres-side NOW() for `created_at` can
        // land microseconds after the Rust-side `Utc::now() + d` for very
        // short expiries, tripping the `api_keys_expiry_after_create` CHECK.
        let created_at = Utc::now();
        let expires_at: Option<DateTime<Utc>> = expires_in
            .map(|d| {
                let delta = chrono::Duration::from_std(d).map_err(|_| {
                    AuthError::InvalidExpiry(format!(
                        "expiry duration {d:?} exceeds chrono::Duration range (~292 years)"
                    ))
                })?;
                Ok::<_, AuthError>(created_at + delta)
            })
            .transpose()?;

        for _ in 0..MAX_RETRIES {
            let generated = token::generate_token();
            let hash = hash_token_async(&generated.plaintext).await?;

            let mut tx = self.pool.begin().await?;

            let insert_result = sqlx::query(
                "INSERT INTO api_keys (prefix, name, hash, kind, created_at, expires_at)
                 VALUES ($1, $2, $3, $4, $5, $6)
                 RETURNING id",
            )
            .bind(&generated.prefix)
            .bind(name)
            .bind(&hash)
            .bind(kind.as_str())
            .bind(created_at)
            .bind(expires_at)
            .fetch_one(&mut *tx)
            .await;

            let row = match insert_result {
                Ok(row) => row,
                Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23505") => {
                    drop(tx);
                    continue;
                }
                Err(e) => return Err(e.into()),
            };

            let id: i64 = row.try_get("id")?;

            for role_id in &role_ids {
                sqlx::query("INSERT INTO key_roles (key_id, role_id) VALUES ($1, $2)")
                    .bind(id)
                    .bind(role_id)
                    .execute(&mut *tx)
                    .await?;
            }

            tx.commit().await?;

            let mut sorted_roles = role_names.to_vec();
            sorted_roles.sort_unstable();

            tracing::info!(
                event_type = "key_created",
                key_id = id,
                prefix = %generated.prefix,
                name,
                kind = %kind,
                roles = %sorted_roles.join(","),
                "API key created"
            );

            return Ok(CreatedKey {
                info: ApiKeyInfo {
                    id,
                    prefix: generated.prefix.clone(),
                    name: name.to_owned(),
                    kind,
                    roles: sorted_roles,
                    active: true,
                    created_at,
                    expires_at,
                    last_used: None,
                    revoked_at: None,
                },
                plaintext_token: generated.plaintext.clone(),
            });
        }

        Err(AuthError::TokenGeneration(
            "failed to generate unique token prefix after retries".into(),
        ))
    }

    /// Every failure surfaces as [`AuthError::InvalidKey`] (with the same opaque
    /// message) so callers cannot distinguish "not found", "wrong hash",
    /// "revoked", or "expired" — that distinction would be an enumeration
    /// oracle.
    ///
    /// The `last_used` UPDATE and the role resolution run as one
    /// transaction on a task of its own, which this call awaits. A caller
    /// dropped meanwhile, as when a client resets its connection, leaves
    /// the task to finish, so the connection returns outside any transaction.
    pub async fn verify_key(&self, plaintext: &str) -> Result<VerifiedKey, AuthError> {
        let prefix = token::extract_prefix(plaintext).ok_or_else(|| {
            AuthError::MalformedToken("token must start with flt_ and be at least 12 chars".into())
        })?;

        let row_opt: Option<PgRow> = sqlx::query(
            "SELECT id, prefix, name, hash, kind, active, expires_at
             FROM api_keys
             WHERE prefix = $1",
        )
        .bind(prefix)
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row_opt else {
            // SECURITY: equalize timing with the hit path so prefix existence
            // doesn't leak via response time.
            let _ = verify_dummy_async(plaintext).await;
            return Err(AuthError::InvalidKey("authentication failed".into()));
        };

        let id: i64 = row.try_get("id")?;
        let db_prefix: String = row.try_get("prefix")?;
        let name: String = row.try_get("name")?;
        let stored_hash: String = row.try_get("hash")?;
        let kind_str: String = row.try_get("kind")?;
        let active: bool = row.try_get("active")?;
        let expires_at: Option<DateTime<Utc>> = row.try_get("expires_at")?;

        let cache_key = VerificationCacheKey::build(prefix, plaintext, &stored_hash);

        // Only the request that ran the KDF may insert. A cache hit skips
        // the KDF and must leave the cache as it found it: re-inserting on
        // a hit would turn a lookup into a use, and the eviction order is
        // insertion order (see the cache module docs).
        let ran_kdf = !self.cache.contains(&cache_key);
        if ran_kdf {
            let hash_valid = verify_token_async(plaintext, &stored_hash).await?;
            if !hash_valid {
                return Err(AuthError::InvalidKey("authentication failed".into()));
            }
        }

        // Liveness checks always run, even on cache hit. The cache only
        // skips the KDF, never the active/expiry/last_used logic.
        let now = Utc::now();
        let is_expired = expires_at.is_some_and(|exp| now > exp);
        if !active || is_expired {
            return Err(AuthError::InvalidKey("authentication failed".into()));
        }

        let roles = self.record_use(id, stored_hash).await?;

        let kind: PrincipalKind = kind_str.parse()?;

        // Insert only after this request ran the KDF and then passed every
        // database check, the hash predicate included, so the entry is for
        // a hash a row still holds.
        if ran_kdf {
            self.cache.insert(cache_key);
        }

        Ok(VerifiedKey::from_roles(id, db_prefix, name, kind, roles))
    }

    async fn record_use(&self, id: i64, stored_hash: String) -> Result<Vec<Role>, AuthError> {
        // Nothing closes the semaphore, so the error arm is unreachable.
        let permit = Arc::clone(&self.verifying)
            .acquire_owned()
            .await
            .map_err(|_| AuthError::Database(sqlx::Error::PoolClosed))?;
        let pool = self.pool.clone();
        tokio::spawn(async move {
            let _permit = permit;
            Self::use_and_roles(&pool, id, &stored_hash).await
        })
        .await
        .map_err(|_| AuthError::Database(sqlx::Error::WorkerCrashed))?
    }

    async fn use_and_roles(
        pool: &PgPool,
        id: i64,
        stored_hash: &str,
    ) -> Result<Vec<Role>, AuthError> {
        // The UPDATE rechecks liveness, requires the hash to be the one this
        // check verified against (a rotation committed since the SELECT
        // in `verify_key` would otherwise authenticate a plaintext the new
        // hash was never asked about), and locks the api_keys row until
        // commit. Role assign/unassign operations lock the same row before
        // mutating key_roles, so the role resolution below observes a role
        // set that cannot change underneath this in-flight verify. Under
        // READ COMMITTED a blocked UPDATE re-evaluates its WHERE on the row
        // version that won, which is what makes the hash predicate a
        // fail-closed check rather than a stale read.
        let mut tx = pool.begin().await?;
        let updated = sqlx::query(
            "UPDATE api_keys
             SET last_used = NOW()
             WHERE id = $1
               AND hash = $2
               AND active = TRUE
               AND (expires_at IS NULL OR expires_at > NOW())
             RETURNING id",
        )
        .bind(id)
        .bind(stored_hash)
        .fetch_optional(&mut *tx)
        .await?;

        if updated.is_none() {
            return Err(AuthError::InvalidKey("authentication failed".into()));
        }

        let roles = Self::load_key_roles(&mut *tx, id).await?;
        tx.commit().await?;
        Ok(roles)
    }

    /// Unlike [`verify_key`], this does NOT update `last_used` — liveness
    /// polling is not a use of the credential.
    ///
    /// [`verify_key`]: Self::verify_key
    pub async fn get_live_key_by_id(&self, id: i64) -> Result<Option<VerifiedKey>, AuthError> {
        let row_opt: Option<PgRow> = sqlx::query(
            "SELECT id, prefix, name, kind
             FROM api_keys
             WHERE id = $1
               AND active = TRUE
               AND (expires_at IS NULL OR expires_at > NOW())",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row_opt else {
            return Ok(None);
        };

        let kind_str: String = row.try_get("kind")?;
        let kind: PrincipalKind = kind_str.parse()?;
        let roles = Self::load_key_roles(&self.pool, id).await?;

        Ok(Some(VerifiedKey::from_roles(
            id,
            row.try_get::<String, _>("prefix")?,
            row.try_get::<String, _>("name")?,
            kind,
            roles,
        )))
    }

    /// A single statement so the resolution is one consistent read (one
    /// statement, one snapshot under READ COMMITTED).
    async fn load_key_roles<'e, E>(executor: E, key_id: i64) -> Result<Vec<Role>, AuthError>
    where
        E: sqlx::PgExecutor<'e>,
    {
        let rows = sqlx::query(
            "SELECT r.id AS role_id, r.name, r.rate_rpm, rp.app, rp.permission
             FROM key_roles kr
             JOIN roles r ON r.id = kr.role_id
             LEFT JOIN role_permissions rp ON rp.role_id = r.id
             WHERE kr.key_id = $1
             ORDER BY r.name, rp.app, rp.permission",
        )
        .bind(key_id)
        .fetch_all(executor)
        .await?;

        rows_to_roles(rows)
    }

    async fn load_key_role_names(&self, key_id: i64) -> Result<Vec<String>, AuthError> {
        let rows = sqlx::query(
            "SELECT r.name
             FROM key_roles kr
             JOIN roles r ON r.id = kr.role_id
             WHERE kr.key_id = $1
             ORDER BY r.name",
        )
        .bind(key_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| Ok(row.try_get("name")?))
            .collect()
    }

    pub async fn list_keys(&self, active_only: bool) -> Result<Vec<ApiKeyInfo>, AuthError> {
        let sql = if active_only {
            "SELECT id, prefix, name, kind, active, created_at, expires_at, last_used, revoked_at
             FROM api_keys
             WHERE active = TRUE
             ORDER BY created_at DESC"
        } else {
            "SELECT id, prefix, name, kind, active, created_at, expires_at, last_used, revoked_at
             FROM api_keys
             ORDER BY created_at DESC"
        };

        let rows = sqlx::query(sql).fetch_all(&self.pool).await?;
        let mut keys: Vec<ApiKeyInfo> = rows
            .iter()
            .map(row_to_api_key_info_no_roles)
            .collect::<Result<Vec<_>, _>>()?;

        for key in &mut keys {
            key.roles = self.load_key_role_names(key.id).await?;
        }

        Ok(keys)
    }

    pub async fn get_key_by_prefix(&self, prefix: &str) -> Result<ApiKeyInfo, AuthError> {
        let row_opt = sqlx::query(
            "SELECT id, prefix, name, kind, active, created_at, expires_at, last_used, revoked_at
             FROM api_keys
             WHERE prefix = $1",
        )
        .bind(prefix)
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row_opt else {
            return Err(AuthError::KeyNotFound {
                prefix: prefix.to_owned(),
            });
        };

        let mut info = row_to_api_key_info_no_roles(&row)?;
        info.roles = self.load_key_role_names(info.id).await?;
        Ok(info)
    }

    pub async fn revoke_key(&self, prefix: &str) -> Result<ApiKeyInfo, AuthError> {
        let updated = sqlx::query(
            "UPDATE api_keys
             SET active = FALSE, revoked_at = NOW()
             WHERE prefix = $1 AND active = TRUE",
        )
        .bind(prefix)
        .execute(&self.pool)
        .await?
        .rows_affected();

        if updated == 0 {
            return Err(self.classify_prefix_miss(prefix).await?);
        }

        tracing::info!(event_type = "key_revoked", prefix, "API key revoked");
        self.get_key_by_prefix(prefix).await
    }

    pub async fn retype_key(
        &self,
        prefix: &str,
        kind: PrincipalKind,
    ) -> Result<ApiKeyInfo, AuthError> {
        let updated =
            sqlx::query("UPDATE api_keys SET kind = $1 WHERE prefix = $2 AND revoked_at IS NULL")
                .bind(kind.as_str())
                .bind(prefix)
                .execute(&self.pool)
                .await?
                .rows_affected();

        if updated == 0 {
            return Err(self.classify_prefix_miss(prefix).await?);
        }

        tracing::info!(event_type = "key_retyped", prefix, kind = %kind, "key kind updated");
        self.get_key_by_prefix(prefix).await
    }

    /// Locks the key row first, so an in-flight `verify_key` on the same key
    /// serializes against this mutation.
    pub async fn assign_role(&self, prefix: &str, role_name: &str) -> Result<(), AuthError> {
        validate_role_name(role_name)?;
        let mut tx = self.pool.begin().await?;
        let key_id = self.lock_active_key_id_by_prefix(&mut tx, prefix).await?;
        let role_id = Self::get_role_id_in(&mut *tx, role_name).await?;

        let result = sqlx::query("INSERT INTO key_roles (key_id, role_id) VALUES ($1, $2)")
            .bind(key_id)
            .bind(role_id)
            .execute(&mut *tx)
            .await;

        match result {
            Ok(_) => {
                tx.commit().await?;
                tracing::info!(
                    event_type = "role_assigned",
                    prefix,
                    role = role_name,
                    "role assigned to key"
                );
                Ok(())
            }
            Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23505") => {
                Err(AuthError::RoleAlreadyAssigned {
                    prefix: prefix.to_owned(),
                    role: role_name.to_owned(),
                })
            }
            Err(e) => Err(e.into()),
        }
    }

    pub async fn unassign_role(&self, prefix: &str, role_name: &str) -> Result<(), AuthError> {
        validate_role_name(role_name)?;
        let mut tx = self.pool.begin().await?;
        let key_id = self.lock_active_key_id_by_prefix(&mut tx, prefix).await?;
        let role_id = Self::get_role_id_in(&mut *tx, role_name).await?;

        let removed = sqlx::query("DELETE FROM key_roles WHERE key_id = $1 AND role_id = $2")
            .bind(key_id)
            .bind(role_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();

        if removed == 0 {
            return Err(AuthError::RoleNotAssigned {
                prefix: prefix.to_owned(),
                role: role_name.to_owned(),
            });
        }

        tx.commit().await?;

        tracing::info!(
            event_type = "role_unassigned",
            prefix,
            role = role_name,
            "role removed from key"
        );
        Ok(())
    }

    pub async fn create_role(
        &self,
        name: &str,
        rate_rpm: Option<u32>,
        permissions: &[RolePermission],
    ) -> Result<Role, AuthError> {
        validate_role_name(name)?;
        for rp in permissions {
            validate_app_namespace(&rp.app)?;
            validate_permission(&rp.permission)?;
        }

        let mut tx = self.pool.begin().await?;
        let insert = sqlx::query("INSERT INTO roles (name, rate_rpm) VALUES ($1, $2) RETURNING id")
            .bind(name)
            .bind(rate_rpm.map(i64::from))
            .fetch_one(&mut *tx)
            .await;

        let row = match insert {
            Ok(row) => row,
            Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23505") => {
                return Err(AuthError::RoleExists {
                    name: name.to_owned(),
                });
            }
            Err(e) => return Err(e.into()),
        };
        let id: i64 = row.try_get("id")?;

        for rp in permissions {
            sqlx::query(
                "INSERT INTO role_permissions (role_id, app, permission)
                 VALUES ($1, $2, $3)
                 ON CONFLICT DO NOTHING",
            )
            .bind(id)
            .bind(&rp.app)
            .bind(&rp.permission)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;

        tracing::info!(
            event_type = "role_created",
            role = name,
            rate_rpm,
            permission_count = permissions.len(),
            "role created"
        );

        self.get_role(name).await
    }

    pub async fn get_role(&self, name: &str) -> Result<Role, AuthError> {
        let rows = sqlx::query(
            "SELECT r.id AS role_id, r.name, r.rate_rpm, rp.app, rp.permission
             FROM roles r
             LEFT JOIN role_permissions rp ON rp.role_id = r.id
             WHERE r.name = $1
             ORDER BY rp.app, rp.permission",
        )
        .bind(name)
        .fetch_all(&self.pool)
        .await?;

        let mut roles = rows_to_roles(rows)?;
        roles.pop().ok_or_else(|| AuthError::RoleNotFound {
            name: name.to_owned(),
        })
    }

    pub async fn list_roles(&self) -> Result<Vec<Role>, AuthError> {
        let rows = sqlx::query(
            "SELECT r.id AS role_id, r.name, r.rate_rpm, rp.app, rp.permission
             FROM roles r
             LEFT JOIN role_permissions rp ON rp.role_id = r.id
             ORDER BY r.name, rp.app, rp.permission",
        )
        .fetch_all(&self.pool)
        .await?;
        rows_to_roles(rows)
    }

    pub async fn count_role_assignments(&self, name: &str) -> Result<i64, AuthError> {
        let role_id = self.get_role_id(name).await?;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM key_roles WHERE role_id = $1")
            .bind(role_id)
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }

    pub async fn set_role_rate_rpm(
        &self,
        name: &str,
        rate_rpm: Option<u32>,
    ) -> Result<Role, AuthError> {
        validate_role_name(name)?;
        let updated = sqlx::query("UPDATE roles SET rate_rpm = $1 WHERE name = $2")
            .bind(rate_rpm.map(i64::from))
            .bind(name)
            .execute(&self.pool)
            .await?
            .rows_affected();

        if updated == 0 {
            return Err(AuthError::RoleNotFound {
                name: name.to_owned(),
            });
        }

        tracing::info!(
            event_type = "role_rate_rpm_set",
            role = name,
            rate_rpm,
            "role rate ceiling updated"
        );
        self.get_role(name).await
    }

    pub async fn delete_role(&self, name: &str, force: bool) -> Result<(), AuthError> {
        validate_role_name(name)?;
        let mut tx = self.pool.begin().await?;
        let role_id = Self::get_role_id_in(&mut *tx, name).await?;

        let key_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM key_roles WHERE role_id = $1")
                .bind(role_id)
                .fetch_one(&mut *tx)
                .await?;

        if key_count > 0 {
            if !force {
                return Err(AuthError::RoleInUse {
                    name: name.to_owned(),
                    key_count,
                });
            }
            sqlx::query("DELETE FROM key_roles WHERE role_id = $1")
                .bind(role_id)
                .execute(&mut *tx)
                .await?;
        }

        sqlx::query("DELETE FROM roles WHERE id = $1")
            .bind(role_id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;

        tracing::info!(
            event_type = "role_deleted",
            role = name,
            key_count,
            forced = force,
            "role deleted"
        );
        Ok(())
    }

    pub async fn add_role_permissions(
        &self,
        name: &str,
        permissions: &[RolePermission],
    ) -> Result<Role, AuthError> {
        for rp in permissions {
            validate_app_namespace(&rp.app)?;
            validate_permission(&rp.permission)?;
        }
        // Reconcile the whole additive bundle transactionally. A controller
        // retry after a database/network failure must observe either the old
        // bundle or the complete requested bundle, never a silently-partial
        // subset.
        let mut tx = self.pool.begin().await?;
        let role_id = Self::get_role_id_in(&mut *tx, name).await?;

        for rp in permissions {
            sqlx::query(
                "INSERT INTO role_permissions (role_id, app, permission)
                 VALUES ($1, $2, $3)
                 ON CONFLICT DO NOTHING",
            )
            .bind(role_id)
            .bind(&rp.app)
            .bind(&rp.permission)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;

        tracing::info!(
            event_type = "role_permissions_added",
            role = name,
            count = permissions.len(),
            "permissions added to role"
        );
        self.get_role(name).await
    }

    pub async fn remove_role_permissions(
        &self,
        name: &str,
        permissions: &[RolePermission],
    ) -> Result<u64, AuthError> {
        let role_id = self.get_role_id(name).await?;

        let mut removed = 0;
        for rp in permissions {
            removed += sqlx::query(
                "DELETE FROM role_permissions
                 WHERE role_id = $1 AND app = $2 AND permission = $3",
            )
            .bind(role_id)
            .bind(&rp.app)
            .bind(&rp.permission)
            .execute(&self.pool)
            .await?
            .rows_affected();
        }

        tracing::info!(
            event_type = "role_permissions_removed",
            role = name,
            removed,
            "permissions removed from role"
        );
        Ok(removed)
    }

    pub async fn is_known_permission(
        &self,
        app: &str,
        permission: &str,
    ) -> Result<bool, AuthError> {
        let known: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM app_permissions WHERE app = $1 AND permission = $2)",
        )
        .bind(app)
        .bind(permission)
        .fetch_one(&self.pool)
        .await?;
        Ok(known)
    }

    async fn classify_prefix_miss(&self, prefix: &str) -> Result<AuthError, AuthError> {
        let exists: bool =
            sqlx::query("SELECT EXISTS(SELECT 1 FROM api_keys WHERE prefix = $1) AS exists")
                .bind(prefix)
                .fetch_one(&self.pool)
                .await?
                .try_get("exists")?;

        Ok(if exists {
            AuthError::KeyRevoked {
                prefix: prefix.to_owned(),
            }
        } else {
            AuthError::KeyNotFound {
                prefix: prefix.to_owned(),
            }
        })
    }

    async fn get_role_id(&self, name: &str) -> Result<i64, AuthError> {
        Self::get_role_id_in(&self.pool, name).await
    }

    async fn get_role_id_in<'e, E>(executor: E, name: &str) -> Result<i64, AuthError>
    where
        E: sqlx::PgExecutor<'e>,
    {
        let row_opt = sqlx::query("SELECT id FROM roles WHERE name = $1")
            .bind(name)
            .fetch_optional(executor)
            .await?;
        let Some(row) = row_opt else {
            return Err(AuthError::RoleNotFound {
                name: name.to_owned(),
            });
        };
        Ok(row.try_get("id")?)
    }

    async fn lock_active_key_id_by_prefix(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        prefix: &str,
    ) -> Result<i64, AuthError> {
        let row_opt = sqlx::query(
            "SELECT id, active
             FROM api_keys
             WHERE prefix = $1
             FOR UPDATE",
        )
        .bind(prefix)
        .fetch_optional(&mut **tx)
        .await?;
        let Some(row) = row_opt else {
            return Err(AuthError::KeyNotFound {
                prefix: prefix.to_owned(),
            });
        };
        let active: bool = row.try_get("active")?;
        if !active {
            return Err(AuthError::KeyRevoked {
                prefix: prefix.to_owned(),
            });
        }
        Ok(row.try_get("id")?)
    }
}

/// Fold `roles LEFT JOIN role_permissions` rows (ordered by role name) into
/// [`Role`] values.
fn rows_to_roles(rows: Vec<PgRow>) -> Result<Vec<Role>, AuthError> {
    let mut roles: Vec<Role> = Vec::new();
    for row in rows {
        let id: i64 = row.try_get("role_id")?;
        let name: String = row.try_get("name")?;
        let rate_rpm: Option<i32> = row.try_get("rate_rpm")?;
        // The CHECK constraint guarantees rate_rpm > 0, so the cast holds.
        let rate_rpm = rate_rpm.and_then(|v| u32::try_from(v).ok());

        if roles.last().is_none_or(|r| r.id != id) {
            roles.push(Role {
                id,
                name,
                rate_rpm,
                permissions: Vec::new(),
            });
        }

        let app: Option<String> = row.try_get("app")?;
        let permission: Option<String> = row.try_get("permission")?;
        if let (Some(app), Some(permission)) = (app, permission)
            && let Some(role) = roles.last_mut()
        {
            role.permissions.push(RolePermission { app, permission });
        }
    }
    Ok(roles)
}

fn row_to_api_key_info_no_roles(row: &PgRow) -> Result<ApiKeyInfo, AuthError> {
    let kind_str: String = row.try_get("kind")?;
    let kind: PrincipalKind = kind_str.parse()?;

    Ok(ApiKeyInfo {
        id: row.try_get("id")?,
        prefix: row.try_get("prefix")?,
        name: row.try_get("name")?,
        kind,
        roles: Vec::new(),
        active: row.try_get("active")?,
        created_at: row.try_get("created_at")?,
        expires_at: row.try_get("expires_at")?,
        last_used: row.try_get("last_used")?,
        revoked_at: row.try_get("revoked_at")?,
    })
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::{Pin, pin};
    use std::sync::mpsc;
    use std::task::Poll;

    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
    use tokio::task::JoinHandle;
    use tokio::time::timeout;

    use super::*;

    /// Failure deadline for a step that must happen. Never a wait for an
    /// event to pass.
    pub(super) const DEADLINE: Duration = Duration::from_secs(30);

    /// Serializes the tests in this crate that take argon2 permits. Tests
    /// that fill all four permits would otherwise deadlock each other, and
    /// a permit held by an unrelated test would make the exact counts below
    /// wrong.
    pub(super) static ARGON2_LIMIT_TESTS: tokio::sync::Mutex<()> =
        tokio::sync::Mutex::const_new(());

    pub(super) struct Parked {
        release: mpsc::Sender<()>,
        caller: JoinHandle<Result<(), AuthError>>,
    }

    impl Parked {
        /// Start a parking check through `on_blocking_pool` and wait until
        /// its closure runs, so it holds a permit.
        pub(super) async fn hold() -> Self {
            let (entered, mut entered_rx) = unbounded_channel();
            let (release, released) = mpsc::channel();
            let caller = tokio::spawn(on_blocking_pool(park(entered, released)));
            wait_entered(&mut entered_rx).await;
            Self { release, caller }
        }

        /// Let the closure return, and wait until its caller has the result,
        /// which is after the permit is released.
        pub(super) async fn finish(self) {
            self.release
                .send(())
                .expect("the parked closure is waiting");
            timeout(DEADLINE, self.caller)
                .await
                .expect("the parked check returns")
                .expect("the parked caller was not cancelled")
                .expect("the parked check succeeds");
        }
    }

    /// A closure that signals `entered` and then waits for `release`. A
    /// dropped release sender also lets it return, so a failed test frees
    /// its permits.
    fn park(
        entered: tokio::sync::mpsc::UnboundedSender<()>,
        release: mpsc::Receiver<()>,
    ) -> impl FnOnce() -> Result<(), AuthError> + Send + 'static {
        move || {
            let _ = entered.send(());
            let _ = release.recv();
            Ok(())
        }
    }

    async fn wait_entered(entered: &mut UnboundedReceiver<()>) {
        timeout(DEADLINE, entered.recv())
            .await
            .expect("the parked closure runs")
            .expect("the parked closure signals entry");
    }

    pub(super) fn available_permits() -> usize {
        ARGON2_CHECKS.available_permits()
    }

    /// Poll a future exactly once. A check that finds no free permit
    /// registers as a waiter on its first poll.
    async fn poll_once<F: Future>(mut fut: Pin<&mut F>) -> Poll<F::Output> {
        std::future::poll_fn(move |cx| Poll::Ready(fut.as_mut().poll(cx))).await
    }

    /// Run `check` as a fifth check while `parked` holds the limit. Its first
    /// poll finds no free permit and registers it as a waiter. When one
    /// parked check ends, its permit goes to the waiter and not back to the
    /// pool: `check` is not polled in between, so it cannot have run and
    /// released the permit yet. A check that bypassed the limit would leave
    /// the freed permit in the pool. The limit is full again on return.
    async fn waits_for_a_freed_permit<T>(
        parked: &mut Vec<Parked>,
        mut check: Pin<&mut impl Future<Output = Result<T, AuthError>>>,
    ) -> T {
        assert_eq!(available_permits(), 0, "the parked checks hold the limit");
        assert!(poll_once(check.as_mut()).await.is_pending());
        parked.pop().expect("a parked check").finish().await;
        assert_eq!(
            available_permits(),
            0,
            "the freed permit belongs to the waiting check"
        );
        let out = timeout(DEADLINE, check)
            .await
            .expect("the waiting check runs")
            .expect("the check succeeds");
        assert_eq!(available_permits(), 1, "the check released its permit");
        parked.push(Parked::hold().await);
        out
    }

    #[tokio::test]
    async fn argon2_checks_share_a_limit_of_four() {
        let _serial = ARGON2_LIMIT_TESTS.lock().await;
        assert_eq!(ARGON2_CHECK_LIMIT, 4);
        assert_eq!(available_permits(), ARGON2_CHECK_LIMIT);

        let key = token::generate_token();
        let stored = hash_token_async(&key.plaintext)
            .await
            .expect("hash the key");
        let stranger = token::generate_token();

        let mut parked = Vec::new();
        for _ in 0..ARGON2_CHECK_LIMIT {
            parked.push(Parked::hold().await);
        }
        assert_eq!(available_permits(), 0, "four parked checks hold the limit");

        let hashed =
            waits_for_a_freed_permit(&mut parked, pin!(hash_token_async("another key"))).await;
        assert!(token::verify_token("another key", &hashed).expect("a PHC string"));
        let verified = waits_for_a_freed_permit(
            &mut parked,
            pin!(verify_token_async(&key.plaintext, &stored)),
        )
        .await;
        assert!(verified, "the key verifies against its own hash");
        let dummy =
            waits_for_a_freed_permit(&mut parked, pin!(verify_dummy_async(&stranger.plaintext)))
                .await;
        assert!(!dummy, "the dummy check never matches");
        assert_eq!(available_permits(), 0, "four parked checks hold the limit");

        for check in parked {
            check.finish().await;
        }
        assert_eq!(available_permits(), ARGON2_CHECK_LIMIT);
    }

    #[tokio::test]
    async fn argon2_permit_outlives_a_cancelled_caller() {
        let _serial = ARGON2_LIMIT_TESTS.lock().await;
        assert_eq!(available_permits(), ARGON2_CHECK_LIMIT);

        let (entered, mut entered_rx) = unbounded_channel();
        let (release, released) = mpsc::channel();
        let caller = tokio::spawn(on_blocking_pool(park(entered, released)));
        wait_entered(&mut entered_rx).await;
        caller.abort();
        let cancelled = timeout(DEADLINE, caller)
            .await
            .expect("the cancelled caller is joined")
            .expect_err("the caller was cancelled");
        assert!(cancelled.is_cancelled());

        assert_eq!(available_permits(), ARGON2_CHECK_LIMIT - 1);
        let mut parked = Vec::new();
        for _ in 1..ARGON2_CHECK_LIMIT {
            parked.push(Parked::hold().await);
        }
        assert_eq!(available_permits(), 0, "the orphaned step keeps its permit");

        // A check that is dropped while it waits for a permit never runs:
        // its closure is dropped unrun, which closes its entry channel.
        let (unrun_entered, mut unrun_entered_rx) = unbounded_channel();
        let (_unrun_release, unrun_released) = mpsc::channel();
        {
            let mut waiting = pin!(on_blocking_pool(park(unrun_entered, unrun_released)));
            assert!(poll_once(waiting.as_mut()).await.is_pending());
        }
        assert!(matches!(
            unrun_entered_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
        ));

        let mut fifth = pin!(hash_token_async("fifth"));
        assert!(poll_once(fifth.as_mut()).await.is_pending());
        release.send(()).expect("the orphaned step is waiting");
        timeout(DEADLINE, fifth)
            .await
            .expect("the fifth check runs once the orphaned step returns")
            .expect("hashing succeeds");

        for check in parked {
            check.finish().await;
        }
        assert_eq!(available_permits(), ARGON2_CHECK_LIMIT);
    }

    #[tokio::test]
    async fn a_panicking_worker_never_carries_its_payload() {
        // It takes an argon2 permit, so it must not disturb the counts the
        // limit tests assert.
        let _serial = ARGON2_LIMIT_TESTS.lock().await;
        let err =
            on_blocking_pool(|| -> Result<(), AuthError> { panic!("zz_argon2_payload_sentinel") })
                .await
                .expect_err("the worker panics");
        assert!(matches!(err, AuthError::Hash(_)), "got {err:?}");
        let shown = format!("{err} {err:?}");
        assert!(!shown.contains("zz_argon2_payload_sentinel"), "{shown}");
        assert!(shown.contains("argon2 worker panicked"), "{shown}");
    }
}

/// The cached-key case of the argon2 limit. It needs a real key, so
/// Postgres, and it needs the crate-internal permits, so it cannot live with
/// the integration tests.
#[cfg(test)]
mod pg_tests {
    use sqlx::PgPool;
    use tokio::time::timeout;

    use std::pin::pin;
    use std::time::Duration;

    use super::tests::{ARGON2_LIMIT_TESTS, DEADLINE, Parked, available_permits};
    use super::{ARGON2_CHECK_LIMIT, KeyStore};
    use crate::cache::VerificationCache;
    use crate::error::AuthError;
    use crate::types::PrincipalKind;

    #[sqlx::test]
    async fn a_cached_key_never_waits_for_an_argon2_permit(pool: PgPool) {
        let _serial = ARGON2_LIMIT_TESTS.lock().await;
        let store = KeyStore::from_pool(pool);
        let created = store
            .create_key("cached", PrincipalKind::Service, &[], None)
            .await
            .expect("create");
        store
            .verify_key(&created.plaintext_token)
            .await
            .expect("the first check runs argon2 and caches the key");

        let mut parked = Vec::new();
        for _ in 0..ARGON2_CHECK_LIMIT {
            parked.push(Parked::hold().await);
        }
        assert_eq!(available_permits(), 0, "four parked checks hold the limit");

        let verified = timeout(DEADLINE, store.verify_key(&created.plaintext_token))
            .await
            .expect("a cached key does not wait for a permit")
            .expect("the cached key verifies");
        assert_eq!(verified.name, "cached");
        assert_eq!(available_permits(), 0);

        for check in parked {
            check.finish().await;
        }
        assert_eq!(available_permits(), ARGON2_CHECK_LIMIT);
    }

    #[sqlx::test]
    async fn a_cache_hit_inserts_nothing(pool: PgPool) {
        let _serial = ARGON2_LIMIT_TESTS.lock().await;
        let store = KeyStore::from_pool(pool);
        let created = store
            .create_key("hit", PrincipalKind::Service, &[], None)
            .await
            .expect("create");

        store
            .verify_key(&created.plaintext_token)
            .await
            .expect("the first check runs argon2 and inserts");
        assert_eq!(store.cache.insert_calls(), 1, "the KDF run inserts once");

        store
            .verify_key(&created.plaintext_token)
            .await
            .expect("the second check hits the cache");
        assert_eq!(
            store.cache.insert_calls(),
            1,
            "a cache hit must not insert again"
        );
        assert_eq!(store.cache_stats().entries, 1);
    }

    #[sqlx::test]
    async fn eviction_forces_a_kdf_run_and_never_authenticates_a_wrong_token(pool: PgPool) {
        let _serial = ARGON2_LIMIT_TESTS.lock().await;
        let store = KeyStore::with_cache(pool, VerificationCache::with_capacity(1));
        let first = store
            .create_key("first", PrincipalKind::Service, &[], None)
            .await
            .expect("create first");
        let second = store
            .create_key("second", PrincipalKind::Service, &[], None)
            .await
            .expect("create second");

        store
            .verify_key(&first.plaintext_token)
            .await
            .expect("first runs argon2 and is cached");
        store
            .verify_key(&second.plaintext_token)
            .await
            .expect("second runs argon2 and evicts first");
        assert_eq!(store.cache_stats().entries, 1);
        assert_eq!(store.cache.insert_calls(), 2);

        // Park every permit. The evicted key needs one, so its check stays
        // pending; a check that found a stale entry would finish at once.
        let mut parked = Vec::new();
        for _ in 0..ARGON2_CHECK_LIMIT {
            parked.push(Parked::hold().await);
        }
        assert_eq!(available_permits(), 0, "four parked checks hold the limit");
        let mut check = pin!(store.verify_key(&first.plaintext_token));
        assert!(
            timeout(Duration::from_millis(500), check.as_mut())
                .await
                .is_err(),
            "an evicted key waits for an argon2 permit"
        );
        parked.pop().expect("a parked check").finish().await;
        let verified = timeout(DEADLINE, check)
            .await
            .expect("the freed permit lets the KDF run")
            .expect("the evicted key verifies again");
        assert_eq!(verified.name, "first");
        assert_eq!(
            store.cache.insert_calls(),
            3,
            "the KDF run inserted the key again"
        );
        assert_eq!(store.cache_stats().entries, 1);
        for check in parked {
            check.finish().await;
        }
        assert_eq!(available_permits(), ARGON2_CHECK_LIMIT);

        let tampered_second = format!(
            "{}AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA0",
            &second.plaintext_token[..12]
        );
        assert!(matches!(
            store.verify_key(&tampered_second).await,
            Err(AuthError::InvalidKey(_))
        ));
        let tampered_first = format!(
            "{}AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA0",
            &first.plaintext_token[..12]
        );
        assert!(matches!(
            store.verify_key(&tampered_first).await,
            Err(AuthError::InvalidKey(_))
        ));
        assert_eq!(
            store.cache.insert_calls(),
            3,
            "a refused token inserts nothing"
        );
        assert_eq!(store.cache_stats().entries, 1);
    }
}
