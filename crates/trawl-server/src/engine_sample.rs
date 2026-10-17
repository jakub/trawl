// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The engine sample: what the `duckdb` health check reads (ADR-0056).
//!
//! One diagnostic connection, cloned from the query database, sits outside
//! the executor pool: it takes no permit and never appears in the query
//! listing. A dedicated OS thread, not tokio's blocking pool, owns it and
//! runs `SELECT 1` on it every [`SAMPLE_INTERVAL`], so a blocking pool full
//! of query work cannot starve the sample. A second thread watches each
//! probe: when one runs past [`SAMPLE_BUDGET`] it publishes the overrun at
//! once and interrupts the probe. The probe loop is sequential, so no new
//! probe starts until the interrupted one returns. An answer counts as a
//! success only if the probe's own elapsed time is within the budget, so a
//! watchdog that runs late cannot let a late answer pass as on time.
//!
//! `/api/v1/health` only reads the latest sample ([`EngineSample::read`]).
//! It never waits for a probe and never starts one, so no number of health
//! requests, and no state of the executor pool, makes `DuckDB` work.

use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use trawl_engine::error::EngineError;
use trawl_engine::executor::DiagnosticConnection;

/// How often the sampler starts a probe. A probe that runs into the next
/// start skips it: the next probe starts at the first start time after
/// the late one returns.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(10);

/// How long one probe may run before it counts as past its budget and is
/// interrupted.
pub const SAMPLE_BUDGET: Duration = Duration::from_millis(250);

/// How recent the latest within-budget success must be for the check to
/// read anything but `error`.
pub const SUCCESS_WINDOW: Duration = Duration::from_secs(60);

/// What the `duckdb` health check reads from the latest engine sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineCheck {
    /// The latest sample succeeded within its budget.
    Ok,
    /// The latest sample ran past its budget, and a sample succeeded
    /// within the last [`SUCCESS_WINDOW`]. Health reads `degraded`, 200.
    Slow,
    /// The latest sample returned a `DuckDB` error, or no sample succeeded
    /// within the last [`SUCCESS_WINDOW`], or none has since boot. Health
    /// reads `unavailable`, 503.
    Error,
}

impl EngineCheck {
    /// The `duckdb` check's wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Slow => "slow",
            Self::Error => "error",
        }
    }
}

/// How the latest sample ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// `SELECT 1` answered within the budget.
    Succeeded,
    /// The probe ran past the budget: it is still running, it answered
    /// late, or it returned the interrupt the watchdog raised on it.
    Overran,
    /// The probe returned a `DuckDB` error of its own, or the sampler
    /// panicked.
    Failed,
}

/// The configuration a sampler runs on. Production uses
/// [`SampleConfig::DEFAULT`]; tests shorten the periods.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SampleConfig {
    pub(crate) interval: Duration,
    pub(crate) budget: Duration,
    pub(crate) success_window: Duration,
}

impl SampleConfig {
    pub(crate) const DEFAULT: Self = Self {
        interval: SAMPLE_INTERVAL,
        budget: SAMPLE_BUDGET,
        success_window: SUCCESS_WINDOW,
    };
}

/// Where the published sample's instants come from. The instants that
/// [`EngineSample::read`] compares come from here, and so does the probe's
/// own elapsed time, which decides whether its answer was within budget.
/// The threads' own waits run on the monotonic clock, so a test can age the
/// latest success without waiting a minute, and make a probe late without
/// the watchdog seeing it.
pub(crate) trait Clock: Send + Sync + 'static {
    fn now(&self) -> Instant;
}

/// The monotonic clock.
#[derive(Debug)]
pub(crate) struct MonotonicClock;

impl Clock for MonotonicClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Stops one running probe from another thread.
pub(crate) trait Interrupt: Send + Sync {
    fn interrupt(&self);
}

impl Interrupt for duckdb::InterruptHandle {
    fn interrupt(&self) {
        duckdb::InterruptHandle::interrupt(self);
    }
}

/// One probe the sampler thread owns and runs.
pub(crate) trait Probe: Send + 'static {
    /// Run the probe once, blocking until it answers.
    fn run(&mut self) -> Result<(), EngineError>;

    /// A handle that stops [`Self::run`] from another thread.
    fn interrupt_handle(&self) -> Arc<dyn Interrupt>;
}

/// The production probe: `SELECT 1` on the diagnostic connection.
///
/// The connection holds the query scratch lock until it closes, so a
/// sampler that outlives the pool keeps the directory its database spills
/// into locked.
struct DiagnosticProbe {
    connection: DiagnosticConnection,
}

impl Probe for DiagnosticProbe {
    fn run(&mut self) -> Result<(), EngineError> {
        self.connection.select_one()
    }

    fn interrupt_handle(&self) -> Arc<dyn Interrupt> {
        self.connection.interrupt_handle()
    }
}

/// The probe the sampler thread is running right now.
#[derive(Debug, Clone, Copy)]
struct InFlight {
    /// Which probe, counted from 1.
    number: u64,
    /// When it started, on the monotonic clock, for the watchdog's budget.
    started: Instant,
    /// Whether the watchdog found it past its budget.
    overran: bool,
}

/// The sampler's state, under one lock shared by both threads and every
/// reader.
#[derive(Debug)]
struct State {
    /// How the latest sample ended, once one has ended or overrun.
    latest: Option<Outcome>,
    /// When the latest within-budget success answered, on the sample clock.
    last_success: Option<Instant>,
    /// When the latest probe started, on the sample clock; when the
    /// sampler started, before its first probe.
    latest_start: Instant,
    /// The probe running now, if one is.
    in_flight: Option<InFlight>,
    /// Probes started since the sampler started.
    probes_started: u64,
    /// Samples published, counting an overrun as one when it is found.
    published: u64,
    /// Set when the last handle drops or the sampler panicked: both
    /// threads leave at their next look.
    stopped: bool,
    /// While set, the sampler thread starts no probe (test seam).
    #[cfg(any(test, feature = "test-support"))]
    held: bool,
}

impl State {
    fn publish(&mut self, outcome: Outcome) {
        self.latest = Some(outcome);
        self.published += 1;
    }
}

/// What both threads and every handle share.
struct Shared {
    state: Mutex<State>,
    /// Signalled on every change to [`State`].
    changed: Condvar,
    config: SampleConfig,
    clock: Arc<dyn Clock>,
    interrupt: Arc<dyn Interrupt>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // Nothing panics while holding the lock: the probe runs outside it.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn wait<'a>(&self, guard: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        self.changed
            .wait(guard)
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn wait_until<'a>(
        &self,
        guard: MutexGuard<'a, State>,
        until: Instant,
    ) -> MutexGuard<'a, State> {
        let left = until.saturating_duration_since(Instant::now());
        self.changed
            .wait_timeout(guard, left)
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0
    }

    /// The `duckdb` check, read at `now` on the sample clock.
    fn check(&self, state: &State, now: Instant) -> EngineCheck {
        let recent = state
            .last_success
            .is_some_and(|at| now.saturating_duration_since(at) < self.config.success_window);
        match state.latest {
            _ if !recent => EngineCheck::Error,
            Some(Outcome::Succeeded) => EngineCheck::Ok,
            Some(Outcome::Overran) => EngineCheck::Slow,
            Some(Outcome::Failed) | None => EngineCheck::Error,
        }
    }
}

/// Stops both threads when the last [`EngineSample`] handle drops.
struct Owner(Arc<Shared>);

impl Drop for Owner {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        state.stopped = true;
        // The probe may be wedged: interrupt it rather than wait for it.
        // Nobody joins either thread; each leaves once it sees the stop.
        if state.in_flight.is_some() {
            self.0.interrupt.interrupt();
        }
        drop(state);
        self.0.changed.notify_all();
    }
}

/// The handle health and `/metrics` read the engine sample through.
///
/// Cheap to clone. When the last clone drops, the sampler stops: it
/// interrupts a running probe and its threads exit on their own.
#[derive(Clone)]
pub struct EngineSample {
    shared: Arc<Shared>,
    _owner: Arc<Owner>,
}

impl std::fmt::Debug for EngineSample {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.shared.lock();
        f.debug_struct("EngineSample")
            .field("latest", &state.latest)
            .field("probes_started", &state.probes_started)
            .finish_non_exhaustive()
    }
}

impl EngineSample {
    /// Start sampling the query database `pool` runs on, on a diagnostic
    /// connection cloned from it.
    ///
    /// # Errors
    /// The diagnostic connection could not be cloned, or a thread could
    /// not be spawned.
    pub fn start(pool: &crate::pool::ExecutorPool) -> std::io::Result<Self> {
        let probe = DiagnosticProbe {
            connection: pool
                .diagnostic_connection()
                .map_err(|e| std::io::Error::other(e.to_string()))?,
        };
        Self::spawn(probe, SampleConfig::DEFAULT, Arc::new(MonotonicClock))
    }

    /// Start a sampler over `probe`.
    ///
    /// # Errors
    /// A thread could not be spawned.
    pub(crate) fn spawn(
        probe: impl Probe,
        config: SampleConfig,
        clock: Arc<dyn Clock>,
    ) -> std::io::Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                latest: None,
                last_success: None,
                latest_start: clock.now(),
                in_flight: None,
                probes_started: 0,
                published: 0,
                stopped: false,
                #[cfg(any(test, feature = "test-support"))]
                held: false,
            }),
            changed: Condvar::new(),
            config,
            interrupt: probe.interrupt_handle(),
            clock,
        });
        let owner = Arc::new(Owner(Arc::clone(&shared)));
        std::thread::Builder::new()
            .name("trawl-engine-sample-watchdog".into())
            .spawn({
                let shared = Arc::clone(&shared);
                move || watch(&shared)
            })?;
        std::thread::Builder::new()
            .name("trawl-engine-sample".into())
            .spawn({
                let shared = Arc::clone(&shared);
                move || sample(&shared, probe)
            })?;
        Ok(Self {
            shared,
            _owner: owner,
        })
    }

    /// The `duckdb` check, from the latest sample. Never waits for a probe
    /// and never starts one.
    #[must_use]
    pub fn read(&self) -> EngineCheck {
        let now = self.shared.clock.now();
        self.shared.check(&self.shared.lock(), now)
    }

    /// How long since the latest probe started, or since the sampler
    /// started before its first probe. It keeps growing while a probe is
    /// wedged, and after the sampler stopped.
    #[must_use]
    pub fn age(&self) -> Duration {
        let now = self.shared.clock.now();
        now.saturating_duration_since(self.shared.lock().latest_start)
    }

    /// Wait until the first sample is published: the first probe answered,
    /// failed, or ran past its budget. Bounded by the budget, since the
    /// watchdog publishes an overrun when the budget runs out.
    pub fn wait_for_first(&self) {
        let mut state = self.shared.lock();
        while state.published == 0 && !state.stopped {
            state = self.shared.wait(state);
        }
    }
}

/// The sampler thread: one probe at a time, every interval, until stopped.
fn sample(shared: &Shared, mut probe: impl Probe) {
    let mut next = Instant::now();
    loop {
        let (number, start, started_at) = {
            let mut state = shared.lock();
            loop {
                if state.stopped {
                    return;
                }
                #[cfg(any(test, feature = "test-support"))]
                if state.held {
                    state = shared.wait(state);
                    continue;
                }
                if Instant::now() >= next {
                    break;
                }
                state = shared.wait_until(state, next);
            }
            state.probes_started += 1;
            let number = state.probes_started;
            let start = Instant::now();
            state.latest_start = shared.clock.now();
            state.in_flight = Some(InFlight {
                number,
                started: start,
                overran: false,
            });
            (number, start, state.latest_start)
        };
        shared.changed.notify_all();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| probe.run()));
        let answered = shared.clock.now();

        let mut state = shared.lock();
        // Past its budget if the watchdog flagged it, or if it took the
        // budget or longer by its own measure. The watchdog is a thread
        // too, and under the CPU starvation `slow` describes it can run
        // late: a probe that answered past its budget before the watchdog
        // looked did not answer within it.
        let flagged = state
            .in_flight
            .take()
            .is_some_and(|in_flight| in_flight.number == number && in_flight.overran);
        let late =
            flagged || answered.saturating_duration_since(started_at) >= shared.config.budget;
        let outcome = match &result {
            Ok(Ok(())) if !late => Outcome::Succeeded,
            // A late answer, or the interrupt the watchdog raised on this
            // probe for running past its budget: slow, not broken.
            Ok(Ok(())) => Outcome::Overran,
            Ok(Err(error)) if late && error.is_interrupt() => Outcome::Overran,
            Ok(Err(_)) | Err(_) => Outcome::Failed,
        };
        if outcome == Outcome::Succeeded {
            state.last_success = Some(answered);
        }
        state.publish(outcome);
        // A panicked probe may have left its connection in any state, so
        // the sampler stops for good, and health reads `error` from here on.
        let panicked = result.is_err();
        if panicked {
            state.stopped = true;
            tracing::error!(
                event_type = "engine_sample_panicked",
                "the engine sample panicked; the duckdb health check reads error until restart"
            );
        }
        drop(state);
        shared.changed.notify_all();
        if panicked {
            return;
        }

        // Skip every start time the probe ran past.
        next = start + shared.config.interval;
        let now = Instant::now();
        while next <= now {
            next += shared.config.interval;
        }
    }
}

/// The watchdog thread: publishes and interrupts a probe that runs past
/// its budget, once per probe.
fn watch(shared: &Shared) {
    let mut state = shared.lock();
    loop {
        if state.stopped {
            return;
        }
        let Some(in_flight) = state.in_flight.filter(|in_flight| !in_flight.overran) else {
            state = shared.wait(state);
            continue;
        };
        let deadline = in_flight.started + shared.config.budget;
        if Instant::now() < deadline {
            state = shared.wait_until(state, deadline);
            continue;
        }
        // Still the same probe, past its budget: it is slow now, whatever
        // it returns later.
        if let Some(running) = state.in_flight.as_mut() {
            running.overran = true;
        }
        state.publish(Outcome::Overran);
        shared.interrupt.interrupt();
        shared.changed.notify_all();
    }
}

#[cfg(any(test, feature = "test-support"))]
impl EngineSample {
    /// Stop the sampler from starting probes until the guard drops, and
    /// wait for a running probe to return first. A probe that never
    /// returns holds this forever, so hold only a sampler whose probe
    /// answers.
    #[must_use]
    pub fn hold(&self) -> SampleHold {
        let mut state = self.shared.lock();
        state.held = true;
        while state.in_flight.is_some() && !state.stopped {
            state = self.shared.wait(state);
        }
        SampleHold(Arc::clone(&self.shared))
    }

    /// Probes started since the sampler started.
    #[must_use]
    pub fn probes_started(&self) -> u64 {
        self.shared.lock().probes_started
    }

    /// Publish a sample that reads `check` now, as a held sampler's latest
    /// one, so a test can drive the health route through each value.
    pub fn publish_for_test(&self, check: EngineCheck) {
        let now = self.shared.clock.now();
        let mut state = self.shared.lock();
        match check {
            EngineCheck::Ok => {
                state.last_success = Some(now);
                state.publish(Outcome::Succeeded);
            }
            EngineCheck::Slow => {
                state.last_success = Some(now);
                state.publish(Outcome::Overran);
            }
            EngineCheck::Error => state.publish(Outcome::Failed),
        }
    }
}

#[cfg(test)]
impl EngineSample {
    /// Drop this handle, which must be the last, and wait until both
    /// sampler threads have left. The sampler thread drops its probe before
    /// it lets go of the shared state, so once no thread holds that state
    /// the probe's connection is closed.
    pub(crate) fn stop_and_wait_for_test(self) {
        let shared = Arc::downgrade(&self.shared);
        drop(self);
        let deadline = Instant::now() + Duration::from_secs(20);
        while shared.strong_count() > 0 {
            assert!(Instant::now() < deadline, "the sampler threads never left");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

/// Keeps a sampler from starting probes ([`EngineSample::hold`]).
#[cfg(any(test, feature = "test-support"))]
pub struct SampleHold(Arc<Shared>);

#[cfg(any(test, feature = "test-support"))]
impl std::fmt::Debug for SampleHold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SampleHold").finish_non_exhaustive()
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Drop for SampleHold {
    fn drop(&mut self) {
        self.0.lock().held = false;
        self.0.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    /// A clock a test moves by hand.
    struct TestClock {
        base: Instant,
        offset: Mutex<Duration>,
    }

    impl TestClock {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                base: Instant::now(),
                offset: Mutex::new(Duration::ZERO),
            })
        }

        fn advance(&self, by: Duration) {
            *self.offset.lock().unwrap() += by;
        }
    }

    impl Clock for TestClock {
        fn now(&self) -> Instant {
            self.base + *self.offset.lock().unwrap()
        }
    }

    /// What one scripted probe does when it runs.
    #[derive(Debug, Clone, Copy)]
    enum Step {
        /// Answer at once.
        Answer,
        /// Return a `DuckDB` error of its own.
        Fail,
        /// Block until interrupted, then until the test releases it, then
        /// return the interrupt.
        Wedge,
        /// Panic.
        Panic,
    }

    /// The probe's side of the script, shared with the test.
    #[derive(Default)]
    struct Script {
        steps: Mutex<std::collections::VecDeque<Step>>,
        /// The step every probe takes once the script ran out.
        then: Mutex<Option<Step>>,
        interrupts: AtomicU64,
        interrupted: AtomicBool,
        released: AtomicBool,
        running: AtomicU64,
        wake: Condvar,
        lock: Mutex<()>,
    }

    impl Script {
        fn new(steps: &[Step], then: Step) -> Arc<Self> {
            let script = Self::default();
            script.steps.lock().unwrap().extend(steps.iter().copied());
            *script.then.lock().unwrap() = Some(then);
            Arc::new(script)
        }

        fn release(&self) {
            let _guard = self.lock.lock().unwrap();
            self.released.store(true, Ordering::SeqCst);
            self.wake.notify_all();
        }
    }

    impl Interrupt for Script {
        fn interrupt(&self) {
            let _guard = self.lock.lock().unwrap();
            self.interrupts.fetch_add(1, Ordering::SeqCst);
            self.interrupted.store(true, Ordering::SeqCst);
            self.wake.notify_all();
        }
    }

    struct ScriptedProbe(Arc<Script>);

    /// A `DuckDB` failure carrying `exception_type`, as the driver builds
    /// one under `errors_as_json`.
    fn duckdb_error(exception_type: &str) -> EngineError {
        EngineError::from(duckdb::Error::DuckDBFailure(
            duckdb::ffi::Error::new(1),
            Some(format!(
                r#"{{"exception_type":"{exception_type}","exception_message":"x"}}"#
            )),
        ))
    }

    impl Probe for ScriptedProbe {
        fn run(&mut self) -> Result<(), EngineError> {
            let script = &self.0;
            let step = script
                .steps
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| script.then.lock().unwrap().expect("a fallback step"));
            script.running.fetch_add(1, Ordering::SeqCst);
            let result = match step {
                Step::Answer => Ok(()),
                Step::Fail => Err(duckdb_error("Catalog")),
                Step::Wedge => {
                    let mut guard = script.lock.lock().unwrap();
                    while !script.interrupted.load(Ordering::SeqCst)
                        || !script.released.load(Ordering::SeqCst)
                    {
                        guard = script.wake.wait(guard).unwrap();
                    }
                    script.interrupted.store(false, Ordering::SeqCst);
                    Err(duckdb_error("INTERRUPT"))
                }
                Step::Panic => {
                    script.running.fetch_sub(1, Ordering::SeqCst);
                    panic!("injected engine sample panic");
                }
            };
            script.running.fetch_sub(1, Ordering::SeqCst);
            result
        }

        fn interrupt_handle(&self) -> Arc<dyn Interrupt> {
            Arc::clone(&self.0) as Arc<dyn Interrupt>
        }
    }

    const FAST: SampleConfig = SampleConfig {
        interval: Duration::from_millis(20),
        budget: Duration::from_millis(50),
        success_window: SUCCESS_WINDOW,
    };

    /// Poll `cond` on the real clock, failing after a generous bound.
    fn until(what: &str, cond: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !cond() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn published(sample: &EngineSample) -> u64 {
        sample.shared.lock().published
    }

    /// The check reads off the latest sample and the latest success alone:
    /// no sample yet is `error`; a success is `ok`; an overrun after a
    /// success is `slow`; a `DuckDB` error is `error`, even right after a
    /// success.
    #[test]
    fn engine_sample_maps_each_outcome_to_its_check() {
        let script = Script::new(&[], Step::Answer);
        let clock = TestClock::new();
        let sample = EngineSample::spawn(ScriptedProbe(script), FAST, clock).unwrap();
        let _hold = sample.hold();
        let shared = &sample.shared;
        let now = shared.clock.now();
        let check = |latest, last_success| {
            let state = State {
                latest,
                last_success,
                latest_start: now,
                in_flight: None,
                probes_started: 0,
                published: 0,
                stopped: false,
                held: false,
            };
            shared.check(&state, now)
        };
        assert_eq!(check(None, None), EngineCheck::Error, "none since boot");
        assert_eq!(check(Some(Outcome::Succeeded), Some(now)), EngineCheck::Ok);
        assert_eq!(check(Some(Outcome::Overran), Some(now)), EngineCheck::Slow);
        assert_eq!(check(Some(Outcome::Failed), Some(now)), EngineCheck::Error);
        assert_eq!(
            check(Some(Outcome::Overran), None),
            EngineCheck::Error,
            "an overrun with no success since boot"
        );
    }

    /// A first probe that answers reads `ok`, and a `DuckDB` error of the
    /// probe's own reads `error` at once, right after that success. The
    /// scripted probe runs on the sampler's own threads, end to end.
    #[test]
    fn engine_sample_reads_ok_then_error_on_a_duckdb_error() {
        let script = Script::new(&[Step::Answer], Step::Fail);
        let sample =
            EngineSample::spawn(ScriptedProbe(Arc::clone(&script)), FAST, TestClock::new())
                .unwrap();
        sample.wait_for_first();
        assert_eq!(sample.read(), EngineCheck::Ok);
        until("the failing probe", || published(&sample) >= 2);
        assert_eq!(sample.read(), EngineCheck::Error);
        assert_eq!(script.interrupts.load(Ordering::SeqCst), 0);
    }

    /// A wedged probe: past its budget the check reads `slow` while it is
    /// still running, the watchdog interrupts it exactly once, and no
    /// second probe starts, across many intervals, until it returns. The
    /// interrupt it then returns is the sampler's own doing and still
    /// reads `slow`, not `error`. Once it returns, sampling resumes.
    #[test]
    fn engine_sample_interrupts_a_wedged_probe_and_starts_no_second() {
        let script = Script::new(&[Step::Answer, Step::Wedge], Step::Answer);
        let sample =
            EngineSample::spawn(ScriptedProbe(Arc::clone(&script)), FAST, TestClock::new())
                .unwrap();
        sample.wait_for_first();
        assert_eq!(sample.read(), EngineCheck::Ok);

        until("the wedged probe is interrupted", || {
            script.interrupts.load(Ordering::SeqCst) == 1
        });
        assert_eq!(sample.read(), EngineCheck::Slow, "slow while it runs");
        assert_eq!(sample.probes_started(), 2);
        // Ten intervals and more: still the one probe, interrupted once.
        std::thread::sleep(FAST.interval * 10 + FAST.budget * 2);
        assert_eq!(sample.probes_started(), 2, "no second probe while one runs");
        assert_eq!(script.running.load(Ordering::SeqCst), 1);
        assert_eq!(script.interrupts.load(Ordering::SeqCst), 1);
        assert_eq!(sample.read(), EngineCheck::Slow);

        // Hold first, so the probe that follows cannot land before the
        // interrupted one is read.
        sample.shared.lock().held = true;
        let before = published(&sample);
        script.release();
        until("the interrupted probe returns", || {
            sample.shared.lock().in_flight.is_none()
        });
        assert!(
            published(&sample) > before,
            "the interrupted probe published"
        );
        assert_eq!(
            sample.read(),
            EngineCheck::Slow,
            "its own interrupt reads slow"
        );
        drop(SampleHold(Arc::clone(&sample.shared)));
        until("sampling resumes", || sample.probes_started() >= 3);
        until("a later answer", || sample.read() == EngineCheck::Ok);
    }

    /// Answers once at once, then once after moving the sample clock past
    /// the budget, then waits for the test.
    struct LateSecond {
        clock: Arc<TestClock>,
        late_by: Duration,
        runs: u64,
        gate: Arc<Script>,
    }

    impl Probe for LateSecond {
        fn run(&mut self) -> Result<(), EngineError> {
            self.runs += 1;
            match self.runs {
                1 => {}
                2 => self.clock.advance(self.late_by),
                _ => {
                    let mut guard = self.gate.lock.lock().unwrap();
                    while !self.gate.released.load(Ordering::SeqCst) {
                        guard = self.gate.wake.wait(guard).unwrap();
                    }
                }
            }
            Ok(())
        }

        fn interrupt_handle(&self) -> Arc<dyn Interrupt> {
            Arc::clone(&self.gate) as Arc<dyn Interrupt>
        }
    }

    /// A probe that answers past its budget is `slow`, and its answer is
    /// no success, even when the watchdog has not flagged it yet: under
    /// the CPU starvation `slow` describes, the watchdog thread can be the
    /// one that runs late. The budget here is an hour of real time, so the
    /// watchdog never fires; the probe's own elapsed time on the sample
    /// clock is what runs past it. The success before it stays the latest.
    #[test]
    fn engine_sample_judges_a_late_answer_by_its_own_elapsed_time() {
        const PATIENT: SampleConfig = SampleConfig {
            interval: Duration::from_millis(20),
            budget: Duration::from_hours(1),
            success_window: Duration::from_hours(4),
        };
        let clock = TestClock::new();
        let on_time = clock.now();
        let gate = Script::new(&[], Step::Answer);
        let sample = EngineSample::spawn(
            LateSecond {
                clock: Arc::clone(&clock),
                late_by: PATIENT.budget + Duration::from_millis(1),
                runs: 0,
                gate: Arc::clone(&gate),
            },
            PATIENT,
            Arc::clone(&clock) as Arc<dyn Clock>,
        )
        .unwrap();
        until("the late answer is published", || published(&sample) >= 2);
        {
            let state = sample.shared.lock();
            assert_eq!(state.latest, Some(Outcome::Overran), "past its budget");
            assert_eq!(
                state.last_success,
                Some(on_time),
                "a late answer is not a success"
            );
        }
        assert_eq!(sample.read(), EngineCheck::Slow);
        assert_eq!(gate.interrupts.load(Ordering::SeqCst), 0, "no watchdog");
        gate.release();
    }

    /// Sixty seconds with no success turn `slow` into `error`, on the
    /// injected clock: a probe that keeps overrunning after one success
    /// reads `slow` for the first 60 s and `error` from then on, and the
    /// age keeps growing while the wedged probe runs.
    #[test]
    fn engine_sample_reads_error_after_sixty_seconds_without_success() {
        let script = Script::new(&[Step::Answer], Step::Wedge);
        let clock = TestClock::new();
        let sample = EngineSample::spawn(
            ScriptedProbe(Arc::clone(&script)),
            FAST,
            Arc::clone(&clock) as Arc<dyn Clock>,
        )
        .unwrap();
        sample.wait_for_first();
        assert_eq!(sample.read(), EngineCheck::Ok);
        until("the wedged probe overruns", || {
            script.interrupts.load(Ordering::SeqCst) == 1
        });
        let age = sample.age();
        clock.advance(Duration::from_secs(59));
        assert_eq!(sample.read(), EngineCheck::Slow);
        assert!(sample.age() >= age + Duration::from_secs(59));
        clock.advance(Duration::from_secs(1));
        assert_eq!(sample.read(), EngineCheck::Error, "60 s without success");
        script.release();
    }

    /// A success that is not recent enough reads `error` even when it is
    /// the latest sample, as when the sampler has stopped.
    #[test]
    fn engine_sample_ages_out_a_stale_success() {
        let script = Script::new(&[], Step::Answer);
        let clock = TestClock::new();
        let sample = EngineSample::spawn(
            ScriptedProbe(script),
            FAST,
            Arc::clone(&clock) as Arc<dyn Clock>,
        )
        .unwrap();
        sample.wait_for_first();
        let _hold = sample.hold();
        assert_eq!(sample.read(), EngineCheck::Ok);
        clock.advance(SUCCESS_WINDOW);
        assert_eq!(sample.read(), EngineCheck::Error);
    }

    /// A panic in the sampler publishes `error` and stops it: no probe
    /// starts after it, and the check never recovers.
    #[test]
    fn engine_sample_panic_publishes_error_and_stops() {
        let script = Script::new(&[Step::Answer, Step::Panic], Step::Answer);
        let sample =
            EngineSample::spawn(ScriptedProbe(Arc::clone(&script)), FAST, TestClock::new())
                .unwrap();
        sample.wait_for_first();
        assert_eq!(sample.read(), EngineCheck::Ok);
        until("the panic is published", || published(&sample) >= 2);
        assert_eq!(sample.read(), EngineCheck::Error);
        std::thread::sleep(FAST.interval * 5);
        assert_eq!(sample.probes_started(), 2, "nothing runs after a panic");
        assert_eq!(sample.read(), EngineCheck::Error);
    }

    /// Reading starts no probe: 50 concurrent readers, with the sampler
    /// held so only a reader could start one, leave the count unchanged.
    #[test]
    fn engine_sample_reads_start_no_probe() {
        let script = Script::new(&[], Step::Answer);
        let sample = EngineSample::spawn(ScriptedProbe(script), FAST, TestClock::new()).unwrap();
        sample.wait_for_first();
        let _hold = sample.hold();
        let started = sample.probes_started();
        std::thread::scope(|scope| {
            for _ in 0..50 {
                scope.spawn(|| assert_eq!(sample.read(), EngineCheck::Ok));
            }
        });
        assert_eq!(sample.probes_started(), started);
    }

    /// Answers once, then runs a statement far past any budget.
    struct Endless {
        connection: DiagnosticConnection,
        answered: bool,
    }

    impl Probe for Endless {
        fn run(&mut self) -> Result<(), EngineError> {
            if !std::mem::replace(&mut self.answered, true) {
                return self.connection.select_one();
            }
            self.connection
                .select_for_test("SELECT max(md5(range::VARCHAR)) FROM range(1000000000000)")
        }

        fn interrupt_handle(&self) -> Arc<dyn Interrupt> {
            self.connection.interrupt_handle()
        }
    }

    /// The production probe on a real query database answers `ok`. Then a
    /// probe on the same database whose statements run far past the
    /// budget, after one success: the watchdog interrupts each, and the
    /// interrupt `DuckDB` returns reads `slow`, not `error`.
    #[test]
    fn engine_sample_reads_duckdb_s_own_interrupt_as_slow() {
        let pool = crate::pool::ExecutorPool::for_test(String::new(), 1, 10, None).unwrap();
        let sample = EngineSample::start(&pool).unwrap();
        sample.wait_for_first();
        assert_eq!(sample.read(), EngineCheck::Ok);
        drop(sample);

        let sample = EngineSample::spawn(
            Endless {
                connection: pool.diagnostic_connection().unwrap(),
                answered: false,
            },
            FAST,
            Arc::new(MonotonicClock),
        )
        .unwrap();
        sample.wait_for_first();
        assert_eq!(sample.read(), EngineCheck::Ok);
        // The second probe was interrupted and came back: a third started.
        until("an interrupted statement returns", || {
            sample.probes_started() >= 3
        });
        assert_eq!(sample.read(), EngineCheck::Slow);
        assert_eq!(sample.shared.lock().latest, Some(Outcome::Overran));
    }
}
