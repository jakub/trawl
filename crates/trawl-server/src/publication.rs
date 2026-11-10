// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Coordinates canonical Parquet publication with readers in this process.
//! External writers and exactly-once ingestion require separate guarantees.
//!
//! # Rollup markers
//!
//! The gate holds every rollup marker it knows of until the marker's file is
//! confirmed gone, and reads refuse `rollup_pending` while any remains. The
//! boot scan and a complete rescan add the markers they find. A rollup or a
//! recovery adds its marker with [`PublicationGate::mark_rollup`], under the
//! publication write guard, before it writes or reads the marker file.
//!
//! The writer owns the [`RollupRegistration`] that call returns, and keeps it
//! until it has removed the marker file. When the call added the marker, the
//! marker is in flight for as long as the registration lives: it is that
//! writer's own, and its file may not exist yet. Dropping the registration
//! ends the flight on every exit path, an early return or a panic included,
//! and leaves the marker registered. [`RollupRegistration::finish`], after
//! the writer removed the file, forgets it; so does a later evaluation under
//! a guard that finds the file gone. A rollup that fails with its marker on
//! disk therefore leaves the marker registered and refusing reads. A marker
//! the gate already held when the call came, such as one that recovery
//! retries, was established before this writer and never goes in flight.
//!
//! [`PublicationGate::read`] and every query that gets a read guard forget
//! the markers whose files are gone, then report the rest. A query that
//! takes no guard while a writer holds the lock or waits for it forgets
//! nothing and reads no file: it reports every marker that is not in flight.
//! [`PublicationGate::pending_rollup_markers`] may run without a guard, so
//! it neither forgets nor lists a marker in flight.
//!
//! # Removal syncs owed
//!
//! A marker's unlink is durable only once its day directory is fsynced, so
//! a missing marker file does not prove that the marker will stay gone
//! after a crash. A writer records the obligation with
//! [`PublicationGate::owe_removal_sync`] before it unlinks a marker, and
//! the obligation ends only when a fsync of the day directory succeeds
//! after the unlink. The gate keeps these obligations apart from the
//! markers: nothing that forgets a marker on path absence touches them,
//! and they never refuse reads, because the files the rollup left are
//! complete. While one is owed, the writers that would replace the day's
//! files consult it ([`PublicationGate::removal_sync_owed`],
//! [`PublicationGate::any_removal_sync_owed`],
//! [`PublicationGate::removal_sync_owed_days`]). The obligations are held in
//! memory and do not survive a restart.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::{
    OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock, RwLockReadGuard, RwLockWriteGuard,
};

use crate::env_dirs;
use crate::error::ServerError;
use crate::ingest::rollup_marker;

/// Why the corpus is not settled, so reads refuse with 503
/// `corpus_recovering` (ADR-0041). When both hold, `RollupPending` is the
/// one reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CorpusUnsettled {
    /// A rollup marker is unresolved, or the rollup scan failed: cold data
    /// may mix rollup generations.
    RollupPending,
    /// WAL written before the restart is not yet proven to be in the hot
    /// buffer or drained to Parquet.
    RestartBacklog,
}

impl CorpusUnsettled {
    /// Every reason, for consumers that enumerate (gauges, zero-init).
    pub const ALL: [Self; 2] = [Self::RollupPending, Self::RestartBacklog];

    /// The fixed `snake_case` literal this reason is reported as: the
    /// health check's value, the gauge's `reason` label and the failure
    /// record's cause kind.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::RollupPending => "rollup_pending",
            Self::RestartBacklog => "restart_backlog",
        }
    }
}

/// The [`CorpusUnsettled`] reasons that hold at one instant. Each holds or
/// not on its own; only [`Self::first`] applies the precedence.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UnsettledReasons {
    rollup_pending: bool,
    restart_backlog: bool,
}

impl UnsettledReasons {
    /// Whether `reason` holds.
    #[must_use]
    pub const fn contains(self, reason: CorpusUnsettled) -> bool {
        match reason {
            CorpusUnsettled::RollupPending => self.rollup_pending,
            CorpusUnsettled::RestartBacklog => self.restart_backlog,
        }
    }

    /// The reason a refusal or the health check names: `RollupPending`
    /// when both hold, `None` when the corpus is settled.
    #[must_use]
    pub const fn first(self) -> Option<CorpusUnsettled> {
        if self.rollup_pending {
            Some(CorpusUnsettled::RollupPending)
        } else if self.restart_backlog {
            Some(CorpusUnsettled::RestartBacklog)
        } else {
            None
        }
    }
}

/// Whether the WAL that survived the last restart is proven covered by the
/// hot buffer or Parquet (ADR-0041 slice 2). It only moves forward:
/// `Starting` to `Settled` or `Overhang`, and `Overhang` to `Settled`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Restart {
    /// Nothing from before the restart is unaccounted for. A gate not born
    /// [`starting`](PublicationGate::starting) starts here.
    #[default]
    Settled,
    /// Boot hydration has not finished.
    Starting,
    /// Hydration left WAL that is neither resident nor proven drained.
    Overhang,
}

/// Everything the gate knows about the corpus, under one mutex. Nothing
/// outside the gate keeps a copy.
#[derive(Debug, Default)]
struct CorpusState {
    /// Rollup markers registered and not yet confirmed gone.
    markers: HashSet<PathBuf>,
    /// The markers that a live [`RollupRegistration`] added to `markers`.
    /// Each registration removes only the entry it added, when it drops.
    /// The write guard admits one writer, and each writer registers one
    /// marker at a time, so at most one entry is present in practice. It is
    /// a set because `mark_rollup` cannot check that its caller holds the
    /// guard, and a set keeps each registration's entry its own however
    /// registrations overlap.
    in_flight: HashSet<PathBuf>,
    /// The marker scan failed, so an unknown marker may exist. Only a later
    /// complete scan clears it.
    scan_failed: bool,
    initialized: bool,
    restart: Restart,
    /// Marker unlinks whose day-directory fsync has not succeeded yet. Only
    /// [`PublicationGate::owe_removal_sync`], a [`RemovalSync`] and
    /// [`PublicationGate::retry_removal_syncs`] change it; see the
    /// [module documentation](self).
    unsynced_removals: HashMap<PathBuf, Removal>,
    /// Numbers each obligation, so a retry ends only the one it synced for.
    removal_generation: u64,
}

/// One owed removal sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Removal {
    generation: u64,
    /// Whether its writer still holds the [`RemovalSync`]. Its unlink may
    /// not have happened yet, so a retry's fsync proves nothing for it.
    with_writer: bool,
}

impl CorpusState {
    /// The one evaluator behind every answer about the corpus: forget
    /// markers whose files are confirmed gone, then report what holds.
    ///
    /// The caller holds a publication guard. A writer registers its rollup
    /// marker before it writes the marker file, so a prune without a guard
    /// could forget a marker that a failed rollup then leaves on disk.
    fn evaluate(&mut self) -> UnsettledReasons {
        self.markers.retain(|path| !is_missing(path));
        UnsettledReasons {
            rollup_pending: self.scan_failed || !self.markers.is_empty(),
            restart_backlog: self.restart != Restart::Settled,
        }
    }

    /// What holds while a writer holds or waits for the publication lock.
    /// No marker is pruned, because the writer's own may not have its file
    /// yet. Every registered marker that is not in flight counts, whether
    /// or not its file still exists, as do a failed marker scan and the
    /// restart state. A marker whose file is gone therefore reads as
    /// pending until the next guarded evaluation forgets it: the stale
    /// answer refuses, never admits.
    fn beside_a_writer(&self) -> UnsettledReasons {
        UnsettledReasons {
            rollup_pending: self.scan_failed
                || self
                    .markers
                    .iter()
                    .any(|path| !self.in_flight.contains(path)),
            restart_backlog: self.restart != Restart::Settled,
        }
    }
}

impl CorpusState {
    /// End the obligation of `marker` if it is still the one numbered
    /// `generation`.
    fn end_removal(&mut self, marker: &Path, generation: u64) {
        if self
            .unsynced_removals
            .get(marker)
            .is_some_and(|removal| removal.generation == generation)
        {
            self.unsynced_removals.remove(marker);
        }
    }
}

/// [`PublicationGate::finish_hydration`] on a gate that is not starting:
/// hydration already finished, or the gate was not born starting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("hydration can finish only once, on a gate born starting")]
pub struct NotStarting;

/// Readers hold a guard through source selection, hot snapshot, and execution.
/// Writers hold a guard through publication and hot drain or source retirement.
///
/// The gate also holds the corpus state: pending rollups and, on an ingest
/// node, whether the WAL from before the restart is covered. While either
/// is unsettled, [`read`](Self::read) refuses (ADR-0026, ADR-0041).
#[derive(Debug, Default)]
pub struct PublicationGate {
    lock: Arc<RwLock<()>>,
    corpus: Mutex<CorpusState>,
    #[cfg(any(test, feature = "test-support"))]
    pause: Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
    #[cfg(test)]
    panic_next_publication: std::sync::atomic::AtomicBool,
}

impl PublicationGate {
    /// A settled gate: a query-only node's, and any gate outside boot.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An ingest node's gate at boot. Reads refuse `restart_backlog` until
    /// [`finish_hydration`](Self::finish_hydration) says the surviving WAL
    /// is resident (ADR-0041 slice 2).
    #[must_use]
    pub fn starting() -> Self {
        Self {
            corpus: Mutex::new(CorpusState {
                restart: Restart::Starting,
                ..CorpusState::default()
            }),
            ..Self::default()
        }
    }

    /// Register every rollup marker [`scan_markers`] finds, in any directory
    /// child of an env, before admitting readers.
    /// Literal directory traversal keeps glob metacharacters in root names
    /// literal. A failed scan refuses reads until a later complete scan
    /// clears it ([`rescan_if_failed`](Self::rescan_if_failed)). Only
    /// compaction rescans, so on a query-only node that is the next restart.
    /// Repeated initialization preserves markers already registered by writers.
    pub fn initialize(&self, root: &Path) {
        let mut corpus = self.corpus.lock();
        if corpus.initialized {
            return;
        }
        corpus.initialized = true;
        if let Err(error) = scan_markers(root, &mut corpus.markers) {
            // The failure episode is counted here, once. Later read
            // refusals and failed rescans are its consequences, not new
            // failures.
            crate::metrics::CompactionOperation::PendingRollupScan.record_failure();
            tracing::error!(event_type = "publication_scan_failed", %error,
                "cannot establish rollup publication state; corpus reads refused until a later scan completes");
            corpus.scan_failed = true;
        }
    }

    /// Whether the marker scan failed and no complete rescan has cleared
    /// it, so a marker the gate does not know may exist.
    pub(crate) fn marker_scan_failed(&self) -> bool {
        self.corpus.lock().scan_failed
    }

    /// Retry a failed marker scan. Only a complete scan clears the failure,
    /// and its markers join those writers registered. A failed retry
    /// changes nothing and records nothing: the failure was counted when
    /// the first scan failed. A gate whose scan did not fail returns at
    /// once. Blocking filesystem I/O.
    pub fn rescan_if_failed(&self, root: &Path) {
        if !self.corpus.lock().scan_failed {
            return;
        }
        let mut found = HashSet::new();
        if scan_markers(root, &mut found).is_err() {
            return;
        }
        {
            let mut corpus = self.corpus.lock();
            corpus.markers.extend(found);
            corpus.scan_failed = false;
        }
        tracing::info!(
            event_type = "publication_scan_recovered",
            "rollup publication state established; the failed marker scan is cleared"
        );
    }

    /// Admit a corpus reader: take the read guard, then refuse with
    /// [`ServerError::CorpusRecovering`] while the corpus is unsettled. A
    /// pending rollup (including incomplete recovery at boot) is named
    /// before a restart backlog.
    pub async fn read(&self) -> Result<OwnedRwLockReadGuard<()>, ServerError> {
        let guard = Arc::clone(&self.lock).read_owned().await;
        match self.corpus.lock().evaluate().first() {
            Some(reason) => Err(ServerError::CorpusRecovering(reason)),
            None => Ok(guard),
        }
    }

    /// Admit compaction's WAL phase: take the read guard, then refuse only
    /// while a rollup is pending. The restart backlog is compaction's to
    /// drain, so it must not stop compaction.
    pub async fn read_rollups_only(&self) -> Result<OwnedRwLockReadGuard<()>, CorpusUnsettled> {
        let guard = Arc::clone(&self.lock).read_owned().await;
        if self
            .corpus
            .lock()
            .evaluate()
            .contains(CorpusUnsettled::RollupPending)
        {
            return Err(CorpusUnsettled::RollupPending);
        }
        Ok(guard)
    }

    /// Why reads would refuse now, by precedence, or `None` when settled.
    /// Never waits behind a publication; see
    /// [`unsettled_reasons`](Self::unsettled_reasons).
    #[must_use]
    pub fn unsettled(&self) -> Option<CorpusUnsettled> {
        self.unsettled_reasons().first()
    }

    /// Every reason that holds now, each on its own. Never waits: when the
    /// publication lock is free, it evaluates under a read guard taken
    /// without waiting. While a writer holds the lock or waits for it, no
    /// registered marker is pruned, and every marker is reported except
    /// one in flight (see the [module documentation](self)); a failed
    /// marker scan and the restart state are reported too. A normal rollup
    /// therefore never moves the answer. A marker that a failed rollup
    /// leaves is reported from the moment its registration drops, beside
    /// the next writer as well, before any guarded evaluation.
    /// [`read`](Self::read) always waits for the writer and evaluates
    /// everything.
    #[must_use]
    pub fn unsettled_reasons(&self) -> UnsettledReasons {
        match self.lock.try_read() {
            Ok(_guard) => self.corpus.lock().evaluate(),
            Err(_) => self.corpus.lock().beside_a_writer(),
        }
    }

    /// Whether the WAL from before the restart is not yet proven covered:
    /// hydration has not finished, or it left overhang. Takes no guard.
    #[must_use]
    pub fn overhang(&self) -> bool {
        self.corpus.lock().restart != Restart::Settled
    }

    /// Whether hydration finished and left overhang, which only compaction's
    /// coverage proof clears. Unlike [`overhang`](Self::overhang), false
    /// while hydration has not finished: there is nothing to prove yet.
    #[must_use]
    pub fn awaits_coverage_proof(&self) -> bool {
        self.corpus.lock().restart == Restart::Overhang
    }

    /// End boot hydration: settled when every surviving WAL file became
    /// resident, overhang otherwise.
    ///
    /// # Errors
    ///
    /// [`NotStarting`] when the gate was not born
    /// [`starting`](Self::starting) or hydration already finished; the gate
    /// is unchanged.
    pub fn finish_hydration(&self, overhang: bool) -> Result<(), NotStarting> {
        let mut corpus = self.corpus.lock();
        if corpus.restart != Restart::Starting {
            return Err(NotStarting);
        }
        corpus.restart = if overhang {
            Restart::Overhang
        } else {
            Restart::Settled
        };
        Ok(())
    }

    /// Clear overhang once a coverage proof holds. `_held` is the publication
    /// write guard the proof ran under, taken from this gate: requiring it
    /// makes the caller hold the guard across the proof and the move. Moves
    /// only `Overhang` to `Settled`, and logs `corpus_settled` once, on that
    /// move.
    pub fn settle_overhang(&self, _held: &RwLockWriteGuard<'_, ()>) {
        // The guard type cannot name its lock. A read that is admitted
        // while `_held` is alive proves it is some other gate's guard.
        debug_assert!(
            self.lock.try_read().is_err(),
            "settle_overhang needs this gate's write guard"
        );
        let settled = {
            let mut corpus = self.corpus.lock();
            let overhang = corpus.restart == Restart::Overhang;
            if overhang {
                corpus.restart = Restart::Settled;
            }
            overhang
        };
        if settled {
            tracing::info!(
                event_type = "corpus_settled",
                "the WAL from before the restart is covered; corpus reads are no longer refused for it"
            );
        }
    }

    pub async fn write(&self) -> OwnedRwLockWriteGuard<()> {
        Arc::clone(&self.lock).write_owned().await
    }

    /// Wait before scheduling HTTP WAL work so blocked requests do not
    /// occupy the blocking threads that active query readers need.
    pub async fn ingest(&self) -> OwnedRwLockReadGuard<()> {
        Arc::clone(&self.lock).read_owned().await
    }

    /// Keep WAL publication and hot insertion ahead of compaction's drain.
    /// Ingest may continue during pending rollup recovery. Call only from a
    /// blocking thread and never acquire this guard recursively.
    pub fn blocking_ingest(&self) -> RwLockReadGuard<'_, ()> {
        self.lock.blocking_read()
    }

    /// Call only from a blocking thread, never from an async runtime task.
    pub fn blocking_write(&self) -> RwLockWriteGuard<'_, ()> {
        self.lock.blocking_write()
    }

    /// Register `marker` before writing or recovering it. The caller holds
    /// the publication write guard, and keeps the returned registration
    /// until it has removed the marker file, then
    /// [`finish`](RollupRegistration::finish)es it. When the gate did not
    /// hold `marker` yet, it is in flight until the registration drops; see
    /// the [module documentation](self).
    pub fn mark_rollup(&self, marker: &Path) -> RollupRegistration<'_> {
        let path = marker.to_path_buf();
        let in_flight = {
            let mut corpus = self.corpus.lock();
            let added = corpus.markers.insert(path.clone());
            if added {
                corpus.in_flight.insert(path.clone());
            }
            added
        };
        RollupRegistration {
            gate: self,
            path,
            in_flight,
        }
    }

    /// Record that the unlink of `marker` owes a fsync of its day
    /// directory, before the unlink is attempted. The caller holds the
    /// publication write guard and keeps the returned [`RemovalSync`]
    /// until it has unlinked the marker, then
    /// [`settle`](RemovalSync::settle)s it. The obligation holds from this
    /// call until a day-directory fsync after the unlink succeeds, whether
    /// or not the marker file still exists.
    pub fn owe_removal_sync(&self, marker: &Path) -> RemovalSync<'_> {
        let mut corpus = self.corpus.lock();
        corpus.removal_generation += 1;
        let generation = corpus.removal_generation;
        corpus.unsynced_removals.insert(
            marker.to_path_buf(),
            Removal {
                generation,
                with_writer: true,
            },
        );
        RemovalSync {
            gate: self,
            marker: marker.to_path_buf(),
            generation,
        }
    }

    /// Whether the unlink of `marker` still owes its day-directory fsync.
    #[must_use]
    pub fn removal_sync_owed(&self, marker: &Path) -> bool {
        self.corpus.lock().unsynced_removals.contains_key(marker)
    }

    /// Whether any marker unlink still owes its day-directory fsync.
    #[must_use]
    pub fn any_removal_sync_owed(&self) -> bool {
        !self.corpus.lock().unsynced_removals.is_empty()
    }

    /// The day directories of every marker whose unlink still owes its
    /// fsync.
    #[must_use]
    pub fn removal_sync_owed_days(&self) -> HashSet<PathBuf> {
        self.corpus
            .lock()
            .unsynced_removals
            .keys()
            .filter_map(|marker| marker.parent().map(Path::to_path_buf))
            .collect()
    }

    /// Retry the day-directory fsync of every owed removal whose writer is
    /// done, once per day directory, with `sync`. Only a successful sync
    /// ends an obligation; any error keeps it, `NotFound` included, and
    /// logs `rollup_removal_sync_owed` naming the env, service and day.
    /// Returns how many obligations are still owed. Blocking I/O, run
    /// with no lock of the gate's held.
    pub fn retry_removal_syncs(&self, mut sync: impl FnMut(&Path) -> std::io::Result<()>) -> usize {
        let mut by_day: HashMap<PathBuf, Vec<(PathBuf, u64)>> = HashMap::new();
        for (marker, removal) in &self.corpus.lock().unsynced_removals {
            if removal.with_writer {
                continue;
            }
            if let Some(day) = marker.parent() {
                by_day
                    .entry(day.to_path_buf())
                    .or_default()
                    .push((marker.clone(), removal.generation));
            }
        }
        for (day, markers) in by_day {
            match sync(&day) {
                Ok(()) => {
                    let mut corpus = self.corpus.lock();
                    for (marker, generation) in markers {
                        corpus.end_removal(&marker, generation);
                    }
                }
                Err(error) => {
                    for (marker, _) in &markers {
                        log_removal_sync_owed(marker, &error);
                    }
                }
            }
        }
        self.corpus.lock().unsynced_removals.len()
    }

    /// Recovery must finish these days before WAL compaction changes any
    /// hourly input named by a marker.
    ///
    /// Forgets the markers whose files are confirmed gone, except one in
    /// flight: its writer may not have written its file yet, and a caller
    /// may hold no guard. Lists every other marker. One in flight is a live
    /// writer's own, not a marker to recover; if that rollup fails with the
    /// marker on disk, the marker is listed once its registration drops.
    pub fn pending_rollup_markers(&self) -> Vec<PathBuf> {
        let mut corpus = self.corpus.lock();
        let CorpusState {
            markers, in_flight, ..
        } = &mut *corpus;
        markers.retain(|path| in_flight.contains(path) || !is_missing(path));
        markers
            .iter()
            .filter(|path| !in_flight.contains(*path))
            .cloned()
            .collect()
    }

    /// Pause the next publication while its caller still holds the write guard.
    #[cfg(any(test, feature = "test-support"))]
    pub fn pause_next_publication_for_test(
        &self,
    ) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        *self.pause.lock() = Some((entered_tx, release_rx));
        (entered_rx, release_tx)
    }

    /// Call on a blocking thread while holding the publication write guard.
    /// Dropping the release sender also releases the pause.
    #[cfg(any(test, feature = "test-support"))]
    pub fn hold_after_publish_for_test(&self) {
        #[cfg(test)]
        assert!(
            !self
                .panic_next_publication
                .swap(false, std::sync::atomic::Ordering::Relaxed),
            "injected publication task panic"
        );
        let pause = self.pause.lock().take();
        if let Some((entered, release)) = pause {
            let _ = entered.send(());
            let _ = release.recv();
        }
    }

    /// Fail one blocking caller at its existing publication checkpoint.
    /// The next hold site reached consumes this flag. Tests pin the intended
    /// site by asserting the published output and retained source/marker state.
    #[cfg(test)]
    pub(crate) fn panic_next_publication_for_test(&self) {
        self.panic_next_publication
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// A writer's claim on one rollup marker, from
/// [`PublicationGate::mark_rollup`] until [`finish`](Self::finish) or drop.
/// It holds no lock: it takes the gate's state mutex only when it is made,
/// finished and dropped.
#[must_use = "a rollup marker stays in flight only while its registration lives"]
#[derive(Debug)]
pub struct RollupRegistration<'a> {
    gate: &'a PublicationGate,
    path: PathBuf,
    /// Whether this registration added the marker, and so put it in flight.
    in_flight: bool,
}

impl RollupRegistration<'_> {
    /// End the rollup or recovery after removing the marker file. The gate
    /// forgets the marker only when its file is confirmed gone: a surviving
    /// file, or one whose metadata cannot be read, still refuses readers.
    /// The caller still holds the publication write guard. Never changes
    /// files.
    pub fn finish(self) {
        if is_missing(&self.path) {
            // Forgotten before the drop ends its flight, so no query reads
            // it as a marker that is registered and not in flight.
            self.gate.corpus.lock().markers.remove(&self.path);
        }
    }
}

impl Drop for RollupRegistration<'_> {
    fn drop(&mut self) {
        if self.in_flight {
            self.gate.corpus.lock().in_flight.remove(&self.path);
        }
    }
}

/// A writer's hold on one owed removal sync, from
/// [`PublicationGate::owe_removal_sync`] until [`settle`](Self::settle) or
/// drop. While it lives, a retry leaves the obligation to the writer.
/// Dropping it unsettled, an early return or a panic included, leaves the
/// obligation owed for the next retry.
#[must_use = "a removal sync is settled only by `settle`"]
#[derive(Debug)]
pub struct RemovalSync<'a> {
    gate: &'a PublicationGate,
    marker: PathBuf,
    generation: u64,
}

impl RemovalSync<'_> {
    /// After the writer's unlink of the marker, fsync its day directory
    /// with `sync`. Success ends the obligation. An error, `NotFound`
    /// included, keeps it owed for the next retry, logs
    /// `rollup_removal_sync_owed`, and is returned.
    pub fn settle(self, sync: impl FnOnce(&Path) -> std::io::Result<()>) -> std::io::Result<()> {
        let day = self.marker.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "a rollup marker path has no day directory",
            )
        });
        match day.and_then(sync) {
            Ok(()) => {
                self.gate
                    .corpus
                    .lock()
                    .end_removal(&self.marker, self.generation);
                Ok(())
            }
            Err(error) => {
                log_removal_sync_owed(&self.marker, &error);
                Err(error)
            }
        }
    }
}

impl Drop for RemovalSync<'_> {
    fn drop(&mut self) {
        if let Some(removal) = self
            .gate
            .corpus
            .lock()
            .unsynced_removals
            .get_mut(&self.marker)
            .filter(|removal| removal.generation == self.generation)
        {
            removal.with_writer = false;
        }
    }
}

/// Log that `marker`'s unlink still owes its day fsync. The env, service
/// and day are logged only once they pass validation, and no path is.
fn log_removal_sync_owed(marker: &Path, error: &std::io::Error) {
    const INVALID: &str = "<invalid>";
    let day_dir = marker.parent();
    let service = marker
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(rollup_marker::marker_service)
        .filter(|service| trawl_config::is_valid_service_name(service))
        .unwrap_or(INVALID);
    let day = day_dir
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .filter(|day| env_dirs::parse_date(day).is_some())
        .unwrap_or(INVALID);
    let env = day_dir
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .filter(|env| trawl_config::is_valid_env_name(env))
        .unwrap_or(INVALID);
    tracing::error!(
        event_type = "rollup_removal_sync_owed",
        env,
        service,
        day,
        error_kind = ?error.kind(),
        "a rollup marker was unlinked but its day directory fsync failed; \
         rollup, repin and conformance wait for that day until a retry succeeds"
    );
}

fn is_missing(path: &Path) -> bool {
    matches!(std::fs::symlink_metadata(path), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
}

fn directory_exists(path: &Path) -> std::io::Result<bool> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_dir()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Collect every `root/{env}/{child}/.rollup-*` marker into `markers`,
/// whatever the child directory's name: a marker under a child that is not
/// a canonical date still refuses reads, and recovery keeps it with every
/// file (ADR-0041). A missing root holds none; any other listing error is
/// an `Err`, because an unknown marker may exist.
pub(crate) fn scan_markers(root: &Path, markers: &mut HashSet<PathBuf>) -> std::io::Result<()> {
    let envs = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        // A missing root, a dangling root link included, holds no marker, as
        // ADR-0026 rules; boot's epoch gate already refuses a data root it
        // cannot inspect.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for env in envs {
        let env = env?;
        let name = env.file_name();
        let Some(name) = name.to_str() else { continue };
        if !trawl_config::is_valid_env_name(name)
            || trawl_config::RESERVED_ENV_NAMES.contains(&name)
        {
            continue;
        }
        if !directory_exists(&env.path())? {
            continue;
        }
        for child in std::fs::read_dir(env.path())? {
            let child = child?;
            if !directory_exists(&child.path())? {
                continue;
            }
            for entry in std::fs::read_dir(child.path())? {
                let entry = entry?;
                if rollup_marker::marker_service(&entry.file_name().to_string_lossy()).is_some() {
                    markers.insert(entry.path());
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn writer_excludes_readers() {
        let gate = PublicationGate::new();
        let writer = gate.write().await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), gate.read())
                .await
                .is_err()
        );
        drop(writer);
        assert!(gate.read().await.is_ok());
    }

    #[tokio::test]
    async fn pending_marker_refuses_reads_until_removed() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join(".rollup-svc");
        std::fs::write(&marker, "").unwrap();
        let gate = PublicationGate::new();
        {
            let _writer = gate.write().await;
            gate.mark_rollup(&marker).finish();
        }
        assert!(matches!(
            gate.read().await,
            Err(ServerError::CorpusRecovering(
                CorpusUnsettled::RollupPending
            ))
        ));
        std::fs::remove_file(marker).unwrap();
        assert!(gate.read().await.is_ok());
    }

    /// A day directory holding an unlinked rollup marker whose unlink owes
    /// its fsync.
    fn owed_marker(dir: &Path) -> PathBuf {
        let day = dir.join("prod/2026-11-06");
        std::fs::create_dir_all(&day).unwrap();
        day.join(".rollup-api")
    }

    fn refused() -> std::io::Error {
        std::io::Error::from(std::io::ErrorKind::StorageFull)
    }

    #[tokio::test]
    async fn an_owed_removal_sync_outlives_the_marker_file_and_never_refuses_reads() {
        let dir = tempfile::tempdir().unwrap();
        let marker = owed_marker(dir.path());
        std::fs::write(&marker, "").unwrap();
        let gate = PublicationGate::new();
        {
            let _writer = gate.write().await;
            let registration = gate.mark_rollup(&marker);
            let removal = gate.owe_removal_sync(&marker);
            std::fs::remove_file(&marker).unwrap();
            assert!(removal.settle(|_| Err(refused())).is_err());
            // Path absence ends the marker, and only the marker.
            registration.finish();
        }
        assert!(!marker.exists());
        assert!(gate.read().await.is_ok(), "reads answer while owed");
        assert_eq!(gate.unsettled(), None);
        assert!(gate.pending_rollup_markers().is_empty());
        assert!(gate.removal_sync_owed(&marker));
        assert!(gate.any_removal_sync_owed());
        assert_eq!(
            gate.removal_sync_owed_days(),
            HashSet::from([marker.parent().unwrap().to_path_buf()])
        );
        // Every evaluator runs again, and still the obligation holds.
        let _ = gate.read_rollups_only().await;
        let _ = gate.unsettled_reasons();
        assert!(gate.removal_sync_owed(&marker));
    }

    #[test]
    fn a_failed_retry_keeps_the_obligation_and_a_successful_one_ends_it() {
        let dir = tempfile::tempdir().unwrap();
        let marker = owed_marker(dir.path());
        let gate = PublicationGate::new();
        drop(gate.owe_removal_sync(&marker));

        let mut synced = Vec::new();
        assert_eq!(
            gate.retry_removal_syncs(|day| {
                synced.push(day.to_path_buf());
                Err(refused())
            }),
            1
        );
        assert!(gate.removal_sync_owed(&marker));
        // A day directory that is gone is a failed sync, not a success.
        assert_eq!(
            gate.retry_removal_syncs(|_| Err(std::io::ErrorKind::NotFound.into())),
            1
        );
        assert!(gate.removal_sync_owed(&marker));
        assert_eq!(
            gate.retry_removal_syncs(|day| {
                synced.push(day.to_path_buf());
                crate::epoch::fsync_dir(day)
            }),
            0
        );
        assert!(!gate.removal_sync_owed(&marker));
        assert!(!gate.any_removal_sync_owed());
        assert!(gate.removal_sync_owed_days().is_empty());
        assert_eq!(synced, vec![marker.parent().unwrap().to_path_buf(); 2]);
    }

    #[test]
    fn a_settled_removal_sync_ends_the_obligation() {
        let dir = tempfile::tempdir().unwrap();
        let marker = owed_marker(dir.path());
        let gate = PublicationGate::new();
        let removal = gate.owe_removal_sync(&marker);
        assert!(gate.removal_sync_owed(&marker), "owed before the unlink");
        removal.settle(crate::epoch::fsync_dir).unwrap();
        assert!(!gate.any_removal_sync_owed());
    }

    #[test]
    fn a_retry_leaves_an_obligation_to_its_writer() {
        let dir = tempfile::tempdir().unwrap();
        let marker = owed_marker(dir.path());
        let gate = PublicationGate::new();
        let removal = gate.owe_removal_sync(&marker);
        // The writer has not unlinked yet, so a sync now proves nothing.
        assert_eq!(gate.retry_removal_syncs(|_| panic!("not retried")), 1);
        assert!(removal.settle(|_| Err(refused())).is_err());
        // Settled unsuccessfully, the writer is done: the next retry runs.
        assert_eq!(gate.retry_removal_syncs(crate::epoch::fsync_dir), 0);
    }

    #[test]
    fn a_retry_never_ends_a_newer_obligation_for_the_same_marker() {
        let dir = tempfile::tempdir().unwrap();
        let marker = owed_marker(dir.path());
        let gate = PublicationGate::new();
        drop(gate.owe_removal_sync(&marker));
        let mut newer = None;
        let still_owed = gate.retry_removal_syncs(|_| {
            // Another writer owes the same marker again while the retry's
            // sync runs.
            newer = Some(gate.owe_removal_sync(&marker));
            Ok(())
        });
        assert_eq!(still_owed, 1);
        drop(newer);
        assert!(gate.removal_sync_owed(&marker));
    }

    #[tokio::test]
    async fn bootstrap_finds_markers_under_literal_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("data[1]*");
        let day = root.join("prod/2026-01-01");
        std::fs::create_dir_all(&day).unwrap();
        let marker = day.join(".rollup-svc");
        std::fs::write(&marker, "").unwrap();
        let gate = PublicationGate::new();
        gate.initialize(&root);
        gate.initialize(&dir.path().join("absent"));
        assert!(gate.read().await.is_err());
        std::fs::remove_file(marker).unwrap();
        assert!(gate.read().await.is_ok());
    }

    /// A failed scan refuses reads until a later complete scan clears it
    /// (ADR-0041 slice 2). Reads and failed rescans in between are the
    /// same failure episode, counted once.
    #[tokio::test]
    async fn scan_failure_stays_closed() {
        use crate::metrics::test_support::sample;
        let recorder = crate::metrics::prometheus_builder().build_recorder();
        let handle = recorder.handle();
        // initialize, rescan and read perform their checks on this thread;
        // none dispatches a worker whose counters this recorder could miss.
        let _recorder = metrics::set_default_local_recorder(&recorder);
        crate::metrics::init_operational_alert_metrics();
        let dir = tempfile::tempdir().unwrap();
        let empty = PublicationGate::new();
        empty.initialize(&dir.path().join("missing"));
        assert!(empty.read().await.is_ok());
        let series = "trawl_compaction_operation_failures_total{operation=\"pending_rollup_scan\"}";
        assert_eq!(sample(&handle, series), 0);
        let root = dir.path().join("file");
        std::fs::write(&root, "").unwrap();
        let gate = PublicationGate::new();
        gate.initialize(&root);
        assert_eq!(sample(&handle, series), 1);
        gate.initialize(dir.path());
        assert!(gate.read().await.is_err());
        assert!(gate.read().await.is_err());
        gate.rescan_if_failed(&root);
        assert!(
            gate.read().await.is_err(),
            "a rescan that fails again changes nothing"
        );
        assert_eq!(
            sample(&handle, series),
            1,
            "a latched refusal or a failed rescan is not a new failure"
        );
        std::fs::remove_file(&root).unwrap();
        std::fs::create_dir(&root).unwrap();
        gate.rescan_if_failed(&root);
        assert!(gate.read().await.is_ok(), "a complete rescan clears it");
        assert_eq!(sample(&handle, series), 1);
        assert_eq!(
            sample(
                &handle,
                "trawl_compaction_operation_failures_total{operation=\"pending_rollup_recovery\"}"
            ),
            0
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dangling_entries_do_not_poison_the_marker_scan() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("data");
        let day = root.join("prod/2026-01-01");
        std::fs::create_dir_all(&day).unwrap();
        std::os::unix::fs::symlink(dir.path().join("absent"), root.join("offline")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("absent"), root.join("prod/2026-01-02"))
            .unwrap();
        let marker = day.join(".rollup-svc");
        std::fs::write(&marker, "").unwrap();
        let gate = PublicationGate::new();
        gate.initialize(&root);
        assert!(
            gate.read().await.is_err(),
            "a real pending marker still refuses queries"
        );
        std::fs::remove_file(marker).unwrap();
        assert!(
            gate.read().await.is_ok(),
            "confirmed missing paths do not latch a scan failure"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unresolvable_existing_env_keeps_marker_state_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let env = dir.path().join("prod");
        std::os::unix::fs::symlink(&env, &env).unwrap();
        let gate = PublicationGate::new();
        gate.initialize(dir.path());
        std::fs::remove_file(&env).unwrap();
        assert!(
            gate.read().await.is_err(),
            "errors other than confirmed absence remain closed until a complete rescan"
        );
    }

    /// `(level, event_type)` of one recorded event.
    type Recorded = (tracing::Level, Option<String>);

    /// Every event recorded while installed.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<Recorded>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            struct EventType(Option<String>);
            impl tracing::field::Visit for EventType {
                fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                    if field.name() == "event_type" {
                        self.0 = Some(value.to_owned());
                    }
                }
                fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
            }
            let mut event_type = EventType(None);
            event.record(&mut event_type);
            self.0
                .lock()
                .push((*event.metadata().level(), event_type.0));
        }
    }

    impl Capture {
        fn count(&self, event_type: &str) -> usize {
            self.0
                .lock()
                .iter()
                .filter(|(_, recorded)| recorded.as_deref() == Some(event_type))
                .count()
        }
    }

    /// Run `f` with every event it logs on this thread captured.
    fn capturing<T>(f: impl FnOnce(&Capture) -> T) -> T {
        use tracing_subscriber::layer::SubscriberExt as _;
        let capture = Capture::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        tracing::subscriber::with_default(subscriber, || f(&capture))
    }

    fn assert_settled(gate: &PublicationGate) {
        assert_eq!(gate.unsettled(), None);
        assert_eq!(gate.unsettled_reasons(), UnsettledReasons::default());
        assert!(!gate.overhang());
    }

    fn assert_restart_backlog(gate: &PublicationGate) {
        assert_eq!(gate.unsettled(), Some(CorpusUnsettled::RestartBacklog));
        assert!(gate.overhang());
        let reasons = gate.unsettled_reasons();
        assert!(reasons.contains(CorpusUnsettled::RestartBacklog));
        assert!(!reasons.contains(CorpusUnsettled::RollupPending));
    }

    #[tokio::test]
    async fn new_and_default_gates_are_born_settled() {
        for gate in [PublicationGate::new(), PublicationGate::default()] {
            assert_settled(&gate);
            assert!(gate.read().await.is_ok());
            assert!(gate.read_rollups_only().await.is_ok());
        }
    }

    #[tokio::test]
    async fn a_starting_gate_refuses_restart_backlog_until_hydration_finishes() {
        let gate = PublicationGate::starting();
        assert_restart_backlog(&gate);
        assert!(matches!(
            gate.read().await,
            Err(ServerError::CorpusRecovering(
                CorpusUnsettled::RestartBacklog
            ))
        ));

        capturing(|capture| {
            assert_eq!(gate.finish_hydration(false), Ok(()));
            assert_eq!(
                capture.count("corpus_settled"),
                0,
                "a boot with nothing left over settles without a coverage proof"
            );
        });
        assert_settled(&gate);
        assert!(gate.read().await.is_ok());
    }

    #[tokio::test]
    async fn overhang_refuses_until_settled_and_logs_the_settling_once() {
        let gate = PublicationGate::starting();
        assert_eq!(gate.finish_hydration(true), Ok(()));
        assert_restart_backlog(&gate);
        assert!(matches!(
            gate.read().await,
            Err(ServerError::CorpusRecovering(
                CorpusUnsettled::RestartBacklog
            ))
        ));

        let held = gate.lock.write().await;
        capturing(|capture| {
            gate.settle_overhang(&held);
            assert_settled(&gate);
            gate.settle_overhang(&held);
            assert_eq!(
                capture.count("corpus_settled"),
                1,
                "logged on the move from overhang, never again"
            );
            assert!(
                capture
                    .0
                    .lock()
                    .iter()
                    .all(|(level, _)| *level == tracing::Level::INFO)
            );
        });
        drop(held);
        assert!(gate.read().await.is_ok());
    }

    #[test]
    fn finish_hydration_is_valid_only_once_from_starting() {
        let settled = PublicationGate::new();
        assert_eq!(settled.finish_hydration(true), Err(NotStarting));
        assert_settled(&settled);

        for overhang in [false, true] {
            let gate = PublicationGate::starting();
            gate.finish_hydration(overhang).unwrap();
            for again in [false, true] {
                assert_eq!(gate.finish_hydration(again), Err(NotStarting));
                assert_eq!(
                    gate.overhang(),
                    overhang,
                    "the refused call changed nothing"
                );
            }
        }
    }

    #[test]
    fn settle_overhang_moves_only_overhang() {
        capturing(|capture| {
            let starting = PublicationGate::starting();
            starting.settle_overhang(&starting.blocking_write());
            assert_restart_backlog(&starting);

            let settled = PublicationGate::new();
            settled.settle_overhang(&settled.blocking_write());
            assert_settled(&settled);

            assert_eq!(capture.count("corpus_settled"), 0);
        });
    }

    /// Only overhang left by a finished hydration awaits a coverage proof: a
    /// starting gate is overhang too, but has nothing to prove yet.
    #[test]
    fn only_overhang_awaits_a_coverage_proof() {
        assert!(!PublicationGate::new().awaits_coverage_proof());
        let starting = PublicationGate::starting();
        assert!(starting.overhang());
        assert!(!starting.awaits_coverage_proof());
        for overhang in [false, true] {
            let gate = PublicationGate::starting();
            gate.finish_hydration(overhang).unwrap();
            assert_eq!(gate.awaits_coverage_proof(), overhang);
            gate.settle_overhang(&gate.blocking_write());
            assert!(!gate.awaits_coverage_proof());
        }
    }

    #[tokio::test]
    async fn rollup_pending_wins_over_restart_backlog() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join(".rollup-svc");
        std::fs::write(&marker, "").unwrap();
        let gate = PublicationGate::starting();
        gate.finish_hydration(true).unwrap();
        {
            let _writer = gate.write().await;
            drop(gate.mark_rollup(&marker));
        }

        let reasons = gate.unsettled_reasons();
        assert!(reasons.contains(CorpusUnsettled::RollupPending));
        assert!(
            reasons.contains(CorpusUnsettled::RestartBacklog),
            "each reason holds on its own"
        );
        assert_eq!(gate.unsettled(), Some(CorpusUnsettled::RollupPending));
        assert!(gate.overhang());
        assert!(matches!(
            gate.read().await,
            Err(ServerError::CorpusRecovering(
                CorpusUnsettled::RollupPending
            ))
        ));

        std::fs::remove_file(&marker).unwrap();
        assert!(matches!(
            gate.read().await,
            Err(ServerError::CorpusRecovering(
                CorpusUnsettled::RestartBacklog
            ))
        ));
        gate.settle_overhang(&gate.lock.write().await);
        assert!(gate.read().await.is_ok());
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "settle_overhang needs this gate's write guard")]
    fn settle_overhang_refuses_another_gates_guard() {
        let gate = PublicationGate::starting();
        gate.finish_hydration(true).unwrap();
        let other = PublicationGate::new();
        gate.settle_overhang(&other.blocking_write());
    }

    #[tokio::test]
    async fn a_failed_scan_is_rollup_pending() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("file");
        std::fs::write(&root, "").unwrap();
        let gate = PublicationGate::starting();
        gate.initialize(&root);
        assert_eq!(gate.unsettled(), Some(CorpusUnsettled::RollupPending));
        assert!(matches!(
            gate.read_rollups_only().await,
            Err(CorpusUnsettled::RollupPending)
        ));
    }

    /// A rollup registers its marker before it writes the marker file. A
    /// query that takes no guard, inside that window, neither forgets the
    /// marker nor reports it. If the rollup then fails with its marker on
    /// disk, the marker is reported from the moment its registration
    /// drops, while the writer still holds the lock, and reads refuse
    /// `rollup_pending` (ADR-0026, ADR-0041).
    #[tokio::test]
    async fn a_lock_free_query_inside_a_rollup_keeps_its_marker() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join(".rollup-svc");
        let gate = PublicationGate::new();
        {
            let _writer = gate.write().await;
            let registration = gate.mark_rollup(&marker);
            assert_eq!(
                gate.unsettled(),
                None,
                "an in-flight marker is not reported"
            );
            assert_eq!(gate.unsettled_reasons(), UnsettledReasons::default());
            assert!(!gate.overhang());
            assert!(
                gate.corpus.lock().markers.contains(&marker),
                "nor forgotten while its file does not exist yet"
            );
            std::fs::write(&marker, "hourly inputs").unwrap();
            assert_eq!(gate.unsettled(), None);
            // The rollup fails before retiring its marker.
            drop(registration);
            assert_eq!(
                gate.unsettled(),
                Some(CorpusUnsettled::RollupPending),
                "a failed rollup's marker, beside its own writer"
            );
        }
        assert!(matches!(
            gate.read().await,
            Err(ServerError::CorpusRecovering(
                CorpusUnsettled::RollupPending
            ))
        ));
        assert_eq!(gate.unsettled(), Some(CorpusUnsettled::RollupPending));
        std::fs::remove_file(&marker).unwrap();
        assert!(gate.read().await.is_ok());
    }

    /// A compaction pass lists the pending markers before it takes the
    /// write guard. Inside a rollup's window, between its registration and
    /// its marker file, that listing neither forgets the marker nor lists
    /// it: the marker is a live writer's, not one to recover. If the rollup
    /// then fails with its marker on disk, the marker refuses reads beside
    /// its own writer and after it, and recovery lists it.
    #[tokio::test]
    async fn listing_pending_markers_inside_a_rollup_keeps_its_marker() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join(".rollup-svc");
        let gate = PublicationGate::new();
        {
            let _writer = gate.write().await;
            let registration = gate.mark_rollup(&marker);
            assert!(
                gate.pending_rollup_markers().is_empty(),
                "an in-flight marker is not pending"
            );
            assert!(
                gate.corpus.lock().markers.contains(&marker),
                "nor forgotten while its file does not exist yet"
            );
            std::fs::write(&marker, "hourly inputs").unwrap();
            // The rollup fails before retiring its marker.
            drop(registration);
            assert_eq!(
                gate.unsettled(),
                Some(CorpusUnsettled::RollupPending),
                "a failed rollup's marker, beside its own writer"
            );
        }
        assert!(matches!(
            gate.read().await,
            Err(ServerError::CorpusRecovering(
                CorpusUnsettled::RollupPending
            ))
        ));
        assert_eq!(gate.pending_rollup_markers(), vec![marker]);
    }

    /// A normal rollup, as health, `/metrics`, the scheduler and a manual
    /// run see it without a guard: the corpus reads the same from
    /// `mark_rollup` to the marker's removal, so none of them flaps. A
    /// restart backlog is still reported throughout.
    #[test]
    fn a_normal_rollup_never_moves_a_lock_free_query() {
        for overhang in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join(".rollup-svc");
            let gate = if overhang {
                let gate = PublicationGate::starting();
                gate.finish_hydration(true).unwrap();
                gate
            } else {
                PublicationGate::new()
            };
            let expected = overhang.then_some(CorpusUnsettled::RestartBacklog);
            assert_eq!(gate.unsettled(), expected, "before");

            // No query between the registration and the marker file: one
            // there is the other test's window.
            let writer = gate.blocking_write();
            let registration = gate.mark_rollup(&marker);
            std::fs::write(&marker, "hourly inputs").unwrap();
            assert_eq!(gate.unsettled(), expected, "marker written");
            assert_eq!(gate.overhang(), overhang);
            std::fs::remove_file(&marker).unwrap();
            registration.finish();
            assert_eq!(gate.unsettled(), expected, "marker removed");
            drop(writer);
            assert_eq!(gate.unsettled(), expected, "after");
        }
    }

    /// Consecutive writers with no evaluation between them. Writer A's
    /// rollup fails with its marker on disk, and writer B takes the lock
    /// before any query takes a guard. A lock-free query beside B reports
    /// A's marker, so the scheduler does not claim a run that `read` would
    /// refuse. B retrying the marker does not hide it, because the gate
    /// held it before B registered it. Once B has removed it and a guarded
    /// evaluation runs, the corpus is settled, and the next normal
    /// rollup's in-flight marker is not reported.
    #[test]
    fn a_failed_rollups_marker_is_reported_beside_the_next_writer() {
        let dir = tempfile::tempdir().unwrap();
        let stuck = dir.path().join(".rollup-stuck");
        let gate = PublicationGate::new();
        {
            let _a = gate.blocking_write();
            let _failed = gate.mark_rollup(&stuck);
            std::fs::write(&stuck, "hourly inputs").unwrap();
        }
        {
            let _b = gate.blocking_write();
            assert_eq!(
                gate.unsettled(),
                Some(CorpusUnsettled::RollupPending),
                "beside the next writer"
            );
            // B recovers the stuck marker under its write guard.
            let recovery = gate.mark_rollup(&stuck);
            assert_eq!(
                gate.unsettled(),
                Some(CorpusUnsettled::RollupPending),
                "a retried marker is not in flight"
            );
            std::fs::remove_file(&stuck).unwrap();
            recovery.finish();
            assert_eq!(gate.unsettled(), None, "recovered, beside its writer");
        }
        assert_eq!(gate.unsettled(), None, "a guarded evaluation");

        let normal = dir.path().join(".rollup-normal");
        let _c = gate.blocking_write();
        let _registration = gate.mark_rollup(&normal);
        assert_eq!(gate.unsettled(), None, "registered, file not yet written");
        std::fs::write(&normal, "hourly inputs").unwrap();
        assert_eq!(gate.unsettled(), None, "file written");
    }

    /// A rollup that fails before it writes its marker file leaves a
    /// registered marker with no file. A query without a guard reads no
    /// file, so beside the next writer the marker still reads as pending:
    /// the stale answer refuses rather than admits. The next guarded
    /// evaluation forgets it.
    #[test]
    fn a_marker_without_a_file_reads_pending_until_a_guarded_evaluation() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join(".rollup-svc");
        let gate = PublicationGate::new();
        {
            let _a = gate.blocking_write();
            drop(gate.mark_rollup(&marker));
        }
        {
            let _b = gate.blocking_write();
            assert_eq!(gate.unsettled(), Some(CorpusUnsettled::RollupPending));
        }
        assert_eq!(gate.unsettled(), None);
        assert!(gate.corpus.lock().markers.is_empty());
    }

    /// A failed marker scan is no writer's marker: a lock-free query
    /// reports it while a writer holds the lock as well.
    #[test]
    fn a_failed_scan_is_reported_while_a_writer_holds() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("file");
        std::fs::write(&root, "").unwrap();
        let gate = PublicationGate::new();
        gate.initialize(&root);
        let _writer = gate.blocking_write();
        assert_eq!(gate.unsettled(), Some(CorpusUnsettled::RollupPending));
    }

    #[tokio::test]
    async fn read_rollups_only_ignores_the_restart_backlog() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join(".rollup-svc");
        let gate = PublicationGate::starting();
        assert!(gate.read().await.is_err());
        assert!(gate.read_rollups_only().await.is_ok(), "starting");
        gate.finish_hydration(true).unwrap();
        assert!(gate.read_rollups_only().await.is_ok(), "overhang");

        std::fs::write(&marker, "").unwrap();
        {
            let _writer = gate.write().await;
            drop(gate.mark_rollup(&marker));
        }
        assert!(matches!(
            gate.read_rollups_only().await,
            Err(CorpusUnsettled::RollupPending)
        ));
        std::fs::remove_file(&marker).unwrap();
        assert!(gate.read_rollups_only().await.is_ok());
    }

    #[tokio::test]
    async fn read_rollups_only_waits_for_a_writer() {
        let gate = PublicationGate::starting();
        let writer = gate.write().await;
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                gate.read_rollups_only()
            )
            .await
            .is_err()
        );
        drop(writer);
        assert!(gate.read_rollups_only().await.is_ok());
    }

    /// A complete rescan clears a failed scan and registers the markers it
    /// finds; those still refuse until they resolve (ADR-0041 slice 2).
    #[tokio::test]
    async fn rollup_boot_recovery_rescan_registers_what_the_complete_scan_finds() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("data");
        std::fs::write(&root, "").unwrap();
        let gate = PublicationGate::new();
        gate.initialize(&root);
        assert_eq!(gate.unsettled(), Some(CorpusUnsettled::RollupPending));

        // A marker a writer registered while the scan was failed survives
        // the rescan that clears the failure.
        let registered = dir.path().join(".rollup-registered");
        std::fs::write(&registered, "").unwrap();
        {
            let _writer = gate.write().await;
            drop(gate.mark_rollup(&registered));
        }

        std::fs::remove_file(&root).unwrap();
        let day = root.join("prod/2026-01-01");
        std::fs::create_dir_all(&day).unwrap();
        let found = day.join(".rollup-svc");
        std::fs::write(&found, "").unwrap();
        gate.rescan_if_failed(&root);

        let mut pending = gate.pending_rollup_markers();
        pending.sort();
        let mut expected = vec![found.clone(), registered.clone()];
        expected.sort();
        assert_eq!(pending, expected);
        assert!(matches!(
            gate.read().await,
            Err(ServerError::CorpusRecovering(
                CorpusUnsettled::RollupPending
            ))
        ));
        std::fs::remove_file(&found).unwrap();
        std::fs::remove_file(&registered).unwrap();
        assert!(
            gate.read().await.is_ok(),
            "the failed scan no longer refuses"
        );
    }

    /// Every entry under `root` by path: a regular file with its bytes,
    /// anything else with none.
    fn tree(root: &Path) -> std::collections::BTreeMap<PathBuf, Option<Vec<u8>>> {
        let mut out = std::collections::BTreeMap::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                let kind = std::fs::symlink_metadata(&path).unwrap().file_type();
                if kind.is_dir() {
                    stack.push(path.clone());
                }
                let bytes = kind.is_file().then(|| std::fs::read(&path).unwrap());
                out.insert(path, bytes);
            }
        }
        out
    }

    /// A rollup marker under an env child that is not a canonical date is
    /// found, refuses corpus reads with a 503 `corpus_recovering` whose
    /// cause is `rollup_pending`, and outlives boot recovery with every
    /// file beside it byte-identical (ADR-0041, amendment of 2026-11-08).
    /// Under a canonical date, its identities would promote the tmp and
    /// retire the input.
    #[test]
    fn marker_under_noncanonical_parent_keeps_reads_refused() {
        use axum::response::IntoResponse as _;

        use crate::ingest::publication_marker::identity_of;
        use crate::ingest::rollup_marker::RollupMarker;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("data");
        let day = root.join("prod/2026-13-45");
        std::fs::create_dir_all(day.join("07")).unwrap();
        let input = day.join("07/svc.parquet");
        let merged = day.join("svc.parquet.tmp");
        std::fs::write(&input, b"hourly rows").unwrap();
        std::fs::write(&merged, b"merged rows").unwrap();
        let marker = RollupMarker::new(
            "svc",
            identity_of(&merged).unwrap(),
            None,
            vec![("07/svc.parquet".to_owned(), identity_of(&input).unwrap())],
        )
        .unwrap();
        let marker_path = marker.marker_path(&day);
        std::fs::write(&marker_path, marker.encode()).unwrap();
        let before = tree(&root);

        let mut found = HashSet::new();
        scan_markers(&root, &mut found).unwrap();
        assert_eq!(found, HashSet::from([marker_path.clone()]));

        let gate = PublicationGate::new();
        gate.initialize(&root);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let refused = |gate: &PublicationGate| match runtime.block_on(gate.read()) {
            Ok(_) => panic!("corpus reads answered"),
            Err(refusal) => refusal,
        };
        let refusal = refused(&gate);
        assert!(matches!(
            refusal,
            ServerError::CorpusRecovering(CorpusUnsettled::RollupPending)
        ));
        assert_eq!(refusal.cause_kind().as_str(), "rollup_pending");
        let response = refusal.into_response();
        assert_eq!(
            response.status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        let body = runtime
            .block_on(axum::body::to_bytes(response.into_body(), usize::MAX))
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["code"], "corpus_recovering");

        capturing(|capture| {
            crate::ingest::compaction::recover_rollups_at_boot(&gate);
            let boot: Vec<tracing::Level> = capture
                .0
                .lock()
                .iter()
                .filter(|(_, event)| event.as_deref() == Some("rollup_boot_recovery"))
                .map(|(level, _)| *level)
                .collect();
            assert_eq!(boot, [tracing::Level::ERROR], "boot finished no recovery");
            assert_eq!(capture.count("rollup_recovery_unresolved"), 1);
        });
        assert_eq!(tree(&root), before, "recovery changed the data root");
        assert_eq!(gate.pending_rollup_markers(), vec![marker_path]);
        assert!(matches!(
            refused(&gate),
            ServerError::CorpusRecovering(CorpusUnsettled::RollupPending)
        ));
    }

    /// A gate whose scan did not fail never rescans: a marker that appears
    /// on disk behind its back is a writer's to register.
    #[tokio::test]
    async fn rollup_boot_recovery_rescan_leaves_a_healthy_gate_alone() {
        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("prod/2026-01-01");
        std::fs::create_dir_all(&day).unwrap();
        let gate = PublicationGate::new();
        gate.initialize(dir.path());
        std::fs::write(day.join(".rollup-svc"), "").unwrap();
        capturing(|capture| {
            gate.rescan_if_failed(dir.path());
            assert_eq!(capture.count("publication_scan_recovered"), 0);
        });
        assert!(gate.pending_rollup_markers().is_empty());
        assert!(gate.read().await.is_ok());
    }
}
