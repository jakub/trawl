//! `SQLite`-backed storage for API keys.
//!
//! [`KeyStore`] owns a `rusqlite::Connection` and provides the full key
//! lifecycle: creation, verification, listing, and revocation.

use std::path::Path;
use std::sync::LazyLock;
use std::time::Duration;

use chrono::Utc;
use rusqlite::OptionalExtension as _;
use rusqlite::params;

use crate::error::AuthError;
use crate::keys::{ApiKeyInfo, CreatedKey, VerifiedKey};
use crate::roles::Role;
use crate::token;

/// Current schema version. Bumped when migrations are needed.
const SCHEMA_VERSION: i64 = 2;

/// Pre-computed dummy argon2id hash used to equalize timing when a prefix
/// lookup returns no rows. Generated once at process start so that the
/// `verify_token` call on the miss path takes the same time as on the hit path.
static DUMMY_HASH: LazyLock<String> =
    LazyLock::new(|| token::hash_token("dummy-timing-equalization").expect("failed to hash dummy"));

/// Map a row from the `api_keys` table to an [`ApiKeyInfo`].
///
/// Expects columns in order: id, prefix, name, role, active, `created_at`,
/// `expires_at`, `last_used`, `revoked_at`.
fn row_to_api_key_info(row: &rusqlite::Row<'_>) -> Result<ApiKeyInfo, rusqlite::Error> {
    Ok(ApiKeyInfo {
        id: row.get(0)?,
        prefix: row.get(1)?,
        name: row.get(2)?,
        role: row.get(3)?,
        active: row.get(4)?,
        created_at: row.get(5)?,
        expires_at: row.get(6)?,
        last_used: row.get(7)?,
        revoked_at: row.get(8)?,
    })
}

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
        // (e.g. fleet-admin revoking a key while daemon is authenticating).
        // busy_timeout: retry on SQLITE_BUSY instead of failing immediately.
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;
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
        conn.execute_batch("PRAGMA busy_timeout=5000;")?;
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
            self.create_tables_v2()?;
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

        Ok(())
    }

    /// Create the `api_keys` table at schema v2 (includes 'ingest' role).
    fn create_tables_v2(&self) -> Result<(), AuthError> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS api_keys (
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

            CREATE UNIQUE INDEX IF NOT EXISTS idx_api_keys_prefix ON api_keys (prefix);",
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

    /// Create a new API key.
    ///
    /// Returns the key info AND the plaintext token (which must be shown to
    /// the user immediately and never stored). Retries on prefix collision
    /// (UNIQUE constraint violation).
    pub fn create_key(
        &self,
        name: &str,
        role: Role,
        expires_in: Option<Duration>,
    ) -> Result<CreatedKey, AuthError> {
        /// Maximum retry attempts for prefix collision (48-bit prefix space
        /// means collisions are astronomically unlikely, but handle gracefully).
        const MAX_RETRIES: usize = 3;

        let now = Utc::now().to_rfc3339();
        let expires_at = expires_in.map(|d| {
            let delta = chrono::Duration::from_std(d).unwrap_or(chrono::Duration::MAX);
            (Utc::now() + delta).to_rfc3339()
        });

        for _ in 0..MAX_RETRIES {
            let generated = token::generate_token();
            let hash = token::hash_token(&generated.plaintext)?;

            match self.conn.execute(
                "INSERT INTO api_keys (prefix, name, hash, role, created_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![generated.prefix, name, hash, &role, now, expires_at,],
            ) {
                Ok(_) => {
                    let id = self.conn.last_insert_rowid();
                    tracing::info!(
                        event_type = "key_created",
                        key_id = id,
                        prefix = %generated.prefix,
                        name,
                        role = %role,
                        "API key created"
                    );
                    return Ok(CreatedKey {
                        info: ApiKeyInfo {
                            id,
                            prefix: generated.prefix,
                            name: name.to_owned(),
                            role,
                            active: true,
                            created_at: now,
                            expires_at,
                            last_used: None,
                            revoked_at: None,
                        },
                        plaintext_token: generated.plaintext.clone(),
                    });
                }
                // Prefix collision — retry with a new token.
                Err(rusqlite::Error::SqliteFailure(err, _))
                    if err.code == rusqlite::ErrorCode::ConstraintViolation => {}
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
            "SELECT id, prefix, name, hash, role, active, expires_at
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
                    row.get::<_, Role>(4)?,
                    row.get::<_, bool>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            })
            .optional()
            .map_err(AuthError::Database)?;

        let Some((id, db_prefix, name, hash, role, active, expires_at)) = row else {
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
            role,
        })
    }

    /// List all keys. Never exposes hashes.
    ///
    /// If `active_only` is true, only returns non-revoked keys.
    pub fn list_keys(&self, active_only: bool) -> Result<Vec<ApiKeyInfo>, AuthError> {
        let sql = if active_only {
            "SELECT id, prefix, name, role, active, created_at, expires_at, last_used, revoked_at
             FROM api_keys WHERE active = 1 ORDER BY created_at DESC"
        } else {
            "SELECT id, prefix, name, role, active, created_at, expires_at, last_used, revoked_at
             FROM api_keys ORDER BY created_at DESC"
        };

        let mut stmt = self.conn.prepare(sql)?;
        let keys = stmt
            .query_map([], row_to_api_key_info)?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(keys)
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

        // Return the updated key info with a direct query.
        let mut stmt = self.conn.prepare(
            "SELECT id, prefix, name, role, active, created_at, expires_at, last_used, revoked_at
             FROM api_keys WHERE prefix = ?1",
        )?;
        stmt.query_row(params![prefix], row_to_api_key_info)
            .optional()
            .map_err(AuthError::Database)?
            .ok_or_else(|| AuthError::KeyNotFound {
                prefix: prefix.to_owned(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> KeyStore {
        KeyStore::open_in_memory().expect("failed to open in-memory store")
    }

    #[test]
    fn open_in_memory_succeeds() {
        let _store = test_store();
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
        let store = test_store();
        let created = store.create_key("test-key", Role::Analyst, None).unwrap();

        assert!(created.plaintext_token.starts_with("flt_"));
        assert_eq!(created.info.name, "test-key");
        assert_eq!(created.info.role, Role::Analyst);
        assert!(created.info.active);
        assert!(created.info.expires_at.is_none());
    }

    #[test]
    fn create_and_verify_key() {
        let store = test_store();
        let created = store.create_key("my-key", Role::Admin, None).unwrap();

        let verified = store.verify_key(&created.plaintext_token).unwrap();
        assert_eq!(verified.name, "my-key");
        assert_eq!(verified.role, Role::Admin);
        assert_eq!(verified.prefix, created.info.prefix);
    }

    #[test]
    fn verify_nonexistent_key() {
        let store = test_store();
        let result = store.verify_key("flt_AAAAAAAAthisisnotarealkeyatall1234567");
        assert!(result.is_err());
    }

    #[test]
    fn verify_wrong_token_for_prefix() {
        let store = test_store();
        let created = store.create_key("test", Role::Reader, None).unwrap();

        // Tamper with the token body but keep the prefix intact.
        let prefix = &created.plaintext_token[..12]; // "flt_" + 8 chars
        let tampered = format!("{prefix}AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA0");
        let result = store.verify_key(&tampered);
        assert!(result.is_err());
    }

    #[test]
    fn revoke_key_blocks_verification() {
        let store = test_store();
        let created = store.create_key("revoke-me", Role::Analyst, None).unwrap();

        store.revoke_key(&created.info.prefix).unwrap();

        // Returns opaque InvalidKey — does NOT reveal that the key was revoked.
        let result = store.verify_key(&created.plaintext_token);
        assert!(matches!(result, Err(AuthError::InvalidKey(_))));
    }

    #[test]
    fn expired_key_fails_verification() {
        let store = test_store();
        let created = store.create_key("expired", Role::Reader, None).unwrap();

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
        let store = test_store();
        let created = store.create_key("track-me", Role::Analyst, None).unwrap();

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
        let store = test_store();
        let created = store.create_key("keep", Role::Admin, None).unwrap();
        let revoked = store.create_key("remove", Role::Reader, None).unwrap();
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
        let store = test_store();
        let created = store
            .create_key("double-revoke", Role::Analyst, None)
            .unwrap();
        store.revoke_key(&created.info.prefix).unwrap();

        let result = store.revoke_key(&created.info.prefix);
        assert!(matches!(result, Err(AuthError::KeyRevoked { .. })));
    }

    #[test]
    fn create_ingest_key() {
        let store = test_store();
        let created = store.create_key("vector", Role::Ingest, None).unwrap();

        assert_eq!(created.info.role, Role::Ingest);
        let verified = store.verify_key(&created.plaintext_token).unwrap();
        assert_eq!(verified.role, Role::Ingest);
    }

    #[test]
    fn duplicate_name_allowed() {
        let store = test_store();
        let a = store.create_key("same-name", Role::Admin, None).unwrap();
        let b = store.create_key("same-name", Role::Reader, None).unwrap();
        assert_ne!(a.info.prefix, b.info.prefix);
    }

    #[test]
    fn create_key_with_expiry() {
        let store = test_store();
        let created = store
            .create_key(
                "expiring",
                Role::Analyst,
                Some(Duration::from_secs(86400 * 90)),
            )
            .unwrap();
        assert!(created.info.expires_at.is_some());
    }

    #[test]
    fn malformed_token_rejected() {
        let store = test_store();
        let result = store.verify_key("not-a-fleet-token");
        assert!(matches!(result, Err(AuthError::MalformedToken(_))));
    }

    #[test]
    fn migrate_v1_to_v2_preserves_keys() {
        // Manually create a v1 database (without 'ingest' in CHECK constraint).
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

        // Now wrap in KeyStore and trigger migration via initialize().
        let store = KeyStore { conn };
        store.initialize().unwrap();

        // Schema version should be 2.
        let version: i64 = store
            .conn
            .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 2);

        // The v1 key should still verify correctly.
        let verified = store.verify_key(&generated.plaintext).unwrap();
        assert_eq!(verified.name, "v1-key");
        assert_eq!(verified.role, Role::Analyst);

        // New 'ingest' role should now be accepted.
        let ingest_key = store.create_key("ingester", Role::Ingest, None).unwrap();
        assert_eq!(ingest_key.info.role, Role::Ingest);
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
        let store = test_store();
        let created = store.create_key("test-key", Role::Analyst, None).unwrap();

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
