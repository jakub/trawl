// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Investigations as text: the board, the activity an attached call
//! carries, and the `investigation` reply (ADR-0059).
//!
//! One renderer prints an investigation for `investigation` with
//! `action: read` and for `read` with `action: investigation`: its title,
//! question, scope, status, sequence and retention, then the entries
//! changed after `since` in `updated_seq` order, then a continuation when
//! trawld holds more or the response budget dropped some. Every entry is
//! one line: `#seq kind by prefix "name" at time: payload`, a step whose
//! outcome came later marked `(changed at #n)`. The actor's name and every
//! text an entry carries print as JSON strings, so stored text stays data
//! and stays on its line.
//!
//! The activity of an attached call is the same lines for the entries
//! other keys changed since the session's `seen_seq`, with two rules that
//! keep each entry printed exactly once per session:
//!
//! - An entry whose line is longer than [`ENTRY_ALLOWANCE_BYTES`] prints
//!   abbreviated: its header and a marker naming the bytes left out and
//!   the read that prints it whole. Every entry fits, so the session can
//!   count it as seen.
//! - The budget cuts entries from the end, never from the middle, and the
//!   block says how many it dropped. A cut, or trawld's `more`, ends the
//!   block with `next: investigation {"action":"read",…,"since":n}` from
//!   the last printed entry. That line belongs to the block, so it
//!   survives when the fitting drops the proposed next calls.
//!
//! A board read prints entries whole. When not even its first entry fits,
//! it prints that one abbreviated, with a marker naming the read at the
//! hard cap when it fits there, or saying no response holds it, so the
//! read still advances.
//!
//! A reply to a write trawld accepted keeps its acknowledgement, the
//! outcome line naming the investigation and the seq written, at every
//! budget. When the entries, the activity and the proposed next calls are
//! gone and the reply still does not fit, the entry written prints
//! abbreviated, its marker naming the read that prints it; then the
//! title, question and scope print as escaped prefixes cut on a character
//! boundary, each with a marker naming the bytes left out and where the
//! whole text is, in the text and the mirror alike. Every reply but one
//! whose attribution alone is larger than the budget fits.
//!
//! The reply's `printed_through` is the `updated_seq` of the last entry the
//! fitted reply printed, read from its mirror by [`printed_through`]: the
//! mirror is the fitted reply, so it holds exactly what the text printed.

use std::fmt::Write as _;

use rmcp::model::{CallToolResult, ContentBlock};
use serde::Serialize;
use serde_json::{Value as Json, json};
use trawl_api::investigations::{
    Activity, Actor, EntryPayload, Investigation, InvestigationRead, InvestigationSummary,
    LogEntry, StepOutcome,
};

use super::budget::{Budget, measure};
use super::refusal::error_result;
use super::rows::{as_u64, json_string, json_text, mirror};
use super::{MAX_NEXT_ACTIONS, one_line, plural, plural_u64};
use crate::reply::{
    AbbreviatedText, ActivityView, ActorView, AttachedView, EntryView, InvestigationReply,
    InvestigationSummaryView, InvestigationView, NextAction, ReadReply,
};
use crate::tools::orient::object;
use crate::tools::schema::{HARD_MAX_BYTES, INVESTIGATION, READ};

/// The most bytes one activity entry's line may take before it prints
/// abbreviated. About a kilobyte: a note of a few sentences, a keep or a
/// short finding print whole; a long finding, a long note or a step with a
/// long query prints its header and the read that prints it whole. With
/// its mirror an entry then costs at most about 2 KiB, so the default
/// 16 KiB budget holds several beside a search's rows.
pub const ENTRY_ALLOWANCE_BYTES: usize = 1024;

/// Why an activity continuation is proposed.
pub const ACTIVITY_CONTINUATION: &str = "print the entries changed after the last one printed here";

/// Why a board continuation is proposed.
pub const BOARD_CONTINUATION: &str = "print the entries changed after the last one printed here";

/// Which tool a board's continuation calls: the one that printed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Continue {
    /// `investigation` with `action: read`.
    Investigation,
    /// `read` with `action: investigation`.
    Read,
}

impl Continue {
    /// The call that prints the entries of `id` changed after `since`.
    #[must_use]
    pub fn call(self, id: &str, since: u64, reason: &str) -> NextAction {
        match self {
            Self::Investigation => NextAction {
                tool: INVESTIGATION.to_owned(),
                arguments: object(json!({"action": "read", "id": id, "since": since})),
                reason: reason.to_owned(),
            },
            Self::Read => NextAction {
                tool: READ.to_owned(),
                arguments: object(json!({
                    "action": "investigation",
                    "investigation_id": id,
                    "since": since,
                })),
                reason: reason.to_owned(),
            },
        }
    }
}

// -- views ----------------------------------------------------------------------

/// `actor` as the reply carries it.
#[must_use]
pub fn actor_view(actor: &Actor) -> ActorView {
    ActorView {
        prefix: actor.prefix.clone(),
        name: actor.name.clone(),
    }
}

/// `investigation` as the reply carries it.
#[must_use]
pub fn investigation_view(investigation: &Investigation) -> InvestigationView {
    InvestigationView {
        id: investigation.id.to_string(),
        title: investigation.title.clone(),
        question: investigation.question.clone(),
        scope: (!investigation.scope.is_empty()).then(|| investigation.scope.clone()),
        status: word(&investigation.status),
        seq: investigation.seq,
        created_at: investigation.created_at.clone(),
        updated_at: investigation.updated_at.clone(),
        retain_until: investigation.retain_until.clone(),
        retain_ceiling: investigation.retain_ceiling.clone(),
        created_by: actor_view(&investigation.created_by),
        updated_by: actor_view(&investigation.updated_by),
        abbreviated: Vec::new(),
    }
}

/// One list row as the reply carries it.
#[must_use]
pub fn summary_view(summary: &InvestigationSummary) -> InvestigationSummaryView {
    InvestigationSummaryView {
        id: summary.id.to_string(),
        title: summary.title.clone(),
        status: word(&summary.status),
        seq: summary.seq,
        updated_at: summary.updated_at.clone(),
        updated_by: actor_view(&summary.updated_by),
    }
}

/// `entry` whole, as the reply carries it.
#[must_use]
pub fn entry_view(entry: &LogEntry) -> EntryView {
    EntryView {
        seq: entry.seq,
        updated_seq: entry.updated_seq,
        kind: word(&entry.payload.kind()),
        actor: actor_view(&entry.actor),
        at: entry.at.clone(),
        payload: serde_json::to_value(&entry.payload).ok(),
        abbreviated_bytes: None,
        abbreviated: None,
    }
}

/// The activity trawld answered for investigation `id` after the session's
/// `since`, each entry longer than [`ENTRY_ALLOWANCE_BYTES`] abbreviated.
#[must_use]
pub fn activity_view(id: &str, since: u64, activity: &Activity) -> ActivityView {
    let entries = activity
        .entries
        .iter()
        .map(|entry| {
            let mut view = entry_view(entry);
            let length = entry_line(&view).len();
            if length > ENTRY_ALLOWANCE_BYTES {
                abbreviate(
                    &mut view,
                    &format!(
                        "investigation action=read id={id} since={}",
                        entry.updated_seq.saturating_sub(1)
                    ),
                );
            }
            view
        })
        .collect();
    let mut view = ActivityView {
        investigation_id: id.to_owned(),
        since,
        entries,
        through_seq: activity.through_seq,
        more: activity.more,
        entries_dropped_to_fit: 0,
        entries_abbreviated: 0,
        printed_through: None,
        continuation: None,
    };
    let total = view.entries.len();
    keep_entries(&mut view, total, total, Continue::Investigation);
    view
}

/// The entries of `read`, a read of investigation `id` after `since`,
/// whole.
#[must_use]
pub fn board_view(id: &str, since: u64, read: &InvestigationRead, tool: Continue) -> ActivityView {
    let mut view = ActivityView {
        investigation_id: id.to_owned(),
        since,
        entries: read.entries.iter().map(entry_view).collect(),
        through_seq: read.through_seq,
        more: read.more,
        entries_dropped_to_fit: 0,
        entries_abbreviated: 0,
        printed_through: None,
        continuation: None,
    };
    let total = view.entries.len();
    keep_entries(&mut view, total, total, tool);
    view
}

/// Keep the first `kept` of the `total` entries trawld sent and make the
/// view's facts and continuation say so.
pub(crate) fn keep_entries(view: &mut ActivityView, kept: usize, total: usize, tool: Continue) {
    view.entries.truncate(kept);
    view.entries_dropped_to_fit = as_u64(total.saturating_sub(kept));
    view.entries_abbreviated = as_u64(
        view.entries
            .iter()
            .filter(|entry| entry.abbreviated.is_some())
            .count(),
    );
    view.printed_through = view.entries.last().map(|entry| entry.updated_seq);
    let from = view.printed_through.unwrap_or(view.since);
    let reason = match tool {
        Continue::Investigation => ACTIVITY_CONTINUATION,
        Continue::Read => BOARD_CONTINUATION,
    };
    view.continuation = (view.entries_dropped_to_fit > 0 || view.more)
        .then(|| tool.call(&view.investigation_id, from, reason));
}

/// Print `view` as its header and `marker`, saying what was left out.
fn abbreviate(view: &mut EntryView, read_with: &str) {
    let omitted = entry_body(view).map_or(0, |body| body.len());
    view.payload = None;
    view.abbreviated_bytes = Some(as_u64(omitted));
    view.abbreviated = Some(format!("+{omitted} bytes: {read_with}"));
}

/// The `updated_seq` of the last entry the fitted reply in `result` printed
/// in its `part` (`activity`, `entries`, or `investigation.activity` for a
/// search), read from its mirror; `None` for an error or a part with no
/// entry printed.
#[must_use]
pub fn printed_through(result: &CallToolResult, part: &[&str]) -> Option<u64> {
    if result.is_error == Some(true) {
        return None;
    }
    let mut at = result.structured_content.as_ref()?;
    for key in part {
        at = at.get(key)?;
    }
    at.get("printed_through")?.as_u64()
}

// -- lines ----------------------------------------------------------------------

/// A serde enum's wire word, such as `finding_status`.
fn word<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

/// An entry's header: its sequence, kind, actor and time.
fn entry_header(entry: &EntryView) -> String {
    let mut header = format!("#{}", entry.seq);
    if entry.updated_seq != entry.seq {
        let _ = write!(header, " (changed at #{})", entry.updated_seq);
    }
    let _ = write!(
        header,
        " {} by {} {} at {}",
        one_line(&entry.kind),
        one_line(&entry.actor.prefix),
        json_string(&entry.actor.name),
        one_line(&entry.at)
    );
    header
}

/// An entry's payload as its line prints it, or `None` when the view
/// holds none (it was abbreviated) or it does not read as an entry.
fn entry_body(entry: &EntryView) -> Option<String> {
    let payload: EntryPayload = serde_json::from_value(entry.payload.clone()?).ok()?;
    Some(payload_text(&payload))
}

/// The one-line text of an entry's payload.
fn payload_text(payload: &EntryPayload) -> String {
    match payload {
        EntryPayload::Step {
            executed,
            bounds,
            request_id,
            query_id,
            purpose,
            outcome,
        } => {
            let mut text = format!("ran {}", json_string(executed));
            if let Some(bounds) = bounds {
                let _ = write!(
                    text,
                    " from {} (inclusive) to {} (exclusive)",
                    one_line(&bounds.earliest),
                    one_line(&bounds.latest)
                );
            }
            let _ = write!(text, "; request id {}", json_string(request_id));
            if let Some(query_id) = query_id {
                let _ = write!(text, "; query id {query_id}");
            }
            if let Some(purpose) = purpose {
                let _ = write!(text, "; purpose {}", json_string(purpose));
            }
            match outcome {
                None => text.push_str("; still running"),
                Some(outcome) => outcome_text(outcome, &mut text),
            }
            text
        }
        EntryPayload::Keep {
            result_id,
            rows,
            reason,
        } => format!(
            "kept rows {} of result {}; reason {}",
            json_text(rows),
            json_string(result_id),
            json_string(reason)
        ),
        EntryPayload::Finding {
            statement,
            status,
            references,
            counterevidence,
            remaining_question,
        } => {
            let mut text = format!(
                "{} {}; references {}",
                word(status),
                json_string(statement),
                json_text(references)
            );
            if !counterevidence.is_empty() {
                let _ = write!(text, "; counterevidence {}", json_text(counterevidence));
            }
            if let Some(question) = remaining_question {
                let _ = write!(text, "; remaining question {}", json_string(question));
            }
            text
        }
        EntryPayload::FindingStatus {
            finding_seq,
            status,
        } => format!("finding #{finding_seq} is now {}", word(status)),
        EntryPayload::Note { text } => json_string(text),
        EntryPayload::Status { status } => {
            format!("the investigation is now {}", word(status))
        }
        EntryPayload::Title { title } => format!("the title is now {}", json_string(title)),
        EntryPayload::Question { question } => {
            format!("the question is now {}", json_string(question))
        }
    }
}

/// A step's outcome, appended to its text.
fn outcome_text(outcome: &StepOutcome, text: &mut String) {
    let _ = write!(text, "; outcome {}", word(&outcome.outcome));
    if let Some(code) = &outcome.code {
        let _ = write!(text, " ({})", word(code));
    }
    if let Some(result_id) = &outcome.result_id {
        let _ = write!(text, "; kept result {}", json_string(result_id));
    }
    if let Some(rows) = outcome.rows {
        let _ = write!(text, " with {rows} {}", plural_u64(rows, "row", "rows"));
    }
    if let Some(ms) = outcome.duration_ms {
        let _ = write!(text, "; ran {ms} ms");
    }
}

/// One entry on one line, whole or abbreviated.
#[must_use]
pub fn entry_line(entry: &EntryView) -> String {
    let header = entry_header(entry);
    match (&entry.abbreviated, entry_body(entry)) {
        (Some(marker), _) => format!("{header}: …[{}]", one_line(marker)),
        (None, Some(body)) => format!("{header}: {body}"),
        (None, None) => format!("{header}: (an entry this server does not read)"),
    }
}

/// An investigation's own lines: id, title, status, sequence, question,
/// scope, retention and attribution.
pub(crate) fn investigation_lines(view: &InvestigationView, lines: &mut Vec<String>) {
    lines.push(format!(
        "investigation {}: {}, {}, seq {}",
        json_string(&view.id),
        header_text(view, "title", &view.title),
        one_line(&view.status),
        view.seq
    ));
    lines.push(format!(
        "question: {}",
        header_text(view, "question", &view.question)
    ));
    if let Some(scope) = &view.scope {
        lines.push(format!("scope: {}", header_text(view, "scope", scope)));
    }
    lines.push(format!(
        "evidence kept until {}, at most until {} unless an operator extends it",
        one_line(&view.retain_until),
        one_line(&view.retain_ceiling)
    ));
    lines.push(format!(
        "created by {} {} at {}; last changed by {} {} at {}",
        one_line(&view.created_by.prefix),
        json_string(&view.created_by.name),
        one_line(&view.created_at),
        one_line(&view.updated_by.prefix),
        json_string(&view.updated_by.name),
        one_line(&view.updated_at)
    ));
}

/// One of an investigation's texts as its line prints it: a JSON string,
/// followed by its marker when the fitting cut it.
fn header_text(view: &InvestigationView, field: &str, text: &str) -> String {
    let printed = json_string(text);
    match view.abbreviated.iter().find(|cut| cut.field == field) {
        Some(cut) => format!("{printed}…[{}]", one_line(&cut.marker)),
        None => printed,
    }
}

/// The bytes `c` prints as inside a JSON string of the text block.
fn escaped_len(c: char) -> usize {
    let mut buf = [0; 4];
    json_string(c.encode_utf8(&mut buf)).len() - 2
}

/// The bytes of `text` printed inside a JSON string, without its quotes.
fn escaped_bytes(text: &str) -> usize {
    text.chars().map(escaped_len).sum()
}

/// The length of the longest prefix of `text` whose escaped form takes at
/// most `cap` bytes, and the escaped bytes it leaves out; `None` when the
/// whole text fits. The prefix ends on a character boundary, so it never
/// splits a scalar or the escape of one.
#[must_use]
pub fn cut_escaped(text: &str, cap: usize) -> Option<(usize, usize)> {
    let total = escaped_bytes(text);
    if total <= cap {
        return None;
    }
    let (mut kept, mut end) = (0, 0);
    for (at, c) in text.char_indices() {
        let len = escaped_len(c);
        if kept + len > cap {
            break;
        }
        kept += len;
        end = at + c.len_utf8();
    }
    Some((end, total - kept))
}

/// Cut each of `view`'s title, question and scope whose escaped form is
/// longer than `cap` bytes to the prefix that fits, its marker saying
/// what was left out and that `whole_at` shows it whole.
fn abbreviate_header(view: &mut InvestigationView, cap: usize, whole_at: &str) {
    let InvestigationView {
        title,
        question,
        scope,
        abbreviated,
        ..
    } = view;
    for (field, text) in [("title", Some(title)), ("question", Some(question))]
        .into_iter()
        .chain([("scope", scope.as_mut())])
    {
        let Some(text) = text else { continue };
        if let Some((end, omitted)) = cut_escaped(text, cap) {
            text.truncate(end);
            abbreviated.push(AbbreviatedText {
                field: field.to_owned(),
                omitted_bytes: as_u64(omitted),
                marker: format!("+{omitted} bytes: {whole_at}"),
            });
        }
    }
}

/// The facts of an entry block: what it prints, abbreviates and drops.
fn block_facts(view: &ActivityView, budget: Budget) -> String {
    let printed = view.entries.len();
    let mut facts = format!("{printed} {}", plural(printed, "entry", "entries"));
    if view.entries_abbreviated > 0 {
        let _ = write!(
            facts,
            ", {} abbreviated to {}",
            view.entries_abbreviated,
            if view.entries_abbreviated == 1 {
                "its header"
            } else {
                "their headers"
            }
        );
    }
    if view.entries_dropped_to_fit > 0 {
        let _ = write!(
            facts,
            "; to fit the {}-byte budget it dropped {} more from the end",
            budget.bytes(),
            view.entries_dropped_to_fit
        );
    }
    if view.more {
        let _ = write!(facts, "; trawld holds more past #{}", view.through_seq);
    }
    facts
}

/// An entry block's lines: the facts line, the entries, the continuation.
fn block_lines(heading: &str, view: &ActivityView, budget: Budget, lines: &mut Vec<String>) {
    if view.entries.is_empty() && view.entries_dropped_to_fit == 0 && !view.more {
        lines.push(format!("{heading}: none."));
        return;
    }
    lines.push(format!("{heading}: {}", block_facts(view, budget)));
    lines.extend(view.entries.iter().map(entry_line));
    if let Some(next) = &view.continuation {
        lines.push(next_line(next));
    }
}

/// One proposed call, as every reply prints it.
fn next_line(action: &NextAction) -> String {
    format!(
        "next: {} {} — {}",
        one_line(&action.tool),
        json_text(&action.arguments),
        one_line(&action.reason)
    )
}

/// The board lines: entries changed after `since`, in change order.
pub(crate) fn board_lines(view: &ActivityView, budget: Budget, lines: &mut Vec<String>) {
    block_lines(
        &format!("entries changed after #{}, in change order", view.since),
        view,
        budget,
        lines,
    );
}

/// The activity lines: entries other keys changed since the session's
/// `seen_seq`.
pub(crate) fn activity_lines(view: &ActivityView, budget: Budget, lines: &mut Vec<String>) {
    block_lines(
        &format!(
            "activity by other keys since #{}, in change order",
            view.since
        ),
        view,
        budget,
        lines,
    );
}

/// An attached search's lines: the investigation, the kept result and the
/// activity, or the sentence saying why there is none.
pub(crate) fn attached_lines(view: &AttachedView, budget: Budget, lines: &mut Vec<String>) {
    let mut line = format!("investigation: {}", json_string(&view.id));
    if let Some(result_id) = &view.result_id {
        let _ = write!(line, "; kept result {}", json_string(result_id));
    }
    lines.push(line);
    if let Some(activity) = &view.activity {
        activity_lines(activity, budget, lines);
    }
    if let Some(note) = &view.note {
        lines.push(format!("activity: {}", one_line(note)));
    }
}

/// The text block of an `investigation` reply, unfitted.
#[must_use]
pub fn investigation_text(reply: &InvestigationReply, budget: Budget) -> String {
    let mut lines = vec![one_line(&reply.outcome)];
    if let Some(view) = &reply.investigation {
        investigation_lines(view, &mut lines);
    }
    if let Some(entry) = &reply.entry {
        lines.push(format!("entry written: {}", entry_line(entry)));
    }
    if let Some(entries) = &reply.entries {
        board_lines(entries, budget, &mut lines);
    }
    if let Some(activity) = &reply.activity {
        activity_lines(activity, budget, &mut lines);
    }
    if let Some(note) = &reply.activity_note {
        lines.push(format!("activity: {}", one_line(note)));
    }
    if !reply.investigations.is_empty() || reply.investigations_dropped_to_fit > 0 {
        list_lines(reply, budget, &mut lines);
    }
    for action in reply.next_actions.iter().take(MAX_NEXT_ACTIONS) {
        lines.push(next_line(action));
    }
    lines.join("\n")
}

/// The list's lines, one investigation each.
fn list_lines(reply: &InvestigationReply, budget: Budget, lines: &mut Vec<String>) {
    let mut heading = format!(
        "investigations, open first, most recently changed first: {} printed",
        reply.investigations.len()
    );
    if reply.investigations_dropped_to_fit > 0 {
        let _ = write!(
            heading,
            "; to fit the {}-byte budget it dropped {} more from the end",
            budget.bytes(),
            reply.investigations_dropped_to_fit
        );
    }
    lines.push(heading);
    lines.extend(reply.investigations.iter().map(summary_line));
}

/// One investigation as a list prints it.
#[must_use]
pub fn summary_line(summary: &InvestigationSummaryView) -> String {
    format!(
        "{} {}, seq {}, changed {} by {} {}: {}",
        json_string(&summary.id),
        one_line(&summary.status),
        summary.seq,
        one_line(&summary.updated_at),
        one_line(&summary.updated_by.prefix),
        json_string(&summary.updated_by.name),
        json_string(&summary.title)
    )
}

// -- fitting --------------------------------------------------------------------

/// A reply whose entry block or list the fitting may cut from the end.
pub(crate) trait Entries: Serialize + Clone {
    /// The text block under `budget`.
    fn text(&self, budget: Budget) -> String;
    /// The board, the activity or the list the fitting cuts, as
    /// [`Cut`].
    fn cut(&mut self) -> Option<Cut<'_>>;
    /// The proposed next calls.
    fn next_actions_mut(&mut self) -> &mut Vec<NextAction>;
    /// The entry a write wrote, which the fitting may abbreviate.
    fn written_mut(&mut self) -> Option<&mut EntryView> {
        None
    }
    /// The investigation's own fields, whose texts the fitting may cut.
    fn header_mut(&mut self) -> Option<&mut InvestigationView>;
}

/// What the fitting cuts from the end.
pub(crate) enum Cut<'a> {
    /// An entry block, the board when `board`, else the activity, whose
    /// continuation calls `tool`.
    Block {
        view: &'a mut ActivityView,
        board: bool,
        tool: Continue,
    },
    /// The investigation list and its dropped count.
    List {
        rows: &'a mut Vec<InvestigationSummaryView>,
        dropped: &'a mut u64,
    },
}

impl Cut<'_> {
    fn len(&self) -> usize {
        match self {
            Self::Block { view, .. } => view.entries.len(),
            Self::List { rows, .. } => rows.len(),
        }
    }
}

impl Entries for InvestigationReply {
    fn text(&self, budget: Budget) -> String {
        investigation_text(self, budget)
    }
    fn cut(&mut self) -> Option<Cut<'_>> {
        if let Some(view) = self.entries.as_mut() {
            return Some(Cut::Block {
                view,
                board: true,
                tool: Continue::Investigation,
            });
        }
        if let Some(view) = self.activity.as_mut() {
            return Some(Cut::Block {
                view,
                board: false,
                tool: Continue::Investigation,
            });
        }
        if self.investigations.is_empty() {
            return None;
        }
        Some(Cut::List {
            rows: &mut self.investigations,
            dropped: &mut self.investigations_dropped_to_fit,
        })
    }
    fn next_actions_mut(&mut self) -> &mut Vec<NextAction> {
        &mut self.next_actions
    }
    fn written_mut(&mut self) -> Option<&mut EntryView> {
        self.entry.as_mut()
    }
    fn header_mut(&mut self) -> Option<&mut InvestigationView> {
        self.investigation.as_mut()
    }
}

impl Entries for ReadReply {
    fn text(&self, budget: Budget) -> String {
        super::read_text(self, budget)
    }
    fn cut(&mut self) -> Option<Cut<'_>> {
        self.entries.as_mut().map(|view| Cut::Block {
            view,
            board: true,
            tool: Continue::Read,
        })
    }
    fn next_actions_mut(&mut self) -> &mut Vec<NextAction> {
        &mut self.next_actions
    }
    fn header_mut(&mut self) -> Option<&mut InvestigationView> {
        self.investigation.as_mut()
    }
}

/// `reply` as a tool result fitted under `budget`; `tool` names the tool
/// for the error a reply that does not fit gets. A board whose first
/// entry does not print, even abbreviated, is an error naming that entry
/// and the read past it.
pub(crate) fn fit_entries<R: Entries>(reply: R, budget: Budget, tool: &str) -> CallToolResult {
    let first = first_entry(&reply);
    let hard = (budget != Budget::HARD)
        .then(|| reply.clone())
        .filter(|_| first.is_some());
    match fit_reply(reply, budget, None) {
        Some(result) => result,
        None => error_result(match first {
            Some((id, entry, tool)) => {
                let fits_hard =
                    hard.is_some_and(|reply| fit_reply(reply, Budget::HARD, None).is_some());
                entry_does_not_fit(&id, &entry, tool, budget, fits_hard)
            }
            None => does_not_fit(tool, budget),
        }),
    }
}

/// The investigation id, the first entry and the continuation tool of
/// `reply`'s board; `None` for a reply with no board entry.
fn first_entry<R: Entries>(reply: &R) -> Option<(String, EntryView, Continue)> {
    match reply.clone().cut() {
        Some(Cut::Block {
            view,
            board: true,
            tool,
        }) => Some((
            view.investigation_id.clone(),
            view.entries.first()?.clone(),
            tool,
        )),
        _ => None,
    }
}

/// Why a board of investigation `id` answered under `budget` prints no
/// entry: `entry`, its first, does not fit even abbreviated. Names the
/// read past it, a larger `max_bytes` when the entry fits at the hard cap
/// (`fits_hard`), and the investigation's page in trawl's web UI.
fn entry_does_not_fit(
    id: &str,
    entry: &EntryView,
    tool: Continue,
    budget: Budget,
    fits_hard: bool,
) -> String {
    let past = tool.call(id, entry.updated_seq, "");
    let larger = if fits_hard {
        format!("; a larger max_bytes, at most {HARD_MAX_BYTES}, prints it abbreviated")
    } else {
        String::new()
    };
    format!(
        "{} answered, but entry #{} of investigation {} does not fit max_bytes={} even \
         abbreviated, so the reply would print no entry: {} {} reads the entries after it{larger}; \
         the investigation's page in trawl's web UI shows it whole.",
        one_line(&past.tool),
        entry.seq,
        json_string(id),
        budget.bytes(),
        one_line(&past.tool),
        json_text(&past.arguments),
    )
}

/// The reply to a write trawld accepted for investigation `id`, fitted
/// under `budget`; `None` only when not even its acknowledgement with
/// every text cut fits, which takes an attribution larger than the budget.
pub(crate) fn fit_written(
    reply: InvestigationReply,
    budget: Budget,
    id: &str,
) -> Option<CallToolResult> {
    fit_reply(reply, budget, Some(id))
}

/// Why a reply `tool` answered does not fit `budget`, and what to do.
#[must_use]
pub fn does_not_fit(tool: &str, budget: Budget) -> String {
    let bytes = budget.bytes();
    if budget == Budget::HARD {
        format!(
            "{tool} answered, but its reply does not fit max_bytes={bytes}, the most one \
             response holds, even with its texts abbreviated and without its entries or the \
             proposed next calls; trawl's web UI shows it."
        )
    } else {
        format!(
            "{tool} answered, but its reply does not fit max_bytes={bytes} even with its texts \
             abbreviated and without its entries or the proposed next calls; send a larger \
             max_bytes, at most {HARD_MAX_BYTES}."
        )
    }
}

/// A rendered reply and the entries or rows of its cut it kept.
type Fitted = (String, Json, usize);

/// `reply` fitted under `budget`, giving up, in order: entries from the
/// end of its block, the proposed next calls, the whole entry written (a
/// write's, of investigation `written_in`), and the whole header texts. A
/// board with entries that prints none of them does not advance, so a
/// later step that prints one wins over it, and when no step prints one
/// the reply is `None`: [`fit_entries`] says which entry did not print.
fn fit_reply<R: Entries>(
    mut reply: R,
    budget: Budget,
    written_in: Option<&str>,
) -> Option<CallToolResult> {
    reply.next_actions_mut().truncate(MAX_NEXT_ACTIONS);
    let needs_one = matches!(
        reply.clone().cut(),
        Some(Cut::Block { view, board: true, .. }) if !view.entries.is_empty()
    );
    let enough = |found: &Option<Fitted>| found.as_ref().is_some_and(|f| !needs_one || f.2 > 0);
    let mut fallback = None;
    let mut found = most_entries(&reply, budget);
    if !enough(&found) {
        fallback = fallback.or(found.take());
        reply.next_actions_mut().clear();
        found = most_entries(&reply, budget);
    }
    if !enough(&found)
        && let Some(id) = written_in
        && let Some(entry) = reply.written_mut()
    {
        fallback = fallback.or(found.take());
        let read_with = format!(
            "investigation action=read id={id} since={}",
            entry.updated_seq.saturating_sub(1)
        );
        abbreviate(entry, &read_with);
        found = most_entries(&reply, budget);
    }
    if !enough(&found) {
        fallback = fallback.or(found.take());
        found = cut_header(&reply, budget);
    }
    let (text, mirror, _) = if enough(&found) {
        found
    } else if needs_one {
        // A board that prints none of its entries would propose the same
        // read again: the caller answers why instead.
        None
    } else {
        found.or(fallback)
    }?;
    let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
    result.structured_content = Some(mirror);
    Some(result)
}

/// `reply` with its title, question and scope cut to the longest escaped
/// prefixes, one cap for all three, with which as many entries print as
/// with empty prefixes; `None` when not even empty prefixes fit. Each
/// marker names the read at the hard cap when the whole header fits there,
/// else the investigation's page in trawl's web UI.
fn cut_header<R: Entries>(reply: &R, budget: Budget) -> Option<Fitted> {
    let mut whole = reply.clone();
    let view = whole.header_mut()?;
    let longest = [Some(&view.title), Some(&view.question), view.scope.as_ref()]
        .into_iter()
        .flatten()
        .map(|text| escaped_bytes(text))
        .max()
        .unwrap_or(0);
    let read_hard = format!(
        "investigation action=read id={} since={} max_bytes={HARD_MAX_BYTES}",
        view.id, view.seq
    );
    let total = reply.clone().cut().map_or(0, |cut| cut.len());
    let whole_at = if fitted(&keeping(reply, 0, total), Budget::HARD).is_some() {
        read_hard
    } else {
        "the investigation's page in trawl's web UI shows it whole".to_owned()
    };
    let attempt = |cap: usize| {
        let mut candidate = reply.clone();
        abbreviate_header(candidate.header_mut()?, cap, &whole_at);
        most_entries(&candidate, budget)
    };
    let mut best = attempt(0)?;
    let kept = best.2;
    let (mut fits, mut does_not) = (0, longest);
    while does_not - fits > 1 {
        let mid = fits + (does_not - fits) / 2;
        match attempt(mid) {
            Some(fitted) if fitted.2 >= kept => {
                best = fitted;
                fits = mid;
            }
            _ => does_not = mid,
        }
    }
    Some(best)
}

/// `candidate` rendered under `budget`, or `None` when it does not fit.
fn fitted<R: Entries>(candidate: &R, budget: Budget) -> Option<(String, Json)> {
    let mirror = mirror(candidate).ok()?;
    let text = candidate.text(budget);
    (measure(&text, &mirror) <= budget.bytes()).then_some((text, mirror))
}

/// `reply` keeping the first `kept` of the `total` entries or rows of its
/// cut.
fn keeping<R: Entries>(reply: &R, kept: usize, total: usize) -> R {
    let mut candidate = reply.clone();
    match candidate.cut() {
        Some(Cut::Block { view, tool, .. }) => keep_entries(view, kept, total, tool),
        Some(Cut::List { rows, dropped }) => {
            rows.truncate(kept);
            *dropped = as_u64(total.saturating_sub(kept));
        }
        None => {}
    }
    candidate
}

/// The reply with the most of its first entries that fit, rendered, or
/// `None` when not even none fit. A binary search over the kept count, as
/// the result fitting searches rows; every answer was measured.
///
/// A board that fits none of its entries prints its first one
/// abbreviated instead, so a read advances; when not even that fits, the
/// read is the error [`fit_entries`] answers.
fn most_entries<R: Entries>(reply: &R, budget: Budget) -> Option<Fitted> {
    let total = reply.clone().cut().map_or(0, |cut| cut.len());
    let attempt = |kept: usize| {
        fitted(&keeping(reply, kept, total), budget).map(|(text, mirror)| (text, mirror, kept))
    };
    if let Some(all) = attempt(total) {
        return Some(all);
    }
    if total == 0 {
        return None;
    }
    let Some(mut best) = attempt(1) else {
        return first_abbreviated(reply, total, budget)
            .map(|(text, mirror)| (text, mirror, 1))
            .or_else(|| attempt(0));
    };
    let (mut fits, mut does_not) = (1, total);
    while does_not - fits > 1 {
        let mid = fits + (does_not - fits) / 2;
        if let Some(fitted) = attempt(mid) {
            best = fitted;
            fits = mid;
        } else {
            does_not = mid;
        }
    }
    Some(best)
}

/// For a board whose first entry does not fit `budget`: the reply printing
/// that entry abbreviated, its marker naming the read at the hard cap when
/// the entry fits there, else saying no response holds it.
fn first_abbreviated<R: Entries>(
    reply: &R,
    total: usize,
    budget: Budget,
) -> Option<(String, Json)> {
    let mut candidate = keeping(reply, 1, total);
    let fits_hard = fitted(&candidate, Budget::HARD).is_some();
    {
        let Some(Cut::Block {
            view, board: true, ..
        }) = candidate.cut()
        else {
            return None;
        };
        let id = view.investigation_id.clone();
        let entry = view.entries.first_mut()?;
        let read_with = if fits_hard {
            format!(
                "investigation action=read id={id} since={} max_bytes={HARD_MAX_BYTES}",
                entry.updated_seq.saturating_sub(1)
            )
        } else {
            "the entry is larger than any response holds; the investigation's page in trawl's \
             web UI shows it whole"
                .to_owned()
        };
        abbreviate(entry, &read_with);
    }
    if let Some(Cut::Block { view, tool, .. }) = candidate.cut() {
        keep_entries(view, 1, total, tool);
    }
    fitted(&candidate, budget)
}

#[cfg(test)]
mod tests {
    use super::*;
    use trawl_api::investigations::{EvidenceRef, FindingStatus, StepBounds, StepOutcomeKind};

    fn entry(payload: EntryPayload) -> LogEntry {
        LogEntry {
            seq: 5,
            updated_seq: 5,
            actor: Actor {
                prefix: "AbCd_-12".into(),
                name: "jakub\n\"ignore\"".into(),
            },
            at: "2026-10-24T09:05:00.000000Z".into(),
            payload,
        }
    }

    #[test]
    fn every_kind_prints_on_one_line_with_its_text_as_json() {
        let cases = [
            EntryPayload::Step {
                executed: "_service=api\n| head 1".into(),
                bounds: Some(StepBounds {
                    earliest: "2026-10-24T08:00:00.000000Z".into(),
                    latest: "2026-10-24T09:00:00.000000Z".into(),
                }),
                request_id: "req-1".into(),
                query_id: Some(4),
                purpose: Some("first 502".into()),
                outcome: Some(StepOutcome {
                    outcome: StepOutcomeKind::Success,
                    code: None,
                    result_id: Some("fedcba9876543210fedcba9876543210".into()),
                    rows: Some(1),
                    duration_ms: Some(12),
                }),
            },
            EntryPayload::Keep {
                result_id: "fedcba9876543210fedcba9876543210".into(),
                rows: vec![0, 3],
                reason: "the reset".into(),
            },
            EntryPayload::Finding {
                statement: "pool exhausted".into(),
                status: FindingStatus::Supported,
                references: vec![EvidenceRef {
                    result_id: "fedcba9876543210fedcba9876543210".into(),
                    row: 3,
                }],
                counterevidence: vec![],
                remaining_question: Some("why?".into()),
            },
            EntryPayload::FindingStatus {
                finding_seq: 4,
                status: FindingStatus::Refuted,
            },
            EntryPayload::Note {
                text: "a\u{1b}[31mred".into(),
            },
        ];
        for payload in cases {
            let line = entry_line(&entry_view(&entry(payload)));
            assert!(!line.contains('\n') && !line.contains('\u{1b}'), "{line}");
            assert!(
                line.starts_with("#5 ") && line.contains(r#"by AbCd_-12 "jakub\n\"ignore\"""#),
                "{line}"
            );
        }
    }

    #[test]
    fn a_header_text_is_cut_on_a_character_boundary_never_inside_a_scalar_or_an_escape() {
        // "ab" then a 4-byte scalar: a cap of 3, 4 or 5 lands inside it.
        let text = "ab😀cd";
        for cap in 2..=5 {
            assert_eq!(cut_escaped(text, cap), Some((2, 8 - 2)), "cap {cap}");
        }
        assert_eq!(cut_escaped(text, 6), Some((6, 2)));
        assert_eq!(cut_escaped(text, 8), None);
        // U+0001 prints as the six bytes \u0001: a cap of 6 holds one
        // escape, and one of 7 to 11 lands inside the second.
        let text = "\u{1}\u{1}\u{1}";
        for cap in 6..=11 {
            assert_eq!(cut_escaped(text, cap), Some((1, 18 - 6)), "cap {cap}");
        }
        assert_eq!(cut_escaped(text, 5), Some((0, 18)));

        let mut view = investigation_view(&Investigation {
            id: trawl_api::investigations::InvestigationId::parse(
                "0123456789abcdef0123456789abcdef",
            )
            .unwrap(),
            title: "t😀😀".into(),
            question: "\u{1}".repeat(3),
            scope: String::new(),
            status: trawl_api::investigations::InvestigationStatus::Open,
            seq: 2,
            created_at: "2026-10-24T09:00:00.000000Z".into(),
            updated_at: "2026-10-24T09:05:00.000000Z".into(),
            retain_until: "2026-10-31T09:05:00.000000Z".into(),
            retain_ceiling: "2026-11-23T09:00:00.000000Z".into(),
            created_by: Actor {
                prefix: "abcd1234".into(),
                name: "agent".into(),
            },
            updated_by: Actor {
                prefix: "abcd1234".into(),
                name: "agent".into(),
            },
        });
        abbreviate_header(&mut view, 8, "where");
        assert_eq!(view.title, "t😀");
        assert_eq!(view.question, "\u{1}");
        assert_eq!(view.scope, None);
        let mut lines = Vec::new();
        investigation_lines(&view, &mut lines);
        assert!(
            lines[0].contains(r#": "t😀"…[+4 bytes: where], open, seq 2"#),
            "{}",
            lines[0]
        );
        assert_eq!(lines[1], r#"question: "\u0001"…[+12 bytes: where]"#);
        assert_eq!(
            view.abbreviated,
            [
                AbbreviatedText {
                    field: "title".into(),
                    omitted_bytes: 4,
                    marker: "+4 bytes: where".into(),
                },
                AbbreviatedText {
                    field: "question".into(),
                    omitted_bytes: 12,
                    marker: "+12 bytes: where".into(),
                },
            ]
        );
    }

    #[test]
    fn a_long_activity_entry_prints_its_header_and_the_read_that_prints_it() {
        let long = entry(EntryPayload::Note {
            text: "x".repeat(2000),
        });
        let activity = Activity {
            entries: vec![long],
            through_seq: 5,
            more: false,
        };
        let id = "0123456789abcdef0123456789abcdef";
        let view = activity_view(id, 3, &activity);
        let line = entry_line(&view.entries[0]);
        assert!(line.len() <= ENTRY_ALLOWANCE_BYTES, "{line}");
        assert!(
            line.ends_with(&format!(
                "…[+2002 bytes: investigation action=read id={id} since=4]"
            )),
            "{line}"
        );
        assert_eq!(view.entries_abbreviated, 1);
        assert_eq!(view.printed_through, Some(5));
        assert_eq!(view.continuation, None);
    }
}
