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
//! `mode: execute` reads trawld's capabilities once, first, then captures
//! the query (`POST /api/v1/results`) and reads the first page of the
//! capture, because the capture answer carries the result's facts and no
//! rows. The page asks for no more rows than `[capture] page_max_rows`, so
//! a default call never strands its own capture behind a refused first
//! page, and the same capabilities give the search URL its origin. The
//! reply carries the four truncation facts (rows produced, rows captured,
//! what the DSL itself cut, what this response left out), facets from the
//! capture's column facts, at most two continuations, the search URL under
//! trawld's `link_origin`, and the computed hints of [`crate::hints`].
//! Nothing here runs the query twice: a later page is `read`.
//!
//! `mode: execute` with `investigation` attaches the capture to that
//! investigation (ADR-0059): trawld records the search as a step, keeps
//! the whole result as evidence, and answers with what other keys changed
//! since the session's `seen_seq`, printed per [`crate::session`]'s rules.
//! Once trawld accepted the capture, an error after it (the page, its
//! encoding, the fitting) opens by naming the investigation and the kept
//! result, since the step stands whatever the reply says. A capture
//! answer nothing here can stand behind is an error before anything
//! prints its result id: an id that is not 32 lowercase hex digits, which
//! trawld never mints and no read can page, or, attached, a result trawld
//! does not say it keeps for the investigation the call named. Its error
//! names no result; an attached one says the step may stand and names the
//! board read whose step entry names the result kept.
//! The id's shape and the purpose's length are checked before any
//! request. `mode: validate` runs nothing, so it ignores both and says
//! which it ignored: beside the investigation's id when the call named
//! one, else on the outcome line, so a purpose given alone is said too
//! and the mirror names no investigation for it.

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

/// How `search` names itself when it attaches.
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

/// `mode: validate`.
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
        hints: Vec::new(),
        next_actions,
    };
    render::search(reply, budget, rows)
}

/// `mode: execute`, reading the call's `rows` rows of the capture's first
/// page, or the fewer trawld's page limit allows; attached to `attach`'s
/// investigation, if any, under the session's lock for it.
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

    let mut found = backend_hints(backend, &captured, now).await;
    found.extend(hints::dominance(&captured));
    found.extend(hints::lag(&page.page));

    let link = link(&capabilities, &captured);
    let view = result_view(&captured, offset, encoded, facets(&captured), link);
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
    };
    let result = render::search(reply, budget, rows);
    if let Some(held) = held.as_mut() {
        held.record(&result, &["investigation", "activity"], answer.step_seq);
    }
    attach::acknowledged(&acknowledgement, result)
}

/// The search URL, under the origin trawld's `capabilities` report.
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

/// The hints that read trawld: the nearest bounds for a capture with no
/// rows, then the fields the catalog holds no pin or observation of.
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
