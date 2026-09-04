// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Postgres-backed repin job store (ADR-0011).
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
use trawl_core::severity::Dialect;

use super::catalog::{FieldConflict, MAX_CONFLICTS_PER_FIELD, record_conflicts_in};
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
    /// The pin at claim time (catalog spelling: `SEVERITY` is not
    /// `BIGINT`).
    pub from_type: String,
    /// The target pin (catalog spelling).
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
    /// The asserted dialect of the corpus's numerals: `Some` exactly for a
    /// `SEVERITY` target. Any other job is `None`, never a backfilled `otel`
    /// it did not assert.
    pub dialect: Option<String>,
    /// Rows whose numeral reads as a different severity in each dialect
    /// (the 1-7 overlap): the scan's projection until the finished shadow
    /// supersedes it with what the rewrite actually saw.
    pub ambiguous_numerals: i64,
    /// Up to five distinct sanitised samples of values the new pin cannot
    /// read at all, `_raw` resurrection included.
    pub unmapped_samples: Vec<String>,
    /// Scan-time liveness: the newest observation of the field anywhere in
    /// the catalog, or `None` when nothing has written it inside the
    /// window.
    pub field_last_seen: Option<DateTime<Utc>>,
    /// One service behind that observation (audit/display only).
    pub field_last_service: Option<String>,
    /// When the scan recorded its plan, if it has. Until then the row's
    /// counts are zeros that mean "not measured yet", not "nothing to
    /// report", which is why the force verdict is absent rather than false
    /// before this is set.
    pub planned_at: Option<DateTime<Utc>>,
}

/// What a repin job is claimed for: the row's immutable half.
///
/// A struct rather than seven positional arguments: `dialect` is the one
/// piece an operator asserts that nothing else can derive, and threading it
/// as the seventh `Option` past two booleans is how a call site ends up
/// asserting syslog by accident.
#[derive(Debug, Clone, Copy)]
pub struct RepinClaim<'a> {
    /// The field to repin (catalog key, folded).
    pub field: &'a str,
    /// The pin at claim time.
    pub from_type: CanonicalType,
    /// The target pin.
    pub to_type: CanonicalType,
    /// The asserted numeral dialect — `Some` exactly for a `SEVERITY`
    /// target (the migration CHECKs the scope, so a mismatch is a 500, not
    /// a silently stored lie).
    pub dialect: Option<Dialect>,
    /// Whether this job stops after the scan.
    pub dry_run: bool,
    /// Whether a lossy projection was explicitly accepted.
    pub force: bool,
    /// Requesting key's display name (audit).
    pub requested_by: Option<&'a str>,
}

/// Everything the scan learned, stamped onto the job row in one statement:
/// the counts, the evidence and the liveness fact. One write because they
/// are one reading of the corpus; a row carrying counts from one scan and
/// samples from another would report a corpus that never existed.
#[derive(Debug, Clone, Default)]
pub struct RepinPlan {
    /// Affected files.
    pub files_total: i64,
    /// Rows carrying a stored value for the field.
    pub rows_carrying: i64,
    /// Stored values the new pin cannot read (would null).
    pub projected_nulls: i64,
    /// Currently-shelved values `_raw` gives back under the new pin.
    pub resurrectable: i64,
    /// Bytes across the affected files (the double-hold peak).
    pub affected_bytes: i64,
    /// Rows whose numeral reads as a different severity in each dialect.
    pub ambiguous_numerals: i64,
    /// Up to `MAX_CONFLICT_SAMPLES` distinct sanitised samples of the values
    /// the new pin cannot read.
    pub unmapped_samples: Vec<String>,
    /// Newest observation of the field inside `repin::LIVENESS_WINDOW`, or
    /// `None` when nothing has written it lately.
    pub field_last_seen: Option<DateTime<Utc>>,
    /// One service behind that observation.
    pub field_last_service: Option<String>,
}

/// The five counts a rewrite ends with: what [`RepinStore::record_progress`]
/// publishes as it goes, and what [`RepinStore::stage_cutover_input`] freezes
/// once the last pass is in.
///
/// A struct because the staging write takes them beside the per-service
/// tallies, and five same-typed positional counts next to a slice is how a
/// call site swaps `rows_nulled` and `rows_resurrected` without the compiler
/// noticing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RepinTotals {
    /// Affected files rewritten.
    pub files_done: i64,
    /// Rows written through the rewrite.
    pub rows_rewritten: i64,
    /// Stored values the new pin could not keep.
    pub rows_nulled: i64,
    /// Values recovered from `_raw`.
    pub rows_resurrected: i64,
    /// Rows whose numeral reads as a different severity in each dialect.
    pub ambiguous_numerals: i64,
}

/// The conflict evidence a completing cutover owes, read off the tallies
/// the rewrite staged (`nulled_services` / `nulled_service_rows`, paired
/// positionally and kept equal in length by a CHECK).
///
/// `None` means the columns are NULL: staging never ran for this job, which
/// is a different fact from staging having proved the repin lossless (empty
/// arrays, `Some(vec![])`). Only the caller can act on that difference, so
/// the distinction rides out rather than collapsing here.
///
/// The evidence names the pin the values were stored under, in the CATALOG
/// spelling: `as_duckdb` is not injective, so a SEVERITY source would indict
/// itself as BIGINT, a pin the field never had.
fn staged_conflicts(
    field: &str,
    to_type: CanonicalType,
    row: &PgRow,
) -> Result<Option<Vec<FieldConflict>>, sqlx::Error> {
    let Some(services): Option<Vec<String>> = row.try_get("nulled_services")? else {
        return Ok(None);
    };
    let rows: Vec<i64> = row
        .try_get::<Option<Vec<i64>>, _>("nulled_service_rows")?
        .unwrap_or_default();
    let observed_type: String = row.try_get("from_type")?;
    Ok(Some(
        services
            .into_iter()
            .zip(rows)
            // A tally of zero is not a conflict — a cast that nulls nothing
            // is convergence, and recording it would add an episode to the
            // degraded verdict's count for a service that lost nothing. The
            // engine stages only positive tallies; this keeps that true of
            // the rows regardless.
            .filter(|(_, nulled)| *nulled > 0)
            .map(|(service, nulled)| FieldConflict {
                field: field.to_owned(),
                service,
                observed_type: observed_type.clone(),
                expected_type: to_type,
                rows_nulled: u64::try_from(nulled).unwrap_or_default(),
                // The rewrite counts what it nulled per service; it never
                // materialises the values. Carrying them back would be a
                // second full pass over the corpus for evidence the operator
                // asked for this repin in spite of.
                samples: Vec::new(),
            })
            .collect(),
    ))
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
        dialect: row.try_get("dialect")?,
        ambiguous_numerals: row.try_get("ambiguous_numerals")?,
        unmapped_samples: row.try_get("unmapped_samples")?,
        field_last_seen: row.try_get("field_last_seen")?,
        field_last_service: row.try_get("field_last_service")?,
        planned_at: row.try_get("planned_at")?,
    })
}

const JOB_COLS: &str = "id, field, from_type, to_type, dry_run, force, status, requested_by, \
     started_at, finished_at, error, files_total, rows_carrying, projected_nulls, \
     resurrectable, affected_bytes, files_done, rows_rewritten, rows_nulled, rows_resurrected, \
     dialect, ambiguous_numerals, unmapped_samples, field_last_seen, field_last_service, \
     planned_at";

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

    /// Claim the running slot: insert a `running` row, mapping a 23505 on
    /// `repin_jobs_one_running` to [`StoreError::RepinAlreadyRunning`].
    pub async fn claim(&self, claim: RepinClaim<'_>) -> Result<i64, StoreError> {
        let result = sqlx::query_scalar::<_, i64>(
            "INSERT INTO repin_jobs (field, from_type, to_type, dialect, dry_run, force,
                                     requested_by)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             RETURNING id",
        )
        .bind(claim.field)
        .bind(claim.from_type.as_catalog())
        .bind(claim.to_type.as_catalog())
        .bind(claim.dialect.map(Dialect::token))
        .bind(claim.dry_run)
        .bind(claim.force)
        .bind(claim.requested_by)
        .fetch_one(&self.pool)
        .await;
        result.map_err(|e| match classify_violation(&e) {
            Some(PgViolation::RepinAlreadyRunning) => StoreError::RepinAlreadyRunning,
            _ => StoreError::from(e),
        })
    }

    /// Stamp the scan's whole reading onto the job row.
    pub async fn record_plan(&self, id: i64, plan: RepinPlan) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE repin_jobs
             SET files_total = $2, rows_carrying = $3, projected_nulls = $4,
                 resurrectable = $5, affected_bytes = $6, ambiguous_numerals = $7,
                 unmapped_samples = $8, field_last_seen = $9, field_last_service = $10,
                 planned_at = now()
             WHERE id = $1",
        )
        .bind(id)
        .bind(plan.files_total)
        .bind(plan.rows_carrying)
        .bind(plan.projected_nulls)
        .bind(plan.resurrectable)
        .bind(plan.affected_bytes)
        .bind(plan.ambiguous_numerals)
        .bind(&plan.unmapped_samples)
        .bind(plan.field_last_seen)
        .bind(plan.field_last_service.as_deref())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Stamp rewrite progress/outcome tallies onto the job row.
    ///
    /// `ambiguous_numerals` is the one column the plan and the outcome
    /// share: the scan's projection stands until the build has actually
    /// written files, and then the shadow's own count (including everything
    /// the catch-up passes folded in) supersedes it. That count is what the
    /// cutover's force gate decides on, so it is what the report must show.
    pub async fn record_progress(
        &self,
        id: i64,
        files_done: i64,
        rows_rewritten: i64,
        rows_nulled: i64,
        rows_resurrected: i64,
        ambiguous_numerals: i64,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE repin_jobs
             SET files_done = $2, rows_rewritten = $3, rows_nulled = $4,
                 rows_resurrected = $5, ambiguous_numerals = $6
             WHERE id = $1",
        )
        .bind(id)
        .bind(files_done)
        .bind(rows_rewritten)
        .bind(rows_nulled)
        .bind(rows_resurrected)
        .bind(ambiguous_numerals)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Freeze the finished shadow's tallies on the job row: the five final
    /// counts and the per-service rows the rewrite nulled, in one statement.
    ///
    /// This is the evidence barrier (issue #137). The cutover materialises
    /// `field_conflicts` rows from these tallies in the same transaction
    /// that flips the pin, so the read that first sees `succeeded` also sees
    /// the evidence. That only works if the tallies are durable before the
    /// `data/REPIN` Cutover marker goes down, because past that marker the
    /// engine is forward-only and a crash completes the flip from the marker
    /// alone, with no shadow left to re-count.
    ///
    /// One UPDATE, not one per column set: the counts and the per-service
    /// tallies are one reading of the finished shadow, and a row carrying
    /// totals from the last pass beside tallies from the one before would
    /// describe a corpus that never existed. `services` is
    /// `(service, rows_nulled)` pairs, already ordered and capped by the
    /// caller.
    ///
    /// Zero rows updated is an invariant violation, never a quiet success:
    /// the `WHERE` demands a `running`, non-dry-run row, so no match means
    /// the job was already terminal (an orphan reconciliation, a concurrent
    /// finish) or is a dry run that has no rewrite to stage. Either way the
    /// caller is about to write a Cutover marker for a job postgres does not
    /// agree is running, and it must not.
    ///
    /// Re-staging the same job while it is still `running` is fine: the
    /// statement is a plain overwrite, so a retried staging call lands the
    /// same row.
    pub async fn stage_cutover_input(
        &self,
        id: i64,
        totals: RepinTotals,
        services: &[(String, i64)],
    ) -> Result<(), StoreError> {
        let names: Vec<&str> = services.iter().map(|(s, _)| s.as_str()).collect();
        let rows: Vec<i64> = services.iter().map(|(_, n)| *n).collect();

        let mut tx = self.pool.begin().await?;
        // The staging commit is the durability barrier the Cutover marker
        // depends on, so it is flushed to disk before this call returns,
        // whatever `synchronous_commit` the session or the server default
        // otherwise carries.
        sqlx::query("SET LOCAL synchronous_commit = on")
            .execute(&mut *tx)
            .await?;
        let staged = sqlx::query_scalar::<_, i64>(
            "UPDATE repin_jobs
             SET files_done = $2, rows_rewritten = $3, rows_nulled = $4,
                 rows_resurrected = $5, ambiguous_numerals = $6,
                 nulled_services = $7, nulled_service_rows = $8
             WHERE id = $1 AND status = 'running' AND NOT dry_run
             RETURNING id",
        )
        .bind(id)
        .bind(totals.files_done)
        .bind(totals.rows_rewritten)
        .bind(totals.rows_nulled)
        .bind(totals.rows_resurrected)
        .bind(totals.ambiguous_numerals)
        .bind(&names)
        .bind(&rows)
        .fetch_optional(&mut *tx)
        .await?;
        if staged.is_none() {
            return Err(StoreError::Validation(format!(
                "repin job id={id} is not a running execution: cutover tallies cannot be staged"
            )));
        }
        tx.commit().await?;
        Ok(())
    }

    /// Move a job to a terminal status (never `running`), stamping
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

    /// The cutover's pin flip: `field_types` takes the new type, the job
    /// completes, and the loss the rewrite staged becomes `field_conflicts`
    /// evidence — one transaction, so a crash between any two of them cannot
    /// leave a flipped pin with a `running` job, or a `succeeded` job whose
    /// losses nothing has recorded.
    ///
    /// That last part is the evidence barrier (issue #137): the read that
    /// first sees `succeeded` also sees the conflicts the repin caused.
    /// Materialising here rather than in the engine after the flip is what
    /// makes it true for boot recovery too — recovery replays this call and
    /// nothing else, so evidence written by the engine would simply never
    /// exist for a job that died between the swap and the flip.
    ///
    /// Idempotent on purpose: boot recovery replays this after a crash in
    /// the cutover or cleanup window, and a redo must neither error nor
    /// restamp `finished_at`.
    ///
    /// The completing UPDATE gates and pays out in one locked statement —
    /// it returns the staged tallies only when this call is the one that
    /// moved the row out of `running`. Both writes that are not naturally
    /// idempotent hang off that: clearing the old evidence (it indicts a pin
    /// that no longer exists, and the analyzer's gate is span-based, so
    /// leaving it would badge the field as degraded forever and the
    /// operator's remedy would not clear the sign that told them to apply
    /// it) and inserting the new. A replay of an already-succeeded job gets
    /// no row back and therefore touches neither.
    ///
    /// The job row's own `field`/`from_type`/`to_type` must agree with the
    /// call's arguments. They are the same facts by two routes — the caller
    /// passes what the `data/REPIN` marker or the engine holds, the row
    /// carries what was claimed — and a disagreement means the wrong job is
    /// about to complete a flip, so the transaction is abandoned untouched.
    pub async fn finish_cutover(
        &self,
        id: i64,
        field: &str,
        to_type: CanonicalType,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;

        // Fails closed before anything is mutated: `field`, `from_type` and
        // `to_type` are immutable for the row's whole life, so reading them
        // unlocked here and completing below cannot race.
        let identity = sqlx::query("SELECT field, to_type FROM repin_jobs WHERE id = $1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(identity) = identity else {
            return Err(StoreError::NotFound {
                id,
                resource: "repin job",
            });
        };
        let claimed_field: String = identity.try_get("field")?;
        let claimed_to: String = identity.try_get("to_type")?;
        if claimed_field != field || claimed_to != to_type.as_catalog() {
            return Err(StoreError::Validation(format!(
                "repin job id={id} was claimed to repin {claimed_field:?} to {claimed_to}, \
                 but the cutover names {field:?} to {}: refusing to flip a pin this job \
                 never planned",
                to_type.as_catalog()
            )));
        }

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
             WHERE id = $1 AND status = 'running'
             RETURNING from_type, rows_nulled, nulled_services, nulled_service_rows",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;

        if let Some(row) = completed {
            super::CatalogStore::clear_conflict_evidence(&mut tx, field).await?;
            match staged_conflicts(field, to_type, &row)? {
                Some(conflicts) if !conflicts.is_empty() => {
                    record_conflicts_in(&mut tx, &conflicts, MAX_CONFLICTS_PER_FIELD).await?;
                }
                Some(_) => {}
                None => {
                    // Forward, never a refusal: the caller is past the
                    // Cutover marker, where the corpus is already the new
                    // generation and the only direction is done. NULL is
                    // precise rather than heuristic — it can only mean the
                    // staging write never ran, so this is a job from before
                    // the barrier existed, or one whose engine died between
                    // the marker and the flip. Its losses are unrecorded and
                    // unrecoverable (the shadow that counted them is gone),
                    // which is worth exactly one line of ops signal.
                    let rows_nulled: i64 = row.try_get("rows_nulled")?;
                    tracing::warn!(
                        event_type = "repin_evidence_unstaged",
                        job_id = id,
                        field,
                        rows_nulled,
                        "completing a repin whose cutover tallies were never staged; \
                         any loss it wrote is absent from the conflict evidence"
                    );
                }
            }
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

    /// Boot reconciliation: fail every `running` row except `keep` (the job
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
