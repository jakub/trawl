//! TUI application state machine and event loop.

pub mod highlight;
pub mod state;
mod ui;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use fleet_client::{
    HistoryResponse, HttpClient, ListSavedResponse, QueryResponse, SchemaResponse, StreamEvent,
};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use self::state::{ChartView, Focus, LiveBuffer, Popup, Sidebar, Tab, TabStatus};
use crate::CliError;
use crate::config::Config;

/// Get text from the system clipboard. Returns `None` if clipboard is unavailable.
fn clipboard_get() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok()
}

/// Set text on the system clipboard. Silently fails if clipboard is unavailable.
fn clipboard_set(text: &str) {
    if let Ok(mut cb) = arboard::Clipboard::new() {
        let _ = cb.set_text(text.to_owned());
    }
}

/// Result of an async query execution.
#[derive(Debug)]
struct QueryResult {
    /// Index of the tab that requested the query.
    tab_idx: usize,
    /// Query execution result.
    result: Result<QueryResponse, String>,
    /// Execution duration.
    duration: Duration,
}

/// Result of a mutation operation (save/delete saved query).
#[derive(Debug)]
enum MutationResult {
    /// Saved query was created successfully.
    SavedQueryCreated { name: String },
    /// Saved query was deleted successfully.
    SavedQueryDeleted { name: String },
    /// Saved queries cache refreshed.
    CacheRefreshed {
        saved: ListSavedResponse,
        /// Name of the query to select after refresh (if any).
        select_name: Option<String>,
    },
    /// Mutation failed.
    Error { message: String },
}

/// Main TUI application state.
pub struct App {
    /// HTTP client for API calls.
    pub client: HttpClient,
    /// Open tabs.
    pub tabs: Vec<Tab>,
    /// Index of the active tab.
    pub active_tab_idx: usize,
    /// Which pane has focus.
    pub focus: Focus,
    /// Active sidebar (if any).
    pub sidebar: Option<Sidebar>,
    /// Active popup (if any).
    pub popup: Option<Popup>,
    /// Cached schema response (fetched at startup).
    pub schema_cache: Option<SchemaResponse>,
    /// Cached history response (fetched at startup).
    pub history_cache: Option<HistoryResponse>,
    /// Cached saved queries (fetched at startup).
    pub saved_cache: Option<ListSavedResponse>,
    /// Selected index in history sidebar.
    pub history_selected_index: usize,
    /// Selected index in saved queries sidebar.
    pub saved_selected_index: usize,
    /// Whether to quit the application.
    pub should_quit: bool,
    /// Whether live tail mode is active.
    pub live_mode: bool,
    /// When true, Enter executes query and Shift+Enter inserts newline.
    pub enter_executes: bool,
    /// Maximum events to retain in live streaming buffer.
    max_live_events: usize,
    /// Handle to the live streaming task (if active).
    live_task: Option<tokio::task::JoinHandle<()>>,
    /// Channel for receiving query results from background tasks.
    query_rx: mpsc::UnboundedReceiver<QueryResult>,
    /// Sender for spawning queries.
    query_tx: mpsc::UnboundedSender<QueryResult>,
    /// Channel for receiving mutation results.
    mutation_rx: mpsc::UnboundedReceiver<MutationResult>,
    /// Sender for mutation operations.
    mutation_tx: mpsc::UnboundedSender<MutationResult>,
}

impl App {
    /// Create a new app with the given client.
    pub fn new(client: HttpClient) -> Self {
        let (query_tx, query_rx) = mpsc::unbounded_channel();
        let (mutation_tx, mutation_rx) = mpsc::unbounded_channel();

        Self {
            client,
            tabs: vec![Tab::new(0)],
            active_tab_idx: 0,
            focus: Focus::Editor,
            sidebar: None,
            popup: None,
            schema_cache: None,
            history_cache: None,
            saved_cache: None,
            history_selected_index: 0,
            saved_selected_index: 0,
            should_quit: false,
            live_mode: false,
            enter_executes: false,
            max_live_events: 1000,
            live_task: None,
            query_rx,
            query_tx,
            mutation_rx,
            mutation_tx,
        }
    }

    /// Get the currently active tab.
    pub fn active_tab(&self) -> &Tab {
        &self.tabs[self.active_tab_idx]
    }

    /// Get the currently active tab mutably.
    pub fn active_tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active_tab_idx]
    }

    /// Execute a query in the background.
    pub fn execute_query(&mut self) {
        // Stop live streaming if active (user is running a new/edited query).
        if self.live_mode {
            self.stop_live_stream();
        }

        let tab = self.active_tab_mut();
        let query = tab.editor.text().trim().to_owned();

        if query.is_empty() {
            return;
        }

        // Update tab status to running.
        tab.status = TabStatus::Running {
            start: Instant::now(),
        };

        // Spawn background task to execute query.
        let client = self.client.clone();
        let tx = self.query_tx.clone();
        let tab_idx = self.active_tab_idx;

        tokio::spawn(async move {
            tracing::info!("background task started for query: {}", query);
            let start = Instant::now();
            let result = client
                .query_paginated(&query, None, None)
                .await
                .map_err(|e| e.to_string());
            let duration = start.elapsed();
            tracing::info!(
                "query completed in {:?}, result: {:?}",
                duration,
                result.is_ok()
            );

            let _ = tx.send(QueryResult {
                tab_idx,
                result,
                duration,
            });
        });
    }

    /// Refresh the saved queries cache from the server.
    fn refresh_saved_cache(&mut self, select_name: Option<String>) {
        let client = self.client.clone();
        let mutation_tx = self.mutation_tx.clone();
        tokio::spawn(async move {
            let result = match client.list_saved().await {
                Ok(saved) => {
                    tracing::debug!(
                        "refreshed saved queries cache: {} queries",
                        saved.queries.len()
                    );
                    MutationResult::CacheRefreshed { saved, select_name }
                }
                Err(e) => {
                    tracing::error!("failed to refresh saved queries: {e}");
                    MutationResult::Error {
                        message: format!("Failed to refresh saved queries: {e}"),
                    }
                }
            };
            let _ = mutation_tx.send(result);
        });
    }

    /// Poll for mutation results and refresh caches.
    pub fn poll_mutations(&mut self) {
        while let Ok(mutation_result) = self.mutation_rx.try_recv() {
            match mutation_result {
                MutationResult::SavedQueryCreated { name } => {
                    tracing::info!("saved query created: {name}");
                    // Refresh saved queries cache and select the new query
                    self.refresh_saved_cache(Some(name.clone()));
                    // Open saved queries sidebar to show the new query
                    self.sidebar = Some(Sidebar::Saved);
                }
                MutationResult::SavedQueryDeleted { name } => {
                    tracing::info!("saved query deleted: {name}");
                    // Refresh saved queries cache
                    self.refresh_saved_cache(None);
                }
                MutationResult::CacheRefreshed { saved, select_name } => {
                    tracing::debug!(
                        "updating saved queries cache with {} queries",
                        saved.queries.len()
                    );

                    // If we should select a specific query, find its index
                    if let Some(name) = select_name {
                        if let Some(idx) = saved.queries.iter().position(|q| q.name == name) {
                            self.saved_selected_index = idx;
                        }
                    }

                    self.saved_cache = Some(saved);

                    // Reset selection if it's now out of bounds
                    if let Some(cache) = &self.saved_cache {
                        if self.saved_selected_index >= cache.queries.len()
                            && !cache.queries.is_empty()
                        {
                            self.saved_selected_index = cache.queries.len().saturating_sub(1);
                        }
                    }
                }
                MutationResult::Error { message } => {
                    tracing::error!("mutation error: {message}");
                    // TODO: Show error in UI (maybe status bar or popup)
                }
            }
        }
    }

    /// Poll for query results and update tabs.
    pub fn poll_query_results(&mut self) {
        while let Ok(query_result) = self.query_rx.try_recv() {
            tracing::info!("received query result for tab {}", query_result.tab_idx);
            if query_result.tab_idx >= self.tabs.len() {
                // Tab was closed while query was running.
                continue;
            }

            let tab = &mut self.tabs[query_result.tab_idx];

            match query_result.result {
                Ok(response) => {
                    // Auto-switch to sparkline view for timechart queries
                    let is_timechart = response
                        .result
                        .columns
                        .first()
                        .is_some_and(|col| col.name == "_time");
                    if is_timechart && tab.chart_view == ChartView::Table {
                        tab.chart_view = ChartView::Sparkline;
                    }

                    tab.result = Some(response);
                    #[allow(clippy::cast_possible_truncation)] // Query duration < u64::MAX ms
                    let duration_ms = query_result.duration.as_millis() as u64;
                    tab.status = TabStatus::Success { duration_ms };
                    tab.scroll_offset = 0; // Reset vertical scroll to top.
                    tab.horizontal_scroll_offset = 0; // Reset horizontal scroll to left.
                }
                Err(message) => {
                    tab.status = TabStatus::Error { message };
                }
            }
        }
    }

    /// Handle a key event.
    pub fn handle_key(&mut self, key: event::KeyEvent) {
        // Popups take priority over everything else.
        if self.popup.is_some() {
            self.handle_popup_key(key);
            return;
        }

        // Global keybindings (work regardless of focus).
        match (key.modifiers, key.code) {
            // Quit: Ctrl+Q
            (KeyModifiers::CONTROL, KeyCode::Char('q')) => {
                self.should_quit = true;
                return;
            }
            // Toggle help: F1
            (KeyModifiers::NONE, KeyCode::F(1)) => {
                self.toggle_sidebar(Sidebar::Help);
                return;
            }
            // Toggle schema: F2
            (KeyModifiers::NONE, KeyCode::F(2)) => {
                self.toggle_sidebar(Sidebar::Schema);
                return;
            }
            // Toggle history: F3
            (KeyModifiers::NONE, KeyCode::F(3)) => {
                self.toggle_sidebar(Sidebar::History);
                return;
            }
            // Toggle saved queries: F4
            (KeyModifiers::NONE, KeyCode::F(4)) => {
                self.toggle_sidebar(Sidebar::Saved);
                return;
            }
            // Toggle live tail: F9
            (KeyModifiers::NONE, KeyCode::F(9)) => {
                self.toggle_live_mode();
                return;
            }
            // Close sidebar: Esc (if sidebar is open)
            (KeyModifiers::NONE, KeyCode::Esc) if self.sidebar.is_some() => {
                self.sidebar = None;
                return;
            }
            // New tab: Ctrl+T
            (KeyModifiers::CONTROL, KeyCode::Char('t')) => {
                let new_id = self.tabs.len();
                self.tabs.push(Tab::new(new_id));
                self.active_tab_idx = new_id;
                return;
            }
            // Close tab: Ctrl+W
            (KeyModifiers::CONTROL, KeyCode::Char('w')) if self.tabs.len() > 1 => {
                self.tabs.remove(self.active_tab_idx);
                if self.active_tab_idx >= self.tabs.len() {
                    self.active_tab_idx = self.tabs.len() - 1;
                }
                return;
            }
            // Cycle tabs: Shift+Tab
            (KeyModifiers::SHIFT, KeyCode::BackTab) if self.tabs.len() > 1 => {
                self.active_tab_idx = (self.active_tab_idx + 1) % self.tabs.len();
                return;
            }
            // Save current query: Ctrl+S
            (KeyModifiers::CONTROL, KeyCode::Char('s')) => {
                let query = self.active_tab().editor.text();
                if !query.trim().is_empty() {
                    self.popup = Some(Popup::SaveQuery {
                        input: String::new(),
                    });
                }
                return;
            }
            _ => {}
        }

        // If sidebar is open, handle sidebar-specific keys.
        if let Some(sidebar) = self.sidebar {
            match sidebar {
                Sidebar::History => self.handle_history_key(key),
                Sidebar::Saved => self.handle_saved_key(key),
                _ => {
                    // Other sidebars don't handle keys yet
                }
            }
            return;
        }

        // Focus-specific keybindings.
        match self.focus {
            Focus::Editor => self.handle_editor_key(key),
            Focus::Results => self.handle_results_key(key),
        }
    }

    /// Handle key events when editor is focused.
    #[allow(clippy::too_many_lines)] // Inherently large key dispatch
    fn handle_editor_key(&mut self, key: event::KeyEvent) {
        // Debug: log the key event to see what we're receiving
        tracing::debug!(
            "editor key: modifiers={:?}, code={:?}",
            key.modifiers,
            key.code
        );

        match (key.modifiers, key.code) {
            // Switch to results: Tab
            (KeyModifiers::NONE, KeyCode::Tab) => {
                self.focus = Focus::Results;
            }
            // Execute query: F5 (easier than Ctrl+Enter which varies by terminal)
            (KeyModifiers::NONE, KeyCode::F(5)) => {
                tracing::info!("executing query with F5");
                self.execute_query();
            }
            // Execute query: Ctrl+Enter (always executes regardless of config)
            (_, KeyCode::Enter) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                tracing::info!("executing query with ctrl+enter");
                self.execute_query();
            }
            // Configurable Enter: when enter_executes=true, Enter executes and Shift+Enter inserts newline
            (KeyModifiers::NONE, KeyCode::Enter) if self.enter_executes => {
                tracing::info!("executing query with enter (enter_executes mode)");
                self.execute_query();
            }
            // Shift+Enter always inserts newline (both modes).
            // iTerm2 sends Shift+Enter as Ctrl+J (ASCII LF), so handle both.
            (KeyModifiers::SHIFT, KeyCode::Enter) | (KeyModifiers::CONTROL, KeyCode::Char('j')) => {
                self.active_tab_mut().editor.insert_newline();
            }
            // Clear editor: Ctrl+L
            (KeyModifiers::CONTROL, KeyCode::Char('l')) => {
                self.active_tab_mut().clear();
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
            // Word movement: Ctrl+Left / Alt+B (readline)
            (KeyModifiers::CONTROL, KeyCode::Left) | (KeyModifiers::ALT, KeyCode::Char('b')) => {
                let editor = &mut self.active_tab_mut().editor;
                editor.clear_selection();
                editor.move_word_left();
            }
            // Word movement: Ctrl+Right / Alt+F (readline)
            (KeyModifiers::CONTROL, KeyCode::Right) | (KeyModifiers::ALT, KeyCode::Char('f')) => {
                let editor = &mut self.active_tab_mut().editor;
                editor.clear_selection();
                editor.move_word_right();
            }
            // Selection: Shift+Arrow
            (_, KeyCode::Left) if key.modifiers.contains(KeyModifiers::SHIFT) => {
                let editor = &mut self.active_tab_mut().editor;
                editor.start_selection();
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    editor.move_word_left();
                } else {
                    editor.move_left();
                }
            }
            (_, KeyCode::Right) if key.modifiers.contains(KeyModifiers::SHIFT) => {
                let editor = &mut self.active_tab_mut().editor;
                editor.start_selection();
                if key.modifiers.contains(KeyModifiers::CONTROL) {
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
            }
            // Redo: Ctrl+Y / Ctrl+Shift+Z
            (KeyModifiers::CONTROL, KeyCode::Char('y')) => {
                self.active_tab_mut().editor.redo();
            }
            (_, KeyCode::Char('Z'))
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.active_tab_mut().editor.redo();
            }
            // Clipboard: Ctrl+C (copy), Ctrl+X (cut)
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                if let Some(text) = self.active_tab().editor.selected_text() {
                    clipboard_set(&text);
                }
                // Without selection, Ctrl+C is intentionally a no-op (Ctrl+Q is quit)
            }
            (KeyModifiers::CONTROL, KeyCode::Char('x')) => {
                if let Some(text) = self.active_tab().editor.selected_text() {
                    clipboard_set(&text);
                    self.active_tab_mut().editor.delete_selection();
                }
            }
            // Clipboard: Ctrl+V (paste)
            (KeyModifiers::CONTROL, KeyCode::Char('v')) => {
                if let Some(text) = clipboard_get() {
                    let editor = &mut self.active_tab_mut().editor;
                    editor.delete_selection();
                    editor.insert_text(&text);
                }
            }
            // Kill word before cursor: Ctrl+W / Ctrl+Backspace
            (KeyModifiers::CONTROL, KeyCode::Char('w') | KeyCode::Backspace) => {
                self.active_tab_mut().editor.delete_word_before();
            }
            // Kill word after cursor: Ctrl+Delete / Alt+D
            (KeyModifiers::CONTROL, KeyCode::Delete) | (KeyModifiers::ALT, KeyCode::Char('d')) => {
                self.active_tab_mut().editor.delete_word_after();
            }
            // Kill to line start: Ctrl+U
            (KeyModifiers::CONTROL, KeyCode::Char('u')) => {
                self.active_tab_mut().editor.delete_to_line_start();
            }
            // Kill to line end: Ctrl+K
            (KeyModifiers::CONTROL, KeyCode::Char('k')) => {
                self.active_tab_mut().editor.delete_to_line_end();
            }
            // Handle text editing.
            _ => {
                let editor = &mut self.active_tab_mut().editor;
                match key.code {
                    KeyCode::Char(ch) => editor.insert_char(ch),
                    KeyCode::Enter => editor.insert_newline(),
                    KeyCode::Backspace => editor.delete_char_before(),
                    KeyCode::Delete => editor.delete_char_at(),
                    KeyCode::Left => {
                        editor.clear_selection();
                        editor.move_left();
                    }
                    KeyCode::Right => {
                        editor.clear_selection();
                        editor.move_right();
                    }
                    KeyCode::Up => {
                        editor.clear_selection();
                        editor.move_up();
                    }
                    KeyCode::Down => {
                        editor.clear_selection();
                        editor.move_down();
                    }
                    KeyCode::Home => {
                        editor.clear_selection();
                        editor.move_to_line_start();
                    }
                    KeyCode::End => {
                        editor.clear_selection();
                        editor.move_to_line_end();
                    }
                    _ => {}
                }
            }
        }
    }

    /// Handle key events when results are focused.
    fn handle_results_key(&mut self, key: event::KeyEvent) {
        match (key.modifiers, key.code) {
            // Switch back to editor
            (KeyModifiers::NONE, KeyCode::Tab) => {
                self.focus = Focus::Editor;
            }
            // Vertical scrolling
            (KeyModifiers::NONE, KeyCode::Up) => {
                let tab = self.active_tab_mut();
                tab.scroll_offset = tab.scroll_offset.saturating_sub(1);
            }
            (KeyModifiers::NONE, KeyCode::Down) => {
                let tab = self.active_tab_mut();
                tab.scroll_offset = tab.scroll_offset.saturating_add(1);
            }
            (KeyModifiers::NONE, KeyCode::PageUp) => {
                let tab = self.active_tab_mut();
                tab.scroll_offset = tab.scroll_offset.saturating_sub(10);
            }
            (KeyModifiers::NONE, KeyCode::PageDown) => {
                let tab = self.active_tab_mut();
                tab.scroll_offset = tab.scroll_offset.saturating_add(10);
            }
            (KeyModifiers::NONE, KeyCode::Home) => {
                self.active_tab_mut().scroll_offset = 0;
            }
            (KeyModifiers::NONE, KeyCode::End) => {
                let tab = self.active_tab_mut();
                // Set to max value, render will clamp to proper bounds
                tab.scroll_offset = usize::MAX;
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
            // Cycle chart view
            (KeyModifiers::NONE, KeyCode::Char('v')) => {
                let tab = self.active_tab_mut();
                tab.chart_view = tab.chart_view.next();
            }
            _ => {}
        }
    }

    /// Handle key events when history sidebar is focused.
    fn handle_history_key(&mut self, key: event::KeyEvent) {
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Up) => {
                if let Some(history) = &self.history_cache {
                    if !history.entries.is_empty() {
                        self.history_selected_index = self.history_selected_index.saturating_sub(1);
                    }
                }
            }
            (KeyModifiers::NONE, KeyCode::Down) => {
                if let Some(history) = &self.history_cache {
                    if !history.entries.is_empty() {
                        let max_index = history.entries.len().saturating_sub(1);
                        self.history_selected_index =
                            (self.history_selected_index + 1).min(max_index);
                    }
                }
            }
            (KeyModifiers::NONE, KeyCode::Enter) => {
                // Load selected query into editor
                // First, extract the query string (to avoid borrow issues)
                let query_text = self
                    .history_cache
                    .as_ref()
                    .and_then(|h| h.entries.get(self.history_selected_index))
                    .map(|entry| entry.query.clone());

                if let Some(query) = query_text {
                    let tab = self.active_tab_mut();
                    tab.editor.clear();
                    tab.editor.insert_text(&query);
                    tab.editor.move_to_line_end();
                    self.sidebar = None;
                }
            }
            _ => {}
        }
    }

    /// Handle key events when saved queries sidebar is focused.
    fn handle_saved_key(&mut self, key: event::KeyEvent) {
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Up) => {
                if let Some(saved) = &self.saved_cache {
                    if !saved.queries.is_empty() {
                        self.saved_selected_index = self.saved_selected_index.saturating_sub(1);
                    }
                }
            }
            (KeyModifiers::NONE, KeyCode::Down) => {
                if let Some(saved) = &self.saved_cache {
                    if !saved.queries.is_empty() {
                        let max_index = saved.queries.len().saturating_sub(1);
                        self.saved_selected_index = (self.saved_selected_index + 1).min(max_index);
                    }
                }
            }
            (KeyModifiers::NONE, KeyCode::Enter) => {
                // Load selected query into editor
                let query_text = self
                    .saved_cache
                    .as_ref()
                    .and_then(|s| s.queries.get(self.saved_selected_index))
                    .map(|entry| entry.query.clone());

                if let Some(query) = query_text {
                    let tab = self.active_tab_mut();
                    tab.editor.clear();
                    tab.editor.insert_text(&query);
                    tab.editor.move_to_line_end();
                    self.sidebar = None;
                }
            }
            (KeyModifiers::NONE, KeyCode::Backspace | KeyCode::Delete) => {
                // Show confirmation popup for delete
                if let Some(saved) = &self.saved_cache {
                    if let Some(query) = saved.queries.get(self.saved_selected_index) {
                        self.popup = Some(Popup::ConfirmDelete {
                            saved_id: query.id,
                            name: query.name.clone(),
                        });
                    }
                }
            }
            _ => {}
        }
    }

    /// Handle key events when a popup is open.
    fn handle_popup_key(&mut self, key: event::KeyEvent) {
        if let Some(popup) = &self.popup {
            match popup {
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
                Popup::SaveQuery { input } => {
                    let mut current_input = input.clone();
                    match (key.modifiers, key.code) {
                        // Confirm save: Enter
                        (KeyModifiers::NONE, KeyCode::Enter) if !current_input.is_empty() => {
                            self.popup = None;
                            self.save_current_query(current_input);
                        }
                        // Cancel: Esc
                        (KeyModifiers::NONE, KeyCode::Esc) => {
                            self.popup = None;
                        }
                        // Backspace
                        (KeyModifiers::NONE, KeyCode::Backspace) => {
                            current_input.pop();
                            self.popup = Some(Popup::SaveQuery {
                                input: current_input,
                            });
                        }
                        // Regular character input
                        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(c)) => {
                            current_input.push(c);
                            self.popup = Some(Popup::SaveQuery {
                                input: current_input,
                            });
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    /// Delete a saved query by ID.
    fn delete_saved_query(&mut self, saved_id: i64, name: String) {
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
    fn save_current_query(&mut self, name: String) {
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

    /// Toggle a sidebar (close if already open, open otherwise).
    fn toggle_sidebar(&mut self, sidebar: Sidebar) {
        if self.sidebar == Some(sidebar) {
            self.sidebar = None;
        } else {
            // Reset selection when opening sidebars
            match sidebar {
                Sidebar::History => self.history_selected_index = 0,
                Sidebar::Saved => self.saved_selected_index = 0,
                _ => {}
            }
            self.sidebar = Some(sidebar);
        }
    }

    /// Toggle live tail mode (F9).
    fn toggle_live_mode(&mut self) {
        if self.live_mode {
            // Stop streaming
            self.stop_live_stream();
        } else {
            // Start streaming
            self.start_live_stream();
        }
    }

    /// Start live streaming with the current query.
    fn start_live_stream(&mut self) {
        let query = self.active_tab().editor.text().trim().to_owned();

        if query.is_empty() {
            tracing::warn!("cannot start live stream with empty query");
            return;
        }

        tracing::info!("starting live stream for query: {}", query);

        // Cancel any existing stream.
        if let Some(task) = self.live_task.take() {
            task.abort();
        }

        let client = self.client.clone();
        let tx = self.query_tx.clone();
        let tab_idx = self.active_tab_idx;
        let max_events = self.max_live_events;

        let task = tokio::spawn(async move {
            tracing::info!("live stream task started");

            let mut stream = match client.stream_events(&query).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("failed to start stream: {e}");
                    let _ = tx.send(QueryResult {
                        tab_idx,
                        result: Err(e.to_string()),
                        duration: Duration::from_secs(0),
                    });
                    return;
                }
            };

            let mut buffer = LiveBuffer::new(max_events);
            let mut event_count = 0usize;

            while let Some(event) = stream.next().await {
                match event {
                    Ok(StreamEvent::Event(map)) => {
                        buffer.push_event(&map);
                        event_count += 1;
                        tracing::debug!("received stream event, total: {event_count}");
                    }
                    Ok(StreamEvent::Snapshot {
                        ref columns,
                        ref rows,
                    }) => {
                        buffer.replace_with_snapshot(columns, rows);
                        // Snapshots are complete results — send immediately.
                        let response = buffer.to_query_response();
                        let _ = tx.send(QueryResult {
                            tab_idx,
                            result: Ok(response),
                            duration: Duration::from_secs(0),
                        });
                    }
                    Ok(StreamEvent::Row(_)) => {
                        // Legacy variant — shouldn't appear from SSE stream.
                        tracing::debug!("ignoring unexpected Row variant in live stream");
                    }
                    Ok(StreamEvent::Error(msg)) => {
                        tracing::error!("stream error event: {msg}");
                    }
                    Ok(StreamEvent::Lagged(n)) => {
                        tracing::warn!(
                            event_type = "stream_lagged",
                            missed = n,
                            "stream subscriber fell behind"
                        );
                    }
                    Err(e) => {
                        tracing::error!("stream error: {e}");
                        break;
                    }
                }

                // Send snapshot every 10 events.
                if event_count % 10 == 0 && event_count > 0 {
                    let response = buffer.to_query_response();
                    tracing::debug!(
                        "sending live buffer snapshot ({} rows)",
                        response.result.row_count()
                    );

                    let _ = tx.send(QueryResult {
                        tab_idx,
                        result: Ok(response),
                        duration: Duration::from_secs(0),
                    });
                }
            }

            // Flush remaining events.
            if !buffer.is_empty() {
                let response = buffer.to_query_response();
                let _ = tx.send(QueryResult {
                    tab_idx,
                    result: Ok(response),
                    duration: Duration::from_secs(0),
                });
            }

            tracing::info!("live stream task ended");
        });

        self.live_task = Some(task);
        self.live_mode = true;
    }

    /// Stop live streaming.
    fn stop_live_stream(&mut self) {
        tracing::info!("stopping live stream");

        if let Some(task) = self.live_task.take() {
            task.abort();
        }

        self.live_mode = false;
    }
}

/// Run the TUI application.
pub async fn run(config: &Config, direct_token: Option<&str>) -> Result<(), CliError> {
    // Load token.
    let token = config.load_token(direct_token)?;

    // Create HTTP client.
    let client = if config.server.insecure {
        HttpClient::new_insecure(&config.server.url, token)?
    } else {
        HttpClient::new(&config.server.url, token)?
    };

    // Set up terminal.
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Fetch schema, history, and saved queries in parallel (non-blocking startup).
    tracing::info!("fetching schema, history, and saved queries");
    let (schema_result, history_result, saved_result) = tokio::join!(
        client.schema(),
        client.history(Some(100), None),
        client.list_saved()
    );

    let schema = match schema_result {
        Ok(s) => {
            tracing::info!("schema fetched: {} columns", s.columns.len());
            Some(s)
        }
        Err(e) => {
            tracing::warn!("failed to fetch schema: {}", e);
            None
        }
    };

    let history = match history_result {
        Ok(h) => {
            tracing::info!("history fetched: {} entries", h.entries.len());
            Some(h)
        }
        Err(e) => {
            tracing::warn!("failed to fetch history: {}", e);
            None
        }
    };

    let saved = match saved_result {
        Ok(sq) => {
            tracing::info!("saved queries fetched: {} entries", sq.queries.len());
            Some(sq)
        }
        Err(e) => {
            tracing::warn!("failed to fetch saved queries: {}", e);
            None
        }
    };

    // Create app with schema, history, and saved queries.
    let mut app = App::new(client);
    app.max_live_events = config.tail.max_events;
    app.enter_executes = config.ui.enter_executes;
    app.schema_cache = schema;
    app.history_cache = history;
    app.saved_cache = saved;

    // Event loop.
    let result = run_event_loop(&mut terminal, &mut app);

    // Restore terminal.
    disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;

    result
}

/// Main event loop.
fn run_event_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> Result<(), CliError> {
    let mut iteration = 0u64;
    loop {
        iteration += 1;
        if iteration % 10 == 0 {
            tracing::debug!("event loop iteration {}", iteration);
        }

        // Poll for query results from background tasks.
        app.poll_query_results();

        // Poll for mutation results (save/delete operations).
        app.poll_mutations();

        // Draw UI.
        terminal.draw(|f| ui::render(app, f))?;

        // Poll for events (100ms timeout).
        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    app.handle_key(key);
                }
                Event::Paste(text) => {
                    if app.focus == Focus::Editor {
                        let editor = &mut app.active_tab_mut().editor;
                        editor.save_snapshot();
                        editor.delete_selection();
                        editor.insert_text(&text);
                    }
                }
                _ => {}
            }
        }

        // Check quit flag.
        if app.should_quit {
            break;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_client::{ListSavedResponse, SavedQueryResponse};
    use fleet_engine::value::{Column, Value};

    /// Create an App with a dummy client for testing.
    /// No network calls will be made — only channel injection.
    pub(crate) fn test_app() -> App {
        let client = HttpClient::new_insecure("https://localhost:0", "flt_test_not_real").unwrap();
        App::new(client)
    }

    /// Build a synthetic `KeyEvent` with no modifiers.
    fn key(code: KeyCode) -> event::KeyEvent {
        event::KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Build a synthetic `KeyEvent` with modifiers.
    fn key_mod(code: KeyCode, modifiers: KeyModifiers) -> event::KeyEvent {
        event::KeyEvent::new(code, modifiers)
    }

    /// Build a successful `QueryResponse` with the given columns and rows.
    fn make_query_response(columns: Vec<&str>, rows: Vec<Vec<Value>>) -> QueryResponse {
        let cols = columns
            .into_iter()
            .map(|name| Column {
                name: name.to_owned(),
            })
            .collect();
        let returned = rows.len();
        QueryResponse {
            result: fleet_engine::value::QueryResult {
                columns: cols,
                rows,
            },
            truncated: false,
            pagination: fleet_client::PaginationMeta {
                limit: 10000,
                offset: 0,
                returned,
            },
        }
    }

    // --- Focus transition tests ---

    #[test]
    fn key_tab_switches_focus_editor_to_results() {
        let mut app = test_app();
        assert_eq!(app.focus, Focus::Editor);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Results);
    }

    #[test]
    fn key_tab_switches_focus_results_to_editor() {
        let mut app = test_app();
        app.focus = Focus::Results;
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Editor);
    }

    // --- Sidebar toggle tests ---

    #[test]
    fn key_f1_toggles_help() {
        let mut app = test_app();
        assert_eq!(app.sidebar, None);
        app.handle_key(key(KeyCode::F(1)));
        assert_eq!(app.sidebar, Some(Sidebar::Help));
        app.handle_key(key(KeyCode::F(1)));
        assert_eq!(app.sidebar, None);
    }

    #[test]
    fn key_f2_toggles_schema() {
        let mut app = test_app();
        app.handle_key(key(KeyCode::F(2)));
        assert_eq!(app.sidebar, Some(Sidebar::Schema));
        app.handle_key(key(KeyCode::F(2)));
        assert_eq!(app.sidebar, None);
    }

    #[test]
    fn key_f3_toggles_history() {
        let mut app = test_app();
        app.handle_key(key(KeyCode::F(3)));
        assert_eq!(app.sidebar, Some(Sidebar::History));
        app.handle_key(key(KeyCode::F(3)));
        assert_eq!(app.sidebar, None);
    }

    #[test]
    fn key_f4_toggles_saved() {
        let mut app = test_app();
        app.handle_key(key(KeyCode::F(4)));
        assert_eq!(app.sidebar, Some(Sidebar::Saved));
        app.handle_key(key(KeyCode::F(4)));
        assert_eq!(app.sidebar, None);
    }

    #[test]
    fn key_esc_closes_sidebar() {
        let mut app = test_app();
        app.sidebar = Some(Sidebar::Help);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.sidebar, None);
    }

    #[test]
    fn sidebar_blocks_focus_keys() {
        let mut app = test_app();
        app.sidebar = Some(Sidebar::Help);
        app.focus = Focus::Editor;
        // Tab should NOT switch focus while sidebar is open
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Editor);
    }

    // --- Tab management tests ---

    #[test]
    fn key_ctrl_t_creates_tab() {
        let mut app = test_app();
        assert_eq!(app.tabs.len(), 1);
        app.handle_key(key_mod(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.active_tab_idx, 1);
    }

    #[test]
    fn key_ctrl_w_closes_tab() {
        let mut app = test_app();
        // Create a second tab first
        app.handle_key(key_mod(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert_eq!(app.tabs.len(), 2);
        app.handle_key(key_mod(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(app.tabs.len(), 1);
    }

    #[test]
    fn key_ctrl_w_with_one_tab_is_noop() {
        let mut app = test_app();
        assert_eq!(app.tabs.len(), 1);
        app.handle_key(key_mod(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(app.tabs.len(), 1);
    }

    #[test]
    fn key_shift_tab_cycles_tabs() {
        let mut app = test_app();
        // Create 3 tabs total
        app.handle_key(key_mod(KeyCode::Char('t'), KeyModifiers::CONTROL));
        app.handle_key(key_mod(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert_eq!(app.tabs.len(), 3);
        assert_eq!(app.active_tab_idx, 2);

        // Cycle: 2 -> 0
        app.handle_key(key_mod(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(app.active_tab_idx, 0);

        // Cycle: 0 -> 1
        app.handle_key(key_mod(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(app.active_tab_idx, 1);
    }

    // --- Quit ---

    #[test]
    fn key_ctrl_q_sets_quit() {
        let mut app = test_app();
        app.handle_key(key_mod(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert!(app.should_quit);
    }

    // --- Popup interaction tests ---

    #[test]
    fn key_ctrl_s_opens_save_popup() {
        let mut app = test_app();
        // Type something so editor isn't empty
        app.handle_key(key(KeyCode::Char('x')));
        app.handle_key(key_mod(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert_eq!(
            app.popup,
            Some(Popup::SaveQuery {
                input: String::new()
            })
        );
    }

    #[test]
    fn key_ctrl_s_with_empty_editor_is_noop() {
        let mut app = test_app();
        app.handle_key(key_mod(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert_eq!(app.popup, None);
    }

    #[test]
    fn popup_blocks_global_keys() {
        let mut app = test_app();
        app.popup = Some(Popup::SaveQuery {
            input: String::new(),
        });
        // F1 should NOT open sidebar while popup is active
        app.handle_key(key(KeyCode::F(1)));
        assert_eq!(app.sidebar, None);
        assert!(app.popup.is_some());
    }

    #[test]
    fn popup_esc_closes() {
        let mut app = test_app();
        app.popup = Some(Popup::SaveQuery {
            input: String::new(),
        });
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.popup, None);
    }

    #[test]
    fn popup_typing_appends() {
        let mut app = test_app();
        app.popup = Some(Popup::SaveQuery {
            input: String::new(),
        });
        app.handle_key(key(KeyCode::Char('a')));
        app.handle_key(key(KeyCode::Char('b')));
        assert_eq!(
            app.popup,
            Some(Popup::SaveQuery {
                input: "ab".to_owned()
            })
        );
    }

    #[test]
    fn popup_backspace_removes() {
        let mut app = test_app();
        app.popup = Some(Popup::SaveQuery {
            input: "abc".to_owned(),
        });
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(
            app.popup,
            Some(Popup::SaveQuery {
                input: "ab".to_owned()
            })
        );
    }

    // --- Word movement keybinding tests ---

    #[test]
    fn key_ctrl_left_moves_word_left() {
        let mut app = test_app();
        // Type "hello world"
        for ch in "hello world".chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
        assert_eq!(app.active_tab().editor.cursor, (0, 11));
        app.handle_key(key_mod(KeyCode::Left, KeyModifiers::CONTROL));
        assert_eq!(app.active_tab().editor.cursor, (0, 6));
    }

    #[test]
    fn key_ctrl_right_moves_word_right() {
        let mut app = test_app();
        for ch in "hello world".chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
        app.active_tab_mut().editor.cursor = (0, 0);
        app.handle_key(key_mod(KeyCode::Right, KeyModifiers::CONTROL));
        assert_eq!(app.active_tab().editor.cursor, (0, 6));
    }

    #[test]
    fn key_alt_b_moves_word_left() {
        let mut app = test_app();
        for ch in "hello world".chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
        app.handle_key(key_mod(KeyCode::Char('b'), KeyModifiers::ALT));
        assert_eq!(app.active_tab().editor.cursor, (0, 6));
    }

    #[test]
    fn key_alt_f_moves_word_right() {
        let mut app = test_app();
        for ch in "hello world".chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
        app.active_tab_mut().editor.cursor = (0, 0);
        app.handle_key(key_mod(KeyCode::Char('f'), KeyModifiers::ALT));
        assert_eq!(app.active_tab().editor.cursor, (0, 6));
    }

    // --- Editor key routing ---

    #[test]
    fn editor_keys_reach_editor() {
        let mut app = test_app();
        assert_eq!(app.focus, Focus::Editor);
        app.handle_key(key(KeyCode::Char('h')));
        app.handle_key(key(KeyCode::Char('i')));
        assert_eq!(app.active_tab().editor.text(), "hi");
    }

    // --- Results key routing ---

    #[test]
    fn results_keys_scroll() {
        let mut app = test_app();
        app.focus = Focus::Results;
        assert_eq!(app.active_tab().scroll_offset, 0);
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.active_tab().scroll_offset, 1);
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.active_tab().scroll_offset, 2);
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.active_tab().scroll_offset, 1);
    }

    // --- Channel injection tests (poll_query_results / poll_mutations) ---

    #[test]
    fn poll_query_result_updates_tab() {
        let mut app = test_app();
        let response = make_query_response(
            vec!["host", "count"],
            vec![vec![Value::String("web-1".into()), Value::Integer(42)]],
        );
        app.query_tx
            .send(QueryResult {
                tab_idx: 0,
                result: Ok(response),
                duration: Duration::from_millis(50),
            })
            .unwrap();

        app.poll_query_results();

        assert!(app.tabs[0].result.is_some());
        assert!(matches!(app.tabs[0].status, TabStatus::Success { .. }));
    }

    #[test]
    fn poll_query_result_sets_error_status() {
        let mut app = test_app();
        app.query_tx
            .send(QueryResult {
                tab_idx: 0,
                result: Err("something broke".to_owned()),
                duration: Duration::from_millis(10),
            })
            .unwrap();

        app.poll_query_results();

        assert!(matches!(
            app.tabs[0].status,
            TabStatus::Error { ref message } if message == "something broke"
        ));
    }

    #[test]
    fn poll_query_result_resets_scroll() {
        let mut app = test_app();
        app.tabs[0].scroll_offset = 42;
        app.tabs[0].horizontal_scroll_offset = 7;

        let response = make_query_response(vec!["x"], vec![vec![Value::Integer(1)]]);
        app.query_tx
            .send(QueryResult {
                tab_idx: 0,
                result: Ok(response),
                duration: Duration::from_millis(1),
            })
            .unwrap();

        app.poll_query_results();

        assert_eq!(app.tabs[0].scroll_offset, 0);
        assert_eq!(app.tabs[0].horizontal_scroll_offset, 0);
    }

    #[test]
    fn poll_query_result_ignores_closed_tab() {
        let mut app = test_app();
        // Send result for tab index 5, which doesn't exist
        app.query_tx
            .send(QueryResult {
                tab_idx: 5,
                result: Ok(make_query_response(vec!["x"], vec![])),
                duration: Duration::from_millis(1),
            })
            .unwrap();

        // Should not panic
        app.poll_query_results();
    }

    #[test]
    fn poll_query_result_timechart_auto_switches_view() {
        let mut app = test_app();
        assert_eq!(app.tabs[0].chart_view, ChartView::Table);

        let response = make_query_response(
            vec!["_time", "count"],
            vec![vec![
                Value::String("2025-01-01T00:00:00Z".into()),
                Value::Integer(10),
            ]],
        );
        app.query_tx
            .send(QueryResult {
                tab_idx: 0,
                result: Ok(response),
                duration: Duration::from_millis(1),
            })
            .unwrap();

        app.poll_query_results();

        assert_eq!(app.tabs[0].chart_view, ChartView::Sparkline);
    }

    #[tokio::test]
    async fn poll_mutation_saved_created() {
        let mut app = test_app();
        app.mutation_tx
            .send(MutationResult::SavedQueryCreated {
                name: "my query".to_owned(),
            })
            .unwrap();

        app.poll_mutations();

        // SavedQueryCreated opens the sidebar to Saved
        assert_eq!(app.sidebar, Some(Sidebar::Saved));
    }

    #[test]
    fn poll_mutation_cache_refreshed() {
        let mut app = test_app();
        let saved = ListSavedResponse {
            queries: vec![SavedQueryResponse {
                id: 1,
                name: "test query".to_owned(),
                query: "level:error".to_owned(),
                created_at: "2025-01-01T00:00:00Z".to_owned(),
                updated_at: "2025-01-01T00:00:00Z".to_owned(),
            }],
        };

        app.mutation_tx
            .send(MutationResult::CacheRefreshed {
                saved,
                select_name: None,
            })
            .unwrap();

        app.poll_mutations();

        assert!(app.saved_cache.is_some());
        assert_eq!(app.saved_cache.as_ref().unwrap().queries.len(), 1);
    }

    #[test]
    fn poll_mutation_cache_refreshed_selects_name() {
        let mut app = test_app();
        let saved = ListSavedResponse {
            queries: vec![
                SavedQueryResponse {
                    id: 1,
                    name: "alpha".to_owned(),
                    query: "a".to_owned(),
                    created_at: String::new(),
                    updated_at: String::new(),
                },
                SavedQueryResponse {
                    id: 2,
                    name: "beta".to_owned(),
                    query: "b".to_owned(),
                    created_at: String::new(),
                    updated_at: String::new(),
                },
            ],
        };

        app.mutation_tx
            .send(MutationResult::CacheRefreshed {
                saved,
                select_name: Some("beta".to_owned()),
            })
            .unwrap();

        app.poll_mutations();

        assert_eq!(app.saved_selected_index, 1);
    }

    #[test]
    fn poll_mutation_error_is_handled() {
        let mut app = test_app();
        app.mutation_tx
            .send(MutationResult::Error {
                message: "kaboom".to_owned(),
            })
            .unwrap();

        // Should not panic
        app.poll_mutations();
    }

    #[test]
    fn multiple_results_processed_in_order() {
        let mut app = test_app();
        // Create 3 tabs
        app.tabs.push(Tab::new(1));
        app.tabs.push(Tab::new(2));

        for i in 0..3 {
            let response = make_query_response(
                vec!["idx"],
                vec![vec![Value::Integer(i64::try_from(i).unwrap())]],
            );
            app.query_tx
                .send(QueryResult {
                    tab_idx: i,
                    result: Ok(response),
                    duration: Duration::from_millis(1),
                })
                .unwrap();
        }

        app.poll_query_results();

        // All 3 tabs should have results
        for (i, tab) in app.tabs.iter().enumerate() {
            assert!(
                tab.result.is_some(),
                "tab {i} should have received its result"
            );
        }
    }
}
