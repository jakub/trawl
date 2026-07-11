// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Legacy-db quarantine (ADR-0004).
//!
//! The fleet-auth cutover moved the API-key keystore to postgres, whose key
//! ids are a fresh sequence unrelated to the old `SQLite` keystore's. The
//! transitional app-state store (history, saved queries, schedules) keys its
//! rows on those key ids, so pointing it at a pre-cutover `auth.db` would let a
//! new postgres key with id N silently inherit the legacy sqlite key N's rows —
//! including auto-executing schedules that fire under a mismatched identity.
//!
//! [`Config::validate`](../../trawl_config) also rejects the default `auth.db`
//! basename, but that is only a naming-convention hint: a legacy keystore
//! renamed to anything else (`keys.db`, a copied `store.db`, …) sails past it.
//! This content check is the real control — it inspects the file's schema, so
//! it catches a legacy keystore regardless of what it is named.

use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::AuthError;

/// Tables that only ever existed in a pre-cutover keystore `auth.db`. The
/// transitional app-state store carries `query_history`, `saved_queries`,
/// `schedules`, and `report_runs` — never any of these.
const LEGACY_KEYSTORE_TABLES: &[&str] = &[
    "api_keys",
    "api_keys_v2",
    "api_keys_v3",
    "api_key_role_assignment",
];

/// Refuse to reuse a pre-cutover keystore file as the transitional app-state
/// store.
///
/// Returns [`AuthError::LegacyKeystore`] when the `SQLite` database at `path`
/// carries a legacy keystore table. A missing file (fresh install) or a file
/// that holds only app-state tables passes.
///
/// # Errors
///
/// Returns [`AuthError::Database`] if the file exists but cannot be opened or
/// queried, and [`AuthError::LegacyKeystore`] if it is a legacy keystore.
pub fn reject_legacy_keystore(path: impl AsRef<Path>) -> Result<(), AuthError> {
    let path = path.as_ref();
    // A missing file cannot be a legacy keystore. Open read-only so the check
    // never creates an empty database as a side effect.
    if !path.exists() {
        return Ok(());
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    check_connection(&conn, &path.display().to_string())
}

/// Inspect an open connection's schema for legacy keystore tables.
fn check_connection(conn: &Connection, path: &str) -> Result<(), AuthError> {
    for &table in LEGACY_KEYSTORE_TABLES {
        let found: Option<String> = conn
            .query_row(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(table) = found {
            return Err(AuthError::LegacyKeystore {
                path: path.to_string(),
                table,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_passes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.db");
        reject_legacy_keystore(&path).unwrap();
    }

    #[test]
    fn fresh_app_state_store_passes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE query_history (id INTEGER PRIMARY KEY);
             CREATE TABLE saved_queries (id INTEGER PRIMARY KEY);",
        )
        .unwrap();
        drop(conn);
        reject_legacy_keystore(&path).unwrap();
    }

    #[test]
    fn renamed_legacy_keystore_is_rejected() {
        // The whole point: a legacy keystore renamed away from `auth.db`
        // (basename guard can't see it) is still caught by content.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE api_keys_v3 (id INTEGER PRIMARY KEY);
             CREATE TABLE query_history (id INTEGER PRIMARY KEY);",
        )
        .unwrap();
        drop(conn);
        let err = reject_legacy_keystore(&path).unwrap_err();
        assert!(
            matches!(err, AuthError::LegacyKeystore { ref table, .. } if table == "api_keys_v3"),
            "got: {err}"
        );
    }

    #[test]
    fn legacy_api_keys_table_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE api_keys (id INTEGER PRIMARY KEY);")
            .unwrap();
        drop(conn);
        let err = reject_legacy_keystore(&path).unwrap_err();
        assert!(
            matches!(err, AuthError::LegacyKeystore { .. }),
            "got: {err}"
        );
    }
}
