// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The block a correlation read prints before its rows (ADR-0058).
//!
//! `context`, `sightings` and `compare` answer with the layout of a search
//! result, and between the four truncation facts and the proposed next
//! calls they print what the read established. `read` prints the same
//! block on every page of such a result, so the denominators and the
//! warnings travel with every page.
//!
//! - context: the scope used, the focus row, the three groups as ranges of
//!   captured rows, and [`NO_ORDER`].
//! - sightings: the literal, the [`denominators`] sentence, the (env,
//!   service) groups with first and last seen, the per-service and per-env
//!   totals summed here from the groups, the fields holding the literal,
//!   and the `_raw`-only count.
//! - compare: both windows with their durations, the grouping, the rate
//!   unit, and every warning in trawld's words.
//!
//! The block is never dropped to fit. The fitting gives up rows, facets,
//! next calls, the search URL and hints first; only when the reply still
//! does not fit does it leave entries off the ends of the sightings lists,
//! and the block says how many. A context or compare block is fixed in
//! size, and a warning is never cut.
//!
//! Every value from stored events (a service, an env, a field name, the
//! literal) prints as JSON; trawld's warning text prints on one line.

use rmcp::model::CallToolResult;
use serde_json::json;
use trawl_api::results::{
    CompareFacts, CompareWindow, ContextFacts, Correlation, RowRange, SightingGroup, SightingsFacts,
};
use trawl_client::CapturedResult;

use crate::reply::{
    CompareView, ContextView, CorrelationKind, CorrelationReply, CorrelationView, FocusView,
    KeyCount, NextAction, ResultView, RowGroup, ScopeView, SightingsView, WarningView, WindowView,
};

use super::budget::{self, Fit};
use super::rows::{json_string, json_text};
use super::{
    Budget, MAX_NEXT_ACTIONS, does_not_fit, head_lines, next_action_lines, normalize, one_line,
    plural_u64, success, tail_lines, utc_micros,
};

/// What every context block says about its rows.
pub const NO_ORDER: &str = "this is a new read; equal timestamps establish no order";

/// The sentence naming what a sightings read's counts are over.
#[must_use]
pub fn denominators(captured: u64, matches_total: u64) -> String {
    format!(
        "field counts are over the {captured} captured matches; service counts are over all \
         {matches_total}"
    )
}

/// A correlation reply as a tool result fitted under `budget`; `page_rows`
/// is the page size a continuation the fitting creates asks for.
#[must_use]
pub fn correlation(mut reply: CorrelationReply, budget: Budget, page_rows: u64) -> CallToolResult {
    normalize(&mut reply.result);
    reply.next_actions.truncate(MAX_NEXT_ACTIONS);
    let result_id = reply.result.result_id.clone();
    let tool = match reply.correlation.kind {
        CorrelationKind::Context => crate::tools::schema::CONTEXT,
        CorrelationKind::Sightings => crate::tools::schema::SIGHTINGS,
        CorrelationKind::Compare => crate::tools::schema::COMPARE,
    };
    match budget::fit(reply, budget, page_rows) {
        Some((text, mirror)) => success(text, mirror),
        None => does_not_fit(tool, Some(&result_id), budget),
    }
}

/// The text block of a correlation reply, unfitted. `budget` is the one
/// the presentation facts name when the fitting dropped something.
#[must_use]
pub fn correlation_text(reply: &CorrelationReply, budget: Budget) -> String {
    let mut lines = vec![one_line(&reply.outcome)];
    head_lines(&reply.result, budget, &mut lines);
    block_lines(&reply.correlation, budget, &mut lines);
    next_action_lines(&reply.next_actions, &mut lines);
    tail_lines(
        &reply.result,
        budget,
        !reply.next_actions.is_empty(),
        &mut lines,
    );
    lines.join("\n")
}

impl Fit for CorrelationReply {
    fn text(&self, budget: Budget) -> String {
        correlation_text(self, budget)
    }
    fn view_mut(&mut self) -> Option<&mut ResultView> {
        Some(&mut self.result)
    }
    fn next_actions_mut(&mut self) -> &mut Vec<NextAction> {
        &mut self.next_actions
    }
    fn hints_mut(&mut self) -> Option<&mut Vec<String>> {
        None
    }
    fn correlation_mut(&mut self) -> Option<&mut CorrelationView> {
        Some(&mut self.correlation)
    }
}

/// The outcome line of a correlation read that captured `result`.
#[must_use]
pub fn outcome(result: &CapturedResult, view: &CorrelationView) -> String {
    let id = json_string(&result.id);
    if let Some(context) = &view.context {
        return format!(
            "The context of row {} of result {} is {} earlier, {} same-time and {} later rows, \
             captured as result {id}.",
            context.focus.row,
            json_string(&context.focus.result_id),
            context.earlier.rows,
            context.same_time.rows,
            context.later.rows
        );
    }
    if let Some(sightings) = &view.sightings {
        let literal = json_string(&sightings.literal);
        let total = sightings.matches_total;
        if total == 0 {
            return format!(
                "The literal {literal} matched no event in the bounds; the read is captured as \
                 result {id}."
            );
        }
        // Built before the fitting, so every group is still listed.
        let groups = super::rows::as_u64(sightings.groups.len());
        let kept = if sightings.captured < total {
            format!("keeps the {} most recent", sightings.captured)
        } else {
            format!("keeps all {}", sightings.captured)
        };
        return format!(
            "The literal {literal} matched {total} {} in {} env and service {}; result {id} \
             {kept}.",
            plural_u64(total, "event", "events"),
            groups,
            plural_u64(groups, "group", "groups"),
        );
    }
    if let Some(compare) = &view.compare {
        let warnings = super::rows::as_u64(compare.warnings.len());
        let warned = match warnings {
            0 => "no warnings".to_owned(),
            n => format!("{n} {}", plural_u64(n, "warning", "warnings")),
        };
        return format!(
            "The filter ranks {} {} over both windows, captured as result {id}, with {warned}.",
            compare.rows_ranked,
            plural_u64(compare.rows_ranked, "group", "groups")
        );
    }
    format!("The correlation read is captured as result {id}.")
}

/// The view of a correlation read's facts. A sightings view states its
/// attribution restriction from the facts alone, so the first read and
/// every later page of one capture say the same.
#[must_use]
pub fn view(correlation: &Correlation) -> CorrelationView {
    match correlation {
        Correlation::Context(facts) => CorrelationView {
            kind: CorrelationKind::Context,
            context: Some(context_view(facts)),
            sightings: None,
            compare: None,
        },
        Correlation::Sightings(facts) => CorrelationView {
            kind: CorrelationKind::Sightings,
            context: None,
            sightings: Some(sightings_view(facts)),
            compare: None,
        },
        Correlation::Compare(facts) => CorrelationView {
            kind: CorrelationKind::Compare,
            context: None,
            sightings: None,
            compare: Some(compare_view(facts)),
        },
    }
}

fn row_group(range: RowRange) -> RowGroup {
    RowGroup {
        start: range.start,
        end: range.end,
        rows: range.end.saturating_sub(range.start),
    }
}

fn context_view(facts: &ContextFacts) -> ContextView {
    ContextView {
        scope_used: ScopeView {
            env: facts.scope_used.env.clone(),
            host: facts.scope_used.host.clone(),
            seconds: facts.scope_used.seconds,
        },
        focus: FocusView {
            result_id: facts.focus.result_id.clone(),
            row: facts.focus.row,
            time: utc_micros(&facts.focus.time),
            env: facts.focus.env.clone(),
            host: facts.focus.host.clone(),
        },
        earlier: row_group(facts.groups.earlier),
        same_time: row_group(facts.groups.same_time),
        later: row_group(facts.groups.later),
        statement: NO_ORDER.to_owned(),
    }
}

fn sightings_view(facts: &SightingsFacts) -> SightingsView {
    let mut sorted: Vec<&SightingGroup> = facts.groups.iter().collect();
    sorted.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.service.cmp(&b.service))
            .then_with(|| a.env.cmp(&b.env))
    });
    let per_service = totals(sorted.iter().map(|g| (g.service.clone(), g.count)));
    let per_env = totals(sorted.iter().map(|g| (g.env.clone(), g.count)));
    let groups = sorted
        .iter()
        .map(|group| {
            vec![
                json!(group.env),
                json!(group.service),
                json!(group.count),
                json!(utc_micros(&group.first_seen)),
                json!(utc_micros(&group.last_seen)),
            ]
        })
        .collect();
    let mut fields: Vec<KeyCount> = facts
        .fields
        .iter()
        .map(|field| KeyCount {
            key: Some(field.field.clone()),
            count: field.rows,
        })
        .collect();
    fields.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.key.cmp(&b.key)));
    SightingsView {
        literal: facts.literal.clone(),
        matches_total: facts.matches_total,
        captured: facts.captured,
        denominators: denominators(facts.captured, facts.matches_total),
        attribution_fields: (!facts.attribution_fields.is_empty())
            .then(|| facts.attribution_fields.clone()),
        groups,
        per_service,
        per_env,
        fields,
        raw_only_rows: facts.raw_only_rows,
        entries_dropped_to_fit: 0,
    }
}

/// Counts summed per key, most first, then by key with null first.
fn totals(counts: impl Iterator<Item = (Option<String>, u64)>) -> Vec<KeyCount> {
    let mut summed: std::collections::BTreeMap<Option<String>, u64> =
        std::collections::BTreeMap::new();
    for (key, count) in counts {
        let total = summed.entry(key).or_insert(0);
        *total = total.saturating_add(count);
    }
    let mut totals: Vec<KeyCount> = summed
        .into_iter()
        .map(|(key, count)| KeyCount { key, count })
        .collect();
    totals.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.key.cmp(&b.key)));
    totals
}

fn window_view(window: &CompareWindow) -> WindowView {
    WindowView {
        start: utc_micros(&window.start),
        end: utc_micros(&window.end),
        duration_ms: window.duration_ms,
    }
}

/// A serde unit enum's wire name.
fn wire_name<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

fn compare_view(facts: &CompareFacts) -> CompareView {
    CompareView {
        foreground: window_view(&facts.foreground),
        baseline: window_view(&facts.baseline),
        group_by: facts.group_by.clone(),
        min_count: facts.min_count,
        rate_unit: wire_name(&facts.rate_unit),
        rows_ranked: facts.rows_ranked,
        warnings: facts
            .warnings
            .iter()
            .map(|warning| WarningView {
                code: wire_name(&warning.code),
                message: warning.message.clone(),
                field: warning.field.clone(),
            })
            .collect(),
    }
}

/// The block's lines, for a reply's text.
pub(crate) fn block_lines(view: &CorrelationView, budget: Budget, lines: &mut Vec<String>) {
    if let Some(context) = &view.context {
        context_lines(context, lines);
    }
    if let Some(sightings) = &view.sightings {
        sightings_lines(sightings, budget, lines);
    }
    if let Some(compare) = &view.compare {
        compare_lines(compare, lines);
    }
}

/// An optional key as JSON: a JSON string, or `null`.
fn key(value: Option<&String>) -> String {
    json_text(&value)
}

fn context_lines(context: &ContextView, lines: &mut Vec<String>) {
    let held = |name: &str, value: Option<&String>| match value {
        Some(value) => format!("{name} {}", json_string(value)),
        None => format!("any {name}"),
    };
    let scope = &context.scope_used;
    lines.push(format!(
        "scope used: {}, {}, {} {} either side of the focus row's _time",
        held("env", scope.env.as_ref()),
        held("host", scope.host.as_ref()),
        scope.seconds,
        plural_u64(scope.seconds, "second", "seconds")
    ));
    let focus = &context.focus;
    lines.push(format!(
        "focus: result {}, row {}, _time {}, env {}, host {}",
        json_string(&focus.result_id),
        focus.row,
        one_line(&focus.time),
        key(focus.env.as_ref()),
        key(focus.host.as_ref())
    ));
    for (name, group) in [
        ("earlier", context.earlier),
        ("same _time", context.same_time),
        ("later", context.later),
    ] {
        lines.push(if group.rows == 0 {
            format!("{name}: no rows")
        } else {
            format!(
                "{name}: captured rows {} to {}, {} {}",
                group.start,
                group.end - 1,
                group.rows,
                plural_u64(group.rows, "row", "rows")
            )
        });
    }
    lines.push(context.statement.clone());
}

/// `[key, count]` pairs on one line.
fn pairs(counts: &[KeyCount]) -> String {
    counts
        .iter()
        .map(|count| json_text(&(&count.key, count.count)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn sightings_lines(sightings: &SightingsView, budget: Budget, lines: &mut Vec<String>) {
    lines.push(format!(
        "sightings of {}: {}",
        json_string(&sightings.literal),
        sightings.denominators
    ));
    if let Some(fields) = &sightings.attribution_fields {
        let names: Vec<String> = fields.iter().map(|name| json_string(name)).collect();
        lines.push(format!(
            "attribution restricted to fields {}; the matches counted are the same",
            names.join(", ")
        ));
    }
    if sightings.groups.is_empty() {
        lines.push("groups: none".to_owned());
    } else {
        lines.push(
            "groups (env, service, matches, first_seen, last_seen), most matches first:".to_owned(),
        );
        lines.extend(sightings.groups.iter().map(json_text));
    }
    if !sightings.per_service.is_empty() {
        lines.push(format!(
            "matches per service: {}",
            pairs(&sightings.per_service)
        ));
    }
    if !sightings.per_env.is_empty() {
        lines.push(format!("matches per env: {}", pairs(&sightings.per_env)));
    }
    lines.push(if sightings.fields.is_empty() {
        format!(
            "fields holding the literal, over the {} captured matches: none",
            sightings.captured
        )
    } else {
        format!(
            "fields holding the literal, over the {} captured matches: {}",
            sightings.captured,
            pairs(&sightings.fields)
        )
    });
    let raw = sightings.raw_only_rows;
    lines.push(format!(
        "_raw only: {raw} captured {} {} the literal in no field but _raw",
        plural_u64(raw, "match", "matches"),
        if raw == 1 { "holds" } else { "hold" }
    ));
    let dropped = sightings.entries_dropped_to_fit;
    if dropped > 0 {
        lines.push(format!(
            "To fit the {}-byte budget the response left {dropped} {} off the ends of the \
             groups, totals and fields lists; the denominators above count every match.",
            budget.bytes(),
            plural_u64(dropped, "entry", "entries")
        ));
    }
}

/// A duration in milliseconds in the largest whole unit that states it
/// exactly.
fn duration(ms: u64) -> String {
    const UNITS: [(u64, &str); 4] = [
        (86_400_000, "d"),
        (3_600_000, "h"),
        (60_000, "min"),
        (1_000, "s"),
    ];
    UNITS
        .iter()
        .find(|(unit, _)| ms > 0 && ms.is_multiple_of(*unit))
        .map_or_else(
            || format!("{ms} ms"),
            |(unit, name)| format!("{} {name}", ms / unit),
        )
}

fn compare_lines(compare: &CompareView, lines: &mut Vec<String>) {
    for (name, window) in [
        ("foreground", &compare.foreground),
        ("baseline", &compare.baseline),
    ] {
        lines.push(format!(
            "{name}: {} (inclusive) to {} (exclusive), {}",
            one_line(&window.start),
            one_line(&window.end),
            duration(window.duration_ms)
        ));
    }
    lines.push(if compare.group_by.is_empty() {
        "grouped by: nothing; one row counts every event of the filter".to_owned()
    } else {
        let names: Vec<String> = compare.group_by.iter().map(|n| json_string(n)).collect();
        format!("grouped by: {}", names.join(", "))
    });
    let ranked = compare.rows_ranked;
    lines.push(format!(
        "{ranked} {} listed with at least {} {} in either window, ranked by absolute rate \
         change descending, then count, then group key",
        plural_u64(ranked, "group", "groups"),
        compare.min_count,
        plural_u64(compare.min_count, "event", "events")
    ));
    lines.push(match compare.rate_unit.as_str() {
        "per_minute" => "rates are events per minute of each window".to_owned(),
        other => format!("rates are in {}", one_line(other)),
    });
    if compare.warnings.is_empty() {
        lines.push("warnings: none raised".to_owned());
    }
    for warning in &compare.warnings {
        let on = warning
            .field
            .as_ref()
            .map(|field| format!(" on {}", json_string(field)))
            .unwrap_or_default();
        lines.push(format!(
            "warning {}{on}: {}",
            one_line(&warning.code),
            one_line(&warning.message)
        ));
    }
}

/// Leave all but the first `kept` entries off each sightings list, and
/// count what was left off; a view of another kind is unchanged.
pub(crate) fn keep_entries(view: &mut CorrelationView, kept: usize) {
    let Some(sightings) = view.sightings.as_mut() else {
        return;
    };
    let mut dropped = 0;
    let mut cut = |len: usize| {
        let gone = len.saturating_sub(kept);
        dropped += gone;
        len - gone
    };
    let groups = cut(sightings.groups.len());
    sightings.groups.truncate(groups);
    let per_service = cut(sightings.per_service.len());
    sightings.per_service.truncate(per_service);
    let per_env = cut(sightings.per_env.len());
    sightings.per_env.truncate(per_env);
    let fields = cut(sightings.fields.len());
    sightings.fields.truncate(fields);
    sightings.entries_dropped_to_fit = super::rows::as_u64(dropped);
}

/// The longest sightings list, the most entries [`keep_entries`] can keep
/// per list; 0 for a view of another kind.
pub(crate) fn longest_list(view: &CorrelationView) -> usize {
    view.sightings.as_ref().map_or(0, |sightings| {
        [
            sightings.groups.len(),
            sightings.per_service.len(),
            sightings.per_env.len(),
            sightings.fields.len(),
        ]
        .into_iter()
        .max()
        .unwrap_or(0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_duration_names_its_largest_exact_unit() {
        assert_eq!(duration(3_600_000), "1 h");
        assert_eq!(duration(5_400_000), "90 min");
        assert_eq!(duration(1_500), "1500 ms");
        assert_eq!(duration(0), "0 ms");
        assert_eq!(duration(172_800_000), "2 d");
    }

    #[test]
    fn totals_sum_per_key_most_first() {
        let summed = totals(
            [
                (Some("api".to_owned()), 2),
                (None, 3),
                (Some("api".to_owned()), 4),
                (Some("db".to_owned()), 3),
            ]
            .into_iter(),
        );
        assert_eq!(
            summed,
            [
                KeyCount {
                    key: Some("api".to_owned()),
                    count: 6
                },
                KeyCount {
                    key: None,
                    count: 3
                },
                KeyCount {
                    key: Some("db".to_owned()),
                    count: 3
                },
            ]
        );
    }
}
