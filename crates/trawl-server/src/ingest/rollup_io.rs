// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The file operations of a daily rollup and its recovery, behind one
//! handle (#357).
//!
//! The rollup and its recovery make every open, footer check, sync, hash,
//! rename, unlink and retirement through a [`RollupIo`], and each call names
//! the [`Phase`] of the publish or recovery it belongs to. The caller passes
//! the handle down and moves it into each blocking task. Nothing is kept in
//! a thread-local or a static.
//!
//! [`RollupIo::REAL`] runs the real calls. Outside tests it has no fields,
//! so each wrapper compiles down to the call it wraps. Under `cfg(test)` or
//! the `test-support` feature, `RollupIo::recording` attaches a
//! `Recorder`. Each call appends one `Entry` when it starts and fills in
//! its result when it returns, so a call made inside another one (the day
//! fsync inside the marker publish) is listed after it, in the order the
//! calls started. The recorder can also fault one call, chosen by phase and
//! by how many earlier calls of that phase this handle made.
//!
//! The entries record the results the system calls returned. They say
//! nothing about what reached the disk: a process that is killed keeps its
//! page cache, so no test here proves the order survives a power loss.

use std::fs::File;
use std::io;
use std::path::Path;

use super::publication_marker::{self, OutputIdentity};
use super::rollup_marker::Observation;

/// The step of the rollup or of its recovery that one call belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Phase {
    /// Open the tmp the merge wrote.
    TmpOpen,
    /// Parse the tmp's footer.
    TmpFooter,
    /// `sync_all` the tmp.
    TmpSync,
    /// Hash the tmp.
    TmpHash,
    /// Open a merged hourly input to hash it.
    InputOpen,
    /// Hash a merged hourly input.
    InputHash,
    /// Open the prior daily file that the merge read.
    PriorOpen,
    /// Hash the prior daily file.
    PriorHash,
    /// fsync the day directory and its ancestors before the marker.
    ChainSync,
    /// Publish the marker durably. Its two syncs follow it as
    /// [`Self::MarkerStagedSync`] and [`Self::MarkerDaySync`].
    MarkerPublish,
    /// `sync_all` the marker's staged file, inside the publish.
    MarkerStagedSync,
    /// fsync the day directory after the marker's rename, inside the
    /// publish.
    MarkerDaySync,
    /// Rename the tmp onto the canonical daily file.
    Rename,
    /// fsync the day directory after the rename.
    RenameDaySync,
    /// Retire one listed hourly input.
    Retire,
    /// fsync one hour directory after its input is retired.
    HourSync,
    /// Unlink the marker.
    MarkerUnlink,
    /// fsync the day directory after the marker's unlink.
    UnlinkDaySync,
    /// A later pass retries the day fsync that an unlink still owes.
    ObligationRetrySync,
    /// Recovery opens or hashes a file to learn its identity.
    RecoveryObserve,
    /// Recovery fsyncs the day directory and its ancestors.
    RecoveryChainSync,
    /// Recovery removes a leftover tmp.
    TmpRemove,
}

/// The kind of call an `Entry` records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Open a file through the caller's opener.
    Open,
    /// Parse a Parquet footer on an open file.
    Footer,
    /// `sync_all` an open file.
    SyncFile,
    /// Hash an open file from its first byte.
    Hash,
    /// fsync a directory.
    SyncDir,
    /// Publish a marker durably.
    PublishMarker,
    /// Rename a file. The entry's path is the source.
    Rename,
    /// Unlink a file.
    RemoveFile,
    /// Retire a merged input through the caller's retirement.
    Retire,
}

/// The I/O handle of one rollup or recovery. See the
/// [module documentation](self).
#[derive(Debug, Clone, Default)]
pub struct RollupIo {
    #[cfg(any(test, feature = "test-support"))]
    recorder: Option<std::sync::Arc<Recorder>>,
}

// The production handle carries nothing, so passing it costs nothing.
#[cfg(not(any(test, feature = "test-support")))]
const _: () = assert!(std::mem::size_of::<RollupIo>() == 0);

impl RollupIo {
    /// Run the real calls and record nothing.
    pub const REAL: Self = Self {
        #[cfg(any(test, feature = "test-support"))]
        recorder: None,
    };

    /// Run every call through `recorder`.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn recording(recorder: std::sync::Arc<Recorder>) -> Self {
        Self {
            recorder: Some(recorder),
        }
    }

    #[inline]
    fn call<T>(
        &self,
        op: Op,
        phase: Phase,
        path: &Path,
        real: impl FnOnce() -> io::Result<T>,
    ) -> io::Result<T> {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(recorder) = &self.recorder {
            return recorder.call(op, phase, path, real);
        }
        let _ = (self, op, phase, path);
        real()
    }

    /// Open `path` with `open`, which judges what it opened. The rollup
    /// passes its no-follow, regular-file-only opener.
    pub fn open<T>(
        &self,
        phase: Phase,
        path: &Path,
        open: impl FnOnce(&Path) -> io::Result<T>,
    ) -> io::Result<T> {
        self.call(Op::Open, phase, path, || open(path))
    }

    /// Parse the Parquet footer of `file`, opened from `path`. A footer
    /// that does not parse is an `InvalidData` error, unless the parquet
    /// reader failed on I/O, whose kind it keeps. The file's offset is left
    /// wherever the reader left it.
    pub fn check_footer(&self, phase: Phase, path: &Path, file: &File) -> io::Result<()> {
        self.call(Op::Footer, phase, path, || {
            trawl_engine::parquet_stats::check_footer(file).map_err(|e| {
                // An I/O failure inside the reader is its `source`.
                let kind = std::error::Error::source(&e)
                    .and_then(|source| source.downcast_ref::<io::Error>())
                    .map_or(io::ErrorKind::InvalidData, io::Error::kind);
                io::Error::new(kind, format!("invalid parquet footer: {e}"))
            })
        })
    }

    /// `sync_all` `file`, opened from `path`.
    pub fn sync_file(&self, phase: Phase, path: &Path, file: &File) -> io::Result<()> {
        self.call(Op::SyncFile, phase, path, || file.sync_all())
    }

    /// The identity of `file`, opened from `path`, hashed from its first
    /// byte ([`publication_marker::identity_of_file`]).
    pub fn hash(&self, phase: Phase, path: &Path, file: &mut File) -> io::Result<OutputIdentity> {
        self.call(Op::Hash, phase, path, || {
            publication_marker::identity_of_file(file)
        })
    }

    /// fsync the directory `dir` ([`crate::epoch::fsync_dir`]).
    pub fn sync_dir(&self, phase: Phase, dir: &Path) -> io::Result<()> {
        self.call(Op::SyncDir, phase, dir, || crate::epoch::fsync_dir(dir))
    }

    /// Publish the marker `dir/name` with `body` durably
    /// ([`crate::epoch::publish_marker_durable_with`]): its staged file's
    /// sync is a [`Phase::MarkerStagedSync`] call and the directory fsync a
    /// [`Phase::MarkerDaySync`] call. Either failing fails the publish.
    pub fn publish_marker(&self, dir: &Path, name: &str, body: &str) -> io::Result<()> {
        let marker = dir.join(name);
        self.call(Op::PublishMarker, Phase::MarkerPublish, &marker, || {
            crate::epoch::publish_marker_durable_with(
                dir,
                name,
                body,
                |staged, file| self.sync_file(Phase::MarkerStagedSync, staged, file),
                |dir| self.sync_dir(Phase::MarkerDaySync, dir),
            )
        })
    }

    /// What is at `path`, for recovery's [`classify`](super::rollup_marker::classify): the
    /// file `open` opens, hashed. The open and the hash are each a
    /// [`Phase::RecoveryObserve`] call. A `NotFound` open is
    /// [`Observation::Absent`]; any other failure is
    /// [`Observation::Unreadable`].
    pub fn observe(
        &self,
        path: &Path,
        open: impl FnOnce(&Path) -> io::Result<File>,
    ) -> Observation {
        match self.open(Phase::RecoveryObserve, path, open) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Observation::Absent,
            Err(_) => Observation::Unreadable,
            Ok(mut file) => match self.hash(Phase::RecoveryObserve, path, &mut file) {
                Ok(identity) => Observation::Identity(identity),
                Err(_) => Observation::Unreadable,
            },
        }
    }

    /// Rename `from` to `to`. The entry records `from`.
    pub fn rename(&self, phase: Phase, from: &Path, to: &Path) -> io::Result<()> {
        self.call(Op::Rename, phase, from, || std::fs::rename(from, to))
    }

    /// Unlink `path`. A missing file is the `NotFound` error the call
    /// returned, for the caller to judge.
    pub fn remove_file(&self, phase: Phase, path: &Path) -> io::Result<()> {
        self.call(Op::RemoveFile, phase, path, || std::fs::remove_file(path))
    }

    /// Retire the merged input `path` with `retire`. A retirement reports
    /// failure as text with no I/O kind, so a real failure records as
    /// `Other`; the text comes back unchanged.
    pub fn retire(
        &self,
        path: &Path,
        retire: impl FnOnce(&Path) -> Result<(), String>,
    ) -> Result<(), String> {
        self.call(Op::Retire, Phase::Retire, path, || {
            retire(path).map_err(io::Error::other)
        })
        .map_err(|e| e.to_string())
    }
}

#[cfg(any(test, feature = "test-support"))]
pub use recorder::{Entry, Outcome, Recorder};

#[cfg(any(test, feature = "test-support"))]
mod recorder {
    use std::collections::HashMap;
    use std::fmt;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use parking_lot::Mutex;

    use super::{Op, Phase, RollupIo};

    /// What one call returned.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Outcome {
        /// The real call succeeded.
        Ok,
        /// The real call failed with this kind.
        Err(io::ErrorKind),
        /// The recorder failed the call with this kind, and the real call
        /// never ran.
        Injected(io::ErrorKind),
    }

    /// One call, in the order calls started.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Entry {
        pub op: Op,
        pub path: PathBuf,
        pub phase: Phase,
        /// How many calls of this phase this recorder saw before this one.
        pub occurrence: usize,
        /// `None` while the call runs, or when it panicked.
        pub result: Option<Outcome>,
    }

    type Hook = Box<dyn FnOnce(&Path) + Send>;

    enum Fault {
        /// Return this kind and skip the real call.
        Fail(io::ErrorKind),
        /// Run the hook on the call's path, then the real call.
        Before(Hook),
    }

    impl fmt::Debug for Fault {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::Fail(kind) => f.debug_tuple("Fail").field(kind).finish(),
                Self::Before(_) => f.write_str("Before(..)"),
            }
        }
    }

    #[derive(Debug, Default)]
    struct State {
        entries: Vec<Entry>,
        seen: HashMap<Phase, usize>,
        faults: HashMap<(Phase, usize), Fault>,
    }

    /// The calls one test's [`RollupIo`] made, and the faults it injects.
    /// Each fault fires at most once.
    #[derive(Debug, Default)]
    pub struct Recorder {
        state: Mutex<State>,
    }

    /// The text of every injected error.
    pub const INJECTED: &str = "injected rollup I/O fault";

    impl Recorder {
        /// A recorder with no entries and no faults.
        #[must_use]
        pub fn new() -> Arc<Self> {
            Arc::default()
        }

        /// A [`RollupIo`] that runs every call through this recorder.
        #[must_use]
        pub fn io(self: &Arc<Self>) -> RollupIo {
            RollupIo::recording(Arc::clone(self))
        }

        /// Fail the `occurrence`th call of `phase`, counting from 0, with
        /// `kind`. The real call does not run.
        pub fn fail(&self, phase: Phase, occurrence: usize, kind: io::ErrorKind) {
            self.state
                .lock()
                .faults
                .insert((phase, occurrence), Fault::Fail(kind));
        }

        /// Run `hook` on the path of the `occurrence`th call of `phase`,
        /// counting from 0, then make the real call.
        pub fn before(
            &self,
            phase: Phase,
            occurrence: usize,
            hook: impl FnOnce(&Path) + Send + 'static,
        ) {
            self.state
                .lock()
                .faults
                .insert((phase, occurrence), Fault::Before(Box::new(hook)));
        }

        /// Every call so far, in the order they started.
        #[must_use]
        pub fn entries(&self) -> Vec<Entry> {
            self.state.lock().entries.clone()
        }

        /// The phases of [`Self::entries`], in order.
        #[must_use]
        pub fn phases(&self) -> Vec<Phase> {
            self.state.lock().entries.iter().map(|e| e.phase).collect()
        }

        pub(super) fn call<T>(
            &self,
            op: Op,
            phase: Phase,
            path: &Path,
            real: impl FnOnce() -> io::Result<T>,
        ) -> io::Result<T> {
            // The lock is released before the call: a nested call records
            // through the same recorder.
            let (index, fault) = {
                let mut state = self.state.lock();
                let seen = state.seen.entry(phase).or_default();
                let occurrence = *seen;
                *seen += 1;
                let fault = state.faults.remove(&(phase, occurrence));
                state.entries.push(Entry {
                    op,
                    path: path.to_path_buf(),
                    phase,
                    occurrence,
                    result: None,
                });
                (state.entries.len() - 1, fault)
            };
            let (result, outcome) = match fault {
                Some(Fault::Fail(kind)) => {
                    (Err(io::Error::new(kind, INJECTED)), Outcome::Injected(kind))
                }
                Some(Fault::Before(hook)) => {
                    hook(path);
                    let result = real();
                    let outcome = outcome_of(&result);
                    (result, outcome)
                }
                None => {
                    let result = real();
                    let outcome = outcome_of(&result);
                    (result, outcome)
                }
            };
            self.state.lock().entries[index].result = Some(outcome);
            result
        }
    }

    fn outcome_of<T>(result: &io::Result<T>) -> Outcome {
        match result {
            Ok(_) => Outcome::Ok,
            Err(e) => Outcome::Err(e.kind()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Read as _};
    use std::path::Path;
    use std::sync::{Arc, Barrier};

    use super::*;

    fn parquet(path: &Path) {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "COPY (SELECT 1 AS n) TO '{}' (FORMAT PARQUET)",
            path.display()
        ))
        .unwrap();
    }

    #[test]
    fn real_io_records_nothing_and_runs_each_call() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("day.parquet");
        parquet(&file);
        let io = RollupIo::REAL;
        let mut opened = io.open(Phase::TmpOpen, &file, |p| File::open(p)).unwrap();
        io.check_footer(Phase::TmpFooter, &file, &opened).unwrap();
        io.sync_file(Phase::TmpSync, &file, &opened).unwrap();
        let identity = io.hash(Phase::TmpHash, &file, &mut opened).unwrap();
        assert_eq!(identity, publication_marker::identity_of(&file).unwrap());
        io.sync_dir(Phase::ChainSync, tmp.path()).unwrap();
        io.publish_marker(tmp.path(), ".rollup-api", "body")
            .unwrap();
        let moved = tmp.path().join("moved.parquet");
        io.rename(Phase::Rename, &file, &moved).unwrap();
        io.retire(&moved, |p| {
            std::fs::remove_file(p).map_err(|e| e.to_string())
        })
        .unwrap();
        io.remove_file(Phase::MarkerUnlink, &tmp.path().join(".rollup-api"))
            .unwrap();
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[test]
    fn entries_record_the_real_result_phase_and_occurrence() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = Recorder::new();
        let io = recorder.io();
        let missing = tmp.path().join("missing");
        let err = io
            .open(Phase::InputOpen, &missing, |p| File::open(p))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        io.sync_dir(Phase::HourSync, tmp.path()).unwrap();
        io.sync_dir(Phase::HourSync, tmp.path()).unwrap();
        assert_eq!(
            recorder.entries(),
            vec![
                Entry {
                    op: Op::Open,
                    path: missing,
                    phase: Phase::InputOpen,
                    occurrence: 0,
                    result: Some(Outcome::Err(io::ErrorKind::NotFound)),
                },
                Entry {
                    op: Op::SyncDir,
                    path: tmp.path().to_path_buf(),
                    phase: Phase::HourSync,
                    occurrence: 0,
                    result: Some(Outcome::Ok),
                },
                Entry {
                    op: Op::SyncDir,
                    path: tmp.path().to_path_buf(),
                    phase: Phase::HourSync,
                    occurrence: 1,
                    result: Some(Outcome::Ok),
                },
            ]
        );
    }

    #[test]
    fn a_fault_on_occurrence_one_fires_on_the_second_call_only() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = Recorder::new();
        recorder.fail(Phase::UnlinkDaySync, 1, io::ErrorKind::StorageFull);
        let io = recorder.io();
        io.sync_dir(Phase::UnlinkDaySync, tmp.path()).unwrap();
        // Another phase's calls do not count toward this one's occurrences.
        io.sync_dir(Phase::RenameDaySync, tmp.path()).unwrap();
        let err = io.sync_dir(Phase::UnlinkDaySync, tmp.path()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::StorageFull);
        assert_eq!(err.to_string(), recorder::INJECTED);
        io.sync_dir(Phase::UnlinkDaySync, tmp.path()).unwrap();
        let results: Vec<_> = recorder
            .entries()
            .into_iter()
            .map(|e| (e.phase, e.occurrence, e.result.unwrap()))
            .collect();
        assert_eq!(
            results,
            [
                (Phase::UnlinkDaySync, 0, Outcome::Ok),
                (Phase::RenameDaySync, 0, Outcome::Ok),
                (
                    Phase::UnlinkDaySync,
                    1,
                    Outcome::Injected(io::ErrorKind::StorageFull)
                ),
                (Phase::UnlinkDaySync, 2, Outcome::Ok),
            ]
        );
    }

    #[test]
    fn an_injected_failure_skips_the_real_call() {
        let tmp = tempfile::tempdir().unwrap();
        let from = tmp.path().join("a");
        let to = tmp.path().join("b");
        std::fs::write(&from, b"x").unwrap();
        let recorder = Recorder::new();
        recorder.fail(Phase::Rename, 0, io::ErrorKind::PermissionDenied);
        recorder.fail(Phase::Retire, 0, io::ErrorKind::PermissionDenied);
        let io = recorder.io();
        assert!(io.rename(Phase::Rename, &from, &to).is_err());
        assert!(from.exists() && !to.exists(), "the rename never ran");
        let err = io
            .retire(&from, |_| panic!("the retirement never runs"))
            .unwrap_err();
        assert_eq!(err, recorder::INJECTED);
        assert!(from.exists());
    }

    #[test]
    fn a_before_hook_changes_the_file_system_then_the_real_call_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_file = tmp.path().join("api.parquet.tmp");
        std::fs::write(&tmp_file, b"x").unwrap();
        let recorder = Recorder::new();
        recorder.before(Phase::TmpRemove, 0, |path| {
            std::fs::remove_file(path).unwrap();
        });
        let io = recorder.io();
        let err = io.remove_file(Phase::TmpRemove, &tmp_file).unwrap_err();
        // The real unlink ran after the hook and found nothing: its own
        // result is what the entry holds.
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert_eq!(
            recorder.entries()[0].result,
            Some(Outcome::Err(io::ErrorKind::NotFound))
        );
    }

    #[test]
    fn a_marker_publish_records_its_inner_syncs_after_it() {
        let tmp = tempfile::tempdir().unwrap();
        let day = tmp.path();
        let recorder = Recorder::new();
        let io = recorder.io();
        io.publish_marker(day, ".rollup-api", "body").unwrap();
        let entries = recorder.entries();
        let shape: Vec<_> = entries
            .iter()
            .map(|e| (e.op, e.phase, e.result.unwrap()))
            .collect();
        assert_eq!(
            shape,
            [
                (Op::PublishMarker, Phase::MarkerPublish, Outcome::Ok),
                (Op::SyncFile, Phase::MarkerStagedSync, Outcome::Ok),
                (Op::SyncDir, Phase::MarkerDaySync, Outcome::Ok),
            ]
        );
        assert_eq!(entries[0].path, day.join(".rollup-api"));
        assert_eq!(entries[2].path, day);
        let mut body = String::new();
        File::open(day.join(".rollup-api"))
            .unwrap()
            .read_to_string(&mut body)
            .unwrap();
        assert_eq!(body, "body");
    }

    #[test]
    fn a_failed_inner_sync_fails_the_publish_with_its_kind() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = Recorder::new();
        recorder.fail(Phase::MarkerDaySync, 0, io::ErrorKind::StorageFull);
        let io = recorder.io();
        let err = io
            .publish_marker(tmp.path(), ".rollup-api", "body")
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::StorageFull);
        let results: Vec<_> = recorder
            .entries()
            .iter()
            .map(|e| e.result.unwrap())
            .collect();
        assert_eq!(
            results,
            [
                // The publish ran and returned the error of the sync inside.
                Outcome::Err(io::ErrorKind::StorageFull),
                Outcome::Ok,
                Outcome::Injected(io::ErrorKind::StorageFull),
            ]
        );
    }

    #[test]
    fn a_bad_footer_is_invalid_data() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("api.parquet.tmp");
        std::fs::write(&path, b"PAR1 not a footer \x08\x00\x00\x00PAR1").unwrap();
        let recorder = Recorder::new();
        let io = recorder.io();
        let file = File::open(&path).unwrap();
        let err = io.check_footer(Phase::TmpFooter, &path, &file).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            recorder.entries()[0].result,
            Some(Outcome::Err(io::ErrorKind::InvalidData))
        );
    }

    #[test]
    fn observe_reports_absent_identity_and_unreadable() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("api.parquet");
        std::fs::write(&file, b"rows").unwrap();
        let recorder = Recorder::new();
        let io = recorder.io();
        assert_eq!(
            io.observe(&file, |p| File::open(p)),
            Observation::Identity(publication_marker::identity_of(&file).unwrap())
        );
        assert_eq!(
            io.observe(&tmp.path().join("03/api.parquet"), |p| File::open(p)),
            Observation::Absent
        );
        assert_eq!(
            io.observe(&file, |_| Err(io::Error::from(io::ErrorKind::InvalidInput))),
            Observation::Unreadable
        );
        let ops: Vec<_> = recorder.entries().iter().map(|e| (e.op, e.phase)).collect();
        assert_eq!(
            ops,
            [
                (Op::Open, Phase::RecoveryObserve),
                (Op::Hash, Phase::RecoveryObserve),
                (Op::Open, Phase::RecoveryObserve),
                (Op::Open, Phase::RecoveryObserve),
            ]
        );
    }

    #[test]
    fn two_recorders_on_two_threads_share_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let run = |phase: Phase, fail_at: usize| {
            let dir = tmp.path().to_path_buf();
            let barrier = Arc::clone(&barrier);
            let recorder = Recorder::new();
            recorder.fail(phase, fail_at, io::ErrorKind::StorageFull);
            let io = recorder.io();
            let handle = std::thread::spawn(move || {
                let mut failed = Vec::new();
                for round in 0..50 {
                    // Both threads are inside their loops at once.
                    if round == 0 {
                        barrier.wait();
                    }
                    if io.sync_dir(phase, &dir).is_err() {
                        failed.push(round);
                    }
                }
                failed
            });
            (recorder, handle)
        };
        let (left, left_thread) = run(Phase::HourSync, 3);
        let (right, right_thread) = run(Phase::HourSync, 7);
        assert_eq!(left_thread.join().unwrap(), [3]);
        assert_eq!(right_thread.join().unwrap(), [7]);
        for (recorder, failed_at) in [(left, 3), (right, 7)] {
            let entries = recorder.entries();
            assert_eq!(entries.len(), 50, "only its own thread's calls");
            for (i, entry) in entries.iter().enumerate() {
                assert_eq!(entry.occurrence, i);
                let expected = if i == failed_at {
                    Outcome::Injected(io::ErrorKind::StorageFull)
                } else {
                    Outcome::Ok
                };
                assert_eq!(entry.result, Some(expected));
            }
        }
    }
}
