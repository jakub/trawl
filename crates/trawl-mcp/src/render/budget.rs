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
//! many it dropped. A tool proposes a page's continuation at the end of the
//! rows it read; when the fitting keeps fewer, the continuation moves to
//! the end of the rows printed, and a shortened cell range's continuation
//! to where the printed bytes end. When even no rows fit, the facets go, then the
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
use serde_json::Value as Json;

use crate::reply::{NextAction, ReadReply, ResultView, ValidationView};
use crate::tools::schema::{DEFAULT_MAX_BYTES, HARD_MAX_BYTES, MIN_MAX_BYTES, READ};

use super::rows::{as_u64, floor_char_boundary};

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
    /// The hints, for a reply that has them.
    fn hints_mut(&mut self) -> Option<&mut Vec<String>>;
    /// The validation verdict whose diagnostics may be cut and dropped,
    /// for a reply that has one.
    fn validation_mut(&mut self) -> Option<&mut ValidationView> {
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
pub(crate) fn fit<R: Fit>(mut reply: R, budget: Budget) -> Option<(String, Json)> {
    let rows = reply.view_mut().map_or(0, |view| view.rows.len());
    for drop in [
        Drop::Nothing,
        Drop::Facets,
        Drop::NextActions,
        Drop::Link,
        Drop::Hints,
        Drop::DiagnosticText,
    ] {
        apply(drop, &mut reply, budget);
        if let Some(fitted) = most_rows(&reply, rows, budget) {
            return Some(fitted);
        }
    }
    most_diagnostics(&reply, rows, budget)
}

/// The reply with the most of its first diagnostics that fit, at least
/// one, rendered with the count it dropped; `None` when the reply has no
/// verdict or not even its first diagnostic fits.
///
/// A binary search over the kept count, as [`most_rows`] searches rows.
fn most_diagnostics<R: Fit>(reply: &R, rows: usize, budget: Budget) -> Option<(String, Json)> {
    let total = reply.clone().validation_mut()?.errors.len();
    if total < 2 {
        return None;
    }
    let attempt = |kept: usize| -> Option<(String, Json)> {
        let mut candidate = reply.clone();
        let validation = candidate.validation_mut()?;
        validation.errors.truncate(kept);
        validation.diagnostics_dropped_to_fit = as_u64(total - kept);
        most_rows(&candidate, rows, budget)
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
/// or `None` when not even zero rows fit.
///
/// A binary search over the row count; every answer it returns was
/// measured, so it fits even where dropping a row grows a count's digits.
fn most_rows<R: Fit>(reply: &R, rows: usize, budget: Budget) -> Option<(String, Json)> {
    let attempt = |kept: usize| -> Option<(String, Json)> {
        let mut candidate = reply.clone();
        let ends = candidate.view_mut().map(|view| {
            keep_rows(view, kept, rows);
            (
                view.offset.saturating_add(as_u64(rows)),
                view.offset.saturating_add(as_u64(kept)),
            )
        });
        if let Some((read_end, kept_end)) = ends {
            move_continuations(
                candidate.next_actions_mut(),
                ("page", "offset"),
                read_end,
                kept_end,
            );
        }
        let mirror = serde_json::to_value(&candidate).ok()?;
        let text = candidate.text(budget);
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
/// that ran to the cell's end proposed no continuation, and gets none.
pub(crate) fn follow_cell_cut(reply: &mut ReadReply, original_next: Option<u64>) {
    let cut_next = reply.cell.as_ref().and_then(|cell| cell.next_offset);
    if let (Some(from), Some(to)) = (original_next, cut_next) {
        move_continuations(&mut reply.next_actions, ("cell", "byte_offset"), from, to);
    }
}

/// Point every proposed `read` call whose `action` is `kind.0` and whose
/// `kind.1` argument is `from` at `to` instead. Other calls stay as the
/// tool proposed them, and none is added.
fn move_continuations(actions: &mut [NextAction], kind: (&str, &str), from: u64, to: u64) {
    if from == to {
        return;
    }
    let (action, key) = kind;
    for proposed in actions.iter_mut().filter(|proposed| proposed.tool == READ) {
        let arguments = &mut proposed.arguments;
        if arguments.get("action").and_then(Json::as_str) == Some(action)
            && arguments.get(key).and_then(Json::as_u64) == Some(from)
        {
            arguments.insert(key.to_owned(), Json::from(to));
        }
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
