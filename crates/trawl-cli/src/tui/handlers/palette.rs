// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Command palette key handling and action dispatch.

use crossterm::event::{self, KeyCode, KeyModifiers};

use super::super::App;
use super::super::palette::{ActionKind, PaletteAction, refilter};
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
                self.popup = None;
                if let Some(action) = action {
                    self.execute_palette_action(action);
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
                if let Some(Popup::CommandPalette {
                    ref mut input,
                    ref mut cursor,
                    ref mut selected,
                    ref mut scroll,
                    ref items,
                    ref mut filtered,
                }) = self.popup
                {
                    input.clear();
                    *cursor = 0;
                    *selected = 0;
                    *scroll = 0;
                    *filtered = refilter("", items);
                }
            }
            // Delete last character.
            (KeyModifiers::NONE, KeyCode::Backspace) => {
                if let Some(Popup::CommandPalette {
                    ref mut input,
                    ref mut cursor,
                    ref mut selected,
                    ref mut scroll,
                    ref items,
                    ref mut filtered,
                }) = self.popup
                    && *cursor > 0
                {
                    let byte_pos = input.char_indices().nth(*cursor - 1).map_or(0, |(i, _)| i);
                    let next_byte = input
                        .char_indices()
                        .nth(*cursor)
                        .map_or(input.len(), |(i, _)| i);
                    input.replace_range(byte_pos..next_byte, "");
                    *cursor -= 1;
                    *selected = 0;
                    *scroll = 0;
                    *filtered = refilter(input, items);
                }
            }
            // Type a character into the filter.
            (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(c)) => {
                if let Some(Popup::CommandPalette {
                    ref mut input,
                    ref mut cursor,
                    ref mut selected,
                    ref mut scroll,
                    ref items,
                    ref mut filtered,
                }) = self.popup
                {
                    let byte_pos = input
                        .char_indices()
                        .nth(*cursor)
                        .map_or(input.len(), |(i, _)| i);
                    input.insert(byte_pos, c);
                    *cursor += 1;
                    *selected = 0;
                    *scroll = 0;
                    *filtered = refilter(input, items);
                }
            }
            _ => {}
        }
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
