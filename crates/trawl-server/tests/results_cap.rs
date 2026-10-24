// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A capture over its cap (ADR-0057): an event-shaped result keeps its most
//! recent rows, newest first, with the exact total; a whole-shaped one is
//! refused rather than cut.
//!
//! The 20,000-event corpus is two hourly files of 10,000. Events come in
//! twins that share a `_time` and differ by a millisecond of `_ingested`,
//! and the twins are offset so the 5000-row cut falls between the two of
//! one pair: which twin is kept is decided by `_ingested` alone. The test
//! computes the expected rows from the same formula the seed uses.
//!
//! The binary is in the ADR-0021 `postgres` nextest group.

mod common;

use std::path::Path;

use chrono::{DateTime, TimeDelta, Utc};
use common::TestServer;
use common::captured::{self, at};
use reqwest::StatusCode;
use serde_json::json;
use trawl_api::results::{ResultPage, ResultShape};
use trawl_api::value::{Value, wire_cell_len};
use trawl_config::CaptureConfig;
use trawl_server::config::RateLimitConfig;

/// The service of every seeded event here; no shared seed row has it.
const SERVICE: &str = "capturecap";

/// Events per hourly file.
const PER_FILE: usize = 10_000;

/// The seeded hours of 2024-01-16.
const HOURS: [u32; 2] = [10, 11];

/// Distinct hosts across the corpus: more groups than a capture keeps.
const HOSTS: usize = 6_000;

/// Distinct severities across the corpus: a whole result that fits.
const SEVERITIES: usize = 7;

/// The window every capture here covers.
fn window() -> serde_json::Value {
    json!({ "earliest": "2024-01-16T10:00:00Z", "latest": "2024-01-16T12:00:00Z" })
}

/// Write `select`, a query over the shared fixture's columns, as
/// `{data}/prod/2024-01-16/{hour}/{SERVICE}.parquet`.
fn seed_file(data: &str, hour: u32, select: &str) {
    let dir = Path::new(data).join(format!("prod/2024-01-16/{hour:02}"));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{SERVICE}.parquet"));
    let conn = duckdb::Connection::open_in_memory().expect("open duckdb");
    conn.execute_batch(&format!(
        "COPY ({select}) TO '{}' (FORMAT PARQUET)",
        path.display()
    ))
    .unwrap();
}

/// Event `i` of the file for `hour`: its `_time` and its `_ingested`.
/// Twins `2k - 1` and `2k` share a `_time`; the odd one was ingested a
/// millisecond later.
fn pair(hour: u32, i: usize) -> (DateTime<Utc>, DateTime<Utc>) {
    let base = at(&format!("2024-01-16T{hour:02}:00:00Z"));
    let twin = i64::try_from(i.div_ceil(2)).unwrap();
    let time = base + TimeDelta::milliseconds(twin * 600);
    let ingested =
        time + TimeDelta::seconds(10) + TimeDelta::milliseconds(i64::try_from(i % 2).unwrap());
    (time, ingested)
}

/// The SQL of [`pair`]'s formula, over `range(0, PER_FILE) t(i)`.
fn corpus_select(hour: u32, first: usize) -> String {
    let base = format!("TIMESTAMP '2024-01-16 {hour:02}:00:00'");
    let time = format!("({base} + to_milliseconds(((i + 1) // 2) * 600))");
    format!(
        "SELECT {time} AS _time,
                {time} + to_seconds(10) + to_milliseconds(i % 2) AS _ingested,
                'event ' || (i + {first}) AS _raw,
                CAST(NULL AS VARCHAR) AS _repairs,
                'prod' AS env,
                '{SERVICE}' AS service,
                'h' || ((i + {first}) % {HOSTS}) AS host,
                CAST((i + {first}) % {SEVERITIES} AS BIGINT) AS severity,
                'info' AS severity_text,
                'event ' || (i + {first}) AS message
         FROM range(0, {PER_FILE}) t(i)"
    )
}

/// The shared seed plus the 20,000-event corpus.
async fn boot() -> TestServer {
    let tmp = tempfile::tempdir().unwrap();
    let data = common::seed_data_root(tmp.path());
    for (file, hour) in HOURS.into_iter().enumerate() {
        seed_file(&data, hour, &corpus_select(hour, file * PER_FILE));
    }
    let server = common::setup_with_capture(
        tmp.path(),
        data,
        RateLimitConfig::default(),
        CaptureConfig::default(),
    )
    .await;
    std::mem::forget(tmp);
    server.results_clock.set(at("2024-01-16T12:00:00Z"));
    server
}

fn code(body: &serde_json::Value) -> &str {
    body["error"]["code"].as_str().unwrap_or_default()
}

fn instant(value: &Value) -> DateTime<Utc> {
    match value {
        Value::String(text) => at(text),
        other => panic!("a timestamp cell is a string, got {other:?}"),
    }
}

/// An event capture past the row cap keeps exactly the 5000 most recent
/// events, ordered `_time DESC, _ingested DESC` on every page and across
/// every page boundary, with the exact total of the one execution.
#[tokio::test(flavor = "multi_thread")]
async fn an_event_capture_keeps_the_most_recent_rows_newest_first() {
    let server = boot().await;
    let max_rows = CaptureConfig::default().max_rows;

    let body = json!({ "query": format!("service={SERVICE}"), "window": window() });
    let result = captured::captured(&server, &server.analyst_token, &body)
        .await
        .result;
    assert_eq!(result.shape, ResultShape::Events);
    assert_eq!(result.captured_rows, max_rows);
    assert_eq!(result.total_rows, (PER_FILE * HOURS.len()) as u64);
    assert!(result.truncated);
    assert!(result.wire_bytes <= CaptureConfig::default().max_wire_bytes);
    assert!(
        result.executed_query.ends_with("| sort -_time, -_ingested"),
        "{:?}",
        result.executed_query
    );

    let mut kept = Vec::new();
    let page_rows = CaptureConfig::default().page_max_rows;
    for offset in (0..max_rows).step_by(usize::try_from(page_rows).unwrap()) {
        let (status, _, page) = captured::page(
            &server,
            &server.analyst_token,
            &result.id,
            &format!("?offset={offset}&limit={page_rows}&columns=_time,_ingested"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        let page: ResultPage = serde_json::from_value(page).unwrap();
        assert_eq!(
            page.page
                .columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            ["_time", "_ingested"]
        );
        kept.extend(
            page.page
                .rows
                .iter()
                .map(|row| (instant(&row[0]), instant(&row[1]))),
        );
    }

    assert!(
        kept.windows(2).all(|two| two[0] >= two[1]),
        "every row is no newer than the one before it, page boundaries included"
    );
    let mut expected: Vec<_> = HOURS
        .into_iter()
        .flat_map(|hour| (0..PER_FILE).map(move |i| pair(hour, i)))
        .collect();
    expected.sort_unstable_by(|a, b| b.cmp(a));
    expected.truncate(usize::try_from(max_rows).unwrap());
    assert_eq!(kept.first(), expected.first(), "the newest event leads");
    // The cut splits a twin pair: the kept twin is the later-ingested one.
    let (last_time, last_ingested) = *expected.last().unwrap();
    let cut_twin = (last_time, last_ingested - TimeDelta::milliseconds(1));
    assert!(!expected.contains(&cut_twin), "the cut splits a pair");
    assert_eq!(kept, expected, "exactly the most recent events, in order");
}

/// An aggregate with more groups than a capture keeps is refused with
/// `result_too_large` and the hint, and leaves nothing behind.
#[tokio::test(flavor = "multi_thread")]
async fn an_aggregate_over_the_cap_is_refused_with_the_hint() {
    let server = boot().await;
    let before = server.state.query.pool.available_permits();

    let body = json!({
        "query": format!("service={SERVICE} | stats count() by host"),
        "window": window(),
    });
    let (status, headers, answer) = captured::capture(&server, &server.analyst_token, &body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
    assert_eq!(code(&answer), "result_too_large", "{answer}");
    let message = answer["error"]["message"].as_str().unwrap();
    assert!(
        message.contains(&format!("{HOSTS} rows")),
        "names the rows: {message}"
    );
    assert!(
        message.contains("aggregate further or narrow the window"),
        "carries the hint: {message}"
    );
    assert!(headers.get("x-request-id").is_some());
    captured::assert_nothing_held(&server, before, "a refused aggregate").await;
    assert!(
        captured::root_names(&server).is_empty(),
        "no file: {:?}",
        captured::root_names(&server)
    );
    assert!(
        captured::listed(&server, &server.analyst_token)
            .await
            .is_empty()
    );
}

/// A whole result within both caps is captured whole: every group, no
/// imposed order, nothing truncated.
#[tokio::test(flavor = "multi_thread")]
async fn a_whole_result_within_the_caps_is_captured_whole() {
    let server = boot().await;

    let body = json!({
        "query": format!("service={SERVICE} | stats count() as n by severity"),
        "window": window(),
    });
    let result = captured::captured(&server, &server.analyst_token, &body)
        .await
        .result;
    assert_eq!(result.shape, ResultShape::Whole);
    assert_eq!(result.captured_rows, SEVERITIES as u64);
    assert_eq!(result.total_rows, SEVERITIES as u64);
    assert!(!result.truncated);
    assert!(
        !result.executed_query.contains("sort -_time"),
        "{:?}",
        result.executed_query
    );

    let (status, _, page) =
        captured::page(&server, &server.analyst_token, &result.id, "?columns=n").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let page: ResultPage = serde_json::from_value(page).unwrap();
    let counted: u64 = page
        .page
        .rows
        .iter()
        .map(|row| match &row[0] {
            Value::Integer(n) => u64::try_from(*n).unwrap(),
            Value::UInt(n) => *n,
            other => panic!("a count is an integer, got {other:?}"),
        })
        .sum();
    assert_eq!(counted, (PER_FILE * HOURS.len()) as u64);
}

/// Events of 64 KiB cells reach `max_wire_bytes` long before `max_rows`:
/// the capture keeps the rows before the first that does not fit, and its
/// `wire_bytes` is the sum of the kept cells as the wire encodes them.
#[tokio::test(flavor = "multi_thread")]
async fn wide_cells_reach_the_wire_cap_before_the_row_cap() {
    const EVENTS: usize = 100;
    const CELL: usize = 64 * 1024 - 64;
    let tmp = tempfile::tempdir().unwrap();
    let data = common::seed_data_root(tmp.path());
    // Every row encodes to the same length: fixed-width times and text.
    seed_file(
        &data,
        10,
        &format!(
            "SELECT TIMESTAMP '2024-01-16 10:00:00' + to_seconds(i) AS _time,
                    TIMESTAMP '2024-01-16 10:00:10' + to_seconds(i) AS _ingested,
                    'wide ' || lpad(CAST(i AS VARCHAR), 3, '0') AS _raw,
                    CAST(NULL AS VARCHAR) AS _repairs,
                    'prod' AS env,
                    '{SERVICE}' AS service,
                    'web01' AS host,
                    CAST(9 AS BIGINT) AS severity,
                    'info' AS severity_text,
                    repeat('x', {CELL}) || lpad(CAST(i AS VARCHAR), 3, '0') AS message
             FROM range(0, {EVENTS}) t(i)"
        ),
    );
    let server = common::setup_with_capture(
        tmp.path(),
        data,
        RateLimitConfig::default(),
        CaptureConfig::default(),
    )
    .await;
    std::mem::forget(tmp);
    server.results_clock.set(at("2024-01-16T12:00:00Z"));
    let capture = CaptureConfig::default();

    let body = json!({ "query": format!("service={SERVICE}"), "window": window() });
    let result = captured::captured(&server, &server.analyst_token, &body)
        .await
        .result;
    assert_eq!(result.shape, ResultShape::Events);
    assert_eq!(result.total_rows, EVENTS as u64);
    assert!(result.truncated);
    assert!(result.captured_rows > 0);
    assert!(result.captured_rows < EVENTS as u64);
    assert!(result.captured_rows < capture.max_rows);
    assert!(result.wire_bytes <= capture.max_wire_bytes);

    let (status, _, page) = captured::page(&server, &server.analyst_token, &result.id, "").await;
    assert_eq!(status, StatusCode::OK);
    let page: ResultPage = serde_json::from_value(page).unwrap();
    assert_eq!(page.page.rows.len() as u64, result.captured_rows);
    let row_len = |row: &[Value]| -> u64 {
        page.page
            .columns
            .iter()
            .zip(row)
            .map(|(column, value)| wire_cell_len(&column.name, &column.ty, value).unwrap() as u64)
            .sum()
    };
    let lengths: Vec<u64> = page.page.rows.iter().map(|row| row_len(row)).collect();
    assert_eq!(
        lengths.iter().sum::<u64>(),
        result.wire_bytes,
        "wire_bytes is the kept cells as the wire encodes them"
    );
    assert!(
        lengths.iter().all(|&len| len == lengths[0]) && lengths[0] > CELL as u64,
        "every row is one wide cell and the same length"
    );
    assert!(
        result.wire_bytes + lengths[0] > capture.max_wire_bytes,
        "the next row would not have fit"
    );
    // Newest first, as for any event capture.
    let times: Vec<DateTime<Utc>> = page.page.rows.iter().map(|row| instant(&row[0])).collect();
    assert_eq!(times[0], at("2024-01-16T10:01:39Z"));
    assert!(times.windows(2).all(|two| two[0] > two[1]));
}
