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
//! many it dropped. When even no rows fit, the facets go, then the
//! proposed next calls, then the search URL (the reason says why), then
//! the hints, each said in the text, and after each the most rows that fit
//! are printed again. A reply that does not fit with all of those gone is
//! not sent: the caller gets an error text naming the result id and the
//! budget, so it can read the result with a larger one.

use serde::Serialize;
use serde_json::Value as Json;

use crate::reply::{NextAction, ResultView};
use crate::tools::schema::{DEFAULT_MAX_BYTES, HARD_MAX_BYTES, MIN_MAX_BYTES};

use super::rows::as_u64;

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
}

/// What the fitting gives up, in order, before it gives up the reply.
#[derive(Debug, Clone, Copy)]
enum Drop {
    Nothing,
    Facets,
    NextActions,
    Link,
    Hints,
}

/// The link's reason when the fitting dropped the search URL.
pub(crate) const LINK_DROPPED: &str = "the search URL did not fit the response's byte budget";

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
    ] {
        apply(drop, &mut reply);
        if let Some(fitted) = most_rows(&reply, rows, budget) {
            return Some(fitted);
        }
    }
    None
}

/// Give up one part of `reply`, saying so in its presentation facts.
fn apply<R: Fit>(drop: Drop, reply: &mut R) {
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
        if let Some(view) = candidate.view_mut() {
            keep_rows(view, kept, rows);
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
