// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What every `trawld --doctor` test runs the doctor with.
//!
//! [`run_doctor`] runs the real `trawld` binary as a child process with an
//! empty environment plus the variables the test names, so no ambient
//! `TRAWL_*`, `PG*` or proxy variable reaches it. While it runs, it watches
//! the doctor for child processes: the doctor starts none, and the
//! crash-dump monitor would be one. [`assert_no_values`] is the leak check
//! every doctor test applies to both output streams.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The password planted in every database URL a doctor test hands the
/// doctor. It must never appear in the doctor's output.
pub const SECRET: &str = "private-secret";

/// A Fleet database URL that carries [`SECRET`] and reaches nothing.
pub const PLANTED_FLEET_URL: &str = "postgres://user:private-secret@127.0.0.1:1/fleet";

/// An app-state database URL that carries [`SECRET`] and reaches nothing.
pub const PLANTED_APP_URL: &str = "postgres://user:private-secret@127.0.0.1:1/trawl";

/// Driver and OS error fragments. Any of them in the output means an
/// error's `Display` reached the report.
pub const ERROR_FRAGMENTS: &[&str] = &[
    "password authentication failed",
    "os error",
    "no such file",
    "connection refused",
    "permission denied (os",
    "error returned from database",
    "error communicating with database",
    "invalid peer certificate",
];

/// The longest a doctor run may take before the test calls it hung. Each
/// network step the doctor takes has its own deadline of at most 10 s.
const RUN_DEADLINE: Duration = Duration::from_secs(60);

/// A `trawld` command with an empty environment, except the loader's
/// library path, which the shared-DuckDB test build needs.
pub fn trawld() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_trawld"));
    command.env_clear();
    for name in ["LD_LIBRARY_PATH", "DYLD_FALLBACK_LIBRARY_PATH"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
}

/// What one doctor run printed, and what was seen of it while it ran.
#[derive(Debug)]
pub struct Observed {
    /// The exit status.
    pub code: i32,
    /// Everything it wrote to stdout.
    pub stdout: String,
    /// Everything it wrote to stderr.
    pub stderr: String,
    /// Whether some sample of `/proc/<pid>/status` showed `NoNewPrivs: 1`,
    /// which the crash-dump seal sets.
    pub sealed_seen: bool,
}

/// Run `trawld` with `args` and exactly the environment `env` (plus the
/// loader path), and return its exit status, stdout and stderr.
///
/// The working directory is `HOME` when `env` sets it, so relative paths in
/// a test's configuration resolve inside the test's own directory.
///
/// # Panics
/// As [`run_doctor_observed`].
pub fn run_doctor<A, K, V>(args: &[A], env: &[(K, V)]) -> (i32, String, String)
where
    A: AsRef<OsStr>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    let observed = run_doctor_observed(args, env);
    (observed.code, observed.stdout, observed.stderr)
}

/// [`run_doctor`], also returning what was seen of the process while it
/// ran.
///
/// # Panics
/// When the run does not finish within [`RUN_DEADLINE`], ends by a signal,
/// or was seen with a child process.
pub fn run_doctor_observed<A, K, V>(args: &[A], env: &[(K, V)]) -> Observed
where
    A: AsRef<OsStr>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    let mut command = trawld();
    command.args(args);
    for (name, value) in env {
        command.env(name, value);
        if name.as_ref() == "HOME" {
            command.current_dir(value.as_ref());
        }
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("trawld starts");

    // Drain both pipes on their own threads, so a report larger than a pipe
    // buffer cannot stall the doctor while this thread watches it.
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let out = std::thread::spawn(move || read_all(&mut stdout));
    let err = std::thread::spawn(move || read_all(&mut stderr));

    let mut watch = ChildWatch::new(child.id());
    let deadline = Instant::now() + RUN_DEADLINE;
    let status = loop {
        watch.sample();
        if let Some(status) = child.try_wait().expect("wait on trawld") {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().expect("kill a hung trawld");
            let _ = child.wait();
            panic!(
                "trawld {:?} did not exit within {RUN_DEADLINE:?}",
                args.iter().map(AsRef::as_ref).collect::<Vec<_>>()
            );
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    let stdout = out.join().expect("stdout reader");
    let stderr = err.join().expect("stderr reader");
    watch.assert_no_child(&stderr);

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        assert!(
            status.signal().is_none(),
            "trawld died by signal {:?}: {stderr}",
            status.signal()
        );
    }
    Observed {
        code: status.code().expect("an exit status"),
        stdout,
        stderr,
        sealed_seen: watch.sealed_seen,
    }
}

/// Whether this test process itself runs with `no_new_privs`, which its
/// children inherit, so that a child showing it proves nothing.
#[cfg(target_os = "linux")]
pub fn harness_has_no_new_privs() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .expect("/proc/self/status")
        .lines()
        .any(|line| line.split_whitespace().collect::<Vec<_>>() == ["NoNewPrivs:", "1"])
}

fn read_all(stream: &mut impl std::io::Read) -> String {
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).expect("read trawld output");
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Watches one process for children, on Linux through
/// `/proc/<pid>/task/*/children`.
struct ChildWatch {
    pid: u32,
    seen: std::collections::BTreeSet<u32>,
    sealed_seen: bool,
}

impl ChildWatch {
    const fn new(pid: u32) -> Self {
        Self {
            pid,
            seen: std::collections::BTreeSet::new(),
            sealed_seen: false,
        }
    }

    /// Record every child any of the process's threads has right now, and
    /// whether the process has `no_new_privs` set.
    fn sample(&mut self) {
        #[cfg(target_os = "linux")]
        {
            if let Ok(status) = std::fs::read_to_string(format!("/proc/{}/status", self.pid)) {
                self.sealed_seen |= status.lines().any(|line| {
                    line.split_whitespace().collect::<Vec<_>>() == ["NoNewPrivs:", "1"]
                });
            }
            let Ok(tasks) = std::fs::read_dir(format!("/proc/{}/task", self.pid)) else {
                return;
            };
            for task in tasks.flatten() {
                if let Ok(children) = std::fs::read_to_string(task.path().join("children")) {
                    self.seen.extend(
                        children
                            .split_whitespace()
                            .filter_map(|child| child.parse::<u32>().ok()),
                    );
                }
            }
        }
    }

    /// Fail when a child was seen, or when a crash-dump monitor naming the
    /// doctor as its parent is still alive after the run.
    fn assert_no_child(&self, stderr: &str) {
        assert!(
            self.seen.is_empty(),
            "trawld --doctor started child processes {:?}: {stderr}",
            self.seen
        );
        #[cfg(target_os = "linux")]
        {
            let marker = format!("TRAWL_CRASHDUMP_PARENT={}", self.pid);
            let procs = std::fs::read_dir("/proc").expect("/proc lists");
            for entry in procs.flatten() {
                let Ok(environ) = std::fs::read(entry.path().join("environ")) else {
                    continue;
                };
                let found = environ
                    .split(|b| *b == 0)
                    .any(|var| var == marker.as_bytes());
                assert!(
                    !found,
                    "a crash-dump monitor for trawld --doctor (pid {}) is running as pid {}",
                    self.pid,
                    entry.file_name().display()
                );
            }
        }
    }
}

/// Every string a doctor test planted that the doctor's output must not
/// contain, beyond [`SECRET`] and [`ERROR_FRAGMENTS`], which are always
/// checked: planted URLs, certificate names and fingerprints, catalog ids,
/// listener addresses.
///
/// # Panics
/// When either stream contains one of them. Error fragments are matched
/// without regard to case.
pub fn assert_no_values(stdout: &str, stderr: &str, planted: &[&str]) {
    for (name, stream) in [("stdout", stdout), ("stderr", stderr)] {
        for value in std::iter::once(&SECRET).chain(planted) {
            assert!(
                !value.is_empty() && !stream.contains(value),
                "{name} shows the planted value {value:?}:\n{stream}"
            );
        }
        let lower = stream.to_lowercase();
        for fragment in ERROR_FRAGMENTS {
            assert!(
                !lower.contains(fragment),
                "{name} shows driver or OS error text {fragment:?}:\n{stream}"
            );
        }
    }
}

/// The settings of a `trawld.toml` a doctor test writes.
#[derive(Debug, Clone)]
pub struct DoctorConfig {
    /// `[server] http_addr`.
    pub http_addr: String,
    /// `[data] path`.
    pub data_path: PathBuf,
    /// `[ingest] enabled`.
    pub ingest: bool,
    /// `[ingest] wal_dir`, when set.
    pub wal_dir: Option<PathBuf>,
    /// `[server] tls_cert_path` and `tls_key_path`, when set.
    pub tls: Option<(PathBuf, PathBuf)>,
    /// `[auth] database_url`, when set.
    pub fleet_url: Option<String>,
    /// `[storage] database_url`, when set.
    pub app_url: Option<String>,
    /// TOML appended as it is, for settings the fields above do not name.
    pub extra: String,
}

impl DoctorConfig {
    /// An ingest node with its data root at `dir/data` (not created), a
    /// listener on a loopback port nothing listens on, auto TLS, and no
    /// database URL in the file.
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            http_addr: "127.0.0.1:1".to_owned(),
            data_path: dir.join("data"),
            ingest: true,
            wal_dir: None,
            tls: None,
            fleet_url: None,
            app_url: None,
            extra: String::new(),
        }
    }
}

/// Write `config` as `dir/trawld.toml` and return its path.
pub fn write_doctor_config(dir: &Path, config: &DoctorConfig) -> PathBuf {
    use std::fmt::Write as _;
    let quote = |path: &Path| toml_string(&path.to_string_lossy());
    let mut document = String::new();
    let mut line = |text: String| writeln!(document, "{text}").expect("writing to a String");
    line("[server]".to_owned());
    line(format!("http_addr = {}", toml_string(&config.http_addr)));
    if let Some((cert, key)) = &config.tls {
        line(format!("tls_cert_path = {}", quote(cert)));
        line(format!("tls_key_path = {}", quote(key)));
    }
    line("[data]".to_owned());
    line(format!("path = {}", quote(&config.data_path)));
    line("[ingest]".to_owned());
    line(format!("enabled = {}", config.ingest));
    if let Some(wal) = &config.wal_dir {
        line(format!("wal_dir = {}", quote(wal)));
    }
    line("[auth]".to_owned());
    if let Some(url) = &config.fleet_url {
        line(format!("database_url = {}", toml_string(url)));
    }
    line("[storage]".to_owned());
    if let Some(url) = &config.app_url {
        line(format!("database_url = {}", toml_string(url)));
    }
    document.push_str(&config.extra);
    let path = dir.join("trawld.toml");
    std::fs::write(&path, document).expect("write trawld.toml");
    path
}

/// `value` as a TOML basic string.
fn toml_string(value: &str) -> String {
    let mut quoted = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            c if c.is_control() => {
                use std::fmt::Write as _;
                write!(quoted, "\\u{:04X}", u32::from(c)).expect("writing to a String");
            }
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

/// The environment of a doctor run in `home`: `HOME`, and the two planted
/// database URLs.
pub fn planted_env(home: &Path) -> Vec<(&'static str, OsString)> {
    vec![
        ("HOME", home.as_os_str().to_owned()),
        ("FLEET_DATABASE_URL", PLANTED_FLEET_URL.into()),
        ("TRAWL_DATABASE_URL", PLANTED_APP_URL.into()),
    ]
}

/// Parse a JSON report the doctor printed.
///
/// # Panics
/// When `stdout` is not one.
pub fn report(stdout: &str) -> trawl_api::doctor::Report {
    serde_json::from_str(stdout)
        .unwrap_or_else(|error| panic!("not a doctor report ({error}):\n{stdout}"))
}
