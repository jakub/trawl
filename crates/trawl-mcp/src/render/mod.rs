// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The text contract: replies as one text block and its mirror (ADR-0058).
//!
//! Pure functions, no I/O: a tool fills a reply from [`crate::reply`], and
//! [`search`] or [`read`] turns it into a `CallToolResult` whose one text
//! block stands alone and whose `structuredContent` is the reply itself,
//! both fitted under the call's [`Budget`].
//!
//! The text block of a result, top to bottom, each part omitted when it is
//! empty:
//!
//! 1. the outcome line, then the result id and how long it stays readable;
//! 2. the bounds, RFC 3339 with microseconds in UTC, and the executed text
//!    as a JSON string;
//! 3. four truncation facts, four sentences: the rows the execution
//!    produced; the rows the capture kept and whether it truncated;
//!    whether the DSL itself sampled, limited or deduplicated; and what
//!    this response abbreviated, omitted or dropped to fit its budget;
//! 4. facets over the captured rows, then the hints;
//! 5. at most two proposed next calls, each a tool name and its arguments
//!    as a compact JSON object;
//! 6. the search URL, or the sentence saying why there is none;
//! 7. one columns line;
//! 8. the rows, one JSON array per line, every cell one JSON value.
//!
//! Every value that came from stored events (a cell, a facet value, a
//! column name) prints as JSON, so event text stays data. The free text a
//! tool writes (the outcome, hints, reasons) and trawld's messages print
//! on one line each, with line breaks and control characters escaped.
//!
//! - [`rows`]: cells as JSON values and the abbreviation of long ones.
//! - [`budget`]: the byte budget and the fitting.
//! - [`refusal`]: refused calls as error text, and caret excerpts.

pub mod budget;
pub mod refusal;
pub mod rows;

use std::fmt::Write as _;

use chrono::{DateTime, SecondsFormat, Utc};
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::Value as Json;
use trawl_client::ValidationResponse;

use crate::reply::{
    CellView, DslEffects, NextAction, ReadReply, ResultView, SearchReply, ValidationView,
};
use crate::tools::schema::{MAX_ROWS, READ, SEARCH};

pub use budget::Budget;
use budget::{Fit, keep_rows};
use rows::{columns_line, floor_char_boundary, json_string, json_text, row_line};

/// The most facets a result prints.
pub const MAX_FACETS: usize = 3;
/// The most values one facet prints.
pub const MAX_FACET_VALUES: usize = 5;
/// The most proposed next calls a reply prints.
pub const MAX_NEXT_ACTIONS: usize = 2;

/// The `search` reply as a tool result fitted under `budget`.
///
/// The renderer owns the presentation facts: it caps the rows at the hard
/// row limit, the facets and next calls at theirs, recounts the
/// abbreviated cells and the rows left after the page, and writes the
/// bounds as RFC 3339 with microseconds in UTC, in the text and the mirror
/// alike.
#[must_use]
pub fn search(mut reply: SearchReply, budget: Budget) -> CallToolResult {
    if let Some(view) = reply.result.as_mut() {
        normalize(view);
    }
    reply.next_actions.truncate(MAX_NEXT_ACTIONS);
    let result_id = reply.result.as_ref().map(|view| view.result_id.clone());
    match budget::fit(reply, budget) {
        Some((text, mirror)) => success(text, mirror),
        None => does_not_fit(SEARCH, result_id.as_deref(), budget),
    }
}

/// The `read` reply as a tool result fitted under `budget`.
///
/// A page is fitted as [`search`] fits a result. A cell range is fitted by
/// printing fewer of its bytes, cut on a character boundary, with its
/// `length` and `next_offset` saying where the rest starts.
#[must_use]
pub fn read(mut reply: ReadReply, budget: Budget) -> CallToolResult {
    if let Some(view) = reply.result.as_mut() {
        normalize(view);
    }
    reply.next_actions.truncate(MAX_NEXT_ACTIONS);
    let result_id = reply
        .result
        .as_ref()
        .map(|view| view.result_id.clone())
        .or_else(|| reply.cell.as_ref().map(|cell| cell.result_id.clone()));
    let fitted = if reply.cell.is_some() {
        fit_cell(&reply, budget)
    } else {
        budget::fit(reply, budget)
    };
    match fitted {
        Some((text, mirror)) => success(text, mirror),
        None => does_not_fit(READ, result_id.as_deref(), budget),
    }
}

/// The text block of a `search` reply, unfitted. `budget` is the one the
/// presentation facts name when the fitting dropped something.
#[must_use]
pub fn search_text(reply: &SearchReply, budget: Budget) -> String {
    let mut lines = vec![one_line(&reply.outcome)];
    if let Some(validation) = &reply.validation {
        validation_lines(validation, &mut lines);
    }
    if let Some(view) = &reply.result {
        head_lines(view, budget, &mut lines);
    }
    hint_lines(&reply.hints, &mut lines);
    next_action_lines(&reply.next_actions, &mut lines);
    if let Some(view) = &reply.result {
        tail_lines(view, &mut lines);
    }
    lines.join("\n")
}

/// The text block of a `read` reply, unfitted. `budget` is the one the
/// presentation facts name when the fitting dropped something.
#[must_use]
pub fn read_text(reply: &ReadReply, budget: Budget) -> String {
    let mut lines = vec![one_line(&reply.outcome)];
    if let Some(view) = &reply.result {
        head_lines(view, budget, &mut lines);
    }
    if let Some(cell) = &reply.cell {
        cell_head_lines(cell, &mut lines);
    }
    next_action_lines(&reply.next_actions, &mut lines);
    if let Some(view) = &reply.result {
        tail_lines(view, &mut lines);
    }
    if let Some(cell) = &reply.cell {
        lines.push(format!("text: {}", json_string(&cell.text)));
    }
    lines.join("\n")
}

/// The verdict of `search` in validate mode, with a caret excerpt cut from
/// `sent`, the text that was validated, for each error with a usable span.
#[must_use]
pub fn validation_view(sent: &str, response: &ValidationResponse) -> ValidationView {
    ValidationView {
        valid: response.valid,
        formatted: response.formatted.clone(),
        errors: refusal::diagnostics(sent, &response.errors),
    }
}

/// The outcome line of a validation verdict. A failed validation is a
/// successful call whose outcome says the query did not validate.
#[must_use]
pub fn validation_outcome(view: &ValidationView) -> String {
    if view.valid {
        "The query is valid.".to_owned()
    } else {
        let n = view.errors.len();
        format!(
            "The query did not validate: {n} {}.",
            plural(n, "error", "errors")
        )
    }
}

impl Fit for SearchReply {
    fn text(&self, budget: Budget) -> String {
        search_text(self, budget)
    }
    fn view_mut(&mut self) -> Option<&mut ResultView> {
        self.result.as_mut()
    }
    fn next_actions_mut(&mut self) -> &mut Vec<NextAction> {
        &mut self.next_actions
    }
    fn hints_mut(&mut self) -> Option<&mut Vec<String>> {
        Some(&mut self.hints)
    }
}

impl Fit for ReadReply {
    fn text(&self, budget: Budget) -> String {
        read_text(self, budget)
    }
    fn view_mut(&mut self) -> Option<&mut ResultView> {
        self.result.as_mut()
    }
    fn next_actions_mut(&mut self) -> &mut Vec<NextAction> {
        &mut self.next_actions
    }
    fn hints_mut(&mut self) -> Option<&mut Vec<String>> {
        None
    }
}

/// A successful result: one text block and its mirror.
fn success(text: String, mirror: Json) -> CallToolResult {
    let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
    result.structured_content = Some(mirror);
    result
}

/// The error a reply gets when it does not fit `budget` with every
/// droppable part gone.
fn does_not_fit(tool: &str, result_id: Option<&str>, budget: Budget) -> CallToolResult {
    let bytes = budget.bytes();
    let tail = match result_id {
        Some(id) => format!(
            " The result {} is kept: read it with a larger max_bytes, at most {}.",
            json_string(id),
            Budget::HARD.bytes()
        ),
        None => " Send a shorter query, or a larger max_bytes.".to_owned(),
    };
    refusal::error_result(format!(
        "{tool} answered, but its reply does not fit max_bytes={bytes} even without rows, \
         facets, hints, next calls or the search URL.{tail}"
    ))
}

/// Hold `view` to the limits the text contract states, and recount its
/// presentation facts.
fn normalize(view: &mut ResultView) {
    view.bounds.earliest = view.bounds.earliest.as_deref().map(utc_micros);
    view.bounds.latest = view.bounds.latest.as_deref().map(utc_micros);
    view.expires_at = view.expires_at.as_deref().map(utc_micros);
    view.facets.truncate(MAX_FACETS);
    for facet in &mut view.facets {
        facet.values.truncate(MAX_FACET_VALUES);
    }
    view.rows.truncate(MAX_ROWS as usize);
    let printed = view.rows.len();
    keep_rows(view, printed, printed);
}

/// An RFC 3339 instant as RFC 3339 with microseconds in UTC; text that
/// does not parse is kept as it is.
fn utc_micros(text: &str) -> String {
    DateTime::parse_from_rfc3339(text).map_or_else(
        |_| text.to_owned(),
        |instant| {
            instant
                .with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Micros, true)
        },
    )
}

/// A cell reply printing the most bytes of its range that fit `budget`.
fn fit_cell(reply: &ReadReply, budget: Budget) -> Option<(String, Json)> {
    let attempt = |kept: usize| -> Option<(String, Json)> {
        let mut candidate = reply.clone();
        if let Some(cell) = candidate.cell.as_mut() {
            keep_cell_bytes(cell, kept);
        }
        let mirror = serde_json::to_value(&candidate).ok()?;
        let text = read_text(&candidate, budget);
        (budget::measure(&text, &mirror) <= budget.bytes()).then_some((text, mirror))
    };
    let whole = reply.cell.as_ref().map_or(0, |cell| cell.text.len());
    if let Some(all) = attempt(whole) {
        return Some(all);
    }
    let mut best = attempt(0)?;
    let (mut fits, mut does_not) = (0, whole);
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

/// Keep at most `kept` bytes of the cell range's text, cut on a character
/// boundary, and say where the rest starts.
fn keep_cell_bytes(cell: &mut CellView, kept: usize) {
    let cut = floor_char_boundary(&cell.text, kept);
    if cut == cell.text.len() {
        return;
    }
    cell.text.truncate(cut);
    cell.length = rows::as_u64(cut);
    cell.next_offset = Some(cell.offset.saturating_add(cell.length));
}

/// The validation verdict's lines.
fn validation_lines(validation: &ValidationView, lines: &mut Vec<String>) {
    if let Some(formatted) = &validation.formatted {
        lines.push(format!("formatted query: {}", json_string(formatted)));
    }
    refusal::diagnostic_lines(&validation.errors, lines);
}

/// Parts 1 to 4 of a result: its id, bounds, executed text, the four
/// truncation facts and the facets.
fn head_lines(view: &ResultView, budget: Budget, lines: &mut Vec<String>) {
    lines.push(match &view.expires_at {
        Some(at) => format!(
            "result: {}, readable until {at}",
            json_string(&view.result_id)
        ),
        None => format!("result: {}, kept", json_string(&view.result_id)),
    });
    match (&view.bounds.earliest, &view.bounds.latest) {
        (Some(earliest), Some(latest)) => lines.push(format!(
            "bounds: earliest {earliest} (inclusive), latest {latest} (exclusive)"
        )),
        (Some(earliest), None) => lines.push(format!("bounds: earliest {earliest} (inclusive)")),
        (None, Some(latest)) => lines.push(format!("bounds: latest {latest} (exclusive)")),
        (None, None) => {}
    }
    lines.push(format!(
        "executed query: {}",
        json_string(&view.executed_query)
    ));

    let produced = view.rows_produced;
    lines.push(format!(
        "The execution produced {produced} {}.",
        plural_u64(produced, "row", "rows")
    ));
    let captured = view.rows_captured;
    lines.push(if view.captured_truncated {
        format!("The capture kept {captured} of the {produced} rows and truncated the result.")
    } else {
        format!(
            "The capture kept all {captured} {}.",
            plural_u64(captured, "row", "rows")
        )
    });
    lines.push(dsl_fact(view.dsl_effects));
    lines.push(presentation_fact(view, budget));

    for facet in &view.facets {
        let values: Vec<String> = facet
            .values
            .iter()
            .map(|value| format!("{} {}", json_text(&value.value), value.count))
            .collect();
        lines.push(format!(
            "facet {} over the {} captured {}: {}",
            json_string(&facet.field),
            facet.captured_rows,
            plural_u64(facet.captured_rows, "row", "rows"),
            values.join(", ")
        ));
    }
}

/// Parts 6 to 8 of a result: the link, the columns and the rows.
fn tail_lines(view: &ResultView, lines: &mut Vec<String>) {
    match (&view.link.url, &view.link.reason) {
        (Some(url), _) => lines.push(format!("search URL: {}", one_line(url))),
        (None, Some(reason)) => lines.push(format!("no search URL: {}", one_line(reason))),
        (None, None) => {}
    }
    if !view.columns.is_empty() {
        lines.push(columns_line(&view.columns));
    }
    if !view.rows.is_empty() {
        lines.push(format!("rows from captured row {}:", view.offset));
        lines.extend(view.rows.iter().map(|row| row_line(row)));
    }
}

/// A cell range's head lines: which cell, and which bytes of it.
fn cell_head_lines(cell: &CellView, lines: &mut Vec<String>) {
    lines.push(format!(
        "cell: row {}, column {} {} {}, of result {}",
        cell.row,
        cell.column,
        json_string(&cell.name),
        cell.ty,
        json_string(&cell.result_id)
    ));
    let end = cell.offset.saturating_add(cell.length);
    let mut range = format!(
        "This prints bytes {} to {end} of the cell's {} bytes of wire JSON text",
        cell.offset, cell.total_bytes
    );
    match cell.next_offset {
        Some(next) => {
            let _ = write!(range, "; the rest starts at byte_offset {next}.");
        }
        None => range.push_str(", to its end."),
    }
    lines.push(range);
}

/// The hints, one line each.
fn hint_lines(hints: &[String], lines: &mut Vec<String>) {
    lines.extend(hints.iter().map(|hint| format!("hint: {}", one_line(hint))));
}

/// The proposed next calls, one line each.
fn next_action_lines(actions: &[NextAction], lines: &mut Vec<String>) {
    for action in actions.iter().take(MAX_NEXT_ACTIONS) {
        let arguments = json_text(&action.arguments);
        lines.push(format!(
            "next: {} {arguments} — {}",
            one_line(&action.tool),
            one_line(&action.reason)
        ));
    }
}

/// Truncation fact 3: what the DSL itself did to the rows.
fn dsl_fact(effects: DslEffects) -> String {
    let all = [
        (effects.sampled, "sampled", "sample"),
        (effects.limited, "limited", "limit"),
        (effects.deduplicated, "deduplicated", "deduplicate"),
    ];
    let did: Vec<&str> = all.iter().filter(|e| e.0).map(|e| e.1).collect();
    let did_not: Vec<&str> = all.iter().filter(|e| !e.0).map(|e| e.2).collect();
    match (did.is_empty(), did_not.is_empty()) {
        (true, _) => "The DSL did not sample, limit or deduplicate the rows.".to_owned(),
        (false, true) => format!("The DSL {} the rows.", and_list(&did)),
        (false, false) => format!(
            "The DSL {} the rows; it did not {} them.",
            and_list(&did),
            or_list(&did_not)
        ),
    }
}

/// Truncation fact 4: what this response printed, abbreviated, omitted
/// and dropped to fit its budget.
fn presentation_fact(view: &ResultView, budget: Budget) -> String {
    let p = view.presentation;
    let printed = view.rows.len();
    let mut fact = if printed == 0 {
        format!(
            "This response prints no rows from captured row {}",
            view.offset
        )
    } else {
        format!(
            "This response prints {printed} {} from captured row {}",
            plural(printed, "row", "rows"),
            view.offset
        )
    };
    let _ = write!(
        fact,
        ", abbreviates {} {} and omits {} {}; {} captured {} {}.",
        p.cells_abbreviated,
        plural_u64(p.cells_abbreviated, "cell", "cells"),
        p.columns_omitted,
        plural_u64(p.columns_omitted, "column", "columns"),
        p.rows_omitted,
        plural_u64(p.rows_omitted, "row", "rows"),
        if p.rows_omitted == 1 {
            "follows"
        } else {
            "follow"
        },
    );
    let mut dropped = Vec::new();
    if p.rows_dropped_to_fit > 0 {
        dropped.push(format!(
            "{} {}",
            p.rows_dropped_to_fit,
            plural_u64(p.rows_dropped_to_fit, "row", "rows")
        ));
    }
    for (gone, part) in [
        (p.facets_dropped_to_fit, "the facets"),
        (p.hints_dropped_to_fit, "the hints"),
        (p.next_actions_dropped_to_fit, "the proposed next calls"),
    ] {
        if gone {
            dropped.push(part.to_owned());
        }
    }
    if !dropped.is_empty() {
        let dropped: Vec<&str> = dropped.iter().map(String::as_str).collect();
        let _ = write!(
            fact,
            " To fit the {}-byte budget it dropped {}.",
            budget.bytes(),
            and_list(&dropped)
        );
    }
    fact
}

/// `text` on one line: line breaks, tabs and other control characters,
/// and the Unicode line and paragraph separators, escaped.
pub(crate) fn one_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() || c == '\u{2028}' || c == '\u{2029}' => {
                let _ = write!(out, "\\u{{{:04x}}}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out
}

/// `a`, `a and b`, `a, b and c`.
fn and_list(items: &[&str]) -> String {
    joined(items, "and")
}

/// `a`, `a or b`, `a, b or c`.
fn or_list(items: &[&str]) -> String {
    joined(items, "or")
}

fn joined(items: &[&str], last: &str) -> String {
    match items {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [init @ .., tail] => format!("{} {last} {tail}", init.join(", ")),
    }
}

fn plural<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 { one } else { many }
}

fn plural_u64<'a>(n: u64, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 { one } else { many }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_line_escapes_every_line_break() {
        assert_eq!(
            one_line("a\nb\r\tc\u{1b}d\u{2028}"),
            "a\\nb\\r\\tc\\u{001b}d\\u{2028}"
        );
    }

    #[test]
    fn bounds_print_in_utc_with_microseconds() {
        assert_eq!(
            utc_micros("2026-03-14T05:00:00+02:00"),
            "2026-03-14T03:00:00.000000Z"
        );
        assert_eq!(utc_micros("not a time"), "not a time");
    }

    #[test]
    fn the_dsl_fact_names_what_ran_and_what_did_not() {
        let effects = DslEffects {
            sampled: false,
            limited: true,
            deduplicated: false,
        };
        assert_eq!(
            dsl_fact(effects),
            "The DSL limited the rows; it did not sample or deduplicate them."
        );
    }
}
