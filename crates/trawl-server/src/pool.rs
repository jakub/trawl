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
//! executor, the interrupt registration and any charge its caller attached
//! with [`WorkContext::holding`]) therefore belongs to [`WorkSlot`], which
//! the request future hands to the blocking task and never owns again.
//! Timeout, caller drop, a refusal before startup, a panic and ordinary
//! completion all converge on that one `Drop`.
//!
//! Between the request's outcome and that cleanup the work is *retained*:
//! it still occupies a permit that `pool_active` counts, and
//! [`retained`](ExecutorPool::retained) is the subset an operator can see.
//! One [`Registry`] mutex covers the interrupt slots, the retained entries
//! and the idle executors together, because the invariant that matters is
//! a cross-map one: an interrupt may only fire while the executor it was
//! taken from is still out on the query it belongs to.

use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use trawl_engine::cancel::CancelLatch;
use trawl_engine::executor::{Executor, QueryBudgets, RowCap};
use trawl_engine::scratch::{QueryScratch, ScratchLock};
use trawl_engine::timing::{PhaseClock, QueryPhase};
use trawl_engine::value::QueryResult;

use crate::deadline::Deadline;
use crate::error::{CAPACITY_NOT_STARTED, ServerError};
use crate::hot_buffer::{HotBuffer, HotSnapshot};
use crate::query_timing::{Outcome, PhysicalOutcome, QueryTiming, Reclaim, emit_reclaimed};
use crate::source::{compute_source, select, selection_bytes};

/// Test-only delay injected into the blocking query task so timeout and
/// cancellation tests can reliably win the `select!`/cancel race. Zero means
/// no delay. Compiled for unit tests and, via the `test-support` feature,
/// for this crate's own integration tests — no production consumer enables
/// that feature, so release builds never carry it.
#[cfg(any(test, feature = "test-support"))]
pub static TEST_QUERY_DELAY_MS: AtomicU64 = AtomicU64::new(0);

/// Hands each [`ExecutorPool`] its `instance` id.
static NEXT_POOL_INSTANCE: AtomicU64 = AtomicU64::new(0);

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
/// permit, the same executor and the same cleanup. `Sample` is the helper
/// lane — it holds capacity like any query, so it is in the registry and in
/// the retained count.
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
    /// Autocomplete's distinct-value sample.
    Sample,
    /// `POST /api/v1/results`: a query whose result is written once as a
    /// capture (ADR-0057).
    Capture,
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
            Self::Sample => "sample",
            Self::Capture => "capture",
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
    /// The server itself: scheduled runs. No human owner,
    /// admin-cancellable.
    System,
}

/// The lifecycle identity of one unit of pool work.
///
/// Clone rather than Copy: it carries the submitter's display name, which
/// the retained listing shows a reader entitled to see it. A lane that
/// needs the kind past the point it hands this over keeps a copy of
/// [`WorkKind`], which is Copy.
#[derive(Debug, Clone)]
pub struct WorkContext {
    pub kind: WorkKind,
    pub owner: WorkOwner,
    /// The submitting key's display name. Display metadata only — never
    /// the anchor for an authorization decision, which is
    /// [`WorkOwner::Key`]'s id, because names are mutable and non-unique.
    user: Option<Arc<str>>,
    /// The query's timing account (ADR-0046), for the DSL lanes whose
    /// caller keeps one. The pool books its waits, the startup and the
    /// worker's phases on its clock, and settles who writes the account
    /// when the request stops waiting before the work stops running.
    timing: Option<QueryTiming>,
    /// What the caller charged for this work and must stay charged until
    /// the work stops ([`Self::holding`]). Opaque to the pool, which only
    /// drops it.
    holds: Option<Arc<dyn Any + Send + Sync>>,
}

impl WorkContext {
    /// Work submitted by an authenticated key, which retains that key's id.
    #[must_use]
    pub fn key(kind: WorkKind, key_id: i64) -> Self {
        Self {
            kind,
            owner: WorkOwner::Key(key_id),
            user: None,
            timing: None,
            holds: None,
        }
    }

    /// Work the server started for itself.
    #[must_use]
    pub fn system(kind: WorkKind) -> Self {
        Self {
            kind,
            owner: WorkOwner::System,
            user: None,
            timing: None,
            holds: None,
        }
    }

    /// Record the submitter's display name for the retained listing.
    #[must_use]
    pub fn with_user(mut self, name: &str) -> Self {
        self.user = Some(Arc::from(name));
        self
    }

    /// Time this work on `timing`'s account (ADR-0046).
    #[must_use]
    pub fn with_timing(mut self, timing: QueryTiming) -> Self {
        self.timing = Some(timing);
        self
    }

    /// Keep `holds` until this work stops, not until its request does.
    ///
    /// The slot that takes the permit takes `holds` with it and drops it
    /// after the permit, however the work ends (ADR-0024): a capture's
    /// admission slot and quota reservation stay charged for exactly as
    /// long as its executor permit (ADR-0057). Work refused before it took
    /// a permit drops `holds` with this context.
    ///
    /// One value per unit of work: a second call replaces, and so drops,
    /// the first. A clone of this context shares the value, so a caller
    /// that keeps a clone keeps the charge.
    #[must_use]
    pub fn holding(mut self, holds: Arc<dyn Any + Send + Sync>) -> Self {
        self.holds = Some(holds);
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
    /// The query's timing account, when its lane keeps one.
    timing: Option<QueryTiming>,
    /// How the physical work ended, written by the worker in
    /// [`WorkSlot::finish`] before this slot drops, for the reclaim event.
    physical: PhysicalOutcome,
    /// The caller's charges for this work ([`WorkContext::holding`]),
    /// dropped last.
    holds: Option<Arc<dyn Any + Send + Sync>>,
}

/// The clock a slot without a timing account books on: records nothing.
static CLOCK_OFF: PhaseClock = PhaseClock::off();

impl WorkSlot {
    /// The clock this work books its phases on.
    fn clock(&self) -> &PhaseClock {
        self.timing.as_ref().map_or(&CLOCK_OFF, QueryTiming::clock)
    }

    /// The worker passed its work-start transition: `startup` ends, and
    /// the account says the work started.
    ///
    /// Outside [`Self::begin_work`] because that runs under the registry
    /// lock, and the account's lock is never taken inside it.
    fn started_timing(&self) {
        if let Some(timing) = &self.timing {
            timing.clock().exit(QueryPhase::Startup);
            timing.mark_work_started();
        }
    }

    /// The worker's last word, before this slot drops: record how the
    /// physical work ended and close the clock, so its final totals are
    /// on the shared clock before [`Drop`] takes the registry lock. The
    /// clock is the one accumulator (ADR-0046): the handler's complete
    /// account and the reclaim both read it, and no copy of it travels.
    ///
    /// Work that never started discards its `startup`: nothing ran, so
    /// the refusal reports no worker phase. The request may have
    /// discarded it already, which is a no-op here.
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
    /// Every lane has a budget, the sampling helper included, so there is
    /// no escape hatch here for work that could start after its caller
    /// stopped waiting.
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
        // executor. Whether the work was retained, and whether it had
        // started, are read in the same section the request's
        // `mark_request_finished` stamps the retention in: that lock is
        // what decides which side writes the final timing account.
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
        // (d) Content-free: which work, what kind, how long it outlived
        // its request, how it ended. Only work that was actually retained
        // logs — the pair with `query_permit_retained`. The request wrote
        // a partial account when it stopped waiting on started work, so
        // this event carries the final totals the worker closed its clock
        // with; work that never started carries no phases.
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
        // (e) Last, the caller's charges: a capture's admission slot and
        // quota reservation (ADR-0057). They stop being charged when the
        // permit does and never before, so a capture that waits on its
        // key's slot cannot start while this work still holds an executor.
        // Dropping them runs the caller's code, which therefore finds
        // everything this slot held already released.
        drop(self.holds.take());
    }
}

/// What the request learned, under the registry lock, when it stopped
/// waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RequestEnd {
    /// This call stamped the work retained: its slot was still
    /// registered, so its cleanup has yet to run.
    retained: bool,
    /// The work passed its work-start transition.
    started: bool,
}

/// The request side of one unit of work, which may stop waiting before
/// the work stops running.
///
/// Armed for as long as the request future is the one waiting. Dropping it
/// while armed is a caller that walked away: the work is asked to stop and
/// its permit is booked as retained, exactly as a timeout does, because
/// nothing else will run on that path — the request future is already
/// gone.
///
/// While armed it also owns the query's timing account (ADR-0046): a
/// request that stops waiting is the one side that can write the account
/// at once. Disarming hands it back to the handler, which has the result.
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

    /// The work finished first: there is nothing to retain, and the
    /// handler writes the account from the result.
    fn disarm(&mut self) {
        self.armed = false;
        if let Some(timing) = &self.timing {
            timing.hand_emit_back();
        }
    }

    /// Stop waiting on purpose, at the deadline. Returns whether the work
    /// had started, which is the caller's 503-vs-504 classification.
    fn abandon(&mut self) -> bool {
        self.armed = false;
        self.pool.cancel_by_id(self.id);
        let end = self.pool.mark_request_finished(self.id, &self.started);
        self.settle(end, Outcome::Timeout, Outcome::CapacityRefused);
        end.started
    }

    /// Write the timing account for a request that stopped waiting.
    ///
    /// The registry lock already decided it ([`RequestEnd`]), and this
    /// runs after that lock is released:
    ///
    /// - Started and retained: the work is still running, so the account
    ///   is partial — the completed totals and the phase in flight. The
    ///   slot's cleanup reports the final totals on the reclaim.
    /// - Started and not retained: the worker finished and its slot is
    ///   gone, so there is no reclaim coming and the account is complete.
    /// - Never started: nothing ran. The account is complete, with no
    ///   worker phase and `startup` discarded.
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

/// Write the account of work refused before it was dispatched: the
/// permit or the publication wait ran out the budget (ADR-0046). The
/// pool names the refusal where it makes it, so nobody reads it back out
/// of the 503's text.
fn account_refused(timing: Option<&QueryTiming>) {
    if let Some(timing) = timing {
        timing.emit_complete(Outcome::CapacityRefused, Instant::now());
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
struct RefusalOnce {
    logged: Arc<AtomicBool>,
    /// The pool whose capacity was refused, so a reader can tell two
    /// pools' identically numbered work apart.
    pool: u64,
}

impl RefusalOnce {
    fn new(pool: u64) -> Self {
        Self {
            logged: Arc::new(AtomicBool::new(false)),
            pool,
        }
    }

    /// The error, logged if nothing has logged this work's refusal yet.
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
        ServerError::ServiceUnavailable(CAPACITY_NOT_STARTED.to_owned())
    }

    fn refuse_outcome(&self, id: u64, kind: WorkKind, refusal: StartRefusal) -> ExecuteOutcome {
        ExecuteOutcome {
            result: Err(self.refuse(id, kind, refusal)),
            debug: None,
            severity_columns: Vec::new(),
        }
    }

    /// Whether this work was refused. Read by a request whose worker
    /// answered first: only a refusal of the start sets it.
    fn refused(&self) -> bool {
        self.logged.load(Ordering::SeqCst)
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

    /// The payload a [`Table::panic_at`] gate panics with. A distinctive
    /// string, so a test can prove it reached nowhere a client or a log
    /// field can see.
    pub const INJECTED_PANIC_PAYLOAD: &str = "zz-test-injected-panic-payload";

    /// A point in the blocking worker a test can hold or fail.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum Seam {
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

        /// Wait for [`Gate::release`], however long that takes.
        ///
        /// A held worker never gives up on a wall clock (#316): a lapse
        /// would let a starved test's worker leave the seam on its own
        /// and answer before the test drove the state the hold pins.
        /// [`Session`]'s drop releases every gate, displaced ones
        /// included, so a failing test still frees its workers, and the test runner's slow-timeout
        /// bounds a test that never drives the release.
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

        /// Let every parked worker out, including one parked at a
        /// displaced gate, and forget every gate.
        pub fn release_all(&self) {
            let gates = std::mem::take(&mut *self.gates.lock().expect("seam table poisoned"));
            let displaced =
                std::mem::take(&mut *self.displaced.lock().expect("seam table poisoned"));
            for gate in gates.values().chain(&displaced) {
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

/// What a capture reads its stored events from (ADR-0057).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureSource {
    /// A live window `[start, end)`. The worker selects what `/query`
    /// selects for the same text (`source::select`), under the publication
    /// guard, measures the files in it that can hold the window
    /// (`source::selection_bytes`), and refuses a measure above
    /// `max_bytes` with [`ServerError::CaptureSourceTooLarge`] before the
    /// engine runs. The read includes the hot buffer, as `/query`'s does.
    Window {
        /// The resolved window's start, the one spliced into the DSL.
        start: DateTime<Utc>,
        /// The resolved window's exclusive end, the one spliced into the
        /// DSL.
        end: DateTime<Utc>,
        /// `[capture] source_max_bytes`.
        max_bytes: u64,
    },
    /// `| from saved`: a source already resolved to a recorded run's
    /// parquet, read as [`ExecutorPool::execute_with_source`] reads one,
    /// with no hot buffer and no publication guard. It is not measured:
    /// the string is the engine's source argument, and the files behind it
    /// are the resolver's to budget.
    Resolved {
        /// The engine's source argument.
        source: String,
    },
}

/// What a capture's `finish` receives (ADR-0057).
///
/// `finish` runs on the worker thread before the slot releases anything,
/// so the file it writes is written under the permit, on the slot's own
/// executor, inside the publication guard a window holds, and within the
/// work's lifetime as cancellation and the retained listing see it.
#[derive(Debug)]
pub struct CaptureInput<'a> {
    /// Every row the executed pipeline produced under the lane's
    /// [`RowCap`], uncut: `finish` makes the capture's own cut and counts
    /// the total from it.
    pub result: QueryResult,
    /// The slot's executor, for [`Executor::write_capture_parquet`]. Its
    /// interrupt handle is registered, so a cancellation breaks a `COPY` in
    /// flight.
    pub executor: &'a Executor,
    /// The query's clock: book the file write under [`QueryPhase::Write`].
    pub clock: &'a PhaseClock,
    /// The work's cancellation latch. The file write takes none, so read
    /// it before starting one.
    pub cancel: &'a CancelLatch,
    /// The bytes a window's source budget measured; `None` for a resolved
    /// source, which is not measured.
    pub source_bytes: Option<u64>,
    /// The pool's seams, for [`seam::Seam::Writing`].
    #[cfg(any(test, feature = "test-support"))]
    pub seams: &'a seam::Table,
}

/// A capture's last step: the worker runs it inside the slot on the
/// executed result ([`CaptureInput`]), and its answer is the lane's.
pub type CaptureFinish<T> = Box<dyn FnOnce(CaptureInput<'_>) -> Result<T, ServerError> + Send>;

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
/// database, the query database (ADR-0056), sharing cached metadata and
/// its budgets.
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
    /// Which pool instance this is, process-wide.
    ///
    /// A daemon builds exactly one pool, so in production this is always
    /// zero and the field is there to say which pool a lifecycle event
    /// came from. A test process builds many, each with its own id space
    /// starting at zero, so the pair (`instance`, `query_id`) is what
    /// identifies a unit of work when several pools are logging at once.
    instance: u64,
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
    /// A connection to the query database that no permit hands out: the
    /// source of [`Self::detached_executor`] and of the engine sample's
    /// connection.
    clone_source: Arc<Mutex<Executor>>,
    /// The query scratch directory: the query database spills into it, and
    /// hot-buffer snapshots and export staging files are made in it
    /// (ADR-0056). Its lock is not held here but by every connection to
    /// the query database ([`Executor::open_query_database`]), so it stays
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

/// Run a query with panic recovery and optional hot buffer union.
///
/// The executor is borrowed from the caller's [`WorkSlot`] and never
/// moves: the slot owns it through every exit, so no panic on this thread
/// can shrink the pool.
///
/// `snapshotted` runs once the hot snapshot is taken and before the
/// statement, where the capture lane reaches [`seam::Seam::Snapshotted`].
///
/// Called inside `spawn_blocking` — all I/O here is synchronous.
#[allow(clippy::too_many_arguments)]
fn run_query_blocking(
    executor: &Executor,
    cancel: &CancelLatch,
    clock: &PhaseClock,
    dsl: &str,
    source: &str,
    hot: Option<(&Arc<HotBuffer>, &QueryScratch)>,
    pins: &trawl_core::schema::FieldTypes,
    cap: RowCap,
    utc_offset_secs: i32,
    capture_debug: bool,
    snapshotted: impl FnOnce(),
) -> (Result<QueryResult, ServerError>, Option<PoolDebugInfo>) {
    let hot_buffer = hot.map(|(buffer, _)| buffer);
    // Snapshot hot buffer to a temp ndjson file so fresh events
    // are visible to this query via UNION ALL BY NAME. Returns a
    // cached file when the buffer hasn't changed since the last snapshot;
    // the snapshot also carries the catalog pins (∩ observed keys) the
    // executor conforms the hot branch with.
    let hot_snapshot = hot_source(hot.map_or(Ok(None), |(hb, scratch)| {
        clock.time(QueryPhase::HotSnapshot, || hb.snapshot(scratch))
    }));

    // Capture debug info if requested (query log is active). Its
    // re-parse and re-emit are the preview's own emission, booked as such.
    let debug = if capture_debug {
        Some(clock.time(QueryPhase::Emit, || {
            capture_pool_debug(dsl, source, hot_buffer, pins)
        }))
    } else {
        None
    };
    let hot_snapshot = match hot_snapshot {
        Ok(snapshot) => snapshot,
        Err(error) => return (Err(error), debug),
    };
    snapshotted();

    // catch_unwind ensures the executor is always returned to the
    // pool even if DuckDB panics (e.g. corrupt parquet file).
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
                    &hot.field_types,
                    pins,
                    cap,
                    utc_offset_secs,
                )
                .map_err(ServerError::from)
        } else {
            executor
                .cancellable(cancel)
                .timed(clock)
                .run_query(dsl, source, pins, cap, utc_offset_secs)
                .map_err(ServerError::from)
        }
    }));
    // hot_snapshot drops here → temp file auto-deleted
    let result = match result {
        Ok(r) => r,
        // The payload is dropped unread: it can quote anything DuckDB or
        // the executor had in hand (ADR-0040).
        Err(_payload) => Err(ServerError::Panicked("query")),
    };

    (result, debug)
}

/// The hot source of one read, from the hot buffer's snapshot: `None` when
/// there is no buffer or it holds no event.
///
/// A snapshot that could not be built, or whose path `DuckDB`'s file reader
/// cannot take because it is not UTF-8, refuses the read with
/// [`ServerError::HotSnapshot`] (ADR-0041). Parquet alone would answer
/// without the newest events, in steady state as after a restart.
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

/// Capture debug info about source selection, hot buffer state, and SQL generation.
///
/// This is cheap (re-parse + emit is <1ms) and only runs when the query log is active.
fn capture_pool_debug(
    dsl: &str,
    source: &str,
    hot_buffer: Option<&Arc<HotBuffer>>,
    pins: &trawl_core::schema::FieldTypes,
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
    /// Opens the query database in the locked `scratch` directory under
    /// `budgets` ([`Executor::open_query_database`]), then pre-creates
    /// `max_concurrent` executors cloned from it, so every query shares one
    /// spill cap, one memory limit and one thread count (ADR-0056). Every
    /// connection to that database holds `scratch`, so the directory stays
    /// locked for as long as any of them is open.
    ///
    /// # Errors
    /// The database or its spill directory could not be set up, as when the
    /// spill directory is not private, or a budget was refused or read back
    /// differently ([`Executor::open_query_database`]). Fatal at startup:
    /// the server cannot function without `DuckDB`, and the start fails
    /// with this error.
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
                interrupts: HashMap::new(),
                retained: HashMap::new(),
                idle: executors,
            })),
            hot_buffer,
            field_catalog: Arc::new(crate::catalog::FieldCatalog::new()),
            #[cfg(any(test, feature = "test-support"))]
            seams: Arc::new(seam::Table::default()),
            clone_source: Arc::new(Mutex::new(root)),
            scratch: query_scratch,
            #[cfg(any(test, feature = "test-support"))]
            _test_scratch: None,
        })
    }

    /// A pool for tests: [`Self::new`] over a fresh scratch directory in a
    /// temp directory the pool owns, under the default budgets.
    ///
    /// # Errors
    /// As [`Self::new`].
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

    /// [`Self::for_test`] under `budgets`.
    ///
    /// # Errors
    /// As [`Self::new`].
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

    /// The query scratch directory this pool's database spills into.
    #[must_use]
    pub fn scratch(&self) -> &QueryScratch {
        &self.scratch
    }

    /// The connection the engine sample runs on (ADR-0056): cloned from the
    /// query database, outside every permit, and never in the registry.
    ///
    /// # Errors
    /// The clone failed ([`Executor::diagnostic_connection`]).
    pub fn diagnostic_connection(
        &self,
    ) -> Result<trawl_engine::executor::DiagnosticConnection, trawl_engine::error::EngineError>
    {
        self.clone_source.lock().diagnostic_connection()
    }

    /// A connection to the query database that takes no permit.
    ///
    /// For work that reads or writes one stored result outside the query
    /// lanes: the scheduler's result write and the saved-run read. It shares
    /// the database's budgets and spill directory with every query, so it
    /// spills nowhere else and counts against the same caps (ADR-0056), but
    /// it is not interruptible through the pool and holds no permit.
    ///
    /// # Errors
    /// The clone failed ([`Executor::try_clone`]).
    pub fn detached_executor(&self) -> Result<Executor, trawl_engine::error::EngineError> {
        self.clone_source.lock().try_clone()
    }

    /// Take this pool's worker seams for one test (see [`seam`]).
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn seams(&self) -> seam::Session {
        seam::Session(Arc::clone(&self.seams))
    }

    /// The worker's side of [`Self::seams`] for work on a detached
    /// executor, which has no slot to carry them: a captured result's read.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn reach(&self, seam: seam::Seam) {
        self.seams.reach(seam);
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
    ///
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

    /// The request stopped waiting while its work may still be running.
    ///
    /// Stamps the moment the permit became *retained* — held by work no
    /// request is waiting on any more — and answers whether that work had
    /// started, which is the request's 503-vs-504 classification. Reading
    /// the flag the work-start transition wrote is what keeps that answer
    /// out of the hands of `select!`'s polling order.
    ///
    /// Also answers whether this call retained the work — its slot was
    /// still registered — which, with `started`, decides who writes the
    /// timing account: both are read in the critical section
    /// [`WorkSlot::drop`] reads them in.
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
    /// [`execute`](Self::execute) / [`execute_capped`](Self::execute_capped) /
    /// [`execute_with_source`](Self::execute_with_source) /
    /// [`export_parquet`](Self::export_parquet) /
    /// [`capture`](Self::capture), and — when the query is
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
    ///
    /// A result past `max_result_rows` is refused. A lane with a cap of its
    /// own goes through [`execute_capped`](Self::execute_capped).
    pub async fn execute(
        &self,
        query_id: u64,
        dsl: &str,
        deadline: Deadline,
        capture_debug: bool,
        utc_offset_secs: i32,
        work: WorkContext,
    ) -> ExecuteOutcome {
        self.execute_capped(
            query_id,
            dsl,
            RowCap::Refuse(self.max_result_rows),
            deadline,
            capture_debug,
            utc_offset_secs,
            work,
        )
        .await
    }

    /// [`execute`](Self::execute) under the caller's [`RowCap`] rather
    /// than the pool's `max_result_rows`: the CSV and JSON exports, which
    /// cut at their export limit the way the Parquet export does.
    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    pub async fn execute_capped(
        &self,
        query_id: u64,
        dsl: &str,
        cap: RowCap,
        deadline: Deadline,
        capture_debug: bool,
        utc_offset_secs: i32,
        work: WorkContext,
    ) -> ExecuteOutcome {
        let kind = work.kind;
        let timing = work.timing.clone();
        let clock = timing
            .as_ref()
            .map_or_else(PhaseClock::off, |timing| timing.clock().clone());
        // One refusal, told once, however this work ends up refused.
        let refusal_log = RefusalOnce::new(self.instance);
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
        // fresh timeout to run in. The phase guard books the wait however
        // it ends, the deadline and a dropped caller included.
        let semaphore = Arc::clone(&self.semaphore);
        let acquired = {
            let _wait = clock.guard(QueryPhase::PoolWait);
            deadline.run(semaphore.acquire_owned()).await
        };
        let permit = match acquired {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => {
                return ExecuteOutcome {
                    result: Err(ServerError::Internal("executor pool shut down".into())),
                    debug: None,
                    severity_columns: Vec::new(),
                };
            }
            Err(crate::deadline::Expired) => {
                let refused = refusal_log.refuse_outcome(query_id, kind, StartRefusal::Expired);
                account_refused(timing.as_ref());
                return refused;
            }
        };

        let published = {
            let _wait = clock.guard(QueryPhase::PublicationWait);
            deadline.run(self.publication.read()).await
        };
        let publication = match published {
            Ok(Ok(guard)) => guard,
            Ok(Err(error)) => {
                return ExecuteOutcome {
                    result: Err(error),
                    debug: None,
                    severity_columns: Vec::new(),
                };
            }
            Err(crate::deadline::Expired) => {
                let refused = refusal_log.refuse_outcome(query_id, kind, StartRefusal::Expired);
                account_refused(timing.as_ref());
                return refused;
            }
        };

        // Everything this work holds, in one guard, moved into the
        // blocking task below: from here on the request future cannot be
        // the thing that releases a permit or returns an executor.
        let mut slot = match self.begin_slot(query_id, work, Some(dsl), permit, Some(publication)) {
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

                    // Test-only: sleep before the query so timeout/cancellation tests
                    // can reliably win the race against spawn_blocking.
                    #[cfg(any(test, feature = "test-support"))]
                    {
                        let delay = TEST_QUERY_DELAY_MS.load(Ordering::Relaxed);
                        if delay > 0 {
                            std::thread::sleep(std::time::Duration::from_millis(delay));
                        }
                    }

                    let clock = slot.clock();
                    let source = clock.time(QueryPhase::Source, || compute_source(&base_dir, &dsl));
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
                    let pins = clock.time(QueryPhase::Source, || field_catalog.all());
                    let (result, debug) = run_query_blocking(
                        slot.executor(),
                        &slot.cancel_latch(),
                        clock,
                        &dsl,
                        &source,
                        hot_buffer.as_ref().map(|hb| (hb, &scratch)),
                        &pins,
                        cap,
                        utc_offset_secs,
                        capture_debug,
                        || {},
                    );
                    // Inside the permit, on the blocking pool: the walk compiles a
                    // regex per `extract` stage, so it may not run on a reactor
                    // thread (see `severity_columns_for`).
                    let severity_columns = if result.is_ok() {
                        clock.time(QueryPhase::Post, || severity_columns_for(&pins, &dsl))
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
                let outcome = outcome.unwrap_or_else(|_payload| ExecuteOutcome {
                    result: Err(ServerError::Panicked("query worker")),
                    debug: None,
                    severity_columns: Vec::new(),
                });
                slot.finish(&outcome.result);
                // Interrupt deregistered, executor re-idled, publication and
                // permit released — on this thread, before the answer goes
                // back.
                drop(slot);
                worker_seam!(seams, Released);
                outcome
            }
        });

        // A caller that walks away (client gone, task aborted) is not a
        // reason to abandon the work: the guard latches cancellation and
        // records the retention on the way out.
        let mut request = RequestGuard::new(self, query_id, kind, &started, timing.as_ref());
        tokio::select! {
            join_result = &mut task => {
                request.disarm();
                match join_result {
                    Ok(outcome) => {
                        // The worker refused to start: a capacity refusal,
                        // named here because the pool made it.
                        if refusal_log.refused() {
                            account_refused(timing.as_ref());
                        }
                        outcome
                    }
                    Err(e) => ExecuteOutcome {
                        result: Err(ServerError::from_join("query", e)),
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
        let timing = work.timing.clone();
        let clock = timing
            .as_ref()
            .map_or_else(PhaseClock::off, |timing| timing.clock().clone());
        // One refusal, told once, however this work ends up refused.
        let refusal_log = RefusalOnce::new(self.instance);
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
        let semaphore = Arc::clone(&self.semaphore);
        let acquired = {
            let _wait = clock.guard(QueryPhase::PoolWait);
            deadline.run(semaphore.acquire_owned()).await
        };
        let permit = match acquired {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => {
                return ExecuteOutcome {
                    result: Err(ServerError::Internal("executor pool shut down".into())),
                    debug: None,
                    severity_columns: Vec::new(),
                };
            }
            Err(crate::deadline::Expired) => {
                let refused = refusal_log.refuse_outcome(query_id, kind, StartRefusal::Expired);
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

                    // No hot buffer — saved query results are self-contained.
                    let clock = slot.clock();
                    let pins = clock.time(QueryPhase::Source, || field_catalog.all());
                    let (result, debug) = run_query_blocking(
                        slot.executor(),
                        &slot.cancel_latch(),
                        clock,
                        &dsl,
                        &source,
                        None,
                        &pins,
                        cap,
                        utc_offset_secs,
                        capture_debug,
                        || {},
                    );
                    // `dsl` here is what follows `| from saved`, whose `PinScope`
                    // rule clears the scope, so the presentation walk roots in an
                    // empty catalog: a saved run's stored columns are not typed by
                    // what this corpus happens to pin now. The comparison pins above
                    // are a separate question and keep the live snapshot.
                    let severity_columns = if result.is_ok() {
                        clock.time(QueryPhase::Post, || {
                            severity_columns_for(&trawl_core::schema::FieldTypes::new(), &dsl)
                        })
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
                let outcome = outcome.unwrap_or_else(|_payload| ExecuteOutcome {
                    result: Err(ServerError::Panicked("query worker")),
                    debug: None,
                    severity_columns: Vec::new(),
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
        let refusal_log = RefusalOnce::new(self.instance);
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
        let mut slot = self.begin_slot(id, work, None, permit, Some(publication))?;
        let started = Arc::clone(&slot.started);
        let fallback_glob = Arc::clone(&self.fallback_glob);
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
                let result = outcome
                    .unwrap_or_else(|_payload| Err(ServerError::Panicked("field values sample")));
                slot.finish(&result);
                result
            }
        });

        // Acquisition is bounded by its own short budget above, but the
        // sample itself reads parquet and can outrun the caller's deadline
        // just as a query can. Same handover as the query lanes: interrupt,
        // leave the work to its own cleanup, and answer with what the
        // work-start transition recorded.
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
        let timing = work.timing.clone();
        let clock = timing
            .as_ref()
            .map_or_else(PhaseClock::off, |timing| timing.clock().clone());
        // One refusal, told once, however this work ends up refused.
        let refusal_log = RefusalOnce::new(self.instance);
        #[cfg(any(test, feature = "test-support"))]
        let seams = Arc::clone(&self.seams);
        let semaphore = Arc::clone(&self.semaphore);
        let acquired = {
            let _wait = clock.guard(QueryPhase::PoolWait);
            deadline.run(semaphore.acquire_owned()).await
        };
        let permit = acquired
            .map_err(|_| {
                let refused = refusal_log.refuse(query_id, kind, StartRefusal::Expired);
                account_refused(timing.as_ref());
                refused
            })?
            .map_err(|_| ServerError::Internal("executor pool shut down".into()))?;

        let published = {
            let _wait = clock.guard(QueryPhase::PublicationWait);
            deadline.run(self.publication.read()).await
        };
        let publication = published.map_err(|_| {
            let refused = refusal_log.refuse(query_id, kind, StartRefusal::Expired);
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

                    let clock = slot.clock();
                    let source = clock.time(QueryPhase::Source, || compute_source(&base_dir, &dsl));

                    // Write to a staging file in the query scratch
                    // directory (ADR-0056), then read it back as bytes.
                    // The error keeps only its kind: `tempfile` appends the
                    // path it tried, and an internal error's text is logged.
                    let tmp = scratch.export_file().map_err(|e| {
                        ServerError::Internal(format!(
                            "failed to create the export staging file: {}",
                            e.kind()
                        ))
                    })?;
                    let tmp_path = tmp.path().to_owned();

                    // Snapshot hot buffer for fresh events.
                    let hot_snapshot = hot_source(hot_buffer.as_ref().map_or(Ok(None), |hb| {
                        clock.time(QueryPhase::HotSnapshot, || hb.snapshot(&scratch))
                    }))?;

                    let pins = clock.time(QueryPhase::Source, || field_catalog.all());
                    let cancel = slot.cancel_latch();
                    let executor = slot.executor().cancellable(&cancel).timed(clock);
                    let written = if let Some(ref hot) = hot_snapshot {
                        // `hot_source` refused a path that is not UTF-8.
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

                    // Reading the written file back is this format's render.
                    let bytes = written.and_then(|()| {
                        clock
                            .time(QueryPhase::Render, || std::fs::read(&tmp_path))
                            .map_err(|e| {
                                ServerError::Internal(format!(
                                    "failed to read parquet temp file: {e}"
                                ))
                            })
                    });
                    worker_seam!(seams, Finished);
                    bytes
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

    /// Capture a query's result (ADR-0057): execute `dsl` and run `finish`
    /// on the result before the slot releases anything, so the capture's
    /// file is written under the same permit, executor and guard as the
    /// read that produced it.
    ///
    /// `query_id` must come from [`allocate_query_id`](Self::allocate_query_id).
    ///
    /// The lifecycle is [`execute`](Self::execute)'s with `finish` inside
    /// it. `deadline` covers the queue, the publication wait, the source
    /// budget, the execution and `finish` together. Past it the request
    /// answers, and the work, `finish` included, keeps the permit, the
    /// publication guard and `work`'s holds until it stops; its answer is
    /// then dropped. `finish` runs only on a query that succeeded, and a
    /// panic in it answers [`ServerError::Panicked`].
    ///
    /// A [`CaptureSource::Window`] waits for the publication guard and reads
    /// the hot buffer, as `/query` does. Its worker selects and measures the
    /// window's source first: a selection over the budget is refused with
    /// [`ServerError::CaptureSourceTooLarge`] before the hot snapshot is
    /// taken or the engine runs. A [`CaptureSource::Resolved`] source takes
    /// neither the guard nor the hot buffer, as
    /// [`execute_with_source`](Self::execute_with_source) does.
    ///
    /// # Errors
    /// A capacity refusal or a timeout as on the other lanes, the
    /// publication guard's refusal for a window, a source budget refusal, the
    /// query's own error, or whatever `finish` answers.
    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    pub async fn capture<T: Send + 'static>(
        &self,
        query_id: u64,
        dsl: &str,
        source: CaptureSource,
        cap: RowCap,
        deadline: Deadline,
        utc_offset_secs: i32,
        work: WorkContext,
        finish: CaptureFinish<T>,
    ) -> Result<T, ServerError> {
        let kind = work.kind;
        let timing = work.timing.clone();
        let clock = timing
            .as_ref()
            .map_or_else(PhaseClock::off, |timing| timing.clock().clone());
        // One refusal, told once, however this work ends up refused.
        let refusal_log = RefusalOnce::new(self.instance);
        #[cfg(any(test, feature = "test-support"))]
        let seams = Arc::clone(&self.seams);
        let semaphore = Arc::clone(&self.semaphore);
        let acquired = {
            let _wait = clock.guard(QueryPhase::PoolWait);
            deadline.run(semaphore.acquire_owned()).await
        };
        let permit = acquired
            .map_err(|_| {
                let refused = refusal_log.refuse(query_id, kind, StartRefusal::Expired);
                account_refused(timing.as_ref());
                refused
            })?
            .map_err(|_| ServerError::Internal("executor pool shut down".into()))?;

        // A window reads the live corpus, so it waits for the publication
        // guard as `/query` does. A resolved source reads a recorded run and
        // takes none, as `execute_with_source` does (ADR-0024 keeps per-lane
        // gate membership as it is).
        let publication = match &source {
            CaptureSource::Window { .. } => {
                let published = {
                    let _wait = clock.guard(QueryPhase::PublicationWait);
                    deadline.run(self.publication.read()).await
                };
                Some(published.map_err(|_| {
                    let refused = refusal_log.refuse(query_id, kind, StartRefusal::Expired);
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

                    // The engine reads the selection `/query` reads for this
                    // text, and the budget is measured over it, under the
                    // publication guard, and refused before anything is
                    // scanned (ADR-0057). A cancellation stops the walk.
                    let clock = slot.clock();
                    let (source, source_bytes, hot) = match source {
                        CaptureSource::Window {
                            start,
                            end,
                            max_bytes,
                        } => {
                            let selected = clock.time(QueryPhase::Source, || {
                                let selection = select(&base_dir, &dsl);
                                selection_bytes(&selection, start, end, &slot.cancel_latch())
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
                                });
                            }
                            (
                                selection.source(),
                                Some(selected_bytes),
                                hot_buffer.as_ref().map(|hb| (hb, &scratch)),
                            )
                        }
                        // No hot buffer: a recorded run is self-contained.
                        CaptureSource::Resolved { source } => (source, None, None),
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
                        hot,
                        &pins,
                        cap,
                        utc_offset_secs,
                        false,
                        || {
                            worker_seam!(seams, Snapshotted);
                        },
                    );
                    worker_seam!(seams, Returned);
                    let result = result?;
                    // Still inside the slot: the permit, the executor, its
                    // interrupt registration and a window's publication
                    // guard are all held while `finish` writes.
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
                // Interrupt deregistered, executor re-idled, publication and
                // permit released, then the holds: on this thread, before the
                // answer goes back.
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

#[cfg(test)]
mod tests {
    use super::*;
    use seam::Seam;

    /// Ordinary interactive work, owned by a key.
    const TEST_WORK: WorkContext = WorkContext {
        kind: WorkKind::Query,
        owner: WorkOwner::Key(7),
        user: None,
        timing: None,
        holds: None,
    };
    const TEST_SAMPLE_WORK: WorkContext = WorkContext {
        kind: WorkKind::Sample,
        owner: WorkOwner::System,
        user: None,
        timing: None,
        holds: None,
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
    /// POOL and `query_id`.
    ///
    /// Both halves are needed. Ids start at zero in every pool, so under
    /// the plain `cargo test` harness, one process running tests in
    /// threads, filtering on the id alone let a sibling test's refusal of its own
    /// query 0 land in this test's count, and "exactly one refusal" is a
    /// count.
    ///
    /// The timing events carry no pool: they are kept by the test's own
    /// `request_id`, which no sibling shares ([`of_timed`]).
    mod capture {
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex as StdMutex, OnceLock};

        type Events = Arc<StdMutex<Vec<HashMap<String, String>>>>;

        /// Which events a sink keeps.
        enum Keep {
            /// This pool's `query_id`, or this `request_id`.
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

        /// Capture what `pool`'s `query_id` logs until the guard drops.
        pub(super) async fn of_query(pool: &super::ExecutorPool, query_id: u64) -> Capture {
            install(Keep::Work {
                pool: pool.instance,
                query_id,
                request_id: None,
            })
            .await
        }

        /// [`of_query`], plus the timing events of `request_id`.
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

        /// Capture every event in the process until the guard drops.
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
        // The envelope columns ingest stamps on every event.
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

    /// An engine refusal crosses the pool intact, sentence and all.
    ///
    /// Nothing between the blocking worker and the caller classifies an
    /// `EngineError` — the worker converts and the async side only
    /// replaces the outcome when the deadline beat it — so what the
    /// engine refused is what the handler answers with. The hot buffer
    /// is the interesting half: with no parquet under the base dir the
    /// query is answered hot-only, which is the retry that has no cold
    /// data to hide and so used to be the one with nothing to stop it.
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
                0,
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

    /// `drop` of a field no event carries is a no-op on the hot lanes too,
    /// as the live tail answers it (ADR-0001): over cold and hot data, and
    /// over a hot buffer with no cold data, every row comes back with every
    /// column. `* EXCLUDE` answered both with an empty result: its refusal
    /// read as a missing column, and the hot-only retry refused the same way.
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
                    0,
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

    /// Rows in a Parquet export's bytes.
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

    /// A read refused for its hot snapshot: 503 `service_unavailable`, the
    /// I/O error's kind as its cause.
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

    /// A query whose hot snapshot cannot be built refuses with a 503 rather
    /// than answer from parquet alone (ADR-0041).
    #[tokio::test]
    async fn snapshot_failure_refuses_a_query_instead_of_answering_cold_only() {
        let (_dir, hot, pool) = cold_and_hot_pool().await;
        let query = || {
            pool.execute(
                pool.allocate_query_id(),
                "* | head 100",
                Deadline::after(Duration::from_secs(30)),
                false,
                0,
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

    /// An export whose hot snapshot cannot be built refuses with a 503
    /// rather than export parquet alone (ADR-0041).
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
        assert_eq!(
            parquet_rows(&export().await.expect("the export answers")),
            2,
            "one cold row and one hot row"
        );

        hot.fail_next_snapshot_for_test();
        let error = export()
            .await
            .map(|bytes| parquet_rows(&bytes))
            .expect_err("the export refuses instead of answering cold-only");
        assert_snapshot_refusal(error, crate::error::CauseKind::IoStorageFull).await;

        assert_eq!(
            parquet_rows(&export().await.expect("the export answers")),
            2,
            "the fault latched nothing"
        );
    }

    /// A snapshot whose path is not UTF-8 cannot be opened by `DuckDB`, so
    /// the read refuses rather than drop the hot source (ADR-0041). Query
    /// and export both take their hot source from [`hot_source`].
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
        };
        assert!(snapshot.path().to_str().is_none());

        let error = hot_source(Ok(Some(snapshot))).expect_err("the read refuses");
        assert_snapshot_refusal(error, crate::error::CauseKind::IoInvalidData).await;
        assert!(
            hot_source(Ok(None)).unwrap().is_none(),
            "an empty buffer is no hot source, not a refusal"
        );
    }

    /// The hot-buffer snapshot files in a pool's scratch directory, by
    /// size.
    fn snapshot_files(pool: &ExecutorPool) -> Vec<u64> {
        std::fs::read_dir(pool.scratch().path())
            .unwrap()
            .map(|entry| entry.unwrap())
            .filter(|entry| trawl_engine::scratch::is_snapshot_file_name(&entry.file_name()))
            .filter_map(|entry| entry.metadata().ok().map(|meta| meta.len()))
            .collect()
    }

    /// Snapshots get no byte budget of their own (ADR-0056): at most one
    /// file per permit plus the cached one exists, each no larger than the
    /// serialized buffer, however many generations pass while every permit
    /// holds a snapshot.
    ///
    /// Each round parks one export per permit at `Finished`, where it still
    /// holds its snapshot, and starts each after a fresh insert, so every
    /// permit holds a different generation's file. A sampler counts the
    /// files the whole time, including while one round's exports release
    /// and the next round's take the permits.
    #[tokio::test(flavor = "multi_thread")]
    #[allow(clippy::too_many_lines)] // one scenario, round by round
    async fn snapshot_files_bounded_by_permits() {
        use crate::bus::IngestBatch;
        use crate::hot_buffer::HotBufferConfig;
        use std::sync::atomic::AtomicUsize;

        /// One round's gate and its parked exports.
        type Round = (
            Arc<seam::Gate>,
            Vec<tokio::task::JoinHandle<Result<Vec<u8>, ServerError>>>,
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

    /// A run that produced no rows carries no presentation metadata: an
    /// error and a timeout both stamp nothing, because there is nothing to
    /// present and the walk's answer would describe a result the caller
    /// never gets.
    #[tokio::test]
    async fn a_failed_run_stamps_no_severity_columns() {
        let catalog = Arc::new(crate::catalog::FieldCatalog::new());
        catalog.repin("lvl", trawl_core::schema::CanonicalType::Severity);
        let pool = ExecutorPool::for_test("/nonexistent".into(), 2, 100_000, None)
            .unwrap()
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
        // snapshot and no walk exist at all. The worker is parked at this
        // pool's own seam rather than slowed by the process-global delay,
        // which a sibling test can reset out from under this one.
        //
        // The clock only moves once the worker is PAST its work-start
        // transition. A short real deadline used to decide the class by
        // wall clock: on a loaded runner expiry beat the blocking pool's
        // own startup and the pool answered with the pre-start capacity
        // 503, which is the correct classification for work that never
        // started and not the one this assertion pins.
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
                    0,
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
                0,
                TEST_WORK,
            )
            .await;
        assert!(outcome.result.is_err());
    }

    #[tokio::test]
    async fn pool_respects_concurrency_limit() {
        let pool = ExecutorPool::for_test("/nonexistent".into(), 1, 100_000, None).unwrap();
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
        let pool = ExecutorPool::for_test("/nonexistent".into(), 2, 100_000, None).unwrap();

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
        let pool = ExecutorPool::for_test("/var/lib/trawl/data".into(), 1, 100_000, None).unwrap();
        assert_eq!(&*pool.fallback_glob, "/var/lib/trawl/data/**/*.parquet");
    }

    #[test]
    fn fallback_glob_strips_trailing_slash() {
        let pool = ExecutorPool::for_test("/var/lib/trawl/data/".into(), 1, 100_000, None).unwrap();
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
        let pool = ExecutorPool::for_test("/nonexistent".into(), 1, 100_000, None).unwrap();
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
        let pool = ExecutorPool::for_test("/nonexistent".into(), 2, 100_000, None).unwrap();

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
                0,
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
        let logged = capture::of_query(&pool, id).await;

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

    /// A panic in the worker answers `Panicked`, and the panic's payload
    /// reaches neither the error's own renderings, the log line the
    /// response writes, nor the response body (ADR-0040): a payload can
    /// quote anything the panicking code had in hand.
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
                0,
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
    /// kind and owner: exports, scheduled runs and captures included, not
    /// just the interactive query lane.
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
                    0,
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
                    0,
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
        let captured = pool
            .capture(
                pool.allocate_query_id(),
                "*",
                window(),
                RowCap::Refuse(100),
                Deadline::after(Duration::from_secs(60)),
                0,
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

    /// The sampling helper may not wait like a query (ADR-0024).
    ///
    /// The pool's one permit is parked in a worker, so the sample can
    /// only queue. It gives up after one second of acquisition, no matter
    /// how long the caller's overall deadline is — and the smaller of the
    /// two always wins, so a 300 ms caller waits 300 ms. It answers the
    /// safe capacity refusal, and leaves nothing behind.
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
                    0,
                    TEST_WORK,
                )
                .await
        });
        until("the pool's one permit is taken", || parked.arrivals() == 1).await;
        assert_eq!(pool.available_permits(), 0);

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

    /// Acquisition is only half of a sample's life. Once it holds a
    /// permit it reads parquet, and that read has to answer to the
    /// caller's deadline too (ADR-0024). Otherwise an autocomplete
    /// request over a huge corpus hangs for as long as the read takes,
    /// with nothing in the retained count to explain the missing permit.
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

        // Past the work-start transition, so expiry is a timeout the
        // caller can act on, not the pre-start capacity refusal.
        tokio::time::advance(Duration::from_secs(6)).await;
        let refused = sample
            .await
            .expect("the sample task joins")
            .expect_err("a sample past its deadline does not answer");
        assert!(
            matches!(refused, ServerError::Timeout),
            "expected a post-start timeout, got {refused:?}"
        );

        // The permit is still out, and the pool says who is holding it.
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

    /// An export whose staging file cannot be made in the query scratch
    /// directory fails without naming it (ADR-0056). The error's text is
    /// what the internal-error log records, and the I/O error `tempfile`
    /// raises carries the path it tried.
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

    /// A budget exhausted by the `_raw`-free retry is the query's answer
    /// (ADR-0056). A bare word binds `_raw`, a corpus without it fails that
    /// bind, and the retry that binds `message` instead runs out of the
    /// shared budget. Reporting the first attempt's missing `_raw` there
    /// answered a 400 for a column the reader never named, and, with a hot
    /// buffer, routed the query into the hot-only fallback and a 200 that
    /// left the cold rows out. Both lanes, with and without a hot buffer,
    /// must answer `query_resource_exhausted`.
    #[tokio::test]
    async fn an_exhausted_raw_free_retry_answers_resource_exhausted() {
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
                .execute(
                    pool.allocate_query_id(),
                    DSL,
                    deadline(),
                    false,
                    0,
                    TEST_WORK,
                )
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
                    export.map(|bytes| parquet_rows(&bytes))
                ));
            }
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    /// Work that outlives its request and its pool keeps the query scratch
    /// directory locked until its connection to the query database closes
    /// (ADR-0056), whichever lane it came through. A field sample and a
    /// `from saved` run park past their deadlines; then the pool and the
    /// engine sampler drop. A second lock on the directory must refuse
    /// while either worker still holds a connection: taking it would
    /// reclaim the spill directory the parked database still writes into.
    /// Once both workers finish, the lock frees, and the database removed
    /// its own spill directory on the way out.
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
                    0,
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
        let pool = ExecutorPool::for_test("/nonexistent".into(), 1, 100_000, None).unwrap();
        // No active queries — cancel_all should be a no-op.
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

    /// A second hold on a seam replaces the first in the table, and a
    /// worker already parked at the first gate must still leave when the
    /// session drops. Otherwise a test that fails between the two holds
    /// hangs its process instead of failing.
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
        // Hang guard: the worker arrives as soon as its thread runs.
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
        // A buggy table never releases the first gate, so this can only
        // end by the hang guard.
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

    /// A capacity refusal writes one complete account with the wait that
    /// ran out and no worker phase: nothing started (ADR-0046).
    #[tokio::test(start_paused = true)]
    async fn capacity_refusal_reports_wait_phases() {
        let pool = ExecutorPool::for_test("/nonexistent".into(), 1, 100_000, None).unwrap();

        // Refused in the queue: the pool's one permit is held elsewhere.
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
                0,
                TEST_WORK.with_timing(timing),
            )
            .await;
        assert!(
            matches!(&outcome.result, Err(ServerError::ServiceUnavailable(msg)) if msg == CAPACITY_NOT_STARTED),
            "{:?}",
            outcome.result
        );
        let queued = capture.of_type("query_timing");
        drop(capture);
        drop(held);

        // Refused at the publication gate: the permit came, the gate
        // did not.
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
                0,
                TEST_WORK.with_timing(timing),
            )
            .await;
        assert!(
            matches!(&outcome.result, Err(ServerError::ServiceUnavailable(msg)) if msg == CAPACITY_NOT_STARTED),
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

    /// A timeout while the worker is parked in bind writes the partial
    /// account at once, naming bind as the phase in flight and booking
    /// none of it. The reclaim, when the work finally stops, carries the
    /// worker's final totals under the same ids (ADR-0046).
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
                    0,
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

    /// The worker finishing at the deadline writes exactly one final
    /// account, in either order (ADR-0046): the registry lock decides.
    ///
    /// Deadline first: the request stamps the work retained while its
    /// answer waits for cleanup, writes the partial account, and the
    /// reclaim carries the totals. Worker first: cleanup already ran and
    /// deregistered the work before the request heard, so there is no
    /// reclaim coming and the request's account is complete.
    #[tokio::test(start_paused = true)]
    async fn deadline_finish_race_writes_one_final_account() {
        let pool = hot_pool(1);

        // Deadline, then release.
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
                    0,
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

        // The worker finished and its slot dropped, then the deadline.
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
                    0,
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

    /// A caller that walks away while the worker binds gets the same
    /// contract as a timeout, as `abandoned`: the partial account now,
    /// the final totals on the reclaim (ADR-0046).
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
                    0,
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

    /// `pool_acquired` is gone from both lanes that wrote it: its one
    /// datum is `query_pool_wait_us` now. The capture keeps every event
    /// in the process, the worker thread's included, so an absence here
    /// is an absence everywhere.
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
                0,
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
                0,
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

    /// [`window`] under a budget of `max_bytes`.
    fn window_under(max_bytes: u64) -> CaptureSource {
        CaptureSource::Window {
            start: "2026-01-03T00:00:00Z".parse().unwrap(),
            end: "2026-01-03T01:00:00Z".parse().unwrap(),
            max_bytes,
        }
    }

    /// A `finish` that answers the executed result's row count.
    fn rows_of() -> CaptureFinish<usize> {
        Box::new(|input: CaptureInput<'_>| Ok(input.result.rows.len()))
    }

    /// What a capture's charge saw as it dropped.
    #[derive(Debug, Clone, Copy)]
    struct Released {
        at: Instant,
        available_permits: usize,
        idle_executors: usize,
        /// Interrupt slots plus lifecycle entries still registered.
        registered: usize,
    }

    /// A stand-in for a capture's admission slot and quota reservation
    /// (ADR-0057): it records what the pool still held when it dropped.
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

    /// Capture work for `key` carrying a fresh [`Charge`] on `pool`, and
    /// the charge's record, empty until it drops.
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

    /// The bytes of every parquet file under `root`.
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

    /// A capture runs `finish` inside its slot, on the slot's own executor,
    /// and its charge is released only once the slot gave everything back:
    /// the permit, the executor and the registration (ADR-0057). The window
    /// reads the cold row and the hot row, and its budget measured exactly
    /// the stored files the read selected.
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
                0,
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

    /// A capture whose caller walks away before its work starts never runs
    /// `finish`, and its charge stays held until the slot it rode in is
    /// gone (ADR-0057).
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
                        0,
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

    /// An answer nobody hears: it records that it was dropped.
    struct Unheard(Arc<AtomicBool>);

    impl Drop for Unheard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// A `finish` still writing at the deadline: the request answers a
    /// timeout, the work keeps its permit and its charge until `finish`
    /// returns, and what `finish` then answers is dropped unheard.
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
                        0,
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
        // The worker hands its answer back after the slot is gone, and the
        // runtime drops it there: the request stopped listening at the
        // deadline.
        until("finish's late answer is dropped, never delivered", || {
            unheard.load(Ordering::SeqCst)
        })
        .await;
        assert_eq!(pool.retained(), 0);
        assert!(registry_is_empty(&pool));
    }

    /// A window over the source budget is refused before the work reaches
    /// `Started`: no hot snapshot, no engine, no `finish`. The refusal is
    /// logged once with its counts, and the permit and the charge come
    /// back (ADR-0057).
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
                0,
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
            } => {
                assert_eq!(selected_bytes, stored);
                assert_eq!(max_bytes, 1);
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

    /// A resolved source (`| from saved`) waits for no publication guard and
    /// reads no hot buffer: it answers with a writer holding the guard, and
    /// with the stored rows only. A window on the same pool does wait.
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
                0,
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
                0,
                WorkContext::key(WorkKind::Capture, 7),
                rows_of(),
            )
            .await
            .expect_err("a window waits for the guard the writer holds");
        assert!(
            matches!(&refused, ServerError::ServiceUnavailable(msg) if msg == CAPACITY_NOT_STARTED),
            "{refused:?}"
        );
        drop(writer);
        assert_eq!(pool.available_permits(), pool.capacity());
    }

    /// A panic in `finish` answers `Panicked` and leaks nothing; a query
    /// that fails never reaches `finish`. Both release the charge.
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
                0,
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
                0,
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
