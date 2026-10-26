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
//!
//! With `investigation`, the read attaches to that investigation as
//! `search` attaches (ADR-0059, [`super::attach`]): trawld records it as a
//! step, keeps the new result as evidence, and answers with what other
//! keys changed since the session's `seen_seq`.

use rmcp::model::CallToolResult;
use trawl_api::results::{
    CONTEXT_DEFAULT_SECONDS, CONTEXT_DEFAULT_SIDE_ROWS, ContextRequest, ContextScope as WireScope,
};

use super::attach::{self, Caller};
use super::correlation_read;
use super::schema::{CONTEXT, ContextInput};
use crate::backend::Backend;
use crate::render::Budget;
use crate::reply::ContextScope;
use crate::session::Seen;

/// How `context` names itself when it attaches.
const CALLER: Caller = Caller {
    tool: CONTEXT,
    input: CONTEXT,
    noun: "context read",
    answer: "context answer",
};

/// Run `context` against `backend`, with `seen` the session's bookkeeping
/// an attached read reads and advances.
pub async fn run<B: Backend>(backend: &B, seen: &Seen, input: ContextInput) -> CallToolResult {
    let attachment =
        match attach::attachment(CALLER, input.investigation.as_ref(), input.purpose.as_ref()) {
            Ok(attachment) => attachment,
            Err(refusal) => return refusal,
        };
    let budget = Budget::new(input.max_bytes);
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
    correlation_read(backend, seen, CALLER, attachment, budget, |attach| {
        let request = ContextRequest {
            investigation: attach,
            ..request
        };
        async move { backend.context(&request).await }
    })
    .await
}
