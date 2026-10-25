// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Two principals share one investigation through `trawl mcp` against a
//! real trawld (ADR-0059, #310).
//!
//! Principal A is an agent: the real `trawl mcp --profile agent` child
//! (`common::mcp`) under a key holding the recipe's permissions, `query`,
//! `schema_read`, `validate`, `investigation_read` and
//! `investigation_write`. Principal B is a second key of the same
//! permissions, driven through `trawl-client` the way the web UI's proxy
//! would drive trawld. Every call A makes is held to the MCP text
//! contract: one text block, and on success a mirror valid under the
//! tool's `outputSchema`.
//!
//! The session prints what B wrote exactly once: in the activity of A's
//! next write after it, or on a board A reads, and never again in that
//! session. A second session starts following where its first touch finds
//! the investigation. B's close refuses A's next write with the way to
//! reopen, and a write sent against a moved sequence prints both
//! sequences and the read that catches up.
//!
//! An attached search records a step whose result trawld keeps from
//! birth: B reads the result before A keeps any of its rows, and then
//! reads the step and its outcome.
//!
//! Without `DATABASE_URL` these tests fail rather than skip. The binary is
//! in the ADR-0021 `postgres` nextest group.

mod common;

use chrono::{SecondsFormat, TimeDelta, Utc};
use common::mcp::{Era, Mcp, home_as};
use common::{Options, Server};
use serde_json::{Value, json};
use trawl_client::{
    AppendEntry, EntryKind, EntryPayload, InvestigationId, InvestigationStatus, PatchInvestigation,
};

/// What the recipe in `reference/mcp.md` grants an agent that
/// investigates.
const INVESTIGATOR_PERMISSIONS: [&str; 5] = [
    "query",
    "schema_read",
    "validate",
    "investigation_read",
    "investigation_write",
];

/// The `checkout` events the search finds: one a second, newest at boot.
const EVENTS: usize = 40;

/// A server that accepts `prod`, with [`EVENTS`] of `checkout` in it,
/// compacted; and the tokens of principals A and B.
async fn server() -> (Server, String, String) {
    let server = Server::start_with(Options {
        envs: vec!["prod".to_owned()],
        ..Options::default()
    })
    .await;
    let now = Utc::now();
    let events: Vec<Value> = (0..EVENTS)
        .map(|i| {
            let at = now - TimeDelta::seconds(i64::try_from(i).unwrap());
            json!({
                "_time": at.to_rfc3339_opts(SecondsFormat::Micros, true),
                "service": "checkout",
                "env": "prod",
                "host": "web-1",
                "status": if i % 4 == 0 { 502 } else { 200 },
                "message": format!("request {i}"),
            })
        })
        .collect();
    server.ingest(&events).await;
    server.compact().await;
    let a = server
        .mint("investigator-a", &INVESTIGATOR_PERMISSIONS)
        .await;
    let b = server
        .mint("investigator-b", &INVESTIGATOR_PERMISSIONS)
        .await;
    (server, a, b)
}

/// The investigation id an `investigation` mirror names.
fn id_of(mirror: &Value) -> InvestigationId {
    InvestigationId::parse(mirror["investigation"]["id"].as_str().expect("an id"))
        .expect("a well-formed id")
}

/// B's note `text`, appended without a `seen_seq`; returns its seq.
async fn b_note(b: &trawl_client::HttpClient, id: &InvestigationId, text: &str) -> u64 {
    b.append_entry(
        id.as_str(),
        &AppendEntry::Note {
            text: text.to_owned(),
            seen_seq: None,
        },
    )
    .await
    .expect("B appends a note")
    .entry
    .seq
}

/// Two principals, one investigation: create, activity printed exactly
/// once, a board read, a second session's first touch, close and a stale
/// sequence, all through the real `trawl mcp` child.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // one shared investigation, each turn asserted in order
async fn two_principals_share_an_investigation_through_trawl_mcp() {
    let (server, token_a, token_b) = server().await;
    let home = home_as(&server, &token_a);
    let b = server.client(&token_b);
    let mut a = Mcp::spawn(home.path(), Era::Initialize);
    a.open().await;
    a.list_tools().await;

    // orient names the investigation permissions and lists no open
    // investigation yet.
    let orient = a.call("orient", json!({})).await;
    assert!(!orient.is_error, "{}", orient.text);
    let mirror = orient.structured.as_ref().unwrap();
    assert_eq!(
        mirror["key"]["permissions"],
        json!(INVESTIGATOR_PERMISSIONS),
        "{}",
        orient.text
    );
    assert!(
        mirror.get("open_investigations").is_none(),
        "{}",
        orient.text
    );

    // A creates; the minted id prints first.
    let created = a
        .call(
            "investigation",
            json!({"action": "create", "title": "502s on checkout",
                   "question": "Why does checkout return 502?", "scope": "prod"}),
        )
        .await;
    assert!(!created.is_error, "{}", created.text);
    let id = id_of(created.structured.as_ref().unwrap());
    assert!(
        created
            .text
            .starts_with(&format!("Created investigation \"{}\"", id.as_str())),
        "{}",
        created.text
    );

    // B reads A's investigation and writes into it.
    let board = b
        .read_investigation(id.as_str(), 0, None)
        .await
        .expect("B reads the investigation");
    assert_eq!(board.investigation.title, "502s on checkout");
    assert_eq!(board.investigation.created_by.name, "investigator-a-key");
    let b1 = b_note(&b, &id, "B: the pool limit is 10").await;

    // A's next write prints B's note in its activity.
    let first = a
        .call(
            "investigation",
            json!({"action": "note", "id": id.as_str(), "text": "A: checking the pool"}),
        )
        .await;
    assert!(!first.is_error, "{}", first.text);
    assert!(
        first.text.contains("activity by other keys since #0"),
        "{}",
        first.text
    );
    assert!(
        first.text.contains(&format!(
            "#{b1} note by {} \"investigator-b-key\"",
            board_prefix(&b, &id, b1).await
        )),
        "{}",
        first.text
    );
    assert!(
        first.text.contains("\"B: the pool limit is 10\""),
        "{}",
        first.text
    );
    let activity = &first.structured.as_ref().unwrap()["activity"];
    assert_eq!(activity["printed_through"], json!(b1), "{}", first.text);

    // and never again in this session.
    let second = a
        .call(
            "investigation",
            json!({"action": "note", "id": id.as_str(), "text": "A: pool at 10 of 10"}),
        )
        .await;
    assert!(!second.is_error, "{}", second.text);
    assert!(!second.text.contains("B: the pool"), "{}", second.text);
    assert!(
        second.text.contains("in change order: none."),
        "{}",
        second.text
    );

    // A board read prints B's next note; the write after it does not.
    let b2 = b_note(&b, &id, "B: the limit came from the 09:00 deploy").await;
    let read = a
        .call(
            "investigation",
            json!({"action": "read", "id": id.as_str()}),
        )
        .await;
    assert!(!read.is_error, "{}", read.text);
    for text in [
        "\"B: the pool limit is 10\"",
        "\"A: checking the pool\"",
        "\"A: pool at 10 of 10\"",
        "\"B: the limit came from the 09:00 deploy\"",
    ] {
        assert!(read.text.contains(text), "{text}: {}", read.text);
    }
    assert_eq!(
        read.structured.as_ref().unwrap()["entries"]["printed_through"],
        json!(b2),
        "{}",
        read.text
    );
    let third = a
        .call(
            "investigation",
            json!({"action": "finding", "id": id.as_str(), "statement": "the pool is exhausted",
                   "status": "hypothesis"}),
        )
        .await;
    assert!(!third.is_error, "{}", third.text);
    assert!(!third.text.contains("09:00 deploy"), "{}", third.text);
    let finding_seq = third.structured.as_ref().unwrap()["entry"]["seq"]
        .as_u64()
        .expect("the finding's seq");

    // The `read` tool reads the same board.
    let through_read = a
        .call(
            "read",
            json!({"action": "investigation", "investigation_id": id.as_str(), "since": b2}),
        )
        .await;
    assert!(!through_read.is_error, "{}", through_read.text);
    assert!(
        through_read.text.contains("\"the pool is exhausted\""),
        "{}",
        through_read.text
    );

    // B moves the finding; A's write against the old seq is refused with
    // both sequences and the read that catches up.
    let at = b
        .read_investigation(id.as_str(), 0, None)
        .await
        .unwrap()
        .investigation
        .seq;
    b.set_finding_status(
        id.as_str(),
        finding_seq,
        &trawl_client::FindingStatusUpdate {
            expected_seq: at,
            status: trawl_client::FindingStatus::Supported,
        },
    )
    .await
    .expect("B supports the finding");
    let stale = a
        .call(
            "investigation",
            json!({"action": "finding_status", "id": id.as_str(), "finding_seq": finding_seq,
                   "status": "refuted", "expected_seq": at}),
        )
        .await;
    assert!(stale.is_error, "{}", stale.raw);
    assert!(
        stale.text.contains("sequence_conflict (HTTP 409)"),
        "{}",
        stale.text
    );
    assert!(
        stale.text.contains(&format!(
            "this call expected seq {at}, and it is at seq {}",
            at + 1
        )),
        "{}",
        stale.text
    );
    assert!(
        stale.text.contains(&format!(
            "investigation action=read id={} since={at}",
            id.as_str()
        )),
        "{}",
        stale.text
    );
    // A close sent against the same old seq is refused the same way.
    let stale_close = a
        .call(
            "investigation",
            json!({"action": "status", "id": id.as_str(), "status": "closed",
                   "expected_seq": at}),
        )
        .await;
    assert!(stale_close.is_error, "{}", stale_close.raw);
    assert!(
        stale_close.text.contains(&format!(
            "this call expected seq {at}, and it is at seq {}. Read what changed with \
             investigation action=read id={} since={at}, then send expected_seq={} if the \
             change still holds.",
            at + 1,
            id.as_str(),
            at + 1
        )),
        "{}",
        stale_close.text
    );

    // An open investigation is listed by orient.
    let orient = a.call("orient", json!({})).await;
    assert!(!orient.is_error, "{}", orient.text);
    assert_eq!(
        orient.structured.as_ref().unwrap()["open_investigations"][0]["id"],
        json!(id.as_str()),
        "{}",
        orient.text
    );

    // B closes it; A's next write names the way to reopen.
    let at = b
        .read_investigation(id.as_str(), 0, None)
        .await
        .unwrap()
        .investigation
        .seq;
    let closed = b
        .patch_investigation(
            id.as_str(),
            &PatchInvestigation {
                expected_seq: at,
                title: None,
                question: None,
                status: Some(InvestigationStatus::Closed),
            },
        )
        .await
        .expect("B closes the investigation");
    assert_eq!(closed.status, InvestigationStatus::Closed);
    let refused = a
        .call(
            "investigation",
            json!({"action": "note", "id": id.as_str(), "text": "A: one more"}),
        )
        .await;
    assert!(refused.is_error, "{}", refused.raw);
    assert!(
        refused.text.contains("investigation_closed (HTTP 409)"),
        "{}",
        refused.text
    );
    assert!(
        refused.text.contains(&format!(
            "investigation action=status id={} status=open",
            id.as_str()
        )),
        "{}",
        refused.text
    );
    let reopened = a
        .call(
            "investigation",
            json!({"action": "status", "id": id.as_str(), "status": "open",
                   "expected_seq": closed.seq}),
        )
        .await;
    assert!(!reopened.is_error, "{}", reopened.text);
    a.finish().await.assert_clean("session one");

    // A second session starts following at its first touch: B's notes are
    // on the board, not in its activity.
    let b3 = b_note(&b, &id, "B: before the second session").await;
    let mut again = Mcp::spawn(home.path(), Era::Discover);
    again.open().await;
    again.list_tools().await;
    let touch = again
        .call(
            "investigation",
            json!({"action": "note", "id": id.as_str(), "text": "A: back again"}),
        )
        .await;
    assert!(!touch.is_error, "{}", touch.text);
    assert!(
        touch
            .text
            .contains("this session starts following this investigation at #"),
        "{}",
        touch.text
    );
    assert!(
        !touch.text.contains("before the second session"),
        "{}",
        touch.text
    );
    let b4 = b_note(&b, &id, "B: after the first touch").await;
    let next = again
        .call(
            "investigation",
            json!({"action": "note", "id": id.as_str(), "text": "A: and again"}),
        )
        .await;
    assert!(!next.is_error, "{}", next.text);
    assert!(
        next.text.contains("\"B: after the first touch\""),
        "{}",
        next.text
    );
    assert!(
        !next.text.contains("before the second session"),
        "{}",
        next.text
    );
    assert!(b4 > b3);
    again.finish().await.assert_clean("session two");

    // trawld recorded every entry once, each under the key that wrote it.
    let log = b.read_investigation(id.as_str(), 0, None).await.unwrap();
    let mine: Vec<&str> = log
        .entries
        .iter()
        .filter(|entry| entry.actor.name == "investigator-a-key")
        .filter_map(|entry| match &entry.payload {
            EntryPayload::Note { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        mine,
        [
            "A: checking the pool",
            "A: pool at 10 of 10",
            "A: back again",
            "A: and again"
        ]
    );
    server.stop().await;
}

/// More activity than one answer carries: trawld sends at most 50 entries
/// with `more`, the reply prints what fits and proposes the read that
/// continues, and following it prints the rest; every entry B wrote is
/// printed exactly once in the session (#310 AC7).
#[tokio::test(flavor = "multi_thread")]
async fn activity_past_one_answer_continues_and_prints_every_entry_once() {
    const NOTES: usize = 60;
    let (server, token_a, token_b) = server().await;
    let home = home_as(&server, &token_a);
    let b = server.client(&token_b);
    let mut a = Mcp::spawn(home.path(), Era::Discover);
    a.open().await;
    a.list_tools().await;
    let created = a
        .call(
            "investigation",
            json!({"action": "create", "title": "busy", "question": "Who wrote what?"}),
        )
        .await;
    assert!(!created.is_error, "{}", created.text);
    let id = id_of(created.structured.as_ref().unwrap());
    let marker = |i: usize| format!("B note {i:02} of {NOTES}");
    for i in 0..NOTES {
        b_note(&b, &id, &marker(i)).await;
    }

    let mut texts = Vec::new();
    let mut call = a
        .call(
            "investigation",
            json!({"action": "note", "id": id.as_str(), "text": "A: catching up"}),
        )
        .await;
    let mut hops = 0;
    loop {
        assert!(!call.is_error, "{}", call.text);
        texts.push(call.text.clone());
        let mirror = call.structured.clone().unwrap();
        let view = if mirror["activity"].is_object() {
            mirror["activity"].clone()
        } else {
            mirror["entries"].clone()
        };
        let Some(next) = view.get("continuation").cloned() else {
            break;
        };
        assert_eq!(next["tool"], "investigation", "{next}");
        hops += 1;
        assert!(hops <= NOTES, "the continuation never ends");
        call = a.call("investigation", next["arguments"].clone()).await;
    }
    assert!(hops >= 1, "60 entries fit no single answer: {texts:?}");
    let all = texts.join("\n");
    for i in 0..NOTES {
        let quoted = format!("\"{}\"", marker(i));
        assert_eq!(
            all.matches(&quoted).count(),
            1,
            "{quoted} printed once across {hops} continuations"
        );
    }
    let after = a
        .call(
            "investigation",
            json!({"action": "note", "id": id.as_str(), "text": "A: caught up"}),
        )
        .await;
    assert!(!after.is_error, "{}", after.text);
    assert!(!after.text.contains("B note "), "{}", after.text);
    a.finish().await.assert_clean("overflow");
    server.stop().await;
}

/// A key whose role is unassigned between two appends is refused the
/// second as a 403 naming the permissions, and the board still names it
/// by the `{prefix, name}` snapshot its first append recorded (#310 AC2).
#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_role_refuses_the_next_append_and_the_board_keeps_the_snapshot() {
    let (server, token_a, token_b) = server().await;
    let home = home_as(&server, &token_a);
    let b = server.client(&token_b);
    let prefix = server
        .client(&token_a)
        .whoami()
        .await
        .expect("A's key")
        .prefix;
    let mut a = Mcp::spawn(home.path(), Era::Initialize);
    a.open().await;
    a.list_tools().await;
    let created = a
        .call(
            "investigation",
            json!({"action": "create", "title": "revoked", "question": "Does the name stay?"}),
        )
        .await;
    assert!(!created.is_error, "{}", created.text);
    let id = id_of(created.structured.as_ref().unwrap());
    let first = a
        .call(
            "investigation",
            json!({"action": "note", "id": id.as_str(), "text": "A: before the revoke"}),
        )
        .await;
    assert!(!first.is_error, "{}", first.text);

    server.unassign(&prefix, "investigator-a").await;
    let refused = a
        .call(
            "investigation",
            json!({"action": "note", "id": id.as_str(), "text": "A: after the revoke"}),
        )
        .await;
    assert!(refused.is_error, "{}", refused.raw);
    assert!(
        refused.text.contains("forbidden (HTTP 403)")
            && refused.text.contains(
                "investigation action=note needs the query, investigation_read and \
                 investigation_write permissions"
            ),
        "{}",
        refused.text
    );
    a.finish().await.assert_clean("revoked");

    let log = b.read_investigation(id.as_str(), 0, None).await.unwrap();
    let notes: Vec<_> = log
        .entries
        .iter()
        .filter(|entry| matches!(entry.payload, EntryPayload::Note { .. }))
        .collect();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0].actor.prefix, prefix);
    assert_eq!(notes[0].actor.name, "investigator-a-key");
    assert_eq!(log.investigation.created_by.prefix, prefix);
    server.stop().await;
}

/// The key prefix trawld records for the writer of entry `seq`.
async fn board_prefix(b: &trawl_client::HttpClient, id: &InvestigationId, seq: u64) -> String {
    b.read_investigation(id.as_str(), 0, None)
        .await
        .unwrap()
        .entries
        .into_iter()
        .find(|entry| entry.seq == seq)
        .expect("the entry")
        .actor
        .prefix
}

/// An attached search records a step and keeps its result from birth: A
/// keeps rows of it, and B reads the step, its outcome and the result.
#[tokio::test(flavor = "multi_thread")]
async fn an_attached_search_is_a_step_whose_result_both_principals_read() {
    let (server, token_a, token_b) = server().await;
    let home = home_as(&server, &token_a);
    let b = server.client(&token_b);
    let mut a = Mcp::spawn(home.path(), Era::Initialize);
    a.open().await;
    a.list_tools().await;

    let created = a
        .call(
            "investigation",
            json!({"action": "create", "title": "502s", "question": "Which requests fail?"}),
        )
        .await;
    assert!(!created.is_error, "{}", created.text);
    let id = id_of(created.structured.as_ref().unwrap());
    let b1 = b_note(&b, &id, "B: start from the 502s").await;

    let search = a
        .call(
            "search",
            json!({"mode": "execute", "query": "last=1h service=checkout status=502",
                   "investigation": id.as_str(), "purpose": "find the failing requests"}),
        )
        .await;
    assert!(!search.is_error, "{}", search.text);
    let mirror = search.structured.as_ref().unwrap();
    let result_id = mirror["result"]["result_id"].as_str().unwrap().to_owned();
    assert_eq!(
        mirror["investigation"]["id"],
        json!(id.as_str()),
        "{}",
        search.text
    );
    assert_eq!(
        mirror["investigation"]["result_id"],
        json!(result_id),
        "{}",
        search.text
    );
    assert!(
        search.text.contains("\"B: start from the 502s\""),
        "{}",
        search.text
    );
    assert_eq!(
        mirror["investigation"]["activity"]["printed_through"],
        json!(b1),
        "{}",
        search.text
    );

    // The result is kept from birth: B, another key, reads it before any
    // keep.
    let page = b
        .result_page(&result_id, 0, 50, None)
        .await
        .expect("B reads the kept result");
    assert_eq!(page.page.rows.len(), EVENTS / 4);
    assert_eq!(
        page.result
            .investigation_id
            .as_ref()
            .map(InvestigationId::as_str),
        Some(id.as_str())
    );

    let kept = a
        .call(
            "investigation",
            json!({"action": "keep", "id": id.as_str(), "result_id": result_id, "rows": [0, 1],
                   "reason": "the first two 502s"}),
        )
        .await;
    assert!(!kept.is_error, "{}", kept.text);
    assert!(!kept.text.contains("start from the 502s"), "{}", kept.text);
    a.finish().await.assert_clean("attached");

    let log = b.read_investigation(id.as_str(), 0, None).await.unwrap();
    let step = log
        .entries
        .iter()
        .find(|entry| entry.payload.kind() == EntryKind::Step)
        .expect("the search is a step");
    assert_eq!(step.actor.name, "investigator-a-key");
    match &step.payload {
        EntryPayload::Step {
            purpose, outcome, ..
        } => {
            assert_eq!(purpose.as_deref(), Some("find the failing requests"));
            let outcome = outcome.as_ref().expect("the step's outcome");
            assert_eq!(outcome.result_id.as_deref(), Some(result_id.as_str()));
            assert_eq!(outcome.rows, Some(u64::try_from(EVENTS / 4).unwrap()));
        }
        other => panic!("not a step: {other:?}"),
    }
    server.stop().await;
}
