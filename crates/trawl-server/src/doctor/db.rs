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
//! checks after it on that database are `not_sampled`, `timed_out`. Each
//! step runs in a task of its own, so a panic in `SQLx` while it decodes
//! what the server sent ends only that task: the connection is dropped with
//! it, the check is `not_sampled`, `protocol_error`, and the other groups
//! still run.
//!
//! Before connecting, the doctor refuses a startup field that holds a
//! control character, since a NUL ends the field early and drops the
//! settings after it, and a password file `SQLx` would read that is not a
//! regular file or is too large. It reads each TLS file the URL or the
//! `PGSSL*` environment names once, through [`fsread`], and hands `SQLx`
//! the bytes, so `SQLx` never opens one. Once connected, it reads the
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

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::str::FromStr;
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgConnection, PgSslMode};
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
/// variable that sets it when no parameter does, the most the doctor
/// reads of it, and how the bytes are handed to `SQLx` in its place.
struct TlsFile {
    setting: &'static str,
    keys: &'static [&'static str],
    env: &'static str,
    cap: u64,
    inline: fn(PgConnectOptions, Vec<u8>) -> PgConnectOptions,
}

/// Every TLS file `SQLx` 0.9.0 reads: `options/parse.rs`,
/// `parse_from_url`, and `options/mod.rs`, `new_without_pgpass`.
const TLS_FILES: [TlsFile; 3] = [
    TlsFile {
        setting: "sslrootcert",
        keys: &["sslrootcert", "ssl-root-cert", "ssl-ca"],
        env: "PGSSLROOTCERT",
        cap: fsread::cap::CERT,
        inline: PgConnectOptions::ssl_root_cert_from_pem,
    },
    TlsFile {
        setting: "sslcert",
        keys: &["sslcert", "ssl-cert"],
        env: "PGSSLCERT",
        cap: fsread::cap::CERT,
        inline: PgConnectOptions::ssl_client_cert_from_pem,
    },
    TlsFile {
        setting: "sslkey",
        keys: &["sslkey", "ssl-key"],
        env: "PGSSLKEY",
        cap: fsread::cap::KEY,
        inline: PgConnectOptions::ssl_client_key_from_pem,
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

/// Why a [`Session`] has no connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lost {
    /// The connect check did not complete, so no step runs on it.
    Unopened,
    /// A step missed [`DEADLINE`], and its connection was dropped.
    TimedOut,
    /// A step panicked, and its connection was dropped with its task.
    Panicked,
}

/// One step's work on a connection, boxed so it can run in a task of its
/// own.
type Step<'c, T> = Pin<Box<dyn Future<Output = T> + Send + 'c>>;

/// Wait at most [`DEADLINE`] for `task`. A task past the deadline is
/// aborted, which drops whatever it holds, such as a connection that may be
/// mid-exchange. A panic inside it, such as `SQLx` decoding a malformed
/// server message, ends only the task (`JoinError::is_panic`).
async fn contained<T>(mut task: tokio::task::JoinHandle<T>) -> Result<T, Lost> {
    match tokio::time::timeout(DEADLINE, &mut task).await {
        Ok(Ok(done)) => Ok(done),
        Ok(Err(joined)) if joined.is_panic() => Err(Lost::Panicked),
        // Nothing but the deadline aborts the task, so this is the runtime
        // shutting down under it.
        Ok(Err(_)) => Err(Lost::TimedOut),
        Err(_) => {
            task.abort();
            Err(Lost::TimedOut)
        }
    }
}

/// One database connection, or why there is none.
struct Session(Result<PgConnection, Lost>);

impl Session {
    /// Run one step on the connection, in a task of its own, under
    /// [`DEADLINE`]. When the step misses the deadline or panics, the
    /// connection is dropped and every later step answers the same
    /// [`Lost`] without running.
    async fn step<T: Send + 'static>(
        &mut self,
        run: impl for<'c> FnOnce(&'c mut PgConnection) -> Step<'c, T> + Send + 'static,
    ) -> Result<T, Lost> {
        let mut conn = match std::mem::replace(&mut self.0, Err(Lost::Unopened)) {
            Ok(conn) => conn,
            Err(lost) => {
                self.0 = Err(lost);
                return Err(lost);
            }
        };
        let task = tokio::spawn(async move {
            let done = run(&mut conn).await;
            (conn, done)
        });
        match contained(task).await {
            Ok((conn, done)) => {
                self.0 = Ok(conn);
                Ok(done)
            }
            Err(lost) => {
                // The protocol may be mid-exchange: nothing more is sent on it.
                self.0 = Err(lost);
                Err(lost)
            }
        }
    }

    /// [`Session::step`], with `read` in its own `BEGIN READ ONLY`
    /// transaction. For the reads that open no transaction of their own;
    /// the ledger validators open a read-only snapshot themselves.
    async fn read<T: Send + 'static>(
        &mut self,
        read: impl for<'c> FnOnce(&'c mut PgConnection) -> Step<'c, Result<T, sqlx::Error>>
        + Send
        + 'static,
    ) -> Result<Result<T, sqlx::Error>, Lost> {
        self.step(move |conn| Box::pin(read_only(conn, read))).await
    }

    /// Close the connection, if there is one, in a task of its own under
    /// [`DEADLINE`].
    async fn close(self) {
        if let Ok(conn) = self.0 {
            let _ = contained(tokio::spawn(conn.close())).await;
        }
    }
}

/// Run `read` in a `BEGIN READ ONLY` transaction and roll it back. An
/// error from `read` wins over an error rolling back.
async fn read_only<T>(
    conn: &mut PgConnection,
    read: impl for<'c> FnOnce(&'c mut PgConnection) -> Step<'c, Result<T, sqlx::Error>>,
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
    read_only(conn, |conn| {
        Box::pin(async move {
            for (show, expected) in SESSION_SHOWN {
                let shown: String = sqlx::query_scalar(show).fetch_one(&mut *conn).await?;
                if shown != expected {
                    return Ok(false);
                }
            }
            Ok(true)
        })
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

/// A password file the doctor does not let `SQLx` read, or a TLS file the
/// doctor could not read for it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileFault {
    file: NamedFile,
    kind: FileFaultKind,
    /// The most the doctor reads, or lets `SQLx` read, of this file.
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

/// Why the doctor does not connect with a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileFaultKind {
    /// Not a regular file: reading a FIFO or a device may block or never
    /// end.
    NotRegular,
    /// Larger than the doctor reads, or lets `SQLx` read.
    TooLarge,
    /// A TLS file that does not exist. `tls_required` says the URL's
    /// `sslmode` requires TLS, so trawld's boot reads the file on every
    /// connection and cannot connect; otherwise it reads it only when the
    /// server offers TLS.
    Missing { tls_required: bool },
    /// A TLS file the running user may not read.
    PermissionDenied,
    /// A TLS file the doctor could not read otherwise.
    Unreadable,
}

/// Whether `SQLx` may read the password file `path`: a regular file of at
/// most `cap` bytes, or nothing the doctor can stat. `SQLx` goes on without
/// a password file it cannot open, so that is not the doctor's to refuse.
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

/// Check the password files `SQLx` reads for this URL, when `password`
/// says it looks in them, before it reads either.
///
/// Trust posture (human decision 2026-09-29, the posture of the #265
/// data-root ruling): the credential files an operator selects are
/// trusted. This `stat` guards against an accident, such as a FIFO or a
/// multi-gigabyte file left at the path, not against someone who can
/// replace the file between this check and `SQLx`'s own read, which opens
/// the path again while it parses the URL. The TLS files are not left to
/// `SQLx` at all: [`inline_tls_files`] reads each once and hands it the
/// bytes.
fn check_pass_files(password: &PasswordSource) -> Result<(), FileFault> {
    let refuse = |file: NamedFile| {
        move |kind| FileFault {
            file,
            kind,
            cap: PASSFILE_CAP,
        }
    };
    if password.reads_pass_files() {
        if let Some(path) = std::env::var_os("PGPASSFILE").map(PathBuf::from) {
            admissible(&path, PASSFILE_CAP).map_err(refuse(NamedFile::PgPassFile(path.clone())))?;
        }
        if let Some(path) = home_pgpass() {
            admissible(&path, PASSFILE_CAP).map_err(refuse(NamedFile::HomePgPass))?;
        }
    }
    Ok(())
}

/// Read each TLS file `SQLx` would read for these options once, through
/// [`fsread`], and hand `SQLx` its bytes in place of its path, so `SQLx`
/// never opens it. The path is the one `SQLx` takes: the URL's last
/// matching parameter, else the `PGSSL*` variable when it names a path
/// rather than holding PEM. With an `sslmode` that never negotiates TLS,
/// `SQLx` reads none of them, and neither does the doctor.
fn inline_tls_files(
    pairs: &[(String, String)],
    mut options: PgConnectOptions,
) -> Result<PgConnectOptions, FileFault> {
    use fsread::ReadFault;
    let mode = options.get_ssl_mode();
    if matches!(mode, PgSslMode::Disable | PgSslMode::Allow) {
        return Ok(options);
    }
    let tls_required = !matches!(mode, PgSslMode::Prefer);
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
        let Some(path) = named else {
            continue;
        };
        let bytes =
            fsread::read_bounded(&path, file.cap, fsread::Links::Follow).map_err(|fault| {
                let kind = match fault {
                    ReadFault::NotRegular => FileFaultKind::NotRegular,
                    ReadFault::TooLarge => FileFaultKind::TooLarge,
                    ReadFault::Missing => FileFaultKind::Missing { tls_required },
                    ReadFault::PermissionDenied => FileFaultKind::PermissionDenied,
                    ReadFault::SymlinkLoop
                    | ReadFault::Changed
                    | ReadFault::Io
                    | ReadFault::TimedOut => FileFaultKind::Unreadable,
                };
                FileFault {
                    file: NamedFile::Tls(file.setting),
                    kind,
                    cap: file.cap,
                }
            })?;
        options = (file.inline)(options, bytes);
    }
    Ok(options)
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
    /// A password file `SQLx` would read is refused, or a TLS file could
    /// not be read.
    File(FileFault),
    /// A startup field holds a control character.
    ControlCharacter,
}

/// Check `url` and the password files `SQLx` reads for it, parse it
/// exactly as trawld's boot does, through `SQLx`, add the doctor's
/// settings, and hand `SQLx` the TLS files' bytes. `SQLx` reads the
/// password file while parsing, so this runs on the blocking pool: a file
/// swapped for a FIFO after its check blocks a thread, not the doctor.
/// Nothing of a parse error is kept, so no parse error text can reach a
/// row.
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
    check_pass_files(&password).map_err(Unprepared::File)?;
    let options = PgConnectOptions::from_url(&given)
        .map_err(|_| Unprepared::Unparsable)?
        .application_name(APPLICATION_NAME)
        .options(SESSION_SETTINGS);
    let options = inline_tls_files(&pairs, options).map_err(Unprepared::File)?;
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
        return Session(Err(Lost::Unopened));
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
        return Session(Err(Lost::Unopened));
    };
    let source = match from {
        DatabaseUrlSource::Environment => Text::new(db.env_var()).lit(" from the environment"),
        DatabaseUrlSource::ConfigFile => Text::new(db.setting()).lit(" in ").path(&ctx.config_path),
    };

    let (options, password) = match prepare_within_deadline(check, url).await {
        Ok(ready) => ready,
        Err(refused) => {
            runner.record(gate, refused.source(source));
            return Session(Err(Lost::Unopened));
        }
    };
    let source = password.named_after(source);

    let connecting = tokio::spawn(async move { PgConnection::connect_with(&options).await });
    let conn = match contained(connecting).await {
        Err(Lost::Panicked) => {
            runner.record(gate, protocol_error(check).source(source));
            return Session(Err(Lost::Unopened));
        }
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
            return Session(Err(Lost::Unopened));
        }
        Ok(Err(error)) => {
            runner.record(gate, connect_failed(check, &error).source(source));
            return Session(Err(Lost::Unopened));
        }
        Ok(Ok(conn)) => conn,
    };

    // Nothing else is sent until the session shows it took the settings.
    let mut session = Session(Ok(conn));
    match session
        .step(|conn| Box::pin(session_took_settings(conn)))
        .await
    {
        Ok(Ok(true)) => {
            runner.record(
                gate,
                Row::complete(check)
                    .detail(Text::new("authenticated; the session is read-only"))
                    .source(source),
            );
            session
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
            session.close().await;
            Session(Err(Lost::Unopened))
        }
        Ok(Err(error)) => {
            runner.record(gate, query_failed(check, &error).source(source));
            session.close().await;
            Session(Err(Lost::Unopened))
        }
        Err(lost) => {
            runner.record(gate, lost_row(check, lost).source(source));
            Session(Err(Lost::Unopened))
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

/// The row for a password file the doctor does not let `SQLx` read, or a
/// TLS file it could not read.
fn file_refused(check: ServerCheck, fault: &FileFault) -> Row {
    let named = match &fault.file {
        NamedFile::PgPassFile(path) => {
            Text::new("PGPASSFILE ").path(&SelectedPath::new(Selection::PgPassFile, path))
        }
        NamedFile::HomePgPass => Text::new("~/.pgpass"),
        NamedFile::Tls(setting) => Text::new("the file ").lit(setting).lit(" names"),
    };
    let bounded = || {
        Text::new("make it a regular file of at most ")
            .int(fault.cap)
            .lit(" bytes, or unset the setting that names it")
    };
    let did_not_connect = |text: Text| text.lit("; the doctor did not connect");
    match fault.kind {
        FileFaultKind::NotRegular => Row::not_sampled(check, reason::UNREADABLE)
            .detail(did_not_connect(named.lit(" is not a regular file")))
            .next(bounded()),
        FileFaultKind::TooLarge => Row::not_sampled(check, reason::TOO_LARGE)
            .detail(did_not_connect(
                named.lit(" is larger than ").int(fault.cap).lit(" bytes"),
            ))
            .next(bounded()),
        FileFaultKind::Missing { tls_required: true } => Row::failed(
            check,
            "a TLS file the URL's sslmode requires does not exist",
        )
        .detail(did_not_connect(named.lit(" does not exist")))
        .next(Text::new("point the setting at the file, or unset it")),
        FileFaultKind::Missing {
            tls_required: false,
        } => Row::not_sampled(check, reason::UNREADABLE)
            .detail(did_not_connect(named.lit(
                " does not exist; trawld reads it only when the server offers TLS",
            )))
            .next(Text::new("point the setting at the file, or unset it")),
        FileFaultKind::PermissionDenied => Row::not_sampled(check, reason::PERMISSION_DENIED)
            .detail(did_not_connect(
                named.lit(" may not be read by the running user"),
            ))
            .next(Text::new("rerun as the service user")),
        FileFaultKind::Unreadable => Row::not_sampled(check, reason::UNREADABLE)
            .detail(did_not_connect(named.lit(" could not be read")))
            .next(bounded()),
    }
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

/// The row for a step that panicked, or ran on a connection an earlier step
/// lost that way: `SQLx` could not decode what the server sent.
fn protocol_error(check: ServerCheck) -> Row {
    Row::not_sampled(check, reason::PROTOCOL_ERROR)
        .detail(Text::new(
            "the database server sent a message the driver could not decode; the doctor \
             dropped the connection",
        ))
        .next(Text::new(
            "check that the URL names a PostgreSQL server, not another service or a proxy \
             that alters the protocol",
        ))
}

/// The row for a step that did not run to its end.
fn lost_row(check: ServerCheck, lost: Lost) -> Row {
    match lost {
        Lost::Panicked => protocol_error(check),
        Lost::TimedOut | Lost::Unopened => timed_out(check),
    }
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
        .step(|conn| Box::pin(fleet_auth::validate_schema_on(conn)))
        .await
    {
        Err(lost) => lost_row(check, lost),
        Ok(Ok(())) => Row::complete(check).detail(Text::new("the ledger is current")),
        Ok(Err(Fleet::Uninitialized)) => {
            Row::failed(check, "the Fleet database has no schema").next(run_fleet_admin())
        }
        Ok(Err(Fleet::PendingMigration { .. })) => {
            Row::failed(check, "the Fleet schema is behind").next(run_fleet_admin())
        }
        Ok(Err(Fleet::LegacyHistory { .. })) => {
            Row::failed(check, "the Fleet ledger is from before 1.0").next(Text::new(
                "provision a new dedicated Fleet database and retain the old one",
            ))
        }
        Ok(Err(Fleet::UntrackedSchema)) => Row::failed(
            check,
            "the Fleet database holds objects no supported ledger tracks",
        )
        .next(Text::new(
            "check that the Fleet URL names the Fleet database; otherwise provision a new \
             dedicated one and retain this one",
        )),
        Ok(Err(Fleet::Migration(MigrateError::Dirty(_)))) => {
            Row::failed(check, "a Fleet migration is dirty").next(Text::new(
                "restore the Fleet database from a backup taken before the failed migration",
            ))
        }
        Ok(Err(Fleet::Migration(MigrateError::VersionMissing(_)))) => {
            Row::failed(check, "the Fleet ledger is ahead of this binary").next(Text::new(
                "run the fleet-admin and trawld release that applied the newer migration",
            ))
        }
        Ok(Err(Fleet::Migration(MigrateError::VersionMismatch(_)))) => {
            Row::failed(check, "a Fleet migration's checksum differs").next(Text::new(
                "check that the Fleet URL names the Fleet database this release migrated",
            ))
        }
        Ok(Err(Fleet::Migration(_))) => Row::failed(check, "the Fleet ledger does not validate")
            .next(Text::new("run fleet-admin migrate to see why")),
        Ok(Err(Fleet::Database(error))) => query_failed(check, &error),
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
    fn from_samples(samples: [&Result<Result<bool, sqlx::Error>, Lost>; 2]) -> Self {
        if samples.iter().any(|sample| matches!(sample, Ok(Ok(true)))) {
            Self::Held
        } else if samples.iter().all(|sample| matches!(sample, Ok(Ok(false)))) {
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
        .read(|conn| Box::pin(migrations::migrator_lock_held(conn)))
        .await;
    let ledger = session
        .step(|conn| Box::pin(migrations::validate_schema(conn)))
        .await;
    let after = session
        .read(|conn| Box::pin(migrations::migrator_lock_held(conn)))
        .await;

    let ledger = match ledger {
        Err(lost) => return runner.record(gate, lost_row(check, lost)),
        Ok(Err(refused)) => return runner.record(gate, app_refused(check, &refused)),
        Ok(Ok(ledger)) => ledger,
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
            // The row of the first sample that gave no answer.
            let unanswered = [&before, &after]
                .into_iter()
                .find_map(|sample| match sample {
                    Ok(Err(error)) => Some(query_failed(check, error)),
                    Err(lost) => Some(lost_row(check, *lost)),
                    Ok(Ok(_)) => None,
                });
            unanswered
                .unwrap_or_else(|| timed_out(check))
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
        .read(|conn| {
            Box::pin(async move {
                let catalog_id = catalog::read_catalog_id(&mut *conn).await?;
                let conformed = catalog::read_conformed(&mut *conn).await?;
                Ok::<_, sqlx::Error>((catalog_id, conformed))
            })
        })
        .await;
    let row = match read {
        Err(lost) => lost_row(check, lost),
        Ok(Err(sqlx::Error::RowNotFound)) => Row::failed(check, "catalog_state holds no catalog")
            .next(Text::new(
                "restore the app-state database from backup, or provision a new dedicated one",
            )),
        Ok(Err(error)) => query_failed(check, &error),
        Ok(Ok((catalog_id, conformed))) => {
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
        .read(|conn| Box::pin(store::writer_lock_held(conn)))
        .await
    {
        Err(lost) => lost_row(check, lost),
        Ok(Err(error)) => query_failed(check, &error),
        Ok(Ok(true)) => {
            ctx.writer_lock_held = Some(true);
            Row::complete_because(check, reason::HELD).detail(Text::new(
                "a session holds trawld's writer lock on this database; it may run on another host",
            ))
        }
        Ok(Ok(false)) => {
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
        let held = Ok(Ok(true));
        let free = Ok(Ok(false));
        let timed_out = Err(Lost::TimedOut);
        let failed = Ok(Err(sqlx::Error::RowNotFound));
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
        assert_eq!(
            refused("sslrootcert=/nonexistent/ca.pem".to_owned()),
            Some((
                NamedFile::Tls("sslrootcert"),
                FileFaultKind::Missing {
                    tls_required: false
                }
            ))
        );
        assert_eq!(
            refused("sslmode=verify-ca&sslrootcert=/nonexistent/ca.pem".to_owned()),
            Some((
                NamedFile::Tls("sslrootcert"),
                FileFaultKind::Missing { tls_required: true }
            ))
        );
        // An sslmode that never negotiates TLS reads no TLS file.
        for mode in ["disable", "allow"] {
            assert_eq!(
                refused(format!("sslmode={mode}&sslkey=/dev/zero")),
                None,
                "{mode}"
            );
        }
    }

    /// Each TLS file reaches `SQLx` as the bytes the doctor read, never as
    /// a path `SQLx` would open.
    #[test]
    fn tls_files_reach_sqlx_as_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let mut query = Vec::new();
        for (key, body) in [
            ("sslrootcert", "root-bytes"),
            ("sslcert", "cert-bytes"),
            ("sslkey", "key-bytes"),
        ] {
            let path = dir.path().join(format!("{key}.pem"));
            std::fs::write(&path, body).unwrap();
            query.push(format!("{key}={}", path.display()));
        }
        let url = format!(
            "postgres://user:pw@127.0.0.1:1/db?sslmode=verify-ca&{}",
            query.join("&")
        );
        let Ok((options, _)) = prepare(&url) else {
            panic!("the URL prepares");
        };
        let shown = format!("{options:?}");
        assert!(!shown.contains("File("), "{shown}");
        for body in ["root-bytes", "cert-bytes", "key-bytes"] {
            let bytes = format!("{:?}", body.as_bytes());
            assert!(
                shown.contains(&format!("Inline({bytes})")),
                "{body}: {shown}"
            );
        }
    }
}
