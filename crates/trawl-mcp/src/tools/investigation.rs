// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `investigation`: open, read, list and write to an investigation that
//! other keys read too (ADR-0059).
//!
//! One flat input: `action` picks the call, and each field names the
//! actions that read it. A field the action does not read, a missing one
//! and one past its trawl-api bound are refused by name before any
//! request, so a malformed call costs no request against the key's rate.
//!
//! - `create` sends `PUT /api/v1/investigations/{id}`, create if absent.
//!   Without an `id` the server here mints one from 16 random bytes as 32
//!   lowercase hex digits, the way trawld mints result ids, and the reply
//!   prints it first; the model never invents one. The same `id` and
//!   fields again return the same investigation, which is how a create
//!   whose answer was lost is recovered, and a failed create prints the id
//!   it sent for that reason.
//! - `read` sends `GET …?since=&limit=` and prints the board
//!   ([`crate::render::activity`]), sharing its renderer with `read`'s
//!   `action: investigation`.
//! - `list` sends `GET /api/v1/investigations`.
//! - `note`, `finding` and `keep` send `POST …/entries` with the session's
//!   `seen_seq` ([`crate::session`]); the reply prints the entry written
//!   and the activity other keys wrote since, each entry once per session.
//! - `finding_status` sends `PUT …/findings/{seq}/status` and `status`
//!   sends `PATCH …` with `status` alone, the one field per request trawld
//!   takes; both carry `expected_seq`, and a stale one prints trawld's
//!   expected and current sequence. Neither route answers activity.
//!
//! Changing the title or question, deleting an investigation and
//! extending its evidence are not actions here: deletion and extension
//! need `server_manage`, an operator's permission. Nothing here retries a
//! refused call.

use rmcp::model::CallToolResult;
use trawl_api::investigations::{
    FINDING_MAX_COUNTEREVIDENCE, FINDING_MAX_REFERENCES, FINDING_STATEMENT_MAX_BYTES,
    InvestigationId, KEEP_MAX_ROWS, KEEP_REASON_MAX_BYTES, NOTE_MAX_BYTES, QUESTION_MAX_BYTES,
    REMAINING_QUESTION_MAX_BYTES, SCOPE_MAX_BYTES, TITLE_MAX_BYTES,
};
use trawl_client::{
    AppendEntry, ClientError, CreateInvestigation, ErrorCode, EvidenceRef, FindingStatus,
    FindingStatusUpdate, InvestigationStatus, PatchInvestigation,
};

use super::schema::{INVESTIGATION, InvestigationInput, RowRef, StatusWord};
use crate::backend::Backend;
use crate::render::activity::{
    self, Continue, activity_view, board_through, board_view, entry_view, investigation_view,
    printed_through, summary_view,
};
use crate::render::refusal::{self, error_result};
use crate::render::rows::json_string;
use crate::render::{Budget, one_line};
use crate::reply::{ActivityView, InvestigationAction, InvestigationReply, InvestigationView};
use crate::session::{Seen, SeenGuard};

/// The most entries one board read asks trawld for: as many as trawld's
/// activity allowance, so a board page and an activity block print alike.
/// The response budget usually prints fewer, and the continuation reads
/// on from the last one printed.
pub const BOARD_PAGE_ENTRIES: u64 = 50;

/// The note when trawld answers a write with no activity.
pub const NO_ACTIVITY_ROUTE: &str = "trawld answers this write with no activity; the next note, \
finding, keep or attached search prints what other keys wrote";

/// Run `investigation` against `backend`, keeping this session's
/// bookkeeping in `seen`.
pub async fn run<B: Backend>(
    backend: &B,
    seen: &Seen,
    input: InvestigationInput,
) -> CallToolResult {
    let budget = Budget::new(input.max_bytes);
    if let Err(refused) = check(&input) {
        return refused;
    }
    match input.action {
        InvestigationAction::Create => create(backend, seen, &input, budget).await,
        InvestigationAction::Read => read(backend, seen, &input, budget).await,
        InvestigationAction::List => list(backend, budget).await,
        InvestigationAction::Note | InvestigationAction::Finding | InvestigationAction::Keep => {
            append(backend, seen, &input, budget).await
        }
        InvestigationAction::FindingStatus => finding_status(backend, seen, &input, budget).await,
        InvestigationAction::Status => status(backend, seen, &input, budget).await,
    }
}

// -- input checks ---------------------------------------------------------------

/// The action's wire word.
fn action_word(action: InvestigationAction) -> &'static str {
    match action {
        InvestigationAction::Create => "create",
        InvestigationAction::Read => "read",
        InvestigationAction::List => "list",
        InvestigationAction::Note => "note",
        InvestigationAction::Finding => "finding",
        InvestigationAction::Keep => "keep",
        InvestigationAction::FindingStatus => "finding_status",
        InvestigationAction::Status => "status",
    }
}

/// The fields `action` reads besides `action` and `max_bytes`, and the
/// ones it needs.
fn takes(action: InvestigationAction) -> (&'static [&'static str], &'static [&'static str]) {
    match action {
        InvestigationAction::Create => (
            &["id", "title", "question", "scope"],
            &["title", "question"],
        ),
        InvestigationAction::Read => (&["id", "since"], &["id"]),
        InvestigationAction::List => (&[], &[]),
        InvestigationAction::Note => (&["id", "text"], &["id", "text"]),
        InvestigationAction::Finding => (
            &[
                "id",
                "statement",
                "status",
                "references",
                "counterevidence",
                "remaining_question",
            ],
            &["id", "statement", "status"],
        ),
        InvestigationAction::Keep => (
            &["id", "result_id", "rows", "reason"],
            &["id", "result_id", "rows", "reason"],
        ),
        InvestigationAction::FindingStatus => (
            &["id", "finding_seq", "status", "expected_seq"],
            &["id", "finding_seq", "status", "expected_seq"],
        ),
        InvestigationAction::Status => (
            &["id", "status", "expected_seq"],
            &["id", "status", "expected_seq"],
        ),
    }
}

/// The optional fields the call gave, by name.
fn given(input: &InvestigationInput) -> Vec<&'static str> {
    [
        ("id", input.id.is_some()),
        ("title", input.title.is_some()),
        ("question", input.question.is_some()),
        ("scope", input.scope.is_some()),
        ("text", input.text.is_some()),
        ("statement", input.statement.is_some()),
        ("status", input.status.is_some()),
        ("references", input.references.is_some()),
        ("counterevidence", input.counterevidence.is_some()),
        ("remaining_question", input.remaining_question.is_some()),
        ("result_id", input.result_id.is_some()),
        ("rows", input.rows.is_some()),
        ("reason", input.reason.is_some()),
        ("finding_seq", input.finding_seq.is_some()),
        ("expected_seq", input.expected_seq.is_some()),
        ("since", input.since.is_some()),
    ]
    .into_iter()
    .filter_map(|(name, present)| present.then_some(name))
    .collect()
}

/// Refuse a call whose fields do not fit its action, or whose values are
/// out of their bounds, before any request.
fn check(input: &InvestigationInput) -> Result<(), CallToolResult> {
    let action = action_word(input.action);
    let (allowed, needed) = takes(input.action);
    let given = given(input);
    let reads = if allowed.is_empty() {
        "it reads only max_bytes".to_owned()
    } else {
        format!("it reads {} and max_bytes", allowed.join(", "))
    };
    if let Some(stray) = given.iter().find(|name| !allowed.contains(name)) {
        return Err(error_result(format!(
            "investigation action={action} does not take {stray}; {reads}."
        )));
    }
    let missing: Vec<&str> = needed
        .iter()
        .copied()
        .filter(|name| !given.contains(name))
        .collect();
    if !missing.is_empty() {
        return Err(error_result(format!(
            "investigation action={action} needs {}; {reads}.",
            missing.join(", ")
        )));
    }
    if let Some(id) = &input.id
        && InvestigationId::parse(id).is_err()
    {
        return Err(error_result(format!(
            "investigation action={action}: id is 32 lowercase hex digits, the id create \
             printed{}.",
            if input.action == InvestigationAction::Create {
                ", or leave it out to mint one"
            } else {
                ""
            }
        )));
    }
    for (name, text, max) in [
        ("title", &input.title, TITLE_MAX_BYTES),
        ("question", &input.question, QUESTION_MAX_BYTES),
        ("scope", &input.scope, SCOPE_MAX_BYTES),
        ("text", &input.text, NOTE_MAX_BYTES),
        ("statement", &input.statement, FINDING_STATEMENT_MAX_BYTES),
        (
            "remaining_question",
            &input.remaining_question,
            REMAINING_QUESTION_MAX_BYTES,
        ),
        ("reason", &input.reason, KEEP_REASON_MAX_BYTES),
    ] {
        if let Some(text) = text
            && text.len() > max
        {
            return Err(error_result(format!(
                "investigation action={action}: {name} is {} bytes; it may hold at most {max}.",
                text.len()
            )));
        }
    }
    for (name, count, max) in [
        (
            "references",
            input.references.as_ref().map(Vec::len),
            FINDING_MAX_REFERENCES,
        ),
        (
            "counterevidence",
            input.counterevidence.as_ref().map(Vec::len),
            FINDING_MAX_COUNTEREVIDENCE,
        ),
        ("rows", input.rows.as_ref().map(Vec::len), KEEP_MAX_ROWS),
    ] {
        if let Some(count) = count
            && count > max
        {
            return Err(error_result(format!(
                "investigation action={action}: {name} lists {count}; it may list at most {max}."
            )));
        }
    }
    if let Some(status) = input.status {
        let fits = match input.action {
            InvestigationAction::Status => investigation_status(status).is_some(),
            _ => finding_status_of(status).is_some(),
        };
        if !fits {
            let words = if input.action == InvestigationAction::Status {
                "open or closed"
            } else {
                "hypothesis, supported, refuted or unresolved"
            };
            return Err(error_result(format!(
                "investigation action={action}: status is {words}."
            )));
        }
    }
    Ok(())
}

/// A finding's status, for a word that names one.
fn finding_status_of(word: StatusWord) -> Option<FindingStatus> {
    match word {
        StatusWord::Hypothesis => Some(FindingStatus::Hypothesis),
        StatusWord::Supported => Some(FindingStatus::Supported),
        StatusWord::Refuted => Some(FindingStatus::Refuted),
        StatusWord::Unresolved => Some(FindingStatus::Unresolved),
        StatusWord::Open | StatusWord::Closed => None,
    }
}

/// An investigation's status, for a word that names one.
fn investigation_status(word: StatusWord) -> Option<InvestigationStatus> {
    match word {
        StatusWord::Open => Some(InvestigationStatus::Open),
        StatusWord::Closed => Some(InvestigationStatus::Closed),
        _ => None,
    }
}

/// The call's checked id. [`check`] has refused a call that needs one and
/// lacks it, or sent a malformed one.
fn id_of(input: &InvestigationInput) -> InvestigationId {
    input
        .id
        .as_deref()
        .and_then(|id| InvestigationId::parse(id).ok())
        .unwrap_or_else(mint)
}

/// A fresh investigation id: 128 random bits as 32 lowercase hex digits,
/// the way trawld mints result ids.
#[must_use]
pub fn mint() -> InvestigationId {
    use std::fmt::Write as _;
    let bytes: [u8; 16] = rand::random();
    let mut id = String::with_capacity(InvestigationId::LEN);
    for byte in bytes {
        let _ = write!(id, "{byte:02x}");
    }
    InvestigationId::parse(&id).expect("32 lowercase hex digits")
}

fn refs(rows: Option<&Vec<RowRef>>) -> Vec<EvidenceRef> {
    rows.map(|rows| {
        rows.iter()
            .map(|row| EvidenceRef {
                result_id: row.result_id.clone(),
                row: row.row,
            })
            .collect()
    })
    .unwrap_or_default()
}

// -- refusals -------------------------------------------------------------------

/// The permissions `action` needs, in words.
fn needs(action: InvestigationAction) -> &'static str {
    match action {
        InvestigationAction::Read | InvestigationAction::List => "query and investigation_read",
        _ => "query, investigation_read and investigation_write",
    }
}

/// The error a refused or failed call answers: trawld's code and message,
/// then what it means for this action and the call that is the way back.
/// It retries nothing.
pub(crate) fn refused(
    action: InvestigationAction,
    id: Option<&InvestigationId>,
    error: &ClientError,
) -> CallToolResult {
    let caller = format!("investigation action={}", action_word(action));
    let advice = advice(&caller, needs(action), action, id, error);
    refusal::refusal_with(INVESTIGATION, error, None, &advice)
}

/// What a refusal of `caller`'s call on investigation `id` means, and the
/// call that is the way back: `needs` names the permissions it takes, and
/// `action` the investigation action it amounts to (`search` attached to
/// an investigation is a write).
pub(crate) fn advice(
    caller: &str,
    needs: &str,
    action: InvestigationAction,
    id: Option<&InvestigationId>,
    error: &ClientError,
) -> Vec<String> {
    let mut advice = Vec::new();
    let named = id.map_or_else(
        || "the investigation".to_owned(),
        |id| json_string(id.as_str()),
    );
    let id_text = id.map_or("<id>", InvestigationId::as_str);
    if let ClientError::Server {
        status,
        error: envelope,
        ..
    } = error
    {
        match (status, &envelope.code) {
            (403, _) => advice.push(format!(
                "{caller} needs the {needs} permissions; orient lists this key's."
            )),
            (404, _) => advice.push(format!(
                "trawld holds no {} by that id; investigation action=list lists them.",
                if action == InvestigationAction::FindingStatus {
                    "investigation or finding"
                } else {
                    "investigation"
                }
            )),
            (_, ErrorCode::SequenceConflict) => match envelope.conflict {
                Some(conflict) => advice.push(format!(
                    "{named} moved: this call expected seq {}, and it is at seq {}. Read what \
                     changed with investigation action=read id={id_text} since={}, then send \
                     expected_seq={} if the change still holds.",
                    conflict.expected, conflict.current, conflict.expected, conflict.current
                )),
                None => advice.push(format!(
                    "{named} moved since the seq this call expected; read it with \
                     investigation action=read id={id_text} for its current seq."
                )),
            },
            (_, ErrorCode::InvestigationExists) => advice.push(format!(
                "{named} exists with another creator or other fields. Read it with \
                 investigation action=read id={id_text}, or create without an id to mint a new \
                 one."
            )),
            (_, ErrorCode::InvestigationClosed) => advice.push(format!(
                "{named} is closed and takes nothing new until a writer reopens it with \
                 investigation action=status id={id_text} status=open and its current seq as \
                 expected_seq."
            )),
            (_, ErrorCode::EvidenceBudgetExceeded) => advice.push(
                "Evidence frees only when it ages out or an operator with server_manage \
                 deletes an investigation; waiting does not help."
                    .to_owned(),
            ),
            _ => {}
        }
    }
    advice
}

// -- actions --------------------------------------------------------------------

/// `create`.
async fn create<B: Backend>(
    backend: &B,
    seen: &Seen,
    input: &InvestigationInput,
    budget: Budget,
) -> CallToolResult {
    let minted = input.id.is_none();
    let id = id_of(input);
    let request = CreateInvestigation {
        title: input.title.clone().unwrap_or_default(),
        question: input.question.clone().unwrap_or_default(),
        scope: input.scope.clone().unwrap_or_default(),
    };
    let mut guard = seen.lock().await;
    let (created, investigation) = match backend.create_investigation(id.as_str(), &request).await {
        Ok(answer) => answer,
        Err(error) => {
            let mut result = refused(InvestigationAction::Create, Some(&id), &error);
            if !matches!(
                &error,
                ClientError::Server {
                    status: 400 | 403,
                    ..
                }
            ) {
                result = with_recovery(&result, &id);
            }
            return result;
        }
    };
    let outcome = match (created, minted) {
        (true, true) => format!(
            "Created investigation {}, an id this server minted: send it as id from now on.",
            json_string(id.as_str())
        ),
        (true, false) => format!("Created investigation {}.", json_string(id.as_str())),
        (false, _) => format!(
            "Investigation {} already exists with this creator and these fields; this is it.",
            json_string(id.as_str())
        ),
    };
    let reply = InvestigationReply {
        outcome,
        action: InvestigationAction::Create,
        investigation: Some(investigation_view(&investigation)),
        entry: None,
        entries: None,
        activity: None,
        activity_note: None,
        investigations: Vec::new(),
        investigations_dropped_to_fit: 0,
        next_actions: Vec::new(),
    };
    match acknowledged(reply, budget, &id) {
        Ok(result) => {
            guard.touch(&id, investigation.seq);
            result
        }
        Err(unfitted) => with_recovery(&unfitted, &id),
    }
}

/// The reply to a write trawld accepted on investigation `id`, fitted
/// under `budget`; or, when not even its acknowledgement fits, an error
/// that still opens with it. The caller moves the session's bookkeeping
/// only on `Ok`.
fn acknowledged(
    reply: InvestigationReply,
    budget: Budget,
    id: &InvestigationId,
) -> Result<CallToolResult, CallToolResult> {
    let outcome = one_line(&reply.outcome);
    activity::fit_written(reply, budget, id.as_str()).ok_or_else(|| {
        error_result(format!(
            "{outcome}\n{}",
            activity::does_not_fit(INVESTIGATION, budget)
        ))
    })
}

/// `result`, a failed create's error, with the line that recovers it: a
/// create whose answer was lost may have created the investigation.
fn with_recovery(result: &CallToolResult, id: &InvestigationId) -> CallToolResult {
    let text = result
        .content
        .first()
        .and_then(|block| block.as_text())
        .map(|text| text.text.clone())
        .unwrap_or_default();
    error_result(format!(
        "{text}\nThis call sent id {}: send the same create with id={} to recover the \
         investigation if trawld created it.",
        json_string(id.as_str()),
        id.as_str()
    ))
}

/// The board of investigation `id` after `since`, read for `tool`, and the
/// investigation, with the guard that will record what the reply prints;
/// or the error to answer.
pub(crate) async fn board<'a, B: Backend>(
    backend: &B,
    seen: &'a Seen,
    id: &InvestigationId,
    since: u64,
    tool: Continue,
    action: InvestigationAction,
) -> Result<(InvestigationView, ActivityView, String, SeenGuard<'a>), CallToolResult> {
    let guard = seen.lock().await;
    let read = backend
        .read_investigation(id.as_str(), since, Some(BOARD_PAGE_ENTRIES))
        .await
        .map_err(|error| refused(action, Some(id), &error))?;
    let view = investigation_view(&read.investigation);
    let outcome = format!(
        "Investigation {} is {} at seq {}.",
        json_string(id.as_str()),
        view.status,
        view.seq
    );
    Ok((
        view,
        board_view(id.as_str(), since, &read, tool),
        outcome,
        guard,
    ))
}

/// `read`.
async fn read<B: Backend>(
    backend: &B,
    seen: &Seen,
    input: &InvestigationInput,
    budget: Budget,
) -> CallToolResult {
    let id = id_of(input);
    let since = input.since.unwrap_or(0);
    let (view, entries, outcome, mut guard) = match board(
        backend,
        seen,
        &id,
        since,
        Continue::Investigation,
        InvestigationAction::Read,
    )
    .await
    {
        Ok(board) => board,
        Err(refused) => return refused,
    };
    let reply = InvestigationReply {
        outcome,
        action: InvestigationAction::Read,
        investigation: Some(view),
        entry: None,
        entries: Some(entries),
        activity: None,
        activity_note: None,
        investigations: Vec::new(),
        investigations_dropped_to_fit: 0,
        next_actions: Vec::new(),
    };
    let result = activity::fit_entries(reply, budget, INVESTIGATION);
    guard.printed(&id, since, board_through(&result, &["entries"]));
    result
}

/// `list`.
async fn list<B: Backend>(backend: &B, budget: Budget) -> CallToolResult {
    let list = match backend.list_investigations().await {
        Ok(list) => list,
        Err(error) => return refused(InvestigationAction::List, None, &error),
    };
    let open = list
        .investigations
        .iter()
        .filter(|row| row.status == InvestigationStatus::Open)
        .count();
    let total = list.investigations.len();
    let reply = InvestigationReply {
        outcome: format!(
            "{total} {}, {open} open.",
            if total == 1 {
                "investigation"
            } else {
                "investigations"
            }
        ),
        action: InvestigationAction::List,
        investigation: None,
        entry: None,
        entries: None,
        activity: None,
        activity_note: None,
        investigations: list.investigations.iter().map(summary_view).collect(),
        investigations_dropped_to_fit: 0,
        next_actions: Vec::new(),
    };
    activity::fit_entries(reply, budget, INVESTIGATION)
}

/// The append `input` asks for, carrying `seen_seq`.
fn append_entry(input: &InvestigationInput, seen_seq: Option<u64>) -> AppendEntry {
    match input.action {
        InvestigationAction::Keep => AppendEntry::Keep {
            result_id: input.result_id.clone().unwrap_or_default(),
            rows: input.rows.clone().unwrap_or_default(),
            reason: input.reason.clone().unwrap_or_default(),
            seen_seq,
        },
        InvestigationAction::Finding => AppendEntry::Finding {
            statement: input.statement.clone().unwrap_or_default(),
            status: input
                .status
                .and_then(finding_status_of)
                .unwrap_or(FindingStatus::Hypothesis),
            references: refs(input.references.as_ref()),
            counterevidence: refs(input.counterevidence.as_ref()),
            remaining_question: input.remaining_question.clone(),
            seen_seq,
        },
        _ => AppendEntry::Note {
            text: input.text.clone().unwrap_or_default(),
            seen_seq,
        },
    }
}

/// The note when this session first touches an investigation at `seq`.
fn first_touch(id: &InvestigationId, seq: u64) -> String {
    format!(
        "this session starts following this investigation at #{seq}; what other keys wrote \
         before it is on the board, which investigation action=read id={} since=0 prints",
        id.as_str()
    )
}

/// `note`, `finding` and `keep`.
async fn append<B: Backend>(
    backend: &B,
    seen: &Seen,
    input: &InvestigationInput,
    budget: Budget,
) -> CallToolResult {
    let id = id_of(input);
    let mut guard = seen.lock().await;
    let seen_seq = guard.get(&id);
    let answer = match backend
        .append_entry(id.as_str(), &append_entry(input, seen_seq))
        .await
    {
        Ok(answer) => answer,
        Err(error) => return refused(input.action, Some(&id), &error),
    };
    let entry = entry_view(&answer.entry);
    let (activity, activity_note) = match (seen_seq, &answer.activity) {
        (None, _) => (None, Some(first_touch(&id, answer.entry.updated_seq))),
        (Some(since), Some(activity)) => (Some(activity_view(id.as_str(), since, activity)), None),
        (Some(_), None) => (
            None,
            Some("trawld sent no activity with this write".to_owned()),
        ),
    };
    let reply = InvestigationReply {
        outcome: format!(
            "Wrote {} #{} to investigation {}.",
            one_line(&entry.kind),
            entry.seq,
            json_string(id.as_str())
        ),
        action: input.action,
        investigation: None,
        entry: Some(entry),
        entries: None,
        activity,
        activity_note,
        investigations: Vec::new(),
        investigations_dropped_to_fit: 0,
        next_actions: Vec::new(),
    };
    let result = match acknowledged(reply, budget, &id) {
        Ok(result) => result,
        Err(unfitted) => return unfitted,
    };
    match seen_seq {
        None => guard.touch(&id, answer.entry.updated_seq),
        Some(since) => guard.printed(&id, since, printed_through(&result, &["activity"])),
    }
    result
}

/// `finding_status`.
async fn finding_status<B: Backend>(
    backend: &B,
    seen: &Seen,
    input: &InvestigationInput,
    budget: Budget,
) -> CallToolResult {
    let id = id_of(input);
    let finding_seq = input.finding_seq.unwrap_or_default();
    let request = FindingStatusUpdate {
        expected_seq: input.expected_seq.unwrap_or_default(),
        status: input
            .status
            .and_then(finding_status_of)
            .unwrap_or(FindingStatus::Hypothesis),
    };
    let mut guard = seen.lock().await;
    let answer = match backend
        .set_finding_status(id.as_str(), finding_seq, &request)
        .await
    {
        Ok(answer) => answer,
        Err(error) => return refused(InvestigationAction::FindingStatus, Some(&id), &error),
    };
    let entry = entry_view(&answer.entry);
    let reply = InvestigationReply {
        outcome: format!(
            "Finding #{finding_seq} of investigation {} is now {}; the investigation is at seq {}.",
            json_string(id.as_str()),
            word_of(request.status),
            entry.seq
        ),
        action: InvestigationAction::FindingStatus,
        investigation: None,
        entry: Some(entry),
        entries: None,
        activity: None,
        activity_note: Some(NO_ACTIVITY_ROUTE.to_owned()),
        investigations: Vec::new(),
        investigations_dropped_to_fit: 0,
        next_actions: Vec::new(),
    };
    let result = match acknowledged(reply, budget, &id) {
        Ok(result) => result,
        Err(unfitted) => return unfitted,
    };
    guard.touch(&id, answer.entry.updated_seq);
    result
}

fn word_of(status: FindingStatus) -> &'static str {
    match status {
        FindingStatus::Hypothesis => "hypothesis",
        FindingStatus::Supported => "supported",
        FindingStatus::Refuted => "refuted",
        FindingStatus::Unresolved => "unresolved",
    }
}

/// `status`: close or reopen, the one field this `PATCH` sends.
async fn status<B: Backend>(
    backend: &B,
    seen: &Seen,
    input: &InvestigationInput,
    budget: Budget,
) -> CallToolResult {
    let id = id_of(input);
    let status = input
        .status
        .and_then(investigation_status)
        .unwrap_or(InvestigationStatus::Open);
    let request = PatchInvestigation {
        expected_seq: input.expected_seq.unwrap_or_default(),
        title: None,
        question: None,
        status: Some(status),
    };
    let mut guard = seen.lock().await;
    let investigation = match backend.patch_investigation(id.as_str(), &request).await {
        Ok(investigation) => investigation,
        Err(error) => return refused(InvestigationAction::Status, Some(&id), &error),
    };
    let view = investigation_view(&investigation);
    let reply = InvestigationReply {
        outcome: format!(
            "Investigation {} is now {} at seq {}.",
            json_string(id.as_str()),
            view.status,
            view.seq
        ),
        action: InvestigationAction::Status,
        investigation: Some(view),
        entry: None,
        entries: None,
        activity: None,
        activity_note: Some(NO_ACTIVITY_ROUTE.to_owned()),
        investigations: Vec::new(),
        investigations_dropped_to_fit: 0,
        next_actions: Vec::new(),
    };
    let result = match acknowledged(reply, budget, &id) {
        Ok(result) => result,
        Err(unfitted) => return unfitted,
    };
    guard.touch(&id, investigation.seq);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minted_id_is_32_lowercase_hex_digits_and_fresh() {
        let (a, b) = (mint(), mint());
        assert_eq!(a.as_str().len(), 32);
        assert!(InvestigationId::parse(a.as_str()).is_ok());
        assert_ne!(a, b);
    }
}
