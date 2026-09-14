// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! End-to-end tests for the catalog schema surface (ADR-0009):
//! `/api/v1/schema` served from the catalog with `?service=`/`?all=`
//! windowing, and the three read routes `/api/v1/schema/fields`,
//! `/api/v1/schema/field?name=`, `/api/v1/schema/conflicts`.
//!
//! Modeled on `field_catalog.rs`: ingest through the real handler, compact
//! through the real compaction path (with the catalog context the daemon
//! wires), read through the real API.

mod common;

use std::time::Duration;

use common::{TestServer, assert_bookkeeping_quiet, bookkeeping_timeouts, setup_in_dir_with_data};
use serde_json::json;
use sqlx::Connection as _;
use trawl_client::HttpClient;
use trawl_server::catalog::CatalogContext;
use trawl_server::config::RateLimitConfig;

/// A current RFC 3339 timestamp so `last=1h` queries cover the events.
fn now_ts() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

/// The catalog context exactly as trawld's main wires it.
fn catalog_ctx(server: &TestServer) -> CatalogContext {
    CatalogContext {
        store: server.state.storage.catalog.clone(),
        cache: server.state.query.field_catalog.clone(),
    }
}

/// Run one compaction tick over the server's WAL with the catalog wired.
async fn compact_tick(server: &TestServer, wal_dir: &std::path::Path, data_dir: &std::path::Path) {
    let hot_buffer = server
        .state
        .query
        .hot_buffer
        .as_ref()
        .expect("ingest-enabled server has a hot buffer");
    let ctx = catalog_ctx(server);
    let errors = trawl_server::ingest::compaction::compact_once(
        wal_dir,
        data_dir,
        Duration::ZERO,
        false,
        Some(hot_buffer),
        500,
        "2GB",
        Some(&ctx),
    )
    .await
    .expect("compaction tick must succeed");
    assert_eq!(errors, 0, "no compaction errors expected");
}

struct Harness {
    server: TestServer,
    wal_dir: std::path::PathBuf,
    data_dir: std::path::PathBuf,
    ingest: HttpClient,
    query: HttpClient,
    raw: reqwest::Client,
}

impl Harness {
    /// GET an API path with the given token; return (status, parsed body).
    async fn get(&self, token: &str, path_and_query: &str) -> (u16, serde_json::Value) {
        let resp = self
            .raw
            .get(format!("{}/api/v1{path_and_query}", self.server.url))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("request");
        let status = resp.status().as_u16();
        let body = resp
            .json::<serde_json::Value>()
            .await
            .unwrap_or(serde_json::Value::Null);
        (status, body)
    }

    /// POST an API path with the given token; return (status, parsed body).
    async fn post(
        &self,
        token: &str,
        path: &str,
        body: serde_json::Value,
    ) -> (u16, serde_json::Value) {
        let resp = self
            .raw
            .post(format!("{}/api/v1{path}", self.server.url))
            .header("Authorization", format!("Bearer {token}"))
            .json(&body)
            .send()
            .await
            .expect("request");
        let status = resp.status().as_u16();
        let body = resp
            .json::<serde_json::Value>()
            .await
            .unwrap_or(serde_json::Value::Null);
        (status, body)
    }

    /// DELETE an API path with the given token; return (status, body text).
    async fn delete(&self, token: &str, path_and_query: &str) -> (u16, String) {
        let resp = self
            .raw
            .delete(format!("{}/api/v1{path_and_query}", self.server.url))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("request");
        let status = resp.status().as_u16();
        (status, resp.text().await.unwrap_or_default())
    }

    /// Scrape `/metrics` (unauthenticated, outside `/api/v1`).
    async fn metrics(&self) -> String {
        self.raw
            .get(format!("{}/metrics", self.server.url))
            .send()
            .await
            .expect("GET /metrics")
            .text()
            .await
            .expect("metrics body")
    }

    /// Schema column names in response order.
    fn column_names(schema: &serde_json::Value) -> Vec<String> {
        schema["columns"]
            .as_array()
            .expect("columns array")
            .iter()
            .map(|c| c["name"].as_str().unwrap().to_owned())
            .collect()
    }
}

async fn harness() -> Harness {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let wal_dir = root.join("wal");
    let data_dir = root.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let data_path = data_dir.to_str().unwrap().to_owned();

    let server = setup_in_dir_with_data(&root, data_path, RateLimitConfig::default()).await;
    // Leak the tempdir so it survives the server (cleaned up by OS).
    std::mem::forget(tmp);

    let ingest = HttpClient::new_insecure(&server.url, &server.ingest_token).unwrap();
    let query = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let raw = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();
    Harness {
        server,
        wal_dir,
        data_dir,
        ingest,
        query,
        raw,
    }
}

/// Ingest `events` and run one compaction tick.
async fn ingest_and_compact(h: &Harness, events: &[serde_json::Value]) {
    let resp = h.ingest.ingest(events).await.expect("ingest");
    assert_eq!(resp.accepted, events.len());
    compact_tick(&h.server, &h.wal_dir, &h.data_dir).await;
}

/// One field's row out of a `/schema/fields` body.
fn field_row<'a>(body: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    body["fields"]
        .as_array()
        .expect("fields array")
        .iter()
        .find(|f| f["name"] == name)
        .unwrap_or_else(|| panic!("field {name} listed"))
}

fn event(service: &str, extra: &serde_json::Value) -> serde_json::Value {
    let mut base = json!({
        "service": service, "env": "prod", "host": "web01",
        "timestamp": now_ts(), "message": "m",
    });
    base.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    base
}

/// Acceptance: an ingested field lands in `/api/v1/schema` with its pinned
/// type, and the query path agrees with the advertised type.
#[tokio::test(flavor = "multi_thread")]
async fn ingested_field_lands_in_schema_with_pinned_type() {
    let h = harness().await;
    ingest_and_compact(&h, &[event("nginx", &json!({"duration": 42}))]).await;

    let (status, schema) = h.get(&h.server.analyst_token, "/schema").await;
    assert_eq!(status, 200);
    let col = schema["columns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "duration")
        .expect("ingested field appears in /schema");
    assert_eq!(
        col["type"], "BIGINT",
        "the catalog pin is the advertised type"
    );

    // The query path agrees: a numeric comparison over the field works.
    let result = h
        .query
        .query_paginated("last=1h | where duration >= 42", None, None)
        .await
        .expect("numeric comparison over the pinned field");
    assert_eq!(result.result.row_count(), 1);
}

/// Acceptance: `?service=` returns only that service's fields.
#[tokio::test(flavor = "multi_thread")]
async fn schema_service_param_scopes_fields() {
    let h = harness().await;
    ingest_and_compact(&h, &[event("svc-a", &json!({"alpha_field": 1}))]).await;
    ingest_and_compact(&h, &[event("svc-b", &json!({"beta_field": 2}))]).await;

    let (status, schema) = h
        .get(&h.server.analyst_token, "/schema?service=svc-a")
        .await;
    assert_eq!(status, 200);
    let names = Harness::column_names(&schema);
    assert!(
        names.iter().any(|n| n == "alpha_field"),
        "svc-a's field present: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n == "beta_field"),
        "svc-b's field absent under ?service=svc-a: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "_time"),
        "envelope fields are observed for every compacted service"
    );
}

/// Regression: the unscoped column set is TTL-cached (its aggregate spans
/// every service, and the service axis is client-chosen and unbounded)
/// while `?service=` is served fresh. The two must not share a slot — a
/// scoped request must neither be answered from the unscoped cache nor
/// poison it for the next unscoped caller.
#[tokio::test(flavor = "multi_thread")]
async fn schema_service_scope_bypasses_the_unscoped_cache() {
    let h = harness().await;
    ingest_and_compact(&h, &[event("svc-a", &json!({"alpha_field": 1}))]).await;
    ingest_and_compact(&h, &[event("svc-b", &json!({"beta_field": 2}))]).await;

    // Prime the unscoped cache: both services' fields.
    let (_, schema) = h.get(&h.server.analyst_token, "/schema").await;
    let names = Harness::column_names(&schema);
    assert!(
        names.iter().any(|n| n == "beta_field"),
        "unscoped listing spans services: {names:?}"
    );

    // Scoped: bypasses the cache entirely, so svc-b's field is gone.
    let (_, schema) = h
        .get(&h.server.analyst_token, "/schema?service=svc-a")
        .await;
    let names = Harness::column_names(&schema);
    assert!(
        !names.iter().any(|n| n == "beta_field"),
        "a scoped request is never served the cached unscoped listing: {names:?}"
    );

    // And the unscoped listing is unchanged after it.
    let (_, schema) = h.get(&h.server.analyst_token, "/schema").await;
    let names = Harness::column_names(&schema);
    assert!(
        names.iter().any(|n| n == "beta_field"),
        "a scoped request must not poison the unscoped cache: {names:?}"
    );
}

/// Acceptance: a field whose last observation predates the retention window
/// is absent from `/schema` by default and present with `?all=true`.
#[tokio::test(flavor = "multi_thread")]
async fn aged_out_field_windowed_away_unless_all() {
    let h = harness().await;
    ingest_and_compact(&h, &[event("nginx", &json!({"old_field": 7}))]).await;

    // Age the observation past the default 90-day retention window.
    let mut conn = sqlx::postgres::PgConnection::connect(&h.server.app_db_url)
        .await
        .expect("connect app db");
    sqlx::query(
        "UPDATE field_services SET last_seen = now() - interval '100 days'
         WHERE field = 'old_field'",
    )
    .execute(&mut conn)
    .await
    .expect("age observation");

    let (_, schema) = h.get(&h.server.analyst_token, "/schema").await;
    let names = Harness::column_names(&schema);
    assert!(
        !names.iter().any(|n| n == "old_field"),
        "aged-out field is windowed away by default: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "_time"),
        "envelope pins survive the window"
    );

    let (_, schema) = h.get(&h.server.analyst_token, "/schema?all=true").await;
    let names = Harness::column_names(&schema);
    assert!(
        names.iter().any(|n| n == "old_field"),
        "?all=true lifts the window: {names:?}"
    );
}

/// A conflicting field that has not been conflicting for LONG carries no
/// verdict at all — not a null one, no key: an install where a shipper had
/// one bad afternoon must read exactly as it did before the analyzer
/// shipped.
#[tokio::test(flavor = "multi_thread")]
async fn a_freshly_conflicting_field_carries_no_verdict() {
    let h = harness().await;
    let timeouts_before = bookkeeping_timeouts(&h.server.url).await;
    ingest_and_compact(&h, &[event("svc-a", &json!({"duration": 4200}))]).await;
    for _ in 0..4 {
        ingest_and_compact(&h, &[event("svc-b", &json!({"duration": "N/A"}))]).await;
    }

    let (status, body) = h.get(&h.server.analyst_token, "/schema/fields").await;
    assert_eq!(status, 200);
    let row = field_row(&body, "duration");
    assert_bookkeeping_quiet(&timeouts_before, &bookkeeping_timeouts(&h.server.url).await);
    assert!(
        row.get("verdict").is_none(),
        "volume without span is not degraded: {row}"
    );
    assert!(row["conflict_count"].as_u64().unwrap() >= 4, "{row}");

    let (_, body) = h
        .get(&h.server.analyst_token, "/schema/field?name=duration")
        .await;
    assert!(body.get("verdict").is_none(), "{body}");
}

/// Acceptance: a pin that has been shelving values for over a day carries
/// the verdict on both read routes, and the evidence rows carry the misfit
/// samples the CLI and SPA render.
#[tokio::test(flavor = "multi_thread")]
async fn a_degraded_field_carries_the_verdict_and_its_samples() {
    let h = harness().await;
    let timeouts_before = bookkeeping_timeouts(&h.server.url).await;
    ingest_and_compact(&h, &[event("svc-a", &json!({"duration": 4200}))]).await;
    for value in ["N/A", "pending", "N/A"] {
        ingest_and_compact(&h, &[event("svc-b", &json!({"duration": value}))]).await;
    }

    // The evidence is real; only its age is simulated. `first_at` is the one
    // thing a test cannot wait 24 hours for.
    let mut conn = sqlx::postgres::PgConnection::connect(&h.server.app_db_url)
        .await
        .expect("connect app db");
    sqlx::query(
        "UPDATE field_conflict_stats SET first_at = now() - interval '48 hours'
         WHERE field = 'duration'",
    )
    .execute(&mut conn)
    .await
    .expect("backdate the evidence");

    let (status, fields) = h.get(&h.server.analyst_token, "/schema/fields").await;
    assert_eq!(status, 200);
    let verdict = field_row(&fields, "duration")["verdict"].clone();
    assert_bookkeeping_quiet(&timeouts_before, &bookkeeping_timeouts(&h.server.url).await);
    assert_eq!(verdict["services"], 1, "one sender is enough: {verdict}");
    assert_eq!(verdict["episodes"], 3, "{verdict}");
    assert_eq!(verdict["rows_shelved"], 3, "{verdict}");
    assert_eq!(
        verdict["suggested_to"], "VARCHAR",
        "strings under a BIGINT pin suggest text: {verdict}"
    );
    assert!(
        verdict["since"].as_str().unwrap().ends_with('Z'),
        "{verdict}"
    );
    let samples: Vec<&str> = verdict["samples"]
        .as_array()
        .expect("samples")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(samples.contains(&"N/A"), "samples: {samples:?}");
    assert!(samples.contains(&"pending"), "samples: {samples:?}");

    // The case file carries the same verdict plus per-row evidence.
    let (status, body) = h
        .get(&h.server.analyst_token, "/schema/field?name=duration")
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["verdict"]["suggested_to"], "VARCHAR", "{body}");
    let row = body["conflicts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| !c["samples"].as_array().unwrap().is_empty())
        .expect("the evidence rows carry their samples");
    assert!(
        ["N/A", "pending"].contains(&row["samples"][0].as_str().unwrap()),
        "{row}"
    );

    // A field with no conflict evidence at all is never badged.
    assert!(
        field_row(&fields, "message").get("verdict").is_none(),
        "an unconflicted envelope field carries no verdict key"
    );

    let (status, _) = h
        .get(&h.server.coastwatch_only_token, "/schema/fields")
        .await;
    assert_eq!(
        status, 403,
        "the verdict rides the existing SchemaRead gate"
    );
}

/// Acceptance: a query that binds a degraded field is stamped with the
/// incomplete-results notice, including when it projects the field away,
/// while a query that binds none carries no key at all, and the gauge
/// reflects the count after a refresh pass.
#[tokio::test(flavor = "multi_thread")]
async fn a_query_binding_a_degraded_field_is_stamped() {
    let h = harness().await;
    let timeouts_before = bookkeeping_timeouts(&h.server.url).await;
    ingest_and_compact(&h, &[event("svc-a", &json!({"duration": 4200}))]).await;
    for value in ["N/A", "pending", "N/A"] {
        ingest_and_compact(&h, &[event("svc-b", &json!({"duration": value}))]).await;
    }
    let mut conn = sqlx::postgres::PgConnection::connect(&h.server.app_db_url)
        .await
        .expect("connect app db");
    sqlx::query(
        "UPDATE field_conflict_stats SET first_at = now() - interval '48 hours'
         WHERE field = 'duration'",
    )
    .execute(&mut conn)
    .await
    .expect("backdate the evidence");

    // Before the tick loads it, the query path knows nothing — the notice is
    // bounded by one refresh interval, deliberately.
    let (status, body) = h
        .post(
            &h.server.analyst_token,
            "/query",
            json!({"query": "last=1h | where duration > 1 | table host"}),
        )
        .await;
    assert_eq!(status, 200);
    assert!(
        body.get("degraded_fields").is_none(),
        "stale-but-clean until the tick runs: {body}"
    );

    trawl_server::schema_refresh::refresh_degraded_fields(&h.server.state).await;

    // Bound in a filter, projected away by `table` — the incomplete case.
    let (status, body) = h
        .post(
            &h.server.analyst_token,
            "/query",
            json!({"query": "last=1h | where duration > 1 | table host"}),
        )
        .await;
    assert_eq!(status, 200);
    assert_bookkeeping_quiet(&timeouts_before, &bookkeeping_timeouts(&h.server.url).await);
    assert_eq!(
        body["degraded_fields"],
        json!(["duration"]),
        "a field filtered on and projected away is exactly the notice's case: {body}"
    );

    // A query binding nothing degraded carries no key.
    let (_, body) = h
        .post(
            &h.server.analyst_token,
            "/query",
            json!({"query": "last=1h | stats count() by service"}),
        )
        .await;
    assert!(
        body.get("degraded_fields").is_none(),
        "healthy queries are byte-identical to before: {body}"
    );

    let metrics = h.metrics().await;
    assert!(
        metrics
            .lines()
            .any(|l| l.trim() == "trawl_catalog_degraded_fields 1"),
        "the gauge carries the count after a refresh pass: {metrics}"
    );
}

/// Acceptance: `/api/v1/schema/services` badges a service with the degraded
/// fields it actually conflicted on. A service that merely carries the same
/// column, having never disagreed with its pin, is not badged.
///
/// This is the false-positive case the wire field exists to prevent: the
/// client-side join a SPA could otherwise do (`columns` ∩ degraded set)
/// would badge both services here, and only one of them has anything to fix.
#[tokio::test(flavor = "multi_thread")]
async fn schema_services_badges_only_the_service_that_conflicted() {
    let h = harness().await;
    let timeouts_before = bookkeeping_timeouts(&h.server.url).await;
    // svc-a pins duration BIGINT and never disagrees with it again; svc-b
    // sends strings under that pin, which the conform shelves.
    ingest_and_compact(&h, &[event("svc-a", &json!({"duration": 4200}))]).await;
    for value in ["N/A", "pending", "N/A"] {
        ingest_and_compact(&h, &[event("svc-b", &json!({"duration": value}))]).await;
    }

    // Only the age of the evidence is simulated: the span half of the
    // degrade gate is the one thing a test cannot wait 24 hours for.
    let mut conn = sqlx::postgres::PgConnection::connect(&h.server.app_db_url)
        .await
        .expect("connect app db");
    sqlx::query(
        "UPDATE field_conflict_stats SET first_at = now() - interval '48 hours'
         WHERE field = 'duration'",
    )
    .execute(&mut conn)
    .await
    .expect("backdate the evidence");

    // The badge is stamped from the schema-refresh tick's caches, so run the
    // real job: it fills the per-service schema (footer walk) and the
    // degraded snapshot behind it.
    let _refresh = trawl_server::schema_refresh::spawn_schema_refresh(h.server.state.clone());
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let (status, _) = h.get(&h.server.analyst_token, "/schema/services").await;
        assert!(
            std::time::Instant::now() < deadline,
            "schema refresh never populated the service cache"
        );
        if status == 200 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Re-run the degraded half explicitly: the tick's own pass may have run
    // before the service cache existed, in which case it had no service axis
    // to key the attribution read on.
    trawl_server::schema_refresh::refresh_degraded_fields(&h.server.state).await;

    let (status, body) = h.get(&h.server.analyst_token, "/schema/services").await;
    assert_eq!(status, 200);
    let services = body["services"].as_array().expect("services array");
    let svc = |name: &str| {
        services
            .iter()
            .find(|s| s["name"] == name)
            .unwrap_or_else(|| panic!("service {name} listed: {body}"))
    };

    // Both services carry the column — that is exactly what makes the join
    // tempting and wrong.
    for name in ["svc-a", "svc-b"] {
        assert!(
            svc(name)["columns"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["name"] == "duration"),
            "{name} carries the duration column"
        );
    }

    assert_bookkeeping_quiet(&timeouts_before, &bookkeeping_timeouts(&h.server.url).await);
    assert_eq!(
        svc("svc-b")["degraded_fields"],
        json!(["duration"]),
        "the sender whose values the pin shelved is badged: {body}"
    );
    assert!(
        svc("svc-a").get("degraded_fields").is_none(),
        "a service that never conflicted carries no degraded_fields key at all: {body}"
    );

    let (status, _) = h
        .get(&h.server.coastwatch_only_token, "/schema/services")
        .await;
    assert_eq!(
        status, 403,
        "the badge rides the existing SchemaRead gate, not a new one"
    );

    // A store error keeps the entire previous snapshot, both halves or
    // neither: clearing on a postgres blip would silently un-badge the
    // install, and publishing a half-built one would un-badge every service
    // while leaving the query notice standing.
    sqlx::query("DROP TABLE field_conflict_stats")
        .execute(&mut conn)
        .await
        .expect("break the evidence read");
    trawl_server::schema_refresh::refresh_degraded_fields(&h.server.state).await;

    let (status, body) = h.get(&h.server.analyst_token, "/schema/services").await;
    assert_eq!(status, 200);
    let services = body["services"].as_array().expect("services array");
    let svc_b = services.iter().find(|s| s["name"] == "svc-b").unwrap();
    assert_eq!(
        svc_b["degraded_fields"],
        json!(["duration"]),
        "the badge half survives a failed refresh: {body}"
    );

    let (_, body) = h
        .post(
            &h.server.analyst_token,
            "/query",
            json!({"query": "last=1h | where duration > 1 | table host"}),
        )
        .await;
    assert_eq!(
        body["degraded_fields"],
        json!(["duration"]),
        "and so does the notice half — the two are one generation: {body}"
    );
}

/// Acceptance: a type conflict surfaces on `/schema/conflicts`, the field
/// detail lists both services, and the nulled original stays findable via
/// `_raw` search.
#[tokio::test(flavor = "multi_thread")]
async fn conflict_evidence_surfaces_on_the_read_routes() {
    let h = harness().await;
    let timeouts_before = bookkeeping_timeouts(&h.server.url).await;
    // svc-a pins duration BIGINT; svc-b disagrees with a string.
    ingest_and_compact(&h, &[event("svc-a", &json!({"duration": 4200}))]).await;
    ingest_and_compact(&h, &[event("svc-b", &json!({"duration": "N/A"}))]).await;

    // /schema/conflicts shows the attributed evidence.
    let (status, body) = h.get(&h.server.analyst_token, "/schema/conflicts").await;
    assert_eq!(status, 200);
    let conflicts = body["conflicts"].as_array().expect("conflicts array");
    assert_bookkeeping_quiet(&timeouts_before, &bookkeeping_timeouts(&h.server.url).await);
    let row = conflicts
        .iter()
        .find(|c| c["field"] == "duration" && c["service"] == "svc-b")
        .expect("conflict row for svc-b's duration");
    assert_eq!(row["expected_type"], "BIGINT");
    assert_eq!(row["rows_nulled"], 1);
    assert_eq!(body["truncated"], false);

    // --last style windowing: a 1-hour window still covers it.
    let (_, body) = h
        .get(&h.server.analyst_token, "/schema/conflicts?since_secs=3600")
        .await;
    assert!(
        body["conflicts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["field"] == "duration"),
        "a fresh conflict is inside a 1h window"
    );

    // Field filter.
    let (_, body) = h
        .get(&h.server.analyst_token, "/schema/conflicts?field=duration")
        .await;
    assert!(
        body["conflicts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["field"] == "duration"),
        "field filter applies"
    );

    // /schema/field?name= lists both services and the conflict.
    let (status, body) = h
        .get(&h.server.analyst_token, "/schema/field?name=duration")
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["name"], "duration");
    assert_eq!(body["type"], "BIGINT");
    let services: Vec<&str> = body["services"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["service"].as_str().unwrap())
        .collect();
    assert!(services.contains(&"svc-a"), "services: {services:?}");
    assert!(services.contains(&"svc-b"), "services: {services:?}");
    assert!(
        !body["conflicts"].as_array().unwrap().is_empty(),
        "the detail carries the conflict evidence"
    );

    // The nulled original stays findable by whole-event search.
    let result = h
        .query
        .query_paginated("\"N/A\" last=1h", None, None)
        .await
        .expect("raw search");
    assert_eq!(
        result.result.row_count(),
        1,
        "the nulled value is findable via _raw"
    );
}

/// The fields listing carries aggregates, fill stats, and a clamped limit.
#[tokio::test(flavor = "multi_thread")]
async fn fields_listing_carries_aggregates_and_truncation() {
    let h = harness().await;
    ingest_and_compact(&h, &[event("nginx", &json!({"duration": 42}))]).await;

    let (status, body) = h.get(&h.server.analyst_token, "/schema/fields").await;
    assert_eq!(status, 200);
    let fields = body["fields"].as_array().expect("fields array");
    let duration = fields
        .iter()
        .find(|f| f["name"] == "duration")
        .expect("pinned field listed");
    assert_eq!(duration["type"], "BIGINT");
    assert_eq!(duration["service_count"], 1);
    assert_eq!(duration["row_count"], 1);
    assert_eq!(duration["conflict_count"], 0);
    assert!(duration["last_seen"].is_string());
    assert!(body["pinned_total"].as_u64().unwrap() >= 11);
    assert!(body["pin_capacity"].as_u64().unwrap() >= 10_000);
    assert_eq!(body["truncated"], false);

    // Envelope-first display order, like /schema.
    assert_eq!(fields[0]["name"], "_time");

    // Limit clamps and reports truncation.
    let (_, body) = h
        .get(&h.server.analyst_token, "/schema/fields?limit=2")
        .await;
    assert_eq!(body["fields"].as_array().unwrap().len(), 2);
    assert_eq!(body["truncated"], true);

    // ?service= scopes the listing.
    let (_, body) = h
        .get(&h.server.analyst_token, "/schema/fields?service=nope")
        .await;
    assert!(body["fields"].as_array().unwrap().is_empty());
}

/// The detail route ASCII-folds `?name=` (mirroring ingest's fold) and
/// 404s for a field the catalog has never pinned.
#[tokio::test(flavor = "multi_thread")]
async fn field_detail_folds_name_and_404s_unpinned() {
    let h = harness().await;
    ingest_and_compact(&h, &[event("nginx", &json!({"duration": 42}))]).await;

    let (status, body) = h
        .get(&h.server.analyst_token, "/schema/field?name=DURATION")
        .await;
    assert_eq!(
        status, 200,
        "case-variant lookups fold to the catalog spelling"
    );
    assert_eq!(body["name"], "duration");

    let (status, _) = h
        .get(&h.server.analyst_token, "/schema/field?name=never_pinned")
        .await;
    assert_eq!(status, 404);
}

/// The detail route never hands back an unbounded service history: the
/// page is capped (and the cap survives a hostile `?limit=`), and the
/// cursor walks the rest exactly once.
///
/// `field_services` rows are ever-observed, cost no pin slot, and their
/// service axis is client-chosen — so a common envelope field's history is
/// the one part of the catalog a sender can grow without limit.
#[tokio::test(flavor = "multi_thread")]
async fn field_detail_pages_a_large_service_history() {
    let h = harness().await;
    ingest_and_compact(&h, &[event("nginx", &json!({"duration": 42}))]).await;

    // 1100 services carrying `duration`, none of which spent a pin slot.
    let mut conn = sqlx::postgres::PgConnection::connect(&h.server.app_db_url)
        .await
        .expect("connect app db");
    sqlx::query(
        "INSERT INTO field_services (field, service, first_seen, last_seen, row_count)
         SELECT 'duration', 'svc-' || lpad(g::text, 5, '0'),
                now() - interval '1 day', now() - (g || ' seconds')::interval, 1
         FROM generate_series(1, 1100) g",
    )
    .execute(&mut conn)
    .await
    .expect("seed a large service history");

    // Default page: bounded, with a cursor for the rest.
    let (status, body) = h
        .get(&h.server.analyst_token, "/schema/field?name=duration")
        .await;
    assert_eq!(status, 200);
    assert_eq!(
        body["services"].as_array().unwrap().len(),
        100,
        "the default page is bounded"
    );
    assert!(
        body["services_cursor"].is_string(),
        "a truncated page advertises its cursor"
    );

    // A caller asking for everything still gets a bounded response.
    let (_, body) = h
        .get(
            &h.server.analyst_token,
            "/schema/field?name=duration&limit=100000",
        )
        .await;
    assert_eq!(
        body["services"].as_array().unwrap().len(),
        1000,
        "?limit= is clamped to the hard ceiling"
    );
    assert!(body["services_cursor"].is_string());

    // The cursor walk covers the whole history exactly once.
    let mut seen: Vec<String> = Vec::new();
    let mut path = "/schema/field?name=duration&limit=500".to_owned();
    for _ in 0..10 {
        let (status, body) = h.get(&h.server.analyst_token, &path).await;
        assert_eq!(status, 200);
        seen.extend(
            body["services"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s["service"].as_str().unwrap().to_owned()),
        );
        let Some(cursor) = body["services_cursor"].as_str() else {
            break;
        };
        // Percent-encode the two characters the cursor spelling carries
        // that a query string may not (`:` from RFC 3339, `|` separator).
        let escaped = cursor
            .replace('%', "%25")
            .replace(':', "%3A")
            .replace('|', "%7C");
        path = format!("/schema/field?name=duration&limit=500&after={escaped}");
    }
    assert_eq!(seen.len(), 1101, "1100 seeded services plus nginx");
    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), seen.len(), "no service delivered twice");
    assert_eq!(seen[0], "nginx", "most recent observation first");

    // A garbled cursor is a client error, not a silent restart at page one.
    let (status, _) = h
        .get(
            &h.server.analyst_token,
            "/schema/field?name=duration&after=nonsense",
        )
        .await;
    assert_eq!(status, 400);
}

/// Acceptance: the three `trawl schema` read commands render populated
/// output against a seeded catalog.
///
/// This is the whole vertical, joined: `run_*` → the typed client's URL and
/// query-param construction → the real routes → the real catalog → the
/// shared driver renderer. The CLI's own unit tests cover the converters
/// from synthetic structs; only this test proves the client actually asks
/// the server the question the flags describe.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // the sqlx macro used to hide the body in an inner fn
async fn read_commands_render_populated_output() {
    use trawl_cli::cli::{ConnectionParams, OutputFormat};

    let h = harness().await;
    let timeouts_before = bookkeeping_timeouts(&h.server.url).await;
    // svc-a pins `duration` and `latency` BIGINT; svc-b's strings disagree,
    // so the catalog carries two pins, two service observations each, and a
    // conflict per field — two, so `--field` has something to exclude.
    ingest_and_compact(
        &h,
        &[event("svc-a", &json!({"duration": 4200, "latency": 5}))],
    )
    .await;
    ingest_and_compact(
        &h,
        &[event(
            "svc-b",
            &json!({"duration": "N/A", "latency": "slow"}),
        )],
    )
    .await;

    let conn = ConnectionParams {
        url: h.server.url.clone(),
        token: h.server.analyst_token.clone(),
        insecure: true,
    };

    // `trawl schema fields --service svc-a --last 1h --limit 50`: every flag
    // travels as a query param, so a mis-built URL shows up as empty output.
    let mut out = Vec::new();
    trawl_cli::schema::run_fields(
        &mut out,
        Some(conn.clone()),
        None,
        Some("svc-a"),
        Some("1h"),
        Some(50),
        Some(OutputFormat::Table),
    )
    .await
    .expect("schema fields");
    let text = String::from_utf8(out).expect("utf8");
    assert_bookkeeping_quiet(&timeouts_before, &bookkeeping_timeouts(&h.server.url).await);
    assert!(text.contains("duration"), "{text}");
    assert!(text.contains("BIGINT"), "{text}");
    assert!(
        text.lines().any(|l| l.ends_with(" row(s)")) && !text.contains("\n0 row(s)"),
        "the scoped listing is populated: {text}"
    );

    // The `--service` scope really reached the server: svc-b's rows are the
    // only ones nulled, so svc-a's listing reports no conflict.
    let mut out = Vec::new();
    trawl_cli::schema::run_fields(
        &mut out,
        Some(conn.clone()),
        None,
        Some("nope"),
        None,
        None,
        Some(OutputFormat::Json),
    )
    .await
    .expect("schema fields --service nope");
    assert!(
        String::from_utf8(out).unwrap().trim().is_empty(),
        "an unknown service scopes the listing to nothing"
    );

    // `trawl schema field DURATION` — the name folds server-side, the header
    // block and both tables (services, conflicts) render.
    let mut out = Vec::new();
    trawl_cli::schema::run_field(
        &mut out,
        Some(conn.clone()),
        "DURATION",
        Some(10),
        None,
        Some(OutputFormat::Table),
    )
    .await
    .expect("schema field");
    let text = String::from_utf8(out).expect("utf8");
    assert!(text.contains("field:       duration"), "{text}");
    assert!(text.contains("type:        BIGINT"), "{text}");
    assert!(text.contains("svc-a") && text.contains("svc-b"), "{text}");
    assert!(text.contains("recent conflicts:"), "{text}");
    assert!(
        text.contains("VARCHAR"),
        "the observed type renders: {text}"
    );

    // `trawl schema conflicts --last 7d` as ndjson: both conflicts.
    let mut out = Vec::new();
    trawl_cli::schema::run_conflicts(
        &mut out,
        Some(conn.clone()),
        None,
        None,
        Some("7d"),
        Some(50),
        Some(OutputFormat::Json),
    )
    .await
    .expect("schema conflicts");
    let fields = ndjson_field_names(&out);
    assert!(fields.iter().any(|f| f == "duration"), "{fields:?}");
    assert!(fields.iter().any(|f| f == "latency"), "{fields:?}");

    // `--field duration` must reach the server as a query param: the same
    // call with the filter drops `latency` and keeps the evidence populated.
    let mut out = Vec::new();
    trawl_cli::schema::run_conflicts(
        &mut out,
        Some(conn),
        Some("duration"),
        None,
        Some("7d"),
        Some(50),
        Some(OutputFormat::Json),
    )
    .await
    .expect("schema conflicts --field duration");
    let text = String::from_utf8(out).expect("utf8");
    let fields = ndjson_field_names(text.as_bytes());
    assert_eq!(
        fields,
        vec!["duration"],
        "the --field filter reached the server"
    );
    let row: serde_json::Value =
        serde_json::from_str(text.lines().next().expect("at least one conflict row"))
            .expect("ndjson row");
    assert_eq!(row["service"], "svc-b");
    assert_eq!(row["expected_type"], "BIGINT");
    assert_eq!(row["rows_nulled"], 1);
}

/// The `field` column of every ndjson row the conflicts renderer emitted.
fn ndjson_field_names(out: &[u8]) -> Vec<String> {
    std::str::from_utf8(out)
        .expect("utf8")
        .lines()
        .map(|l| {
            serde_json::from_str::<serde_json::Value>(l).expect("ndjson row")["field"]
                .as_str()
                .expect("field column")
                .to_owned()
        })
        .collect()
}

/// All three read routes gate on `schema_read`: a key without it is denied
/// with 403, as is a key with no trawl grant at all; the reader key passes.
#[tokio::test(flavor = "multi_thread")]
async fn catalog_routes_require_schema_read() {
    let h = harness().await;
    ingest_and_compact(&h, &[event("nginx", &json!({"duration": 42}))]).await;

    for path in [
        "/schema/fields",
        "/schema/field?name=duration",
        "/schema/conflicts",
    ] {
        let (status, body) = h.get(&h.server.ingest_token, path).await;
        assert_eq!(status, 403, "{path} must deny a key without schema_read");
        assert_eq!(body["error"]["code"], "forbidden", "{path}: {body}");
        assert_eq!(
            body["error"]["message"], "insufficient permissions",
            "{path}: {body}"
        );

        let (status, _) = h.get(&h.server.coastwatch_only_token, path).await;
        assert_eq!(status, 403, "{path} must 403 a grantless key");

        let (status, _) = h.get(&h.server.reader_token, path).await;
        assert_eq!(status, 200, "{path} must 200 for schema_read holders");
    }
}

// -- degraded-badge acknowledgement (issue #111) ------------------------------

/// Pin `duration` BIGINT, then shelve three string values under it and
/// backdate the evidence so the span half of the gate is met.
///
/// Only the AGE is simulated: the conflicts are real, written by the real
/// conform. Waiting 24 hours is the one thing a test cannot do.
async fn degraded_duration(h: &Harness) {
    ingest_and_compact(h, &[event("svc-a", &json!({"duration": 4200}))]).await;
    for value in ["N/A", "pending", "N/A"] {
        ingest_and_compact(h, &[event("svc-b", &json!({"duration": value}))]).await;
    }
    let mut conn = sqlx::postgres::PgConnection::connect(&h.server.app_db_url)
        .await
        .expect("connect app db");
    sqlx::query(
        "UPDATE field_conflict_stats SET first_at = now() - interval '48 hours'
         WHERE field = 'duration'",
    )
    .execute(&mut conn)
    .await
    .expect("backdate the evidence");
}

/// The verdict on `/schema/fields` for one field, if it carries one.
async fn listed_verdict(h: &Harness, field: &str) -> Option<serde_json::Value> {
    let (status, body) = h.get(&h.server.analyst_token, "/schema/fields").await;
    assert_eq!(status, 200);
    let row = field_row(&body, field);
    row.get("verdict").cloned()
}

/// Whether a query binding `duration` is stamped with the notice, after the
/// refresh tick has republished the snapshot.
async fn query_is_stamped(h: &Harness) -> bool {
    trawl_server::schema_refresh::refresh_degraded_fields(&h.server.state).await;
    let (status, body) = h
        .post(
            &h.server.analyst_token,
            "/query",
            json!({"query": "last=1h | where duration > 1 | table host"}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    body.get("degraded_fields").is_some()
}

/// Whether `/schema/services` badges svc-b, the sender that conflicted.
async fn service_is_badged(h: &Harness) -> bool {
    trawl_server::schema_refresh::refresh_degraded_fields(&h.server.state).await;
    let (status, body) = h.get(&h.server.analyst_token, "/schema/services").await;
    assert_eq!(status, 200, "{body}");
    body["services"]
        .as_array()
        .expect("services array")
        .iter()
        .find(|s| s["name"] == "svc-b")
        .expect("svc-b listed")
        .get("degraded_fields")
        .is_some()
}

/// Acceptance: acknowledging a degraded pin takes the badge off all FOUR
/// read surfaces at once — the field listing, the field detail, the query
/// notice and the per-service badge — because every one of them asks the
/// same `is_degraded`. Then one more shelved batch re-raises all four,
/// while the acknowledgement itself stays visible on the detail: "we knew
/// on Tuesday, it is still happening" is the information the operator needs.
#[tokio::test(flavor = "multi_thread")]
async fn an_ack_suppresses_every_surface_until_new_evidence_arrives() {
    let h = harness().await;
    degraded_duration(&h).await;

    // Populate the per-service schema cache the badge is stamped from.
    let _refresh = trawl_server::schema_refresh::spawn_schema_refresh(h.server.state.clone());
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let (status, _) = h.get(&h.server.analyst_token, "/schema/services").await;
        assert!(
            std::time::Instant::now() < deadline,
            "schema refresh never populated the service cache"
        );
        if status == 200 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert!(listed_verdict(&h, "duration").await.is_some(), "badged");
    assert!(query_is_stamped(&h).await, "notice stands before the ack");
    assert!(service_is_badged(&h).await, "svc-b badged before the ack");

    let (status, ack) = h
        .post(
            &h.server.schema_admin_token,
            "/schema/field/ack?name=DURATION",
            json!({"note": "sender is being fixed, ticket OPS-12"}),
        )
        .await;
    assert_eq!(status, 200, "{ack}");
    assert_eq!(
        ack["evidence_through"], 3,
        "the ack covers the evidence that exists: {ack}"
    );
    assert!(
        !ack["acked_by"].as_str().expect("acked_by").is_empty(),
        "the acknowledging key's stable prefix is recorded: {ack}"
    );
    assert_eq!(ack["note"], "sender is being fixed, ticket OPS-12");

    assert!(
        listed_verdict(&h, "duration").await.is_none(),
        "the listing badge is suppressed (the name folds like every other \
         catalog lookup: ?name=DURATION acked `duration`)"
    );
    let (_, detail) = h
        .get(&h.server.analyst_token, "/schema/field?name=duration")
        .await;
    assert!(detail.get("verdict").is_none(), "detail verdict: {detail}");
    assert_eq!(
        detail["ack"]["evidence_through"], 3,
        "the ack is what the detail shows instead: {detail}"
    );
    assert!(
        !query_is_stamped(&h).await,
        "the query notice is suppressed"
    );
    assert!(
        !service_is_badged(&h).await,
        "the service badge is suppressed"
    );

    // One more shelved value: episode 4 is past the acknowledged 3.
    ingest_and_compact(&h, &[event("svc-b", &json!({"duration": "later"}))]).await;

    let verdict = listed_verdict(&h, "duration")
        .await
        .expect("new evidence re-raises the listing badge");
    assert_eq!(verdict["episodes"], 4, "{verdict}");
    let (_, detail) = h
        .get(&h.server.analyst_token, "/schema/field?name=duration")
        .await;
    assert!(
        detail.get("verdict").is_some(),
        "detail re-raised: {detail}"
    );
    assert_eq!(
        detail["ack"]["evidence_through"], 3,
        "the stale ack stays beside the re-raised verdict: {detail}"
    );
    assert!(query_is_stamped(&h).await, "the notice is back");
    assert!(service_is_badged(&h).await, "the service badge is back");
}

/// A field nobody could badge cannot be acknowledged: the 409 names the
/// threshold, and an unknown field is a 404. Neither writes anything.
#[tokio::test(flavor = "multi_thread")]
async fn acking_a_field_with_no_verdict_is_refused() {
    let h = harness().await;
    // Real conflicts, all of them from the last few seconds: volume without
    // span is exactly what the badge is designed not to fire on.
    ingest_and_compact(&h, &[event("svc-a", &json!({"duration": 4200}))]).await;
    for _ in 0..4 {
        ingest_and_compact(&h, &[event("svc-b", &json!({"duration": "N/A"}))]).await;
    }

    let (status, body) = h
        .post(
            &h.server.schema_admin_token,
            "/schema/field/ack?name=duration",
            json!({}),
        )
        .await;
    assert_eq!(status, 409, "{body}");
    let message = body["error"]["message"].as_str().expect("error message");
    assert!(message.contains("24"), "names the span: {message}");
    assert!(message.contains("100"), "names the row floor: {message}");
    assert!(message.contains('3'), "names the episode floor: {message}");

    let (status, body) = h
        .post(
            &h.server.schema_admin_token,
            "/schema/field/ack?name=never_pinned_field",
            json!({}),
        )
        .await;
    assert_eq!(status, 404, "{body}");

    let (_, detail) = h
        .get(&h.server.analyst_token, "/schema/field?name=duration")
        .await;
    assert!(
        detail.get("ack").is_none(),
        "a refused ack writes nothing: {detail}"
    );
}

/// Withdrawing an ack re-raises the badge and is idempotent: the second
/// DELETE is the same 204, because "not acknowledged" is the state the
/// caller asked for either way. Only an unknown field refuses.
#[tokio::test(flavor = "multi_thread")]
async fn withdrawing_an_ack_re_raises_the_badge_and_repeats_cleanly() {
    let h = harness().await;
    degraded_duration(&h).await;
    let (status, _) = h
        .post(
            &h.server.schema_admin_token,
            "/schema/field/ack?name=duration",
            json!({}),
        )
        .await;
    assert_eq!(status, 200);
    assert!(listed_verdict(&h, "duration").await.is_none());

    let (status, body) = h
        .delete(
            &h.server.schema_admin_token,
            "/schema/field/ack?name=duration",
        )
        .await;
    assert_eq!(status, 204, "{body}");
    assert!(body.is_empty(), "204 carries no body: {body:?}");
    assert!(
        listed_verdict(&h, "duration").await.is_some(),
        "the badge is back the moment the ack is withdrawn"
    );
    let (_, detail) = h
        .get(&h.server.analyst_token, "/schema/field?name=duration")
        .await;
    assert!(detail.get("ack").is_none(), "the ack is gone: {detail}");

    let (status, _) = h
        .delete(
            &h.server.schema_admin_token,
            "/schema/field/ack?name=duration",
        )
        .await;
    assert_eq!(status, 204, "withdrawing nothing is still 204");

    let (status, _) = h
        .delete(
            &h.server.schema_admin_token,
            "/schema/field/ack?name=never_pinned_field",
        )
        .await;
    assert_eq!(status, 404, "an unknown field is the one refusal");
}

/// The note is capped at 1024 bytes, and the refusal happens before the
/// store: a 1025-byte note is a 400 naming the limit, not a constraint
/// violation. The boundary itself is accepted.
#[tokio::test(flavor = "multi_thread")]
async fn an_oversized_ack_note_is_refused_before_the_store() {
    let h = harness().await;
    degraded_duration(&h).await;

    let (status, body) = h
        .post(
            &h.server.schema_admin_token,
            "/schema/field/ack?name=duration",
            json!({"note": "x".repeat(1025)}),
        )
        .await;
    assert_eq!(status, 400, "{body}");
    let message = body["error"]["message"].as_str().expect("error message");
    assert!(message.contains("1025"), "{message}");
    assert!(message.contains("1024"), "{message}");
    assert!(
        listed_verdict(&h, "duration").await.is_some(),
        "the refused ack suppressed nothing"
    );

    let (status, ack) = h
        .post(
            &h.server.schema_admin_token,
            "/schema/field/ack?name=duration",
            json!({"note": "x".repeat(1024)}),
        )
        .await;
    assert_eq!(status, 200, "the boundary is inclusive: {ack}");
    assert_eq!(ack["note"].as_str().expect("note").len(), 1024);
}

/// Both ack verbs ride `schema_write`: a `schema_read` key may read the
/// badge and not answer it. Both an authenticated key without the permission
/// and a key with no trawl grant at all receive 403.
#[tokio::test(flavor = "multi_thread")]
async fn the_ack_routes_require_schema_write() {
    let h = harness().await;
    degraded_duration(&h).await;

    for token in [&h.server.analyst_token, &h.server.reader_token] {
        let (status, body) = h
            .post(token, "/schema/field/ack?name=duration", json!({}))
            .await;
        assert_eq!(status, 403, "a schema_read key cannot acknowledge");
        assert_eq!(body["error"]["code"], "forbidden", "{body}");
        assert_eq!(body["error"]["message"], "insufficient permissions");
        let (status, body) = h.delete(token, "/schema/field/ack?name=duration").await;
        assert_eq!(status, 403, "nor withdraw");
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["error"]["code"], "forbidden", "{body}");
        assert_eq!(body["error"]["message"], "insufficient permissions");
    }

    let (status, _) = h
        .post(
            &h.server.coastwatch_only_token,
            "/schema/field/ack?name=duration",
            json!({}),
        )
        .await;
    assert_eq!(
        status, 403,
        "a grantless key is refused by the policy layer"
    );

    assert!(
        listed_verdict(&h, "duration").await.is_some(),
        "none of the refusals acknowledged anything"
    );
}

/// The audit trail: who acknowledged what, whether the row was created or
/// advanced, and which of the two things clears one. The operator's note is
/// never copied into an event — only whether they wrote one.
#[tokio::test(flavor = "multi_thread")]
// One process, one global subscriber, so the whole lifecycle (ack, advance,
// operator withdrawal, repin clear) has to be asserted from one body.
#[allow(clippy::too_many_lines)]
async fn the_ack_audit_records_the_actor_and_never_the_note() {
    use common::audit_capture::Capture;
    use tracing_subscriber::prelude::*;

    const NOTE: &str = "operator prose that must never reach a log line";

    let capture = Capture::default();
    let subscriber = tracing_subscriber::registry().with(
        capture
            .clone()
            .with_filter(tracing_subscriber::EnvFilter::new("trawl_server=info")),
    );
    // Global: the repin job runs detached on other workers.
    tracing::subscriber::set_global_default(subscriber).expect("no prior global subscriber");

    let h = harness().await;
    degraded_duration(&h).await;

    let (status, _) = h
        .post(
            &h.server.schema_admin_token,
            "/schema/field/ack?name=duration",
            json!({"note": NOTE}),
        )
        .await;
    assert_eq!(status, 200);

    // New evidence, then a second ack: the row is advanced, not created.
    ingest_and_compact(&h, &[event("svc-b", &json!({"duration": "later"}))]).await;
    let (status, _) = h
        .post(
            &h.server.schema_admin_token,
            "/schema/field/ack?name=duration",
            json!({}),
        )
        .await;
    assert_eq!(status, 200);

    let acked = capture.of_type("field_degraded_acked", "field", "duration");
    assert_eq!(acked.len(), 2, "one record per accepted ack: {acked:?}");
    assert!(acked[0].field("field").contains("duration"));
    assert_eq!(acked[0].field("created"), "true", "the row was inserted");
    assert_eq!(
        acked[0].field("advanced"),
        "true",
        "an insert is this call's own high-water"
    );
    assert_eq!(acked[0].field("evidence_through"), "3");
    assert_eq!(acked[0].field("note_present"), "true");
    assert!(
        !acked[0].field("actor_prefix").is_empty(),
        "the stable prefix identifies the key: {:?}",
        acked[0]
    );
    assert!(acked[0].field("actor").contains("schema-admin-key"));
    assert_eq!(acked[1].field("created"), "false", "advanced, not created");
    assert_eq!(
        acked[1].field("advanced"),
        "true",
        "it acknowledged the episode the first ack did not cover"
    );
    assert_eq!(acked[1].field("evidence_through"), "4");
    assert_eq!(acked[1].field("note_present"), "false");

    // The operator withdraws it: one clear, reason `operator`. The second
    // DELETE removes nothing, so it records nothing.
    let (status, _) = h
        .delete(
            &h.server.schema_admin_token,
            "/schema/field/ack?name=duration",
        )
        .await;
    assert_eq!(status, 204);
    let (status, _) = h
        .delete(
            &h.server.schema_admin_token,
            "/schema/field/ack?name=duration",
        )
        .await;
    assert_eq!(status, 204);

    // Re-ack, then repin the field: the pin the ack was about is gone, so
    // the cutover takes the ack with the evidence and says so.
    let (status, _) = h
        .post(
            &h.server.schema_admin_token,
            "/schema/field/ack?name=duration",
            json!({}),
        )
        .await;
    assert_eq!(status, 200);
    let (status, started) = h
        .post(
            &h.server.schema_admin_token,
            "/schema/repin",
            json!({"field": "duration", "to": "VARCHAR", "force": true}),
        )
        .await;
    assert_eq!(status, 202, "the repin runs detached: {started}");
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        let (_, body) = h.get(&h.server.analyst_token, "/schema/repin/status").await;
        let job_status = body["job"]["status"].as_str().unwrap_or("").to_owned();
        if job_status == "succeeded" {
            break;
        }
        assert!(
            !["failed", "refused_needs_force", "blocked"].contains(&job_status.as_str()),
            "the repin must reach the cutover: {body}"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "the repin never finished: {body}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let cleared = capture.of_type("field_degraded_ack_cleared", "field", "duration");
    let reasons: Vec<String> = cleared.iter().map(|e| e.field("reason")).collect();
    assert_eq!(
        cleared.len(),
        2,
        "one withdrawal and one repin; the no-op DELETE records nothing: \
         {cleared:?}"
    );
    assert!(reasons[0].contains("operator"), "{reasons:?}");
    assert!(reasons[1].contains("repin"), "{reasons:?}");
    assert!(
        !cleared[1].field("job_id").is_empty(),
        "the repin clear names its job: {:?}",
        cleared[1]
    );

    for record in capture.events() {
        for (name, value) in &record.fields {
            assert!(
                !value.contains(NOTE),
                "the note reached a log line as {name}: {record:?}"
            );
        }
    }
}
