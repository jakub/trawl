// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What every `trawl-web --doctor` test runs the doctor with.
//!
//! [`run_web_doctor`] runs the real `trawl-web` binary as a child process
//! with an empty environment plus the variables the test names, so no
//! ambient `TRAWL_*`, `FLEET_SESSION_*` or proxy variable reaches it. It
//! always applies [`assert_no_values`] to both output streams before it
//! returns, and it watches the doctor for child processes, which it starts
//! none of. [`run_web_doctor_in_userns`] does the same as root in a user
//! namespace ([`Userns`]).
//!
//! The fixtures write what a doctor run reads: a `trawld.toml`
//! ([`WebDoctorConfig`]), a cookie key file ([`write_key`]) and a pinned
//! CA ([`write_ca`]). [`fs_snapshot`] and [`assert_unchanged`] prove a run
//! wrote nothing.

#![allow(dead_code, reason = "each group uses a subset of these helpers")]

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use trawl_api::doctor::{Check, Outcome, Report};

#[path = "../../../trawl-server/tests/trawld_doctor/shapes.rs"]
mod shapes;

pub use shapes::forbidden_shape;

/// The secret planted in every value a doctor test hands the doctor that
/// the report must not show. It must never appear in the doctor's output.
pub const SECRET: &str = "private-secret";

/// A browser origin that carries [`SECRET`] in its host.
pub const PLANTED_ORIGIN: &str = "https://proxy.private-secret.example";

/// The subject of the CA [`write_ca`] makes; it carries [`SECRET`].
pub const PLANTED_CA_SUBJECT: &str = "trawl doctor private-secret CA";

/// The 32 key bytes [`write_key`] writes, cut or padded to the length a
/// test asks for; they carry [`SECRET`].
pub const PLANTED_KEY: &[u8; 32] = b"private-secret-private-secret-ab";

/// Error text fragments. Any of them in the output means the `Display` of
/// a configuration, TOML, OS, TLS or HTTP error reached the report.
/// Matched without regard to case.
pub const ERROR_FRAGMENTS: &[&str] = &[
    // trawl-web's `ConfigError`.
    "failed to read config file",
    "failed to parse config file",
    "is referenced by config but not set",
    "is set but not valid utf-8",
    "holds an aead key",
    "cookie secret file error",
    "invalid fleet session value",
    "upstream ca file",
    // TOML.
    "toml parse error",
    "invalid basic string",
    // The OS.
    "os error",
    "no such file",
    "connection refused",
    "permission denied (os",
    // reqwest, hyper and rustls.
    "error sending request",
    "client error (connect)",
    "tcp connect error",
    "invalid peer certificate",
    "unknownissuer",
    "notvalidforname",
    "builder error",
];

/// The longest a doctor run may take before the test calls it hung. Each
/// step the doctor takes has its own deadline of at most 11 s.
const RUN_DEADLINE: Duration = Duration::from_secs(60);

/// A `trawl-web` command with an empty environment.
pub fn trawl_web() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_trawl-web"));
    command.env_clear();
    command
}

/// What one doctor run printed.
#[derive(Debug)]
pub struct Observed {
    /// The exit status.
    pub code: i32,
    /// Everything it wrote to stdout.
    pub stdout: String,
    /// Everything it wrote to stderr.
    pub stderr: String,
}

/// Run `trawl-web` with `args` and exactly the environment `env`, check
/// both output streams with [`assert_no_values`] for `planted`, and return
/// what it printed.
///
/// The working directory is `HOME` when `env` sets it, so relative paths in
/// a test's configuration resolve inside the test's own directory.
///
/// # Panics
/// When the run leaks a value, does not finish within [`RUN_DEADLINE`],
/// ends by a signal, or was seen with a child process.
pub fn run_web_doctor<A, K, V>(args: &[A], env: &[(K, V)], planted: &[&str]) -> Observed
where
    A: AsRef<OsStr>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    let mut command = trawl_web();
    command.args(args);
    let observed = observe(command, args, env);
    assert_no_values(&observed.stdout, &observed.stderr, planted);
    observed
}

/// [`run_web_doctor`] as root in a user namespace, with `read_only`, when
/// given, bind-mounted read-only over itself for the run.
///
/// # Panics
/// As [`run_web_doctor`], and when the namespace set-up fails after
/// [`Userns::probe`] succeeded.
pub fn run_web_doctor_in_userns<A, K, V>(
    userns: &Userns,
    read_only: Option<&Path>,
    args: &[A],
    env: &[(K, V)],
    planted: &[&str],
) -> Observed
where
    A: AsRef<OsStr>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    let mut command = userns.command(read_only);
    command.arg(env!("CARGO_BIN_EXE_trawl-web")).args(args);
    let observed = observe(command, args, env);
    assert_no_values(&observed.stdout, &observed.stderr, planted);
    assert!(
        matches!(observed.code, 0..=3),
        "the user namespace set-up failed (exit {}): {}",
        observed.code,
        observed.stderr
    );
    observed
}

/// Run `command`, a doctor run as built by [`trawl_web`] or [`Userns`],
/// with `env` added, and watch it until it exits.
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
        .expect("trawl-web starts");

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
        if let Some(status) = child.try_wait().expect("wait on trawl-web") {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().expect("kill a hung trawl-web");
            let _ = child.wait();
            panic!(
                "trawl-web {:?} did not exit within {RUN_DEADLINE:?}",
                args.iter().map(AsRef::as_ref).collect::<Vec<_>>()
            );
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    let stdout = out.join().expect("stdout reader");
    let stderr = err.join().expect("stderr reader");
    assert!(
        watch.seen.is_empty(),
        "trawl-web --doctor started child processes {:?}: {stderr}",
        watch.seen
    );

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        assert!(
            status.signal().is_none(),
            "trawl-web died by signal {:?}: {stderr}",
            status.signal()
        );
    }
    Observed {
        code: status.code().expect("an exit status"),
        stdout,
        stderr,
    }
}

fn read_all(stream: &mut impl std::io::Read) -> String {
    let mut bytes = Vec::new();
    stream
        .read_to_end(&mut bytes)
        .expect("read trawl-web output");
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Watches one process for children, on Linux through
/// `/proc/<pid>/task/*/children`.
struct ChildWatch {
    pid: u32,
    seen: BTreeSet<u32>,
}

impl ChildWatch {
    const fn new(pid: u32) -> Self {
        Self {
            pid,
            seen: BTreeSet::new(),
        }
    }

    /// Record every child any of the process's threads has right now, once
    /// it is trawl-web: a user-namespace run starts as `unshare` and a
    /// shell, whose `mount` children are the set-up, not the doctor's.
    fn sample(&mut self) {
        #[cfg(target_os = "linux")]
        {
            let comm = std::fs::read_to_string(format!("/proc/{}/comm", self.pid));
            if comm.ok().as_deref().map(str::trim_end) != Some("trawl-web") {
                return;
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
}

/// Fail when either stream shows [`SECRET`], a value in `planted`, an
/// [`ERROR_FRAGMENTS`] entry, or the shape of a value no report may show
/// ([`forbidden_shape`]: a catalog id, a certificate fingerprint, an IP
/// address). So a test that did not think to plant its upstream's address
/// is still covered.
///
/// # Panics
/// When either stream contains one of them.
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
                "{name} shows error text {fragment:?}:\n{stream}"
            );
        }
        if let Some((shape, found)) = forbidden_shape(stream) {
            panic!("{name} shows {shape} {found:?}:\n{stream}");
        }
    }
}

// -- Root in a user namespace ----------------------------------------------

/// The name that makes an unavailable user namespace a test failure rather
/// than a skip. CI sets it, so a privileged test that did not run there is
/// never counted as passing evidence.
pub const REQUIRE_USERNS: &str = "TRAWL_TEST_REQUIRE_USERNS";

/// Running the doctor as root inside an unprivileged user namespace:
/// `unshare --user --map-root-user --mount`, which maps this test's uid to
/// 0, so the doctor sees euid 0 and the files the test made as owned by
/// root.
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

    /// `unshare` with an empty environment, set to run [`USERNS_SCRIPT`]'s
    /// arguments, with `read_only` bound read-only.
    fn command(&self, read_only: Option<&Path>) -> Command {
        let mut command = Command::new(&self.unshare);
        command.env_clear();
        command
            .args(["--user", "--map-root-user", "--mount"])
            .arg(&self.shell)
            .args(["-c", USERNS_SCRIPT, "sh"])
            .arg(&self.mount)
            .arg(read_only.map_or_else(OsString::new, |dir| dir.as_os_str().to_owned()));
        command
    }
}

// -- Proof that nothing was written ----------------------------------------

/// One entry of a [`fs_snapshot`]: everything a write changes, and not the
/// access time, which a read changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    kind: &'static str,
    mode: u32,
    size: u64,
    inode: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
    /// A file's bytes, a symlink's target; empty for a directory, FIFO,
    /// socket or device, which are never opened.
    content: Vec<u8>,
}

/// Every entry under `root`, `root` itself included as `.`, by its path
/// relative to `root`. Files the test cannot read (mode 000) are recorded
/// without their content.
pub fn fs_snapshot(root: &Path) -> BTreeMap<String, Entry> {
    use std::os::unix::fs::MetadataExt as _;
    fn walk(root: &Path, path: &Path, into: &mut BTreeMap<String, Entry>) {
        let meta = std::fs::symlink_metadata(path).expect("metadata");
        let kind = if meta.is_dir() {
            "dir"
        } else if meta.file_type().is_symlink() {
            "symlink"
        } else if meta.is_file() {
            "file"
        } else {
            "other"
        };
        let content = match kind {
            "file" => std::fs::read(path).unwrap_or_default(),
            "symlink" => std::fs::read_link(path)
                .expect("read a link")
                .into_os_string()
                .into_encoded_bytes(),
            _ => Vec::new(),
        };
        let name = path.strip_prefix(root).unwrap().display().to_string();
        into.insert(
            if name.is_empty() {
                ".".to_owned()
            } else {
                name
            },
            Entry {
                kind,
                mode: meta.mode(),
                size: meta.size(),
                inode: meta.ino(),
                mtime: (meta.mtime(), meta.mtime_nsec()),
                ctime: (meta.ctime(), meta.ctime_nsec()),
                content,
            },
        );
        if kind == "dir" {
            let mut children: Vec<PathBuf> = std::fs::read_dir(path)
                .expect("list a directory")
                .map(|entry| entry.expect("an entry").path())
                .collect();
            children.sort();
            for child in children {
                walk(root, &child, into);
            }
        }
    }
    let mut entries = BTreeMap::new();
    walk(root, root, &mut entries);
    entries
}

/// Fail, naming only the entries that differ, when `after` is not
/// `before`.
///
/// # Panics
/// When an entry was added, removed or changed.
pub fn assert_unchanged(
    before: &BTreeMap<String, Entry>,
    after: &BTreeMap<String, Entry>,
    label: &str,
) {
    let changed: Vec<String> = before
        .keys()
        .chain(after.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|name| before.get(*name) != after.get(*name))
        .map(|name| {
            let show = |entry: Option<&Entry>| {
                entry.map(|e| {
                    format!(
                        "{} mode {:o} size {} inode {} mtime {:?} ctime {:?}",
                        e.kind, e.mode, e.size, e.inode, e.mtime, e.ctime
                    )
                })
            };
            format!(
                "{name}:\n    before {:?}\n    after  {:?}",
                show(before.get(name)),
                show(after.get(name))
            )
        })
        .collect();
    assert!(
        changed.is_empty(),
        "{label} wrote:\n  {}",
        changed.join("\n  ")
    );
}

// -- Fixtures ----------------------------------------------------------------

/// The settings of a `trawld.toml` a web doctor test writes: the `[web]`
/// block, and the `[server]` listen address the upstream URL derives from
/// when `upstream_url` is unset.
#[derive(Debug, Clone)]
pub struct WebDoctorConfig {
    /// `[server] http_addr`.
    pub http_addr: String,
    /// `[data] path`, which the schema requires and trawl-web ignores.
    pub data_path: PathBuf,
    /// `[web] bind_addr`.
    pub bind_addr: Option<String>,
    /// `[web] public_origins`.
    pub public_origins: Vec<String>,
    /// `[web] upstream_url`.
    pub upstream_url: Option<String>,
    /// `[web] upstream_connect_addr`.
    pub upstream_connect_addr: Option<String>,
    /// `[web] upstream_ca_path`.
    pub upstream_ca_path: Option<PathBuf>,
    /// `[web] cookie_secret_path`.
    pub cookie_secret_path: Option<PathBuf>,
    /// `[web] cookie_secret_env`.
    pub cookie_secret_env: Option<String>,
    /// `[web] session_ttl_secs`.
    pub session_ttl_secs: Option<u64>,
    /// `[web] allow_insecure_cookies`.
    pub allow_insecure_cookies: bool,
    /// `[web] shared_domain`.
    pub shared_domain: Option<String>,
    /// TOML appended as it is, for settings the fields above do not name.
    pub extra: String,
}

impl WebDoctorConfig {
    /// A proxy whose one public origin is [`PLANTED_ORIGIN`], with no key
    /// source, the platform roots, and an upstream derived from a
    /// `[server] http_addr` on a loopback port nothing listens on.
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            http_addr: "127.0.0.1:1".to_owned(),
            data_path: dir.join("data"),
            bind_addr: None,
            public_origins: vec![PLANTED_ORIGIN.to_owned()],
            upstream_url: None,
            upstream_connect_addr: None,
            upstream_ca_path: None,
            cookie_secret_path: None,
            cookie_secret_env: None,
            session_ttl_secs: None,
            allow_insecure_cookies: false,
            shared_domain: None,
            extra: String::new(),
        }
    }

    /// Every configured value the report must not show: the origins, the
    /// upstream URL and its host, the connect address, and the shared
    /// domain. Paths and the `cookie_secret_env` name may be shown.
    pub fn planted(&self) -> Vec<String> {
        let mut values = self.public_origins.clone();
        if let Some(url) = &self.upstream_url {
            values.push(url.clone());
            if let Some(host) = reqwest::Url::parse(url)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
            {
                values.push(host);
            }
        }
        values.extend(self.upstream_connect_addr.clone());
        values.extend(self.shared_domain.clone());
        values.extend(self.bind_addr.clone());
        values.retain(|value| !value.is_empty());
        values
    }
}

/// Write `config` as `dir/trawld.toml` and return its path.
pub fn write_config(dir: &Path, config: &WebDoctorConfig) -> PathBuf {
    use std::fmt::Write as _;
    let quote = |path: &Path| toml_string(&path.to_string_lossy());
    let mut document = String::new();
    let mut line = |text: String| writeln!(document, "{text}").expect("writing to a String");
    line("[server]".to_owned());
    line(format!("http_addr = {}", toml_string(&config.http_addr)));
    line("[data]".to_owned());
    line(format!("path = {}", quote(&config.data_path)));
    line("[web]".to_owned());
    if let Some(addr) = &config.bind_addr {
        line(format!("bind_addr = {}", toml_string(addr)));
    }
    let origins: Vec<String> = config
        .public_origins
        .iter()
        .map(|origin| toml_string(origin))
        .collect();
    line(format!("public_origins = [{}]", origins.join(", ")));
    if let Some(url) = &config.upstream_url {
        line(format!("upstream_url = {}", toml_string(url)));
    }
    if let Some(addr) = &config.upstream_connect_addr {
        line(format!("upstream_connect_addr = {}", toml_string(addr)));
    }
    if let Some(path) = &config.upstream_ca_path {
        line(format!("upstream_ca_path = {}", quote(path)));
    }
    if let Some(path) = &config.cookie_secret_path {
        line(format!("cookie_secret_path = {}", quote(path)));
    }
    if let Some(name) = &config.cookie_secret_env {
        line(format!("cookie_secret_env = {}", toml_string(name)));
    }
    if let Some(ttl) = config.session_ttl_secs {
        line(format!("session_ttl_secs = {ttl}"));
    }
    if config.allow_insecure_cookies {
        line("allow_insecure_cookies = true".to_owned());
    }
    if let Some(domain) = &config.shared_domain {
        line(format!("shared_domain = {}", toml_string(domain)));
    }
    document.push_str(&config.extra);
    let path = dir.join("trawld.toml");
    std::fs::write(&path, document).expect("write trawld.toml");
    path
}

/// `value` as a TOML basic string.
pub fn toml_string(value: &str) -> String {
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

/// Write a cookie key file of `len` bytes at `path`, mode 0600: the start
/// of [`PLANTED_KEY`], padded with `x` past 32 bytes. Its parent directory
/// is created.
pub fn write_key(path: &Path, len: usize) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let mut bytes = PLANTED_KEY.to_vec();
    bytes.resize(len, b'x');
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("the key's directory");
    }
    std::fs::write(path, bytes).expect("write the key");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod the key");
    path.to_owned()
}

/// Write a self-signed CA certificate whose subject is
/// [`PLANTED_CA_SUBJECT`] at `path`, as PEM, and return the path. Its
/// parent directory is created.
pub fn write_ca(path: &Path) -> PathBuf {
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).expect("CA params");
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, PLANTED_CA_SUBJECT);
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::CrlSign,
    ];
    let key = rcgen::KeyPair::generate().expect("CA key");
    let cert = params.self_signed(&key).expect("CA certificate");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("the CA's directory");
    }
    std::fs::write(path, cert.pem()).expect("write the CA");
    path.to_owned()
}

/// trawld's healthy answer to `GET /api/v1/health`: `status: ok`, every
/// check trawld reports `ok`, and a version.
pub fn healthy_answer() -> wiremock::ResponseTemplate {
    let checks: serde_json::Map<String, serde_json::Value> =
        trawl_api::doctor::health::HEALTH_CHECK_NAMES
            .iter()
            .map(|name| ((*name).to_owned(), "ok".into()))
            .collect();
    wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "status": "ok",
        "version": "1.0.0",
        "checks": checks,
    }))
}

/// A real rustls upstream serving [`healthy_answer`], under a fresh CA
/// whose subject is [`PLANTED_CA_SUBJECT`]. Point `upstream_url` at its
/// `url()` and pin its `ca_path()`, and both upstream checks pass.
pub async fn healthy_upstream() -> crate::test_support::TlsUpstream {
    use wiremock::matchers::{method, path};

    let ca = crate::test_support::TestCa::named(PLANTED_CA_SUBJECT);
    let upstream = crate::test_support::TlsUpstream::issued_by(&ca).await;
    wiremock::Mock::given(method("GET"))
        .and(path("/api/v1/health"))
        .respond_with(healthy_answer())
        .mount(upstream.mock())
        .await;
    upstream
}

/// The environment of a doctor run in `home`: `HOME` alone.
pub fn home_env(home: &Path) -> Vec<(&'static str, OsString)> {
    vec![("HOME", home.as_os_str().to_owned())]
}

/// The doctor's arguments for `config` in `format`.
pub fn doctor_args(config: &Path, format: &str) -> [OsString; 5] {
    [
        "--doctor".into(),
        "--config".into(),
        config.as_os_str().to_owned(),
        "--format".into(),
        format.into(),
    ]
}

/// Run the doctor over `config` with `env`, in JSON and then in table
/// form, each checked for `planted`, and return the exit status and the
/// JSON report. Both forms must exit alike, the exit status must be the
/// report's verdict, and a report run writes nothing to stderr.
///
/// # Panics
/// As [`run_web_doctor`], and when the two runs disagree.
pub fn doctor(config: &Path, env: &[(&str, OsString)], planted: &[String]) -> (i32, Report) {
    let planted: Vec<&str> = planted.iter().map(String::as_str).collect();
    let json = run_web_doctor(&doctor_args(config, "json"), env, &planted);
    let table = run_web_doctor(&doctor_args(config, "table"), env, &planted);
    assert_eq!(json.code, table.code, "{}\n{}", json.stdout, table.stdout);
    assert!(json.stderr.is_empty(), "{}", json.stderr);
    assert!(table.stderr.is_empty(), "{}", table.stderr);
    let parsed = report(&json.stdout);
    assert_eq!(
        i32::from(parsed.verdict().exit_code()),
        json.code,
        "the exit status is the verdict's: {}",
        json.stdout
    );
    assert!(
        table.stdout.contains(&format!("(exit {})", json.code)),
        "{}",
        table.stdout
    );
    (json.code, parsed)
}

/// Parse a JSON report the doctor printed.
///
/// # Panics
/// When `stdout` is not one, or not the web vantage's.
pub fn report(stdout: &str) -> Report {
    let report: Report = serde_json::from_str(stdout)
        .unwrap_or_else(|error| panic!("not a doctor report ({error}):\n{stdout}"));
    assert_eq!(
        report.vantage(),
        trawl_api::doctor::Vantage::Web,
        "{stdout}"
    );
    report
}

/// The row `id` of `report`.
///
/// # Panics
/// When there is none.
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

/// The ids of the checks every report carries, in order, before any
/// `proxy.upstream.health.<key>` row.
pub const CHECK_IDS: [&str; 7] = [
    "proxy.config",
    "proxy.identity",
    "proxy.public_origins",
    "proxy.cookie_settings",
    "proxy.cookie_key",
    "proxy.upstream.trust",
    "proxy.upstream.health",
];
