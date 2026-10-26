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
//! with the rest. A continuation always advances: when not one row of the
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
//! counts them. A reply that does not fit with all of those gone is not
//! sent: the caller gets an error text naming the result id and the
//! budget, so it can read the result with a larger one.

use serde::Serialize;
use serde_json::{Value as Json, json};

use crate::reply::{ActivityView, NextAction, ReadReply, ResultView, ValidationView};
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
/// Why the fitting proposes a row's first cell: a page printed none of its
/// rows, and the next does not fit a page even at [`Budget::HARD`].
pub(crate) const ROW_BY_CELLS: &str =
    "the row does not fit a page at any max_bytes; read its cells one at a time, from column 0";

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
pub(crate) fn fit<R: Fit>(mut reply: R, budget: Budget, page_rows: u64) -> Option<(String, Json)> {
    let rows = reply.view_mut().map_or(0, |view| view.rows.len());
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
        if let Some(fitted) = most_activity(&reply, rows, budget, page_rows) {
            return Some(fitted);
        }
    }
    most_diagnostics(&reply, rows, budget, page_rows)
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
) -> Option<(String, Json)> {
    let Some(total) = reply.clone().activity_mut().map(|view| view.entries.len()) else {
        return most_rows(reply, rows, budget, page_rows);
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
        most_rows(&candidate, rows, budget, page_rows)
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
        most_rows(&candidate, rows, budget, page_rows)
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
/// or `None` when no count of them fits, not even zero.
///
/// A binary search over the positive row counts, from one; every answer
/// it returns was measured, so it fits even where dropping a row grows a
/// count's digits. The reply that keeps no row is tried only when not
/// even one row fits, because it is not the smallest: its outcome line
/// can be longer than one naming a printed range, and it proposes a
/// continuation that advances. Each candidate carries the continuation
/// [`keeping`] gives it.
fn most_rows<R: Fit>(
    reply: &R,
    rows: usize,
    budget: Budget,
    page_rows: Option<u64>,
) -> Option<(String, Json)> {
    let attempt = |kept: usize| fitted(&keeping(reply, kept, rows, page_rows), budget);
    if let Some(all) = attempt(rows) {
        return Some(all);
    }
    if rows == 0 {
        return None;
    }
    let Some(mut best) = (rows > 1).then(|| attempt(1)).flatten() else {
        return attempt(0);
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
/// there, else that row's cells one at a time from column 0. Either one
/// leads the proposed calls in place of the tool's page continuation.
fn keeping<R: Fit>(reply: &R, kept: usize, rows: usize, page_rows: Option<u64>) -> R {
    let mut candidate = reply.clone();
    let Some((result_id, offset)) = candidate.view_mut().map(|view| {
        keep_rows(view, kept, rows);
        (view.result_id.clone(), view.offset)
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
                    arguments: object(json!({
                        "action": "page",
                        "result_id": result_id,
                        "offset": kept_end,
                        "rows": page_rows,
                    })),
                    reason: PAGE_CONTINUATION.to_owned(),
                },
            );
        }
        Some(page_rows) => {
            let next_row_fits =
                fitted(&keeping(reply, 1, rows, Some(page_rows)), Budget::HARD).is_some();
            let actions = candidate.next_actions_mut();
            actions.retain(|proposed| !reads_from(proposed, ("page", "offset"), read_end));
            lead_with(actions, advance(&result_id, offset, next_row_fits));
        }
    }
    candidate
}

/// The continuation of a page that printed none of its rows, from
/// captured row `offset`: that row alone at [`Budget::HARD`] when it fits
/// there, else its cells one at a time.
fn advance(result_id: &str, offset: u64, next_row_fits: bool) -> NextAction {
    if next_row_fits {
        NextAction {
            tool: READ.to_owned(),
            arguments: object(json!({
                "action": "page",
                "result_id": result_id,
                "offset": offset,
                "rows": 1,
                "max_bytes": HARD_MAX_BYTES,
            })),
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
                "column": 0,
            })),
            reason: ROW_BY_CELLS.to_owned(),
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
