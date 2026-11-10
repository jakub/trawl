// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Real WAL, hot buffer and `DuckDB` reads across a paused publication.

use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use trawl_server::bus::IngestBatch;
use trawl_server::deadline::Deadline;
use trawl_server::error::ServerError;
use trawl_server::hot_buffer::{HotBuffer, HotBufferConfig};
use trawl_server::ingest::{compaction::compact_once, wal::WalWriter};
use trawl_server::pool::{ExecutorPool, WorkContext, WorkKind, seam};
use trawl_server::publication::{CorpusUnsettled, PublicationGate};
use trawl_server::test_timing::{HANG_GUARD, frozen, until, until_polling};

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
    let pool =
        ExecutorPool::for_test(data.to_str().unwrap().into(), 3, 100, Some(hot.clone())).unwrap();
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
    hot.insert_for_test(Arc::new(IngestBatch {
        batch_id: format!("prod/{}", path.file_stem().unwrap().to_str().unwrap()).into(),
        service: "nginx".into(),
        byte_size: ndjson.len(),
        events,
    }));
    let before = pool
        .execute(
            pool.allocate_query_id(),
            "*",
            Deadline::after(HANG_GUARD),
            false,
            WorkContext::system(WorkKind::Query),
        )
        .await;
    assert_eq!(before.result.unwrap().rows.len(), 3);

    // Counted from here on, so the successful query above does not
    // contribute an arrival.
    // The session stays alive: dropping it forgets the gate.
    let seams = pool.seams();
    let entry = seams.watch(seam::Seam::Entry);
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
    // The channel is the event: the compaction's publication arrived at the
    // pause. The bound only names a hang.
    tokio::task::spawn_blocking(move || entered.recv_timeout(HANG_GUARD).unwrap())
        .await
        .unwrap();
    assert_eq!(hot.event_count(), 3, "pause must precede hot drain");

    let blocked = pool
        .execute(
            pool.allocate_query_id(),
            "*",
            Deadline::after(Duration::from_millis(30)),
            false,
            WorkContext::system(WorkKind::Query),
        )
        .await;
    // The publication gate is a wait before any work starts, so a budget
    // that runs out there is a capacity refusal, not a query timeout
    // (ADR-0024).
    assert!(matches!(
        blocked.result,
        Err(ServerError::CapacityNotStarted)
    ));
    let blocked_export = pool
        .export_parquet(
            pool.allocate_query_id(),
            "*",
            100,
            Deadline::after(Duration::from_millis(30)),
            WorkContext::system(WorkKind::Export),
        )
        .await;
    assert!(matches!(
        blocked_export,
        Err(ServerError::CapacityNotStarted)
    ));
    assert_eq!(
        pool.available_permits(),
        3,
        "waiting readers return their permits"
    );
    // Entry is the worker's first step, before `begin_work`
    // (`ExecutorPool::execute`), so no arrival with every permit free
    // proves the publication gate refused the work. A refusal that merely
    // came from a worker starting late would have left an arrival here.
    assert_eq!(
        entry.arrivals(),
        0,
        "the held gate must refuse before any worker starts"
    );

    release.send(()).unwrap();
    // The compaction finishing is the event; the bound only names a hang.
    let compacted = tokio::time::timeout(HANG_GUARD, compact)
        .await
        .expect("the compaction did not finish after the publication was released");
    assert_eq!(compacted.unwrap().unwrap(), 0);
    assert_eq!(hot.event_count(), 0);
    let after = pool
        .execute(
            pool.allocate_query_id(),
            "*",
            Deadline::after(HANG_GUARD),
            false,
            WorkContext::system(WorkKind::Query),
        )
        .await;
    assert_eq!(after.result.unwrap().rows.len(), 3);
    let (export, rows) = pool
        .export_parquet(
            pool.allocate_query_id(),
            "*",
            100,
            Deadline::after(HANG_GUARD),
            WorkContext::system(WorkKind::Export),
        )
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
    assert_eq!(rows, 3, "the returned count is the file's");
}

#[tokio::test]
async fn query_only_pool_refuses_incomplete_rollup_after_restart() {
    let root = tempfile::tempdir().unwrap();
    let day = root.path().join("prod/2026-02-15");
    std::fs::create_dir_all(&day).unwrap();
    let marker = day.join(".rollup-nginx");
    std::fs::write(&marker, "unfinished").unwrap();
    let pool = ExecutorPool::for_test(root.path().to_str().unwrap().into(), 1, 100, None).unwrap();
    let result = pool
        .execute(
            pool.allocate_query_id(),
            "*",
            Deadline::after(Duration::from_secs(1)),
            false,
            WorkContext::system(WorkKind::Query),
        )
        .await;
    assert!(matches!(
        result.result,
        Err(ServerError::CorpusRecovering(
            CorpusUnsettled::RollupPending
        ))
    ));
    let export = pool
        .export_parquet(
            pool.allocate_query_id(),
            "*",
            100,
            Deadline::after(Duration::from_secs(1)),
            WorkContext::system(WorkKind::Export),
        )
        .await;
    assert!(matches!(
        export,
        Err(ServerError::CorpusRecovering(
            CorpusUnsettled::RollupPending
        ))
    ));
    assert!(matches!(
        pool.sample_field_values(
            "message",
            None,
            10,
            Deadline::after(Duration::from_secs(1)),
            WorkContext::system(WorkKind::Sample),
        )
        .await,
        Err(ServerError::CorpusRecovering(
            CorpusUnsettled::RollupPending
        ))
    ));
    assert_eq!(pool.available_permits(), 1);
    std::fs::remove_file(marker).unwrap();
    assert!(pool.publication().read().await.is_ok());
}

/// Whether `publication.write()` is still pending after one poll: a writer
/// that would change files the blocking reader still has open.
async fn writer_waits(publication: &PublicationGate) -> bool {
    until_polling(
        "one poll of the writer",
        &mut pin!(publication.write()),
        || true,
    )
    .await
    .is_none()
}

/// A request that times out answers at once, but the `DuckDB` worker keeps
/// the publication guard until it really finishes. The worker is held
/// past its work-start transition, so the deadline lands on running work,
/// and the paused clock reaches the deadline only when the test advances
/// it. Residual exposure: the real-clock hang guard on each wait.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn query_timeout_keeps_publication_guard_until_duckdb_task_finishes() {
    const DEADLINE: Duration = Duration::from_secs(1);
    let root = tempfile::tempdir().unwrap();
    let pool = ExecutorPool::for_test(root.path().to_str().unwrap().into(), 1, 100, None).unwrap();
    // Held for the whole test: its drop releases every gate.
    let seams = pool.seams();
    let started = seams.hold(seam::Seam::Started);
    let released = seams.watch(seam::Seam::Released);
    let reader_pool = pool.clone();
    let id = pool.allocate_query_id();
    let mut request = tokio::spawn(async move {
        reader_pool
            .execute(
                id,
                "*",
                Deadline::after(DEADLINE),
                false,
                WorkContext::system(WorkKind::Query),
            )
            .await
    });
    // The worker is past its work-start transition and parked.
    until("the reader started", || started.arrivals() == 1).await;
    tokio::time::advance(DEADLINE + Duration::from_millis(1)).await;
    let result = frozen("the timed-out request", &mut request).await.unwrap();
    assert!(
        matches!(result.result, Err(ServerError::Timeout)),
        "expected Timeout, got: {:?}",
        result.result
    );
    let publication = pool.publication();
    assert!(
        writer_waits(&publication).await,
        "async timeout must not let a writer change files still used by the blocking reader"
    );
    started.release();
    drop(frozen("the writer after the reader", publication.write()).await);
    // The worker has dropped everything it held.
    until("the worker released its work", || released.arrivals() == 1).await;
}

/// Aborting the request that started a read leaves the `DuckDB` worker
/// running and holding the publication guard until it finishes. The
/// worker is held past its work-start transition, so the abort lands on
/// running work. Residual exposure: the real-clock hang guard on each
/// wait.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn aborting_request_keeps_running_reader_protected() {
    let root = tempfile::tempdir().unwrap();
    let pool = ExecutorPool::for_test(root.path().to_str().unwrap().into(), 1, 100, None).unwrap();
    // Held for the whole test: its drop releases every gate.
    let seams = pool.seams();
    let started = seams.hold(seam::Seam::Started);
    let released = seams.watch(seam::Seam::Released);
    let reader_pool = pool.clone();
    let id = pool.allocate_query_id();
    let mut request = tokio::spawn(async move {
        reader_pool
            .execute(
                id,
                "*",
                Deadline::after(Duration::from_secs(10)),
                false,
                WorkContext::system(WorkKind::Query),
            )
            .await
    });
    // Wait for the reader to actually be reading. A registered slot is not
    // enough: work that has not passed its work-start transition holds no
    // files open, and cancelling it there would end it before the abort
    // could prove anything (ADR-0024).
    until("the reader started", || started.arrivals() == 1).await;
    request.abort();
    assert!(
        frozen("the aborted request", &mut request)
            .await
            .unwrap_err()
            .is_cancelled()
    );
    let publication = pool.publication();
    assert!(
        writer_waits(&publication).await,
        "an aborted request must not let a writer change files still used by the blocking reader"
    );
    started.release();
    drop(frozen("the writer after the reader", publication.write()).await);
    // The worker has dropped everything it held.
    until("the worker released its work", || released.arrivals() == 1).await;
}
