// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared harness for trawld's pg-backed end-to-end tests.
//!
//! Starts a real TLS server on a random port with a self-signed cert,
//! mints keys in an ephemeral fleet-auth postgres database (shared
//! `PgFixture` from fleet-auth's `test-support` feature), and hands out
//! role tokens.

#![allow(dead_code)] // each test binary uses a subset of these items

use std::net::TcpListener;
use std::path::PathBuf;

use fleet_auth::test_support::PgFixture;
use fleet_auth::{KeyStore, PrincipalKind, RoleAssignment};
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
///
/// Holds the per-test postgres fixture so the ephemeral database outlives
/// the server (dropped when the test ends).
pub struct TestServer {
    pub url: String,
    pub analyst_token: String,
    pub admin_token: String,
    pub reader_token: String,
    pub ingest_token: String,
    pub coastwatch_only_token: String,
    /// Per-test postgres fixture; kept so the ephemeral database outlives
    /// the server.
    pub fx: PgFixture,
}

/// Set up the per-test postgres fixture, honouring skip-or-fail semantics:
/// missing `FLEET_DATABASE_URL` skips unless `FLEET_TESTS_REQUIRED=1`.
pub async fn pg_fixture_or_skip() -> Option<PgFixture> {
    let fx = PgFixture::setup().await;
    if fx.is_none() {
        assert!(
            !fleet_auth::test_support::require_database(),
            "FLEET_DATABASE_URL not set but FLEET_TESTS_REQUIRED is — hard failure"
        );
        eprintln!("http_api test skipped: FLEET_DATABASE_URL not set or empty");
    }
    fx
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
pub async fn setup_with_rate_limit(rate_limit: RateLimitConfig) -> Option<TestServer> {
    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    let server = setup_in_dir(tmp.path(), rate_limit).await;
    // Leak the tempdir so it survives the test (cleaned up by OS).
    std::mem::forget(tmp);
    server
}

/// Set up a test server whose transitional sqlite store + WAL live under
/// the given directory (which must outlive the server). Lets tests seed the
/// directory beforehand — e.g. the legacy-auth.db quarantine test.
pub async fn setup_in_dir(
    dir: &std::path::Path,
    rate_limit: RateLimitConfig,
) -> Option<TestServer> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let fx = pg_fixture_or_skip().await?;
    // Transitional sqlite app-state store — FRESH file, never auth.db.
    let store_db = dir.join("store.db");

    let store = KeyStore::from_pool(fx.pool());
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
            db_path: store_db,
            database_url: Some(fx.database_url()),
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
        storage: StorageConfig::default(),
    };

    let (state, http_config) = AppState::from_config(&config, test_metrics_handle())
        .await
        .expect("failed to create app state");
    let server_config = config.server.clone();
    let state_dir = config.state_dir();
    tokio::spawn(async move {
        http::serve(state, &http_config, &server_config, &state_dir, None)
            .await
            .unwrap();
    });
    wait_for_ready(&addr).await;

    Some(TestServer {
        url: format!("https://{addr}"),
        analyst_token,
        admin_token,
        reader_token,
        ingest_token,
        coastwatch_only_token: coastwatch_only.plaintext_token.to_string(),
        fx,
    })
}

/// Set up a test server with fixtures and return a `TestServer` handle.
///
/// Returns `None` (after logging) when no `FLEET_DATABASE_URL` is configured
/// and `FLEET_TESTS_REQUIRED` is unset — callers `else { return }` to skip.
pub async fn setup() -> Option<TestServer> {
    setup_with_rate_limit(RateLimitConfig::default()).await
}
