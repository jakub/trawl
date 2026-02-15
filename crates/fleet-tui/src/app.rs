//! TUI application state machine and event loop.

use color_eyre::eyre::Result;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use fleet_client::{HistoryResponse, HttpClient, ListSavedResponse, QueryResponse, SchemaResponse};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use crate::config::Config;
use crate::state::{Focus, Popup, Sidebar, Tab, TabStatus};
use crate::ui;

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
            let result = client.query(&query).await.map_err(|e| e.to_string());
            let duration = start.elapsed();
            tracing::info!(
                "query completed in {:?}, result: {:?}",
                duration,
                result.is_ok()
            );

            // Convert QueryResult to QueryResponse (map the result).
            let result = result.map(|r| QueryResponse {
                result: r,
                truncated: false,
                pagination: fleet_client::PaginationMeta {
                    limit: 0,
                    offset: 0,
                    returned: 0,
                },
            });

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
            // Execute query: Ctrl+Enter
            (_, KeyCode::Enter) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                tracing::info!("executing query with ctrl+enter");
                self.execute_query();
            }
            // Clear editor: Ctrl+L
            (KeyModifiers::CONTROL, KeyCode::Char('l')) => {
                self.active_tab_mut().clear();
            }
            // Handle text editing.
            _ => {
                let editor = &mut self.active_tab_mut().editor;
                match key.code {
                    KeyCode::Char(ch) => editor.insert_char(ch),
                    KeyCode::Enter => editor.insert_newline(),
                    KeyCode::Backspace => editor.delete_char_before(),
                    KeyCode::Delete => editor.delete_char_at(),
                    KeyCode::Left => editor.move_left(),
                    KeyCode::Right => editor.move_right(),
                    KeyCode::Up => editor.move_up(),
                    KeyCode::Down => editor.move_down(),
                    KeyCode::Home => editor.move_to_line_start(),
                    KeyCode::End => editor.move_to_line_end(),
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
                    // Clear editor and set query
                    tab.editor.clear();
                    // Insert the query text line by line
                    for (i, line) in query.lines().enumerate() {
                        if i > 0 {
                            tab.editor.insert_newline();
                        }
                        for ch in line.chars() {
                            tab.editor.insert_char(ch);
                        }
                    }
                    // Move cursor to end
                    tab.editor.move_to_line_end();
                    // Close sidebar
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
                    // Clear editor and set query
                    tab.editor.clear();
                    // Insert the query text line by line
                    for (i, line) in query.lines().enumerate() {
                        if i > 0 {
                            tab.editor.insert_newline();
                        }
                        for ch in line.chars() {
                            tab.editor.insert_char(ch);
                        }
                    }
                    // Move cursor to end
                    tab.editor.move_to_line_end();
                    // Close sidebar
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
    #[allow(clippy::too_many_lines)] // SSE parsing requires detailed logic
    fn start_live_stream(&mut self) {
        let query = self.active_tab().editor.text().trim().to_owned();

        if query.is_empty() {
            tracing::warn!("cannot start live stream with empty query");
            return;
        }

        tracing::info!("starting live stream for query: {}", query);

        // Cancel any existing stream
        if let Some(task) = self.live_task.take() {
            task.abort();
        }

        // Get column names from existing result (if any) for the stream
        let existing_columns = self
            .active_tab()
            .result
            .as_ref()
            .map(|r| r.result.columns.clone());

        let client = self.client.clone();
        let tx = self.query_tx.clone();
        let tab_idx = self.active_tab_idx;

        // Spawn background task to stream results
        let task = tokio::spawn(async move {
            tracing::info!("live stream task started");

            // Start SSE stream (5 second interval)
            let resp = match client.stream(&query, Some(5)).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!("failed to start stream: {}", e);
                    let _ = tx.send(QueryResult {
                        tab_idx,
                        result: Err(e.to_string()),
                        duration: Duration::from_secs(0),
                    });
                    return;
                }
            };

            // Read SSE events from response body
            let mut stream = resp.bytes_stream();

            let mut buffer = String::new();
            let mut accumulated_rows: Vec<Vec<fleet_engine::value::Value>> = Vec::new();
            let columns = existing_columns;

            while let Some(chunk) = stream.next().await {
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::error!("stream error: {}", e);
                        break;
                    }
                };

                buffer.push_str(&String::from_utf8_lossy(&chunk));

                // Parse SSE events from buffer (events are separated by \n\n)
                while let Some(pos) = buffer.find("\n\n") {
                    let event_text = buffer[..pos].to_owned();
                    buffer.drain(..pos + 2);

                    // Parse SSE event structure
                    let mut event_type = None;
                    let mut event_data = None;

                    for line in event_text.lines() {
                        if let Some(event) = line.strip_prefix("event: ") {
                            event_type = Some(event.to_owned());
                        } else if let Some(data) = line.strip_prefix("data: ") {
                            event_data = Some(data.to_owned());
                        }
                    }

                    // Only process "data" events (individual rows from query result)
                    if event_type.as_deref() == Some("data") {
                        if let Some(data) = event_data {
                            // Parse JSON row (array of values)
                            match serde_json::from_str::<Vec<fleet_engine::value::Value>>(&data) {
                                Ok(row) => {
                                    accumulated_rows.push(row);
                                    tracing::debug!(
                                        "received stream row, total: {}",
                                        accumulated_rows.len()
                                    );
                                }
                                Err(e) => {
                                    tracing::warn!("failed to parse stream row: {}", e);
                                }
                            }
                        }
                    } else if event_type.as_deref() == Some("error") {
                        // Handle error events
                        if let Some(data) = event_data {
                            tracing::error!("stream error event: {}", data);
                        }
                    }
                    // Ignore other event types (keep-alive, etc.)
                }

                // Periodically send accumulated rows (every second or when we have enough)
                if !accumulated_rows.is_empty() && accumulated_rows.len() >= 10 {
                    // Use existing columns or generate generic ones
                    let cols = if let Some(ref cols) = columns {
                        cols.clone()
                    } else if !accumulated_rows.is_empty() {
                        // Generate column names (col_0, col_1, etc.)
                        let col_count = accumulated_rows[0].len();
                        (0..col_count)
                            .map(|i| fleet_engine::value::Column {
                                name: format!("col_{i}"),
                            })
                            .collect()
                    } else {
                        Vec::new()
                    };

                    let result = fleet_engine::value::QueryResult {
                        columns: cols,
                        rows: std::mem::take(&mut accumulated_rows),
                    };

                    tracing::debug!("sending batch of {} rows", result.row_count());

                    let _ = tx.send(QueryResult {
                        tab_idx,
                        result: Ok(QueryResponse {
                            result,
                            truncated: false,
                            pagination: fleet_client::PaginationMeta {
                                limit: 0,
                                offset: 0,
                                returned: 0,
                            },
                        }),
                        duration: Duration::from_secs(0),
                    });
                }
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
pub async fn run(config: &Config) -> Result<()> {
    // Load token.
    let token = config.load_token()?;

    // Create HTTP client.
    let client = if config.server.insecure {
        HttpClient::new_insecure(&config.server.url, token)?
    } else {
        HttpClient::new(&config.server.url, token)?
    };

    // Set up terminal.
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen)?;
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
    app.schema_cache = schema;
    app.history_cache = history;
    app.saved_cache = saved;

    // Event loop.
    let result = run_event_loop(&mut terminal, &mut app);

    // Restore terminal.
    disable_raw_mode()?;
    crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

/// Main event loop.
fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<()> {
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
            if let Event::Key(key) = event::read()? {
                app.handle_key(key);
            }
        }

        // Check quit flag.
        if app.should_quit {
            break;
        }
    }

    Ok(())
}
