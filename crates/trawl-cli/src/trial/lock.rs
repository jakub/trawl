// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The lifecycle lock: `up`, `stop`, and `down` run one at a time.
//!
//! An exclusive `flock` on `$XDG_STATE_HOME/trawl/trial.lock`, through
//! std's `File::lock`. The lock belongs to the open file, so the kernel
//! releases it when the process exits, however it exits. The file is never
//! unlinked: a waiter blocks on the inode it opened, and unlinking would let
//! the next arrival lock a fresh inode alongside it.
//!
//! `status`, `key`, and `-p trial` do not take the lock. They read files
//! that are only ever replaced by an atomic rename.
//!
//! The lock serializes invocations by one user on one state directory.
//! Two users, or two state directories, on one Docker engine are kept
//! apart by the engine claim container instead.

use std::fs::{File, TryLockError};
use std::path::Path;

use super::TrialError;

/// Held for the life of the value; dropping it releases the lock.
#[derive(Debug)]
pub struct TrialLock {
    _file: File,
}

impl TrialLock {
    /// Take the lock, waiting for it if another invocation holds it.
    /// `on_wait` runs once, before blocking, only when the lock is busy,
    /// so the caller can say what it is waiting for.
    pub fn acquire(path: &Path, on_wait: impl FnOnce()) -> Result<Self, TrialError> {
        if let Some(lock) = Self::try_acquire(path)? {
            return Ok(lock);
        }
        on_wait();
        let file = open(path)?;
        file.lock().map_err(|e| TrialError::io("lock", path, e))?;
        Ok(Self { _file: file })
    }

    /// Take the lock if it is free; `Ok(None)` if another invocation
    /// holds it.
    pub fn try_acquire(path: &Path) -> Result<Option<Self>, TrialError> {
        let file = open(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(TrialError::io("lock", path, e)),
        }
    }
}

/// Open (creating at 0600) the lock file without following a symlink.
/// The caller has verified the state root.
fn open(path: &Path) -> Result<File, TrialError> {
    let (file, chmod) = trawl_config::fs::open_with_mode(path, 0o600, |opts| {
        opts.read(true).write(true).create(true);
    })
    .map_err(|e| TrialError::io("open", path, e))?;
    if let Some(e) = chmod {
        return Err(TrialError::io("set the mode of", path, e));
    }
    let meta = file
        .metadata()
        .map_err(|e| TrialError::io("inspect", path, e))?;
    if !meta.is_file() {
        return Err(TrialError::NotPrivateFile {
            path: path.to_owned(),
            reason: "the lock must be a regular file",
        });
    }
    Ok(file)
}

#[cfg(test)]
pub(super) mod tests {
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::process::{Child, ChildStderr, Command, Stdio};
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;
    use crate::trial::paths::TrialPaths;

    /// Set in a re-executed test binary to make [`lock_child_process`]
    /// act as a lock holder or waiter instead of passing as a no-op.
    const CHILD_ROLE: &str = "TRAWL_TEST_TRIAL_LOCK_ROLE";
    const CHILD_PATH: &str = "TRAWL_TEST_TRIAL_LOCK_PATH";

    const HELD: &str = "trial-lock: held";
    const WAITING: &str = "trial-lock: waiting";
    const ACQUIRED: &str = "trial-lock: acquired";

    /// Generous: a child only has to start and take a free lock.
    const STEP: Duration = Duration::from_secs(60);

    fn ready_paths() -> (tempfile::TempDir, TrialPaths) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = TrialPaths::resolve(Some(&tmp.path().join("state")), None).unwrap();
        paths.ensure_root().unwrap();
        (tmp, paths)
    }

    /// The child side of the cross-process test. Under a normal test run
    /// neither variable is set and it passes without doing anything.
    ///
    /// - `hold`: take the lock, report `HELD`, and keep it until stdin
    ///   yields a line (or closes).
    /// - `wait`: take the lock through [`TrialLock::acquire`], reporting
    ///   `WAITING` from `on_wait`, then `ACQUIRED`, and exit.
    ///
    /// Markers go to stderr: libtest writes its own report to stdout.
    #[test]
    fn lock_child_process() {
        let (Some(role), Some(path)) = (std::env::var_os(CHILD_ROLE), std::env::var_os(CHILD_PATH))
        else {
            return;
        };
        let path = Path::new(&path);
        match role.to_str() {
            Some("hold") => {
                let _lock = TrialLock::try_acquire(path)
                    .unwrap()
                    .expect("the holder starts first, so the lock is free");
                eprintln!("{HELD}");
                let mut line = String::new();
                let _ = std::io::stdin().read_line(&mut line);
            }
            Some("wait") => {
                let _lock = TrialLock::acquire(path, || eprintln!("{WAITING}")).unwrap();
                eprintln!("{ACQUIRED}");
            }
            other => panic!("unknown lock child role {other:?}"),
        }
    }

    /// Re-execute this test binary as a lock child.
    pub(crate) fn spawn_lock_child(role: &str, path: &Path) -> (Child, mpsc::Receiver<String>) {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "trial::lock::tests::lock_child_process",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_ROLE, role)
            .env(CHILD_PATH, path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("re-execute the test binary");
        let lines = forward_lines(child.stderr.take().unwrap());
        (child, lines)
    }

    /// Stream a child's stderr lines through a channel, so a missing
    /// marker times out instead of hanging the test.
    fn forward_lines(stderr: ChildStderr) -> mpsc::Receiver<String> {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        rx
    }

    /// Wait for `marker`, failing on any other marker or on silence.
    pub(crate) fn expect_marker(lines: &mpsc::Receiver<String>, marker: &str) {
        loop {
            let line = lines
                .recv_timeout(STEP)
                .unwrap_or_else(|e| panic!("no {marker:?} from the lock child: {e}"));
            if line == marker {
                return;
            }
            assert!(
                !line.starts_with("trial-lock:"),
                "expected {marker:?}, the child reported {line:?}"
            );
        }
    }

    fn exit_ok(mut child: Child) {
        let status = child.wait().unwrap();
        assert!(status.success(), "lock child failed: {status}");
    }

    /// Two real processes: while the first holds the lock, the second
    /// finds it busy, says so, and blocks; it acquires only after the
    /// first releases.
    #[test]
    fn a_second_process_waits_until_the_first_releases() {
        let (_tmp, paths) = ready_paths();

        let (mut holder, holder_lines) = spawn_lock_child("hold", &paths.lock);
        expect_marker(&holder_lines, HELD);

        let (mut waiter, waiter_lines) = spawn_lock_child("wait", &paths.lock);
        // `WAITING` is printed only after `try_lock` reported the lock busy.
        expect_marker(&waiter_lines, WAITING);
        // The waiter exits right after acquiring, so still running means
        // still blocked.
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            waiter.try_wait().unwrap().is_none(),
            "the waiter finished while the holder still held the lock"
        );
        assert!(
            waiter_lines.try_recv().is_err(),
            "the waiter reported more before the holder released"
        );

        holder
            .stdin
            .take()
            .unwrap()
            .write_all(b"release\n")
            .unwrap();
        exit_ok(holder);

        expect_marker(&waiter_lines, ACQUIRED);
        let _ = waiter.stdin.take();
        exit_ok(waiter);

        assert!(paths.lock.exists(), "the lock file is never unlinked");
    }

    /// `flock` belongs to the open file, so two opens in one process
    /// exclude each other as two processes do.
    #[test]
    fn try_acquire_reports_a_held_lock() {
        let (_tmp, paths) = ready_paths();
        let first = TrialLock::try_acquire(&paths.lock).unwrap().expect("free");
        assert!(TrialLock::try_acquire(&paths.lock).unwrap().is_none());
        drop(first);
        let again = TrialLock::try_acquire(&paths.lock).unwrap();
        assert!(again.is_some(), "dropping the holder releases the lock");
    }

    #[test]
    fn acquire_calls_on_wait_only_when_busy() {
        let (_tmp, paths) = ready_paths();
        let mut waited = false;
        let lock = TrialLock::acquire(&paths.lock, || waited = true).unwrap();
        assert!(!waited, "a free lock is taken without waiting");
        drop(lock);
        assert!(paths.lock.exists(), "releasing does not unlink the file");
    }

    #[test]
    fn a_symlinked_lock_file_is_refused() {
        let (tmp, paths) = ready_paths();
        let target = tmp.path().join("target");
        std::fs::write(&target, "").unwrap();
        std::os::unix::fs::symlink(&target, &paths.lock).unwrap();
        assert!(TrialLock::try_acquire(&paths.lock).is_err());
    }
}
