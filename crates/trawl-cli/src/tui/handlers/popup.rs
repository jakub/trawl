// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Popup key handling — help, save query, confirm delete, error, schedule, event detail.

use crossterm::event::{self, KeyCode, KeyModifiers};

use super::super::state::Popup;
use super::super::{App, MutationResult};

/// Check if a saved query name matches `[a-zA-Z0-9_-]+`.
///
/// Mirrors the server-side validation in `trawl-auth` for immediate
/// client-side feedback (the server still validates independently).
fn is_valid_query_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

impl App {
    /// Handle key events when a popup is open.
    #[allow(clippy::too_many_lines)] // Inherently large popup dispatch
    pub(crate) fn handle_popup_key(&mut self, key: event::KeyEvent) {
        // Help popup needs mutable access to scroll — handle before immutable borrow.
        if let Some(Popup::Help { ref mut scroll }) = self.popup {
            match (key.modifiers, key.code) {
                (KeyModifiers::NONE, KeyCode::Esc | KeyCode::Char('q') | KeyCode::F(1)) => {
                    self.popup = None;
                }
                (KeyModifiers::NONE, KeyCode::Up) => {
                    *scroll = scroll.saturating_sub(1);
                }
                (KeyModifiers::NONE, KeyCode::Down) => {
                    *scroll += 1;
                }
                (KeyModifiers::NONE, KeyCode::PageUp) => {
                    *scroll = scroll.saturating_sub(10);
                }
                (KeyModifiers::NONE, KeyCode::PageDown) => {
                    *scroll += 10;
                }
                (KeyModifiers::NONE, KeyCode::Home) => {
                    *scroll = 0;
                }
                (KeyModifiers::NONE, KeyCode::End) => {
                    *scroll = usize::MAX;
                }
                _ => {}
            }
            return;
        }

        // Command palette needs mutable access — handle before immutable borrow.
        if matches!(self.popup, Some(Popup::CommandPalette { .. })) {
            self.handle_command_palette_key(key);
            return;
        }

        if let Some(popup) = &self.popup {
            match popup {
                Popup::Help { .. } | Popup::CommandPalette { .. } => {
                    unreachable!("handled above")
                }
                Popup::ConfirmDelete { saved_id, name } => {
                    match (key.modifiers, key.code) {
                        // Confirm deletion: Y or Enter
                        (KeyModifiers::NONE, KeyCode::Char('y' | 'Y') | KeyCode::Enter) => {
                            let saved_id = *saved_id;
                            let name_copy = name.clone();
                            self.popup = None;
                            // Spawn async task to delete
                            self.delete_saved_query(saved_id, name_copy);
                        }
                        // Cancel: N, Esc, or any other key
                        _ => {
                            self.popup = None;
                        }
                    }
                }
                Popup::SaveQuery { .. } => {
                    self.handle_save_query_key(key);
                }
                Popup::Error { .. } => {
                    if matches!(
                        (key.modifiers, key.code),
                        (KeyModifiers::NONE, KeyCode::Esc | KeyCode::Enter)
                    ) {
                        self.popup = None;
                    }
                }
                Popup::SetSchedule { .. } => {
                    self.handle_set_schedule_key(key);
                }
                Popup::ColumnPicker { selected, scroll } => {
                    let selected = *selected;
                    let scroll = *scroll;
                    let col_count = self
                        .active_tab()
                        .column_config
                        .as_ref()
                        .map_or(0, |c| c.columns.len());

                    match (key.modifiers, key.code) {
                        (KeyModifiers::NONE, KeyCode::Esc | KeyCode::Char('q')) => {
                            self.popup = None;
                        }
                        (KeyModifiers::NONE, KeyCode::Up) => {
                            self.popup = Some(Popup::ColumnPicker {
                                selected: selected.saturating_sub(1),
                                scroll: if selected.saturating_sub(1) < scroll {
                                    selected.saturating_sub(1)
                                } else {
                                    scroll
                                },
                            });
                        }
                        (KeyModifiers::NONE, KeyCode::Down) => {
                            let new_sel = (selected + 1).min(col_count.saturating_sub(1));
                            self.popup = Some(Popup::ColumnPicker {
                                selected: new_sel,
                                scroll: scroll.max(new_sel.saturating_sub(15)),
                            });
                        }
                        // Toggle visibility
                        (KeyModifiers::NONE, KeyCode::Char(' ')) => {
                            if let Some(ref mut config) = self.active_tab_mut().column_config {
                                let vis = config.visible_count();
                                if let Some(entry) = config.columns.get_mut(selected)
                                    && (entry.hidden || vis > 1)
                                {
                                    entry.hidden = !entry.hidden;
                                }
                            }
                        }
                        // Toggle pin
                        (KeyModifiers::NONE, KeyCode::Char('p')) => {
                            if let Some(ref mut config) = self.active_tab_mut().column_config
                                && let Some(entry) = config.columns.get_mut(selected)
                            {
                                entry.pinned = !entry.pinned;
                            }
                        }
                        _ => {}
                    }
                }
                Popup::EventDetail {
                    row_index, scroll, ..
                } => {
                    let row_index = *row_index;
                    let scroll = *scroll;
                    let row_count = self
                        .active_tab()
                        .result
                        .as_ref()
                        .map_or(0, |r| r.result.row_count());

                    match (key.modifiers, key.code) {
                        // Close: Esc or q
                        (KeyModifiers::NONE, KeyCode::Esc | KeyCode::Char('q')) => {
                            self.popup = None;
                        }
                        // Scroll up
                        (KeyModifiers::NONE, KeyCode::Up) => {
                            self.popup = Some(Popup::EventDetail {
                                row_index,
                                scroll: scroll.saturating_sub(1),
                            });
                        }
                        // Scroll down
                        (KeyModifiers::NONE, KeyCode::Down) => {
                            self.popup = Some(Popup::EventDetail {
                                row_index,
                                scroll: scroll + 1,
                            });
                        }
                        // Page scroll
                        (KeyModifiers::NONE, KeyCode::PageUp) => {
                            self.popup = Some(Popup::EventDetail {
                                row_index,
                                scroll: scroll.saturating_sub(10),
                            });
                        }
                        (KeyModifiers::NONE, KeyCode::PageDown) => {
                            self.popup = Some(Popup::EventDetail {
                                row_index,
                                scroll: scroll + 10,
                            });
                        }
                        // Navigate to prev row
                        (KeyModifiers::NONE, KeyCode::Char('[')) if row_index > 0 => {
                            let new_idx = row_index - 1;
                            self.active_tab_mut().selected_row = Some(new_idx);
                            self.popup = Some(Popup::EventDetail {
                                row_index: new_idx,
                                scroll: 0,
                            });
                        }
                        // Navigate to next row
                        (KeyModifiers::NONE, KeyCode::Char(']')) if row_index + 1 < row_count => {
                            let new_idx = row_index + 1;
                            self.active_tab_mut().selected_row = Some(new_idx);
                            self.popup = Some(Popup::EventDetail {
                                row_index: new_idx,
                                scroll: 0,
                            });
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    /// Handle key events for the save query popup (delegates to `SimpleEditor`).
    fn handle_save_query_key(&mut self, key: event::KeyEvent) {
        match (key.modifiers, key.code) {
            // Confirm save: Enter
            (KeyModifiers::NONE, KeyCode::Enter) => {
                if let Some(Popup::SaveQuery { ref editor }) = self.popup {
                    let name = editor.text().trim().to_owned();
                    if name.is_empty() {
                        // Ignore empty names.
                    } else if !is_valid_query_name(&name) {
                        self.popup = Some(Popup::Error {
                            message: "name must contain only letters, numbers, \
                                      hyphens, and underscores"
                                .to_string(),
                        });
                    } else {
                        self.popup = None;
                        self.save_current_query(name);
                    }
                }
            }
            // Cancel: Esc
            (KeyModifiers::NONE, KeyCode::Esc) => {
                self.popup = None;
            }
            // Delegate all other keys to the editor
            _ => {
                if let Some(Popup::SaveQuery { ref mut editor }) = self.popup {
                    editor.handle_key(key);
                }
            }
        }
    }

    /// Handle key events for the set-schedule popup.
    fn handle_set_schedule_key(&mut self, key: event::KeyEvent) {
        match (key.modifiers, key.code) {
            // Confirm: Enter
            (KeyModifiers::NONE, KeyCode::Enter) => {
                if let Some(Popup::SetSchedule {
                    saved_id,
                    ref editor,
                    ..
                }) = self.popup
                {
                    let interval = editor.text().trim().to_owned();
                    if !interval.is_empty() {
                        let saved_id_copy = saved_id;
                        self.popup = None;
                        let client = self.client.clone();
                        let mutation_tx = self.mutation_tx.clone();
                        tokio::spawn(async move {
                            let result = match client
                                .set_schedule(saved_id_copy, &interval, None, true)
                                .await
                            {
                                Ok(_) => MutationResult::ScheduleSet {
                                    saved_query_id: saved_id_copy,
                                },
                                Err(e) => MutationResult::Error {
                                    message: format!("failed to set schedule: {e}"),
                                },
                            };
                            let _ = mutation_tx.send(result);
                        });
                    }
                }
            }
            // Cancel: Esc
            (KeyModifiers::NONE, KeyCode::Esc) => {
                self.popup = None;
            }
            // Delegate all other keys to the editor
            _ => {
                if let Some(Popup::SetSchedule { ref mut editor, .. }) = self.popup {
                    editor.handle_key(key);
                }
            }
        }
    }

    /// Delete a saved query by ID.
    pub(crate) fn delete_saved_query(&mut self, saved_id: i64, name: String) {
        let client = self.client.clone();
        let mutation_tx = self.mutation_tx.clone();

        tokio::spawn(async move {
            let result = match client.delete_saved(saved_id).await {
                Ok(_) => {
                    tracing::info!("deleted saved query '{name}'");
                    MutationResult::SavedQueryDeleted { name }
                }
                Err(e) => {
                    tracing::error!("failed to delete saved query: {e}");
                    MutationResult::Error {
                        message: format!("Failed to delete query: {e}"),
                    }
                }
            };
            let _ = mutation_tx.send(result);
        });
    }

    /// Save the current query with the given name.
    pub(crate) fn save_current_query(&mut self, name: String) {
        let query = self.active_tab().editor.text();
        if query.trim().is_empty() {
            return;
        }

        let client = self.client.clone();
        let mutation_tx = self.mutation_tx.clone();
        tokio::spawn(async move {
            let result = match client.create_saved(&name, &query).await {
                Ok(_) => {
                    tracing::info!("saved query '{name}'");
                    MutationResult::SavedQueryCreated { name: name.clone() }
                }
                Err(e) => {
                    tracing::error!("failed to save query: {e}");
                    MutationResult::Error {
                        message: format!("Failed to save query: {e}"),
                    }
                }
            };
            let _ = mutation_tx.send(result);
        });
    }
}
