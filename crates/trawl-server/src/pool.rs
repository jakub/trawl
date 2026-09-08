// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Semaphore-bounded executor pool for concurrent query execution.
//!
//! Pre-creates a pool of [`Executor`] instances sharing the same underlying
//! `DuckDB` database via [`Executor::try_clone`]. Long-lived connections
//! benefit from `DuckDB`'s internal metadata caching (parquet file stats,
//! column statistics, prepared statement cache).
//!
//! The semaphore limits concurrency, and each permit corresponds to exactly
//! one pooled executor. Timed-out queries are interrupted via `DuckDB`'s
//! interrupt handle; the executor is reclaimed once the interrupted work
//! actually stops, which is not when the request answered.
//!
//! # The request and the work it started are two lifetimes (ADR-0024)
//!
//! A request that times out, or whose caller walks away, stops waiting —
//! it does not stop the `DuckDB` bind or scan its permit is paying for.
//! Every resource that work holds (the permit, the publication guard, the
//! executor and the interrupt registration) therefore belongs to
//! [`WorkSlot`], which the request future hands to the blocking task and
//! never owns again. Timeout, caller drop, a refusal before startup, a
//! panic and ordinary completion all converge on that one `Drop`.
//!
//! Between the request's outcome and that cleanup the work is *retained*:
//! it still occupies a permit that `pool_active` counts, and
//! [`retained`](ExecutorPool::retained) is the subset an operator can see.
//! One [`Registry`] mutex covers the interrupt slots, the retained entries
//! and the idle executors together, because the invariant that matters is
//! a cross-map one: an interrupt may only fire while the executor it was
//! taken from is still out on the query it belongs to.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use parking_lot::Mutex;
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use trawl_engine::cancel::CancelLatch;
use trawl_engine::executor::Executor;
use trawl_engine::value::QueryResult;

use crate::deadline::Deadline;
use crate::error::{CAPACITY_NOT_STARTED, ServerError};
use crate::hot_buffer::HotBuffer;
use crate::source::compute_source;

/// Test-only delay injected into the blocking query task so timeout and
/// cancellation tests can reliably win the `select!`/cancel race. Zero means
/// no delay. Compiled for unit tests and, via the `test-support` feature,
/// for this crate's own integration tests — no production consumer enables
/// that feature, so release builds never carry it.
#[cfg(any(test, feature = "test-support"))]
pub static TEST_QUERY_DELAY_MS: AtomicU64 = AtomicU64::new(0);

/// The whole budget one health probe gets: queue wait plus the `SELECT 1`
/// (ADR-0024).
///
/// `/health` is unauthenticated and unthrottled, so its cost under a busy
/// pool has to be a fast unhealthy answer, not a request that waits as
/// long as a query would.
const PING_BUDGET: Duration = Duration::from_millis(250);

/// The longest an autocomplete sample waits to GET a permit and the
/// publication guard, together (ADR-0024).
///
/// Its own overall deadline still caps the whole call: this is the
/// smaller of the two, so a slow acquisition fails the sample instead of
/// eating a query-length budget before it reads anything.
const SAMPLE_ACQUIRE_BUDGET: Duration = Duration::from_secs(1);

/// Which door a unit of pool work came through.
///
/// Presentation and authorization metadata only: every kind takes the same
/// permit, the same executor and the same cleanup. `Ping` and `Sample` are
/// the helper lanes — they hold capacity like any query, so they are in
/// the registry and in the retained count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkKind {
    /// `POST /api/v1/query`.
    Query,
    /// A `| from saved` query, reading a recorded run's parquet.
    FromSaved,
    /// `POST /api/v1/export`, any format.
    Export,
    /// A scheduled report run.
    Scheduled,
    /// The health route's `SELECT 1` probe.
    Ping,
    /// Autocomplete's distinct-value sample.
    Sample,
}

impl WorkKind {
    /// The stable wire/log spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::FromSaved => "from_saved",
            Self::Export => "export",
            Self::Scheduled => "scheduled",
            Self::Ping => "ping",
            Self::Sample => "sample",
        }
    }
}

/// Whose authority a unit of work runs under.
///
/// The fleet keystore id, never a name: names are mutable and non-unique,
/// and this is the anchor non-admin cancellation is authorized against.
/// It stays inside the server — no route puts an owner id on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkOwner {
    /// The key that submitted the request.
    Key(i64),
    /// The server itself: scheduled runs, health probes. No human owner,
    /// admin-cancellable.
    System,
}

/// The lifecycle identity of one unit of pool work.
///
/// Clone rather than Copy: it carries the submitter's display name, which
/// the retained listing shows a reader entitled to see it. A lane that
/// needs the kind past the point it hands this over keeps a copy of
/// [`WorkKind`], which is Copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkContext {
    pub kind: WorkKind,
    pub owner: WorkOwner,
    /// The submitting key's display name. Display metadata only — never
    /// the anchor for an authorization decision, which is
    /// [`WorkOwner::Key`]'s id, because names are mutable and non-unique.
    user: Option<Arc<str>>,
}

impl WorkContext {
    /// Work submitted by an authenticated key, which retains that key's id.
    #[must_use]
    pub fn key(kind: WorkKind, key_id: i64) -> Self {
        Self {
            kind,
            owner: WorkOwner::Key(key_id),
            user: None,
        }
    }

    /// Work the server started for itself.
    #[must_use]
    pub fn system(kind: WorkKind) -> Self {
        Self {
            kind,
            owner: WorkOwner::System,
            user: None,
        }
    }

    /// Record the submitter's display name for the retained listing.
    #[must_use]
    pub fn with_user(mut self, name: &str) -> Self {
        self.user = Some(Arc::from(name));
        self
    }
}

/// One registered query's cancellation state.
///
/// The slot exists from registration — before the blocking task runs, let
/// alone binds — so a cancellation arriving before the handle does is
/// latched rather than lost, and the worker refuses to start on it.
struct InterruptSlot {
    /// `DuckDB`'s interrupt handle, once the worker published it.
    handle: Option<Arc<duckdb::InterruptHandle>>,
    /// Cancellation was requested. Never cleared: a repeat cancellation is
    /// accepted while the work exists, and an acknowledgement means
    /// requested, not stopped.
    ///
    /// Written only inside the registry lock, so the decisions that read
    /// it here — publish the handle, start the work — stay serialized
    /// against invocation. It is an atomic because the executor reads the
    /// same flag from its blocking thread at the bind-to-execute boundary
    /// ([`WorkSlot::cancel_latch`]), where taking the registry lock would
    /// put a `DuckDB` bind inside it.
    cancelled: Arc<AtomicBool>,
}

/// The accounting half of a registered unit of work.
struct RetainedEntry {
    id: u64,
    kind: WorkKind,
    owner: WorkOwner,
    /// The submitting key's display name, for the retained listing.
    user: Option<Arc<str>>,
    /// The DSL, for the operator-facing retained listing. Filtered by
    /// owner and permission at the route, never by presence here; helper
    /// lanes carry no DSL at all.
    display: Option<String>,
    /// The worker passed its work-start transition. A held permit alone
    /// does not prove this.
    started_work: bool,
    registered_at: Instant,
    /// When the request recorded its outcome and left the work running.
    /// `None` while a request is still waiting on it.
    retained_since: Option<Instant>,
}

/// Everything the pool must decide atomically about work in flight.
///
/// One mutex, not three: the proof that a stale interrupt cannot hit a
/// reused connection is that invocation and deregistration-plus-re-idle
/// are the same critical section. Split the maps and that ordering
/// becomes a hope. Critical sections here are map operations only — no
/// awaits, no query execution.
struct Registry {
    interrupts: HashMap<u64, InterruptSlot>,
    retained: HashMap<u64, RetainedEntry>,
    idle: Vec<Executor>,
}

impl Registry {
    /// Latch cancellation for one unit of work and interrupt it if it has
    /// published a handle.
    ///
    /// The slot is deliberately kept: cancellation is a request, not a
    /// stop, so a repeat cancellation must keep succeeding while the work
    /// exists, and the worker still has to read the latch at its
    /// work-start transition. Only [`WorkSlot::drop`] removes a slot, in
    /// this same critical section — which is why an interrupt can never
    /// reach the next query to take that executor.
    fn request_cancel(&mut self, id: u64) -> bool {
        let Some(slot) = self.interrupts.get_mut(&id) else {
            return false;
        };
        slot.cancelled.store(true, Ordering::SeqCst);
        if let Some(handle) = &slot.handle {
            handle.interrupt();
        }
        true
    }

    fn retained_count(&self) -> usize {
        self.retained
            .values()
            .filter(|entry| entry.retained_since.is_some())
            .count()
    }
}

/// Why a worker refused to start its physical work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartRefusal {
    /// The budget was gone before any work began.
    Expired,
    /// Cancellation was latched before any work began.
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

/// A unit of retained physical work, as an operator sees it.
///
/// `owner`, `user` and `display` stay crate-internal: which reader may
/// see the submitter or the DSL is the retained-listing route's decision
/// ([`crate::handlers::retained_snapshot`]), and an owner id never
/// reaches the wire at all. Cancellation authorization does not read this
/// type; it reads the registry directly, through
/// [`ExecutorPool::owner_of`].
#[derive(Debug, Clone)]
pub struct RetainedWork {
    pub id: u64,
    pub kind: WorkKind,
    /// Whether the work passed its work-start transition.
    pub started: bool,
    /// How long the work has outlived its request.
    pub retained: Duration,
    pub(crate) owner: WorkOwner,
    pub(crate) user: Option<Arc<str>>,
    pub(crate) display: Option<String>,
}

/// Every resource one unit of physical work holds, released together.
///
/// Constructed before dispatch and moved into the blocking closure, so the
/// request future cannot own the last reference: a timeout, a dropped
/// caller, a refusal at startup, a panic and a clean finish all reach the
/// same `Drop`.
struct WorkSlot {
    registry: Arc<Mutex<Registry>>,
    id: u64,
    kind: WorkKind,
    /// Shared with the request future, written only inside the registry
    /// lock. It outlives the registry entry, so a request that finds the
    /// entry already gone still classifies its own outcome correctly.
    started: Arc<AtomicBool>,
    /// The registry's cancellation flag for this work, shared so the
    /// executor can read it without the registry lock
    /// ([`WorkSlot::cancel_latch`]).
    cancelled: Arc<AtomicBool>,
    executor: Option<Executor>,
    publication: Option<tokio::sync::OwnedRwLockReadGuard<()>>,
    permit: Option<OwnedSemaphorePermit>,
}

impl WorkSlot {
    /// The executor this work runs on. It never leaves the slot, so no
    /// panic anywhere in the worker can lose it from the pool.
    fn executor(&self) -> &Executor {
        self.executor
            .as_ref()
            .expect("the executor leaves the slot only in Drop")
    }

    /// Hand `DuckDB`'s interrupt handle to the registry.
    ///
    /// Returns false when cancellation was already latched — the caller
    /// asked for this work to stop before it could be interrupted, so the
    /// worker must not start it. Publishing under the same lock the
    /// latch is written in is what makes that answer final.
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

    /// The flag the executor reads at its bind-to-execute boundary.
    ///
    /// `DuckDB`'s interrupt is the first half of stopping work and cannot
    /// be the whole of it: an interrupt raised while a statement is
    /// binding may be swallowed, and binding is where an expensive query
    /// spends its time (ADR-0024). Handing the same flag
    /// [`Registry::request_cancel`] sets to the engine gives that
    /// cancellation a second, reliable place to land — after the bind,
    /// before execution — without a lock on the blocking thread's path.
    fn cancel_latch(&self) -> CancelLatch {
        CancelLatch::new(Arc::clone(&self.cancelled))
    }

    /// The work-start transition: the moment a permit becomes physical
    /// work (ADR-0024).
    ///
    /// Runs inside the blocking closure before source discovery or any
    /// other physical work, and decides 503-vs-504 for the whole request:
    /// a refusal here means nothing ran, so the answer is a capacity
    /// refusal; past it an expiry is the ordinary query timeout. Both
    /// halves are read and the flag is written in one critical section,
    /// so the classification never depends on which side of a `select!`
    /// was polled first.
    ///
    /// Every lane has a budget, helpers included: ping brings its own
    /// [`PING_BUDGET`] one, so there is no escape hatch here for work
    /// that could start after its caller stopped waiting.
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
        let retained_for = {
            let mut registry = self.registry.lock();
            registry.interrupts.remove(&self.id);
            let entry = registry.retained.remove(&self.id);
            if let Some(executor) = self.executor.take() {
                registry.idle.push(executor);
            }
            entry.and_then(|entry| entry.retained_since.map(|since| since.elapsed()))
        };
        // (b) then the publication guard, (c) then the permit: a
        // compaction publisher or a cutover waiting on either must not
        // observe capacity it cannot use.
        drop(self.publication.take());
        drop(self.permit.take());
        // (d) Content-free: which work, what kind, how long it outlived
        // its request. Only work that was actually retained logs — the
        // pair with `query_permit_retained`.
        if let Some(retained_for) = retained_for {
            tracing::info!(
                event_type = "query_permit_reclaimed",
                query_id = self.id,
                kind = self.kind.as_str(),
                retained_ms = u64::try_from(retained_for.as_millis()).unwrap_or(u64::MAX),
                "retained query permit reclaimed"
            );
        }
    }
}

/// The request side of one unit of work, which may stop waiting before
/// the work stops running.
///
/// Armed for as long as the request future is the one waiting. Dropping it
/// while armed is a caller that walked away: the work is asked to stop and
/// its permit is booked as retained, exactly as a timeout does, because
/// nothing else will run on that path — the request future is already
/// gone.
struct RequestGuard<'a> {
    pool: &'a ExecutorPool,
    id: u64,
    kind: WorkKind,
    started: Arc<AtomicBool>,
    armed: bool,
}

impl<'a> RequestGuard<'a> {
    fn new(pool: &'a ExecutorPool, id: u64, kind: WorkKind, started: &Arc<AtomicBool>) -> Self {
        Self {
            pool,
            id,
            kind,
            started: Arc::clone(started),
            armed: true,
        }
    }

    /// The work finished first: there is nothing to retain.
    fn disarm(&mut self) {
        self.armed = false;
    }

    /// Stop waiting on purpose. Returns whether the work had started,
    /// which is the caller's 503-vs-504 classification.
    fn abandon(&mut self) -> bool {
        self.armed = false;
        self.pool.cancel_by_id(self.id);
        self.pool.mark_request_finished(self.id, &self.started)
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
        self.pool.mark_request_finished(self.id, &self.started);
    }
}

/// The one answer a request gets for work that never started, told once
/// (ADR-0024).
///
/// A capacity refusal, not a timeout: nothing ran, so 503 rather than 504,
/// and no timeout history row. The client-facing text is fixed and says
/// nothing about the query — whether the budget ran out or a cancellation
/// beat the start is an operator fact, and it goes to the log.
///
/// One refused request can reach two of these: the request future gives
/// up on its budget, which latches cancellation, and the worker it
/// released then refuses to start on that latch. Both are the same
/// refusal, so the FIRST one to arrive logs and names the reason —
/// whichever actually happened first — and the second is silent. Without
/// this, one refusal wrote two `query_not_started` events with
/// contradictory reasons, and an operator counting them counted twice.
#[derive(Clone)]
struct RefusalOnce(Arc<AtomicBool>);

impl RefusalOnce {
    fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    /// The error, logged if nothing has logged this work's refusal yet.
    fn refuse(&self, id: u64, kind: WorkKind, refusal: StartRefusal) -> ServerError {
        if !self.0.swap(true, Ordering::SeqCst) {
            tracing::info!(
                event_type = "query_not_started",
                query_id = id,
                kind = kind.as_str(),
                reason = refusal.as_str(),
                "query work refused before it started"
            );
        }
        ServerError::ServiceUnavailable(CAPACITY_NOT_STARTED.to_owned())
    }

    fn refuse_outcome(&self, id: u64, kind: WorkKind, refusal: StartRefusal) -> ExecuteOutcome {
        ExecuteOutcome {
            result: Err(self.refuse(id, kind, refusal)),
            debug: None,
            severity_columns: Vec::new(),
        }
    }
}

/// Deterministic control over the blocking worker, for tests only.
///
/// The lifecycle races this module has to get right — a request expiring
/// exactly as its work starts, a cancellation arriving before the interrupt
/// handle exists, a completion landing on the same instant as a timeout —
/// are all orderings between an async task and a blocking thread. Proving
/// one with a sleep proves it on one machine on one day. These gates let a
/// test park the worker at a named point, observe that it arrived, and
/// release it, so the ordering is stated rather than raced for.
///
/// Compiled for this crate's unit tests and, via the `test-support`
/// feature, its integration tests. No production consumer enables it.
#[cfg(any(test, feature = "test-support"))]
pub mod seam {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex as StdMutex};

    /// A point in the blocking worker a test can hold or fail.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum Seam {
        /// First statement of the worker, before the interrupt handle is
        /// published and before the work-start transition.
        Entry,
        /// Immediately after a successful work-start transition, before
        /// source discovery.
        Started,
        /// After the work produced its answer, before cleanup.
        Finished,
    }

    /// A held (or merely watched) worker seam.
    #[derive(Debug)]
    pub struct Gate {
        open: StdMutex<bool>,
        opened: Condvar,
        arrivals: AtomicUsize,
        panics: bool,
    }

    impl Gate {
        /// How many workers have reached this seam.
        #[must_use]
        pub fn arrivals(&self) -> usize {
            self.arrivals.load(Ordering::SeqCst)
        }

        /// Let every parked worker through, and every later one straight
        /// past.
        pub fn release(&self) {
            *self.open.lock().expect("seam gate poisoned") = true;
            self.opened.notify_all();
        }

        fn pass(&self) {
            self.arrivals.fetch_add(1, Ordering::SeqCst);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
            let mut open = self.open.lock().expect("seam gate poisoned");
            while !*open {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                assert!(!left.is_zero(), "a worker sat at a seam nobody released");
                open = self
                    .opened
                    .wait_timeout(open, left)
                    .expect("seam gate poisoned")
                    .0;
            }
            drop(open);
            assert!(!self.panics, "test-injected panic at a worker seam");
        }
    }

    /// The seams of ONE pool.
    ///
    /// Per pool, not per process: gates used to live in a global table,
    /// so a hold installed by one test parked the workers of every other
    /// pool alive in the same process — a cross-test panic or a 20-second
    /// stall under the plain `cargo test` harness, which runs tests in
    /// threads rather than nextest's separate processes. A pool's clones
    /// share its table, because they are the same pool.
    #[derive(Debug, Default)]
    pub struct Table {
        gates: StdMutex<HashMap<Seam, Arc<Gate>>>,
    }

    impl Table {
        fn install(&self, seam: Seam, gate: Gate) -> Arc<Gate> {
            let gate = Arc::new(gate);
            self.gates
                .lock()
                .expect("seam table poisoned")
                .insert(seam, Arc::clone(&gate));
            gate
        }

        /// Park every worker of this pool that reaches `seam`, until the
        /// gate is released.
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

        /// Count arrivals at `seam` without holding anything.
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

        /// Panic the worker when it reaches `seam`.
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

        /// The worker's side: free unless this pool's test installed
        /// something.
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

        /// Let every parked worker out and forget every gate.
        pub fn release_all(&self) {
            let gates = std::mem::take(&mut *self.gates.lock().expect("seam table poisoned"));
            for gate in gates.values() {
                gate.release();
            }
        }
    }

    /// A test's handle on one pool's seams, released on drop.
    ///
    /// Dropping releases before forgetting, so a test that fails while a
    /// worker is parked still lets that worker out: the runtime's own
    /// drop waits for blocking tasks, and a gate nobody opens would turn
    /// a failed assertion into a hung test process.
    #[derive(Debug)]
    pub struct Session(pub(crate) Arc<Table>);

    impl Session {
        /// Park every worker that reaches `seam`.
        #[must_use]
        pub fn hold(&self, seam: Seam) -> Arc<Gate> {
            self.0.hold(seam)
        }

        /// Count arrivals at `seam` without holding anything.
        #[must_use]
        pub fn watch(&self, seam: Seam) -> Arc<Gate> {
            self.0.watch(seam)
        }

        /// Panic the worker when it reaches `seam`.
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

/// The worker's seam call, compiled away entirely in a release build.
macro_rules! worker_seam {
    ($seams:expr, $seam:ident) => {
        #[cfg(any(test, feature = "test-support"))]
        {
            $seams.reach(crate::pool::seam::Seam::$seam);
        }
    };
}

/// Debug info captured from the pool's blocking execution path.
///
/// Only populated when `capture_debug` is true (i.e. query log is active).
#[derive(Debug)]
pub struct PoolDebugInfo {
    /// The computed source argument passed to `read_parquet()`.
    pub computed_source: String,
    /// Number of glob patterns in the source list.
    pub glob_count: usize,
    /// Service name extracted from the DSL, if any.
    pub service_filter: Option<String>,
    /// Time filter duration in seconds, if any.
    pub time_filter_secs: Option<u64>,
    /// Hot buffer status: "disabled", "empty", or "active".
    pub hot_status: &'static str,
    /// Hot buffer event count at snapshot time.
    pub hot_events: usize,
    /// Hot buffer batch count at snapshot time.
    pub hot_batches: usize,
    /// Hot buffer estimated byte size at snapshot time.
    pub hot_bytes: usize,
    /// Generated SQL (parameterized).
    pub sql: String,
    /// SQL parameter values (Display form).
    pub params: Vec<String>,
    /// Time spent waiting for pool permit (ms).
    pub pool_wait_ms: u64,
}

/// Exclusive hold over the whole executor pool (see
/// [`ExecutorPool::exclusive`]). Dropping it releases every permit at once.
#[derive(Debug)]
pub struct PoolExclusive {
    _permits: tokio::sync::OwnedSemaphorePermit,
}

/// Query result paired with optional debug info.
#[derive(Debug)]
pub struct ExecuteOutcome {
    /// The query result (success or error).
    pub result: Result<QueryResult, ServerError>,
    /// Debug info, populated only when `capture_debug` was true.
    pub debug: Option<PoolDebugInfo>,
    /// Result columns carrying the `SEVERITY` pin at the end of the
    /// pipeline, which the human-facing renderers display as `OTel` tokens
    /// (ADR-0013 ruling 9). [`severity_columns_for`] computes it inside the
    /// permit-holding blocking task, under the catalog snapshot the run
    /// executed with, so a repin landing between execution and the response
    /// can neither token-render `BIGINT` rows nor strip tokens from rows a
    /// `SEVERITY` pin produced. Empty for a failed, timed-out or
    /// never-started run: there are no rows to present.
    pub severity_columns: Vec<String>,
}

/// The result columns that carry the `SEVERITY` pin at the end of the
/// pipeline, which the human-facing renderers display as `OTel` tokens
/// (ADR-0013 ruling 9).
///
/// `pins` is the root scope of the walk, and it must be the snapshot the
/// run executed under, never a fresh read: `repin --to severity` can retype
/// a field mid-flight, so a later snapshot would token-render rows produced
/// under a `BIGINT`/`VARCHAR` pin, or strip tokens from rows a `SEVERITY`
/// pin produced.
///
/// Presentation only, so an unparseable query (execution reports the parse
/// error) and an empty catalog both answer "nothing", never an error.
///
/// Call it on the blocking pool, inside the query's permit, never on a
/// reactor thread: the parse is a second one (the executor already parsed
/// and emitted) and [`trawl_core::pin_scope::PinScope::advance`] compiles a
/// `Regex` per `extract` stage purely to enumerate capture names. That cost
/// is the client's choice of DSL, so it belongs where `max_concurrent`
/// bounds it and the query timeout covers it, beside the identical walk the
/// emitter already performs there. Only a pipe stage can move a pin off the
/// name it was pinned under (`sev()` included), and a pipe stage needs a
/// `|`, so a DSL without one is answered from the root scope directly. A
/// `|` inside a quoted literal or a regex pays the parse and answers
/// identically: the shortcut only ever errs toward the walk.
pub(crate) fn severity_columns_for(
    pins: &trawl_core::schema::FieldTypes,
    dsl: &str,
) -> Vec<String> {
    let root = trawl_core::pin_scope::PinScope::root(pins);
    if !dsl.contains('|') {
        return trawl_core::pin_scope::severity_output_columns(&[], &root);
    }
    let Ok(query) = trawl_core::parser::parse(dsl) else {
        return Vec::new();
    };
    trawl_core::pin_scope::severity_output_columns(&query.pipeline, &root)
}

/// Pool that bounds concurrent `DuckDB` query execution.
///
/// Executors are pre-created at startup and reused across queries.
/// Each executor holds a connection to the same in-memory `DuckDB`
/// database, sharing cached metadata.
#[derive(Clone)]
pub struct ExecutorPool {
    publication: Arc<crate::publication::PublicationGate>,
    /// Base directory for parquet data (e.g. `/var/lib/trawl/data`).
    base_dir: Arc<str>,
    /// Full recursive glob for queries without a time filter.
    fallback_glob: Arc<str>,
    semaphore: Arc<Semaphore>,
    max_concurrent: usize,
    max_result_rows: usize,
    /// Monotonic ID counter for tracking active query handles.
    next_id: Arc<AtomicU64>,
    /// Interrupt slots, retained-work accounting and the idle executors,
    /// under one lock (see [`Registry`]).
    registry: Arc<Mutex<Registry>>,
    /// Hot buffer for fresh events not yet compacted to parquet.
    hot_buffer: Option<Arc<HotBuffer>>,
    /// Field catalog whose full pin snapshot types every query's
    /// search-stage comparisons. Defaults to an empty catalog; the server
    /// wires the shared cache via [`Self::with_field_catalog`].
    field_catalog: Arc<crate::catalog::FieldCatalog>,
    /// This pool's worker seams (see [`seam`]). Per pool, so a test
    /// holding one pool's workers cannot park another's.
    #[cfg(any(test, feature = "test-support"))]
    seams: Arc<seam::Table>,
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

/// Run a query with panic recovery and optional hot buffer union.
///
/// The executor is borrowed from the caller's [`WorkSlot`] and never
/// moves: the slot owns it through every exit, so no panic on this thread
/// can shrink the pool.
///
/// Called inside `spawn_blocking` — all I/O here is synchronous.
#[allow(clippy::too_many_arguments)]
fn run_query_blocking(
    executor: &Executor,
    cancel: &CancelLatch,
    dsl: &str,
    source: &str,
    hot_buffer: Option<&Arc<HotBuffer>>,
    pins: &trawl_core::schema::FieldTypes,
    max_result_rows: usize,
    utc_offset_secs: i32,
    capture_debug: bool,
    pool_wait_ms: u64,
) -> (Result<QueryResult, ServerError>, Option<PoolDebugInfo>) {
    // Snapshot hot buffer to a temp ndjson file so fresh events
    // are visible to this query via UNION ALL BY NAME. Returns a
    // cached file when the buffer hasn't changed since the last snapshot;
    // the snapshot also carries the catalog pins (∩ observed keys) the
    // executor conforms the hot branch with.
    let hot_snapshot = hot_buffer.and_then(|hb| hb.snapshot());

    // Filter out hot files whose paths aren't valid UTF-8 (required by
    // DuckDB's file reader). This is extremely unlikely on any modern OS
    // but avoids a panic in production.
    let hot_snapshot = hot_snapshot.and_then(|s| {
        if s.path().to_str().is_some() {
            Some(s)
        } else {
            tracing::warn!("hot buffer temp file path is not valid UTF-8, skipping hot source");
            None
        }
    });

    // Capture debug info if requested (query log is active).
    let debug = if capture_debug {
        Some(capture_pool_debug(
            dsl,
            source,
            hot_buffer,
            pins,
            pool_wait_ms,
        ))
    } else {
        None
    };

    // catch_unwind ensures the executor is always returned to the
    // pool even if DuckDB panics (e.g. corrupt parquet file).
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Some(ref hot) = hot_snapshot {
            // Safety: we verified UTF-8 validity above.
            let hot_path = hot.path().to_str().unwrap_or_default();
            executor
                .cancellable(cancel)
                .run_query_with_hot(
                    dsl,
                    source,
                    hot_path,
                    &hot.field_types,
                    pins,
                    max_result_rows,
                    utc_offset_secs,
                )
                .map_err(ServerError::from)
        } else {
            executor
                .cancellable(cancel)
                .run_query(dsl, source, pins, max_result_rows, utc_offset_secs)
                .map_err(ServerError::from)
        }
    }));
    // hot_snapshot drops here → temp file auto-deleted
    let result = match result {
        Ok(r) => r,
        Err(payload) => Err(ServerError::Internal(format!(
            "query panicked: {}",
            panic_text(payload.as_ref())
        ))),
    };

    (result, debug)
}

/// The message a caught panic carried, for an internal-error string.
fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload.downcast_ref::<&str>().map_or_else(
        || {
            payload
                .downcast_ref::<String>()
                .cloned()
                .unwrap_or_else(|| "unknown panic".to_owned())
        },
        |s| (*s).to_owned(),
    )
}

/// Capture debug info about source selection, hot buffer state, and SQL generation.
///
/// This is cheap (re-parse + emit is <1ms) and only runs when the query log is active.
fn capture_pool_debug(
    dsl: &str,
    source: &str,
    hot_buffer: Option<&Arc<HotBuffer>>,
    pins: &trawl_core::schema::FieldTypes,
    pool_wait_ms: u64,
) -> PoolDebugInfo {
    let glob_count = if source.starts_with('[') {
        source.matches(',').count() + 1
    } else {
        1
    };
    // Re-parse AST to extract filters and emit SQL (cheap, <1ms).
    let (service_filter, time_filter_secs, sql, params) =
        if let Ok(ast) = trawl_core::parser::parse(dsl) {
            let service = ast.search.groups.first().and_then(|g| {
                g.iter().find_map(|t| {
                    if let trawl_core::ast::SearchToken::FieldFilter(trawl_core::ast::FieldFilter {
                        field,
                        op: trawl_core::ast::FilterOp::Eq,
                        value: trawl_core::ast::FilterValue::Literal(s),
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
            // it renders the planner's source, and the executor may narrow a
            // list source's elements (dropping ones no file backs) before it
            // reads, so the logged source can be wider than the one that ran.
            // Its `now()` anchor is its own for the same reason: a preview is
            // not the run, and there is no run's anchor to inherit here.
            let (sql, params) = match trawl_core::emitter::emit_with_pins(
                &ast,
                source,
                pins,
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

    PoolDebugInfo {
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
        pool_wait_ms,
    }
}

impl ExecutorPool {
    /// Publication interlock shared with compaction and rollup.
    #[must_use]
    pub fn publication(&self) -> Arc<crate::publication::PublicationGate> {
        Arc::clone(&self.publication)
    }

    /// Create a pool with the given concurrency limit and base data directory.
    ///
    /// Pre-creates `max_concurrent` executors sharing the same in-memory
    /// `DuckDB` database. Panics if the database cannot be initialized
    /// (fatal at startup — the server cannot function without `DuckDB`).
    pub fn new(
        base_dir: String,
        max_concurrent: usize,
        max_result_rows: usize,
        hot_buffer: Option<Arc<HotBuffer>>,
    ) -> Self {
        let root = Executor::new().expect("failed to create DuckDB connection at startup");
        let mut executors = Vec::with_capacity(max_concurrent);
        for _ in 1..max_concurrent {
            executors.push(
                root.try_clone()
                    .expect("failed to clone DuckDB connection at startup"),
            );
        }
        executors.push(root);

        let fallback_glob: Arc<str> =
            Arc::from(format!("{}/**/*.parquet", base_dir.trim_end_matches('/')));

        let publication = hot_buffer.as_ref().map_or_else(
            || Arc::new(crate::publication::PublicationGate::new()),
            |buffer| buffer.publication(),
        );
        publication.initialize(std::path::Path::new(&base_dir));
        Self {
            publication,
            base_dir: Arc::from(base_dir),
            fallback_glob,
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            max_concurrent,
            max_result_rows,
            next_id: Arc::new(AtomicU64::new(0)),
            registry: Arc::new(Mutex::new(Registry {
                interrupts: HashMap::new(),
                retained: HashMap::new(),
                idle: executors,
            })),
            hot_buffer,
            field_catalog: Arc::new(crate::catalog::FieldCatalog::new()),
            #[cfg(any(test, feature = "test-support"))]
            seams: Arc::new(seam::Table::default()),
        }
    }

    /// Take this pool's worker seams for one test (see [`seam`]).
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn seams(&self) -> seam::Session {
        seam::Session(Arc::clone(&self.seams))
    }

    /// Attach the shared field-catalog pin cache. Builder-style, mirroring
    /// `HotBuffer::with_field_catalog`, so the test call sites that need no
    /// catalog stay on `new`.
    #[must_use]
    pub fn with_field_catalog(mut self, catalog: Arc<crate::catalog::FieldCatalog>) -> Self {
        self.field_catalog = catalog;
        self
    }

    /// Register one unit of physical work and give it everything it holds.
    ///
    /// The permit is already in hand, so an executor is guaranteed: every
    /// executor is either idle or inside a [`WorkSlot`] whose `Drop`
    /// returns it before releasing its permit, and that `Drop` runs on
    /// completion, timeout, caller drop, refusal and panic alike. An empty
    /// pool here is therefore a broken invariant, not pressure — this
    /// deliberately does not mint a replacement connection, which would
    /// silently enlarge the pool past `max_concurrent` and hand the
    /// cutover's `exclusive()` a false exclusivity.
    ///
    /// `display` is the work's DSL, kept for the operator-facing retained
    /// listing; the helper lanes have none.
    fn begin_slot(
        &self,
        id: u64,
        work: &WorkContext,
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
            return Err(ServerError::Internal("executor pool inconsistent".into()));
        };
        let cancelled = Arc::new(AtomicBool::new(false));
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
                user: work.user.clone(),
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
        })
    }

    /// The request stopped waiting while its work may still be running.
    ///
    /// Stamps the moment the permit became *retained* — held by work no
    /// request is waiting on any more — and answers whether that work had
    /// started, which is the request's 503-vs-504 classification. Reading
    /// the flag the work-start transition wrote is what keeps that answer
    /// out of the hands of `select!`'s polling order.
    fn mark_request_finished(&self, id: u64, started: &AtomicBool) -> bool {
        let entry = {
            let mut registry = self.registry.lock();
            match registry.retained.get_mut(&id) {
                Some(entry) if entry.retained_since.is_none() => {
                    entry.retained_since = Some(Instant::now());
                    Some((
                        entry.kind,
                        entry.started_work,
                        entry.registered_at.elapsed(),
                    ))
                }
                _ => None,
            }
        };
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
        started.load(Ordering::SeqCst)
    }

    /// Whose work this is, while it is registered.
    ///
    /// The query tracker forgets a query the moment its request records an
    /// outcome, but the physical work can outlive that (ADR-0024). This
    /// record lasts until cleanup, so the key that submitted a query it
    /// has already been told timed out can still ask for it to stop.
    /// `None` once cleanup ran, or for an id that never existed — callers
    /// read that as "not yours".
    pub(crate) fn owner_of(&self, id: u64) -> Option<WorkOwner> {
        self.registry
            .lock()
            .retained
            .get(&id)
            .map(|entry| entry.owner)
    }

    /// How many permits are held by work no request is waiting on.
    #[must_use]
    pub fn retained(&self) -> usize {
        self.registry.lock().retained_count()
    }

    /// The retained work an operator can be shown, newest state as of now.
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

    /// Allocate a query id from the pool's counter.
    ///
    /// This is the single id authority: callers pass the returned id to
    /// [`execute`](Self::execute) / [`execute_with_source`](Self::execute_with_source) /
    /// [`export_parquet`](Self::export_parquet), and — when the query is
    /// user-visible — to `QueryTracker::start`, so `/queries` listings and
    /// [`cancel_by_id`](Self::cancel_by_id) speak the same id space.
    pub fn allocate_query_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Execute a DSL query, blocking on semaphore acquisition if at capacity.
    ///
    /// `query_id` must come from [`allocate_query_id`](Self::allocate_query_id);
    /// it keys the interrupt slot [`cancel_by_id`](Self::cancel_by_id) latches
    /// and the lifecycle entry [`retained_work`](Self::retained_work) lists.
    ///
    /// `deadline` is the caller's whole budget, stamped once at handler
    /// entry (ADR-0024). Every wait this lane performs — the queue, the
    /// publication gate, execution itself — spends from that one instant,
    /// so a query that queued for most of it does not then get a full
    /// timeout to run in.
    ///
    /// Past the deadline the request answers and stops waiting; the work
    /// does not stop with it. It is interrupted, and it keeps its permit,
    /// its publication guard and its executor until it actually ends —
    /// [`retained`](Self::retained) counts it meanwhile. Whether the answer
    /// is a 504 timeout or a 503 capacity refusal is decided by the
    /// worker's own work-start transition, not by this future.
    ///
    /// When `capture_debug` is true, captures source selection, hot buffer
    /// state, and generated SQL for the query debug log.
    ///
    /// `utc_offset_secs` is applied to all timestamp values in the result.
    #[allow(clippy::too_many_lines)]
    pub async fn execute(
        &self,
        query_id: u64,
        dsl: &str,
        deadline: Deadline,
        capture_debug: bool,
        utc_offset_secs: i32,
        work: WorkContext,
    ) -> ExecuteOutcome {
        let kind = work.kind;
        // One refusal, told once, however this work ends up refused.
        let refusal_log = RefusalOnce::new();
        #[cfg(any(test, feature = "test-support"))]
        let seams = Arc::clone(&self.seams);
        let available = self.semaphore.available_permits();
        if available == 0 {
            tracing::warn!(
                event_type = "pool_pressure",
                max_concurrent = self.max_concurrent,
                "executor pool at capacity, query queued"
            );
        }

        // The queue wait spends the caller's budget like every other wait
        // (ADR-0024): it used to be measured for the log line only, so a
        // query could sit here for minutes and still be handed a whole
        // fresh timeout to run in.
        let wait_start = std::time::Instant::now();
        let semaphore = Arc::clone(&self.semaphore);
        let permit = match deadline.run(semaphore.acquire_owned()).await {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => {
                return ExecuteOutcome {
                    result: Err(ServerError::Internal("executor pool shut down".into())),
                    debug: None,
                    severity_columns: Vec::new(),
                };
            }
            Err(crate::deadline::Expired) => {
                return refusal_log.refuse_outcome(query_id, kind, StartRefusal::Expired);
            }
        };

        let wait_ms = wait_start.elapsed().as_millis();
        #[allow(clippy::cast_possible_truncation)]
        let pool_wait_ms = wait_ms as u64;
        tracing::info!(
            event_type = "pool_acquired",
            wait_ms,
            available = self.semaphore.available_permits(),
            "semaphore permit acquired"
        );

        let publication = match deadline.run(self.publication.read()).await {
            Ok(Ok(guard)) => guard,
            Ok(Err(error)) => {
                return ExecuteOutcome {
                    result: Err(error),
                    debug: None,
                    severity_columns: Vec::new(),
                };
            }
            Err(crate::deadline::Expired) => {
                return refusal_log.refuse_outcome(query_id, kind, StartRefusal::Expired);
            }
        };

        // Everything this work holds, in one guard, moved into the
        // blocking task below: from here on the request future cannot be
        // the thing that releases a permit or returns an executor.
        let slot = match self.begin_slot(query_id, &work, Some(dsl), permit, Some(publication)) {
            Ok(slot) => slot,
            Err(error) => {
                return ExecuteOutcome {
                    result: Err(error),
                    debug: None,
                    severity_columns: Vec::new(),
                };
            }
        };
        let started = Arc::clone(&slot.started);

        let dsl = dsl.to_owned();
        let base_dir = Arc::clone(&self.base_dir);
        let max_result_rows = self.max_result_rows;
        let hot_buffer = self.hot_buffer.clone();
        let field_catalog = Arc::clone(&self.field_catalog);

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
                    worker_seam!(seams, Started);

                    // Test-only: sleep before the query so timeout/cancellation tests
                    // can reliably win the race against spawn_blocking.
                    #[cfg(any(test, feature = "test-support"))]
                    {
                        let delay = TEST_QUERY_DELAY_MS.load(Ordering::Relaxed);
                        if delay > 0 {
                            std::thread::sleep(std::time::Duration::from_millis(delay));
                        }
                    }

                    let source = compute_source(&base_dir, &dsl);
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

                    // One catalog snapshot per query: every retry inside the
                    // executor sees the same comparison pins, and the presentation
                    // metadata computed beside the rows is rooted in that same
                    // snapshot.
                    let pins = field_catalog.all();
                    let (result, debug) = run_query_blocking(
                        slot.executor(),
                        &slot.cancel_latch(),
                        &dsl,
                        &source,
                        hot_buffer.as_ref(),
                        &pins,
                        max_result_rows,
                        utc_offset_secs,
                        capture_debug,
                        pool_wait_ms,
                    );
                    // Inside the permit, on the blocking pool: the walk compiles a
                    // regex per `extract` stage, so it may not run on a reactor
                    // thread (see `severity_columns_for`).
                    let severity_columns = if result.is_ok() {
                        severity_columns_for(&pins, &dsl)
                    } else {
                        Vec::new()
                    };
                    worker_seam!(seams, Finished);
                    ExecuteOutcome {
                        result,
                        debug,
                        severity_columns,
                    }
                }));
                outcome.unwrap_or_else(|payload| ExecuteOutcome {
                    result: Err(ServerError::Internal(format!(
                        "query worker panicked: {}",
                        panic_text(payload.as_ref())
                    ))),
                    debug: None,
                    severity_columns: Vec::new(),
                })
                // `slot` drops here, on this thread: interrupt deregistered,
                // executor re-idled, publication and permit released.
            }
        });

        // A caller that walks away (client gone, task aborted) is not a
        // reason to abandon the work: the guard latches cancellation and
        // records the retention on the way out.
        let mut request = RequestGuard::new(self, query_id, kind, &started);
        tokio::select! {
            join_result = &mut task => {
                request.disarm();
                match join_result {
                    Ok(outcome) => outcome,
                    Err(e) => ExecuteOutcome {
                        result: Err(ServerError::Internal(format!("query task panicked: {e}"))),
                        debug: None,
                        severity_columns: Vec::new(),
                    },
                }
            }
            // The budget is gone. Interrupt, hand the work over to its own
            // cleanup, and answer with what the work-start transition
            // recorded: a timeout if it ran, a capacity refusal if the
            // permit never became work.
            () = tokio::time::sleep_until(deadline.instant()) => {
                let result = if request.abandon() {
                    Err(ServerError::Timeout)
                } else {
                    Err(refusal_log.refuse(query_id, kind, StartRefusal::Expired))
                };
                ExecuteOutcome { result, debug: None, severity_columns: Vec::new() }
            }
        }
    }

    /// Execute a DSL query with a pre-computed source, skipping glob computation.
    ///
    /// `query_id` must come from [`allocate_query_id`](Self::allocate_query_id).
    ///
    /// Used for `| from saved` queries where the source is a `read_parquet()`
    /// expression pointing at scheduled result files, not the normal data dir.
    /// The hot buffer is intentionally skipped — saved query results are
    /// self-contained parquet files.
    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    pub async fn execute_with_source(
        &self,
        query_id: u64,
        dsl: &str,
        source: &str,
        deadline: Deadline,
        capture_debug: bool,
        utc_offset_secs: i32,
        work: WorkContext,
    ) -> ExecuteOutcome {
        let kind = work.kind;
        // One refusal, told once, however this work ends up refused.
        let refusal_log = RefusalOnce::new();
        #[cfg(any(test, feature = "test-support"))]
        let seams = Arc::clone(&self.seams);
        let available = self.semaphore.available_permits();
        if available == 0 {
            tracing::warn!(
                event_type = "pool_pressure",
                max_concurrent = self.max_concurrent,
                "executor pool at capacity, query queued"
            );
        }

        // Same budget rule as the ordinary lane. This one takes no
        // publication guard (ADR-0024 keeps per-lane gate membership as
        // it is), so the queue and execution are the whole of its wait.
        let wait_start = std::time::Instant::now();
        let semaphore = Arc::clone(&self.semaphore);
        let permit = match deadline.run(semaphore.acquire_owned()).await {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => {
                return ExecuteOutcome {
                    result: Err(ServerError::Internal("executor pool shut down".into())),
                    debug: None,
                    severity_columns: Vec::new(),
                };
            }
            Err(crate::deadline::Expired) => {
                return refusal_log.refuse_outcome(query_id, kind, StartRefusal::Expired);
            }
        };

        let wait_ms = wait_start.elapsed().as_millis();
        #[allow(clippy::cast_possible_truncation)]
        let pool_wait_ms = wait_ms as u64;
        tracing::info!(
            event_type = "pool_acquired",
            wait_ms,
            available = self.semaphore.available_permits(),
            "semaphore permit acquired (from saved)"
        );

        let slot = match self.begin_slot(query_id, &work, Some(dsl), permit, None) {
            Ok(slot) => slot,
            Err(error) => {
                return ExecuteOutcome {
                    result: Err(error),
                    debug: None,
                    severity_columns: Vec::new(),
                };
            }
        };
        let started = Arc::clone(&slot.started);

        let dsl = dsl.to_owned();
        let source = source.to_owned();
        let max_result_rows = self.max_result_rows;
        let field_catalog = Arc::clone(&self.field_catalog);

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

                    // No hot buffer — saved query results are self-contained.
                    let pins = field_catalog.all();
                    let (result, debug) = run_query_blocking(
                        slot.executor(),
                        &slot.cancel_latch(),
                        &dsl,
                        &source,
                        None,
                        &pins,
                        max_result_rows,
                        utc_offset_secs,
                        capture_debug,
                        pool_wait_ms,
                    );
                    // `dsl` here is what follows `| from saved`, whose `PinScope`
                    // rule clears the scope, so the presentation walk roots in an
                    // empty catalog: a saved run's stored columns are not typed by
                    // what this corpus happens to pin now. The comparison pins above
                    // are a separate question and keep the live snapshot.
                    let severity_columns = if result.is_ok() {
                        severity_columns_for(&trawl_core::schema::FieldTypes::new(), &dsl)
                    } else {
                        Vec::new()
                    };
                    worker_seam!(seams, Finished);
                    ExecuteOutcome {
                        result,
                        debug,
                        severity_columns,
                    }
                }));
                outcome.unwrap_or_else(|payload| ExecuteOutcome {
                    result: Err(ServerError::Internal(format!(
                        "query worker panicked: {}",
                        panic_text(payload.as_ref())
                    ))),
                    debug: None,
                    severity_columns: Vec::new(),
                })
            }
        });

        let mut request = RequestGuard::new(self, query_id, kind, &started);
        tokio::select! {
            join_result = &mut task => {
                request.disarm();
                match join_result {
                    Ok(outcome) => outcome,
                    Err(e) => ExecuteOutcome {
                        result: Err(ServerError::Internal(format!("query task panicked: {e}"))),
                        debug: None,
                        severity_columns: Vec::new(),
                    },
                }
            }
            () = tokio::time::sleep_until(deadline.instant()) => {
                let result = if request.abandon() {
                    Err(ServerError::Timeout)
                } else {
                    Err(refusal_log.refuse(query_id, kind, StartRefusal::Expired))
                };
                ExecuteOutcome { result, debug: None, severity_columns: Vec::new() }
            }
        }
    }

    /// Acquire every permit: the repin cutover's exclusion primitive.
    ///
    /// Every lane that can read parquet funnels through this semaphore
    /// (queries, `from saved`, exports, value sampling, ping), and each lane
    /// computes its source and snapshots its comparison pins inside the
    /// permit-holding task, so holding all permits means no query can
    /// straddle the per-env swap or the pin flip. SSE streams hold no
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
                // Name what is in the way. A permit held by work whose
                // request already answered looks identical from here to a
                // permit held by a query someone is waiting on, and the
                // two call for different operator moves: one is a query
                // to wait out, the other is work to cancel by id
                // (ADR-0024). Counts only — no ids, no DSL.
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

    /// Interrupt all currently executing queries. Called during shutdown
    /// to cancel in-flight `DuckDB` operations before draining connections.
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

    /// Request cancellation of one unit of work by id.
    ///
    /// True means the work exists and cancellation is now latched — not
    /// that anything has stopped. It stays true for repeat requests while
    /// the work exists, including after its request already timed out, and
    /// covers the window before the worker publishes an interrupt handle:
    /// the latch is what makes that worker refuse to start.
    pub fn cancel_by_id(&self, query_id: u64) -> bool {
        self.registry.lock().request_cancel(query_id)
    }

    pub fn max_result_rows(&self) -> usize {
        self.max_result_rows
    }

    pub fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
    }

    /// The total pool capacity, i.e. the max concurrent queries.
    pub fn capacity(&self) -> usize {
        self.max_concurrent
    }

    pub fn base_dir(&self) -> &str {
        &self.base_dir
    }

    pub fn fallback_glob(&self) -> &Arc<str> {
        &self.fallback_glob
    }

    /// Lightweight health check: acquire a permit, grab an executor, run
    /// `SELECT 1`, and return it. Proves the pool and `DuckDB` are functional.
    ///
    /// A permit-holding lane like any other, so it registers in the
    /// lifecycle registry and its capacity shows up in the retained count
    /// if the probe outlives its caller.
    ///
    /// It carries its own [`PING_BUDGET`] deadline, covering the queue
    /// wait AND the probe together (ADR-0024). `/health` is
    /// unauthenticated and unthrottled, so a pool full of slow queries
    /// must cost it a fast unhealthy answer rather than a hung request
    /// per prober — and a probe that gives up is exactly like any other
    /// abandoned request: it keeps its permit until the work stops, and
    /// the failure lands on this probe's own record and nothing else.
    pub async fn ping(&self) -> Result<(), ServerError> {
        let deadline = Deadline::after(PING_BUDGET);
        let semaphore = Arc::clone(&self.semaphore);
        let id = self.allocate_query_id();
        let work = WorkContext::system(WorkKind::Ping);
        let kind = work.kind;
        // One refusal, told once, however this work ends up refused.
        let refusal_log = RefusalOnce::new();
        #[cfg(any(test, feature = "test-support"))]
        let seams = Arc::clone(&self.seams);
        let permit = match deadline.run(semaphore.acquire_owned()).await {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => return Err(ServerError::Internal("executor pool shut down".into())),
            Err(crate::deadline::Expired) => {
                return Err(refusal_log.refuse(id, kind, StartRefusal::Expired));
            }
        };

        let slot = self.begin_slot(id, &work, None, permit, None)?;
        let started = Arc::clone(&slot.started);

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
                    let result = slot.executor().ping().map_err(ServerError::from);
                    worker_seam!(seams, Finished);
                    result
                }));
                outcome.unwrap_or_else(|_| Err(ServerError::Internal("ping panicked".into())))
            }
        });

        let mut request = RequestGuard::new(self, id, kind, &started);
        tokio::select! {
            joined = &mut task => {
                request.disarm();
                joined.map_err(|e| ServerError::Internal(format!("ping task panicked: {e}")))?
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

    /// Sample distinct values of one field for autocomplete.
    ///
    /// A parquet-reading lane like any other, so it funnels through the
    /// same semaphore [`exclusive`](Self::exclusive) drains, and, like the
    /// query lanes, expands its glob inside the permit-holding task, so it
    /// cannot list paths before the repin cutover's per-env swap and read
    /// them after.
    ///
    /// `service`, when present, scopes the glob to one service's files; the
    /// caller is responsible for validating the name.
    /// `deadline` is the caller's whole budget: the queue wait and the
    /// publication wait both spend from it.
    pub async fn sample_field_values(
        &self,
        field: &str,
        service: Option<&str>,
        limit: usize,
        deadline: Deadline,
        work: WorkContext,
    ) -> Result<Vec<String>, ServerError> {
        let kind = work.kind;
        // One refusal, told once, however this work ends up refused.
        let refusal_log = RefusalOnce::new();
        #[cfg(any(test, feature = "test-support"))]
        let seams = Arc::clone(&self.seams);
        let semaphore = Arc::clone(&self.semaphore);
        let id = self.allocate_query_id();

        // ONE acquisition budget across both gates, not one each: two
        // second-long allowances would let a sample spend two seconds
        // before reading anything. It is also never longer than what is
        // left of the caller's own deadline.
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

        // Past acquisition the caller's overall deadline governs again.
        let slot = self.begin_slot(id, &work, None, permit, Some(publication))?;
        let started = Arc::clone(&slot.started);
        let fallback_glob = Arc::clone(&self.fallback_glob);
        let field = field.to_owned();
        let service = service.map(ToOwned::to_owned);

        let task = tokio::task::spawn_blocking({
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
                        Some(svc) => {
                            let base = fallback_glob.as_ref();
                            let base_prefix = base.find('*').map_or(base, |pos| &base[..pos]);
                            format!("{base_prefix}**/{svc}.parquet")
                        }
                        None => fallback_glob.to_string(),
                    };
                    let result = slot
                        .executor()
                        .sample_field_values(&glob, &field, limit)
                        .map_err(ServerError::from);
                    worker_seam!(seams, Finished);
                    result
                }));
                outcome.unwrap_or_else(|_| {
                    Err(ServerError::Internal("field values sample panicked".into()))
                })
            }
        });

        let mut request = RequestGuard::new(self, id, kind, &started);
        let result = task
            .await
            .map_err(|e| ServerError::Internal(format!("field values task panicked: {e}")));
        request.disarm();
        result?
    }

    /// Export query results to Parquet via `DuckDB` `COPY TO`.
    ///
    /// `query_id` must come from [`allocate_query_id`](Self::allocate_query_id).
    ///
    /// Acquires a pool executor, writes to a temp file, and returns the
    /// raw bytes. Respects the hot buffer for fresh event visibility.
    ///
    /// The same lifecycle as [`execute`](Self::execute): the budget covers
    /// every wait up to bytes in hand, and an export that outlives its
    /// request keeps its permit until the `COPY TO` actually stops.
    #[allow(clippy::too_many_lines)]
    pub async fn export_parquet(
        &self,
        query_id: u64,
        dsl: &str,
        max_rows: usize,
        deadline: Deadline,
        work: WorkContext,
    ) -> Result<Vec<u8>, ServerError> {
        let kind = work.kind;
        // One refusal, told once, however this work ends up refused.
        let refusal_log = RefusalOnce::new();
        #[cfg(any(test, feature = "test-support"))]
        let seams = Arc::clone(&self.seams);
        let semaphore = Arc::clone(&self.semaphore);
        let permit = deadline
            .run(semaphore.acquire_owned())
            .await
            .map_err(|_| refusal_log.refuse(query_id, kind, StartRefusal::Expired))?
            .map_err(|_| ServerError::Internal("executor pool shut down".into()))?;

        let publication = deadline
            .run(self.publication.read())
            .await
            .map_err(|_| refusal_log.refuse(query_id, kind, StartRefusal::Expired))??;

        let slot = self.begin_slot(query_id, &work, Some(dsl), permit, Some(publication))?;
        let started = Arc::clone(&slot.started);
        let dsl = dsl.to_owned();
        let base_dir = Arc::clone(&self.base_dir);
        let hot_buffer = self.hot_buffer.clone();
        let field_catalog = Arc::clone(&self.field_catalog);

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
                    worker_seam!(seams, Started);

                    let source = compute_source(&base_dir, &dsl);

                    // Write to a temp file, then read it back as bytes.
                    let tmp = tempfile::NamedTempFile::new().map_err(|e| {
                        ServerError::Internal(format!("failed to create temp file: {e}"))
                    })?;
                    let tmp_path = tmp.path().to_owned();

                    // Snapshot hot buffer for fresh events.
                    let hot_snapshot = hot_buffer
                        .as_ref()
                        .and_then(|hb| hb.snapshot())
                        .filter(|s| s.path().to_str().is_some());

                    let pins = field_catalog.all();
                    let cancel = slot.cancel_latch();
                    let executor = slot.executor().cancellable(&cancel);
                    let written = if let Some(ref hot) = hot_snapshot {
                        let hot_path = hot.path().to_str().unwrap_or_default();
                        executor.export_parquet_with_hot(
                            &dsl,
                            &source,
                            hot_path,
                            &hot.field_types,
                            &pins,
                            &tmp_path,
                            max_rows,
                        )
                    } else {
                        executor.export_parquet(&dsl, &source, &pins, &tmp_path, max_rows)
                    }
                    .map_err(ServerError::from);

                    let bytes = written.and_then(|()| {
                        std::fs::read(&tmp_path).map_err(|e| {
                            ServerError::Internal(format!("failed to read parquet temp file: {e}"))
                        })
                    });
                    worker_seam!(seams, Finished);
                    bytes
                }));
                outcome.unwrap_or_else(|payload| {
                    Err(ServerError::Internal(format!(
                        "export panicked: {}",
                        panic_text(payload.as_ref())
                    )))
                })
            }
        });

        let mut request = RequestGuard::new(self, query_id, kind, &started);
        tokio::select! {
            join_result = &mut task => {
                request.disarm();
                match join_result {
                    Ok(result) => result,
                    Err(e) => Err(ServerError::Internal(format!("export task panicked: {e}"))),
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

#[cfg(test)]
mod tests {
    use super::*;
    use seam::Seam;

    /// Ordinary interactive work, owned by a key.
    const TEST_WORK: WorkContext = WorkContext {
        kind: WorkKind::Query,
        owner: WorkOwner::Key(7),
        user: None,
    };
    const TEST_SAMPLE_WORK: WorkContext = WorkContext {
        kind: WorkKind::Sample,
        owner: WorkOwner::System,
        user: None,
    };

    fn idle_len(pool: &ExecutorPool) -> usize {
        pool.registry.lock().idle.len()
    }

    /// Nothing is registered: no interrupt slot, no lifecycle entry.
    fn registry_is_empty(pool: &ExecutorPool) -> bool {
        let registry = pool.registry.lock();
        registry.interrupts.is_empty() && registry.retained.is_empty()
    }

    /// A pool whose queries answer from a one-event hot buffer, so an
    /// ordinary query succeeds without a parquet fixture.
    fn hot_pool(max_concurrent: usize) -> ExecutorPool {
        use crate::bus::IngestBatch;
        use crate::hot_buffer::HotBufferConfig;

        let hot = Arc::new(HotBuffer::new(HotBufferConfig {
            max_events: 100,
            max_bytes: 1 << 20,
        }));
        let mut event = serde_json::Map::new();
        event.insert("service".into(), "svc".into());
        event.insert("message".into(), "hello".into());
        hot.insert(Arc::new(IngestBatch {
            batch_id: "b1".into(),
            service: "svc".into(),
            byte_size: 64,
            events: vec![event],
        }));
        ExecutorPool::new("/nonexistent".into(), max_concurrent, 100_000, Some(hot))
    }

    /// Wait for a blocking thread to reach a state, without letting paused
    /// time move.
    ///
    /// Tokio auto-advances a paused clock only when the scheduler has
    /// nothing ready, so a yield loop pins virtual time exactly where the
    /// test put it while the worker thread makes real progress. The bound
    /// is the real clock, which is not what any assertion here is about —
    /// it only turns a deadlock into a failure instead of a hang.
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

    /// Captured `tracing` events for ONE unit of work.
    ///
    /// A global subscriber, not a thread-local one: the events these
    /// tests are about are emitted from the blocking worker thread, which
    /// does not inherit `set_default`. The sink is set for one capturing
    /// test at a time and keeps only the events carrying that test's own
    /// `query_id`, so a concurrent test's lifecycle logging cannot be
    /// counted here.
    mod capture {
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex as StdMutex, OnceLock};

        type Events = Arc<StdMutex<Vec<HashMap<String, String>>>>;

        struct Sink {
            query_id: u64,
            events: Events,
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
                    && fields.get("query_id").map(String::as_str)
                        == Some(sink.query_id.to_string().as_str())
                {
                    sink.events.lock().expect("capture poisoned").push(fields);
                }
            }
        }

        /// The events one unit of work logged while this guard lived.
        pub(super) struct Capture {
            events: Events,
            _turn: tokio::sync::MutexGuard<'static, ()>,
        }

        impl Capture {
            /// Every captured field map whose `event_type` matches.
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

        /// Capture what `query_id` logs until the guard drops.
        pub(super) async fn of_query(query_id: u64) -> Capture {
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
                query_id,
                events: Arc::clone(&events),
            });
            Capture {
                events,
                _turn: turn,
            }
        }
    }

    /// Let every spawned task poll before the clock moves.
    ///
    /// A task registers its timers on its first poll, so advancing paused
    /// time before that would place a deadline the test then never
    /// reaches — and a paused runtime with a blocking thread in flight
    /// does not reliably auto-advance to it either.
    async fn settle() {
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
    }

    /// The severity presentation metadata is a property of the snapshot the
    /// run executed under, so a repin landing between execution and the
    /// response cannot retype the rows it already produced: the same DSL
    /// answers differently for a `SEVERITY` snapshot and a `BIGINT` one, and
    /// the executing task walks under the snapshot it ran with.
    #[test]
    fn severity_columns_come_from_the_snapshot_it_is_given() {
        use trawl_core::schema::{CanonicalType, FieldTypes};

        let mut executed_under = FieldTypes::new();
        executed_under.insert("sev", CanonicalType::Severity);
        assert_eq!(
            severity_columns_for(&executed_under, "* | table sev"),
            vec!["sev".to_owned()],
            "a column pinned SEVERITY in the run's snapshot renders tokens"
        );

        let mut repinned = FieldTypes::new();
        repinned.insert("sev", CanonicalType::BigInt);
        assert!(
            severity_columns_for(&repinned, "* | table sev").is_empty(),
            "a later BIGINT pin is not the interpretation those rows carry"
        );
    }

    /// The pipe-free shortcut answers exactly what the walk answers: with no
    /// pipeline the terminal scope is the root scope, and a `|` that is only
    /// regex punctuation still takes the walk.
    ///
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
            severity_columns_for(&pins, "service=nginx last=1h"),
            vec!["lvl".to_owned()],
            "a pipe-free query answers from the root scope without a parse"
        );
        assert_eq!(
            severity_columns_for(&pins, "message=/a|b/ last=1h"),
            vec!["lvl".to_owned()],
            "a `|` inside a regex costs the parse and answers the same"
        );
        // A pipeline that drops the column drops the stamp with it, and an
        // unparseable query answers nothing (execution reports the error).
        assert!(
            severity_columns_for(&pins, "* | table status").is_empty(),
            "a projection that leaves the column out names nothing"
        );
        assert!(severity_columns_for(&pins, "| | |").is_empty());
        assert!(severity_columns_for(&FieldTypes::new(), "service=nginx").is_empty());
    }

    /// The stamp rides back on `ExecuteOutcome` from the run's own catalog
    /// snapshot, taken inside the permit-holding blocking task. Repinning
    /// the field between the two runs changes the answer, which is the whole
    /// coherence property: the response describes the pins that produced its
    /// rows, not the catalog as it stands when the JSON is assembled.
    #[tokio::test]
    async fn execute_stamps_severity_columns_from_the_runs_own_snapshot() {
        use crate::bus::IngestBatch;
        use crate::hot_buffer::HotBufferConfig;
        use trawl_core::schema::CanonicalType;

        let catalog = Arc::new(crate::catalog::FieldCatalog::new());
        catalog.repin("lvl", CanonicalType::Severity);

        // Hot-buffer-only corpus: no parquet under the base dir, so the
        // query answers from the hot branch and needs no fixture files.
        let hot = Arc::new(
            HotBuffer::new(HotBufferConfig {
                max_events: 100,
                max_bytes: 1 << 20,
            })
            .with_field_catalog(Arc::clone(&catalog)),
        );
        let mut event = serde_json::Map::new();
        event.insert("service".into(), "svc".into());
        event.insert("lvl".into(), 17.into());
        hot.insert(Arc::new(IngestBatch {
            batch_id: "b1".into(),
            service: "svc".into(),
            byte_size: 64,
            events: vec![event],
        }));

        let pool = ExecutorPool::new("/nonexistent".into(), 2, 100_000, Some(Arc::clone(&hot)))
            .with_field_catalog(Arc::clone(&catalog));

        let outcome = pool
            .execute(
                pool.allocate_query_id(),
                "* | table lvl",
                Deadline::after(Duration::from_secs(30)),
                false,
                0,
                TEST_WORK,
            )
            .await;
        assert!(outcome.result.is_ok(), "{:?}", outcome.result);
        assert_eq!(
            outcome.severity_columns,
            vec!["lvl".to_owned()],
            "the run's own snapshot pins `lvl` SEVERITY, so the answer says so"
        );

        // The operator repins it away. The next run's snapshot is the new
        // one, and its rows are BIGINTs the renderers must not tokenize.
        catalog.repin("lvl", CanonicalType::BigInt);
        let outcome = pool
            .execute(
                pool.allocate_query_id(),
                "* | table lvl",
                Deadline::after(Duration::from_secs(30)),
                false,
                0,
                TEST_WORK,
            )
            .await;
        assert!(outcome.result.is_ok(), "{:?}", outcome.result);
        assert!(
            outcome.severity_columns.is_empty(),
            "a repinned field stops being presented as a severity"
        );
    }

    /// A run that produced no rows carries no presentation metadata: an
    /// error and a timeout both stamp nothing, because there is nothing to
    /// present and the walk's answer would describe a result the caller
    /// never gets.
    #[tokio::test]
    async fn a_failed_run_stamps_no_severity_columns() {
        let catalog = Arc::new(crate::catalog::FieldCatalog::new());
        catalog.repin("lvl", trawl_core::schema::CanonicalType::Severity);
        let pool = ExecutorPool::new("/nonexistent".into(), 2, 100_000, None)
            .with_field_catalog(Arc::clone(&catalog));

        // A parse error: reported by execution, so the walk is skipped.
        let outcome = pool
            .execute(
                pool.allocate_query_id(),
                "| | |",
                Deadline::after(Duration::from_secs(10)),
                false,
                0,
                TEST_WORK,
            )
            .await;
        assert!(outcome.result.is_err(), "{:?}", outcome.result);
        assert!(outcome.severity_columns.is_empty());

        // A timeout: the outcome is assembled on the async side, where no
        // snapshot and no walk exist at all.
        TEST_QUERY_DELAY_MS.store(200, Ordering::Relaxed);
        let outcome = pool
            .execute(
                pool.allocate_query_id(),
                "* | table lvl",
                Deadline::after(Duration::from_millis(10)),
                false,
                0,
                TEST_WORK,
            )
            .await;
        TEST_QUERY_DELAY_MS.store(0, Ordering::Relaxed);
        assert!(matches!(outcome.result, Err(ServerError::Timeout)));
        assert!(outcome.severity_columns.is_empty());
    }

    #[tokio::test]
    async fn pool_rejects_invalid_dsl() {
        let pool = ExecutorPool::new("/nonexistent".into(), 2, 100_000, None);
        // Must start with `|` to trigger a parse error — bare text is valid DSL.
        let outcome = pool
            .execute(
                pool.allocate_query_id(),
                "| | invalid",
                Deadline::after(Duration::from_secs(10)),
                false,
                0,
                TEST_WORK,
            )
            .await;
        assert!(outcome.result.is_err());
    }

    #[tokio::test]
    async fn pool_respects_concurrency_limit() {
        let pool = ExecutorPool::new("/nonexistent".into(), 1, 100_000, None);
        // just verifying it doesn't panic with a single permit
        let _ = pool
            .execute(
                pool.allocate_query_id(),
                "service:test",
                Deadline::after(Duration::from_secs(10)),
                false,
                0,
                TEST_WORK,
            )
            .await;
    }

    #[tokio::test]
    async fn pool_reuses_executors() {
        let pool = ExecutorPool::new("/nonexistent".into(), 2, 100_000, None);

        // Run two sequential queries — both should succeed and the pool
        // should have the same number of idle executors before and after.
        let idle_before = idle_len(&pool);
        let _ = pool
            .execute(
                pool.allocate_query_id(),
                "service:test",
                Deadline::after(Duration::from_secs(10)),
                false,
                0,
                TEST_WORK,
            )
            .await;
        let _ = pool
            .execute(
                pool.allocate_query_id(),
                "service:test",
                Deadline::after(Duration::from_secs(10)),
                false,
                0,
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
        let pool = ExecutorPool::new("/var/lib/trawl/data".into(), 1, 100_000, None);
        assert_eq!(&*pool.fallback_glob, "/var/lib/trawl/data/**/*.parquet");
    }

    #[test]
    fn fallback_glob_strips_trailing_slash() {
        let pool = ExecutorPool::new("/var/lib/trawl/data/".into(), 1, 100_000, None);
        assert_eq!(&*pool.fallback_glob, "/var/lib/trawl/data/**/*.parquet");
    }

    /// Waiting in the queue spends the caller's budget (ADR-0024). The
    /// permit is held for the whole test, so the query never starts: what
    /// ends the wait is the deadline, and before this it was nothing at
    /// all — `acquire_owned` had no bound, and a query that finally got a
    /// permit was handed a full fresh timeout to run in.
    ///
    /// Paused time, so the five seconds are virtual and exact. A test
    /// that slept for them would prove the same thing an hour later.
    ///
    /// Nothing ran, so the answer is a capacity refusal rather than a
    /// query timeout.
    #[tokio::test(start_paused = true)]
    async fn queue_wait_counts_against_the_deadline() {
        let pool = ExecutorPool::new("/nonexistent".into(), 1, 100_000, None);
        let idle_before = idle_len(&pool);
        // The pool's one permit, held by someone else for the duration.
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
                0,
                TEST_WORK,
            )
            .await;
        let waited = start.elapsed();

        assert!(
            matches!(&outcome.result, Err(ServerError::ServiceUnavailable(msg)) if msg == CAPACITY_NOT_STARTED),
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

    /// The publication gate is the second wait, and it spends the same
    /// budget: the permit is acquired, then the cutover's writer holds
    /// the gate until the deadline ends the wait. Still before any work,
    /// so still a capacity refusal.
    #[tokio::test(start_paused = true)]
    async fn publication_wait_counts_against_the_deadline() {
        let pool = ExecutorPool::new("/nonexistent".into(), 1, 100_000, None);
        let idle_before = idle_len(&pool);
        let writer = pool.publication.write().await;

        let start = tokio::time::Instant::now();
        let outcome = pool
            .execute(
                pool.allocate_query_id(),
                "*",
                Deadline::after(Duration::from_secs(5)),
                false,
                0,
                TEST_WORK,
            )
            .await;
        let waited = start.elapsed();

        assert!(
            matches!(&outcome.result, Err(ServerError::ServiceUnavailable(msg)) if msg == CAPACITY_NOT_STARTED),
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
        let pool = ExecutorPool::new("/nonexistent".into(), 1, 100_000, None);
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
                    0,
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
        let pool = ExecutorPool::new("/nonexistent".into(), 1, 100_000, None);
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
                    0,
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

        // Reclamation follows the work, not the request.
        started.release();
        until("the executor comes back", || {
            idle_len(&pool) == idle_before && pool.available_permits() == 1
        })
        .await;
    }

    /// The cutover's exclusion primitive: `exclusive()` holds every permit,
    /// so no query can start while the guard lives, and a wedged in-flight
    /// query bounds out with a timeout instead of starving the cutover
    /// forever.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exclusive_drains_queries_and_blocks_new_ones() {
        let pool = ExecutorPool::new("/nonexistent".into(), 2, 100_000, None);

        // Nothing in flight: exclusivity is immediate.
        let guard = pool
            .exclusive(Duration::from_secs(1))
            .await
            .expect("an idle pool is immediately exclusive");
        assert_eq!(pool.available_permits(), 0, "every permit is held");

        // A query submitted while exclusive must wait, not run.
        let p2 = pool.clone();
        let queued = tokio::spawn(async move {
            p2.execute(
                p2.allocate_query_id(),
                "service:test",
                Deadline::after(Duration::from_secs(10)),
                false,
                0,
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
        // (The DSL result itself is irrelevant; the point is that it ran.)
        let _ = outcome.result;
    }

    /// Value sampling reads parquet, so it is a permit-taking lane like any
    /// other — otherwise it could expand its glob before the cutover's
    /// per-env swap and read after it, sampling a half-swapped corpus.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exclusive_blocks_field_value_sampling() {
        let pool = ExecutorPool::new("/nonexistent".into(), 2, 100_000, None);
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
        let pool = ExecutorPool::new("/nonexistent".into(), 1, 100_000, None);
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
            matches!(&result, Err(ServerError::ServiceUnavailable(msg)) if msg == CAPACITY_NOT_STARTED),
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

    /// A held query permit starves `exclusive()` past its budget: the
    /// bounded timeout must surface as `ServerError::Timeout` (mapped to
    /// the job's `blocked` outcome), never an indefinite wait.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exclusive_times_out_bounded_while_a_query_runs() {
        TEST_QUERY_DELAY_MS.store(300, Ordering::Relaxed);
        let pool = ExecutorPool::new("/nonexistent".into(), 2, 100_000, None);
        let p2 = pool.clone();
        let running = tokio::spawn(async move {
            p2.execute(
                p2.allocate_query_id(),
                "service:test",
                Deadline::after(Duration::from_secs(10)),
                false,
                0,
                TEST_WORK,
            )
            .await
        });
        // Let the query take its permit.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let err = pool
            .exclusive(Duration::from_millis(20))
            .await
            .expect_err("a held permit must bound out");
        assert!(matches!(err, ServerError::Timeout), "got {err:?}");

        TEST_QUERY_DELAY_MS.store(0, Ordering::Relaxed);
        let _ = running.await;
        // And once the query drains, exclusivity succeeds.
        pool.exclusive(Duration::from_secs(2))
            .await
            .expect("drained pool becomes exclusive");
    }

    /// A permit is not work. A worker parked before its work-start
    /// transition holds everything a running query holds, and if the
    /// budget runs out there, the answer is a capacity refusal — nothing
    /// ran, so there is nothing to have timed out (ADR-0024).
    #[tokio::test(start_paused = true)]
    async fn startup_wait_counts_against_the_deadline() {
        let pool = hot_pool(1);
        let seams = pool.seams();
        let entry = seams.hold(Seam::Entry);
        let started = seams.watch(Seam::Started);
        let baseline_idle = idle_len(&pool);
        let id = pool.allocate_query_id();
        let logged = capture::of_query(id).await;

        let submitted = pool.clone();
        let request = tokio::spawn(async move {
            submitted
                .execute(
                    id,
                    "*",
                    Deadline::after(Duration::from_secs(5)),
                    false,
                    0,
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
            matches!(&outcome.result, Err(ServerError::ServiceUnavailable(msg)) if msg == CAPACITY_NOT_STARTED),
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

        // One refusal, one event. The request gave up on its budget and
        // latched cancellation on the way out, and the worker it released
        // then refused to start on that latch — two paths, one refusal,
        // and the reason is the one that actually happened.
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

    /// Work that started and outlived its request is visible as retained
    /// capacity, stays cancellable while it exists, and gives everything
    /// back when it ends.
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
                    0,
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

        // The owner can keep asking while the work exists.
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

    /// A client told its query timed out can still ask for it to stop.
    /// The request's outcome and the work's life are two different things,
    /// so the cancellation record has to outlive the response.
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
                    0,
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

    /// The stale-interrupt hazard: an interrupt for a finished query must
    /// never reach the next query on that executor. Deregistration and
    /// invocation are the same critical section, so either the
    /// cancellation finds the old work (and the executor is still its own)
    /// or it finds nothing at all. Both orders are exercised here.
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
                    0,
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

        // The next query takes that same executor. The old id reaches
        // nothing, and the new query answers normally.
        assert!(!pool.cancel_by_id(first), "the old slot is gone");
        let second = pool.allocate_query_id();
        let outcome = pool
            .execute(
                second,
                "*",
                Deadline::after(Duration::from_secs(60)),
                false,
                0,
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
                    0,
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

    /// A cancellation can arrive before the work has an interrupt handle
    /// to be stopped with. It latches, and the worker refuses to start —
    /// otherwise the answer would be "cancelled" for a query that then ran
    /// to completion anyway.
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
                    0,
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
            matches!(&outcome.result, Err(ServerError::ServiceUnavailable(msg)) if msg == CAPACITY_NOT_STARTED),
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

    /// A cancellation that lands after the work started stops it at the
    /// bind-to-execute boundary (ADR-0024).
    ///
    /// The interrupt handle is published by then, but an interrupt raised
    /// while `DuckDB` is binding may be swallowed — which is exactly the
    /// case the budget exists for, since binding is where an expensive
    /// query spends its time. The latch the pool shares with the executor
    /// is the recovery: the worker reads it once binding is over, and the
    /// request ends cancelled with no rows.
    ///
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
                    0,
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

    /// The start and the expiry can land in either order, and the answer
    /// is exactly one classification either way — because the work-start
    /// transition records it under the registry lock, not because a
    /// `select!` happened to poll one arm first.
    #[tokio::test(start_paused = true)]
    async fn start_and_expiry_race_yields_exactly_one_classification() {
        let pool = hot_pool(2);
        let seams = pool.seams();

        // Expiry first: the worker is still at the door.
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
                    0,
                    TEST_WORK,
                )
                .await
        });
        until("parked", || entry.arrivals() == 1).await;
        tokio::time::advance(Duration::from_secs(6)).await;
        let expiry_first = request.await.expect("the request joins").result;
        entry.release();
        until("cleanup", || pool.retained() == 0).await;

        // Start first: the worker is past the transition when the budget
        // runs out.
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
                    0,
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
            matches!(&expiry_first, Err(ServerError::ServiceUnavailable(msg)) if msg == CAPACITY_NOT_STARTED),
            "expiry before the start is a capacity refusal, got {expiry_first:?}"
        );
        assert!(
            matches!(start_first, Err(ServerError::Timeout)),
            "expiry after the start is a timeout, got {start_first:?}"
        );
    }

    /// A caller that walks away is not a reason to abandon the work it
    /// started: the permit stays booked until the query actually stops,
    /// and it is visible as retained meanwhile.
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
                    0,
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

    /// A panic anywhere in the worker — before the start, in source
    /// discovery, or after the answer is in hand — leaves nothing behind.
    /// The cleanup guard is outside the unwind boundary, so it runs on the
    /// way out of every one of them.
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
                    0,
                    TEST_WORK,
                )
                .await;
            assert!(
                matches!(outcome.result, Err(ServerError::Internal(_))),
                "a panic at {seam_point:?} reports an internal error, got {:?}",
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

    /// A completion landing on the same instant as the timeout produces
    /// one request outcome and one cleanup, never an orphaned slot.
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
                    0,
                    TEST_WORK,
                )
                .await
        });
        // The work is done; only its cleanup is outstanding.
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

    /// Every lane that takes a permit is in the registry under its own
    /// kind and owner: exports and scheduled runs included, not just the
    /// interactive query lane.
    #[tokio::test(start_paused = true)]
    async fn every_lane_registers_its_kind_and_owner() {
        let pool = hot_pool(2);
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
                    0,
                    WorkContext::system(WorkKind::Scheduled),
                )
                .await
        });

        until("both lanes start", || started.arrivals() == 2).await;
        tokio::time::advance(Duration::from_secs(6)).await;
        assert!(matches!(
            export.await.expect("the export joins"),
            Err(ServerError::Timeout)
        ));
        assert!(matches!(
            scheduled.await.expect("the run joins").result,
            Err(ServerError::Timeout)
        ));

        let mut retained = pool.retained_work();
        retained.sort_by_key(|work| work.id);
        assert_eq!(retained.len(), 2);
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

        started.release();
        until("cleanup", || pool.retained() == 0).await;
    }

    /// Nothing accumulates. After a mixed run of every lane the pool is
    /// back exactly where it started.
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
                    0,
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
        pool.ping().await.expect("the pool pings");

        assert_eq!(idle_len(&pool), baseline_idle);
        assert_eq!(pool.available_permits(), baseline_permits);
        assert_eq!(pool.retained(), 0);
        assert!(pool.retained_work().is_empty());
        assert!(registry_is_empty(&pool));
    }

    /// Neither helper lane may wait like a query (ADR-0024).
    ///
    /// The pool's one permit is parked in a worker, so both helpers can
    /// only queue. Ping gives up after its own 250 ms probe budget and
    /// sampling after one second of acquisition, no matter how long the
    /// caller's overall deadline is — and the smaller of the two always
    /// wins, so a 300 ms caller waits 300 ms. Each answers the safe
    /// capacity refusal, and neither leaves anything behind.
    #[tokio::test(start_paused = true)]
    async fn ping_and_sampling_bound_their_wait() {
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
                    0,
                    TEST_WORK,
                )
                .await
        });
        until("the pool's one permit is taken", || parked.arrivals() == 1).await;
        assert_eq!(pool.available_permits(), 0);

        // -- ping: 250 ms, acquisition included --
        let prober = pool.clone();
        let probe = tokio::spawn(async move { prober.ping().await });
        settle().await;
        tokio::time::advance(Duration::from_millis(200)).await;
        settle().await;
        assert!(
            !probe.is_finished(),
            "the probe must still be waiting inside its budget"
        );
        tokio::time::advance(Duration::from_millis(51)).await;
        settle().await;
        let refused = probe
            .await
            .expect("the probe task joins")
            .expect_err("a full pool cannot be pinged");
        assert!(
            matches!(&refused, ServerError::ServiceUnavailable(msg) if msg == CAPACITY_NOT_STARTED),
            "expected the safe capacity refusal, got {refused:?}"
        );

        // -- sampling: one second of acquisition under a long deadline --
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
            matches!(&refused, ServerError::ServiceUnavailable(msg) if msg == CAPACITY_NOT_STARTED),
            "expected the safe capacity refusal, got {refused:?}"
        );

        // -- and a shorter overall deadline is the one that governs --
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
        assert!(short.await.expect("the sample task joins").is_err_and(
            |e| matches!(&e, ServerError::ServiceUnavailable(m) if m == CAPACITY_NOT_STARTED)
        ),);

        // Nothing a refused helper touched is still held: it never got a
        // permit, so it never registered.
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

    /// When the cutover cannot take the pool, the log says what is in the
    /// way — including how much of it is work no request is waiting on
    /// (ADR-0024). "Two permits held" and "two permits held by abandoned
    /// work" call for different moves, and only the second is something an
    /// operator can cancel by id.
    #[tokio::test(start_paused = true)]
    async fn exclusive_names_retained_permits_when_blocked() {
        /// The bounded wait the cutover gives up after.
        const BUDGET: Duration = Duration::from_millis(20);

        use std::sync::Mutex as StdMutex;

        use tracing_subscriber::prelude::*;

        // Its own thread-local capture rather than the shared one: this
        // event is emitted by the caller, on this thread, and it carries
        // no query_id for the shared sink to key on.

        /// One captured event's fields, stringified.
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
                    0,
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

        // The retained reader is inside the publication gate as well as
        // the semaphore, so a publisher cannot slip past a pool that
        // merely looks busy: the cutover's exclusion is both halves.
        let publisher = Arc::clone(&pool.publication);
        let publishing = tokio::spawn(async move { publisher.write().await });
        settle().await;
        assert!(
            !publishing.is_finished(),
            "retained work still holds its publication read guard"
        );

        // Once the retained work stops, both waits clear.
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
        let pool = ExecutorPool::new("/nonexistent".into(), 1, 100_000, None);
        // No active queries — cancel_all should be a no-op.
        pool.cancel_all();
        assert!(pool.registry.lock().interrupts.is_empty());
    }
}
