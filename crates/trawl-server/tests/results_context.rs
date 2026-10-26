// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `POST /api/v1/results/context` end to end (ADR-0058): the rows around
//! a focus row, read again as a capture of kind `context`.
//!
//! The corpus sits on 2024-01-17, which no other seed touches: one event a
//! second on `prod`/`web01` from 10:00:00 to 10:00:59, two more at the
//! focus instant 10:00:30 ingested later, and neighbours on another host
//! and another env, so each scope selects a different population. Every
//! row carries its own `seq`.
//!
//! The binary is in the ADR-0021 `postgres` nextest group.

mod common;

use chrono::{DateTime, TimeDelta, Utc};
use common::TestServer;
use common::captured::{self, Row, at};
use reqwest::StatusCode;
use serde_json::json;
use trawl_api::results::{
    CONTEXT_STATEMENT, CaptureKind, CapturedResult, ContextFacts, Correlation, ResultPage, RowRange,
};
use trawl_api::value::Value;
use trawl_config::CaptureConfig;

const SERVICE: &str = "ctxcorpus";

fn second(n: i64) -> DateTime<Utc> {
    at("2024-01-17T10:00:00Z") + TimeDelta::seconds(n)
}

fn row(seconds: i64, env: &str, host: &str, seq: &str) -> Row {
    Row::new(second(seconds), env, SERVICE, host, &format!("ctx {seq}")).field("seq", seq)
}

/// The corpus (module docs). `seq` names each row: `w1-<second>` on
/// `prod`/`web01`, `dup-a` and `dup-b` at the focus instant beside
/// `w1-30`, `w2-<second>` on `prod`/`web02`, `st-<second>` on
/// `staging`/`web01`, and `nohost` on `prod` without a host.
fn corpus() -> Vec<Row> {
    let mut rows: Vec<Row> = (0..60)
        .map(|s| row(s, "prod", "web01", &format!("w1-{s}")))
        .collect();
    rows.push(row(30, "prod", "web01", "dup-a").ingested_after(TimeDelta::seconds(20)));
    rows.push(row(30, "prod", "web01", "dup-b").ingested_after(TimeDelta::seconds(30)));
    for s in [29, 30, 31] {
        rows.push(row(s, "prod", "web02", &format!("w2-{s}")));
    }
    for s in [28, 30, 32] {
        rows.push(row(s, "staging", "web01", &format!("st-{s}")));
    }
    let mut nohost = row(45, "prod", "web01", "nohost");
    nohost.host = None;
    rows.push(nohost);
    rows
}

async fn boot() -> TestServer {
    boot_with(CaptureConfig::default()).await
}

async fn boot_with(capture: CaptureConfig) -> TestServer {
    let server = captured::boot_seeded(capture, |data| captured::seed_rows(data, &corpus())).await;
    server.results_clock.set(at("2024-01-17T11:00:00Z"));
    server
}

fn code(body: &serde_json::Value) -> &str {
    body["error"]["code"].as_str().unwrap_or_default()
}

fn permits(server: &TestServer) -> usize {
    server.state.query.pool.available_permits()
}

/// Capture `query` over the corpus minute, as `token`.
async fn capture(server: &TestServer, token: &str, query: &str) -> CapturedResult {
    let body = json!({
        "query": query,
        "window": {
            "earliest": "2024-01-17T10:00:00Z",
            "latest": "2024-01-17T10:01:00Z",
        },
    });
    captured::captured(server, token, &body).await.result
}

/// Every row of capture `id`.
async fn page(server: &TestServer, token: &str, id: &str) -> ResultPage {
    let (status, _, body) = captured::page(server, token, id, "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    serde_json::from_value(body).expect("a result page")
}

/// The text cell of column `name` in each row of `page`.
fn column(page: &ResultPage, name: &str) -> Vec<Option<String>> {
    let index = page
        .page
        .columns
        .iter()
        .position(|c| c.name == name)
        .unwrap_or_else(|| panic!("no column {name}"));
    page.page
        .rows
        .iter()
        .map(|row| match &row[index] {
            Value::String(text) => Some(text.clone()),
            Value::Null => None,
            other => panic!("{name}: {other:?}"),
        })
        .collect()
}

/// The row of `page` whose `seq` is `seq`.
fn row_of(page: &ResultPage, seq: &str) -> u64 {
    let seqs = column(page, "seq");
    let index = seqs
        .iter()
        .position(|s| s.as_deref() == Some(seq))
        .unwrap_or_else(|| panic!("no row {seq}"));
    u64::try_from(index).unwrap()
}

/// A focus capture of the whole corpus service and the row of `w1-30`.
async fn focus(server: &TestServer) -> (String, u64) {
    let result = capture(server, &server.analyst_token, &format!("service={SERVICE}")).await;
    let rows = page(server, &server.analyst_token, &result.id).await;
    (result.id, row_of(&rows, "w1-30"))
}

fn facts(result: &CapturedResult) -> &ContextFacts {
    match result.correlation.as_ref() {
        Some(Correlation::Context(facts)) => facts,
        other => panic!("context facts: {other:?}"),
    }
}

/// The `seq`s of a context capture, split into its three groups.
async fn groups(server: &TestServer, result: &CapturedResult) -> [Vec<String>; 3] {
    let rows = page(server, &server.analyst_token, &result.id).await;
    let seqs: Vec<String> = column(&rows, "seq").into_iter().flatten().collect();
    let groups = &facts(result).groups;
    let slice = |range: RowRange| {
        let start = usize::try_from(range.start).unwrap();
        let end = usize::try_from(range.end).unwrap();
        seqs[start..end].to_vec()
    };
    [
        slice(groups.earlier),
        slice(groups.same_time),
        slice(groups.later),
    ]
}

fn seqs(prefix: &str, seconds: impl IntoIterator<Item = i64>) -> Vec<String> {
    seconds
        .into_iter()
        .map(|s| format!("{prefix}-{s}"))
        .collect()
}

fn sorted(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values
}

// -- the read -----------------------------------------------------------------

/// AC1, AC2: on the default scope the read returns the ten rows before,
/// every row at the focus instant and the ten rows after, on the focus
/// row's env and host, as three groups ordered by `_time`. The duplicates
/// at the focus instant stay distinct rows, the focus row is named and not
/// copied, and the facts say this is a new read whose equal timestamps
/// establish no order.
#[tokio::test(flavor = "multi_thread")]
async fn context_returns_earlier_equal_and_later_rows_with_the_duplicates_distinct() {
    let server = boot().await;
    let (id, focus_row) = focus(&server).await;

    let answer = captured::correlated(
        &server,
        &server.analyst_token,
        "context",
        &json!({ "result_id": id, "row": focus_row }),
    )
    .await;
    let result = &answer.result;
    assert_eq!(result.kind, CaptureKind::Context);
    assert!(answer.execution.is_some());
    let facts = facts(result);
    assert_eq!(facts.focus.result_id, id);
    assert_eq!(facts.focus.row, focus_row);
    assert_eq!(facts.focus.time, "2024-01-17T10:00:30.000000Z");
    assert_eq!(facts.focus.env.as_deref(), Some("prod"));
    assert_eq!(facts.focus.host.as_deref(), Some("web01"));
    assert_eq!(facts.scope_used.env.as_deref(), Some("prod"));
    assert_eq!(facts.scope_used.host.as_deref(), Some("web01"));
    assert_eq!(facts.scope_used.seconds, 30);
    assert_eq!(facts.statement, CONTEXT_STATEMENT);
    assert_eq!(
        result.earliest.as_deref(),
        Some("2024-01-17T10:00:00.000000Z")
    );
    assert_eq!(
        result.latest.as_deref(),
        Some("2024-01-17T10:01:00.000001Z")
    );

    let [earlier, same, later] = groups(&server, result).await;
    assert_eq!(earlier, seqs("w1", 20..30));
    assert_eq!(
        sorted(same.clone()),
        vec!["dup-a".to_owned(), "dup-b".into(), "w1-30".into()],
        "three distinct rows at the focus instant, the focus event among them"
    );
    assert_eq!(later, seqs("w1", 31..41));
    assert_eq!(result.captured_rows, 23);

    // Ordered by `_time` across the groups.
    let rows = page(&server, &server.analyst_token, &result.id).await;
    let times: Vec<String> = column(&rows, "_time").into_iter().flatten().collect();
    let mut ascending = times.clone();
    ascending.sort();
    assert_eq!(times, ascending);
    assert!(
        times[10..13]
            .iter()
            .all(|t| t == "2024-01-17T10:00:30.000000Z")
    );
}

/// `before` and `after` choose how many rows either side; the nearest are
/// kept.
#[tokio::test(flavor = "multi_thread")]
async fn before_and_after_keep_the_nearest_rows() {
    let server = boot().await;
    let (id, focus_row) = focus(&server).await;
    let answer = captured::correlated(
        &server,
        &server.analyst_token,
        "context",
        &json!({ "result_id": id, "row": focus_row, "before": 2, "after": 0, "seconds": 5 }),
    )
    .await;
    let [earlier, same, later] = groups(&server, &answer.result).await;
    assert_eq!(earlier, seqs("w1", 28..30));
    assert_eq!(same.len(), 3);
    assert!(later.is_empty());
    assert_eq!(facts(&answer.result).scope_used.seconds, 5);
}

/// `scope: env` holds the read to the focus row's env across its hosts, and
/// reports the host it did not hold.
#[tokio::test(flavor = "multi_thread")]
async fn the_env_scope_reads_every_host_of_the_focus_env() {
    let server = boot().await;
    let (id, focus_row) = focus(&server).await;
    let answer = captured::correlated(
        &server,
        &server.analyst_token,
        "context",
        &json!({ "result_id": id, "row": focus_row, "scope": "env", "before": 2, "after": 2 }),
    )
    .await;
    let facts = facts(&answer.result);
    assert_eq!(facts.scope_used.env.as_deref(), Some("prod"));
    assert_eq!(facts.scope_used.host, None);
    assert_eq!(facts.focus.host.as_deref(), Some("web01"));
    let [earlier, same, later] = groups(&server, &answer.result).await;
    assert_eq!(
        sorted(earlier),
        vec!["w1-29".to_owned(), "w2-29".into()],
        "web02 joins on the env scope"
    );
    assert_eq!(
        sorted(same),
        vec![
            "dup-a".to_owned(),
            "dup-b".into(),
            "w1-30".into(),
            "w2-30".into()
        ]
    );
    assert_eq!(sorted(later), vec!["w1-31".to_owned(), "w2-31".into()]);
}

/// `scope: host` holds the read to the focus row's host across its envs.
#[tokio::test(flavor = "multi_thread")]
async fn the_host_scope_reads_every_env_of_the_focus_host() {
    let server = boot().await;
    let (id, focus_row) = focus(&server).await;
    let answer = captured::correlated(
        &server,
        &server.analyst_token,
        "context",
        &json!({ "result_id": id, "row": focus_row, "scope": "host", "before": 3, "after": 3 }),
    )
    .await;
    let facts = facts(&answer.result);
    assert_eq!(facts.scope_used.env, None);
    assert_eq!(facts.scope_used.host.as_deref(), Some("web01"));
    let [earlier, same, later] = groups(&server, &answer.result).await;
    assert_eq!(
        sorted(earlier),
        vec!["st-28".to_owned(), "w1-28".into(), "w1-29".into()]
    );
    assert_eq!(
        sorted(same),
        vec![
            "dup-a".to_owned(),
            "dup-b".into(),
            "st-30".into(),
            "w1-30".into()
        ]
    );
    assert_eq!(
        sorted(later),
        vec!["st-32".to_owned(), "w1-31".into(), "w1-32".into()]
    );
}

/// The stored row is a `context` capture whose facts come back on every
/// page, the single copy the response carried.
#[tokio::test(flavor = "multi_thread")]
async fn the_stored_context_capture_carries_its_facts_on_every_page() {
    let server = boot().await;
    let (id, focus_row) = focus(&server).await;
    let answer = captured::correlated(
        &server,
        &server.analyst_token,
        "context",
        &json!({ "result_id": id, "row": focus_row }),
    )
    .await;
    for query in ["", "?offset=5&limit=3", "?offset=22"] {
        let (status, _, body) =
            captured::page(&server, &server.analyst_token, &answer.result.id, query).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let page: ResultPage = serde_json::from_value(body).unwrap();
        assert_eq!(page.result.kind, CaptureKind::Context);
        assert_eq!(
            page.result.correlation, answer.result.correlation,
            "{query}"
        );
    }
    assert!(
        captured::listed(&server, &server.analyst_token)
            .await
            .contains(&answer.result.id)
    );
}

/// A kept focus row whose env retention has since emptied reads a source
/// that holds no files: with the hot buffer empty too, the engine answers
/// no columns at all, and the read answers three empty groups, not an
/// internal error.
#[tokio::test(flavor = "multi_thread")]
async fn a_context_read_whose_window_has_left_the_corpus_answers_empty_groups() {
    let server = boot().await;
    let (id, focus_row) = focus(&server).await;
    // Every day of `prod`, the shared seed's included: one left behind
    // matches the env's source, and the engine then answers its columns.
    // Nothing writes to the seeded root once the fixture is up: this test
    // ingests nothing, so the removal races with no writer.
    let prod = std::path::Path::new(server.state.query.pool.base_dir()).join("prod");
    for day in std::fs::read_dir(&prod).unwrap() {
        std::fs::remove_dir_all(day.unwrap().path()).unwrap();
    }
    assert_eq!(
        server
            .state
            .query
            .hot_buffer
            .as_ref()
            .map(|hot| hot.event_count()),
        Some(0),
        "an empty hot buffer: no hot lane to answer columns from"
    );

    let answer = captured::correlated(
        &server,
        &server.analyst_token,
        "context",
        &json!({ "result_id": id, "row": focus_row }),
    )
    .await;
    let result = &answer.result;
    assert_eq!(result.kind, CaptureKind::Context);
    assert_eq!(result.captured_rows, 0);
    let facts = facts(result);
    assert_eq!(facts.focus.time, "2024-01-17T10:00:30.000000Z");
    let empty = RowRange { start: 0, end: 0 };
    assert_eq!(facts.groups.earlier, empty);
    assert_eq!(facts.groups.same_time, empty);
    assert_eq!(facts.groups.later, empty);
}

// -- refusals -----------------------------------------------------------------

/// A refused read took nothing and left no capture beyond the focus one.
async fn assert_refused(
    server: &TestServer,
    body: serde_json::Value,
    status: StatusCode,
    expected: &str,
) -> String {
    let before = permits(server);
    let listed = captured::listed(server, &server.analyst_token).await;
    let (got, _, answer) =
        captured::correlate(server, &server.analyst_token, "context", &body).await;
    assert_eq!(got, status, "{body}: {answer}");
    assert_eq!(code(&answer), expected, "{answer}");
    captured::assert_nothing_held(server, before, expected).await;
    assert_eq!(
        captured::listed(server, &server.analyst_token).await,
        listed,
        "a refused read commits no capture"
    );
    answer["error"]["message"].as_str().unwrap().to_owned()
}

/// Rows either side are capped at 50, named with the value sent.
#[tokio::test(flavor = "multi_thread")]
async fn more_than_fifty_rows_either_side_are_refused() {
    let server = boot().await;
    let (id, focus_row) = focus(&server).await;
    for side in ["before", "after"] {
        let mut body = json!({ "result_id": id, "row": focus_row });
        body[side] = json!(51);
        let message = assert_refused(&server, body, StatusCode::BAD_REQUEST, "bad_request").await;
        assert!(
            message.contains(side) && message.contains("51"),
            "{message}"
        );
        assert!(message.contains("50"), "{message}");
    }
    let mut body = json!({ "result_id": id, "row": focus_row });
    body["before"] = json!(50);
    body["after"] = json!(50);
    let (status, _, answer) =
        captured::correlate(&server, &server.analyst_token, "context", &body).await;
    assert_eq!(status, StatusCode::OK, "fifty is the cap: {answer}");
}

/// A window over the capture span limit is the bounds' refusal, as for any
/// capture: `seconds` has no cap of its own.
#[tokio::test(flavor = "multi_thread")]
async fn a_window_over_seven_days_is_refused_as_any_capture_is() {
    let server = boot().await;
    let (id, focus_row) = focus(&server).await;
    let body = json!({ "result_id": id, "row": focus_row, "seconds": 4 * 86_400 });
    let message = assert_refused(&server, body, StatusCode::BAD_REQUEST, "bad_request").await;
    assert!(message.contains("at most 7d"), "{message}");

    // 3.5 days either side, plus the microsecond that holds the focus
    // instant, is just over the span.
    let body = json!({ "result_id": id, "row": focus_row, "seconds": 302_400 });
    assert_refused(&server, body, StatusCode::BAD_REQUEST, "bad_request").await;
    let body = json!({ "result_id": id, "row": focus_row, "seconds": 302_399 });
    let (status, _, answer) =
        captured::correlate(&server, &server.analyst_token, "context", &body).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
}

/// A row past the end is `focus_incomplete`, naming the result's row
/// count.
#[tokio::test(flavor = "multi_thread")]
async fn a_row_past_the_end_is_refused_with_the_row_count() {
    let server = boot().await;
    let (id, _) = focus(&server).await;
    let rows = page(&server, &server.analyst_token, &id)
        .await
        .pagination
        .total;
    for row in [rows, rows + 5] {
        let body = json!({ "result_id": id, "row": row });
        let message =
            assert_refused(&server, body, StatusCode::BAD_REQUEST, "focus_incomplete").await;
        assert!(
            message.contains(&format!("{rows} captured rows")),
            "{message}"
        );
    }
}

/// A focus row without the host the scope holds the read to is refused by
/// name, never widened; a scope without the host reads it.
#[tokio::test(flavor = "multi_thread")]
async fn a_focus_row_without_its_scoped_key_is_refused_by_name() {
    let server = boot().await;
    let result = capture(
        &server,
        &server.analyst_token,
        &format!("service={SERVICE}"),
    )
    .await;
    let rows = page(&server, &server.analyst_token, &result.id).await;
    let nohost = row_of(&rows, "nohost");

    for scope in ["env_and_host", "host"] {
        let body = json!({ "result_id": result.id, "row": nohost, "scope": scope });
        let message =
            assert_refused(&server, body, StatusCode::BAD_REQUEST, "focus_incomplete").await;
        assert!(message.contains("`host`"), "{message}");
    }
    let answer = captured::correlated(
        &server,
        &server.analyst_token,
        "context",
        &json!({ "result_id": result.id, "row": nohost, "scope": "env" }),
    )
    .await;
    let facts = facts(&answer.result);
    assert_eq!(facts.focus.host, None);
    assert_eq!(facts.scope_used.env.as_deref(), Some("prod"));

    // A focus result that dropped the column refuses alike.
    let dropped = capture(
        &server,
        &server.analyst_token,
        &format!("service={SERVICE} | drop host"),
    )
    .await;
    let body = json!({ "result_id": dropped.id, "row": 0 });
    let message = assert_refused(&server, body, StatusCode::BAD_REQUEST, "focus_incomplete").await;
    assert!(message.contains("`host`"), "{message}");
}

/// A focus result without `_time` refuses on every scope; any other shape
/// that carries the fields is accepted.
#[tokio::test(flavor = "multi_thread")]
async fn a_focus_row_without_time_is_refused_and_any_shape_with_the_fields_is_read() {
    let server = boot().await;
    let no_time = capture(
        &server,
        &server.analyst_token,
        &format!("service={SERVICE} | table env, host, seq"),
    )
    .await;
    for scope in ["env_and_host", "env", "host"] {
        let body = json!({ "result_id": no_time.id, "row": 0, "scope": scope });
        let message =
            assert_refused(&server, body, StatusCode::BAD_REQUEST, "focus_incomplete").await;
        assert!(message.contains("`_time`"), "{message}");
    }

    let projected = capture(
        &server,
        &server.analyst_token,
        &format!("service={SERVICE} seq=\"w1-30\" | table _time, env, host"),
    )
    .await;
    let answer = captured::correlated(
        &server,
        &server.analyst_token,
        "context",
        &json!({ "result_id": projected.id, "row": 0, "before": 1, "after": 1 }),
    )
    .await;
    let [earlier, same, later] = groups(&server, &answer.result).await;
    assert_eq!(earlier, vec!["w1-29".to_owned()]);
    assert_eq!(same.len(), 3);
    assert_eq!(later, vec!["w1-31".to_owned()]);
}

/// Two columns of a name the read needs are `focus_ambiguous`.
#[tokio::test(flavor = "multi_thread")]
async fn two_columns_of_a_focus_key_are_ambiguous() {
    let server = boot().await;
    let doubled = capture(
        &server,
        &server.analyst_token,
        &format!("service={SERVICE} | table _time, env, host, host"),
    )
    .await;
    let names: Vec<&str> = doubled.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        ["_time", "env", "host", "host"],
        "the fixture's shape"
    );
    let body = json!({ "result_id": doubled.id, "row": 0 });
    let message = assert_refused(&server, body, StatusCode::BAD_REQUEST, "focus_ambiguous").await;
    assert!(message.contains("`host`"), "{message}");

    // A scope that does not hold the read to the doubled key reads it.
    let (status, _, answer) = captured::correlate(
        &server,
        &server.analyst_token,
        "context",
        &json!({ "result_id": doubled.id, "row": 0, "scope": "env" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
}

/// Another key's result is not found, as a page of it is; a key without
/// `query` is refused first.
#[tokio::test(flavor = "multi_thread")]
async fn another_keys_result_is_not_found_and_query_is_required() {
    let server = boot().await;
    let (id, focus_row) = focus(&server).await;
    let body = json!({ "result_id": id, "row": focus_row });

    let before = permits(&server);
    let (status, _, answer) =
        captured::correlate(&server, &server.reader_token, "context", &body).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{answer}");
    assert_eq!(code(&answer), "not_found", "{answer}");
    captured::assert_nothing_held(&server, before, "another key's focus").await;
    assert!(
        captured::listed(&server, &server.reader_token)
            .await
            .is_empty()
    );

    let (status, _, answer) =
        captured::correlate(&server, &server.ingest_token, "context", &body).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");

    let body = json!({ "result_id": "not-an-id", "row": 0 });
    let (status, _, answer) =
        captured::correlate(&server, &server.analyst_token, "context", &body).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{answer}");
}

/// A GET on the static path is not a page of a result named `context`.
#[tokio::test(flavor = "multi_thread")]
async fn a_get_on_the_context_path_is_method_not_allowed() {
    let server = boot().await;
    let response = common::harness_client_builder()
        .build()
        .unwrap()
        .get(format!("{}/api/v1/results/context", server.url))
        .bearer_auth(&server.analyst_token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
}
