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
use trawl_api::investigations::PURPOSE_MAX_BYTES;
use trawl_api::value::{ColumnType, field_display_rank};
use trawl_client::{
    Attach, CaptureRequest, CapturedResult, ClientError, ColumnFacts, InvestigationId,
    ResultColumn, ResultPage, is_result_id,
};

use super::orient::object;
use super::schema::{DEFAULT_ROWS, MAX_ROWS, SEARCH, SearchInput};
use super::{
    continuations, failure_summary, listed_values, page_rows, result_view, unencodable_text,
};
use crate::backend::Backend;
use crate::hints;
use crate::render::activity::{activity_view, printed_through};
use crate::render::link::search_link;
use crate::render::rows::{EncodedPage, encode_page, json_string};
use crate::render::{self, Budget, MAX_FACETS, refusal};
use crate::reply::{
    AttachedView, Facet, InvestigationAction, Link, NextAction, SearchMode, SearchReply,
};
use crate::session::Seen;

/// The permissions an attached search needs, in words.
const ATTACHED_NEEDS: &str = "query, investigation_read and investigation_write";

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
            let attach = match attachment(&input) {
                Ok(attach) => attach,
                Err(refusal) => return refusal,
            };
            execute(backend, seen, &input, attach, budget, rows, now).await
        }
    }
}

/// The investigation an execute attaches to and why, checked before any
/// request; `None` for a search attached to nothing.
fn attachment(
    input: &SearchInput,
) -> Result<Option<(InvestigationId, Option<String>)>, CallToolResult> {
    let Some(id) = &input.investigation else {
        if input.purpose.is_some() {
            return Err(refusal::error_result(
                "search mode=execute takes purpose only with investigation: the purpose is \
                 recorded on the investigation's step."
                    .to_owned(),
            ));
        }
        return Ok(None);
    };
    let Ok(id) = InvestigationId::parse(id) else {
        return Err(refusal::error_result(format!(
            "search mode=execute: investigation is 32 lowercase hex digits, and {} is not; \
             investigation action=list lists them.",
            json_string(id)
        )));
    };
    if let Some(purpose) = &input.purpose
        && purpose.len() > PURPOSE_MAX_BYTES
    {
        return Err(refusal::error_result(format!(
            "search mode=execute: purpose is {} bytes; it may hold at most {PURPOSE_MAX_BYTES}.",
            purpose.len()
        )));
    }
    Ok(Some((id, input.purpose.clone())))
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
    attach: Option<(InvestigationId, Option<String>)>,
    budget: Budget,
    rows: u64,
    now: DateTime<Utc>,
) -> CallToolResult {
    let capabilities = backend.capabilities().await;
    let rows = page_rows(rows, capabilities.as_ref().ok());
    // An attached search holds the session's lock across the capture and
    // the render, so the session's next call sends the seq this one printed
    // through. An unattached search reads and writes nothing of `seen`, so
    // it takes no lock and never waits on, or holds up, another call.
    let mut guard = if attach.is_some() {
        Some(seen.lock().await)
    } else {
        None
    };
    let seen_seq = attach
        .as_ref()
        .zip(guard.as_ref())
        .and_then(|((id, _), guard)| guard.get(id));
    let request = CaptureRequest {
        query: input.query.clone(),
        window: None,
        investigation: attach.as_ref().map(|(id, purpose)| Attach {
            id: id.clone(),
            seen_seq,
            purpose: purpose.clone(),
        }),
    };
    let answer = match backend.capture(&request).await {
        Ok(answer) => answer,
        Err(error) => {
            return match &attach {
                Some((id, _)) => refusal::refusal_with(
                    SEARCH,
                    &error,
                    Some(&input.query),
                    &super::investigation::advice(
                        "search with investigation",
                        ATTACHED_NEEDS,
                        InvestigationAction::Note,
                        Some(id),
                        &error,
                    ),
                ),
                None => refusal::refusal(SEARCH, &error, Some(&input.query)),
            };
        }
    };
    let captured = answer.result;
    if let Some(unsound) = unsound_capture(&captured, attach.as_ref().map(|(id, _)| id), seen_seq) {
        return unsound;
    }
    // From here on the search is a step of the attached investigation,
    // whatever this reply says, and every error opens by saying so.
    let acknowledgement = attach
        .as_ref()
        .map_or_else(String::new, |(id, _)| acknowledgement(id, &captured.id));
    let (page, offset, encoded) =
        match first_page(backend, &captured.id, rows, &acknowledgement).await {
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
        investigation: attach.as_ref().map(|(id, _)| {
            attached(
                id,
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
    match (&attach, guard.as_mut(), seen_seq, answer.step_seq) {
        (Some((id, _)), Some(guard), Some(since), _) => guard.printed(
            id,
            since,
            printed_through(&result, &["investigation", "activity"]),
        ),
        // A session that had seen nothing of the investigation follows it
        // from the step this capture appended, once the reply saying so
        // fitted: the entries after it are what the next attached call
        // asks for.
        (Some((id, _)), Some(guard), None, Some(step_seq)) if result.is_error != Some(true) => {
            guard.advance(id, step_seq);
        }
        _ => {}
    }
    if result.is_error == Some(true) && !acknowledgement.is_empty() {
        // The renderer's error names the budget and the result in a few
        // hundred bytes, which its cap never cuts, so this is the one cap
        // that can.
        let text = result
            .content
            .first()
            .and_then(|block| block.as_text())
            .map_or("", |text| text.text.as_str());
        return refusal::error_after(&acknowledgement, text);
    }
    result
}

/// The error for a capture answer nothing here can stand behind, before
/// anything prints its result id; `None` for a sound one. An unsound
/// answer names a result id that is not 32 lowercase hex digits, which
/// trawld never mints and no read can page, or, for a capture attached to
/// investigation `attached`, a result trawld does not say it keeps for
/// that investigation.
///
/// The error names no result. An attached search's step may stand
/// whatever the answer said, so its error says so and names the board
/// read from `seen_seq`, the session's sequence, whose step entry for this
/// search names the result kept.
fn unsound_capture(
    captured: &CapturedResult,
    attached: Option<&InvestigationId>,
    seen_seq: Option<u64>,
) -> Option<CallToolResult> {
    const MALFORMED: &str = "names a result id that is not 32 lowercase hex digits";
    let malformed = !is_result_id(&captured.id);
    let Some(id) = attached else {
        return malformed.then(|| {
            refusal::error_result(format!(
                "search failed: trawld's capture answer {MALFORMED}, so no read can page the \
                 result. Run the search again."
            ))
        });
    };
    let reason = if malformed {
        MALFORMED
    } else {
        match &captured.investigation_id {
            Some(kept_for) if kept_for == id => return None,
            Some(_) => "names another investigation as the one the result is kept for",
            None => "names no investigation the result is kept for",
        }
    };
    let since = seen_seq.unwrap_or(0);
    Some(refusal::error_result(format!(
        "search with investigation cannot tell what trawld kept: its capture answer {reason}. \
         Investigation {} may hold this search as a step; investigation action=read id={} \
         since={since} prints the entries changed after #{since}, and a step for this search \
         names the result it keeps.",
        json_string(id.as_str()),
        id.as_str(),
    )))
}

/// The first page of `rows` rows of captured result `result_id`, its
/// offset and its encoding; or the error, opening with `acknowledgement`
/// when the capture was attached and capped once with it, so the
/// request id and the `Retry-After` wait of a refused page survive.
async fn first_page<B: Backend>(
    backend: &B,
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
                SEARCH,
                &error,
                &[format!(
                    "The result {} was captured; read it with read action=page.",
                    json_string(result_id)
                )],
            )
        })?;
    let offset = page.pagination.offset as u64;
    let encoded = encode_page(&page.page, &page.positions, offset).map_err(|error| {
        refusal::error_after(
            acknowledgement,
            &unencodable_text(SEARCH, result_id, &error),
        )
    })?;
    Ok((page, offset, encoded))
}

/// The line an error opens with after trawld recorded the search as a
/// step of investigation `id` and kept result `result_id` for it, as an
/// investigation write's error opens with its acknowledgement.
fn acknowledgement(id: &InvestigationId, result_id: &str) -> String {
    format!(
        "Investigation {} recorded this search as a step and keeps result {} for it.",
        json_string(id.as_str()),
        json_string(result_id)
    )
}

/// The attached view of a capture kept for investigation `id`: the
/// activity since `seen_seq`, or why there is none. A first call, with no
/// `seen_seq`, says where the session's following starts: at the step the
/// capture appended (`step_seq`), or, when trawld named none, at the
/// session's next write.
fn attached(
    id: &InvestigationId,
    captured: &CapturedResult,
    seen_seq: Option<u64>,
    step_seq: Option<u64>,
    activity: Option<&trawl_client::Activity>,
) -> AttachedView {
    let (activity, note) = match (seen_seq, activity) {
        (Some(since), Some(activity)) => (Some(activity_view(id.as_str(), since, activity)), None),
        (Some(_), None) => (
            None,
            Some("trawld sent no activity with this search".to_owned()),
        ),
        (None, _) if step_seq.is_some() => (
            None,
            Some(format!(
                "this session follows this investigation from this step on; what other keys \
                 wrote before it is on the board, which investigation action=read id={} \
                 since=0 prints",
                id.as_str()
            )),
        ),
        (None, _) => (
            None,
            Some(format!(
                "this session starts following this investigation at its next write here; what \
                 other keys wrote is on the board, which investigation action=read id={} \
                 since=0 prints",
                id.as_str()
            )),
        ),
    };
    AttachedView {
        id: id.as_str().to_owned(),
        result_id: Some(captured.id.clone()),
        activity,
        note,
    }
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
