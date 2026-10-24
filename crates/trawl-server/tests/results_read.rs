// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Reading captured results end to end (ADR-0057): the caller's list, pages
//! with a column projection, byte ranges of one cell, and the reads a
//! capture refuses.
//!
//! Every capture here names an explicit window over the capture corpus
//! (`common::captured::corpus`), so a test can move the results clock
//! freely to order or expire captures. The binary is in the ADR-0021
//! `postgres` nextest group.

mod common;

use chrono::DateTime;
use common::TestServer;
use common::captured::{self, at};
use reqwest::StatusCode;
use serde_json::json;
use trawl_api::results::{CapturedResult, ResultCell, ResultPage};
use trawl_api::value::{Value, wire_cell_text};
use trawl_config::CaptureConfig;
use trawl_server::config::RateLimitConfig;

/// Seconds a temporary capture stays readable in these tests.
const TTL_SECS: u64 = 600;

async fn boot() -> TestServer {
    captured::boot(
        RateLimitConfig::default(),
        CaptureConfig {
            ttl_secs: TTL_SECS,
            ..CaptureConfig::default()
        },
    )
    .await
}

/// Capture `query` over the whole corpus as `token`, at `clock`.
async fn capture_at(server: &TestServer, token: &str, clock: &str, query: &str) -> CapturedResult {
    server.results_clock.set(at(clock));
    let body = json!({
        "query": query,
        "window": {
            "earliest": "2024-01-16T10:00:00Z",
            "latest": "2024-01-16T10:01:00Z",
        },
    });
    captured::captured(server, token, &body).await.result
}

/// The corpus's events, captured as `token` at 10:30.
async fn events(server: &TestServer, token: &str) -> CapturedResult {
    let captured = capture_at(
        server,
        token,
        "2024-01-16T10:30:00Z",
        &format!("service={}", captured::SERVICE),
    )
    .await;
    assert_eq!(captured.captured_rows, captured::ROWS as u64);
    captured
}

fn position(result: &CapturedResult, name: &str) -> usize {
    result
        .columns
        .iter()
        .position(|c| c.name == name)
        .unwrap_or_else(|| panic!("no column {name}: {:?}", result.columns))
}

async fn page_ok(server: &TestServer, id: &str, query: &str) -> ResultPage {
    let (status, _, body) = captured::page(server, &server.analyst_token, id, query).await;
    assert_eq!(status, StatusCode::OK, "{query}: {body}");
    serde_json::from_value(body).expect("a result page")
}

fn assert_status(answer: &captured::Answer, status: StatusCode, code: &str, what: &str) {
    assert_eq!(answer.0, status, "{what}: {}", answer.2);
    assert_eq!(answer.2["error"]["code"], code, "{what}: {}", answer.2);
}

/// The list holds the caller's unexpired captures, newest first, and none
/// of another key's.
#[tokio::test(flavor = "multi_thread")]
async fn the_list_is_the_callers_live_captures_newest_first() {
    let server = boot().await;
    let query = format!("service={}", captured::SERVICE);
    let analyst = &server.analyst_token;

    let expired = capture_at(&server, analyst, "2024-01-16T10:30:00Z", &query).await;
    let expires_at = DateTime::parse_from_rfc3339(expired.expires_at.as_deref().unwrap()).unwrap();
    assert_eq!(
        expires_at,
        at("2024-01-16T10:40:00Z"),
        "a temporary capture lives for its ttl"
    );
    let older = capture_at(&server, analyst, "2024-01-16T10:41:00Z", &query).await;
    let newer = capture_at(
        &server,
        analyst,
        "2024-01-16T10:42:00Z",
        &format!("{query} | stats count() by host"),
    )
    .await;
    let others = capture_at(
        &server,
        &server.reader_token,
        "2024-01-16T10:43:00Z",
        &query,
    )
    .await;

    assert_eq!(
        captured::listed(&server, analyst).await,
        [newer.id.clone(), older.id.clone()],
        "the expired capture and the reader's are not listed"
    );
    assert_eq!(
        captured::listed(&server, &server.reader_token).await,
        [others.id]
    );
}

/// `columns` selects by display name: the page answers the selected
/// positions ascending, whatever order the names came in; a name two
/// columns share selects both; an unknown name is a 400.
#[tokio::test(flavor = "multi_thread")]
async fn columns_select_positions_by_display_name() {
    let server = boot().await;
    let result = events(&server, &server.analyst_token).await;

    let mut expected = vec![position(&result, "host"), position(&result, "_time")];
    expected.sort_unstable();
    let page = page_ok(&server, &result.id, "?columns=host,_time").await;
    assert_eq!(page.positions, expected);
    let names: Vec<&str> = page.page.columns.iter().map(|c| c.name.as_str()).collect();
    let stored: Vec<&str> = expected
        .iter()
        .map(|&p| result.columns[p].name.as_str())
        .collect();
    assert_eq!(names, stored, "the page keeps the stored order");
    assert_eq!(page.page.rows.len(), captured::ROWS);
    assert!(page.page.rows.iter().all(|row| row.len() == 2));

    let shared = capture_at(
        &server,
        &server.analyst_token,
        "2024-01-16T10:31:00Z",
        &format!("service={} | table host, _time, host", captured::SERVICE),
    )
    .await;
    let hosts: Vec<usize> = shared
        .columns
        .iter()
        .enumerate()
        .filter(|(_, c)| c.name == "host")
        .map(|(p, _)| p)
        .collect();
    assert_eq!(
        hosts.len(),
        2,
        "two columns share a name: {:?}",
        shared.columns
    );
    let page = page_ok(&server, &shared.id, "?columns=host").await;
    assert_eq!(page.positions, hosts);

    let answer = captured::page(
        &server,
        &server.analyst_token,
        &result.id,
        "?columns=host,nosuchfield",
    )
    .await;
    assert_status(
        &answer,
        StatusCode::BAD_REQUEST,
        "bad_request",
        "an unknown name",
    );
}

/// A cell read in ranges reassembles, byte for byte, into the page cell's
/// wire JSON. A range that would end inside a character is cut back to
/// the boundary, and a range may not start inside one.
#[tokio::test(flavor = "multi_thread")]
async fn a_cell_read_in_ranges_reassembles_the_page_cell() {
    let server = boot().await;
    let result = events(&server, &server.analyst_token).await;
    let column = position(&result, "message");
    let page = page_ok(&server, &result.id, "").await;
    let row = page
        .page
        .rows
        .iter()
        .position(|row| row[column] == Value::String(captured::WIDE_MESSAGE.into()))
        .expect("the wide event is captured");
    let meta = &result.columns[column];
    let expected = wire_cell_text(&meta.name, &meta.ty, &page.page.rows[row][column]).unwrap();
    assert!(expected.len() > captured::WIDE_MESSAGE.chars().count());

    let mut text = String::new();
    let mut offset = 0;
    let mut cut_back = false;
    loop {
        let (status, _, body) = captured::cell(
            &server,
            &server.analyst_token,
            &result.id,
            row as u64,
            column,
            &format!("?offset={offset}&length=4"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "offset {offset}: {body}");
        let range: ResultCell = serde_json::from_value(body).unwrap();
        assert_eq!(range.offset, offset);
        assert_eq!(range.total_bytes, expected.len() as u64);
        assert!(range.length <= 4 && range.length > 0, "{range:?}");
        cut_back |= range.length < 4 && range.next_offset.is_some();
        text.push_str(&range.text);
        match range.next_offset {
            Some(next) => offset = next,
            None => break,
        }
    }
    assert_eq!(text, expected);
    assert!(
        cut_back,
        "some range ended inside a character and was cut back"
    );

    let inside = expected.find('✓').unwrap() + 1;
    let answer = captured::cell(
        &server,
        &server.analyst_token,
        &result.id,
        row as u64,
        column,
        &format!("?offset={inside}"),
    )
    .await;
    assert_status(
        &answer,
        StatusCode::BAD_REQUEST,
        "bad_request",
        "an offset inside a character",
    );
}

/// Indexes past the capture and a page larger than the page limit are
/// refused with 400 before any file is read.
#[tokio::test(flavor = "multi_thread")]
async fn reads_past_the_capture_are_refused() {
    let server = boot().await;
    let result = events(&server, &server.analyst_token).await;
    let analyst = &server.analyst_token;
    let page_max_rows = server.state.results.capture.page_max_rows;
    let rows = result.captured_rows;
    let columns = result.columns.len();

    let cases = [
        (
            "a column past the end",
            captured::cell(&server, analyst, &result.id, 0, columns, "").await,
        ),
        (
            "a row past the end",
            captured::cell(&server, analyst, &result.id, rows, 0, "").await,
        ),
        (
            "an offset past the captured rows",
            captured::page(
                &server,
                analyst,
                &result.id,
                &format!("?offset={}", rows + 1),
            )
            .await,
        ),
        (
            "a limit above the page limit",
            captured::page(
                &server,
                analyst,
                &result.id,
                &format!("?limit={}", page_max_rows + 1),
            )
            .await,
        ),
        (
            "a parameter the route does not take",
            captured::page(&server, analyst, &result.id, "?cursor=1").await,
        ),
    ];
    for (what, answer) in &cases {
        assert_status(answer, StatusCode::BAD_REQUEST, "bad_request", what);
    }

    // The end itself is a page with no rows.
    let page = page_ok(&server, &result.id, &format!("?offset={rows}")).await;
    assert!(page.page.rows.is_empty());
    assert_eq!(page.pagination.total, captured::ROWS);
}

/// Every read needs the `query` permission.
#[tokio::test(flavor = "multi_thread")]
async fn every_read_needs_the_query_permission() {
    let server = boot().await;
    let result = events(&server, &server.analyst_token).await;
    let ingest = &server.ingest_token;

    for (what, answer) in [
        ("the list", captured::get(&server, ingest, "/results").await),
        (
            "a page",
            captured::page(&server, ingest, &result.id, "").await,
        ),
        (
            "a cell",
            captured::cell(&server, ingest, &result.id, 0, 0, "").await,
        ),
    ] {
        assert_status(&answer, StatusCode::FORBIDDEN, "forbidden", what);
    }
}

/// Another key's capture reads as one that does not exist.
#[tokio::test(flavor = "multi_thread")]
async fn another_keys_capture_is_not_found() {
    let server = boot().await;
    let result = events(&server, &server.analyst_token).await;
    let reader = &server.reader_token;
    let unknown = "0".repeat(result.id.len());

    for (what, answer) in [
        (
            "another key's page",
            captured::page(&server, reader, &result.id, "").await,
        ),
        (
            "another key's cell",
            captured::cell(&server, reader, &result.id, 0, 0, "").await,
        ),
        (
            "an unknown id",
            captured::page(&server, reader, &unknown, "").await,
        ),
        (
            "a malformed id",
            captured::page(&server, reader, "not-an-id", "").await,
        ),
    ] {
        assert_status(&answer, StatusCode::NOT_FOUND, "not_found", what);
    }
}
