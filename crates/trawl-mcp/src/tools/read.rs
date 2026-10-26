// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `read`: a page of a captured result, or one cell's text, without
//! running anything again (ADR-0058, ADR-0057).
//!
//! Both actions read trawld's capabilities once, first, for the limits
//! trawld holds a read to. `action: page` reads `GET /api/v1/results/{id}`
//! from `offset`, at most `[capture] page_max_rows` rows; a page of a
//! `context`, `sightings` or `compare` result prints that read's block,
//! as the read itself printed it. `action: cell`
//! reads the result's metadata with a zero-row page, for the column's name
//! and type, then a byte range of the cell from `byte_offset`, as many
//! bytes as the response budget holds and `[capture] max_wire_bytes`
//! allows; the reply says where the rest starts. A result past its
//! deadline answers trawld's 410 `result_gone`, an unknown id 404: both
//! are errors naming the code, never an empty page.
//!
//! `action: investigation` reads an investigation's entries changed after
//! `since` and prints them with the renderer `investigation` with
//! `action: read` uses ([`crate::render::activity`]), proposing `read`
//! again for the rest. Like that read, it counts what it prints as seen
//! by this session.

use rmcp::model::CallToolResult;
use serde_json::json;
use trawl_api::CapabilitiesResponse;

use super::orient::object;
use super::schema::{DEFAULT_ROWS, MAX_ROWS, READ, ReadInput};
use super::{continuations, follow_ups, page_rows, result_view, unencodable};
use crate::backend::Backend;
use crate::render::activity::{Continue, board_through};
use crate::render::budget::CELL_CONTINUATION;
use crate::render::rows::{encode_page, json_string};
use crate::render::{self, Budget, refusal};
use crate::reply::InvestigationAction;
use crate::reply::{CellView, Link, NextAction, ReadAction, ReadReply};
use crate::session::Seen;

/// The link reason on a page `read` prints: the link belongs to the
/// search that captured the result.
pub const READ_LINK: &str = "read does not repeat the search URL; the search that captured this \
result printed it";

/// Run `read` against `backend`, keeping the session's investigation
/// bookkeeping in `seen`.
pub async fn run<B: Backend>(backend: &B, seen: &Seen, input: ReadInput) -> CallToolResult {
    let budget = Budget::new(input.max_bytes);
    let rows = u64::from(input.rows.unwrap_or(DEFAULT_ROWS).clamp(1, MAX_ROWS));
    if input.action == ReadAction::Investigation {
        return investigation(backend, seen, &input, budget).await;
    }
    let Some(result_id) = input.result_id.as_deref() else {
        return refusal::error_result(format!(
            "read with action={} needs result_id, the id search returned.",
            if input.action == ReadAction::Page {
                "page"
            } else {
                "cell"
            }
        ));
    };
    match input.action {
        ReadAction::Page => page(backend, result_id, &input, budget, rows).await,
        _ => cell(backend, result_id, &input, budget, rows).await,
    }
}

/// `action: investigation`.
async fn investigation<B: Backend>(
    backend: &B,
    seen: &Seen,
    input: &ReadInput,
    budget: Budget,
) -> CallToolResult {
    let Some(id) = input
        .investigation_id
        .as_deref()
        .and_then(|id| trawl_api::investigations::InvestigationId::parse(id).ok())
    else {
        return refusal::error_result(
            "read with action=investigation needs investigation_id, 32 lowercase hex digits."
                .to_owned(),
        );
    };
    let since = input.since.unwrap_or(0);
    let (view, entries, outcome, mut guard) = match super::investigation::board(
        backend,
        seen,
        &id,
        since,
        Continue::Read,
        InvestigationAction::Read,
    )
    .await
    {
        Ok(board) => board,
        Err(refused) => return refused,
    };
    let reply = ReadReply {
        outcome,
        action: ReadAction::Investigation,
        result: None,
        correlation: None,
        cell: None,
        investigation: Some(view),
        entries: Some(entries),
        next_actions: Vec::new(),
    };
    let result = render::read(reply, budget, 0);
    guard.printed(&id, since, board_through(&result, &["entries"]));
    result
}

/// `action: page`, reading the call's `rows` rows, or the fewer trawld's
/// page limit allows.
async fn page<B: Backend>(
    backend: &B,
    result_id: &str,
    input: &ReadInput,
    budget: Budget,
    rows: u64,
) -> CallToolResult {
    let capabilities = backend.capabilities().await.ok();
    let rows = page_rows(rows, capabilities.as_ref());
    let offset = input.offset.unwrap_or(0);
    let page = match backend.result_page(result_id, offset, rows, None).await {
        Ok(page) => page,
        Err(error) => return refusal::refusal(READ, &error, None),
    };
    let encoded = match encode_page(&page.page, &page.positions, offset) {
        Ok(encoded) => encoded,
        Err(error) => return unencodable(READ, result_id, &error),
    };
    let link = Link {
        url: None,
        reason: Some(READ_LINK.to_owned()),
    };
    let view = result_view(&page.result, offset, encoded, Vec::new(), link);
    // The renderer says this again over the rows it prints, when its
    // budget prints fewer than were read.
    let outcome = render::page_outcome(&view);
    let mut next_actions = continuations(&view);
    // A correlation result's block, and a sightings read's follow-ups,
    // travel with every page.
    let correlation = page.result.correlation.as_ref().map(|correlation| {
        next_actions.extend(follow_ups(correlation, &page.result));
        render::correlation::view(correlation)
    });
    let reply = ReadReply {
        outcome,
        action: ReadAction::Page,
        result: Some(view),
        correlation,
        cell: None,
        investigation: None,
        entries: None,
        next_actions,
    };
    render::read(reply, budget, rows)
}

/// `action: cell`. `rows` is the call's `rows` argument, which a cell
/// range does not use.
async fn cell<B: Backend>(
    backend: &B,
    result_id: &str,
    input: &ReadInput,
    budget: Budget,
    rows: u64,
) -> CallToolResult {
    let (Some(row), Some(column)) = (input.row, input.column) else {
        return refusal::error_result(
            "read with action=cell needs both row (the 0-based captured row) and column (the \
             0-based column position)."
                .to_owned(),
        );
    };
    let Ok(position) = usize::try_from(column) else {
        return refusal::error_result(format!("read: column {column} is out of range."));
    };
    let capabilities = backend.capabilities().await.ok();
    // A zero-row page carries the metadata: the column's name and type,
    // and trawld's own answer for an unknown or expired result.
    let meta = match backend.result_page(result_id, row, 0, None).await {
        Ok(page) => page.result,
        Err(error) => return refusal::refusal(READ, &error, None),
    };
    let Some(meta_column) = meta.columns.get(position) else {
        return refusal::error_result(format!(
            "read: result {} has {} columns, so there is no column {column}.",
            json_string(&meta.id),
            meta.columns.len()
        ));
    };
    let length = cell_length(budget, capabilities.as_ref());
    let range = match backend
        .result_cell(result_id, row, position, input.byte_offset, Some(length))
        .await
    {
        Ok(range) => range,
        Err(error) => return refusal::refusal(READ, &error, None),
    };
    let next_actions = range
        .next_offset
        .map(|next| NextAction {
            tool: READ.to_owned(),
            arguments: object(json!({
                "action": "cell",
                "result_id": meta.id,
                "row": row,
                "column": column,
                "byte_offset": next,
            })),
            reason: CELL_CONTINUATION.to_owned(),
        })
        .into_iter()
        .collect();
    let reply = ReadReply {
        outcome: format!(
            "Cell at row {row}, column {column} of result {}: {} bytes of wire JSON text in all.",
            json_string(&meta.id),
            range.total_bytes
        ),
        action: ReadAction::Cell,
        result: None,
        correlation: None,
        cell: Some(CellView {
            result_id: meta.id.clone(),
            row,
            column,
            name: meta_column.name.clone(),
            ty: range.ty.to_string(),
            offset: range.offset,
            length: range.length,
            total_bytes: range.total_bytes,
            next_offset: range.next_offset,
            text: range.text,
        }),
        investigation: None,
        entries: None,
        next_actions,
    };
    render::read(reply, budget, rows)
}

/// The bytes a cell read asks trawld for: the budget, held to the
/// `[capture] max_wire_bytes` trawld's `capabilities` report, because
/// trawld refuses any longer range. When the capabilities could not be
/// read, the budget, and trawld's answer stands.
fn cell_length(budget: Budget, capabilities: Option<&CapabilitiesResponse>) -> u64 {
    let length = budget.bytes() as u64;
    capabilities.map_or(length, |capabilities| {
        length.min(capabilities.bounds.max_wire_bytes)
    })
}
