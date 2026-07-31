// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};

const DEFAULT_TIMEOUT: Duration = Duration::from_mins(5);
const DEFAULT_STDOUT_LIMIT: usize = 1024 * 1024;
const DEFAULT_STDERR_LIMIT: usize = 256 * 1024;

/// Fully specified subprocess request.
///
/// Its `Debug` representation intentionally omits environment values because
/// credential and resolver children may receive secrets.
#[derive(Clone)]
pub struct CommandSpec {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    pub environment: BTreeMap<OsString, OsString>,
    pub clear_environment: bool,
    pub timeout: Duration,
    pub stdout_limit: usize,
    pub stderr_limit: usize,
    pub report_stderr: bool,
}

impl CommandSpec {
    #[must_use]
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            environment: BTreeMap::new(),
            clear_environment: true,
            timeout: DEFAULT_TIMEOUT,
            stdout_limit: DEFAULT_STDOUT_LIMIT,
            stderr_limit: DEFAULT_STDERR_LIMIT,
            report_stderr: false,
        }
    }

    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    #[must_use]
    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    #[must_use]
    pub fn environment(mut self, environment: BTreeMap<OsString, OsString>) -> Self {
        self.environment = environment;
        self
    }

    #[must_use]
    pub fn env(mut self, name: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.environment.insert(name.into(), value.into());
        self
    }

    #[must_use]
    pub const fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    #[must_use]
    pub const fn output_limits(mut self, stdout: usize, stderr: usize) -> Self {
        self.stdout_limit = stdout;
        self.stderr_limit = stderr;
        self
    }

    #[must_use]
    pub const fn report_stderr(mut self) -> Self {
        self.report_stderr = true;
        self
    }

    #[must_use]
    pub fn display_program(&self) -> String {
        self.program.to_string_lossy().into_owned()
    }
}

impl std::fmt::Debug for CommandSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandSpec")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("cwd", &self.cwd)
            .field(
                "environment_names",
                &self.environment.keys().collect::<Vec<_>>(),
            )
            .field("clear_environment", &self.clear_environment)
            .field("timeout", &self.timeout)
            .field("stdout_limit", &self.stdout_limit)
            .field("stderr_limit", &self.stderr_limit)
            .field("report_stderr", &self.report_stderr)
            .finish()
    }
}

pub trait CommandRunner: std::fmt::Debug + Send + Sync {
    fn output(&self, spec: &CommandSpec) -> Result<Output>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    #[allow(clippy::too_many_lines)] // Keep spawn, limits, reaping, and capture in one audit boundary.
    fn output(&self, spec: &CommandSpec) -> Result<Output> {
        let mut command = std::process::Command::new(&spec.program);
        command
            .args(&spec.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if spec.clear_environment {
            command.env_clear();
        }
        command.envs(&spec.environment);
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command.spawn().map_err(|source| Error::Spawn {
            program: spec.display_program(),
            source,
        })?;
        let stdout = child.stdout.take().expect("stdout was configured as piped");
        let stderr = child.stderr.take().expect("stderr was configured as piped");
        let overflow = Arc::new(AtomicBool::new(false));
        let stop_readers = Arc::new(AtomicBool::new(false));
        let stdout_done = Arc::new(AtomicBool::new(false));
        let stderr_done = Arc::new(AtomicBool::new(false));
        let stdout_reader = spawn_bounded_reader(
            stdout,
            spec.stdout_limit,
            Arc::clone(&overflow),
            Arc::clone(&stop_readers),
            Arc::clone(&stdout_done),
        );
        let stderr_reader = spawn_bounded_reader(
            stderr,
            spec.stderr_limit,
            Arc::clone(&overflow),
            Arc::clone(&stop_readers),
            Arc::clone(&stderr_done),
        );
        let deadline = Instant::now() + spec.timeout;
        let mut exited_at = None;
        let (status, mut limit_error) = loop {
            if overflow.load(Ordering::Acquire) {
                terminate_process_tree(&mut child)?;
                break (
                    child.wait()?,
                    Some(format!(
                        "output exceeded the configured {}/{} byte stdout/stderr limits",
                        spec.stdout_limit, spec.stderr_limit
                    )),
                );
            }
            if Instant::now() >= deadline {
                terminate_process_tree(&mut child)?;
                break (
                    child.wait()?,
                    Some(format!("deadline of {:?} expired", spec.timeout)),
                );
            }
            if let Some(status) = child.try_wait()? {
                let exited_at = exited_at.get_or_insert_with(Instant::now);
                if stdout_done.load(Ordering::Acquire) && stderr_done.load(Ordering::Acquire) {
                    break (status, None);
                }
                if exited_at.elapsed() >= Duration::from_millis(100) {
                    terminate_process_tree(&mut child)?;
                    break (
                        status,
                        Some(
                            "a descendant retained subprocess output after the command exited"
                                .to_owned(),
                        ),
                    );
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        stop_readers.store(true, Ordering::Release);
        let stdout = join_reader(stdout_reader)?;
        let stderr = join_reader(stderr_reader)?;
        if overflow.load(Ordering::Acquire) && limit_error.is_none() {
            limit_error = Some(format!(
                "output exceeded the configured {}/{} byte stdout/stderr limits",
                spec.stdout_limit, spec.stderr_limit
            ));
        }
        if let Some(reason) = limit_error {
            let mut stdout = stdout;
            let mut stderr = stderr;
            zeroize::Zeroize::zeroize(&mut stdout);
            zeroize::Zeroize::zeroize(&mut stderr);
            return Err(Error::CommandLimit {
                program: spec.display_program(),
                reason,
            });
        }
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }
}

fn spawn_bounded_reader<R>(
    reader: R,
    limit: usize,
    overflow: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
) -> std::thread::JoinHandle<std::io::Result<Vec<u8>>>
where
    R: Read + Send + std::os::fd::AsFd + 'static,
{
    std::thread::spawn(move || {
        let result = read_bounded(reader, limit, &overflow, &stop);
        done.store(true, Ordering::Release);
        result
    })
}

fn read_bounded(
    mut reader: impl Read + std::os::fd::AsFd,
    limit: usize,
    overflow: &AtomicBool,
    stop: &AtomicBool,
) -> std::io::Result<Vec<u8>> {
    set_nonblocking(&reader)?;
    let mut output = Vec::with_capacity(limit.min(8192));
    let mut buffer = [0_u8; 8192];
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if stop.load(Ordering::Acquire) {
                    return Ok(output);
                }
                std::thread::sleep(Duration::from_millis(2));
                continue;
            }
            Err(error) => return Err(error),
        };
        if read == 0 {
            return Ok(output);
        }
        let remaining = limit.saturating_sub(output.len());
        output.extend_from_slice(&buffer[..read.min(remaining)]);
        if read > remaining {
            overflow.store(true, Ordering::Release);
            return Ok(output);
        }
    }
}

fn set_nonblocking(reader: &impl std::os::fd::AsFd) -> std::io::Result<()> {
    use nix::fcntl::{FcntlArg, OFlag, fcntl};
    use std::os::fd::AsRawFd;
    let fd = reader.as_fd().as_raw_fd();
    let flags = fcntl(fd, FcntlArg::F_GETFL).map_err(std::io::Error::other)?;
    let flags = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;
    fcntl(fd, FcntlArg::F_SETFL(flags))
        .map(|_| ())
        .map_err(std::io::Error::other)
}

fn join_reader(handle: std::thread::JoinHandle<std::io::Result<Vec<u8>>>) -> Result<Vec<u8>> {
    handle
        .join()
        .map_err(|_| Error::InvalidArgument("subprocess output reader thread panicked".to_owned()))?
        .map_err(Error::Io)
}

fn terminate_process_tree(child: &mut std::process::Child) -> Result<()> {
    #[cfg(unix)]
    {
        let id = child.id();
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(id.cast_signed()),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    child.kill().or_else(|error| {
        if error.kind() == std::io::ErrorKind::InvalidInput {
            Ok(())
        } else {
            Err(error)
        }
    })?;
    Ok(())
}

pub fn require_success(
    spec: &CommandSpec,
    output: &Output,
    controlled_message: impl Into<String>,
) -> Result<()> {
    if output.status.success() {
        Ok(())
    } else {
        let mut message = controlled_message.into();
        if spec.report_stderr {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr = stderr.trim();
            if !stderr.is_empty() {
                message.push_str(": ");
                message.push_str(stderr);
            }
        }
        Err(Error::CommandFailed {
            program: spec.display_program(),
            status: output.status,
            message,
        })
    }
}

#[must_use]
pub fn os(name: impl AsRef<OsStr>) -> OsString {
    name.as_ref().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_environment_values() {
        let spec = CommandSpec::new("op")
            .args(["whoami"])
            .env("OP_SERVICE_ACCOUNT_TOKEN", "very-secret-value");
        let debug = format!("{spec:?}");
        assert!(debug.contains("OP_SERVICE_ACCOUNT_TOKEN"));
        assert!(!debug.contains("very-secret-value"));
    }

    #[test]
    fn system_runner_replaces_instead_of_extending_the_ambient_environment() {
        let spec = CommandSpec::new("env").env("FLEET_DEV_VISIBLE", "yes");
        let output = SystemCommandRunner.output(&spec).unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert_eq!(stdout, "FLEET_DEV_VISIBLE=yes\n");
        assert!(!stdout.contains("OP_SERVICE_ACCOUNT_TOKEN"));
        assert!(!stdout.contains("SSH_AUTH_SOCK"));
    }

    #[test]
    fn system_runner_enforces_output_limits_while_the_child_is_running() {
        let spec = CommandSpec::new("sh")
            .args([
                "-c",
                "while :; do printf xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; done",
            ])
            .timeout(Duration::from_secs(2))
            .output_limits(1024, 1024);
        let error = SystemCommandRunner.output(&spec).unwrap_err();
        assert!(matches!(error, Error::CommandLimit { .. }));
    }

    #[test]
    fn system_runner_rejects_fast_finite_output_over_the_limit() {
        let spec = CommandSpec::new("sh")
            .args(["-c", "head -c 8192 /dev/zero"])
            .output_limits(1024, 1024);
        let error = SystemCommandRunner.output(&spec).unwrap_err();
        assert!(matches!(error, Error::CommandLimit { .. }));
    }

    #[test]
    fn system_runner_does_not_hang_when_a_descendant_retains_the_pipes() {
        let spec = CommandSpec::new("sh")
            .args(["-c", "sleep 30 &"])
            .timeout(Duration::from_secs(2));
        let started = Instant::now();
        let error = SystemCommandRunner.output(&spec).unwrap_err();
        assert!(matches!(error, Error::CommandLimit { .. }));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn system_runner_enforces_deadlines_and_reaps_the_child() {
        let spec = CommandSpec::new("sh")
            .args(["-c", "sleep 30"])
            .timeout(Duration::from_millis(25));
        let error = SystemCommandRunner.output(&spec).unwrap_err();
        assert!(matches!(error, Error::CommandLimit { .. }));
    }
}
