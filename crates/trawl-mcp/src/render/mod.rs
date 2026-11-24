// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The text contract: replies as one text block and its mirror (ADR-0058).
//!
//! The mirror carries every string with the escape text a row prints for
//! line breaks and control characters, because a client may show the model
//! the mirror in place of the text block, or render its decoded strings.

pub mod activity;
pub mod budget;
pub mod correlation;
pub mod fields;
pub mod link;
pub mod orient;
pub mod refusal;
pub mod rows;

use std::fmt::Write as _;

use chrono::{DateTime, SecondsFormat, Utc};
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::Value as Json;
use trawl_client::ValidationResponse;
use trawl_client::types::FormatErrorKind;
use trawl_core::sanitize::is_unsafe_display_char;
use trawl_core::schema::MAX_FIELD_NAME_BYTES;

use crate::reply::{
    CellView, CorrelationView, DslEffects, FormatRefusal, FormatRefusalKind, NextAction, PageRead,
    ReadAction, ReadReply, ResultView, SearchReply, ValidationView,
};
use crate::tools::schema::{MAX_ROWS, PAGE_MAX_COLUMNS, READ, SEARCH};

pub use budget::Budget;
use budget::{Fit, keep_rows};
pub use correlation::correlation;
pub use fields::fields;
pub use orient::orient;
use rows::{columns_line, floor_char_boundary, json_string, json_text, row_line};

pub const MAX_FACETS: usize = 3;
pub const MAX_FACET_VALUES: usize = 5;
pub const MAX_NEXT_ACTIONS: usize = 2;

/// `page_rows` is the call's `rows` argument, which a page continuation the
/// fitting creates asks for.
#[must_use]
pub fn search(mut reply: SearchReply, budget: Budget, page_rows: u64) -> CallToolResult {
    if let Some(view) = reply.result.as_mut() {
        record_read(view);
        normalize(view);
    }
    reply.next_actions.truncate(MAX_NEXT_ACTIONS);
    let recovery = reply.result.as_ref().map(Recovery::of);
    match budget::fit(reply, budget, page_rows) {
        Some((text, mirror)) => success(text, mirror),
        None => does_not_fit(SEARCH, recovery.as_ref(), budget),
    }
}

#[must_use]
pub fn read(mut reply: ReadReply, budget: Budget, page_rows: u64) -> CallToolResult {
    if reply.entries.is_some() {
        return activity::fit_entries(reply, budget, READ);
    }
    if let Some(view) = reply.result.as_mut() {
        record_read(view);
        normalize(view);
    }
    reply.next_actions.truncate(MAX_NEXT_ACTIONS);
    if let Some(cell) = &reply.cell {
        return match fit_cell(&reply, cell, budget) {
            Some((text, mirror)) => success(text, mirror),
            None => does_not_fit(READ, Some(&Recovery::of_cell(cell)), budget),
        };
    }
    let recovery = reply.result.as_ref().map(Recovery::of);
    match budget::fit(reply, budget, page_rows) {
        Some((text, mirror)) => success(text, mirror),
        None => does_not_fit(READ, recovery.as_ref(), budget),
    }
}

#[must_use]
pub fn search_text(reply: &SearchReply, budget: Budget) -> String {
    let mut lines = vec![one_line(&reply.outcome)];
    if let Some(validation) = &reply.validation {
        validation_lines(validation, budget, &mut lines);
    }
    if let Some(view) = &reply.result {
        head_lines(view, budget, &mut lines);
    }
    if let Some(attached) = &reply.investigation {
        activity::attached_lines(attached, budget, &mut lines);
    }
    hint_lines(&reply.hints, &mut lines);
    next_action_lines(&reply.next_actions, &mut lines);
    if let Some(view) = &reply.result {
        tail_lines(view, budget, !reply.next_actions.is_empty(), &mut lines);
    }
    lines.join("\n")
}

#[must_use]
pub fn read_text(reply: &ReadReply, budget: Budget) -> String {
    let mut lines = vec![one_line(&reply.outcome)];
    if let Some(view) = &reply.result {
        head_lines(view, budget, &mut lines);
    }
    if let Some(correlation) = &reply.correlation {
        correlation::block_lines(correlation, budget, &mut lines);
    }
    if let Some(cell) = &reply.cell {
        cell_head_lines(cell, &mut lines);
    }
    if let Some(investigation) = &reply.investigation {
        activity::investigation_lines(investigation, &mut lines);
    }
    if let Some(entries) = &reply.entries {
        activity::board_lines(entries, budget, &mut lines);
    }
    next_action_lines(&reply.next_actions, &mut lines);
    if let Some(view) = &reply.result {
        tail_lines(view, budget, !reply.next_actions.is_empty(), &mut lines);
    }
    if let Some(cell) = &reply.cell {
        lines.push(format!("text: {}", json_string(&cell.text)));
    }
    lines.join("\n")
}

#[must_use]
pub fn page_outcome(view: &ResultView) -> String {
    let id = json_string(&view.result_id);
    let captured = view.rows_captured;
    let held = format!(
        "{captured} captured {}",
        plural_u64(captured, "row", "rows")
    );
    let offset = view.offset;
    let printed = rows::as_u64(view.rows.len());
    if printed > 0 {
        format!(
            "Rows {offset} to {} of the {held} of result {id}.",
            offset.saturating_add(printed - 1)
        )
    } else if view.presentation.rows_dropped_to_fit == 0 {
        format!("Result {id} holds {held}; none starts at row {offset}.")
    } else {
        format!("Result {id} holds {held}; the row at {offset} does not fit this response.")
    }
}

#[must_use]
pub fn validation_view(sent: &str, response: &ValidationResponse) -> ValidationView {
    ValidationView {
        valid: response.valid,
        formatted: response.formatted.clone(),
        format_error: response.format_error.as_ref().map(|refused| FormatRefusal {
            kind: match refused.kind {
                FormatErrorKind::TooLong => FormatRefusalKind::TooLong,
                FormatErrorKind::Unfaithful => FormatRefusalKind::Unfaithful,
            },
            message: refused.message.clone(),
        }),
        errors: refusal::diagnostics(sent, &response.errors),
        diagnostics_dropped_to_fit: 0,
        hints_dropped_to_fit: false,
    }
}

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
    fn hints_dropped(&mut self) {
        if let Some(view) = self.result.as_mut() {
            view.presentation.hints_dropped_to_fit = true;
        }
        if let Some(validation) = self.validation.as_mut() {
            validation.hints_dropped_to_fit = true;
        }
    }
    fn drop_advice(&mut self) -> bool {
        let advice = std::mem::take(&mut self.advice_hint_count).min(self.hints.len());
        if advice == 0 {
            return false;
        }
        self.hints.drain(..advice);
        self.hints_dropped();
        true
    }
    /// A reply that can carry advice, every reply but an invalid verdict,
    /// is charged as if it said its hints were dropped, whether or not it
    /// has advice or hints, so dropping advice changes nothing else the
    /// fitting keeps, at the exact boundary too.
    fn charged(&self) -> Option<Self> {
        let flagged = match (&self.validation, &self.result) {
            (Some(validation), _) if !validation.valid => return None,
            (Some(validation), _) => validation.hints_dropped_to_fit,
            (None, Some(view)) => view.presentation.hints_dropped_to_fit,
            (None, None) => return None,
        };
        if flagged {
            return None;
        }
        let mut charged = self.clone();
        charged.hints_dropped();
        Some(charged)
    }
    fn validation_mut(&mut self) -> Option<&mut ValidationView> {
        self.validation.as_mut()
    }
    fn activity_mut(&mut self) -> Option<&mut crate::reply::ActivityView> {
        self.investigation.as_mut()?.activity.as_mut()
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
    fn correlation_mut(&mut self) -> Option<&mut CorrelationView> {
        self.correlation.as_mut()
    }
    fn rows_kept(&mut self) {
        if self.action == ReadAction::Page
            && let Some(view) = &self.result
        {
            self.outcome = page_outcome(view);
        }
    }
}

fn success(text: String, mirror: Json) -> CallToolResult {
    let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
    result.structured_content = Some(mirror);
    result
}

/// Taken from the reply before the fitting consumes it.
#[derive(Debug, Clone)]
pub(crate) struct Recovery {
    result_id: String,
    /// The captured row the page read first, and the page's first captured
    /// column position; `None` when the page read no row, or the call read
    /// a cell.
    cell: Option<(u64, u64)>,
}

impl Recovery {
    pub(crate) fn of(view: &ResultView) -> Self {
        Self {
            result_id: view.result_id.clone(),
            cell: (!view.rows.is_empty() && view.read.columns > 0)
                .then_some((view.offset, view.read.first_position)),
        }
    }

    /// The recovery of a cell read, which names no cell call: the call
    /// that does not fit was one.
    fn of_cell(cell: &CellView) -> Self {
        Self {
            result_id: cell.result_id.clone(),
            cell: None,
        }
    }
}

/// The cell call comes right after the fixed first sentence, where the
/// error's cap at [`refusal::MAX_ERROR_BYTES`] cannot cut it.
fn does_not_fit(tool: &str, recovery: Option<&Recovery>, budget: Budget) -> CallToolResult {
    let bytes = budget.bytes();
    let unfit = "even without rows, facets, hints, next calls or the search URL.";
    let Some(recovery) = recovery else {
        return refusal::error_result(format!(
            "{tool} answered, but its reply does not fit max_bytes={bytes} {unfit} Send a \
             shorter query, or a larger max_bytes."
        ));
    };
    let id = json_string(&recovery.result_id);
    if budget != Budget::HARD {
        return refusal::error_result(format!(
            "{tool} answered, but its reply does not fit max_bytes={bytes} {unfit} The result \
             {id} is kept: read it with a larger max_bytes, at most {}, or print only the \
             columns you name with read action=page result_id={id} columns=[…], at most \
             {PAGE_MAX_COLUMNS} names of the columns fields source=result result_id={id} \
             describes.",
            Budget::HARD.bytes()
        ));
    }
    let mut text = format!(
        "{tool} answered, but its reply does not fit max_bytes={bytes}, the most one response \
         holds, {unfit}"
    );
    if let Some((row, column)) = recovery.cell {
        let _ = write!(
            text,
            " read action=cell result_id={id} row={row} column={column} prints row {row}'s cell \
             at captured column {column}, the page's first; a cell reply prints neither the \
             executed query nor the columns line, and the same call with another column reads \
             the row's other cells."
        );
    }
    let _ = write!(
        text,
        " The result {id} is kept: fields source=result result_id={id} describes its columns, \
         and read action=page result_id={id} columns=[…], at most {PAGE_MAX_COLUMNS} names, \
         prints only the columns it names."
    );
    refusal::error_result(text)
}

/// Record in `view` what the call read, before the renderer caps or fits
/// anything: which columns were null in every row read, by index (a
/// column's name need not be unique).
fn record_read(view: &mut ResultView) {
    let null_columns = if view.rows.is_empty() {
        Vec::new()
    } else {
        (0..view.columns.len())
            .map(|index| {
                view.rows
                    .iter()
                    .all(|row| row.get(index).is_some_and(Json::is_null))
            })
            .collect()
    };
    view.read = PageRead {
        projection: view.read.projection.take(),
        null_columns,
        columns: rows::as_u64(view.columns.len()),
        projected_out: view.presentation.columns_omitted,
        first_position: view.columns.first().map_or(0, |column| column.position),
    };
}

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

fn utc_micros(text: &str) -> String {
    DateTime::parse_from_rfc3339(text).map_or_else(
        |_| one_line(text),
        |instant| {
            instant
                .with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Micros, true)
        },
    )
}

/// The longest a cell reply prints a column name it abbreviates, as a JSON
/// string with its marker: the longest name a stored field may have.
const ABBREVIATED_NAME_BYTES: usize = MAX_FIELD_NAME_BYTES;

/// A cut that prints none of a non-empty range's bytes would propose the
/// very call that made it, so it is never sent.
fn fit_cell(reply: &ReadReply, cell: &CellView, budget: Budget) -> Option<(String, Json)> {
    let whole = json_string(&cell.name).len();
    let mut names = vec![cell.name.clone()];
    if whole > ABBREVIATED_NAME_BYTES {
        names.push(abbreviated_name(&cell.name, ABBREVIATED_NAME_BYTES));
    }
    let marker = abbreviated_name(&cell.name, 0);
    if json_string(&marker).len() < whole {
        names.push(marker);
    }
    names
        .iter()
        .find_map(|name| most_cell_bytes(reply, cell, name, budget))
}

fn most_cell_bytes(
    reply: &ReadReply,
    cell: &CellView,
    name: &str,
    budget: Budget,
) -> Option<(String, Json)> {
    let keeping = |kept: usize| {
        let mut candidate = reply.clone();
        if let Some(cut) = candidate.cell.as_mut() {
            name.clone_into(&mut cut.name);
            keep_cell_bytes(cut, kept);
        }
        budget::follow_cell_cut(&mut candidate, cell.next_offset);
        candidate
    };
    let attempt = |kept: usize| budget::fitted(&keeping(kept), budget);
    let whole = cell.text.len();
    if let Some(all) = attempt(whole) {
        return Some(all);
    }
    let first = cell.text.chars().next()?.len_utf8();
    let mut best = attempt(first)?;
    let (mut fits, mut does_not) = (first, whole);
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

/// A character's JSON escape does not depend on its neighbours, so the
/// bound is met one character at a time.
fn abbreviated_name(name: &str, bound: usize) -> String {
    let marker = format!(
        "…[name cut from {} bytes to fit the response's byte budget]",
        name.len()
    );
    let mut room = bound.saturating_sub(json_string(&marker).len());
    let mut end = 0;
    for (at, c) in name.char_indices() {
        let escaped = json_string(c.encode_utf8(&mut [0; 4])).len() - 2;
        if escaped > room {
            break;
        }
        room -= escaped;
        end = at + c.len_utf8();
    }
    format!("{}{marker}", &name[..end])
}

fn keep_cell_bytes(cell: &mut CellView, kept: usize) {
    let cut = floor_char_boundary(&cell.text, kept);
    if cut == cell.text.len() {
        return;
    }
    cell.text.truncate(cut);
    cell.length = rows::as_u64(cut);
    cell.next_offset = Some(cell.offset.saturating_add(cell.length));
}

fn validation_lines(validation: &ValidationView, budget: Budget, lines: &mut Vec<String>) {
    if let Some(formatted) = &validation.formatted {
        lines.push(format!("formatted query: {}", json_string(formatted)));
    }
    if let Some(refused) = &validation.format_error {
        lines.push(format!("not formatted: {}", json_string(&refused.message)));
    }
    refusal::diagnostic_lines(&validation.errors, lines);
    let dropped = validation.diagnostics_dropped_to_fit;
    if dropped > 0 {
        lines.push(format!(
            "To fit the {}-byte budget the response dropped {dropped} {} from the end.",
            budget.bytes(),
            plural_u64(dropped, "error", "errors")
        ));
    }
    if validation.hints_dropped_to_fit {
        lines.push(format!(
            "To fit the {}-byte budget the response dropped the hints.",
            budget.bytes()
        ));
    }
}

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

fn tail_lines(view: &ResultView, budget: Budget, continued: bool, lines: &mut Vec<String>) {
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
    } else if view.presentation.rows_dropped_to_fit > 0 {
        let mut line = format!("no row fits max_bytes {}", budget.bytes());
        if continued {
            line.push_str("; see next actions");
        }
        lines.push(line);
    }
}

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

fn hint_lines(hints: &[String], lines: &mut Vec<String>) {
    lines.extend(hints.iter().map(|hint| format!("hint: {}", one_line(hint))));
}

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
        ", abbreviates {} {} and omits {} {}",
        p.cells_abbreviated,
        plural_u64(p.cells_abbreviated, "cell", "cells"),
        p.columns_omitted,
        plural_u64(p.columns_omitted, "column", "columns"),
    );
    let fitted_out = view
        .read
        .columns
        .saturating_sub(rows::as_u64(view.columns.len()));
    if fitted_out > 0 {
        if p.null_columns_omitted > 0 {
            let _ = write!(fact, " ({} null in every row read)", p.null_columns_omitted);
        }
        fact.push_str("; fields source=result describes them");
    }
    let _ = write!(
        fact,
        "; {} captured {} {}.",
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

pub(crate) fn one_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if is_unsafe_display_char(c) => {
                let _ = write!(out, "\\u{{{:04x}}}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out
}

fn and_list(items: &[&str]) -> String {
    joined(items, "and")
}

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
        assert_eq!(utc_micros("not\na time"), "not\\na time");
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
