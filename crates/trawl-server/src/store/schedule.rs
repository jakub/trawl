// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Postgres-backed storage for scheduled queries and report runs.
//!
//! Concurrency is redesigned for postgres rather than ported from sqlite
//! (whose stores were atomic by accident of a process-wide mutex):
//!
//! - the no-concurrent-run guard is the partial unique index
//!   `report_runs_one_running`; [`ScheduleStore::start_run`] INSERTs directly
//!   and maps the named 23505 to `Ok(None)`;
//! - the manual-trigger path's `max_runs` check joins the run claim in one
//!   transaction ([`ScheduleStore::claim_run`], `FOR UPDATE` on the schedule
//!   row);
//! - deletions lock the parent row then every `report_runs` row (`FOR
//!   UPDATE`, no `result_path` filter) before collecting parquet paths, so a
//!   concurrent `finish_run` either lands its path before the lock or blocks
//!   until the cascade removes its row — never orphaning the file;
//! - [`ScheduleStore::finish_run`] reports whether it updated a row so a run
//!   cascade-deleted mid-flight can have its freshly-written file removed.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row as _};

use super::error::{PgViolation, StoreError, classify_violation};
use super::history::{bind_u64, bind_usize};
use super::saved::{SavedQuery, row_to_saved_query_at};
use super::status::{RunStatus, decode_status};

const MIN_INTERVAL_SECS: u64 = 60;

/// A schedule attached to a saved query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    pub id: i64,
    pub saved_query_id: i64,
    pub key_id: i64,
    pub interval_secs: u64,
    pub max_runs: Option<u64>,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A single report run (legacy result blob fetched separately via
/// [`ScheduleStore::get_run_result`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportRun {
    pub id: i64,
    pub schedule_id: i64,
    pub saved_query_id: i64,
    pub query: String,
    pub status: RunStatus,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<u64>,
    pub row_count: Option<usize>,
    pub error_message: Option<String>,
    /// Filesystem path to the parquet result file (relative to data dir).
    pub result_path: Option<String>,
}

/// Outcome of a transactional run claim ([`ScheduleStore::claim_run`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunClaim {
    /// A run row was created; execute it.
    Started(i64),
    /// A run is already in progress for this schedule.
    AlreadyRunning,
    /// The schedule has reached its `max_runs` cap.
    MaxRunsReached,
}

/// Parse a duration string (same syntax as the DSL `last:` filter) into seconds.
///
/// Supported units: `s`, `m`, `h`, `d`, `w`. Minimum interval is 60 seconds.
pub fn parse_interval(s: &str) -> Result<u64, StoreError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(StoreError::InvalidInterval {
            input: s.to_string(),
        });
    }

    let (digits, unit) = s.split_at(s.len() - 1);
    let value: u64 = digits.parse().map_err(|_| StoreError::InvalidInterval {
        input: s.to_string(),
    })?;

    let secs = match unit {
        "s" => value,
        "m" => value * 60,
        "h" => value * 3600,
        "d" => value * 86400,
        "w" => value * 604_800,
        _ => {
            return Err(StoreError::InvalidInterval {
                input: s.to_string(),
            });
        }
    };

    if secs < MIN_INTERVAL_SECS {
        return Err(StoreError::IntervalTooShort { secs });
    }

    Ok(secs)
}

/// Format seconds into a human-readable duration string (e.g. "5m", "1h").
pub fn format_interval(secs: u64) -> String {
    if secs.is_multiple_of(604_800) {
        format!("{}w", secs / 604_800)
    } else if secs.is_multiple_of(86400) {
        format!("{}d", secs / 86400)
    } else if secs.is_multiple_of(3600) {
        format!("{}h", secs / 3600)
    } else if secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

/// Decode a [`Schedule`] from columns named `{prefix}id`, `{prefix}key_id`, …
pub(crate) fn row_to_schedule_at(row: &PgRow, prefix: &str) -> Result<Schedule, sqlx::Error> {
    let col = |name: &str| format!("{prefix}{name}");
    Ok(Schedule {
        id: row.try_get(col("id").as_str())?,
        saved_query_id: row.try_get(col("saved_query_id").as_str())?,
        key_id: row.try_get(col("key_id").as_str())?,
        interval_secs: u64::try_from(row.try_get::<i64, _>(col("interval_secs").as_str())?)
            .unwrap_or_default(),
        max_runs: row
            .try_get::<Option<i64>, _>(col("max_runs").as_str())?
            .map(|v| u64::try_from(v).unwrap_or_default()),
        enabled: row.try_get(col("enabled").as_str())?,
        created_at: row.try_get(col("created_at").as_str())?,
        updated_at: row.try_get(col("updated_at").as_str())?,
    })
}

/// Decode a [`ReportRun`] from columns named `{prefix}id`, `{prefix}status`, …
pub(crate) fn row_to_report_run_at(row: &PgRow, prefix: &str) -> Result<ReportRun, sqlx::Error> {
    let col = |name: &str| format!("{prefix}{name}");
    Ok(ReportRun {
        id: row.try_get(col("id").as_str())?,
        schedule_id: row.try_get(col("schedule_id").as_str())?,
        saved_query_id: row.try_get(col("saved_query_id").as_str())?,
        query: row.try_get(col("query").as_str())?,
        status: decode_status(row, col("status").as_str())?,
        started_at: row.try_get(col("started_at").as_str())?,
        finished_at: row.try_get(col("finished_at").as_str())?,
        duration_ms: row
            .try_get::<Option<i64>, _>(col("duration_ms").as_str())?
            .map(|v| u64::try_from(v).unwrap_or_default()),
        row_count: row
            .try_get::<Option<i64>, _>(col("row_count").as_str())?
            .map(|v| usize::try_from(v).unwrap_or_default()),
        error_message: row.try_get(col("error_message").as_str())?,
        result_path: row.try_get(col("result_path").as_str())?,
    })
}

fn row_to_schedule(row: &PgRow) -> Result<Schedule, sqlx::Error> {
    row_to_schedule_at(row, "")
}

fn row_to_report_run(row: &PgRow) -> Result<ReportRun, sqlx::Error> {
    row_to_report_run_at(row, "")
}

/// SELECT fragment exposing the latest report run (aliased `r_*`) and the total
/// run count (`run_count`) resolved by [`LATEST_RUN_JOINS`]. Decode the pair
/// with [`latest_run_and_count_from_row`]. Requires a schedule aliased `s` and
/// the lateral joins `lr`/`rc`.
pub(crate) const LATEST_RUN_COLS: &str = "lr.id             AS r_id,
     lr.schedule_id    AS r_schedule_id,
     lr.saved_query_id AS r_saved_query_id,
     lr.query          AS r_query,
     lr.status         AS r_status,
     lr.started_at     AS r_started_at,
     lr.finished_at    AS r_finished_at,
     lr.duration_ms    AS r_duration_ms,
     lr.row_count      AS r_row_count,
     lr.error_message  AS r_error_message,
     lr.result_path    AS r_result_path,
     rc.run_count      AS run_count";

/// LEFT JOIN LATERAL fragment resolving the single latest run (`lr`, tie-broken
/// by `started_at DESC, id DESC`) and the total run count (`rc`) for the
/// schedule aliased `s`. Pairs with [`LATEST_RUN_COLS`].
pub(crate) const LATEST_RUN_JOINS: &str = "LEFT JOIN LATERAL (
         SELECT * FROM report_runs r
         WHERE r.schedule_id = s.id
         ORDER BY r.started_at DESC, r.id DESC
         LIMIT 1
     ) lr ON TRUE
     LEFT JOIN LATERAL (
         SELECT COUNT(*) AS run_count FROM report_runs r2 WHERE r2.schedule_id = s.id
     ) rc ON TRUE";

/// Decode the latest run (`r_` prefix, `None` when the schedule has no runs)
/// and total run count (`run_count`) columns produced by [`LATEST_RUN_COLS`].
pub(crate) fn latest_run_and_count_from_row(
    row: &PgRow,
) -> Result<(Option<ReportRun>, u64), sqlx::Error> {
    let latest_run = if row.try_get::<Option<i64>, _>("r_id")?.is_some() {
        Some(row_to_report_run_at(row, "r_")?)
    } else {
        None
    };
    let total_runs =
        u64::try_from(row.try_get::<Option<i64>, _>("run_count")?.unwrap_or(0)).unwrap_or_default();
    Ok((latest_run, total_runs))
}

const SCHEDULE_COLS: &str =
    "id, saved_query_id, key_id, interval_secs, max_runs, enabled, created_at, updated_at";

const RUN_COLS: &str = "id, schedule_id, saved_query_id, query, status, started_at, finished_at, \
     duration_ms, row_count, error_message, result_path";

/// Postgres-backed storage for schedules and report runs. Cheap to clone.
#[derive(Debug, Clone)]
pub struct ScheduleStore {
    pool: PgPool,
}

impl ScheduleStore {
    /// Wrap the shared app-state pool.
    ///
    /// The pool must point at an already-migrated trawl app-state database
    /// (production goes through `StorageState::connect`, which migrates;
    /// tests use `#[sqlx::test]`-migrated pools directly).
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a schedule for a saved query.
    pub async fn create_schedule(
        &self,
        saved_query_id: i64,
        key_id: i64,
        interval_secs: u64,
        max_runs: Option<u64>,
    ) -> Result<Schedule, StoreError> {
        let row = sqlx::query(&format!(
            "INSERT INTO schedules
                 (saved_query_id, key_id, interval_secs, max_runs, enabled, created_at, updated_at)
             VALUES ($1, $2, $3, $4, TRUE, now(), now())
             RETURNING {SCHEDULE_COLS}"
        ))
        .bind(saved_query_id)
        .bind(key_id)
        .bind(bind_u64(interval_secs))
        .bind(max_runs.map(bind_u64))
        .fetch_one(&self.pool)
        .await
        .map_err(|e| match classify_violation(&e) {
            Some(PgViolation::ScheduleTaken) => StoreError::ScheduleExists { saved_query_id },
            Some(PgViolation::ForeignKey) => StoreError::NotFound {
                id: saved_query_id,
                resource: "saved query",
            },
            _ => StoreError::from(e),
        })?;

        let schedule = row_to_schedule(&row)?;

        tracing::info!(
            event_type = "schedule_created",
            schedule_id = schedule.id,
            saved_query_id,
            key_id,
            interval_secs,
            "Schedule created"
        );

        Ok(schedule)
    }

    /// Update an existing schedule. Returns `NotFound` if not owned by `key_id`.
    pub async fn update_schedule(
        &self,
        id: i64,
        key_id: i64,
        interval_secs: u64,
        max_runs: Option<u64>,
        enabled: bool,
    ) -> Result<Schedule, StoreError> {
        let row = sqlx::query(&format!(
            "UPDATE schedules
             SET interval_secs = $1, max_runs = $2, enabled = $3, updated_at = now()
             WHERE id = $4 AND key_id = $5
             RETURNING {SCHEDULE_COLS}"
        ))
        .bind(bind_u64(interval_secs))
        .bind(max_runs.map(bind_u64))
        .bind(enabled)
        .bind(id)
        .bind(key_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(StoreError::NotFound {
            id,
            resource: "schedule",
        })?;

        tracing::info!(
            event_type = "schedule_updated",
            schedule_id = id,
            key_id,
            interval_secs,
            enabled,
            "Schedule updated"
        );

        Ok(row_to_schedule(&row)?)
    }

    /// Delete a schedule by its saved query id, collecting the parquet paths
    /// of its runs in the SAME transaction (cascade wipes the rows).
    ///
    /// Lock order matches [`Self::claim_run`]: the schedule row first (`FOR
    /// UPDATE`, which also blocks a concurrent run INSERT via its FK `FOR KEY
    /// SHARE`), then every one of its `report_runs`. Locking all run rows — not
    /// just those with a non-null `result_path` — forces a concurrent
    /// `finish_run` to either commit its path before us (collected here) or
    /// block until our cascade deletes its row (it then updates zero rows and
    /// the caller unlinks the file). Filtering on `result_path IS NOT NULL`
    /// would skip still-running rows and reopen the orphan-file race.
    ///
    /// Returns the relative parquet paths for the caller to unlink, or
    /// `NotFound` if no schedule is owned by `key_id` for this saved query.
    pub async fn delete_schedule(
        &self,
        saved_query_id: i64,
        key_id: i64,
    ) -> Result<Vec<String>, StoreError> {
        let mut tx = self.pool.begin().await?;

        let schedule_id: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM schedules WHERE saved_query_id = $1 AND key_id = $2 FOR UPDATE",
        )
        .bind(saved_query_id)
        .bind(key_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(schedule_id) = schedule_id else {
            tx.rollback().await?;
            return Err(StoreError::NotFound {
                id: saved_query_id,
                resource: "schedule",
            });
        };

        let paths: Vec<String> = sqlx::query_scalar::<_, Option<String>>(
            "SELECT result_path FROM report_runs WHERE schedule_id = $1 FOR UPDATE",
        )
        .bind(schedule_id)
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .flatten()
        .collect();

        sqlx::query("DELETE FROM schedules WHERE id = $1")
            .bind(schedule_id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;

        tracing::info!(
            event_type = "schedule_deleted",
            saved_query_id,
            key_id,
            "Schedule deleted"
        );

        Ok(paths)
    }

    /// Get the schedule for a given saved query, if owned by `key_id`.
    pub async fn get_schedule_for_saved_query(
        &self,
        saved_query_id: i64,
        key_id: i64,
    ) -> Result<Option<Schedule>, StoreError> {
        let row = sqlx::query(&format!(
            "SELECT {SCHEDULE_COLS} FROM schedules WHERE saved_query_id = $1 AND key_id = $2"
        ))
        .bind(saved_query_id)
        .bind(key_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(row_to_schedule).transpose()?)
    }

    /// Get a schedule with its latest run and total run count in one
    /// statement (feeds `ScheduleResponse` without per-item lookups).
    pub async fn get_schedule_with_stats(
        &self,
        saved_query_id: i64,
        key_id: i64,
    ) -> Result<Option<(Schedule, Option<ReportRun>, u64)>, StoreError> {
        let row = sqlx::query(&format!(
            "SELECT s.id, s.saved_query_id, s.key_id, s.interval_secs, s.max_runs,
                    s.enabled, s.created_at, s.updated_at,
                    {LATEST_RUN_COLS}
             FROM schedules s
             {LATEST_RUN_JOINS}
             WHERE s.saved_query_id = $1 AND s.key_id = $2",
        ))
        .bind(saved_query_id)
        .bind(key_id)
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else { return Ok(None) };
        let schedule = row_to_schedule(&row)?;
        let (latest_run, total_runs) = latest_run_and_count_from_row(&row)?;
        Ok(Some((schedule, latest_run, total_runs)))
    }

    /// List all enabled schedules with their associated saved queries.
    /// Used by the scheduler — no ownership check (internal/cross-user).
    pub async fn list_enabled_schedules(&self) -> Result<Vec<(Schedule, SavedQuery)>, StoreError> {
        let rows = sqlx::query(
            "SELECT s.id, s.saved_query_id, s.key_id, s.interval_secs, s.max_runs,
                    s.enabled, s.created_at, s.updated_at,
                    sq.id         AS sq_id,
                    sq.key_id     AS sq_key_id,
                    sq.name       AS sq_name,
                    sq.query      AS sq_query,
                    sq.created_at AS sq_created_at,
                    sq.updated_at AS sq_updated_at
             FROM schedules s
             JOIN saved_queries sq ON sq.id = s.saved_query_id
             WHERE s.enabled = TRUE
             ORDER BY s.id",
        )
        .fetch_all(&self.pool)
        .await?;

        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            out.push((row_to_schedule(row)?, row_to_saved_query_at(row, "sq_")?));
        }
        Ok(out)
    }

    /// Count enabled schedules (dashboard/monitor).
    pub async fn count_enabled_schedules(&self) -> Result<u64, StoreError> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM schedules WHERE enabled = TRUE")
            .fetch_one(&self.pool)
            .await?;
        Ok(u64::try_from(count).unwrap_or_default())
    }

    /// Start a new run. Returns `Ok(None)` if a run with status `running`
    /// already exists for this schedule — the partial unique index
    /// `report_runs_one_running` is the guard, mapped from its named 23505.
    pub async fn start_run(
        &self,
        schedule_id: i64,
        saved_query_id: i64,
        query: &str,
    ) -> Result<Option<i64>, StoreError> {
        let result = sqlx::query_scalar::<_, i64>(
            "INSERT INTO report_runs (schedule_id, saved_query_id, query, status, started_at)
             VALUES ($1, $2, $3, 'running', now())
             RETURNING id",
        )
        .bind(schedule_id)
        .bind(saved_query_id)
        .bind(query)
        .fetch_one(&self.pool)
        .await;

        match result {
            Ok(id) => {
                tracing::info!(
                    event_type = "report_run_started",
                    run_id = id,
                    schedule_id,
                    saved_query_id,
                    "Report run started"
                );
                Ok(Some(id))
            }
            Err(e) => match classify_violation(&e) {
                Some(PgViolation::RunAlreadyRunning) => Ok(None),
                Some(PgViolation::ForeignKey) => Err(StoreError::NotFound {
                    id: schedule_id,
                    resource: "schedule",
                }),
                _ => Err(e.into()),
            },
        }
    }

    /// Claim a run transactionally: lock the schedule row, enforce
    /// `max_runs`, and insert the running row — all in one transaction so
    /// concurrent manual triggers can never exceed the cap.
    pub async fn claim_run(
        &self,
        schedule_id: i64,
        saved_query_id: i64,
        query: &str,
        max_runs: Option<u64>,
    ) -> Result<RunClaim, StoreError> {
        let mut tx = self.pool.begin().await?;

        // FOR UPDATE serializes concurrent claims on this schedule; a claim
        // that lost the race observes the winner's committed run count.
        let locked: Option<i64> =
            sqlx::query_scalar("SELECT id FROM schedules WHERE id = $1 FOR UPDATE")
                .bind(schedule_id)
                .fetch_optional(&mut *tx)
                .await?;
        if locked.is_none() {
            tx.rollback().await?;
            return Err(StoreError::NotFound {
                id: schedule_id,
                resource: "schedule",
            });
        }

        if let Some(max) = max_runs {
            let count: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM report_runs WHERE schedule_id = $1")
                    .bind(schedule_id)
                    .fetch_one(&mut *tx)
                    .await?;
            if u64::try_from(count).unwrap_or_default() >= max {
                tx.rollback().await?;
                return Ok(RunClaim::MaxRunsReached);
            }
        }

        let inserted = sqlx::query_scalar::<_, i64>(
            "INSERT INTO report_runs (schedule_id, saved_query_id, query, status, started_at)
             VALUES ($1, $2, $3, 'running', now())
             RETURNING id",
        )
        .bind(schedule_id)
        .bind(saved_query_id)
        .bind(query)
        .fetch_one(&mut *tx)
        .await;

        match inserted {
            Ok(id) => {
                tx.commit().await?;
                tracing::info!(
                    event_type = "report_run_started",
                    run_id = id,
                    schedule_id,
                    saved_query_id,
                    "Report run started"
                );
                Ok(RunClaim::Started(id))
            }
            Err(e) => {
                tx.rollback().await?;
                match classify_violation(&e) {
                    Some(PgViolation::RunAlreadyRunning) => Ok(RunClaim::AlreadyRunning),
                    _ => Err(e.into()),
                }
            }
        }
    }

    /// Finish a run with status, timing, and optional result path or blob.
    ///
    /// Returns `Ok(false)` when zero rows were updated — the run was
    /// cascade-deleted mid-flight (its saved query or schedule is gone), and
    /// the caller must remove any result file it just wrote.
    #[allow(clippy::too_many_arguments)]
    pub async fn finish_run(
        &self,
        run_id: i64,
        status: RunStatus,
        duration_ms: u64,
        row_count: Option<usize>,
        error_message: Option<&str>,
        result_data: Option<&[u8]>,
        result_path: Option<&str>,
    ) -> Result<bool, StoreError> {
        let updated = sqlx::query(
            "UPDATE report_runs
             SET status = $1, finished_at = now(), duration_ms = $2, row_count = $3,
                 error_message = $4, result_data = $5, result_path = $6
             WHERE id = $7",
        )
        .bind(status.as_str())
        .bind(bind_u64(duration_ms))
        .bind(row_count.map(bind_usize))
        .bind(error_message)
        .bind(result_data)
        .bind(result_path)
        .bind(run_id)
        .execute(&self.pool)
        .await
        .map_err(|e| match classify_violation(&e) {
            // Backstop: unreachable via the typed API, kept so a future raw
            // path still maps the CHECK to Validation rather than 503.
            Some(PgViolation::Check) => {
                StoreError::Validation(format!("invalid run status {:?}", status.as_str()))
            }
            _ => StoreError::from(e),
        })?
        .rows_affected();

        tracing::info!(
            event_type = "report_run_finished",
            run_id,
            status = status.as_str(),
            duration_ms,
            orphaned = (updated == 0),
            "Report run finished"
        );

        Ok(updated > 0)
    }

    /// Flip a run to `error`, but only while it is still `running`.
    ///
    /// This is the scheduler's ambiguous-commit recovery path: a prior
    /// `finish_run("success", …)` returned `Err`, which for a single autocommit
    /// UPDATE can mean the COMMIT landed server-side while the client's ack was
    /// lost. An unconditional overwrite would destroy that committed success —
    /// clearing `row_count`/`result_data`/`result_path` and permanently
    /// orphaning the parquet file the row pointed at. Guarding on
    /// `status = 'running'` makes completion a state transition: the flip lands
    /// only if the success did NOT commit.
    ///
    /// Returns `Ok(true)` when a running row was flipped (the earlier success
    /// never committed, so any parquet the caller wrote is now orphaned and
    /// should be removed), `Ok(false)` when no running row matched — either the
    /// ambiguous success actually committed (its result must be preserved) or
    /// the run was cascade-deleted.
    pub async fn fail_run_if_running(
        &self,
        run_id: i64,
        duration_ms: u64,
        error_message: &str,
    ) -> Result<bool, StoreError> {
        let updated = sqlx::query(
            "UPDATE report_runs
             SET status = 'error', finished_at = now(), duration_ms = $1,
                 row_count = NULL, error_message = $2, result_data = NULL,
                 result_path = NULL
             WHERE id = $3 AND status = 'running'",
        )
        .bind(bind_u64(duration_ms))
        .bind(error_message)
        .bind(run_id)
        .execute(&self.pool)
        .await?
        .rows_affected();

        tracing::info!(
            event_type = "report_run_fail_if_running",
            run_id,
            flipped = (updated > 0),
            "Guarded run failure applied"
        );

        Ok(updated > 0)
    }

    /// List runs for a saved query, paginated. Excludes result blobs.
    pub async fn list_runs(
        &self,
        saved_query_id: i64,
        key_id: i64,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<ReportRun>, StoreError> {
        let rows = sqlx::query(&format!(
            "SELECT {cols}
             FROM report_runs r
             JOIN schedules s ON s.id = r.schedule_id
             WHERE r.saved_query_id = $1 AND s.key_id = $2
             ORDER BY r.started_at DESC, r.id DESC
             LIMIT $3 OFFSET $4",
            cols = run_cols("r.")
        ))
        .bind(saved_query_id)
        .bind(key_id)
        .bind(bind_usize(limit))
        .bind(bind_usize(offset))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(row_to_report_run)
            .collect::<Result<Vec<_>, _>>()?)
    }

    /// Get a single run by id (no result blob). Checks ownership via schedule.
    pub async fn get_run(&self, run_id: i64, key_id: i64) -> Result<Option<ReportRun>, StoreError> {
        let row = sqlx::query(&format!(
            "SELECT {cols}
             FROM report_runs r
             JOIN schedules s ON s.id = r.schedule_id
             WHERE r.id = $1 AND s.key_id = $2",
            cols = run_cols("r.")
        ))
        .bind(run_id)
        .bind(key_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(row_to_report_run).transpose()?)
    }

    /// Get the legacy compressed result blob for a run. Checks ownership.
    pub async fn get_run_result(
        &self,
        run_id: i64,
        key_id: i64,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let row: Option<Option<Vec<u8>>> = sqlx::query_scalar(
            "SELECT r.result_data
             FROM report_runs r
             JOIN schedules s ON s.id = r.schedule_id
             WHERE r.id = $1 AND s.key_id = $2",
        )
        .bind(run_id)
        .bind(key_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.flatten())
    }

    /// Count runs for a schedule.
    pub async fn count_runs(&self, schedule_id: i64) -> Result<u64, StoreError> {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM report_runs WHERE schedule_id = $1")
                .bind(schedule_id)
                .fetch_one(&self.pool)
                .await?;
        Ok(u64::try_from(count).unwrap_or_default())
    }

    /// Get the most recent run for a schedule.
    pub async fn latest_run(&self, schedule_id: i64) -> Result<Option<ReportRun>, StoreError> {
        let row = sqlx::query(&format!(
            "SELECT {RUN_COLS} FROM report_runs
             WHERE schedule_id = $1
             ORDER BY started_at DESC, id DESC
             LIMIT 1"
        ))
        .bind(schedule_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(row_to_report_run).transpose()?)
    }

    /// Get the most recent successful run for a saved query (`run=latest`).
    pub async fn latest_successful_run(
        &self,
        saved_query_id: i64,
    ) -> Result<Option<ReportRun>, StoreError> {
        let row = sqlx::query(&format!(
            "SELECT {RUN_COLS} FROM report_runs
             WHERE saved_query_id = $1 AND status = 'success' AND result_path IS NOT NULL
             ORDER BY started_at DESC, id DESC
             LIMIT 1"
        ))
        .bind(saved_query_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(row_to_report_run).transpose()?)
    }

    /// List all successful runs with parquet results for a saved query
    /// (`run=all`), oldest first.
    pub async fn list_successful_runs(
        &self,
        saved_query_id: i64,
    ) -> Result<Vec<ReportRun>, StoreError> {
        let rows = sqlx::query(&format!(
            "SELECT {RUN_COLS} FROM report_runs
             WHERE saved_query_id = $1 AND status = 'success' AND result_path IS NOT NULL
             ORDER BY started_at ASC, id ASC"
        ))
        .bind(saved_query_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(row_to_report_run)
            .collect::<Result<Vec<_>, _>>()?)
    }

    /// Mark any runs with status `running` as `error` (crash recovery on
    /// startup — safe because the advisory lock guarantees no live sibling).
    pub async fn cleanup_stale_runs(&self) -> Result<usize, StoreError> {
        let updated = sqlx::query(
            "UPDATE report_runs
             SET status = 'error', finished_at = now(),
                 error_message = 'interrupted by server restart'
             WHERE status = 'running'",
        )
        .execute(&self.pool)
        .await?
        .rows_affected();

        if updated > 0 {
            tracing::warn!(
                event_type = "stale_runs_cleaned",
                count = updated,
                "Marked stale running report runs as error"
            );
        }

        Ok(usize::try_from(updated).unwrap_or_default())
    }

    /// Delete old runs for retention. Keeps at most `max_per_schedule` runs
    /// per schedule and deletes runs older than `max_age_days` — both
    /// deletions and the path collection happen in ONE transaction.
    ///
    /// Returns `(count_deleted, result_paths)`; the caller unlinks the
    /// parquet files.
    pub async fn delete_old_runs(
        &self,
        max_age_days: u64,
        max_per_schedule: u64,
    ) -> Result<(usize, Vec<String>), StoreError> {
        let cutoff =
            Utc::now() - chrono::Duration::days(i64::try_from(max_age_days).unwrap_or(365));

        let mut tx = self.pool.begin().await?;

        let aged: Vec<Option<String>> = sqlx::query_scalar(
            "DELETE FROM report_runs WHERE started_at < $1 RETURNING result_path",
        )
        .bind(cutoff)
        .fetch_all(&mut *tx)
        .await?;

        let excess: Vec<Option<String>> = sqlx::query_scalar(
            "DELETE FROM report_runs
             WHERE id NOT IN (
                 SELECT id FROM (
                     SELECT id, ROW_NUMBER() OVER (
                         PARTITION BY schedule_id ORDER BY started_at DESC, id DESC
                     ) AS rn
                     FROM report_runs
                 ) ranked
                 WHERE rn <= $1
             )
             RETURNING result_path",
        )
        .bind(bind_u64(max_per_schedule))
        .fetch_all(&mut *tx)
        .await?;

        tx.commit().await?;

        let total = aged.len() + excess.len();
        let paths: HashSet<String> = aged.into_iter().chain(excess).flatten().collect();

        if total > 0 {
            tracing::info!(
                event_type = "old_runs_deleted",
                total,
                parquet_paths = paths.len(),
                "Deleted old report runs"
            );
        }

        Ok((total, paths.into_iter().collect()))
    }

    /// Count total runs for a saved query (for pagination).
    pub async fn count_runs_for_saved_query(
        &self,
        saved_query_id: i64,
        key_id: i64,
    ) -> Result<usize, StoreError> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)
             FROM report_runs r
             JOIN schedules s ON s.id = r.schedule_id
             WHERE r.saved_query_id = $1 AND s.key_id = $2",
        )
        .bind(saved_query_id)
        .bind(key_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(usize::try_from(count).unwrap_or_default())
    }

    /// List runs across ALL saved queries for a user, paginated.
    /// Returns `(ReportRun, net_name)` pairs, most recent first.
    pub async fn list_all_runs(
        &self,
        key_id: i64,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<(ReportRun, String)>, StoreError> {
        let rows = sqlx::query(&format!(
            "SELECT {cols}, sq.name AS sq_name
             FROM report_runs r
             JOIN schedules s ON s.id = r.schedule_id
             JOIN saved_queries sq ON sq.id = r.saved_query_id
             WHERE s.key_id = $1
             ORDER BY r.started_at DESC, r.id DESC
             LIMIT $2 OFFSET $3",
            cols = run_cols("r.")
        ))
        .bind(key_id)
        .bind(bind_usize(limit))
        .bind(bind_usize(offset))
        .fetch_all(&self.pool)
        .await?;

        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let run = row_to_report_run(row)?;
            let net_name: String = row.try_get("sq_name")?;
            out.push((run, net_name));
        }
        Ok(out)
    }

    /// Count all runs across all saved queries for a user.
    pub async fn count_all_runs(&self, key_id: i64) -> Result<usize, StoreError> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)
             FROM report_runs r
             JOIN schedules s ON s.id = r.schedule_id
             WHERE s.key_id = $1",
        )
        .bind(key_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(usize::try_from(count).unwrap_or_default())
    }

    /// Aggregate stats across all runs for a user:
    /// `(total, success, error, timeout, avg_duration_ms)`.
    pub async fn runs_stats(
        &self,
        key_id: i64,
    ) -> Result<(u64, u64, u64, u64, Option<u64>), StoreError> {
        let row = sqlx::query(
            "SELECT COUNT(*)                                                        AS total,
                    COALESCE(SUM(CASE WHEN r.status = 'success' THEN 1 ELSE 0 END), 0) AS success,
                    COALESCE(SUM(CASE WHEN r.status = 'error' THEN 1 ELSE 0 END), 0)   AS error,
                    COALESCE(SUM(CASE WHEN r.status = 'timeout' THEN 1 ELSE 0 END), 0) AS timeout,
                    AVG(r.duration_ms) FILTER (WHERE r.duration_ms IS NOT NULL)::DOUBLE PRECISION
                                                                                    AS avg_ms
             FROM report_runs r
             JOIN schedules s ON s.id = r.schedule_id
             WHERE s.key_id = $1",
        )
        .bind(key_id)
        .fetch_one(&self.pool)
        .await?;

        let get_u64 = |name: &str| -> Result<u64, sqlx::Error> {
            Ok(u64::try_from(row.try_get::<i64, _>(name)?).unwrap_or_default())
        };
        let avg: Option<f64> = row.try_get("avg_ms")?;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let avg_ms = avg.map(|v| v.max(0.0) as u64);
        Ok((
            get_u64("total")?,
            get_u64("success")?,
            get_u64("error")?,
            get_u64("timeout")?,
            avg_ms,
        ))
    }
}

/// Build a prefixed run column list (e.g. `r.id, r.schedule_id, …`).
fn run_cols(prefix: &str) -> String {
    RUN_COLS
        .split(", ")
        .map(|c| format!("{prefix}{}", c.trim()))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pure-function coverage for interval parsing/formatting lives here (no
    // database); pg-backed behaviour is covered in tests/store_pg.rs.

    #[test]
    fn parse_interval_valid() {
        assert_eq!(parse_interval("60s").unwrap(), 60);
        assert_eq!(parse_interval("5m").unwrap(), 300);
        assert_eq!(parse_interval("1h").unwrap(), 3600);
        assert_eq!(parse_interval("1d").unwrap(), 86400);
        assert_eq!(parse_interval("1w").unwrap(), 604_800);
    }

    #[test]
    fn parse_interval_rejects_too_short() {
        assert!(matches!(
            parse_interval("30s"),
            Err(StoreError::IntervalTooShort { secs: 30 })
        ));
        assert!(matches!(
            parse_interval("0s"),
            Err(StoreError::IntervalTooShort { secs: 0 })
        ));
    }

    #[test]
    fn parse_interval_rejects_invalid() {
        assert!(parse_interval("").is_err());
        assert!(parse_interval("abc").is_err());
        assert!(parse_interval("5x").is_err());
    }

    #[test]
    fn format_interval_roundtrip() {
        assert_eq!(format_interval(60), "1m");
        assert_eq!(format_interval(300), "5m");
        assert_eq!(format_interval(3600), "1h");
        assert_eq!(format_interval(86400), "1d");
        assert_eq!(format_interval(604_800), "1w");
        assert_eq!(format_interval(90), "90s");
    }

    #[test]
    fn run_cols_prefixes_every_column() {
        let cols = run_cols("r.");
        assert!(cols.starts_with("r.id, r.schedule_id"));
        assert!(cols.ends_with("r.result_path"));
        assert!(!cols.contains(" ,"));
    }
}
