// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Mouse event handlers — click, scroll wheel dispatch.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use super::super::App;
use super::super::state::{Focus, MainTab, Popup, TabStatus};

impl App {
    /// Top-level mouse event dispatch.
    pub(crate) fn handle_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.handle_click(mouse.column, mouse.row);
            }
            MouseEventKind::ScrollUp => {
                self.handle_scroll(mouse.column, mouse.row, -3);
            }
            MouseEventKind::ScrollDown => {
                self.handle_scroll(mouse.column, mouse.row, 3);
            }
            // Ignore drag, right-click, middle-click, release, hover.
            _ => {}
        }
    }

    /// Handle a left-click at the given terminal coordinates.
    fn handle_click(&mut self, col: u16, row: u16) {
        // Popup takes priority: click outside dismisses (except `ConfirmDelete`).
        if self.popup.is_some() {
            if let Some(popup_rect) = self.layout.popup {
                if contains(popup_rect, col, row) {
                    // Click inside popup — no-op for v1.
                    return;
                }
                // Click outside popup — dismiss unless it's ConfirmDelete.
                if !matches!(self.popup, Some(Popup::ConfirmDelete { .. })) {
                    self.popup = None;
                }
            }
            return;
        }

        // Tab bar click.
        if contains(self.layout.tab_bar, col, row) {
            self.handle_tab_bar_click(col);
            return;
        }

        // Editor area click.
        if let Some(editor_area) = self.layout.editor
            && contains(editor_area, col, row)
        {
            self.focus = Focus::Editor;
            self.handle_editor_click(col, row, editor_area);
            return;
        }

        // Results area click.
        if let Some(results_area) = self.layout.results
            && contains(results_area, col, row)
        {
            self.focus = Focus::Results;
            self.handle_results_click(col, row, results_area);
            return;
        }

        // Panel area click.
        if let Some(panel_area) = self.layout.panel
            && contains(panel_area, col, row)
        {
            self.focus = Focus::Panel;
            self.handle_panel_click(col, row, panel_area);
        }
    }

    /// Derive which tab was clicked from the x-coordinate and switch to it.
    ///
    /// Replicates the label-width logic from `ui/tabs.rs`.
    fn handle_tab_bar_click(&mut self, click_x: u16) {
        let width = self.layout.tab_bar.width as usize;
        let show_shortcuts = width >= 75;

        let mut tabs: Vec<(MainTab, &str, &str)> = vec![
            (MainTab::Query, "Query", "M-1"),
            (MainTab::History, "History", "M-2"),
            (MainTab::Schema, "Schema", "M-3"),
            (MainTab::Saved, "Saved", "M-4"),
        ];
        if self.dashboard.is_admin {
            tabs.push((MainTab::Dashboard, "Dashboard", "M-5"));
        }

        let mut x = self.layout.tab_bar.x;
        for (idx, (tab, label, shortcut)) in tabs.iter().enumerate() {
            // Compute displayed label width (matches tabs.rs rendering).
            let label_text = if *tab == MainTab::Query {
                let indicator = match &self.tab.status {
                    TabStatus::Idle => "",
                    TabStatus::Running { .. } => " ◉",
                    TabStatus::Success { .. } => " ✓",
                    TabStatus::Error { .. } => " ✗",
                };
                if show_shortcuts {
                    format!(" {shortcut} {label}{indicator} ")
                } else {
                    format!(" {label}{indicator} ")
                }
            } else if show_shortcuts {
                format!(" {shortcut} {label} ")
            } else {
                format!(" {label} ")
            };

            // Unicode-aware display width.
            #[allow(clippy::cast_possible_truncation)]
            let span_width = unicode_display_width(&label_text) as u16;

            if click_x >= x && click_x < x + span_width {
                self.switch_to_main_tab(*tab);
                return;
            }
            x += span_width;

            // 1-char separator between tabs.
            if idx < tabs.len() - 1 {
                x += 1;
            }
        }
    }

    /// Place editor cursor at the clicked position.
    fn handle_editor_click(&mut self, col: u16, row: u16, area: Rect) {
        let tab = self.active_tab_mut();
        let scroll_row = tab.editor.scroll_row;
        let scroll_col = tab.editor.scroll_col;

        // Reverse the cursor math from ui/mod.rs render_query_layout:
        //   x = area.x + col_offset + 2  (border + padding)
        //   y = area.y + row_offset + 1  (border)
        let target_row = (row.saturating_sub(area.y).saturating_sub(1) as usize) + scroll_row;
        let target_col = (col.saturating_sub(area.x).saturating_sub(2) as usize) + scroll_col;

        tab.editor.place_cursor(target_row, target_col);
    }

    /// Select a result row at the clicked position.
    fn handle_results_click(&mut self, _col: u16, row: u16, area: Rect) {
        let row_count = self
            .active_tab()
            .result
            .as_ref()
            .map_or(0, |r| r.result.row_count());
        if row_count == 0 {
            return;
        }

        // Data rows start at area.y + 2 (border + header row).
        let data_start_y = area.y + 2;
        if row < data_start_y {
            return;
        }

        let scroll_offset = self.active_tab().scroll_offset;
        let relative_row = (row - data_start_y) as usize;
        let absolute_row = relative_row + scroll_offset;

        if absolute_row < row_count {
            self.active_tab_mut().selected_row = Some(absolute_row);
        }
    }

    /// Handle click within a panel area (History/Schema/Saved).
    fn handle_panel_click(&mut self, col: u16, row: u16, area: Rect) {
        match self.main_tab {
            MainTab::History => {
                let item_count = self.history_cache.as_ref().map_or(0, |h| h.entries.len());
                if item_count == 0 {
                    return;
                }
                // List items start at area.y + 1 (border).
                let content_start_y = area.y + 1;
                if row < content_start_y {
                    return;
                }
                let relative = (row - content_start_y) as usize;
                if relative < item_count {
                    self.panel.history_selected = relative;
                }
            }
            MainTab::Schema => {
                // Only clicks in the left tree half (55% of panel width) should select.
                #[allow(clippy::cast_possible_truncation)]
                let tree_width = (u32::from(area.width) * 55 / 100) as u16;
                if col >= area.x + tree_width {
                    return;
                }

                // Deactivate filter input on tree click.
                if let Some(schema) = self.panel.schema.as_mut()
                    && schema.filter_active
                {
                    schema.filter_active = false;
                }

                let node_count = self.visible_tree_node_count();
                if node_count == 0 {
                    return;
                }

                let scroll = self.panel.schema.as_ref().map_or(0, |s| s.scroll);

                // Tree items start at area.y + 1 (border) + 1 (filter bar) = area.y + 2.
                let content_start_y = area.y + 2;
                if row < content_start_y {
                    return;
                }
                let relative = (row - content_start_y) as usize + scroll;
                if relative < node_count
                    && let Some(schema) = self.panel.schema.as_mut()
                {
                    schema.selected = relative;
                }
            }
            MainTab::Saved => {
                let item_count = self.saved_cache.as_ref().map_or(0, |s| s.queries.len());
                if item_count == 0 {
                    return;
                }
                let content_start_y = area.y + 1;
                if row < content_start_y {
                    return;
                }
                let relative = (row - content_start_y) as usize;
                if relative < item_count {
                    self.panel.saved_selected = relative;
                }
            }
            // Dashboard and Query don't have clickable panel lists.
            _ => {}
        }
    }

    /// Handle scroll wheel over the pane under the cursor.
    fn handle_scroll(&mut self, col: u16, row: u16, delta: i32) {
        // Popup scroll (Help and `EventDetail` support it).
        if let Some(popup_rect) = self.layout.popup
            && contains(popup_rect, col, row)
        {
            self.handle_popup_scroll(delta);
            return;
        }

        // Editor scroll.
        if let Some(editor_area) = self.layout.editor
            && contains(editor_area, col, row)
        {
            let tab = self.active_tab_mut();
            let max_scroll = tab.editor.lines.len().saturating_sub(1);
            tab.editor.scroll_row = apply_scroll_delta(tab.editor.scroll_row, delta, max_scroll);
            return;
        }

        // Results scroll.
        if let Some(results_area) = self.layout.results
            && contains(results_area, col, row)
        {
            let row_count = self
                .active_tab()
                .result
                .as_ref()
                .map_or(0, |r| r.result.row_count());
            let visible = self.active_tab().last_visible_rows;
            let max_scroll = row_count.saturating_sub(visible);
            let tab = self.active_tab_mut();
            tab.scroll_offset = apply_scroll_delta(tab.scroll_offset, delta, max_scroll);
            return;
        }

        // Panel scroll.
        if let Some(panel_area) = self.layout.panel
            && contains(panel_area, col, row)
        {
            self.handle_panel_scroll(delta);
        }
    }

    /// Scroll inside a popup (Help or `EventDetail`).
    fn handle_popup_scroll(&mut self, delta: i32) {
        if let Some(Popup::Help { scroll } | Popup::EventDetail { scroll, .. }) = &mut self.popup {
            *scroll = apply_scroll_delta(*scroll, delta, usize::MAX / 2);
        }
    }

    /// Scroll panel content by delta (dispatches by active tab).
    fn handle_panel_scroll(&mut self, delta: i32) {
        match self.main_tab {
            MainTab::History => {
                let max = self
                    .history_cache
                    .as_ref()
                    .map_or(0, |h| h.entries.len().saturating_sub(1));
                self.panel.history_selected =
                    apply_scroll_delta(self.panel.history_selected, delta, max);
            }
            MainTab::Schema => {
                let max = self.visible_tree_node_count().saturating_sub(1);
                if let Some(schema) = self.panel.schema.as_mut() {
                    schema.selected = apply_scroll_delta(schema.selected, delta, max);
                }
            }
            MainTab::Saved => {
                let max = self
                    .saved_cache
                    .as_ref()
                    .map_or(0, |s| s.queries.len().saturating_sub(1));
                self.panel.saved_selected =
                    apply_scroll_delta(self.panel.saved_selected, delta, max);
            }
            _ => {}
        }
    }
}

/// Check if a point (col, row) is inside a `Rect`.
fn contains(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x && col < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}

/// Apply a signed scroll delta to a usize offset, clamping to [0, max].
fn apply_scroll_delta(current: usize, delta: i32, max: usize) -> usize {
    if delta < 0 {
        current.saturating_sub(delta.unsigned_abs() as usize)
    } else {
        #[allow(clippy::cast_sign_loss)] // delta is positive here
        (current + delta as usize).min(max)
    }
}

/// Compute the unicode display width of a string.
///
/// Simple heuristic: each `char` = 1 cell. Handles the indicators (◉, ✓, ✗)
/// which are all single-width.
fn unicode_display_width(s: &str) -> usize {
    s.chars().count()
}
