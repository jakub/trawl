// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `fields` reply as one text block and its mirror (ADR-0058).

use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{Value as Json, json};

use super::budget::{Budget, measure};
use super::orient::next_action_lines;
use super::refusal::error_result;
use super::rows::{as_u64, json_text, mirror};
use super::{MAX_NEXT_ACTIONS, one_line};
use crate::reply::{FieldView, FieldsReply, FieldsSource};

pub const FIELDS_HEADER: &str =
    "fields: [name, type, provenance, degraded, rows, nulls, distinct, last_seen, values]";

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

fn fit(mut reply: FieldsReply, budget: Budget) -> Option<(String, Json)> {
    let all = reply.fields.len();
    if let Some(fitted) = most_fields(&reply, all, budget) {
        return Some(fitted);
    }
    reply.next_actions.clear();
    most_fields(&reply, all, budget)
}

fn most_fields(reply: &FieldsReply, all: usize, budget: Budget) -> Option<(String, Json)> {
    let attempt = |kept: usize| -> Option<(String, Json)> {
        let mut candidate = reply.clone();
        candidate.fields.truncate(kept);
        candidate.fields_dropped_to_fit = as_u64(all - kept);
        let mirror = mirror(&candidate).ok()?;
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
