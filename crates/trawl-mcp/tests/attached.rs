// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `search`, `context`, `sightings` and `compare` attached to an
//! investigation, and `orient`'s open investigations, over a recording
//! backend (ADR-0059, #310, #309).
//!
//! An attached execute sends the session's `seen_seq` with the capture and
//! prints the activity trawld answers with under the same exactly-once
//! rules as an append; `validate` ignores the attachment and says so; the
//! id's shape and the purpose's length are refused before any request;
//! trawld's refusals name the way back. The correlation reads attach as
//! `search` does, and a sightings block's lists give way only after the
//! activity entries do. `orient` lists open investigations
//! only for a key that may read them, and a list it cannot read is one
//! line.

mod support;

use chrono::{DateTime, Utc};
use rmcp::model::CallToolResult;
use serde_json::{Value as Json, json};
use trawl_api::investigations::{
    Activity, InvestigationId, InvestigationList, InvestigationRead, InvestigationStatus,
    InvestigationSummary, LogEntry,
};
use trawl_api::value::{ColumnType, Value};
use trawl_client::{
    CaptureKind, CaptureResponse, CapturedResult, CompareFacts, CompareWarning, CompareWarningCode,
    CompareWindow, ContextFacts, ContextGroups, Correlation, CorrelationResponse, ErrorCode,
    FieldSightings, FocusRow, RateUnit, ResultPage, RowRange, ScopeUsed, SightingGroup,
    SightingsDenominators, SightingsFacts,
};
use trawl_mcp::session::Seen;
use trawl_mcp::tools::schema::{SEARCH, SearchInput, tools};
use trawl_mcp::tools::{investigation, orient, search};

use support::*;

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-03-14T04:05:06Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn text_of(result: &CallToolResult) -> &str {
    assert_eq!(result.content.len(), 1, "one text block");
    &result.content[0].as_text().expect("a text block").text
}

fn used_bytes(result: &CallToolResult) -> usize {
    text_of(result).len()
        + result
            .structured_content
            .as_ref()
            .map_or(0, |mirror| mirror.to_string().len())
}

fn id() -> InvestigationId {
    InvestigationId::parse(INVESTIGATION_ID).unwrap()
}

/// Two short captured rows, and their first page.
fn two_rows() -> (CapturedResult, ResultPage) {
    let captured = captured(
        "service=api",
        &[("message", ColumnType::String)],
        vec![no_values(2)],
        2,
        2,
    );
    let page = page(&captured, vec![vec![s("one")], vec![s("two")]], 0);
    (captured, page)
}

/// A backend whose capture answers `activity`, kept for the
/// investigation, with the capture's step at `step_seq`.
fn attached_backend(activity: Option<Activity>, step_seq: u64) -> Fake {
    let (mut captured, page) = two_rows();
    captured.investigation_id = Some(id());
    Fake {
        capabilities: ok(capabilities(None)),
        capture: ok(CaptureResponse {
            result: captured,
            execution: None,
            activity,
            step_seq: Some(step_seq),
        }),
        result_page: ok(page),
        ..Fake::default()
    }
}

/// An attached backend whose capture, kept with its step at 7, executed
/// `service=api` and 4096 spaces: a text no 4096-byte reply can print.
fn unprintable_backend() -> Fake {
    let query = format!("service=api{}", " ".repeat(4096));
    let mut captured = captured(
        &query,
        &[("message", ColumnType::String)],
        vec![no_values(2)],
        2,
        2,
    );
    captured.investigation_id = Some(id());
    let page = page(&captured, vec![vec![s("one")], vec![s("two")]], 0);
    Fake {
        capabilities: ok(capabilities(None)),
        capture: ok(CaptureResponse {
            result: captured,
            execution: None,
            activity: None,
            step_seq: Some(7),
        }),
        result_page: ok(page),
        ..Fake::default()
    }
}

async fn run(backend: &Fake, seen: &Seen, arguments: Json) -> CallToolResult {
    let input: SearchInput = serde_json::from_value(arguments).expect("a search input");
    search::run(backend, seen, input, now()).await
}

fn execute(extra: Json) -> Json {
    let mut arguments = json!({"mode": "execute", "query": "service=api",
                               "investigation": INVESTIGATION_ID});
    let Json::Object(extra) = extra else {
        panic!("extra arguments are an object");
    };
    arguments.as_object_mut().unwrap().extend(extra);
    arguments
}

fn assert_valid_mirror(result: &CallToolResult) {
    let tool = tools().into_iter().find(|t| t.name == SEARCH).unwrap();
    let schema = Json::Object((*tool.output_schema.unwrap()).clone());
    let validator = jsonschema::draft202012::new(&schema).unwrap();
    let mirror = result.structured_content.as_ref().expect("a mirror");
    let errors: Vec<String> = validator
        .iter_errors(mirror)
        .map(|e| e.to_string())
        .collect();
    assert!(errors.is_empty(), "{errors:?}");
}

// -- attached search ----------------------------------------------------------------

#[tokio::test]
async fn an_attached_execute_sends_seen_seq_prints_the_activity_and_advances_seen() {
    let backend = attached_backend(
        Some(Activity {
            entries: vec![note(2, human(), "theirs")],
            through_seq: 2,
            more: false,
        }),
        3,
    );
    let seen = Seen::default();
    seen.lock().await.touch(&id(), 1);
    let result = run(
        &backend,
        &seen,
        execute(json!({"purpose": "find the first 502"})),
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{}", text_of(&result));
    assert_eq!(
        backend.calls()[1],
        format!(
            "capture service=api attach {{\"id\":\"{INVESTIGATION_ID}\",\"seen_seq\":1,\
             \"purpose\":\"find the first 502\"}}"
        )
    );
    let text = text_of(&result);
    assert!(
        text.contains(&format!(
            "\ninvestigation: \"{INVESTIGATION_ID}\"; kept result \"{RESULT_ID}\"\n\
             activity by other keys since #1, in change order: 1 entry\n\
             #2 note by Zy9x8w7v \"jakub\" at 2026-10-24T09:05:00.000000Z: \"theirs\"\n"
        )),
        "{text}"
    );
    let mirror = result.structured_content.as_ref().unwrap();
    assert_eq!(
        mirror["investigation"]["activity"]["printed_through"],
        json!(2)
    );
    assert_eq!(seen.lock().await.get(&id()), Some(2));
    assert_valid_mirror(&result);
}

/// The first attached execute of a session has seen nothing, so it sends
/// no `seen_seq`; the answer's `step_seq` is where the session starts
/// following, and the next attached execute sends it.
#[tokio::test]
async fn the_first_attached_execute_sends_no_seen_seq_and_says_where_following_starts() {
    let backend = attached_backend(None, 7);
    let seen = Seen::default();
    let result = run(&backend, &seen, execute(json!({}))).await;
    assert_ne!(result.is_error, Some(true), "{}", text_of(&result));
    assert_eq!(
        captures(&backend),
        [format!(
            "capture service=api attach {{\"id\":\"{INVESTIGATION_ID}\"}}"
        )]
    );
    assert!(
        text_of(&result).contains(&format!(
            "activity: this session follows this investigation from this step on; what \
             other keys wrote before it is on the board, which investigation action=read \
             id={INVESTIGATION_ID} since=0 prints"
        )),
        "{}",
        text_of(&result)
    );
    assert_eq!(seen.lock().await.get(&id()), Some(7));
    assert_valid_mirror(&result);

    let result = run(&backend, &seen, execute(json!({}))).await;
    assert_ne!(result.is_error, Some(true), "{}", text_of(&result));
    assert_eq!(
        captures(&backend).last().unwrap(),
        &format!("capture service=api attach {{\"id\":\"{INVESTIGATION_ID}\",\"seen_seq\":7}}")
    );
}

/// The first attached execute follows its step only once its reply fitted:
/// a page that fails after the capture, or a reply that does not fit,
/// leaves the session as untouched as before the call, and the next call
/// sends no `seen_seq` again.
#[tokio::test]
async fn the_first_attached_execute_follows_its_step_only_after_its_reply_fits() {
    let mut failed_page = attached_backend(None, 7);
    failed_page.result_page = refuse(503, ErrorCode::ServiceUnavailable, "starting");
    let seen = Seen::default();
    let result = run(&failed_page, &seen, execute(json!({}))).await;
    assert_eq!(result.is_error, Some(true), "{}", text_of(&result));
    assert_eq!(seen.lock().await.get(&id()), None, "a failed page");

    let unfitted = unprintable_backend();
    let result = run(&unfitted, &seen, execute(json!({"max_bytes": 4096}))).await;
    assert_eq!(result.is_error, Some(true), "{}", text_of(&result));
    assert_eq!(seen.lock().await.get(&id()), None, "an unfitted reply");

    let fitted = attached_backend(None, 7);
    let result = run(&fitted, &seen, execute(json!({}))).await;
    assert_ne!(result.is_error, Some(true), "{}", text_of(&result));
    assert_eq!(
        captures(&fitted),
        [format!(
            "capture service=api attach {{\"id\":\"{INVESTIGATION_ID}\"}}"
        )],
        "the failed calls left nothing to send"
    );
    assert_eq!(seen.lock().await.get(&id()), Some(7), "a fitted reply");
}

/// An attached capture trawld accepted is a step of the investigation
/// whatever happens after it: a page that fails, or a reply that does not
/// fit, is an error that opens by naming the investigation and the result
/// kept for it.
#[tokio::test]
async fn an_attached_search_that_succeeded_but_cannot_print_keeps_its_acknowledgement() {
    let mut failed_page = attached_backend(None, 7);
    failed_page.result_page = refuse(503, ErrorCode::ServiceUnavailable, "starting");
    for (label, backend) in [
        ("failed page", failed_page),
        ("unfitted", unprintable_backend()),
    ] {
        let result = run(
            &backend,
            &Seen::default(),
            execute(json!({"max_bytes": 4096})),
        )
        .await;
        assert_eq!(result.is_error, Some(true), "{label}: {}", text_of(&result));
        assert!(
            text_of(&result).starts_with(&format!(
                "Investigation \"{INVESTIGATION_ID}\" recorded this search as a step and keeps \
                 result \"{RESULT_ID}\" for it.\n"
            )),
            "{label}: {}",
            text_of(&result)
        );
    }
}

/// A page refusal after an accepted capture is capped once: the
/// acknowledgement opens the text, the line naming the captured result and
/// the `retry after` and `request id` lines end it, and only trawld's
/// message is cut. At the sizes two reviewers found the second cap
/// clipping the trailer (a 7933-byte 503, a 7965-byte 429 with a wait),
/// one byte either side, and far past the cap; attached and not.
#[tokio::test]
async fn a_page_refusal_after_a_capture_keeps_its_acknowledgement_and_its_trailer() {
    use trawl_client::{ClientError, ErrorEnvelope};
    use trawl_mcp::render::refusal::MAX_ERROR_BYTES;

    let refusal = |status: u16, bytes: usize, request_id: &str, wait: Option<u32>| {
        let request_id = request_id.to_owned();
        let code = if status == 429 {
            ErrorCode::RateLimited
        } else {
            ErrorCode::ServiceUnavailable
        };
        let handler: support::Handler<ResultPage> = Box::new(move || {
            Err(ClientError::Server {
                status,
                error: ErrorEnvelope::simple(code.clone(), "m".repeat(bytes)),
                request_id: Some(request_id.clone()),
                retry_after_secs: wait,
            })
        });
        handler
    };
    let acknowledgement = format!(
        "Investigation \"{INVESTIGATION_ID}\" recorded this search as a step and keeps result \
         \"{RESULT_ID}\" for it.\n"
    );
    let captured_line =
        format!("The result \"{RESULT_ID}\" was captured; read it with read action=page.");
    let cases = [
        (
            503,
            "search was refused: service_unavailable (HTTP 503).",
            "req-0001",
            None,
            "request id: req-0001".to_owned(),
        ),
        (
            429,
            "search was refused: rate_limited (HTTP 429).",
            "probe-request-429",
            Some(12),
            "retry after: 12 seconds\nrequest id: probe-request-429".to_owned(),
        ),
    ];
    for (status, failure, request_id, wait, trailer) in cases {
        let reviewed = if status == 429 { 7965 } else { 7933 };
        for bytes in [reviewed - 1, reviewed, reviewed + 1, 4 * MAX_ERROR_BYTES] {
            for attached in [true, false] {
                let label = format!("{status} at {bytes} bytes, attached {attached}");
                let mut backend = attached_backend(None, 7);
                backend.result_page = Some(refusal(status, bytes, request_id, wait));
                let arguments = if attached {
                    execute(json!({}))
                } else {
                    json!({"mode": "execute", "query": "service=api"})
                };
                let result = run(&backend, &Seen::default(), arguments).await;
                assert_eq!(result.is_error, Some(true), "{label}");
                let text = text_of(&result);
                assert!(text.len() <= MAX_ERROR_BYTES, "{label}: {}", text.len());
                assert_eq!(
                    text.starts_with(&acknowledgement),
                    attached,
                    "{label}: {text:.300}"
                );
                assert!(
                    text.ends_with(&format!("\n{captured_line}\n{trailer}")),
                    "{label}: …{}",
                    &text[text.len().saturating_sub(300)..]
                );
                // Whole, the text would be this long; past the cap, only
                // trawld's message is cut, and the cut says so.
                let whole = usize::from(attached) * acknowledgement.len()
                    + failure.len()
                    + "\ntrawld says: ".len()
                    + bytes
                    + 1
                    + captured_line.len()
                    + 1
                    + trailer.len();
                let cut = text.contains("\n…[error text cut at 8192 bytes]\n");
                assert_eq!(cut, whole > MAX_ERROR_BYTES, "{label}: {whole}");
                if !cut {
                    assert_eq!(text.len(), whole, "{label}");
                }
                let opening = if attached {
                    format!("{acknowledgement}{failure}\ntrawld says: mmm")
                } else {
                    format!("{failure}\ntrawld says: mmm")
                };
                assert!(text.starts_with(&opening), "{label}: {text:.300}");
            }
        }
    }
}

/// The longest lead and tail a page refusal after an accepted capture can
/// carry leave the head nearly all of the cap, so the cap cuts only
/// trawld's message: the acknowledgement names two ids of 32 hex digits,
/// the captured line one, the wait is at most `u32::MAX` seconds or the
/// longer sentence for none, and trawl-client reads a request id of at most
/// `MAX_REQUEST_ID_BYTES`, or none at all, which leaves the rest whole.
#[tokio::test]
async fn the_longest_lead_and_tail_leave_the_head_nearly_all_of_the_cap() {
    use trawl_client::{ClientError, ErrorEnvelope, MAX_REQUEST_ID_BYTES};
    use trawl_mcp::render::refusal::MAX_ERROR_BYTES;

    const CUT: &str = "\n…[error text cut at 8192 bytes]\n";
    let longest_id = "r".repeat(MAX_REQUEST_ID_BYTES);
    let acknowledgement = format!(
        "Investigation \"{INVESTIGATION_ID}\" recorded this search as a step and keeps result \
         \"{RESULT_ID}\" for it.\n"
    );
    let captured_line =
        format!("The result \"{RESULT_ID}\" was captured; read it with read action=page.");
    let request_line = format!("\nrequest id: {longest_id}");
    let cases = [
        (
            Some(u32::MAX),
            Some(longest_id.as_str()),
            "retry after: 4294967295 seconds",
            request_line.as_str(),
        ),
        (
            None,
            Some(longest_id.as_str()),
            "retry after: trawld named no wait",
            request_line.as_str(),
        ),
        (Some(7), None, "retry after: 7 seconds", ""),
    ];
    let mut longest = 0;
    for (wait, request_id, retry, request) in cases {
        let request_id = request_id.map(str::to_owned);
        let mut backend = attached_backend(None, 7);
        backend.result_page = Some(Box::new(move || {
            Err(ClientError::Server {
                status: 429,
                error: ErrorEnvelope::simple(
                    ErrorCode::RateLimited,
                    "m".repeat(4 * MAX_ERROR_BYTES),
                ),
                request_id: request_id.clone(),
                retry_after_secs: wait,
            })
        }));
        let result = run(&backend, &Seen::default(), execute(json!({}))).await;
        assert_eq!(result.is_error, Some(true), "{retry}");
        let text = text_of(&result);
        let tail = format!("{captured_line}\n{retry}{request}");
        assert!(text.starts_with(&acknowledgement), "{text:.300}");
        assert!(
            text.ends_with(&format!("{CUT}{tail}")),
            "…{}",
            &text[text.len().saturating_sub(300)..]
        );
        // The head of ASCII fills the room the lead and tail leave.
        assert_eq!(text.len(), MAX_ERROR_BYTES, "{retry}");
        longest = longest.max(acknowledgement.len() + CUT.len() + tail.len());
    }
    assert!(longest <= MAX_ERROR_BYTES / 16, "{longest}");
}

/// An id no trawld mints, which names nothing a read can page.
const MALFORMED_RESULT_ID: &str = "NOT-A-RESULT-ID\u{202e}";

/// Another investigation than [`INVESTIGATION_ID`].
const OTHER_INVESTIGATION_ID: &str = "00000000000000000000000000000001";

/// A backend whose attached capture answers result `result_id` kept for
/// `kept_for`, with its step at 7.
fn answering(result_id: &str, kept_for: Option<&str>) -> Fake {
    let mut backend = attached_backend(None, 7);
    let (mut captured, page) = two_rows();
    result_id.clone_into(&mut captured.id);
    captured.investigation_id = kept_for.map(|id| InvestigationId::parse(id).unwrap());
    backend.capture = ok(CaptureResponse {
        result: captured,
        execution: None,
        activity: None,
        step_seq: Some(7),
    });
    backend.result_page = ok(page);
    backend
}

/// A capture answer the server cannot stand behind (a result id trawld
/// does not mint, or a result not kept for the investigation the search
/// named) is a protocol error: it says what is unknown and that the board
/// shows what was kept, certifies no result, reads no page, prints no id
/// trawld sent, and leaves the session as it was.
#[tokio::test]
async fn an_attached_capture_answer_for_an_unreadable_or_foreign_result_certifies_nothing() {
    for (label, backend, reason) in [
        (
            "malformed id",
            answering(MALFORMED_RESULT_ID, Some(INVESTIGATION_ID)),
            "its capture answer names a result id that is not 32 lowercase hex digits",
        ),
        (
            "another investigation",
            answering(RESULT_ID, Some(OTHER_INVESTIGATION_ID)),
            "its capture answer names another investigation as the one the result is kept for",
        ),
        (
            "no investigation",
            answering(RESULT_ID, None),
            "its capture answer names no investigation the result is kept for",
        ),
    ] {
        let seen = Seen::default();
        seen.lock().await.touch(&id(), 4);
        let result = run(&backend, &seen, execute(json!({}))).await;
        assert_eq!(result.is_error, Some(true), "{label}: {}", text_of(&result));
        assert!(result.structured_content.is_none(), "{label}");
        let text = text_of(&result);
        assert_eq!(
            text,
            format!(
                "search with investigation cannot tell what trawld kept: {reason}. \
                 Investigation \"{INVESTIGATION_ID}\" may hold this search as a step; \
                 investigation action=read id={INVESTIGATION_ID} since=4 prints the entries \
                 changed after #4, and a step for this search names the result it keeps."
            ),
            "{label}"
        );
        for absent in ["NOT-A-RESULT", OTHER_INVESTIGATION_ID, "keeps result"] {
            assert!(!text.contains(absent), "{label}: {absent:?} in {text}");
        }
        assert!(
            !backend.methods().contains(&"result_page".to_owned()),
            "{label}: {:?}",
            backend.calls()
        );
        assert_eq!(seen.lock().await.get(&id()), Some(4), "{label}");
    }

    // A session that had seen nothing reads the board from its start.
    let backend = answering(MALFORMED_RESULT_ID, Some(INVESTIGATION_ID));
    let seen = Seen::default();
    let result = run(&backend, &seen, execute(json!({}))).await;
    assert!(
        text_of(&result).contains(&format!(
            "investigation action=read id={INVESTIGATION_ID} since=0 prints"
        )),
        "{}",
        text_of(&result)
    );
    assert_eq!(seen.lock().await.get(&id()), None);
}

/// An unattached capture answer whose result id trawld does not mint is
/// an error that prints no id and reads no page.
#[tokio::test]
async fn a_capture_answer_with_a_malformed_result_id_reads_no_page() {
    let backend = answering(MALFORMED_RESULT_ID, None);
    let input = json!({"mode": "execute", "query": "service=api"});
    let result = run(&backend, &Seen::default(), input).await;
    assert_eq!(result.is_error, Some(true), "{}", text_of(&result));
    assert_eq!(
        text_of(&result),
        "search failed: trawld's capture answer names a result id that is not 32 lowercase hex \
         digits, so no read can page the result. Run the search again."
    );
    assert_eq!(backend.methods(), ["capabilities", "capture"]);
}

/// The capture calls the backend saw, in order.
fn captures(backend: &Fake) -> Vec<String> {
    backend
        .calls()
        .into_iter()
        .filter(|call| call.starts_with("capture "))
        .collect()
}

#[tokio::test]
async fn a_cut_attached_activity_continues_from_the_last_entry_it_printed() {
    let theirs: Vec<LogEntry> = (2..=31)
        .map(|seq| note(seq, human(), &format!("{seq}: {}", "w".repeat(300))))
        .collect();
    let backend = attached_backend(
        Some(Activity {
            entries: theirs,
            through_seq: 31,
            more: false,
        }),
        32,
    );
    let seen = Seen::default();
    seen.lock().await.touch(&id(), 1);
    let result = run(&backend, &seen, execute(json!({"max_bytes": 4096}))).await;
    assert_ne!(result.is_error, Some(true), "{}", text_of(&result));
    assert!(used_bytes(&result) <= 4096, "{}", used_bytes(&result));
    let mirror = result.structured_content.as_ref().unwrap();
    let view = &mirror["investigation"]["activity"];
    let printed = view["entries"].as_array().unwrap().len();
    assert!(printed > 0 && printed < 30, "{printed}");
    let through = view["printed_through"].as_u64().unwrap();
    assert_eq!(through, 1 + printed as u64);
    assert!(
        text_of(&result).contains(&format!(
            "next: investigation {{\"action\":\"read\",\"id\":\"{INVESTIGATION_ID}\",\"since\":{through}}}"
        )),
        "{}",
        text_of(&result)
    );
    assert_eq!(seen.lock().await.get(&id()), Some(through));
    assert_valid_mirror(&result);
}

/// Under a budget that holds the activity or the rows but not both, the
/// rows give way: every activity entry prints and no continuation is due.
#[tokio::test]
async fn rows_give_way_before_activity_entries_do() {
    let captured = captured(
        "service=api",
        &[("message", ColumnType::String)],
        vec![no_values(10)],
        10,
        10,
    );
    let rows = (0..10)
        .map(|i| vec![s(&format!("{i:02}{}", "x".repeat(600)))])
        .collect();
    let page = page(&captured, rows, 0);
    let mut captured = captured;
    captured.investigation_id = Some(id());
    let backend = Fake {
        capabilities: ok(capabilities(None)),
        capture: ok(CaptureResponse {
            result: captured,
            execution: None,
            activity: Some(Activity {
                entries: (2..=5)
                    .map(|seq| note(seq, human(), &format!("{seq}: {}", "w".repeat(300))))
                    .collect(),
                through_seq: 5,
                more: false,
            }),
            step_seq: Some(6),
        }),
        result_page: ok(page),
        ..Fake::default()
    };
    let seen = Seen::default();
    seen.lock().await.touch(&id(), 1);
    let result = run(&backend, &seen, execute(json!({"max_bytes": 8192}))).await;
    assert_ne!(result.is_error, Some(true), "{}", text_of(&result));
    assert!(used_bytes(&result) <= 8192, "{}", used_bytes(&result));
    let mirror = result.structured_content.as_ref().unwrap();
    let view = &mirror["investigation"]["activity"];
    assert_eq!(
        view["entries"].as_array().unwrap().len(),
        4,
        "{}",
        text_of(&result)
    );
    assert_eq!(view["entries_dropped_to_fit"], json!(0));
    assert!(view.get("continuation").is_none(), "{view}");
    let printed_rows = mirror["result"]["rows"].as_array().map_or(0, Vec::len);
    assert!(printed_rows < 10, "{printed_rows}");
    assert_eq!(
        mirror["result"]["presentation"]["rows_dropped_to_fit"],
        json!(10 - printed_rows),
        "{}",
        text_of(&result)
    );
    assert_eq!(seen.lock().await.get(&id()), Some(5));
}

#[tokio::test]
async fn validate_ignores_the_investigation_and_says_so() {
    let backend = Fake {
        validate: ok(serde_json::from_value(json!({"valid": true, "errors": []})).unwrap()),
        ..Fake::default()
    };
    let seen = Seen::default();
    let result = run(
        &backend,
        &seen,
        json!({"mode": "validate", "query": "service=api", "investigation": INVESTIGATION_ID,
               "purpose": "p"}),
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{}", text_of(&result));
    assert_eq!(backend.calls(), ["validate service=api"]);
    assert!(
        text_of(&result).contains(
            "activity: validate runs nothing, so it ignored investigation and purpose; \
             mode=execute records the search as a step"
        ),
        "{}",
        text_of(&result)
    );
    assert_eq!(seen.lock().await.get(&id()), None);
    assert_valid_mirror(&result);
}

/// R4: validate says it ignored whatever attachment input the call gave.
/// A purpose without an investigation is said too, and the mirror invents
/// no investigation for it.
#[tokio::test]
async fn validate_says_it_ignored_a_purpose_given_without_an_investigation() {
    let backend = Fake {
        validate: ok(serde_json::from_value(json!({"valid": true, "errors": []})).unwrap()),
        ..Fake::default()
    };
    for (extra, ignored) in [
        (json!({"purpose": "p"}), "purpose"),
        (json!({"investigation": INVESTIGATION_ID}), "investigation"),
        (
            json!({"investigation": INVESTIGATION_ID, "purpose": "p"}),
            "investigation and purpose",
        ),
    ] {
        let mut arguments = json!({"mode": "validate", "query": "service=api"});
        arguments
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let result = run(&backend, &Seen::default(), arguments.clone()).await;
        assert_ne!(result.is_error, Some(true), "{}", text_of(&result));
        let text = text_of(&result);
        let notice = format!(
            "runs nothing, so it ignored {ignored}; mode=execute records the search as a step"
        );
        if extra.get("investigation").is_some() {
            assert!(
                text.contains(&format!("activity: validate {notice}")),
                "{arguments}: {text}"
            );
        } else {
            // No investigation to print it beside: the outcome line says it.
            let first = text.lines().next().unwrap();
            assert_eq!(first, format!("The query is valid. Validate {notice}."));
        }
        let mirror = result.structured_content.as_ref().unwrap();
        assert_eq!(
            mirror.get("investigation").is_some(),
            extra.get("investigation").is_some(),
            "{arguments}: {mirror}"
        );
        assert_valid_mirror(&result);
    }
}

/// R1: an attached search trawld accepted names its investigation and the
/// result kept for it at every budget, whatever activity it carries.
#[tokio::test]
async fn an_attached_execute_names_its_investigation_and_kept_result_at_every_budget() {
    use trawl_api::investigations::{NOTE_MAX_BYTES, PURPOSE_MAX_BYTES};
    for fill in ['x', '\u{1}'] {
        let long = fill.to_string().repeat(NOTE_MAX_BYTES);
        for max_bytes in [4096, 16384, 32768] {
            let backend = attached_backend(
                Some(Activity {
                    entries: (2..=4).map(|seq| note(seq, human(), &long)).collect(),
                    through_seq: 4,
                    more: true,
                }),
                5,
            );
            let seen = Seen::default();
            seen.lock().await.touch(&id(), 1);
            let result = run(
                &backend,
                &seen,
                execute(
                    json!({"purpose": fill.to_string().repeat(PURPOSE_MAX_BYTES),
                               "max_bytes": max_bytes}),
                ),
            )
            .await;
            let label = format!("fill={fill:?} max_bytes={max_bytes}");
            assert_ne!(result.is_error, Some(true), "{label}: {}", text_of(&result));
            assert!(used_bytes(&result) <= max_bytes, "{label}");
            assert!(
                text_of(&result).contains(&format!(
                    "investigation: \"{INVESTIGATION_ID}\"; kept result \"{RESULT_ID}\""
                )),
                "{label}: {}",
                text_of(&result)
            );
            assert_valid_mirror(&result);
        }
    }
}

#[tokio::test]
async fn a_malformed_attachment_is_refused_before_any_request() {
    let backend = Fake::default();
    for (arguments, expected) in [
        (
            json!({"mode": "execute", "query": "x", "investigation": "NOT-HEX"}),
            "search mode=execute: investigation is 32 lowercase hex digits, and \"NOT-HEX\" is \
             not; investigation action=list lists them.",
        ),
        (
            execute(json!({"purpose": "p".repeat(1025)})),
            "search mode=execute: purpose is 1025 bytes; it may hold at most 1024.",
        ),
        (
            json!({"mode": "execute", "query": "x", "purpose": "p"}),
            "search mode=execute takes purpose only with investigation",
        ),
    ] {
        let result = run(&backend, &Seen::default(), arguments).await;
        assert_eq!(result.is_error, Some(true));
        assert!(
            text_of(&result).starts_with(expected),
            "{}",
            text_of(&result)
        );
    }
    assert!(backend.calls().is_empty(), "{:?}", backend.calls());
}

#[tokio::test]
async fn a_refused_attached_capture_names_the_way_back_and_moves_nothing() {
    for (status, code, message, expected) in [
        (
            409,
            ErrorCode::InvestigationClosed,
            "closed",
            vec![
                "search was refused: investigation_closed (HTTP 409).".to_owned(),
                format!("investigation action=status id={INVESTIGATION_ID} status=open"),
            ],
        ),
        (
            409,
            ErrorCode::EvidenceBudgetExceeded,
            "evidence budget: 512 MiB used of 512 MiB",
            vec![
                "evidence_budget_exceeded (HTTP 409)".to_owned(),
                "trawld says: evidence budget: 512 MiB used of 512 MiB".to_owned(),
                "deletes an investigation; waiting does not help".to_owned(),
            ],
        ),
        (
            403,
            ErrorCode::Forbidden,
            "insufficient permissions",
            vec![
                "search with investigation needs the query, investigation_read and \
                 investigation_write permissions"
                    .to_owned(),
            ],
        ),
        (
            404,
            ErrorCode::NotFound,
            "no such investigation",
            vec!["investigation action=list lists them".to_owned()],
        ),
    ] {
        let backend = Fake {
            capabilities: ok(capabilities(None)),
            capture: refuse(status, code, message),
            ..Fake::default()
        };
        let seen = Seen::default();
        seen.lock().await.touch(&id(), 4);
        let result = run(&backend, &seen, execute(json!({}))).await;
        assert_eq!(result.is_error, Some(true));
        let text = text_of(&result);
        for part in expected {
            assert!(text.contains(&part), "{part:?} not in {text}");
        }
        assert_eq!(backend.calls().len(), 2, "nothing retries");
        assert_eq!(seen.lock().await.get(&id()), Some(4));
    }
}

/// A search with no investigation reads and writes nothing of the
/// session's `seen`, so it takes no session lock: while its capture is held
/// open, an investigation read on the same session completes. The search
/// then completes once released.
#[tokio::test]
async fn an_unattached_search_takes_no_session_lock() {
    let (captured, page) = two_rows();
    let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
    let backend = Fake {
        capabilities: ok(capabilities(None)),
        capture: ok(CaptureResponse {
            result: captured,
            execution: None,
            activity: None,
            step_seq: None,
        }),
        result_page: ok(page),
        read_investigation: ok(InvestigationRead {
            investigation: investigation(3),
            entries: vec![],
            through_seq: 3,
            more: false,
        }),
        capture_gate: Some(std::sync::Arc::clone(&gate)),
        ..Fake::default()
    };
    let seen = Seen::default();
    let searching = run(
        &backend,
        &seen,
        json!({"mode": "execute", "query": "service=api"}),
    );
    let reading = async {
        while !backend
            .calls()
            .iter()
            .any(|call| call.starts_with("capture "))
        {
            tokio::task::yield_now().await;
        }
        let read = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            investigation::run(
                &backend,
                &seen,
                serde_json::from_value(json!({"action": "read", "id": INVESTIGATION_ID})).unwrap(),
            ),
        )
        .await;
        gate.add_permits(1);
        read
    };
    let (searched, read) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(searching, reading)
    })
    .await
    .expect("the search completes once released");
    let read = read.expect("the read completes while the search's capture is held");
    assert_ne!(read.is_error, Some(true), "{}", text_of(&read));
    assert_ne!(searched.is_error, Some(true), "{}", text_of(&searched));
    assert_eq!(
        backend.calls()[..2],
        ["capabilities".to_owned(), "capture service=api".to_owned()],
        "{:?}",
        backend.calls()
    );
}

// -- orient ---------------------------------------------------------------------------

fn summary(index: u64, status: InvestigationStatus) -> InvestigationSummary {
    InvestigationSummary {
        id: InvestigationId::parse(&format!("{index:032x}")).unwrap(),
        title: format!("investigation {index}"),
        status,
        seq: index,
        created_at: "2026-03-14T03:00:00.000000Z".to_owned(),
        updated_at: format!("2026-03-14T04:0{index}:00.000000Z"),
        created_by: agent(),
        updated_by: human(),
    }
}

fn orient_backend(permissions: &[&str], list: Option<support::Handler<InvestigationList>>) -> Fake {
    Fake {
        whoami: ok(whoami(permissions)),
        capabilities: ok(capabilities(None)),
        health: ok((200, health())),
        schema_services: ok(schema_services()),
        query: ok(query_response(vec![], vec![], 0)),
        list_investigations: list,
        ..Fake::default()
    }
}

#[tokio::test]
async fn orient_lists_the_five_most_recently_changed_open_investigations() {
    let mut investigations: Vec<_> = (1..=6)
        .map(|index| summary(index, InvestigationStatus::Open))
        .collect();
    investigations.push(summary(7, InvestigationStatus::Closed));
    let backend = orient_backend(
        &["query", "investigation_read"],
        ok(InvestigationList { investigations }),
    );
    let result = orient::run(&backend, "UTC", now()).await;
    assert_ne!(result.is_error, Some(true), "{}", text_of(&result));
    let text = text_of(&result);
    let listed: Vec<&str> = text
        .lines()
        .skip_while(|line| *line != "open investigations, most recently changed first:")
        .skip(1)
        .take(6)
        .collect();
    let id = |index: u64| format!("{index:032x}");
    assert_eq!(
        listed,
        [
            format!(
                "\"{}\" open, seq 6, changed 2026-03-14T04:06:00.000000Z by Zy9x8w7v \"jakub\": \
                 \"investigation 6\"",
                id(6)
            )
            .as_str(),
            &format!(
                "\"{}\" open, seq 5, changed 2026-03-14T04:05:00.000000Z by Zy9x8w7v \"jakub\": \
                 \"investigation 5\"",
                id(5)
            ),
            &format!(
                "\"{}\" open, seq 4, changed 2026-03-14T04:04:00.000000Z by Zy9x8w7v \"jakub\": \
                 \"investigation 4\"",
                id(4)
            ),
            &format!(
                "\"{}\" open, seq 3, changed 2026-03-14T04:03:00.000000Z by Zy9x8w7v \"jakub\": \
                 \"investigation 3\"",
                id(3)
            ),
            &format!(
                "\"{}\" open, seq 2, changed 2026-03-14T04:02:00.000000Z by Zy9x8w7v \"jakub\": \
                 \"investigation 2\"",
                id(2)
            ),
            "open investigations: 1 more open; investigation action=list lists them all",
        ],
        "{text}"
    );
    let mirror = result.structured_content.as_ref().unwrap();
    assert_eq!(mirror["open_investigations"].as_array().unwrap().len(), 5);
    let tool = tools().into_iter().find(|t| t.name == "orient").unwrap();
    let schema = Json::Object((*tool.output_schema.unwrap()).clone());
    assert!(
        jsonschema::draft202012::new(&schema)
            .unwrap()
            .is_valid(mirror)
    );
}

#[tokio::test]
async fn orient_lists_nothing_for_a_key_that_cannot_read_investigations() {
    let backend = orient_backend(&["query"], None);
    let result = orient::run(&backend, "UTC", now()).await;
    assert_ne!(result.is_error, Some(true));
    assert!(
        !text_of(&result).contains("open investigations"),
        "{}",
        text_of(&result)
    );
    assert!(
        !backend
            .methods()
            .contains(&"list_investigations".to_owned())
    );
}

#[tokio::test]
async fn orient_says_in_one_line_when_there_is_no_open_investigation_or_no_list() {
    for (list, expected) in [
        (
            ok(InvestigationList {
                investigations: vec![summary(1, InvestigationStatus::Closed)],
            }),
            "\nopen investigations: none\n",
        ),
        (
            refuse(503, ErrorCode::ServiceUnavailable, "starting"),
            "\nopen investigations: could not be listed: service_unavailable (HTTP 503)\n",
        ),
    ] {
        let backend = orient_backend(&["query", "investigation_read"], list);
        let result = orient::run(&backend, "UTC", now()).await;
        assert_ne!(result.is_error, Some(true), "{}", text_of(&result));
        assert!(text_of(&result).contains(expected), "{}", text_of(&result));
    }
}

// -- attached correlation reads -------------------------------------------------------

/// The id of the result a correlation read captures; the focus row of a
/// `context` read lives in [`RESULT_ID`].
const CORRELATED_ID: &str = "abcdef0123456789abcdef0123456789";

/// The three correlation tools, each with the arguments of one read and
/// the noun its step goes by.
const CORRELATION_TOOLS: [&str; 3] = ["context", "sightings", "compare"];

fn noun(tool: &str) -> &'static str {
    match tool {
        "context" => "context read",
        "sightings" => "sightings read",
        _ => "compare",
    }
}

fn correlation_arguments(tool: &str, extra: &Json) -> Json {
    let mut arguments = match tool {
        "context" => json!({"result_id": RESULT_ID, "row": 3}),
        "sightings" => json!({"literal": "10.0.4.17", "last": "1h"}),
        _ => json!({
            "filter": "service=auth",
            "foreground_earliest": "2026-03-14T03:00:00Z",
            "foreground_latest": "2026-03-14T04:00:00Z",
        }),
    };
    arguments
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    arguments
}

/// `extra` and the attachment to [`INVESTIGATION_ID`].
fn attached_to(extra: Json) -> Json {
    let Json::Object(extra) = extra else {
        panic!("extra arguments are an object");
    };
    let mut arguments = json!({"investigation": INVESTIGATION_ID});
    arguments.as_object_mut().unwrap().extend(extra);
    arguments
}

/// Call correlation tool `tool` as `tools/call` routes it.
async fn call(tool: &str, backend: &Fake, seen: &Seen, arguments: Json) -> CallToolResult {
    let Json::Object(arguments) = arguments else {
        panic!("arguments are an object");
    };
    trawl_mcp::tools::dispatch(tool, Some(arguments), backend, "UTC", seen)
        .await
        .expect("a known tool")
}

/// The recorded request of `method`, as JSON; the last one.
fn request_of(backend: &Fake, method: &str) -> Json {
    let call = backend
        .calls()
        .into_iter()
        .rev()
        .find(|call| call.starts_with(&format!("{method} ")))
        .unwrap_or_else(|| panic!("no {method} call in {:?}", backend.calls()));
    serde_json::from_str(&call[method.len() + 1..]).unwrap()
}

fn assert_valid_output(tool: &str, result: &CallToolResult) {
    let tool = tools().into_iter().find(|t| t.name == tool).unwrap();
    let schema = Json::Object((*tool.output_schema.unwrap()).clone());
    let validator = jsonschema::draft202012::new(&schema).unwrap();
    let mirror = result.structured_content.as_ref().expect("a mirror");
    let errors: Vec<String> = validator
        .iter_errors(mirror)
        .map(|e| e.to_string())
        .collect();
    assert!(errors.is_empty(), "{errors:?}");
}

fn context_facts() -> Correlation {
    Correlation::Context(ContextFacts {
        focus: FocusRow {
            result_id: RESULT_ID.to_owned(),
            row: 3,
            time: "2026-03-14T03:30:00.000000Z".to_owned(),
            env: Some("prod".to_owned()),
            host: Some("web-1".to_owned()),
        },
        scope_used: ScopeUsed {
            env: Some("prod".to_owned()),
            host: Some("web-1".to_owned()),
            seconds: 30,
        },
        groups: ContextGroups {
            earlier: RowRange { start: 0, end: 1 },
            same_time: RowRange { start: 1, end: 3 },
            later: RowRange { start: 3, end: 4 },
        },
        statement: trawl_api::results::CONTEXT_STATEMENT.to_owned(),
    })
}

/// Sightings over `groups` (env, service) groups, with two follow-ups.
fn sightings_facts(groups: u64) -> Correlation {
    Correlation::Sightings(SightingsFacts {
        literal: "10.0.4.17".to_owned(),
        matches_total: 20_000,
        captured: 5000,
        attribution_fields: vec![],
        groups: (0..groups)
            .map(|i| SightingGroup {
                env: Some("prod".to_owned()),
                service: Some(format!("service-{i:03}")),
                count: 1000 - i,
                first_seen: "2026-03-14T03:00:00.000000Z".to_owned(),
                last_seen: "2026-03-14T03:59:59.999999Z".to_owned(),
            })
            .collect(),
        fields: vec![FieldSightings {
            field: "client_ip".to_owned(),
            rows: 3999,
        }],
        fields_omitted: 0,
        raw_only_rows: 1,
        denominators: SightingsDenominators {
            field_counts_over: 5000,
            service_counts_over: 20_000,
        },
        follow_ups: vec![
            "\"10.0.4.17\" service=service-000".to_owned(),
            "\"10.0.4.17\"\n| timechart span=1h count() by service".to_owned(),
        ],
    })
}

fn compare_facts() -> Correlation {
    Correlation::Compare(CompareFacts {
        foreground: CompareWindow {
            start: "2026-03-14T03:00:00.000000Z".to_owned(),
            end: "2026-03-14T04:00:00.000000Z".to_owned(),
            duration_ms: 3_600_000,
        },
        baseline: CompareWindow {
            start: "2026-03-14T02:00:00.000000Z".to_owned(),
            end: "2026-03-14T03:00:00.000000Z".to_owned(),
            duration_ms: 3_600_000,
        },
        group_by: vec!["service".to_owned()],
        min_count: 1,
        rate_unit: RateUnit::PerMinute,
        rows_ranked: 4,
        warnings: vec![CompareWarning {
            code: CompareWarningCode::RetentionReach,
            message: "the baseline starts before the oldest stored date".to_owned(),
            field: None,
        }],
    })
}

/// The answer of correlation tool `tool`: a capture of `query` with its
/// facts and four `message` rows, kept for the investigation; the page
/// holds the first two.
fn correlated(tool: &str, query: &str) -> (CapturedResult, ResultPage) {
    let (kind, facts) = match tool {
        "context" => (CaptureKind::Context, context_facts()),
        "sightings" => (CaptureKind::Sightings, sightings_facts(3)),
        _ => (CaptureKind::Compare, compare_facts()),
    };
    let mut result = captured(
        query,
        &[("message", ColumnType::String)],
        vec![no_values(4)],
        4,
        4,
    );
    CORRELATED_ID.clone_into(&mut result.id);
    result.kind = kind;
    result.correlation = Some(facts);
    result.investigation_id = Some(id());
    let page = page(&result, vec![vec![s("one")], vec![s("two")]], 0);
    (result, page)
}

/// A backend whose `tool` read answers `result` with `activity` and its
/// step at `step_seq`, and whose page is `page`.
fn correlation_backend(
    tool: &str,
    result: CapturedResult,
    page: ResultPage,
    activity: Option<Activity>,
    step_seq: Option<u64>,
) -> Fake {
    let answer = ok(CorrelationResponse {
        result,
        execution: None,
        activity,
        step_seq,
    });
    let mut backend = Fake {
        capabilities: ok(capabilities(None)),
        result_page: ok(page),
        ..Fake::default()
    };
    match tool {
        "context" => backend.context = answer,
        "sightings" => backend.sightings = answer,
        _ => backend.compare = answer,
    }
    backend
}

/// [`correlation_backend`] over [`correlated`]'s answer.
fn attached_correlation(tool: &str, activity: Option<Activity>, step_seq: u64) -> Fake {
    let (result, page) = correlated(tool, "\"10.0.4.17\"");
    correlation_backend(tool, result, page, activity, Some(step_seq))
}

/// The acknowledgement an attached `tool` read opens its errors with.
fn correlation_acknowledgement(tool: &str) -> String {
    format!(
        "Investigation \"{INVESTIGATION_ID}\" recorded this {} as a step and keeps result \
         \"{CORRELATED_ID}\" for it.\n",
        noun(tool)
    )
}

fn line_index(text: &str, prefix: &str) -> usize {
    text.lines()
        .position(|line| line.starts_with(prefix))
        .unwrap_or_else(|| panic!("no line starts with {prefix:?}:\n{text}"))
}

#[tokio::test]
async fn an_attached_context_sends_seen_seq_prints_the_activity_and_advances_seen() {
    let backend = attached_correlation(
        "context",
        Some(Activity {
            entries: vec![note(2, human(), "theirs")],
            through_seq: 2,
            more: false,
        }),
        3,
    );
    let seen = Seen::default();
    seen.lock().await.touch(&id(), 1);
    let result = call(
        "context",
        &backend,
        &seen,
        correlation_arguments(
            "context",
            &attached_to(json!({"purpose": "what web-1 did around it"})),
        ),
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{}", text_of(&result));
    assert_eq!(
        backend.methods(),
        ["capabilities", "context", "result_page"]
    );
    assert_eq!(
        request_of(&backend, "context")["investigation"],
        json!({"id": INVESTIGATION_ID, "seen_seq": 1, "purpose": "what web-1 did around it"})
    );
    let text = text_of(&result);
    assert!(
        text.contains(&format!(
            "\ninvestigation: \"{INVESTIGATION_ID}\"; kept result \"{CORRELATED_ID}\"\n\
             activity by other keys since #1, in change order: 1 entry\n\
             #2 note by Zy9x8w7v \"jakub\" at 2026-10-24T09:05:00.000000Z: \"theirs\"\n"
        )),
        "{text}"
    );
    // After the result head and the block, before the proposed calls.
    let attached = line_index(text, "investigation: ");
    assert!(line_index(text, "scope used: ") < attached, "{text}");
    assert!(attached < line_index(text, "next: read "), "{text}");
    let mirror = result.structured_content.as_ref().unwrap();
    assert_eq!(mirror["investigation"]["id"], json!(INVESTIGATION_ID));
    assert_eq!(mirror["investigation"]["result_id"], json!(CORRELATED_ID));
    assert_eq!(
        mirror["investigation"]["activity"]["printed_through"],
        json!(2)
    );
    assert_eq!(seen.lock().await.get(&id()), Some(2));
    assert_valid_output("context", &result);
}

/// The first attached compare of a session sends no `seen_seq` and follows
/// its step only once its reply fitted: a page that fails after the read,
/// or a reply that does not fit, leaves the session untouched.
#[tokio::test]
async fn the_first_attached_compare_follows_its_step_only_after_its_reply_fits() {
    let seen = Seen::default();
    let arguments = correlation_arguments("compare", &attached_to(json!({"max_bytes": 4096})));

    let mut failed_page = attached_correlation("compare", None, 7);
    failed_page.result_page = refuse(503, ErrorCode::ServiceUnavailable, "starting");
    let result = call("compare", &failed_page, &seen, arguments.clone()).await;
    assert_eq!(result.is_error, Some(true), "{}", text_of(&result));
    assert_eq!(seen.lock().await.get(&id()), None, "a failed page");

    let (result, page) = correlated("compare", &format!("service=auth{}", " ".repeat(4096)));
    let unfitted = correlation_backend("compare", result, page, None, Some(7));
    let result = call("compare", &unfitted, &seen, arguments.clone()).await;
    assert_eq!(result.is_error, Some(true), "{}", text_of(&result));
    assert_eq!(seen.lock().await.get(&id()), None, "an unfitted reply");

    let fitted = attached_correlation("compare", None, 7);
    let result = call("compare", &fitted, &seen, arguments.clone()).await;
    assert_ne!(result.is_error, Some(true), "{}", text_of(&result));
    assert_eq!(
        request_of(&fitted, "compare")["investigation"],
        json!({"id": INVESTIGATION_ID}),
        "the failed calls left nothing to send"
    );
    assert!(
        text_of(&result).contains(&format!(
            "activity: this session follows this investigation from this step on; what \
             other keys wrote before it is on the board, which investigation action=read \
             id={INVESTIGATION_ID} since=0 prints"
        )),
        "{}",
        text_of(&result)
    );
    assert_eq!(seen.lock().await.get(&id()), Some(7), "a fitted reply");
    assert_valid_output("compare", &result);

    let result = call("compare", &fitted, &seen, arguments).await;
    assert_ne!(result.is_error, Some(true), "{}", text_of(&result));
    assert_eq!(
        request_of(&fitted, "compare")["investigation"],
        json!({"id": INVESTIGATION_ID, "seen_seq": 7})
    );
}

/// An attached correlation answer the server cannot stand behind (a result
/// id trawld does not mint, or a result not kept for the investigation the
/// call named) certifies no result, reads no page, prints no id trawld
/// sent, and leaves the session as it was. Unattached, a malformed id is
/// an error that reads no page either.
#[tokio::test]
async fn an_attached_correlation_answer_for_an_unreadable_or_foreign_result_certifies_nothing() {
    for tool in CORRELATION_TOOLS {
        let noun = noun(tool);
        for (label, result_id, kept_for, reason) in [
            (
                "malformed id",
                MALFORMED_RESULT_ID,
                Some(INVESTIGATION_ID),
                "names a result id that is not 32 lowercase hex digits",
            ),
            (
                "another investigation",
                CORRELATED_ID,
                Some(OTHER_INVESTIGATION_ID),
                "names another investigation as the one the result is kept for",
            ),
            (
                "no investigation",
                CORRELATED_ID,
                None,
                "names no investigation the result is kept for",
            ),
        ] {
            let label = format!("{tool}: {label}");
            let (mut result, page) = correlated(tool, "\"10.0.4.17\"");
            result_id.clone_into(&mut result.id);
            result.investigation_id = kept_for.map(|id| InvestigationId::parse(id).unwrap());
            let backend = correlation_backend(tool, result, page, None, Some(7));
            let seen = Seen::default();
            seen.lock().await.touch(&id(), 4);
            let answer = call(
                tool,
                &backend,
                &seen,
                correlation_arguments(tool, &attached_to(json!({}))),
            )
            .await;
            assert_eq!(answer.is_error, Some(true), "{label}: {}", text_of(&answer));
            assert!(answer.structured_content.is_none(), "{label}");
            let text = text_of(&answer);
            assert_eq!(
                text,
                format!(
                    "{tool} with investigation cannot tell what trawld kept: its {tool} answer \
                     {reason}. Investigation \"{INVESTIGATION_ID}\" may hold this {noun} as a \
                     step; investigation action=read id={INVESTIGATION_ID} since=4 prints the \
                     entries changed after #4, and a step for this {noun} names the result it \
                     keeps."
                ),
                "{label}"
            );
            for absent in ["NOT-A-RESULT", OTHER_INVESTIGATION_ID, "keeps result"] {
                assert!(!text.contains(absent), "{label}: {absent:?} in {text}");
            }
            assert!(
                !backend.methods().contains(&"result_page".to_owned()),
                "{label}: {:?}",
                backend.calls()
            );
            assert_eq!(seen.lock().await.get(&id()), Some(4), "{label}");
        }

        let (mut result, page) = correlated(tool, "\"10.0.4.17\"");
        MALFORMED_RESULT_ID.clone_into(&mut result.id);
        result.investigation_id = None;
        let backend = correlation_backend(tool, result, page, None, None);
        let answer = call(
            tool,
            &backend,
            &Seen::default(),
            correlation_arguments(tool, &json!({})),
        )
        .await;
        assert_eq!(
            text_of(&answer),
            format!(
                "{tool} failed: trawld's {tool} answer names a result id that is not 32 \
                 lowercase hex digits, so no read can page the result. Run the {noun} again."
            )
        );
        assert_eq!(backend.methods(), ["capabilities", tool]);
    }
}

#[tokio::test]
async fn a_refused_attached_sightings_names_the_way_back_and_moves_nothing() {
    for (status, code, message, expected) in [
        (
            409,
            ErrorCode::InvestigationClosed,
            "closed",
            vec![
                "sightings was refused: investigation_closed (HTTP 409).".to_owned(),
                format!("investigation action=status id={INVESTIGATION_ID} status=open"),
            ],
        ),
        (
            409,
            ErrorCode::EvidenceBudgetExceeded,
            "evidence budget: 512 MiB used of 512 MiB",
            vec![
                "evidence_budget_exceeded (HTTP 409)".to_owned(),
                "deletes an investigation; waiting does not help".to_owned(),
            ],
        ),
        (
            403,
            ErrorCode::Forbidden,
            "insufficient permissions",
            vec![
                "sightings with investigation needs the query, investigation_read and \
                 investigation_write permissions"
                    .to_owned(),
            ],
        ),
        (
            404,
            ErrorCode::NotFound,
            "no such investigation",
            vec!["investigation action=list lists them".to_owned()],
        ),
    ] {
        let backend = Fake {
            capabilities: ok(capabilities(None)),
            sightings: refuse(status, code, message),
            ..Fake::default()
        };
        let seen = Seen::default();
        seen.lock().await.touch(&id(), 4);
        let result = call(
            "sightings",
            &backend,
            &seen,
            correlation_arguments("sightings", &attached_to(json!({}))),
        )
        .await;
        assert_eq!(result.is_error, Some(true));
        let text = text_of(&result);
        for part in expected {
            assert!(text.contains(&part), "{part:?} not in {text}");
        }
        assert_eq!(
            backend.methods(),
            ["capabilities", "sightings"],
            "nothing retries"
        );
        assert_eq!(seen.lock().await.get(&id()), Some(4));
    }
}

#[tokio::test]
async fn a_correlation_tool_refuses_a_malformed_attachment_or_a_lone_purpose_before_any_request() {
    let backend = Fake::default();
    for tool in CORRELATION_TOOLS {
        for (extra, expected) in [
            (
                json!({"investigation": "NOT-HEX"}),
                format!(
                    "{tool}: investigation is 32 lowercase hex digits, and \"NOT-HEX\" is not; \
                     investigation action=list lists them."
                ),
            ),
            (
                attached_to(json!({"purpose": "p".repeat(1025)})),
                format!("{tool}: purpose is 1025 bytes; it may hold at most 1024."),
            ),
            (
                attached_to(json!({"purpose": "before\u{0}after"})),
                format!("{tool}: purpose holds a NUL character"),
            ),
            (
                json!({"purpose": "p"}),
                format!("{tool} takes purpose only with investigation"),
            ),
        ] {
            let result = call(
                tool,
                &backend,
                &Seen::default(),
                correlation_arguments(tool, &extra),
            )
            .await;
            assert_eq!(result.is_error, Some(true), "{tool} {extra}");
            assert!(
                text_of(&result).starts_with(&expected),
                "{tool} {extra}: {}",
                text_of(&result)
            );
        }
    }
    assert!(backend.calls().is_empty(), "{:?}", backend.calls());
}

/// A correlation read trawld accepted attached is a step whatever happens
/// after it: a page that fails, a cell that does not encode, an answer
/// without its facts, or a reply that does not fit is an error that opens
/// by naming the investigation and the result kept for it, and moves
/// nothing of the session.
#[tokio::test]
async fn an_attached_correlation_read_that_cannot_print_keeps_its_acknowledgement() {
    let mut failed_page = attached_correlation("context", None, 7);
    failed_page.result_page = refuse(503, ErrorCode::ServiceUnavailable, "starting");

    let (result, _) = correlated("context", "\"10.0.4.17\"");
    let mut unencodable_page = page(&result, vec![vec![Value::Integer(1)]], 0);
    unencodable_page.page.columns[0] =
        trawl_api::value::Column::new("message".to_owned(), ColumnType::Boolean);
    let unencodable = correlation_backend("context", result, unencodable_page, None, Some(7));

    let (mut result, page) = correlated("context", "\"10.0.4.17\"");
    result.correlation = None;
    let no_facts = correlation_backend("context", result, page, None, Some(7));

    let (result, page) = correlated("context", &format!("\"x\"{}", " ".repeat(4096)));
    let unfitted = correlation_backend("context", result, page, None, Some(7));

    for (label, backend, says) in [
        ("failed page", failed_page, "service_unavailable (HTTP 503)"),
        ("unencodable", unencodable, "does not fit its column's type"),
        ("no facts", no_facts, "sent no correlation facts"),
        ("unfitted", unfitted, "does not fit max_bytes=4096"),
    ] {
        let seen = Seen::default();
        let result = call(
            "context",
            &backend,
            &seen,
            correlation_arguments("context", &attached_to(json!({"max_bytes": 4096}))),
        )
        .await;
        assert_eq!(result.is_error, Some(true), "{label}: {}", text_of(&result));
        let text = text_of(&result);
        assert!(
            text.starts_with(&correlation_acknowledgement("context")),
            "{label}: {text}"
        );
        assert!(text.contains(says), "{label}: {text}");
        assert_eq!(seen.lock().await.get(&id()), None, "{label}");
    }
}

/// Under a budget that cannot hold a sightings block's lists and the
/// activity both, the activity entries go first, from the end, with the
/// board read that continues from the last one printed; the lists are cut
/// only once no entry is left, and the block's meaning stays.
#[tokio::test]
async fn an_attached_sightings_cuts_activity_entries_before_its_lists() {
    let activity = |count: u64| Activity {
        entries: (2..2 + count)
            .map(|seq| note(seq, human(), &format!("{seq}: {}", "w".repeat(600))))
            .collect(),
        through_seq: 1 + count,
        more: false,
    };
    let sightings = |groups: u64, count: u64| {
        let (mut result, page) = correlated("sightings", "\"10.0.4.17\"");
        result.correlation = Some(sightings_facts(groups));
        correlation_backend("sightings", result, page, Some(activity(count)), Some(40))
    };
    let denominators =
        "field counts are over the 5000 captured matches; service counts are over all 20000";

    // Fifty groups fit the default budget whole; twenty 600-byte entries
    // beside them do not: entries go, the lists stay whole.
    let backend = sightings(50, 20);
    let seen = Seen::default();
    seen.lock().await.touch(&id(), 1);
    let result = call(
        "sightings",
        &backend,
        &seen,
        correlation_arguments("sightings", &attached_to(json!({}))),
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{}", text_of(&result));
    assert!(used_bytes(&result) <= 16_384, "{}", used_bytes(&result));
    let mirror = result.structured_content.as_ref().unwrap();
    assert_eq!(
        mirror["correlation"]["sightings"]["entries_dropped_to_fit"],
        json!(0),
        "{}",
        text_of(&result)
    );
    let view = &mirror["investigation"]["activity"];
    let printed = view["entries"].as_array().unwrap().len();
    assert!(printed < 20, "{printed}");
    assert_eq!(view["entries_dropped_to_fit"], json!(20 - printed));
    let through = 1 + printed as u64;
    if printed > 0 {
        assert_eq!(view["printed_through"], json!(through));
    }
    assert!(
        text_of(&result).contains(&format!(
            "next: investigation {{\"action\":\"read\",\"id\":\"{INVESTIGATION_ID}\",\"since\":{through}}}"
        )),
        "{}",
        text_of(&result)
    );
    assert_eq!(seen.lock().await.get(&id()), Some(through));
    assert_valid_output("sightings", &result);

    // Four hundred groups do not fit 4096 bytes: every entry goes before
    // the lists are cut, and the board read continues from the session's
    // seq.
    let backend = sightings(400, 6);
    let seen = Seen::default();
    seen.lock().await.touch(&id(), 1);
    let result = call(
        "sightings",
        &backend,
        &seen,
        correlation_arguments("sightings", &attached_to(json!({"max_bytes": 4096}))),
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{}", text_of(&result));
    assert!(used_bytes(&result) <= 4096, "{}", used_bytes(&result));
    let mirror = result.structured_content.as_ref().unwrap();
    let block = &mirror["correlation"]["sightings"];
    assert!(
        block["entries_dropped_to_fit"].as_u64().unwrap() > 0,
        "{block}"
    );
    let view = &mirror["investigation"]["activity"];
    assert_eq!(view["entries"], json!([]), "{}", text_of(&result));
    assert_eq!(view["entries_dropped_to_fit"], json!(6));
    let text = text_of(&result);
    assert!(text.contains(denominators), "{text}");
    assert!(
        text.contains(&format!(
            "investigation: \"{INVESTIGATION_ID}\"; kept result \"{CORRELATED_ID}\""
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "next: investigation {{\"action\":\"read\",\"id\":\"{INVESTIGATION_ID}\",\"since\":1}}"
        )),
        "{text}"
    );
    assert_eq!(seen.lock().await.get(&id()), Some(1));
    assert_valid_output("sightings", &result);
}

/// A correlation read with no investigation takes no session lock: while
/// its read is held open, an investigation read on the same session
/// completes. An attached one takes the lock before it sends: while the
/// session is held, it sends nothing.
#[tokio::test]
async fn an_unattached_correlation_read_takes_no_session_lock() {
    let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
    let mut backend = attached_correlation("context", None, 7);
    backend.read_investigation = ok(InvestigationRead {
        investigation: investigation(3),
        entries: vec![],
        through_seq: 3,
        more: false,
    });
    backend.correlation_gate = Some(std::sync::Arc::clone(&gate));
    let seen = Seen::default();
    let reading_context = call(
        "context",
        &backend,
        &seen,
        correlation_arguments("context", &json!({})),
    );
    let reading_board = async {
        while !backend.methods().contains(&"context".to_owned()) {
            tokio::task::yield_now().await;
        }
        let read = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            investigation::run(
                &backend,
                &seen,
                serde_json::from_value(json!({"action": "read", "id": INVESTIGATION_ID})).unwrap(),
            ),
        )
        .await;
        gate.add_permits(1);
        read
    };
    let (context, read) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(reading_context, reading_board)
    })
    .await
    .expect("the context read completes once released");
    let read = read.expect("the board read completes while the context read is held");
    assert_ne!(read.is_error, Some(true), "{}", text_of(&read));
    assert_ne!(context.is_error, Some(true), "{}", text_of(&context));

    let backend = attached_correlation("context", None, 7);
    let seen = Seen::default();
    let held = seen.lock().await;
    let waiting = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        call(
            "context",
            &backend,
            &seen,
            correlation_arguments("context", &attached_to(json!({}))),
        ),
    )
    .await;
    assert!(waiting.is_err(), "an attached read waits for the session");
    assert!(
        !backend.methods().contains(&"context".to_owned()),
        "{:?}",
        backend.calls()
    );
    drop(held);
}

/// The follow-up searches an attached sightings read proposes stay on its
/// investigation and over its frozen bounds; the purpose was this read's,
/// so none carries it.
#[tokio::test]
async fn an_attached_sightings_follow_up_carries_the_investigation_not_the_purpose() {
    let backend = attached_correlation("sightings", None, 7);
    let result = call(
        "sightings",
        &backend,
        &Seen::default(),
        correlation_arguments(
            "sightings",
            &attached_to(json!({"purpose": "who else saw it"})),
        ),
    )
    .await;
    assert_ne!(result.is_error, Some(true), "{}", text_of(&result));
    let mirror = result.structured_content.as_ref().unwrap();
    let searches: Vec<&Json> = mirror["next_actions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|action| action["tool"] == json!("search"))
        .collect();
    assert!(!searches.is_empty(), "{}", text_of(&result));
    for search in searches {
        let arguments = search["arguments"].as_object().unwrap();
        assert_eq!(
            arguments["investigation"],
            json!(INVESTIGATION_ID),
            "{search}"
        );
        assert!(!arguments.contains_key("purpose"), "{search}");
        let query = arguments["query"].as_str().unwrap();
        let parsed = trawl_core::parser::parse(query).expect("a follow-up parses");
        assert_eq!(
            parsed.search.earliest.as_ref().map(|b| b.node.as_str()),
            Some("2026-03-14T03:00:00.250000Z")
        );
        assert_eq!(
            parsed.search.latest.as_ref().map(|b| b.node.as_str()),
            Some("2026-03-14T04:00:00.750000Z")
        );
    }
    let text = text_of(&result);
    assert!(
        text.lines()
            .any(|line| line.starts_with("next: search ") && line.contains(INVESTIGATION_ID)),
        "{text}"
    );
    assert_valid_output("sightings", &result);
}
