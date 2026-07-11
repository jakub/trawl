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
//! - AC7: legacy auth.db quarantine (colliding ids expose nothing)
//! - AC8: audit poller sees out-of-process key mutations
//! - SSE: revoked-before-connect → 401 (handshake-only auth is accepted
//!   policy for this slice)

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{ensure_fixtures, pg_fixture_or_skip, setup, setup_in_dir, trawl_only};
use fleet_auth::{KeyStore, PrincipalKind};
use parking_lot::Mutex;
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

#[tokio::test]
async fn ac1_key_roundtrip_create_whoami_revoke_401() {
    let Some(server) = setup().await else { return };
    let store = KeyStore::from_pool(server.fx.pool());

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

#[tokio::test]
async fn ac3_public_probes_unauthenticated() {
    let Some(server) = setup().await else { return };
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

#[tokio::test]
async fn ac3_reader_blocked_from_export_stream_manage() {
    let Some(server) = setup().await else { return };
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

#[tokio::test]
async fn ac3_ingest_only_key_can_ingest_but_nothing_else() {
    let Some(server) = setup().await else { return };

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

#[tokio::test]
async fn ac3_admin_cannot_ingest() {
    let Some(server) = setup().await else { return };
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

#[tokio::test]
async fn ac3_grantless_key_403_on_every_authenticated_route() {
    let Some(server) = setup().await else { return };
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

#[tokio::test]
async fn ac3_grantless_key_never_reaches_rate_limiter() {
    // Tight bucket for every role. A grantless key never reaches the rate
    // limiter (mandatory policy 403s it first) — so no request can ever be
    // 429, and none can succeed.
    let Some(server) = common::setup_with_rate_limit(RateLimitConfig {
        admin: 1,
        analyst: 1,
        reader: 1,
        ingest: 1,
    })
    .await
    else {
        return;
    };

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

#[tokio::test]
async fn ac4_whoami_golden_json_per_role() {
    let Some(server) = setup().await else { return };

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

#[tokio::test]
async fn ac5_envelope_missing_and_malformed_and_invalid_tokens() {
    let Some(server) = setup().await else { return };

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

#[tokio::test]
async fn ac5_envelope_revoked_and_expired_tokens() {
    let Some(server) = setup().await else { return };
    let store = KeyStore::from_pool(server.fx.pool());

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

#[tokio::test]
async fn ac5_envelope_grantless_403() {
    let Some(server) = setup().await else { return };
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

#[tokio::test]
async fn ac5_envelope_pg_down_503_without_backend_detail() {
    let Some(server) = setup().await else { return };

    // Kill the auth backend under the running server.
    server.fx.kill_database().await;

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

/// Drive the real scheduler over sqlite schedule state and a pg keystore,
/// applying `mutate` to the owning key BEFORE the scheduler polls.
/// Returns the number of runs recorded after ~2.5s of 1s polling.
async fn scheduler_runs_after(
    mutate: impl AsyncFnOnce(&KeyStore, &fleet_auth::CreatedKey),
) -> Option<u64> {
    let fx = pg_fixture_or_skip().await?;
    let key_store = KeyStore::from_pool(fx.pool());
    let created = key_store
        .create_key(
            "sched-owner",
            PrincipalKind::Service,
            &trawl_only(Role::Analyst),
            None,
        )
        .await
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let store_db = tmp.path().join("store.db");
    let saved_store = trawl_auth::SavedQueryStore::open(&store_db).unwrap();
    let schedule_store = trawl_auth::ScheduleStore::open(&store_db).unwrap();

    let saved = saved_store
        .create(created.info.id, "gate-net", "* | head 1")
        .unwrap();
    let schedule = schedule_store
        .create_schedule(saved.id, created.info.id, 1, None)
        .unwrap();

    mutate(&key_store, &created).await;

    // Real parquet fixtures so a permitted run actually succeeds.
    let data_glob = ensure_fixtures();
    let base_dir = data_glob.trim_end_matches("/**/*.parquet").to_owned();
    let pool = trawl_server::pool::ExecutorPool::new(base_dir, 1, 1000, None);

    let schedule_store = Arc::new(Mutex::new(schedule_store));
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let handle = trawl_server::scheduler::spawn_scheduler(
        Arc::clone(&schedule_store),
        key_store.clone(),
        pool,
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

    let runs = schedule_store.lock().count_runs(schedule.id).unwrap();
    Some(runs)
}

#[tokio::test]
async fn ac6_scheduler_runs_for_live_key() {
    let Some(runs) = scheduler_runs_after(async |_store, _key| {}).await else {
        return;
    };
    assert!(runs >= 1, "control case: live key must execute, got {runs}");
}

#[tokio::test]
async fn ac6_scheduler_skips_revoked_key() {
    let Some(runs) =
        scheduler_runs_after(async |store: &KeyStore, key: &fleet_auth::CreatedKey| {
            store.revoke_key(&key.info.prefix).await.unwrap();
        })
        .await
    else {
        return;
    };
    assert_eq!(runs, 0, "revoked key must not execute schedules");
}

#[tokio::test]
async fn ac6_scheduler_skips_expired_key() {
    // Expiry can't be set post-hoc through the public API, so this case
    // creates its own short-lived key by re-running the harness inline.
    let Some(fx) = pg_fixture_or_skip().await else {
        return;
    };
    let key_store = KeyStore::from_pool(fx.pool());
    let created = key_store
        .create_key(
            "sched-expiring",
            PrincipalKind::Service,
            &trawl_only(Role::Analyst),
            Some(Duration::from_millis(100)),
        )
        .await
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let store_db = tmp.path().join("store.db");
    let saved_store = trawl_auth::SavedQueryStore::open(&store_db).unwrap();
    let schedule_store = trawl_auth::ScheduleStore::open(&store_db).unwrap();
    let saved = saved_store
        .create(created.info.id, "exp-net", "* | head 1")
        .unwrap();
    let schedule = schedule_store
        .create_schedule(saved.id, created.info.id, 1, None)
        .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await; // key expires

    let data_glob = ensure_fixtures();
    let base_dir = data_glob.trim_end_matches("/**/*.parquet").to_owned();
    let pool = trawl_server::pool::ExecutorPool::new(base_dir, 1, 1000, None);
    let schedule_store = Arc::new(Mutex::new(schedule_store));
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let handle = trawl_server::scheduler::spawn_scheduler(
        Arc::clone(&schedule_store),
        key_store,
        pool,
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

    let runs = schedule_store.lock().count_runs(schedule.id).unwrap();
    assert_eq!(runs, 0, "expired key must not execute schedules");
}

#[tokio::test]
async fn ac6_scheduler_skips_grant_stripped_key() {
    let Some(runs) =
        scheduler_runs_after(async |store: &KeyStore, key: &fleet_auth::CreatedKey| {
            store
                .revoke_assignment(&key.info.prefix, "trawl")
                .await
                .unwrap();
        })
        .await
    else {
        return;
    };
    assert_eq!(runs, 0, "grant-stripped key must not execute schedules");
}

// ---------------------------------------------------------------------------
// AC7: legacy auth.db quarantine
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac7_legacy_auth_db_quarantined_with_colliding_ids() {
    // Seed a populated legacy sqlite auth.db whose key ids WILL collide with
    // the fresh pg sequence (both start at 1).
    let tmp = tempfile::tempdir().unwrap();
    let legacy_db = tmp.path().join("auth.db");
    {
        let mut legacy_keys = trawl_auth::KeyStore::open(&legacy_db).unwrap();
        let legacy_key = legacy_keys
            .create_key(
                "legacy-key",
                trawl_auth::assignments::PrincipalKind::Service,
                &[trawl_auth::assignments::RoleAssignment {
                    app: "trawl".into(),
                    role: "analyst".into(),
                }],
                None,
            )
            .unwrap();
        assert_eq!(legacy_key.info.id, 1, "collision precondition");

        let history = trawl_auth::HistoryStore::open(&legacy_db).unwrap();
        history
            .record_query(1, "* | stats count()", 5, 42, "success")
            .unwrap();

        let saved = trawl_auth::SavedQueryStore::open(&legacy_db).unwrap();
        let legacy_saved = saved.create(1, "legacy-net", "* | head 1").unwrap();

        let schedules = trawl_auth::ScheduleStore::open(&legacy_db).unwrap();
        schedules
            .create_schedule(legacy_saved.id, 1, 1, None)
            .unwrap();
    }

    // Boot slice-1 trawld against the same datadir (fresh store.db).
    let Some(server) = setup_in_dir(tmp.path(), RateLimitConfig::default()).await else {
        return;
    };
    // analyst is the first key minted → pg id 1, colliding with legacy id 1.

    let (status, history) = request(
        &server.url,
        reqwest::Method::GET,
        "/api/v1/history",
        Some(&server.analyst_token),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        history["total"], 0,
        "legacy history must be invisible: {history}"
    );

    let (status, saved) = request(
        &server.url,
        reqwest::Method::GET,
        "/api/v1/saved",
        Some(&server.analyst_token),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        saved["queries"].as_array().map(Vec::len),
        Some(0),
        "legacy saved queries must be invisible: {saved}"
    );

    let (status, runs) = request(
        &server.url,
        reqwest::Method::GET,
        "/api/v1/runs",
        Some(&server.analyst_token),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(runs["total"], 0, "no legacy runs visible: {runs}");

    // The legacy file is preserved in place, never deleted.
    assert!(legacy_db.exists(), "auth.db must stay quarantined on disk");

    // And zero legacy schedules execute: the scheduler polls the FRESH
    // store.db (empty), so nothing can run — the /runs assertion above
    // covers it. The legacy schedule is still present in the quarantined
    // file, proving it was never migrated or drained.
    let legacy_schedules = trawl_auth::ScheduleStore::open(&legacy_db).unwrap();
    assert_eq!(
        legacy_schedules.list_enabled_schedules().unwrap().len(),
        1,
        "legacy schedule untouched in quarantined file"
    );

    // Config-level guard: pointing db_path at the legacy file is a loud
    // startup error (deb conffile upgrades preserve old trawld.toml).
    let toml = format!(
        r#"
[server]
[data]
path = "/data"
[auth]
db_path = "{}"
database_url = "postgres://unused/db"
"#,
        legacy_db.display()
    );
    let err = trawl_server::config::Config::from_toml(&toml).unwrap_err();
    assert!(err.to_string().contains("runbook"), "got: {err}");
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

#[tokio::test]
async fn ac8_audit_poller_emits_events_for_out_of_process_mutations() {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let writer = CaptureWriter(Arc::clone(&buf));
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || writer.clone())
        .try_init();

    let Some(fx) = pg_fixture_or_skip().await else {
        return;
    };
    let key_store = KeyStore::from_pool(fx.pool());

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
    let second = KeyStore::connect(&fx.database_url()).await.unwrap();
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

#[tokio::test]
async fn sse_stream_rejects_revoked_key_at_handshake() {
    let Some(server) = setup().await else { return };
    let store = KeyStore::from_pool(server.fx.pool());

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
