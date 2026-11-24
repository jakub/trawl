// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! End-to-end tests for the field catalog (ADR-0009).

mod common;

use std::time::Duration;

use common::{TestServer, setup_in_dir_with_data};
use serde_json::json;

fn now_ts() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}
use trawl_api::value::Value;
use trawl_client::HttpClient;
use trawl_server::catalog::CatalogContext;
use trawl_server::config::RateLimitConfig;

fn catalog_ctx(server: &TestServer) -> CatalogContext {
    CatalogContext {
        store: server.state.storage.catalog.clone(),
        cache: server.state.query.field_catalog.clone(),
    }
}

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

#[tokio::test(flavor = "multi_thread")]
async fn cold_cold_conflict_returns_full_history_with_attribution() {
    let h = harness().await;

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

    let all = h
        .query
        .query_paginated("last=1h", None, None)
        .await
        .expect("query all");
    assert_eq!(all.result.row_count(), 5, "no history is dropped");

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

#[tokio::test(flavor = "multi_thread")]
async fn pin_write_failure_retains_wal_and_writes_nothing() {
    let h = harness().await;

    let event = json!({
        "service": "svc-pinfail", "env": "prod", "host": "h",
        "timestamp": now_ts(), "message": "m",
        "brand_new_field": 1,
    });
    h.ingest.ingest(&[event]).await.expect("ingest");

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

    compact_tick(&h.server, &h.wal_dir, &h.data_dir).await;
    assert!(!walkdir_parquet(&h.data_dir).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn hot_conflicting_event_is_nulled_and_cold_history_survives() {
    let h = harness().await;

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

#[tokio::test(flavor = "multi_thread")]
async fn case_variant_field_names_fold_to_one_column_across_services() {
    let h = harness().await;

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

mod boot {
    use std::collections::HashSet;

    use trawl_server::catalog::{FieldCatalog, conform};
    use trawl_server::store::CatalogStore;

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
        let summary = conform::ensure_conformance(
            &store,
            &cache,
            &data_dir,
            &data_dir.join("wal"),
            HashSet::new(),
            "2GB",
        )
        .await
        .expect("boot pass runs");
        assert!(summary.ran, "first boot must run the pass");
        assert_eq!(
            summary.rewritten, 1,
            "exactly the minority file is rewritten"
        );

        assert_eq!(
            cache.get("duration"),
            Some(trawl_core::schema::CanonicalType::BigInt),
            "the cache is hydrated with the most-rows-wins pin"
        );

        assert_eq!(column_type(&minority, "duration"), "BIGINT");
        assert_eq!(column_type(&majority, "duration"), "BIGINT");

        let conflicts = store.conflicts_for_field("duration").await.unwrap();
        assert!(
            conflicts
                .iter()
                .any(|c| c.service == "svc-b" && c.expected_type == "BIGINT"),
            "boot rewrite must record the conflict: {conflicts:?}"
        );

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

        let marker = std::fs::read_to_string(data_dir.join("CATALOG")).unwrap();
        assert_eq!(marker.trim(), store.catalog_id().await.unwrap());
    }

    #[sqlx::test]
    async fn unparseable_standing_time_conforms_to_the_partition_instant(pool: sqlx::PgPool) {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        plant(
            &data_dir,
            "prod/2026-08-01/10/svc-a.parquet",
            "SELECT TIMESTAMP '2026-08-01 10:00:00' + INTERVAL (x) MINUTE AS \"_time\", \
             'svc-a' AS service FROM (VALUES (1), (2), (3)) t(x)",
        );
        let minority = plant(
            &data_dir,
            "prod/2026-08-01/11/svc-b.parquet",
            "SELECT 'yesterday-ish' AS \"_time\", 'svc-b' AS service",
        );

        let store = CatalogStore::new(pool.clone());
        let cache = FieldCatalog::new();
        let summary = conform::ensure_conformance(
            &store,
            &cache,
            &data_dir,
            &data_dir.join("wal"),
            HashSet::new(),
            "2GB",
        )
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

        let conflicts = store.conflicts_for_field("_time").await.unwrap();
        assert!(
            conflicts
                .iter()
                .any(|c| c.service == "svc-b" && c.rows_nulled == 1),
            "the unreadable timestamp is conflict evidence: {conflicts:?}"
        );
    }

    #[sqlx::test]
    async fn all_null_column_in_a_bigger_file_does_not_win_the_pin(pool: sqlx::PgPool) {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        plant(
            &data_dir,
            "prod/2026-08-01/10/svc-a.parquet",
            "SELECT TIMESTAMP '2026-08-01 10:00:00' AS \"_time\", 'svc-a' AS service, \
             NULL::BIGINT AS duration FROM range(6)",
        );
        let populated = plant(
            &data_dir,
            "prod/2026-08-01/11/svc-b.parquet",
            "SELECT TIMESTAMP '2026-08-01 11:00:00' AS \"_time\", 'svc-b' AS service, \
             x AS duration FROM (VALUES ('1.5s'), ('2.5s')) t(x)",
        );

        let store = CatalogStore::new(pool.clone());
        let cache = FieldCatalog::new();
        conform::ensure_conformance(
            &store,
            &cache,
            &data_dir,
            &data_dir.join("wal"),
            HashSet::new(),
            "2GB",
        )
        .await
        .expect("boot pass runs");

        assert_eq!(
            cache.get("duration"),
            Some(trawl_core::schema::CanonicalType::Varchar),
            "the only column carrying values wins the pin"
        );
        assert_eq!(column_type(&populated, "duration"), "VARCHAR");

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

    #[sqlx::test]
    async fn mixed_case_columns_fold_to_one_pin_and_are_renamed(pool: sqlx::PgPool) {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let majority = plant(
            &data_dir,
            "prod/2026-08-01/10/svc-a.parquet",
            "SELECT TIMESTAMP '2026-08-01 10:00:00' AS \"_time\", 'svc-a' AS service, \
             x::BIGINT AS \"Dur\" FROM (VALUES (410), (420), (430)) t(x)",
        );
        let minority = plant(
            &data_dir,
            "prod/2026-08-01/11/svc-b.parquet",
            "SELECT TIMESTAMP '2026-08-01 11:00:00' AS \"_time\", 'svc-b' AS service, \
             '1.5s' AS dur",
        );

        let store = CatalogStore::new(pool.clone());
        let cache = FieldCatalog::new();
        let summary = conform::ensure_conformance(
            &store,
            &cache,
            &data_dir,
            &data_dir.join("wal"),
            HashSet::new(),
            "2GB",
        )
        .await
        .expect("boot pass runs");
        assert_eq!(
            summary.rewritten, 2,
            "the majority file is renamed, the minority file is cast"
        );

        assert_eq!(
            cache.get("dur"),
            Some(trawl_core::schema::CanonicalType::BigInt),
            "the folded group pins once, most-rows-wins"
        );
        assert_eq!(cache.get("Dur"), None, "no mixed-case pin may exist");

        for file in [&majority, &minority] {
            let names = column_names(file);
            assert!(
                names.contains(&"dur".to_owned()) && !names.contains(&"Dur".to_owned()),
                "rewritten file must carry the folded column: {names:?}"
            );
            assert_eq!(column_type(file, "dur"), "BIGINT");
        }

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
        conform::ensure_conformance(
            &store,
            &cache,
            &data_dir,
            &data_dir.join("wal"),
            HashSet::new(),
            "2GB",
        )
        .await
        .unwrap();
        let mtimes = |p: &std::path::Path| std::fs::metadata(p).unwrap().modified().unwrap();
        let (m1, m2) = (mtimes(&majority), mtimes(&minority));

        let summary = conform::ensure_conformance(
            &store,
            &cache,
            &data_dir,
            &data_dir.join("wal"),
            HashSet::new(),
            "2GB",
        )
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
        conform::ensure_conformance(
            &store,
            &cache,
            &data_dir,
            &data_dir.join("wal"),
            HashSet::new(),
            "2GB",
        )
        .await
        .unwrap();

        std::fs::write(data_dir.join("CATALOG"), "someone-elses-catalog\n").unwrap();
        let summary = conform::ensure_conformance(
            &store,
            &cache,
            &data_dir,
            &data_dir.join("wal"),
            HashSet::new(),
            "2GB",
        )
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

    #[cfg(unix)]
    #[sqlx::test]
    async fn unconformed_boot_never_opens_the_catalog_marker(pool: sqlx::PgPool) {
        use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};

        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let fifo = data_dir.join("CATALOG");
        nix::unistd::mkfifo(
            &fifo,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        let store = CatalogStore::new(pool.clone());
        assert!(!store.is_conformed().await.unwrap(), "a fresh catalog");

        // The pass runs on a blocking thread, so a blocking open inside it
        // stalls that thread only; this task keeps driving the runtime, its
        // I/O and the timeout.
        let (pass_store, dir) = (store.clone(), data_dir.clone());
        let handle = tokio::runtime::Handle::current();
        let mut pass = tokio::task::spawn_blocking(move || {
            let cache = FieldCatalog::new();
            handle.block_on(conform::ensure_conformance(
                &pass_store,
                &cache,
                &dir,
                &dir.join("wal"),
                HashSet::new(),
                "2GB",
            ))
        });
        let Ok(joined) = tokio::time::timeout(std::time::Duration::from_secs(15), &mut pass).await
        else {
            // Release the blocked reader so the thread ends, then fail.
            // O_NONBLOCK: a writer open succeeds only while a reader waits,
            // so a pass stuck anywhere else cannot hang the release.
            let released = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(nix::libc::O_NONBLOCK)
                .open(&fifo)
                .map(drop);
            if released.is_ok() {
                let _ = pass.await;
            }
            panic!("boot read data/CATALOG before running the pass (release: {released:?})");
        };
        let summary = joined.unwrap().unwrap();
        assert!(summary.ran, "an unconformed catalog runs the pass");
        let file_type = std::fs::symlink_metadata(&fifo).unwrap().file_type();
        assert!(file_type.is_file() && !file_type.is_fifo(), "{file_type:?}");
        let marker = std::fs::read_to_string(&fifo).unwrap();
        assert_eq!(marker.trim(), store.catalog_id().await.unwrap());
    }

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
        conform::ensure_conformance(
            &store,
            &cache,
            &data_dir,
            &data_dir.join("wal"),
            HashSet::new(),
            "2GB",
        )
        .await
        .unwrap();
        let mtime = |p: &std::path::Path| std::fs::metadata(p).unwrap().modified().unwrap();
        let before = mtime(&file);

        std::fs::write(data_dir.join("CATALOG"), "someone-elses-catalog\n").unwrap();
        let summary = conform::ensure_conformance(
            &store,
            &cache,
            &data_dir,
            &data_dir.join("wal"),
            HashSet::new(),
            "2GB",
        )
        .await
        .unwrap();
        assert!(summary.ran, "identity mismatch re-arms the pass");
        assert_eq!(
            summary.rewritten, 0,
            "every _severity is inside 1-24: the guard is the identity"
        );
        assert_eq!(mtime(&file), before, "no file touched");
    }

    #[sqlx::test]
    async fn crash_before_marker_rerun_rewrites_nothing_and_republishes(pool: sqlx::PgPool) {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let (majority, minority) = plant_disagreeing_corpus(&data_dir);

        let store = CatalogStore::new(pool.clone());
        let cache = FieldCatalog::new();
        conform::ensure_conformance(
            &store,
            &cache,
            &data_dir,
            &data_dir.join("wal"),
            HashSet::new(),
            "2GB",
        )
        .await
        .unwrap();

        std::fs::remove_file(data_dir.join("CATALOG")).unwrap();
        let mtimes = |p: &std::path::Path| std::fs::metadata(p).unwrap().modified().unwrap();
        let (m1, m2) = (mtimes(&majority), mtimes(&minority));

        let summary = conform::ensure_conformance(
            &store,
            &cache,
            &data_dir,
            &data_dir.join("wal"),
            HashSet::new(),
            "2GB",
        )
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

    #[sqlx::test]
    async fn boot_pass_backfills_observations_for_a_pre_catalog_corpus(pool: sqlx::PgPool) {
        use trawl_server::store::FieldListFilter;

        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
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
        let summary = conform::ensure_conformance(
            &store,
            &cache,
            &data_dir,
            &data_dir.join("wal"),
            HashSet::new(),
            "2GB",
        )
        .await
        .expect("boot pass runs");
        assert!(summary.observed > 0, "the standing corpus must be observed");

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

        std::fs::remove_file(data_dir.join("CATALOG")).unwrap();
        let rerun = conform::ensure_conformance(
            &store,
            &cache,
            &data_dir,
            &data_dir.join("wal"),
            HashSet::new(),
            "2GB",
        )
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
        let error = conform::ensure_conformance(
            &store,
            &cache,
            &data_dir,
            &data_dir.join("wal"),
            HashSet::new(),
            "2GB",
        )
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
        let retry = conform::ensure_conformance(
            &store,
            &cache,
            &data_dir,
            &data_dir.join("wal"),
            HashSet::new(),
            "2GB",
        )
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

        let settled = conform::ensure_conformance(
            &store,
            &cache,
            &data_dir,
            &data_dir.join("wal"),
            HashSet::new(),
            "2GB",
        )
        .await
        .unwrap();
        assert!(
            !settled.ran,
            "completion and the matching marker skip the pass"
        );
    }

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
        conform::ensure_conformance(
            &store,
            &cache,
            &data_dir,
            &data_dir.join("wal"),
            HashSet::new(),
            "2GB",
        )
        .await
        .unwrap();
        store
            .touch_services("svc-a", &["duration".to_owned()], 900)
            .await
            .unwrap();

        std::fs::remove_file(data_dir.join("CATALOG")).unwrap();
        conform::ensure_conformance(
            &store,
            &cache,
            &data_dir,
            &data_dir.join("wal"),
            HashSet::new(),
            "2GB",
        )
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

    #[sqlx::test]
    async fn scheduled_dir_is_never_scanned(pool: sqlx::PgPool) {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        plant_disagreeing_corpus(&data_dir);
        let sched = plant(
            &data_dir,
            "scheduled/42/report.parquet",
            "SELECT 'x' AS sched_only_field, '1.5s' AS duration",
        );

        let store = CatalogStore::new(pool.clone());
        let cache = FieldCatalog::new();
        let before = std::fs::metadata(&sched).unwrap().modified().unwrap();
        conform::ensure_conformance(
            &store,
            &cache,
            &data_dir,
            &data_dir.join("wal"),
            HashSet::new(),
            "2GB",
        )
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

    #[sqlx::test]
    async fn foreign_layout_files_are_never_rewritten(pool: sqlx::PgPool) {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let (majority, minority) = plant_disagreeing_corpus(&data_dir);

        let foreign = plant(
            &data_dir,
            "operator-scratch/export.parquet",
            "SELECT 'x' AS operator_only_field, '1.5s' AS duration",
        );
        let bytes_before = std::fs::read(&foreign).unwrap();

        let store = CatalogStore::new(pool.clone());
        let cache = FieldCatalog::new();
        let summary = conform::ensure_conformance(
            &store,
            &cache,
            &data_dir,
            &data_dir.join("wal"),
            HashSet::new(),
            "2GB",
        )
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
        let summary = conform::ensure_conformance(
            &store,
            &cache,
            &data_dir,
            &data_dir.join("wal"),
            HashSet::new(),
            "2GB",
        )
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

        assert_eq!(std::fs::read(&junk).unwrap(), b"not a parquet file at all");
        assert_eq!(
            std::fs::read(&unreadable).unwrap(),
            b"PAR1\xff\xff\xff\xffPAR1"
        );

        assert!(
            !data_dir.join("CATALOG").exists(),
            "an unproven corpus must not publish the conformance identity"
        );
        let again = conform::ensure_conformance(
            &store,
            &cache,
            &data_dir,
            &data_dir.join("wal"),
            HashSet::new(),
            "2GB",
        )
        .await
        .unwrap();
        assert!(again.ran, "skipped files force a re-run on the next boot");
        assert_eq!(again.skipped, 2);
        assert_eq!(again.rewritten, 0, "the readable corpus already conforms");

        std::fs::remove_file(&junk).unwrap();
        std::fs::remove_file(&unreadable).unwrap();
        let clean = conform::ensure_conformance(
            &store,
            &cache,
            &data_dir,
            &data_dir.join("wal"),
            HashSet::new(),
            "2GB",
        )
        .await
        .unwrap();
        assert!(clean.ran);
        assert_eq!(clean.skipped, 0);
        let marker = std::fs::read_to_string(data_dir.join("CATALOG")).unwrap();
        assert_eq!(marker.trim(), store.catalog_id().await.unwrap());
    }

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
        let summary = conform::ensure_conformance(
            &store,
            &cache,
            &data_dir,
            &data_dir.join("wal"),
            HashSet::new(),
            "2GB",
        )
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

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        let clean = conform::ensure_conformance(
            &store,
            &cache,
            &data_dir,
            &data_dir.join("wal"),
            HashSet::new(),
            "2GB",
        )
        .await
        .unwrap();
        assert!(clean.ran, "the skipped directory forced a re-run");
        assert_eq!(clean.skipped, 0);
        assert!(data_dir.join("CATALOG").exists());
    }

    #[sqlx::test]
    async fn query_only_boot_refuses_an_archive_from_another_catalog(pool: sqlx::PgPool) {
        let store = CatalogStore::new(pool.clone());

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
        let summary = conform::ensure_conformance(
            &store,
            &cache,
            &skipped_data,
            &skipped_data.join("wal"),
            HashSet::new(),
            "2GB",
        )
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

    #[sqlx::test]
    async fn query_only_boot_accepts_its_own_and_empty_archives(pool: sqlx::PgPool) {
        let store = CatalogStore::new(pool.clone());
        let cache = FieldCatalog::new();

        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        plant_disagreeing_corpus(&data_dir);
        conform::ensure_conformance(
            &store,
            &cache,
            &data_dir,
            &data_dir.join("wal"),
            HashSet::new(),
            "2GB",
        )
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

    #[sqlx::test]
    async fn pending_publication_marker_keeps_the_pass_off_its_files(pool: sqlx::PgPool) {
        use trawl_server::ingest::publication_marker::{
            ValidatedMarker, identity_of, remove_marker_durably, write_marker,
        };

        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let wal_dir = tmp.path().join("wal");
        let (_, claimed) = plant_disagreeing_corpus(&data_dir);
        let staged = claimed.with_extension("parquet.tmp");
        std::fs::write(&staged, b"staged output of the pending publish").unwrap();
        let marker = ValidatedMarker::new(
            "prod",
            "svc-b",
            chrono::NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(),
            11,
            vec!["svc-b_1700000000000_ab12.ndjson".to_owned()],
            identity_of(&claimed).unwrap(),
        )
        .unwrap();
        std::fs::create_dir_all(marker.wal_env_dir(&wal_dir)).unwrap();
        write_marker(&wal_dir, &marker).unwrap();
        let canonical_before = std::fs::read(&claimed).unwrap();
        let staged_before = std::fs::read(&staged).unwrap();

        let store = CatalogStore::new(pool.clone());
        let cache = FieldCatalog::new();
        let held =
            conform::ensure_conformance(&store, &cache, &data_dir, &wal_dir, HashSet::new(), "2GB")
                .await
                .expect("a claimed file is skipped, not fatal");
        assert!(held.ran);
        assert_eq!(held.rewritten, 0, "the claimed canonical is not rewritten");
        assert_eq!(held.skipped, 1, "the claimed canonical counts as skipped");
        assert_eq!(std::fs::read(&claimed).unwrap(), canonical_before);
        assert_eq!(std::fs::read(&staged).unwrap(), staged_before);
        assert!(
            !store.is_conformed().await.unwrap() && !data_dir.join("CATALOG").exists(),
            "a skipped file must keep the pass armed"
        );

        remove_marker_durably(&marker.marker_path(&wal_dir)).unwrap();
        std::fs::remove_file(&staged).unwrap();
        let resolved =
            conform::ensure_conformance(&store, &cache, &data_dir, &wal_dir, HashSet::new(), "2GB")
                .await
                .unwrap();
        assert!(resolved.ran, "the incomplete pass re-runs");
        assert_eq!((resolved.rewritten, resolved.skipped), (1, 0));
        assert_eq!(column_type(&claimed, "duration"), "BIGINT");
        assert!(store.is_conformed().await.unwrap());
        assert_eq!(
            std::fs::read_to_string(data_dir.join("CATALOG"))
                .unwrap()
                .trim(),
            store.catalog_id().await.unwrap()
        );
    }

    #[sqlx::test]
    async fn rollup_unlink_sync_retry_keeps_the_pass_off_an_owed_day(pool: sqlx::PgPool) {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        plant(
            &data_dir,
            "prod/2026-08-02/10/svc-a.parquet",
            "SELECT TIMESTAMP '2026-08-02 10:00:00' AS \"_time\", 'svc-a' AS service, \
             x::BIGINT AS duration FROM (VALUES (410), (420), (430)) t(x)",
        );
        let owed = plant(
            &data_dir,
            "prod/2026-08-01/svc-b.parquet",
            "SELECT TIMESTAMP '2026-08-01 11:00:00' AS \"_time\", 'svc-b' AS service, \
             '1.5s' AS duration",
        );
        let free = plant(
            &data_dir,
            "prod/2026-08-03/11/svc-b.parquet",
            "SELECT TIMESTAMP '2026-08-03 11:00:00' AS \"_time\", 'svc-b' AS service, \
             '2.5s' AS duration",
        );
        assert!(!data_dir.join("prod/2026-08-01/.rollup-svc-b").exists());
        let owed_before = std::fs::read(&owed).unwrap();

        let store = CatalogStore::new(pool.clone());
        let cache = FieldCatalog::new();
        let wal_dir = tmp.path().join("wal");
        let held = conform::ensure_conformance(
            &store,
            &cache,
            &data_dir,
            &wal_dir,
            HashSet::from([data_dir.join("prod/2026-08-01")]),
            "2GB",
        )
        .await
        .expect("an owed day is skipped, not fatal");
        assert!(held.ran);
        assert_eq!((held.rewritten, held.skipped), (1, 1));
        assert_eq!(std::fs::read(&owed).unwrap(), owed_before);
        assert_eq!(column_type(&free, "duration"), "BIGINT");
        assert!(
            !store.is_conformed().await.unwrap() && !data_dir.join("CATALOG").exists(),
            "a deferred day must keep the pass armed"
        );

        let settled =
            conform::ensure_conformance(&store, &cache, &data_dir, &wal_dir, HashSet::new(), "2GB")
                .await
                .unwrap();
        assert!(settled.ran, "the incomplete pass re-runs");
        assert_eq!((settled.rewritten, settled.skipped), (1, 0));
        assert_eq!(column_type(&owed, "duration"), "BIGINT");
        assert!(store.is_conformed().await.unwrap());
    }

    #[sqlx::test]
    async fn unreadable_publication_markers_skip_every_file(pool: sqlx::PgPool) {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let (majority, minority) = plant_disagreeing_corpus(&data_dir);
        let before = [
            std::fs::read(&majority).unwrap(),
            std::fs::read(&minority).unwrap(),
        ];
        let wal_dir = tmp.path().join("wal");
        std::fs::write(&wal_dir, b"not a directory").unwrap();

        let store = CatalogStore::new(pool.clone());
        let cache = FieldCatalog::new();
        let summary =
            conform::ensure_conformance(&store, &cache, &data_dir, &wal_dir, HashSet::new(), "2GB")
                .await
                .expect("unreadable markers skip the corpus, they do not fail the boot");
        assert_eq!(
            (summary.scanned, summary.rewritten, summary.skipped),
            (0, 0, 2)
        );
        assert_eq!(std::fs::read(&majority).unwrap(), before[0]);
        assert_eq!(std::fs::read(&minority).unwrap(), before[1]);
        assert!(!store.is_conformed().await.unwrap());
        assert!(!data_dir.join("CATALOG").exists());
    }

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

        assert_eq!(
            server.state.query.field_catalog.get("duration"),
            Some(trawl_core::schema::CanonicalType::BigInt),
            "boot must seed and hydrate the pin cache"
        );

        assert_eq!(column_type(&minority, "duration"), "BIGINT");
        assert_eq!(column_type(&majority, "duration"), "BIGINT");

        let query =
            trawl_client::HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
        let result = query
            .query_paginated("service=svc-a OR service=svc-b", None, None)
            .await
            .expect("query across the corrected corpus must succeed");
        assert_eq!(result.result.row_count(), 4, "full corpus visible");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pinned_varchar_comparisons_behave_over_http() {
    let h = harness().await;

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

    assert_eq!(count("service=pin-svc status=200 last=1h").await, 1);
    assert_eq!(count("service=pin-svc status=200,301 last=1h").await, 2);
    assert_eq!(count("service=pin-svc status!=200 last=1h").await, 3);
    assert_eq!(
        count("service=pin-svc status>=400 last=1h").await,
        1,
        "only '404' is >= 400 — 'accepted' NULLs out instead of erroring"
    );

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

    // Under the VARCHAR pin: `read_json` types the column from the whole
    // batch, so the fractional sibling widens it to DOUBLE and `200` is
    // stored as the text "200.0".
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

#[tokio::test(flavor = "multi_thread")]
async fn sse_stream_applies_varchar_pin() {
    let h = harness().await;

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
