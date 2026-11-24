// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The lifecycle lock: `up`, `stop`, and `down` run one at a time.
//!
//! An exclusive `flock` on `$XDG_STATE_HOME/trawl/trial.lock`, through
//! std's `File::lock`. The file is never unlinked: a waiter blocks on the
//! inode it opened, and unlinking would let the next arrival lock a fresh
//! inode alongside it.
//!
//! Its descriptor is inheritable ([`share_with_children`]), so every
//! `docker` child holds the lock too. A `trawl` killed by SIGKILL, which
//! runs no destructor, leaves its docker children holding the lock until
//! they exit, and a rerun waits for them instead of racing an orphaned
//! `compose run` or `compose exec`. A killed `compose run` client leaves
//! its container running, and a container holds no lock. So `up`,
//! `stop`, and `down` still wait for running one-off containers.
//!
//! Every docker call made under the lock ends on its own, and nothing
//! follows logs or events.
//!
//! `status`, `key`, and `-p trial` do not take the lock. They read files
//! that are only ever replaced by an atomic rename.
//!
//! The lock serializes invocations by one user on one state directory.
//! Two users, or two state directories, on one Docker engine are kept
//! apart by the engine claim container instead.

use std::fs::{File, TryLockError};
use std::io;
use std::os::fd::AsRawFd as _;
use std::path::Path;

use nix::fcntl::{FcntlArg, FdFlag, fcntl};

use super::TrialError;

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
        Self::held(file, path)
    }

    pub fn try_acquire(path: &Path) -> Result<Option<Self>, TrialError> {
        let file = open(path)?;
        match file.try_lock() {
            Ok(()) => Self::held(file, path).map(Some),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(TrialError::io("lock", path, e)),
        }
    }

    fn held(file: File, path: &Path) -> Result<Self, TrialError> {
        share_with_children(&file).map_err(|e| TrialError::io("share the lock", path, e))?;
        Ok(Self { _file: file })
    }
}

/// std opens every file close-on-exec, and neither std nor tokio closes
/// an inheritable descriptor when it spawns a child, so clearing
/// `FD_CLOEXEC` is the only step needed.
fn share_with_children(file: &File) -> io::Result<()> {
    let fd = file.as_raw_fd();
    let flags = FdFlag::from_bits_truncate(fcntl(fd, FcntlArg::F_GETFD)?);
    fcntl(fd, FcntlArg::F_SETFD(flags - FdFlag::FD_CLOEXEC))?;
    Ok(())
}

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

    const CHILD_ROLE: &str = "TRAWL_TEST_TRIAL_LOCK_ROLE";
    const CHILD_PATH: &str = "TRAWL_TEST_TRIAL_LOCK_PATH";

    const HELD: &str = "trial-lock: held";
    const WAITING: &str = "trial-lock: waiting";
    const ACQUIRED: &str = "trial-lock: acquired";

    const STUB_PID: &str = "docker-stub.pid";
    const STUB_RELEASE: &str = "docker-stub.release";

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
            Some("spawn") => {
                use crate::trial::docker::{Args, DockerCli, LocalEndpoint, Sensitivity};

                let _lock = TrialLock::try_acquire(path)
                    .unwrap()
                    .expect("the holder starts first, so the lock is free");
                let dir = path.parent().unwrap();
                let script = dir.join("docker.sh");
                std::fs::write(
                    &script,
                    "echo $$ > \"$1\"\n\
                     while [ -e \"$1\" ] && [ ! -e \"$2\" ]; do sleep 0.05; done\n",
                )
                .unwrap();
                let docker = DockerCli::stub(&script)
                    .pin(LocalEndpoint::unchecked("unix:///stub/docker.sock"), dir);
                let args = Args::new()
                    .arg(dir.join(STUB_PID))
                    .arg(dir.join(STUB_RELEASE));
                eprintln!("{HELD}");
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(docker.output(
                        &args,
                        None,
                        Sensitivity::Diagnose,
                        Duration::from_secs(600),
                    ))
                    .unwrap();
            }
            other => panic!("unknown lock child role {other:?}"),
        }
    }

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

    /// Creates the stub's release file when dropped, so an orphaned stub
    /// exits even when an assertion fails first.
    struct Release(std::path::PathBuf);

    impl Drop for Release {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.0, "");
        }
    }

    fn alive(pid: i32) -> bool {
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
    }

    #[test]
    fn a_killed_holder_s_docker_child_keeps_the_lock() {
        let (_tmp, paths) = ready_paths();
        let dir = paths.lock.parent().unwrap().to_owned();
        let release = Release(dir.join(STUB_RELEASE));

        let (mut holder, holder_lines) = spawn_lock_child("spawn", &paths.lock);
        expect_marker(&holder_lines, HELD);
        let deadline = std::time::Instant::now() + STEP;
        let stub = loop {
            let text = std::fs::read_to_string(dir.join(STUB_PID)).unwrap_or_default();
            if let Ok(pid) = text.trim().parse::<i32>() {
                break pid;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the docker stub did not start"
            );
            std::thread::sleep(Duration::from_millis(20));
        };

        holder.kill().unwrap();
        holder.wait().unwrap();
        assert!(alive(stub), "the docker stub outlives its killed parent");

        let (mut waiter, waiter_lines) = spawn_lock_child("wait", &paths.lock);
        expect_marker(&waiter_lines, WAITING);
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            waiter.try_wait().unwrap().is_none(),
            "the waiter acquired while the docker stub still held the lock"
        );

        drop(release);
        expect_marker(&waiter_lines, ACQUIRED);
        let _ = waiter.stdin.take();
        exit_ok(waiter);
    }

    /// This and the next test take the lock in the test process itself.
    /// nextest runs each test in its own process; under a threaded
    /// `cargo test`, a child another test spawns meanwhile would inherit
    /// the descriptor and hold the lock until it exits.
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
