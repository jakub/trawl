// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The search URL and the computed hints (ADR-0058, #308 AC11, AC12,
//! AC13): the link round-trips through the codec to exactly the captured
//! bounds, each of the four reasons for no link, and each hint firing and
//! not firing at its threshold.

mod support;

use chrono::{DateTime, Utc};
use trawl_api::value::{Column, ColumnType, QueryResult, Value};
use trawl_client::{CatalogFieldResponse, CatalogFieldServiceRow, ResultSource};
use trawl_core::ast::{FilterOp, FilterValue, SearchToken};
use trawl_mcp::hints::{self, NEAREST_NOT_COMPUTED};
use trawl_mcp::render::link::{
    MAX_LINK_BYTES, NO_BOUNDS, NO_ORIGIN, REFUSED, TOO_LONG, search_link,
};
use trawl_search_url::query_merge::{RangeSpec, effective_query};
use trawl_search_url::result_path::result_search_path;
use trawl_search_url::search_url::{
    MAX_SEARCH_BYTES, Mode, SearchRead, Verdict, build_search_url, decode_range, first_value,
    read_search,
};

use support::*;

const EARLIEST: &str = "2026-03-14T03:00:00.250000Z";
const LATEST: &str = "2026-03-14T04:00:00.750000Z";

fn at(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text)
        .unwrap()
        .with_timezone(&Utc)
}

fn result(query: &str) -> trawl_client::CapturedResult {
    captured(
        query,
        &[("_time", ColumnType::Timestamp)],
        vec![no_values(0)],
        0,
        0,
    )
}

// -- the search URL -----------------------------------------------------------

#[test]
fn the_search_url_round_trips_to_exactly_the_captured_bounds() {
    let captured = result("service=\"api gw\" status>=500 # it's late");
    let link = search_link(Some(ORIGIN), &captured);
    let url = link.url.expect("a link");
    assert!(link.reason.is_none());
    let search = url
        .strip_prefix(&format!("{ORIGIN}/search?"))
        .expect("the origin and the Search path");

    let SearchRead::Params(params) = read_search(search) else {
        panic!("the reader refused the link");
    };
    let q = first_value(&params, "q").expect("q");
    assert_eq!(q, captured.executed_query);
    assert_eq!(first_value(&params, "page"), Some("0"));
    assert_eq!(first_value(&params, "f"), None);
    assert_eq!(first_value(&params, "mode"), None);
    let Verdict::Valid(range) = decode_range(first_value(&params, "r").expect("r")) else {
        panic!("r does not decode");
    };
    assert_eq!(
        range,
        RangeSpec::Absolute {
            from: "2026-03-14T03:00:00Z".to_owned(),
            to: "2026-03-14T04:00:01Z".to_owned(),
        }
    );

    let effective = effective_query(q, &[], &range);
    let parsed = trawl_core::parser::parse(&effective).expect("the effective query parses");
    assert_eq!(
        parsed.search.earliest.as_ref().map(|b| b.node.as_str()),
        Some(EARLIEST)
    );
    assert_eq!(
        parsed.search.latest.as_ref().map(|b| b.node.as_str()),
        Some(LATEST)
    );
    let hull: Vec<(FilterOp, String)> = parsed
        .search
        .all_tokens()
        .filter_map(|token| match &token.node {
            SearchToken::FieldFilter(filter) if filter.field == "_time" => match &filter.value {
                FilterValue::Literal(value) => Some((filter.op, value.clone())),
                FilterValue::List(_) => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(
        hull,
        [
            (FilterOp::Gte, "2026-03-14T03:00:00Z".to_owned()),
            (FilterOp::Lte, "2026-03-14T04:00:01Z".to_owned()),
        ]
    );
}

#[test]
fn the_link_path_is_the_shared_codecs_result_search_path() {
    let overlong = "x".repeat(MAX_SEARCH_BYTES);
    let cases: [(&str, Option<&str>, Option<&str>); 5] = [
        ("service=api", Some(EARLIEST), Some(LATEST)),
        (
            "host=\"a b\" 100% done & more",
            Some("2026-03-14T05:00:00.5+02:00"),
            Some("2026-03-14T06:00:00+02:00"),
        ),
        ("service=api", Some("2026-03-14T03:00:00Z"), Some("soon")),
        (&overlong, Some(EARLIEST), Some(LATEST)),
        ("service=api", None, None),
    ];
    for (query, earliest, latest) in cases {
        let mut captured = result(query);
        captured.earliest = earliest.map(str::to_owned);
        captured.latest = latest.map(str::to_owned);
        let link = search_link(Some(ORIGIN), &captured);
        let (Some(e), Some(l)) = (earliest, latest) else {
            assert_eq!(link.reason.as_deref(), Some(NO_BOUNDS));
            continue;
        };
        let path = result_search_path(&captured.executed_query, e, l);
        match path {
            Ok(path) => assert_eq!(link.url, Some(format!("{ORIGIN}{path}")), "{query}"),
            Err(_) => assert_eq!(link.url, None, "{query}"),
        }
    }
}

#[test]
fn no_origin_no_link() {
    let link = search_link(None, &result("service=api"));
    assert_eq!(link.url, None);
    assert_eq!(link.reason.as_deref(), Some(NO_ORIGIN));
}

#[test]
fn a_stored_source_has_no_bounds_and_no_link() {
    let mut stored = result("| from saved \"daily\"");
    stored.source = ResultSource::Stored;
    stored.earliest = None;
    stored.latest = None;
    let link = search_link(Some(ORIGIN), &stored);
    assert_eq!(link.url, None);
    assert_eq!(link.reason.as_deref(), Some(NO_BOUNDS));
}

/// The search part's length for an executed text of `n` bytes that
/// percent-encode as themselves.
fn search_bytes(n: usize) -> usize {
    let path = build_search_url(
        &"a".repeat(n),
        0,
        Mode::Snapshot,
        &[],
        &RangeSpec::Absolute {
            from: "2026-03-14T03:00:00Z".to_owned(),
            to: "2026-03-14T04:00:01Z".to_owned(),
        },
    );
    path.len() - "/search?".len()
}

#[test]
fn a_link_the_codec_would_refuse_is_not_built() {
    let overhead = search_bytes(0);
    let mut long = result("service=api");
    long.executed_query = "a".repeat(MAX_SEARCH_BYTES - overhead + 1);
    assert_eq!(
        search_bytes(long.executed_query.len()),
        MAX_SEARCH_BYTES + 1
    );
    let link = search_link(Some(ORIGIN), &long);
    assert_eq!(link.url, None);
    assert_eq!(link.reason.as_deref(), Some(REFUSED));
}

#[test]
fn a_link_over_32_kib_with_its_origin_is_not_built() {
    let overhead = search_bytes(0);
    let mut long = result("service=api");
    long.executed_query = "a".repeat(MAX_SEARCH_BYTES - overhead);
    // The reader would take the query string, but not the whole link.
    assert_eq!(search_bytes(long.executed_query.len()), MAX_SEARCH_BYTES);
    let link = search_link(Some(ORIGIN), &long);
    assert_eq!(link.url, None);
    assert_eq!(link.reason.as_deref(), Some(TOO_LONG));

    // Short enough with the origin, the same text links.
    let origin_and_path = ORIGIN.len() + "/search?".len();
    long.executed_query = "a".repeat(MAX_LINK_BYTES - overhead - origin_and_path);
    let link = search_link(Some(ORIGIN), &long);
    assert_eq!(link.url.map(|url| url.len()), Some(MAX_LINK_BYTES));
}

// -- dominance ----------------------------------------------------------------

fn dominated(top: u64, rest: u64, distinct_capped: bool) -> trawl_client::CapturedResult {
    let mut facts = values(&[(s("api"), top), (s("db"), rest)]);
    facts.distinct_capped = distinct_capped;
    captured(
        "last=1h",
        &[
            ("_time", ColumnType::Timestamp),
            ("service", ColumnType::String),
        ],
        vec![no_values(100), facts],
        top + rest,
        top + rest,
    )
}

#[test]
fn dominance_fires_at_eighty_percent_and_not_below() {
    assert_eq!(
        hints::dominance(&dominated(80, 20, false)),
        [
            r#"dominance: "service" "api" holds 80 of the 100 captured rows (80%) among 2 distinct values"#
        ]
    );
    assert!(hints::dominance(&dominated(79, 21, false)).is_empty());
    assert!(hints::dominance(&dominated(80, 20, true)).is_empty());
}

#[test]
fn one_value_alone_is_not_dominance() {
    let only = captured(
        "service=api",
        &[("service", ColumnType::String)],
        vec![values(&[(s("api"), 100)])],
        100,
        100,
    );
    assert!(hints::dominance(&only).is_empty());
}

// -- lag ----------------------------------------------------------------------

fn lag_page(lag_secs: i64) -> QueryResult {
    let time = at("2026-03-14T03:00:00Z");
    let ingested = time + chrono::TimeDelta::seconds(lag_secs);
    let stamp =
        |at: DateTime<Utc>| Value::String(at.to_rfc3339_opts(chrono::SecondsFormat::Micros, true));
    QueryResult {
        columns: vec![
            Column::new("_time", ColumnType::Timestamp),
            Column::new("service", ColumnType::String),
            Column::new("host", ColumnType::String),
            Column::new("_ingested", ColumnType::Timestamp),
        ],
        rows: vec![
            vec![stamp(time), s("api"), s("web-1"), stamp(time)],
            vec![stamp(time), s("api"), s("web-2\n"), stamp(ingested)],
        ],
    }
}

#[test]
fn lag_fires_past_five_minutes_and_names_the_sender() {
    assert_eq!(
        hints::lag(&lag_page(301)).as_deref(),
        Some(
            r#"lag: service "api" on host "web-2\n" sent an event 301 s after its _time (the largest _ingested - _time over the 2 rows this response read)"#
        )
    );
    assert_eq!(hints::lag(&lag_page(300)), None);
}

#[test]
fn lag_needs_both_time_columns() {
    let mut page = lag_page(9000);
    page.columns.truncate(3);
    for row in &mut page.rows {
        row.truncate(3);
    }
    assert_eq!(hints::lag(&page), None);
}

// -- not in the catalog -------------------------------------------------------

fn observed_in(services: &[&str]) -> CatalogFieldResponse {
    serde_json::from_value(serde_json::json!({
        "name": "x",
        "type": "BIGINT",
        "pinned_at": "2026-03-01T00:00:00Z",
        "services": services.iter().map(|service| CatalogFieldServiceRow {
            service: (*service).to_owned(),
            first_seen: "2026-03-01T00:00:00Z".to_owned(),
            last_seen: "2026-03-14T00:00:00Z".to_owned(),
            row_count: 1,
        }).map(|row| serde_json::json!({
            "service": row.service, "first_seen": row.first_seen,
            "last_seen": row.last_seen, "row_count": row.row_count,
        })).collect::<Vec<_>>(),
        "conflicts": [],
    }))
    .unwrap()
}

#[tokio::test]
async fn a_field_the_service_never_sent_is_named() {
    let mut backend = Fake::default();
    backend
        .catalog_field
        .insert("status".to_owned(), observed_in(&["db"]));
    backend
        .catalog_field
        .insert("duration".to_owned(), observed_in(&["db", "api"]));
    let found = hints::never_observed(
        &backend,
        "service=api status>=500 duration>1 nope=1 _severity>=error | stats count() by host",
    )
    .await;
    assert_eq!(
        found,
        [
            r#"the catalog has no pin for field "nope""#,
            r#"the catalog has no observation of field "status" in service "api"; it has observations of it in 1 other service"#,
        ]
    );
    assert_eq!(
        backend.calls(),
        [
            "catalog_field duration Some(100) None",
            "catalog_field nope Some(100) None",
            "catalog_field status Some(100) None",
        ]
    );
}

#[tokio::test]
async fn without_a_named_service_only_an_unknown_field_is_named() {
    let mut backend = Fake::default();
    backend
        .catalog_field
        .insert("status".to_owned(), observed_in(&["db"]));
    let found = hints::never_observed(&backend, "status>=500 OR nope=1").await;
    assert_eq!(found, [r#"the catalog has no pin for field "nope""#]);
}

// -- nearest bounds -----------------------------------------------------------

fn nearest_answer() -> trawl_client::QueryResponse {
    query_response(
        vec![
            Column::new("side", ColumnType::String),
            Column::new("newest", ColumnType::Timestamp),
            Column::new("oldest", ColumnType::Timestamp),
            Column::new("events", ColumnType::Integer),
        ],
        vec![
            vec![
                s("before"),
                s("2026-03-13T22:10:00.000000Z"),
                s("2026-03-10T01:00:00.000000Z"),
                Value::Integer(12),
            ],
            vec![
                s("after"),
                s("2026-03-14T09:00:00.000000Z"),
                s("2026-03-14T05:30:00.000000Z"),
                Value::Integer(3),
            ],
        ],
        2,
    )
}

#[tokio::test]
async fn a_zero_row_search_reports_the_nearest_events_for_the_same_search() {
    let backend = Fake {
        query: ok(nearest_answer()),
        ..Fake::default()
    };
    let empty = result("service=api status>=500 last=1h");
    let hint = hints::nearest_bounds(&backend, &empty, at("2026-03-14T10:00:00Z")).await;
    assert_eq!(
        hint,
        "nearest bounds over events matching the same search stage from \
         2026-03-07T03:00:00.250000Z to 2026-03-14T10:00:00.000000Z: the last event before the \
         bounds is at 2026-03-13T22:10:00.000000Z (12 events from 2026-03-07T03:00:00.250000Z \
         to 2026-03-14T03:00:00.250000Z); the first event after them is at \
         2026-03-14T05:30:00.000000Z (3 events from 2026-03-14T04:00:00.750000Z to \
         2026-03-14T10:00:00.000000Z)"
    );
    // The time clause is gone, the widened bounds clipped at now are in
    // its place, and the rest of the search is the caller's.
    assert_eq!(
        backend.calls(),
        ["query Some(3) None Some(\"UTC\") service=api status>=500 \
             earliest=\"2026-03-07T03:00:00.250000Z\" latest=\"2026-03-14T10:00:00.000000Z\"\n\
             | let side = case(_time < \"2026-03-14T03:00:00.250000Z\", \"before\", _time >= \
             \"2026-03-14T04:00:00.750000Z\", \"after\", \"inside\")\n\
             | stats max(_time) as newest, min(_time) as oldest, count() as events by side"]
    );
}

#[tokio::test]
async fn the_far_bound_is_seven_days_out_when_now_is_later() {
    let backend = Fake {
        query: ok(query_response(vec![], vec![], 0)),
        ..Fake::default()
    };
    let empty = result("error");
    let hint = hints::nearest_bounds(&backend, &empty, at("2026-04-01T00:00:00Z")).await;
    assert!(
        hint.starts_with("nearest bounds: not computed; the widened query answered without"),
        "{hint}"
    );
    assert!(
        backend.calls()[0].contains("latest=\"2026-03-21T04:00:00.750000Z\""),
        "{:?}",
        backend.calls()
    );
}

#[tokio::test]
async fn a_pipeline_is_not_widened() {
    let backend = Fake::default();
    let empty = result("service=api | stats count() by host");
    let hint = hints::nearest_bounds(&backend, &empty, at("2026-03-14T10:00:00Z")).await;
    assert_eq!(hint, NEAREST_NOT_COMPUTED);
    assert!(backend.calls().is_empty());
}

#[tokio::test]
async fn nothing_on_either_side_says_so() {
    let backend = Fake {
        query: ok(query_response(
            nearest_answer().result.columns,
            vec![vec![
                s("inside"),
                s("2026-03-14T03:30:00.000000Z"),
                s("2026-03-14T03:30:00.000000Z"),
                Value::Integer(2),
            ]],
            1,
        )),
        ..Fake::default()
    };
    let hint = hints::nearest_bounds(&backend, &result("error"), at("2026-03-14T04:00:01Z")).await;
    assert!(
        hint.contains("no event from 2026-03-07T03:00:00.250000Z to"),
        "{hint}"
    );
    assert!(
        hint.contains("no event from 2026-03-14T04:00:00.750000Z to"),
        "{hint}"
    );
    assert!(
        hint.ends_with("2 events now fall inside the bounds, so they arrived after the capture"),
        "{hint}"
    );
}

/// The one query `nearest_bounds` submitted, parsed back.
fn submitted_query(backend: &Fake) -> trawl_core::ast::Query {
    let calls = backend.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    let dsl = calls[0]
        .strip_prefix("query Some(3) None Some(\"UTC\") ")
        .unwrap_or_else(|| panic!("{calls:?}"));
    trawl_core::parser::parse(dsl).unwrap_or_else(|e| panic!("{dsl:?}: {e:?}"))
}

/// The search terms of a parsed query's groups, spans aside.
fn search_terms(query: &trawl_core::ast::Query) -> Vec<Vec<SearchToken>> {
    query
        .search
        .groups
        .iter()
        .map(|group| group.iter().map(|token| token.node.clone()).collect())
        .collect()
}

fn phrase_and_service(phrase: &str) -> Vec<Vec<SearchToken>> {
    vec![vec![
        SearchToken::QuotedSearch(trawl_core::ast::QuotedSearch {
            phrase: phrase.to_owned(),
        }),
        SearchToken::FieldFilter(trawl_core::ast::FieldFilter {
            field: "service".to_owned(),
            op: FilterOp::Eq,
            value: FilterValue::Literal("api".to_owned()),
        }),
    ]]
}

/// A phrase holding quotes and the word OR stays one phrase in the
/// widened query: it must not split into three groups that also match
/// `service=other`.
#[tokio::test]
async fn nearest_preserves_escaped_phrase_filters() {
    let backend = Fake {
        query: ok(nearest_answer()),
        ..Fake::default()
    };
    let empty = result(r#""alpha\" OR service=other OR \"omega" service=api last=1h"#);
    let hint = hints::nearest_bounds(&backend, &empty, at("2026-03-14T10:00:00Z")).await;
    assert!(hint.starts_with("nearest bounds over events"), "{hint}");
    let submitted = submitted_query(&backend);
    assert_eq!(
        search_terms(&submitted),
        phrase_and_service(r#"alpha" OR service=other OR "omega"#)
    );
    assert_eq!(submitted.pipeline.len(), 2);
}

/// Escaped backslashes, including one that ends the phrase, keep their
/// meaning in the widened query.
#[tokio::test]
async fn nearest_preserves_escaped_backslashes() {
    let backend = Fake {
        query: ok(nearest_answer()),
        ..Fake::default()
    };
    let empty = result(r#""c:\\tmp\\" service=api last=1h"#);
    let hint = hints::nearest_bounds(&backend, &empty, at("2026-03-14T10:00:00Z")).await;
    assert!(hint.starts_with("nearest bounds over events"), "{hint}");
    assert_eq!(
        search_terms(&submitted_query(&backend)),
        phrase_and_service(r"c:\tmp\")
    );
}
