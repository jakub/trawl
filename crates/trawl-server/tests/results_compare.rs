// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `POST /api/v1/results/compare` end to end (ADR-0058): a filter's group
//! rates in a foreground window against a baseline, from one statement,
//! captured as the ranked table.
//!
//! The corpus sits on 2024-01-19, which no other seed touches. On `prod`,
//! `authsvc` logs auth failures per `user`: 10:00-11:00 holds alice 6, bob
//! 5 and dave 3; 11:00-12:00 holds alice 30, bob 5 and carol 2. `auth ok`
//! rows of the same users stay out of the filter. On `staging` the only
//! date partition is 2024-01-19, so a window starting the day before
//! reaches past what retention kept.
//!
//! The binary is in the ADR-0021 `postgres` nextest group.

mod common;

use std::sync::Arc;

use chrono::{DateTime, TimeDelta, Utc};
use common::TestServer;
use common::captured::{self, Row, at};
use reqwest::StatusCode;
use serde_json::json;
use trawl_api::results::{
    CaptureKind, CapturedResult, CompareFacts, CompareWarningCode, CompareWindow, Correlation,
    RateUnit, ResultPage, ResultShape,
};
use trawl_api::value::Value;
use trawl_config::CaptureConfig;
use trawl_server::state::DegradedSnapshot;

const FILTER: &str = r#"env=prod service=authsvc "auth failed""#;

fn failures(env: &str, start: DateTime<Utc>, user: &str, count: i64) -> Vec<Row> {
    (0..count)
        .map(|n| {
            Row::new(
                start + TimeDelta::seconds(60 * n + 7),
                env,
                "authsvc",
                "idp01",
                &format!("auth failed for {user}"),
            )
            .field("user", user)
        })
        .collect()
}

fn corpus() -> Vec<Row> {
    let baseline = at("2024-01-19T10:00:00Z");
    let foreground = at("2024-01-19T11:00:00Z");
    let mut rows = Vec::new();
    rows.extend(failures("prod", baseline, "alice", 6));
    rows.extend(failures("prod", baseline, "bob", 5));
    rows.extend(failures("prod", baseline, "dave", 3));
    rows.extend(failures("prod", foreground, "alice", 30));
    rows.extend(failures("prod", foreground, "bob", 5));
    rows.extend(failures("prod", foreground, "carol", 2));
    for user in ["alice", "bob", "erin"] {
        rows.push(
            Row::new(
                baseline + TimeDelta::minutes(30),
                "prod",
                "authsvc",
                "idp01",
                "auth ok",
            )
            .field("user", user),
        );
    }
    rows.extend(failures("staging", at("2024-01-19T05:00:00Z"), "alice", 4));
    rows.extend(failures("staging", at("2024-01-19T10:30:00Z"), "bob", 2));
    rows
}

async fn boot() -> TestServer {
    let server = captured::boot_seeded(CaptureConfig::default(), |data| {
        captured::seed_rows(data, &corpus());
    })
    .await;
    server.results_clock.set(at("2024-01-20T00:00:00Z"));
    server
}

fn code(body: &serde_json::Value) -> &str {
    body["error"]["code"].as_str().unwrap_or_default()
}

fn facts(result: &CapturedResult) -> &CompareFacts {
    match result.correlation.as_ref() {
        Some(Correlation::Compare(facts)) => facts,
        other => panic!("compare facts: {other:?}"),
    }
}

fn hour(start: &str, end: &str) -> serde_json::Value {
    json!({ "earliest": start, "latest": end })
}

fn foreground() -> serde_json::Value {
    hour("2024-01-19T11:00:00Z", "2024-01-19T12:00:00Z")
}

async fn compare(server: &TestServer, body: serde_json::Value) -> CapturedResult {
    captured::correlated(server, &server.analyst_token, "compare", &body)
        .await
        .result
}

async fn page(server: &TestServer, id: &str) -> ResultPage {
    let (status, _, body) = captured::page(server, &server.analyst_token, id, "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    serde_json::from_value(body).expect("a result page")
}

/// One ranked row: user, foreground and baseline counts, status.
#[derive(Debug, PartialEq)]
struct Ranked {
    user: Option<String>,
    foreground: i64,
    baseline: i64,
    status: String,
}

fn ranked(page: &ResultPage) -> Vec<Ranked> {
    let at = |name: &str| {
        page.page
            .columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
            .unwrap_or_else(|| panic!("no column {name}"))
    };
    let (user, fg, bl, status) = (
        at("user"),
        at("foreground_count"),
        at("baseline_count"),
        at("status"),
    );
    page.page
        .rows
        .iter()
        .map(|row| Ranked {
            user: match &row[user] {
                Value::String(text) => Some(text.clone()),
                _ => None,
            },
            foreground: match row[fg] {
                Value::Integer(n) => n,
                ref other => panic!("{other:?}"),
            },
            baseline: match row[bl] {
                Value::Integer(n) => n,
                ref other => panic!("{other:?}"),
            },
            status: match &row[status] {
                Value::String(text) => text.clone(),
                other => panic!("{other:?}"),
            },
        })
        .collect()
}

fn row(user: &str, foreground: i64, baseline: i64, status: &str) -> Ranked {
    Ranked {
        user: Some(user.to_owned()),
        foreground,
        baseline,
        status: status.to_owned(),
    }
}

fn float(page: &ResultPage, index: usize, name: &str) -> Option<f64> {
    let at = page
        .page
        .columns
        .iter()
        .position(|c| c.name == name)
        .unwrap();
    match page.page.rows[index][at] {
        Value::Float(x) => Some(x),
        Value::Null => None,
        ref other => panic!("{name}: {other:?}"),
    }
}

/// The issue's question: did auth failures rise against the previous
/// hour? Counts, rates per minute, absolute and relative change, a group
/// new in the foreground, a vanished one, ranked by absolute rate change,
/// from one statement over the hull. The baseline defaults to the hour
/// before.
#[tokio::test(flavor = "multi_thread")]
async fn auth_failures_rising_hour_over_hour_are_ranked_from_one_statement() {
    let server = boot().await;
    let result = compare(
        &server,
        json!({ "filter": FILTER, "foreground": foreground(), "group_by": ["user"] }),
    )
    .await;
    assert_eq!(result.kind, CaptureKind::Compare);
    assert_eq!(result.shape, ResultShape::Whole);
    assert_eq!(
        (result.earliest.as_deref(), result.latest.as_deref()),
        (
            Some("2024-01-19T10:00:00.000000Z"),
            Some("2024-01-19T12:00:00.000000Z")
        ),
        "the capture covers the hull"
    );
    let facts = facts(&result);
    assert_eq!(
        facts.foreground,
        CompareWindow {
            start: "2024-01-19T11:00:00.000000Z".into(),
            end: "2024-01-19T12:00:00.000000Z".into(),
            duration_ms: 3_600_000,
        }
    );
    assert_eq!(
        facts.baseline,
        CompareWindow {
            start: "2024-01-19T10:00:00.000000Z".into(),
            end: "2024-01-19T11:00:00.000000Z".into(),
            duration_ms: 3_600_000,
        },
        "the default baseline is the hour before"
    );
    assert_eq!(facts.group_by, vec!["user".to_owned()]);
    assert_eq!(facts.min_count, 1);
    assert_eq!(facts.rate_unit, RateUnit::PerMinute);
    assert_eq!(facts.rows_ranked, 4);
    assert!(facts.warnings.is_empty(), "{:?}", facts.warnings);

    let page = page(&server, &result.id).await;
    assert_eq!(
        ranked(&page),
        vec![
            row("alice", 30, 6, "both"),
            row("dave", 0, 3, "vanished"),
            row("carol", 2, 0, "new_in_foreground"),
            row("bob", 5, 5, "both"),
        ]
    );
    let close = |got: Option<f64>, want: f64| {
        let got = got.expect("a rate");
        assert!((got - want).abs() < 1e-9, "{got} != {want}");
    };
    close(float(&page, 0, "foreground_rate"), 0.5);
    close(float(&page, 0, "baseline_rate"), 0.1);
    close(float(&page, 0, "rate_change"), 0.4);
    close(float(&page, 0, "relative_change"), 4.0);
    close(float(&page, 1, "relative_change"), -1.0);
    assert_eq!(
        float(&page, 2, "relative_change"),
        None,
        "no relative change from a zero baseline"
    );
    close(float(&page, 3, "rate_change"), 0.0);
    assert_eq!(page.result.correlation, result.correlation);
}

/// The same request twice ranks byte for byte the same.
#[tokio::test(flavor = "multi_thread")]
async fn the_ranking_is_the_same_across_runs() {
    let server = boot().await;
    let body = json!({ "filter": FILTER, "foreground": foreground(), "group_by": ["user"] });
    let first = compare(&server, body.clone()).await;
    let second = compare(&server, body).await;
    assert_ne!(first.id, second.id);
    let (a, b) = (
        page(&server, &first.id).await,
        page(&server, &second.id).await,
    );
    assert_eq!(
        serde_json::to_string(&a.page).unwrap(),
        serde_json::to_string(&b.page).unwrap()
    );
}

/// A baseline that touches the foreground is accepted and is the default;
/// `min_count` lists a group only when either window reaches it.
#[tokio::test(flavor = "multi_thread")]
async fn touching_windows_are_accepted_and_min_count_filters_groups() {
    let server = boot().await;
    let explicit = compare(
        &server,
        json!({
            "filter": FILTER,
            "foreground": foreground(),
            "baseline": hour("2024-01-19T10:00:00Z", "2024-01-19T11:00:00Z"),
            "group_by": ["user"],
        }),
    )
    .await;
    let defaulted = compare(
        &server,
        json!({ "filter": FILTER, "foreground": foreground(), "group_by": ["user"] }),
    )
    .await;
    assert_eq!(facts(&explicit), facts(&defaulted));
    assert_eq!(
        ranked(&page(&server, &explicit.id).await),
        ranked(&page(&server, &defaulted.id).await)
    );

    let six = compare(
        &server,
        json!({ "filter": FILTER, "foreground": foreground(), "group_by": ["user"], "min_count": 6 }),
    )
    .await;
    assert_eq!(facts(&six).rows_ranked, 1);
    assert_eq!(
        ranked(&page(&server, &six.id).await),
        vec![row("alice", 30, 6, "both")]
    );

    // No group field: one row of totals.
    let totals = compare(
        &server,
        json!({ "filter": FILTER, "foreground": foreground() }),
    )
    .await;
    let page = page(&server, &totals.id).await;
    assert_eq!(page.page.rows.len(), 1);
    let at = |name: &str| {
        page.page
            .columns
            .iter()
            .position(|c| c.name == name)
            .unwrap()
    };
    assert_eq!(
        page.page.rows[0][at("foreground_count")],
        Value::Integer(37)
    );
    assert_eq!(page.page.rows[0][at("baseline_count")], Value::Integer(14));
}

/// A retention reach and a degraded group field each warn, and neither
/// warning claims completeness.
#[tokio::test(flavor = "multi_thread")]
async fn a_window_past_retention_and_a_degraded_group_field_each_warn() {
    let server = boot().await;
    let mut degraded = DegradedSnapshot::default();
    degraded.fields.insert("user".to_owned());
    *server.state.query.degraded_fields.lock() = Arc::new(degraded);

    // Eight hours from 04:00: the default baseline starts at 20:00 the
    // day before, before staging's only partition, 2024-01-19.
    let result = compare(
        &server,
        json!({
            "filter": "env=staging service=authsvc",
            "foreground": hour("2024-01-19T04:00:00Z", "2024-01-19T12:00:00Z"),
            "group_by": ["User"],
        }),
    )
    .await;
    let warned = facts(&result);
    assert_eq!(warned.baseline.start, "2024-01-18T20:00:00.000000Z");
    let codes: Vec<_> = warned
        .warnings
        .iter()
        .map(|w| (w.code, w.field.clone()))
        .collect();
    assert_eq!(
        codes,
        vec![
            (CompareWarningCode::RetentionReach, None),
            (CompareWarningCode::DegradedPin, Some("User".to_owned())),
        ],
        "{:?}",
        warned.warnings
    );
    let reach = &warned.warnings[0].message;
    assert!(
        reach.contains("baseline") && reach.contains("staging"),
        "{reach}"
    );
    assert!(
        reach.contains("data before 2024-01-19 may be missing"),
        "{reach}"
    );
    for warning in &warned.warnings {
        assert!(!warning.message.contains("complete"), "{}", warning.message);
    }
    assert_eq!(
        ranked(&page(&server, &result.id).await),
        vec![
            row("alice", 4, 0, "new_in_foreground"),
            row("bob", 2, 0, "new_in_foreground")
        ]
    );

    // A foreground inside what retention kept, and no degraded group
    // field, warns of nothing.
    *server.state.query.degraded_fields.lock() = Arc::new(DegradedSnapshot::default());
    let quiet = compare(
        &server,
        json!({
            "filter": "env=staging service=authsvc",
            "foreground": hour("2024-01-19T10:00:00Z", "2024-01-19T12:00:00Z"),
            "group_by": ["user"],
        }),
    )
    .await;
    assert!(
        facts(&quiet).warnings.is_empty(),
        "{:?}",
        facts(&quiet).warnings
    );
}

// -- refusals before the capture ---------------------------------------------

async fn assert_refused(server: &TestServer, body: serde_json::Value, expected: &str) -> String {
    let before = server.state.query.pool.available_permits();
    let (status, _, answer) =
        captured::correlate(server, &server.analyst_token, "compare", &body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {answer}");
    assert_eq!(code(&answer), expected, "{answer}");
    captured::assert_nothing_held(server, before, expected).await;
    assert!(
        captured::listed(server, &server.analyst_token)
            .await
            .is_empty(),
        "a refused read commits no capture"
    );
    answer["error"]["message"].as_str().unwrap().to_owned()
}

/// Overlapping windows are refused by name; a hull over seven days is the
/// capture's span refusal.
#[tokio::test(flavor = "multi_thread")]
async fn overlapping_windows_and_a_hull_over_seven_days_are_refused() {
    let server = boot().await;
    let message = assert_refused(
        &server,
        json!({
            "filter": FILTER,
            "foreground": foreground(),
            "baseline": hour("2024-01-19T10:30:00Z", "2024-01-19T11:30:00Z"),
        }),
        "windows_overlap",
    )
    .await;
    assert!(message.contains("2024-01-19T10:30:00.000000Z"), "{message}");

    let message = assert_refused(
        &server,
        json!({
            "filter": FILTER,
            "foreground": foreground(),
            "baseline": hour("2024-01-10T00:00:00Z", "2024-01-10T01:00:00Z"),
        }),
        "bad_request",
    )
    .await;
    assert!(message.contains("at most 7d"), "{message}");

    let message = assert_refused(
        &server,
        json!({ "filter": FILTER, "foreground": { "last": "8d" } }),
        "bad_request",
    )
    .await;
    assert!(message.contains("at most 7d"), "{message}");
}

/// The filter is search tokens only: a pipe stage or a time clause is
/// `correlation_invalid`, naming what it found.
#[tokio::test(flavor = "multi_thread")]
async fn a_filter_with_a_pipe_stage_or_a_time_clause_is_refused() {
    let server = boot().await;
    let message = assert_refused(
        &server,
        json!({ "filter": "service=authsvc | stats count()", "foreground": foreground() }),
        "correlation_invalid",
    )
    .await;
    assert!(message.contains("stats"), "{message}");
    let message = assert_refused(
        &server,
        json!({ "filter": "service=authsvc last=1h", "foreground": foreground() }),
        "correlation_invalid",
    )
    .await;
    assert!(message.contains("last="), "{message}");
}

/// At most two group fields, distinct under the fold; a name the statement
/// assigns is the builder's refusal.
#[tokio::test(flavor = "multi_thread")]
async fn group_fields_are_capped_distinct_and_never_the_statements_own() {
    let server = boot().await;
    let message = assert_refused(
        &server,
        json!({ "filter": FILTER, "foreground": foreground(), "group_by": ["user", "host", "env"] }),
        "bad_request",
    )
    .await;
    assert!(message.contains('3') && message.contains('2'), "{message}");
    let message = assert_refused(
        &server,
        json!({ "filter": FILTER, "foreground": foreground(), "group_by": ["Host", "host"] }),
        "bad_request",
    )
    .await;
    assert!(message.contains("\"Host\""), "{message}");
    let message = assert_refused(
        &server,
        json!({ "filter": FILTER, "foreground": foreground(), "group_by": ["Status"] }),
        "correlation_invalid",
    )
    .await;
    assert!(message.contains("\"Status\""), "{message}");
}

/// AC9 for all three reads: a selection over `source_max_bytes` is
/// refused with `capture_source_too_large` before the work starts, and
/// nothing is held. The budget is exactly the `authsvc` file of 10:00 on
/// `prod`, so a focus capture of that service fits, while each read's
/// selection adds a second file.
#[tokio::test(flavor = "multi_thread")]
async fn every_read_over_the_source_budget_is_refused_before_work_starts() {
    let mut rows = corpus();
    rows.push(Row::new(
        at("2024-01-19T10:00:30Z"),
        "prod",
        "noise",
        "idp01",
        "auth failed nearby",
    ));
    let tmp = tempfile::tempdir().unwrap();
    let data = common::seed_data_root(tmp.path());
    captured::seed_rows(&data, &rows);
    let budget =
        std::fs::metadata(std::path::Path::new(&data).join("prod/2024-01-19/10/authsvc.parquet"))
            .unwrap()
            .len();
    let server = common::setup_with_capture(
        tmp.path(),
        data,
        trawl_server::config::RateLimitConfig::default(),
        CaptureConfig {
            source_max_bytes: budget,
            ..CaptureConfig::default()
        },
    )
    .await;
    std::mem::forget(tmp);
    server.results_clock.set(at("2024-01-20T00:00:00Z"));
    // The session's gates live as long as it does.
    let seams = server.state.query.pool.seams();
    let started = seams.watch(trawl_server::pool::seam::Seam::Started);

    let focus = captured::captured(
        &server,
        &server.analyst_token,
        &json!({
            "query": "service=authsvc",
            "window": hour("2024-01-19T10:00:00Z", "2024-01-19T10:01:00Z"),
        }),
    )
    .await
    .result;
    assert_eq!(started.arrivals(), 1, "the focus capture fits the budget");

    let reads = [
        (
            "context",
            json!({ "result_id": focus.id, "row": 0, "scope": "env" }),
        ),
        ("sightings", json!({ "literal": "auth failed" })),
        (
            "compare",
            json!({ "filter": FILTER, "foreground": foreground(), "group_by": ["user"] }),
        ),
    ];
    for (read, body) in reads {
        let before = server.state.query.pool.available_permits();
        let (status, _, answer) =
            captured::correlate(&server, &server.analyst_token, read, &body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{read}: {answer}");
        assert_eq!(
            code(&answer),
            "capture_source_too_large",
            "{read}: {answer}"
        );
        assert_eq!(started.arrivals(), 1, "{read}: the work never started");
        captured::assert_nothing_held(&server, before, read).await;
    }
    assert_eq!(
        captured::listed(&server, &server.analyst_token).await,
        vec![focus.id],
        "no read committed a capture"
    );
}

/// A key without `query` is refused.
#[tokio::test(flavor = "multi_thread")]
async fn a_key_without_query_is_refused() {
    let server = boot().await;
    let (status, _, answer) = captured::correlate(
        &server,
        &server.ingest_token,
        "compare",
        &json!({ "filter": FILTER, "foreground": foreground() }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
}
