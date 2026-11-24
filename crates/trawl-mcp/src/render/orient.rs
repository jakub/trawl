// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `orient` reply as one text block and its mirror (ADR-0058).

use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{Value as Json, json};

use super::activity::summary_line;
use super::budget::{Budget, measure};
use super::refusal::error_result;
use super::rows::{as_u64, json_string, json_text, mirror};
use super::{MAX_NEXT_ACTIONS, one_line};
use crate::reply::{NextAction, OrientReply};

#[must_use]
pub fn orient(mut reply: OrientReply, budget: Budget) -> CallToolResult {
    reply.next_actions.truncate(MAX_NEXT_ACTIONS);
    match fit(reply, budget) {
        Some((text, mirror)) => {
            let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
            result.structured_content = Some(mirror);
            result
        }
        None => error_result(format!(
            "orient answered, but its reply does not fit {} bytes even without the \
             inventory, the DSL function and resource lists or the next calls.",
            budget.bytes()
        )),
    }
}

#[must_use]
pub fn orient_text(reply: &OrientReply) -> String {
    let mut lines = vec![one_line(&reply.outcome)];
    installation_lines(reply, &mut lines);
    inventory_lines(reply, &mut lines);
    retention_lines(reply, &mut lines);
    investigation_lines(reply, &mut lines);
    catalog_lines(reply, &mut lines);
    next_action_lines(&reply.next_actions, &mut lines);
    lines.push(reply.essentials.trim_end().to_owned());
    lines.join("\n")
}

fn installation_lines(reply: &OrientReply, lines: &mut Vec<String>) {
    if let Some(version) = &reply.server_version {
        lines.push(format!("version: trawld {}", one_line(version)));
    }
    lines.push(format!(
        "now: {} (UTC); {} (display zone: {})",
        reply.now_utc,
        reply.now_display,
        one_line(&reply.display_zone)
    ));
    let permissions = if reply.key.permissions.is_empty() {
        "none".to_owned()
    } else {
        word_list(&reply.key.permissions)
    };
    lines.push(format!(
        "key: {}, prefix {}; permissions: {permissions}",
        json_string(&reply.key.name),
        json_string(&reply.key.prefix)
    ));
    lines.push(format!(
        "health: {} (HTTP {})",
        one_line(&reply.health.status),
        reply.health.http_status
    ));
    if !reply.health.problems.is_empty() {
        let problems: Vec<String> = reply.health.problems.iter().map(|p| one_line(p)).collect();
        lines.push(format!("health problems: {}", problems.join("; ")));
    }
}

fn inventory_lines(reply: &OrientReply, lines: &mut Vec<String>) {
    if !reply.inventory.is_empty() {
        lines.push(
            "inventory, last 24 hours, busiest first: [env, service, host, events, last_seen]"
                .to_owned(),
        );
        for row in &reply.inventory {
            lines.push(json_text(&json!([
                row.env,
                row.service,
                row.host,
                row.events_24h,
                row.last_seen
            ])));
        }
    }
    if let Some(note) = &reply.inventory_note {
        lines.push(format!("inventory: {}", one_line(note)));
    }
}

fn retention_lines(reply: &OrientReply, lines: &mut Vec<String>) {
    for reach in &reply.retention {
        let policy = if reach.max_age_days == 0 {
            "policy keeps data forever".to_owned()
        } else {
            format!("policy {} days", reach.max_age_days)
        };
        let oldest = reach
            .oldest
            .as_deref()
            .map_or_else(String::new, |oldest| format!(" since {}", one_line(oldest)));
        lines.push(format!(
            "retention reach {}: {} days kept{oldest}; {policy}; {}",
            json_string(&reach.env),
            reach.days_kept,
            one_line(&reach.projection)
        ));
    }
    if let Some(note) = &reply.retention_note {
        lines.push(format!("retention reach: {}", one_line(note)));
    }
}

fn investigation_lines(reply: &OrientReply, lines: &mut Vec<String>) {
    if !reply.open_investigations.is_empty() {
        lines.push("open investigations, most recently changed first:".to_owned());
        lines.extend(reply.open_investigations.iter().map(summary_line));
    }
    if let Some(note) = &reply.open_investigations_note {
        lines.push(format!("open investigations: {}", one_line(note)));
    }
}

fn catalog_lines(reply: &OrientReply, lines: &mut Vec<String>) {
    if let Some(note) = &reply.degraded_note {
        lines.push(format!("degraded fields: {}", one_line(note)));
    } else if reply.degraded_fields.is_empty() {
        lines.push("degraded fields: none".to_owned());
    } else {
        let fields: Vec<String> = reply
            .degraded_fields
            .iter()
            .map(|f| json_string(f))
            .collect();
        lines.push(format!("degraded fields: {}", fields.join(", ")));
    }
    if !reply.limits.is_empty() {
        let limits: Vec<String> = reply
            .limits
            .iter()
            .map(|limit| format!("{}={}", one_line(&limit.name), one_line(&limit.value)))
            .collect();
        lines.push(format!("limits: {}", limits.join(", ")));
    }
    if !reply.dsl.stages.is_empty() {
        lines.push(format!("DSL stages: {}", word_list(&reply.dsl.stages)));
    }
    if !reply.dsl.functions.is_empty() {
        lines.push(format!(
            "DSL functions: {}",
            word_list(&reply.dsl.functions)
        ));
    }
    if !reply.resources.is_empty() {
        lines.push(format!("DSL resources: {}", word_list(&reply.resources)));
    }
}

pub(crate) fn next_action_lines(actions: &[NextAction], lines: &mut Vec<String>) {
    for action in actions.iter().take(MAX_NEXT_ACTIONS) {
        lines.push(format!(
            "next: {} {} — {}",
            one_line(&action.tool),
            json_text(&action.arguments),
            one_line(&action.reason)
        ));
    }
}

fn word_list(words: &[String]) -> String {
    words
        .iter()
        .map(|word| one_line(word))
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Debug, Clone, Copy)]
enum Drop {
    Nothing,
    Functions,
    Resources,
    NextActions,
}

fn fit(mut reply: OrientReply, budget: Budget) -> Option<(String, Json)> {
    let rows = reply.inventory.len();
    let note = reply.inventory_note.clone();
    for drop in [
        Drop::Nothing,
        Drop::Functions,
        Drop::Resources,
        Drop::NextActions,
    ] {
        match drop {
            Drop::Nothing => {}
            Drop::Functions => reply.dsl.functions.clear(),
            Drop::Resources => reply.resources.clear(),
            Drop::NextActions => reply.next_actions.clear(),
        }
        if let Some(fitted) = most_rows(&reply, rows, note.as_deref(), budget) {
            return Some(fitted);
        }
    }
    None
}

fn most_rows(
    reply: &OrientReply,
    rows: usize,
    note: Option<&str>,
    budget: Budget,
) -> Option<(String, Json)> {
    let attempt = |kept: usize| -> Option<(String, Json)> {
        let mut candidate = reply.clone();
        candidate.inventory.truncate(kept);
        if kept < rows {
            let dropped = format!(
                "{} of the {rows} rows above did not fit the {}-byte budget and are not printed",
                as_u64(rows - kept),
                budget.bytes()
            );
            candidate.inventory_note = Some(match note {
                Some(note) => format!("{note}; {dropped}"),
                None => dropped,
            });
        }
        let mirror = mirror(&candidate).ok()?;
        let text = orient_text(&candidate);
        (measure(&text, &mirror) <= budget.bytes()).then_some((text, mirror))
    };
    if let Some(all) = attempt(rows) {
        return Some(all);
    }
    let mut best = attempt(0)?;
    let (mut fits, mut does_not) = (0, rows);
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
