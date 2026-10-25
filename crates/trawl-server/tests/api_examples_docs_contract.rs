// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The API reference's response examples are wire values. Each JSON
//! example under the sections below must decode as the `trawl_api` type
//! the route answers with and encode back to the same JSON, so a renamed,
//! misspelled, or invented field fails here.
//!
//! Ingest: every `reason` an example under "Ingest events" and "Preview
//! ingest" shows must be a `RejectReason` wire code, and every repair a
//! `RepairCode`, imported rather than copied. The preview example was
//! captured from a real trawld; only `arrival` and `_ingested` vary
//! between runs, and both are free-form strings to the wire type.
//!
//! Results: the examples under "Result columns", "Run a query", "Report
//! run" and "Stream live events" decode through the typed result codec
//! (ADR-0035, amended 2026-10-22), so an untyped column, a JSON-number
//! integer or a cell that disagrees with its column's type fails here.
//!
//! Captured results: the request and response examples under "Capture a
//! result", "List captured results", "Read a page of a captured result"
//! and "Read one cell of a captured result" decode as the `trawl_api::results`
//! types the routes answer with (ADR-0057), and agree with themselves: a
//! page's positions name its columns, and a cell's ranges join into a cell
//! that lands under its type.
//!
//! Correlation reads: the request and response examples under "Read the
//! rows around a captured row", "Find where a literal appears" and
//! "Compare two windows" decode as `ContextRequest`, `SightingsRequest`,
//! `CompareRequest` and `CorrelationResponse` (ADR-0058), and agree with
//! themselves: the facts name the request's inputs and the result's
//! counts, and each `query` is the text trawl-core composes from the
//! request.
//!
//! Capabilities: the example under "Server capabilities" decodes as
//! `CapabilitiesResponse` (ADR-0058), and its DSL lists are names the
//! grammar knows.

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use trawl_api::capabilities::CapabilitiesResponse;
use trawl_api::ingest_preview::{PreviewEvent, PreviewResponse};
use trawl_api::results::{
    CONTEXT_SAME_TIME_ROWS, CONTEXT_STATEMENT, CaptureKind, CaptureRequest, CaptureResponse,
    CapturedResult, CompareRequest, ContextRequest, Correlation, CorrelationResponse, ResultCell,
    ResultList, ResultPage, ResultShape, ResultSource, SightingsRequest,
};
use trawl_api::stream::SnapshotFrame;
use trawl_api::value::{Column, ColumnType, QueryResult, Value as Cell, land_cell};
use trawl_api::{IngestResponse, QueryResponse, ReportRunResponse};
use trawl_server::ingest::envelope::{RejectReason, RepairCode};

const API: &str = include_str!("../../../docs/src/content/docs/reference/api.md");

/// The text under the `### {heading}` section, up to the next heading of
/// level 2 or 3.
fn section(heading: &str) -> &'static str {
    let marker = format!("\n### {heading}\n");
    let start = API
        .find(&marker)
        .unwrap_or_else(|| panic!("the API reference has a {heading:?} section"))
        + marker.len();
    let rest = &API[start..];
    let end = ["\n## ", "\n### "]
        .iter()
        .filter_map(|next| rest.find(next))
        .min()
        .unwrap_or(rest.len());
    &rest[..end]
}

/// Every fenced `json` block in `text`, in document order.
fn json_blocks(text: &str) -> Vec<&str> {
    let mut blocks = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find("```json\n") {
        let body = &rest[open + "```json\n".len()..];
        let close = body.find("\n```").expect("a closed json block");
        blocks.push(&body[..close]);
        rest = &body[close..];
    }
    blocks
}

/// Decode `example` as `T` and check it encodes back to the same JSON.
/// Serde ignores unknown fields when decoding, so the round trip is what
/// catches a field the wire type does not have.
fn decode_exactly<T: DeserializeOwned + Serialize>(heading: &str, example: &str) -> T {
    let documented: Value = serde_json::from_str(example)
        .unwrap_or_else(|e| panic!("{heading:?} example is not JSON: {e}\n{example}"));
    let decoded: T = serde_json::from_value(documented.clone()).unwrap_or_else(|e| {
        panic!("{heading:?} example does not decode as the wire type: {e}\n{example}")
    });
    let encoded = serde_json::to_value(&decoded).expect("encode the wire type");
    assert_eq!(
        encoded, documented,
        "{heading:?} example has fields the wire type drops or spells differently"
    );
    decoded
}

fn assert_reject_reason(heading: &str, reason: &str) {
    assert!(
        RejectReason::ALL
            .iter()
            .any(|known| known.as_str() == reason),
        "{heading:?} example shows reason {reason:?}, which is not a RejectReason wire code"
    );
}

fn assert_repair_code(heading: &str, code: &str) {
    assert!(
        RepairCode::ALL.iter().any(|known| known.as_str() == code),
        "{heading:?} example shows repair {code:?}, which is not a RepairCode wire code"
    );
}

#[test]
fn ingest_response_example_is_the_wire_type_with_real_reasons() {
    let heading = "Ingest events";
    let examples = json_blocks(section(heading));
    assert!(!examples.is_empty(), "no JSON example under {heading:?}");
    let mut reasons = 0;
    for example in examples {
        let response: IngestResponse = decode_exactly(heading, example);
        for error in &response.errors {
            assert_reject_reason(heading, &error.reason);
            reasons += 1;
        }
    }
    assert!(reasons > 0, "the {heading:?} examples show no reason");
}

#[test]
fn preview_response_example_is_the_wire_type_with_real_codes() {
    let heading = "Preview ingest";
    let examples = json_blocks(section(heading));
    assert!(!examples.is_empty(), "no JSON example under {heading:?}");
    for example in examples {
        let response: PreviewResponse = decode_exactly(heading, example);
        let mut accepted = 0;
        for event in &response.events {
            match event {
                PreviewEvent::Accepted {
                    event,
                    repairs,
                    lineage,
                    ..
                } => {
                    accepted += 1;
                    for code in repairs {
                        assert_repair_code(heading, code);
                    }
                    for change in &lineage.fields {
                        if let Some(code) = &change.code {
                            assert_repair_code(heading, code);
                        }
                    }
                    assert_eq!(
                        event.get("_repairs").and_then(Value::as_str),
                        Some(repairs.join(",").as_str()).filter(|joined| !joined.is_empty()),
                        "{heading:?} example's repairs disagree with its event's _repairs"
                    );
                    assert_eq!(
                        event.get("_ingested").and_then(Value::as_str),
                        Some(response.arrival.as_str()),
                        "{heading:?} example's arrival is not its events' _ingested"
                    );
                }
                PreviewEvent::Rejected { reason, .. } => assert_reject_reason(heading, reason),
            }
        }
        assert_eq!(
            (response.accepted, response.rejected),
            (accepted, response.events.len() - accepted),
            "{heading:?} example's counts disagree with its events"
        );
    }
}

#[test]
fn the_result_column_example_is_a_typed_column() {
    let heading = "Result columns";
    let examples = json_blocks(section(heading));
    assert_eq!(examples.len(), 1, "one column example under {heading:?}");
    let column: Column = decode_exactly(heading, examples[0]);
    assert_eq!(column.ty, ColumnType::list(ColumnType::Integer));
}

#[test]
fn the_query_response_example_carries_typed_big_integers() {
    let heading = "Run a query";
    let examples = json_blocks(section(heading));
    assert_eq!(examples.len(), 1, "one response example under {heading:?}");
    let response: QueryResponse = decode_exactly(heading, examples[0]);
    assert_eq!(
        response.result.columns,
        vec![
            Column::new("service", ColumnType::String),
            Column::new("count", ColumnType::Integer),
            Column::new("bytes", ColumnType::Integer),
        ]
    );
    assert_eq!(
        response.result.rows[0][2],
        Cell::UInt(u64::MAX),
        "the example shows an integer past i64::MAX"
    );
    assert_eq!(
        response.result.rows[1][2],
        Cell::Integer(9_007_199_254_740_993),
        "and one past 2^53"
    );
    assert_eq!(response.pagination.returned, response.result.rows.len());
}

#[test]
fn the_report_run_examples_are_typed_including_a_zero_row_run() {
    let heading = "Report run";
    let examples = json_blocks(section(heading));
    assert_eq!(
        examples.len(),
        2,
        "a run and a zero-row run under {heading:?}"
    );
    let runs: Vec<ReportRunResponse> = examples
        .iter()
        .map(|example| decode_exactly(heading, example))
        .collect();
    for run in &runs {
        let result = run.result.as_ref().expect("a successful run has a result");
        assert_eq!(
            run.summary.row_count,
            Some(result.rows.len()),
            "{heading:?} example's row_count disagrees with its rows"
        );
        assert!(
            result.columns.iter().any(|c| c.ty == ColumnType::Integer),
            "{heading:?} example shows an integer column"
        );
    }
    assert!(runs[1].result.as_ref().unwrap().rows.is_empty());
}

#[test]
fn the_stream_examples_are_typed_result_frames() {
    let heading = "Stream live events";
    let text = section(heading);
    let examples = json_blocks(text);
    assert_eq!(
        examples.len(),
        2,
        "a data frame and a snapshot under {heading:?}"
    );
    let data: QueryResult = decode_exactly(heading, examples[0]);
    assert_eq!(data.rows.len(), 1, "a data frame is one row");
    let snapshot: SnapshotFrame = decode_exactly(heading, examples[1]);
    assert!(
        !snapshot.capped.is_empty(),
        "{heading:?} snapshot example shows capped"
    );

    // The SSE transcript's frames are wire values too.
    let mut frames = 0;
    let mut event = "";
    for line in text.lines() {
        if let Some(name) = line.strip_prefix("event: ") {
            event = name;
        } else if let Some(body) = line.strip_prefix("data: ") {
            frames += 1;
            match event {
                "data" => {
                    let frame: QueryResult = decode_exactly(heading, body);
                    assert_eq!(frame, data, "the transcript shows the documented frame");
                }
                "lagged" => {
                    let lagged: Value = serde_json::from_str(body).unwrap();
                    assert!(lagged["missed"].is_u64(), "{body}");
                }
                other => panic!("{heading:?} transcript shows an unknown event {other:?}"),
            }
        }
    }
    assert_eq!(frames, 2, "the transcript shows a data and a lagged frame");
}

#[test]
fn the_capture_examples_are_a_request_and_a_cut_event_capture() {
    let heading = "Capture a result";
    let examples = json_blocks(section(heading));
    assert_eq!(
        examples.len(),
        2,
        "a request and a response under {heading:?}"
    );
    let request: CaptureRequest = decode_exactly(heading, examples[0]);
    let window = request.window.expect("the request example shows a window");
    assert!(window.last.is_some(), "{heading:?} request uses last");

    let response: CaptureResponse = decode_exactly(heading, examples[1]);
    let result = &response.result;
    assert_eq!(result.query, request.query, "the response echoes the query");
    assert_eq!(result.source, ResultSource::Live);
    assert_eq!(result.shape, ResultShape::Events);
    assert!(
        result.executed_query.ends_with("| sort -_time, -_ingested"),
        "an events capture records the imposed order"
    );
    assert!(result.truncated && result.captured_rows < result.total_rows);
    assert_eq!(result.column_facts.len(), result.columns.len());
    assert!(response.execution.is_some());
}

#[test]
fn the_capture_list_example_is_a_result_list() {
    let heading = "List captured results";
    let examples = json_blocks(section(heading));
    assert_eq!(examples.len(), 1, "one response example under {heading:?}");
    let list: ResultList = decode_exactly(heading, examples[0]);
    assert!(!list.results.is_empty());
    for result in &list.results {
        assert_eq!(result.truncated, result.captured_rows < result.total_rows);
        assert_eq!(result.column_facts.len(), result.columns.len());
    }
}

#[test]
fn the_result_page_example_projects_by_position() {
    let heading = "Read a page of a captured result";
    let examples = json_blocks(section(heading));
    assert_eq!(examples.len(), 1, "one response example under {heading:?}");
    let page: ResultPage = decode_exactly(heading, examples[0]);
    assert_eq!(page.positions.len(), page.page.columns.len());
    for (position, column) in page.positions.iter().zip(&page.page.columns) {
        let stored = &page.result.columns[*position];
        assert_eq!(
            (&stored.name, &stored.ty),
            (&column.name, &column.ty),
            "{heading:?} position {position} names its column"
        );
    }
    assert_eq!(page.pagination.returned, page.page.rows.len());
    assert_eq!(
        page.pagination.total as u64, page.result.captured_rows,
        "a page's total is the captured rows"
    );
}

#[test]
fn the_cell_examples_join_into_one_cell() {
    let heading = "Read one cell of a captured result";
    let examples = json_blocks(section(heading));
    assert_eq!(
        examples.len(),
        2,
        "two ranges of one cell under {heading:?}"
    );
    let first: ResultCell = decode_exactly(heading, examples[0]);
    let last: ResultCell = decode_exactly(heading, examples[1]);
    assert_eq!(first.offset, 0);
    assert_eq!(first.next_offset, Some(last.offset));
    assert_eq!(last.next_offset, None);
    for range in [&first, &last] {
        assert_eq!(range.length, range.text.len() as u64);
        assert_eq!(range.total_bytes, first.total_bytes);
    }
    let whole = format!("{}{}", first.text, last.text);
    assert_eq!(whole.len() as u64, first.total_bytes);
    let cell: Value = serde_json::from_str(&whole).expect("the ranges join into JSON");
    assert!(matches!(
        land_cell(&first.ty, cell).expect("the joined text lands under its type"),
        Cell::String(_)
    ));
}

#[test]
fn the_capabilities_example_is_the_wire_type_with_known_dsl_names() {
    let heading = "Server capabilities";
    let examples = json_blocks(section(heading));
    assert_eq!(examples.len(), 1, "one response example under {heading:?}");
    let response: CapabilitiesResponse = decode_exactly(heading, examples[0]);
    let known = trawl_core::parser::suggest::KNOWN_PIPE_STAGES;
    for stage in &response.dsl.stages {
        assert!(known.contains(&stage.as_str()), "unknown stage {stage:?}");
    }
    let known = trawl_core::parser::suggest::KNOWN_FUNCTIONS;
    for function in &response.dsl.functions {
        assert!(
            known.contains(&function.as_str()),
            "unknown function {function:?}"
        );
    }
    assert_eq!(
        response.dsl.max_query_bytes,
        trawl_core::parser::MAX_QUERY_LEN as u64
    );
    assert_eq!(
        response.dsl.max_pipeline_stages as usize,
        trawl_core::complexity::MAX_PIPELINE_STAGES
    );
}

/// What every correlation response example shares: its kind, one fact per
/// column, `truncated` read from its counts, a result id, an executed text
/// that is the query under its window, and listed column values that count
/// every kept row.
fn assert_correlated(heading: &str, result: &CapturedResult, kind: CaptureKind, rows: u64) {
    assert_eq!(result.kind, kind, "{heading:?} kind");
    assert_eq!(
        result.column_facts.len(),
        result.columns.len(),
        "{heading:?}"
    );
    assert_eq!(result.truncated, result.captured_rows < result.total_rows);
    assert!(
        result.id.len() == 32
            && result
                .id
                .chars()
                .all(|c| c.is_ascii_digit() || matches!(c, 'a'..='f')),
        "{heading:?} id {:?}",
        result.id
    );
    let bounded = format!(
        "earliest=\"{}\" latest=\"{}\" {}",
        result.earliest.as_deref().expect("a window start"),
        result.latest.as_deref().expect("a window end"),
        result.query
    );
    let executed = match result.shape {
        ResultShape::Events => format!("{bounded}\n| sort -_time, -_ingested"),
        ResultShape::Whole => bounded,
    };
    assert_eq!(
        result.executed_query, executed,
        "{heading:?} executed query"
    );
    for (column, facts) in result.columns.iter().zip(&result.column_facts) {
        if let Some(values) = &facts.values {
            assert_eq!(
                values.iter().map(|v| v.count).sum::<u64>() + facts.nulls,
                rows,
                "{heading:?} values of {}",
                column.name
            );
        }
    }
}

/// An RFC 3339 instant as UTC microseconds.
fn micros(at: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(at)
        .expect("an RFC 3339 instant")
        .timestamp_micros()
}

#[test]
fn the_context_examples_are_a_request_and_its_neighbourhood() {
    let heading = "Read the rows around a captured row";
    let examples = json_blocks(section(heading));
    assert_eq!(
        examples.len(),
        2,
        "a request and a response under {heading:?}"
    );
    let request: ContextRequest = decode_exactly(heading, examples[0]);
    let response: CorrelationResponse = decode_exactly(heading, examples[1]);
    let result = &response.result;
    assert_correlated(heading, result, CaptureKind::Context, result.captured_rows);
    assert_eq!(result.shape, ResultShape::Events);
    let Some(Correlation::Context(facts)) = &result.correlation else {
        panic!("{heading:?} shows context facts")
    };
    assert_eq!(
        (facts.focus.result_id.as_str(), facts.focus.row),
        (request.result_id.as_str(), request.row)
    );
    assert_eq!(facts.scope_used.seconds, request.seconds);
    assert_eq!(facts.statement, CONTEXT_STATEMENT);
    let groups = &facts.groups;
    assert_eq!(groups.earlier.start, 0);
    assert_eq!(groups.earlier.end, groups.same_time.start);
    assert_eq!(groups.same_time.end, groups.later.start);
    assert_eq!(groups.later.end, result.captured_rows);
    assert!(groups.earlier.end - groups.earlier.start <= request.before);
    assert!(groups.later.end - groups.later.start <= request.after);
    assert!(groups.same_time.end - groups.same_time.start <= CONTEXT_SAME_TIME_ROWS);
    let composed = trawl_core::correlate::context_query(
        facts.scope_used.env.as_deref(),
        facts.scope_used.host.as_deref(),
    )
    .expect("the scope composes");
    assert_eq!(result.query, composed.text(), "{heading:?} query");
}

#[test]
fn the_sightings_examples_count_both_populations() {
    let heading = "Find where a literal appears";
    let examples = json_blocks(section(heading));
    assert_eq!(
        examples.len(),
        2,
        "a request and a response under {heading:?}"
    );
    let request: SightingsRequest = decode_exactly(heading, examples[0]);
    let response: CorrelationResponse = decode_exactly(heading, examples[1]);
    let result = &response.result;
    let Some(Correlation::Sightings(facts)) = &result.correlation else {
        panic!("{heading:?} shows sightings facts")
    };
    assert_correlated(heading, result, CaptureKind::Sightings, facts.captured);
    assert_eq!(facts.literal, request.literal);
    assert_eq!(facts.attribution_fields, request.fields);
    assert_eq!(facts.matches_total, result.total_rows);
    assert_eq!(facts.captured, result.captured_rows);
    assert_eq!(
        facts.groups.iter().map(|g| g.count).sum::<u64>(),
        facts.matches_total,
        "{heading:?} groups count every match"
    );
    assert!(facts.groups.windows(2).all(|w| w[0].count >= w[1].count));
    assert_eq!(
        (
            facts.denominators.field_counts_over,
            facts.denominators.service_counts_over
        ),
        (facts.captured, facts.matches_total)
    );
    assert!(
        facts
            .fields
            .iter()
            .all(|f| request.fields.is_empty() || request.fields.contains(&f.field)),
        "{heading:?} lists a field outside the restriction"
    );
    if !request.fields.is_empty() {
        assert_eq!(facts.fields_omitted, 0, "a restricted read omits nothing");
    }
    let composed =
        trawl_core::correlate::sightings_query(&request.literal).expect("the literal composes");
    assert_eq!(result.query, composed.text(), "{heading:?} query");
    let busiest = facts.groups.first().and_then(|g| g.service.as_deref());
    let follow_ups: Vec<String> = trawl_core::correlate::follow_ups(&request.literal, busiest)
        .iter()
        .map(|c| c.text().to_owned())
        .collect();
    assert_eq!(facts.follow_ups, follow_ups, "{heading:?} follow-ups");
}

#[test]
fn the_compare_examples_rank_one_statement_over_both_windows() {
    let heading = "Compare two windows";
    let examples = json_blocks(section(heading));
    assert_eq!(
        examples.len(),
        2,
        "a request and a response under {heading:?}"
    );
    let request: CompareRequest = decode_exactly(heading, examples[0]);
    let response: CorrelationResponse = decode_exactly(heading, examples[1]);
    let result = &response.result;
    let Some(Correlation::Compare(facts)) = &result.correlation else {
        panic!("{heading:?} shows compare facts")
    };
    assert_correlated(heading, result, CaptureKind::Compare, facts.rows_ranked);
    assert_eq!(result.shape, ResultShape::Whole);
    assert_eq!(facts.group_by, request.group_by);
    assert_eq!(facts.min_count, request.min_count);
    assert_eq!(facts.rows_ranked, result.captured_rows);
    assert_eq!(
        result.earliest.as_deref(),
        Some(facts.baseline.start.as_str())
    );
    assert_eq!(
        result.latest.as_deref(),
        Some(facts.foreground.end.as_str())
    );
    assert!(facts.warnings.len() <= 4);
    for column in &result.columns[facts.group_by.len()..] {
        assert!(
            column.name.starts_with("compare_"),
            "{heading:?} column {} is the statement's own",
            column.name
        );
    }
    let window = |w: &trawl_api::results::CompareWindow| trawl_core::correlate::Window {
        start_us: micros(&w.start),
        end_us: micros(&w.end),
    };
    let composed = trawl_core::correlate::compare_query(
        &request.filter,
        window(&facts.foreground),
        window(&facts.baseline),
        &request.group_by,
        request.min_count,
    )
    .expect("the request composes");
    assert_eq!(result.query, composed.text(), "{heading:?} query");
}
