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
    AuthConfig, Config, DataConfig, IngestConfig, RateLimitConfig, RetentionConfig, ServerConfig,
};
use fleet_server::state::AppState;
use fleet_server::transport::http;

/// Create a `PrometheusHandle` for test contexts.
///
/// Uses `PrometheusBuilder` with a noop recorder since tests don't scrape
/// the endpoint. Each call creates an independent recorder which is NOT
/// installed globally (the handle is self-contained).
fn test_metrics_handle() -> metrics_exporter_prometheus::PrometheusHandle {
    metrics_exporter_prometheus::PrometheusBuilder::new()
        .build_recorder()
        .handle()
}

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

/// Test server handle with analyst, admin, and ingest tokens.
struct TestServer {
    url: String,
    analyst_token: String,
    admin_token: String,
    ingest_token: String,
}

/// Set up a test server with custom rate limiting for rate limit tests.
async fn setup_with_rate_limit(rate_limit: RateLimitConfig) -> TestServer {
    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    let data_glob = ensure_fixtures(tmp.path());
    let auth_db = tmp.path().join("auth.db");

    let store = KeyStore::open(&auth_db).unwrap();
    let analyst = store.create_key("test-key", Role::Analyst, None).unwrap();
    let admin = store.create_key("admin-key", Role::Admin, None).unwrap();
    let ingest = store.create_key("ingest-key", Role::Ingest, None).unwrap();
    drop(store);

    let (cert_path, key_path) = generate_test_cert(tmp.path());
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
        },
        data: DataConfig { path: data_glob },
        auth: AuthConfig {
            db_path: auth_db,
            audit_interval_secs: 0,
            auth_cache_ttl_secs: 300,
        },
        ingest: {
            let wal_dir = tmp.path().join("wal");
            std::fs::create_dir_all(&wal_dir).unwrap();
            IngestConfig {
                wal_dir: Some(wal_dir),
                ..IngestConfig::default()
            }
        },
        retention: RetentionConfig::default(),
    };

    let (state, http_config) =
        AppState::from_config(&config, test_metrics_handle()).expect("failed to create app state");
    let server_config = config.server.clone();
    tokio::spawn(async move {
        http::serve(state, &http_config, &server_config)
            .await
            .unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    std::mem::forget(tmp);

    TestServer {
        url: format!("https://{addr}"),
        analyst_token: analyst.plaintext_token.to_string(),
        admin_token: admin.plaintext_token.to_string(),
        ingest_token: ingest.plaintext_token.to_string(),
    }
}

/// Set up a test server with fixtures and return a `TestServer` handle.
async fn setup() -> TestServer {
    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    let data_glob = ensure_fixtures(tmp.path());
    let auth_db = tmp.path().join("auth.db");

    // Create API keys for all test roles.
    let store = KeyStore::open(&auth_db).unwrap();
    let analyst = store.create_key("test-key", Role::Analyst, None).unwrap();
    let admin = store.create_key("admin-key", Role::Admin, None).unwrap();
    let ingest = store.create_key("ingest-key", Role::Ingest, None).unwrap();
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
            rate_limit: RateLimitConfig::default(),
        },
        data: DataConfig { path: data_glob },
        auth: AuthConfig {
            db_path: auth_db,
            audit_interval_secs: 0,
            auth_cache_ttl_secs: 300,
        },
        ingest: {
            let wal_dir = tmp.path().join("wal");
            std::fs::create_dir_all(&wal_dir).unwrap();
            IngestConfig {
                wal_dir: Some(wal_dir),
                ..IngestConfig::default()
            }
        },
        retention: RetentionConfig::default(),
    };

    let (state, http_config) =
        AppState::from_config(&config, test_metrics_handle()).expect("failed to create app state");

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
        ingest_token: ingest.plaintext_token.to_string(),
    }
}

#[tokio::test]
async fn health_returns_ok() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, "unused").unwrap();
    let health = client.health().await.unwrap();
    assert_eq!(health.status, fleet_api::HealthStatus::Ok);
}

#[tokio::test]
async fn query_returns_results() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let result = client.query_paginated("*", None, None).await.unwrap();
    assert_eq!(result.result.row_count(), 3);
}

#[tokio::test]
async fn query_with_filter() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let result = client
        .query_paginated("service:nginx", None, None)
        .await
        .unwrap();
    assert_eq!(result.result.row_count(), 2);
}

#[tokio::test]
async fn query_with_stats_pipeline() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let result = client
        .query_paginated("* | stats count() by service", None, None)
        .await
        .unwrap();
    // nginx: 2, postgres: 1 → 2 rows
    assert_eq!(result.result.row_count(), 2);
}

#[tokio::test]
async fn query_rejects_missing_auth() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, "").unwrap();

    let result = client.query_paginated("*", None, None).await;
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

    let result = client.query_paginated("*", None, None).await;
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

    let result = client.query_paginated("| | | broken {{{", None, None).await;
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
    analyst.query_paginated("*", None, None).await.unwrap();

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

// -- ingest endpoint tests ---------------------------------------------------

/// Build a raw reqwest client that accepts self-signed certs.
fn raw_client() -> reqwest::Client {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap()
}

#[tokio::test]
async fn ingest_accepts_ndjson() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.ingest_token).unwrap();

    let records = vec![
        serde_json::json!({"service": "test-svc", "host": "web01", "message": "hello"}),
        serde_json::json!({"service": "test-svc", "host": "web02", "message": "world"}),
    ];

    let resp = client.ingest(&records).await.unwrap();
    assert_eq!(resp.accepted, 2);
}

#[tokio::test]
async fn ingest_rejects_missing_auth() {
    let server = setup().await;
    let client = raw_client();

    let resp = client
        .post(format!("{}/api/v1/ingest", server.url))
        .header("content-type", "application/x-ndjson")
        .body(r#"{"service":"x","message":"y"}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn ingest_rejects_analyst_role() {
    let server = setup().await;
    let client = raw_client();

    let resp = client
        .post(format!("{}/api/v1/ingest", server.url))
        .header("authorization", format!("Bearer {}", server.analyst_token))
        .header("content-type", "application/x-ndjson")
        .body(r#"{"service":"x","message":"y"}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn ingest_partial_success() {
    let server = setup().await;
    let client = raw_client();

    // 3 ndjson events: good, bad json, good
    let body = "{\"service\":\"test-svc\",\"message\":\"one\"}\nnot json\n{\"service\":\"test-svc\",\"message\":\"three\"}";

    let resp = client
        .post(format!("{}/api/v1/ingest", server.url))
        .header("authorization", format!("Bearer {}", server.ingest_token))
        .header("content-type", "application/x-ndjson")
        .body(body)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: fleet_api::IngestResponse = resp.json().await.unwrap();
    assert_eq!(body.accepted, 2);
    assert_eq!(body.rejected, 1);
    assert_eq!(body.errors.len(), 1);
    assert_eq!(body.errors[0].index, 1);
}

#[tokio::test]
async fn ingest_all_rejected_per_event() {
    let server = setup().await;
    let client = raw_client();

    // All 3 events are bad (no service field)
    let body = "{\"message\":\"no svc\"}\n{\"message\":\"also no svc\"}\n{\"message\":\"nope\"}";

    let resp = client
        .post(format!("{}/api/v1/ingest", server.url))
        .header("authorization", format!("Bearer {}", server.ingest_token))
        .header("content-type", "application/x-ndjson")
        .body(body)
        .send()
        .await
        .unwrap();

    // Returns 200 even when all events rejected (per-event, not batch-level).
    assert_eq!(resp.status(), 200);
    let body: fleet_api::IngestResponse = resp.json().await.unwrap();
    assert_eq!(body.accepted, 0);
    assert_eq!(body.rejected, 3);
    assert_eq!(body.errors.len(), 3);
}

// -- rate limit tests --------------------------------------------------------

#[tokio::test]
async fn rate_limit_returns_429() {
    let server = setup_with_rate_limit(RateLimitConfig {
        admin: 0,
        analyst: 2, // burst of 2
        reader: 0,
        ingest: 0,
    })
    .await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    // First 2 should succeed (burst capacity).
    client.query_paginated("*", None, None).await.unwrap();
    client.query_paginated("*", None, None).await.unwrap();

    // 3rd should be rate limited.
    let result = client.query_paginated("*", None, None).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    match err {
        fleet_client::ClientError::Server { status, .. } => {
            assert_eq!(status, 429, "expected 429 Too Many Requests");
        }
        other => panic!("expected 429 rate limit error, got: {other:?}"),
    }
}

// ── new endpoint tests (cancellation, validation, pagination, stats, field values) ──

#[tokio::test]
async fn cancel_query_by_admin() {
    let server = setup().await;
    let analyst = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let admin = HttpClient::new_insecure(&server.url, &server.admin_token).unwrap();

    // Spawn a slow query in the background.
    let analyst_clone = analyst.clone();
    let slow_query = tokio::spawn(async move {
        // This query will take a while (timechart with small span).
        let _ = analyst_clone
            .query_paginated("* | timechart span=1s count()", None, None)
            .await;
    });

    // Give it a moment to start.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Admin cancels query ID 1 (first query).
    // May or may not catch it depending on timing — just verify the endpoint works.
    let _cancel_resp = admin.cancel_query(1).await.unwrap();

    // Wait for the spawned task to finish.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), slow_query).await;
}

#[tokio::test]
async fn cancel_query_nonexistent_returns_false() {
    let server = setup().await;
    let admin = HttpClient::new_insecure(&server.url, &server.admin_token).unwrap();

    let resp = admin.cancel_query(9999).await.unwrap();
    assert!(!resp.cancelled);
    assert_eq!(resp.query_id, 9999);
}

#[tokio::test]
async fn cancel_query_by_non_admin_fails() {
    let server = setup().await;
    let analyst = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    // Analyst tries to cancel a query they don't own.
    let result = analyst.cancel_query(1).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn validate_query_valid() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let resp = client
        .validate("service:nginx | stats count()")
        .await
        .unwrap();
    assert!(resp.valid);
    assert!(resp.errors.is_empty());
}

#[tokio::test]
async fn validate_query_syntax_error() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let resp = client
        .validate("service:nginx | bad_command")
        .await
        .unwrap();
    assert!(!resp.valid);
    assert!(!resp.errors.is_empty());
}

#[tokio::test]
async fn validate_query_unknown_function() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let resp = client.validate("* | stats unknown_func()").await.unwrap();
    assert!(!resp.valid);
    assert!(resp.errors.iter().any(|e| e.message.contains("unknown")));
}

#[tokio::test]
async fn query_pagination_limit_offset() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    // We have 3 rows total. Request 2 rows starting at offset 1.
    let resp = client.query_paginated("*", Some(2), Some(1)).await.unwrap();
    assert_eq!(resp.pagination.limit, 2);
    assert_eq!(resp.pagination.offset, 1);
    assert_eq!(resp.pagination.returned, 2);
    assert_eq!(resp.result.row_count(), 2);
}

#[tokio::test]
async fn query_pagination_offset_beyond_results() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let resp = client
        .query_paginated("*", Some(10), Some(100))
        .await
        .unwrap();
    assert_eq!(resp.pagination.offset, 100);
    assert_eq!(resp.pagination.returned, 0);
    assert_eq!(resp.result.row_count(), 0);
}

#[tokio::test]
async fn query_pagination_defaults() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    // No limit/offset specified — defaults should apply.
    let resp = client.query_paginated("*", None, None).await.unwrap();
    assert_eq!(resp.pagination.offset, 0);
    assert_eq!(resp.pagination.returned, 3);
}

#[tokio::test]
async fn stats_endpoint_admin_only() {
    let server = setup().await;
    let admin = HttpClient::new_insecure(&server.url, &server.admin_token).unwrap();

    let stats = admin.stats().await.unwrap();
    assert!(stats.pool_capacity > 0);
}

#[tokio::test]
async fn stats_endpoint_analyst_forbidden() {
    let server = setup().await;
    let analyst = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let result = analyst.stats().await;
    assert!(result.is_err());
    match result.unwrap_err() {
        fleet_client::ClientError::Server { status, .. } => assert_eq!(status, 401),
        other => panic!("expected 401, got: {other:?}"),
    }
}

#[tokio::test]
async fn field_values_endpoint() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let resp = client.field_values("service", Some(5)).await.unwrap();
    assert_eq!(resp.field, "service");
    assert!(!resp.values.is_empty());
    assert!(resp.values.iter().any(|v| v == "nginx"));
}

#[tokio::test]
async fn field_values_cached() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    // First request should populate cache.
    let resp1 = client.field_values("service", None).await.unwrap();
    assert!(!resp1.cached);

    // Second request should hit cache.
    let resp2 = client.field_values("service", None).await.unwrap();
    assert!(resp2.cached);
}

#[tokio::test]
async fn field_values_invalid_field_name() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let result = client.field_values("bad;name", None).await;
    assert!(result.is_err());
    match result.unwrap_err() {
        fleet_client::ClientError::Server { status, .. } => assert_eq!(status, 400),
        other => panic!("expected 400, got: {other:?}"),
    }
}

// -- request ID tests --------------------------------------------------------

#[tokio::test]
async fn response_includes_ulid_request_id() {
    let server = setup().await;
    let client = raw_client();

    let resp = client
        .get(format!("{}/api/v1/health", server.url))
        .send()
        .await
        .unwrap();

    let header = resp
        .headers()
        .get("x-request-id")
        .expect("missing x-request-id header");
    let value = header.to_str().unwrap();
    assert_eq!(value.len(), 26, "request ID should be a 26-char ULID");
    assert!(
        value.chars().all(|c| c.is_ascii_alphanumeric()),
        "request ID should be alphanumeric crockford base32"
    );
}
