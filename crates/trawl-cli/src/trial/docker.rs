// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Docker driver: the only place the trial spawns `docker`.
//!
//! Every call is an argv vector handed to `tokio::process`, never a shell
//! string on the host. Children are killed when their future is dropped,
//! run with every `COMPOSE_*` variable removed from their environment (so
//! an inherited `COMPOSE_PROFILES`, `COMPOSE_FILE`, or
//! `COMPOSE_PROJECT_NAME` cannot redirect the project), and are bounded by
//! a timeout and by capture limits.
//!
//! Two ways to run a command:
//!
//! - [`Docker::stream`] hands the child our stderr for both of its output
//!   streams. It is for the long, human-facing steps: `docker pull`,
//!   `compose up --wait`, and `compose stop`.
//! - [`Docker::output`] and [`Docker::capture`] buffer both streams. Every
//!   query the trial parses and every call that carries a secret runs this
//!   way.
//!
//! A buffered call is [`Sensitivity::Diagnose`] or
//! [`Sensitivity::Sealed`]. A sealed call carried a secret on stdin, or
//! its stdout is a secret, or its output can echo one (`psql` quotes a
//! failing statement, `fleet-admin keys create` prints the token). Its
//! stderr is discarded, and its failure reports the exit status only. Its
//! stdout still reaches the caller, in a buffer that is zeroed when
//! dropped.
//!
//! Errors name the command through [`Args`], which shows every argument
//! except those added with [`Args::withheld`], so a key prefix passed to
//! `keys revoke` never reaches an error message.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::os::fd::AsFd as _;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::Command;
use zeroize::Zeroizing;

/// The program the driver runs.
const DOCKER: &str = "docker";

/// A buffered command's stdout above this is refused, not truncated: a
/// truncated listing would parse as a shorter, wrong one.
const MAX_STDOUT: usize = 8 * 1024 * 1024;

/// A buffered command's stderr is kept up to this, then drained unread.
const MAX_STDERR: usize = 64 * 1024;

/// A failure message quotes at most this much of a diagnosable stderr.
const MAX_DETAIL: usize = 2000;

/// Timeout for a read-only query: `version`, `context inspect`, listings.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// Whether a buffered command's output may appear in an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sensitivity {
    /// The output holds no secret; a failure quotes its stderr.
    Diagnose,
    /// The command carried or produced a secret. Its stderr is discarded
    /// and a failure reports the exit status only.
    Sealed,
}

/// A docker argument vector and the way errors show it.
#[derive(Clone, Default)]
pub struct Args {
    argv: Vec<OsString>,
    shown: Vec<String>,
}

impl Args {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append an argument that errors may show.
    #[must_use]
    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        let arg = arg.as_ref();
        self.shown.push(arg.to_string_lossy().into_owned());
        self.argv.push(arg.to_owned());
        self
    }

    /// Append arguments that errors may show.
    #[must_use]
    pub fn args<I>(self, args: I) -> Self
    where
        I: IntoIterator,
        I::Item: AsRef<OsStr>,
    {
        args.into_iter().fold(self, Self::arg)
    }

    /// Append an argument that errors show as `<withheld>`, such as a key
    /// prefix.
    #[must_use]
    pub fn withheld(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.shown.push("<withheld>".to_owned());
        self.argv.push(arg.as_ref().to_owned());
        self
    }

    /// Append every argument of `rest`, keeping what each one withholds.
    #[must_use]
    pub fn then(mut self, rest: Self) -> Self {
        self.argv.extend(rest.argv);
        self.shown.extend(rest.shown);
        self
    }

    pub fn argv(&self) -> &[OsString] {
        &self.argv
    }

    /// The command as errors show it.
    pub fn display(&self) -> String {
        std::iter::once(DOCKER)
            .chain(self.shown.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Only the shown form, so `{:?}` cannot print a withheld argument.
impl fmt::Debug for Args {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Args").field(&self.display()).finish()
    }
}

/// What a buffered command left behind.
pub struct Output {
    pub status: ExitStatus,
    /// Zeroed on drop: it can be a token.
    pub stdout: Zeroizing<Vec<u8>>,
    /// Empty for a [`Sensitivity::Sealed`] call.
    pub stderr: Vec<u8>,
}

impl fmt::Debug for Output {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Output")
            .field("status", &self.status)
            .field("stdout_len", &self.stdout.len())
            .field("stderr_len", &self.stderr.len())
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DockerError {
    #[error(
        "the `docker` command was not found. The trial needs Docker Engine with the \
         Compose v2 plugin, version 2.20 or later"
    )]
    NotInstalled,

    #[error("could not run `{command}`: {source}")]
    Spawn { command: String, source: io::Error },

    #[error("`{command}` failed ({status}){}", detail.as_deref().map(|d| format!(": {d}")).unwrap_or_default())]
    Failed {
        command: String,
        status: String,
        detail: Option<String>,
    },

    #[error("`{command}` failed ({status}); its output is withheld because it can carry a secret")]
    SealedFailed { command: String, status: String },

    #[error("`{command}` did not finish within {secs}s and was stopped")]
    TimedOut { command: String, secs: u64 },

    #[error("`{command}` wrote more than {limit} bytes to stdout")]
    OutputTooLarge { command: String, limit: usize },
}

/// Runs `docker`, and `docker compose` for the trial project.
#[derive(Debug, Clone)]
pub struct Docker {
    program: OsString,
    /// Arguments before every argv: empty for `docker`; tests run a stub
    /// script through `/bin/sh` with the script path here.
    lead: Vec<OsString>,
    /// The trial directory: Compose's project directory, holding
    /// `compose.json`.
    dir: PathBuf,
}

impl Docker {
    /// The driver for the trial whose host directory is `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            program: DOCKER.into(),
            lead: Vec::new(),
            dir: dir.into(),
        }
    }

    /// The rendered Compose file.
    pub fn compose_file(&self) -> PathBuf {
        self.dir.join(super::compose::COMPOSE_FILE_NAME)
    }

    /// `docker compose` for the trial project, followed by `rest`.
    ///
    /// The project name, file, and directory are always explicit, so
    /// neither the working directory nor an inherited variable picks them.
    pub fn compose(&self, rest: Args) -> Args {
        Args::new()
            .args(["compose", "--project-name", super::PROJECT, "--file"])
            .arg(self.compose_file())
            .arg("--project-directory")
            .arg(&self.dir)
            .then(rest)
    }

    /// Run a command with both output streams buffered, whatever its exit
    /// status. `stdin`, when given, is written and then closed.
    pub async fn output(
        &self,
        args: &Args,
        stdin: Option<&[u8]>,
        sensitivity: Sensitivity,
        timeout: Duration,
    ) -> Result<Output, DockerError> {
        let mut command = self.command(args);
        command
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = spawn(&mut command, args)?;
        let child_stdin = child.stdin.take();
        let child_stdout = child.stdout.take().expect("stdout is piped");
        let child_stderr = child.stderr.take().expect("stderr is piped");

        let work = async {
            let feed = async {
                if let (Some(mut pipe), Some(bytes)) = (child_stdin, stdin) {
                    // A child that exits without reading closes the pipe;
                    // its exit status reports that, not this write.
                    let _ = pipe.write_all(bytes).await;
                    let _ = pipe.shutdown().await;
                }
            };
            let ((), stdout, stderr) = tokio::join!(
                feed,
                read_stdout(child_stdout),
                read_stderr(child_stderr, sensitivity),
            );
            let status = child.wait().await;
            (status, stdout, stderr)
        };
        let finished = tokio::time::timeout(timeout, work).await;
        let Ok((status, stdout, stderr)) = finished else {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(DockerError::TimedOut {
                command: args.display(),
                secs: timeout.as_secs(),
            });
        };

        let pipe_error = |source| DockerError::Spawn {
            command: args.display(),
            source,
        };
        let status = status.map_err(pipe_error)?;
        let stdout = stdout
            .map_err(pipe_error)?
            .ok_or(DockerError::OutputTooLarge {
                command: args.display(),
                limit: MAX_STDOUT,
            })?;
        let stderr = stderr.map_err(pipe_error)?;
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }

    /// [`Self::output`], refusing a non-zero exit status.
    pub async fn capture(
        &self,
        args: &Args,
        stdin: Option<&[u8]>,
        sensitivity: Sensitivity,
        timeout: Duration,
    ) -> Result<Output, DockerError> {
        let output = self.output(args, stdin, sensitivity, timeout).await?;
        if output.status.success() {
            return Ok(output);
        }
        Err(failure(args, output.status, &output.stderr, sensitivity))
    }

    /// Run a command with both of its output streams on our stderr, and
    /// refuse a non-zero exit status.
    pub async fn stream(&self, args: &Args, timeout: Duration) -> Result<(), DockerError> {
        let to_stderr = || {
            io::stderr()
                .as_fd()
                .try_clone_to_owned()
                .map(Stdio::from)
                .map_err(|source| DockerError::Spawn {
                    command: args.display(),
                    source,
                })
        };
        let mut command = self.command(args);
        command
            .stdin(Stdio::null())
            .stdout(to_stderr()?)
            .stderr(to_stderr()?);
        let mut child = spawn(&mut command, args)?;
        let finished = tokio::time::timeout(timeout, child.wait()).await;
        let Ok(status) = finished else {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(DockerError::TimedOut {
                command: args.display(),
                secs: timeout.as_secs(),
            });
        };
        let status = status.map_err(|source| DockerError::Spawn {
            command: args.display(),
            source,
        })?;
        if status.success() {
            return Ok(());
        }
        // The output is already on the terminal above this message.
        Err(DockerError::Failed {
            command: args.display(),
            status: describe(status),
            detail: None,
        })
    }

    fn command(&self, args: &Args) -> Command {
        let mut command = Command::new(&self.program);
        command
            .args(&self.lead)
            .args(args.argv())
            .kill_on_drop(true);
        for name in compose_variables(std::env::vars_os().map(|(name, _)| name)) {
            command.env_remove(name);
        }
        command
    }
}

#[cfg(test)]
impl Docker {
    /// A driver that runs `script` through `/bin/sh` in place of docker.
    pub(crate) fn stub(script: &std::path::Path, dir: impl Into<PathBuf>) -> Self {
        Self {
            program: "/bin/sh".into(),
            lead: vec![script.as_os_str().to_owned()],
            dir: dir.into(),
        }
    }
}

/// The inherited variables to remove from a docker child: every
/// `COMPOSE_*`, because Compose reads them as project settings.
fn compose_variables(names: impl Iterator<Item = OsString>) -> Vec<OsString> {
    names
        .filter(|name| name.as_encoded_bytes().starts_with(b"COMPOSE_"))
        .collect()
}

fn spawn(command: &mut Command, args: &Args) -> Result<tokio::process::Child, DockerError> {
    command.spawn().map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound {
            DockerError::NotInstalled
        } else {
            DockerError::Spawn {
                command: args.display(),
                source,
            }
        }
    })
}

/// The error for a non-zero exit status.
pub fn failure(
    args: &Args,
    status: ExitStatus,
    stderr: &[u8],
    sensitivity: Sensitivity,
) -> DockerError {
    match sensitivity {
        Sensitivity::Sealed => DockerError::SealedFailed {
            command: args.display(),
            status: describe(status),
        },
        Sensitivity::Diagnose => DockerError::Failed {
            command: args.display(),
            status: describe(status),
            detail: detail(stderr),
        },
    }
}

fn describe(status: ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt as _;
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exit status {code}"),
        (None, Some(signal)) => format!("killed by signal {signal}"),
        (None, None) => "unknown exit status".to_owned(),
    }
}

/// The tail of a diagnosable stderr, trimmed and bounded.
fn detail(stderr: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let chars = text.chars().count();
    Some(if chars > MAX_DETAIL {
        let tail: String = text.chars().skip(chars - MAX_DETAIL).collect();
        format!("…{tail}")
    } else {
        text.to_owned()
    })
}

/// Read stdout into a buffer that is zeroed on drop, including every
/// buffer it outgrows. `None` when it exceeds [`MAX_STDOUT`]; the rest is
/// drained so the child never blocks on a full pipe.
async fn read_stdout(mut pipe: impl AsyncRead + Unpin) -> io::Result<Option<Zeroizing<Vec<u8>>>> {
    let mut out = Zeroizing::new(Vec::new());
    let mut chunk = Zeroizing::new(vec![0u8; 8192]);
    let mut overflow = false;
    loop {
        let n = pipe.read(&mut chunk[..]).await?;
        if n == 0 {
            break;
        }
        if overflow || out.len() + n > MAX_STDOUT {
            overflow = true;
            continue;
        }
        if out.len() + n > out.capacity() {
            // Grow by copying into a fresh buffer, so the old allocation is
            // zeroed on drop instead of freed with its bytes by `Vec`.
            let mut grown =
                Zeroizing::new(Vec::with_capacity((out.len() + n).max(2 * out.capacity())));
            grown.extend_from_slice(&out);
            out = grown;
        }
        out.extend_from_slice(&chunk[..n]);
    }
    Ok((!overflow).then_some(out))
}

/// Read stderr up to [`MAX_STDERR`] and drain the rest. A sealed call's
/// stderr is read into a zeroed buffer and dropped.
async fn read_stderr(
    mut pipe: impl AsyncRead + Unpin,
    sensitivity: Sensitivity,
) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut chunk = Zeroizing::new(vec![0u8; 8192]);
    loop {
        let n = pipe.read(&mut chunk[..]).await?;
        if n == 0 {
            break;
        }
        if sensitivity == Sensitivity::Diagnose && out.len() < MAX_STDERR {
            let keep = n.min(MAX_STDERR - out.len());
            out.extend_from_slice(&chunk[..keep]);
        }
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    /// Write a stub docker script; the driver runs it through `/bin/sh`,
    /// so it needs no exec bit and cannot race a concurrent fork into
    /// "text file busy".
    pub(crate) fn stub(body: &str) -> (tempfile::TempDir, Docker) {
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("docker.sh");
        std::fs::write(&script, format!("set -u\n{body}\n")).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o600)).unwrap();
        let docker = Docker::stub(&script, tmp.path().join("trial"));
        (tmp, docker)
    }

    fn args(argv: &[&str]) -> Args {
        Args::new().args(argv)
    }

    #[tokio::test]
    async fn stdin_reaches_the_child_and_stdout_comes_back() {
        let (_tmp, docker) = stub(r#"printf 'argv:%s|' "$@"; cat"#);
        let out = docker
            .capture(
                &args(&["a b", "c"]),
                Some(b"piped"),
                Sensitivity::Diagnose,
                PROBE_TIMEOUT,
            )
            .await
            .unwrap();
        assert_eq!(&out.stdout[..], b"argv:a b|argv:c|piped");
    }

    #[tokio::test]
    async fn a_diagnosable_failure_quotes_stderr() {
        let (_tmp, docker) = stub("echo 'no such service: web' >&2; exit 3");
        let err = docker
            .capture(
                &args(&["compose", "up"]),
                None,
                Sensitivity::Diagnose,
                PROBE_TIMEOUT,
            )
            .await
            .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("docker compose up"), "{text}");
        assert!(text.contains("exit status 3"), "{text}");
        assert!(text.contains("no such service: web"), "{text}");
    }

    #[tokio::test]
    async fn a_sealed_failure_shows_neither_stream_nor_withheld_arguments() {
        let (_tmp, docker) =
            stub("cat >/dev/null; echo 'secret-stdout'; echo 'secret-stderr' >&2; exit 1");
        let call = args(&["keys", "revoke"]).withheld("pfx12345").arg("--yes");
        let err = docker
            .capture(
                &call,
                Some(b"secret-stdin"),
                Sensitivity::Sealed,
                PROBE_TIMEOUT,
            )
            .await
            .unwrap_err();
        let text = format!("{err} {err:?}");
        for secret in ["secret-stdout", "secret-stderr", "secret-stdin", "pfx12345"] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
        assert!(
            text.contains("docker keys revoke <withheld> --yes"),
            "{text}"
        );
        assert!(text.contains("exit status 1"), "{text}");
    }

    #[tokio::test]
    async fn a_sealed_call_returns_stdout_but_no_stderr() {
        let (_tmp, docker) = stub("echo token; echo metadata >&2");
        let out = docker
            .capture(&args(&[]), None, Sensitivity::Sealed, PROBE_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(&out.stdout[..], b"token\n");
        assert!(out.stderr.is_empty());
        assert!(!format!("{out:?}").contains("token"));
    }

    #[tokio::test]
    async fn output_returns_a_failed_status_without_erroring() {
        let (_tmp, docker) = stub("echo partial; exit 1");
        let out = docker
            .output(&args(&[]), None, Sensitivity::Diagnose, PROBE_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(out.status.code(), Some(1));
        assert_eq!(&out.stdout[..], b"partial\n");
    }

    #[tokio::test]
    async fn a_slow_command_is_killed_at_the_timeout() {
        let (_tmp, docker) = stub("exec sleep 30");
        let started = std::time::Instant::now();
        let err = docker
            .output(
                &args(&["pull"]),
                None,
                Sensitivity::Diagnose,
                Duration::from_millis(200),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, DockerError::TimedOut { .. }), "{err:?}");
        assert!(started.elapsed() < Duration::from_secs(10));

        let err = docker
            .stream(&args(&["pull"]), Duration::from_millis(200))
            .await
            .unwrap_err();
        assert!(matches!(err, DockerError::TimedOut { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn oversized_stdout_is_refused_not_truncated() {
        let (_tmp, docker) = stub(&format!("head -c {} /dev/zero", MAX_STDOUT + 1));
        let err = docker
            .output(&args(&["ps"]), None, Sensitivity::Diagnose, PROBE_TIMEOUT)
            .await
            .unwrap_err();
        assert!(matches!(err, DockerError::OutputTooLarge { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn stderr_is_bounded_and_drained() {
        let (_tmp, docker) = stub(&format!(
            "head -c {} /dev/zero >&2; echo done",
            4 * MAX_STDERR
        ));
        let out = docker
            .capture(&args(&[]), None, Sensitivity::Diagnose, PROBE_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(out.stderr.len(), MAX_STDERR);
        assert_eq!(&out.stdout[..], b"done\n");
    }

    #[tokio::test]
    async fn stream_refuses_a_non_zero_exit() {
        let (_tmp, docker) = stub("exit 0");
        docker
            .stream(&args(&["stop"]), PROBE_TIMEOUT)
            .await
            .unwrap();
        let (_tmp, docker) = stub("exit 4");
        let err = docker
            .stream(&args(&["stop"]), PROBE_TIMEOUT)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("exit status 4"), "{err}");
    }

    #[tokio::test]
    async fn a_missing_program_is_named_as_missing_docker() {
        let docker = Docker {
            program: "/nonexistent/docker".into(),
            lead: Vec::new(),
            dir: PathBuf::from("/nonexistent"),
        };
        let err = docker
            .output(
                &args(&["version"]),
                None,
                Sensitivity::Diagnose,
                PROBE_TIMEOUT,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, DockerError::NotInstalled), "{err:?}");
        assert!(err.to_string().contains("Compose v2 plugin, version 2.20"));
    }

    #[test]
    fn compose_arguments_pin_the_project_file_and_directory() {
        let docker = Docker::new("/state/trawl/trial");
        let call = docker.compose(Args::new().args(["up", "-d"]));
        assert_eq!(
            call.display(),
            "docker compose --project-name trawl-trial --file /state/trawl/trial/compose.json \
             --project-directory /state/trawl/trial up -d"
        );
    }

    #[test]
    fn every_compose_variable_is_removed_and_nothing_else() {
        let names = [
            "COMPOSE_PROFILES",
            "COMPOSE_FILE",
            "COMPOSE_PROJECT_NAME",
            "COMPOSE_ENV_FILES",
            "DOCKER_HOST",
            "PATH",
            "MY_COMPOSE_THING",
        ]
        .map(OsString::from);
        let removed = compose_variables(names.into_iter());
        assert_eq!(
            removed,
            [
                "COMPOSE_PROFILES",
                "COMPOSE_FILE",
                "COMPOSE_PROJECT_NAME",
                "COMPOSE_ENV_FILES"
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn debug_never_shows_a_withheld_argument() {
        let call = Args::new().arg("revoke").withheld("pfx12345");
        assert!(!format!("{call:?}").contains("pfx12345"));
        assert_eq!(call.argv()[1], OsString::from("pfx12345"));
    }
}
