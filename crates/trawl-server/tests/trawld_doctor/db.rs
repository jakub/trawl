// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawld --doctor` tests of the Fleet and app-state database checks.
//!
//! Every database is real: minted empty by `common`, then migrated,
//! tampered with, or locked through `SQLx` itself as each test needs. The
//! doctor logs in as [`ROLE`], whose password is the planted
//! [`SECRET`](crate::support::SECRET), so every URL it is handed carries
//! the secret and none of its output may.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use sqlx::migrate::{Migrate as _, Migrator};
use sqlx::{Connection as _, Executor as _, PgConnection};
use trawl_api::doctor::{Check, Outcome, Report};

use crate::common;
use crate::support::{
    DoctorConfig, SECRET, assert_no_values, report, run_doctor, write_doctor_config,
};

/// The login role every doctor run here uses. Its password is [`SECRET`];
/// it inherits the admin role's privileges on the test databases.
const ROLE: &str = "trawl_doctor_planted";

/// The pre-1.0 app-state migrations boot refuses to adopt.
const LEGACY_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../scripts/schema-baseline/fixtures/trawl"
);

/// Make sure [`ROLE`] exists with the planted password and the admin
/// role's privileges. Idempotent, and serialized across the test processes
/// that share the cluster by a transaction-scoped lock on the admin
/// connection (the test's, never the doctor's).
async fn ensure_role() {
    let mut admin = PgConnection::connect(&common::admin_database_url())
        .await
        .expect("connect to the admin database");
    let mut tx = admin.begin().await.expect("begin");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('trawl_doctor_planted'))")
        .execute(&mut *tx)
        .await
        .expect("serialize role setup");
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1)")
            .bind(ROLE)
            .fetch_one(&mut *tx)
            .await
            .expect("look up the role");
    if !exists {
        tx.execute(sqlx::AssertSqlSafe(format!("CREATE ROLE {ROLE} LOGIN")))
            .await
            .expect("create the planted role");
    }
    tx.execute(sqlx::AssertSqlSafe(format!(
        "ALTER ROLE {ROLE} LOGIN PASSWORD '{SECRET}'"
    )))
    .await
    .expect("set the planted password");
    let admin_role: String = sqlx::query_scalar("SELECT current_user::text")
        .fetch_one(&mut *tx)
        .await
        .expect("admin role");
    tx.execute(sqlx::AssertSqlSafe(format!(
        r#"GRANT "{admin_role}" TO {ROLE}"#
    )))
    .await
    .expect("let the planted role read what the admin role owns");
    tx.commit().await.expect("commit role setup");
    admin.close().await.expect("close");
}

/// `url` (an admin URL naming a test database) with its userinfo replaced
/// by `user` and, when given, `password`.
fn with_login(url: &str, user: &str, password: Option<&str>) -> String {
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
fn planted(url: &str) -> String {
    with_login(url, ROLE, Some(SECRET))
}

/// What the report must not show of a database URL: the URL, its host and
/// port, its database name, and the role.
fn url_values(url: &str) -> Vec<String> {
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
    ]
}

/// A connection to `url` as the admin role.
async fn admin(url: &str) -> PgConnection {
    PgConnection::connect(url)
        .await
        .expect("connect to a test database")
}

/// A test database with the Fleet schema current.
async fn migrated_fleet() -> String {
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
async fn migrated_app() -> String {
    let url = common::create_app_database().await;
    let mut conn = admin(&url).await;
    trawl_server::store::migrations::MIGRATOR
        .run(&mut conn)
        .await
        .expect("migrate the app-state database");
    conn.close().await.unwrap();
    url
}

/// One doctor run over an ingest node in its own directory, with `env`
/// added to `HOME`. Returns the exit status, the parsed report, and both
/// streams, after asserting neither stream shows a planted value.
fn doctor(
    dir: &Path,
    config: &DoctorConfig,
    env: &[(&str, OsString)],
    planted_values: &[String],
) -> (i32, Report, String) {
    let path = write_doctor_config(dir, config);
    let mut full: Vec<(&str, OsString)> = vec![("HOME", dir.as_os_str().to_owned())];
    full.extend(env.iter().cloned());
    let args = [
        OsString::from("--doctor"),
        OsString::from("--config"),
        path.clone().into_os_string(),
        OsString::from("--format"),
        OsString::from("json"),
    ];
    let (code, stdout, stderr) = run_doctor(&args, &full);
    let values: Vec<&str> = planted_values.iter().map(String::as_str).collect();
    assert_no_values(&stdout, &stderr, &values);

    // The table form is held to the same rule.
    let args = [
        OsString::from("--doctor"),
        OsString::from("--config"),
        path.into_os_string(),
        OsString::from("--format"),
        OsString::from("table"),
    ];
    let (table_code, table_out, table_err) = run_doctor(&args, &full);
    assert_eq!(table_code, code, "{table_out}\n{table_err}");
    assert_no_values(&table_out, &table_err, &values);

    (code, report(&stdout), stderr)
}

/// A doctor run with both URLs in the environment.
fn doctor_env(dir: &Path, fleet: &str, app: &str) -> (i32, Report, String) {
    let mut planted_values = url_values(fleet);
    planted_values.extend(url_values(app));
    doctor(
        dir,
        &DoctorConfig::in_dir(dir),
        &[
            ("FLEET_DATABASE_URL", fleet.into()),
            ("TRAWL_DATABASE_URL", app.into()),
        ],
        &planted_values,
    )
}

fn row<'a>(report: &'a Report, id: &str) -> &'a Check {
    report
        .checks()
        .iter()
        .find(|check| check.id == id)
        .unwrap_or_else(|| panic!("no {id} row: {report:?}"))
}

/// `(outcome, reason)` of one row.
fn verdict<'a>(report: &'a Report, id: &str) -> (Outcome, Option<&'a str>) {
    let check = row(report, id);
    (check.outcome, check.reason.as_deref())
}

#[tokio::test]
async fn doctor_unmigrated_fleet_fails() {
    ensure_role().await;
    let fleet = common::create_fleet_database().await;
    let app = common::create_app_database().await;
    let dir = tempfile::tempdir().unwrap();

    let (code, report, stderr) = doctor_env(dir.path(), &planted(&fleet), &planted(&app));
    assert_eq!(code, 1, "{report:?}\n{stderr}");
    assert_eq!(
        verdict(&report, "server.fleet.connect"),
        (Outcome::Complete, None)
    );
    assert_eq!(
        row(&report, "server.fleet.connect").source.as_deref(),
        Some("FLEET_DATABASE_URL from the environment; password in the URL")
    );
    let schema = row(&report, "server.fleet.schema");
    assert_eq!(
        (schema.outcome, schema.reason.as_deref()),
        (Outcome::Failed, Some("the Fleet database has no schema"))
    );
    assert!(
        schema
            .next_action
            .as_deref()
            .is_some_and(|next| next.contains("fleet-admin migrate")),
        "{schema:?}"
    );
    // The empty app-state database is one boot initializes.
    assert_eq!(
        verdict(&report, "server.app.schema"),
        (Outcome::Complete, Some("will_initialize"))
    );
    assert_eq!(
        verdict(&report, "server.app.writer"),
        (Outcome::Complete, Some("not_observed"))
    );

    // Once migrated, the same Fleet database is current.
    let mut conn = admin(&fleet).await;
    fleet_auth::MIGRATOR.run(&mut conn).await.unwrap();
    conn.close().await.unwrap();
    let (_, report, _) = doctor_env(dir.path(), &planted(&fleet), &planted(&app));
    assert_eq!(
        verdict(&report, "server.fleet.schema"),
        (Outcome::Complete, None)
    );
}

/// The migrator's lock, taken by `SQLx`'s own `Migrate::lock` on a
/// dedicated connection, is seen: a fresh or behind ledger under it is
/// `migration_in_progress`, a current one is `complete`, and the same
/// ledgers with the lock free are `will_initialize`. The lock holder keeps
/// its lock throughout.
#[tokio::test]
async fn doctor_detects_real_migrator_lock() {
    use trawl_server::store::migrations::{BASELINE_VERSION, MIGRATOR};

    ensure_role().await;
    let fleet = migrated_fleet().await;
    let app = common::create_app_database().await;
    let dir = tempfile::tempdir().unwrap();
    let app_schema = |report: &Report| {
        let check = row(report, "server.app.schema").clone();
        (check.outcome, check.reason, check.detail)
    };
    let run = || doctor_env(dir.path(), &planted(&fleet), &planted(&app));

    // Fresh, lock free. Exit codes are left to the other groups' rows.
    let (_, report, _) = run();
    assert_eq!(
        app_schema(&report).1.as_deref(),
        Some("will_initialize"),
        "{report:?}"
    );

    let mut holder = admin(&app).await;
    let holder_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut holder)
        .await
        .unwrap();
    holder.lock().await.expect("take the migrator's lock");

    // Fresh, lock held.
    let (_, report, _) = run();
    let (outcome, reason, _) = app_schema(&report);
    assert_eq!(
        (outcome, reason.as_deref()),
        (Outcome::NotSampled, Some("migration_in_progress"))
    );

    // Behind, lock held: the holder applies the baseline only, through
    // SQLx's runner, which takes the same lock again on the same session.
    MIGRATOR
        .run_to(BASELINE_VERSION, &mut holder)
        .await
        .expect("apply the baseline");
    let (_, report, _) = run();
    let (outcome, reason, _) = app_schema(&report);
    assert_eq!(
        (outcome, reason.as_deref()),
        (Outcome::NotSampled, Some("migration_in_progress"))
    );

    // Behind, lock free.
    holder.unlock().await.expect("release the migrator's lock");
    let (_, report, _) = run();
    let (outcome, reason, detail) = app_schema(&report);
    assert_eq!(
        (outcome, reason.as_deref()),
        (Outcome::Complete, Some("will_initialize"))
    );
    let pending = MIGRATOR.iter().count() - 1;
    assert!(
        detail
            .as_deref()
            .is_some_and(|detail| detail.starts_with(&format!("{pending} embedded migrations"))),
        "{detail:?}"
    );

    // Current, lock held.
    holder.lock().await.expect("take the migrator's lock again");
    MIGRATOR.run(&mut holder).await.expect("apply the rest");
    let (_, report, _) = run();
    assert_eq!(app_schema(&report).0, Outcome::Complete, "{report:?}");
    assert_eq!(app_schema(&report).1, None, "{report:?}");

    // The holder kept its lock through every run.
    let mut watcher = admin(&app).await;
    let still: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_locks
         WHERE pid = $1 AND locktype = 'advisory' AND granted",
    )
    .bind(holder_pid)
    .fetch_one(&mut watcher)
    .await
    .unwrap();
    assert_eq!(still, 1, "the migrator's lock holder lost its lock");
    holder.unlock().await.unwrap();
    holder.close().await.unwrap();
    watcher.close().await.unwrap();
}

/// What the `pg_locks` watcher saw of the doctor's backends.
#[derive(Debug, Default)]
struct Watched {
    /// Samples taken.
    samples: u64,
    /// Samples in which a doctor backend was connected to each database.
    fleet_seen: u64,
    app_seen: u64,
    /// Advisory locks, held or awaited, by a doctor backend:
    /// `(database, classid, objid, objsubid, granted)`.
    advisory: Vec<(String, i64, i64, i32, bool)>,
}

/// One sampled row: a doctor backend's database, and its advisory lock's
/// `classid`, `objid`, `objsubid` and `granted`, when it has one.
type LockRow = (String, Option<i64>, Option<i64>, Option<i32>, Option<bool>);

/// Poll, back to back on one admin connection, every advisory lock row of
/// every backend whose `application_name` is the doctor's and whose
/// database is one of the two, until `stop` is set.
async fn watch_doctor_locks(
    fleet_db: String,
    app_db: String,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Watched {
    use std::sync::atomic::Ordering;
    let mut conn = PgConnection::connect(&common::admin_database_url())
        .await
        .expect("the watcher connects");
    let mut watched = Watched::default();
    while !stop.load(Ordering::SeqCst) {
        // One statement per sample, so activity and locks are read at one
        // instant; in autocommit each sample reads fresh statistics.
        let rows: Vec<LockRow> = sqlx::query_as(
            "SELECT a.datname::text, l.classid::int8, l.objid::int8, l.objsubid::int4, l.granted
                 FROM pg_stat_activity a
                 LEFT JOIN pg_locks l ON l.pid = a.pid AND l.locktype = 'advisory'
                 WHERE a.application_name = 'trawld-doctor' AND a.datname = ANY($1)",
        )
        .bind([fleet_db.clone(), app_db.clone()])
        .fetch_all(&mut conn)
        .await
        .expect("sample pg_locks");
        watched.samples += 1;
        if rows.iter().any(|row| row.0 == fleet_db) {
            watched.fleet_seen += 1;
        }
        if rows.iter().any(|row| row.0 == app_db) {
            watched.app_seen += 1;
        }
        for (database, classid, objid, objsubid, granted) in rows {
            if let (Some(classid), Some(objid), Some(objsubid), Some(granted)) =
                (classid, objid, objsubid, granted)
            {
                watched
                    .advisory
                    .push((database, classid, objid, objsubid, granted));
            }
        }
    }
    conn.close().await.unwrap();
    watched
}

/// The database name a test URL names.
fn database_of(url: &str) -> String {
    url.rsplit('/')
        .next()
        .and_then(|last| last.split('?').next())
        .unwrap()
        .to_owned()
}

/// The pid holding trawld's writer lock on `database`, if any.
async fn writer_pid(conn: &mut PgConnection, database: &str) -> Option<i32> {
    sqlx::query_scalar(
        "SELECT pid FROM pg_locks
         WHERE locktype = 'advisory' AND granted
           AND classid = 7631457 AND objid = 2003575089 AND objsubid = 1
           AND database = (SELECT oid FROM pg_database WHERE datname = $1)",
    )
    .bind(database)
    .fetch_optional(conn)
    .await
    .unwrap()
}

/// While a real trawld holds its writer lock, a watcher polling
/// `pg_locks` back to back sees the doctor's backends on both databases
/// and no advisory lock held or awaited by either; afterwards the same
/// trawld session still holds the writer lock and trawld still serves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doctor_takes_no_advisory_lock() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    ensure_role().await;
    let server = common::setup().await;
    let fleet_db = database_of(&server.fleet_db_url);
    let app_db = database_of(&server.app_db_url);
    let mut admin_conn = PgConnection::connect(&common::admin_database_url())
        .await
        .unwrap();
    let before = writer_pid(&mut admin_conn, &app_db)
        .await
        .expect("the running trawld holds its writer lock");

    let stop = Arc::new(AtomicBool::new(false));
    let sampler = tokio::spawn(watch_doctor_locks(
        fleet_db.clone(),
        app_db.clone(),
        Arc::clone(&stop),
    ));
    let dir = tempfile::tempdir().unwrap();
    let fleet = planted(&server.fleet_db_url);
    let app = planted(&server.app_db_url);
    let home = dir.path().to_owned();
    let (_, report, stderr) = tokio::task::spawn_blocking(move || doctor_env(&home, &fleet, &app))
        .await
        .unwrap();
    stop.store(true, Ordering::SeqCst);
    let watched = sampler.await.unwrap();

    assert_eq!(
        verdict(&report, "server.app.writer"),
        (Outcome::Complete, Some("held")),
        "{report:?}\n{stderr}"
    );
    assert_eq!(
        verdict(&report, "server.app.schema"),
        (Outcome::Complete, None)
    );
    assert_eq!(
        verdict(&report, "server.fleet.schema"),
        (Outcome::Complete, None)
    );
    assert!(
        watched.fleet_seen > 0 && watched.app_seen > 0,
        "the watcher never overlapped a doctor backend on both databases: {watched:?}"
    );
    assert!(
        watched.advisory.is_empty(),
        "the doctor's backends held or awaited advisory locks: {watched:?}"
    );
    assert_eq!(
        writer_pid(&mut admin_conn, &app_db).await,
        Some(before),
        "trawld's writer session lost its lock"
    );
    let health = common::harness_client_builder()
        .build()
        .unwrap()
        .get(format!("{}/api/v1/health", server.url))
        .send()
        .await
        .expect("trawld still answers");
    assert!(health.status().is_success(), "{}", health.status());

    // pg_locks lists every database's locks, and every trawld takes the
    // same key: a doctor pointed at another app-state database does not
    // see this trawld's lock, which is still held.
    let other = planted(&migrated_app().await);
    let fleet = planted(&server.fleet_db_url);
    let home = dir.path().to_owned();
    let (_, report, _) = tokio::task::spawn_blocking(move || doctor_env(&home, &fleet, &other))
        .await
        .unwrap();
    assert_eq!(
        verdict(&report, "server.app.writer"),
        (Outcome::Complete, Some("not_observed"))
    );
    assert_eq!(writer_pid(&mut admin_conn, &app_db).await, Some(before));
    admin_conn.close().await.unwrap();
}

/// Every app-state ledger boot refuses is `failed` with its own stable
/// reason, whatever else the run finds, and the run exits 1.
#[tokio::test]
async fn doctor_boot_refusals_fail() {
    use trawl_server::store::migrations::BASELINE_VERSION;

    ensure_role().await;
    let fleet = migrated_fleet().await;
    let dir = tempfile::tempdir().unwrap();

    let cases: [(&str, &str); 6] = [
        ("dirty", "an app-state migration is dirty"),
        ("ahead", "the app-state ledger is ahead of this binary"),
        ("checksum", "an app-state migration's checksum differs"),
        ("legacy", "the app-state ledger is from before 1.0"),
        (
            "untracked",
            "the app-state database holds objects no supported ledger tracks",
        ),
        // Pointed at the Fleet database: a ledger of another application.
        ("foreign", "an app-state migration's checksum differs"),
    ];
    for (state, expected) in cases {
        let app = match state {
            "foreign" => migrated_fleet().await,
            "legacy" => {
                let url = common::create_app_database().await;
                let mut conn = admin(&url).await;
                Migrator::new(PathBuf::from(LEGACY_DIR))
                    .await
                    .unwrap()
                    .run(&mut conn)
                    .await
                    .unwrap();
                conn.close().await.unwrap();
                url
            }
            "untracked" => {
                let url = common::create_app_database().await;
                let mut conn = admin(&url).await;
                conn.execute("CREATE TABLE sentinel (payload TEXT)")
                    .await
                    .unwrap();
                conn.close().await.unwrap();
                url
            }
            _ => {
                let url = migrated_app().await;
                let tamper = match state {
                    "dirty" => format!(
                        "UPDATE _sqlx_migrations SET success = false WHERE version = {BASELINE_VERSION}"
                    ),
                    "ahead" => "INSERT INTO _sqlx_migrations \
                        (version, description, success, checksum, execution_time) \
                        VALUES (29990101000001, 'from a newer release', true, '\\x00', 0)"
                        .to_owned(),
                    _ => format!(
                        "UPDATE _sqlx_migrations SET checksum = decode('00', 'hex') \
                         WHERE version = {BASELINE_VERSION}"
                    ),
                };
                let mut conn = admin(&url).await;
                conn.execute(sqlx::AssertSqlSafe(tamper)).await.unwrap();
                conn.close().await.unwrap();
                url
            }
        };
        let (code, report, stderr) = doctor_env(dir.path(), &planted(&fleet), &planted(&app));
        assert_eq!(code, 1, "{state}: {report:?}\n{stderr}");
        let schema = row(&report, "server.app.schema");
        assert_eq!(
            (schema.outcome, schema.reason.as_deref()),
            (Outcome::Failed, Some(expected)),
            "{state}"
        );
        assert!(schema.next_action.is_some(), "{state}: {schema:?}");
        assert_eq!(
            verdict(&report, "server.app.connect"),
            (Outcome::Complete, None),
            "{state}"
        );
        // The writer lock is observed whatever the ledger holds.
        assert_eq!(
            verdict(&report, "server.app.writer"),
            (Outcome::Complete, Some("not_observed")),
            "{state}"
        );
    }
}

/// Each connect check names the setting its URL came from and where
/// `SQLx` found the password, never a value.
#[tokio::test]
async fn doctor_names_database_sources_not_values() {
    ensure_role().await;
    let fleet = migrated_fleet().await;
    let app = migrated_app().await;
    let dir = tempfile::tempdir().unwrap();
    let mut values = url_values(&planted(&fleet));
    values.extend(url_values(&planted(&app)));
    let config_path = dir.path().join("trawld.toml");
    let bare = |url: &str| with_login(url, ROLE, None);
    let source = |report: &Report, id: &str| row(report, id).source.clone().unwrap_or_default();

    // In the file, the password from PGPASSWORD.
    let config = DoctorConfig {
        fleet_url: Some(bare(&fleet)),
        app_url: Some(bare(&app)),
        ..DoctorConfig::in_dir(dir.path())
    };
    let (_, report, _) = doctor(
        dir.path(),
        &config,
        &[("PGPASSWORD", SECRET.into())],
        &values,
    );
    assert_eq!(
        verdict(&report, "server.fleet.connect"),
        (Outcome::Complete, None)
    );
    assert_eq!(
        source(&report, "server.fleet.connect"),
        format!(
            "[auth] database_url in {}; password from PGPASSWORD",
            config_path.display()
        )
    );
    assert_eq!(
        source(&report, "server.app.connect"),
        format!(
            "[storage] database_url in {}; password from PGPASSWORD",
            config_path.display()
        )
    );

    // In the environment, the password from PGPASSFILE.
    let passfile = dir.path().join("pgpass");
    std::fs::write(&passfile, format!("*:*:*:{ROLE}:{SECRET}\n")).unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&passfile, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let (_, report, _) = doctor(
        dir.path(),
        &DoctorConfig::in_dir(dir.path()),
        &[
            ("FLEET_DATABASE_URL", bare(&fleet).into()),
            ("TRAWL_DATABASE_URL", bare(&app).into()),
            ("PGPASSFILE", passfile.clone().into_os_string()),
        ],
        &values,
    );
    assert_eq!(
        verdict(&report, "server.app.connect"),
        (Outcome::Complete, None)
    );
    assert_eq!(
        source(&report, "server.app.connect"),
        format!(
            "TRAWL_DATABASE_URL from the environment; password from PGPASSFILE {}",
            passfile.display()
        )
    );
}

/// Each refusal to connect has its own reason, and blocks the schema check
/// behind it; the other database is checked all the same.
#[tokio::test]
async fn doctor_connect_refusals_fail() {
    ensure_role().await;
    let fleet = migrated_fleet().await;
    let app = migrated_app().await;
    let dir = tempfile::tempdir().unwrap();
    let mut values = url_values(&planted(&fleet));
    values.extend(url_values(&planted(&app)));

    // A wrong password, no such database, nothing listening.
    let wrong = with_login(&fleet, ROLE, Some("private-secret-wrong"));
    let missing = {
        let url = planted(&app);
        let at = url.rfind('/').unwrap();
        format!("{}/trawl_doctor_no_such_database", &url[..at])
    };
    for (fleet_url, expected) in [
        (wrong.as_str(), "authentication failed"),
        (missing.as_str(), "the database does not exist"),
        (
            crate::support::PLANTED_FLEET_URL,
            "the database server is unreachable",
        ),
    ] {
        let mut planted_values = values.clone();
        planted_values.push(fleet_url.to_owned());
        planted_values.push("trawl_doctor_no_such_database".to_owned());
        let (code, report, _) = doctor(
            dir.path(),
            &DoctorConfig::in_dir(dir.path()),
            &[
                ("FLEET_DATABASE_URL", fleet_url.into()),
                ("TRAWL_DATABASE_URL", planted(&app).into()),
            ],
            &planted_values,
        );
        assert_eq!(code, 1, "{expected}: {report:?}");
        assert_eq!(
            verdict(&report, "server.fleet.connect"),
            (Outcome::Failed, Some(expected))
        );
        let schema = row(&report, "server.fleet.schema");
        assert_eq!(
            (schema.outcome, schema.blocked_by.as_deref()),
            (Outcome::NotSampled, Some("server.fleet.connect"))
        );
        // The other database is unaffected.
        assert_eq!(
            verdict(&report, "server.app.schema"),
            (Outcome::Complete, None)
        );
    }
}
