// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Live frames speak the result wire (ADR-0035, issue #214 AC 6).
//!
//! A pass-through stream sends each event as a one-row typed result, its
//! cells conformed under the stream's output pin scope first, so a field
//! the catalog pins BIGINT arrives as the integer a batch query answers,
//! not as the text a sender wrote. An aggregate stream's snapshot carries
//! typed columns beside `capped`. These run the real SSE loop on a live
//! server with real ingest and a pin the real compaction path wrote, and
//! read the frames twice: as bytes off the socket, and through
//! `trawl_client::HttpClient::stream_events`, the decoder the CLI and the
//! TUI use.

mod common;

use std::time::Duration;

use common::{TestServer, setup_in_dir_with_data};
use serde_json::json;
use tokio_stream::StreamExt as _;
use trawl_api::stream::{SnapshotCap, SnapshotCapKind};
use trawl_api::value::{Column, ColumnType, QueryResult, Value};
use trawl_client::{HttpClient, StreamEvent};
use trawl_server::catalog::CatalogContext;
use trawl_server::config::RateLimitConfig;

/// The live group cap, as the wire states it.
const MAX_GROUPS: usize = 10_000;

/// 2^53 + 1: the first integer a double cannot hold, so a frame that
/// carried it as a JSON number would hand a JavaScript reader 2^53.
const PAST_DOUBLE: i64 = 9_007_199_254_740_993;

struct Harness {
    server: TestServer,
    wal_dir: std::path::PathBuf,
    data_dir: std::path::PathBuf,
    ingest: HttpClient,
    analyst: HttpClient,
}

async fn harness() -> Harness {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let wal_dir = root.join("wal");
    let data_dir = root.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let data_path = data_dir.to_str().unwrap().to_owned();

    let server = setup_in_dir_with_data(&root, data_path, RateLimitConfig::default()).await;
    // Leak the tempdir so it survives the server (cleaned up by OS).
    std::mem::forget(tmp);

    let ingest = HttpClient::new_insecure(&server.url, &server.ingest_token).unwrap();
    let analyst = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    Harness {
        server,
        wal_dir,
        data_dir,
        ingest,
        analyst,
    }
}

/// One compaction tick over the server's WAL, with the catalog context
/// trawld's main wires, so the batch's types become pins.
async fn compact_tick(h: &Harness) {
    let hot_buffer = h
        .server
        .state
        .query
        .hot_buffer
        .as_ref()
        .expect("ingest-enabled server has a hot buffer");
    let ctx = CatalogContext {
        store: h.server.state.storage.catalog.clone(),
        cache: h.server.state.query.field_catalog.clone(),
    };
    let errors = trawl_server::ingest::compaction::compact_once(
        &h.wal_dir,
        &h.data_dir,
        Duration::ZERO,
        false,
        Some(hot_buffer),
        500,
        "2GB",
        Some(&ctx),
    )
    .await
    .expect("compaction tick must succeed");
    assert_eq!(errors, 0, "no compaction errors expected");
}

/// Pin `status` BIGINT the way production does: integer events,
/// compacted with the catalog wired.
async fn pin_status_bigint(h: &Harness) {
    let seed: Vec<serde_json::Value> = [200, 404, 500]
        .iter()
        .map(|status| json!({"service": "typed-seed", "host": "web01", "message": "seed", "status": status}))
        .collect();
    assert_eq!(h.ingest.ingest(&seed).await.unwrap().accepted, 3);
    compact_tick(h).await;
    assert_eq!(
        h.server.state.query.field_catalog.get("status"),
        Some(trawl_core::schema::CanonicalType::BigInt)
    );
}

/// One open stream read as raw SSE bytes.
struct RawFrames {
    resp: reqwest::Response,
    buf: Vec<u8>,
}

impl RawFrames {
    async fn open(server: &TestServer, query: &str) -> Self {
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

    /// The `data` payload of the next `event` frame whose payload contains
    /// `needle`. Panics after 30 s, naming what did arrive.
    async fn frame_with(&mut self, event: &str, needle: &str) -> String {
        let mut seen = Vec::new();
        let read = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                while let Some(end) = self.buf.windows(2).position(|w| w == b"\n\n") {
                    let frame = String::from_utf8(self.buf[..end].to_vec()).expect("utf-8 frame");
                    self.buf.drain(..end + 2);
                    let mut name = None;
                    let mut data = None;
                    for line in frame.lines() {
                        if let Some(n) = line.strip_prefix("event: ") {
                            name = Some(n.to_owned());
                        } else if let Some(d) = line.strip_prefix("data: ") {
                            data = Some(d.to_owned());
                        }
                    }
                    if let (Some(name), Some(data)) = (name, data) {
                        if name == event && data.contains(needle) {
                            return data;
                        }
                        // A 10,000-row snapshot is not worth printing whole.
                        seen.push(format!(
                            "{name}: {}",
                            data.chars().take(120).collect::<String>()
                        ));
                    }
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
        read.unwrap_or_else(|_| panic!("no {event} frame with {needle:?} arrived; saw {seen:?}"))
    }
}

/// The next decoded event from `stream_events` that `wanted` picks out,
/// skipping the rest. Panics after 30 s.
async fn next_event<S>(stream: &mut S, wanted: impl Fn(&StreamEvent) -> bool) -> StreamEvent
where
    S: tokio_stream::Stream<Item = Result<StreamEvent, trawl_client::ClientError>> + Unpin,
{
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let event = stream
                .next()
                .await
                .expect("stream ended early")
                .expect("every frame decodes");
            if wanted(&event) {
                return event;
            }
        }
    })
    .await
    .expect("the wanted event arrived")
}

/// The cell a one-row frame holds under `name`, with its column's type.
fn cell<'r>(frame: &'r QueryResult, name: &str) -> (&'r ColumnType, &'r Value) {
    let idx = frame
        .columns
        .iter()
        .position(|c| c.name == name)
        .unwrap_or_else(|| panic!("no {name} column in {frame:?}"));
    (&frame.columns[idx].ty, &frame.rows[0][idx])
}

/// The one-row event frame whose `message` is `message`.
fn is_event(message: &'static str) -> impl Fn(&StreamEvent) -> bool {
    move |event| {
        matches!(event, StreamEvent::Event(frame)
            if cell(frame, "message").1 == &Value::String(message.to_owned()))
    }
}

/// AC 6, and the first two pin cases: numeric text under a BIGINT pin
/// leaves as an `integer` column carrying the digits as a decimal string,
/// and lands as `Value::Integer`; text the conform cannot read keeps its
/// own kind, `string`. `_time` is `timestamp`, from the envelope table.
#[tokio::test(flavor = "multi_thread")]
async fn raw_frames_carry_pinned_fields_as_typed_cells() {
    let h = harness().await;
    pin_status_bigint(&h).await;

    let query = "service=typed-live";
    let mut raw = RawFrames::open(&h.server, query).await;
    let mut decoded = h
        .analyst
        .stream_events(query)
        .await
        .expect("open the stream");

    let live = vec![
        json!({"service": "typed-live", "host": "web01", "message": "pinned", "status": PAST_DOUBLE.to_string()}),
        json!({"service": "typed-live", "host": "web01", "message": "unreadable", "status": "n/a"}),
    ];
    assert_eq!(h.ingest.ingest(&live).await.unwrap().accepted, 2);

    let pinned = raw.frame_with("data", "pinned").await;
    assert!(
        pinned.contains(r#"{"name":"status","type":"integer"}"#),
        "a BIGINT-pinned field is an integer column: {pinned}"
    );
    assert!(
        pinned.contains(&format!("\"{PAST_DOUBLE}\"")),
        "the integer rides as a decimal string: {pinned}"
    );
    assert!(
        pinned.contains(r#"{"name":"_time","type":"timestamp"}"#),
        "{pinned}"
    );
    let unreadable = raw.frame_with("data", "unreadable").await;
    assert!(
        unreadable.contains(r#"{"name":"status","type":"string"}"#)
            && unreadable.contains(r#""n/a""#),
        "text the conform cannot read keeps its own kind: {unreadable}"
    );

    let StreamEvent::Event(frame) = next_event(&mut decoded, is_event("pinned")).await else {
        unreachable!()
    };
    assert_eq!(
        cell(&frame, "status"),
        (&ColumnType::Integer, &Value::Integer(PAST_DOUBLE))
    );
    let (time_ty, time) = cell(&frame, "_time");
    assert_eq!(time_ty, &ColumnType::Timestamp);
    let Value::String(time) = time else {
        panic!("a timestamp cell is text: {time:?}")
    };
    assert!(
        chrono::DateTime::parse_from_rfc3339(time).is_ok() && time.ends_with('Z'),
        "{time}"
    );

    let StreamEvent::Event(frame) = next_event(&mut decoded, is_event("unreadable")).await else {
        unreachable!()
    };
    assert_eq!(
        cell(&frame, "status"),
        (&ColumnType::String, &Value::String("n/a".to_owned()))
    );
}

/// The other two pin cases, discriminated in one frame over the same text
/// `"404"`: a renamed pinned field keeps its pin under the new name and
/// leaves as an integer, while a `let` that overwrites the pinned name is
/// typed by the value it derived, never by the pin it replaced.
#[tokio::test(flavor = "multi_thread")]
async fn renamed_and_overwritten_fields_follow_the_pin_scope() {
    let h = harness().await;
    pin_status_bigint(&h).await;

    let query = "service=typed-scope | rename status as code | let status = lower(message)";
    let mut raw = RawFrames::open(&h.server, query).await;
    let mut decoded = h
        .analyst
        .stream_events(query)
        .await
        .expect("open the stream");

    let live =
        vec![json!({"service": "typed-scope", "host": "web01", "message": "404", "status": "404"})];
    assert_eq!(h.ingest.ingest(&live).await.unwrap().accepted, 1);

    let bytes = raw.frame_with("data", "typed-scope").await;
    assert!(
        bytes.contains(r#"{"name":"code","type":"integer"}"#)
            && bytes.contains(r#"{"name":"status","type":"string"}"#),
        "{bytes}"
    );

    let StreamEvent::Event(frame) =
        next_event(&mut decoded, |e| matches!(e, StreamEvent::Event(_))).await
    else {
        unreachable!()
    };
    assert_eq!(
        cell(&frame, "code"),
        (&ColumnType::Integer, &Value::Integer(404))
    );
    assert_eq!(
        cell(&frame, "status"),
        (&ColumnType::String, &Value::String("404".to_owned()))
    );
}

/// AC 6's aggregate half: a snapshot past the group cap carries typed
/// columns, in the aggregation's declared order, beside `capped`.
#[tokio::test(flavor = "multi_thread")]
async fn snapshots_carry_typed_columns_and_capped() {
    let h = harness().await;
    let query = "service=typed-caps | stats count() by host";
    let mut raw = RawFrames::open(&h.server, query).await;
    let mut decoded = h
        .analyst
        .stream_events(query)
        .await
        .expect("open the stream");

    let records: Vec<serde_json::Value> = (0..=MAX_GROUPS)
        .map(|i| json!({"service": "typed-caps", "host": format!("h{i}"), "message": "m"}))
        .collect();
    assert_eq!(
        h.ingest.ingest(&records).await.unwrap().accepted,
        MAX_GROUPS + 1
    );

    let bytes = raw.frame_with("snapshot", r#""capped""#).await;
    assert!(
        bytes.starts_with(
            r#"{"columns":[{"name":"host","type":"string"},{"name":"count","type":"integer"}],"rows":[["#
        ),
        "{}",
        &bytes[..bytes.len().min(200)]
    );
    assert!(
        bytes.ends_with(r#"],"capped":[{"cap":"groups","limit":10000}]}"#),
        "{}",
        &bytes[bytes.len().saturating_sub(200)..]
    );

    let StreamEvent::Snapshot { result, capped } = next_event(
        &mut decoded,
        |e| matches!(e, StreamEvent::Snapshot { capped, .. } if !capped.is_empty()),
    )
    .await
    else {
        unreachable!()
    };
    assert_eq!(
        result.columns,
        vec![
            Column::new("host", ColumnType::String),
            Column::new("count", ColumnType::Integer),
        ]
    );
    assert_eq!(result.rows.len(), MAX_GROUPS, "the cap held");
    assert!(
        result.rows.iter().all(|row| row[1] == Value::Integer(1)),
        "every kept group counted its one event"
    );
    assert_eq!(
        capped,
        vec![SnapshotCap {
            cap: SnapshotCapKind::Groups,
            limit: MAX_GROUPS as u64,
        }]
    );
}
