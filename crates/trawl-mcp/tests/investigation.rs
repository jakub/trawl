// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `investigation` over a recording backend (ADR-0059, #310): the request
//! each action sends, the session's `seen_seq` bookkeeping, activity
//! printed exactly once under the response budget, the board renderer it
//! shares with `read`, and the refusals.
//!
//! The real two-principal run through `trawl mcp` against a trawld lives
//! in `crates/trawl-cli/tests/mcp_investigation.rs`: this crate never
//! depends on trawl-server.

mod support;

use rmcp::model::CallToolResult;
use serde_json::{Value as Json, json};
use trawl_api::investigations::{
    Activity, AppendResponse, EntryPayload, EvidenceRef, FindingStatus, InvestigationId,
    InvestigationList, InvestigationRead, InvestigationStatus, InvestigationSummary, LogEntry,
    SequenceConflict, StepBounds, StepOutcome, StepOutcomeKind,
};
use trawl_client::{ClientError, ErrorCode, ErrorEnvelope};
use trawl_mcp::render::activity::ENTRY_ALLOWANCE_BYTES;
use trawl_mcp::session::Seen;
use trawl_mcp::tools::schema::{INVESTIGATION, InvestigationInput, ReadInput, tools};
use trawl_mcp::tools::{investigation, read};

use support::*;

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

fn input(arguments: Json) -> InvestigationInput {
    serde_json::from_value(arguments).expect("the arguments decode")
}

async fn call(backend: &Fake, seen: &Seen, arguments: Json) -> CallToolResult {
    investigation::run(backend, seen, input(arguments)).await
}

fn id() -> InvestigationId {
    InvestigationId::parse(INVESTIGATION_ID).unwrap()
}

/// The JSON body a recorded call sent, after its method and id.
fn sent(call: &str) -> Json {
    let body = &call[call.find('{').expect("a body")..];
    serde_json::from_str(body).expect("a JSON body")
}

fn appended(entry: LogEntry, activity: Option<Activity>) -> AppendResponse {
    AppendResponse { entry, activity }
}

/// trawld's answers to the appends, in order.
fn answers(responses: Vec<AppendResponse>) -> Option<Handler<AppendResponse>> {
    each(responses.into_iter().map(Ok).collect())
}

fn activity(entries: Vec<LogEntry>, more: bool) -> Activity {
    let through_seq = entries.last().map_or(0, |entry| entry.updated_seq);
    Activity {
        entries,
        through_seq,
        more,
    }
}

/// The mirror's activity block, or the board, at `part`.
fn block<'a>(result: &'a CallToolResult, part: &str) -> &'a Json {
    &result.structured_content.as_ref().expect("a mirror")[part]
}

fn assert_ok(result: &CallToolResult) {
    assert_ne!(result.is_error, Some(true), "{}", text_of(result));
}

// -- the tool ---------------------------------------------------------------------

#[test]
fn the_tool_is_listed_fifth_with_honest_annotations() {
    let tools = tools();
    let tool = &tools[4];
    assert_eq!(tool.name, INVESTIGATION);
    let annotations = tool.annotations.as_ref().unwrap();
    assert_eq!(annotations.read_only_hint, Some(false));
    assert_eq!(annotations.idempotent_hint, Some(false));
    assert_eq!(annotations.destructive_hint, Some(false));
    assert_eq!(annotations.open_world_hint, Some(true));
    let description = tool.description.as_deref().unwrap();
    assert!(
        description.contains("server_manage") && description.contains("deleting"),
        "{description}"
    );
    // `read` keeps its read-only hint while it reads investigations too.
    assert_eq!(
        tools[2].annotations.as_ref().unwrap().read_only_hint,
        Some(true)
    );
}

#[tokio::test]
async fn create_mints_an_id_prints_it_first_and_starts_the_session_there() {
    let backend = Fake {
        create_investigation: ok((true, investigation(0))),
        ..Fake::default()
    };
    let seen = Seen::default();
    let result = call(
        &backend,
        &seen,
        json!({"action": "create", "title": "502s", "question": "why?"}),
    )
    .await;
    assert_ok(&result);
    let calls = backend.calls();
    let minted = calls[0].split(' ').nth(1).unwrap().to_owned();
    assert!(InvestigationId::parse(&minted).is_ok(), "{minted}");
    assert_eq!(
        sent(&calls[0]),
        json!({"title": "502s", "question": "why?"})
    );
    let first = text_of(&result).lines().next().unwrap();
    assert!(
        first.starts_with(&format!("Created investigation \"{minted}\"")),
        "{first}"
    );
    let minted = InvestigationId::parse(&minted).unwrap();
    assert_eq!(seen.lock().await.get(&minted), Some(0));

    // Another create mints another id.
    let backend_again = Fake {
        create_investigation: ok((true, investigation(0))),
        ..Fake::default()
    };
    let _ = call(
        &backend_again,
        &seen,
        json!({"action": "create", "title": "502s", "question": "why?"}),
    )
    .await;
    assert_ne!(
        backend_again.calls()[0].split(' ').nth(1).unwrap(),
        minted.as_str()
    );
}

#[tokio::test]
async fn create_with_an_id_replays_and_keeps_it() {
    let backend = Fake {
        create_investigation: ok((false, investigation(7))),
        ..Fake::default()
    };
    let seen = Seen::default();
    let result = call(
        &backend,
        &seen,
        json!({"action": "create", "id": INVESTIGATION_ID, "title": "502s", "question": "why?",
               "scope": "checkout"}),
    )
    .await;
    assert_ok(&result);
    assert!(backend.calls()[0].starts_with(&format!("create_investigation {INVESTIGATION_ID} ")));
    assert_eq!(sent(&backend.calls()[0])["scope"], json!("checkout"));
    assert!(
        text_of(&result).starts_with(&format!(
            "Investigation \"{INVESTIGATION_ID}\" already exists"
        )),
        "{}",
        text_of(&result)
    );
    assert_eq!(seen.lock().await.get(&id()), Some(7));
}

#[tokio::test]
async fn a_failed_create_prints_the_id_it_sent_to_recover_it() {
    let backend = Fake {
        create_investigation: Some(Box::new(|| {
            Err(ClientError::Network(trawl_client::NetworkError::new(
                trawl_client::NetworkKind::Timeout,
                "timed out",
            )))
        })),
        ..Fake::default()
    };
    let result = call(
        &backend,
        &Seen::default(),
        json!({"action": "create", "title": "t", "question": "q"}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    let minted = backend.calls()[0].split(' ').nth(1).unwrap().to_owned();
    assert!(
        text_of(&result).contains(&format!("send the same create with id={minted}")),
        "{}",
        text_of(&result)
    );
}

#[tokio::test]
async fn a_field_the_action_does_not_take_is_refused_by_name_before_any_request() {
    let backend = Fake::default();
    let seen = Seen::default();
    for (arguments, expected) in [
        (
            json!({"action": "note", "id": INVESTIGATION_ID, "text": "n", "statement": "s"}),
            "investigation action=note does not take statement; it reads id, text and max_bytes.",
        ),
        (
            json!({"action": "list", "id": INVESTIGATION_ID}),
            "investigation action=list does not take id; it reads only max_bytes.",
        ),
        (
            json!({"action": "status", "id": INVESTIGATION_ID, "status": "closed"}),
            "investigation action=status needs expected_seq;",
        ),
        (
            json!({"action": "status", "id": INVESTIGATION_ID, "status": "supported",
                   "expected_seq": 3}),
            "investigation action=status: status is open or closed.",
        ),
        (
            json!({"action": "note", "id": "NOT-HEX", "text": "n"}),
            "investigation action=note: id is 32 lowercase hex digits",
        ),
        (
            json!({"action": "note", "id": INVESTIGATION_ID, "text": "x".repeat(4097)}),
            "investigation action=note: text is 4097 bytes; it may hold at most 4096.",
        ),
        (
            json!({"action": "keep", "id": INVESTIGATION_ID, "result_id": RESULT_ID,
                   "rows": vec![0; 201], "reason": "r"}),
            "investigation action=keep: rows lists 201; it may list at most 200.",
        ),
    ] {
        let result = call(&backend, &seen, arguments).await;
        assert_eq!(result.is_error, Some(true));
        assert!(
            text_of(&result).starts_with(expected),
            "{}",
            text_of(&result)
        );
    }
    assert!(backend.calls().is_empty(), "{:?}", backend.calls());
}

// -- requests and seen_seq ----------------------------------------------------------

#[tokio::test]
async fn every_write_sends_its_request_and_the_sessions_seen_seq() {
    let backend = Fake {
        append_entry: answers(vec![
            appended(note(1, agent(), "first"), None),
            appended(
                LogEntry {
                    payload: EntryPayload::Keep {
                        result_id: RESULT_ID.to_owned(),
                        rows: vec![0, 2],
                        reason: "the reset".to_owned(),
                    },
                    ..note(2, agent(), "")
                },
                Some(activity(vec![], false)),
            ),
            appended(
                LogEntry {
                    payload: EntryPayload::Finding {
                        statement: "pool exhausted".to_owned(),
                        status: FindingStatus::Supported,
                        references: vec![EvidenceRef {
                            result_id: RESULT_ID.to_owned(),
                            row: 2,
                        }],
                        counterevidence: vec![],
                        remaining_question: Some("why 09:00?".to_owned()),
                    },
                    ..note(3, agent(), "")
                },
                Some(activity(vec![], false)),
            ),
        ]),
        set_finding_status: answers(vec![appended(
            LogEntry {
                payload: EntryPayload::FindingStatus {
                    finding_seq: 3,
                    status: FindingStatus::Refuted,
                },
                ..note(4, agent(), "")
            },
            None,
        )]),
        patch_investigation: ok(Investigation {
            status: InvestigationStatus::Closed,
            ..investigation(5)
        }),
        ..Fake::default()
    };
    let seen = Seen::default();
    for arguments in [
        json!({"action": "note", "id": INVESTIGATION_ID, "text": "first"}),
        json!({"action": "keep", "id": INVESTIGATION_ID, "result_id": RESULT_ID, "rows": [0, 2],
               "reason": "the reset"}),
        json!({"action": "finding", "id": INVESTIGATION_ID, "statement": "pool exhausted",
               "status": "supported", "references": [{"result_id": RESULT_ID, "row": 2}],
               "remaining_question": "why 09:00?"}),
        json!({"action": "finding_status", "id": INVESTIGATION_ID, "finding_seq": 3,
               "status": "refuted", "expected_seq": 3}),
        json!({"action": "status", "id": INVESTIGATION_ID, "status": "closed",
               "expected_seq": 4}),
    ] {
        assert_ok(&call(&backend, &seen, arguments).await);
    }
    let calls = backend.calls();
    // The first touch sends no seen_seq; the session then starts at the
    // entry it wrote.
    assert_eq!(
        sent(&calls[0]),
        json!({"kind": "note", "text": "first"}),
        "{calls:?}"
    );
    assert_eq!(
        sent(&calls[1]),
        json!({"kind": "keep", "result_id": RESULT_ID, "rows": [0, 2], "reason": "the reset",
               "seen_seq": 1})
    );
    assert_eq!(
        sent(&calls[2]),
        json!({"kind": "finding", "statement": "pool exhausted", "status": "supported",
               "references": [{"result_id": RESULT_ID, "row": 2}],
               "remaining_question": "why 09:00?", "seen_seq": 1})
    );
    assert!(calls[3].starts_with(&format!("set_finding_status {INVESTIGATION_ID} 3 ")));
    assert_eq!(
        sent(&calls[3]),
        json!({"expected_seq": 3, "status": "refuted"})
    );
    // PATCH carries status alone: one field per request.
    assert_eq!(
        sent(&calls[4]),
        json!({"expected_seq": 4, "status": "closed"})
    );
    // Nothing other keys wrote was printed, so seen stays at the first
    // touch.
    assert_eq!(seen.lock().await.get(&id()), Some(1));
}

// -- exactly once -------------------------------------------------------------------

#[tokio::test]
async fn activity_prints_the_other_keys_entries_exactly_once_per_session() {
    let backend = Fake {
        append_entry: answers(vec![
            appended(note(1, agent(), "mine 1"), None),
            appended(
                note(4, agent(), "mine 2"),
                Some(activity(
                    vec![note(2, human(), "theirs 2"), note(3, human(), "theirs 3")],
                    false,
                )),
            ),
            appended(note(5, agent(), "mine 3"), Some(activity(vec![], false))),
        ]),
        ..Fake::default()
    };
    let seen = Seen::default();
    let note_call = |text: &str| json!({"action": "note", "id": INVESTIGATION_ID, "text": text});

    let first = call(&backend, &seen, note_call("mine 1")).await;
    assert_ok(&first);
    assert!(
        text_of(&first)
            .contains("activity: this session starts following this investigation at #1"),
        "{}",
        text_of(&first)
    );

    let second = call(&backend, &seen, note_call("mine 2")).await;
    assert_ok(&second);
    let text = text_of(&second);
    assert!(
        text.contains("activity by other keys since #1, in change order: 2 entries"),
        "{text}"
    );
    assert!(
        text.contains(r#"#2 note by Zy9x8w7v "jakub" at 2026-10-24T09:05:00.000000Z: "theirs 2""#),
        "{text}"
    );
    assert!(text.contains(r#"#3 note by Zy9x8w7v "jakub""#), "{text}");
    assert!(!text.contains("next: investigation"), "{text}");
    assert_eq!(block(&second, "activity")["printed_through"], json!(3));
    assert_eq!(seen.lock().await.get(&id()), Some(3));

    let third = call(&backend, &seen, note_call("mine 3")).await;
    assert_ok(&third);
    assert!(
        text_of(&third).contains("activity by other keys since #3, in change order: none."),
        "{}",
        text_of(&third)
    );
    assert!(!text_of(&third).contains("theirs"), "{}", text_of(&third));
    let calls = backend.calls();
    assert_eq!(sent(&calls[1])["seen_seq"], json!(1));
    assert_eq!(sent(&calls[2])["seen_seq"], json!(3));
}

#[tokio::test]
async fn a_cut_activity_counts_only_what_it_printed_and_continues_from_there() {
    let theirs: Vec<LogEntry> = (2..=41)
        .map(|seq| note(seq, human(), &format!("{seq}: {}", "w".repeat(300))))
        .collect();
    let rest: Vec<LogEntry> = theirs.clone();
    let backend = Fake {
        append_entry: answers(vec![
            appended(note(1, agent(), "mine"), None),
            appended(
                note(42, agent(), "mine again"),
                Some(activity(theirs, true)),
            ),
        ]),
        read_investigation: Some(Box::new(move || {
            Ok(InvestigationRead {
                investigation: investigation(60),
                entries: rest.clone(),
                through_seq: 41,
                more: false,
            })
        })),
        ..Fake::default()
    };
    let seen = Seen::default();
    let _ = call(
        &backend,
        &seen,
        json!({"action": "note", "id": INVESTIGATION_ID, "text": "mine"}),
    )
    .await;
    let cut = call(
        &backend,
        &seen,
        json!({"action": "note", "id": INVESTIGATION_ID, "text": "mine again",
               "max_bytes": 4096}),
    )
    .await;
    assert_ok(&cut);
    assert!(used_bytes(&cut) <= 4096, "{}", used_bytes(&cut));
    let view = block(&cut, "activity");
    let printed = view["entries"].as_array().unwrap().len();
    assert!(printed > 0 && printed < 40, "{printed}");
    let through = view["printed_through"].as_u64().unwrap();
    assert_eq!(through, 1 + printed as u64, "entries cut from the end only");
    assert_eq!(view["entries_dropped_to_fit"], json!(40 - printed));
    let text = text_of(&cut);
    let next = format!(
        "next: investigation {{\"action\":\"read\",\"id\":\"{INVESTIGATION_ID}\",\"since\":{through}}}"
    );
    assert!(text.contains(&next), "{text}");
    assert!(
        text.contains("to fit the 4096-byte budget it dropped"),
        "{text}"
    );
    assert_eq!(seen.lock().await.get(&id()), Some(through));

    // The continuation prints the rest: a read from there moves seen past
    // the rest, so the next write carries none of them again.
    let read = call(
        &backend,
        &seen,
        json!({"action": "read", "id": INVESTIGATION_ID, "since": through}),
    )
    .await;
    assert_ok(&read);
    assert!(
        backend
            .calls()
            .last()
            .unwrap()
            .ends_with(&format!("{through} Some(50)"))
    );
    let read_through = block(&read, "entries")["printed_through"].as_u64().unwrap();
    assert!(read_through > through);
    assert_eq!(seen.lock().await.get(&id()), Some(read_through));
}

#[tokio::test]
async fn a_long_entry_prints_abbreviated_and_counts_as_seen() {
    let long = note(2, human(), &"z".repeat(3000));
    let backend = Fake {
        append_entry: answers(vec![
            appended(note(1, agent(), "mine"), None),
            appended(note(3, agent(), "again"), Some(activity(vec![long], false))),
        ]),
        ..Fake::default()
    };
    let seen = Seen::default();
    let _ = call(
        &backend,
        &seen,
        json!({"action": "note", "id": INVESTIGATION_ID, "text": "mine"}),
    )
    .await;
    let result = call(
        &backend,
        &seen,
        json!({"action": "note", "id": INVESTIGATION_ID, "text": "again"}),
    )
    .await;
    assert_ok(&result);
    let line = text_of(&result)
        .lines()
        .find(|line| line.starts_with("#2 "))
        .unwrap()
        .to_owned();
    assert!(line.len() <= ENTRY_ALLOWANCE_BYTES, "{line}");
    assert!(
        line.ends_with(&format!(
            ": …[+3002 bytes: investigation action=read id={INVESTIGATION_ID} since=1]"
        )),
        "{line}"
    );
    let entry = &block(&result, "activity")["entries"][0];
    assert!(entry.get("payload").is_none(), "{entry}");
    assert_eq!(entry["abbreviated_bytes"], json!(3002));
    assert_eq!(seen.lock().await.get(&id()), Some(2));
}

#[tokio::test]
async fn a_refused_write_moves_nothing() {
    let backend = Fake {
        append_entry: each(vec![
            Ok(appended(note(1, agent(), "mine"), None)),
            Err(server_error(409, ErrorCode::InvestigationClosed, "closed")),
        ]),
        ..Fake::default()
    };
    let seen = Seen::default();
    let _ = call(
        &backend,
        &seen,
        json!({"action": "note", "id": INVESTIGATION_ID, "text": "mine"}),
    )
    .await;
    let refused = call(
        &backend,
        &seen,
        json!({"action": "note", "id": INVESTIGATION_ID, "text": "again"}),
    )
    .await;
    assert_eq!(refused.is_error, Some(true));
    assert_eq!(seen.lock().await.get(&id()), Some(1));
}

// -- refusals ---------------------------------------------------------------------

fn conflict(expected: u64, current: u64) -> ClientError {
    ClientError::Server {
        status: 409,
        error: ErrorEnvelope {
            conflict: Some(SequenceConflict { expected, current }),
            ..ErrorEnvelope::simple(ErrorCode::SequenceConflict, "the investigation moved")
        },
        request_id: Some("req-409".to_owned()),
        retry_after_secs: None,
    }
}

/// Each refusal case: the backend, the call, and what its text must say.
fn refusal_cases() -> Vec<(Fake, Json, Vec<String>)> {
    let status_call = json!({"action": "status", "id": INVESTIGATION_ID, "status": "closed",
                             "expected_seq": 4});
    let note_call = json!({"action": "note", "id": INVESTIGATION_ID, "text": "n"});
    let create_call = json!({"action": "create", "id": INVESTIGATION_ID, "title": "t",
                             "question": "q"});
    vec![
        (
            Fake {
                patch_investigation: Some(Box::new(|| Err(conflict(4, 9)))),
                ..Fake::default()
            },
            status_call.clone(),
            vec![
                "investigation was refused: sequence_conflict (HTTP 409).".to_owned(),
                "this call expected seq 4, and it is at seq 9".to_owned(),
                format!("investigation action=read id={INVESTIGATION_ID} since=4"),
                "expected_seq=9".to_owned(),
                "request id: req-409".to_owned(),
            ],
        ),
        (
            Fake {
                create_investigation: refuse(409, ErrorCode::InvestigationExists, "taken"),
                ..Fake::default()
            },
            create_call,
            vec![
                "investigation_exists (HTTP 409)".to_owned(),
                "exists with another creator or other fields".to_owned(),
                "create without an id".to_owned(),
            ],
        ),
        (
            Fake {
                append_entry: refuse(409, ErrorCode::InvestigationClosed, "closed"),
                ..Fake::default()
            },
            note_call.clone(),
            vec![
                "investigation_closed (HTTP 409)".to_owned(),
                format!("investigation action=status id={INVESTIGATION_ID} status=open"),
            ],
        ),
        (
            Fake {
                append_entry: refuse(
                    409,
                    ErrorCode::EvidenceBudgetExceeded,
                    "evidence budget: 64 MiB used of 64 MiB",
                ),
                ..Fake::default()
            },
            json!({"action": "keep", "id": INVESTIGATION_ID, "result_id": RESULT_ID,
                   "rows": [1], "reason": "r"}),
            vec![
                "evidence_budget_exceeded (HTTP 409)".to_owned(),
                "trawld says: evidence budget: 64 MiB used of 64 MiB".to_owned(),
                "server_manage".to_owned(),
            ],
        ),
        (
            Fake {
                append_entry: refuse(403, ErrorCode::Forbidden, "insufficient permissions"),
                ..Fake::default()
            },
            note_call,
            vec![
                "forbidden (HTTP 403)".to_owned(),
                "investigation action=note needs the query, investigation_read and \
                 investigation_write permissions"
                    .to_owned(),
            ],
        ),
        (
            Fake {
                read_investigation: refuse(403, ErrorCode::Forbidden, "insufficient permissions"),
                ..Fake::default()
            },
            json!({"action": "read", "id": INVESTIGATION_ID}),
            vec![
                "investigation action=read needs the query and investigation_read permissions"
                    .to_owned(),
            ],
        ),
        (
            Fake {
                read_investigation: refuse(404, ErrorCode::NotFound, "no such investigation"),
                ..Fake::default()
            },
            json!({"action": "read", "id": INVESTIGATION_ID}),
            vec![
                "not_found (HTTP 404)".to_owned(),
                "investigation action=list".to_owned(),
            ],
        ),
    ]
}

#[tokio::test]
async fn each_refusal_names_trawlds_code_and_the_way_back() {
    for (backend, arguments, expected) in refusal_cases() {
        let seen = Seen::default();
        let result = call(&backend, &seen, arguments.clone()).await;
        assert_eq!(result.is_error, Some(true), "{arguments}");
        assert!(result.structured_content.is_none());
        let text = text_of(&result);
        for part in expected {
            assert!(text.contains(&part), "{arguments}: {part:?} not in {text}");
        }
        assert!(!text.contains("key_id"), "{text}");
        assert_eq!(backend.calls().len(), 1, "nothing retries");
        assert_eq!(seen.lock().await.get(&id()), None);
    }
}

// -- the board ----------------------------------------------------------------------

fn every_kind() -> Vec<LogEntry> {
    let entry = |seq: u64, updated_seq: u64, payload: EntryPayload| LogEntry {
        seq,
        updated_seq,
        actor: if seq.is_multiple_of(2) {
            human()
        } else {
            agent()
        },
        at: "2026-10-24T09:05:00.000000Z".to_owned(),
        payload,
    };
    vec![
        entry(
            2,
            2,
            EntryPayload::Keep {
                result_id: RESULT_ID.to_owned(),
                rows: vec![7, 9],
                reason: "the first 502\nand its reset".to_owned(),
            },
        ),
        entry(
            1,
            3,
            EntryPayload::Step {
                executed: "_service=checkout status>=500".to_owned(),
                bounds: Some(StepBounds {
                    earliest: "2026-10-24T08:00:00.000000Z".to_owned(),
                    latest: "2026-10-24T09:00:00.000000Z".to_owned(),
                }),
                request_id: "req-1".to_owned(),
                query_id: Some(41),
                purpose: Some("find the first 502".to_owned()),
                outcome: Some(StepOutcome {
                    outcome: StepOutcomeKind::Success,
                    code: None,
                    result_id: Some(RESULT_ID.to_owned()),
                    rows: Some(120),
                    duration_ms: Some(250),
                }),
            },
        ),
        entry(
            4,
            4,
            EntryPayload::Finding {
                statement: "the upstream pool is exhausted".to_owned(),
                status: FindingStatus::Hypothesis,
                references: vec![EvidenceRef {
                    result_id: RESULT_ID.to_owned(),
                    row: 7,
                }],
                counterevidence: vec![EvidenceRef {
                    result_id: RESULT_ID.to_owned(),
                    row: 9,
                }],
                remaining_question: Some("why at 09:00?".to_owned()),
            },
        ),
        entry(
            5,
            5,
            EntryPayload::FindingStatus {
                finding_seq: 4,
                status: FindingStatus::Supported,
            },
        ),
        entry(
            6,
            6,
            EntryPayload::Note {
                text: "paged the on-call \u{1b}[2J".to_owned(),
            },
        ),
        entry(
            7,
            7,
            EntryPayload::Status {
                status: InvestigationStatus::Closed,
            },
        ),
        entry(
            8,
            8,
            EntryPayload::Title {
                title: "pool exhaustion".to_owned(),
            },
        ),
        entry(
            9,
            9,
            EntryPayload::Question {
                question: "What exhausts the pool?".to_owned(),
            },
        ),
    ]
}

#[tokio::test]
async fn a_board_read_prints_every_entry_kind_on_its_own_line() {
    let board = InvestigationRead {
        investigation: Investigation {
            scope: "checkout in prod".to_owned(),
            updated_by: human(),
            ..investigation(9)
        },
        entries: every_kind(),
        through_seq: 9,
        more: false,
    };
    let backend = Fake {
        read_investigation: ok(board),
        ..Fake::default()
    };
    let seen = Seen::default();
    let result = call(
        &backend,
        &seen,
        json!({"action": "read", "id": INVESTIGATION_ID}),
    )
    .await;
    assert_ok(&result);
    assert_eq!(
        backend.calls(),
        [format!("read_investigation {INVESTIGATION_ID} 0 Some(50)")]
    );
    let text = text_of(&result);
    let expected = [
        format!("Investigation \"{INVESTIGATION_ID}\" is open at seq 9."),
        format!("investigation \"{INVESTIGATION_ID}\": \"502s on checkout\", open, seq 9"),
        "question: \"Why does checkout return 502 since 09:00?\"".to_owned(),
        "scope: \"checkout in prod\"".to_owned(),
        "evidence kept until 2026-10-31T09:05:00.000000Z, at most until \
         2026-11-23T09:00:00.000000Z unless an operator extends it"
            .to_owned(),
        "created by abcd1234 \"agent\" at 2026-10-24T09:00:00.000000Z; last changed by \
         Zy9x8w7v \"jakub\" at 2026-10-24T09:05:00.000000Z"
            .to_owned(),
        "entries changed after #0, in change order: 8 entries".to_owned(),
        format!(
            "#2 keep by Zy9x8w7v \"jakub\" at 2026-10-24T09:05:00.000000Z: kept rows [7,9] of \
             result \"{RESULT_ID}\"; reason \"the first 502\\nand its reset\""
        ),
        format!(
            "#1 (changed at #3) step by abcd1234 \"agent\" at 2026-10-24T09:05:00.000000Z: ran \
             \"_service=checkout status>=500\" from 2026-10-24T08:00:00.000000Z (inclusive) to \
             2026-10-24T09:00:00.000000Z (exclusive); request id \"req-1\"; query id 41; \
             purpose \"find the first 502\"; outcome success; kept result \"{RESULT_ID}\" with \
             120 rows; ran 250 ms"
        ),
        format!(
            "#4 finding by Zy9x8w7v \"jakub\" at 2026-10-24T09:05:00.000000Z: hypothesis \"the \
             upstream pool is exhausted\"; references [{{\"result_id\":\"{RESULT_ID}\",\"row\":7}}]; \
             counterevidence [{{\"result_id\":\"{RESULT_ID}\",\"row\":9}}]; remaining question \
             \"why at 09:00?\""
        ),
        "#5 finding_status by abcd1234 \"agent\" at 2026-10-24T09:05:00.000000Z: finding #4 is \
         now supported"
            .to_owned(),
        "#6 note by Zy9x8w7v \"jakub\" at 2026-10-24T09:05:00.000000Z: \"paged the on-call \
         \\u001b[2J\""
            .to_owned(),
        "#7 status by abcd1234 \"agent\" at 2026-10-24T09:05:00.000000Z: the investigation is \
         now closed"
            .to_owned(),
        "#8 title by Zy9x8w7v \"jakub\" at 2026-10-24T09:05:00.000000Z: the title is now \"pool \
         exhaustion\""
            .to_owned(),
        "#9 question by abcd1234 \"agent\" at 2026-10-24T09:05:00.000000Z: the question is now \
         \"What exhausts the pool?\""
            .to_owned(),
    ];
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines, expected, "{text}");
    assert!(!text.contains('\u{1b}'));
    // The whole board read from the start counts as seen.
    assert_eq!(seen.lock().await.get(&id()), Some(9));
    // The mirror carries each entry as trawld spells it.
    assert_eq!(
        block(&result, "entries")["entries"][1]["payload"]["kind"],
        json!("step")
    );
}

#[tokio::test]
async fn the_read_tool_reads_a_board_with_the_same_renderer_and_continues_in_read() {
    let board = InvestigationRead {
        investigation: investigation(9),
        entries: every_kind(),
        through_seq: 9,
        more: true,
    };
    let backend = Fake {
        read_investigation: ok(board),
        ..Fake::default()
    };
    let seen = Seen::default();
    let read_input: ReadInput = serde_json::from_value(json!({
        "action": "investigation", "investigation_id": INVESTIGATION_ID, "since": 0
    }))
    .unwrap();
    let from_read = read::run(&backend, &seen, read_input).await;
    assert_ok(&from_read);
    let from_tool = call(
        &backend,
        &Seen::default(),
        json!({"action": "read", "id": INVESTIGATION_ID}),
    )
    .await;
    let lines = |result: &CallToolResult| -> Vec<String> {
        text_of(result)
            .lines()
            .filter(|line| line.starts_with('#'))
            .map(str::to_owned)
            .collect()
    };
    assert_eq!(lines(&from_read), lines(&from_tool));
    assert!(
        text_of(&from_read).contains(&format!(
            "next: read {{\"action\":\"investigation\",\"investigation_id\":\"{INVESTIGATION_ID}\",\"since\":9}}"
        )),
        "{}",
        text_of(&from_read)
    );
    assert!(
        text_of(&from_tool).contains(&format!(
            "next: investigation {{\"action\":\"read\",\"id\":\"{INVESTIGATION_ID}\",\"since\":9}}"
        )),
        "{}",
        text_of(&from_tool)
    );
    assert_eq!(seen.lock().await.get(&id()), Some(9));
}

/// A board entry no budget fits prints as its header and a marker, and the
/// read still counts it as seen: the marker names the call that prints it
/// whole, or says that no response can.
#[tokio::test]
async fn a_board_entry_too_long_for_the_budget_prints_abbreviated_and_the_read_advances() {
    let marker_at_the_hard_cap =
        format!("investigation action=read id={INVESTIGATION_ID} since=2 max_bytes=32768]");
    let marker_past_every_cap = "the entry is larger than any response holds; the \
                                 investigation's page in trawl's web UI shows it whole]"
        .to_owned();
    for (executed_bytes, marker) in [
        (12_000, marker_at_the_hard_cap),
        (20_000, marker_past_every_cap),
    ] {
        let huge = LogEntry {
            payload: EntryPayload::Step {
                executed: "x".repeat(executed_bytes),
                bounds: None,
                request_id: "req-9".to_owned(),
                query_id: None,
                purpose: None,
                outcome: None,
            },
            ..note(3, human(), "")
        };
        let backend = Fake {
            read_investigation: ok(InvestigationRead {
                investigation: investigation(4),
                entries: vec![huge, note(4, human(), "after")],
                through_seq: 4,
                more: false,
            }),
            ..Fake::default()
        };
        let seen = Seen::default();
        let result = call(
            &backend,
            &seen,
            json!({"action": "read", "id": INVESTIGATION_ID, "since": 2}),
        )
        .await;
        assert_ok(&result);
        let text = text_of(&result);
        assert!(
            text.contains(&format!(
                "#3 step by Zy9x8w7v \"jakub\" at 2026-10-24T09:05:00.000000Z: …[+{} bytes: \
                 {marker}",
                executed_bytes + 41
            )),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "next: investigation {{\"action\":\"read\",\"id\":\"{INVESTIGATION_ID}\",\"since\":3}}"
            )),
            "{text}"
        );
        assert_eq!(seen.lock().await.get(&id()), Some(3));
    }
}

/// A board whose first entry does not fit even abbreviated, its
/// attribution larger than any response, answers an error naming the
/// investigation and the entry and the read that steps past it, never a
/// success that prints no entry and proposes the same read again; the
/// read past it prints the next entry.
#[tokio::test]
async fn a_board_entry_that_cannot_print_is_an_error_naming_it_and_the_read_past_it() {
    let huge = Actor {
        name: "n".repeat(40_000),
        ..human()
    };
    for max_bytes in [4096, 32768] {
        let backend = Fake {
            read_investigation: ok(InvestigationRead {
                investigation: investigation(2),
                entries: vec![note(1, huge.clone(), "first"), note(2, human(), "after")],
                through_seq: 2,
                more: false,
            }),
            ..Fake::default()
        };
        let seen = Seen::default();
        let result = call(
            &backend,
            &seen,
            json!({"action": "read", "id": INVESTIGATION_ID, "since": 0,
                   "max_bytes": max_bytes}),
        )
        .await;
        let text = text_of(&result);
        assert_eq!(result.is_error, Some(true), "max_bytes={max_bytes}: {text}");
        for part in [
            format!("entry #1 of investigation \"{INVESTIGATION_ID}\""),
            format!(
                "investigation {{\"action\":\"read\",\"id\":\"{INVESTIGATION_ID}\",\"since\":1}}"
            ),
            "trawl's web UI".to_owned(),
        ] {
            assert!(
                text.contains(&part),
                "max_bytes={max_bytes}: {part:?} not in {text}"
            );
        }
        assert_eq!(seen.lock().await.get(&id()), None, "max_bytes={max_bytes}");
        assert!(!text.contains("a larger max_bytes"), "{text}");
    }

    // An attribution the hard cap holds: the error names the larger budget.
    let backend = Fake {
        read_investigation: ok(InvestigationRead {
            investigation: investigation(2),
            entries: vec![note(
                1,
                Actor {
                    name: "n".repeat(10_000),
                    ..human()
                },
                "first",
            )],
            through_seq: 1,
            more: false,
        }),
        ..Fake::default()
    };
    let result = call(
        &backend,
        &Seen::default(),
        json!({"action": "read", "id": INVESTIGATION_ID, "since": 0, "max_bytes": 4096}),
    )
    .await;
    assert_eq!(result.is_error, Some(true), "{}", text_of(&result));
    assert!(
        text_of(&result).contains("a larger max_bytes, at most 32768, prints it abbreviated"),
        "{}",
        text_of(&result)
    );

    let backend = Fake {
        read_investigation: ok(InvestigationRead {
            investigation: investigation(2),
            entries: vec![note(2, human(), "after")],
            through_seq: 2,
            more: false,
        }),
        ..Fake::default()
    };
    let result = call(
        &backend,
        &Seen::default(),
        json!({"action": "read", "id": INVESTIGATION_ID, "since": 1, "max_bytes": 4096}),
    )
    .await;
    assert_ok(&result);
    assert!(
        text_of(&result).contains("#2 note by"),
        "{}",
        text_of(&result)
    );
}

#[tokio::test]
async fn list_prints_every_investigation_on_one_line() {
    let summary = |id: &str, status: InvestigationStatus, title: &str| InvestigationSummary {
        id: InvestigationId::parse(id).unwrap(),
        title: title.to_owned(),
        status,
        seq: 3,
        created_at: "2026-10-24T09:00:00.000000Z".to_owned(),
        updated_at: "2026-10-24T09:05:00.000000Z".to_owned(),
        created_by: agent(),
        updated_by: human(),
    };
    let backend = Fake {
        list_investigations: ok(InvestigationList {
            investigations: vec![
                summary(
                    INVESTIGATION_ID,
                    InvestigationStatus::Open,
                    "502s\non checkout",
                ),
                summary(RESULT_ID, InvestigationStatus::Closed, "disk"),
            ],
        }),
        ..Fake::default()
    };
    let result = call(&backend, &Seen::default(), json!({"action": "list"})).await;
    assert_ok(&result);
    let text = text_of(&result);
    assert!(text.starts_with("2 investigations, 1 open."), "{text}");
    assert!(
        text.contains(&format!(
            "\"{INVESTIGATION_ID}\" open, seq 3, changed 2026-10-24T09:05:00.000000Z by Zy9x8w7v \
             \"jakub\": \"502s\\non checkout\""
        )),
        "{text}"
    );
}

#[tokio::test]
async fn every_reply_validates_against_the_output_schema() {
    let tool = tools()
        .into_iter()
        .find(|t| t.name == INVESTIGATION)
        .unwrap();
    let schema = Json::Object((*tool.output_schema.unwrap()).clone());
    let validator = jsonschema::draft202012::new(&schema).unwrap();
    let backend = Fake {
        read_investigation: ok(InvestigationRead {
            investigation: investigation(9),
            entries: every_kind(),
            through_seq: 9,
            more: true,
        }),
        append_entry: answers(vec![
            appended(note(1, agent(), "a"), None),
            appended(
                note(10, agent(), "b"),
                Some(activity(
                    vec![
                        note(11, human(), &"long ".repeat(400)),
                        note(12, human(), "c"),
                    ],
                    true,
                )),
            ),
        ]),
        create_investigation: ok((true, investigation(0))),
        ..Fake::default()
    };
    let seen = Seen::default();
    for arguments in [
        json!({"action": "create", "title": "t", "question": "q"}),
        json!({"action": "read", "id": INVESTIGATION_ID, "max_bytes": 4096}),
        json!({"action": "note", "id": INVESTIGATION_ID, "text": "a"}),
        json!({"action": "note", "id": INVESTIGATION_ID, "text": "b"}),
    ] {
        let result = call(&backend, &seen, arguments.clone()).await;
        assert_ok(&result);
        let mirror = result.structured_content.unwrap();
        let errors: Vec<String> = validator
            .iter_errors(&mirror)
            .map(|e| e.to_string())
            .collect();
        assert!(errors.is_empty(), "{arguments}: {errors:?}");
    }
}

use trawl_client::Investigation;

// -- a write trawld accepted is acknowledged at every budget ------------------------

/// The budgets every acknowledgement must fit: the minimum, the default,
/// the hard cap.
const BUDGETS: [u32; 3] = [4096, 16384, 32768];

/// `bytes` of `fill`: `x`, or U+0001, which prints escaped to six bytes.
fn filled(fill: char, bytes: usize) -> String {
    fill.to_string().repeat(bytes / fill.len_utf8())
}

/// An investigation at `seq` whose title, question and scope are as long
/// as trawld takes, each byte `fill`.
fn maximal_investigation(fill: char, seq: u64) -> Investigation {
    use trawl_api::investigations::{QUESTION_MAX_BYTES, SCOPE_MAX_BYTES, TITLE_MAX_BYTES};
    Investigation {
        title: filled(fill, TITLE_MAX_BYTES),
        question: filled(fill, QUESTION_MAX_BYTES),
        scope: filled(fill, SCOPE_MAX_BYTES),
        ..investigation(seq)
    }
}

fn row_refs(count: usize) -> Vec<EvidenceRef> {
    (0..count)
        .map(|row| EvidenceRef {
            result_id: RESULT_ID.to_owned(),
            row: row as u64,
        })
        .collect()
}

fn validate_against(tool: &str, result: &CallToolResult) {
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

/// This key's entry #4 holding `payload`.
fn mine(payload: EntryPayload) -> LogEntry {
    LogEntry {
        payload,
        ..note(4, agent(), "")
    }
}

/// A backend that appends `entry`, answering two other keys' entries as
/// long as a note may be, each byte `fill`.
fn appends(entry: LogEntry, fill: char) -> Fake {
    use trawl_api::investigations::NOTE_MAX_BYTES;
    let theirs = activity(
        vec![
            note(2, human(), &filled(fill, NOTE_MAX_BYTES)),
            note(3, human(), &filled(fill, NOTE_MAX_BYTES)),
        ],
        false,
    );
    Fake {
        append_entry: ok(appended(entry, Some(theirs))),
        ..Fake::default()
    }
}

/// Every write action with the largest payload trawld accepts, each text
/// byte `fill`: its arguments, the backend that accepts it, and the seq of
/// the entry it writes (0 for create).
fn maximal_writes(fill: char) -> Vec<(Json, Fake, u64)> {
    use trawl_api::investigations::{
        FINDING_MAX_COUNTEREVIDENCE, FINDING_MAX_REFERENCES, FINDING_STATEMENT_MAX_BYTES,
        KEEP_MAX_ROWS, KEEP_REASON_MAX_BYTES, NOTE_MAX_BYTES, REMAINING_QUESTION_MAX_BYTES,
    };
    let board = maximal_investigation(fill, 0);
    let text = filled(fill, NOTE_MAX_BYTES);
    let statement = filled(fill, FINDING_STATEMENT_MAX_BYTES);
    let remaining = filled(fill, REMAINING_QUESTION_MAX_BYTES);
    let reason = filled(fill, KEEP_REASON_MAX_BYTES);
    let rows: Vec<u64> = (0..KEEP_MAX_ROWS as u64).collect();
    let references = row_refs(FINDING_MAX_REFERENCES);
    let counterevidence = row_refs(FINDING_MAX_COUNTEREVIDENCE);
    vec![
        (
            json!({"action": "create", "id": INVESTIGATION_ID, "title": board.title,
                   "question": board.question, "scope": board.scope}),
            Fake {
                create_investigation: ok((true, board.clone())),
                ..Fake::default()
            },
            0,
        ),
        (
            json!({"action": "note", "id": INVESTIGATION_ID, "text": text}),
            appends(note(4, agent(), &text), fill),
            4,
        ),
        (
            json!({"action": "finding", "id": INVESTIGATION_ID, "statement": statement,
                   "status": "supported", "references": references,
                   "counterevidence": counterevidence, "remaining_question": remaining}),
            appends(
                mine(EntryPayload::Finding {
                    statement: statement.clone(),
                    status: FindingStatus::Supported,
                    references: references.clone(),
                    counterevidence: counterevidence.clone(),
                    remaining_question: Some(remaining.clone()),
                }),
                fill,
            ),
            4,
        ),
        (
            json!({"action": "keep", "id": INVESTIGATION_ID, "result_id": RESULT_ID,
                   "rows": rows, "reason": reason}),
            appends(
                mine(EntryPayload::Keep {
                    result_id: RESULT_ID.to_owned(),
                    rows: rows.clone(),
                    reason: reason.clone(),
                }),
                fill,
            ),
            4,
        ),
        (
            json!({"action": "finding_status", "id": INVESTIGATION_ID, "finding_seq": 3,
                   "status": "refuted", "expected_seq": 3}),
            Fake {
                set_finding_status: ok(appended(
                    mine(EntryPayload::FindingStatus {
                        finding_seq: 3,
                        status: FindingStatus::Refuted,
                    }),
                    None,
                )),
                ..Fake::default()
            },
            4,
        ),
        (
            json!({"action": "status", "id": INVESTIGATION_ID, "status": "closed",
                   "expected_seq": 3}),
            Fake {
                patch_investigation: ok(Investigation {
                    status: InvestigationStatus::Closed,
                    ..maximal_investigation(fill, 4)
                }),
                ..Fake::default()
            },
            4,
        ),
    ]
}

/// R1: every write trawld accepted is acknowledged, with the
/// investigation's id and the written seq, at every budget and for every
/// payload trawld takes; an echo too long for the budget prints
/// abbreviated, its marker naming the read that prints it.
#[tokio::test]
async fn every_accepted_write_keeps_its_acknowledgement_at_every_budget() {
    for fill in ['x', '\u{1}'] {
        for max_bytes in BUDGETS {
            for (mut arguments, backend, seq) in maximal_writes(fill) {
                arguments["max_bytes"] = json!(max_bytes);
                let action = arguments["action"].as_str().unwrap().to_owned();
                let label = format!("{action} fill={fill:?} max_bytes={max_bytes}");
                let seen = Seen::default();
                seen.lock().await.touch(&id(), 1);
                let result = call(&backend, &seen, arguments).await;
                assert_ne!(result.is_error, Some(true), "{label}: {}", text_of(&result));
                assert!(used_bytes(&result) <= max_bytes as usize, "{label}");
                validate_against(INVESTIGATION, &result);
                let text = text_of(&result);
                let first = text.lines().next().unwrap();
                let mirror = result.structured_content.as_ref().unwrap();
                assert!(first.contains(INVESTIGATION_ID), "{label}: {first}");
                if action == "create" {
                    assert_eq!(
                        mirror["investigation"]["id"],
                        json!(INVESTIGATION_ID),
                        "{label}"
                    );
                    assert_eq!(mirror["investigation"]["seq"], json!(0), "{label}");
                    assert!(text.contains(", seq 0"), "{label}: {text}");
                    assert_eq!(seen.lock().await.get(&id()), Some(1), "{label}");
                } else {
                    assert!(
                        first.contains(&format!("#{seq}")) || first.contains(&format!("seq {seq}")),
                        "{label}: {first}"
                    );
                }
                if let Some(marker) = mirror["entry"]["abbreviated"].as_str() {
                    let read_with = format!(
                        "investigation action=read id={INVESTIGATION_ID} since={}",
                        seq - 1
                    );
                    assert!(marker.ends_with(&read_with), "{label}: {marker}");
                    assert!(text.contains(&format!("{read_with}]")), "{label}: {text}");
                }
            }
        }
    }
}

/// F2: a create at the minimum budget with the longest title, question and
/// scope trawld takes prints the minted id on its first line, and the
/// session follows it from seq 0 only once the reply fitted.
#[tokio::test]
async fn a_create_with_maximal_metadata_at_the_minimum_prints_the_id_first() {
    for fill in ['x', '\u{1}'] {
        let board = maximal_investigation(fill, 0);
        let backend = Fake {
            create_investigation: ok((true, board.clone())),
            ..Fake::default()
        };
        let seen = Seen::default();
        let result = call(
            &backend,
            &seen,
            json!({"action": "create", "title": board.title, "question": board.question,
                   "scope": board.scope, "max_bytes": 4096}),
        )
        .await;
        assert_ok(&result);
        let minted = backend.calls()[0].split(' ').nth(1).unwrap().to_owned();
        let first = text_of(&result).lines().next().unwrap();
        assert!(
            first.starts_with(&format!("Created investigation \"{minted}\"")),
            "{first}"
        );
        let minted = InvestigationId::parse(&minted).unwrap();
        assert_eq!(seen.lock().await.get(&minted), Some(0));
    }
}

/// F3/R2: the board header is fitted like an entry. An empty board whose
/// title, question and scope are as long as trawld takes reads at the
/// minimum and at the hard cap, through both tools; the cut texts are
/// prefixes of the whole ones in the mirror, each with a marker, and the
/// id, status, seq, retention and attribution stay whole.
#[tokio::test]
async fn an_empty_board_with_maximal_metadata_reads_at_every_budget() {
    for fill in ['x', '\u{1}'] {
        let board = maximal_investigation(fill, 5);
        let backend = Fake {
            read_investigation: ok(InvestigationRead {
                investigation: board.clone(),
                entries: vec![],
                through_seq: 5,
                more: false,
            }),
            ..Fake::default()
        };
        for max_bytes in BUDGETS {
            let label = format!("fill={fill:?} max_bytes={max_bytes}");
            let from_tool = call(
                &backend,
                &Seen::default(),
                json!({"action": "read", "id": INVESTIGATION_ID, "max_bytes": max_bytes}),
            )
            .await;
            let read_input: ReadInput = serde_json::from_value(json!({
                "action": "investigation", "investigation_id": INVESTIGATION_ID,
                "max_bytes": max_bytes
            }))
            .unwrap();
            let from_read = read::run(&backend, &Seen::default(), read_input).await;
            for (tool, result) in [
                (INVESTIGATION, &from_tool),
                (trawl_mcp::tools::schema::READ, &from_read),
            ] {
                assert_ne!(
                    result.is_error,
                    Some(true),
                    "{label} {tool}: {}",
                    text_of(result)
                );
                assert!(used_bytes(result) <= max_bytes as usize, "{label} {tool}");
                validate_against(tool, result);
                let text = text_of(result);
                assert!(
                    text.contains(&format!("investigation \"{INVESTIGATION_ID}\": ")),
                    "{label} {tool}: {text}"
                );
                assert!(text.contains(", open, seq 5"), "{label} {tool}: {text}");
                assert!(text.contains(&board.retain_until), "{label} {tool}");
                let view = &result.structured_content.as_ref().unwrap()["investigation"];
                assert_eq!(
                    view["retain_until"],
                    json!(board.retain_until),
                    "{label} {tool}"
                );
                assert_eq!(view["updated_by"]["name"], json!("agent"), "{label} {tool}");
                // ASCII prints whole from the default budget up; the
                // controls, six bytes each, never do.
                let cut = view["abbreviated"].as_array().cloned().unwrap_or_default();
                assert_eq!(
                    cut.is_empty(),
                    fill == 'x' && max_bytes >= 16384,
                    "{label} {tool}: {text}"
                );
                for field in &cut {
                    let name = field["field"].as_str().unwrap();
                    let whole = match name {
                        "title" => &board.title,
                        "question" => &board.question,
                        "scope" => &board.scope,
                        other => panic!("{other} is not a header text"),
                    };
                    // The mirror carries each control as the escape the
                    // text prints for it.
                    let whole = whole.replace('\u{1}', "\\u0001");
                    let printed = view[name].as_str().unwrap();
                    assert!(
                        whole.starts_with(printed) && printed.len() < whole.len(),
                        "{label} {tool} {name}"
                    );
                    let marker = field["marker"].as_str().unwrap();
                    assert!(
                        marker.starts_with(&format!("+{} bytes: ", field["omitted_bytes"])),
                        "{label} {tool}: {marker}"
                    );
                    assert!(
                        text.contains(&format!("…[{marker}]")),
                        "{label} {tool}: {text}"
                    );
                }
            }
        }
    }
}

/// F1: the board read the abbreviated echo of a maximal finding names
/// renders it at every budget, abbreviated itself when it must be.
#[tokio::test]
async fn the_read_an_abbreviated_echo_names_prints_the_entry_at_every_budget() {
    use trawl_api::investigations::{
        FINDING_MAX_COUNTEREVIDENCE, FINDING_MAX_REFERENCES, FINDING_STATEMENT_MAX_BYTES,
        REMAINING_QUESTION_MAX_BYTES,
    };
    for fill in ['x', '\u{1}'] {
        let finding = LogEntry {
            payload: EntryPayload::Finding {
                statement: filled(fill, FINDING_STATEMENT_MAX_BYTES),
                status: FindingStatus::Supported,
                references: row_refs(FINDING_MAX_REFERENCES),
                counterevidence: row_refs(FINDING_MAX_COUNTEREVIDENCE),
                remaining_question: Some(filled(fill, REMAINING_QUESTION_MAX_BYTES)),
            },
            ..note(4, agent(), "")
        };
        let backend = Fake {
            read_investigation: ok(InvestigationRead {
                investigation: maximal_investigation(fill, 4),
                entries: vec![finding],
                through_seq: 4,
                more: false,
            }),
            ..Fake::default()
        };
        for max_bytes in BUDGETS {
            let label = format!("fill={fill:?} max_bytes={max_bytes}");
            let seen = Seen::default();
            seen.lock().await.touch(&id(), 3);
            let result = call(
                &backend,
                &seen,
                json!({"action": "read", "id": INVESTIGATION_ID, "since": 3,
                       "max_bytes": max_bytes}),
            )
            .await;
            assert_ne!(result.is_error, Some(true), "{label}: {}", text_of(&result));
            assert!(used_bytes(&result) <= max_bytes as usize, "{label}");
            validate_against(INVESTIGATION, &result);
            assert!(
                text_of(&result).contains("#4 finding by abcd1234 \"agent\""),
                "{label}: {}",
                text_of(&result)
            );
            assert_eq!(seen.lock().await.get(&id()), Some(4), "{label}");
        }
    }
}

/// R3: the session's bookkeeping moves only on a fitted success. A reply
/// that cannot fit, here because trawld names an actor longer than any
/// response, is an error that names the accepted write and leaves `seen`
/// where it was.
#[tokio::test]
async fn seen_moves_only_after_a_fitted_success() {
    let huge = Actor {
        prefix: "abcd1234".to_owned(),
        name: "n".repeat(40_000),
    };
    let unprintable = Investigation {
        updated_by: huge.clone(),
        ..investigation(4)
    };
    let backend = Fake {
        create_investigation: ok((
            true,
            Investigation {
                seq: 0,
                ..unprintable.clone()
            },
        )),
        patch_investigation: ok(unprintable),
        set_finding_status: ok(appended(
            LogEntry {
                actor: huge.clone(),
                payload: EntryPayload::FindingStatus {
                    finding_seq: 3,
                    status: FindingStatus::Refuted,
                },
                ..note(4, agent(), "")
            },
            None,
        )),
        append_entry: ok(appended(note(4, huge, "a"), None)),
        ..Fake::default()
    };
    for arguments in [
        json!({"action": "create", "id": INVESTIGATION_ID, "title": "t", "question": "q"}),
        json!({"action": "create", "id": INVESTIGATION_ID, "title": "t", "question": "q",
               "max_bytes": 32768}),
        json!({"action": "status", "id": INVESTIGATION_ID, "status": "closed",
               "expected_seq": 3}),
        json!({"action": "finding_status", "id": INVESTIGATION_ID, "finding_seq": 3,
               "status": "refuted", "expected_seq": 3}),
        json!({"action": "note", "id": INVESTIGATION_ID, "text": "a"}),
    ] {
        let seen = Seen::default();
        let result = call(&backend, &seen, arguments.clone()).await;
        assert_eq!(result.is_error, Some(true), "{arguments}");
        let text = text_of(&result);
        assert!(text.contains(INVESTIGATION_ID), "{arguments}: {text}");
        if arguments["max_bytes"] == json!(32768) {
            // No larger budget exists to suggest.
            assert!(
                !text.contains("send a larger max_bytes"),
                "{arguments}: {text}"
            );
        }
        if arguments["action"] == json!("create") {
            assert!(
                text.contains(&format!("send the same create with id={INVESTIGATION_ID}")),
                "{arguments}: {text}"
            );
        }
        assert_eq!(seen.lock().await.get(&id()), None, "{arguments}");
    }
    // The same create with printable fields fits, and only then moves seen.
    let backend = Fake {
        create_investigation: ok((true, investigation(0))),
        ..Fake::default()
    };
    let seen = Seen::default();
    assert_ok(
        &call(
            &backend,
            &seen,
            json!({"action": "create", "id": INVESTIGATION_ID, "title": "t", "question": "q"}),
        )
        .await,
    );
    assert_eq!(seen.lock().await.get(&id()), Some(0));
}

use trawl_api::investigations::Actor;

// -- format characters -------------------------------------------------------------

/// A right-to-left override (it reorders the text after it), a zero-width
/// space (it hides text) and a soft hyphen (invisible): characters another
/// principal chose, in text the board prints into an agent's context.
const FORMAT_CHARS: &str = "\u{202e}\u{200b}\u{00ad}";

fn assert_no_raw_format_char(text: &str) {
    for c in FORMAT_CHARS.chars() {
        assert!(
            !text.contains(c),
            "U+{:04X} reached the text raw: {text:?}",
            u32::from(c)
        );
    }
}

fn hostile(label: &str) -> String {
    format!("{label}{FORMAT_CHARS}tail")
}

/// `label` as the text block prints it: a JSON string, each format
/// character its `\uXXXX` escape, as a control character is.
fn printed(label: &str) -> String {
    format!("\"{}\"", mirrored(label))
}

/// `label` as the mirror carries it: the very characters the text block
/// prints between the quotes, each format character its `\uXXXX` escape.
fn mirrored(label: &str) -> String {
    format!("{label}\\u202e\\u200b\\u00adtail")
}

/// The mirror's serialization, which a client may show the model in place
/// of the text, holds no raw format character.
fn assert_mirror_escaped(result: &CallToolResult) {
    let mirror = result.structured_content.as_ref().expect("a mirror");
    assert_no_raw_format_char(&mirror.to_string());
}

#[tokio::test]
async fn a_board_prints_format_characters_in_stored_text_escaped_in_the_text_and_the_mirror() {
    let other = Actor {
        prefix: "Zy9x8w7v".to_owned(),
        name: hostile("mallory"),
    };
    let board = InvestigationRead {
        investigation: Investigation {
            title: hostile("title"),
            question: hostile("question"),
            scope: hostile("scope"),
            updated_by: other.clone(),
            ..investigation(2)
        },
        entries: vec![
            note(1, other.clone(), &hostile("note")),
            LogEntry {
                payload: EntryPayload::Title {
                    title: hostile("retitled"),
                },
                ..note(2, other, "")
            },
        ],
        through_seq: 2,
        more: false,
    };
    let backend = Fake {
        read_investigation: ok(board),
        ..Fake::default()
    };
    let result = call(
        &backend,
        &Seen::default(),
        json!({"action": "read", "id": INVESTIGATION_ID}),
    )
    .await;
    assert_ok(&result);
    let text = text_of(&result);
    assert_no_raw_format_char(text);
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines.contains(&format!("question: {}", printed("question")).as_str()),
        "{text}"
    );
    assert!(
        lines.contains(&format!("scope: {}", printed("scope")).as_str()),
        "{text}"
    );
    assert!(
        lines.iter().any(|line| line.starts_with(&format!(
            "investigation \"{INVESTIGATION_ID}\": {}, open",
            printed("title")
        ))),
        "{text}"
    );
    assert!(
        lines.iter().any(|line| line.starts_with(&format!(
            "#1 note by Zy9x8w7v {} at 2026-10-24T09:05:00.000000Z: {}",
            printed("mallory"),
            printed("note")
        ))),
        "{text}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.ends_with(&format!("the title is now {}", printed("retitled")))),
        "{text}"
    );
    // The mirror carries the same escapes, so a client that shows the
    // model the mirror shows what the text block prints.
    assert_mirror_escaped(&result);
    let mirror = result.structured_content.as_ref().expect("a mirror");
    for (path, label) in [
        (&["investigation", "title"][..], "title"),
        (&["investigation", "question"][..], "question"),
        (&["investigation", "scope"][..], "scope"),
        (&["investigation", "updated_by", "name"][..], "mallory"),
        (&["entries", "entries", "0", "payload", "text"][..], "note"),
        (&["entries", "entries", "0", "actor", "name"][..], "mallory"),
        (
            &["entries", "entries", "1", "payload", "title"][..],
            "retitled",
        ),
    ] {
        let at = path
            .iter()
            .fold(mirror, |at, key| match key.parse::<usize>() {
                Ok(index) => &at[index],
                Err(_) => &at[*key],
            });
        assert_eq!(at, &Json::String(mirrored(label)), "{path:?}");
    }
}

#[tokio::test]
async fn a_write_echo_and_its_activity_print_format_characters_escaped() {
    let other = Actor {
        prefix: "Zy9x8w7v".to_owned(),
        name: hostile("mallory"),
    };
    let backend = Fake {
        append_entry: answers(vec![
            appended(note(1, agent(), "mine"), None),
            appended(
                note(3, agent(), &hostile("echoed")),
                Some(activity(vec![note(2, other, &hostile("theirs"))], false)),
            ),
        ]),
        ..Fake::default()
    };
    let seen = Seen::default();
    let note_call = |text: &str| json!({"action": "note", "id": INVESTIGATION_ID, "text": text});
    let _ = call(&backend, &seen, note_call("mine")).await;
    let result = call(&backend, &seen, note_call(&hostile("echoed"))).await;
    assert_ok(&result);
    let text = text_of(&result);
    assert_no_raw_format_char(text);
    assert!(
        text.lines().any(
            |line| line.starts_with("entry written: #3 note by abcd1234")
                && line.ends_with(&printed("echoed"))
        ),
        "{text}"
    );
    assert!(
        text.lines().any(|line| line
            .starts_with(&format!("#2 note by Zy9x8w7v {} at", printed("mallory")))
            && line.ends_with(&printed("theirs"))),
        "{text}"
    );
    assert_mirror_escaped(&result);
    assert_eq!(
        block(&result, "entry")["payload"]["text"],
        Json::String(mirrored("echoed"))
    );
    assert_eq!(
        block(&result, "activity")["entries"][0]["payload"]["text"],
        Json::String(mirrored("theirs"))
    );
    assert_eq!(
        block(&result, "activity")["entries"][0]["actor"]["name"],
        Json::String(mirrored("mallory"))
    );
}

#[tokio::test]
async fn a_create_echo_prints_a_title_with_format_characters_escaped() {
    let backend = Fake {
        create_investigation: ok((
            true,
            Investigation {
                title: hostile("title"),
                question: hostile("question"),
                ..investigation(0)
            },
        )),
        ..Fake::default()
    };
    let result = call(
        &backend,
        &Seen::default(),
        json!({
            "action": "create",
            "title": hostile("title"),
            "question": hostile("question"),
        }),
    )
    .await;
    assert_ok(&result);
    let text = text_of(&result);
    assert_no_raw_format_char(text);
    assert!(
        text.lines()
            .any(|line| line == format!("question: {}", printed("question"))),
        "{text}"
    );
    assert_mirror_escaped(&result);
    assert_eq!(
        block(&result, "investigation")["title"],
        Json::String(mirrored("title"))
    );
}

/// The fitter counts a format character as the six bytes `\uXXXX` it
/// prints, so a cut keeps what the budget holds.
#[test]
fn the_fitter_counts_a_format_character_as_its_escape() {
    use trawl_mcp::render::activity::cut_escaped;
    let text = "\u{202e}\u{200b}\u{00ad}";
    assert_eq!(cut_escaped(text, 18), None);
    // Room for two escapes of the three: the cut keeps the first two
    // scalars (3 + 3 bytes) and leaves out the six bytes of the third.
    assert_eq!(cut_escaped(text, 12), Some((6, 6)));
    assert_eq!(cut_escaped(text, 11), Some((3, 12)));
}
