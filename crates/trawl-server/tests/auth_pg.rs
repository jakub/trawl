// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pg-backed auth integration matrix for the fleet-auth cutover
//! (ADR-0004 slice 1, issue #36 acceptance criteria).
//!
//! - AC1: fleet-minted key round-trip (create → whoami → revoke → 401)
//! - AC3: route matrix (public probes, per-role denials, grantless 403
//!   everywhere, no rate-limiter bypass)
//! - AC4: frozen /whoami wire shape per role (golden JSON)
//! - AC5: error envelope per failure class (401 opaque, 403 grantless,
//!   503 pg-down without backend detail)
//! - AC6: scheduler skips revoked / expired / grant-stripped keys
//! - AC8: audit poller sees out-of-process key mutations
//!
//! The slice-1 AC7 quarantine tests (legacy auth.db with colliding ids)
//! were deleted in ADR-0004 slice 3: the transitional sqlite store and its
//! quarantine apparatus no longer exist — the stores live in a dedicated
//! postgres database keyed by fleet ids from day one.
//! - SSE: revoked-before-connect → 401 (handshake-only auth is accepted
//!   policy for this slice)

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{ensure_fixtures, setup, trawl_only};
use fleet_auth::{KeyStore, PrincipalKind};
use parking_lot::Mutex;
use sqlx::PgPool;
use trawl_server::config::RateLimitConfig;
use trawl_server::policy::Role;

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

#[sqlx::test(migrations = false)]
async fn ac1_key_roundtrip_create_whoami_revoke_401(pool: PgPool) {
    let server = setup(pool).await;
    let store = KeyStore::from_pool(server.fleet_pool.clone());

    // create (what fleet-admin does)
    let created = store
        .create_key(
            "roundtrip",
            PrincipalKind::Human,
            &trawl_only(Role::Analyst),
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

#[sqlx::test(migrations = false)]
async fn ac3_public_probes_unauthenticated(pool: PgPool) {
    let server = setup(pool).await;
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

#[sqlx::test(migrations = false)]
async fn ac3_reader_blocked_from_export_stream_manage(pool: PgPool) {
    let server = setup(pool).await;
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
        assert_eq!(status, 401, "{method} {path} must be denied for reader");
        assert_eq!(
            body["error"]["code"], "unauthorized",
            "{method} {path} body: {body}"
        );
    }
}

#[sqlx::test(migrations = false)]
async fn ac3_ingest_only_key_can_ingest_but_nothing_else(pool: PgPool) {
    let server = setup(pool).await;

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
        let (status, _) = request(
            &server.url,
            method.clone(),
            path,
            Some(&server.ingest_token),
        )
        .await;
        assert_eq!(status, 401, "{method} {path} must be denied for ingest key");
    }
}

#[sqlx::test(migrations = false)]
async fn ac3_admin_cannot_ingest(pool: PgPool) {
    let server = setup(pool).await;
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
        401,
        "admin and ingest are orthogonal"
    );
}

#[sqlx::test(migrations = false)]
async fn ac3_grantless_key_403_on_every_authenticated_route(pool: PgPool) {
    let server = setup(pool).await;
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

#[sqlx::test(migrations = false)]
async fn ac3_grantless_key_never_reaches_rate_limiter(pool: PgPool) {
    // Tight bucket for every key. A grantless key never reaches the rate
    // limiter (mandatory policy 403s it first) — so no request can ever be
    // 429, and none can succeed.
    let server = common::setup_with_rate_limit(
        pool,
        RateLimitConfig {
            default_rpm: 1,
            ..RateLimitConfig::default()
        },
    )
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

// ---------------------------------------------------------------------------
// AC4: /whoami wire shape frozen (golden JSON per role)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = false)]
async fn ac4_whoami_golden_json_per_role(pool: PgPool) {
    let server = setup(pool).await;

    let cases = [
        (
            &server.admin_token,
            "admin-key",
            "admin",
            serde_json::json!([
                "query",
                "schema_read",
                "validate",
                "saved_query",
                "export",
                "stream",
                "query_cancel",
                "key_manage",
                "server_manage"
            ]),
        ),
        (
            &server.analyst_token,
            "test-key",
            "analyst",
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
            "reader",
            serde_json::json!(["query", "schema_read", "query_cancel"]),
        ),
        (
            &server.ingest_token,
            "ingest-key",
            "ingest",
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
            "assignments": [{ "app": "trawl", "role": role }],
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

#[sqlx::test(migrations = false)]
async fn ac5_envelope_missing_and_malformed_and_invalid_tokens(pool: PgPool) {
    let server = setup(pool).await;

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

#[sqlx::test(migrations = false)]
async fn ac5_envelope_revoked_and_expired_tokens(pool: PgPool) {
    let server = setup(pool).await;
    let store = KeyStore::from_pool(server.fleet_pool.clone());

    // revoked
    let revoked = store
        .create_key(
            "rev",
            PrincipalKind::Service,
            &trawl_only(Role::Reader),
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
            &trawl_only(Role::Reader),
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

#[sqlx::test(migrations = false)]
async fn ac5_envelope_grantless_403(pool: PgPool) {
    let server = setup(pool).await;
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

#[sqlx::test(migrations = false)]
async fn ac5_envelope_pg_down_503_without_backend_detail(pool: PgPool) {
    let server = setup(pool).await;

    // Kill the auth backend under the running server.
    server.kill_fleet_database().await;

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
/// applying `mutate` to the owning key BEFORE the scheduler polls.
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
            &trawl_only(Role::Analyst),
            key_ttl,
        )
        .await
        .unwrap();

    // Dedicated app-state database, migrated via the real boot path.
    let app_db_url = common::create_app_database(&pool).await;
    let storage = trawl_server::store::StorageState::connect(&app_db_url)
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
        .create_schedule(saved.id, created.info.id, 60, None)
        .await
        .unwrap();

    mutate(&key_store, &created).await;

    if pre_sleep_ms > 0 {
        tokio::time::sleep(Duration::from_millis(pre_sleep_ms)).await;
    }

    // Real parquet fixtures so a permitted run actually succeeds.
    let data_glob = ensure_fixtures();
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
async fn ac6_scheduler_skips_grant_stripped_key(pool: PgPool) {
    let runs = scheduler_runs_after(
        pool,
        None,
        0,
        async |store: &KeyStore, key: &fleet_auth::CreatedKey| {
            store
                .revoke_assignment(&key.info.prefix, "trawl")
                .await
                .unwrap();
        },
    )
    .await;
    assert_eq!(runs, 0, "grant-stripped key must not execute schedules");
}

/// A live key whose trawl grant is downgraded from analyst to a role lacking
/// saved-query authority must stop running its schedules — otherwise removing
/// a role leaves durable execution privilege behind.
async fn assert_downgrade_stops_schedules(pool: PgPool, new_role: &str) {
    let runs = scheduler_runs_after(
        pool,
        None,
        0,
        async move |store: &KeyStore, key: &fleet_auth::CreatedKey| {
            store
                .revoke_assignment(&key.info.prefix, "trawl")
                .await
                .unwrap();
            store
                .grant_assignment(
                    &key.info.prefix,
                    &fleet_auth::RoleAssignment {
                        app: "trawl".into(),
                        role: new_role.into(),
                    },
                )
                .await
                .unwrap();
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
    assert_downgrade_stops_schedules(pool, "reader").await;
}

#[sqlx::test(migrations = false)]
async fn ac6_scheduler_skips_analyst_downgraded_to_ingest(pool: PgPool) {
    // Ingest holds neither Query nor SavedQuery.
    assert_downgrade_stops_schedules(pool, "ingest").await;
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
        .with_writer(move || writer.clone())
        .try_init();

    let key_store = common::fleet_keystore(&pool).await;

    // Baseline key so the initial snapshot is non-trivial.
    key_store
        .create_key(
            "baseline",
            PrincipalKind::Service,
            &trawl_only(Role::Reader),
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

    // Out-of-process mutation: a SECOND connection to the same database
    // (what fleet-admin does).
    let second = KeyStore::connect(&common::fleet_database_url(&pool))
        .await
        .unwrap();
    let created = second
        .create_key(
            "made-by-fleet-admin",
            PrincipalKind::Human,
            &trawl_only(Role::Admin),
            None,
        )
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
}

// ---------------------------------------------------------------------------
// SSE: revoked-before-connect → 401 (handshake-only auth, accepted policy)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = false)]
async fn sse_stream_rejects_revoked_key_at_handshake(pool: PgPool) {
    let server = setup(pool).await;
    let store = KeyStore::from_pool(server.fleet_pool.clone());

    let created = store
        .create_key(
            "sse",
            PrincipalKind::Service,
            &trawl_only(Role::Analyst),
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
// AC9 (slice 3): storage-loss observability + redacted store errors
// ---------------------------------------------------------------------------

/// Killing the app-state database under a running server degrades /health
/// (HTTP 200 — queries still serve) and turns store-backed endpoints into
/// redacted 503s. Never a pg diagnostic on the wire.
#[sqlx::test(migrations = false)]
async fn storage_loss_degrades_health_and_503s_store_endpoints(pool: PgPool) {
    let server = setup(pool).await;
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
            let storage = body["checks"]["storage_db"].as_str().unwrap_or_default();
            assert!(storage.starts_with("error:"), "got: {body}");
            assert_eq!(body["checks"]["duckdb"], "ok", "got: {body}");
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
#[sqlx::test(migrations = false)]
async fn dashboard_reports_schedule_count_from_pg(pool: PgPool) {
    let server = setup(pool).await;
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
