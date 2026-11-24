// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The repin job's two compaction interlocks (ADR-0011).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::{OwnedRwLockReadGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Shared interlock between the repin engine and the compaction loop.
#[derive(Debug)]
pub struct RepinCoordinator {
    corpus_gate: Arc<RwLock<()>>,
    rollup_pauses: AtomicUsize,
    /// Test-only widening of the cutover pause, held on the coordinator
    /// rather than in a static so one test's widened pause cannot leak
    /// into another test's job in the same binary.
    #[cfg(any(test, feature = "test-support"))]
    cutover_hold: CutoverHold,
}

#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Default)]
struct CutoverHold {
    millis: std::sync::atomic::AtomicU64,
    active: std::sync::atomic::AtomicBool,
}

impl Default for RepinCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl RepinCoordinator {
    #[must_use]
    pub fn new() -> Self {
        Self {
            corpus_gate: Arc::new(RwLock::new(())),
            rollup_pauses: AtomicUsize::new(0),
            #[cfg(any(test, feature = "test-support"))]
            cutover_hold: CutoverHold::default(),
        }
    }

    pub async fn compaction_guard(&self) -> RwLockReadGuard<'_, ()> {
        self.corpus_gate.read().await
    }

    /// [`Self::compaction_guard`], owned so a blocking task (publication
    /// marker recovery) keeps it after cancellation of its async caller.
    pub(crate) async fn compaction_guard_owned(&self) -> OwnedRwLockReadGuard<()> {
        Arc::clone(&self.corpus_gate).read_owned().await
    }

    pub async fn cutover_guard(&self) -> RwLockWriteGuard<'_, ()> {
        self.corpus_gate.write().await
    }

    #[must_use]
    pub fn pause_rollup(self: &Arc<Self>) -> RollupPause {
        self.rollup_pauses.fetch_add(1, Ordering::SeqCst);
        RollupPause {
            coordinator: Arc::clone(self),
        }
    }

    /// A cheap early-out for the whole pass; the binding decision for a unit
    /// about to move a file is [`Self::rollup_unit_guard`].
    #[must_use]
    pub fn rollup_paused(&self) -> bool {
        self.rollup_pauses.load(Ordering::SeqCst) > 0
    }

    /// The read guard is taken first and the pause is read under it, and
    /// that order is the whole point. Read under the guard, `false` is
    /// binding: no cutover can start while a read guard is held.
    pub async fn rollup_unit_guard(&self) -> Option<RwLockReadGuard<'_, ()>> {
        let guard = self.corpus_gate.read().await;
        (!self.rollup_paused()).then_some(guard)
    }

    /// The same claim as [`Self::rollup_unit_guard`], owned so a blocking
    /// recovery task can retain it after cancellation of its async caller.
    pub(crate) async fn rollup_unit_guard_owned(&self) -> Option<OwnedRwLockReadGuard<()>> {
        let guard = Arc::clone(&self.corpus_gate).read_owned().await;
        (!self.rollup_paused()).then_some(guard)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_cutover_hold_ms(&self, millis: u64) {
        self.cutover_hold
            .millis
            .store(millis, std::sync::atomic::Ordering::SeqCst);
    }

    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn cutover_hold_active(&self) -> bool {
        self.cutover_hold
            .active
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Test-only: the cutover's own side of that hold, called with both
    /// exclusion guards held.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) async fn hold_cutover_for_tests(&self) {
        let millis = self
            .cutover_hold
            .millis
            .load(std::sync::atomic::Ordering::SeqCst);
        if millis == 0 {
            return;
        }
        self.cutover_hold
            .active
            .store(true, std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(millis)).await;
        self.cutover_hold
            .active
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// One count of the rollup pause. A leaked guard would suppress
/// consolidation forever.
#[derive(Debug)]
pub struct RollupPause {
    coordinator: Arc<RepinCoordinator>,
}

impl Drop for RollupPause {
    fn drop(&mut self) {
        self.coordinator
            .rollup_pauses
            .fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cutover_excludes_compaction_batches() {
        let c = Arc::new(RepinCoordinator::new());

        let batch = c.compaction_guard().await;
        let c2 = Arc::clone(&c);
        let cutover = tokio::spawn(async move {
            let _guard = c2.cutover_guard().await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !cutover.is_finished(),
            "the cutover must wait for the in-flight batch"
        );
        drop(batch);
        cutover.await.expect("cutover proceeds once batches drain");

        let held = c.cutover_guard().await;
        let c3 = Arc::clone(&c);
        let batch = tokio::spawn(async move {
            let _guard = c3.compaction_guard().await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!batch.is_finished(), "no batch may straddle the cutover");
        drop(held);
        batch.await.expect("batch proceeds after the cutover");
    }

    #[tokio::test]
    async fn rollup_pause_is_raii() {
        let c = Arc::new(RepinCoordinator::new());
        assert!(!c.rollup_paused());
        {
            let _pause = c.pause_rollup();
            assert!(c.rollup_paused());
        }
        assert!(!c.rollup_paused(), "dropping the guard resumes the rollup");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pause_remains_until_last_owner_drops() {
        let c = Arc::new(RepinCoordinator::new());

        for older_drops_first in [true, false] {
            let older = c.pause_rollup();
            let newer = c.pause_rollup();
            let (first, last) = if older_drops_first {
                (older, newer)
            } else {
                (newer, older)
            };
            drop(first);
            assert!(
                c.rollup_paused(),
                "one guard still lives (older first: {older_drops_first})"
            );
            assert!(c.rollup_unit_guard().await.is_none());
            drop(last);
            assert!(!c.rollup_paused(), "older first: {older_drops_first}");
            assert!(c.rollup_unit_guard().await.is_some());
        }

        let survivor = c.pause_rollup();
        let unwound = {
            let c = Arc::clone(&c);
            std::thread::spawn(move || {
                let _pause = c.pause_rollup();
                panic!("a pause owner unwinds");
            })
        };
        assert!(unwound.join().is_err(), "the owner panicked");
        assert!(
            c.rollup_paused(),
            "an unwinding drop must not resume the rollup under a live guard"
        );
        assert!(c.rollup_unit_guard().await.is_none());
        drop(survivor);
        assert!(!c.rollup_paused());

        let (finish, finished) = tokio::sync::oneshot::channel::<()>();
        let kept = c.pause_rollup();
        let moved = c.pause_rollup();
        let task = tokio::spawn(async move {
            let _moved = moved;
            finished.await.ok();
        });
        drop(kept);
        assert!(
            c.rollup_paused(),
            "the guard moved into the task still lives"
        );
        assert!(c.rollup_unit_guard().await.is_none());
        finish.send(()).unwrap();
        task.await.unwrap();
        assert!(!c.rollup_paused(), "the task dropped the last guard");
        assert!(c.rollup_unit_guard().await.is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rollup_unit_guard_is_the_claim_and_the_gate() {
        let c = Arc::new(RepinCoordinator::new());

        let unit = c
            .rollup_unit_guard()
            .await
            .expect("an unclaimed rollup proceeds");

        let c2 = Arc::clone(&c);
        let cutover = tokio::spawn(async move {
            let _guard = c2.cutover_guard().await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !cutover.is_finished(),
            "the cutover must wait out the rollup unit in flight"
        );

        // The pause lands while that unit runs — as it does for a job whose
        // scan outlived the start of the rollup pass.
        let pause = c.pause_rollup();
        drop(unit);
        cutover.await.expect("cutover proceeds once the unit ends");

        assert!(
            c.rollup_unit_guard().await.is_none(),
            "every later unit stands down without touching a file"
        );
        drop(pause);
        assert!(
            c.rollup_unit_guard().await.is_some(),
            "consolidation resumes once the job releases its claim"
        );
    }
}
