// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tool inputs and the tool definitions `tools/list` sends (ADR-0058).
//!
//! Inputs are flat objects: one enum `mode`, `action` or `source` field
//! picks the variant, and the fields only one variant reads are optional.
//! No schema has a root `anyOf` or `oneOf`, and every property name stays
//! within `[A-Za-z0-9_.-]`, because Claude Code drops a tool that breaks
//! either rule. Schemas are generated as JSON Schema 2020-12 by rmcp's
//! `schema_for_input` and `schema_for_output`; `tests/schemas.rs` holds
//! them to those rules. An optional field without a default skips
//! `None`, so its schema carries no `"default": null` its type refuses.

use std::sync::Arc;

use rmcp::handler::server::tool::{schema_for_input, schema_for_output};
use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use schemars::JsonSchema;
use serde::Deserialize;

use trawl_api::results::{
    COMPARE_DEFAULT_MIN_COUNT, COMPARE_MAX_GROUP_FIELDS, CONTEXT_DEFAULT_SECONDS,
    CONTEXT_DEFAULT_SIDE_ROWS, CONTEXT_MAX_SIDE_ROWS,
};

use crate::reply::{
    ContextScope, CorrelationReply, FieldsReply, FieldsSource, OrientReply, ReadAction, ReadReply,
    SearchMode, SearchReply,
};

/// The `orient` tool's name.
pub const ORIENT: &str = "orient";
/// The `search` tool's name.
pub const SEARCH: &str = "search";
/// The `read` tool's name.
pub const READ: &str = "read";
/// The `fields` tool's name.
pub const FIELDS: &str = "fields";
/// The `context` tool's name.
pub const CONTEXT: &str = "context";
/// The `sightings` tool's name.
pub const SIGHTINGS: &str = "sightings";
/// The `compare` tool's name.
pub const COMPARE: &str = "compare";

/// The most fields a `sightings` call may restrict attribution to.
pub const SIGHTINGS_MAX_FIELDS: usize = 16;
/// The longest literal a `sightings` call may send, in bytes.
pub const SIGHTINGS_MAX_LITERAL_BYTES: usize = 1024;

/// Rows a response prints when the call names none.
pub const DEFAULT_ROWS: u32 = 50;
/// The most rows one response prints.
pub const MAX_ROWS: u32 = 200;
/// The response budget, text block plus mirror, when the call names none.
pub const DEFAULT_MAX_BYTES: u32 = 16 * 1024;
/// The smallest response budget a call may ask for.
pub const MIN_MAX_BYTES: u32 = 4 * 1024;
/// The hard response budget, text block plus mirror.
pub const HARD_MAX_BYTES: u32 = 32 * 1024;

/// `orient` takes no arguments.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrientInput {}

/// `search`: validate or execute one DSL query.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchInput {
    /// `validate` checks the query without running it; `execute` runs it
    /// and keeps the rows as a result `read` can page.
    pub mode: SearchMode,
    /// The trawl DSL query. Put its time bounds in the text, such as
    /// `last=1h` or `earliest="2026-03-14T03:00:00Z"`.
    pub query: String,
    /// Rows to print, for `execute`.
    #[serde(default)]
    #[schemars(with = "u32", range(min = 1, max = MAX_ROWS), extend("default" = DEFAULT_ROWS))]
    pub rows: Option<u32>,
    /// Bytes the whole response may use, for `execute`.
    #[serde(default)]
    #[schemars(
        with = "u32",
        range(min = MIN_MAX_BYTES, max = HARD_MAX_BYTES),
        extend("default" = DEFAULT_MAX_BYTES)
    )]
    pub max_bytes: Option<u32>,
}

/// `read`: page a captured result, or read one cell whole.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadInput {
    /// `page` reads rows; `cell` reads one cell's full text.
    pub action: ReadAction,
    /// The id `search` returned for the captured result.
    pub result_id: String,
    /// For `page`: the first captured row to print, 0-based.
    #[serde(default)]
    #[schemars(with = "u64", extend("default" = 0))]
    pub offset: Option<u64>,
    /// For `page`: rows to print.
    #[serde(default)]
    #[schemars(with = "u32", range(min = 1, max = MAX_ROWS), extend("default" = DEFAULT_ROWS))]
    pub rows: Option<u32>,
    /// For `cell`: the 0-based captured row.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "u64")]
    pub row: Option<u64>,
    /// For `cell`: the 0-based column position.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "u64")]
    pub column: Option<u64>,
    /// For `cell`: the byte offset in the cell's text to continue from.
    #[serde(default)]
    #[schemars(with = "u64", extend("default" = 0))]
    pub byte_offset: Option<u64>,
    /// Bytes the whole response may use.
    #[serde(default)]
    #[schemars(
        with = "u32",
        range(min = MIN_MAX_BYTES, max = HARD_MAX_BYTES),
        extend("default" = DEFAULT_MAX_BYTES)
    )]
    pub max_bytes: Option<u32>,
}

/// `fields`: describe a service's fields or a captured result's columns.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FieldsInput {
    /// `service` lists the fields the catalog observed for `service`;
    /// `result` lists the columns of `result_id`.
    pub source: FieldsSource,
    /// For `service`: the service name.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    pub service: Option<String>,
    /// For `result`: the id `search` returned.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    pub result_id: Option<String>,
}

/// `context`: the rows around one captured row, on its env and host.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextInput {
    /// The id of the captured result holding the focus row.
    pub result_id: String,
    /// The focus row: its 0-based captured row in that result.
    pub row: u64,
    /// Which of the focus row's keys hold the rows.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "ContextScope", extend("default" = "env_and_host"))]
    pub scope: Option<ContextScope>,
    /// Seconds either side of the focus row's `_time`.
    #[serde(default)]
    #[schemars(with = "u64", range(min = 1), extend("default" = CONTEXT_DEFAULT_SECONDS))]
    pub seconds: Option<u64>,
    /// Strictly earlier rows to return.
    #[serde(default)]
    #[schemars(
        with = "u64",
        range(max = CONTEXT_MAX_SIDE_ROWS),
        extend("default" = CONTEXT_DEFAULT_SIDE_ROWS)
    )]
    pub before: Option<u64>,
    /// Strictly later rows to return.
    #[serde(default)]
    #[schemars(
        with = "u64",
        range(max = CONTEXT_MAX_SIDE_ROWS),
        extend("default" = CONTEXT_DEFAULT_SIDE_ROWS)
    )]
    pub after: Option<u64>,
    /// Bytes the whole response may use.
    #[serde(default)]
    #[schemars(
        with = "u32",
        range(min = MIN_MAX_BYTES, max = HARD_MAX_BYTES),
        extend("default" = DEFAULT_MAX_BYTES)
    )]
    pub max_bytes: Option<u32>,
}

/// `sightings`: where one literal occurs, by service, environment and
/// field. The bounds are `last`, or `earliest` and `latest`; with none,
/// the last 24 hours.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SightingsInput {
    /// The text to find, matched as a literal substring of `message` and
    /// `_raw`, case-insensitively.
    #[schemars(length(min = 1, max = SIGHTINGS_MAX_LITERAL_BYTES))]
    pub literal: String,
    /// The inclusive start, an RFC 3339 instant such as
    /// `2026-03-14T03:00:00Z`.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    pub earliest: Option<String>,
    /// The exclusive end, an RFC 3339 instant; open, it ends now.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    pub latest: Option<String>,
    /// A duration ending now, such as `24h` or `7d`, instead of `earliest`
    /// and `latest`. At most 7 days.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    pub last: Option<String>,
    /// Restrict which fields the literal is attributed to. The matches
    /// counted stay the same.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Vec<String>", length(max = SIGHTINGS_MAX_FIELDS))]
    pub fields: Option<Vec<String>>,
    /// Bytes the whole response may use.
    #[serde(default)]
    #[schemars(
        with = "u32",
        range(min = MIN_MAX_BYTES, max = HARD_MAX_BYTES),
        extend("default" = DEFAULT_MAX_BYTES)
    )]
    pub max_bytes: Option<u32>,
}

/// `compare`: one filter's counts in a foreground window against a
/// baseline window, from one statement. Every bound is an RFC 3339
/// instant such as `2026-03-14T03:00:00Z`; each window is half-open.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CompareInput {
    /// DSL search terms, such as `service=auth "failed password"`: no
    /// pipe stage and no time clause.
    pub filter: String,
    /// The foreground's inclusive start.
    pub foreground_earliest: String,
    /// The foreground's exclusive end.
    pub foreground_latest: String,
    /// The baseline's inclusive start. With neither baseline bound, the
    /// baseline is the window of equal duration just before the
    /// foreground.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    pub baseline_earliest: Option<String>,
    /// The baseline's exclusive end.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    pub baseline_latest: Option<String>,
    /// At most two distinct fields to group the counts by.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Vec<String>", length(max = COMPARE_MAX_GROUP_FIELDS))]
    pub group_by: Option<Vec<String>>,
    /// A group is listed when either window counts at least this many
    /// events for it.
    #[serde(default)]
    #[schemars(with = "u64", range(min = 1), extend("default" = COMPARE_DEFAULT_MIN_COUNT))]
    pub min_count: Option<u64>,
    /// Bytes the whole response may use.
    #[serde(default)]
    #[schemars(
        with = "u32",
        range(min = MIN_MAX_BYTES, max = HARD_MAX_BYTES),
        extend("default" = DEFAULT_MAX_BYTES)
    )]
    pub max_bytes: Option<u32>,
}

/// The tools in the fixed order `tools/list` sends them.
#[must_use]
pub fn tools() -> Vec<Tool> {
    vec![
        orient(),
        search(),
        read(),
        context(),
        sightings(),
        compare(),
        fields(),
    ]
}

/// A corpus tool: each call records a new captured result, so it is not
/// read-only and not idempotent, but additive, never destructive.
fn corpus_annotations(title: &str) -> ToolAnnotations {
    ToolAnnotations::with_title(title)
        .read_only(false)
        .destructive(false)
        .idempotent(false)
        .open_world(true)
}

/// A read-only tool: it changes nothing, so repeating it is harmless.
fn read_only_annotations(title: &str) -> ToolAnnotations {
    ToolAnnotations::with_title(title)
        .read_only(true)
        .destructive(false)
        .idempotent(true)
        .open_world(true)
}

fn orient() -> Tool {
    tool::<OrientInput, OrientReply>(
        ORIENT,
        "Describe this trawl installation in one call: version, the time now, environments, \
         services and hosts with 24-hour volume, health, your permissions, the limits, and a \
         short guide to the query language. Call it first.",
        read_only_annotations("Orient on this trawl installation"),
    )
}

fn search() -> Tool {
    tool::<SearchInput, SearchReply>(
        SEARCH,
        "Validate or run one trawl DSL query. mode=execute keeps the rows as a result and \
         answers with a summary, facets and the first rows; page the rest with read instead \
         of running the query again.",
        corpus_annotations("Search logs"),
    )
}

fn read() -> Tool {
    tool::<ReadInput, ReadReply>(
        READ,
        "Read a result that search captured, without running the query again: action=page \
         prints rows from offset, action=cell prints one whole cell that a page abbreviated.",
        read_only_annotations("Read a captured result"),
    )
}

fn context() -> Tool {
    tool::<ContextInput, CorrelationReply>(
        CONTEXT,
        "Show what happened around one captured row: the rows strictly before it, the rows at \
         its exact _time and the rows after it, on its env and host, as a new captured result. \
         Name the row by result_id and row from search or read.",
        corpus_annotations("Show context around a row"),
    )
}

fn sightings() -> Tool {
    tool::<SightingsInput, CorrelationReply>(
        SIGHTINGS,
        "Find where one literal, such as an IP address or a user name, occurs: exact match \
         counts with first and last seen per env and service, the fields that hold it in the \
         captured matches, and follow-up queries. Bounds default to the last 24 hours.",
        corpus_annotations("Find sightings of a literal"),
    )
}

fn compare() -> Tool {
    tool::<CompareInput, CorrelationReply>(
        COMPARE,
        "Compare one filter's event counts and rates in a foreground window against a baseline \
         window (default: the equal-duration window just before), grouped by up to two fields, \
         from one snapshot. Lists new and vanished groups and every warning.",
        corpus_annotations("Compare two windows"),
    )
}

fn fields() -> Tool {
    tool::<FieldsInput, FieldsReply>(
        FIELDS,
        "List the fields of one service (source=service) or the columns of a captured result \
         (source=result), each with where its facts come from.",
        read_only_annotations("List fields"),
    )
}

fn tool<I, O>(name: &'static str, description: &'static str, annotations: ToolAnnotations) -> Tool
where
    I: JsonSchema + 'static,
    O: JsonSchema + 'static,
{
    let input: Arc<JsonObject> = schema_for_input::<I>()
        .unwrap_or_else(|error| panic!("the {name} input schema is not an object: {error}"));
    let title = annotations.title.clone().unwrap_or_default();
    Tool::new(name, description, input)
        .with_title(title)
        .with_raw_output_schema(schema_for_output::<O>())
        .with_annotations(annotations)
}
