// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Postgres-backed API key store with data-defined roles (ADR-0006).
//!
//! [`KeyStore`] is the runtime entry point: construct with [`from_pool`], call
//! [`verify_key`] on the request path, call the admin operations (key
//! lifecycle, role CRUD, role assignment) from whatever CLI / API surface
//! owns the lifecycle.
//!
//! Construction does NOT run migrations — apply them via [`crate::MIGRATOR`]
//! from the operational tool that owns deploys (e.g. `fleet-admin migrate`).
//!
//! [`from_pool`]: KeyStore::from_pool
//! [`verify_key`]: KeyStore::verify_key

use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::Row as _;
use sqlx::postgres::{PgPool, PgRow};

use crate::cache::{VerificationCache, VerificationCacheKey, VerificationCacheStats};
use crate::error::AuthError;
use crate::token;
use crate::types::{ApiKeyInfo, CreatedKey, PrincipalKind, Role, RolePermission, VerifiedKey};
use crate::validation::{validate_app_namespace, validate_permission, validate_role_name};

/// Run argon2id hashing on the tokio blocking pool.
///
/// Production argon2id with 128 MiB / 3 iterations takes ~200 ms and would
/// stall a tokio worker thread for that long — invalid-token spray could
/// starve the runtime. `spawn_blocking` moves the CPU work to a dedicated
/// pool sized for blocking I/O.
async fn hash_token_async(plaintext: &str) -> Result<String, AuthError> {
    let plaintext = plaintext.to_owned();
    tokio::task::spawn_blocking(move || token::hash_token(&plaintext))
        .await
        .map_err(|e| AuthError::Hash(format!("argon2 worker panicked: {e}")))?
}

/// Run argon2id verification on the tokio blocking pool. See `hash_token_async`.
async fn verify_token_async(plaintext: &str, hash: &str) -> Result<bool, AuthError> {
    let plaintext = plaintext.to_owned();
    let hash = hash.to_owned();
    tokio::task::spawn_blocking(move || token::verify_token(&plaintext, &hash))
        .await
        .map_err(|e| AuthError::Hash(format!("argon2 worker panicked: {e}")))?
}

/// Postgres + argon2id-cache backed API key store.
///
/// Cheap to `Clone` — the inner `PgPool` is already `Arc`-shared and the
/// cache is `Arc`-shared via `VerificationCache`. Suitable for axum
/// `FromRef` extraction.
#[derive(Clone, Debug)]
pub struct KeyStore {
    pool: PgPool,
    cache: VerificationCache,
}

impl KeyStore {
    /// Construct from an existing pool. Infallible, no I/O, no migrations.
    ///
    /// Callers own pool lifecycle and connectivity validation (use
    /// [`Self::ping`] to confirm the database is reachable). Migrations are
    /// applied via [`crate::MIGRATOR`] from an operational tool, never here.
    pub fn from_pool(pool: PgPool) -> Self {
        Self {
            pool,
            cache: VerificationCache::new(),
        }
    }

    /// Connect to Postgres from a database URL and construct a store.
    ///
    /// Establishes the pool eagerly (the first connection is opened before
    /// this returns), so daemon consumers fail fast at startup when the
    /// auth backend is unreachable instead of on the first request.
    ///
    /// Does NOT run migrations — apply them via [`crate::MIGRATOR`] from the
    /// operational tool that owns deploys (e.g. `fleet-admin migrate`).
    ///
    /// # Errors
    /// Returns [`AuthError::Database`] when the URL is malformed or the
    /// database is unreachable.
    pub async fn connect(database_url: &str) -> Result<Self, AuthError> {
        /// Upper bound on pooled connections. Sized for a single daemon's
        /// request path plus background pollers — not a tunable yet.
        const MAX_CONNECTIONS: u32 = 8;

        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(MAX_CONNECTIONS)
            .connect(database_url)
            .await?;
        Ok(Self::from_pool(pool))
    }

    /// Borrow the underlying pool (e.g. for higher-level layers that need to
    /// run their own queries against the same connection budget).
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Snapshot the argon2id result cache state (entry count etc.).
    pub fn cache_stats(&self) -> VerificationCacheStats {
        self.cache.stats()
    }

    /// Lightweight health check: runs `SELECT 1` against the pool.
    pub async fn ping(&self) -> Result<(), AuthError> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    // -- key lifecycle --------------------------------------------------------

    /// Create a new API key holding the named roles.
    ///
    /// Returns the metadata AND the plaintext token — the latter must be
    /// shown to the user immediately, it cannot be recovered later. Retries
    /// on prefix collision (UNIQUE constraint), though collisions are
    /// astronomically unlikely with a 48-bit prefix space.
    ///
    /// `role_names` may be empty (the key authenticates but authorizes
    /// nothing until roles are assigned). Every named role must pre-exist —
    /// unknown names error with [`AuthError::RoleNotFound`] rather than
    /// silently minting a capability-less key. Duplicate names in the batch
    /// error with [`AuthError::RoleAlreadyAssigned`].
    pub async fn create_key(
        &self,
        name: &str,
        kind: PrincipalKind,
        role_names: &[String],
        expires_in: Option<Duration>,
    ) -> Result<CreatedKey, AuthError> {
        /// Retry attempts for prefix collision (UNIQUE violation).
        const MAX_RETRIES: usize = 3;

        for role in role_names {
            validate_role_name(role)?;
        }

        // Reject duplicate role names explicitly so the error names the
        // duplicate instead of relying on the key_roles PK inside the
        // transaction.
        let mut seen = std::collections::HashSet::new();
        for role in role_names {
            if !seen.insert(role) {
                return Err(AuthError::RoleAlreadyAssigned {
                    prefix: "(new key)".to_owned(),
                    role: role.clone(),
                });
            }
        }

        // Resolve every role name up front — RoleNotFound must name the
        // missing role before any row is written.
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
                    // Prefix collision — drop transaction, retry with a new token.
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

    /// Verify a plaintext token.
    ///
    /// Returns the verified identity if every check passes. Every failure
    /// surfaces as [`AuthError::InvalidKey`] (with the same opaque message)
    /// so callers cannot use the variant to distinguish "not found", "wrong
    /// hash", "revoked", or "expired" — that distinction would be an
    /// enumeration oracle.
    ///
    /// See the module docs for the full algorithm, including the timing
    /// equalization on prefix miss and the TOCTOU recheck in the `last_used`
    /// update.
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
            // doesn't leak via response time. spawn_blocking so the argon2id
            // work doesn't block a tokio worker thread.
            let _ = verify_token_async(plaintext, &token::DUMMY_HASH).await;
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

        if !self.cache.contains(&cache_key) {
            // Cache miss — pay the argon2id cost on the blocking pool so
            // the tokio worker thread stays available for other tasks.
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

        // The UPDATE both rechecks liveness and locks the api_keys row until
        // commit. Role assign/unassign operations lock the same row before
        // mutating key_roles, so the role resolution below observes a role
        // set that cannot change underneath this in-flight verify (#11).
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query(
            "UPDATE api_keys
             SET last_used = NOW()
             WHERE id = $1
               AND active = TRUE
               AND (expires_at IS NULL OR expires_at > NOW())
             RETURNING id",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;

        if updated.is_none() {
            return Err(AuthError::InvalidKey("authentication failed".into()));
        }

        // Resolve key → roles → permissions fresh from the DB, in one
        // snapshot inside the row-lock transaction — never cached.
        let roles = Self::load_key_roles(&mut *tx, id).await?;
        tx.commit().await?;

        let kind: PrincipalKind = kind_str.parse()?;

        // Only insert on full success — never pollute the cache with
        // (prefix, hash, fingerprint) triples that lost a race.
        self.cache.insert(cache_key);

        Ok(VerifiedKey::from_roles(id, db_prefix, name, kind, roles))
    }

    /// Look up a key by database id and return it only if it is *live*:
    /// active (not revoked) AND unexpired. Roles + permissions are resolved
    /// fresh from the database on every call — never cached.
    ///
    /// This is the substrate hook for consumers that gate background work on
    /// key liveness (e.g. trawl's scheduler skipping runs whose owning key
    /// was revoked, expired, or stripped of its app capability — ADR-0004).
    /// Returns `Ok(None)` for unknown ids, revoked keys, and expired keys;
    /// callers cannot distinguish those cases (they all mean "don't run").
    ///
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

    /// Load a key's roles WITH their permission bundles in one snapshot.
    ///
    /// A single `key_roles JOIN roles LEFT JOIN role_permissions` statement
    /// so the resolution is one consistent read (one statement, one
    /// snapshot under READ COMMITTED). Runs against either the pool or an
    /// open transaction (`verify_key` passes its row-lock transaction).
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

    /// Load just the sorted role NAMES for a key (admin list/detail views).
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

    /// List all keys. Never exposes hashes.
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

        // Eager-load role names. N+1 is fine for admin list views at fleet
        // scale; a JOIN + group_concat is messier and we don't need streaming.
        for key in &mut keys {
            key.roles = self.load_key_role_names(key.id).await?;
        }

        Ok(keys)
    }

    /// Look up a key's full metadata by prefix.
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

    /// Revoke a key by prefix.
    ///
    /// Errors with [`AuthError::KeyRevoked`] if the key exists but is already
    /// revoked, and [`AuthError::KeyNotFound`] if the prefix isn't known.
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

    /// Change the kind (human/service) of a non-revoked key.
    ///
    /// Refuses to retype revoked keys — retyping a dead key is likely a typo.
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

    // -- role assignment ------------------------------------------------------

    /// Assign a role to an existing key.
    ///
    /// Locks the key row first — an in-flight `verify_key` on the same key
    /// serializes against this mutation (#11). Errors with
    /// [`AuthError::RoleAlreadyAssigned`] on a duplicate so assignment is
    /// never a silent no-op, and [`AuthError::RoleNotFound`] for unknown
    /// role names.
    pub async fn assign_role(&self, prefix: &str, role_name: &str) -> Result<(), AuthError> {
        validate_role_name(role_name)?;
        let mut tx = self.pool.begin().await?;
        let key_id = self.lock_key_id_by_prefix(&mut tx, prefix).await?;
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

    /// Remove a role from a key.
    ///
    /// Locks the key row first (same serialization contract as
    /// [`Self::assign_role`]). Errors with [`AuthError::RoleNotAssigned`]
    /// when the key doesn't hold the role — silently succeeding would mask
    /// typos — and [`AuthError::RoleNotFound`] for unknown role names.
    pub async fn unassign_role(&self, prefix: &str, role_name: &str) -> Result<(), AuthError> {
        validate_role_name(role_name)?;
        let mut tx = self.pool.begin().await?;
        let key_id = self.lock_key_id_by_prefix(&mut tx, prefix).await?;
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

    // -- role CRUD ------------------------------------------------------------

    /// Create a new role with an optional rate ceiling and initial
    /// permission bundle.
    ///
    /// Errors with [`AuthError::RoleExists`] on a name collision. Every
    /// `(app, permission)` pair is shape-validated; vocabulary membership is
    /// NOT enforced here (the registry is warn-only — callers like
    /// fleet-admin surface the warning).
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

    /// Look up a role (with its permission bundle) by name.
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

    /// List every role (with permission bundles), ordered by name.
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

    /// Count keys currently holding a role (by name).
    pub async fn count_role_assignments(&self, name: &str) -> Result<i64, AuthError> {
        let role_id = self.get_role_id(name).await?;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM key_roles WHERE role_id = $1")
            .bind(role_id)
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }

    /// Delete a role.
    ///
    /// Refuses with [`AuthError::RoleInUse`] (carrying the affected-key
    /// count) while any key still holds the role, unless `force` — forcing
    /// unassigns the role from every key first, which strips capability
    /// from those keys immediately.
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

    /// Add `(app, permission)` pairs to an existing role. Duplicates are
    /// no-ops (idempotent).
    pub async fn add_role_permissions(
        &self,
        name: &str,
        permissions: &[RolePermission],
    ) -> Result<Role, AuthError> {
        for rp in permissions {
            validate_app_namespace(&rp.app)?;
            validate_permission(&rp.permission)?;
        }
        let role_id = self.get_role_id(name).await?;

        for rp in permissions {
            sqlx::query(
                "INSERT INTO role_permissions (role_id, app, permission)
                 VALUES ($1, $2, $3)
                 ON CONFLICT DO NOTHING",
            )
            .bind(role_id)
            .bind(&rp.app)
            .bind(&rp.permission)
            .execute(&self.pool)
            .await?;
        }

        tracing::info!(
            event_type = "role_permissions_added",
            role = name,
            count = permissions.len(),
            "permissions added to role"
        );
        self.get_role(name).await
    }

    /// Remove `(app, permission)` pairs from an existing role. Returns the
    /// number of rows actually removed — callers can warn when a named pair
    /// wasn't on the role.
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

    /// Whether `(app, permission)` is registered in the `app_permissions`
    /// vocabulary. The registry is warn-only: an unknown pair is legal to
    /// persist, but tooling should surface the gap (typo protection).
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

    // -- internals ------------------------------------------------------------

    /// Classify a "zero rows affected" UPDATE on `api_keys` by prefix —
    /// distinguishes a not-found prefix from one that exists but is already
    /// revoked. Used by `revoke_key` and `retype_key`, both of which guard
    /// their UPDATE on `active = TRUE` / `revoked_at IS NULL`.
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

    /// Resolve a role name to its row id against the pool.
    async fn get_role_id(&self, name: &str) -> Result<i64, AuthError> {
        Self::get_role_id_in(&self.pool, name).await
    }

    /// Resolve a role name to its row id against any executor.
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

    /// Lock the key row for the duration of a role-assignment mutation.
    async fn lock_key_id_by_prefix(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        prefix: &str,
    ) -> Result<i64, AuthError> {
        let row_opt = sqlx::query(
            "SELECT id
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
        Ok(row.try_get("id")?)
    }
}

/// Fold `roles LEFT JOIN role_permissions` rows (ordered by role name) into
/// [`Role`] values. A role with no permissions yields NULL app/permission
/// from the LEFT JOIN and an empty bundle here.
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

/// Map a row from `api_keys` to [`ApiKeyInfo`] without roles.
///
/// Callers populate `roles` separately.
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
