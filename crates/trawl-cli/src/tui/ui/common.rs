// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared UI helpers used across multiple rendering modules.

use ratatui::layout::{Constraint, Layout, Rect};

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
