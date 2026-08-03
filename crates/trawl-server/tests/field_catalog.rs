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

/// Acceptance: a HOT-side conflict — an uncompacted event disagreeing with
/// an existing pin — is nulled on the hot branch of the union while the
/// full cold history stays visible. The end-to-end proof of the
/// `HotSnapshot` pin plumbing: the outcome is never hot-only (the cold rows
/// ARE present) and never a loud error (the pin conformance resolves the
/// conflict in one execution).
#[sqlx::test(migrations = false)]
async fn hot_conflicting_event_is_nulled_and_cold_history_survives(pool: sqlx::PgPool) {
    let h = harness(pool).await;

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
    // 1 row; the deleted coerced retry would show strings.
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

// ---------------------------------------------------------------------------
// boot conformance pass (ADR-0009 slice 2, section 2b)
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

    /// The vote weighs rows that CARRY the field, not the file's row count:
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

    /// Crash-mid-pass: the marker is published LAST, so a crash after the
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

    /// Wiring: server boot itself runs the conformance pass — pins land in
    /// the process cache and one query returns the full corrected corpus,
    /// without this test ever calling `ensure_conformance`.
    #[sqlx::test(migrations = false)]
    async fn server_boot_runs_the_conformance_pass(pool: sqlx::PgPool) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let data_dir = root.join("data");
        let (majority, minority) = plant_disagreeing_corpus(&data_dir);
        let data_glob = format!("{}/**/*.parquet", data_dir.display());

        let server = crate::common::setup_in_dir_with_data(
            pool,
            &root,
            data_glob,
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
