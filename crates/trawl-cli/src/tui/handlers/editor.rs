// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Editor key handling — query input, ghost-text autocomplete, clipboard ops.

use crossterm::event::{self, KeyCode, KeyModifiers};

use super::super::autocomplete;
use super::super::state::{Focus, TabStatus};
use super::super::{App, clipboard_get, clipboard_set};

impl App {
    /// Recompute ghost-text autocomplete for the current editor state.
    pub(crate) fn update_ghost_text(&mut self) {
        let schema_fields: Vec<autocomplete::SchemaField> = self
            .schema_cache
            .as_ref()
            .map(|s| {
                s.columns
                    .iter()
                    .map(|c| {
                        let is_numeric = matches!(
                            c.data_type.to_uppercase().as_str(),
                            "INTEGER"
                                | "BIGINT"
                                | "SMALLINT"
                                | "TINYINT"
                                | "HUGEINT"
                                | "FLOAT"
                                | "DOUBLE"
                                | "DECIMAL"
                                | "UBIGINT"
                                | "UINTEGER"
                                | "USMALLINT"
                                | "UTINYINT"
                        );
                        autocomplete::SchemaField {
                            name: c.name.clone(),
                            is_numeric,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        let tab = &self.tab;
        // Don't show ghost text when there's an active selection.
        if tab.editor.selection_anchor.is_some() {
            self.tab.ghost = None;
            return;
        }

        let (row, col) = tab.editor.cursor;
        let cursor_byte = autocomplete::cursor_to_byte_offset(&tab.editor.lines, row, col);
        let text = tab.editor.text();

        self.tab.ghost = autocomplete::complete(&text, cursor_byte, &schema_fields);
    }

    /// Accept the current ghost-text completion.
    pub(crate) fn accept_ghost_completion(&mut self) {
        if let Some(ghost) = self.tab.ghost.take() {
            self.tab.editor.replace_at_cursor(
                ghost.replace_len,
                &ghost.insert_text,
                ghost.cursor_offset,
            );
            self.tab.mark_editor_dirty();
            // Recompute ghost for the new editor state.
            self.update_ghost_text();
        }
    }

    /// Handle key events when editor is focused.
    #[allow(clippy::too_many_lines)] // Inherently large key dispatch
    pub(crate) fn handle_editor_key(&mut self, key: event::KeyEvent) {
        // Debug: log the key event to see what we're receiving
        tracing::debug!(
            "editor key: modifiers={:?}, code={:?}",
            key.modifiers,
            key.code
        );

        match (key.modifiers, key.code) {
            // Tab: accept ghost completion if active, otherwise switch to results.
            (KeyModifiers::NONE, KeyCode::Tab) => {
                if self.tab.ghost.is_some() {
                    self.accept_ghost_completion();
                    return;
                }
                self.focus = Focus::Results;
            }
            // Esc: dismiss ghost text and clear selection.
            (KeyModifiers::NONE, KeyCode::Esc) => {
                self.tab.ghost = None;
                self.tab.editor.clear_selection();
            }
            // Execute query: F5
            (KeyModifiers::NONE, KeyCode::F(5)) => {
                tracing::info!("executing query with F5");
                self.execute_query();
            }
            // Execute query: Ctrl+Enter
            (_, KeyCode::Enter) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                tracing::info!("executing query with ctrl+enter");
                self.execute_query();
            }
            // Execute query: Shift+Enter (or iTerm2's Ctrl+J)
            (KeyModifiers::SHIFT, KeyCode::Enter) | (KeyModifiers::CONTROL, KeyCode::Char('j')) => {
                tracing::info!("executing query with shift+enter");
                self.execute_query();
            }
            // Clear editor: Ctrl+L
            (KeyModifiers::CONTROL, KeyCode::Char('l')) => {
                self.active_tab_mut().clear();
                self.active_tab_mut().mark_editor_dirty();
            }
            // Format query: Ctrl+F
            (KeyModifiers::CONTROL, KeyCode::Char('f')) => {
                self.format_editor_query();
            }
            // Readline: Ctrl+A → line start
            (KeyModifiers::CONTROL, KeyCode::Char('a')) => {
                let editor = &mut self.active_tab_mut().editor;
                editor.clear_selection();
                editor.move_to_line_start();
            }
            // Readline: Ctrl+E → line end
            (KeyModifiers::CONTROL, KeyCode::Char('e')) => {
                let editor = &mut self.active_tab_mut().editor;
                editor.clear_selection();
                editor.move_to_line_end();
            }
            // Word movement: Ctrl+Left / Alt+Left / Alt+B (readline / macOS / linux)
            (KeyModifiers::CONTROL | KeyModifiers::ALT, KeyCode::Left)
            | (KeyModifiers::ALT, KeyCode::Char('b')) => {
                let editor = &mut self.active_tab_mut().editor;
                editor.clear_selection();
                editor.move_word_left();
            }
            // Word movement: Ctrl+Right / Alt+Right / Alt+F (readline / macOS / linux)
            (KeyModifiers::CONTROL | KeyModifiers::ALT, KeyCode::Right)
            | (KeyModifiers::ALT, KeyCode::Char('f')) => {
                let editor = &mut self.active_tab_mut().editor;
                editor.clear_selection();
                editor.move_word_right();
            }
            // Selection: Shift+Arrow (Shift+Ctrl or Shift+Alt = select word)
            (_, KeyCode::Left) if key.modifiers.contains(KeyModifiers::SHIFT) => {
                let editor = &mut self.active_tab_mut().editor;
                editor.start_selection();
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    || key.modifiers.contains(KeyModifiers::ALT)
                {
                    editor.move_word_left();
                } else {
                    editor.move_left();
                }
            }
            (_, KeyCode::Right) if key.modifiers.contains(KeyModifiers::SHIFT) => {
                let editor = &mut self.active_tab_mut().editor;
                editor.start_selection();
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    || key.modifiers.contains(KeyModifiers::ALT)
                {
                    editor.move_word_right();
                } else {
                    editor.move_right();
                }
            }
            (KeyModifiers::SHIFT, KeyCode::Up) => {
                let editor = &mut self.active_tab_mut().editor;
                editor.start_selection();
                editor.move_up();
            }
            (KeyModifiers::SHIFT, KeyCode::Down) => {
                let editor = &mut self.active_tab_mut().editor;
                editor.start_selection();
                editor.move_down();
            }
            (KeyModifiers::SHIFT, KeyCode::Home) => {
                let editor = &mut self.active_tab_mut().editor;
                editor.start_selection();
                editor.move_to_line_start();
            }
            (KeyModifiers::SHIFT, KeyCode::End) => {
                let editor = &mut self.active_tab_mut().editor;
                editor.start_selection();
                editor.move_to_line_end();
            }
            // Undo: Ctrl+Z
            (KeyModifiers::CONTROL, KeyCode::Char('z')) => {
                self.active_tab_mut().editor.undo();
                self.active_tab_mut().mark_editor_dirty();
            }
            // Redo: Ctrl+Y / Ctrl+Shift+Z
            (KeyModifiers::CONTROL, KeyCode::Char('y')) => {
                self.active_tab_mut().editor.redo();
                self.active_tab_mut().mark_editor_dirty();
            }
            (_, KeyCode::Char('Z'))
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.active_tab_mut().editor.redo();
                self.active_tab_mut().mark_editor_dirty();
            }
            // Clipboard: Ctrl+C (copy), or cancel running query if no selection
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                if let Some(text) = self.active_tab().editor.selected_text() {
                    clipboard_set(&text);
                } else if matches!(self.active_tab().status, TabStatus::Running { .. }) {
                    self.cancel_query();
                }
            }
            (KeyModifiers::CONTROL, KeyCode::Char('x')) => {
                if let Some(text) = self.active_tab().editor.selected_text() {
                    clipboard_set(&text);
                    self.active_tab_mut().editor.delete_selection();
                    self.active_tab_mut().mark_editor_dirty();
                }
            }
            // Clipboard: Ctrl+V (paste)
            (KeyModifiers::CONTROL, KeyCode::Char('v')) => {
                if let Some(text) = clipboard_get() {
                    let tab = self.active_tab_mut();
                    tab.editor.delete_selection();
                    tab.editor.insert_text(&text);
                    tab.mark_editor_dirty();
                }
            }
            // Kill word before cursor: Ctrl+W / Ctrl+Backspace
            (KeyModifiers::CONTROL, KeyCode::Char('w') | KeyCode::Backspace) => {
                self.active_tab_mut().editor.delete_word_before();
                self.active_tab_mut().mark_editor_dirty();
            }
            // Kill word after cursor: Ctrl+Delete / Alt+D
            (KeyModifiers::CONTROL, KeyCode::Delete) | (KeyModifiers::ALT, KeyCode::Char('d')) => {
                self.active_tab_mut().editor.delete_word_after();
                self.active_tab_mut().mark_editor_dirty();
            }
            // Kill to line start: Ctrl+U
            (KeyModifiers::CONTROL, KeyCode::Char('u')) => {
                self.active_tab_mut().editor.delete_to_line_start();
                self.active_tab_mut().mark_editor_dirty();
            }
            // Kill to line end: Ctrl+K
            (KeyModifiers::CONTROL, KeyCode::Char('k')) => {
                self.active_tab_mut().editor.delete_to_line_end();
                self.active_tab_mut().mark_editor_dirty();
            }
            // Handle text editing.
            _ => {
                let tab = self.active_tab_mut();
                let editor = &mut tab.editor;
                let text_modified = match key.code {
                    KeyCode::Char(ch) => {
                        editor.insert_char(ch);
                        true
                    }
                    KeyCode::Enter => {
                        editor.insert_newline();
                        true
                    }
                    KeyCode::Backspace => {
                        editor.delete_char_before();
                        true
                    }
                    KeyCode::Delete => {
                        editor.delete_char_at();
                        true
                    }
                    KeyCode::Left => {
                        editor.clear_selection();
                        editor.move_left();
                        false
                    }
                    KeyCode::Right => {
                        editor.clear_selection();
                        editor.move_right();
                        false
                    }
                    KeyCode::Up => {
                        editor.clear_selection();
                        editor.move_up();
                        false
                    }
                    KeyCode::Down => {
                        editor.clear_selection();
                        editor.move_down();
                        false
                    }
                    KeyCode::Home => {
                        editor.clear_selection();
                        editor.move_to_line_start();
                        false
                    }
                    KeyCode::End => {
                        editor.clear_selection();
                        editor.move_to_line_end();
                        false
                    }
                    _ => false,
                };
                if text_modified {
                    tab.mark_editor_dirty();
                }
            }
        }

        // Recompute ghost text after every editor keypress.
        self.update_ghost_text();
    }
}
