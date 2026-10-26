// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The fixed tool table and dispatch (ADR-0058).
//!
//! [`schema::tools`] is the table, in the order `tools/list` sends it:
//! `orient`, `search`, `read`, `context`, `sightings`, `compare`, `fields`,
//! `investigation`. [`dispatch`] routes one
//! `tools/call` to its tool after decoding the arguments into the tool's
//! input type. `context`, `sightings` and `compare` share
//! [`correlation_answer`], which reads the first page of the result a
//! correlation read captured and renders it with its block. Each tool module has a `run` function that takes the
//! backend, the decoded input and the instant it treats as now, so a test
//! can call it with a fixed clock.

mod attach;
pub mod compare;
pub mod context;
pub mod fields;
pub mod investigation;
pub mod orient;
pub mod read;
pub mod schema;
pub mod search;
pub mod sightings;

use chrono::Utc;
use rmcp::ErrorData;
use rmcp::model::{CallToolResult, JsonObject};
use serde::de::DeserializeOwned;
use serde_json::json;
use trawl_api::CapabilitiesResponse;
use trawl_api::value::wire_cell_text;
use trawl_client::{
    CapturedResult, ClientError, Correlation, CorrelationResponse, ResultColumn, ResultPage,
    ValueCount,
};
use trawl_core::ast::{PipeStage, Spanned};

use crate::backend::Backend;
use crate::render::link::search_link;
use crate::render::rows::{ABBREVIATE_OVER_BYTES, EncodedPage, encode_page, json_string};
use crate::render::{self, Budget, MAX_FACET_VALUES, refusal};
use crate::reply::{
    Bounds, CorrelationReply, DslEffects, Facet, FacetValue, Link, NextAction, Presentation,
    ResultView,
};
use crate::session::Seen;
use schema::{
    COMPARE, CONTEXT, CompareInput, ContextInput, DEFAULT_ROWS, FIELDS, FieldsInput, INVESTIGATION,
    InvestigationInput, ORIENT, OrientInput, READ, ReadInput, SEARCH, SIGHTINGS, SearchInput,
    SightingsInput,
};

/// Run the tool `name` with `arguments` against `backend`, printing times
/// in `display_zone` where a tool prints a display time and keeping the
/// session's investigation bookkeeping in `seen`.
///
/// Arguments that do not decode into the tool's input are a tool error the
/// model can read and correct, not a protocol error. An unknown tool name
/// is a protocol error, `-32602`.
pub async fn dispatch<B: Backend>(
    name: &str,
    arguments: Option<JsonObject>,
    backend: &B,
    display_zone: &str,
    seen: &Seen,
) -> Result<CallToolResult, ErrorData> {
    match name {
        ORIENT => Ok(match decode::<OrientInput>(name, arguments) {
            Ok(_input) => orient::run(backend, display_zone, Utc::now()).await,
            Err(refusal) => refusal,
        }),
        SEARCH => Ok(match decode::<SearchInput>(name, arguments) {
            Ok(input) => search::run(backend, seen, input, Utc::now()).await,
            Err(refusal) => refusal,
        }),
        READ => Ok(match decode::<ReadInput>(name, arguments) {
            Ok(input) => read::run(backend, seen, input).await,
            Err(refusal) => refusal,
        }),
        CONTEXT => Ok(match decode::<ContextInput>(name, arguments) {
            Ok(input) => context::run(backend, input).await,
            Err(refusal) => refusal,
        }),
        SIGHTINGS => Ok(match decode::<SightingsInput>(name, arguments) {
            Ok(input) => sightings::run(backend, input).await,
            Err(refusal) => refusal,
        }),
        COMPARE => Ok(match decode::<CompareInput>(name, arguments) {
            Ok(input) => compare::run(backend, input).await,
            Err(refusal) => refusal,
        }),
        FIELDS => Ok(match decode::<FieldsInput>(name, arguments) {
            Ok(input) => fields::run(backend, input).await,
            Err(refusal) => refusal,
        }),
        INVESTIGATION => Ok(match decode::<InvestigationInput>(name, arguments) {
            Ok(input) => investigation::run(backend, seen, input).await,
            Err(refusal) => refusal,
        }),
        _ => Err(ErrorData::invalid_params(
            "unknown tool; the tools are orient, search, read, context, sightings, compare, \
             fields and investigation",
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

/// The rows a page asks trawld for: the call's `rows`, held to the
/// `[capture] page_max_rows` trawld's `capabilities` report, because trawld
/// refuses a longer page. Every continuation the reply proposes asks for
/// these rows too. When the capabilities could not be read, the call's
/// `rows`, and trawld's answer stands.
pub(crate) fn page_rows(rows: u64, capabilities: Option<&CapabilitiesResponse>) -> u64 {
    capabilities.map_or(rows, |capabilities| {
        rows.min(capabilities.bounds.page_max_rows)
    })
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
            reason: crate::render::budget::PAGE_CONTINUATION.to_owned(),
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

/// The first page of `rows` rows of captured result `result_id`, its
/// offset and its encoding; or `tool`'s error, opening with
/// `acknowledgement` when the capture was attached and capped once with
/// it, so the request id and the `Retry-After` wait of a refused page
/// survive.
pub(crate) async fn first_page<B: Backend>(
    backend: &B,
    tool: &str,
    result_id: &str,
    rows: u64,
    acknowledgement: &str,
) -> Result<(ResultPage, u64, EncodedPage), CallToolResult> {
    let page = backend
        .result_page(result_id, 0, rows, None)
        .await
        .map_err(|error| {
            refusal::refusal_after(
                acknowledgement,
                tool,
                &error,
                &[format!(
                    "The result {} was captured; read it with read action=page.",
                    json_string(result_id)
                )],
            )
        })?;
    let offset = page.pagination.offset as u64;
    let encoded = encode_page(&page.page, &page.positions, offset).map_err(|error| {
        refusal::error_after(acknowledgement, &unencodable_text(tool, result_id, &error))
    })?;
    Ok((page, offset, encoded))
}

/// The answer of a correlation read `tool` that captured `response`:
/// the first page of its result, at most [`DEFAULT_ROWS`] rows held to
/// `[capture] page_max_rows`, with the read's block, the page continuation
/// and follow-ups, and the search URL under the origin `capabilities`
/// report. `capabilities` were read before the read was sent.
pub(crate) async fn correlation_answer<B: Backend>(
    backend: &B,
    tool: &str,
    capabilities: &Result<CapabilitiesResponse, ClientError>,
    response: CorrelationResponse,
    budget: Budget,
) -> CallToolResult {
    let captured = response.result;
    let Some(facts) = captured.correlation.as_ref() else {
        return refusal::error_result(format!(
            "{tool} captured result {} but trawld sent no correlation facts with it, \
             which a correlation route always fills; this is a trawld defect. \
             Read the rows with read action=page.",
            json_string(&captured.id)
        ));
    };
    let rows = page_rows(u64::from(DEFAULT_ROWS), capabilities.as_ref().ok());
    let page = match backend.result_page(&captured.id, 0, rows, None).await {
        Ok(page) => page,
        Err(error) => {
            return refusal::error_result(format!(
                "{}\nThe result {} was captured; read it with read action=page.",
                refusal::refusal_text(tool, &error, None),
                json_string(&captured.id)
            ));
        }
    };
    let offset = page.pagination.offset as u64;
    let encoded = match encode_page(&page.page, &page.positions, offset) {
        Ok(encoded) => encoded,
        Err(error) => return unencodable(tool, &captured.id, &error),
    };
    let link = match capabilities {
        Ok(capabilities) => search_link(capabilities.link_origin.as_deref(), &captured),
        Err(error) => Link {
            url: None,
            reason: Some(format!(
                "trawld's capabilities could not be read for its link_origin: {}",
                failure_summary(error)
            )),
        },
    };
    let correlation = render::correlation::view(facts);
    let view = result_view(&captured, offset, encoded, Vec::new(), link);
    let mut next_actions = continuations(&view);
    next_actions.extend(follow_ups(facts, &captured));
    let reply = CorrelationReply {
        outcome: render::correlation::outcome(&captured, &correlation),
        correlation,
        result: view,
        next_actions,
    };
    render::correlation(reply, budget, rows)
}

/// A sightings read's follow-up queries as `search` calls over the
/// result's own bounds, spliced into each query's search stage as AST;
/// none for another read, and none for a result without bounds or a
/// follow-up that does not parse.
pub(crate) fn follow_ups(correlation: &Correlation, result: &CapturedResult) -> Vec<NextAction> {
    let Correlation::Sightings(facts) = correlation else {
        return Vec::new();
    };
    let (Some(earliest), Some(latest)) = (result.earliest.as_deref(), result.latest.as_deref())
    else {
        return Vec::new();
    };
    let micros = |text: &str| {
        chrono::DateTime::parse_from_rfc3339(text).ok().map(|at| {
            at.with_timezone(&Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
        })
    };
    let (Some(earliest), Some(latest)) = (micros(earliest), micros(latest)) else {
        return Vec::new();
    };
    facts
        .follow_ups
        .iter()
        .filter_map(|text| {
            let mut query = trawl_core::parser::parse(text).ok()?;
            let search = &mut query.search;
            if search.time_filter.is_some() || !search.hoisted.is_empty() {
                return None;
            }
            search.earliest = Some(Spanned::new(earliest.clone(), 0..0));
            search.latest = Some(Spanned::new(latest.clone(), 0..0));
            let reason = if query.pipeline.is_empty() {
                "the literal on its busiest service, over the same bounds"
            } else {
                "the literal's volume by service over time, over the same bounds"
            };
            Some(NextAction {
                tool: SEARCH.to_owned(),
                arguments: orient::object(json!({
                    "mode": "execute",
                    "query": trawl_core::format::format_query(&query),
                })),
                reason: reason.to_owned(),
            })
        })
        .collect()
}

/// A column's listed values as JSON values with their counts, at most
/// [`MAX_FACET_VALUES`], most frequent first; `None` when a value does not
/// encode under the column's type or is too long to print whole.
pub(crate) fn listed_values(
    column: &ResultColumn,
    values: &[ValueCount],
) -> Option<Vec<FacetValue>> {
    values
        .iter()
        .take(MAX_FACET_VALUES)
        .map(|value| {
            let wire = wire_cell_text(&column.name, &column.ty, &value.value).ok()?;
            (wire.len() <= ABBREVIATE_OVER_BYTES).then_some(())?;
            Some(FacetValue {
                value: serde_json::from_str(&wire).ok()?,
                count: value.count,
            })
        })
        .collect()
}

/// The error for a page that came back with a cell its column type does
/// not hold, naming the result so the caller can read it otherwise.
pub(crate) fn unencodable(
    tool: &str,
    result_id: &str,
    error: &serde_json::Error,
) -> CallToolResult {
    crate::render::refusal::error_result(unencodable_text(tool, result_id, error))
}

/// The text of [`unencodable`], uncapped.
pub(crate) fn unencodable_text(tool: &str, result_id: &str, error: &serde_json::Error) -> String {
    format!(
        "{tool} read result {} but a cell does not fit its column's type: {}",
        json_string(result_id),
        crate::render::one_line(&error.to_string())
    )
}
