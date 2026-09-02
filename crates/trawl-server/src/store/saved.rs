// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Postgres-backed storage for saved queries.

use chrono::{DateTime, Utc};
use sqlx::postgres::PgRow;
use sqlx::{AssertSqlSafe, PgPool, Row as _};

use super::error::{PgViolation, StoreError, classify_violation};
use super::schedule::{
    LATEST_RUN_COLS, LATEST_RUN_JOINS, ReportRun, Schedule, latest_run_and_count_from_row,
    row_to_schedule_at,
};

/// Validate that a saved query name matches `[a-zA-Z0-9_-]+`.
fn validate_name(name: &str) -> Result<(), StoreError> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(StoreError::InvalidName {
            name: name.to_owned(),
        });
    }
    Ok(())
}

/// A single saved query entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedQuery {
    pub id: i64,
    pub key_id: i64,
    pub name: String,
    pub query: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A saved query enriched with its schedule and run stats — the bulk-join
/// shape backing `GET /api/v1/saved` (one query, independent of item count).
#[derive(Debug, Clone)]
pub struct SavedQueryDetails {
    pub saved: SavedQuery,
    pub schedule: Option<ScheduleWithStats>,
}

/// A schedule enriched with its latest run and total run count.
#[derive(Debug, Clone)]
pub struct ScheduleWithStats {
    pub schedule: Schedule,
    pub latest_run: Option<ReportRun>,
    pub total_runs: u64,
}

/// Decode a [`SavedQuery`] from columns named `{prefix}id`, `{prefix}name`, …
pub(crate) fn row_to_saved_query_at(row: &PgRow, prefix: &str) -> Result<SavedQuery, sqlx::Error> {
    let col = |name: &str| format!("{prefix}{name}");
    Ok(SavedQuery {
        id: row.try_get(col("id").as_str())?,
        key_id: row.try_get(col("key_id").as_str())?,
        name: row.try_get(col("name").as_str())?,
        query: row.try_get(col("query").as_str())?,
        created_at: row.try_get(col("created_at").as_str())?,
        updated_at: row.try_get(col("updated_at").as_str())?,
    })
}

fn row_to_saved_query(row: &PgRow) -> Result<SavedQuery, sqlx::Error> {
    row_to_saved_query_at(row, "")
}

const SAVED_COLS: &str = "id, key_id, name, query, created_at, updated_at";

/// Postgres-backed storage for saved queries. Cheap to clone (shared pool).
#[derive(Debug, Clone)]
pub struct SavedQueryStore {
    pool: PgPool,
}

impl SavedQueryStore {
    /// Wrap the shared app-state pool.
    ///
    /// The pool must point at an already-migrated trawl app-state database
    /// (production goes through `StorageState::connect`, which migrates;
    /// tests use `#[sqlx::test]`-migrated pools directly).
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// List all saved queries for a user, sorted by name.
    pub async fn list(&self, key_id: i64) -> Result<Vec<SavedQuery>, StoreError> {
        let rows = sqlx::query(AssertSqlSafe(format!(
            "SELECT {SAVED_COLS} FROM saved_queries WHERE key_id = $1 ORDER BY name ASC"
        )))
        .bind(key_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(row_to_saved_query)
            .collect::<Result<Vec<_>, _>>()?)
    }

    /// List saved queries with schedule + latest-run + run-count details in
    /// a single statement: the lateral joins keep it one round trip whatever
    /// the item count, instead of three follow-up lookups per saved query.
    pub async fn list_with_details(
        &self,
        key_id: i64,
    ) -> Result<Vec<SavedQueryDetails>, StoreError> {
        let rows = sqlx::query(AssertSqlSafe(format!(
            "SELECT sq.id, sq.key_id, sq.name, sq.query, sq.created_at, sq.updated_at,
                    s.id             AS s_id,
                    s.saved_query_id AS s_saved_query_id,
                    s.key_id         AS s_key_id,
                    s.interval_secs  AS s_interval_secs,
                    s.max_runs       AS s_max_runs,
                    s.enabled        AS s_enabled,
                    s.created_at     AS s_created_at,
                    s.updated_at     AS s_updated_at,
                    {LATEST_RUN_COLS}
             FROM saved_queries sq
             LEFT JOIN schedules s ON s.saved_query_id = sq.id
             {LATEST_RUN_JOINS}
             WHERE sq.key_id = $1
             ORDER BY sq.name ASC",
        )))
        .bind(key_id)
        .fetch_all(&self.pool)
        .await?;

        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let saved = row_to_saved_query(row)?;
            let schedule = if row.try_get::<Option<i64>, _>("s_id")?.is_some() {
                let schedule = row_to_schedule_at(row, "s_")?;
                let (latest_run, total_runs) = latest_run_and_count_from_row(row)?;
                Some(ScheduleWithStats {
                    schedule,
                    latest_run,
                    total_runs,
                })
            } else {
                None
            };
            out.push(SavedQueryDetails { saved, schedule });
        }
        Ok(out)
    }

    /// Look up a saved query by id for a specific user.
    pub async fn get(&self, id: i64, key_id: i64) -> Result<Option<SavedQuery>, StoreError> {
        let row = sqlx::query(AssertSqlSafe(format!(
            "SELECT {SAVED_COLS} FROM saved_queries WHERE id = $1 AND key_id = $2"
        )))
        .bind(id)
        .bind(key_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(row_to_saved_query).transpose()?)
    }

    /// Look up a saved query by name for a specific user.
    pub async fn get_by_name(
        &self,
        key_id: i64,
        name: &str,
    ) -> Result<Option<SavedQuery>, StoreError> {
        let row = sqlx::query(AssertSqlSafe(format!(
            "SELECT {SAVED_COLS} FROM saved_queries WHERE key_id = $1 AND name = $2"
        )))
        .bind(key_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(row_to_saved_query).transpose()?)
    }

    /// Create a new saved query.
    ///
    /// Returns `InvalidName` for names outside `[a-zA-Z0-9_-]+` and
    /// `DuplicateName` on the named unique constraint.
    pub async fn create(
        &self,
        key_id: i64,
        name: &str,
        query: &str,
    ) -> Result<SavedQuery, StoreError> {
        validate_name(name)?;

        let row = sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO saved_queries (key_id, name, query, created_at, updated_at)
             VALUES ($1, $2, $3, now(), now())
             RETURNING {SAVED_COLS}"
        )))
        .bind(key_id)
        .bind(name)
        .bind(query)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| match classify_violation(&e) {
            Some(PgViolation::SavedNameTaken) => StoreError::DuplicateName {
                name: name.to_owned(),
            },
            _ => StoreError::from(e),
        })?;

        let saved = row_to_saved_query(&row)?;

        tracing::info!(
            event_type = "saved_query_created",
            saved_id = saved.id,
            key_id,
            name,
            "Saved query created"
        );

        Ok(saved)
    }

    /// Update an existing saved query (DSL and optionally name).
    ///
    /// Returns `NotFound` if the query doesn't exist or isn't owned by the
    /// user, `InvalidName`/`DuplicateName` on name problems.
    pub async fn update(
        &self,
        id: i64,
        key_id: i64,
        query: &str,
        name: Option<&str>,
    ) -> Result<SavedQuery, StoreError> {
        if let Some(n) = name {
            validate_name(n)?;
        }

        let row = sqlx::query(AssertSqlSafe(format!(
            "UPDATE saved_queries
             SET query = $1, name = COALESCE($2, name), updated_at = now()
             WHERE id = $3 AND key_id = $4
             RETURNING {SAVED_COLS}"
        )))
        .bind(query)
        .bind(name)
        .bind(id)
        .bind(key_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| match classify_violation(&e) {
            Some(PgViolation::SavedNameTaken) => StoreError::DuplicateName {
                name: name.unwrap_or_default().to_owned(),
            },
            _ => StoreError::from(e),
        })?
        .ok_or(StoreError::NotFound {
            id,
            resource: "saved query",
        })?;

        tracing::info!(
            event_type = "saved_query_updated",
            saved_id = id,
            key_id,
            "Saved query updated"
        );

        Ok(row_to_saved_query(&row)?)
    }

    /// Delete a saved query, collecting the parquet result paths of its runs
    /// in the same transaction as the delete.
    ///
    /// Lock order (shared with [`super::ScheduleStore::claim_run`]): the parent
    /// row first, then its `report_runs`. Locking the parent `FOR UPDATE` blocks
    /// a concurrent run INSERT (which needs a `FOR KEY SHARE` on the same row via
    /// the FK), so no new run can slip in after we collect paths. Locking every
    /// run row, not just those with a non-null `result_path`, forces a
    /// concurrent `finish_run` to either commit its path before us (we collect it
    /// here) or block until our cascade deletes its row (it then updates zero rows
    /// and the caller unlinks the file it wrote). Filtering on
    /// `result_path IS NOT NULL` would skip still-running rows and reopen that
    /// race, orphaning the parquet file.
    ///
    /// Returns the relative parquet paths for the caller to unlink, or
    /// `NotFound` if the query doesn't exist or isn't owned by the user.
    pub async fn delete(&self, id: i64, key_id: i64) -> Result<Vec<String>, StoreError> {
        let mut tx = self.pool.begin().await?;

        let owned: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM saved_queries WHERE id = $1 AND key_id = $2 FOR UPDATE",
        )
        .bind(id)
        .bind(key_id)
        .fetch_optional(&mut *tx)
        .await?;
        if owned.is_none() {
            tx.rollback().await?;
            return Err(StoreError::NotFound {
                id,
                resource: "saved query",
            });
        }

        let paths: Vec<String> = sqlx::query_scalar::<_, Option<String>>(
            "SELECT result_path FROM report_runs WHERE saved_query_id = $1 FOR UPDATE",
        )
        .bind(id)
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .flatten()
        .collect();

        sqlx::query("DELETE FROM saved_queries WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;

        tracing::info!(
            event_type = "saved_query_deleted",
            saved_id = id,
            key_id,
            "Saved query deleted"
        );

        Ok(paths)
    }
}
