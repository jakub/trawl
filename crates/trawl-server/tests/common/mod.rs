// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared harness for trawld's pg-backed end-to-end tests.
//!
//! Each test runs under `#[sqlx::test(migrations = false)]`: the sqlx
//! harness (driven by `DATABASE_URL` — unset means a loud failure, never a
//! skip) hands the test a fresh ephemeral database, which we use as the
//! FLEET database (running `fleet_auth::MIGRATOR` on it explicitly). The
//! trawl app-state database is a sibling database created empty by
//! [`create_app_database`] and migrated by trawld's REAL boot path
//! (`StorageState::connect`: pool → advisory lock → migrate), so every
//! server test exercises boot-time migration.
//!
//! The two schemas must live in two databases: sqlx 0.8 hardwires one
//! `_sqlx_migrations` table per database and both migration sets would
//! collide in it. Two databases also match the production shape.
//!
//! Sibling databases are best-effort leftovers: nextest is process-per-test
//! and the server's pools stay open until process exit, so we don't DROP
//! them (a throwaway CI container makes leaks acceptable; local dev reuses
//! names prefixed `trawl_app_test_` for easy bulk cleanup).

#![allow(dead_code)] // each test binary uses a subset of these items

use std::net::TcpListener;
use std::path::PathBuf;

use fleet_auth::{KeyStore, PrincipalKind, RoleAssignment};
use sqlx::{Connection as _, Executor as _, PgPool, postgres::PgConnection};
use trawl_server::policy::Role;

/// Helper: build a single-grant `trawl:<role>` assignment vector for tests.
pub fn trawl_only(role: Role) -> Vec<RoleAssignment> {
    vec![RoleAssignment {
        app: "trawl".into(),
        role: role.as_str().into(),
    }]
}
use trawl_server::config::{
    AuthConfig, Config, DataConfig, IngestConfig, RateLimitConfig, RetentionConfig,
    SchedulerConfig, ServerConfig, StorageConfig, SyslogConfig, WebConfig,
};
use trawl_server::state::AppState;
use trawl_server::transport::http;

/// Create a `PrometheusHandle` for test contexts.
///
/// Uses `PrometheusBuilder` with a noop recorder since tests don't scrape
/// the endpoint. Each call creates an independent recorder which is NOT
/// installed globally (the handle is self-contained).
pub fn test_metrics_handle() -> metrics_exporter_prometheus::PrometheusHandle {
    metrics_exporter_prometheus::PrometheusBuilder::new()
        .build_recorder()
        .handle()
}

/// Find an available port by binding to :0 and reading back the assigned port.
pub fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind ephemeral port");
    listener.local_addr().unwrap().port()
}

/// The base admin DSN the sqlx test harness runs on.
///
/// `#[sqlx::test]` hardwires `DATABASE_URL`; it is guaranteed set by the
/// time a test body runs (the harness panics loudly otherwise).
pub fn admin_database_url() -> String {
    std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set for #[sqlx::test] (the harness enforces this)")
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

/// DSN of the sqlx-provided per-test database (used as the FLEET database).
pub fn fleet_database_url(pool: &PgPool) -> String {
    let opts = pool.connect_options();
    let db = opts.get_database().expect("test pool has a database");
    swap_database(&admin_database_url(), db)
}

/// Create an EMPTY sibling database for the trawl app-state store and
/// return its DSN. trawld's real boot path migrates it (AC5).
pub async fn create_app_database(pool: &PgPool) -> String {
    sweep_stale_app_databases(pool).await;
    // The run marker + owner pid are encoded in the name so future runs can
    // tell an abandoned database from a live sibling's (see the sweep).
    let name = format!(
        "trawl_app_test_{}_{}_{}",
        run_marker(),
        std::process::id(),
        random_db_suffix()
    );
    pool.execute(sqlx::AssertSqlSafe(format!(r#"CREATE DATABASE "{name}""#)))
        .await
        .expect("CREATE DATABASE for app store — does the role have CREATEDB?");
    swap_database(&admin_database_url(), &name)
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

/// Parse `(run_marker, pid)` out of a `trawl_app_test_{run}_{pid}_{rand}`
/// name. Names from other schemes yield `None` and are left alone.
fn owner_of(datname: &str) -> Option<(String, u32)> {
    let mut parts = datname.strip_prefix("trawl_app_test_")?.split('_');
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

/// Best-effort sweep of sibling app databases leaked by earlier runs.
///
/// The server's pools stay open until process exit, so a test cannot drop
/// its OWN app database; instead each run garbage-collects its
/// predecessors'. A database is only dropped when BOTH guards agree it is
/// abandoned: (a) it belongs to a DIFFERENT nextest run — same-run
/// siblings are structurally never touched, even in the window between
/// their CREATE and the server's first connection — and (b) the owner pid
/// encoded in its name is no longer alive. A single advisory lock elects
/// one sweeper at a time, and DROP without FORCE is a final safety net
/// (live connections make it error harmlessly).
async fn sweep_stale_app_databases(pool: &PgPool) {
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

    if let Ok(rows) =
        sqlx::query("SELECT datname FROM pg_database WHERE datname LIKE 'trawl_app_test_%'")
            .fetch_all(&mut admin)
            .await
    {
        let marker = run_marker();
        for row in rows {
            let Ok(name): Result<String, _> = row.try_get("datname") else {
                continue;
            };
            let Some((owner_marker, owner_pid)) = owner_of(&name) else {
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
    let _ = pool; // sweep uses its own admin connection
}

#[test]
fn sweep_guards_parse_own_database_name() {
    // The sweeper's abandoned-db detection must round-trip the naming
    // scheme `create_app_database` uses — a parse mismatch here silently
    // turns the sweeper into a live-sibling killer (it did once: the
    // guard-bypassing bug behind transient 'database does not exist' boot
    // failures).
    let name = format!(
        "trawl_app_test_{}_{}_{}",
        run_marker(),
        std::process::id(),
        "abcdefghijkl"
    );
    let (marker, pid) = owner_of(&name).expect("own name must parse");
    assert_eq!(marker, run_marker());
    assert_eq!(pid, std::process::id());
    assert!(pid_alive(pid), "our own pid is alive");
}

/// Forcibly drop a database by DSN, terminating live connections —
/// simulates a backend dying under a running server.
pub async fn kill_database(url: &str) {
    let name = url
        .rsplit('/')
        .next()
        .and_then(|last| last.split('?').next())
        .expect("database name in url");
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
    let name = url
        .rsplit('/')
        .next()
        .and_then(|last| last.split('?').next())
        .expect("database name in url");
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

/// Generate test parquet fixtures using `DuckDB`.
///
/// Writes fixtures to a stable path under `CARGO_MANIFEST_DIR` so all
/// nextest processes share the same files. Uses PID-unique temp files
/// and atomic rename for race-free coordination.
pub fn ensure_fixtures() -> String {
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("parquet");
    let nginx_path = dir.join("nginx.parquet");

    // Remove stale scheduled-run results from prior test invocations —
    // the `**/*.parquet` glob would otherwise include them as log data.
    let scheduled_dir = dir.join("scheduled");
    if scheduled_dir.exists() {
        let _ = std::fs::remove_dir_all(&scheduled_dir);
    }

    if !nginx_path.exists() {
        std::fs::create_dir_all(&dir).unwrap();
        let suffix = format!("_{}", std::process::id());

        let conn = duckdb::Connection::open_in_memory().unwrap();

        conn.execute_batch(
            "CREATE TABLE logs (
                timestamp TIMESTAMP,
                host VARCHAR,
                service VARCHAR,
                level VARCHAR,
                message VARCHAR
            )",
        )
        .unwrap();

        conn.execute_batch(
            "INSERT INTO logs VALUES
            ('2024-01-15 10:00:00', 'web01', 'nginx', 'info', 'request ok'),
            ('2024-01-15 10:00:01', 'web01', 'nginx', 'error', 'upstream timeout'),
            ('2024-01-15 10:00:02', 'db01', 'postgres', 'info', 'checkpoint complete')",
        )
        .unwrap();

        // Write per-service parquet files to match compaction naming convention.
        let nginx_tmp = dir.join(format!("nginx{suffix}.parquet"));
        let postgres_tmp = dir.join(format!("postgres{suffix}.parquet"));

        conn.execute_batch(&format!(
            "COPY (SELECT * FROM logs WHERE service = 'nginx') TO '{}' (FORMAT PARQUET)",
            nginx_tmp.display()
        ))
        .unwrap();
        conn.execute_batch(&format!(
            "COPY (SELECT * FROM logs WHERE service = 'postgres') TO '{}' (FORMAT PARQUET)",
            postgres_tmp.display()
        ))
        .unwrap();

        // Atomic rename — loser's rename fails harmlessly if winner already placed the file.
        let _ = std::fs::rename(&nginx_tmp, nginx_path);
        let _ = std::fs::rename(&postgres_tmp, dir.join("postgres.parquet"));
        // Clean up if we lost the race.
        let _ = std::fs::remove_file(&nginx_tmp);
        let _ = std::fs::remove_file(&postgres_tmp);
    }

    format!("{}/**/*.parquet", dir.display())
}

/// Return a shared self-signed cert/key pair, generating on first call.
///
/// Uses the same PID-unique temp + atomic rename pattern as fixtures
/// for race-free coordination across nextest processes.
pub fn ensure_test_cert() -> (PathBuf, PathBuf) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("tls");
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");

    if !cert_path.exists() {
        std::fs::create_dir_all(&dir).unwrap();
        let suffix = format!("_{}", std::process::id());

        let san = vec!["localhost".to_owned(), "127.0.0.1".to_owned()];
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(san).unwrap();

        let cert_tmp = dir.join(format!("cert{suffix}.pem"));
        let key_tmp = dir.join(format!("key{suffix}.pem"));
        std::fs::write(&cert_tmp, cert.pem()).unwrap();
        std::fs::write(&key_tmp, signing_key.serialize_pem()).unwrap();

        let _ = std::fs::rename(&cert_tmp, &cert_path);
        let _ = std::fs::rename(&key_tmp, &key_path);
        // Clean up if we lost the race.
        let _ = std::fs::remove_file(&cert_tmp);
        let _ = std::fs::remove_file(&key_tmp);
    }

    (cert_path, key_path)
}

/// Test server handle with analyst, admin, and ingest tokens.
pub struct TestServer {
    pub url: String,
    pub analyst_token: String,
    pub admin_token: String,
    pub reader_token: String,
    pub ingest_token: String,
    pub coastwatch_only_token: String,
    /// Pool on the per-test FLEET database (mint/revoke keys mid-test).
    pub fleet_pool: PgPool,
    /// DSN of the fleet database (kill it to simulate auth-backend loss).
    pub fleet_db_url: String,
    /// DSN of the sibling trawl app-state database.
    pub app_db_url: String,
}

impl TestServer {
    /// Force-drop the fleet keystore database under the running server.
    pub async fn kill_fleet_database(&self) {
        kill_database(&self.fleet_db_url).await;
    }

    /// Force-drop the app-state database under the running server.
    pub async fn kill_app_database(&self) {
        kill_database(&self.app_db_url).await;
    }
}

/// Create the standard role keys in the fleet keystore.
/// Returns (analyst, admin, reader, ingest) plaintext tokens.
pub async fn mint_role_keys(store: &KeyStore) -> (String, String, String, String) {
    let analyst = store
        .create_key(
            "test-key",
            PrincipalKind::Service,
            &trawl_only(Role::Analyst),
            None,
        )
        .await
        .unwrap();
    let admin = store
        .create_key(
            "admin-key",
            PrincipalKind::Service,
            &trawl_only(Role::Admin),
            None,
        )
        .await
        .unwrap();
    let reader = store
        .create_key(
            "reader-key",
            PrincipalKind::Service,
            &trawl_only(Role::Reader),
            None,
        )
        .await
        .unwrap();
    let ingest = store
        .create_key(
            "ingest-key",
            PrincipalKind::Service,
            &trawl_only(Role::Ingest),
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

/// Migrate the sqlx-provided database with the FLEET schema and return a
/// keystore on it. (`migrations = false` hands us a bare database.)
pub async fn fleet_keystore(pool: &PgPool) -> KeyStore {
    fleet_auth::MIGRATOR
        .run(pool)
        .await
        .expect("apply fleet-auth migrations to per-test database");
    KeyStore::from_pool(pool.clone())
}

/// Poll the health endpoint until the server is ready (up to 1s).
pub async fn wait_for_ready(addr: &str) {
    let poll_client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();
    let health_url = format!("https://{addr}/api/v1/health");
    for _ in 0..100 {
        if poll_client.get(&health_url).send().await.is_ok() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("test server failed to become ready within 1s");
}

/// Set up a test server with custom rate limiting for rate limit tests.
pub async fn setup_with_rate_limit(pool: PgPool, rate_limit: RateLimitConfig) -> TestServer {
    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    let server = setup_in_dir(pool, tmp.path(), rate_limit).await;
    // Leak the tempdir so it survives the test (cleaned up by OS).
    std::mem::forget(tmp);
    server
}

/// Set up a test server whose WAL lives under the given directory (which
/// must outlive the server).
pub async fn setup_in_dir(
    pool: PgPool,
    dir: &std::path::Path,
    rate_limit: RateLimitConfig,
) -> TestServer {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let store = fleet_keystore(&pool).await;
    let fleet_db_url = fleet_database_url(&pool);
    let app_db_url = create_app_database(&pool).await;

    let (analyst_token, admin_token, reader_token, ingest_token) = mint_role_keys(&store).await;
    let coastwatch_only = store
        .create_key(
            "coastwatch-only",
            PrincipalKind::Service,
            &[RoleAssignment {
                app: "coastwatch".into(),
                role: "viewer".into(),
            }],
            None,
        )
        .await
        .unwrap();

    let (cert_path, key_path) = ensure_test_cert();
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");

    let config = Config {
        server: ServerConfig {
            http_addr: addr.clone(),
            timeout_secs: 10,
            max_concurrent_queries: 2,
            max_result_rows: 100_000,
            max_export_rows: 1_000_000,
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
            rate_limit,
            monitor_refresh_ms: 1000,
        },
        data: DataConfig {
            path: ensure_fixtures(),
        },
        auth: AuthConfig {
            db_path: None,
            database_url: Some(fleet_db_url.clone()),
            audit_interval_secs: 0,
        },
        ingest: {
            let wal_dir = dir.join("wal");
            std::fs::create_dir_all(&wal_dir).unwrap();
            IngestConfig {
                wal_dir: Some(wal_dir),
                ..IngestConfig::default()
            }
        },
        retention: RetentionConfig::default(),
        scheduler: SchedulerConfig::default(),
        syslog: SyslogConfig::default(),
        web: WebConfig::default(),
        storage: StorageConfig {
            database_url: Some(app_db_url.clone()),
        },
    };

    let (state, http_config) = AppState::from_config(&config, test_metrics_handle())
        .await
        .expect("failed to create app state");

    // Snapshot collector: makes GET /api/v1/dashboard live in tests, same
    // as trawld's main() does in production.
    let _collector = trawl_server::monitor::spawn_snapshot_collector(
        state.clone(),
        addr.clone(),
        config.server.max_sse_connections,
        config.scheduler.enabled,
        false,
    );

    let server_config = config.server.clone();
    let state_dir = config.state_dir();
    tokio::spawn(async move {
        http::serve(state, &http_config, &server_config, &state_dir, None)
            .await
            .unwrap();
    });
    wait_for_ready(&addr).await;

    TestServer {
        url: format!("https://{addr}"),
        analyst_token,
        admin_token,
        reader_token,
        ingest_token,
        coastwatch_only_token: coastwatch_only.plaintext_token.to_string(),
        fleet_pool: pool,
        fleet_db_url,
        app_db_url,
    }
}

/// Set up a test server with fixtures and return a `TestServer` handle.
///
/// `pool` is the `#[sqlx::test(migrations = false)]`-provided per-test
/// database; it becomes the fleet keystore database.
pub async fn setup(pool: PgPool) -> TestServer {
    setup_with_rate_limit(pool, RateLimitConfig::default()).await
}
