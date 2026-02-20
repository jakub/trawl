//! TUI application state machine and event loop.

pub mod driver;
pub mod highlight;
pub mod state;
mod ui;

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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
use tokio::sync::mpsc;

use self::driver::{
    DriverCommand, DriverData, DriverResponse, ExecuteWaiter, parse_key_string,
    query_response_to_data,
};
use self::state::{
    ChartView, Focus, LiveBuffer, Popup, ProfiledColumn, ResultsSearch, SchemaView, Sidebar,
    SimpleEditor, Tab, TabStatus,
};
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

/// Analyze query results to compute per-column non-null counts and sample values.
///
/// Only columns with at least one non-null value are included. Results are
/// sorted by population count descending, then name ascending.
fn profile_columns(
    response: &QueryResponse,
    schema_columns: &[(String, String)],
) -> Vec<ProfiledColumn> {
    use std::collections::{HashMap, HashSet};

    let total_rows = response.result.rows.len();
    let col_names: Vec<&str> = response
        .result
        .columns
        .iter()
        .map(|c| c.name.as_str())
        .collect();

    let type_map: HashMap<&str, &str> = schema_columns
        .iter()
        .map(|(n, t)| (n.as_str(), t.as_str()))
        .collect();

    let mut profiled: Vec<ProfiledColumn> = Vec::new();

    for (col_idx, col_name) in col_names.iter().enumerate() {
        let mut non_null = 0usize;
        let mut seen_values: Vec<String> = Vec::new();
        let mut seen_set: HashSet<String> = HashSet::new();

        for row in &response.result.rows {
            if let Some(val) = row.get(col_idx) {
                if *val != fleet_engine::value::Value::Null {
                    non_null += 1;
                    if seen_set.len() < 8 {
                        let s = value_display(val);
                        if seen_set.insert(s.clone()) {
                            seen_values.push(s);
                        }
                    }
                }
            }
        }

        // Skip columns that are entirely null for this service.
        if non_null == 0 {
            continue;
        }

        let data_type = type_map.get(col_name).unwrap_or(&"UNKNOWN").to_string();

        profiled.push(ProfiledColumn {
            name: (*col_name).to_string(),
            data_type,
            non_null_count: non_null,
            total_rows,
            sample_values: seen_values,
        });
    }

    // Sort by population descending, then name ascending for ties.
    profiled.sort_by(|a, b| {
        b.non_null_count
            .cmp(&a.non_null_count)
            .then_with(|| a.name.cmp(&b.name))
    });

    profiled
}

/// Format a value for display as a sample value string.
fn value_display(v: &fleet_engine::value::Value) -> String {
    use fleet_engine::value::Value;
    match v {
        Value::Null => "NULL".to_owned(),
        Value::Boolean(b) => b.to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Float(f) => format!("{f:.2}"),
        Value::String(s) => {
            if s.len() > 30 {
                format!("{}...", &s[..27])
            } else {
                s.clone()
            }
        }
        Value::Array(arr) => format!("[{} items]", arr.len()),
    }
}

/// Error from an async query execution.
#[derive(Debug)]
struct QueryError {
    /// Human-readable error message.
    message: String,
    /// Structured error details with optional span info.
    details: Vec<fleet_client::ErrorDetail>,
}

/// Result of an async query execution.
#[derive(Debug)]
struct QueryResult {
    /// Index of the tab that requested the query.
    tab_idx: usize,
    /// Query execution result.
    result: Result<QueryResponse, QueryError>,
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

/// Result of a background schema profiling query.
#[derive(Debug)]
struct SchemaProfileResult {
    /// Service that was profiled.
    service: String,
    /// Profiled columns + total rows, or error message.
    result: Result<(Vec<ProfiledColumn>, usize), String>,
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
    /// Timezone configuration string for timestamp display.
    pub timezone: String,
    /// Active results search (vim-style `/`).
    pub results_search: Option<ResultsSearch>,
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
    /// Cached service list (fetched at startup via `field_values("service")`).
    pub service_list_cache: Option<Vec<String>>,
    /// Per-service profiled column cache (populated on drill-in).
    schema_profile_cache: std::collections::HashMap<String, Vec<ProfiledColumn>>,
    /// Channel for receiving schema profile results from background tasks.
    schema_profile_rx: mpsc::UnboundedReceiver<SchemaProfileResult>,
    /// Sender for schema profile background tasks.
    schema_profile_tx: mpsc::UnboundedSender<SchemaProfileResult>,
    /// Channel for receiving driver commands from the unix socket.
    driver_rx: Option<mpsc::UnboundedReceiver<DriverCommand>>,
    /// Path to the driver socket (for cleanup on exit).
    driver_socket_path: Option<PathBuf>,
    /// Pending execute waiter: the socket task blocks on this until
    /// the matching tab's query completes.
    driver_execute_waiter: Option<ExecuteWaiter>,
}

impl App {
    /// Create a new app with the given client.
    pub fn new(client: HttpClient) -> Self {
        let (query_tx, query_rx) = mpsc::unbounded_channel();
        let (mutation_tx, mutation_rx) = mpsc::unbounded_channel();
        let (schema_profile_tx, schema_profile_rx) = mpsc::unbounded_channel();

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
            timezone: "local".to_owned(),
            results_search: None,
            max_live_events: 1000,
            live_task: None,
            query_rx,
            query_tx,
            mutation_rx,
            mutation_tx,
            service_list_cache: None,
            schema_profile_cache: std::collections::HashMap::new(),
            schema_profile_rx,
            schema_profile_tx,
            driver_rx: None,
            driver_socket_path: None,
            driver_execute_waiter: None,
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

        // Abort any previously running query on this tab.
        if let Some(handle) = tab.query_task.take() {
            handle.abort();
        }

        // Update tab status to running.
        tab.status = TabStatus::Running {
            start: Instant::now(),
        };

        // Spawn background task to execute query.
        let client = self.client.clone();
        let tx = self.query_tx.clone();
        let tab_idx = self.active_tab_idx;
        let timezone = self.timezone.clone();

        let handle = tokio::spawn(async move {
            tracing::info!("background task started for query: {}", query);
            let start = Instant::now();
            let result = client
                .query_paginated_tz(&query, None, None, Some(timezone))
                .await
                .map_err(|e| QueryError {
                    message: e.to_string(),
                    details: e.error_details().to_vec(),
                });
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

        // Store handle for cancellation.
        self.active_tab_mut().query_task = Some(handle);
    }

    /// Cancel the currently running query on the active tab (if any).
    fn cancel_query(&mut self) {
        let tab = self.active_tab_mut();
        if let Some(handle) = tab.query_task.take() {
            handle.abort();
            tab.status = TabStatus::Error {
                message: "cancelled".to_owned(),
                details: Vec::new(),
            };
            tracing::info!("query cancelled by user");
        }
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
            tab.query_task = None; // Query finished, clear the handle.

            match query_result.result {
                Ok(response) => {
                    // Auto-switch to sparkline view for timechart queries.
                    // Check for _time in any column position (UNION ALL BY NAME
                    // can reorder columns vs. the original SELECT order).
                    let is_timechart = response.result.columns.iter().any(|c| c.name == "_time");
                    if is_timechart && tab.chart_view == ChartView::Table {
                        tab.chart_view = ChartView::Sparkline;
                    }

                    tab.result = Some(response);
                    #[allow(clippy::cast_possible_truncation)] // Query duration < u64::MAX ms
                    let duration_ms = query_result.duration.as_millis() as u64;
                    tab.status = TabStatus::Success { duration_ms };
                    tab.scroll_offset = 0;
                    tab.horizontal_scroll_offset = 0;
                    tab.selected_row = None;
                    tab.column_widths = None;

                    // Notify driver execute waiter if this tab matches.
                    if self
                        .driver_execute_waiter
                        .as_ref()
                        .is_some_and(|w| w.tab_idx == query_result.tab_idx)
                    {
                        let waiter = self.driver_execute_waiter.take().unwrap();
                        let mut data = query_response_to_data(
                            self.tabs[query_result.tab_idx].result.as_ref().unwrap(),
                        );
                        data.duration_ms = Some(duration_ms);
                        let _ = waiter.reply.send(DriverResponse::ok_with(data));
                    }
                }
                Err(ref err) => {
                    // Notify driver execute waiter of failure.
                    if self
                        .driver_execute_waiter
                        .as_ref()
                        .is_some_and(|w| w.tab_idx == query_result.tab_idx)
                    {
                        let waiter = self.driver_execute_waiter.take().unwrap();
                        let _ = waiter.reply.send(DriverResponse::err(&err.message));
                    }
                    tab.status = TabStatus::Error {
                        message: err.message.clone(),
                        details: err.details.clone(),
                    };
                }
            }
        }
    }

    /// Handle a key event.
    #[allow(clippy::too_many_lines)] // Key dispatch is inherently large
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
                self.toggle_sidebar(Sidebar::Help { scroll: 0 });
                return;
            }
            // Toggle schema: F2
            (KeyModifiers::NONE, KeyCode::F(2)) => {
                let view = if let Some(services) = &self.service_list_cache {
                    SchemaView::ServiceList {
                        services: services.clone(),
                        selected: 0,
                    }
                } else {
                    SchemaView::ServiceList {
                        services: Vec::new(),
                        selected: 0,
                    }
                };
                self.toggle_sidebar(Sidebar::Schema(view));
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
            // Close sidebar: Esc (if sidebar is open, but NOT in ServiceDetail — that uses Esc for back)
            (KeyModifiers::NONE, KeyCode::Esc)
                if self.sidebar.is_some()
                    && !matches!(
                        self.sidebar,
                        Some(Sidebar::Schema(SchemaView::ServiceDetail { .. }))
                    ) =>
            {
                self.sidebar = None;
                return;
            }
            // Cancel running query: Esc (if query is running, no sidebar)
            (KeyModifiers::NONE, KeyCode::Esc)
                if matches!(self.active_tab().status, TabStatus::Running { .. }) =>
            {
                self.cancel_query();
                return;
            }
            // New tab: Ctrl+T
            (KeyModifiers::CONTROL, KeyCode::Char('t')) => {
                let new_id = self.tabs.len();
                self.tabs.push(Tab::new(new_id));
                self.active_tab_idx = new_id;
                return;
            }
            // Close tab: Ctrl+W (when NOT in editor focus — editor uses Ctrl+W for kill-word)
            (KeyModifiers::CONTROL, KeyCode::Char('w')) if self.focus != Focus::Editor => {
                if self.tabs.len() > 1 {
                    self.tabs.remove(self.active_tab_idx);
                    if self.active_tab_idx >= self.tabs.len() {
                        self.active_tab_idx = self.tabs.len() - 1;
                    }
                } else {
                    // Last tab: clear instead of closing
                    self.active_tab_mut().clear();
                }
                return;
            }
            // Cycle tabs: Shift+Tab (BackTab) — cycles forward, wrapping around
            (_, KeyCode::BackTab) => {
                if self.tabs.len() > 1 {
                    self.active_tab_idx = (self.active_tab_idx + 1) % self.tabs.len();
                }
                return;
            }
            // Direct tab jump: Alt+1 through Alt+9
            (KeyModifiers::ALT, KeyCode::Char(ch @ '1'..='9')) => {
                let idx = (ch as usize) - ('1' as usize);
                if idx < self.tabs.len() {
                    self.active_tab_idx = idx;
                }
                return;
            }
            // Save current query: Ctrl+S
            (KeyModifiers::CONTROL, KeyCode::Char('s')) => {
                let query = self.active_tab().editor.text();
                if !query.trim().is_empty() {
                    self.popup = Some(Popup::SaveQuery {
                        editor: SimpleEditor::new_single_line(),
                    });
                }
                return;
            }
            _ => {}
        }

        // If sidebar is open, handle sidebar-specific keys.
        if let Some(ref sidebar) = self.sidebar {
            match sidebar {
                Sidebar::Help { .. } => {
                    self.handle_help_key(key);
                    return;
                }
                Sidebar::Schema(_) => {
                    self.handle_schema_key(key);
                    return;
                }
                Sidebar::History => {
                    self.handle_history_key(key);
                    return;
                }
                Sidebar::Saved => {
                    self.handle_saved_key(key);
                    return;
                }
            }
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
    #[allow(clippy::too_many_lines)] // Key dispatch with search mode requires many arms
    fn handle_results_key(&mut self, key: event::KeyEvent) {
        // If search input is active, route keys to the search bar first.
        if self.results_search.as_ref().is_some_and(|s| s.input_active) {
            self.handle_search_input_key(key);
            return;
        }

        let row_count = self
            .active_tab()
            .result
            .as_ref()
            .map_or(0, |r| r.result.row_count());

        match (key.modifiers, key.code) {
            // Switch back to editor
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
            // Page up: move selection by 10
            (KeyModifiers::NONE, KeyCode::PageUp) => {
                if row_count > 0 {
                    let tab = self.active_tab_mut();
                    tab.selected_row = Some(match tab.selected_row {
                        Some(r) => r.saturating_sub(10),
                        None => 0,
                    });
                    self.ensure_selected_row_visible();
                }
            }
            // Page down: move selection by 10
            (KeyModifiers::NONE, KeyCode::PageDown) => {
                if row_count > 0 {
                    let tab = self.active_tab_mut();
                    let max_row = row_count.saturating_sub(1);
                    tab.selected_row = Some(match tab.selected_row {
                        Some(r) => (r + 10).min(max_row),
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
        if let Some(ref mut search) = self.results_search {
            if let Some(ref response) = self.tabs[self.active_tab_idx].result {
                search.update_matches(&response.result);
            }
        }
    }

    /// Jump to the current search match: select its row and scroll to it.
    fn jump_to_current_match(&mut self) {
        if let Some(ref search) = self.results_search {
            if let Some(&(row_idx, _col_idx)) = search.matches.get(search.current_match) {
                self.active_tab_mut().selected_row = Some(row_idx);
                self.ensure_selected_row_visible();
            }
        }
    }

    /// Adjust scroll offset to keep the selected row visible.
    fn ensure_selected_row_visible(&mut self) {
        let tab = self.active_tab_mut();
        if let Some(selected) = tab.selected_row {
            // Estimate visible rows (will be approximate, but good enough)
            let visible_rows = 15; // conservative estimate
            if selected < tab.scroll_offset {
                tab.scroll_offset = selected;
            } else if selected >= tab.scroll_offset + visible_rows {
                tab.scroll_offset = selected - visible_rows + 1;
            }
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

    /// Handle key events when help sidebar is focused.
    fn handle_help_key(&mut self, key: event::KeyEvent) {
        if let Some(Sidebar::Help { ref mut scroll }) = self.sidebar {
            match (key.modifiers, key.code) {
                (KeyModifiers::NONE, KeyCode::Up) => {
                    *scroll = scroll.saturating_sub(1);
                }
                (KeyModifiers::NONE, KeyCode::Down) => {
                    *scroll = scroll.saturating_add(1);
                }
                (KeyModifiers::NONE, KeyCode::PageUp) => {
                    *scroll = scroll.saturating_sub(10);
                }
                (KeyModifiers::NONE, KeyCode::PageDown) => {
                    *scroll = scroll.saturating_add(10);
                }
                (KeyModifiers::NONE, KeyCode::Home) => {
                    *scroll = 0;
                }
                (KeyModifiers::NONE, KeyCode::End) => {
                    *scroll = usize::MAX;
                }
                _ => {}
            }
        }
    }

    /// Handle key events when schema sidebar is focused.
    #[allow(clippy::too_many_lines)]
    fn handle_schema_key(&mut self, key: event::KeyEvent) {
        let Some(Sidebar::Schema(ref mut view)) = self.sidebar else {
            return;
        };

        match view {
            SchemaView::ServiceList { services, selected } => match (key.modifiers, key.code) {
                (KeyModifiers::NONE, KeyCode::Up) => {
                    *selected = selected.saturating_sub(1);
                }
                (KeyModifiers::NONE, KeyCode::Down) => {
                    if !services.is_empty() {
                        *selected = (*selected + 1).min(services.len() - 1);
                    }
                }
                (KeyModifiers::NONE, KeyCode::Home) => *selected = 0,
                (KeyModifiers::NONE, KeyCode::End) => {
                    *selected = services.len().saturating_sub(1);
                }
                (KeyModifiers::NONE, KeyCode::Enter) => {
                    if let Some(svc) = services.get(*selected).cloned() {
                        self.drill_into_service(svc);
                    }
                }
                _ => {}
            },
            SchemaView::Loading { .. } => {
                // No-op while loading (Esc falls through to global handler)
            }
            SchemaView::ServiceDetail {
                columns,
                selected,
                expanded,
                service,
                ..
            } => match (key.modifiers, key.code) {
                (KeyModifiers::NONE, KeyCode::Up) => {
                    *selected = selected.saturating_sub(1);
                }
                (KeyModifiers::NONE, KeyCode::Down) => {
                    if !columns.is_empty() {
                        *selected = (*selected + 1).min(columns.len() - 1);
                    }
                }
                (KeyModifiers::NONE, KeyCode::PageUp) => {
                    *selected = selected.saturating_sub(10);
                }
                (KeyModifiers::NONE, KeyCode::PageDown) => {
                    if !columns.is_empty() {
                        *selected = (*selected + 10).min(columns.len() - 1);
                    }
                }
                (KeyModifiers::NONE, KeyCode::Home) => *selected = 0,
                (KeyModifiers::NONE, KeyCode::End) => {
                    *selected = columns.len().saturating_sub(1);
                }
                // Enter: insert field name at cursor in editor
                (KeyModifiers::NONE, KeyCode::Enter) => {
                    if let Some(col) = columns.get(*selected) {
                        let name = col.name.clone();
                        self.tabs[self.active_tab_idx].editor.insert_text(&name);
                    }
                }
                // Space: toggle sample values expansion
                (KeyModifiers::NONE, KeyCode::Char(' ')) => {
                    if *expanded == Some(*selected) {
                        *expanded = None;
                    } else {
                        *expanded = Some(*selected);
                    }
                }
                // Esc/Backspace: back to service list
                (KeyModifiers::NONE, KeyCode::Esc | KeyCode::Backspace) => {
                    let svc = service.clone();
                    let services = self.service_list_cache.clone().unwrap_or_default();
                    let idx = services.iter().position(|s| *s == svc).unwrap_or(0);
                    self.sidebar = Some(Sidebar::Schema(SchemaView::ServiceList {
                        services,
                        selected: idx,
                    }));
                }
                _ => {}
            },
        }
    }

    /// Drill into a service: check cache or spawn background profiling query.
    fn drill_into_service(&mut self, service: String) {
        // Check cache first.
        if let Some(cached) = self.schema_profile_cache.get(&service) {
            let total_schema_columns = self.schema_cache.as_ref().map_or(0, |s| s.columns.len());
            let total_rows = cached.first().map_or(0, |c| c.total_rows);
            self.sidebar = Some(Sidebar::Schema(SchemaView::ServiceDetail {
                service,
                columns: cached.clone(),
                selected: 0,
                scroll: 0,
                expanded: None,
                total_rows,
                total_schema_columns,
            }));
            return;
        }

        // Set loading state.
        self.sidebar = Some(Sidebar::Schema(SchemaView::Loading {
            service: service.clone(),
        }));

        // Spawn background query.
        let client = self.client.clone();
        let tx = self.schema_profile_tx.clone();
        let schema_columns: Vec<(String, String)> =
            self.schema_cache.as_ref().map_or_else(Vec::new, |s| {
                s.columns
                    .iter()
                    .map(|c| (c.name.clone(), c.data_type.clone()))
                    .collect()
            });
        let svc = service;

        tokio::spawn(async move {
            // Quote service name if it contains spaces or special chars.
            let query = if svc.contains(' ') || svc.contains('"') {
                format!(r#"service:"{}" | head 100"#, svc.replace('"', r#"\""#))
            } else {
                format!("service:{svc} | head 100")
            };

            let result = client.query_paginated(&query, None, None).await;

            let profiled = match result {
                Ok(response) => {
                    let total_rows = response.result.rows.len();
                    let columns = profile_columns(&response, &schema_columns);
                    Ok((columns, total_rows))
                }
                Err(e) => Err(e.to_string()),
            };

            let _ = tx.send(SchemaProfileResult {
                service: svc,
                result: profiled,
            });
        });
    }

    /// Poll for schema profile results from background tasks.
    fn poll_schema_profiles(&mut self) {
        while let Ok(result) = self.schema_profile_rx.try_recv() {
            match result.result {
                Ok((columns, total_rows)) => {
                    // Cache the result.
                    self.schema_profile_cache
                        .insert(result.service.clone(), columns.clone());

                    let total_schema_columns =
                        self.schema_cache.as_ref().map_or(0, |s| s.columns.len());

                    // If we're still in Loading state for this service, transition.
                    if matches!(
                        self.sidebar,
                        Some(Sidebar::Schema(SchemaView::Loading { ref service }))
                        if *service == result.service
                    ) {
                        self.sidebar = Some(Sidebar::Schema(SchemaView::ServiceDetail {
                            service: result.service,
                            columns,
                            selected: 0,
                            scroll: 0,
                            expanded: None,
                            total_rows,
                            total_schema_columns,
                        }));
                    }
                }
                Err(msg) => {
                    tracing::error!("schema profile failed for {}: {}", result.service, msg);
                    // Go back to service list on error.
                    if matches!(
                        self.sidebar,
                        Some(Sidebar::Schema(SchemaView::Loading { ref service }))
                        if *service == result.service
                    ) {
                        let services = self.service_list_cache.clone().unwrap_or_default();
                        self.sidebar = Some(Sidebar::Schema(SchemaView::ServiceList {
                            services,
                            selected: 0,
                        }));
                    }
                }
            }
        }
    }

    /// Handle key events when a popup is open.
    #[allow(clippy::too_many_lines)] // Inherently large popup dispatch
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
                Popup::SaveQuery { .. } => {
                    self.handle_save_query_key(key);
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
                        (KeyModifiers::NONE, KeyCode::Char('[')) => {
                            if row_index > 0 {
                                let new_idx = row_index - 1;
                                self.active_tab_mut().selected_row = Some(new_idx);
                                self.popup = Some(Popup::EventDetail {
                                    row_index: new_idx,
                                    scroll: 0,
                                });
                            }
                        }
                        // Navigate to next row
                        (KeyModifiers::NONE, KeyCode::Char(']')) => {
                            if row_index + 1 < row_count {
                                let new_idx = row_index + 1;
                                self.active_tab_mut().selected_row = Some(new_idx);
                                self.popup = Some(Popup::EventDetail {
                                    row_index: new_idx,
                                    scroll: 0,
                                });
                            }
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
                    if !name.is_empty() {
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
                    match key.code {
                        KeyCode::Char(ch) => editor.insert_char(ch),
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
        if self
            .sidebar
            .as_ref()
            .is_some_and(|s| s.same_variant(&sidebar))
        {
            self.sidebar = None;
        } else {
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
    #[allow(clippy::too_many_lines)]
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

        // Extract field order from the DSL so the live buffer preserves it.
        let field_order = extract_field_order(&query);

        // Resolve timezone offset for timestamp display in streaming events.
        let utc_offset_secs =
            fleet_engine::timezone::resolve_utc_offset(&self.timezone).unwrap_or(0);

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
                        result: Err(QueryError {
                            message: e.to_string(),
                            details: e.error_details().to_vec(),
                        }),
                        duration: Duration::from_secs(0),
                    });
                    return;
                }
            };

            let mut buffer = LiveBuffer::new(max_events);
            if !field_order.is_empty() {
                buffer = buffer.with_column_order(field_order);
            }
            let mut event_count = 0usize;

            while let Some(event) = stream.next().await {
                match event {
                    Ok(StreamEvent::Event(mut map)) => {
                        apply_tz_to_event(&mut map, utc_offset_secs);
                        buffer.push_event(&map);
                        event_count += 1;
                        tracing::debug!("received stream event, total: {event_count}");
                    }
                    Ok(StreamEvent::Snapshot {
                        ref columns,
                        ref rows,
                    }) => {
                        let rows: Vec<_> = rows
                            .iter()
                            .cloned()
                            .map(|mut row| {
                                apply_tz_to_event(&mut row, utc_offset_secs);
                                row
                            })
                            .collect();
                        buffer.replace_with_snapshot(columns, &rows);
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

    // -- Driver integration ---------------------------------------------------

    /// Start the driver socket listener if a path is provided.
    fn start_driver(&mut self, path: &Path) {
        match driver::spawn_listener(path) {
            Ok(rx) => {
                self.driver_rx = Some(rx);
                self.driver_socket_path = Some(path.to_owned());
                tracing::info!("driver started at {}", path.display());
            }
            Err(e) => {
                tracing::error!("failed to start driver socket: {e}");
            }
        }
    }

    /// Clean up the driver socket file.
    fn cleanup_driver(&mut self) {
        if let Some(ref path) = self.driver_socket_path {
            let _ = std::fs::remove_file(path);
            tracing::info!("cleaned up driver socket at {}", path.display());
        }
    }

    /// Process pending driver commands (up to 10 per tick to avoid starving UI).
    fn poll_driver_commands<B: ratatui::backend::Backend>(&mut self, terminal: &mut Terminal<B>) {
        // Take the receiver out to avoid borrow conflicts with &mut self.
        let Some(mut rx) = self.driver_rx.take() else {
            return;
        };

        for _ in 0..10 {
            let Ok(cmd) = rx.try_recv() else { break };

            tracing::debug!("processing driver command: {:?}", cmd.request);

            match cmd.request {
                driver::DriverRequest::Status => {
                    let resp = self.handle_driver_status();
                    let _ = cmd.reply.send(resp);
                }
                driver::DriverRequest::SetQuery { query } => {
                    self.handle_driver_set_query(&query);
                    let _ = cmd.reply.send(DriverResponse::ok());
                }
                driver::DriverRequest::Execute { .. } => {
                    self.handle_driver_execute(cmd.reply);
                    // Don't send reply here — it's deferred until query completes.
                }
                driver::DriverRequest::Capture { width, height } => {
                    let resp = self.handle_driver_capture(terminal, width, height);
                    let _ = cmd.reply.send(resp);
                }
                driver::DriverRequest::Key { key } => {
                    let resp = self.handle_driver_key(&key);
                    let _ = cmd.reply.send(resp);
                }
                driver::DriverRequest::Keys { keys } => {
                    let resp = self.handle_driver_keys(&keys);
                    let _ = cmd.reply.send(resp);
                }
                driver::DriverRequest::GetResults { tab } => {
                    let resp = self.handle_driver_get_results(tab);
                    let _ = cmd.reply.send(resp);
                }
                driver::DriverRequest::Quit => {
                    let _ = cmd.reply.send(DriverResponse::ok());
                    self.should_quit = true;
                }
            }
        }

        // Put the receiver back.
        self.driver_rx = Some(rx);
    }

    fn handle_driver_status(&self) -> DriverResponse {
        let tab = self.active_tab();
        let tab_status = match &tab.status {
            TabStatus::Idle => "idle",
            TabStatus::Running { .. } => "running",
            TabStatus::Success { .. } => "success",
            TabStatus::Error { .. } => "error",
        };
        let (result_rows, result_columns) = match &tab.result {
            Some(r) => (
                Some(r.result.row_count()),
                Some(r.result.columns.iter().map(|c| c.name.clone()).collect()),
            ),
            None => (None, None),
        };

        DriverResponse::ok_with(DriverData {
            focus: Some(format!("{:?}", self.focus).to_lowercase()),
            tab_count: Some(self.tabs.len()),
            active_tab: Some(self.active_tab_idx),
            tab_status: Some(tab_status.to_owned()),
            query: Some(tab.editor.text()),
            live_mode: Some(self.live_mode),
            result_rows,
            result_columns,
            ..DriverData::default()
        })
    }

    fn handle_driver_set_query(&mut self, query: &str) {
        let tab = self.active_tab_mut();
        tab.editor.clear();
        tab.editor.insert_text(query);
    }

    fn handle_driver_execute(&mut self, reply: tokio::sync::oneshot::Sender<DriverResponse>) {
        // If there's already a waiter, reject.
        if self.driver_execute_waiter.is_some() {
            let _ = reply.send(DriverResponse::err("another execute is already pending"));
            return;
        }

        // If the editor is empty, reject.
        if self.active_tab().editor.text().trim().is_empty() {
            let _ = reply.send(DriverResponse::err("empty query"));
            return;
        }

        let tab_idx = self.active_tab_idx;
        self.driver_execute_waiter = Some(ExecuteWaiter { tab_idx, reply });

        // Trigger query execution (same as F5 / ctrl+enter).
        self.execute_query();
    }

    fn handle_driver_capture<B: ratatui::backend::Backend>(
        &mut self,
        _real_terminal: &mut Terminal<B>,
        width: Option<u16>,
        height: Option<u16>,
    ) -> DriverResponse {
        let w = width.unwrap_or(120);
        let h = height.unwrap_or(40);

        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut test_terminal = match Terminal::new(backend) {
            Ok(t) => t,
            Err(e) => return DriverResponse::err(format!("failed to create test terminal: {e}")),
        };

        if let Err(e) = test_terminal.draw(|f| ui::render(self, f)) {
            return DriverResponse::err(format!("render failed: {e}"));
        }

        let content = test_terminal.backend().to_string();

        DriverResponse::ok_with(DriverData {
            content: Some(content),
            width: Some(w),
            height: Some(h),
            ..DriverData::default()
        })
    }

    fn handle_driver_key(&mut self, key: &str) -> DriverResponse {
        match parse_key_string(key) {
            Ok(key_event) => {
                self.handle_key(key_event);
                DriverResponse::ok()
            }
            Err(e) => DriverResponse::err(e),
        }
    }

    fn handle_driver_keys(&mut self, keys: &[String]) -> DriverResponse {
        for key_str in keys {
            match parse_key_string(key_str) {
                Ok(key_event) => self.handle_key(key_event),
                Err(e) => return DriverResponse::err(format!("key '{key_str}': {e}")),
            }
        }
        DriverResponse::ok()
    }

    fn handle_driver_get_results(&self, tab: Option<usize>) -> DriverResponse {
        let tab_idx = tab.unwrap_or(self.active_tab_idx);
        if tab_idx >= self.tabs.len() {
            return DriverResponse::err(format!("tab index {tab_idx} out of range"));
        }

        match &self.tabs[tab_idx].result {
            Some(response) => DriverResponse::ok_with(query_response_to_data(response)),
            None => DriverResponse::ok_with(DriverData {
                row_count: Some(0),
                columns: Some(Vec::new()),
                rows: Some(Vec::new()),
                truncated: Some(false),
                ..DriverData::default()
            }),
        }
    }
}

/// Run the TUI application.
pub async fn run(
    config: &Config,
    direct_token: Option<&str>,
    driver_path: Option<&Path>,
) -> Result<(), CliError> {
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

    // Fetch schema, history, saved queries, and service list in parallel.
    tracing::info!("fetching schema, history, saved queries, and service list");
    let (schema_result, history_result, saved_result, services_result) = tokio::join!(
        client.schema(),
        client.history(Some(100), None),
        client.list_saved(),
        client.field_values("service", Some(500)),
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

    let services = match services_result {
        Ok(fv) => {
            tracing::info!("service list fetched: {} services", fv.values.len());
            Some(fv.values)
        }
        Err(e) => {
            tracing::warn!("failed to fetch service list: {}", e);
            None
        }
    };

    // Create app with schema, history, and saved queries.
    let mut app = App::new(client);
    app.max_live_events = config.tail.max_events;
    app.enter_executes = config.ui.enter_executes;
    app.timezone = config.ui.timezone.clone();
    app.schema_cache = schema;
    app.history_cache = history;
    app.saved_cache = saved;
    app.service_list_cache = services;

    // Start driver socket if requested.
    if let Some(path) = driver_path {
        app.start_driver(path);
    }

    // Event loop.
    let result = run_event_loop(&mut terminal, &mut app);

    // Clean up driver socket.
    app.cleanup_driver();

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

/// Extract the user-specified field order from a `fields`/`table` pipe stage.
///
/// Returns the mapped column names in user-specified order, or empty vec if
/// no table stage is present (or the query fails to parse).
fn extract_field_order(query: &str) -> Vec<String> {
    let Ok(ast) = fleet_core::parser::parse(query) else {
        return Vec::new();
    };
    for stage in &ast.pipeline {
        if let fleet_core::ast::PipeStage::Table(t) = &stage.node {
            return t
                .fields
                .iter()
                .map(|f| fleet_core::emitter::map_field_name(f).to_string())
                .collect();
        }
    }
    Vec::new()
}

/// Apply timezone offset to timestamp-valued fields in a streaming event.
///
/// Converts RFC 3339 UTC strings to the display format used by the query
/// executor, with the configured UTC offset applied. Handles both the
/// standard `timestamp` field and timechart's `_time` bucket field.
fn apply_tz_to_event(event: &mut serde_json::Map<String, serde_json::Value>, utc_offset_secs: i32) {
    if utc_offset_secs == 0 {
        return;
    }
    for key in &["timestamp", "_time"] {
        if let Some(serde_json::Value::String(ts)) = event.get(*key) {
            let converted = fleet_engine::timezone::reformat_rfc3339(ts, utc_offset_secs);
            event.insert((*key).to_owned(), serde_json::Value::String(converted));
        }
    }
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

        // Poll for schema profile results from background tasks.
        app.poll_schema_profiles();

        // Poll for driver commands from the unix socket.
        app.poll_driver_commands(terminal);

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
        assert_eq!(app.sidebar, Some(Sidebar::Help { scroll: 0 }));
        app.handle_key(key(KeyCode::F(1)));
        assert_eq!(app.sidebar, None);
    }

    #[test]
    fn key_f2_toggles_schema() {
        let mut app = test_app();
        app.handle_key(key(KeyCode::F(2)));
        assert!(matches!(
            app.sidebar,
            Some(Sidebar::Schema(SchemaView::ServiceList { .. }))
        ));
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
        app.sidebar = Some(Sidebar::Help { scroll: 0 });
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.sidebar, None);
    }

    #[test]
    fn sidebar_blocks_focus_keys() {
        let mut app = test_app();
        app.sidebar = Some(Sidebar::Help { scroll: 0 });
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
    fn key_ctrl_w_closes_tab_from_results() {
        let mut app = test_app();
        // Create a second tab first
        app.handle_key(key_mod(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert_eq!(app.tabs.len(), 2);
        // Switch to results focus so Ctrl+W closes tab (not kill-word)
        app.focus = Focus::Results;
        app.handle_key(key_mod(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(app.tabs.len(), 1);
    }

    #[test]
    fn key_ctrl_w_in_editor_does_not_close_tab() {
        let mut app = test_app();
        app.handle_key(key_mod(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert_eq!(app.tabs.len(), 2);
        // In editor focus, Ctrl+W is kill-word, not close tab
        app.focus = Focus::Editor;
        app.handle_key(key_mod(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(app.tabs.len(), 2);
    }

    #[test]
    fn key_ctrl_w_with_one_tab_clears() {
        let mut app = test_app();
        // Type something so we can verify it gets cleared
        app.handle_key(key(KeyCode::Char('x')));
        assert_eq!(app.active_tab().editor.text(), "x");
        app.focus = Focus::Results;
        app.handle_key(key_mod(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.active_tab().editor.text(), "");
    }

    #[test]
    fn key_shift_tab_cycles_tabs() {
        let mut app = test_app();
        // Create 3 tabs total
        app.handle_key(key_mod(KeyCode::Char('t'), KeyModifiers::CONTROL));
        app.handle_key(key_mod(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert_eq!(app.tabs.len(), 3);
        assert_eq!(app.active_tab_idx, 2);

        // Cycle forward: 2 -> 0
        app.handle_key(key_mod(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(app.active_tab_idx, 0);

        // Cycle forward: 0 -> 1
        app.handle_key(key_mod(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(app.active_tab_idx, 1);
    }

    #[test]
    fn key_alt_number_jumps_to_tab() {
        let mut app = test_app();
        // Create 3 tabs total
        app.handle_key(key_mod(KeyCode::Char('t'), KeyModifiers::CONTROL));
        app.handle_key(key_mod(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert_eq!(app.active_tab_idx, 2);

        // Alt+1 -> tab 0
        app.handle_key(key_mod(KeyCode::Char('1'), KeyModifiers::ALT));
        assert_eq!(app.active_tab_idx, 0);

        // Alt+3 -> tab 2
        app.handle_key(key_mod(KeyCode::Char('3'), KeyModifiers::ALT));
        assert_eq!(app.active_tab_idx, 2);

        // Alt+9 -> out of range, no change
        app.handle_key(key_mod(KeyCode::Char('9'), KeyModifiers::ALT));
        assert_eq!(app.active_tab_idx, 2);
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
        assert!(matches!(app.popup, Some(Popup::SaveQuery { .. })));
    }

    #[test]
    fn key_ctrl_s_with_empty_editor_is_noop() {
        let mut app = test_app();
        app.handle_key(key_mod(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert!(app.popup.is_none());
    }

    #[test]
    fn popup_blocks_global_keys() {
        let mut app = test_app();
        app.popup = Some(Popup::SaveQuery {
            editor: SimpleEditor::new_single_line(),
        });
        // F1 should NOT open sidebar while popup is active
        app.handle_key(key(KeyCode::F(1)));
        assert!(app.sidebar.is_none());
        assert!(app.popup.is_some());
    }

    #[test]
    fn popup_esc_closes() {
        let mut app = test_app();
        app.popup = Some(Popup::SaveQuery {
            editor: SimpleEditor::new_single_line(),
        });
        app.handle_key(key(KeyCode::Esc));
        assert!(app.popup.is_none());
    }

    #[test]
    fn popup_typing_appends() {
        let mut app = test_app();
        app.popup = Some(Popup::SaveQuery {
            editor: SimpleEditor::new_single_line(),
        });
        app.handle_key(key(KeyCode::Char('a')));
        app.handle_key(key(KeyCode::Char('b')));
        if let Some(Popup::SaveQuery { ref editor }) = app.popup {
            assert_eq!(editor.text(), "ab");
        } else {
            panic!("expected SaveQuery popup");
        }
    }

    #[test]
    fn popup_backspace_removes() {
        let mut app = test_app();
        let mut editor = SimpleEditor::new_single_line();
        editor.insert_text("abc");
        app.popup = Some(Popup::SaveQuery { editor });
        app.handle_key(key(KeyCode::Backspace));
        if let Some(Popup::SaveQuery { ref editor }) = app.popup {
            assert_eq!(editor.text(), "ab");
        } else {
            panic!("expected SaveQuery popup");
        }
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
    fn results_keys_select_rows() {
        let mut app = test_app();
        app.focus = Focus::Results;
        // Give it some result data so row selection works
        app.tabs[0].result = Some(make_query_response(
            vec!["x"],
            vec![
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
                vec![Value::Integer(3)],
            ],
        ));
        assert_eq!(app.active_tab().selected_row, None);
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.active_tab().selected_row, Some(0));
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.active_tab().selected_row, Some(1));
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.active_tab().selected_row, Some(0));
        // Esc deselects
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.active_tab().selected_row, None);
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
                result: Err(QueryError {
                    message: "something broke".to_owned(),
                    details: Vec::new(),
                }),
                duration: Duration::from_millis(10),
            })
            .unwrap();

        app.poll_query_results();

        assert!(matches!(
            app.tabs[0].status,
            TabStatus::Error { ref message, .. } if message == "something broke"
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
