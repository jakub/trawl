// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `sightings`: where one literal occurs (ADR-0058).

use rmcp::model::CallToolResult;
use trawl_api::results::{CaptureWindow, SightingsRequest};

use super::attach::{self, Caller};
use super::correlation_read;
use super::schema::{SIGHTINGS, SightingsInput};
use crate::backend::Backend;
use crate::render::Budget;
use crate::session::Seen;

const CALLER: Caller = Caller {
    tool: SIGHTINGS,
    input: SIGHTINGS,
    noun: "sightings read",
    answer: "sightings answer",
};

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
