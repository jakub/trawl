// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! End-to-end tests for the field catalog (ADR-0009): write-time type
//! conformance closes two silent data-loss paths, the cold/cold
//! whole-history drop and the nested-key whole-batch column drop.
//!
//! Shaped like `timestamp_repair.rs`: ingest through the real handler,
//! compact through the real compaction path (with the catalog context the
//! daemon wires), query through the real API.

mod common;

use std::time::Duration;

use common::{TestServer, setup_in_dir_with_data};
use serde_json::json;

/// A current RFC 3339 timestamp so `last=1h` queries cover the events.
fn now_ts() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}
use trawl_api::value::Value;
use trawl_client::HttpClient;
use trawl_server::catalog::CatalogContext;
use trawl_server::config::RateLimitConfig;

/// The catalog context exactly as trawld's main wires it: the server's
/// catalog store plus the server's shared in-process pin cache.
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

fn column_index(result: &trawl_api::value::QueryResult, column: &str) -> Option<usize> {
    result.columns.iter().position(|c| c.name == column)
}

fn column_values(result: &trawl_api::value::QueryResult, column: &str) -> Vec<Value> {
    let idx = column_index(result, column).unwrap_or_else(|| panic!("column {column} present"));
    result.rows.iter().map(|r| r[idx].clone()).collect()
}

struct Harness {
    server: TestServer,
    wal_dir: std::path::PathBuf,
    data_dir: std::path::PathBuf,
    ingest: HttpClient,
    query: HttpClient,
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
    Harness {
        server,
        wal_dir,
        data_dir,
        ingest,
        query,
    }
}

/// Acceptance: a cold/cold type conflict returns ALL history, not
/// hot-only; the conflict is recorded and attributed; the counter is on
/// /metrics.
#[tokio::test(flavor = "multi_thread")]
async fn cold_cold_conflict_returns_full_history_with_attribution() {
    let h = harness().await;

    // Service A: duration is an integer. Compacted first, so it pins BIGINT.
    let a_events: Vec<serde_json::Value> = (0..3)
        .map(|i| {
            json!({
                "service": "svc-a", "env": "prod", "host": "web01",
                "timestamp": now_ts(),
                "message": format!("a{i}"), "duration": 4200 + i,
            })
        })
        .collect();
    let resp = h.ingest.ingest(&a_events).await.expect("ingest A");
    assert_eq!(resp.accepted, 3);
    compact_tick(&h.server, &h.wal_dir, &h.data_dir).await;

    // Service B: duration is a string — the batch that disagrees.
    let b_events: Vec<serde_json::Value> = (0..2)
        .map(|i| {
            json!({
                "service": "svc-b", "env": "prod", "host": "web02",
                "timestamp": now_ts(),
                "message": format!("b{i}"), "duration": "1.5s",
            })
        })
        .collect();
    let resp = h.ingest.ingest(&b_events).await.expect("ingest B");
    assert_eq!(resp.accepted, 2);
    compact_tick(&h.server, &h.wal_dir, &h.data_dir).await;

    // Both services' history is on disk; the hot buffer is drained. A
    // cross-service query over the conflicted field must return service
    // A's parquet rows, never degrade to hot-only: that would be zero rows
    // under an HTTP 200.
    let result = h
        .query
        .query_paginated("last=1h | where duration > 1000", None, None)
        .await
        .expect("cross-service query must succeed");
    assert_eq!(
        result.result.row_count(),
        3,
        "service A's full parquet history must be returned"
    );

    // And the full corpus is visible: all 5 rows across both services.
    let all = h
        .query
        .query_paginated("last=1h", None, None)
        .await
        .expect("query all");
    assert_eq!(all.result.row_count(), 5, "no history is dropped");

    // The conflict is recorded and attributed: field duration, service B,
    // expected BIGINT, observed VARCHAR, non-zero rows_nulled.
    let conflicts = h
        .server
        .state
        .storage
        .catalog
        .conflicts_for_field("duration")
        .await
        .expect("read field_conflicts");
    let row = conflicts
        .iter()
        .find(|c| c.service == "svc-b")
        .expect("a conflict row names service B");
    assert_eq!(row.expected_type, "BIGINT");
    assert_eq!(row.observed_type, "VARCHAR");
    assert!(row.rows_nulled > 0, "the nulled rows are counted");

    // The nulled originals stay recoverable from _raw.
    let b_rows = h
        .query
        .query_paginated("service=svc-b last=1h", None, None)
        .await
        .expect("query B");
    assert_eq!(b_rows.result.row_count(), 2);
    let raws = column_values(&b_rows.result, "_raw");
    assert!(
        raws.iter()
            .all(|v| matches!(v, Value::String(s) if s.contains("1.5s"))),
        "the nulled value must be findable in _raw: {raws:?}"
    );

    // The conflict increments a metric on /metrics.
    let metrics_client = common::harness_client_builder().build().unwrap();
    let body = metrics_client
        .get(format!("{}/metrics", h.server.url))
        .send()
        .await
        .expect("GET /metrics")
        .text()
        .await
        .unwrap();
    assert!(
        body.contains("trawl_catalog_conflicts_total"),
        "conflict counter must be exposed: {body}"
    );
    assert!(
        body.contains("trawl_catalog_rows_nulled_total"),
        "rows-nulled counter must be exposed"
    );
}

/// Acceptance: a nested-object field no longer drops its batch-mates'
/// columns, and the nested value stays reachable via
/// `json_extract_string`.
#[tokio::test(flavor = "multi_thread")]
async fn nested_object_keeps_batchmates_and_stays_reachable() {
    let h = harness().await;

    let events = vec![
        json!({
            "service": "svc-k8s", "env": "prod", "host": "node1",
            "timestamp": now_ts(), "message": "nested",
            "k8s": {"pod": "x", "ns": "default"},
            "status": 200, "uri": "/healthz",
        }),
        json!({
            "service": "svc-k8s", "env": "prod", "host": "node1",
            "timestamp": now_ts(), "message": "flat",
            "status": 500, "uri": "/api", "src_ip": "10.0.0.9",
        }),
    ];
    let resp = h.ingest.ingest(&events).await.expect("ingest");
    assert_eq!(resp.accepted, 2);
    compact_tick(&h.server, &h.wal_dir, &h.data_dir).await;

    // Every custom column landed in parquet: a nested value costs its own
    // column at most, never its batch-mates'.
    let result = h
        .query
        .query_paginated("service=svc-k8s last=1h", None, None)
        .await
        .expect("query");
    assert_eq!(result.result.row_count(), 2);
    for col in ["status", "uri", "src_ip", "k8s"] {
        assert!(
            column_index(&result.result, col).is_some(),
            "custom column {col} must survive compaction; got {:?}",
            result.result.columns
        );
    }

    // The nested value is reachable as JSON text.
    let reach = h
        .query
        .query_paginated(
            "service=svc-k8s last=1h | eval pod = json_extract_string(k8s, \"$.pod\") \
             | where isnotnull(pod) | fields pod",
            None,
            None,
        )
        .await
        .expect("json_extract_string query");
    assert_eq!(reach.result.row_count(), 1);
    assert_eq!(
        column_values(&reach.result, "pod"),
        vec![Value::String("x".into())],
        "json_extract_string must reach the nested value"
    );
}

/// Acceptance: an all-null first batch defers the pin; a later typed batch
/// pins it, and a query across both reads cleanly.
#[tokio::test(flavor = "multi_thread")]
async fn all_null_first_batch_defers_then_pins() {
    let h = harness().await;

    let first = json!({
        "service": "svc-defer", "env": "prod", "host": "h",
        "timestamp": now_ts(), "message": "null batch",
        "maybe": null,
    });
    h.ingest.ingest(&[first]).await.expect("ingest 1");
    compact_tick(&h.server, &h.wal_dir, &h.data_dir).await;
    assert!(
        h.server.state.query.field_catalog.get("maybe").is_none(),
        "an all-null batch must not pin"
    );

    let second = json!({
        "service": "svc-defer", "env": "prod", "host": "h",
        "timestamp": now_ts(), "message": "typed batch",
        "maybe": 7,
    });
    h.ingest.ingest(&[second]).await.expect("ingest 2");
    compact_tick(&h.server, &h.wal_dir, &h.data_dir).await;
    assert_eq!(
        h.server.state.query.field_catalog.get("maybe"),
        Some(trawl_core::schema::CanonicalType::BigInt),
        "the first typed batch pins"
    );

    let result = h
        .query
        .query_paginated(
            "service=svc-defer last=1h | fields message, maybe",
            None,
            None,
        )
        .await
        .expect("cross-batch query unions cleanly");
    assert_eq!(result.result.row_count(), 2);
    let values = column_values(&result.result, "maybe");
    assert!(values.contains(&Value::Integer(7)));
    assert!(values.contains(&Value::Null), "deferred rows read as NULL");
}

/// Acceptance: a pin write failure means no parquet is written and the WAL
/// is retained for retry — never an unconformant file.
#[tokio::test(flavor = "multi_thread")]
async fn pin_write_failure_retains_wal_and_writes_nothing() {
    let h = harness().await;

    let event = json!({
        "service": "svc-pinfail", "env": "prod", "host": "h",
        "timestamp": now_ts(), "message": "m",
        "brand_new_field": 1,
    });
    h.ingest.ingest(&[event]).await.expect("ingest");

    // A catalog whose store points at an unreachable postgres: pins cannot
    // become durable, so the batch must not be written.
    // Deliberately unreachable: this pool exists to simulate a dead
    // catalog store, not to reach a fixture database (ADR-0021 ruling 3).
    #[allow(clippy::disallowed_methods)]
    let dead_pool = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(200))
        .connect_lazy("postgres://nobody@127.0.0.1:1/nowhere")
        .expect("lazy pool");
    let dead_ctx = CatalogContext {
        store: trawl_server::store::CatalogStore::new(dead_pool),
        cache: std::sync::Arc::new(trawl_server::catalog::FieldCatalog::new()),
    };
    let hot_buffer = h.server.state.query.hot_buffer.as_ref().unwrap();
    // The tick itself completes (per-chunk failures are logged + retried).
    trawl_server::ingest::compaction::compact_once(
        &h.wal_dir,
        &h.data_dir,
        Duration::ZERO,
        false,
        Some(hot_buffer),
        500,
        "2GB",
        Some(&dead_ctx),
    )
    .await
    .expect("tick completes; the chunk fails and retries");

    let parquet: Vec<_> = walkdir_parquet(&h.data_dir);
    assert!(
        parquet.is_empty(),
        "no parquet may be written before pins are durable: {parquet:?}"
    );
    let wal_left: Vec<_> = std::fs::read_dir(h.wal_dir.join("prod"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "ndjson"))
        .collect();
    assert!(
        !wal_left.is_empty(),
        "the WAL batch must be retained for retry"
    );
    assert!(
        hot_buffer.event_count() > 0,
        "the hot buffer keeps serving the batch meanwhile"
    );

    // The real catalog drains it on the next tick.
    compact_tick(&h.server, &h.wal_dir, &h.data_dir).await;
    assert!(!walkdir_parquet(&h.data_dir).is_empty());
}

/// Acceptance: a HOT-side conflict — an uncompacted event disagreeing with
/// an existing pin — is nulled on the hot branch of the union while the
/// full cold history stays visible. The end-to-end proof of the
/// `HotSnapshot` pin plumbing: the outcome is never hot-only (the cold rows
/// ARE present) and never a loud error (the pin conformance resolves the
/// conflict in one execution).
#[tokio::test(flavor = "multi_thread")]
async fn hot_conflicting_event_is_nulled_and_cold_history_survives() {
    let h = harness().await;

    // Seed the pin: three integer durations, compacted → duration pins
    // BIGINT and the parquet history exists.
    let cold_events: Vec<serde_json::Value> = (0..3)
        .map(|i| {
            json!({
                "service": "svc-hot", "env": "prod", "host": "web01",
                "timestamp": now_ts(),
                "message": format!("cold{i}"), "duration": 4200 + i,
            })
        })
        .collect();
    let resp = h.ingest.ingest(&cold_events).await.expect("ingest cold");
    assert_eq!(resp.accepted, 3);
    compact_tick(&h.server, &h.wal_dir, &h.data_dir).await;
    assert_eq!(
        h.server.state.query.field_catalog.get("duration"),
        Some(trawl_core::schema::CanonicalType::BigInt),
        "the cold batch must pin duration BIGINT"
    );

    // The conflicting event arrives and stays UNCOMPACTED — hot only.
    let hot_event = json!({
        "service": "svc-hot", "env": "prod", "host": "web01",
        "timestamp": now_ts(),
        "message": "hot-conflict", "duration": "n/a",
    });
    let resp = h.ingest.ingest(&[hot_event]).await.expect("ingest hot");
    assert_eq!(resp.accepted, 1);
    let hot_buffer = h.server.state.query.hot_buffer.as_ref().unwrap();
    assert!(
        hot_buffer.event_count() > 0,
        "the conflicting event must still be in the hot buffer"
    );

    // The union succeeds in one execution: all four rows, the hot value
    // NULL, the cold values still integers. A hot-only fallback would show
    // 1 row.
    let result = h
        .query
        .query_paginated("service=svc-hot last=1h", None, None)
        .await
        .expect("hot+cold query with a pinned conflict must succeed");
    assert_eq!(
        result.result.row_count(),
        4,
        "cold history AND the hot event must both be visible — never hot-only"
    );
    let values = column_values(&result.result, "duration");
    let ints = values
        .iter()
        .filter(|v| matches!(v, Value::Integer(_)))
        .count();
    let nulls = values.iter().filter(|v| matches!(v, Value::Null)).count();
    assert_eq!(
        (ints, nulls),
        (3, 1),
        "cold values stay BIGINT, the conflicting hot value degrades to NULL: {values:?}"
    );

    // The nulled hot original stays recoverable from _raw.
    let hot_row = h
        .query
        .query_paginated("service=svc-hot last=1h message=hot-conflict", None, None)
        .await
        .expect("query hot row");
    let raws = column_values(&hot_row.result, "_raw");
    assert!(
        raws.iter()
            .all(|v| matches!(v, Value::String(s) if s.contains("n/a"))),
        "the nulled hot value must be findable in _raw: {raws:?}"
    );
}

/// End-to-end regression for the case-variant blockers: two services
/// shipping `Dur` and `dur` used to pin independently (case-sensitive
/// catalog keys), each file conformed to its own pin, and
/// `read_parquet(union_by_name)` folded them into one column — a hard
/// `Conversion` error on every spanning query; the in-loop mitigation then
/// degraded the pin to VARCHAR, permanently breaking numeric comparisons.
/// With ingest-time folding there is ONE spelling, one pin, one column,
/// and numeric predicates keep working across services.
#[tokio::test(flavor = "multi_thread")]
async fn case_variant_field_names_fold_to_one_column_across_services() {
    let h = harness().await;

    // svc-a ships `Dur` (uppercase), numeric. Compacted first: pins `dur`.
    let a_events: Vec<serde_json::Value> = (0..3)
        .map(|i| {
            json!({
                "service": "svc-a", "env": "prod", "host": "web01",
                "timestamp": now_ts(),
                "message": format!("a{i}"), "Dur": 4200 + i,
            })
        })
        .collect();
    assert_eq!(
        h.ingest.ingest(&a_events).await.expect("ingest A").accepted,
        3
    );
    compact_tick(&h.server, &h.wal_dir, &h.data_dir).await;

    // svc-b ships `dur` (lowercase), numeric too.
    let b_events: Vec<serde_json::Value> = (0..2)
        .map(|i| {
            json!({
                "service": "svc-b", "env": "prod", "host": "web02",
                "timestamp": now_ts(),
                "message": format!("b{i}"), "dur": 100 + i,
            })
        })
        .collect();
    assert_eq!(
        h.ingest.ingest(&b_events).await.expect("ingest B").accepted,
        2
    );
    compact_tick(&h.server, &h.wal_dir, &h.data_dir).await;

    // One pin, under the folded spelling.
    assert_eq!(
        h.server.state.query.field_catalog.get("dur"),
        Some(trawl_core::schema::CanonicalType::BigInt),
        "both spellings must resolve to one folded pin"
    );
    assert_eq!(
        h.server.state.query.field_catalog.get("Dur"),
        None,
        "no mixed-case pin may exist"
    );

    // A spanning query returns all rows: no Conversion error, no hot-only
    // degrade, one column in the result.
    let all = h
        .query
        .query_paginated("last=1h", None, None)
        .await
        .expect("the spanning query must not hit a union conflict");
    assert_eq!(all.result.row_count(), 5, "both services fully visible");
    assert!(
        column_index(&all.result, "dur").is_some(),
        "the folded column is the one column: {:?}",
        all.result.columns
    );
    assert!(
        column_index(&all.result, "Dur").is_none(),
        "the unfolded spelling must not be a column: {:?}",
        all.result.columns
    );

    // Numeric comparison binds across services, under the one pin.
    let big = h
        .query
        .query_paginated("last=1h | where dur > 1000", None, None)
        .await
        .expect("numeric comparison on the folded column must bind");
    assert_eq!(
        big.result.row_count(),
        3,
        "svc-a's values compare numerically"
    );

    // The fold is visible in _repairs for the folded sender.
    let a_rows = h
        .query
        .query_paginated("service=svc-a last=1h", None, None)
        .await
        .expect("query A");
    let repairs = column_values(&a_rows.result, "_repairs");
    assert!(
        repairs
            .iter()
            .all(|v| matches!(v, Value::String(s) if s.contains("field.name_case_folded"))),
        "folding is recorded per event: {repairs:?}"
    );
}

/// Acceptance: `field_services` is ever-observed — compaction advances
/// `last_seen`, and retention deleting a date directory leaves the rows in
/// place (documented semantics, not a leak).
#[tokio::test(flavor = "multi_thread")]
async fn field_services_is_ever_observed() {
    let h = harness().await;

    let ev = |i: u32| {
        json!({
            "service": "svc-obs", "env": "prod", "host": "h",
            "timestamp": now_ts(), "message": format!("m{i}"),
            "duration": i,
        })
    };
    h.ingest.ingest(&[ev(1)]).await.expect("ingest 1");
    compact_tick(&h.server, &h.wal_dir, &h.data_dir).await;
    let first = h
        .server
        .state
        .storage
        .catalog
        .field_services("duration", None, 1000)
        .await
        .unwrap()
        .0;
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].service, "svc-obs");

    tokio::time::sleep(Duration::from_millis(20)).await;
    h.ingest.ingest(&[ev(2)]).await.expect("ingest 2");
    compact_tick(&h.server, &h.wal_dir, &h.data_dir).await;
    let second = h
        .server
        .state
        .storage
        .catalog
        .field_services("duration", None, 1000)
        .await
        .unwrap()
        .0;
    assert!(
        second[0].last_seen > first[0].last_seen,
        "compaction advances last_seen"
    );
    assert_eq!(second[0].first_seen, first[0].first_seen);

    // Retention deleting the day's data leaves the observations in place.
    for entry in std::fs::read_dir(h.data_dir.join("prod"))
        .unwrap()
        .flatten()
    {
        if entry.path().is_dir() {
            std::fs::remove_dir_all(entry.path()).unwrap();
        }
    }
    let after = h
        .server
        .state
        .storage
        .catalog
        .field_services("duration", None, 1000)
        .await
        .unwrap()
        .0;
    assert_eq!(
        after.len(),
        1,
        "field_services rows are ever-observed; retention never reconciles them"
    );
}

fn walkdir_parquet(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                out.extend(walkdir_parquet(&p));
            } else if p.extension().is_some_and(|e| e == "parquet") {
                out.push(p);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// boot conformance pass (ADR-0009)
// ---------------------------------------------------------------------------

mod boot {
    use trawl_server::catalog::{FieldCatalog, conform};
    use trawl_server::store::CatalogStore;

    /// Plant a parquet file at `data_dir/rel` from a SELECT.
    fn plant(data_dir: &std::path::Path, rel: &str, select: &str) -> std::path::PathBuf {
        let path = data_dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let conn = duckdb::Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "COPY ({select}) TO '{}' (FORMAT PARQUET)",
            path.display()
        ))
        .unwrap();
        path
    }

    fn column_type(path: &std::path::Path, column: &str) -> String {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let mut stmt = conn
            .prepare(&format!(
                "DESCRIBE SELECT \"{column}\" FROM read_parquet('{}')",
                path.display()
            ))
            .unwrap();
        stmt.query_row([], |row| row.get(1)).unwrap()
    }

    /// A pre-catalog corpus where two files disagree on `duration`:
    /// majority (3 rows) BIGINT, minority (1 row) VARCHAR.
    fn plant_disagreeing_corpus(
        data_dir: &std::path::Path,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let majority = plant(
            data_dir,
            "prod/2026-08-01/10/svc-a.parquet",
            "SELECT TIMESTAMP '2026-08-01 10:00:00' AS \"_time\", 'svc-a' AS service, \
             x::BIGINT AS duration FROM (VALUES (410), (420), (430)) t(x)",
        );
        let minority = plant(
            data_dir,
            "prod/2026-08-01/11/svc-b.parquet",
            "SELECT TIMESTAMP '2026-08-01 11:00:00' AS \"_time\", 'svc-b' AS service, \
             '1.5s' AS duration",
        );
        (majority, minority)
    }

    #[sqlx::test]
    async fn boot_pass_pins_by_most_rows_and_rewrites_minority(pool: sqlx::PgPool) {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let (majority, minority) = plant_disagreeing_corpus(&data_dir);

        let store = CatalogStore::new(pool.clone());
        let cache = FieldCatalog::new();
        let summary = conform::ensure_conformance(&store, &cache, &data_dir, "2GB")
            .await
            .expect("boot pass runs");
        assert!(summary.ran, "first boot must run the pass");
        assert_eq!(
            summary.rewritten, 1,
            "exactly the minority file is rewritten"
        );

        // Pin: most-rows-wins → BIGINT.
        assert_eq!(
            cache.get("duration"),
            Some(trawl_core::schema::CanonicalType::BigInt),
            "the cache is hydrated with the most-rows-wins pin"
        );

        // The minority file now conforms; the majority was left alone.
        assert_eq!(column_type(&minority, "duration"), "BIGINT");
        assert_eq!(column_type(&majority, "duration"), "BIGINT");

        // The conflict is recorded against the minority file's service.
        let conflicts = store.conflicts_for_field("duration").await.unwrap();
        assert!(
            conflicts
                .iter()
                .any(|c| c.service == "svc-b" && c.expected_type == "BIGINT"),
            "boot rewrite must record the conflict: {conflicts:?}"
        );

        // Full corpus reads through one union, all rows present.
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let count: i64 = conn
            .query_row(
                &format!(
                    "SELECT count(*)::BIGINT FROM read_parquet('{}/**/*.parquet', \
                     union_by_name=true)",
                    data_dir.display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 4, "no rows lost by the rewrite");

        // The marker mirrors the catalog identity.
        let marker = std::fs::read_to_string(data_dir.join("CATALOG")).unwrap();
        assert_eq!(marker.trim(), store.catalog_id().await.unwrap());
    }

    /// A standing file whose `_time` is VARCHAR text no parser can read must
    /// not be rewritten to a NULL partition key: a NULL sorts first and falls
    /// outside every `last=Xh` filter, so the row would survive the conform
    /// and become permanently unqueryable by time (ADR-0008). The rewrite
    /// substitutes the file's own partition instant, exactly as compaction
    /// substitutes the WAL filename's.
    #[sqlx::test]
    async fn unparseable_standing_time_conforms_to_the_partition_instant(pool: sqlx::PgPool) {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        // Majority (3 rows) pins `_time` TIMESTAMP.
        plant(
            &data_dir,
            "prod/2026-08-01/10/svc-a.parquet",
            "SELECT TIMESTAMP '2026-08-01 10:00:00' + INTERVAL (x) MINUTE AS \"_time\", \
             'svc-a' AS service FROM (VALUES (1), (2), (3)) t(x)",
        );
        // Minority: VARCHAR `_time` carrying text no TRY_CAST can read.
        let minority = plant(
            &data_dir,
            "prod/2026-08-01/11/svc-b.parquet",
            "SELECT 'yesterday-ish' AS \"_time\", 'svc-b' AS service",
        );

        let store = CatalogStore::new(pool.clone());
        let cache = FieldCatalog::new();
        let summary = conform::ensure_conformance(&store, &cache, &data_dir, "2GB")
            .await
            .expect("boot pass runs");
        assert_eq!(summary.rewritten, 1, "only the minority file disagrees");
        assert_eq!(column_type(&minority, "_time"), "TIMESTAMP");

        let conn = duckdb::Connection::open_in_memory().unwrap();
        let (nulls, substituted): (i64, String) = conn
            .query_row(
                &format!(
                    "SELECT count(*) FILTER (WHERE \"_time\" IS NULL)::BIGINT, \
                     max(\"_time\")::VARCHAR FROM read_parquet('{}/**/*.parquet', \
                     union_by_name=true)",
                    data_dir.display()
                ),
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(nulls, 0, "the conform must never manufacture a NULL _time");
        assert_eq!(
            substituted, "2026-08-01 11:00:00",
            "the unreadable row takes its own hour directory's instant"
        );

        // Substituting a value is still losing one: the evidence is recorded.
        let conflicts = store.conflicts_for_field("_time").await.unwrap();
        assert!(
            conflicts
                .iter()
                .any(|c| c.service == "svc-b" && c.rows_nulled == 1),
            "the unreadable timestamp is conflict evidence: {conflicts:?}"
        );
    }

    /// The vote weighs rows that carry the field, not the file's row count:
    /// a mostly-NULL `duration` in a big file holds no values to describe, so
    /// it must not win the pin and `TRY_CAST` the small file's real values away.
    #[sqlx::test]
    async fn all_null_column_in_a_bigger_file_does_not_win_the_pin(pool: sqlx::PgPool) {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        // 6 rows, `duration` typed BIGINT but empty in every one of them.
        plant(
            &data_dir,
            "prod/2026-08-01/10/svc-a.parquet",
            "SELECT TIMESTAMP '2026-08-01 10:00:00' AS \"_time\", 'svc-a' AS service, \
             NULL::BIGINT AS duration FROM range(6)",
        );
        // 2 rows, `duration` VARCHAR and populated.
        let populated = plant(
            &data_dir,
            "prod/2026-08-01/11/svc-b.parquet",
            "SELECT TIMESTAMP '2026-08-01 11:00:00' AS \"_time\", 'svc-b' AS service, \
             x AS duration FROM (VALUES ('1.5s'), ('2.5s')) t(x)",
        );

        let store = CatalogStore::new(pool.clone());
        let cache = FieldCatalog::new();
        conform::ensure_conformance(&store, &cache, &data_dir, "2GB")
            .await
            .expect("boot pass runs");

        assert_eq!(
            cache.get("duration"),
            Some(trawl_core::schema::CanonicalType::Varchar),
            "the only column carrying values wins the pin"
        );
        assert_eq!(column_type(&populated, "duration"), "VARCHAR");

        // Nothing was nulled: both real values survive.
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let kept: i64 = conn
            .query_row(
                &format!(
                    "SELECT count(duration)::BIGINT FROM \
                     read_parquet('{}/**/*.parquet', union_by_name=true)",
                    data_dir.display()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(kept, 2, "no populated value was TRY_CAST away");
    }

    fn column_names(path: &std::path::Path) -> Vec<String> {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let mut stmt = conn
            .prepare(&format!(
                "DESCRIBE SELECT * FROM read_parquet('{}')",
                path.display()
            ))
            .unwrap();
        stmt.query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    /// Parquet trawl did not write can carry mixed-case column names. The
    /// boot pass folds when seeding, so `Dur` and `dur` form one group with
    /// most-rows-wins inside it, and the rewrite renames columns to the
    /// folded form: one spelling, one pin, a clean cross-file union.
    #[sqlx::test]
    async fn mixed_case_columns_fold_to_one_pin_and_are_renamed(pool: sqlx::PgPool) {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        // Majority spelling `Dur`, BIGINT, 3 populated rows.
        let majority = plant(
            &data_dir,
            "prod/2026-08-01/10/svc-a.parquet",
            "SELECT TIMESTAMP '2026-08-01 10:00:00' AS \"_time\", 'svc-a' AS service, \
             x::BIGINT AS \"Dur\" FROM (VALUES (410), (420), (430)) t(x)",
        );
        // Minority spelling `dur`, VARCHAR, 1 row.
        let minority = plant(
            &data_dir,
            "prod/2026-08-01/11/svc-b.parquet",
            "SELECT TIMESTAMP '2026-08-01 11:00:00' AS \"_time\", 'svc-b' AS service, \
             '1.5s' AS dur",
        );

        let store = CatalogStore::new(pool.clone());
        let cache = FieldCatalog::new();
        let summary = conform::ensure_conformance(&store, &cache, &data_dir, "2GB")
            .await
            .expect("boot pass runs");
        assert_eq!(
            summary.rewritten, 2,
            "the majority file is renamed, the minority file is cast"
        );

        // One pin, under the folded name, decided by most rows in the group.
        assert_eq!(
            cache.get("dur"),
            Some(trawl_core::schema::CanonicalType::BigInt),
            "the folded group pins once, most-rows-wins"
        );
        assert_eq!(cache.get("Dur"), None, "no mixed-case pin may exist");

        // Both files store the folded spelling at the pinned type.
        for file in [&majority, &minority] {
            let names = column_names(file);
            assert!(
                names.contains(&"dur".to_owned()) && !names.contains(&"Dur".to_owned()),
                "rewritten file must carry the folded column: {names:?}"
            );
            assert_eq!(column_type(file, "dur"), "BIGINT");
        }

        // The whole corpus reads through one union, one column, all rows.
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let (rows, kept): (i64, i64) = conn
            .query_row(
                &format!(
                    "SELECT count(*)::BIGINT, count(dur)::BIGINT FROM \
                     read_parquet('{}/**/*.parquet', union_by_name=true)",
                    data_dir.display()
                ),
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(rows, 4, "no rows lost");
        assert_eq!(kept, 3, "the unconvertible '1.5s' nulled (in _raw)");
    }

    #[sqlx::test]
    async fn second_boot_is_a_noop(pool: sqlx::PgPool) {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let (majority, minority) = plant_disagreeing_corpus(&data_dir);

        let store = CatalogStore::new(pool.clone());
        let cache = FieldCatalog::new();
        conform::ensure_conformance(&store, &cache, &data_dir, "2GB")
            .await
            .unwrap();
        let mtimes = |p: &std::path::Path| std::fs::metadata(p).unwrap().modified().unwrap();
        let (m1, m2) = (mtimes(&majority), mtimes(&minority));

        let summary = conform::ensure_conformance(&store, &cache, &data_dir, "2GB")
            .await
            .unwrap();
        assert!(!summary.ran, "second boot skips: marker + catalog agree");
        assert_eq!(summary.rewritten, 0);
        assert_eq!(mtimes(&majority), m1, "no file touched");
        assert_eq!(mtimes(&minority), m2, "no file touched");
    }

    #[sqlx::test]
    async fn mismatched_marker_forces_a_rerun(pool: sqlx::PgPool) {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        plant_disagreeing_corpus(&data_dir);

        let store = CatalogStore::new(pool.clone());
        let cache = FieldCatalog::new();
        conform::ensure_conformance(&store, &cache, &data_dir, "2GB")
            .await
            .unwrap();

        // A restored-from-backup data root (or repointed DATABASE_URL)
        // shows up as an identity mismatch — the pass must re-run.
        std::fs::write(data_dir.join("CATALOG"), "someone-elses-catalog\n").unwrap();
        let summary = conform::ensure_conformance(&store, &cache, &data_dir, "2GB")
            .await
            .unwrap();
        assert!(summary.ran, "identity mismatch must force a re-run");
        assert_eq!(
            summary.rewritten, 0,
            "already-conformant corpus: zero rewrites"
        );
        let marker = std::fs::read_to_string(data_dir.join("CATALOG")).unwrap();
        assert_eq!(marker.trim(), store.catalog_id().await.unwrap());
    }

    /// `_severity` is SEVERITY-pinned and sits in every parquet trawl
    /// writes, and its conform is a domain guard rather than a pass-through.
    /// Without a data-decided skip, a re-armed pass (identity mismatch, or
    /// the `clear_conformed` inside a repin cutover) would rewrite the
    /// entire archive in place on every re-arm; an in-ladder corpus must
    /// come out untouched.
    #[sqlx::test]
    async fn a_rerun_over_an_in_ladder_severity_corpus_rewrites_nothing(pool: sqlx::PgPool) {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let file = plant(
            &data_dir,
            "prod/2026-08-01/10/svc-a.parquet",
            "SELECT TIMESTAMP '2026-08-01 10:00:00' AS \"_time\", 'svc-a' AS service, \
             x::BIGINT AS \"_severity\" FROM (VALUES (9), (17), (24)) t(x)",
        );

        let store = CatalogStore::new(pool.clone());
        let cache = FieldCatalog::new();
        conform::ensure_conformance(&store, &cache, &data_dir, "2GB")
            .await
            .unwrap();
        let mtime = |p: &std::path::Path| std::fs::metadata(p).unwrap().modified().unwrap();
        let before = mtime(&file);

        std::fs::write(data_dir.join("CATALOG"), "someone-elses-catalog\n").unwrap();
        let summary = conform::ensure_conformance(&store, &cache, &data_dir, "2GB")
            .await
            .unwrap();
        assert!(summary.ran, "identity mismatch re-arms the pass");
        assert_eq!(
            summary.rewritten, 0,
            "every _severity is inside 1-24: the guard is the identity"
        );
        assert_eq!(mtime(&file), before, "no file touched");
    }

    /// Crash-mid-pass: the marker is published last, so a crash after the
    /// rewrites but before the marker leaves a conformant corpus with no
    /// marker. The re-run must be restartable — rewrite nothing (everything
    /// already conforms) and republish the marker.
    #[sqlx::test]
    async fn crash_before_marker_rerun_rewrites_nothing_and_republishes(pool: sqlx::PgPool) {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let (majority, minority) = plant_disagreeing_corpus(&data_dir);

        let store = CatalogStore::new(pool.clone());
        let cache = FieldCatalog::new();
        conform::ensure_conformance(&store, &cache, &data_dir, "2GB")
            .await
            .unwrap();

        // Simulate the crash window: marker gone, corpus already conformant.
        std::fs::remove_file(data_dir.join("CATALOG")).unwrap();
        let mtimes = |p: &std::path::Path| std::fs::metadata(p).unwrap().modified().unwrap();
        let (m1, m2) = (mtimes(&majority), mtimes(&minority));

        let summary = conform::ensure_conformance(&store, &cache, &data_dir, "2GB")
            .await
            .unwrap();
        assert!(summary.ran, "a missing marker must force the pass to run");
        assert_eq!(summary.rewritten, 0, "the conformant corpus is untouched");
        assert_eq!(mtimes(&majority), m1, "no file rewritten on the re-run");
        assert_eq!(mtimes(&minority), m2, "no file rewritten on the re-run");
        let marker = std::fs::read_to_string(data_dir.join("CATALOG")).unwrap();
        assert_eq!(
            marker.trim(),
            store.catalog_id().await.unwrap(),
            "the marker must be republished"
        );
    }

    /// Only live compaction writes `field_services`, so a corpus adopted
    /// from disk would carry pins but no observations, and the reads those
    /// rows are authoritative for answer wrong: `?service=` returns nothing
    /// for a service that has sent nothing since, and its pins never enter
    /// the `last_seen` window (a never-observed pin is always shown, by
    /// design). The boot pass backfills from the files it adopts.
    #[sqlx::test]
    async fn boot_pass_backfills_observations_for_a_pre_catalog_corpus(pool: sqlx::PgPool) {
        use trawl_server::store::FieldListFilter;

        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        // Two services with disjoint custom fields, three and two rows.
        plant(
            &data_dir,
            "prod/2026-08-01/10/svc-a.parquet",
            "SELECT TIMESTAMP '2026-08-01 10:00:00' AS \"_time\", 'svc-a' AS service, \
             x::BIGINT AS duration FROM (VALUES (410), (420), (430)) t(x)",
        );
        plant(
            &data_dir,
            "prod/2026-08-01/11/svc-b.parquet",
            "SELECT TIMESTAMP '2026-08-01 11:00:00' AS \"_time\", 'svc-b' AS service, \
             '/api' AS path FROM (VALUES (1), (2)) t(x)",
        );

        let store = CatalogStore::new(pool.clone());
        let cache = FieldCatalog::new();
        let summary = conform::ensure_conformance(&store, &cache, &data_dir, "2GB")
            .await
            .expect("boot pass runs");
        assert!(summary.observed > 0, "the standing corpus must be observed");

        // `?service=` answers for a service that has sent nothing since.
        let by_service = |service: &str| FieldListFilter {
            service: Some(service.to_owned()),
            since: None,
            limit: 100,
            with_conflicts: true,
        };
        let (rows, _) = store.list_fields(&by_service("svc-a")).await.unwrap();
        let names: Vec<&str> = rows.iter().map(|r| r.field.as_str()).collect();
        assert!(
            names.contains(&"duration") && names.contains(&"_time"),
            "svc-a's own columns must be listed: {names:?}"
        );
        assert!(
            !names.contains(&"path"),
            "svc-b's column must not leak into svc-a's listing: {names:?}"
        );

        // Observations are stamped from the partition directory, not now(),
        // and weighed by the rows the files hold.
        let obs = store
            .field_services("duration", None, 1000)
            .await
            .unwrap()
            .0;
        assert_eq!(obs.len(), 1);
        assert_eq!(obs[0].service, "svc-a");
        assert_eq!(obs[0].row_count, 3);
        assert_eq!(obs[0].first_seen.to_rfc3339(), "2026-08-01T10:00:00+00:00");
        assert_eq!(obs[0].last_seen.to_rfc3339(), "2026-08-01T10:00:00+00:00");

        // The observations are what let the `last_seen` window age the
        // corpus out at all.
        let (windowed, _) = store
            .list_fields(&FieldListFilter {
                service: None,
                since: Some(
                    chrono::DateTime::parse_from_rfc3339("2026-09-01T00:00:00Z")
                        .unwrap()
                        .into(),
                ),
                limit: 100,
                with_conflicts: true,
            })
            .await
            .unwrap();
        let windowed: Vec<&str> = windowed.iter().map(|r| r.field.as_str()).collect();
        assert!(
            !windowed.contains(&"duration"),
            "an observed-but-aged-out field must leave the window: {windowed:?}"
        );
        assert!(
            windowed.contains(&"message"),
            "a never-observed pin is always shown: {windowed:?}"
        );

        // Idempotent: the pass re-runs until the corpus is proven conformant
        // (missing marker, restored data root), and a re-run must not
        // re-accumulate the row counts it already recorded.
        std::fs::remove_file(data_dir.join("CATALOG")).unwrap();
        let rerun = conform::ensure_conformance(&store, &cache, &data_dir, "2GB")
            .await
            .unwrap();
        assert!(rerun.ran, "a missing marker forces the re-run");
        let again = store
            .field_services("duration", None, 1000)
            .await
            .unwrap()
            .0;
        assert_eq!(again[0].row_count, 3, "the backfill is idempotent");
        assert_eq!(again[0].first_seen, obs[0].first_seen);
        assert_eq!(again[0].last_seen, obs[0].last_seen);
    }

    /// Completion covers both file rewrites and observations. A database
    /// failure after scanning must leave the pass armed, so the next boot
    /// retries the standing corpus even when no sender sends it again.
    #[sqlx::test]
    async fn failed_observation_backfill_keeps_conformance_armed(pool: sqlx::PgPool) {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        plant(
            &data_dir,
            "prod/2026-08-01/10/svc-a.parquet",
            "SELECT TIMESTAMP '2026-08-01 10:00:00' AS \"_time\", 'svc-a' AS service, \
             x::BIGINT AS duration FROM (VALUES (410), (420), (430)) t(x)",
        );

        let store = CatalogStore::new(pool.clone());
        let cache = FieldCatalog::new();
        sqlx::raw_sql(
            "CREATE FUNCTION refuse_observation() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN RAISE EXCEPTION 'injected observation failure'; END $$;
             CREATE TRIGGER refuse_observation BEFORE INSERT ON field_services
             FOR EACH ROW EXECUTE FUNCTION refuse_observation();",
        )
        .execute(&pool)
        .await
        .unwrap();
        let error = conform::ensure_conformance(&store, &cache, &data_dir, "2GB")
            .await
            .expect_err("the observation failure must fail the boot pass");
        assert!(
            error.contains("failed to backfill field_services"),
            "{error}"
        );
        assert!(!store.is_conformed().await.unwrap());
        assert!(!data_dir.join("CATALOG").exists());
        assert!(
            store
                .field_services("duration", None, 1000)
                .await
                .unwrap()
                .0
                .is_empty()
        );

        sqlx::raw_sql(
            "DROP TRIGGER refuse_observation ON field_services;
             DROP FUNCTION refuse_observation();",
        )
        .execute(&pool)
        .await
        .unwrap();
        let retry = conform::ensure_conformance(&store, &cache, &data_dir, "2GB")
            .await
            .unwrap();
        assert!(retry.ran, "an incomplete pass must retry");
        assert!(store.is_conformed().await.unwrap());
        assert_eq!(
            std::fs::read_to_string(data_dir.join("CATALOG"))
                .unwrap()
                .trim(),
            store.catalog_id().await.unwrap()
        );
        let obs = store
            .field_services("duration", None, 1000)
            .await
            .unwrap()
            .0;
        assert_eq!(obs.len(), 1, "the standing corpus is observed: {obs:?}");
        assert_eq!(obs[0].service, "svc-a");
        assert_eq!(obs[0].row_count, 3);

        let settled = conform::ensure_conformance(&store, &cache, &data_dir, "2GB")
            .await
            .unwrap();
        assert!(
            !settled.ran,
            "completion and the matching marker skip the pass"
        );
    }

    /// A live tick's accumulated `row_count` must survive a later backfill
    /// that sees a retention-shrunk corpus: the backfill takes the max, it
    /// never rewrites a count downward.
    #[sqlx::test]
    async fn backfill_never_clobbers_a_live_count_downward(pool: sqlx::PgPool) {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        plant(
            &data_dir,
            "prod/2026-08-01/10/svc-a.parquet",
            "SELECT TIMESTAMP '2026-08-01 10:00:00' AS \"_time\", 'svc-a' AS service, \
             x::BIGINT AS duration FROM (VALUES (410), (420), (430)) t(x)",
        );
        let store = CatalogStore::new(pool.clone());
        let cache = FieldCatalog::new();
        conform::ensure_conformance(&store, &cache, &data_dir, "2GB")
            .await
            .unwrap();
        // Live compaction accumulates past the corpus the pass saw.
        store
            .touch_services("svc-a", &["duration".to_owned()], 900)
            .await
            .unwrap();

        std::fs::remove_file(data_dir.join("CATALOG")).unwrap();
        conform::ensure_conformance(&store, &cache, &data_dir, "2GB")
            .await
            .unwrap();

        let obs = store
            .field_services("duration", None, 1000)
            .await
            .unwrap()
            .0;
        assert_eq!(obs[0].row_count, 903, "the live count stands");
        assert!(
            obs[0].last_seen > obs[0].first_seen,
            "the live touch's now() stays the latest observation"
        );
    }

    /// `data/scheduled/**` holds report-run outputs, not the log corpus —
    /// the boot pass must never scan it: no pins from its columns, no
    /// rewrite of its files.
    #[sqlx::test]
    async fn scheduled_dir_is_never_scanned(pool: sqlx::PgPool) {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        plant_disagreeing_corpus(&data_dir);
        // A scheduled-output file whose column set would otherwise pin.
        let sched = plant(
            &data_dir,
            "scheduled/42/report.parquet",
            "SELECT 'x' AS sched_only_field, '1.5s' AS duration",
        );

        let store = CatalogStore::new(pool.clone());
        let cache = FieldCatalog::new();
        let before = std::fs::metadata(&sched).unwrap().modified().unwrap();
        conform::ensure_conformance(&store, &cache, &data_dir, "2GB")
            .await
            .expect("boot pass runs");

        assert!(
            cache.get("sched_only_field").is_none(),
            "columns seen only under scheduled/ must not pin"
        );
        assert_eq!(
            std::fs::metadata(&sched).unwrap().modified().unwrap(),
            before,
            "scheduled/ files must never be rewritten"
        );
        assert_eq!(
            column_type(&sched, "duration"),
            "VARCHAR",
            "the scheduled file keeps its own schema even where it disagrees \
             with the corpus pin"
        );
    }

    /// A readable parquet in a foreign layout is still not trawl's file.
    /// The rewrite is in place, lossy and irreversible (no backup, no
    /// dry-run, no opt-in), so the boot pass decides "mine" from the path,
    /// before it opens anything: an operator's own parquet dropped under the
    /// data root is left byte-identical, never votes on a pin, and, like
    /// every other skip, withholds completion so the next boot re-runs
    /// rather than declaring the corpus proven.
    #[sqlx::test]
    async fn foreign_layout_files_are_never_rewritten(pool: sqlx::PgPool) {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let (majority, minority) = plant_disagreeing_corpus(&data_dir);

        // Perfectly readable, disagrees with the corpus pin, carries a field
        // of its own — and sits nowhere ingest could have written it.
        let foreign = plant(
            &data_dir,
            "operator-scratch/export.parquet",
            "SELECT 'x' AS operator_only_field, '1.5s' AS duration",
        );
        let bytes_before = std::fs::read(&foreign).unwrap();

        let store = CatalogStore::new(pool.clone());
        let cache = FieldCatalog::new();
        let summary = conform::ensure_conformance(&store, &cache, &data_dir, "2GB")
            .await
            .expect("boot pass runs");

        assert_eq!(summary.skipped, 1, "the foreign file is one skip");
        assert_eq!(summary.scanned, 2, "only trawl's own files are scanned");
        assert_eq!(summary.rewritten, 1, "the minority file still conforms");
        assert_eq!(column_type(&minority, "duration"), "BIGINT");
        assert_eq!(column_type(&majority, "duration"), "BIGINT");

        assert_eq!(
            std::fs::read(&foreign).unwrap(),
            bytes_before,
            "a foreign file must come out of the boot pass byte-identical"
        );
        assert!(
            cache.get("operator_only_field").is_none(),
            "a foreign file's columns must not vote on the catalog"
        );
        assert!(
            !data_dir.join("CATALOG").exists(),
            "an unproven corpus must not publish the conformance identity"
        );
    }

    /// One unreadable or foreign `.parquet` under the data root must never
    /// take trawld's boot down with it: the pass isolates it per file
    /// (skip + warn + count), conforms everything else, and — because the
    /// corpus was not proven conformant — withholds completion so the next
    /// boot re-runs. Once the bad file is gone, the pass completes normally.
    #[sqlx::test]
    async fn unreadable_files_are_skipped_not_boot_fatal(pool: sqlx::PgPool) {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let (majority, minority) = plant_disagreeing_corpus(&data_dir);

        // Fails the magic-byte sniff outright (a non-trawl file dropped in,
        // or a zero-fill from a disk-full event).
        let junk = data_dir.join("prod/2026-08-01/12/junk.parquet");
        std::fs::create_dir_all(junk.parent().unwrap()).unwrap();
        std::fs::write(&junk, b"not a parquet file at all").unwrap();
        // Passes the sniff (PAR1 bookends) but `read_parquet` cannot parse
        // it: bogus footer length, i.e. bit rot in the footer.
        let unreadable = data_dir.join("prod/2026-08-01/13/rotted.parquet");
        std::fs::create_dir_all(unreadable.parent().unwrap()).unwrap();
        std::fs::write(&unreadable, b"PAR1\xff\xff\xff\xffPAR1").unwrap();

        let store = CatalogStore::new(pool.clone());
        let cache = FieldCatalog::new();
        let summary = conform::ensure_conformance(&store, &cache, &data_dir, "2GB")
            .await
            .expect("a bad file must not fail the boot pass");
        assert!(summary.ran);
        assert_eq!(summary.skipped, 2, "both bad files are skipped, not fatal");
        assert_eq!(summary.scanned, 2, "only the readable corpus is scanned");
        assert_eq!(
            summary.rewritten, 1,
            "the readable minority file still gets conformed"
        );
        assert_eq!(column_type(&minority, "duration"), "BIGINT");
        assert_eq!(column_type(&majority, "duration"), "BIGINT");

        // Skipped means untouched — the boot pass never moves or rewrites a
        // file it could not read.
        assert_eq!(std::fs::read(&junk).unwrap(), b"not a parquet file at all");
        assert_eq!(
            std::fs::read(&unreadable).unwrap(),
            b"PAR1\xff\xff\xff\xffPAR1"
        );

        // Completion is withheld: no marker, and the next boot re-runs.
        assert!(
            !data_dir.join("CATALOG").exists(),
            "an unproven corpus must not publish the conformance identity"
        );
        let again = conform::ensure_conformance(&store, &cache, &data_dir, "2GB")
            .await
            .unwrap();
        assert!(again.ran, "skipped files force a re-run on the next boot");
        assert_eq!(again.skipped, 2);
        assert_eq!(again.rewritten, 0, "the readable corpus already conforms");

        // Operator removes the bad files → the pass completes and publishes.
        std::fs::remove_file(&junk).unwrap();
        std::fs::remove_file(&unreadable).unwrap();
        let clean = conform::ensure_conformance(&store, &cache, &data_dir, "2GB")
            .await
            .unwrap();
        assert!(clean.ran);
        assert_eq!(clean.skipped, 0);
        let marker = std::fs::read_to_string(data_dir.join("CATALOG")).unwrap();
        assert_eq!(marker.trim(), store.catalog_id().await.unwrap());
    }

    /// A directory the walk cannot enumerate is isolated exactly like an
    /// unreadable file. The walk covers more than the corpus — the WAL tree
    /// lives under the data root by default — so one `chmod 000` subdirectory
    /// (permissions, a transient fault, a stale handle) must not be the
    /// difference between a daemon that boots and one that refuses to.
    #[cfg(unix)]
    #[sqlx::test]
    async fn unreadable_directories_are_skipped_not_boot_fatal(pool: sqlx::PgPool) {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let (majority, minority) = plant_disagreeing_corpus(&data_dir);

        let locked = data_dir.join("prod/2026-08-01/14");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_dir(&locked).is_ok() {
            // Running as root: mode bits are not enforced, nothing to test.
            let _ = std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755));
            return;
        }

        let store = CatalogStore::new(pool.clone());
        let cache = FieldCatalog::new();
        let summary = conform::ensure_conformance(&store, &cache, &data_dir, "2GB")
            .await
            .expect("an unreadable directory must not fail the boot pass");
        assert!(summary.ran);
        assert_eq!(summary.skipped, 1, "the unreadable directory is one skip");
        assert_eq!(summary.scanned, 2, "the readable corpus is still scanned");
        assert_eq!(
            summary.rewritten, 1,
            "the readable minority file still gets conformed"
        );
        assert_eq!(column_type(&minority, "duration"), "BIGINT");
        assert_eq!(column_type(&majority, "duration"), "BIGINT");
        assert!(
            !data_dir.join("CATALOG").exists(),
            "an unproven corpus must not publish the conformance identity"
        );

        // Operator fixes the permissions → the pass completes and publishes.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        let clean = conform::ensure_conformance(&store, &cache, &data_dir, "2GB")
            .await
            .unwrap();
        assert!(clean.ran, "the skipped directory forced a re-run");
        assert_eq!(clean.skipped, 0);
        assert!(data_dir.join("CATALOG").exists());
    }

    /// A query-only node (`[ingest] enabled = false`) never runs the pass,
    /// so nothing else proves the archive it serves came from the catalog it
    /// is connected to. `/api/v1/schema` answers from that catalog's pins, so
    /// a repointed app database over a populated foreign archive would
    /// advertise the seeded envelope while queries read entirely different
    /// physical columns. A marker naming another catalog is positive proof of
    /// that pairing, and the boot gate refuses it.
    #[sqlx::test]
    async fn query_only_boot_refuses_an_archive_from_another_catalog(pool: sqlx::PgPool) {
        let store = CatalogStore::new(pool.clone());

        // Marker naming a different catalog (the app database was repointed,
        // or the data root was restored from another install's backup).
        let stale = tempfile::tempdir().unwrap();
        let stale_data = stale.path().join("data");
        plant_disagreeing_corpus(&stale_data);
        std::fs::write(
            stale_data.join("CATALOG"),
            "00000000-0000-0000-0000-000000000000\n",
        )
        .unwrap();
        let err = conform::verify_archive_identity(&store, &stale_data)
            .await
            .expect_err("a marker from another catalog must not pass the gate");
        assert!(
            err.contains("00000000-0000-0000-0000-000000000000"),
            "the refusal must name the foreign catalog id: {err}"
        );
    }

    /// An archive with NO marker is not proof of a foreign catalog — it is
    /// what an incomplete conformance pass leaves behind (any skipped path
    /// withholds `data/CATALOG`), and an ingest node warns and serves it. The
    /// query-only gate must answer that identical state the same way, or
    /// "disable ingest and restart to investigate" becomes a startup failure
    /// curable only by re-enabling ingest.
    #[sqlx::test]
    async fn query_only_boot_serves_an_unmarked_archive_unproven(pool: sqlx::PgPool) {
        let store = CatalogStore::new(pool.clone());

        let unmarked = tempfile::tempdir().unwrap();
        let unmarked_data = unmarked.path().join("data");
        plant_disagreeing_corpus(&unmarked_data);
        assert_eq!(
            conform::verify_archive_identity(&store, &unmarked_data)
                .await
                .expect("an unmarked archive must not refuse the boot"),
            conform::ArchiveIdentity::Unproven,
            "no marker means unproven, not foreign"
        );

        // The state a pass that skipped a path actually leaves: it ran, it
        // rewrote what it owned, and it withheld the marker.
        let cache = FieldCatalog::new();
        let skipped = tempfile::tempdir().unwrap();
        let skipped_data = skipped.path().join("data");
        plant_disagreeing_corpus(&skipped_data);
        std::fs::create_dir_all(skipped_data.join("exports")).unwrap();
        std::fs::copy(
            skipped_data.join("prod/2026-08-01/10/svc-a.parquet"),
            skipped_data.join("exports/report.parquet"),
        )
        .unwrap();
        let summary = conform::ensure_conformance(&store, &cache, &skipped_data, "2GB")
            .await
            .expect("an operator's export subtree is skipped, not fatal");
        assert!(summary.skipped > 0 && !skipped_data.join("CATALOG").exists());
        assert_eq!(
            conform::verify_archive_identity(&store, &skipped_data)
                .await
                .expect("the same corpus must boot a query-only node too"),
            conform::ArchiveIdentity::Unproven
        );
    }

    /// The gate is identity, not paranoia: an archive this catalog conformed
    /// passes, and so does a cold start with no parquet at all (there is no
    /// schema to get wrong).
    #[sqlx::test]
    async fn query_only_boot_accepts_its_own_and_empty_archives(pool: sqlx::PgPool) {
        let store = CatalogStore::new(pool.clone());
        let cache = FieldCatalog::new();

        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        plant_disagreeing_corpus(&data_dir);
        conform::ensure_conformance(&store, &cache, &data_dir, "2GB")
            .await
            .expect("boot pass runs");
        assert_eq!(
            conform::verify_archive_identity(&store, &data_dir)
                .await
                .expect("an archive this catalog conformed must pass the gate"),
            conform::ArchiveIdentity::Proven,
            "the published marker proves the identity"
        );

        let empty = tempfile::tempdir().unwrap();
        assert_eq!(
            conform::verify_archive_identity(&store, &empty.path().join("data"))
                .await
                .expect("a cold start has no archive to misdescribe"),
            conform::ArchiveIdentity::Empty,
            "an empty archive is unproven but harmless"
        );
    }

    /// Wiring: server boot itself runs the conformance pass — pins land in
    /// the process cache and one query returns the full corrected corpus,
    /// without this test ever calling `ensure_conformance`.
    #[tokio::test(flavor = "multi_thread")]
    async fn server_boot_runs_the_conformance_pass() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let data_dir = root.join("data");
        let (majority, minority) = plant_disagreeing_corpus(&data_dir);
        let data_path = data_dir.to_str().unwrap().to_owned();

        let server = crate::common::setup_in_dir_with_data(
            &root,
            data_path,
            trawl_server::config::RateLimitConfig::default(),
        )
        .await;
        std::mem::forget(tmp);

        // Pins were hydrated by boot, not by this test.
        assert_eq!(
            server.state.query.field_catalog.get("duration"),
            Some(trawl_core::schema::CanonicalType::BigInt),
            "boot must seed and hydrate the pin cache"
        );

        // The corpus was corrected on disk.
        assert_eq!(column_type(&minority, "duration"), "BIGINT");
        assert_eq!(column_type(&majority, "duration"), "BIGINT");

        // One union returns the full corrected corpus.
        let query =
            trawl_client::HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
        let result = query
            .query_paginated("service=svc-a OR service=svc-b", None, None)
            .await
            .expect("query across the corrected corpus must succeed");
        assert_eq!(result.result.row_count(), 4, "full corpus visible");
    }
}

/// ADR-0011 slice A acceptance: comparisons against a VARCHAR-pinned
/// field follow the pin over HTTP — with the hot buffer populated AND
/// drained (the drained case exercises the formerly pin-blind cold-only
/// `run_query` branch).
#[tokio::test(flavor = "multi_thread")]
async fn pinned_varchar_comparisons_behave_over_http() {
    let h = harness().await;

    // Mixed numeric-looking and word statuses (all strings) — the ladder
    // pins VARCHAR at first compaction.
    let statuses = ["200", "404", "accepted", "301"];
    let events: Vec<serde_json::Value> = statuses
        .iter()
        .map(|s| {
            json!({
                "service": "pin-svc", "env": "prod", "host": "web01",
                "timestamp": now_ts(), "message": format!("s-{s}"), "status": s,
            })
        })
        .collect();
    let resp = h.ingest.ingest(&events).await.expect("ingest");
    assert_eq!(resp.accepted, 4);
    compact_tick(&h.server, &h.wal_dir, &h.data_dir).await;
    assert_eq!(
        h.server.state.query.field_catalog.get("status"),
        Some(trawl_core::schema::CanonicalType::Varchar),
        "the mixed batch must pin status VARCHAR"
    );

    let count = |dsl: &str| {
        let query = &h.query;
        let dsl = dsl.to_owned();
        async move {
            query
                .query_paginated(&dsl, None, None)
                .await
                .unwrap_or_else(|e| panic!("{dsl}: pinned comparison must not error: {e}"))
                .result
                .row_count()
        }
    };

    // Drained phase: the hot buffer emptied at compaction, so these ride
    // the cold-only run_query branch.
    assert_eq!(count("service=pin-svc status=200 last=1h").await, 1);
    assert_eq!(count("service=pin-svc status=200,301 last=1h").await, 2);
    assert_eq!(count("service=pin-svc status!=200 last=1h").await, 3);
    assert_eq!(
        count("service=pin-svc status>=400 last=1h").await,
        1,
        "only '404' is >= 400 — 'accepted' NULLs out instead of erroring"
    );

    // Populated phase: fresh uncompacted events ride the hot branch of
    // the union, under the same comparison pins.
    let more: Vec<serde_json::Value> = ["500", "accepted"]
        .iter()
        .map(|s| {
            json!({
                "service": "pin-svc", "env": "prod", "host": "web01",
                "timestamp": now_ts(), "message": format!("hot-{s}"), "status": s,
            })
        })
        .collect();
    let resp = h.ingest.ingest(&more).await.expect("ingest hot");
    assert_eq!(resp.accepted, 2);

    assert_eq!(count("service=pin-svc status=200 last=1h").await, 1);
    assert_eq!(count("service=pin-svc status=200,301 last=1h").await, 2);
    assert_eq!(count("service=pin-svc status!=200 last=1h").await, 5);
    assert_eq!(
        count("service=pin-svc status>=400 last=1h").await,
        2,
        "cold '404' plus hot '500'"
    );

    // Wire numbers under the same VARCHAR pin. `read_json` types the
    // column from the whole batch, so the fractional sibling widens it to
    // DOUBLE and `200` is stored as the text "200.0" — durably, once
    // compaction writes the parquet. The equality rule carries the
    // value's numeric reading precisely so this row is still `status=200`
    // on both the hot and the cold side, and on the live tail.
    let numeric: Vec<serde_json::Value> = [json!(200), json!(200.5)]
        .iter()
        .map(|n| {
            json!({
                "service": "pin-svc", "env": "prod", "host": "web01",
                "timestamp": now_ts(), "message": format!("num-{n}"), "status": n,
            })
        })
        .collect();
    assert_eq!(
        h.ingest
            .ingest(&numeric)
            .await
            .expect("ingest numeric")
            .accepted,
        2
    );
    assert_eq!(
        count("service=pin-svc status=200 last=1h").await,
        2,
        "the wire 200 matches alongside the stored '200'"
    );

    compact_tick(&h.server, &h.wal_dir, &h.data_dir).await;
    assert_eq!(
        h.server.state.query.field_catalog.get("status"),
        Some(trawl_core::schema::CanonicalType::Varchar),
        "the pin is unchanged — numbers conform to text"
    );
    assert_eq!(
        count("service=pin-svc status=200 last=1h").await,
        2,
        "and still matches once the widened text is on disk"
    );
    assert_eq!(
        count("service=pin-svc status>=200.5 last=1h").await,
        4,
        "'301', '404', '500' and the stored '200.5'"
    );
}

/// Read SSE chunks until `needle` shows up (bounded), returning the
/// accumulated body.
async fn read_until(resp: &mut reqwest::Response, needle: &str) -> String {
    let mut buf = String::new();
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            let chunk = resp.chunk().await.unwrap().expect("stream ended early");
            buf.push_str(&String::from_utf8_lossy(&chunk));
            if buf.contains(needle) {
                break;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("needle {needle:?} never arrived; got: {buf}"));
    buf
}

/// ADR-0011 slice A acceptance: live tail receives the pins. A string
/// "404" event matches `status>=400` on the SSE stream, and the
/// equality rule is discriminably live — `status!=200` must match
/// "accepted" (the pin-blind numeric coercion drops it) while still
/// excluding every spelling of 200.
#[tokio::test(flavor = "multi_thread")]
async fn sse_stream_applies_varchar_pin() {
    let h = harness().await;

    // Seed the VARCHAR pin.
    let seed: Vec<serde_json::Value> = ["200", "404", "accepted"]
        .iter()
        .map(|s| {
            json!({
                "service": "sse-svc", "env": "prod", "host": "web01",
                "timestamp": now_ts(), "message": format!("seed-{s}"), "status": s,
            })
        })
        .collect();
    h.ingest.ingest(&seed).await.expect("ingest seed");
    compact_tick(&h.server, &h.wal_dir, &h.data_dir).await;
    assert_eq!(
        h.server.state.query.field_catalog.get("status"),
        Some(trawl_core::schema::CanonicalType::Varchar)
    );

    let raw = common::harness_client_builder().build().unwrap();

    // Ordered-numeric rule live: string "404" matches status>=400.
    let mut stream = raw
        .get(format!("{}/api/v1/stream", h.server.url))
        .query(&[("query", "service=sse-svc status>=400")])
        .bearer_auth(&h.server.analyst_token)
        .send()
        .await
        .expect("open stream");
    assert!(stream.status().is_success(), "{stream:?}");
    let live: Vec<serde_json::Value> = vec![
        json!({
            "service": "sse-svc", "env": "prod", "host": "web01",
            "timestamp": now_ts(), "message": "sse-decoy", "status": "accepted",
        }),
        json!({
            "service": "sse-svc", "env": "prod", "host": "web01",
            "timestamp": now_ts(), "message": "sse-needle", "status": "404",
        }),
    ];
    h.ingest.ingest(&live).await.expect("ingest live");
    let buf = read_until(&mut stream, "sse-needle").await;
    assert!(
        !buf.contains("sse-decoy"),
        "'accepted' must NULL out of status>=400, not match: {buf}"
    );
    drop(stream);

    // Equality rule live, discriminably: `status!=200` must return
    // "accepted", which a pin-blind numeric coercion cannot read and would
    // answer no-match for, while both spellings of 200 stay excluded: the
    // text one through the text arm, "200.00" through the numeric reading
    // that keeps batch and live agreeing about a widened column.
    let mut stream = raw
        .get(format!("{}/api/v1/stream", h.server.url))
        .query(&[("query", "service=sse-svc status!=200")])
        .bearer_auth(&h.server.analyst_token)
        .send()
        .await
        .expect("open eq stream");
    assert!(stream.status().is_success(), "{stream:?}");
    let live: Vec<serde_json::Value> = vec![
        json!({
            "service": "sse-svc", "env": "prod", "host": "web01",
            "timestamp": now_ts(), "message": "exact-form", "status": "200",
        }),
        json!({
            "service": "sse-svc", "env": "prod", "host": "web01",
            "timestamp": now_ts(), "message": "decimal-form", "status": "200.00",
        }),
        json!({
            "service": "sse-svc", "env": "prod", "host": "web01",
            "timestamp": now_ts(), "message": "word-form", "status": "accepted",
        }),
    ];
    h.ingest.ingest(&live).await.expect("ingest eq live");
    let buf = read_until(&mut stream, "word-form").await;
    assert!(
        !buf.contains("exact-form") && !buf.contains("decimal-form"),
        "no spelling of 200 may match status!=200: {buf}"
    );
}
