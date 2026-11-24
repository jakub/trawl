// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pg-backed auth integration matrix over the fleet-auth keystore
//! (ADR-0004).

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{roles, seed_data_root, setup};
use fleet_auth::{KeyStore, PrincipalKind};
use parking_lot::Mutex;
use sqlx::PgPool;
use trawl_server::audit;
use trawl_server::config::RateLimitConfig;

fn raw_client() -> reqwest::Client {
    common::harness_client_builder().build().unwrap()
}

async fn request(
    server_url: &str,
    method: reqwest::Method,
    path: &str,
    token: Option<&str>,
) -> (u16, serde_json::Value) {
    let client = raw_client();
    let mut req = client.request(method, format!("{server_url}{path}"));
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    // Harmless JSON body for POST/PUT endpoints that parse one.
    req = req
        .header("content-type", "application/json")
        .body(r#"{"query":"* | head 1","name":"x","interval":"1h"}"#);
    let resp = req.send().await.unwrap_or_else(|error| {
        panic!(
            "request with {} path bytes failed: {}",
            path.len(),
            error.without_url()
        )
    });
    let status = resp.status().as_u16();
    let body = resp.json::<serde_json::Value>().await.unwrap_or_default();
    (status, body)
}

fn authenticated_routes() -> Vec<(reqwest::Method, &'static str)> {
    use reqwest::Method;
    vec![
        (Method::POST, "/api/v1/query"),
        (Method::POST, "/api/v1/validate"),
        (Method::GET, "/api/v1/schema"),
        (Method::GET, "/api/v1/schema/services"),
        (Method::GET, "/api/v1/schema/values/service"),
        (Method::GET, "/api/v1/queries"),
        (Method::DELETE, "/api/v1/queries/1"),
        (Method::GET, "/api/v1/stats"),
        (Method::GET, "/api/v1/dashboard"),
        (Method::GET, "/api/v1/whoami"),
        (Method::GET, "/api/v1/history"),
        (Method::DELETE, "/api/v1/history"),
        (Method::GET, "/api/v1/saved"),
        (Method::POST, "/api/v1/saved"),
        (Method::PUT, "/api/v1/saved/1"),
        (Method::DELETE, "/api/v1/saved/1"),
        (Method::PUT, "/api/v1/saved/1/schedule"),
        (Method::GET, "/api/v1/saved/1/schedule"),
        (Method::DELETE, "/api/v1/saved/1/schedule"),
        (Method::GET, "/api/v1/runs"),
        (Method::GET, "/api/v1/runs/stats"),
        (Method::POST, "/api/v1/saved/1/run"),
        (Method::GET, "/api/v1/saved/1/runs"),
        (Method::GET, "/api/v1/saved/1/runs/1"),
        (Method::POST, "/api/v1/export"),
        (Method::GET, "/api/v1/stream?query=*"),
        (Method::POST, "/api/v1/ingest"),
        (Method::POST, "/api/v1/ingest/preview"),
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn ac1_key_roundtrip_create_whoami_revoke_401() {
    let server = setup().await;
    let store = KeyStore::from_pool(server.fleet_pool.clone());

    let created = store
        .create_key(
            "roundtrip",
            PrincipalKind::Human,
            &roles(&["trawl-analyst"]),
            None,
        )
        .await
        .unwrap();
    let token = created.plaintext_token.to_string();

    let (status, body) = request(
        &server.url,
        reqwest::Method::GET,
        "/api/v1/whoami",
        Some(&token),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["name"], "roundtrip");
    assert_eq!(body["prefix"], created.info.prefix.as_str());

    store.revoke_key(&created.info.prefix).await.unwrap();
    let (status, body) = request(
        &server.url,
        reqwest::Method::GET,
        "/api/v1/whoami",
        Some(&token),
    )
    .await;
    assert_eq!(status, 401);
    assert_eq!(body["error"]["code"], "auth_error");
}

#[tokio::test(flavor = "multi_thread")]
async fn ac3_public_probes_unauthenticated() {
    let server = setup().await;
    let client = raw_client();

    let health = client
        .get(format!("{}/api/v1/health", server.url))
        .send()
        .await
        .unwrap();
    assert_eq!(health.status().as_u16(), 200);

    let metrics = client
        .get(format!("{}/metrics", server.url))
        .send()
        .await
        .unwrap();
    assert_eq!(metrics.status().as_u16(), 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn ac3_reader_blocked_from_export_stream_manage() {
    let server = setup().await;
    for (method, path) in [
        (reqwest::Method::POST, "/api/v1/export"),
        (reqwest::Method::GET, "/api/v1/stream?query=*"),
        (reqwest::Method::GET, "/api/v1/stats"),
        (reqwest::Method::GET, "/api/v1/dashboard"),
        (reqwest::Method::GET, "/api/v1/saved"),
    ] {
        let (status, body) = request(
            &server.url,
            method.clone(),
            path,
            Some(&server.reader_token),
        )
        .await;
        assert_eq!(status, 403, "{method} {path} must be denied for reader");
        assert_eq!(
            body,
            serde_json::json!({
                "error": {
                    "code": "forbidden",
                    "message": "insufficient permissions"
                }
            }),
            "{method} {path} body: {body}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ac3_ingest_only_key_can_ingest_but_nothing_else() {
    let server = setup().await;

    let resp = raw_client()
        .post(format!("{}/api/v1/ingest", server.url))
        .header("authorization", format!("Bearer {}", server.ingest_token))
        .header("content-type", "application/x-ndjson")
        .body(r#"{"service":"t","message":"m"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    for (method, path) in [
        (reqwest::Method::POST, "/api/v1/query"),
        (reqwest::Method::POST, "/api/v1/export"),
        (reqwest::Method::GET, "/api/v1/stream?query=*"),
        (reqwest::Method::GET, "/api/v1/saved"),
        (reqwest::Method::GET, "/api/v1/stats"),
        (reqwest::Method::GET, "/api/v1/schema"),
    ] {
        let (status, body) = request(
            &server.url,
            method.clone(),
            path,
            Some(&server.ingest_token),
        )
        .await;
        assert_eq!(status, 403, "{method} {path} must be denied for ingest key");
        assert_eq!(
            body,
            serde_json::json!({
                "error": {
                    "code": "forbidden",
                    "message": "insufficient permissions"
                }
            }),
            "{method} {path} body: {body}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ac3_admin_cannot_ingest() {
    let server = setup().await;
    let resp = raw_client()
        .post(format!("{}/api/v1/ingest", server.url))
        .header("authorization", format!("Bearer {}", server.admin_token))
        .header("content-type", "application/x-ndjson")
        .body(r#"{"service":"t","message":"m"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "admin and ingest are orthogonal"
    );
    assert_eq!(
        resp.json::<serde_json::Value>().await.unwrap(),
        serde_json::json!({
            "error": {
                "code": "forbidden",
                "message": "insufficient permissions"
            }
        })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ac3_grantless_key_403_on_every_authenticated_route() {
    let server = setup().await;
    for (method, path) in authenticated_routes() {
        let (status, body) = request(
            &server.url,
            method.clone(),
            path,
            Some(&server.coastwatch_only_token),
        )
        .await;
        assert_eq!(status, 403, "{method} {path} must 403 for grantless key");
        assert_eq!(
            body["error"]["code"], "forbidden",
            "{method} {path} body: {body}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ac3_grantless_key_never_reaches_rate_limiter() {
    let server = common::setup_with_rate_limit(RateLimitConfig {
        default_rpm: 1,
        ..RateLimitConfig::default()
    })
    .await;

    for _ in 0..30 {
        let (status, _) = request(
            &server.url,
            reqwest::Method::GET,
            "/api/v1/whoami",
            Some(&server.coastwatch_only_token),
        )
        .await;
        assert_eq!(status, 403, "grantless key must always 403, never 429/200");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ac3_zero_permission_role_key_403_everywhere() {
    let server = setup().await;
    let store = KeyStore::from_pool(server.fleet_pool.clone());

    store
        .create_role("empty-shell", None, &[])
        .await
        .expect("create permissionless role");
    let created = store
        .create_key(
            "shell-holder",
            PrincipalKind::Service,
            &roles(&["empty-shell"]),
            None,
        )
        .await
        .unwrap();

    for (method, path) in authenticated_routes() {
        let (status, body) = request(
            &server.url,
            method.clone(),
            path,
            Some(&created.plaintext_token),
        )
        .await;
        assert_eq!(
            status, 403,
            "{method} {path} must 403 for zero-permission-role key"
        );
        assert_eq!(
            body["error"]["code"], "forbidden",
            "{method} {path} body: {body}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ac5_rate_rpm_role_ceiling_spent_separately_per_class() {
    let server = common::setup_with_rate_limit(RateLimitConfig {
        default_rpm: 10_000,
        ingest_rpm: 10_000,
    })
    .await;
    let store = KeyStore::from_pool(server.fleet_pool.clone());

    store
        .create_role(
            "capped",
            Some(2),
            &common::trawl_perms(&["query", "ingest"]),
        )
        .await
        .expect("create capped role");
    let created = store
        .create_key(
            "capped-key",
            PrincipalKind::Service,
            &roles(&["capped"]),
            None,
        )
        .await
        .unwrap();
    let token = created.plaintext_token.to_string();

    let mut statuses = Vec::new();
    for _ in 0..4 {
        let (status, _) = request(
            &server.url,
            reqwest::Method::POST,
            "/api/v1/query",
            Some(&token),
        )
        .await;
        statuses.push(status);
    }
    assert!(
        statuses[..2].iter().all(|s| *s != 429),
        "first two interactive requests are within the ceiling: {statuses:?}"
    );
    assert_eq!(
        statuses[2..],
        [429, 429],
        "role rate_rpm=2 must cap /query despite the huge class default: {statuses:?}"
    );

    let client = raw_client();
    let mut ingest_statuses = Vec::new();
    for _ in 0..4 {
        let resp = client
            .post(format!("{}/api/v1/ingest", server.url))
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/x-ndjson")
            .body(r#"{"service":"t","message":"m"}"#)
            .send()
            .await
            .unwrap();
        ingest_statuses.push(resp.status().as_u16());
    }
    assert_eq!(
        ingest_statuses[..2],
        [200, 200],
        "ingest budget is separate from the interactive one: {ingest_statuses:?}"
    );
    assert_eq!(
        ingest_statuses[2..],
        [429, 429],
        "the same rate_rpm=2 ceiling applies independently on ingest: {ingest_statuses:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ac4_whoami_golden_json_per_role() {
    let server = setup().await;

    let cases = [
        (
            &server.admin_token,
            "admin-key",
            "trawl-admin",
            serde_json::json!([
                "query",
                "schema_read",
                "validate",
                "saved_query",
                "export",
                "stream",
                "query_cancel",
                "server_manage"
            ]),
        ),
        (
            &server.analyst_token,
            "test-key",
            "trawl-analyst",
            serde_json::json!([
                "query",
                "schema_read",
                "validate",
                "saved_query",
                "export",
                "stream",
                "query_cancel"
            ]),
        ),
        (
            &server.reader_token,
            "reader-key",
            "trawl-reader",
            serde_json::json!(["query", "schema_read", "query_cancel"]),
        ),
        (
            &server.ingest_token,
            "ingest-key",
            "trawl-ingest",
            serde_json::json!(["ingest"]),
        ),
    ];

    for (token, name, role, permissions) in cases {
        let (status, body) = request(
            &server.url,
            reqwest::Method::GET,
            "/api/v1/whoami",
            Some(token),
        )
        .await;
        assert_eq!(status, 200);

        // The prefix is dynamic (first 8 chars of the token body).
        let prefix = &token[4..12];
        let expected = serde_json::json!({
            "prefix": prefix,
            "name": name,
            "kind": "service",
            "roles": [role],
            "permissions": permissions,
        });
        assert_eq!(body, expected, "whoami wire shape drifted for {role}");
    }
}

fn opaque_401() -> serde_json::Value {
    serde_json::json!({ "error": { "code": "auth_error", "message": "authentication failed" } })
}

#[tokio::test(flavor = "multi_thread")]
async fn ac5_envelope_missing_and_malformed_and_invalid_tokens() {
    let server = setup().await;

    let (status, body) = request(&server.url, reqwest::Method::GET, "/api/v1/whoami", None).await;
    assert_eq!(status, 401);
    assert_eq!(body, opaque_401());

    let resp = raw_client()
        .get(format!("{}/api/v1/whoami", server.url))
        .header("authorization", "Basic dXNlcjpwYXNz")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);
    assert_eq!(
        resp.json::<serde_json::Value>().await.unwrap(),
        opaque_401()
    );

    let (status, body) = request(
        &server.url,
        reqwest::Method::GET,
        "/api/v1/whoami",
        Some("not-a-fleet-token"),
    )
    .await;
    assert_eq!(status, 401);
    assert_eq!(body, opaque_401());

    let (status, body) = request(
        &server.url,
        reqwest::Method::GET,
        "/api/v1/whoami",
        Some("flt_ZZZZZZZZtotallyfaketokenbody1234567890"),
    )
    .await;
    assert_eq!(status, 401);
    assert_eq!(body, opaque_401());
}

#[tokio::test(flavor = "multi_thread")]
async fn ac5_envelope_revoked_and_expired_tokens() {
    let server = setup().await;
    let store = KeyStore::from_pool(server.fleet_pool.clone());

    let revoked = store
        .create_key(
            "rev",
            PrincipalKind::Service,
            &roles(&["trawl-reader"]),
            None,
        )
        .await
        .unwrap();
    store.revoke_key(&revoked.info.prefix).await.unwrap();
    let (status, body) = request(
        &server.url,
        reqwest::Method::GET,
        "/api/v1/whoami",
        Some(&revoked.plaintext_token),
    )
    .await;
    assert_eq!(status, 401);
    assert_eq!(body, opaque_401());

    let expiring = store
        .create_key(
            "exp",
            PrincipalKind::Service,
            &roles(&["trawl-reader"]),
            Some(Duration::from_millis(50)),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let (status, body) = request(
        &server.url,
        reqwest::Method::GET,
        "/api/v1/whoami",
        Some(&expiring.plaintext_token),
    )
    .await;
    assert_eq!(status, 401);
    assert_eq!(body, opaque_401());
}

#[tokio::test(flavor = "multi_thread")]
async fn ac5_envelope_grantless_403() {
    let server = setup().await;
    let (status, body) = request(
        &server.url,
        reqwest::Method::GET,
        "/api/v1/whoami",
        Some(&server.coastwatch_only_token),
    )
    .await;
    assert_eq!(status, 403);
    assert_eq!(
        body,
        serde_json::json!({
            "error": { "code": "forbidden", "message": "no trawl grant for this key" }
        })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ac5_health_and_envelope_pg_down_without_backend_detail() {
    let server = setup().await;
    let client = raw_client();

    // Prime the memoised successful auth probe, then force the next health
    // request to reach the backend after it is removed.
    let healthy = client
        .get(format!("{}/api/v1/health", server.url))
        .send()
        .await
        .unwrap();
    assert_eq!(healthy.status().as_u16(), 200);
    let healthy: serde_json::Value = healthy.json().await.unwrap();
    let version = healthy["version"].as_str().unwrap().to_owned();
    assert_eq!(
        healthy,
        serde_json::json!({
            "status": "ok",
            "checks": {
                "duckdb": "ok",
                "auth_db": "ok",
                "storage_db": "ok",
                "data_path": "ok",
                "ingest_capacity": "ok",
                "corpus": "ok"
            },
            "version": version
        })
    );

    server.kill_fleet_database().await;
    *server.state.auth.auth_ping.lock().await = None;

    let health = client
        .get(format!("{}/api/v1/health", server.url))
        .send()
        .await
        .unwrap();
    assert_eq!(health.status().as_u16(), 200);
    let health: serde_json::Value = health.json().await.unwrap();
    assert_eq!(
        health,
        serde_json::json!({
            "status": "degraded",
            "checks": {
                "duckdb": "ok",
                "auth_db": "error",
                "storage_db": "ok",
                "data_path": "ok",
                "ingest_capacity": "ok",
                "corpus": "ok"
            },
            "version": version
        }),
        "health must not leak postgres errors, DSNs, or paths"
    );

    let (status, body) = request(
        &server.url,
        reqwest::Method::GET,
        "/api/v1/whoami",
        Some(&server.analyst_token),
    )
    .await;
    assert_eq!(status, 503);
    assert_eq!(
        body,
        serde_json::json!({
            "error": { "code": "service_unavailable", "message": "auth backend unavailable" }
        }),
        "503 must not leak postgres details"
    );
}

/// `key_ttl`/`pre_sleep_ms` support the expiry case (expiry can't be set
/// post-hoc through the public API).
async fn scheduler_runs_after(
    pool: PgPool,
    key_ttl: Option<Duration>,
    pre_sleep_ms: u64,
    mutate: impl AsyncFnOnce(&KeyStore, &fleet_auth::CreatedKey),
) -> u64 {
    let key_store = common::fleet_keystore(&pool).await;
    let created = key_store
        .create_key(
            "sched-owner",
            PrincipalKind::Service,
            &roles(&["trawl-analyst"]),
            key_ttl,
        )
        .await
        .unwrap();

    let app_db_url = common::create_app_database().await;
    let storage = trawl_server::store::StorageState::from_pool(common::app_pool(&app_db_url).await)
        .await
        .expect("boot app storage");

    let saved = storage
        .saved
        .create(created.info.id, "gate-net", "* | head 1")
        .await
        .unwrap();
    let schedule = storage
        .schedule
        // 60s is the store minimum; the first poll runs regardless of interval,
        // so cadence is immaterial here.
        .create_schedule(saved.id, created.info.id, 60, None, None, 0, chrono::Utc::now())
        .await
        .unwrap();

    mutate(&key_store, &created).await;

    if pre_sleep_ms > 0 {
        tokio::time::sleep(Duration::from_millis(pre_sleep_ms)).await;
    }

    // The root is this test's own copy: the scheduler WRITES report output
    // under `scheduled/`, which no other test may see.
    let data_dir = tempfile::tempdir().expect("scheduler data root");
    let base_dir = seed_data_root(data_dir.path());
    let exec_pool = trawl_server::pool::ExecutorPool::for_test(base_dir, 1, 1000, None).unwrap();
    let results_root = std::sync::Arc::new(
        trawl_server::results::root::ResultsRoot::open_apart_from(
            &data_dir.path().join("data.results"),
            &[],
        )
        .expect("hold a results root"),
    );
    let results = std::sync::Arc::new(trawl_server::results::ResultsState {
        gate: trawl_server::results::charge::CaptureGate::new(
            &trawl_server::config::CaptureConfig::default(),
            storage.results.clone(),
            std::sync::Arc::clone(&results_root),
            trawl_server::results::ResultsClock::System,
        ),
        root: results_root,
        store: storage.results.clone(),
        capture: trawl_server::config::CaptureConfig::default(),
        investigation: trawl_server::config::InvestigationConfig::default(),
        clock: trawl_server::results::ResultsClock::System,
        in_flight: std::sync::Arc::default(),
        steps: {
            let capture = trawl_server::config::CaptureConfig::default();
            trawl_server::investigations::step::StepWriter::start(
                std::time::Duration::from_secs(capture.deadline_secs),
                usize::try_from(capture.per_key_running + capture.queue_depth).unwrap(),
            )
        },
    });

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let handle = trawl_server::scheduler::spawn_scheduler(
        storage.schedule.clone(),
        key_store.clone(),
        exec_pool,
        results,
        trawl_server::config::SchedulerConfig {
            enabled: true,
            poll_interval_secs: 1,
            max_runs_per_schedule: 100,
            report_retention_days: 30,
            max_catchup_intervals: 24,
        },
        10,
        shutdown_rx,
    );

    tokio::time::sleep(Duration::from_millis(2500)).await;
    let _ = shutdown_tx.send(true);
    let _ = handle.await;

    storage.schedule.count_runs(schedule.id).await.unwrap()
}

#[sqlx::test(migrations = false)]
async fn ac6_scheduler_runs_for_live_key(pool: PgPool) {
    let runs = scheduler_runs_after(pool, None, 0, async |_store, _key| {}).await;
    assert!(runs >= 1, "control case: live key must execute, got {runs}");
}

#[sqlx::test(migrations = false)]
async fn ac6_scheduler_skips_revoked_key(pool: PgPool) {
    let runs = scheduler_runs_after(
        pool,
        None,
        0,
        async |store: &KeyStore, key: &fleet_auth::CreatedKey| {
            store.revoke_key(&key.info.prefix).await.unwrap();
        },
    )
    .await;
    assert_eq!(runs, 0, "revoked key must not execute schedules");
}

#[sqlx::test(migrations = false)]
async fn ac6_scheduler_skips_expired_key(pool: PgPool) {
    let runs = scheduler_runs_after(
        pool,
        Some(Duration::from_millis(100)),
        200,
        async |_store, _key| {},
    )
    .await;
    assert_eq!(runs, 0, "expired key must not execute schedules");
}

#[sqlx::test(migrations = false)]
async fn ac6_scheduler_skips_role_stripped_key(pool: PgPool) {
    let runs = scheduler_runs_after(
        pool,
        None,
        0,
        async |store: &KeyStore, key: &fleet_auth::CreatedKey| {
            store
                .unassign_role(&key.info.prefix, "trawl-analyst")
                .await
                .unwrap();
        },
    )
    .await;
    assert_eq!(runs, 0, "role-stripped key must not execute schedules");
}

async fn assert_downgrade_stops_schedules(pool: PgPool, new_role: &'static str) {
    let runs = scheduler_runs_after(
        pool,
        None,
        0,
        async move |store: &KeyStore, key: &fleet_auth::CreatedKey| {
            store
                .unassign_role(&key.info.prefix, "trawl-analyst")
                .await
                .unwrap();
            store.assign_role(&key.info.prefix, new_role).await.unwrap();
        },
    )
    .await;
    assert_eq!(
        runs, 0,
        "key downgraded to {new_role} must not execute schedules"
    );
}

#[sqlx::test(migrations = false)]
async fn ac6_scheduler_skips_analyst_downgraded_to_reader(pool: PgPool) {
    assert_downgrade_stops_schedules(pool, "trawl-reader").await;
}

#[sqlx::test(migrations = false)]
async fn ac6_scheduler_skips_analyst_downgraded_to_ingest(pool: PgPool) {
    assert_downgrade_stops_schedules(pool, "trawl-ingest").await;
}

#[sqlx::test(migrations = false)]
async fn ac6_scheduler_stops_when_role_loses_saved_query_permission(pool: PgPool) {
    let runs = scheduler_runs_after(
        pool,
        None,
        0,
        async |store: &KeyStore, _key: &fleet_auth::CreatedKey| {
            store
                .remove_role_permissions(
                    "trawl-analyst",
                    &[fleet_auth::RolePermission {
                        app: "trawl".into(),
                        permission: "saved_query".into(),
                    }],
                )
                .await
                .unwrap();
        },
    )
    .await;
    assert_eq!(
        runs, 0,
        "removing saved_query from the ROLE must stop schedules"
    );
}

#[derive(Clone)]
struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The subscriber is global (`try_init`), so the capture holds one test's
/// log only because nextest runs each test in its own process. Run these
/// with nextest.
fn capture_logs() -> Arc<Mutex<Vec<u8>>> {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let writer = CaptureWriter(Arc::clone(&buf));
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        // Plain text: the assertions match field names literally, and ANSI
        // styling splits `permissions=` with escape codes.
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .try_init();
    buf
}

fn log_text(buf: &Mutex<Vec<u8>>) -> String {
    String::from_utf8(buf.lock().clone()).unwrap()
}

/// fleet-auth logs its own `key_created` and `key_revoked` for mutations
/// made in this process; only the poller's lines carry this suffix.
fn detected(log: &str) -> Vec<&str> {
    log.lines()
        .filter(|l| l.contains("detected by audit"))
        .collect()
}

fn detected_events<'a>(log: &'a str, event_type: &str, needle: &str) -> Vec<&'a str> {
    let field = format!("event_type=\"{event_type}\"");
    detected(log)
        .into_iter()
        .filter(|l| l.contains(&field) && l.contains(needle))
        .collect()
}

const HANG_GUARD: Duration = Duration::from_secs(10);

async fn wait_parked(parked: tokio::sync::oneshot::Receiver<()>, what: &str) {
    tokio::time::timeout(HANG_GUARD, parked)
        .await
        .unwrap_or_else(|_| panic!("audit task never reached {what}"))
        .expect("audit hooks dropped the hold");
}

/// Release the task from `held`, let it finish that poll, and hold it again
/// before the next poll's first read. Parking there proves every event of
/// the released poll was emitted, because emission has no await.
async fn next_poll(
    hooks: &audit::AuditHooks,
    held: tokio::sync::oneshot::Sender<()>,
) -> tokio::sync::oneshot::Sender<()> {
    let next = hooks.hold(audit::Phase::Poll, audit::Point::BeforeListKeys);
    drop(held);
    wait_parked(next.parked, "the next poll").await;
    next.release
}

/// Spawn the audit task held before its first poll, once its baseline and
/// `audit_start` are in. Returns the release for that hold.
async fn spawn_held_audit(
    key_store: &KeyStore,
    hooks: &audit::AuditHooks,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> (
    tokio::task::JoinHandle<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let first = hooks.hold(audit::Phase::Poll, audit::Point::BeforeListKeys);
    let handle = audit::spawn_audit_task_with_hooks(
        key_store.clone(),
        Duration::from_millis(20),
        shutdown,
        hooks.clone(),
    );
    wait_parked(first.parked, "the first poll").await;
    (handle, first.release)
}

async fn stop_audit(
    shutdown: tokio::sync::watch::Sender<bool>,
    handle: tokio::task::JoinHandle<()>,
) {
    let _ = shutdown.send(true);
    tokio::time::timeout(HANG_GUARD, handle)
        .await
        .expect("audit task must stop on shutdown")
        .unwrap();
}

fn timestamp_field(line: &str, field: &str) -> chrono::DateTime<chrono::Utc> {
    let start = line
        .find(&format!("{field}="))
        .unwrap_or_else(|| panic!("no {field} in: {line}"))
        + field.len()
        + 1;
    let value = line[start..].split_whitespace().next().unwrap();
    chrono::DateTime::parse_from_rfc3339(value)
        .unwrap_or_else(|e| panic!("{field}={value} is not rfc3339: {e}"))
        .with_timezone(&chrono::Utc)
}

#[sqlx::test(migrations = false)]
async fn ac8_audit_poller_emits_events_for_out_of_process_mutations(pool: PgPool) {
    let buf = capture_logs();
    let key_store = common::fleet_keystore(&pool).await;

    let baseline = key_store
        .create_key(
            "baseline",
            PrincipalKind::Service,
            &roles(&["trawl-reader"]),
            None,
        )
        .await
        .unwrap();

    let hooks = audit::AuditHooks::default();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let (handle, held) = spawn_held_audit(&key_store, &hooks, shutdown_rx).await;

    // Out-of-process mutation: a second connection to the same database
    // (what fleet-admin does).
    let second = KeyStore::from_pool(common::fleet_pool(&common::fleet_database_url(&pool)).await);
    let created = second
        .create_key(
            "made-by-fleet-admin",
            PrincipalKind::Human,
            &roles(&["trawl-admin"]),
            None,
        )
        .await
        .unwrap();
    let held = next_poll(&hooks, held).await;

    second
        .add_role_permissions(
            "trawl-reader",
            &[fleet_auth::RolePermission {
                app: "trawl".into(),
                permission: "server_manage".into(),
            }],
        )
        .await
        .unwrap();
    second
        .assign_role(&baseline.info.prefix, "trawl-admin")
        .await
        .unwrap();
    let held = next_poll(&hooks, held).await;

    let fleeting = second
        .create_key(
            "created-and-revoked",
            PrincipalKind::Service,
            &roles(&["trawl-reader"]),
            None,
        )
        .await
        .unwrap();
    second.revoke_key(&fleeting.info.prefix).await.unwrap();
    let held = next_poll(&hooks, held).await;

    second.revoke_key(&created.info.prefix).await.unwrap();
    let held = next_poll(&hooks, held).await;

    stop_audit(shutdown_tx, handle).await;
    drop(held);
    second.pool().close().await;

    let log = log_text(&buf);
    let created_lines = detected_events(&log, "key_created", &created.info.prefix);
    assert_eq!(
        created_lines.len(),
        1,
        "audit must emit key_created once for the new key; log:\n{log}"
    );
    assert!(
        created_lines[0].contains("permissions=") && created_lines[0].contains("trawl:"),
        "key_created must record resolved permissions; line:\n{}",
        created_lines[0]
    );
    assert_eq!(
        detected_events(&log, "key_revoked", &created.info.prefix).len(),
        1,
        "audit must emit key_revoked once for the revoked key; log:\n{log}"
    );
    let role_lines = detected_events(&log, "role_changed", "trawl-reader");
    assert!(
        role_lines.len() == 1 && role_lines[0].contains("trawl:server_manage"),
        "audit must emit role_changed naming the added permission; log:\n{log}"
    );
    let assign_lines = detected_events(&log, "key_roles_changed", &baseline.info.prefix);
    assert!(
        assign_lines.len() == 1 && assign_lines[0].contains("trawl-admin"),
        "audit must emit key_roles_changed for the reassigned key; log:\n{log}"
    );
    let fleeting_lines: Vec<&str> = detected(&log)
        .into_iter()
        .filter(|l| l.contains(&fleeting.info.prefix))
        .collect();
    assert!(
        fleeting_lines.len() == 2
            && fleeting_lines[0].contains("event_type=\"key_created\"")
            && fleeting_lines[1].contains("event_type=\"key_revoked\"")
            && fleeting_lines[1].contains("permissions="),
        "a key created and revoked between polls must report key_created then key_revoked; \
         log:\n{log}"
    );
}

#[sqlx::test(migrations = false)]
async fn audit_poll_failure_between_reads_keeps_baseline(pool: PgPool) {
    let buf = capture_logs();
    let key_store = common::fleet_keystore(&pool).await;
    let hooks = audit::AuditHooks::default();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let (handle, held) = spawn_held_audit(&key_store, &hooks, shutdown_rx).await;

    let key = key_store
        .create_key(
            "between-reads",
            PrincipalKind::Service,
            &roles(&["trawl-reader"]),
            None,
        )
        .await
        .unwrap();
    hooks.fail(audit::Phase::Poll, audit::Step::ListRoles, 1);
    // Parking before `list_roles` proves `list_keys` ran for real first.
    let between = hooks.hold(audit::Phase::Poll, audit::Point::BeforeListRoles);
    drop(held);
    wait_parked(between.parked, "the role read").await;
    let held = next_poll(&hooks, between.release).await;

    let log = log_text(&buf);
    assert!(
        detected(&log).is_empty(),
        "a failed poll must emit no change events; log:\n{log}"
    );
    let errors: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("event_type=\"audit_error\""))
        .collect();
    assert!(
        errors.len() == 1
            && errors[0].contains("phase=\"poll\"")
            && errors[0].contains("operation=\"list_roles\""),
        "one sanitized audit_error for the failed role read; log:\n{log}"
    );
    assert!(
        !log.contains(audit::INJECTED_ERROR_TEXT),
        "raw error text must not reach the log; log:\n{log}"
    );

    let held = next_poll(&hooks, held).await;
    let log = log_text(&buf);
    assert_eq!(
        detected_events(&log, "key_created", &key.info.prefix).len(),
        1,
        "the next poll reports the change once; log:\n{log}"
    );

    let held = next_poll(&hooks, held).await;
    let log = log_text(&buf);
    assert_eq!(
        detected_events(&log, "key_created", &key.info.prefix).len(),
        1,
        "a later poll does not report it again; log:\n{log}"
    );

    stop_audit(shutdown_tx, handle).await;
    drop(held);
}

#[sqlx::test(migrations = false)]
async fn audit_init_recovers_after_failures(pool: PgPool) {
    let buf = capture_logs();
    let key_store = common::fleet_keystore(&pool).await;
    let hooks = audit::AuditHooks::default();
    // Attempts 1 and 2 fail on keys, attempt 3 on roles, attempt 4 succeeds.
    hooks.fail(audit::Phase::Init, audit::Step::ListKeys, 2);
    hooks.fail(audit::Phase::Init, audit::Step::ListRoles, 1);
    let first_attempt = hooks.hold(audit::Phase::Init, audit::Point::BeforeListKeys);

    let spawned_at = chrono::Utc::now();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let handle = audit::spawn_audit_task_with_hooks(
        key_store.clone(),
        Duration::from_millis(20),
        shutdown_rx,
        hooks.clone(),
    );
    wait_parked(first_attempt.parked, "the first init attempt").await;
    let during_first_attempt = chrono::Utc::now();
    let held = next_poll(&hooks, first_attempt.release).await;

    let log = log_text(&buf);
    let errors: Vec<(usize, &str)> = log
        .match_indices("event_type=\"audit_error\"")
        .map(|(at, _)| (at, log[at..].lines().next().unwrap()))
        .collect();
    assert_eq!(errors.len(), 3, "one audit_error per failure; log:\n{log}");
    for (_, line) in &errors {
        assert!(
            line.contains("phase=\"init\""),
            "audit_error names the phase: {line}"
        );
    }
    let operations: Vec<bool> = errors
        .iter()
        .map(|(_, l)| l.contains("operation=\"list_keys\""))
        .collect();
    assert_eq!(operations, [true, true, false], "log:\n{log}");
    assert!(
        errors[2].1.contains("operation=\"list_roles\""),
        "log:\n{log}"
    );
    assert!(
        !log.contains(audit::INJECTED_ERROR_TEXT),
        "raw error text must not reach the log; log:\n{log}"
    );

    let gaps: Vec<usize> = log
        .match_indices("event_type=\"audit_gap\"")
        .map(|(at, _)| at)
        .collect();
    assert_eq!(gaps.len(), 1, "one audit_gap; log:\n{log}");
    let gap_line = log[gaps[0]..].lines().next().unwrap();
    let from = timestamp_field(gap_line, "unaudited_from");
    let until = timestamp_field(gap_line, "unaudited_until");
    assert!(
        spawned_at <= from && from <= during_first_attempt,
        "unaudited_from {from} must be at or before the first attempt ({during_first_attempt})"
    );
    assert!(
        until > during_first_attempt,
        "unaudited_until {until} must be after the first attempt ({during_first_attempt})"
    );
    assert!(gap_line.contains("attempts=3"), "{gap_line}");
    assert!(gap_line.contains("reconstruction=\"none\""), "{gap_line}");
    let start = log
        .find("action=\"audit_start\"")
        .expect("audit_start after recovery");
    assert!(
        errors[2].0 < gaps[0] && gaps[0] < start,
        "order must be audit_error x3, audit_gap, audit_start; log:\n{log}"
    );

    let key = key_store
        .create_key(
            "after-recovery",
            PrincipalKind::Service,
            &roles(&["trawl-reader"]),
            None,
        )
        .await
        .unwrap();
    let held = next_poll(&hooks, held).await;
    let log = log_text(&buf);
    assert_eq!(
        detected_events(&log, "key_created", &key.info.prefix).len(),
        1,
        "a mutation after recovery is audited; log:\n{log}"
    );

    stop_audit(shutdown_tx, handle).await;
    drop(held);
}

/// The hold is never released, so only the shutdown race can end it.
#[sqlx::test(migrations = false)]
async fn audit_shutdown_while_init_fetch_is_held(pool: PgPool) {
    let buf = capture_logs();
    let key_store = common::fleet_keystore(&pool).await;
    let hooks = audit::AuditHooks::default();
    let held = hooks.hold(audit::Phase::Init, audit::Point::BeforeListRoles);
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let handle = audit::spawn_audit_task_with_hooks(
        key_store.clone(),
        Duration::from_millis(20),
        shutdown_rx,
        hooks.clone(),
    );
    wait_parked(held.parked, "the init role read").await;

    stop_audit(shutdown_tx, handle).await;
    drop(held.release);

    let log = log_text(&buf);
    assert!(log.contains("action=\"audit_stop\""), "log:\n{log}");
    assert!(!log.contains("action=\"audit_start\""), "log:\n{log}");
}

/// The interval is an hour, so a sleep outside the shutdown race would trip
/// the hang guard.
#[sqlx::test(migrations = false)]
async fn audit_shutdown_while_waiting_between_init_attempts(pool: PgPool) {
    let buf = capture_logs();
    let key_store = common::fleet_keystore(&pool).await;
    let hooks = audit::AuditHooks::default();
    hooks.fail(audit::Phase::Init, audit::Step::ListKeys, 1);
    let before_wait = hooks.hold(audit::Phase::Init, audit::Point::BeforeWait);
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let handle = audit::spawn_audit_task_with_hooks(
        key_store.clone(),
        Duration::from_secs(3600),
        shutdown_rx,
        hooks.clone(),
    );
    wait_parked(before_wait.parked, "the wait after a failed init").await;
    drop(before_wait.release);
    // The task signals `resumed` from inside the poll that runs on into the
    // sleep, after that poll's shutdown check, so the shutdown below can
    // only end a sleep that races it.
    wait_parked(before_wait.resumed, "the sleep after the released hold").await;

    stop_audit(shutdown_tx, handle).await;

    let log = log_text(&buf);
    assert!(
        log.contains("event_type=\"audit_error\"") && log.contains("phase=\"init\""),
        "log:\n{log}"
    );
    assert!(log.contains("action=\"audit_stop\""), "log:\n{log}");
    assert!(!log.contains("action=\"audit_start\""), "log:\n{log}");
}

#[tokio::test(flavor = "multi_thread")]
async fn sse_stream_rejects_revoked_key_at_handshake() {
    let server = setup().await;
    let store = KeyStore::from_pool(server.fleet_pool.clone());

    let created = store
        .create_key(
            "sse",
            PrincipalKind::Service,
            &roles(&["trawl-analyst"]),
            None,
        )
        .await
        .unwrap();
    store.revoke_key(&created.info.prefix).await.unwrap();

    let resp = raw_client()
        .get(format!("{}/api/v1/stream?query=*", server.url))
        .header(
            "authorization",
            format!("Bearer {}", created.plaintext_token.as_str()),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);
}

#[tokio::test(flavor = "multi_thread")]
async fn storage_loss_degrades_health_and_503s_store_endpoints() {
    let server = setup().await;
    let client = raw_client();

    server.kill_app_database().await;

    // The storage ping is memoised (~5s TTL) — poll until degradation
    // propagates.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let resp = client
            .get(format!("{}/api/v1/health", server.url))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status().as_u16(),
            200,
            "storage loss must be Degraded + HTTP 200"
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        if body["status"] == "degraded" {
            assert_eq!(
                body["checks"],
                serde_json::json!({
                    "duckdb": "ok",
                    "auth_db": "ok",
                    "storage_db": "error",
                    "data_path": "ok",
                    "ingest_capacity": "ok",
                    "corpus": "ok"
                }),
                "got: {body}"
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "health never degraded: {body}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    let (status, body) = request(
        &server.url,
        reqwest::Method::GET,
        "/api/v1/history",
        Some(&server.analyst_token),
    )
    .await;
    assert_eq!(status, 503);
    assert_eq!(
        body,
        serde_json::json!({
            "error": {
                "code": "service_unavailable",
                "message": "app-state store unavailable"
            }
        }),
        "503 must not leak postgres details"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn dashboard_reports_schedule_count_from_pg() {
    let server = setup().await;
    let client = raw_client();

    let resp = client
        .post(format!("{}/api/v1/saved", server.url))
        .header("authorization", format!("Bearer {}", server.analyst_token))
        .json(&serde_json::json!({"name": "dash-net", "query": "* | head 1"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let saved: serde_json::Value = resp.json().await.unwrap();
    let saved_id = saved["id"].as_i64().unwrap();

    let resp = client
        .put(format!("{}/api/v1/saved/{saved_id}/schedule", server.url))
        .header("authorization", format!("Bearer {}", server.analyst_token))
        .json(&serde_json::json!({"interval": "1h"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let (status, body) = request(
            &server.url,
            reqwest::Method::GET,
            "/api/v1/dashboard",
            Some(&server.admin_token),
        )
        .await;
        if status == 200 && body["scheduler_schedules"] == 1 {
            assert_eq!(body["scheduler_enabled"], true, "got: {body}");
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "dashboard never showed the schedule: {status} {body}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn history_filter_scopes_matching_total_and_pages_to_authenticated_key() {
    let server = setup().await;
    let keys = KeyStore::from_pool(server.fleet_pool.clone());
    let reader = keys.verify_key(&server.reader_token).await.unwrap();
    let analyst = keys.verify_key(&server.analyst_token).await.unwrap();
    for (key, query) in [
        (reader.id, "old IP 192.0.2.1 two  spaces %_\\+'"),
        (reader.id, "unrelated"),
        (reader.id, "new ip 192.0.2.1 two  spaces %_\\+'"),
        (analyst.id, "foreign ip 192.0.2.1 two  spaces %_\\+'"),
    ] {
        server
            .state
            .storage
            .history
            .record_query(key, query, 1, 1, trawl_server::store::RunStatus::Success)
            .await
            .unwrap();
    }
    for (offset, expected) in [(0, Some("new ip")), (1, Some("old IP")), (2, None)] {
        let path = format!(
            "/api/v1/history?filter=ip+192.0.2.1&limit=1&offset={offset}&key_id={}",
            analyst.id
        );
        let (status, body) = request(
            &server.url,
            reqwest::Method::GET,
            &path,
            Some(&server.reader_token),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(body["total"], 2);
        let entries = body["entries"].as_array().unwrap();
        if let Some(prefix) = expected {
            assert_eq!(entries.len(), 1);
            assert!(entries[0]["query"].as_str().unwrap().starts_with(prefix));
        } else {
            assert!(entries.is_empty());
        }
    }
    let (status, body) = request(
        &server.url,
        reqwest::Method::GET,
        "/api/v1/history?filter=%25_%5C%2B%27&limit=0",
        Some(&server.reader_token),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["total"], 2);
    assert_eq!(body["entries"], serde_json::json!([]));
    let (status, _) = request(
        &server.url,
        reqwest::Method::GET,
        "/api/v1/history?filter=ip",
        None,
    )
    .await;
    assert_eq!(status, 401);
}

#[tokio::test(flavor = "multi_thread")]
async fn history_filter_http_admits_decoded_byte_and_pair_boundaries() {
    let server = setup().await;
    let admitted = [
        String::new(),
        "filter=".into(),
        "filter=%2B%25%27%E6%97%A5&unknown=%FF%00%GG".into(),
        "filter=%2531".into(),
        "filter=two++spaces".into(),
        format!("filter={}", "x".repeat(32_768)),
        // Keep these wire requests under http::Uri's 65534-byte transport
        // ceiling while still testing UTF-8 decoded-byte admission at 32768.
        format!("filter={}%E6%97%A5", "x".repeat(32_765)),
        vec!["unknown=%FF"; 64].join("&"),
    ];
    for raw in admitted {
        let (status, body) = request(
            &server.url,
            reqwest::Method::GET,
            &format!("/api/v1/history?{raw}"),
            Some(&server.reader_token),
        )
        .await;
        assert_eq!(status, 200, "admitted {} byte query: {body}", raw.len());
        assert_eq!(body["total"], 0);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn history_filter_permission_and_raw_refusal_precede_history_sql() {
    let server = setup().await;
    server.kill_app_database().await;
    let mut refused: Vec<String> = [
        "filter=private-marker&filter=b",
        "filter=&%66ilter=b",
        "filter=%",
        "filter=%1",
        "filter=%GG",
        "filter=%FF",
        "filter=%E6%97",
        "filter=%00",
        "%GG=ignored",
        "%FF=ignored",
        "%00=ignored",
        "limit=1&%6cimit=2",
        "limit=%GG",
        "offset=-1",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    refused.push(format!("filter={}", "x".repeat(32_769)));
    refused.push(format!("filter={}%E6%97%A5", "x".repeat(32_766)));
    refused.push(vec!["unknown=x"; 65].join("&"));
    for raw in refused {
        let path = format!("/api/v1/history?{raw}");
        let (status, body) = request(
            &server.url,
            reqwest::Method::GET,
            &path,
            Some(&server.reader_token),
        )
        .await;
        assert_eq!(status, 400, "invalid {} byte query", raw.len());
        assert_eq!(
            body,
            serde_json::json!({"error": {
                "code": "bad_request", "message": "invalid history parameters"
            }})
        );
        let (status, body) = request(
            &server.url,
            reqwest::Method::GET,
            &path,
            Some(&server.ingest_token),
        )
        .await;
        assert_eq!(status, 403);
        assert_eq!(body["error"]["code"], "forbidden");
    }
    // A valid filter reaches the unavailable store. This distinguishes raw
    // refusal from a test that never exercised a functioning History route.
    let (status, body) = request(
        &server.url,
        reqwest::Method::GET,
        "/api/v1/history?filter=private-marker",
        Some(&server.reader_token),
    )
    .await;
    assert_eq!(status, 503);
    assert_eq!(
        body,
        serde_json::json!({"error": {
            "code": "service_unavailable", "message": "app-state store unavailable"
        }})
    );
    let (status, _) = request(
        &server.url,
        reqwest::Method::GET,
        "/api/v1/history?filter=valid",
        Some(&server.ingest_token),
    )
    .await;
    assert_eq!(status, 403);
}

#[tokio::test(flavor = "multi_thread")]
async fn history_filter_handler_admits_raw_cap_before_store_and_after_permission() {
    use axum::response::IntoResponse as _;

    // http::Uri limits a transport URI to 65534 bytes. Exercise the larger
    // application raw-query cap through the actual handler, without pretending
    // that these constructed RawQuery values traversed the HTTP transport.
    async fn invoke(
        server: &common::TestServer,
        key: &fleet_auth::VerifiedKey,
        raw: &str,
    ) -> (u16, serde_json::Value) {
        let response = trawl_server::handlers::history(
            axum::extract::State(server.state.clone()),
            axum::Extension(key.clone()),
            axum::extract::RawQuery(Some(raw.to_owned())),
        )
        .await
        .into_response();
        let status = response.status().as_u16();
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    let server = setup().await;
    let keys = KeyStore::from_pool(server.fleet_pool.clone());
    let reader = keys.verify_key(&server.reader_token).await.unwrap();
    let ingest = keys.verify_key(&server.ingest_token).await.unwrap();
    let raw_boundary = format!("unknown={}", "x".repeat(98_432 - "unknown=".len()));
    let admitted = [
        format!("filter={}", "%61".repeat(32_768)),
        raw_boundary.clone(),
        format!("?{}", &raw_boundary[..raw_boundary.len() - 1]),
    ];
    for raw in &admitted {
        let (status, body) = invoke(&server, &reader, raw).await;
        assert_eq!(status, 200, "admitted {} raw bytes", raw.len());
        assert_eq!(body, serde_json::json!({"entries": [], "total": 0}));
    }

    server.kill_app_database().await;
    for raw in &admitted {
        let (status, body) = invoke(&server, &reader, raw).await;
        assert_eq!(status, 503);
        assert_eq!(
            body,
            serde_json::json!({"error": {
                "code": "service_unavailable", "message": "app-state store unavailable"
            }})
        );
        assert_eq!(invoke(&server, &ingest, raw).await.0, 403);
    }
    for raw in [
        format!("{raw_boundary}x"),
        format!("?{raw_boundary}"),
        format!("?{raw_boundary}x"),
        format!("filter={}", "%61".repeat(32_769)),
    ] {
        let (status, body) = invoke(&server, &reader, &raw).await;
        assert_eq!(status, 400, "refused {} raw bytes", raw.len());
        assert_eq!(
            body,
            serde_json::json!({"error": {
                "code": "bad_request", "message": "invalid history parameters"
            }})
        );
        assert_eq!(invoke(&server, &ingest, &raw).await.0, 403);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn history_clear_requires_query_and_scopes_to_caller() {
    let server = setup().await;
    let reader = trawl_client::HttpClient::new_insecure(&server.url, &server.reader_token).unwrap();
    let analyst =
        trawl_client::HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    reader
        .query_paginated("* | head 1", None, None)
        .await
        .unwrap();
    reader
        .query_paginated("* | head 2", None, None)
        .await
        .unwrap();
    analyst
        .query_paginated("* | head 3", None, None)
        .await
        .unwrap();
    let foreign = analyst.history(None, None).await.unwrap();
    let saved = analyst.create_saved("keep", "*").await.unwrap();

    let (status, _) = request(
        &server.url,
        reqwest::Method::DELETE,
        "/api/v1/history",
        Some(&server.ingest_token),
    )
    .await;
    assert_eq!(status, 403);
    assert_eq!(reader.history(None, None).await.unwrap().total, 2);

    // Client-supplied actor identifiers have no authority.
    let response = raw_client()
        .delete(format!(
            "{}/api/v1/history?key_id=0&filter=does-not-match",
            server.url
        ))
        .bearer_auth(&server.reader_token)
        .json(&serde_json::json!({"key_id": 0}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap(),
        serde_json::json!({"deleted": 2})
    );
    assert_eq!(reader.clear_history().await.unwrap().deleted, 0);
    assert_eq!(reader.history(None, None).await.unwrap().total, 0);
    assert_eq!(
        serde_json::to_value(analyst.history(None, None).await.unwrap()).unwrap(),
        serde_json::to_value(foreign).unwrap()
    );
    assert_eq!(analyst.list_saved().await.unwrap().queries[0].id, saved.id);
}

#[tokio::test(flavor = "multi_thread")]
async fn history_clear_redacts_store_failure() {
    let server = setup().await;
    server.kill_app_database().await;
    let (status, body) = request(
        &server.url,
        reqwest::Method::DELETE,
        "/api/v1/history",
        Some(&server.reader_token),
    )
    .await;
    assert_eq!(status, 503);
    assert_eq!(
        body,
        serde_json::json!({
            "error": {"code": "service_unavailable", "message": "app-state store unavailable"}
        })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn history_clear_audits_only_successful_actor_and_count() {
    use common::audit_capture::Capture;
    use tracing_subscriber::prelude::*;

    let capture = Capture::default();
    let subscriber = tracing_subscriber::registry().with(
        capture
            .clone()
            .with_filter(tracing_subscriber::EnvFilter::new("trawl_server=info")),
    );
    tracing::subscriber::set_global_default(subscriber).expect("no prior global subscriber");

    let server = setup().await;
    let key_store = KeyStore::from_pool(server.fleet_pool.clone());
    let actor = key_store.verify_key(&server.reader_token).await.unwrap();
    let reader = trawl_client::HttpClient::new_insecure(&server.url, &server.reader_token).unwrap();
    // Seed sensitive query text without executing a query that emits its own
    // unrelated diagnostics. The clear event must contain metadata alone.
    server
        .state
        .storage
        .history
        .record_query(
            actor.id,
            "private-query-payload-must-not-be-logged",
            1,
            0,
            trawl_server::store::RunStatus::Success,
        )
        .await
        .unwrap();
    let events = || {
        capture
            .events()
            .into_iter()
            .filter(|event| {
                event
                    .fields
                    .get("event_type")
                    .is_some_and(|value| value == "\"history_cleared\"")
            })
            .collect::<Vec<_>>()
    };

    let (status, _) = request(
        &server.url,
        reqwest::Method::DELETE,
        "/api/v1/history",
        Some(&server.ingest_token),
    )
    .await;
    assert_eq!(status, 403);
    assert!(events().is_empty(), "denial must not emit a clear event");
    assert_eq!(reader.clear_history().await.unwrap().deleted, 1);
    assert_eq!(reader.clear_history().await.unwrap().deleted, 0);

    let cleared = events();
    assert_eq!(cleared.len(), 2);
    for (event, deleted) in cleared.iter().zip(["1", "0"]) {
        assert_eq!(event.field("key_id"), actor.id.to_string());
        assert_eq!(event.field("deleted"), deleted);
        assert_eq!(event.field("message"), "Query history cleared");
        assert_eq!(
            event.fields.keys().map(String::as_str).collect::<Vec<_>>(),
            ["deleted", "event_type", "key_id", "message"]
        );
        assert!(event.fields.values().all(|value| {
            !value.contains("private-query-payload") && !value.contains(&server.reader_token)
        }));
    }

    server.kill_app_database().await;
    assert!(reader.clear_history().await.is_err());
    assert_eq!(events().len(), 2, "storage failure must not emit success");
}
