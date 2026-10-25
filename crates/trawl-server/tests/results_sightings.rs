// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `POST /api/v1/results/sightings` end to end (ADR-0058): one literal,
//! counted per (`env`, `service`) over every match, the most recent
//! matches captured, and fields attributed over the captured ones.
//!
//! The corpus sits on 2024-01-18, which no other seed touches: the IP
//! `10.0.4.17` under `client_ip` in `auth` and under `upstream_addr` in
//! `proxy`, in a message once, and in an `edge` event's `_raw` alone, on
//! two envs; rows of the same services that do not hold it; and a pair of
//! values that differ by `İ` and `I`. The 20,000-match corpus has a test
//! of its own.
//!
//! The binary is in the ADR-0021 `postgres` nextest group.

mod common;

use chrono::{DateTime, TimeDelta, Utc};
use common::TestServer;
use common::captured::{self, Row, at};
use reqwest::StatusCode;
use serde_json::json;
use trawl_api::results::{
    CaptureKind, CapturedResult, Correlation, FieldSightings, ResultPage, ResultShape,
    SightingGroup, SightingsFacts,
};
use trawl_config::CaptureConfig;

const IP: &str = "10.0.4.17";

fn minute(n: i64) -> DateTime<Utc> {
    at("2024-01-18T10:00:00Z") + TimeDelta::minutes(n)
}

fn corpus() -> Vec<Row> {
    vec![
        Row::new(minute(1), "prod", "auth", "web01", "login ok")
            .raw(&format!(r#"{{"client_ip":"{IP}","msg":"login ok"}}"#))
            .field("client_ip", IP),
        Row::new(minute(2), "prod", "auth", "web01", "login ok")
            .raw(&format!(r#"{{"client_ip":"{IP}","msg":"login ok"}}"#))
            .field("client_ip", IP),
        Row::new(minute(3), "prod", "auth", "web01", &format!("blocked {IP}"))
            .field("client_ip", "10.0.9.9"),
        Row::new(minute(4), "staging", "auth", "web09", "login ok")
            .raw(&format!(r#"{{"client_ip":"{IP}"}}"#))
            .field("client_ip", IP),
        Row::new(minute(5), "prod", "proxy", "lb01", "forwarded")
            .raw(&format!("forwarded upstream={IP}"))
            .field("upstream_addr", IP),
        Row::new(minute(6), "prod", "edge", "gw01", "conn dropped")
            .raw(&format!("conn {IP} dropped")),
        // Rows of the same services without the literal.
        Row::new(minute(7), "prod", "auth", "web01", "login ok")
            .raw(r#"{"client_ip":"10.0.4.18"}"#)
            .field("client_ip", "10.0.4.18"),
        Row::new(minute(8), "prod", "proxy", "lb01", "forwarded")
            .raw("forwarded upstream=10.0.4.71")
            .field("upstream_addr", "10.0.4.71"),
        // The fold pair: `İ` folds to `i` in DuckDB's table.
        Row::new(minute(9), "prod", "idp", "sso01", "token İXMARK seen").field("user", "İXMARK"),
        Row::new(minute(10), "prod", "idp", "sso01", "token ixmark seen").field("user", "ixmark"),
    ]
}

async fn boot_over(rows: Vec<Row>, capture: CaptureConfig) -> TestServer {
    let server = captured::boot_seeded(capture, |data| captured::seed_rows(data, &rows)).await;
    server.results_clock.set(at("2024-01-18T13:00:00Z"));
    server
}

async fn boot() -> TestServer {
    boot_over(corpus(), CaptureConfig::default()).await
}

fn code(body: &serde_json::Value) -> &str {
    body["error"]["code"].as_str().unwrap_or_default()
}

fn facts(result: &CapturedResult) -> &SightingsFacts {
    match result.correlation.as_ref() {
        Some(Correlation::Sightings(facts)) => facts,
        other => panic!("sightings facts: {other:?}"),
    }
}

fn field(name: &str, rows: u64) -> FieldSightings {
    FieldSightings {
        field: name.to_owned(),
        rows,
    }
}

fn group(env: &str, service: &str, count: u64, first: i64, last: i64) -> SightingGroup {
    let text = |m: i64| format!("2024-01-18T10:{m:02}:00.000000Z");
    SightingGroup {
        env: Some(env.to_owned()),
        service: Some(service.to_owned()),
        count,
        first_seen: text(first),
        last_seen: text(last),
    }
}

async fn sightings(server: &TestServer, body: serde_json::Value) -> CapturedResult {
    captured::correlated(server, &server.analyst_token, "sightings", &body)
        .await
        .result
}

/// AC3: the IP is found under two field names in two services and in a
/// message; the `_raw`-only match is counted apart; both denominators are
/// named; groups are joint (`env`, `service`), exact, count first.
#[tokio::test(flavor = "multi_thread")]
async fn one_ip_is_found_under_two_fields_in_two_services_and_raw_only_apart() {
    let server = boot().await;
    let result = sightings(&server, json!({ "literal": IP })).await;
    assert_eq!(result.kind, CaptureKind::Sightings);
    assert_eq!(
        result.shape,
        ResultShape::Events,
        "the event shape is imposed"
    );
    assert_eq!(
        result.earliest.as_deref(),
        Some("2024-01-17T13:00:00.000000Z"),
        "the default window is the last 24 hours"
    );
    assert!(result.executed_query.contains("eventstats"));

    let facts = facts(&result);
    assert_eq!(facts.literal, IP);
    assert_eq!(facts.matches_total, 6);
    assert_eq!(facts.captured, 6);
    assert!(facts.attribution_fields.is_empty());
    assert_eq!(
        facts.groups,
        vec![
            group("prod", "auth", 3, 1, 3),
            group("staging", "auth", 1, 4, 4),
            group("prod", "edge", 1, 6, 6),
            group("prod", "proxy", 1, 5, 5),
        ]
    );
    assert_eq!(
        facts.fields,
        vec![
            field("client_ip", 3),
            field("message", 1),
            field("upstream_addr", 1)
        ]
    );
    assert_eq!(
        facts.raw_only_rows, 1,
        "the edge event holds the IP in `_raw` alone"
    );
    assert_eq!(facts.denominators.field_counts_over, 6);
    assert_eq!(facts.denominators.service_counts_over, 6);
    assert_eq!(facts.follow_ups.len(), 2);
    assert!(
        facts.follow_ups[0].contains("service=auth"),
        "{:?}",
        facts.follow_ups
    );
    assert!(
        facts.follow_ups[1].contains("timechart"),
        "{:?}",
        facts.follow_ups
    );
}

/// AC3: `fields` restricts attribution and never the population: the same
/// matches, captured rows and groups, counts over the named fields only,
/// and the restriction stated in the facts on every page.
#[tokio::test(flavor = "multi_thread")]
async fn fields_restrict_attribution_without_changing_the_population() {
    let server = boot().await;
    let open = sightings(&server, json!({ "literal": IP })).await;
    let restricted = sightings(&server, json!({ "literal": IP, "fields": ["Client_IP"] })).await;
    let (open, narrow) = (facts(&open), facts(&restricted));
    assert_eq!(narrow.matches_total, open.matches_total);
    assert_eq!(narrow.captured, open.captured);
    assert_eq!(narrow.groups, open.groups);
    assert_eq!(narrow.fields, vec![field("client_ip", 3)]);
    assert_eq!(
        narrow.raw_only_rows, open.raw_only_rows,
        "raw-only is a population fact, counted over every field"
    );
    assert_eq!(
        narrow.raw_only_rows, 1,
        "the edge event alone holds it in `_raw` alone; the message and \
         upstream_addr matches are outside `fields`, not raw-only"
    );
    assert_eq!(narrow.attribution_fields, vec!["Client_IP".to_owned()]);
    assert_eq!(restricted.total_rows, 6);

    let (status, _, body) =
        captured::page(&server, &server.analyst_token, &restricted.id, "?offset=3").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let page: ResultPage = serde_json::from_value(body).unwrap();
    assert_eq!(page.result.correlation, restricted.correlation);
}

/// Attribution folds case as the search's `ILIKE` does, with `DuckDB`'s own
/// table: `İXMARK` and `ixmark` both hold `ixmark` and `İxmark`.
#[tokio::test(flavor = "multi_thread")]
async fn the_dotted_capital_i_is_attributed_as_duckdb_matches_it() {
    let server = boot().await;
    for literal in ["ixmark", "İxmark"] {
        let result = sightings(&server, json!({ "literal": literal })).await;
        let facts = facts(&result);
        assert_eq!(facts.matches_total, 2, "{literal}: the search matches both");
        assert_eq!(
            facts.fields,
            vec![field("message", 2), field("user", 2)],
            "{literal}"
        );
        assert_eq!(facts.raw_only_rows, 0, "{literal}");
    }
}

/// A literal of DSL syntax is one quoted value: it searches for itself.
#[tokio::test(flavor = "multi_thread")]
async fn a_hostile_literal_searches_for_itself() {
    let hostile = r#"say "hi" | head 1 // # \ %_*"#;
    let mut rows = corpus();
    rows.push(Row::new(
        minute(11),
        "prod",
        "chat",
        "c01",
        &format!("he wrote {hostile} twice"),
    ));
    let server = boot_over(rows, CaptureConfig::default()).await;
    let result = sightings(&server, json!({ "literal": hostile })).await;
    let facts = facts(&result);
    assert_eq!(facts.matches_total, 1);
    assert_eq!(facts.fields, vec![field("message", 1)]);
}

/// AC4: over 20,000 matches the groups are exact while only the 5000 most
/// recent are captured, a group whose matches all sit past the cut
/// included: the statement counts before the cut.
#[tokio::test(flavor = "multi_thread")]
async fn groups_are_exact_over_twenty_thousand_matches_while_five_thousand_are_captured() {
    let start_new = at("2024-01-18T11:00:00Z");
    let start_old = at("2024-01-18T10:00:00Z");
    let mut rows = Vec::with_capacity(20_000);
    for n in 0..12_000 {
        let time = start_new + TimeDelta::milliseconds(200 * n);
        rows.push(Row::new(
            time,
            "prod",
            "bulknew",
            "b01",
            &format!("needle4b {n}"),
        ));
    }
    for n in 0..8_000 {
        let time = start_old + TimeDelta::milliseconds(400 * n);
        rows.push(Row::new(
            time,
            "prod",
            "bulkold",
            "b02",
            &format!("needle4b {n}"),
        ));
    }
    let server = boot_over(rows, CaptureConfig::default()).await;

    let result = sightings(&server, json!({ "literal": "needle4b" })).await;
    let facts = facts(&result);
    assert_eq!(result.total_rows, 20_000);
    assert_eq!(result.captured_rows, 5000);
    assert_eq!(facts.matches_total, 20_000);
    assert_eq!(facts.captured, 5000);
    assert_eq!(facts.denominators.service_counts_over, 20_000);
    assert_eq!(facts.denominators.field_counts_over, 5000);
    assert_eq!(
        facts.groups,
        vec![
            SightingGroup {
                env: Some("prod".into()),
                service: Some("bulknew".into()),
                count: 12_000,
                first_seen: "2024-01-18T11:00:00.000000Z".into(),
                last_seen: "2024-01-18T11:39:59.800000Z".into(),
            },
            SightingGroup {
                env: Some("prod".into()),
                service: Some("bulkold".into()),
                count: 8_000,
                first_seen: "2024-01-18T10:00:00.000000Z".into(),
                last_seen: "2024-01-18T10:53:19.600000Z".into(),
            },
        ],
        "bulkold's matches are all older than the cut and still counted"
    );
    // Captured are the most recent: bulknew alone, down to the last
    // captured row.
    for query in ["?columns=service", "?columns=service&offset=4800"] {
        let (status, _, body) =
            captured::page(&server, &server.analyst_token, &result.id, query).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let page: ResultPage = serde_json::from_value(body).unwrap();
        assert_eq!(page.page.rows.len(), 200, "{query}");
        assert!(
            page.page
                .rows
                .iter()
                .all(|row| row[0] == trawl_api::value::Value::String("bulknew".into())),
            "{query}"
        );
    }
    assert_eq!(facts.fields, vec![field("message", 5000)]);
}

/// More distinct groups than the cap is `result_too_large`, naming the fix.
#[tokio::test(flavor = "multi_thread")]
async fn more_groups_than_the_cap_are_refused_naming_the_fix() {
    // 5001 services, one match each, written into one file.
    let rows: Vec<Row> = (0..5001)
        .map(|n| {
            Row::new(
                at("2024-01-18T10:00:00Z") + TimeDelta::milliseconds(n),
                "prod",
                &format!("s{n}"),
                "h",
                "needlegroups",
            )
        })
        .collect();
    let server = captured::boot_seeded(CaptureConfig::default(), |data| {
        captured::seed_file(data, "prod/2024-01-18/10", "many", &rows);
    })
    .await;
    server.results_clock.set(at("2024-01-18T13:00:00Z"));
    let (status, _, answer) = captured::correlate(
        &server,
        &server.analyst_token,
        "sightings",
        &json!({ "literal": "needlegroups" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
    assert_eq!(code(&answer), "result_too_large", "{answer}");
    let message = answer["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("5001 groups") && message.contains("5000"),
        "{message}"
    );
    assert!(
        message.contains("narrow the window or the literal"),
        "{message}"
    );
}

// -- refusals before the capture ---------------------------------------------

async fn assert_refused(server: &TestServer, body: serde_json::Value, expected: &str) -> String {
    let before = server.state.query.pool.available_permits();
    let (status, _, answer) =
        captured::correlate(server, &server.analyst_token, "sightings", &body).await;
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

/// The literal's caps and NUL, the `fields` cap and duplicates, and a
/// window over seven days are refused before anything is taken.
#[tokio::test(flavor = "multi_thread")]
async fn the_literal_fields_and_window_caps_refuse_before_anything_is_taken() {
    let server = boot().await;
    let message = assert_refused(&server, json!({ "literal": "" }), "bad_request").await;
    assert!(message.contains("empty"), "{message}");
    let message = assert_refused(&server, json!({ "literal": "a\u{0}b" }), "bad_request").await;
    assert!(message.contains("NUL"), "{message}");
    let long = "a".repeat(1025);
    let message = assert_refused(&server, json!({ "literal": long }), "bad_request").await;
    assert!(
        message.contains("1025") && message.contains("1024"),
        "{message}"
    );

    let seventeen: Vec<String> = (0..17).map(|n| format!("f{n}")).collect();
    let message = assert_refused(
        &server,
        json!({ "literal": IP, "fields": seventeen }),
        "bad_request",
    )
    .await;
    assert!(
        message.contains("17") && message.contains("16"),
        "{message}"
    );
    let message = assert_refused(
        &server,
        json!({ "literal": IP, "fields": ["Host", "host"] }),
        "bad_request",
    )
    .await;
    assert!(message.contains("twice"), "{message}");

    let message = assert_refused(
        &server,
        json!({ "literal": IP, "window": { "last": "8d" } }),
        "bad_request",
    )
    .await;
    assert!(message.contains("at most 7d"), "{message}");

    // Sixteen fields and seven days are the caps.
    let sixteen: Vec<String> = (0..16).map(|n| format!("f{n}")).collect();
    let (status, _, answer) = captured::correlate(
        &server,
        &server.analyst_token,
        "sightings",
        &json!({ "literal": IP, "fields": sixteen, "window": { "last": "7d" } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
}

/// `_raw` and the statement's `sighting_*` columns are never attributed:
/// naming one in `fields`, under any case, is refused before anything is
/// taken, naming the reserved name.
#[tokio::test(flavor = "multi_thread")]
async fn a_reserved_name_in_fields_is_refused_before_anything_is_taken() {
    let server = boot().await;
    for name in [
        "_raw",
        "_RAW",
        "sighting_count",
        "Sighting_Count",
        "sighting_first",
        "sighting_last",
    ] {
        let message = assert_refused(
            &server,
            json!({ "literal": IP, "fields": ["client_ip", name] }),
            "bad_request",
        )
        .await;
        assert!(
            message.contains(&format!("{name:?}")) && message.contains("reserved"),
            "{name}: {message}"
        );
    }
}

/// A key without `query` is refused.
#[tokio::test(flavor = "multi_thread")]
async fn a_key_without_query_is_refused() {
    let server = boot().await;
    let (status, _, answer) = captured::correlate(
        &server,
        &server.ingest_token,
        "sightings",
        &json!({ "literal": IP }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{answer}");
}
