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
//! conflict. Pins are add-only until the repin machinery (#53) — and
//! because a pin is therefore permanent while its name is a client-chosen
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
    /// Only fields observed for this service (`EXISTS` over
    /// `field_services`). `None` lists every pin.
    pub service: Option<String>,
    /// Window on the field's most recent observation: a field whose
    /// `max(last_seen)` predates this instant is hidden. A field with NO
    /// observations at all (e.g. the envelope seed on a fresh install) is
    /// ALWAYS shown — there is nothing to age out. `None` disables the
    /// window.
    pub since: Option<DateTime<Utc>>,
    /// Maximum rows returned (the caller clamps; see the route handlers).
    pub limit: i64,
}

impl Default for FieldListFilter {
    fn default() -> Self {
        Self {
            service: None,
            since: None,
            limit: MAX_PINNED_FIELDS,
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
    /// Distinct services that ever carried the field.
    pub service_count: i64,
    /// Cumulative rows across all services' observations.
    pub row_count: i64,
    /// Earliest observation across services (`None` when never observed).
    pub first_seen: Option<DateTime<Utc>>,
    /// Most recent observation across services (`None` when never observed).
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
    /// When the conflict was recorded.
    pub at: DateTime<Utc>,
}

/// Maximum number of fields the catalog will ever pin.
///
/// Field names are client-chosen JSON keys, and a pin is PERMANENT (pins
/// are add-only until the repin machinery, #53, and retention never
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
/// What neither buys is a REMEDY: reclaiming a taken slot means proving no
/// standing parquet carries the column and rewriting the ones that do, which
/// is the shadow-generation rewrite of #53; a hand-run
/// `DELETE FROM field_types` breaks the write-time conformance invariant for
/// files already on disk and must not be recommended. Until #53, a sustained
/// sender can still fill the catalog — the ration slows it and the gauges
/// make it visible while it happens.
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
        // are already gone (and gone permanently, until #53).
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
    /// Both statements run in ONE transaction, so a reader never sees the
    /// window overfull and a failed trim never leaves the insert behind.
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

        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "INSERT INTO field_conflicts
                 (field, service, observed_type, expected_type, rows_nulled)
             SELECT * FROM UNNEST($1::text[], $2::text[], $3::text[], $4::text[], $5::bigint[])",
        )
        .bind(&fields)
        .bind(&services)
        .bind(&observed)
        .bind(&expected)
        .bind(&nulled)
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
               AND ($2::timestamptz IS NULL
                    OR last_seen < $2
                    OR (last_seen = $2 AND service > $3))
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

    /// Read the conflict rows for one field, most recent first.
    pub async fn conflicts_for_field(
        &self,
        field: &str,
    ) -> Result<Vec<FieldConflictRow>, StoreError> {
        let rows = sqlx::query(
            "SELECT service, observed_type, expected_type, rows_nulled, at
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
                    at: row.try_get("at")?,
                })
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()
            .map_err(StoreError::from)
    }

    /// List pins with their aggregated observation and conflict evidence —
    /// the read model behind `/api/v1/schema` and `/api/v1/schema/fields`.
    ///
    /// One query: `field_types` LEFT JOIN grouped `field_services` LEFT
    /// JOIN grouped `field_conflicts`. LEFT joins are load-bearing: a pin
    /// with no observations (the envelope seed on a fresh install, or a
    /// boot-pass pin over standing parquet) must always appear — windowing
    /// only hides fields whose evidence says they aged out.
    ///
    /// Returns `(rows, truncated)`; `truncated` is set when more rows
    /// matched than `filter.limit` allowed back.
    pub async fn list_fields(
        &self,
        filter: &FieldListFilter,
    ) -> Result<(Vec<FieldSummaryRow>, bool), StoreError> {
        let limit = filter.limit.max(0);
        let rows = sqlx::query(
            "SELECT t.field, t.duckdb_type, t.pinned_from, t.pinned_at,
                    COALESCE(s.service_count, 0)::bigint AS service_count,
                    COALESCE(s.row_count, 0)::bigint     AS row_count,
                    s.first_seen, s.last_seen,
                    COALESCE(c.conflict_count, 0)::bigint AS conflict_count,
                    COALESCE(c.rows_nulled, 0)::bigint    AS rows_nulled
             FROM field_types t
             LEFT JOIN (
                 SELECT field, count(*) AS service_count, sum(row_count) AS row_count,
                        min(first_seen) AS first_seen, max(last_seen) AS last_seen
                 FROM field_services GROUP BY field
             ) s ON s.field = t.field
             LEFT JOIN (
                 SELECT field, count(*) AS conflict_count, sum(rows_nulled) AS rows_nulled
                 FROM field_conflicts GROUP BY field
             ) c ON c.field = t.field
             WHERE ($1::text IS NULL OR EXISTS (
                        SELECT 1 FROM field_services fs
                        WHERE fs.field = t.field AND fs.service = $1))
               AND ($2::timestamptz IS NULL
                        OR s.last_seen IS NULL
                        OR s.last_seen >= $2)
             ORDER BY t.field
             LIMIT $3",
        )
        .bind(filter.service.as_deref())
        .bind(filter.since)
        // Fetch one extra row purely to learn whether the limit truncated.
        .bind(limit.saturating_add(1))
        .fetch_all(&self.pool)
        .await?;

        let truncated = i64::try_from(rows.len()).unwrap_or(i64::MAX) > limit;
        let take = usize::try_from(limit).unwrap_or(usize::MAX);
        rows.iter()
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
                    conflict_count: row.try_get("conflict_count")?,
                    rows_nulled: row.try_get("rows_nulled")?,
                })
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()
            .map(|rows| (rows, truncated))
            .map_err(StoreError::from)
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
            "SELECT field, service, observed_type, expected_type, rows_nulled, at
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
        row.map(|row| {
            Ok(FieldPinRow {
                field: row.try_get("field")?,
                duckdb_type: row.try_get("duckdb_type")?,
                pinned_from: row.try_get("pinned_from")?,
                pinned_at: row.try_get("pinned_at")?,
            })
        })
        .transpose()
        .map_err(|e: sqlx::Error| StoreError::from(e))
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
