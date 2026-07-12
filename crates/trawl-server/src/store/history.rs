// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Postgres-backed storage for query history.

use chrono::{DateTime, Utc};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row as _};

use super::error::{StoreError, classify_violation};

/// A single query history entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub id: i64,
    pub key_id: i64,
    pub query: String,
    pub executed_at: DateTime<Utc>,
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

fn row_to_history_entry(row: &PgRow) -> Result<HistoryEntry, sqlx::Error> {
    Ok(HistoryEntry {
        id: row.try_get("id")?,
        key_id: row.try_get("key_id")?,
        query: row.try_get("query")?,
        executed_at: row.try_get("executed_at")?,
        duration_ms: u64::try_from(row.try_get::<i64, _>("duration_ms")?).unwrap_or_default(),
        row_count: usize::try_from(row.try_get::<i64, _>("row_count")?).unwrap_or_default(),
        status: row.try_get("status")?,
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
}

impl HistoryStore {
    /// Wrap the shared app-state pool.
    ///
    /// The pool must point at an already-migrated trawl app-state database
    /// (production goes through `StorageState::connect`, which migrates;
    /// tests use `#[sqlx::test]`-migrated pools directly).
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Record a query execution. Returns the new history entry id.
    pub async fn record_query(
        &self,
        key_id: i64,
        query: &str,
        duration_ms: u64,
        row_count: usize,
        status: &str,
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
        .bind(status)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| match classify_violation(&e) {
            Some(super::error::PgViolation::Check) => {
                StoreError::Validation(format!("invalid history status {status:?}"))
            }
            _ => StoreError::from(e),
        })?;

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
    /// Most recent first — `ORDER BY executed_at DESC, id DESC` (the id
    /// tie-break makes same-timestamp pages deterministic).
    pub async fn get_user_history(
        &self,
        key_id: i64,
        limit: usize,
        offset: usize,
    ) -> Result<HistoryPage, StoreError> {
        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM query_history WHERE key_id = $1")
            .bind(key_id)
            .fetch_one(&self.pool)
            .await?;

        let rows = sqlx::query(
            "SELECT id, key_id, query, executed_at, duration_ms, row_count, status
             FROM query_history
             WHERE key_id = $1
             ORDER BY executed_at DESC, id DESC
             LIMIT $2 OFFSET $3",
        )
        .bind(key_id)
        .bind(bind_usize(limit))
        .bind(bind_usize(offset))
        .fetch_all(&self.pool)
        .await?;

        let entries = rows
            .iter()
            .map(row_to_history_entry)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(HistoryPage {
            entries,
            total: usize::try_from(total).unwrap_or_default(),
        })
    }
}
