// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Write-ahead log for crash-safe event ingestion.
//!
//! Ingest writes one WAL file per `(env, service)` batch; compaction later
//! converts those files to parquet. [`WalWriter::write`] carries the
//! durability sequence and the reason for each step.

use std::collections::HashSet;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use parking_lot::Mutex;

/// A WAL write that was not acknowledged.
///
/// The variant tells the caller whether the batch's bytes are still visible
/// to compaction, which decides whether writing them again is safe.
#[derive(Debug, thiserror::Error)]
pub enum WalWriteError {
    /// No file from this write remains under a `.ndjson` name. The write
    /// failed before its rename, or its directory fsync failed and the file
    /// was withdrawn. Writing the same events again cannot duplicate them.
    #[error("{0}")]
    NotPublished(#[source] std::io::Error),
    /// The directory fsync failed after the rename, and removing the file
    /// failed too. The file stays under its final name, where compaction
    /// will merge it, so writing the same events again would duplicate them.
    #[error(
        "{sync}; withdrawing {} also failed ({withdraw}), so it stays visible to compaction",
        path.display()
    )]
    LeftVisible {
        path: PathBuf,
        #[source]
        sync: std::io::Error,
        withdraw: std::io::Error,
    },
}

impl WalWriteError {
    /// Whether the unacknowledged file is still visible to compaction.
    pub fn left_visible(&self) -> bool {
        matches!(self, Self::LeftVisible { .. })
    }
}

impl From<std::io::Error> for WalWriteError {
    fn from(error: std::io::Error) -> Self {
        Self::NotPublished(error)
    }
}

/// Atomic WAL file writer for ingest events.
#[derive(Debug)]
pub struct WalWriter {
    wal_dir: PathBuf,
    /// Environments whose directory entry this process has made durable
    /// in `wal_dir`. An env is added only after the root fsync succeeds,
    /// so two racing first writers both sync and neither skips the barrier.
    /// A directory recreated after removal is synced again regardless.
    durable_envs: Mutex<HashSet<String>>,
    #[cfg(any(test, feature = "test-support"))]
    fail_next_directory_sync: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    fail_next_withdraw: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    panic_after_writes: std::sync::atomic::AtomicUsize,
}

impl WalWriter {
    pub fn new(wal_dir: PathBuf) -> Self {
        Self {
            wal_dir,
            durable_envs: Mutex::new(HashSet::new()),
            #[cfg(any(test, feature = "test-support"))]
            fail_next_directory_sync: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            fail_next_withdraw: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            panic_after_writes: std::sync::atomic::AtomicUsize::new(usize::MAX),
        }
    }

    pub fn ensure_dir(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.wal_dir)
    }

    pub fn dir(&self) -> &Path {
        &self.wal_dir
    }

    /// Write events durably: create `.tmp`, fsync its data, rename to
    /// `.ndjson`, then fsync the parent directory so the rename itself
    /// survives a crash. `Ok` is the acknowledgement: every step,
    /// directory fsyncs included, has succeeded.
    ///
    /// The fsync *before* the rename prevents a torn write: without it the
    /// rename can be journaled before the data blocks reach the device, so a
    /// hard kill / power loss leaves a full-length `.ndjson` of NUL bytes
    /// that head-of-line-blocks compaction.
    ///
    /// The parent-directory fsync makes the rename entry durable. Without
    /// it, a power loss can drop an acknowledged batch. When it fails, the
    /// file is withdrawn (unlinked) before the error returns, so a caller
    /// that retries cannot duplicate the batch. If the withdrawal fails too,
    /// [`WalWriteError::LeftVisible`] says so. The first write into an
    /// environment in this process also fsyncs `wal_dir`, which holds the
    /// env directory's own entry, and fails before writing anything if
    /// that sync fails.
    ///
    /// Files land in `wal_dir/{env}/` (lazily created), named
    /// `{service}_{unix_millis}_{4_hex}.ndjson` with the service name
    /// verbatim — path encoding is injective by validation (ADR-0009):
    /// both `env` and `service` were validated at ingest, so `api.v2`
    /// and `api_v2` are distinct files and pruning stays exact.
    pub fn write(&self, env: &str, service: &str, events: &[u8]) -> Result<PathBuf, WalWriteError> {
        let filename = Self::generate_filename(service)?;
        let env_dir = self.wal_dir.join(env);
        self.ensure_env_dir(env, &env_dir)?;
        let tmp_path = env_dir.join(format!("{filename}.tmp"));
        let final_path = env_dir.join(format!("{filename}.ndjson"));

        let mut file = File::create(&tmp_path)?;
        file.write_all(events)?;
        file.sync_all()?;
        drop(file);

        std::fs::rename(&tmp_path, &final_path)?;

        // fsync the directory entry so the rename is durable, not just the
        // file's data. The rename has already made the file visible to
        // compaction, so a failed sync withdraws it before rejecting the
        // write: an unacknowledged batch must not be merged, or the
        // sender's retry would duplicate it.
        if let Err(sync) = self.sync_directory(&env_dir) {
            Self::count_directory_sync_failure();
            let withdrawn = self.withdraw(&final_path);
            // `withdrawn = false` means compaction can still merge the file.
            tracing::warn!(
                event_type = "wal_dir_fsync_failed",
                dir = %env_dir.display(),
                error = %sync,
                withdrawn = withdrawn.is_ok(),
                withdraw_error = withdrawn.as_ref().err().map(tracing::field::display),
                "WAL directory fsync failed; the write is rejected"
            );
            return Err(match withdrawn {
                Ok(()) => WalWriteError::NotPublished(sync),
                Err(withdraw) => WalWriteError::LeftVisible {
                    path: final_path,
                    sync,
                    withdraw,
                },
            });
        }

        #[cfg(test)]
        {
            use std::sync::atomic::Ordering;
            let previous = self.panic_after_writes.fetch_update(
                Ordering::Relaxed,
                Ordering::Relaxed,
                |remaining| (remaining != usize::MAX).then(|| remaining.saturating_sub(1)),
            );
            assert_ne!(previous, Ok(1), "injected panic after durable WAL write");
        }
        Ok(final_path)
    }

    /// Create `env_dir` if it is missing, and make its entry in `wal_dir`
    /// durable whenever this call created it or this process has not synced
    /// it yet. A file acknowledged into a directory whose own entry is lost
    /// on power failure is lost with it.
    fn ensure_env_dir(&self, env: &str, env_dir: &Path) -> std::io::Result<()> {
        let created = match std::fs::create_dir(env_dir) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(env_dir)?;
                true
            }
            Err(e) => return Err(e),
        };
        if !created && self.durable_envs.lock().contains(env) {
            return Ok(());
        }
        if let Err(e) = self.sync_directory(&self.wal_dir) {
            Self::count_directory_sync_failure();
            tracing::warn!(
                event_type = "wal_dir_fsync_failed",
                dir = %self.wal_dir.display(),
                error = %e,
                "WAL directory fsync failed before the first write into this \
                 environment; the write is rejected"
            );
            return Err(e);
        }
        self.durable_envs.lock().insert(env.to_owned());
        Ok(())
    }

    fn count_directory_sync_failure() {
        metrics::counter!(crate::metrics::WAL_DURABILITY_FAILURES_TOTAL,
            "operation" => crate::metrics::WalDurabilityOperation::ParentDirectorySync.label())
        .increment(1);
    }

    #[cfg_attr(not(any(test, feature = "test-support")), allow(clippy::unused_self))]
    fn sync_directory(&self, dir: &Path) -> std::io::Result<()> {
        #[cfg(any(test, feature = "test-support"))]
        if self
            .fail_next_directory_sync
            .swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            return Err(std::io::Error::other("injected WAL directory sync failure"));
        }
        File::open(dir).and_then(|d| d.sync_all())
    }

    #[cfg_attr(not(test), allow(clippy::unused_self))]
    fn withdraw(&self, path: &Path) -> std::io::Result<()> {
        #[cfg(test)]
        if self
            .fail_next_withdraw
            .swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            return Err(std::io::Error::other("injected WAL withdrawal failure"));
        }
        std::fs::remove_file(path)
    }

    /// Fail this writer's next directory barrier: the `wal_dir` sync of a
    /// first write into an env, or the env directory sync after a rename.
    #[cfg(any(test, feature = "test-support"))]
    pub fn fail_next_directory_sync_for_test(&self) {
        self.fail_next_directory_sync
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Fail the next withdrawal of a file whose directory sync failed,
    /// leaving that file visible under its final name.
    #[cfg(test)]
    pub(crate) fn fail_next_withdraw_for_test(&self) {
        self.fail_next_withdraw
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Panic after this many completed durable writes on this writer only.
    #[cfg(test)]
    pub(crate) fn panic_after_writes_for_test(&self, count: usize) {
        assert!(count > 0);
        self.panic_after_writes
            .store(count, std::sync::atomic::Ordering::Relaxed);
    }

    /// Generate a unique filename: `{service}_{unix_millis}_{4_hex_random}`.
    ///
    /// The service name is carried verbatim — it was validated at ingest.
    fn generate_filename(service: &str) -> std::io::Result<String> {
        let millis = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(std::io::Error::other)?
            .as_millis();

        // 4 hex chars of randomness to avoid collisions within the same ms.
        let random: u16 = rand::random();
        let hex = format!("{random:04x}");

        Ok(format!("{service}_{millis}_{hex}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIR_SYNC_FAILURES: &str =
        "trawl_wal_durability_failures_total{operation=\"parent_directory_sync\"}";

    /// Every `.ndjson` and `.tmp` name under `dir`, sorted.
    fn wal_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn directory_sync_failure_rejects_the_write_and_withdraws_its_file() {
        use crate::metrics::test_support::sample;
        let recorder = crate::metrics::prometheus_builder().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            crate::metrics::init_operational_alert_metrics();
            let tmp = tempfile::tempdir().unwrap();
            let writer = WalWriter::new(tmp.path().join("wal"));
            let healthy = writer.write("prod", "healthy", b"{\"id\":1}\n").unwrap();
            assert_eq!(sample(&handle, DIR_SYNC_FAILURES), 0);
            // `prod` is already durable, so the injected failure hits the
            // env directory sync that follows the rename.
            writer.fail_next_directory_sync_for_test();
            let err = writer
                .write("prod", "degraded", b"{\"id\":2}\n")
                .unwrap_err();
            assert!(
                matches!(err, WalWriteError::NotPublished(_)),
                "the file was withdrawn: {err:?}"
            );
            assert!(!err.left_visible());
            assert_eq!(sample(&handle, DIR_SYNC_FAILURES), 1);
            assert_eq!(
                wal_names(&writer.dir().join("prod")),
                [healthy.file_name().unwrap().to_str().unwrap()],
                "an unacknowledged write leaves no file for compaction"
            );
            let next = writer.write("prod", "next", b"{\"id\":3}\n").unwrap();
            assert_eq!(sample(&handle, DIR_SYNC_FAILURES), 1);
            assert_eq!(std::fs::read(healthy).unwrap(), b"{\"id\":1}\n");
            assert_eq!(std::fs::read(next).unwrap(), b"{\"id\":3}\n");
            // The writer rejects; the caller's lane owns the event count.
            assert_eq!(
                sample(&handle, crate::metrics::SYSLOG_WAL_EVENTS_DISCARDED_TOTAL),
                0
            );
            assert_eq!(
                sample(
                    &handle,
                    "trawl_ingest_events_rejected_total{reason=\"wal_failure\"}"
                ),
                0
            );
        });
    }

    #[test]
    fn root_sync_failure_rejects_the_first_write_into_a_new_env() {
        use crate::metrics::test_support::sample;
        let recorder = crate::metrics::prometheus_builder().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            crate::metrics::init_operational_alert_metrics();
            let tmp = tempfile::tempdir().unwrap();
            let writer = WalWriter::new(tmp.path().join("wal"));
            writer.ensure_dir().unwrap();
            writer.fail_next_directory_sync_for_test();
            let err = writer.write("lab", "first", b"{\"id\":1}\n").unwrap_err();
            assert!(matches!(err, WalWriteError::NotPublished(_)), "{err:?}");
            assert_eq!(sample(&handle, DIR_SYNC_FAILURES), 1);
            assert!(
                wal_names(&writer.dir().join("lab")).is_empty(),
                "the write stops before creating any file"
            );
            // The env was not remembered as durable, so this write syncs
            // the root again, and it succeeds.
            let next = writer.write("lab", "first", b"{\"id\":2}\n").unwrap();
            assert_eq!(std::fs::read(next).unwrap(), b"{\"id\":2}\n");
            assert_eq!(sample(&handle, DIR_SYNC_FAILURES), 1);
        });
    }

    #[test]
    fn a_removed_env_dir_is_recreated_and_its_entry_synced_again() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = WalWriter::new(tmp.path().join("wal"));
        std::fs::remove_file(writer.write("prod", "first", b"{}\n").unwrap()).unwrap();
        std::fs::remove_dir(writer.dir().join("prod")).unwrap();
        // The recreated directory's entry is new, so the root sync runs
        // again: failing it proves the barrier was not skipped.
        writer.fail_next_directory_sync_for_test();
        let err = writer.write("prod", "second", b"{}\n").unwrap_err();
        assert!(matches!(err, WalWriteError::NotPublished(_)), "{err:?}");
        assert!(wal_names(&writer.dir().join("prod")).is_empty());
        let path = writer.write("prod", "third", b"{\"id\":3}\n").unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"{\"id\":3}\n");
    }

    #[test]
    fn failed_withdrawal_reports_the_file_left_visible() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = WalWriter::new(tmp.path().join("wal"));
        writer.write("prod", "warm", b"{}\n").unwrap();
        writer.fail_next_directory_sync_for_test();
        writer.fail_next_withdraw_for_test();
        let err = writer.write("prod", "stuck", b"{\"id\":1}\n").unwrap_err();
        assert!(err.left_visible(), "{err:?}");
        let WalWriteError::LeftVisible { path, .. } = err else {
            unreachable!()
        };
        assert_eq!(path.extension().unwrap(), "ndjson");
        assert_eq!(std::fs::read(path).unwrap(), b"{\"id\":1}\n");
    }

    #[test]
    fn write_creates_ndjson_file_under_env_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = WalWriter::new(tmp.path().to_path_buf());
        writer.ensure_dir().unwrap();

        let events = b"{\"service\":\"test\",\"message\":\"hello\"}\n";
        let path = writer.write("prod", "test", events).unwrap();

        assert!(path.exists());
        assert!(path.extension().is_some_and(|ext| ext == "ndjson"));
        assert_eq!(std::fs::read(&path).unwrap(), events);
        assert_eq!(
            path.parent().unwrap(),
            tmp.path().join("prod"),
            "WAL files land under wal_dir/{{env}}/"
        );
    }

    #[test]
    fn filename_carries_service_verbatim() {
        // api.v2 and api_v2 must be distinct files: path encoding is
        // injective by validation, with no sanitizer to collapse them
        // (ADR-0009).
        let dotted = WalWriter::generate_filename("api.v2").unwrap();
        let underscored = WalWriter::generate_filename("api_v2").unwrap();
        assert!(dotted.starts_with("api.v2_"), "got {dotted}");
        assert!(underscored.starts_with("api_v2_"), "got {underscored}");
    }

    #[test]
    fn two_envs_same_service_are_separate_files() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = WalWriter::new(tmp.path().to_path_buf());
        writer.ensure_dir().unwrap();

        let prod = writer.write("prod", "svc", b"{}\n").unwrap();
        let lab = writer.write("lab", "svc", b"{}\n").unwrap();
        assert_ne!(prod, lab);
        assert!(prod.starts_with(tmp.path().join("prod")));
        assert!(lab.starts_with(tmp.path().join("lab")));
    }

    #[test]
    fn no_tmp_file_left_on_success() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = WalWriter::new(tmp.path().to_path_buf());
        writer.ensure_dir().unwrap();

        writer.write("prod", "test", b"{}\n").unwrap();

        let tmp_files: Vec<_> = std::fs::read_dir(tmp.path().join("prod"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "tmp"))
            .collect();
        assert!(tmp_files.is_empty(), "no .tmp files should remain");
    }
}
