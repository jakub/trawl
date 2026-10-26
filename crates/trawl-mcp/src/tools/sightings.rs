// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `sightings`: where one literal occurs (ADR-0058).
//!
//! Reads trawld's capabilities once, first, then sends
//! `POST /api/v1/results/sightings` with the literal, the bounds and the
//! optional `fields`, and answers with the first page of the captured
//! matches and its block. The block names both denominators: field counts
//! are over the captured matches, service counts over every match. `fields`
//! restricts which fields the literal is attributed to and never which
//! events match, so the counts stay those of the same population. The
//! restriction is frozen in the captured facts, so `read` states it on
//! every page. trawld lists at most 256 fields holding the literal and
//! counts the rest; the block says how many and that `fields` attributes
//! them. The read's follow-up queries become `search` calls over the
//! result's own bounds.
//!
//! With `investigation`, the read attaches to that investigation as
//! `search` attaches (ADR-0059, [`super::attach`]): trawld records it as a
//! step, keeps the matches as evidence, and answers with what other keys
//! changed since the session's `seen_seq`. Its follow-up searches attach
//! to the same investigation, without the read's purpose.

use rmcp::model::CallToolResult;
use trawl_api::results::{CaptureWindow, SightingsRequest};

use super::attach::{self, Caller};
use super::correlation_read;
use super::schema::{SIGHTINGS, SightingsInput};
use crate::backend::Backend;
use crate::render::Budget;
use crate::session::Seen;

/// How `sightings` names itself when it attaches.
const CALLER: Caller = Caller {
    tool: SIGHTINGS,
    input: SIGHTINGS,
    noun: "sightings read",
    answer: "sightings answer",
};

/// Run `sightings` against `backend`, with `seen` the session's
/// bookkeeping an attached read reads and advances.
pub async fn run<B: Backend>(backend: &B, seen: &Seen, input: SightingsInput) -> CallToolResult {
    let attachment =
        match attach::attachment(CALLER, input.investigation.as_ref(), input.purpose.as_ref()) {
            Ok(attachment) => attachment,
            Err(refusal) => return refusal,
        };
    let budget = Budget::new(input.max_bytes);
    let fields = input.fields.unwrap_or_default();
    let window = (input.earliest.is_some() || input.latest.is_some() || input.last.is_some())
        .then_some(CaptureWindow {
            earliest: input.earliest,
            latest: input.latest,
            last: input.last,
        });
    let request = SightingsRequest {
        literal: input.literal,
        window,
        fields,
        investigation: None,
    };
    correlation_read(backend, seen, CALLER, attachment, budget, |attach| {
        let request = SightingsRequest {
            investigation: attach,
            ..request
        };
        async move { backend.sightings(&request).await }
    })
    .await
}
