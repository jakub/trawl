// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Postgres-backed storage for query history.

use chrono::{DateTime, Utc};
use sqlx::postgres::PgRow;
use sqlx::{Acquire as _, PgPool, Row as _};

use super::error::{StoreError, classify_violation};
use super::status::{RunStatus, decode_status};

// Both statements bind the same key and optional literal needle. Keeping the
// predicate here prevents count and page selection from acquiring different
// matching semantics. Empty filters are normalized before either statement.
macro_rules! history_query {
    ($before:literal, $after:literal) => {
        concat!(
            $before,
            "key_id = $1 AND ($2::text IS NULL OR strpos(lower(query), lower($2::text)) > 0)",
            $after
        )
    };
}

/// A single query history entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub id: i64,
    pub key_id: i64,
    pub query: String,
    pub executed_at: DateTime<Utc>,
    pub duration_ms: u64,
    pub row_count: usize,
    pub status: RunStatus,
}

/// Paginated history response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryPage {
    pub entries: Vec<HistoryEntry>,
    pub total: usize,
}

fn row_to_history_entry(row: &PgRow) -> Result<HistoryEntry, sqlx::Error> {
    Ok(HistoryEntry {
        id: row.try_get("id")?,
        key_id: row.try_get("key_id")?,
        query: row.try_get("query")?,
        executed_at: row.try_get("executed_at")?,
        duration_ms: u64::try_from(row.try_get::<i64, _>("duration_ms")?).unwrap_or_default(),
        row_count: usize::try_from(row.try_get::<i64, _>("row_count")?).unwrap_or_default(),
        status: decode_status(row, "status")?,
    })
}

/// Clamp a usize into a non-negative i64 for binding.
pub(crate) fn bind_usize(v: usize) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// Clamp a u64 into a non-negative i64 for binding.
pub(crate) fn bind_u64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// Postgres-backed storage for query history. Cheap to clone (shared pool).
#[derive(Debug, Clone)]
pub struct HistoryStore {
    pool: PgPool,
    #[cfg(test)]
    read_barrier: std::sync::Arc<parking_lot::Mutex<Option<tests::ReadBarrier>>>,
}

impl HistoryStore {
    /// Wrap the shared app-state pool.
    ///
    /// The pool must point at an already-migrated trawl app-state database
    /// (production goes through `StorageState::connect`, which migrates;
    /// tests use `#[sqlx::test]`-migrated pools directly).
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            #[cfg(test)]
            read_barrier: std::sync::Arc::default(),
        }
    }

    /// Record a query execution. Returns the new history entry id.
    pub async fn record_query(
        &self,
        key_id: i64,
        query: &str,
        duration_ms: u64,
        row_count: usize,
        status: RunStatus,
    ) -> Result<i64, StoreError> {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO query_history (key_id, query, executed_at, duration_ms, row_count, status)
             VALUES ($1, $2, now(), $3, $4, $5)
             RETURNING id",
        )
        .bind(key_id)
        .bind(query)
        .bind(bind_u64(duration_ms))
        .bind(bind_usize(row_count))
        .bind(status.as_str())
        .fetch_one(&self.pool)
        .await
        .map_err(|e| match classify_violation(&e) {
            // Backstop: unreachable via the typed API (`running` is the only
            // out-of-history-domain variant and callers never pass it), but
            // kept so a future raw path still maps the CHECK to Validation.
            Some(super::error::PgViolation::Check) => {
                StoreError::Validation(format!("invalid history status {:?}", status.as_str()))
            }
            _ => StoreError::from(e),
        })?;

        tracing::debug!(
            event_type = "query_recorded",
            history_id = id,
            key_id,
            duration_ms,
            row_count,
            status = status.as_str(),
            "Query saved to history"
        );

        Ok(id)
    }

    /// Delete this key's history visible to the DELETE statement's snapshot.
    ///
    /// A concurrent query may commit a new history row after that snapshot,
    /// so a successful clear does not promise that history remains empty.
    pub async fn clear_user_history(&self, key_id: i64) -> Result<u64, StoreError> {
        Ok(sqlx::query("DELETE FROM query_history WHERE key_id = $1")
            .bind(key_id)
            .execute(&self.pool)
            .await?
            .rows_affected())
    }

    /// Get a user's query history, filtering query text before pagination.
    ///
    /// Most recent first — `ORDER BY executed_at DESC, id DESC` (the id
    /// tie-break makes same-timestamp pages deterministic).
    /// Matching is a literal substring after PostgreSQL's locale-dependent
    /// lowercase conversion. Whitespace is significant. Count and rows share
    /// one read-only, repeatable-read snapshot; subsequent requests may differ.
    pub async fn get_user_history(
        &self,
        key_id: i64,
        limit: usize,
        offset: usize,
        filter: Option<&str>,
    ) -> Result<HistoryPage, StoreError> {
        let filter = filter.filter(|needle| !needle.is_empty());
        let mut connection = self.pool.acquire().await?;
        let mut transaction = connection.begin().await?;
        // Configure the transaction before its first data query. A default
        // READ COMMITTED transaction permits count and rows to disagree.
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *transaction)
            .await?;

        let total: i64 = sqlx::query_scalar(history_query!(
            "SELECT COUNT(*) FROM query_history WHERE ",
            ""
        ))
        .bind(key_id)
        .bind(filter)
        .fetch_one(&mut *transaction)
        .await?;

        #[cfg(test)]
        {
            // Instance-scoped and consumed once. Tests pause the real method
            // after count, allowing another connection to commit a mutation.
            let barrier = self.read_barrier.lock().take();
            if let Some(barrier) = barrier {
                barrier.pause(&mut transaction).await?;
            }
        }

        let rows = sqlx::query(history_query!(
            "SELECT id, key_id, query, executed_at, duration_ms, row_count, status
             FROM query_history
             WHERE ",
            " ORDER BY executed_at DESC, id DESC
             LIMIT $3 OFFSET $4"
        ))
        .bind(key_id)
        .bind(filter)
        .bind(bind_usize(limit))
        .bind(bind_usize(offset))
        .fetch_all(&mut *transaction)
        .await?;

        let entries = rows
            .iter()
            .map(row_to_history_entry)
            .collect::<Result<Vec<_>, _>>()?;
        transaction.commit().await?;

        Ok(HistoryPage {
            entries,
            total: usize::try_from(total).unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;

    #[derive(Debug)]
    pub(super) struct ReadBarrier {
        counted: oneshot::Sender<(String, String, String, i32)>,
        resume: oneshot::Receiver<()>,
    }

    impl ReadBarrier {
        pub(super) async fn pause(
            self,
            connection: &mut sqlx::PgConnection,
        ) -> Result<(), sqlx::Error> {
            let settings = sqlx::query_as::<_, (String, String, String, i32)>(
                "SELECT current_setting('transaction_isolation'),
                        current_setting('transaction_read_only'),
                        current_setting('default_transaction_isolation'), pg_backend_pid()",
            )
            .fetch_one(connection)
            .await?;
            self.counted
                .send(settings)
                .expect("snapshot observer alive");
            self.resume.await.expect("snapshot test releases read");
            Ok(())
        }
    }

    async fn history_snapshot_with_mutation(pool: PgPool, clear: bool) {
        let store = HistoryStore::new(pool.clone());
        for (key, query) in [
            (1, "matching first"),
            (1, "matching second"),
            (2, "matching foreign"),
        ] {
            store
                .record_query(key, query, 1, 1, RunStatus::Success)
                .await
                .unwrap();
        }
        let before = store
            .get_user_history(1, 50, 0, Some("matching"))
            .await
            .unwrap();
        let (counted_tx, counted_rx) = oneshot::channel();
        let (resume_tx, resume_rx) = oneshot::channel();
        *store.read_barrier.lock() = Some(ReadBarrier {
            counted: counted_tx,
            resume: resume_rx,
        });
        let reader = store.clone();
        let reading = tokio::spawn(async move {
            reader
                .get_user_history(1, 50, 0, Some("matching"))
                .await
                .unwrap()
        });
        let (isolation, read_only, default_isolation, reader_pid) = counted_rx.await.unwrap();
        assert_eq!(isolation, "repeatable read");
        assert_eq!(read_only, "on");
        // This proves the method selected the stronger isolation rather than
        // accidentally inheriting it from the test database's defaults.
        assert_eq!(default_isolation, "read committed");

        let mut writer = pool.acquire().await.unwrap();
        let writer_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *writer)
            .await
            .unwrap();
        assert_ne!(reader_pid, writer_pid);
        if clear {
            sqlx::query("DELETE FROM query_history WHERE key_id = 1")
                .execute(&mut *writer)
                .await
                .unwrap();
        } else {
            sqlx::query("INSERT INTO query_history (key_id, query, executed_at, duration_ms, row_count, status)
                         VALUES (1, 'matching later', now(), 1, 1, 'success')")
                .execute(&mut *writer).await.unwrap();
        }
        // These autocommit statements complete before row selection resumes.
        resume_tx.send(()).unwrap();
        assert_eq!(reading.await.unwrap(), before);
        let after = store
            .get_user_history(1, 50, 0, Some("matching"))
            .await
            .unwrap();
        assert_eq!(after.total, if clear { 0 } else { 3 });
        assert_eq!(
            store.get_user_history(2, 50, 0, None).await.unwrap().total,
            1
        );
    }

    #[sqlx::test]
    async fn history_snapshot_survives_insert_between_count_and_rows(pool: PgPool) {
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            history_snapshot_with_mutation(pool, false),
        )
        .await
        .expect("coordinated history insert completes");
    }

    #[sqlx::test]
    async fn history_snapshot_survives_clear_between_count_and_rows(pool: PgPool) {
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            history_snapshot_with_mutation(pool, true),
        )
        .await
        .expect("coordinated history clear completes");
    }
}
