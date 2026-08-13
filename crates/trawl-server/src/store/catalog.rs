// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Postgres-backed field catalog: type pins, per-service observations, and
//! conflict evidence (ADR-0009 slice 2).
//!
//! The catalog is the write-time type authority: compaction pins every
//! dynamic field's canonical type here BEFORE the first parquet file
//! carrying it is written, and conforms every batch to the pins — so
//! `union_by_name` across any set of trawl-written files can never
//! conflict. Pins are add-only on the ingest path (the one mutation is the
//! operator-triggered repin cutover, `store::repin`, ADR-0011 slice B) —
//! and because a pin slot is therefore permanent while its name is a client-chosen
//! JSON key, the catalog is bounded where a sender controls the axis: name
//! length by [`trawl_core::schema::is_storable_field_name`], pin count by
//! [`MAX_PINNED_FIELDS`], and per-field conflict evidence by
//! [`MAX_CONFLICTS_PER_FIELD`]. `field_services` rows are deliberately
//! ever-observed — nothing removes one — and its worst case is bounded by
//! the pin cap on the field axis times the services a deployment really
//! runs; consumers window on `last_seen`. Its SERVICE axis has no cap at
//! all (service names are client-chosen and no row is ever removed), so the
//! read surface pages it: [`CatalogStore::field_services`] takes a bounded
//! limit and a [`ServiceCursor`], never the whole history.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row as _};
use trawl_core::schema::CanonicalType;

use super::error::StoreError;
use crate::catalog::analyzer::ConflictAggregate;

/// A proposed pin for a field absent from the catalog.
#[derive(Debug, Clone)]
pub struct PinProposal {
    /// Field name (the global key).
    pub field: String,
    /// The type the proposing batch's pin algorithm derived.
    pub ty: CanonicalType,
    /// The service whose batch proposed the pin (audit trail).
    pub pinned_from: String,
}

/// One append-only conflict record: a batch column whose `TRY_CAST` to its
/// pin NULLED at least one value. A cast that nulls nothing is convergence,
/// not conflict, and is never recorded (see `ConformPlan::tally_conflicts`)
/// — an append-only row per lossless cast per compaction tick would grow
/// without bound. What a genuinely lossy sender appends is bounded instead
/// by [`MAX_CONFLICTS_PER_FIELD`].
#[derive(Debug, Clone)]
pub struct FieldConflict {
    /// Field name.
    pub field: String,
    /// The service whose batch disagreed with the pin.
    pub service: String,
    /// The `DuckDB` type the batch actually carried (free-form spelling —
    /// may be `JSON` or a complex type, not only the canonical five).
    pub observed_type: String,
    /// The pinned type the values were cast to.
    pub expected_type: CanonicalType,
    /// Rows whose value the cast nulled (recoverable from `_raw`).
    pub rows_nulled: u64,
    /// Up to [`MAX_CONFLICT_SAMPLES`] DISTINCT values the cast nulled, each
    /// sanitised and cut to [`MAX_CONFLICT_SAMPLE_BYTES`] at capture. A
    /// SAMPLE, never a manifest: the exhaustive record of what was shelved
    /// is `_raw`. Empty where the lane has no values in hand (the repin
    /// rewrite counts its own nulls, ADR-0011 slice B).
    pub samples: Vec<String>,
}

/// One `(field, service)` observation reconstructed from a standing parquet
/// file rather than reported by the batch that wrote it — the boot
/// conformance pass's backfill ([`CatalogStore::backfill_services`]).
#[derive(Debug, Clone)]
pub struct ServiceObservation {
    /// Field name (already ASCII-folded — a catalog key).
    pub field: String,
    /// Service that carries the field.
    pub service: String,
    /// Earliest instant the corpus attests to (the oldest partition
    /// directory carrying the column).
    pub first_seen: DateTime<Utc>,
    /// Latest instant the corpus attests to (the newest such partition).
    pub last_seen: DateTime<Utc>,
    /// Rows the corpus holds for this `(field, service)` pair.
    pub row_count: i64,
}

/// A `field_services` observation row.
#[derive(Debug, Clone)]
pub struct FieldServiceRow {
    /// Service name.
    pub service: String,
    /// First time compaction observed the field for this service.
    pub first_seen: DateTime<Utc>,
    /// Most recent observation (consumers window on this — rows are
    /// ever-observed: retention never reconciles them and nothing else
    /// removes one).
    pub last_seen: DateTime<Utc>,
    /// Cumulative rows compacted in batches that wrote the field — the sum
    /// of per-batch row counts, not a per-value non-null tally.
    pub row_count: i64,
}

/// A position inside one field's observation listing: the
/// `(last_seen, service)` of the last row already delivered.
///
/// `(last_seen, service)` is unique per field — `(field, service)` is the
/// primary key — so the pair identifies an exact row, and resuming after it
/// can neither repeat nor skip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceCursor {
    /// Last observation instant of the row the page stopped at.
    pub last_seen: DateTime<Utc>,
    /// Service name of the row the page stopped at.
    pub service: String,
}

impl ServiceCursor {
    /// Wire spelling: `<rfc3339-micros>|<service>`.
    ///
    /// `|` is outside the service charset (`[A-Za-z0-9._-]`, enforced at
    /// every ingest door), and RFC 3339 has no `|` either, so splitting at
    /// the LAST `|` recovers both halves unambiguously.
    #[must_use]
    pub fn encode(&self) -> String {
        format!(
            "{}|{}",
            self.last_seen
                .to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
            self.service
        )
    }

    /// Parse a cursor the server previously issued. `None` for anything
    /// malformed — a cursor is opaque to clients, so a garbled one is a
    /// client error, never a silently ignored filter.
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

/// Filter for [`CatalogStore::list_fields`].
#[derive(Debug, Clone)]
pub struct FieldListFilter {
    /// Only fields observed for this service — and every aggregate below
    /// (`service_count`, `row_count`, `first_seen`, `last_seen`, and the
    /// `since` window read off them) is computed from THAT service's
    /// observations alone. `None` lists every pin over every service.
    pub service: Option<String>,
    /// Window on the field's most recent observation: a field whose
    /// `max(last_seen)` — within `service`, when set — predates this
    /// instant is hidden. A field with NO observations at all (e.g. the
    /// envelope seed on a fresh install) is ALWAYS shown — there is
    /// nothing to age out. `None` disables the window.
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

/// One row of the field listing: the pin plus its aggregated observation
/// and conflict evidence.
#[derive(Debug, Clone)]
pub struct FieldSummaryRow {
    /// Field name.
    pub field: String,
    /// Pinned `DuckDB` type spelling.
    pub duckdb_type: String,
    /// Which service's batch set the pin (`_declared` for the envelope
    /// seed; `NULL` only in hand-edited catalogs).
    pub pinned_from: Option<String>,
    /// When the pin was written.
    pub pinned_at: DateTime<Utc>,
    // The four observation aggregates below span every service that ever
    // carried the field — or exactly the one service, when the listing was
    // scoped by [`FieldListFilter::service`].
    /// Distinct services that ever carried the field (1 when scoped).
    pub service_count: i64,
    /// Cumulative rows across the observations in scope.
    pub row_count: i64,
    /// Earliest observation in scope (`None` when never observed).
    pub first_seen: Option<DateTime<Utc>>,
    /// Most recent observation in scope (`None` when never observed).
    pub last_seen: Option<DateTime<Utc>>,
    /// Conflict evidence rows currently retained for the field.
    pub conflict_count: i64,
    /// Total rows nulled across the retained evidence.
    pub rows_nulled: i64,
}

/// One field's pin row ([`CatalogStore::field_pin`]).
#[derive(Debug, Clone)]
pub struct FieldPinRow {
    /// Field name.
    pub field: String,
    /// Pinned `DuckDB` type spelling.
    pub duckdb_type: String,
    /// Which service's batch set the pin.
    pub pinned_from: Option<String>,
    /// When the pin was written.
    pub pinned_at: DateTime<Utc>,
}

/// A `field_conflicts` row with its field name — the cross-field listing
/// shape ([`CatalogStore::recent_conflicts`]).
#[derive(Debug, Clone)]
pub struct ConflictListRow {
    /// Field name.
    pub field: String,
    /// Service that disagreed.
    pub service: String,
    /// Observed `DuckDB` type spelling.
    pub observed_type: String,
    /// Expected (pinned) type spelling.
    pub expected_type: String,
    /// Rows nulled by the conforming cast.
    pub rows_nulled: i64,
    /// A bounded sample of the values the cast nulled (see
    /// [`FieldConflict::samples`]).
    pub samples: Vec<String>,
    /// When the conflict was recorded.
    pub at: DateTime<Utc>,
}

/// A `field_conflicts` row as read back (types as stored text).
#[derive(Debug, Clone)]
pub struct FieldConflictRow {
    /// Service that disagreed.
    pub service: String,
    /// Observed `DuckDB` type spelling.
    pub observed_type: String,
    /// Expected (pinned) type spelling.
    pub expected_type: String,
    /// Rows nulled by the conforming cast.
    pub rows_nulled: i64,
    /// A bounded sample of the values the cast nulled (see
    /// [`FieldConflict::samples`]); empty for evidence recorded before
    /// migration 0008, or by a lane with no values in hand.
    pub samples: Vec<String>,
    /// When the conflict was recorded.
    pub at: DateTime<Utc>,
}

/// Maximum number of fields the catalog will ever pin.
///
/// Field names are client-chosen JSON keys, and a pin SLOT is PERMANENT
/// (a repin retypes a pin, nothing reclaims one, and retention never
/// reconciles `field_services`). Without a count bound, a sender that
/// embeds identifiers in its keys — `user_12345_status`, accidental or
/// hostile — grows postgres, the in-process [`crate::catalog::FieldCatalog`]
/// cache, and every snapshot taken of it without limit. Name LENGTH is
/// bounded by [`trawl_core::schema::is_storable_field_name`]; this bounds
/// the COUNT.
///
/// A field denied a pin gets the same treatment as an unstorable name: it
/// is absent from the pin map, so compaction's conform step drops the
/// column and the values stay findable in `_raw`. Deliberately generous —
/// a real corpus that legitimately reaches five figures of distinct field
/// names has a modelling problem this cap should surface, not a capacity
/// problem trawl should silently absorb.
///
/// **FILLING the cap is the attack the cap itself invites.** A slot, once
/// taken, is taken forever, and denial is silent in the data (the column is
/// simply absent from every later parquet file). Left first-come-first-served
/// the whole catalog was consumable in ONE compaction batch — a single
/// request carrying ten thousand junk keys — after which every genuinely new
/// field on the install, from every service, was permanently unstored. Two
/// things keep that unreachable:
///
/// - Admission from the ingest path is RATIONED: one batch may take at most
///   half the FREE slots ([`Ration::HalfOfFree`]), so exhaustion is a slope
///   requiring sustained, repeated effort instead of a one-shot cliff, and
///   there is always headroom left for the next field a legitimate sender
///   introduces.
/// - The fill level is a first-class signal —
///   `trawl_catalog_pinned_fields` against `trawl_catalog_pin_capacity` —
///   so an operator alerts on the slope, not on the wreckage. (The
///   `catalog_pin_cap_reached` warning only fires once slots are already
///   gone.)
///
/// What neither buys is a REMEDY: the repin engine (ADR-0011 slice B)
/// retypes a wrong pin, but reclaiming a taken SLOT means proving no
/// standing parquet carries the column — deliberately out of scope; a
/// hand-run `DELETE FROM field_types` breaks the write-time conformance
/// invariant for files already on disk and must not be recommended. A
/// sustained sender can still fill the catalog — the ration slows it and
/// the gauges make it visible while it happens.
pub const MAX_PINNED_FIELDS: i64 = 10_000;

/// Maximum `field_conflicts` rows kept per field — the newest survive.
///
/// [`MAX_PINNED_FIELDS`] bounds how many fields can exist; nothing bounds
/// how often one of them disagrees with its pin. A field pinned `BIGINT`
/// that keeps receiving strings appends a row per service per compaction
/// tick — every ten seconds, forever — so without a per-field bound the
/// evidence table grows without limit while the sender's behaviour, and
/// therefore the evidence's information content, stays constant.
///
/// The bound is per FIELD, not per `(field, service)`: service names are
/// client-chosen too, so a per-service window would only move the
/// unbounded axis. A field conflicting across more services than the
/// window is therefore sampled, not covered — the exhaustive, never-lossy
/// tallies are the `trawl_catalog_conflicts_total` /
/// `trawl_catalog_rows_nulled_total` counters; these rows exist to show an
/// operator WHICH values a pin is currently costing them, and the newest
/// evidence is the evidence they act on.
pub const MAX_CONFLICTS_PER_FIELD: i64 = 100;

/// Distinct misfit values kept per conflict row (ADR-0011 slice C1).
///
/// The samples answer "which values is this pin costing me", which five
/// distinct examples answer as well as five hundred — while five hundred
/// per row, at the cap above, per field, at the pin cap, is a table whose
/// size is set by how creatively a sender misformats its values. The
/// exhaustive record is `_raw`, which already holds every one of them.
pub const MAX_CONFLICT_SAMPLES: usize = 5;

/// Bytes kept per sample, cut on a `char` boundary at capture.
///
/// A misfit value is client text of client-chosen length — a whole embedded
/// document can arrive under a BIGINT pin. Enough to recognise the shape of
/// what is arriving, far short of storing the payload a second time.
pub const MAX_CONFLICT_SAMPLE_BYTES: usize = 256;

/// Conflict rows per field the verdict is built from
/// ([`CatalogStore::conflict_evidence_for`]), newest first.
///
/// Sized to what a verdict actually consumes: each row carries up to
/// [`MAX_CONFLICT_SAMPLES`] distinct samples, so this many rows can fill the
/// sample budget several times over even when they overlap heavily.
///
/// It also bounds the OBSERVED-TYPE evidence the suggested target is derived
/// from, and that is a deliberate narrowing: the suggestion now describes the
/// newest episodes rather than the whole retained window. That reads the
/// right way round — a field whose recent traffic is uniformly one rung
/// should be repinned to that rung — and the suggestion is a starting point
/// for a dry run, never an action.
const VERDICT_EVIDENCE_ROWS: i64 = 5;

// Written as a literal because it binds as a postgres `bigint`, and pinned
// to the sample budget it is derived from: change one and this fails the
// build rather than silently under-filling a verdict.
const _: () = assert!(MAX_CONFLICT_SAMPLES == 5);

/// Rows per statement in [`CatalogStore::backfill_services`]. The backfill's
/// size is (pinned fields x services), which nothing bounds below five
/// figures, so it is written in chunks rather than one array-of-everything.
const BACKFILL_CHUNK: usize = 1_000;

/// One field name shortened for logging: a name can be as long as a client
/// made it, so echoing it whole turns the log line into an amplifier of
/// whatever was sent.
fn short_name(name: &str) -> String {
    let head: String = name.chars().take(48).collect();
    format!("{head}... ({} bytes)", name.len())
}

/// The names in `names` that cannot be a catalog key, de-duplicated and
/// each shortened for logging (see [`short_name`]).
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

/// How many of the free pin slots ONE call may consume.
///
/// The pin count is bounded ([`MAX_PINNED_FIELDS`]) but a slot is spent
/// permanently, so *how fast* the free slots can be spent is its own
/// property — a bound nothing can take in one gulp behaves very differently
/// from one anything can. See [`MAX_PINNED_FIELDS`].
#[derive(Debug, Clone, Copy)]
enum Ration {
    /// Ingest path: at most half the free slots, so no single batch — and
    /// therefore no single ingest request — can consume the catalog.
    HalfOfFree,
    /// Boot conformance pass: every free slot, because there a denied pin
    /// deletes a column that is already on disk
    /// ([`CatalogStore::pin_missing_unrationed`]).
    EveryFreeSlot,
}

/// Publish the catalog's fill level: `used` and the ceiling it is measured
/// against, so an alert is a ratio and needs no knowledge of the constant.
#[allow(clippy::cast_precision_loss)] // gauge values are f64; a 10k cap is exact
fn set_fill_gauges(pinned: i64, cap: i64) {
    metrics::gauge!(crate::metrics::CATALOG_PINNED_FIELDS).set(pinned as f64);
    metrics::gauge!(crate::metrics::CATALOG_PIN_CAPACITY).set(cap as f64);
}

/// Count fields denied a pin, by reason. No field-name label: the names are
/// client-chosen and unbounded, which is the very problem being counted.
fn bump_rejected(reason: &'static str, count: usize) {
    metrics::counter!(
        crate::metrics::CATALOG_PINS_REJECTED_TOTAL,
        "reason" => reason
    )
    .increment(count as u64);
}

/// Postgres-backed field catalog. Cheap to clone (shared pool).
#[derive(Debug, Clone)]
pub struct CatalogStore {
    pool: PgPool,
    pin_cap: i64,
    conflict_cap: i64,
}

impl CatalogStore {
    /// Wrap the shared app-state pool (must already be migrated — see
    /// [`super::HistoryStore::new`] for the contract).
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            pin_cap: MAX_PINNED_FIELDS,
            conflict_cap: MAX_CONFLICTS_PER_FIELD,
        }
    }

    /// Override the pin-count cap. Exists so tests can drive the cap
    /// boundary without inserting [`MAX_PINNED_FIELDS`] rows; production
    /// uses the constant.
    #[must_use]
    pub fn with_pin_cap(mut self, cap: i64) -> Self {
        self.pin_cap = cap;
        self
    }

    /// Override the per-field conflict-retention window. Same purpose as
    /// [`Self::with_pin_cap`]: drive the boundary without writing
    /// [`MAX_CONFLICTS_PER_FIELD`] rows.
    #[must_use]
    pub fn with_conflict_cap(mut self, cap: i64) -> Self {
        self.conflict_cap = cap;
        self
    }

    /// Load every pin in the catalog.
    ///
    /// A stored spelling outside the canonical five fails loudly — the
    /// catalog is written by code, so that is corruption, not data.
    pub async fn load_pins(&self) -> Result<Vec<(String, CanonicalType)>, StoreError> {
        let rows = sqlx::query("SELECT field, duckdb_type FROM field_types ORDER BY field")
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|row| {
                let field: String = row.try_get("field")?;
                let spelling: String = row.try_get("duckdb_type")?;
                let ty = CanonicalType::from_duckdb(&spelling).ok_or_else(|| {
                    sqlx::Error::Decode(
                        format!("field_types.{field} holds non-canonical type {spelling:?}").into(),
                    )
                })?;
                Ok((field, ty))
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()
            .map_err(StoreError::from)
    }

    /// Pin every proposal whose field is absent from the catalog, then
    /// return the AUTHORITATIVE pins for all proposed fields.
    ///
    /// First-writer-wins: `INSERT ... ON CONFLICT (field) DO NOTHING`, then
    /// a re-read — so two racing batches (or a proposal against an existing
    /// pin) both come back with the same pin, never their own proposal.
    ///
    /// A field whose name cannot be a catalog key (see
    /// [`unstorable_names`]) is DROPPED from the proposal set rather than
    /// allowed to error the insert: pinning gates every parquet write, so
    /// one such name would otherwise retain the batch for retry on every
    /// tick, forever. Absent from the returned map, the column is simply
    /// unpinned — which the conform step already treats as "drop it".
    ///
    /// The same treatment bounds the catalog's SIZE: the insert only fills
    /// the slots left under [`MAX_PINNED_FIELDS`], so a batch arriving at a
    /// full catalog pins nothing new and its novel columns are dropped from
    /// the parquet with their values still in `_raw` (never an `Err` — that
    /// would wedge compaction for a condition retrying cannot clear).
    ///
    /// This is the ingest path, so admission is RATIONED to half the free
    /// slots ([`Ration::HalfOfFree`] — see [`MAX_PINNED_FIELDS`] for why a
    /// batch that can take every remaining slot is the attack). A batch
    /// proposing more novel fields than its ration keeps the surplus for its
    /// next tick: an unpinned column is dropped from THIS file only, and a
    /// field a sender keeps sending is proposed again ten seconds later.
    pub async fn pin_missing(
        &self,
        proposals: &[PinProposal],
    ) -> Result<HashMap<String, CanonicalType>, StoreError> {
        self.pin_missing_with(proposals, Ration::HalfOfFree).await
    }

    /// Pin proposals WITHOUT the per-batch ration — the boot conformance
    /// pass only.
    ///
    /// That pass does not propose client input as it arrives: it proposes
    /// what a standing parquet corpus already contains, and its next act is
    /// to rewrite the files that disagree. There, a denied pin does not
    /// decline to store a new column — it DELETES a column that is already
    /// on disk. Rationing the seed would therefore destroy data to slow an
    /// attacker who has already spent the slots, so the seed gets every free
    /// slot the cap allows; [`MAX_PINNED_FIELDS`] still bounds it absolutely.
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
        let types: Vec<&str> = proposals.iter().map(|p| p.ty.as_duckdb()).collect();
        let sources: Vec<&str> = proposals.iter().map(|p| p.pinned_from.as_str()).collect();

        // Only the free slots under the cap are filled, and on the ingest
        // path only half of them (see `Ration`). Already-pinned proposals
        // are excluded from `candidate` so they never consume a slot, and
        // the surplus is ordered by name so an overflowing batch picks
        // deterministically rather than by arrival accident — within one
        // batch there is no legitimacy signal to rank by, which is exactly
        // why the ration, not the ordering, is what protects the tail.
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

        // The fill level is the signal an operator can act on BEFORE the cap
        // bites; `catalog_pin_cap_reached` below only fires once the slots
        // are already gone — and gone permanently (a repin retypes a slot,
        // nothing reclaims one).
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
                let ty = CanonicalType::from_duckdb(&spelling).ok_or_else(|| {
                    sqlx::Error::Decode(
                        format!("field_types.{field} holds non-canonical type {spelling:?}").into(),
                    )
                })?;
                Ok((field, ty))
            })
            .collect::<Result<HashMap<_, _>, sqlx::Error>>()
            .map_err(StoreError::from)?;

        // Whatever the insert could not seat comes back missing from the
        // authoritative re-read — the one place that sees the cap bite,
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

    /// Upsert per-service observations for a compacted batch: `first_seen`
    /// is set once, `last_seen` advances, `row_count` accumulates.
    ///
    /// `row_count` is ADDED to the stored value, so callers must pass the
    /// rows THIS batch wrote — never a whole-file total, which would
    /// re-count every earlier batch on every tick.
    ///
    /// Names that cannot be a catalog key are skipped for the same reason
    /// as in [`Self::pin_missing`] — `field_services` keys on
    /// `(field, service)`, so an over-long name overflows this btree too.
    ///
    /// Rows are ever-observed: nothing removes one, deliberately (the
    /// issue's acceptance criterion). "Which services ever carried this
    /// field" is historical fact, and consumers window on `last_seen`. The
    /// table's worst case is bounded by [`MAX_PINNED_FIELDS`] on the field
    /// axis times the distinct service names a deployment really ships.
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

    /// Backfill observations for a corpus that predates the catalog — the
    /// boot conformance pass, not the ingest path.
    ///
    /// `field_services` is the authority behind `?service=` and the
    /// `last_seen` window on the schema surfaces, and only compaction ever
    /// wrote it: on an upgrade, migration 0002 creates the table EMPTY while
    /// the boot pass pins (and rewrites) a corpus that no live batch will
    /// re-observe until its service next sends the field. A service that
    /// stopped sending — or an env retired but retained — would therefore
    /// answer `?service=` with nothing at all, and its pins would sit outside
    /// the `last_seen` window forever (a never-observed pin is always shown,
    /// by design). This closes that gap from the standing files themselves.
    ///
    /// **Idempotent**, because the pass re-runs on every boot until the
    /// corpus is proven conformant: `first_seen` only moves earlier,
    /// `last_seen` only later, and `row_count` takes the MAX rather than
    /// accumulating — so re-running over the same corpus is a no-op and a
    /// live tick's accumulated count is never clobbered downward by a
    /// backfill that sees a retention-shrunk corpus.
    ///
    /// Callers must pass at most one row per `(field, service)`: postgres
    /// refuses to let one `ON CONFLICT DO UPDATE` statement touch a row
    /// twice. The only caller aggregates into a map keyed by exactly that
    /// pair.
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
        // ships), which the pin cap bounds at five figures on one axis alone
        // — too many parameters' worth of arrays for a single statement.
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

    /// Append conflict evidence rows (never aggregated), then trim every
    /// field this call touched back to its newest [`MAX_CONFLICTS_PER_FIELD`]
    /// rows.
    ///
    /// Append + trim, not an upsert: the value of a conflict row is the
    /// individual episode (when, which service, how many rows it cost), and
    /// aggregating loses exactly that. Trimming only the touched fields
    /// keeps the work proportional to the batch — a field that stops
    /// conflicting keeps the evidence it already has and is never re-read.
    ///
    /// A THIRD statement upserts the durable per-`(field, service)`
    /// aggregates (`field_conflict_stats`, migration 0009) the degraded-field
    /// analyzer judges a pin on. It is here, and not in a caller, because the
    /// trim above is exactly what makes it necessary: the detail rows a
    /// span-based verdict would have to measure are the ones the window
    /// evicts first, so the aggregate has to be written by the same
    /// transaction that evicts them or it is a different number.
    ///
    /// All three statements run in ONE transaction, so a reader never sees
    /// the window overfull, a failed trim never leaves the insert behind, and
    /// no episode is ever counted in the aggregates without its evidence row
    /// (or the reverse).
    pub async fn record_conflicts(&self, conflicts: &[FieldConflict]) -> Result<(), StoreError> {
        if conflicts.is_empty() {
            return Ok(());
        }
        let fields: Vec<&str> = conflicts.iter().map(|c| c.field.as_str()).collect();
        let services: Vec<&str> = conflicts.iter().map(|c| c.service.as_str()).collect();
        let observed: Vec<&str> = conflicts.iter().map(|c| c.observed_type.as_str()).collect();
        let expected: Vec<&str> = conflicts
            .iter()
            .map(|c| c.expected_type.as_duckdb())
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

        let mut tx = self.pool.begin().await?;

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
        .bind(self.conflict_cap)
        .execute(&mut *tx)
        .await?;

        // The durable aggregates the trim above must not be able to erase.
        //
        // The GROUP BY is not an optimisation: postgres refuses to let one
        // `ON CONFLICT DO UPDATE` touch a row twice, and the boot conformance
        // pass accumulates conflicts across every file it rewrites, so one
        // call routinely carries many rows for the same `(field, service)`.
        // Pre-aggregating in the statement makes a call of N such rows one
        // upsert of N episodes — the same hazard `backfill_services` avoids
        // by aggregating in its caller, answered here in SQL because this
        // caller's rows are the evidence and may not be collapsed.
        //
        // `last_at` is `now()`, not `GREATEST(existing, now())`: it is the
        // transaction's own clock, which cannot run backwards against a row
        // this same statement is the only writer of. `first_at` is left
        // untouched on conflict for the mirror-image reason — the row's
        // existing value is by construction the earliest evidence there is
        // (migration 0009's backfill included).
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

        tx.commit().await?;
        Ok(())
    }

    /// Read ONE PAGE of the observation rows for one field, most recent
    /// first, resuming after `after`.
    ///
    /// Paged, never whole: `field_services` is bounded on the field axis by
    /// the pin cap but NOT on the service axis — service names are
    /// client-chosen and rows are ever-observed, so the history of a common
    /// envelope field grows with every service a sender ever invents,
    /// without spending a pin slot. An unpaged read would hand one
    /// `schema_read` request an arbitrarily large database read, allocation,
    /// and response body.
    ///
    /// Keyset, not `OFFSET`: the page walks `(last_seen DESC, service ASC)`,
    /// which is unique per field (`(field, service)` is the primary key), so
    /// a cursor names an exact position and a concurrent observation update
    /// cannot make a page repeat or skip a row it already delivered.
    ///
    /// The cursor predicate is deliberately written as
    /// `last_seen <= cursor AND (last_seen < cursor OR service > $3)` rather
    /// than the equivalent single `OR` chain, and `COALESCE`s the absent
    /// cursor to `infinity` rather than guarding it with `IS NULL`. Both
    /// shapes select the same rows, but only this one is *sargable*: the
    /// leading conjunct is a bound postgres can push into
    /// `field_services_field_last_seen_idx` (migration 0004) as an index
    /// scan key, so the page STARTS at the cursor. Under the `OR` chain the
    /// whole thing degrades to a filter and every page re-reads the field's
    /// entire history — bounding the allocation and the response body, but
    /// not the read, which makes walking the pages quadratic.
    ///
    /// Returns `(rows, next)`; `next` is `Some` when more rows follow.
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
        // Fetch one extra row purely to learn whether a next page exists.
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

    /// [`Self::conflict_aggregates`], page-keyed shape: `$1` field names.
    /// See the method docs for why this is its own SQL text.
    const CONFLICT_AGGREGATES_KEYED_SQL: &'static str = "\
        SELECT field, min(first_at) AS first_at, max(last_at) AS last_at,
               count(*)::bigint                      AS services,
               COALESCE(sum(episodes), 0)::bigint    AS episodes,
               COALESCE(sum(rows_nulled_total), 0)::bigint AS rows_nulled_total
        FROM field_conflict_stats
        WHERE field = ANY($1)
        GROUP BY field";

    /// [`Self::conflict_aggregates`], whole-catalog shape.
    const CONFLICT_AGGREGATES_ALL_SQL: &'static str = "\
        SELECT field, min(first_at) AS first_at, max(last_at) AS last_at,
               count(*)::bigint                      AS services,
               COALESCE(sum(episodes), 0)::bigint    AS episodes,
               COALESCE(sum(rows_nulled_total), 0)::bigint AS rows_nulled_total
        FROM field_conflict_stats
        GROUP BY field";

    /// Aggregate the durable conflict evidence per FIELD — the input the
    /// degraded-field analyzer judges ([`crate::catalog::analyzer`]).
    ///
    /// `fields` keys the read to a page of pin names; `None` aggregates the
    /// whole table, which is what a refresh tick wants and is bounded on
    /// the returned axis by the pin cap.
    ///
    /// Two SQL TEXTS rather than one `($1 IS NULL OR field = ANY($1))`
    /// shape, for the reason [`Self::list_fields`] documents at length: a
    /// prepared statement switching to its generic plan cannot push the
    /// `IS NULL`-guarded `OR` into the primary key, so the page-keyed read
    /// would degrade into a full aggregate over a table whose SERVICE axis
    /// is client-chosen and never pruned.
    pub async fn conflict_aggregates(
        &self,
        fields: Option<&[String]>,
    ) -> Result<Vec<ConflictAggregate>, StoreError> {
        let rows = match fields {
            Some([]) => return Ok(Vec::new()),
            Some(names) => {
                sqlx::query(Self::CONFLICT_AGGREGATES_KEYED_SQL)
                    .bind(names)
                    .fetch_all(&self.pool)
                    .await?
            }
            None => {
                sqlx::query(Self::CONFLICT_AGGREGATES_ALL_SQL)
                    .fetch_all(&self.pool)
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
                })
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()
            .map_err(StoreError::from)
    }

    /// The retained detail evidence for `fields`, newest first: the
    /// `(observed_type, samples)` pairs a verdict is built from.
    ///
    /// Read only for the fields that came back DEGRADED — usually none —
    /// and bounded PER FIELD by [`VERDICT_EVIDENCE_ROWS`] through a LATERAL
    /// subquery, not by an outer LIMIT: a page-wide limit would spend the
    /// whole budget on the first field and leave the rest verdictless.
    /// Unbounded, one `/schema/fields` request with a large `?limit=` over a
    /// pathologically conflicted catalog would materialise
    /// `MAX_CONFLICTS_PER_FIELD` rows — each carrying up to its full sample
    /// payload — for every degraded field on the page.
    pub async fn conflict_evidence_for(
        &self,
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
        .fetch_all(&self.pool)
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

    /// Drop every trace of one field's conflict evidence, inside a caller's
    /// transaction.
    ///
    /// Called by the repin cutover ([`crate::store::RepinStore::finish_cutover`]):
    /// the evidence indicts a pin that no longer exists, and the analyzer's
    /// gate is span-based — left standing, a repinned field would keep its
    /// verdict forever, and the badge that told the operator to act would
    /// survive their acting on it. Ordering is the cutover's, not ours: the
    /// clear runs before the job records its own outcome, so a forced lossy
    /// repin's fresh evidence is not swept away with the old.
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

    /// Read the conflict rows for one field, most recent first.
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

    /// List pins with their aggregated observation and conflict evidence —
    /// the read model behind `/api/v1/schema` and `/api/v1/schema/fields`.
    ///
    /// The pin page is one query: `field_types` LEFT JOIN grouped
    /// `field_services`. The LEFT join is load-bearing: a pin with no
    /// observations (the envelope seed on a fresh install, or a boot-pass
    /// pin over standing parquet) must always appear — windowing only hides
    /// fields whose evidence says they aged out.
    ///
    /// `filter.service` scopes the AGGREGATE, not just the row set: the
    /// predicate goes INSIDE the `field_services` grouping, so a scoped
    /// listing reports that service's own counts and instants, and the
    /// `since` window is evaluated against that service's `last_seen`.
    /// Filtering only in the `WHERE` clause would leak every other
    /// service's numbers into the listing and keep a field alive in the
    /// window because somebody ELSE still sends it. The `EXISTS` stays as
    /// the presence test — a pin the service never carried has no group
    /// row, and the never-observed rule would otherwise show it.
    ///
    /// The conflict evidence is a SECOND query, keyed on the field names
    /// this page actually returned, and skipped entirely when
    /// `filter.with_conflicts` is false. Joining a grouped
    /// `SELECT ... FROM field_conflicts GROUP BY field` instead carried no
    /// predicate a planner could push down, so every call — including the
    /// deliberately uncached `?service=` one — materialised an aggregate
    /// over the whole table, which is bounded only by
    /// [`MAX_CONFLICTS_PER_FIELD`] x [`MAX_PINNED_FIELDS`] and not by the
    /// pin cap the scoped listing promises. Keyed on the page it rides
    /// 0002's `(field, at DESC)` index and reads at most
    /// `limit` x [`MAX_CONFLICTS_PER_FIELD`] rows.
    ///
    /// Scoped and unscoped are two SQL TEXTS, not one
    /// `($1 IS NULL OR service = $1)` shape: sqlx prepares and caches every
    /// statement per pooled connection, and on execution 6 postgres
    /// (`plan_cache_mode=auto`) switches a prepared statement to its
    /// generic plan — under which the `IS NULL`-guarded `OR` cannot be
    /// pushed into `field_services_service_field_idx` (migration 0003) as a
    /// scan key, so the scoped listing degrades to reading the whole table
    /// (measured at 0003's own sizing: 2 318 → 504 366 buffers), unbounded
    /// in the client-chosen service axis. Same reasoning as the cursor
    /// predicate in [`Self::field_services`]. The `since` guard keeps the
    /// `IS NULL`-`OR` shape: it filters the joined rows AFTER aggregation,
    /// bounded by the pin cap, and is no index's scan key either way.
    ///
    /// Returns `(rows, truncated)`; `truncated` is set when more rows
    /// matched than `filter.limit` allowed back.
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
            // Fetch one extra row purely to learn whether the limit truncated.
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

    /// The catalog's fill level: `(pinned, capacity)` — the same pair the
    /// `trawl_catalog_pinned_fields` / `trawl_catalog_pin_capacity` gauges
    /// publish, surfaced on the fields listing so a client sees headroom.
    pub async fn pin_stats(&self) -> Result<(i64, i64), StoreError> {
        let pinned: i64 = sqlx::query_scalar("SELECT count(*) FROM field_types")
            .fetch_one(&self.pool)
            .await?;
        Ok((pinned, self.pin_cap))
    }

    /// Cross-field conflict listing, most recent first — the schema-health
    /// dashboard read (`trawl schema conflicts --last 7d`).
    ///
    /// `field`/`service` filter exactly; `since` windows on the recording
    /// instant. Returns `(rows, truncated)` like [`Self::list_fields`].
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

    /// Read one field's pin row, or `None` when the field is not pinned.
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

    /// The catalog's stable identity (mirrored into the `data/CATALOG`
    /// marker so `DATABASE_URL` repoints and data-root restores are
    /// self-detecting).
    pub async fn catalog_id(&self) -> Result<String, StoreError> {
        let id: String = sqlx::query_scalar("SELECT catalog_id::text FROM catalog_state")
            .fetch_one(&self.pool)
            .await?;
        Ok(id)
    }

    /// Whether the boot conformance pass has completed for this catalog.
    pub async fn is_conformed(&self) -> Result<bool, StoreError> {
        let conformed: bool =
            sqlx::query_scalar("SELECT conformed_at IS NOT NULL FROM catalog_state")
                .fetch_one(&self.pool)
                .await?;
        Ok(conformed)
    }

    /// Record boot-conformance completion.
    pub async fn mark_conformed(&self) -> Result<(), StoreError> {
        sqlx::query("UPDATE catalog_state SET conformed_at = now()")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Re-arm the boot conformance pass (ADR-0011 slice B): a repin cutover
    /// recovered at boot clears this so `ensure_conformance` re-proves the
    /// corpus against the flipped pin in the same boot — the backstop for
    /// any file an interrupted repin missed.
    pub async fn clear_conformed(&self) -> Result<(), StoreError> {
        sqlx::query("UPDATE catalog_state SET conformed_at = NULL")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Whether the boot pass has backfilled `field_services` from the
    /// standing corpus for this catalog.
    ///
    /// Tracked separately from [`Self::is_conformed`] on purpose: the
    /// backfill ([`Self::backfill_services`]) shipped a slice after
    /// conformance did, so an install that conformed under the earlier slice
    /// carries `conformed_at` set and this flag NULL — the one state where
    /// the pass must run again.
    pub async fn services_backfilled(&self) -> Result<bool, StoreError> {
        let backfilled: bool =
            sqlx::query_scalar("SELECT services_backfilled_at IS NOT NULL FROM catalog_state")
                .fetch_one(&self.pool)
                .await?;
        Ok(backfilled)
    }

    /// Record that the boot pass observed the standing corpus.
    pub async fn mark_services_backfilled(&self) -> Result<(), StoreError> {
        sqlx::query("UPDATE catalog_state SET services_backfilled_at = now()")
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cursor is opaque to clients but must round-trip exactly: a
    /// microsecond lost in the encoding would re-deliver or skip the row it
    /// names. Service names carry `.`, `-`, and `_`; none is `|`.
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

    /// A malformed cursor is a client error, never a silently dropped
    /// filter: decoding fails so the handler can 400 instead of restarting
    /// the walk at page one.
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
