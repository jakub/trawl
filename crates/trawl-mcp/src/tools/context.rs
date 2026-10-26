// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `context`: the rows around one captured row (ADR-0058).
//!
//! Reads trawld's capabilities once, first, then sends
//! `POST /api/v1/results/context` with the focus row, the scope and the
//! group sizes, and answers with the first page of the new result and its
//! block: the scope trawld used, the focus row, the earlier, same-time and
//! later groups as ranges of captured rows, and the sentence that this is
//! a new read whose equal timestamps establish no order. A focus row
//! without `_time` or a key the scope names is trawld's `focus_incomplete`
//! refusal, never a wider read.

use rmcp::model::CallToolResult;
use trawl_api::results::{
    CONTEXT_DEFAULT_SECONDS, CONTEXT_DEFAULT_SIDE_ROWS, ContextRequest, ContextScope as WireScope,
};

use super::correlation_answer;
use super::schema::{CONTEXT, ContextInput};
use crate::backend::Backend;
use crate::render::{Budget, refusal};
use crate::reply::ContextScope;

/// Run `context` against `backend`.
pub async fn run<B: Backend>(backend: &B, input: ContextInput) -> CallToolResult {
    let budget = Budget::new(input.max_bytes);
    let capabilities = backend.capabilities().await;
    let request = ContextRequest {
        result_id: input.result_id,
        row: input.row,
        scope: match input.scope.unwrap_or_default() {
            ContextScope::EnvAndHost => WireScope::EnvAndHost,
            ContextScope::Env => WireScope::Env,
            ContextScope::Host => WireScope::Host,
        },
        seconds: input.seconds.unwrap_or(CONTEXT_DEFAULT_SECONDS),
        before: input.before.unwrap_or(CONTEXT_DEFAULT_SIDE_ROWS),
        after: input.after.unwrap_or(CONTEXT_DEFAULT_SIDE_ROWS),
        investigation: None,
    };
    let response = match backend.context(&request).await {
        Ok(response) => response,
        Err(error) => return refusal::refusal(CONTEXT, &error, None),
    };
    correlation_answer(backend, CONTEXT, &capabilities, response, budget).await
}
