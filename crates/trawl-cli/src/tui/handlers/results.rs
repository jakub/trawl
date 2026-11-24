// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crossterm::event::{self, KeyCode, KeyModifiers};

use super::super::App;
use super::super::state::{Focus, Popup, ResultsSearch};
use super::super::ui;

impl App {
    #[allow(clippy::too_many_lines)]
    pub(crate) fn handle_results_key(&mut self, key: event::KeyEvent) {
        // Ctrl+C cancels a running query from every results mode, so the
        // search bar and column mode cannot swallow it.
        if (key.modifiers, key.code) == (KeyModifiers::CONTROL, KeyCode::Char('c'))
            && self.active_tab().execution.is_batch()
        {
            self.cancel_query();
            return;
        }

        if self.results_search.as_ref().is_some_and(|s| s.input_active) {
            self.handle_search_input_key(key);
            return;
        }

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
            (KeyModifiers::NONE, KeyCode::Tab) => {
                self.focus = Focus::Editor;
            }
            (KeyModifiers::NONE, KeyCode::Char('/')) if self.active_tab().result.is_some() => {
                self.results_search = Some(ResultsSearch::new());
            }
            (KeyModifiers::NONE, KeyCode::Char('n')) => {
                if let Some(ref mut search) = self.results_search {
                    search.next_match();
                    self.jump_to_current_match();
                }
            }
            (KeyModifiers::SHIFT, KeyCode::Char('N')) => {
                if let Some(ref mut search) = self.results_search {
                    search.prev_match();
                    self.jump_to_current_match();
                }
            }
            (KeyModifiers::NONE, KeyCode::Char('c')) => {
                if let Some(ref mut config) = self.active_tab_mut().column_config
                    && config.visible_count() > 0
                {
                    let first = config.display_order().into_iter().next();
                    config.selected = first;
                }
            }
            (KeyModifiers::SHIFT, KeyCode::Char('H'))
                if self.active_tab().column_config.is_some() =>
            {
                self.popup = Some(Popup::ColumnPicker {
                    selected: 0,
                    scroll: 0,
                });
            }
            (KeyModifiers::NONE, KeyCode::Up) if row_count > 0 => {
                let tab = self.active_tab_mut();
                tab.selected_row = Some(match tab.selected_row {
                    Some(r) => r.saturating_sub(1),
                    None => 0,
                });
                self.ensure_selected_row_visible();
            }
            (KeyModifiers::NONE, KeyCode::Down) if row_count > 0 => {
                let tab = self.active_tab_mut();
                let max_row = row_count.saturating_sub(1);
                tab.selected_row = Some(match tab.selected_row {
                    Some(r) => (r + 1).min(max_row),
                    None => 0,
                });
                self.ensure_selected_row_visible();
            }
            (KeyModifiers::NONE, KeyCode::PageUp) if row_count > 0 => {
                let tab = self.active_tab_mut();
                let page = tab.last_visible_rows;
                tab.selected_row = Some(match tab.selected_row {
                    Some(r) => r.saturating_sub(page),
                    None => 0,
                });
                self.ensure_selected_row_visible();
            }
            (KeyModifiers::NONE, KeyCode::PageDown) if row_count > 0 => {
                let tab = self.active_tab_mut();
                let page = tab.last_visible_rows;
                let max_row = row_count.saturating_sub(1);
                tab.selected_row = Some(match tab.selected_row {
                    Some(r) => (r + page).min(max_row),
                    None => 0,
                });
                self.ensure_selected_row_visible();
            }
            (KeyModifiers::NONE, KeyCode::Home) if row_count > 0 => {
                self.active_tab_mut().selected_row = Some(0);
                self.ensure_selected_row_visible();
            }
            (KeyModifiers::NONE, KeyCode::End) if row_count > 0 => {
                self.active_tab_mut().selected_row = Some(row_count.saturating_sub(1));
                self.ensure_selected_row_visible();
            }
            (KeyModifiers::NONE, KeyCode::Enter) if self.active_tab().selected_row.is_some() => {
                let row_index = self.active_tab().selected_row.unwrap();
                self.popup = Some(Popup::EventDetail {
                    row_index,
                    scroll: 0,
                });
            }
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
            (KeyModifiers::NONE, KeyCode::Left) => {
                let tab = self.active_tab_mut();
                tab.horizontal_scroll_offset = tab.horizontal_scroll_offset.saturating_sub(1);
            }
            (KeyModifiers::NONE, KeyCode::Right) => {
                let tab = self.active_tab_mut();
                tab.horizontal_scroll_offset = tab.horizontal_scroll_offset.saturating_add(1);
            }
            (KeyModifiers::NONE, KeyCode::Char('v')) => {
                if let Some(ref r) = self.active_tab().result {
                    let timechart = trawl_api::display::is_timechart_result(&r.result);
                    let bar_chartable = ui::results::is_bar_chartable(&r.result);
                    if timechart || bar_chartable {
                        let tab = self.active_tab_mut();
                        tab.chart_view = tab.chart_view.next_for(timechart, bar_chartable);
                    }
                }
            }
            (_, KeyCode::Enter) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.execute_query();
            }
            (KeyModifiers::NONE, KeyCode::F(5))
            | (KeyModifiers::SHIFT, KeyCode::Enter)
            | (KeyModifiers::CONTROL, KeyCode::Char('j')) => {
                self.execute_query();
            }
            _ => {}
        }
    }

    fn handle_column_mode_key(&mut self, key: event::KeyEvent) {
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Esc | KeyCode::Char('c')) => {
                if let Some(ref mut config) = self.active_tab_mut().column_config {
                    config.selected = None;
                }
            }
            (KeyModifiers::NONE, KeyCode::Left) => {
                if let Some(ref mut config) = self.active_tab_mut().column_config {
                    config.move_cursor_left();
                }
            }
            (KeyModifiers::NONE, KeyCode::Right) => {
                if let Some(ref mut config) = self.active_tab_mut().column_config {
                    config.move_cursor_right();
                }
            }
            (KeyModifiers::NONE, KeyCode::Char('p')) => {
                if let Some(ref mut config) = self.active_tab_mut().column_config {
                    config.toggle_pin_selected();
                }
            }
            (KeyModifiers::NONE, KeyCode::Char('h')) => {
                if let Some(ref mut config) = self.active_tab_mut().column_config {
                    config.hide_selected();
                }
            }
            (KeyModifiers::NONE, KeyCode::Char('+' | '=')) => {
                if let Some(ref mut config) = self.active_tab_mut().column_config {
                    config.adjust_selected_width(2);
                }
            }
            (KeyModifiers::NONE, KeyCode::Char('-')) => {
                if let Some(ref mut config) = self.active_tab_mut().column_config {
                    config.adjust_selected_width(-2);
                }
            }
            (KeyModifiers::NONE, KeyCode::Char('0')) => {
                if let Some(ref mut config) = self.active_tab_mut().column_config {
                    config.reset_selected_width();
                }
            }
            (KeyModifiers::SHIFT, KeyCode::Char('H'))
                if self.active_tab().column_config.is_some() =>
            {
                self.popup = Some(Popup::ColumnPicker {
                    selected: 0,
                    scroll: 0,
                });
            }
            _ => {}
        }
    }

    fn handle_search_input_key(&mut self, key: event::KeyEvent) {
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Esc) => {
                self.results_search = None;
            }
            (KeyModifiers::NONE, KeyCode::Enter) => {
                if let Some(ref mut search) = self.results_search {
                    search.input_active = false;
                    self.jump_to_current_match();
                }
            }
            (KeyModifiers::NONE, KeyCode::Backspace) => {
                if let Some(ref mut search) = self.results_search {
                    search.query.pop();
                }
                self.recompute_search_matches();
                self.jump_to_current_match();
            }
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

    fn recompute_search_matches(&mut self) {
        if let Some(ref mut search) = self.results_search
            && let Some(ref response) = self.tab.result
        {
            search.update_matches(
                &response.result,
                self.tab.column_config.as_ref(),
                self.display_zone,
            );
        }
    }

    fn jump_to_current_match(&mut self) {
        if let Some(ref search) = self.results_search
            && let Some(&(row_idx, _col_idx)) = search.matches.get(search.current_match)
        {
            self.active_tab_mut().selected_row = Some(row_idx);
            self.ensure_selected_row_visible();
        }
    }

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
