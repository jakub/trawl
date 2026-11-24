// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Postgres-backed field catalog: type pins, per-service observations, and
//! conflict evidence (ADR-0009).
//!
//! Pins are add-only on the ingest path, and a pin slot is spent for good while
//! its name is a client-chosen key, so the catalog is bounded where a sender
//! controls the axis: name length by
//! [`trawl_core::schema::is_storable_field_name`], pin count by
//! [`MAX_PINNED_FIELDS`], and per-field conflict evidence by
//! [`MAX_CONFLICTS_PER_FIELD`]. The `field_services` service axis has no cap at
//! all, so [`CatalogStore::field_services`] pages it.

use std::collections::{BTreeSet, HashMap};

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row as _};
use trawl_core::schema::CanonicalType;

use super::error::{PgViolation, StoreError, classify_violation};
use super::pooled::PooledTransaction;
use crate::catalog::analyzer::{ConflictAggregate, is_degraded, meets_degraded_threshold};

#[derive(Debug, Clone)]
pub struct PinProposal {
    pub field: String,
    pub ty: CanonicalType,
    pub pinned_from: String,
}

/// A batch column whose `TRY_CAST` to its pin nulled at least one value. A cast
/// that nulls nothing is convergence, not conflict, and is never recorded (see
/// `ConformPlan::tally_conflicts`); a row per lossless cast per compaction tick
/// would grow without bound.
#[derive(Debug, Clone)]
pub struct FieldConflict {
    pub field: String,
    pub service: String,
    /// The `DuckDB` type the batch actually carried (free-form spelling —
    /// may be `JSON` or a complex type, not only the canonical five).
    pub observed_type: String,
    pub expected_type: CanonicalType,
    /// Rows whose value the cast nulled (recoverable from `_raw`).
    pub rows_nulled: u64,
    /// Up to [`MAX_CONFLICT_SAMPLES`] distinct values the cast nulled, each
    /// sanitised and cut to [`MAX_CONFLICT_SAMPLE_BYTES`] at capture. A
    /// sample, never a manifest: the exhaustive record of what was shelved
    /// is `_raw`. Empty where the lane has no values in hand (the repin
    /// rewrite counts its own nulls).
    pub samples: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ServiceObservation {
    /// Field name (already ASCII-folded — a catalog key).
    pub field: String,
    pub service: String,
    /// Earliest instant the corpus attests to (the oldest partition
    /// directory carrying the column).
    pub first_seen: DateTime<Utc>,
    /// Latest instant the corpus attests to (the newest such partition).
    pub last_seen: DateTime<Utc>,
    pub row_count: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictServicePair {
    pub field: String,
    pub service: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DegradedAck {
    pub field: String,
    pub acked_at: DateTime<Utc>,
    /// The acknowledging key's stable prefix — never its display name: this
    /// row outlives renames and rotations.
    pub acked_by: String,
    /// Operator prose, capped at 1024 bytes by the migration's CHECK.
    pub note: Option<String>,
    /// The acknowledged episode high-water: the field's summed
    /// `field_conflict_stats.episodes` at the instant of the ack. Evidence
    /// up to and including this count is suppressed; the next episode
    /// re-raises the verdict.
    pub evidence_through: i64,
}

#[derive(Debug, Clone)]
pub struct FieldHealthSnapshot {
    pub aggregate: Option<crate::catalog::analyzer::ConflictAggregate>,
    pub evidence: Vec<(String, Vec<String>)>,
    pub ack: Option<DegradedAck>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AckOutcome {
    Acked {
        /// The stored row, as postgres returned it. On a stale call this is
        /// the OTHER operator's row, unchanged.
        ack: DegradedAck,
        created: bool,
        /// True when this call's evidence high-water was at least the
        /// stored one, so the row now carries this call's actor, note and
        /// timestamp. False means a concurrent ack had already covered more
        /// evidence and this call changed nothing.
        advanced: bool,
    },
    Unpinned,
    /// The field's evidence does not meet the degraded threshold, so there
    /// is no verdict to suppress. Acknowledging it would install a
    /// high-water that silently swallows the evidence that would have
    /// raised the badge for the first time.
    NotDegraded,
}

#[derive(Debug, Clone)]
pub struct FieldServiceRow {
    pub service: String,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    /// Cumulative rows compacted in batches that wrote the field — the sum
    /// of per-batch row counts, not a per-value non-null tally.
    pub row_count: i64,
}

/// `(last_seen, service)` is unique per field — `(field, service)` is the
/// primary key — so the pair identifies an exact row, and resuming after it
/// can neither repeat nor skip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceCursor {
    pub last_seen: DateTime<Utc>,
    pub service: String,
}

impl ServiceCursor {
    /// Wire spelling: `<rfc3339-micros>|<service>`.
    ///
    /// `|` is outside the service charset (`[A-Za-z0-9._-]`, enforced at
    /// every ingest door), and RFC 3339 has no `|` either, so splitting at
    /// the last `|` recovers both halves unambiguously.
    #[must_use]
    pub fn encode(&self) -> String {
        format!(
            "{}|{}",
            self.last_seen
                .to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
            self.service
        )
    }

    #[must_use]
    pub fn decode(raw: &str) -> Option<Self> {
        let (ts, service) = raw.rsplit_once('|')?;
        if service.is_empty() {
            return None;
        }
        let last_seen = DateTime::parse_from_rfc3339(ts).ok()?.with_timezone(&Utc);
        Some(Self {
            last_seen,
            service: service.to_owned(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct FieldListFilter {
    /// Only fields observed for this service, and every aggregate below
    /// (`service_count`, `row_count`, `first_seen`, `last_seen`, and the
    /// `since` window read off them) is computed from that service's
    /// observations alone. `None` lists every pin over every service.
    pub service: Option<String>,
    /// Window on the field's most recent observation: a field whose
    /// `max(last_seen)` (within `service`, when set) predates this instant
    /// is hidden. A field with no observations at all, such as the envelope
    /// seed on a fresh install, is always shown, since there is nothing to
    /// age out. `None` disables the window.
    pub since: Option<DateTime<Utc>>,
    /// Maximum rows returned (the caller clamps; see the route handlers).
    pub limit: i64,
    /// Fetch the per-field conflict evidence (`conflict_count`,
    /// `rows_nulled`). Costs a second, page-scoped query against
    /// `field_conflicts`; callers that only want names and types
    /// (`/api/v1/schema`) set this `false` and read zeroes.
    pub with_conflicts: bool,
}

impl Default for FieldListFilter {
    fn default() -> Self {
        Self {
            service: None,
            since: None,
            limit: MAX_PINNED_FIELDS,
            with_conflicts: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FieldSummaryRow {
    pub field: String,
    pub duckdb_type: String,
    /// Which service's batch set the pin (`_declared` for the envelope
    /// seed; `NULL` only in hand-edited catalogs).
    pub pinned_from: Option<String>,
    pub pinned_at: DateTime<Utc>,
    // The four observation aggregates below span every service that ever
    // carried the field — or exactly the one service, when the listing was
    // scoped by [`FieldListFilter::service`].
    /// Distinct services that ever carried the field (1 when scoped).
    pub service_count: i64,
    pub row_count: i64,
    pub first_seen: Option<DateTime<Utc>>,
    pub last_seen: Option<DateTime<Utc>>,
    pub conflict_count: i64,
    pub rows_nulled: i64,
}

/// The metadata axis has not been consulted yet, so this is a pin to
/// disprove with a parquet footer, never a decision to delete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcPinRow {
    pub field: String,
    pub duckdb_type: String,
    pub pinned_at: DateTime<Utc>,
    pub last_seen: Option<DateTime<Utc>>,
    /// Distinct services that ever carried the field, over its whole history
    /// rather than the window.
    pub services: i64,
}

/// Postgres enforces them, so the bound survives what a client-side timeout
/// cannot: the caller must not cancel that future, because a cancelled commit
/// is an unknown outcome the pin cache would have to guess at.
const PURGE_STATEMENT_TIMEOUT_SQL: &str = "SET LOCAL statement_timeout = '5s'";
const PURGE_LOCK_TIMEOUT_SQL: &str = "SET LOCAL lock_timeout = '5s'";

/// A connection that dies mid-statement leaves this task waiting on a socket
/// while the corpus gate stays shut, and no database-side setting can end that
/// wait. The bound must EXCEED the composed honest maximum: `statement_timeout`
/// resets per statement, so an honest worst case approaches fifty seconds.
pub const PURGE_PREPARE_BOUND: std::time::Duration = std::time::Duration::from_secs(60);

/// The commit is the one step postgres does not bound: once durable commit
/// processing has begun the backend stops honouring cancellation, so a commit
/// stuck behind a stalled fsync can outlast every `statement_timeout` in the
/// transaction.
///
/// Giving up here does NOT cancel the commit. The future is detached onto its
/// own task and runs to completion.
pub const PURGE_COMMIT_BOUND: std::time::Duration = std::time::Duration::from_secs(30);

/// A [`tokio::time::timeout`] around the commit would DROP the future, and a
/// dropped commit is one postgres may still perform. Here the future is boxed,
/// so the elapsed arm can hand the same future to a task of its own instead.
async fn commit_or_detach(tx: PooledTransaction, purged: usize) -> Result<(), StoreError> {
    let mut commit = Box::pin(tx.commit());
    tokio::select! {
        // The commit is polled first on every wake, so one that lands in
        // the same tick as the timer is a commit, not a timeout.
        biased;
        result = &mut commit => match result {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::debug!(
                    target: "storage.backend",
                    event_type = "catalog_pin_purge_commit_failed",
                    fields = purged,
                    error = %e,
                    "a field-catalog pin purge's commit returned an error; postgres \
                     may have applied it anyway, so whether those pins are gone is \
                     unknown"
                );
                let fault = crate::error::ServerError::from(StoreError::from(e));
                tracing::warn!(
                    target: "storage.backend",
                    event_type = "catalog_pin_purge_commit_failed",
                    fields = purged,
                    error_class = fault.error_class(),
                    cause_kind = fault.cause_kind().as_str(),
                    "a field-catalog pin purge's commit returned an error; postgres \
                     may have applied it anyway, so whether those pins are gone is \
                     unknown"
                );
                Err(StoreError::PurgeCommitUnknown)
            }
        },
        () = tokio::time::sleep(PURGE_COMMIT_BOUND) => {
            tokio::spawn(async move {
                match commit.await {
                    Ok(()) => tracing::info!(
                        event_type = "catalog_pin_purge_commit_late",
                        fields = purged,
                        after_secs = PURGE_COMMIT_BOUND.as_secs(),
                        "a field-catalog pin purge committed after its caller stopped \
                         waiting; those pins are gone"
                    ),
                    Err(e) => {
                        tracing::debug!(
                            target: "storage.backend",
                            event_type = "catalog_pin_purge_commit_late_failed",
                            fields = purged,
                            after_secs = PURGE_COMMIT_BOUND.as_secs(),
                            error = %e,
                            "a field-catalog pin purge's commit reported an error after \
                             its caller stopped waiting; whether those pins are gone is \
                             unknown"
                        );
                        let fault = crate::error::ServerError::from(StoreError::from(e));
                        tracing::warn!(
                            target: "storage.backend",
                            event_type = "catalog_pin_purge_commit_late_failed",
                            fields = purged,
                            after_secs = PURGE_COMMIT_BOUND.as_secs(),
                            error_class = fault.error_class(),
                            cause_kind = fault.cause_kind().as_str(),
                            "a field-catalog pin purge's commit reported an error after \
                             its caller stopped waiting; whether those pins are gone is \
                             unknown"
                        );
                    }
                }
            });
            Err(StoreError::PurgeCommitUnknown)
        }
    }
}

/// Everything here is read inside the purge transaction rather than carried
/// over from the caller's earlier candidate snapshot: between that snapshot and
/// the purge, a repin cutover can retype the row and compaction can observe the
/// field again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurgedPin {
    pub field: String,
    pub duckdb_type: String,
    pub pinned_from: Option<String>,
    pub pinned_at: DateTime<Utc>,
    pub last_seen: Option<DateTime<Utc>>,
    pub services: i64,
}

/// All of it is read INSIDE the transaction, so the caller can finish the
/// reclaim (evict from the pin cache, publish the gauges, audit) with nothing
/// fallible left to do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PurgedPins {
    /// The pins `field_types` actually gave up, as the DELETE returned
    /// them, sorted by field: the only set an audit event may name.
    pub deleted: Vec<PurgedPin>,
    /// Pins remaining at commit, for the fill gauges. Meaningless when
    /// nothing was deleted (the empty-input early return reads zero), so
    /// callers publish it only after a real purge.
    pub pinned_now: i64,
}

#[derive(Debug, Clone)]
pub struct FieldPinRow {
    pub field: String,
    pub duckdb_type: String,
    pub pinned_from: Option<String>,
    pub pinned_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct ConflictListRow {
    pub field: String,
    pub service: String,
    pub observed_type: String,
    pub expected_type: String,
    pub rows_nulled: i64,
    pub samples: Vec<String>,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct FieldConflictRow {
    pub service: String,
    pub observed_type: String,
    pub expected_type: String,
    pub rows_nulled: i64,
    pub samples: Vec<String>,
    pub at: DateTime<Utc>,
}

/// A field denied a pin gets the same treatment as an unstorable name: it is
/// absent from the pin map, so compaction's conform step drops the column and
/// the values stay findable in `_raw`.
///
/// A hand-run `DELETE FROM field_types` proves neither of gc's proofs and must
/// still never be recommended.
pub const MAX_PINNED_FIELDS: i64 = 10_000;

/// Maximum `field_conflicts` rows kept per field — the newest survive.
pub const MAX_CONFLICTS_PER_FIELD: i64 = 100;

/// Distinct misfit values kept per conflict row.
pub const MAX_CONFLICT_SAMPLES: usize = 5;

/// Bytes kept per sample, cut on a `char` boundary at capture.
pub const MAX_CONFLICT_SAMPLE_BYTES: usize = 256;

/// Conflict rows per field the verdict is built from
/// ([`CatalogStore::conflict_evidence_for`]), newest first.
const VERDICT_EVIDENCE_ROWS: i64 = 5;

// Written as a literal because it binds as a postgres `bigint`, and pinned
// to the sample budget it is derived from: change one and this fails the
// build rather than silently under-filling a verdict.
const _: () = assert!(MAX_CONFLICT_SAMPLES == 5);

const BACKFILL_CHUNK: usize = 1_000;

/// One field name shortened for logging: a name can be as long as a client
/// made it, so echoing it whole turns the log line into an amplifier of
/// whatever was sent.
fn short_name(name: &str) -> String {
    let head: String = name.chars().take(48).collect();
    format!("{head}... ({} bytes)", name.len())
}

fn unstorable_names<'a>(names: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut out: Vec<String> = names
        .filter(|n| !trawl_core::schema::is_storable_field_name(n))
        .map(short_name)
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// One warning per call site for skipped names. Deliberately a warning and
/// not an error: the alternative — failing the statement — wedges the
/// caller (compaction retains the batch and re-fails forever), which is the
/// failure this skipping exists to prevent.
fn warn_unstorable(op: &str, rejected: &[String]) {
    tracing::warn!(
        event_type = "catalog_field_name_unstorable",
        op,
        max_bytes = trawl_core::schema::MAX_FIELD_NAME_BYTES,
        fields = ?rejected,
        "field name(s) too long to be a catalog key — skipped; the column is \
         not pinned (and so not stored), values remain in _raw"
    );
    bump_rejected("name_too_long", rejected.len());
}

#[derive(Debug, Clone, Copy)]
enum Ration {
    HalfOfFree,
    /// Boot conformance pass: every free slot, because there a denied pin
    /// deletes a column that is already on disk
    /// ([`CatalogStore::pin_missing_unrationed`]).
    EveryFreeSlot,
}

#[allow(clippy::cast_precision_loss)] // gauge values are f64; a 10k cap is exact
fn set_fill_gauges(pinned: i64, cap: i64) {
    metrics::gauge!(crate::metrics::CATALOG_PINNED_FIELDS).set(pinned as f64);
    metrics::gauge!(crate::metrics::CATALOG_PIN_CAPACITY).set(cap as f64);
}

/// No field-name label: the names are client-chosen and unbounded, which is the
/// very problem being counted.
fn bump_rejected(reason: &'static str, count: usize) {
    metrics::counter!(
        crate::metrics::CATALOG_PINS_REJECTED_TOTAL,
        "reason" => reason
    )
    .increment(count as u64);
}

/// It commits nothing: the repin cutover has to write this evidence in the SAME
/// transaction that flips the pin, so that the read which first sees
/// `succeeded` also sees the conflicts the repin caused.
pub(super) async fn record_conflicts_in(
    tx: &mut sqlx::PgConnection,
    conflicts: &[FieldConflict],
    conflict_cap: i64,
) -> Result<(), StoreError> {
    let fields: Vec<&str> = conflicts.iter().map(|c| c.field.as_str()).collect();
    let services: Vec<&str> = conflicts.iter().map(|c| c.service.as_str()).collect();
    let observed: Vec<&str> = conflicts.iter().map(|c| c.observed_type.as_str()).collect();
    let expected: Vec<&str> = conflicts
        .iter()
        .map(|c| c.expected_type.as_catalog())
        .collect();
    let nulled: Vec<i64> = conflicts
        .iter()
        .map(|c| i64::try_from(c.rows_nulled).unwrap_or(i64::MAX))
        .collect();
    // `TEXT[][]` has no sqlx binding (postgres multidimensional arrays
    // must be rectangular, and these rows are not), so the per-row sample
    // sets ride as JSON and are unnested back to arrays in the statement.
    let samples: Vec<String> = conflicts
        .iter()
        .map(|c| serde_json::to_string(&c.samples).unwrap_or_else(|_| "[]".to_owned()))
        .collect();

    sqlx::query(
        "INSERT INTO field_conflicts
             (field, service, observed_type, expected_type, rows_nulled, samples)
         SELECT f, s, o, e, n, ARRAY(SELECT jsonb_array_elements_text(j::jsonb))
         FROM UNNEST($1::text[], $2::text[], $3::text[], $4::text[], $5::bigint[],
                     $6::text[]) AS t(f, s, o, e, n, j)",
    )
    .bind(&fields)
    .bind(&services)
    .bind(&observed)
    .bind(&expected)
    .bind(&nulled)
    .bind(&samples)
    .execute(&mut *tx)
    .await?;

    // The trim runs as its own statement rather than a CTE beside the
    // insert: CTEs of one statement share its snapshot, so a ranking
    // CTE cannot see the rows the insert alongside it just wrote —
    // which is precisely the set the window has to rank.
    sqlx::query(
        "DELETE FROM field_conflicts c
         USING (
             SELECT id, row_number() OVER (
                 PARTITION BY field ORDER BY at DESC, id DESC
             ) AS rn
             FROM field_conflicts WHERE field = ANY($1)
         ) ranked
         WHERE c.id = ranked.id AND ranked.rn > $2",
    )
    .bind(&fields)
    .bind(conflict_cap)
    .execute(&mut *tx)
    .await?;

    // The GROUP BY is not an optimisation: postgres refuses to let one `ON
    // CONFLICT DO UPDATE` touch a row twice, and one call routinely carries
    // many rows for the same `(field, service)`.
    sqlx::query(
        "INSERT INTO field_conflict_stats
             (field, service, episodes, rows_nulled_total)
         SELECT f, s, count(*)::bigint, COALESCE(sum(n), 0)::bigint
         FROM UNNEST($1::text[], $2::text[], $3::bigint[]) AS t(f, s, n)
         GROUP BY f, s
         ON CONFLICT (field, service) DO UPDATE
         SET last_at           = now(),
             episodes          = field_conflict_stats.episodes + EXCLUDED.episodes,
             rows_nulled_total = field_conflict_stats.rows_nulled_total
                                 + EXCLUDED.rows_nulled_total",
    )
    .bind(&fields)
    .bind(&services)
    .bind(&nulled)
    .execute(&mut *tx)
    .await?;

    Ok(())
}

#[derive(Debug, Clone)]
pub struct CatalogStore {
    pool: PgPool,
    pin_cap: i64,
    conflict_cap: i64,
}

impl CatalogStore {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            pin_cap: MAX_PINNED_FIELDS,
            conflict_cap: MAX_CONFLICTS_PER_FIELD,
        }
    }

    #[must_use]
    pub fn with_pin_cap(mut self, cap: i64) -> Self {
        self.pin_cap = cap;
        self
    }

    #[must_use]
    pub fn with_conflict_cap(mut self, cap: i64) -> Self {
        self.conflict_cap = cap;
        self
    }

    pub async fn load_pins(&self) -> Result<Vec<(String, CanonicalType)>, StoreError> {
        let rows = sqlx::query("SELECT field, duckdb_type FROM field_types ORDER BY field")
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|row| {
                let field: String = row.try_get("field")?;
                let spelling: String = row.try_get("duckdb_type")?;
                let ty = CanonicalType::from_catalog(&spelling).ok_or_else(|| {
                    sqlx::Error::Decode(
                        format!("field_types.{field} holds non-canonical type {spelling:?}").into(),
                    )
                })?;
                Ok((field, ty))
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()
            .map_err(StoreError::from)
    }

    pub async fn pin_missing(
        &self,
        proposals: &[PinProposal],
    ) -> Result<HashMap<String, CanonicalType>, StoreError> {
        self.pin_missing_with(proposals, Ration::HalfOfFree).await
    }

    pub async fn pin_missing_unrationed(
        &self,
        proposals: &[PinProposal],
    ) -> Result<HashMap<String, CanonicalType>, StoreError> {
        self.pin_missing_with(proposals, Ration::EveryFreeSlot)
            .await
    }

    async fn pin_missing_with(
        &self,
        proposals: &[PinProposal],
        ration: Ration,
    ) -> Result<HashMap<String, CanonicalType>, StoreError> {
        let rejected = unstorable_names(proposals.iter().map(|p| p.field.as_str()));
        if !rejected.is_empty() {
            warn_unstorable("pin", &rejected);
        }
        let proposals: Vec<&PinProposal> = proposals
            .iter()
            .filter(|p| trawl_core::schema::is_storable_field_name(&p.field))
            .collect();
        if proposals.is_empty() {
            return Ok(HashMap::new());
        }
        let fields: Vec<&str> = proposals.iter().map(|p| p.field.as_str()).collect();
        let types: Vec<&str> = proposals.iter().map(|p| p.ty.as_catalog()).collect();
        let sources: Vec<&str> = proposals.iter().map(|p| p.pinned_from.as_str()).collect();

        sqlx::query(
            "WITH candidate AS (
                 SELECT u.f, u.t, u.s, row_number() OVER (ORDER BY u.f) AS rn
                 FROM UNNEST($1::text[], $2::text[], $3::text[]) AS u(f, t, s)
                 WHERE NOT EXISTS (SELECT 1 FROM field_types x WHERE x.field = u.f)
             ),
             free AS (
                 SELECT GREATEST($4::bigint - (SELECT count(*) FROM field_types), 0) AS slots
             ),
             capacity AS (
                 -- Integer division rounds down, so `+ 1` keeps a lone free
                 -- slot grantable: the ration bounds a burst, it never
                 -- strands capacity.
                 SELECT CASE WHEN $5 THEN slots ELSE (slots + 1) / 2 END AS slots FROM free
             )
             INSERT INTO field_types (field, duckdb_type, pinned_from)
             SELECT c.f, c.t, c.s FROM candidate c, capacity
             WHERE c.rn <= capacity.slots
             ON CONFLICT (field) DO NOTHING",
        )
        .bind(&fields)
        .bind(&types)
        .bind(&sources)
        .bind(self.pin_cap)
        .bind(matches!(ration, Ration::EveryFreeSlot))
        .execute(&self.pool)
        .await?;

        let pinned_now: i64 = sqlx::query_scalar("SELECT count(*) FROM field_types")
            .fetch_one(&self.pool)
            .await?;
        set_fill_gauges(pinned_now, self.pin_cap);

        let rows = sqlx::query("SELECT field, duckdb_type FROM field_types WHERE field = ANY($1)")
            .bind(&fields)
            .fetch_all(&self.pool)
            .await?;
        let pins = rows
            .iter()
            .map(|row| {
                let field: String = row.try_get("field")?;
                let spelling: String = row.try_get("duckdb_type")?;
                let ty = CanonicalType::from_catalog(&spelling).ok_or_else(|| {
                    sqlx::Error::Decode(
                        format!("field_types.{field} holds non-canonical type {spelling:?}").into(),
                    )
                })?;
                Ok((field, ty))
            })
            .collect::<Result<HashMap<_, _>, sqlx::Error>>()
            .map_err(StoreError::from)?;

        // Whatever the insert could not seat comes back missing from the
        // authoritative re-read, the one place that sees the cap bite,
        // however the slots were lost (full catalog, or a racing batch that
        // took the last ones).
        let denied: Vec<String> = {
            let mut d: Vec<String> = fields
                .iter()
                .filter(|f| !pins.contains_key(**f))
                .map(|f| short_name(f))
                .collect();
            d.sort_unstable();
            d.dedup();
            d
        };
        if !denied.is_empty() {
            tracing::warn!(
                event_type = "catalog_pin_cap_reached",
                cap = self.pin_cap,
                fields = ?denied,
                "field catalog is full — field(s) left unpinned; their columns \
                 are not stored, values remain in _raw"
            );
            bump_rejected("cap", denied.len());
        }
        Ok(pins)
    }

    /// `row_count` is added to the stored value, so callers must pass the rows
    /// this batch wrote, never a whole-file total, which would re-count every
    /// earlier batch on every tick.
    pub async fn touch_services(
        &self,
        service: &str,
        fields: &[String],
        row_count: u64,
    ) -> Result<(), StoreError> {
        let rejected = unstorable_names(fields.iter().map(String::as_str));
        if !rejected.is_empty() {
            warn_unstorable("observe", &rejected);
        }
        let fields: Vec<&str> = fields
            .iter()
            .map(String::as_str)
            .filter(|f| trawl_core::schema::is_storable_field_name(f))
            .collect();
        if fields.is_empty() {
            return Ok(());
        }

        sqlx::query(
            "INSERT INTO field_services (field, service, row_count)
             SELECT f, $2, $3 FROM UNNEST($1::text[]) AS t(f)
             ON CONFLICT (field, service) DO UPDATE
             SET last_seen = now(),
                 row_count = field_services.row_count + EXCLUDED.row_count",
        )
        .bind(&fields)
        .bind(service)
        .bind(i64::try_from(row_count).unwrap_or(i64::MAX))
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Idempotent, because the pass re-runs on every boot until the corpus is
    /// proven conformant: `first_seen` only moves earlier, `last_seen` only
    /// later, and `row_count` takes the MAX rather than accumulating.
    ///
    /// Callers must pass at most one row per `(field, service)`: postgres
    /// refuses to let one `ON CONFLICT DO UPDATE` statement touch a row twice.
    pub async fn backfill_services(
        &self,
        observations: &[ServiceObservation],
    ) -> Result<(), StoreError> {
        let rejected = unstorable_names(observations.iter().map(|o| o.field.as_str()));
        if !rejected.is_empty() {
            warn_unstorable("backfill", &rejected);
        }
        let storable: Vec<&ServiceObservation> = observations
            .iter()
            .filter(|o| trawl_core::schema::is_storable_field_name(&o.field))
            .collect();

        // Chunked: the row count is (pinned fields x services a deployment
        // ships), which the pin cap bounds at five figures on one axis
        // alone: too many parameters' worth of arrays for one statement.
        for chunk in storable.chunks(BACKFILL_CHUNK) {
            let fields: Vec<&str> = chunk.iter().map(|o| o.field.as_str()).collect();
            let services: Vec<&str> = chunk.iter().map(|o| o.service.as_str()).collect();
            let first: Vec<DateTime<Utc>> = chunk.iter().map(|o| o.first_seen).collect();
            let last: Vec<DateTime<Utc>> = chunk.iter().map(|o| o.last_seen).collect();
            let rows: Vec<i64> = chunk.iter().map(|o| o.row_count).collect();

            sqlx::query(
                "INSERT INTO field_services (field, service, first_seen, last_seen, row_count)
                 SELECT * FROM UNNEST(
                     $1::text[], $2::text[], $3::timestamptz[], $4::timestamptz[], $5::bigint[])
                 ON CONFLICT (field, service) DO UPDATE
                 SET first_seen = LEAST(field_services.first_seen, EXCLUDED.first_seen),
                     last_seen  = GREATEST(field_services.last_seen, EXCLUDED.last_seen),
                     row_count  = GREATEST(field_services.row_count, EXCLUDED.row_count)",
            )
            .bind(&fields)
            .bind(&services)
            .bind(&first)
            .bind(&last)
            .bind(&rows)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    pub async fn record_conflicts(&self, conflicts: &[FieldConflict]) -> Result<(), StoreError> {
        if conflicts.is_empty() {
            return Ok(());
        }
        // Its caller bounds it from outside (compaction's bookkeeping
        // budget), so the transaction is one a drop cannot return to the
        // pool open.
        let mut tx = PooledTransaction::begin(self.pool.acquire().await?).await?;
        record_conflicts_in(&mut tx, conflicts, self.conflict_cap).await?;
        tx.commit().await?;
        Ok(())
    }

    /// The cursor predicate is deliberately written as `last_seen <= cursor AND
    /// (last_seen < cursor OR service > $3)` rather than the equivalent single
    /// `OR` chain, and `COALESCE`s the absent cursor to `infinity` rather than
    /// guarding it with `IS NULL`. Only this shape is sargable: the leading
    /// conjunct is a bound postgres can push into
    /// `field_services_field_last_seen_idx`, so the page starts at the cursor.
    /// Under the `OR` chain every page re-reads the field's entire history,
    /// which makes walking the pages quadratic.
    pub async fn field_services(
        &self,
        field: &str,
        after: Option<&ServiceCursor>,
        limit: i64,
    ) -> Result<(Vec<FieldServiceRow>, Option<ServiceCursor>), StoreError> {
        let limit = limit.max(1);
        let rows = sqlx::query(
            "SELECT service, first_seen, last_seen, row_count
             FROM field_services
             WHERE field = $1
               AND last_seen <= COALESCE($2::timestamptz, 'infinity')
               AND (last_seen < COALESCE($2::timestamptz, 'infinity')
                    OR service > $3)
             ORDER BY last_seen DESC, service
             LIMIT $4",
        )
        .bind(field)
        .bind(after.map(|c| c.last_seen))
        .bind(after.map_or("", |c| c.service.as_str()))
        .bind(limit.saturating_add(1))
        .fetch_all(&self.pool)
        .await?;

        let has_more = i64::try_from(rows.len()).unwrap_or(i64::MAX) > limit;
        let mut rows = rows
            .iter()
            .map(|row| {
                Ok(FieldServiceRow {
                    service: row.try_get("service")?,
                    first_seen: row.try_get("first_seen")?,
                    last_seen: row.try_get("last_seen")?,
                    row_count: row.try_get("row_count")?,
                })
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()
            .map_err(StoreError::from)?;
        rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));

        let next = has_more.then(|| {
            let last = rows.last().expect("a truncating page has a last row");
            ServiceCursor {
                last_seen: last.last_seen,
                service: last.service.clone(),
            }
        });
        Ok((rows, next))
    }

    /// The ack join hangs off the GROUPED rows, not the raw ones: joining after
    /// the aggregation keeps the page's key pushed into the primary key scan,
    /// and the join then runs over at most a page of rows.
    const CONFLICT_AGGREGATES_KEYED_SQL: &'static str = "\
        SELECT s.field, s.first_at, s.last_at, s.services, s.episodes,
               s.rows_nulled_total, a.evidence_through AS ack_evidence_through
        FROM (
            SELECT field, min(first_at) AS first_at, max(last_at) AS last_at,
                   count(*)::bigint                      AS services,
                   COALESCE(sum(episodes), 0)::bigint    AS episodes,
                   COALESCE(sum(rows_nulled_total), 0)::bigint AS rows_nulled_total
            FROM field_conflict_stats
            WHERE field = ANY($1)
            GROUP BY field
        ) s
        LEFT JOIN field_degraded_ack a ON a.field = s.field";

    const CONFLICT_AGGREGATES_ALL_SQL: &'static str = "\
        SELECT s.field, s.first_at, s.last_at, s.services, s.episodes,
               s.rows_nulled_total, a.evidence_through AS ack_evidence_through
        FROM (
            SELECT field, min(first_at) AS first_at, max(last_at) AS last_at,
                   count(*)::bigint                      AS services,
                   COALESCE(sum(episodes), 0)::bigint    AS episodes,
                   COALESCE(sum(rows_nulled_total), 0)::bigint AS rows_nulled_total
            FROM field_conflict_stats
            GROUP BY field
        ) s
        LEFT JOIN field_degraded_ack a ON a.field = s.field";

    /// Two SQL texts rather than one `($1 IS NULL OR field = ANY($1))` shape: a
    /// prepared statement switching to its generic plan cannot push the `IS
    /// NULL`-guarded `OR` into the primary key, so the page-keyed read would
    /// degrade into a full aggregate.
    pub async fn conflict_aggregates(
        &self,
        fields: Option<&[String]>,
    ) -> Result<Vec<ConflictAggregate>, StoreError> {
        let mut conn = self.pool.acquire().await?;
        Self::conflict_aggregates_tx(&mut conn, fields).await
    }

    pub async fn conflict_aggregates_tx(
        tx: &mut sqlx::PgConnection,
        fields: Option<&[String]>,
    ) -> Result<Vec<ConflictAggregate>, StoreError> {
        let rows = match fields {
            Some([]) => return Ok(Vec::new()),
            Some(names) => {
                sqlx::query(Self::CONFLICT_AGGREGATES_KEYED_SQL)
                    .bind(names)
                    .fetch_all(&mut *tx)
                    .await?
            }
            None => {
                sqlx::query(Self::CONFLICT_AGGREGATES_ALL_SQL)
                    .fetch_all(&mut *tx)
                    .await?
            }
        };
        rows.iter()
            .map(|row| {
                Ok(ConflictAggregate {
                    field: row.try_get("field")?,
                    first_at: row.try_get("first_at")?,
                    last_at: row.try_get("last_at")?,
                    services: row.try_get("services")?,
                    episodes: row.try_get("episodes")?,
                    rows_nulled_total: row.try_get("rows_nulled_total")?,
                    ack_evidence_through: row.try_get("ack_evidence_through")?,
                })
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()
            .map_err(StoreError::from)
    }

    const CONFLICT_SERVICE_PAIRS_SQL: &'static str = "\
        SELECT field, service
        FROM field_conflict_stats
        WHERE field = ANY($1) AND service = ANY($2)
        ORDER BY field, service";

    pub async fn conflict_service_pairs(
        &self,
        fields: &[String],
        visible_services: &[String],
    ) -> Result<Vec<ConflictServicePair>, StoreError> {
        let mut conn = self.pool.acquire().await?;
        Self::conflict_service_pairs_tx(&mut conn, fields, visible_services).await
    }

    pub async fn conflict_service_pairs_tx(
        tx: &mut sqlx::PgConnection,
        fields: &[String],
        visible_services: &[String],
    ) -> Result<Vec<ConflictServicePair>, StoreError> {
        if fields.is_empty() || visible_services.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(Self::CONFLICT_SERVICE_PAIRS_SQL)
            .bind(fields)
            .bind(visible_services)
            .fetch_all(&mut *tx)
            .await?;
        rows.iter()
            .map(|row| {
                Ok(ConflictServicePair {
                    field: row.try_get("field")?,
                    service: row.try_get("service")?,
                })
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()
            .map_err(StoreError::from)
    }

    pub async fn begin_evidence_snapshot(
        &self,
    ) -> Result<sqlx::Transaction<'_, sqlx::Postgres>, StoreError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await?;
        Ok(tx)
    }

    /// A repin's evidence clear ([`Self::clear_conflict_evidence`]) deletes
    /// from the table both halves read. Run under two snapshots, a clear
    /// landing between them publishes a generation with a degraded field and no
    /// services attributed to it, so both reads share one `REPEATABLE READ`
    /// snapshot.
    pub async fn degraded_snapshot(
        &self,
        visible_services: &[String],
    ) -> Result<(BTreeSet<String>, Vec<ConflictServicePair>), StoreError> {
        if !self
            .conflict_aggregates(None)
            .await?
            .iter()
            .any(is_degraded)
        {
            return Ok((BTreeSet::new(), Vec::new()));
        }

        let mut tx = self.begin_evidence_snapshot().await?;
        // Re-read rather than reuse the probe: the probe's rows come from a
        // snapshot this transaction does not share, so trusting them would
        // reintroduce the tear one statement earlier.
        let degraded: BTreeSet<String> = Self::conflict_aggregates_tx(&mut tx, None)
            .await?
            .iter()
            .filter(|agg| is_degraded(agg))
            .map(|agg| agg.field.clone())
            .collect();
        let names: Vec<String> = degraded.iter().cloned().collect();
        let pairs = Self::conflict_service_pairs_tx(&mut tx, &names, visible_services).await?;
        tx.commit().await?;
        Ok((degraded, pairs))
    }

    /// Three statements under one `REPEATABLE READ` snapshot, for the same
    /// reason [`Self::degraded_snapshot`] takes one.
    pub async fn field_health_snapshot(
        &self,
        field: &str,
    ) -> Result<FieldHealthSnapshot, StoreError> {
        let mut tx = self.begin_evidence_snapshot().await?;
        let names = [field.to_owned()];
        let aggregate = Self::conflict_aggregates_tx(&mut tx, Some(&names))
            .await?
            .into_iter()
            .next();
        let evidence = if aggregate.is_some() {
            Self::conflict_evidence_for_tx(&mut tx, &names)
                .await?
                .remove(field)
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let ack = Self::degraded_ack_tx(&mut tx, field).await?;
        tx.commit().await?;
        Ok(FieldHealthSnapshot {
            aggregate,
            evidence,
            ack,
        })
    }

    /// Bounded per field by [`VERDICT_EVIDENCE_ROWS`] through a LATERAL
    /// subquery, not by an outer LIMIT: a page-wide limit would spend the whole
    /// budget on the first field and leave the rest verdictless.
    pub async fn conflict_evidence_for(
        &self,
        fields: &[String],
    ) -> Result<HashMap<String, Vec<(String, Vec<String>)>>, StoreError> {
        let mut conn = self.pool.acquire().await?;
        Self::conflict_evidence_for_tx(&mut conn, fields).await
    }

    async fn conflict_evidence_for_tx(
        tx: &mut sqlx::PgConnection,
        fields: &[String],
    ) -> Result<HashMap<String, Vec<(String, Vec<String>)>>, StoreError> {
        if fields.is_empty() {
            return Ok(HashMap::new());
        }
        let rows = sqlx::query(
            "SELECT f.field AS field, c.observed_type, c.samples
             FROM UNNEST($1::text[]) AS f(field)
             CROSS JOIN LATERAL (
                 SELECT observed_type, samples
                 FROM field_conflicts fc
                 WHERE fc.field = f.field
                 ORDER BY fc.at DESC, fc.id DESC
                 LIMIT $2
             ) c",
        )
        .bind(fields)
        .bind(VERDICT_EVIDENCE_ROWS)
        .fetch_all(&mut *tx)
        .await?;

        let mut out: HashMap<String, Vec<(String, Vec<String>)>> = HashMap::new();
        for row in &rows {
            let field: String = row.try_get("field")?;
            out.entry(field)
                .or_default()
                .push((row.try_get("observed_type")?, row.try_get("samples")?));
        }
        Ok(out)
    }

    /// Ordering is the cutover's, not ours: the clear runs first and the forced
    /// lossy repin's own evidence is written after it, in that same
    /// transaction, so the new evidence is never swept away with the old.
    pub async fn clear_conflict_evidence(
        tx: &mut sqlx::PgConnection,
        field: &str,
    ) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM field_conflict_stats WHERE field = $1")
            .bind(field)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM field_conflicts WHERE field = $1")
            .bind(field)
            .execute(&mut *tx)
            .await?;
        Ok(())
    }

    /// The whole method is one transaction, and it opens with `SELECT 1 FROM
    /// field_types WHERE field = $1 FOR SHARE`. That row lock is the
    /// serialization point against the repin cutover, whose transaction UPDATEs
    /// the same `field_types` row and DELETEs the ack. `FOR SHARE` is enough
    /// because `RepinStore::finish_cutover` opens by taking `FOR UPDATE` on
    /// that same row unconditionally.
    ///
    /// The upsert ADVANCES: `evidence_through` never moves backwards. The
    /// actor, the note and the timestamp are replaced only when the incoming
    /// high-water is at least the stored one.
    pub async fn acknowledge_degraded_field(
        &self,
        field: &str,
        acked_by: &str,
        note: Option<&str>,
    ) -> Result<AckOutcome, StoreError> {
        let mut tx = self.pool.begin().await?;

        let pinned = sqlx::query("SELECT 1 FROM field_types WHERE field = $1 FOR SHARE")
            .bind(field)
            .fetch_optional(&mut *tx)
            .await?;
        if pinned.is_none() {
            tx.rollback().await?;
            return Ok(AckOutcome::Unpinned);
        }

        let aggregates = Self::conflict_aggregates_tx(&mut tx, Some(&[field.to_owned()])).await?;
        let Some(agg) = aggregates.into_iter().find(|a| a.field == field) else {
            tx.rollback().await?;
            return Ok(AckOutcome::NotDegraded);
        };
        // The RAW rule, not the suppressed one: a field this operator has
        // already acknowledged still meets the threshold, and re-acking it
        // is how the high-water advances over evidence that re-raised the
        // badge. Reading `is_degraded` here would refuse exactly the ack
        // that answers a re-raise.
        if !meets_degraded_threshold(&agg) {
            tx.rollback().await?;
            return Ok(AckOutcome::NotDegraded);
        }

        // `xmax = 0` on a RETURNING row of an upsert is postgres' own tell
        // for "this tuple was inserted, not updated": an updated tuple
        // carries the deleting transaction's id in xmax, an inserted one
        // carries zero.
        //
        // The three metadata columns move together under one condition, as
        // CASE expressions rather than a WHERE on the DO UPDATE: a WHERE'd
        // upsert that skips the update returns NO row, and the caller would
        // lose both the stored ack and the ability to say whether it was
        // this call that wrote it.
        let row = sqlx::query(
            "INSERT INTO field_degraded_ack (field, acked_by, note, evidence_through)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (field) DO UPDATE
                SET acked_by         = CASE WHEN EXCLUDED.evidence_through
                                                 >= field_degraded_ack.evidence_through
                                            THEN EXCLUDED.acked_by
                                            ELSE field_degraded_ack.acked_by END,
                    note             = CASE WHEN EXCLUDED.evidence_through
                                                 >= field_degraded_ack.evidence_through
                                            THEN EXCLUDED.note
                                            ELSE field_degraded_ack.note END,
                    acked_at         = CASE WHEN EXCLUDED.evidence_through
                                                 >= field_degraded_ack.evidence_through
                                            THEN now()
                                            ELSE field_degraded_ack.acked_at END,
                    evidence_through = GREATEST(field_degraded_ack.evidence_through,
                                                EXCLUDED.evidence_through)
             RETURNING field, acked_at, acked_by, note, evidence_through, (xmax = 0) AS created",
        )
        .bind(field)
        .bind(acked_by)
        .bind(note)
        .bind(agg.episodes)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| match classify_violation(&e) {
            Some(PgViolation::Check) => {
                StoreError::Validation("acknowledgement note is too long".to_owned())
            }
            _ => StoreError::from(e),
        })?;
        let ack = DegradedAck {
            field: row.try_get("field")?,
            acked_at: row.try_get("acked_at")?,
            acked_by: row.try_get("acked_by")?,
            note: row.try_get("note")?,
            evidence_through: row.try_get("evidence_through")?,
        };
        // The stored high-water is GREATEST(previous, ours), so it exceeds
        // ours exactly when a concurrent ack had already covered more
        // evidence and the CASE arms above kept its metadata. RETURNING
        // cannot read the pre-update row, and it does not need to.
        let advanced = ack.evidence_through <= agg.episodes;
        let outcome = AckOutcome::Acked {
            ack,
            created: row.try_get("created")?,
            advanced,
        };
        tx.commit().await?;
        Ok(outcome)
    }

    pub async fn degraded_ack(&self, field: &str) -> Result<Option<DegradedAck>, StoreError> {
        let mut conn = self.pool.acquire().await?;
        Self::degraded_ack_tx(&mut conn, field).await
    }

    async fn degraded_ack_tx(
        tx: &mut sqlx::PgConnection,
        field: &str,
    ) -> Result<Option<DegradedAck>, StoreError> {
        let row = sqlx::query(
            "SELECT field, acked_at, acked_by, note, evidence_through
             FROM field_degraded_ack WHERE field = $1",
        )
        .bind(field)
        .fetch_optional(&mut *tx)
        .await?;
        row.map(|row| -> Result<DegradedAck, sqlx::Error> {
            Ok(DegradedAck {
                field: row.try_get("field")?,
                acked_at: row.try_get("acked_at")?,
                acked_by: row.try_get("acked_by")?,
                note: row.try_get("note")?,
                evidence_through: row.try_get("evidence_through")?,
            })
        })
        .transpose()
        .map_err(StoreError::from)
    }

    pub async fn clear_degraded_ack(
        tx: &mut sqlx::PgConnection,
        field: &str,
    ) -> Result<bool, StoreError> {
        let done = sqlx::query("DELETE FROM field_degraded_ack WHERE field = $1")
            .bind(field)
            .execute(&mut *tx)
            .await?;
        Ok(done.rows_affected() > 0)
    }

    pub async fn clear_degraded_ack_owned(&self, field: &str) -> Result<bool, StoreError> {
        let mut conn = self.pool.acquire().await?;
        Self::clear_degraded_ack(&mut conn, field).await
    }

    pub async fn conflicts_for_field(
        &self,
        field: &str,
    ) -> Result<Vec<FieldConflictRow>, StoreError> {
        let rows = sqlx::query(
            "SELECT service, observed_type, expected_type, rows_nulled, samples, at
             FROM field_conflicts WHERE field = $1
             ORDER BY at DESC, id DESC",
        )
        .bind(field)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(FieldConflictRow {
                    service: row.try_get("service")?,
                    observed_type: row.try_get("observed_type")?,
                    expected_type: row.try_get("expected_type")?,
                    rows_nulled: row.try_get("rows_nulled")?,
                    samples: row.try_get("samples")?,
                    at: row.try_get("at")?,
                })
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()
            .map_err(StoreError::from)
    }

    /// [`Self::list_fields`], `?service=` shape: `$1` service, `$2` since,
    /// `$3` limit. See the method docs for why this is its own SQL text.
    const LIST_FIELDS_SCOPED_SQL: &'static str = "\
        SELECT t.field, t.duckdb_type, t.pinned_from, t.pinned_at,
               COALESCE(s.service_count, 0)::bigint AS service_count,
               COALESCE(s.row_count, 0)::bigint     AS row_count,
               s.first_seen, s.last_seen
        FROM field_types t
        LEFT JOIN (
            SELECT field, count(*) AS service_count, sum(row_count) AS row_count,
                   min(first_seen) AS first_seen, max(last_seen) AS last_seen
            FROM field_services
            WHERE service = $1
            GROUP BY field
        ) s ON s.field = t.field
        WHERE EXISTS (
                  SELECT 1 FROM field_services fs
                  WHERE fs.field = t.field AND fs.service = $1)
          AND ($2::timestamptz IS NULL
                   OR s.last_seen IS NULL
                   OR s.last_seen >= $2)
        ORDER BY t.field
        LIMIT $3";

    /// [`Self::list_fields`], unscoped shape: `$1` since, `$2` limit.
    const LIST_FIELDS_UNSCOPED_SQL: &'static str = "\
        SELECT t.field, t.duckdb_type, t.pinned_from, t.pinned_at,
               COALESCE(s.service_count, 0)::bigint AS service_count,
               COALESCE(s.row_count, 0)::bigint     AS row_count,
               s.first_seen, s.last_seen
        FROM field_types t
        LEFT JOIN (
            SELECT field, count(*) AS service_count, sum(row_count) AS row_count,
                   min(first_seen) AS first_seen, max(last_seen) AS last_seen
            FROM field_services
            GROUP BY field
        ) s ON s.field = t.field
        WHERE ($1::timestamptz IS NULL
                   OR s.last_seen IS NULL
                   OR s.last_seen >= $1)
        ORDER BY t.field
        LIMIT $2";

    /// The LEFT join is load-bearing: a pin with no observations (the envelope
    /// seed on a fresh install, or a boot-pass pin over standing parquet) must
    /// always appear.
    ///
    /// `filter.service` scopes the aggregate, not just the row set: the
    /// predicate goes inside the `field_services` grouping. Filtering only in
    /// the `WHERE` clause would leak every other service's numbers into the
    /// listing and keep a field alive in the window because somebody else still
    /// sends it. The `EXISTS` stays as the presence test: a pin the service
    /// never carried has no group row, and the never-observed rule would
    /// otherwise show it.
    ///
    /// The conflict evidence is a second query, keyed on the field names this
    /// page actually returned: a joined `GROUP BY field` would carry no
    /// predicate a planner can push down, so every call would materialise an
    /// aggregate over the whole table.
    ///
    /// Scoped and unscoped are two SQL texts, not one `($1 IS NULL OR service =
    /// $1)` shape: sqlx prepares and caches every statement per pooled
    /// connection, and on execution 6 postgres (`plan_cache_mode=auto`)
    /// switches a prepared statement to its generic plan, under which the `IS
    /// NULL`-guarded `OR` cannot be pushed into
    /// `field_services_service_field_idx` as a scan key, so the scoped listing
    /// degrades to reading the whole table.
    pub async fn list_fields(
        &self,
        filter: &FieldListFilter,
    ) -> Result<(Vec<FieldSummaryRow>, bool), StoreError> {
        let limit = filter.limit.max(0);
        let query = if let Some(service) = filter.service.as_deref() {
            sqlx::query(Self::LIST_FIELDS_SCOPED_SQL).bind(service)
        } else {
            sqlx::query(Self::LIST_FIELDS_UNSCOPED_SQL)
        };
        let rows = query
            .bind(filter.since)
            .bind(limit.saturating_add(1))
            .fetch_all(&self.pool)
            .await?;

        let truncated = i64::try_from(rows.len()).unwrap_or(i64::MAX) > limit;
        let take = usize::try_from(limit).unwrap_or(usize::MAX);
        let mut summaries = rows
            .iter()
            .take(take)
            .map(|row| {
                Ok(FieldSummaryRow {
                    field: row.try_get("field")?,
                    duckdb_type: row.try_get("duckdb_type")?,
                    pinned_from: row.try_get("pinned_from")?,
                    pinned_at: row.try_get("pinned_at")?,
                    service_count: row.try_get("service_count")?,
                    row_count: row.try_get("row_count")?,
                    first_seen: row.try_get("first_seen")?,
                    last_seen: row.try_get("last_seen")?,
                    conflict_count: 0,
                    rows_nulled: 0,
                })
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()
            .map_err(StoreError::from)?;

        if filter.with_conflicts && !summaries.is_empty() {
            let names: Vec<String> = summaries.iter().map(|r| r.field.clone()).collect();
            let evidence = sqlx::query(
                "SELECT field,
                        count(*)::bigint                    AS conflict_count,
                        COALESCE(sum(rows_nulled), 0)::bigint AS rows_nulled
                 FROM field_conflicts
                 WHERE field = ANY($1)
                 GROUP BY field",
            )
            .bind(&names)
            .fetch_all(&self.pool)
            .await?;
            let by_field: HashMap<String, (i64, i64)> = evidence
                .iter()
                .map(|row| {
                    Ok((
                        row.try_get("field")?,
                        (row.try_get("conflict_count")?, row.try_get("rows_nulled")?),
                    ))
                })
                .collect::<Result<_, sqlx::Error>>()
                .map_err(StoreError::from)?;
            for summary in &mut summaries {
                if let Some(&(conflict_count, rows_nulled)) = by_field.get(&summary.field) {
                    summary.conflict_count = conflict_count;
                    summary.rows_nulled = rows_nulled;
                }
            }
        }

        Ok((summaries, truncated))
    }

    pub async fn pin_stats(&self) -> Result<(i64, i64), StoreError> {
        let pinned: i64 = sqlx::query_scalar("SELECT count(*) FROM field_types")
            .fetch_one(&self.pool)
            .await?;
        Ok((pinned, self.pin_cap))
    }

    pub async fn recent_conflicts(
        &self,
        field: Option<&str>,
        service: Option<&str>,
        since: Option<DateTime<Utc>>,
        limit: i64,
    ) -> Result<(Vec<ConflictListRow>, bool), StoreError> {
        let limit = limit.max(0);
        let rows = sqlx::query(
            "SELECT field, service, observed_type, expected_type, rows_nulled, samples, at
             FROM field_conflicts
             WHERE ($1::text IS NULL OR field = $1)
               AND ($2::text IS NULL OR service = $2)
               AND ($3::timestamptz IS NULL OR at >= $3)
             ORDER BY at DESC, id DESC
             LIMIT $4",
        )
        .bind(field)
        .bind(service)
        .bind(since)
        .bind(limit.saturating_add(1))
        .fetch_all(&self.pool)
        .await?;

        let truncated = i64::try_from(rows.len()).unwrap_or(i64::MAX) > limit;
        let take = usize::try_from(limit).unwrap_or(usize::MAX);
        rows.iter()
            .take(take)
            .map(|row| {
                Ok(ConflictListRow {
                    field: row.try_get("field")?,
                    service: row.try_get("service")?,
                    observed_type: row.try_get("observed_type")?,
                    expected_type: row.try_get("expected_type")?,
                    rows_nulled: row.try_get("rows_nulled")?,
                    samples: row.try_get("samples")?,
                    at: row.try_get("at")?,
                })
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()
            .map(|rows| (rows, truncated))
            .map_err(StoreError::from)
    }

    pub async fn field_pin(&self, field: &str) -> Result<Option<FieldPinRow>, StoreError> {
        let row = sqlx::query(
            "SELECT field, duckdb_type, pinned_from, pinned_at
             FROM field_types WHERE field = $1",
        )
        .bind(field)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| -> Result<FieldPinRow, sqlx::Error> {
            Ok(FieldPinRow {
                field: row.try_get("field")?,
                duckdb_type: row.try_get("duckdb_type")?,
                pinned_from: row.try_get("pinned_from")?,
                pinned_at: row.try_get("pinned_at")?,
            })
        })
        .transpose()
        .map_err(StoreError::from)
    }

    /// Never-observed pins are included, a deliberate divergence from
    /// [`Self::list_fields`]: the listing errs toward showing; gc errs toward
    /// reclaiming.
    ///
    /// The newest observation is a per-pin LATERAL `ORDER BY last_seen DESC
    /// LIMIT 1`, an index-only lookup of one row. Deliberately not the `GROUP
    /// BY field` aggregate [`Self::list_fields`] uses: that one reads every
    /// observation row in the table, and the service axis is client-chosen and
    /// never pruned.
    ///
    /// The MATERIALIZED CTE fences the service count so it runs per candidate
    /// rather than per pin.
    pub async fn pins_unobserved_since(
        &self,
        cutoff: DateTime<Utc>,
    ) -> Result<Vec<GcPinRow>, StoreError> {
        let rows = sqlx::query(
            "WITH candidate AS MATERIALIZED (
                 SELECT t.field, t.duckdb_type, t.pinned_at, n.last_seen
                 FROM field_types t
                 LEFT JOIN LATERAL (
                     SELECT fs.last_seen
                     FROM field_services fs
                     WHERE fs.field = t.field
                     ORDER BY fs.last_seen DESC
                     LIMIT 1
                 ) n ON TRUE
                 WHERE n.last_seen IS NULL OR n.last_seen < $1
             )
             SELECT c.field, c.duckdb_type, c.pinned_at, c.last_seen,
                    (SELECT count(*) FROM field_services fs
                     WHERE fs.field = c.field)::bigint AS services
             FROM candidate c
             ORDER BY c.field",
        )
        .bind(cutoff)
        .fetch_all(&self.pool)
        .await?;

        rows.iter()
            .map(|row| {
                Ok(GcPinRow {
                    field: row.try_get("field")?,
                    duckdb_type: row.try_get("duckdb_type")?,
                    pinned_at: row.try_get("pinned_at")?,
                    last_seen: row.try_get("last_seen")?,
                    services: row.try_get("services")?,
                })
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()
            .map_err(StoreError::from)
    }

    /// A [`trawl_core::schema::is_contract_typed`] name is an error, not a
    /// skipped element: the caller filters those out before it ever gets here,
    /// so one arriving means the candidate set was built wrong.
    pub async fn delete_pins(&self, fields: &[String]) -> Result<PurgedPins, StoreError> {
        if fields.is_empty() {
            return Ok(PurgedPins::default());
        }
        if let Some(contract) = fields
            .iter()
            .find(|f| trawl_core::schema::is_contract_typed(f))
        {
            return Err(StoreError::Validation(format!(
                "{contract} is one of trawl's contract fields — its pin is \
                 declared, not reclaimable"
            )));
        }

        // Phase one, cancellable: cancelling it can only roll back.
        let (tx, deleted, pinned_now) =
            match tokio::time::timeout(PURGE_PREPARE_BOUND, self.prepare_purge(fields)).await {
                Ok(prepared) => prepared?,
                Err(_elapsed) => {
                    tracing::warn!(
                        target: "storage.backend",
                        event_type = "catalog_pin_purge_prepare_timeout",
                        fields = fields.len(),
                        after_secs = PURGE_PREPARE_BOUND.as_secs(),
                        "a field-catalog pin purge gave up on the database before \
                         committing; the transaction was dropped and rolled back"
                    );
                    return Err(StoreError::PurgePrepareTimeout);
                }
            };
        // Phase two, never cancellable: detached instead.
        commit_or_detach(tx, fields.len()).await?;

        Ok(PurgedPins {
            deleted,
            pinned_now,
        })
    }

    async fn prepare_purge(
        &self,
        fields: &[String],
    ) -> Result<(PooledTransaction, Vec<PurgedPin>, i64), StoreError> {
        let mut tx = PooledTransaction::begin(self.pool.acquire().await?).await?;
        // Bound the pre-commit work database-side, before anything that can
        // wait.
        for sql in [PURGE_STATEMENT_TIMEOUT_SQL, PURGE_LOCK_TIMEOUT_SQL] {
            sqlx::query(sql).execute(&mut *tx).await?;
        }
        // The lock, then the re-check, then the delete: this transaction is
        // the authority on "no repin owns a pin I am about to reclaim", and
        // the caller's earlier checks are courtesy fast-paths. A repin claim
        // takes the same lock (`RepinStore::claim`), so a claim that lands
        // after gc's last look is either still waiting here or already
        // visible to the SELECT below.
        super::lock_catalog_lifecycle(&mut tx).await?;
        let running: Option<i64> =
            sqlx::query_scalar("SELECT id FROM repin_jobs WHERE status = 'running' LIMIT 1")
                .fetch_optional(&mut *tx)
                .await?;
        if running.is_some() {
            return Err(StoreError::RepinAlreadyRunning);
        }
        let observed: HashMap<String, (Option<DateTime<Utc>>, i64)> = sqlx::query(
            "SELECT field, max(last_seen) AS last_seen, count(*)::bigint AS services
             FROM field_services WHERE field = ANY($1) GROUP BY field",
        )
        .bind(fields)
        .fetch_all(&mut *tx)
        .await?
        .iter()
        .map(|row| {
            Ok((
                row.try_get("field")?,
                (row.try_get("last_seen")?, row.try_get("services")?),
            ))
        })
        .collect::<Result<_, sqlx::Error>>()?;

        // `field_degraded_ack` is listed rather than left to its FK cascade:
        // every other table the purge empties is named in this list, and
        // re-pinning the same name later must not resurrect a judgement made
        // about the pin that is gone.
        for sql in [
            "DELETE FROM field_degraded_ack WHERE field = ANY($1)",
            "DELETE FROM field_conflict_stats WHERE field = ANY($1)",
            "DELETE FROM field_conflicts WHERE field = ANY($1)",
            "DELETE FROM field_services WHERE field = ANY($1)",
        ] {
            sqlx::query(sql).bind(fields).execute(&mut *tx).await?;
        }
        // RETURNING, not `rows_affected`: the caller audits one event per
        // reclaimed pin, and a name it merely ASKED for is not a name it
        // deleted. A candidate can lose its row to a concurrent write
        // between the scan and this statement, and a repin that finished in
        // the same window leaves a type the caller's snapshot no longer
        // knows — so the type comes back from here too.
        let deleted_rows = sqlx::query(
            "DELETE FROM field_types WHERE field = ANY($1)
             RETURNING field, duckdb_type, pinned_from, pinned_at",
        )
        .bind(fields)
        .fetch_all(&mut *tx)
        .await?;
        let mut deleted = deleted_rows
            .iter()
            .map(|row| {
                let field: String = row.try_get("field")?;
                let (last_seen, services) = observed.get(&field).copied().unwrap_or((None, 0));
                Ok(PurgedPin {
                    field,
                    duckdb_type: row.try_get("duckdb_type")?,
                    pinned_from: row.try_get("pinned_from")?,
                    pinned_at: row.try_get("pinned_at")?,
                    last_seen,
                    services,
                })
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()?;
        // DELETE returns rows in no defined order; the report is field-sorted.
        deleted.sort_by(|a, b| a.field.cmp(&b.field));
        let pinned_now: i64 = sqlx::query_scalar("SELECT count(*) FROM field_types")
            .fetch_one(&mut *tx)
            .await?;

        Ok((tx, deleted, pinned_now))
    }

    /// The reconcile read behind a purge that failed BEFORE its commit was
    /// sent: those errors prove a rollback, and this read settles the pin
    /// cache against the surviving rows. A commit-phase failure never
    /// reaches this read (the outcome is unknown and the engine over-evicts
    /// instead, because this read could race a still-landing commit).
    pub async fn pins_present(&self, fields: &[String]) -> Result<BTreeSet<String>, StoreError> {
        if fields.is_empty() {
            return Ok(BTreeSet::new());
        }
        let present: Vec<String> =
            sqlx::query_scalar("SELECT field FROM field_types WHERE field = ANY($1)")
                .bind(fields)
                .fetch_all(&self.pool)
                .await?;
        Ok(present.into_iter().collect())
    }

    /// Publish the pin fill gauges from a count the caller already holds.
    ///
    /// Infallible and cheap, so it can run at a point where a failure would
    /// be unrecoverable — after [`Self::delete_pins`] has committed and the
    /// in-process cache has dropped the pins.
    pub fn publish_fill_gauges(&self, pinned: i64) {
        set_fill_gauges(pinned, self.pin_cap);
    }

    /// The catalog's stable identity (mirrored into the `data/CATALOG`
    /// marker so `DATABASE_URL` repoints and data-root restores are
    /// self-detecting).
    pub async fn catalog_id(&self) -> Result<String, StoreError> {
        Ok(read_catalog_id(&self.pool).await?)
    }

    pub async fn is_conformed(&self) -> Result<bool, StoreError> {
        Ok(read_conformed(&self.pool).await?)
    }

    pub async fn mark_conformed(&self) -> Result<(), StoreError> {
        sqlx::query("UPDATE catalog_state SET conformed_at = now()")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Re-arm the boot conformance pass: a repin cutover recovered at boot
    /// clears this so `ensure_conformance` re-proves the corpus against the
    /// flipped pin in the same boot, the backstop for any file an
    /// interrupted repin missed.
    pub async fn clear_conformed(&self) -> Result<(), StoreError> {
        sqlx::query("UPDATE catalog_state SET conformed_at = NULL")
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

pub async fn read_catalog_id<'e>(conn: impl sqlx::PgExecutor<'e>) -> Result<String, sqlx::Error> {
    sqlx::query_scalar("SELECT catalog_id::text FROM catalog_state")
        .fetch_one(conn)
        .await
}

pub async fn read_conformed<'e>(conn: impl sqlx::PgExecutor<'e>) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT conformed_at IS NOT NULL FROM catalog_state")
        .fetch_one(conn)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_cursor_roundtrips_exactly() {
        for service in ["nginx", "api.v2", "svc-a_b", "x"] {
            let cursor = ServiceCursor {
                last_seen: DateTime::parse_from_rfc3339("2026-08-02T10:00:00.123456Z")
                    .unwrap()
                    .with_timezone(&Utc),
                service: service.to_owned(),
            };
            let decoded = ServiceCursor::decode(&cursor.encode()).expect("decodes");
            assert_eq!(decoded, cursor, "{service}");
        }
    }

    #[test]
    fn the_purge_sets_its_bounds_before_it_takes_the_lock() {
        let src = include_str!("catalog.rs");
        let start = src
            .find("    async fn prepare_purge(")
            .expect("the pre-commit phase is one method");
        let body = &src[start..];
        let end = body.find("\n    }\n").expect("the method closes");
        let body = &body[..end];

        let bounds = body
            .find("for sql in [PURGE_STATEMENT_TIMEOUT_SQL, PURGE_LOCK_TIMEOUT_SQL]")
            .expect("the transaction sets both bounds, statement_timeout first");
        let begin = body
            .find("PooledTransaction::begin(")
            .expect("the transaction opens");
        let lock = body
            .find("lock_catalog_lifecycle")
            .expect("the transaction takes the catalog lifecycle lock");
        assert!(
            begin < bounds && bounds < lock,
            "the bounds belong between BEGIN and the advisory lock"
        );
        assert!(
            PURGE_STATEMENT_TIMEOUT_SQL.starts_with("SET LOCAL statement_timeout")
                && PURGE_LOCK_TIMEOUT_SQL.starts_with("SET LOCAL lock_timeout"),
            "both bounds are transaction-local"
        );
    }

    #[test]
    fn the_purge_detaches_its_commit_instead_of_cancelling_it() {
        let src = include_str!("catalog.rs");
        let start = src
            .find("async fn commit_or_detach(")
            .expect("the commit has one owner");
        let body = &src[start..];
        let end = body.find("\n}\n").expect("the function closes");
        let body = &body[..end];

        assert!(
            src.contains("commit_or_detach(tx, fields.len()).await?;"),
            "delete_pins commits through that owner"
        );
        let select = body
            .find("tokio::select! {")
            .expect("the commit is raced against a timer, not wrapped in a timeout");
        assert!(
            !body.contains("tokio::time::timeout"),
            "a timeout around the commit would CANCEL it; the commit must be detached"
        );
        let biased = body[select..]
            .find("biased;")
            .expect("the select polls the commit first");
        let timer = body[select..]
            .find("tokio::time::sleep(PURGE_COMMIT_BOUND)")
            .expect("the other arm is the bound");
        assert!(biased < timer, "`biased` must precede the arms it orders");

        let elapsed = &body[select + timer..];
        let spawn = elapsed
            .find("tokio::spawn(")
            .expect("the elapsed arm detaches the still-boxed commit future");
        let returns = elapsed
            .find("Err(StoreError::PurgeCommitUnknown)")
            .expect("the elapsed arm reports an unknown outcome");
        assert!(
            spawn < returns,
            "the commit must be detached before the method walks away from it"
        );
        for logged in ["tracing::info!", "tracing::warn!"] {
            assert!(
                elapsed[spawn..returns].contains(logged),
                "the detached task logs both outcomes; missing {logged}"
            );
        }

        let completion = &body[select..select + timer];
        let failed = completion
            .find("Err(e) =>")
            .expect("the completion arm distinguishes a failed commit");
        assert!(
            completion[failed..].contains("Err(StoreError::PurgeCommitUnknown)"),
            "a commit that completes with an error is an unknown outcome"
        );
        assert!(
            !completion.contains("Ok(result?)") && !completion.contains("StoreError::Unavailable"),
            "the `?` shorthand would map a failed commit onto an ordinary \
             store error and send the caller to the reconcile read"
        );
        assert!(
            completion[failed..].contains("tracing::warn!"),
            "the pg diagnostics stay in the log; the caller gets the outcome word"
        );
    }

    #[test]
    fn the_purge_bounds_its_pre_commit_phase_and_only_that() {
        let src = include_str!("catalog.rs");
        let start = src
            .find("    pub async fn delete_pins(")
            .expect("delete_pins is one method");
        let body = &src[start..];
        let end = body.find("\n    }\n").expect("the method closes");
        let body = &body[..end];

        let bounded = body
            .find("tokio::time::timeout(PURGE_PREPARE_BOUND, self.prepare_purge(fields))")
            .expect("the pre-commit phase runs under the client-side bound");
        let commit = body
            .find("commit_or_detach(tx, fields.len()).await?;")
            .expect("the commit runs through its own owner");
        assert!(
            bounded < commit,
            "the bound belongs to the prepare phase, which must finish before \
             the commit begins"
        );
        assert_eq!(
            body.matches("tokio::time::timeout").count(),
            1,
            "exactly one cancelling bound, and it stops at the commit"
        );
        assert!(
            body.contains("return Err(StoreError::PurgePrepareTimeout);"),
            "an elapsed prepare is an ordinary bounded failure: nothing committed"
        );

        let prepare = src
            .find("    async fn prepare_purge(")
            .expect("the pre-commit phase is one method");
        let prepare = &src[prepare..];
        let prepare = &prepare[..prepare.find("\n    }\n").expect("the method closes")];
        assert!(
            !prepare.contains(".commit()") && !prepare.contains("commit_or_detach"),
            "the prepare phase must return the transaction, never commit it"
        );
        assert!(
            prepare.contains("PooledTransaction::begin(self.pool.acquire().await?)")
                && !prepare.contains(".begin()"),
            "the cancellable half opens a transaction whose drop closes its \
             connection: a `BEGIN` cut short must not go back to the pool open"
        );
    }

    #[test]
    fn service_cursor_rejects_garbage() {
        for raw in [
            "",
            "nginx",
            "|nginx",
            "2026-08-02T10:00:00Z|",
            "not-a-time|nginx",
            "2026-08-02T10:00:00Z",
        ] {
            assert!(ServiceCursor::decode(raw).is_none(), "{raw:?}");
        }
    }
}

#[cfg(test)]
mod pg_tests {
    use sqlx::PgPool;

    use super::{CatalogStore, FieldConflict, MAX_CONFLICTS_PER_FIELD, record_conflicts_in};
    use trawl_core::schema::CanonicalType;

    #[sqlx::test]
    async fn record_conflicts_in_commits_nothing_of_its_own(pool: PgPool) {
        let store = CatalogStore::new(pool.clone());
        let conflict = FieldConflict {
            field: "duration".to_owned(),
            service: "svc-a".to_owned(),
            observed_type: "VARCHAR".to_owned(),
            expected_type: CanonicalType::BigInt,
            rows_nulled: 3,
            samples: vec!["accepted".to_owned()],
        };

        let mut tx = pool.begin().await.unwrap();
        record_conflicts_in(
            &mut tx,
            std::slice::from_ref(&conflict),
            MAX_CONFLICTS_PER_FIELD,
        )
        .await
        .unwrap();
        let inside: i64 =
            sqlx::query_scalar("SELECT count(*) FROM field_conflicts WHERE field = 'duration'")
                .fetch_one(&mut *tx)
                .await
                .unwrap();
        assert_eq!(inside, 1, "the rows are there inside the transaction");
        tx.rollback().await.unwrap();

        assert!(
            store
                .conflicts_for_field("duration")
                .await
                .unwrap()
                .is_empty(),
            "a rolled-back transaction leaves no evidence rows"
        );
        assert!(
            store
                .conflict_aggregates(Some(&["duration".to_owned()]))
                .await
                .unwrap()
                .is_empty(),
            "nor any durable aggregate"
        );
    }
}
