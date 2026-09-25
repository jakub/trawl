// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared harness for trawld's pg-backed end-to-end tests.
//!
//! A full-server fixture owns every resource it hands the server (ADR-0021
//! rulings 1-3). It mints BOTH databases itself — fleet keystore and trawl
//! app state — from `DATABASE_URL`, builds one sized pool on each, and
//! passes those pools in through `AppState::from_parts`. Nothing here runs
//! under `#[sqlx::test]`: that macro's teardown closes the pool under a
//! 10-second timeout and then drops the database regardless, while the
//! server's detached tasks (collector, scheduler, compaction) still hold
//! connections. `#[sqlx::test]` survives only where the handed pool is the
//! sole connection holder — the store-level suites.
//!
//! The app database is still migrated by trawld's REAL boot path
//! (`StorageState::from_pool`: advisory lock → migrate), so every server
//! test exercises boot-time migration; the fixture runs
//! `fleet_auth::MIGRATOR` on the fleet database itself.
//!
//! The two schemas must live in two databases: sqlx hardwires one
//! `_sqlx_migrations` table per database and both migration sets would
//! collide in it. Two databases also match the production shape.
//!
//! Sibling databases are best-effort leftovers: nextest is process-per-test
//! and the server's pools stay open until process exit, so we don't drop
//! them (a throwaway CI container makes leaks acceptable; local dev reuses
//! names prefixed `trawl_app_test_` for easy bulk cleanup).

#![allow(dead_code)] // each test binary uses a subset of these items

use std::net::TcpListener;
use std::path::PathBuf;

use fleet_auth::{KeyStore, PrincipalKind, RolePermission};
use sqlx::{Connection as _, Executor as _, PgPool, postgres::PgConnection};

/// Build a `trawl`-namespace [`RolePermission`] list from permission strings.
pub fn trawl_perms(perms: &[&str]) -> Vec<RolePermission> {
    perms
        .iter()
        .map(|p| RolePermission {
            app: "trawl".into(),
            permission: (*p).to_owned(),
        })
        .collect()
}

/// Seed the four trawl roles whose permission sets the production migration
/// freezes (so the `/whoami` goldens are the real wire shape), the
/// schema-admin role, and a coastwatch-only role for grantless-key tests.
pub async fn seed_trawl_roles(store: &KeyStore) {
    store
        .create_role(
            "trawl-admin",
            None,
            &trawl_perms(&[
                "query",
                "schema_read",
                "validate",
                "saved_query",
                "export",
                "stream",
                "query_cancel",
                "server_manage",
            ]),
        )
        .await
        .expect("seed trawl-admin");
    store
        .create_role(
            "trawl-analyst",
            None,
            &trawl_perms(&[
                "query",
                "schema_read",
                "validate",
                "saved_query",
                "export",
                "stream",
                "query_cancel",
            ]),
        )
        .await
        .expect("seed trawl-analyst");
    store
        .create_role(
            "trawl-reader",
            None,
            &trawl_perms(&["query", "schema_read", "query_cancel"]),
        )
        .await
        .expect("seed trawl-reader");
    store
        .create_role("trawl-ingest", None, &trawl_perms(&["ingest"]))
        .await
        .expect("seed trawl-ingest");
    // The schema-admin shape: schema_write without server_manage, which is
    // the whole point of the separate permission. The fleet migration
    // registers schema_write and grants it to no role, so a test that needs
    // it seeds this one.
    store
        .create_role(
            "trawl-schema-admin",
            None,
            &trawl_perms(&["query", "schema_read", "schema_write"]),
        )
        .await
        .expect("seed trawl-schema-admin");
    store
        .create_role(
            "coastwatch-viewer",
            None,
            &[RolePermission {
                app: "coastwatch".into(),
                permission: "stories_read".into(),
            }],
        )
        .await
        .expect("seed coastwatch-viewer");
}

/// Helper: a single-role name list for `create_key`.
pub fn roles(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| (*s).to_owned()).collect()
}
use trawl_server::config::{
    AuthConfig, Config, DataConfig, IngestConfig, RateLimitConfig, RetentionConfig,
    SchedulerConfig, ServerConfig, StorageConfig, SyslogConfig, WebConfig,
};
use trawl_server::state::AppState;
use trawl_server::transport::http;

/// Scrape `trawl_catalog_bookkeeping_timeouts_total` off a running server,
/// keyed by its `write` label. A label with no series reads 0.
///
/// Read DELTAS only, never absolute values: the prometheus recorder is
/// process-global (see `test_metrics_handle`), so under plain `cargo test`
/// every test in the binary contributes to the same counter. The difference
/// across one test's own window is the only part that belongs to it.
pub async fn bookkeeping_timeouts(url: &str) -> std::collections::BTreeMap<String, f64> {
    let name = trawl_server::metrics::CATALOG_BOOKKEEPING_TIMEOUTS_TOTAL;
    let body = harness_client_builder()
        .build()
        .expect("client")
        .get(format!("{url}/metrics"))
        .send()
        .await
        .expect("GET /metrics")
        .text()
        .await
        .expect("metrics body");

    let mut counts: std::collections::BTreeMap<String, f64> =
        trawl_server::metrics::BookkeepingWrite::ALL
            .iter()
            .map(|w| (w.label().to_owned(), 0.0))
            .collect();
    for line in body.lines() {
        let Some(rest) = line.strip_prefix(name) else {
            continue;
        };
        let Some((labels, value)) = rest
            .strip_prefix("{write=\"")
            .and_then(|r| r.split_once("\"}"))
        else {
            continue;
        };
        if let Ok(n) = value.trim().parse::<f64>() {
            counts.insert(labels.to_owned(), n);
        }
    }
    counts
}

/// Assert that no catalog bookkeeping write was abandoned during the window
/// the two scrapes bracket.
///
/// A test that reads conflict rows or per-service observations is reading
/// what compaction's phase-4 bookkeeping wrote, and that write is
/// best-effort under a two-second budget. When postgres is slow enough on a
/// loaded machine, the budget expires, the evidence never lands, and the
/// test fails on a value assertion that says nothing about the code under
/// test. This turns that into a sentence naming the cause.
///
/// The recorder is process-global, so under plain `cargo test` (which runs
/// tests as threads of one process) a SIBLING test's timeout inside this
/// window trips the assertion too. That is deliberate slack, not a defect:
/// either way the failure names bookkeeping starvation rather than a
/// mystery value, and nextest (the repo's runner everywhere) isolates
/// per-process, where the window can only see its own test.
pub fn assert_bookkeeping_quiet(
    before: &std::collections::BTreeMap<String, f64>,
    after: &std::collections::BTreeMap<String, f64>,
) {
    for (write, now) in after {
        let then = before.get(write).copied().unwrap_or(0.0);
        assert!(
            *now <= then,
            "catalog bookkeeping starvation: the {write} write was abandoned at its \
             budget during this test's window ({then} -> {now} on {}), so evidence \
             is missing and the assertion below is measuring a slow postgres, not \
             the product (under plain `cargo test` the abandoning test may be a \
             concurrent sibling; nextest isolates per-process)",
            trawl_server::metrics::CATALOG_BOOKKEEPING_TIMEOUTS_TOTAL,
        );
    }
}

/// Create a `PrometheusHandle` for test contexts.
///
/// The prometheus recorder is process-global: `metrics::counter!` always
/// records into whichever recorder was installed first, regardless of
/// which handle a given server renders `/metrics` from. Under nextest
/// (process-per-test) that is always the test's own; under plain
/// `cargo test` every test in the binary shares one process, so a second
/// server's freshly-built recorder would never see the counters the
/// handlers emit — its `/metrics` renders empty and any assertion on it
/// fails only under `cargo test`. Every harness server in the process
/// therefore renders the same handle: the first call installs the global
/// recorder and caches its handle, and every later call clones it.
pub fn test_metrics_handle() -> metrics_exporter_prometheus::PrometheusHandle {
    static HANDLE: std::sync::OnceLock<metrics_exporter_prometheus::PrometheusHandle> =
        std::sync::OnceLock::new();
    HANDLE
        .get_or_init(|| {
            metrics_exporter_prometheus::PrometheusBuilder::new()
                .install_recorder()
                .expect(
                    "the harness owns recorder installation for this process; \
                     nothing else may install one first",
                )
        })
        .clone()
}

/// The base admin DSN every fixture database is minted from.
///
/// Read straight from the environment, because the full-server fixture no
/// longer runs under `#[sqlx::test]` and so has no harness reading it on
/// the fixture's behalf. An unset variable panics loudly and fails the
/// test: a pg-backed suite that silently skips is a suite that passes
/// while proving nothing.
pub fn admin_database_url() -> String {
    std::env::var("DATABASE_URL").expect(
        "DATABASE_URL must be set for the pg-backed suites \
         (e.g. postgres://fleet:fleet@localhost:5433/fleet_test); \
         these tests fail rather than skip",
    )
}

/// Replace the database path of a postgres URL, preserving any query string.
pub fn swap_database(url: &str, db: &str) -> String {
    let (base, query) = url
        .split_once('?')
        .map_or((url, None), |(b, q)| (b, Some(q)));
    let after_scheme = base.find("://").map_or(0, |i| i + 3);
    let authority = base[after_scheme..]
        .find('/')
        .map_or(base, |i| &base[..after_scheme + i]);
    match query {
        Some(q) => format!("{authority}/{db}?{q}"),
        None => format!("{authority}/{db}"),
    }
}

fn random_db_suffix() -> String {
    use rand::Rng as _;
    let mut rng = rand::thread_rng();
    (0..12)
        .map(|_| {
            let n: u8 = rng.gen_range(0..26);
            (b'a' + n) as char
        })
        .collect()
}

/// DSN of a pool's own database, rebuilt against the admin DSN — for the
/// `#[sqlx::test]` suites, which need a second connection to the database
/// the macro handed them.
pub fn fleet_database_url(pool: &PgPool) -> String {
    let opts = pool.connect_options();
    let db = opts.get_database().expect("test pool has a database");
    swap_database(&admin_database_url(), db)
}

/// Prefix of the app-state databases this fixture mints.
const APP_DB_PREFIX: &str = "trawl_app_test";

/// Prefix of the fleet-keystore databases this fixture mints (ADR-0021
/// ruling 2: a full-server fixture outlives `#[sqlx::test]`'s teardown, so
/// it mints the fleet database itself instead of borrowing the harness's).
const FLEET_DB_PREFIX: &str = "trawl_fleet_test";

/// Every prefix the sweeper is allowed to drop. A prefix that mints must
/// appear here or its leftovers accumulate forever.
const SWEPT_PREFIXES: [&str; 2] = [APP_DB_PREFIX, FLEET_DB_PREFIX];

/// Fleet-keystore pool size for one fixture server.
pub const FLEET_POOL_MAX: u32 = 3;

/// App-store pool size for one fixture server.
pub const APP_POOL_MAX: u32 = 3;

/// Ceiling of the pool `#[sqlx::test]` hands a test.
///
/// Not ours to choose: sqlx builds it at `max_connections(5)`
/// (`sqlx-postgres/src/testing/mod.rs`). It is charged in full to every
/// shape that keeps the macro, because a test is free to drive five
/// queries at once through it.
pub const SQLX_TEST_POOL_MAX: u32 = 5;

/// The advisory-lock session one `StorageState` holds for its lifetime.
///
/// Charged on top of the app pool's own ceiling: the store acquires it from
/// that pool and then detaches it, so the pool refills the slot and the
/// process holds `APP_POOL_MAX` plus this one.
pub const LOCK_CONNECTIONS: u32 = 1;

/// The short-lived admin connection a mint, a sweep or a kill opens and
/// closes. Charged to the RUN (see `connection_budget`'s headroom), not to
/// a shape: it exists for one statement, not for the test.
pub const ADMIN_TRANSIENT: u32 = 1;

// -- per-shape ceilings ----------------------------------------------------
//
// A shape is charged what a test HOLDS for its duration: every live pool at
// its `max_connections`, plus every raw connection it keeps open. The
// nextest group width is derived from the WORST of them against
// [`CI_MAX_CONNECTIONS`] (ADR-0021 ruling 4).
//
// One shape is deliberately absent: fleet-auth's `connect_and_ping_via_url`
// builds a PRODUCTION keystore pool (`KeyStore::connect`, ceiling 8) beside
// its harness pool, which by pool-ceiling arithmetic would be 13. That test
// is the one place the production constructor itself is under test, it
// issues one query at a time and closes the pool before returning, so it
// holds at most three connections. Read it before trusting this note.

/// A full-server fixture: fleet pool + app pool + the app store's
/// advisory-lock session.
pub const FULL_SERVER_CONNECTION_CEILING: u32 = FLEET_POOL_MAX + APP_POOL_MAX + LOCK_CONNECTIONS;

/// A full-server fixture whose TEST also opens its own pool on the server's
/// app database, to drive a store directly (`http_api`'s schedule cases).
pub const DIRECT_STORE_CONNECTION_CEILING: u32 = FULL_SERVER_CONNECTION_CEILING + APP_POOL_MAX;

/// A store-level `#[sqlx::test]` that keeps the harness pool AND boots one
/// `StorageState` of its own on a fixture-minted database (`auth_pg`'s
/// `ac6_*` scheduler cases).
pub const SQLX_STORE_CONNECTION_CEILING: u32 = SQLX_TEST_POOL_MAX + APP_POOL_MAX + LOCK_CONNECTIONS;

/// A boot test holding TWO live `StorageState`s at once (the original and
/// its replacement, in the advisory-lock cases). These tests take no
/// harness pool at all.
pub const BOOT_CONNECTION_CEILING: u32 = 2 * (APP_POOL_MAX + LOCK_CONNECTIONS);

/// The widest shape in the postgres admission group. `connection_budget`
/// multiplies THIS by the group width.
pub const WORST_TEST_CONNECTION_CEILING: u32 = max_u32(
    max_u32(
        FULL_SERVER_CONNECTION_CEILING,
        DIRECT_STORE_CONNECTION_CEILING,
    ),
    max_u32(SQLX_STORE_CONNECTION_CEILING, BOOT_CONNECTION_CEILING),
);

/// `u32::max` is not const-callable in this MSRV path; this is.
const fn max_u32(a: u32, b: u32) -> u32 {
    if a > b { a } else { b }
}

/// `max_connections` of the postgres CI runs against.
pub const CI_MAX_CONNECTIONS: u32 = 100;

/// The one sanctioned pool constructor for test code.
///
/// Every fixture pool is built here with an explicit ceiling, so the
/// per-test connection budget is a property of this file rather than of
/// whichever test last copied a `PgPoolOptions` chain. The `allow` below
/// is the single enforcement point: once the pool constructors land in
/// clippy's `disallowed_methods` for test code, this is the only site
/// that opts out.
pub async fn fixture_pool(dsn: &str, max_connections: u32) -> PgPool {
    #[allow(clippy::disallowed_methods)]
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(max_connections)
        .connect(dsn)
        .await
        .expect("fixture pool connect");
    pool
}

/// A fleet-keystore pool sized by [`FLEET_POOL_MAX`].
pub async fn fleet_pool(dsn: &str) -> PgPool {
    fixture_pool(dsn, FLEET_POOL_MAX).await
}

/// An app-store pool sized by [`APP_POOL_MAX`].
pub async fn app_pool(dsn: &str) -> PgPool {
    fixture_pool(dsn, APP_POOL_MAX).await
}

/// Create an EMPTY database under `prefix` and return its DSN.
///
/// The run marker and owner pid are encoded in the name so a future run
/// can tell an abandoned database from a live sibling's (see the sweep),
/// which is why every mint goes through this one formatter.
async fn create_test_database(prefix: &str) -> String {
    sweep_stale_test_databases().await;
    let name = format!(
        "{prefix}_{}_{}_{}",
        run_marker(),
        std::process::id(),
        random_db_suffix()
    );
    let mut admin = PgConnection::connect(&admin_database_url())
        .await
        .expect("connect to admin DB to mint a test database");
    admin
        .execute(sqlx::AssertSqlSafe(format!(r#"CREATE DATABASE "{name}""#)))
        .await
        .expect("CREATE DATABASE for a test database — does the role have CREATEDB?");
    swap_database(&admin_database_url(), &name)
}

/// Create an EMPTY sibling database for the trawl app-state store and
/// return its DSN. trawld's real boot path migrates it (AC5).
pub async fn create_app_database() -> String {
    create_test_database(APP_DB_PREFIX).await
}

/// Create an EMPTY database for a fixture-owned fleet keystore and return
/// its DSN. The caller runs `fleet_auth::MIGRATOR` on it.
pub async fn create_fleet_database() -> String {
    create_test_database(FLEET_DB_PREFIX).await
}

/// A marker shared by every test process of THIS nextest run (nextest
/// exposes `NEXTEST_RUN_ID`; plain `cargo test` shares one process, so the
/// pid suffices as fallback). Only alphanumerics survive, for db-name
/// safety.
fn run_marker() -> String {
    std::env::var("NEXTEST_RUN_ID")
        .unwrap_or_else(|_| std::process::id().to_string())
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(12)
        .collect()
}

/// Parse `(run_marker, pid)` out of a `{prefix}_{run}_{pid}_{rand}` name.
/// Names from other schemes yield `None` and are left alone.
fn owner_of(datname: &str, prefix: &str) -> Option<(String, u32)> {
    let mut parts = datname.strip_prefix(prefix)?.strip_prefix('_')?.split('_');
    let marker = parts.next()?.to_owned();
    let pid = parts.next()?.parse().ok()?;
    Some((marker, pid))
}

/// Whether a process with this pid is still alive (same host — nextest
/// processes are local). Errs on the side of "alive".
fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stderr(std::process::Stdio::null())
        .status()
        .map_or(true, |s| s.success())
}

/// Best-effort sweep of test databases leaked by earlier runs, over every
/// prefix in [`SWEPT_PREFIXES`].
///
/// The server's pools stay open until process exit, so a test cannot drop
/// its OWN databases; instead each run garbage-collects its predecessors'.
/// A database is only dropped when BOTH guards agree it is abandoned: (a)
/// it belongs to a DIFFERENT nextest run — same-run siblings are
/// structurally never touched, even in the window between their CREATE and
/// the server's first connection — and (b) the owner pid encoded in its
/// name is no longer alive. A single advisory lock elects one sweeper at a
/// time, and DROP without FORCE is a final safety net (live connections
/// make it error harmlessly). One lock, one pass, one LIKE per prefix.
async fn sweep_stale_test_databases() {
    use sqlx::Row as _;

    let Ok(mut admin) = PgConnection::connect(&admin_database_url()).await else {
        return;
    };
    // One sweeper at a time, and only if the lock is free right now.
    let Ok(got_lock) = sqlx::query_scalar::<_, bool>("SELECT pg_try_advisory_lock(741_852_963)")
        .fetch_one(&mut admin)
        .await
    else {
        return;
    };
    if !got_lock {
        return;
    }

    let marker = run_marker();
    for prefix in SWEPT_PREFIXES {
        let Ok(rows) = sqlx::query("SELECT datname FROM pg_database WHERE datname LIKE $1")
            .bind(format!("{prefix}_%"))
            .fetch_all(&mut admin)
            .await
        else {
            continue;
        };
        for row in rows {
            let Ok(name): Result<String, _> = row.try_get("datname") else {
                continue;
            };
            let Some((owner_marker, owner_pid)) = owner_of(&name, prefix) else {
                continue;
            };
            if owner_marker == marker || pid_alive(owner_pid) {
                continue;
            }
            let _ = admin
                .execute(sqlx::AssertSqlSafe(format!(
                    r#"DROP DATABASE IF EXISTS "{name}""#
                )))
                .await;
        }
    }
    let _ = sqlx::query("SELECT pg_advisory_unlock(741_852_963)")
        .execute(&mut admin)
        .await;
}

#[test]
fn sweep_guards_parse_own_database_name() {
    // The sweeper's abandoned-db detection must round-trip the naming
    // scheme `create_test_database` uses, for EVERY prefix it mints under
    // — a parse mismatch here silently turns the sweeper into a
    // live-sibling killer (it did once: the guard-bypassing bug behind
    // transient 'database does not exist' boot failures).
    for prefix in SWEPT_PREFIXES {
        let name = format!(
            "{prefix}_{}_{}_{}",
            run_marker(),
            std::process::id(),
            "abcdefghijkl"
        );
        let (marker, pid) = owner_of(&name, prefix).expect("own name must parse");
        assert_eq!(marker, run_marker());
        assert_eq!(pid, std::process::id());
        assert!(pid_alive(pid), "our own pid is alive");
    }
    // A name minted under one prefix must not parse as another's: the
    // sweeper reads each LIKE result under the prefix that matched it.
    let app = format!(
        "{APP_DB_PREFIX}_{}_{}_abcdefghijkl",
        run_marker(),
        std::process::id()
    );
    assert!(owner_of(&app, FLEET_DB_PREFIX).is_none());
    assert!(owner_of("someone_elses_database", APP_DB_PREFIX).is_none());
}

/// The database name a DSN points at.
fn database_name(url: &str) -> &str {
    url.rsplit('/')
        .next()
        .and_then(|last| last.split('?').next())
        .expect("database name in url")
}

/// Forcibly drop a database by DSN, terminating live connections —
/// simulates a backend dying under a running server.
pub async fn kill_database(url: &str) {
    let name = database_name(url);
    let mut admin = PgConnection::connect(&admin_database_url())
        .await
        .expect("connect to admin DB");
    admin
        .execute(sqlx::AssertSqlSafe(format!(
            r#"DROP DATABASE IF EXISTS "{name}" WITH (FORCE)"#
        )))
        .await
        .expect("force-drop database");
}

/// Terminate every backend connected to the database named in `url`,
/// leaving the database itself intact — simulates the lock-holding session
/// dying (pg restart, idle-timeout culling, `pg_terminate_backend`) while
/// the database stays up so a replacement instance can re-acquire the lock.
///
/// Kills the dedicated advisory-lock connection *and* the app-state pool's
/// connections; sqlx transparently reconnects the pool (the split-brain
/// hazard), but the raw lock connection cannot, so its session-held
/// advisory lock is released.
pub async fn terminate_backends(url: &str) {
    let name = database_name(url);
    let mut admin = PgConnection::connect(&admin_database_url())
        .await
        .expect("connect to admin DB");
    // `pg_stat_activity`/`pg_terminate_backend` are cluster-wide; the admin
    // connection sits on a different database, so filter by datname.
    sqlx::query(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE datname = $1 AND pid <> pg_backend_pid()",
    )
    .bind(name)
    .execute(&mut admin)
    .await
    .expect("terminate app-database backends");
}

/// Serializes [`publish_dir_once`] within one test process.
static PUBLISH_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();

/// Publish a directory exactly once, atomically, for every test process
/// and every test thread that races to build it.
///
/// If `dest` exists, there is nothing to do. Otherwise `build` fills a
/// sibling staging directory (a sibling so the rename stays on one
/// filesystem) and one `rename` publishes it whole.
///
/// The old shape published each FILE with its own rename and then decided
/// "already built?" by testing ONE of them (ADR-0021 ruling 5). A reader
/// arriving between the two renames saw the cert without the key, or the
/// nginx parquet without the postgres one, and failed on a fixture that
/// was merely half-published. One rename per directory removes that
/// window: a consumer sees either no directory or a complete one.
///
/// Two races, two guards. ACROSS processes the rename decides: the loser
/// gets `EEXIST`/`ENOTEMPTY` and adopts the winner's tree, but only after
/// checking that every entry it staged is present under `dest`. A rename
/// that failed for any other reason, or a `dest` that is missing something
/// we built, is an error, never a silent pass. WITHIN one process (plain
/// `cargo test` runs many tests per binary in one process, on many
/// threads) the staging path itself was the hazard: it was named after the
/// pid alone, so two threads shared it and one deleted the other's
/// half-built tree. The staging name now carries a per-call nonce, and one
/// process-wide mutex serializes publication so the second thread finds
/// `dest` already in place instead of building a rival copy. No caller ever
/// removes a staging path but its own.
fn publish_dir_once(
    dest: &std::path::Path,
    build: impl FnOnce(&std::path::Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    if dest.exists() {
        return Ok(());
    }

    // A panicking publisher leaves the lock poisoned but the filesystem
    // consistent (staging trees are private and `dest` only ever appears
    // whole), so the next caller may proceed.
    let _serialized = PUBLISH_LOCK
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // A sibling thread may have published while we waited for the lock.
    if dest.exists() {
        return Ok(());
    }

    let staging = dest.with_file_name(format!(
        "{}.staging.{}",
        dest.file_name()
            .and_then(std::ffi::OsStr::to_str)
            .expect("fixture destination has a UTF-8 file name"),
        staging_nonce()
    ));
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // `create_dir`, not `create_dir_all`: this path is ours alone, so an
    // existing one is a surprise worth failing on rather than a tree to
    // wipe.
    std::fs::create_dir(&staging)?;
    if let Err(e) = build(&staging) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }

    let staged_entries = dir_entry_names(&staging)?;
    match std::fs::rename(&staging, dest) {
        Ok(()) => Ok(()),
        Err(e) => {
            let rival_won = matches!(
                e.kind(),
                std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::DirectoryNotEmpty
            );
            let adopted = rival_won && published_tree_is_complete(dest, &staged_entries);
            let _ = std::fs::remove_dir_all(&staging);
            if adopted { Ok(()) } else { Err(e) }
        }
    }
}

/// A staging-directory suffix no other caller can pick: the pid separates
/// processes, the counter separates threads and repeat calls in one
/// process, and the random tail separates us from a crashed predecessor
/// whose pid the OS has since handed back.
fn staging_nonce() -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{}.{n}.{}", std::process::id(), random_db_suffix())
}

/// Sorted top-level entry names of a directory.
fn dir_entry_names(dir: &std::path::Path) -> std::io::Result<Vec<std::ffi::OsString>> {
    let mut names = std::fs::read_dir(dir)?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<std::io::Result<Vec<_>>>()?;
    names.sort();
    Ok(names)
}

/// Whether the tree a rival published carries at least everything we
/// staged. Losing the rename only counts as success when it did.
fn published_tree_is_complete(dest: &std::path::Path, staged: &[std::ffi::OsString]) -> bool {
    let Ok(published) = dir_entry_names(dest) else {
        return false;
    };
    staged.iter().all(|name| published.contains(name))
}

#[test]
fn publish_dir_once_has_one_winner_under_thread_contention() {
    // The regression guard for the same-process race: under plain `cargo
    // test` many tests share one process, so two threads can reach an
    // unpublished fixture at the same instant. Exactly one may build, and
    // every caller must return to a COMPLETE tree. The old pid-only
    // staging name let the second thread delete the first one's half-built
    // directory and publish the remains.
    let tmp = tempfile::tempdir().expect("tempdir");
    let dest = tmp.path().join("fixture-v1");
    let builds = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let files = ["a.txt", "b.txt", "c.txt"];

    std::thread::scope(|scope| {
        for _ in 0..8 {
            let dest = dest.clone();
            let builds = std::sync::Arc::clone(&builds);
            scope.spawn(move || {
                publish_dir_once(&dest, |staging| {
                    builds.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    for name in files {
                        // Sleep between files: a build that is instantaneous
                        // would not expose a rival deleting it mid-flight.
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        std::fs::write(staging.join(name), name)?;
                    }
                    Ok(())
                })
                .expect("every racing caller must succeed");
            });
        }
    });

    assert_eq!(
        builds.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "exactly one thread may build the fixture"
    );
    for name in files {
        let content = std::fs::read_to_string(dest.join(name))
            .unwrap_or_else(|e| panic!("published tree must carry {name}: {e}"));
        assert_eq!(content, name);
    }
    let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
        .expect("read tempdir")
        .filter_map(|e| e.ok().map(|e| e.file_name()))
        .filter(|name| name != "fixture-v1")
        .collect();
    assert!(
        leftovers.is_empty(),
        "staging directories must not survive publication: {leftovers:?}"
    );
}

/// Root of the shared, published fixture directories.
fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

/// Publish the parquet seed tree once and return its root.
///
/// The directory name carries a version. Change the seed data and bump
/// `-v1`: a stale tree from an older checkout is then simply a directory
/// nobody looks at, instead of a half-recognised fixture some test has to
/// detect and delete.
///
/// The tree is READ-ONLY to tests. Each test copies it into its own data
/// root ([`seed_data_root`]), so nothing writes here.
fn published_parquet_root() -> PathBuf {
    let dest = fixture_root().join("parquet-v1");
    publish_dir_once(&dest, |staging| {
        // The ADR-0009 on-disk layout the query planner prunes over:
        // `{data}/{env}/{date}/{HH}/{service}.parquet`. Fixtures live
        // there because that is the only shape the server ever writes — a
        // flat data root is only reachable through a whole-root `**`
        // glob, which the planner deliberately no longer emits (it would
        // swallow `scheduled/`).
        let hour_dir = staging.join("prod").join("2024-01-15").join("10");
        std::fs::create_dir_all(&hour_dir)?;

        let conn = duckdb::Connection::open_in_memory().expect("open duckdb");
        conn.execute_batch(
            "CREATE TABLE logs (
                _time TIMESTAMP,
                _ingested TIMESTAMP,
                _raw VARCHAR,
                _repairs VARCHAR,
                env VARCHAR,
                service VARCHAR,
                host VARCHAR,
                severity BIGINT,
                severity_text VARCHAR,
                message VARCHAR
            )",
        )
        .expect("create fixture table");
        conn.execute_batch(
            "INSERT INTO logs VALUES
            ('2024-01-15 10:00:00', '2024-01-15 10:00:10', 'raw0', NULL, 'prod', 'nginx', 'web01', 9, 'info', 'request ok'),
            ('2024-01-15 10:00:01', '2024-01-15 10:00:11', 'raw1', NULL, 'prod', 'nginx', 'web01', 17, 'error', 'upstream timeout'),
            ('2024-01-15 10:00:02', '2024-01-15 10:00:12', 'raw2', NULL, 'prod', 'postgres', 'db01', 9, 'info', 'checkpoint complete')",
        )
        .expect("insert fixture rows");

        for service in ["nginx", "postgres"] {
            let path = hour_dir.join(format!("{service}.parquet"));
            conn.execute_batch(&format!(
                "COPY (SELECT * FROM logs WHERE service = '{service}') TO '{}' (FORMAT PARQUET)",
                path.display()
            ))
            .expect("write fixture parquet");
        }
        Ok(())
    })
    .expect("publish the shared parquet fixture tree");
    dest
}

/// Copy a directory tree recursively.
fn copy_tree(src: &std::path::Path, dest: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let to = dest.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

/// Seed a PRIVATE data root under `dir` from the published parquet tree
/// and return its directory path.
///
/// Every test gets its own copy because the data root is WRITABLE: the
/// scheduler drops report output under `scheduled/`, and repin stages
/// siblings of it. Sharing one root meant tests deleting each other's
/// `scheduled/` directory to stay deterministic (ADR-0021 ruling 7).
/// A private root removes both the wipe and the cross-test coupling.
///
/// The seed is COPIED, not hardlinked: `dir` is a tempdir on /tmp, which
/// is a tmpfs here, so a link across from the repo's filesystem is EXDEV.
pub fn seed_data_root(dir: &std::path::Path) -> String {
    let data = dir.join("data");
    copy_tree(&published_parquet_root(), &data).expect("seed the private data root");
    data.to_str().unwrap().to_owned()
}

/// Return a shared self-signed cert/key pair, published on first call.
///
/// Both files are staged and published by ONE rename, so a reader never
/// sees the cert without its key.
pub fn ensure_test_cert() -> (PathBuf, PathBuf) {
    let dir = fixture_root().join("tls-v1");
    publish_dir_once(&dir, |staging| {
        let san = vec!["localhost".to_owned(), "127.0.0.1".to_owned()];
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(san).expect("generate self-signed pair");
        std::fs::write(staging.join("cert.pem"), cert.pem())?;
        std::fs::write(staging.join("key.pem"), signing_key.serialize_pem())?;
        Ok(())
    })
    .expect("publish the shared TLS fixture pair");
    (dir.join("cert.pem"), dir.join("key.pem"))
}

/// A reqwest builder that trusts the [`ensure_test_cert`] certificate as a
/// root, with certificate and hostname verification left on.
///
/// Every server these suites start serves that pair: the in-process fixture
/// passes it as `tls_cert_path`, and `startup_pg` writes it into the spawned
/// trawld's config. Its SANs cover `localhost` and `127.0.0.1`. Callers add
/// their own timeout and redirect settings before `build()`.
pub fn harness_client_builder() -> reqwest::ClientBuilder {
    let (cert_path, _) = ensure_test_cert();
    let pem = std::fs::read(&cert_path).expect("read the shared test certificate");
    let certificate =
        reqwest::Certificate::from_pem(&pem).expect("parse the shared test certificate");
    reqwest::Client::builder().add_root_certificate(certificate)
}

/// What a failing test needs to know about its own fixture (ADR-0021
/// ruling 8): which two databases it minted, which port it bound, and the
/// connection ceilings it was sized against.
///
/// Names and numbers ONLY. There is no DSN and no token in this struct,
/// so no print of it can leak one. The admin DSN the activity snapshot
/// connects with is read from the environment at print time and never
/// echoed.
/// Private on purpose: the panic path is the only consumer, and a `pub`
/// field here is a field a test could overwrite with a DSN, which is
/// exactly the leak the struct exists to avoid.
struct FixtureFacts {
    /// Name (not DSN) of the app-state database.
    app_db: String,
    /// Name (not DSN) of the fleet-keystore database.
    fleet_db: String,
    /// `host:port` the server bound.
    bound_addr: String,
    fleet_pool_max: u32,
    app_pool_max: u32,
    worst_connection_ceiling: u32,
}

impl FixtureFacts {
    fn new(app_db_url: &str, fleet_db_url: &str, bound_addr: &str) -> Self {
        Self {
            app_db: database_name(app_db_url).to_owned(),
            fleet_db: database_name(fleet_db_url).to_owned(),
            bound_addr: bound_addr.to_owned(),
            fleet_pool_max: FLEET_POOL_MAX,
            app_pool_max: APP_POOL_MAX,
            worst_connection_ceiling: WORST_TEST_CONNECTION_CEILING,
        }
    }

    /// Print the facts, then whatever postgres is willing to say about
    /// the backends on those two databases.
    fn report(&self) {
        eprintln!("--- fixture facts ---");
        eprintln!("  app database:   {}", self.app_db);
        eprintln!("  fleet database: {}", self.fleet_db);
        eprintln!("  bound addr:     {}", self.bound_addr);
        eprintln!(
            "  pool ceilings:  fleet={} app={} worst-shape={}",
            self.fleet_pool_max, self.app_pool_max, self.worst_connection_ceiling
        );
        print_pg_activity(vec![self.app_db.clone(), self.fleet_db.clone()]);
    }
}

/// Print a `pg_stat_activity` census of this test's two databases.
///
/// Grouped counts only: datname, `backend_type`, state and the wait
/// event. No `usename`, `client_addr`, `application_name` or `query`: a
/// diagnostic that dumps SQL text or connection identities into CI logs
/// is a leak, and the grouped counts are what distinguish "pool
/// exhausted" from "everyone is waiting on one lock".
///
/// `Drop` cannot await, and `Handle::block_on` panics when called from a
/// runtime worker thread, so the work happens on a fresh std thread with
/// its own current-thread runtime. The join is capped at 2 seconds: a
/// wedged postgres must not turn one failing test into a hung run. Any
/// failure prints a CLASS and nothing else.
fn print_pg_activity(databases: Vec<String>) {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let outcome = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt.block_on(collect_pg_activity(&databases)),
            Err(_) => Err("runtime"),
        };
        let _ = tx.send(outcome);
    });
    match rx.recv_timeout(std::time::Duration::from_secs(2)) {
        Ok(Ok(lines)) => {
            eprintln!("--- pg_stat_activity ---");
            if lines.is_empty() {
                eprintln!("  (no backends on either database)");
            }
            for line in lines {
                eprintln!("  {line}");
            }
        }
        Ok(Err(class)) => eprintln!("diagnostics unavailable: {class}"),
        Err(_) => eprintln!("diagnostics unavailable: timeout"),
    }
}

async fn collect_pg_activity(databases: &[String]) -> Result<Vec<String>, &'static str> {
    use sqlx::Row as _;

    let Ok(dsn) = std::env::var("DATABASE_URL") else {
        return Err("no DATABASE_URL");
    };
    let mut admin = PgConnection::connect(&dsn).await.map_err(|_| "connect")?;
    let rows = sqlx::query(
        "SELECT datname, backend_type, state, wait_event_type, wait_event, count(*) AS n \
         FROM pg_stat_activity WHERE datname = ANY($1) \
         GROUP BY 1, 2, 3, 4, 5 ORDER BY 1, 2, 3",
    )
    .bind(databases)
    .fetch_all(&mut admin)
    .await
    .map_err(|_| "query")?;

    let unset = |v: Option<String>| v.unwrap_or_else(|| "-".to_owned());
    Ok(rows
        .into_iter()
        .map(|row| {
            let n: i64 = row.try_get("n").unwrap_or(-1);
            format!(
                "{} {} state={} wait={}/{} count={n}",
                unset(row.try_get("datname").ok().flatten()),
                unset(row.try_get("backend_type").ok().flatten()),
                unset(row.try_get("state").ok().flatten()),
                unset(row.try_get("wait_event_type").ok().flatten()),
                unset(row.try_get("wait_event").ok().flatten()),
            )
        })
        .collect())
}

/// Test server handle with analyst, admin, and ingest tokens.
pub struct TestServer {
    pub url: String,
    pub analyst_token: String,
    pub admin_token: String,
    pub reader_token: String,
    pub ingest_token: String,
    pub schema_admin_token: String,
    /// The schema-admin key's stable prefix — the identity the repin
    /// cancel audit events name beside the display name (#109).
    pub schema_admin_prefix: String,
    pub coastwatch_only_token: String,
    /// Pool on the per-test fleet database (mint/revoke keys mid-test).
    pub fleet_pool: PgPool,
    /// DSN of the fleet database (kill it to simulate auth-backend loss).
    pub fleet_db_url: String,
    /// DSN of the sibling trawl app-state database.
    pub app_db_url: String,
    /// The server's shared state — e.g. to reach the hot buffer when a test
    /// drives compaction directly against the server's WAL/data dirs.
    pub state: AppState,
    /// The serve task, retained so the readiness poll can tell "still
    /// starting" from "already dead" (see [`wait_for_ready`]).
    pub serve_task: tokio::task::JoinHandle<()>,
    /// What a second boot over this state needs: TLS paths and drain
    /// budget. Private, because a test that mutated them would be
    /// describing a server this fixture did not build.
    server_config: ServerConfig,
    http_config: trawl_server::state::HttpConfig,
    state_dir: PathBuf,
    /// Printed by the drop guard when the test panics.
    facts: FixtureFacts,
}

/// On a PANICKING unwind only, print the fixture's facts and a census of
/// its postgres backends. A passing test prints nothing: the guard exists
/// so a CI failure carries the state that explains it, not so every run
/// grows a diagnostics tail.
impl Drop for TestServer {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.facts.report();
        }
    }
}

impl TestServer {
    /// Boot a SECOND server over this fixture's state and TLS material, on
    /// an ephemeral port of its own, driven by a shutdown channel the
    /// caller owns.
    ///
    /// The fixture's own server is already parked in `accept()` by the time
    /// a test body runs, so it cannot answer what a signal does when it
    /// lands BEFORE the accept loop first polls. Here the caller decides:
    /// set the flag, then spawn.
    pub fn spawn_server_with_shutdown(
        &self,
        shutdown: trawl_server::shutdown::ShutdownRx,
    ) -> tokio::task::JoinHandle<Result<(), trawl_server::error::ServerError>> {
        let listener =
            TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port for a second server");
        let state = self.state.clone();
        let http_config = self.http_config.clone();
        let server_config = self.server_config.clone();
        let state_dir = self.state_dir.clone();
        tokio::spawn(async move {
            http::serve_with_listener(
                listener,
                state,
                &http_config,
                &server_config,
                &state_dir,
                Some(shutdown),
            )
            .await
        })
    }

    /// Force-drop the fleet keystore database under the running server.
    pub async fn kill_fleet_database(&self) {
        kill_database(&self.fleet_db_url).await;
    }

    /// Force-drop the app-state database under the running server.
    pub async fn kill_app_database(&self) {
        kill_database(&self.app_db_url).await;
    }
}

/// Create the standard role keys in the fleet keystore (roles must already
/// be seeded via [`seed_trawl_roles`]).
/// Returns (analyst, admin, reader, ingest) plaintext tokens.
pub async fn mint_role_keys(store: &KeyStore) -> (String, String, String, String) {
    let analyst = store
        .create_key(
            "test-key",
            PrincipalKind::Service,
            &roles(&["trawl-analyst"]),
            None,
        )
        .await
        .unwrap();
    let admin = store
        .create_key(
            "admin-key",
            PrincipalKind::Service,
            &roles(&["trawl-admin"]),
            None,
        )
        .await
        .unwrap();
    let reader = store
        .create_key(
            "reader-key",
            PrincipalKind::Service,
            &roles(&["trawl-reader"]),
            None,
        )
        .await
        .unwrap();
    let ingest = store
        .create_key(
            "ingest-key",
            PrincipalKind::Service,
            &roles(&["trawl-ingest"]),
            None,
        )
        .await
        .unwrap();
    (
        analyst.plaintext_token.to_string(),
        admin.plaintext_token.to_string(),
        reader.plaintext_token.to_string(),
        ingest.plaintext_token.to_string(),
    )
}

/// Migrate a bare database with the FLEET schema, seed the converted-shape
/// roles, and return a keystore on it. Both callers hand it an empty
/// database: the fixture its own mint, the store-level suites their
/// `#[sqlx::test(migrations = false)]` one.
pub async fn fleet_keystore(pool: &PgPool) -> KeyStore {
    fleet_auth::MIGRATOR
        .run(pool)
        .await
        .expect("apply fleet-auth migrations to per-test database");
    let store = KeyStore::from_pool(pool.clone());
    seed_trawl_roles(&store).await;
    store
}

/// Poll the health endpoint until the server answers, under ONE overall
/// deadline.
///
/// Three properties the pre-listener fixture did not need. First, the
/// fixture binds the socket before the serve task runs, so a connection
/// nothing is accepting yet sits in the kernel backlog instead of being
/// refused: an attempt that hangs must be cut off, or the poll never
/// advances. Second, a serve task that has already finished means the
/// server will never answer (TLS setup failed, or the accept loop
/// returned), so report that instead of spending the whole budget and then
/// blaming readiness. Third, the budget is the WALL CLOCK, not a count of
/// attempts: 100 attempts of up to 500ms each plus sleeps is 51 seconds of
/// worst case behind a doc comment that says "1s".
///
/// [`READY_TIMEOUT`] is 10 seconds because this fixture boots a real
/// trawld: two migrations and the boot conformance pass run before
/// `serve_with_listener` accepts anything, and on a loaded CI runner that
/// is seconds, not milliseconds. It is a ceiling on a hang, not a target;
/// a healthy fixture answers on the first or second attempt.
const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Upper bound on ONE readiness attempt, further clamped by whatever is
/// left of [`READY_TIMEOUT`].
const READY_ATTEMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

pub async fn wait_for_ready(addr: &str, serve_task: &tokio::task::JoinHandle<()>) {
    let poll_client = harness_client_builder()
        .timeout(READY_ATTEMPT_TIMEOUT)
        .build()
        .unwrap();
    let health_url = format!("https://{addr}/api/v1/health");
    let started = std::time::Instant::now();
    let mut attempts = 0_u32;
    loop {
        let Some(remaining) = READY_TIMEOUT.checked_sub(started.elapsed()) else {
            panic!(
                "test server on {addr} was not ready within {READY_TIMEOUT:?} \
                 ({attempts} attempts)"
            );
        };
        attempts += 1;
        let attempt = tokio::time::timeout(
            remaining.min(READY_ATTEMPT_TIMEOUT),
            poll_client.get(&health_url).send(),
        )
        .await;
        if matches!(attempt, Ok(Ok(_))) {
            return;
        }
        assert!(
            !serve_task.is_finished(),
            "the serve task exited before the server answered on {addr}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// Set up a test server with custom rate limiting for rate limit tests.
pub async fn setup_with_rate_limit(rate_limit: RateLimitConfig) -> TestServer {
    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    let server = setup_in_dir(tmp.path(), rate_limit).await;
    // Leak the tempdir so it survives the test (cleaned up by OS).
    std::mem::forget(tmp);
    server
}

/// Set up a test server whose WAL and data root both live under the given
/// directory (which must outlive the server). The data root is this
/// test's own copy of the published parquet seed. Scheduled report
/// output and repin staging therefore land where no other test can see
/// them.
pub async fn setup_in_dir(dir: &std::path::Path, rate_limit: RateLimitConfig) -> TestServer {
    setup_in_dir_with_data(dir, seed_data_root(dir), rate_limit).await
}

/// The derivation policy, resolved the way `main` resolves it.
///
/// Production resolves it before the tracing subscriber (the telemetry
/// layer needs the same policy) and hands it to `AppState::from_config`;
/// the harness resolves the same config the same way, boot-fatally.
fn resolve_derivation(
    config: &Config,
) -> std::sync::Arc<trawl_server::ingest::producer::Derivation> {
    std::sync::Arc::new(
        trawl_server::ingest::producer::Derivation::resolve(&config.ingest)
            .expect("the test config's derivation lists must resolve"),
    )
}

/// Like [`setup_in_dir`], but with an explicit cold-data directory — for tests
/// that compact into a per-test data directory instead of the shared
/// fixtures.
#[allow(clippy::too_many_lines)] // linear assembly: two databases, two pools, one config
pub async fn setup_in_dir_with_data(
    dir: &std::path::Path,
    data_path: String,
    rate_limit: RateLimitConfig,
) -> TestServer {
    setup_in_dir_with_data_and_timeout(dir, data_path, rate_limit, DEFAULT_TEST_TIMEOUT_SECS).await
}

/// The query timeout every fixture uses unless a test needs to reach it.
pub const DEFAULT_TEST_TIMEOUT_SECS: u64 = 10;

/// A fixture whose query timeout a test can shorten.
///
/// Reaching the timeout is the only way to observe a retained permit end
/// to end: the request has to stop waiting while the work is still
/// running (ADR-0024), and ten seconds of it per test is not a bill worth
/// paying.
#[allow(clippy::too_many_lines)] // linear assembly: two databases, two pools, one config
pub async fn setup_in_dir_with_data_and_timeout(
    dir: &std::path::Path,
    data_path: String,
    rate_limit: RateLimitConfig,
    timeout_secs: u64,
) -> TestServer {
    setup_with_ingest_config(
        dir,
        data_path,
        rate_limit,
        timeout_secs,
        true,
        RowCaps::DEFAULT,
        SchedulerConfig::default(),
        None,
    )
    .await
}

/// The `[server]` row caps a fixture boots with.
#[derive(Debug, Clone, Copy)]
pub struct RowCaps {
    pub max_result_rows: usize,
    pub max_export_rows: usize,
}

impl RowCaps {
    /// The caps every fixture uses unless a test needs to reach one.
    pub const DEFAULT: Self = Self {
        max_result_rows: 100_000,
        max_export_rows: 1_000_000,
    };
}

/// A fixture with its own row caps, for tests that must cross one with a
/// handful of events rather than a hundred thousand.
pub async fn setup_with_row_caps(caps: RowCaps) -> TestServer {
    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    let server = setup_with_ingest_config(
        tmp.path(),
        seed_data_root(tmp.path()),
        RateLimitConfig::default(),
        DEFAULT_TEST_TIMEOUT_SECS,
        true,
        caps,
        SchedulerConfig::default(),
        None,
    )
    .await;
    // Leak the tempdir so it survives the test (cleaned up by OS).
    std::mem::forget(tmp);
    server
}

/// Exercise configured WAL presence through real `AppState` construction, without
/// replacing the writer or preconstructing dashboard metadata in the test.
pub async fn setup_in_dir_with_ingest(dir: &std::path::Path, enabled: bool) -> TestServer {
    setup_with_ingest_config(
        dir,
        seed_data_root(dir),
        RateLimitConfig::default(),
        DEFAULT_TEST_TIMEOUT_SECS,
        enabled,
        RowCaps::DEFAULT,
        SchedulerConfig::default(),
        None,
    )
    .await
}

/// A fixture with its own `[scheduler]` section, for tests whose answer
/// depends on a scheduler setting the request path reads (a manual run's
/// catch-up clamp). The fixture never spawns the scheduler loop itself.
pub async fn setup_with_scheduler(scheduler: SchedulerConfig) -> TestServer {
    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    let server = setup_with_ingest_config(
        tmp.path(),
        seed_data_root(tmp.path()),
        RateLimitConfig::default(),
        DEFAULT_TEST_TIMEOUT_SECS,
        true,
        RowCaps::DEFAULT,
        scheduler,
        None,
    )
    .await;
    // Leak the tempdir so it survives the test (cleaned up by OS).
    std::mem::forget(tmp);
    server
}

/// The `[ingest]` hot-buffer caps and compaction interval a fixture boots
/// with, for tests that fill the buffer with a handful of events. The
/// fixture runs no compaction loop, so the interval reaches only what the
/// request path reads from it (the `Retry-After` of a full buffer).
#[derive(Debug, Clone, Copy)]
pub struct HotBufferKnobs {
    pub max_events: usize,
    pub max_bytes: usize,
    pub compaction_interval_secs: u64,
}

/// A fixture with its own hot-buffer caps and compaction interval. The
/// tempdir holding its WAL is leaked so it outlives the server.
pub async fn setup_with_hot_buffer(knobs: HotBufferKnobs) -> TestServer {
    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    let server = setup_with_ingest_config(
        tmp.path(),
        seed_data_root(tmp.path()),
        RateLimitConfig::default(),
        DEFAULT_TEST_TIMEOUT_SECS,
        true,
        RowCaps::DEFAULT,
        SchedulerConfig::default(),
        Some(knobs),
    )
    .await;
    // Leak the tempdir so it survives the test (cleaned up by OS).
    std::mem::forget(tmp);
    server
}

#[allow(clippy::too_many_arguments)] // one knob per fixture variant, all private
#[allow(clippy::too_many_lines)] // linear assembly: two databases, two pools, one config
async fn setup_with_ingest_config(
    dir: &std::path::Path,
    data_path: String,
    rate_limit: RateLimitConfig,
    timeout_secs: u64,
    ingest_enabled: bool,
    row_caps: RowCaps,
    scheduler: SchedulerConfig,
    hot_buffer: Option<HotBufferKnobs>,
) -> TestServer {
    assert!(
        std::path::Path::new(&data_path).is_dir(),
        "the fixture data path must be an existing directory: {data_path}"
    );
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Both databases are the fixture's own (ADR-0021 ruling 2): the server's
    // detached tasks outlive any test-macro teardown, so nothing may drop a
    // database out from under them mid-test.
    let fleet_db_url = create_fleet_database().await;
    let app_db_url = create_app_database().await;

    let fleet = fleet_pool(&fleet_db_url).await;
    let store = fleet_keystore(&fleet).await;

    let (analyst_token, admin_token, reader_token, ingest_token) = mint_role_keys(&store).await;
    let schema_admin = store
        .create_key(
            "schema-admin-key",
            PrincipalKind::Service,
            &roles(&["trawl-schema-admin"]),
            None,
        )
        .await
        .unwrap();
    // Holds a role, but one with zero trawl permissions — the "grantless"
    // shape under roles-as-data.
    let coastwatch_only = store
        .create_key(
            "coastwatch-only",
            PrincipalKind::Service,
            &roles(&["coastwatch-viewer"]),
            None,
        )
        .await
        .unwrap();

    let (cert_path, key_path) = ensure_test_cert();
    // ADR-0021 ruling 1: the port is owned from bind to serve. The fixture
    // holds this listener until `serve_with_listener` adopts it, so nothing
    // can take the port in between.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port for the server");
    let addr = listener
        .local_addr()
        .expect("listener local_addr")
        .to_string();

    let config = Config {
        server: ServerConfig {
            http_addr: addr.clone(),
            timeout_secs,
            max_concurrent_queries: 2,
            max_result_rows: row_caps.max_result_rows,
            max_export_rows: row_caps.max_export_rows,
            max_request_body_bytes: 128 * 1024,
            max_concurrent_requests: 256,
            shutdown_drain_secs: 5,
            log_file: None,
            tls_cert_path: Some(cert_path),
            tls_key_path: Some(key_path),
            tls_reload_interval_secs: 0,
            cors_allowed_origins: vec![],
            schema_cache_ttl_secs: 60,
            max_query_history: 1000,
            max_sse_connections: 32,
            query_log: None,
            query_log_max_bytes: trawl_server::config::DEFAULT_QUERY_LOG_MAX_BYTES,
            rate_limit,
            monitor_refresh_ms: 1000,
        },
        data: DataConfig { path: data_path },
        auth: AuthConfig {
            database_url: Some(fleet_db_url.clone()),
            audit_interval_secs: 0,
        },
        ingest: {
            let wal_dir = dir.join("wal");
            std::fs::create_dir_all(&wal_dir).unwrap();
            let mut ingest = IngestConfig {
                enabled: ingest_enabled,
                wal_dir: Some(wal_dir),
                ..IngestConfig::default()
            };
            if let Some(knobs) = hot_buffer {
                ingest.hot_buffer_max_events = knobs.max_events;
                ingest.hot_buffer_max_bytes = knobs.max_bytes;
                ingest.compaction_interval_secs = knobs.compaction_interval_secs;
            }
            ingest
        },
        retention: RetentionConfig::default(),
        scheduler,
        syslog: SyslogConfig::default(),
        web: WebConfig::default(),
        storage: StorageConfig {
            database_url: Some(app_db_url.clone()),
        },
    };

    // ADR-0021 ruling 3: the server receives pools the fixture built and
    // sized; only production connects for itself. The app store still boots
    // through the real path (advisory lock, then migrate).
    let auth = trawl_server::state::AuthState::from_key_store(KeyStore::from_pool(fleet.clone()));
    let storage = trawl_server::store::StorageState::from_pool(app_pool(&app_db_url).await)
        .await
        .expect("boot the app-state database");
    let (state, http_config) = AppState::from_parts(
        &config,
        test_metrics_handle(),
        resolve_derivation(&config),
        auth,
        storage,
    )
    .await
    .expect("failed to create app state");

    boot_conformance_pass(&state, &config).await;

    // Snapshot collector: makes GET /api/v1/dashboard live in tests, same
    // as trawld's `main()` does in production.
    let _collector = trawl_server::monitor::spawn_snapshot_collector(
        state.clone(),
        addr.clone(),
        config.server.max_sse_connections,
        config.scheduler.enabled,
        false,
    );

    let serve_task = serve_and_wait(listener, &state, &config, http_config.clone(), &addr).await;

    TestServer {
        facts: FixtureFacts::new(&app_db_url, &fleet_db_url, &addr),
        server_config: config.server.clone(),
        http_config,
        state_dir: config.state_dir(),
        url: format!("https://{addr}"),
        analyst_token,
        admin_token,
        reader_token,
        ingest_token,
        schema_admin_token: schema_admin.plaintext_token.to_string(),
        schema_admin_prefix: schema_admin.info.prefix.clone(),
        coastwatch_only_token: coastwatch_only.plaintext_token.to_string(),
        fleet_pool: fleet,
        fleet_db_url,
        app_db_url,
        state,
        serve_task,
    }
}

/// Spawn the HTTPS server task over the fixture's listener and wait for it
/// to answer on `addr`. Returns the task handle so the caller can keep it.
async fn serve_and_wait(
    listener: TcpListener,
    state: &AppState,
    config: &Config,
    http_config: trawl_server::state::HttpConfig,
    addr: &str,
) -> tokio::task::JoinHandle<()> {
    let server_config = config.server.clone();
    let state_dir = config.state_dir();
    let spawned_state = state.clone();
    let task = tokio::spawn(async move {
        http::serve_with_listener(
            listener,
            spawned_state,
            &http_config,
            &server_config,
            &state_dir,
            None,
        )
        .await
        .unwrap();
    });
    wait_for_ready(addr, &task).await;
    task
}

/// Boot conformance pass, same as trawld's `main()` does in production
/// (ingest is enabled in the test config default).
async fn boot_conformance_pass(state: &trawl_server::state::AppState, config: &Config) {
    if config.ingest.enabled {
        trawl_server::catalog::conform::ensure_conformance(
            &state.storage.catalog,
            &state.query.field_catalog,
            &config.data.base_dir(),
            &config.wal_dir(),
            &config.ingest.compaction_memory_limit,
        )
        .await
        .expect("boot conformance pass must succeed");
    }
}

/// Set up a test server whose queries time out after `timeout_secs`.
pub async fn setup_with_query_timeout(
    rate_limit: RateLimitConfig,
    timeout_secs: u64,
) -> TestServer {
    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    let server = setup_in_dir_with_data_and_timeout(
        tmp.path(),
        seed_data_root(tmp.path()),
        rate_limit,
        timeout_secs,
    )
    .await;
    // Leak the tempdir so it survives the test (cleaned up by OS).
    std::mem::forget(tmp);
    server
}

/// Set up a test server with fixtures and return a `TestServer` handle.
///
/// The fixture mints both databases; the caller supplies nothing.
pub async fn setup() -> TestServer {
    setup_with_rate_limit(RateLimitConfig::default()).await
}

/// A tracing capture layer, for the tests that assert on audit events.
///
/// Shared because the events it exists to prove are emitted from several
/// subsystems (the repin engine, the schema handlers) and asserted from
/// several test binaries, each of which is its own process.
pub mod audit_capture {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    /// One captured tracing event's fields, stringified.
    #[derive(Debug, Clone)]
    pub struct Captured {
        pub fields: BTreeMap<String, String>,
    }

    impl Captured {
        /// One field's captured value, or a panic naming the event.
        pub fn field(&self, name: &str) -> String {
            self.fields
                .get(name)
                .unwrap_or_else(|| panic!("{name} missing from {self:?}"))
                .clone()
        }
    }

    /// Capture layer recording every event's fields as strings.
    #[derive(Clone, Default)]
    pub struct Capture {
        events: Arc<Mutex<Vec<Captured>>>,
    }

    impl Capture {
        pub fn events(&self) -> Vec<Captured> {
            self.events.lock().unwrap().clone()
        }

        /// Every captured event whose `event_type` is `name` and whose
        /// `key` field carries `value`.
        ///
        /// The correlation half is not optional. The subscriber this layer
        /// installs is GLOBAL, so under `cargo test` — which runs a
        /// binary's tests as threads in one process, unlike nextest's
        /// process per test — a sibling test's ack or repin lands in the
        /// same buffer and an exact-count assertion fails for reasons that
        /// have nothing to do with the code under test. Every audit event
        /// worth asserting names its subject (a field, an actor, a job), so
        /// filtering on the unique name the test chose costs nothing and
        /// makes the count mean what it says.
        ///
        /// Values are captured through `Debug`, so a string field arrives
        /// quoted: the match is `contains`, not equality.
        pub fn of_type(&self, name: &str, key: &str, value: &str) -> Vec<Captured> {
            self.events()
                .into_iter()
                .filter(|e| {
                    e.fields.get("event_type").is_some_and(|t| t.contains(name))
                        && e.fields.get(key).is_some_and(|v| v.contains(value))
                })
                .collect()
        }
    }

    struct Visitor<'a>(&'a mut BTreeMap<String, String>);

    impl tracing::field::Visit for Visitor<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0.insert(field.name().to_owned(), format!("{value:?}"));
        }
    }

    impl<S> tracing_subscriber::Layer<S> for Capture
    where
        S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = BTreeMap::new();
            event.record(&mut Visitor(&mut fields));
            self.events.lock().unwrap().push(Captured { fields });
        }
    }
}
