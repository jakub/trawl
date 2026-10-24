// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `search`: validate one DSL query, or execute it as a capture
//! (ADR-0058, ADR-0057).
//!
//! `mode: validate` asks `/validate`. A query that does not validate is a
//! successful call whose outcome says so, with a caret excerpt per error;
//! only a refused request is an error.
//!
//! `mode: execute` captures the query (`POST /api/v1/results`), then reads
//! the first page of the capture, because the capture answer carries the
//! result's facts and no rows. The reply carries the four truncation facts
//! (rows produced, rows captured, what the DSL itself cut, what this
//! response left out), facets from the capture's column facts, at most two
//! continuations, the search URL under trawld's `link_origin`, and the
//! computed hints of [`crate::hints`]. Nothing here runs the query twice:
//! a later page is `read`.

use chrono::{DateTime, Utc};
use rmcp::model::CallToolResult;
use serde_json::json;
use trawl_api::value::{ColumnType, field_display_rank};
use trawl_client::{CaptureRequest, CapturedResult, ColumnFacts, ResultColumn};

use super::orient::object;
use super::schema::{DEFAULT_ROWS, MAX_ROWS, SEARCH, SearchInput};
use super::{continuations, failure_summary, listed_values, result_view, unencodable};
use crate::backend::Backend;
use crate::hints;
use crate::render::link::search_link;
use crate::render::rows::{encode_page, json_string};
use crate::render::{self, Budget, MAX_FACETS, refusal};
use crate::reply::{Facet, Link, NextAction, SearchMode, SearchReply};

/// Run `search` against `backend`, with `now` the instant the nearest
/// bounds hint clips its far end at.
pub async fn run<B: Backend>(
    backend: &B,
    input: SearchInput,
    now: DateTime<Utc>,
) -> CallToolResult {
    let budget = Budget::new(input.max_bytes);
    match input.mode {
        SearchMode::Validate => validate(backend, &input.query, budget).await,
        SearchMode::Execute => execute(backend, &input, budget, now).await,
    }
}

/// `mode: validate`.
async fn validate<B: Backend>(backend: &B, query: &str, budget: Budget) -> CallToolResult {
    let response = match backend.validate(query).await {
        Ok(response) => response,
        Err(error) => return refusal::refusal(SEARCH, &error, Some(query)),
    };
    let view = render::validation_view(query, &response);
    let next_actions = if view.valid {
        vec![NextAction {
            tool: SEARCH.to_owned(),
            arguments: object(json!({"mode": "execute", "query": query})),
            reason: "run the query and keep its rows as a result".to_owned(),
        }]
    } else {
        Vec::new()
    };
    let reply = SearchReply {
        outcome: render::validation_outcome(&view),
        mode: SearchMode::Validate,
        validation: Some(view),
        result: None,
        hints: Vec::new(),
        next_actions,
    };
    render::search(reply, budget)
}

/// `mode: execute`.
async fn execute<B: Backend>(
    backend: &B,
    input: &SearchInput,
    budget: Budget,
    now: DateTime<Utc>,
) -> CallToolResult {
    let request = CaptureRequest {
        query: input.query.clone(),
        window: None,
    };
    let captured = match backend.capture(&request).await {
        Ok(captured) => captured.result,
        Err(error) => return refusal::refusal(SEARCH, &error, Some(&input.query)),
    };
    let rows = u64::from(input.rows.unwrap_or(DEFAULT_ROWS).clamp(1, MAX_ROWS));
    let page = match backend.result_page(&captured.id, 0, rows, None).await {
        Ok(page) => page,
        Err(error) => {
            return refusal::error_result(format!(
                "{}\nThe result {} was captured; read it with read action=page.",
                refusal::refusal_text(SEARCH, &error, None),
                json_string(&captured.id)
            ));
        }
    };
    let offset = page.pagination.offset as u64;
    let encoded = match encode_page(&page.page, &page.positions, offset) {
        Ok(encoded) => encoded,
        Err(error) => return unencodable(SEARCH, &captured.id, &error),
    };

    let (link, mut found) = tokio::join!(
        link(backend, &captured),
        backend_hints(backend, &captured, now)
    );
    found.extend(hints::dominance(&captured));
    found.extend(hints::lag(&page.page));

    let view = result_view(&captured, offset, encoded, facets(&captured), link);
    let next_actions = continuations(&view);
    let reply = SearchReply {
        outcome: outcome(&captured),
        mode: SearchMode::Execute,
        validation: None,
        result: Some(view),
        hints: found,
        next_actions,
    };
    render::search(reply, budget)
}

/// The search URL, under the origin the capabilities route reports.
async fn link<B: Backend>(backend: &B, captured: &CapturedResult) -> Link {
    match backend.capabilities().await {
        Ok(capabilities) => search_link(capabilities.link_origin.as_deref(), captured),
        Err(error) => Link {
            url: None,
            reason: Some(format!(
                "trawld's capabilities could not be read for its link_origin: {}",
                failure_summary(&error)
            )),
        },
    }
}

/// The hints that read trawld: the nearest bounds for a capture with no
/// rows, then the fields the catalog never observed.
async fn backend_hints<B: Backend>(
    backend: &B,
    captured: &CapturedResult,
    now: DateTime<Utc>,
) -> Vec<String> {
    let nearest = async {
        if captured.total_rows == 0 {
            Some(hints::nearest_bounds(backend, captured, now).await)
        } else {
            None
        }
    };
    let (nearest, unobserved) =
        tokio::join!(nearest, hints::never_observed(backend, &captured.query));
    nearest.into_iter().chain(unobserved).collect()
}

/// The outcome line.
fn outcome(captured: &CapturedResult) -> String {
    let id = json_string(&captured.id);
    match (captured.total_rows, captured.captured_rows) {
        (0, _) => format!("The query produced no rows; it is captured as result {id}."),
        (total, kept) if kept < total => format!(
            "The query produced {total} rows; result {id} keeps the {kept} the capture held."
        ),
        (1, _) => format!("The query produced 1 row, captured as result {id}."),
        (total, _) => format!("The query produced {total} rows, all captured as result {id}."),
    }
}

/// Facets over the captured rows: at most [`MAX_FACETS`] columns whose
/// facts list every distinct value, in field display order, each with at
/// most [`MAX_FACET_VALUES`] values, most frequent first.
///
/// Time columns and trailing metadata (`_raw`, `_ingested` and the rest)
/// are not faceted, nor a column with a value too long to print whole.
fn facets(captured: &CapturedResult) -> Vec<Facet> {
    let mut candidates: Vec<(usize, &ResultColumn, &ColumnFacts)> = captured
        .columns
        .iter()
        .zip(&captured.column_facts)
        .enumerate()
        .filter(|(_, (column, facts))| facetable(column, facts))
        .map(|(position, (column, facts))| (position, column, facts))
        .collect();
    candidates.sort_by_key(|(position, column, _)| (field_display_rank(&column.name), *position));
    candidates
        .into_iter()
        .filter_map(|(_, column, facts)| {
            let values = listed_values(column, facts.values.as_deref()?)?;
            Some(Facet {
                field: column.name.clone(),
                values,
                captured_rows: captured.captured_rows,
            })
        })
        .take(MAX_FACETS)
        .collect()
}

/// Whether a column's facts can be a facet.
fn facetable(column: &ResultColumn, facts: &ColumnFacts) -> bool {
    !facts.distinct_capped
        && facts
            .values
            .as_ref()
            .is_some_and(|values| !values.is_empty())
        && !matches!(
            column.ty,
            ColumnType::Timestamp | ColumnType::Date | ColumnType::Time
        )
        && field_display_rank(&column.name).0 != 2
}
