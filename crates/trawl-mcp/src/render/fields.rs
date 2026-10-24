// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `fields` reply as one text block and its mirror (ADR-0058).
//!
//! The text block, top to bottom:
//!
//! 1. the outcome line;
//! 2. a header naming the array layout, then one JSON array per field,
//!    `[name, type, provenance, degraded, rows, nulls, distinct, last_seen,
//!    values]`, with `null` for a fact the source does not report and
//!    `values` a list of `[value, count]` pairs; a result's columns print
//!    in position order;
//! 3. the line saying how many fields did not fit, when any;
//! 4. at most two proposed next calls.
//!
//! Every field name and value prints as JSON. The fitting drops fields from
//! the end first, then the next calls; a reply that does not fit with all
//! of those gone is an error.

use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{Value as Json, json};

use super::budget::{Budget, measure};
use super::orient::next_action_lines;
use super::refusal::error_result;
use super::rows::{as_u64, json_text};
use super::{MAX_NEXT_ACTIONS, one_line};
use crate::reply::{FieldView, FieldsReply, FieldsSource};

/// The header line before the field arrays.
pub const FIELDS_HEADER: &str =
    "fields: [name, type, provenance, degraded, rows, nulls, distinct, last_seen, values]";

/// The `fields` reply as a tool result fitted under `budget`.
#[must_use]
pub fn fields(mut reply: FieldsReply, budget: Budget) -> CallToolResult {
    reply.next_actions.truncate(MAX_NEXT_ACTIONS);
    match fit(reply, budget) {
        Some((text, mirror)) => {
            let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
            result.structured_content = Some(mirror);
            result
        }
        None => error_result(format!(
            "fields answered, but its reply does not fit {} bytes even without its fields or \
             the next calls.",
            budget.bytes()
        )),
    }
}

/// The text block of a `fields` reply, unfitted.
#[must_use]
pub fn fields_text(reply: &FieldsReply) -> String {
    let mut lines = vec![one_line(&reply.outcome)];
    if !reply.fields.is_empty() {
        lines.push(match reply.source {
            FieldsSource::Service => FIELDS_HEADER.to_owned(),
            FieldsSource::Result => format!("{FIELDS_HEADER}, in column order from position 0"),
        });
        lines.extend(reply.fields.iter().map(field_line));
    }
    if reply.fields_dropped_to_fit > 0 {
        let printed = as_u64(reply.fields.len());
        lines.push(format!(
            "{} more fields after the {printed} above did not fit the response budget and are \
             not printed",
            reply.fields_dropped_to_fit
        ));
    }
    next_action_lines(&reply.next_actions, &mut lines);
    lines.join("\n")
}

/// One field as a JSON array.
fn field_line(field: &FieldView) -> String {
    let values: Vec<Json> = field
        .values
        .iter()
        .map(|value| json!([value.value, value.count]))
        .collect();
    json_text(&json!([
        field.name,
        field.ty,
        field.provenance,
        field.degraded,
        field.rows,
        field.nulls,
        field.distinct,
        field.last_seen,
        values
    ]))
}

/// `reply` fitted under `budget`, or `None` when it does not fit with
/// every field and next call gone.
fn fit(mut reply: FieldsReply, budget: Budget) -> Option<(String, Json)> {
    let all = reply.fields.len();
    if let Some(fitted) = most_fields(&reply, all, budget) {
        return Some(fitted);
    }
    reply.next_actions.clear();
    most_fields(&reply, all, budget)
}

/// The reply with the most of its first `all` fields that fit, rendered;
/// `None` when not even zero fields fit.
fn most_fields(reply: &FieldsReply, all: usize, budget: Budget) -> Option<(String, Json)> {
    let attempt = |kept: usize| -> Option<(String, Json)> {
        let mut candidate = reply.clone();
        candidate.fields.truncate(kept);
        candidate.fields_dropped_to_fit = as_u64(all - kept);
        let mirror = serde_json::to_value(&candidate).ok()?;
        let text = fields_text(&candidate);
        (measure(&text, &mirror) <= budget.bytes()).then_some((text, mirror))
    };
    if let Some(everything) = attempt(all) {
        return Some(everything);
    }
    let mut best = attempt(0)?;
    let (mut fits, mut does_not) = (0, all);
    while does_not - fits > 1 {
        let mid = fits + (does_not - fits) / 2;
        if let Some(fitted) = attempt(mid) {
            best = fitted;
            fits = mid;
        } else {
            does_not = mid;
        }
    }
    Some(best)
}
