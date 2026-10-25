// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `compare`: one filter's counts in two windows (ADR-0058).
//!
//! Reads trawld's capabilities once, first, then sends
//! `POST /api/v1/results/compare` with the filter, both windows, the group
//! fields and the minimum count, and answers with the first page of the
//! ranked groups and its block: both windows with their durations, the
//! grouping, the rate unit and every warning trawld raised, in its words.
//! A baseline with neither bound is left to trawld, which takes the window
//! of equal duration just before the foreground. Overlapping windows are
//! trawld's `windows_overlap` refusal.

use rmcp::model::CallToolResult;
use trawl_api::results::{COMPARE_DEFAULT_MIN_COUNT, CaptureWindow, CompareRequest};

use super::correlation_answer;
use super::schema::{COMPARE, CompareInput};
use crate::backend::Backend;
use crate::render::{Budget, refusal};

/// Run `compare` against `backend`.
pub async fn run<B: Backend>(backend: &B, input: CompareInput) -> CallToolResult {
    let budget = Budget::new(input.max_bytes);
    let capabilities = backend.capabilities().await;
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
    };
    let response = match backend.compare(&request).await {
        Ok(response) => response,
        Err(error) => return refusal::refusal(COMPARE, &error, None),
    };
    correlation_answer(backend, COMPARE, &capabilities, response, None, budget).await
}
