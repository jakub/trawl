// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The query preparation allowance (ADR-0063, amending ADR-0054).
//!
//! Preparing query text means parsing it, validating it and building its
//! patterns, and near-limit patterns can spend about 0.85 s of CPU doing
//! it. A reactor thread that spends that time stalls every task scheduled
//! on it, health and metrics included. So every door that takes query text
//! prepares on the blocking pool, through [`Preparation::run`], behind a
//! fixed number of slots.
//!
//! A request and the job it dispatched are two lifetimes, as a request and
//! its executor work are (ADR-0024). The slot belongs to the job
//! ([`PrepJob`]), which the blocking closure owns and drops last. So a job
//! whose request stopped waiting keeps its slot, and whatever its work
//! holds, such as a regex reservation, until its closure exits. When the
//! request stops waiting, at its deadline or because its caller went away,
//! its guard ([`Waiting`]) requests the job's [`Stop`], and the work ends at
//! its next cancellation point.
//!
//! The allowance shares only its shape with the ADR-0054 allowances: a
//! size fixed in code, gauges, a fixed refusal sentence and no
//! `Retry-After`. A job never holds or awaits an executor permit.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use parking_lot::Mutex;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use trawl_core::cancel::Stop;

use crate::deadline::Deadline;
use crate::error::ServerError;
#[cfg(any(test, feature = "test-support"))]
use crate::pool::seam;

/// The allowance's size: half the cores, at least 2, at most the executor
/// permits. With one permit the allowance is one slot.
#[must_use]
pub fn allowance(cores: usize, permits: usize) -> usize {
    (cores / 2).max(2).min(permits)
}

/// Which preparation a job runs, for its log events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Door {
    /// [`crate::admission::admit`]: the parse and validation every door
    /// that runs query text shares.
    Admission,
    /// `POST /api/v1/validate`: the parse, validation and formatting its
    /// answer reports.
    Validate,
    /// Live-stream setup: admission and the live compile, in one job.
    Stream,
    /// Capture planning: admission and the bounds, in one job.
    Capture,
}

impl Door {
    /// The stable log spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Admission => "admission",
            Self::Validate => "validate",
            Self::Stream => "stream",
            Self::Capture => "capture",
        }
    }
}

/// The counts behind the retained-job reads.
#[derive(Debug, Default)]
struct Counters {
    /// Jobs whose request stopped waiting and whose closure has not exited.
    retained: AtomicUsize,
    /// Retained jobs whose slot came back. Each job counts once.
    reclaimed: AtomicU64,
}

/// The preparation allowance: one slot for each preparation job running
/// on the blocking pool. Clones share the slots.
#[derive(Debug, Clone)]
pub struct Preparation {
    slots: Arc<Semaphore>,
    size: usize,
    counters: Arc<Counters>,
    /// The seams of the pool that owns this allowance.
    #[cfg(any(test, feature = "test-support"))]
    seams: Arc<seam::Table>,
}

impl Preparation {
    /// An allowance of `size` slots.
    pub(crate) fn new(size: usize) -> Self {
        Self {
            slots: Arc::new(Semaphore::new(size)),
            size,
            counters: Arc::default(),
            #[cfg(any(test, feature = "test-support"))]
            seams: Arc::default(),
        }
    }

    /// Reach the seams of the pool that owns this allowance.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub(crate) fn with_seams(mut self, seams: Arc<seam::Table>) -> Self {
        self.seams = seams;
        self
    }

    /// How many slots the allowance has.
    #[must_use]
    pub fn size(&self) -> usize {
        self.size
    }

    /// How many slots are held right now, by jobs a request waits on and
    /// by retained jobs.
    #[must_use]
    pub fn in_use(&self) -> usize {
        self.size.saturating_sub(self.slots.available_permits())
    }

    /// How many jobs hold a slot that no request waits on any more.
    #[must_use]
    pub fn retained(&self) -> usize {
        self.counters.retained.load(Ordering::SeqCst)
    }

    /// How many retained jobs have given their slot back since the
    /// allowance was made.
    #[must_use]
    pub fn reclaimed(&self) -> u64 {
        self.counters.reclaimed.load(Ordering::SeqCst)
    }

    /// Run `work` on the blocking pool once a slot is free, and answer with
    /// what it returns.
    ///
    /// The wait for a slot spends `deadline` like every other wait
    /// (ADR-0024). Once a slot is taken it belongs to the job, which keeps
    /// it until its closure exits, past `deadline` and past a caller that
    /// walked away. Either asks `work` to stop through its [`Stop`]. The
    /// task is never aborted: `work` ends at its next cancellation point.
    ///
    /// # Errors
    ///
    /// - [`ServerError::PreparationUnavailable`] when no slot came free
    ///   before `deadline`, or `deadline` was spent when one did. Nothing
    ///   ran.
    /// - [`ServerError::Timeout`] when `deadline` passed while the job ran.
    /// - [`ServerError::Panicked`] when `work` panicked. Its slot is back.
    pub async fn run<T, F>(&self, deadline: Deadline, door: Door, work: F) -> Result<T, ServerError>
    where
        F: FnOnce(&Stop) -> T + Send + 'static,
        T: Send + 'static,
    {
        let slot = match deadline.run(Arc::clone(&self.slots).acquire_owned()).await {
            // A free slot is taken on the first poll, before the wait reads
            // the deadline, so a spent deadline is checked here too: a job
            // is never dispatched after its deadline.
            Ok(Ok(slot)) if !deadline.expired() => slot,
            // Nothing closes the semaphore.
            Ok(Err(_closed)) => {
                return Err(ServerError::Internal(
                    "query preparation allowance closed".into(),
                ));
            }
            Ok(Ok(_)) | Err(crate::deadline::Expired) => {
                tracing::info!(
                    event_type = "preparation_refused",
                    door = door.as_str(),
                    slots = self.size,
                    "query preparation refused: no slot came free before the deadline"
                );
                return Err(ServerError::PreparationUnavailable);
            }
        };
        let stop = Stop::new();
        let shared = Arc::new(Mutex::new(JobState::default()));
        let job = PrepJob {
            slot: Some(slot),
            shared: Arc::clone(&shared),
            counters: Arc::clone(&self.counters),
            door,
        };
        let (answer, mut answered) = tokio::sync::oneshot::channel();
        #[cfg(any(test, feature = "test-support"))]
        let seams = Arc::clone(&self.seams);
        let job_stop = stop.clone();
        drop(tokio::task::spawn_blocking(move || {
            #[cfg(any(test, feature = "test-support"))]
            seams.reach(seam::Seam::Preparing);
            // The payload drops unread: it can quote the query (ADR-0040).
            let out =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&job_stop))).ok();
            #[cfg(any(test, feature = "test-support"))]
            seams.reach(seam::Seam::Prepared);
            // With no request waiting, the answer and everything it holds
            // drop here, while the job still holds its slot.
            drop(answer.send(out));
            drop(job);
        }));

        let mut waiting = Waiting {
            stop,
            shared,
            counters: Arc::clone(&self.counters),
            door,
            armed: true,
            timed_out: false,
        };
        tokio::select! {
            biased;
            out = &mut answered => {
                waiting.armed = false;
                match out {
                    Ok(Some(out)) => Ok(out),
                    Ok(None) => Err(ServerError::Panicked("query preparation")),
                    Err(_dropped) => Err(ServerError::Internal(
                        "query preparation ended without an answer".into(),
                    )),
                }
            }
            () = tokio::time::sleep_until(deadline.instant()) => {
                waiting.timed_out = true;
                drop(waiting);
                Err(ServerError::Timeout)
            }
        }
    }
}

/// What a job and the request waiting on it decide together, under one
/// lock: whether the job outlived its request.
#[derive(Debug, Default)]
struct JobState {
    /// The job's closure is ending: its slot is on its way back.
    done: bool,
    /// When the request stopped waiting on a job that was still running.
    retained_since: Option<Instant>,
}

/// What one preparation job holds: its slot. The blocking closure owns it
/// and drops it last, so a timeout, a caller that walked away, a panic and
/// an ordinary finish all give the slot back in the same place, once.
struct PrepJob {
    slot: Option<OwnedSemaphorePermit>,
    shared: Arc<Mutex<JobState>>,
    counters: Arc<Counters>,
    door: Door,
}

impl Drop for PrepJob {
    fn drop(&mut self) {
        // Read in the critical section the request stamps in, so a job is
        // either retained here or was done before its request stopped.
        let retained_since = {
            let mut state = self.shared.lock();
            state.done = true;
            state.retained_since
        };
        drop(self.slot.take());
        if let Some(since) = retained_since {
            self.counters.retained.fetch_sub(1, Ordering::SeqCst);
            self.counters.reclaimed.fetch_add(1, Ordering::SeqCst);
            tracing::info!(
                event_type = "preparation_reclaimed",
                door = self.door.as_str(),
                retained_ms = u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX),
                "retained query preparation slot reclaimed"
            );
        }
    }
}

/// The request side of one job, armed while the request waits on it.
///
/// Dropped armed, at the deadline or because the caller walked away, it
/// asks the job to stop and stamps the job retained if it is still
/// running. The job's own drop then reclaims the slot.
struct Waiting {
    stop: Stop,
    shared: Arc<Mutex<JobState>>,
    counters: Arc<Counters>,
    door: Door,
    armed: bool,
    timed_out: bool,
}

impl Drop for Waiting {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.stop.request();
        let retained = {
            let mut state = self.shared.lock();
            let retained = !state.done && state.retained_since.is_none();
            if retained {
                state.retained_since = Some(Instant::now());
                self.counters.retained.fetch_add(1, Ordering::SeqCst);
            }
            retained
        };
        if retained {
            tracing::info!(
                event_type = "preparation_retained",
                door = self.door.as_str(),
                reason = if self.timed_out {
                    "deadline"
                } else {
                    "abandoned"
                },
                "the request stopped waiting; its preparation keeps its slot until it stops"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll, Waker};
    use std::thread::ThreadId;
    use std::time::Duration;

    use super::{Door, Preparation, allowance};
    use crate::deadline::Deadline;
    use crate::error::ServerError;
    use crate::pool::seam::{Seam, Session, Table};

    /// An allowance of `size` slots, and a test's hold on its seams.
    fn allowance_of(size: usize) -> (Preparation, Session) {
        let table = Arc::new(Table::default());
        (
            Preparation::new(size).with_seams(Arc::clone(&table)),
            Session(table),
        )
    }

    /// A deadline no test reaches.
    fn far() -> Deadline {
        Deadline::after(Duration::from_secs(600))
    }

    /// Yield until `reached`, bounded by the real clock so a deadlock fails
    /// the test instead of hanging it.
    async fn until(label: &str, mut reached: impl FnMut() -> bool) {
        let start = std::time::Instant::now();
        while !reached() {
            assert!(
                start.elapsed() < Duration::from_secs(20),
                "never reached: {label}"
            );
            tokio::task::yield_now().await;
        }
    }

    /// Poll `fut` once, waking nothing.
    fn poll_once<F: Future + Unpin>(fut: &mut F) -> Poll<F::Output> {
        Pin::new(fut).poll(&mut Context::from_waker(Waker::noop()))
    }

    /// What a job's answer saw when it dropped: the slots in use, and the
    /// thread it dropped on.
    type Dropped = Arc<Mutex<Option<(usize, ThreadId)>>>;

    /// A job's answer that records where it dropped, as a regex
    /// reservation held in an answer would be released.
    struct Held {
        preparation: Preparation,
        dropped: Dropped,
    }

    impl Drop for Held {
        fn drop(&mut self) {
            *self.dropped.lock().unwrap() =
                Some((self.preparation.in_use(), std::thread::current().id()));
        }
    }

    #[test]
    fn preparation_allowance_sizing() {
        for ((cores, permits), size) in [
            ((16, 8), 8),
            ((4, 8), 2),
            ((2, 8), 2),
            ((1, 8), 2),
            ((16, 1), 1),
            ((16, 3), 3),
        ] {
            assert_eq!(
                allowance(cores, permits),
                size,
                "{cores} cores and {permits} permits"
            );
        }
    }

    /// The pool builds its allowance from its own permits: one permit is
    /// one slot, whatever the host's cores.
    #[test]
    fn the_pool_sizes_its_allowance_from_its_permits() {
        let pool =
            crate::pool::ExecutorPool::for_test("/nonexistent".into(), 1, 100, None).unwrap();
        assert_eq!(pool.preparation().size(), 1);
        let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
        let pool =
            crate::pool::ExecutorPool::for_test("/nonexistent".into(), 3, 100, None).unwrap();
        assert_eq!(pool.preparation().size(), allowance(cores, 3));
    }

    /// The test thread is the runtime's only thread. With a job parked on
    /// its seam, an unrelated task still runs to completion.
    #[tokio::test(flavor = "current_thread")]
    async fn parked_preparation_does_not_block_the_runtime() {
        let (preparation, seams) = allowance_of(2);
        let parked = seams.hold(Seam::Preparing);

        // A runtime the job blocks would never get here to release it, so
        // a watchdog lets the job go after 20 s and says it had to.
        let (done, watched) = std::sync::mpsc::channel::<()>();
        let rescued = Arc::new(AtomicBool::new(false));
        let watchdog = std::thread::spawn({
            let parked = Arc::clone(&parked);
            let rescued = Arc::clone(&rescued);
            move || {
                if watched.recv_timeout(Duration::from_secs(20)).is_err() {
                    rescued.store(true, std::sync::atomic::Ordering::SeqCst);
                    parked.release();
                }
            }
        });

        let job = tokio::spawn({
            let preparation = preparation.clone();
            async move { preparation.run(far(), Door::Admission, |_stop| 7).await }
        });
        until("the job parks", || parked.arrivals() == 1).await;
        let unrelated = tokio::spawn(async {
            tokio::task::yield_now().await;
            42
        });
        assert_eq!(unrelated.await.unwrap(), 42);
        assert!(
            !rescued.load(std::sync::atomic::Ordering::SeqCst),
            "the unrelated task ran only after the parked job was let go"
        );
        assert_eq!(preparation.in_use(), 1, "the job is still parked");

        done.send(()).unwrap();
        watchdog.join().unwrap();
        parked.release();
        assert_eq!(job.await.unwrap().unwrap(), 7);
    }

    /// With every slot held by a parked job, a further job waits for a slot
    /// and, when none frees before its deadline, is refused without
    /// running. A job that finds a slot freeing in time runs.
    #[tokio::test]
    async fn a_full_allowance_waits_then_refuses_at_the_deadline() {
        let (preparation, seams) = allowance_of(2);
        let parked = seams.hold(Seam::Preparing);
        let holders: Vec<_> = (0..2)
            .map(|_| {
                let preparation = preparation.clone();
                tokio::spawn(async move { preparation.run(far(), Door::Admission, |_| ()).await })
            })
            .collect();
        until("both slots are held", || parked.arrivals() == 2).await;
        assert_eq!(preparation.in_use(), 2);

        let budget = Duration::from_millis(200);
        let asked = tokio::time::Instant::now();
        let refused = preparation
            .run(Deadline::after(budget), Door::Admission, |_| ())
            .await;
        assert!(
            matches!(refused, Err(ServerError::PreparationUnavailable)),
            "{refused:?}"
        );
        assert!(asked.elapsed() >= budget, "it waited for its deadline");
        assert_eq!(parked.arrivals(), 2, "the refused job never ran");

        let waiter = tokio::spawn({
            let preparation = preparation.clone();
            async move { preparation.run(far(), Door::Admission, |_| 3).await }
        });
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
        assert_eq!(parked.arrivals(), 2, "the waiter has no slot yet");
        parked.release();
        assert_eq!(waiter.await.unwrap().unwrap(), 3);
        for holder in holders {
            holder.await.unwrap().unwrap();
        }
        until("every slot is back", || preparation.in_use() == 0).await;
        assert_eq!(preparation.reclaimed(), 0, "no job outlived its request");
    }

    /// A job whose request timed out keeps its slot until its closure
    /// exits. Its work is asked to stop, its answer drops inside the
    /// closure while the slot is still held, and the slot comes back once.
    #[tokio::test]
    async fn a_timed_out_job_keeps_its_slot_until_its_closure_exits() {
        let (preparation, seams) = allowance_of(2);
        let parked = seams.hold(Seam::Preparing);
        let dropped = Dropped::default();
        let saw_stop = Arc::new(AtomicBool::new(false));

        let answer = preparation
            .run(
                Deadline::after(Duration::from_millis(100)),
                Door::Admission,
                {
                    let preparation = preparation.clone();
                    let dropped = Arc::clone(&dropped);
                    let saw_stop = Arc::clone(&saw_stop);
                    move |stop| {
                        saw_stop.store(stop.requested(), std::sync::atomic::Ordering::SeqCst);
                        Held {
                            preparation,
                            dropped,
                        }
                    }
                },
            )
            .await;
        assert!(matches!(answer, Err(ServerError::Timeout)), "timed out");
        assert_eq!(parked.arrivals(), 1);
        assert_eq!(preparation.in_use(), 1, "the parked job keeps its slot");
        assert_eq!(preparation.retained(), 1);
        assert_eq!(preparation.reclaimed(), 0);

        parked.release();
        until("the slot comes back", || preparation.reclaimed() == 1).await;
        assert_eq!(preparation.in_use(), 0, "exactly one slot came back");
        assert_eq!(preparation.retained(), 0);
        assert!(
            saw_stop.load(std::sync::atomic::Ordering::SeqCst),
            "the work saw its stop"
        );
        let (in_use, thread) = dropped.lock().unwrap().expect("the answer dropped");
        assert_eq!(in_use, 1, "the answer dropped while the job held its slot");
        assert_ne!(thread, std::thread::current().id(), "inside the closure");
        assert_eq!(preparation.reclaimed(), 1, "reclaimed once");
    }

    /// A caller that walks away is a request that stopped waiting: its job
    /// is asked to stop and keeps its slot until its closure exits.
    #[tokio::test]
    async fn an_abandoned_job_keeps_its_slot_until_its_closure_exits() {
        let (preparation, seams) = allowance_of(2);
        let parked = seams.hold(Seam::Preparing);
        let saw_stop = Arc::new(AtomicBool::new(false));
        let request = tokio::spawn({
            let preparation = preparation.clone();
            let saw_stop = Arc::clone(&saw_stop);
            async move {
                preparation
                    .run(far(), Door::Admission, move |stop| {
                        saw_stop.store(stop.requested(), std::sync::atomic::Ordering::SeqCst);
                    })
                    .await
            }
        });
        until("the job parks", || parked.arrivals() == 1).await;
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        assert_eq!(preparation.in_use(), 1, "the parked job keeps its slot");
        assert_eq!(preparation.retained(), 1);

        parked.release();
        until("the slot comes back", || preparation.reclaimed() == 1).await;
        assert_eq!(preparation.in_use(), 0);
        assert!(saw_stop.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// A panic in the work gives the slot back and answers the internal
    /// error, with a fixed label and nothing of the payload.
    #[tokio::test]
    async fn a_panic_in_the_work_returns_its_slot() {
        let (preparation, _seams) = allowance_of(1);
        let answer: Result<u32, _> = preparation
            .run(far(), Door::Admission, |_| {
                panic!("zz-prepare-panic-payload");
            })
            .await;
        let err = answer.expect_err("the work panicked");
        assert!(matches!(err, ServerError::Panicked(_)), "{err:?}");
        assert_eq!(err.error_class(), "panic");
        assert!(!err.to_string().contains("zz-prepare-panic-payload"));
        until("the slot comes back", || preparation.in_use() == 0).await;
        assert_eq!(preparation.retained(), 0);

        assert_eq!(
            preparation
                .run(far(), Door::Admission, |_| 5)
                .await
                .unwrap(),
            5,
            "the slot is usable again"
        );
    }

    /// A request whose deadline is already spent when a slot is free is
    /// refused before its job is dispatched: the slot goes back and the
    /// work never runs.
    #[tokio::test]
    async fn a_spent_deadline_is_refused_before_dispatch() {
        let (preparation, _seams) = allowance_of(1);
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let job_ran = Arc::clone(&ran);
        let outcome = preparation
            .run(
                Deadline::after(Duration::ZERO),
                Door::Admission,
                move |_| {
                    job_ran.store(true, std::sync::atomic::Ordering::SeqCst);
                },
            )
            .await;

        assert!(
            matches!(outcome, Err(ServerError::PreparationUnavailable)),
            "{outcome:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !ran.load(std::sync::atomic::Ordering::SeqCst),
            "the work ran"
        );
        assert_eq!(preparation.in_use(), 0);
        assert_eq!(preparation.retained(), 0);
    }

    /// A job's answer and its deadline can be ready at the same poll. The
    /// answer wins, and nothing is left retained.
    #[tokio::test]
    async fn an_answer_ready_with_the_deadline_wins() {
        let (preparation, seams) = allowance_of(1);
        let prepared = seams.hold(Seam::Prepared);
        let mut request = Box::pin(preparation.run(
            Deadline::after(Duration::from_millis(100)),
            Door::Admission,
            |_| 7,
        ));
        assert!(poll_once(&mut request).is_pending(), "dispatched");
        until("the work ran", || prepared.arrivals() == 1).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        prepared.release();
        // The job sends its answer before it gives its slot back.
        until("the answer is sent", || preparation.in_use() == 0).await;

        assert_eq!(request.await.unwrap(), 7);
        assert_eq!(preparation.retained(), 0);
        assert_eq!(preparation.reclaimed(), 0);
    }

    /// When the deadline is ready first, the request times out, and the
    /// answer the job finishes afterwards drops inside its closure while
    /// it still holds its slot. The slot comes back once.
    #[tokio::test]
    async fn an_answer_after_the_deadline_drops_in_its_job() {
        let (preparation, seams) = allowance_of(1);
        let prepared = seams.hold(Seam::Prepared);
        let dropped = Dropped::default();
        let answer = preparation
            .run(
                Deadline::after(Duration::from_millis(100)),
                Door::Admission,
                {
                    let preparation = preparation.clone();
                    let dropped = Arc::clone(&dropped);
                    move |_| Held {
                        preparation,
                        dropped,
                    }
                },
            )
            .await;
        assert!(matches!(answer, Err(ServerError::Timeout)), "timed out");
        assert_eq!(prepared.arrivals(), 1, "the work had run");
        assert!(
            dropped.lock().unwrap().is_none(),
            "the answer is still held"
        );
        assert_eq!(preparation.retained(), 1);

        prepared.release();
        until("the slot comes back", || preparation.reclaimed() == 1).await;
        assert_eq!(preparation.in_use(), 0);
        let (in_use, thread) = dropped.lock().unwrap().expect("the answer dropped");
        assert_eq!(in_use, 1, "the answer dropped while the job held its slot");
        assert_ne!(thread, std::thread::current().id(), "inside the closure");
    }

    /// Preparation takes no executor permit: with every permit held, a job
    /// still runs.
    #[tokio::test]
    async fn a_preparation_takes_no_executor_permit() {
        let pool =
            crate::pool::ExecutorPool::for_test("/nonexistent".into(), 1, 100, None).unwrap();
        let _every_permit = pool.exclusive(Duration::from_secs(5)).await.unwrap();
        assert_eq!(pool.available_permits(), 0);
        let answer = pool
            .prepare(
                Deadline::after(Duration::from_secs(5)),
                Door::Admission,
                |_| 11,
            )
            .await;
        assert_eq!(answer.unwrap(), 11);
    }
}
