// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! End-to-end tests for the field catalog (ADR-0009 slice 2): write-time
//! type conformance kills both silent data-loss paths — the cold/cold
//! whole-history drop and the nested-key whole-batch column drop.
//!
//! Modeled on `timestamp_repair.rs`: ingest through the real handler,
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
#[sqlx::test(migrations = false)]
async fn cold_cold_conflict_returns_full_history_with_attribution(pool: sqlx::PgPool) {
    let h = harness(pool).await;

    // Service A: duration is an integer. Compacted FIRST, so it pins BIGINT.
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
    // A's parquet rows — never degrade to hot-only (which here would be
    // zero rows with HTTP 200: the exact defect).
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
    let metrics_client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();
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
#[sqlx::test(migrations = false)]
async fn nested_object_keeps_batchmates_and_stays_reachable(pool: sqlx::PgPool) {
    let h = harness(pool).await;

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

    // Every custom column landed in parquet — nothing was dropped for the
    // batch (the old ten-column fallback would have kept only the
    // envelope).
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
#[sqlx::test(migrations = false)]
async fn all_null_first_batch_defers_then_pins(pool: sqlx::PgPool) {
    let h = harness(pool).await;

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

/// Acceptance: a pin write failure means NO parquet is written and the WAL
/// is retained for retry — never an unconformant file.
#[sqlx::test(migrations = false)]
async fn pin_write_failure_retains_wal_and_writes_nothing(pool: sqlx::PgPool) {
    let h = harness(pool).await;

    let event = json!({
        "service": "svc-pinfail", "env": "prod", "host": "h",
        "timestamp": now_ts(), "message": "m",
        "brand_new_field": 1,
    });
    h.ingest.ingest(&[event]).await.expect("ingest");

    // A catalog whose store points at an unreachable postgres: pins cannot
    // become durable, so the batch must not be written.
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

/// Acceptance: `field_services` is ever-observed — compaction advances
/// `last_seen`, and retention deleting a date directory leaves the rows in
/// place (documented semantics, not a leak).
#[sqlx::test(migrations = false)]
async fn field_services_is_ever_observed(pool: sqlx::PgPool) {
    let h = harness(pool).await;

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
        .field_services("duration")
        .await
        .unwrap();
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
        .field_services("duration")
        .await
        .unwrap();
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
        .field_services("duration")
        .await
        .unwrap();
    assert_eq!(
        after.len(),
        1,
        "field_services rows are ever-observed; retention never reconciles them"
    );
}

/// Recursively find parquet files.
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
