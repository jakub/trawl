// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Test-only holds on one repin job's lifecycle.
//!
//! The holds live on the [`crate::repin::RepinEngine`] that runs the job,
//! not in a static, so one test's hold cannot reach another engine's job.
//!
//! The hold latches on arrival because a test cannot know the job id in
//! time. A small corpus can reach the post-swap sweep before the request
//! that started the job has answered with the id.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

const DISARMED: i64 = 0;

/// Job ids are positive, so neither sentinel can collide with a latched id.
const NEXT: i64 = -1;

const POLL: Duration = Duration::from_millis(10);

/// A test must release and disarm the hold on every exit, a failing one
/// included: a job held inside a blocking-pool closure would keep the
/// runtime from shutting down.
#[derive(Debug)]
pub struct JobHold {
    /// [`DISARMED`], [`NEXT`], or the id of the job this hold latched.
    job: AtomicI64,
    released: AtomicBool,
}

impl Default for JobHold {
    fn default() -> Self {
        Self {
            job: AtomicI64::new(DISARMED),
            released: AtomicBool::new(false),
        }
    }
}

impl JobHold {
    pub fn arm(&self) {
        self.released.store(false, Ordering::SeqCst);
        self.job.store(NEXT, Ordering::SeqCst);
    }

    #[must_use]
    pub fn held_job(&self) -> Option<i64> {
        let job = self.job.load(Ordering::SeqCst);
        (job > 0).then_some(job)
    }

    pub fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
    }

    pub fn disarm(&self) {
        self.job.store(DISARMED, Ordering::SeqCst);
        self.released.store(true, Ordering::SeqCst);
    }

    pub(crate) async fn reach(&self, job_id: i64) {
        if !self.latch(job_id) {
            return;
        }
        while self.holding(job_id) {
            tokio::time::sleep(POLL).await;
        }
    }

    pub(crate) fn reach_blocking(&self, job_id: i64) {
        if !self.latch(job_id) {
            return;
        }
        while self.holding(job_id) {
            std::thread::sleep(POLL);
        }
    }

    fn latch(&self, job_id: i64) -> bool {
        match self
            .job
            .compare_exchange(NEXT, job_id, Ordering::SeqCst, Ordering::SeqCst)
        {
            Ok(_) => true,
            Err(latched) => latched == job_id,
        }
    }

    fn holding(&self, job_id: i64) -> bool {
        !self.released.load(Ordering::SeqCst) && self.job.load(Ordering::SeqCst) == job_id
    }
}

#[derive(Debug, Default)]
pub struct JobHolds {
    /// Inside the post-swap sweep's blocking closure, before it deletes
    /// anything. The Cleanup marker is written and the cutover's exclusion
    /// guards are dropped, so the job holds no lock and still owns its
    /// staging roots.
    pub before_post_swap_sweep: JobHold,
    /// In the job task, after `run_job` returns, right before that task
    /// drops its share of the repin permit. The sweep is done, the cancel
    /// registry is disarmed, and the job's rollup pause and job-generation
    /// guard are dropped. A second repin held here is refused only by the
    /// permit.
    pub before_permit_release: JobHold,
    panic_next_decision: AtomicBool,
}

impl JobHolds {
    /// Make the next decision task panic right after it claims its rollup
    /// pause, with a pause, a claimed job row and an armed cancel registry
    /// to unwind.
    pub fn panic_next_decision(&self) {
        self.panic_next_decision.store(true, Ordering::SeqCst);
    }

    pub(crate) fn take_decision_panic(&self) -> bool {
        self.panic_next_decision.swap(false, Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_hold_latches_the_first_job_and_passes_the_rest() {
        let hold = std::sync::Arc::new(JobHold::default());
        hold.reach(5).await;
        assert_eq!(hold.held_job(), None, "a disarmed hold holds nothing");

        hold.arm();
        let job_seven = {
            let hold = std::sync::Arc::clone(&hold);
            tokio::spawn(async move { hold.reach(7).await })
        };
        crate::test_timing::until("job 7 latched the hold", || hold.held_job() == Some(7)).await;
        hold.reach(8).await;
        hold.reach_blocking(9);
        assert!(
            !job_seven.is_finished(),
            "job 7 is held until it is released"
        );

        hold.release();
        job_seven.await.unwrap();
        hold.reach(7).await;
        assert_eq!(hold.held_job(), Some(7));

        hold.disarm();
        assert_eq!(hold.held_job(), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn disarming_frees_a_blocking_holder() {
        let hold = std::sync::Arc::new(JobHold::default());
        hold.arm();
        let blocking = {
            let hold = std::sync::Arc::clone(&hold);
            tokio::task::spawn_blocking(move || hold.reach_blocking(3))
        };
        crate::test_timing::until("job 3 latched the hold", || hold.held_job() == Some(3)).await;
        hold.disarm();
        blocking.await.unwrap();
    }
}
