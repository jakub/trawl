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

    /// Whether the file-relocating rollup is currently suppressed.
    #[must_use]
    pub fn rollup_paused(&self) -> bool {
        *self.rollup_pause.borrow()
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
}
