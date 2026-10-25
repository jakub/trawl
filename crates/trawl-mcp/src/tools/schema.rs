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

use crate::reply::{
    FieldsReply, FieldsSource, InvestigationAction, InvestigationReply, OrientReply, ReadAction,
    ReadReply, SearchMode, SearchReply,
};

/// The `orient` tool's name.
pub const ORIENT: &str = "orient";
/// The `search` tool's name.
pub const SEARCH: &str = "search";
/// The `read` tool's name.
pub const READ: &str = "read";
/// The `fields` tool's name.
pub const FIELDS: &str = "fields";
/// The `investigation` tool's name.
pub const INVESTIGATION: &str = "investigation";

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

/// `read`: page a captured result, read one cell whole, or read an
/// investigation.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadInput {
    /// `page` reads rows; `cell` reads one cell's full text;
    /// `investigation` reads an investigation's entries.
    pub action: ReadAction,
    /// For `page` and `cell`: the id `search` returned for the captured
    /// result.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    pub result_id: Option<String>,
    /// For `investigation`: the investigation's id.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    pub investigation_id: Option<String>,
    /// For `investigation`: print the entries changed after this sequence;
    /// 0 prints from the start.
    #[serde(default)]
    #[schemars(with = "u64", extend("default" = 0))]
    pub since: Option<u64>,
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

/// A status `investigation` writes: a finding's `hypothesis`,
/// `supported`, `refuted` or `unresolved` (for `finding` and
/// `finding_status`), or the investigation's `open` or `closed` (for
/// `status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(inline)]
pub enum StatusWord {
    Hypothesis,
    Supported,
    Refuted,
    Unresolved,
    Open,
    Closed,
}

/// A row of a captured result a finding cites.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RowRef {
    /// The captured result's id, kept for this investigation.
    pub result_id: String,
    /// The 0-based row number in it.
    pub row: u64,
}

/// `investigation`: open, read, list and write to an investigation that
/// other keys read too (ADR-0059). Each field names the actions that read
/// it; a field the action does not read is refused.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InvestigationInput {
    /// `create` opens one; `read` prints its entries; `list` lists every
    /// investigation; `note`, `finding` and `keep` append; `finding_status`
    /// sets a finding's status; `status` closes or reopens it.
    pub action: InvestigationAction,
    /// The investigation's id, 32 lowercase hex digits: for every action
    /// but `list`. `create` mints one when it is absent; send the printed
    /// id again to recover a create whose answer was lost.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    pub id: Option<String>,
    /// For `create`: a short title, at most 200 bytes.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    pub title: Option<String>,
    /// For `create`: the question it answers, at most 2048 bytes.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    pub question: Option<String>,
    /// For `create`: what it covers, at most 2048 bytes.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    pub scope: Option<String>,
    /// For `note`: the text, at most 4096 bytes.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    pub text: Option<String>,
    /// For `finding`: the statement, at most 4096 bytes.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    pub statement: Option<String>,
    /// For `finding` and `finding_status`: `hypothesis`, `supported`,
    /// `refuted` or `unresolved`. For `status`: `open` or `closed`.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "StatusWord")]
    pub status: Option<StatusWord>,
    /// For `finding`: the rows that support it, at most 32.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Vec<RowRef>")]
    pub references: Option<Vec<RowRef>>,
    /// For `finding`: the rows that count against it, at most 32.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Vec<RowRef>")]
    pub counterevidence: Option<Vec<RowRef>>,
    /// For `finding`: what the evidence leaves open, at most 1024 bytes.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    pub remaining_question: Option<String>,
    /// For `keep`: the captured result whose rows you keep.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    pub result_id: Option<String>,
    /// For `keep`: the 0-based row numbers, at most 200.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Vec<u64>")]
    pub rows: Option<Vec<u64>>,
    /// For `keep`: why, at most 1024 bytes.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "String")]
    pub reason: Option<String>,
    /// For `finding_status`: the finding's sequence, which is its id.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "u64")]
    pub finding_seq: Option<u64>,
    /// For `finding_status` and `status`: the investigation's `seq` you
    /// last saw. A write since then refuses the overwrite.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "u64")]
    pub expected_seq: Option<u64>,
    /// For `read`: print the entries changed after this sequence; 0 prints
    /// from the start.
    #[serde(default)]
    #[schemars(with = "u64", extend("default" = 0))]
    pub since: Option<u64>,
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
    vec![orient(), search(), read(), fields(), investigation()]
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
        // A capture records a new result each call: not read-only and not
        // idempotent, but additive, never destructive.
        ToolAnnotations::with_title("Search logs")
            .read_only(false)
            .destructive(false)
            .idempotent(false)
            .open_world(true),
    )
}

fn read() -> Tool {
    tool::<ReadInput, ReadReply>(
        READ,
        "Read a result that search captured, without running the query again: action=page \
         prints rows from offset, action=cell prints one whole cell that a page abbreviated. \
         action=investigation prints an investigation's entries changed after since.",
        read_only_annotations("Read a captured result"),
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

fn investigation() -> Tool {
    tool::<InvestigationInput, InvestigationReply>(
        INVESTIGATION,
        "Work in an investigation other keys read too: create one (you get its id), read its \
         entries, list them, append a note, a finding or a keep of rows from a captured \
         result, set a finding's status, or close and reopen it (status with expected_seq). \
         Every write prints what other keys wrote since this session last saw it, each entry \
         once. Changing the title or question, deleting an investigation and extending its \
         evidence are not here; deletion and extension are operator actions that need \
         server_manage.",
        // A write appends to a shared log: not read-only, not idempotent
        // (a repeated note is a second note), never destructive.
        ToolAnnotations::with_title("Work in an investigation")
            .read_only(false)
            .destructive(false)
            .idempotent(false)
            .open_world(true),
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
