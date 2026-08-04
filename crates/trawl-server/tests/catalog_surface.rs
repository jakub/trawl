// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! End-to-end tests for the catalog schema surface (ADR-0009 slice 3, #51):
//! `/api/v1/schema` served from the catalog with `?service=`/`?all=`
//! windowing, and the three read routes `/api/v1/schema/fields`,
//! `/api/v1/schema/field?name=`, `/api/v1/schema/conflicts`.
//!
//! Modeled on `field_catalog.rs`: ingest through the real handler, compact
//! through the real compaction path (with the catalog context the daemon
//! wires), read through the real API.

mod common;

use std::time::Duration;

use common::{TestServer, setup_in_dir_with_data};
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

async fn harness(pool: sqlx::PgPool) -> Harness {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let wal_dir = root.join("wal");
    let data_dir = root.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let data_glob = format!("{}/**/*.parquet", data_dir.display());

    let server = setup_in_dir_with_data(pool, &root, data_glob, RateLimitConfig::default()).await;
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
#[sqlx::test(migrations = false)]
async fn ingested_field_lands_in_schema_with_pinned_type(pool: sqlx::PgPool) {
    let h = harness(pool).await;
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
#[sqlx::test(migrations = false)]
async fn schema_service_param_scopes_fields(pool: sqlx::PgPool) {
    let h = harness(pool).await;
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

/// Regression: the UNSCOPED column set is TTL-cached (its aggregate spans
/// every service, and the service axis is client-chosen and unbounded)
/// while `?service=` is served fresh. The two must not share a slot — a
/// scoped request must neither be answered from the unscoped cache nor
/// poison it for the next unscoped caller.
#[sqlx::test(migrations = false)]
async fn schema_service_scope_bypasses_the_unscoped_cache(pool: sqlx::PgPool) {
    let h = harness(pool).await;
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
#[sqlx::test(migrations = false)]
async fn aged_out_field_windowed_away_unless_all(pool: sqlx::PgPool) {
    let h = harness(pool).await;
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

/// Acceptance: a type conflict surfaces on `/schema/conflicts`, the field
/// detail lists both services, and the nulled original stays findable via
/// `_raw` search.
#[sqlx::test(migrations = false)]
async fn conflict_evidence_surfaces_on_the_read_routes(pool: sqlx::PgPool) {
    let h = harness(pool).await;
    // svc-a pins duration BIGINT; svc-b disagrees with a string.
    ingest_and_compact(&h, &[event("svc-a", &json!({"duration": 4200}))]).await;
    ingest_and_compact(&h, &[event("svc-b", &json!({"duration": "N/A"}))]).await;

    // /schema/conflicts shows the attributed evidence.
    let (status, body) = h.get(&h.server.analyst_token, "/schema/conflicts").await;
    assert_eq!(status, 200);
    let conflicts = body["conflicts"].as_array().expect("conflicts array");
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
#[sqlx::test(migrations = false)]
async fn fields_listing_carries_aggregates_and_truncation(pool: sqlx::PgPool) {
    let h = harness(pool).await;
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
#[sqlx::test(migrations = false)]
async fn field_detail_folds_name_and_404s_unpinned(pool: sqlx::PgPool) {
    let h = harness(pool).await;
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
#[sqlx::test(migrations = false)]
async fn field_detail_pages_a_large_service_history(pool: sqlx::PgPool) {
    let h = harness(pool).await;
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
#[sqlx::test(migrations = false)]
async fn read_commands_render_populated_output(pool: sqlx::PgPool) {
    use trawl_cli::cli::{ConnectionParams, OutputFormat};

    let h = harness(pool).await;
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
    // block and BOTH tables (services, conflicts) render.
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
/// (401 insufficient-permissions per the handler convention; a key with no
/// trawl grant at all is the 403 case), the reader key passes.
#[sqlx::test(migrations = false)]
async fn catalog_routes_require_schema_read(pool: sqlx::PgPool) {
    let h = harness(pool).await;
    ingest_and_compact(&h, &[event("nginx", &json!({"duration": 42}))]).await;

    for path in [
        "/schema/fields",
        "/schema/field?name=duration",
        "/schema/conflicts",
    ] {
        let (status, _) = h.get(&h.server.ingest_token, path).await;
        assert_eq!(status, 401, "{path} must deny a key without schema_read");

        let (status, _) = h.get(&h.server.coastwatch_only_token, path).await;
        assert_eq!(status, 403, "{path} must 403 a grantless key");

        let (status, _) = h.get(&h.server.reader_token, path).await;
        assert_eq!(status, 200, "{path} must 200 for schema_read holders");
    }
}
