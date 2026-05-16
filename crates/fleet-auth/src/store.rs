// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Postgres-backed API key store.
//!
//! [`KeyStore`] is the runtime entry point: construct with [`from_pool`], call
//! [`verify_key`] on the request path, call the admin operations
//! (`create_key`, `revoke_key`, `list_keys`, grant management, retype) from
//! whatever CLI / API surface owns the lifecycle.
//!
//! Construction does NOT run migrations — apply them via [`crate::MIGRATOR`]
//! from the operational tool that owns deploys (e.g. `fleet-admin migrate`).
//!
//! [`from_pool`]: KeyStore::from_pool
//! [`verify_key`]: KeyStore::verify_key

use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx_core::row::Row as _;
use sqlx_postgres::{PgPool, PgRow};

use crate::cache::{VerificationCache, VerificationCacheKey, VerificationCacheStats};
use crate::error::AuthError;
use crate::token;
use crate::types::{ApiKeyInfo, CreatedKey, PrincipalKind, RoleAssignment, VerifiedKey};
use crate::validation::{validate_app_namespace, validate_assignment};

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
        sqlx_core::query::query("SELECT 1")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Create a new API key.
    ///
    /// Returns the metadata AND the plaintext token — the latter must be
    /// shown to the user immediately, it cannot be recovered later. Retries
    /// on prefix collision (UNIQUE constraint), though collisions are
    /// astronomically unlikely with a 48-bit prefix space.
    ///
    /// `assignments` may be empty (the key authorizes nothing until grants
    /// are added). Each grant is validated for shape; the entire batch is
    /// rejected on duplicate apps so a partial set is never persisted.
    pub async fn create_key(
        &self,
        name: &str,
        kind: PrincipalKind,
        assignments: &[RoleAssignment],
        expires_in: Option<Duration>,
    ) -> Result<CreatedKey, AuthError> {
        /// Retry attempts for prefix collision (UNIQUE violation).
        const MAX_RETRIES: usize = 3;

        for a in assignments {
            validate_assignment(a)?;
        }

        // Reject duplicate apps explicitly so the error names the duplicate
        // instead of relying on the UNIQUE(key_id, app) constraint inside
        // the transaction.
        let mut seen = std::collections::HashSet::new();
        for a in assignments {
            if !seen.insert(&a.app) {
                return Err(AuthError::GrantExists {
                    prefix: "(new key)".to_owned(),
                    app: a.app.clone(),
                });
            }
        }

        // Set both timestamps client-side so they share the same wall-clock
        // reading. Without this, Postgres-side NOW() for `created_at` can
        // land microseconds after the Rust-side `Utc::now() + d` for very
        // short expiries, tripping the `api_keys_expiry_after_create` CHECK.
        let created_at = Utc::now();
        let expires_at: Option<DateTime<Utc>> = expires_in.map(|d| {
            let delta =
                chrono::Duration::from_std(d).unwrap_or_else(|_| chrono::Duration::seconds(0));
            created_at + delta
        });

        for _ in 0..MAX_RETRIES {
            let generated = token::generate_token();
            let hash = token::hash_token(&generated.plaintext)?;

            let mut tx = self.pool.begin().await?;

            let insert_result = sqlx_core::query::query(
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
                Err(sqlx_core::Error::Database(e)) if e.code().as_deref() == Some("23505") => {
                    // Prefix collision — drop transaction, retry with a new token.
                    drop(tx);
                    continue;
                }
                Err(e) => return Err(e.into()),
            };

            let id: i64 = row.try_get("id")?;

            for a in assignments {
                sqlx_core::query::query(
                    "INSERT INTO api_key_role_assignment (key_id, app, role)
                     VALUES ($1, $2, $3)",
                )
                .bind(id)
                .bind(&a.app)
                .bind(&a.role)
                .execute(&mut *tx)
                .await?;
            }

            tx.commit().await?;

            tracing::info!(
                event_type = "key_created",
                key_id = id,
                prefix = %generated.prefix,
                name,
                kind = %kind,
                assignments = %render_assignments(assignments),
                "API key created"
            );

            return Ok(CreatedKey {
                info: ApiKeyInfo {
                    id,
                    prefix: generated.prefix.clone(),
                    name: name.to_owned(),
                    kind,
                    assignments: assignments.to_vec(),
                    active: true,
                    created_at,
                    expires_at,
                    last_used: None,
                    revoked_at: None,
                },
                plaintext_token: generated.plaintext.clone(),
            });
        }

        Err(AuthError::Hash(
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

        let row_opt: Option<PgRow> = sqlx_core::query::query(
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
            let _ = token::verify_token(plaintext, &token::DUMMY_HASH);
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
            // Cache miss — pay the argon2id cost.
            let hash_valid = token::verify_token(plaintext, &stored_hash)?;
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

        // Conditional UPDATE doubles as a TOCTOU recheck: if the key was
        // revoked / expired between the initial SELECT and now (e.g.
        // during a ~200 ms argon2id run on cache miss), zero rows update
        // and we reject opaquely.
        let updated = sqlx_core::query::query(
            "UPDATE api_keys
             SET last_used = NOW()
             WHERE id = $1
               AND active = TRUE
               AND (expires_at IS NULL OR expires_at > NOW())",
        )
        .bind(id)
        .execute(&self.pool)
        .await?
        .rows_affected();

        if updated == 0 {
            return Err(AuthError::InvalidKey("authentication failed".into()));
        }

        // Load assignments fresh from the DB — never cache them. A grant
        // change must take effect on the very next verify.
        let assignments = self.load_assignments(id).await?;

        let kind: PrincipalKind = kind_str.parse()?;

        // Only insert on full success — never pollute the cache with
        // (prefix, hash, fingerprint) triples that lost a race.
        self.cache.insert(cache_key);

        Ok(VerifiedKey {
            id,
            prefix: db_prefix,
            name,
            kind,
            assignments,
        })
    }

    /// Load all `(app, role)` grants for a key, ordered by app.
    async fn load_assignments(&self, key_id: i64) -> Result<Vec<RoleAssignment>, AuthError> {
        let rows = sqlx_core::query::query(
            "SELECT app, role
             FROM api_key_role_assignment
             WHERE key_id = $1
             ORDER BY app",
        )
        .bind(key_id)
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                Ok(RoleAssignment {
                    app: row.try_get("app")?,
                    role: row.try_get("role")?,
                })
            })
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

        let rows = sqlx_core::query::query(sql).fetch_all(&self.pool).await?;
        let mut keys: Vec<ApiKeyInfo> = rows
            .iter()
            .map(row_to_api_key_info_no_assignments)
            .collect::<Result<Vec<_>, _>>()?;

        // Eager-load assignments. N+1 is fine for admin list views at fleet
        // scale; a JOIN + group_concat is messier and we don't need streaming.
        for key in &mut keys {
            key.assignments = self.load_assignments(key.id).await?;
        }

        Ok(keys)
    }

    /// Look up a key's full metadata by prefix.
    pub async fn get_key_by_prefix(&self, prefix: &str) -> Result<ApiKeyInfo, AuthError> {
        let row_opt = sqlx_core::query::query(
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

        let mut info = row_to_api_key_info_no_assignments(&row)?;
        info.assignments = self.load_assignments(info.id).await?;
        Ok(info)
    }

    /// Revoke a key by prefix.
    ///
    /// Errors with [`AuthError::KeyRevoked`] if the key exists but is already
    /// revoked, and [`AuthError::KeyNotFound`] if the prefix isn't known.
    pub async fn revoke_key(&self, prefix: &str) -> Result<ApiKeyInfo, AuthError> {
        let updated = sqlx_core::query::query(
            "UPDATE api_keys
             SET active = FALSE, revoked_at = NOW()
             WHERE prefix = $1 AND active = TRUE",
        )
        .bind(prefix)
        .execute(&self.pool)
        .await?
        .rows_affected();

        if updated == 0 {
            let exists: bool = sqlx_core::query::query(
                "SELECT EXISTS(SELECT 1 FROM api_keys WHERE prefix = $1) AS exists",
            )
            .bind(prefix)
            .fetch_one(&self.pool)
            .await?
            .try_get("exists")?;

            return Err(if exists {
                AuthError::KeyRevoked {
                    prefix: prefix.to_owned(),
                }
            } else {
                AuthError::KeyNotFound {
                    prefix: prefix.to_owned(),
                }
            });
        }

        tracing::info!(event_type = "key_revoked", prefix, "API key revoked");
        self.get_key_by_prefix(prefix).await
    }

    /// Grant a `(app, role)` to an existing key.
    ///
    /// Errors with [`AuthError::GrantExists`] if the key already has a grant
    /// for this app — explicit revoke-first prevents accidental privilege
    /// inflation via silent overwrite.
    pub async fn grant_assignment(
        &self,
        prefix: &str,
        assignment: &RoleAssignment,
    ) -> Result<(), AuthError> {
        validate_assignment(assignment)?;
        let key_id = self.get_key_id_by_prefix(prefix).await?;

        let result = sqlx_core::query::query(
            "INSERT INTO api_key_role_assignment (key_id, app, role)
             VALUES ($1, $2, $3)",
        )
        .bind(key_id)
        .bind(&assignment.app)
        .bind(&assignment.role)
        .execute(&self.pool)
        .await;

        match result {
            Ok(_) => {
                tracing::info!(
                    event_type = "grant_added",
                    prefix,
                    app = %assignment.app,
                    role = %assignment.role,
                    "grant added to key"
                );
                Ok(())
            }
            Err(sqlx_core::Error::Database(e)) if e.code().as_deref() == Some("23505") => {
                Err(AuthError::GrantExists {
                    prefix: prefix.to_owned(),
                    app: assignment.app.clone(),
                })
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Revoke a key's grant on an app.
    ///
    /// Errors with [`AuthError::GrantNotFound`] if no grant exists —
    /// silently succeeding would mask typos.
    pub async fn revoke_assignment(&self, prefix: &str, app: &str) -> Result<(), AuthError> {
        validate_app_namespace(app)?;
        let key_id = self.get_key_id_by_prefix(prefix).await?;

        let removed = sqlx_core::query::query(
            "DELETE FROM api_key_role_assignment WHERE key_id = $1 AND app = $2",
        )
        .bind(key_id)
        .bind(app)
        .execute(&self.pool)
        .await?
        .rows_affected();

        if removed == 0 {
            return Err(AuthError::GrantNotFound {
                prefix: prefix.to_owned(),
                app: app.to_owned(),
            });
        }

        tracing::info!(
            event_type = "grant_revoked",
            prefix,
            app,
            "grant removed from key"
        );
        Ok(())
    }

    /// List all grants for a key.
    pub async fn list_assignments(&self, prefix: &str) -> Result<Vec<RoleAssignment>, AuthError> {
        let key_id = self.get_key_id_by_prefix(prefix).await?;
        self.load_assignments(key_id).await
    }

    /// Change the kind (human/service) of a non-revoked key.
    ///
    /// Refuses to retype revoked keys — retyping a dead key is likely a typo.
    pub async fn retype_key(
        &self,
        prefix: &str,
        kind: PrincipalKind,
    ) -> Result<ApiKeyInfo, AuthError> {
        let updated = sqlx_core::query::query(
            "UPDATE api_keys SET kind = $1 WHERE prefix = $2 AND revoked_at IS NULL",
        )
        .bind(kind.as_str())
        .bind(prefix)
        .execute(&self.pool)
        .await?
        .rows_affected();

        if updated == 0 {
            let exists: bool = sqlx_core::query::query(
                "SELECT EXISTS(SELECT 1 FROM api_keys WHERE prefix = $1) AS exists",
            )
            .bind(prefix)
            .fetch_one(&self.pool)
            .await?
            .try_get("exists")?;

            return Err(if exists {
                AuthError::KeyRevoked {
                    prefix: prefix.to_owned(),
                }
            } else {
                AuthError::KeyNotFound {
                    prefix: prefix.to_owned(),
                }
            });
        }

        tracing::info!(event_type = "key_retyped", prefix, kind = %kind, "key kind updated");
        self.get_key_by_prefix(prefix).await
    }

    /// Internal helper: resolve a prefix to its database row id.
    async fn get_key_id_by_prefix(&self, prefix: &str) -> Result<i64, AuthError> {
        let row_opt = sqlx_core::query::query("SELECT id FROM api_keys WHERE prefix = $1")
            .bind(prefix)
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row_opt else {
            return Err(AuthError::KeyNotFound {
                prefix: prefix.to_owned(),
            });
        };
        Ok(row.try_get("id")?)
    }
}

/// Map a row from `api_keys` to [`ApiKeyInfo`] without assignments.
///
/// Callers populate `assignments` separately.
fn row_to_api_key_info_no_assignments(row: &PgRow) -> Result<ApiKeyInfo, AuthError> {
    let kind_str: String = row.try_get("kind")?;
    let kind: PrincipalKind = kind_str.parse()?;

    Ok(ApiKeyInfo {
        id: row.try_get("id")?,
        prefix: row.try_get("prefix")?,
        name: row.try_get("name")?,
        kind,
        assignments: Vec::new(),
        active: row.try_get("active")?,
        created_at: row.try_get("created_at")?,
        expires_at: row.try_get("expires_at")?,
        last_used: row.try_get("last_used")?,
        revoked_at: row.try_get("revoked_at")?,
    })
}

fn render_assignments(assignments: &[RoleAssignment]) -> String {
    if assignments.is_empty() {
        return "none".to_owned();
    }
    assignments
        .iter()
        .map(|a| format!("{}:{}", a.app, a.role))
        .collect::<Vec<_>>()
        .join(",")
}
