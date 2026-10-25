// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The investigation page's reading of one investigation (ADR-0059).
//!
//! A read answers the entries changed after a `since` sequence, in
//! `updated_seq` order, bounded, with the `through_seq` the next read
//! starts from. [`Board`] folds those reads into what the page shows:
//! every entry once, at its latest change, keyed by the `seq` that
//! created it. Load more and Refresh are the same read, `since` the
//! board's `through_seq`, so a step that gained its outcome after the
//! page loaded replaces its running copy in place.
//!
//! The page has two readings of the log. Findings are the claims, each
//! with its current status: a `finding_status` entry overwrites the
//! status of the finding whose `seq` it names. Everything else (steps,
//! notes, keeps, status, title and question changes, and the finding
//! status changes themselves) is the log, in `seq` order, so a note sits
//! between the steps it was written between.
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
use trawl_api::results::ResultPage;

use crate::results_layout::cell_text;

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
        board.merge(read);
        board
    }

    /// Fold a later read in. An entry replaces the copy already held only
    /// when it is at least as new, and the investigation's fields only
    /// when its `seq` is at least the one held, so a read that crossed a
    /// newer one in flight cannot move the board backwards.
    pub fn merge(&mut self, read: InvestigationRead) {
        if read.investigation.seq >= self.investigation.seq {
            self.investigation = read.investigation;
        }
        for entry in read.entries {
            let newer = self
                .entries
                .get(&entry.seq)
                .is_none_or(|held| entry.updated_seq >= held.updated_seq);
            if newer {
                self.entries.insert(entry.seq, entry);
            }
        }
        if read.through_seq >= self.through_seq {
            self.through_seq = read.through_seq;
            self.more = read.more;
        }
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

    /// The log: every entry that is not a finding, in `seq` order.
    #[must_use]
    pub fn log(&self) -> Vec<LogEntry> {
        self.entries
            .values()
            .filter(|entry| !matches!(entry.payload, EntryPayload::Finding { .. }))
            .cloned()
            .collect()
    }
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

/// One cited row, as the evidence table renders it: the column names and
/// the row's cells as text. `None` when the page holds no row at the
/// offset it was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceRow {
    /// The column names, in page order.
    pub columns: Vec<String>,
    /// The row's cells as text, positional with `columns`.
    pub cells: Vec<String>,
}

/// The cited row out of a one-row page read at `row`.
#[must_use]
pub fn evidence_row(page: &ResultPage, row: u64) -> Option<EvidenceRow> {
    if u64::try_from(page.pagination.offset).ok() != Some(row) {
        return None;
    }
    let cells = page.page.rows.first()?;
    Some(EvidenceRow {
        columns: page.page.columns.iter().map(|c| c.name.clone()).collect(),
        cells: cells
            .iter()
            .enumerate()
            .map(|(i, value)| cell_text(page.page.columns.get(i).map(|c| &c.ty), value))
            .collect(),
    })
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
        assert_eq!(board.log().len(), 2);
        board.merge(read(
            3,
            vec![entry(1, 3, step(Some(StepOutcomeKind::Interrupted)))],
            3,
            false,
        ));
        let log = board.log();
        assert_eq!(log.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 2]);
        let EntryPayload::Step { outcome, .. } = &log[0].payload else {
            panic!("{log:?}");
        };
        assert_eq!(outcome_label(outcome.as_ref()), "Interrupted");
        assert_eq!(board.through_seq, 3);
        assert_eq!(board.investigation.title, "t3");
    }

    #[test]
    fn an_older_read_landing_late_moves_nothing_backwards() {
        let mut board = Board::new(read(
            5,
            vec![entry(1, 4, step(Some(StepOutcomeKind::Success)))],
            5,
            false,
        ));
        board.merge(read(3, vec![entry(1, 1, step(None))], 3, true));
        let EntryPayload::Step { outcome, .. } = &board.log()[0].payload else {
            panic!();
        };
        assert_eq!(outcome_label(outcome.as_ref()), "Succeeded");
        assert_eq!((board.through_seq, board.more), (5, false));
        assert_eq!(board.investigation.seq, 5);
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
        assert_eq!(board.log().len(), 3);
        assert!(!board.more);
        assert_eq!(board.through_seq, 3);
    }

    #[test]
    fn findings_fold_their_latest_status_and_stay_out_of_the_log() {
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
        assert_eq!(
            board.log().iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![1, 4, 5]
        );
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
        assert_eq!(board.log().len(), 1);
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

    #[test]
    fn an_evidence_row_is_the_page_row_at_the_cited_offset() {
        let page: ResultPage = serde_json::from_value(json!({
            "result": {
                "id": RESULT,
                "query": "service=shop",
                "executed_query": "service=shop",
                "source": "live",
                "shape": "events",
                "created_at": "2026-10-24T09:00:00.000000Z",
                "state": "kept",
                "columns": [
                    {"name": "_time", "type": "timestamp", "stored_as": "native"},
                    {"name": "status", "type": "integer", "stored_as": "native"},
                ],
                "captured_rows": 9,
                "total_rows": 9,
                "wire_bytes": 10,
                "file_bytes": 10,
                "truncated": false,
                "column_facts": [
                    {"nulls": 0, "distinct": 1, "distinct_capped": false},
                    {"nulls": 0, "distinct": 1, "distinct_capped": false},
                ],
                "format_version": 1,
            },
            "positions": [0, 1],
            "page": {
                "columns": [
                    {"name": "_time", "type": "timestamp"},
                    {"name": "status", "type": "integer"},
                ],
                "rows": [["2026-10-24T09:01:02.000000Z", "502"]],
            },
            "pagination": {"limit": 1, "offset": 7, "returned": 1, "total": 9},
        }))
        .unwrap();
        let shown = evidence_row(&page, 7).expect("the cited row");
        assert_eq!(shown.columns, vec!["_time", "status"]);
        assert_eq!(shown.cells.len(), 2);
        assert_eq!(shown.cells[1], "502");
        assert!(evidence_row(&page, 8).is_none());

        let mut empty = page;
        empty.page.rows.clear();
        assert!(evidence_row(&empty, 7).is_none());
    }
}
