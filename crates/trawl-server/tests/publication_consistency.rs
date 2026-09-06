// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Real WAL, hot buffer and `DuckDB` reads across a paused publication.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use trawl_server::bus::IngestBatch;
use trawl_server::error::ServerError;
use trawl_server::hot_buffer::{HotBuffer, HotBufferConfig};
use trawl_server::ingest::{compaction::compact_once, wal::WalWriter};
use trawl_server::pool::ExecutorPool;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)] // one controlled publication, checked through both readers
async fn queries_and_exports_wait_for_publish_and_drain() {
    let root = tempfile::tempdir().unwrap();
    let wal = root.path().join("wal");
    let data = root.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let hot = Arc::new(HotBuffer::new(HotBufferConfig {
        max_events: 100,
        max_bytes: 100_000,
    }));
    let pool = ExecutorPool::new(data.to_str().unwrap().into(), 3, 100, Some(hot.clone()));
    // Identical payloads are distinct accepted events. DISTINCT would fail.
    let event = json!({"_time":"2026-02-15T12:00:00Z",
        "_ingested":"2026-02-15T12:00:01Z", "service":"nginx",
        "_severity":9, "message":"same payload"})
    .as_object()
    .unwrap()
    .clone();
    let events = vec![event; 3];
    let mut ndjson = Vec::new();
    for event in &events {
        serde_json::to_writer(&mut ndjson, event).unwrap();
        ndjson.push(b'\n');
    }
    let writer = WalWriter::new(wal.clone());
    writer.ensure_dir().unwrap();
    let path = writer.write("prod", "nginx", &ndjson).unwrap();
    hot.insert(Arc::new(IngestBatch {
        batch_id: format!("prod/{}", path.file_stem().unwrap().to_str().unwrap()).into(),
        service: "nginx".into(),
        byte_size: ndjson.len(),
        events,
    }));
    let before = pool
        .execute(
            pool.allocate_query_id(),
            "*",
            Duration::from_secs(10),
            false,
            0,
        )
        .await;
    assert_eq!(before.result.unwrap().rows.len(), 3);

    let (entered, release) = hot.publication().pause_next_publication_for_test();
    let compact_hot = hot.clone();
    let compact = tokio::spawn(async move {
        compact_once(
            &wal,
            &data,
            Duration::ZERO,
            false,
            Some(&compact_hot),
            500,
            "2GB",
            None,
        )
        .await
    });
    tokio::task::spawn_blocking(move || entered.recv_timeout(Duration::from_secs(10)).unwrap())
        .await
        .unwrap();
    assert_eq!(hot.event_count(), 3, "pause must precede hot drain");

    let blocked = pool
        .execute(
            pool.allocate_query_id(),
            "*",
            Duration::from_millis(30),
            false,
            0,
        )
        .await;
    assert!(matches!(blocked.result, Err(ServerError::Timeout)));
    let blocked_export = pool
        .export_parquet(
            pool.allocate_query_id(),
            "*",
            100,
            Duration::from_millis(30),
        )
        .await;
    assert!(matches!(blocked_export, Err(ServerError::Timeout)));
    assert_eq!(
        pool.available_permits(),
        3,
        "waiting readers return their permits"
    );

    release.send(()).unwrap();
    assert_eq!(compact.await.unwrap().unwrap(), 0);
    assert_eq!(hot.event_count(), 0);
    let after = pool
        .execute(
            pool.allocate_query_id(),
            "*",
            Duration::from_secs(10),
            false,
            0,
        )
        .await;
    assert_eq!(after.result.unwrap().rows.len(), 3);
    let export = pool
        .export_parquet(pool.allocate_query_id(), "*", 100, Duration::from_secs(10))
        .await
        .unwrap();
    let exported = root.path().join("export.parquet");
    std::fs::write(&exported, export).unwrap();
    let conn = duckdb::Connection::open_in_memory().unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT count(*) FROM read_parquet(?)",
            [exported.to_str().unwrap()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 3, "export preserves accepted identical events");
}

#[tokio::test]
async fn query_only_pool_refuses_incomplete_rollup_after_restart() {
    let root = tempfile::tempdir().unwrap();
    let day = root.path().join("prod/2026-02-15");
    std::fs::create_dir_all(&day).unwrap();
    let marker = day.join(".rollup-nginx");
    std::fs::write(&marker, "unfinished").unwrap();
    let pool = ExecutorPool::new(root.path().to_str().unwrap().into(), 1, 100, None);
    let result = pool
        .execute(
            pool.allocate_query_id(),
            "*",
            Duration::from_secs(1),
            false,
            0,
        )
        .await;
    assert!(matches!(
        result.result,
        Err(ServerError::ServiceUnavailable(_))
    ));
    let export = pool
        .export_parquet(pool.allocate_query_id(), "*", 100, Duration::from_secs(1))
        .await;
    assert!(matches!(export, Err(ServerError::ServiceUnavailable(_))));
    assert!(matches!(
        pool.sample_field_values("message", None, 10).await,
        Err(ServerError::ServiceUnavailable(_))
    ));
    assert_eq!(pool.available_permits(), 1);
    std::fs::remove_file(marker).unwrap();
    assert!(pool.publication().read().await.is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_timeout_keeps_publication_guard_until_duckdb_task_finishes() {
    use std::sync::atomic::Ordering;
    use trawl_server::pool::TEST_QUERY_DELAY_MS;
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_QUERY_DELAY_MS.store(0, Ordering::Relaxed);
        }
    }
    let _reset = Reset;
    let root = tempfile::tempdir().unwrap();
    let pool = ExecutorPool::new(root.path().to_str().unwrap().into(), 1, 100, None);
    TEST_QUERY_DELAY_MS.store(500, Ordering::Relaxed);
    let result = pool
        .execute(
            pool.allocate_query_id(),
            "*",
            Duration::from_millis(30),
            false,
            0,
        )
        .await;
    assert!(matches!(result.result, Err(ServerError::Timeout)));
    let publication = pool.publication();
    assert!(
        tokio::time::timeout(Duration::from_millis(30), publication.write())
            .await
            .is_err(),
        "async timeout must not let a writer change files still used by the blocking reader"
    );
    tokio::time::timeout(Duration::from_secs(5), publication.write())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aborting_request_keeps_running_reader_protected() {
    use std::sync::atomic::Ordering;
    use trawl_server::pool::TEST_QUERY_DELAY_MS;
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_QUERY_DELAY_MS.store(0, Ordering::Relaxed);
        }
    }
    let _reset = Reset;
    let root = tempfile::tempdir().unwrap();
    let pool = ExecutorPool::new(root.path().to_str().unwrap().into(), 1, 100, None);
    TEST_QUERY_DELAY_MS.store(1000, Ordering::Relaxed);
    let reader_pool = pool.clone();
    let id = pool.allocate_query_id();
    let request = tokio::spawn(async move {
        reader_pool
            .execute(id, "*", Duration::from_secs(10), false, 0)
            .await
    });
    // An interrupt handle is registered only after the blocking task starts.
    tokio::time::timeout(Duration::from_secs(5), async {
        while !pool.cancel_by_id(id) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    let publication = pool.publication();
    assert!(
        tokio::time::timeout(Duration::from_millis(30), publication.write())
            .await
            .is_err()
    );
    tokio::time::timeout(Duration::from_secs(5), publication.write())
        .await
        .unwrap();
}
