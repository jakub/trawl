// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Write-ahead log for crash-safe event ingestion.
//!
//! Ingest writes one WAL file per `(env, service)` batch; compaction later
//! converts those files to parquet. [`WalWriter::write`] carries the
//! durability sequence and the reason for each step.

use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use parking_lot::Mutex;

use super::publication_marker;

/// How many names one write tries before it gives up. A name is taken
/// when an entry already holds it or a pending publication marker claims
/// it. Every fresh name carries a new random nonce, so a second taken name
/// is already unlikely; the bound keeps a write from looping.
const MAX_NAME_ATTEMPTS: usize = 8;

/// A WAL write that was not acknowledged.
///
/// The variant tells the caller whether the batch's bytes may still reach
/// compaction, which decides whether writing them again is safe.
#[derive(Debug, thiserror::Error)]
pub enum WalWriteError {
    /// No file from this write remains under a `.ndjson` name, and none can
    /// come back. The write failed before its final name was linked, or its
    /// directory fsync failed and the name was withdrawn by an unlink whose
    /// own directory fsync succeeded. Writing the same events again cannot
    /// duplicate them.
    #[error("{0}")]
    NotPublished(#[source] std::io::Error),
    /// The directory fsync failed after the final name was linked, and the
    /// name could not be withdrawn durably: the unlink failed, or the
    /// directory fsync after it did. The file stays under its final name,
    /// where compaction will merge it, or a power loss may bring the name
    /// back for compaction or the next boot to find. Writing the same events
    /// again could duplicate them.
    #[error(
        "{sync}; withdrawing {} failed or is not durable ({withdraw}), so compaction may still merge it",
        path.display()
    )]
    LeftVisible {
        path: PathBuf,
        #[source]
        sync: std::io::Error,
        /// The unlink's error, or that of the directory fsync after it.
        withdraw: std::io::Error,
    },
}

impl WalWriteError {
    /// Whether the unacknowledged file may still reach compaction: it is
    /// visible now, or a power loss may restore it.
    pub fn left_visible(&self) -> bool {
        matches!(self, Self::LeftVisible { .. })
    }
}

impl From<std::io::Error> for WalWriteError {
    fn from(error: std::io::Error) -> Self {
        Self::NotPublished(error)
    }
}

/// A WAL file name as [`WalWriter`] produces it:
/// `{service}_{unix_millis}_{4 lowercase hex}.ndjson`.
///
/// The service is carried verbatim. It was validated at ingest (ADR-0009),
/// so a service holding `_` stays unambiguous: the millis and the nonce
/// are the last two `_`-separated fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalName {
    pub service: String,
    /// Milliseconds since the Unix epoch when the writer named the file.
    pub millis: u64,
    /// Random, so writes within one millisecond get distinct names.
    pub nonce: u16,
}

impl WalName {
    /// Parse a file name the writer could have produced, and nothing else:
    /// a valid service name, the millis in decimal without a sign or
    /// leading zeros, exactly four lowercase hex digits, and `.ndjson`.
    pub fn parse(file_name: &str) -> Option<Self> {
        let stem = file_name.strip_suffix(".ndjson")?;
        let (rest, nonce) = stem.rsplit_once('_')?;
        let (service, millis) = rest.rsplit_once('_')?;
        let name = Self {
            service: service.to_owned(),
            millis: millis.parse().ok()?,
            nonce: u16::from_str_radix(nonce, 16).ok()?,
        };
        // Integer parsing also accepts a sign, leading zeros and uppercase
        // hex, none of which the writer produces.
        (trawl_config::is_valid_service_name(&name.service) && name.file_name() == file_name)
            .then_some(name)
    }

    /// `{service}_{millis}_{nonce:04x}.ndjson`.
    pub fn file_name(&self) -> String {
        format!("{}.ndjson", self.stem())
    }

    fn stem(&self) -> String {
        format!("{}_{}_{:04x}", self.service, self.millis, self.nonce)
    }
}

/// Append `map` to `out` as one WAL line: its compact JSON, then `\n`.
///
/// This is the WAL's one line format. Every writer of WAL lines encodes
/// through here, because hydration (ADR-0041) accepts a line only if it
/// encodes back to the same bytes. On error `out` may hold part of the
/// line, and the caller truncates it.
pub(crate) fn encode_line(
    map: &serde_json::Map<String, serde_json::Value>,
    out: &mut Vec<u8>,
) -> serde_json::Result<()> {
    serde_json::to_writer(&mut *out, map)?;
    out.push(b'\n');
    Ok(())
}

/// A directory's `(device, inode)`, which tells a recreated directory apart
/// from the one whose entry was synced.
type DirIdentity = (u64, u64);

/// The process-wide order in which WAL files were acknowledged, for tests
/// that check write order. Filename millis tie within a millisecond, and
/// tmpfs mtimes tie on its coarse clock.
#[cfg(test)]
static ACK_ORDER: std::sync::LazyLock<Mutex<HashMap<PathBuf, u64>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// The position of `path` in the process-wide acknowledgement order, or
/// `None` if no [`WalWriter`] acknowledged it.
#[cfg(test)]
pub(crate) fn ack_sequence_for_test(path: &Path) -> Option<u64> {
    ACK_ORDER.lock().get(path).copied()
}

/// Atomic WAL file writer for ingest events.
#[derive(Debug)]
pub struct WalWriter {
    wal_dir: PathBuf,
    /// Environment directories whose entry in `wal_dir` this process has
    /// made durable, keyed by env and holding that directory's identity.
    /// An env is recorded only after the root fsync succeeds, so racing
    /// first writers both sync. A writer that creates the directory clears
    /// the record under this lock before anyone else can see the new
    /// directory, and a writer that finds a directory whose identity is not
    /// the recorded one syncs the root itself.
    durable_envs: Mutex<HashMap<String, DirIdentity>>,
    #[cfg(any(test, feature = "test-support"))]
    failing_directory_syncs: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    fail_next_withdraw: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    fail_next_tmp_unlink: std::sync::atomic::AtomicBool,
    /// Names the next writes take, in order, before generated ones.
    #[cfg(test)]
    forced_names: Mutex<std::collections::VecDeque<WalName>>,
    /// Every directory this writer tried to fsync, in call order.
    #[cfg(test)]
    synced_dirs: Mutex<Vec<PathBuf>>,
    /// Parks the next write that creates an env directory, after the
    /// creation and before its root sync: the write waits on the barrier
    /// once to say it is parked and once more to resume.
    #[cfg(test)]
    pause_after_create: Mutex<Option<std::sync::Arc<std::sync::Barrier>>>,
    #[cfg(test)]
    panic_after_writes: std::sync::atomic::AtomicUsize,
}

impl WalWriter {
    pub fn new(wal_dir: PathBuf) -> Self {
        Self {
            wal_dir,
            durable_envs: Mutex::new(HashMap::new()),
            #[cfg(any(test, feature = "test-support"))]
            failing_directory_syncs: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            fail_next_withdraw: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            fail_next_tmp_unlink: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            forced_names: Mutex::new(std::collections::VecDeque::new()),
            #[cfg(test)]
            synced_dirs: Mutex::new(Vec::new()),
            #[cfg(test)]
            pause_after_create: Mutex::new(None),
            #[cfg(test)]
            panic_after_writes: std::sync::atomic::AtomicUsize::new(usize::MAX),
        }
    }

    /// Create `wal_dir` if it is missing, then fsync every ancestor of it
    /// on its mount, once per boot, whether or not this process
    /// created it ([`crate::epoch::sync_ancestor_chain`]). An `Err` means
    /// the root may not survive a power loss, so nothing may be
    /// acknowledged into it.
    pub fn ensure_dir(&self) -> std::io::Result<()> {
        self.create_dir_all_durably(&self.wal_dir)?;
        crate::epoch::sync_ancestor_chain(&self.wal_dir, crate::epoch::mount_root, |dir| {
            self.sync_or_count(dir)
        })
    }

    pub fn dir(&self) -> &Path {
        &self.wal_dir
    }

    /// Write events durably under a name no other file holds: stage them in
    /// a new `.tmp`, fsync its data, link it under a free `.ndjson` name,
    /// unlink the `.tmp`, then fsync the parent directory so the link
    /// survives a crash. `Ok` is the acknowledgement: every step,
    /// directory fsyncs included, has succeeded.
    ///
    /// The fsync *before* the link prevents a torn write: without it the
    /// link can be journaled before the data blocks reach the device, so a
    /// hard kill / power loss leaves a full-length `.ndjson` of NUL bytes
    /// that head-of-line-blocks compaction.
    ///
    /// The writer never replaces an existing file (ADR-0041). The `.tmp` is
    /// created exclusively, and the final name is published with a hard
    /// link, which fails on an existing entry where a rename would replace
    /// it. A taken name, or one that the pending publication marker of this
    /// `(env, service)` lists, gets a fresh name ([`Self::link_free_name`]).
    /// A marker of this `(env, service)` that is invalid or cannot be read
    /// refuses the write as [`WalWriteError::NotPublished`], so the sender
    /// retries once recovery or an operator resolves it.
    ///
    /// The parent-directory fsync makes the link durable. Without it, a
    /// power loss can drop an acknowledged batch. When it fails, the file
    /// is withdrawn (unlinked) and the unlink synced before the error
    /// returns, so a caller that retries cannot duplicate the batch. If the
    /// withdrawal or its sync fails too, [`WalWriteError::LeftVisible`] says
    /// so. A `.tmp` that cannot be unlinked after the link is logged and
    /// left behind: only `.ndjson` names are WAL files, so it changes no
    /// outcome.
    ///
    /// The first write into an environment directory in this process,
    /// including one recreated after removal, also fsyncs `wal_dir`, which
    /// holds the env directory's own entry, and fails before writing
    /// anything if that sync fails. A write that finds `wal_dir` itself
    /// gone recreates it through the same barrier as [`Self::ensure_dir`].
    ///
    /// Files land in `wal_dir/{env}/` (lazily created), named
    /// `{service}_{unix_millis}_{4_hex}.ndjson` ([`WalName`]) with the
    /// service name verbatim — path encoding is injective by validation
    /// (ADR-0009): both `env` and `service` were validated at ingest, so
    /// `api.v2` and `api_v2` are distinct files and pruning stays exact.
    pub fn write(&self, env: &str, service: &str, events: &[u8]) -> Result<PathBuf, WalWriteError> {
        let env_dir = self.wal_dir.join(env);
        self.ensure_env_dir(env, &env_dir)?;
        let (name, tmp_path) = self.stage(&env_dir, service, events)?;
        let final_path = match self.link_free_name(&env_dir, name, &tmp_path) {
            Ok(path) => path,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp_path);
                return Err(e.into());
            }
        };

        // The final name holds the batch now, and the directory sync below
        // covers this unlink too.
        if let Err(e) = self.unlink_tmp(&tmp_path) {
            tracing::warn!(
                event_type = "wal_tmp_unlink_failed",
                path = %tmp_path.display(),
                error = %e,
                "WAL staging file could not be removed after its link; it stays \
                 behind, and no WAL scan reads it"
            );
        }

        // fsync the directory entry so the link is durable, not just the
        // file's data. The link has already made the file visible to
        // compaction, so a failed sync withdraws it before rejecting the
        // write: an unacknowledged batch must not be merged, or the
        // sender's retry would duplicate it. The unlink is synced in turn;
        // until that sync succeeds, a power loss can bring the name back,
        // so only a durable withdrawal is `NotPublished`.
        if let Err(sync) = self.sync_directory(&env_dir) {
            Self::count_directory_sync_failure();
            let withdrawn = self.withdraw(&final_path);
            let withdrawal_sync = withdrawn
                .as_ref()
                .ok()
                .map(|()| self.sync_directory(&env_dir));
            if let Some(Err(_)) = withdrawal_sync {
                Self::count_directory_sync_failure();
            }
            // `withdrawn = false` means compaction can still merge the file.
            // `withdrawal_durable = false` means a power loss may restore it.
            tracing::warn!(
                event_type = "wal_dir_fsync_failed",
                dir = %env_dir.display(),
                error = %sync,
                withdrawn = withdrawn.is_ok(),
                withdraw_error = withdrawn.as_ref().err().map(tracing::field::display),
                withdrawal_durable = matches!(withdrawal_sync, Some(Ok(()))),
                withdrawal_sync_error = withdrawal_sync
                    .as_ref()
                    .and_then(|r| r.as_ref().err())
                    .map(tracing::field::display),
                "WAL directory fsync failed; the write is rejected"
            );
            // Without a withdrawal sync, the withdrawal itself failed.
            let withdrawal = withdrawn.and(withdrawal_sync.unwrap_or(Ok(())));
            return Err(match withdrawal {
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
            let mut order = ACK_ORDER.lock();
            let sequence = order.len() as u64;
            order.insert(final_path.clone(), sequence);
            drop(order);
            let previous = self.panic_after_writes.fetch_update(
                Ordering::Relaxed,
                Ordering::Relaxed,
                |remaining| (remaining != usize::MAX).then(|| remaining.saturating_sub(1)),
            );
            assert_ne!(previous, Ok(1), "injected panic after durable WAL write");
        }
        Ok(final_path)
    }

    /// Stage `events` in a new `.tmp` in `env_dir` and fsync them. The file
    /// is created exclusively, so another writer's staging file, or one a
    /// crash left behind, is never truncated: a taken `.tmp` name gets a
    /// fresh name, up to [`MAX_NAME_ATTEMPTS`] names. A staging file whose
    /// write or fsync fails is removed.
    fn stage(
        &self,
        env_dir: &Path,
        service: &str,
        events: &[u8],
    ) -> std::io::Result<(WalName, PathBuf)> {
        let mut attempt = 1;
        let (name, tmp_path, mut file) = loop {
            let name = self.next_name(service)?;
            let tmp_path = env_dir.join(format!("{}.tmp", name.stem()));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp_path)
            {
                Ok(file) => break (name, tmp_path, file),
                Err(e)
                    if e.kind() == std::io::ErrorKind::AlreadyExists
                        && attempt < MAX_NAME_ATTEMPTS =>
                {
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        };
        if let Err(e) = file.write_all(events).and_then(|()| file.sync_all()) {
            drop(file);
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e);
        }
        Ok((name, tmp_path))
    }

    /// Link the staged file at `tmp_path` under a free final name, trying
    /// the staging file's own name first. A name is taken when an entry
    /// already holds it, which the link detects without replacing anything,
    /// or when the pending publication marker of its service lists it
    /// ([`claimed_by_marker`]). A taken name gets a fresh one. After
    /// [`MAX_NAME_ATTEMPTS`] taken names the write fails with
    /// `AlreadyExists`, having published nothing. A marker whose claims
    /// cannot be established fails the write before any link.
    fn link_free_name(
        &self,
        env_dir: &Path,
        mut name: WalName,
        tmp_path: &Path,
    ) -> std::io::Result<PathBuf> {
        for attempt in 1..=MAX_NAME_ATTEMPTS {
            if attempt > 1 {
                name = self.next_name(&name.service)?;
            }
            let file_name = name.file_name();
            if claimed_by_marker(env_dir, &name.service, &file_name)? {
                continue;
            }
            let final_path = env_dir.join(file_name);
            match std::fs::hard_link(tmp_path, &final_path) {
                Ok(()) => return Ok(final_path),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("no free WAL file name in {MAX_NAME_ATTEMPTS} attempts"),
        ))
    }

    /// The next name to try for `service`: a forced one in tests, otherwise
    /// a fresh [`Self::generate_filename`].
    #[cfg_attr(not(test), allow(clippy::unused_self))]
    fn next_name(&self, service: &str) -> std::io::Result<WalName> {
        #[cfg(test)]
        if let Some(name) = self.forced_names.lock().pop_front() {
            return Ok(name);
        }
        Self::generate_filename(service)
    }

    /// Create `env_dir` if it is missing, and make its entry in `wal_dir`
    /// durable unless this process already synced this same directory. A
    /// file acknowledged into a directory whose own entry is lost on power
    /// failure is lost with it.
    ///
    /// Creation and the durability check share one lock, so a writer that
    /// finds a directory another writer just created, before that writer
    /// has synced the root, never sees the old directory's record. The
    /// identity check covers a directory removed and recreated outside this
    /// writer, unless the new directory reuses the old inode number.
    fn ensure_env_dir(&self, env: &str, env_dir: &Path) -> std::io::Result<()> {
        let identity = {
            let mut durable = self.durable_envs.lock();
            let created = match std::fs::create_dir(env_dir) {
                Ok(()) => true,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
                // `wal_dir` itself is gone, so its entry and those of any
                // ancestors created with it need syncing too.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    self.create_dir_all_durably(env_dir)?;
                    true
                }
                Err(e) => return Err(e),
            };
            if created {
                durable.remove(env);
            }
            let identity = Self::dir_identity(env_dir)?;
            if durable.get(env) == Some(&identity) {
                return Ok(());
            }
            #[cfg(test)]
            if created {
                let pause = self.pause_after_create.lock().take();
                if let Some(barrier) = pause {
                    drop(durable);
                    barrier.wait();
                    barrier.wait();
                }
            }
            identity
        };
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
        self.durable_envs.lock().insert(env.to_owned(), identity);
        Ok(())
    }

    /// [`crate::epoch::create_dir_all_durably`] through this writer's
    /// directory barrier.
    fn create_dir_all_durably(&self, dir: &Path) -> std::io::Result<()> {
        crate::epoch::create_dir_all_durably(dir, |parent| self.sync_or_count(parent))
    }

    /// Sync a directory that holds the WAL root or one of its ancestors,
    /// counting and logging a failure.
    fn sync_or_count(&self, dir: &Path) -> std::io::Result<()> {
        self.sync_directory(dir).inspect_err(|e| {
            Self::count_directory_sync_failure();
            tracing::warn!(
                event_type = "wal_dir_fsync_failed",
                dir = %dir.display(),
                error = %e,
                "WAL directory fsync failed on the path to the WAL root; \
                 nothing is acknowledged into the directories below it"
            );
        })
    }

    fn dir_identity(dir: &Path) -> std::io::Result<DirIdentity> {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(dir)?;
        Ok((metadata.dev(), metadata.ino()))
    }

    fn count_directory_sync_failure() {
        metrics::counter!(crate::metrics::WAL_DURABILITY_FAILURES_TOTAL,
            "operation" => crate::metrics::WalDurabilityOperation::ParentDirectorySync.label())
        .increment(1);
    }

    #[cfg_attr(not(any(test, feature = "test-support")), allow(clippy::unused_self))]
    fn sync_directory(&self, dir: &Path) -> std::io::Result<()> {
        #[cfg(test)]
        self.synced_dirs.lock().push(dir.to_path_buf());
        #[cfg(any(test, feature = "test-support"))]
        if self
            .failing_directory_syncs
            .fetch_update(
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
                |remaining| remaining.checked_sub(1),
            )
            .is_ok()
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

    #[cfg_attr(not(test), allow(clippy::unused_self))]
    fn unlink_tmp(&self, path: &Path) -> std::io::Result<()> {
        #[cfg(test)]
        if self
            .fail_next_tmp_unlink
            .swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            return Err(std::io::Error::other(
                "injected WAL staging file unlink failure",
            ));
        }
        std::fs::remove_file(path)
    }

    /// Fail this writer's next directory barrier: the parent sync after
    /// creating `wal_dir` or an ancestor, a boot sync of an ancestor of
    /// `wal_dir`, the `wal_dir` sync of a first
    /// write into an env, or the env directory sync after a link.
    #[cfg(any(test, feature = "test-support"))]
    pub fn fail_next_directory_sync_for_test(&self) {
        self.failing_directory_syncs
            .store(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Fail this writer's next `count` directory barriers.
    #[cfg(test)]
    pub(crate) fn fail_next_directory_syncs_for_test(&self, count: usize) {
        self.failing_directory_syncs
            .store(count, std::sync::atomic::Ordering::Relaxed);
    }

    /// Drain the directories this writer tried to fsync, in call order.
    #[cfg(test)]
    pub(crate) fn take_synced_dirs_for_test(&self) -> Vec<PathBuf> {
        std::mem::take(&mut *self.synced_dirs.lock())
    }

    /// Fail the next withdrawal of a file whose directory sync failed,
    /// leaving that file visible under its final name.
    #[cfg(test)]
    pub(crate) fn fail_next_withdraw_for_test(&self) {
        self.fail_next_withdraw
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Fail the next unlink of a staging file after its link, leaving the
    /// `.tmp` behind.
    #[cfg(test)]
    pub(crate) fn fail_next_tmp_unlink_for_test(&self) {
        self.fail_next_tmp_unlink
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Make the next names this writer tries `names`, in order, before any
    /// generated name. A write takes one for its staging file, whose name
    /// is also its first final name, and one per fresh name after that.
    #[cfg(test)]
    pub(crate) fn force_names_for_test(&self, names: impl IntoIterator<Item = WalName>) {
        self.forced_names.lock().extend(names);
    }

    /// Panic after this many completed durable writes on this writer only.
    #[cfg(test)]
    pub(crate) fn panic_after_writes_for_test(&self, count: usize) {
        assert!(count > 0);
        self.panic_after_writes
            .store(count, std::sync::atomic::Ordering::Relaxed);
    }

    /// Generate a fresh name: `{service}_{unix_millis}_{4_hex_random}`.
    ///
    /// The service name is carried verbatim — it was validated at ingest.
    fn generate_filename(service: &str) -> std::io::Result<WalName> {
        let millis = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(std::io::Error::other)?
            .as_millis();

        Ok(WalName {
            service: service.to_owned(),
            millis: u64::try_from(millis).map_err(std::io::Error::other)?,
            // 4 hex chars of randomness to avoid collisions within the same ms.
            nonce: rand::random(),
        })
    }
}

/// Whether the pending publication marker of `service` in `env_dir` lists
/// `file_name` among its consumed WAL files. Recovery retires every file a
/// marker lists, so a new file under such a name would be retired as if it
/// had been merged. A missing marker claims nothing.
///
/// A marker that exists but whose claims cannot be established is an
/// error, which refuses the write: one that is invalid, over the size
/// bound, or that cannot be read. It may claim any name, and it may yet be
/// repaired and recovered, so no name is safe to take until it resolves.
/// The error names the service, not the path: it can reach the sender.
fn claimed_by_marker(env_dir: &Path, service: &str, file_name: &str) -> std::io::Result<bool> {
    let marker = env_dir.join(publication_marker::marker_file_name(service));
    let why = match publication_marker::read_marker(&marker) {
        Ok(marker) => return Ok(marker.wal_names().iter().any(|name| name == file_name)),
        Err(publication_marker::MarkerError::Missing) => return Ok(false),
        Err(publication_marker::MarkerError::Invalid(_)) => "is invalid",
        Err(publication_marker::MarkerError::Io(_)) => "cannot be read",
    };
    Err(std::io::Error::other(format!(
        "the pending publication marker of service {service:?} {why}, so the names it \
         claims are unknown; writes for the service are refused until the marker is resolved"
    )))
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
            // env directory sync that follows the link.
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

    /// Every ancestor of `dir`'s canonical path on `dir`'s mount, from its
    /// parent up to the mount root. Where that root lies depends on the
    /// host's mounts.
    fn ancestor_chain(dir: &Path) -> Vec<PathBuf> {
        let dir = std::fs::canonicalize(dir).unwrap();
        let mut chain = Vec::new();
        let mut child = dir.as_path();
        while let Some(parent) = child.parent() {
            if crate::epoch::mount_root(child).unwrap() == Some(true) {
                break;
            }
            chain.push(parent.to_path_buf());
            child = parent;
        }
        chain
    }

    #[test]
    fn a_new_wal_root_is_durable_in_its_parent_before_the_first_ack() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("state").join("data").join("wal");
        let writer = WalWriter::new(root.clone());
        writer.ensure_dir().unwrap();
        let synced = writer.take_synced_dirs_for_test();
        assert_eq!(
            synced[..3],
            [
                tmp.path().to_path_buf(),
                tmp.path().join("state"),
                tmp.path().join("state").join("data"),
            ],
            "each created directory's entry is synced, from the first \
             ancestor that already existed: {synced:?}"
        );
        assert_eq!(synced[3..], ancestor_chain(&root));
        writer.write("prod", "first", b"{}\n").unwrap();
        assert_eq!(
            writer.take_synced_dirs_for_test(),
            [root.clone(), root.join("prod")]
        );
    }

    #[test]
    fn boot_syncs_the_ancestor_chain_of_a_wal_root_it_did_not_create() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("state").join("wal");
        // A process killed after creating the root, before syncing its
        // parent, leaves it present with no sync recorded anywhere.
        std::fs::create_dir_all(&root).unwrap();
        let writer = WalWriter::new(root.clone());
        writer.ensure_dir().unwrap();
        let chain = ancestor_chain(&root);
        let tmp_dir = std::fs::canonicalize(tmp.path()).unwrap();
        assert_eq!(chain[..2], [tmp_dir.join("state"), tmp_dir]);
        assert_eq!(writer.take_synced_dirs_for_test(), chain);

        // A failed ancestor sync fails `ensure_dir`, which fails boot.
        writer.fail_next_directory_sync_for_test();
        writer.ensure_dir().unwrap_err();
    }

    #[test]
    fn a_failed_root_barrier_fails_ensure_dir_and_leaves_the_root_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = WalWriter::new(tmp.path().join("state").join("wal"));
        writer.fail_next_directory_sync_for_test();
        writer.ensure_dir().unwrap_err();
        assert!(
            !tmp.path().join("state").exists(),
            "the created directories are removed, so a retry syncs them again"
        );
        writer.take_synced_dirs_for_test();
        writer.ensure_dir().unwrap();
        assert_eq!(
            writer.take_synced_dirs_for_test()[..2],
            [tmp.path().to_path_buf(), tmp.path().join("state")]
        );
    }

    #[test]
    fn a_write_that_recreates_the_wal_root_acks_only_after_its_parent_sync() {
        use crate::metrics::test_support::sample;
        let recorder = crate::metrics::prometheus_builder().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            crate::metrics::init_operational_alert_metrics();
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("state").join("wal");
            // No `ensure_dir`: the write finds the root missing, as after
            // its removal at runtime.
            let writer = WalWriter::new(root.clone());
            writer.fail_next_directory_sync_for_test();
            let err = writer.write("prod", "first", b"{\"id\":1}\n").unwrap_err();
            assert!(matches!(err, WalWriteError::NotPublished(_)), "{err:?}");
            assert_eq!(sample(&handle, DIR_SYNC_FAILURES), 1);
            assert!(!tmp.path().join("state").exists());
            writer.take_synced_dirs_for_test();

            let path = writer.write("prod", "first", b"{\"id\":2}\n").unwrap();
            assert_eq!(std::fs::read(path).unwrap(), b"{\"id\":2}\n");
            let synced = writer.take_synced_dirs_for_test();
            assert_eq!(
                synced[..2],
                [tmp.path().to_path_buf(), tmp.path().join("state")],
                "the retry syncs the new root's parent chain before acking: {synced:?}"
            );
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
    fn a_writer_that_finds_a_recreated_env_dir_syncs_its_entry_before_acking() {
        use std::sync::{Arc, Barrier};
        let tmp = tempfile::tempdir().unwrap();
        let writer = Arc::new(WalWriter::new(tmp.path().join("wal")));
        // `prod` is recorded as durable, then its directory goes away.
        std::fs::remove_file(writer.write("prod", "first", b"{}\n").unwrap()).unwrap();
        std::fs::remove_dir(writer.dir().join("prod")).unwrap();
        writer.take_synced_dirs_for_test();

        // Writer A recreates `prod` and parks before its root sync.
        let barrier = Arc::new(Barrier::new(2));
        *writer.pause_after_create.lock() = Some(Arc::clone(&barrier));
        let a = std::thread::spawn({
            let writer = Arc::clone(&writer);
            move || writer.write("prod", "a", b"{\"id\":1}\n")
        });
        barrier.wait();
        assert_eq!(writer.take_synced_dirs_for_test(), Vec::<PathBuf>::new());

        // Writer B finds the new directory already there. Its entry in
        // the root is not durable yet, so B must sync the root itself.
        let b = writer.write("prod", "b", b"{\"id\":2}\n").unwrap();
        assert_eq!(
            writer.take_synced_dirs_for_test(),
            [writer.dir().to_path_buf(), writer.dir().join("prod")],
            "B acknowledged only after the root and env directory syncs"
        );
        barrier.wait();
        let a = a.join().unwrap().unwrap();
        assert_eq!(std::fs::read(a).unwrap(), b"{\"id\":1}\n");
        assert_eq!(std::fs::read(b).unwrap(), b"{\"id\":2}\n");
    }

    #[test]
    fn a_withdrawal_is_made_durable_before_the_rejection_returns() {
        use crate::metrics::test_support::sample;
        let recorder = crate::metrics::prometheus_builder().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            crate::metrics::init_operational_alert_metrics();
            let tmp = tempfile::tempdir().unwrap();
            let writer = WalWriter::new(tmp.path().join("wal"));
            writer.write("prod", "warm", b"{}\n").unwrap();
            let env_dir = writer.dir().join("prod");
            writer.take_synced_dirs_for_test();

            writer.fail_next_directory_sync_for_test();
            let err = writer.write("prod", "lost", b"{\"id\":1}\n").unwrap_err();
            assert!(matches!(err, WalWriteError::NotPublished(_)), "{err:?}");
            assert_eq!(
                writer.take_synced_dirs_for_test(),
                [env_dir.clone(), env_dir.clone()],
                "the unlink is synced after the failed link sync"
            );
            assert_eq!(sample(&handle, DIR_SYNC_FAILURES), 1);

            // Both syncs fail: the name is gone now, but a power loss may
            // bring it back for compaction or hydration to find. The write
            // is rejected as `LeftVisible`, not `NotPublished`, because a
            // retry could then duplicate it. Each failed sync is counted.
            writer.fail_next_directory_syncs_for_test(2);
            let err = writer.write("prod", "lost", b"{\"id\":2}\n").unwrap_err();
            assert!(err.left_visible(), "{err:?}");
            assert_eq!(
                writer.take_synced_dirs_for_test(),
                [env_dir.clone(), env_dir.clone()]
            );
            assert_eq!(sample(&handle, DIR_SYNC_FAILURES), 3);
            assert_eq!(wal_names(&env_dir).len(), 1, "only the warm file remains");
        });
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
        let dotted = WalWriter::generate_filename("api.v2").unwrap().file_name();
        let underscored = WalWriter::generate_filename("api_v2").unwrap().file_name();
        assert!(dotted.starts_with("api.v2_"), "got {dotted}");
        assert!(underscored.starts_with("api_v2_"), "got {underscored}");
    }

    #[test]
    fn wal_name_round_trips_through_the_file_name() {
        for service in ["api", "api_v2", "api.v2", "a_1_b", "x-y"] {
            let generated = WalWriter::generate_filename(service).unwrap();
            assert_eq!(generated.service, service);
            let file_name = generated.file_name();
            assert_eq!(WalName::parse(&file_name), Some(generated), "{file_name}");
        }
        let name = WalName {
            service: "a_1_b".to_owned(),
            millis: 1_700_000_000_123,
            nonce: 0x0a0f,
        };
        assert_eq!(name.file_name(), "a_1_b_1700000000123_0a0f.ndjson");
        assert_eq!(
            WalName::parse("a_1_b_1700000000123_0a0f.ndjson"),
            Some(name)
        );
        assert_eq!(
            WalName::parse("svc_0_0000.ndjson"),
            Some(WalName {
                service: "svc".to_owned(),
                millis: 0,
                nonce: 0,
            })
        );
    }

    #[test]
    fn wal_name_parses_only_names_the_writer_produces() {
        for rejected in [
            "svc_1700000000123_0a0f",
            "svc_1700000000123_0a0f.tmp",
            "svc_1700000000123_0a0f.ndjson.merged",
            "svc_1700000000123_0A0F.ndjson",
            "svc_1700000000123_a0f.ndjson",
            "svc_1700000000123_00a0f.ndjson",
            "svc_1700000000123_+a0f.ndjson",
            "svc_01700000000123_0a0f.ndjson",
            "svc_+1700000000123_0a0f.ndjson",
            "svc__0a0f.ndjson",
            "svc_18446744073709551616_0a0f.ndjson",
            "_1700000000123_0a0f.ndjson",
            ".svc_1700000000123_0a0f.ndjson",
            "s v_1700000000123_0a0f.ndjson",
            "1700000000123_0a0f.ndjson",
            ".publish-svc.json",
            "",
        ] {
            assert_eq!(WalName::parse(rejected), None, "{rejected:?}");
        }
    }

    #[test]
    fn encode_line_is_the_writer_line_format() {
        let map = serde_json::json!({
            "b_false": false,
            "b_true": true,
            "f_exp": 1e300,
            "f_neg_zero": -0.0,
            "f_max": f64::MAX,
            "f_min_positive": 5e-324,
            "f_tenth": 0.1,
            "f_long": 123_456_789.123_456_79,
            "i_max": u64::MAX,
            "i_min": i64::MIN,
            "i_zero": 0,
            "null": null,
            "s_escaped": "quote \" backslash \\ newline \n tab \t nul \u{0} bell \u{7}",
            "s_unicode": "zażółć 🦀 \u{2028}",
        });
        let map = map.as_object().unwrap();
        let mut expected = serde_json::to_vec(map).unwrap();
        expected.push(b'\n');

        let mut line = Vec::new();
        encode_line(map, &mut line).unwrap();
        assert_eq!(line, expected);

        // It appends, so consecutive lines concatenate into ndjson.
        let mut two = b"prefix\n".to_vec();
        encode_line(map, &mut two).unwrap();
        encode_line(map, &mut two).unwrap();
        assert_eq!(two, [b"prefix\n".as_slice(), &expected, &expected].concat());
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

/// The writer never replaces an existing name and never answers a failure
/// after its final name exists with a retryable [`WalWriteError::NotPublished`]
/// while that name may still reach compaction (ADR-0041).
#[cfg(test)]
mod no_clobber {
    use super::*;

    const DIR_SYNC_FAILURES: &str =
        "trawl_wal_durability_failures_total{operation=\"parent_directory_sync\"}";

    fn forced(service: &str, millis: u64, nonce: u16) -> WalName {
        WalName {
            service: service.to_owned(),
            millis,
            nonce,
        }
    }

    /// A writer whose `prod` env directory is already durable, so the next
    /// injected directory-sync failure hits the sync after the link.
    fn warmed(root: &Path) -> WalWriter {
        let writer = WalWriter::new(root.join("wal"));
        std::fs::remove_file(writer.write("prod", "warm", b"{}\n").unwrap()).unwrap();
        writer.take_synced_dirs_for_test();
        writer
    }

    /// Every entry name in `dir`, sorted.
    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// Run `f` with a thread-local subscriber and return what it logged.
    fn logged<T>(f: impl FnOnce() -> T) -> (T, String) {
        #[derive(Clone)]
        struct Sink(std::sync::Arc<Mutex<Vec<u8>>>);
        impl Write for Sink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let logs = std::sync::Arc::new(Mutex::new(Vec::new()));
        let sink = Sink(std::sync::Arc::clone(&logs));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || sink.clone())
            .finish();
        let value = tracing::subscriber::with_default(subscriber, f);
        let text = String::from_utf8_lossy(&logs.lock()).into_owned();
        (value, text)
    }

    #[test]
    fn a_forced_collision_keeps_the_existing_file_and_takes_a_fresh_name() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = warmed(tmp.path());
        let env_dir = writer.dir().join("prod");
        let taken = forced("svc", 1_000, 0xabcd);
        let existing = env_dir.join(taken.file_name());
        std::fs::write(&existing, b"{\"id\":\"existing\"}\n").unwrap();

        writer.force_names_for_test([taken.clone()]);
        let path = writer.write("prod", "svc", b"{\"id\":\"new\"}\n").unwrap();

        assert_ne!(path, existing, "the write took a fresh name");
        let fresh = WalName::parse(path.file_name().unwrap().to_str().unwrap()).unwrap();
        assert_eq!(fresh.service, "svc");
        assert_ne!(fresh, taken);
        assert_eq!(
            std::fs::read(&existing).unwrap(),
            b"{\"id\":\"existing\"}\n",
            "the existing file is byte-identical"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"id\":\"new\"}\n");
        assert_eq!(
            entries(&env_dir),
            [taken.file_name(), fresh.file_name()],
            "no staging file is left"
        );
    }

    #[test]
    fn an_existing_tmp_is_never_truncated() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = warmed(tmp.path());
        let env_dir = writer.dir().join("prod");
        // Another writer's staging file, or one a crash left behind.
        let staging = env_dir.join("svc_1000_abcd.tmp");
        std::fs::write(&staging, b"staged by someone else").unwrap();

        writer.force_names_for_test([forced("svc", 1_000, 0xabcd)]);
        let path = writer.write("prod", "svc", b"{\"id\":1}\n").unwrap();

        assert_ne!(path.file_name().unwrap(), "svc_1000_abcd.ndjson");
        assert_eq!(std::fs::read(&staging).unwrap(), b"staged by someone else");
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"id\":1}\n");
    }

    fn pending_marker(wal_dir: &Path, service: &str, consumed: &[&str]) {
        use crate::ingest::publication_marker::{OutputIdentity, ValidatedMarker, write_marker};
        let marker = ValidatedMarker::new(
            "prod",
            service,
            chrono::NaiveDate::from_ymd_opt(2026, 9, 27).unwrap(),
            3,
            consumed.iter().map(|name| (*name).to_owned()).collect(),
            OutputIdentity {
                size: 0,
                hash: blake3::hash(b""),
            },
        )
        .unwrap();
        write_marker(wal_dir, &marker).unwrap();
    }

    #[test]
    fn a_name_a_pending_marker_claims_is_taken() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = warmed(tmp.path());
        let env_dir = writer.dir().join("prod");
        // The marker's consumed file is already retired, so the name is
        // free on disk, but recovery would retire it again.
        let claimed = forced("svc", 1_000, 0xabcd);
        let other = forced("svc", 1_000, 0x0001);
        pending_marker(
            writer.dir(),
            "svc",
            &[&other.file_name(), &claimed.file_name()],
        );

        writer.force_names_for_test([claimed.clone()]);
        let path = writer.write("prod", "svc", b"{\"id\":1}\n").unwrap();

        assert_ne!(path, env_dir.join(claimed.file_name()));
        assert!(!env_dir.join(claimed.file_name()).exists());
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"id\":1}\n");
        assert_eq!(
            entries(&env_dir),
            [
                ".publish-svc.json".to_owned(),
                path.file_name().unwrap().to_str().unwrap().to_owned()
            ]
        );
    }

    #[test]
    fn a_marker_claims_only_the_names_it_lists() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = warmed(tmp.path());
        let env_dir = writer.dir().join("prod");
        pending_marker(writer.dir(), "svc", &["svc_1000_0001.ndjson"]);

        let unclaimed = forced("svc", 1_000, 0xabcd);
        writer.force_names_for_test([unclaimed.clone()]);
        let path = writer.write("prod", "svc", b"{\"id\":1}\n").unwrap();
        assert_eq!(path, env_dir.join(unclaimed.file_name()));
    }

    #[test]
    fn a_missing_marker_claims_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = warmed(tmp.path());
        let env_dir = writer.dir().join("prod");
        // Only this service's marker can claim its names: another
        // service's invalid marker does not stop it.
        std::fs::write(env_dir.join(".publish-other.json"), b"not json").unwrap();

        let named = forced("svc", 1_000, 0xabcd);
        writer.force_names_for_test([named.clone()]);
        let path = writer.write("prod", "svc", b"{\"id\":1}\n").unwrap();
        assert_eq!(path, env_dir.join(named.file_name()));
    }

    /// Plant `.publish-svc.json` with `body`, try a write for `svc`, and
    /// check that it was refused as retryable with nothing published: the
    /// `.tmp` is gone and no `.ndjson` name holds the batch. Returns the
    /// refusal's text.
    fn refused_by_marker(writer: &WalWriter) -> String {
        let env_dir = writer.dir().join("prod");
        writer.force_names_for_test([forced("svc", 1_000, 0xabcd)]);
        let err = writer.write("prod", "svc", b"{\"id\":1}\n").unwrap_err();
        let WalWriteError::NotPublished(ref cause) = err else {
            panic!("nothing was linked, so a retry is safe: {err:?}");
        };
        assert_eq!(
            entries(&env_dir),
            [".publish-svc.json"],
            "no staging file and no WAL file is left"
        );
        assert!(
            writer.take_synced_dirs_for_test().is_empty(),
            "nothing was published, so nothing needed a directory sync"
        );
        cause.to_string()
    }

    #[test]
    fn an_invalid_marker_refuses_the_write() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = warmed(tmp.path());
        let env_dir = writer.dir().join("prod");
        std::fs::write(env_dir.join(".publish-svc.json"), b"not json").unwrap();

        // The writer cannot tell which names the marker claims, and
        // recovery may yet retire them: taking any name could lose the
        // batch.
        let refusal = refused_by_marker(&writer);
        assert!(refusal.contains("is invalid"), "{refusal}");
        assert!(!refusal.contains(tmp.path().to_str().unwrap()), "{refusal}");

        // Other services write as before, and so does this one once the
        // marker is resolved.
        writer.write("prod", "other", b"{\"id\":2}\n").unwrap();
        std::fs::remove_file(env_dir.join(".publish-svc.json")).unwrap();
        writer.write("prod", "svc", b"{\"id\":3}\n").unwrap();
    }

    /// A marker far over the size bound refuses the write from `fstat` on
    /// its descriptor, before any read. It is sparse, so it costs no disk.
    #[cfg(unix)]
    #[test]
    fn an_oversized_marker_refuses_the_write_without_being_read() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = warmed(tmp.path());
        let marker = writer.dir().join("prod").join(".publish-svc.json");
        std::fs::File::create(&marker)
            .unwrap()
            .set_len(1 << 40)
            .unwrap();

        let refusal = refused_by_marker(&writer);
        assert!(refusal.contains("is invalid"), "{refusal}");
    }

    /// A FIFO at the marker path refuses the write at once: the writer
    /// never waits for a FIFO writer to come.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_fifo_marker_refuses_the_write_without_blocking() {
        use crate::ingest::no_follow::test_support::{make_fifo, returns_promptly};

        let tmp = tempfile::tempdir().unwrap();
        let writer = warmed(tmp.path());
        make_fifo(&writer.dir().join("prod").join(".publish-svc.json"));

        let refusal = returns_promptly(move || refused_by_marker(&writer));
        assert!(refusal.contains("is invalid"), "{refusal}");
    }

    /// A symlink at the marker path refuses the write, even to a valid
    /// marker that claims no name the write would take: the writer never
    /// follows it.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_marker_refuses_the_write() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = warmed(tmp.path());
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(elsewhere.join("prod")).unwrap();
        pending_marker(&elsewhere, "svc", &["svc_1000_0001.ndjson"]);
        std::os::unix::fs::symlink(
            elsewhere.join("prod").join(".publish-svc.json"),
            writer.dir().join("prod").join(".publish-svc.json"),
        )
        .unwrap();

        let refusal = refused_by_marker(&writer);
        assert!(refusal.contains("is invalid"), "{refusal}");
    }

    /// A marker that exists but cannot be read may claim any name.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_marker_that_cannot_be_read_refuses_the_write() {
        use std::os::unix::fs::PermissionsExt as _;

        crate::ingest::hydration::enforce_mode_bits_on_this_thread();
        let tmp = tempfile::tempdir().unwrap();
        let writer = warmed(tmp.path());
        pending_marker(writer.dir(), "svc", &["svc_1000_0001.ndjson"]);
        let marker = writer.dir().join("prod").join(".publish-svc.json");
        std::fs::set_permissions(&marker, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert!(std::fs::File::open(&marker).is_err());

        let refusal = refused_by_marker(&writer);
        assert!(refusal.contains("cannot be read"), "{refusal}");
        assert!(!refusal.contains(tmp.path().to_str().unwrap()), "{refusal}");

        std::fs::set_permissions(&marker, std::fs::Permissions::from_mode(0o600)).unwrap();
        writer.write("prod", "svc", b"{\"id\":2}\n").unwrap();
    }

    #[test]
    fn exhausted_names_reject_the_write_and_remove_the_tmp() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = warmed(tmp.path());
        let env_dir = writer.dir().join("prod");
        let taken = forced("svc", 1_000, 0xabcd);
        std::fs::write(env_dir.join(taken.file_name()), b"existing\n").unwrap();

        writer.force_names_for_test(std::iter::repeat_n(taken.clone(), MAX_NAME_ATTEMPTS + 3));
        let err = writer.write("prod", "svc", b"{\"id\":1}\n").unwrap_err();

        let WalWriteError::NotPublished(ref cause) = err else {
            panic!("nothing was linked, so a retry is safe: {err:?}");
        };
        assert_eq!(cause.kind(), std::io::ErrorKind::AlreadyExists, "{err:?}");
        assert_eq!(
            writer.forced_names.lock().len(),
            3,
            "one name per attempt, {MAX_NAME_ATTEMPTS} attempts"
        );
        assert_eq!(
            entries(&env_dir),
            [taken.file_name()],
            "no staging file is left"
        );
        assert_eq!(
            std::fs::read(env_dir.join(taken.file_name())).unwrap(),
            b"existing\n"
        );
        assert!(
            writer.take_synced_dirs_for_test().is_empty(),
            "nothing was published, so nothing needed a directory sync"
        );

        writer.forced_names.lock().clear();
        let path = writer.write("prod", "svc", b"{\"id\":2}\n").unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"{\"id\":2}\n");
    }

    #[test]
    fn a_failed_tmp_unlink_still_acknowledges_the_write() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = warmed(tmp.path());
        let env_dir = writer.dir().join("prod");

        writer.force_names_for_test([forced("svc", 1_000, 0xabcd)]);
        writer.fail_next_tmp_unlink_for_test();
        let (written, logs) = logged(|| writer.write("prod", "svc", b"{\"id\":1}\n"));

        let path = written.unwrap();
        assert_eq!(path, env_dir.join("svc_1000_abcd.ndjson"));
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"id\":1}\n");
        assert_eq!(
            writer.take_synced_dirs_for_test(),
            std::slice::from_ref(&env_dir),
            "the directory sync ran and acknowledged the write"
        );
        assert!(ack_sequence_for_test(&path).is_some());
        assert!(logs.contains("WARN"), "{logs}");
        assert!(
            logs.contains("event_type=\"wal_tmp_unlink_failed\""),
            "{logs}"
        );
        // The staging name stays; compaction and hydration never read it.
        assert_eq!(
            entries(&env_dir),
            ["svc_1000_abcd.ndjson", "svc_1000_abcd.tmp"]
        );
    }

    #[test]
    fn a_failed_tmp_unlink_does_not_change_a_durable_withdrawal() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = warmed(tmp.path());
        let env_dir = writer.dir().join("prod");

        writer.force_names_for_test([forced("svc", 1_000, 0xabcd)]);
        writer.fail_next_tmp_unlink_for_test();
        writer.fail_next_directory_sync_for_test();
        let err = writer.write("prod", "svc", b"{\"id\":1}\n").unwrap_err();

        assert!(matches!(err, WalWriteError::NotPublished(_)), "{err:?}");
        assert_eq!(
            entries(&env_dir),
            ["svc_1000_abcd.tmp"],
            "no .ndjson name holds the batch"
        );
    }

    #[test]
    fn a_durable_withdrawal_is_not_published() {
        use crate::metrics::test_support::sample;
        let recorder = crate::metrics::prometheus_builder().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            crate::metrics::init_operational_alert_metrics();
            let tmp = tempfile::tempdir().unwrap();
            let writer = warmed(tmp.path());
            let env_dir = writer.dir().join("prod");

            writer.fail_next_directory_sync_for_test();
            let err = writer.write("prod", "svc", b"{\"id\":1}\n").unwrap_err();

            assert!(matches!(err, WalWriteError::NotPublished(_)), "{err:?}");
            assert!(!err.left_visible());
            assert_eq!(
                writer.take_synced_dirs_for_test(),
                [env_dir.clone(), env_dir.clone()],
                "the withdrawal was synced before the rejection"
            );
            assert!(entries(&env_dir).is_empty(), "nothing remains");
            assert_eq!(sample(&handle, DIR_SYNC_FAILURES), 1);
        });
    }

    #[test]
    fn a_failed_withdrawal_is_left_visible() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = warmed(tmp.path());
        let env_dir = writer.dir().join("prod");

        writer.fail_next_directory_sync_for_test();
        writer.fail_next_withdraw_for_test();
        let err = writer.write("prod", "svc", b"{\"id\":1}\n").unwrap_err();

        assert!(err.left_visible(), "{err:?}");
        let WalWriteError::LeftVisible { path, .. } = err else {
            unreachable!()
        };
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"id\":1}\n");
        assert_eq!(
            entries(&env_dir),
            [path.file_name().unwrap().to_str().unwrap()],
            "the final name stays and the staging name is gone"
        );
    }

    #[test]
    fn a_withdrawal_whose_sync_fails_is_left_visible() {
        use crate::metrics::test_support::sample;
        let recorder = crate::metrics::prometheus_builder().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            crate::metrics::init_operational_alert_metrics();
            let tmp = tempfile::tempdir().unwrap();
            let writer = warmed(tmp.path());
            let env_dir = writer.dir().join("prod");

            writer.fail_next_directory_syncs_for_test(2);
            let err = writer.write("prod", "svc", b"{\"id\":1}\n").unwrap_err();

            // The name is gone now, but a power loss may restore it, so a
            // retry could duplicate the batch.
            assert!(err.left_visible(), "{err:?}");
            let WalWriteError::LeftVisible { path, .. } = err else {
                unreachable!()
            };
            assert_eq!(path.parent().unwrap(), env_dir);
            assert!(!path.exists());
            assert!(entries(&env_dir).is_empty());
            assert_eq!(
                writer.take_synced_dirs_for_test(),
                [env_dir.clone(), env_dir.clone()]
            );
            assert_eq!(sample(&handle, DIR_SYNC_FAILURES), 2);
        });
    }
}
