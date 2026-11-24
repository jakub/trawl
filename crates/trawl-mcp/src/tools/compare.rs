// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `compare`: one filter's counts in two windows (ADR-0058).

use rmcp::model::CallToolResult;
use trawl_api::results::{COMPARE_DEFAULT_MIN_COUNT, CaptureWindow, CompareRequest};

use super::attach::{self, Caller};
use super::correlation_read;
use super::schema::{COMPARE, CompareInput};
use crate::backend::Backend;
use crate::render::Budget;
use crate::session::Seen;

const CALLER: Caller = Caller {
    tool: COMPARE,
    input: COMPARE,
    noun: "compare",
    answer: "compare answer",
};

pub async fn run<B: Backend>(backend: &B, seen: &Seen, input: CompareInput) -> CallToolResult {
    let attachment =
        match attach::attachment(CALLER, input.investigation.as_ref(), input.purpose.as_ref()) {
            Ok(attachment) => attachment,
            Err(refusal) => return refusal,
        };
    let budget = Budget::new(input.max_bytes);
    let baseline = (input.baseline_earliest.is_some() || input.baseline_latest.is_some())
        .then_some(CaptureWindow {
            earliest: input.baseline_earliest,
            latest: input.baseline_latest,
            last: None,
        });
    let request = CompareRequest {
        filter: input.filter,
        foreground: CaptureWindow {
            earliest: Some(input.foreground_earliest),
            latest: Some(input.foreground_latest),
            last: None,
        },
        baseline,
        group_by: input.group_by.unwrap_or_default(),
        min_count: input.min_count.unwrap_or(COMPARE_DEFAULT_MIN_COUNT),
        investigation: None,
    };
    correlation_read(backend, seen, CALLER, attachment, budget, |attach| {
        let request = CompareRequest {
            investigation: attach,
            ..request
        };
        async move { backend.compare(&request).await }
    })
    .await
}
