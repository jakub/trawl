// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Wire types for captured results (ADR-0057).
//!
//! `POST /api/v1/results` runs one query and keeps its answer as an
//! immutable, typed, expiring result; `GET /api/v1/results/{id}` pages its
//! rows, `GET /api/v1/results/{id}/cells/{row}/{column}` reads one cell by
//! byte range, and `GET /api/v1/results` lists the caller's own unexpired
//! captures. A captured row is the pair (result id, 0-based row number):
//! a row of a frozen copy, never an event identity.
//!
//! Columns are positional, so duplicate display names survive. Every page
//! carries the result's metadata ([`CapturedResult`]) so a client never
//! reads rows without the bounds, the executed text and the expiry that
//! say what they are.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::value::{ColumnType, QueryResult, Value, land_cell, serialize_cell};
use crate::{PaginationMeta, QueryExecution};

// -- request -----------------------------------------------------------------

/// Request body for `POST /api/v1/results`.
///
/// The bounds come from one source: either `window` or the DSL's own
/// `last=`, `earliest=` and `latest=` clauses in `query`. A request that
/// carries both is refused; one that carries neither captures the last 15
/// minutes. `from saved` takes no bounds and no window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureRequest {
    /// The trawl DSL query text, as the caller wrote it.
    pub query: String,
    /// A structured time window, instead of time clauses in the text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<CaptureWindow>,
}

/// A structured capture window. `last` is a DSL duration such as `1h`.
/// `earliest` and `latest` are RFC 3339 instants with an offset, such as
/// `2026-10-23T09:00:00Z`; the server accepts no other spelling. The server
/// resolves the window once, at execution time, to a half-open
/// `[earliest, latest)` in microseconds.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureWindow {
    /// The inclusive start, as an RFC 3339 instant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub earliest: Option<String>,
    /// The exclusive end, as an RFC 3339 instant. An open end closes at the
    /// execution time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest: Option<String>,
    /// A duration ending at the execution time, as a DSL duration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<String>,
}

/// Seconds either side of the focus row a `context` read covers by
/// default.
pub const CONTEXT_DEFAULT_SECONDS: u64 = 30;
/// Strictly earlier, or strictly later, rows a `context` read returns by
/// default.
pub const CONTEXT_DEFAULT_SIDE_ROWS: u64 = 10;
/// The most strictly earlier, or strictly later, rows a `context` read
/// returns.
pub const CONTEXT_MAX_SIDE_ROWS: u64 = 50;
/// The most rows sharing the focus row's `_time` a `context` read returns.
pub const CONTEXT_SAME_TIME_ROWS: u64 = 20;
/// What every `context` answer says about itself.
pub const CONTEXT_STATEMENT: &str = "new read; equal timestamps establish no order";
/// The most group fields a `compare` read accepts.
pub const COMPARE_MAX_GROUP_FIELDS: usize = 2;
/// The `min_count` a `compare` read applies by default.
pub const COMPARE_DEFAULT_MIN_COUNT: u64 = 1;

/// Request body for `POST /api/v1/results/context` (ADR-0058).
///
/// The focus row is captured row `row` of result `result_id`. The read
/// covers `seconds` either side of the focus row's `_time` on the focus
/// keys `scope` names, with the values the focus row holds. A focus row
/// lacking `_time` or a key the scope names is refused with
/// `focus_incomplete`, never widened.
///
/// `before` and `after` are at most [`CONTEXT_MAX_SIDE_ROWS`]; the server
/// refuses more.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextRequest {
    /// The captured result holding the focus row.
    pub result_id: String,
    /// The focus row's 0-based captured row number.
    pub row: u64,
    /// Which focus keys constrain the read.
    #[serde(default)]
    pub scope: ContextScope,
    /// Seconds either side of the focus row's `_time`.
    #[serde(default = "context_default_seconds")]
    pub seconds: u64,
    /// Strictly earlier rows to return.
    #[serde(default = "context_default_side_rows")]
    pub before: u64,
    /// Strictly later rows to return.
    #[serde(default = "context_default_side_rows")]
    pub after: u64,
}

fn context_default_seconds() -> u64 {
    CONTEXT_DEFAULT_SECONDS
}

fn context_default_side_rows() -> u64 {
    CONTEXT_DEFAULT_SIDE_ROWS
}

/// Which of the focus row's keys a `context` read constrains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextScope {
    /// The focus row's `env` and `host`.
    #[default]
    EnvAndHost,
    /// The focus row's `env` alone, across its hosts.
    Env,
    /// The focus row's `host` alone, across its envs.
    Host,
}

/// Request body for `POST /api/v1/results/sightings` (ADR-0058).
///
/// One literal, searched once as a quoted text search over `message` and
/// `_raw`. `window` defaults to the last 24 hours and spans at most 7
/// days. `fields` restricts which fields the literal is attributed to in
/// the captured rows; it never selects a different population.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SightingsRequest {
    /// The literal: non-empty, no NUL, at most 1024 bytes.
    pub literal: String,
    /// The bounds, as a capture window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<CaptureWindow>,
    /// The fields attribution is restricted to; empty attributes to every
    /// field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<String>,
}

/// Request body for `POST /api/v1/results/compare` (ADR-0058).
///
/// One statement scans the hull of both windows and labels each event's
/// membership. `baseline` defaults to the window of equal duration
/// immediately before `foreground`. The windows are half-open and may
/// touch; overlapping windows are refused with `windows_overlap`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompareRequest {
    /// DSL search tokens: no pipe stage and no time clause.
    pub filter: String,
    /// The window under question.
    pub foreground: CaptureWindow,
    /// The window it is compared against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<CaptureWindow>,
    /// At most [`COMPARE_MAX_GROUP_FIELDS`] distinct fields to group by.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_by: Vec<String>,
    /// A group is listed when either window counts at least this many
    /// events for it.
    #[serde(default = "compare_default_min_count")]
    pub min_count: u64,
}

fn compare_default_min_count() -> u64 {
    COMPARE_DEFAULT_MIN_COUNT
}

// -- enums -------------------------------------------------------------------

/// Which read made a capture: a plain search, or one of the correlation
/// reads (ADR-0058).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureKind {
    /// `POST /api/v1/results`.
    #[default]
    Search,
    /// `POST /api/v1/results/context`.
    Context,
    /// `POST /api/v1/results/sightings`.
    Sightings,
    /// `POST /api/v1/results/compare`.
    Compare,
}

impl CaptureKind {
    /// Whether this is [`Self::Search`], which the wire leaves unspelled.
    #[must_use]
    pub fn is_search(&self) -> bool {
        *self == Self::Search
    }
}

/// Where a capture's rows came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultSource {
    /// The corpus, read inside the capture's resolved bounds.
    Live,
    /// A stored report run, read through `from saved`. A stored source has
    /// no bounds: the run's own window decided what it holds.
    Stored,
}

/// Which rule kept the captured rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultShape {
    /// Events in `_time DESC, _ingested DESC` order. Past the cap the most
    /// recent rows are kept and the rest are counted in `total_rows`.
    Events,
    /// A whole answer (an aggregate, a projection, a stored run). Never cut:
    /// a whole result past the cap is refused with `result_too_large`.
    Whole,
}

/// A capture's lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultState {
    /// Owned by the key that made it and readable until `expires_at`.
    Temporary,
    /// Attached to an investigation and kept under its evidence policy
    /// (ADR-0059). Has no `expires_at`.
    Kept,
    /// Past its deadline. Reads answer `result_gone` while the tombstone
    /// lasts.
    Expired,
    /// Removed to bring its key back inside a lowered quota. Reads answer
    /// `result_gone`.
    Pruned,
}

/// How one column is stored in the capture's parquet file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoredAs {
    /// In the engine's own parquet type for the column's [`ColumnType`].
    Native,
    /// As a VARCHAR holding each cell's #214 wire JSON, because no single
    /// engine type holds the column: it is `mixed`, or it is an integer
    /// column that needs both a negative value and one above `i64::MAX`.
    /// Read back with [`land_cell`] under the column's type.
    WireText,
}

// -- metadata ----------------------------------------------------------------

/// One column of a captured result: its display name, its type and how
/// the file stores it.
///
/// The type is flattened beside the name the way [`crate::value::Column`]
/// spells it: `{"name":"status","type":"integer","stored_as":"native"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultColumn {
    /// The display name. Not unique: columns are addressed by position.
    pub name: String,
    /// The column's wire type; every cell of the column is encoded under it.
    #[serde(flatten)]
    pub ty: ColumnType,
    /// How the parquet file stores the column.
    pub stored_as: StoredAs,
}

/// One distinct value of a column and how many captured rows hold it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValueCount {
    /// The value, as a self-tagged cell (the `mixed` cell encoding:
    /// `{"type":"integer","value":"200"}`), so it reads without its
    /// column's type. Text is tagged `string` whatever the column type;
    /// the column says whether it is a timestamp.
    #[serde(with = "tagged_cell")]
    pub value: Value,
    /// Captured rows holding the value.
    pub count: u64,
}

/// Facts about one column over the captured rows, computed once at capture
/// so a reader can see what a column holds without paging it.
///
/// Values are self-tagged cells, as in [`ValueCount`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnFacts {
    /// Captured rows whose cell in this column is null.
    pub nulls: u64,
    /// Distinct non-null values, counted up to a small cap.
    pub distinct: u64,
    /// Whether counting stopped at the cap, so `distinct` is a lower bound.
    pub distinct_capped: bool,
    /// Every distinct non-null value with its count, present when the
    /// column holds at most five, most frequent first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub values: Option<Vec<ValueCount>>,
    /// The smallest value. Present for `_time` only.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_tagged_cell"
    )]
    pub min: Option<Value>,
    /// The largest value. Present for `_time` only.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_tagged_cell"
    )]
    pub max: Option<Value>,
}

/// The metadata of one captured result, carried by every capture, list and
/// page answer.
///
/// Timestamps are RFC 3339 UTC with microseconds. `truncated` is
/// `captured_rows < total_rows`; the server sets both from the same
/// execution, and the type does not enforce the relation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapturedResult {
    /// The result id: 32 lowercase hex digits, a random 128-bit value never
    /// reused.
    pub id: String,
    /// The query text as the caller sent it.
    pub query: String,
    /// The text that ran: the caller's text with the resolved bounds
    /// spliced in and, for an event-shaped result, the imposed
    /// `sort -_time, -_ingested`.
    pub executed_query: String,
    /// Where the rows came from.
    pub source: ResultSource,
    /// Which rule kept the rows.
    pub shape: ResultShape,
    /// The resolved inclusive start. Absent for a `stored` source, which
    /// has no bounds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub earliest: Option<String>,
    /// The resolved exclusive end. Absent for a `stored` source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest: Option<String>,
    /// When the result was committed.
    pub created_at: String,
    /// When a temporary result stops being readable. Absent once kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// The lifecycle state.
    pub state: ResultState,
    /// The columns in stored positional order.
    pub columns: Vec<ResultColumn>,
    /// Rows the file holds.
    pub captured_rows: u64,
    /// Rows the executed query produced before the capture cut, exact,
    /// from the same execution.
    pub total_rows: u64,
    /// Bytes of #214 wire JSON over every captured cell, without framing
    /// (the cap's measure, [`crate::value::wire_cell_len`]).
    pub wire_bytes: u64,
    /// Bytes of the stored parquet file; zero for a result with no
    /// columns, which has no file.
    pub file_bytes: u64,
    /// Whether rows were cut: `captured_rows < total_rows`.
    pub truncated: bool,
    /// Facts per column, positional with `columns`.
    pub column_facts: Vec<ColumnFacts>,
    /// The stored file's layout version.
    pub format_version: u16,
    /// Which read made the capture. Absent on the wire for a search, so a
    /// search capture is spelled as it was before the correlation reads.
    #[serde(default, skip_serializing_if = "CaptureKind::is_search")]
    pub kind: CaptureKind,
    /// The facts a correlation read computed, carried on every answer
    /// about the capture. Absent for a search.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation: Option<Correlation>,
}

// -- correlation facts -------------------------------------------------------

/// What a correlation read established beside its captured rows, tagged
/// by `kind` (ADR-0058).
///
/// Counts here are JSON numbers: the server computes them, they are not
/// result cells.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Correlation {
    /// A `context` read.
    Context(ContextFacts),
    /// A `sightings` read.
    Sightings(SightingsFacts),
    /// A `compare` read.
    Compare(CompareFacts),
}

/// A half-open range `[start, end)` of 0-based captured row numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RowRange {
    /// The first row in the range.
    pub start: u64,
    /// One past the last row; equal to `start` for an empty range.
    pub end: u64,
}

/// The facts of a `context` read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextFacts {
    /// The row the read was centred on.
    pub focus: FocusRow,
    /// The constraints the read applied.
    pub scope_used: ScopeUsed,
    /// Where each group sits in the captured rows.
    pub groups: ContextGroups,
    /// Always [`CONTEXT_STATEMENT`]: the rows are a new read, and rows
    /// sharing a timestamp are in no meaningful order.
    pub statement: String,
}

/// The focus row of a `context` read, as the request named it and the
/// values the read took from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusRow {
    /// The captured result holding the focus row.
    pub result_id: String,
    /// The focus row's 0-based captured row number.
    pub row: u64,
    /// The focus row's `_time`, RFC 3339 UTC with microseconds.
    pub time: String,
    /// The focus row's `env`, null when the row has none.
    pub env: Option<String>,
    /// The focus row's `host`, null when the row has none.
    pub host: Option<String>,
}

/// The constraints a `context` read applied: a key the scope did not name
/// is null.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeUsed {
    /// The `env` the read was held to.
    pub env: Option<String>,
    /// The `host` the read was held to.
    pub host: Option<String>,
    /// Seconds either side of the focus row's `_time`.
    pub seconds: u64,
}

/// The three groups of a `context` read, each a range of the captured
/// rows, each ordered by `_time`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextGroups {
    /// Rows strictly earlier than the focus row's `_time`.
    pub earlier: RowRange,
    /// Rows sharing the focus row's `_time`, the focus event among them.
    pub same_time: RowRange,
    /// Rows strictly later than the focus row's `_time`.
    pub later: RowRange,
}

/// The facts of a `sightings` read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SightingsFacts {
    /// The literal searched for.
    pub literal: String,
    /// Events the search matched, over the whole bounds.
    pub matches_total: u64,
    /// Matches captured: the most recent ones, at most the capture's cut.
    pub captured: u64,
    /// Matches per (`env`, `service`), exact over every match.
    pub groups: Vec<SightingGroup>,
    /// Captured matches per field the literal occurs in.
    pub fields: Vec<FieldSightings>,
    /// Captured matches in which no field but `_raw` holds the literal.
    pub raw_only_rows: u64,
    /// The populations the counts are over.
    pub denominators: SightingsDenominators,
    /// At most two next queries, as DSL text over the same bounds.
    pub follow_ups: Vec<String>,
}

/// One (`env`, `service`) group of a `sightings` read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SightingGroup {
    /// The group's `env`, null for events without one.
    pub env: Option<String>,
    /// The group's `service`, null for events without one.
    pub service: Option<String>,
    /// Matches in the group.
    pub count: u64,
    /// The group's earliest matching `_time`, RFC 3339 UTC.
    pub first_seen: String,
    /// The group's latest matching `_time`, RFC 3339 UTC.
    pub last_seen: String,
}

/// How many captured matches hold the literal in one field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldSightings {
    /// The field name.
    pub field: String,
    /// Captured matches whose value of the field holds the literal.
    pub rows: u64,
}

/// What each count of a `sightings` read is over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SightingsDenominators {
    /// Field counts are over this many captured matches.
    pub field_counts_over: u64,
    /// Group counts are over this many matches, every one in the bounds.
    pub service_counts_over: u64,
}

/// The facts of a `compare` read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompareFacts {
    /// The window under question.
    pub foreground: CompareWindow,
    /// The window it was compared against.
    pub baseline: CompareWindow,
    /// The fields the counts are grouped by.
    pub group_by: Vec<String>,
    /// The count a group reached in either window to be listed.
    pub min_count: u64,
    /// The unit of every rate column.
    pub rate_unit: RateUnit,
    /// Groups listed, ranked: absolute rate change descending, then
    /// count, then group key.
    pub rows_ranked: u64,
    /// What the read could not vouch for. Never a claim of completeness.
    pub warnings: Vec<CompareWarning>,
}

/// One resolved `compare` window, half-open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompareWindow {
    /// The inclusive start, RFC 3339 UTC with microseconds.
    pub start: String,
    /// The exclusive end, RFC 3339 UTC with microseconds.
    pub end: String,
    /// `end - start` in milliseconds.
    pub duration_ms: u64,
}

/// The unit a `compare` rate is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateUnit {
    /// Events per minute of the window.
    PerMinute,
}

/// A warning a `compare` read attaches to its answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompareWarning {
    /// What kind of warning it is.
    pub code: CompareWarningCode,
    /// The warning, worded for a reader.
    pub message: String,
    /// The group field it is about, when it is about one.
    #[serde(default)]
    pub field: Option<String>,
}

/// The kinds of `compare` warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompareWarningCode {
    /// A window starts before the oldest data retention kept, so data
    /// from its start may be missing.
    RetentionReach,
    /// A group field has a degraded pin: its type keeps shelving real
    /// values, so events whose value was shelved group under null.
    DegradedPin,
}

// -- responses ---------------------------------------------------------------

/// Response body for `POST /api/v1/results`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureResponse {
    /// The captured result.
    pub result: CapturedResult,
    /// Execution facts, spelled as `/api/v1/query` spells them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<QueryExecution>,
}

/// Response body for `POST /api/v1/results/context`, `/sightings` and
/// `/compare`: the capture response with the read's facts beside it.
///
/// `correlation` is the value `result.correlation` carries, given at the
/// top so a caller of one of these routes reads it without unwrapping.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CorrelationResponse {
    /// The captured result.
    pub result: CapturedResult,
    /// Execution facts, spelled as `/api/v1/query` spells them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<QueryExecution>,
    /// What the read established.
    pub correlation: Correlation,
}

/// Response body for `GET /api/v1/results/{id}`: one page of captured rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultPage {
    /// The result the page belongs to.
    pub result: CapturedResult,
    /// For each column of `page`, its 0-based position in `result.columns`,
    /// ascending.
    pub positions: Vec<usize>,
    /// The projected columns and the page's rows. Row `i` of the page is
    /// captured row `pagination.offset + i`.
    pub page: QueryResult,
    /// The page window; `total` is the result's `captured_rows`.
    pub pagination: PaginationMeta,
}

/// Response body for `GET /api/v1/results/{id}/cells/{row}/{column}`: a
/// byte range of one cell's #214 wire JSON text.
///
/// The type is flattened beside the range, as in [`ResultColumn`]. A client
/// reads the whole cell by following `next_offset` until it is absent and
/// lands the joined text with [`land_cell`] under the type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultCell {
    /// The column's wire type.
    #[serde(flatten)]
    pub ty: ColumnType,
    /// Byte offset of `text` within the cell's wire text.
    pub offset: u64,
    /// Byte length of `text`. It can be shorter than asked for, because
    /// the range is cut back to a UTF-8 boundary.
    pub length: u64,
    /// Byte length of the cell's whole wire text.
    pub total_bytes: u64,
    /// The offset of the next range, absent when `text` reaches the end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<u64>,
    /// The range of the cell's wire text.
    pub text: String,
}

/// Response body for `GET /api/v1/results`: the caller's unexpired
/// captures.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultList {
    /// The captures, newest first.
    pub results: Vec<CapturedResult>,
}

// -- self-tagged cells -------------------------------------------------------

/// A [`Value`] as a self-tagged `mixed` cell.
mod tagged_cell {
    use super::{ColumnType, Deserialize, Deserializer, Serializer, Value, land_cell};
    use serde::de::Error as _;

    pub(super) fn serialize<S: Serializer>(
        value: &Value,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        super::serialize_cell("value", &ColumnType::Mixed, value, serializer)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Value, D::Error> {
        let cell = serde_json::Value::deserialize(deserializer)?;
        land_cell(&ColumnType::Mixed, cell).map_err(D::Error::custom)
    }
}

/// An optional [`Value`] as a self-tagged `mixed` cell.
mod optional_tagged_cell {
    use super::{Deserializer, Serializer, Value};

    // serde's `with` hands an `Option` field to the module by reference.
    #[allow(clippy::ref_option)]
    pub(super) fn serialize<S: Serializer>(
        value: &Option<Value>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(value) => super::tagged_cell::serialize(value, serializer),
            None => serializer.serialize_none(),
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Value>, D::Error> {
        super::tagged_cell::deserialize(deserializer).map(|value| match value {
            Value::Null => None,
            value => Some(value),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Column;
    use serde_json::json;

    fn round_trip<T>(value: &T) -> T
    where
        T: Serialize + for<'de> Deserialize<'de>,
    {
        let text = serde_json::to_string(value).expect("encodes");
        serde_json::from_str(&text).expect("decodes")
    }

    fn captured() -> CapturedResult {
        CapturedResult {
            id: "0123456789abcdef0123456789abcdef".into(),
            query: "_service=nginx last=1h".into(),
            executed_query: "_service=nginx earliest=\"2026-10-23T09:00:00.000000Z\" \
                             latest=\"2026-10-23T10:00:00.000000Z\" | sort -_time, -_ingested"
                .into(),
            source: ResultSource::Live,
            shape: ResultShape::Events,
            earliest: Some("2026-10-23T09:00:00.000000Z".into()),
            latest: Some("2026-10-23T10:00:00.000000Z".into()),
            created_at: "2026-10-23T10:00:00.250000Z".into(),
            expires_at: Some("2026-10-23T10:15:00.250000Z".into()),
            state: ResultState::Temporary,
            columns: vec![
                ResultColumn {
                    name: "_time".into(),
                    ty: ColumnType::Timestamp,
                    stored_as: StoredAs::Native,
                },
                ResultColumn {
                    name: "status".into(),
                    ty: ColumnType::Integer,
                    stored_as: StoredAs::WireText,
                },
                ResultColumn {
                    name: "status".into(),
                    ty: ColumnType::Mixed,
                    stored_as: StoredAs::WireText,
                },
            ],
            captured_rows: 5000,
            total_rows: 20000,
            wire_bytes: 123_456,
            file_bytes: 65_536,
            truncated: true,
            column_facts: vec![
                ColumnFacts {
                    nulls: 0,
                    distinct: 6,
                    distinct_capped: true,
                    values: None,
                    min: Some(Value::String("2026-10-23T09:58:00.000000Z".into())),
                    max: Some(Value::String("2026-10-23T09:59:59.999999Z".into())),
                },
                ColumnFacts {
                    nulls: 1,
                    distinct: 2,
                    distinct_capped: false,
                    values: Some(vec![
                        ValueCount {
                            value: Value::UInt(u64::MAX),
                            count: 4000,
                        },
                        ValueCount {
                            value: Value::Integer(-1),
                            count: 999,
                        },
                    ]),
                    min: None,
                    max: None,
                },
                ColumnFacts {
                    nulls: 5000,
                    distinct: 0,
                    distinct_capped: false,
                    values: Some(vec![]),
                    min: None,
                    max: None,
                },
            ],
            format_version: 1,
            kind: CaptureKind::Search,
            correlation: None,
        }
    }

    /// The whole metadata spelling, so a renamed or retyped field shows up
    /// in review.
    #[test]
    fn captured_result_has_a_golden_spelling() {
        let golden = json!({
            "id": "0123456789abcdef0123456789abcdef",
            "query": "_service=nginx last=1h",
            "executed_query": "_service=nginx earliest=\"2026-10-23T09:00:00.000000Z\" latest=\"2026-10-23T10:00:00.000000Z\" | sort -_time, -_ingested",
            "source": "live",
            "shape": "events",
            "earliest": "2026-10-23T09:00:00.000000Z",
            "latest": "2026-10-23T10:00:00.000000Z",
            "created_at": "2026-10-23T10:00:00.250000Z",
            "expires_at": "2026-10-23T10:15:00.250000Z",
            "state": "temporary",
            "columns": [
                {"name": "_time", "type": "timestamp", "stored_as": "native"},
                {"name": "status", "type": "integer", "stored_as": "wire_text"},
                {"name": "status", "type": "mixed", "stored_as": "wire_text"},
            ],
            "captured_rows": 5000,
            "total_rows": 20000,
            "wire_bytes": 123_456,
            "file_bytes": 65_536,
            "truncated": true,
            "column_facts": [
                {
                    "nulls": 0,
                    "distinct": 6,
                    "distinct_capped": true,
                    "min": {"type": "string", "value": "2026-10-23T09:58:00.000000Z"},
                    "max": {"type": "string", "value": "2026-10-23T09:59:59.999999Z"},
                },
                {
                    "nulls": 1,
                    "distinct": 2,
                    "distinct_capped": false,
                    "values": [
                        {"value": {"type": "integer", "value": "18446744073709551615"}, "count": 4000},
                        {"value": {"type": "integer", "value": "-1"}, "count": 999},
                    ],
                },
                {
                    "nulls": 5000,
                    "distinct": 0,
                    "distinct_capped": false,
                    "values": [],
                },
            ],
            "format_version": 1,
        });
        let result = captured();
        assert_eq!(serde_json::to_value(&result).unwrap(), golden);
        let decoded: CapturedResult = serde_json::from_value(golden).unwrap();
        assert_eq!(decoded, result);
        assert_eq!(round_trip(&result), result);
    }

    /// A stored source has no bounds and a kept result no deadline: the
    /// members are absent, not null, and decode back to `None`.
    #[test]
    fn a_stored_kept_result_omits_bounds_and_deadline() {
        let result = CapturedResult {
            source: ResultSource::Stored,
            shape: ResultShape::Whole,
            earliest: None,
            latest: None,
            expires_at: None,
            state: ResultState::Kept,
            truncated: false,
            captured_rows: 3,
            total_rows: 3,
            ..captured()
        };
        let wire = serde_json::to_value(&result).unwrap();
        for absent in ["earliest", "latest", "expires_at"] {
            assert!(wire.get(absent).is_none(), "{absent} in {wire}");
        }
        assert_eq!(round_trip(&result), result);
    }

    /// `truncated` is carried as sent: the type documents the relation to
    /// the row counts and leaves it to the server.
    #[test]
    fn truncated_is_carried_not_derived() {
        let result = CapturedResult {
            truncated: false,
            ..captured()
        };
        assert!(result.captured_rows < result.total_rows);
        assert!(!round_trip(&result).truncated);
    }

    #[test]
    fn enums_have_their_wire_spellings() {
        let spell = |value: serde_json::Value| value.as_str().unwrap().to_owned();
        let cases = [
            (serde_json::to_value(ResultSource::Live).unwrap(), "live"),
            (
                serde_json::to_value(ResultSource::Stored).unwrap(),
                "stored",
            ),
            (serde_json::to_value(ResultShape::Events).unwrap(), "events"),
            (serde_json::to_value(ResultShape::Whole).unwrap(), "whole"),
            (
                serde_json::to_value(ResultState::Temporary).unwrap(),
                "temporary",
            ),
            (serde_json::to_value(ResultState::Kept).unwrap(), "kept"),
            (
                serde_json::to_value(ResultState::Expired).unwrap(),
                "expired",
            ),
            (serde_json::to_value(ResultState::Pruned).unwrap(), "pruned"),
            (serde_json::to_value(StoredAs::Native).unwrap(), "native"),
            (
                serde_json::to_value(StoredAs::WireText).unwrap(),
                "wire_text",
            ),
        ];
        for (wire, expected) in cases {
            assert_eq!(spell(wire), expected);
        }
        for state in [
            ResultState::Temporary,
            ResultState::Kept,
            ResultState::Expired,
            ResultState::Pruned,
        ] {
            assert_eq!(round_trip(&state), state);
        }
        assert!(serde_json::from_value::<ResultState>(json!("Temporary")).is_err());
    }

    #[test]
    fn capture_request_round_trips() {
        let bare = CaptureRequest {
            query: "_service=nginx last=1h".into(),
            window: None,
        };
        assert_eq!(
            serde_json::to_value(&bare).unwrap(),
            json!({"query": "_service=nginx last=1h"})
        );
        assert_eq!(round_trip(&bare), bare);

        let windowed = CaptureRequest {
            query: "_service=nginx".into(),
            window: Some(CaptureWindow {
                earliest: Some("2026-10-23T08:00:00Z".into()),
                latest: Some("2026-10-23T10:00:00Z".into()),
                last: None,
            }),
        };
        assert_eq!(
            serde_json::to_value(&windowed).unwrap(),
            json!({
                "query": "_service=nginx",
                "window": {"earliest": "2026-10-23T08:00:00Z", "latest": "2026-10-23T10:00:00Z"},
            })
        );
        assert_eq!(round_trip(&windowed), windowed);
    }

    /// A misspelled member is refused, not ignored: an ignored `windw`
    /// would silently capture the default 15 minutes.
    #[test]
    fn capture_request_refuses_unknown_members() {
        let err = serde_json::from_value::<CaptureRequest>(json!({
            "query": "x",
            "windw": {"last": "1h"},
        }))
        .unwrap_err();
        assert!(err.to_string().contains("unknown field `windw`"), "{err}");

        let err = serde_json::from_value::<CaptureRequest>(json!({
            "query": "x",
            "window": {"last": "1h", "span": "2h"},
        }))
        .unwrap_err();
        assert!(err.to_string().contains("unknown field `span`"), "{err}");

        assert!(serde_json::from_value::<CaptureRequest>(json!({"window": {}})).is_err());
    }

    #[test]
    fn capture_response_round_trips_with_and_without_execution() {
        let response = CaptureResponse {
            result: captured(),
            execution: Some(QueryExecution {
                started_at: "2026-10-23T10:00:00.000000Z".into(),
                duration_ms: 250,
            }),
        };
        let wire = serde_json::to_value(&response).unwrap();
        assert_eq!(wire["execution"]["duration_ms"], json!(250));
        assert_eq!(round_trip(&response), response);

        let bare = CaptureResponse {
            execution: None,
            ..response
        };
        assert!(
            serde_json::to_value(&bare)
                .unwrap()
                .get("execution")
                .is_none()
        );
        assert_eq!(round_trip(&bare), bare);
    }

    #[test]
    fn result_page_round_trips() {
        let page = ResultPage {
            result: captured(),
            positions: vec![0, 2],
            page: QueryResult {
                columns: vec![
                    Column::new("_time", ColumnType::Timestamp),
                    Column::new("status", ColumnType::Mixed),
                ],
                rows: vec![
                    vec![
                        Value::String("2026-10-23T09:59:59.999999Z".into()),
                        Value::UInt(u64::MAX),
                    ],
                    vec![
                        Value::String("2026-10-23T09:59:59.000000Z".into()),
                        Value::Null,
                    ],
                ],
            },
            pagination: PaginationMeta {
                limit: 2,
                offset: 10,
                returned: 2,
                total: 5000,
            },
        };
        let wire = serde_json::to_value(&page).unwrap();
        assert_eq!(wire["positions"], json!([0, 2]));
        assert_eq!(
            wire["page"]["rows"][0][1],
            json!({"type": "integer", "value": "18446744073709551615"})
        );
        assert_eq!(round_trip(&page), page);
    }

    #[test]
    fn result_cell_round_trips() {
        let first = ResultCell {
            ty: ColumnType::list(ColumnType::String),
            offset: 0,
            length: 5,
            total_bytes: 9,
            next_offset: Some(5),
            text: "[\"żó".into(),
        };
        assert_eq!(
            serde_json::to_value(&first).unwrap(),
            json!({
                "type": "list",
                "items": {"type": "string"},
                "offset": 0,
                "length": 5,
                "total_bytes": 9,
                "next_offset": 5,
                "text": "[\"żó",
            })
        );
        assert_eq!(round_trip(&first), first);

        let last = ResultCell {
            offset: 5,
            length: 4,
            next_offset: None,
            text: "łw\"]".into(),
            ..first.clone()
        };
        assert!(
            serde_json::to_value(&last)
                .unwrap()
                .get("next_offset")
                .is_none()
        );
        assert_eq!(round_trip(&last), last);

        // The ranges join into wire text that lands under the type.
        let whole: serde_json::Value =
            serde_json::from_str(&format!("{}{}", first.text, last.text)).unwrap();
        assert_eq!(
            land_cell(&first.ty, whole).unwrap(),
            Value::Array(vec![Value::String("żółw".into())])
        );
    }

    #[test]
    fn result_list_round_trips() {
        let list = ResultList {
            results: vec![captured(), captured()],
        };
        assert_eq!(round_trip(&list), list);
        let empty = ResultList { results: vec![] };
        assert_eq!(
            serde_json::to_value(&empty).unwrap(),
            json!({"results": []})
        );
        assert_eq!(round_trip(&empty), empty);
    }

    #[test]
    fn column_facts_round_trip_every_value_kind() {
        let facts = ColumnFacts {
            nulls: 2,
            distinct: 5,
            distinct_capped: false,
            values: Some(vec![
                ValueCount {
                    value: Value::Boolean(true),
                    count: 1,
                },
                ValueCount {
                    value: Value::Float(f64::INFINITY),
                    count: 1,
                },
                ValueCount {
                    value: Value::String("é\"\n".into()),
                    count: 1,
                },
                ValueCount {
                    value: Value::Array(vec![Value::Integer(1), Value::Float(2.5)]),
                    count: 1,
                },
                ValueCount {
                    value: Value::Integer(i64::MIN),
                    count: 1,
                },
            ]),
            min: None,
            max: None,
        };
        assert_eq!(round_trip(&facts), facts);
        // A fact value that is not a tagged cell does not decode.
        assert!(serde_json::from_value::<ValueCount>(json!({"value": "200", "count": 1})).is_err());
    }

    #[test]
    fn result_column_wire_matches_column() {
        let column = ResultColumn {
            name: "tags".into(),
            ty: ColumnType::list(ColumnType::Integer),
            stored_as: StoredAs::Native,
        };
        let wire = serde_json::to_value(&column).unwrap();
        let plain = serde_json::to_value(Column::new("tags", column.ty.clone())).unwrap();
        assert_eq!(wire["type"], plain["type"]);
        assert_eq!(wire["items"], plain["items"]);
        assert_eq!(round_trip(&column), column);
    }

    // -- correlation reads ---------------------------------------------------

    fn context_facts() -> Correlation {
        Correlation::Context(ContextFacts {
            focus: FocusRow {
                result_id: "0123456789abcdef0123456789abcdef".into(),
                row: 3,
                time: "2026-10-23T09:59:00.000000Z".into(),
                env: Some("prod".into()),
                host: None,
            },
            scope_used: ScopeUsed {
                env: Some("prod".into()),
                host: None,
                seconds: 30,
            },
            groups: ContextGroups {
                earlier: RowRange { start: 0, end: 10 },
                same_time: RowRange { start: 10, end: 12 },
                later: RowRange { start: 12, end: 12 },
            },
            statement: CONTEXT_STATEMENT.into(),
        })
    }

    fn sightings_facts() -> Correlation {
        Correlation::Sightings(SightingsFacts {
            literal: "10.0.4.17".into(),
            matches_total: 20_000,
            captured: 5000,
            groups: vec![
                SightingGroup {
                    env: Some("prod".into()),
                    service: Some("auth".into()),
                    count: 19_000,
                    first_seen: "2026-10-22T10:00:00.000000Z".into(),
                    last_seen: "2026-10-23T09:59:00.000000Z".into(),
                },
                SightingGroup {
                    env: None,
                    service: None,
                    count: 1000,
                    first_seen: "2026-10-22T11:00:00.000000Z".into(),
                    last_seen: "2026-10-22T11:30:00.000000Z".into(),
                },
            ],
            fields: vec![
                FieldSightings {
                    field: "client_ip".into(),
                    rows: 4000,
                },
                FieldSightings {
                    field: "peer".into(),
                    rows: 900,
                },
            ],
            raw_only_rows: 100,
            denominators: SightingsDenominators {
                field_counts_over: 5000,
                service_counts_over: 20_000,
            },
            follow_ups: vec!["\"10.0.4.17\" service=auth".into()],
        })
    }

    fn compare_facts() -> Correlation {
        Correlation::Compare(CompareFacts {
            foreground: CompareWindow {
                start: "2026-10-23T09:00:00.000000Z".into(),
                end: "2026-10-23T10:00:00.000000Z".into(),
                duration_ms: 3_600_000,
            },
            baseline: CompareWindow {
                start: "2026-10-23T08:00:00.000000Z".into(),
                end: "2026-10-23T09:00:00.000000Z".into(),
                duration_ms: 3_600_000,
            },
            group_by: vec!["host".into()],
            min_count: 1,
            rate_unit: RateUnit::PerMinute,
            rows_ranked: 7,
            warnings: vec![
                CompareWarning {
                    code: CompareWarningCode::RetentionReach,
                    message: "data before 2026-10-23 may be missing".into(),
                    field: None,
                },
                CompareWarning {
                    code: CompareWarningCode::DegradedPin,
                    message: "host has a degraded pin".into(),
                    field: Some("host".into()),
                },
            ],
        })
    }

    /// Each kind's whole spelling: the tag is `kind`, counts are numbers,
    /// and an absent key is `null`, never missing.
    #[test]
    fn correlation_facts_have_golden_spellings() {
        let cases = [
            (
                context_facts(),
                json!({
                    "kind": "context",
                    "focus": {
                        "result_id": "0123456789abcdef0123456789abcdef",
                        "row": 3,
                        "time": "2026-10-23T09:59:00.000000Z",
                        "env": "prod",
                        "host": null,
                    },
                    "scope_used": {"env": "prod", "host": null, "seconds": 30},
                    "groups": {
                        "earlier": {"start": 0, "end": 10},
                        "same_time": {"start": 10, "end": 12},
                        "later": {"start": 12, "end": 12},
                    },
                    "statement": "new read; equal timestamps establish no order",
                }),
            ),
            (
                sightings_facts(),
                json!({
                    "kind": "sightings",
                    "literal": "10.0.4.17",
                    "matches_total": 20000,
                    "captured": 5000,
                    "groups": [
                        {
                            "env": "prod",
                            "service": "auth",
                            "count": 19000,
                            "first_seen": "2026-10-22T10:00:00.000000Z",
                            "last_seen": "2026-10-23T09:59:00.000000Z",
                        },
                        {
                            "env": null,
                            "service": null,
                            "count": 1000,
                            "first_seen": "2026-10-22T11:00:00.000000Z",
                            "last_seen": "2026-10-22T11:30:00.000000Z",
                        },
                    ],
                    "fields": [
                        {"field": "client_ip", "rows": 4000},
                        {"field": "peer", "rows": 900},
                    ],
                    "raw_only_rows": 100,
                    "denominators": {"field_counts_over": 5000, "service_counts_over": 20000},
                    "follow_ups": ["\"10.0.4.17\" service=auth"],
                }),
            ),
            (
                compare_facts(),
                json!({
                    "kind": "compare",
                    "foreground": {
                        "start": "2026-10-23T09:00:00.000000Z",
                        "end": "2026-10-23T10:00:00.000000Z",
                        "duration_ms": 3_600_000,
                    },
                    "baseline": {
                        "start": "2026-10-23T08:00:00.000000Z",
                        "end": "2026-10-23T09:00:00.000000Z",
                        "duration_ms": 3_600_000,
                    },
                    "group_by": ["host"],
                    "min_count": 1,
                    "rate_unit": "per_minute",
                    "rows_ranked": 7,
                    "warnings": [
                        {
                            "code": "retention_reach",
                            "message": "data before 2026-10-23 may be missing",
                            "field": null,
                        },
                        {
                            "code": "degraded_pin",
                            "message": "host has a degraded pin",
                            "field": "host",
                        },
                    ],
                }),
            ),
        ];
        for (facts, golden) in cases {
            assert_eq!(serde_json::to_value(&facts).unwrap(), golden);
            assert_eq!(
                serde_json::from_value::<Correlation>(golden).unwrap(),
                facts
            );
            assert_eq!(round_trip(&facts), facts);
        }
        assert!(
            serde_json::from_value::<Correlation>(json!({"kind": "anomaly"})).is_err(),
            "an unknown kind is refused"
        );
    }

    /// A capture made before the correlation reads, without `kind` or
    /// `correlation`, decodes as a search, and a search encodes without
    /// either member: #308's decoders and the documented examples hold.
    #[test]
    fn a_search_capture_carries_no_kind_and_no_correlation() {
        let mut wire = serde_json::to_value(captured()).unwrap();
        assert!(wire.get("kind").is_none(), "{wire}");
        assert!(wire.get("correlation").is_none(), "{wire}");
        let old: CapturedResult = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(old.kind, CaptureKind::Search);
        assert_eq!(old.correlation, None);

        wire["kind"] = json!("search");
        assert_eq!(
            serde_json::from_value::<CapturedResult>(wire).unwrap(),
            captured()
        );
    }

    #[test]
    fn a_correlation_capture_spells_its_kind_and_facts() {
        for (kind, facts, spelled) in [
            (CaptureKind::Context, context_facts(), "context"),
            (CaptureKind::Sightings, sightings_facts(), "sightings"),
            (CaptureKind::Compare, compare_facts(), "compare"),
        ] {
            let result = CapturedResult {
                kind,
                correlation: Some(facts.clone()),
                ..captured()
            };
            let wire = serde_json::to_value(&result).unwrap();
            assert_eq!(wire["kind"], json!(spelled));
            assert_eq!(wire["correlation"]["kind"], json!(spelled));
            assert_eq!(round_trip(&result), result);

            let response = CorrelationResponse {
                result: result.clone(),
                execution: Some(QueryExecution {
                    started_at: "2026-10-23T10:00:00.000000Z".into(),
                    duration_ms: 40,
                }),
                correlation: facts,
            };
            let wire = serde_json::to_value(&response).unwrap();
            assert_eq!(wire["correlation"], wire["result"]["correlation"]);
            assert_eq!(round_trip(&response), response);
            let bare = CorrelationResponse {
                execution: None,
                ..response
            };
            assert!(
                serde_json::to_value(&bare)
                    .unwrap()
                    .get("execution")
                    .is_none()
            );
            assert_eq!(round_trip(&bare), bare);
        }
    }

    #[test]
    fn correlation_requests_fill_their_defaults() {
        let context: ContextRequest = serde_json::from_value(json!({
            "result_id": "0123456789abcdef0123456789abcdef",
            "row": 3,
        }))
        .unwrap();
        assert_eq!(
            context,
            ContextRequest {
                result_id: "0123456789abcdef0123456789abcdef".into(),
                row: 3,
                scope: ContextScope::EnvAndHost,
                seconds: CONTEXT_DEFAULT_SECONDS,
                before: CONTEXT_DEFAULT_SIDE_ROWS,
                after: CONTEXT_DEFAULT_SIDE_ROWS,
            }
        );
        assert_eq!(
            (context.seconds, context.before, context.after),
            (30, 10, 10)
        );
        assert_eq!(round_trip(&context), context);

        let sightings: SightingsRequest =
            serde_json::from_value(json!({"literal": "10.0.4.17"})).unwrap();
        assert_eq!(
            sightings,
            SightingsRequest {
                literal: "10.0.4.17".into(),
                window: None,
                fields: vec![],
            }
        );
        assert_eq!(
            serde_json::to_value(&sightings).unwrap(),
            json!({"literal": "10.0.4.17"})
        );

        let compare: CompareRequest = serde_json::from_value(json!({
            "filter": "service=auth",
            "foreground": {"last": "1h"},
        }))
        .unwrap();
        assert_eq!(
            compare,
            CompareRequest {
                filter: "service=auth".into(),
                foreground: CaptureWindow {
                    last: Some("1h".into()),
                    ..CaptureWindow::default()
                },
                baseline: None,
                group_by: vec![],
                min_count: 1,
            }
        );
        assert_eq!(
            serde_json::to_value(&compare).unwrap(),
            json!({"filter": "service=auth", "foreground": {"last": "1h"}, "min_count": 1})
        );
    }

    #[test]
    fn correlation_requests_round_trip_every_member() {
        let context = ContextRequest {
            result_id: "0123456789abcdef0123456789abcdef".into(),
            row: 0,
            scope: ContextScope::Host,
            seconds: 5,
            before: 50,
            after: 0,
        };
        assert_eq!(
            serde_json::to_value(&context).unwrap(),
            json!({
                "result_id": "0123456789abcdef0123456789abcdef",
                "row": 0,
                "scope": "host",
                "seconds": 5,
                "before": 50,
                "after": 0,
            })
        );
        assert_eq!(round_trip(&context), context);

        let sightings = SightingsRequest {
            literal: "a\"b|c#d".into(),
            window: Some(CaptureWindow {
                earliest: Some("2026-10-16T00:00:00Z".into()),
                latest: Some("2026-10-23T00:00:00Z".into()),
                last: None,
            }),
            fields: vec!["client_ip".into(), "peer".into()],
        };
        assert_eq!(round_trip(&sightings), sightings);

        let compare = CompareRequest {
            filter: "service=auth \"failed password\"".into(),
            foreground: CaptureWindow {
                earliest: Some("2026-10-23T09:00:00Z".into()),
                latest: Some("2026-10-23T10:00:00Z".into()),
                last: None,
            },
            baseline: Some(CaptureWindow {
                earliest: Some("2026-10-22T09:00:00Z".into()),
                latest: Some("2026-10-22T10:00:00Z".into()),
                last: None,
            }),
            group_by: vec!["host".into(), "user".into()],
            min_count: 5,
        };
        assert_eq!(round_trip(&compare), compare);
    }

    /// A misspelled member is refused, not ignored, in each request and in
    /// the windows they carry.
    #[test]
    fn correlation_requests_refuse_unknown_members() {
        let refused = [
            serde_json::from_value::<ContextRequest>(json!({
                "result_id": "x", "row": 1, "secs": 5,
            }))
            .err(),
            serde_json::from_value::<SightingsRequest>(json!({
                "literal": "x", "field": ["a"],
            }))
            .err(),
            serde_json::from_value::<SightingsRequest>(json!({
                "literal": "x", "window": {"last": "1h", "span": "2h"},
            }))
            .err(),
            serde_json::from_value::<CompareRequest>(json!({
                "filter": "x", "foreground": {"last": "1h"}, "groups": ["host"],
            }))
            .err(),
            serde_json::from_value::<CompareRequest>(json!({
                "filter": "x", "foreground": {"last": "1h", "until": "now"},
            }))
            .err(),
        ];
        for (i, err) in refused.into_iter().enumerate() {
            let err = err.unwrap_or_else(|| panic!("case {i} was accepted"));
            assert!(err.to_string().contains("unknown field"), "case {i}: {err}");
        }
        // Required members stay required.
        assert!(serde_json::from_value::<ContextRequest>(json!({"row": 1})).is_err());
        assert!(serde_json::from_value::<SightingsRequest>(json!({})).is_err());
        assert!(serde_json::from_value::<CompareRequest>(json!({"filter": "x"})).is_err());
        assert!(
            serde_json::from_value::<ContextRequest>(json!({
                "result_id": "x", "row": 1, "scope": "everything",
            }))
            .is_err()
        );
    }

    #[test]
    fn correlation_enums_have_their_wire_spellings() {
        let spell = |value: serde_json::Value| value.as_str().unwrap().to_owned();
        let cases = [
            (serde_json::to_value(CaptureKind::Search).unwrap(), "search"),
            (
                serde_json::to_value(CaptureKind::Context).unwrap(),
                "context",
            ),
            (
                serde_json::to_value(CaptureKind::Sightings).unwrap(),
                "sightings",
            ),
            (
                serde_json::to_value(CaptureKind::Compare).unwrap(),
                "compare",
            ),
            (
                serde_json::to_value(ContextScope::EnvAndHost).unwrap(),
                "env_and_host",
            ),
            (serde_json::to_value(ContextScope::Env).unwrap(), "env"),
            (serde_json::to_value(ContextScope::Host).unwrap(), "host"),
            (
                serde_json::to_value(RateUnit::PerMinute).unwrap(),
                "per_minute",
            ),
            (
                serde_json::to_value(CompareWarningCode::RetentionReach).unwrap(),
                "retention_reach",
            ),
            (
                serde_json::to_value(CompareWarningCode::DegradedPin).unwrap(),
                "degraded_pin",
            ),
        ];
        for (wire, expected) in cases {
            assert_eq!(spell(wire), expected);
        }
        for kind in [
            CaptureKind::Search,
            CaptureKind::Context,
            CaptureKind::Sightings,
            CaptureKind::Compare,
        ] {
            assert_eq!(round_trip(&kind), kind);
        }
        assert_eq!(CaptureKind::default(), CaptureKind::Search);
        assert_eq!(ContextScope::default(), ContextScope::EnvAndHost);
    }
}
