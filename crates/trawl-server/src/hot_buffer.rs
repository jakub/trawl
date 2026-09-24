// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Hot buffer: batch-keyed in-memory event store for query freshness.
//!
//! Events land here via synchronous insertion during ingest/telemetry.
//! The buffer makes fresh events visible to every query by providing
//! a temporary ndjson file that the executor can `UNION ALL BY NAME`
//! with the parquet source.
//!
//! Events stay in the hot buffer until compaction writes parquet and
//! calls [`drain`](HotBuffer::drain). The shared publication guard keeps
//! query and export readers outside the interval between the canonical
//! file rename and hot drain, so they cannot count both copies.
//!
//! # Admission (ADR-0043)
//!
//! Space is admitted, never reclaimed by eviction. A producer calls
//! [`HotBuffer::reserve`] with the exact [`Charge`] of what it is about to
//! write, before it writes the WAL. The ledger checks and charges both
//! dimensions (events, serialized ndjson bytes) in one critical section, so
//! concurrent producers can never overshoot a cap together. The charge then
//! lives in a [`Reservation`]: [`HotBuffer::insert`] moves it into the
//! resident batch, [`HotBuffer::drain`] releases it, and dropping the
//! reservation (a failed write, a panic, a cancelled task) releases it too.
//! The ledger's fields are private to this module, so a charge moves only
//! through a `Reservation`.
//!
//! External producers (HTTP, syslog) may fill [`EXTERNAL_SHARE`] of each
//! cap; self-telemetry ([`ProducerKind::Trawld`]) may fill the whole cap.
//! The producer is named by the calling code path, never by event data.
//!
//! Lock order is always batch map, then ledger. Nothing here traces from
//! [`HotBuffer::reserve`], [`HotBuffer::insert`] or under the ledger lock:
//! self-telemetry's own flush passes through them, so a log line there is
//! an ingestion loop.

use std::io::{BufWriter, Write as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use indexmap::map::Entry;
use parking_lot::{Mutex, RwLock};
use tokio::sync::watch;

use crate::bus::IngestBatch;
use crate::ingest::producer::ProducerKind;

/// One buffered event, as stored in [`IngestBatch::events`].
type Event = serde_json::Map<String, serde_json::Value>;

/// Configuration for the hot buffer.
#[derive(Debug, Clone)]
pub struct HotBufferConfig {
    /// Maximum number of events across all batches.
    pub max_events: usize,
    /// Maximum estimated memory usage in bytes (from ndjson byte sizes).
    ///
    /// This is the serialized ndjson byte count, which underestimates
    /// actual in-memory usage by ~3-5x (in-memory `Map<String, Value>`
    /// has allocator overhead, hash table buckets, `String` headers, etc.).
    /// Set this conservatively — e.g. to 1/4 of the actual memory budget
    /// you want to allocate for the hot buffer.
    pub max_bytes: usize,
}

// -- admission ledger ----------------------------------------------------------

/// The share of each cap that external producers (HTTP, syslog) may fill,
/// as `(numerator, denominator)`. The rest is self-telemetry's floor of
/// opportunity: trawl's own records of a stall stay searchable during it.
pub const EXTERNAL_SHARE: (usize, usize) = (15, 16);

/// Occupancy at or above this fraction of EITHER full cap enters
/// [`AdmissionState::Pressure`].
pub const PRESSURE_ENTER: (usize, usize) = (1, 2);

/// Occupancy below this fraction of BOTH full caps is the only way back to
/// [`AdmissionState::Open`], from `Pressure` and `Refusing` alike.
pub const PRESSURE_EXIT: (usize, usize) = (1, 4);

/// A hot-buffer charge in both ledger dimensions: event count and
/// serialized ndjson bytes (the same units [`IngestBatch::byte_size`]
/// counts).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Charge {
    pub events: usize,
    pub bytes: usize,
}

impl Charge {
    pub const ZERO: Self = Self {
        events: 0,
        bytes: 0,
    };

    /// Whether both dimensions are zero.
    pub const fn is_zero(self) -> bool {
        self.events == 0 && self.bytes == 0
    }

    /// Whether this charge fits under `ceiling` on both dimensions.
    pub const fn fits(self, ceiling: Self) -> bool {
        self.events <= ceiling.events && self.bytes <= ceiling.bytes
    }

    /// Both dimensions added, or `None` if either overflows.
    pub fn checked_add(self, other: Self) -> Option<Self> {
        Some(Self {
            events: self.events.checked_add(other.events)?,
            bytes: self.bytes.checked_add(other.bytes)?,
        })
    }

    /// Both dimensions subtracted, or `None` if either would go negative.
    pub fn checked_sub(self, other: Self) -> Option<Self> {
        Some(Self {
            events: self.events.checked_sub(other.events)?,
            bytes: self.bytes.checked_sub(other.bytes)?,
        })
    }

    fn saturating_add(self, other: Self) -> Self {
        Self {
            events: self.events.saturating_add(other.events),
            bytes: self.bytes.saturating_add(other.bytes),
        }
    }

    fn saturating_sub(self, other: Self) -> Self {
        Self {
            events: self.events.saturating_sub(other.events),
            bytes: self.bytes.saturating_sub(other.bytes),
        }
    }
}

/// The external producers' ceiling for full caps `caps`:
/// `floor(cap * 15 / 16)` per dimension, in `u128` so no cap overflows.
///
/// A cap of 0 or 1 gives a zero ceiling on that dimension, so every
/// non-empty external request is [`Refusal::Oversized`].
pub fn external_ceiling(caps: Charge) -> Charge {
    Charge {
        events: share(caps.events, EXTERNAL_SHARE),
        bytes: share(caps.bytes, EXTERNAL_SHARE),
    }
}

/// `floor(value * num / den)`. The result never exceeds `value` for a
/// proper fraction, so it always converts back.
fn share(value: usize, (num, den): (usize, usize)) -> usize {
    let scaled = value as u128 * num as u128 / den as u128;
    usize::try_from(scaled).unwrap_or(usize::MAX)
}

/// `value >= cap * num / den`, exactly.
fn at_or_above(value: usize, cap: usize, (num, den): (usize, usize)) -> bool {
    value as u128 * den as u128 >= cap as u128 * num as u128
}

/// `value < cap * num / den`, exactly.
fn below(value: usize, cap: usize, (num, den): (usize, usize)) -> bool {
    value as u128 * (den as u128) < cap as u128 * num as u128
}

/// Why a reservation was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The charge fits the producer's ceiling but not the free space.
    /// Retrying after compaction drains can succeed.
    Full,
    /// The charge exceeds the producer's ceiling. It can never fit, so a
    /// retry of the same charge cannot succeed.
    Oversized,
}

impl Refusal {
    pub const ALL: [Self; 2] = [Self::Full, Self::Oversized];

    /// The `kind` label on `trawl_hot_buffer_admission_refusals_total`.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Oversized => "oversized",
        }
    }
}

/// The ledger's hysteresis state, exported as
/// `trawl_hot_buffer_admission_state` (the discriminant).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionState {
    /// Below the pressure threshold, no refusal outstanding.
    Open = 0,
    /// At or above [`PRESSURE_ENTER`] of either cap: compaction should
    /// drain without waiting for WAL age.
    Pressure = 1,
    /// A reservation was refused for lack of space. Latched until
    /// occupancy falls below [`PRESSURE_EXIT`] of both caps.
    Refusing = 2,
}

impl AdmissionState {
    /// The gauge value.
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    const fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Open,
            1 => Self::Pressure,
            _ => Self::Refusing,
        }
    }
}

/// Everything the ledger mutex guards: both dimensions and the hysteresis
/// state move together in one critical section.
#[derive(Debug)]
struct LedgerState {
    /// Reserved plus resident, every producer included.
    charged: Charge,
    admission: AdmissionState,
}

impl LedgerState {
    /// The sole hysteresis evaluator, run under the mutex after every
    /// charge and release (never after a refusal).
    ///
    /// Any state returns to `Open` only on a release (`released`) that
    /// leaves the ledger below [`PRESSURE_EXIT`] of BOTH caps. `Open` enters
    /// `Pressure` at [`PRESSURE_ENTER`] of EITHER cap. `Refusing` is set only
    /// by a refusal and leaves only through the exit, so it never decays to
    /// `Pressure`, and a refusal latches it whatever the occupancy: only a
    /// later drain or dropped reservation can clear it.
    fn settle(&mut self, caps: Charge, released: bool) {
        let c = self.charged;
        let exit = c.is_zero()
            || (below(c.events, caps.events, PRESSURE_EXIT)
                && below(c.bytes, caps.bytes, PRESSURE_EXIT));
        if released && exit {
            self.admission = AdmissionState::Open;
        } else if self.admission == AdmissionState::Open
            && (at_or_above(c.events, caps.events, PRESSURE_ENTER)
                || at_or_above(c.bytes, caps.bytes, PRESSURE_ENTER))
        {
            self.admission = AdmissionState::Pressure;
        }
    }
}

/// The process-wide admission ledger. Private to this module: only a
/// [`Reservation`], [`HotBuffer::insert`] and [`HotBuffer::drain`] move
/// charge.
struct Ledger {
    /// The full caps: self-telemetry's ceiling and the hysteresis base.
    caps: Charge,
    /// [`external_ceiling`] of `caps`, computed once.
    #[allow(
        dead_code,
        reason = "#253 transition: producers adopt admission in later checkpoints"
    )]
    external: Charge,
    state: Mutex<LedgerState>,
    /// Lock-free copy of `state.admission`, written under the mutex.
    mirror: AtomicU8,
    /// Advanced after every release (dropped reservation or drain).
    released: watch::Sender<u64>,
    /// Advanced on a `Full` refusal, and on an insert while not `Open`.
    pressure: watch::Sender<u64>,
    /// Batches removed by drain, monotonic.
    drained_batches: AtomicU64,
}

#[allow(
    dead_code,
    reason = "#253 transition: producers adopt admission in later checkpoints"
)]
impl Ledger {
    fn new(caps: Charge) -> Self {
        Self {
            caps,
            external: external_ceiling(caps),
            state: Mutex::new(LedgerState {
                charged: Charge::ZERO,
                admission: AdmissionState::Open,
            }),
            mirror: AtomicU8::new(AdmissionState::Open.as_u8()),
            released: watch::Sender::new(0),
            pressure: watch::Sender::new(0),
            drained_batches: AtomicU64::new(0),
        }
    }

    fn ceiling(&self, producer: ProducerKind) -> Charge {
        match producer {
            ProducerKind::Trawld => self.caps,
            ProducerKind::Http | ProducerKind::Syslog => self.external,
        }
    }

    fn admission(&self) -> AdmissionState {
        AdmissionState::from_u8(self.mirror.load(Ordering::Acquire))
    }

    /// Settle and mirror. Call with the mutex held.
    fn publish(&self, state: &mut LedgerState, released: bool) {
        state.settle(self.caps, released);
        self.mirror
            .store(state.admission.as_u8(), Ordering::Release);
    }

    /// Latch `Refusing` for a lack-of-space refusal, without settling.
    /// Call with the mutex held.
    fn refuse(&self, state: &mut LedgerState) {
        state.admission = AdmissionState::Refusing;
        self.mirror
            .store(state.admission.as_u8(), Ordering::Release);
    }

    fn reserve(
        self: &Arc<Self>,
        producer: ProducerKind,
        charge: Charge,
    ) -> Result<Reservation, Refusal> {
        if charge.is_zero() {
            return Ok(Reservation::metered(Arc::clone(self), Charge::ZERO));
        }
        let ceiling = self.ceiling(producer);
        if !charge.fits(ceiling) {
            // Decided before occupancy: a request that can never fit is
            // oversized whatever the buffer holds, and it is no evidence
            // that compaction is behind.
            count_refusal(producer, Refusal::Oversized);
            return Err(Refusal::Oversized);
        }
        let admitted = {
            let mut state = self.state.lock();
            if let Some(total) = state
                .charged
                .checked_add(charge)
                .filter(|total| total.fits(ceiling))
            {
                state.charged = total;
                self.publish(&mut state, false);
                true
            } else {
                self.refuse(&mut state);
                false
            }
        };
        if admitted {
            // No pressure wake here: the WAL file does not exist yet, so a
            // compaction pass woken now would find nothing to drain.
            Ok(Reservation::metered(Arc::clone(self), charge))
        } else {
            count_refusal(producer, Refusal::Full);
            advance(&self.pressure);
            Err(Refusal::Full)
        }
    }

    fn ensure_free_space(&self, producer: ProducerKind) -> Result<(), Refusal> {
        let ceiling = self.ceiling(producer);
        if ceiling.events == 0 || ceiling.bytes == 0 {
            // No real batch (events and bytes) can fit this producer (a cap of 0 or
            // 1 for an external one): that is `Oversized`, not a full
            // buffer, and it must not latch `Refusing` on an empty ledger.
            count_refusal(producer, Refusal::Oversized);
            return Err(Refusal::Oversized);
        }
        let full = {
            let mut state = self.state.lock();
            let full =
                state.charged.events >= ceiling.events || state.charged.bytes >= ceiling.bytes;
            if full {
                self.refuse(&mut state);
            }
            full
        };
        if full {
            count_refusal(producer, Refusal::Full);
            advance(&self.pressure);
            Err(Refusal::Full)
        } else {
            Ok(())
        }
    }

    /// Give `charge` back and advance `released`.
    fn release(&self, charge: Charge) {
        if charge.is_zero() {
            return;
        }
        {
            let mut state = self.state.lock();
            debug_assert!(
                charge.fits(state.charged),
                "hot-buffer ledger release of {charge:?} exceeds charged {:?}",
                state.charged
            );
            state.charged = state.charged.saturating_sub(charge);
            self.publish(&mut state, true);
        }
        advance(&self.released);
    }

    /// Replace `held` with `actual` in the charged total, for an insert
    /// whose reservation does not match its batch (a caller bug, caught by
    /// `debug_assert` in debug builds). The ledger then charges exactly
    /// what becomes resident, keeping `charged == reserved + resident`. A
    /// shortfall is charged without a capacity check: the batch is already
    /// durable and must be accounted. Returns `actual`, the resident charge.
    fn reconcile(&self, held: Charge, actual: Charge) -> Charge {
        if held == actual {
            return actual;
        }
        let gave_back = actual.events < held.events || actual.bytes < held.bytes;
        {
            let mut state = self.state.lock();
            state.charged = state.charged.saturating_sub(held).saturating_add(actual);
            self.publish(&mut state, gave_back);
        }
        if gave_back {
            advance(&self.released);
        }
        actual
    }

    /// Charge without a capacity check, for [`HotBuffer::insert_evicting`]
    /// only: it evicts to make room and then must account what it stores.
    fn charge_unchecked(&self, charge: Charge) {
        let mut state = self.state.lock();
        state.charged = state.charged.saturating_add(charge);
        self.publish(&mut state, false);
    }

    fn charged(&self) -> Charge {
        self.state.lock().charged
    }
}

/// Bump a generation. Receivers compare generations, so a waiter that
/// subscribes before its attempt cannot miss a change that follows it.
fn advance(signal: &watch::Sender<u64>) {
    signal.send_modify(|generation| *generation = generation.wrapping_add(1));
}

/// Metrics only: this runs on the telemetry flush path.
#[allow(
    dead_code,
    reason = "#253 transition: producers adopt admission in later checkpoints"
)]
fn count_refusal(producer: ProducerKind, refusal: Refusal) {
    metrics::counter!(
        crate::metrics::HOT_BUFFER_ADMISSION_REFUSALS_TOTAL,
        "producer" => producer.as_str(),
        "kind" => refusal.label(),
    )
    .increment(1);
}

/// Admitted hot-buffer space, held until it is inserted or released.
///
/// Dropping a reservation releases whatever charge it still holds, so every
/// failure path (an error return, a panic unwinding through the holder, a
/// cancelled task) gives the space back. [`HotBuffer::insert`] consumes it
/// and moves the charge into the resident batch without charging again.
#[must_use = "dropping a Reservation releases its hot-buffer charge"]
pub struct Reservation {
    /// `None` for an unmetered reservation (a pipeline with no hot buffer)
    /// and once the charge has moved into a resident batch.
    ledger: Option<Arc<Ledger>>,
    charge: Charge,
}

impl std::fmt::Debug for Reservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reservation")
            .field("charge", &self.charge)
            .field("metered", &self.ledger.is_some())
            .finish()
    }
}

#[allow(
    dead_code,
    reason = "#253 transition: producers adopt admission in later checkpoints"
)]
impl Reservation {
    fn metered(ledger: Arc<Ledger>, charge: Charge) -> Self {
        Self {
            ledger: Some(ledger),
            charge,
        }
    }

    /// A reservation against no ledger: what a pipeline without a hot
    /// buffer hands out. Dropping it releases nothing.
    pub(crate) fn unmetered(charge: Charge) -> Self {
        Self {
            ledger: None,
            charge,
        }
    }

    /// The charge this reservation still holds.
    pub fn charge(&self) -> Charge {
        self.charge
    }

    /// Split `part` off into its own reservation, without re-checking
    /// capacity: the space is already admitted, only its ownership moves.
    ///
    /// # Panics
    ///
    /// If `part` exceeds the remaining charge on either dimension.
    pub fn take(&mut self, part: Charge) -> Reservation {
        self.charge = self.charge.checked_sub(part).unwrap_or_else(|| {
            panic!(
                "Reservation::take of {part:?} exceeds the remaining {:?}",
                self.charge
            )
        });
        Reservation {
            ledger: self.ledger.clone(),
            charge: part,
        }
    }

    /// Whether this reservation draws on `ledger`.
    fn is_for(&self, ledger: &Arc<Ledger>) -> bool {
        self.ledger
            .as_ref()
            .is_some_and(|own| Arc::ptr_eq(own, ledger))
    }

    /// Hand the charge to a resident batch: dropping `self` afterwards
    /// releases nothing.
    fn convert(&mut self) -> Charge {
        self.ledger = None;
        std::mem::replace(&mut self.charge, Charge::ZERO)
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if let Some(ledger) = self.ledger.take() {
            ledger.release(self.charge);
        }
    }
}

/// One resident batch and the charge it holds until drain.
#[derive(Debug)]
struct Resident {
    batch: Arc<IngestBatch>,
    charge: Charge,
    /// Monotonic insert time, for the oldest-batch age gauge.
    inserted: Instant,
}

/// An atomic view of the hot buffer for one query: the snapshot file and
/// the catalog pins that apply to it.
///
/// `field_types` is the catalog's pins intersected with the key set the
/// snapshot's events actually carry, computed fresh on every
/// [`HotBuffer::snapshot`] call, never cached per generation. Compaction
/// makes pins durable and refreshes the in-process cache before the atomic
/// rename publishes the conformant parquet, but the hot drain that bumps the
/// buffer generation happens after that rename; a generation-cached pin set
/// would be stale in between and the union would hard-error. Intersecting
/// with the observed keys also guarantees the emitter's `REPLACE` never names
/// a column absent from the snapshot.
#[derive(Debug, Clone)]
pub struct HotSnapshot {
    /// The ndjson snapshot file (shared across concurrent queries).
    pub file: Arc<tempfile::NamedTempFile>,
    /// Catalog pins ∩ observed keys — what the emitter conforms the hot
    /// branch of the union with.
    pub field_types: Arc<trawl_core::schema::FieldTypes>,
}

impl HotSnapshot {
    /// Path of the snapshot file (delegates to the temp file).
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        self.file.path()
    }
}

/// One cache slot: the generation it was built at, the snapshot file, and
/// the key set the file actually carries (the same pass that finds the
/// schema pioneers — pins are intersected against it on every call).
struct CachedSnapshot {
    generation: u64,
    file: Arc<tempfile::NamedTempFile>,
    keys: Vec<String>,
}

/// Batch-keyed in-memory event store.
///
/// Uses an `IndexMap` for insertion-order iteration (the first entry is
/// the oldest resident) keyed by batch id (`{env}/{WAL filename stem}`,
/// the shape compaction derives to drain what it just wrote).
pub struct HotBuffer {
    publication: Arc<crate::publication::PublicationGate>,
    batches: RwLock<IndexMap<Arc<str>, Resident>>,
    /// Resident events only; the ledger's charge also counts reservations.
    /// Invariant: resident <= charged <= full cap.
    total_events: AtomicUsize,
    /// Resident bytes only, like `total_events`.
    total_bytes: AtomicUsize,
    config: HotBufferConfig,
    ledger: Arc<Ledger>,
    /// Monotonic counter bumped on every mutation (insert, drain).
    /// Used to invalidate the snapshot cache.
    generation: AtomicU64,
    /// Cached snapshot, reused across concurrent queries when the buffer
    /// hasn't changed, avoiding O(events × queries) I/O. Old snapshots stay
    /// alive via Arc until all queries using them finish.
    snapshot_cache: Mutex<Option<CachedSnapshot>>,
    /// Shared in-process pin cache; empty for a catalog-less buffer
    /// (embedded mode, unit tests), which yields empty `field_types` on
    /// every snapshot.
    field_catalog: Arc<crate::catalog::FieldCatalog>,
}

impl std::fmt::Debug for HotBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HotBuffer")
            .field("total_events", &self.total_events.load(Ordering::Relaxed))
            .field("total_bytes", &self.total_bytes.load(Ordering::Relaxed))
            .field("generation", &self.generation.load(Ordering::Relaxed))
            .field("config", &self.config)
            .field("charged", &self.charged())
            .field("admission", &self.admission_state())
            .finish_non_exhaustive()
    }
}

impl HotBuffer {
    /// Create a new hot buffer with the given limits (catalog-less — every
    /// snapshot carries empty `field_types`).
    pub fn new(config: HotBufferConfig) -> Self {
        Self {
            publication: Arc::new(crate::publication::PublicationGate::new()),
            batches: RwLock::new(IndexMap::new()),
            total_events: AtomicUsize::new(0),
            total_bytes: AtomicUsize::new(0),
            ledger: Arc::new(Ledger::new(Charge {
                events: config.max_events,
                bytes: config.max_bytes,
            })),
            config,
            generation: AtomicU64::new(0),
            snapshot_cache: Mutex::new(None),
            field_catalog: Arc::new(crate::catalog::FieldCatalog::new()),
        }
    }

    /// Shared publication interlock for this buffer and its cold corpus.
    #[must_use]
    pub fn publication(&self) -> Arc<crate::publication::PublicationGate> {
        Arc::clone(&self.publication)
    }

    /// Attach the shared in-process pin cache; snapshots then carry the
    /// pins intersected with their observed key set.
    #[must_use]
    pub fn with_field_catalog(mut self, catalog: Arc<crate::catalog::FieldCatalog>) -> Self {
        self.field_catalog = catalog;
        self
    }

    /// Admit `charge` for `producer`, or refuse it without changing any
    /// state but the refusal counter (and, for [`Refusal::Full`], the
    /// latched [`AdmissionState::Refusing`] and a pressure wake).
    ///
    /// Synchronous, O(1), no I/O, no tracing. A zero charge is admitted
    /// without taking the lock. [`Refusal::Oversized`] is decided from the
    /// producer's ceiling alone, before occupancy is consulted.
    #[allow(
        dead_code,
        reason = "#253 transition: producers adopt admission in later checkpoints"
    )]
    pub(crate) fn reserve(
        &self,
        producer: ProducerKind,
        charge: Charge,
    ) -> Result<Reservation, Refusal> {
        self.ledger.reserve(producer, charge)
    }

    /// Refuse early, before any decompression or parsing, when `producer`
    /// has no free space at all: [`Refusal::Full`] iff the charged total is
    /// at or above the producer's ceiling on either dimension. Counts the
    /// refusal, latches `Refusing` and wakes compaction, like a `Full`
    /// [`reserve`](Self::reserve).
    ///
    /// A producer whose ceiling is zero on a dimension (a cap of 0 or 1 for
    /// an external producer) can fit no non-empty charge, so it gets
    /// [`Refusal::Oversized`] instead, with no state change.
    #[allow(
        dead_code,
        reason = "#253 transition: producers adopt admission in later checkpoints"
    )]
    pub(crate) fn ensure_free_space(&self, producer: ProducerKind) -> Result<(), Refusal> {
        self.ledger.ensure_free_space(producer)
    }

    /// The most `producer` may have charged at once: the full caps for
    /// self-telemetry, [`external_ceiling`] for everything else.
    #[allow(
        dead_code,
        reason = "#253 transition: producers adopt admission in later checkpoints"
    )]
    pub(crate) fn ceiling(&self, producer: ProducerKind) -> Charge {
        self.ledger.ceiling(producer)
    }

    /// Insert an admitted batch. Performs no capacity check and evicts
    /// nothing: the space was admitted by `reservation`, whose charge moves
    /// into the resident batch here and is released by [`drain`](Self::drain).
    ///
    /// The reservation must come from this buffer, hold exactly the batch's
    /// `(events.len(), byte_size)`, and the batch id must not already be
    /// resident. A duplicate id keeps the existing resident and releases
    /// the new reservation, so a resident is never silently replaced.
    ///
    /// # Panics
    ///
    /// If the reservation was not taken from this buffer (or is unmetered):
    /// its charge lives in another ledger, and no local repair keeps both
    /// ledgers exact. A charge that differs from the batch is a debug
    /// assertion; release builds reconcile the ledger to the batch's actual
    /// charge.
    pub fn insert(&self, mut reservation: Reservation, batch: Arc<IngestBatch>) {
        assert!(
            reservation.is_for(&self.ledger),
            "hot-buffer insert with a reservation from another ledger"
        );
        let actual = Charge {
            events: batch.events.len(),
            bytes: batch.byte_size,
        };
        debug_assert_eq!(
            reservation.charge(),
            actual,
            "hot-buffer insert whose reservation does not match the batch"
        );
        let inserted = {
            let mut map = self.batches.write();
            match map.entry(Arc::clone(&batch.batch_id)) {
                Entry::Occupied(_) => {
                    debug_assert!(false, "duplicate hot-buffer batch id {}", batch.batch_id);
                    false
                }
                Entry::Vacant(slot) => {
                    self.total_events
                        .fetch_add(batch.events.len(), Ordering::Relaxed);
                    self.total_bytes
                        .fetch_add(batch.byte_size, Ordering::Relaxed);
                    // Map lock, then ledger lock (inside `reconcile`).
                    let charge = self.ledger.reconcile(reservation.convert(), actual);
                    slot.insert(Resident {
                        batch,
                        charge,
                        inserted: Instant::now(),
                    });
                    true
                }
            }
        };
        // A duplicate's reservation still holds its charge: release it here,
        // outside the map lock.
        drop(reservation);
        if inserted {
            self.generation.fetch_add(1, Ordering::Relaxed);
            // The WAL file now exists, so a pressure pass has work to find.
            if self.ledger.admission() != AdmissionState::Open {
                advance(&self.ledger.pressure);
            }
        }
    }

    /// TRANSITIONAL (#253 CP1): the pre-admission insert, kept so producers
    /// not yet migrated to [`reserve`](Self::reserve) +
    /// [`insert`](Self::insert) keep compiling. Deleted once every producer
    /// reserves.
    ///
    /// If insertion would exceed either the event or byte limit,
    /// the oldest batches are evicted first (logged as warnings). The batch
    /// is charged to the ledger unchecked, so drain accounting stays exact.
    pub fn insert_evicting(&self, batch: Arc<IngestBatch>) {
        let event_count = batch.events.len();
        let byte_count = batch.byte_size;
        let charge = Charge {
            events: event_count,
            bytes: byte_count,
        };

        // Evict oldest batches if over either limit.
        {
            let mut map = self.batches.write();
            while self.over_limit(event_count, byte_count) {
                if let Some((evicted_id, evicted)) = map.shift_remove_index(0) {
                    self.total_events
                        .fetch_sub(evicted.batch.events.len(), Ordering::Relaxed);
                    self.total_bytes
                        .fetch_sub(evicted.batch.byte_size, Ordering::Relaxed);
                    self.ledger.release(evicted.charge);
                    tracing::warn!(
                        event_type = "hot_buffer_eviction",
                        batch_id = %evicted_id,
                        events = evicted.batch.events.len(),
                        bytes = evicted.batch.byte_size,
                        "hot buffer evicted batch (over limit)"
                    );
                } else {
                    break;
                }
            }

            self.ledger.charge_unchecked(charge);
            self.total_events.fetch_add(event_count, Ordering::Relaxed);
            self.total_bytes.fetch_add(byte_count, Ordering::Relaxed);
            let replaced = map.insert(
                Arc::clone(&batch.batch_id),
                Resident {
                    batch,
                    charge,
                    inserted: Instant::now(),
                },
            );
            if let Some(old) = replaced {
                self.total_events
                    .fetch_sub(old.batch.events.len(), Ordering::Relaxed);
                self.total_bytes
                    .fetch_sub(old.batch.byte_size, Ordering::Relaxed);
                self.ledger.release(old.charge);
            }
        }

        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// Check if adding `extra_events` / `extra_bytes` would exceed limits.
    fn over_limit(&self, extra_events: usize, extra_bytes: usize) -> bool {
        self.total_events.load(Ordering::Relaxed) + extra_events > self.config.max_events
            || self.total_bytes.load(Ordering::Relaxed) + extra_bytes > self.config.max_bytes
    }

    /// Insert without a producer's reservation, for tests: reserves as
    /// [`ProducerKind::Trawld`] (the full caps) and panics on refusal.
    #[cfg(any(test, feature = "test-support"))]
    pub fn insert_for_test(&self, batch: Arc<IngestBatch>) {
        let charge = Charge {
            events: batch.events.len(),
            bytes: batch.byte_size,
        };
        let reservation = self
            .reserve(ProducerKind::Trawld, charge)
            .unwrap_or_else(|refusal| panic!("insert_for_test refused {charge:?}: {refusal:?}"));
        self.insert(reservation, batch);
    }

    /// Remove batches that have been compacted to parquet, releasing their
    /// charge.
    ///
    /// Called after compaction writes parquet — the same batch IDs
    /// are drained from the hot buffer. The only way anything leaves the
    /// buffer. The release happens under the map lock (map, then ledger),
    /// so no observer sees a batch gone while its charge is still held.
    pub fn drain(&self, batch_ids: &[&str]) {
        let mut map = self.batches.write();
        let mut freed = Charge::ZERO;
        let mut removed: u64 = 0;
        for id in batch_ids {
            if let Some(resident) = map.shift_remove(*id) {
                self.total_events
                    .fetch_sub(resident.batch.events.len(), Ordering::Relaxed);
                self.total_bytes
                    .fetch_sub(resident.batch.byte_size, Ordering::Relaxed);
                freed = freed.saturating_add(resident.charge);
                removed += 1;
            }
        }
        if removed > 0 {
            self.ledger
                .drained_batches
                .fetch_add(removed, Ordering::Relaxed);
            self.ledger.release(freed);
        }
        drop(map);
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// The ledger's hysteresis state, lock-free.
    pub fn admission_state(&self) -> AdmissionState {
        self.ledger.admission()
    }

    /// Everything charged: outstanding reservations plus resident batches,
    /// every producer included.
    pub fn charged(&self) -> Charge {
        self.ledger.charged()
    }

    /// Age of the oldest resident batch on the monotonic clock, or `None`
    /// when the buffer is empty.
    pub fn oldest_batch_age(&self) -> Option<Duration> {
        self.batches
            .read()
            .first()
            .map(|(_, resident)| resident.inserted.elapsed())
    }

    /// Batches removed by [`drain`](Self::drain) since construction,
    /// monotonic. Compaction measures its progress by this delta.
    pub fn drained_batches(&self) -> u64 {
        self.ledger.drained_batches.load(Ordering::Relaxed)
    }

    /// A generation that advances after every release of charge (a dropped
    /// reservation, a drain). Drop the borrow before calling back into the
    /// buffer.
    pub fn subscribe_released(&self) -> watch::Receiver<u64> {
        self.ledger.released.subscribe()
    }

    /// A generation that advances on every `Full` refusal and on every
    /// insert while the state is not `Open`: the compaction pressure wake.
    /// Drop the borrow before calling back into the buffer.
    pub fn subscribe_pressure(&self) -> watch::Receiver<u64> {
        self.ledger.pressure.subscribe()
    }

    /// Get a snapshot of all buffered events as a temporary ndjson file,
    /// paired with the catalog pins that apply to it.
    ///
    /// Returns `None` if the buffer is empty.
    /// Uses a generation-based cache: concurrent queries against an unchanged
    /// buffer share a single snapshot file (1 disk write instead of N).
    /// The `Arc` ensures the temp file stays alive until all queries using it finish.
    ///
    /// The file is cached per generation; the pin intersection is not — see
    /// [`HotSnapshot`] for why a generation-cached pin set would be stale in
    /// the compaction rename-to-drain window. The intersect is O(observed
    /// keys) over an in-process map, negligible next to the query itself.
    ///
    /// The snapshot cache mutex is held for the entire check-then-build
    /// cycle to serialize concurrent misses — one thread builds while
    /// others wait ~40ms and get the cached result, preventing thundering
    /// herd I/O.
    pub fn snapshot(&self) -> Option<HotSnapshot> {
        // Fast path: no events at all → skip locking entirely.
        if self.total_events.load(Ordering::Relaxed) == 0 {
            return None;
        }

        let current_gen = self.generation.load(Ordering::Relaxed);

        let mut cache = self.snapshot_cache.lock();

        if let Some(cached) = cache.as_ref()
            && cached.generation == current_gen
        {
            return Some(HotSnapshot {
                field_types: Arc::new(self.pins_for(&cached.keys)),
                file: Arc::clone(&cached.file),
            });
        }

        // Cache miss — build under lock so concurrent queries wait.
        let (file, keys) = self.build_snapshot()?;
        let file = Arc::new(file);
        let field_types = Arc::new(self.pins_for(&keys));
        *cache = Some(CachedSnapshot {
            generation: current_gen,
            file: Arc::clone(&file),
            keys,
        });

        Some(HotSnapshot { file, field_types })
    }

    /// The pins that apply to one snapshot: the catalog intersected with
    /// the keys the file carries. An exact-name lookup on both sides:
    /// `ingest::envelope::canonicalize` folds every field name before it can
    /// reach [`HotBuffer::insert`], so key set and catalog agree on one
    /// spelling per `DuckDB` identifier.
    fn pins_for(&self, keys: &[String]) -> trawl_core::schema::FieldTypes {
        self.field_catalog
            .intersect(keys.iter().map(String::as_str))
    }

    /// Build a fresh snapshot file from the current buffer contents,
    /// returning it with the key set the file carries.
    ///
    /// Events are written schema-pioneers-first (see [`survey_schema`]) so
    /// that the reader can rely on `DuckDB`'s cheap default schema sample.
    ///
    /// The key set is ASCII-lowercase by construction, so no case merging
    /// happens here: the production callers of [`HotBuffer::insert`] and
    /// [`HotBuffer::insert_evicting`] are exactly `PipelineWriter`'s publish
    /// paths and telemetry's flush, and every event reaching them was
    /// canonicalized in `ingest::envelope::canonicalize`,
    /// the one door that folds field names. Test-only constructors that
    /// insert unfolded keys get the loud behaviour: an unnameable `x_1` twin
    /// column, not a silent merge.
    fn build_snapshot(&self) -> Option<(tempfile::NamedTempFile, Vec<String>)> {
        let map = self.batches.read();
        if map.is_empty() {
            return None;
        }

        let events: Vec<(&Arc<str>, &Event)> = map
            .values()
            .map(|resident| &resident.batch)
            .flat_map(|batch| batch.events.iter().map(move |e| (&batch.batch_id, e)))
            .collect();
        let (pioneer, keys) = survey_schema(events.iter().map(|(_, e)| *e));

        let mut tmpfile = tempfile::Builder::new().suffix(".ndjson").tempfile().ok()?;
        // serde_json emits many small writes per event. Buffer them before
        // crossing into the filesystem, then flush before publishing the file.
        let mut writer = BufWriter::new(&mut tmpfile);
        let mut wrote_any = false;

        let order = (0..events.len())
            .filter(|&i| pioneer[i])
            .chain((0..events.len()).filter(|&i| !pioneer[i]));
        for (batch_id, event) in order.map(|i| events[i]) {
            // Events are written verbatim: ingest canonicalization already
            // stringified top-level object/array values (ADR-0009), so every
            // value here is a scalar.
            //
            // Serialization failure is very unlikely (the event parsed during
            // ingest), but log and skip rather than poisoning the whole
            // snapshot.
            match serde_json::to_writer(&mut writer, event) {
                Ok(()) => {
                    if let Err(e) = writer.write_all(b"\n") {
                        tracing::error!(event_type = "hot_buffer_error", error = %e, "hot buffer snapshot write failed");
                        return None;
                    }
                    wrote_any = true;
                }
                Err(e) => {
                    tracing::error!(
                        event_type = "hot_buffer_error",
                        batch_id = %batch_id,
                        error = %e,
                        "failed to serialize event in hot buffer snapshot"
                    );
                }
            }
        }

        if !wrote_any {
            return None;
        }

        // Flush to ensure DuckDB can read the file.
        if let Err(e) = writer.flush() {
            tracing::error!(event_type = "hot_buffer_error", error = %e, "hot buffer snapshot flush failed");
            return None;
        }

        drop(writer);
        Some((tmpfile, keys))
    }

    /// Total number of events across all batches.
    pub fn event_count(&self) -> usize {
        self.total_events.load(Ordering::Relaxed)
    }

    /// Total estimated bytes across all batches.
    pub fn byte_count(&self) -> usize {
        self.total_bytes.load(Ordering::Relaxed)
    }

    /// Number of batches in the buffer.
    pub fn batch_count(&self) -> usize {
        self.batches.read().len()
    }

    /// Buffer configuration (max events, max bytes).
    pub fn config(&self) -> &HotBufferConfig {
        &self.config
    }
}

/// Survey the events in one pass: flag the schema pioneers and collect the
/// full observed key set.
///
/// `DuckDB`'s `read_json` auto-detection infers the schema from a bounded
/// prefix of the file (~20480 records) and then hard-errors (`unknown key`)
/// on any later record carrying a key outside it. `_repairs` is sparse by
/// construction (only repaired events carry it, ADR-0008) and the buffer
/// holds up to `max_events` (100k by default), so a single repaired event
/// past the prefix would break every query touching the hot buffer.
///
/// Detecting over the whole file (`sample_size=-1`) also cures that, but
/// costs a re-parse of the entire snapshot on every query and SSE poll:
/// ~2.7x the read (+135ms measured on a full 100k-event / 100 MiB buffer,
/// ~1s at half a million records), growing with the configured buffer size.
/// Writing the pioneers first puts the complete key set inside the detection
/// prefix for the price of one pass over the buffer, and does it with the
/// events' real values: an always-emitted null placeholder column would be
/// inferred as JSON, so `_repairs` would come back quoted and numeric fields
/// would stop being numbers.
///
/// A homogeneous buffer has exactly one pioneer (the first event), so the
/// snapshot order is unchanged in the common case.
///
/// The key set falls out of the same pass for free: the seen-set is the
/// union of every event's keys. The caller intersects catalog pins against
/// it, so the emitter's `REPLACE` can never name an absent column.
fn survey_schema<'a>(events: impl Iterator<Item = &'a Event>) -> (Vec<bool>, Vec<String>) {
    let mut seen: std::collections::HashSet<&'a str> = std::collections::HashSet::new();
    let pioneers = events
        .map(|event| {
            let mut novel = false;
            for key in event.keys() {
                novel |= seen.insert(key.as_str());
            }
            novel
        })
        .collect();
    let keys = seen.into_iter().map(str::to_owned).collect();
    (pioneers, keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_batch(id: &str, n: usize) -> Arc<IngestBatch> {
        let events: Vec<_> = (0..n)
            .map(|i| {
                let mut m = serde_json::Map::new();
                m.insert(
                    "message".into(),
                    serde_json::Value::String(format!("event_{i}")),
                );
                m.insert("service".into(), serde_json::Value::String("test".into()));
                m
            })
            .collect();
        Arc::new(IngestBatch {
            batch_id: id.into(),
            service: "test".into(),
            byte_size: n * 50, // rough estimate
            events,
        })
    }

    fn make_batch_with_bytes(id: &str, n: usize, byte_size: usize) -> Arc<IngestBatch> {
        let events: Vec<_> = (0..n)
            .map(|i| {
                let mut m = serde_json::Map::new();
                m.insert(
                    "message".into(),
                    serde_json::Value::String(format!("event_{i}")),
                );
                m.insert("service".into(), serde_json::Value::String("test".into()));
                m
            })
            .collect();
        Arc::new(IngestBatch {
            batch_id: id.into(),
            service: "test".into(),
            byte_size,
            events,
        })
    }

    #[test]
    fn insert_and_snapshot() {
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 1000,
            max_bytes: 10_000_000,
        });
        buf.insert_evicting(make_batch("batch_001", 3));
        assert_eq!(buf.event_count(), 3);
        assert_eq!(buf.batch_count(), 1);

        let tmpfile = buf.snapshot().expect("should have events");
        let content = std::fs::read_to_string(tmpfile.path()).unwrap();
        assert_eq!(content.lines().count(), 3);
        assert!(content.contains("event_0"));
        assert!(content.contains("event_2"));
    }

    /// Build `n` plain events plus one carrying the sparse `_repairs`
    /// key, with the sparse one last — the order a prefix-sampled read
    /// fails on unless the writer hoists the pioneer.
    fn events_with_trailing_sparse_key(n: usize) -> Vec<Event> {
        let mut events: Vec<Event> = (0..n)
            .map(|i| {
                let mut m = serde_json::Map::new();
                m.insert(
                    "_time".into(),
                    serde_json::Value::String("2024-01-15T10:00:00Z".into()),
                );
                m.insert(
                    "_ingested".into(),
                    serde_json::Value::String("2024-01-15T10:00:00Z".into()),
                );
                m.insert("service".into(), serde_json::Value::String("svc".into()));
                m.insert("message".into(), serde_json::Value::String(format!("m{i}")));
                m
            })
            .collect();
        let mut repaired = serde_json::Map::new();
        repaired.insert(
            "_time".into(),
            serde_json::Value::String("2024-01-15T10:00:00Z".into()),
        );
        repaired.insert(
            "_ingested".into(),
            serde_json::Value::String("2024-01-15T10:00:00Z".into()),
        );
        repaired.insert("service".into(), serde_json::Value::String("svc".into()));
        repaired.insert(
            "message".into(),
            serde_json::Value::String("repaired".into()),
        );
        repaired.insert(
            "_repairs".into(),
            serde_json::Value::String("time.from_ingest".into()),
        );
        events.push(repaired);
        events
    }

    #[test]
    fn snapshot_hoists_schema_pioneers_to_the_front() {
        // DuckDB infers the snapshot's schema from a bounded prefix and then
        // hard-errors on a later record with a key outside it. `_repairs`
        // is sparse by construction (ADR-0008), so the writer moves the events
        // that introduce a new key to the front — cheaper than making every
        // query re-detect over the whole file.
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 1000,
            max_bytes: 10_000_000,
        });
        let events = events_with_trailing_sparse_key(50);
        buf.insert_evicting(Arc::new(IngestBatch {
            batch_id: "b1".into(),
            service: "svc".into(),
            byte_size: 100,
            events,
        }));

        let tmpfile = buf.snapshot().expect("should have events");
        let content = std::fs::read_to_string(tmpfile.path()).unwrap();
        let lines: Vec<&str> = content.lines().collect();

        assert_eq!(
            lines.len(),
            51,
            "hoisting must not drop or duplicate events"
        );
        let sparse_at = lines
            .iter()
            .position(|l| l.contains("_repairs"))
            .expect("the repaired event must still be in the snapshot");
        assert!(
            sparse_at < 2,
            "the only event carrying the sparse key must be hoisted into the \
             schema-detection prefix, found at line {sparse_at}"
        );
        let messages: std::collections::HashSet<String> = lines
            .iter()
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l).unwrap()["message"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(messages.len(), 51, "every event must survive reordering");
    }

    #[test]
    fn sparse_key_survives_a_snapshot_larger_than_the_detection_prefix() {
        // End-to-end: a buffer bigger than DuckDB's ~20480-record JSON sample
        // whose only repaired event is the last one inserted. Both hot reader
        // call sites are exercised — the hot-only reader (no parquet yet) and
        // the hot+cold union.
        use trawl_engine::executor::Executor;

        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 100_000,
            max_bytes: 100 * 1024 * 1024,
        });
        buf.insert_evicting(Arc::new(IngestBatch {
            batch_id: "b1".into(),
            service: "svc".into(),
            byte_size: 1000,
            events: events_with_trailing_sparse_key(30_000),
        }));
        let snapshot = buf.snapshot().expect("should have events");
        let hot = snapshot.path().to_str().unwrap();

        for with_cold in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            if with_cold {
                let conn = duckdb::Connection::open_in_memory().unwrap();
                conn.execute_batch(&format!(
                    "COPY (SELECT CAST('2024-01-15 10:00:00' AS TIMESTAMP) AS \"timestamp\", \
                                  'svc' AS service, 'cold row' AS message) \
                     TO '{}' (FORMAT PARQUET)",
                    dir.path().join("cold.parquet").display()
                ))
                .unwrap();
            }

            let exec = Executor::new().expect("executor should initialize");
            let source = format!("{}/*.parquet", dir.path().display());
            let result = exec
                .run_query_with_hot(
                    "*",
                    &source,
                    hot,
                    &trawl_core::schema::FieldTypes::new(),
                    &trawl_core::schema::FieldTypes::new(),
                    usize::MAX,
                    0,
                )
                .expect("hot query must succeed");

            let col_names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
            assert!(
                col_names.contains(&"_repairs"),
                "the preserved original must survive (with_cold={with_cold}); \
                 got columns {col_names:?}"
            );
        }
    }

    #[test]
    fn snapshot_field_types_is_pins_intersect_observed_keys() {
        // The snapshot's pin set is the catalog intersected with the keys
        // the buffered events actually carry: a pin on a field no event has
        // must not reach the emitter (REPLACE on an absent column throws),
        // and an observed key without a pin contributes nothing.
        use trawl_core::schema::CanonicalType;

        let catalog = Arc::new(crate::catalog::FieldCatalog::new());
        catalog.replace([
            ("duration".to_string(), CanonicalType::BigInt),
            ("absent_field".to_string(), CanonicalType::Varchar),
        ]);
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 1000,
            max_bytes: 10_000_000,
        })
        .with_field_catalog(Arc::clone(&catalog));

        let mut ev = serde_json::Map::new();
        ev.insert("service".into(), serde_json::Value::String("svc".into()));
        ev.insert("duration".into(), serde_json::Value::from(42));
        buf.insert_evicting(Arc::new(IngestBatch {
            batch_id: "b1".into(),
            service: "svc".into(),
            byte_size: 100,
            events: vec![ev],
        }));

        let snap = buf.snapshot().expect("should have events");
        assert_eq!(
            snap.field_types.get("duration"),
            Some(CanonicalType::BigInt),
            "pinned + observed field must be conformed"
        );
        assert_eq!(
            snap.field_types.get("absent_field"),
            None,
            "a pin on a field no event carries must not reach the emitter"
        );
        assert_eq!(snap.field_types.len(), 1);
    }

    #[test]
    fn snapshot_reflects_new_pins_at_same_generation() {
        // Compaction makes pins durable and refreshes the cache before the
        // atomic rename publishes the conformant parquet, but the hot drain
        // that bumps the generation happens after it. A generation-cached pin
        // set would be stale in that window, and nothing retries a failed
        // union, so a stale set is a hard error. Pins are therefore
        // intersected on every snapshot() call, cache hit included.
        use trawl_core::schema::CanonicalType;

        let catalog = Arc::new(crate::catalog::FieldCatalog::new());
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 1000,
            max_bytes: 10_000_000,
        })
        .with_field_catalog(Arc::clone(&catalog));

        let mut ev = serde_json::Map::new();
        ev.insert("service".into(), serde_json::Value::String("svc".into()));
        ev.insert("duration".into(), serde_json::Value::from(42));
        buf.insert_evicting(Arc::new(IngestBatch {
            batch_id: "b1".into(),
            service: "svc".into(),
            byte_size: 100,
            events: vec![ev],
        }));

        let first = buf.snapshot().expect("should have events");
        assert!(
            first.field_types.is_empty(),
            "no pins yet — nothing to conform"
        );

        // Pin lands mid-window: no buffer mutation, same generation.
        catalog.replace([("duration".to_string(), CanonicalType::BigInt)]);

        let second = buf.snapshot().expect("should have events");
        assert!(
            Arc::ptr_eq(&first.file, &second.file),
            "same generation must reuse the cached snapshot file"
        );
        assert_eq!(
            second.field_types.get("duration"),
            Some(CanonicalType::BigInt),
            "a pin added between snapshots at the SAME generation must be \
             reflected immediately"
        );
    }

    #[test]
    fn drain_removes_batches() {
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 1000,
            max_bytes: 10_000_000,
        });
        buf.insert_evicting(make_batch("batch_001", 2));
        buf.insert_evicting(make_batch("batch_002", 3));
        assert_eq!(buf.event_count(), 5);

        buf.drain(&["batch_001"]);
        assert_eq!(buf.event_count(), 3);
        assert_eq!(buf.batch_count(), 1);

        buf.drain(&["batch_002"]);
        assert_eq!(buf.event_count(), 0);
        assert!(buf.snapshot().is_none());
    }

    #[test]
    fn eviction_removes_oldest_by_insertion_order() {
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 5,
            max_bytes: 10_000_000,
        });
        // Insert "zzz" first, then "aaa" — FIFO should evict "zzz" first
        // even though it sorts last lexicographically.
        buf.insert_evicting(make_batch("zzz_001", 3));
        buf.insert_evicting(make_batch("aaa_002", 3));
        // Inserting 3 more events when limit is 5: must evict zzz (oldest inserted).
        assert_eq!(buf.event_count(), 3);
        assert_eq!(buf.batch_count(), 1);

        // Only aaa_002 should remain.
        let tmpfile = buf.snapshot().unwrap();
        let content = std::fs::read_to_string(tmpfile.path()).unwrap();
        assert_eq!(content.lines().count(), 3);
    }

    #[test]
    fn eviction_by_bytes_limit() {
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 1_000_000, // effectively unlimited
            max_bytes: 500,
        });
        buf.insert_evicting(make_batch_with_bytes("batch_001", 2, 300));
        buf.insert_evicting(make_batch_with_bytes("batch_002", 2, 300));
        // 300 + 300 = 600 > 500, so batch_001 should be evicted.
        assert_eq!(buf.event_count(), 2);
        assert_eq!(buf.byte_count(), 300);
        assert_eq!(buf.batch_count(), 1);
    }

    #[test]
    fn drain_updates_byte_count() {
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 1000,
            max_bytes: 10_000_000,
        });
        buf.insert_evicting(make_batch_with_bytes("batch_001", 2, 200));
        buf.insert_evicting(make_batch_with_bytes("batch_002", 3, 400));
        assert_eq!(buf.byte_count(), 600);

        buf.drain(&["batch_001"]);
        assert_eq!(buf.byte_count(), 400);
    }

    #[test]
    fn snapshot_includes_all_batches() {
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 1000,
            max_bytes: 10_000_000,
        });
        buf.insert_evicting(make_batch("batch_001", 2));
        buf.insert_evicting(make_batch("batch_002", 3));

        let tmpfile = buf.snapshot().expect("should have events");
        let content = std::fs::read_to_string(tmpfile.path()).unwrap();
        // All 5 events from both batches should be in the snapshot.
        assert_eq!(content.lines().count(), 5);
    }

    #[test]
    fn drain_after_snapshot_leaves_snapshot_valid() {
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 1000,
            max_bytes: 10_000_000,
        });
        buf.insert_evicting(make_batch("batch_001", 2));
        buf.insert_evicting(make_batch("batch_002", 3));

        // Take a snapshot (Arc-wrapped temp file).
        let snapshot = buf.snapshot().expect("should have events");
        let content_before = std::fs::read_to_string(snapshot.path()).unwrap();
        assert_eq!(content_before.lines().count(), 5);

        // Drain batch_001 — simulates compaction finishing.
        buf.drain(&["batch_001"]);

        // The pre-drain snapshot file is still valid (Arc keeps it alive).
        let content_after = std::fs::read_to_string(snapshot.path()).unwrap();
        assert_eq!(content_after, content_before);
    }

    #[test]
    fn empty_buffer_returns_none() {
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 100,
            max_bytes: 10_000_000,
        });
        assert!(buf.snapshot().is_none());
    }

    #[test]
    fn drain_nonexistent_is_noop() {
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 100,
            max_bytes: 10_000_000,
        });
        buf.insert_evicting(make_batch("batch_001", 2));
        buf.drain(&["nonexistent"]);
        assert_eq!(buf.event_count(), 2);
    }

    // -- admission ledger ----------------------------------------------------

    use crate::ingest::producer::ProducerKind::{Http, Syslog, Trawld};

    fn ledger_buffer(events: usize, bytes: usize) -> HotBuffer {
        HotBuffer::new(HotBufferConfig {
            max_events: events,
            max_bytes: bytes,
        })
    }

    const fn charge(events: usize, bytes: usize) -> Charge {
        Charge { events, bytes }
    }

    /// A batch whose `(events.len(), byte_size)` is exactly `charge`.
    fn batch_of(id: &str, charge: Charge) -> Arc<IngestBatch> {
        Arc::new(IngestBatch {
            batch_id: id.into(),
            service: "test".into(),
            byte_size: charge.bytes,
            events: vec![serde_json::Map::new(); charge.events],
        })
    }

    #[test]
    fn external_ceiling_is_fifteen_sixteenths_of_each_default_cap() {
        let defaults = charge(
            trawl_config::DEFAULT_HOT_BUFFER_MAX_EVENTS,
            trawl_config::DEFAULT_HOT_BUFFER_MAX_BYTES,
        );
        assert_eq!(external_ceiling(defaults), charge(93_750, 98_304_000));
        // u128 arithmetic: the largest cap neither overflows nor rounds up.
        // floor((2^64 - 1) * 15 / 16) = 15 * 2^60 - 1.
        let max = external_ceiling(charge(usize::MAX, usize::MAX));
        assert_eq!(max.events, usize::MAX - usize::MAX / 16 - 1);
        assert_eq!(max.bytes, max.events);
        // Floor, not round: only a cap of 0 or 1 leaves external producers
        // nothing.
        assert_eq!(external_ceiling(charge(15, 15)), charge(14, 14));
        assert_eq!(external_ceiling(charge(1, 0)), Charge::ZERO);
        assert_eq!(external_ceiling(charge(16, 32)), charge(15, 30));
    }

    #[test]
    fn oversized_is_refused_on_an_empty_buffer_without_touching_state() {
        let recorder = crate::metrics::prometheus_builder().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            crate::metrics::init_operational_alert_metrics();
            let buf = ledger_buffer(160, 16_000);
            let mut pressure = buf.subscribe_pressure();
            let mut released = buf.subscribe_released();
            pressure.borrow_and_update();
            released.borrow_and_update();

            for too_big in [charge(151, 1), charge(1, 15_001)] {
                assert_eq!(buf.reserve(Http, too_big).unwrap_err(), Refusal::Oversized);
                assert_eq!(
                    buf.reserve(Syslog, too_big).unwrap_err(),
                    Refusal::Oversized
                );
            }
            assert_eq!(
                buf.reserve(Trawld, charge(161, 1)).unwrap_err(),
                Refusal::Oversized
            );
            assert_eq!(buf.charged(), Charge::ZERO);
            assert_eq!(buf.admission_state(), AdmissionState::Open);
            assert!(
                !pressure.has_changed().unwrap(),
                "oversized must not wake compaction"
            );
            assert!(!released.has_changed().unwrap());
            let series = |producer: &str, kind: &str| {
                crate::metrics::test_support::sample(
                    &handle,
                    &format!(
                        "trawl_hot_buffer_admission_refusals_total{{producer=\"{producer}\",kind=\"{kind}\"}}"
                    ),
                )
            };
            assert_eq!(series("http", "oversized"), 2);
            assert_eq!(series("syslog", "oversized"), 2);
            assert_eq!(series("trawld", "oversized"), 1);
            for producer in ["http", "syslog", "trawld"] {
                assert_eq!(series(producer, "full"), 0);
            }

            // What exactly fits the ceiling is admitted on an empty buffer.
            let fits = buf.reserve(Http, charge(150, 15_000)).unwrap();
            assert_eq!(buf.charged(), charge(150, 15_000));
            drop(fits);
            let whole = buf.reserve(Trawld, charge(160, 16_000)).unwrap();
            assert_eq!(buf.charged(), charge(160, 16_000));
            drop(whole);
            assert_eq!(buf.charged(), Charge::ZERO);
        });
    }

    #[test]
    fn trawld_may_fill_the_full_cap_while_external_producers_stop_at_fifteen_sixteenths() {
        let buf = ledger_buffer(160, 16_000);
        assert_eq!(buf.ceiling(Http), charge(150, 15_000));
        assert_eq!(buf.ceiling(Syslog), charge(150, 15_000));
        assert_eq!(buf.ceiling(Trawld), charge(160, 16_000));

        // Events dimension.
        let http = buf.reserve(Http, charge(150, 1)).unwrap();
        assert_eq!(buf.reserve(Http, charge(1, 0)).unwrap_err(), Refusal::Full);
        assert_eq!(
            buf.reserve(Syslog, charge(1, 0)).unwrap_err(),
            Refusal::Full
        );
        let trawld = buf.reserve(Trawld, charge(10, 1)).unwrap();
        assert_eq!(buf.charged(), charge(160, 2));
        assert_eq!(
            buf.reserve(Trawld, charge(1, 0)).unwrap_err(),
            Refusal::Full
        );
        drop((http, trawld));
        assert_eq!(buf.charged(), Charge::ZERO);

        // Bytes dimension.
        let http = buf.reserve(Http, charge(1, 15_000)).unwrap();
        assert_eq!(buf.reserve(Http, charge(0, 1)).unwrap_err(), Refusal::Full);
        let trawld = buf.reserve(Trawld, charge(1, 1_000)).unwrap();
        assert_eq!(
            buf.reserve(Trawld, charge(0, 1)).unwrap_err(),
            Refusal::Full
        );
        drop((http, trawld));

        // The predicate counts everything charged, telemetry included: the
        // reserve is a floor of opportunity for trawld, not an allocation.
        let trawld = buf.reserve(Trawld, charge(150, 0)).unwrap();
        assert_eq!(buf.reserve(Http, charge(1, 0)).unwrap_err(), Refusal::Full);
        drop(trawld);
        assert_eq!(buf.charged(), Charge::ZERO);
    }

    #[test]
    fn zero_charge_is_admitted_even_when_full() {
        let buf = ledger_buffer(16, 1_600);
        let full = buf.reserve(Trawld, charge(16, 1_600)).unwrap();
        let state = buf.admission_state();
        let mut released = buf.subscribe_released();
        released.borrow_and_update();
        let zero = buf.reserve(Http, Charge::ZERO).unwrap();
        assert!(zero.charge().is_zero());
        assert_eq!(buf.charged(), charge(16, 1_600));
        assert_eq!(buf.admission_state(), state);
        drop(zero);
        assert_eq!(buf.charged(), charge(16, 1_600));
        assert!(
            !released.has_changed().unwrap(),
            "a zero release frees nothing"
        );
        drop(full);
        assert_eq!(buf.charged(), Charge::ZERO);
    }

    /// Moves the ledger to an exact level through real reservations: up by
    /// reserving the difference as trawld, down by splitting it off a held
    /// reservation and dropping the split.
    struct Level<'a> {
        buf: &'a HotBuffer,
        held: Vec<Reservation>,
    }

    impl Level<'_> {
        fn to(&mut self, events: usize, bytes: usize) {
            let current = self.buf.charged();
            let up = charge(
                events.saturating_sub(current.events),
                bytes.saturating_sub(current.bytes),
            );
            if !up.is_zero() {
                self.held.push(
                    self.buf
                        .reserve(Trawld, up)
                        .expect("level fits the full cap"),
                );
            }
            let mut down = charge(
                current.events.saturating_sub(events),
                current.bytes.saturating_sub(bytes),
            );
            for held in self.held.iter_mut().rev() {
                let part = charge(
                    down.events.min(held.charge().events),
                    down.bytes.min(held.charge().bytes),
                );
                drop(held.take(part));
                down = down.checked_sub(part).unwrap();
            }
            assert!(down.is_zero(), "held reservations cover every release");
            self.held.retain(|held| !held.charge().is_zero());
            assert_eq!(self.buf.charged(), charge(events, bytes));
        }
    }

    #[test]
    fn hysteresis_boundary_table() {
        use AdmissionState::{Open, Pressure, Refusing};
        // Caps 100 events / 1000 bytes: enter at >= 50 | >= 500, exit only
        // below 25 & below 250. External ceiling: 93 events / 937 bytes.
        enum Step {
            Level(usize, usize),
            /// An HTTP reservation of this many events that must be refused
            /// `Full`; nothing is charged.
            RefuseHttp(usize),
        }
        use Step::{Level as L, RefuseHttp};
        let table: &[(Step, AdmissionState, &str)] = &[
            (L(49, 499), Open, "just below enter on both"),
            (L(50, 499), Pressure, "events reach half"),
            (L(49, 499), Pressure, "no exit just below enter"),
            (L(25, 249), Pressure, "events at a quarter is not below it"),
            (L(24, 249), Open, "below a quarter of both"),
            (L(24, 500), Pressure, "bytes reach half"),
            (L(24, 250), Pressure, "bytes at a quarter is not below it"),
            (L(24, 249), Open, "below a quarter of both again"),
            (L(0, 0), Open, "empty"),
            (L(90, 0), Pressure, "high occupancy"),
            (RefuseHttp(4), Refusing, "94 > 93: full refusal latches"),
            (L(60, 0), Refusing, "latched, never decays to pressure"),
            (L(25, 0), Refusing, "a quarter is not below it"),
            (L(24, 0), Open, "exit clears refusing"),
            (L(40, 0), Open, "below enter stays open"),
            (
                RefuseHttp(60),
                Refusing,
                "a refusal below the enter threshold still latches",
            ),
            (L(24, 0), Open, "exit"),
            (L(10, 0), Open, "low occupancy"),
            (
                RefuseHttp(90),
                Refusing,
                "a full refusal latches even below the exit threshold",
            ),
            (L(11, 0), Refusing, "a charge never clears refusing"),
            (L(10, 0), Open, "the next release below the exit clears it"),
        ];
        let buf = ledger_buffer(100, 1_000);
        let mut level = Level {
            buf: &buf,
            held: Vec::new(),
        };
        for (i, (step, expected, why)) in table.iter().enumerate() {
            match step {
                L(events, bytes) => level.to(*events, *bytes),
                RefuseHttp(events) => {
                    let before = buf.charged();
                    assert_eq!(
                        buf.reserve(Http, charge(*events, 0)).unwrap_err(),
                        Refusal::Full,
                        "row {i}: {why}"
                    );
                    assert_eq!(buf.charged(), before, "row {i}: a refusal charges nothing");
                }
            }
            assert_eq!(buf.admission_state(), *expected, "row {i}: {why}");
        }
        level.to(0, 0);
        assert_eq!(buf.admission_state(), Open);
    }

    #[test]
    fn ensure_free_space_refuses_only_at_or_above_the_ceiling() {
        let recorder = crate::metrics::prometheus_builder().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            crate::metrics::init_operational_alert_metrics();
            let buf = ledger_buffer(100, 1_000);
            let mut pressure = buf.subscribe_pressure();
            pressure.borrow_and_update();
            let mut level = Level {
                buf: &buf,
                held: Vec::new(),
            };
            level.to(92, 0);
            assert_eq!(buf.ensure_free_space(Http), Ok(()));
            assert!(!pressure.has_changed().unwrap());
            level.to(93, 0);
            assert_eq!(buf.ensure_free_space(Http), Err(Refusal::Full));
            assert_eq!(buf.ensure_free_space(Syslog), Err(Refusal::Full));
            assert_eq!(buf.admission_state(), AdmissionState::Refusing);
            assert!(
                pressure.has_changed().unwrap(),
                "a full refusal wakes compaction"
            );
            assert_eq!(
                buf.ensure_free_space(Trawld),
                Ok(()),
                "trawld keeps its floor"
            );
            level.to(0, 937);
            assert_eq!(
                buf.ensure_free_space(Http),
                Err(Refusal::Full),
                "bytes alone"
            );
            level.to(0, 0);
            assert_eq!(buf.admission_state(), AdmissionState::Open);
            assert_eq!(
                crate::metrics::test_support::sample(
                    &handle,
                    "trawl_hot_buffer_admission_refusals_total{producer=\"http\",kind=\"full\"}"
                ),
                2
            );

            // A cap of 1 leaves external producers a zero ceiling: that
            // can never fit, so it is oversized and latches nothing.
            let tiny = ledger_buffer(1, 1_000);
            assert_eq!(tiny.ensure_free_space(Http), Err(Refusal::Oversized));
            assert_eq!(tiny.admission_state(), AdmissionState::Open);
            assert_eq!(tiny.ensure_free_space(Trawld), Ok(()));
        });
    }

    #[test]
    fn pressure_wakes_on_insert_while_not_open_and_on_full_refusal_only() {
        let buf = ledger_buffer(100, 1_000);
        let mut pressure = buf.subscribe_pressure();
        pressure.borrow_and_update();

        // Open: an insert is not a pressure signal.
        let small = buf.reserve(Syslog, charge(10, 0)).unwrap();
        buf.insert(small, batch_of("env/small", charge(10, 0)));
        assert!(!pressure.has_changed().unwrap());

        // A successful reserve that enters pressure does not wake: the WAL
        // file does not exist yet.
        let big = buf.reserve(Syslog, charge(50, 0)).unwrap();
        assert_eq!(buf.admission_state(), AdmissionState::Pressure);
        assert!(!pressure.has_changed().unwrap());
        buf.insert(big, batch_of("env/big", charge(50, 0)));
        assert!(
            pressure.has_changed().unwrap(),
            "insert while in pressure wakes"
        );
        pressure.borrow_and_update();

        assert_eq!(buf.reserve(Http, charge(40, 0)).unwrap_err(), Refusal::Full);
        assert!(pressure.has_changed().unwrap(), "full refusal wakes");
        pressure.borrow_and_update();

        buf.drain(&["env/big", "env/small"]);
        assert_eq!(buf.admission_state(), AdmissionState::Open);
        assert!(
            !pressure.has_changed().unwrap(),
            "drain is not a pressure signal"
        );
    }

    #[test]
    fn dropped_reservation_releases_its_charge() {
        let buf = ledger_buffer(100, 1_000);
        let _base = buf.reserve(Syslog, charge(3, 30)).unwrap();
        let before = buf.charged();
        let mut released = buf.subscribe_released();
        released.borrow_and_update();
        let reservation = buf.reserve(Http, charge(10, 100)).unwrap();
        assert_eq!(buf.charged(), before.checked_add(charge(10, 100)).unwrap());
        drop(reservation);
        assert_eq!(buf.charged(), before);
        assert!(
            released.has_changed().unwrap(),
            "a release advances the generation"
        );
    }

    #[test]
    fn panic_unwinding_through_the_holder_releases_its_charge() {
        let buf = ledger_buffer(100, 1_000);
        let before = buf.charged();
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = buf.reserve(Http, charge(10, 100)).unwrap();
            assert_eq!(buf.charged(), charge(10, 100));
            panic!("writer failed while holding a reservation");
        }));
        assert!(unwound.is_err());
        assert_eq!(buf.charged(), before);
        assert_eq!(buf.admission_state(), AdmissionState::Open);
    }

    #[tokio::test]
    async fn aborted_task_holding_a_reservation_releases_its_charge() {
        let buf = Arc::new(ledger_buffer(100, 1_000));
        let before = buf.charged();
        let (held_tx, held_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn({
            let buf = Arc::clone(&buf);
            async move {
                let _held = buf.reserve(Http, charge(10, 100)).unwrap();
                held_tx.send(()).unwrap();
                std::future::pending::<()>().await;
            }
        });
        held_rx.await.unwrap();
        assert_eq!(buf.charged(), charge(10, 100));
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(buf.charged(), before);
    }

    #[test]
    fn insert_converts_without_double_charge_and_drain_releases_it() {
        let buf = ledger_buffer(100, 1_000);
        let mut released = buf.subscribe_released();
        let reservation = buf.reserve(Syslog, charge(4, 40)).unwrap();
        assert_eq!(buf.charged(), charge(4, 40));
        buf.insert(reservation, batch_of("prod/a", charge(4, 40)));
        assert_eq!(
            buf.charged(),
            charge(4, 40),
            "insert moves the charge, never adds"
        );
        assert_eq!((buf.event_count(), buf.byte_count()), (4, 40));
        assert!(buf.oldest_batch_age().is_some());
        released.borrow_and_update();

        buf.drain(&["prod/absent"]);
        assert_eq!(buf.drained_batches(), 0, "only removed batches count");
        assert_eq!(buf.charged(), charge(4, 40));
        assert!(!released.has_changed().unwrap());

        buf.drain(&["prod/a"]);
        assert_eq!(buf.charged(), Charge::ZERO);
        assert_eq!((buf.event_count(), buf.byte_count()), (0, 0));
        assert_eq!(buf.drained_batches(), 1);
        assert!(buf.oldest_batch_age().is_none());
        assert!(released.has_changed().unwrap(), "drain advances released");
    }

    #[test]
    fn take_splits_ownership_and_each_part_releases_on_drop() {
        let buf = ledger_buffer(100, 1_000);
        let mut whole = buf.reserve(Http, charge(10, 100)).unwrap();
        let part = whole.take(charge(4, 40));
        assert_eq!(part.charge(), charge(4, 40));
        assert_eq!(whole.charge(), charge(6, 60));
        assert_eq!(buf.charged(), charge(10, 100), "a split charges nothing");
        drop(part);
        assert_eq!(buf.charged(), charge(6, 60));
        // A taken share converts on insert; the remainder still releases.
        let share = whole.take(charge(2, 20));
        buf.insert(share, batch_of("prod/share", charge(2, 20)));
        drop(whole);
        assert_eq!(buf.charged(), charge(2, 20));
        buf.drain(&["prod/share"]);
        assert_eq!(buf.charged(), Charge::ZERO);
    }

    #[test]
    #[should_panic(expected = "exceeds the remaining")]
    fn take_more_than_remains_panics() {
        let buf = ledger_buffer(100, 1_000);
        let mut whole = buf.reserve(Http, charge(1, 10)).unwrap();
        let _ = whole.take(charge(2, 0));
    }

    #[test]
    fn oldest_batch_age_follows_the_first_resident() {
        let buf = ledger_buffer(100, 1_000);
        buf.insert_for_test(batch_of("prod/first", charge(1, 1)));
        std::thread::sleep(Duration::from_millis(20));
        buf.insert_for_test(batch_of("prod/second", charge(1, 1)));
        let oldest = buf.oldest_batch_age().unwrap();
        assert!(oldest >= Duration::from_millis(20), "{oldest:?}");
        buf.drain(&["prod/first"]);
        assert!(buf.oldest_batch_age().unwrap() < oldest);
    }

    #[test]
    fn concurrent_reservations_never_exceed_either_cap() {
        use rand::{Rng as _, SeedableRng as _};
        use std::sync::atomic::AtomicBool;

        const CAPS: Charge = charge(64, 4_096);
        const WORKERS: u64 = 8;
        const ROUNDS: usize = 20_000;
        let buf = ledger_buffer(CAPS.events, CAPS.bytes);
        let running = AtomicBool::new(true);
        let samples = AtomicUsize::new(0);
        let outcomes = [
            AtomicUsize::new(0),
            AtomicUsize::new(0),
            AtomicUsize::new(0),
        ];
        std::thread::scope(|scope| {
            let observer = scope.spawn(|| {
                while running.load(Ordering::Acquire) {
                    let charged = buf.charged();
                    assert!(
                        charged.fits(CAPS),
                        "charged {charged:?} exceeds caps {CAPS:?}"
                    );
                    samples.fetch_add(1, Ordering::Relaxed);
                }
            });
            let workers: Vec<_> = (0..WORKERS)
                .map(|worker| {
                    let (buf, outcomes) = (&buf, &outcomes);
                    scope.spawn(move || {
                        let mut rng = rand::rngs::StdRng::seed_from_u64(0x253 + worker);
                        let mut resident: Vec<String> = Vec::new();
                        for round in 0..ROUNDS {
                            let producer = [Http, Syslog, Trawld][rng.gen_range(0..3)];
                            let requested = if rng.gen_bool(0.05) {
                                charge(rng.gen_range(0..=80), rng.gen_range(0..=5_000))
                            } else {
                                charge(rng.gen_range(0..=20), rng.gen_range(0..=1_500))
                            };
                            match buf.reserve(producer, requested) {
                                Ok(mut reservation) => {
                                    outcomes[0].fetch_add(1, Ordering::Relaxed);
                                    let id = format!("w{worker}/{round}");
                                    match rng.gen_range(0..3) {
                                        0 => drop(reservation),
                                        1 => {
                                            buf.insert(reservation, batch_of(&id, requested));
                                            resident.push(id);
                                        }
                                        _ => {
                                            let part = charge(
                                                rng.gen_range(0..=requested.events),
                                                rng.gen_range(0..=requested.bytes),
                                            );
                                            let share = reservation.take(part);
                                            drop(reservation);
                                            buf.insert(share, batch_of(&id, part));
                                            resident.push(id);
                                        }
                                    }
                                }
                                Err(Refusal::Oversized) => {
                                    outcomes[1].fetch_add(1, Ordering::Relaxed);
                                    assert!(!requested.fits(buf.ceiling(producer)));
                                }
                                Err(Refusal::Full) => {
                                    outcomes[2].fetch_add(1, Ordering::Relaxed);
                                    assert!(requested.fits(buf.ceiling(producer)));
                                }
                            }
                            if rng.gen_bool(0.3) && !resident.is_empty() {
                                let ids: Vec<&str> = resident.iter().map(String::as_str).collect();
                                buf.drain(&ids);
                                resident.clear();
                            }
                        }
                        let ids: Vec<&str> = resident.iter().map(String::as_str).collect();
                        buf.drain(&ids);
                    })
                })
                .collect();
            for worker in workers {
                worker.join().unwrap();
            }
            running.store(false, Ordering::Release);
            observer.join().unwrap();
        });
        assert!(samples.load(Ordering::Relaxed) > 0);
        for (i, outcome) in outcomes.iter().enumerate() {
            assert!(
                outcome.load(Ordering::Relaxed) > 0,
                "outcome {i} (admitted, oversized, full) never exercised"
            );
        }
        assert_eq!(buf.charged(), Charge::ZERO);
        assert_eq!(
            (buf.event_count(), buf.byte_count(), buf.batch_count()),
            (0, 0, 0)
        );
        assert_eq!(buf.admission_state(), AdmissionState::Open);
    }

    #[test]
    #[should_panic(expected = "reservation from another ledger")]
    fn insert_refuses_a_reservation_from_another_ledger() {
        let ours = ledger_buffer(100, 1_000);
        let theirs = ledger_buffer(100, 1_000);
        let foreign = theirs.reserve(Http, charge(1, 1)).unwrap();
        ours.insert(foreign, batch_of("prod/foreign", charge(1, 1)));
    }

    #[test]
    #[should_panic(expected = "reservation from another ledger")]
    fn insert_refuses_an_unmetered_reservation() {
        let buf = ledger_buffer(100, 1_000);
        buf.insert(
            Reservation::unmetered(charge(1, 1)),
            batch_of("prod/unmetered", charge(1, 1)),
        );
    }

    #[test]
    fn reconcile_charges_exactly_the_resident_batch() {
        let buf = ledger_buffer(100, 1_000);
        let ledger = &buf.ledger;
        let _other = buf.reserve(Syslog, charge(5, 50)).unwrap();
        let mut held = buf.reserve(Http, charge(10, 100)).unwrap();
        let mut released = buf.subscribe_released();
        released.borrow_and_update();

        // Matching charge: nothing moves.
        assert_eq!(
            ledger.reconcile(charge(10, 100), charge(10, 100)),
            charge(10, 100)
        );
        assert_eq!(buf.charged(), charge(15, 150));
        assert!(!released.has_changed().unwrap());

        // Mismatch on both sides: fewer events, more bytes than reserved.
        let resident = ledger.reconcile(held.convert(), charge(7, 120));
        assert_eq!(resident, charge(7, 120));
        assert_eq!(
            buf.charged(),
            charge(12, 170),
            "charged == other reservation + actual resident charge"
        );
        assert!(
            released.has_changed().unwrap(),
            "the event shortfall is released"
        );
        drop(held);
        assert_eq!(
            buf.charged(),
            charge(12, 170),
            "a converted reservation releases nothing"
        );

        // Releasing the resident charge brings the ledger back exactly.
        ledger.release(resident);
        assert_eq!(buf.charged(), charge(5, 50));
    }
}
