// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared UI helpers used across multiple rendering modules.

use ratatui::layout::{Constraint, Layout, Rect};
use unicode_width::UnicodeWidthStr;

/// Create a centered rect using percentage-based constraints.
pub fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let pad_y = (100 - percent_y) / 2;
    let pad_x = (100 - percent_x) / 2;

    let [_, mid, _] = Layout::vertical([
        Constraint::Percentage(pad_y),
        Constraint::Percentage(percent_y),
        Constraint::Percentage(pad_y),
    ])
    .areas(r);

    let [_, center, _] = Layout::horizontal([
        Constraint::Percentage(pad_x),
        Constraint::Percentage(percent_x),
        Constraint::Percentage(pad_x),
    ])
    .areas(mid);

    center
}

/// `fitted`, then spaces until a ratatui `Span` holding it advances its
/// `Line` by `column` cells.
///
/// A `Line` moves the next span on by `Span::width`, the unicode-width of
/// the whole span, which can be less than the cells its grapheme clusters
/// draw in: `ｶﾞ` draws in two cells and has a width of one. `fitted` must
/// already fit `column` cells as
/// [`fit_cells`](trawl_core::text_layout::fit_cells) measures them, so its
/// glyphs end inside the column. The next span starts at the column's edge
/// and draws over any spaces drawn past it.
pub fn span_advancing(mut fitted: String, column: usize) -> String {
    let advance = UnicodeWidthStr::width(fitted.as_str());
    fitted.extend(std::iter::repeat_n(' ', column.saturating_sub(advance)));
    fitted
}
