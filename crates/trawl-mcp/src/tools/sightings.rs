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
//! read's follow-up queries become `search` calls over the result's own
//! bounds.

use rmcp::model::CallToolResult;
use trawl_api::results::{CaptureWindow, SightingsRequest};

use super::correlation_answer;
use super::schema::{SIGHTINGS, SightingsInput};
use crate::backend::Backend;
use crate::render::{Budget, refusal};

/// Run `sightings` against `backend`.
pub async fn run<B: Backend>(backend: &B, input: SightingsInput) -> CallToolResult {
    let budget = Budget::new(input.max_bytes);
    let capabilities = backend.capabilities().await;
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
    };
    let response = match backend.sightings(&request).await {
        Ok(response) => response,
        Err(error) => return refusal::refusal(SIGHTINGS, &error, None),
    };
    correlation_answer(
        backend,
        SIGHTINGS,
        &capabilities,
        response,
        Some(&request.fields),
        budget,
    )
    .await
}
