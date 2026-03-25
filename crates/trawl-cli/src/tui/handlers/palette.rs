// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Command palette key handling and action dispatch.

use crossterm::event::{self, KeyCode, KeyModifiers};

use super::super::App;
use super::super::palette::{
    ActionKind, PaletteAction, build_palette_items, build_service_column_items, compute_ghost_text,
    refilter,
};
use super::super::state::{Focus, MainTab, Popup, SimpleEditor};
use super::super::ui;

impl App {
    /// Handle key events when the command palette is open.
    #[allow(clippy::too_many_lines)] // Palette key dispatch requires many destructured arms
    pub(crate) fn handle_command_palette_key(&mut self, key: event::KeyEvent) {
        match (key.modifiers, key.code) {
            // Close palette.
            (KeyModifiers::NONE, KeyCode::Esc) | (KeyModifiers::CONTROL, KeyCode::Char('p')) => {
                self.popup = None;
            }
            // Execute selected item.
            (KeyModifiers::NONE, KeyCode::Enter) => {
                let action = {
                    let Some(Popup::CommandPalette {
                        ref filtered,
                        ref items,
                        selected,
                        ..
                    }) = self.popup
                    else {
                        return;
                    };
                    filtered
                        .get(selected)
                        .map(|f| items[f.item_index].action.clone())
                };
                if let Some(PaletteAction::DrillService(ref name)) = action {
                    // Drill into service columns — don't close the palette.
                    let new_input = format!("{name}.");
                    self.set_palette_input(&new_input);
                } else {
                    self.popup = None;
                    if let Some(action) = action {
                        self.execute_palette_action(action);
                    }
                }
            }
            // Accept ghost text completion.
            (KeyModifiers::NONE, KeyCode::Tab) => {
                let suffix = {
                    let Some(Popup::CommandPalette { ref ghost, .. }) = self.popup else {
                        return;
                    };
                    ghost.clone()
                };
                if let Some(suffix) = suffix {
                    if let Some(Popup::CommandPalette {
                        ref mut input,
                        ref mut cursor,
                        ..
                    }) = self.popup
                    {
                        input.push_str(&suffix);
                        *cursor = input.len();
                    }
                    self.refresh_palette_state();
                }
            }
            // Move selection up.
            (KeyModifiers::NONE, KeyCode::Up) => {
                if let Some(Popup::CommandPalette {
                    ref mut selected, ..
                }) = self.popup
                {
                    *selected = selected.saturating_sub(1);
                }
            }
            // Move selection down.
            (KeyModifiers::NONE, KeyCode::Down) => {
                if let Some(Popup::CommandPalette {
                    ref mut selected,
                    ref filtered,
                    ..
                }) = self.popup
                {
                    let max = filtered.len().saturating_sub(1);
                    *selected = (*selected + 1).min(max);
                }
            }
            // Clear filter text.
            (KeyModifiers::CONTROL, KeyCode::Char('u')) => {
                self.set_palette_input("");
            }
            // Delete last character.
            (KeyModifiers::NONE, KeyCode::Backspace) => {
                let new_input = {
                    let Some(Popup::CommandPalette {
                        ref input, cursor, ..
                    }) = self.popup
                    else {
                        return;
                    };
                    if cursor == 0 {
                        return;
                    }
                    let byte_pos = input.char_indices().nth(cursor - 1).map_or(0, |(i, _)| i);
                    let next_byte = input
                        .char_indices()
                        .nth(cursor)
                        .map_or(input.len(), |(i, _)| i);
                    let mut new = input.clone();
                    new.replace_range(byte_pos..next_byte, "");
                    new
                };
                self.set_palette_input(&new_input);
            }
            // Type a character into the filter.
            (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(c)) => {
                let new_input = {
                    let Some(Popup::CommandPalette {
                        ref input, cursor, ..
                    }) = self.popup
                    else {
                        return;
                    };
                    let byte_pos = input
                        .char_indices()
                        .nth(cursor)
                        .map_or(input.len(), |(i, _)| i);
                    let mut new = input.clone();
                    new.insert(byte_pos, c);
                    new
                };
                self.set_palette_input(&new_input);
            }
            _ => {}
        }
    }

    /// Set the palette input text and refresh all derived state.
    ///
    /// This is the single entry point for all input mutations — it handles
    /// dot-aware item rebuilding, refiltering, and ghost text computation.
    fn set_palette_input(&mut self, new_input: &str) {
        // Determine if we're in "service columns" mode (input contains a dot).
        let (items, filter_text) = if let Some(dot_pos) = new_input.find('.') {
            let service_prefix = &new_input[..dot_pos];
            let after_dot = &new_input[dot_pos + 1..];
            if let Some(ref schema) = self.panel.schema {
                let items = build_service_column_items(schema, service_prefix);
                (items, after_dot.to_string())
            } else {
                (Vec::new(), after_dot.to_string())
            }
        } else {
            // Full catalog mode.
            let items = build_palette_items(
                self.dashboard.is_admin,
                self.saved_cache.as_ref(),
                self.history_cache.as_ref(),
                self.panel.schema.as_ref(),
            );
            (items, new_input.to_string())
        };

        let filtered = refilter(&filter_text, &items);
        let ghost = compute_ghost_text(new_input, &items, &filtered);

        if let Some(Popup::CommandPalette {
            input: ref mut inp,
            ref mut cursor,
            ref mut selected,
            ref mut scroll,
            items: ref mut cur_items,
            filtered: ref mut cur_filtered,
            ghost: ref mut cur_ghost,
        }) = self.popup
        {
            *inp = new_input.to_string();
            *cursor = new_input.len();
            *selected = 0;
            *scroll = 0;
            *cur_items = items;
            *cur_filtered = filtered;
            *cur_ghost = ghost;
        }
    }

    /// Refresh palette items, filter, and ghost text from the current input.
    ///
    /// Called after Tab (ghost text acceptance) to recompute derived state
    /// without changing the input text.
    fn refresh_palette_state(&mut self) {
        let input = {
            let Some(Popup::CommandPalette { ref input, .. }) = self.popup else {
                return;
            };
            input.clone()
        };
        self.set_palette_input(&input);
    }

    /// Dispatch a palette action — called after the palette closes.
    fn execute_palette_action(&mut self, action: PaletteAction) {
        match action {
            PaletteAction::SwitchTab(tab) => {
                self.switch_to_main_tab(tab);
            }
            PaletteAction::LoadQuery(query) => {
                self.tab.editor.clear();
                self.tab.editor.insert_text(&query);
                self.tab.editor.move_to_line_end();
                self.switch_to_main_tab(MainTab::Query);
                self.focus = Focus::Editor;
            }
            PaletteAction::InsertAtCursor(text) => {
                self.tab.editor.insert_text(&text);
                self.switch_to_main_tab(MainTab::Query);
                self.focus = Focus::Editor;
            }
            PaletteAction::RunAction(kind) => {
                self.execute_palette_builtin(kind);
            }
            PaletteAction::DrillService(_) => {
                // Handled inline in Enter key dispatch (doesn't close palette).
                // Should not reach here.
            }
        }
    }

    /// Execute a built-in action by kind.
    fn execute_palette_builtin(&mut self, kind: ActionKind) {
        match kind {
            ActionKind::ExecuteQuery => {
                let query = self.tab.editor.text();
                if !query.trim().is_empty() {
                    self.execute_query();
                }
            }
            ActionKind::SaveQuery => {
                let query = self.tab.editor.text();
                if !query.trim().is_empty() {
                    self.popup = Some(Popup::SaveQuery {
                        editor: SimpleEditor::new_single_line(),
                    });
                }
            }
            ActionKind::FormatQuery => {
                self.format_editor_query();
            }
            ActionKind::ClearEditor => {
                self.tab.clear();
            }
            ActionKind::ToggleLiveMode => {
                self.toggle_live_mode();
            }
            ActionKind::ToggleHelp => {
                self.popup = Some(Popup::Help { scroll: 0 });
            }
            ActionKind::Quit => {
                self.should_quit = true;
            }
            ActionKind::CycleChartView => {
                if let Some(ref r) = self.tab.result {
                    let timechart = ui::results::is_timechart_result(&r.result);
                    let bar_chartable = ui::results::is_bar_chartable(&r.result);
                    if timechart || bar_chartable {
                        self.tab.chart_view =
                            self.tab.chart_view.next_for(timechart, bar_chartable);
                    }
                }
            }
            ActionKind::OpenColumnPicker => {
                self.popup = Some(Popup::ColumnPicker {
                    selected: 0,
                    scroll: 0,
                });
            }
            ActionKind::EnterColumnMode => {
                if let Some(ref mut config) = self.tab.column_config
                    && config.visible_count() > 0
                {
                    let first = config.display_order().into_iter().next();
                    config.selected = first;
                    self.switch_to_main_tab(MainTab::Query);
                    self.focus = Focus::Results;
                }
            }
        }
    }
}
