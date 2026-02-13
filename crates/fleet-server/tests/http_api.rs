//! End-to-end HTTPS API tests for fleetd.
//!
//! Starts a real TLS server on a random port with a self-signed cert,
//! creates API keys, and verifies the full request lifecycle.

use std::net::TcpListener;
use std::path::PathBuf;

use fleet_auth::roles::Role;
use fleet_auth::store::KeyStore;
use fleet_client::HttpClient;
use fleet_server::config::{
    AuthConfig, Config, DataConfig, IngestConfig, RateLimitConfig, ServerConfig,
};
use fleet_server::state::AppState;
use fleet_server::transport::http;

/// Find an available port by binding to :0 and reading back the assigned port.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind ephemeral port");
    listener.local_addr().unwrap().port()
}

/// Generate test parquet fixtures using `DuckDB`.
///
/// Creates per-service parquet files matching the compaction naming
/// convention (`{service}.parquet`), so service-scoped glob narrowing
/// works correctly in integration tests.
fn ensure_fixtures(dir: &std::path::Path) -> String {
    let parquet_dir = dir.join("parquet");
    let nginx_path = parquet_dir.join("nginx.parquet");
    if nginx_path.exists() {
        return format!("{}/**/*.parquet", parquet_dir.display());
    }

    std::fs::create_dir_all(&parquet_dir).unwrap();
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
    let postgres_path = parquet_dir.join("postgres.parquet");
    conn.execute_batch(&format!(
        "COPY (SELECT * FROM logs WHERE service = 'nginx') TO '{}' (FORMAT PARQUET)",
        nginx_path.display()
    ))
    .unwrap();
    conn.execute_batch(&format!(
        "COPY (SELECT * FROM logs WHERE service = 'postgres') TO '{}' (FORMAT PARQUET)",
        postgres_path.display()
    ))
    .unwrap();

    format!("{}/**/*.parquet", parquet_dir.display())
}

/// Generate a self-signed cert/key pair in the given directory.
fn generate_test_cert(dir: &std::path::Path) -> (PathBuf, PathBuf) {
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");

    let san = vec!["localhost".to_owned(), "127.0.0.1".to_owned()];
    let rcgen::CertifiedKey { cert, key_pair } = rcgen::generate_simple_self_signed(san).unwrap();

    std::fs::write(&cert_path, cert.pem()).unwrap();
    std::fs::write(&key_path, key_pair.serialize_pem()).unwrap();

    (cert_path, key_path)
}

/// Test server handle with analyst and admin tokens.
struct TestServer {
    url: String,
    analyst_token: String,
    admin_token: String,
}

/// Set up a test server with fixtures and return a `TestServer` handle.
async fn setup() -> TestServer {
    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    let data_glob = ensure_fixtures(tmp.path());
    let auth_db = tmp.path().join("auth.db");

    // Create API keys for both roles.
    let store = KeyStore::open(&auth_db).unwrap();
    let analyst = store.create_key("test-key", Role::Analyst, None).unwrap();
    let admin = store.create_key("admin-key", Role::Admin, None).unwrap();
    drop(store);

    // Generate ephemeral self-signed cert.
    let (cert_path, key_path) = generate_test_cert(tmp.path());

    let port = free_port();
    let addr = format!("127.0.0.1:{port}");

    let config = Config {
        server: ServerConfig {
            http_addr: addr.clone(),
            timeout_secs: 10,
            max_concurrent_queries: 2,
            max_result_rows: 100_000,
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
            rate_limit: RateLimitConfig::default(),
        },
        data: DataConfig { path: data_glob },
        auth: AuthConfig { db_path: auth_db },
        ingest: IngestConfig::default(),
    };

    let (state, http_config) = AppState::from_config(&config).expect("failed to create app state");

    // Spawn the HTTPS server in a background task.
    let server_config = config.server.clone();
    tokio::spawn(async move {
        http::serve(state, &http_config, &server_config)
            .await
            .unwrap();
    });

    // Give the server a moment to start and bind.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Leak the tempdir so it survives the test (cleaned up by OS).
    std::mem::forget(tmp);

    TestServer {
        url: format!("https://{addr}"),
        analyst_token: analyst.plaintext_token.to_string(),
        admin_token: admin.plaintext_token.to_string(),
    }
}

#[tokio::test]
async fn health_returns_ok() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, "unused").unwrap();
    let health = client.health().await.unwrap();
    assert_eq!(health["status"], "ok");
}

#[tokio::test]
async fn query_returns_results() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let result = client.query("*").await.unwrap();
    assert_eq!(result.row_count(), 3);
}

#[tokio::test]
async fn query_with_filter() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let result = client.query("service:nginx").await.unwrap();
    assert_eq!(result.row_count(), 2);
}

#[tokio::test]
async fn query_with_stats_pipeline() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let result = client.query("* | stats count() by service").await.unwrap();
    // nginx: 2, postgres: 1 → 2 rows
    assert_eq!(result.row_count(), 2);
}

#[tokio::test]
async fn query_rejects_missing_auth() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, "").unwrap();

    let result = client.query("*").await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    match err {
        fleet_client::ClientError::Server { status, .. } => {
            assert!(status == 400 || status == 401);
        }
        other => panic!("expected Server error, got: {other:?}"),
    }
}

#[tokio::test]
async fn query_rejects_invalid_token() {
    let server = setup().await;
    let client =
        HttpClient::new_insecure(&server.url, "flt_ZZZZZZZZ_totally_fake_token_here1234").unwrap();

    let result = client.query("*").await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    match err {
        fleet_client::ClientError::Server { status, .. } => {
            assert_eq!(status, 401);
        }
        other => panic!("expected Server error with 401, got: {other:?}"),
    }
}

#[tokio::test]
async fn query_rejects_bad_dsl() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let result = client.query("| | | broken {{{").await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    match err {
        fleet_client::ClientError::Server { status, .. } => {
            assert_eq!(status, 400);
        }
        other => panic!("expected 400 for bad DSL, got: {other:?}"),
    }
}

// -- schema endpoint tests ---------------------------------------------------

#[tokio::test]
async fn schema_returns_columns() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let schema = client.schema().await.unwrap();
    assert!(!schema.columns.is_empty());

    // Our test fixture has these exact columns.
    let names: Vec<&str> = schema.columns.iter().map(|c| c.name.as_str()).collect();
    assert!(names.contains(&"timestamp"), "missing timestamp column");
    assert!(names.contains(&"host"), "missing host column");
    assert!(names.contains(&"service"), "missing service column");
    assert!(names.contains(&"level"), "missing level column");
    assert!(names.contains(&"message"), "missing message column");
    assert_eq!(schema.file_count, 2);
}

#[tokio::test]
async fn schema_caching_works() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let first = client.schema().await.unwrap();
    assert!(!first.cached, "first call should not be cached");

    let second = client.schema().await.unwrap();
    assert!(second.cached, "second call should be cached");
}

// -- queries endpoint tests --------------------------------------------------

#[tokio::test]
async fn queries_shows_history() {
    let server = setup().await;
    let analyst = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let admin = HttpClient::new_insecure(&server.url, &server.admin_token).unwrap();

    // Run a query so there's something in history.
    analyst.query("*").await.unwrap();

    let queries = admin.queries().await.unwrap();
    assert!(
        !queries.recent.is_empty(),
        "recent history should contain the query we just ran"
    );
    assert_eq!(queries.recent[0].rows, Some(3));
    assert!(!queries.recent[0].timed_out);
}

#[tokio::test]
async fn queries_rejects_non_admin() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let result = client.queries().await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    match err {
        fleet_client::ClientError::Server { status, .. } => {
            assert_eq!(status, 401);
        }
        other => panic!("expected 401 for non-admin, got: {other:?}"),
    }
}
