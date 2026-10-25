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
