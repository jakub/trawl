// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ndjson query debug log.
//!
//! One JSON object per query execution, capturing DSL, generated SQL,
//! source paths, hot buffer state, result sample, and timing. Designed
//! for `tail -f /var/lib/trawl/query-debug.log | jq` debugging workflows.
//!
//! This file is MORE sensitive than the underlying event corpus: one
//! record combines identity, raw query text, SQL parameter values,
//! filesystem layout, and result samples. It is therefore owner-only
//! (`0600` on Unix, tightening a pre-existing looser file at open) and
//! size-bounded: past `server.query_log_max_bytes` the file rolls over
//! to a single retained `<path>.1` (also owner-only; `0` disables
//! rollover). A rollover that fails — the log deleted under a running
//! trawld, an occupied or unwritable `<path>.1` — keeps the entry and
//! backs off a whole cap before trying again, so an unrotatable path
//! costs one attempt per `max_bytes` written rather than one per query.
//! A rollover that lands its rename but cannot reopen the active path
//! (fd exhaustion, a re-planted symlink) is undone; in the one case
//! where even the undo fails the writer is left holding a file that is
//! no longer the configured path, so the log *stops* rather than growing
//! an unbounded, unwatched file full of identity and query text.
//!
//! Tightening is best-effort in exactly one direction: POSIX `chmod`
//! requires the caller to own the file, so a pre-existing log owned by
//! another uid cannot be re-moded even when it opens fine for append.
//! That refuses the open only when the file is *actually* reachable by
//! group or other — an already-owner-only foreign file is as tight as
//! this code would have made it, so it warns and continues rather than
//! turning an opt-in debug feature into a boot failure.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;
use serde_json::Value as JsonValue;

/// Append-only ndjson query debug log with owner-only permissions and
/// single-file rollover.
pub struct QueryLog {
    inner: Mutex<Inner>,
}

struct Inner {
    writer: BufWriter<File>,
    path: PathBuf,
    /// Rollover threshold in bytes; `0` disables rollover.
    max_bytes: u64,
    /// Current size of the active file.
    size: u64,
    /// Size past which the next rollover is attempted — `max_bytes`
    /// normally, but a *failed* attempt pushes it one whole cap ahead.
    /// Without that back-off an unrotatable path (the log deleted under
    /// a running trawld, a `<path>.1` that is a directory, a
    /// foreign-owned rotated file) would re-run the flush + rename
    /// syscall pair and emit another persisted `warn` on every single
    /// query, forever.
    next_attempt_bytes: u64,
    /// Set when a rollover renamed the active file away, failed to
    /// reopen `path`, AND failed to undo the rename: `writer` then holds
    /// the *rotated* generation, and no further rollover can ever
    /// succeed because `path` no longer exists. Writing on would append
    /// to a file nobody is tailing, without bound and without the cap
    /// this log exists to honour, so the log is closed until restart.
    detached: bool,
}

/// Open `path` for appending, owner-only on Unix (`0600` at creation,
/// and a pre-existing looser file is tightened).
fn open_owner_only(path: &Path) -> io::Result<File> {
    // The helper sets `0600` at creation AND re-applies it to a
    // pre-existing looser file, handing back a failed `chmod` instead of
    // raising it — the tolerance below is this log's own policy.
    let (file, chmod_error) = trawl_config::fs::open_with_mode(path, 0o600, |opts| {
        opts.create(true).append(true);
    })?;
    #[cfg(unix)]
    if let Some(err) = chmod_error {
        tolerate_chmod_failure(&file, path, &err)?;
    }
    #[cfg(not(unix))]
    let _ = chmod_error;
    Ok(file)
}

/// Whether a failed `chmod` on the log must refuse the open: it must,
/// unless the file's observed mode is already free of every group and
/// other bit. `None` (mode unreadable) is treated as unsafe.
#[cfg(unix)]
fn chmod_failure_is_fatal(mode: Option<u32>) -> bool {
    mode.is_none_or(|m| m & 0o077 != 0)
}

/// Decide what a failed `chmod 0600` on an already-open log file means,
/// tolerating the one failure that carries no exposure: a foreign-owned
/// file that is already owner-only (see the module docs).
#[cfg(unix)]
fn tolerate_chmod_failure(file: &File, path: &Path, chmod_err: &io::Error) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = file
        .metadata()
        .ok()
        .map(|meta| meta.permissions().mode() & 0o777);
    if chmod_failure_is_fatal(mode) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is group/other-accessible and cannot be tightened to 0600: {chmod_err}",
                path.display()
            ),
        ));
    }
    tracing::warn!(
        event_type = "query_log_chmod_failed",
        path = %path.display(),
        error = %chmod_err,
        "could not chmod query log to 0600 (not its owner?); the existing \
         mode is already owner-only, continuing"
    );
    Ok(())
}

impl QueryLog {
    /// Open (or create) a query log file in append mode, owner-only on
    /// Unix. `max_bytes` bounds the file: past it the log rolls over to a
    /// single retained `<path>.1`; `0` disables rollover.
    pub fn open(path: &Path, max_bytes: u64) -> io::Result<Self> {
        let file = open_owner_only(path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            inner: Mutex::new(Inner {
                writer: BufWriter::new(file),
                path: path.to_path_buf(),
                max_bytes,
                size,
                next_attempt_bytes: max_bytes,
                detached: false,
            }),
        })
    }

    /// Serialize and append an entry, rolling the file over first when it
    /// would exceed the size cap. Never panics or propagates errors —
    /// serialization failure is a bug, I/O failure is logged via tracing.
    pub fn write(&self, entry: &QueryLogEntry) {
        let line = match serde_json::to_string(entry) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "failed to serialize query log entry");
                return;
            }
        };
        let Ok(mut inner) = self.inner.lock() else {
            tracing::warn!("query log mutex poisoned, skipping entry");
            return;
        };
        if inner.detached {
            // A rollover left the writer on a file that is no longer the
            // configured path and could not be undone (warned once, at
            // detach). Appending here would defeat the size cap forever.
            return;
        }
        let line_bytes = line.len() as u64 + 1;
        if inner.max_bytes > 0
            && inner.size > 0
            && inner.size + line_bytes > inner.next_attempt_bytes
            && let Err(e) = inner.rollover()
        {
            // Keep writing to the over-cap file rather than lose the
            // entry — the cap is a bound, not a durability contract —
            // but do not re-attempt (or re-warn) until another cap's
            // worth of entries has been written.
            inner.next_attempt_bytes = inner.size.saturating_add(inner.max_bytes);
            tracing::warn!(
                error = %e,
                retry_after_bytes = inner.next_attempt_bytes,
                "query log rollover failed; continuing in current file"
            );
        }
        if let Err(e) = writeln!(inner.writer, "{line}") {
            tracing::warn!(error = %e, "failed to write query log entry");
            return;
        }
        inner.size += line_bytes;
        if let Err(e) = inner.writer.flush() {
            tracing::warn!(error = %e, "failed to flush query log");
        }
    }
}

impl Inner {
    /// Roll the active file over to `<path>.1` (replacing any previous
    /// rotated file — exactly one previous generation is retained) and
    /// reopen a fresh active file. The rename preserves the `0600` mode.
    ///
    /// On error the caller keeps writing through the existing writer, so
    /// a rename that lands but is not followed by a successful reopen is
    /// undone: otherwise the writer would go on appending into the file
    /// that is now the *rotated* generation, breaking the "entries are
    /// split, not duplicated" contract — and, because `path` would no
    /// longer exist, no later rollover could bound that file either.
    /// Should the undo itself fail, the writer is unrecoverably detached
    /// from `path` and the log is closed rather than left unbounded.
    fn rollover(&mut self) -> io::Result<()> {
        self.rollover_with(open_owner_only)
    }

    /// `rollover`, with the reopen injected so the post-rename failure
    /// window is reachable from tests.
    fn rollover_with(&mut self, reopen: fn(&Path) -> io::Result<File>) -> io::Result<()> {
        self.writer.flush()?;
        let rotated = self.rotated_path();
        std::fs::rename(&self.path, &rotated)?;
        match reopen(&self.path) {
            Ok(file) => {
                self.writer = BufWriter::new(file);
                self.size = 0;
                self.next_attempt_bytes = self.max_bytes;
                Ok(())
            }
            Err(err) => {
                if let Err(restore) = std::fs::rename(&rotated, &self.path) {
                    self.detached = true;
                    tracing::error!(
                        error = %restore,
                        reopen_error = %err,
                        path = %self.path.display(),
                        "query log rollover could not be undone; the writer no longer \
                         holds the configured path, so the log is closed until restart"
                    );
                }
                Err(err)
            }
        }
    }

    /// The single retained previous generation, `<path>.1`.
    fn rotated_path(&self) -> PathBuf {
        let mut rotated = self.path.clone().into_os_string();
        rotated.push(".1");
        PathBuf::from(rotated)
    }
}

impl std::fmt::Debug for QueryLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueryLog").finish_non_exhaustive()
    }
}

/// A single query execution log entry.
#[derive(Debug, Serialize)]
pub struct QueryLogEntry {
    /// ISO 8601 timestamp of the log entry.
    pub ts: String,
    /// Authenticated user name.
    pub user: String,
    /// User's role.
    pub role: String,
    /// Raw DSL query string.
    pub dsl: String,
    /// Source file selection debug info.
    pub source: SourceDebug,
    /// Hot buffer state at query time.
    pub hot_buffer: HotBufferDebug,
    /// Generated SQL (parameterized).
    pub sql: String,
    /// SQL parameter values (Display form).
    pub params: Vec<String>,
    /// Query result summary.
    pub result: ResultDebug,
    /// Execution timing breakdown.
    pub timing_ms: TimingDebug,
    /// Error message if the query failed, null otherwise.
    pub error: Option<String>,
}

/// Debug info about parquet source file selection.
#[derive(Debug, Serialize)]
pub struct SourceDebug {
    /// The computed source argument passed to `read_parquet()`.
    pub computed: String,
    /// Number of glob patterns in the source list.
    pub globs: usize,
    /// Service name extracted from the DSL, if any.
    pub service_filter: Option<String>,
    /// Time filter duration in seconds, if any.
    pub time_filter_secs: Option<u64>,
    /// Whether the source fell back to recursive glob.
    pub is_fallback: bool,
}

/// Debug info about the hot buffer state at query time.
#[derive(Debug, Serialize)]
pub struct HotBufferDebug {
    /// "disabled", "empty", or "active".
    pub status: &'static str,
    /// Total events in the hot buffer.
    pub events: usize,
    /// Number of batches in the hot buffer.
    pub batches: usize,
    /// Estimated byte size of the hot buffer.
    pub bytes: usize,
}

/// Debug info about the query result.
#[derive(Debug, Serialize)]
pub struct ResultDebug {
    /// "success" or "error".
    pub status: &'static str,
    /// Column names in the result set.
    pub columns: Vec<String>,
    /// Total row count.
    pub row_count: usize,
    /// First N rows as self-describing maps.
    pub sample: Vec<BTreeMap<String, JsonValue>>,
}

/// Execution timing breakdown.
#[derive(Debug, Serialize)]
pub struct TimingDebug {
    /// Time spent waiting for a pool permit (ms).
    pub pool_wait: u64,
    /// Total query execution time (ms).
    pub total: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn entry(marker: &str) -> QueryLogEntry {
        QueryLogEntry {
            ts: "2026-01-01T00:00:00Z".into(),
            user: "tester".into(),
            role: "analyst".into(),
            dsl: marker.to_owned(),
            source: SourceDebug {
                computed: String::new(),
                globs: 0,
                service_filter: None,
                time_filter_secs: None,
                is_fallback: false,
            },
            hot_buffer: HotBufferDebug {
                status: "disabled",
                events: 0,
                batches: 0,
                bytes: 0,
            },
            sql: "SELECT 1".into(),
            params: vec![],
            result: ResultDebug {
                status: "success",
                columns: vec![],
                row_count: 0,
                sample: vec![],
            },
            timing_ms: TimingDebug {
                pool_wait: 0,
                total: 0,
            },
            error: None,
        }
    }

    #[cfg(unix)]
    #[test]
    fn open_creates_owner_only_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("query.log");
        let _log = QueryLog::open(&path, 0).unwrap();
        assert_eq!(mode_of(&path), 0o600, "query log must be owner-only");
    }

    #[cfg(unix)]
    #[test]
    fn open_tightens_existing_looser_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("query.log");
        std::fs::write(&path, "{}\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let _log = QueryLog::open(&path, 0).unwrap();
        assert_eq!(
            mode_of(&path),
            0o600,
            "pre-existing query log must be tightened to owner-only"
        );
    }

    /// A `chmod` we are not permitted to make (foreign-owned file) is
    /// only worth refusing the open over when the file is actually
    /// exposed — otherwise trawld would refuse to boot over an opt-in
    /// debug log that leaks nothing.
    #[cfg(unix)]
    #[test]
    fn chmod_failure_is_fatal_only_when_group_or_other_can_reach_the_file() {
        for mode in [0o600, 0o400, 0o200, 0o000] {
            assert!(
                !chmod_failure_is_fatal(Some(mode)),
                "{mode:04o} is already owner-only; failing chmod must not refuse the open"
            );
        }
        for mode in [0o644, 0o640, 0o660, 0o606, 0o601, 0o666] {
            assert!(
                chmod_failure_is_fatal(Some(mode)),
                "{mode:04o} is group/other-accessible and untightenable; must refuse"
            );
        }
        assert!(
            chmod_failure_is_fatal(None),
            "an unreadable mode must be assumed unsafe"
        );
    }

    #[test]
    fn rollover_at_cap_leaves_current_and_one_previous_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("query.log");
        let rotated = tmp.path().join("query.log.1");

        // Cap small enough that the second entry triggers a rollover.
        let probe = serde_json::to_string(&entry("first")).unwrap();
        let cap = (probe.len() + 10) as u64;

        let log = QueryLog::open(&path, cap).unwrap();
        log.write(&entry("first"));
        log.write(&entry("second"));

        assert!(rotated.exists(), "rollover must create <path>.1");
        let old = std::fs::read_to_string(&rotated).unwrap();
        let new = std::fs::read_to_string(&path).unwrap();
        assert!(old.contains("first"), "rotated file keeps older entries");
        assert!(new.contains("second"), "current file has newer entries");
        assert!(!new.contains("first"), "entries are split, not duplicated");

        #[cfg(unix)]
        {
            assert_eq!(mode_of(&path), 0o600);
            assert_eq!(mode_of(&rotated), 0o600, "rotated file stays owner-only");
        }
    }

    #[test]
    fn rollover_replaces_previous_rotated_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("query.log");
        let rotated = tmp.path().join("query.log.1");

        let probe = serde_json::to_string(&entry("aaaaa")).unwrap();
        let cap = (probe.len() + 10) as u64;

        let log = QueryLog::open(&path, cap).unwrap();
        log.write(&entry("aaaaa"));
        log.write(&entry("bbbbb")); // rotates: .1 = aaaaa
        log.write(&entry("ccccc")); // rotates: .1 = bbbbb

        let old = std::fs::read_to_string(&rotated).unwrap();
        assert!(old.contains("bbbbb"), "only ONE previous file is retained");
        assert!(!old.contains("aaaaa"), "oldest generation is gone");
        let files: Vec<_> = std::fs::read_dir(tmp.path()).unwrap().collect();
        assert_eq!(files.len(), 2, "exactly path + path.1");
    }

    /// A rollover that cannot happen (here: `<path>.1` occupied by a
    /// directory — same shape as the log being deleted under a running
    /// trawld) must not be re-attempted per entry: every attempt is a
    /// flush + rename syscall pair and a *persisted* warn, so retrying
    /// one per query would be exactly the write amplification this log
    /// is bounded to avoid.
    #[test]
    fn failed_rollover_backs_off_instead_of_retrying_every_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("query.log");
        let rotated = tmp.path().join("query.log.1");
        std::fs::create_dir(&rotated).unwrap();

        // Two entries fit under the cap; the third triggers a rollover.
        let line = serde_json::to_string(&entry("e1")).unwrap().len() as u64 + 1;
        let cap = 2 * line + 18;

        let log = QueryLog::open(&path, cap).unwrap();
        log.write(&entry("e1"));
        log.write(&entry("e2"));
        log.write(&entry("e3")); // rollover attempted, fails on the directory
        assert!(rotated.is_dir(), "the obstruction is untouched");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("e1") && content.contains("e3"),
            "a failed rollover still writes the entry"
        );

        // Clear the obstruction: the very next entry must NOT re-attempt.
        std::fs::remove_dir(&rotated).unwrap();
        log.write(&entry("e4"));
        assert!(
            !rotated.exists(),
            "a failed rollover is backed off, not retried on the next entry"
        );

        // ...but one cap's worth later it tries again, and succeeds.
        log.write(&entry("e5"));
        assert!(rotated.is_file(), "back-off expires and rollover resumes");
        let new = std::fs::read_to_string(&path).unwrap();
        assert!(new.contains("e5") && !new.contains("e4"), "entries split");
    }

    /// The rename can land and the reopen still fail (fd exhaustion, a
    /// symlink re-planted at the path, a directory gone read-only in the
    /// window). The writer must not be left holding the rotated file:
    /// that would append every later entry to `<path>.1` — unbounded,
    /// since `path` no longer exists for any future rollover to rename.
    #[test]
    fn failed_reopen_undoes_the_rename_and_keeps_the_active_path() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("query.log");
        let rotated = tmp.path().join("query.log.1");

        let log = QueryLog::open(&path, 4096).unwrap();
        log.write(&entry("before"));

        let err = log
            .inner
            .lock()
            .unwrap()
            .rollover_with(|_| Err(io::Error::other("reopen refused")))
            .unwrap_err();
        assert_eq!(err.to_string(), "reopen refused");

        assert!(path.is_file(), "the active log is restored, not left gone");
        assert!(!rotated.exists(), "the half-done rotation is undone");

        log.write(&entry("after"));
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("before") && content.contains("after"),
            "writing continues into the active path, not the rotated one"
        );
        assert!(!rotated.exists(), "still nothing at the rotated path");
    }

    /// ...and when even the undo fails — here the reopen loses a race to
    /// a directory planted at the path — the writer is holding a file
    /// that is no longer `path` and never can be rotated again, so the
    /// log closes instead of growing without bound.
    #[test]
    fn unrestorable_rollover_closes_the_log_instead_of_growing_it() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("query.log");
        let rotated = tmp.path().join("query.log.1");

        let log = QueryLog::open(&path, 4096).unwrap();
        log.write(&entry("before"));

        let err = log
            .inner
            .lock()
            .unwrap()
            .rollover_with(|p| {
                // Occupy the active path so the undo rename cannot land.
                std::fs::create_dir(p)?;
                Err(io::Error::other("reopen refused"))
            })
            .unwrap_err();
        assert_eq!(err.to_string(), "reopen refused");
        assert!(path.is_dir(), "the obstruction is untouched");

        let rotated_len = std::fs::metadata(&rotated).unwrap().len();
        log.write(&entry("after"));
        assert_eq!(
            std::fs::metadata(&rotated).unwrap().len(),
            rotated_len,
            "a detached log stops writing rather than appending to the rotated file"
        );
        assert!(
            log.inner.lock().unwrap().detached,
            "the log is marked closed until restart"
        );
    }

    #[test]
    fn zero_cap_disables_rollover() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("query.log");

        let log = QueryLog::open(&path, 0).unwrap();
        for i in 0..50 {
            log.write(&entry(&format!("entry_{i}")));
        }
        assert!(!tmp.path().join("query.log.1").exists());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("entry_0") && content.contains("entry_49"));
    }
}
