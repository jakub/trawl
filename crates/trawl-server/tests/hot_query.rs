// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Integration test for the full hot buffer pipeline.
//!
//! Verifies that events inserted into the hot buffer are immediately
//! visible to queries, and that compaction drains the buffer without
//! introducing duplicates.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value, json};

use trawl_server::bus::IngestBatch;
use trawl_server::deadline::Deadline;
use trawl_server::hot_buffer::{HotBuffer, HotBufferConfig};
use trawl_server::ingest::wal::WalWriter;
use trawl_server::pool::{ExecutorPool, WorkContext, WorkKind};

fn make_event(service: &str, message: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("_time".into(), json!("2026-02-15T12:00:00Z"));
    m.insert("_ingested".into(), json!("2026-02-15T12:00:01Z"));
    m.insert("service".into(), json!(service));
    m.insert(trawl_core::schema::SEVERITY.into(), json!(9));
    m.insert("message".into(), json!(message));
    m
}

fn events_to_ndjson(events: &[Map<String, Value>]) -> Vec<u8> {
    let mut buf = Vec::new();
    for e in events {
        serde_json::to_writer(&mut buf, e).unwrap();
        buf.push(b'\n');
    }
    buf
}

#[tokio::test]
async fn hot_buffer_makes_events_immediately_queryable() {
    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    let wal_dir = tmp.path().join("wal");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&wal_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    // --- set up infrastructure ---

    let hot_buffer = Arc::new(HotBuffer::new(HotBufferConfig {
        max_events: 10_000,
        max_bytes: 10_000_000,
    }));

    let pool = ExecutorPool::new(
        data_dir.to_str().unwrap().to_owned(),
        1,    // single executor for test
        1000, // max result rows
        Some(Arc::clone(&hot_buffer)),
    );

    // --- ingest events ---

    let events: Vec<Map<String, Value>> = vec![
        make_event("nginx", "request handled ok"),
        make_event("nginx", "upstream timeout error"),
        make_event("postgres", "checkpoint complete"),
    ];

    // Write to WAL.
    let wal_writer = WalWriter::new(wal_dir.clone());
    wal_writer.ensure_dir().unwrap();
    let ndjson = events_to_ndjson(&events);
    let wal_path = wal_writer.write("prod", "nginx", &ndjson).unwrap();

    // Insert directly into hot buffer (simulating what the ingest handler does).
    // `{env}/{stem}` — must match what compaction derives from the env dir.
    let batch_id: Arc<str> =
        format!("prod/{}", wal_path.file_stem().unwrap().to_str().unwrap()).into();
    let ndjson_bytes = ndjson.len();
    let batch = Arc::new(IngestBatch {
        batch_id,
        service: "nginx".into(),
        byte_size: ndjson_bytes,
        events: events.clone(),
    });
    hot_buffer.insert(batch);

    // --- query BEFORE compaction → events should be visible from hot buffer ---

    let result = pool
        .execute(
            pool.allocate_query_id(),
            "* | head 100",
            Deadline::after(Duration::from_secs(10)),
            false,
            0,
            WorkContext::system(WorkKind::Query),
        )
        .await;

    let query_result = result.result.expect("query should succeed");
    assert_eq!(
        query_result.rows.len(),
        3,
        "expected 3 rows from hot buffer, got {}",
        query_result.rows.len()
    );

    // --- compact WAL → parquet, drain hot buffer ---

    trawl_server::ingest::compaction::compact_once(
        &wal_dir,
        &data_dir,
        Duration::ZERO, // no min age — compact immediately
        false,          // no daily rollup
        Some(&hot_buffer),
        500,
        "2GB",
        None,
    )
    .await
    .expect("compaction should succeed");

    assert_eq!(
        hot_buffer.event_count(),
        0,
        "hot buffer should be empty after compaction"
    );

    assert!(
        !wal_path.exists(),
        "WAL file should be deleted after compaction"
    );

    // --- query AFTER compaction → same events now from parquet ---

    let result_after = pool
        .execute(
            pool.allocate_query_id(),
            "* | head 100",
            Deadline::after(Duration::from_secs(10)),
            false,
            0,
            WorkContext::system(WorkKind::Query),
        )
        .await;

    let query_result_after = result_after
        .result
        .expect("post-compaction query should succeed");
    assert_eq!(
        query_result_after.rows.len(),
        3,
        "expected 3 rows from parquet after compaction, got {}",
        query_result_after.rows.len()
    );
}

#[tokio::test]
async fn hot_buffer_and_parquet_produce_no_duplicates() {
    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    let wal_dir = tmp.path().join("wal");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&wal_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    let hot_buffer = Arc::new(HotBuffer::new(HotBufferConfig {
        max_events: 10_000,
        max_bytes: 10_000_000,
    }));

    let pool = ExecutorPool::new(
        data_dir.to_str().unwrap().to_owned(),
        1,
        1000,
        Some(Arc::clone(&hot_buffer)),
    );

    // --- first batch: ingest + compact ---

    let batch1_events = vec![
        make_event("nginx", "batch one alpha"),
        make_event("nginx", "batch one beta"),
    ];

    let wal_writer = WalWriter::new(wal_dir.clone());
    wal_writer.ensure_dir().unwrap();
    let ndjson1 = events_to_ndjson(&batch1_events);
    let wal1 = wal_writer.write("prod", "nginx", &ndjson1).unwrap();

    let batch1 = Arc::new(IngestBatch {
        batch_id: format!("prod/{}", wal1.file_stem().unwrap().to_str().unwrap()).into(),
        service: "nginx".into(),
        byte_size: ndjson1.len(),
        events: batch1_events,
    });
    hot_buffer.insert(batch1);

    // Compact batch 1 → parquet.
    trawl_server::ingest::compaction::compact_once(
        &wal_dir,
        &data_dir,
        Duration::ZERO,
        false,
        Some(&hot_buffer),
        500,
        "2GB",
        None,
    )
    .await
    .expect("compaction 1 should succeed");

    // --- second batch: ingest but DON'T compact (stays in hot buffer) ---

    let batch2_events = vec![make_event("nginx", "batch two gamma")];
    let ndjson2 = events_to_ndjson(&batch2_events);
    let _wal2 = wal_writer.write("prod", "nginx", &ndjson2).unwrap();

    let batch2 = Arc::new(IngestBatch {
        batch_id: "batch2_manual".into(),
        service: "nginx".into(),
        byte_size: ndjson2.len(),
        events: batch2_events,
    });
    hot_buffer.insert(batch2);

    // --- query → should see both batches without duplicates ---

    let result = pool
        .execute(
            pool.allocate_query_id(),
            "* | head 100",
            Deadline::after(Duration::from_secs(10)),
            false,
            0,
            WorkContext::system(WorkKind::Query),
        )
        .await;

    let query_result = result.result.expect("mixed query should succeed");
    assert_eq!(
        query_result.rows.len(),
        3,
        "expected 3 total rows (2 parquet + 1 hot buffer), got {}",
        query_result.rows.len()
    );
}

/// Hot/cold agreement through the real pin plumbing: a compaction tick
/// with the catalog wired seeds the pin, a later conflicting event sits in
/// a `HotBuffer` sharing the same `FieldCatalog`, and the query returns
/// every cold row with the hot value nulled, so the pins travel buffer to
/// snapshot to pool to emitter without a test-side shortcut.
#[sqlx::test]
async fn hot_conflict_after_pin_seeding_keeps_all_cold_rows(pool: sqlx::PgPool) {
    use trawl_server::catalog::{CatalogContext, FieldCatalog};
    use trawl_server::store::CatalogStore;

    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    let wal_dir = tmp.path().join("wal");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&wal_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    let catalog = Arc::new(FieldCatalog::new());
    let ctx = CatalogContext {
        store: CatalogStore::new(pool),
        cache: Arc::clone(&catalog),
    };
    let hot_buffer = Arc::new(
        HotBuffer::new(HotBufferConfig {
            max_events: 10_000,
            max_bytes: 10_000_000,
        })
        .with_field_catalog(Arc::clone(&catalog)),
    );
    let exec_pool = ExecutorPool::new(
        data_dir.to_str().unwrap().to_owned(),
        1,
        1000,
        Some(Arc::clone(&hot_buffer)),
    );

    // Cold batch: integer durations, compacted with the catalog wired —
    // the tick seeds the BIGINT pin.
    let cold: Vec<Map<String, Value>> = (0..2)
        .map(|i| {
            let mut m = make_event("nginx", &format!("cold{i}"));
            m.insert("duration".into(), json!(4200 + i));
            m
        })
        .collect();
    let wal_writer = WalWriter::new(wal_dir.clone());
    wal_writer.ensure_dir().unwrap();
    let ndjson = events_to_ndjson(&cold);
    wal_writer.write("prod", "nginx", &ndjson).unwrap();
    trawl_server::ingest::compaction::compact_once(
        &wal_dir,
        &data_dir,
        Duration::ZERO,
        false,
        Some(&hot_buffer),
        500,
        "2GB",
        Some(&ctx),
    )
    .await
    .expect("compaction should succeed");
    assert_eq!(
        catalog.get("duration"),
        Some(trawl_core::schema::CanonicalType::BigInt),
        "the compaction tick must seed the pin"
    );

    // Conflicting hot event — never compacted.
    let mut hot = make_event("nginx", "hot-conflict");
    hot.insert("duration".into(), json!("n/a"));
    hot_buffer.insert(Arc::new(IngestBatch {
        batch_id: "hot_manual".into(),
        service: "nginx".into(),
        byte_size: 64,
        events: vec![hot],
    }));

    let result = exec_pool
        .execute(
            exec_pool.allocate_query_id(),
            "* | head 100",
            Deadline::after(Duration::from_secs(10)),
            false,
            0,
            WorkContext::system(WorkKind::Query),
        )
        .await;
    let query_result = result.result.expect("pinned hot conflict must not error");
    assert_eq!(
        query_result.rows.len(),
        3,
        "ALL cold rows plus the hot row must be present — never hot-only"
    );
    let dur = query_result
        .columns
        .iter()
        .position(|c| c.name == "duration")
        .expect("duration column present");
    let ints = query_result
        .rows
        .iter()
        .filter(|r| matches!(r[dur], trawl_api::value::Value::Integer(_)))
        .count();
    let nulls = query_result
        .rows
        .iter()
        .filter(|r| matches!(r[dur], trawl_api::value::Value::Null))
        .count();
    assert_eq!(
        (ints, nulls),
        (2, 1),
        "cold values stay integers; the hot conflict degrades to NULL"
    );
}

// NOTE: there is deliberately no hot-buffer case-variant test here. Every
// producer enters `envelope::canonicalize`, the one door that ASCII-folds
// field names, so a hot buffer carrying a mixed-case spelling of a pinned
// field cannot be produced by the wired system. The end-to-end proofs are
// `case_variant_field_names_fold_to_one_column_across_services` in
// tests/field_catalog.rs and `syslog_mixed_case_sd_param_lands_folded_and_
// pins_folded` below.

/// The syslog producer routes through `envelope::canonicalize`, so it
/// inherits that door's fold instead of carrying a listener-local one at
/// SD-key construction (ADR-0013). The whole journey: a mixed-case RFC
/// 5424 SD param reaches the hot buffer folded, and compaction pins it
/// under the folded name.
#[sqlx::test]
async fn syslog_mixed_case_sd_param_lands_folded_and_pins_folded(pool: sqlx::PgPool) {
    use indexmap::IndexMap;
    use trawl_server::catalog::{CatalogContext, FieldCatalog};
    use trawl_server::ingest::pipeline::{PipelineWriter, ServiceBatch};
    use trawl_server::ingest::producer::Derivation;
    use trawl_server::store::CatalogStore;
    use trawl_server::syslog::convert::SyslogDoor;

    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    let wal_dir = tmp.path().join("wal");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let catalog = Arc::new(FieldCatalog::new());
    let hot_buffer = Arc::new(
        HotBuffer::new(HotBufferConfig {
            max_events: 10_000,
            max_bytes: 10_000_000,
        })
        .with_field_catalog(Arc::clone(&catalog)),
    );
    let wal_writer = Arc::new(WalWriter::new(wal_dir.clone()));
    wal_writer.ensure_dir().unwrap();
    let pipeline =
        PipelineWriter::new(Arc::clone(&wal_writer), Some(Arc::clone(&hot_buffer)), None);

    let door = SyslogDoor {
        envs: vec!["prod".to_owned()].into(),
        default_env: "prod".into(),
        trusted_relays: Vec::new().into(),
        derivation: Arc::new(Derivation::defaults()),
    };
    let raw = r#"<165>1 2026-02-15T12:00:00Z web01 app 1234 ID47 [exampleSDID@32473 eventID="1011"] boom"#;
    let event = door
        .admit(
            raw,
            "10.0.0.1".parse().unwrap(),
            &std::collections::HashMap::new(),
            "syslog",
            "udp",
        )
        .expect("the syslog profile must admit a well-formed frame");
    assert_eq!(event.map["sd_examplesdid@32473_eventid"], "1011");
    assert!(
        event
            .map
            .keys()
            .all(|k| !k.bytes().any(|b| b.is_ascii_uppercase())),
        "the syslog producer must emit folded keys only: {:?}",
        event.map.keys().collect::<Vec<_>>()
    );

    let mut batch = ServiceBatch::default();
    batch.push(event.map);
    let mut batches = IndexMap::new();
    batches.insert((event.env, event.service), batch);
    assert_eq!(
        tokio::task::spawn_blocking(move || pipeline.write(batches))
            .await
            .unwrap(),
        1
    );

    // The hot snapshot carries the folded key and no unfolded spelling.
    let snap = hot_buffer
        .snapshot()
        .expect("event must be in the hot buffer");
    let content = std::fs::read_to_string(snap.path()).unwrap();
    assert!(
        content.contains("sd_examplesdid@32473_eventid"),
        "hot snapshot must carry the folded key: {content}"
    );
    assert!(
        !content.contains("\"sd_exampleSDID@32473_eventID\""),
        "no unfolded KEY may reach the hot buffer (the wire spelling still \
         lives in _raw's value, correctly): {content}"
    );

    // Compaction pins under the folded name, never the wire spelling.
    let ctx = CatalogContext {
        store: CatalogStore::new(pool),
        cache: Arc::clone(&catalog),
    };
    trawl_server::ingest::compaction::compact_once(
        &wal_dir,
        &data_dir,
        Duration::ZERO,
        false,
        Some(&hot_buffer),
        500,
        "2GB",
        Some(&ctx),
    )
    .await
    .expect("compaction should succeed");
    assert!(
        catalog.get("sd_examplesdid@32473_eventid").is_some(),
        "the folded name must be pinned"
    );
    assert!(
        catalog.get("sd_exampleSDID@32473_eventID").is_none(),
        "the wire spelling must not exist as a pin"
    );
}

#[tokio::test]
async fn query_works_without_hot_buffer() {
    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    let wal_dir = tmp.path().join("wal");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&wal_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    // Pool without hot buffer.
    let pool = ExecutorPool::new(data_dir.to_str().unwrap().to_owned(), 1, 1000, None);

    // Write WAL and compact directly (no bus, no hot buffer).
    let events = vec![make_event("nginx", "standalone event")];
    let wal_writer = WalWriter::new(wal_dir.clone());
    wal_writer.ensure_dir().unwrap();
    let ndjson = events_to_ndjson(&events);
    let _wal = wal_writer.write("prod", "nginx", &ndjson).unwrap();

    trawl_server::ingest::compaction::compact_once(
        &wal_dir,
        &data_dir,
        Duration::ZERO,
        false,
        None,
        500,
        "2GB",
        None,
    )
    .await
    .expect("compaction without hot buffer should succeed");

    let result = pool
        .execute(
            pool.allocate_query_id(),
            "* | head 100",
            Deadline::after(Duration::from_secs(10)),
            false,
            0,
            WorkContext::system(WorkKind::Query),
        )
        .await;

    let query_result = result
        .result
        .expect("query without hot buffer should succeed");
    assert_eq!(
        query_result.rows.len(),
        1,
        "expected 1 row from parquet, got {}",
        query_result.rows.len()
    );
}

#[tokio::test]
async fn service_scoped_query_without_hot_buffer_survives_sibling_service_hours() {
    // End to end through the real plumbing (compute_source, pool,
    // executor): one service's data in one hour partition, sibling hour
    // directories owned by another service, and no hot buffer to paper over
    // it. DuckDB rejects a list source wholesale when a single element
    // matches nothing, so an hour directory in range that holds no nginx
    // file must be resolved away before the read.
    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    let wal_dir = tmp.path().join("wal");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&wal_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    let pool = ExecutorPool::new(data_dir.to_str().unwrap().to_owned(), 1, 1000, None);

    // One nginx event, timestamped now so it lands in the current hour
    // partition and inside a `last=6h` window.
    let now = chrono::Utc::now();
    let stamp = now.to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
    let mut event = make_event("nginx", "sibling-hour survivor");
    event.insert("_time".into(), json!(stamp));
    event.insert("_ingested".into(), json!(stamp));

    let wal_writer = WalWriter::new(wal_dir.clone());
    wal_writer.ensure_dir().unwrap();
    let ndjson = events_to_ndjson(std::slice::from_ref(&event));
    let _wal = wal_writer.write("prod", "nginx", &ndjson).unwrap();

    trawl_server::ingest::compaction::compact_once(
        &wal_dir,
        &data_dir,
        Duration::ZERO,
        false,
        None,
        500,
        "2GB",
        None,
    )
    .await
    .expect("compaction should succeed");

    // The other service owns the preceding hours: their directories exist and
    // hold a file, but never nginx's.
    for back in 1..=5_i64 {
        let dt = now - chrono::Duration::hours(back);
        let hour_dir = data_dir
            .join("prod")
            .join(dt.format("%Y-%m-%d").to_string())
            .join(dt.format("%H").to_string());
        std::fs::create_dir_all(&hour_dir).unwrap();
        // Never read: `service=nginx` pins the filename, so this file only
        // has to exist for the hour directory to look occupied.
        std::fs::write(hour_dir.join("postgres.parquet"), b"other service").unwrap();
    }

    let result = pool
        .execute(
            pool.allocate_query_id(),
            "service=nginx last=6h",
            Deadline::after(Duration::from_secs(10)),
            false,
            0,
            WorkContext::system(WorkKind::Query),
        )
        .await;

    let query_result = result
        .result
        .expect("a service-scoped query must not fail over sibling-service hours");
    assert_eq!(
        query_result.rows.len(),
        1,
        "nginx's compacted row must come back even though five hour \
         directories in range hold no nginx file, got {} rows",
        query_result.rows.len()
    );
}

/// Pinned `| where`/`| let` end to end through the real query plumbing
/// (catalog, pool, emitter): over the hot buffer, then over parquet after
/// compaction, plus the SSE plan lane over the same events with the single
/// snapshot the handler feeds it. The scope shapes ride along: a rename
/// remaps the pin, a bare-alias let copies it, a computed let kills it.
#[tokio::test]
#[allow(clippy::too_many_lines)] // one linear end-to-end narrative
async fn pinned_where_let_hot_cold_and_stream_agree() {
    use trawl_core::filter::CompiledFilter;
    use trawl_core::pin_scope::PinScope;
    use trawl_core::stream::{StreamPlan, compile_stream_plan};
    use trawl_server::catalog::FieldCatalog;

    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    let wal_dir = tmp.path().join("wal");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&wal_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    // A VARCHAR pin, seeded straight into the in-process cache the pool
    // and the stream both read.
    let catalog = Arc::new(FieldCatalog::new());
    catalog.merge([(
        "status".to_string(),
        trawl_core::schema::CanonicalType::Varchar,
    )]);

    let hot_buffer = Arc::new(
        HotBuffer::new(HotBufferConfig {
            max_events: 10_000,
            max_bytes: 10_000_000,
        })
        .with_field_catalog(Arc::clone(&catalog)),
    );
    let pool = ExecutorPool::new(
        data_dir.to_str().unwrap().to_owned(),
        1,
        1000,
        Some(Arc::clone(&hot_buffer)),
    )
    .with_field_catalog(Arc::clone(&catalog));

    // Three events: numeric-text, below-threshold, and no-reading.
    let events: Vec<Map<String, Value>> = [("404", "a"), ("200", "b"), ("accepted", "c")]
        .into_iter()
        .map(|(status, msg)| {
            let mut m = make_event("nginx", msg);
            m.insert("status".into(), json!(status));
            m
        })
        .collect();

    let wal_writer = WalWriter::new(wal_dir.clone());
    wal_writer.ensure_dir().unwrap();
    let ndjson = events_to_ndjson(&events);
    let wal_path = wal_writer.write("prod", "nginx", &ndjson).unwrap();
    hot_buffer.insert(Arc::new(IngestBatch {
        batch_id: format!("prod/{}", wal_path.file_stem().unwrap().to_str().unwrap()).into(),
        service: "nginx".into(),
        byte_size: ndjson.len(),
        events: events.clone(),
    }));

    let run = |dsl: &'static str| {
        let pool = &pool;
        async move {
            pool.execute(
                pool.allocate_query_id(),
                dsl,
                Deadline::after(Duration::from_secs(10)),
                false,
                0,
                WorkContext::system(WorkKind::Query),
            )
            .await
            .result
            .unwrap_or_else(|e| panic!("{dsl:?} must succeed: {e}"))
            .rows
            .len()
        }
    };

    // (dsl, expected rows) — the SSE plan below must agree on each.
    let cases: [(&'static str, usize); 4] = [
        // '404' has the only reading above 400; 'accepted' is UNKNOWN.
        ("* | where status > 400", 1),
        // NOT does not invert UNKNOWN: only '200' answers FALSE→TRUE.
        ("* | where not (status > 400)", 1),
        // rename remaps the pin to the new name.
        ("* | rename status as st | where st > 400", 1),
        // a bare-alias let copies the pin — a broken walk would emit the
        // literal-driven comparison here, which throws over 'accepted'.
        ("* | let s2 = status | where s2 > 400", 1),
    ];

    // Hot lane (pre-compaction).
    for (dsl, expected) in cases {
        assert_eq!(run(dsl).await, expected, "hot: {dsl}");
    }

    // The SSE plan lane: one snapshot feeds filter + plan, exactly as
    // stream_query wires it.
    let stream_matches = |dsl: &str| -> usize {
        let ast = trawl_core::parser::parse(dsl).expect("parses");
        let pins = catalog.all();
        let filter = CompiledFilter::compile(&ast.search, &pins).expect("filter compiles");
        let plan =
            compile_stream_plan(&ast.pipeline, &PinScope::root(&pins)).expect("plan compiles");
        let StreamPlan::PassThrough(mut stages) = plan else {
            panic!("pass-through pipelines only in this test");
        };
        events
            .iter()
            .filter(|event| {
                // The handler's own shape: one instant per event, one
                // door owning both the filter and the stages (ADR-0017
                // §3). A test that sampled two clocks would stop
                // mirroring the lane it exists to mirror.
                let ctx = trawl_core::context::EvalContext::capture();
                matches!(
                    trawl_core::stream::accept_event(&filter, &mut stages, event, &ctx),
                    trawl_core::stream::LiveOutcome::Emit(_)
                )
            })
            .count()
    };
    for (dsl, expected) in cases {
        assert_eq!(stream_matches(dsl), expected, "stream: {dsl}");
    }
    // The computed-let kill, observable stream-side without an error
    // channel: the derived value is literal-driven, so nothing matches.
    assert_eq!(
        stream_matches("* | let status = lower(status) | where status > 400"),
        0,
        "a computed let kills the pin"
    );

    // Cold lane (post-compaction): same answers off parquet.
    trawl_server::ingest::compaction::compact_once(
        &wal_dir,
        &data_dir,
        Duration::ZERO,
        false,
        Some(&hot_buffer),
        500,
        "2GB",
        None,
    )
    .await
    .expect("compaction should succeed");
    assert_eq!(hot_buffer.event_count(), 0);
    for (dsl, expected) in cases {
        assert_eq!(run(dsl).await, expected, "cold: {dsl}");
    }
}

/// The SEVERITY pin answers identically hot and cold, and in the stream
/// lane, over the whole token vocabulary (ADR-0013 §6).
///
/// The vocabulary is the point: a band token, an `OTel` exact short name,
/// an integer, an ordered comparison, an IN list and a glob over the
/// canonical text all bind through one rule table, so a divergence here
/// is a missed lane rather than a missed case.
#[tokio::test]
#[allow(clippy::too_many_lines)] // one linear end-to-end narrative
async fn severity_pin_agrees_hot_cold_and_stream() {
    use trawl_core::filter::CompiledFilter;
    use trawl_server::catalog::FieldCatalog;

    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    let wal_dir = tmp.path().join("wal");
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&wal_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    // The declared seed pins `_severity` SEVERITY on day one; here it is
    // seeded straight into the in-process cache the pool and the stream
    // both read.
    let catalog = Arc::new(FieldCatalog::new());
    catalog.merge([(
        trawl_core::schema::SEVERITY.to_string(),
        trawl_core::schema::CanonicalType::Severity,
    )]);

    let hot_buffer = Arc::new(
        HotBuffer::new(HotBufferConfig {
            max_events: 10_000,
            max_bytes: 10_000_000,
        })
        .with_field_catalog(Arc::clone(&catalog)),
    );
    let pool = ExecutorPool::new(
        data_dir.to_str().unwrap().to_owned(),
        1,
        1000,
        Some(Arc::clone(&hot_buffer)),
    )
    .with_field_catalog(Arc::clone(&catalog));

    // One row per band edge, plus the game-server row that derives none.
    let events: Vec<Map<String, Value>> = [
        (Some(17), "error-lo"),
        (Some(20), "error-hi"),
        (Some(18), "error2"),
        (Some(13), "warn"),
        (Some(9), "info"),
        (None, "gold"),
    ]
    .into_iter()
    .map(|(sev, msg)| {
        let mut m = make_event("nginx", msg);
        match sev {
            Some(n) => m.insert(trawl_core::schema::SEVERITY.into(), json!(n)),
            None => m.remove(trawl_core::schema::SEVERITY),
        };
        m.insert("level".into(), json!(msg));
        m
    })
    .collect();

    let wal_writer = WalWriter::new(wal_dir.clone());
    wal_writer.ensure_dir().unwrap();
    let ndjson = events_to_ndjson(&events);
    let wal_path = wal_writer.write("prod", "nginx", &ndjson).unwrap();
    hot_buffer.insert(Arc::new(IngestBatch {
        batch_id: format!("prod/{}", wal_path.file_stem().unwrap().to_str().unwrap()).into(),
        service: "nginx".into(),
        byte_size: ndjson.len(),
        events: events.clone(),
    }));

    let run = |dsl: &'static str| {
        let pool = &pool;
        async move {
            pool.execute(
                pool.allocate_query_id(),
                dsl,
                Deadline::after(Duration::from_secs(10)),
                false,
                0,
                WorkContext::system(WorkKind::Query),
            )
            .await
            .result
            .unwrap_or_else(|e| panic!("{dsl:?} must succeed: {e}"))
            .rows
            .len()
        }
    };

    // (dsl, expected rows)
    let cases: [(&'static str, usize); 8] = [
        ("_severity=error", 3),
        ("_severity=error2", 1),
        ("_severity=17", 1),
        ("_severity>=warn", 4),
        ("_severity=warn,error", 4),
        ("_severity=warn*", 1),
        // `!=` widens with NULL in the search stage, so the row with no
        // derived severity matches.
        ("_severity!=error", 3),
        // `level` is ordinary sender vocabulary, not a severity spelling.
        ("level=gold", 1),
    ];

    for (dsl, expected) in cases {
        assert_eq!(run(dsl).await, expected, "hot: {dsl}");
    }

    // The live lane, over the same single snapshot the handler feeds it.
    let stream_matches = |dsl: &str| -> usize {
        let ast = trawl_core::parser::parse(dsl).expect("parses");
        let pins = catalog.all();
        let filter = CompiledFilter::compile(&ast.search, &pins).expect("filter compiles");
        events
            .iter()
            .filter(|event| filter.matches_at(event, &trawl_core::context::EvalContext::capture()))
            .count()
    };
    for (dsl, expected) in cases {
        assert_eq!(stream_matches(dsl), expected, "stream: {dsl}");
    }

    // Cold lane: same answers off parquet, after conformance wrote the
    // column as the physical BIGINT the pin names.
    trawl_server::ingest::compaction::compact_once(
        &wal_dir,
        &data_dir,
        Duration::ZERO,
        false,
        Some(&hot_buffer),
        500,
        "2GB",
        None,
    )
    .await
    .expect("compaction should succeed");
    assert_eq!(hot_buffer.event_count(), 0);
    for (dsl, expected) in cases {
        assert_eq!(run(dsl).await, expected, "cold: {dsl}");
    }
}
