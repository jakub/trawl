// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawl-web --doctor`: check, from where the web proxy runs, whether
//! trawl-web will start, keep sessions, and reach trawld with a verified
//! certificate (ADR-0047 and its 2026-09-28 amendment, #271).
//!
//! `main` reaches [`run`] before it installs a tracing subscriber and
//! before anything of startup runs. The doctor reads the `--config` file
//! and the process environment, resolves them through
//! [`Sources::resolve`], which has no side effect, and asks trawld's
//! health endpoint through the production client. It resolves the listen
//! address as startup's listener does but never binds it, never generates
//! a session key, and writes nothing.
//!
//! The checks and their prerequisites are data ([`WebCheck`]). One
//! [`Runner`] visits them in [`WebCheck::ALL`] order: a check whose
//! prerequisite did not complete becomes `not_sampled`, reason `blocked`,
//! naming that prerequisite, and never looks. `proxy.config` and
//! `proxy.identity` are checked here; the rest belong to two groups, each
//! in its own file and each writing rows only through the runner:
//!
//! - [`settings`]: the public origins, the cookie settings and the cookie
//!   key.
//! - [`upstream`]: the upstream's trust and its health.
//!
//! Each component row owns whatever error arrives in its slot of
//! [`Sources`]; `proxy.config` owns the file's read and parse and the
//! listen address. Every row is built from [`output`]'s constructors,
//! which accept no free-form text, so no error text, origin, upstream URL,
//! certificate name or key reaches the report. Every file is read through
//! [`read`].

use std::io::{self, Write};
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use trawl_api::doctor::{Outcome, Report, Target, Vantage, reason};

use crate::config::{
    ConfigError, CookieSettings, KeySource, Origins, PinnedRoots, RuntimeParts, SettingSource,
    Sources, UpstreamPlan,
};

pub mod output;
pub mod read;
mod settings;
mod upstream;

pub use output::Format;
use output::{Row, SelectedPath, Selection, Text, UserName};

/// The named checks `trawl-web --doctor` runs, in order.
/// `proxy.upstream.health.<key>` rows are part of
/// [`WebCheck::UpstreamHealth`] and share its prerequisite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebCheck {
    /// The `--config` file reads and parses as the whole schema, and the
    /// listen address resolves, as at startup.
    Config,
    /// The effective user and uid the doctor ran as.
    Identity,
    /// A non-empty, valid browser-origin allowlist resolves.
    PublicOrigins,
    /// The session cookie's domain, path, Secure flag and lifetime resolve.
    CookieSettings,
    /// A persistent cookie key source is selected and usable.
    CookieKey,
    /// The upstream URL is https without userinfo, and its trust resolves.
    UpstreamTrust,
    /// trawld's health endpoint answers through the production client, one
    /// row per reported check.
    UpstreamHealth,
}

impl WebCheck {
    /// Every check, in the order the runner visits them.
    pub const ALL: [Self; 7] = [
        Self::Config,
        Self::Identity,
        Self::PublicOrigins,
        Self::CookieSettings,
        Self::CookieKey,
        Self::UpstreamTrust,
        Self::UpstreamHealth,
    ];

    /// The stable id the report carries.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Config => "proxy.config",
            Self::Identity => "proxy.identity",
            Self::PublicOrigins => "proxy.public_origins",
            Self::CookieSettings => "proxy.cookie_settings",
            Self::CookieKey => "proxy.cookie_key",
            Self::UpstreamTrust => "proxy.upstream.trust",
            Self::UpstreamHealth => "proxy.upstream.health",
        }
    }

    /// The checks that must be satisfied before this one looks: `complete`
    /// with any reason, or `not_sampled` with reason `ran_as_root`.
    ///
    /// Every component comes from the parsed file. The Secure rule judges
    /// the effective origins, so the cookie settings wait on them, and
    /// health is asked only through a trust that resolved.
    #[must_use]
    pub const fn prerequisites(self) -> &'static [Self] {
        match self {
            Self::Config | Self::Identity => &[],
            Self::PublicOrigins | Self::CookieKey | Self::UpstreamTrust => &[Self::Config],
            Self::CookieSettings => &[Self::Config, Self::PublicOrigins],
            Self::UpstreamHealth => &[Self::UpstreamTrust],
        }
    }

    /// Whether the check may assert that the running user can read
    /// something. Only the cookie key does, and only for a
    /// `cookie_secret_path` file: its row carries [`Row::access`], and a
    /// root run reports that row's `complete` as `not_sampled`, reason
    /// `ran_as_root` (D12). A key from the environment is content.
    #[must_use]
    pub const fn is_access(self) -> bool {
        matches!(self, Self::CookieKey)
    }
}

/// The user the doctor runs as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunAs {
    /// The effective uid, where the platform has one.
    pub uid: Option<u32>,
    /// Whether that uid is 0.
    pub root: bool,
}

impl RunAs {
    /// The running process's effective user.
    #[must_use]
    pub fn current() -> Self {
        #[cfg(unix)]
        {
            let uid = rustix::process::geteuid().as_raw();
            Self {
                uid: Some(uid),
                root: uid == 0,
            }
        }
        #[cfg(not(unix))]
        {
            Self {
                uid: None,
                root: false,
            }
        }
    }
}

/// Everything the check groups share, built once `proxy.config` resolved
/// the configuration.
///
/// The component slots are [`Sources`]' own, moved here whole: each holds
/// its component's selection and source, or the error that component
/// failed with, which only that component's row reports. A group takes
/// what it needs from its own slots; no group reads another's.
///
/// - [`settings`] reads `public_origins`, `cookie_settings` and
///   `cookie_key`.
/// - [`upstream`] reads `upstream`, and `proxy.upstream.trust` fills
///   `pinned_roots` for `proxy.upstream.health`.
pub(crate) struct Ctx {
    /// The `--config` path, as the report may name it.
    pub(crate) config_path: SelectedPath,
    /// The user the doctor runs as.
    pub(crate) run_as: RunAs,
    /// [`Sources::public_origins`]: `proxy.public_origins`.
    pub(crate) public_origins: Result<Origins, ConfigError>,
    /// [`Sources::cookie_settings`]: `proxy.cookie_settings`.
    pub(crate) cookie_settings: Result<CookieSettings, ConfigError>,
    /// [`Sources::cookie_key`]: `proxy.cookie_key`.
    pub(crate) cookie_key: Result<KeySource, ConfigError>,
    /// [`Sources::upstream`]: `proxy.upstream.trust`.
    pub(crate) upstream: Result<UpstreamPlan, ConfigError>,
    /// Filled by `proxy.upstream.trust` when the trust source is a pinned
    /// CA file and it read and parsed: the roots the probe trusts, so the
    /// file is read once. `None` under the platform roots, and whenever
    /// the trust check did not complete, which blocks the probe.
    pub(crate) pinned_roots: Option<PinnedRoots>,
}

/// The component slots carry configured values and errors whose text can
/// quote them, so `Ctx`'s `Debug` shows only the path, the user and the
/// slots' own value-free `Debug`.
impl std::fmt::Debug for Ctx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fn slot<T: std::fmt::Debug>(result: &Result<T, ConfigError>) -> String {
            match result {
                Ok(value) => format!("{value:?}"),
                Err(_) => "Err(<config error>)".to_owned(),
            }
        }
        f.debug_struct("Ctx")
            .field("config_path", &self.config_path)
            .field("run_as", &self.run_as)
            .field("public_origins", &slot(&self.public_origins))
            .field("cookie_settings", &slot(&self.cookie_settings))
            .field("cookie_key", &slot(&self.cookie_key))
            .field("upstream", &slot(&self.upstream))
            .field("pinned_roots", &self.pinned_roots.is_some())
            .finish()
    }
}

/// Leave to look at one check, from [`Runner::gate`]. Only the runner makes
/// one, and [`Runner::record`] consumes it.
#[derive(Debug)]
#[must_use = "a gated check must record its row"]
pub(crate) struct Gate {
    check: WebCheck,
}

impl Gate {
    /// The check this gate opens.
    pub(crate) const fn check(&self) -> WebCheck {
        self.check
    }
}

/// Visits the checks in [`WebCheck::ALL`] order and collects their rows.
#[derive(Debug)]
pub(crate) struct Runner {
    root: bool,
    /// Each check's own row, in order: what prerequisites are judged on.
    outcomes: Vec<(WebCheck, Outcome, Option<&'static str>)>,
    /// The check gated and not yet recorded.
    open: Option<WebCheck>,
    rows: Vec<Row>,
    notes: Vec<String>,
}

impl Runner {
    /// A runner for a run as `run_as`.
    pub(crate) fn new(run_as: RunAs) -> Self {
        Self {
            root: run_as.root,
            outcomes: Vec::new(),
            open: None,
            rows: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// Ask to look at `check`, the next check in [`WebCheck::ALL`].
    ///
    /// When a prerequisite is not satisfied, the runner records `check` as
    /// `not_sampled`, reason `blocked`, naming the first such prerequisite,
    /// and returns `None`: the check does not look.
    ///
    /// # Panics
    /// When `check` is not the next check, or the last gate was not
    /// recorded. Both are bugs in a group, never input.
    pub(crate) fn gate(&mut self, check: WebCheck) -> Option<Gate> {
        assert!(self.open.is_none(), "a gated check was not recorded");
        assert_eq!(
            WebCheck::ALL.get(self.outcomes.len()),
            Some(&check),
            "checks run once each, in WebCheck::ALL order"
        );
        if let Some(by) = check
            .prerequisites()
            .iter()
            .find(|pre| !self.satisfied(**pre))
        {
            self.push(Row::blocked(check, *by));
            return None;
        }
        self.open = Some(check);
        Some(Gate { check })
    }

    /// Record the row of a gated check.
    ///
    /// # Panics
    /// When the row is not the gated check's own row.
    pub(crate) fn record(&mut self, gate: Gate, row: Row) {
        self.record_with(gate, row, Vec::new());
    }

    /// Record the row of a gated check and its per-key rows, such as
    /// `proxy.upstream.health` and its `proxy.upstream.health.<key>` rows.
    /// The check's own row is what its dependents are judged on.
    ///
    /// Run as root, an own row marked [`Row::access`] that is `complete`
    /// is recorded as `not_sampled`, reason `ran_as_root`.
    ///
    /// # Panics
    /// When `row` is not the gated check's own row, a keyed row belongs to
    /// another check, or a row carries the access mark on a check that
    /// asserts no access.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "taking the gate by value spends it: one row per gate"
    )]
    pub(crate) fn record_with(&mut self, gate: Gate, row: Row, keyed: Vec<Row>) {
        let Gate { check } = gate;
        assert_eq!(self.open.take(), Some(check), "record follows gate");
        assert!(
            row.check() == check && !row.is_keyed(),
            "the check's own row"
        );
        assert!(
            !row.is_access() || check.is_access(),
            "only an access check's row asserts access"
        );
        assert!(
            keyed
                .iter()
                .all(|sub| sub.check() == check && sub.is_keyed() && !sub.is_access()),
            "keyed rows of the gated check"
        );
        let row = if self.root { row.into_root_run() } else { row };
        self.push(row);
        self.rows.extend(keyed);
    }

    /// The outcome and reason `check` recorded, once it has.
    pub(crate) fn outcome(&self, check: WebCheck) -> Option<(Outcome, Option<&'static str>)> {
        self.outcomes
            .iter()
            .find(|(recorded, ..)| *recorded == check)
            .map(|(_, outcome, reason)| (*outcome, *reason))
    }

    /// Add a report-level note, which never changes an outcome.
    #[allow(dead_code, reason = "a group may add a note; none does yet")]
    pub(crate) fn note(&mut self, note: Text) {
        self.notes.push(note.into_string());
    }

    fn satisfied(&self, check: WebCheck) -> bool {
        match self.outcome(check) {
            Some((Outcome::Complete, _)) => true,
            Some((Outcome::NotSampled, Some(why))) => why == reason::RAN_AS_ROOT,
            _ => false,
        }
    }

    fn push(&mut self, row: Row) {
        self.outcomes
            .push((row.check(), row.outcome(), row.reason()));
        self.rows.push(row);
    }

    /// Every check has a row: build the report.
    fn finish(self, target: Target) -> Report {
        assert_eq!(
            self.outcomes.len(),
            WebCheck::ALL.len(),
            "every check records one row"
        );
        let checks = self.rows.into_iter().map(Row::into_check).collect();
        Report::new(Vantage::Web, target, checks, self.notes)
    }
}

/// How long the doctor waits, at exit, for blocking work still running,
/// such as a file read past its deadline.
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(1);

/// How long expanding `~` in the `--config` path may take: a file read's
/// budget. With HOME unset or empty the expansion asks the password
/// database, which can be a network service. The key and pin paths expand
/// in their own bounded reads ([`read`]).
const VALIDATE_DEADLINE: Duration = read::READ_DEADLINE;

/// How long resolving the listen address may take: a file read's budget.
/// A host name resolves through the system resolver, which can be a
/// network service.
const LISTEN_RESOLVE_DEADLINE: Duration = read::READ_DEADLINE;

/// How long the running user's name may take to look up. The password
/// database can be a network service.
const USER_LOOKUP_DEADLINE: Duration = Duration::from_secs(2);

/// Run `trawl-web --doctor --config <config>` and return its exit status:
/// 0 pass, 1 fail, 3 incomplete (ADR-0047). `config` is the argument as
/// given; `proxy.config` expands its `~`.
///
/// Call on the main thread, before any runtime or subscriber exists.
#[must_use]
pub fn run(config: &Path, format: Option<Format>) -> u8 {
    install_panic_hook(io::stderr);
    let format = format.unwrap_or_else(Format::detect);
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        eprintln!("[trawl-web] doctor refused: its async runtime could not be built");
        return 1;
    };
    // The probe's client verifies TLS with this provider, the one startup
    // installs. An earlier install in this process is the same provider.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let report = runtime.block_on(check(config));
    runtime.shutdown_timeout(SHUTDOWN_BUDGET);

    let stdout = io::stdout();
    let mut out = stdout.lock();
    if output::render(&report, format, &mut out)
        .and_then(|()| out.flush())
        .is_err()
    {
        return 1;
    }
    report.verdict().exit_code()
}

/// The one line the doctor's panic hook prints.
const PANIC_LINE: &str = "[trawl-web] doctor stopped a check: an internal error occurred";

/// Replace the default panic hook, which prints the panic's message and
/// location, with one that writes [`PANIC_LINE`] to `sink` and nothing
/// else. A dependency's panic message can quote what it was given, such as
/// an upstream URL, so no part of it reaches the output.
fn install_panic_hook<W: Write>(sink: impl Fn() -> W + Send + Sync + 'static) {
    std::panic::set_hook(Box::new(move |_| {
        let _ = writeln!(sink(), "{PANIC_LINE}");
    }));
}

/// Run every check and build the report. Nothing is rendered until the
/// whole report exists. `config_arg` is the `--config` argument as given.
pub async fn check(config_arg: &Path) -> Report {
    let run_as = RunAs::current();
    let mut runner = Runner::new(run_as);

    let (shown, sources) = check_config(&mut runner, config_arg).await;
    check_identity(&mut runner, run_as).await;

    if let Some(sources) = sources {
        let Sources {
            public_origins,
            cookie_key,
            cookie_settings,
            upstream,
            ..
        } = sources;
        let mut ctx = Ctx {
            config_path: shown.clone(),
            run_as,
            public_origins,
            cookie_settings,
            cookie_key,
            upstream,
            pinned_roots: None,
        };
        settings::run(&mut runner, &mut ctx).await;
        upstream::run(&mut runner, &mut ctx).await;
    } else {
        // Every other check reads the configuration, so each is blocked.
        for check in &WebCheck::ALL[2..] {
            let gate = runner.gate(*check);
            assert!(
                gate.is_none(),
                "every check after identity needs the config"
            );
        }
    }

    runner.finish(Target {
        origin: None,
        source: format!("--config {}", shown.as_shown()),
    })
}

/// Expand `~` in `path` as startup expands the `--config` path. With HOME
/// unset or empty, `~` resolves through the password database.
fn tilde_path(path: &Path) -> PathBuf {
    PathBuf::from(shellexpand::tilde(&path.to_string_lossy()).into_owned())
}

/// `proxy.config`: expand `~` in the `--config` argument, read the file
/// through the bounded reader, parse it as the whole schema, observe the
/// environment once, and resolve the sources as startup does. Expanding
/// runs on the blocking pool under [`VALIDATE_DEADLINE`]. Resolving the
/// sources looks nothing up: the key and pin paths stay as written until
/// their reads. The selected listen address then resolves as startup's
/// listener resolves it ([`listen_addr_resolves`]), on the blocking pool
/// under [`LISTEN_RESOLVE_DEADLINE`], and is never bound.
///
/// The row owns the file's read and parse and the listen address; every
/// other component's error stays in its slot for its own row. Returns the
/// `--config` path as the report names it, expanded when the expansion
/// finished, and the sources when the file parsed and the listen address
/// resolved.
async fn check_config(runner: &mut Runner, arg: &Path) -> (SelectedPath, Option<Sources>) {
    let gate = runner
        .gate(WebCheck::Config)
        .expect("proxy.config has no prerequisite");
    let check = WebCheck::Config;
    let expanded = blocking_within(VALIDATE_DEADLINE, {
        let arg = arg.to_owned();
        move || tilde_path(&arg)
    })
    .await;
    let shown = SelectedPath::new(Selection::ConfigFlag, expanded.as_deref().unwrap_or(arg));
    let source = Text::new("--config ").path(&shown);
    let fix = Text::new("correct the setting in ").path(&shown);

    let loaded = async {
        let path = expanded.map_err(|unfinished| {
            unfinished_config(
                unfinished,
                "expanding ~ in the --config path did not finish",
            )
        })?;
        let bytes = read::read(path, read::cap::CONFIG)
            .await
            .map_err(config_read_fault)?;
        let text = String::from_utf8(bytes).map_err(|_| {
            Row::failed(check, "the configuration is not UTF-8 text").next(fix.clone())
        })?;
        let config = trawl_config::Config::parse_toml(&text).map_err(|error| match error {
            trawl_config::ConfigError::Parse { reason, .. } => {
                Row::failed(check, "the configuration does not parse")
                    .detail(Text::new(reason))
                    .next(fix.clone())
            }
            trawl_config::ConfigError::Io { .. } | trawl_config::ConfigError::Validation(_) => {
                Row::failed(check, "the configuration does not parse").next(fix.clone())
            }
        })?;
        let sources = Sources::resolve(
            &config.web,
            Some(&config.server),
            RuntimeParts::from_process_env(),
        );
        let listen_at = match &sources.bind_addr {
            Ok(listen_at) => listen_at.clone(),
            Err(error) => return Err(bind_addr_fault(error)),
        };
        let resolved = blocking_within(LISTEN_RESOLVE_DEADLINE, move || {
            listen_addr_resolves(&listen_at.addr)
        })
        .await
        .map_err(|unfinished| {
            unfinished_config(unfinished, "resolving the listen address did not finish")
        })?;
        if resolved {
            Ok((listen_at.from, sources))
        } else {
            Err(unresolved_listen_addr(listen_at.from))
        }
    }
    .await;
    match loaded {
        Ok((bind_from, sources)) => {
            let row = Row::complete(check)
                .detail(Text::new("parses; the listen address comes from ").setting(bind_from))
                .source(source);
            runner.record(gate, row);
            (shown, Some(sources))
        }
        Err(row) => {
            runner.record(gate, row.source(source));
            (shown, None)
        }
    }
}

/// The `proxy.config` row for a `--config` file the bounded reader did not
/// read.
fn config_read_fault(fault: read::ReadFault) -> Row {
    use read::ReadFault;
    let check = WebCheck::Config;
    let pass_path = || Text::new("pass the path of trawld.toml to --config");
    match fault {
        ReadFault::Missing => {
            Row::failed(check, "the configuration file does not exist").next(pass_path())
        }
        ReadFault::NotRegular => {
            Row::failed(check, "the configuration is not a regular file").next(pass_path())
        }
        ReadFault::SymlinkLoop => {
            Row::failed(check, "the configuration path is a symlink loop").next(pass_path())
        }
        ReadFault::PermissionDenied => Row::not_sampled(check, reason::PERMISSION_DENIED).next(
            Text::new("rerun as the service user, which can read its configuration"),
        ),
        ReadFault::TooLarge => Row::not_sampled(check, reason::TOO_LARGE).detail(
            Text::new("the doctor reads at most ")
                .int(read::cap::CONFIG)
                .lit(" bytes"),
        ),
        ReadFault::TimedOut => Row::not_sampled(check, reason::TIMED_OUT),
        ReadFault::Io => Row::not_sampled(check, reason::UNREADABLE),
    }
}

/// The `proxy.config` row for a listen address that does not resolve. The
/// only way it fails is `TRAWL_WEB_BIND_ADDR` set to text that is not
/// UTF-8.
fn bind_addr_fault(error: &ConfigError) -> Row {
    let row = Row::failed(WebCheck::Config, "the listen address does not resolve");
    match error {
        ConfigError::EnvUtf8 { .. } => row.next(
            Text::new("set ")
                .lit(crate::config::ENV_BIND_ADDR)
                .lit(" to UTF-8 text, or unset it"),
        ),
        _ => row,
    }
}

/// Whether `addr` resolves to at least one socket address, as startup's
/// `TcpListener::bind` resolves it before binding: a literal `ip:port` or
/// `[ipv6]:port` parses, and a `host:port` asks the system resolver.
/// Startup fails before serving on text that is neither, on a port out of
/// range, and on a name with no address. Nothing is bound or connected.
/// Blocking: call it on the blocking pool.
fn listen_addr_resolves(addr: &str) -> bool {
    addr.to_socket_addrs()
        .is_ok_and(|mut found| found.next().is_some())
}

/// The `proxy.config` row for a selected listen address that does not
/// resolve, naming where it came from. Neither the address nor the
/// resolver's error is shown: both quote the operator's text.
fn unresolved_listen_addr(from: SettingSource) -> Row {
    Row::failed(
        WebCheck::Config,
        "the listen address is not a host:port that resolves",
    )
    .detail(Text::new("the listen address comes from ").setting(from))
    .next(Text::new(
        "set it to a host and a port from 0 to 65535, such as a loopback address and a free port",
    ))
}

/// Why blocking work gave no answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unfinished {
    /// It was still running at the deadline, and was left behind.
    TimedOut,
    /// It panicked.
    Panicked,
}

/// Run `work` on the blocking pool and wait at most `deadline` for it.
/// Work past the deadline is left on its thread, which the runtime's
/// bounded shutdown ([`SHUTDOWN_BUDGET`]) does not wait out.
async fn blocking_within<T: Send + 'static>(
    deadline: Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Unfinished> {
    match tokio::time::timeout(deadline, tokio::task::spawn_blocking(work)).await {
        Ok(Ok(done)) => Ok(done),
        Ok(Err(_)) => Err(Unfinished::Panicked),
        Err(_) => Err(Unfinished::TimedOut),
    }
}

/// The `proxy.config` row for a blocking step that gave no answer; `what`
/// says which step, when it ran out of time.
fn unfinished_config(unfinished: Unfinished, what: &'static str) -> Row {
    let check = WebCheck::Config;
    match unfinished {
        Unfinished::TimedOut => Row::not_sampled(check, reason::TIMED_OUT).detail(Text::new(what)),
        Unfinished::Panicked => Row::not_sampled(check, reason::UNREADABLE),
    }
}

/// `proxy.identity`: the effective uid and, when the password database
/// answers in time with a plain name, the user name.
///
/// Run as root it is `not_sampled`, reason `ran_as_root`, always (D12):
/// root reads what the service user may not, so a root run proves nothing
/// about the service's access and never exits 0. Otherwise `complete`.
async fn check_identity(runner: &mut Runner, run_as: RunAs) {
    let gate = runner
        .gate(WebCheck::Identity)
        .expect("proxy.identity has no prerequisite");
    let check = WebCheck::Identity;
    let Some(uid) = run_as.uid else {
        runner.record(
            gate,
            Row::complete(check).detail(Text::new("this platform has no uid")),
        );
        return;
    };
    let name = user_name(uid).await;
    let mut detail = Text::new("uid ").int(uid);
    if let Some(name) = &name {
        detail = detail.lit(" (").user(name).lit(")");
    }
    let row = if run_as.root {
        Row::not_sampled(check, reason::RAN_AS_ROOT)
            .detail(detail.lit("; root reads what the service user may not"))
            .next(Text::new(
                "rerun as the service user, such as trawl-web, to check what it can read",
            ))
    } else {
        Row::complete(check).detail(detail)
    };
    runner.record(gate, row);
}

/// The name of `uid` in the password database, on the blocking pool under
/// [`USER_LOOKUP_DEADLINE`]. `None` when there is none, it is not a plain
/// name, or the lookup failed or ran out of time.
async fn user_name(uid: u32) -> Option<UserName> {
    #[cfg(unix)]
    {
        let lookup = tokio::task::spawn_blocking(move || {
            nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
                .ok()
                .flatten()
                .and_then(|user| UserName::new(&user.name))
        });
        tokio::time::timeout(USER_LOOKUP_DEADLINE, lookup)
            .await
            .ok()
            .and_then(Result::ok)
            .flatten()
    }
    #[cfg(not(unix))]
    {
        let _ = uid;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The graph is data: ids are unique and stable, every prerequisite
    /// comes earlier in `ALL`, the edges are the design's (D14), only the
    /// cookie key is an access check, and the two groups cover every check
    /// after config and identity, in order.
    #[test]
    fn web_checks_form_an_ordered_graph() {
        use WebCheck as C;
        let ids: Vec<&str> = WebCheck::ALL.iter().map(|c| c.id()).collect();
        assert_eq!(
            ids,
            [
                "proxy.config",
                "proxy.identity",
                "proxy.public_origins",
                "proxy.cookie_settings",
                "proxy.cookie_key",
                "proxy.upstream.trust",
                "proxy.upstream.health",
            ]
        );
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), WebCheck::ALL.len());

        for (at, check) in WebCheck::ALL.iter().enumerate() {
            for pre in check.prerequisites() {
                assert!(
                    WebCheck::ALL[..at].contains(pre),
                    "{check:?} waits on {pre:?}, which does not come before it"
                );
            }
        }
        let edges: [(C, &[C]); 7] = [
            (C::Config, &[]),
            (C::Identity, &[]),
            (C::PublicOrigins, &[C::Config]),
            (C::CookieSettings, &[C::Config, C::PublicOrigins]),
            (C::CookieKey, &[C::Config]),
            (C::UpstreamTrust, &[C::Config]),
            (C::UpstreamHealth, &[C::UpstreamTrust]),
        ];
        for (check, pres) in edges {
            assert_eq!(check.prerequisites(), pres, "{check:?}");
        }
        let access: Vec<C> = C::ALL.into_iter().filter(|c| c.is_access()).collect();
        assert_eq!(access, [C::CookieKey]);

        let grouped: Vec<C> = [&settings::CHECKS[..], &upstream::CHECKS[..]].concat();
        assert_eq!(grouped, C::ALL[2..]);
    }

    fn run_as(root: bool) -> RunAs {
        RunAs {
            uid: Some(if root { 0 } else { 1000 }),
            root,
        }
    }

    /// Record `row` for `check`, or nothing when the runner blocked it.
    fn step(runner: &mut Runner, check: WebCheck, row: impl FnOnce() -> Row) {
        if let Some(gate) = runner.gate(check) {
            runner.record(gate, row());
        }
    }

    fn summary(report: &Report) -> Vec<(&str, Outcome, Option<&str>, Option<&str>)> {
        report
            .checks()
            .iter()
            .map(|c| {
                (
                    c.id.as_str(),
                    c.outcome,
                    c.reason.as_deref(),
                    c.blocked_by.as_deref(),
                )
            })
            .collect()
    }

    fn target() -> Target {
        Target {
            origin: None,
            source: "--config /etc/trawl/trawld.toml".to_owned(),
        }
    }

    /// Every check gets exactly one row, in order, plus the keyed rows its
    /// own row brings; the report is the web vantage's.
    #[test]
    fn every_check_gets_exactly_one_row() {
        use WebCheck as C;
        let mut runner = Runner::new(run_as(false));
        for check in &C::ALL[..6] {
            step(&mut runner, *check, || Row::complete(*check));
        }
        let gate = runner.gate(C::UpstreamHealth).expect("trust completed");
        let key = |name| output::HealthKey::new(name).expect("a health key");
        runner.record_with(
            gate,
            Row::complete(C::UpstreamHealth),
            vec![
                Row::for_key(C::UpstreamHealth, key("duckdb"), Outcome::Complete, None),
                Row::for_key(C::UpstreamHealth, key("data_path"), Outcome::Complete, None),
            ],
        );
        let report = runner.finish(target());
        let ids: Vec<&str> = report.checks().iter().map(|c| c.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "proxy.config",
                "proxy.identity",
                "proxy.public_origins",
                "proxy.cookie_settings",
                "proxy.cookie_key",
                "proxy.upstream.trust",
                "proxy.upstream.health",
                "proxy.upstream.health.duckdb",
                "proxy.upstream.health.data_path",
            ]
        );
        assert_eq!(report.vantage(), Vantage::Web);
        assert_eq!(report.verdict(), trawl_api::doctor::Verdict::Pass);
    }

    #[test]
    #[should_panic(expected = "WebCheck::ALL order")]
    fn a_check_runs_only_once() {
        let mut runner = Runner::new(run_as(false));
        step(&mut runner, WebCheck::Config, || {
            Row::complete(WebCheck::Config)
        });
        step(&mut runner, WebCheck::Config, || {
            Row::complete(WebCheck::Config)
        });
    }

    #[test]
    #[should_panic(expected = "WebCheck::ALL order")]
    fn checks_out_of_order_are_a_bug() {
        let mut runner = Runner::new(run_as(false));
        let _ = runner.gate(WebCheck::Identity);
    }

    #[test]
    #[should_panic(expected = "was not recorded")]
    fn a_gate_must_be_recorded_before_the_next() {
        let mut runner = Runner::new(run_as(false));
        let _gate = runner.gate(WebCheck::Config);
        let _ = runner.gate(WebCheck::Identity);
    }

    #[test]
    #[should_panic(expected = "the check's own row")]
    fn a_gate_records_only_its_own_row() {
        let mut runner = Runner::new(run_as(false));
        let gate = runner.gate(WebCheck::Config).unwrap();
        runner.record(gate, Row::complete(WebCheck::Identity));
    }

    #[test]
    #[should_panic(expected = "only an access check's row asserts access")]
    fn only_an_access_check_carries_the_access_mark() {
        let mut runner = Runner::new(run_as(false));
        let gate = runner.gate(WebCheck::Config).unwrap();
        runner.record(gate, Row::complete(WebCheck::Config).access());
    }

    #[test]
    #[should_panic(expected = "every check records one row")]
    fn a_report_needs_every_check() {
        let mut runner = Runner::new(run_as(false));
        step(&mut runner, WebCheck::Config, || {
            Row::complete(WebCheck::Config)
        });
        let _ = runner.finish(target());
    }

    /// A check that did not complete blocks its dependents, transitively,
    /// each naming its own first unsatisfied prerequisite, and never looks.
    #[test]
    fn a_prerequisite_that_did_not_complete_blocks_its_dependents() {
        use WebCheck as C;
        let mut runner = Runner::new(run_as(false));
        step(&mut runner, C::Config, || Row::complete(C::Config));
        step(&mut runner, C::Identity, || Row::complete(C::Identity));
        step(&mut runner, C::PublicOrigins, || {
            Row::failed(C::PublicOrigins, "the origin list is empty")
        });
        step(&mut runner, C::CookieSettings, || unreachable!("blocked"));
        step(&mut runner, C::CookieKey, || {
            Row::not_configured(C::CookieKey, reason::EPHEMERAL_EACH_START)
        });
        step(&mut runner, C::UpstreamTrust, || {
            Row::not_sampled(C::UpstreamTrust, reason::CA_NOT_PRESENT)
        });
        step(&mut runner, C::UpstreamHealth, || unreachable!("blocked"));
        let report = runner.finish(target());
        let rows = summary(&report);
        assert_eq!(
            rows[3],
            (
                "proxy.cookie_settings",
                Outcome::NotSampled,
                Some("blocked"),
                Some("proxy.public_origins")
            )
        );
        assert_eq!(
            rows[6],
            (
                "proxy.upstream.health",
                Outcome::NotSampled,
                Some("blocked"),
                Some("proxy.upstream.trust")
            )
        );
        assert_eq!(report.verdict(), trawl_api::doctor::Verdict::Fail);

        // A failed config blocks every component, naming the config.
        let mut runner = Runner::new(run_as(false));
        step(&mut runner, C::Config, || {
            Row::failed(C::Config, "the configuration does not parse")
        });
        step(&mut runner, C::Identity, || Row::complete(C::Identity));
        for check in &C::ALL[2..] {
            step(&mut runner, *check, || unreachable!("blocked"));
        }
        let report = runner.finish(target());
        for (id, outcome, why, by) in &summary(&report)[2..] {
            assert_eq!(*outcome, Outcome::NotSampled, "{id}");
            assert_eq!(*why, Some("blocked"), "{id}");
            let expected = if *id == "proxy.upstream.health" {
                "proxy.upstream.trust"
            } else {
                "proxy.config"
            };
            assert_eq!(*by, Some(expected), "{id}");
        }
    }

    /// Run as root, only a `complete` row marked as asserting access
    /// becomes `not_sampled`, `ran_as_root`; content rows keep their
    /// outcome, a failed access row stays failed, and the run cannot pass.
    #[test]
    fn a_root_run_samples_content_not_access() {
        use WebCheck as C;
        let mut runner = Runner::new(run_as(true));
        for check in C::ALL {
            step(&mut runner, check, || {
                let row = Row::complete(check);
                if check == C::CookieKey {
                    row.access()
                } else {
                    row
                }
            });
        }
        let report = runner.finish(target());
        for (id, outcome, why, by) in summary(&report) {
            if id == "proxy.cookie_key" {
                assert_eq!(
                    (outcome, why),
                    (Outcome::NotSampled, Some(reason::RAN_AS_ROOT))
                );
            } else {
                assert_eq!(outcome, Outcome::Complete, "{id}");
            }
            assert_eq!(by, None, "{id}");
        }
        assert_eq!(report.verdict(), trawl_api::doctor::Verdict::Incomplete);

        // A key from the environment is content: complete, even as root.
        let mut runner = Runner::new(run_as(true));
        for check in &C::ALL[..5] {
            step(&mut runner, *check, || Row::complete(*check));
        }
        assert_eq!(
            runner.outcome(C::CookieKey),
            Some((Outcome::Complete, None))
        );

        // A failed access row stays failed as root.
        let mut runner = Runner::new(run_as(true));
        for check in &C::ALL[..4] {
            step(&mut runner, *check, || Row::complete(*check));
        }
        step(&mut runner, C::CookieKey, || {
            Row::failed(C::CookieKey, "the key file is not 32 bytes").access()
        });
        assert_eq!(
            runner.outcome(C::CookieKey),
            Some((Outcome::Failed, Some("the key file is not 32 bytes")))
        );
    }

    /// Run as root, the identity row is `not_sampled`, `ran_as_root`, so
    /// no root run exits 0 (D12); as another user it is `complete`.
    #[tokio::test]
    async fn identity_as_root_is_never_complete() {
        for (root, outcome) in [(true, Outcome::NotSampled), (false, Outcome::Complete)] {
            let mut runner = Runner::new(run_as(root));
            step(&mut runner, WebCheck::Config, || {
                Row::complete(WebCheck::Config)
            });
            check_identity(&mut runner, run_as(root)).await;
            let recorded = runner.outcome(WebCheck::Identity).unwrap();
            assert_eq!(recorded.0, outcome, "root {root}");
            if root {
                assert_eq!(recorded.1, Some(reason::RAN_AS_ROOT));
            }
        }
    }

    /// A panic under the doctor's hook writes the fixed line and nothing of
    /// its message or location. The hook is process-global, so another
    /// test panicking meanwhile may add lines; each must be the fixed one.
    #[test]
    fn the_panic_hook_prints_only_a_fixed_line() {
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct Sink(Arc<Mutex<Vec<u8>>>);
        impl io::Write for Sink {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let sink = Sink::default();
        let previous = std::panic::take_hook();
        let writer = sink.clone();
        install_panic_hook(move || writer.clone());
        let caught = std::panic::catch_unwind(|| {
            panic!("https://user:private-secret@upstream.example/");
        });
        std::panic::set_hook(previous);
        assert!(caught.is_err());

        let written = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
        assert!(!written.is_empty(), "the hook wrote nothing");
        for line in written.lines() {
            assert_eq!(line, PANIC_LINE, "{written}");
        }
    }

    /// Work that outlives its deadline costs the step its answer and does
    /// not hold the run; work that finishes answers.
    #[tokio::test]
    async fn blocking_work_past_its_deadline_is_timed_out() {
        let (release, blocked) = std::sync::mpsc::channel::<()>();
        let started = std::time::Instant::now();
        let outcome = blocking_within(Duration::from_millis(50), move || blocked.recv()).await;
        assert_eq!(outcome.map(|_| ()), Err(Unfinished::TimedOut));
        assert!(started.elapsed() < Duration::from_secs(5));
        drop(release);

        let row = unfinished_config(Unfinished::TimedOut, "the step did not finish");
        assert_eq!(
            (row.check(), row.outcome(), row.reason()),
            (
                WebCheck::Config,
                Outcome::NotSampled,
                Some(reason::TIMED_OUT)
            )
        );
        assert_eq!(
            blocking_within(Duration::from_secs(5), || 7).await,
            Ok(7),
            "work that finishes in time answers"
        );
    }

    /// `Ctx`'s `Debug` shows no configured value.
    #[test]
    fn ctx_debug_shows_no_value() {
        let ctx = Ctx {
            config_path: SelectedPath::new(Selection::ConfigFlag, Path::new("/etc/trawld.toml")),
            run_as: run_as(false),
            public_origins: Err(ConfigError::EnvMissing {
                name: "PRIVATE_SECRET".to_owned(),
            }),
            cookie_settings: Err(ConfigError::EnvKey {
                name: "PRIVATE_SECRET".to_owned(),
            }),
            cookie_key: Ok(KeySource::ConfigEnv {
                name: "PRIVATE_SECRET".to_owned(),
            }),
            upstream: Err(ConfigError::EnvUtf8 {
                name: "PRIVATE_SECRET".to_owned(),
            }),
            pinned_roots: None,
        };
        let shown = format!("{ctx:?}");
        assert!(!shown.contains("PRIVATE"), "{shown}");
    }

    /// The source files of the doctor, as compiled, and their names. The
    /// directory must hold exactly these, so a new file joins the guard.
    const DOCTOR_SOURCES: [(&str, &str); 5] = [
        ("mod.rs", include_str!("mod.rs")),
        ("output.rs", include_str!("output.rs")),
        ("read.rs", include_str!("read.rs")),
        ("settings.rs", include_str!("settings.rs")),
        ("upstream.rs", include_str!("upstream.rs")),
    ];

    /// Words the doctor's code must not contain (D15), in any path or
    /// import: startup's configuration, state, key generation and log
    /// setup; listeners and sockets; every call that creates, writes,
    /// moves or removes a file; and `macro_rules`. A macro such as `write!`
    /// is not a call of the same name: it formats into the sink it is
    /// given. `macro_rules!` is refused with its `!`: the doctor declares
    /// no macro, and one could rewrite what the scan reads, such as
    /// unwrapping a test module into running code.
    const FORBIDDEN_WORDS: [&str; 31] = [
        "ResolvedConfig",
        "AppState",
        "from_sources",
        "generate",
        "tracing_subscriber",
        "TcpListener",
        "TcpSocket",
        "UdpSocket",
        "UnixListener",
        "UnixDatagram",
        "bind",
        "listen",
        "socket",
        "OpenOptions",
        "create",
        "create_new",
        "create_dir",
        "create_dir_all",
        "write",
        "write_all",
        "write_at",
        "set_len",
        "set_permissions",
        "rename",
        "copy",
        "hard_link",
        "symlink",
        "remove_file",
        "remove_dir",
        "remove_dir_all",
        "macro_rules",
    ];

    /// The one forbidden word refused even as a macro: it defines macros.
    const MACRO_DEFINITION: &str = "macro_rules";

    /// Modules whose items the doctor names only as `module::Item` with the
    /// pair in [`ALLOWED_ITEMS`]: the file system and the network, in
    /// `std`, `tokio` or any other crate. A grouped import, a rename, a
    /// glob or the module alone is refused, so no spelling reaches an item
    /// that is not listed.
    const GUARDED_MODULES: [&str; 2] = ["fs", "net"];

    /// The read-only items the doctor uses from [`GUARDED_MODULES`]. The
    /// doctor reads files through `read::read` and the upstream through
    /// reqwest, so it names none of those directly.
    const ALLOWED_ITEMS: [(&str, &str); 1] = [
        // Name resolution only, as startup's listener resolves the listen
        // address before it binds: it opens no listener and connects
        // nothing beyond what the system resolver does.
        ("net", "ToSocketAddrs"),
    ];

    /// A token of the code the guard reads: a word (identifier or keyword)
    /// or one punctuation character. A literal is the single `"` token.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Token {
        Word(String),
        Punct(char),
    }

    /// The tokens of `source`, without comments and with every string,
    /// byte-string, raw-string and character literal reduced to one `"`,
    /// so text in a comment or literal neither trips the scan nor hides a
    /// brace from it. A raw identifier `r#name` is the word `name`.
    fn tokens(source: &str) -> Vec<Token> {
        /// The index just past the `quote` that closes a literal whose body
        /// starts at `i`, honouring backslash escapes.
        fn closed(chars: &[char], mut i: usize, quote: char) -> usize {
            while let Some(&c) = chars.get(i) {
                match c {
                    '\\' => i += 2,
                    c if c == quote => return i + 1,
                    _ => i += 1,
                }
            }
            i
        }
        let chars: Vec<char> = source.chars().collect();
        let at = |i: usize| chars.get(i).copied();
        let mut out = Vec::new();
        let mut i = 0;
        while let Some(c) = at(i) {
            if c == '/' && at(i + 1) == Some('/') {
                while at(i).is_some_and(|c| c != '\n') {
                    i += 1;
                }
            } else if c == '/' && at(i + 1) == Some('*') {
                // Block comments nest.
                let mut depth = 0_usize;
                while i < chars.len() {
                    if at(i) == Some('/') && at(i + 1) == Some('*') {
                        depth += 1;
                        i += 2;
                    } else if at(i) == Some('*') && at(i + 1) == Some('/') {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
            } else if c == '"' {
                out.push(Token::Punct('"'));
                i = closed(&chars, i + 1, '"');
            } else if c == '\'' {
                // A character literal, or else a lifetime or label.
                if at(i + 1) == Some('\\') {
                    out.push(Token::Punct('"'));
                    i = closed(&chars, i + 1, '\'');
                } else if at(i + 2) == Some('\'') {
                    out.push(Token::Punct('"'));
                    i += 3;
                } else {
                    out.push(Token::Punct('\''));
                    i += 1;
                }
            } else if c.is_alphabetic() || c == '_' {
                let start = i;
                while at(i).is_some_and(|c| c.is_alphanumeric() || c == '_') {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                let hashes = chars[i..].iter().take_while(|&&c| c == '#').count();
                match (word.as_str(), at(i)) {
                    ("r" | "br" | "cr", Some('"' | '#')) if at(i + hashes) == Some('"') => {
                        let close: Vec<char> = std::iter::once('"')
                            .chain(std::iter::repeat_n('#', hashes))
                            .collect();
                        out.push(Token::Punct('"'));
                        i = (i + hashes + 1..chars.len())
                            .find(|&j| chars[j..].starts_with(&close))
                            .map_or(chars.len(), |j| j + close.len());
                    }
                    // A raw identifier: the next pass reads its name.
                    ("r", Some('#')) if hashes == 1 => i += 1,
                    ("b" | "c", Some('"')) => {
                        out.push(Token::Punct('"'));
                        i = closed(&chars, i + 1, '"');
                    }
                    ("b", Some('\'')) => {
                        out.push(Token::Punct('"'));
                        i = closed(&chars, i + 1, '\'');
                    }
                    _ => out.push(Token::Word(word)),
                }
            } else {
                if !c.is_whitespace() {
                    out.push(Token::Punct(c));
                }
                i += 1;
            }
        }
        out
    }

    /// The tokens of `source` without each `#[cfg(test)] mod tests { .. }`
    /// module, which ends at its matching brace: the code the doctor runs.
    /// Code after a test module stays in the scan. A test may write its
    /// fixtures.
    ///
    /// Only a test module at the top level of the file is left out: one
    /// that starts inside any `(`, `[` or `{`, such as a macro call's
    /// arguments, is scanned like any other code, since a macro can turn
    /// it into running code.
    fn runtime_tokens(source: &str) -> Vec<Token> {
        let head = [
            Token::Punct('#'),
            Token::Punct('['),
            Token::Word("cfg".into()),
            Token::Punct('('),
            Token::Word("test".into()),
            Token::Punct(')'),
            Token::Punct(']'),
            Token::Word("mod".into()),
            Token::Word("tests".into()),
            Token::Punct('{'),
        ];
        let all = tokens(source);
        let mut code = Vec::new();
        // How deep in `(`, `[` and `{` the scan is, outside test modules.
        let mut nesting = 0_usize;
        let mut i = 0;
        while i < all.len() {
            if nesting == 0 && all[i..].starts_with(&head) {
                let mut depth = 0_usize;
                i += head.len() - 1;
                while let Some(token) = all.get(i) {
                    i += 1;
                    match token {
                        Token::Punct('{') => depth += 1,
                        Token::Punct('}') => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            } else {
                match all[i] {
                    Token::Punct('(' | '[' | '{') => nesting += 1,
                    Token::Punct(')' | ']' | '}') => nesting = nesting.saturating_sub(1),
                    _ => {}
                }
                code.push(all[i].clone());
                i += 1;
            }
        }
        code
    }

    /// What `source`'s runtime code names that D15 forbids: each forbidden
    /// word, and each file-system or network path that is not a listed
    /// `module::Item`.
    fn violations(source: &str) -> Vec<String> {
        let code = runtime_tokens(source);
        let mut found = Vec::new();
        for (i, token) in code.iter().enumerate() {
            let Token::Word(word) = token else { continue };
            let word = word.as_str();
            let called_as_macro = code.get(i + 1) == Some(&Token::Punct('!'));
            if FORBIDDEN_WORDS.contains(&word) && (!called_as_macro || word == MACRO_DEFINITION) {
                found.push(word.to_owned());
            }
            if GUARDED_MODULES.contains(&word) {
                let item = match (code.get(i + 1), code.get(i + 2), code.get(i + 3)) {
                    (Some(Token::Punct(':')), Some(Token::Punct(':')), Some(Token::Word(item))) => {
                        Some(item.as_str())
                    }
                    _ => None,
                };
                if !item.is_some_and(|item| ALLOWED_ITEMS.contains(&(word, item))) {
                    found.push(format!("{word}::{}", item.unwrap_or("{..}")));
                }
            }
        }
        found
    }

    /// The doctor's code names none of startup's side effects (D15): the
    /// running configuration or state, its only constructor, key
    /// generation, log setup, a listener, a socket, or a file write, and
    /// reaches the file system and the network only through listed items.
    ///
    /// Threat model. This is a drift guard against honest mistakes by the
    /// people who write this crate: a later change that imports a write,
    /// reaches for `ResolvedConfig`, or binds a socket, in whatever
    /// spelling came naturally. It reads tokens, not a parsed crate, so it
    /// is not a scanner for deliberately obfuscated code, and it does not
    /// try to be: code written to hide a write from it (a macro from
    /// another crate, a `cfg` trick, a build script) is out of its scope,
    /// and the answer to such code is review, not another guard rule. Its
    /// rules close the spellings review has shown to be plausible slips:
    /// grouped and renamed imports, raw identifiers, code after the test
    /// module, and a test module that a macro could unwrap, which is why
    /// `macro_rules` is refused and only a top-level test module is left
    /// out of the scan.
    ///
    /// The behavioral proof that the doctor never binds, never generates a
    /// key and never writes is not this guard. It is the subprocess tests,
    /// which run the real binary against filesystem snapshots:
    /// `web_doctor_never_binds` and `web_doctor_generates_no_key` in
    /// `tests/web_doctor/`.
    #[test]
    fn the_doctor_reaches_no_startup_side_effect() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/doctor");
        let mut on_disk: Vec<String> = std::fs::read_dir(&dir)
            .expect("list src/doctor")
            .map(|entry| {
                entry
                    .expect("an entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        on_disk.sort();
        let guarded: Vec<&str> = DOCTOR_SOURCES.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            on_disk, guarded,
            "add every file in src/doctor to DOCTOR_SOURCES"
        );

        for (name, source) in DOCTOR_SOURCES {
            assert_eq!(
                violations(source),
                Vec::<String>::new(),
                "src/doctor/{name} names a startup side effect"
            );
        }

        // The scan sees code and skips tests and comments.
        let planted = "fn f() {\n    // AppState is fine here\n    let s = App\
                       State::new();\n}\n#[cfg(test)]\nmod tests {\n    fs::write(x);\n}\n";
        assert_eq!(violations(planted), ["AppState"]);
    }

    /// Each way around a text scan that review named is caught: code after
    /// the test module, grouped and aliased imports of the file system and
    /// the network, and a write spelled without its module. Comments,
    /// literals, macros and the test module itself are not code the doctor
    /// runs, and stay out of the scan.
    #[test]
    fn the_guard_sees_through_every_spelling() {
        let caught = [
            (
                "code after the test module",
                "fn f() {}\n#[cfg(test)]\nmod tests {\n    fn t() {\n        let _ = \"}\";\n    }\n}\n\nfn later() {\n    std::fs::write(p, b).ok();\n}\n",
            ),
            (
                "a grouped write import",
                "use std::fs::{write};\nfn f() {\n    write(p, b).ok();\n}\n",
            ),
            (
                "the module imported as self",
                "use std::fs::{self};\nfn f() {\n    fs::write(p, b).ok();\n}\n",
            ),
            (
                "a grouped listener import",
                "use std::net::{TcpListener as L};\n",
            ),
            ("both modules in one group", "use std::{fs, net};\n"),
            (
                "the module renamed",
                "use tokio::fs as disk;\nasync fn f() {\n    disk::File::open(p).await.ok();\n}\n",
            ),
            (
                "a create call on its own",
                "fn f(o: &mut Opts) {\n    o.create(true);\n}\n",
            ),
            ("a raw identifier", "fn f() {\n    r#write(p, b);\n}\n"),
            (
                "a write from another module",
                "fn f() {\n    rustix::io::write(fd, b).ok();\n}\n",
            ),
            (
                "a socket syscall",
                "fn f() {\n    unsafe { libc::socket(2, 1, 0) };\n}\n",
            ),
            (
                "startup's state",
                "fn f() {\n    let s = AppState::new();\n}\n",
            ),
            (
                "a macro that unwraps a test module",
                "macro_rules! hide {\n    (#[cfg(test)] mod tests { $($body:tt)* }) => { $($body)* };\n}\n\
                 hide! {\n    #[cfg(test)]\n    mod tests {\n        \
                 fn f() {\n            std::fs::write(p, b).ok();\n        }\n    }\n}\n",
            ),
        ];
        for (name, planted) in caught {
            assert!(!violations(planted).is_empty(), "missed {name}");
        }

        // A test module inside another item or a macro call is scanned: a
        // macro from elsewhere could unwrap it too.
        for nested in [
            "other::unwrap! {\n    #[cfg(test)]\n    mod tests {\n        fn f() {\n            std::fs::write(p, b).ok();\n        }\n    }\n}\n",
            "other::unwrap!(#[cfg(test)] mod tests { fn f() { std::fs::write(p, b).ok(); } });\n",
            "mod inner {\n    #[cfg(test)]\n    mod tests {\n        fn f() {\n            std::fs::write(p, b).ok();\n        }\n    }\n}\n",
        ] {
            assert_eq!(violations(nested), ["fs::write", "write"], "{nested}");
        }
        assert_eq!(
            violations("macro_rules! m {\n    () => {};\n}\n"),
            ["macro_rules"]
        );

        let clean = "use std::io::Write as _;\n\
                     // std::fs::write and AppState in a comment\n\
                     /* a block comment: TcpListener { */\n\
                     fn f(out: &mut impl std::io::Write, k: &KeySource) -> std::io::Result<()> {\n    \
                         let _ = (k, '}', b'{', \"std::fs::write\", r#\"create(\"}\"#);\n    \
                         writeln!(out, \"write the key\")?;\n    \
                         write!(out, \"{}\", 1)\n\
                     }\n\
                     #[cfg(test)]\n\
                     mod tests {\n    \
                         fn t() {\n        \
                             std::fs::write(\"x\", b\"}\").unwrap();\n    \
                         }\n\
                     }\n";
        assert_eq!(violations(clean), Vec::<String>::new());
    }
}
