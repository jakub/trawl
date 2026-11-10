// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The whole-result histogram over a real server (#197).
//!
//! Events go in through `/api/v1/ingest` and come back through
//! `/api/v1/query`, read by real `DuckDB` over the hot buffer. A request
//! that asks with `histogram: true` gets every row of the execution
//! counted by `_time`, in the same response as its page.

mod common;

use common::{TestServer, setup_in_dir_with_data};
use trawl_api::value::{ColumnType, Value};
use trawl_api::{EventHistogram, HistogramBucket, HistogramReason, QueryRequest, QueryResponse};
use trawl_server::config::RateLimitConfig;

/// 2026-01-15T09:00:00Z, the first seeded event's `_time`, in microseconds.
const BASE_MICROS: i64 = 1_768_467_600_000_000;

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

/// The `_time` text of the `i`th seeded event: `i` seconds past
/// [`BASE_MICROS`].
fn sent_at(i: usize) -> String {
    format!("2026-01-15T09:{:02}:{:02}Z", i / 60, i % 60)
}

/// `count` events one second apart, oldest first, on hosts `h0`-`h2`.
/// Event `i` carries `level=error` when `error(i)`, else `level=info`.
fn events(count: usize, error: impl Fn(usize) -> bool) -> Vec<serde_json::Value> {
    (0..count)
        .map(|i| {
            serde_json::json!({
                "service": "histo",
                "host": format!("h{}", i % 3),
                "timestamp": sent_at(i),
                "level": if error(i) { "error" } else { "info" },
                "message": format!("event {i}"),
            })
        })
        .collect()
}

/// `count` kv-shaped events, one second apart from [`BASE_MICROS`].
fn kv_events(count: usize) -> Vec<serde_json::Value> {
    (0..count)
        .map(|i| {
            serde_json::json!({
                "service": "histo",
                "host": format!("h{}", i % 3),
                "timestamp": sent_at(i),
                "message": format!("user=u{i} code={}", 200 + i % 5),
            })
        })
        .collect()
}

/// A request for `dsl` that asks for the histogram.
fn asking(dsl: &str, limit: Option<usize>, offset: Option<usize>) -> QueryRequest {
    QueryRequest {
        query: dsl.to_owned(),
        limit,
        offset,
        histogram: true,
    }
}

/// POST `body` to `/api/v1/query` as the analyst: the status and the body
/// text.
async fn post(server: &TestServer, body: &serde_json::Value) -> (u16, String) {
    let response = common::harness_client_builder()
        .build()
        .unwrap()
        .post(format!("{}/api/v1/query", server.url))
        .bearer_auth(&server.analyst_token)
        .json(body)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.text().await.unwrap())
}

/// Run `request`, require a 200, and decode the answer. The raw JSON comes
/// back beside it, for the keys a test checks are absent.
async fn query(server: &TestServer, request: &QueryRequest) -> (QueryResponse, serde_json::Value) {
    let (status, body) = post(server, &serde_json::to_value(request).unwrap()).await;
    assert_eq!(status, 200, "{}: {body}", request.query);
    let answer =
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("{}: {e}: {body}", request.query));
    let raw = serde_json::from_str(&body).unwrap();
    (answer, raw)
}

/// A ready histogram's fields, after checking it against the answer's
/// own `total`.
struct Ready {
    start: i64,
    end: i64,
    buckets: Vec<HistogramBucket>,
    undated: u64,
    severity: bool,
    kept_some: bool,
}

impl Ready {
    fn events(&self) -> u64 {
        self.buckets.iter().map(|b| b.events).sum()
    }

    fn errors(&self) -> u64 {
        self.buckets.iter().map(|b| b.errors).sum()
    }
}

fn ready(dsl: &str, answer: &QueryResponse) -> Ready {
    let histogram = answer
        .histogram
        .clone()
        .unwrap_or_else(|| panic!("{dsl}: a histogram"));
    assert!(
        histogram.consistent_with(answer.pagination.total),
        "{dsl}: {histogram:?} against total {}",
        answer.pagination.total
    );
    match histogram {
        EventHistogram::Ready {
            start,
            end,
            buckets,
            undated,
            severity,
            kept_some,
        } => {
            let ready = Ready {
                start: trawl_api::zone::wire_micros(&start).unwrap(),
                end: trawl_api::zone::wire_micros(&end).unwrap(),
                buckets,
                undated,
                severity,
                kept_some,
            };
            assert_eq!(
                u128::from(ready.events()) + u128::from(ready.undated),
                answer.pagination.total as u128,
                "{dsl}: every row is counted"
            );
            ready
        }
        EventHistogram::Unavailable { reason } => panic!("{dsl}: unavailable: {reason:?}"),
    }
}

fn reason(dsl: &str, answer: &QueryResponse) -> HistogramReason {
    match &answer.histogram {
        Some(EventHistogram::Unavailable { reason }) => *reason,
        other => panic!("{dsl}: a reason, got {other:?}"),
    }
}

/// The premise of #197: a Rust-tail result keeps `_time` typed
/// `timestamp`, with every non-null cell in the wire spelling
/// `wire_micros` reads, so the counter needs no reader for `DuckDB`'s
/// cast text. A kv tail asked for its histogram is counted whole.
#[tokio::test(flavor = "multi_thread")]
async fn histogram_reads_a_rust_tail_result() {
    let server = server().await;
    ingest(&server, &kv_events(12)).await;

    let dsl = "* | extract kv";
    let (answer, _) = query(&server, &asking(dsl, Some(1_000), None)).await;
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

    let counted = ready(dsl, &answer);
    assert_eq!(counted.events(), 12);
    assert_eq!(counted.undated, 0);
    assert_eq!(counted.start, BASE_MICROS);
    assert!(counted.end > BASE_MICROS + 11_000_000);
}

/// 120 events whose 20 errors are all older than the newest 60: every
/// page of 50 answers the same whole-result count, errors included, and
/// only its own rows. The page past the end still counts the result.
#[tokio::test(flavor = "multi_thread")]
async fn histogram_counts_rows_before_the_page_is_cut() {
    let server = server().await;
    let erroring = |i: usize| i < 60 && i.is_multiple_of(3);
    ingest(&server, &events(120, erroring)).await;
    let seeded_errors = (0..120).filter(|&i| erroring(i)).count() as u64;
    assert_eq!(seeded_errors, 20);

    // An explicit order, so each page's own rows are known: newest first,
    // row `r` is event `119 - r`.
    let dsl = "* | sort -_time";
    let mut first_bytes = None;
    for offset in [0_usize, 50, 100, 150] {
        let (answer, _) = query(&server, &asking(dsl, Some(50), Some(offset))).await;
        let label = format!("{dsl} at offset {offset}");
        assert_eq!(answer.pagination.total, 120, "{label}");
        assert_eq!(
            answer.pagination.returned,
            50.min(120_usize.saturating_sub(offset)),
            "{label}"
        );

        let counted = ready(&label, &answer);
        assert_eq!(counted.events() + counted.undated, 120, "{label}");
        assert_eq!(counted.errors(), seeded_errors, "{label}");
        assert!(counted.severity, "{label}");
        assert!(!counted.kept_some, "{label}");
        assert_eq!(counted.start, BASE_MICROS, "{label}: the oldest event");
        assert!(counted.end > BASE_MICROS + 119_000_000, "{label}");

        let bytes = serde_json::to_string(answer.histogram.as_ref().unwrap()).unwrap();
        match &first_bytes {
            None => first_bytes = Some(bytes),
            Some(first) => assert_eq!(&bytes, first, "{label}: byte-equal to page 1's"),
        }

        let columns = &answer.result.columns;
        let time = columns.iter().position(|c| c.name == "_time").unwrap();
        let severity = columns.iter().position(|c| c.name == "_severity").unwrap();
        let times: Vec<i64> = answer
            .result
            .rows
            .iter()
            .map(|row| match &row[time] {
                Value::String(text) => trawl_api::zone::wire_micros(text).unwrap(),
                other => panic!("{label}: {other:?}"),
            })
            .collect();
        let own: Vec<i64> = (offset..(offset + 50).min(120))
            .map(|r| BASE_MICROS + i64::try_from(119 - r).unwrap() * 1_000_000)
            .collect();
        assert_eq!(times, own, "{label}: only its own rows");
        if offset == 0 {
            assert!(
                answer
                    .result
                    .rows
                    .iter()
                    .all(|row| !matches!(row[severity], Value::Integer(n) if n >= 17)),
                "page 1 holds no error, yet its histogram counts all {seeded_errors}"
            );
        }
    }
}

/// The count is of the final result: `head`, `tail` and `dedup` count
/// what they kept and say so, and `sort` keeps every row.
#[tokio::test(flavor = "multi_thread")]
async fn histogram_counts_the_final_result() {
    let server = server().await;
    ingest(&server, &events(120, |_| false)).await;

    for (dsl, rows, kept_some) in [
        ("* | head 10", 10, true),
        ("* | limit 7", 7, true),
        ("* | tail 5", 5, true),
        ("* | dedup host", 3, true),
        ("* | sort -_time", 120, false),
    ] {
        let (answer, _) = query(&server, &asking(dsl, Some(1_000), None)).await;
        assert_eq!(answer.pagination.total, rows, "{dsl}");
        let counted = ready(dsl, &answer);
        assert_eq!(counted.events() + counted.undated, rows as u64, "{dsl}");
        assert_eq!(counted.kept_some, kept_some, "{dsl}");
    }
}

/// A sampled result is refused by name in both forms; a result without a
/// timestamp `_time` names that; a projection that keeps `_time` but drops
/// `_severity` is counted without severity; an aggregation carries no
/// histogram key at all.
#[tokio::test(flavor = "multi_thread")]
async fn histogram_refuses_shapes_with_a_reason() {
    let server = server().await;
    ingest(&server, &events(12, |i| i == 4)).await;

    for dsl in ["* | sample 5", "* | sample 50%"] {
        let (answer, _) = query(&server, &asking(dsl, None, None)).await;
        assert_eq!(reason(dsl, &answer), HistogramReason::Sample);
    }

    let dsl = "* | fields host";
    let (answer, _) = query(&server, &asking(dsl, None, None)).await;
    assert_eq!(answer.pagination.total, 12);
    assert_eq!(reason(dsl, &answer), HistogramReason::NoTimeColumn);

    let dsl = "* | fields _time, host";
    let (answer, _) = query(&server, &asking(dsl, None, None)).await;
    let counted = ready(dsl, &answer);
    assert!(!counted.severity, "`_severity` was projected away");
    assert_eq!(counted.errors(), 0);
    assert_eq!(counted.events(), 12);

    for dsl in ["* | stats count()", "* | timechart span=1m count()"] {
        let (answer, raw) = query(&server, &asking(dsl, None, None)).await;
        assert!(answer.pagination.total > 0, "{dsl}");
        assert!(raw.get("histogram").is_none(), "{dsl}: {raw}");
    }
}

/// The flag is opt-in: a request without it, or with it false, gets no
/// histogram key, and a false flag is not sent.
#[tokio::test(flavor = "multi_thread")]
async fn histogram_is_absent_unless_asked() {
    let server = server().await;
    ingest(&server, &events(6, |_| false)).await;

    let unasked = QueryRequest {
        query: "*".to_owned(),
        limit: None,
        offset: None,
        histogram: false,
    };
    let sent = serde_json::to_value(&unasked).unwrap();
    assert_eq!(
        sent,
        serde_json::json!({"query": "*"}),
        "a false flag is not sent"
    );

    for body in [
        serde_json::json!({"query": "*"}),
        serde_json::json!({"query": "*", "histogram": false}),
    ] {
        let (status, text) = post(&server, &body).await;
        assert_eq!(status, 200, "{body}: {text}");
        let raw: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(raw["pagination"]["total"], 6, "{body}");
        assert!(raw.get("histogram").is_none(), "{body}: {raw}");
    }

    let client = trawl_client::HttpClient::new_insecure(&server.url, &server.analyst_token)
        .expect("an analyst client");
    let answer = client.query_paginated("*", None, None).await.unwrap();
    assert_eq!(answer.histogram, None, "the CLI's client does not ask");

    let (answer, raw) = query(&server, &asking("*", None, None)).await;
    assert!(raw.get("histogram").is_some(), "{raw}");
    assert_eq!(ready("*", &answer).events(), 6);
}

/// A result at `max_result_rows` is counted. One row more is refused as
/// too large, as it always was, and the refusal carries no histogram.
#[tokio::test(flavor = "multi_thread")]
async fn histogram_obeys_the_row_cap() {
    let server = common::setup_with_row_caps(common::RowCaps {
        max_result_rows: 12,
        max_export_rows: 1_000,
    })
    .await;
    let mut seeded = events(13, |i| i == 0);
    for (i, event) in seeded.iter_mut().enumerate() {
        event["service"] = "histocap".into();
        event["group"] = if i < 12 { "a" } else { "b" }.into();
    }
    ingest(&server, &seeded).await;

    let dsl = "service=histocap group=a";
    let (answer, _) = query(&server, &asking(dsl, None, None)).await;
    assert_eq!(answer.pagination.total, 12);
    let counted = ready(dsl, &answer);
    assert_eq!(counted.events(), 12);
    assert_eq!(counted.errors(), 1);

    let over = serde_json::to_value(asking("service=histocap", None, None)).unwrap();
    let (status, text) = post(&server, &over).await;
    assert_eq!(status, 400, "{text}");
    let raw: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(raw["error"]["code"], "result_too_large", "{raw}");
    assert!(raw.get("histogram").is_none(), "{raw}");
}
