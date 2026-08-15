// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Postgres-backed repin job store (ADR-0011 slice B, issue #53).
//!
//! One repin at a time, install-wide, enforced by the `repin_jobs_one_running`
//! partial unique index — never an in-process mutex, so the guarantee holds
//! across restarts and the claim race is decided by postgres exactly as the
//! scheduler's run claim is. Dry runs are jobs too: the row is the dry-run
//! report, and the scan → refusal → execution lifecycle is one code path.

use chrono::{DateTime, Utc};
use sqlx::postgres::PgRow;
use sqlx::{AssertSqlSafe, PgPool, Row as _};
use trawl_core::schema::CanonicalType;

use super::error::{PgViolation, StoreError, classify_violation};

/// Closed job status vocabulary (mirrors the migration CHECK).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepinJobStatus {
    /// Claimed; scanning, building, or cutting over.
    Running,
    /// Terminal: dry-run report ready, or the corpus is repinned.
    Succeeded,
    /// Terminal: errored (recorded); the live corpus was never mutated in
    /// place, so it stands at the pre-repin generation.
    Failed,
    /// Terminal: nulled values without a force flag — either projected by
    /// the pre-build scan, or actually written by the finished shadow
    /// (data ingested after the scan), refused at the cutover gate.
    RefusedNeedsForce,
    /// Terminal: the cutover could not drain queries within its budget.
    Blocked,
}

impl RepinJobStatus {
    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::RefusedNeedsForce => "refused_needs_force",
            Self::Blocked => "blocked",
        }
    }

    /// Parse a stored spelling. The vocabulary is CHECK-enforced, so an
    /// unknown string is corruption, not data.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "running" => Some(Self::Running),
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            "refused_needs_force" => Some(Self::RefusedNeedsForce),
            "blocked" => Some(Self::Blocked),
            _ => None,
        }
    }
}

/// One `repin_jobs` row.
#[derive(Debug, Clone)]
pub struct RepinJob {
    /// Job id.
    pub id: i64,
    /// The repinned field (catalog key, folded).
    pub field: String,
    /// The pin at claim time (`DuckDB` spelling).
    pub from_type: String,
    /// The target pin (`DuckDB` spelling).
    pub to_type: String,
    /// Whether this job stops after the scan.
    pub dry_run: bool,
    /// Whether a lossy projection was explicitly accepted.
    pub force: bool,
    /// Job status.
    pub status: RepinJobStatus,
    /// Requesting key's display name (audit; `None` for internal callers).
    pub requested_by: Option<String>,
    /// Claim instant.
    pub started_at: DateTime<Utc>,
    /// Terminal instant.
    pub finished_at: Option<DateTime<Utc>>,
    /// Terminal error text, when any.
    pub error: Option<String>,
    /// Scan plan: affected files.
    pub files_total: i64,
    /// Scan plan: rows carrying a stored value for the field.
    pub rows_carrying: i64,
    /// Scan plan: stored values the new pin cannot read (would null).
    pub projected_nulls: i64,
    /// Scan plan: currently-shelved values `_raw` gives back under the new
    /// pin.
    pub resurrectable: i64,
    /// Scan plan: bytes across the affected files (the double-hold peak).
    pub affected_bytes: i64,
    /// Progress: affected files rewritten so far.
    pub files_done: i64,
    /// Outcome: rows written through the rewrite.
    pub rows_rewritten: i64,
    /// Outcome: stored values the rewrite nulled.
    pub rows_nulled: i64,
    /// Outcome: values resurrected from `_raw`.
    pub rows_resurrected: i64,
}

fn row_to_job(row: &PgRow) -> Result<RepinJob, sqlx::Error> {
    let status: String = row.try_get("status")?;
    let status = RepinJobStatus::parse(&status).ok_or_else(|| {
        sqlx::Error::Decode(format!("repin_jobs holds non-canonical status {status:?}").into())
    })?;
    Ok(RepinJob {
        id: row.try_get("id")?,
        field: row.try_get("field")?,
        from_type: row.try_get("from_type")?,
        to_type: row.try_get("to_type")?,
        dry_run: row.try_get("dry_run")?,
        force: row.try_get("force")?,
        status,
        requested_by: row.try_get("requested_by")?,
        started_at: row.try_get("started_at")?,
        finished_at: row.try_get("finished_at")?,
        error: row.try_get("error")?,
        files_total: row.try_get("files_total")?,
        rows_carrying: row.try_get("rows_carrying")?,
        projected_nulls: row.try_get("projected_nulls")?,
        resurrectable: row.try_get("resurrectable")?,
        affected_bytes: row.try_get("affected_bytes")?,
        files_done: row.try_get("files_done")?,
        rows_rewritten: row.try_get("rows_rewritten")?,
        rows_nulled: row.try_get("rows_nulled")?,
        rows_resurrected: row.try_get("rows_resurrected")?,
    })
}

const JOB_COLS: &str = "id, field, from_type, to_type, dry_run, force, status, requested_by, \
     started_at, finished_at, error, files_total, rows_carrying, projected_nulls, \
     resurrectable, affected_bytes, files_done, rows_rewritten, rows_nulled, rows_resurrected";

/// Postgres-backed repin job store. Cheap to clone (shared pool).
#[derive(Debug, Clone)]
pub struct RepinStore {
    pool: PgPool,
}

impl RepinStore {
    /// Wrap the shared app-state pool (must already be migrated).
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Claim THE running slot: insert a `running` row, mapping a 23505 on
    /// `repin_jobs_one_running` to [`StoreError::RepinAlreadyRunning`].
    pub async fn claim(
        &self,
        field: &str,
        from_type: CanonicalType,
        to_type: CanonicalType,
        dry_run: bool,
        force: bool,
        requested_by: Option<&str>,
    ) -> Result<i64, StoreError> {
        let result = sqlx::query_scalar::<_, i64>(
            "INSERT INTO repin_jobs (field, from_type, to_type, dry_run, force, requested_by)
             VALUES ($1, $2, $3, $4, $5, $6)
             RETURNING id",
        )
        .bind(field)
        .bind(from_type.as_catalog())
        .bind(to_type.as_catalog())
        .bind(dry_run)
        .bind(force)
        .bind(requested_by)
        .fetch_one(&self.pool)
        .await;
        result.map_err(|e| match classify_violation(&e) {
            Some(PgViolation::RepinAlreadyRunning) => StoreError::RepinAlreadyRunning,
            _ => StoreError::from(e),
        })
    }

    /// Stamp the scan plan onto the job row.
    pub async fn record_plan(
        &self,
        id: i64,
        files_total: i64,
        rows_carrying: i64,
        projected_nulls: i64,
        resurrectable: i64,
        affected_bytes: i64,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE repin_jobs
             SET files_total = $2, rows_carrying = $3, projected_nulls = $4,
                 resurrectable = $5, affected_bytes = $6
             WHERE id = $1",
        )
        .bind(id)
        .bind(files_total)
        .bind(rows_carrying)
        .bind(projected_nulls)
        .bind(resurrectable)
        .bind(affected_bytes)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Stamp rewrite progress/outcome tallies onto the job row.
    pub async fn record_progress(
        &self,
        id: i64,
        files_done: i64,
        rows_rewritten: i64,
        rows_nulled: i64,
        rows_resurrected: i64,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE repin_jobs
             SET files_done = $2, rows_rewritten = $3, rows_nulled = $4,
                 rows_resurrected = $5
             WHERE id = $1",
        )
        .bind(id)
        .bind(files_done)
        .bind(rows_rewritten)
        .bind(rows_nulled)
        .bind(rows_resurrected)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Move a job to a TERMINAL status (never `running`), stamping
    /// `finished_at` once.
    pub async fn finish(
        &self,
        id: i64,
        status: RepinJobStatus,
        error: Option<&str>,
    ) -> Result<(), StoreError> {
        debug_assert_ne!(status, RepinJobStatus::Running);
        sqlx::query(
            "UPDATE repin_jobs
             SET status = $2, error = $3, finished_at = COALESCE(finished_at, now())
             WHERE id = $1",
        )
        .bind(id)
        .bind(status.as_str())
        .bind(error)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The cutover's pin flip: `field_types` takes the new type and the job
    /// completes, in ONE transaction — a crash between the two cannot leave
    /// a flipped pin with a `running` job or vice versa.
    ///
    /// Idempotent on purpose: boot recovery replays this after a crash in
    /// the cutover or cleanup window, and a redo must neither error nor
    /// restamp `finished_at`.
    ///
    /// The field's conflict evidence is cleared in the same transaction
    /// (ADR-0011 slice C1): it indicts a pin that no longer exists, and the
    /// analyzer's gate is span-based, so evidence left behind would badge
    /// the field as degraded forever — the operator's remedy would not clear
    /// the sign that told them to apply it.
    ///
    /// The clear is gated on THIS call being the one that completed the job,
    /// which is the only part of the flip that is not naturally idempotent.
    /// A forced lossy repin records its OWN fresh evidence after
    /// `finish_cutover` returns (`repin::engine`'s `record_outcome`), so a
    /// boot replay of an already-succeeded job — the cleanup window crashed,
    /// the marker survived — would otherwise delete evidence describing the
    /// NEW pin, which nothing would ever write again.
    pub async fn finish_cutover(
        &self,
        id: i64,
        field: &str,
        to_type: CanonicalType,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "UPDATE field_types
             SET duckdb_type = $2, pinned_from = '_repin', pinned_at = now()
             WHERE field = $1 AND duckdb_type IS DISTINCT FROM $2",
        )
        .bind(field)
        .bind(to_type.as_catalog())
        .execute(&mut *tx)
        .await?;
        let completed = sqlx::query(
            "UPDATE repin_jobs
             SET status = 'succeeded', finished_at = COALESCE(finished_at, now())
             WHERE id = $1 AND status = 'running'",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if completed > 0 {
            super::CatalogStore::clear_conflict_evidence(&mut tx, field).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Read one job row.
    pub async fn get(&self, id: i64) -> Result<Option<RepinJob>, StoreError> {
        let row = sqlx::query(AssertSqlSafe(format!(
            "SELECT {JOB_COLS} FROM repin_jobs WHERE id = $1"
        )))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref()
            .map(row_to_job)
            .transpose()
            .map_err(StoreError::from)
    }

    /// The running job, if any.
    pub async fn running(&self) -> Result<Option<RepinJob>, StoreError> {
        let row = sqlx::query(AssertSqlSafe(format!(
            "SELECT {JOB_COLS} FROM repin_jobs WHERE status = 'running'"
        )))
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref()
            .map(row_to_job)
            .transpose()
            .map_err(StoreError::from)
    }

    /// The status surface's one row: the running job if any, else the
    /// newest job of any status.
    pub async fn latest(&self) -> Result<Option<RepinJob>, StoreError> {
        let row = sqlx::query(AssertSqlSafe(format!(
            "SELECT {JOB_COLS} FROM repin_jobs
             ORDER BY (status = 'running') DESC, started_at DESC, id DESC
             LIMIT 1"
        )))
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref()
            .map(row_to_job)
            .transpose()
            .map_err(StoreError::from)
    }

    /// Boot reconciliation: fail every `running` row EXCEPT `keep` (the job
    /// a standing `data/REPIN` marker still owns — its recovery completes
    /// it instead). A `running` row with no marker is an orphan from a
    /// killed process whose job never reached the cutover: the corpus is
    /// untouched, so the honest status is `failed`.
    pub async fn reconcile_orphans(&self, keep: Option<i64>) -> Result<u64, StoreError> {
        let done = sqlx::query(
            "UPDATE repin_jobs
             SET status = 'failed',
                 error = 'orphaned by restart: the process died before the cutover \
                          (corpus untouched)',
                 finished_at = COALESCE(finished_at, now())
             WHERE status = 'running' AND ($1::bigint IS NULL OR id <> $1)",
        )
        .bind(keep)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected())
    }
}
