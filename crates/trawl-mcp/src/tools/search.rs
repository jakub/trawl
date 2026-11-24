// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `search`: validate one DSL query, or execute it as a capture
//! (ADR-0058, ADR-0057).

use chrono::{DateTime, Utc};
use rmcp::model::CallToolResult;
use serde_json::json;
use trawl_api::CapabilitiesResponse;
use trawl_api::value::{ColumnType, field_display_rank};
use trawl_client::{CaptureRequest, CapturedResult, ClientError, ColumnFacts, ResultColumn};

use super::attach::{self, Attachment, Caller, Held};
use super::orient::object;
use super::schema::{DEFAULT_ROWS, MAX_ROWS, SEARCH, SearchInput};
use super::{continuations, failure_summary, first_page, listed_values, page_rows, result_view};
use crate::backend::Backend;
use crate::hints;
use crate::render::link::search_link;
use crate::render::rows::json_string;
use crate::render::{self, Budget, MAX_FACETS, refusal};
use crate::reply::{AttachedView, Facet, Link, NextAction, SearchMode, SearchReply};
use crate::session::Seen;

const CALLER: Caller = Caller {
    tool: SEARCH,
    input: "search mode=execute",
    noun: "search",
    answer: "capture answer",
};

/// Run `search` against `backend`, with `now` the instant the nearest
/// bounds hint clips its far end at, and `seen` the session's bookkeeping
/// an attached search reads and advances.
pub async fn run<B: Backend>(
    backend: &B,
    seen: &Seen,
    input: SearchInput,
    now: DateTime<Utc>,
) -> CallToolResult {
    let budget = Budget::new(input.max_bytes);
    let rows = u64::from(input.rows.unwrap_or(DEFAULT_ROWS).clamp(1, MAX_ROWS));
    match input.mode {
        SearchMode::Validate => validate(backend, &input, budget, rows).await,
        SearchMode::Execute => {
            let attach = match attach::attachment(
                CALLER,
                input.investigation.as_ref(),
                input.purpose.as_ref(),
            ) {
                Ok(attach) => attach,
                Err(refusal) => return refusal,
            };
            execute(backend, seen, &input, attach, budget, rows, now).await
        }
    }
}

async fn validate<B: Backend>(
    backend: &B,
    input: &SearchInput,
    budget: Budget,
    rows: u64,
) -> CallToolResult {
    let query = input.query.as_str();
    let response = match backend.validate(query).await {
        Ok(response) => response,
        Err(error) => return refusal::refusal(SEARCH, &error, Some(query)),
    };
    let view = render::validation_view(query, &response);
    // A text with any error shows only its errors.
    let advice = if view.valid {
        hints::query_advice(query).await
    } else {
        Vec::new()
    };
    let next_actions = if view.valid {
        vec![NextAction {
            tool: SEARCH.to_owned(),
            arguments: object(json!({"mode": "execute", "query": query})),
            reason: "run the query and keep its rows as a result".to_owned(),
        }]
    } else {
        Vec::new()
    };
    let ignored = match (&input.investigation, &input.purpose) {
        (Some(_), Some(_)) => Some("investigation and purpose"),
        (Some(_), None) => Some("investigation"),
        (None, Some(_)) => Some("purpose"),
        (None, None) => None,
    };
    let notice = |validate: &str| {
        ignored.map(|ignored| {
            format!(
                "{validate} runs nothing, so it ignored {ignored}; mode=execute records the \
                 search as a step"
            )
        })
    };
    let mut outcome = render::validation_outcome(&view);
    if input.investigation.is_none()
        && let Some(notice) = notice("Validate")
    {
        outcome = format!("{outcome} {notice}.");
    }
    let reply = SearchReply {
        outcome,
        mode: SearchMode::Validate,
        validation: Some(view),
        result: None,
        investigation: input.investigation.as_ref().map(|id| AttachedView {
            id: id.clone(),
            result_id: None,
            activity: None,
            note: notice("validate"),
        }),
        advice_hint_count: advice.len(),
        hints: advice,
        next_actions,
    };
    render::search(reply, budget, rows)
}

async fn execute<B: Backend>(
    backend: &B,
    seen: &Seen,
    input: &SearchInput,
    attach: Option<Attachment>,
    budget: Budget,
    rows: u64,
    now: DateTime<Utc>,
) -> CallToolResult {
    let capabilities = backend.capabilities().await;
    let rows = page_rows(rows, capabilities.as_ref().ok());
    let mut held = attach::hold(seen, attach).await;
    let request = CaptureRequest {
        query: input.query.clone(),
        window: None,
        investigation: held.as_ref().map(Held::attach),
    };
    let answer = match backend.capture(&request).await {
        Ok(answer) => answer,
        Err(error) => {
            return match &held {
                Some(held) => attach::refused(CALLER, held.id(), &error, Some(&input.query)),
                None => refusal::refusal(SEARCH, &error, Some(&input.query)),
            };
        }
    };
    let captured = answer.result;
    let seen_seq = held.as_ref().and_then(Held::seen_seq);
    if let Some(unsound) = attach::unsound(CALLER, &captured, held.as_ref().map(Held::id), seen_seq)
    {
        return unsound;
    }
    // From here on the search is a step of the attached investigation,
    // whatever this reply says, and every error opens by saying so.
    let acknowledgement = held.as_ref().map_or_else(String::new, |held| {
        attach::acknowledgement(CALLER, held.id(), &captured.id)
    });
    let (page, offset, encoded) =
        match first_page(backend, SEARCH, &captured.id, rows, &acknowledgement).await {
            Ok(first) => first,
            Err(failed) => return failed,
        };

    let (mut found, nearest, unobserved) =
        hints::search_hints(backend, &captured, &input.query, now).await;
    let advice_hint_count = found.len();
    found.extend(nearest);
    found.extend(unobserved);
    found.extend(hints::dominance(&captured));
    found.extend(hints::lag(&page.page));

    let link = link(&capabilities, &captured);
    let view = result_view(&captured, offset, encoded, facets(&captured), link).await;
    let next_actions = continuations(&view);
    let reply = SearchReply {
        outcome: outcome(&captured),
        mode: SearchMode::Execute,
        validation: None,
        result: Some(view),
        investigation: held.as_ref().map(|held| {
            attach::view(
                CALLER,
                held.id(),
                &captured,
                seen_seq,
                answer.step_seq,
                answer.activity.as_ref(),
            )
        }),
        hints: found,
        next_actions,
        advice_hint_count,
    };
    let result = render::search(reply, budget, rows);
    if let Some(held) = held.as_mut() {
        held.record(&result, &["investigation", "activity"], answer.step_seq);
    }
    attach::acknowledged(&acknowledgement, result)
}

fn link(
    capabilities: &Result<CapabilitiesResponse, ClientError>,
    captured: &CapturedResult,
) -> Link {
    match capabilities {
        Ok(capabilities) => search_link(capabilities.link_origin.as_deref(), captured),
        Err(error) => Link {
            url: None,
            reason: Some(format!(
                "trawld's capabilities could not be read for its link_origin: {}",
                failure_summary(error)
            )),
        },
    }
}

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
