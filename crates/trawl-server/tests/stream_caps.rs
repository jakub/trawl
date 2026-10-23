// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A live aggregate discloses the caps that dropped its input (ADR-0001).
//!
//! Every snapshot of an aggregate stream carries `capped`, one
//! `{cap, limit}` per cap that dropped input since the stream started, and
//! omits the field when nothing was dropped. These run the real SSE loop on
//! a live server with real ingest, because the property lives in the wiring:
//! the dropped event has to count toward the next snapshot, and the report
//! has to ride every snapshot after it. The batch lane's Rust tail runs the
//! same accumulators and has no snapshot to carry a report, so it refuses
//! instead; the last test pins that answer over HTTP.

mod common;

use std::time::Duration;

use common::setup;
use serde_json::{Value, json};
use trawl_client::HttpClient;

/// The live group cap, as the wire states it.
const MAX_GROUPS: usize = 10_000;

/// One open aggregate stream and the bytes read off it so far.
struct SnapshotReader {
    resp: reqwest::Response,
    buf: Vec<u8>,
}

impl SnapshotReader {
    async fn open(server: &common::TestServer, query: &str) -> Self {
        let resp = common::harness_client_builder()
            .build()
            .unwrap()
            .get(format!("{}/api/v1/stream", server.url))
            .query(&[("query", query)])
            .bearer_auth(&server.analyst_token)
            .send()
            .await
            .expect("open the stream");
        assert!(resp.status().is_success(), "{resp:?}");
        Self {
            resp,
            buf: Vec::new(),
        }
    }

    /// The next complete `snapshot` frame that satisfies `wanted`, skipping
    /// the ones before it. Panics after 30 s, naming what did arrive.
    async fn snapshot_where(&mut self, wanted: impl Fn(&Value) -> bool) -> Value {
        let mut seen = Vec::new();
        let read = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                while let Some(end) = find(&self.buf, b"\n\n") {
                    let frame = String::from_utf8(self.buf[..end].to_vec()).expect("utf-8 frame");
                    self.buf.drain(..end + 2);
                    let mut event = None;
                    let mut data = None;
                    for line in frame.lines() {
                        if let Some(name) = line.strip_prefix("event: ") {
                            event = Some(name.to_owned());
                        } else if let Some(json) = line.strip_prefix("data: ") {
                            data = Some(json.to_owned());
                        }
                    }
                    if event.as_deref() != Some("snapshot") {
                        continue;
                    }
                    let payload: Value =
                        serde_json::from_str(&data.expect("a snapshot has data")).unwrap();
                    if wanted(&payload) {
                        return payload;
                    }
                    seen.push(summary(&payload));
                }
                let chunk = self
                    .resp
                    .chunk()
                    .await
                    .unwrap()
                    .expect("stream ended early");
                self.buf.extend_from_slice(&chunk);
            }
        })
        .await;
        read.unwrap_or_else(|_| panic!("no wanted snapshot arrived; saw {seen:?}"))
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// A snapshot's row count and `capped` field, for failure messages: a
/// 10,000-row payload is not worth printing whole.
fn summary(payload: &Value) -> String {
    format!(
        "rows={} capped={:?}",
        rows(payload).len(),
        payload.get("capped")
    )
}

fn rows(payload: &Value) -> &Vec<Value> {
    payload["rows"].as_array().expect("rows is an array")
}

/// The count a snapshot holds for `host`, read through the shared frame
/// type: the rows are positional and the counts typed `integer`.
fn count_of(payload: &Value, host: &str) -> Option<u64> {
    use trawl_api::value::Value as Cell;

    let frame: trawl_api::stream::SnapshotFrame =
        serde_json::from_value(payload.clone()).expect("a snapshot frame decodes");
    let place = |name: &str| {
        frame
            .result
            .columns
            .iter()
            .position(|c| c.name == name)
            .unwrap_or_else(|| panic!("no {name} column"))
    };
    let (host_at, count_at) = (place("host"), place("count"));
    frame
        .result
        .rows
        .iter()
        .find(|row| row[host_at] == Cell::String(host.to_owned()))
        .and_then(|row| match row[count_at] {
            Cell::Integer(n) => u64::try_from(n).ok(),
            Cell::UInt(n) => Some(n),
            ref other => panic!("count is not an integer cell: {other:?}"),
        })
}

fn groups_cap() -> Value {
    json!([{"cap": "groups", "limit": MAX_GROUPS}])
}

/// `count` events for `service`, one per host `{prefix}{i}`.
fn hosts(service: &str, prefix: &str, range: std::ops::Range<usize>) -> Vec<Value> {
    range
        .map(|i| json!({"service": service, "host": format!("{prefix}{i}"), "message": "m"}))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn sse_snapshot_reports_groups_cap() {
    let server = setup().await;
    let ingest = HttpClient::new_insecure(&server.url, &server.ingest_token).unwrap();
    let mut stream =
        SnapshotReader::open(&server, "service=caps-svc | stats count() by host").await;

    let records = hosts("caps-svc", "h", 0..MAX_GROUPS + 1);
    assert_eq!(
        ingest.ingest(&records).await.unwrap().accepted,
        MAX_GROUPS + 1
    );

    let snapshot = stream
        .snapshot_where(|payload| payload.get("capped").is_some())
        .await;
    assert_eq!(snapshot["capped"], groups_cap(), "{}", summary(&snapshot));
    assert_eq!(rows(&snapshot).len(), MAX_GROUPS, "the cap held");
}

#[tokio::test(flavor = "multi_thread")]
async fn sse_snapshot_omits_capped_under_limits() {
    let server = setup().await;
    let ingest = HttpClient::new_insecure(&server.url, &server.ingest_token).unwrap();
    let mut stream = SnapshotReader::open(
        &server,
        "service=caps-svc | stats count(), dc(message), median(n) by host",
    )
    .await;

    let records: Vec<Value> = (0..3)
        .map(|i| json!({"service": "caps-svc", "host": format!("h{i}"), "message": "m", "n": i}))
        .collect();
    assert_eq!(ingest.ingest(&records).await.unwrap().accepted, 3);

    let snapshot = stream
        .snapshot_where(|payload| rows(payload).len() == 3)
        .await;
    let object = snapshot.as_object().expect("a snapshot is an object");
    assert!(
        !object.contains_key("capped"),
        "nothing dropped, so no field: {snapshot}"
    );
    assert_eq!(object.len(), 2, "columns and rows only: {snapshot}");
}

/// Reaching the cap exactly reports nothing. The one event that is then
/// dropped is the last matching input, and no threshold of further events
/// follows it, so only the scheduled snapshot can disclose it, and it
/// must. A later event that drops nothing still sees the report.
#[tokio::test(flavor = "multi_thread")]
async fn sse_scheduled_snapshot_discloses_a_cap_with_no_later_input() {
    let server = setup().await;
    let ingest = HttpClient::new_insecure(&server.url, &server.ingest_token).unwrap();
    let mut stream =
        SnapshotReader::open(&server, "service=caps-svc | stats count() by host").await;

    let full = hosts("caps-svc", "h", 0..MAX_GROUPS);
    assert_eq!(ingest.ingest(&full).await.unwrap().accepted, MAX_GROUPS);
    let at_cap = stream
        .snapshot_where(|payload| rows(payload).len() == MAX_GROUPS)
        .await;
    assert!(
        at_cap.get("capped").is_none(),
        "exactly at the cap drops nothing: {}",
        summary(&at_cap)
    );

    // One new group, far below the event threshold: the dropped event is
    // the last input this stream sees.
    let overflow = hosts("caps-svc", "late", 0..1);
    assert_eq!(ingest.ingest(&overflow).await.unwrap().accepted, 1);
    let disclosed = stream
        .snapshot_where(|payload| payload.get("capped").is_some())
        .await;
    assert_eq!(disclosed["capped"], groups_cap(), "{}", summary(&disclosed));
    assert_eq!(rows(&disclosed).len(), MAX_GROUPS);
    assert_eq!(count_of(&disclosed, "late0"), None, "the new group dropped");

    // A known group still counts, and the report stays.
    let known = hosts("caps-svc", "h", 0..1);
    assert_eq!(ingest.ingest(&known).await.unwrap().accepted, 1);
    let later = stream
        .snapshot_where(|payload| count_of(payload, "h0") == Some(2))
        .await;
    assert_eq!(later["capped"], groups_cap(), "{}", summary(&later));
}

/// The batch lane has no snapshot to carry a report, so a Rust-tail
/// aggregation that would drop input refuses with the row cap's contract:
/// 400 `result_too_large`, naming the cap and its limit.
#[tokio::test(flavor = "multi_thread")]
async fn kv_tail_over_group_cap_refuses_over_http() {
    let server = setup().await;
    let ingest = HttpClient::new_insecure(&server.url, &server.ingest_token).unwrap();
    let analyst = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let records: Vec<Value> = (0..=MAX_GROUPS)
        .map(|i| json!({"service": "kvcap-svc", "host": "web01", "message": format!("k=v{i}")}))
        .collect();
    assert_eq!(
        ingest.ingest(&records).await.unwrap().accepted,
        MAX_GROUPS + 1
    );

    let refused = analyst
        .query_paginated(
            "service=kvcap-svc | extract kv | stats count() by k",
            None,
            None,
        )
        .await;
    match refused.expect_err("a tail past the group cap must not answer") {
        trawl_client::ClientError::Server { status, error } => {
            assert_eq!(status, 400);
            assert_eq!(error.code, trawl_api::ErrorCode::ResultTooLarge);
            assert_eq!(
                error.message,
                "result too large: the aggregation has more than 10000 groups; narrow the query"
            );
        }
        other => panic!("expected a 400 result_too_large, got {other:?}"),
    }
}
