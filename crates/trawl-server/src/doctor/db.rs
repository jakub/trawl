// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `trawld --doctor` checks of the Fleet and app-state databases.
//!
//! Connections are bare `PgConnection`s with a bounded connect and query
//! time, in read-only transactions; no pool, no lock, no migration
//! (ADR-0021 ruling 3, ADR-0047). The group fills [`Ctx::app`] and
//! [`Ctx::writer_lock_held`] for the groups after it.
//!
//! Each database gets one connection, opened with the URL trawld's boot
//! would use and these startup settings: `default_transaction_read_only`
//! on, a 5 s `statement_timeout`, a 1 s `lock_timeout`, and
//! `application_name` `trawld-doctor`, which is how a watcher finds the
//! doctor's backend. Connecting and every query also run under a 5 s
//! deadline here; a step that misses it drops the connection, and the
//! checks after it on that database are `not_sampled`, `timed_out`.
//!
//! Before connecting, the doctor refuses a startup field that holds a
//! control character, since a NUL ends the field early and drops the
//! settings after it, and a password or TLS file `SQLx` would read that is
//! not a regular file or is too large. Once connected, it reads the
//! settings back, `default_transaction_read_only` first; a session that
//! did not take them is `not_sampled`, `session_not_read_only`, and gets
//! no other query. Every query runs in an explicit read-only transaction.
//!
//! The doctor reads `SELECT`s only: the two ledgers, `catalog_state`, and
//! `pg_locks`. Both advisory locks it reports on, the `SQLx` migrator's
//! and trawld's writer lock, are observed in `pg_locks`, never taken.
//!
//! No error's text reaches a row. Every database error maps to a fixed
//! reason by its kind or SQLSTATE, and no row names the URL, host, user,
//! or password; the source names the setting the URL came from and where
//! `SQLx` looks for the password. A refusal the server proves, such as a
//! failed login or a ledger boot refuses, is `failed`; a query that broke
//! or erred after the login proves nothing about boot and is
//! `not_sampled`.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgConnection};
use sqlx::{ConnectOptions as _, Connection as _};
use trawl_api::doctor::reason;

use super::fsread;
use super::output::{Row, SelectedPath, Selection, Text};
use super::{CatalogFacts, CatalogId, Ctx, Runner, ServerCheck};
use crate::config::{ConfigError, DatabaseUrlSource};
use crate::store::migrations::{self, Ledger, SchemaError};
use crate::store::{self, catalog};

/// This group's checks, in [`ServerCheck::ALL`] order. [`run`] gates them
/// one by one; the graph test in [`super`] reads this list.
#[cfg(test)]
pub(super) const CHECKS: [ServerCheck; 5] = [
    ServerCheck::FleetConnect,
    ServerCheck::FleetSchema,
    ServerCheck::AppConnect,
    ServerCheck::AppSchema,
    ServerCheck::AppWriter,
];

/// How long connecting, or any one query, may take.
const DEADLINE: Duration = Duration::from_secs(5);

/// The `application_name` of every doctor connection.
const APPLICATION_NAME: &str = "trawld-doctor";

/// The server settings every doctor session starts with. Read-only by
/// default, so a statement that writes fails instead of writing, and
/// bounded on the server as well as here.
const SESSION_SETTINGS: [(&str, &str); 3] = [
    ("default_transaction_read_only", "on"),
    ("statement_timeout", "5000"),
    ("lock_timeout", "1000"),
];

/// How a session shows each of [`SESSION_SETTINGS`], and the
/// `application_name`, once it took them: the statement that reads the
/// setting and what it shows. `default_transaction_read_only` comes first,
/// so a writable session answers nothing else.
const SESSION_SHOWN: [(&str, &str); 4] = [
    ("SHOW default_transaction_read_only", "on"),
    ("SHOW statement_timeout", "5s"),
    ("SHOW lock_timeout", "1s"),
    ("SHOW application_name", APPLICATION_NAME),
];

/// The most a password file may hold for the doctor to let `SQLx` read it.
/// `SQLx` reads it whole, while parsing the URL.
const PASSFILE_CAP: u64 = 1024 * 1024;

/// A TLS file `SQLx` reads while connecting: the setting's name as the
/// report shows it, the URL parameters that set it, the environment
/// variable that sets it when no parameter does, and the most the doctor
/// lets `SQLx` read.
struct TlsFile {
    setting: &'static str,
    keys: &'static [&'static str],
    env: &'static str,
    cap: u64,
}

/// Every TLS file `SQLx` 0.9.0 reads: `options/parse.rs`,
/// `parse_from_url`, and `options/mod.rs`, `new_without_pgpass`.
const TLS_FILES: [TlsFile; 3] = [
    TlsFile {
        setting: "sslrootcert",
        keys: &["sslrootcert", "ssl-root-cert", "ssl-ca"],
        env: "PGSSLROOTCERT",
        cap: fsread::cap::CERT,
    },
    TlsFile {
        setting: "sslcert",
        keys: &["sslcert", "ssl-cert"],
        env: "PGSSLCERT",
        cap: fsread::cap::CERT,
    },
    TlsFile {
        setting: "sslkey",
        keys: &["sslkey", "ssl-key"],
        env: "PGSSLKEY",
        cap: fsread::cap::KEY,
    },
];

/// Run this group's checks through `runner`.
pub(super) async fn run(ctx: &mut Ctx, runner: &mut Runner) {
    let mut fleet = connect(ctx, runner, Database::Fleet).await;
    fleet_schema(runner, &mut fleet).await;
    fleet.close().await;

    let mut app = connect(ctx, runner, Database::App).await;
    app_schema(ctx, runner, &mut app).await;
    app_writer(ctx, runner, &mut app).await;
    app.close().await;
}

/// The two databases trawld's boot connects to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Database {
    Fleet,
    App,
}

impl Database {
    const fn connect_check(self) -> ServerCheck {
        match self {
            Self::Fleet => ServerCheck::FleetConnect,
            Self::App => ServerCheck::AppConnect,
        }
    }

    /// The environment variable that names this database's URL.
    const fn env_var(self) -> &'static str {
        match self {
            Self::Fleet => "FLEET_DATABASE_URL",
            Self::App => "TRAWL_DATABASE_URL",
        }
    }

    /// The configuration setting that names this database's URL.
    const fn setting(self) -> &'static str {
        match self {
            Self::Fleet => "[auth] database_url",
            Self::App => "[storage] database_url",
        }
    }

    /// The URL and the setting it came from, resolved as boot resolves it.
    fn resolve(self, ctx: &Ctx) -> Result<(String, DatabaseUrlSource), ConfigError> {
        match self {
            Self::Fleet => ctx.config.auth.resolve_database_url_with_source(),
            Self::App => ctx.config.storage.resolve_database_url_with_source(),
        }
    }
}

/// One database connection, or none once a step missed its deadline or
/// the connect check did not complete.
struct Session(Option<PgConnection>);

impl Session {
    /// Run one step on the connection under [`DEADLINE`]. `None` when the
    /// step missed it, and the connection is dropped, or an earlier step
    /// on this connection already had.
    async fn step<T>(&mut self, run: impl AsyncFnOnce(&mut PgConnection) -> T) -> Option<T> {
        let conn = self.0.as_mut()?;
        let done = tokio::time::timeout(DEADLINE, run(conn)).await.ok();
        if done.is_none() {
            // The protocol may be mid-exchange: nothing more is sent on it.
            self.0 = None;
        }
        done
    }

    /// [`Session::step`], with `read` in its own `BEGIN READ ONLY`
    /// transaction. For the reads that open no transaction of their own;
    /// the ledger validators open a read-only snapshot themselves.
    async fn read<T>(
        &mut self,
        read: impl AsyncFnOnce(&mut PgConnection) -> Result<T, sqlx::Error>,
    ) -> Option<Result<T, sqlx::Error>> {
        self.step(async move |conn| read_only(conn, read).await)
            .await
    }

    /// Close the connection, if there is one, under [`DEADLINE`].
    async fn close(self) {
        if let Some(conn) = self.0 {
            let _ = tokio::time::timeout(DEADLINE, conn.close()).await;
        }
    }
}

/// Run `read` in a `BEGIN READ ONLY` transaction and roll it back. An
/// error from `read` wins over an error rolling back.
async fn read_only<T>(
    conn: &mut PgConnection,
    read: impl AsyncFnOnce(&mut PgConnection) -> Result<T, sqlx::Error>,
) -> Result<T, sqlx::Error> {
    let mut tx = conn.begin_with("BEGIN READ ONLY").await?;
    let read = read(&mut tx).await;
    let rollback = tx.rollback().await;
    let value = read?;
    rollback?;
    Ok(value)
}

/// Whether the session took [`SESSION_SETTINGS`] and the doctor's
/// `application_name`, read back one `SHOW` at a time in the order of
/// [`SESSION_SHOWN`], stopping at the first that differs.
async fn session_took_settings(conn: &mut PgConnection) -> Result<bool, sqlx::Error> {
    read_only(conn, async |conn| {
        for (show, expected) in SESSION_SHOWN {
            let shown: String = sqlx::query_scalar(show).fetch_one(&mut *conn).await?;
            if shown != expected {
                return Ok(false);
            }
        }
        Ok(true)
    })
    .await
}

/// Where `SQLx` looks for the password it will send, following its own
/// precedence (sqlx-postgres 0.9.0, `options/mod.rs` and
/// `options/parse.rs`): `PgConnectOptions::new_without_pgpass` starts from
/// `PGPASSWORD`; a password in the URL's userinfo or its `password` query
/// parameter replaces it; only when none of these gave one does
/// `apply_pgpass` read a password file, `PGPASSFILE` first, then
/// `~/.pgpass` (`options/pgpass.rs`, `load_password`). Decided from the URL
/// as given and the environment, never from what `SQLx` resolved, so the
/// doctor does not know whether a password file held a matching line.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PasswordSource {
    /// The URL carries it.
    Url,
    /// `PGPASSWORD` is set and the URL carries none.
    PgPassword,
    /// `SQLx` looks in a password file: `PGPASSFILE` when that is set, and
    /// `~/.pgpass`, when `home` says that file exists.
    PassFile {
        pgpassfile: Option<PathBuf>,
        home: bool,
    },
    /// No source gave one; the server may not ask.
    Unnamed,
}

impl PasswordSource {
    /// `text`, followed by where the password came from.
    fn named_after(&self, text: Text) -> Text {
        match self {
            Self::Url => text.lit("; password in the URL"),
            Self::PgPassword => text.lit("; password from PGPASSWORD"),
            Self::PassFile {
                pgpassfile: Some(path),
                home,
            } => {
                let text = text
                    .lit("; password from PGPASSFILE ")
                    .path(&SelectedPath::new(Selection::PgPassFile, path));
                if *home {
                    text.lit(" or ~/.pgpass")
                } else {
                    text
                }
            }
            Self::PassFile {
                pgpassfile: None, ..
            } => text.lit("; password from ~/.pgpass"),
            Self::Unnamed => text.lit("; no password named"),
        }
    }

    /// Whether `SQLx` reads a password file for this URL.
    const fn reads_pass_files(&self) -> bool {
        matches!(self, Self::PassFile { .. } | Self::Unnamed)
    }
}

/// A file `SQLx` would read while connecting that the doctor does not let
/// it read.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileFault {
    file: NamedFile,
    kind: FileFaultKind,
    /// The most the doctor lets `SQLx` read of this file.
    cap: u64,
}

/// Which file a [`FileFault`] is about, as the report may name it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NamedFile {
    /// The file `PGPASSFILE` names.
    PgPassFile(PathBuf),
    /// `~/.pgpass`.
    HomePgPass,
    /// A TLS file, by its setting's name. Its path comes from the URL or
    /// the environment, so the report does not show it.
    Tls(&'static str),
}

/// Why the doctor does not let `SQLx` read a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileFaultKind {
    /// Not a regular file: reading a FIFO or a device may block or never
    /// end.
    NotRegular,
    /// Larger than the doctor lets `SQLx` read.
    TooLarge,
}

/// Whether `SQLx` may read `path`: a regular file of at most `cap` bytes,
/// or nothing the doctor can stat. `SQLx` goes on without a password file
/// it cannot open, and reports a TLS file it cannot open as a connection
/// error, so neither is the doctor's to refuse.
fn admissible(path: &Path, cap: u64) -> Result<(), FileFaultKind> {
    let Ok(meta) = std::fs::metadata(path) else {
        return Ok(());
    };
    if !meta.is_file() {
        Err(FileFaultKind::NotRegular)
    } else if meta.len() > cap {
        Err(FileFaultKind::TooLarge)
    } else {
        Ok(())
    }
}

/// Whether `value` is a PEM document rather than a path, as `SQLx` 0.9.0
/// decides for the `PGSSL*` variables (`sqlx-core`, `net/tls/mod.rs`,
/// `From<String> for CertificateInput`). URL parameters are always paths.
fn is_inline_pem(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed.starts_with("-----BEGIN") && trimmed.ends_with("-----")
}

/// Check every file `SQLx` would read for this URL before it reads any:
/// the password files, when `password` says `SQLx` looks in them, and each
/// TLS file the URL's last matching parameter, or else its environment
/// variable, names.
fn check_files(pairs: &[(String, String)], password: &PasswordSource) -> Result<(), FileFault> {
    let refuse = |file: NamedFile, cap: u64| move |kind| FileFault { file, kind, cap };
    if password.reads_pass_files() {
        if let Some(path) = std::env::var_os("PGPASSFILE").map(PathBuf::from) {
            admissible(&path, PASSFILE_CAP)
                .map_err(refuse(NamedFile::PgPassFile(path.clone()), PASSFILE_CAP))?;
        }
        if let Some(path) = home_pgpass() {
            admissible(&path, PASSFILE_CAP).map_err(refuse(NamedFile::HomePgPass, PASSFILE_CAP))?;
        }
    }
    for file in &TLS_FILES {
        let named = pairs
            .iter()
            .rev()
            .find(|(key, _)| file.keys.contains(&key.as_str()))
            .map(|(_, value)| PathBuf::from(value))
            .or_else(|| {
                std::env::var(file.env)
                    .ok()
                    .filter(|value| !is_inline_pem(value))
                    .map(PathBuf::from)
            });
        if let Some(path) = named {
            admissible(&path, file.cap).map_err(refuse(NamedFile::Tls(file.setting), file.cap))?;
        }
    }
    Ok(())
}

/// `~/.pgpass`, found as `SQLx` finds it, through `std::env::home_dir`.
fn home_pgpass() -> Option<PathBuf> {
    std::env::home_dir().map(|dir| dir.join(".pgpass"))
}

/// Whether every field `SQLx` sends in the startup message is free of
/// control characters. The message is a list of NUL-terminated strings: a
/// NUL inside `options` ends it early and moves the doctor's own settings,
/// appended after it, out of the field the server reads them from.
fn startup_fields_clean(options: &PgConnectOptions) -> bool {
    [
        Some(options.get_username()),
        options.get_database(),
        options.get_options(),
        options.get_application_name(),
    ]
    .into_iter()
    .flatten()
    .all(|field| !field.chars().any(char::is_control))
}

/// Why [`prepare`] did not ready a URL for connecting.
#[derive(Debug)]
enum Unprepared {
    /// `SQLx` does not parse it.
    Unparsable,
    /// A file `SQLx` would read is refused.
    File(FileFault),
    /// A startup field holds a control character.
    ControlCharacter,
}

/// Check `url` and the files `SQLx` reads for it, then parse it exactly as
/// trawld's boot does, through `SQLx`, and add the doctor's settings.
/// `SQLx` reads the password file while parsing, so this runs on the
/// blocking pool: a file swapped for a FIFO after its check blocks a
/// thread, not the doctor. Nothing of a parse error is kept, so no parse
/// error text can reach a row.
fn prepare(url: &str) -> Result<(PgConnectOptions, PasswordSource), Unprepared> {
    // The URL as SQLx's own parser reads it, before SQLx resolves anything.
    let given = parse_like(PgConnectOptions::from_url, url).ok_or(Unprepared::Unparsable)?;
    let pairs: Vec<(String, String)> = given
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    let password = if given.password().is_some() || pairs.iter().any(|(key, _)| key == "password") {
        PasswordSource::Url
    } else if std::env::var("PGPASSWORD").is_ok() {
        PasswordSource::PgPassword
    } else {
        let pgpassfile = std::env::var_os("PGPASSFILE").map(PathBuf::from);
        let home = home_pgpass().is_some_and(|path| path.exists());
        if pgpassfile.is_none() && !home {
            PasswordSource::Unnamed
        } else {
            PasswordSource::PassFile { pgpassfile, home }
        }
    };
    check_files(&pairs, &password).map_err(Unprepared::File)?;
    let options = PgConnectOptions::from_url(&given)
        .map_err(|_| Unprepared::Unparsable)?
        .application_name(APPLICATION_NAME)
        .options(SESSION_SETTINGS);
    if !startup_fields_clean(&options) {
        return Err(Unprepared::ControlCharacter);
    }
    Ok((options, password))
}

/// Parse `text` as the URL type `from_url` takes. This names `SQLx`'s own
/// URL type, which it does not re-export, without a dependency of the
/// doctor's own: the URL the doctor inspects is parsed by the parser
/// `SQLx` uses.
fn parse_like<U: FromStr, T>(_from_url: fn(&U) -> Result<T, sqlx::Error>, text: &str) -> Option<U> {
    text.parse().ok()
}

/// `server.fleet.connect` or `server.app.connect`: resolve the URL, check
/// it, and connect once, read-only, within [`DEADLINE`].
async fn connect(ctx: &Ctx, runner: &mut Runner, db: Database) -> Session {
    let check = db.connect_check();
    let Some(gate) = runner.gate(check) else {
        return Session(None);
    };
    let Ok((url, from)) = db.resolve(ctx) else {
        // server.config refuses a configuration without this URL, so a
        // gate that opened always has one.
        runner.record(
            gate,
            Row::failed(check, "no database URL is set").next(
                Text::new("set ")
                    .lit(db.env_var())
                    .lit(" or ")
                    .lit(db.setting()),
            ),
        );
        return Session(None);
    };
    let source = match from {
        DatabaseUrlSource::Environment => Text::new(db.env_var()).lit(" from the environment"),
        DatabaseUrlSource::ConfigFile => Text::new(db.setting()).lit(" in ").path(&ctx.config_path),
    };

    let (options, password) = match prepare_within_deadline(check, url).await {
        Ok(ready) => ready,
        Err(refused) => {
            runner.record(gate, refused.source(source));
            return Session(None);
        }
    };
    let source = password.named_after(source);

    let mut conn = match tokio::time::timeout(DEADLINE, PgConnection::connect_with(&options)).await
    {
        Err(_) => {
            runner.record(
                gate,
                Row::not_sampled(check, reason::TIMED_OUT)
                    .detail(Text::new("no connection within 5 s"))
                    .source(source)
                    .next(Text::new(
                        "check that the database server is up and reachable from this host",
                    )),
            );
            return Session(None);
        }
        Ok(Err(error)) => {
            runner.record(gate, connect_failed(check, &error).source(source));
            return Session(None);
        }
        Ok(Ok(conn)) => conn,
    };

    // Nothing else is sent until the session shows it took the settings.
    match tokio::time::timeout(DEADLINE, session_took_settings(&mut conn)).await {
        Ok(Ok(true)) => {
            runner.record(
                gate,
                Row::complete(check)
                    .detail(Text::new("authenticated; the session is read-only"))
                    .source(source),
            );
            Session(Some(conn))
        }
        Ok(Ok(false)) => {
            runner.record(
                gate,
                Row::not_sampled(check, reason::SESSION_NOT_READ_ONLY)
                    .detail(Text::new(
                        "the session did not take the doctor's read-only setting, timeouts and \
                         application_name, so the doctor sent no other query",
                    ))
                    .source(source)
                    .next(Text::new(
                        "check the URL's options and PGOPTIONS, and connect to the database \
                         server directly rather than through a pooler that drops startup options",
                    )),
            );
            Session(Some(conn)).close().await;
            Session(None)
        }
        Ok(Err(error)) => {
            runner.record(gate, query_failed(check, &error).source(source));
            Session(Some(conn)).close().await;
            Session(None)
        }
        Err(_) => {
            runner.record(gate, timed_out(check).source(source));
            Session(None)
        }
    }
}

/// [`prepare`] on the blocking pool under [`DEADLINE`]. The error is the
/// connect check's row, without its source.
async fn prepare_within_deadline(
    check: ServerCheck,
    url: String,
) -> Result<(PgConnectOptions, PasswordSource), Row> {
    let prepared =
        tokio::time::timeout(DEADLINE, tokio::task::spawn_blocking(move || prepare(&url))).await;
    match prepared {
        Ok(Ok(Ok(ready))) => Ok(ready),
        // A panic while parsing is a URL SQLx could not take either.
        Ok(Ok(Err(Unprepared::Unparsable)) | Err(_)) => {
            Err(
                Row::failed(check, "the database URL does not parse").next(Text::new(
                    "write the URL as postgres://user@host:port/database",
                )),
            )
        }
        Ok(Ok(Err(Unprepared::File(fault)))) => Err(file_refused(check, &fault)),
        Ok(Ok(Err(Unprepared::ControlCharacter))) => Err(Row::not_sampled(
            check,
            reason::SESSION_NOT_READ_ONLY,
        )
        .detail(Text::new(
            "the user, database or options setting holds a control character, which would keep \
             the session from starting read-only; the doctor did not connect",
        ))
        .next(Text::new(
            "remove control characters from the URL and from PGUSER, PGDATABASE and PGOPTIONS",
        ))),
        // Checking or reading a password or TLS file did not finish.
        Err(_) => Err(Row::not_sampled(check, reason::TIMED_OUT)
            .detail(Text::new(
                "checking the password and TLS files took longer than 5 s",
            ))
            .next(Text::new(
                "check that PGPASSFILE, ~/.pgpass and the TLS files are regular files",
            ))),
    }
}

/// The row for a file the doctor does not let `SQLx` read.
fn file_refused(check: ServerCheck, fault: &FileFault) -> Row {
    let named = match &fault.file {
        NamedFile::PgPassFile(path) => {
            Text::new("PGPASSFILE ").path(&SelectedPath::new(Selection::PgPassFile, path))
        }
        NamedFile::HomePgPass => Text::new("~/.pgpass"),
        NamedFile::Tls(setting) => Text::new("the file ").lit(setting).lit(" names"),
    };
    let (outcome_reason, detail) = match fault.kind {
        FileFaultKind::NotRegular => (reason::UNREADABLE, named.lit(" is not a regular file")),
        FileFaultKind::TooLarge => (
            reason::TOO_LARGE,
            named.lit(" is larger than ").int(fault.cap).lit(" bytes"),
        ),
    };
    Row::not_sampled(check, outcome_reason)
        .detail(detail.lit("; the doctor did not connect"))
        .next(
            Text::new("make it a regular file of at most ")
                .int(fault.cap)
                .lit(" bytes, or unset the setting that names it"),
        )
}

/// The row for a connection the server or the network refused.
fn connect_failed(check: ServerCheck, error: &sqlx::Error) -> Row {
    match error {
        sqlx::Error::Database(db) => match db.code().as_deref() {
            // invalid_password, invalid_authorization_specification.
            Some("28P01" | "28000") => Row::failed(check, "authentication failed").next(Text::new(
                "check the role and password the URL, PGPASSWORD or the password file supplies",
            )),
            // invalid_catalog_name.
            Some("3D000") => Row::failed(check, "the database does not exist").next(Text::new(
                "check the database name in the URL, or create the database",
            )),
            // insufficient_privilege: the role lacks CONNECT.
            Some("42501") => Row::failed(check, "the role may not connect to this database")
                .next(Text::new("grant the role CONNECT on the database")),
            // too_many_connections.
            Some("53300") => Row::failed(check, "the database server refused more connections")
                .next(Text::new(
                    "raise max_connections, or stop what holds the connections",
                )),
            _ => Row::failed(check, "the database server refused the connection").next(Text::new(
                "read the database server's log for this connection",
            )),
        },
        sqlx::Error::Io(_) => Row::failed(check, "the database server is unreachable").next(
            Text::new("check that the database server is up and the URL names its host and port"),
        ),
        sqlx::Error::Tls(_) => Row::failed(check, "TLS to the database server failed").next(
            Text::new("check the URL's sslmode and the server's certificate"),
        ),
        sqlx::Error::Configuration(_) => Row::failed(check, "the connection settings are refused")
            .next(Text::new(
                "check the URL's parameters and the PG* environment",
            )),
        _ => Row::failed(check, "the database connection failed").next(Text::new(
            "read the database server's log for this connection",
        )),
    }
}

/// The row for a query that failed on a connection that authenticated.
/// None of these proves what boot would find, so each is `not_sampled`.
fn query_failed(check: ServerCheck, error: &sqlx::Error) -> Row {
    match error {
        sqlx::Error::Database(db) => match db.code().as_deref() {
            // query_canceled (statement_timeout), lock_not_available.
            Some("57014" | "55P03") => Row::not_sampled(check, reason::TIMED_OUT)
                .detail(Text::new("the query ran past the session's timeout")),
            // insufficient_privilege.
            Some("42501") => Row::not_sampled(check, reason::PERMISSION_DENIED).next(Text::new(
                "grant the role SELECT on what trawld reads, or rerun with trawld's own URL",
            )),
            // connection_exception, and the server ending the session:
            // admin_shutdown, crash_shutdown, cannot_connect_now, and
            // the rest of class 57P.
            Some(code) if code.starts_with("08") || code.starts_with("57P") => {
                lost_connection(check)
            }
            _ => Row::not_sampled(check, reason::QUERY_FAILED)
                .detail(Text::new(
                    "the database server answered a read-only query with an error",
                ))
                .next(Text::new(
                    "read the database server's log for this connection",
                )),
        },
        sqlx::Error::Io(_) => lost_connection(check),
        _ => Row::not_sampled(check, reason::QUERY_FAILED)
            .detail(Text::new(
                "a read-only query gave no answer the doctor reads",
            ))
            .next(Text::new(
                "read the database server's log for this connection",
            )),
    }
}

/// The row for a connection that broke, or that the server ended, after
/// it authenticated.
fn lost_connection(check: ServerCheck) -> Row {
    Row::not_sampled(check, reason::CONNECTION_LOST)
        .detail(Text::new("the connection broke after it authenticated"))
        .next(Text::new("check the database server, then rerun"))
}

/// The row for a step that missed [`DEADLINE`], or ran on a connection an
/// earlier step lost that way.
fn timed_out(check: ServerCheck) -> Row {
    Row::not_sampled(check, reason::TIMED_OUT)
        .detail(Text::new("a query on this connection ran past 5 s"))
        .next(Text::new("rerun once the database server answers promptly"))
}
/// `server.fleet.schema`: the Fleet ledger is current, in one read-only
/// snapshot. trawld's boot refuses an empty or behind Fleet schema, so
/// both fail; `fleet-admin migrate` fixes them.
async fn fleet_schema(runner: &mut Runner, session: &mut Session) {
    use fleet_auth::SchemaError as Fleet;
    use sqlx::migrate::MigrateError;

    let check = ServerCheck::FleetSchema;
    let Some(gate) = runner.gate(check) else {
        return;
    };
    let run_fleet_admin = || Text::new("run fleet-admin migrate against the Fleet database");
    let row = match session
        .step(async |conn| fleet_auth::validate_schema_on(conn).await)
        .await
    {
        None => timed_out(check),
        Some(Ok(())) => Row::complete(check).detail(Text::new("the ledger is current")),
        Some(Err(Fleet::Uninitialized)) => {
            Row::failed(check, "the Fleet database has no schema").next(run_fleet_admin())
        }
        Some(Err(Fleet::PendingMigration { .. })) => {
            Row::failed(check, "the Fleet schema is behind").next(run_fleet_admin())
        }
        Some(Err(Fleet::LegacyHistory { .. })) => {
            Row::failed(check, "the Fleet ledger is from before 1.0").next(Text::new(
                "provision a new dedicated Fleet database and retain the old one",
            ))
        }
        Some(Err(Fleet::UntrackedSchema)) => Row::failed(
            check,
            "the Fleet database holds objects no supported ledger tracks",
        )
        .next(Text::new(
            "check that the Fleet URL names the Fleet database; otherwise provision a new \
             dedicated one and retain this one",
        )),
        Some(Err(Fleet::Migration(MigrateError::Dirty(_)))) => {
            Row::failed(check, "a Fleet migration is dirty").next(Text::new(
                "restore the Fleet database from a backup taken before the failed migration",
            ))
        }
        Some(Err(Fleet::Migration(MigrateError::VersionMissing(_)))) => {
            Row::failed(check, "the Fleet ledger is ahead of this binary").next(Text::new(
                "run the fleet-admin and trawld release that applied the newer migration",
            ))
        }
        Some(Err(Fleet::Migration(MigrateError::VersionMismatch(_)))) => {
            Row::failed(check, "a Fleet migration's checksum differs").next(Text::new(
                "check that the Fleet URL names the Fleet database this release migrated",
            ))
        }
        Some(Err(Fleet::Migration(_))) => Row::failed(check, "the Fleet ledger does not validate")
            .next(Text::new("run fleet-admin migrate to see why")),
        Some(Err(Fleet::Database(error))) => query_failed(check, &error),
    };
    runner.record(gate, row);
}

/// What the migrator's lock looked like around the ledger read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockSeen {
    /// Held in at least one sample.
    Held,
    /// Free in both samples.
    Free,
    /// Not held in any sample that answered, and at least one did not.
    Unknown,
}

impl LockSeen {
    fn from_samples(samples: [&Option<Result<bool, sqlx::Error>>; 2]) -> Self {
        if samples
            .iter()
            .any(|sample| matches!(sample, Some(Ok(true))))
        {
            Self::Held
        } else if samples
            .iter()
            .all(|sample| matches!(sample, Some(Ok(false))))
        {
            Self::Free
        } else {
            Self::Unknown
        }
    }
}

/// `server.app.schema`: trawld's boot will admit the app-state ledger.
///
/// The migrator's lock is sampled before and after the ledger's snapshot.
/// A ledger boot refuses fails whatever the lock shows. A current ledger
/// is `complete` even while a migrator holds the lock. A fresh or behind
/// one is `complete`, `will_initialize`, when the lock was free in both
/// samples, and `not_sampled`, `migration_in_progress`, when either saw it
/// held: a migrator may be bringing it current right now.
///
/// On a current or behind ledger the check also reads `catalog_state` for
/// the storage group. Its `catalog_id` is written once, by the baseline
/// migration every admitted non-fresh ledger holds, and never updated, so
/// reading it just after the snapshot reads the value the snapshot held.
async fn app_schema(ctx: &mut Ctx, runner: &mut Runner, session: &mut Session) {
    let check = ServerCheck::AppSchema;
    let Some(gate) = runner.gate(check) else {
        return;
    };
    let before = session
        .read(async |conn| migrations::migrator_lock_held(conn).await)
        .await;
    let ledger = session
        .step(async |conn| migrations::validate_schema(conn).await)
        .await;
    let after = session
        .read(async |conn| migrations::migrator_lock_held(conn).await)
        .await;

    let ledger = match ledger {
        None => return runner.record(gate, timed_out(check)),
        Some(Err(refused)) => return runner.record(gate, app_refused(check, &refused)),
        Some(Ok(ledger)) => ledger,
    };
    ctx.app.ledger = Some(ledger);
    let lock = LockSeen::from_samples([&before, &after]);
    let row = match (ledger, lock) {
        (Ledger::Current, _) => Row::complete(check).detail(Text::new("the ledger is current")),
        (Ledger::Fresh | Ledger::Behind { .. }, LockSeen::Held) => {
            Row::not_sampled(check, reason::MIGRATION_IN_PROGRESS)
                .detail(Text::new(
                    "a session holds the migrator's lock while the ledger is not current",
                ))
                .next(Text::new("rerun once the migration finishes"))
        }
        (Ledger::Fresh | Ledger::Behind { .. }, LockSeen::Unknown) => {
            let failed = [&before, &after]
                .into_iter()
                .find_map(|sample| match sample {
                    Some(Err(error)) => Some(error),
                    _ => None,
                });
            match failed {
                Some(error) => query_failed(check, error),
                None => timed_out(check),
            }
            .detail(Text::new(
                "the migrator's lock could not be observed, so a running migration cannot be \
                 ruled out",
            ))
        }
        (Ledger::Fresh, LockSeen::Free) => Row::complete_because(check, reason::WILL_INITIALIZE)
            .detail(Text::new(
                "the database is empty; trawld creates the schema at its next start",
            )),
        (Ledger::Behind { pending }, LockSeen::Free) => Row::complete_because(
            check,
            reason::WILL_INITIALIZE,
        )
        .detail(
            Text::new("")
                .int(u64::try_from(pending).unwrap_or(u64::MAX))
                .lit(" embedded migrations are not applied; trawld applies them at its next start"),
        ),
    };
    if row.outcome() != trawl_api::doctor::Outcome::Complete || ledger == Ledger::Fresh {
        return runner.record(gate, row);
    }

    let read = session
        .read(async |conn| {
            let catalog_id = catalog::read_catalog_id(&mut *conn).await?;
            let conformed = catalog::read_conformed(&mut *conn).await?;
            Ok::<_, sqlx::Error>((catalog_id, conformed))
        })
        .await;
    let row = match read {
        None => timed_out(check),
        Some(Err(sqlx::Error::RowNotFound)) => Row::failed(check, "catalog_state holds no catalog")
            .next(Text::new(
                "restore the app-state database from backup, or provision a new dedicated one",
            )),
        Some(Err(error)) => query_failed(check, &error),
        Some(Ok((catalog_id, conformed))) => {
            ctx.app.catalog = Some(CatalogFacts {
                catalog_id: CatalogId(catalog_id),
                conformed,
            });
            row
        }
    };
    runner.record(gate, row);
}

/// The row for an app-state ledger boot refuses, or a ledger read that
/// failed.
fn app_refused(check: ServerCheck, refused: &SchemaError) -> Row {
    use sqlx::migrate::MigrateError;
    match refused {
        SchemaError::LegacyHistory { .. } => {
            Row::failed(check, "the app-state ledger is from before 1.0").next(Text::new(
                "provision a new dedicated app-state database and retain the old one; do not \
             rewrite its migration history",
            ))
        }
        SchemaError::UntrackedSchema => Row::failed(
            check,
            "the app-state database holds objects no supported ledger tracks",
        )
        .next(Text::new(
            "check that the app-state URL names trawld's own database; otherwise provision a \
             new dedicated one and retain this one",
        )),
        SchemaError::Migration(MigrateError::Dirty(_)) => {
            Row::failed(check, "an app-state migration is dirty").next(Text::new(
                "restore the app-state database from a backup taken before the failed migration",
            ))
        }
        SchemaError::Migration(MigrateError::VersionMissing(_)) => {
            Row::failed(check, "the app-state ledger is ahead of this binary").next(Text::new(
                "run the trawld release that applied the newer migration",
            ))
        }
        SchemaError::Migration(MigrateError::VersionMismatch(_)) => {
            Row::failed(check, "an app-state migration's checksum differs").next(Text::new(
                "check that the app-state URL names trawld's own database, not another \
                 application's",
            ))
        }
        SchemaError::Migration(_) => Row::failed(check, "the app-state ledger does not validate")
            .next(Text::new("start trawld and read its startup error")),
        SchemaError::Database(error) => query_failed(check, error),
    }
}

/// `server.app.writer`: whether some session holds trawld's writer lock.
/// Always `complete` when observed: a held lock is `held`, a free one
/// `not_observed`. The lock is never taken.
async fn app_writer(ctx: &mut Ctx, runner: &mut Runner, session: &mut Session) {
    let check = ServerCheck::AppWriter;
    let Some(gate) = runner.gate(check) else {
        return;
    };
    let row = match session
        .read(async |conn| store::writer_lock_held(conn).await)
        .await
    {
        None => timed_out(check),
        Some(Err(error)) => query_failed(check, &error),
        Some(Ok(true)) => {
            ctx.writer_lock_held = Some(true);
            Row::complete_because(check, reason::HELD).detail(Text::new(
                "a session holds trawld's writer lock on this database; it may run on another host",
            ))
        }
        Some(Ok(false)) => {
            ctx.writer_lock_held = Some(false);
            Row::complete_because(check, reason::NOT_OBSERVED).detail(Text::new(
                "no session holds trawld's writer lock on this database",
            ))
        }
    };
    runner.record(gate, row);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_sample_wins_and_a_missing_one_is_unknown() {
        let held = Some(Ok(true));
        let free = Some(Ok(false));
        let timed_out = None;
        let failed = Some(Err(sqlx::Error::RowNotFound));
        assert_eq!(LockSeen::from_samples([&free, &free]), LockSeen::Free);
        assert_eq!(LockSeen::from_samples([&free, &held]), LockSeen::Held);
        assert_eq!(LockSeen::from_samples([&held, &timed_out]), LockSeen::Held);
        assert_eq!(
            LockSeen::from_samples([&free, &timed_out]),
            LockSeen::Unknown
        );
        assert_eq!(LockSeen::from_samples([&failed, &free]), LockSeen::Unknown);
    }

    #[test]
    fn a_password_source_names_no_value() {
        let path = PathBuf::from("/etc/trawl/pgpass");
        for (source, shown) in [
            (PasswordSource::Url, "X; password in the URL"),
            (PasswordSource::PgPassword, "X; password from PGPASSWORD"),
            (
                PasswordSource::PassFile {
                    pgpassfile: Some(path.clone()),
                    home: false,
                },
                "X; password from PGPASSFILE /etc/trawl/pgpass",
            ),
            (
                PasswordSource::PassFile {
                    pgpassfile: Some(path),
                    home: true,
                },
                "X; password from PGPASSFILE /etc/trawl/pgpass or ~/.pgpass",
            ),
            (
                PasswordSource::PassFile {
                    pgpassfile: None,
                    home: true,
                },
                "X; password from ~/.pgpass",
            ),
            (PasswordSource::Unnamed, "X; no password named"),
        ] {
            assert_eq!(source.named_after(Text::new("X")).as_str(), shown);
        }
    }

    /// A password in the userinfo or in the `password` parameter is "in
    /// the URL", as `SQLx` reads both, and an IPv6 `hostaddr` parses: the
    /// doctor never renders the options back into a URL, which `SQLx`
    /// 0.9.0 does with the host unbracketed and panics on. The test
    /// process's own environment decides the other sources, so only the
    /// URL cases are asserted.
    #[test]
    fn a_password_in_the_url_is_named_so() {
        for url in [
            "postgres://user:private-secret@127.0.0.1:1/db",
            "postgres://user@127.0.0.1:1/db?password=private-secret",
            "postgres://user:private-secret@localhost:1/db?hostaddr=::1",
        ] {
            let Ok((_, source)) = prepare(url) else {
                panic!("{url} is not ready");
            };
            assert_eq!(source, PasswordSource::Url, "{url}");
        }
        assert!(matches!(
            prepare("postgres://user:pw@[::1/db"),
            Err(Unprepared::Unparsable)
        ));
    }

    /// A control character in any startup field stops the connection
    /// before it is made; the password is not a startup field.
    #[test]
    fn a_control_character_in_a_startup_field_is_refused() {
        for url in [
            "postgres://user:pw@127.0.0.1:1/db?options=%00",
            "postgres://user:pw@127.0.0.1:1/db?options=-c%20work_mem%3D64kB%00",
            "postgres://us%01er:pw@127.0.0.1:1/db",
            "postgres://user:pw@127.0.0.1:1/d%0Ab",
            "postgres://user:pw@127.0.0.1:1/db?user=a%7Fb",
        ] {
            assert!(
                matches!(prepare(url), Err(Unprepared::ControlCharacter)),
                "{url}"
            );
        }
        assert!(matches!(
            prepare("postgres://user:p%00w@127.0.0.1:1/db?options=-c%20work_mem%3D64kB"),
            Ok(..)
        ));
    }

    /// A TLS file named in the URL that is not a regular file, or is too
    /// large, is refused before `SQLx` reads it; the last parameter wins,
    /// as it does in `SQLx`.
    #[test]
    fn a_tls_file_that_is_not_regular_or_too_large_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.pem");
        std::fs::File::create(&big)
            .unwrap()
            .set_len(fsread::cap::CERT + 1)
            .unwrap();
        let small = dir.path().join("small.pem");
        std::fs::write(&small, b"not a certificate").unwrap();
        let url = |query: String| format!("postgres://user:pw@127.0.0.1:1/db?{query}");
        let refused = |query: String| match prepare(&url(query)) {
            Err(Unprepared::File(fault)) => Some((fault.file, fault.kind)),
            _ => None,
        };
        assert_eq!(
            refused(format!("sslrootcert={}", dir.path().display())),
            Some((NamedFile::Tls("sslrootcert"), FileFaultKind::NotRegular))
        );
        assert_eq!(
            refused("ssl-key=/dev/zero".to_owned()),
            Some((NamedFile::Tls("sslkey"), FileFaultKind::NotRegular))
        );
        assert_eq!(
            refused(format!("sslcert={}", big.display())),
            Some((NamedFile::Tls("sslcert"), FileFaultKind::TooLarge))
        );
        assert_eq!(
            refused(format!(
                "ssl-ca={}&sslrootcert={}",
                big.display(),
                small.display()
            )),
            None
        );
        assert_eq!(refused("sslrootcert=/nonexistent/ca.pem".to_owned()), None);
    }
}
