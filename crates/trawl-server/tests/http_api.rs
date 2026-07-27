// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! End-to-end HTTPS API tests for trawld.
//!
//! Starts a real TLS server on a random port with a self-signed cert,
//! creates API keys, and verifies the full request lifecycle.

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use common::{setup, setup_with_rate_limit, trawl_only};
use fleet_auth::{KeyStore, PrincipalKind};
use trawl_client::HttpClient;
use trawl_server::config::RateLimitConfig;
use trawl_server::policy::Role;

#[sqlx::test(migrations = false)]
async fn health_returns_ok(pool: sqlx::PgPool) {
    let server = setup(pool).await;
    let client = HttpClient::new_insecure(&server.url, "unused").unwrap();
    let health = client.health().await.unwrap();
    assert_eq!(health.status, trawl_api::HealthStatus::Ok);
}

#[sqlx::test(migrations = false)]
async fn query_returns_results(pool: sqlx::PgPool) {
    let server = setup(pool).await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let result = client.query_paginated("*", None, None).await.unwrap();
    assert_eq!(result.result.row_count(), 3);
}

#[sqlx::test(migrations = false)]
async fn query_with_filter(pool: sqlx::PgPool) {
    let server = setup(pool).await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let result = client
        .query_paginated("service=nginx", None, None)
        .await
        .unwrap();
    assert_eq!(result.result.row_count(), 2);
}

#[sqlx::test(migrations = false)]
async fn query_with_stats_pipeline(pool: sqlx::PgPool) {
    let server = setup(pool).await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let result = client
        .query_paginated("* | stats count() by service", None, None)
        .await
        .unwrap();
    // nginx: 2, postgres: 1 → 2 rows
    assert_eq!(result.result.row_count(), 2);
}

#[sqlx::test(migrations = false)]
async fn query_rejects_missing_auth(pool: sqlx::PgPool) {
    let server = setup(pool).await;
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

#[sqlx::test(migrations = false)]
async fn query_rejects_invalid_token(pool: sqlx::PgPool) {
    let server = setup(pool).await;
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

#[sqlx::test(migrations = false)]
async fn query_rejects_bad_dsl(pool: sqlx::PgPool) {
    let server = setup(pool).await;
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

#[sqlx::test(migrations = false)]
async fn schema_returns_columns(pool: sqlx::PgPool) {
    let server = setup(pool).await;
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

#[sqlx::test(migrations = false)]
async fn schema_caching_works(pool: sqlx::PgPool) {
    let server = setup(pool).await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let first = client.schema().await.unwrap();
    assert!(!first.cached, "first call should not be cached");

    let second = client.schema().await.unwrap();
    assert!(second.cached, "second call should be cached");
}

// -- queries endpoint tests --------------------------------------------------

#[sqlx::test(migrations = false)]
async fn queries_shows_history(pool: sqlx::PgPool) {
    let server = setup(pool).await;
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

#[sqlx::test(migrations = false)]
async fn queries_accessible_by_analyst_and_reader(pool: sqlx::PgPool) {
    let server = setup(pool).await;
    let analyst = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let reader = HttpClient::new_insecure(&server.url, &server.reader_token).unwrap();

    // Both analyst and reader can list running queries (loosened from admin-only).
    analyst.queries().await.unwrap();
    reader.queries().await.unwrap();
}

#[sqlx::test(migrations = false)]
async fn queries_rejects_ingest_role(pool: sqlx::PgPool) {
    let server = setup(pool).await;
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

#[sqlx::test(migrations = false)]
async fn ingest_accepts_ndjson(pool: sqlx::PgPool) {
    let server = setup(pool).await;
    let client = HttpClient::new_insecure(&server.url, &server.ingest_token).unwrap();

    let records = vec![
        serde_json::json!({"service": "test-svc", "host": "web01", "message": "hello"}),
        serde_json::json!({"service": "test-svc", "host": "web02", "message": "world"}),
    ];

    let resp = client.ingest(&records).await.unwrap();
    assert_eq!(resp.accepted, 2);
}

#[sqlx::test(migrations = false)]
async fn ingest_rejects_missing_auth(pool: sqlx::PgPool) {
    let server = setup(pool).await;
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

#[sqlx::test(migrations = false)]
async fn ingest_rejects_analyst_role(pool: sqlx::PgPool) {
    let server = setup(pool).await;
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

#[sqlx::test(migrations = false)]
async fn ingest_partial_success(pool: sqlx::PgPool) {
    let server = setup(pool).await;
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

#[sqlx::test(migrations = false)]
async fn ingest_all_rejected_per_event(pool: sqlx::PgPool) {
    let server = setup(pool).await;
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

#[sqlx::test(migrations = false)]
async fn rate_limit_returns_429(pool: sqlx::PgPool) {
    let server = setup_with_rate_limit(
        pool,
        RateLimitConfig {
            default_rpm: 2, // burst of 2
            ..RateLimitConfig::default()
        },
    )
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

#[sqlx::test(migrations = false)]
async fn cancel_query_by_admin(pool: sqlx::PgPool) {
    let server = setup(pool).await;
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

#[sqlx::test(migrations = false)]
async fn cancel_query_nonexistent_returns_false(pool: sqlx::PgPool) {
    let server = setup(pool).await;
    let admin = HttpClient::new_insecure(&server.url, &server.admin_token).unwrap();

    let resp = admin.cancel_query(9999).await.unwrap();
    assert!(!resp.cancelled);
    assert_eq!(resp.query_id, 9999);
}

#[sqlx::test(migrations = false)]
async fn cancel_query_by_analyst_for_nonexistent(pool: sqlx::PgPool) {
    let server = setup(pool).await;
    let analyst = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    // Analyst gets "cannot cancel" for non-existent queries — no information
    // disclosure about whether the query ID exists (only admin sees the difference).
    let result = analyst.cancel_query(9999).await;
    assert!(result.is_err());
}

#[sqlx::test(migrations = false)]
async fn cancel_query_rejects_ingest_role(pool: sqlx::PgPool) {
    let server = setup(pool).await;
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

/// Non-admin cancellation is authorized by exact keystore id, not display
/// name. Two `QueryCancel` keys sharing a name ("twin") must stay isolated:
/// key B cannot cancel key A's in-flight query, while owner A passes the gate
/// and actually interrupts the query.
///
/// A revert to name-matching would let B (same name) through, so this test
/// guards that regression. It needs a genuinely *active* query — a completed
/// one has no tracked owner under either scheme, so only a live entry can tell
/// id-matching from name-matching apart.
///
/// The pool's `TEST_QUERY_DELAY_MS` hook (via the crate's `test-support`
/// feature) holds the query in-flight deterministically — no dataset-size
/// timing bets against fast CI runners.
#[sqlx::test(migrations = false)]
async fn cancel_query_isolated_by_key_id_not_name(pool: sqlx::PgPool) {
    // Permissive rate limits: the observe/cancel polls below run in a tight
    // window and must not trip the per-minute buckets.
    let permissive = RateLimitConfig {
        default_rpm: 1_000_000,
        ..RateLimitConfig::default()
    };
    let server = setup_with_rate_limit(pool, permissive).await;

    // Hold every pool query open long enough to observe and cancel it.
    // nextest runs each test in its own process, so the global is private
    // to this test; reset at the end regardless.
    trawl_server::pool::TEST_QUERY_DELAY_MS.store(3_000, Ordering::Relaxed);

    // Two analyst keys with the SAME display name but distinct keystore ids.
    let store = KeyStore::from_pool(server.fleet_pool.clone());
    let key_a = store
        .create_key(
            "twin",
            PrincipalKind::Service,
            &trawl_only(Role::Analyst),
            None,
        )
        .await
        .unwrap();
    let key_b = store
        .create_key(
            "twin",
            PrincipalKind::Service,
            &trawl_only(Role::Analyst),
            None,
        )
        .await
        .unwrap();
    assert_ne!(
        key_a.info.id, key_b.info.id,
        "twin keys must have distinct keystore ids"
    );

    let a = HttpClient::new_insecure(&server.url, key_a.plaintext_token.as_str()).unwrap();
    let b = HttpClient::new_insecure(&server.url, key_b.plaintext_token.as_str()).unwrap();

    // Fire a query as key A in the background; the injected delay keeps it
    // tracked as active until we are done asserting.
    let slow = {
        let a = a.clone();
        tokio::spawn(async move { a.query_paginated("* | stats count()", None, None).await })
    };

    // Wait until A's query shows up as active, capturing its id.
    let mut target = None;
    for _ in 0..500 {
        if let Ok(resp) = a.queries().await
            && let Some(id) = resp
                .active
                .iter()
                .filter(|q| q.user == "twin")
                .map(|q| q.id)
                .max()
        {
            target = Some(id);
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let target = target.expect("key A should have an active query");

    // Key B (same name, QueryCancel, different id) is rejected by the
    // ownership gate — the query is guaranteed still in-flight here.
    let denied = b.cancel_query(target).await;
    match denied {
        Err(trawl_client::ClientError::Server { status, .. }) => {
            assert_eq!(status, 401, "twin key B must not cancel key A's query");
        }
        other => panic!("expected 401 for non-owner cancel, got: {other:?}"),
    }

    // Owner A passes the ownership gate AND interrupts the tracked query:
    // `cancelled == true` proves the tracker id and the pool's interrupt-map
    // id are the same id space. The interrupt handle is registered by the
    // pool task shortly after the tracker entry appears, so retry briefly.
    let mut cancelled = false;
    for _ in 0..500 {
        let resp = a
            .cancel_query(target)
            .await
            .expect("owner key A must pass the ownership gate");
        if resp.cancelled {
            cancelled = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert!(
        cancelled,
        "owner cancel must interrupt the in-flight query (shared id space)"
    );

    trawl_server::pool::TEST_QUERY_DELAY_MS.store(0, Ordering::Relaxed);

    // The interrupted query surfaces an error to its submitter.
    let _ = slow.await;
}

#[sqlx::test(migrations = false)]
async fn validate_query_valid(pool: sqlx::PgPool) {
    let server = setup(pool).await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let resp = client
        .validate("service=nginx | stats count()")
        .await
        .unwrap();
    assert!(resp.valid);
    assert!(resp.errors.is_empty());
}

#[sqlx::test(migrations = false)]
async fn validate_query_syntax_error(pool: sqlx::PgPool) {
    let server = setup(pool).await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let resp = client
        .validate("service=nginx | bad_command")
        .await
        .unwrap();
    assert!(!resp.valid);
    assert!(!resp.errors.is_empty());
}

#[sqlx::test(migrations = false)]
async fn validate_query_unknown_function(pool: sqlx::PgPool) {
    let server = setup(pool).await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let resp = client.validate("* | stats unknown_func()").await.unwrap();
    assert!(!resp.valid);
    assert!(resp.errors.iter().any(|e| e.message.contains("unknown")));
}

#[sqlx::test(migrations = false)]
async fn query_pagination_limit_offset(pool: sqlx::PgPool) {
    let server = setup(pool).await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    // We have 3 rows total. Request 2 rows starting at offset 1.
    let resp = client.query_paginated("*", Some(2), Some(1)).await.unwrap();
    assert_eq!(resp.pagination.limit, 2);
    assert_eq!(resp.pagination.offset, 1);
    assert_eq!(resp.pagination.returned, 2);
    assert_eq!(resp.result.row_count(), 2);
}

#[sqlx::test(migrations = false)]
async fn query_pagination_offset_beyond_results(pool: sqlx::PgPool) {
    let server = setup(pool).await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let resp = client
        .query_paginated("*", Some(10), Some(100))
        .await
        .unwrap();
    assert_eq!(resp.pagination.offset, 100);
    assert_eq!(resp.pagination.returned, 0);
    assert_eq!(resp.result.row_count(), 0);
}

#[sqlx::test(migrations = false)]
async fn query_pagination_defaults(pool: sqlx::PgPool) {
    let server = setup(pool).await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    // No limit/offset specified — defaults should apply.
    let resp = client.query_paginated("*", None, None).await.unwrap();
    assert_eq!(resp.pagination.offset, 0);
    assert_eq!(resp.pagination.returned, 3);
}

#[sqlx::test(migrations = false)]
async fn stats_endpoint_admin_only(pool: sqlx::PgPool) {
    let server = setup(pool).await;
    let admin = HttpClient::new_insecure(&server.url, &server.admin_token).unwrap();

    let stats = admin.stats().await.unwrap();
    assert!(stats.pool_capacity > 0);
}

#[sqlx::test(migrations = false)]
async fn stats_endpoint_analyst_forbidden(pool: sqlx::PgPool) {
    let server = setup(pool).await;
    let analyst = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let result = analyst.stats().await;
    assert!(result.is_err());
    match result.unwrap_err() {
        trawl_client::ClientError::Server { status, .. } => assert_eq!(status, 401),
        other => panic!("expected 401, got: {other:?}"),
    }
}

#[sqlx::test(migrations = false)]
async fn dashboard_rejects_analyst(pool: sqlx::PgPool) {
    let server = setup(pool).await;
    let analyst = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let result = analyst.dashboard().await;
    assert!(result.is_err());
    match result.unwrap_err() {
        trawl_client::ClientError::Server { status, .. } => assert_eq!(status, 401),
        other => panic!("expected 401, got: {other:?}"),
    }
}

#[sqlx::test(migrations = false)]
async fn dashboard_becomes_available_once_collector_ticks(pool: sqlx::PgPool) {
    // The harness spawns the snapshot collector (same as trawld's main());
    // before its first tick the endpoint is 503 (snapshot is None), after
    // it the snapshot serves. Poll briefly.
    let server = setup(pool).await;
    let admin = HttpClient::new_insecure(&server.url, &server.admin_token).unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match admin.dashboard().await {
            Ok(snapshot) => {
                assert!(snapshot.pool_capacity > 0);
                return;
            }
            Err(trawl_client::ClientError::Server { status: 503, .. }) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "dashboard never became available"
                );
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }
}

#[sqlx::test(migrations = false)]
async fn dashboard_stream_rejects_analyst(pool: sqlx::PgPool) {
    let server = setup(pool).await;
    let client = raw_client();

    let resp = client
        .get(format!("{}/api/v1/dashboard/stream", server.url))
        .header("authorization", format!("Bearer {}", server.analyst_token))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 401);
}

#[sqlx::test(migrations = false)]
async fn dashboard_stream_emits_stats_event(pool: sqlx::PgPool) {
    let server = setup(pool).await;
    let client = raw_client();

    let resp = client
        .get(format!("{}/api/v1/dashboard/stream", server.url))
        .header("authorization", format!("Bearer {}", server.admin_token))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        content_type.starts_with("text/event-stream"),
        "unexpected content-type: {content_type}"
    );

    // The handler skips ticks until the collector's first snapshot lands,
    // so read chunks until a full `stats` frame arrives (bounded overall).
    let mut resp = resp;
    let frame = tokio::time::timeout(Duration::from_secs(10), async {
        let mut buf = String::new();
        loop {
            let chunk = resp.chunk().await.unwrap().expect("stream ended early");
            buf.push_str(&String::from_utf8_lossy(&chunk));
            if let Some(start) = buf.find("event: stats")
                && let Some(end) = buf[start..].find("\n\n")
            {
                return buf[start..start + end].to_owned();
            }
        }
    })
    .await
    .expect("no stats event within 10s");

    let data_line = frame
        .lines()
        .find_map(|l| l.strip_prefix("data: "))
        .expect("stats frame missing data line");
    let snapshot: trawl_api::DashboardSnapshot = serde_json::from_str(data_line).unwrap();
    assert!(snapshot.pool_capacity > 0);
}

#[sqlx::test(migrations = false)]
async fn whoami_admin_has_server_manage(pool: sqlx::PgPool) {
    let server = setup(pool).await;
    let admin = HttpClient::new_insecure(&server.url, &server.admin_token).unwrap();

    let resp = admin.whoami().await.unwrap();
    assert_eq!(resp.role_for("trawl"), Some("admin"));
    assert!(resp.permissions.contains(&"server_manage".to_owned()));
    assert!(resp.permissions.contains(&"query".to_owned()));
    // prefix is the stable 8-char fingerprint of the key; downstream
    // consumers (e.g. coastwatch) key audit records off it, so the
    // endpoint must surface it verbatim.
    assert_eq!(resp.prefix.len(), 8, "prefix must be exactly 8 chars");
    assert!(
        resp.prefix
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "prefix must be base64url charset: got {}",
        resp.prefix
    );
}

#[sqlx::test(migrations = false)]
async fn whoami_reader_lacks_server_manage(pool: sqlx::PgPool) {
    let server = setup(pool).await;
    let reader = HttpClient::new_insecure(&server.url, &server.reader_token).unwrap();

    let resp = reader.whoami().await.unwrap();
    assert_eq!(resp.role_for("trawl"), Some("reader"));
    assert!(!resp.permissions.contains(&"server_manage".to_owned()));
    assert!(resp.permissions.contains(&"query".to_owned()));
}

#[sqlx::test(migrations = false)]
async fn whoami_rejects_missing_auth(pool: sqlx::PgPool) {
    let server = setup(pool).await;
    let client = HttpClient::new_insecure(&server.url, "invalid-token").unwrap();

    let result = client.whoami().await;
    assert!(result.is_err());
}

#[sqlx::test(migrations = false)]
async fn whoami_no_trawl_grant_is_403(pool: sqlx::PgPool) {
    // Policy change with the fleet-auth cutover (ADR-0004): a foreign-app-only
    // key is rejected by the mandatory trawl policy layer on EVERY
    // authenticated route — including /whoami, which previously leaked
    // cross-app assignments to grantless keys.
    let server = setup(pool).await;
    let client = HttpClient::new_insecure(&server.url, &server.coastwatch_only_token).unwrap();

    let result = client.whoami().await;
    match result.unwrap_err() {
        trawl_client::ClientError::Server { status, .. } => assert_eq!(status, 403),
        other => panic!("expected 403, got: {other:?}"),
    }
}

#[sqlx::test(migrations = false)]
async fn field_values_endpoint(pool: sqlx::PgPool) {
    let server = setup(pool).await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let resp = client.field_values("service", Some(5), None).await.unwrap();
    assert_eq!(resp.field, "service");
    assert!(!resp.values.is_empty());
    assert!(resp.values.iter().any(|v| v == "nginx"));
}

#[sqlx::test(migrations = false)]
async fn field_values_cached(pool: sqlx::PgPool) {
    let server = setup(pool).await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    // First request should populate cache.
    let resp1 = client.field_values("service", None, None).await.unwrap();
    assert!(!resp1.cached);

    // Second request should hit cache.
    let resp2 = client.field_values("service", None, None).await.unwrap();
    assert!(resp2.cached);
}

#[sqlx::test(migrations = false)]
async fn field_values_invalid_field_name(pool: sqlx::PgPool) {
    let server = setup(pool).await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let result = client.field_values("bad;name", None, None).await;
    assert!(result.is_err());
    match result.unwrap_err() {
        trawl_client::ClientError::Server { status, .. } => assert_eq!(status, 400),
        other => panic!("expected 400, got: {other:?}"),
    }
}

// -- request ID tests --------------------------------------------------------

#[sqlx::test(migrations = false)]
async fn response_includes_ulid_request_id(pool: sqlx::PgPool) {
    let server = setup(pool).await;
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

#[sqlx::test(migrations = false)]
async fn validate_rejects_reader(pool: sqlx::PgPool) {
    let server = setup(pool).await;
    let reader = HttpClient::new_insecure(&server.url, &server.reader_token).unwrap();

    assert_401(reader.validate("* | head 1").await);
}

#[sqlx::test(migrations = false)]
async fn saved_queries_reject_reader(pool: sqlx::PgPool) {
    let server = setup(pool).await;
    let reader = HttpClient::new_insecure(&server.url, &server.reader_token).unwrap();

    assert_401(reader.list_saved().await);
    assert_401(reader.create_saved("test", "* | head 1").await);
    assert_401(reader.update_saved(1, "* | head 2").await);
    assert_401(reader.delete_saved(1).await);
}

#[sqlx::test(migrations = false)]
async fn export_rejects_reader(pool: sqlx::PgPool) {
    let server = setup(pool).await;
    let reader = HttpClient::new_insecure(&server.url, &server.reader_token).unwrap();

    assert_401(
        reader
            .export("* | head 1", trawl_api::ExportFormat::Csv, None)
            .await,
    );
}

#[sqlx::test(migrations = false)]
async fn reader_can_query_and_view_history(pool: sqlx::PgPool) {
    let server = setup(pool).await;
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

// ---------------------------------------------------------------------------
// Runs stats endpoint
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = false)]
async fn runs_stats_empty(pool: sqlx::PgPool) {
    let server = setup(pool).await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let stats = client.runs_stats().await.unwrap();
    assert_eq!(stats.total_runs, 0);
    assert_eq!(stats.success_count, 0);
    assert_eq!(stats.error_count, 0);
    assert_eq!(stats.timeout_count, 0);
    assert_eq!(stats.avg_duration_ms, None);
}

#[sqlx::test(migrations = false)]
async fn runs_stats_rejects_reader(pool: sqlx::PgPool) {
    let server = setup(pool).await;
    let reader = HttpClient::new_insecure(&server.url, &server.reader_token).unwrap();

    assert_401(reader.runs_stats().await);
}

// ---------------------------------------------------------------------------
// Trigger run endpoint
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = false)]
async fn trigger_run_requires_schedule(pool: sqlx::PgPool) {
    let server = setup(pool).await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    // Create a net without a schedule.
    let saved = client
        .create_saved("trigger-test", "* | head 5")
        .await
        .unwrap();

    // Triggering should fail with 400 (no schedule attached).
    let err = client
        .trigger_run(saved.id)
        .await
        .expect_err("expected error");
    match err {
        trawl_client::ClientError::Server { status, .. } => {
            assert_eq!(status, 400);
        }
        other => panic!("expected 400, got: {other:?}"),
    }
}

#[sqlx::test(migrations = false)]
async fn trigger_run_starts_execution(pool: sqlx::PgPool) {
    let server = setup(pool).await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    // Create net + attach schedule.
    let saved = client
        .create_saved("trigger-exec", "* | head 3")
        .await
        .unwrap();
    client
        .set_schedule(saved.id, "1h", None, true)
        .await
        .unwrap();

    // Trigger the run.
    let summary = client.trigger_run(saved.id).await.unwrap();
    assert_eq!(summary.status, "running");
    assert_eq!(summary.query, "* | head 3");

    // Wait briefly for execution to complete (test fixtures are tiny).
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Verify the run completed.
    let runs = client
        .list_report_runs(saved.id, Some(10), None)
        .await
        .unwrap();
    assert_eq!(runs.runs.len(), 1);
    assert_eq!(runs.runs[0].status, "success");

    // Stats should reflect the completed run.
    let stats = client.runs_stats().await.unwrap();
    assert_eq!(stats.total_runs, 1);
    assert_eq!(stats.success_count, 1);
    assert!(stats.avg_duration_ms.is_some());
}

#[sqlx::test(migrations = false)]
async fn trigger_run_rejects_reader(pool: sqlx::PgPool) {
    let server = setup(pool).await;
    let reader = HttpClient::new_insecure(&server.url, &server.reader_token).unwrap();

    assert_401(reader.trigger_run(1).await);
}

#[sqlx::test(migrations = false)]
async fn trigger_run_rejects_when_max_runs_reached(pool: sqlx::PgPool) {
    use trawl_server::store::schedule::{RunClaim, ScheduleStore};

    let server = setup(pool).await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let saved = client
        .create_saved("trigger-cap", "* | head 3")
        .await
        .unwrap();
    let schedule = client
        .set_schedule(saved.id, "1h", Some(1), true)
        .await
        .unwrap();

    // Seed a run directly so the schedule is already at its max_runs=1 cap;
    // count(*) >= max_runs short-circuits the claim before the insert.
    let store = ScheduleStore::new(sqlx::PgPool::connect(&server.app_db_url).await.unwrap());
    let seeded = store
        .claim_run(schedule.id, saved.id, "* | head 3", None)
        .await
        .unwrap();
    assert!(
        matches!(seeded, RunClaim::Started(_)),
        "seeding the cap must start a run, got {seeded:?}"
    );

    // Triggering again exceeds the cap -> 400 "max runs reached".
    let err = client
        .trigger_run(saved.id)
        .await
        .expect_err("expected max-runs error");
    match err {
        trawl_client::ClientError::Server { status, error } => {
            assert_eq!(status, 400);
            assert!(
                error.message.contains("max runs reached"),
                "unexpected message: {}",
                error.message
            );
        }
        other => panic!("expected 400, got: {other:?}"),
    }
}

#[sqlx::test(migrations = false)]
async fn trigger_run_rejects_when_already_running(pool: sqlx::PgPool) {
    use trawl_server::store::schedule::{RunClaim, ScheduleStore};

    let server = setup(pool).await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let saved = client
        .create_saved("trigger-busy", "* | head 3")
        .await
        .unwrap();
    // No max_runs cap, so the in-progress guard is what rejects the trigger.
    let schedule = client
        .set_schedule(saved.id, "1h", None, true)
        .await
        .unwrap();

    // Seed an in-progress run directly, avoiding a race with the background
    // task the happy-path trigger spawns.
    let store = ScheduleStore::new(sqlx::PgPool::connect(&server.app_db_url).await.unwrap());
    let seeded = store
        .claim_run(schedule.id, saved.id, "* | head 3", None)
        .await
        .unwrap();
    assert!(
        matches!(seeded, RunClaim::Started(_)),
        "seeding an in-progress run must start it, got {seeded:?}"
    );

    // Triggering while a run is in progress -> 400 "already in progress".
    let err = client
        .trigger_run(saved.id)
        .await
        .expect_err("expected already-running error");
    match err {
        trawl_client::ClientError::Server { status, error } => {
            assert_eq!(status, 400);
            assert!(
                error.message.contains("already in progress"),
                "unexpected message: {}",
                error.message
            );
        }
        other => panic!("expected 400, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Rename net (update with name)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = false)]
async fn rename_saved_query(pool: sqlx::PgPool) {
    let server = setup(pool).await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let saved = client.create_saved("old-name", "* | head 1").await.unwrap();

    // Rename it.
    let updated = client
        .update_saved_with_name(saved.id, "* | head 1", Some("new-name"))
        .await
        .unwrap();
    assert_eq!(updated.name, "new-name");
    assert_eq!(updated.query, "* | head 1");

    // Verify via list.
    let list = client.list_saved().await.unwrap();
    assert!(list.queries.iter().any(|q| q.name == "new-name"));
    assert!(!list.queries.iter().any(|q| q.name == "old-name"));
}

#[sqlx::test(migrations = false)]
async fn rename_to_duplicate_fails(pool: sqlx::PgPool) {
    let server = setup(pool).await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    client
        .create_saved("taken-name", "* | head 1")
        .await
        .unwrap();
    let other = client
        .create_saved("other-name", "* | head 2")
        .await
        .unwrap();

    // Try to rename `other` to the taken name. Conflicts are 409 since the
    // pg cutover (ADR-0004 StoreError -> HTTP table).
    let err = client
        .update_saved_with_name(other.id, "* | head 2", Some("taken-name"))
        .await
        .expect_err("expected conflict");
    match err {
        trawl_client::ClientError::Server { status, .. } => {
            assert_eq!(status, 409);
        }
        other => panic!("expected 409, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// List all runs endpoint
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = false)]
async fn list_all_runs_paginated(pool: sqlx::PgPool) {
    let server = setup(pool).await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    // Empty initially.
    let resp = client.list_all_runs(Some(10), None).await.unwrap();
    assert_eq!(resp.total, 0);
    assert!(resp.runs.is_empty());
}
