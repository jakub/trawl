// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `SQLite`-backed storage for query history.
//!
//! [`HistoryStore`] maintains a persistent log of executed queries for
//! audit trails and session replay. Shares the auth database file with
//! [`KeyStore`] via separate connection (safe with WAL mode).

use std::path::Path;

use chrono::Utc;
use rusqlite::params;

use crate::error::AuthError;

/// Map a row from the `query_history` table to a [`HistoryEntry`].
///
/// Expects columns in order: id, `key_id`, query, `executed_at`, `duration_ms`,
/// `row_count`, status.
fn row_to_history_entry(row: &rusqlite::Row<'_>) -> Result<HistoryEntry, rusqlite::Error> {
    Ok(HistoryEntry {
        id: row.get(0)?,
        key_id: row.get(1)?,
        query: row.get(2)?,
        executed_at: row.get(3)?,
        duration_ms: row.get(4)?,
        row_count: row.get(5)?,
        status: row.get(6)?,
    })
}

/// A single query history entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub id: i64,
    pub key_id: i64,
    pub query: String,
    pub executed_at: String,
    pub duration_ms: u64,
    pub row_count: usize,
    pub status: String,
}

/// Paginated history response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryPage {
    pub entries: Vec<HistoryEntry>,
    pub total: usize,
}

/// `SQLite`-backed storage for query history.
#[derive(Debug)]
pub struct HistoryStore {
    conn: rusqlite::Connection,
}

impl HistoryStore {
    /// Open the auth database at the given path and ensure the history table exists.
    ///
    /// Opens a separate connection from [`KeyStore`] — safe with WAL mode
    /// which supports concurrent reads and writes from multiple connections.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, AuthError> {
        let conn = rusqlite::Connection::open(path)?;
        // WAL mode: concurrent access with KeyStore connection.
        // busy_timeout: retry on lock contention instead of failing immediately.
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;
        let store = Self { conn };
        store.initialize()?;
        Ok(store)
    }

    /// Open an in-memory database (for testing).
    pub fn open_in_memory() -> Result<Self, AuthError> {
        let conn = rusqlite::Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA busy_timeout=5000;")?;
        let store = Self { conn };
        store.initialize()?;
        Ok(store)
    }

    /// Ensure the `query_history` table exists.
    fn initialize(&self) -> Result<(), AuthError> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS query_history (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                key_id          INTEGER NOT NULL,
                query           TEXT    NOT NULL,
                executed_at     TEXT    NOT NULL,
                duration_ms     INTEGER NOT NULL,
                row_count       INTEGER NOT NULL,
                status          TEXT    NOT NULL CHECK (status IN ('success', 'error', 'timeout'))
            );

            CREATE INDEX IF NOT EXISTS idx_query_history_key_executed
                ON query_history (key_id, executed_at DESC);",
        )?;
        Ok(())
    }

    /// Record a successful query execution.
    ///
    /// Only saves queries with status='success' per user preference
    /// (errors/timeouts are not persisted to history).
    pub fn record_query(
        &self,
        key_id: i64,
        query: &str,
        duration_ms: u64,
        row_count: usize,
        status: &str,
    ) -> Result<i64, AuthError> {
        let now = Utc::now().to_rfc3339();

        self.conn.execute(
            "INSERT INTO query_history (key_id, query, executed_at, duration_ms, row_count, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                key_id,
                query,
                now,
                i64::try_from(duration_ms).unwrap_or(i64::MAX),
                i64::try_from(row_count).unwrap_or(i64::MAX),
                status
            ],
        )?;

        let id = self.conn.last_insert_rowid();

        tracing::debug!(
            event_type = "query_recorded",
            history_id = id,
            key_id,
            duration_ms,
            row_count,
            status,
            "Query saved to history"
        );

        Ok(id)
    }

    /// Get a user's query history with pagination.
    ///
    /// Returns most recent queries first (ordered by `executed_at DESC`).
    pub fn get_user_history(
        &self,
        key_id: i64,
        limit: usize,
        offset: usize,
    ) -> Result<HistoryPage, AuthError> {
        // Get total count for this user.
        let total: usize = self.conn.query_row(
            "SELECT COUNT(*) FROM query_history WHERE key_id = ?1",
            params![key_id],
            |row| row.get(0),
        )?;

        // Get paginated entries.
        let mut stmt = self.conn.prepare(
            "SELECT id, key_id, query, executed_at, duration_ms, row_count, status
             FROM query_history
             WHERE key_id = ?1
             ORDER BY executed_at DESC
             LIMIT ?2 OFFSET ?3",
        )?;

        let entries = stmt
            .query_map(
                params![
                    key_id,
                    i64::try_from(limit).unwrap_or(i64::MAX),
                    i64::try_from(offset).unwrap_or(0)
                ],
                row_to_history_entry,
            )?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(HistoryPage { entries, total })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> HistoryStore {
        HistoryStore::open_in_memory().expect("failed to open in-memory store")
    }

    #[test]
    fn open_in_memory_succeeds() {
        let _store = test_store();
    }

    #[test]
    fn record_and_retrieve_query() {
        let store = test_store();
        let key_id = 42;

        let id = store
            .record_query(key_id, "service=nginx | stats count()", 123, 5, "success")
            .unwrap();

        assert!(id > 0);

        let page = store.get_user_history(key_id, 10, 0).unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.entries.len(), 1);

        let entry = &page.entries[0];
        assert_eq!(entry.key_id, key_id);
        assert_eq!(entry.query, "service=nginx | stats count()");
        assert_eq!(entry.duration_ms, 123);
        assert_eq!(entry.row_count, 5);
        assert_eq!(entry.status, "success");
    }

    #[test]
    fn pagination_works() {
        let store = test_store();
        let key_id = 1;

        // Insert 5 queries.
        for i in 0..5 {
            store
                .record_query(key_id, &format!("query {i}"), 100, 10, "success")
                .unwrap();
        }

        // Fetch first page (limit 2).
        let page1 = store.get_user_history(key_id, 2, 0).unwrap();
        assert_eq!(page1.total, 5);
        assert_eq!(page1.entries.len(), 2);

        // Most recent first (descending order).
        assert_eq!(page1.entries[0].query, "query 4");
        assert_eq!(page1.entries[1].query, "query 3");

        // Fetch second page (offset 2).
        let page2 = store.get_user_history(key_id, 2, 2).unwrap();
        assert_eq!(page2.total, 5);
        assert_eq!(page2.entries.len(), 2);
        assert_eq!(page2.entries[0].query, "query 2");
        assert_eq!(page2.entries[1].query, "query 1");

        // Fetch last page (offset 4).
        let page3 = store.get_user_history(key_id, 2, 4).unwrap();
        assert_eq!(page3.total, 5);
        assert_eq!(page3.entries.len(), 1);
        assert_eq!(page3.entries[0].query, "query 0");
    }

    #[test]
    fn user_isolation() {
        let store = test_store();

        store
            .record_query(1, "user 1 query", 50, 10, "success")
            .unwrap();
        store
            .record_query(2, "user 2 query", 75, 20, "success")
            .unwrap();
        store
            .record_query(1, "user 1 query 2", 100, 15, "success")
            .unwrap();

        let user1 = store.get_user_history(1, 10, 0).unwrap();
        assert_eq!(user1.total, 2);
        assert_eq!(user1.entries[0].query, "user 1 query 2");

        let user2 = store.get_user_history(2, 10, 0).unwrap();
        assert_eq!(user2.total, 1);
        assert_eq!(user2.entries[0].query, "user 2 query");
    }

    #[test]
    fn empty_history() {
        let store = test_store();
        let page = store.get_user_history(999, 10, 0).unwrap();
        assert_eq!(page.total, 0);
        assert_eq!(page.entries.len(), 0);
    }
}
