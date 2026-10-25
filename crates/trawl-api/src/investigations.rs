// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Wire types for investigations (ADR-0059).
//!
//! An investigation is installation-owned: `investigation_read` with
//! `query` reads every one, and `investigation_write` writes to any of
//! them. Its log is append-only and sequenced. Every write advances the
//! investigation's `seq` by one, and every entry records the `seq` that
//! created it and the `updated_seq` of its last change (a step gains its
//! outcome after it was appended). A reader asks for the entries changed
//! since a `seq` it saw.
//!
//! Appends (keep, finding, note) carry no expected sequence. Overwrites
//! (title, question, status, finding status, retention) carry the `seq`
//! they observed and fail with 409 `sequence_conflict` and a
//! [`SequenceConflict`] when it moved.
//!
//! Timestamps are RFC 3339 UTC with microseconds, as in
//! [`crate::results`]. Text bounds are bytes of UTF-8 and are the
//! `pub const`s below; the server refuses a request over any of them.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::ErrorCode;

// -- bounds ------------------------------------------------------------------

/// Most bytes of an investigation's title.
pub const TITLE_MAX_BYTES: usize = 200;
/// Most bytes of an investigation's question.
pub const QUESTION_MAX_BYTES: usize = 2048;
/// Most bytes of an investigation's scope text.
pub const SCOPE_MAX_BYTES: usize = 2048;
/// Most bytes of a note.
pub const NOTE_MAX_BYTES: usize = 4096;
/// Most bytes of a finding's statement.
pub const FINDING_STATEMENT_MAX_BYTES: usize = 4096;
/// Most bytes of a finding's remaining question.
pub const REMAINING_QUESTION_MAX_BYTES: usize = 1024;
/// Most rows a finding cites as references.
pub const FINDING_MAX_REFERENCES: usize = 32;
/// Most rows a finding cites as counterevidence.
pub const FINDING_MAX_COUNTEREVIDENCE: usize = 32;
/// Most row numbers one keep records.
pub const KEEP_MAX_ROWS: usize = 200;
/// Most bytes of a keep's reason.
pub const KEEP_REASON_MAX_BYTES: usize = 1024;
/// Most bytes of the purpose an attached search states for its step.
pub const PURPOSE_MAX_BYTES: usize = 1024;

// -- identity ----------------------------------------------------------------

/// An investigation id: 32 lowercase hex digits, a random 128-bit value the
/// MCP server mints for a create, never the model (ADR-0059). The shape is
/// the captured result id's.
///
/// It decodes only in that shape, so an id that reaches a route or a store
/// is one.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct InvestigationId(String);

/// The text is not 32 lowercase hex digits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidInvestigationId;

impl fmt::Display for InvalidInvestigationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an investigation id is 32 lowercase hex digits")
    }
}

impl std::error::Error for InvalidInvestigationId {}

impl InvestigationId {
    /// The id's length in hex digits.
    pub const LEN: usize = 32;

    /// Parse `text` as an id.
    ///
    /// # Errors
    /// When `text` is not exactly 32 lowercase hex digits.
    pub fn parse(text: &str) -> Result<Self, InvalidInvestigationId> {
        let valid = text.len() == Self::LEN
            && text
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if valid {
            Ok(Self(text.to_owned()))
        } else {
            Err(InvalidInvestigationId)
        }
    }

    /// The id's text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for InvestigationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for InvestigationId {
    type Err = InvalidInvestigationId;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl TryFrom<String> for InvestigationId {
    type Error = InvalidInvestigationId;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text)
    }
}

impl From<InvestigationId> for String {
    fn from(id: InvestigationId) -> Self {
        id.0
    }
}

/// Who wrote something: the key id and the key's name when it wrote. The
/// name is a snapshot, so it still reads after the key is revoked.
/// Attribution only: it grants nothing (ADR-0059).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Actor {
    /// The fleet key id.
    pub key_id: i64,
    /// The key's name at the time of the write.
    pub name: String,
}

// -- enums -------------------------------------------------------------------

/// An investigation's status. A closed investigation accepts nothing new
/// until a writer reopens it; work admitted before the close still
/// finishes and records its outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvestigationStatus {
    /// Accepting attached work, keeps, findings and notes.
    Open,
    /// Read-only until reopened.
    Closed,
}

/// A finding's status: the author's assertion, visibly attributed. trawl
/// checks that a finding's references exist and are readable, not that
/// they prove it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingStatus {
    /// Proposed, not yet supported or refuted.
    Hypothesis,
    /// The evidence supports it.
    Supported,
    /// The evidence refutes it.
    Refuted,
    /// The evidence does not settle it.
    Unresolved,
}

/// How a step's work ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepOutcomeKind {
    /// The work finished and its result is kept.
    Success,
    /// The work was refused after admission, such as by a budget.
    Refused,
    /// The work ran past its deadline.
    Timeout,
    /// The work failed.
    Failed,
    /// The client went away, or the work was cancelled.
    Aborted,
    /// trawld stopped while the work ran; boot recorded this.
    Interrupted,
}

/// The kind of a log entry, as [`EntryPayload`] tags it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    /// Work the server ran for the investigation.
    Step,
    /// Rows circled in kept evidence, with a reason.
    Keep,
    /// A statement with a status and cited rows.
    Finding,
    /// A finding's status, overwritten.
    FindingStatus,
    /// Free text.
    Note,
    /// The investigation closed or reopened.
    Status,
    /// The investigation's title, overwritten.
    Title,
    /// The investigation's question, overwritten.
    Question,
}

// -- the investigation -------------------------------------------------------

/// An investigation's own fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Investigation {
    /// The id.
    pub id: InvestigationId,
    /// A short title.
    pub title: String,
    /// The question the investigation answers.
    pub question: String,
    /// What the investigation covers, as free text. Empty when none was
    /// given.
    #[serde(default)]
    pub scope: String,
    /// Open or closed.
    pub status: InvestigationStatus,
    /// The sequence of the latest write.
    pub seq: u64,
    /// When it was created.
    pub created_at: String,
    /// When it last changed.
    pub updated_at: String,
    /// When its evidence files stop being kept, unless it changes again.
    pub retain_until: String,
    /// The latest `retain_until` can slide to: 30 days after creation by
    /// default, raised only by `server_manage`.
    pub retain_ceiling: String,
    /// Who created it.
    pub created_by: Actor,
    /// Who changed it last.
    pub updated_by: Actor,
}

/// One row of `GET /api/v1/investigations`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvestigationSummary {
    /// The id.
    pub id: InvestigationId,
    /// The title.
    pub title: String,
    /// Open or closed.
    pub status: InvestigationStatus,
    /// The sequence of the latest write.
    pub seq: u64,
    /// When it was created.
    pub created_at: String,
    /// When it last changed.
    pub updated_at: String,
    /// Who created it.
    pub created_by: Actor,
    /// Who changed it last.
    pub updated_by: Actor,
}

/// Response body for `GET /api/v1/investigations`: open investigations
/// first, then the rest, each group most recently changed first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvestigationList {
    /// The investigations.
    pub investigations: Vec<InvestigationSummary>,
}

// -- the log -----------------------------------------------------------------

/// One row of a captured result: the result id and its 0-based row number
/// (ADR-0057). A citation, never an event identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceRef {
    /// The captured result's id.
    pub result_id: String,
    /// The 0-based row number in that result.
    pub row: u64,
}

/// The resolved time bounds a step ran inside, half-open
/// `[earliest, latest)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepBounds {
    /// The inclusive start.
    pub earliest: String,
    /// The exclusive end.
    pub latest: String,
}

/// How a step ended, written once when its work ends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepOutcome {
    /// How the work ended.
    pub outcome: StepOutcomeKind,
    /// The error code the work ended with, for an outcome other than
    /// success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<ErrorCode>,
    /// The captured result the work kept, for a success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_id: Option<String>,
    /// Rows the result holds, for a success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u64>,
    /// How long the work ran, in milliseconds, when it ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// What a log entry records, tagged by `kind`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EntryPayload {
    /// Work the server ran for the investigation, appended at admission.
    /// The server writes it; no client sends one.
    Step {
        /// The text that ran, with the resolved bounds spliced in.
        executed: String,
        /// The resolved bounds. Absent for a stored source, which has
        /// none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bounds: Option<StepBounds>,
        /// The request that admitted the work.
        request_id: String,
        /// The query id of the ADR-0046 facts, once the work has one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query_id: Option<u64>,
        /// Why the client ran it, as the client stated.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        purpose: Option<String>,
        /// How it ended. Absent while the work runs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        outcome: Option<StepOutcome>,
    },
    /// Rows circled in kept evidence.
    Keep {
        /// The kept result.
        result_id: String,
        /// The 0-based row numbers kept.
        rows: Vec<u64>,
        /// Why.
        reason: String,
    },
    /// A statement with a status and the rows it cites.
    Finding {
        /// The statement.
        statement: String,
        /// The status the finding was written with. A later
        /// `finding_status` entry overwrites it.
        status: FindingStatus,
        /// Rows that support the statement.
        references: Vec<EvidenceRef>,
        /// Rows that count against it.
        #[serde(default)]
        counterevidence: Vec<EvidenceRef>,
        /// What the evidence leaves open.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remaining_question: Option<String>,
    },
    /// A finding's status, overwritten.
    FindingStatus {
        /// The `seq` of the finding's own entry, which is its id.
        finding_seq: u64,
        /// The new status.
        status: FindingStatus,
    },
    /// Free text.
    Note {
        /// The text.
        text: String,
    },
    /// The investigation closed or reopened.
    Status {
        /// The new status.
        status: InvestigationStatus,
    },
    /// The investigation's title, overwritten.
    Title {
        /// The new title.
        title: String,
    },
    /// The investigation's question, overwritten.
    Question {
        /// The new question.
        question: String,
    },
}

impl EntryPayload {
    /// The entry's kind.
    #[must_use]
    pub fn kind(&self) -> EntryKind {
        match self {
            Self::Step { .. } => EntryKind::Step,
            Self::Keep { .. } => EntryKind::Keep,
            Self::Finding { .. } => EntryKind::Finding,
            Self::FindingStatus { .. } => EntryKind::FindingStatus,
            Self::Note { .. } => EntryKind::Note,
            Self::Status { .. } => EntryKind::Status,
            Self::Title { .. } => EntryKind::Title,
            Self::Question { .. } => EntryKind::Question,
        }
    }
}

/// One entry of an investigation's log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    /// The sequence that created the entry; a finding's id.
    pub seq: u64,
    /// The sequence of the entry's latest change: `seq`, or later for a
    /// step whose outcome was written after it was appended.
    pub updated_seq: u64,
    /// Who wrote the entry. For a step, the key whose request admitted the
    /// work.
    pub actor: Actor,
    /// When the entry was created.
    pub at: String,
    /// What it records.
    pub payload: EntryPayload,
}

// -- requests ----------------------------------------------------------------

/// Request body for `PUT /api/v1/investigations/{id}`: create if absent.
/// The same creator sending the same fields gets the existing
/// investigation back; anything else is 409 `investigation_exists`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateInvestigation {
    /// A short title, at most [`TITLE_MAX_BYTES`].
    pub title: String,
    /// The question, at most [`QUESTION_MAX_BYTES`].
    pub question: String,
    /// What it covers, at most [`SCOPE_MAX_BYTES`]. Absent is empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub scope: String,
}

/// Request body for `POST /api/v1/investigations/{id}/entries`: one
/// append, tagged by `kind`. Appends carry no expected sequence. A
/// `seen_seq` asks for the other actors' entries changed since it, in the
/// response's `activity`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AppendEntry {
    /// Circle rows of a captured result. A row of the caller's own
    /// temporary result promotes the whole result to kept evidence.
    Keep {
        /// The result.
        result_id: String,
        /// The 0-based row numbers, at most [`KEEP_MAX_ROWS`].
        rows: Vec<u64>,
        /// Why, at most [`KEEP_REASON_MAX_BYTES`].
        reason: String,
        /// The latest `seq` the caller has seen.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seen_seq: Option<u64>,
    },
    /// Record a finding.
    Finding {
        /// The statement, at most [`FINDING_STATEMENT_MAX_BYTES`].
        statement: String,
        /// The author's assertion.
        status: FindingStatus,
        /// Rows that support it, at most [`FINDING_MAX_REFERENCES`].
        references: Vec<EvidenceRef>,
        /// Rows against it, at most [`FINDING_MAX_COUNTEREVIDENCE`].
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        counterevidence: Vec<EvidenceRef>,
        /// What it leaves open, at most [`REMAINING_QUESTION_MAX_BYTES`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remaining_question: Option<String>,
        /// The latest `seq` the caller has seen.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seen_seq: Option<u64>,
    },
    /// Write a note.
    Note {
        /// The text, at most [`NOTE_MAX_BYTES`].
        text: String,
        /// The latest `seq` the caller has seen.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seen_seq: Option<u64>,
    },
}

impl AppendEntry {
    /// The `seq` the caller has seen, if it sent one.
    #[must_use]
    pub fn seen_seq(&self) -> Option<u64> {
        match self {
            Self::Keep { seen_seq, .. }
            | Self::Finding { seen_seq, .. }
            | Self::Note { seen_seq, .. } => *seen_seq,
        }
    }
}

/// Request body for `PATCH /api/v1/investigations/{id}`: overwrite the
/// title, the question or the status. Each field present advances the
/// sequence by one and records its own entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchInvestigation {
    /// The `seq` the caller observed.
    pub expected_seq: u64,
    /// A new title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// A new question.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    /// Close or reopen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<InvestigationStatus>,
}

/// Request body for `PUT /api/v1/investigations/{id}/findings/{seq}/status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FindingStatusUpdate {
    /// The `seq` the caller observed.
    pub expected_seq: u64,
    /// The new status.
    pub status: FindingStatus,
}

/// Request body for `PUT /api/v1/investigations/{id}/retention`, which
/// needs `server_manage`: keep the evidence files until `retain_until`,
/// past the ceiling if need be.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionExtension {
    /// The `seq` the caller observed.
    pub expected_seq: u64,
    /// The new deadline, as an RFC 3339 instant.
    pub retain_until: String,
}

/// The investigation a capture or a correlation read attaches to, in its
/// request body. Attaching needs `investigation_write` and an open
/// investigation; the server appends the step at admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attach {
    /// The investigation.
    pub id: InvestigationId,
    /// The latest `seq` the caller has seen, for the response's
    /// `activity`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seen_seq: Option<u64>,
    /// Why the client runs it, at most [`PURPOSE_MAX_BYTES`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
}

// -- responses ---------------------------------------------------------------

/// Response body for `GET /api/v1/investigations/{id}?since=<seq>`: the
/// investigation and the entries changed after `since`, in `updated_seq`
/// order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvestigationRead {
    /// The investigation.
    pub investigation: Investigation,
    /// The entries, bounded.
    pub entries: Vec<LogEntry>,
    /// The `updated_seq` the entries reach: the next read's `since`.
    pub through_seq: u64,
    /// Whether entries past `through_seq` remain.
    pub more: bool,
}

/// The entries other actors changed after the caller's `seen_seq`, carried
/// by an attached or append response. Bounded: `more` says a read
/// `since=<through_seq>` has the rest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activity {
    /// The entries, in `updated_seq` order.
    pub entries: Vec<LogEntry>,
    /// The `updated_seq` the entries reach.
    pub through_seq: u64,
    /// Whether entries past `through_seq` remain.
    pub more: bool,
}

/// Response body for an append.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppendResponse {
    /// The entry written.
    pub entry: LogEntry,
    /// The other actors' entries since the request's `seen_seq`. Absent
    /// when the request sent none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<Activity>,
}

/// The `conflict` member of a 409 `sequence_conflict` error: the sequence
/// the request expected and the current one. The client re-reads and
/// retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SequenceConflict {
    /// The `expected_seq` the request carried.
    pub expected: u64,
    /// The investigation's `seq` now.
    pub current: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ID: &str = "0123456789abcdef0123456789abcdef";

    fn round_trip<T>(value: &T) -> T
    where
        T: Serialize + for<'de> Deserialize<'de>,
    {
        let text = serde_json::to_string(value).expect("encodes");
        serde_json::from_str(&text).expect("decodes")
    }

    fn agent() -> Actor {
        Actor {
            key_id: 7,
            name: "agent".into(),
        }
    }

    fn investigation() -> Investigation {
        Investigation {
            id: InvestigationId::parse(ID).unwrap(),
            title: "502s on checkout".into(),
            question: "Why does checkout return 502 since 09:00?".into(),
            scope: String::new(),
            status: InvestigationStatus::Open,
            seq: 4,
            created_at: "2026-10-24T09:00:00.000000Z".into(),
            updated_at: "2026-10-24T09:05:00.000000Z".into(),
            retain_until: "2026-10-31T09:05:00.000000Z".into(),
            retain_ceiling: "2026-11-23T09:00:00.000000Z".into(),
            created_by: agent(),
            updated_by: Actor {
                key_id: 3,
                name: "jakub".into(),
            },
        }
    }

    #[test]
    fn an_id_is_exactly_32_lowercase_hex_digits() {
        assert_eq!(InvestigationId::parse(ID).unwrap().as_str(), ID);
        for bad in [
            "",
            "0123456789abcdef0123456789abcde",
            "0123456789abcdef0123456789abcdef0",
            "0123456789ABCDEF0123456789abcdef",
            "0123456789abcdeg0123456789abcdef",
            " 123456789abcdef0123456789abcdef",
            "../../../../../../../../etc/pass",
        ] {
            assert!(InvestigationId::parse(bad).is_err(), "{bad:?}");
            assert!(
                serde_json::from_value::<InvestigationId>(json!(bad)).is_err(),
                "{bad:?}"
            );
        }
        // Multi-byte text of 32 bytes is refused by the digit check.
        assert!(InvestigationId::parse("ąąąąąąąąąąąąąąąą").is_err());
        let id: InvestigationId = serde_json::from_value(json!(ID)).unwrap();
        assert_eq!(serde_json::to_value(&id).unwrap(), json!(ID));
        assert_eq!(id.to_string(), ID);
        assert_eq!(ID.parse::<InvestigationId>().unwrap(), id);
    }

    #[test]
    fn an_investigation_has_a_golden_spelling() {
        let golden = json!({
            "id": ID,
            "title": "502s on checkout",
            "question": "Why does checkout return 502 since 09:00?",
            "scope": "",
            "status": "open",
            "seq": 4,
            "created_at": "2026-10-24T09:00:00.000000Z",
            "updated_at": "2026-10-24T09:05:00.000000Z",
            "retain_until": "2026-10-31T09:05:00.000000Z",
            "retain_ceiling": "2026-11-23T09:00:00.000000Z",
            "created_by": {"key_id": 7, "name": "agent"},
            "updated_by": {"key_id": 3, "name": "jakub"},
        });
        assert_eq!(serde_json::to_value(investigation()).unwrap(), golden);
        assert_eq!(
            serde_json::from_value::<Investigation>(golden).unwrap(),
            investigation()
        );
    }

    /// The step payloads with their wire spelling.
    fn step_cases() -> Vec<(EntryPayload, serde_json::Value)> {
        vec![
            (
                EntryPayload::Step {
                    executed: "_service=checkout earliest=\"a\" latest=\"b\"".into(),
                    bounds: Some(StepBounds {
                        earliest: "2026-10-24T08:00:00.000000Z".into(),
                        latest: "2026-10-24T09:00:00.000000Z".into(),
                    }),
                    request_id: "req-1".into(),
                    query_id: Some(41),
                    purpose: Some("find the first 502".into()),
                    outcome: Some(StepOutcome {
                        outcome: StepOutcomeKind::Success,
                        code: None,
                        result_id: Some("fedcba9876543210fedcba9876543210".into()),
                        rows: Some(120),
                        duration_ms: Some(250),
                    }),
                },
                json!({
                    "kind": "step",
                    "executed": "_service=checkout earliest=\"a\" latest=\"b\"",
                    "bounds": {
                        "earliest": "2026-10-24T08:00:00.000000Z",
                        "latest": "2026-10-24T09:00:00.000000Z",
                    },
                    "request_id": "req-1",
                    "query_id": 41,
                    "purpose": "find the first 502",
                    "outcome": {
                        "outcome": "success",
                        "result_id": "fedcba9876543210fedcba9876543210",
                        "rows": 120,
                        "duration_ms": 250,
                    },
                }),
            ),
            (
                EntryPayload::Step {
                    executed: "from saved nightly".into(),
                    bounds: None,
                    request_id: "req-2".into(),
                    query_id: None,
                    purpose: None,
                    outcome: None,
                },
                json!({"kind": "step", "executed": "from saved nightly", "request_id": "req-2"}),
            ),
        ]
    }

    /// Every payload kind with its wire spelling.
    fn entry_cases() -> Vec<(EntryPayload, serde_json::Value)> {
        let reference = EvidenceRef {
            result_id: "fedcba9876543210fedcba9876543210".into(),
            row: 7,
        };
        let mut cases = step_cases();
        cases.extend([
            (
                EntryPayload::Keep {
                    result_id: reference.result_id.clone(),
                    rows: vec![7, 9],
                    reason: "the first 502 and its upstream reset".into(),
                },
                json!({
                    "kind": "keep",
                    "result_id": "fedcba9876543210fedcba9876543210",
                    "rows": [7, 9],
                    "reason": "the first 502 and its upstream reset",
                }),
            ),
            (
                EntryPayload::Finding {
                    statement: "the upstream pool is exhausted".into(),
                    status: FindingStatus::Supported,
                    references: vec![reference.clone()],
                    counterevidence: vec![],
                    remaining_question: Some("why at 09:00?".into()),
                },
                json!({
                    "kind": "finding",
                    "statement": "the upstream pool is exhausted",
                    "status": "supported",
                    "references": [{"result_id": "fedcba9876543210fedcba9876543210", "row": 7}],
                    "counterevidence": [],
                    "remaining_question": "why at 09:00?",
                }),
            ),
            (
                EntryPayload::FindingStatus {
                    finding_seq: 5,
                    status: FindingStatus::Refuted,
                },
                json!({"kind": "finding_status", "finding_seq": 5, "status": "refuted"}),
            ),
            (
                EntryPayload::Note {
                    text: "paged the on-call".into(),
                },
                json!({"kind": "note", "text": "paged the on-call"}),
            ),
            (
                EntryPayload::Status {
                    status: InvestigationStatus::Closed,
                },
                json!({"kind": "status", "status": "closed"}),
            ),
            (
                EntryPayload::Title {
                    title: "pool exhaustion".into(),
                },
                json!({"kind": "title", "title": "pool exhaustion"}),
            ),
            (
                EntryPayload::Question {
                    question: "What exhausts the pool?".into(),
                },
                json!({"kind": "question", "question": "What exhausts the pool?"}),
            ),
        ]);
        cases
    }

    #[test]
    fn every_entry_kind_has_a_golden_spelling_and_round_trips() {
        for (payload, golden) in entry_cases() {
            assert_eq!(serde_json::to_value(&payload).unwrap(), golden);
            assert_eq!(
                serde_json::from_value::<EntryPayload>(golden.clone()).unwrap(),
                payload
            );
            assert_eq!(
                golden["kind"],
                serde_json::to_value(payload.kind()).unwrap()
            );
            let entry = LogEntry {
                seq: 5,
                updated_seq: 6,
                actor: agent(),
                at: "2026-10-24T09:05:00.000000Z".into(),
                payload,
            };
            assert_eq!(round_trip(&entry), entry);
        }
    }

    #[test]
    fn a_failed_step_outcome_carries_its_code() {
        let outcome = StepOutcome {
            outcome: StepOutcomeKind::Refused,
            code: Some(ErrorCode::EvidenceBudgetExceeded),
            result_id: None,
            rows: None,
            duration_ms: None,
        };
        assert_eq!(
            serde_json::to_value(&outcome).unwrap(),
            json!({"outcome": "refused", "code": "evidence_budget_exceeded"})
        );
        assert_eq!(round_trip(&outcome), outcome);
        for (kind, wire) in [
            (StepOutcomeKind::Success, "success"),
            (StepOutcomeKind::Refused, "refused"),
            (StepOutcomeKind::Timeout, "timeout"),
            (StepOutcomeKind::Failed, "failed"),
            (StepOutcomeKind::Aborted, "aborted"),
            (StepOutcomeKind::Interrupted, "interrupted"),
        ] {
            assert_eq!(serde_json::to_value(kind).unwrap(), json!(wire));
        }
    }

    #[test]
    fn appends_are_tagged_and_refuse_unknown_members() {
        let note: AppendEntry =
            serde_json::from_value(json!({"kind": "note", "text": "hi", "seen_seq": 3})).unwrap();
        assert_eq!(
            note,
            AppendEntry::Note {
                text: "hi".into(),
                seen_seq: Some(3)
            }
        );
        assert_eq!(note.seen_seq(), Some(3));
        assert_eq!(round_trip(&note), note);

        let finding: AppendEntry = serde_json::from_value(json!({
            "kind": "finding",
            "statement": "s",
            "status": "hypothesis",
            "references": [{"result_id": "r", "row": 0}],
        }))
        .unwrap();
        let AppendEntry::Finding {
            counterevidence,
            remaining_question,
            seen_seq,
            ..
        } = &finding
        else {
            panic!("{finding:?}");
        };
        assert!(counterevidence.is_empty());
        assert_eq!((remaining_question, seen_seq), (&None, &None));
        assert_eq!(round_trip(&finding), finding);

        let keep = AppendEntry::Keep {
            result_id: "r".into(),
            rows: vec![1],
            reason: "why".into(),
            seen_seq: None,
        };
        assert_eq!(
            serde_json::to_value(&keep).unwrap(),
            json!({"kind": "keep", "result_id": "r", "rows": [1], "reason": "why"})
        );

        for refused in [
            json!({"kind": "note", "text": "hi", "expected_seq": 3}),
            json!({"kind": "keep", "result_id": "r", "rows": [1], "reason": "x", "row": 2}),
            json!({"kind": "step", "executed": "x", "request_id": "r"}),
            json!({"kind": "status", "status": "closed"}),
            json!({"text": "untagged"}),
            json!({"kind": "finding", "statement": "s", "status": "proven", "references": []}),
            json!({"kind": "finding", "statement": "s", "status": "supported",
                   "references": [{"result_id": "r", "row": 0, "col": 1}]}),
        ] {
            assert!(
                serde_json::from_value::<AppendEntry>(refused.clone()).is_err(),
                "{refused}"
            );
        }
    }

    #[test]
    fn overwrite_requests_need_the_expected_sequence_and_refuse_unknown_members() {
        let patch: PatchInvestigation =
            serde_json::from_value(json!({"expected_seq": 4, "status": "closed"})).unwrap();
        assert_eq!(
            patch,
            PatchInvestigation {
                expected_seq: 4,
                title: None,
                question: None,
                status: Some(InvestigationStatus::Closed),
            }
        );
        assert_eq!(
            serde_json::to_value(&patch).unwrap(),
            json!({"expected_seq": 4, "status": "closed"})
        );
        assert!(serde_json::from_value::<PatchInvestigation>(json!({"status": "closed"})).is_err());
        assert!(
            serde_json::from_value::<PatchInvestigation>(json!({"expected_seq": 4, "scope": "x"}))
                .is_err()
        );

        let status: FindingStatusUpdate =
            serde_json::from_value(json!({"expected_seq": 9, "status": "unresolved"})).unwrap();
        assert_eq!(round_trip(&status), status);
        assert!(
            serde_json::from_value::<FindingStatusUpdate>(json!({"status": "refuted"})).is_err()
        );

        let extension: RetentionExtension = serde_json::from_value(
            json!({"expected_seq": 9, "retain_until": "2026-12-01T00:00:00Z"}),
        )
        .unwrap();
        assert_eq!(round_trip(&extension), extension);
        assert!(
            serde_json::from_value::<RetentionExtension>(
                json!({"expected_seq": 9, "retain_until": "x", "force": true})
            )
            .is_err()
        );
    }

    #[test]
    fn create_treats_an_absent_scope_as_empty() {
        let bare: CreateInvestigation =
            serde_json::from_value(json!({"title": "t", "question": "q"})).unwrap();
        let empty: CreateInvestigation =
            serde_json::from_value(json!({"title": "t", "question": "q", "scope": ""})).unwrap();
        assert_eq!(bare, empty);
        assert_eq!(
            serde_json::to_value(&bare).unwrap(),
            json!({"title": "t", "question": "q"})
        );
        assert!(
            serde_json::from_value::<CreateInvestigation>(
                json!({"title": "t", "question": "q", "id": ID})
            )
            .is_err()
        );
        assert!(serde_json::from_value::<CreateInvestigation>(json!({"title": "t"})).is_err());
    }

    #[test]
    fn attach_validates_the_id_and_refuses_unknown_members() {
        let attach: Attach =
            serde_json::from_value(json!({"id": ID, "seen_seq": 2, "purpose": "p"})).unwrap();
        assert_eq!(attach.id.as_str(), ID);
        assert_eq!(round_trip(&attach), attach);
        assert_eq!(
            serde_json::to_value(Attach {
                seen_seq: None,
                purpose: None,
                ..attach
            })
            .unwrap(),
            json!({"id": ID})
        );
        assert!(serde_json::from_value::<Attach>(json!({"id": "1"})).is_err());
        assert!(serde_json::from_value::<Attach>(json!({"id": ID, "keep": true})).is_err());
    }

    #[test]
    fn reads_activity_and_conflicts_round_trip() {
        let entry = LogEntry {
            seq: 5,
            updated_seq: 5,
            actor: agent(),
            at: "2026-10-24T09:05:00.000000Z".into(),
            payload: EntryPayload::Note { text: "n".into() },
        };
        let read = InvestigationRead {
            investigation: investigation(),
            entries: vec![entry.clone()],
            through_seq: 5,
            more: true,
        };
        assert_eq!(round_trip(&read), read);

        let activity = Activity {
            entries: vec![entry.clone()],
            through_seq: 5,
            more: false,
        };
        assert_eq!(round_trip(&activity), activity);
        let appended = AppendResponse {
            entry,
            activity: None,
        };
        assert!(
            serde_json::to_value(&appended)
                .unwrap()
                .get("activity")
                .is_none()
        );
        assert_eq!(round_trip(&appended), appended);

        let conflict = SequenceConflict {
            expected: 4,
            current: 6,
        };
        assert_eq!(
            serde_json::to_value(conflict).unwrap(),
            json!({"expected": 4, "current": 6})
        );

        let list = InvestigationList {
            investigations: vec![InvestigationSummary {
                id: investigation().id,
                title: "t".into(),
                status: InvestigationStatus::Closed,
                seq: 9,
                created_at: "2026-10-24T09:00:00.000000Z".into(),
                updated_at: "2026-10-24T10:00:00.000000Z".into(),
                created_by: agent(),
                updated_by: agent(),
            }],
        };
        assert_eq!(round_trip(&list), list);
    }

    #[test]
    fn enums_have_their_wire_spellings() {
        for (value, wire) in [
            (
                serde_json::to_value(InvestigationStatus::Open).unwrap(),
                "open",
            ),
            (
                serde_json::to_value(InvestigationStatus::Closed).unwrap(),
                "closed",
            ),
            (
                serde_json::to_value(FindingStatus::Hypothesis).unwrap(),
                "hypothesis",
            ),
            (
                serde_json::to_value(FindingStatus::Supported).unwrap(),
                "supported",
            ),
            (
                serde_json::to_value(FindingStatus::Refuted).unwrap(),
                "refuted",
            ),
            (
                serde_json::to_value(FindingStatus::Unresolved).unwrap(),
                "unresolved",
            ),
            (
                serde_json::to_value(EntryKind::FindingStatus).unwrap(),
                "finding_status",
            ),
        ] {
            assert_eq!(value, json!(wire));
        }
    }
}
