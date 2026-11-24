// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared harness for trawld's pg-backed end-to-end tests.
//!
//! The two schemas must live in two databases: sqlx hardwires one
//! `_sqlx_migrations` table per database and both migration sets would
//! collide in it. Two databases also match the production shape.

#![allow(dead_code)] // each test binary uses a subset of these items

pub mod userns;

use std::net::TcpListener;
use std::path::PathBuf;

use fleet_auth::{KeyStore, PrincipalKind, RolePermission};
use sqlx::{Connection as _, Executor as _, PgPool, postgres::PgConnection};

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

/// The prometheus recorder is process-global: `metrics::counter!` always
/// records into whichever recorder was installed first, regardless of
/// which handle a given server renders `/metrics` from. Every harness
/// server in the process therefore renders the same handle.
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

pub fn admin_database_url() -> String {
    std::env::var("DATABASE_URL").expect(
        "DATABASE_URL must be set for the pg-backed suites \
         (e.g. postgres://fleet:fleet@localhost:5433/fleet_test); \
         these tests fail rather than skip",
    )
}

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

const APP_DB_PREFIX: &str = "trawl_app_test";

const FLEET_DB_PREFIX: &str = "trawl_fleet_test";

/// Every prefix the sweeper is allowed to drop. A prefix that mints must
/// appear here or its leftovers accumulate forever.
const SWEPT_PREFIXES: [&str; 2] = [APP_DB_PREFIX, FLEET_DB_PREFIX];

/// The sweep runs inside a test's setup, and every `DROP DATABASE` forces a
/// checkpoint and waits for it. Every mint adds one database and sweeps,
/// and a sweep that wins the lock drops up to this many, so a backlog
/// shrinks whenever more than one mint in four wins it.
const SWEEP_MAX_DROPS: usize = 4;

pub const FLEET_POOL_MAX: u32 = 3;

/// Four requests wait inside `verify_key`'s prefix lookup behind a table
/// lock, each holding a connection, and one more is free for health's
/// keystore ping.
pub const PARKED_AUTH_FLEET_POOL_MAX: u32 = 5;

/// How long that pool waits for a free connection: longer than the
/// request-limit tests' 20-second failure deadline, so a parked window
/// never ends in a pool timeout before the test's own deadline names the
/// hang.
pub const PARKED_AUTH_ACQUIRE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The raw connection a parked-auth test keeps open to hold its table
/// lock. It is not taken from any pool.
pub const TABLE_LOCK_CONNECTIONS: u32 = 1;

pub const APP_POOL_MAX: u32 = 3;

/// Not ours to choose: sqlx builds the `#[sqlx::test]` pool at
/// `max_connections(5)` (`sqlx-postgres/src/testing/mod.rs`).
pub const SQLX_TEST_POOL_MAX: u32 = 5;

/// Charged on top of the app pool's own ceiling: the store acquires it from
/// that pool and then detaches it, so the pool refills the slot and the
/// process holds `APP_POOL_MAX` plus this one.
pub const LOCK_CONNECTIONS: u32 = 1;

/// The short-lived admin connection a mint, a sweep or a kill opens and
/// closes. Charged to the RUN (see `connection_budget`'s headroom), not to
/// a shape: it exists for one statement, not for the test.
pub const ADMIN_TRANSIENT: u32 = 1;

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

pub const PARKED_AUTH_CONNECTION_CEILING: u32 =
    PARKED_AUTH_FLEET_POOL_MAX + APP_POOL_MAX + LOCK_CONNECTIONS + TABLE_LOCK_CONNECTIONS;

pub const WORST_TEST_CONNECTION_CEILING: u32 = max_u32(
    max_u32(
        max_u32(
            FULL_SERVER_CONNECTION_CEILING,
            DIRECT_STORE_CONNECTION_CEILING,
        ),
        max_u32(SQLX_STORE_CONNECTION_CEILING, BOOT_CONNECTION_CEILING),
    ),
    PARKED_AUTH_CONNECTION_CEILING,
);

/// `u32::max` is not const-callable in this MSRV path; this is.
const fn max_u32(a: u32, b: u32) -> u32 {
    if a > b { a } else { b }
}

pub const CI_MAX_CONNECTIONS: u32 = 100;

/// The one sanctioned pool constructor for test code.
pub async fn fixture_pool(dsn: &str, max_connections: u32) -> PgPool {
    fixture_pool_with_acquire_timeout(dsn, max_connections, None).await
}

/// [`fixture_pool`] with an explicit acquire timeout, for a test whose
/// pool has to outwait a hold of its own. `None` is the hang guard
/// ([`trawl_server::test_timing::HANG_GUARD`]), not sqlx's 30 s default:
/// a loaded machine can keep a correct build's connections busy past
/// 30 s, so a shorter bound would fail a correct build with
/// `PoolTimedOut`.
pub async fn fixture_pool_with_acquire_timeout(
    dsn: &str,
    max_connections: u32,
    acquire_timeout: Option<std::time::Duration>,
) -> PgPool {
    let options = sqlx::postgres::PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(acquire_timeout.unwrap_or(trawl_server::test_timing::HANG_GUARD));
    #[allow(clippy::disallowed_methods)]
    let pool = options.connect(dsn).await.expect("fixture pool connect");
    pool
}

pub async fn fleet_pool(dsn: &str) -> PgPool {
    fixture_pool(dsn, FLEET_POOL_MAX).await
}

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

/// The server's pools stay open until process exit, so a test cannot drop
/// its OWN databases; instead each run garbage-collects its predecessors'.
/// A database is only dropped when BOTH guards agree it is abandoned: (a)
/// it belongs to a DIFFERENT nextest run — same-run siblings are
/// structurally never touched, even in the window between their CREATE and
/// the server's first connection — and (b) the owner pid encoded in its
/// name is no longer alive. A single advisory lock elects one sweeper at a
/// time, and DROP without FORCE is a final safety net (live connections
/// make it error harmlessly).
async fn sweep_stale_test_databases() {
    use sqlx::Row as _;

    let Ok(mut admin) = PgConnection::connect(&admin_database_url()).await else {
        return;
    };
    let Ok(got_lock) = sqlx::query_scalar::<_, bool>("SELECT pg_try_advisory_lock(741_852_963)")
        .fetch_one(&mut admin)
        .await
    else {
        return;
    };
    if !got_lock {
        return;
    }

    let mut listed = Vec::new();
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
            listed.push((prefix, name));
        }
    }
    for name in sweep_batch(&listed, &run_marker(), pid_alive) {
        let _ = admin
            .execute(sqlx::AssertSqlSafe(format!(
                r#"DROP DATABASE IF EXISTS "{name}""#
            )))
            .await;
    }
    let _ = sqlx::query("SELECT pg_advisory_unlock(741_852_963)")
        .execute(&mut admin)
        .await;
}

fn sweep_batch<'a>(
    listed: &'a [(&'static str, String)],
    marker: &str,
    mut alive: impl FnMut(u32) -> bool,
) -> Vec<&'a str> {
    listed
        .iter()
        .filter(|(prefix, name)| {
            owner_of(name, prefix).is_some_and(|(owner_marker, owner_pid)| {
                owner_marker != marker && !alive(owner_pid)
            })
        })
        .map(|(_, name)| name.as_str())
        .take(SWEEP_MAX_DROPS)
        .collect()
}

#[test]
fn sweep_guards_parse_own_database_name() {
    // The sweeper's abandoned-db detection must round-trip the naming
    // scheme `create_test_database` uses, for EVERY prefix it mints under
    // — a parse mismatch here silently turns the sweeper into a
    // live-sibling killer.
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

#[test]
fn a_sweep_drops_one_batch_of_a_backlog() {
    // A finished run leaves hundreds of databases behind. Each DROP forces
    // a checkpoint, and dropping them all inside one test's setup runs past
    // nextest's 300 s timeout under load. A sweep takes one batch.
    let backlog: Vec<(&'static str, String)> = (0..3 * SWEEP_MAX_DROPS)
        .map(|n| {
            let prefix = SWEPT_PREFIXES[n % SWEPT_PREFIXES.len()];
            (prefix, format!("{prefix}_finishedrun_4194000_{n:0>12}"))
        })
        .collect();
    let mut asked = 0;
    let batch = sweep_batch(&backlog, &run_marker(), |_| {
        asked += 1;
        false
    });
    let first: Vec<&str> = backlog[..SWEEP_MAX_DROPS]
        .iter()
        .map(|(_, name)| name.as_str())
        .collect();
    assert_eq!(batch, first);
    assert_eq!(
        asked, SWEEP_MAX_DROPS,
        "no owner is checked past a full batch"
    );

    let own = format!("{APP_DB_PREFIX}_{}_4194000_abcdefghijkl", run_marker());
    let live = format!("{FLEET_DB_PREFIX}_otherrun_1_abcdefghijkl");
    let abandoned = format!("{APP_DB_PREFIX}_otherrun_4194000_abcdefghijkl");
    let listed = [
        (APP_DB_PREFIX, own),
        (FLEET_DB_PREFIX, live),
        (APP_DB_PREFIX, format!("{APP_DB_PREFIX}_unparsed")),
        (APP_DB_PREFIX, abandoned.clone()),
    ];
    let mut asked = Vec::new();
    let batch = sweep_batch(&listed, &run_marker(), |pid| {
        asked.push(pid);
        pid == 1
    });
    assert_eq!(batch, [abandoned.as_str()]);
    assert_eq!(
        asked,
        [1, 4_194_000],
        "the own-run name is never pid-checked"
    );
}

fn database_name(url: &str) -> &str {
    url.rsplit('/')
        .next()
        .and_then(|last| last.split('?').next())
        .expect("database name in url")
}

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

/// Kills the dedicated advisory-lock connection *and* the app-state pool's
/// connections; sqlx transparently reconnects the pool (the split-brain
/// hazard), but the raw lock connection cannot, so its session-held
/// advisory lock is released.
pub async fn terminate_backends(url: &str) {
    let name = database_name(url);
    let mut admin = PgConnection::connect(&admin_database_url())
        .await
        .expect("connect to admin DB");
    sqlx::query(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE datname = $1 AND pid <> pg_backend_pid()",
    )
    .bind(name)
    .execute(&mut admin)
    .await
    .expect("terminate app-database backends");
}

static PUBLISH_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();

/// If `dest` exists, there is nothing to do. Otherwise `build` fills a
/// sibling staging directory (a sibling so the rename stays on one
/// filesystem) and one `rename` publishes it whole.
///
/// ACROSS processes the rename decides: the loser gets
/// `EEXIST`/`ENOTEMPTY` and adopts the winner's tree, but only after
/// checking that every entry it staged is present under `dest`. WITHIN
/// one process (plain `cargo test` runs many tests per binary in one
/// process, on many threads) one process-wide mutex serializes
/// publication so the second thread finds `dest` already in place.
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

fn dir_entry_names(dir: &std::path::Path) -> std::io::Result<Vec<std::ffi::OsString>> {
    let mut names = std::fs::read_dir(dir)?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<std::io::Result<Vec<_>>>()?;
    names.sort();
    Ok(names)
}

fn published_tree_is_complete(dest: &std::path::Path, staged: &[std::ffi::OsString]) -> bool {
    let Ok(published) = dir_entry_names(dest) else {
        return false;
    };
    staged.iter().all(|name| published.contains(name))
}

#[test]
fn publish_dir_once_has_one_winner_under_thread_contention() {
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

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

/// The directory name carries a version. Change the seed data and bump
/// `-v1`: a stale tree from an older checkout is then simply a directory
/// nobody looks at, instead of a half-recognised fixture some test has to
/// detect and delete.
fn published_parquet_root() -> PathBuf {
    let dest = fixture_root().join("parquet-v1");
    publish_dir_once(&dest, |staging| {
        // The ADR-0009 on-disk layout the query planner prunes over:
        // `{data}/{env}/{date}/{HH}/{service}.parquet`. Fixtures live
        // there because that is the only shape the server ever writes.
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

/// The seed is COPIED, not hardlinked: `dir` is a tempdir on /tmp, which
/// is a tmpfs here, so a link across from the repo's filesystem is EXDEV.
pub fn seed_data_root(dir: &std::path::Path) -> String {
    let data = dir.join("data");
    copy_tree(&published_parquet_root(), &data).expect("seed the private data root");
    data.to_str().unwrap().to_owned()
}

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

pub fn harness_client_builder() -> reqwest::ClientBuilder {
    let (cert_path, _) = ensure_test_cert();
    let pem = std::fs::read(&cert_path).expect("read the shared test certificate");
    let certificate =
        reqwest::Certificate::from_pem(&pem).expect("parse the shared test certificate");
    reqwest::Client::builder().add_root_certificate(certificate)
}

pub struct RawResponse {
    pub status: u16,
    /// Header names lowercased, in the order the server sent them.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RawResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v.as_str())
    }
}

pub fn assert_request_too_large(resp: &RawResponse, message: &str, what: &str) {
    assert_eq!(resp.status, 413, "{what}: status");
    assert_eq!(
        resp.header("content-type"),
        Some("application/json"),
        "{what}: Content-Type"
    );
    let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap_or_else(|e| {
        panic!(
            "{what}: the body is not JSON ({e}): {}",
            String::from_utf8_lossy(&resp.body)
        )
    });
    assert_eq!(body["error"]["code"], "request_too_large", "{what}: code");
    assert_eq!(body["error"]["message"], message, "{what}: message");
}

/// For requests reqwest cannot send without a race: a body the server
/// refuses before reading all of it. reqwest writes the whole body before
/// it reads, so a server that answers early and closes leaves it with a
/// broken pipe instead of the answer. Here the caller sends only what the
/// server will consume before it answers (a head that announces more body
/// than it sends, or a chunk with no terminator), and nothing is written
/// after the response starts, so no write can fail and no unread byte is
/// left to turn the server's close into a reset.
pub async fn raw_https_exchange(url: &str, request: &[u8]) -> RawResponse {
    raw_https_exchange_within(url, request, std::time::Duration::from_secs(30))
        .await
        .expect("the fixture server answered within 30 s")
}

/// [`raw_https_exchange`] for a request the client never finishes, such as
/// a chunked body begun and never ended: `None` when no whole response
/// arrived within `within`.
///
/// Nothing is written after `request` and the client never half-closes,
/// so for as long as the server waits the body is still open, and an
/// answer within `within` is one the server gave without waiting on the
/// rest of the body. The bound is the assertion: a server that waits on
/// the body never answers.
pub async fn raw_https_exchange_within(
    url: &str,
    request: &[u8],
    within: std::time::Duration,
) -> Option<RawResponse> {
    tokio::time::timeout(within, raw_https_exchange_unbounded(url, request))
        .await
        .ok()
}

async fn raw_https_exchange_unbounded(url: &str, request: &[u8]) -> RawResponse {
    use rustls::pki_types::pem::PemObject as _;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let authority = url
        .strip_prefix("https://")
        .expect("a fixture URL is https://host:port");
    let host = authority
        .rsplit_once(':')
        .expect("a fixture URL carries a port")
        .0;
    let (cert_path, _) = ensure_test_cert();
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from_pem_file(&cert_path).unwrap())
        .unwrap();
    let config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let tcp = tokio::net::TcpStream::connect(authority).await.unwrap();
    let mut tls = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config))
        .connect(
            rustls::pki_types::ServerName::try_from(host.to_owned()).unwrap(),
            tcp,
        )
        .await
        .unwrap();
    tls.write_all(request).await.unwrap();
    tls.flush().await.unwrap();

    let mut buf = Vec::new();
    let head_end = loop {
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break at;
        }
        let mut chunk = [0u8; 4096];
        let n = tls.read(&mut chunk).await.unwrap();
        assert!(n > 0, "the server closed before a full response head");
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = std::str::from_utf8(&buf[..head_end]).expect("an ASCII response head");
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .expect("an HTTP/1.1 status line");
    let headers: Vec<(String, String)> = lines
        .map(|line| {
            let (name, value) = line.split_once(':').expect("a header line");
            (name.trim().to_ascii_lowercase(), value.trim().to_owned())
        })
        .collect();
    let length: usize = headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .map(|(_, v)| v.parse().unwrap())
        .expect("the response carries Content-Length");
    let mut body = buf.split_off(head_end + 4);
    while body.len() < length {
        let mut chunk = [0u8; 4096];
        let n = tls.read(&mut chunk).await.unwrap();
        assert!(n > 0, "the server closed mid-body");
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(length);
    RawResponse {
        status,
        headers,
        body,
    }
}

/// Names and numbers ONLY. There is no DSN and no token in this struct,
/// so no print of it can leak one.
/// Private on purpose: the panic path is the only consumer, and a `pub`
/// field here is a field a test could overwrite with a DSN, which is
/// exactly the leak the struct exists to avoid.
struct FixtureFacts {
    app_db: String,
    fleet_db: String,
    bound_addr: String,
    fleet_pool_max: u32,
    app_pool_max: u32,
    worst_connection_ceiling: u32,
}

impl FixtureFacts {
    fn new(app_db_url: &str, fleet_db_url: &str, bound_addr: &str, fleet_pool_max: u32) -> Self {
        Self {
            app_db: database_name(app_db_url).to_owned(),
            fleet_db: database_name(fleet_db_url).to_owned(),
            bound_addr: bound_addr.to_owned(),
            fleet_pool_max,
            app_pool_max: APP_POOL_MAX,
            worst_connection_ceiling: WORST_TEST_CONNECTION_CEILING,
        }
    }

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

pub struct TestServer {
    pub url: String,
    pub analyst_token: String,
    pub admin_token: String,
    pub reader_token: String,
    pub ingest_token: String,
    pub schema_admin_token: String,
    /// The schema-admin key's stable prefix — the identity the repin
    /// cancel audit events name beside the display name.
    pub schema_admin_prefix: String,
    pub coastwatch_only_token: String,
    pub fleet_pool: PgPool,
    pub fleet_db_url: String,
    pub app_db_url: String,
    pub state: AppState,
    /// The clock captured results' lifecycle reads, fixed at the instant the
    /// fixture was built. `set` moves it for the server too: it is the
    /// clock in `state.results`.
    pub results_clock: trawl_server::results::ResultsClock,
    /// The serve task, retained so the readiness poll can tell "still
    /// starting" from "already dead" (see [`wait_for_ready`]).
    pub serve_task: tokio::task::JoinHandle<()>,
    /// What a second boot over this state needs: TLS paths and drain
    /// budget. Private, because a test that mutated them would be
    /// describing a server this fixture did not build.
    server_config: ServerConfig,
    http_config: trawl_server::state::HttpConfig,
    state_dir: PathBuf,
    facts: FixtureFacts,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.facts.report();
        }
    }
}

impl TestServer {
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

    pub async fn spawn_server_with_timeouts(
        &self,
        timeouts: trawl_server::transport::http::ConnectionTimeouts,
        probe: std::sync::Arc<trawl_server::transport::http::ListenerProbe>,
        shutdown: Option<trawl_server::shutdown::ShutdownRx>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener =
            TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port for a second server");
        let addr = listener
            .local_addr()
            .expect("listener local_addr")
            .to_string();
        let state = self.state.clone();
        let http_config = self.http_config.clone();
        let server_config = self.server_config.clone();
        let state_dir = self.state_dir.clone();
        let task = tokio::spawn(async move {
            http::serve_with_listener_and_timeouts(
                listener,
                timeouts,
                probe,
                state,
                &http_config,
                &server_config,
                &state_dir,
                shutdown,
            )
            .await
            .unwrap();
        });
        wait_for_ready(&addr, &task).await;
        (format!("https://{addr}"), task)
    }

    /// For in-process `oneshot` requests, which must carry a `SocketAddr`
    /// extension where a route reads the peer (ingest does): only the accept
    /// loop inserts one, for every real connection.
    pub fn router_with(
        &self,
        f: impl FnOnce(&mut trawl_server::state::HttpConfig),
    ) -> axum::Router {
        self.router_over(self.state.clone(), f)
    }

    pub fn router_over(
        &self,
        state: AppState,
        f: impl FnOnce(&mut trawl_server::state::HttpConfig),
    ) -> axum::Router {
        let mut http_config = self.http_config.clone();
        f(&mut http_config);
        http::router(state, &http_config)
    }

    pub async fn spawn_server_with(
        &self,
        f: impl FnOnce(&mut trawl_server::state::HttpConfig),
        faults: Vec<std::io::Error>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener =
            TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port for a second server");
        let addr = listener
            .local_addr()
            .expect("listener local_addr")
            .to_string();
        let state = self.state.clone();
        let mut http_config = self.http_config.clone();
        f(&mut http_config);
        let server_config = self.server_config.clone();
        let state_dir = self.state_dir.clone();
        let task = tokio::spawn(async move {
            let served = if faults.is_empty() {
                http::serve_with_listener(
                    listener,
                    state,
                    &http_config,
                    &server_config,
                    &state_dir,
                    None,
                )
                .await
            } else {
                http::serve_with_listener_failing_accepts(
                    listener,
                    faults,
                    state,
                    &http_config,
                    &server_config,
                    &state_dir,
                    None,
                )
                .await
            };
            served.unwrap();
        });
        wait_for_ready(&addr, &task).await;
        (format!("https://{addr}"), task)
    }

    /// trawld serves HTTP/1.1 only (ADR-0054). This listener proves the
    /// router and a client under HTTP/2, not the production edge.
    pub async fn spawn_server_h2_for_tests(&self) -> (String, tokio::task::JoinHandle<()>) {
        let listener =
            TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port for a second server");
        let addr = listener
            .local_addr()
            .expect("listener local_addr")
            .to_string();
        let state = self.state.clone();
        let http_config = self.http_config.clone();
        let server_config = self.server_config.clone();
        let state_dir = self.state_dir.clone();
        let task = tokio::spawn(async move {
            http::serve_with_listener_h2_for_tests(
                listener,
                state,
                &http_config,
                &server_config,
                &state_dir,
                None,
            )
            .await
            .unwrap();
        });
        wait_for_ready(&addr, &task).await;
        (format!("https://{addr}"), task)
    }

    pub async fn kill_fleet_database(&self) {
        kill_database(&self.fleet_db_url).await;
    }

    pub async fn kill_app_database(&self) {
        kill_database(&self.app_db_url).await;
    }
}

/// Roles must already be seeded via [`seed_trawl_roles`].
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

pub struct MintedKey {
    pub token: String,
    /// The key's stable prefix, the identity the wire attributes writes to.
    pub prefix: String,
    pub name: String,
}

pub async fn mint_key(fleet_pool: &PgPool, name: &str, perms: &[&str]) -> MintedKey {
    let store = KeyStore::from_pool(fleet_pool.clone());
    let role = format!("role-{name}");
    store
        .create_role(&role, None, &trawl_perms(perms))
        .await
        .unwrap_or_else(|e| panic!("create role {role}: {e}"));
    let created = store
        .create_key(name, PrincipalKind::Service, &roles(&[&role]), None)
        .await
        .unwrap_or_else(|e| panic!("create key {name}: {e}"));
    MintedKey {
        token: created.plaintext_token.to_string(),
        prefix: created.info.prefix,
        name: created.info.name,
    }
}

pub async fn fleet_keystore(pool: &PgPool) -> KeyStore {
    fleet_auth::MIGRATOR
        .run(pool)
        .await
        .expect("apply fleet-auth migrations to per-test database");
    let store = KeyStore::from_pool(pool.clone());
    seed_trawl_roles(&store).await;
    store
}

/// [`READY_TIMEOUT`] is the hang guard
/// ([`trawl_server::test_timing::HANG_GUARD`]) because this fixture boots a
/// real trawld: two migrations and the boot conformance pass run before
/// `serve_with_listener` accepts anything, and on a loaded machine that
/// can take far longer than on an idle one. It is a ceiling on a hang, not
/// a target.
const READY_TIMEOUT: std::time::Duration = trawl_server::test_timing::HANG_GUARD;

/// How long the poll waits on its pending attempts before it starts one
/// more beside them, doubled each time it does. An attempt that has not
/// answered is never cancelled: a loaded machine can slow a correct
/// exchange past any fixed bound, and only [`READY_TIMEOUT`] ends the poll.
/// The extra attempts are what still make progress when an earlier one
/// sits in the kernel backlog.
const READY_ATTEMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

pub async fn wait_for_ready(addr: &str, serve_task: &tokio::task::JoinHandle<()>) {
    let poll_client = harness_client_builder().build().unwrap();
    let health_url = format!("https://{addr}/api/v1/health");
    let started = std::time::Instant::now();
    let mut attempts = 0_u32;
    let mut pending = tokio::task::JoinSet::new();
    let mut patience = READY_ATTEMPT_TIMEOUT;
    let mut start_attempt = |pending: &mut tokio::task::JoinSet<_>| {
        attempts += 1;
        let request = poll_client.get(&health_url).send();
        pending.spawn(request);
    };
    start_attempt(&mut pending);
    loop {
        let Some(remaining) = READY_TIMEOUT.checked_sub(started.elapsed()) else {
            panic!(
                "test server on {addr} was not ready within {READY_TIMEOUT:?} \
                 ({attempts} attempts)"
            );
        };
        match tokio::time::timeout(remaining.min(patience), pending.join_next()).await {
            Ok(Some(Ok(Ok(_)))) => return,
            Ok(Some(_) | None) => {
                assert!(
                    !serve_task.is_finished(),
                    "the serve task exited before the server answered on {addr}"
                );
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                start_attempt(&mut pending);
            }
            Err(_) => {
                patience = patience.saturating_mul(2);
                start_attempt(&mut pending);
            }
        }
    }
}

pub async fn setup_with_rate_limit(rate_limit: RateLimitConfig) -> TestServer {
    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    let server = setup_in_dir(tmp.path(), rate_limit).await;
    std::mem::forget(tmp);
    server
}

pub async fn setup_in_dir(dir: &std::path::Path, rate_limit: RateLimitConfig) -> TestServer {
    setup_in_dir_with_data(dir, seed_data_root(dir), rate_limit).await
}

fn resolve_derivation(
    config: &Config,
) -> std::sync::Arc<trawl_server::ingest::producer::Derivation> {
    std::sync::Arc::new(
        trawl_server::ingest::producer::Derivation::resolve(&config.ingest)
            .expect("the test config's derivation lists must resolve"),
    )
}

#[allow(clippy::too_many_lines)]
pub async fn setup_in_dir_with_data(
    dir: &std::path::Path,
    data_path: String,
    rate_limit: RateLimitConfig,
) -> TestServer {
    setup_in_dir_with_data_and_timeout(dir, data_path, rate_limit, DEFAULT_TEST_TIMEOUT_SECS).await
}

/// The hang guard, because the deadline is real time and covers a kept
/// result's fsync, which a loaded disk can stall for seconds: a shorter
/// bound would fail a correct build with a 504.
pub const DEFAULT_TEST_TIMEOUT_SECS: u64 = trawl_server::test_timing::HANG_GUARD.as_secs();

#[allow(clippy::too_many_lines)]
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
        BodyLimits::DEFAULT,
        SchedulerConfig::default(),
        None,
        None,
        &[],
        FleetPoolSize::DEFAULT,
        QueryKnobs::DEFAULT,
        trawl_config::CaptureConfig::default(),
        trawl_config::InvestigationConfig::default(),
    )
    .await
}

#[derive(Debug, Clone, Copy)]
pub struct QueryKnobs {
    pub max_concurrent_queries: usize,
    pub spill_max_bytes: std::num::NonZeroU64,
    pub memory_limit: std::num::NonZeroU64,
    pub threads: std::num::NonZeroU32,
}

impl QueryKnobs {
    pub const DEFAULT: Self = Self {
        max_concurrent_queries: 2,
        spill_max_bytes: trawl_server::config::DEFAULT_QUERY_SPILL_MAX_BYTES,
        memory_limit: trawl_server::config::DEFAULT_QUERY_MEMORY_LIMIT,
        threads: trawl_server::config::DEFAULT_QUERY_THREADS,
    };
}

pub async fn setup_with_query_knobs(
    dir: &std::path::Path,
    data_path: String,
    timeout_secs: u64,
    query: QueryKnobs,
) -> TestServer {
    setup_with_ingest_config(
        dir,
        data_path,
        RateLimitConfig::default(),
        timeout_secs,
        true,
        RowCaps::DEFAULT,
        BodyLimits::DEFAULT,
        SchedulerConfig::default(),
        None,
        None,
        &[],
        FleetPoolSize::DEFAULT,
        query,
        trawl_config::CaptureConfig::default(),
        trawl_config::InvestigationConfig::default(),
    )
    .await
}

#[derive(Debug, Clone, Copy)]
pub struct FleetPoolSize {
    pub max_connections: u32,
    /// `None` keeps sqlx's default acquire timeout.
    pub acquire_timeout: Option<std::time::Duration>,
}

impl FleetPoolSize {
    pub const DEFAULT: Self = Self {
        max_connections: FLEET_POOL_MAX,
        acquire_timeout: None,
    };

    pub const PARKED_AUTH: Self = Self {
        max_connections: PARKED_AUTH_FLEET_POOL_MAX,
        acquire_timeout: Some(PARKED_AUTH_ACQUIRE_TIMEOUT),
    };
}

#[derive(Debug, Clone, Copy)]
pub struct RowCaps {
    pub max_result_rows: usize,
    pub max_export_rows: usize,
}

impl RowCaps {
    pub const DEFAULT: Self = Self {
        max_result_rows: 100_000,
        max_export_rows: 1_000_000,
    };
}

pub async fn setup_with_row_caps(caps: RowCaps) -> TestServer {
    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    let server = setup_with_ingest_config(
        tmp.path(),
        seed_data_root(tmp.path()),
        RateLimitConfig::default(),
        DEFAULT_TEST_TIMEOUT_SECS,
        true,
        caps,
        BodyLimits::DEFAULT,
        SchedulerConfig::default(),
        None,
        None,
        &[],
        FleetPoolSize::DEFAULT,
        QueryKnobs::DEFAULT,
        trawl_config::CaptureConfig::default(),
        trawl_config::InvestigationConfig::default(),
    )
    .await;
    std::mem::forget(tmp);
    server
}

#[derive(Debug, Clone, Copy)]
pub struct BodyLimits {
    pub max_request_body_bytes: usize,
    pub ingest_max_body_bytes: usize,
}

impl BodyLimits {
    pub const DEFAULT: Self = Self {
        max_request_body_bytes: 128 * 1024,
        ingest_max_body_bytes: trawl_server::config::DEFAULT_INGEST_MAX_BODY_BYTES,
    };
}

/// A fixture with its own body limits, for tests that must send a whole
/// body past a limit: a body small enough that trawld's first read takes
/// all of it, so the server's close leaves nothing unread.
pub async fn setup_with_body_limits(limits: BodyLimits) -> TestServer {
    setup_with_limits(limits, RateLimitConfig::default()).await
}

pub async fn setup_with_limits(limits: BodyLimits, rate_limit: RateLimitConfig) -> TestServer {
    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    let server = setup_with_ingest_config(
        tmp.path(),
        seed_data_root(tmp.path()),
        rate_limit,
        DEFAULT_TEST_TIMEOUT_SECS,
        true,
        RowCaps::DEFAULT,
        limits,
        SchedulerConfig::default(),
        None,
        None,
        &[],
        FleetPoolSize::DEFAULT,
        QueryKnobs::DEFAULT,
        trawl_config::CaptureConfig::default(),
        trawl_config::InvestigationConfig::default(),
    )
    .await;
    std::mem::forget(tmp);
    server
}

pub async fn setup_in_dir_with_ingest(dir: &std::path::Path, enabled: bool) -> TestServer {
    setup_with_ingest_config(
        dir,
        seed_data_root(dir),
        RateLimitConfig::default(),
        DEFAULT_TEST_TIMEOUT_SECS,
        enabled,
        RowCaps::DEFAULT,
        BodyLimits::DEFAULT,
        SchedulerConfig::default(),
        None,
        None,
        &[],
        FleetPoolSize::DEFAULT,
        QueryKnobs::DEFAULT,
        trawl_config::CaptureConfig::default(),
        trawl_config::InvestigationConfig::default(),
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
        BodyLimits::DEFAULT,
        scheduler,
        None,
        None,
        &[],
        FleetPoolSize::DEFAULT,
        QueryKnobs::DEFAULT,
        trawl_config::CaptureConfig::default(),
        trawl_config::InvestigationConfig::default(),
    )
    .await;
    std::mem::forget(tmp);
    server
}

pub async fn setup_with_trusted_relays(cidrs: &[&str]) -> TestServer {
    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    let server = setup_with_ingest_config(
        tmp.path(),
        seed_data_root(tmp.path()),
        RateLimitConfig::default(),
        DEFAULT_TEST_TIMEOUT_SECS,
        true,
        RowCaps::DEFAULT,
        BodyLimits::DEFAULT,
        SchedulerConfig::default(),
        None,
        None,
        cidrs,
        FleetPoolSize::DEFAULT,
        QueryKnobs::DEFAULT,
        trawl_config::CaptureConfig::default(),
        trawl_config::InvestigationConfig::default(),
    )
    .await;
    std::mem::forget(tmp);
    server
}

/// The `[ingest]` hot-buffer caps and compaction interval a fixture boots
/// with, for tests that fill the buffer with a handful of events. The
/// fixture runs no compaction loop, so the interval reaches only what the
/// request path reads from it (the `Retry-After` of a full buffer), unless
/// the test spawns a loop of its own.
#[derive(Debug, Clone, Copy)]
pub struct HotBufferKnobs {
    pub max_events: usize,
    pub max_bytes: usize,
    pub compaction_interval_secs: u64,
}

pub async fn setup_with_hot_buffer(knobs: HotBufferKnobs) -> TestServer {
    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    let server = setup_with_hot_buffer_in(tmp.path(), knobs).await;
    std::mem::forget(tmp);
    server
}

/// [`setup_with_hot_buffer`] in a caller-owned `dir`, for a test that runs
/// its own compaction loop and so needs the WAL (`dir/wal`) and data
/// (`dir/data`) paths.
pub async fn setup_with_hot_buffer_in(dir: &std::path::Path, knobs: HotBufferKnobs) -> TestServer {
    setup_with_ingest_config(
        dir,
        seed_data_root(dir),
        RateLimitConfig::default(),
        DEFAULT_TEST_TIMEOUT_SECS,
        true,
        RowCaps::DEFAULT,
        BodyLimits::DEFAULT,
        SchedulerConfig::default(),
        Some(knobs),
        None,
        &[],
        FleetPoolSize::DEFAULT,
        QueryKnobs::DEFAULT,
        trawl_config::CaptureConfig::default(),
        trawl_config::InvestigationConfig::default(),
    )
    .await
}

pub async fn setup_observing_boot(
    dir: &std::path::Path,
    data_path: String,
    knobs: Option<HotBufferKnobs>,
    before_boot: &mut dyn FnMut(&AppState),
) -> TestServer {
    setup_with_ingest_config(
        dir,
        data_path,
        RateLimitConfig::default(),
        DEFAULT_TEST_TIMEOUT_SECS,
        true,
        RowCaps::DEFAULT,
        BodyLimits::DEFAULT,
        SchedulerConfig::default(),
        knobs,
        Some(before_boot),
        &[],
        FleetPoolSize::DEFAULT,
        QueryKnobs::DEFAULT,
        trawl_config::CaptureConfig::default(),
        trawl_config::InvestigationConfig::default(),
    )
    .await
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
    body_limits: BodyLimits,
    scheduler: SchedulerConfig,
    hot_buffer: Option<HotBufferKnobs>,
    before_boot: Option<&mut dyn FnMut(&AppState)>,
    trusted_relays: &[&str],
    fleet_pool_size: FleetPoolSize,
    query: QueryKnobs,
    capture: trawl_config::CaptureConfig,
    investigation: trawl_config::InvestigationConfig,
) -> TestServer {
    assert!(
        std::path::Path::new(&data_path).is_dir(),
        "the fixture data path must be an existing directory: {data_path}"
    );
    let _ = rustls::crypto::ring::default_provider().install_default();

    let fleet_db_url = create_fleet_database().await;
    let app_db_url = create_app_database().await;

    let fleet = fixture_pool_with_acquire_timeout(
        &fleet_db_url,
        fleet_pool_size.max_connections,
        fleet_pool_size.acquire_timeout,
    )
    .await;
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
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port for the server");
    let addr = listener
        .local_addr()
        .expect("listener local_addr")
        .to_string();

    let config = Config {
        server: ServerConfig {
            http_addr: addr.clone(),
            timeout_secs,
            max_concurrent_queries: query.max_concurrent_queries,
            max_result_rows: row_caps.max_result_rows,
            max_export_rows: row_caps.max_export_rows,
            max_request_body_bytes: body_limits.max_request_body_bytes,
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
            query_scratch_dir: None,
            results_dir: None,
            link_origin: None,
            query_spill_max_bytes: query.spill_max_bytes,
            query_memory_limit: query.memory_limit,
            query_threads: query.threads,
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
                trusted_relays: trusted_relays.iter().map(|c| (*c).to_owned()).collect(),
                max_body_bytes: body_limits.ingest_max_body_bytes,
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
        capture,
        investigation,
    };

    let auth = trawl_server::state::AuthState::from_key_store(KeyStore::from_pool(fleet.clone()));
    let storage = trawl_server::store::StorageState::from_pool(app_pool(&app_db_url).await)
        .await
        .expect("boot the app-state database");
    let scratch = trawl_server::state::open_query_scratch(&config).expect("lock the query scratch");
    let results_root =
        trawl_server::state::open_results_root(&config).expect("hold the results root");
    // Captured results' lifecycle reads a clock the test moves; it starts at
    // the wall-clock instant the fixture was built.
    let results_clock = trawl_server::results::ResultsClock::fixed(chrono::Utc::now());
    let (state, http_config) = AppState::from_parts(
        &config,
        test_metrics_handle(),
        resolve_derivation(&config),
        auth,
        storage,
        scratch,
        results_root,
        results_clock.clone(),
    )
    .await
    .expect("failed to create app state");

    if let Some(observe) = before_boot {
        observe(&state);
    }
    prepare_corpus(&state, &config).await;

    let _collector = trawl_server::monitor::spawn_snapshot_collector(
        state.clone(),
        addr.clone(),
        config.server.max_sse_connections,
        config.scheduler.enabled,
        false,
    );

    let serve_task = serve_and_wait(listener, &state, &config, http_config.clone(), &addr).await;

    TestServer {
        facts: FixtureFacts::new(
            &app_db_url,
            &fleet_db_url,
            &addr,
            fleet_pool_size.max_connections,
        ),
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
        results_clock,
        serve_task,
    }
}

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
    // The connection deadlines are the hang guard, not production's 10 s
    // handshake and 30 s header read: a loaded machine can starve a correct
    // connection task past those, and the fixture would reset a valid
    // request. Tests of the deadlines themselves boot their own server
    // through `spawn_server_with_timeouts`.
    let timeouts = http::ConnectionTimeouts {
        tls_handshake: trawl_server::test_timing::HANG_GUARD,
        header_read: trawl_server::test_timing::HANG_GUARD,
    };
    let task = tokio::spawn(async move {
        http::serve_with_listener_and_timeouts(
            listener,
            timeouts,
            std::sync::Arc::new(http::ListenerProbe::default()),
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

/// The fixture never spawns compaction, so overhang the boot leaves stays
/// until the test drives compaction itself.
async fn prepare_corpus(state: &AppState, config: &Config) {
    trawl_server::boot::prepare_corpus(state, config)
        .await
        .expect("the boot corpus preparation must succeed");
}

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
    std::mem::forget(tmp);
    server
}

/// A fixture whose fleet-keystore pool is built as `size`, for a test that
/// parks bearer verifications in Postgres and still needs a free
/// connection beside them. With [`FleetPoolSize::PARKED_AUTH`] and the
/// test's table-lock connection, the shape is
/// [`PARKED_AUTH_CONNECTION_CEILING`].
pub async fn setup_with_fleet_pool(size: FleetPoolSize) -> TestServer {
    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    let server = setup_with_ingest_config(
        tmp.path(),
        seed_data_root(tmp.path()),
        RateLimitConfig::default(),
        DEFAULT_TEST_TIMEOUT_SECS,
        true,
        RowCaps::DEFAULT,
        BodyLimits::DEFAULT,
        SchedulerConfig::default(),
        None,
        None,
        &[],
        size,
        QueryKnobs::DEFAULT,
        trawl_config::CaptureConfig::default(),
        trawl_config::InvestigationConfig::default(),
    )
    .await;
    std::mem::forget(tmp);
    server
}

/// `dir` holds the WAL and stays the caller's, so a test can seed the data
/// root before boot.
pub async fn setup_with_capture(
    dir: &std::path::Path,
    data_path: String,
    rate_limit: RateLimitConfig,
    capture: trawl_config::CaptureConfig,
) -> TestServer {
    setup_with_capture_timeout(
        dir,
        data_path,
        rate_limit,
        capture,
        DEFAULT_TEST_TIMEOUT_SECS,
    )
    .await
}

pub async fn setup_with_capture_timeout(
    dir: &std::path::Path,
    data_path: String,
    rate_limit: RateLimitConfig,
    capture: trawl_config::CaptureConfig,
    timeout_secs: u64,
) -> TestServer {
    setup_with_evidence(
        dir,
        data_path,
        capture,
        trawl_config::InvestigationConfig::default(),
        rate_limit,
        timeout_secs,
    )
    .await
}

pub async fn setup_with_evidence(
    dir: &std::path::Path,
    data_path: String,
    capture: trawl_config::CaptureConfig,
    investigation: trawl_config::InvestigationConfig,
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
        BodyLimits::DEFAULT,
        SchedulerConfig::default(),
        None,
        None,
        &[],
        FleetPoolSize::DEFAULT,
        QueryKnobs::DEFAULT,
        capture,
        investigation,
    )
    .await
}

/// Captured results through the fixture's HTTPS listener (ADR-0057).
pub mod captured {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use chrono::{DateTime, Utc};
    use reqwest::StatusCode;
    use reqwest::header::HeaderMap;
    use trawl_api::results::CaptureResponse;

    use super::TestServer;

    pub type Answer = (StatusCode, HeaderMap, serde_json::Value);

    fn client() -> reqwest::Client {
        super::harness_client_builder().build().unwrap()
    }

    async fn answer(request: reqwest::RequestBuilder) -> Answer {
        let response = request.send().await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let text = response.text().await.unwrap();
        let body = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("{status}: the body is not JSON ({e}): {text}"));
        (status, headers, body)
    }

    pub async fn capture(server: &TestServer, token: &str, body: &serde_json::Value) -> Answer {
        answer(
            client()
                .post(format!("{}/api/v1/results", server.url))
                .bearer_auth(token)
                .json(body),
        )
        .await
    }

    pub async fn captured(
        server: &TestServer,
        token: &str,
        body: &serde_json::Value,
    ) -> CaptureResponse {
        let (status, _, answer) = capture(server, token, body).await;
        assert_eq!(status, StatusCode::OK, "{body}: {answer}");
        serde_json::from_value(answer).expect("a capture response")
    }

    pub async fn get(server: &TestServer, token: &str, path: &str) -> Answer {
        answer(
            client()
                .get(format!("{}/api/v1{path}", server.url))
                .bearer_auth(token),
        )
        .await
    }

    pub async fn page(server: &TestServer, token: &str, id: &str, query: &str) -> Answer {
        get(server, token, &format!("/results/{id}{query}")).await
    }

    pub async fn cell(
        server: &TestServer,
        token: &str,
        id: &str,
        row: u64,
        column: usize,
        query: &str,
    ) -> Answer {
        get(
            server,
            token,
            &format!("/results/{id}/cells/{row}/{column}{query}"),
        )
        .await
    }

    /// Display names are not unique (ADR-0057), but no query makes two
    /// columns of one name: `table` and `fields` list a repeated name once,
    /// `pivot` gets `host_1` from `DuckDB` for a value that matches a row
    /// key, and every other stage refuses a duplicate target. This edits
    /// the recorded `columns` only. The stored file names its columns by
    /// position (`c0`, `c1`, ...), so its bytes and digest still hold.
    pub async fn rename_column(
        server: &TestServer,
        token: &str,
        result: &trawl_api::results::CapturedResult,
        position: usize,
        name: &str,
    ) -> trawl_api::results::CapturedResult {
        use sqlx::Connection as _;

        let mut columns = result.columns.clone();
        name.clone_into(&mut columns[position].name);
        let mut conn = sqlx::postgres::PgConnection::connect(&server.app_db_url)
            .await
            .unwrap();
        let updated = sqlx::query("UPDATE captured_results SET columns = $2 WHERE id = $1")
            .bind(&result.id)
            .bind(serde_json::to_string(&columns).unwrap())
            .execute(&mut conn)
            .await
            .unwrap();
        assert_eq!(updated.rows_affected(), 1, "capture {}", result.id);
        conn.close().await.unwrap();

        let (status, _, body) = page(server, token, &result.id, "").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let page: trawl_api::results::ResultPage =
            serde_json::from_value(body).expect("a result page");
        assert_eq!(page.result.columns, columns, "the recorded names");
        page.result
    }

    pub async fn assert_own_cells(
        server: &TestServer,
        token: &str,
        result: &trawl_api::results::CapturedResult,
        stored: &trawl_api::results::ResultPage,
        name: &str,
    ) {
        let shared: Vec<usize> = result
            .columns
            .iter()
            .enumerate()
            .filter(|(_, column)| column.name == name)
            .map(|(position, _)| position)
            .collect();
        assert!(
            shared.len() >= 2,
            "`{name}` is shared: {:?}",
            result.columns
        );
        assert!(!stored.page.rows.is_empty(), "the capture holds rows");
        let null = trawl_api::value::Value::Null;
        let mut told_apart = false;
        for row in &stored.page.rows {
            for (i, &p) in shared.iter().enumerate() {
                for &q in &shared[i + 1..] {
                    if row[p] == null && row[q] == null {
                        continue;
                    }
                    assert_ne!(row[p], row[q], "positions {p} and {q} differ: {row:?}");
                    told_apart = true;
                }
            }
        }
        assert!(told_apart, "a stored row holds a `{name}` value");
        for query in [String::new(), format!("?columns={name}")] {
            let (status, _, body) = page(server, token, &result.id, &query).await;
            assert_eq!(status, StatusCode::OK, "{query}: {body}");
            let read: trawl_api::results::ResultPage =
                serde_json::from_value(body).expect("a result page");
            if query.is_empty() {
                // `own` below reads the positions the page claims, so a
                // page that dropped a shared position would agree with it.
                assert_eq!(
                    read.positions,
                    (0..result.columns.len()).collect::<Vec<_>>(),
                    "a full page keeps every position"
                );
            } else {
                assert_eq!(read.positions, shared, "{query}");
            }
            let own: Vec<Vec<_>> = stored
                .page
                .rows
                .iter()
                .map(|row| read.positions.iter().map(|&p| row[p].clone()).collect())
                .collect();
            assert_eq!(read.page.rows, own, "{query}: each position's own cells");
        }
    }

    pub async fn listed(server: &TestServer, token: &str) -> Vec<String> {
        let (status, _, body) = get(server, token, "/results").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let list: trawl_api::results::ResultList =
            serde_json::from_value(body).expect("a result list");
        list.results.into_iter().map(|r| r.id).collect()
    }

    pub struct Event {
        pub time: DateTime<Utc>,
        pub host: String,
        pub message: String,
    }

    pub fn seed_hour(data: &str, service: &str, events: &[Event]) -> PathBuf {
        let first = events.first().expect("a corpus has events").time;
        let hour = first.format("%Y-%m-%d/%H").to_string();
        assert!(
            events
                .iter()
                .all(|e| e.time.format("%Y-%m-%d/%H").to_string() == hour),
            "every event falls in {hour}"
        );
        seed_partition(data, &format!("prod/{hour}"), service, events)
    }

    /// Writes `events` whatever their times: compaction names a partition by
    /// when it wrote it, not by `_time`, so this is the file a sender whose
    /// clock was off leaves behind.
    pub fn seed_partition(data: &str, partition: &str, service: &str, events: &[Event]) -> PathBuf {
        let dir = Path::new(data).join(partition);
        std::fs::create_dir_all(&dir).unwrap();
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
        .unwrap();
        let micros = |at: DateTime<Utc>| at.format("%Y-%m-%d %H:%M:%S%.6f").to_string();
        let mut insert = conn
            .prepare(
                "INSERT INTO logs VALUES
                 (CAST(? AS TIMESTAMP), CAST(? AS TIMESTAMP), ?, NULL, 'prod', ?, ?, 9, 'info', ?)",
            )
            .unwrap();
        for event in events {
            insert
                .execute(duckdb::params![
                    micros(event.time),
                    micros(event.time + chrono::TimeDelta::seconds(10)),
                    event.message,
                    service,
                    event.host,
                    event.message,
                ])
                .unwrap();
        }
        let path = dir.join(format!("{service}.parquet"));
        conn.execute_batch(&format!(
            "COPY logs TO '{}' (FORMAT PARQUET)",
            path.display()
        ))
        .unwrap();
        path
    }

    #[derive(Debug, Clone)]
    pub struct Row {
        pub time: DateTime<Utc>,
        pub ingested_after: chrono::TimeDelta,
        pub env: String,
        pub service: String,
        pub host: Option<String>,
        pub message: Option<String>,
        pub raw: String,
        /// Extra fields, by name; a field a row does not name is null.
        pub fields: Vec<(String, Cell)>,
    }

    /// An extra field's value. A file's column takes the type of the first
    /// row that names it.
    #[derive(Debug, Clone, PartialEq)]
    pub enum Cell {
        Text(String),
        Boolean(bool),
        Float(f64),
    }

    impl Cell {
        fn sql_type(&self) -> &'static str {
            match self {
                Self::Text(_) => "VARCHAR",
                Self::Boolean(_) => "BOOLEAN",
                Self::Float(_) => "DOUBLE",
            }
        }

        fn param(&self) -> String {
            match self {
                Self::Text(text) => text.clone(),
                Self::Boolean(b) => b.to_string(),
                Self::Float(f) => f.to_string(),
            }
        }
    }

    impl Row {
        pub fn new(
            time: DateTime<Utc>,
            env: &str,
            service: &str,
            host: &str,
            message: &str,
        ) -> Self {
            Self {
                time,
                ingested_after: chrono::TimeDelta::seconds(10),
                env: env.to_owned(),
                service: service.to_owned(),
                host: Some(host.to_owned()),
                message: Some(message.to_owned()),
                raw: message.to_owned(),
                fields: Vec::new(),
            }
        }

        #[must_use]
        pub fn raw(mut self, raw: &str) -> Self {
            raw.clone_into(&mut self.raw);
            self
        }

        #[must_use]
        pub fn field(mut self, name: &str, value: &str) -> Self {
            self.fields
                .push((name.to_owned(), Cell::Text(value.to_owned())));
            self
        }

        #[must_use]
        pub fn boolean_field(mut self, name: &str, value: bool) -> Self {
            self.fields.push((name.to_owned(), Cell::Boolean(value)));
            self
        }

        #[must_use]
        pub fn float_field(mut self, name: &str, value: f64) -> Self {
            self.fields.push((name.to_owned(), Cell::Float(value)));
            self
        }

        #[must_use]
        pub fn ingested_after(mut self, after: chrono::TimeDelta) -> Self {
            self.ingested_after = after;
            self
        }
    }

    /// Seed before the fixture boots.
    pub fn seed_rows(data: &str, rows: &[Row]) {
        let mut files: std::collections::BTreeMap<(String, String, String), Vec<&Row>> =
            std::collections::BTreeMap::new();
        for row in rows {
            let hour = row.time.format("%Y-%m-%d/%H").to_string();
            files
                .entry((row.env.clone(), hour, row.service.clone()))
                .or_default()
                .push(row);
        }
        for ((env, hour, service), rows) in files {
            write_rows(data, &format!("{env}/{hour}"), &service, &rows);
        }
    }

    pub fn seed_file(data: &str, partition: &str, file: &str, rows: &[Row]) {
        let rows: Vec<&Row> = rows.iter().collect();
        write_rows(data, partition, file, &rows);
    }

    fn write_rows(data: &str, partition: &str, service: &str, rows: &[&Row]) {
        use std::fmt::Write as _;
        let dir = Path::new(data).join(partition);
        std::fs::create_dir_all(&dir).unwrap();
        let mut extra: Vec<(String, &'static str)> = Vec::new();
        for row in rows {
            for (name, cell) in &row.fields {
                if !extra.iter().any(|(known, _)| known == name) {
                    extra.push((name.clone(), cell.sql_type()));
                }
            }
        }
        let conn = duckdb::Connection::open_in_memory().expect("open duckdb");
        let mut extra_columns = String::new();
        let mut placeholders = String::new();
        for (name, ty) in &extra {
            let _ = write!(extra_columns, ", \"{}\" {ty}", name.replace('"', "\"\""));
            let _ = write!(placeholders, ", CAST(? AS {ty})");
        }
        conn.execute_batch(&format!(
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
                message VARCHAR{extra_columns}
            )"
        ))
        .unwrap();
        let micros = |at: DateTime<Utc>| at.format("%Y-%m-%d %H:%M:%S%.6f").to_string();
        let mut insert = conn
            .prepare(&format!(
                "INSERT INTO logs VALUES
                 (CAST(? AS TIMESTAMP), CAST(? AS TIMESTAMP), ?, NULL, ?, ?, ?, 9, 'info', ?{placeholders})"
            ))
            .unwrap();
        conn.execute_batch("BEGIN").unwrap();
        for row in rows {
            let mut params: Vec<Option<String>> = vec![
                Some(micros(row.time)),
                Some(micros(row.time + row.ingested_after)),
                Some(row.raw.clone()),
                Some(row.env.clone()),
                Some(row.service.clone()),
                row.host.clone(),
                row.message.clone(),
            ];
            for (name, _) in &extra {
                params.push(
                    row.fields
                        .iter()
                        .find(|(field, _)| field == name)
                        .map(|(_, cell)| cell.param()),
                );
            }
            insert
                .execute(duckdb::params_from_iter(params.iter()))
                .unwrap();
        }
        conn.execute_batch("COMMIT").unwrap();
        let path = dir.join(format!("{service}.parquet"));
        conn.execute_batch(&format!(
            "COPY logs TO '{}' (FORMAT PARQUET)",
            path.display()
        ))
        .unwrap();
    }

    /// The tempdir is leaked so it outlives the server.
    pub async fn boot_seeded(
        capture: trawl_config::CaptureConfig,
        seed: impl FnOnce(&str),
    ) -> TestServer {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let data = super::seed_data_root(tmp.path());
        seed(&data);
        let server = super::setup_with_capture_timeout(
            tmp.path(),
            data,
            super::RateLimitConfig::default(),
            capture,
            super::DEFAULT_TEST_TIMEOUT_SECS,
        )
        .await;
        std::mem::forget(tmp);
        server
    }

    pub async fn correlate(
        server: &TestServer,
        token: &str,
        read: &str,
        body: &serde_json::Value,
    ) -> Answer {
        answer(
            client()
                .post(format!("{}/api/v1/results/{read}", server.url))
                .bearer_auth(token)
                .json(body),
        )
        .await
    }

    pub async fn correlated(
        server: &TestServer,
        token: &str,
        read: &str,
        body: &serde_json::Value,
    ) -> trawl_api::results::CorrelationResponse {
        let (status, _, answer) = correlate(server, token, read, body).await;
        assert_eq!(status, StatusCode::OK, "{read} {body}: {answer}");
        serde_json::from_value(answer).expect("a correlation response")
    }

    /// The service of the [`corpus`] events, which no shared seed row has.
    pub const SERVICE: &str = "capturecorpus";

    pub const ROWS: usize = 20;

    pub const WIDE_SEQ: usize = 7;

    pub const WIDE_MESSAGE: &str = "wide ✓ ünïcødé ☕🐟 end";

    pub fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    pub fn corpus() -> Vec<Event> {
        let start = at("2024-01-16T10:00:00Z");
        (0..ROWS)
            .map(|seq| Event {
                time: start + chrono::TimeDelta::seconds(i64::try_from(seq).unwrap()),
                host: if seq % 2 == 0 { "web01" } else { "web02" }.to_owned(),
                message: if seq == WIDE_SEQ {
                    WIDE_MESSAGE.to_owned()
                } else {
                    format!("event {seq}")
                },
            })
            .collect()
    }

    /// The service of the [`slow_corpus`], which no other seed row has.
    pub const SLOW_SERVICE: &str = "slowcorpus";

    /// Events in the [`slow_corpus`]: one Parquet row group, so one thread
    /// scans all of it.
    pub const SLOW_ROWS: u32 = 32_768;

    /// A regex no [`slow_corpus`] message matches. On hex text its run of 60
    /// exhausts RE2's DFA state cache, and the search costs about a
    /// microsecond a byte: seconds of execution over a few megabytes.
    pub const SLOW_PATTERN: &str = "[0-9a][0-9a-f]{60}z";

    /// How long a mid-execution leg waits past `Started` before it drops the
    /// client: long past the bind of one file, and a quarter or less of the
    /// slow query's execution, which its control holds to.
    pub const INTO_EXECUTE: Duration = Duration::from_millis(500);

    pub fn slow_corpus(data: &str) {
        let hour = Path::new(data).join("prod/2024-01-16/10");
        std::fs::create_dir_all(&hour).expect("create the corpus hour");
        let conn = duckdb::Connection::open_in_memory().expect("open duckdb");
        conn.execute_batch(&format!(
            "COPY (SELECT TIMESTAMP '2024-01-16 10:00:00' + to_microseconds(i) AS _time, \
                    TIMESTAMP '2024-01-16 10:00:10' + to_microseconds(i) AS _ingested, \
                    NULL::VARCHAR AS _raw, NULL::VARCHAR AS _repairs, 'prod' AS env, \
                    '{SLOW_SERVICE}' AS service, 'web01' AS host, 9::BIGINT AS severity, \
                    'info' AS severity_text, \
                    md5(i::VARCHAR) || md5((i * 7)::VARCHAR) || md5((i * 11)::VARCHAR) \
                        || md5((i * 13)::VARCHAR) AS message \
             FROM range({SLOW_ROWS}) t(i)) TO '{}' (FORMAT PARQUET)",
            hour.join(format!("{SLOW_SERVICE}.parquet")).display()
        ))
        .expect("write the slow corpus");
    }

    pub async fn boot(
        rate_limit: super::RateLimitConfig,
        capture: trawl_config::CaptureConfig,
    ) -> TestServer {
        boot_with_data(rate_limit, capture).await.0
    }

    pub async fn boot_with_data(
        rate_limit: super::RateLimitConfig,
        capture: trawl_config::CaptureConfig,
    ) -> (TestServer, String) {
        boot_with_data_timeout(rate_limit, capture, super::DEFAULT_TEST_TIMEOUT_SECS).await
    }

    /// [`boot_with_data`] whose queries time out after `timeout_secs`, which
    /// also caps every capture's deadline.
    pub async fn boot_with_data_timeout(
        rate_limit: super::RateLimitConfig,
        capture: trawl_config::CaptureConfig,
        timeout_secs: u64,
    ) -> (TestServer, String) {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let data = super::seed_data_root(tmp.path());
        seed_hour(&data, SERVICE, &corpus());
        let server = super::setup_with_capture_timeout(
            tmp.path(),
            data.clone(),
            rate_limit,
            capture,
            timeout_secs,
        )
        .await;
        std::mem::forget(tmp);
        (server, data)
    }

    pub async fn assert_nothing_held(server: &TestServer, permits: usize, what: &str) {
        let pool = &server.state.query.pool;
        assert_eq!(pool.available_permits(), permits, "{what}: no permit held");
        let writer = tokio::time::timeout(Duration::from_secs(5), pool.publication().write()).await;
        assert!(
            writer.is_ok(),
            "{what}: a publication writer acquires, so no reader guard is held"
        );
    }

    pub async fn key_id(server: &TestServer, token: &str) -> i64 {
        fleet_auth::KeyStore::from_pool(server.fleet_pool.clone())
            .verify_key(token)
            .await
            .expect("a fixture token verifies")
            .id
    }

    pub fn root_names(server: &TestServer) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(server.state.results.root.path())
            .expect("read the results root")
            .map(|entry| {
                entry
                    .expect("a root entry")
                    .file_name()
                    .into_string()
                    .expect("a UTF-8 name")
            })
            .collect();
        names.sort();
        names
    }

    /// Where a [`Restartable`] fixture keeps what outlives one boot: the
    /// directory holding its WAL (`dir/wal`) and its data root, beside which
    /// the results and scratch roots sit.
    pub struct Disk {
        pub dir: PathBuf,
        pub data: String,
    }

    impl Disk {
        pub fn wal(&self) -> PathBuf {
            self.dir.join("wal")
        }
    }

    /// Accept `events` through `/api/v1/ingest` as the ingest key: into the
    /// WAL and the hot buffer, not yet compacted.
    pub async fn ingest(server: &TestServer, events: &[serde_json::Value]) {
        let client = trawl_client::HttpClient::new_insecure(&server.url, &server.ingest_token)
            .expect("an ingest client");
        let answer = client.ingest(events).await.expect("the ingest succeeds");
        assert_eq!(answer.accepted, events.len(), "{answer:?}");
    }

    /// One production compaction pass over the fixture's WAL into its data
    /// root, draining its hot buffer and pinning through its catalog, as
    /// trawld's loop runs it. Takes the publication write guard while it
    /// publishes, as every compaction does.
    pub async fn compact(server: &TestServer, disk: &Disk) {
        let state = &server.state;
        let ingest = trawl_config::IngestConfig::default();
        let catalog = trawl_server::catalog::CatalogContext {
            store: state.storage.catalog.clone(),
            cache: state.query.field_catalog.clone(),
        };
        let failures = trawl_server::ingest::compaction::compact_once(
            &disk.wal(),
            Path::new(&disk.data),
            Duration::ZERO,
            false,
            state.query.hot_buffer.as_ref(),
            ingest.compaction_chunk_size,
            &ingest.compaction_memory_limit,
            Some(&catalog),
        )
        .await
        .expect("the compaction pass runs");
        assert_eq!(failures, 0, "the compaction pass lost nothing");
    }

    struct Keys {
        analyst: String,
        admin: String,
        reader: String,
        ingest: String,
        schema_admin: String,
        schema_admin_prefix: String,
        coastwatch_only: String,
    }

    /// A fixture that can be restarted in process: dropped whole, then
    /// booted again over the same WAL, data root, results root, query
    /// scratch and both databases, as a trawld restart finds them.
    ///
    /// Unlike [`super::setup_with_capture`], a boot here spawns no dashboard
    /// collector. That detached task holds a clone of the state for the life
    /// of the process, and with it the results root's lock, the scratch
    /// lock and the app-state advisory lock, so no second boot could take
    /// them. No captured-results test reads the dashboard.
    pub struct Restartable {
        pub disk: Disk,
        pub rate_limit: super::RateLimitConfig,
        pub capture: trawl_config::CaptureConfig,
        /// Whether the next boot is an ingest node (the default). `false`
        /// boots a query-only node: no WAL writer and no hot buffer, and
        /// `boot::prepare_corpus` takes its query-only door.
        pub ingest_enabled: bool,
        /// The `[scheduler]` section of the next boot. The fixture never
        /// spawns the scheduler loop; a test that needs it spawns it over
        /// [`Unserved::state`], between preparation and serve, as trawld's
        /// `main` does.
        pub scheduler: trawl_server::config::SchedulerConfig,
    }

    /// A boot whose `boot::prepare_corpus` succeeded and whose listener is
    /// bound but not yet served: nothing has answered a request, so a test
    /// can read what the preparation left before any client can act on it.
    pub struct Unserved {
        pub state: trawl_server::state::AppState,
        pub app_db_url: String,
        listener: std::net::TcpListener,
        addr: String,
        config: trawl_server::config::Config,
        http_config: trawl_server::state::HttpConfig,
        keys: Keys,
        fleet: sqlx::PgPool,
        fleet_db_url: String,
        results_clock: trawl_server::results::ResultsClock,
    }

    /// A boot `boot::prepare_corpus` refused. Its listener and state are
    /// already dropped, so the port no longer accepts connections and the
    /// locks the state held are released.
    pub struct RefusedBoot {
        pub error: String,
        pub addr: String,
    }

    impl Restartable {
        pub async fn boot(
            rate_limit: super::RateLimitConfig,
            capture: trawl_config::CaptureConfig,
            seed: impl FnOnce(&str),
        ) -> (Self, TestServer) {
            let tmp = tempfile::tempdir().expect("failed to create temp dir");
            let data = super::seed_data_root(tmp.path());
            seed(&data);
            let disk = Disk {
                dir: tmp.path().to_path_buf(),
                data,
            };
            std::mem::forget(tmp);

            let fleet_db_url = super::create_fleet_database().await;
            let app_db_url = super::create_app_database().await;
            let fleet =
                super::fixture_pool(&fleet_db_url, super::FleetPoolSize::DEFAULT.max_connections)
                    .await;
            let store = super::fleet_keystore(&fleet).await;
            let (analyst, admin, reader, ingest) = super::mint_role_keys(&store).await;
            let schema_admin = store
                .create_key(
                    "schema-admin-key",
                    fleet_auth::PrincipalKind::Service,
                    &super::roles(&["trawl-schema-admin"]),
                    None,
                )
                .await
                .unwrap();
            let coastwatch_only = store
                .create_key(
                    "coastwatch-only",
                    fleet_auth::PrincipalKind::Service,
                    &super::roles(&["coastwatch-viewer"]),
                    None,
                )
                .await
                .unwrap();
            let keys = Keys {
                analyst,
                admin,
                reader,
                ingest,
                schema_admin: schema_admin.plaintext_token.to_string(),
                schema_admin_prefix: schema_admin.info.prefix.clone(),
                coastwatch_only: coastwatch_only.plaintext_token.to_string(),
            };
            let fixture = Self {
                disk,
                rate_limit,
                capture,
                ingest_enabled: true,
                scheduler: trawl_server::config::SchedulerConfig::default(),
            };
            let server = admitted(
                Box::pin(fixture.prepare_over(fleet, fleet_db_url, app_db_url, keys, Utc::now()))
                    .await,
            )
            .serve()
            .await;
            (fixture, server)
        }

        pub async fn restart(&self, server: TestServer) -> TestServer {
            admitted(self.try_reboot(server).await).serve().await
        }

        /// The serve task stops and every handle on the old state drops;
        /// the locks it held are released as its connections close, so the
        /// new boot retries each while the old holder lets go. Then the boot
        /// runs as trawld's does, `boot::prepare_corpus` (and with it the
        /// results reconciliation) included.
        pub async fn try_reboot(&self, server: TestServer) -> Result<Unserved, RefusedBoot> {
            let now = server.results_clock.now();
            let keys = Keys {
                analyst: server.analyst_token.clone(),
                admin: server.admin_token.clone(),
                reader: server.reader_token.clone(),
                ingest: server.ingest_token.clone(),
                schema_admin: server.schema_admin_token.clone(),
                schema_admin_prefix: server.schema_admin_prefix.clone(),
                coastwatch_only: server.coastwatch_only_token.clone(),
            };
            let fleet = server.fleet_pool.clone();
            let fleet_db_url = server.fleet_db_url.clone();
            let app_db_url = server.app_db_url.clone();
            server.serve_task.abort();
            drop(server);
            Box::pin(self.prepare_over(fleet, fleet_db_url, app_db_url, keys, now)).await
        }

        #[allow(clippy::too_many_lines)] // linear assembly, as the shared fixture's
        async fn prepare_over(
            &self,
            fleet: sqlx::PgPool,
            fleet_db_url: String,
            app_db_url: String,
            keys: Keys,
            clock_at: DateTime<Utc>,
        ) -> Result<Unserved, RefusedBoot> {
            use trawl_server::config::{
                AuthConfig, Config, DataConfig, IngestConfig, RetentionConfig, ServerConfig,
                StorageConfig, SyslogConfig, WebConfig,
            };

            let _ = rustls::crypto::ring::default_provider().install_default();
            let (cert_path, key_path) = super::ensure_test_cert();
            let listener = std::net::TcpListener::bind("127.0.0.1:0")
                .expect("bind an ephemeral port for the server");
            let addr = listener
                .local_addr()
                .expect("listener local_addr")
                .to_string();
            let query = super::QueryKnobs::DEFAULT;
            let config = Config {
                server: ServerConfig {
                    http_addr: addr.clone(),
                    timeout_secs: super::DEFAULT_TEST_TIMEOUT_SECS,
                    max_concurrent_queries: query.max_concurrent_queries,
                    max_result_rows: super::RowCaps::DEFAULT.max_result_rows,
                    max_export_rows: super::RowCaps::DEFAULT.max_export_rows,
                    max_request_body_bytes: super::BodyLimits::DEFAULT.max_request_body_bytes,
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
                    rate_limit: self.rate_limit.clone(),
                    monitor_refresh_ms: 1000,
                    query_scratch_dir: None,
                    results_dir: None,
                    link_origin: None,
                    query_spill_max_bytes: query.spill_max_bytes,
                    query_memory_limit: query.memory_limit,
                    query_threads: query.threads,
                },
                data: DataConfig {
                    path: self.disk.data.clone(),
                },
                auth: AuthConfig {
                    database_url: Some(fleet_db_url.clone()),
                    audit_interval_secs: 0,
                },
                ingest: {
                    std::fs::create_dir_all(self.disk.wal()).unwrap();
                    IngestConfig {
                        enabled: self.ingest_enabled,
                        wal_dir: Some(self.disk.wal()),
                        max_body_bytes: super::BodyLimits::DEFAULT.ingest_max_body_bytes,
                        ..IngestConfig::default()
                    }
                },
                retention: RetentionConfig::default(),
                scheduler: self.scheduler.clone(),
                syslog: SyslogConfig::default(),
                web: WebConfig::default(),
                storage: StorageConfig {
                    database_url: Some(app_db_url.clone()),
                },
                capture: self.capture.clone(),
                investigation: trawl_config::InvestigationConfig::default(),
            };

            let auth = trawl_server::state::AuthState::from_key_store(
                fleet_auth::KeyStore::from_pool(fleet.clone()),
            );
            let app = super::app_pool(&app_db_url).await;
            let storage = until_released("the app-state advisory lock", || {
                let app = app.clone();
                async move {
                    match trawl_server::store::StorageState::from_pool(app).await {
                        Err(trawl_server::store::StoreError::LockHeld) => None,
                        other => Some(other.expect("boot the app-state database")),
                    }
                }
            })
            .await;
            let scratch = until_released("the query scratch lock", || async {
                match trawl_server::state::open_query_scratch(&config) {
                    Err(trawl_engine::scratch::ScratchError::Locked { .. }) => None,
                    other => Some(other.expect("lock the query scratch")),
                }
            })
            .await;
            let results_root = until_released("the results root lock", || async {
                match trawl_server::state::open_results_root(&config) {
                    Err(trawl_engine::scratch::ScratchError::Locked { .. }) => None,
                    other => Some(other.expect("hold the results root")),
                }
            })
            .await;
            let results_clock = trawl_server::results::ResultsClock::fixed(clock_at);
            let (state, http_config) = trawl_server::state::AppState::from_parts(
                &config,
                super::test_metrics_handle(),
                super::resolve_derivation(&config),
                auth,
                storage,
                scratch,
                results_root,
                results_clock.clone(),
            )
            .await
            .expect("failed to create app state");
            if let Err(error) = trawl_server::boot::prepare_corpus(&state, &config).await {
                drop(state);
                drop(listener);
                return Err(RefusedBoot { error, addr });
            }
            Ok(Unserved {
                state,
                app_db_url,
                listener,
                addr,
                config,
                http_config,
                keys,
                fleet,
                fleet_db_url,
                results_clock,
            })
        }
    }

    impl Unserved {
        pub async fn serve(self) -> TestServer {
            let Self {
                state,
                app_db_url,
                listener,
                addr,
                config,
                http_config,
                keys,
                fleet,
                fleet_db_url,
                results_clock,
            } = self;
            let serve_task =
                super::serve_and_wait(listener, &state, &config, http_config.clone(), &addr).await;

            TestServer {
                facts: super::FixtureFacts::new(
                    &app_db_url,
                    &fleet_db_url,
                    &addr,
                    super::FleetPoolSize::DEFAULT.max_connections,
                ),
                server_config: config.server.clone(),
                http_config,
                state_dir: config.state_dir(),
                url: format!("https://{addr}"),
                analyst_token: keys.analyst,
                admin_token: keys.admin,
                reader_token: keys.reader,
                ingest_token: keys.ingest,
                schema_admin_token: keys.schema_admin,
                schema_admin_prefix: keys.schema_admin_prefix,
                coastwatch_only_token: keys.coastwatch_only,
                fleet_pool: fleet,
                fleet_db_url,
                app_db_url,
                state,
                results_clock,
                serve_task,
            }
        }
    }

    fn admitted(boot: Result<Unserved, RefusedBoot>) -> Unserved {
        boot.unwrap_or_else(|refused| {
            panic!(
                "the boot corpus preparation must succeed: {}",
                refused.error
            )
        })
    }

    /// Retry `take` until it answers, while a stopped server's last handles
    /// let go of the lock it names; a lock still held after 30 s fails the
    /// test by name.
    async fn until_released<T, F, Fut>(what: &str, mut take: F) -> T
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Option<T>>,
    {
        let give_up = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(taken) = take().await {
                return taken;
            }
            assert!(
                std::time::Instant::now() < give_up,
                "{what} was still held 30 s after the old server stopped"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

pub async fn setup() -> TestServer {
    setup_with_rate_limit(RateLimitConfig::default()).await
}

/// The rows of a typed result, each as the plain JSON object a test reads
/// fields from: column name → cell, so an absent field indexes as null.
pub fn result_rows(result: &trawl_api::value::QueryResult) -> Vec<serde_json::Value> {
    result
        .rows
        .iter()
        .map(|row| {
            result
                .columns
                .iter()
                .zip(row)
                .map(|(column, cell)| (column.name.clone(), cell.to_plain_json()))
                .collect()
        })
        .collect()
}

pub mod audit_capture {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone)]
    pub struct Captured {
        pub fields: BTreeMap<String, String>,
    }

    impl Captured {
        pub fn field(&self, name: &str) -> String {
            self.fields
                .get(name)
                .unwrap_or_else(|| panic!("{name} missing from {self:?}"))
                .clone()
        }
    }

    #[derive(Clone, Default)]
    pub struct Capture {
        events: Arc<Mutex<Vec<Captured>>>,
    }

    impl Capture {
        pub fn events(&self) -> Vec<Captured> {
            self.events.lock().unwrap().clone()
        }

        /// Filtering on `key` is not optional: the subscriber this layer
        /// installs is GLOBAL, so under `cargo test` a sibling test's ack or
        /// repin lands in the same buffer.
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

/// The investigation routes through the fixture's HTTPS listener
/// (ADR-0059): raw JSON requests as one principal, decoded through the wire
/// types where a test asserts on them.
pub mod investigating {
    use reqwest::{Method, StatusCode};
    use serde_json::{Value, json};
    use trawl_api::investigations::{
        EntryPayload, Investigation, InvestigationRead, LogEntry, StepOutcome,
    };

    use super::{MintedKey, TestServer};

    pub fn id(n: u32) -> String {
        format!("{n:032x}")
    }

    /// The answer's body is `null` when it is empty, as a 204's is.
    pub async fn call(
        server: &TestServer,
        token: &str,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> (StatusCode, Value) {
        let client = super::harness_client_builder().build().unwrap();
        let mut request = client
            .request(method, format!("{}/api/v1{path}", server.url))
            .bearer_auth(token);
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        let body = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text)
                .unwrap_or_else(|e| panic!("{status}: the body is not JSON ({e}): {text}"))
        };
        (status, body)
    }

    pub fn code(body: &Value) -> &str {
        body["error"]["code"].as_str().unwrap_or_default()
    }

    pub fn message(body: &Value) -> &str {
        body["error"]["message"].as_str().unwrap_or_default()
    }

    pub fn creation() -> Value {
        json!({
            "title": "502s on checkout",
            "question": "Why does checkout return 502 since 09:00?",
        })
    }

    pub async fn create(server: &TestServer, key: &MintedKey, n: u32) -> Investigation {
        let (status, body) = call(
            server,
            &key.token,
            Method::PUT,
            &format!("/investigations/{}", id(n)),
            Some(&creation()),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        serde_json::from_value(body).expect("an investigation")
    }

    pub async fn read(
        server: &TestServer,
        key: &MintedKey,
        n: u32,
        query: &str,
    ) -> InvestigationRead {
        let (status, body) = call(
            server,
            &key.token,
            Method::GET,
            &format!("/investigations/{}{query}", id(n)),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        serde_json::from_value(body).expect("an investigation read")
    }

    pub async fn steps(server: &TestServer, key: &MintedKey, n: u32) -> Vec<LogEntry> {
        let mut steps: Vec<LogEntry> = read(server, key, n, "")
            .await
            .entries
            .into_iter()
            .filter(|entry| matches!(entry.payload, EntryPayload::Step { .. }))
            .collect();
        steps.sort_by_key(|entry| entry.seq);
        steps
    }

    /// A step entry's outcome, `None` while its work runs.
    pub fn outcome(step: &LogEntry) -> Option<&StepOutcome> {
        match &step.payload {
            EntryPayload::Step { outcome, .. } => outcome.as_ref(),
            other => panic!("not a step: {other:?}"),
        }
    }

    /// A step written by a task the test no longer awaits, such as an
    /// admission whose request went away, may land after the test's own write
    /// returns, so a step not there yet is waited for like one without an
    /// outcome.
    pub async fn settled(server: &TestServer, key: &MintedKey, n: u32, seq: u64) -> LogEntry {
        let give_up = std::time::Instant::now() + std::time::Duration::from_mins(1);
        loop {
            let found = steps(server, key, n)
                .await
                .into_iter()
                .find(|step| step.seq == seq);
            match found {
                Some(step) if outcome(&step).is_some() => return step,
                found => assert!(
                    std::time::Instant::now() < give_up,
                    "step {seq} of investigation {n} never settled: {found:?}"
                ),
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    pub const INVESTIGATION_LOCK: &str = "SELECT status, seq FROM investigations%";

    pub async fn hold(server: &TestServer, n: u32) -> sqlx::PgConnection {
        use sqlx::Connection as _;
        let mut held = sqlx::PgConnection::connect(&server.app_db_url)
            .await
            .unwrap();
        sqlx::query("BEGIN").execute(&mut held).await.unwrap();
        sqlx::query("SELECT 1 FROM investigations WHERE id = $1 FOR UPDATE")
            .bind(id(n))
            .execute(&mut held)
            .await
            .unwrap();
        held
    }

    pub const PROMOTION_LOCK: &str = "UPDATE captured_results%SET state = 'kept'%";

    pub async fn hold_result(server: &TestServer, result: &str) -> sqlx::PgConnection {
        use sqlx::Connection as _;
        let mut held = sqlx::PgConnection::connect(&server.app_db_url)
            .await
            .unwrap();
        sqlx::query("BEGIN").execute(&mut held).await.unwrap();
        let locked = sqlx::query("SELECT 1 FROM captured_results WHERE id = $1 FOR UPDATE")
            .bind(result)
            .execute(&mut held)
            .await
            .unwrap();
        assert_eq!(locked.rows_affected(), 1, "result {result} exists");
        held
    }

    pub async fn lock_waits(server: &TestServer, pattern: &str, count: i64) {
        let probe = super::fixture_pool(&server.app_db_url, 1).await;
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while waiting(&probe, pattern).await != count {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{count} sessions never waited on {pattern}"));
        probe.close().await;
    }

    pub async fn lock_waiters(server: &TestServer, pattern: &str) -> i64 {
        let probe = super::fixture_pool(&server.app_db_url, 1).await;
        let count = waiting(&probe, pattern).await;
        probe.close().await;
        count
    }

    /// [`lock_waiters`] through the caller's `probe`, for a test that polls
    /// it without sleeping, as one on a paused clock does.
    pub async fn waiting(probe: &sqlx::PgPool, pattern: &str) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
             WHERE datname = current_database() AND wait_event_type = 'Lock'
               AND query LIKE $1",
        )
        .bind(pattern)
        .fetch_one(probe)
        .await
        .unwrap()
    }

    /// `method` on `/api/v1{path}` as `token` for an in-process router, with
    /// an optional JSON body and the peer the accept loop would insert.
    pub fn routed(
        token: &str,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> axum::http::Request<axum::body::Body> {
        let builder = axum::http::Request::builder()
            .method(method)
            .uri(format!("/api/v1{path}"))
            .header("authorization", format!("Bearer {token}"));
        let mut request = match body {
            Some(body) => builder
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body.to_string())),
            None => builder.body(axum::body::Body::empty()),
        }
        .unwrap();
        request
            .extensions_mut()
            .insert(std::net::SocketAddr::from(([127, 0, 0, 1], 40_310)));
        request
    }

    pub async fn until_admitted(app: &axum::Router, token: &str) {
        use tower::ServiceExt as _;
        let give_up = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let response = app
                .clone()
                .oneshot(routed(token, "GET", "/investigations", None))
                .await
                .unwrap();
            if response.status() == StatusCode::OK {
                return;
            }
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(code(&body), "request_limit_reached", "{body}");
            assert!(
                std::time::Instant::now() < give_up,
                "the request count never freed"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    pub fn attached(query: &str, n: u32, attach: &Value) -> Value {
        let mut investigation = json!({ "id": id(n) });
        if let Value::Object(members) = attach {
            for (name, value) in members {
                investigation[name] = value.clone();
            }
        }
        json!({
            "query": query,
            "window": { "last": "1h" },
            "investigation": investigation,
        })
    }
}
