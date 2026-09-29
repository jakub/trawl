// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawld --doctor`: check, from the server host, whether trawld will start
//! and serve with this configuration (ADR-0047 and its 2026-09-28
//! amendment, #269).
//!
//! `main` reaches [`run`] only after the process is sealed exactly as
//! `--check-config` seals it, and never enters the normal boot: no crash-dump
//! monitor, no tracing subscriber, no log file, no pool, no lock, no
//! migration. The doctor reads the configuration and the process
//! environment, connects to what the configuration names, and writes
//! nothing.
//!
//! The checks and their prerequisites are data ([`ServerCheck`]). One
//! [`Runner`] visits them in [`ServerCheck::ALL`] order: a check whose
//! prerequisite did not complete becomes `not_sampled`, reason `blocked`,
//! naming that prerequisite, and never looks. `server.config` and
//! `server.identity` are checked here; the rest belong to three groups, each
//! in its own file and each writing rows only through the runner:
//!
//! - [`db`]: the Fleet and app-state databases.
//! - [`storage`]: the data root and its recovery markers.
//! - [`listener`]: the certificate and trawld's own listener.
//!
//! Every row is built from [`output`]'s constructors, which accept no
//! free-form text, so no error text, URL, certificate name, catalog
//! identifier or listener address reaches the report. Every file is read
//! through [`fsread`].

use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use trawl_api::doctor::{Outcome, Report, Target, Vantage, reason};

use crate::config::{Config, ConfigError};
use crate::config_check::{LoadedFault, LogFileRefusal, check_loaded};

mod db;
pub mod fsread;
mod listener;
pub mod output;
mod storage;

pub use output::Format;
use output::{Row, SelectedPath, Selection, Text, UserName};

/// The named checks `trawld --doctor` runs, in order.
/// `server.listener.health.<key>` rows are part of
/// [`ServerCheck::ListenerHealth`] and share its prerequisite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerCheck {
    /// The configuration loads and validates as `--check-config` checks it.
    Config,
    /// The effective user and uid the doctor ran as.
    Identity,
    /// A connection to the Fleet database authenticates.
    FleetConnect,
    /// The Fleet ledger is current.
    FleetSchema,
    /// A connection to the app-state database authenticates.
    AppConnect,
    /// trawld's boot will admit the app-state ledger.
    AppSchema,
    /// Whether trawld's writer lock is held.
    AppWriter,
    /// The data root exists and is a directory, or boot will create it, and
    /// the running user may use it.
    DataRoot,
    /// The data root's epoch is current, or boot will initialize it.
    DataEpoch,
    /// The data root belongs to the app-state database's catalog.
    DataIdentity,
    /// Conformance is recorded, or will run at boot.
    DataConformance,
    /// No repin marker, or one whose phase boot completes.
    RecoveryRepin,
    /// Publication and rollup markers are readable and well formed.
    RecoveryPublication,
    /// The certificate and key parse, match, and are in date, or boot will
    /// generate them.
    TlsMaterial,
    /// The listener answers TLS with exactly the certificate on disk.
    ListenerIdentity,
    /// The listener's health endpoint answers, one row per reported check.
    ListenerHealth,
}

impl ServerCheck {
    /// Every check, in the order the runner visits them.
    pub const ALL: [Self; 16] = [
        Self::Config,
        Self::Identity,
        Self::FleetConnect,
        Self::FleetSchema,
        Self::AppConnect,
        Self::AppSchema,
        Self::AppWriter,
        Self::DataRoot,
        Self::DataEpoch,
        Self::DataIdentity,
        Self::DataConformance,
        Self::RecoveryRepin,
        Self::RecoveryPublication,
        Self::TlsMaterial,
        Self::ListenerIdentity,
        Self::ListenerHealth,
    ];

    /// The stable id the report carries.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Config => "server.config",
            Self::Identity => "server.identity",
            Self::FleetConnect => "server.fleet.connect",
            Self::FleetSchema => "server.fleet.schema",
            Self::AppConnect => "server.app.connect",
            Self::AppSchema => "server.app.schema",
            Self::AppWriter => "server.app.writer",
            Self::DataRoot => "server.data.root",
            Self::DataEpoch => "server.data.epoch",
            Self::DataIdentity => "server.data.identity",
            Self::DataConformance => "server.data.conformance",
            Self::RecoveryRepin => "server.recovery.repin",
            Self::RecoveryPublication => "server.recovery.publication",
            Self::TlsMaterial => "server.tls.material",
            Self::ListenerIdentity => "server.listener.identity",
            Self::ListenerHealth => "server.listener.health",
        }
    }

    /// The checks that must be satisfied before this one looks: `complete`
    /// with any reason, or `not_sampled` with reason `ran_as_root`.
    ///
    /// Everything but the identity row reads the configuration. Catalog
    /// identity compares the data root's marker with the ledger's
    /// `catalog_state`, so it waits on both. The listener is compared with
    /// the material on disk, so it waits on that, and health is read only
    /// from a listener that proved its identity.
    #[must_use]
    pub const fn prerequisites(self) -> &'static [Self] {
        match self {
            Self::Config | Self::Identity => &[],
            Self::FleetConnect | Self::AppConnect | Self::DataRoot | Self::TlsMaterial => {
                &[Self::Config]
            }
            Self::FleetSchema => &[Self::FleetConnect],
            Self::AppSchema | Self::AppWriter => &[Self::AppConnect],
            Self::DataEpoch => &[Self::DataRoot],
            Self::DataIdentity => &[Self::DataEpoch, Self::AppSchema],
            Self::DataConformance => &[Self::DataIdentity],
            Self::RecoveryRepin | Self::RecoveryPublication => &[Self::DataEpoch],
            Self::ListenerIdentity => &[Self::TlsMaterial],
            Self::ListenerHealth => &[Self::ListenerIdentity],
        }
    }

    /// Whether the check asserts that the running user may read or write
    /// something. A root run cannot observe that, so the runner reports a
    /// `complete` access check as `not_sampled`, reason `ran_as_root`
    /// (ADR-0047 amendment: root sees content, not access). Only the data
    /// root is an access check; database authentication and certificate
    /// reads are content.
    #[must_use]
    pub const fn is_access(self) -> bool {
        matches!(self, Self::DataRoot)
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

/// A catalog identifier as read from `catalog_state`. The report never
/// shows one, so neither does `Debug`.
#[derive(Clone, PartialEq, Eq)]
pub struct CatalogId(pub String);

impl std::fmt::Debug for CatalogId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CatalogId(..)")
    }
}

/// What `server.app.schema` read of `catalog_state`, just after its snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogFacts {
    /// `catalog_state.catalog_id`.
    pub catalog_id: CatalogId,
    /// Whether `catalog_state.conformed_at` is set.
    pub conformed: bool,
}

/// What the database group learned that the storage group needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppFacts {
    /// The ledger `server.app.schema` classified, when it read one boot
    /// admits. Set whatever the migrator's lock showed.
    pub ledger: Option<crate::store::migrations::Ledger>,
    /// `catalog_state`, when `server.app.schema` read it. `None` when that
    /// check did not complete, or completed on a ledger that has no
    /// catalog yet.
    pub catalog: Option<CatalogFacts>,
}

/// A certificate as DER bytes. Never shown, so `Debug` names its length
/// only.
#[derive(Clone, PartialEq, Eq)]
pub struct Der(pub Vec<u8>);

impl std::fmt::Debug for Der {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Der({} bytes)", self.0.len())
    }
}

/// What `server.tls.material` read, for the listener checks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TlsFacts {
    /// The leaf certificate on disk, when the material parsed.
    pub leaf: Option<Der>,
}

/// Everything the check groups share. Built once `server.config` loaded
/// the configuration; each group reads what it needs and fills in its own
/// facts for the groups after it.
pub struct Ctx {
    /// The configuration `server.config` loaded and validated.
    pub config: Config,
    /// The `--config` path, as the report may name it.
    pub config_path: SelectedPath,
    /// The user the doctor runs as.
    pub run_as: RunAs,
    /// Filled by [`db`]: what the app-state database holds.
    pub app: AppFacts,
    /// Filled by [`db`]: whether trawld's writer lock is held, when
    /// `server.app.writer` observed it.
    pub writer_lock_held: Option<bool>,
    /// Filled by [`listener`]: the material on disk.
    pub tls: TlsFacts,
    /// Filled by [`listener`]: nothing accepted a connection at the
    /// listener's address.
    pub listener_refused: bool,
}

/// `Config` carries database URLs, so `Ctx`'s `Debug` leaves it out.
impl std::fmt::Debug for Ctx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ctx")
            .field("config_path", &self.config_path)
            .field("run_as", &self.run_as)
            .field("app", &self.app)
            .field("writer_lock_held", &self.writer_lock_held)
            .field("tls", &self.tls)
            .field("listener_refused", &self.listener_refused)
            .finish_non_exhaustive()
    }
}

/// Leave to look at one check, from [`Runner::gate`]. Only the runner makes
/// one, and [`Runner::record`] consumes it.
#[derive(Debug)]
#[must_use = "a gated check must record its row"]
pub struct Gate {
    check: ServerCheck,
}

impl Gate {
    /// The check this gate opens.
    #[must_use]
    pub const fn check(&self) -> ServerCheck {
        self.check
    }
}

/// Visits the checks in [`ServerCheck::ALL`] order and collects their rows.
#[derive(Debug)]
pub struct Runner {
    root: bool,
    /// Each check's own row, in order: what prerequisites are judged on.
    outcomes: Vec<(ServerCheck, Outcome, Option<&'static str>)>,
    /// The check gated and not yet recorded.
    open: Option<ServerCheck>,
    rows: Vec<Row>,
    notes: Vec<String>,
}

impl Runner {
    /// A runner for a run as `run_as`.
    #[must_use]
    pub fn new(run_as: RunAs) -> Self {
        Self {
            root: run_as.root,
            outcomes: Vec::new(),
            open: None,
            rows: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// Ask to look at `check`, the next check in [`ServerCheck::ALL`].
    ///
    /// When a prerequisite is not satisfied, the runner records `check` as
    /// `not_sampled`, reason `blocked`, naming the first such prerequisite,
    /// and returns `None`: the check does not look.
    ///
    /// # Panics
    /// When `check` is not the next check, or the last gate was not
    /// recorded. Both are bugs in a group, never input.
    pub fn gate(&mut self, check: ServerCheck) -> Option<Gate> {
        assert!(self.open.is_none(), "a gated check was not recorded");
        assert_eq!(
            ServerCheck::ALL.get(self.outcomes.len()),
            Some(&check),
            "checks run once each, in ServerCheck::ALL order"
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
    pub fn record(&mut self, gate: Gate, row: Row) {
        self.record_with(gate, row, Vec::new());
    }

    /// Record the row of a gated check and its per-key rows, such as
    /// `server.listener.health` and its `server.listener.health.<key>`
    /// rows. The check's own row is what its dependents are judged on.
    ///
    /// # Panics
    /// When `row` is not the gated check's own row, or a keyed row belongs
    /// to another check.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "taking the gate by value spends it: one row per gate"
    )]
    pub fn record_with(&mut self, gate: Gate, row: Row, keyed: Vec<Row>) {
        let Gate { check } = gate;
        assert_eq!(self.open.take(), Some(check), "record follows gate");
        assert!(
            row.check() == check && !row.is_keyed(),
            "the check's own row"
        );
        assert!(
            keyed
                .iter()
                .all(|sub| sub.check() == check && sub.is_keyed()),
            "keyed rows of the gated check"
        );
        let row = if self.root && check.is_access() {
            row.into_root_run()
        } else {
            row
        };
        self.push(row);
        self.rows.extend(keyed);
    }

    /// The outcome and reason `check` recorded, once it has.
    #[must_use]
    pub fn outcome(&self, check: ServerCheck) -> Option<(Outcome, Option<&'static str>)> {
        self.outcomes
            .iter()
            .find(|(recorded, ..)| *recorded == check)
            .map(|(_, outcome, reason)| (*outcome, *reason))
    }

    /// Add a report-level note, which never changes an outcome.
    pub fn note(&mut self, note: Text) {
        self.notes.push(note.into_string());
    }

    fn satisfied(&self, check: ServerCheck) -> bool {
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
            ServerCheck::ALL.len(),
            "every check records one row"
        );
        let checks = self.rows.into_iter().map(Row::into_check).collect();
        Report::new(Vantage::Server, target, checks, self.notes)
    }
}

/// How long the doctor waits, at exit, for blocking work still running,
/// such as a file read past its deadline.
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(1);

/// How long the checks after parsing may take. With the file logger in use
/// they inspect `server.log_file` and the data root's markers, path
/// metadata a hung filesystem can stall, so they get a file read's budget.
const VALIDATE_DEADLINE: Duration = fsread::READ_DEADLINE;

/// How long the running user's name may take to look up. The password
/// database can be a network service.
const USER_LOOKUP_DEADLINE: Duration = Duration::from_secs(2);

/// Run `trawld --doctor --config <config>` and return its exit status: 0
/// pass, 1 fail, 3 incomplete (ADR-0047).
///
/// Call only from a sealed process (`seal_for_config_check`), on its main
/// thread, before any other thread exists.
#[must_use]
pub fn run(config: &Path, format: Option<Format>) -> u8 {
    install_panic_hook(io::stderr);
    let format = format.unwrap_or_else(Format::detect);
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        eprintln!("[trawld] doctor refused: its async runtime could not be built");
        return 1;
    };
    // The listener probe builds its TLS client from this provider. An
    // earlier install in this process is the same provider.
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
const PANIC_LINE: &str = "[trawld] doctor stopped a check: an internal error occurred";

/// Replace the default panic hook, which prints the panic's message and
/// location, with one that writes [`PANIC_LINE`] to `sink` and nothing
/// else. A dependency's panic message can quote what it was given, such as
/// a database URL, so no part of it reaches the output.
fn install_panic_hook<W: Write>(sink: impl Fn() -> W + Send + Sync + 'static) {
    std::panic::set_hook(Box::new(move |_| {
        let _ = writeln!(sink(), "{PANIC_LINE}");
    }));
}

/// Run every check and build the report. Nothing is rendered until the
/// whole report exists.
pub async fn check(config_path: &Path) -> Report {
    let run_as = RunAs::current();
    let shown = SelectedPath::new(Selection::ConfigFlag, config_path);
    let mut runner = Runner::new(run_as);

    let config = check_config(&mut runner, config_path, &shown).await;
    check_identity(&mut runner, run_as).await;

    if let Some(config) = config {
        let mut ctx = Ctx {
            config,
            config_path: shown.clone(),
            run_as,
            app: AppFacts::default(),
            writer_lock_held: None,
            tls: TlsFacts::default(),
            listener_refused: false,
        };
        db::run(&mut ctx, &mut runner).await;
        storage::run(&mut ctx, &mut runner).await;
        listener::run(&mut ctx, &mut runner).await;
        if ctx.writer_lock_held == Some(true) && ctx.listener_refused {
            runner.note(Text::new(
                "trawld's writer lock is held while the listener at the configured address \
                 refuses connections: another process may own this database",
            ));
        }
    } else {
        // Every other check reads the configuration, so each is blocked.
        for check in &ServerCheck::ALL[2..] {
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

/// `server.config`: read the file through the bounded reader, then run the
/// checks `--check-config` runs, on the blocking pool under
/// [`VALIDATE_DEADLINE`]. The validation text is never shown; the next
/// action is `--check-config`, which prints it.
async fn check_config(runner: &mut Runner, path: &Path, shown: &SelectedPath) -> Option<Config> {
    let gate = runner
        .gate(ServerCheck::Config)
        .expect("server.config has no prerequisite");
    let check = ServerCheck::Config;
    let source = Text::new("--config ").path(shown);
    let explain = Text::new("run trawld --check-config --config ")
        .path(shown)
        .lit(" to see why");
    let refused = |reason: &'static str| {
        Row::failed(check, reason)
            .source(source.clone())
            .next(explain.clone())
    };

    let loaded =
        match fsread::read(path.to_owned(), fsread::cap::CONFIG, fsread::Links::Follow).await {
            Err(fault) => Err(config_read_fault(fault).source(source.clone())),
            Ok(bytes) => match String::from_utf8(bytes) {
                Err(_) => Err(refused("the configuration is not UTF-8 text")),
                Ok(text) => match Config::from_toml(&text) {
                    Err(ConfigError::Parse { reason, .. }) => {
                        Err(refused("the configuration does not parse").detail(Text::new(reason)))
                    }
                    Err(ConfigError::Validation(_) | ConfigError::Io { .. }) => {
                        Err(refused("the configuration does not validate"))
                    }
                    Ok(config) => {
                        let validated = blocking_within(VALIDATE_DEADLINE, move || {
                            check_loaded(&config).map(|()| config)
                        })
                        .await;
                        match validated {
                            Ok(Ok(config)) => Ok(config),
                            Ok(Err(fault)) => Err(loaded_fault(&fault, &explain)),
                            Err(unfinished) => Err(unfinished_validation(unfinished)),
                        }
                        .map_err(|row| row.source(source.clone()))
                    }
                },
            },
        };
    match loaded {
        Ok(config) => {
            let row = Row::complete(check)
                .detail(Text::new("loads and validates"))
                .source(source);
            runner.record(gate, row);
            Some(config)
        }
        Err(row) => {
            runner.record(gate, row);
            None
        }
    }
}

/// The `server.config` row for a file the bounded reader did not read.
fn config_read_fault(fault: fsread::ReadFault) -> Row {
    use fsread::ReadFault;
    let check = ServerCheck::Config;
    match fault {
        ReadFault::Missing => Row::failed(check, "the configuration file does not exist")
            .next(Text::new("pass the path of trawld.toml to --config")),
        ReadFault::NotRegular => Row::failed(check, "the configuration is not a regular file")
            .next(Text::new("pass the path of trawld.toml to --config")),
        ReadFault::SymlinkLoop => Row::failed(check, "the configuration path is a symlink loop")
            .next(Text::new("pass the path of trawld.toml to --config")),
        ReadFault::PermissionDenied => Row::not_sampled(check, reason::PERMISSION_DENIED).next(
            Text::new("rerun as the service user, which can read its configuration"),
        ),
        ReadFault::TooLarge => Row::not_sampled(check, reason::TOO_LARGE).detail(
            Text::new("the doctor reads at most ")
                .int(fsread::cap::CONFIG)
                .lit(" bytes"),
        ),
        ReadFault::Changed => Row::not_sampled(check, reason::MATERIAL_CHANGED)
            .next(Text::new("rerun once the configuration stops changing")),
        ReadFault::TimedOut => Row::not_sampled(check, reason::TIMED_OUT),
        ReadFault::Io => Row::not_sampled(check, reason::UNREADABLE),
    }
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

/// The `server.config` row for checks after parsing that gave no answer.
fn unfinished_validation(unfinished: Unfinished) -> Row {
    let check = ServerCheck::Config;
    match unfinished {
        Unfinished::TimedOut => Row::not_sampled(check, reason::TIMED_OUT).detail(Text::new(
            "inspecting server.log_file and the data root's markers did not finish",
        )),
        Unfinished::Panicked => Row::not_sampled(check, reason::UNREADABLE),
    }
}

/// The `server.config` row for a configuration that parsed but that the
/// checks after parsing refuse, in `--check-config`'s order.
///
/// A `server.log_file` refusal fails the check only when it proves the
/// destination invalid. One the running user could not inspect, or that
/// failed to inspect for another reason, proves nothing about what trawld
/// will see, so it is `not_sampled`.
fn loaded_fault(fault: &LoadedFault, explain: &Text) -> Row {
    let check = ServerCheck::Config;
    match fault {
        LoadedFault::LogFile(error) => match LogFileRefusal::of(error) {
            LogFileRefusal::MarkerOverlap => {
                Row::failed(check, "server.log_file is a reserved storage marker").next(Text::new(
                    "select a log file outside the data root's markers",
                ))
            }
            LogFileRefusal::Invalid => {
                Row::failed(check, "server.log_file does not validate").next(explain.clone())
            }
            LogFileRefusal::PermissionDenied => Row::not_sampled(check, reason::PERMISSION_DENIED)
                .detail(Text::new("server.log_file could not be inspected"))
                .next(Text::new(
                    "rerun as the service user, which can reach its log destination",
                )),
            LogFileRefusal::Unobserved => Row::not_sampled(check, reason::UNREADABLE)
                .detail(Text::new("server.log_file could not be inspected"))
                .next(explain.clone()),
        },
        LoadedFault::FleetUrl(_) => Row::failed(check, "no Fleet database URL is set")
            .next(Text::new("set FLEET_DATABASE_URL or [auth] database_url")),
        LoadedFault::AppUrl(_) => Row::failed(check, "no app-state database URL is set").next(
            Text::new("set TRAWL_DATABASE_URL or [storage] database_url"),
        ),
        LoadedFault::Ingest => {
            Row::failed(check, "the ingest settings do not resolve").next(explain.clone())
        }
    }
}

/// `server.identity`: the effective uid and, when the password database
/// answers in time with a plain name, the user name. Always `complete`.
async fn check_identity(runner: &mut Runner, run_as: RunAs) {
    let gate = runner
        .gate(ServerCheck::Identity)
        .expect("server.identity has no prerequisite");
    let check = ServerCheck::Identity;
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
    if run_as.root {
        detail = detail.lit("; root reads content, so access checks are not sampled");
    }
    runner.record(gate, Row::complete(check).detail(detail));
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
    /// comes earlier in `ALL`, the edges are the design's, only the data
    /// root is an access check, and the three groups cover every check
    /// after config and identity, in order.
    #[test]
    fn server_checks_form_an_ordered_graph() {
        use ServerCheck as C;
        let ids: Vec<&str> = ServerCheck::ALL.iter().map(|c| c.id()).collect();
        assert_eq!(
            ids,
            [
                "server.config",
                "server.identity",
                "server.fleet.connect",
                "server.fleet.schema",
                "server.app.connect",
                "server.app.schema",
                "server.app.writer",
                "server.data.root",
                "server.data.epoch",
                "server.data.identity",
                "server.data.conformance",
                "server.recovery.repin",
                "server.recovery.publication",
                "server.tls.material",
                "server.listener.identity",
                "server.listener.health",
            ]
        );
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), ServerCheck::ALL.len());

        for (at, check) in ServerCheck::ALL.iter().enumerate() {
            for pre in check.prerequisites() {
                assert!(
                    ServerCheck::ALL[..at].contains(pre),
                    "{check:?} waits on {pre:?}, which does not come before it"
                );
            }
        }
        let edges: [(C, &[C]); 16] = [
            (C::Config, &[]),
            (C::Identity, &[]),
            (C::FleetConnect, &[C::Config]),
            (C::FleetSchema, &[C::FleetConnect]),
            (C::AppConnect, &[C::Config]),
            (C::AppSchema, &[C::AppConnect]),
            (C::AppWriter, &[C::AppConnect]),
            (C::DataRoot, &[C::Config]),
            (C::DataEpoch, &[C::DataRoot]),
            (C::DataIdentity, &[C::DataEpoch, C::AppSchema]),
            (C::DataConformance, &[C::DataIdentity]),
            (C::RecoveryRepin, &[C::DataEpoch]),
            (C::RecoveryPublication, &[C::DataEpoch]),
            (C::TlsMaterial, &[C::Config]),
            (C::ListenerIdentity, &[C::TlsMaterial]),
            (C::ListenerHealth, &[C::ListenerIdentity]),
        ];
        for (check, pres) in edges {
            assert_eq!(check.prerequisites(), pres, "{check:?}");
        }
        let access: Vec<C> = C::ALL.into_iter().filter(|c| c.is_access()).collect();
        assert_eq!(access, [C::DataRoot]);

        let grouped: Vec<C> =
            [&db::CHECKS[..], &storage::CHECKS[..], &listener::CHECKS[..]].concat();
        assert_eq!(grouped, C::ALL[2..]);
    }

    fn run_as(root: bool) -> RunAs {
        RunAs {
            uid: Some(if root { 0 } else { 1000 }),
            root,
        }
    }

    /// Record `row` for `check`, or nothing when the runner blocked it.
    fn step(runner: &mut Runner, check: ServerCheck, row: impl FnOnce() -> Row) {
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

    /// A check that did not complete blocks its dependents, transitively,
    /// each naming its own first unsatisfied prerequisite.
    #[test]
    fn a_prerequisite_that_did_not_complete_blocks_its_dependents() {
        use ServerCheck as C;
        let mut runner = Runner::new(run_as(false));
        step(&mut runner, C::Config, || Row::complete(C::Config));
        step(&mut runner, C::Identity, || Row::complete(C::Identity));
        step(&mut runner, C::FleetConnect, || {
            Row::failed(C::FleetConnect, "authentication failed")
        });
        step(&mut runner, C::FleetSchema, || unreachable!("blocked"));
        step(&mut runner, C::AppConnect, || Row::complete(C::AppConnect));
        step(&mut runner, C::AppSchema, || {
            Row::not_sampled(C::AppSchema, reason::MIGRATION_IN_PROGRESS)
        });
        step(&mut runner, C::AppWriter, || {
            Row::complete_because(C::AppWriter, reason::NOT_OBSERVED)
        });
        step(&mut runner, C::DataRoot, || {
            Row::complete_because(C::DataRoot, reason::WILL_INITIALIZE)
        });
        step(&mut runner, C::DataEpoch, || Row::complete(C::DataEpoch));
        step(&mut runner, C::DataIdentity, || unreachable!("blocked"));
        step(&mut runner, C::DataConformance, || unreachable!("blocked"));
        step(&mut runner, C::RecoveryRepin, || {
            Row::complete(C::RecoveryRepin)
        });
        step(&mut runner, C::RecoveryPublication, || {
            Row::complete(C::RecoveryPublication)
        });
        step(&mut runner, C::TlsMaterial, || {
            Row::complete(C::TlsMaterial)
        });
        step(&mut runner, C::ListenerIdentity, || {
            Row::not_sampled(C::ListenerIdentity, reason::NOT_LISTENING)
        });
        step(&mut runner, C::ListenerHealth, || unreachable!("blocked"));
        let report = runner.finish(target());
        let rows = summary(&report);
        assert_eq!(
            rows[3],
            (
                "server.fleet.schema",
                Outcome::NotSampled,
                Some("blocked"),
                Some("server.fleet.connect")
            )
        );
        assert_eq!(
            rows[9],
            (
                "server.data.identity",
                Outcome::NotSampled,
                Some("blocked"),
                Some("server.app.schema")
            )
        );
        assert_eq!(
            rows[10],
            (
                "server.data.conformance",
                Outcome::NotSampled,
                Some("blocked"),
                Some("server.data.identity")
            )
        );
        assert_eq!(
            rows[15],
            (
                "server.listener.health",
                Outcome::NotSampled,
                Some("blocked"),
                Some("server.listener.identity")
            )
        );
        // `complete` with any reason satisfies a prerequisite.
        assert_eq!(rows[8].1, Outcome::Complete);
        assert_eq!(report.verdict(), trawl_api::doctor::Verdict::Fail);
        assert_eq!(report.vantage(), Vantage::Server);
    }

    /// Run as root, a complete access check becomes `not_sampled`,
    /// `ran_as_root`, and still satisfies the checks that wait on it, so
    /// content checks report. A content check is left as it is, and the
    /// run cannot pass.
    #[test]
    fn a_root_run_samples_content_not_access() {
        use ServerCheck as C;
        let mut runner = Runner::new(run_as(true));
        for check in C::ALL {
            step(&mut runner, check, || Row::complete(check));
        }
        let report = runner.finish(target());
        let rows = summary(&report);
        for (id, outcome, reason, blocked_by) in &rows {
            if *id == "server.data.root" {
                assert_eq!(
                    (*outcome, *reason),
                    (Outcome::NotSampled, Some(reason::RAN_AS_ROOT))
                );
            } else {
                assert_eq!(*outcome, Outcome::Complete, "{id}");
            }
            assert_eq!(*blocked_by, None, "{id}");
        }
        assert_eq!(report.verdict(), trawl_api::doctor::Verdict::Incomplete);

        // A failed access check stays failed as root.
        let mut runner = Runner::new(run_as(true));
        step(&mut runner, C::Config, || Row::complete(C::Config));
        step(&mut runner, C::Identity, || Row::complete(C::Identity));
        for check in &C::ALL[2..7] {
            step(&mut runner, *check, || Row::complete(*check));
        }
        step(&mut runner, C::DataRoot, || {
            Row::failed(C::DataRoot, "the data root is not a directory")
        });
        assert_eq!(
            runner.outcome(C::DataRoot),
            Some((Outcome::Failed, Some("the data root is not a directory")))
        );
    }

    #[test]
    #[should_panic(expected = "ServerCheck::ALL order")]
    fn checks_out_of_order_are_a_bug() {
        let mut runner = Runner::new(run_as(false));
        let _ = runner.gate(ServerCheck::Identity);
    }

    #[test]
    #[should_panic(expected = "was not recorded")]
    fn a_gate_must_be_recorded_before_the_next() {
        let mut runner = Runner::new(run_as(false));
        let _gate = runner.gate(ServerCheck::Config);
        let _ = runner.gate(ServerCheck::Identity);
    }

    #[test]
    #[should_panic(expected = "every check records one row")]
    fn a_report_needs_every_check() {
        let mut runner = Runner::new(run_as(false));
        step(&mut runner, ServerCheck::Config, || {
            Row::complete(ServerCheck::Config)
        });
        let _ = runner.finish(target());
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
            panic!("postgres://user:private-secret@[::1/db");
        });
        std::panic::set_hook(previous);
        assert!(caught.is_err());

        let written = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
        assert!(!written.is_empty(), "the hook wrote nothing");
        for line in written.lines() {
            assert_eq!(line, PANIC_LINE, "{written}");
        }
    }

    #[test]
    fn debug_never_shows_a_catalog_id_or_certificate() {
        let facts = AppFacts {
            ledger: None,
            catalog: Some(CatalogFacts {
                catalog_id: CatalogId("0b7c9d2e-private".to_owned()),
                conformed: true,
            }),
        };
        let tls = TlsFacts {
            leaf: Some(Der(b"private-der".to_vec())),
        };
        let shown = format!("{facts:?} {tls:?}");
        assert!(!shown.contains("private"), "{shown}");
    }

    /// The configuration check reads through the bounded reader, so a FIFO
    /// in place of `trawld.toml` fails promptly instead of hanging the run,
    /// and every later check is blocked by it.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_fifo_config_fails_promptly_and_blocks_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("trawld.toml");
        crate::ingest::no_follow::test_support::make_fifo(&fifo);
        let report = tokio::time::timeout(Duration::from_secs(10), check(&fifo))
            .await
            .expect("the doctor returned");
        let rows = summary(&report);
        assert_eq!(
            rows[0],
            (
                "server.config",
                Outcome::Failed,
                Some("the configuration is not a regular file"),
                None
            )
        );
        assert_eq!(rows[1].0, "server.identity");
        assert_eq!(rows[1].1, Outcome::Complete);
        for row in &rows[2..] {
            assert_eq!(row.1, Outcome::NotSampled, "{}", row.0);
            assert_eq!(row.2, Some("blocked"), "{}", row.0);
        }
        assert_eq!(report.checks().len(), ServerCheck::ALL.len());
    }

    /// Validation that outlives its deadline costs `server.config` its
    /// answer, `not_sampled`/`timed_out`, and does not hold the run: the
    /// wait ends at the deadline while the work is still blocked.
    #[tokio::test]
    async fn validation_past_its_deadline_is_timed_out() {
        let (release, blocked) = std::sync::mpsc::channel::<()>();
        let started = std::time::Instant::now();
        let outcome = blocking_within(Duration::from_millis(50), move || blocked.recv()).await;
        assert_eq!(outcome.map(|_| ()), Err(Unfinished::TimedOut));
        assert!(started.elapsed() < Duration::from_secs(5));
        // Let the abandoned thread finish, so the test runtime can drop.
        drop(release);

        let row = unfinished_validation(Unfinished::TimedOut);
        assert_eq!(
            (row.check(), row.outcome(), row.reason()),
            (
                ServerCheck::Config,
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

    /// A `server.log_file` refusal is `failed` only when it proves the
    /// destination invalid; a denied or failed inspection is `not_sampled`.
    #[test]
    fn log_file_refusals_fail_only_when_proven() {
        use std::io::{Error, ErrorKind};
        let explain = Text::new("run trawld --check-config");
        let row = |error: Error| {
            let row = loaded_fault(&LoadedFault::LogFile(error), &explain);
            (row.outcome(), row.reason())
        };
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let overlap =
            crate::config_check::validate_log_destination(&data.join("EPOCH"), &data).unwrap_err();
        assert_eq!(
            row(overlap),
            (
                Outcome::Failed,
                Some("server.log_file is a reserved storage marker")
            )
        );
        assert_eq!(
            row(Error::new(ErrorKind::NotADirectory, "x")),
            (Outcome::Failed, Some("server.log_file does not validate"))
        );
        assert_eq!(
            row(Error::new(ErrorKind::PermissionDenied, "x")),
            (Outcome::NotSampled, Some(reason::PERMISSION_DENIED))
        );
        for kind in [
            ErrorKind::InvalidInput,
            ErrorKind::TimedOut,
            ErrorKind::NotFound,
        ] {
            assert_eq!(
                row(Error::new(kind, "x")),
                (Outcome::NotSampled, Some(reason::UNREADABLE)),
                "{kind:?}"
            );
        }
    }
}
