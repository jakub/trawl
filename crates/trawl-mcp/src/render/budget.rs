// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The response byte budget (ADR-0058).
//!
//! A response's size is the bytes of its text block plus the bytes of its
//! `structuredContent` mirror serialized compactly, because a client may
//! count both against its spill limit. The budget is the call's
//! `max_bytes`: [`Budget::DEFAULT`] when it names none, never above
//! [`Budget::HARD`].
//!
//! A reply is fitted by dropping whole rows from the end of the page,
//! never cutting a row or a cell in the middle, and the response says how
//! many it dropped; a `read` page's outcome line names the rows it prints
//! after the cut, in the text and the mirror alike. A tool proposes a page's continuation at the end of the
//! rows it read; when the fitting keeps fewer, the continuation moves to
//! the end of the rows printed, and a shortened cell range's continuation
//! to where the printed bytes end. When the tool proposed none, because
//! the server's page or range was the last, the fitting creates the
//! continuation its cut needs, first among the proposed calls and measured
//! with the rest. Every page continuation, the tool's and the fitting's,
//! sends the `columns` the page was projected to; a cell call names a
//! captured position and sends none. A continuation always advances: when not one row of the
//! page fits, it reads the next row alone at [`Budget::HARD`] if that row
//! fits there, else that row's cells one at a time, and a cell range with
//! room for none of its bytes is refused with the smallest `max_bytes` that
//! prints one. When even no rows fit, an attached search's activity
//! entries go from the end, each count printed with the most rows that
//! fit it, then the facets, then the
//! proposed next calls, then the search URL (the reason says why), then
//! the hints, each said in the text, and after each the most rows that fit
//! are printed again. A validation verdict is fitted after those: each
//! diagnostic's message and hint is cut on a character boundary to
//! [`DIAGNOSTIC_TEXT_SHARE`] of the budget with a marker saying so, then
//! diagnostics are dropped from the end, never the first, and the mirror
//! counts them. A correlation block is never dropped: only after all of
//! that, every activity entry of an attached read among it, does the
//! fitting leave entries off the ends of a sightings block's lists, as
//! many as it must, and the block counts them. A reply that does
//! not fit with all of those gone is not sent: the caller gets an error text naming the result id and the
//! budget, so it can read the result with a larger one.
//!
//! A result reply that fits whole is sent as it is. One that does not
//! first omits every column whose cell is null in every row the call read,
//! all of them and not only the rows it prints, in the columns line, the
//! rows and the mirror together, and then fits as above. When that finds
//! no reply printing a row, even with every droppable part gone, it omits
//! the fewest columns from the end, in stored order, that let one row
//! print with every droppable part gone, and fits again from the start,
//! now only to replies that print a row. Printed columns keep their
//! captured positions, so `read` with `action: cell` names any omitted
//! cell. A page that read no row omits no column.

use std::collections::HashSet;

use serde::Serialize;
use serde_json::{Map, Value as Json, json};

use crate::reply::{
    ActivityView, CorrelationView, NextAction, ReadReply, ResultView, ValidationView,
};
use crate::tools::orient::object;
use crate::tools::schema::{DEFAULT_MAX_BYTES, HARD_MAX_BYTES, MIN_MAX_BYTES, READ};

use super::MAX_NEXT_ACTIONS;
use super::rows::{as_u64, floor_char_boundary, mirror};

/// Why a page continuation is proposed, whether a tool or the fitting
/// proposes it.
pub(crate) const PAGE_CONTINUATION: &str =
    "print the next captured rows without running the query again";
/// Why a cell continuation is proposed, whether a tool or the fitting
/// proposes it.
pub(crate) const CELL_CONTINUATION: &str = "print the rest of the cell";
/// Why the fitting proposes a row's cell at captured column `column`, the
/// page's first: a page printed none of its rows, and the next does not
/// fit a page even at [`Budget::HARD`].
pub(crate) fn row_by_cells(column: u64) -> String {
    format!(
        "the row does not fit a page at any max_bytes; read its cells one at a time, from \
         column {column}"
    )
}

/// The bytes one response may use, text block and mirror together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget(usize);

impl Budget {
    /// The budget of a call that names none: 16 KiB.
    pub const DEFAULT: Self = Self(DEFAULT_MAX_BYTES as usize);
    /// The smallest budget a call may ask for: 4 KiB.
    pub const MIN: Self = Self(MIN_MAX_BYTES as usize);
    /// The hard budget: 32 KiB.
    pub const HARD: Self = Self(HARD_MAX_BYTES as usize);

    /// The budget for a call's `max_bytes`, held within
    /// [`Self::MIN`]..=[`Self::HARD`].
    #[must_use]
    pub fn new(max_bytes: Option<u32>) -> Self {
        max_bytes.map_or(Self::DEFAULT, |bytes| {
            Self(
                usize::try_from(bytes)
                    .unwrap_or(usize::MAX)
                    .clamp(Self::MIN.0, Self::HARD.0),
            )
        })
    }

    /// The budget in bytes.
    #[must_use]
    pub fn bytes(self) -> usize {
        self.0
    }
}

impl Default for Budget {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The bytes a response with `text` and `mirror` uses against a budget.
#[must_use]
pub fn measure(text: &str, mirror: &Json) -> usize {
    text.len() + mirror.to_string().len()
}

/// A reply the budget can fit: its text, its mirror, and the parts the
/// fitting may drop.
pub(crate) trait Fit: Serialize + Clone {
    /// The text block for this reply under `budget`.
    fn text(&self, budget: Budget) -> String;
    /// The result view whose rows, facets and link may be dropped.
    fn view_mut(&mut self) -> Option<&mut ResultView>;
    /// The proposed next calls.
    fn next_actions_mut(&mut self) -> &mut Vec<NextAction>;
    /// Say again what the reply says about its rows, once the fitting kept
    /// fewer of them than it read. A reply whose text names no printed
    /// range has nothing to say again.
    fn rows_kept(&mut self) {}
    /// The hints, for a reply that has them.
    fn hints_mut(&mut self) -> Option<&mut Vec<String>>;
    /// The validation verdict whose diagnostics may be cut and dropped,
    /// for a reply that has one.
    fn validation_mut(&mut self) -> Option<&mut ValidationView> {
        None
    }
    /// The correlation block whose sightings lists may be cut, for a reply
    /// that has one.
    fn correlation_mut(&mut self) -> Option<&mut CorrelationView> {
        None
    }
    /// The activity an attached call carries, whose entries may be cut
    /// from the end, for a reply that has one.
    fn activity_mut(&mut self) -> Option<&mut ActivityView> {
        None
    }
}

/// What the fitting gives up, in order, before it gives up the reply.
#[derive(Debug, Clone, Copy)]
enum Drop {
    Nothing,
    Facets,
    NextActions,
    Link,
    Hints,
    DiagnosticText,
}

/// The link's reason when the fitting dropped the search URL.
pub(crate) const LINK_DROPPED: &str = "the search URL did not fit the response's byte budget";

/// A diagnostic's message and its hint are each cut to at most the
/// budget divided by this, marker included, when the verdict does not fit
/// whole.
pub const DIAGNOSTIC_TEXT_SHARE: usize = 16;

/// `reply` fitted under `budget`: its text and its mirror, or `None` when
/// it does not fit with every droppable part gone.
///
/// `page_rows` is the call's `rows` argument, which a page continuation
/// the fitting creates asks for. Once the proposed next calls are dropped
/// it creates none, and a cut that needed one says the calls were dropped.
///
/// A reply that fits whole is returned as it is. Otherwise its null
/// columns go and [`ladder`] fits it; when that prints no row and not even
/// one row fits with every droppable part gone, [`widest_prefix`] keeps the
/// most columns with which one does, and the ladder fits again at that
/// width, to replies that print a row. When no width prints one, the
/// reply is the ladder's at the width before the cut.
pub(crate) fn fit<R: Fit>(mut reply: R, budget: Budget, page_rows: u64) -> Option<(String, Json)> {
    let rows = reply.view_mut().map_or(0, |view| view.rows.len());
    if let Some(whole) = fitted(&whole(&reply, rows, page_rows), budget) {
        return Some(whole);
    }
    omit_null_columns(&mut reply);
    let laddered = ladder(&reply, rows, budget, page_rows, 0);
    if rows == 0
        || laddered
            .as_ref()
            .is_some_and(|(_, mirror)| prints_a_row(mirror))
        || row_fits_stripped(&reply, rows, budget)
    {
        return laddered;
    }
    let Some(width) = widest_prefix(&reply, rows, budget) else {
        return laddered;
    };
    ladder(&with_width(&reply, width), rows, budget, page_rows, 1).or(laddered)
}

/// `reply` with every row and every activity entry it holds, as the
/// fitting's first candidate renders it.
fn whole<R: Fit>(reply: &R, rows: usize, page_rows: u64) -> R {
    let mut candidate = reply.clone();
    if let Some(view) = candidate.activity_mut() {
        let total = view.entries.len();
        super::activity::keep_entries(view, total, total, super::activity::Continue::Investigation);
    }
    keeping(&candidate, rows, rows, Some(page_rows))
}

/// `reply` fitted under `budget` by giving up, in order, rows, an attached
/// call's activity entries, the facets, the proposed next calls, the
/// search URL, the hints and the diagnostics' text, then diagnostics and
/// sightings list entries; `None` when it does not fit with all of them
/// gone. Every candidate prints at least `least` rows, 0 or 1.
fn ladder<R: Fit>(
    reply: &R,
    rows: usize,
    budget: Budget,
    page_rows: u64,
    least: usize,
) -> Option<(String, Json)> {
    let mut reply = reply.clone();
    let mut page_rows = Some(page_rows);
    for drop in [
        Drop::Nothing,
        Drop::Facets,
        Drop::NextActions,
        Drop::Link,
        Drop::Hints,
        Drop::DiagnosticText,
    ] {
        apply(drop, &mut reply, budget);
        if matches!(drop, Drop::NextActions) {
            page_rows = None;
        }
        if let Some(fitted) = most_activity(&reply, rows, budget, page_rows, least) {
            return Some(fitted);
        }
    }
    most_diagnostics(&reply, rows, budget, page_rows, least)
        .or_else(|| most_entries(&reply, rows, budget, page_rows, least))
}

/// Whether the fitted reply whose mirror is `mirror` prints a row of its
/// result: the mirror is the fitted reply, so it holds what the text
/// printed.
fn prints_a_row(mirror: &Json) -> bool {
    mirror
        .get("result")
        .and_then(|view| view.get("rows"))
        .and_then(Json::as_array)
        .is_some_and(|rows| !rows.is_empty())
}

/// `reply` with every part the fitting may give up gone: the facets, the
/// proposed next calls, the search URL, the hints, the diagnostics' text
/// and all but the first diagnostic, every activity entry, and every
/// sightings list entry.
fn stripped<R: Fit>(reply: &R, budget: Budget) -> R {
    let mut stripped = reply.clone();
    for drop in [
        Drop::Facets,
        Drop::NextActions,
        Drop::Link,
        Drop::Hints,
        Drop::DiagnosticText,
    ] {
        apply(drop, &mut stripped, budget);
    }
    if let Some(view) = stripped.activity_mut() {
        let total = view.entries.len();
        super::activity::keep_entries(view, 0, total, super::activity::Continue::Investigation);
    }
    if let Some(validation) = stripped.validation_mut() {
        let total = validation.errors.len();
        if total > 1 {
            validation.errors.truncate(1);
            validation.diagnostics_dropped_to_fit = as_u64(total - 1);
        }
    }
    if let Some(correlation) = stripped.correlation_mut() {
        super::correlation::keep_entries(correlation, 0);
    }
    stripped
}

/// Whether the first of the `rows` rows of `reply` fits `budget` with
/// every droppable part gone, at the columns `reply` prints.
fn row_fits_stripped<R: Fit>(reply: &R, rows: usize, budget: Budget) -> bool {
    rows > 0 && fitted(&keeping(&stripped(reply, budget), 1, rows, None), budget).is_some()
}

/// `reply` printing only the first `width` of the columns it prints.
fn with_width<R: Fit>(reply: &R, width: usize) -> R {
    let mut cut = reply.clone();
    if let Some(view) = cut.view_mut() {
        keep_columns(view, width);
    }
    cut
}

/// The most of the columns `reply` prints, from the first in stored order
/// and at least one, with which the first of its `rows` rows fits `budget`
/// with every droppable part gone; `None` when not even one column lets
/// it fit. Called when all of them do not.
///
/// A binary search over the width, then a step up while one more column
/// still fits, so the width returned was measured.
fn widest_prefix<R: Fit>(reply: &R, rows: usize, budget: Budget) -> Option<usize> {
    let printed = reply
        .clone()
        .view_mut()
        .map_or(0, |view| view.columns.len());
    let fits = |width: usize| row_fits_stripped(&with_width(reply, width), rows, budget);
    if printed < 2 || !fits(1) {
        return None;
    }
    let (mut wide, mut too_wide) = (1, printed);
    while too_wide - wide > 1 {
        let mid = wide + (too_wide - wide) / 2;
        if fits(mid) {
            wide = mid;
        } else {
            too_wide = mid;
        }
    }
    while wide + 1 < printed && fits(wide + 1) {
        wide += 1;
    }
    Some(wide)
}

/// Whether [`fit`] under `budget` prints the first of the `rows` rows of
/// `reply` by omitting a suffix of its columns: one row does not fit with
/// every droppable part gone at the columns `reply` prints, and fits so
/// with fewer of them.
fn prints_with_a_suffix_cut<R: Fit>(reply: &R, rows: usize, budget: Budget) -> bool {
    !row_fits_stripped(reply, rows, budget) && widest_prefix(reply, rows, budget).is_some()
}

/// Omit every column of `reply` whose cell was null in every row the call
/// read, and count them.
fn omit_null_columns<R: Fit>(reply: &mut R) {
    let Some(view) = reply.view_mut() else {
        return;
    };
    let null = view.read.null_columns.clone();
    let omitted = null.iter().filter(|&&null| null).count();
    if omitted == 0 {
        return;
    }
    omit_columns(view, |index| null.get(index).copied().unwrap_or(false));
    view.presentation.null_columns_omitted = as_u64(omitted);
}

/// Print only the first `width` of the columns `view` prints.
fn keep_columns(view: &mut ResultView, width: usize) {
    omit_columns(view, |index| index >= width);
}

/// Omit every column of `view` at a printed index `omit` holds for: from
/// the columns, from every row, and from the abbreviated cells, whose
/// records name a captured position. Every kept column keeps its captured
/// position. `columns_omitted` becomes the columns trawld's projection
/// left out plus every column of the page this view no longer prints.
pub(crate) fn omit_columns(view: &mut ResultView, omit: impl Fn(usize) -> bool) {
    let keep: Vec<bool> = (0..view.columns.len()).map(|index| !omit(index)).collect();
    let mut kept = keep.iter();
    view.columns
        .retain(|_| kept.next().copied().unwrap_or(false));
    for row in &mut view.rows {
        let mut kept = keep.iter();
        row.retain(|_| kept.next().copied().unwrap_or(false));
    }
    let printed: HashSet<u64> = view.columns.iter().map(|column| column.position).collect();
    view.abbreviated
        .retain(|cell| printed.contains(&cell.column));
    view.presentation.cells_abbreviated = as_u64(view.abbreviated.len());
    view.presentation.columns_omitted = view
        .read
        .projected_out
        .saturating_add(view.read.columns.saturating_sub(as_u64(view.columns.len())));
}

/// The reply with the most entries of each sightings list that fit, the
/// lists cut to one length; `None` when the reply has no list to cut or
/// not even empty lists fit. An attached reply's activity entries are all
/// gone first: a dropped entry prints again on the session's next
/// attached call, and a dropped list entry is part of the answer lost.
///
/// A binary search over the kept length, as [`most_rows`] searches rows.
fn most_entries<R: Fit>(
    reply: &R,
    rows: usize,
    budget: Budget,
    page_rows: Option<u64>,
    least: usize,
) -> Option<(String, Json)> {
    let longest = super::correlation::longest_list(reply.clone().correlation_mut()?);
    if longest == 0 {
        return None;
    }
    let mut reply = reply.clone();
    if let Some(view) = reply.activity_mut() {
        let total = view.entries.len();
        super::activity::keep_entries(view, 0, total, super::activity::Continue::Investigation);
    }
    let attempt = |kept: usize| -> Option<(String, Json)> {
        let mut candidate = reply.clone();
        super::correlation::keep_entries(candidate.correlation_mut()?, kept);
        most_rows(&candidate, rows, budget, page_rows, least)
    };
    let mut best = attempt(0)?;
    let (mut fits, mut does_not) = (0, longest);
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

/// The reply with the most of its activity entries that fit, each count
/// fitted with [`most_rows`], so rows go before any activity entry does;
/// `None` when not even no entry fits. A reply without activity is
/// [`most_rows`] alone.
///
/// Entries are cut from the end only, and the activity's facts and
/// continuation say so (ADR-0059): the session counts as seen only what
/// the fitted reply prints.
fn most_activity<R: Fit>(
    reply: &R,
    rows: usize,
    budget: Budget,
    page_rows: Option<u64>,
    least: usize,
) -> Option<(String, Json)> {
    let Some(total) = reply.clone().activity_mut().map(|view| view.entries.len()) else {
        return most_rows(reply, rows, budget, page_rows, least);
    };
    let attempt = |kept: usize| {
        let mut candidate = reply.clone();
        if let Some(view) = candidate.activity_mut() {
            super::activity::keep_entries(
                view,
                kept,
                total,
                super::activity::Continue::Investigation,
            );
        }
        most_rows(&candidate, rows, budget, page_rows, least)
    };
    if let Some(all) = attempt(total) {
        return Some(all);
    }
    let mut best = attempt(0)?;
    let (mut fits, mut does_not) = (0, total);
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

/// The reply with the most of its first diagnostics that fit, at least
/// one, rendered with the count it dropped; `None` when the reply has no
/// verdict or not even its first diagnostic fits.
///
/// A binary search over the kept count, as [`most_rows`] searches rows.
fn most_diagnostics<R: Fit>(
    reply: &R,
    rows: usize,
    budget: Budget,
    page_rows: Option<u64>,
    least: usize,
) -> Option<(String, Json)> {
    let total = reply.clone().validation_mut()?.errors.len();
    if total < 2 {
        return None;
    }
    let attempt = |kept: usize| -> Option<(String, Json)> {
        let mut candidate = reply.clone();
        let validation = candidate.validation_mut()?;
        validation.errors.truncate(kept);
        validation.diagnostics_dropped_to_fit = as_u64(total - kept);
        most_rows(&candidate, rows, budget, page_rows, least)
    };
    let mut best = attempt(1)?;
    let (mut fits, mut does_not) = (1, total);
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

/// `text` cut to at most `max` bytes on a character boundary, ending with
/// a marker that says so; unchanged when it is no longer than `max`.
fn cut_to(text: &mut String, max: usize) {
    if text.len() <= max {
        return;
    }
    let marker = format!("…[cut at {max} bytes to fit the response's byte budget]");
    let keep = floor_char_boundary(text, max.saturating_sub(marker.len()));
    text.truncate(keep);
    text.push_str(&marker);
}

/// Give up one part of `reply`, saying so in its presentation facts.
fn apply<R: Fit>(drop: Drop, reply: &mut R, budget: Budget) {
    match drop {
        Drop::Nothing => {}
        Drop::Facets => {
            if let Some(view) = reply.view_mut()
                && !view.facets.is_empty()
            {
                view.facets.clear();
                view.presentation.facets_dropped_to_fit = true;
            }
        }
        Drop::NextActions => {
            if !reply.next_actions_mut().is_empty() {
                reply.next_actions_mut().clear();
                if let Some(view) = reply.view_mut() {
                    view.presentation.next_actions_dropped_to_fit = true;
                }
            }
        }
        Drop::Link => {
            if let Some(view) = reply.view_mut()
                && view.link.url.is_some()
            {
                view.link.url = None;
                view.link.reason = Some(LINK_DROPPED.to_owned());
            }
        }
        Drop::Hints => {
            if let Some(hints) = reply.hints_mut()
                && !hints.is_empty()
            {
                hints.clear();
                if let Some(view) = reply.view_mut() {
                    view.presentation.hints_dropped_to_fit = true;
                }
            }
        }
        Drop::DiagnosticText => {
            if let Some(validation) = reply.validation_mut() {
                let max = budget.bytes() / DIAGNOSTIC_TEXT_SHARE;
                for diagnostic in &mut validation.errors {
                    cut_to(&mut diagnostic.message, max);
                    if let Some(hint) = diagnostic.hint.as_mut() {
                        cut_to(hint, max);
                    }
                }
            }
        }
    }
}

/// The reply with the most of its first `rows` rows that fit, rendered,
/// or `None` when no count of them fits, not even zero, or not even one
/// when `least` is 1.
///
/// A binary search over the positive row counts, from one; every answer
/// it returns was measured, so it fits even where dropping a row grows a
/// count's digits. The reply that keeps no row is tried only when not
/// even one row fits and `least` is 0, because it is not the smallest: its
/// outcome line can be longer than one naming a printed range, and it
/// proposes a continuation that advances. Each candidate carries the
/// continuation [`keeping`] gives it.
fn most_rows<R: Fit>(
    reply: &R,
    rows: usize,
    budget: Budget,
    page_rows: Option<u64>,
    least: usize,
) -> Option<(String, Json)> {
    let attempt = |kept: usize| fitted(&keeping(reply, kept, rows, page_rows), budget);
    if let Some(all) = attempt(rows) {
        return Some(all);
    }
    if rows == 0 {
        return None;
    }
    let Some(mut best) = (rows > 1).then(|| attempt(1)).flatten() else {
        return if least == 0 { attempt(0) } else { None };
    };
    let (mut fits, mut does_not) = (1, rows);
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

/// `reply` keeping the first `kept` of the `rows` rows it read, saying so
/// through [`Fit::rows_kept`], with the page continuation from the rows it
/// keeps: the tool's, moved, or one
/// created asking for `page_rows` rows. With `page_rows` `None`, the
/// proposed next calls were dropped, and the candidate says so instead.
///
/// A candidate that keeps none of the rows it read would continue with the
/// very call that made it, so its continuation must advance: the next row
/// alone at [`Budget::HARD`] when a one-row candidate of `reply` fits
/// there, or the fitting there prints it by omitting a suffix of its
/// columns, else that row's cells one at a time from the page's first
/// captured column. Either one leads the proposed calls in place of
/// the tool's page continuation.
fn keeping<R: Fit>(reply: &R, kept: usize, rows: usize, page_rows: Option<u64>) -> R {
    let mut candidate = reply.clone();
    let Some((result_id, offset, first_column, projection)) = candidate.view_mut().map(|view| {
        keep_rows(view, kept, rows);
        (
            view.result_id.clone(),
            view.offset,
            view.read.first_position,
            view.read.projection.clone(),
        )
    }) else {
        return candidate;
    };
    if kept == rows {
        return candidate;
    }
    candidate.rows_kept();
    let read_end = offset.saturating_add(as_u64(rows));
    let kept_end = offset.saturating_add(as_u64(kept));
    match page_rows {
        None => {
            if let Some(view) = candidate.view_mut() {
                view.presentation.next_actions_dropped_to_fit = true;
            }
        }
        Some(page_rows) if kept > 0 => {
            let actions = candidate.next_actions_mut();
            move_continuations(actions, ("page", "offset"), read_end, kept_end);
            ensure_continuation(
                actions,
                NextAction {
                    tool: READ.to_owned(),
                    arguments: projected(
                        object(json!({
                            "action": "page",
                            "result_id": result_id,
                            "offset": kept_end,
                            "rows": page_rows,
                        })),
                        projection.as_deref(),
                    ),
                    reason: PAGE_CONTINUATION.to_owned(),
                },
            );
        }
        Some(page_rows) => {
            let next_row_fits = fitted(&keeping(reply, 1, rows, Some(page_rows)), Budget::HARD)
                .is_some()
                || prints_with_a_suffix_cut(reply, rows, Budget::HARD);
            let actions = candidate.next_actions_mut();
            actions.retain(|proposed| !reads_from(proposed, ("page", "offset"), read_end));
            lead_with(
                actions,
                advance(
                    &result_id,
                    offset,
                    next_row_fits,
                    first_column,
                    projection.as_deref(),
                ),
            );
        }
    }
    candidate
}

/// The arguments of a `read` page call, with the `columns` of
/// `projection`, the call's own, when it projected the page: a page
/// continuation reads the columns the page it continues read.
pub(crate) fn projected(
    mut arguments: Map<String, Json>,
    projection: Option<&[String]>,
) -> Map<String, Json> {
    if let Some(columns) = projection {
        arguments.insert("columns".to_owned(), json!(columns));
    }
    arguments
}

/// The continuation of a page that printed none of its rows, from
/// captured row `offset`: that row alone at [`Budget::HARD`], with the
/// page's `projection`, when it fits there, else its cells one at a time
/// from captured column `first_column`, the page's first. A cell call
/// names a captured position, so it sends no projection.
fn advance(
    result_id: &str,
    offset: u64,
    next_row_fits: bool,
    first_column: u64,
    projection: Option<&[String]>,
) -> NextAction {
    if next_row_fits {
        NextAction {
            tool: READ.to_owned(),
            arguments: projected(
                object(json!({
                    "action": "page",
                    "result_id": result_id,
                    "offset": offset,
                    "rows": 1,
                    "max_bytes": HARD_MAX_BYTES,
                })),
                projection,
            ),
            reason: format!(
                "the next row does not fit this call's max_bytes; read it alone at \
                 max_bytes={HARD_MAX_BYTES}, the hard cap"
            ),
        }
    } else {
        NextAction {
            tool: READ.to_owned(),
            arguments: object(json!({
                "action": "cell",
                "result_id": result_id,
                "row": offset,
                "column": first_column,
            })),
            reason: row_by_cells(first_column),
        }
    }
}

/// `candidate` rendered under `budget`: its text and its mirror, or `None`
/// when they do not fit.
pub(crate) fn fitted<R: Fit>(candidate: &R, budget: Budget) -> Option<(String, Json)> {
    let mirror = mirror(candidate).ok()?;
    let text = candidate.text(budget);
    (measure(&text, &mirror) <= budget.bytes()).then_some((text, mirror))
}

/// Keep the first `kept` of the view's `read` rows and make its
/// presentation facts say so.
pub(crate) fn keep_rows(view: &mut ResultView, kept: usize, read: usize) {
    view.rows.truncate(kept);
    let end = view.offset.saturating_add(as_u64(kept));
    view.abbreviated.retain(|cell| cell.row < end);
    view.presentation.cells_abbreviated = as_u64(view.abbreviated.len());
    view.presentation.rows_omitted = view.rows_captured.saturating_sub(end);
    view.presentation.rows_dropped_to_fit = as_u64(read.saturating_sub(kept));
}

/// Move the continuation of a cell range whose bytes were cut: every
/// proposed `read` cell call that started at `original_next`, the range's
/// end before the cut, now starts where the printed bytes end. A range
/// that ran to the cell's end proposed no continuation, so when the cut
/// leaves bytes the fitting creates one where the printed bytes end.
pub(crate) fn follow_cell_cut(reply: &mut ReadReply, original_next: Option<u64>) {
    let Some(cell) = reply.cell.as_ref() else {
        return;
    };
    let Some(cut_next) = cell.next_offset else {
        return;
    };
    let continuation = NextAction {
        tool: READ.to_owned(),
        arguments: object(json!({
            "action": "cell",
            "result_id": cell.result_id,
            "row": cell.row,
            "column": cell.column,
            "byte_offset": cut_next,
        })),
        reason: CELL_CONTINUATION.to_owned(),
    };
    if let Some(from) = original_next {
        move_continuations(
            &mut reply.next_actions,
            ("cell", "byte_offset"),
            from,
            cut_next,
        );
    }
    ensure_continuation(&mut reply.next_actions, continuation);
}

/// Make `actions` hold `continuation`. A proposed call that
/// [`continues`] there already does; otherwise `continuation` goes first,
/// and when the calls are at [`MAX_NEXT_ACTIONS`] it replaces the last,
/// because a continuation outranks a suggestion.
fn ensure_continuation(actions: &mut Vec<NextAction>, continuation: NextAction) {
    if actions
        .iter()
        .any(|proposed| continues(proposed, &continuation))
    {
        return;
    }
    actions.truncate(MAX_NEXT_ACTIONS - 1);
    actions.insert(0, continuation);
}

/// Put `continuation` first in `actions`, in place of every proposed call
/// that [`continues`] there, and of the last when the calls are at
/// [`MAX_NEXT_ACTIONS`]: its reason says why the call that made this reply
/// is not repeated as it was.
fn lead_with(actions: &mut Vec<NextAction>, continuation: NextAction) {
    actions.retain(|proposed| !continues(proposed, &continuation));
    actions.truncate(MAX_NEXT_ACTIONS - 1);
    actions.insert(0, continuation);
}

/// Whether `proposed` calls the tool of `continuation` with every argument
/// of `continuation` but `rows`.
fn continues(proposed: &NextAction, continuation: &NextAction) -> bool {
    proposed.tool == continuation.tool
        && continuation
            .arguments
            .iter()
            .filter(|(key, _)| key.as_str() != "rows")
            .all(|(key, value)| proposed.arguments.get(key) == Some(value))
}

/// Whether `proposed` is a `read` call whose `action` is `kind.0` and whose
/// `kind.1` argument is `at`.
fn reads_from(proposed: &NextAction, kind: (&str, &str), at: u64) -> bool {
    let (action, key) = kind;
    proposed.tool == READ
        && proposed.arguments.get("action").and_then(Json::as_str) == Some(action)
        && proposed.arguments.get(key).and_then(Json::as_u64) == Some(at)
}

/// Point every proposed `read` call whose `action` is `kind.0` and whose
/// `kind.1` argument is `from` at `to` instead. Other calls stay as the
/// tool proposed them.
fn move_continuations(actions: &mut [NextAction], kind: (&str, &str), from: u64, to: u64) {
    if from == to {
        return;
    }
    for proposed in actions
        .iter_mut()
        .filter(|proposed| reads_from(proposed, kind, from))
    {
        proposed.arguments.insert(kind.1.to_owned(), Json::from(to));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_budget_stays_within_its_bounds() {
        assert_eq!(Budget::new(None), Budget::DEFAULT);
        assert_eq!(Budget::new(Some(1)), Budget::MIN);
        assert_eq!(Budget::new(Some(u32::MAX)), Budget::HARD);
        assert_eq!(Budget::new(Some(5000)).bytes(), 5000);
    }
}
