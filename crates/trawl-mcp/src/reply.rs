// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The reply model: what each tool answers, as data (ADR-0058).
//!
//! Every successful tool result has one text block that stands alone and a
//! `structuredContent` object that mirrors it. The structs here are that
//! mirror: a tool fills one, the renderer prints its text block from it,
//! and its schema is the tool's `outputSchema`. A refused call is
//! `isError` with text only, so these describe success.
//!
//! The text block's layout, top to bottom, is the field order of
//! [`ResultView`]: the outcome line, the bounds and the executed text, the
//! four truncation facts, the facets, the proposed next actions, the link
//! to Search, one columns line, then the rows as JSON arrays. A
//! [`CorrelationReply`] prints its [`CorrelationView`] between the
//! truncation facts and the next actions, and so does a `read` page of a
//! correlation result.
//!
//! Schema rules every struct here keeps, because Claude Code drops a tool
//! that breaks them: property names stay within `[A-Za-z0-9_.-]`, the root
//! is an object with no `anyOf` or `oneOf`, alternatives are flat structs
//! with optional fields, and enums are inlined string enums whose variants
//! carry no doc comments, so each is one `enum` keyword rather than a
//! `oneOf` of constants: the enum's own doc names every variant.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

// -- shared pieces ------------------------------------------------------------

/// Which `search` mode ran: `validate` checks the query without running
/// it (`/api/v1/validate`); `execute` runs it as a capture (ADR-0057).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
pub enum SearchMode {
    Validate,
    Execute,
}

/// Which `read` action ran: `page` reads a page of a captured result's
/// rows; `cell` reads a byte range of one captured cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
pub enum ReadAction {
    Page,
    Cell,
}

/// Which focus keys a `context` read holds the rows to: `env_and_host`,
/// the focus row's `env` and `host` (the default); `env`, its `env` alone,
/// across hosts; `host`, its `host` alone, across environments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
pub enum ContextScope {
    #[default]
    EnvAndHost,
    Env,
    Host,
}

/// Which correlation read made a result: `context`, the rows around a
/// focus row; `sightings`, where one literal occurs; `compare`, one
/// filter's counts in two windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
pub enum CorrelationKind {
    Context,
    Sightings,
    Compare,
}

/// What `fields` describes: `service`, the fields the catalog has
/// observed for one service; `result`, the columns of one captured result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
pub enum FieldsSource {
    Service,
    Result,
}

/// Where a field fact comes from: `catalog_observation`, the catalog's
/// observation of the field in stored data; `parquet_footer`, a stored
/// parquet file's footer; `cached_sample`, the daemon's cached sample of
/// recent values; `capture`, the captured result itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
pub enum Provenance {
    CatalogObservation,
    ParquetFooter,
    CachedSample,
    Capture,
}

/// A tool call the model can make next, with its arguments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NextAction {
    /// The tool to call, one of the listed tools.
    pub tool: String,
    /// The call's arguments, valid under that tool's input schema.
    pub arguments: Map<String, Value>,
    /// Why the call is proposed, in one sentence.
    pub reason: String,
}

/// One value of a facet and the captured rows that hold it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FacetValue {
    /// The value, as a JSON value (text stays a JSON string).
    pub value: Value,
    /// Captured rows holding the value.
    pub count: u64,
}

/// The most frequent values of one low-cardinality field over the
/// captured rows: at most five values, at most three facets per result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Facet {
    /// The column's display name.
    pub field: String,
    /// Most frequent first.
    pub values: Vec<FacetValue>,
    /// The captured rows every count is out of.
    pub captured_rows: u64,
}

/// The link that opens Search with the executed query and its bounds,
/// or the reason there is none. Exactly one field is present.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Link {
    /// The search URL, under the configured `[server] link_origin`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Why no link was built: no `link_origin` configured, or the link
    /// would exceed the 32 KiB admission limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The resolved bounds of a capture, RFC 3339 with microseconds in UTC.
/// Both are absent for a result read from stored rows, which has none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Bounds {
    /// The inclusive start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub earliest: Option<String>,
    /// The exclusive end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest: Option<String>,
}

/// Whether the DSL itself reduced the rows, independent of any cut the
/// capture or the response made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DslEffects {
    /// A `sample` stage ran.
    pub sampled: bool,
    /// A `head`, `limit` or `tail` stage cut the rows.
    pub limited: bool,
    /// A `dedup` stage dropped rows.
    pub deduplicated: bool,
}

/// What this response left out of what it read, so the model can ask for
/// the rest with `read`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Presentation {
    /// Cells printed shorter than they are, each listed in `abbreviated`.
    pub cells_abbreviated: u64,
    /// Columns of the result this response did not print.
    pub columns_omitted: u64,
    /// Captured rows after the last printed row.
    pub rows_omitted: u64,
    /// Of `rows_omitted`, the rows this response read and dropped from
    /// its end to fit the byte budget.
    pub rows_dropped_to_fit: u64,
    /// Whether the facets were dropped to fit the byte budget.
    pub facets_dropped_to_fit: bool,
    /// Whether the hints were dropped to fit the byte budget.
    pub hints_dropped_to_fit: bool,
    /// Whether the proposed next calls were dropped to fit the byte budget.
    pub next_actions_dropped_to_fit: bool,
}

/// One column of a result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Column {
    /// The display name. Not unique: columns are addressed by position.
    pub name: String,
    /// The 0-based position in the captured result.
    pub position: u64,
    /// The wire type, such as `string`, `integer` or `list<string>`.
    #[serde(rename = "type")]
    pub ty: String,
}

/// A cell printed shorter than it is. `read` with `action: cell` returns
/// it whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AbbreviatedCell {
    /// The 0-based captured row.
    pub row: u64,
    /// The 0-based column position.
    pub column: u64,
    /// Bytes of the cell's whole wire JSON text.
    pub total_bytes: u64,
}

/// A captured result as one response shows it: the facts, then the rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ResultView {
    /// The captured result's id, for `read` and `fields`.
    pub result_id: String,
    /// When the result stops being readable, absent once kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// The resolved bounds.
    pub bounds: Bounds,
    /// The text that ran, with the bounds spliced in.
    pub executed_query: String,
    /// Rows the execution produced before the capture cut, exact.
    pub rows_produced: u64,
    /// Rows the capture kept.
    pub rows_captured: u64,
    /// Whether the capture kept fewer rows than the execution produced.
    pub captured_truncated: bool,
    /// Whether the DSL sampled, limited or deduplicated.
    pub dsl_effects: DslEffects,
    /// What this response abbreviated or left out.
    pub presentation: Presentation,
    /// At most three facets over the captured rows.
    pub facets: Vec<Facet>,
    /// The link to Search, or why there is none.
    pub link: Link,
    /// The printed columns, in printed order.
    pub columns: Vec<Column>,
    /// The captured row the first printed row is.
    pub offset: u64,
    /// The printed rows; every cell is one JSON value, positional with
    /// `columns`.
    pub rows: Vec<Vec<Value>>,
    /// The cells printed shorter than they are.
    pub abbreviated: Vec<AbbreviatedCell>,
}

/// One diagnostic of a query trawld refused or found invalid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Diagnostic {
    /// trawld's message.
    pub message: String,
    /// The lines of the sent text around the error with a caret line under
    /// it, cut on character boundaries; absent without a span.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
    /// Byte offset where the error starts in the sent text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<u64>,
    /// Byte offset where the error ends in the sent text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<u64>,
    /// trawld's suggestion, such as a corrected stage name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// The verdict of `search` in validate mode. An invalid query is a
/// successful call whose verdict is `valid: false`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ValidationView {
    /// Whether the query is valid.
    pub valid: bool,
    /// The canonically formatted query, when valid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub formatted: Option<String>,
    /// Why the query is invalid; empty when valid.
    pub errors: Vec<Diagnostic>,
    /// Diagnostics left off the end of `errors` to fit the response
    /// budget.
    #[serde(default)]
    pub diagnostics_dropped_to_fit: u64,
}

// -- per-tool replies ---------------------------------------------------------

/// `orient`: the installation in one call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct OrientReply {
    /// The outcome line.
    pub outcome: String,
    /// trawld's version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_version: Option<String>,
    /// Now, in UTC, RFC 3339.
    pub now_utc: String,
    /// The display zone's label, the MCP host's setting.
    pub display_zone: String,
    /// Now, in the display zone, RFC 3339.
    pub now_display: String,
    /// Why the display zone is not the one configured, when the setting
    /// did not parse and UTC stands in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_zone_note: Option<String>,
    /// The key that answers: its name, prefix and effective permissions.
    pub key: KeyView,
    /// Health as trawld reports it.
    pub health: HealthView,
    /// Environments, services and hosts with their 24-hour volume.
    pub inventory: Vec<InventoryRow>,
    /// Why the inventory is empty or partial, when it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inventory_note: Option<String>,
    /// How far back stored data reaches, per environment.
    pub retention: Vec<RetentionReach>,
    /// Why retention reach is absent, when it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_note: Option<String>,
    /// Fields whose catalog pin is degraded.
    pub degraded_fields: Vec<String>,
    /// Why the degraded fields are unknown, when the schema could not be
    /// read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded_note: Option<String>,
    /// The applied bounds, quotas and admission numbers, by name.
    pub limits: Vec<NamedValue>,
    /// The installed DSL stages and functions.
    pub dsl: DslFeatures,
    /// The DSL in a few lines.
    pub essentials: String,
    /// The DSL reference resources, by URI.
    pub resources: Vec<String>,
    /// At most two proposed next calls.
    pub next_actions: Vec<NextAction>,
}

/// The answering key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct KeyView {
    /// The key's human-readable name.
    pub name: String,
    /// The key's stable 8-character prefix.
    pub prefix: String,
    /// The recognized trawl permissions the key holds on this server.
    pub permissions: Vec<String>,
}

/// trawld's health.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HealthView {
    /// `ok`, `degraded` or `unavailable`.
    pub status: String,
    /// The HTTP status the answer came with: 200 or 503.
    pub http_status: u16,
    /// The checks that are not ok, each as `name: detail`.
    pub problems: Vec<String>,
}

/// One environment, service and host with its last 24 hours. A part the
/// events did not carry is absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InventoryRow {
    /// The environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    /// The service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// The host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Events in the last 24 hours.
    pub events_24h: u64,
    /// The newest event's `_time`, RFC 3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<String>,
}

/// How far back one environment's stored data reaches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RetentionReach {
    /// The environment.
    pub env: String,
    /// Whole days of stored data, the oldest stored date through today
    /// in UTC.
    pub days_kept: u64,
    /// The oldest stored date, `YYYY-MM-DD`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest: Option<String>,
    /// The retention policy in days; 0 keeps the data forever.
    pub max_age_days: u64,
    /// trawld's projection of how much of the policy the disk holds if the
    /// observed days repeat, in words, or why trawld withholds one.
    pub projection: String,
}

/// A named number or setting, printed as `name: value`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NamedValue {
    /// The name, such as `max_result_rows`.
    pub name: String,
    /// The value as text.
    pub value: String,
}

/// The DSL features this trawld installs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DslFeatures {
    /// Pipe stage names.
    pub stages: Vec<String>,
    /// Function names.
    pub functions: Vec<String>,
}

/// `fields`: the fields of a service, or the columns of a captured result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FieldsReply {
    /// The outcome line.
    pub outcome: String,
    /// What was described.
    pub source: FieldsSource,
    /// The service, for `source: service`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// The result id, for `source: result`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_id: Option<String>,
    /// The fields, each with where its facts come from.
    pub fields: Vec<FieldView>,
    /// Fields left off the end of `fields` to fit the response budget.
    #[serde(default)]
    pub fields_dropped_to_fit: u64,
    /// At most two proposed next calls.
    pub next_actions: Vec<NextAction>,
}

/// One field of a service or one column of a result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FieldView {
    /// The field or column name.
    pub name: String,
    /// The column position, for a result's column.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<u64>,
    /// The type, as the catalog pin or the result spells it.
    #[serde(rename = "type")]
    pub ty: String,
    /// Where the facts come from.
    pub provenance: Provenance,
    /// Whether the field's catalog pin is degraded.
    pub degraded: bool,
    /// The rows or events the facts count over, when known: the rows the
    /// catalog observed in the service, the rows in the service's parquet
    /// footers, or the captured rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u64>,
    /// Rows or events holding no value, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nulls: Option<u64>,
    /// Distinct values, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distinct: Option<u64>,
    /// When the field was last observed, RFC 3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<String>,
    /// Up to five values with counts, most frequent first.
    pub values: Vec<FacetValue>,
}

/// `search`: a validation verdict, or a captured result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SearchReply {
    /// The outcome line.
    pub outcome: String,
    /// Which mode ran.
    pub mode: SearchMode,
    /// The verdict, for `mode: validate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation: Option<ValidationView>,
    /// The captured result, for `mode: execute`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ResultView>,
    /// Computed hints: dominance, the nearest bounds holding data, a
    /// lagging sender, a field the catalog holds no pin or observation of.
    pub hints: Vec<String>,
    /// At most two proposed next calls.
    pub next_actions: Vec<NextAction>,
}

/// `read`: a page of a captured result, or one cell's text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ReadReply {
    /// The outcome line.
    pub outcome: String,
    /// Which action ran.
    pub action: ReadAction,
    /// The page, for `action: page`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ResultView>,
    /// What the correlation read that made the result established, on
    /// every page of a `context`, `sightings` or `compare` result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation: Option<CorrelationView>,
    /// The cell range, for `action: cell`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell: Option<CellView>,
    /// At most two proposed next calls.
    pub next_actions: Vec<NextAction>,
}

/// A byte range of one captured cell's wire JSON text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CellView {
    /// The captured result's id.
    pub result_id: String,
    /// The 0-based captured row.
    pub row: u64,
    /// The 0-based column position.
    pub column: u64,
    /// The column's display name.
    pub name: String,
    /// The column's wire type.
    #[serde(rename = "type")]
    pub ty: String,
    /// Byte offset of `text` in the cell's wire text.
    pub offset: u64,
    /// Bytes of `text`.
    pub length: u64,
    /// Bytes of the whole wire text.
    pub total_bytes: u64,
    /// Where the next range starts, absent at the end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<u64>,
    /// The range of the cell's wire JSON text.
    pub text: String,
}

/// `context`, `sightings` and `compare`: a correlation read's facts, then
/// the first page of the result it captured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CorrelationReply {
    /// The outcome line.
    pub outcome: String,
    /// What the read established, printed before any row and never
    /// dropped to fit.
    pub correlation: CorrelationView,
    /// The captured result and its first page.
    pub result: ResultView,
    /// At most two proposed next calls: the page continuation, then the
    /// read's follow-up queries.
    pub next_actions: Vec<NextAction>,
}

/// What a correlation read established. `kind` names the read, and the
/// member of that name holds its facts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CorrelationView {
    /// Which read made the result.
    pub kind: CorrelationKind,
    /// The facts, for `kind: context`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<ContextView>,
    /// The facts, for `kind: sightings`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sightings: Option<SightingsView>,
    /// The facts, for `kind: compare`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compare: Option<CompareView>,
}

/// The facts of a `context` read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ContextView {
    /// The constraints the read applied.
    pub scope_used: ScopeView,
    /// The row the read was centred on.
    pub focus: FocusView,
    /// Captured rows strictly earlier than the focus row's `_time`.
    pub earlier: RowGroup,
    /// Captured rows sharing the focus row's `_time`, the focus event
    /// among them and not told apart.
    pub same_time: RowGroup,
    /// Captured rows strictly later than the focus row's `_time`.
    pub later: RowGroup,
    /// That this is a new read and equal timestamps establish no order.
    pub statement: String,
}

/// The constraints a `context` read applied; a key the scope left free is
/// null.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ScopeView {
    /// The `env` the rows were held to.
    pub env: Option<String>,
    /// The `host` the rows were held to.
    pub host: Option<String>,
    /// Seconds either side of the focus row's `_time`.
    pub seconds: u64,
}

/// The focus row of a `context` read and the values taken from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FocusView {
    /// The captured result holding the focus row.
    pub result_id: String,
    /// The focus row's 0-based captured row.
    pub row: u64,
    /// The focus row's `_time`, RFC 3339 with microseconds in UTC.
    pub time: String,
    /// The focus row's `env`, null when it has none.
    pub env: Option<String>,
    /// The focus row's `host`, null when it has none.
    pub host: Option<String>,
}

/// One group of a `context` read: the captured rows `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RowGroup {
    /// The group's first captured row.
    pub start: u64,
    /// One past its last captured row.
    pub end: u64,
    /// Rows in the group.
    pub rows: u64,
}

/// The facts of a `sightings` read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SightingsView {
    /// The literal searched for.
    pub literal: String,
    /// Events the search matched in the bounds, every one counted.
    pub matches_total: u64,
    /// The most recent matches the result captured.
    pub captured: u64,
    /// What the counts are over, in one sentence.
    pub denominators: String,
    /// The fields attribution was restricted to, from the captured facts so
    /// every page of the result states it; absent when attribution ran over
    /// every field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution_fields: Option<Vec<String>>,
    /// Matches per (`env`, `service`), exact over every match, most first,
    /// each `[env, service, matches, first_seen, last_seen]`: the env and
    /// service a JSON string or null, the matches a number, and the
    /// earliest and latest matching `_time` RFC 3339 with microseconds in
    /// UTC.
    pub groups: Vec<Vec<Value>>,
    /// Matches per `service`, summed from `groups`, most first.
    pub per_service: Vec<KeyCount>,
    /// Matches per `env`, summed from `groups`, most first.
    pub per_env: Vec<KeyCount>,
    /// Captured matches per field holding the literal.
    pub fields: Vec<KeyCount>,
    /// Fields holding the literal that trawld did not list, because more
    /// than 256 held it; `fields` in the call attributes them.
    #[serde(default)]
    pub fields_omitted: u64,
    /// Captured matches in which no field but `_raw` holds the literal.
    pub raw_only_rows: u64,
    /// Entries left off the ends of `groups`, `per_service`, `per_env` and
    /// `fields` together, to fit the response budget once nothing else
    /// could go.
    #[serde(default)]
    pub entries_dropped_to_fit: u64,
}

/// A key, null when events carry none, and a count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct KeyCount {
    /// The key: a service, an environment or a field name.
    pub key: Option<String>,
    /// The count.
    pub count: u64,
}

/// The facts of a `compare` read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CompareView {
    /// The window under question.
    pub foreground: WindowView,
    /// The window it was compared against.
    pub baseline: WindowView,
    /// The fields the counts are grouped by.
    pub group_by: Vec<String>,
    /// The count a group reached in either window to be listed.
    pub min_count: u64,
    /// The unit of every rate column, such as `per_minute`.
    pub rate_unit: String,
    /// Groups listed, ranked: absolute rate change descending, then count,
    /// then group key.
    pub rows_ranked: u64,
    /// Every warning the read raised, as trawld worded it.
    pub warnings: Vec<WarningView>,
}

/// One resolved `compare` window, half-open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WindowView {
    /// The inclusive start, RFC 3339 with microseconds in UTC.
    pub start: String,
    /// The exclusive end, RFC 3339 with microseconds in UTC.
    pub end: String,
    /// `end - start` in milliseconds.
    pub duration_ms: u64,
}

/// A warning a `compare` read raised: what it could not vouch for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WarningView {
    /// `retention_reach` or `degraded_pin`, as trawld names it.
    pub code: String,
    /// trawld's wording.
    pub message: String,
    /// The group field it is about, when it is about one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
}
