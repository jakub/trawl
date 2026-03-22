// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! History, Saved, and Schema panel key handling.

use crossterm::event::{self, KeyCode, KeyModifiers};

use super::super::App;
use super::super::state::{Focus, MainTab, Popup, SavedFocus, SimpleEditor};

impl App {
    /// Handle key events for the History tab panel.
    pub(crate) fn handle_panel_history_key(&mut self, key: event::KeyEvent) {
        let item_count = self.history_cache.as_ref().map_or(0, |h| h.entries.len());

        match (key.modifiers, key.code) {
            // Tab: switch back to Query tab
            (KeyModifiers::NONE, KeyCode::Tab) => {
                self.switch_to_main_tab(MainTab::Query);
            }
            (KeyModifiers::NONE, KeyCode::Up) => {
                self.panel.history_selected = self.panel.history_selected.saturating_sub(1);
            }
            (KeyModifiers::NONE, KeyCode::Down) => {
                if item_count > 0 {
                    self.panel.history_selected =
                        (self.panel.history_selected + 1).min(item_count - 1);
                }
            }
            // Enter: load selected query into editor, switch to Query tab
            (KeyModifiers::NONE, KeyCode::Enter) => {
                let selected = self.panel.history_selected;
                let query_text = self
                    .history_cache
                    .as_ref()
                    .and_then(|h| h.entries.get(selected))
                    .map(|entry| entry.query.clone());

                if let Some(query) = query_text {
                    let editor = &mut self.tab.editor;
                    editor.clear();
                    editor.insert_text(&query);
                    editor.move_to_line_end();
                    self.switch_to_main_tab(MainTab::Query);
                    self.focus = Focus::Editor;
                }
            }
            _ => {}
        }
    }

    /// Handle key events for the Saved tab panel.
    ///
    /// Dispatches to sub-handlers based on the current `SavedFocus`.
    pub(crate) fn handle_panel_saved_key(&mut self, key: event::KeyEvent) {
        match self.panel.saved_focus {
            SavedFocus::List => self.handle_saved_list_key(key),
            SavedFocus::Detail => self.handle_saved_detail_key(key),
            SavedFocus::RunResults => self.handle_saved_run_results_key(key),
        }
    }

    /// Key handling for `SavedFocus::List` — the left-pane saved query list.
    fn handle_saved_list_key(&mut self, key: event::KeyEvent) {
        let item_count = self.saved_cache.as_ref().map_or(0, |s| s.queries.len());

        match (key.modifiers, key.code) {
            // Tab: switch back to Query tab
            (KeyModifiers::NONE, KeyCode::Tab) => {
                self.switch_to_main_tab(MainTab::Query);
            }
            (KeyModifiers::NONE, KeyCode::Up) => {
                self.panel.saved_selected = self.panel.saved_selected.saturating_sub(1);
            }
            (KeyModifiers::NONE, KeyCode::Down) => {
                if item_count > 0 {
                    self.panel.saved_selected = (self.panel.saved_selected + 1).min(item_count - 1);
                }
            }
            // Right / Enter: open detail pane with run history
            (KeyModifiers::NONE, KeyCode::Enter | KeyCode::Right) => {
                let selected = self.panel.saved_selected;
                if let Some(ref saved) = self.saved_cache
                    && let Some(entry) = saved.queries.get(selected)
                {
                    // Mark loading and fetch runs
                    self.panel.saved_detail = Some(super::super::state::SavedDetailState {
                        saved_id: entry.id,
                        runs: Vec::new(),
                        total_runs: 0,
                        run_selected: 0,
                        run_scroll: 0,
                        result: None,
                        result_scroll: 0,
                        loading: true,
                    });
                    self.panel.saved_focus = SavedFocus::Detail;
                    self.fetch_runs(entry.id);
                }
            }
            // Delete: confirm deletion
            (KeyModifiers::NONE, KeyCode::Delete | KeyCode::Backspace) => {
                let selected = self.panel.saved_selected;
                if let Some(ref saved) = self.saved_cache
                    && let Some(entry) = saved.queries.get(selected)
                {
                    self.popup = Some(Popup::ConfirmDelete {
                        saved_id: entry.id,
                        name: entry.name.clone(),
                    });
                }
            }
            // Schedule: open set-schedule popup
            (KeyModifiers::NONE, KeyCode::Char('s')) => {
                let selected = self.panel.saved_selected;
                if let Some(ref saved) = self.saved_cache
                    && let Some(entry) = saved.queries.get(selected)
                {
                    self.popup = Some(Popup::SetSchedule {
                        saved_id: entry.id,
                        name: entry.name.clone(),
                        editor: SimpleEditor::new(),
                    });
                }
            }
            _ => {}
        }
    }

    /// Key handling for `SavedFocus::Detail` — the right-pane run history list.
    fn handle_saved_detail_key(&mut self, key: event::KeyEvent) {
        let run_count = self.panel.saved_detail.as_ref().map_or(0, |d| d.runs.len());

        match (key.modifiers, key.code) {
            // Tab: switch back to Query tab
            (KeyModifiers::NONE, KeyCode::Tab) => {
                self.switch_to_main_tab(MainTab::Query);
            }
            (KeyModifiers::NONE, KeyCode::Up) => {
                if let Some(ref mut detail) = self.panel.saved_detail {
                    detail.run_selected = detail.run_selected.saturating_sub(1);
                }
            }
            (KeyModifiers::NONE, KeyCode::Down) => {
                if run_count > 0
                    && let Some(ref mut detail) = self.panel.saved_detail
                {
                    detail.run_selected = (detail.run_selected + 1).min(run_count - 1);
                }
            }
            // Left: back to list
            (KeyModifiers::NONE, KeyCode::Left) => {
                self.panel.saved_detail = None;
                self.panel.saved_focus = SavedFocus::List;
            }
            // Enter: load run result
            (KeyModifiers::NONE, KeyCode::Enter) => {
                if let Some(ref detail) = self.panel.saved_detail
                    && let Some(run) = detail.runs.get(detail.run_selected)
                {
                    let saved_id = detail.saved_id;
                    let run_id = run.id;
                    self.fetch_run_result(saved_id, run_id);
                }
            }
            // q: pre-fill editor with `| from saved` reference
            (KeyModifiers::NONE, KeyCode::Char('q')) => {
                self.prefill_from_saved_run();
            }
            _ => {}
        }
    }

    /// Key handling for `SavedFocus::RunResults` — viewing a run's result table.
    fn handle_saved_run_results_key(&mut self, key: event::KeyEvent) {
        match (key.modifiers, key.code) {
            // Tab: switch back to Query tab
            (KeyModifiers::NONE, KeyCode::Tab) => {
                self.switch_to_main_tab(MainTab::Query);
            }
            (KeyModifiers::NONE, KeyCode::Up) => {
                if let Some(ref mut detail) = self.panel.saved_detail {
                    detail.result_scroll = detail.result_scroll.saturating_sub(1);
                }
            }
            (KeyModifiers::NONE, KeyCode::Down) => {
                if let Some(ref mut detail) = self.panel.saved_detail {
                    let row_count = detail.result.as_ref().map_or(0, |r| r.rows.len());
                    if row_count > 0 {
                        detail.result_scroll =
                            (detail.result_scroll + 1).min(row_count.saturating_sub(1));
                    }
                }
            }
            // Left: back to detail
            (KeyModifiers::NONE, KeyCode::Left) => {
                if let Some(ref mut detail) = self.panel.saved_detail {
                    detail.result = None;
                    detail.result_scroll = 0;
                }
                self.panel.saved_focus = SavedFocus::Detail;
            }
            // q: pre-fill editor with `| from saved` reference
            (KeyModifiers::NONE, KeyCode::Char('q')) => {
                self.prefill_from_saved_run();
            }
            _ => {}
        }
    }

    /// Pre-fill the editor with `| from saved {name} run={id}` and switch to Query tab.
    fn prefill_from_saved_run(&mut self) {
        let selected_idx = self.panel.saved_selected;
        let Some(ref saved) = self.saved_cache else {
            return;
        };
        let Some(entry) = saved.queries.get(selected_idx) else {
            return;
        };
        let name = &entry.name;

        // Determine the run ID from the current focus context.
        let run_id = match self.panel.saved_focus {
            SavedFocus::RunResults | SavedFocus::Detail => self
                .panel
                .saved_detail
                .as_ref()
                .and_then(|d| d.runs.get(d.run_selected))
                .map(|r| r.id),
            SavedFocus::List => None,
        };

        let query_text = if let Some(rid) = run_id {
            format!("| from saved \"{name}\" run={rid}")
        } else {
            format!("| from saved \"{name}\"")
        };

        let editor = &mut self.tab.editor;
        editor.clear();
        editor.insert_text(&query_text);
        editor.move_to_line_end();

        // Reset saved tab state
        self.panel.saved_detail = None;
        self.panel.saved_focus = SavedFocus::List;

        self.switch_to_main_tab(MainTab::Query);
        self.focus = Focus::Editor;
    }

    /// Handle key events for the Schema tab panel.
    pub(crate) fn handle_panel_schema_key(&mut self, key: event::KeyEvent) {
        let Some(schema) = self.panel.schema.as_ref() else {
            return;
        };

        // Filter input mode captures all keys.
        if schema.filter_active {
            self.handle_schema_filter_key(key);
            return;
        }

        match (key.modifiers, key.code) {
            // Tab: switch back to Query tab
            (KeyModifiers::NONE, KeyCode::Tab) => {
                self.switch_to_main_tab(MainTab::Query);
            }
            // '/' or Ctrl+P: activate filter
            (KeyModifiers::NONE, KeyCode::Char('/'))
            | (KeyModifiers::CONTROL, KeyCode::Char('p')) => {
                if let Some(s) = self.panel.schema.as_mut() {
                    s.filter_active = true;
                }
            }
            // Delegate to tree navigation
            _ => self.handle_schema_tree_key(key),
        }
    }

    /// Handle key events for the schema filter input.
    fn handle_schema_filter_key(&mut self, key: event::KeyEvent) {
        let Some(schema) = self.panel.schema.as_mut() else {
            return;
        };
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Esc) => {
                schema.filter_active = false;
                schema.filter.clear();
                schema.selected = 0;
                schema.scroll = 0;
            }
            (KeyModifiers::NONE, KeyCode::Enter) => {
                schema.filter_active = false;
            }
            (KeyModifiers::NONE, KeyCode::Backspace) => {
                schema.filter.pop();
                schema.selected = 0;
                schema.scroll = 0;
            }
            (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(c)) => {
                schema.filter.push(c);
                schema.selected = 0;
                schema.scroll = 0;
            }
            _ => {}
        }
    }
}
