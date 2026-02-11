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

    let port = free_port();
    let addr = format!("127.0.0.1:{port}");

    let config = Config {
        server: ServerConfig {
            http_addr: addr.clone(),
            timeout_secs: 10,
            max_concurrent_queries: 2,
            log_file: None,
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

    TestServer {
        url: format!("http://{addr}"),
        analyst_token: analyst.plaintext_token,
        admin_token: admin.plaintext_token,
    }
}

#[tokio::test]
async fn health_returns_ok() {
    let server = setup().await;
    let client = HttpClient::new(&server.url, "unused");
    let health = client.health().await.unwrap();
    assert_eq!(health["status"], "ok");
}

#[tokio::test]
async fn query_returns_results() {
    let server = setup().await;
    let client = HttpClient::new(&server.url, &server.analyst_token);
    let result = client.query("*").await.unwrap();
    assert_eq!(result.row_count(), 3);
}

#[tokio::test]
async fn query_with_filter() {
    let server = setup().await;
    let client = HttpClient::new(&server.url, &server.analyst_token);
    let result = client.query("service:nginx").await.unwrap();
    assert_eq!(result.row_count(), 2);
}

#[tokio::test]
async fn query_with_stats_pipeline() {
    let server = setup().await;
    let client = HttpClient::new(&server.url, &server.analyst_token);
    let result = client.query("* | stats count() by service").await.unwrap();
    // nginx: 2, postgres: 1 → 2 rows
    assert_eq!(result.row_count(), 2);
}

#[tokio::test]
async fn query_rejects_missing_auth() {
    let server = setup().await;
    let client = HttpClient::new(&server.url, "");

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
    let client = HttpClient::new(&server.url, "flt_ZZZZZZZZ_totally_fake_token_here1234");

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
    let client = HttpClient::new(&server.url, &server.analyst_token);

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
    let client = HttpClient::new(&server.url, &server.analyst_token);

    let schema = client.schema().await.unwrap();
    assert!(!schema.columns.is_empty());

    // Our test fixture has these exact columns.
    let names: Vec<&str> = schema.columns.iter().map(|c| c.name.as_str()).collect();
    assert!(names.contains(&"timestamp"), "missing timestamp column");
    assert!(names.contains(&"host"), "missing host column");
    assert!(names.contains(&"service"), "missing service column");
    assert!(names.contains(&"level"), "missing level column");
    assert!(names.contains(&"message"), "missing message column");
    assert_eq!(schema.file_count, 1);
}

#[tokio::test]
async fn schema_caching_works() {
    let server = setup().await;
    let client = HttpClient::new(&server.url, &server.analyst_token);

    let first = client.schema().await.unwrap();
    assert!(!first.cached, "first call should not be cached");

    let second = client.schema().await.unwrap();
    assert!(second.cached, "second call should be cached");
}

// -- queries endpoint tests --------------------------------------------------

#[tokio::test]
async fn queries_shows_history() {
    let server = setup().await;
    let analyst = HttpClient::new(&server.url, &server.analyst_token);
    let admin = HttpClient::new(&server.url, &server.admin_token);

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
    let client = HttpClient::new(&server.url, &server.analyst_token);

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
