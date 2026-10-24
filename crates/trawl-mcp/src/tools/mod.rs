// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The fixed tool table and dispatch (ADR-0058).
//!
//! [`schema::tools`] is the table, in the order `tools/list` sends it:
//! `orient`, `search`, `read`, `fields`. [`dispatch`] routes one
//! `tools/call` to its tool after decoding the arguments into the tool's
//! input type. Each tool module has a `run` function that takes the
//! backend, the decoded input and the instant it treats as now, so a test
//! can call it with a fixed clock.

pub mod orient;
pub mod read;
pub mod schema;
pub mod search;

use chrono::Utc;
use rmcp::ErrorData;
use rmcp::model::{CallToolResult, ContentBlock, JsonObject};
use serde::de::DeserializeOwned;
use serde_json::json;
use trawl_client::{CapturedResult, ClientError};
use trawl_core::ast::PipeStage;

use crate::backend::Backend;
use crate::render::rows::{EncodedPage, json_string};
use crate::reply::{Bounds, DslEffects, Facet, Link, NextAction, Presentation, ResultView};
use schema::{FIELDS, FieldsInput, ORIENT, OrientInput, READ, ReadInput, SEARCH, SearchInput};

/// Run the tool `name` with `arguments` against `backend`, printing times
/// in `display_zone` where a tool prints a display time.
///
/// Arguments that do not decode into the tool's input are a tool error the
/// model can read and correct, not a protocol error. An unknown tool name
/// is a protocol error, `-32602`.
pub async fn dispatch<B: Backend>(
    name: &str,
    arguments: Option<JsonObject>,
    backend: &B,
    display_zone: &str,
) -> Result<CallToolResult, ErrorData> {
    match name {
        ORIENT => Ok(match decode::<OrientInput>(name, arguments) {
            Ok(_input) => orient::run(backend, display_zone, Utc::now()).await,
            Err(refusal) => refusal,
        }),
        SEARCH => Ok(match decode::<SearchInput>(name, arguments) {
            Ok(input) => search::run(backend, input, Utc::now()).await,
            Err(refusal) => refusal,
        }),
        READ => Ok(match decode::<ReadInput>(name, arguments) {
            Ok(input) => read::run(backend, input).await,
            Err(refusal) => refusal,
        }),
        FIELDS => Ok(match decode::<FieldsInput>(name, arguments) {
            Ok(_input) => not_implemented(name),
            Err(refusal) => refusal,
        }),
        _ => Err(ErrorData::invalid_params(
            "unknown tool; the tools are orient, search, read and fields",
            None,
        )),
    }
}

/// Decode a call's arguments; absent arguments are an empty object.
fn decode<I: DeserializeOwned>(
    name: &str,
    arguments: Option<JsonObject>,
) -> Result<I, CallToolResult> {
    serde_json::from_value(serde_json::Value::Object(arguments.unwrap_or_default()))
        .map_err(|error| crate::render::refusal::invalid_arguments(name, &error))
}

/// The placeholder each tool answers until its implementation lands.
fn not_implemented(name: &str) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(format!(
        "{name} is not implemented in this checkpoint"
    ))])
}

/// A refusal or failure in a few words, for a tool that carries on without
/// the part that failed: the error code and HTTP status trawld sent, or
/// the class of a failure on the way. Never a library reason or a path.
pub(crate) fn failure_summary(error: &ClientError) -> String {
    match error {
        ClientError::Server {
            status,
            error: envelope,
            ..
        } => {
            let code = serde_json::to_value(&envelope.code)
                .ok()
                .and_then(|code| code.as_str().map(str::to_owned))
                .unwrap_or_else(|| "unknown".to_owned());
            format!("{code} (HTTP {status})")
        }
        ClientError::Network(_) => "the request to trawld failed in transport".to_owned(),
        ClientError::Parse { .. } => "trawld's response did not parse".to_owned(),
        ClientError::TooLarge { cap, .. } => {
            format!("trawld's response was larger than the {cap}-byte cap")
        }
        ClientError::InvalidCa(_) | ClientError::InvalidUrl(_) => {
            "the profile's server settings are not usable".to_owned()
        }
    }
}

/// Whether the DSL itself sampled, limited or deduplicated, read from the
/// executed text's pipe stages. A text that does not parse claims none.
pub(crate) fn dsl_effects(executed_query: &str) -> DslEffects {
    let mut effects = DslEffects {
        sampled: false,
        limited: false,
        deduplicated: false,
    };
    if let Ok(query) = trawl_core::parser::parse(executed_query) {
        for stage in &query.pipeline {
            match stage.node {
                PipeStage::Sample(_) => effects.sampled = true,
                PipeStage::Limit(_) | PipeStage::Tail(_) => effects.limited = true,
                PipeStage::Dedup(_) => effects.deduplicated = true,
                _ => {}
            }
        }
    }
    effects
}

/// A captured result's page as a [`ResultView`], before the renderer
/// counts what it prints.
pub(crate) fn result_view(
    result: &CapturedResult,
    offset: u64,
    page: EncodedPage,
    facets: Vec<Facet>,
    link: Link,
) -> ResultView {
    let columns_omitted = result.columns.len().saturating_sub(page.columns.len());
    ResultView {
        result_id: result.id.clone(),
        expires_at: result.expires_at.clone(),
        bounds: Bounds {
            earliest: result.earliest.clone(),
            latest: result.latest.clone(),
        },
        executed_query: result.executed_query.clone(),
        rows_produced: result.total_rows,
        rows_captured: result.captured_rows,
        captured_truncated: result.truncated,
        dsl_effects: dsl_effects(&result.executed_query),
        presentation: Presentation {
            cells_abbreviated: 0,
            columns_omitted: crate::render::rows::as_u64(columns_omitted),
            rows_omitted: 0,
            rows_dropped_to_fit: 0,
            facets_dropped_to_fit: false,
            hints_dropped_to_fit: false,
            next_actions_dropped_to_fit: false,
        },
        facets,
        link,
        columns: page.columns,
        offset,
        rows: page.rows,
        abbreviated: page.abbreviated,
    }
}

/// At most two continuations of a page: the next page, when captured rows
/// follow the rows read, and the first abbreviated cell whole.
///
/// The page continuation's `offset` is the end of the rows the view
/// holds, the shape the renderer moves when its budget prints fewer.
pub(crate) fn continuations(view: &ResultView) -> Vec<NextAction> {
    let mut actions = Vec::new();
    let read = crate::render::rows::as_u64(view.rows.len());
    let end = view.offset.saturating_add(read);
    if read > 0 && end < view.rows_captured {
        actions.push(NextAction {
            tool: schema::READ.to_owned(),
            arguments: orient::object(json!({
                "action": "page",
                "result_id": view.result_id,
                "offset": end,
                "rows": read,
            })),
            reason: "print the next captured rows without running the query again".to_owned(),
        });
    }
    if let Some(cell) = view.abbreviated.first() {
        actions.push(NextAction {
            tool: schema::READ.to_owned(),
            arguments: orient::object(json!({
                "action": "cell",
                "result_id": view.result_id,
                "row": cell.row,
                "column": cell.column,
            })),
            reason: format!(
                "print the whole of the {}-byte cell this page abbreviated",
                cell.total_bytes
            ),
        });
    }
    actions
}

/// The error text for a page that came back with a cell its column type
/// does not hold, naming the result so the caller can read it otherwise.
pub(crate) fn unencodable(
    tool: &str,
    result_id: &str,
    error: &serde_json::Error,
) -> CallToolResult {
    crate::render::refusal::error_result(format!(
        "{tool} read result {} but a cell does not fit its column's type: {}",
        json_string(result_id),
        crate::render::one_line(&error.to_string())
    ))
}
