// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A capture reads the same bytes while the corpus moves under it and after
//! trawld restarts (ADR-0057).
//!
//! One test, one capture of the 20-event corpus
//! (`common::captured::corpus`), read whole: the default page, the same rows
//! in pages of seven, a projection, and every cell. The bytes each read
//! answered are kept and compared after each of these:
//!
//! - more matching events are ingested and a real compaction pass is held
//!   mid-publish, its files renamed into the data root and the publication
//!   write guard still held, so the reads run with the corpus changing
//!   under them;
//! - the compaction finishes;
//! - the fixture restarts in process over the same WAL, data root, results
//!   root and databases (`common::captured::Restartable`).
//!
//! A `/query` of the recorded text answers the new events too, so the
//! capture is a snapshot and not a re-run. The binary is in the ADR-0021
//! `postgres` nextest group.

mod common;

use std::time::Duration;

use common::TestServer;
use common::captured::{self, Restartable, at};
use reqwest::StatusCode;
use serde_json::json;
use trawl_config::CaptureConfig;
use trawl_server::config::RateLimitConfig;

/// Matching events ingested after the capture, inside its window.
const LATE: usize = 5;

/// Every read of a capture, as `(path, bytes answered)`.
type Reads = Vec<(String, Vec<u8>)>;

/// Read every page form and every cell of capture `id` with `columns`
/// columns, keeping the bytes each answered. One client for the batch,
/// dropped with it: a connection kept alive would hold the old server's
/// state across a restart.
async fn read_all(server: &TestServer, id: &str, columns: usize) -> Reads {
    let client = common::harness_client_builder().build().unwrap();
    let mut paths = vec![
        format!("/results/{id}"),
        format!("/results/{id}?columns=message,_time"),
    ];
    paths.extend(
        (0..captured::ROWS)
            .step_by(7)
            .map(|offset| format!("/results/{id}?offset={offset}&limit=7")),
    );
    for row in 0..captured::ROWS {
        for column in 0..columns {
            paths.push(format!("/results/{id}/cells/{row}/{column}"));
        }
    }
    let mut reads = Vec::with_capacity(paths.len());
    for path in paths {
        let response = client
            .get(format!("{}/api/v1{path}", server.url))
            .bearer_auth(&server.analyst_token)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.bytes().await.unwrap().to_vec();
        assert_eq!(
            status,
            StatusCode::OK,
            "{path}: {}",
            String::from_utf8_lossy(&bytes)
        );
        reads.push((path, bytes));
    }
    reads
}

/// `after` answered every read with the bytes `before` did.
fn assert_same(what: &str, before: &Reads, after: &Reads) {
    assert_eq!(before.len(), after.len(), "{what}: the same reads");
    for ((path, then), (_, now)) in before.iter().zip(after) {
        assert!(
            then == now,
            "{what}: {path} changed:\n  before {}\n  after  {}",
            String::from_utf8_lossy(then),
            String::from_utf8_lossy(now)
        );
    }
}

/// How many rows `/query` answers for `dsl` now.
async fn query_rows(server: &TestServer, dsl: &str) -> usize {
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
    response
        .json::<trawl_api::QueryResponse>()
        .await
        .unwrap()
        .result
        .rows
        .len()
}

/// Matching events inside the capture's window, ingested after it.
fn late_events() -> Vec<serde_json::Value> {
    (0..LATE)
        .map(|seq| {
            json!({
                "_time": format!("2024-01-16T10:05:{seq:02}Z"),
                "service": captured::SERVICE,
                "host": "web03",
                "message": format!("late {seq}"),
            })
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_capture_reads_the_same_bytes_through_ingest_compaction_and_a_restart() {
    // Every page form and every cell is a request: no per-key rate.
    let unlimited = RateLimitConfig {
        default_rpm: 0,
        ..RateLimitConfig::default()
    };
    let (fixture, server) = Restartable::boot(unlimited, CaptureConfig::default(), |data| {
        captured::seed_hour(data, captured::SERVICE, &captured::corpus());
    })
    .await;
    server.results_clock.set(at("2024-01-16T10:30:00Z"));

    let body =
        json!({ "query": format!("service={}", captured::SERVICE), "window": { "last": "1h" } });
    let result = captured::captured(&server, &server.analyst_token, &body)
        .await
        .result;
    assert_eq!(result.captured_rows, captured::ROWS as u64);
    let columns = result.columns.len();
    let before = read_all(&server, &result.id, columns).await;
    assert_eq!(
        query_rows(&server, &result.executed_query).await,
        captured::ROWS,
        "the recorded text answers the corpus before the late events"
    );

    // Late events land in the WAL and the hot buffer, then a compaction
    // pass moves them into the data root. It is held after its rename with
    // the publication write guard still held: every read runs while the
    // corpus is mid-publish.
    captured::ingest(&server, &late_events()).await;
    let publication = server
        .state
        .query
        .hot_buffer
        .as_ref()
        .expect("an ingest fixture has a hot buffer")
        .publication();
    let (entered, release) = publication.pause_next_publication_for_test();
    let ((), during) = tokio::join!(captured::compact(&server, &fixture.disk), async {
        tokio::task::spawn_blocking(move || {
            entered
                .recv_timeout(Duration::from_secs(60))
                .expect("the compaction pass reaches its publish");
        })
        .await
        .unwrap();
        let during = tokio::time::timeout(
            Duration::from_secs(60),
            read_all(&server, &result.id, columns),
        )
        .await
        .expect("a capture read never waits for the publication guard");
        release.send(()).unwrap();
        during
    });
    assert_same("while a compaction is mid-publish", &before, &during);
    assert_eq!(
        server
            .state
            .query
            .hot_buffer
            .as_ref()
            .unwrap()
            .event_count(),
        0,
        "the compaction drained the late events into the data root"
    );

    let after = read_all(&server, &result.id, columns).await;
    assert_same("after the compaction", &before, &after);
    assert_eq!(
        query_rows(&server, &result.executed_query).await,
        captured::ROWS + LATE,
        "the recorded text now answers the late events: the capture is a snapshot"
    );

    let old_url = server.url.clone();
    let server = fixture.restart(server).await;
    assert_ne!(server.url, old_url);
    assert!(
        common::harness_client_builder()
            .build()
            .unwrap()
            .get(format!("{old_url}/api/v1/results"))
            .bearer_auth(&server.analyst_token)
            .send()
            .await
            .is_err(),
        "the first server is gone: its listener is closed"
    );
    let restarted = read_all(&server, &result.id, columns).await;
    assert_same("after a restart", &before, &restarted);
    assert_eq!(
        captured::listed(&server, &server.analyst_token).await,
        vec![result.id.clone()],
        "the capture outlived the restart as the caller's"
    );
    assert_eq!(
        query_rows(&server, &result.executed_query).await,
        captured::ROWS + LATE
    );
}
