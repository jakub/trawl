// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! History, Saved, and Schema panel key handling.

use crossterm::event::{self, KeyCode, KeyModifiers};

use super::super::App;
use super::super::state::{Focus, MainTab, Popup, SimpleEditor};

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
    pub(crate) fn handle_panel_saved_key(&mut self, key: event::KeyEvent) {
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
            // Enter: load selected query into editor, switch to Query tab
            (KeyModifiers::NONE, KeyCode::Enter) => {
                let selected = self.panel.saved_selected;
                let query_text = self
                    .saved_cache
                    .as_ref()
                    .and_then(|s| s.queries.get(selected))
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
