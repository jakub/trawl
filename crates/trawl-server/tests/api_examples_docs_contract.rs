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
//!
//! Investigations: the `sequence_conflict` envelope in the chapter's
//! introduction decodes as `ErrorResponse` with its `conflict` member and
//! the server's own message, and every request and response example under
//! the investigation routes, "Attach a capture to an investigation"
//! included, decodes as the `trawl_api::investigations` or
//! `trawl_api::results` type the route takes or answers with (ADR-0059).
//! The examples agree with each other: a response echoes its request, an
//! overwrite answers the sequence after the one it expected, and a read
//! reaches the investigation's own `seq`. The limits and the fixed texts
//! the pages quote are the server's constants and messages.

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use trawl_api::capabilities::CapabilitiesResponse;
use trawl_api::ingest_preview::{PreviewEvent, PreviewResponse};
use trawl_api::investigations::{
    AppendEntry, AppendResponse, CreateInvestigation, EntryPayload, FindingStatusUpdate,
    Investigation, InvestigationList, InvestigationRead, InvestigationStatus, PatchInvestigation,
    RetentionExtension,
};
use trawl_api::results::{
    CONTEXT_SAME_TIME_ROWS, CONTEXT_STATEMENT, CaptureKind, CaptureRequest, CaptureResponse,
    CapturedResult, CompareRequest, ContextRequest, Correlation, CorrelationResponse, ResultCell,
    ResultList, ResultPage, ResultShape, ResultSource, ResultState, SightingsRequest,
};
use trawl_api::stream::SnapshotFrame;
use trawl_api::value::{Column, ColumnType, QueryResult, Value as Cell, land_cell};
use trawl_api::{ErrorCode, ErrorResponse, IngestResponse, QueryResponse, ReportRunResponse};
use trawl_server::error::ServerError;
use trawl_server::ingest::envelope::{RejectReason, RepairCode};
use trawl_server::investigations::service::{ACTIVITY_LIMIT, READ_DEFAULT_LIMIT, READ_MAX_LIMIT};
use trawl_server::results::service::GoneReason;

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

/// The text under the `## {heading}` chapter, up to its first `###`
/// section or the next chapter.
fn chapter_intro(heading: &str) -> &'static str {
    let marker = format!("\n## {heading}\n");
    let start = API
        .find(&marker)
        .unwrap_or_else(|| panic!("the API reference has a {heading:?} chapter"))
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

/// The JSON examples under `heading`, which must number `count`.
fn examples(heading: &str, count: usize) -> Vec<&'static str> {
    let examples = json_blocks(section(heading));
    assert_eq!(
        examples.len(),
        count,
        "{count} JSON examples under {heading:?}"
    );
    examples
}

#[test]
fn the_sequence_conflict_envelope_carries_both_sequences_and_the_server_text() {
    let heading = "Investigations";
    let examples = json_blocks(chapter_intro(heading));
    assert_eq!(examples.len(), 1, "one envelope in the {heading:?} intro");
    let response: ErrorResponse = decode_exactly(heading, examples[0]);
    assert_eq!(response.error.code, ErrorCode::SequenceConflict);
    let conflict = response
        .error
        .conflict
        .expect("a sequence_conflict envelope has a conflict member");
    assert_eq!(
        response.error.message,
        ServerError::SequenceConflict {
            expected: conflict.expected,
            current: conflict.current,
        }
        .to_string(),
        "the documented message is the server's"
    );
    assert!(conflict.current > conflict.expected);
}

#[test]
fn the_investigation_list_example_puts_open_ones_first() {
    let heading = "List investigations";
    let list: InvestigationList = decode_exactly(heading, examples(heading, 1)[0]);
    assert!(list.investigations.len() >= 2, "{heading:?} shows two");
    let open = list
        .investigations
        .iter()
        .take_while(|i| i.status == InvestigationStatus::Open)
        .count();
    assert!(
        list.investigations[open..]
            .iter()
            .all(|i| i.status == InvestigationStatus::Closed),
        "{heading:?} lists open investigations before closed ones"
    );
}

#[test]
fn the_create_examples_are_a_request_and_its_new_investigation() {
    let heading = "Create an investigation";
    let examples = examples(heading, 2);
    let request: CreateInvestigation = decode_exactly(heading, examples[0]);
    let created: Investigation = decode_exactly(heading, examples[1]);
    assert_eq!(
        (&created.title, &created.question, &created.scope),
        (&request.title, &request.question, &request.scope),
        "the response echoes the request"
    );
    assert_eq!(created.seq, 0, "creation is sequence 0");
    assert_eq!(created.status, InvestigationStatus::Open);
    assert_eq!(created.created_by, created.updated_by);
    assert_eq!(created.created_at, created.updated_at);
    assert!(created.retain_until <= created.retain_ceiling);
}

#[test]
fn the_read_example_is_a_board_that_reaches_its_investigation() {
    let heading = "Read an investigation";
    let text = section(heading);
    let read: InvestigationRead = decode_exactly(heading, examples(heading, 1)[0]);
    assert!(!read.more);
    assert_eq!(read.through_seq, read.investigation.seq);
    let updated: Vec<u64> = read.entries.iter().map(|e| e.updated_seq).collect();
    assert!(
        updated.windows(2).all(|pair| pair[0] < pair[1]),
        "{heading:?} entries are in updated_seq order"
    );
    for entry in &read.entries {
        assert!(entry.updated_seq >= entry.seq);
        if let EntryPayload::FindingStatus { finding_seq, .. } = entry.payload {
            assert!(
                read.entries
                    .iter()
                    .any(|f| f.seq == finding_seq
                        && matches!(f.payload, EntryPayload::Finding { .. })),
                "{heading:?} overwrites the status of a finding it shows"
            );
        }
    }
    assert!(
        read.entries
            .iter()
            .any(|e| matches!(e.payload, EntryPayload::Step { .. }) && e.updated_seq > e.seq),
        "{heading:?} shows a step whose outcome came later"
    );
    assert!(text.contains(&format!("Default `{READ_DEFAULT_LIMIT}`")));
    assert!(text.contains(&format!("1 to {READ_MAX_LIMIT}")));
}

#[test]
fn the_append_examples_are_three_entries_and_the_finding_answered() {
    let heading = "Append a keep, a finding, or a note";
    let examples = examples(heading, 4);
    let requests: Vec<AppendEntry> = examples[..3]
        .iter()
        .map(|example| decode_exactly(heading, example))
        .collect();
    assert!(matches!(requests[0], AppendEntry::Keep { .. }));
    assert!(matches!(requests[1], AppendEntry::Finding { .. }));
    assert!(matches!(requests[2], AppendEntry::Note { .. }));
    let response: AppendResponse = decode_exactly(heading, examples[3]);
    let AppendEntry::Finding {
        statement,
        references,
        seen_seq,
        ..
    } = &requests[1]
    else {
        unreachable!()
    };
    let EntryPayload::Finding {
        statement: written,
        references: cited,
        ..
    } = &response.entry.payload
    else {
        panic!("{heading:?} answers the finding it shows");
    };
    assert_eq!((written, cited), (statement, references));
    let activity = response
        .activity
        .as_ref()
        .expect("the finding sent seen_seq, so the answer carries activity");
    assert!(seen_seq.is_some());
    assert!(activity.through_seq >= response.entry.seq);
    assert!(section(heading).contains(&format!("at most {ACTIVITY_LIMIT}")));
}

#[test]
fn the_overwrite_examples_answer_the_sequence_after_the_expected_one() {
    let heading = "Overwrite a title, question, or status";
    let examples = examples(heading, 2);
    let patch: PatchInvestigation = decode_exactly(heading, examples[0]);
    let fields = [
        patch.title.is_some(),
        patch.question.is_some(),
        patch.status.is_some(),
    ];
    assert_eq!(
        fields.iter().filter(|f| **f).count(),
        1,
        "one field a patch"
    );
    let after: Investigation = decode_exactly(heading, examples[1]);
    assert_eq!(after.seq, patch.expected_seq + 1);
    assert_eq!(Some(after.status), patch.status);

    let heading = "Overwrite a finding's status";
    let examples = self::examples(heading, 2);
    let update: FindingStatusUpdate = decode_exactly(heading, examples[0]);
    let answer: AppendResponse = decode_exactly(heading, examples[1]);
    assert_eq!(answer.entry.seq, update.expected_seq + 1);
    assert!(
        answer.activity.is_none(),
        "an overwrite carries no activity"
    );
    assert!(matches!(
        answer.entry.payload,
        EntryPayload::FindingStatus { status, .. } if status == update.status
    ));

    let heading = "Extend evidence retention";
    let examples = self::examples(heading, 2);
    let extension: RetentionExtension = decode_exactly(heading, examples[0]);
    let extended: Investigation = decode_exactly(heading, examples[1]);
    assert_eq!(extended.seq, extension.expected_seq + 1);
    assert!(
        extended
            .retain_until
            .starts_with(extension.retain_until.trim_end_matches('Z'))
    );
    assert_eq!(extended.retain_until, extended.retain_ceiling);
}

#[test]
fn the_attached_capture_is_kept_from_birth_and_names_its_step() {
    let heading = "Attach a capture to an investigation";
    let examples = examples(heading, 2);
    let request: CaptureRequest = decode_exactly(heading, examples[0]);
    let attach = request
        .investigation
        .as_ref()
        .expect("the request attaches");
    let response: CaptureResponse = decode_exactly(heading, examples[1]);
    let result = &response.result;
    assert_eq!(result.state, ResultState::Kept);
    assert_eq!(result.expires_at, None, "a kept result has no deadline");
    assert_eq!(result.investigation_id.as_ref(), Some(&attach.id));
    assert_eq!(result.query, request.query);
    let step = response
        .step_seq
        .expect("an attached capture names its step");
    let activity = response
        .activity
        .as_ref()
        .expect("the request sent seen_seq");
    assert!(attach.seen_seq.is_some_and(|seen| seen < step));
    assert!(
        activity.through_seq > step,
        "the outcome advanced the sequence"
    );
}

#[test]
fn the_kept_evidence_section_quotes_the_retention_message() {
    let text = section("Kept evidence");
    assert!(
        text.contains(GoneReason::RetentionEnded.message()),
        "the section quotes the server's result_gone text for ended retention"
    );
}
