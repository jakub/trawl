// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The phase clock a caller hands the executor to split a query's time
//! (ADR-0046).
//!
//! Durations accumulate at full precision and floor to whole microseconds
//! once, when a [`PhaseTotals`] is taken, so repeated short phases do not
//! lose a microsecond per entry.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QueryPhase {
    DslCheck,
    SavedLookup,
    PoolWait,
    PublicationWait,
    Startup,
    Source,
    HotSnapshot,
    Emit,
    Probe,
    Bind,
    Execute,
    Copy,
    Post,
    Write,
    Render,
}

const PHASES: usize = 15;

impl QueryPhase {
    pub const ALL: [Self; PHASES] = [
        Self::DslCheck,
        Self::SavedLookup,
        Self::PoolWait,
        Self::PublicationWait,
        Self::Startup,
        Self::Source,
        Self::HotSnapshot,
        Self::Emit,
        Self::Probe,
        Self::Bind,
        Self::Execute,
        Self::Copy,
        Self::Post,
        Self::Write,
        Self::Render,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DslCheck => "dsl_check",
            Self::SavedLookup => "saved_lookup",
            Self::PoolWait => "pool_wait",
            Self::PublicationWait => "publication_wait",
            Self::Startup => "startup",
            Self::Source => "source",
            Self::HotSnapshot => "hot_snapshot",
            Self::Emit => "emit",
            Self::Probe => "probe",
            Self::Bind => "bind",
            Self::Execute => "execute",
            Self::Copy => "copy",
            Self::Post => "post",
            Self::Write => "write",
            Self::Render => "render",
        }
    }

    /// Spelled out rather than formatted, because tracing field names are
    /// `'static` and the server's drift test compares these against the
    /// fields its events actually declare.
    #[must_use]
    pub const fn field(self) -> &'static str {
        match self {
            Self::DslCheck => "query_dsl_check_us",
            Self::SavedLookup => "query_saved_lookup_us",
            Self::PoolWait => "query_pool_wait_us",
            Self::PublicationWait => "query_publication_wait_us",
            Self::Startup => "query_startup_us",
            Self::Source => "query_source_us",
            Self::HotSnapshot => "query_hot_snapshot_us",
            Self::Emit => "query_emit_us",
            Self::Probe => "query_probe_us",
            Self::Bind => "query_bind_us",
            Self::Execute => "query_execute_us",
            Self::Copy => "query_copy_us",
            Self::Post => "query_post_us",
            Self::Write => "query_write_us",
            Self::Render => "query_render_us",
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fallback {
    #[default]
    None,
    HotOnly,
}

impl Fallback {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::HotOnly => "hot_only",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PhaseTotals {
    us: [Option<u64>; PHASES],
    pub attempts: u32,
    pub fallback: Fallback,
    pub active: Option<(QueryPhase, u64)>,
}

impl PhaseTotals {
    #[must_use]
    pub fn get(&self, phase: QueryPhase) -> Option<u64> {
        self.us[phase.index()]
    }

    pub fn present(&self) -> impl Iterator<Item = (QueryPhase, u64)> + '_ {
        QueryPhase::ALL
            .into_iter()
            .filter_map(|phase| self.get(phase).map(|us| (phase, us)))
    }

    #[must_use]
    pub fn booked_us(&self) -> u64 {
        self.present()
            .fold(0_u64, |sum, (_, us)| sum.saturating_add(us))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set(&mut self, phase: QueryPhase, us: Option<u64>) {
        self.us[phase.index()] = us;
    }
}

#[cfg(any(test, feature = "test-support"))]
type EnterHook = Arc<dyn Fn() + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    Enter(QueryPhase),
    Exit(QueryPhase),
    Discard(QueryPhase),
}

#[derive(Default)]
struct ClockState {
    us: [Option<Duration>; PHASES],
    attempts: u32,
    fallback: Fallback,
    active: Option<(QueryPhase, Instant)>,
    #[cfg(any(test, feature = "test-support"))]
    hold: Option<(QueryPhase, EnterHook)>,
    #[cfg(any(test, feature = "test-support"))]
    log: Vec<Transition>,
}

impl ClockState {
    fn book(&mut self, phase: QueryPhase, elapsed: Duration) {
        let slot = &mut self.us[phase.index()];
        *slot = Some(slot.unwrap_or_default().saturating_add(elapsed));
    }

    fn totals(&self, active: Option<(QueryPhase, u64)>) -> PhaseTotals {
        PhaseTotals {
            us: self.us.map(|d| d.map(micros)),
            attempts: self.attempts,
            fallback: self.fallback,
            active,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    fn note(&mut self, transition: Transition) {
        self.log.push(transition);
    }

    #[cfg(not(any(test, feature = "test-support")))]
    #[allow(clippy::unused_self)]
    fn note(&mut self, _transition: Transition) {}
}

fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

/// Clones share one account. The lock is held only to read or write that
/// account: never across a `DuckDB` call, an await, or a test hook, so a
/// request thread can always take a partial copy while a worker is parked
/// inside a phase.
#[derive(Clone, Default)]
pub struct PhaseClock(Option<Arc<Mutex<ClockState>>>);

impl std::fmt::Debug for PhaseClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("PhaseClock")
            .field(&if self.0.is_some() { "on" } else { "off" })
            .finish()
    }
}

#[must_use = "the phase ends when the guard drops"]
#[derive(Debug)]
pub struct PhaseGuard<'a> {
    clock: &'a PhaseClock,
    phase: QueryPhase,
}

impl Drop for PhaseGuard<'_> {
    fn drop(&mut self) {
        self.clock.exit(self.phase);
    }
}

impl PhaseClock {
    #[must_use]
    pub const fn off() -> Self {
        Self(None)
    }

    #[must_use]
    pub fn new() -> Self {
        Self(Some(Arc::new(Mutex::new(ClockState::default()))))
    }

    #[must_use]
    pub const fn is_on(&self) -> bool {
        self.0.is_some()
    }

    /// The account, recovering it from a poisoned lock: every write under
    /// it is a plain field store, so a panic elsewhere cannot leave it
    /// half-written.
    fn state(&self) -> Option<MutexGuard<'_, ClockState>> {
        self.0
            .as_ref()
            .map(|m| m.lock().unwrap_or_else(PoisonError::into_inner))
    }

    pub fn enter(&self, phase: QueryPhase) {
        let Some(mut state) = self.state() else {
            return;
        };
        debug_assert!(
            state.active.is_none(),
            "entered {phase:?} while {:?} is active",
            state.active.map(|(p, _)| p)
        );
        state.active = Some((phase, Instant::now()));
        state.note(Transition::Enter(phase));
        #[cfg(any(test, feature = "test-support"))]
        {
            let hook = state
                .hold
                .as_ref()
                .filter(|(held, _)| *held == phase)
                .map(|(_, hook)| Arc::clone(hook));
            // The hook runs with the lock released, so a partial copy
            // taken while it parks sees this phase active.
            drop(state);
            if let Some(hook) = hook {
                hook();
            }
        }
    }

    /// Finish `phase` now and add its elapsed time to its total.
    ///
    /// A phase that is no longer active was already booked by
    /// [`Self::close_at`]: an account written while a guard was still in
    /// scope, as when a dropped request future releases its locals in
    /// any order. That exit is a no-op. Exiting a phase while a different
    /// one is active is a bug in the caller's instrumentation.
    pub fn exit(&self, phase: QueryPhase) {
        let now = Instant::now();
        let Some(mut state) = self.state() else {
            return;
        };
        match state.active {
            Some((active, start)) if active == phase => {
                state.active = None;
                state.book(phase, now.saturating_duration_since(start));
                state.note(Transition::Exit(phase));
            }
            None => {}
            Some((other, _)) => debug_assert!(false, "exited {phase:?} while {other:?} is active"),
        }
    }

    /// Abandon `phase` without booking it, so its elapsed time stays in the
    /// residual.
    ///
    /// A phase that is no longer active is a no-op: a refused start is
    /// discarded by whichever of the request and its worker gets there first.
    /// Discarding a phase while a different one is active is a bug in the
    /// caller's instrumentation.
    pub fn discard(&self, phase: QueryPhase) {
        let Some(mut state) = self.state() else {
            return;
        };
        match state.active {
            Some((active, _)) if active == phase => {
                state.active = None;
                state.note(Transition::Discard(phase));
            }
            None => {}
            Some((other, _)) => {
                debug_assert!(false, "discarded {phase:?} while {other:?} is active");
            }
        }
    }

    pub fn guard(&self, phase: QueryPhase) -> PhaseGuard<'_> {
        self.enter(phase);
        PhaseGuard { clock: self, phase }
    }

    pub fn time<R>(&self, phase: QueryPhase, work: impl FnOnce() -> R) -> R {
        let _guard = self.guard(phase);
        work()
    }

    pub fn bind_attempt(&self) {
        if let Some(mut state) = self.state() {
            state.attempts = state.attempts.saturating_add(1);
        }
    }

    pub fn fallback(&self, fallback: Fallback) {
        if fallback == Fallback::None {
            return;
        }
        if let Some(mut state) = self.state() {
            state.fallback = fallback;
        }
    }

    /// `cut` is read after the account is locked, so no phase can finish
    /// between it and the totals.
    #[must_use]
    pub fn snapshot(&self) -> Option<(PhaseTotals, Instant)> {
        let state = self.state()?;
        let cut = Instant::now();
        let active = state
            .active
            .map(|(phase, start)| (phase, micros(cut.saturating_duration_since(start))));
        Some((state.totals(active), cut))
    }

    #[must_use]
    pub fn close_at(&self, now: Instant) -> Option<PhaseTotals> {
        let mut state = self.state()?;
        if let Some((phase, start)) = state.active.take() {
            state.book(phase, now.saturating_duration_since(start));
            state.note(Transition::Exit(phase));
        }
        Some(state.totals(None))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn hold_on_enter(&self, phase: QueryPhase, hook: impl Fn() + Send + Sync + 'static) {
        if let Some(mut state) = self.state() {
            state.hold = Some((phase, Arc::new(hook)));
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn transitions(&self) -> Vec<Transition> {
        self.state().map(|s| s.log.clone()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};
    use std::time::{Duration, Instant};

    use super::{Fallback, PhaseClock, QueryPhase, Transition, micros};

    #[test]
    fn phase_names_and_fields_agree() {
        for phase in QueryPhase::ALL {
            assert_eq!(phase.field(), format!("query_{}_us", phase.as_str()));
        }
        let mut names: Vec<_> = QueryPhase::ALL.iter().map(|p| p.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), QueryPhase::ALL.len(), "names are unique");
        for (i, phase) in QueryPhase::ALL.into_iter().enumerate() {
            assert_eq!(phase.index(), i, "ALL is in declaration order");
        }
        assert_eq!(QueryPhase::Write.as_str(), "write");
        assert_eq!(
            QueryPhase::ALL[QueryPhase::Post.index() + 1],
            QueryPhase::Write
        );
    }

    #[test]
    fn an_off_clock_records_nothing() {
        let clock = PhaseClock::off();
        assert!(!clock.is_on());
        clock.time(QueryPhase::Bind, || ());
        clock.bind_attempt();
        clock.fallback(Fallback::HotOnly);
        assert!(clock.snapshot().is_none());
        assert!(clock.close_at(Instant::now()).is_none());
        assert!(clock.transitions().is_empty());
    }

    #[test]
    fn absent_until_entered_and_zero_is_measured() {
        let clock = PhaseClock::new();
        let totals = clock.close_at(Instant::now()).expect("on");
        assert!(totals.present().next().is_none(), "nothing entered");

        clock.time(QueryPhase::Emit, || ());
        let totals = clock.close_at(Instant::now()).expect("on");
        assert!(totals.get(QueryPhase::Emit).is_some(), "a zero is present");
        assert_eq!(totals.get(QueryPhase::Bind), None);
    }

    #[test]
    fn repeated_entries_add_to_one_total() {
        let clock = PhaseClock::new();
        for _ in 0..2 {
            clock.bind_attempt();
            clock.time(QueryPhase::Bind, || {
                std::thread::sleep(Duration::from_millis(2));
            });
        }
        let totals = clock.close_at(Instant::now()).expect("on");
        assert_eq!(totals.attempts, 2);
        assert!(
            totals.get(QueryPhase::Bind).expect("bind booked") >= 4_000,
            "both entries booked: {totals:?}"
        );
        assert_eq!(
            clock.transitions(),
            [
                Transition::Enter(QueryPhase::Bind),
                Transition::Exit(QueryPhase::Bind),
                Transition::Enter(QueryPhase::Bind),
                Transition::Exit(QueryPhase::Bind),
            ]
        );
    }

    #[test]
    fn a_partial_copy_reports_the_active_phase_without_booking_it() {
        let clock = PhaseClock::new();
        clock.time(QueryPhase::Source, || ());
        let before = Instant::now();
        clock.enter(QueryPhase::Bind);
        let (partial, cut) = clock.snapshot().expect("on");
        assert!(partial.get(QueryPhase::Source).is_some());
        assert_eq!(
            partial.get(QueryPhase::Bind),
            None,
            "unfinished is unbooked"
        );
        let (phase, elapsed) = partial.active.expect("bind is active");
        assert_eq!(phase, QueryPhase::Bind);
        assert!(
            elapsed <= micros(cut.saturating_duration_since(before)),
            "elapsed measured to the cut: {elapsed}"
        );

        clock.exit(QueryPhase::Bind);
        let complete = clock.close_at(Instant::now()).expect("on");
        assert!(complete.active.is_none());
        assert!(complete.get(QueryPhase::Bind).is_some());
    }

    #[test]
    fn an_exit_after_close_is_a_no_op() {
        let clock = PhaseClock::new();
        let guard = clock.guard(QueryPhase::SavedLookup);
        let closed = clock.close_at(Instant::now()).expect("on");
        drop(guard);
        assert_eq!(clock.close_at(Instant::now()).expect("on"), closed);
    }

    #[test]
    fn close_books_a_phase_left_open() {
        let clock = PhaseClock::new();
        clock.enter(QueryPhase::Execute);
        let totals = clock
            .close_at(Instant::now() + Duration::from_millis(3))
            .expect("on");
        assert!(totals.active.is_none());
        assert!(totals.get(QueryPhase::Execute).expect("booked") >= 3_000);
    }

    #[test]
    fn a_discarded_phase_is_absent() {
        let clock = PhaseClock::new();
        clock.enter(QueryPhase::SavedLookup);
        clock.discard(QueryPhase::SavedLookup);
        let totals = clock.close_at(Instant::now()).expect("on");
        assert_eq!(totals.get(QueryPhase::SavedLookup), None);
        assert_eq!(
            clock.transitions(),
            [
                Transition::Enter(QueryPhase::SavedLookup),
                Transition::Discard(QueryPhase::SavedLookup),
            ]
        );
    }

    #[test]
    fn a_guard_books_on_unwind() {
        let clock = PhaseClock::new();
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            clock.time(QueryPhase::Post, || panic!("tail stage panicked"));
        }));
        assert!(unwound.is_err());
        let totals = clock.close_at(Instant::now()).expect("on");
        assert!(totals.get(QueryPhase::Post).is_some());
    }

    #[test]
    fn microseconds_floor_once_at_the_copy() {
        let clock = PhaseClock::new();
        if let Some(mut state) = clock.state() {
            // Three 600ns entries are 1.8µs: one whole microsecond once
            // summed, zero if each were floored on its own.
            for _ in 0..3 {
                state.book(QueryPhase::Emit, Duration::from_nanos(600));
            }
        }
        let totals = clock.close_at(Instant::now()).expect("on");
        assert_eq!(totals.get(QueryPhase::Emit), Some(1));
        assert_eq!(totals.booked_us(), 1);
    }

    #[test]
    fn a_recorded_fallback_stays_recorded() {
        use Fallback::{HotOnly, None};
        let clock = PhaseClock::new();
        assert_eq!(clock.close_at(Instant::now()).expect("on").fallback, None);
        clock.fallback(HotOnly);
        clock.fallback(None);
        assert_eq!(
            clock.close_at(Instant::now()).expect("on").fallback,
            HotOnly
        );
        assert_eq!([None, HotOnly].map(Fallback::as_str), ["none", "hot_only"]);
    }

    #[test]
    fn a_held_phase_is_visible_to_another_thread() {
        let clock = PhaseClock::new();
        let parked = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        {
            let parked = Arc::clone(&parked);
            let release = Arc::clone(&release);
            clock.hold_on_enter(QueryPhase::Bind, move || {
                parked.wait();
                release.wait();
            });
        }
        let worker = {
            let clock = clock.clone();
            std::thread::spawn(move || {
                clock.time(QueryPhase::Emit, || ());
                clock.bind_attempt();
                clock.time(QueryPhase::Bind, || ());
            })
        };
        parked.wait();
        let (partial, _) = clock.snapshot().expect("on");
        assert_eq!(partial.active.map(|(p, _)| p), Some(QueryPhase::Bind));
        assert!(partial.get(QueryPhase::Emit).is_some());
        assert_eq!(partial.get(QueryPhase::Bind), None);
        release.wait();
        worker.join().expect("worker finishes");
        let complete = clock.close_at(Instant::now()).expect("on");
        assert!(complete.get(QueryPhase::Bind).is_some());
        assert_eq!(complete.attempts, 1);
    }
}
