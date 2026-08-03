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
use trawl_server::hot_buffer::{HotBuffer, HotBufferConfig};
use trawl_server::ingest::wal::WalWriter;
use trawl_server::pool::ExecutorPool;

/// Build a JSON event map with the given fields.
fn make_event(service: &str, message: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("_time".into(), json!("2026-02-15T12:00:00Z"));
    m.insert("_ingested".into(), json!("2026-02-15T12:00:01Z"));
    m.insert("service".into(), json!(service));
    m.insert("severity".into(), json!(9));
    m.insert("severity_text".into(), json!("info"));
    m.insert("message".into(), json!(message));
    m
}

/// Serialize events to ndjson bytes (for WAL writes).
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

    // Create executor pool pointing at our temp data dir.
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
            Duration::from_secs(10),
            false,
            0,
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

    // Verify hot buffer was drained.
    assert_eq!(
        hot_buffer.event_count(),
        0,
        "hot buffer should be empty after compaction"
    );

    // Verify WAL file was deleted.
    assert!(
        !wal_path.exists(),
        "WAL file should be deleted after compaction"
    );

    // --- query AFTER compaction → same events now from parquet ---

    let result_after = pool
        .execute(
            pool.allocate_query_id(),
            "* | head 100",
            Duration::from_secs(10),
            false,
            0,
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
            Duration::from_secs(10),
            false,
            0,
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

#[tokio::test]
async fn query_works_without_hot_buffer() {
    // Verify the query path still works when no hot buffer is configured.
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
            Duration::from_secs(10),
            false,
            0,
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
