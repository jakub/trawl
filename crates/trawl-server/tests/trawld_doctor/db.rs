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
use trawl_api::doctor::{Outcome, Report};

use crate::common;
use crate::support::{
    ADVISORY_FUNCTIONS, DoctorConfig, LOCKLESS, ROLE, SECRET, admin, assert_fresh_install_rows,
    assert_no_values, database_rows_complete, ensure_role, forbid_advisory_locks, lockless,
    migrated_app, migrated_fleet, planted, report, row, run_doctor, url_values, verdict,
    with_login, write_doctor_config,
};

/// The pre-1.0 app-state migrations boot refuses to adopt.
const LEGACY_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../scripts/schema-baseline/fixtures/trawl"
);

/// `url` with `query` added to its query string.
fn with_query(url: &str, query: &str) -> String {
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}{query}")
}

/// The `options` parameter that puts the schema [`PROBE_SCHEMA`] first on
/// the doctor's `search_path`, so the doctor's unqualified calls to
/// `current_database()` reach the probe's function of that name.
const PROBE_OPTIONS: &str = "options=-c%20search_path%3Ddoctor_probe%2Cpg_catalog%2Cpublic";

/// The schema holding the probe function [`install_probe`] writes.
const PROBE_SCHEMA: &str = "doctor_probe";

/// In the database `url` names, (re)define `doctor_probe.current_database()`
/// to run `body`, a PL/pgSQL statement list, and then return the real
/// `current_database()`. Every role may call it. With [`PROBE_OPTIONS`]
/// on the doctor's URL, the doctor's own lock queries call it, so `body`
/// runs inside the doctor's session.
async fn install_probe(url: &str, body: &str) {
    let mut conn = admin(url).await;
    conn.execute(sqlx::AssertSqlSafe(format!(
        "CREATE SCHEMA IF NOT EXISTS {PROBE_SCHEMA};
         GRANT USAGE ON SCHEMA {PROBE_SCHEMA} TO PUBLIC;
         CREATE OR REPLACE FUNCTION {PROBE_SCHEMA}.current_database() RETURNS name
         LANGUAGE plpgsql AS $$ BEGIN {body} RETURN pg_catalog.current_database(); END $$;"
    )))
    .await
    .expect("install the probe function");
    conn.close().await.unwrap();
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

/// The fresh installation of `doctor_fresh_install_is_incomplete` with an
/// empty Fleet database: the run fails on `server.fleet.schema`, whose next
/// action is `fleet-admin migrate`, and every row that says what boot
/// initializes says so as it does there (#269 AC3).
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
    assert_fresh_install_rows(&report);
    let failed: Vec<&str> = report
        .checks()
        .iter()
        .filter(|check| check.outcome == Outcome::Failed)
        .map(|check| check.id.as_str())
        .collect();
    assert_eq!(failed, ["server.fleet.schema"], "{report:#?}");

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

/// The part of `doctor_takes_no_advisory_lock` over a fresh and a behind
/// app-state ledger, with the Fleet database at `fleet_url` and the runs in
/// `dir`. There the migrator's lock samples decide the row, so a sample the
/// database refused would make `server.app.schema` `not_sampled`.
async fn lockless_over_ledgers_that_are_not_current(fleet_url: &str, dir: &Path) {
    use trawl_server::store::migrations::{BASELINE_VERSION, MIGRATOR};

    let fresh = common::create_app_database().await;
    let behind = common::create_app_database().await;
    let mut conn = admin(&behind).await;
    MIGRATOR
        .run_to(BASELINE_VERSION, &mut conn)
        .await
        .expect("apply the baseline");
    conn.close().await.unwrap();
    for (label, app) in [("fresh", fresh), ("behind", behind)] {
        forbid_advisory_locks(&app).await;
        let app = lockless(&app);
        let fleet = lockless(fleet_url);
        let home = dir.to_owned();
        let (_, report, stderr) =
            tokio::task::spawn_blocking(move || doctor_env(&home, &fleet, &app))
                .await
                .unwrap();
        database_rows_complete(&report)
            .unwrap_or_else(|why| panic!("{label}: {why}: {report:?}\n{stderr}"));
        assert_eq!(
            verdict(&report, "server.app.schema"),
            (Outcome::Complete, Some("will_initialize")),
            "{label}: {report:?}"
        );
    }
}

/// While a real trawld holds its writer lock, the doctor runs as
/// [`LOCKLESS`] on databases where that role may execute no advisory-lock
/// function, and every database row completes: had the doctor called one,
/// the call would have been refused and its row would not be `complete`
/// (`an_advisory_lock_call_fails_the_lockless_proof` is the negative
/// control). As supporting evidence, a watcher polling `pg_locks` back to
/// back sees the doctor's backends on both databases and no advisory lock
/// held or awaited by either. Afterwards the same trawld session still
/// holds the writer lock and trawld still serves.
///
/// A current ledger is `complete` whatever the migrator's lock samples
/// show, so a refused call among those samples would change no row there.
/// The same run over a fresh and a behind app-state ledger closes that gap:
/// each is `complete`, `will_initialize`, only when both samples answered.
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
    forbid_advisory_locks(&server.fleet_db_url).await;
    forbid_advisory_locks(&server.app_db_url).await;

    let stop = Arc::new(AtomicBool::new(false));
    let sampler = tokio::spawn(watch_doctor_locks(
        fleet_db.clone(),
        app_db.clone(),
        Arc::clone(&stop),
    ));
    let dir = tempfile::tempdir().unwrap();
    let fleet = lockless(&server.fleet_db_url);
    let app = lockless(&server.app_db_url);
    let home = dir.path().to_owned();
    let (_, report, stderr) = tokio::task::spawn_blocking(move || doctor_env(&home, &fleet, &app))
        .await
        .unwrap();
    stop.store(true, Ordering::SeqCst);
    let watched = sampler.await.unwrap();

    database_rows_complete(&report).unwrap_or_else(|why| panic!("{why}: {report:?}\n{stderr}"));

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
    let other = migrated_app().await;
    forbid_advisory_locks(&other).await;
    let other = lockless(&other);
    let fleet = lockless(&server.fleet_db_url);
    let home = dir.path().to_owned();
    let (_, report, _) = tokio::task::spawn_blocking(move || doctor_env(&home, &fleet, &other))
        .await
        .unwrap();
    database_rows_complete(&report).unwrap_or_else(|why| panic!("{why}: {report:?}"));
    assert_eq!(
        verdict(&report, "server.app.writer"),
        (Outcome::Complete, Some("not_observed"))
    );
    assert_eq!(writer_pid(&mut admin_conn, &app_db).await, Some(before));
    admin_conn.close().await.unwrap();

    lockless_over_ledgers_that_are_not_current(&server.fleet_db_url, dir.path()).await;
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

/// The negative control of `doctor_takes_no_advisory_lock`. On databases
/// where [`forbid_advisory_locks`] ran, [`LOCKLESS`] calling any one of the
/// advisory-lock functions is refused with `insufficient_privilege`
/// (SQLSTATE 42501), so a doctor run as that role cannot take, await or
/// release an advisory lock without the call erring. The doctor run as
/// that role completes every database row: had it made such a call, the
/// error would have turned the row that made it `not_sampled`.
#[tokio::test]
async fn an_advisory_lock_call_fails_the_lockless_proof() {
    ensure_role().await;
    let fleet = migrated_fleet().await;
    let app = migrated_app().await;
    forbid_advisory_locks(&fleet).await;
    forbid_advisory_locks(&app).await;

    for url in [&fleet, &app] {
        let mut conn = PgConnection::connect(&lockless(url))
            .await
            .expect("the lockless role connects");
        let functions: Vec<(String, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT proname::text, pg_get_function_identity_arguments(oid)
             FROM pg_proc WHERE oid IN ({ADVISORY_FUNCTIONS}) ORDER BY 1, 2"
        )))
        .fetch_all(&mut conn)
        .await
        .expect("list the advisory-lock functions");
        assert!(
            functions.len() >= 21,
            "only {} advisory-lock functions found",
            functions.len()
        );
        for (name, arguments) in functions {
            let arguments = arguments
                .split(", ")
                .filter(|argument| !argument.is_empty())
                .map(|argument| format!("1::{argument}"))
                .collect::<Vec<_>>()
                .join(", ");
            let call = format!("SELECT pg_catalog.{name}({arguments})");
            let refused = conn
                .execute(sqlx::AssertSqlSafe(call.clone()))
                .await
                .expect_err(&format!("{LOCKLESS} ran {call}"));
            let code = refused
                .as_database_error()
                .and_then(|error| error.code().map(std::borrow::Cow::into_owned));
            assert_eq!(code.as_deref(), Some("42501"), "{call}: {refused}");
        }
        conn.close().await.unwrap();
    }

    let dir = tempfile::tempdir().unwrap();
    let (_, report, stderr) = doctor_env(dir.path(), &lockless(&fleet), &lockless(&app));
    database_rows_complete(&report).unwrap_or_else(|why| panic!("{why}: {report:?}\n{stderr}"));
}

/// A query that errs or a connection that breaks after the login proves
/// nothing about boot: each row is `not_sampled` with a closed reason, and
/// the error's text, which here carries the planted secret, is not shown.
#[tokio::test]
async fn doctor_query_failures_after_login_are_not_sampled() {
    ensure_role().await;
    let fleet = migrated_fleet().await;
    let app = migrated_app().await;
    let dir = tempfile::tempdir().unwrap();
    let probed = with_query(&planted(&app), PROBE_OPTIONS);

    // Every lock query errs. The current ledger is complete whatever its
    // lock samples show; the writer row has only the failed read. The
    // writer query calls current_database() only for a pg_locks row that
    // matches its other conditions, so a session of the test holds
    // trawld's writer lock, as `writer_pid` spells its key, meanwhile.
    install_probe(
        &app,
        &format!("RAISE EXCEPTION USING MESSAGE = '{SECRET} probe', ERRCODE = 'XX000';"),
    )
    .await;
    let mut holder = admin(&app).await;
    sqlx::query("SELECT pg_advisory_lock(0x0074_7261_776c_2131)")
        .execute(&mut holder)
        .await
        .expect("hold trawld's writer lock");
    let (_, report, _) = doctor_env(dir.path(), &planted(&fleet), &probed);
    holder.close().await.unwrap();
    assert_eq!(
        verdict(&report, "server.app.writer"),
        (Outcome::NotSampled, Some("query_failed")),
        "{report:?}"
    );
    assert_eq!(
        verdict(&report, "server.app.schema"),
        (Outcome::Complete, None)
    );

    // The first lock query ends the doctor's own backend: the server
    // reports the termination, and every read after it finds the
    // connection gone.
    install_probe(
        &app,
        "PERFORM pg_catalog.pg_terminate_backend(pg_catalog.pg_backend_pid());",
    )
    .await;
    let (_, report, _) = doctor_env(dir.path(), &planted(&fleet), &probed);
    for id in ["server.app.schema", "server.app.writer"] {
        assert_eq!(
            verdict(&report, id),
            (Outcome::NotSampled, Some("connection_lost")),
            "{id}: {report:?}"
        );
    }
}

/// One case of `doctor_refuses_unbounded_credential_files`.
struct FileCase {
    label: &'static str,
    /// Added to the doctor's environment.
    env: Vec<(&'static str, OsString)>,
    /// The app-state URL, which carries no password when a password file
    /// is to be read.
    app_url: String,
    reason: &'static str,
    detail: String,
    /// The setting holds for every connection the process makes, so the
    /// Fleet connection is refused the same way.
    process_wide: bool,
}

/// The cases of `doctor_refuses_unbounded_credential_files`, with their
/// FIFOs and oversized file made in `dir`, over the planted app-state URL
/// `app` and the same URL without a password, `bare_app`. A TLS path comes
/// from the URL or the environment, so it is never shown.
fn credential_file_cases(dir: &Path, app: &str, bare_app: &str) -> Vec<FileCase> {
    use nix::sys::stat::Mode;

    let fifo = dir.join("fifo");
    nix::unistd::mkfifo(&fifo, Mode::S_IRWXU).unwrap();
    nix::unistd::mkfifo(&dir.join("tls-fifo"), Mode::S_IRWXU).unwrap();
    let big = dir.join("big");
    std::fs::File::create(&big)
        .unwrap()
        .set_len(1024 * 1024 + 1)
        .unwrap();
    let home_fifo = dir.join("home");
    std::fs::create_dir(&home_fifo).unwrap();
    nix::unistd::mkfifo(&home_fifo.join(".pgpass"), Mode::S_IRWXU).unwrap();
    let not_regular =
        |named: &str| format!("{named} is not a regular file; the doctor did not connect");
    vec![
        FileCase {
            label: "PGPASSFILE is a FIFO",
            env: vec![("PGPASSFILE", fifo.clone().into_os_string())],
            app_url: bare_app.to_owned(),
            reason: "unreadable",
            detail: not_regular(&format!("PGPASSFILE {}", fifo.display())),
            process_wide: false,
        },
        FileCase {
            label: "PGPASSFILE is too large",
            env: vec![("PGPASSFILE", big.clone().into_os_string())],
            app_url: bare_app.to_owned(),
            reason: "too_large",
            detail: format!(
                "PGPASSFILE {} is larger than 1048576 bytes; the doctor did not connect",
                big.display()
            ),
            process_wide: false,
        },
        FileCase {
            label: "~/.pgpass is a FIFO",
            env: vec![("HOME", home_fifo.into_os_string())],
            app_url: bare_app.to_owned(),
            reason: "unreadable",
            detail: not_regular("~/.pgpass"),
            process_wide: false,
        },
        FileCase {
            label: "the URL's sslrootcert is a FIFO",
            env: Vec::new(),
            app_url: with_query(
                app,
                &format!("sslrootcert={}", dir.join("tls-fifo").display()),
            ),
            reason: "unreadable",
            detail: not_regular("the file sslrootcert names"),
            process_wide: false,
        },
        FileCase {
            label: "PGSSLKEY is a device",
            env: vec![("PGSSLKEY", "/dev/zero".into())],
            app_url: app.to_owned(),
            reason: "unreadable",
            detail: not_regular("the file sslkey names"),
            // SQLx reads PGSSLKEY for every connection.
            process_wide: true,
        },
    ]
}

/// A password file `SQLx` would read, or a TLS file the doctor reads for
/// it, that is not a regular file or is larger than the cap is refused
/// before the doctor connects: `not_sampled`, promptly, with the file
/// named by its setting. Only `PGPASSFILE`'s path is shown. A file named
/// in the environment for every connection refuses both databases.
#[tokio::test]
async fn doctor_refuses_unbounded_credential_files() {
    ensure_role().await;
    let fleet = migrated_fleet().await;
    let app = migrated_app().await;
    let mut values = url_values(&planted(&fleet));
    values.extend(url_values(&planted(&app)));

    let dir = tempfile::tempdir().unwrap();
    let bare_app = with_login(&app, ROLE, None);
    for case in credential_file_cases(dir.path(), &planted(&app), &bare_app) {
        let label = case.label;
        let mut env = case.env;
        env.push(("FLEET_DATABASE_URL", planted(&fleet).into()));
        env.push(("TRAWL_DATABASE_URL", case.app_url.clone().into()));
        let mut planted_values = values.clone();
        planted_values.push(case.app_url);
        planted_values.push(dir.path().join("tls-fifo").display().to_string());
        let started = std::time::Instant::now();
        let (_, report, _) = doctor(
            dir.path(),
            &DoctorConfig::in_dir(dir.path()),
            &env,
            &planted_values,
        );
        // Both output forms ran; neither waited out a 5 s deadline.
        assert!(
            started.elapsed() < std::time::Duration::from_secs(8),
            "{label}: {:?}",
            started.elapsed()
        );
        let refused = |connect: &str, schema: &str| {
            let check = row(&report, connect);
            assert_eq!(
                (
                    check.outcome,
                    check.reason.as_deref(),
                    check.detail.as_deref()
                ),
                (
                    Outcome::NotSampled,
                    Some(case.reason),
                    Some(case.detail.as_str())
                ),
                "{label}: {connect}"
            );
            assert_eq!(
                row(&report, schema).blocked_by.as_deref(),
                Some(connect),
                "{label}: {schema}"
            );
        };
        refused("server.app.connect", "server.app.schema");
        if case.process_wide {
            refused("server.fleet.connect", "server.fleet.schema");
        } else {
            // The Fleet URL carries its password, so no password file is
            // read for it, and it names no TLS file.
            assert_eq!(
                verdict(&report, "server.fleet.connect"),
                (Outcome::Complete, None),
                "{label}"
            );
        }
    }
}

/// An IPv6 `hostaddr` connects. `SQLx` 0.9.0 panics rendering such options
/// back into a URL, and the default panic hook prints the panic's message
/// and location, so the doctor does neither: the rows complete and stderr
/// stays empty.
#[tokio::test]
async fn doctor_takes_an_ipv6_hostaddr() {
    ensure_role().await;
    let fleet = migrated_fleet().await;
    let app = migrated_app().await;
    let dir = tempfile::tempdir().unwrap();
    let v6 = |url: &str| with_query(&planted(url), "hostaddr=::1");

    let (_, report, stderr) = doctor_env(dir.path(), &v6(&fleet), &v6(&app));
    for id in ["server.fleet.connect", "server.app.connect"] {
        let check = row(&report, id);
        assert_eq!(
            (check.outcome, check.source.as_deref()),
            (
                Outcome::Complete,
                Some(if id == "server.fleet.connect" {
                    "FLEET_DATABASE_URL from the environment; password in the URL"
                } else {
                    "TRAWL_DATABASE_URL from the environment; password in the URL"
                })
            ),
            "{id}: {report:?}"
        );
    }
    database_rows_complete(&report).unwrap_or_else(|why| panic!("{why}: {report:?}"));
    assert!(stderr.is_empty(), "{stderr}");
}

/// A session that does not start read-only with the doctor's timeouts is
/// `not_sampled`, `session_not_read_only`, and the checks behind it are
/// blocked: no query of theirs ran. A NUL in `options` would cut the
/// doctor's own settings out of the startup message, so the doctor does
/// not connect with one; a session the server changed after the login,
/// here by a login event trigger, is caught when the doctor reads its
/// settings back.
#[tokio::test]
async fn doctor_refuses_a_session_that_is_not_read_only() {
    ensure_role().await;
    let fleet = migrated_fleet().await;
    let app = migrated_app().await;
    let dir = tempfile::tempdir().unwrap();
    let refused = |report: &Report, label: &str| {
        assert_eq!(
            verdict(report, "server.app.connect"),
            (Outcome::NotSampled, Some("session_not_read_only")),
            "{label}: {report:?}"
        );
        for id in ["server.app.schema", "server.app.writer"] {
            assert_eq!(
                row(report, id).blocked_by.as_deref(),
                Some("server.app.connect"),
                "{label}: {id}"
            );
        }
        assert_eq!(
            verdict(report, "server.fleet.schema"),
            (Outcome::Complete, None),
            "{label}"
        );
    };

    for options in ["options=%00", "options=-c%20work_mem%3D64kB%00"] {
        let url = with_query(&planted(&app), options);
        let (_, report, _) = doctor_env(dir.path(), &planted(&fleet), &url);
        refused(&report, options);
    }

    // A login trigger that undoes one of the doctor's settings for the
    // doctor's sessions only.
    let mut conn = admin(&app).await;
    conn.execute(sqlx::AssertSqlSafe(format!(
        "CREATE SCHEMA {PROBE_SCHEMA};
         CREATE FUNCTION {PROBE_SCHEMA}.undo() RETURNS event_trigger
         LANGUAGE plpgsql AS $$ BEGIN END $$;
         CREATE EVENT TRIGGER doctor_undo ON login EXECUTE FUNCTION {PROBE_SCHEMA}.undo();"
    )))
    .await
    .expect("install the login trigger");
    for (setting, value) in [
        ("default_transaction_read_only", "off"),
        ("statement_timeout", "0"),
        ("lock_timeout", "0"),
    ] {
        conn.execute(sqlx::AssertSqlSafe(format!(
            "CREATE OR REPLACE FUNCTION {PROBE_SCHEMA}.undo() RETURNS event_trigger
             LANGUAGE plpgsql AS $$ BEGIN
                 IF current_setting('application_name') = 'trawld-doctor' THEN
                     PERFORM set_config('{setting}', '{value}', false);
                 END IF;
             END $$"
        )))
        .await
        .expect("set the login trigger's body");
        let (_, report, _) = doctor_env(dir.path(), &planted(&fleet), &planted(&app));
        refused(&report, setting);
    }

    // With the trigger doing nothing again, the same URL is read-only.
    conn.execute(sqlx::AssertSqlSafe(format!(
        "CREATE OR REPLACE FUNCTION {PROBE_SCHEMA}.undo() RETURNS event_trigger
         LANGUAGE plpgsql AS $$ BEGIN END $$"
    )))
    .await
    .unwrap();
    conn.close().await.unwrap();
    let (_, report, _) = doctor_env(dir.path(), &planted(&fleet), &planted(&app));
    database_rows_complete(&report).unwrap_or_else(|why| panic!("{why}: {report:?}"));
}

/// A database server that speaks TLS for [`doctor_hands_sqlx_the_tls_file_bytes`].
///
/// For each connection it reads the `SSLRequest`, moves `ca` aside, and
/// only then answers `S`. It completes the TLS handshake with `config`,
/// puts `ca` back, reads the startup message, and refuses it with SQLSTATE
/// 28P01. A driver that opened `ca` during the handshake finds nothing at
/// the path; one that holds its bytes already connects.
fn tls_database_server(config: rustls::ServerConfig, ca: PathBuf) -> std::net::SocketAddr {
    use std::io::{Read as _, Write as _};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let config = std::sync::Arc::new(config);
    let aside = ca.with_extension("aside");
    std::thread::spawn(move || {
        for tcp in listener.incoming() {
            let Ok(mut tcp) = tcp else { return };
            let mut request = [0_u8; 8];
            if tcp.read_exact(&mut request).is_err()
                || request != [0, 0, 0, 8, 0x04, 0xd2, 0x16, 0x2f]
            {
                continue;
            }
            std::fs::rename(&ca, &aside).unwrap();
            let _ = tcp.write_all(b"S");
            let mut conn = rustls::ServerConnection::new(std::sync::Arc::clone(&config)).unwrap();
            let mut handshake = Ok(());
            while handshake.is_ok() && conn.is_handshaking() {
                handshake = conn.complete_io(&mut tcp).map(drop);
            }
            std::fs::rename(&aside, &ca).unwrap();
            if handshake.is_err() {
                continue;
            }
            let mut tls = rustls::StreamOwned::new(conn, tcp);
            let mut length = [0_u8; 4];
            if tls.read_exact(&mut length).is_err() {
                continue;
            }
            let mut startup = vec![0_u8; (u32::from_be_bytes(length) as usize).saturating_sub(4)];
            if tls.read_exact(&mut startup).is_err() {
                continue;
            }
            let fields = b"SFATAL\0VFATAL\0C28P01\0Mrefused\0\0";
            let mut refusal = vec![b'E'];
            refusal.extend_from_slice(&u32::try_from(fields.len() + 4).unwrap().to_be_bytes());
            refusal.extend_from_slice(fields);
            let _ = tls.write_all(&refusal);
            let _ = tls.flush();
            tls.conn.send_close_notify();
            let _ = tls.conn.complete_io(&mut tls.sock);
        }
    });
    addr
}

/// The doctor reads each database TLS file once and hands `SQLx` the
/// bytes, so `SQLx` never opens the file (S2). The app-state URL names a
/// valid CA in `sslrootcert` with `sslmode=verify-ca`, and the server moves
/// the CA aside before it starts the handshake: the handshake still
/// verifies, and the server's refusal of the login is the row,
/// `failed`/`authentication failed`, where a driver reading the path would
/// have failed TLS. A FIFO at the same setting is refused without
/// hanging (`doctor_refuses_unbounded_credential_files`).
#[tokio::test(flavor = "multi_thread")]
async fn doctor_hands_sqlx_the_tls_file_bytes() {
    ensure_role().await;
    let fleet = migrated_fleet().await;
    let dir = tempfile::tempdir().unwrap();

    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
    let ca = dir.path().join("ca.pem");
    std::fs::write(&ca, cert.pem()).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![cert.der().clone()],
        rustls::pki_types::PrivateKeyDer::Pkcs8(signing_key.serialize_der().into()),
    )
    .unwrap();
    let addr = tls_database_server(config, ca.clone());

    let app_url = format!(
        "postgres://trawl@{addr}/trawl?sslmode=verify-ca&sslrootcert={}",
        ca.display()
    );
    let mut values = url_values(&planted(&fleet));
    values.extend([app_url.clone(), ca.display().to_string()]);
    let env = [
        ("FLEET_DATABASE_URL", OsString::from(planted(&fleet))),
        ("TRAWL_DATABASE_URL", OsString::from(app_url)),
    ];
    let (_, report, _) = tokio::task::spawn_blocking({
        let dir = dir.path().to_owned();
        move || doctor(&dir, &DoctorConfig::in_dir(&dir), &env, &values)
    })
    .await
    .unwrap();
    assert_eq!(
        verdict(&report, "server.app.connect"),
        (Outcome::Failed, Some("authentication failed")),
        "{report:#?}"
    );
    assert!(ca.exists(), "the server put the CA back");
}

/// The nine bytes a hostile or broken database server sends for
/// `AuthenticationMD5Password`: type `R`, length 8, method 5, and no salt.
/// `SQLx` 0.9.0 decodes the method, then copies a four-byte salt the
/// message does not hold, and panics.
const MD5_WITHOUT_SALT: [u8; 9] = [0x52, 0, 0, 0, 8, 0, 0, 0, 5];

/// The line the doctor's panic hook prints, and nothing else of a panic.
const PANIC_LINE: &str = "[trawld] doctor stopped a check: an internal error occurred";

/// A database server for [`doctor_contains_a_driver_panic`]. On each
/// connection it reads one length-prefixed packet; an `SSLRequest` gets
/// `N`, as a server without TLS answers, and the startup message after it
/// is read the same way. The startup message gets [`MD5_WITHOUT_SALT`], and
/// the connection is then held until the client drops it. Each startup
/// message, without its length, is recorded as it arrives.
fn malformed_auth_server() -> (std::net::SocketAddr, Received) {
    use std::io::{Read as _, Write as _};

    fn packet(tcp: &mut std::net::TcpStream) -> Option<Vec<u8>> {
        let mut length = [0_u8; 4];
        tcp.read_exact(&mut length).ok()?;
        let mut body = vec![0_u8; (u32::from_be_bytes(length) as usize).checked_sub(4)?];
        tcp.read_exact(&mut body).ok()?;
        Some(body)
    }

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let startups = Received::default();
    let recorded = startups.clone();
    std::thread::spawn(move || {
        for tcp in listener.incoming() {
            let Ok(mut tcp) = tcp else { return };
            let recorded = recorded.clone();
            std::thread::spawn(move || {
                tcp.set_read_timeout(Some(std::time::Duration::from_secs(60)))
                    .unwrap();
                let Some(mut first) = packet(&mut tcp) else {
                    return;
                };
                if first == [0x04, 0xd2, 0x16, 0x2f] {
                    let _ = tcp.write_all(b"N");
                    let Some(startup) = packet(&mut tcp) else {
                        return;
                    };
                    first = startup;
                }
                // A startup message: protocol 3.0, then its fields.
                assert_eq!(first[..4], [0, 3, 0, 0], "a startup message");
                recorded.lock().unwrap().push(first);
                let _ = tcp.write_all(&MD5_WITHOUT_SALT);
                let _ = tcp.flush();
                let mut rest = Vec::new();
                let _ = tcp.read_to_end(&mut rest);
            });
        }
    });
    (addr, startups)
}

/// A database server that sends a message `SQLx` panics decoding
/// ([`MD5_WITHOUT_SALT`]) costs only that database's rows: each connect
/// check is `not_sampled`, `protocol_error`, the checks behind it are
/// blocked, and the storage and listener checks still run, so the report
/// renders. The Fleet URL disables TLS; the app-state URL keeps the default
/// `sslmode`, `prefer`, whose `SSLRequest` the server declines, so both
/// reach the decoder. The panic hook's fixed line, once per database, is
/// all stderr holds. Under `disable` the doctor's own probe reads the same
/// server first, and its startup message is byte for byte the one `SQLx`
/// sends after it.
#[tokio::test(flavor = "multi_thread")]
async fn doctor_contains_a_driver_panic() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, startups) = malformed_auth_server();
    let fleet = format!("postgres://{ROLE}:{SECRET}@{addr}/planted_fleet_db?sslmode=disable");
    let app = format!("postgres://{ROLE}:{SECRET}@{addr}/planted_app_db");
    let mut values = url_values(&fleet);
    values.extend(url_values(&app));
    let env = [
        ("FLEET_DATABASE_URL", OsString::from(&fleet)),
        ("TRAWL_DATABASE_URL", OsString::from(&app)),
    ];
    let (code, report, stderr) = tokio::task::spawn_blocking({
        let dir = dir.path().to_owned();
        move || doctor(&dir, &DoctorConfig::in_dir(&dir), &env, &values)
    })
    .await
    .unwrap();

    for (connect, behind) in [
        ("server.fleet.connect", &["server.fleet.schema"][..]),
        (
            "server.app.connect",
            &["server.app.schema", "server.app.writer"][..],
        ),
    ] {
        assert_eq!(
            verdict(&report, connect),
            (Outcome::NotSampled, Some("protocol_error")),
            "{connect}: {report:#?}"
        );
        for id in behind {
            assert_eq!(
                row(&report, id).blocked_by.as_deref(),
                Some(connect),
                "{id}: {report:#?}"
            );
        }
    }
    // The groups after the databases ran.
    for id in [
        "server.data.root",
        "server.data.epoch",
        "server.tls.material",
    ] {
        assert_eq!(
            verdict(&report, id),
            (Outcome::Complete, Some("will_initialize")),
            "{id}: {report:#?}"
        );
    }
    assert_eq!(
        verdict(&report, "server.listener.identity"),
        (Outcome::NotSampled, Some("not_listening")),
        "{report:#?}"
    );
    assert_eq!(code, 3, "{report:#?}\n{stderr}");
    assert_eq!(
        stderr.lines().collect::<Vec<_>>(),
        [PANIC_LINE, PANIC_LINE],
        "{stderr}"
    );
    let startups = startups.lock().unwrap().clone();
    let fleet_startups: Vec<&Vec<u8>> = startups
        .iter()
        .filter(|body| {
            body.windows(b"planted_fleet_db".len())
                .any(|window| window == b"planted_fleet_db")
        })
        .collect();
    // Two runs, JSON and table, each with the Fleet probe, then SQLx for
    // each database.
    assert_eq!(startups.len(), 6, "{startups:?}");
    assert_eq!(fleet_startups.len(), 4, "{startups:?}");
    assert!(
        fleet_startups.iter().all(|body| *body == fleet_startups[0]),
        "the probe sends SQLx's startup message: {startups:?}"
    );
}

/// The `SSLRequest` message, as `SQLx` sends it.
const SSL_REQUEST: [u8; 8] = [0, 0, 0, 8, 0x04, 0xd2, 0x16, 0x2f];

/// What an HTTP server answers bytes it cannot parse as a request. `SQLx`
/// reading it as a PostgreSQL message would take `H` as the type and
/// `TTP/` as a length of about 1.4 GB.
const HTTP_400: &[u8] = b"HTTP/1.1 400 Bad Request\r\n\r\n";

/// What an SSH server sends as soon as a connection opens. `SQLx` 0.9.0
/// reads `S` as a `ParameterStatus` type and `SH-2` as a length of about
/// 1.4 GB, and `S` is also a PostgreSQL server's answer to an `SSLRequest`.
const SSH_BANNER: &[u8] = b"SSH-2.0-OpenSSH_9.9\r\n";

/// An authentication request whose length claims 1 GiB: a type byte a
/// PostgreSQL server does answer a startup message with, and a length none
/// sends.
const HUGE_AUTHENTICATION: &[u8] = &[b'R', 0x40, 0, 0, 0, 0, 0, 0, 10];

/// How a [`fake_server`] speaks: `greeting` goes out as soon as a
/// connection opens, and `answer` once the client's first bytes arrive.
#[derive(Clone, Copy)]
struct Speech {
    greeting: &'static [u8],
    answer: &'static [u8],
}

/// An HTTP server a database URL might name by accident.
const HTTP: Speech = Speech {
    greeting: b"",
    answer: HTTP_400,
};

/// An SSH server, which speaks first.
const SSH: Speech = Speech {
    greeting: SSH_BANNER,
    answer: b"",
};

/// A server whose first answer has a PostgreSQL type and a huge length.
const HUGE_R: Speech = Speech {
    greeting: b"",
    answer: HUGE_AUTHENTICATION,
};

/// Every byte each connection to a [`fake_server`] sent, recorded when the
/// client closes it.
type Received = std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>;

/// Serve one connection as `speech` says, hold it open, and record what
/// the client sent until it closes.
fn fake_connection<S>(mut stream: S, speech: Speech, received: &Received)
where
    S: std::io::Read + std::io::Write + Send + 'static,
{
    let received = received.clone();
    std::thread::spawn(move || {
        if !speech.greeting.is_empty() {
            let _ = stream.write_all(speech.greeting);
        }
        let mut seen = Vec::new();
        let mut buf = [0_u8; 4096];
        loop {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if seen.is_empty() && !speech.answer.is_empty() {
                        let _ = stream.write_all(speech.answer);
                    }
                    seen.extend_from_slice(&buf[..n]);
                }
            }
        }
        received.lock().unwrap().push(seen);
    });
}

/// Where a [`fake_server`] listens.
enum FakeAt {
    Tcp(std::net::SocketAddr),
    /// The socket directory; the socket is `.s.PGSQL.5432` in it.
    Unix(PathBuf),
}

/// A server that is not PostgreSQL, speaking as `speech` says, on loopback
/// TCP, or on a Unix socket in `dir` when `unix`, and what it received.
fn fake_server(dir: &Path, unix: bool, speech: Speech) -> (FakeAt, Received) {
    let received = Received::default();
    let at = if unix {
        let sockets = dir.join("sock");
        std::fs::create_dir(&sockets).unwrap();
        let listener =
            std::os::unix::net::UnixListener::bind(sockets.join(".s.PGSQL.5432")).unwrap();
        let received = received.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(60)))
                    .unwrap();
                fake_connection(stream, speech, &received);
            }
        });
        FakeAt::Unix(sockets)
    } else {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let received = received.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(60)))
                    .unwrap();
                fake_connection(stream, speech, &received);
            }
        });
        FakeAt::Tcp(addr)
    };
    (at, received)
}

/// What `received` holds once `count` connections have closed, waiting up
/// to 10 s for the last of them, then 200 ms more for any connection past
/// `count` to show.
fn closed_connections(received: &Received, count: usize) -> Vec<Vec<u8>> {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while received.lock().unwrap().len() < count && std::time::Instant::now() < until {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    std::thread::sleep(std::time::Duration::from_millis(200));
    received.lock().unwrap().clone()
}

/// The name and value pairs of a v3.0 startup message that makes up all of
/// `sent`, after asserting its framing: a length that counts every byte,
/// protocol 3.0, NUL-terminated fields, and a closing NUL.
fn startup_fields(sent: &[u8]) -> Vec<(String, String)> {
    let length = u32::from_be_bytes(sent[..4].try_into().unwrap()) as usize;
    assert_eq!(length, sent.len(), "one message and nothing else: {sent:?}");
    assert_eq!(sent[4..8], [0, 3, 0, 0], "protocol 3.0: {sent:?}");
    let fields = sent[8..]
        .strip_suffix(b"\0\0")
        .unwrap_or_else(|| panic!("a closing NUL: {sent:?}"));
    let strings: Vec<String> = fields
        .split(|&byte| byte == 0)
        .map(|field| String::from_utf8(field.to_vec()).unwrap())
        .collect();
    assert_eq!(
        strings.len() % 2,
        0,
        "names and values in pairs: {strings:?}"
    );
    strings
        .chunks(2)
        .map(|pair| (pair[0].clone(), pair[1].clone()))
        .collect()
}

/// Assert `seen` is one connection per database the doctor probed, each
/// carrying the doctor's startup message and nothing else: the planted
/// role, the database, `application_name` `trawld-doctor`, and no byte of
/// the planted secret.
fn assert_one_startup_per_database(seen: &[Vec<u8>], databases: &[&str], label: &str) {
    assert_eq!(
        seen.len(),
        databases.len(),
        "{label}: one connection per database, and no second: {seen:?}"
    );
    let mut probed: Vec<String> = seen
        .iter()
        .map(|sent| {
            assert!(
                !sent
                    .windows(SECRET.len())
                    .any(|window| window == SECRET.as_bytes()),
                "{label}: the password reached the server: {sent:?}"
            );
            let fields = startup_fields(sent);
            let field = |name: &str| {
                fields
                    .iter()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.as_str())
            };
            assert_eq!(field("user"), Some(ROLE), "{label}: {fields:?}");
            assert_eq!(
                field("application_name"),
                Some("trawld-doctor"),
                "{label}: {fields:?}"
            );
            assert_eq!(field("password"), None, "{label}: {fields:?}");
            field("database").unwrap().to_owned()
        })
        .collect();
    probed.sort();
    let mut expected: Vec<String> = databases.iter().map(|db| (*db).to_owned()).collect();
    expected.sort();
    assert_eq!(probed, expected, "{label}");
}

/// Time one JSON doctor run with `fleet` and `app` in the environment,
/// asserting neither stream shows a value of either URL.
fn timed_doctor(dir: &Path, fleet: &str, app: &str) -> (std::time::Duration, i32, Report) {
    let mut values = url_values(fleet);
    values.extend(url_values(app));
    let config = write_doctor_config(dir, &DoctorConfig::in_dir(dir));
    let env = [
        ("HOME", dir.as_os_str().to_owned()),
        ("FLEET_DATABASE_URL", OsString::from(fleet)),
        ("TRAWL_DATABASE_URL", OsString::from(app)),
    ];
    let args = [
        OsString::from("--doctor"),
        OsString::from("--config"),
        config.into_os_string(),
        OsString::from("--format"),
        OsString::from("json"),
    ];
    let started = std::time::Instant::now();
    let (code, stdout, stderr) = run_doctor(&args, &env);
    let elapsed = started.elapsed();
    let values: Vec<&str> = values.iter().map(String::as_str).collect();
    assert_no_values(&stdout, &stderr, &values);
    (elapsed, code, report(&stdout))
}

/// Assert both connect checks are `failed`, `not_postgres`, the run exits
/// `1`, and it did not wait out a 5 s connect deadline, as a run that
/// handed either URL to `SQLx` does.
fn assert_refused_as_not_postgres(
    report: &Report,
    code: i32,
    elapsed: std::time::Duration,
    label: &str,
) {
    for id in ["server.fleet.connect", "server.app.connect"] {
        let check = row(report, id);
        assert_eq!(
            (
                check.outcome,
                check.reason.as_deref(),
                check.next_action.as_deref()
            ),
            (
                Outcome::Failed,
                Some("not_postgres"),
                Some("check the database URL's host and port")
            ),
            "{label} {id}: {report:#?}"
        );
    }
    assert_eq!(code, 1, "{label}: {report:#?}");
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "{label}: took {elapsed:?}"
    );
}

/// A URL with `sslmode` `disable` or `allow` that names an HTTP server by
/// accident is `failed`, `not_postgres`, and the run does not wait on it:
/// the doctor sends its own startup message, reads the header `HTTP/`,
/// whose type `H` no PostgreSQL server answers with, and never hands the
/// URL to `SQLx`, which would read `HTTP/` as a frame of about 1.4 GB. Each
/// database gets one connection that carries the doctor's startup message
/// and no password, over a TCP host and port, a `hostaddr` that overrides
/// an unresolvable host name, and a Unix socket.
#[tokio::test(flavor = "multi_thread")]
async fn doctor_refuses_a_server_that_is_not_postgres() {
    for shape in ["host", "hostaddr", "unix"] {
        let dir = tempfile::tempdir().unwrap();
        let (at, received) = fake_server(dir.path(), shape == "unix", HTTP);
        let url = |db: &str, mode: &str| match (&at, shape) {
            (FakeAt::Tcp(addr), "host") => {
                format!("postgres://{ROLE}:{SECRET}@{addr}/{db}?sslmode={mode}")
            }
            (FakeAt::Tcp(addr), _) => format!(
                "postgres://{ROLE}:{SECRET}@planted-host.invalid:{}/{db}?sslmode={mode}\
                 &hostaddr={}",
                addr.port(),
                addr.ip()
            ),
            (FakeAt::Unix(sockets), _) => format!(
                "postgres://{ROLE}:{SECRET}@{}/{db}?sslmode={mode}",
                sockets.to_str().unwrap().replace('/', "%2F")
            ),
        };
        let fleet = url("planted_fleet_db", "disable");
        let app = url("planted_app_db", "allow");
        let (elapsed, code, report) = tokio::task::spawn_blocking({
            let dir = dir.path().to_owned();
            move || timed_doctor(&dir, &fleet, &app)
        })
        .await
        .unwrap();

        assert_refused_as_not_postgres(&report, code, elapsed, shape);
        assert_one_startup_per_database(
            &closed_connections(&received, 2),
            &["planted_fleet_db", "planted_app_db"],
            shape,
        );
    }
}

/// A server that speaks first, as SSH does, is `failed`, `not_postgres`
/// under `sslmode` `disable` and `allow`, and promptly: its banner's `S`
/// is what a PostgreSQL server answers an `SSLRequest` with, but no
/// PostgreSQL server answers a startup message with it. Each database gets
/// one connection, and `SQLx` none.
#[tokio::test(flavor = "multi_thread")]
async fn doctor_refuses_a_server_that_speaks_first() {
    let dir = tempfile::tempdir().unwrap();
    let (FakeAt::Tcp(addr), received) = fake_server(dir.path(), false, SSH) else {
        unreachable!("a TCP server")
    };
    let fleet = format!("postgres://{ROLE}:{SECRET}@{addr}/planted_fleet_db?sslmode=disable");
    let app = format!("postgres://{ROLE}:{SECRET}@{addr}/planted_app_db?sslmode=allow");
    let (elapsed, code, report) = tokio::task::spawn_blocking({
        let dir = dir.path().to_owned();
        move || timed_doctor(&dir, &fleet, &app)
    })
    .await
    .unwrap();

    assert_refused_as_not_postgres(&report, code, elapsed, "ssh");
    assert_one_startup_per_database(
        &closed_connections(&received, 2),
        &["planted_fleet_db", "planted_app_db"],
        "ssh",
    );
}

/// A first answer with a PostgreSQL type byte, `R`, and a length of 1 GiB
/// is `failed`, `not_postgres`: the doctor reads the 5-byte header and
/// refuses a length past 8 KiB instead of waiting for, or buffering, the
/// body it declares.
#[tokio::test(flavor = "multi_thread")]
async fn doctor_refuses_a_first_answer_past_its_length_bound() {
    let dir = tempfile::tempdir().unwrap();
    let (FakeAt::Tcp(addr), received) = fake_server(dir.path(), false, HUGE_R) else {
        unreachable!("a TCP server")
    };
    let fleet = format!("postgres://{ROLE}:{SECRET}@{addr}/planted_fleet_db?sslmode=disable");
    let app = format!("postgres://{ROLE}:{SECRET}@{addr}/planted_app_db?sslmode=allow");
    let (elapsed, code, report) = tokio::task::spawn_blocking({
        let dir = dir.path().to_owned();
        move || timed_doctor(&dir, &fleet, &app)
    })
    .await
    .unwrap();

    assert_refused_as_not_postgres(&report, code, elapsed, "huge R");
    assert_one_startup_per_database(
        &closed_connections(&received, 2),
        &["planted_fleet_db", "planted_app_db"],
        "huge R",
    );
}

/// Under the default `sslmode`, `prefer`, and under `require`, `SQLx` opens
/// with its own `SSLRequest` and refuses the byte `H`, so the same server
/// fails the connect check promptly, having received only the `SSLRequest`:
/// nothing was buffered toward a frame length.
#[tokio::test(flavor = "multi_thread")]
async fn sqlx_refuses_a_server_that_is_not_postgres_when_it_asks_for_tls() {
    let dir = tempfile::tempdir().unwrap();
    let (FakeAt::Tcp(addr), received) = fake_server(dir.path(), false, HTTP) else {
        unreachable!("a TCP server")
    };
    let fleet = format!("postgres://{ROLE}:{SECRET}@{addr}/planted_fleet_db");
    let app = format!("postgres://{ROLE}:{SECRET}@{addr}/planted_app_db?sslmode=require");
    let (elapsed, code, report) = tokio::task::spawn_blocking({
        let dir = dir.path().to_owned();
        move || timed_doctor(&dir, &fleet, &app)
    })
    .await
    .unwrap();

    for id in ["server.fleet.connect", "server.app.connect"] {
        assert_eq!(
            row(&report, id).outcome,
            Outcome::Failed,
            "{id}: {report:#?}"
        );
    }
    assert_eq!(code, 1, "{report:#?}");
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "took {elapsed:?}"
    );
    let seen = closed_connections(&received, 2);
    assert_eq!(seen, [SSL_REQUEST.to_vec(), SSL_REQUEST.to_vec()]);
}

/// A real PostgreSQL server under `sslmode` `disable` and `allow` answers
/// the doctor's startup message with an authentication message, the probe
/// ends without a password, and every database row completes.
#[tokio::test]
async fn doctor_takes_postgres_without_tls() {
    ensure_role().await;
    let fleet = with_query(&planted(&migrated_fleet().await), "sslmode=disable");
    let app = with_query(&planted(&migrated_app().await), "sslmode=allow");
    let dir = tempfile::tempdir().unwrap();
    let (_, report, stderr) = doctor_env(dir.path(), &fleet, &app);
    database_rows_complete(&report).unwrap_or_else(|why| panic!("{why}: {report:#?}"));
    assert!(stderr.is_empty(), "{stderr}");
}
