// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The boot steps that make the corpus ready to serve (ADR-0041 slice 2),
//! through `boot::prepare_corpus` over a real `AppState`: rollup-marker
//! recovery, then conformance, then WAL hydration.
//!
//! Each test plants WAL or markers in its own directories before the
//! fixture boots, observes the state before the boot through the fixture's
//! hook, and then checks what the boot left. The fixture never spawns
//! compaction.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{HotBufferKnobs, TestServer, seed_data_root, setup_observing_boot};
use trawl_server::bus::{EventBus as _, EventSubscriber as _, LocalSubscriber};
use trawl_server::publication::CorpusUnsettled;
use trawl_server::state::AppState;

/// Every planted event's service, so counts see no seed row.
const SERVICE: &str = "boothydrated";

/// A WAL file in the live writer's name and line format, planted under
/// `dir/wal/prod`, holding `count` events of [`SERVICE`].
fn plant_wal(dir: &Path, count: usize) -> PathBuf {
    let time = (chrono::Utc::now() - chrono::Duration::minutes(5))
        .to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
    let mut ndjson = Vec::new();
    for seq in 0..count {
        let event = serde_json::json!({
            "_time": time,
            "_ingested": time,
            "env": "prod",
            "service": SERVICE,
            "seq": seq,
            "message": format!("planted {seq}"),
        });
        serde_json::to_writer(&mut ndjson, &event).unwrap();
        ndjson.push(b'\n');
    }
    let writer = trawl_server::ingest::wal::WalWriter::new(dir.join("wal"));
    writer.ensure_dir().unwrap();
    writer.write("prod", SERVICE, &ndjson).unwrap()
}

fn client(server: &TestServer) -> trawl_client::HttpClient {
    trawl_client::HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap()
}

/// `stats count()` over `dsl` through the query API.
async fn count(server: &TestServer, dsl: &str) -> i64 {
    let dsl = format!("{dsl} | stats count()");
    let result = client(server)
        .query_paginated(&dsl, None, None)
        .await
        .unwrap_or_else(|e| panic!("{dsl}: {e}"));
    match &result.result.rows[0][0] {
        trawl_api::value::Value::Integer(n) => *n,
        other => panic!("{dsl}: count must be an integer, got {other:?}"),
    }
}

/// A query answers 503 `corpus_recovering`, with no `Retry-After`.
async fn assert_query_refused(server: &TestServer) {
    let response = common::harness_client_builder()
        .build()
        .unwrap()
        .post(format!("{}/api/v1/query", server.url))
        .bearer_auth(&server.analyst_token)
        .json(&serde_json::json!({ "query": "service=nginx" }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    assert!(
        response.headers().get("retry-after").is_none(),
        "no retry hint for a corpus refusal"
    );
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "corpus_recovering", "{body}");
}

/// `/api/v1/health`: HTTP 200 with `status` and `checks.corpus`.
async fn health(server: &TestServer) -> (String, String) {
    let response = common::harness_client_builder()
        .build()
        .unwrap()
        .get(format!("{}/api/v1/health", server.url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    (
        body["status"].as_str().unwrap().to_owned(),
        body["checks"]["corpus"].as_str().unwrap().to_owned(),
    )
}

/// The gate an ingest node's state reads through.
fn unsettled(state: &AppState) -> Option<CorpusUnsettled> {
    state.query.pool.publication().unsettled()
}

/// Boot hydrates WAL that survived a restart: a query counts every planted
/// event exactly once, before and after the compaction pass that drains
/// it, the gate settles without a coverage proof, and nothing reaches the
/// app's event bus, which was subscribed before the boot. The same
/// subscriber then receives an event ingested over HTTP, so its silence
/// during the boot is no dead subscription.
#[tokio::test(flavor = "multi_thread")]
async fn boot_hydration_counts_planted_wal_once_and_publishes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let data = seed_data_root(dir.path());
    plant_wal(dir.path(), 7);
    plant_wal(dir.path(), 5);

    let mut subscriber: Option<LocalSubscriber> = None;
    let mut born = None;
    let server = setup_observing_boot(dir.path(), data.clone(), None, &mut |state| {
        born = Some(unsettled(state));
        subscriber = Some(state.ingest.event_bus.as_ref().unwrap().subscribe());
    })
    .await;
    assert_eq!(
        born,
        Some(Some(CorpusUnsettled::RestartBacklog)),
        "an ingest node's gate is born starting"
    );
    assert_eq!(unsettled(&server.state), None, "everything fitted");

    let hot = server.state.query.hot_buffer.clone().unwrap();
    assert_eq!(hot.event_count(), 12);
    assert_eq!(count(&server, &format!("service={SERVICE}")).await, 12);
    let mut subscriber = subscriber.unwrap();
    assert!(
        tokio::time::timeout(Duration::ZERO, subscriber.recv())
            .await
            .is_err(),
        "hydration publishes nothing to the event bus"
    );
    assert_eq!(
        server
            .state
            .ingest
            .total_events
            .load(std::sync::atomic::Ordering::Relaxed),
        0,
        "hydration is not ingested traffic"
    );

    // The hydrated batch ids are the ones compaction drains.
    trawl_server::ingest::compaction::compact_once(
        &dir.path().join("wal"),
        Path::new(&data),
        Duration::ZERO,
        false,
        Some(&hot),
        500,
        "2GB",
        None,
    )
    .await
    .unwrap();
    assert_eq!(hot.event_count(), 0, "the pass drained both batches");
    assert_eq!(count(&server, &format!("service={SERVICE}")).await, 12);

    // The positive control: live ingest reaches the same subscriber.
    let response = common::harness_client_builder()
        .build()
        .unwrap()
        .post(format!("{}/api/v1/ingest", server.url))
        .bearer_auth(&server.ingest_token)
        .header("content-type", "application/x-ndjson")
        .body(r#"{"service":"busprobe","message":"live"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let batch = tokio::time::timeout(Duration::from_secs(10), subscriber.recv())
        .await
        .expect("the subscriber receives live ingest")
        .expect("the bus is open and the subscriber has not lagged");
    assert_eq!(&*batch.service, "busprobe");
    assert_eq!(batch.events.len(), 1);
}

/// WAL the caps cannot hold is overhang: reads refuse `restart_backlog`
/// and health degrades at HTTP 200, so probes do not restart the server.
#[tokio::test(flavor = "multi_thread")]
async fn boot_hydration_above_the_caps_leaves_overhang() {
    let dir = tempfile::tempdir().unwrap();
    let data = seed_data_root(dir.path());
    plant_wal(dir.path(), 5);

    let knobs = HotBufferKnobs {
        max_events: 2,
        max_bytes: 1 << 20,
        compaction_interval_secs: 3600,
    };
    let server = setup_observing_boot(dir.path(), data, Some(knobs), &mut |_| {}).await;
    assert_eq!(
        unsettled(&server.state),
        Some(CorpusUnsettled::RestartBacklog)
    );
    assert_eq!(
        server
            .state
            .query
            .hot_buffer
            .as_ref()
            .unwrap()
            .event_count(),
        0
    );
    assert_query_refused(&server).await;
    assert_eq!(
        health(&server).await,
        ("degraded".to_owned(), "restart_backlog".to_owned())
    );
}

/// The seed's nginx hourly file, its day, and a daily file holding the
/// same rows, as an interrupted rollup leaves them.
fn plant_rollup(data: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let day = data.join("prod/2024-01-15");
    let hourly = day.join("10/nginx.parquet");
    std::fs::copy(&hourly, day.join("nginx.parquet")).unwrap();
    let marker = day.join(".rollup-nginx");
    std::fs::write(&marker, hourly.to_string_lossy().as_bytes()).unwrap();
    (day, hourly, marker)
}

/// A pending rollup marker is recovered by the boot, before the listener:
/// the hourly input the daily file already holds is retired, and each row
/// is counted once.
#[tokio::test(flavor = "multi_thread")]
async fn rollup_boot_recovery_recovers_a_pending_marker_before_serving() {
    let dir = tempfile::tempdir().unwrap();
    let data = seed_data_root(dir.path());
    let (day, hourly, marker) = plant_rollup(Path::new(&data));

    let server = setup_observing_boot(dir.path(), data, None, &mut |state| {
        assert!(
            state
                .query
                .pool
                .publication()
                .unsettled_reasons()
                .contains(CorpusUnsettled::RollupPending),
            "the gate's boot scan registered the marker"
        );
    })
    .await;
    assert!(!marker.exists(), "recovered before the fixture served");
    assert!(!hourly.exists(), "the hourly input was retired");
    assert!(day.join("nginx.parquet").is_file());
    assert_eq!(unsettled(&server.state), None);
    assert_eq!(count(&server, "service=nginx").await, 2);
    assert_eq!(health(&server).await, ("ok".to_owned(), "ok".to_owned()));
}

/// A rollup recovery that fails does not stop the boot. The marker stays,
/// reads refuse `rollup_pending`, and health names it.
#[tokio::test(flavor = "multi_thread")]
async fn rollup_boot_recovery_failure_refuses_reads_as_rollup_pending() {
    let dir = tempfile::tempdir().unwrap();
    let data = seed_data_root(dir.path());
    let day = Path::new(&data).join("prod/2024-01-15");
    // An hourly input that can be neither removed nor set aside.
    let stuck = day.join("10/stuck.parquet");
    for path in [stuck.clone(), day.join("10/stuck.parquet.merged")] {
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("child"), b"occupied").unwrap();
    }
    std::fs::copy(day.join("10/nginx.parquet"), day.join("stuck.parquet")).unwrap();
    let marker = day.join(".rollup-stuck");
    std::fs::write(&marker, stuck.to_string_lossy().as_bytes()).unwrap();

    let server = setup_observing_boot(dir.path(), data, None, &mut |_| {}).await;
    assert!(marker.exists(), "the marker waits for a compaction pass");
    assert_eq!(
        unsettled(&server.state),
        Some(CorpusUnsettled::RollupPending)
    );
    assert_query_refused(&server).await;
    assert_eq!(
        health(&server).await,
        ("degraded".to_owned(), "rollup_pending".to_owned())
    );
}

/// A query-only node has no hot buffer and a settled gate: it never
/// hydrates, so nothing holds its reads.
#[tokio::test(flavor = "multi_thread")]
async fn query_only_state_is_born_settled() {
    let dir = tempfile::tempdir().unwrap();
    let server = common::setup_in_dir_with_ingest(dir.path(), false).await;
    assert!(server.state.query.hot_buffer.is_none());
    assert_eq!(unsettled(&server.state), None);
    assert_eq!(count(&server, "service=nginx").await, 2);
}
