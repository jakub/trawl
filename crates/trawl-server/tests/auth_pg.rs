// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pg-backed auth integration matrix over the fleet-auth keystore
//! (ADR-0004).
//!
//! - AC1: fleet-minted key round-trip (create → whoami → revoke → 401)
//! - AC3: route matrix (public probes, per-role denials, grantless 403
//!   everywhere, no rate-limiter bypass)
//! - AC4: frozen /whoami wire shape per role (golden JSON)
//! - AC5: error envelope per failure class (401 credential, 403 authorization,
//!   503 pg-down without backend detail)
//! - AC6: scheduler skips revoked / expired / grant-stripped keys
//! - AC8: audit poller sees out-of-process key mutations
//! - SSE: revoked-before-connect → 401 (handshake-only auth is accepted
//!   policy)

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{roles, seed_data_root, setup};
use fleet_auth::{KeyStore, PrincipalKind};
use parking_lot::Mutex;
use sqlx::PgPool;
use trawl_server::config::RateLimitConfig;

/// Raw reqwest client accepting the self-signed test cert.
fn raw_client() -> reqwest::Client {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap()
}

/// Issue a request with an optional bearer token; return (status, json body).
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
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    let body = resp.json::<serde_json::Value>().await.unwrap_or_default();
    (status, body)
}

/// Every authenticated route class (method, path).
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
    ]
}

// ---------------------------------------------------------------------------
// AC1: fleet-minted key round-trip
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn ac1_key_roundtrip_create_whoami_revoke_401() {
    let server = setup().await;
    let store = KeyStore::from_pool(server.fleet_pool.clone());

    // create (what fleet-admin does)
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

    // whoami works
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

    // revoke → immediate 401 (per-request liveness, no TTL cache)
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

// ---------------------------------------------------------------------------
// AC3: route matrix
// ---------------------------------------------------------------------------

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

    // Can ingest.
    let resp = raw_client()
        .post(format!("{}/api/v1/ingest", server.url))
        .header("authorization", format!("Bearer {}", server.ingest_token))
        .header("content-type", "application/x-ndjson")
        .body(r#"{"service":"t","message":"m"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    // Nothing else.
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
    // Tight bucket for every key. A grantless key never reaches the rate
    // limiter (mandatory policy 403s it first) — so no request can ever be
    // 429, and none can succeed.
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

/// Roles-as-data addition to the grantless matrix: a key whose role EXISTS
/// but resolves zero permissions anywhere is still 403 on every
/// authenticated route — holding a role is not capability, permissions are.
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

// ---------------------------------------------------------------------------
// AC5 (rate_rpm): a role ceiling overrides the class defaults end-to-end,
// spent separately on the interactive and ingest classes.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn ac5_rate_rpm_role_ceiling_spent_separately_per_class() {
    // Generous class defaults so any 429 observed can only come from the
    // role's rate_rpm override — proving override-not-max.
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

    // Interactive class: burst of 2, then 429 — despite default_rpm 10_000.
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

    // Ingest class: its own budget of 2 — the interactive spend above must
    // not have consumed it.
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

// ---------------------------------------------------------------------------
// AC4: /whoami wire shape frozen (golden JSON per role)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn ac4_whoami_golden_json_per_role() {
    let server = setup().await;

    // The frozen permission set each seeded role resolves to.
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

        // The prefix is dynamic (first 8 chars of the token body); everything
        // else is asserted as the exact frozen wire shape.
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

// ---------------------------------------------------------------------------
// AC5: error envelope per failure class
// ---------------------------------------------------------------------------

/// The exact opaque 401 body for every credential-failure class.
fn opaque_401() -> serde_json::Value {
    serde_json::json!({ "error": { "code": "auth_error", "message": "authentication failed" } })
}

#[tokio::test(flavor = "multi_thread")]
async fn ac5_envelope_missing_and_malformed_and_invalid_tokens() {
    let server = setup().await;

    // missing
    let (status, body) = request(&server.url, reqwest::Method::GET, "/api/v1/whoami", None).await;
    assert_eq!(status, 401);
    assert_eq!(body, opaque_401());

    // malformed scheme
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

    // not a flt_ token at all
    let (status, body) = request(
        &server.url,
        reqwest::Method::GET,
        "/api/v1/whoami",
        Some("not-a-fleet-token"),
    )
    .await;
    assert_eq!(status, 401);
    assert_eq!(body, opaque_401());

    // well-formed but unknown token
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

    // revoked
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

    // expired
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
                "data_path": "ok"
            },
            "version": version
        })
    );

    // Kill the auth backend under the running server.
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
                "data_path": "ok"
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

// ---------------------------------------------------------------------------
// AC6: scheduler gates on key liveness + usable trawl grant
// ---------------------------------------------------------------------------

/// Drive the real scheduler over pg schedule state and a pg keystore,
/// applying `mutate` to the owning key before the scheduler polls.
/// Returns the number of runs recorded after ~2.5s of 1s polling.
///
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

    // Dedicated app-state database, migrated via the real boot path
    // (`from_pool` is where the advisory lock and the migration live;
    // `connect` only adds the pool). The pool is the fixture's, sized by
    // `APP_POOL_MAX`: `StorageState::connect` would build trawld's
    // PRODUCTION pool of 8 on top of the 5 this test already holds from
    // `#[sqlx::test]`, which is most of a per-test connection budget spent
    // on a scheduler that runs one query.
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
        // 60s is the store minimum; the first poll runs regardless of interval
        // (never-run schedules are always due), so cadence is immaterial here.
        .create_schedule(saved.id, created.info.id, 60, None, None, 0, chrono::Utc::now())
        .await
        .unwrap();

    mutate(&key_store, &created).await;

    if pre_sleep_ms > 0 {
        tokio::time::sleep(Duration::from_millis(pre_sleep_ms)).await;
    }

    // Real parquet fixtures so a permitted run actually succeeds. The
    // root is this test's own copy: the scheduler WRITES report output
    // under `scheduled/`, which no other test may see.
    let data_dir = tempfile::tempdir().expect("scheduler data root");
    let data_glob = seed_data_root(data_dir.path());
    let base_dir = data_glob.trim_end_matches("/**/*.parquet").to_owned();
    let exec_pool = trawl_server::pool::ExecutorPool::new(base_dir, 1, 1000, None);

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let handle = trawl_server::scheduler::spawn_scheduler(
        storage.schedule.clone(),
        key_store.clone(),
        exec_pool,
        trawl_server::config::SchedulerConfig {
            enabled: true,
            poll_interval_secs: 1,
            report_max_rows: 1000,
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
    // Short-lived key + a sleep past its expiry before the scheduler starts.
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

/// A live key whose role set is downgraded from analyst to a role lacking
/// saved-query authority must stop running its schedules — otherwise removing
/// a role leaves durable execution privilege behind.
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
    // Reader holds Query but not SavedQuery: it cannot manage saved queries
    // interactively, so it must not keep running them on a schedule.
    assert_downgrade_stops_schedules(pool, "trawl-reader").await;
}

#[sqlx::test(migrations = false)]
async fn ac6_scheduler_skips_analyst_downgraded_to_ingest(pool: PgPool) {
    // Ingest holds neither Query nor SavedQuery.
    assert_downgrade_stops_schedules(pool, "trawl-ingest").await;
}

/// The key keeps its role; the role itself loses `saved_query`. The
/// scheduler's Query-AND-SavedQuery gate must observe the mutation on its
/// next liveness poll.
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

// ---------------------------------------------------------------------------
// AC8: audit poller sees out-of-process mutations
// ---------------------------------------------------------------------------

/// `std::io::Write` sink capturing tracing output for assertions.
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

#[sqlx::test(migrations = false)]
async fn ac8_audit_poller_emits_events_for_out_of_process_mutations(pool: PgPool) {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let writer = CaptureWriter(Arc::clone(&buf));
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        // Plain text: the assertions below match field names literally, and
        // ANSI styling splits `permissions=` with escape codes.
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .try_init();

    let key_store = common::fleet_keystore(&pool).await;

    // Baseline key so the initial snapshot is non-trivial.
    let baseline = key_store
        .create_key(
            "baseline",
            PrincipalKind::Service,
            &roles(&["trawl-reader"]),
            None,
        )
        .await
        .unwrap();

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let handle = trawl_server::audit::spawn_audit_task(
        key_store.clone(),
        Duration::from_millis(200),
        shutdown_rx,
    );
    // Let the initial snapshot land before mutating.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Out-of-process mutation: a second connection to the same database
    // (what fleet-admin does).
    // A SECOND pool on the same database, sized by the fixture rather than
    // by `KeyStore::connect`'s production ceiling of 8: what the test needs
    // is a connection the audit poller does not own, not a daemon's worth
    // of them.
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
    tokio::time::sleep(Duration::from_millis(600)).await;

    // Privilege escalation with no key mutation at all: the role the baseline
    // key already holds gains a permission. Plus a role assignment.
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
    tokio::time::sleep(Duration::from_millis(600)).await;

    second.revoke_key(&created.info.prefix).await.unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;

    let _ = shutdown_tx.send(true);
    let _ = handle.await;
    second.pool().close().await;

    let log = String::from_utf8(buf.lock().clone()).unwrap();
    assert!(
        log.contains("key_created") && log.contains(&created.info.prefix),
        "audit must emit key_created for the new key; log:\n{log}"
    );
    assert!(
        log.contains("key_revoked"),
        "audit must emit key_revoked; log:\n{log}"
    );
    // The effective capability, not just the mutable role name, is recorded.
    let created_line = log
        .lines()
        .find(|l| l.contains("key_created") && l.contains("detected by audit"))
        .unwrap_or_default();
    assert!(
        created_line.contains("permissions=") && created_line.contains("trawl:"),
        "key_created must record resolved permissions; line:\n{created_line}"
    );
    // Mutating a role escalates every key holding it — that must be audited.
    let role_line = log
        .lines()
        .find(|l| l.contains("role_changed"))
        .unwrap_or_default();
    assert!(
        role_line.contains("trawl-reader") && role_line.contains("trawl:server_manage"),
        "audit must emit role_changed naming the added permission; log:\n{log}"
    );
    let assign_line = log
        .lines()
        .find(|l| l.contains("key_roles_changed"))
        .unwrap_or_default();
    assert!(
        assign_line.contains(&baseline.info.prefix) && assign_line.contains("trawl-admin"),
        "audit must emit key_roles_changed for the reassigned key; log:\n{log}"
    );
}

// ---------------------------------------------------------------------------
// SSE: revoked-before-connect → 401 (handshake-only auth, accepted policy)
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// AC9: storage-loss observability + redacted store errors
// ---------------------------------------------------------------------------

/// Killing the app-state database under a running server degrades /health
/// (HTTP 200 — queries still serve) and turns store-backed endpoints into
/// redacted 503s. Never a pg diagnostic on the wire.
#[tokio::test(flavor = "multi_thread")]
async fn storage_loss_degrades_health_and_503s_store_endpoints() {
    let server = setup().await;
    let client = raw_client();

    server.kill_app_database().await;

    // The storage ping is memoised (~5s TTL) — poll until degradation
    // propagates. The HTTP status must stay 200 throughout: app-state loss
    // is non-critical (degraded), never a liveness failure.
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
                    "data_path": "ok"
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

    // Store-backed endpoint: 503 with the fixed redacted envelope.
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

/// `/api/v1/dashboard` reports the enabled-schedule count from postgres:
/// the async snapshot collector polls the store and the sync snapshot
/// carries it onto the wire.
#[tokio::test(flavor = "multi_thread")]
async fn dashboard_reports_schedule_count_from_pg() {
    let server = setup().await;
    let client = raw_client();

    // Create a saved query + schedule through the API.
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

    // The collector ticks every second; poll the dashboard until the count
    // lands.
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
        .delete(format!("{}/api/v1/history?key_id=0", server.url))
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
