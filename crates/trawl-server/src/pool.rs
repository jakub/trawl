// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Semaphore-bounded executor pool for concurrent query execution.
//!
//! The semaphore limits concurrency, and each permit corresponds to exactly
//! one pooled executor. The request and the work it started are two
//! lifetimes (ADR-0024).

use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use trawl_api::EventHistogram;
use trawl_core::cancel::Stop;
use trawl_engine::cancel::CancelLatch;
use trawl_engine::error::EngineError;
use trawl_engine::executor::{Executor, QueryBudgets, RowCap};
use trawl_engine::scratch::{QueryScratch, ScratchLock};
use trawl_engine::timing::{PhaseClock, QueryPhase};
use trawl_engine::value::QueryResult;

use crate::deadline::Deadline;
use crate::error::{CaptureSourceKind, ServerError};
use crate::histogram::HistogramAsk;
use crate::hot_buffer::{HotBuffer, HotSnapshot};
use crate::query_timing::{Outcome, PhysicalOutcome, QueryTiming, Reclaim, emit_reclaimed};
use crate::source::{compute_source, select, selection_bytes};

#[cfg(any(test, feature = "test-support"))]
pub static TEST_QUERY_DELAY_MS: AtomicU64 = AtomicU64::new(0);

static NEXT_POOL_INSTANCE: AtomicU64 = AtomicU64::new(0);

const SAMPLE_ACQUIRE_BUDGET: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkKind {
    Query,
    FromSaved,
    Export,
    Scheduled,
    Sample,
    Capture,
}

impl WorkKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::FromSaved => "from_saved",
            Self::Export => "export",
            Self::Scheduled => "scheduled",
            Self::Sample => "sample",
            Self::Capture => "capture",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkOwner {
    Key(i64),
    System,
}

#[derive(Debug, Clone)]
pub struct WorkContext {
    pub kind: WorkKind,
    pub owner: WorkOwner,
    user: Option<Arc<str>>,
    timing: Option<QueryTiming>,
    holds: Option<Arc<dyn Any + Send + Sync>>,
    histogram: Option<HistogramAsk>,
}

impl WorkContext {
    #[must_use]
    pub fn key(kind: WorkKind, key_id: i64) -> Self {
        Self {
            kind,
            owner: WorkOwner::Key(key_id),
            user: None,
            timing: None,
            holds: None,
            histogram: None,
        }
    }

    #[must_use]
    pub fn system(kind: WorkKind) -> Self {
        Self {
            kind,
            owner: WorkOwner::System,
            user: None,
            timing: None,
            holds: None,
            histogram: None,
        }
    }

    #[must_use]
    pub fn with_user(mut self, name: &str) -> Self {
        self.user = Some(Arc::from(name));
        self
    }

    #[must_use]
    pub fn with_timing(mut self, timing: QueryTiming) -> Self {
        self.timing = Some(timing);
        self
    }

    /// Only [`ExecutorPool::execute`] and [`ExecutorPool::execute_capped`] read
    /// the ask.
    #[must_use]
    pub fn with_histogram(mut self, ask: HistogramAsk) -> Self {
        self.histogram = Some(ask);
        self
    }

    /// Keep `holds` until this work stops, not until its request does.
    /// A clone of this context shares the value, so a caller that keeps a clone
    /// keeps the charge.
    #[must_use]
    pub fn holding(mut self, holds: Arc<dyn Any + Send + Sync>) -> Self {
        self.holds = Some(holds);
        self
    }
}

pub(crate) fn scheduled_cap(permits: usize) -> usize {
    (permits / 2).max(1)
}

pub(crate) fn manual_cap(permits: usize) -> usize {
    (permits / 4).max(1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManualBusy {
    pub retry_after_secs: u64,
}

#[derive(Debug)]
struct ManualGate {
    limit: usize,
    holders: Mutex<HashMap<i64, Instant>>,
}

impl ManualGate {
    fn try_take(
        self: &Arc<Self>,
        key_id: i64,
        estimate: Duration,
        now: Instant,
    ) -> Result<ManualSlot, ManualBusy> {
        let mut holders = self.holders.lock();
        let refusing = if let Some(&end) = holders.get(&key_id) {
            Some(end)
        } else if holders.len() >= self.limit {
            holders.values().min().copied()
        } else {
            None
        };
        if let Some(end) = refusing {
            return Err(ManualBusy {
                retry_after_secs: crate::results::charge::ceil_secs(
                    end.saturating_duration_since(now),
                ),
            });
        }
        let end = now
            .checked_add(estimate)
            .unwrap_or_else(|| now + Duration::from_secs(u64::from(u32::MAX)));
        holders.insert(key_id, end);
        Ok(ManualSlot {
            gate: Arc::clone(self),
            key_id,
        })
    }
}

struct ManualSlot {
    gate: Arc<ManualGate>,
    key_id: i64,
}

impl Drop for ManualSlot {
    fn drop(&mut self) {
        self.gate.holders.lock().remove(&self.key_id);
    }
}

#[derive(Clone)]
pub struct RunLease {
    charge: Arc<dyn Any + Send + Sync>,
    #[cfg(any(test, feature = "test-support"))]
    seams: Arc<seam::Table>,
}

impl std::fmt::Debug for RunLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunLease").finish_non_exhaustive()
    }
}

impl RunLease {
    #[must_use]
    pub fn into_hold(self) -> Arc<dyn Any + Send + Sync> {
        self.charge
    }

    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn unattached() -> Self {
        Self {
            charge: Arc::new(()),
            seams: Arc::new(seam::Table::default()),
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn reach(&self, seam: seam::Seam) {
        self.seams.reach(seam);
    }
}

struct InterruptSlot {
    handle: Option<Arc<duckdb::InterruptHandle>>,
    /// Once the slot exists, written only inside the registry lock. It is an
    /// atomic because the executor reads the same flag from its blocking thread
    /// at the bind-to-execute boundary ([`WorkSlot::cancel_latch`]), where
    /// taking the registry lock would put a `DuckDB` bind inside it.
    cancelled: Arc<AtomicBool>,
}

struct RetainedEntry {
    id: u64,
    kind: WorkKind,
    owner: WorkOwner,
    user: Option<Arc<str>>,
    display: Option<String>,
    started_work: bool,
    registered_at: Instant,
    /// When the request recorded its outcome and left the work running.
    /// `None` while a request is still waiting on it.
    retained_since: Option<Instant>,
}

struct PendingEntry {
    cancelled: Arc<AtomicBool>,
    wake: tokio::sync::watch::Sender<bool>,
}

/// Critical sections here are map operations only — no awaits, no
/// query execution.
struct Registry {
    pending: HashMap<u64, PendingEntry>,
    interrupts: HashMap<u64, InterruptSlot>,
    retained: HashMap<u64, RetainedEntry>,
    idle: Vec<Executor>,
}

impl Registry {
    fn request_cancel(&mut self, id: u64) -> bool {
        if let Some(slot) = self.interrupts.get_mut(&id) {
            slot.cancelled.store(true, Ordering::SeqCst);
            if let Some(handle) = &slot.handle {
                handle.interrupt();
            }
            return true;
        }
        let Some(entry) = self.pending.get(&id) else {
            return false;
        };
        // The flag before the wake: a wait the wake ends finds it latched.
        entry.cancelled.store(true, Ordering::SeqCst);
        entry.wake.send_replace(true);
        true
    }

    fn retained_count(&self) -> usize {
        self.retained
            .values()
            .filter(|entry| entry.retained_since.is_some())
            .count()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StartRefusal {
    Expired,
    Cancelled,
}

impl StartRefusal {
    fn as_str(self) -> &'static str {
        match self {
            Self::Expired => "expired",
            Self::Cancelled => "cancelled",
        }
    }
}

/// A channel closed while its value is still false is not a cancel:
/// promotion or the request's guard closed it, and the wait goes on under
/// its deadline.
#[derive(Debug, Clone, Default)]
pub(crate) struct Wake(Option<tokio::sync::watch::Receiver<bool>>);

impl Wake {
    pub(crate) async fn cancelled(&self) {
        if let Some(receiver) = &self.0 {
            let mut receiver = receiver.clone();
            if receiver.wait_for(|cancelled| *cancelled).await.is_ok() {
                return;
            }
        }
        std::future::pending::<()>().await;
    }

    /// The cancel is polled first, so a cancel already latched beats a
    /// `wait` that is ready on the same poll: a request that was told
    /// "cancelled" never takes what it waited for.
    pub(crate) async fn run<F: std::future::Future>(
        &self,
        deadline: Deadline,
        wait: F,
    ) -> Result<F::Output, StartRefusal> {
        tokio::select! {
            biased;
            () = self.cancelled() => Err(StartRefusal::Cancelled),
            out = deadline.run(wait) => {
                out.map_err(|crate::deadline::Expired| StartRefusal::Expired)
            }
        }
    }
}

pub(crate) struct Pending {
    registry: Arc<Mutex<Registry>>,
    id: u64,
    kind: WorkKind,
    cancelled: Arc<AtomicBool>,
    wake: Wake,
    refusal: RefusalOnce,
}

impl Pending {
    pub(crate) fn wake(&self) -> &Wake {
        &self.wake
    }

    pub(crate) fn stop(&self) -> Stop {
        Stop::from_flag(Arc::clone(&self.cancelled))
    }

    /// Whether the request's cancel flag is latched: by a cancel by id, or
    /// by a preparation the request stopped waiting on, which asks its job
    /// to stop through the same flag.
    pub(crate) fn latched(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub(crate) fn refuse(&self) -> ServerError {
        self.refusal
            .refuse(self.id, self.kind, StartRefusal::Cancelled)
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        let mut registry = self.registry.lock();
        // This request's own entry only. After promotion the id is in
        // `interrupts`, which only the work's slot removes.
        if registry
            .pending
            .get(&self.id)
            .is_some_and(|entry| Arc::ptr_eq(&entry.cancelled, &self.cancelled))
        {
            registry.pending.remove(&self.id);
        }
    }
}

#[derive(Debug, Clone)]
pub struct RetainedWork {
    pub id: u64,
    pub kind: WorkKind,
    pub started: bool,
    pub retained: Duration,
    pub(crate) owner: WorkOwner,
    pub(crate) user: Option<Arc<str>>,
    pub(crate) display: Option<String>,
}

struct WorkSlot {
    registry: Arc<Mutex<Registry>>,
    id: u64,
    kind: WorkKind,
    /// Shared with the request future, written only inside the registry
    /// lock. It outlives the registry entry, so a request that finds the
    /// entry already gone still classifies its own outcome correctly.
    started: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    executor: Option<Executor>,
    /// `None` for a lane over a recorded run, whose files never change.
    publication: Option<tokio::sync::OwnedRwLockReadGuard<()>>,
    permit: Option<OwnedSemaphorePermit>,
    timing: Option<QueryTiming>,
    physical: PhysicalOutcome,
    holds: Option<Arc<dyn Any + Send + Sync>>,
}

static CLOCK_OFF: PhaseClock = PhaseClock::off();

impl WorkSlot {
    fn clock(&self) -> &PhaseClock {
        self.timing.as_ref().map_or(&CLOCK_OFF, QueryTiming::clock)
    }

    /// Outside [`Self::begin_work`] because that runs under the registry
    /// lock, and the account's lock is never taken inside it.
    fn started_timing(&self) {
        if let Some(timing) = &self.timing {
            timing.clock().exit(QueryPhase::Startup);
            timing.mark_work_started();
        }
    }

    /// The worker's last word, before this slot drops: close the clock, so
    /// its final totals are on the shared clock before [`Drop`] takes the
    /// registry lock.
    fn finish<T>(&mut self, result: &Result<T, ServerError>) {
        let started = self.started.load(Ordering::SeqCst);
        self.physical = if started {
            PhysicalOutcome::of_result(result)
        } else {
            PhysicalOutcome::NotStarted
        };
        let Some(timing) = &self.timing else {
            return;
        };
        let clock = timing.clock();
        if !started {
            clock.discard(QueryPhase::Startup);
        }
        // Closing books any phase still active; the totals it returns are
        // read from the clock by whoever writes the account.
        let _ = clock.close_at(Instant::now());
    }

    fn executor(&self) -> &Executor {
        self.executor
            .as_ref()
            .expect("the executor leaves the slot only in Drop")
    }

    fn publish_interrupt(&self, handle: Arc<duckdb::InterruptHandle>) -> bool {
        let mut registry = self.registry.lock();
        match registry.interrupts.get_mut(&self.id) {
            Some(slot) if !slot.cancelled.load(Ordering::SeqCst) => {
                slot.handle = Some(handle);
                true
            }
            _ => false,
        }
    }

    /// An interrupt raised while a statement is binding may be swallowed, and
    /// binding is where an expensive query spends its time (ADR-0024).
    fn cancel_latch(&self) -> CancelLatch {
        CancelLatch::new(Arc::clone(&self.cancelled))
    }

    fn begin_work(&self, deadline: Deadline) -> Result<(), StartRefusal> {
        let mut registry = self.registry.lock();
        if registry
            .interrupts
            .get(&self.id)
            .is_some_and(|slot| slot.cancelled.load(Ordering::SeqCst))
        {
            return Err(StartRefusal::Cancelled);
        }
        if deadline.expired() {
            return Err(StartRefusal::Expired);
        }
        if let Some(entry) = registry.retained.get_mut(&self.id) {
            entry.started_work = true;
        }
        self.started.store(true, Ordering::SeqCst);
        Ok(())
    }
}

impl Drop for WorkSlot {
    fn drop(&mut self) {
        // (a) Stop being interruptible, return the executor, and stop
        // being retained — one critical section, because an interrupt
        // that fired here would land on the next query to take this
        // executor.
        let retained = {
            let mut registry = self.registry.lock();
            registry.interrupts.remove(&self.id);
            let entry = registry.retained.remove(&self.id);
            if let Some(executor) = self.executor.take() {
                registry.idle.push(executor);
            }
            entry.and_then(|entry| {
                entry
                    .retained_since
                    .map(|since| (since.elapsed(), entry.started_work))
            })
        };
        // (b) then the publication guard, (c) then the permit: a
        // compaction publisher or a cutover waiting on either must not
        // observe capacity it cannot use.
        drop(self.publication.take());
        drop(self.permit.take());
        if let Some((retained_for, started)) = retained {
            let totals = if started {
                self.timing
                    .as_ref()
                    .and_then(|timing| timing.clock().close_at(Instant::now()))
            } else {
                None
            };
            emit_reclaimed(&Reclaim {
                query_id: self.id,
                kind: self.kind,
                retained_for,
                correlation: self.timing.as_ref().map(QueryTiming::correlation),
                physical: if started {
                    self.physical
                } else {
                    PhysicalOutcome::NotStarted
                },
                totals: totals.as_ref(),
            });
        }
        // Last, the caller's charges (ADR-0057). They stop being charged
        // when the permit does and never before. Dropping them runs the
        // caller's code, which finds everything this slot held released.
        drop(self.holds.take());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RequestEnd {
    /// This call stamped the work retained: its slot was still
    /// registered, so its cleanup has yet to run.
    retained: bool,
    started: bool,
}

struct RequestGuard<'a> {
    pool: &'a ExecutorPool,
    id: u64,
    kind: WorkKind,
    started: Arc<AtomicBool>,
    timing: Option<QueryTiming>,
    armed: bool,
}

impl<'a> RequestGuard<'a> {
    fn new(
        pool: &'a ExecutorPool,
        id: u64,
        kind: WorkKind,
        started: &Arc<AtomicBool>,
        timing: Option<&QueryTiming>,
    ) -> Self {
        if let Some(timing) = timing {
            timing.hand_emit_to_pool();
        }
        Self {
            pool,
            id,
            kind,
            started: Arc::clone(started),
            timing: timing.cloned(),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
        if let Some(timing) = &self.timing {
            timing.hand_emit_back();
        }
    }

    fn abandon(&mut self) -> bool {
        self.armed = false;
        self.pool.cancel_by_id(self.id);
        let end = self.pool.mark_request_finished(self.id, &self.started);
        self.settle(end, Outcome::Timeout, Outcome::CapacityRefused);
        end.started
    }

    fn settle(&self, end: RequestEnd, if_started: Outcome, if_not_started: Outcome) {
        let Some(timing) = &self.timing else {
            return;
        };
        let now = Instant::now();
        if end.started {
            // The worker marks the account itself, after its transition
            // and outside the registry lock, so it may not have yet.
            timing.mark_work_started();
            if end.retained {
                timing.emit_partial(if_started);
            } else {
                timing.emit_complete(if_started, now);
            }
        } else {
            timing.clock().discard(QueryPhase::Startup);
            timing.emit_complete(if_not_started, now);
        }
    }
}

impl Drop for RequestGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        tracing::info!(
            event_type = "query_request_abandoned",
            query_id = self.id,
            kind = self.kind.as_str(),
            "the caller stopped waiting; its work keeps its permit until cleanup"
        );
        self.pool.cancel_by_id(self.id);
        let end = self.pool.mark_request_finished(self.id, &self.started);
        // A caller that walked away before the work started was not
        // refused anything: it is abandoned either way.
        self.settle(end, Outcome::Abandoned, Outcome::Abandoned);
    }
}

fn account_refused(timing: Option<&QueryTiming>) {
    if let Some(timing) = timing {
        timing.emit_complete(Outcome::CapacityRefused, Instant::now());
    }
}

/// One refused request can refuse twice: the request future gives up on
/// its budget, which latches cancellation, and the worker it released
/// then refuses to start on that latch. It is one refusal, so the first
/// to arrive logs and names the reason, and the second is silent.
#[derive(Clone)]
struct RefusalOnce {
    logged: Arc<AtomicBool>,
    pool: u64,
}

impl RefusalOnce {
    fn new(pool: u64) -> Self {
        Self {
            logged: Arc::new(AtomicBool::new(false)),
            pool,
        }
    }

    fn refuse(&self, id: u64, kind: WorkKind, refusal: StartRefusal) -> ServerError {
        if !self.logged.swap(true, Ordering::SeqCst) {
            tracing::info!(
                event_type = "query_not_started",
                pool = self.pool,
                query_id = id,
                kind = kind.as_str(),
                reason = refusal.as_str(),
                "query work refused before it started"
            );
        }
        ServerError::CapacityNotStarted
    }

    fn refuse_outcome(&self, id: u64, kind: WorkKind, refusal: StartRefusal) -> ExecuteOutcome {
        ExecuteOutcome {
            result: Err(self.refuse(id, kind, refusal)),
            debug: None,
            severity_columns: Vec::new(),
            histogram: None,
        }
    }

    fn refused(&self) -> bool {
        self.logged.load(Ordering::SeqCst)
    }
}

/// Deterministic control over the blocking worker, for tests only.
#[cfg(any(test, feature = "test-support"))]
pub mod seam {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex as StdMutex};

    pub const INJECTED_PANIC_PAYLOAD: &str = "zz-test-injected-panic-payload";

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum Seam {
        /// On the request's own task, once a lane subscribed to its
        /// request's cancel and before it waits for an executor permit. A
        /// test may watch this seam and must never hold it: a hold parks a
        /// runtime thread, not a blocking worker.
        Queued,
        /// First statement of the worker, before the interrupt handle is
        /// published and before the work-start transition.
        Entry,
        /// Immediately after a successful work-start transition, before
        /// source discovery. The capture lane selects and measures a
        /// window's source first, so a capture refused for its source
        /// budget never reaches it.
        Started,
        /// Inside the capture lane's statement run, once the catalog pins
        /// and the hot buffer's snapshot (when the read has one) are
        /// taken and before the engine runs the statement: an event
        /// ingested while a worker sits here is in neither snapshot. The
        /// capture lane only.
        Snapshotted,
        /// Once the engine returned, before its answer is read: the slot
        /// still holds the permit, the executor, a window's publication
        /// guard and the work's holds, and `finish` has not run. The
        /// capture lane only.
        Returned,
        /// Inside a capture's `finish`, once its file is staged and before
        /// it is written. The capture lane only.
        Writing,
        /// In the query lane, once the engine returned its rows and before
        /// the post phase walks them for presentation: a cancel latched
        /// here is one the presentation walks read. Not the recorded-source
        /// lane.
        Presenting,
        /// After the work produced its answer, before cleanup.
        Finished,
        /// After cleanup released everything the work held, before the
        /// worker hands its answer back: the slot is gone and the request
        /// has not heard. The query, export and capture lanes only; the
        /// sample lane has no such point.
        Released,
        /// In a captured result's page or cell read, once its row says it
        /// is readable and before its file's digest is checked. The read
        /// runs on a detached executor, outside any slot.
        Verifying,
        /// In a captured result's page or cell read, once its file's
        /// digest matched and before the engine opens the file by path.
        Opening,
        /// In a query preparation job, once it holds its slot and before
        /// its work runs. The preparation allowance only.
        Preparing,
        /// In a query preparation job, once its work returned and before
        /// it sends its answer: the job still holds its slot. The
        /// preparation allowance only.
        Prepared,
        /// In a run claim's window resolution on the blocking pool, while
        /// the resolution holds a clone of the run's lease. Reached
        /// through [`super::RunLease::reach`].
        Resolving,
        /// On the scheduler tick's task, before each live lookup of a
        /// schedule's owning key. A test may watch this seam to count the
        /// lookups and must never hold it: a hold parks a runtime thread,
        /// not a blocking worker.
        KeyChecked,
        /// On a scheduled or manual run's own task, once its query
        /// returned rows and before its result is written: the run's row
        /// is still running and its frame still holds the run's lease.
        /// The frame reaches it inside `block_in_place` on a
        /// multi-thread runtime, so a test that holds it runs on one.
        RunWriting,
    }

    #[derive(Debug)]
    pub struct Gate {
        open: StdMutex<bool>,
        opened: Condvar,
        arrivals: AtomicUsize,
        panics: bool,
    }

    impl Gate {
        #[must_use]
        pub fn arrivals(&self) -> usize {
            self.arrivals.load(Ordering::SeqCst)
        }

        pub fn release(&self) {
            *self.open.lock().expect("seam gate poisoned") = true;
            self.opened.notify_all();
        }

        /// A held worker never gives up on a wall clock: a lapse would let a
        /// starved test's worker leave the seam on its own and answer before
        /// the test drove the state the hold pins. [`Session`]'s drop
        /// releases every gate, displaced ones included, so a failing test
        /// still frees its workers.
        fn pass(&self) {
            self.arrivals.fetch_add(1, Ordering::SeqCst);
            let mut open = self.open.lock().expect("seam gate poisoned");
            while !*open {
                open = self.opened.wait(open).expect("seam gate poisoned");
            }
            drop(open);
            assert!(!self.panics, "{INJECTED_PANIC_PAYLOAD}");
        }
    }

    /// The seams of ONE pool, not per process: the plain `cargo test`
    /// harness runs tests in threads rather than nextest's separate
    /// processes.
    #[derive(Debug, Default)]
    pub struct Table {
        gates: StdMutex<HashMap<Seam, Arc<Gate>>>,
        /// Gates a later install on the same seam replaced. A worker can
        /// still be parked at one, so [`Table::release_all`] opens them
        /// too.
        displaced: StdMutex<Vec<Arc<Gate>>>,
    }

    impl Table {
        fn install(&self, seam: Seam, gate: Gate) -> Arc<Gate> {
            let gate = Arc::new(gate);
            let displaced = self
                .gates
                .lock()
                .expect("seam table poisoned")
                .insert(seam, Arc::clone(&gate));
            if let Some(displaced) = displaced {
                self.displaced
                    .lock()
                    .expect("seam table poisoned")
                    .push(displaced);
            }
            gate
        }

        #[must_use]
        pub fn hold(&self, seam: Seam) -> Arc<Gate> {
            self.install(
                seam,
                Gate {
                    open: StdMutex::new(false),
                    opened: Condvar::new(),
                    arrivals: AtomicUsize::new(0),
                    panics: false,
                },
            )
        }

        #[must_use]
        pub fn watch(&self, seam: Seam) -> Arc<Gate> {
            self.install(
                seam,
                Gate {
                    open: StdMutex::new(true),
                    opened: Condvar::new(),
                    arrivals: AtomicUsize::new(0),
                    panics: false,
                },
            )
        }

        #[must_use]
        pub fn panic_at(&self, seam: Seam) -> Arc<Gate> {
            self.install(
                seam,
                Gate {
                    open: StdMutex::new(true),
                    opened: Condvar::new(),
                    arrivals: AtomicUsize::new(0),
                    panics: true,
                },
            )
        }

        pub(crate) fn reach(&self, seam: Seam) {
            let gate = self
                .gates
                .lock()
                .expect("seam table poisoned")
                .get(&seam)
                .map(Arc::clone);
            if let Some(gate) = gate {
                gate.pass();
            }
        }

        pub fn release_all(&self) {
            let gates = std::mem::take(&mut *self.gates.lock().expect("seam table poisoned"));
            let displaced =
                std::mem::take(&mut *self.displaced.lock().expect("seam table poisoned"));
            for gate in gates.values().chain(&displaced) {
                gate.release();
            }
        }
    }

    /// Dropping releases before forgetting, so a test that fails while a
    /// worker is parked still lets that worker out: the runtime's own
    /// drop waits for blocking tasks, and a gate nobody opens would turn
    /// a failed assertion into a hung test process.
    #[derive(Debug)]
    pub struct Session(pub(crate) Arc<Table>);

    impl Session {
        #[must_use]
        pub fn hold(&self, seam: Seam) -> Arc<Gate> {
            self.0.hold(seam)
        }

        #[must_use]
        pub fn watch(&self, seam: Seam) -> Arc<Gate> {
            self.0.watch(seam)
        }

        #[must_use]
        pub fn panic_at(&self, seam: Seam) -> Arc<Gate> {
            self.0.panic_at(seam)
        }
    }

    impl Drop for Session {
        fn drop(&mut self) {
            self.0.release_all();
        }
    }
}

macro_rules! worker_seam {
    ($seams:expr, $seam:ident) => {
        #[cfg(any(test, feature = "test-support"))]
        {
            $seams.reach(crate::pool::seam::Seam::$seam);
        }
    };
}

/// Only populated when `capture_debug` is true (i.e. query log is active).
#[derive(Debug)]
pub struct PoolDebugInfo {
    /// The computed source argument passed to `read_parquet()`.
    pub computed_source: String,
    pub glob_count: usize,
    pub service_filter: Option<String>,
    pub time_filter_secs: Option<u64>,
    /// Hot buffer status: "disabled", "empty", or "active".
    pub hot_status: &'static str,
    pub hot_events: usize,
    pub hot_batches: usize,
    pub hot_bytes: usize,
    /// Generated SQL (parameterized).
    pub sql: String,
    /// SQL parameter values (Display form).
    pub params: Vec<String>,
}

#[derive(Debug)]
pub struct PoolExclusive {
    _permits: tokio::sync::OwnedSemaphorePermit,
}

#[derive(Debug)]
pub struct ExecuteOutcome {
    pub result: Result<QueryResult, ServerError>,
    pub debug: Option<PoolDebugInfo>,
    /// Result columns carrying the `SEVERITY` pin at the end of the
    /// pipeline (ADR-0013 ruling 9). Empty for a failed, timed-out or
    /// never-started run.
    pub severity_columns: Vec<String>,
    /// The final result counted by `_time` before the page is cut, when the
    /// work asked ([`WorkContext::with_histogram`]).
    pub histogram: Option<EventHistogram>,
}

/// What a capture reads its stored events from (ADR-0057).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureSource {
    /// A live window `[start, end)`. The read includes the hot buffer, as
    /// `/query`'s does.
    Window {
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        /// `[capture] source_max_bytes`.
        max_bytes: u64,
    },
    /// `| from saved`: a source already resolved to a recorded run's
    /// parquet, with no hot buffer and no publication guard. It is not
    /// measured.
    Resolved { source: String },
}

/// `finish` runs on the worker thread before the slot releases anything,
/// so the file it writes is written under the permit, on the slot's own
/// executor, inside the publication guard a window holds.
#[derive(Debug)]
pub struct CaptureInput<'a> {
    /// Every row the executed pipeline produced under the lane's
    /// [`RowCap`], uncut: `finish` makes the capture's own cut and counts
    /// the total from it.
    pub result: QueryResult,
    /// Its interrupt handle is registered, so a cancellation breaks a `COPY`
    /// in flight.
    pub executor: &'a Executor,
    /// The query's clock: book the file write under [`QueryPhase::Write`].
    pub clock: &'a PhaseClock,
    /// The work's cancellation latch. The file write takes none, so read
    /// it before starting one.
    pub cancel: &'a CancelLatch,
    /// The bytes a window's source budget measured; `None` for a resolved
    /// source, which is not measured.
    pub source_bytes: Option<u64>,
    #[cfg(any(test, feature = "test-support"))]
    pub seams: &'a seam::Table,
}

pub type CaptureFinish<T> = Box<dyn FnOnce(CaptureInput<'_>) -> Result<T, ServerError> + Send>;

/// The result columns that carry the `SEVERITY` pin at the end of the
/// pipeline (ADR-0013 ruling 9).
///
/// `pins` must be the snapshot the run executed under, never a fresh read:
/// `repin --to severity` can retype a field mid-flight.
///
/// Call it on the blocking pool, inside the query's permit, never on a
/// reactor thread: its cost is the client's choice of DSL.
///
/// Only a pipe stage can move a pin off the name it was pinned under
/// (`sev()` included), and a pipe stage needs a `|`, so a DSL without one
/// is answered from the root scope directly.
///
/// # Errors
/// [`EngineError::Cancelled`] once `cancel` is latched during the parse.
pub(crate) fn severity_columns_for(
    pins: &trawl_core::schema::FieldTypes,
    dsl: &str,
    cancel: &CancelLatch,
) -> Result<Vec<String>, EngineError> {
    let root = trawl_core::pin_scope::PinScope::root(pins);
    if !dsl.contains('|') {
        return Ok(trawl_core::pin_scope::severity_output_columns(&[], &root));
    }
    // The walk reads pins and capture names and runs no pattern, so its
    // parse builds none.
    let Ok(query) = trawl_core::parser::parse_unbuilt_cancellable(dsl, &cancel.stop())? else {
        return Ok(Vec::new());
    };
    Ok(trawl_core::pin_scope::severity_output_columns(
        &query.pipeline,
        &root,
    ))
}

fn present(
    result: Result<QueryResult, ServerError>,
    pins: &trawl_core::schema::FieldTypes,
    dsl: &str,
    histogram: Option<HistogramAsk>,
    cancel: &CancelLatch,
    clock: &PhaseClock,
) -> (
    Result<QueryResult, ServerError>,
    Vec<String>,
    Option<EventHistogram>,
) {
    let Ok(rows) = result else {
        return (result, Vec::new(), None);
    };
    let presented = clock.time(QueryPhase::Post, || {
        let columns = severity_columns_for(pins, dsl, cancel)?;
        let histogram = histogram
            .map(|ask| crate::histogram::count(&rows, ask.kept_some, cancel))
            .transpose()?;
        Ok::<_, EngineError>((columns, histogram))
    });
    match presented {
        Ok((columns, histogram)) => (Ok(rows), columns, histogram),
        Err(error) => (Err(error.into()), Vec::new(), None),
    }
}

#[derive(Clone)]
pub struct ExecutorPool {
    publication: Arc<crate::publication::PublicationGate>,
    base_dir: Arc<str>,
    /// Full recursive glob for queries without a time filter.
    fallback_glob: Arc<str>,
    semaphore: Arc<Semaphore>,
    max_concurrent: usize,
    max_result_rows: usize,
    next_id: Arc<AtomicU64>,
    /// A test process builds many pools, each with its own id space starting
    /// at zero, so the pair (`instance`, `query_id`) is what identifies a
    /// unit of work when several pools are logging at once.
    instance: u64,
    registry: Arc<Mutex<Registry>>,
    hot_buffer: Option<Arc<HotBuffer>>,
    /// Field catalog whose full pin snapshot types every query's
    /// search-stage comparisons. Defaults to an empty catalog; the server
    /// wires the shared cache via [`Self::with_field_catalog`].
    field_catalog: Arc<crate::catalog::FieldCatalog>,
    #[cfg(any(test, feature = "test-support"))]
    seams: Arc<seam::Table>,
    clone_source: Arc<Mutex<Executor>>,
    preparation: crate::prepare::Preparation,
    scheduled: Arc<Semaphore>,
    manual: Arc<ManualGate>,
    /// The scratch directory's lock is not held here but by every connection
    /// to the query database ([`Executor::open_query_database`]), so it stays
    /// held while any worker's connection is open, the pool gone or not.
    scratch: QueryScratch,
    /// The temp directory a [`Self::for_test`] pool's scratch lives in,
    /// declared last so it is removed after the pool's own connections
    /// closed. A worker that outlives the pool does not hold it: a test
    /// that drops a pool with retained work makes its own scratch
    /// directory.
    #[cfg(any(test, feature = "test-support"))]
    _test_scratch: Option<Arc<tempfile::TempDir>>,
}

impl std::fmt::Debug for ExecutorPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (active, retained, idle) = {
            let registry = self.registry.lock();
            (
                registry.interrupts.len(),
                registry.retained_count(),
                registry.idle.len(),
            )
        };
        f.debug_struct("ExecutorPool")
            .field("base_dir", &self.base_dir)
            .field("fallback_glob", &self.fallback_glob)
            .field("semaphore", &self.semaphore)
            .field("max_result_rows", &self.max_result_rows)
            .field("next_id", &self.next_id)
            .field("active_queries", &active)
            .field("retained_queries", &retained)
            .field("idle_executors", &idle)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy)]
enum ReadInput<'a> {
    Corpus(Option<(&'a Arc<HotBuffer>, &'a QueryScratch)>),
    Recorded,
}

#[allow(clippy::too_many_arguments)]
fn run_query_blocking(
    executor: &Executor,
    cancel: &CancelLatch,
    clock: &PhaseClock,
    dsl: &str,
    source: &str,
    input: ReadInput<'_>,
    pins: &trawl_core::schema::FieldTypes,
    cap: RowCap,
    capture_debug: bool,
    snapshotted: impl FnOnce(),
) -> (Result<QueryResult, ServerError>, Option<PoolDebugInfo>) {
    let hot = match input {
        ReadInput::Corpus(hot) => hot,
        ReadInput::Recorded => None,
    };
    let hot_buffer = hot.map(|(buffer, _)| buffer);
    let hot_snapshot = hot_source(hot.map_or(Ok(None), |(hb, scratch)| {
        clock.time(QueryPhase::HotSnapshot, || hb.snapshot(scratch))
    }));

    let debug = if capture_debug {
        match clock.time(QueryPhase::Emit, || {
            capture_pool_debug(dsl, source, hot_buffer, pins, cancel)
        }) {
            Ok(debug) => Some(debug),
            Err(error) => return (Err(error.into()), None),
        }
    } else {
        None
    };
    let hot_snapshot = match hot_snapshot {
        Ok(snapshot) => snapshot,
        Err(error) => return (Err(error), debug),
    };
    snapshotted();

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Some(ref hot) = hot_snapshot {
            // `hot_source` refused a path that is not UTF-8.
            let hot_path = hot.path().to_str().unwrap_or_default();
            executor
                .cancellable(cancel)
                .timed(clock)
                .run_query_with_hot(
                    dsl,
                    source,
                    hot_path,
                    &hot.columns,
                    &hot.field_types,
                    pins,
                    cap,
                )
                .map_err(ServerError::from)
        } else {
            let executor = executor.cancellable(cancel).timed(clock);
            match input {
                ReadInput::Corpus(_) => executor.run_query(dsl, source, pins, cap),
                ReadInput::Recorded => executor.run_recorded_query(dsl, source, pins, cap),
            }
            .map_err(ServerError::from)
        }
    }));
    let result = match result {
        Ok(r) => r,
        // The payload is dropped unread: it can quote anything DuckDB or
        // the executor had in hand (ADR-0040).
        Err(_payload) => Err(ServerError::Panicked("query")),
    };

    (result, debug)
}

/// A snapshot whose path `DuckDB`'s file reader cannot take because it is
/// not UTF-8 refuses the read with [`ServerError::HotSnapshot`] (ADR-0041).
fn hot_source(
    snapshot: std::io::Result<Option<HotSnapshot>>,
) -> Result<Option<HotSnapshot>, ServerError> {
    match snapshot {
        Ok(Some(snapshot)) if snapshot.path().to_str().is_none() => {
            Err(ServerError::HotSnapshot(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "hot snapshot path is not valid UTF-8",
            )))
        }
        Ok(snapshot) => Ok(snapshot),
        Err(error) => Err(ServerError::HotSnapshot(error)),
    }
}

/// # Errors
/// [`EngineError::Cancelled`] once `cancel` is latched during the parse.
fn capture_pool_debug(
    dsl: &str,
    source: &str,
    hot_buffer: Option<&Arc<HotBuffer>>,
    pins: &trawl_core::schema::FieldTypes,
    cancel: &CancelLatch,
) -> Result<PoolDebugInfo, EngineError> {
    use trawl_core::ast::{FieldFilter, FilterOp, FilterValue, Scalar, SearchToken};

    let glob_count = if source.starts_with('[') {
        source.matches(',').count() + 1
    } else {
        1
    };
    // Emission runs no pattern, so the parse builds none.
    let (service_filter, time_filter_secs, sql, params) =
        if let Ok(ast) = trawl_core::parser::parse_unbuilt_cancellable(dsl, &cancel.stop())? {
            let service = ast.search.groups.first().and_then(|g| {
                g.iter().find_map(|t| {
                    if let SearchToken::FieldFilter(FieldFilter {
                        field,
                        op: FilterOp::Eq,
                        value: FilterValue::Single(Scalar::Exact(s)),
                    }) = &t.node
                        && field == "service"
                    {
                        return Some(s.clone());
                    }
                    None
                })
            });
            let time_secs = ast
                .search
                .time_filter
                .as_ref()
                .map(|tf| tf.node.duration.to_seconds());
            // Pin-aware, so the debug-log SQL preview types comparisons the
            // way the executed query does. It is a preview, not a transcript:
            // the logged source can be wider than the one that ran.
            let (sql, params) = match trawl_core::emitter::emit_with_pins(
                &ast,
                source,
                pins,
                &trawl_core::emitter::InputColumns::unprobed(),
                trawl_core::context::EvalContext::capture(),
            ) {
                Ok(emitted) => (
                    emitted.sql,
                    emitted.params.iter().map(ToString::to_string).collect(),
                ),
                Err(_) => (String::new(), vec![]),
            };
            (service, time_secs, sql, params)
        } else {
            (None, None, String::new(), vec![])
        };

    let (hot_status, hot_events, hot_batches, hot_bytes) = match hot_buffer {
        None => ("disabled", 0, 0, 0),
        Some(hb) => {
            let events = hb.event_count();
            if events == 0 {
                ("empty", 0, hb.batch_count(), 0)
            } else {
                ("active", events, hb.batch_count(), hb.byte_count())
            }
        }
    };

    Ok(PoolDebugInfo {
        computed_source: source.to_owned(),
        glob_count,
        service_filter,
        time_filter_secs,
        hot_status,
        hot_events,
        hot_batches,
        hot_bytes,
        sql,
        params,
    })
}

impl ExecutorPool {
    #[must_use]
    pub fn publication(&self) -> Arc<crate::publication::PublicationGate> {
        Arc::clone(&self.publication)
    }

    /// # Errors
    /// The database or its spill directory could not be set up
    /// ([`Executor::open_query_database`]). Fatal at startup.
    pub fn new(
        base_dir: String,
        max_concurrent: usize,
        max_result_rows: usize,
        hot_buffer: Option<Arc<HotBuffer>>,
        scratch: Arc<ScratchLock>,
        budgets: &QueryBudgets,
    ) -> Result<Self, trawl_engine::error::EngineError> {
        let query_scratch = scratch.scratch().clone();
        let root = Executor::open_query_database(scratch, budgets)?;
        let mut executors = Vec::with_capacity(max_concurrent);
        for _ in 0..max_concurrent {
            executors.push(root.try_clone()?);
        }

        let fallback_glob: Arc<str> =
            Arc::from(format!("{}/**/*.parquet", base_dir.trim_end_matches('/')));

        let publication = hot_buffer.as_ref().map_or_else(
            || Arc::new(crate::publication::PublicationGate::new()),
            |buffer| buffer.publication(),
        );
        publication.initialize(std::path::Path::new(&base_dir));
        #[cfg(any(test, feature = "test-support"))]
        let seams = Arc::new(seam::Table::default());
        let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
        let preparation =
            crate::prepare::Preparation::new(crate::prepare::allowance(cores, max_concurrent));
        #[cfg(any(test, feature = "test-support"))]
        let preparation = preparation.with_seams(Arc::clone(&seams));
        Ok(Self {
            publication,
            base_dir: Arc::from(base_dir),
            fallback_glob,
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            max_concurrent,
            max_result_rows,
            next_id: Arc::new(AtomicU64::new(0)),
            instance: NEXT_POOL_INSTANCE.fetch_add(1, Ordering::Relaxed),
            registry: Arc::new(Mutex::new(Registry {
                pending: HashMap::new(),
                interrupts: HashMap::new(),
                retained: HashMap::new(),
                idle: executors,
            })),
            hot_buffer,
            field_catalog: Arc::new(crate::catalog::FieldCatalog::new()),
            #[cfg(any(test, feature = "test-support"))]
            seams,
            clone_source: Arc::new(Mutex::new(root)),
            preparation,
            scheduled: Arc::new(Semaphore::new(scheduled_cap(max_concurrent))),
            manual: Arc::new(ManualGate {
                limit: manual_cap(max_concurrent),
                holders: Mutex::new(HashMap::new()),
            }),
            scratch: query_scratch,
            #[cfg(any(test, feature = "test-support"))]
            _test_scratch: None,
        })
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test(
        base_dir: String,
        max_concurrent: usize,
        max_result_rows: usize,
        hot_buffer: Option<Arc<HotBuffer>>,
    ) -> Result<Self, trawl_engine::error::EngineError> {
        Self::for_test_with_budgets(
            base_dir,
            max_concurrent,
            max_result_rows,
            hot_buffer,
            &QueryBudgets {
                spill_max_bytes: crate::config::DEFAULT_QUERY_SPILL_MAX_BYTES,
                memory_limit: crate::config::DEFAULT_QUERY_MEMORY_LIMIT,
                threads: crate::config::DEFAULT_QUERY_THREADS,
            },
        )
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test_with_budgets(
        base_dir: String,
        max_concurrent: usize,
        max_result_rows: usize,
        hot_buffer: Option<Arc<HotBuffer>>,
        budgets: &QueryBudgets,
    ) -> Result<Self, trawl_engine::error::EngineError> {
        let dir = tempfile::tempdir()?;
        let scratch = ScratchLock::open(&dir.path().join("query-scratch"))
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let mut pool = Self::new(
            base_dir,
            max_concurrent,
            max_result_rows,
            hot_buffer,
            Arc::new(scratch),
            budgets,
        )?;
        #[expect(
            clippy::used_underscore_binding,
            reason = "the field is never read: it is held so the directory outlives the pool"
        )]
        {
            pool._test_scratch = Some(Arc::new(dir));
        }
        Ok(pool)
    }

    #[must_use]
    pub fn scratch(&self) -> &QueryScratch {
        &self.scratch
    }

    pub fn diagnostic_connection(
        &self,
    ) -> Result<trawl_engine::executor::DiagnosticConnection, trawl_engine::error::EngineError>
    {
        self.clone_source.lock().diagnostic_connection()
    }

    /// A connection to the query database that takes no permit. It is not
    /// interruptible through the pool.
    pub fn detached_executor(&self) -> Result<Executor, trawl_engine::error::EngineError> {
        self.clone_source.lock().try_clone()
    }

    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn seams(&self) -> seam::Session {
        seam::Session(Arc::clone(&self.seams))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn reach(&self, seam: seam::Seam) {
        self.seams.reach(seam);
    }

    #[must_use]
    pub fn with_field_catalog(mut self, catalog: Arc<crate::catalog::FieldCatalog>) -> Self {
        self.field_catalog = catalog;
        self
    }

    /// `work` is taken whole so its holds move into the slot: a hold left
    /// with the request future would be released when the request stops
    /// waiting, while the work may still be running.
    fn begin_slot(
        &self,
        id: u64,
        work: WorkContext,
        display: Option<&str>,
        permit: OwnedSemaphorePermit,
        publication: Option<tokio::sync::OwnedRwLockReadGuard<()>>,
    ) -> Result<WorkSlot, ServerError> {
        let mut registry = self.registry.lock();
        let Some(executor) = registry.idle.pop() else {
            drop(registry);
            tracing::error!(
                event_type = "pool_invariant",
                query_id = id,
                kind = work.kind.as_str(),
                "a permit was granted with no idle executor behind it"
            );
            // The slot's release order, with no slot: the holds go after
            // the permit here too.
            drop(publication);
            drop(permit);
            drop(work);
            return Err(ServerError::Internal("executor pool inconsistent".into()));
        };
        let cancelled = registry
            .pending
            .remove(&id)
            .map_or_else(|| Arc::new(AtomicBool::new(false)), |entry| entry.cancelled);
        registry.interrupts.insert(
            id,
            InterruptSlot {
                handle: None,
                cancelled: Arc::clone(&cancelled),
            },
        );
        registry.retained.insert(
            id,
            RetainedEntry {
                id,
                kind: work.kind,
                owner: work.owner,
                user: work.user,
                display: display.map(ToOwned::to_owned),
                started_work: false,
                registered_at: Instant::now(),
                retained_since: None,
            },
        );
        drop(registry);
        Ok(WorkSlot {
            registry: Arc::clone(&self.registry),
            id,
            kind: work.kind,
            started: Arc::new(AtomicBool::new(false)),
            cancelled,
            executor: Some(executor),
            publication,
            permit: Some(permit),
            timing: work.timing,
            physical: PhysicalOutcome::NotStarted,
            holds: work.holds,
        })
    }

    fn mark_request_finished(&self, id: u64, started: &AtomicBool) -> RequestEnd {
        let (entry, started) = {
            let mut registry = self.registry.lock();
            let entry = match registry.retained.get_mut(&id) {
                Some(entry) if entry.retained_since.is_none() => {
                    entry.retained_since = Some(Instant::now());
                    Some((
                        entry.kind,
                        entry.started_work,
                        entry.registered_at.elapsed(),
                    ))
                }
                _ => None,
            };
            (entry, started.load(Ordering::SeqCst))
        };
        let retained = entry.is_some();
        if let Some((kind, started_work, held_for)) = entry {
            tracing::info!(
                event_type = "query_permit_retained",
                query_id = id,
                kind = kind.as_str(),
                started = started_work,
                held_ms = u64::try_from(held_for.as_millis()).unwrap_or(u64::MAX),
                "query permit retained past its request"
            );
        }
        RequestEnd { retained, started }
    }

    pub(crate) fn owner_of(&self, id: u64) -> Option<WorkOwner> {
        self.registry
            .lock()
            .retained
            .get(&id)
            .map(|entry| entry.owner)
    }

    #[must_use]
    pub fn retained(&self) -> usize {
        self.registry.lock().retained_count()
    }

    #[must_use]
    pub fn retained_work(&self) -> Vec<RetainedWork> {
        let now = Instant::now();
        let registry = self.registry.lock();
        registry
            .retained
            .values()
            .filter_map(|entry| {
                let since = entry.retained_since?;
                Some(RetainedWork {
                    id: entry.id,
                    kind: entry.kind,
                    started: entry.started_work,
                    retained: now.saturating_duration_since(since),
                    owner: entry.owner,
                    user: entry.user.clone(),
                    display: entry.display.clone(),
                })
            })
            .collect()
    }

    /// The single id authority: pass the returned id to the work and, for
    /// user-visible work, to `QueryTracker::start`, so `/queries` listings
    /// and [`cancel_by_id`](Self::cancel_by_id) speak the same id space.
    pub fn allocate_query_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn pend(&self, id: u64, kind: WorkKind) -> Pending {
        let cancelled = Arc::new(AtomicBool::new(false));
        let (wake, receiver) = tokio::sync::watch::channel(false);
        {
            let mut registry = self.registry.lock();
            debug_assert!(
                !registry.pending.contains_key(&id) && !registry.interrupts.contains_key(&id),
                "query id {id} is already in the registry"
            );
            registry.pending.insert(
                id,
                PendingEntry {
                    cancelled: Arc::clone(&cancelled),
                    wake,
                },
            );
        }
        Pending {
            registry: Arc::clone(&self.registry),
            id,
            kind,
            cancelled,
            wake: Wake(Some(receiver)),
            refusal: RefusalOnce::new(self.instance),
        }
    }

    fn wake_of(&self, id: u64) -> Wake {
        Wake(
            self.registry
                .lock()
                .pending
                .get(&id)
                .map(|entry| entry.wake.subscribe()),
        )
    }

    /// `query_id` must come from [`allocate_query_id`](Self::allocate_query_id).
    /// Timestamps in the result are UTC.
    pub async fn execute(
        &self,
        query_id: u64,
        dsl: &str,
        deadline: Deadline,
        capture_debug: bool,
        work: WorkContext,
    ) -> ExecuteOutcome {
        self.execute_capped(
            query_id,
            dsl,
            RowCap::Refuse(self.max_result_rows),
            deadline,
            capture_debug,
            work,
        )
        .await
    }

    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    pub async fn execute_capped(
        &self,
        query_id: u64,
        dsl: &str,
        cap: RowCap,
        deadline: Deadline,
        capture_debug: bool,
        work: WorkContext,
    ) -> ExecuteOutcome {
        let kind = work.kind;
        let histogram = work.histogram;
        let timing = work.timing.clone();
        let clock = timing
            .as_ref()
            .map_or_else(PhaseClock::off, |timing| timing.clock().clone());
        let refusal_log = RefusalOnce::new(self.instance);
        let wake = self.wake_of(query_id);
        #[cfg(any(test, feature = "test-support"))]
        let seams = Arc::clone(&self.seams);
        worker_seam!(seams, Queued);
        let available = self.semaphore.available_permits();
        if available == 0 {
            tracing::warn!(
                event_type = "pool_pressure",
                max_concurrent = self.max_concurrent,
                "executor pool at capacity, query queued"
            );
        }

        let semaphore = Arc::clone(&self.semaphore);
        let acquired = {
            let _wait = clock.guard(QueryPhase::PoolWait);
            wake.run(deadline, semaphore.acquire_owned()).await
        };
        let permit = match acquired {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => {
                return ExecuteOutcome {
                    result: Err(ServerError::Internal("executor pool shut down".into())),
                    debug: None,
                    severity_columns: Vec::new(),
                    histogram: None,
                };
            }
            Err(refusal) => {
                let refused = refusal_log.refuse_outcome(query_id, kind, refusal);
                account_refused(timing.as_ref());
                return refused;
            }
        };

        let published = {
            let _wait = clock.guard(QueryPhase::PublicationWait);
            wake.run(deadline, self.publication.read()).await
        };
        let publication = match published {
            Ok(Ok(guard)) => guard,
            Ok(Err(error)) => {
                return ExecuteOutcome {
                    result: Err(error),
                    debug: None,
                    severity_columns: Vec::new(),
                    histogram: None,
                };
            }
            Err(refusal) => {
                let refused = refusal_log.refuse_outcome(query_id, kind, refusal);
                account_refused(timing.as_ref());
                return refused;
            }
        };

        let mut slot = match self.begin_slot(query_id, work, Some(dsl), permit, Some(publication)) {
            Ok(slot) => slot,
            Err(error) => {
                return ExecuteOutcome {
                    result: Err(error),
                    debug: None,
                    severity_columns: Vec::new(),
                    histogram: None,
                };
            }
        };
        let started = Arc::clone(&slot.started);

        let dsl = dsl.to_owned();
        let base_dir = Arc::clone(&self.base_dir);
        let hot_buffer = self.hot_buffer.clone();
        let scratch = self.scratch.clone();
        let field_catalog = Arc::clone(&self.field_catalog);

        // Dispatch to the work-start transition; the worker ends it.
        clock.enter(QueryPhase::Startup);
        let mut task = tokio::task::spawn_blocking({
            let refusal_log = refusal_log.clone();
            #[cfg(any(test, feature = "test-support"))]
            let seams = Arc::clone(&seams);
            move || {
                // Wider than `run_query_blocking`'s own catch: source
                // discovery, the catalog snapshot and the presentation walk
                // are all physical work on this thread, and a panic in any of
                // them must still reach the slot's cleanup below.
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    worker_seam!(seams, Entry);
                    // Interrupt registration first, so a cancellation arriving
                    // during the bind has something to reach. A refusal here
                    // means the caller already asked for this work to stop.
                    if !slot.publish_interrupt(slot.executor().interrupt_handle()) {
                        return refusal_log.refuse_outcome(query_id, kind, StartRefusal::Cancelled);
                    }
                    if let Err(refusal) = slot.begin_work(deadline) {
                        return refusal_log.refuse_outcome(query_id, kind, refusal);
                    }
                    slot.started_timing();
                    worker_seam!(seams, Started);

                    #[cfg(any(test, feature = "test-support"))]
                    {
                        let delay = TEST_QUERY_DELAY_MS.load(Ordering::Relaxed);
                        if delay > 0 {
                            std::thread::sleep(std::time::Duration::from_millis(delay));
                        }
                    }

                    let clock = slot.clock();
                    let cancel = slot.cancel_latch();
                    let source = clock.time(QueryPhase::Source, || {
                        compute_source(&base_dir, &dsl, &cancel)
                    });
                    let (result, debug, severity_columns, histogram) = match source {
                        Ok(source) => {
                            let file_globs: usize = if source.starts_with('[') {
                                source.matches(',').count() + 1
                            } else {
                                1
                            };
                            tracing::debug!(
                                event_type = "query_source",
                                file_globs,
                                source = %source,
                                "computed query source"
                            );

                            // One catalog snapshot per query: every retry inside
                            // the executor sees the same comparison pins, and the
                            // presentation metadata computed beside the rows is
                            // rooted in that same snapshot.
                            let pins = clock.time(QueryPhase::Source, || field_catalog.all());
                            let (result, debug) = run_query_blocking(
                                slot.executor(),
                                &cancel,
                                clock,
                                &dsl,
                                &source,
                                ReadInput::Corpus(hot_buffer.as_ref().map(|hb| (hb, &scratch))),
                                &pins,
                                cap,
                                capture_debug,
                                || {},
                            );
                            worker_seam!(seams, Presenting);
                            let (result, severity_columns, histogram) =
                                present(result, &pins, &dsl, histogram, &cancel, clock);
                            (result, debug, severity_columns, histogram)
                        }
                        Err(error) => (Err(error.into()), None, Vec::new(), None),
                    };
                    worker_seam!(seams, Finished);
                    ExecuteOutcome {
                        result,
                        debug,
                        severity_columns,
                        histogram,
                    }
                }));
                let outcome = outcome.unwrap_or_else(|_payload| ExecuteOutcome {
                    result: Err(ServerError::Panicked("query worker")),
                    debug: None,
                    severity_columns: Vec::new(),
                    histogram: None,
                });
                slot.finish(&outcome.result);
                drop(slot);
                worker_seam!(seams, Released);
                outcome
            }
        });

        let mut request = RequestGuard::new(self, query_id, kind, &started, timing.as_ref());
        tokio::select! {
            join_result = &mut task => {
                request.disarm();
                match join_result {
                    Ok(outcome) => {
                        if refusal_log.refused() {
                            account_refused(timing.as_ref());
                        }
                        outcome
                    }
                    Err(e) => ExecuteOutcome {
                        result: Err(ServerError::from_join("query", e)),
                        debug: None,
                        severity_columns: Vec::new(),
                        histogram: None,
                    },
                }
            }
            () = tokio::time::sleep_until(deadline.instant()) => {
                let result = if request.abandon() {
                    Err(ServerError::Timeout)
                } else {
                    Err(refusal_log.refuse(query_id, kind, StartRefusal::Expired))
                };
                ExecuteOutcome { result, debug: None, severity_columns: Vec::new(), histogram: None }
            }
        }
    }

    /// `query_id` must come from [`allocate_query_id`](Self::allocate_query_id).
    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    pub async fn execute_with_source(
        &self,
        query_id: u64,
        dsl: &str,
        source: &str,
        deadline: Deadline,
        capture_debug: bool,
        work: WorkContext,
    ) -> ExecuteOutcome {
        let kind = work.kind;
        let timing = work.timing.clone();
        let clock = timing
            .as_ref()
            .map_or_else(PhaseClock::off, |timing| timing.clock().clone());
        let refusal_log = RefusalOnce::new(self.instance);
        let wake = self.wake_of(query_id);
        #[cfg(any(test, feature = "test-support"))]
        let seams = Arc::clone(&self.seams);
        worker_seam!(seams, Queued);
        let available = self.semaphore.available_permits();
        if available == 0 {
            tracing::warn!(
                event_type = "pool_pressure",
                max_concurrent = self.max_concurrent,
                "executor pool at capacity, query queued"
            );
        }

        let semaphore = Arc::clone(&self.semaphore);
        let acquired = {
            let _wait = clock.guard(QueryPhase::PoolWait);
            wake.run(deadline, semaphore.acquire_owned()).await
        };
        let permit = match acquired {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => {
                return ExecuteOutcome {
                    result: Err(ServerError::Internal("executor pool shut down".into())),
                    debug: None,
                    severity_columns: Vec::new(),
                    histogram: None,
                };
            }
            Err(refusal) => {
                let refused = refusal_log.refuse_outcome(query_id, kind, refusal);
                account_refused(timing.as_ref());
                return refused;
            }
        };

        let mut slot = match self.begin_slot(query_id, work, Some(dsl), permit, None) {
            Ok(slot) => slot,
            Err(error) => {
                return ExecuteOutcome {
                    result: Err(error),
                    debug: None,
                    severity_columns: Vec::new(),
                    histogram: None,
                };
            }
        };
        let started = Arc::clone(&slot.started);

        let dsl = dsl.to_owned();
        let source = source.to_owned();
        let cap = RowCap::Refuse(self.max_result_rows);
        let field_catalog = Arc::clone(&self.field_catalog);

        // Dispatch to the work-start transition; the worker ends it.
        clock.enter(QueryPhase::Startup);
        let mut task = tokio::task::spawn_blocking({
            let refusal_log = refusal_log.clone();
            #[cfg(any(test, feature = "test-support"))]
            let seams = Arc::clone(&seams);
            move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    worker_seam!(seams, Entry);
                    if !slot.publish_interrupt(slot.executor().interrupt_handle()) {
                        return refusal_log.refuse_outcome(query_id, kind, StartRefusal::Cancelled);
                    }
                    if let Err(refusal) = slot.begin_work(deadline) {
                        return refusal_log.refuse_outcome(query_id, kind, refusal);
                    }
                    slot.started_timing();
                    worker_seam!(seams, Started);

                    #[cfg(any(test, feature = "test-support"))]
                    {
                        let delay = TEST_QUERY_DELAY_MS.load(Ordering::Relaxed);
                        if delay > 0 {
                            std::thread::sleep(std::time::Duration::from_millis(delay));
                        }
                    }

                    tracing::debug!(
                        event_type = "query_source",
                        source = %source,
                        "using pre-computed source (from saved)"
                    );

                    let clock = slot.clock();
                    let cancel = slot.cancel_latch();
                    let pins = clock.time(QueryPhase::Source, || field_catalog.all());
                    let (result, debug) = run_query_blocking(
                        slot.executor(),
                        &cancel,
                        clock,
                        &dsl,
                        &source,
                        ReadInput::Recorded,
                        &pins,
                        cap,
                        capture_debug,
                        || {},
                    );
                    // `dsl` here is what follows `| from saved`, whose `PinScope`
                    // rule clears the scope, so the presentation walk roots in an
                    // empty catalog. A stored run's rows are not events: the
                    // handler answers `stored_result` and asks for no count.
                    let (result, severity_columns, _) = present(
                        result,
                        &trawl_core::schema::FieldTypes::new(),
                        &dsl,
                        None,
                        &cancel,
                        clock,
                    );
                    worker_seam!(seams, Finished);
                    ExecuteOutcome {
                        result,
                        debug,
                        severity_columns,
                        histogram: None,
                    }
                }));
                let outcome = outcome.unwrap_or_else(|_payload| ExecuteOutcome {
                    result: Err(ServerError::Panicked("query worker")),
                    debug: None,
                    severity_columns: Vec::new(),
                    histogram: None,
                });
                slot.finish(&outcome.result);
                drop(slot);
                worker_seam!(seams, Released);
                outcome
            }
        });

        let mut request = RequestGuard::new(self, query_id, kind, &started, timing.as_ref());
        tokio::select! {
            join_result = &mut task => {
                request.disarm();
                match join_result {
                    Ok(outcome) => {
                        if refusal_log.refused() {
                            account_refused(timing.as_ref());
                        }
                        outcome
                    }
                    Err(e) => ExecuteOutcome {
                        result: Err(ServerError::from_join("query", e)),
                        debug: None,
                        severity_columns: Vec::new(),
                        histogram: None,
                    },
                }
            }
            () = tokio::time::sleep_until(deadline.instant()) => {
                let result = if request.abandon() {
                    Err(ServerError::Timeout)
                } else {
                    Err(refusal_log.refuse(query_id, kind, StartRefusal::Expired))
                };
                ExecuteOutcome { result, debug: None, severity_columns: Vec::new(), histogram: None }
            }
        }
    }

    /// Acquire every permit: the repin cutover's exclusion primitive.
    ///
    /// Every lane that can read parquet funnels through this semaphore
    /// (queries, `from saved`, exports, captures, value sampling), and
    /// each lane computes its source and snapshots its comparison pins
    /// inside the permit-holding task, so holding all permits means no
    /// query can straddle the per-env swap or the pin flip. SSE streams hold no
    /// permit, read no parquet, and keep their compile-time snapshot until
    /// reconnect, so a repin reaches a live stream at its next connect.
    ///
    /// Bounded: a wedged query holds a permit forever, and an unbounded
    /// wait here would starve the cutover with the rollup suppressed and
    /// retention disabled. Past `timeout` this returns
    /// [`ServerError::Timeout`] and the caller aborts to its `blocked`
    /// outcome.
    pub async fn exclusive(&self, timeout: Duration) -> Result<PoolExclusive, ServerError> {
        let permits = u32::try_from(self.max_concurrent)
            .map_err(|_| ServerError::Internal("pool size exceeds u32".into()))?;
        let semaphore = Arc::clone(&self.semaphore);
        match tokio::time::timeout(timeout, semaphore.acquire_many_owned(permits)).await {
            Ok(Ok(permits)) => Ok(PoolExclusive { _permits: permits }),
            Ok(Err(_)) => Err(ServerError::Internal("executor pool shut down".into())),
            Err(_) => {
                let retained = self.retained();
                tracing::warn!(
                    event_type = "pool_exclusive_blocked",
                    timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                    capacity = self.max_concurrent,
                    held = self.max_concurrent - self.semaphore.available_permits(),
                    retained,
                    "could not acquire the whole pool within its budget"
                );
                Err(ServerError::Timeout)
            }
        }
    }

    pub fn cancel_all(&self) {
        let count = {
            let mut registry = self.registry.lock();
            let ids: Vec<u64> = registry.interrupts.keys().copied().collect();
            for id in &ids {
                registry.request_cancel(*id);
            }
            ids.len()
        };
        if count > 0 {
            tracing::info!(
                event_type = "lifecycle",
                count,
                "interrupted active queries for shutdown"
            );
        }
    }

    pub fn cancel_by_id(&self, query_id: u64) -> bool {
        self.registry.lock().request_cancel(query_id)
    }

    pub fn max_result_rows(&self) -> usize {
        self.max_result_rows
    }

    pub fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
    }

    pub fn capacity(&self) -> usize {
        self.max_concurrent
    }

    #[must_use]
    pub fn preparation(&self) -> &crate::prepare::Preparation {
        &self.preparation
    }

    #[must_use]
    pub fn try_lease_scheduled(&self) -> Option<RunLease> {
        Arc::clone(&self.scheduled)
            .try_acquire_owned()
            .ok()
            .map(|permit| RunLease {
                charge: Arc::new(permit),
                #[cfg(any(test, feature = "test-support"))]
                seams: Arc::clone(&self.seams),
            })
    }

    #[must_use]
    pub fn scheduled_leases_available(&self) -> usize {
        self.scheduled.available_permits()
    }

    #[must_use]
    pub fn scheduled_leases_in_use(&self) -> usize {
        self.scheduled_lease_limit()
            .saturating_sub(self.scheduled.available_permits())
    }

    #[must_use]
    pub fn scheduled_lease_limit(&self) -> usize {
        scheduled_cap(self.max_concurrent)
    }

    /// `estimate` is how long the run may take, from which a refusal derives
    /// its advisory `Retry-After`.
    pub fn try_lease_manual(
        &self,
        key_id: i64,
        estimate: Duration,
    ) -> Result<RunLease, ManualBusy> {
        self.try_lease_manual_at(key_id, estimate, Instant::now())
    }

    fn try_lease_manual_at(
        &self,
        key_id: i64,
        estimate: Duration,
        now: Instant,
    ) -> Result<RunLease, ManualBusy> {
        let slot = self.manual.try_take(key_id, estimate, now)?;
        Ok(RunLease {
            charge: Arc::new(slot),
            #[cfg(any(test, feature = "test-support"))]
            seams: Arc::clone(&self.seams),
        })
    }

    #[must_use]
    pub fn manual_leases_in_use(&self) -> usize {
        self.manual.holders.lock().len()
    }

    #[must_use]
    pub fn manual_lease_limit(&self) -> usize {
        self.manual.limit
    }

    pub async fn prepare<T, F>(
        &self,
        deadline: Deadline,
        door: crate::prepare::Door,
        work: F,
    ) -> Result<T, ServerError>
    where
        F: FnOnce(&trawl_core::cancel::Stop) -> T + Send + 'static,
        T: Send + 'static,
    {
        self.preparation.run(deadline, door, work).await
    }

    pub fn base_dir(&self) -> &str {
        &self.base_dir
    }

    /// `service`, when present, scopes the glob to one service's files; the
    /// caller is responsible for validating the name.
    pub async fn sample_field_values(
        &self,
        field: &str,
        service: Option<&str>,
        limit: usize,
        deadline: Deadline,
        work: WorkContext,
    ) -> Result<Vec<String>, ServerError> {
        let kind = work.kind;
        let refusal_log = RefusalOnce::new(self.instance);
        #[cfg(any(test, feature = "test-support"))]
        let seams = Arc::clone(&self.seams);
        let semaphore = Arc::clone(&self.semaphore);
        let id = self.allocate_query_id();

        let acquire = Deadline::after(SAMPLE_ACQUIRE_BUDGET.min(deadline.remaining()));
        let permit = acquire
            .run(semaphore.acquire_owned())
            .await
            .map_err(|_| refusal_log.refuse(id, kind, StartRefusal::Expired))?
            .map_err(|_| ServerError::Internal("executor pool shut down".into()))?;

        let publication = acquire
            .run(self.publication.read())
            .await
            .map_err(|_| refusal_log.refuse(id, kind, StartRefusal::Expired))??;

        let mut slot = self.begin_slot(id, work, None, permit, Some(publication))?;
        let started = Arc::clone(&slot.started);
        let fallback_glob = Arc::clone(&self.fallback_glob);
        let base_dir = Arc::clone(&self.base_dir);
        let field = field.to_owned();
        let service = service.map(ToOwned::to_owned);

        let mut task = tokio::task::spawn_blocking({
            let refusal_log = refusal_log.clone();
            #[cfg(any(test, feature = "test-support"))]
            let seams = Arc::clone(&seams);
            move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    worker_seam!(seams, Entry);
                    if !slot.publish_interrupt(slot.executor().interrupt_handle()) {
                        return Err(refusal_log.refuse(id, kind, StartRefusal::Cancelled));
                    }
                    if let Err(refusal) = slot.begin_work(deadline) {
                        return Err(refusal_log.refuse(id, kind, refusal));
                    }
                    worker_seam!(seams, Started);
                    let glob = match service {
                        Some(svc) => service_glob(&base_dir, &svc),
                        None => fallback_glob.to_string(),
                    };
                    let result = slot
                        .executor()
                        .sample_field_values(&glob, &field, limit)
                        .map_err(ServerError::from);
                    worker_seam!(seams, Finished);
                    result
                }));
                let result = outcome
                    .unwrap_or_else(|_payload| Err(ServerError::Panicked("field values sample")));
                slot.finish(&result);
                result
            }
        });

        let mut request = RequestGuard::new(self, id, kind, &started, None);
        tokio::select! {
            joined = &mut task => {
                request.disarm();
                joined.map_err(|e| ServerError::from_join("field values sample", e))?
            }
            () = tokio::time::sleep_until(deadline.instant()) => {
                if request.abandon() {
                    Err(ServerError::Timeout)
                } else {
                    Err(refusal_log.refuse(id, kind, StartRefusal::Expired))
                }
            }
        }
    }

    /// `query_id` must come from [`allocate_query_id`](Self::allocate_query_id).
    #[allow(clippy::too_many_lines)]
    pub async fn export_parquet(
        &self,
        query_id: u64,
        dsl: &str,
        max_rows: usize,
        deadline: Deadline,
        work: WorkContext,
    ) -> Result<(Vec<u8>, usize), ServerError> {
        let kind = work.kind;
        let timing = work.timing.clone();
        let clock = timing
            .as_ref()
            .map_or_else(PhaseClock::off, |timing| timing.clock().clone());
        let refusal_log = RefusalOnce::new(self.instance);
        let wake = self.wake_of(query_id);
        #[cfg(any(test, feature = "test-support"))]
        let seams = Arc::clone(&self.seams);
        worker_seam!(seams, Queued);
        let semaphore = Arc::clone(&self.semaphore);
        let acquired = {
            let _wait = clock.guard(QueryPhase::PoolWait);
            wake.run(deadline, semaphore.acquire_owned()).await
        };
        let permit = acquired
            .map_err(|refusal| {
                let refused = refusal_log.refuse(query_id, kind, refusal);
                account_refused(timing.as_ref());
                refused
            })?
            .map_err(|_| ServerError::Internal("executor pool shut down".into()))?;

        let published = {
            let _wait = clock.guard(QueryPhase::PublicationWait);
            wake.run(deadline, self.publication.read()).await
        };
        let publication = published.map_err(|refusal| {
            let refused = refusal_log.refuse(query_id, kind, refusal);
            account_refused(timing.as_ref());
            refused
        })??;

        let mut slot = self.begin_slot(query_id, work, Some(dsl), permit, Some(publication))?;
        let started = Arc::clone(&slot.started);
        let dsl = dsl.to_owned();
        let base_dir = Arc::clone(&self.base_dir);
        let hot_buffer = self.hot_buffer.clone();
        let scratch = self.scratch.clone();
        let field_catalog = Arc::clone(&self.field_catalog);

        // Dispatch to the work-start transition; the worker ends it.
        clock.enter(QueryPhase::Startup);
        let mut task = tokio::task::spawn_blocking({
            let refusal_log = refusal_log.clone();
            #[cfg(any(test, feature = "test-support"))]
            let seams = Arc::clone(&seams);
            move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    worker_seam!(seams, Entry);
                    if !slot.publish_interrupt(slot.executor().interrupt_handle()) {
                        return Err(refusal_log.refuse(query_id, kind, StartRefusal::Cancelled));
                    }
                    if let Err(refusal) = slot.begin_work(deadline) {
                        return Err(refusal_log.refuse(query_id, kind, refusal));
                    }
                    slot.started_timing();
                    worker_seam!(seams, Started);

                    #[cfg(any(test, feature = "test-support"))]
                    {
                        let delay = TEST_QUERY_DELAY_MS.load(Ordering::Relaxed);
                        if delay > 0 {
                            std::thread::sleep(std::time::Duration::from_millis(delay));
                        }
                    }

                    let clock = slot.clock();
                    let cancel = slot.cancel_latch();
                    let source = clock.time(QueryPhase::Source, || {
                        compute_source(&base_dir, &dsl, &cancel)
                    })?;

                    // The error keeps only its kind: `tempfile` appends the
                    // path it tried, and an internal error's text is logged.
                    let tmp = scratch.export_file().map_err(|e| {
                        ServerError::Internal(format!(
                            "failed to create the export staging file: {}",
                            e.kind()
                        ))
                    })?;
                    let tmp_path = tmp.path().to_owned();

                    let hot_snapshot = hot_source(hot_buffer.as_ref().map_or(Ok(None), |hb| {
                        clock.time(QueryPhase::HotSnapshot, || hb.snapshot(&scratch))
                    }))?;

                    let pins = clock.time(QueryPhase::Source, || field_catalog.all());
                    let executor = slot.executor().cancellable(&cancel).timed(clock);
                    let written = if let Some(ref hot) = hot_snapshot {
                        // `hot_source` refused a path that is not UTF-8.
                        let hot_path = hot.path().to_str().unwrap_or_default();
                        executor.export_parquet_with_hot(
                            &dsl,
                            &source,
                            hot_path,
                            &hot.columns,
                            &hot.field_types,
                            &pins,
                            &tmp_path,
                            max_rows,
                        )
                    } else {
                        executor.export_parquet(&dsl, &source, &pins, &tmp_path, max_rows)
                    }
                    .map_err(ServerError::from);

                    // Reading the written file back is this format's render.
                    let exported = written.and_then(|rows| {
                        clock
                            .time(QueryPhase::Render, || std::fs::read(&tmp_path))
                            .map(|bytes| (bytes, rows))
                            .map_err(|e| {
                                ServerError::Internal(format!(
                                    "failed to read parquet temp file: {e}"
                                ))
                            })
                    });
                    worker_seam!(seams, Finished);
                    exported
                }));
                let result =
                    outcome.unwrap_or_else(|_payload| Err(ServerError::Panicked("export")));
                slot.finish(&result);
                drop(slot);
                worker_seam!(seams, Released);
                result
            }
        });

        let mut request = RequestGuard::new(self, query_id, kind, &started, timing.as_ref());
        tokio::select! {
            join_result = &mut task => {
                request.disarm();
                if refusal_log.refused() {
                    account_refused(timing.as_ref());
                }
                match join_result {
                    Ok(result) => result,
                    Err(e) => Err(ServerError::from_join("export", e)),
                }
            }
            () = tokio::time::sleep_until(deadline.instant()) => {
                if request.abandon() {
                    Err(ServerError::Timeout)
                } else {
                    Err(refusal_log.refuse(query_id, kind, StartRefusal::Expired))
                }
            }
        }
    }

    /// `query_id` must come from [`allocate_query_id`](Self::allocate_query_id).
    /// `finish` runs only on a query that succeeded.
    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    pub async fn capture<T: Send + 'static>(
        &self,
        query_id: u64,
        dsl: &str,
        source: CaptureSource,
        cap: RowCap,
        deadline: Deadline,
        work: WorkContext,
        finish: CaptureFinish<T>,
    ) -> Result<T, ServerError> {
        let kind = work.kind;
        let timing = work.timing.clone();
        let clock = timing
            .as_ref()
            .map_or_else(PhaseClock::off, |timing| timing.clock().clone());
        let refusal_log = RefusalOnce::new(self.instance);
        let wake = self.wake_of(query_id);
        #[cfg(any(test, feature = "test-support"))]
        let seams = Arc::clone(&self.seams);
        worker_seam!(seams, Queued);
        let semaphore = Arc::clone(&self.semaphore);
        let acquired = {
            let _wait = clock.guard(QueryPhase::PoolWait);
            wake.run(deadline, semaphore.acquire_owned()).await
        };
        let permit = acquired
            .map_err(|refusal| {
                let refused = refusal_log.refuse(query_id, kind, refusal);
                account_refused(timing.as_ref());
                refused
            })?
            .map_err(|_| ServerError::Internal("executor pool shut down".into()))?;

        let publication = match &source {
            CaptureSource::Window { .. } => {
                let published = {
                    let _wait = clock.guard(QueryPhase::PublicationWait);
                    wake.run(deadline, self.publication.read()).await
                };
                Some(published.map_err(|refusal| {
                    let refused = refusal_log.refuse(query_id, kind, refusal);
                    account_refused(timing.as_ref());
                    refused
                })??)
            }
            CaptureSource::Resolved { .. } => None,
        };

        let mut slot = self.begin_slot(query_id, work, Some(dsl), permit, publication)?;
        let started = Arc::clone(&slot.started);
        let dsl = dsl.to_owned();
        let base_dir = Arc::clone(&self.base_dir);
        let hot_buffer = self.hot_buffer.clone();
        let scratch = self.scratch.clone();
        let field_catalog = Arc::clone(&self.field_catalog);
        let instance = self.instance;

        // Dispatch to the work-start transition; the worker ends it.
        clock.enter(QueryPhase::Startup);
        let mut task = tokio::task::spawn_blocking({
            let refusal_log = refusal_log.clone();
            #[cfg(any(test, feature = "test-support"))]
            let seams = Arc::clone(&seams);
            move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    worker_seam!(seams, Entry);
                    if !slot.publish_interrupt(slot.executor().interrupt_handle()) {
                        return Err(refusal_log.refuse(query_id, kind, StartRefusal::Cancelled));
                    }
                    if let Err(refusal) = slot.begin_work(deadline) {
                        return Err(refusal_log.refuse(query_id, kind, refusal));
                    }
                    slot.started_timing();

                    let clock = slot.clock();
                    let (source, source_bytes, input) = match source {
                        CaptureSource::Window {
                            start,
                            end,
                            max_bytes,
                        } => {
                            let selected = clock.time(QueryPhase::Source, || {
                                let cancel = slot.cancel_latch();
                                let selection = select(&base_dir, &dsl, &cancel)?;
                                selection_bytes(&selection, start, end, &cancel)
                                    .map(|bytes| (selection, bytes))
                            });
                            let (selection, selected_bytes) = selected?;
                            if selected_bytes > max_bytes {
                                // Counts only: no path, no partition, no DSL.
                                tracing::warn!(
                                    event_type = "capture_refused",
                                    pool = instance,
                                    query_id,
                                    kind = kind.as_str(),
                                    reason = "source_too_large",
                                    selected_bytes,
                                    max_bytes,
                                    "capture refused before the engine ran: its window selects \
                                     more stored data than the capture source budget"
                                );
                                return Err(ServerError::CaptureSourceTooLarge {
                                    selected_bytes,
                                    max_bytes,
                                    kind: CaptureSourceKind::Window,
                                });
                            }
                            (
                                selection.source(),
                                Some(selected_bytes),
                                ReadInput::Corpus(hot_buffer.as_ref().map(|hb| (hb, &scratch))),
                            )
                        }
                        CaptureSource::Resolved { source } => (source, None, ReadInput::Recorded),
                    };
                    worker_seam!(seams, Started);
                    tracing::debug!(
                        event_type = "query_source",
                        source = %source,
                        "computed capture source"
                    );

                    let pins = clock.time(QueryPhase::Source, || field_catalog.all());
                    let cancel = slot.cancel_latch();
                    let (result, _debug) = run_query_blocking(
                        slot.executor(),
                        &cancel,
                        clock,
                        &dsl,
                        &source,
                        input,
                        &pins,
                        cap,
                        false,
                        || {
                            worker_seam!(seams, Snapshotted);
                        },
                    );
                    worker_seam!(seams, Returned);
                    let result = result?;
                    let answer = finish(CaptureInput {
                        result,
                        executor: slot.executor(),
                        clock,
                        cancel: &cancel,
                        source_bytes,
                        #[cfg(any(test, feature = "test-support"))]
                        seams: &seams,
                    });
                    worker_seam!(seams, Finished);
                    answer
                }));
                let result =
                    outcome.unwrap_or_else(|_payload| Err(ServerError::Panicked("capture worker")));
                slot.finish(&result);
                drop(slot);
                worker_seam!(seams, Released);
                result
            }
        });

        let mut request = RequestGuard::new(self, query_id, kind, &started, timing.as_ref());
        tokio::select! {
            join_result = &mut task => {
                request.disarm();
                if refusal_log.refused() {
                    account_refused(timing.as_ref());
                }
                match join_result {
                    Ok(result) => result,
                    Err(e) => Err(ServerError::from_join("capture", e)),
                }
            }
            () = tokio::time::sleep_until(deadline.instant()) => {
                if request.abandon() {
                    Err(ServerError::Timeout)
                } else {
                    Err(refusal_log.refuse(query_id, kind, StartRefusal::Expired))
                }
            }
        }
    }
}

fn service_glob(base_dir: &str, service: &str) -> String {
    format!("{}/**/{service}.parquet", base_dir.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use seam::Seam;

    const TEST_WORK: WorkContext = WorkContext {
        kind: WorkKind::Query,
        owner: WorkOwner::Key(7),
        user: None,
        timing: None,
        holds: None,
        histogram: None,
    };
    const TEST_SAMPLE_WORK: WorkContext = WorkContext {
        kind: WorkKind::Sample,
        owner: WorkOwner::System,
        user: None,
        timing: None,
        holds: None,
        histogram: None,
    };

    fn idle_len(pool: &ExecutorPool) -> usize {
        pool.registry.lock().idle.len()
    }

    #[test]
    fn scheduled_leases_are_half_the_permits_and_shared() {
        for (permits, cap) in [(1, 1), (2, 1), (3, 1), (4, 2), (5, 2), (8, 4)] {
            assert_eq!(scheduled_cap(permits), cap, "{permits} permits");
        }

        let pool = ExecutorPool::for_test("/nonexistent".into(), 4, 100, None).unwrap();
        let clone = pool.clone();
        assert_eq!(pool.scheduled_leases_available(), 2);
        let first = pool.try_lease_scheduled().expect("a free lease");
        let second = clone
            .try_lease_scheduled()
            .expect("a clone of the pool takes from the same leases");
        assert!(pool.try_lease_scheduled().is_none(), "the cap is full");
        assert!(clone.try_lease_scheduled().is_none());

        drop(second);
        assert_eq!(pool.scheduled_leases_available(), 1);
        let kept = first.clone();
        let hold = first.into_hold();
        drop(kept);
        assert_eq!(
            clone.scheduled_leases_available(),
            1,
            "the hold keeps the lease"
        );
        drop(hold);
        assert_eq!(clone.scheduled_leases_available(), 2);
    }

    #[test]
    fn run_gate_limits_follow_the_pool() {
        for (permits, manual, scheduled) in [(1, 1, 1), (2, 1, 1), (3, 1, 1), (4, 1, 2), (8, 2, 4)]
        {
            assert_eq!(manual_cap(permits), manual, "{permits} permits");
            assert_eq!(scheduled_cap(permits), scheduled, "{permits} permits");

            let pool = ExecutorPool::for_test("/nonexistent".into(), permits, 100, None).unwrap();
            assert_eq!(pool.manual_lease_limit(), manual, "{permits} permits");
            assert_eq!(pool.scheduled_lease_limit(), scheduled, "{permits} permits");

            let manual_leases: Vec<RunLease> = (0..manual)
                .map(|key| {
                    pool.try_lease_manual(i64::try_from(key).unwrap(), Duration::from_secs(60))
                        .expect("a free manual lease")
                })
                .collect();
            assert!(
                pool.try_lease_manual(-1, Duration::from_secs(60)).is_err(),
                "{permits} permits: the manual gate is full at {manual}"
            );
            assert_eq!(pool.manual_leases_in_use(), manual);

            let scheduled_leases: Vec<RunLease> = (0..scheduled)
                .map(|_| pool.try_lease_scheduled().expect("a free scheduled lease"))
                .collect();
            assert!(
                pool.try_lease_scheduled().is_none(),
                "{permits} permits: the scheduled cap is full at {scheduled}"
            );
            assert_eq!(pool.scheduled_leases_in_use(), scheduled);

            drop(manual_leases);
            drop(scheduled_leases);
            assert_eq!(pool.manual_leases_in_use(), 0);
            assert_eq!(pool.scheduled_leases_in_use(), 0);
        }
    }

    #[test]
    fn manual_gate_acquires_atomically() {
        let pool = ExecutorPool::for_test("/nonexistent".into(), 8, 100, None).unwrap();
        assert_eq!(pool.manual_lease_limit(), 2);
        manual_gate_under_contention(&pool);
        manual_gate_one_at_a_time(&pool);
    }

    fn manual_gate_under_contention(pool: &ExecutorPool) {
        use std::sync::Barrier;
        use std::sync::atomic::AtomicUsize;

        const THREADS: usize = 16;
        const ROUNDS: usize = 300;
        const KEYS: usize = 6;

        let limit = pool.manual_lease_limit();
        let live = Arc::new(AtomicUsize::new(0));
        let per_key: Arc<Vec<AtomicUsize>> =
            Arc::new((0..KEYS).map(|_| AtomicUsize::new(0)).collect());
        let admitted = Arc::new(AtomicUsize::new(0));
        let refused = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(Barrier::new(THREADS));
        let workers: Vec<_> = (0..THREADS)
            .map(|thread| {
                let pool = pool.clone();
                let (live, per_key, admitted, refused, start) = (
                    Arc::clone(&live),
                    Arc::clone(&per_key),
                    Arc::clone(&admitted),
                    Arc::clone(&refused),
                    Arc::clone(&start),
                );
                std::thread::spawn(move || {
                    start.wait();
                    for round in 0..ROUNDS {
                        let key = (thread + round) % KEYS;
                        let Ok(lease) = pool
                            .try_lease_manual(i64::try_from(key).unwrap(), Duration::from_secs(30))
                        else {
                            refused.fetch_add(1, Ordering::SeqCst);
                            assert!(pool.manual_leases_in_use() <= limit);
                            continue;
                        };
                        admitted.fetch_add(1, Ordering::SeqCst);
                        let now_live = live.fetch_add(1, Ordering::SeqCst) + 1;
                        assert!(now_live <= limit, "{now_live} admitted at once");
                        let holders = per_key[key].fetch_add(1, Ordering::SeqCst) + 1;
                        assert_eq!(holders, 1, "key {key} holds two places");
                        assert!(pool.manual_leases_in_use() <= limit);
                        // Some leases travel as clones and holds, as a
                        // run's do; the place lasts until the last drops.
                        let kept = (round % 3 == 0).then(|| lease.clone().into_hold());
                        std::thread::yield_now();
                        per_key[key].fetch_sub(1, Ordering::SeqCst);
                        live.fetch_sub(1, Ordering::SeqCst);
                        drop(lease);
                        drop(kept);
                    }
                })
            })
            .collect();
        for worker in workers {
            worker
                .join()
                .expect("a worker found the gate over its limit");
        }
        assert!(admitted.load(Ordering::SeqCst) > 0);
        assert!(refused.load(Ordering::SeqCst) > 0, "the requests contended");
        assert_eq!(
            pool.manual_leases_in_use(),
            0,
            "every entry left with its last lease"
        );
    }

    fn manual_gate_one_at_a_time(pool: &ExecutorPool) {
        let now = Instant::now();
        let later = pool
            .try_lease_manual_at(1, Duration::from_secs(30), now)
            .expect("a free place");
        assert_eq!(pool.manual_leases_in_use(), 1);
        assert_eq!(
            pool.try_lease_manual_at(1, Duration::from_secs(5), now + Duration::from_millis(500))
                .map(drop),
            Err(ManualBusy {
                retry_after_secs: 30
            }),
            "a key holding a place is refused with its own estimate, in a gate not full"
        );
        assert_eq!(pool.manual_leases_in_use(), 1, "a refusal holds nothing");
        let earlier = pool
            .try_lease_manual_at(2, Duration::from_secs(10), now)
            .expect("the second place");
        assert_eq!(pool.manual_leases_in_use(), 2);

        let half = now + Duration::from_millis(500);
        assert_eq!(
            pool.try_lease_manual_at(1, Duration::from_secs(5), half)
                .map(drop),
            Err(ManualBusy {
                retry_after_secs: 30
            }),
            "the key's own estimate, not the earliest holder's"
        );
        assert_eq!(
            pool.try_lease_manual_at(3, Duration::from_secs(5), half)
                .map(drop),
            Err(ManualBusy {
                retry_after_secs: 10
            }),
            "a full gate answers with its earliest holder, rounded up"
        );
        assert_eq!(
            pool.try_lease_manual_at(3, Duration::from_secs(5), now + Duration::from_secs(20))
                .map(drop),
            Err(ManualBusy {
                retry_after_secs: 1
            }),
            "an expired estimate still gives one second"
        );
        assert_eq!(
            pool.try_lease_manual_at(1, Duration::from_secs(5), now + Duration::from_secs(40))
                .map(drop),
            Err(ManualBusy {
                retry_after_secs: 1
            }),
            "the key's own expired estimate gives one second"
        );
        assert_eq!(pool.manual_leases_in_use(), 2, "refusals hold nothing");

        let clone = earlier.clone();
        let hold = earlier.clone().into_hold();
        drop(earlier);
        assert_eq!(pool.manual_leases_in_use(), 2, "the clones keep the place");
        assert!(
            pool.try_lease_manual(2, Duration::from_secs(5)).is_err(),
            "a clone keeps its key's place"
        );
        drop(clone);
        assert_eq!(pool.manual_leases_in_use(), 2, "the hold keeps the place");
        drop(hold);
        assert_eq!(
            pool.manual_leases_in_use(),
            1,
            "the last clone removed the key"
        );
        let again = pool
            .try_lease_manual(2, Duration::from_secs(5))
            .expect("the key's place came back");
        assert_eq!(pool.manual_leases_in_use(), 2);
        drop(later);
        assert_eq!(pool.manual_leases_in_use(), 1);
        drop(again);
        assert_eq!(pool.manual_leases_in_use(), 0);
        let other = pool.clone();
        let taken = pool.try_lease_manual(9, Duration::from_secs(5)).unwrap();
        assert!(other.try_lease_manual(9, Duration::from_secs(5)).is_err());
        assert_eq!(other.manual_leases_in_use(), 1);
        drop(taken);
        assert_eq!(other.manual_leases_in_use(), 0);
    }

    fn registry_is_empty(pool: &ExecutorPool) -> bool {
        let registry = pool.registry.lock();
        registry.pending.is_empty()
            && registry.interrupts.is_empty()
            && registry.retained.is_empty()
    }

    fn hot_pool(max_concurrent: usize) -> ExecutorPool {
        use crate::bus::IngestBatch;
        use crate::hot_buffer::HotBufferConfig;

        let hot = Arc::new(HotBuffer::new(HotBufferConfig {
            max_events: 100,
            max_bytes: 1 << 20,
        }));
        // The envelope columns ingest stamps on every event: written here
        // by hand because this buffer is filled directly.
        let mut event = serde_json::Map::new();
        event.insert("_time".into(), "2026-01-03T00:05:00Z".into());
        event.insert("_ingested".into(), "2026-01-03T00:05:01Z".into());
        event.insert("service".into(), "svc".into());
        event.insert("message".into(), "hello".into());
        hot.insert_for_test(Arc::new(IngestBatch {
            batch_id: "b1".into(),
            service: "svc".into(),
            byte_size: 64,
            events: vec![event],
        }));
        ExecutorPool::for_test("/nonexistent".into(), max_concurrent, 100_000, Some(hot)).unwrap()
    }

    /// Tokio auto-advances a paused clock only when the scheduler has
    /// nothing ready, so a yield loop pins virtual time exactly where the
    /// test put it while the worker thread makes real progress. The bound
    /// is the real clock: it only turns a deadlock into a failure instead
    /// of a hang.
    async fn until(label: &str, mut reached: impl FnMut() -> bool) {
        let start = std::time::Instant::now();
        while !reached() {
            assert!(
                start.elapsed() < Duration::from_secs(20),
                "the worker never reached: {label}"
            );
            tokio::task::yield_now().await;
        }
    }

    /// A global subscriber, not a thread-local one: the events these
    /// tests are about are emitted from the blocking worker thread, which
    /// does not inherit `set_default`. A work sink matches POOL and
    /// `query_id` together: ids start at zero in every pool, and the plain
    /// `cargo test` harness runs tests in threads. Timing events carry no
    /// pool: a work sink keeps them by the test's own `request_id`.
    mod capture {
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex as StdMutex, OnceLock};

        type Events = Arc<StdMutex<Vec<HashMap<String, String>>>>;

        enum Keep {
            Work {
                pool: u64,
                query_id: u64,
                request_id: Option<String>,
            },
            /// Every event in the process: for asserting that an event
            /// type is emitted by nothing at all.
            Everything,
        }

        struct Sink {
            keep: Keep,
            events: Events,
        }

        impl Keep {
            fn keeps(&self, fields: &HashMap<String, String>) -> bool {
                match self {
                    Self::Everything => true,
                    Self::Work {
                        pool,
                        query_id,
                        request_id,
                    } => {
                        let field = |name: &str| fields.get(name).map(String::as_str);
                        (field("pool") == Some(pool.to_string().as_str())
                            && field("query_id") == Some(query_id.to_string().as_str()))
                            || (request_id.is_some()
                                && field("request_id") == request_id.as_deref())
                    }
                }
            }
        }

        static SINK: StdMutex<Option<Sink>> = StdMutex::new(None);
        /// One capturing test at a time: the sink is process-wide.
        static TURN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

        struct Fields<'a>(&'a mut HashMap<String, String>);

        impl tracing::field::Visit for Fields<'_> {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.insert(field.name().to_owned(), format!("{value:?}"));
            }
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                self.0.insert(field.name().to_owned(), value.to_owned());
            }
            fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
                self.0.insert(field.name().to_owned(), value.to_string());
            }
        }

        struct Layer;

        impl<S> tracing_subscriber::Layer<S> for Layer
        where
            S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
        {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                let mut fields = HashMap::new();
                event.record(&mut Fields(&mut fields));
                let sink = SINK.lock().expect("capture sink poisoned");
                if let Some(sink) = sink.as_ref()
                    && sink.keep.keeps(&fields)
                {
                    sink.events.lock().expect("capture poisoned").push(fields);
                }
            }
        }

        pub(super) struct Capture {
            events: Events,
            _turn: tokio::sync::MutexGuard<'static, ()>,
        }

        impl Capture {
            pub(super) fn of_type(&self, event_type: &str) -> Vec<HashMap<String, String>> {
                self.events
                    .lock()
                    .expect("capture poisoned")
                    .iter()
                    .filter(|f| f.get("event_type").map(String::as_str) == Some(event_type))
                    .cloned()
                    .collect()
            }
        }

        impl Drop for Capture {
            fn drop(&mut self) {
                *SINK.lock().expect("capture sink poisoned") = None;
            }
        }

        pub(super) async fn of_query(pool: &super::ExecutorPool, query_id: u64) -> Capture {
            install(Keep::Work {
                pool: pool.instance,
                query_id,
                request_id: None,
            })
            .await
        }

        pub(super) async fn of_timed(
            pool: &super::ExecutorPool,
            query_id: u64,
            request_id: &str,
        ) -> Capture {
            install(Keep::Work {
                pool: pool.instance,
                query_id,
                request_id: Some(request_id.to_owned()),
            })
            .await
        }

        pub(super) async fn everything() -> Capture {
            install(Keep::Everything).await
        }

        async fn install(keep: Keep) -> Capture {
            use tracing_subscriber::prelude::*;

            static INSTALLED: OnceLock<()> = OnceLock::new();
            INSTALLED.get_or_init(|| {
                // Ignored on the second binary-wide attempt: another test
                // may already own the global subscriber, and this layer
                // is additive either way.
                let _ = tracing::subscriber::set_global_default(
                    tracing_subscriber::registry().with(Layer),
                );
            });

            let turn = TURN.lock().await;
            let events: Events = Arc::default();
            *SINK.lock().expect("capture sink poisoned") = Some(Sink {
                keep,
                events: Arc::clone(&events),
            });
            Capture {
                events,
                _turn: turn,
            }
        }
    }

    /// A task registers its timers on its first poll, so advancing paused
    /// time before that would place a deadline the test then never
    /// reaches — and a paused runtime with a blocking thread in flight
    /// does not reliably auto-advance to it either.
    async fn settle() {
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
    }

    #[test]
    fn severity_columns_come_from_the_snapshot_it_is_given() {
        use trawl_core::schema::{CanonicalType, FieldTypes};

        let mut executed_under = FieldTypes::new();
        executed_under.insert("sev", CanonicalType::Severity);
        assert_eq!(
            severity_columns_for(&executed_under, "* | table sev", &CancelLatch::never()).unwrap(),
            vec!["sev".to_owned()],
            "a column pinned SEVERITY in the run's snapshot renders tokens"
        );

        let mut repinned = FieldTypes::new();
        repinned.insert("sev", CanonicalType::BigInt);
        assert!(
            severity_columns_for(&repinned, "* | table sev", &CancelLatch::never())
                .unwrap()
                .is_empty(),
            "a later BIGINT pin is not the interpretation those rows carry"
        );
    }

    #[test]
    fn worker_parses_read_the_latch() {
        use trawl_core::schema::FieldTypes;

        let pins = FieldTypes::new();
        let cancelled = CancelLatch::new(Arc::new(AtomicBool::new(true)));
        let dsl = "message=/a+b/ | extract /(?P<sev>c+)/ | table sev";
        let builds = trawl_core::pattern::thread_builds();
        let severity = severity_columns_for(&pins, dsl, &cancelled);
        assert!(
            matches!(severity, Err(EngineError::Cancelled)),
            "{severity:?}"
        );
        let debug = capture_pool_debug(dsl, "src", None, &pins, &cancelled);
        assert!(matches!(debug, Err(EngineError::Cancelled)), "{debug:?}");
        assert_eq!(trawl_core::pattern::thread_builds(), builds);

        let running = CancelLatch::never();
        assert!(
            severity_columns_for(&pins, "| | |", &running)
                .unwrap()
                .is_empty()
        );
        let debug = capture_pool_debug("| | |", "src", None, &pins, &running).unwrap();
        assert_eq!(
            (debug.service_filter, debug.sql.as_str()),
            (None, ""),
            "an unparseable query previews nothing"
        );
    }

    /// The envelope's own `_severity` is deliberately not named: every
    /// renderer keys that column by name, so the list carries only the
    /// columns a pipeline moved the pin onto (`PinScope::severity_columns`).
    #[test]
    fn severity_columns_shortcut_agrees_with_the_walk() {
        use trawl_core::schema::{CanonicalType, FieldTypes};

        let mut pins = FieldTypes::new();
        pins.insert(trawl_core::schema::SEVERITY, CanonicalType::Severity);
        pins.insert("lvl", CanonicalType::Severity);
        pins.insert("status", CanonicalType::Varchar);

        assert_eq!(
            severity_columns_for(&pins, "service=nginx last=1h", &CancelLatch::never()).unwrap(),
            vec!["lvl".to_owned()],
            "a pipe-free query answers from the root scope without a parse"
        );
        assert_eq!(
            severity_columns_for(&pins, "message=/a|b/ last=1h", &CancelLatch::never()).unwrap(),
            vec!["lvl".to_owned()],
            "a `|` inside a regex costs the parse and answers the same"
        );
        assert!(
            severity_columns_for(&pins, "* | table status", &CancelLatch::never())
                .unwrap()
                .is_empty(),
            "a projection that leaves the column out names nothing"
        );
        assert!(
            severity_columns_for(&pins, "| | |", &CancelLatch::never())
                .unwrap()
                .is_empty()
        );
        assert!(
            severity_columns_for(&FieldTypes::new(), "service=nginx", &CancelLatch::never())
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn execute_stamps_severity_columns_from_the_runs_own_snapshot() {
        use crate::bus::IngestBatch;
        use crate::hot_buffer::HotBufferConfig;
        use trawl_core::schema::CanonicalType;

        let catalog = Arc::new(crate::catalog::FieldCatalog::new());
        catalog.repin("lvl", CanonicalType::Severity);

        let hot = Arc::new(
            HotBuffer::new(HotBufferConfig {
                max_events: 100,
                max_bytes: 1 << 20,
            })
            .with_field_catalog(Arc::clone(&catalog)),
        );
        let mut event = serde_json::Map::new();
        event.insert("_time".into(), "2026-01-03T00:05:00Z".into());
        event.insert("_ingested".into(), "2026-01-03T00:05:01Z".into());
        event.insert("service".into(), "svc".into());
        event.insert("lvl".into(), 17.into());
        hot.insert_for_test(Arc::new(IngestBatch {
            batch_id: "b1".into(),
            service: "svc".into(),
            byte_size: 64,
            events: vec![event],
        }));

        let pool =
            ExecutorPool::for_test("/nonexistent".into(), 2, 100_000, Some(Arc::clone(&hot)))
                .unwrap()
                .with_field_catalog(Arc::clone(&catalog));

        let outcome = pool
            .execute(
                pool.allocate_query_id(),
                "* | table lvl",
                Deadline::after(Duration::from_secs(30)),
                false,
                TEST_WORK,
            )
            .await;
        assert!(outcome.result.is_ok(), "{:?}", outcome.result);
        assert_eq!(
            outcome.severity_columns,
            vec!["lvl".to_owned()],
            "the run's own snapshot pins `lvl` SEVERITY, so the answer says so"
        );

        catalog.repin("lvl", CanonicalType::BigInt);
        let outcome = pool
            .execute(
                pool.allocate_query_id(),
                "* | table lvl",
                Deadline::after(Duration::from_secs(30)),
                false,
                TEST_WORK,
            )
            .await;
        assert!(outcome.result.is_ok(), "{:?}", outcome.result);
        assert!(
            outcome.severity_columns.is_empty(),
            "a repinned field stops being presented as a severity"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn execute_carries_the_histogram_from_the_worker() {
        use crate::histogram::HistogramAsk;

        let pool = hot_pool(1);
        let seams = pool.seams();
        let presenting = seams.hold(Seam::Presenting);
        let id = pool.allocate_query_id();
        let submitted = pool.clone();
        let request = tokio::spawn(async move {
            submitted
                .execute(
                    id,
                    "* | head 1",
                    Deadline::after(Duration::from_secs(60)),
                    false,
                    TEST_WORK.with_histogram(HistogramAsk { kept_some: true }),
                )
                .await
        });
        until("the worker reaches its presentation", || {
            presenting.arrivals() == 1
        })
        .await;
        assert_eq!(pool.available_permits(), 0, "the worker holds the permit");
        presenting.release();
        let outcome = request.await.expect("the request joins");
        let rows = outcome
            .result
            .as_ref()
            .expect("the run succeeds")
            .row_count();
        assert_eq!(rows, 1);
        let histogram = outcome.histogram.expect("the worker counted");
        assert!(histogram.consistent_with(rows), "{histogram:?}");
        let trawl_api::EventHistogram::Ready {
            start,
            buckets,
            kept_some,
            ..
        } = histogram
        else {
            panic!("the hot event has a usable `_time`: {histogram:?}");
        };
        assert_eq!(start, "2026-01-03T00:05:00.000000Z");
        assert_eq!(buckets[0].events, 1);
        assert!(kept_some, "the ask's flag is the answer's");

        let unasked = pool
            .execute(
                pool.allocate_query_id(),
                "*",
                Deadline::after(Duration::from_secs(60)),
                false,
                TEST_WORK,
            )
            .await;
        assert!(unasked.result.is_ok(), "{:?}", unasked.result);
        assert_eq!(unasked.histogram, None, "work that did not ask");

        let failed = pool
            .execute(
                pool.allocate_query_id(),
                "* | stats",
                Deadline::after(Duration::from_secs(60)),
                false,
                TEST_WORK.with_histogram(HistogramAsk { kept_some: false }),
            )
            .await;
        assert!(failed.result.is_err(), "{:?}", failed.result);
        assert_eq!(failed.histogram, None, "a failed run has no rows to count");
    }

    /// The DSL has no `|`, so the severity walk reads no latch and the
    /// histogram walk is the one that sees the cancel.
    #[tokio::test(start_paused = true)]
    async fn histogram_walk_stops_on_a_latched_cancel() {
        use crate::histogram::HistogramAsk;

        let pool = hot_pool(1);
        let seams = pool.seams();
        let presenting = seams.hold(Seam::Presenting);
        let id = pool.allocate_query_id();
        let submitted = pool.clone();
        let request = tokio::spawn(async move {
            submitted
                .execute(
                    id,
                    "*",
                    Deadline::after(Duration::from_secs(60)),
                    false,
                    TEST_WORK.with_histogram(HistogramAsk { kept_some: false }),
                )
                .await
        });
        until("the worker reaches its presentation", || {
            presenting.arrivals() == 1
        })
        .await;
        assert!(pool.cancel_by_id(id), "the work is still registered");
        presenting.release();
        let outcome = request.await.expect("the request joins");
        assert!(
            matches!(
                outcome.result,
                Err(ServerError::Engine(EngineError::Cancelled))
            ),
            "{:?}",
            outcome.result
        );
        assert_eq!(outcome.histogram, None);
        assert!(outcome.severity_columns.is_empty());
    }

    #[tokio::test]
    async fn an_engine_refusal_crosses_the_pool_intact() {
        use crate::bus::IngestBatch;
        use crate::hot_buffer::HotBufferConfig;

        let hot = Arc::new(HotBuffer::new(HotBufferConfig {
            max_events: 100,
            max_bytes: 1 << 20,
        }));
        // The envelope columns a snapshot carries by contract: written
        // here by hand because this buffer is filled directly, not
        // through ingest.
        let mut event = serde_json::Map::new();
        event.insert("_time".into(), "2026-01-03T00:05:00Z".into());
        event.insert("_ingested".into(), "2026-01-03T00:05:01Z".into());
        event.insert("service".into(), "svc".into());
        event.insert("hostname".into(), "web-1".into());
        hot.insert_for_test(Arc::new(IngestBatch {
            batch_id: "b1".into(),
            service: "svc".into(),
            byte_size: 64,
            events: vec![event],
        }));

        let pool = ExecutorPool::for_test("/nonexistent".into(), 2, 100_000, Some(hot)).unwrap();
        let outcome = pool
            .execute(
                pool.allocate_query_id(),
                "* | let t = null | timechart on t count()",
                Deadline::after(Duration::from_secs(30)),
                false,
                TEST_WORK,
            )
            .await;

        let expected = "timechart on 't' is not a timestamp: INTEGER";
        match outcome.result {
            Err(err @ ServerError::Engine(trawl_engine::error::EngineError::Refused { .. })) => {
                assert_eq!(err.error_class(), "refused");
                assert_eq!(
                    err.safe_message(),
                    expected,
                    "the refusal is trawl-authored and quotes the caller's own \
                     tokens, so redaction has nothing to take"
                );
            }
            Err(other) => panic!("expected the bucket-type refusal, got {other:?}"),
            Ok(result) => panic!("expected a refusal, got {} rows", result.row_count()),
        }
    }

    /// A pool over one compacted cold event and one hot event: a read that
    /// answered from parquet alone would come back one row short.
    async fn cold_and_hot_pool() -> (tempfile::TempDir, Arc<HotBuffer>, ExecutorPool) {
        use crate::bus::IngestBatch;
        use crate::hot_buffer::HotBufferConfig;
        use crate::ingest::wal::WalWriter;

        let dir = tempfile::tempdir().unwrap();
        let (wal_dir, data_dir) = (dir.path().join("wal"), dir.path().join("data"));
        std::fs::create_dir_all(&data_dir).unwrap();
        let event = |message: &str| {
            let mut event = serde_json::Map::new();
            event.insert("_time".into(), "2026-01-03T00:05:00Z".into());
            event.insert("_ingested".into(), "2026-01-03T00:05:01Z".into());
            event.insert("service".into(), "svc".into());
            event.insert("message".into(), message.into());
            event
        };

        let wal = WalWriter::new(wal_dir.clone());
        wal.ensure_dir().unwrap();
        let mut line = serde_json::to_vec(&event("cold")).unwrap();
        line.push(b'\n');
        wal.write("prod", "svc", &line).unwrap();
        crate::ingest::compaction::compact_once(
            &wal_dir,
            &data_dir,
            Duration::ZERO,
            false,
            None,
            500,
            "2GB",
            None,
        )
        .await
        .unwrap();

        let hot = Arc::new(HotBuffer::new(HotBufferConfig {
            max_events: 100,
            max_bytes: 1 << 20,
        }));
        hot.insert_for_test(Arc::new(IngestBatch {
            batch_id: "prod/svc_hot".into(),
            service: "svc".into(),
            byte_size: 64,
            events: vec![event("hot")],
        }));
        let pool = ExecutorPool::for_test(
            data_dir.to_str().unwrap().to_owned(),
            1,
            100_000,
            Some(Arc::clone(&hot)),
        )
        .unwrap();
        (dir, hot, pool)
    }

    #[tokio::test]
    async fn drop_of_an_absent_field_keeps_every_row_on_the_hot_lanes() {
        use crate::bus::IngestBatch;
        use crate::hot_buffer::HotBufferConfig;

        async fn columns_and_rows(pool: &ExecutorPool, dsl: &str) -> (Vec<String>, usize) {
            let result = pool
                .execute(
                    pool.allocate_query_id(),
                    dsl,
                    Deadline::after(Duration::from_secs(30)),
                    false,
                    TEST_WORK,
                )
                .await
                .result
                .unwrap_or_else(|e| panic!("{dsl}: {e:?}"));
            let mut names: Vec<String> = result.columns.iter().map(|c| c.name.clone()).collect();
            names.sort();
            (names, result.row_count())
        }

        let (_dir, _hot, hot_and_cold) = cold_and_hot_pool().await;
        let all = columns_and_rows(&hot_and_cold, "*").await;
        assert_eq!(all.1, 2, "one cold row and one hot row");
        assert_eq!(columns_and_rows(&hot_and_cold, "* | drop nope").await, all);

        let hot = Arc::new(HotBuffer::new(HotBufferConfig {
            max_events: 100,
            max_bytes: 1 << 20,
        }));
        let mut event = serde_json::Map::new();
        event.insert("_time".into(), "2026-01-03T00:05:00Z".into());
        event.insert("_ingested".into(), "2026-01-03T00:05:01Z".into());
        event.insert("service".into(), "svc".into());
        event.insert("message".into(), "hello".into());
        hot.insert_for_test(Arc::new(IngestBatch {
            batch_id: "b1".into(),
            service: "svc".into(),
            byte_size: 64,
            events: vec![event],
        }));
        let hot_only =
            ExecutorPool::for_test("/nonexistent".into(), 1, 100_000, Some(hot)).unwrap();
        let all = columns_and_rows(&hot_only, "*").await;
        assert_eq!(all.1, 1, "the one hot row");
        assert!(all.0.contains(&"message".to_owned()), "{all:?}");
        assert_eq!(columns_and_rows(&hot_only, "* | drop nope").await, all);
    }

    fn parquet_rows(bytes: &[u8]) -> i64 {
        let file = tempfile::Builder::new()
            .suffix(".parquet")
            .tempfile()
            .unwrap();
        std::fs::write(file.path(), bytes).unwrap();
        duckdb::Connection::open_in_memory()
            .unwrap()
            .query_row(
                &format!(
                    "SELECT count(*) FROM read_parquet('{}')",
                    file.path().display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap()
    }

    async fn assert_snapshot_refusal(error: ServerError, kind: crate::error::CauseKind) {
        assert!(matches!(error, ServerError::HotSnapshot(_)), "{error:?}");
        assert_eq!(error.error_class(), "service_unavailable");
        assert_eq!(error.cause_kind(), kind);
        let response = axum::response::IntoResponse::into_response(error);
        assert_eq!(
            response.status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["code"], "service_unavailable");
    }

    #[tokio::test]
    async fn snapshot_failure_refuses_a_query_instead_of_answering_cold_only() {
        let (_dir, hot, pool) = cold_and_hot_pool().await;
        let query = || {
            pool.execute(
                pool.allocate_query_id(),
                "* | head 100",
                Deadline::after(Duration::from_secs(30)),
                false,
                TEST_WORK,
            )
        };
        let rows = |outcome: ExecuteOutcome| outcome.result.expect("the read answers").row_count();
        assert_eq!(rows(query().await), 2, "one cold row and one hot row");

        hot.fail_next_snapshot_for_test();
        let error = query()
            .await
            .result
            .expect_err("the read refuses instead of answering cold-only");
        assert_snapshot_refusal(error, crate::error::CauseKind::IoStorageFull).await;

        assert_eq!(rows(query().await), 2, "the fault latched nothing");
    }

    #[tokio::test]
    async fn snapshot_failure_refuses_an_export_instead_of_answering_cold_only() {
        let (_dir, hot, pool) = cold_and_hot_pool().await;
        let export = || {
            pool.export_parquet(
                pool.allocate_query_id(),
                "* | head 100",
                1_000,
                Deadline::after(Duration::from_secs(30)),
                TEST_WORK,
            )
        };
        let (bytes, rows) = export().await.expect("the export answers");
        assert_eq!(parquet_rows(&bytes), 2, "one cold row and one hot row");
        assert_eq!(rows, 2, "the returned count is the file's");

        hot.fail_next_snapshot_for_test();
        let error = export()
            .await
            .map(|(bytes, _)| parquet_rows(&bytes))
            .expect_err("the export refuses instead of answering cold-only");
        assert_snapshot_refusal(error, crate::error::CauseKind::IoStorageFull).await;

        let (bytes, rows) = export().await.expect("the export answers");
        assert_eq!(parquet_rows(&bytes), 2, "the fault latched nothing");
        assert_eq!(rows, 2, "the returned count is the file's");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn snapshot_failure_on_a_non_utf8_path_refuses_the_read() {
        use std::os::unix::ffi::OsStrExt as _;

        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(std::ffi::OsStr::from_bytes(b"hot-\xff"));
        std::fs::create_dir(&dir).unwrap();
        let snapshot = HotSnapshot {
            file: Arc::new(
                tempfile::Builder::new()
                    .suffix(".ndjson")
                    .tempfile_in(&dir)
                    .unwrap(),
            ),
            field_types: Arc::default(),
            columns: Arc::default(),
        };
        assert!(snapshot.path().to_str().is_none());

        let error = hot_source(Ok(Some(snapshot))).expect_err("the read refuses");
        assert_snapshot_refusal(error, crate::error::CauseKind::IoInvalidData).await;
        assert!(
            hot_source(Ok(None)).unwrap().is_none(),
            "an empty buffer is no hot source, not a refusal"
        );
    }

    fn snapshot_files(pool: &ExecutorPool) -> Vec<u64> {
        std::fs::read_dir(pool.scratch().path())
            .unwrap()
            .map(|entry| entry.unwrap())
            .filter(|entry| trawl_engine::scratch::is_snapshot_file_name(&entry.file_name()))
            .filter_map(|entry| entry.metadata().ok().map(|meta| meta.len()))
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    #[allow(clippy::too_many_lines)]
    async fn snapshot_files_bounded_by_permits() {
        use crate::bus::IngestBatch;
        use crate::hot_buffer::HotBufferConfig;
        use std::sync::atomic::AtomicUsize;

        type Round = (
            Arc<seam::Gate>,
            Vec<tokio::task::JoinHandle<Result<(Vec<u8>, usize), ServerError>>>,
        );
        const PERMITS: usize = 3;
        const ROUNDS: usize = 3;

        let hot = Arc::new(HotBuffer::new(HotBufferConfig {
            max_events: 10_000,
            max_bytes: 64 << 20,
        }));
        let pool = ExecutorPool::for_test(
            "/nonexistent".into(),
            PERMITS,
            100_000,
            Some(Arc::clone(&hot)),
        )
        .unwrap();
        let seams = pool.seams();
        let mut inserted = 0_usize;
        let mut insert = || {
            inserted += 1;
            let events = (0..50)
                .map(|i| {
                    let mut event = serde_json::Map::new();
                    event.insert("_time".into(), "2026-01-03T00:05:00Z".into());
                    event.insert("_ingested".into(), "2026-01-03T00:05:01Z".into());
                    event.insert("service".into(), "svc".into());
                    event.insert(
                        "message".into(),
                        format!("batch {inserted} event {i}").into(),
                    );
                    event
                })
                .collect();
            hot.insert_for_test(Arc::new(IngestBatch {
                batch_id: format!("prod/svc_{inserted}").into(),
                service: "svc".into(),
                byte_size: 4096,
                events,
            }));
        };

        let stop = Arc::new(AtomicBool::new(false));
        let most = Arc::new(AtomicUsize::new(0));
        let sampler = std::thread::spawn({
            let (pool, stop, most) = (pool.clone(), Arc::clone(&stop), Arc::clone(&most));
            move || {
                while !stop.load(Ordering::SeqCst) {
                    most.fetch_max(snapshot_files(&pool).len(), Ordering::SeqCst);
                    std::thread::yield_now();
                }
            }
        });

        let mut previous: Option<Round> = None;
        for round in 0..ROUNDS {
            let gate = seams.hold(Seam::Finished);
            if let Some((gate, exports)) = previous.take() {
                gate.release();
                for export in exports {
                    export.await.unwrap().expect("the export answers");
                }
                assert_eq!(
                    snapshot_files(&pool).len(),
                    1,
                    "round {round}: with no permit held, only the cached file remains"
                );
            }
            let mut exports = Vec::new();
            for held in 1..=PERMITS {
                insert();
                let pool = pool.clone();
                exports.push(tokio::spawn(async move {
                    pool.export_parquet(
                        pool.allocate_query_id(),
                        "*",
                        1_000,
                        Deadline::after(Duration::from_secs(60)),
                        TEST_WORK,
                    )
                    .await
                }));
                until("an export holds its snapshot", || gate.arrivals() == held).await;
            }
            let files = snapshot_files(&pool);
            assert_eq!(
                files.len(),
                PERMITS,
                "round {round}: one file per permit, each its own generation, the newest cached"
            );
            let whole = hot.serialized_len_for_test();
            assert!(
                files.iter().all(|&len| len > 0 && len <= whole),
                "round {round}: every file is at most the serialized buffer ({whole}): {files:?}"
            );
            previous = Some((gate, exports));
        }
        let (gate, exports) = previous.unwrap();
        gate.release();
        for export in exports {
            export.await.unwrap().expect("the export answers");
        }
        stop.store(true, Ordering::SeqCst);
        sampler.join().unwrap();

        let most = most.load(Ordering::SeqCst);
        assert!(
            most <= PERMITS + 1,
            "{most} snapshot files existed at once with {PERMITS} permits"
        );
        assert_eq!(
            snapshot_files(&pool).len(),
            1,
            "the cached file alone remains"
        );
    }

    #[tokio::test]
    async fn a_failed_run_stamps_no_severity_columns() {
        let catalog = Arc::new(crate::catalog::FieldCatalog::new());
        catalog.repin("lvl", trawl_core::schema::CanonicalType::Severity);
        let pool = ExecutorPool::for_test("/nonexistent".into(), 2, 100_000, None)
            .unwrap()
            .with_field_catalog(Arc::clone(&catalog));

        let outcome = pool
            .execute(
                pool.allocate_query_id(),
                "| | |",
                Deadline::after(Duration::from_secs(10)),
                false,
                TEST_WORK,
            )
            .await;
        assert!(outcome.result.is_err(), "{:?}", outcome.result);
        assert!(outcome.severity_columns.is_empty());

        // A timeout: the outcome is assembled on the async side, where no
        // snapshot and no walk exist at all. The worker is parked at this
        // pool's own seam rather than slowed by the process-global delay,
        // which a sibling test can reset out from under this one. The clock
        // only moves once the worker is PAST its work-start transition.
        tokio::time::pause();
        let seams = pool.seams();
        let parked = seams.hold(Seam::Started);
        let id = pool.allocate_query_id();
        let submitted = pool.clone();
        let request = tokio::spawn(async move {
            submitted
                .execute(
                    id,
                    "* | table lvl",
                    Deadline::after(Duration::from_secs(5)),
                    false,
                    TEST_WORK,
                )
                .await
        });
        until("work starts", || parked.arrivals() == 1).await;
        tokio::time::advance(Duration::from_secs(6)).await;
        let outcome = request.await.expect("the request joins");
        parked.release();
        assert!(
            matches!(outcome.result, Err(ServerError::Timeout)),
            "{:?}",
            outcome.result
        );
        assert!(outcome.severity_columns.is_empty());
    }

    #[tokio::test]
    async fn pool_rejects_invalid_dsl() {
        let pool = ExecutorPool::for_test("/nonexistent".into(), 2, 100_000, None).unwrap();
        // Must start with `|` to trigger a parse error — bare text is valid DSL.
        let outcome = pool
            .execute(
                pool.allocate_query_id(),
                "| | invalid",
                Deadline::after(Duration::from_secs(10)),
                false,
                TEST_WORK,
            )
            .await;
        assert!(outcome.result.is_err());
    }

    #[tokio::test]
    async fn pool_respects_concurrency_limit() {
        let pool = ExecutorPool::for_test("/nonexistent".into(), 1, 100_000, None).unwrap();
        let _ = pool
            .execute(
                pool.allocate_query_id(),
                "service:test",
                Deadline::after(Duration::from_secs(10)),
                false,
                TEST_WORK,
            )
            .await;
    }

    #[tokio::test]
    async fn pool_reuses_executors() {
        let pool = ExecutorPool::for_test("/nonexistent".into(), 2, 100_000, None).unwrap();

        let idle_before = idle_len(&pool);
        let _ = pool
            .execute(
                pool.allocate_query_id(),
                "service:test",
                Deadline::after(Duration::from_secs(10)),
                false,
                TEST_WORK,
            )
            .await;
        let _ = pool
            .execute(
                pool.allocate_query_id(),
                "service:test",
                Deadline::after(Duration::from_secs(10)),
                false,
                TEST_WORK,
            )
            .await;
        let idle_after = idle_len(&pool);

        assert_eq!(
            idle_before, idle_after,
            "executors should be returned to pool"
        );
    }

    #[test]
    fn fallback_glob_derived_from_base_dir() {
        let pool = ExecutorPool::for_test("/var/lib/trawl/data".into(), 1, 100_000, None).unwrap();
        assert_eq!(&*pool.fallback_glob, "/var/lib/trawl/data/**/*.parquet");
    }

    #[test]
    fn configured_data_root_is_passed_directly() {
        for base in ["/var/lib/trawl/data", "/var/lib/trawl/data/"] {
            assert_eq!(
                service_glob(base, "nginx"),
                "/var/lib/trawl/data/**/nginx.parquet"
            );
        }
        assert_eq!(service_glob("/", "nginx"), "/**/nginx.parquet");
    }

    #[test]
    fn fallback_glob_strips_trailing_slash() {
        let pool = ExecutorPool::for_test("/var/lib/trawl/data/".into(), 1, 100_000, None).unwrap();
        assert_eq!(&*pool.fallback_glob, "/var/lib/trawl/data/**/*.parquet");
    }

    #[tokio::test(start_paused = true)]
    async fn queue_wait_counts_against_the_deadline() {
        let pool = ExecutorPool::for_test("/nonexistent".into(), 1, 100_000, None).unwrap();
        let idle_before = idle_len(&pool);
        let held = pool
            .exclusive(Duration::from_secs(1))
            .await
            .expect("the idle pool grants exclusivity at once");

        let start = tokio::time::Instant::now();
        let outcome = pool
            .execute(
                pool.allocate_query_id(),
                "*",
                Deadline::after(Duration::from_secs(5)),
                false,
                TEST_WORK,
            )
            .await;
        let waited = start.elapsed();

        assert!(
            matches!(&outcome.result, Err(ServerError::CapacityNotStarted)),
            "a query that never left the queue is refused, not timed out: {:?}",
            outcome.result
        );
        assert_eq!(waited, Duration::from_secs(5), "it waited the budget");
        assert_eq!(
            idle_len(&pool),
            idle_before,
            "a query that never left the queue took no executor"
        );
        drop(held);
    }

    #[tokio::test(start_paused = true)]
    async fn publication_wait_counts_against_the_deadline() {
        let pool = ExecutorPool::for_test("/nonexistent".into(), 1, 100_000, None).unwrap();
        let idle_before = idle_len(&pool);
        let writer = pool.publication.write().await;

        let start = tokio::time::Instant::now();
        let outcome = pool
            .execute(
                pool.allocate_query_id(),
                "*",
                Deadline::after(Duration::from_secs(5)),
                false,
                TEST_WORK,
            )
            .await;
        let waited = start.elapsed();

        assert!(
            matches!(&outcome.result, Err(ServerError::CapacityNotStarted)),
            "a query that never passed the gate is refused, not timed out: {:?}",
            outcome.result
        );
        assert_eq!(waited, Duration::from_secs(5), "it waited the budget");
        assert_eq!(
            pool.available_permits(),
            1,
            "the permit it was holding is released"
        );
        assert_eq!(
            idle_len(&pool),
            idle_before,
            "a query that never passed the gate took no executor"
        );
        drop(writer);
    }

    #[tokio::test(start_paused = true)]
    async fn pool_timeout_returns_error() {
        // The worker is held past its work-start transition, so what the
        // request reports is a timeout on work that really is running.
        // Sleeping instead would race the blocking pool's own startup.
        let pool = ExecutorPool::for_test("/nonexistent".into(), 1, 100_000, None).unwrap();
        let seams = pool.seams();
        let started = seams.hold(Seam::Started);
        let id = pool.allocate_query_id();
        let submitted = pool.clone();
        let request = tokio::spawn(async move {
            submitted
                .execute(
                    id,
                    "*",
                    Deadline::after(Duration::from_secs(5)),
                    false,
                    TEST_WORK,
                )
                .await
        });
        until("work starts", || started.arrivals() == 1).await;
        tokio::time::advance(Duration::from_secs(6)).await;
        let outcome = request.await.expect("the request joins");
        started.release();
        assert!(
            matches!(outcome.result, Err(ServerError::Timeout)),
            "expected Timeout, got: {:?}",
            outcome.result
        );
    }

    #[tokio::test(start_paused = true)]
    async fn pool_executor_reclaimed_after_timeout() {
        let pool = ExecutorPool::for_test("/nonexistent".into(), 1, 100_000, None).unwrap();
        let seams = pool.seams();
        let started = seams.hold(Seam::Started);
        let idle_before = idle_len(&pool);
        let id = pool.allocate_query_id();
        let submitted = pool.clone();
        let request = tokio::spawn(async move {
            submitted
                .execute(
                    id,
                    "*",
                    Deadline::after(Duration::from_secs(5)),
                    false,
                    TEST_WORK,
                )
                .await
        });
        until("work starts", || started.arrivals() == 1).await;
        tokio::time::advance(Duration::from_secs(6)).await;
        let _ = request.await.expect("the request joins");
        assert_eq!(
            idle_len(&pool),
            idle_before - 1,
            "the timed-out query still holds its executor"
        );

        started.release();
        until("the executor comes back", || {
            idle_len(&pool) == idle_before && pool.available_permits() == 1
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exclusive_drains_queries_and_blocks_new_ones() {
        let pool = ExecutorPool::for_test("/nonexistent".into(), 2, 100_000, None).unwrap();

        let guard = pool
            .exclusive(Duration::from_secs(1))
            .await
            .expect("an idle pool is immediately exclusive");
        assert_eq!(pool.available_permits(), 0, "every permit is held");

        let p2 = pool.clone();
        let queued = tokio::spawn(async move {
            p2.execute(
                p2.allocate_query_id(),
                "service:test",
                Deadline::after(Duration::from_secs(10)),
                false,
                TEST_WORK,
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !queued.is_finished(),
            "a query must not execute while the cutover holds the pool"
        );

        drop(guard);
        let outcome = queued.await.expect("queued query joins");
        let _ = outcome.result;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exclusive_blocks_field_value_sampling() {
        let pool = ExecutorPool::for_test("/nonexistent".into(), 2, 100_000, None).unwrap();
        let guard = pool
            .exclusive(Duration::from_secs(1))
            .await
            .expect("an idle pool is immediately exclusive");

        let p2 = pool.clone();
        let queued = tokio::spawn(async move {
            p2.sample_field_values(
                "service",
                None,
                10,
                Deadline::after(Duration::from_secs(10)),
                TEST_SAMPLE_WORK,
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !queued.is_finished(),
            "value sampling must not read parquet while the cutover holds the pool"
        );

        drop(guard);
        // (The sample itself fails against a nonexistent corpus; the point
        // is that it only ran once exclusivity was released.)
        let _ = queued.await.expect("queued sample joins");
    }

    #[tokio::test]
    async fn field_value_sampling_publication_timeout_releases_permit() {
        let pool = ExecutorPool::for_test("/nonexistent".into(), 1, 100_000, None).unwrap();
        let writer = pool.publication.write().await;
        let idle_before = idle_len(&pool);

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            pool.sample_field_values(
                "service",
                None,
                10,
                Deadline::after(Duration::from_millis(20)),
                TEST_SAMPLE_WORK,
            ),
        )
        .await
        .expect("publication wait must return before the writer releases its guard");

        assert!(
            matches!(&result, Err(ServerError::CapacityNotStarted)),
            "a sample that never started is a capacity refusal, got {result:?}"
        );
        assert_eq!(
            pool.available_permits(),
            1,
            "the waiting permit is released"
        );
        assert_eq!(
            idle_len(&pool),
            idle_before,
            "publication timeout must not take an executor"
        );
        let exclusive = pool
            .exclusive(Duration::from_secs(1))
            .await
            .expect("the recovered permit is available while publication remains blocked");
        drop(exclusive);
        drop(writer);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exclusive_times_out_bounded_while_a_query_runs() {
        let pool = ExecutorPool::for_test("/nonexistent".into(), 2, 100_000, None).unwrap();
        // This pool's own seam holds the query mid-flight: waiting a fixed
        // 50 ms for it to take its permit, and relying on a delay any
        // sibling test could reset, is a bet on how busy the runner is.
        let seams = pool.seams();
        let parked = seams.hold(Seam::Started);
        let p2 = pool.clone();
        let running = tokio::spawn(async move {
            p2.execute(
                p2.allocate_query_id(),
                "service:test",
                Deadline::after(Duration::from_secs(10)),
                false,
                TEST_WORK,
            )
            .await
        });
        until("the query takes its permit", || parked.arrivals() == 1).await;

        let err = pool
            .exclusive(Duration::from_millis(20))
            .await
            .expect_err("a held permit must bound out");
        assert!(matches!(err, ServerError::Timeout), "got {err:?}");

        parked.release();
        let _ = running.await;
        pool.exclusive(Duration::from_secs(2))
            .await
            .expect("drained pool becomes exclusive");
    }

    #[tokio::test(start_paused = true)]
    async fn startup_wait_counts_against_the_deadline() {
        let pool = hot_pool(1);
        let seams = pool.seams();
        let entry = seams.hold(Seam::Entry);
        let started = seams.watch(Seam::Started);
        let baseline_idle = idle_len(&pool);
        let id = pool.allocate_query_id();
        let logged = capture::of_query(&pool, id).await;

        let submitted = pool.clone();
        let request = tokio::spawn(async move {
            submitted
                .execute(
                    id,
                    "*",
                    Deadline::after(Duration::from_secs(5)),
                    false,
                    TEST_WORK,
                )
                .await
        });

        until("the worker parks at its first statement", || {
            entry.arrivals() == 1
        })
        .await;
        assert_eq!(
            pool.available_permits(),
            0,
            "a worker that has not started still holds its permit"
        );

        tokio::time::advance(Duration::from_secs(6)).await;
        let outcome = request.await.expect("the request joins");
        assert!(
            matches!(&outcome.result, Err(ServerError::CapacityNotStarted)),
            "expected a capacity refusal, got {:?}",
            outcome.result
        );
        assert_eq!(pool.retained(), 1, "the permit outlives the request");
        assert_eq!(pool.available_permits(), 0, "and is not back in the pool");

        entry.release();
        until("cleanup returns everything", || {
            pool.retained() == 0
                && idle_len(&pool) == baseline_idle
                && pool.available_permits() == 1
        })
        .await;
        assert_eq!(
            started.arrivals(),
            0,
            "an expired worker performs no physical work at all"
        );
        assert!(registry_is_empty(&pool));

        let refusals = logged.of_type("query_not_started");
        assert_eq!(
            refusals.len(),
            1,
            "a refused request must log exactly once, got {refusals:?}"
        );
        assert_eq!(
            refusals[0].get("reason").map(String::as_str),
            Some("expired")
        );
        assert_eq!(refusals[0].get("kind").map(String::as_str), Some("query"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_retained_permit_is_visible_and_reclaimed() {
        let pool = hot_pool(2);
        let seams = pool.seams();
        let started = seams.hold(Seam::Started);
        let baseline_idle = idle_len(&pool);
        let id = pool.allocate_query_id();

        let submitted = pool.clone();
        let request = tokio::spawn(async move {
            submitted
                .execute(
                    id,
                    "*",
                    Deadline::after(Duration::from_secs(5)),
                    false,
                    TEST_WORK,
                )
                .await
        });

        until("work starts", || started.arrivals() == 1).await;
        assert_eq!(
            pool.retained(),
            0,
            "work a request is still waiting on is not retained"
        );

        tokio::time::advance(Duration::from_secs(6)).await;
        let outcome = request.await.expect("the request joins");
        assert!(
            matches!(outcome.result, Err(ServerError::Timeout)),
            "work that started and ran out of budget is a timeout, got {:?}",
            outcome.result
        );

        assert_eq!(pool.retained(), 1);
        assert!(
            pool.retained() <= pool.capacity() - pool.available_permits(),
            "retained work is a subset of the permits actually held"
        );
        let snapshot = pool.retained_work();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].id, id);
        assert_eq!(snapshot[0].kind, WorkKind::Query);
        assert!(snapshot[0].started, "this work did start");
        assert_eq!(snapshot[0].owner, WorkOwner::Key(7));

        assert!(pool.cancel_by_id(id));
        assert!(pool.cancel_by_id(id), "cancellation is repeatable");
        assert_eq!(pool.owner_of(id), Some(WorkOwner::Key(7)));

        started.release();
        until("the permit comes back", || {
            pool.retained() == 0
                && idle_len(&pool) == baseline_idle
                && pool.available_permits() == pool.capacity()
        })
        .await;
        assert!(pool.retained_work().is_empty());
        assert!(registry_is_empty(&pool));
        assert!(
            !pool.cancel_by_id(id),
            "work that has ended is no longer cancellable"
        );
        assert_eq!(pool.owner_of(id), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_timed_out_query_stays_cancellable() {
        let pool = hot_pool(1);
        let seams = pool.seams();
        let started = seams.hold(Seam::Started);
        let id = pool.allocate_query_id();

        let submitted = pool.clone();
        let request = tokio::spawn(async move {
            submitted
                .execute(
                    id,
                    "*",
                    Deadline::after(Duration::from_secs(5)),
                    false,
                    TEST_WORK,
                )
                .await
        });
        until("work starts", || started.arrivals() == 1).await;
        tokio::time::advance(Duration::from_secs(6)).await;
        let outcome = request.await.expect("the request joins");
        assert!(matches!(outcome.result, Err(ServerError::Timeout)));

        assert!(pool.cancel_by_id(id), "still reachable after the timeout");
        assert!(pool.cancel_by_id(id), "and still reachable after that");
        started.release();
        until("cleanup", || pool.retained() == 0).await;
        assert!(!pool.cancel_by_id(id));
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_cannot_interrupt_a_reused_executor() {
        let pool = hot_pool(1);
        let seams = pool.seams();
        // One executor, so the second query provably reuses the first's.

        // Order one: the cancellation arrives while the finished work is
        // still holding its slot.
        let finished = seams.hold(Seam::Finished);
        let first = pool.allocate_query_id();
        let submitted = pool.clone();
        let request = tokio::spawn(async move {
            submitted
                .execute(
                    first,
                    "*",
                    Deadline::after(Duration::from_secs(60)),
                    false,
                    TEST_WORK,
                )
                .await
        });
        until("the first query finishes its work", || {
            finished.arrivals() == 1
        })
        .await;
        assert!(pool.cancel_by_id(first), "its slot is still registered");
        finished.release();
        let outcome = request.await.expect("the request joins");
        assert!(outcome.result.is_ok(), "{:?}", outcome.result);
        until("the executor is back", || pool.available_permits() == 1).await;

        assert!(!pool.cancel_by_id(first), "the old slot is gone");
        let second = pool.allocate_query_id();
        let outcome = pool
            .execute(
                second,
                "*",
                Deadline::after(Duration::from_secs(60)),
                false,
                TEST_WORK,
            )
            .await;
        assert!(
            outcome.result.is_ok(),
            "a stale cancellation must not disturb the reused connection: {:?}",
            outcome.result
        );

        // Order two: the cancellation arrives while the reused executor is
        // mid-query. Naming the old id must still reach nothing.
        let started = seams.hold(Seam::Started);
        let third = pool.allocate_query_id();
        let submitted = pool.clone();
        let request = tokio::spawn(async move {
            submitted
                .execute(
                    third,
                    "*",
                    Deadline::after(Duration::from_secs(60)),
                    false,
                    TEST_WORK,
                )
                .await
        });
        until("the third query starts", || started.arrivals() == 1).await;
        assert!(!pool.cancel_by_id(first));
        assert!(!pool.cancel_by_id(second));
        started.release();
        let outcome = request.await.expect("the request joins");
        assert!(outcome.result.is_ok(), "{:?}", outcome.result);
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_before_the_handle_exists_refuses_the_start() {
        let pool = hot_pool(1);
        let seams = pool.seams();
        let entry = seams.hold(Seam::Entry);
        let started = seams.watch(Seam::Started);
        let baseline_idle = idle_len(&pool);
        let id = pool.allocate_query_id();

        let submitted = pool.clone();
        let request = tokio::spawn(async move {
            submitted
                .execute(
                    id,
                    "*",
                    Deadline::after(Duration::from_secs(60)),
                    false,
                    TEST_WORK,
                )
                .await
        });
        until("the worker parks before publishing a handle", || {
            entry.arrivals() == 1
        })
        .await;
        assert!(
            pool.cancel_by_id(id),
            "the slot exists from registration, handle or not"
        );

        entry.release();
        let outcome = request.await.expect("the request joins");
        assert!(
            matches!(&outcome.result, Err(ServerError::CapacityNotStarted)),
            "expected a refusal, got {:?}",
            outcome.result
        );
        assert_eq!(
            started.arrivals(),
            0,
            "a cancelled worker runs no physical query"
        );
        until("cleanup", || {
            idle_len(&pool) == baseline_idle && pool.available_permits() == 1
        })
        .await;
        assert!(registry_is_empty(&pool));
    }

    fn far() -> Deadline {
        Deadline::after(Duration::from_secs(60))
    }

    /// Which of the registry's two maps hold `id`: (pending, registered).
    fn maps(pool: &ExecutorPool, id: u64) -> (bool, bool) {
        let registry = pool.registry.lock();
        (
            registry.pending.contains_key(&id),
            registry.interrupts.contains_key(&id),
        )
    }

    /// Take a permit and promote `id` into a slot, as a lane does once its
    /// last wait is over: there is no seam between that wait and
    /// `begin_slot`, so the tests drive the promotion directly.
    async fn promote(pool: &ExecutorPool, id: u64) -> WorkSlot {
        let permit = Arc::clone(&pool.semaphore)
            .acquire_owned()
            .await
            .expect("the semaphore is never closed");
        pool.begin_slot(id, TEST_WORK, Some("*"), permit, None)
            .expect("an idle executor is behind the permit")
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancel_reaches_a_query_queued_for_a_permit() {
        let pool = ExecutorPool::for_test("/nonexistent".into(), 1, 100_000, None).unwrap();
        let seams = pool.seams();
        let entry = seams.watch(Seam::Entry);
        let idle_before = idle_len(&pool);
        let held = pool
            .exclusive(Duration::from_secs(1))
            .await
            .expect("the idle pool grants exclusivity at once");
        let id = pool.allocate_query_id();
        let (timing, request_id) = timed(&pool, id);
        let log = capture::of_timed(&pool, id, &request_id).await;
        let pending = pool.pend(id, WorkKind::Query);
        assert_eq!(maps(&pool, id), (true, false), "pending, not registered");

        let deadline = far();
        let start = tokio::time::Instant::now();
        let submitted = pool.clone();
        let request = tokio::spawn(async move {
            submitted
                .execute(id, "*", deadline, false, TEST_WORK.with_timing(timing))
                .await
        });
        settle().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(!request.is_finished(), "the query is queued for the permit");

        assert!(pool.cancel_by_id(id), "a queued query is cancellable by id");
        let outcome = request.await.expect("the request joins");
        assert!(
            matches!(&outcome.result, Err(ServerError::CapacityNotStarted)),
            "a cancelled wait is a capacity refusal: {:?}",
            outcome.result
        );
        assert_eq!(
            start.elapsed(),
            Duration::from_secs(1),
            "the cancel ended the wait, not the deadline"
        );
        assert!(!deadline.expired());
        assert_eq!(
            pool.available_permits(),
            0,
            "the blocker still holds the pool's permit"
        );

        let refusals = log.of_type("query_not_started");
        assert_eq!(refusals.len(), 1, "one refusal: {refusals:?}");
        assert_eq!(value(&refusals[0], "reason"), Some("cancelled"));
        assert_eq!(value(&refusals[0], "kind"), Some("query"));
        let timings = log.of_type("query_timing");
        assert_eq!(timings.len(), 1, "one account: {timings:?}");
        assert_eq!(value(&timings[0], "outcome"), Some("capacity_refused"));
        assert_eq!(value(&timings[0], "work_started"), Some("false"));
        drop(log);

        drop(held);
        settle().await;
        assert_eq!(
            entry.arrivals(),
            0,
            "the cancelled query never reached the worker, even once the permit freed"
        );
        assert_eq!(idle_len(&pool), idle_before, "it took no executor");
        assert_eq!(pool.available_permits(), pool.capacity());
        assert!(
            pool.cancel_by_id(id),
            "the request still holds its guard, so a repeat cancel answers true"
        );
        drop(pending);
        assert!(!pool.cancel_by_id(id));
        assert!(registry_is_empty(&pool));
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancel_during_the_publication_wait_returns_the_permit() {
        let pool = ExecutorPool::for_test("/nonexistent".into(), 1, 100_000, None).unwrap();
        let seams = pool.seams();
        let entry = seams.watch(Seam::Entry);
        let idle_before = idle_len(&pool);
        let writer = pool.publication.write().await;
        let id = pool.allocate_query_id();
        let pending = pool.pend(id, WorkKind::Query);

        let deadline = far();
        let start = tokio::time::Instant::now();
        let submitted = pool.clone();
        let request =
            tokio::spawn(
                async move { submitted.execute(id, "*", deadline, false, TEST_WORK).await },
            );
        settle().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(!request.is_finished(), "the query waits for the guard");
        assert_eq!(
            pool.available_permits(),
            0,
            "it holds the permit while it waits"
        );

        assert!(pool.cancel_by_id(id));
        let outcome = request.await.expect("the request joins");
        assert!(
            matches!(&outcome.result, Err(ServerError::CapacityNotStarted)),
            "{:?}",
            outcome.result
        );
        assert_eq!(start.elapsed(), Duration::from_secs(1));
        assert!(!deadline.expired());
        assert_eq!(
            pool.available_permits(),
            pool.capacity(),
            "the permit is back while the writer still holds the guard"
        );
        assert_eq!(idle_len(&pool), idle_before);

        drop(writer);
        settle().await;
        assert_eq!(entry.arrivals(), 0, "the worker never saw the query");
        drop(pending);
        assert!(registry_is_empty(&pool));
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancel_before_the_wait_is_seen_by_the_wait() {
        let pool = hot_pool(1);
        let seams = pool.seams();
        let entry = seams.watch(Seam::Entry);
        let idle_before = idle_len(&pool);
        let id = pool.allocate_query_id();
        let pending = pool.pend(id, WorkKind::Query);
        assert!(pool.cancel_by_id(id), "a pending request is cancellable");
        assert!(pool.cancel_by_id(id), "and a repeat cancel answers true");
        assert!(pending.latched());

        let start = tokio::time::Instant::now();
        let deadline = far();
        let outcome = pool.execute(id, "*", deadline, false, TEST_WORK).await;
        assert!(
            matches!(&outcome.result, Err(ServerError::CapacityNotStarted)),
            "{:?}",
            outcome.result
        );
        assert_eq!(
            start.elapsed(),
            Duration::ZERO,
            "the first wait ended at once"
        );
        assert_eq!(
            entry.arrivals(),
            0,
            "the wait refused it: the worker never ran to refuse it"
        );
        assert_eq!(pool.available_permits(), pool.capacity());
        assert_eq!(idle_len(&pool), idle_before);
        drop(pending);
        assert!(registry_is_empty(&pool));
    }

    #[tokio::test(start_paused = true)]
    async fn a_pending_guard_stops_and_refuses_on_its_cancel() {
        let pool = ExecutorPool::for_test("/nonexistent".into(), 1, 100_000, None).unwrap();
        let id = pool.allocate_query_id();
        let log = capture::of_query(&pool, id).await;
        let pending = pool.pend(id, WorkKind::Export);
        let stop = pending.stop();
        assert!(!stop.requested());
        assert!(pool.cancel_by_id(id));
        assert!(stop.requested(), "the stop reads the pending flag");
        assert!(pending.stop().requested());
        for _ in 0..2 {
            assert!(matches!(pending.refuse(), ServerError::CapacityNotStarted));
        }
        let refusals = log.of_type("query_not_started");
        assert_eq!(refusals.len(), 1, "told once: {refusals:?}");
        assert_eq!(value(&refusals[0], "reason"), Some("cancelled"));
        assert_eq!(value(&refusals[0], "kind"), Some("export"));
        drop(log);
        drop(pending);
        assert!(registry_is_empty(&pool));
    }

    #[tokio::test(start_paused = true)]
    async fn a_pending_cancel_survives_promotion() {
        let pool = hot_pool(1);

        let id = pool.allocate_query_id();
        let pending = pool.pend(id, WorkKind::Query);
        assert_eq!(maps(&pool, id), (true, false));
        assert!(pool.cancel_by_id(id));
        let slot = promote(&pool, id).await;
        assert_eq!(
            maps(&pool, id),
            (false, true),
            "promotion moves the id in one critical section"
        );
        assert!(
            Arc::ptr_eq(
                &pool.registry.lock().interrupts[&id].cancelled,
                &pending.cancelled
            ),
            "the flag keeps its identity across promotion"
        );
        assert!(
            !slot.publish_interrupt(slot.executor().interrupt_handle()),
            "the worker finds the cancel latched"
        );
        assert_eq!(slot.begin_work(far()), Err(StartRefusal::Cancelled));
        assert!(pool.cancel_by_id(id), "a repeat cancel reaches the slot");
        drop(pending);
        assert_eq!(
            maps(&pool, id),
            (false, true),
            "the request's guard leaves the registered slot alone"
        );
        drop(slot);
        assert_eq!(maps(&pool, id), (false, false));
        assert!(registry_is_empty(&pool));

        let id = pool.allocate_query_id();
        let pending = pool.pend(id, WorkKind::Query);
        let slot = promote(&pool, id).await;
        assert_eq!(maps(&pool, id), (false, true));
        assert!(!pending.latched());
        assert!(pool.cancel_by_id(id));
        assert!(
            pending.latched(),
            "the guard reads the flag the registered slot latched"
        );
        assert!(!slot.publish_interrupt(slot.executor().interrupt_handle()));
        drop(slot);
        assert_eq!(maps(&pool, id), (false, false), "worker cleanup first");
        drop(pending);
        assert!(registry_is_empty(&pool));

        let outcome = pool
            .execute(pool.allocate_query_id(), "*", far(), false, TEST_WORK)
            .await;
        assert!(
            outcome.result.is_ok(),
            "no cancel reaches the reused executor: {:?}",
            outcome.result
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_pending_entry_leaves_with_its_request() {
        let pool = hot_pool(1);

        let id = pool.allocate_query_id();
        let pending = pool.pend(id, WorkKind::Query);
        assert_eq!(maps(&pool, id), (true, false));
        drop(pending);
        assert_eq!(maps(&pool, id), (false, false));
        assert!(!pool.cancel_by_id(id), "nothing is left to cancel");
        assert!(registry_is_empty(&pool));

        let held = pool
            .exclusive(Duration::from_secs(1))
            .await
            .expect("the idle pool grants exclusivity at once");
        let id = pool.allocate_query_id();
        let pending = pool.pend(id, WorkKind::Query);
        let outcome = pool
            .execute(
                id,
                "*",
                Deadline::after(Duration::from_secs(5)),
                false,
                TEST_WORK,
            )
            .await;
        assert!(
            matches!(&outcome.result, Err(ServerError::CapacityNotStarted)),
            "{:?}",
            outcome.result
        );
        assert_eq!(
            maps(&pool, id),
            (true, false),
            "the wait ended and the request has not"
        );
        assert!(pool.cancel_by_id(id));
        drop(pending);
        assert!(!pool.cancel_by_id(id));
        drop(held);

        let id = pool.allocate_query_id();
        let pending = pool.pend(id, WorkKind::Query);
        let slot = promote(&pool, id).await;
        drop(pending);
        assert_eq!(maps(&pool, id), (false, true));
        assert!(
            pool.cancel_by_id(id),
            "a guard dropped after promotion leaves the work cancellable"
        );
        drop(slot);
        assert!(!pool.cancel_by_id(id));
        assert!(registry_is_empty(&pool));
    }

    #[tokio::test(start_paused = true)]
    async fn a_closed_wake_is_not_a_cancel() {
        let pool = hot_pool(1);
        let soon = || Deadline::after(Duration::from_secs(1));

        let id = pool.allocate_query_id();
        let pending = pool.pend(id, WorkKind::Query);
        let wake = pending.wake().clone();
        let slot = promote(&pool, id).await;
        assert_eq!(
            wake.run(soon(), std::future::pending::<()>()).await,
            Err(StartRefusal::Expired)
        );
        drop(slot);
        drop(pending);
        assert_eq!(
            wake.run(soon(), std::future::pending::<()>()).await,
            Err(StartRefusal::Expired)
        );

        let id = pool.allocate_query_id();
        let pending = pool.pend(id, WorkKind::Query);
        let wake = pending.wake().clone();
        assert!(pool.cancel_by_id(id));
        let slot = promote(&pool, id).await;
        assert_eq!(
            wake.run(far(), std::future::pending::<()>()).await,
            Err(StartRefusal::Cancelled)
        );
        drop(slot);
        drop(pending);

        assert_eq!(
            Wake::default()
                .run(soon(), std::future::pending::<()>())
                .await,
            Err(StartRefusal::Expired)
        );
        assert!(registry_is_empty(&pool));
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancel_reaches_a_resolved_source_query_queued_for_a_permit() {
        let pool = ExecutorPool::for_test("/nonexistent".into(), 1, 100_000, None).unwrap();
        let seams = pool.seams();
        let entry = seams.watch(Seam::Entry);
        let held = pool
            .exclusive(Duration::from_secs(1))
            .await
            .expect("the idle pool grants exclusivity at once");
        let id = pool.allocate_query_id();
        let pending = pool.pend(id, WorkKind::Query);

        let deadline = far();
        let start = tokio::time::Instant::now();
        let submitted = pool.clone();
        let request = tokio::spawn(async move {
            submitted
                .execute_with_source(
                    id,
                    "*",
                    "'/nonexistent/saved.parquet'",
                    deadline,
                    false,
                    WorkContext::key(WorkKind::FromSaved, 7),
                )
                .await
        });
        settle().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(!request.is_finished(), "queued for the permit");
        assert!(pool.cancel_by_id(id));
        let outcome = request.await.expect("the request joins");
        assert!(
            matches!(&outcome.result, Err(ServerError::CapacityNotStarted)),
            "{:?}",
            outcome.result
        );
        assert_eq!(start.elapsed(), Duration::from_secs(1));
        assert!(!deadline.expired());

        drop(held);
        settle().await;
        assert_eq!(entry.arrivals(), 0);
        assert_eq!(pool.available_permits(), pool.capacity());
        drop(pending);
        assert!(registry_is_empty(&pool));
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancel_reaches_a_parquet_export_at_either_wait() {
        let pool = ExecutorPool::for_test("/nonexistent".into(), 1, 100_000, None).unwrap();
        let seams = pool.seams();
        let entry = seams.watch(Seam::Entry);
        let export = |id: u64, deadline: Deadline| {
            let submitted = pool.clone();
            tokio::spawn(async move {
                submitted
                    .export_parquet(
                        id,
                        "*",
                        100,
                        deadline,
                        WorkContext::key(WorkKind::Export, 7),
                    )
                    .await
            })
        };

        let held = pool
            .exclusive(Duration::from_secs(1))
            .await
            .expect("the idle pool grants exclusivity at once");
        let id = pool.allocate_query_id();
        let pending = pool.pend(id, WorkKind::Export);
        let deadline = far();
        let start = tokio::time::Instant::now();
        let request = export(id, deadline);
        settle().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(!request.is_finished(), "queued for the permit");
        assert!(pool.cancel_by_id(id));
        let answer = request.await.expect("the request joins");
        assert!(
            matches!(&answer, Err(ServerError::CapacityNotStarted)),
            "{answer:?}"
        );
        assert_eq!(start.elapsed(), Duration::from_secs(1));
        assert!(!deadline.expired());
        drop(held);
        drop(pending);

        let writer = pool.publication.write().await;
        let id = pool.allocate_query_id();
        let pending = pool.pend(id, WorkKind::Export);
        let deadline = far();
        let start = tokio::time::Instant::now();
        let request = export(id, deadline);
        settle().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(!request.is_finished(), "waiting for the guard");
        assert_eq!(pool.available_permits(), 0, "holding the permit");
        assert!(pool.cancel_by_id(id));
        let answer = request.await.expect("the request joins");
        assert!(
            matches!(&answer, Err(ServerError::CapacityNotStarted)),
            "{answer:?}"
        );
        assert_eq!(start.elapsed(), Duration::from_secs(1));
        assert!(!deadline.expired());
        assert_eq!(
            pool.available_permits(),
            pool.capacity(),
            "the permit is back while the writer still holds the guard"
        );
        drop(writer);
        drop(pending);

        settle().await;
        assert_eq!(entry.arrivals(), 0, "neither export reached the worker");
        assert!(registry_is_empty(&pool));
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancel_reaches_a_window_capture_at_either_wait() {
        let pool = ExecutorPool::for_test("/nonexistent".into(), 1, 100_000, None).unwrap();
        let seams = pool.seams();
        let entry = seams.watch(Seam::Entry);
        let finished = Arc::new(AtomicBool::new(false));
        let capture = |id: u64, deadline: Deadline, work: WorkContext| {
            let submitted = pool.clone();
            let finished = Arc::clone(&finished);
            tokio::spawn(async move {
                submitted
                    .capture(
                        id,
                        "*",
                        window(),
                        RowCap::Refuse(100),
                        deadline,
                        work,
                        Box::new(move |_: CaptureInput<'_>| {
                            finished.store(true, Ordering::SeqCst);
                            Ok(())
                        }),
                    )
                    .await
            })
        };

        let held = pool
            .exclusive(Duration::from_secs(1))
            .await
            .expect("the idle pool grants exclusivity at once");
        let (work, released) = charged(&pool, 7);
        let id = pool.allocate_query_id();
        let pending = pool.pend(id, WorkKind::Capture);
        let deadline = far();
        let start = tokio::time::Instant::now();
        let request = capture(id, deadline, work);
        settle().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(!request.is_finished(), "queued for the permit");
        assert!(
            released.lock().is_none(),
            "the waiting capture holds its charge"
        );
        assert!(pool.cancel_by_id(id));
        let answer = request.await.expect("the request joins");
        assert!(
            matches!(&answer, Err(ServerError::CapacityNotStarted)),
            "{answer:?}"
        );
        assert_eq!(start.elapsed(), Duration::from_secs(1));
        assert!(!deadline.expired());
        assert!(released.lock().is_some(), "the refusal dropped the charge");
        drop(held);
        drop(pending);

        let writer = pool.publication.write().await;
        let (work, released) = charged(&pool, 7);
        let id = pool.allocate_query_id();
        let pending = pool.pend(id, WorkKind::Capture);
        let deadline = far();
        let start = tokio::time::Instant::now();
        let request = capture(id, deadline, work);
        settle().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(!request.is_finished(), "waiting for the window's guard");
        assert_eq!(pool.available_permits(), 0, "holding the permit");
        assert!(pool.cancel_by_id(id));
        let answer = request.await.expect("the request joins");
        assert!(
            matches!(&answer, Err(ServerError::CapacityNotStarted)),
            "{answer:?}"
        );
        assert_eq!(start.elapsed(), Duration::from_secs(1));
        assert!(!deadline.expired());
        let released = released.lock().expect("the refusal dropped the charge");
        assert_eq!(
            released.available_permits,
            pool.capacity(),
            "the permit was back before the charge dropped"
        );
        assert_eq!(released.registered, 0);
        drop(writer);
        drop(pending);

        settle().await;
        assert_eq!(entry.arrivals(), 0, "neither capture reached the worker");
        assert!(!finished.load(Ordering::SeqCst), "finish never ran");
        assert!(registry_is_empty(&pool));
    }

    /// There is no seam between the bind and the execution, so the
    /// cancellation is latched with the worker parked at `Started`, one
    /// statement earlier. The engine reads the same flag either way.
    #[tokio::test(start_paused = true)]
    async fn a_latched_cancellation_stops_the_query_at_the_bind_boundary() {
        let pool = hot_pool(1);
        let seams = pool.seams();
        let started = seams.hold(Seam::Started);
        let baseline_idle = idle_len(&pool);
        let id = pool.allocate_query_id();

        let submitted = pool.clone();
        let request = tokio::spawn(async move {
            submitted
                .execute(
                    id,
                    "*",
                    Deadline::after(Duration::from_secs(60)),
                    false,
                    TEST_WORK,
                )
                .await
        });
        until("work starts", || started.arrivals() == 1).await;
        assert!(pool.cancel_by_id(id), "the work is still registered");
        started.release();

        let outcome = request.await.expect("the request joins");
        assert!(
            matches!(
                &outcome.result,
                Err(ServerError::Engine(
                    trawl_engine::error::EngineError::Cancelled
                ))
            ),
            "expected the engine's cancellation, got {:?}",
            outcome.result
        );
        assert_eq!(
            ServerError::Engine(trawl_engine::error::EngineError::Cancelled).error_class(),
            "cancelled",
            "a cancelled run is classified as such, not as a database failure"
        );

        until("cleanup", || {
            pool.retained() == 0
                && idle_len(&pool) == baseline_idle
                && pool.available_permits() == pool.capacity()
        })
        .await;
        assert!(registry_is_empty(&pool));
    }

    #[tokio::test(start_paused = true)]
    async fn start_and_expiry_race_yields_exactly_one_classification() {
        let pool = hot_pool(2);
        let seams = pool.seams();

        let entry = seams.hold(Seam::Entry);
        let id = pool.allocate_query_id();
        let submitted = pool.clone();
        let request = tokio::spawn(async move {
            submitted
                .execute(
                    id,
                    "*",
                    Deadline::after(Duration::from_secs(5)),
                    false,
                    TEST_WORK,
                )
                .await
        });
        until("parked", || entry.arrivals() == 1).await;
        tokio::time::advance(Duration::from_secs(6)).await;
        let expiry_first = request.await.expect("the request joins").result;
        entry.release();
        until("cleanup", || pool.retained() == 0).await;

        let started = seams.hold(Seam::Started);
        let id = pool.allocate_query_id();
        let submitted = pool.clone();
        let request = tokio::spawn(async move {
            submitted
                .execute(
                    id,
                    "*",
                    Deadline::after(Duration::from_secs(5)),
                    false,
                    TEST_WORK,
                )
                .await
        });
        until("started", || started.arrivals() == 1).await;
        tokio::time::advance(Duration::from_secs(6)).await;
        let start_first = request.await.expect("the request joins").result;
        started.release();
        until("cleanup", || pool.retained() == 0).await;

        assert!(
            matches!(&expiry_first, Err(ServerError::CapacityNotStarted)),
            "expiry before the start is a capacity refusal, got {expiry_first:?}"
        );
        assert!(
            matches!(start_first, Err(ServerError::Timeout)),
            "expiry after the start is a timeout, got {start_first:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_dropped_caller_retains_and_then_reclaims() {
        let pool = hot_pool(1);
        let seams = pool.seams();
        let started = seams.hold(Seam::Started);
        let baseline_idle = idle_len(&pool);
        let id = pool.allocate_query_id();

        let submitted = pool.clone();
        let request = tokio::spawn(async move {
            submitted
                .execute(
                    id,
                    "*",
                    Deadline::after(Duration::from_secs(600)),
                    false,
                    TEST_WORK,
                )
                .await
        });
        until("work starts", || started.arrivals() == 1).await;

        request.abort();
        assert!(
            request.await.unwrap_err().is_cancelled(),
            "the requesting task is gone"
        );
        until("the abandoned work books its permit", || {
            pool.retained() == 1
        })
        .await;
        assert_eq!(pool.available_permits(), 0, "the permit is still held");
        assert_eq!(pool.retained_work().len(), 1);

        started.release();
        until("cleanup", || {
            pool.retained() == 0
                && idle_len(&pool) == baseline_idle
                && pool.available_permits() == 1
        })
        .await;
        assert!(registry_is_empty(&pool));
    }

    #[tokio::test]
    async fn a_panicking_worker_leaks_no_permit_or_executor() {
        let pool = hot_pool(2);
        let seams = pool.seams();
        let baseline_idle = idle_len(&pool);

        for seam_point in [Seam::Entry, Seam::Started, Seam::Finished] {
            let _gate = seams.panic_at(seam_point);
            let id = pool.allocate_query_id();
            let outcome = pool
                .execute(
                    id,
                    "*",
                    Deadline::after(Duration::from_secs(60)),
                    false,
                    TEST_WORK,
                )
                .await;
            assert!(
                matches!(outcome.result, Err(ServerError::Panicked(_))),
                "a panic at {seam_point:?} reports a panic, got {:?}",
                outcome.result
            );
            until("cleanup after the panic", || {
                idle_len(&pool) == baseline_idle && pool.available_permits() == pool.capacity()
            })
            .await;
            assert!(
                registry_is_empty(&pool),
                "a panic at {seam_point:?} left a registration behind"
            );
            assert_eq!(pool.retained(), 0);
        }
    }

    #[tokio::test]
    async fn a_pool_panic_answers_panicked_without_its_payload() {
        use axum::response::IntoResponse;
        use std::io::Write;

        #[derive(Clone, Default)]
        struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);
        impl Write for Captured {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0
                    .lock()
                    .expect("capture poisoned")
                    .extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let pool = hot_pool(1);
        let seams = pool.seams();
        let _gate = seams.panic_at(Seam::Started);
        let id = pool.allocate_query_id();
        let err = pool
            .execute(
                id,
                "*",
                Deadline::after(Duration::from_secs(60)),
                false,
                TEST_WORK,
            )
            .await
            .result
            .expect_err("the worker panics");

        assert!(matches!(err, ServerError::Panicked(_)), "got {err:?}");
        assert_eq!(err.error_class(), "panic");
        let sentinel = seam::INJECTED_PANIC_PAYLOAD;
        for rendered in [err.to_string(), format!("{err:?}"), err.safe_message()] {
            assert!(!rendered.contains(sentinel), "payload leaked: {rendered}");
        }

        let log = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer({
                let log = log.clone();
                move || log.clone()
            })
            .finish();
        let response = {
            let _default = tracing::subscriber::set_default(subscriber);
            err.into_response()
        };
        assert_eq!(
            response.status(),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        );
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("the body reads");
        let body = String::from_utf8_lossy(&bytes);
        assert!(body.contains("internal server error"), "got: {body}");
        assert!(!body.contains(sentinel), "payload leaked: {body}");

        let log =
            String::from_utf8(log.0.lock().expect("capture poisoned").clone()).expect("utf-8 log");
        assert!(log.contains("panic"), "the response logs the panic: {log}");
        assert!(!log.contains(sentinel), "payload leaked: {log}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_timeout_racing_completion_leaves_no_orphan() {
        let pool = hot_pool(1);
        let seams = pool.seams();
        let finished = seams.hold(Seam::Finished);
        let baseline_idle = idle_len(&pool);
        let id = pool.allocate_query_id();

        let submitted = pool.clone();
        let request = tokio::spawn(async move {
            submitted
                .execute(
                    id,
                    "*",
                    Deadline::after(Duration::from_secs(5)),
                    false,
                    TEST_WORK,
                )
                .await
        });
        until("the answer is in hand", || finished.arrivals() == 1).await;
        tokio::time::advance(Duration::from_secs(6)).await;
        let outcome = request.await.expect("the request joins");
        assert!(
            matches!(outcome.result, Err(ServerError::Timeout)),
            "the request that stopped waiting reports the timeout it saw"
        );
        assert_eq!(pool.retained(), 1);

        finished.release();
        until("cleanup", || {
            pool.retained() == 0
                && idle_len(&pool) == baseline_idle
                && pool.available_permits() == 1
        })
        .await;
        assert!(registry_is_empty(&pool));
    }

    #[tokio::test(start_paused = true)]
    async fn every_lane_registers_its_kind_and_owner() {
        let pool = hot_pool(3);
        let seams = pool.seams();
        let started = seams.hold(Seam::Started);

        let export_id = pool.allocate_query_id();
        let exporter = pool.clone();
        let export = tokio::spawn(async move {
            exporter
                .export_parquet(
                    export_id,
                    "*",
                    10,
                    Deadline::after(Duration::from_secs(5)),
                    WorkContext::key(WorkKind::Export, 11),
                )
                .await
        });
        let scheduled_id = pool.allocate_query_id();
        let reporting = pool.clone();
        let scheduled = tokio::spawn(async move {
            reporting
                .execute(
                    scheduled_id,
                    "*",
                    Deadline::after(Duration::from_secs(5)),
                    false,
                    WorkContext::system(WorkKind::Scheduled),
                )
                .await
        });
        let capture_id = pool.allocate_query_id();
        let capturer = pool.clone();
        let capture = tokio::spawn(async move {
            capturer
                .capture(
                    capture_id,
                    "*",
                    window(),
                    RowCap::Refuse(100),
                    Deadline::after(Duration::from_secs(5)),
                    WorkContext::key(WorkKind::Capture, 13),
                    rows_of(),
                )
                .await
        });

        until("every lane starts", || started.arrivals() == 3).await;
        tokio::time::advance(Duration::from_secs(6)).await;
        assert!(matches!(
            export.await.expect("the export joins"),
            Err(ServerError::Timeout)
        ));
        assert!(matches!(
            scheduled.await.expect("the run joins").result,
            Err(ServerError::Timeout)
        ));
        assert!(matches!(
            capture.await.expect("the capture joins"),
            Err(ServerError::Timeout)
        ));

        let mut retained = pool.retained_work();
        retained.sort_by_key(|work| work.id);
        assert_eq!(retained.len(), 3);
        assert_eq!(retained[0].id, export_id);
        assert_eq!(retained[0].kind, WorkKind::Export);
        assert_eq!(retained[0].owner, WorkOwner::Key(11));
        assert_eq!(retained[1].id, scheduled_id);
        assert_eq!(retained[1].kind, WorkKind::Scheduled);
        assert_eq!(
            retained[1].owner,
            WorkOwner::System,
            "a scheduled run has no human owner"
        );
        assert_eq!(retained[2].id, capture_id);
        assert_eq!(retained[2].kind, WorkKind::Capture);
        assert_eq!(retained[2].kind.as_str(), "capture");
        assert_eq!(retained[2].owner, WorkOwner::Key(13));
        assert_eq!(
            retained[2].display.as_deref(),
            Some("*"),
            "a capture shows its DSL like any query"
        );

        started.release();
        until("cleanup", || pool.retained() == 0).await;
    }

    #[tokio::test]
    async fn counts_return_to_baseline() {
        let pool = hot_pool(2);
        let baseline_idle = idle_len(&pool);
        let baseline_permits = pool.available_permits();

        for _ in 0..3 {
            let outcome = pool
                .execute(
                    pool.allocate_query_id(),
                    "*",
                    Deadline::after(Duration::from_secs(60)),
                    false,
                    TEST_WORK,
                )
                .await;
            assert!(outcome.result.is_ok(), "{:?}", outcome.result);
        }
        let _ = pool
            .export_parquet(
                pool.allocate_query_id(),
                "*",
                10,
                Deadline::after(Duration::from_secs(60)),
                WorkContext::key(WorkKind::Export, 7),
            )
            .await;
        let _ = pool
            .sample_field_values(
                "service",
                None,
                10,
                Deadline::after(Duration::from_secs(60)),
                TEST_SAMPLE_WORK,
            )
            .await;
        let captured = pool
            .capture(
                pool.allocate_query_id(),
                "*",
                window(),
                RowCap::Refuse(100),
                Deadline::after(Duration::from_secs(60)),
                WorkContext::key(WorkKind::Capture, 7),
                rows_of(),
            )
            .await;
        assert_eq!(captured.expect("the capture answers"), 1, "the hot row");

        assert_eq!(idle_len(&pool), baseline_idle);
        assert_eq!(pool.available_permits(), baseline_permits);
        assert_eq!(pool.retained(), 0);
        assert!(pool.retained_work().is_empty());
        assert!(registry_is_empty(&pool));
    }

    #[tokio::test(start_paused = true)]
    async fn sampling_bounds_its_wait() {
        let pool = hot_pool(1);
        let seams = pool.seams();
        let parked = seams.hold(Seam::Started);
        let baseline_idle = idle_len(&pool);

        let occupant = pool.clone();
        let occupied = tokio::spawn(async move {
            occupant
                .execute(
                    occupant.allocate_query_id(),
                    "*",
                    Deadline::after(Duration::from_secs(600)),
                    false,
                    TEST_WORK,
                )
                .await
        });
        until("the pool's one permit is taken", || parked.arrivals() == 1).await;
        assert_eq!(pool.available_permits(), 0);

        let sampler = pool.clone();
        let sample = tokio::spawn(async move {
            sampler
                .sample_field_values(
                    "service",
                    None,
                    10,
                    Deadline::after(Duration::from_secs(600)),
                    TEST_SAMPLE_WORK,
                )
                .await
        });
        settle().await;
        tokio::time::advance(Duration::from_millis(900)).await;
        settle().await;
        assert!(
            !sample.is_finished(),
            "the sample must still be waiting inside its acquisition budget"
        );
        tokio::time::advance(Duration::from_millis(101)).await;
        settle().await;
        let refused = sample
            .await
            .expect("the sample task joins")
            .expect_err("a full pool cannot be sampled");
        assert!(
            matches!(&refused, ServerError::CapacityNotStarted),
            "expected the safe capacity refusal, got {refused:?}"
        );

        let sampler = pool.clone();
        let short = tokio::spawn(async move {
            sampler
                .sample_field_values(
                    "service",
                    None,
                    10,
                    Deadline::after(Duration::from_millis(300)),
                    TEST_SAMPLE_WORK,
                )
                .await
        });
        settle().await;
        tokio::time::advance(Duration::from_millis(301)).await;
        settle().await;
        assert!(
            short
                .await
                .expect("the sample task joins")
                .is_err_and(|e| matches!(&e, ServerError::CapacityNotStarted)),
        );

        assert_eq!(pool.retained(), 0);
        assert_eq!(pool.available_permits(), 0, "the query still holds the one");

        parked.release();
        let outcome = occupied.await.expect("the query joins");
        assert!(outcome.result.is_ok(), "{:?}", outcome.result);
        until("cleanup returns everything", || {
            idle_len(&pool) == baseline_idle && pool.available_permits() == pool.capacity()
        })
        .await;
        assert!(registry_is_empty(&pool));
    }

    #[tokio::test(start_paused = true)]
    async fn a_sample_that_overruns_is_a_timeout_and_stays_retained() {
        let pool = hot_pool(1);
        let seams = pool.seams();
        let started = seams.hold(Seam::Started);
        let baseline_idle = idle_len(&pool);

        let sampler = pool.clone();
        let sample = tokio::spawn(async move {
            sampler
                .sample_field_values(
                    "service",
                    None,
                    10,
                    Deadline::after(Duration::from_secs(5)),
                    TEST_SAMPLE_WORK,
                )
                .await
        });
        until("the sample reaches its work", || started.arrivals() == 1).await;

        tokio::time::advance(Duration::from_secs(6)).await;
        let refused = sample
            .await
            .expect("the sample task joins")
            .expect_err("a sample past its deadline does not answer");
        assert!(
            matches!(refused, ServerError::Timeout),
            "expected a post-start timeout, got {refused:?}"
        );

        let retained = pool.retained_work();
        assert_eq!(pool.retained(), 1);
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].kind, WorkKind::Sample);
        assert!(retained[0].started);
        let id = retained[0].id;
        assert!(pool.cancel_by_id(id), "the abandoned sample is cancellable");
        assert!(pool.cancel_by_id(id), "and repeatedly so");

        started.release();
        until("cleanup returns everything", || {
            pool.retained() == 0
                && idle_len(&pool) == baseline_idle
                && pool.available_permits() == pool.capacity()
        })
        .await;
        assert!(!pool.cancel_by_id(id));
        assert!(registry_is_empty(&pool));
    }

    #[tokio::test]
    async fn a_staging_file_failure_names_no_path() {
        let pool = ExecutorPool::for_test("/nonexistent".into(), 1, 100, None).unwrap();
        let scratch = pool.scratch().path().to_owned();
        std::fs::remove_dir_all(&scratch).expect("the scratch directory");

        let error = pool
            .export_parquet(
                pool.allocate_query_id(),
                "*",
                10,
                Deadline::after(Duration::from_secs(30)),
                TEST_WORK,
            )
            .await
            .expect_err("no staging file, no export");
        assert!(matches!(error, ServerError::Internal(_)), "{error:?}");
        let shown = error.to_string();
        for named in [scratch.to_str().unwrap(), "trawl-export-"] {
            assert!(
                !shown.contains(named),
                "{named:?} reached the error: {shown}"
            );
        }
    }

    /// The bare word reads the absent `_raw` as NULL and searches `message`,
    /// and the dedup over a million rows runs out of the shared budget.
    #[tokio::test]
    async fn an_exhausted_budget_over_a_rawless_corpus_answers_resource_exhausted() {
        use std::num::{NonZeroU32, NonZeroU64};

        use crate::bus::IngestBatch;
        use crate::hot_buffer::HotBufferConfig;
        use trawl_engine::error::EngineError;

        const MIB: u64 = 1024 * 1024;
        const DSL: &str = "hello | dedup message | stats count()";

        let dir = tempfile::tempdir().expect("temp dir");
        let data = dir.path().join("data");
        let hour = data.join("prod/2026-01-03/00");
        std::fs::create_dir_all(&hour).expect("hour dir");
        let corpus = hour.join("svc.parquet");
        duckdb::Connection::open_in_memory()
            .expect("fixture duckdb")
            .execute_batch(&format!(
                "COPY (SELECT TIMESTAMP '2026-01-03 00:05:00' AS _time, \
                 'hello ' || i::VARCHAR AS message FROM range(1000000) t(i)) \
                 TO '{}' (FORMAT PARQUET)",
                corpus.to_str().expect("utf-8")
            ))
            .expect("a corpus with no _raw column");
        let budgets = QueryBudgets {
            spill_max_bytes: NonZeroU64::new(4 * MIB).unwrap(),
            memory_limit: NonZeroU64::new(32 * MIB).unwrap(),
            threads: NonZeroU32::new(2).unwrap(),
        };
        let hot = || {
            let hot = Arc::new(HotBuffer::new(HotBufferConfig {
                max_events: 100,
                max_bytes: 1 << 20,
            }));
            let mut event = serde_json::Map::new();
            event.insert("_time".into(), "2026-01-03T00:05:00Z".into());
            event.insert("_ingested".into(), "2026-01-03T00:05:01Z".into());
            event.insert("service".into(), "svc".into());
            event.insert("message".into(), "hello hot".into());
            hot.insert_for_test(Arc::new(IngestBatch {
                batch_id: "prod/svc_hot".into(),
                service: "svc".into(),
                byte_size: 64,
                events: vec![event],
            }));
            hot
        };

        let exhausted = |error: &ServerError| {
            matches!(error, ServerError::Engine(EngineError::ResourceExhausted))
        };
        let mut wrong = Vec::new();
        for (label, hot) in [("cold only", None), ("with a hot buffer", Some(hot()))] {
            let pool = ExecutorPool::for_test_with_budgets(
                data.to_str().expect("utf-8").to_owned(),
                1,
                100_000,
                hot,
                &budgets,
            )
            .expect("pool");
            let deadline = || Deadline::after(Duration::from_secs(120));

            let query = pool
                .execute(pool.allocate_query_id(), DSL, deadline(), false, TEST_WORK)
                .await
                .result;
            if !query.as_ref().is_err_and(exhausted) {
                wrong.push(format!("query, {label}: {:?}", query.map(|r| r.rows)));
            }
            let export = pool
                .export_parquet(pool.allocate_query_id(), DSL, 1_000, deadline(), TEST_WORK)
                .await;
            if !export.as_ref().is_err_and(exhausted) {
                wrong.push(format!(
                    "export, {label}: {:?}",
                    export.map(|(bytes, _)| parquet_rows(&bytes))
                ));
            }
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_retained_worker_keeps_the_scratch_locked_past_its_pool() {
        use trawl_engine::scratch::{Reclaimed, ScratchError};

        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("query-scratch");
        let pool = ExecutorPool::new(
            "/nonexistent".into(),
            2,
            100,
            None,
            Arc::new(ScratchLock::open(&path).expect("the first lock")),
            &QueryBudgets {
                spill_max_bytes: crate::config::DEFAULT_QUERY_SPILL_MAX_BYTES,
                memory_limit: crate::config::DEFAULT_QUERY_MEMORY_LIMIT,
                threads: crate::config::DEFAULT_QUERY_THREADS,
            },
        )
        .expect("pool");
        let sample = crate::engine_sample::EngineSample::start(&pool).expect("sampler");
        sample.wait_for_first();
        let seams = pool.seams();
        let started = seams.hold(Seam::Started);

        let sampler = pool.clone();
        let field = tokio::spawn(async move {
            sampler
                .sample_field_values(
                    "service",
                    None,
                    10,
                    Deadline::after(Duration::from_secs(5)),
                    TEST_SAMPLE_WORK,
                )
                .await
        });
        until("the field sample starts", || started.arrivals() == 1).await;
        let saved = pool.clone();
        let run = tokio::spawn(async move {
            saved
                .execute_with_source(
                    saved.allocate_query_id(),
                    "*",
                    "read_parquet('/nonexistent/run.parquet')",
                    Deadline::after(Duration::from_secs(5)),
                    false,
                    TEST_WORK,
                )
                .await
        });
        until("the saved run starts", || started.arrivals() == 2).await;

        tokio::time::advance(Duration::from_secs(6)).await;
        let field = field.await.expect("the sample joins");
        assert!(matches!(field, Err(ServerError::Timeout)), "{field:?}");
        let run = run.await.expect("the run joins").result;
        assert!(matches!(run, Err(ServerError::Timeout)), "{run:?}");
        assert_eq!(pool.retained(), 2, "both workers outlive their requests");

        sample.stop_and_wait_for_test();
        drop(pool);
        let refused = ScratchLock::open(&path).expect_err("a parked worker still holds the lock");
        assert!(
            matches!(refused, ScratchError::Locked { .. }),
            "expected the held lock, got {refused}"
        );

        started.release();
        let begun = std::time::Instant::now();
        let reopened = loop {
            match ScratchLock::open(&path) {
                Ok(lock) => break lock,
                Err(ScratchError::Locked { .. }) => {
                    assert!(
                        begun.elapsed() < Duration::from_secs(20),
                        "the lock never freed after the workers finished"
                    );
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("the scratch directory refused a reopen: {error}"),
            }
        };
        assert_eq!(
            reopened.reclaimed(),
            Reclaimed::default(),
            "the database removed its own spill directory before the lock freed"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn exclusive_names_retained_permits_when_blocked() {
        const BUDGET: Duration = Duration::from_millis(20);

        use std::sync::Mutex as StdMutex;

        use tracing_subscriber::prelude::*;

        // Its own thread-local capture rather than the shared one: this
        // event is emitted by the caller, on this thread, and it carries
        // no query_id for the shared sink to key on.

        #[derive(Clone, Default)]
        struct Capture(Arc<StdMutex<Vec<std::collections::HashMap<String, String>>>>);

        struct Fields<'a>(&'a mut std::collections::HashMap<String, String>);

        impl tracing::field::Visit for Fields<'_> {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.insert(field.name().to_owned(), format!("{value:?}"));
            }
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                self.0.insert(field.name().to_owned(), value.to_owned());
            }
            fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
                self.0.insert(field.name().to_owned(), value.to_string());
            }
        }

        impl<S> tracing_subscriber::Layer<S> for Capture
        where
            S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
        {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                let mut fields = std::collections::HashMap::new();
                event.record(&mut Fields(&mut fields));
                self.0.lock().expect("capture poisoned").push(fields);
            }
        }

        let pool = hot_pool(1);
        let seams = pool.seams();
        let parked = seams.hold(Seam::Started);
        let id = pool.allocate_query_id();

        let submitted = pool.clone();
        let request = tokio::spawn(async move {
            submitted
                .execute(
                    id,
                    "*",
                    Deadline::after(Duration::from_secs(5)),
                    false,
                    TEST_WORK,
                )
                .await
        });
        until("the work starts", || parked.arrivals() == 1).await;
        tokio::time::advance(Duration::from_secs(6)).await;
        let outcome = request.await.expect("the request joins");
        assert!(matches!(outcome.result, Err(ServerError::Timeout)));
        assert_eq!(pool.retained(), 1, "the permit outlived its request");

        let captured = Capture::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let guard = tracing::subscriber::set_default(subscriber);

        let blocked_pool = pool.clone();
        let attempt = tokio::spawn(async move { blocked_pool.exclusive(BUDGET).await });
        settle().await;
        tokio::time::advance(BUDGET + Duration::from_millis(1)).await;
        settle().await;
        let err = attempt
            .await
            .expect("the attempt joins")
            .expect_err("a retained permit blocks exclusivity");
        assert!(matches!(err, ServerError::Timeout), "got {err:?}");
        drop(guard);

        let events = captured.0.lock().expect("capture poisoned").clone();
        let blocked = events
            .iter()
            .find(|f| f.get("event_type").map(String::as_str) == Some("pool_exclusive_blocked"))
            .expect("the failed acquisition must say why");
        assert_eq!(blocked.get("retained").map(String::as_str), Some("1"));
        assert_eq!(blocked.get("held").map(String::as_str), Some("1"));
        assert_eq!(blocked.get("capacity").map(String::as_str), Some("1"));
        assert_eq!(blocked.get("timeout_ms").map(String::as_str), Some("20"));

        let publisher = Arc::clone(&pool.publication);
        let publishing = tokio::spawn(async move { publisher.write().await });
        settle().await;
        assert!(
            !publishing.is_finished(),
            "retained work still holds its publication read guard"
        );

        parked.release();
        until("the permit comes back", || pool.retained() == 0).await;
        settle().await;
        assert!(
            publishing.is_finished(),
            "cleanup releases the publication guard as well as the permit"
        );
        drop(publishing.await.expect("the publisher joins"));
        pool.exclusive(Duration::from_secs(2))
            .await
            .expect("a drained pool becomes exclusive, with no wait to drive");
    }

    #[tokio::test]
    async fn cancel_all_clears_interrupts() {
        let pool = ExecutorPool::for_test("/nonexistent".into(), 1, 100_000, None).unwrap();
        pool.cancel_all();
        assert!(pool.registry.lock().interrupts.is_empty());
    }

    /// A timing account for one test query, correlated by a request id
    /// no sibling test shares, so a capture can keep its events.
    fn timed(pool: &ExecutorPool, id: u64) -> (QueryTiming, String) {
        let request_id = format!("req-{}-{id}", pool.instance);
        let timing = QueryTiming::new(
            Instant::now(),
            id,
            crate::query_timing::Correlation::Request(request_id.clone()),
            crate::query_timing::TimingKind::Query,
        );
        (timing, request_id)
    }

    /// Park whoever enters `phase` on `timing`'s clock, at a gate of its
    /// own. The session releases it on drop, as a pool's seams do, so a
    /// failed assertion cannot leave the worker parked.
    fn hold_phase(timing: &QueryTiming, phase: QueryPhase) -> (seam::Session, Arc<seam::Gate>) {
        let table = Arc::new(seam::Table::default());
        // The seam name is only this private table's key.
        let gate = table.hold(Seam::Entry);
        let hook = Arc::clone(&table);
        timing
            .clock()
            .hold_on_enter(phase, move || hook.reach(Seam::Entry));
        (seam::Session(table), gate)
    }

    #[test]
    fn dropping_the_session_releases_a_displaced_gate() {
        let table = Arc::new(seam::Table::default());
        let first = table.hold(Seam::Entry);
        let worker = Arc::clone(&table);
        let (left, leaving) = std::sync::mpsc::channel();
        let parked = std::thread::spawn(move || {
            worker.reach(Seam::Entry);
            left.send(()).expect("the test is listening");
        });
        let deadline = std::time::Instant::now() + crate::test_timing::HANG_GUARD;
        while first.arrivals() == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the worker never reached the first gate"
            );
            std::thread::yield_now();
        }
        let _second = table.hold(Seam::Entry);
        drop(seam::Session(table));
        leaving
            .recv_timeout(crate::test_timing::HANG_GUARD)
            .expect("the session's drop let the worker parked at the displaced gate out");
        parked.join().expect("the worker thread finished");
    }

    /// The phases only a worker that started books.
    const WORKER_PHASES: [QueryPhase; 10] = [
        QueryPhase::Startup,
        QueryPhase::Source,
        QueryPhase::HotSnapshot,
        QueryPhase::Emit,
        QueryPhase::Probe,
        QueryPhase::Bind,
        QueryPhase::Execute,
        QueryPhase::Copy,
        QueryPhase::Post,
        QueryPhase::Render,
    ];

    type Fields = std::collections::HashMap<String, String>;

    fn value<'a>(event: &'a Fields, name: &str) -> Option<&'a str> {
        event.get(name).map(String::as_str)
    }

    fn micros(event: &Fields, name: &str) -> u64 {
        value(event, name)
            .unwrap_or_else(|| panic!("{name} present: {event:?}"))
            .parse()
            .unwrap_or_else(|_| panic!("{name} is an integer: {event:?}"))
    }

    /// The accounts one query wrote: complete `query_timing` events,
    /// partial ones, and reclaims carrying the worker's totals.
    fn accounts(capture: &capture::Capture) -> (usize, usize, usize) {
        let timings = capture.of_type("query_timing");
        let complete = timings
            .iter()
            .filter(|e| value(e, "timing_complete") == Some("true"))
            .count();
        let reclaims = capture
            .of_type("query_permit_reclaimed")
            .iter()
            .filter(|e| e.contains_key("duckdb_attempts"))
            .count();
        (complete, timings.len() - complete, reclaims)
    }

    #[tokio::test(start_paused = true)]
    async fn capacity_refusal_reports_wait_phases() {
        let pool = ExecutorPool::for_test("/nonexistent".into(), 1, 100_000, None).unwrap();

        let held = pool
            .exclusive(Duration::from_secs(1))
            .await
            .expect("the idle pool grants exclusivity at once");
        let id = pool.allocate_query_id();
        let (timing, request_id) = timed(&pool, id);
        let capture = capture::of_timed(&pool, id, &request_id).await;
        let outcome = pool
            .execute(
                id,
                "*",
                Deadline::after(Duration::from_secs(5)),
                false,
                TEST_WORK.with_timing(timing),
            )
            .await;
        assert!(
            matches!(&outcome.result, Err(ServerError::CapacityNotStarted)),
            "{:?}",
            outcome.result
        );
        let queued = capture.of_type("query_timing");
        drop(capture);
        drop(held);

        let writer = pool.publication.write().await;
        let id = pool.allocate_query_id();
        let (timing, request_id) = timed(&pool, id);
        let capture = capture::of_timed(&pool, id, &request_id).await;
        let outcome = pool
            .execute(
                id,
                "*",
                Deadline::after(Duration::from_secs(5)),
                false,
                TEST_WORK.with_timing(timing),
            )
            .await;
        assert!(
            matches!(&outcome.result, Err(ServerError::CapacityNotStarted)),
            "{:?}",
            outcome.result
        );
        let gated = capture.of_type("query_timing");
        drop(capture);
        drop(writer);

        for (label, events, waits) in [
            ("queue", queued, &[QueryPhase::PoolWait][..]),
            (
                "publication",
                gated,
                &[QueryPhase::PoolWait, QueryPhase::PublicationWait][..],
            ),
        ] {
            assert_eq!(events.len(), 1, "{label}: one account: {events:?}");
            let event = &events[0];
            assert_eq!(value(event, "outcome"), Some("capacity_refused"), "{label}");
            assert_eq!(value(event, "work_started"), Some("false"), "{label}");
            assert_eq!(value(event, "timing_complete"), Some("true"), "{label}");
            assert_eq!(value(event, "error_class"), None, "{label}");
            for phase in waits {
                micros(event, phase.field());
            }
            for phase in WORKER_PHASES {
                assert!(
                    !event.contains_key(phase.field()),
                    "{label}: no worker phase {}: {event:?}",
                    phase.field()
                );
            }
            if label == "queue" {
                assert!(
                    !event.contains_key("query_publication_wait_us"),
                    "{event:?}"
                );
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_in_bind_reports_active_phase_then_reclaim_totals() {
        let pool = hot_pool(1);
        let id = pool.allocate_query_id();
        let (timing, request_id) = timed(&pool, id);
        let (_bind_session, bind) = hold_phase(&timing, QueryPhase::Bind);
        let capture = capture::of_timed(&pool, id, &request_id).await;

        let submitted = pool.clone();
        let work = TEST_WORK.with_timing(timing);
        let request = tokio::spawn(async move {
            submitted
                .execute(
                    id,
                    "*",
                    Deadline::after(Duration::from_secs(5)),
                    false,
                    work,
                )
                .await
        });
        until("the worker binds", || bind.arrivals() == 1).await;
        tokio::time::advance(Duration::from_secs(6)).await;
        let outcome = request.await.expect("the request joins");
        assert!(
            matches!(outcome.result, Err(ServerError::Timeout)),
            "{:?}",
            outcome.result
        );

        let timings = capture.of_type("query_timing");
        assert_eq!(timings.len(), 1, "{timings:?}");
        let partial = &timings[0];
        assert_eq!(value(partial, "query_id"), Some(id.to_string().as_str()));
        assert_eq!(value(partial, "request_id"), Some(request_id.as_str()));
        assert_eq!(value(partial, "outcome"), Some("timeout"));
        assert_eq!(value(partial, "work_started"), Some("true"));
        assert_eq!(value(partial, "timing_complete"), Some("false"));
        assert_eq!(value(partial, "active_query_phase"), Some("bind"));
        assert!(micros(partial, "active_elapsed_us") > 0, "{partial:?}");
        assert!(
            !partial.contains_key("query_bind_us"),
            "unfinished: {partial:?}"
        );
        for phase in [
            QueryPhase::PoolWait,
            QueryPhase::PublicationWait,
            QueryPhase::Startup,
            QueryPhase::Source,
        ] {
            micros(partial, phase.field());
        }
        assert!(
            capture.of_type("query_permit_reclaimed").is_empty(),
            "the work is still running"
        );

        bind.release();
        until("the work is reclaimed", || {
            capture.of_type("query_permit_reclaimed").len() == 1
        })
        .await;
        let reclaims = capture.of_type("query_permit_reclaimed");
        let reclaim = &reclaims[0];
        assert_eq!(value(reclaim, "query_id"), Some(id.to_string().as_str()));
        assert_eq!(value(reclaim, "request_id"), Some(request_id.as_str()));
        assert_eq!(
            value(reclaim, "physical_outcome"),
            Some("cancelled"),
            "the latched cancellation stops it after the bind: {reclaim:?}"
        );
        micros(reclaim, "query_bind_us");
        micros(reclaim, "query_startup_us");
        assert!(micros(reclaim, "duckdb_attempts") >= 1, "{reclaim:?}");
        // No parquet under the base dir: the engine answers hot-only.
        assert_eq!(value(reclaim, "fallback"), Some("hot_only"));
        assert_eq!(
            capture.of_type("query_timing").len(),
            1,
            "the reclaim finishes the account; it does not write a second"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_finish_race_writes_one_final_account() {
        let pool = hot_pool(1);

        let seams = pool.seams();
        let finished = seams.hold(Seam::Finished);
        let id = pool.allocate_query_id();
        let (timing, request_id) = timed(&pool, id);
        let capture = capture::of_timed(&pool, id, &request_id).await;
        let submitted = pool.clone();
        let work = TEST_WORK.with_timing(timing);
        let request = tokio::spawn(async move {
            submitted
                .execute(
                    id,
                    "*",
                    Deadline::after(Duration::from_secs(5)),
                    false,
                    work,
                )
                .await
        });
        until("the answer is in hand", || finished.arrivals() == 1).await;
        tokio::time::advance(Duration::from_secs(6)).await;
        let outcome = request.await.expect("the request joins");
        assert!(matches!(outcome.result, Err(ServerError::Timeout)));
        assert_eq!(accounts(&capture), (0, 1, 0), "partial, reclaim pending");
        finished.release();
        until("the work is reclaimed", || {
            capture.of_type("query_permit_reclaimed").len() == 1
        })
        .await;
        let deadline_first = accounts(&capture);
        drop(capture);
        drop(seams);

        let seams = pool.seams();
        let released = seams.hold(Seam::Released);
        let id = pool.allocate_query_id();
        let (timing, request_id) = timed(&pool, id);
        let capture = capture::of_timed(&pool, id, &request_id).await;
        let submitted = pool.clone();
        let work = TEST_WORK.with_timing(timing);
        let request = tokio::spawn(async move {
            submitted
                .execute(
                    id,
                    "*",
                    Deadline::after(Duration::from_secs(5)),
                    false,
                    work,
                )
                .await
        });
        until("cleanup ran", || released.arrivals() == 1).await;
        assert!(
            pool.owner_of(id).is_none(),
            "the slot is gone before the request hears"
        );
        tokio::time::advance(Duration::from_secs(6)).await;
        let outcome = request.await.expect("the request joins");
        assert!(matches!(outcome.result, Err(ServerError::Timeout)));
        released.release();
        until("the worker hands back", || pool.available_permits() == 1).await;
        let worker_first = accounts(&capture);
        let timings = capture.of_type("query_timing");
        drop(capture);

        assert_eq!(deadline_first, (0, 1, 1), "partial + one reclaim");
        assert_eq!(worker_first, (1, 0, 0), "one complete account");
        for (complete, partial, reclaims) in [deadline_first, worker_first] {
            assert_eq!(complete + reclaims, 1, "never both, never neither");
            assert_eq!(
                partial, reclaims,
                "a partial account is finished by its reclaim"
            );
        }
        assert_eq!(value(&timings[0], "outcome"), Some("timeout"));
        assert_eq!(value(&timings[0], "active_query_phase"), None);
    }

    #[tokio::test(start_paused = true)]
    async fn abandoned_request_reports_partial_timing() {
        let pool = hot_pool(1);
        let id = pool.allocate_query_id();
        let (timing, request_id) = timed(&pool, id);
        let (_bind_session, bind) = hold_phase(&timing, QueryPhase::Bind);
        let capture = capture::of_timed(&pool, id, &request_id).await;

        let submitted = pool.clone();
        let work = TEST_WORK.with_timing(timing);
        let request = tokio::spawn(async move {
            submitted
                .execute(
                    id,
                    "*",
                    Deadline::after(Duration::from_secs(600)),
                    false,
                    work,
                )
                .await
        });
        until("the worker binds", || bind.arrivals() == 1).await;
        request.abort();
        assert!(
            request.await.unwrap_err().is_cancelled(),
            "the requesting task is gone"
        );

        let timings = capture.of_type("query_timing");
        assert_eq!(timings.len(), 1, "{timings:?}");
        let partial = &timings[0];
        assert_eq!(value(partial, "outcome"), Some("abandoned"));
        assert_eq!(value(partial, "work_started"), Some("true"));
        assert_eq!(value(partial, "timing_complete"), Some("false"));
        assert_eq!(value(partial, "active_query_phase"), Some("bind"));
        assert!(!partial.contains_key("query_bind_us"), "{partial:?}");
        assert!(capture.of_type("query_permit_reclaimed").is_empty());

        bind.release();
        until("the work is reclaimed", || {
            capture.of_type("query_permit_reclaimed").len() == 1
        })
        .await;
        let reclaims = capture.of_type("query_permit_reclaimed");
        let reclaim = &reclaims[0];
        assert_eq!(value(reclaim, "query_id"), Some(id.to_string().as_str()));
        assert_eq!(value(reclaim, "request_id"), Some(request_id.as_str()));
        assert_eq!(value(reclaim, "physical_outcome"), Some("cancelled"));
        micros(reclaim, "query_bind_us");
        assert_eq!(accounts(&capture), (0, 1, 1));
    }

    #[tokio::test]
    async fn pool_acquired_is_gone() {
        let pool = hot_pool(1);
        let capture = capture::everything().await;
        let outcome = pool
            .execute(
                pool.allocate_query_id(),
                "*",
                Deadline::after(Duration::from_secs(30)),
                false,
                TEST_WORK,
            )
            .await;
        assert!(outcome.result.is_ok(), "{:?}", outcome.result);
        let _ = pool
            .execute_with_source(
                pool.allocate_query_id(),
                "*",
                "'/nonexistent/saved.parquet'",
                Deadline::after(Duration::from_secs(30)),
                false,
                TEST_WORK,
            )
            .await;
        assert!(
            !capture.of_type("query_source").is_empty(),
            "the capture sees the blocking worker's events"
        );
        assert!(
            capture.of_type("pool_acquired").is_empty(),
            "{:?}",
            capture.of_type("pool_acquired")
        );
    }

    /// The hour holding [`cold_and_hot_pool`]'s rows, under a budget
    /// nothing reaches.
    fn window() -> CaptureSource {
        window_under(u64::MAX)
    }

    fn window_under(max_bytes: u64) -> CaptureSource {
        CaptureSource::Window {
            start: "2026-01-03T00:00:00Z".parse().unwrap(),
            end: "2026-01-03T01:00:00Z".parse().unwrap(),
            max_bytes,
        }
    }

    fn rows_of() -> CaptureFinish<usize> {
        Box::new(|input: CaptureInput<'_>| Ok(input.result.rows.len()))
    }

    #[derive(Debug, Clone, Copy)]
    struct Released {
        at: Instant,
        available_permits: usize,
        idle_executors: usize,
        /// Interrupt slots plus lifecycle entries still registered.
        registered: usize,
    }

    struct Charge {
        registry: Arc<Mutex<Registry>>,
        semaphore: Arc<Semaphore>,
        released: Arc<Mutex<Option<Released>>>,
    }

    impl Drop for Charge {
        fn drop(&mut self) {
            let (idle_executors, registered) = {
                let registry = self.registry.lock();
                (
                    registry.idle.len(),
                    registry.interrupts.len() + registry.retained.len(),
                )
            };
            *self.released.lock() = Some(Released {
                at: Instant::now(),
                available_permits: self.semaphore.available_permits(),
                idle_executors,
                registered,
            });
        }
    }

    fn charged(pool: &ExecutorPool, key: i64) -> (WorkContext, Arc<Mutex<Option<Released>>>) {
        let released = Arc::new(Mutex::new(None));
        let charge = Charge {
            registry: Arc::clone(&pool.registry),
            semaphore: Arc::clone(&pool.semaphore),
            released: Arc::clone(&released),
        };
        (
            WorkContext::key(WorkKind::Capture, key).holding(Arc::new(charge)),
            released,
        )
    }

    fn stored_parquet_bytes(root: &std::path::Path) -> u64 {
        let mut total = 0;
        for entry in std::fs::read_dir(root).unwrap() {
            let entry = entry.unwrap();
            let meta = entry.metadata().unwrap();
            if meta.is_dir() {
                total += stored_parquet_bytes(&entry.path());
            } else if entry.path().extension().is_some_and(|ext| ext == "parquet") {
                total += meta.len();
            }
        }
        total
    }

    #[tokio::test]
    async fn a_capture_finishes_inside_its_slot_and_releases_its_charge_after_the_permit() {
        let (dir, _hot, pool) = cold_and_hot_pool().await;
        let (work, released) = charged(&pool, 7);
        let out = tempfile::tempdir().unwrap();
        let dest = out.path().join("capture.parquet");
        let semaphore = Arc::clone(&pool.semaphore);
        let finish: CaptureFinish<_> = Box::new({
            let dest = dest.clone();
            move |input: CaptureInput<'_>| -> Result<_, ServerError> {
                let permits_inside = semaphore.available_permits();
                assert!(!input.cancel.latched());
                let file = input.clock.time(QueryPhase::Write, || {
                    input.executor.write_capture_parquet(&input.result, &dest)
                })?;
                Ok((
                    input.result.rows.len(),
                    file,
                    input.source_bytes,
                    permits_inside,
                ))
            }
        });

        let answer = pool
            .capture(
                pool.allocate_query_id(),
                "*",
                window(),
                RowCap::Refuse(100),
                Deadline::after(Duration::from_secs(60)),
                work,
                finish,
            )
            .await;
        let answered = Instant::now();

        let (rows, file, source_bytes, permits_inside) = answer.expect("the capture answers");
        assert_eq!(rows, 2, "one cold row and one hot row");
        assert_eq!(file.rows, 2);
        assert_eq!(parquet_rows(&std::fs::read(&dest).unwrap()), 2);
        assert_eq!(
            permits_inside, 0,
            "finish runs while the slot holds the pool's one permit"
        );
        assert_eq!(
            source_bytes,
            Some(stored_parquet_bytes(&dir.path().join("data"))),
            "the budget measured the stored files the window selects"
        );
        let released = released.lock().expect("the charge was released");
        assert_eq!(
            released.available_permits,
            pool.capacity(),
            "the permit was back before the charge dropped"
        );
        assert_eq!(released.idle_executors, pool.capacity());
        assert_eq!(released.registered, 0);
        assert!(
            released.at <= answered,
            "a finished capture releases its charge before its request hears"
        );
        assert!(registry_is_empty(&pool));
    }

    #[tokio::test(start_paused = true)]
    async fn a_capture_dropped_before_it_starts_never_finishes_and_releases_its_charge() {
        let pool = hot_pool(1);
        let seams = pool.seams();
        let entry = seams.hold(Seam::Entry);
        let started = seams.watch(Seam::Started);
        let baseline_idle = idle_len(&pool);
        let (work, released) = charged(&pool, 7);
        let finished = Arc::new(AtomicBool::new(false));

        let submitted = pool.clone();
        let request = tokio::spawn({
            let finished = Arc::clone(&finished);
            async move {
                submitted
                    .capture(
                        submitted.allocate_query_id(),
                        "*",
                        window(),
                        RowCap::Refuse(100),
                        Deadline::after(Duration::from_secs(600)),
                        work,
                        Box::new(move |_: CaptureInput<'_>| {
                            finished.store(true, Ordering::SeqCst);
                            Ok(())
                        }),
                    )
                    .await
            }
        });
        until("the worker is dispatched", || entry.arrivals() == 1).await;
        assert!(
            released.lock().is_none(),
            "the parked work holds its charge"
        );

        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        until("the abandoned capture books its permit", || {
            pool.retained() == 1
        })
        .await;
        assert_eq!(pool.available_permits(), 0);
        assert!(
            released.lock().is_none(),
            "the charge outlives the request while the work exists"
        );

        entry.release();
        until("the charge is released", || released.lock().is_some()).await;
        let released = released.lock().expect("released");
        assert_eq!(released.available_permits, 1);
        assert_eq!(released.idle_executors, baseline_idle);
        assert_eq!(released.registered, 0);
        assert_eq!(
            started.arrivals(),
            0,
            "the latched cancellation refused the start"
        );
        assert!(!finished.load(Ordering::SeqCst), "finish never ran");
        assert!(registry_is_empty(&pool));
    }

    struct Unheard(Arc<AtomicBool>);

    impl Drop for Unheard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_finish_past_the_deadline_times_out_and_holds_its_charge_until_it_returns() {
        let pool = hot_pool(1);
        let baseline_idle = idle_len(&pool);
        let (work, released) = charged(&pool, 7);
        let entered = Arc::new(AtomicBool::new(false));
        let unheard = Arc::new(AtomicBool::new(false));
        let (unblock, blocked) = std::sync::mpsc::channel::<()>();
        let id = pool.allocate_query_id();

        let submitted = pool.clone();
        let request = tokio::spawn({
            let (entered, unheard) = (Arc::clone(&entered), Arc::clone(&unheard));
            async move {
                submitted
                    .capture(
                        id,
                        "*",
                        window(),
                        RowCap::Refuse(100),
                        Deadline::after(Duration::from_secs(5)),
                        work,
                        Box::new(move |_: CaptureInput<'_>| {
                            entered.store(true, Ordering::SeqCst);
                            blocked
                                .recv_timeout(Duration::from_secs(60))
                                .expect("the test unblocks finish");
                            Ok(Unheard(unheard))
                        }),
                    )
                    .await
            }
        });
        until("finish is writing", || entered.load(Ordering::SeqCst)).await;
        tokio::time::advance(Duration::from_secs(6)).await;
        let answer = request.await.expect("the request joins");
        assert!(
            matches!(answer, Err(ServerError::Timeout)),
            "work that started is a timeout"
        );
        assert!(
            released.lock().is_none(),
            "the charge is held while finish runs"
        );
        assert_eq!(pool.available_permits(), 0);
        let retained = pool.retained_work();
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].id, id);
        assert_eq!(retained[0].kind, WorkKind::Capture);
        assert!(retained[0].started);
        assert!(pool.cancel_by_id(id), "the retained capture is cancellable");

        unblock.send(()).unwrap();
        until("the charge is released", || released.lock().is_some()).await;
        let released = released.lock().expect("released");
        assert_eq!(released.available_permits, 1);
        assert_eq!(released.idle_executors, baseline_idle);
        assert_eq!(released.registered, 0);
        until("finish's late answer is dropped, never delivered", || {
            unheard.load(Ordering::SeqCst)
        })
        .await;
        assert_eq!(pool.retained(), 0);
        assert!(registry_is_empty(&pool));
    }

    #[tokio::test]
    async fn a_capture_over_its_source_budget_refuses_before_it_starts() {
        let (dir, _hot, pool) = cold_and_hot_pool().await;
        let stored = stored_parquet_bytes(&dir.path().join("data"));
        assert!(stored > 1, "the fixture stores a parquet file");
        let seams = pool.seams();
        let started = seams.watch(Seam::Started);
        let (work, released) = charged(&pool, 7);
        let finished = Arc::new(AtomicBool::new(false));
        let id = pool.allocate_query_id();
        let log = capture::of_query(&pool, id).await;

        let refused = pool
            .capture(
                id,
                "*",
                window_under(1),
                RowCap::Refuse(100),
                Deadline::after(Duration::from_secs(60)),
                work,
                Box::new({
                    let finished = Arc::clone(&finished);
                    move |_: CaptureInput<'_>| {
                        finished.store(true, Ordering::SeqCst);
                        Ok(())
                    }
                }),
            )
            .await
            .expect_err("the window is over budget");

        match refused {
            ServerError::CaptureSourceTooLarge {
                selected_bytes,
                max_bytes,
                kind,
            } => {
                assert_eq!(selected_bytes, stored);
                assert_eq!(max_bytes, 1);
                assert_eq!(kind, CaptureSourceKind::Window);
            }
            other => panic!("expected the source budget refusal, got {other:?}"),
        }
        assert_eq!(started.arrivals(), 0, "the refusal came before Started");
        assert!(!finished.load(Ordering::SeqCst), "finish never ran");
        assert!(
            snapshot_files(&pool).is_empty(),
            "no hot snapshot was taken"
        );
        let released = released.lock().expect("the charge was released");
        assert_eq!(released.available_permits, pool.capacity());
        assert_eq!(released.registered, 0);
        assert!(registry_is_empty(&pool));

        let refusals = log.of_type("capture_refused");
        assert_eq!(refusals.len(), 1, "{refusals:?}");
        let field = |name: &str| refusals[0].get(name).map(String::as_str);
        assert_eq!(field("reason"), Some("source_too_large"));
        assert_eq!(field("kind"), Some("capture"));
        assert_eq!(field("selected_bytes"), Some(stored.to_string().as_str()));
        assert_eq!(field("max_bytes"), Some("1"));
        assert!(
            log.of_type("query_not_started").is_empty(),
            "a budget refusal is not a capacity refusal"
        );
    }

    #[tokio::test]
    async fn a_resolved_capture_skips_the_publication_guard_and_the_hot_buffer() {
        let pool = hot_pool(1);
        let saved_dir = tempfile::tempdir().unwrap();
        let saved = saved_dir.path().join("saved.parquet");
        duckdb::Connection::open_in_memory()
            .unwrap()
            .execute_batch(&format!(
                "COPY (SELECT TIMESTAMP '2026-01-03 00:05:00' AS _time, \
                 TIMESTAMP '2026-01-03 00:05:01' AS _ingested, 'svc' AS service, \
                 'saved ' || i::VARCHAR AS message FROM range(3) t(i)) \
                 TO '{}' (FORMAT PARQUET)",
                saved.to_str().unwrap()
            ))
            .unwrap();
        let (work, released) = charged(&pool, 7);
        let writer = pool.publication.write().await;

        let source_bytes = pool
            .capture(
                pool.allocate_query_id(),
                "*",
                CaptureSource::Resolved {
                    source: saved.to_str().unwrap().to_owned(),
                },
                RowCap::Refuse(100),
                Deadline::after(Duration::from_secs(30)),
                work,
                Box::new(|input: CaptureInput<'_>| {
                    Ok((input.result.rows.len(), input.source_bytes))
                }),
            )
            .await
            .expect("a resolved capture does not wait for the guard");
        assert_eq!(source_bytes, (3, None), "the stored rows alone, unmeasured");
        assert!(
            snapshot_files(&pool).is_empty(),
            "no hot snapshot was taken"
        );
        assert!(released.lock().is_some());

        let refused = pool
            .capture(
                pool.allocate_query_id(),
                "*",
                window(),
                RowCap::Refuse(100),
                Deadline::after(Duration::from_millis(200)),
                WorkContext::key(WorkKind::Capture, 7),
                rows_of(),
            )
            .await
            .expect_err("a window waits for the guard the writer holds");
        assert!(
            matches!(&refused, ServerError::CapacityNotStarted),
            "{refused:?}"
        );
        drop(writer);
        assert_eq!(pool.available_permits(), pool.capacity());
    }

    #[tokio::test]
    async fn a_failing_capture_releases_everything() {
        let pool = hot_pool(1);
        let baseline_idle = idle_len(&pool);

        let (work, released) = charged(&pool, 7);
        let panicked = pool
            .capture(
                pool.allocate_query_id(),
                "*",
                window(),
                RowCap::Refuse(100),
                Deadline::after(Duration::from_secs(30)),
                work,
                Box::new(|_: CaptureInput<'_>| -> Result<(), ServerError> {
                    panic!("{}", seam::INJECTED_PANIC_PAYLOAD)
                }),
            )
            .await
            .expect_err("finish panics");
        assert!(
            matches!(panicked, ServerError::Panicked("capture worker")),
            "{panicked:?}"
        );
        assert!(released.lock().is_some(), "the charge was released");

        let (work, released) = charged(&pool, 7);
        let finished = Arc::new(AtomicBool::new(false));
        let failed = pool
            .capture(
                pool.allocate_query_id(),
                "| | invalid",
                window(),
                RowCap::Refuse(100),
                Deadline::after(Duration::from_secs(30)),
                work,
                Box::new({
                    let finished = Arc::clone(&finished);
                    move |_: CaptureInput<'_>| {
                        finished.store(true, Ordering::SeqCst);
                        Ok(())
                    }
                }),
            )
            .await
            .expect_err("the query does not parse");
        assert!(
            matches!(
                failed,
                ServerError::Engine(trawl_engine::error::EngineError::Parse(_))
            ),
            "{failed:?}"
        );
        assert!(!finished.load(Ordering::SeqCst), "finish never ran");
        assert!(released.lock().is_some(), "the charge was released");

        assert_eq!(idle_len(&pool), baseline_idle);
        assert_eq!(pool.available_permits(), pool.capacity());
        assert!(registry_is_empty(&pool));
    }
}
