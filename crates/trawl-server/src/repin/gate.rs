// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The repin job's two compaction interlocks (ADR-0011).
//!
//! 1. The corpus gate, a `tokio::sync::RwLock`. The read side wraps each
//!    compaction batch's pin-snapshot, conform and publish phase; the
//!    cutover holds the write side across its final catch-up increment,
//!    the per-env swap and the pin flip. Compaction snapshotting its pins
//!    inside the read guard is what makes the pin flip safe without a
//!    stamp-and-validate protocol: no batch can conform against the old
//!    pin and publish after the flip.
//! 2. The rollup pause, a count of live [`RollupPause`] guards. While any
//!    guard lives the file-relocating daily rollup is suppressed, so the
//!    catch-up diff stays additive: while the shadow is being built,
//!    compaction only adds or replaces hour files, never moves them across
//!    paths. Each guard counts once, so one owner's drop cannot resume the
//!    rollup under another owner that still needs it (ADR-0026). The pause
//!    does not stop WAL to parquet draining; only the few seconds under the
//!    write guard do, and the hot buffer keeps every undrained event
//!    queryable throughout (ADR-0008).
//!
//! A pause alone would only stop a rollup that has not started: the job
//! claims the pause after a scan the rollup may already be running behind,
//! and each of its day/service merges is a minutes-long relocation. So the
//! two primitives are used together, through
//! [`RepinCoordinator::rollup_unit_guard`] — every relocating unit runs
//! under the corpus gate's read side and reads the pause under it. The
//! cutover waits out the one unit already in flight, and every later unit
//! stands down.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::{OwnedRwLockReadGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Shared interlock between the repin engine and the compaction loop.
#[derive(Debug)]
pub struct RepinCoordinator {
    corpus_gate: Arc<RwLock<()>>,
    /// How many [`RollupPause`] guards are alive. The rollup is suppressed
    /// while it is above zero.
    rollup_pauses: AtomicUsize,
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
        Self {
            corpus_gate: Arc::new(RwLock::new(())),
            rollup_pauses: AtomicUsize::new(0),
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

    /// [`Self::compaction_guard`], owned so a blocking task (publication
    /// marker recovery) keeps it after cancellation of its async caller.
    pub(crate) async fn compaction_guard_owned(&self) -> OwnedRwLockReadGuard<()> {
        Arc::clone(&self.corpus_gate).read_owned().await
    }

    /// The cutover's side: exclusive against every compaction batch. WAL
    /// draining pauses only while this is held — seconds, bounded by the
    /// final catch-up increment — and the hot buffer keeps the undrained
    /// events queryable throughout.
    pub async fn cutover_guard(&self) -> RwLockWriteGuard<'_, ()> {
        self.corpus_gate.write().await
    }

    /// Suppress the daily rollup until the returned guard drops. Guards
    /// count, so the rollup resumes only when the last live one drops.
    #[must_use]
    pub fn pause_rollup(self: &Arc<Self>) -> RollupPause {
        self.rollup_pauses.fetch_add(1, Ordering::SeqCst);
        RollupPause {
            coordinator: Arc::clone(self),
        }
    }

    /// Whether the file-relocating rollup is currently suppressed. A
    /// cheap early-out for the whole pass; the binding decision for a unit
    /// about to move a file is [`Self::rollup_unit_guard`].
    #[must_use]
    pub fn rollup_paused(&self) -> bool {
        self.rollup_pauses.load(Ordering::SeqCst) > 0
    }

    /// One file-relocating rollup unit's claim on the corpus: `Some` =
    /// proceed while holding the returned read guard, `None` = stand down
    /// without touching a file.
    ///
    /// The read guard is taken first and the pause is read under it, and
    /// that order is the whole point. A pause read on its own only says
    /// the rollup was unclaimed *at that instant* — the job could claim it
    /// the next moment, finish its build and swap the shadow in while the
    /// unit was still merging, so the unit's rename would republish a
    /// pre-repin file into the new generation and delete the repinned
    /// hourlies under it. That is the silently-promoted mixed corpus the
    /// design exists to exclude (ADR-0011 amendment §2). Read under the
    /// guard, `false` is binding: no cutover can start while a read guard
    /// is held, so it means no swap can happen before this unit finishes.
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

/// One count of the rollup pause. Dropping it (job completion, failure, or
/// an engine task's panic unwinding) takes its count back, and the last
/// live guard's drop resumes the rollup. A leaked guard would suppress
/// consolidation forever.
///
/// It owns its coordinator, so it can move into the task that does the
/// work it pauses for.
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

    /// The pause is a count of live guards (ADR-0026): it holds until the
    /// last guard drops, in either drop order, when one guard drops during
    /// an unwind while another lives, and for guards moved into a task.
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

    /// A relocating rollup unit is both claimed and gated: a pause makes
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
