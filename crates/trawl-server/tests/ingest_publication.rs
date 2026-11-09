// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Durable WAL batches must enter hot memory before compaction can drain them.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, SubsecRound as _, TimeDelta, Utc};
use common::captured;
use indexmap::IndexMap;
use serde_json::json;
use trawl_api::value::Value;
use trawl_config::CaptureConfig;
use trawl_server::config::RateLimitConfig;
use trawl_server::hot_buffer::{HotBuffer, HotBufferConfig};
use trawl_server::ingest::envelope::{EnvelopeContext, canonicalize};
use trawl_server::ingest::pipeline::{PipelineWriter, ServiceBatch};
use trawl_server::ingest::producer::{Derivation, Producer};
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
            tokio::task::spawn_blocking(move || pipeline.write(pipeline.admit_for_test(batches)))
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
            trawl_server::ingest::handler::IngestKey::new(verified).unwrap(),
            axum::Extension("127.0.0.1:12345".parse().unwrap()),
            axum::http::HeaderMap::new(),
            Ok(axum::body::Bytes::from_static(
                b"{\"service\":\"http\",\"message\":\"same payload\"}\n{\"service\":\"http\",\"message\":\"same payload\"}\n",
            )),
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

/// Render a handler failure the way the router does and return its status
/// and body text.
async fn rendered(error: trawl_server::error::ServerError) -> (axum::http::StatusCode, String) {
    use axum::response::IntoResponse;
    let response = error.into_response();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

/// A failed group fails the whole request with a redacted 500, after the
/// groups that did write are published and counted.
async fn assert_redacted_wal_500(error: trawl_server::error::ServerError, private: &[&str]) {
    let (status, body) = rendered(error).await;
    assert_eq!(status, axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    assert!(body.contains("internal server error"), "{body}");
    for text in private {
        assert!(!body.contains(text), "{text:?} leaked into {body}");
    }
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
    let error = trawl_server::ingest::handler::ingest(
        axum::extract::State(state),
        trawl_server::ingest::handler::IngestKey::new(verified).unwrap(),
        axum::Extension("127.0.0.1:12345".parse().unwrap()),
        axum::http::HeaderMap::new(),
        Ok(axum::body::Bytes::from_static(
            b"{\"env\":\"prod\",\"service\":\"before\",\"id\":1}\n\
              {\"env\":\"blocked\",\"service\":\"failed\",\"id\":2}\n\
              {\"env\":\"blocked\",\"service\":\"failed\",\"id\":3}\n\
              {\"env\":\"blocked\",\"service\":\"failed\",\"id\":4}\n\
              {\"env\":\"prod\",\"service\":\"after\",\"id\":5}\n\
              {\"env\":\"prod\",\"service\":\"after\",\"id\":6}\n",
        )),
    )
    .await
    .unwrap_err();
    // Any failed WAL group fails the request, and the filesystem error
    // never reaches the body. The rejection metric still records all
    // three events of the failed group.
    assert_redacted_wal_500(error, &["blocked", "WAL", "os error"]).await;
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
    let scratch = trawl_server::hot_buffer::TestScratch::new();
    let snapshot = hot.snapshot(&scratch).unwrap().unwrap();
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

/// A failed WAL directory fsync is a failed ack: the handler answers a
/// redacted 500, the failed group leaves no WAL file, and the group that
/// wrote before it failed is still published.
#[tokio::test]
async fn http_handler_directory_sync_failure_is_a_redacted_500_after_publishing_written_groups() {
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
    let state = server.state.clone();
    let hot = state.query.hot_buffer.as_ref().unwrap().clone();
    let wal = state.ingest.wal_writer.as_ref().unwrap().clone();
    // Make `prod` durable first, so the injected failure hits the env
    // directory sync after the first group's link, not the root sync.
    std::fs::remove_file(wal.write("prod", "warm", b"{}\n").unwrap()).unwrap();

    let metrics = common::test_metrics_handle();
    trawl_server::metrics::init_operational_alert_metrics();
    let rejected_series = "trawl_ingest_events_rejected_total{reason=\"wal_failure\"}";
    let sync_series = "trawl_wal_durability_failures_total{operation=\"parent_directory_sync\"}";
    let rejected_before = counter(&metrics, rejected_series);
    let sync_before = counter(&metrics, sync_series);
    wal.fail_next_directory_sync_for_test();
    let error = trawl_server::ingest::handler::ingest(
        axum::extract::State(state),
        trawl_server::ingest::handler::IngestKey::new(verified).unwrap(),
        axum::Extension("127.0.0.1:12345".parse().unwrap()),
        axum::http::HeaderMap::new(),
        Ok(axum::body::Bytes::from_static(
            b"{\"service\":\"failed\",\"id\":1}\n\
              {\"service\":\"written\",\"id\":2}\n\
              {\"service\":\"written\",\"id\":3}\n",
        )),
    )
    .await
    .unwrap_err();
    assert_redacted_wal_500(error, &["injected", "directory", "failed", "WAL"]).await;
    assert_eq!(counter(&metrics, sync_series) - sync_before, 1);
    assert_eq!(counter(&metrics, rejected_series) - rejected_before, 1);

    let files: Vec<_> = std::fs::read_dir(wal.dir().join("prod"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(files.len(), 1, "only the written group's file: {files:?}");
    assert!(
        files[0]
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("written_")
    );
    let durable: Vec<u64> = std::fs::read_to_string(&files[0])
        .unwrap()
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["id"]
                .as_u64()
                .unwrap()
        })
        .collect();
    assert_eq!(durable, [2, 3]);
    assert_eq!(hot.event_count(), 2, "the written group is published");
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
            trawl_server::ingest::handler::IngestKey::new(verified).unwrap(),
            axum::Extension("127.0.0.1:12345".parse().unwrap()),
            axum::http::HeaderMap::new(),
            Ok(axum::body::Bytes::from_static(
                b"{\"service\":\"http\",\"message\":\"wait\"}\n",
            )),
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
        drop(publication.mark_rollup(&marker));
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
            tokio::task::spawn_blocking(move || pipeline.write(pipeline.admit_for_test(batches))),
        )
        .await
        .unwrap()
        .unwrap(),
        1
    );
    assert_eq!(hot.event_count(), 1);
    assert!(publication.read().await.is_err());
}

/// One `/api/v1/query` answer as rows of column name to value, read with
/// real `DuckDB` over the hot buffer and the selected parquet files.
async fn query_rows(server: &common::TestServer, dsl: &str) -> Vec<Row> {
    let response = common::harness_client_builder()
        .build()
        .unwrap()
        .post(format!("{}/api/v1/query", server.url))
        .bearer_auth(&server.analyst_token)
        .json(&trawl_api::QueryRequest {
            query: dsl.to_owned(),
            limit: Some(10_000),
            offset: None,
            timezone: None,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{dsl}: {:?}", response.text().await);
    let result = response
        .json::<trawl_api::QueryResponse>()
        .await
        .unwrap()
        .result;
    let names: Vec<String> = result.columns.iter().map(|c| c.name.clone()).collect();
    result
        .rows
        .into_iter()
        .map(|row| names.iter().cloned().zip(row).collect())
        .collect()
}

type Row = std::collections::BTreeMap<String, Value>;

/// The instant a query answer spells as RFC 3339 UTC text.
fn instant(value: &Value) -> DateTime<Utc> {
    match value {
        Value::String(text) => DateTime::parse_from_rfc3339(text)
            .unwrap_or_else(|e| panic!("{text}: {e}"))
            .to_utc(),
        other => panic!("not a timestamp: {other:?}"),
    }
}

/// The repair codes a row's `_repairs` lists.
fn repairs(row: &Row) -> Vec<&str> {
    match row.get("_repairs") {
        Some(Value::String(codes)) => codes.split(',').collect(),
        _ => Vec::new(),
    }
}

/// Every file named `name` under `root`, at any depth.
fn files_named(root: &Path, name: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().is_some_and(|n| n == name) {
                found.push(path);
            }
        }
    }
    found
}

/// The one `ahead` row, stored at its arrival with the repair, and the
/// sender's time still in its `timestamp` column.
fn assert_stored_at_arrival(
    rows: &[Row],
    sent: DateTime<Utc>,
    arrived: std::ops::RangeInclusive<DateTime<Utc>>,
    label: &str,
) {
    assert_eq!(rows.len(), 1, "{label}: exactly once: {rows:?}");
    let row = &rows[0];
    assert!(
        arrived.contains(&instant(&row["_time"])),
        "{label}: `_time` is the arrival: {row:?}"
    );
    assert_eq!(row["_time"], row["_ingested"], "{label}: arrival filled it");
    assert!(
        repairs(row).contains(&"time.ahead_of_arrival"),
        "{label}: {row:?}"
    );
    assert_eq!(
        instant(&row["timestamp"]),
        sent,
        "{label}: the sender's time stays as its column"
    );
}

/// An event whose sender stamps it three hours ahead is stored at its
/// arrival with `time.ahead_of_arrival`, so `last=15m` returns it exactly
/// once, from its hourly file and from its day file.
///
/// The real rollup skips today and compaction has no clock seam, so the
/// day-file leg moves the compacted hourly file to the name the rollup
/// writes, `{day}/{service}.parquet`. The aged-directory property, where
/// the walk must still reach a directory older than an hour, is
/// `source::tests::last_window_reaches_every_row_within_the_lead`. A
/// `recent` service keeps its hourly file in the same hour, so the walk
/// has a recent directory and the day holds daily and hourly files.
#[tokio::test(flavor = "multi_thread")]
async fn ahead_of_arrival_event_is_found_by_last_after_compaction_and_rollup() {
    let (fixture, server) =
        captured::Restartable::boot(RateLimitConfig::default(), CaptureConfig::default(), |_| {})
            .await;
    let before = Utc::now();
    let sent = DateTime::from_timestamp((before + TimeDelta::hours(3)).timestamp(), 0).unwrap();
    captured::ingest(
        &server,
        &[
            json!({"service": "ahead", "host": "h", "message": "m",
                   "timestamp": sent.format("%Y-%m-%dT%H:%M:%SZ").to_string()}),
            json!({"service": "recent", "host": "h", "message": "r"}),
        ],
    )
    .await;
    let arrived = before..=Utc::now();
    captured::compact(&server, &fixture.disk).await;
    let hot = server.state.query.hot_buffer.as_ref().unwrap();
    assert_eq!(hot.event_count(), 0, "both events are stored, not hot");

    let rows = query_rows(&server, "service=ahead last=15m").await;
    assert_stored_at_arrival(&rows, sent, arrived.clone(), "hourly");

    let prod = Path::new(&fixture.disk.data).join("prod");
    let [hourly] = files_named(&prod, "ahead.parquet").try_into().unwrap();
    let hour_dir = hourly.parent().unwrap();
    assert!(
        hour_dir.join("recent.parquet").exists(),
        "the recent directory"
    );
    let daily = hour_dir.parent().unwrap().join("ahead.parquet");
    std::fs::rename(&hourly, &daily).unwrap();

    let rows = query_rows(&server, "service=ahead last=15m").await;
    assert_stored_at_arrival(&rows, sent, arrived, "daily");
    let rows = query_rows(&server, "last=15m").await;
    for service in ["ahead", "recent"] {
        let found = rows
            .iter()
            .filter(|row| row["service"] == Value::String(service.to_owned()))
            .count();
        assert_eq!(found, 1, "{service} once from the day's mixed files");
    }
}

/// Rows stored before the bound keep the `_time` they were written with. A
/// WAL batch in the writer's own format, planted before boot with a `_time`
/// three hours ahead, keeps it through hydration and compaction, since
/// neither canonicalizes again, and a parquet file holding such a `_time`
/// reads as it is. Neither earns `time.ahead_of_arrival`.
#[tokio::test(flavor = "multi_thread")]
async fn preexisting_future_time_is_not_rewritten() {
    let now = Utc::now().trunc_subsecs(6);
    let future = now + TimeDelta::hours(3);
    let planted = future.to_rfc3339_opts(SecondsFormat::Micros, true);
    let (fixture, server) = captured::Restartable::boot(
        RateLimitConfig::default(),
        CaptureConfig::default(),
        |data| {
            // What the canonicalizer wrote before the bound for a sender
            // three hours ahead: a clean event with that `_time`.
            let arrival = now.to_rfc3339_opts(SecondsFormat::Micros, true);
            let envs = ["prod".to_owned()];
            let derivation = Derivation::defaults();
            let ctx = EnvelopeContext {
                arrival: &arrival,
                arrival_instant: now,
                envs: &envs,
                default_env: "prod",
                producer: Producer::Http {
                    peer_host: "10.0.4.55",
                    peer_is_trusted_relay: false,
                },
                derivation: &derivation,
            };
            let event = json!({"service": "planted", "env": "prod", "host": "h",
                               "message": "m", "_time": arrival});
            let mut canonical = canonicalize(event.as_object().unwrap(), &ctx).unwrap();
            assert!(canonical.repairs.is_empty(), "{:?}", canonical.repairs);
            canonical.obj.insert("_time".into(), json!(planted));
            let mut batch = ServiceBatch::default();
            batch.push(canonical.obj);
            let wal = Path::new(data).parent().unwrap().join("wal");
            let pipeline = PipelineWriter::new(Arc::new(WalWriter::new(wal)), None, None);
            let batches = IndexMap::from([(("prod".into(), "planted".into()), batch)]);
            assert_eq!(pipeline.write(pipeline.admit_for_test(batches)), 1);

            captured::seed_partition(
                data,
                &format!("prod/{}", now.format("%Y-%m-%d/%H")),
                "stored",
                &[captured::Event {
                    time: future,
                    host: "h".into(),
                    message: "stored".into(),
                }],
            );
        },
    )
    .await;
    let hot = server.state.query.hot_buffer.as_ref().unwrap();
    assert_eq!(hot.event_count(), 1, "hydration took the planted batch");

    let kept = |rows: &[Row], label: &str| {
        assert_eq!(rows.len(), 1, "{label}: {rows:?}");
        assert_eq!(rows[0]["_time"], Value::String(planted.clone()), "{label}");
        assert!(
            !repairs(&rows[0]).contains(&"time.ahead_of_arrival"),
            "{label}: {rows:?}"
        );
    };
    kept(
        &query_rows(&server, "service=planted last=15m").await,
        "hydrated",
    );
    captured::compact(&server, &fixture.disk).await;
    assert_eq!(hot.event_count(), 0, "the planted batch is stored, not hot");
    kept(
        &query_rows(&server, "service=planted last=15m").await,
        "compacted",
    );
    kept(
        &query_rows(&server, "service=stored last=15m").await,
        "parquet",
    );
}
