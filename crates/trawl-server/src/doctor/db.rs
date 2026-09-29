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
//! The doctor reads `SELECT`s only: the two ledgers, `catalog_state`, and
//! `pg_locks`. Both advisory locks it reports on, the `SQLx` migrator's
//! and trawld's writer lock, are observed in `pg_locks`, never taken.
//!
//! No error's text reaches a row. Every database error maps to a fixed
//! reason by its kind or SQLSTATE, and no row names the URL, host, user,
//! or password; the source names the setting the URL came from and where
//! `SQLx` found the password.

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgConnection};
use sqlx::{ConnectOptions as _, Connection as _};
use trawl_api::doctor::reason;

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

    /// Close the connection, if there is one, under [`DEADLINE`].
    async fn close(self) {
        if let Some(conn) = self.0 {
            let _ = tokio::time::timeout(DEADLINE, conn.close()).await;
        }
    }
}

/// Where `SQLx` found the password it will send, following its own
/// precedence (sqlx-postgres 0.9.0, `options/mod.rs` and
/// `options/parse.rs`): `PgConnectOptions::new_without_pgpass` starts from
/// `PGPASSWORD`; a password in the URL's userinfo or its `password` query
/// parameter replaces it; only when none of these gave one does
/// `apply_pgpass` read a password file, `PGPASSFILE` first, then
/// `~/.pgpass` (`options/pgpass.rs`, `load_password`).
#[derive(Debug, Clone, PartialEq, Eq)]
enum PasswordSource {
    /// The URL carries it.
    Url,
    /// `PGPASSWORD` is set and the URL carries none.
    PgPassword,
    /// A password file supplied it: `PGPASSFILE` when that is set, and
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
}

/// Parse `url` exactly as trawld's boot does, through `SQLx`, and say where
/// the password came from. `SQLx` reads the password file while parsing, so
/// this runs on the blocking pool: a FIFO in its place blocks a thread,
/// not the doctor. `Err` carries nothing, so no parse error text can reach
/// a row.
fn parse_options(url: &str) -> Result<(PgConnectOptions, PasswordSource), ()> {
    let options = PgConnectOptions::from_str(url).map_err(drop)?;
    // What the password resolution ended with, in SQLx's own URL type.
    let resolved = options.to_url_lossy();
    let in_url = reparse_as(&resolved, url).is_some_and(|given| {
        given.password().is_some() || given.query_pairs().any(|(key, _)| key == "password")
    });
    let source = if in_url {
        PasswordSource::Url
    } else if std::env::var("PGPASSWORD").is_ok() {
        PasswordSource::PgPassword
    } else if resolved.password().is_some() {
        // SQLx finds ~/.pgpass through std::env::home_dir, so the doctor
        // does too.
        let home = std::env::home_dir().is_some_and(|dir| dir.join(".pgpass").exists());
        PasswordSource::PassFile {
            pgpassfile: std::env::var_os("PGPASSFILE").map(PathBuf::from),
            home,
        }
    } else {
        PasswordSource::Unnamed
    };
    Ok((options, source))
}

/// Parse `text` as the type of `_like`. This names `SQLx`'s own URL type,
/// which it does not re-export, without a dependency of the doctor's own:
/// the URL the doctor inspects is parsed by the parser `SQLx` used.
fn reparse_as<T: FromStr>(_like: &T, text: &str) -> Option<T> {
    text.parse().ok()
}

/// `server.fleet.connect` or `server.app.connect`: resolve the URL, parse
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

    let parsed = tokio::time::timeout(
        DEADLINE,
        tokio::task::spawn_blocking(move || parse_options(&url)),
    )
    .await;
    let (options, password) = match parsed {
        Ok(Ok(Ok(parsed))) => parsed,
        // A panic while parsing is a URL SQLx could not take either.
        Ok(Ok(Err(())) | Err(_)) => {
            runner.record(
                gate,
                Row::failed(check, "the database URL does not parse")
                    .source(source)
                    .next(Text::new(
                        "write the URL as postgres://user@host:port/database",
                    )),
            );
            return Session(None);
        }
        Err(_) => {
            // The blocking read of a password file did not finish.
            runner.record(
                gate,
                Row::not_sampled(check, reason::TIMED_OUT)
                    .detail(Text::new("reading the password file took longer than 5 s"))
                    .source(source)
                    .next(Text::new(
                        "check that PGPASSFILE or ~/.pgpass is a regular file",
                    )),
            );
            return Session(None);
        }
    };
    let source = password.named_after(source);
    let options = options
        .application_name(APPLICATION_NAME)
        .options(SESSION_SETTINGS);

    match tokio::time::timeout(DEADLINE, PgConnection::connect_with(&options)).await {
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
            Session(None)
        }
        Ok(Err(error)) => {
            runner.record(gate, connect_failed(check, &error).source(source));
            Session(None)
        }
        Ok(Ok(conn)) => {
            runner.record(
                gate,
                Row::complete(check)
                    .detail(Text::new("authenticated; the session is read-only"))
                    .source(source),
            );
            Session(Some(conn))
        }
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
            _ => Row::failed(check, "the database refused a read-only query").next(Text::new(
                "read the database server's log for this connection",
            )),
        },
        sqlx::Error::Io(_) => Row::failed(check, "the database connection broke")
            .next(Text::new("check the database server, then rerun")),
        _ => Row::failed(check, "a read-only query failed").next(Text::new(
            "read the database server's log for this connection",
        )),
    }
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
        .step(async |conn| migrations::migrator_lock_held(conn).await)
        .await;
    let ledger = session
        .step(async |conn| migrations::validate_schema(conn).await)
        .await;
    let after = session
        .step(async |conn| migrations::migrator_lock_held(conn).await)
        .await;

    let ledger = match ledger {
        None => return runner.record(gate, timed_out(check)),
        Some(Err(refused)) => return runner.record(gate, app_refused(check, &refused)),
        Some(Ok(ledger)) => ledger,
    };
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
        .step(async |conn| {
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
        .step(async |conn| store::writer_lock_held(conn).await)
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
    /// the URL", as `SQLx` reads both. The test process's own environment
    /// decides the other sources, so only the URL cases are asserted.
    #[test]
    fn a_password_in_the_url_is_named_so() {
        for url in [
            "postgres://user:private-secret@127.0.0.1:1/db",
            "postgres://user@127.0.0.1:1/db?password=private-secret",
        ] {
            let (_, source) = parse_options(url).unwrap();
            assert_eq!(source, PasswordSource::Url, "{url}");
        }
        assert!(parse_options("postgres://user:pw@[::1/db").is_err());
    }
}
