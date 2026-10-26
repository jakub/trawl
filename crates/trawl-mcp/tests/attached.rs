// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `search` attached to an investigation and `orient`'s open
//! investigations, over a recording backend (ADR-0059, #310).
//!
//! An attached execute sends the session's `seen_seq` with the capture and
//! prints the activity trawld answers with under the same exactly-once
//! rules as an append; `validate` ignores the attachment and says so; the
//! id's shape and the purpose's length are refused before any request;
//! trawld's refusals name the way back. `orient` lists open investigations
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
use trawl_api::value::ColumnType;
use trawl_client::{CaptureResponse, CapturedResult, ErrorCode, ResultPage};
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
