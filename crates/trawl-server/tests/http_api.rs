// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! End-to-end HTTPS API tests for trawld.
//!
//! Starts a real TLS server on a random port with a self-signed cert,
//! creates API keys, and verifies the full request lifecycle.

use std::net::TcpListener;
use std::path::PathBuf;

use trawl_auth::roles::Role;
use trawl_auth::store::KeyStore;
use trawl_client::HttpClient;
use trawl_server::config::{
    AuthConfig, Config, DataConfig, IngestConfig, RateLimitConfig, RetentionConfig,
    SchedulerConfig, ServerConfig, SyslogConfig,
};
use trawl_server::state::AppState;
use trawl_server::transport::http;

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
/// Writes fixtures to a stable path under `CARGO_MANIFEST_DIR` so all
/// nextest processes share the same files. Uses PID-unique temp files
/// and atomic rename for race-free coordination.
fn ensure_fixtures() -> String {
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("parquet");
    let nginx_path = dir.join("nginx.parquet");

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
fn ensure_test_cert() -> (PathBuf, PathBuf) {
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
struct TestServer {
    url: String,
    analyst_token: String,
    admin_token: String,
    reader_token: String,
    ingest_token: String,
}

/// Poll the health endpoint until the server is ready (up to 1s).
async fn wait_for_ready(addr: &str) {
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
async fn setup_with_rate_limit(rate_limit: RateLimitConfig) -> TestServer {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    let data_glob = ensure_fixtures();
    let auth_db = tmp.path().join("auth.db");

    let store = KeyStore::open(&auth_db).unwrap();
    let analyst = store.create_key("test-key", Role::Analyst, None).unwrap();
    let admin = store.create_key("admin-key", Role::Admin, None).unwrap();
    let reader = store.create_key("reader-key", Role::Reader, None).unwrap();
    let ingest = store.create_key("ingest-key", Role::Ingest, None).unwrap();
    drop(store);

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
        scheduler: SchedulerConfig::default(),
        syslog: SyslogConfig::default(),
    };

    let (state, http_config) =
        AppState::from_config(&config, test_metrics_handle()).expect("failed to create app state");
    let server_config = config.server.clone();
    let state_dir = config.state_dir();
    tokio::spawn(async move {
        http::serve(state, &http_config, &server_config, &state_dir, None)
            .await
            .unwrap();
    });
    wait_for_ready(&addr).await;
    std::mem::forget(tmp);

    TestServer {
        url: format!("https://{addr}"),
        analyst_token: analyst.plaintext_token.to_string(),
        admin_token: admin.plaintext_token.to_string(),
        reader_token: reader.plaintext_token.to_string(),
        ingest_token: ingest.plaintext_token.to_string(),
    }
}

/// Set up a test server with fixtures and return a `TestServer` handle.
async fn setup() -> TestServer {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    let data_glob = ensure_fixtures();
    let auth_db = tmp.path().join("auth.db");

    // Create API keys for all test roles.
    let store = KeyStore::open(&auth_db).unwrap();
    let analyst = store.create_key("test-key", Role::Analyst, None).unwrap();
    let admin = store.create_key("admin-key", Role::Admin, None).unwrap();
    let reader = store.create_key("reader-key", Role::Reader, None).unwrap();
    let ingest = store.create_key("ingest-key", Role::Ingest, None).unwrap();
    drop(store);

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
            rate_limit: RateLimitConfig::default(),
            monitor_refresh_ms: 1000,
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
        scheduler: SchedulerConfig::default(),
        syslog: SyslogConfig::default(),
    };

    let (state, http_config) =
        AppState::from_config(&config, test_metrics_handle()).expect("failed to create app state");

    // Spawn the HTTPS server in a background task.
    let server_config = config.server.clone();
    let state_dir = config.state_dir();
    tokio::spawn(async move {
        http::serve(state, &http_config, &server_config, &state_dir, None)
            .await
            .unwrap();
    });

    wait_for_ready(&addr).await;

    // Leak the tempdir so it survives the test (cleaned up by OS).
    std::mem::forget(tmp);

    TestServer {
        url: format!("https://{addr}"),
        analyst_token: analyst.plaintext_token.to_string(),
        admin_token: admin.plaintext_token.to_string(),
        reader_token: reader.plaintext_token.to_string(),
        ingest_token: ingest.plaintext_token.to_string(),
    }
}

#[tokio::test]
async fn health_returns_ok() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, "unused").unwrap();
    let health = client.health().await.unwrap();
    assert_eq!(health.status, trawl_api::HealthStatus::Ok);
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
        .query_paginated("service=nginx", None, None)
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
        trawl_client::ClientError::Server { status, .. } => {
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
        trawl_client::ClientError::Server { status, .. } => {
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
        trawl_client::ClientError::Server { status, .. } => {
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
async fn queries_accessible_by_analyst_and_reader() {
    let server = setup().await;
    let analyst = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let reader = HttpClient::new_insecure(&server.url, &server.reader_token).unwrap();

    // Both analyst and reader can list running queries (loosened from admin-only).
    analyst.queries().await.unwrap();
    reader.queries().await.unwrap();
}

#[tokio::test]
async fn queries_rejects_ingest_role() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.ingest_token).unwrap();

    let result = client.queries().await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    match err {
        trawl_client::ClientError::Server { status, .. } => {
            assert_eq!(status, 401);
        }
        other => panic!("expected 401 for ingest role, got: {other:?}"),
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
    let body: trawl_api::IngestResponse = resp.json().await.unwrap();
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
    let body: trawl_api::IngestResponse = resp.json().await.unwrap();
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
        trawl_client::ClientError::Server { status, .. } => {
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
async fn cancel_query_by_analyst_for_nonexistent() {
    let server = setup().await;
    let analyst = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    // Analyst gets "cannot cancel" for non-existent queries — no information
    // disclosure about whether the query ID exists (only admin sees the difference).
    let result = analyst.cancel_query(9999).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn cancel_query_rejects_ingest_role() {
    let server = setup().await;
    let ingest = HttpClient::new_insecure(&server.url, &server.ingest_token).unwrap();

    let result = ingest.cancel_query(1).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    match err {
        trawl_client::ClientError::Server { status, .. } => {
            assert_eq!(status, 401);
        }
        other => panic!("expected 401 for ingest role, got: {other:?}"),
    }
}

#[tokio::test]
async fn validate_query_valid() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let resp = client
        .validate("service=nginx | stats count()")
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
        .validate("service=nginx | bad_command")
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
        trawl_client::ClientError::Server { status, .. } => assert_eq!(status, 401),
        other => panic!("expected 401, got: {other:?}"),
    }
}

#[tokio::test]
async fn dashboard_rejects_analyst() {
    let server = setup().await;
    let analyst = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let result = analyst.dashboard().await;
    assert!(result.is_err());
    match result.unwrap_err() {
        trawl_client::ClientError::Server { status, .. } => assert_eq!(status, 401),
        other => panic!("expected 401, got: {other:?}"),
    }
}

#[tokio::test]
async fn dashboard_returns_503_before_collector_runs() {
    // Test harness doesn't spawn the snapshot collector, so the endpoint
    // returns 503 Service Unavailable (snapshot is None).
    let server = setup().await;
    let admin = HttpClient::new_insecure(&server.url, &server.admin_token).unwrap();

    let result = admin.dashboard().await;
    assert!(result.is_err());
    match result.unwrap_err() {
        trawl_client::ClientError::Server { status, .. } => assert_eq!(status, 503),
        other => panic!("expected 503, got: {other:?}"),
    }
}

#[tokio::test]
async fn whoami_admin_has_server_manage() {
    let server = setup().await;
    let admin = HttpClient::new_insecure(&server.url, &server.admin_token).unwrap();

    let resp = admin.whoami().await.unwrap();
    assert_eq!(resp.role, "admin");
    assert!(resp.permissions.contains(&"server_manage".to_owned()));
    assert!(resp.permissions.contains(&"query".to_owned()));
}

#[tokio::test]
async fn whoami_reader_lacks_server_manage() {
    let server = setup().await;
    let reader = HttpClient::new_insecure(&server.url, &server.reader_token).unwrap();

    let resp = reader.whoami().await.unwrap();
    assert_eq!(resp.role, "reader");
    assert!(!resp.permissions.contains(&"server_manage".to_owned()));
    assert!(resp.permissions.contains(&"query".to_owned()));
}

#[tokio::test]
async fn whoami_rejects_missing_auth() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, "invalid-token").unwrap();

    let result = client.whoami().await;
    assert!(result.is_err());
}

#[tokio::test]
async fn field_values_endpoint() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let resp = client.field_values("service", Some(5), None).await.unwrap();
    assert_eq!(resp.field, "service");
    assert!(!resp.values.is_empty());
    assert!(resp.values.iter().any(|v| v == "nginx"));
}

#[tokio::test]
async fn field_values_cached() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    // First request should populate cache.
    let resp1 = client.field_values("service", None, None).await.unwrap();
    assert!(!resp1.cached);

    // Second request should hit cache.
    let resp2 = client.field_values("service", None, None).await.unwrap();
    assert!(resp2.cached);
}

#[tokio::test]
async fn field_values_invalid_field_name() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let result = client.field_values("bad;name", None, None).await;
    assert!(result.is_err());
    match result.unwrap_err() {
        trawl_client::ClientError::Server { status, .. } => assert_eq!(status, 400),
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

// -- reader role restriction tests -------------------------------------------

/// Helper to assert a client call returns HTTP 401.
fn assert_401<T: std::fmt::Debug>(result: Result<T, trawl_client::ClientError>) {
    let err = result.expect_err("expected 401 but got success");
    match err {
        trawl_client::ClientError::Server { status, .. } => {
            assert_eq!(status, 401, "expected 401, got {status}");
        }
        other => panic!("expected Server error with 401, got: {other:?}"),
    }
}

#[tokio::test]
async fn validate_rejects_reader() {
    let server = setup().await;
    let reader = HttpClient::new_insecure(&server.url, &server.reader_token).unwrap();

    assert_401(reader.validate("* | head 1").await);
}

#[tokio::test]
async fn saved_queries_reject_reader() {
    let server = setup().await;
    let reader = HttpClient::new_insecure(&server.url, &server.reader_token).unwrap();

    assert_401(reader.list_saved().await);
    assert_401(reader.create_saved("test", "* | head 1").await);
    assert_401(reader.update_saved(1, "* | head 2").await);
    assert_401(reader.delete_saved(1).await);
}

#[tokio::test]
async fn export_rejects_reader() {
    let server = setup().await;
    let reader = HttpClient::new_insecure(&server.url, &server.reader_token).unwrap();

    assert_401(
        reader
            .export("* | head 1", trawl_api::ExportFormat::Csv, None)
            .await,
    );
}

#[tokio::test]
async fn reader_can_query_and_view_history() {
    let server = setup().await;
    let reader = HttpClient::new_insecure(&server.url, &server.reader_token).unwrap();

    // Reader can execute queries.
    let resp = reader
        .query_paginated("* | head 1", None, None)
        .await
        .unwrap();
    assert!(!resp.result.columns.is_empty());

    // Reader can view schema.
    reader.schema().await.unwrap();

    // Reader can view history.
    reader.history(Some(10), None).await.unwrap();
}
