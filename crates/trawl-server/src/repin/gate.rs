// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The repin job's two compaction interlocks (ADR-0011 slice B).
//!
//! 1. **The corpus gate** — a `tokio::sync::RwLock` whose READ side wraps
//!    each compaction batch's pin-snapshot → conform → publish phase and
//!    whose WRITE side the cutover holds across its final catch-up
//!    increment, the per-env swap and the pin flip. Compaction snapshotting
//!    its pins INSIDE the read guard is what makes the cutover's pin flip
//!    safe without a stamp-and-validate protocol: no batch can conform
//!    against the old pin and publish after the flip.
//! 2. **The rollup pause** — a `watch`-backed flag suppressing the
//!    file-RELOCATING daily rollup for the WHOLE job (RAII guard), so the
//!    catch-up diff stays additive: compaction only ever adds or replaces
//!    hour files while the shadow is being built, never moves them across
//!    paths. WAL→parquet draining is never paused by this flag — only the
//!    few seconds under the write guard pause it, and the hot buffer keeps
//!    every undrained event queryable throughout (ADR-0008).
//!
//! A flag alone would only stop a rollup that has not STARTED: the job
//! claims the pause after a scan the rollup may already be running behind,
//! and each of its day/service merges is a minutes-long relocation. So the
//! two primitives are used TOGETHER, through [`RepinCoordinator::rollup_unit_guard`]
//! — every relocating unit runs under the corpus gate's read side and reads
//! the pause under it. The cutover therefore waits out the one unit already
//! in flight, and every later unit stands down.

use std::sync::Arc;

use tokio::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard, watch};

/// Shared interlock between the repin engine and the compaction loop.
#[derive(Debug)]
pub struct RepinCoordinator {
    corpus_gate: RwLock<()>,
    rollup_pause: watch::Sender<bool>,
    /// Test-only widening of the cutover pause, held on the coordinator
    /// rather than in a static so one test's widened pause cannot leak
    /// into another test's job in the same binary.
    #[cfg(any(test, feature = "test-support"))]
    cutover_hold: CutoverHold,
}

/// Test-only pause widening: how long the cutover sleeps under both
/// exclusion guards, and whether it is sleeping right now.
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
    /// A coordinator with nothing paused and nothing gated.
    #[must_use]
    pub fn new() -> Self {
        let (rollup_pause, _) = watch::channel(false);
        Self {
            corpus_gate: RwLock::new(()),
            rollup_pause,
            #[cfg(any(test, feature = "test-support"))]
            cutover_hold: CutoverHold::default(),
        }
    }

    /// Compaction's side: held (read) across one batch's pin-snapshot →
    /// conform → publish phase. Many batches may run concurrently; none may
    /// straddle a cutover.
    pub async fn compaction_guard(&self) -> RwLockReadGuard<'_, ()> {
        self.corpus_gate.read().await
    }

    /// The cutover's side: exclusive against every compaction batch. WAL
    /// draining pauses only while this is held — seconds, bounded by the
    /// final catch-up increment — and the hot buffer keeps the undrained
    /// events queryable throughout.
    pub async fn cutover_guard(&self) -> RwLockWriteGuard<'_, ()> {
        self.corpus_gate.write().await
    }

    /// Suppress the daily rollup for the life of the returned guard.
    #[must_use]
    pub fn pause_rollup(self: &Arc<Self>) -> RollupPause {
        self.rollup_pause.send_replace(true);
        RollupPause {
            coordinator: Arc::clone(self),
        }
    }

    /// Whether the file-relocating rollup is currently suppressed. A
    /// cheap early-out for the whole pass; the binding decision for a unit
    /// about to move a file is [`Self::rollup_unit_guard`].
    #[must_use]
    pub fn rollup_paused(&self) -> bool {
        *self.rollup_pause.borrow()
    }

    /// One file-relocating rollup unit's claim on the corpus: `Some` =
    /// proceed while holding the returned read guard, `None` = stand down
    /// without touching a file.
    ///
    /// The read guard is taken FIRST and the pause is read UNDER it, and
    /// that order is the whole point. A pause read on its own only says
    /// the rollup was unclaimed *at that instant* — the job could claim it
    /// the next moment, finish its build and swap the shadow in while the
    /// unit was still merging, so the unit's rename would republish a
    /// pre-repin file into the new generation and delete the repinned
    /// hourlies under it. That is precisely the silently-promoted mixed
    /// corpus the whole design exists to exclude (ADR-0011 amendment §2).
    /// Read under the guard, `false` is binding: no cutover can start
    /// while a read guard is held, so it means "no swap can happen before
    /// this unit finishes".
    pub async fn rollup_unit_guard(&self) -> Option<RwLockReadGuard<'_, ()>> {
        let guard = self.corpus_gate.read().await;
        (!self.rollup_paused()).then_some(guard)
    }

    /// Test-only: widen this coordinator's cutover pause to `millis`, so a
    /// test can ingest and inspect the hot buffer inside the one window
    /// that stops WAL draining.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_cutover_hold_ms(&self, millis: u64) {
        self.cutover_hold
            .millis
            .store(millis, std::sync::atomic::Ordering::SeqCst);
    }

    /// Test-only: whether the cutover is sleeping in that widened pause
    /// right now — the test's rising edge into it, and its proof that an
    /// assertion landed inside it.
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

/// RAII guard for the rollup pause: dropping it (job completion, failure,
/// OR an engine task panic unwinding) resumes the rollup — a leaked pause
/// would suppress consolidation forever.
#[derive(Debug)]
pub struct RollupPause {
    coordinator: Arc<RepinCoordinator>,
}

impl Drop for RollupPause {
    fn drop(&mut self) {
        self.coordinator.rollup_pause.send_replace(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cutover cannot begin while a compaction batch holds the read
    /// side — and once it holds the write side, no batch may start.
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

        // And symmetrically.
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

    /// The rollup pause is job-long and RAII: set on acquisition, cleared
    /// on drop — including an unwinding drop.
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

    /// A relocating rollup unit is both claimed AND gated: a pause makes
    /// the next unit stand down, and a unit that got in first excludes the
    /// cutover until it finishes — the flag alone would only stop a rollup
    /// that had not started.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rollup_unit_guard_is_the_claim_and_the_gate() {
        let c = Arc::new(RepinCoordinator::new());

        let unit = c
            .rollup_unit_guard()
            .await
            .expect("an unclaimed rollup proceeds");

        // The cutover cannot swap the shadow in under a unit that is
        // already relocating files.
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
