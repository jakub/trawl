//! End-to-end HTTP API tests for fleetd.
//!
//! Starts a real server on a random port, creates API keys, and
//! verifies the full request lifecycle.

use std::net::TcpListener;

use fleet_auth::roles::Role;
use fleet_auth::store::KeyStore;
use fleet_client::HttpClient;
use fleet_server::config::{AuthConfig, Config, DataConfig, ServerConfig};
use fleet_server::state::AppState;
use fleet_server::transport::http;

/// Find an available port by binding to :0 and reading back the assigned port.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind ephemeral port");
    listener.local_addr().unwrap().port()
}

/// Generate test parquet fixtures using `DuckDB`.
fn ensure_fixtures(dir: &std::path::Path) -> String {
    let parquet_dir = dir.join("parquet");
    let file_path = parquet_dir.join("logs.parquet");
    if file_path.exists() {
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

    conn.execute_batch(&format!(
        "COPY logs TO '{}' (FORMAT PARQUET)",
        file_path.display()
    ))
    .unwrap();

    format!("{}/**/*.parquet", parquet_dir.display())
}

/// Set up a test server with fixtures and return (addr, token).
async fn setup() -> (String, String) {
    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    let data_glob = ensure_fixtures(tmp.path());
    let auth_db = tmp.path().join("auth.db");

    // Create an API key.
    let store = KeyStore::open(&auth_db).unwrap();
    let created = store.create_key("test-key", Role::Analyst, None).unwrap();
    let token = created.plaintext_token;
    drop(store);

    let port = free_port();
    let addr = format!("127.0.0.1:{port}");

    let config = Config {
        server: ServerConfig {
            http_addr: addr.clone(),
            timeout_secs: 10,
            max_concurrent_queries: 2,
        },
        data: DataConfig { path: data_glob },
        auth: AuthConfig { db_path: auth_db },
    };

    let state = AppState::from_config(&config);
    let app = http::router(state);

    // Spawn the server in a background task.
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Give the server a moment to start.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Leak the tempdir so it survives the test (cleaned up by OS).
    std::mem::forget(tmp);

    (format!("http://{addr}"), token)
}

#[tokio::test]
async fn health_returns_ok() {
    let (url, _token) = setup().await;
    let client = HttpClient::new(&url, "unused");
    let health = client.health().await.unwrap();
    assert_eq!(health["status"], "ok");
}

#[tokio::test]
async fn query_returns_results() {
    let (url, token) = setup().await;
    let client = HttpClient::new(&url, &token);
    let result = client.query("*").await.unwrap();
    assert_eq!(result.row_count(), 3);
}

#[tokio::test]
async fn query_with_filter() {
    let (url, token) = setup().await;
    let client = HttpClient::new(&url, &token);
    let result = client.query("service:nginx").await.unwrap();
    assert_eq!(result.row_count(), 2);
}

#[tokio::test]
async fn query_with_stats_pipeline() {
    let (url, token) = setup().await;
    let client = HttpClient::new(&url, &token);
    let result = client.query("* | stats count() by service").await.unwrap();
    // nginx: 2, postgres: 1 → 2 rows
    assert_eq!(result.row_count(), 2);
}

#[tokio::test]
async fn query_rejects_missing_auth() {
    let (url, _token) = setup().await;
    let client = HttpClient::new(&url, "");

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
    let (url, _token) = setup().await;
    let client = HttpClient::new(&url, "flt_ZZZZZZZZ_totally_fake_token_here1234");

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
    let (url, token) = setup().await;
    let client = HttpClient::new(&url, &token);

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
