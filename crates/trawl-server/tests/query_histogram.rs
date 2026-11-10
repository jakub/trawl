// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The whole-result histogram over a real server (#197).
//!
//! Events go in through `/api/v1/ingest` and come back through
//! `/api/v1/query`, read by real `DuckDB` over the hot buffer.

mod common;

use common::{TestServer, setup_in_dir_with_data};
use trawl_api::value::{ColumnType, Value};
use trawl_server::config::RateLimitConfig;

/// A server over an empty data root, so `*` reads only what a test
/// ingests.
async fn server() -> TestServer {
    let tmp = tempfile::tempdir().expect("tempdir");
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&data).expect("the data root");
    let server = setup_in_dir_with_data(
        tmp.path(),
        data.to_str().expect("a UTF-8 path").to_owned(),
        RateLimitConfig::default(),
    )
    .await;
    // Leak the tempdir so it outlives the server (cleaned up by the OS).
    std::mem::forget(tmp);
    server
}

/// Accept `events` through `/api/v1/ingest` as the ingest key.
async fn ingest(server: &TestServer, events: &[serde_json::Value]) {
    let client = trawl_client::HttpClient::new_insecure(&server.url, &server.ingest_token)
        .expect("an ingest client");
    let answer = client.ingest(events).await.expect("the ingest succeeds");
    assert_eq!(answer.accepted, events.len(), "{answer:?}");
}

/// `count` kv-shaped events, one second apart from 2026-01-15 09:00 UTC.
fn kv_events(count: usize) -> Vec<serde_json::Value> {
    (0..count)
        .map(|i| {
            serde_json::json!({
                "service": "histo",
                "host": format!("h{}", i % 3),
                "timestamp": format!("2026-01-15T09:{:02}:{:02}Z", i / 60, i % 60),
                "message": format!("user=u{i} code={}", 200 + i % 5),
            })
        })
        .collect()
}

/// Run `request` over HTTP as the analyst and require a 200.
async fn query(server: &TestServer, request: &trawl_api::QueryRequest) -> trawl_api::QueryResponse {
    let response = common::harness_client_builder()
        .build()
        .unwrap()
        .post(format!("{}/api/v1/query", server.url))
        .bearer_auth(&server.analyst_token)
        .json(request)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(status, 200, "{}: {body}", request.query);
    serde_json::from_str(&body).unwrap_or_else(|e| panic!("{}: {e}: {body}", request.query))
}

/// The premise of #197: a Rust-tail result keeps `_time` typed
/// `timestamp`, with every non-null cell in the wire spelling
/// `wire_micros` reads, so the counter needs no reader for `DuckDB`'s
/// cast text.
#[tokio::test(flavor = "multi_thread")]
async fn histogram_reads_a_rust_tail_result() {
    let server = server().await;
    ingest(&server, &kv_events(12)).await;

    let answer = query(
        &server,
        &trawl_api::QueryRequest {
            query: "* | extract kv".to_owned(),
            limit: Some(1_000),
            offset: None,
        },
    )
    .await;
    let result = &answer.result;
    assert_eq!(answer.pagination.total, 12, "{result:?}");
    let at = result
        .columns
        .iter()
        .position(|c| c.name == "_time")
        .unwrap_or_else(|| panic!("a `_time` column: {:?}", result.columns));
    let first = result.rows.first().map(|row| &row[at]);
    assert_eq!(
        result.columns[at].ty,
        ColumnType::Timestamp,
        "`_time` stays typed timestamp after the kv tail; first cell {first:?}"
    );
    assert!(
        result.columns.iter().any(|c| c.name == "user"),
        "the kv tail ran: {:?}",
        result.columns
    );
    for row in &result.rows {
        match &row[at] {
            Value::Null => {}
            Value::String(text) => assert!(
                trawl_api::zone::wire_micros(text).is_some(),
                "`wire_micros` reads {text:?}"
            ),
            other => panic!("a `_time` cell is text or null, got {other:?}"),
        }
    }
}
