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
//! every doctor test applies to both output streams. The Postgres roles the
//! database-backed tests log in as, and the databases they mint, are here
//! too, so every group logs in the same way.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use sqlx::{Connection as _, Executor as _, PgConnection};
use trawl_api::doctor::{Check, Outcome, Report};

use crate::common;

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
    observe(command, args, env)
}

/// Run `command`, a doctor run as built by [`trawld`] or [`Userns`], with
/// `env` added, and watch it until it exits.
fn observe<A, K, V>(mut command: Command, args: &[A], env: &[(K, V)]) -> Observed
where
    A: AsRef<OsStr>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
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

/// The name that makes an unavailable user namespace a test failure rather
/// than a skip. CI sets it, so a privileged test that did not run there is
/// never counted as passing evidence.
pub const REQUIRE_USERNS: &str = "TRAWL_TEST_REQUIRE_USERNS";

/// Running the doctor as root inside an unprivileged user namespace, the
/// way the root-run and read-only-mount tests do: `unshare --user
/// --map-root-user --mount`, which maps this test's uid to 0, so the doctor
/// sees euid 0 and the files the test made as owned by root.
#[derive(Debug, Clone)]
pub struct Userns {
    unshare: PathBuf,
    mount: PathBuf,
    shell: PathBuf,
}

/// Inside the namespace: bind the directory `$2` onto itself read-only when
/// it is not empty, then exec the rest of the arguments from the same
/// working directory. `$1` is `mount`.
/// Any failure ends the run before the doctor starts, with a status the
/// doctor never exits with.
const USERNS_SCRIPT: &str = r#"set -e
mount="$1"
ro="$2"
shift 2
if [ -n "$ro" ]; then
    "$mount" --bind "$ro" "$ro"
    "$mount" -o remount,bind,ro "$ro"
    # A working directory opened before the mount still names the
    # writable tree; look it up again, through the read-only one.
    cd "$PWD"
fi
exec "$@"
"#;

impl Userns {
    /// The namespace tools, once a probe proved this host lets this user
    /// make the namespace, be root in it, and mount a directory read-only
    /// there; otherwise why not.
    ///
    /// # Errors
    /// The reason the namespace is unavailable.
    pub fn probe() -> Result<Self, String> {
        let find = |name: &str| {
            let path = std::env::var_os("PATH").unwrap_or_default();
            std::env::split_paths(&path)
                .chain(["/usr/bin", "/bin", "/usr/sbin", "/sbin"].map(PathBuf::from))
                .map(|dir| dir.join(name))
                .find(|candidate| candidate.is_file())
                .ok_or_else(|| format!("no {name} on PATH"))
        };
        let tools = Self {
            unshare: find("unshare")?,
            mount: find("mount")?,
            shell: find("sh")?,
        };
        let dir = tempfile::tempdir().map_err(|e| format!("a probe directory: {e}"))?;
        let probe = dir.path().join("probe");
        std::fs::write(&probe, b"probe").map_err(|e| format!("a probe file: {e}"))?;
        // As root in the namespace, a write to the read-only bind mount must
        // fail; `id -u` must say 0.
        let output = tools
            .command(Some(dir.path()))
            .args([
                tools.shell.as_os_str(),
                OsStr::new("-c"),
                OsStr::new(
                    r#"[ "$(id -u)" = 0 ] || exit 90; if ( : > "$1/probe" ) 2>/dev/null; then exit 91; fi"#,
                ),
                OsStr::new("sh"),
                dir.path().as_os_str(),
            ])
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .output()
            .map_err(|e| format!("{} does not run: {e}", tools.unshare.display()))?;
        match output.status.code() {
            Some(0) => {}
            Some(90) => return Err("the namespace's uid is not 0".to_owned()),
            Some(91) => return Err("the read-only bind mount took a write".to_owned()),
            _ => {
                return Err(format!(
                    "unshare --user --map-root-user --mount with a read-only bind mount \
                     failed ({}): {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
        }
        if std::fs::read(&probe).ok().as_deref() != Some(b"probe".as_slice()) {
            return Err("the read-only bind mount's file changed".to_owned());
        }
        Ok(tools)
    }

    /// [`Userns::probe`] for the test `test`: the tools, or `None` after
    /// printing `SKIPPED: ...` when the namespace is unavailable.
    ///
    /// # Panics
    /// When the namespace is unavailable and [`REQUIRE_USERNS`] is `1`.
    pub fn for_test(test: &str) -> Option<Self> {
        match Self::probe() {
            Ok(tools) => Some(tools),
            Err(why) if std::env::var_os(REQUIRE_USERNS).is_some_and(|v| v == "1") => {
                panic!("{test}: {REQUIRE_USERNS}=1, and a user namespace is unavailable: {why}")
            }
            Err(why) => {
                eprintln!("SKIPPED: {test}: a user namespace is unavailable here ({why})");
                None
            }
        }
    }

    /// `unshare` with the empty environment [`trawld`] runs with, set to
    /// run [`USERNS_SCRIPT`]'s arguments, with `read_only` bound read-only.
    fn command(&self, read_only: Option<&Path>) -> Command {
        let mut command = Command::new(&self.unshare);
        command.env_clear();
        for name in ["LD_LIBRARY_PATH", "DYLD_FALLBACK_LIBRARY_PATH"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
            .args(["--user", "--map-root-user", "--mount"])
            .arg(&self.shell)
            .args(["-c", USERNS_SCRIPT, "sh"])
            .arg(&self.mount)
            .arg(read_only.map_or_else(OsString::new, |dir| dir.as_os_str().to_owned()));
        command
    }
}

/// [`run_doctor`] as root in a user namespace, with `read_only`, when
/// given, bind-mounted read-only over itself for the run.
///
/// # Panics
/// As [`run_doctor_observed`], and when the namespace set-up fails after
/// [`Userns::probe`] succeeded.
pub fn run_doctor_in_userns<A, K, V>(
    userns: &Userns,
    read_only: Option<&Path>,
    args: &[A],
    env: &[(K, V)],
) -> (i32, String, String)
where
    A: AsRef<OsStr>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    let mut command = userns.command(read_only);
    command.arg(env!("CARGO_BIN_EXE_trawld")).args(args);
    let observed = observe(command, args, env);
    assert!(
        matches!(observed.code, 0..=3),
        "the user namespace set-up failed (exit {}): {}",
        observed.code,
        observed.stderr
    );
    (observed.code, observed.stdout, observed.stderr)
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
    /// whether the process has `no_new_privs` set, once it is trawld: a
    /// user-namespace run starts as `unshare` and a shell, whose `mount`
    /// children are the set-up, not the doctor's.
    fn sample(&mut self) {
        #[cfg(target_os = "linux")]
        {
            let comm = std::fs::read_to_string(format!("/proc/{}/comm", self.pid));
            if comm.ok().as_deref().map(str::trim_end) != Some("trawld") {
                return;
            }
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
/// Whatever a test plants, the output is also checked for the shapes of
/// the values no report may show ([`forbidden_shape`]): a catalog id, a
/// certificate fingerprint, a listener address. So a test that did not
/// think to plant its catalog id or its listener's port is still covered.
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
        if let Some((shape, found)) = forbidden_shape(stream) {
            panic!("{name} shows {shape} {found:?}:\n{stream}");
        }
    }
}

/// The first value in `text` shaped like one no doctor report may show,
/// named, or `None`:
///
/// - a UUID, the form of every catalog id;
/// - 32 or more hex digits in a row, or 8 or more hex pairs joined by
///   colons, the forms of a certificate fingerprint;
/// - a dotted IPv4 address, with or without a port, or a bracketed IPv6
///   address, the forms of a listener address and a certificate's IP SAN.
pub fn forbidden_shape(text: &str) -> Option<(&'static str, String)> {
    let bytes = text.as_bytes();
    let hex = |b: u8| b.is_ascii_hexdigit();
    for start in 0..bytes.len() {
        // Only look where a token starts, so a match is not the tail of a
        // longer run already judged.
        if start > 0 && bytes[start - 1].is_ascii_alphanumeric() {
            continue;
        }
        let rest = &bytes[start..];
        let run = rest.iter().take_while(|b| hex(**b)).count();
        if run >= 32 {
            return Some(("a hex fingerprint", text[start..start + run].to_owned()));
        }
        if let Some(len) = uuid_at(rest) {
            return Some(("a catalog id", text[start..start + len].to_owned()));
        }
        if let Some(len) = colon_hex_at(rest) {
            return Some((
                "a colon-hex fingerprint",
                text[start..start + len].to_owned(),
            ));
        }
        if let Some(len) = ipv4_at(rest) {
            return Some(("an IPv4 address", text[start..start + len].to_owned()));
        }
        if rest.first() == Some(&b'[') {
            let inner = rest[1..]
                .iter()
                .take_while(|b| b.is_ascii_alphanumeric() || matches!(**b, b':' | b'.' | b'%'))
                .count();
            if inner >= 2 && rest.get(1 + inner) == Some(&b']') && rest[1..=inner].contains(&b':') {
                return Some(("an IPv6 address", text[start..start + inner + 2].to_owned()));
            }
        }
    }
    None
}

/// The length of the UUID `bytes` starts with (8-4-4-4-12 hex digits).
fn uuid_at(bytes: &[u8]) -> Option<usize> {
    let mut at = 0;
    for (index, group) in [8, 4, 4, 4, 12].into_iter().enumerate() {
        if index > 0 {
            (bytes.get(at) == Some(&b'-')).then_some(())?;
            at += 1;
        }
        let digits = bytes[at.min(bytes.len())..]
            .iter()
            .take_while(|b| b.is_ascii_hexdigit())
            .count();
        (digits == group).then_some(())?;
        at += group;
    }
    Some(at)
}

/// The length of the colon-joined hex pairs `bytes` starts with, when
/// there are at least 8 of them.
fn colon_hex_at(bytes: &[u8]) -> Option<usize> {
    let mut pairs = 0;
    let mut at = 0;
    loop {
        let pair = bytes.get(at..at + 2)?;
        if !pair.iter().all(u8::is_ascii_hexdigit) {
            break;
        }
        pairs += 1;
        at += 2;
        if bytes.get(at) == Some(&b':') && bytes.get(at + 1).is_some_and(u8::is_ascii_hexdigit) {
            at += 1;
        } else {
            break;
        }
    }
    (pairs >= 8).then_some(at)
}

/// The length of the dotted IPv4 address, and its port if one follows,
/// that `bytes` starts with.
fn ipv4_at(bytes: &[u8]) -> Option<usize> {
    let mut at = 0;
    for octet in 0..4 {
        if octet > 0 {
            (bytes.get(at) == Some(&b'.')).then_some(())?;
            at += 1;
        }
        let digits = bytes[at.min(bytes.len())..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count();
        (1..=3).contains(&digits).then_some(())?;
        at += digits;
    }
    if bytes.get(at).is_some_and(u8::is_ascii_alphanumeric) || bytes.get(at) == Some(&b'.') {
        return None;
    }
    if bytes.get(at) == Some(&b':') {
        at += 1 + bytes[at + 1..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count();
    }
    Some(at)
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

// -- Postgres roles and databases ------------------------------------------
//
// The roles every database-backed doctor test logs in as, and the databases
// they log in to. Shared by `db` and `proof`.

/// The login role every doctor run here uses. Its password is [`SECRET`];
/// it inherits the admin role's privileges on the test databases.
pub const ROLE: &str = "trawl_doctor_planted";

/// A second login role, also with the password [`SECRET`], that is a
/// member of no role: it holds only what a test database grants it or
/// `PUBLIC`. [`forbid_advisory_locks`] leaves it no advisory-lock function.
pub const LOCKLESS: &str = "trawl_doctor_lockless";

/// Make sure [`ROLE`] exists with the planted password and the admin
/// role's privileges, and [`LOCKLESS`] with the planted password and no
/// membership. Idempotent, and serialized across the test processes that
/// share the cluster by a transaction-scoped lock on the admin connection
/// (the test's, never the doctor's).
pub async fn ensure_role() {
    let mut admin = PgConnection::connect(&common::admin_database_url())
        .await
        .expect("connect to the admin database");
    let mut tx = admin.begin().await.expect("begin");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('trawl_doctor_planted'))")
        .execute(&mut *tx)
        .await
        .expect("serialize role setup");
    for role in [ROLE, LOCKLESS] {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1)")
                .bind(role)
                .fetch_one(&mut *tx)
                .await
                .expect("look up the role");
        if !exists {
            tx.execute(sqlx::AssertSqlSafe(format!("CREATE ROLE {role} LOGIN")))
                .await
                .expect("create the planted role");
        }
        tx.execute(sqlx::AssertSqlSafe(format!(
            "ALTER ROLE {role} LOGIN NOSUPERUSER PASSWORD '{SECRET}'"
        )))
        .await
        .expect("set the planted password");
    }
    let admin_role: String = sqlx::query_scalar("SELECT current_user::text")
        .fetch_one(&mut *tx)
        .await
        .expect("admin role");
    tx.execute(sqlx::AssertSqlSafe(format!(
        r#"GRANT "{admin_role}" TO {ROLE}"#
    )))
    .await
    .expect("let the planted role read what the admin role owns");
    let memberships: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_auth_members m JOIN pg_roles r ON r.oid = m.member
         WHERE r.rolname = $1",
    )
    .bind(LOCKLESS)
    .fetch_one(&mut *tx)
    .await
    .expect("look up the lockless role's memberships");
    assert_eq!(memberships, 0, "{LOCKLESS} must inherit nothing");
    tx.commit().await.expect("commit role setup");
    admin.close().await.expect("close");
}

/// The advisory-lock functions: every `pg_advisory*` and
/// `pg_try_advisory*` function, including the unlocks.
pub const ADVISORY_FUNCTIONS: &str =
    "SELECT oid FROM pg_proc WHERE proname ~ '^pg_(try_)?advisory'";

/// In the database `url` names: revoke `EXECUTE` on every advisory-lock
/// function from `PUBLIC`, grant it back to the admin role, which trawld
/// runs as here, and let [`LOCKLESS`] read every table. Function
/// privileges live in each database's own `pg_proc`, so this changes no
/// other database. Asserts afterwards that [`LOCKLESS`] may execute none
/// of them, so any call it makes is refused with `insufficient_privilege`.
pub async fn forbid_advisory_locks(url: &str) {
    let mut conn = admin(url).await;
    conn.execute(sqlx::AssertSqlSafe(format!(
        "DO $$ DECLARE f regprocedure; BEGIN
            FOR f IN SELECT oid::regprocedure FROM pg_proc WHERE oid IN ({ADVISORY_FUNCTIONS}) LOOP
                EXECUTE format('REVOKE EXECUTE ON FUNCTION %s FROM PUBLIC', f);
                EXECUTE format('GRANT EXECUTE ON FUNCTION %s TO %I', f, current_user);
            END LOOP;
        END $$"
    )))
    .await
    .expect("revoke the advisory-lock functions from PUBLIC");
    conn.execute(sqlx::AssertSqlSafe(format!(
        "GRANT SELECT ON ALL TABLES IN SCHEMA public TO {LOCKLESS}"
    )))
    .await
    .expect("let the lockless role read the ledger and catalog_state");
    let (total, allowed): (i64, i64) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT count(*), count(*) FILTER (WHERE has_function_privilege($1, oid, 'EXECUTE'))
         FROM pg_proc WHERE oid IN ({ADVISORY_FUNCTIONS})"
    )))
    .bind(LOCKLESS)
    .fetch_one(&mut conn)
    .await
    .expect("count the advisory-lock functions");
    // Postgres 18 has 21: lock, lock_shared, unlock, unlock_shared,
    // xact_lock and xact_lock_shared in both key forms, their try_
    // forms, and unlock_all.
    assert!(total >= 21, "only {total} advisory-lock functions found");
    assert_eq!(allowed, 0, "{LOCKLESS} may still take an advisory lock");
    conn.close().await.unwrap();
}

/// `url` as [`LOCKLESS`] with the planted password in it.
pub fn lockless(url: &str) -> String {
    with_login(url, LOCKLESS, Some(SECRET))
}

/// Every database row of `report` is `complete`: what a doctor run as
/// [`LOCKLESS`] shows when it calls no advisory-lock function. A call is
/// refused with `insufficient_privilege`, which turns the row that made
/// it `not_sampled`, `permission_denied`.
pub fn database_rows_complete(report: &Report) -> Result<(), String> {
    for id in [
        "server.fleet.connect",
        "server.fleet.schema",
        "server.app.connect",
        "server.app.schema",
        "server.app.writer",
    ] {
        let check = row(report, id);
        if check.outcome != Outcome::Complete {
            return Err(format!(
                "{id} is {:?} ({:?})",
                check.outcome,
                check.reason.as_deref()
            ));
        }
    }
    Ok(())
}

/// `url` (an admin URL naming a test database) with its userinfo replaced
/// by `user` and, when given, `password`.
pub fn with_login(url: &str, user: &str, password: Option<&str>) -> String {
    let (scheme, rest) = url.split_once("://").expect("a URL with a scheme");
    let host_at = rest.find('/').unwrap_or(rest.len());
    let authority = &rest[..host_at];
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let login = match password {
        Some(password) => format!("{user}:{password}"),
        None => user.to_owned(),
    };
    format!("{scheme}://{login}@{host}{}", &rest[host_at..])
}

/// `url` as [`ROLE`] with the planted password in it.
pub fn planted(url: &str) -> String {
    with_login(url, ROLE, Some(SECRET))
}

/// What the report must not show of a database URL: the URL, its host and
/// port, its database name, and the role.
pub fn url_values(url: &str) -> Vec<String> {
    let rest = url.split_once("://").unwrap().1;
    let authority = &rest[..rest.find('/').unwrap_or(rest.len())];
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let database = rest[rest.find('/').unwrap() + 1..]
        .split('?')
        .next()
        .unwrap();
    vec![
        url.to_owned(),
        host.to_owned(),
        database.to_owned(),
        ROLE.to_owned(),
        LOCKLESS.to_owned(),
    ]
}

/// A connection to `url` as the admin role.
pub async fn admin(url: &str) -> PgConnection {
    PgConnection::connect(url)
        .await
        .expect("connect to a test database")
}

/// A test database with the Fleet schema current.
pub async fn migrated_fleet() -> String {
    let url = common::create_fleet_database().await;
    let mut conn = admin(&url).await;
    fleet_auth::MIGRATOR
        .run(&mut conn)
        .await
        .expect("migrate the Fleet database");
    conn.close().await.unwrap();
    url
}

/// A test database with the app-state schema current.
pub async fn migrated_app() -> String {
    let url = common::create_app_database().await;
    let mut conn = admin(&url).await;
    trawl_server::store::migrations::MIGRATOR
        .run(&mut conn)
        .await
        .expect("migrate the app-state database");
    conn.close().await.unwrap();
    url
}

pub fn row<'a>(report: &'a Report, id: &str) -> &'a Check {
    report
        .checks()
        .iter()
        .find(|check| check.id == id)
        .unwrap_or_else(|| panic!("no {id} row: {report:?}"))
}

/// `(outcome, reason)` of one row.
pub fn verdict<'a>(report: &'a Report, id: &str) -> (Outcome, Option<&'a str>) {
    let check = row(report, id);
    (check.outcome, check.reason.as_deref())
}

/// The rows of an installation before its first start, on an ingest node
/// with auto TLS and trawld not running (#269 AC3): boot initializes the
/// app-state schema, creates the data root and writes its epoch, and
/// generates the certificate, so each of those is `complete`,
/// `will_initialize`; nothing listens yet.
///
/// # Panics
/// When a row differs.
pub fn assert_fresh_install_rows(report: &Report) {
    for id in [
        "server.app.schema",
        "server.data.root",
        "server.data.epoch",
        "server.tls.material",
    ] {
        assert_eq!(
            verdict(report, id),
            (Outcome::Complete, Some("will_initialize")),
            "{id}: {report:#?}"
        );
    }
    assert_eq!(
        verdict(report, "server.listener.identity"),
        (Outcome::NotSampled, Some("not_listening")),
        "{report:#?}"
    );
}
