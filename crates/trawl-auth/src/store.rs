// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `SQLite`-backed storage for API keys.
//!
//! [`KeyStore`] owns a `rusqlite::Connection` and provides the full key
//! lifecycle: creation, verification, listing, revocation, and grant management.

use std::path::Path;
use std::sync::LazyLock;
use std::time::Duration;

use chrono::Utc;
use rusqlite::OptionalExtension as _;
use rusqlite::params;

use crate::assignments::{
    PrincipalKind, RoleAssignment, validate_app_namespace, validate_assignment,
};
use crate::error::AuthError;
use crate::keys::{ApiKeyInfo, CreatedKey, VerifiedKey};
use crate::token;

/// Current schema version. Bumped when migrations are needed.
const SCHEMA_VERSION: i64 = 3;

/// Pre-computed dummy argon2id hash used to equalize timing when a prefix
/// lookup returns no rows. Generated once at process start so that the
/// `verify_token` call on the miss path takes the same time as on the hit path.
static DUMMY_HASH: LazyLock<String> =
    LazyLock::new(|| token::hash_token("dummy-timing-equalization").expect("failed to hash dummy"));

/// `SQLite`-backed storage for API keys.
#[derive(Debug)]
pub struct KeyStore {
    conn: rusqlite::Connection,
}

impl KeyStore {
    /// Open (or create) the auth database at the given path.
    ///
    /// On unix, ensures the file has mode 0600 (owner-only access) to
    /// protect stored argon2 hashes from local users.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, AuthError> {
        Self::ensure_restricted_permissions(path.as_ref())?;
        let conn = rusqlite::Connection::open(path)?;
        // WAL mode: better crash safety + allows concurrent reads during writes
        // (e.g. trawl-admin revoking a key while daemon is authenticating).
        // busy_timeout: retry on SQLITE_BUSY instead of failing immediately.
        // foreign_keys: enforce the api_key_role_assignment → api_keys FK so
        //   cascading delete on key revoke isn't surprising.
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA busy_timeout=5000;
             PRAGMA foreign_keys=ON;",
        )?;
        let store = Self { conn };
        store.initialize()?;
        Ok(store)
    }

    /// Ensure the database file has restricted permissions on unix.
    ///
    /// Pre-creates with mode 0600 if new, or tightens existing perms.
    fn ensure_restricted_permissions(path: &Path) -> Result<(), AuthError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            if path.exists() {
                // Ensure existing file isn't world-readable.
                let perms = std::fs::metadata(path)?.permissions();
                if perms.mode() & 0o077 != 0 {
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
                }
            } else {
                use std::os::unix::fs::OpenOptionsExt;
                // Pre-create with restricted permissions so rusqlite inherits them.
                drop(
                    std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(path)?,
                );
            }
        }
        Ok(())
    }

    /// Open an in-memory database (for testing).
    pub fn open_in_memory() -> Result<Self, AuthError> {
        let conn = rusqlite::Connection::open_in_memory()?;
        // WAL not applicable for in-memory, but set busy_timeout for consistency.
        conn.execute_batch("PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;")?;
        let store = Self { conn };
        store.initialize()?;
        Ok(store)
    }

    /// Run schema migrations.
    fn initialize(&self) -> Result<(), AuthError> {
        // Bootstrap: create schema_version table if this is a fresh database.
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (
                version INTEGER NOT NULL
            );",
        )?;

        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM schema_version", [], |row| row.get(0))?;

        let current_version = if count == 0 {
            // Fresh database — create tables at current schema version.
            self.create_tables_v3()?;
            self.conn.execute(
                "INSERT INTO schema_version (version) VALUES (?1)",
                params![SCHEMA_VERSION],
            )?;
            SCHEMA_VERSION
        } else {
            self.conn
                .query_row("SELECT version FROM schema_version", [], |row| row.get(0))?
        };

        // Run migrations for existing databases.
        if current_version < 2 {
            self.migrate_v1_to_v2()?;
        }
        if current_version < 3 {
            self.migrate_v2_to_v3()?;
        }

        Ok(())
    }

    /// Create the v3 schema:
    ///
    /// - `api_keys` carries `kind` instead of `role` (role lives in the assignment table)
    /// - `api_key_role_assignment` carries the canonical `(key, app, role)` triples
    fn create_tables_v3(&self) -> Result<(), AuthError> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS api_keys (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                prefix      TEXT    NOT NULL,
                name        TEXT    NOT NULL,
                hash        TEXT    NOT NULL,
                kind        TEXT    NOT NULL CHECK (kind IN ('human', 'service')),
                active      INTEGER NOT NULL DEFAULT 1,
                created_at  TEXT    NOT NULL,
                expires_at  TEXT,
                last_used   TEXT,
                revoked_at  TEXT
            );

            CREATE UNIQUE INDEX IF NOT EXISTS idx_api_keys_prefix ON api_keys (prefix);

            CREATE TABLE IF NOT EXISTS api_key_role_assignment (
                id       INTEGER PRIMARY KEY AUTOINCREMENT,
                key_id   INTEGER NOT NULL REFERENCES api_keys(id) ON DELETE CASCADE,
                app      TEXT    NOT NULL,
                role     TEXT    NOT NULL,
                UNIQUE (key_id, app)
            );

            CREATE INDEX IF NOT EXISTS idx_assignment_key ON api_key_role_assignment (key_id);",
        )?;
        Ok(())
    }

    /// Migrate from schema v1 → v2: recreate `api_keys` with updated CHECK constraint.
    ///
    /// Wrapped in an explicit transaction so a crash mid-migration can't
    /// leave the database in a half-migrated state with keys destroyed.
    fn migrate_v1_to_v2(&self) -> Result<(), AuthError> {
        self.conn.execute_batch(
            "BEGIN;

            CREATE TABLE IF NOT EXISTS api_keys_v2 (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                prefix      TEXT    NOT NULL,
                name        TEXT    NOT NULL,
                hash        TEXT    NOT NULL,
                role        TEXT    NOT NULL CHECK (role IN ('admin', 'analyst', 'reader', 'ingest')),
                active      INTEGER NOT NULL DEFAULT 1,
                created_at  TEXT    NOT NULL,
                expires_at  TEXT,
                last_used   TEXT,
                revoked_at  TEXT
            );

            INSERT INTO api_keys_v2 (id, prefix, name, hash, role, active, created_at, expires_at, last_used, revoked_at)
                SELECT id, prefix, name, hash, role, active, created_at, expires_at, last_used, revoked_at
                FROM api_keys;

            DROP TABLE api_keys;

            ALTER TABLE api_keys_v2 RENAME TO api_keys;

            CREATE UNIQUE INDEX IF NOT EXISTS idx_api_keys_prefix ON api_keys (prefix);

            UPDATE schema_version SET version = 2;

            COMMIT;",
        )?;
        Ok(())
    }

    /// Migrate from schema v2 → v3 (ADR-0021):
    ///
    /// - Drop the `role` column on `api_keys`, add `kind` (defaulting to `'human'`)
    /// - Backfill every existing key with a single `('trawl', <old role>)` grant
    ///   in a new `api_key_role_assignment` table
    ///
    /// Wrapped in an explicit transaction so a crash mid-migration can't
    /// leave a half-migrated database.
    fn migrate_v2_to_v3(&self) -> Result<(), AuthError> {
        let tx = self.conn.unchecked_transaction()?;

        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS api_keys_v3 (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                prefix      TEXT    NOT NULL,
                name        TEXT    NOT NULL,
                hash        TEXT    NOT NULL,
                kind        TEXT    NOT NULL CHECK (kind IN ('human', 'service')),
                active      INTEGER NOT NULL DEFAULT 1,
                created_at  TEXT    NOT NULL,
                expires_at  TEXT,
                last_used   TEXT,
                revoked_at  TEXT
            )",
        )
        .inspect_err(|e| tracing::error!(step = "create api_keys_v3", %e, "v3 migration failed"))?;

        tx.execute(
            "INSERT INTO api_keys_v3 (id, prefix, name, hash, kind, active, created_at, expires_at, last_used, revoked_at)
                SELECT id, prefix, name, hash, 'human', active, created_at, expires_at, last_used, revoked_at
                FROM api_keys",
            [],
        )
        .inspect_err(|e| tracing::error!(step = "copy keys", %e, "v3 migration failed"))?;

        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS api_key_role_assignment (
                id       INTEGER PRIMARY KEY AUTOINCREMENT,
                key_id   INTEGER NOT NULL REFERENCES api_keys_v3(id) ON DELETE CASCADE,
                app      TEXT    NOT NULL,
                role     TEXT    NOT NULL,
                UNIQUE (key_id, app)
            )",
        )
        .inspect_err(|e| {
            tracing::error!(step = "create assignment table", %e, "v3 migration failed");
        })?;

        tx.execute(
            "INSERT INTO api_key_role_assignment (key_id, app, role)
                SELECT id, 'trawl', role FROM api_keys",
            [],
        )
        .inspect_err(|e| tracing::error!(step = "backfill grants", %e, "v3 migration failed"))?;

        tx.execute_batch(
            "DROP TABLE api_keys;
             ALTER TABLE api_keys_v3 RENAME TO api_keys;
             CREATE UNIQUE INDEX IF NOT EXISTS idx_api_keys_prefix ON api_keys (prefix);
             CREATE INDEX IF NOT EXISTS idx_assignment_key ON api_key_role_assignment (key_id);
             UPDATE schema_version SET version = 3;",
        )
        .inspect_err(|e| tracing::error!(step = "finalize", %e, "v3 migration failed"))?;

        tx.commit()
            .inspect_err(|e| tracing::error!(step = "commit", %e, "v3 migration failed"))?;
        Ok(())
    }

    /// Create a new API key.
    ///
    /// Returns the key info AND the plaintext token (which must be shown to
    /// the user immediately and never stored). Retries on prefix collision
    /// (UNIQUE constraint violation).
    ///
    /// `assignments` may be empty — a key with no grants is valid (it just
    /// won't authorize anything until grants are added). Each assignment's
    /// `(app, role)` is validated for shape before persisting.
    pub fn create_key(
        &mut self,
        name: &str,
        kind: PrincipalKind,
        assignments: &[RoleAssignment],
        expires_in: Option<Duration>,
    ) -> Result<CreatedKey, AuthError> {
        /// Maximum retry attempts for prefix collision (48-bit prefix space
        /// means collisions are astronomically unlikely, but handle gracefully).
        const MAX_RETRIES: usize = 3;

        // Pre-validate every assignment so we don't insert a partial set.
        for a in assignments {
            validate_assignment(a)?;
        }

        // Reject duplicate apps early — UNIQUE(key_id, app) would catch it
        // inside the transaction, but the error message would be opaque.
        {
            let mut seen = std::collections::HashSet::new();
            for a in assignments {
                if !seen.insert(&a.app) {
                    return Err(AuthError::GrantExists {
                        prefix: "(new key)".to_owned(),
                        app: a.app.clone(),
                    });
                }
            }
        }

        let now = Utc::now().to_rfc3339();
        let expires_at = expires_in.map(|d| {
            let delta = chrono::Duration::from_std(d).unwrap_or(chrono::Duration::MAX);
            (Utc::now() + delta).to_rfc3339()
        });

        for _ in 0..MAX_RETRIES {
            let generated = token::generate_token();
            let hash = token::hash_token(&generated.plaintext)?;

            let tx = self.conn.transaction()?;
            let insert_result = tx.execute(
                "INSERT INTO api_keys (prefix, name, hash, kind, created_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![generated.prefix, name, hash, kind.as_str(), now, expires_at],
            );

            match insert_result {
                Ok(_) => {
                    let id = tx.last_insert_rowid();
                    {
                        let mut stmt = tx.prepare(
                            "INSERT INTO api_key_role_assignment (key_id, app, role)
                             VALUES (?1, ?2, ?3)",
                        )?;
                        for a in assignments {
                            stmt.execute(params![id, &a.app, &a.role])?;
                        }
                    }
                    tx.commit()?;
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
                            prefix: generated.prefix,
                            name: name.to_owned(),
                            kind,
                            assignments: assignments.to_vec(),
                            active: true,
                            created_at: now,
                            expires_at,
                            last_used: None,
                            revoked_at: None,
                        },
                        plaintext_token: generated.plaintext.clone(),
                    });
                }
                // Prefix collision — drop the transaction and retry with a new token.
                Err(rusqlite::Error::SqliteFailure(err, _))
                    if err.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    drop(tx);
                }
                Err(e) => return Err(e.into()),
            }
        }

        Err(AuthError::Hash(
            "failed to generate unique token prefix after retries".into(),
        ))
    }

    /// Verify a plaintext token.
    ///
    /// Returns the verified identity if valid, or [`AuthError::InvalidKey`] if
    /// the token is invalid for any reason. Deliberately returns a single opaque
    /// error to prevent enumeration oracles — callers cannot distinguish between
    /// "not found", "revoked", "expired", or "wrong token".
    ///
    /// Updates `last_used` timestamp on success.
    pub fn verify_key(&self, plaintext: &str) -> Result<VerifiedKey, AuthError> {
        let prefix = token::extract_prefix(plaintext).ok_or_else(|| {
            AuthError::MalformedToken("token must start with flt_ and be at least 12 chars".into())
        })?;

        // Look up the key by prefix (exactly 0 or 1 due to UNIQUE constraint).
        let mut stmt = self.conn.prepare(
            "SELECT id, prefix, name, hash, kind, active, expires_at
             FROM api_keys WHERE prefix = ?1",
        )?;

        // SECURITY: convert to Option instead of early-returning on no rows.
        // This ensures we always run argon2 verification regardless of whether
        // the prefix exists, preventing timing oracles that leak prefix validity.
        let row = stmt
            .query_row(params![prefix], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, bool>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            })
            .optional()
            .map_err(AuthError::Database)?;

        let Some((id, db_prefix, name, hash, kind_str, active, expires_at)) = row else {
            // SECURITY: run argon2 against a dummy hash to equalize timing
            // with the real-prefix path. The verification will always fail.
            let _ = token::verify_token(plaintext, &DUMMY_HASH);
            return Err(AuthError::InvalidKey("authentication failed".into()));
        };

        // SECURITY: always verify the hash FIRST (constant-time via argon2),
        // then check state. Return the same opaque error regardless of which
        // check fails — prevents enumeration oracles.
        let hash_valid = token::verify_token(plaintext, &hash)?;

        let is_revoked = !active;

        let is_expired = expires_at
            .as_ref()
            .and_then(|exp| chrono::DateTime::parse_from_rfc3339(exp).ok())
            .is_some_and(|exp_time| Utc::now() > exp_time);

        if !hash_valid || is_revoked || is_expired {
            return Err(AuthError::InvalidKey("authentication failed".into()));
        }

        let kind: PrincipalKind = kind_str
            .parse()
            .map_err(|e: String| AuthError::InvalidKey(e))?;
        let assignments = self.load_assignments(id)?;

        // Update last_used timestamp.
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "UPDATE api_keys SET last_used = ?1 WHERE id = ?2",
            params![now, id],
        )?;

        Ok(VerifiedKey {
            id,
            prefix: db_prefix,
            name,
            kind,
            assignments,
        })
    }

    /// Load all `(app, role)` grants for a single key.
    fn load_assignments(&self, key_id: i64) -> Result<Vec<RoleAssignment>, AuthError> {
        let mut stmt = self.conn.prepare(
            "SELECT app, role FROM api_key_role_assignment
             WHERE key_id = ?1 ORDER BY app",
        )?;
        let rows = stmt
            .query_map(params![key_id], |row| {
                Ok(RoleAssignment {
                    app: row.get(0)?,
                    role: row.get(1)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Lightweight health check: runs `SELECT 1` against the `SQLite` connection.
    pub fn ping(&self) -> Result<(), AuthError> {
        self.conn
            .query_row("SELECT 1", [], |_row| Ok(()))
            .map_err(AuthError::Database)
    }

    /// List all keys. Never exposes hashes.
    ///
    /// If `active_only` is true, only returns non-revoked keys.
    pub fn list_keys(&self, active_only: bool) -> Result<Vec<ApiKeyInfo>, AuthError> {
        let sql = if active_only {
            "SELECT id, prefix, name, kind, active, created_at, expires_at, last_used, revoked_at
             FROM api_keys WHERE active = 1 ORDER BY created_at DESC"
        } else {
            "SELECT id, prefix, name, kind, active, created_at, expires_at, last_used, revoked_at
             FROM api_keys ORDER BY created_at DESC"
        };

        let mut stmt = self.conn.prepare(sql)?;
        let mut keys = stmt
            .query_map([], row_to_api_key_info_no_assignments)?
            .collect::<Result<Vec<_>, _>>()?;

        // Eager-load assignments. N+1 here is fine for admin list views;
        // a JOIN + group_concat is messier and we don't need streaming.
        for key in &mut keys {
            key.assignments = self.load_assignments(key.id)?;
        }

        Ok(keys)
    }

    /// Look up a key's full info by its prefix.
    ///
    /// Returns `KeyNotFound` if no key with this prefix exists.
    pub fn get_key_by_prefix(&self, prefix: &str) -> Result<ApiKeyInfo, AuthError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, prefix, name, kind, active, created_at, expires_at, last_used, revoked_at
             FROM api_keys WHERE prefix = ?1",
        )?;
        let mut info = stmt
            .query_row(params![prefix], row_to_api_key_info_no_assignments)
            .optional()
            .map_err(AuthError::Database)?
            .ok_or_else(|| AuthError::KeyNotFound {
                prefix: prefix.to_owned(),
            })?;
        info.assignments = self.load_assignments(info.id)?;
        Ok(info)
    }

    /// Get the internal database ID for a key by its prefix.
    ///
    /// Used by history recording to store the FK relationship to `api_keys(id)`.
    /// Returns `KeyNotFound` if no key with this prefix exists.
    pub fn get_key_id_by_prefix(&self, prefix: &str) -> Result<i64, AuthError> {
        self.conn
            .query_row(
                "SELECT id FROM api_keys WHERE prefix = ?1",
                params![prefix],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| AuthError::KeyNotFound {
                prefix: prefix.to_owned(),
            })
    }

    /// Check whether a key is active (not revoked) by its database id.
    pub fn is_key_active(&self, key_id: i64) -> Result<bool, AuthError> {
        let active: Option<i64> = self
            .conn
            .query_row(
                "SELECT active FROM api_keys WHERE id = ?1",
                params![key_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(active == Some(1))
    }

    /// Revoke a key by its prefix.
    pub fn revoke_key(&self, prefix: &str) -> Result<ApiKeyInfo, AuthError> {
        let now = Utc::now().to_rfc3339();

        let updated = self.conn.execute(
            "UPDATE api_keys SET active = 0, revoked_at = ?1 WHERE prefix = ?2 AND active = 1",
            params![now, prefix],
        )?;

        if updated == 0 {
            // Check if the key exists at all (might already be revoked).
            let exists: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM api_keys WHERE prefix = ?1)",
                params![prefix],
                |row| row.get(0),
            )?;

            if exists {
                return Err(AuthError::KeyRevoked {
                    prefix: prefix.to_owned(),
                });
            }
            return Err(AuthError::KeyNotFound {
                prefix: prefix.to_owned(),
            });
        }

        tracing::info!(event_type = "key_revoked", prefix, "API key revoked");

        self.get_key_by_prefix(prefix)
    }

    /// Grant a role on a given app to an existing key.
    ///
    /// Errors with [`AuthError::GrantExists`] if the key already has a grant
    /// for this app — callers must revoke the existing one first to make
    /// the swap intentional.
    pub fn grant_assignment(
        &self,
        prefix: &str,
        assignment: &RoleAssignment,
    ) -> Result<(), AuthError> {
        validate_assignment(assignment)?;
        let key_id = self.get_key_id_by_prefix(prefix)?;

        let result = self.conn.execute(
            "INSERT INTO api_key_role_assignment (key_id, app, role) VALUES (?1, ?2, ?3)",
            params![key_id, &assignment.app, &assignment.role],
        );

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
            Err(rusqlite::Error::SqliteFailure(err, _))
                if err.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Err(AuthError::GrantExists {
                    prefix: prefix.to_owned(),
                    app: assignment.app.clone(),
                })
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Remove a grant on a given app from an existing key.
    ///
    /// Errors with [`AuthError::GrantNotFound`] if the key doesn't have a
    /// grant for this app — silently succeeding would mask typos.
    pub fn revoke_assignment(&self, prefix: &str, app: &str) -> Result<(), AuthError> {
        validate_app_namespace(app)?;
        let key_id = self.get_key_id_by_prefix(prefix)?;
        let removed = self.conn.execute(
            "DELETE FROM api_key_role_assignment WHERE key_id = ?1 AND app = ?2",
            params![key_id, app],
        )?;
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
    pub fn list_assignments(&self, prefix: &str) -> Result<Vec<RoleAssignment>, AuthError> {
        let key_id = self.get_key_id_by_prefix(prefix)?;
        self.load_assignments(key_id)
    }

    /// Change the `kind` (human/service) of an existing key.
    ///
    /// Refuses to retype revoked keys — retyping a dead key is likely a mistake.
    pub fn retype_key(&self, prefix: &str, kind: PrincipalKind) -> Result<ApiKeyInfo, AuthError> {
        let updated = self.conn.execute(
            "UPDATE api_keys SET kind = ?1 WHERE prefix = ?2 AND revoked_at IS NULL",
            params![kind.as_str(), prefix],
        )?;
        if updated == 0 {
            let exists: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM api_keys WHERE prefix = ?1)",
                params![prefix],
                |row| row.get(0),
            )?;
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
        tracing::info!(
            event_type = "key_retyped",
            prefix,
            kind = %kind,
            "key kind updated"
        );
        self.get_key_by_prefix(prefix)
    }
}

/// Map a row from the `api_keys` table to an [`ApiKeyInfo`] without assignments.
///
/// Expects columns in order: id, prefix, name, kind, active, `created_at`,
/// `expires_at`, `last_used`, `revoked_at`.
///
/// Callers must populate `assignments` separately via [`KeyStore::load_assignments`].
fn row_to_api_key_info_no_assignments(
    row: &rusqlite::Row<'_>,
) -> Result<ApiKeyInfo, rusqlite::Error> {
    let kind_str: String = row.get(3)?;
    let kind: PrincipalKind = kind_str.parse().map_err(|e: String| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, e.into())
    })?;

    Ok(ApiKeyInfo {
        id: row.get(0)?,
        prefix: row.get(1)?,
        name: row.get(2)?,
        kind,
        assignments: Vec::new(),
        active: row.get(4)?,
        created_at: row.get(5)?,
        expires_at: row.get(6)?,
        last_used: row.get(7)?,
        revoked_at: row.get(8)?,
    })
}

/// Render a slice of assignments as `"app1:role1,app2:role2"` for tracing.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roles::Role;

    fn test_store() -> KeyStore {
        KeyStore::open_in_memory().expect("failed to open in-memory store")
    }

    fn grant(app: &str, role: &str) -> RoleAssignment {
        RoleAssignment {
            app: app.into(),
            role: role.into(),
        }
    }

    fn trawl_grants(role: Role) -> Vec<RoleAssignment> {
        vec![grant("trawl", role.as_str())]
    }

    #[test]
    fn open_in_memory_succeeds() {
        let _store = test_store();
    }

    #[test]
    fn ping_succeeds() {
        let store = test_store();
        store
            .ping()
            .expect("ping should succeed on a valid connection");
    }

    #[test]
    fn schema_version_is_set() {
        let store = test_store();
        let version: i64 = store
            .conn
            .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn create_key_returns_valid_token() {
        let mut store = test_store();
        let created = store
            .create_key(
                "test-key",
                PrincipalKind::Human,
                &trawl_grants(Role::Analyst),
                None,
            )
            .unwrap();

        assert!(created.plaintext_token.starts_with("flt_"));
        assert_eq!(created.info.name, "test-key");
        assert_eq!(created.info.kind, PrincipalKind::Human);
        assert_eq!(created.info.trawl_role(), Some(Role::Analyst));
        assert!(created.info.active);
        assert!(created.info.expires_at.is_none());
    }

    #[test]
    fn create_and_verify_key() {
        let mut store = test_store();
        let created = store
            .create_key(
                "my-key",
                PrincipalKind::Human,
                &trawl_grants(Role::Admin),
                None,
            )
            .unwrap();

        let verified = store.verify_key(&created.plaintext_token).unwrap();
        assert_eq!(verified.name, "my-key");
        assert_eq!(verified.kind, PrincipalKind::Human);
        assert_eq!(verified.trawl_role(), Some(Role::Admin));
        assert_eq!(verified.prefix, created.info.prefix);
    }

    #[test]
    fn create_key_with_multiple_app_grants() {
        let mut store = test_store();
        let created = store
            .create_key(
                "multi",
                PrincipalKind::Service,
                &[
                    grant("trawl", "analyst"),
                    grant("coastwatch", "siem_consumer"),
                ],
                None,
            )
            .unwrap();

        let verified = store.verify_key(&created.plaintext_token).unwrap();
        assert_eq!(verified.kind, PrincipalKind::Service);
        assert_eq!(verified.assignments.len(), 2);
        assert_eq!(verified.trawl_role(), Some(Role::Analyst));
        assert_eq!(verified.role_for("coastwatch"), Some("siem_consumer"));
    }

    #[test]
    fn create_key_rejects_bad_namespace() {
        let mut store = test_store();
        let err = store
            .create_key(
                "bad",
                PrincipalKind::Human,
                &[grant("BAD-NAMESPACE", "admin")],
                None,
            )
            .unwrap_err();
        assert!(matches!(err, AuthError::InvalidApp(_)));
    }

    #[test]
    fn create_key_with_empty_grants_is_allowed() {
        let mut store = test_store();
        let created = store
            .create_key("no-grants", PrincipalKind::Service, &[], None)
            .unwrap();
        let verified = store.verify_key(&created.plaintext_token).unwrap();
        assert!(verified.assignments.is_empty());
        assert_eq!(verified.trawl_role(), None);
    }

    #[test]
    fn verify_nonexistent_key() {
        let store = test_store();
        let result = store.verify_key("flt_AAAAAAAAthisisnotarealkeyatall1234567");
        assert!(result.is_err());
    }

    #[test]
    fn verify_wrong_token_for_prefix() {
        let mut store = test_store();
        let created = store
            .create_key(
                "test",
                PrincipalKind::Human,
                &trawl_grants(Role::Reader),
                None,
            )
            .unwrap();

        // Tamper with the token body but keep the prefix intact.
        let prefix = &created.plaintext_token[..12]; // "flt_" + 8 chars
        let tampered = format!("{prefix}AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA0");
        let result = store.verify_key(&tampered);
        assert!(result.is_err());
    }

    #[test]
    fn revoke_key_blocks_verification() {
        let mut store = test_store();
        let created = store
            .create_key(
                "revoke-me",
                PrincipalKind::Human,
                &trawl_grants(Role::Analyst),
                None,
            )
            .unwrap();

        store.revoke_key(&created.info.prefix).unwrap();

        // Returns opaque InvalidKey — does NOT reveal that the key was revoked.
        let result = store.verify_key(&created.plaintext_token);
        assert!(matches!(result, Err(AuthError::InvalidKey(_))));
    }

    #[test]
    fn expired_key_fails_verification() {
        let mut store = test_store();
        let created = store
            .create_key(
                "expired",
                PrincipalKind::Human,
                &trawl_grants(Role::Reader),
                None,
            )
            .unwrap();

        // Manually set expires_at to the past.
        let past = (Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        store
            .conn
            .execute(
                "UPDATE api_keys SET expires_at = ?1 WHERE id = ?2",
                params![past, created.info.id],
            )
            .unwrap();

        // Returns opaque InvalidKey — does NOT reveal that the key expired.
        let result = store.verify_key(&created.plaintext_token);
        assert!(matches!(result, Err(AuthError::InvalidKey(_))));
    }

    #[test]
    fn verify_updates_last_used() {
        let mut store = test_store();
        let created = store
            .create_key(
                "track-me",
                PrincipalKind::Human,
                &trawl_grants(Role::Analyst),
                None,
            )
            .unwrap();

        // Before verification, last_used should be None.
        let keys = store.list_keys(false).unwrap();
        assert!(keys[0].last_used.is_none());

        // After verification, last_used should be set.
        store.verify_key(&created.plaintext_token).unwrap();
        let keys = store.list_keys(false).unwrap();
        assert!(keys[0].last_used.is_some());
    }

    #[test]
    fn list_keys_active_only() {
        let mut store = test_store();
        let created = store
            .create_key(
                "keep",
                PrincipalKind::Human,
                &trawl_grants(Role::Admin),
                None,
            )
            .unwrap();
        let revoked = store
            .create_key(
                "remove",
                PrincipalKind::Human,
                &trawl_grants(Role::Reader),
                None,
            )
            .unwrap();
        store.revoke_key(&revoked.info.prefix).unwrap();

        let active = store.list_keys(true).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].prefix, created.info.prefix);

        let all = store.list_keys(false).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn revoke_nonexistent_prefix() {
        let store = test_store();
        let result = store.revoke_key("ZZZZZZZZ");
        assert!(matches!(result, Err(AuthError::KeyNotFound { .. })));
    }

    #[test]
    fn revoke_already_revoked() {
        let mut store = test_store();
        let created = store
            .create_key(
                "double-revoke",
                PrincipalKind::Human,
                &trawl_grants(Role::Analyst),
                None,
            )
            .unwrap();
        store.revoke_key(&created.info.prefix).unwrap();

        let result = store.revoke_key(&created.info.prefix);
        assert!(matches!(result, Err(AuthError::KeyRevoked { .. })));
    }

    #[test]
    fn create_ingest_key() {
        let mut store = test_store();
        let created = store
            .create_key(
                "vector",
                PrincipalKind::Service,
                &trawl_grants(Role::Ingest),
                None,
            )
            .unwrap();

        assert_eq!(created.info.trawl_role(), Some(Role::Ingest));
        let verified = store.verify_key(&created.plaintext_token).unwrap();
        assert_eq!(verified.trawl_role(), Some(Role::Ingest));
    }

    #[test]
    fn duplicate_name_allowed() {
        let mut store = test_store();
        let a = store
            .create_key(
                "same-name",
                PrincipalKind::Human,
                &trawl_grants(Role::Admin),
                None,
            )
            .unwrap();
        let b = store
            .create_key(
                "same-name",
                PrincipalKind::Human,
                &trawl_grants(Role::Reader),
                None,
            )
            .unwrap();
        assert_ne!(a.info.prefix, b.info.prefix);
    }

    #[test]
    fn create_key_with_expiry() {
        let mut store = test_store();
        let created = store
            .create_key(
                "expiring",
                PrincipalKind::Human,
                &trawl_grants(Role::Analyst),
                Some(Duration::from_secs(86400 * 90)),
            )
            .unwrap();
        assert!(created.info.expires_at.is_some());
    }

    #[test]
    fn malformed_token_rejected() {
        let store = test_store();
        let result = store.verify_key("not-a-trawl-token");
        assert!(matches!(result, Err(AuthError::MalformedToken(_))));
    }

    #[test]
    fn grant_and_revoke_assignment_roundtrip() {
        let mut store = test_store();
        let created = store
            .create_key(
                "shared",
                PrincipalKind::Service,
                &trawl_grants(Role::Analyst),
                None,
            )
            .unwrap();

        store
            .grant_assignment(&created.info.prefix, &grant("coastwatch", "siem_consumer"))
            .unwrap();

        let assignments = store.list_assignments(&created.info.prefix).unwrap();
        assert_eq!(assignments.len(), 2);

        store
            .revoke_assignment(&created.info.prefix, "coastwatch")
            .unwrap();
        let assignments = store.list_assignments(&created.info.prefix).unwrap();
        assert_eq!(assignments.len(), 1);
    }

    #[test]
    fn grant_rejects_duplicate_app() {
        let mut store = test_store();
        let created = store
            .create_key(
                "k",
                PrincipalKind::Human,
                &trawl_grants(Role::Analyst),
                None,
            )
            .unwrap();
        let err = store
            .grant_assignment(&created.info.prefix, &grant("trawl", "admin"))
            .unwrap_err();
        assert!(matches!(err, AuthError::GrantExists { .. }));
    }

    #[test]
    fn revoke_missing_grant_errors() {
        let mut store = test_store();
        let created = store
            .create_key(
                "k",
                PrincipalKind::Human,
                &trawl_grants(Role::Analyst),
                None,
            )
            .unwrap();
        let err = store
            .revoke_assignment(&created.info.prefix, "nonexistent")
            .unwrap_err();
        assert!(matches!(err, AuthError::GrantNotFound { .. }));
    }

    #[test]
    fn retype_key_changes_kind() {
        let mut store = test_store();
        let created = store
            .create_key(
                "morphing",
                PrincipalKind::Human,
                &trawl_grants(Role::Analyst),
                None,
            )
            .unwrap();
        let updated = store
            .retype_key(&created.info.prefix, PrincipalKind::Service)
            .unwrap();
        assert_eq!(updated.kind, PrincipalKind::Service);
    }

    #[test]
    fn retype_revoked_key_errors() {
        let mut store = test_store();
        let created = store
            .create_key(
                "doomed",
                PrincipalKind::Human,
                &trawl_grants(Role::Reader),
                None,
            )
            .unwrap();
        store.revoke_key(&created.info.prefix).unwrap();
        let err = store
            .retype_key(&created.info.prefix, PrincipalKind::Service)
            .unwrap_err();
        assert!(matches!(err, AuthError::KeyRevoked { .. }));
    }

    #[test]
    fn create_key_rejects_duplicate_app_in_batch() {
        let mut store = test_store();
        let err = store
            .create_key(
                "dup-grants",
                PrincipalKind::Human,
                &[
                    RoleAssignment {
                        app: "trawl".into(),
                        role: "admin".into(),
                    },
                    RoleAssignment {
                        app: "trawl".into(),
                        role: "reader".into(),
                    },
                ],
                None,
            )
            .unwrap_err();
        assert!(matches!(err, AuthError::GrantExists { .. }));
    }

    #[test]
    fn migrate_v1_to_v3_preserves_keys() {
        // Manually create a v1 database (without 'ingest' in CHECK constraint,
        // no kind column, no assignment table).
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER NOT NULL);
             INSERT INTO schema_version (version) VALUES (1);

             CREATE TABLE api_keys (
                 id          INTEGER PRIMARY KEY AUTOINCREMENT,
                 prefix      TEXT    NOT NULL UNIQUE,
                 name        TEXT    NOT NULL,
                 hash        TEXT    NOT NULL,
                 role        TEXT    NOT NULL CHECK (role IN ('admin', 'analyst', 'reader')),
                 active      INTEGER NOT NULL DEFAULT 1,
                 created_at  TEXT    NOT NULL,
                 expires_at  TEXT,
                 last_used   TEXT,
                 revoked_at  TEXT
             );",
        )
        .unwrap();

        // Insert a key using v1 schema.
        let generated = token::generate_token();
        let hash = token::hash_token(&generated.plaintext).unwrap();
        conn.execute(
            "INSERT INTO api_keys (prefix, name, hash, role, created_at)
             VALUES (?1, ?2, ?3, 'analyst', datetime('now'))",
            params![generated.prefix, "v1-key", hash],
        )
        .unwrap();

        // Now wrap in KeyStore and trigger migrations via initialize().
        let store = KeyStore { conn };
        store.initialize().unwrap();

        // Schema version should be 3.
        let version: i64 = store
            .conn
            .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 3);

        // The v1 key should still verify correctly and carry a `trawl:analyst` grant.
        let verified = store.verify_key(&generated.plaintext).unwrap();
        assert_eq!(verified.name, "v1-key");
        assert_eq!(verified.kind, PrincipalKind::Human);
        assert_eq!(verified.trawl_role(), Some(Role::Analyst));
        assert_eq!(verified.assignments.len(), 1);
    }

    #[test]
    fn migrate_v2_to_v3_backfills_grants() {
        // Manually create a v2 database with two keys of different roles.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER NOT NULL);
             INSERT INTO schema_version (version) VALUES (2);

             CREATE TABLE api_keys (
                 id          INTEGER PRIMARY KEY AUTOINCREMENT,
                 prefix      TEXT    NOT NULL,
                 name        TEXT    NOT NULL,
                 hash        TEXT    NOT NULL,
                 role        TEXT    NOT NULL CHECK (role IN ('admin', 'analyst', 'reader', 'ingest')),
                 active      INTEGER NOT NULL DEFAULT 1,
                 created_at  TEXT    NOT NULL,
                 expires_at  TEXT,
                 last_used   TEXT,
                 revoked_at  TEXT
             );
             CREATE UNIQUE INDEX idx_api_keys_prefix ON api_keys (prefix);",
        )
        .unwrap();

        let g_admin = token::generate_token();
        let g_ingest = token::generate_token();
        conn.execute(
            "INSERT INTO api_keys (prefix, name, hash, role, created_at)
             VALUES (?1, ?2, ?3, 'admin', datetime('now'))",
            params![
                g_admin.prefix,
                "admin-key",
                token::hash_token(&g_admin.plaintext).unwrap()
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO api_keys (prefix, name, hash, role, created_at)
             VALUES (?1, ?2, ?3, 'ingest', datetime('now'))",
            params![
                g_ingest.prefix,
                "vector",
                token::hash_token(&g_ingest.plaintext).unwrap()
            ],
        )
        .unwrap();

        let store = KeyStore { conn };
        store.initialize().unwrap();

        let version: i64 = store
            .conn
            .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 3);

        // Both keys should have exactly one ('trawl', <old_role>) grant.
        let admin = store.verify_key(&g_admin.plaintext).unwrap();
        assert_eq!(admin.kind, PrincipalKind::Human);
        assert_eq!(admin.assignments.len(), 1);
        assert_eq!(admin.trawl_role(), Some(Role::Admin));

        let ingest = store.verify_key(&g_ingest.plaintext).unwrap();
        assert_eq!(ingest.kind, PrincipalKind::Human);
        assert_eq!(ingest.trawl_role(), Some(Role::Ingest));

        // api_keys.role column must no longer exist.
        let cols: Vec<String> = store
            .conn
            .prepare("PRAGMA table_info(api_keys)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(cols.contains(&"kind".to_owned()));
        assert!(!cols.contains(&"role".to_owned()));
    }

    #[cfg(unix)]
    #[test]
    fn auth_db_created_with_restricted_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("auth.db");

        let _store = KeyStore::open(&db_path).unwrap();

        let perms = std::fs::metadata(&db_path).unwrap().permissions();
        let mode = perms.mode() & 0o777;
        assert_eq!(mode, 0o600, "auth.db should be owner-only, got {mode:o}");
    }

    #[test]
    fn get_key_id_by_prefix() {
        let mut store = test_store();
        let created = store
            .create_key(
                "test-key",
                PrincipalKind::Human,
                &trawl_grants(Role::Analyst),
                None,
            )
            .unwrap();

        let key_id = store.get_key_id_by_prefix(&created.info.prefix).unwrap();
        assert_eq!(key_id, created.info.id);
    }

    #[test]
    fn get_key_id_nonexistent_prefix() {
        let store = test_store();
        let result = store.get_key_id_by_prefix("ZZZZZZZZ");
        assert!(matches!(result, Err(AuthError::KeyNotFound { .. })));
    }

    #[cfg(unix)]
    #[test]
    fn auth_db_fixes_loose_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("auth.db");

        // Create with world-readable permissions.
        std::fs::write(&db_path, b"").unwrap();
        std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let _store = KeyStore::open(&db_path).unwrap();

        let perms = std::fs::metadata(&db_path).unwrap().permissions();
        let mode = perms.mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "auth.db should be tightened to owner-only, got {mode:o}"
        );
    }
}
