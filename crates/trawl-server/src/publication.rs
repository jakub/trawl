// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Coordinates canonical Parquet publication with readers in this process.
//! External writers and exactly-once ingestion require separate guarantees.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::{
    OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock, RwLockReadGuard, RwLockWriteGuard,
};

use crate::error::ServerError;

#[derive(Debug, Default)]
struct PendingRollups {
    markers: HashSet<PathBuf>,
    scan_failed: bool,
    initialized: bool,
}

/// Readers hold a guard through source selection, hot snapshot, and execution.
/// Writers hold a guard through publication and hot drain or source retirement.
#[derive(Debug, Default)]
pub struct PublicationGate {
    lock: Arc<RwLock<()>>,
    pending: Mutex<PendingRollups>,
    #[cfg(any(test, feature = "test-support"))]
    pause: Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
    #[cfg(test)]
    panic_next_publication: std::sync::atomic::AtomicBool,
}

impl PublicationGate {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register root/env/date/.rollup-* markers before admitting readers.
    /// Literal directory traversal keeps glob metacharacters in root names
    /// literal. A failed scan refuses reads until this gate is replaced.
    /// Repeated initialization preserves markers already registered by writers.
    pub fn initialize(&self, root: &Path) {
        let mut pending = self.pending.lock();
        if pending.initialized {
            return;
        }
        pending.initialized = true;
        if let Err(error) = scan_markers(root, &mut pending.markers) {
            // This scan is attempted once. Later read refusals are its
            // consequences, not new recovery or scan attempts.
            crate::metrics::CompactionOperation::PendingRollupScan.record_failure();
            tracing::error!(event_type = "publication_scan_failed", %error,
                "cannot establish rollup publication state; corpus reads refused until restart");
            pending.scan_failed = true;
        }
    }

    /// Refuse mixed rollup generations, including incomplete recovery at boot.
    pub async fn read(&self) -> Result<OwnedRwLockReadGuard<()>, ServerError> {
        let guard = Arc::clone(&self.lock).read_owned().await;
        let mut pending = self.pending.lock();
        pending.markers.retain(|path| !is_missing(path));
        if pending.scan_failed || !pending.markers.is_empty() {
            return Err(ServerError::ServiceUnavailable(
                "query data is temporarily unavailable".to_owned(),
            ));
        }
        Ok(guard)
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

    /// The caller must hold the publication write guard until retirement ends.
    pub fn mark_rollup(&self, marker: &Path) {
        self.pending.lock().markers.insert(marker.to_path_buf());
    }

    /// The caller must hold the publication write guard. A surviving marker or
    /// metadata error still refuses readers. This method never changes files.
    pub fn finish_rollup(&self, marker: &Path) {
        if is_missing(marker) {
            self.pending.lock().markers.remove(marker);
        }
    }

    /// Recovery must finish these days before WAL compaction changes any
    /// hourly input named by a marker.
    pub fn pending_rollup_markers(&self) -> Vec<PathBuf> {
        let mut pending = self.pending.lock();
        pending.markers.retain(|path| !is_missing(path));
        pending.markers.iter().cloned().collect()
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

fn scan_markers(root: &Path, markers: &mut HashSet<PathBuf>) -> std::io::Result<()> {
    let envs = match std::fs::read_dir(root) {
        Ok(entries) => entries,
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
        for date in std::fs::read_dir(env.path())? {
            let date = date?;
            if chrono::NaiveDate::parse_from_str(&date.file_name().to_string_lossy(), "%Y-%m-%d")
                .is_err()
            {
                continue;
            }
            if !directory_exists(&date.path())? {
                continue;
            }
            for entry in std::fs::read_dir(date.path())? {
                let entry = entry?;
                if entry.file_name().to_string_lossy().starts_with(".rollup-") {
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
            gate.mark_rollup(&marker);
            gate.finish_rollup(&marker);
        }
        assert!(matches!(
            gate.read().await,
            Err(ServerError::ServiceUnavailable(_))
        ));
        std::fs::remove_file(marker).unwrap();
        assert!(gate.read().await.is_ok());
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

    #[tokio::test]
    async fn scan_failure_stays_closed() {
        use crate::metrics::test_support::sample;
        let recorder = crate::metrics::prometheus_builder().build_recorder();
        let handle = recorder.handle();
        // initialize and read perform their checks on this thread; neither
        // dispatches a worker whose counters this recorder could miss.
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
        assert_eq!(
            sample(&handle, series),
            1,
            "a latched refusal is not a new scan"
        );
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
            "errors other than confirmed absence remain closed until restart"
        );
    }
}
