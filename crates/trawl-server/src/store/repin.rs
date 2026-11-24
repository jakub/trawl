// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Postgres-backed repin job store (ADR-0011).

use chrono::{DateTime, Utc};
use sqlx::postgres::PgRow;
use sqlx::{AssertSqlSafe, PgPool, Row as _};
use trawl_core::schema::CanonicalType;
use trawl_core::severity::Dialect;

use super::catalog::{FieldConflict, MAX_CONFLICTS_PER_FIELD, record_conflicts_in};
use super::error::{PgViolation, StoreError, classify_violation};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepinJobStatus {
    Running,
    Succeeded,
    Failed,
    /// Terminal: nulled values without a force flag — either projected by
    /// the pre-build scan, or actually written by the finished shadow
    /// (data ingested after the scan), refused at the cutover gate.
    RefusedNeedsForce,
    Blocked,
    Cancelled,
}

impl RepinJobStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::RefusedNeedsForce => "refused_needs_force",
            Self::Blocked => "blocked",
            Self::Cancelled => "cancelled",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "running" => Some(Self::Running),
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            "refused_needs_force" => Some(Self::RefusedNeedsForce),
            "blocked" => Some(Self::Blocked),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RepinJob {
    pub(crate) force_terms: Option<crate::repin::ceiling::ForceTerms>,
    pub id: i64,
    pub field: String,
    /// The pin at claim time (catalog spelling: `SEVERITY` is not
    /// `BIGINT`).
    pub from_type: String,
    pub to_type: String,
    pub dry_run: bool,
    pub force: bool,
    pub status: RepinJobStatus,
    pub requested_by: Option<String>,
    pub requested_by_key_id: Option<i64>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    pub files_total: i64,
    pub rows_carrying: i64,
    pub projected_nulls: i64,
    pub resurrectable: i64,
    pub affected_bytes: i64,
    pub files_done: i64,
    pub rows_rewritten: i64,
    pub rows_nulled: i64,
    pub rows_resurrected: i64,
    /// The asserted dialect of the corpus's numerals: `Some` exactly for a
    /// `SEVERITY` target. Any other job is `None`, never a backfilled `otel`
    /// it did not assert.
    pub dialect: Option<String>,
    pub ambiguous_numerals: i64,
    pub unmapped_samples: Vec<String>,
    pub field_last_seen: Option<DateTime<Utc>>,
    pub field_last_service: Option<String>,
    /// Ceiling on nulled rows as the REQUEST stated it, or `None` when the
    /// operator stated none.
    pub max_nulled_rows: Option<i64>,
    pub max_ambiguous_rows: Option<i64>,
    /// The nulled-row ceiling the job is actually HELD to, resolved once at
    /// plan time. `None` until the scan records its plan, and for unforced jobs.
    pub accepted_max_nulled_rows: Option<i64>,
    pub accepted_max_ambiguous_rows: Option<i64>,
    /// When the scan recorded its plan, if it has. Until then the row's
    /// counts are zeros that mean "not measured yet", not "nothing to
    /// report", which is why the force verdict is absent rather than false
    /// before this is set.
    pub planned_at: Option<DateTime<Utc>>,
    /// When a cancel request was accepted for this job, if one was. A
    /// populated pair on a non-`Cancelled` row is not a contradiction: the
    /// job crashed before the request took effect, or the cancel lost its
    /// race with the point of no return.
    pub cancel_requested_at: Option<DateTime<Utc>>,
    pub cancelled_by: Option<String>,
}

/// Who requested a repin job. `key_id` is stable and never reissued, so it
/// is what the status read by id matches on; two keys may share one `name`,
/// which is why it decides nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requester {
    pub key_id: i64,
    pub name: String,
}

impl Requester {
    pub fn new(key_id: i64, name: impl Into<String>) -> Self {
        Self {
            key_id,
            name: name.into(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RepinClaim<'a> {
    pub field: &'a str,
    pub from_type: CanonicalType,
    pub to_type: CanonicalType,
    pub dialect: Option<Dialect>,
    pub dry_run: bool,
    pub force: bool,
    pub max_nulled_rows: Option<i64>,
    pub max_ambiguous_rows: Option<i64>,
    pub requester: Option<&'a Requester>,
}

/// Everything the scan learned, stamped onto the job row in one statement:
/// the counts, the evidence and the liveness fact. One write because they
/// are one reading of the corpus; a row carrying counts from one scan and
/// samples from another would report a corpus that never existed.
#[derive(Debug, Clone, Default)]
pub struct RepinPlan {
    pub files_total: i64,
    pub rows_carrying: i64,
    /// Stored values the new pin cannot read (would null).
    pub projected_nulls: i64,
    /// Currently-shelved values `_raw` gives back under the new pin.
    pub resurrectable: i64,
    /// Bytes across the affected files (the double-hold peak).
    pub affected_bytes: i64,
    pub ambiguous_numerals: i64,
    pub unmapped_samples: Vec<String>,
    pub field_last_seen: Option<DateTime<Utc>>,
    pub field_last_service: Option<String>,
    /// The nulled-row ceiling this job is held to. `None` only for an unforced
    /// job, which refuses on any loss and has nothing to hold.
    pub accepted_max_nulled_rows: Option<i64>,
    pub accepted_max_ambiguous_rows: Option<i64>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct JobTotals {
    pub files_done: i64,
    pub rows_rewritten: i64,
    pub rows_nulled: i64,
    pub rows_resurrected: i64,
    pub ambiguous_numerals: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CutoverOutcome {
    pub completed: bool,
    /// This call removed the field's degraded acknowledgement. False when
    /// there was none, and always false on a replay.
    pub cleared_ack: bool,
}

/// The conflict evidence a completing cutover owes, read off the tallies
/// the rewrite staged.
///
/// `None` means the columns are NULL: staging never ran for this job, which
/// is a different fact from staging having proved the repin lossless (empty
/// arrays, `Some(vec![])`).
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
            // A tally of zero is not a conflict: recording it would add an
            // episode to the degraded verdict's count for a service that lost
            // nothing.
            .filter(|(_, nulled)| *nulled > 0)
            .map(|(service, nulled)| FieldConflict {
                field: field.to_owned(),
                service,
                observed_type: observed_type.clone(),
                expected_type: to_type,
                rows_nulled: u64::try_from(nulled).unwrap_or_default(),
                // The rewrite counts what it nulled per service; it never
                // materialises the values.
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
    let force: bool = row.try_get("force")?;
    let planned_at: Option<DateTime<Utc>> = row.try_get("planned_at")?;
    let force_terms = crate::repin::ceiling::ForceTerms::from_record(
        force,
        planned_at.is_some(),
        row.try_get("accepted_max_nulled_rows")?,
        row.try_get("accepted_max_ambiguous_rows")?,
    )
    .map_err(|message| sqlx::Error::Decode(message.into()))?;
    if !force
        && (row.try_get::<Option<i64>, _>("max_nulled_rows")?.is_some()
            || row
                .try_get::<Option<i64>, _>("max_ambiguous_rows")?
                .is_some())
    {
        return Err(sqlx::Error::Decode(
            "unforced repin has requested force ceilings".into(),
        ));
    }
    Ok(RepinJob {
        force_terms,
        id: row.try_get("id")?,
        field: row.try_get("field")?,
        from_type: row.try_get("from_type")?,
        to_type: row.try_get("to_type")?,
        dry_run: row.try_get("dry_run")?,
        force: row.try_get("force")?,
        status,
        requested_by: row.try_get("requested_by")?,
        requested_by_key_id: row.try_get("requested_by_key_id")?,
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
        max_nulled_rows: row.try_get("max_nulled_rows")?,
        max_ambiguous_rows: row.try_get("max_ambiguous_rows")?,
        accepted_max_nulled_rows: row.try_get("accepted_max_nulled_rows")?,
        accepted_max_ambiguous_rows: row.try_get("accepted_max_ambiguous_rows")?,
        planned_at: row.try_get("planned_at")?,
        cancel_requested_at: row.try_get("cancel_requested_at")?,
        cancelled_by: row.try_get("cancelled_by")?,
    })
}

const JOB_COLS: &str = "id, field, from_type, to_type, dry_run, force, status, requested_by, \
     requested_by_key_id, started_at, finished_at, error, files_total, rows_carrying, \
     projected_nulls, \
     resurrectable, affected_bytes, files_done, rows_rewritten, rows_nulled, rows_resurrected, \
     dialect, ambiguous_numerals, unmapped_samples, field_last_seen, field_last_service, \
     max_nulled_rows, max_ambiguous_rows, accepted_max_nulled_rows, \
     accepted_max_ambiguous_rows, planned_at, cancel_requested_at, cancelled_by";

#[derive(Debug, Clone)]
pub struct RepinStore {
    pool: PgPool,
}

impl RepinStore {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// One transaction under [`super::CATALOG_LIFECYCLE_LOCK_KEY`], because
    /// the claim is the second half of a check-then-act the engine started
    /// when it read the field's pin out of the in-process cache. Pin gc's
    /// purge takes the same lock: gc commits first and this claim finds no pin,
    /// or the claim commits first and gc's own in-transaction running-row check
    /// refuses the purge.
    ///
    /// The revalidation is exact: the pin must still be the one the caller
    /// prepared against, not merely present.
    pub async fn claim(&self, claim: RepinClaim<'_>) -> Result<i64, StoreError> {
        Ok(self.claim_job(claim).await?.id)
    }

    pub async fn claim_job(&self, claim: RepinClaim<'_>) -> Result<RepinJob, StoreError> {
        let mut tx = self.pool.begin().await?;
        // Bound the advisory-lock wait the same way the purge bounds its
        // own side: `lock_timeout` does not cover advisory locks, so
        // without this a claim could wait on a gc purge without limit.
        sqlx::query("SET LOCAL statement_timeout = '5s'")
            .execute(&mut *tx)
            .await?;
        super::lock_catalog_lifecycle(&mut tx).await?;

        let expected = claim.from_type.as_catalog();
        let found: Option<String> =
            sqlx::query_scalar("SELECT duckdb_type FROM field_types WHERE field = $1")
                .bind(claim.field)
                .fetch_optional(&mut *tx)
                .await?;
        if found.as_deref() != Some(expected) {
            return Err(StoreError::RepinPinVanished {
                field: claim.field.to_owned(),
                expected,
                found,
            });
        }

        let result = sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO repin_jobs (field, from_type, to_type, dialect, dry_run, force,
                                     max_nulled_rows, max_ambiguous_rows, requested_by,
                                     requested_by_key_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             RETURNING {JOB_COLS}"
        )))
        .bind(claim.field)
        .bind(expected)
        .bind(claim.to_type.as_catalog())
        .bind(claim.dialect.map(Dialect::token))
        .bind(claim.dry_run)
        .bind(claim.force)
        .bind(claim.max_nulled_rows)
        .bind(claim.max_ambiguous_rows)
        .bind(claim.requester.map(|r| r.name.as_str()))
        .bind(claim.requester.map(|r| r.key_id))
        .fetch_one(&mut *tx)
        .await;
        let row = result.map_err(|e| match classify_violation(&e) {
            Some(PgViolation::RepinAlreadyRunning) => StoreError::RepinAlreadyRunning,
            _ => StoreError::from(e),
        })?;
        let job = row_to_job(&row)?;
        tx.commit().await?;
        Ok(job)
    }

    pub async fn record_plan(&self, id: i64, plan: RepinPlan) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE repin_jobs
             SET files_total = $2, rows_carrying = $3, projected_nulls = $4,
                 resurrectable = $5, affected_bytes = $6, ambiguous_numerals = $7,
                 unmapped_samples = $8, field_last_seen = $9, field_last_service = $10,
                 accepted_max_nulled_rows = $11, accepted_max_ambiguous_rows = $12,
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
        .bind(plan.accepted_max_nulled_rows)
        .bind(plan.accepted_max_ambiguous_rows)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// `ambiguous_numerals` is the one column the plan and the outcome
    /// share: the scan's projection stands until the build has actually
    /// written files, and then the shadow's own count (including everything
    /// the catch-up passes folded in) supersedes it. That count is what the
    /// cutover's force gate decides on, so it is what the report must show.
    pub async fn record_progress(&self, id: i64, totals: JobTotals) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE repin_jobs
             SET files_done = $2, rows_rewritten = $3, rows_nulled = $4,
                 rows_resurrected = $5, ambiguous_numerals = $6
             WHERE id = $1",
        )
        .bind(id)
        .bind(totals.files_done)
        .bind(totals.rows_rewritten)
        .bind(totals.rows_nulled)
        .bind(totals.rows_resurrected)
        .bind(totals.ambiguous_numerals)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Freeze the finished shadow's tallies on the job row: the five final
    /// counts and the per-service rows the rewrite nulled, in one statement
    /// (ADR-0022 ruling 2). `services` is `(service, rows_nulled)` pairs,
    /// already ordered and capped by the caller.
    ///
    /// Zero rows updated is an invariant violation, never a quiet success:
    /// the job was already terminal or is a dry run that has no rewrite to
    /// stage. Either way the caller is about to write a Cutover marker, and
    /// it must not.
    pub async fn stage_cutover_input(
        &self,
        id: i64,
        totals: JobTotals,
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

    /// Move a *running* job to a terminal status, stamping `finished_at`
    /// once. Returns whether this call is the one that terminalized the row.
    ///
    /// Conditional on purpose: the job's own ladder and the cancel path's
    /// unwind race for the terminal row, and an unconditional write would let
    /// a job reported `cancelled` turn back into `succeeded`.
    ///
    /// `totals` is `Some` when the caller holds counts the row must not
    /// contradict (the cutover's ceiling refusal): writing them in the same
    /// statement as the status makes the verdict and the numbers behind it
    /// land together or not at all.
    pub async fn finish_if_running(
        &self,
        id: i64,
        status: RepinJobStatus,
        error: Option<&str>,
        totals: Option<JobTotals>,
    ) -> Result<bool, StoreError> {
        debug_assert_ne!(status, RepinJobStatus::Running);
        let done = sqlx::query(
            "UPDATE repin_jobs
             SET status = $2, error = $3, finished_at = COALESCE(finished_at, now()),
                 files_done = COALESCE($4, files_done),
                 rows_rewritten = COALESCE($5, rows_rewritten),
                 rows_nulled = COALESCE($6, rows_nulled),
                 rows_resurrected = COALESCE($7, rows_resurrected),
                 ambiguous_numerals = COALESCE($8, ambiguous_numerals)
             WHERE id = $1 AND status = 'running'",
        )
        .bind(id)
        .bind(status.as_str())
        .bind(error)
        .bind(totals.map(|t| t.files_done))
        .bind(totals.map(|t| t.rows_rewritten))
        .bind(totals.map(|t| t.rows_nulled))
        .bind(totals.map(|t| t.rows_resurrected))
        .bind(totals.map(|t| t.ambiguous_numerals))
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(done == 1)
    }

    /// Record an accepted cancel request against the running job, returning
    /// the row it landed on (`None` when the job is no longer running).
    ///
    /// This is a record of the request, not of its effect: the terminal
    /// status is written later, by whichever file boundary observes the
    /// cancel token.
    pub async fn record_cancel_request(
        &self,
        id: i64,
        actor: &str,
    ) -> Result<Option<RepinJob>, StoreError> {
        let row = sqlx::query(AssertSqlSafe(format!(
            "UPDATE repin_jobs
             SET cancel_requested_at = COALESCE(cancel_requested_at, now()),
                 cancelled_by = COALESCE(cancelled_by, $2)
             WHERE id = $1 AND status = 'running'
             RETURNING {JOB_COLS}"
        )))
        .bind(id)
        .bind(actor)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref()
            .map(row_to_job)
            .transpose()
            .map_err(StoreError::from)
    }

    /// The cutover's pin flip: `field_types` takes the new type, the job
    /// completes, and the loss the rewrite staged becomes `field_conflicts`
    /// evidence — one transaction (ADR-0022). Idempotent on purpose: boot
    /// recovery replays this, and a redo must neither error nor restamp
    /// `finished_at`.
    ///
    /// The transaction OPENS with `SELECT 1 FROM field_types WHERE field =
    /// $1 FOR UPDATE`. The flip's own UPDATE is predicated `duckdb_type IS
    /// DISTINCT FROM $2`, so a resurrection-only repin (`to == current`) takes
    /// no lock at all, and `acknowledge_degraded_field` serializes against this
    /// transaction by taking `FOR SHARE` on that same row. Without the explicit
    /// lock the ack's upsert lands as a high-water over counters that were just
    /// reset, suppressing the new pin's first episodes.
    pub async fn finish_cutover(
        &self,
        id: i64,
        field: &str,
        to_type: CanonicalType,
    ) -> Result<CutoverOutcome, StoreError> {
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

        sqlx::query("SELECT 1 FROM field_types WHERE field = $1 FOR UPDATE")
            .bind(field)
            .fetch_optional(&mut *tx)
            .await?;
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

        let mut cleared_ack = false;
        if let Some(row) = &completed {
            super::CatalogStore::clear_conflict_evidence(&mut tx, field).await?;
            cleared_ack = super::CatalogStore::clear_degraded_ack(&mut tx, field).await?;
            match staged_conflicts(field, to_type, row)? {
                Some(conflicts) if !conflicts.is_empty() => {
                    record_conflicts_in(&mut tx, &conflicts, MAX_CONFLICTS_PER_FIELD).await?;
                }
                Some(_) => {}
                None => {
                    // Forward, never a refusal: the caller is past the
                    // Cutover marker. NULL can only mean the staging write
                    // never ran; the shadow that counted the losses is gone.
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
        Ok(CutoverOutcome {
            completed: completed.is_some(),
            cleared_ack,
        })
    }

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

    pub async fn get_requested_by(
        &self,
        id: i64,
        key_id: i64,
    ) -> Result<Option<RepinJob>, StoreError> {
        let row = sqlx::query(AssertSqlSafe(format!(
            "SELECT {JOB_COLS} FROM repin_jobs WHERE id = $1 AND requested_by_key_id = $2"
        )))
        .bind(id)
        .bind(key_id)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref()
            .map(row_to_job)
            .transpose()
            .map_err(StoreError::from)
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every status arm, listed once. `index_of` below is an exhaustive
    /// match, so a new arm cannot be added without landing in this array
    /// (and, through the test, in the migration's CHECK).
    const ALL_STATUSES: [RepinJobStatus; 6] = [
        RepinJobStatus::Running,
        RepinJobStatus::Succeeded,
        RepinJobStatus::Failed,
        RepinJobStatus::RefusedNeedsForce,
        RepinJobStatus::Blocked,
        RepinJobStatus::Cancelled,
    ];

    const fn index_of(status: RepinJobStatus) -> usize {
        match status {
            RepinJobStatus::Running => 0,
            RepinJobStatus::Succeeded => 1,
            RepinJobStatus::Failed => 2,
            RepinJobStatus::RefusedNeedsForce => 3,
            RepinJobStatus::Blocked => 4,
            RepinJobStatus::Cancelled => 5,
        }
    }

    /// The spellings inside the status CHECK's `IN (...)` list, in the
    /// initial schema's `repin_jobs` table. Scope to the named constraint so
    /// report/history status checks cannot satisfy the assertion.
    fn migration_status_spellings() -> Vec<String> {
        const SQL: &str = include_str!("../../migrations/20260913000001_initial_schema.sql");
        const OPEN: &str = "CHECK (status IN (";
        let constraint = SQL
            .split_once("CONSTRAINT repin_jobs_status_check")
            .expect("repin status constraint")
            .1;
        let start = constraint.find(OPEN).expect("repin status IN list") + OPEN.len();
        let rest = &constraint[start..];
        let end = rest.find(')').expect("the IN list closes");
        rest[..end]
            .split(',')
            .map(|s| s.trim().trim_matches('\'').to_owned())
            .collect()
    }

    #[test]
    fn status_vocabulary_matches_the_migration_check() {
        for status in ALL_STATUSES {
            assert_eq!(
                ALL_STATUSES[index_of(status)],
                status,
                "ALL_STATUSES must list every arm at its own index"
            );
        }

        let sql: std::collections::BTreeSet<String> =
            migration_status_spellings().into_iter().collect();
        let rust: std::collections::BTreeSet<String> =
            ALL_STATUSES.iter().map(|s| s.as_str().to_owned()).collect();
        assert_eq!(sql, rust, "repin_jobs status CHECK vs RepinJobStatus");
    }

    #[test]
    fn parse_round_trips_every_stored_spelling() {
        for status in ALL_STATUSES {
            assert_eq!(RepinJobStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(RepinJobStatus::parse("cancelling"), None);
    }
}
