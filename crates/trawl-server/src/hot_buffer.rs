// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Hot buffer: batch-keyed in-memory event store for query freshness.
//!
//! Events stay in the hot buffer until compaction writes parquet and
//! calls [`drain`](HotBuffer::drain). The shared publication guard keeps
//! query and export readers outside the interval between the canonical
//! file rename and hot drain, so they cannot count both copies.
//!
//! # Admission (ADR-0043)
//!
//! Lock order is always batch map, then ledger. Nothing here traces from
//! [`HotBuffer::reserve`], [`HotBuffer::insert`] or under the ledger lock:
//! self-telemetry's own flush passes through them, so a log line there is
//! an ingestion loop. The one exception is the duplicate-batch-id ERROR in
//! `insert`, a writer bug: it is emitted after every lock is released, and
//! the telemetry batch it produces carries a fresh WAL name, so it cannot
//! recur on its own.

use std::io::{BufWriter, Write as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use indexmap::map::Entry;
use parking_lot::{Mutex, RwLock};
use tokio::sync::watch;
use trawl_engine::scratch::QueryScratch;

use crate::bus::IngestBatch;
use crate::ingest::producer::ProducerKind;

type Event = serde_json::Map<String, serde_json::Value>;

#[derive(Debug, Clone)]
pub struct HotBufferConfig {
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

/// The share of each cap that external producers (HTTP, syslog) may fill,
/// as `(numerator, denominator)`.
pub const EXTERNAL_SHARE: (usize, usize) = (15, 16);

pub const PRESSURE_ENTER: (usize, usize) = (1, 2);

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

    pub const fn is_zero(self) -> bool {
        self.events == 0 && self.bytes == 0
    }

    pub const fn fits(self, ceiling: Self) -> bool {
        self.events <= ceiling.events && self.bytes <= ceiling.bytes
    }

    pub fn checked_add(self, other: Self) -> Option<Self> {
        Some(Self {
            events: self.events.checked_add(other.events)?,
            bytes: self.bytes.checked_add(other.bytes)?,
        })
    }

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

pub fn external_ceiling(caps: Charge) -> Charge {
    Charge {
        events: share(caps.events, EXTERNAL_SHARE),
        bytes: share(caps.bytes, EXTERNAL_SHARE),
    }
}

fn share(value: usize, (num, den): (usize, usize)) -> usize {
    let scaled = value as u128 * num as u128 / den as u128;
    usize::try_from(scaled).unwrap_or(usize::MAX)
}

fn at_or_above(value: usize, cap: usize, (num, den): (usize, usize)) -> bool {
    value as u128 * den as u128 >= cap as u128 * num as u128
}

fn below(value: usize, cap: usize, (num, den): (usize, usize)) -> bool {
    value as u128 * (den as u128) < cap as u128 * num as u128
}

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

    pub const fn label(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Oversized => "oversized",
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionState {
    Open = 0,
    Pressure = 1,
    Refusing = 2,
}

impl AdmissionState {
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

#[derive(Debug)]
struct LedgerState {
    /// Reserved plus resident, every producer included.
    charged: Charge,
    admission: AdmissionState,
}

impl LedgerState {
    /// Run under the mutex after every charge and release, never after a
    /// refusal. `Refusing` is set only by a refusal and leaves only through
    /// the exit, so it never decays to `Pressure`.
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

struct Ledger {
    caps: Charge,
    external: Charge,
    state: Mutex<LedgerState>,
    /// Lock-free copy of `state.admission`, written under the mutex.
    mirror: AtomicU8,
    released: watch::Sender<u64>,
    pressure: watch::Sender<u64>,
    drained_batches: AtomicU64,
    inserted_batches: AtomicU64,
}

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
            inserted_batches: AtomicU64::new(0),
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

    /// For an insert whose reservation does not match its batch (a caller bug).
    /// A shortfall is charged without a capacity check: the batch is already
    /// durable and must be accounted.
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

    fn charged(&self) -> Charge {
        self.state.lock().charged
    }
}

/// Call with no hot-buffer lock held.
fn duplicate_batch(batch: &IngestBatch) {
    metrics::counter!(crate::metrics::HOT_BUFFER_DUPLICATE_BATCHES_TOTAL).increment(1);
    tracing::error!(
        event_type = "hot_buffer_duplicate_batch",
        service = %batch.service,
        events = batch.events.len(),
        "hot-buffer batch id already resident; kept the resident and dropped the new batch"
    );
}

/// Bump a generation. Receivers compare generations, so a waiter that
/// subscribes before its attempt cannot miss a change that follows it.
fn advance(signal: &watch::Sender<u64>) {
    signal.send_modify(|generation| *generation = generation.wrapping_add(1));
}

/// Metrics only: this runs on the telemetry flush path.
fn count_refusal(producer: ProducerKind, refusal: Refusal) {
    metrics::counter!(
        crate::metrics::HOT_BUFFER_ADMISSION_REFUSALS_TOTAL,
        "producer" => producer.as_str(),
        "kind" => refusal.label(),
    )
    .increment(1);
}

/// Admitted hot-buffer space, held until it is inserted or released.
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

impl Reservation {
    fn metered(ledger: Arc<Ledger>, charge: Charge) -> Self {
        Self {
            ledger: Some(ledger),
            charge,
        }
    }

    pub(crate) fn unmetered(charge: Charge) -> Self {
        Self {
            ledger: None,
            charge,
        }
    }

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

    fn is_for(&self, ledger: &Arc<Ledger>) -> bool {
        self.ledger
            .as_ref()
            .is_some_and(|own| Arc::ptr_eq(own, ledger))
    }

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

#[derive(Debug)]
struct Resident {
    batch: Arc<IngestBatch>,
    charge: Charge,
    inserted: Instant,
    /// Age the batch already had when it became resident: zero for an
    /// insert, the WAL file's age for a hydrated batch.
    backdate: Duration,
}

impl Resident {
    fn age(&self) -> Duration {
        self.inserted.elapsed().saturating_add(self.backdate)
    }
}

#[derive(Debug)]
pub(crate) struct HydratedBatch {
    /// The batch under its existing id (`{env}/{WAL filename stem}`); its
    /// `(events.len(), byte_size)` is the charge.
    pub batch: Arc<IngestBatch>,
    /// How old the WAL file already is, from the time in its name. The
    /// caller gives zero for a name in the future.
    pub age: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HydrateError {
    #[error("the hot buffer was already hydrated")]
    AlreadyHydrated,
    /// Something is already charged: a resident batch or a reservation.
    #[error("the hot buffer is not empty")]
    NotEmpty,
    #[error("the hydration plan exceeds the hot-buffer caps")]
    ExceedsCaps,
}

/// `field_types` is computed fresh on every [`HotBuffer::snapshot`] call,
/// never cached per generation: the hot drain that bumps the generation
/// happens after compaction's rename publishes the conformant parquet, so
/// a generation-cached pin set would be stale in between and the union
/// would hard-error.
#[derive(Debug, Clone)]
pub struct HotSnapshot {
    pub file: Arc<tempfile::NamedTempFile>,
    pub field_types: Arc<trawl_core::schema::FieldTypes>,
    /// Every key the file's events carry and, among them, the keys whose
    /// every value is JSON `null` (ADR-0062). Built once per generation by
    /// the same survey that writes the file, so the evidence describes
    /// exactly the file it travels with.
    pub columns: Arc<trawl_core::emitter::HotColumns>,
}

impl HotSnapshot {
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        self.file.path()
    }
}

struct CachedSnapshot {
    generation: u64,
    file: Arc<tempfile::NamedTempFile>,
    keys: Vec<String>,
    columns: Arc<trawl_core::emitter::HotColumns>,
    #[cfg(any(test, feature = "test-support"))]
    events: usize,
}

/// One answered [`HotBuffer::snapshot`] call, as the test recorder logs it
/// ([`HotBuffer::snapshots_for_test`]).
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotRecord {
    pub generation: u64,
    /// The events the answer's file holds: the resident events at the
    /// instant it was built. Zero for an empty buffer's `None`.
    pub events: usize,
}

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
    hydrated: AtomicBool,
    generation: AtomicU64,
    snapshot_cache: Mutex<Option<CachedSnapshot>>,
    field_catalog: Arc<crate::catalog::FieldCatalog>,
    #[cfg(any(test, feature = "test-support"))]
    fail_next_snapshot: AtomicBool,
    #[cfg(any(test, feature = "test-support"))]
    snapshot_log: Mutex<Vec<SnapshotRecord>>,
    #[cfg(any(test, feature = "test-support"))]
    built_events: AtomicUsize,
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
            hydrated: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            snapshot_cache: Mutex::new(None),
            field_catalog: Arc::new(crate::catalog::FieldCatalog::new()),
            #[cfg(any(test, feature = "test-support"))]
            fail_next_snapshot: AtomicBool::new(false),
            #[cfg(any(test, feature = "test-support"))]
            snapshot_log: Mutex::new(Vec::new()),
            #[cfg(any(test, feature = "test-support"))]
            built_events: AtomicUsize::new(0),
        }
    }

    #[must_use]
    pub fn publication(&self) -> Arc<crate::publication::PublicationGate> {
        Arc::clone(&self.publication)
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_publication_for_test(
        mut self,
        publication: Arc<crate::publication::PublicationGate>,
    ) -> Self {
        self.publication = publication;
        self
    }

    #[must_use]
    pub fn with_field_catalog(mut self, catalog: Arc<crate::catalog::FieldCatalog>) -> Self {
        self.field_catalog = catalog;
        self
    }

    /// Replace the settled gate [`new`](Self::new) builds, before the buffer
    /// is shared. An ingest node's buffer gets a gate born
    /// [`starting`](crate::publication::PublicationGate::starting), so reads
    /// refuse until boot hydration finishes (ADR-0041 slice 2).
    #[must_use]
    pub fn with_publication(mut self, gate: crate::publication::PublicationGate) -> Self {
        self.publication = Arc::new(gate);
        self
    }

    pub(crate) fn reserve(
        &self,
        producer: ProducerKind,
        charge: Charge,
    ) -> Result<Reservation, Refusal> {
        self.ledger.reserve(producer, charge)
    }

    pub(crate) fn ensure_free_space(&self, producer: ProducerKind) -> Result<(), Refusal> {
        self.ledger.ensure_free_space(producer)
    }

    pub(crate) fn ceiling(&self, producer: ProducerKind) -> Charge {
        self.ledger.ceiling(producer)
    }

    /// # Panics
    ///
    /// If the reservation was not taken from this buffer (or is unmetered):
    /// its charge lives in another ledger, and no local repair keeps both
    /// ledgers exact.
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
        let duplicate = {
            let mut map = self.batches.write();
            match map.entry(Arc::clone(&batch.batch_id)) {
                Entry::Occupied(_) => Some(batch),
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
                        backdate: Duration::ZERO,
                    });
                    None
                }
            }
        };
        // A duplicate's reservation still holds its charge: release it here,
        // outside the map lock.
        drop(reservation);
        if let Some(batch) = duplicate {
            duplicate_batch(&batch);
        } else {
            self.generation.fetch_add(1, Ordering::Relaxed);
            // Counted before the pressure wake below: a compaction loop that
            // this wake reaches then reads the new count.
            self.ledger.inserted_batches.fetch_add(1, Ordering::Relaxed);
            // The WAL file now exists, so a pressure pass has work to find.
            if self.ledger.admission() != AdmissionState::Open {
                advance(&self.ledger.pressure);
            }
        }
    }

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

    /// Install the WAL that survived a restart as resident batches, once,
    /// into an empty buffer (ADR-0041 slice 2).
    ///
    /// Batches become resident oldest first by `age` (ties keep the plan's
    /// order), so the map's first entry stays the oldest.
    pub(crate) fn hydrate(&self, batches: Vec<HydratedBatch>) -> Result<Charge, HydrateError> {
        if self.hydrated.swap(true, Ordering::AcqRel) {
            return Err(HydrateError::AlreadyHydrated);
        }
        let mut seen = std::collections::HashSet::with_capacity(batches.len());
        let (mut plan, repeated): (Vec<_>, Vec<_>) = batches
            .into_iter()
            .partition(|hydrated| seen.insert(Arc::clone(&hydrated.batch.batch_id)));
        let total = plan
            .iter()
            .try_fold(Charge::ZERO, |total, hydrated| {
                total.checked_add(Charge {
                    events: hydrated.batch.events.len(),
                    bytes: hydrated.batch.byte_size,
                })
            })
            .filter(|total| total.fits(self.ledger.caps))
            .ok_or(HydrateError::ExceedsCaps)?;
        plan.sort_by_key(|hydrated| std::cmp::Reverse(hydrated.age));
        {
            // Map lock, then ledger lock, held from the emptiness check
            // through the install.
            let mut map = self.batches.write();
            let mut state = self.ledger.state.lock();
            if !map.is_empty() || !state.charged.is_zero() {
                return Err(HydrateError::NotEmpty);
            }
            let inserted = Instant::now();
            for HydratedBatch { batch, age } in plan {
                let charge = Charge {
                    events: batch.events.len(),
                    bytes: batch.byte_size,
                };
                map.insert(
                    Arc::clone(&batch.batch_id),
                    Resident {
                        batch,
                        charge,
                        inserted,
                        backdate: age,
                    },
                );
            }
            self.total_events.fetch_add(total.events, Ordering::Relaxed);
            self.total_bytes.fetch_add(total.bytes, Ordering::Relaxed);
            state.charged = total;
            self.ledger.publish(&mut state, false);
        }
        self.generation.fetch_add(1, Ordering::Relaxed);
        for hydrated in &repeated {
            duplicate_batch(&hydrated.batch);
        }
        Ok(total)
    }

    pub(crate) fn is_resident(&self, batch_id: &str) -> bool {
        self.batches.read().contains_key(batch_id)
    }

    /// Remove batches that have been compacted to parquet, releasing their
    /// charge. The release happens under the map lock (map, then ledger),
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

    pub fn admission_state(&self) -> AdmissionState {
        self.ledger.admission()
    }

    /// Everything charged: outstanding reservations plus resident batches,
    /// every producer included.
    pub fn charged(&self) -> Charge {
        self.ledger.charged()
    }

    /// Age of the oldest resident batch on the monotonic clock, or `None`
    /// when the buffer is empty. A hydrated batch counts from the time in
    /// its WAL file name, saturating at [`Duration::MAX`].
    pub fn oldest_batch_age(&self) -> Option<Duration> {
        self.batches
            .read()
            .first()
            .map(|(_, resident)| resident.age())
    }

    /// Batches removed by [`drain`](Self::drain) since construction,
    /// monotonic. Compaction measures its progress by this delta.
    pub fn drained_batches(&self) -> u64 {
        self.ledger.drained_batches.load(Ordering::Relaxed)
    }

    /// Batches made resident by [`insert`](Self::insert) since
    /// construction, monotonic. A refusal, a dropped reservation or a
    /// duplicate batch id does not count. Compaction reads it to tell an
    /// insert's pressure wake from a refusal's.
    pub fn inserted_batches(&self) -> u64 {
        self.ledger.inserted_batches.load(Ordering::Relaxed)
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

    /// Returns `Ok(None)` if the buffer holds no event. A failure is never an
    /// empty buffer, because a reader that took it for one would answer
    /// without the newest events (ADR-0041).
    ///
    /// The snapshot cache mutex is held for the entire check-then-build
    /// cycle to serialize concurrent misses.
    ///
    /// # Errors
    ///
    /// The I/O error that stopped the build. Its text can quote a path;
    /// callers report its kind, not its text.
    pub fn snapshot(&self, scratch: &QueryScratch) -> std::io::Result<Option<HotSnapshot>> {
        if self.total_events.load(Ordering::Relaxed) == 0 {
            #[cfg(any(test, feature = "test-support"))]
            self.record_snapshot(self.generation.load(Ordering::Relaxed), 0);
            return Ok(None);
        }

        let current_gen = self.generation.load(Ordering::Relaxed);

        let mut cache = self.snapshot_cache.lock();

        if let Some(cached) = cache.as_ref()
            && cached.generation == current_gen
        {
            #[cfg(any(test, feature = "test-support"))]
            self.record_snapshot(current_gen, cached.events);
            return Ok(Some(HotSnapshot {
                field_types: Arc::new(self.pins_for(&cached.keys)),
                file: Arc::clone(&cached.file),
                columns: Arc::clone(&cached.columns),
            }));
        }

        let Some((file, keys, null_only)) = self.build_snapshot(scratch)? else {
            #[cfg(any(test, feature = "test-support"))]
            self.record_snapshot(current_gen, 0);
            return Ok(None);
        };
        #[cfg(any(test, feature = "test-support"))]
        let events = self.built_events.load(Ordering::Relaxed);
        #[cfg(any(test, feature = "test-support"))]
        self.record_snapshot(current_gen, events);
        let file = Arc::new(file);
        let field_types = Arc::new(self.pins_for(&keys));
        let columns = Arc::new(trawl_core::emitter::HotColumns::new(&keys, null_only));
        *cache = Some(CachedSnapshot {
            generation: current_gen,
            file: Arc::clone(&file),
            keys,
            columns: Arc::clone(&columns),
            #[cfg(any(test, feature = "test-support"))]
            events,
        });

        Ok(Some(HotSnapshot {
            file,
            field_types,
            columns,
        }))
    }

    #[cfg(any(test, feature = "test-support"))]
    fn record_snapshot(&self, generation: u64, events: usize) {
        self.snapshot_log
            .lock()
            .push(SnapshotRecord { generation, events });
    }

    /// Every snapshot call answered since the buffer was made or the log
    /// last cleared, in order: a build, a cache hit or an empty buffer's
    /// `None`, each with the generation it is keyed by and the events its
    /// file holds. A failed build is not logged.
    #[cfg(any(test, feature = "test-support"))]
    pub fn snapshots_for_test(&self) -> Vec<SnapshotRecord> {
        self.snapshot_log.lock().clone()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn clear_snapshots_for_test(&self) {
        self.snapshot_log.lock().clear();
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn generation_for_test(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Make the next snapshot build fail: the cached file is dropped, and
    /// the next build's first event write is refused with
    /// [`StorageFull`](std::io::ErrorKind::StorageFull), as if the temp
    /// directory had filled up. One shot; nothing latches.
    #[cfg(any(test, feature = "test-support"))]
    pub fn fail_next_snapshot_for_test(&self) {
        let mut cache = self.snapshot_cache.lock();
        *cache = None;
        self.fail_next_snapshot.store(true, Ordering::Relaxed);
    }

    /// The bytes a snapshot of the whole buffer holds right now: every
    /// resident event serialized as [`Self::snapshot`] writes it.
    #[cfg(test)]
    pub(crate) fn serialized_len_for_test(&self) -> u64 {
        struct Count(u64);
        impl std::io::Write for Count {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0 += buf.len() as u64;
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let map = self.batches.read();
        let mut count = Count(0);
        write_events(
            &mut count,
            map.values()
                .flat_map(|resident| resident.batch.events.iter()),
        )
        .expect("counting cannot fail");
        count.0
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
    /// returning it with the key set the file carries and the keys among
    /// them whose every value is JSON `null`. Both come from the survey
    /// over the events the file is written from, under the same read lock,
    /// so no insert can slip between the evidence and the file.
    fn build_snapshot(&self, scratch: &QueryScratch) -> std::io::Result<Option<SurveyedFile>> {
        let map = self.batches.read();
        let events: Vec<&Event> = map
            .values()
            .flat_map(|resident| resident.batch.events.iter())
            .collect();
        if events.is_empty() {
            return Ok(None);
        }
        #[cfg(any(test, feature = "test-support"))]
        self.built_events.store(events.len(), Ordering::Relaxed);
        let Survey {
            pioneers: pioneer,
            keys,
            null_only,
        } = survey_schema(events.iter().copied());
        let order = (0..events.len())
            .filter(|&i| pioneer[i])
            .chain((0..events.len()).filter(|&i| !pioneer[i]))
            .map(|i| events[i]);

        let mut tmpfile = scratch.snapshot_file()?;
        // serde_json emits many small writes per event. Buffer them before
        // crossing into the filesystem, then flush before publishing the file.
        let writer = BufWriter::new(&mut tmpfile);
        // The injected fault sits in front of the buffer, so the first
        // event's serialization is what fails.
        #[cfg(any(test, feature = "test-support"))]
        let writer = test_seam::Tripwire {
            tripped: self.fail_next_snapshot.swap(false, Ordering::Relaxed),
            inner: writer,
        };
        let mut writer = writer;
        write_events(&mut writer, order)?;
        writer.flush()?;
        drop(writer);
        Ok(Some((tmpfile, keys, null_only)))
    }

    pub fn event_count(&self) -> usize {
        self.total_events.load(Ordering::Relaxed)
    }

    pub fn byte_count(&self) -> usize {
        self.total_bytes.load(Ordering::Relaxed)
    }

    pub fn batch_count(&self) -> usize {
        self.batches.read().len()
    }

    pub fn config(&self) -> &HotBufferConfig {
        &self.config
    }
}

type SurveyedFile = (tempfile::NamedTempFile, Vec<String>, Vec<String>);

struct Survey {
    /// Per event, in order: whether it carries a key no earlier event did.
    pioneers: Vec<bool>,
    keys: Vec<String>,
    /// The keys no event holds a non-null value under.
    null_only: Vec<String>,
}

/// Writing the pioneers first puts the complete key set inside `DuckDB`'s
/// detection prefix (ADR-0008). The null-only evidence (ADR-0062) is read
/// off the values themselves, never off `DuckDB`'s inferred type.
fn survey_schema<'a>(events: impl Iterator<Item = &'a Event>) -> Survey {
    let mut seen: std::collections::HashSet<&'a str> = std::collections::HashSet::new();
    let mut non_null: std::collections::HashSet<&'a str> = std::collections::HashSet::new();
    let pioneers = events
        .map(|event| {
            let mut novel = false;
            for (key, value) in event {
                novel |= seen.insert(key.as_str());
                if !value.is_null() {
                    non_null.insert(key.as_str());
                }
            }
            novel
        })
        .collect();
    let null_only = seen
        .difference(&non_null)
        .map(|key| (*key).to_owned())
        .collect();
    let keys = seen.into_iter().map(str::to_owned).collect();
    Survey {
        pioneers,
        keys,
        null_only,
    }
}

fn write_events<'a, W: std::io::Write>(
    writer: &mut W,
    events: impl Iterator<Item = &'a Event>,
) -> std::io::Result<()> {
    for event in events {
        serde_json::to_writer(&mut *writer, event)?;
        writer.write_all(b"\n")?;
    }
    Ok(())
}

/// A query scratch directory in a fresh temp directory, for tests that
/// take hot-buffer snapshots without a pool. It dereferences to the
/// [`QueryScratch`] [`HotBuffer::snapshot`] takes, and is removed on drop.
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug)]
pub struct TestScratch {
    scratch: QueryScratch,
    _dir: tempfile::TempDir,
}

#[cfg(any(test, feature = "test-support"))]
impl TestScratch {
    #[must_use]
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("make a scratch temp dir");
        Self {
            scratch: QueryScratch::new(dir.path()),
            _dir: dir,
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Default for TestScratch {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(any(test, feature = "test-support"))]
impl std::ops::Deref for TestScratch {
    type Target = QueryScratch;

    fn deref(&self) -> &QueryScratch {
        &self.scratch
    }
}

#[cfg(any(test, feature = "test-support"))]
mod test_seam {
    //! The fault behind [`HotBuffer::fail_next_snapshot_for_test`].

    /// Passes writes through to `inner` until `tripped`, then refuses
    /// every one with `StorageFull`.
    pub(super) struct Tripwire<W> {
        pub(super) tripped: bool,
        pub(super) inner: W,
    }

    impl<W: std::io::Write> std::io::Write for Tripwire<W> {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.tripped {
                return Err(std::io::ErrorKind::StorageFull.into());
            }
            self.inner.write(buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.inner.flush()
        }
    }
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
            byte_size: n * 50,
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
        let scratch = TestScratch::new();
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 1000,
            max_bytes: 10_000_000,
        });
        buf.insert_for_test(make_batch("batch_001", 3));
        assert_eq!(buf.event_count(), 3);
        assert_eq!(buf.batch_count(), 1);

        let tmpfile = buf.snapshot(&scratch).unwrap().expect("should have events");
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
        let scratch = TestScratch::new();
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 1000,
            max_bytes: 10_000_000,
        });
        let events = events_with_trailing_sparse_key(50);
        buf.insert_for_test(Arc::new(IngestBatch {
            batch_id: "b1".into(),
            service: "svc".into(),
            byte_size: 100,
            events,
        }));

        let tmpfile = buf.snapshot(&scratch).unwrap().expect("should have events");
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
        let scratch = TestScratch::new();

        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 100_000,
            max_bytes: 100 * 1024 * 1024,
        });
        buf.insert_for_test(Arc::new(IngestBatch {
            batch_id: "b1".into(),
            service: "svc".into(),
            byte_size: 1000,
            events: events_with_trailing_sparse_key(30_000),
        }));
        let snapshot = buf.snapshot(&scratch).unwrap().expect("should have events");
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
                    &snapshot.columns,
                    &trawl_core::schema::FieldTypes::new(),
                    &trawl_core::schema::FieldTypes::new(),
                    usize::MAX,
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
        use trawl_core::schema::CanonicalType;
        let scratch = TestScratch::new();

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
        buf.insert_for_test(Arc::new(IngestBatch {
            batch_id: "b1".into(),
            service: "svc".into(),
            byte_size: 100,
            events: vec![ev],
        }));

        let snap = buf.snapshot(&scratch).unwrap().expect("should have events");
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
        use trawl_core::schema::CanonicalType;
        let scratch = TestScratch::new();

        let catalog = Arc::new(crate::catalog::FieldCatalog::new());
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 1000,
            max_bytes: 10_000_000,
        })
        .with_field_catalog(Arc::clone(&catalog));

        let mut ev = serde_json::Map::new();
        ev.insert("service".into(), serde_json::Value::String("svc".into()));
        ev.insert("duration".into(), serde_json::Value::from(42));
        buf.insert_for_test(Arc::new(IngestBatch {
            batch_id: "b1".into(),
            service: "svc".into(),
            byte_size: 100,
            events: vec![ev],
        }));

        let first = buf.snapshot(&scratch).unwrap().expect("should have events");
        assert!(
            first.field_types.is_empty(),
            "no pins yet — nothing to conform"
        );

        // Pin lands mid-window: no buffer mutation, same generation.
        catalog.replace([("duration".to_string(), CanonicalType::BigInt)]);

        let second = buf.snapshot(&scratch).unwrap().expect("should have events");
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

    fn event_with(i: usize, fields: &[(&str, serde_json::Value)]) -> Event {
        let mut m = serde_json::Map::new();
        m.insert(
            "_time".into(),
            serde_json::Value::String("2024-01-15T10:00:00Z".into()),
        );
        m.insert("message".into(), serde_json::Value::String(format!("m{i}")));
        for (key, value) in fields {
            m.insert((*key).into(), value.clone());
        }
        m
    }

    fn insert_events(buf: &HotBuffer, id: &str, events: Vec<Event>) {
        buf.insert_for_test(Arc::new(IngestBatch {
            batch_id: id.into(),
            service: "svc".into(),
            byte_size: 100,
            events,
        }));
    }

    #[test]
    fn snapshot_null_only_evidence_agrees_with_the_files_values() {
        // `gone` is JSON null in every event; `late` is null until the
        // last event, which sits past DuckDB's ~20480-record inference
        // prefix (its first event is the pioneer, so nothing hoists the
        // late one); `mixed` alternates. Only `gone` is null-only: any
        // non-null value clears it, wherever it sits in the file.
        let scratch = TestScratch::new();
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 100_000,
            max_bytes: 100 * 1024 * 1024,
        });
        let n = 25_000;
        let events = (0..n)
            .map(|i| {
                let late = if i == n - 1 {
                    serde_json::Value::from(7)
                } else {
                    serde_json::Value::Null
                };
                let mixed = if i % 2 == 0 {
                    serde_json::Value::String("x".into())
                } else {
                    serde_json::Value::Null
                };
                event_with(
                    i,
                    &[
                        ("gone", serde_json::Value::Null),
                        ("late", late),
                        ("mixed", mixed),
                    ],
                )
            })
            .collect();
        insert_events(&buf, "b1", events);

        let snap = buf.snapshot(&scratch).unwrap().expect("should have events");
        let null_only: Vec<&str> = snap.columns.null_only().collect();
        assert_eq!(null_only, ["gone"], "only the all-null key is null-only");
        for key in ["_time", "message", "gone", "late", "mixed"] {
            assert!(snap.columns.contains(key), "{key} is a snapshot key");
        }

        let from_file = trawl_engine::executor::hot_columns_of(snap.path());
        assert_eq!(
            *snap.columns, from_file,
            "the survey and hot_columns_of must agree on the same file"
        );
    }

    #[test]
    fn null_only_evidence_is_kept_per_generation() {
        use trawl_core::schema::CanonicalType;
        let scratch = TestScratch::new();
        let catalog = Arc::new(crate::catalog::FieldCatalog::new());
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 1000,
            max_bytes: 10_000_000,
        })
        .with_field_catalog(Arc::clone(&catalog));
        insert_events(
            &buf,
            "b1",
            vec![event_with(0, &[("k", serde_json::Value::Null)])],
        );

        let first = buf.snapshot(&scratch).unwrap().expect("should have events");
        assert!(first.columns.is_null_only("k"));

        catalog.replace([("k".to_string(), CanonicalType::BigInt)]);
        let pinned = buf.snapshot(&scratch).unwrap().expect("should have events");
        assert!(Arc::ptr_eq(&first.file, &pinned.file));
        assert!(Arc::ptr_eq(&first.columns, &pinned.columns));
        assert!(pinned.columns.is_null_only("k"));
        assert_eq!(pinned.field_types.get("k"), Some(CanonicalType::BigInt));

        insert_events(
            &buf,
            "b2",
            vec![event_with(1, &[("k", serde_json::Value::from(5))])],
        );
        let next = buf.snapshot(&scratch).unwrap().expect("should have events");
        assert!(!Arc::ptr_eq(&first.file, &next.file));
        assert!(next.columns.contains("k"));
        assert!(
            !next.columns.is_null_only("k"),
            "a non-null value in the new generation clears null-only"
        );
    }

    #[test]
    fn drain_removes_batches() {
        let scratch = TestScratch::new();
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 1000,
            max_bytes: 10_000_000,
        });
        buf.insert_for_test(make_batch("batch_001", 2));
        buf.insert_for_test(make_batch("batch_002", 3));
        assert_eq!(buf.event_count(), 5);

        buf.drain(&["batch_001"]);
        assert_eq!(buf.event_count(), 3);
        assert_eq!(buf.batch_count(), 1);

        buf.drain(&["batch_002"]);
        assert_eq!(buf.event_count(), 0);
        assert!(buf.snapshot(&scratch).unwrap().is_none());
    }

    #[test]
    fn drain_updates_byte_count() {
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 1000,
            max_bytes: 10_000_000,
        });
        buf.insert_for_test(make_batch_with_bytes("batch_001", 2, 200));
        buf.insert_for_test(make_batch_with_bytes("batch_002", 3, 400));
        assert_eq!(buf.byte_count(), 600);

        buf.drain(&["batch_001"]);
        assert_eq!(buf.byte_count(), 400);
    }

    #[test]
    fn snapshot_includes_all_batches() {
        let scratch = TestScratch::new();
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 1000,
            max_bytes: 10_000_000,
        });
        buf.insert_for_test(make_batch("batch_001", 2));
        buf.insert_for_test(make_batch("batch_002", 3));

        let tmpfile = buf.snapshot(&scratch).unwrap().expect("should have events");
        let content = std::fs::read_to_string(tmpfile.path()).unwrap();
        assert_eq!(content.lines().count(), 5);
    }

    #[test]
    fn drain_after_snapshot_leaves_snapshot_valid() {
        let scratch = TestScratch::new();
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 1000,
            max_bytes: 10_000_000,
        });
        buf.insert_for_test(make_batch("batch_001", 2));
        buf.insert_for_test(make_batch("batch_002", 3));

        let snapshot = buf.snapshot(&scratch).unwrap().expect("should have events");
        let content_before = std::fs::read_to_string(snapshot.path()).unwrap();
        assert_eq!(content_before.lines().count(), 5);

        buf.drain(&["batch_001"]);

        let content_after = std::fs::read_to_string(snapshot.path()).unwrap();
        assert_eq!(content_after, content_before);
    }

    #[test]
    fn empty_buffer_returns_none() {
        let scratch = TestScratch::new();
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 100,
            max_bytes: 10_000_000,
        });
        assert!(buf.snapshot(&scratch).unwrap().is_none());
    }

    /// Refuses the first write of line `refuse_line` (0-based) once, and
    /// accepts every other write: one event whose serialization fails while
    /// its neighbours would succeed.
    struct RefusesOneLine {
        refuse_line: usize,
        refused: bool,
        /// Whole lines accepted so far. JSON escapes a newline inside a
        /// value, so a raw `\n` only ever ends a line.
        line: usize,
        out: Vec<u8>,
    }

    impl std::io::Write for RefusesOneLine {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if !self.refused && self.line == self.refuse_line {
                self.refused = true;
                return Err(std::io::ErrorKind::StorageFull.into());
            }
            if buf.contains(&b'\n') {
                self.line += 1;
            }
            self.out.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn snapshot_failure_of_one_event_fails_the_whole_write() {
        let batch = make_batch("batch_001", 3);
        let mut writer = RefusesOneLine {
            refuse_line: 1,
            refused: false,
            line: 0,
            out: Vec::new(),
        };
        let err = write_events(&mut writer, batch.events.iter())
            .expect_err("the second event's serialization failed");
        assert!(writer.refused);
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::StorageFull,
            "an I/O failure keeps its kind"
        );
        let written = String::from_utf8(writer.out).unwrap();
        assert_eq!(
            written.lines().collect::<Vec<_>>(),
            [serde_json::to_string(&batch.events[0]).unwrap()],
            "nothing after the failed event was written: {written}"
        );
    }

    #[test]
    fn snapshot_failure_is_an_error_never_an_empty_buffer() {
        let scratch = TestScratch::new();
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 1000,
            max_bytes: 10_000_000,
        });
        assert!(
            buf.snapshot(&scratch).unwrap().is_none(),
            "empty is Ok(None)"
        );
        buf.insert_for_test(make_batch("batch_001", 3));
        // Built and cached first: the fault must not be answered from the
        // cache.
        buf.snapshot(&scratch).unwrap().expect("should have events");

        buf.fail_next_snapshot_for_test();
        let err = buf
            .snapshot(&scratch)
            .expect_err("an event that cannot be written fails the snapshot");
        assert_eq!(err.kind(), std::io::ErrorKind::StorageFull);

        let snapshot = buf.snapshot(&scratch).unwrap().expect("should have events");
        let content = std::fs::read_to_string(snapshot.path()).unwrap();
        assert_eq!(content.lines().count(), 3);
    }

    #[test]
    fn drain_nonexistent_is_noop() {
        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 100,
            max_bytes: 10_000_000,
        });
        buf.insert_for_test(make_batch("batch_001", 2));
        buf.drain(&["nonexistent"]);
        assert_eq!(buf.event_count(), 2);
    }

    use crate::ingest::producer::ProducerKind::{Http, Syslog, Trawld};

    pub(super) fn ledger_buffer(events: usize, bytes: usize) -> HotBuffer {
        HotBuffer::new(HotBufferConfig {
            max_events: events,
            max_bytes: bytes,
        })
    }

    pub(super) const fn charge(events: usize, bytes: usize) -> Charge {
        Charge { events, bytes }
    }

    pub(super) fn batch_of(id: &str, charge: Charge) -> Arc<IngestBatch> {
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

        let http = buf.reserve(Http, charge(1, 15_000)).unwrap();
        assert_eq!(buf.reserve(Http, charge(0, 1)).unwrap_err(), Refusal::Full);
        let trawld = buf.reserve(Trawld, charge(1, 1_000)).unwrap();
        assert_eq!(
            buf.reserve(Trawld, charge(0, 1)).unwrap_err(),
            Refusal::Full
        );
        drop((http, trawld));

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

        let small = buf.reserve(Syslog, charge(10, 0)).unwrap();
        buf.insert(small, batch_of("env/small", charge(10, 0)));
        assert!(!pressure.has_changed().unwrap());
        assert_eq!(buf.inserted_batches(), 1, "every insert counts, Open too");

        let big = buf.reserve(Syslog, charge(50, 0)).unwrap();
        assert_eq!(buf.admission_state(), AdmissionState::Pressure);
        assert!(!pressure.has_changed().unwrap());
        buf.insert(big, batch_of("env/big", charge(50, 0)));
        assert!(
            pressure.has_changed().unwrap(),
            "insert while in pressure wakes"
        );
        pressure.borrow_and_update();

        assert_eq!(buf.inserted_batches(), 2);

        assert_eq!(buf.reserve(Http, charge(40, 0)).unwrap_err(), Refusal::Full);
        assert!(pressure.has_changed().unwrap(), "full refusal wakes");
        pressure.borrow_and_update();
        assert_eq!(buf.inserted_batches(), 2, "a refusal is not an insert");

        buf.drain(&["env/big", "env/small"]);
        assert_eq!(buf.admission_state(), AdmissionState::Open);
        assert!(
            !pressure.has_changed().unwrap(),
            "drain is not a pressure signal"
        );
        assert_eq!(buf.inserted_batches(), 2, "a drain is not an insert");
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

        ledger.release(resident);
        assert_eq!(buf.charged(), charge(5, 50));
    }
}

#[cfg(test)]
mod hydrate {
    //! [`HotBuffer::hydrate`]: the boot reload of surviving WAL (ADR-0041
    //! slice 2) charges the full caps and is not a producer (ADR-0043).

    use super::tests::{batch_of, charge, ledger_buffer};
    use super::*;
    use crate::ingest::producer::ProducerKind::{Http, Trawld};

    fn fresh(id: &str, of: Charge) -> HydratedBatch {
        HydratedBatch {
            batch: batch_of(id, of),
            age: Duration::ZERO,
        }
    }

    fn aged(id: &str, of: Charge, age: Duration) -> HydratedBatch {
        HydratedBatch {
            batch: batch_of(id, of),
            age,
        }
    }

    /// Every `{producer, kind}` refusal series, which the recorder has
    /// zero-initialized.
    fn refusal_series(handle: &metrics_exporter_prometheus::PrometheusHandle) -> Vec<u64> {
        let mut samples = Vec::new();
        for producer in ProducerKind::ALL {
            for kind in Refusal::ALL {
                samples.push(crate::metrics::test_support::sample(
                    handle,
                    &format!(
                        "trawl_hot_buffer_admission_refusals_total{{producer=\"{}\",kind=\"{}\"}}",
                        producer.as_str(),
                        kind.label()
                    ),
                ));
            }
        }
        samples
    }

    #[test]
    fn hydration_at_half_a_cap_is_pressure_and_counts_no_refusal() {
        let recorder = crate::metrics::prometheus_builder().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            crate::metrics::init_operational_alert_metrics();
            // Caps 100 events / 1000 bytes: Pressure enters at 50 | 500.
            for (plan, why) in [
                (
                    vec![
                        fresh("prod/a", charge(30, 100)),
                        fresh("prod/b", charge(20, 100)),
                    ],
                    "events",
                ),
                (
                    vec![
                        fresh("prod/a", charge(1, 250)),
                        fresh("prod/b", charge(1, 250)),
                    ],
                    "bytes",
                ),
            ] {
                let buf = ledger_buffer(100, 1_000);
                let total = buf.hydrate(plan).expect("half a cap fits");
                assert_eq!(buf.charged(), total, "{why}: charged exactly the plan");
                assert_eq!(buf.admission_state(), AdmissionState::Pressure, "{why}");
                assert_eq!(
                    (buf.event_count(), buf.byte_count(), buf.batch_count()),
                    (total.events, total.bytes, 2),
                    "{why}"
                );
            }
            assert!(
                refusal_series(&handle).iter().all(|&n| n == 0),
                "hydration counts no admission refusal"
            );
        });
    }

    #[test]
    fn hydration_to_the_full_cap_is_pressure_never_refusing() {
        let recorder = crate::metrics::prometheus_builder().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            crate::metrics::init_operational_alert_metrics();
            let buf = ledger_buffer(100, 1_000);
            let total = buf
                .hydrate(vec![
                    fresh("prod/a", charge(60, 600)),
                    fresh("prod/b", charge(40, 400)),
                ])
                .expect("exactly the full caps fit");
            assert_eq!(total, charge(100, 1_000));
            assert_eq!(buf.charged(), charge(100, 1_000));
            assert_eq!(buf.admission_state(), AdmissionState::Pressure);
            assert!(refusal_series(&handle).iter().all(|&n| n == 0));
        });
    }

    /// The buffer holds no event bus, so "hydration publishes nothing to the
    /// bus" is proven against a real `AppState` at boot, in
    /// `tests/boot_corpus.rs`.
    #[test]
    fn hydration_wakes_nothing() {
        // Subscribe to every watch a producer's insert can touch before the
        // hydration, so a signal it sends cannot be missed.
        let buf = ledger_buffer(100, 1_000);
        let mut pressure = buf.subscribe_pressure();
        let mut released = buf.subscribe_released();
        pressure.borrow_and_update();
        released.borrow_and_update();

        buf.hydrate(vec![
            fresh("prod/a", charge(40, 400)),
            fresh("prod/b", charge(30, 300)),
        ])
        .unwrap();
        assert_eq!(buf.admission_state(), AdmissionState::Pressure);

        assert!(
            !pressure.has_changed().unwrap(),
            "hydration sends no pressure wake"
        );
        assert!(
            !released.has_changed().unwrap(),
            "hydration releases nothing"
        );
        assert_eq!(
            buf.inserted_batches(),
            0,
            "a hydrated batch is not a producer's insert"
        );
        assert_eq!(buf.drained_batches(), 0);
    }

    #[test]
    fn a_real_full_refusal_after_hydration_latches_refusing() {
        let recorder = crate::metrics::prometheus_builder().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            crate::metrics::init_operational_alert_metrics();
            let buf = ledger_buffer(100, 1_000);
            let mut pressure = buf.subscribe_pressure();
            pressure.borrow_and_update();
            buf.hydrate(vec![fresh("prod/a", charge(50, 0))]).unwrap();
            assert_eq!(buf.admission_state(), AdmissionState::Pressure);

            // 50 + 44 > 93, the external ceiling: the normal reserve path.
            assert_eq!(buf.reserve(Http, charge(44, 0)).unwrap_err(), Refusal::Full);
            assert_eq!(buf.admission_state(), AdmissionState::Refusing);
            assert!(
                pressure.has_changed().unwrap(),
                "the refusal wakes compaction"
            );
            assert_eq!(
                crate::metrics::test_support::sample(
                    &handle,
                    "trawl_hot_buffer_admission_refusals_total{producer=\"http\",kind=\"full\"}"
                ),
                1
            );
            assert_eq!(refusal_series(&handle).iter().sum::<u64>(), 1);

            buf.drain(&["prod/a"]);
            assert_eq!(buf.charged(), Charge::ZERO);
            assert_eq!(buf.admission_state(), AdmissionState::Open);
        });
    }

    #[test]
    fn only_a_buffer_given_a_starting_gate_starts_unsettled() {
        use crate::publication::{CorpusUnsettled, PublicationGate};
        assert_eq!(ledger_buffer(10, 100).publication().unsettled(), None);

        let buf = ledger_buffer(10, 100).with_publication(PublicationGate::starting());
        let gate = buf.publication();
        assert_eq!(gate.unsettled(), Some(CorpusUnsettled::RestartBacklog));
        assert!(
            Arc::ptr_eq(&gate, &buf.publication()),
            "the buffer shares the one installed gate"
        );
        gate.finish_hydration(false).unwrap();
        assert_eq!(buf.publication().unsettled(), None);
    }

    #[test]
    fn second_hydrate_is_already_hydrated() {
        let buf = ledger_buffer(100, 1_000);
        assert_eq!(buf.hydrate(Vec::new()), Ok(Charge::ZERO));
        assert_eq!(
            buf.hydrate(vec![fresh("prod/a", charge(1, 1))]),
            Err(HydrateError::AlreadyHydrated),
            "an empty install still spends the one shot"
        );
        assert_eq!(buf.charged(), Charge::ZERO);
        assert_eq!(buf.batch_count(), 0);

        let buf = ledger_buffer(100, 1_000);
        buf.hydrate(vec![fresh("prod/a", charge(1, 1))]).unwrap();
        buf.drain(&["prod/a"]);
        assert_eq!(
            buf.hydrate(vec![fresh("prod/b", charge(1, 1))]),
            Err(HydrateError::AlreadyHydrated),
            "an emptied buffer cannot be hydrated again"
        );
        assert!(!buf.is_resident("prod/b"));
    }

    #[test]
    fn hydrate_on_a_non_empty_ledger_is_not_empty() {
        let buf = ledger_buffer(100, 1_000);
        buf.insert_for_test(batch_of("prod/live", charge(1, 10)));
        assert_eq!(
            buf.hydrate(vec![fresh("prod/a", charge(1, 1))]),
            Err(HydrateError::NotEmpty)
        );
        assert_eq!(buf.charged(), charge(1, 10));
        assert!(!buf.is_resident("prod/a"));

        let buf = ledger_buffer(100, 1_000);
        let held = buf.reserve(Trawld, charge(1, 10)).unwrap();
        assert_eq!(
            buf.hydrate(vec![fresh("prod/a", charge(1, 1))]),
            Err(HydrateError::NotEmpty)
        );
        assert_eq!(buf.charged(), charge(1, 10));
        assert_eq!(buf.batch_count(), 0);
        drop(held);
        assert_eq!(
            buf.hydrate(vec![fresh("prod/a", charge(1, 1))]),
            Err(HydrateError::AlreadyHydrated),
            "a refused call spends the one shot too"
        );
    }

    #[test]
    fn over_cap_plan_is_exceeds_caps_and_leaves_the_ledger_untouched() {
        let scratch = TestScratch::new();
        for (plan, why) in [
            (
                vec![
                    fresh("prod/a", charge(60, 1)),
                    fresh("prod/b", charge(41, 1)),
                ],
                "events",
            ),
            (
                vec![
                    fresh("prod/a", charge(1, 600)),
                    fresh("prod/b", charge(1, 401)),
                ],
                "bytes",
            ),
            (
                vec![
                    HydratedBatch {
                        batch: Arc::new(IngestBatch {
                            batch_id: "prod/a".into(),
                            service: "test".into(),
                            byte_size: usize::MAX,
                            events: Vec::new(),
                        }),
                        age: Duration::ZERO,
                    },
                    fresh("prod/b", charge(0, 1)),
                ],
                "overflow",
            ),
        ] {
            let buf = ledger_buffer(100, 1_000);
            let mut pressure = buf.subscribe_pressure();
            let mut released = buf.subscribe_released();
            pressure.borrow_and_update();
            released.borrow_and_update();
            assert_eq!(buf.hydrate(plan), Err(HydrateError::ExceedsCaps), "{why}");
            assert_eq!(buf.charged(), Charge::ZERO, "{why}");
            assert_eq!(buf.admission_state(), AdmissionState::Open, "{why}");
            assert_eq!(
                (buf.event_count(), buf.byte_count(), buf.batch_count()),
                (0, 0, 0),
                "{why}"
            );
            assert!(!buf.is_resident("prod/a"), "{why}");
            assert!(buf.snapshot(&scratch).unwrap().is_none(), "{why}");
            assert!(!pressure.has_changed().unwrap(), "{why}");
            assert!(!released.has_changed().unwrap(), "{why}");
        }
    }

    #[test]
    fn hydrated_batches_are_resident_readable_and_drain_like_any_other() {
        let scratch = TestScratch::new();
        let buf = ledger_buffer(100, 1_000);
        let mut events = vec![serde_json::Map::new(); 2];
        events[0].insert("message".into(), "first".into());
        events[1].insert("message".into(), "second".into());
        buf.hydrate(vec![
            HydratedBatch {
                batch: Arc::new(IngestBatch {
                    batch_id: "prod/svc_1_abcd".into(),
                    service: "svc".into(),
                    byte_size: 40,
                    events,
                }),
                age: Duration::ZERO,
            },
            fresh("dev/svc_2_abcd", charge(1, 5)),
        ])
        .unwrap();
        assert!(buf.is_resident("prod/svc_1_abcd"));
        assert!(buf.is_resident("dev/svc_2_abcd"));
        assert!(!buf.is_resident("prod/svc_3_abcd"));

        let snapshot = buf
            .snapshot(&scratch)
            .unwrap()
            .expect("hydrated rows are readable");
        let content = std::fs::read_to_string(snapshot.path()).unwrap();
        assert_eq!(content.lines().count(), 3);
        assert!(content.contains("first") && content.contains("second"));

        let live = buf.reserve(Http, charge(1, 5)).unwrap();
        buf.insert(live, batch_of("prod/live", charge(1, 5)));
        assert_eq!(buf.charged(), charge(4, 50));

        buf.drain(&["prod/svc_1_abcd", "dev/svc_2_abcd", "prod/live"]);
        assert!(!buf.is_resident("prod/svc_1_abcd"));
        assert_eq!(buf.charged(), Charge::ZERO);
        assert_eq!(buf.drained_batches(), 3);
        assert!(buf.snapshot(&scratch).unwrap().is_none());
    }

    #[test]
    fn backdated_age_is_reflected_by_the_oldest_batch_age_and_saturates() {
        let hour = Duration::from_secs(3_600);
        let buf = ledger_buffer(100, 1_000);
        // Handed over newest first: the oldest still reads as the oldest.
        buf.hydrate(vec![
            aged("prod/new", charge(1, 1), Duration::from_secs(5)),
            aged("prod/old", charge(1, 1), hour),
            aged("prod/mid", charge(1, 1), Duration::from_secs(60)),
        ])
        .unwrap();
        let oldest = buf.oldest_batch_age().unwrap();
        assert!(oldest >= hour, "{oldest:?}");
        assert!(oldest < hour + Duration::from_secs(60), "{oldest:?}");

        let recorder = crate::metrics::prometheus_builder().build_recorder();
        let handle = recorder.handle();
        let buf = Arc::new(buf);
        metrics::with_local_recorder(&recorder, || {
            crate::metrics::collect_gauges(
                Some(&buf),
                &buf.publication(),
                std::path::Path::new("/nonexistent"),
                None,
                std::path::Path::new("/nonexistent/scratch"),
                crate::metrics::QueryGauges::default(),
                &crate::repin::JobGeneration::default(),
            );
        });
        let rendered = handle.render();
        let gauge: f64 = rendered
            .lines()
            .find_map(|line| line.strip_prefix("trawl_hot_buffer_oldest_batch_age_seconds "))
            .expect("the age gauge is rendered")
            .parse()
            .unwrap();
        assert!(gauge >= hour.as_secs_f64(), "{gauge}");

        buf.drain(&["prod/old"]);
        let next = buf.oldest_batch_age().unwrap();
        assert!(next >= Duration::from_secs(60) && next < hour, "{next:?}");

        let buf = ledger_buffer(100, 1_000);
        buf.hydrate(vec![aged("prod/ancient", charge(1, 1), Duration::MAX)])
            .unwrap();
        assert_eq!(buf.oldest_batch_age(), Some(Duration::MAX));
    }
}

#[cfg(test)]
mod duplicate_identity {
    //! A batch id already resident keeps the resident (ADR-0041): release
    //! builds and debug builds behave the same way.

    use super::tests::{batch_of, charge, ledger_buffer};
    use super::*;
    use crate::ingest::producer::ProducerKind::Syslog;

    /// `(level, event_type, field names)` of one recorded event.
    type Recorded = (tracing::Level, Option<String>, Vec<String>);

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<Recorded>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            #[derive(Default)]
            struct Fields {
                event_type: Option<String>,
                names: Vec<String>,
            }
            impl tracing::field::Visit for Fields {
                fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                    if field.name() == "event_type" {
                        self.event_type = Some(value.to_owned());
                    }
                    self.names.push(field.name().to_owned());
                }
                fn record_debug(&mut self, field: &tracing::field::Field, _: &dyn std::fmt::Debug) {
                    self.names.push(field.name().to_owned());
                }
            }
            let mut fields = Fields::default();
            event.record(&mut fields);
            self.0
                .lock()
                .push((*event.metadata().level(), fields.event_type, fields.names));
        }
    }

    fn rows(id: &str, messages: &[&str]) -> Arc<IngestBatch> {
        let events: Vec<Event> = messages
            .iter()
            .map(|message| {
                let mut event = serde_json::Map::new();
                event.insert("message".into(), (*message).into());
                event
            })
            .collect();
        Arc::new(IngestBatch {
            batch_id: id.into(),
            service: "svc".into(),
            byte_size: 10 * events.len(),
            events,
        })
    }

    fn duplicates(handle: &metrics_exporter_prometheus::PrometheusHandle) -> u64 {
        crate::metrics::test_support::sample(handle, "trawl_hot_buffer_duplicate_batches_total")
    }

    #[test]
    fn a_duplicate_insert_keeps_the_resident_and_is_counted_and_logged() {
        let scratch = TestScratch::new();
        let recorder = crate::metrics::prometheus_builder().build_recorder();
        let handle = recorder.handle();
        let capture = Capture::default();
        let subscriber = {
            use tracing_subscriber::layer::SubscriberExt as _;
            tracing_subscriber::registry().with(capture.clone())
        };
        tracing::subscriber::with_default(subscriber, || {
            metrics::with_local_recorder(&recorder, || {
                crate::metrics::init_operational_alert_metrics();
                assert_eq!(duplicates(&handle), 0, "zero-initialized");

                let buf = ledger_buffer(100, 1_000);
                let resident = rows("prod/svc_1_abcd", &["kept_one", "kept_two"]);
                let held = buf.reserve(Syslog, charge(2, 20)).unwrap();
                buf.insert(held, resident);
                let mut released = buf.subscribe_released();
                released.borrow_and_update();

                // Same id, different rows and charge.
                let intruder = rows("prod/svc_1_abcd", &["lost_one", "lost_two", "lost_three"]);
                let held = buf.reserve(Syslog, charge(3, 30)).unwrap();
                buf.insert(held, intruder);

                assert_eq!(buf.batch_count(), 1);
                assert_eq!((buf.event_count(), buf.byte_count()), (2, 20));
                assert_eq!(
                    buf.charged(),
                    charge(2, 20),
                    "the duplicate's reservation is released"
                );
                assert!(released.has_changed().unwrap());
                assert_eq!(buf.inserted_batches(), 1, "a duplicate is not an insert");
                let snapshot = buf.snapshot(&scratch).unwrap().unwrap();
                let content = std::fs::read_to_string(snapshot.path()).unwrap();
                assert!(content.contains("kept_one") && content.contains("kept_two"));
                assert!(!content.contains("lost_"), "{content}");
                assert_eq!(duplicates(&handle), 1);

                buf.drain(&["prod/svc_1_abcd"]);
                assert_eq!(buf.charged(), Charge::ZERO);
            });
        });

        let events = capture.0.lock();
        let errors: Vec<_> = events
            .iter()
            .filter(|(level, ..)| *level == tracing::Level::ERROR)
            .collect();
        assert_eq!(errors.len(), 1, "{events:?}");
        let (_, event_type, names) = errors[0];
        assert_eq!(event_type.as_deref(), Some("hot_buffer_duplicate_batch"));
        assert!(
            !names.iter().any(|name| name.contains("path")),
            "no path field: {names:?}"
        );
    }

    #[test]
    fn a_duplicate_id_within_a_hydration_plan_keeps_the_first() {
        let scratch = TestScratch::new();
        let recorder = crate::metrics::prometheus_builder().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            crate::metrics::init_operational_alert_metrics();
            let buf = ledger_buffer(100, 1_000);
            let total = buf
                .hydrate(vec![
                    HydratedBatch {
                        batch: rows("prod/svc_1_abcd", &["kept"]),
                        age: Duration::ZERO,
                    },
                    HydratedBatch {
                        batch: rows("prod/svc_1_abcd", &["lost_one", "lost_two"]),
                        age: Duration::ZERO,
                    },
                    HydratedBatch {
                        batch: batch_of("prod/svc_2_abcd", charge(1, 1)),
                        age: Duration::ZERO,
                    },
                ])
                .unwrap();
            assert_eq!(total, charge(2, 11), "the duplicate is not charged");
            assert_eq!(buf.charged(), total);
            assert_eq!(buf.batch_count(), 2);
            let content =
                std::fs::read_to_string(buf.snapshot(&scratch).unwrap().unwrap().path()).unwrap();
            assert!(content.contains("kept") && !content.contains("lost_"));
            assert_eq!(duplicates(&handle), 1);
        });
    }
}
