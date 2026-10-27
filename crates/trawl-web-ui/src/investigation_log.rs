// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The investigation page's reading of one investigation (ADR-0059).
//!
//! A read answers the entries changed after a `since` sequence, in
//! `updated_seq` order, bounded, with the `through_seq` the next read
//! starts from. [`Board`] folds those reads into what the page shows:
//! every entry once, at its latest change, keyed by the `seq` that
//! created it. Every read is the same read, `since` the board's
//! `through_seq`, whether the page polls, or a person asked with Refresh
//! or Load more, so a step that gained its outcome after the page loaded
//! replaces its running copy in place, and the page updates that step
//! where it stands instead of adding another.
//!
//! The page has three readings of the log. Findings are the claims, each
//! with its current status: a `finding_status` entry overwrites the
//! status of the finding whose `seq` it names. The activity is every
//! entry, findings and their status changes included, one line each in
//! `seq` order, so a note sits between the steps it was written between.
//! The steps are the queries that ran, each with the text it executed,
//! its bounds and its outcome.
//!
//! A finding later in the log can be the latest one, and a later
//! `finding_status` entry can overwrite a status, so the findings are
//! claimed only from the whole log ([`Reach`]): a read that leaves
//! entries past the board is followed at once, up to [`AUTO_PAGES`]
//! reads, and past that what the page shows is as of the entry it
//! reached.
//!
//! Pure and ungated so its tests run natively; the page that renders it
//! is wasm32-only.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use std::collections::BTreeMap;

use fleet_ui::Tone;
use trawl_api::investigations::{
    Actor, EntryPayload, EvidenceRef, FindingStatus, Investigation, InvestigationRead,
    InvestigationStatus, LogEntry, StepOutcome, StepOutcomeKind,
};
use trawl_api::results::{CapturedResult, ResultPage};

use crate::results_layout::cell_text;
use crate::severity_cell::{severity_columns, severity_display};

/// Entries one read asks for.
pub const ENTRY_PAGE: usize = 100;

/// Reads the page makes one after another, from the read the person
/// asked for, before it leaves the rest of the log to Load more: 2000
/// entries.
pub const AUTO_PAGES: u32 = 20;

/// How much of the log the board holds, which decides what the page may
/// claim about findings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// Every entry is read: the findings are the log's.
    Whole,
    /// Entries remain and the page is reading them: it claims no finding
    /// yet.
    Reading,
    /// Entries remain and the page stopped reading, at [`AUTO_PAGES`] or
    /// on a failed read: what it shows is as of this entry, and Load more
    /// reads on.
    AsOf(u64),
}

/// One investigation as the page has read it so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Board {
    /// The investigation's own fields, from the newest read.
    pub investigation: Investigation,
    /// Every entry read, at its latest change, by the `seq` that created
    /// it.
    entries: BTreeMap<u64, LogEntry>,
    /// The `updated_seq` the reads reach: the next read's `since`.
    pub through_seq: u64,
    /// Whether the server holds entries past `through_seq`.
    pub more: bool,
}

/// What one read changed on the board.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Merged {
    /// Entries the board did not hold before.
    pub added: usize,
    /// Whether anything on the board changed: an entry, the
    /// investigation's fields, or how far the reads reach.
    pub changed: bool,
}

/// One entry as the activity shows it: what happened, in a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activity {
    /// The `seq` that created the entry, which is its identity.
    pub seq: u64,
    /// What kind of entry it is, in words.
    pub kind: &'static str,
    /// A step's outcome or a finding's status, as a badge.
    pub badge: Option<(&'static str, Tone)>,
    /// The line itself.
    pub text: String,
    /// Who wrote the entry.
    pub actor: Actor,
    /// When it was written.
    pub at: String,
}

/// A finding with its current status folded in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The `seq` of the finding's own entry, which is its id.
    pub seq: u64,
    /// Who wrote it.
    pub author: Actor,
    /// When it was written.
    pub at: String,
    /// The statement.
    pub statement: String,
    /// The current status: the newest `finding_status` entry naming this
    /// finding, else the status it was written with.
    pub status: FindingStatus,
    /// Who set the current status, when someone overwrote the one the
    /// finding was written with.
    pub status_by: Option<Actor>,
    /// Rows that support it.
    pub references: Vec<EvidenceRef>,
    /// Rows against it.
    pub counterevidence: Vec<EvidenceRef>,
    /// What it leaves open.
    pub remaining_question: Option<String>,
}

impl Board {
    /// The board after the first read.
    #[must_use]
    pub fn new(read: InvestigationRead) -> Self {
        let mut board = Self {
            investigation: read.investigation.clone(),
            entries: BTreeMap::new(),
            through_seq: 0,
            more: false,
        };
        let _ = board.merge(read);
        board
    }

    /// Fold a later read in, and say what it changed. An entry replaces
    /// the copy already held only when it is at least as new, and the
    /// investigation's fields only when its `seq` is at least the one
    /// held, so a read that crossed a newer one in flight cannot move the
    /// board backwards.
    pub fn merge(&mut self, read: InvestigationRead) -> Merged {
        let mut merged = Merged::default();
        if read.investigation.seq >= self.investigation.seq {
            merged.changed |= read.investigation != self.investigation;
            self.investigation = read.investigation;
        }
        for entry in read.entries {
            match self.entries.get(&entry.seq) {
                None => {
                    merged.added += 1;
                    merged.changed = true;
                }
                Some(held) if entry.updated_seq >= held.updated_seq => {
                    merged.changed |= *held != entry;
                }
                Some(_) => continue,
            }
            self.entries.insert(entry.seq, entry);
        }
        if read.through_seq >= self.through_seq {
            merged.changed |= (read.through_seq, read.more) != (self.through_seq, self.more);
            self.through_seq = read.through_seq;
            self.more = read.more;
        }
        merged
    }

    /// Take the investigation's fields from a write's answer: a close or
    /// reopen the server accepted shows at once, even if the read after it
    /// fails. Only a copy at least as new as the one held is taken, and
    /// the entries and `through_seq` stay as the reads left them: the next
    /// read still brings the entry the write recorded. Whether anything
    /// changed.
    pub fn adopt(&mut self, investigation: Investigation) -> bool {
        if investigation.seq < self.investigation.seq || investigation == self.investigation {
            return false;
        }
        self.investigation = investigation;
        true
    }

    /// Whether another read is due at once after a merge, `pages` reads
    /// into the run the person started: entries remain and the run is
    /// under [`AUTO_PAGES`].
    #[must_use]
    pub fn follow_up_due(&self, pages: u32) -> bool {
        self.more && pages < AUTO_PAGES
    }

    /// How far the board reaches, `pages` reads into the current run,
    /// `reading` while a read of it is in flight.
    #[must_use]
    pub fn reach(&self, pages: u32, reading: bool) -> Reach {
        if !self.more {
            Reach::Whole
        } else if reading && pages < AUTO_PAGES {
            Reach::Reading
        } else {
            Reach::AsOf(self.through_seq)
        }
    }

    /// Every finding, oldest first, each with its current status.
    #[must_use]
    pub fn findings(&self) -> Vec<Finding> {
        let mut findings: BTreeMap<u64, Finding> = BTreeMap::new();
        for entry in self.entries.values() {
            if let EntryPayload::Finding {
                statement,
                status,
                references,
                counterevidence,
                remaining_question,
            } = &entry.payload
            {
                findings.insert(
                    entry.seq,
                    Finding {
                        seq: entry.seq,
                        author: entry.actor.clone(),
                        at: entry.at.clone(),
                        statement: statement.clone(),
                        status: *status,
                        status_by: None,
                        references: references.clone(),
                        counterevidence: counterevidence.clone(),
                        remaining_question: remaining_question.clone(),
                    },
                );
            }
        }
        // `entries` iterates in `seq` order, so the last overwrite of a
        // finding's status is the one that stands.
        for entry in self.entries.values() {
            if let EntryPayload::FindingStatus {
                finding_seq,
                status,
            } = &entry.payload
                && let Some(finding) = findings.get_mut(finding_seq)
            {
                finding.status = *status;
                finding.status_by = Some(entry.actor.clone());
            }
        }
        findings.into_values().collect()
    }

    /// The finding written last.
    #[must_use]
    pub fn latest_finding(&self) -> Option<Finding> {
        self.findings().pop()
    }

    /// The activity: every entry, one line each, in `seq` order. A step
    /// is its purpose and its outcome so far; what it executed is left to
    /// [`Self::steps`]. A status change of a finding the board holds
    /// quotes that finding's statement.
    #[must_use]
    pub fn activity(&self) -> Vec<Activity> {
        self.entries
            .values()
            .map(|entry| {
                let (kind, badge, text) = match &entry.payload {
                    EntryPayload::Step {
                        purpose, outcome, ..
                    } => (
                        "Step",
                        Some((
                            outcome_label(outcome.as_ref()),
                            outcome_tone(outcome.as_ref()),
                        )),
                        purpose.clone().unwrap_or_else(|| NO_PURPOSE.to_owned()),
                    ),
                    EntryPayload::Note { text } => ("Note", None, text.clone()),
                    EntryPayload::Keep {
                        result_id,
                        rows,
                        reason,
                    } => ("Keep", None, keep_text(result_id, rows, reason)),
                    EntryPayload::Finding {
                        statement, status, ..
                    } => (
                        "Finding",
                        Some((finding_status_label(*status), finding_status_tone(*status))),
                        statement.clone(),
                    ),
                    EntryPayload::FindingStatus {
                        finding_seq,
                        status,
                    } => (
                        "Finding status",
                        Some((finding_status_label(*status), finding_status_tone(*status))),
                        match self.entries.get(finding_seq).map(|e| &e.payload) {
                            Some(EntryPayload::Finding { statement, .. }) => statement.clone(),
                            _ => format!("Finding #{finding_seq}, not read yet."),
                        },
                    ),
                    EntryPayload::Status { status } => (
                        "Status",
                        None,
                        match status {
                            InvestigationStatus::Open => "Reopened the investigation.",
                            InvestigationStatus::Closed => "Closed the investigation.",
                        }
                        .to_owned(),
                    ),
                    EntryPayload::Title { title } => ("Title", None, title.clone()),
                    EntryPayload::Question { question } => ("Question", None, question.clone()),
                };
                Activity {
                    seq: entry.seq,
                    kind,
                    badge,
                    text,
                    actor: entry.actor.clone(),
                    at: entry.at.clone(),
                }
            })
            .collect()
    }

    /// Every step, in `seq` order, each at its latest change.
    #[must_use]
    pub fn steps(&self) -> Vec<LogEntry> {
        self.entries
            .values()
            .filter(|entry| matches!(entry.payload, EntryPayload::Step { .. }))
            .cloned()
            .collect()
    }
}

/// What a step that was given no purpose says in its place.
pub const NO_PURPOSE: &str = "No purpose given.";

/// A keep in a line: the rows, the result and the reason.
fn keep_text(result_id: &str, rows: &[u64], reason: &str) -> String {
    let rows = rows
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let noun = if rows.contains(',') { "rows" } else { "row" };
    format!(
        "Kept {noun} {rows} of result {}: {reason}",
        short_result_id(result_id)
    )
}

/// A finding status in words.
#[must_use]
pub fn finding_status_label(status: FindingStatus) -> &'static str {
    match status {
        FindingStatus::Hypothesis => "Hypothesis",
        FindingStatus::Supported => "Supported",
        FindingStatus::Refuted => "Refuted",
        FindingStatus::Unresolved => "Unresolved",
    }
}

/// The badge tone of a finding status.
#[must_use]
pub fn finding_status_tone(status: FindingStatus) -> Tone {
    match status {
        FindingStatus::Hypothesis => Tone::Info,
        FindingStatus::Supported => Tone::Success,
        FindingStatus::Refuted => Tone::Danger,
        FindingStatus::Unresolved => Tone::Warn,
    }
}

/// An investigation status in words.
#[must_use]
pub fn investigation_status_label(status: InvestigationStatus) -> &'static str {
    match status {
        InvestigationStatus::Open => "Open",
        InvestigationStatus::Closed => "Closed",
    }
}

/// The badge tone of an investigation status.
#[must_use]
pub fn investigation_status_tone(status: InvestigationStatus) -> Tone {
    match status {
        InvestigationStatus::Open => Tone::Info,
        InvestigationStatus::Closed => Tone::Neutral,
    }
}

/// How a step ended, in words. A step with no outcome is still running.
#[must_use]
pub fn outcome_label(outcome: Option<&StepOutcome>) -> &'static str {
    match outcome.map(|o| o.outcome) {
        None => "Running",
        Some(StepOutcomeKind::Success) => "Succeeded",
        Some(StepOutcomeKind::Refused) => "Refused",
        Some(StepOutcomeKind::Timeout) => "Timed out",
        Some(StepOutcomeKind::Failed) => "Failed",
        Some(StepOutcomeKind::Aborted) => "Aborted",
        Some(StepOutcomeKind::Interrupted) => "Interrupted",
    }
}

/// The badge tone of a step's outcome.
#[must_use]
pub fn outcome_tone(outcome: Option<&StepOutcome>) -> Tone {
    match outcome.map(|o| o.outcome) {
        None => Tone::Info,
        Some(StepOutcomeKind::Success) => Tone::Success,
        Some(StepOutcomeKind::Refused | StepOutcomeKind::Aborted) => Tone::Neutral,
        Some(StepOutcomeKind::Interrupted | StepOutcomeKind::Timeout) => Tone::Warn,
        Some(StepOutcomeKind::Failed) => Tone::Danger,
    }
}

/// The facts of a finished step after its outcome badge: the error code,
/// the rows kept and the duration, whichever it has, joined by ` · `.
/// Empty for a running step.
#[must_use]
pub fn outcome_facts(outcome: Option<&StepOutcome>) -> String {
    let Some(outcome) = outcome else {
        return String::new();
    };
    let mut facts = Vec::new();
    if let Some(code) = &outcome.code
        && let Ok(serde_json::Value::String(code)) = serde_json::to_value(code)
    {
        facts.push(code);
    }
    if let Some(rows) = outcome.rows {
        facts.push(format!("{rows} row{}", if rows == 1 { "" } else { "s" }));
    }
    if let Some(ms) = outcome.duration_ms {
        facts.push(fleet_ui::time::format_duration(ms));
    }
    facts.join(" · ")
}

/// Whether `id` has a captured result id's shape: 32 lowercase hex
/// digits. A reference of any other shape never reaches a URL.
#[must_use]
pub fn is_result_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The short form of a result id the page prints: its first eight
/// digits. The full id rides in the element's title.
#[must_use]
pub fn short_result_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// A cited row's name: "Row 7 of result fedcba98".
#[must_use]
pub fn evidence_label(reference: &EvidenceRef) -> String {
    format!(
        "Row {} of result {}",
        reference.row,
        short_result_id(&reference.result_id)
    )
}

/// One cited row, as the board's evidence table renders it: the column
/// names and the row's cells as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceRow {
    /// The column names, in page order.
    pub columns: Vec<String>,
    /// The row's cells as text, positional with `columns`.
    pub cells: Vec<String>,
}

/// One page of a captured result, as the evidence viewer and the board
/// render it: the column names in page order, and each row with its
/// captured row number and its cells as text.
///
/// A column keeps its display name as the server sent it, so two columns
/// of one name stay two columns. A cell is [`cell_text`] under its
/// column's #214 type: a timestamp in the grids' short form, an integer
/// in full however large, a null as `NULL`, a list as its elements. A
/// `_severity` cell is its token (`17` → `error`), as in Search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceRows {
    /// The column names, in page order.
    pub columns: Vec<String>,
    /// The page's rows, in captured order.
    pub rows: Vec<CapturedRow>,
}

/// One row of a captured result page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedRow {
    /// The 0-based captured row number: the page's offset plus the row's
    /// place in it. A keep and a finding cite a row by this number.
    pub number: u64,
    /// The row's cells as text, positional with the page's columns.
    pub cells: Vec<String>,
}

impl EvidenceRows {
    /// The row captured at `number`, as the board's one-row table shows
    /// it. `None` when this page does not hold it.
    #[must_use]
    pub fn row(&self, number: u64) -> Option<EvidenceRow> {
        let row = self.rows.iter().find(|r| r.number == number)?;
        Some(EvidenceRow {
            columns: self.columns.clone(),
            cells: row.cells.clone(),
        })
    }
}

/// A captured result page's rows, each numbered from the page's offset.
/// A `_severity` cell reads as its token, as the Search results table
/// shows it ([`severity_columns`], [`severity_display`]); a captured
/// result declares no `sev()` columns, so `_severity` is the only one.
#[must_use]
pub fn evidence_rows(page: &ResultPage) -> EvidenceRows {
    let columns = &page.page.columns;
    let severity = severity_columns(columns.iter().map(|c| c.name.as_str()), &[]);
    let first = u64::try_from(page.pagination.offset).unwrap_or(u64::MAX);
    EvidenceRows {
        columns: columns.iter().map(|c| c.name.clone()).collect(),
        rows: page
            .page
            .rows
            .iter()
            .zip(0u64..)
            .map_while(|(cells, i)| {
                Some(CapturedRow {
                    number: first.checked_add(i)?,
                    cells: cells
                        .iter()
                        .enumerate()
                        .map(|(c, value)| {
                            if severity.contains(&c) {
                                severity_display(value)
                            } else {
                                cell_text(columns.get(c).map(|c| &c.ty), value)
                            }
                        })
                        .collect(),
                })
            })
            .collect(),
    }
}

/// The Search path that reopens a captured result: its executed text and
/// the hull of its bounds through the shared codec, the same path the MCP
/// server links to (ADR-0027, ADR-0058). `None` for a result with no
/// bounds, or one the codec refuses, so the viewer offers no link rather
/// than one that would read back as something else.
#[must_use]
pub fn result_search_href(result: &CapturedResult) -> Option<String> {
    let (earliest, latest) = (result.earliest.as_deref()?, result.latest.as_deref()?);
    trawl_search_url::result_path::result_search_path(&result.executed_query, earliest, latest).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use trawl_api::investigations::InvestigationId;

    const ID: &str = "0123456789abcdef0123456789abcdef";
    const RESULT: &str = "fedcba9876543210fedcba9876543210";

    fn actor(name: &str) -> Actor {
        Actor {
            prefix: "AbCd_-12".into(),
            name: name.into(),
        }
    }

    fn investigation(seq: u64, title: &str) -> Investigation {
        Investigation {
            id: InvestigationId::parse(ID).unwrap(),
            title: title.into(),
            question: "q".into(),
            scope: String::new(),
            status: InvestigationStatus::Open,
            seq,
            created_at: "2026-10-24T09:00:00.000000Z".into(),
            updated_at: "2026-10-24T09:00:00.000000Z".into(),
            retain_until: "2026-10-31T09:00:00.000000Z".into(),
            retain_ceiling: "2026-11-23T09:00:00.000000Z".into(),
            created_by: actor("agent"),
            updated_by: actor("agent"),
        }
    }

    fn entry(seq: u64, updated_seq: u64, payload: EntryPayload) -> LogEntry {
        LogEntry {
            seq,
            updated_seq,
            actor: actor("agent"),
            at: "2026-10-24T09:00:00.000000Z".into(),
            payload,
        }
    }

    fn step(outcome: Option<StepOutcomeKind>) -> EntryPayload {
        EntryPayload::Step {
            executed: "service=shop".into(),
            bounds: None,
            request_id: "r".into(),
            query_id: None,
            purpose: None,
            outcome: outcome.map(|outcome| StepOutcome {
                outcome,
                code: None,
                result_id: None,
                rows: None,
                duration_ms: None,
            }),
        }
    }

    fn finding(statement: &str, status: FindingStatus) -> EntryPayload {
        EntryPayload::Finding {
            statement: statement.into(),
            status,
            references: vec![EvidenceRef {
                result_id: RESULT.into(),
                row: 3,
            }],
            counterevidence: vec![],
            remaining_question: None,
        }
    }

    fn note(text: &str) -> EntryPayload {
        EntryPayload::Note { text: text.into() }
    }

    fn read(seq: u64, entries: Vec<LogEntry>, through_seq: u64, more: bool) -> InvestigationRead {
        InvestigationRead {
            investigation: investigation(seq, &format!("t{seq}")),
            entries,
            through_seq,
            more,
        }
    }

    #[test]
    fn a_step_that_finished_later_replaces_its_running_copy_in_place() {
        let mut board = Board::new(read(
            2,
            vec![entry(1, 1, step(None)), entry(2, 2, note("n"))],
            2,
            false,
        ));
        assert_eq!(board.activity().len(), 2);
        assert_eq!(board.activity()[0].badge, Some(("Running", Tone::Info)));
        let merged = board.merge(read(
            3,
            vec![entry(1, 3, step(Some(StepOutcomeKind::Interrupted)))],
            3,
            false,
        ));
        // An update, not a new entry.
        assert_eq!(
            merged,
            Merged {
                added: 0,
                changed: true
            }
        );
        let steps = board.steps();
        assert_eq!(steps.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1]);
        let EntryPayload::Step { outcome, .. } = &steps[0].payload else {
            panic!("{steps:?}");
        };
        assert_eq!(outcome_label(outcome.as_ref()), "Interrupted");
        let activity = board.activity();
        assert_eq!(
            activity.iter().map(|a| a.seq).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(activity[0].badge, Some(("Interrupted", Tone::Warn)));
        assert_eq!(board.through_seq, 3);
        assert_eq!(board.investigation.title, "t3");
    }

    #[test]
    fn a_write_answer_updates_the_fields_but_never_the_cursor() {
        let mut board = Board::new(read(4, vec![entry(1, 1, note("a"))], 4, false));
        let mut closed = investigation(5, "t4");
        closed.status = InvestigationStatus::Closed;
        assert!(board.adopt(closed.clone()));
        assert_eq!(board.investigation.status, InvestigationStatus::Closed);
        assert_eq!((board.through_seq, board.more), (4, false));
        // The same copy again, or an older one, changes nothing.
        assert!(!board.adopt(closed));
        assert!(!board.adopt(investigation(3, "old")));
        assert_eq!(board.investigation.seq, 5);
        // The read that follows brings the status entry and moves on.
        let merged = board.merge(read(
            5,
            vec![entry(
                5,
                5,
                EntryPayload::Status {
                    status: InvestigationStatus::Closed,
                },
            )],
            5,
            false,
        ));
        assert_eq!(merged.added, 1);
        assert_eq!(board.through_seq, 5);
    }

    #[test]
    fn an_older_read_landing_late_moves_nothing_backwards() {
        let mut board = Board::new(read(
            5,
            vec![entry(1, 4, step(Some(StepOutcomeKind::Success)))],
            5,
            false,
        ));
        let merged = board.merge(read(3, vec![entry(1, 1, step(None))], 3, true));
        assert_eq!(merged, Merged::default());
        let EntryPayload::Step { outcome, .. } = &board.steps()[0].payload else {
            panic!();
        };
        assert_eq!(outcome_label(outcome.as_ref()), "Succeeded");
        assert_eq!((board.through_seq, board.more), (5, false));
        assert_eq!(board.investigation.seq, 5);
    }

    #[test]
    fn a_read_says_how_many_entries_it_added_and_whether_anything_changed() {
        let mut board = Board::new(read(2, vec![entry(1, 1, note("a"))], 2, false));
        // Nothing past the board: nothing changed, the page need not
        // render again.
        assert_eq!(board.merge(read(2, vec![], 2, false)), Merged::default());
        // The same entry again changes nothing either.
        assert_eq!(
            board.merge(read(2, vec![entry(1, 1, note("a"))], 2, false)),
            Merged::default()
        );
        // A change to the investigation alone is a change.
        let merged = board.merge(read(3, vec![], 3, false));
        assert_eq!(
            merged,
            Merged {
                added: 0,
                changed: true
            }
        );
        let merged = board.merge(read(
            5,
            vec![entry(4, 4, note("b")), entry(5, 5, note("c"))],
            5,
            false,
        ));
        assert_eq!(
            merged,
            Merged {
                added: 2,
                changed: true
            }
        );
    }

    #[test]
    fn pages_accumulate_and_more_follows_the_furthest_read() {
        let mut board = Board::new(read(4, vec![entry(1, 1, note("a"))], 1, true));
        assert!(board.more);
        board.merge(read(
            4,
            vec![entry(2, 2, note("b")), entry(3, 3, note("c"))],
            3,
            false,
        ));
        assert_eq!(board.activity().len(), 3);
        assert!(!board.more);
        assert_eq!(board.through_seq, 3);
    }

    #[test]
    fn findings_fold_their_latest_status_and_the_activity_holds_every_entry() {
        let mut overwrite = entry(
            4,
            4,
            EntryPayload::FindingStatus {
                finding_seq: 2,
                status: FindingStatus::Refuted,
            },
        );
        overwrite.actor = actor("jakub");
        let board = Board::new(read(
            5,
            vec![
                entry(1, 1, step(Some(StepOutcomeKind::Success))),
                entry(2, 2, finding("pool exhausted", FindingStatus::Hypothesis)),
                entry(3, 3, finding("upstream reset", FindingStatus::Supported)),
                overwrite,
                entry(5, 5, note("paged")),
            ],
            5,
            false,
        ));
        let findings = board.findings();
        assert_eq!(
            findings.iter().map(|f| f.seq).collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert_eq!(findings[0].status, FindingStatus::Refuted);
        assert_eq!(findings[0].status_by.as_ref().unwrap().name, "jakub");
        assert_eq!(findings[1].status, FindingStatus::Supported);
        assert!(findings[1].status_by.is_none());
        assert_eq!(board.latest_finding().unwrap().statement, "upstream reset");

        let activity = board.activity();
        assert_eq!(
            activity
                .iter()
                .map(|a| (a.seq, a.kind, a.badge.map(|b| b.0), a.text.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (1, "Step", Some("Succeeded"), NO_PURPOSE),
                (2, "Finding", Some("Hypothesis"), "pool exhausted"),
                (3, "Finding", Some("Supported"), "upstream reset"),
                (4, "Finding status", Some("Refuted"), "pool exhausted"),
                (5, "Note", None, "paged"),
            ]
        );
        assert_eq!(activity[3].actor.name, "jakub");
        assert_eq!(
            board.steps().iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![1]
        );
    }

    #[test]
    fn every_other_entry_reads_as_one_line() {
        let board = Board::new(read(
            5,
            vec![
                entry(
                    1,
                    1,
                    EntryPayload::Keep {
                        result_id: RESULT.into(),
                        rows: vec![0, 2],
                        reason: "first 502".into(),
                    },
                ),
                entry(
                    2,
                    2,
                    EntryPayload::Keep {
                        result_id: RESULT.into(),
                        rows: vec![7],
                        reason: "the reset".into(),
                    },
                ),
                entry(
                    3,
                    3,
                    EntryPayload::Status {
                        status: InvestigationStatus::Closed,
                    },
                ),
                entry(4, 4, EntryPayload::Title { title: "t".into() }),
                entry(
                    5,
                    5,
                    EntryPayload::Question {
                        question: "why".into(),
                    },
                ),
            ],
            5,
            false,
        ));
        assert_eq!(
            board
                .activity()
                .iter()
                .map(|a| (a.kind, a.text.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("Keep", "Kept rows 0, 2 of result fedcba98: first 502"),
                ("Keep", "Kept row 7 of result fedcba98: the reset"),
                ("Status", "Closed the investigation."),
                ("Title", "t"),
                ("Question", "why"),
            ]
        );
        assert!(board.steps().is_empty());
    }

    #[test]
    fn a_status_overwrite_for_an_unread_finding_changes_nothing() {
        let board = Board::new(read(
            2,
            vec![entry(
                2,
                2,
                EntryPayload::FindingStatus {
                    finding_seq: 1,
                    status: FindingStatus::Refuted,
                },
            )],
            2,
            true,
        ));
        assert!(board.findings().is_empty());
        assert!(board.latest_finding().is_none());
        let activity = board.activity();
        assert_eq!(activity.len(), 1);
        assert_eq!(activity[0].text, "Finding #1, not read yet.");
    }

    /// A finding at 1 written supported, 99 notes, a refutation of it at
    /// 101 and a new finding at 102, read 100 entries at a time.
    fn long_log() -> (InvestigationRead, InvestigationRead) {
        let mut first = vec![entry(
            1,
            1,
            finding("pool exhausted", FindingStatus::Supported),
        )];
        first.extend((2..=100).map(|seq| entry(seq, seq, note("n"))));
        let second = vec![
            entry(
                101,
                101,
                EntryPayload::FindingStatus {
                    finding_seq: 1,
                    status: FindingStatus::Refuted,
                },
            ),
            entry(
                102,
                102,
                finding("upstream reset", FindingStatus::Hypothesis),
            ),
        ];
        (read(102, first, 100, true), read(102, second, 102, false))
    }

    #[test]
    fn a_long_log_claims_no_finding_until_the_page_has_read_it_whole() {
        let (first, second) = long_log();
        let mut board = Board::new(first);
        // What the first page alone would claim: the old finding, still
        // supported.
        let early = board.latest_finding().unwrap();
        assert_eq!((early.seq, early.status), (1, FindingStatus::Supported));
        assert!(board.follow_up_due(1));
        assert_eq!(board.reach(1, true), Reach::Reading);

        board.merge(second);
        assert!(!board.follow_up_due(2));
        assert_eq!(board.reach(2, false), Reach::Whole);
        let latest = board.latest_finding().unwrap();
        assert_eq!(
            (latest.seq, latest.statement.as_str()),
            (102, "upstream reset")
        );
        let findings = board.findings();
        assert_eq!(
            findings
                .iter()
                .map(|f| (f.seq, f.status))
                .collect::<Vec<_>>(),
            vec![
                (1, FindingStatus::Refuted),
                (102, FindingStatus::Hypothesis)
            ]
        );
    }

    #[test]
    fn one_page_that_holds_the_whole_log_is_whole_at_once() {
        let board = Board::new(read(
            2,
            vec![entry(1, 1, note("a")), entry(2, 2, note("b"))],
            2,
            false,
        ));
        assert!(!board.follow_up_due(1));
        assert_eq!(board.reach(1, false), Reach::Whole);
        // A refresh in flight over a whole board keeps claiming it.
        assert_eq!(board.reach(0, true), Reach::Whole);
    }

    #[test]
    fn reading_stops_at_the_cap_and_says_which_entry_it_is_as_of() {
        let mut board = Board::new(read(4, vec![entry(1, 1, note("a"))], 1, true));
        for (pages, seq) in (2..=AUTO_PAGES).zip(2u64..) {
            assert!(board.follow_up_due(pages - 1));
            board.merge(read(4, vec![entry(seq, seq, note("a"))], seq, true));
        }
        assert!(!board.follow_up_due(AUTO_PAGES));
        assert_eq!(board.reach(AUTO_PAGES, false), Reach::AsOf(20));
        // Load more starts a new run: it reads again until the next cap.
        assert_eq!(board.reach(0, true), Reach::Reading);
        assert!(board.follow_up_due(1));
    }

    #[test]
    fn a_run_that_failed_with_entries_left_is_as_of_where_it_stopped() {
        let board = Board::new(read(9, vec![entry(1, 1, note("a"))], 1, true));
        assert_eq!(board.reach(1, false), Reach::AsOf(1));
    }

    #[test]
    fn every_outcome_has_words_and_a_tone() {
        assert_eq!(outcome_label(None), "Running");
        assert_eq!(outcome_tone(None), Tone::Info);
        for (kind, label) in [
            (StepOutcomeKind::Success, "Succeeded"),
            (StepOutcomeKind::Refused, "Refused"),
            (StepOutcomeKind::Timeout, "Timed out"),
            (StepOutcomeKind::Failed, "Failed"),
            (StepOutcomeKind::Aborted, "Aborted"),
            (StepOutcomeKind::Interrupted, "Interrupted"),
        ] {
            let outcome = StepOutcome {
                outcome: kind,
                code: None,
                result_id: None,
                rows: None,
                duration_ms: None,
            };
            assert_eq!(outcome_label(Some(&outcome)), label);
        }
    }

    #[test]
    fn outcome_facts_name_the_code_rows_and_duration() {
        let outcome: StepOutcome = serde_json::from_value(json!({
            "outcome": "refused", "code": "evidence_budget_exceeded"
        }))
        .unwrap();
        assert_eq!(outcome_facts(Some(&outcome)), "evidence_budget_exceeded");
        let success: StepOutcome = serde_json::from_value(json!({
            "outcome": "success", "result_id": RESULT, "rows": 1, "duration_ms": 250
        }))
        .unwrap();
        assert_eq!(
            outcome_facts(Some(&success)),
            format!("1 row · {}", fleet_ui::time::format_duration(250))
        );
        assert_eq!(outcome_facts(None), "");
    }

    #[test]
    fn only_a_result_id_shape_reaches_a_url() {
        assert!(is_result_id(RESULT));
        for bad in [
            "",
            "FEDCBA9876543210fedcba9876543210",
            "fedcba9876543210fedcba987654321",
            "../../../../../../../../etc/pass",
            "fedcba9876543210fedcba987654321?",
        ] {
            assert!(!is_result_id(bad), "{bad:?}");
        }
        assert_eq!(
            evidence_label(&EvidenceRef {
                result_id: RESULT.into(),
                row: 7
            }),
            "Row 7 of result fedcba98"
        );
        assert_eq!(short_result_id("ab"), "ab");
    }

    /// A page of the kept result, its `result` metadata and `page` from
    /// `columns`, `rows` and the window `offset`/`total`.
    fn page(
        columns: serde_json::Value,
        rows: serde_json::Value,
        offset: usize,
        total: usize,
    ) -> ResultPage {
        let returned = rows.as_array().map_or(0, Vec::len);
        serde_json::from_value(json!({
            "result": {
                "id": RESULT,
                "query": "service=shop",
                "executed_query": "service=shop earliest=\"2026-10-24T08:00:00.250000Z\" latest=\"2026-10-24T10:00:00.000000Z\"",
                "source": "live",
                "shape": "events",
                "earliest": "2026-10-24T08:00:00.250000Z",
                "latest": "2026-10-24T10:00:00.000000Z",
                "created_at": "2026-10-24T09:00:00.000000Z",
                "state": "kept",
                "columns": [],
                "captured_rows": total,
                "total_rows": total,
                "wire_bytes": 10,
                "file_bytes": 10,
                "truncated": false,
                "column_facts": [],
                "format_version": 1,
            },
            "positions": [],
            "page": serde_json::Value::Object(serde_json::Map::from_iter([
                ("columns".to_owned(), columns),
                ("rows".to_owned(), rows),
            ])),
            "pagination": {"limit": 3, "offset": offset, "returned": returned, "total": total},
        }))
        .unwrap()
    }

    #[test]
    fn a_page_numbers_its_rows_from_its_offset_and_keeps_every_typed_cell() {
        let page = page(
            json!([
                {"name": "_time", "type": "timestamp"},
                {"name": "host", "type": "string"},
                {"name": "host", "type": "string"},
                {"name": "bytes", "type": "integer"},
                {"name": "tags", "type": "list", "items": {"type": "string"}},
            ]),
            json!([
                [
                    "2026-10-24T09:01:02.500000Z",
                    "gw-1",
                    "gw-1.lan",
                    "18446744073709551615",
                    ["a", "b"]
                ],
                [
                    "2026-10-24T09:01:03.000000Z",
                    null,
                    "gw-2.lan",
                    "-9223372036854775808",
                    []
                ],
                ["2026-10-24T09:01:04.000000Z", "gw-3", null, null, null],
            ]),
            3,
            7,
        );
        let shown = evidence_rows(&page);
        // Two columns of one display name stay two columns.
        assert_eq!(
            shown.columns,
            vec!["_time", "host", "host", "bytes", "tags"]
        );
        assert_eq!(
            shown.rows.iter().map(|r| r.number).collect::<Vec<_>>(),
            vec![3, 4, 5]
        );
        assert_eq!(
            shown.rows[0].cells,
            vec![
                "2026-10-24 09:01:02.5",
                "gw-1",
                "gw-1.lan",
                // Past 2^53, in full.
                "18446744073709551615",
                "[a, b]",
            ]
        );
        assert_eq!(
            shown.rows[1].cells,
            vec![
                "2026-10-24 09:01:03",
                "NULL",
                "gw-2.lan",
                "-9223372036854775808",
                "[]"
            ]
        );
        assert_eq!(shown.rows[2].cells[2..], ["NULL", "NULL", "NULL"]);
    }

    /// A captured `_severity` reads as Search's table reads it: the token,
    /// never the number (ADR-0013 §9). A bare `severity` column is sender
    /// data and keeps its value.
    #[test]
    fn a_severity_cell_shows_its_token_as_search_does() {
        let page = page(
            json!([
                {"name": "_time", "type": "timestamp"},
                {"name": "_severity", "type": "integer"},
                {"name": "severity", "type": "integer"},
            ]),
            json!([
                ["2026-10-24T09:01:02.000000Z", "17", "17"],
                ["2026-10-24T09:01:03.000000Z", "9", "9"],
                ["2026-10-24T09:01:04.000000Z", "99", null],
                ["2026-10-24T09:01:05.000000Z", null, "13"],
            ]),
            0,
            4,
        );
        let shown = evidence_rows(&page);
        let cells: Vec<_> = shown.rows.iter().map(|r| &r.cells[1..]).collect();
        assert_eq!(
            cells,
            [
                ["error", "17"],
                ["info", "9"],
                // Off the ladder, or absent: the raw value, never a guess.
                ["99", "NULL"],
                ["NULL", "13"],
            ]
        );
        assert_eq!(shown.row(0).unwrap().cells[1], "error");
    }

    #[test]
    fn the_board_shows_a_cited_row_only_from_a_page_that_holds_it() {
        let page = page(
            json!([
                {"name": "_time", "type": "timestamp"},
                {"name": "status", "type": "integer"},
            ]),
            json!([["2026-10-24T09:01:02.000000Z", "502"]]),
            7,
            9,
        );
        let shown = evidence_rows(&page).row(7).expect("the cited row");
        assert_eq!(shown.columns, vec!["_time", "status"]);
        assert_eq!(shown.cells, vec!["2026-10-24 09:01:02", "502"]);
        assert!(evidence_rows(&page).row(8).is_none());

        let mut empty = page;
        empty.page.rows.clear();
        assert!(evidence_rows(&empty).rows.is_empty());
        assert!(evidence_rows(&empty).row(7).is_none());
    }

    #[test]
    fn open_in_search_is_the_shared_codec_path_and_absent_without_bounds() {
        let mut page = page(json!([]), json!([]), 0, 0);
        let href = result_search_href(&page.result).expect("a bounded result has a path");
        assert_eq!(
            Ok(href),
            trawl_search_url::result_path::result_search_path(
                &page.result.executed_query,
                "2026-10-24T08:00:00.250000Z",
                "2026-10-24T10:00:00.000000Z",
            )
        );
        page.result.latest = None;
        assert_eq!(result_search_href(&page.result), None);
        page.result.latest = Some("not an instant".into());
        assert_eq!(result_search_href(&page.result), None);
    }
}
