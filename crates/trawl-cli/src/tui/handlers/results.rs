// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Results pane key handling — row selection, search, scrolling, chart view cycling.

use crossterm::event::{self, KeyCode, KeyModifiers};

use super::super::App;
use super::super::state::{Focus, Popup, ResultsSearch, TabStatus};
use super::super::ui;

impl App {
    /// Handle key events when results are focused.
    #[allow(clippy::too_many_lines)] // Key dispatch with search + column modes requires many arms
    pub(crate) fn handle_results_key(&mut self, key: event::KeyEvent) {
        // If search input is active, route keys to the search bar first.
        if self.results_search.as_ref().is_some_and(|s| s.input_active) {
            self.handle_search_input_key(key);
            return;
        }

        // If column mode is active, route to column mode handler.
        if self
            .active_tab()
            .column_config
            .as_ref()
            .is_some_and(|c| c.selected.is_some())
        {
            self.handle_column_mode_key(key);
            return;
        }

        let row_count = self
            .active_tab()
            .result
            .as_ref()
            .map_or(0, |r| r.result.row_count());

        match (key.modifiers, key.code) {
            // Tab: cycle to editor
            (KeyModifiers::NONE, KeyCode::Tab) => {
                self.focus = Focus::Editor;
            }
            // Open search with `/`
            (KeyModifiers::NONE, KeyCode::Char('/')) => {
                if self.active_tab().result.is_some() {
                    self.results_search = Some(ResultsSearch::new());
                }
            }
            // Next match: n
            (KeyModifiers::NONE, KeyCode::Char('n')) => {
                if let Some(ref mut search) = self.results_search {
                    search.next_match();
                    self.jump_to_current_match();
                }
            }
            // Prev match: N (Shift+n)
            (KeyModifiers::SHIFT, KeyCode::Char('N')) => {
                if let Some(ref mut search) = self.results_search {
                    search.prev_match();
                    self.jump_to_current_match();
                }
            }
            // Enter column mode: c
            (KeyModifiers::NONE, KeyCode::Char('c')) => {
                if let Some(ref mut config) = self.active_tab_mut().column_config
                    && config.visible_count() > 0
                {
                    let first = config.display_order().into_iter().next();
                    config.selected = first;
                }
            }
            // Open column picker: H (Shift+h)
            (KeyModifiers::SHIFT, KeyCode::Char('H')) => {
                if self.active_tab().column_config.is_some() {
                    self.popup = Some(Popup::ColumnPicker {
                        selected: 0,
                        scroll: 0,
                    });
                }
            }
            // Row selection: Up
            (KeyModifiers::NONE, KeyCode::Up) => {
                if row_count > 0 {
                    let tab = self.active_tab_mut();
                    tab.selected_row = Some(match tab.selected_row {
                        Some(r) => r.saturating_sub(1),
                        None => 0,
                    });
                    self.ensure_selected_row_visible();
                }
            }
            // Row selection: Down
            (KeyModifiers::NONE, KeyCode::Down) => {
                if row_count > 0 {
                    let tab = self.active_tab_mut();
                    let max_row = row_count.saturating_sub(1);
                    tab.selected_row = Some(match tab.selected_row {
                        Some(r) => (r + 1).min(max_row),
                        None => 0,
                    });
                    self.ensure_selected_row_visible();
                }
            }
            // Page up: move selection by one page
            (KeyModifiers::NONE, KeyCode::PageUp) => {
                if row_count > 0 {
                    let tab = self.active_tab_mut();
                    let page = tab.last_visible_rows;
                    tab.selected_row = Some(match tab.selected_row {
                        Some(r) => r.saturating_sub(page),
                        None => 0,
                    });
                    self.ensure_selected_row_visible();
                }
            }
            // Page down: move selection by one page
            (KeyModifiers::NONE, KeyCode::PageDown) => {
                if row_count > 0 {
                    let tab = self.active_tab_mut();
                    let page = tab.last_visible_rows;
                    let max_row = row_count.saturating_sub(1);
                    tab.selected_row = Some(match tab.selected_row {
                        Some(r) => (r + page).min(max_row),
                        None => 0,
                    });
                    self.ensure_selected_row_visible();
                }
            }
            // Home: jump to first row
            (KeyModifiers::NONE, KeyCode::Home) => {
                if row_count > 0 {
                    self.active_tab_mut().selected_row = Some(0);
                    self.ensure_selected_row_visible();
                }
            }
            // End: jump to last row
            (KeyModifiers::NONE, KeyCode::End) => {
                if row_count > 0 {
                    self.active_tab_mut().selected_row = Some(row_count.saturating_sub(1));
                    self.ensure_selected_row_visible();
                }
            }
            // Enter: open detail view for selected row
            (KeyModifiers::NONE, KeyCode::Enter) => {
                if self.active_tab().selected_row.is_some() {
                    let row_index = self.active_tab().selected_row.unwrap();
                    self.popup = Some(Popup::EventDetail {
                        row_index,
                        scroll: 0,
                    });
                }
            }
            // Esc: close search → deselect row (cancel handled globally)
            (KeyModifiers::NONE, KeyCode::Esc) => {
                if self.results_search.is_some() {
                    self.results_search = None;
                } else {
                    let tab = self.active_tab_mut();
                    if tab.selected_row.is_some() {
                        tab.selected_row = None;
                    }
                }
            }
            // Ctrl+C: cancel running query (from results focus)
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                if matches!(self.active_tab().status, TabStatus::Running { .. }) {
                    self.cancel_query();
                }
            }
            // Horizontal scrolling
            (KeyModifiers::NONE, KeyCode::Left) => {
                let tab = self.active_tab_mut();
                tab.horizontal_scroll_offset = tab.horizontal_scroll_offset.saturating_sub(1);
            }
            (KeyModifiers::NONE, KeyCode::Right) => {
                let tab = self.active_tab_mut();
                tab.horizontal_scroll_offset = tab.horizontal_scroll_offset.saturating_add(1);
            }
            // Cycle chart view (only for timechart results)
            (KeyModifiers::NONE, KeyCode::Char('v')) => {
                let is_timechart = self
                    .active_tab()
                    .result
                    .as_ref()
                    .is_some_and(|r| ui::results::is_timechart_result(&r.result));
                if is_timechart {
                    let tab = self.active_tab_mut();
                    tab.chart_view = tab.chart_view.next();
                }
            }
            // Execute query: Ctrl+Enter
            (_, KeyCode::Enter) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.execute_query();
            }
            // Execute query: F5, Shift+Enter, Ctrl+J
            (KeyModifiers::NONE, KeyCode::F(5))
            | (KeyModifiers::SHIFT, KeyCode::Enter)
            | (KeyModifiers::CONTROL, KeyCode::Char('j')) => {
                self.execute_query();
            }
            _ => {}
        }
    }

    /// Handle keys in column mode (cursor on header row).
    fn handle_column_mode_key(&mut self, key: event::KeyEvent) {
        match (key.modifiers, key.code) {
            // Exit column mode
            (KeyModifiers::NONE, KeyCode::Esc) => {
                if let Some(ref mut config) = self.active_tab_mut().column_config {
                    config.selected = None;
                }
            }
            // Navigate left in display order
            (KeyModifiers::NONE, KeyCode::Left) => {
                if let Some(ref mut config) = self.active_tab_mut().column_config {
                    config.move_cursor_left();
                }
            }
            // Navigate right in display order
            (KeyModifiers::NONE, KeyCode::Right) => {
                if let Some(ref mut config) = self.active_tab_mut().column_config {
                    config.move_cursor_right();
                }
            }
            // Toggle pin
            (KeyModifiers::NONE, KeyCode::Char('p')) => {
                if let Some(ref mut config) = self.active_tab_mut().column_config {
                    config.toggle_pin_selected();
                }
            }
            // Hide column
            (KeyModifiers::NONE, KeyCode::Char('h')) => {
                if let Some(ref mut config) = self.active_tab_mut().column_config {
                    config.hide_selected();
                }
            }
            // Widen column
            (KeyModifiers::NONE, KeyCode::Char('+' | '=')) => {
                if let Some(ref mut config) = self.active_tab_mut().column_config {
                    config.adjust_selected_width(2);
                }
            }
            // Narrow column
            (KeyModifiers::NONE, KeyCode::Char('-')) => {
                if let Some(ref mut config) = self.active_tab_mut().column_config {
                    config.adjust_selected_width(-2);
                }
            }
            // Reset width to auto
            (KeyModifiers::NONE, KeyCode::Char('0')) => {
                if let Some(ref mut config) = self.active_tab_mut().column_config {
                    config.reset_selected_width();
                }
            }
            // Open column picker
            (KeyModifiers::SHIFT, KeyCode::Char('H')) => {
                if self.active_tab().column_config.is_some() {
                    self.popup = Some(Popup::ColumnPicker {
                        selected: 0,
                        scroll: 0,
                    });
                }
            }
            _ => {}
        }
    }

    /// Handle key events for the search input bar (when actively typing a search).
    fn handle_search_input_key(&mut self, key: event::KeyEvent) {
        match (key.modifiers, key.code) {
            // Esc: close search entirely
            (KeyModifiers::NONE, KeyCode::Esc) => {
                self.results_search = None;
            }
            // Enter: close input but keep highlights (search stays with input_active=false)
            (KeyModifiers::NONE, KeyCode::Enter) => {
                if let Some(ref mut search) = self.results_search {
                    search.input_active = false;
                    self.jump_to_current_match();
                }
            }
            // Backspace: delete last char
            (KeyModifiers::NONE, KeyCode::Backspace) => {
                if let Some(ref mut search) = self.results_search {
                    search.query.pop();
                }
                self.recompute_search_matches();
                self.jump_to_current_match();
            }
            // Type characters into search
            (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(ch)) => {
                if let Some(ref mut search) = self.results_search {
                    search.query.push(ch);
                }
                self.recompute_search_matches();
                self.jump_to_current_match();
            }
            _ => {}
        }
    }

    /// Recompute search matches against the active tab's result data.
    fn recompute_search_matches(&mut self) {
        if let Some(ref mut search) = self.results_search
            && let Some(ref response) = self.tab.result
        {
            search.update_matches(&response.result, self.tab.column_config.as_ref());
        }
    }

    /// Jump to the current search match: select its row and scroll to it.
    fn jump_to_current_match(&mut self) {
        if let Some(ref search) = self.results_search
            && let Some(&(row_idx, _col_idx)) = search.matches.get(search.current_match)
        {
            self.active_tab_mut().selected_row = Some(row_idx);
            self.ensure_selected_row_visible();
        }
    }

    /// Adjust scroll offset to keep the selected row visible.
    pub(crate) fn ensure_selected_row_visible(&mut self) {
        let tab = self.active_tab_mut();
        if let Some(selected) = tab.selected_row {
            let visible_rows = tab.last_visible_rows;
            if selected < tab.scroll_offset {
                tab.scroll_offset = selected;
            } else if selected >= tab.scroll_offset + visible_rows {
                tab.scroll_offset = selected - visible_rows + 1;
            }
        }
    }
}
