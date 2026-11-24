// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Postgres-backed storage for saved queries.

use chrono::{DateTime, Utc};
use sqlx::postgres::PgRow;
use sqlx::{AssertSqlSafe, PgPool, Row as _};

use super::error::{PgViolation, StoreError, WindowWriteError, classify_violation};
use super::schedule::{
    LATEST_RUN_COLS, LATEST_RUN_JOINS, ReportRun, Schedule, decode_window, latest_run_from_row,
    row_to_schedule_at, schedule_cols_as,
};
use crate::report_window::check_window_compatibility;

fn validate_name(name: &str) -> Result<&str, StoreError> {
    trawl_core::saved_name::normalize(name).map_err(|_| StoreError::InvalidName {
        name: name.to_owned(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedQuery {
    pub id: i64,
    pub key_id: i64,
    pub name: String,
    pub query: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct SavedQueryDetails {
    pub saved: SavedQuery,
    pub schedule: Option<ScheduleWithStats>,
}

#[derive(Debug, Clone)]
pub struct ScheduleWithStats {
    pub schedule: Schedule,
    pub latest_run: Option<ReportRun>,
}

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

#[derive(Debug, Clone)]
pub struct SavedQueryStore {
    pool: PgPool,
}

impl SavedQueryStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

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

    pub async fn list_with_details(
        &self,
        key_id: i64,
    ) -> Result<Vec<SavedQueryDetails>, StoreError> {
        let rows = sqlx::query(AssertSqlSafe(format!(
            "SELECT sq.id, sq.key_id, sq.name, sq.query, sq.created_at, sq.updated_at,
                    {schedule_cols},
                    {LATEST_RUN_COLS}
             FROM saved_queries sq
             LEFT JOIN schedules s ON s.saved_query_id = sq.id
             {LATEST_RUN_JOINS}
             WHERE sq.key_id = $1
             ORDER BY sq.name ASC",
            schedule_cols = schedule_cols_as("s.", "s_")
        )))
        .bind(key_id)
        .fetch_all(&self.pool)
        .await?;

        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let saved = row_to_saved_query(row)?;
            let schedule = if row.try_get::<Option<i64>, _>("s_id")?.is_some() {
                let schedule = row_to_schedule_at(row, "s_")?;
                let latest_run = latest_run_from_row(row)?;
                Some(ScheduleWithStats {
                    schedule,
                    latest_run,
                })
            } else {
                None
            };
            out.push(SavedQueryDetails { saved, schedule });
        }
        Ok(out)
    }

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

    pub async fn create(
        &self,
        key_id: i64,
        name: &str,
        query: &str,
    ) -> Result<SavedQuery, StoreError> {
        let name = validate_name(name)?;

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

    /// Update an existing saved query (DSL and optionally name), refusing
    /// text that contradicts the window its schedule already carries
    /// (ADR-0018 rulings 7 and 12).
    ///
    /// LOCK ORDER: `saved_queries` FOR UPDATE, then a plain read of the
    /// schedule. The read takes no lock on purpose. Every writer of this
    /// pair — this method and
    /// [`super::ScheduleStore::set_schedule_checked`] — locks the saved
    /// query first, so a window write is either already committed (and this
    /// read sees it) or is waiting for this transaction to end.
    pub async fn update_checked(
        &self,
        id: i64,
        key_id: i64,
        query: &str,
        name: Option<&str>,
    ) -> Result<SavedQuery, WindowWriteError> {
        let name = name.map(validate_name).transpose()?;

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
            }
            .into());
        }

        let schedule =
            sqlx::query("SELECT window_kind, window_secs FROM schedules WHERE saved_query_id = $1")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        let window = match schedule.as_ref().map(|row| decode_window(row, "")) {
            Some(decoded) => decoded?,
            None => None,
        };

        if let Err(e) = check_window_compatibility(window, query).await {
            tx.rollback().await?;
            return Err(e.into());
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
        .fetch_optional(&mut *tx)
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

        tx.commit().await?;

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
    /// LOCK ORDER: `saved_queries` -> `schedules` -> `report_runs`
    /// ([`super::schedule`]'s module docs). The middle level is not
    /// decoration: the cascade to `schedules` would otherwise reach for it
    /// while holding the run rows. A `since_last` [`super::ScheduleStore::finish_run`]
    /// goes `schedules` then `report_runs`, and the pair deadlocks.
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

        let _schedules: Vec<i64> =
            sqlx::query_scalar("SELECT id FROM schedules WHERE saved_query_id = $1 FOR UPDATE")
                .bind(id)
                .fetch_all(&mut *tx)
                .await?;

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
