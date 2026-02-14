//! `SQLite`-backed storage for saved queries.
//!
//! [`SavedQueryStore`] maintains user-scoped named queries for quick access
//! to common searches. Shares the auth database file with [`KeyStore`] via
//! separate connection (safe with WAL mode).

use std::path::Path;

use chrono::Utc;
use rusqlite::params;

use crate::error::AuthError;

/// Map a row from the `saved_queries` table to a [`SavedQuery`].
///
/// Expects columns in order: id, `key_id`, name, query, `created_at`, `updated_at`.
fn row_to_saved_query(row: &rusqlite::Row<'_>) -> Result<SavedQuery, rusqlite::Error> {
    Ok(SavedQuery {
        id: row.get(0)?,
        key_id: row.get(1)?,
        name: row.get(2)?,
        query: row.get(3)?,
        created_at: row.get(4)?,
        updated_at: row.get(5)?,
    })
}

/// A single saved query entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedQuery {
    pub id: i64,
    pub key_id: i64,
    pub name: String,
    pub query: String,
    pub created_at: String,
    pub updated_at: String,
}

/// `SQLite`-backed storage for saved queries.
#[derive(Debug)]
pub struct SavedQueryStore {
    conn: rusqlite::Connection,
}

impl SavedQueryStore {
    /// Open the auth database at the given path and ensure the `saved_queries` table exists.
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

    /// Ensure the `saved_queries` table exists.
    fn initialize(&self) -> Result<(), AuthError> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS saved_queries (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                key_id      INTEGER NOT NULL,
                name        TEXT    NOT NULL,
                query       TEXT    NOT NULL,
                created_at  TEXT    NOT NULL,
                updated_at  TEXT    NOT NULL,

                UNIQUE (key_id, name)
            );

            CREATE INDEX IF NOT EXISTS idx_saved_queries_key ON saved_queries (key_id);",
        )?;
        Ok(())
    }

    /// List all saved queries for a user.
    pub fn list(&self, key_id: i64) -> Result<Vec<SavedQuery>, AuthError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, key_id, name, query, created_at, updated_at
             FROM saved_queries
             WHERE key_id = ?1
             ORDER BY name ASC",
        )?;

        let queries = stmt
            .query_map(params![key_id], row_to_saved_query)?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(queries)
    }

    /// Create a new saved query.
    ///
    /// Returns `Conflict` if a query with the same name already exists for this user.
    pub fn create(&self, key_id: i64, name: &str, query: &str) -> Result<SavedQuery, AuthError> {
        let now = Utc::now().to_rfc3339();

        match self.conn.execute(
            "INSERT INTO saved_queries (key_id, name, query, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![key_id, name, query, &now, &now],
        ) {
            Ok(_) => {
                let id = self.conn.last_insert_rowid();

                tracing::info!(
                    event_type = "saved_query_created",
                    saved_id = id,
                    key_id,
                    name,
                    "Saved query created"
                );

                Ok(SavedQuery {
                    id,
                    key_id,
                    name: name.to_owned(),
                    query: query.to_owned(),
                    created_at: now.clone(),
                    updated_at: now,
                })
            }
            Err(rusqlite::Error::SqliteFailure(err, _))
                if err.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Err(AuthError::DuplicateName {
                    name: name.to_owned(),
                })
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Update an existing saved query.
    ///
    /// Returns `NotFound` if the query doesn't exist or doesn't belong to the user.
    pub fn update(&self, id: i64, key_id: i64, query: &str) -> Result<SavedQuery, AuthError> {
        let now = Utc::now().to_rfc3339();

        let updated = self.conn.execute(
            "UPDATE saved_queries SET query = ?1, updated_at = ?2 WHERE id = ?3 AND key_id = ?4",
            params![query, &now, id, key_id],
        )?;

        if updated == 0 {
            return Err(AuthError::NotFound {
                id,
                resource: "saved query".into(),
            });
        }

        tracing::info!(
            event_type = "saved_query_updated",
            saved_id = id,
            key_id,
            "Saved query updated"
        );

        // Return the updated query.
        let mut stmt = self.conn.prepare(
            "SELECT id, key_id, name, query, created_at, updated_at
             FROM saved_queries WHERE id = ?1",
        )?;
        stmt.query_row(params![id], row_to_saved_query)
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => AuthError::NotFound {
                    id,
                    resource: "saved query".into(),
                },
                e => e.into(),
            })
    }

    /// Delete a saved query.
    ///
    /// Returns `NotFound` if the query doesn't exist or doesn't belong to the user.
    pub fn delete(&self, id: i64, key_id: i64) -> Result<(), AuthError> {
        let deleted = self.conn.execute(
            "DELETE FROM saved_queries WHERE id = ?1 AND key_id = ?2",
            params![id, key_id],
        )?;

        if deleted == 0 {
            return Err(AuthError::NotFound {
                id,
                resource: "saved query".into(),
            });
        }

        tracing::info!(
            event_type = "saved_query_deleted",
            saved_id = id,
            key_id,
            "Saved query deleted"
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> SavedQueryStore {
        SavedQueryStore::open_in_memory().expect("failed to open in-memory store")
    }

    #[test]
    fn open_in_memory_succeeds() {
        let _store = test_store();
    }

    #[test]
    fn create_and_list_query() {
        let store = test_store();
        let key_id = 1;

        let saved = store
            .create(key_id, "nginx errors", "service:nginx level:error")
            .unwrap();

        assert_eq!(saved.name, "nginx errors");
        assert_eq!(saved.query, "service:nginx level:error");

        let list = store.list(key_id).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "nginx errors");
    }

    #[test]
    fn duplicate_name_returns_error() {
        let store = test_store();
        let key_id = 1;

        store.create(key_id, "test", "query 1").unwrap();
        let result = store.create(key_id, "test", "query 2");

        assert!(matches!(result, Err(AuthError::DuplicateName { .. })));
    }

    #[test]
    fn update_query() {
        let store = test_store();
        let key_id = 1;

        let created = store.create(key_id, "test", "original query").unwrap();

        let updated = store.update(created.id, key_id, "updated query").unwrap();

        assert_eq!(updated.query, "updated query");
        assert_eq!(updated.name, "test");
        assert_ne!(updated.updated_at, updated.created_at);
    }

    #[test]
    fn update_nonexistent_returns_error() {
        let store = test_store();
        let result = store.update(999, 1, "query");
        assert!(matches!(result, Err(AuthError::NotFound { .. })));
    }

    #[test]
    fn update_other_users_query_returns_error() {
        let store = test_store();
        let created = store.create(1, "test", "query").unwrap();

        // Try to update with a different key_id.
        let result = store.update(created.id, 2, "updated");
        assert!(matches!(result, Err(AuthError::NotFound { .. })));
    }

    #[test]
    fn delete_query() {
        let store = test_store();
        let key_id = 1;

        let created = store.create(key_id, "test", "query").unwrap();
        store.delete(created.id, key_id).unwrap();

        let list = store.list(key_id).unwrap();
        assert_eq!(list.len(), 0);
    }

    #[test]
    fn delete_nonexistent_returns_error() {
        let store = test_store();
        let result = store.delete(999, 1);
        assert!(matches!(result, Err(AuthError::NotFound { .. })));
    }

    #[test]
    fn delete_other_users_query_returns_error() {
        let store = test_store();
        let created = store.create(1, "test", "query").unwrap();

        // Try to delete with a different key_id.
        let result = store.delete(created.id, 2);
        assert!(matches!(result, Err(AuthError::NotFound { .. })));
    }

    #[test]
    fn user_isolation() {
        let store = test_store();

        store.create(1, "user1 query", "query 1").unwrap();
        store.create(2, "user2 query", "query 2").unwrap();
        store.create(1, "user1 another", "query 3").unwrap();

        let user1 = store.list(1).unwrap();
        assert_eq!(user1.len(), 2);
        assert_eq!(user1[0].name, "user1 another");
        assert_eq!(user1[1].name, "user1 query");

        let user2 = store.list(2).unwrap();
        assert_eq!(user2.len(), 1);
        assert_eq!(user2[0].name, "user2 query");
    }

    #[test]
    fn list_sorted_by_name() {
        let store = test_store();

        store.create(1, "z last", "query").unwrap();
        store.create(1, "a first", "query").unwrap();
        store.create(1, "m middle", "query").unwrap();

        let list = store.list(1).unwrap();
        assert_eq!(list[0].name, "a first");
        assert_eq!(list[1].name, "m middle");
        assert_eq!(list[2].name, "z last");
    }

    #[test]
    fn empty_list() {
        let store = test_store();
        let list = store.list(999).unwrap();
        assert_eq!(list.len(), 0);
    }
}
