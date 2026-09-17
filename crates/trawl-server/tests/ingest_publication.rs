// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Durable WAL batches must enter hot memory before compaction can drain them.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use indexmap::IndexMap;
use trawl_server::hot_buffer::{HotBuffer, HotBufferConfig};
use trawl_server::ingest::pipeline::{PipelineWriter, ServiceBatch};
use trawl_server::ingest::wal::WalWriter;

async fn assert_cancelled_caller_finishes_insert(
    task: tokio::task::JoinHandle<()>,
    hot: &HotBuffer,
    wal_root: &Path,
    entered: std::sync::mpsc::Receiver<()>,
    release: std::sync::mpsc::Sender<()>,
) {
    tokio::task::spawn_blocking(move || entered.recv_timeout(Duration::from_secs(10)).unwrap())
        .await
        .unwrap();
    let wal_files: Vec<_> = std::fs::read_dir(wal_root.join("prod"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "ndjson"))
        .collect();
    assert_eq!(wal_files.len(), 1, "the paused batch is already durable");
    let rows = std::fs::read_to_string(&wal_files[0])
        .unwrap()
        .lines()
        .count();
    assert_eq!(
        rows, 2,
        "identical accepted payloads remain separate events"
    );
    assert_eq!(hot.event_count(), 0, "the pause precedes hot insertion");

    let publication = hot.publication();
    let reader = tokio::time::timeout(Duration::from_secs(1), publication.read())
        .await
        .expect("ingestion permits concurrent query readers")
        .unwrap();
    drop(reader);

    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let mut writer = Box::pin(publication.write());
    assert!(
        tokio::time::timeout(Duration::from_millis(30), writer.as_mut())
            .await
            .is_err(),
        "cancelling the caller must not admit a compactor before hot insertion"
    );
    release.send(()).unwrap();
    let _writer = tokio::time::timeout(Duration::from_secs(5), writer)
        .await
        .expect("the queued writer must enter after hot insertion");
    assert_eq!(hot.event_count(), rows);
    let batch_id = format!(
        "prod/{}",
        wal_files[0].file_stem().unwrap().to_str().unwrap()
    );
    hot.drain(&[&batch_id]);
    assert_eq!(
        hot.event_count(),
        0,
        "the WAL batch cannot arrive after its drain"
    );
}

#[tokio::test]
async fn cancelled_syslog_caller_keeps_wal_and_hot_insert_together() {
    let root = tempfile::tempdir().unwrap();
    let hot = Arc::new(HotBuffer::new(HotBufferConfig {
        max_events: 100,
        max_bytes: 100_000,
    }));
    let wal = Arc::new(WalWriter::new(root.path().join("wal")));
    let pipeline = Arc::new(PipelineWriter::new(wal.clone(), Some(hot.clone()), None));
    let (entered, release) = pipeline.pause_next_insert_for_test();
    let mut batch = ServiceBatch::default();
    let event = serde_json::json!({"service": "syslog", "message": "same payload"})
        .as_object()
        .unwrap()
        .clone();
    batch.push(event.clone());
    batch.push(event);
    let batches = IndexMap::from([(("prod".into(), "syslog".into()), batch)]);
    let task = tokio::spawn(async move {
        assert_eq!(
            tokio::task::spawn_blocking(move || pipeline.write(batches))
                .await
                .unwrap(),
            2
        );
    });
    assert_cancelled_caller_finishes_insert(task, &hot, wal.dir(), entered, release).await;
}

#[tokio::test]
async fn cancelled_http_handler_keeps_wal_and_hot_insert_together() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let server = common::setup_in_dir_with_data(
        root.path(),
        data.to_str().unwrap().to_owned(),
        trawl_server::config::RateLimitConfig::default(),
    )
    .await;
    let verified = fleet_auth::KeyStore::from_pool(server.fleet_pool.clone())
        .verify_key(&server.ingest_token)
        .await
        .unwrap();
    let hot = server.state.query.hot_buffer.as_ref().unwrap().clone();
    let wal = server.state.ingest.wal_writer.as_ref().unwrap().clone();
    let pipeline = server.state.ingest.pipeline.as_ref().unwrap();
    let (entered, release) = pipeline.pause_next_insert_for_test();
    let state = server.state.clone();
    let task = tokio::spawn(async move {
        let response = trawl_server::ingest::handler::ingest(
            axum::extract::State(state),
            axum::Extension(verified),
            axum::Extension("127.0.0.1:12345".parse().unwrap()),
            axum::http::HeaderMap::new(),
            axum::body::Bytes::from_static(
                b"{\"service\":\"http\",\"message\":\"same payload\"}\n{\"service\":\"http\",\"message\":\"same payload\"}\n",
            ),
        )
        .await
        .unwrap();
        assert_eq!(response.0.accepted, 2);
    });
    assert_cancelled_caller_finishes_insert(task, &hot, wal.dir(), entered, release).await;
}

fn counter(handle: &metrics_exporter_prometheus::PrometheusHandle, series: &str) -> u64 {
    let rendered = handle.render();
    rendered
        .lines()
        .find_map(|line| {
            let (name, value) = line.rsplit_once(' ')?;
            (name == series).then(|| value.parse().expect("integer counter sample"))
        })
        .unwrap_or_else(|| panic!("missing series {series} in {rendered}"))
}

/// Exercise the actual handler and its blocking finalization, rather than
/// manually emitting parsed rejection counts. This is not a socket request.
#[tokio::test]
async fn http_handler_wal_failure_emits_rejections_and_publishes_only_successful_groups() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let server = common::setup_in_dir_with_data(
        root.path(),
        data.to_str().unwrap().to_owned(),
        trawl_server::config::RateLimitConfig::default(),
    )
    .await;
    let verified = fleet_auth::KeyStore::from_pool(server.fleet_pool.clone())
        .verify_key(&server.ingest_token)
        .await
        .unwrap();
    let mut state = server.state.clone();
    // This handler invocation admits both environments. Only the blocked
    // environment's WAL path fails; the shared fixture needs no new option.
    state.ingest.envs = vec!["prod".to_owned(), "blocked".to_owned()].into();
    let hot = state.query.hot_buffer.as_ref().unwrap().clone();
    let wal = state.ingest.wal_writer.as_ref().unwrap().clone();
    std::fs::write(wal.dir().join("blocked"), b"retain this obstruction").unwrap();
    assert_eq!(hot.event_count(), 0);

    let metrics = common::test_metrics_handle();
    trawl_server::metrics::init_operational_alert_metrics();
    let rejected_series = "trawl_ingest_events_rejected_total{reason=\"wal_failure\"}";
    let rejected_before = counter(&metrics, rejected_series);
    let discarded_before = counter(
        &metrics,
        trawl_server::metrics::SYSLOG_WAL_EVENTS_DISCARDED_TOTAL,
    );
    let response = trawl_server::ingest::handler::ingest(
        axum::extract::State(state),
        axum::Extension(verified),
        axum::Extension("127.0.0.1:12345".parse().unwrap()),
        axum::http::HeaderMap::new(),
        axum::body::Bytes::from_static(
            b"{\"env\":\"prod\",\"service\":\"before\",\"id\":1}\n\
              {\"env\":\"blocked\",\"service\":\"failed\",\"id\":2}\n\
              {\"env\":\"blocked\",\"service\":\"failed\",\"id\":3}\n\
              {\"env\":\"blocked\",\"service\":\"failed\",\"id\":4}\n\
              {\"env\":\"prod\",\"service\":\"after\",\"id\":5}\n\
              {\"env\":\"prod\",\"service\":\"after\",\"id\":6}\n",
        ),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(response.accepted, 3);
    // Preserve the existing response contract: a failed WAL group adds one
    // error entry, while its rejection metric records all three events.
    assert_eq!(response.rejected, 1);
    assert_eq!(response.errors.len(), 1);
    assert!(response.errors[0].message.contains("failed"));
    assert_eq!(counter(&metrics, rejected_series) - rejected_before, 3);
    assert_eq!(
        counter(
            &metrics,
            trawl_server::metrics::SYSLOG_WAL_EVENTS_DISCARDED_TOTAL
        ) - discarded_before,
        0
    );

    let mut durable = Vec::new();
    let files: Vec<_> = std::fs::read_dir(wal.dir().join("prod"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(files.len(), 2);
    for path in files {
        assert_eq!(path.extension().unwrap(), "ndjson");
        durable.extend(
            std::fs::read_to_string(path)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap()),
        );
    }
    let mut durable_ids: Vec<_> = durable
        .iter()
        .map(|event| event["id"].as_u64().unwrap())
        .collect();
    durable_ids.sort_unstable();
    assert_eq!(durable_ids, [1, 5, 6]);
    assert!(durable.iter().all(|event| event["env"] == "prod"));
    assert_eq!(hot.event_count(), 3);
    let snapshot = hot.snapshot().unwrap();
    let mut hot_ids: Vec<_> = std::fs::read_to_string(snapshot.path())
        .unwrap()
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["id"]
                .as_u64()
                .unwrap()
        })
        .collect();
    hot_ids.sort_unstable();
    assert_eq!(hot_ids, durable_ids);
    assert_eq!(
        std::fs::read(wal.dir().join("blocked")).unwrap(),
        b"retain this obstruction"
    );
}

#[test]
fn waiting_http_ingest_leaves_blocking_pool_available() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let server = common::setup_in_dir_with_data(
            root.path(),
            data.to_str().unwrap().to_owned(),
            trawl_server::config::RateLimitConfig::default(),
        )
        .await;
        let verified = fleet_auth::KeyStore::from_pool(server.fleet_pool.clone())
            .verify_key(&server.ingest_token)
            .await
            .unwrap();
        let hot = server.state.query.hot_buffer.as_ref().unwrap().clone();
        let publication = hot.publication();
        let writer = publication.write().await;
        let mut request = Box::pin(trawl_server::ingest::handler::ingest(
            axum::extract::State(server.state.clone()),
            axum::Extension(verified),
            axum::Extension("127.0.0.1:12345".parse().unwrap()),
            axum::http::HeaderMap::new(),
            axum::body::Bytes::from_static(b"{\"service\":\"http\",\"message\":\"wait\"}\n"),
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(30), request.as_mut())
                .await
                .is_err(),
            "ingestion waits for publication"
        );
        tokio::time::timeout(Duration::from_secs(5), tokio::task::spawn_blocking(|| ()))
            .await
            .expect("waiting HTTP requests must leave the only blocking thread available")
            .unwrap();
        drop(writer);
        let response = tokio::time::timeout(Duration::from_secs(5), request)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.0.accepted, 1);
        assert_eq!(hot.event_count(), 1);
    });
}

#[tokio::test]
async fn pending_rollup_does_not_reject_ingest() {
    let root = tempfile::tempdir().unwrap();
    let hot = Arc::new(HotBuffer::new(HotBufferConfig {
        max_events: 100,
        max_bytes: 100_000,
    }));
    let publication = hot.publication();
    let marker = root.path().join(".rollup-syslog");
    std::fs::write(&marker, "unfinished").unwrap();
    {
        let _writer = publication.write().await;
        publication.mark_rollup(&marker);
    }
    assert!(publication.read().await.is_err());
    let pipeline = PipelineWriter::new(
        Arc::new(WalWriter::new(root.path().join("wal"))),
        Some(hot.clone()),
        None,
    );
    let mut batch = ServiceBatch::default();
    batch.push(serde_json::Map::new());
    let batches = IndexMap::from([(("prod".into(), "syslog".into()), batch)]);
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(5),
            tokio::task::spawn_blocking(move || pipeline.write(batches)),
        )
        .await
        .unwrap()
        .unwrap(),
        1
    );
    assert_eq!(hot.event_count(), 1);
    assert!(publication.read().await.is_err());
}
