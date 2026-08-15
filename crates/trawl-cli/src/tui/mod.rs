// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! TUI application state machine and event loop.

mod autocomplete;
pub mod driver;
mod handlers;
pub mod highlight;
mod live;
pub mod palette;
pub mod state;
pub mod theme;
mod ui;

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEventKind, KeyModifiers,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;
use trawl_client::{HistoryResponse, HttpClient, ListSavedResponse, QueryResponse, SchemaResponse};

use self::driver::{DriverCommand, DriverResponse, ExecuteWaiter, query_response_to_data};
use self::state::{
    ChartView, ColumnConfig, DashboardState, Focus, LayoutAreas, MainTab, PanelState, Popup,
    ResultsSearch, SchemaBrowser, SimpleEditor, Tab, TabStatus,
};
use crate::CliError;
use crate::config::Config;

/// Get text from the system clipboard. Returns `None` if clipboard is unavailable.
fn clipboard_get() -> Option<String> {
    #[cfg(feature = "clipboard")]
    {
        arboard::Clipboard::new().ok()?.get_text().ok()
    }
    #[cfg(not(feature = "clipboard"))]
    {
        None
    }
}

/// Set text on the system clipboard. Silently fails if clipboard is unavailable.
fn clipboard_set(text: &str) {
    #[cfg(feature = "clipboard")]
    if let Ok(mut cb) = arboard::Clipboard::new() {
        let _ = cb.set_text(text.to_owned());
    }
    #[cfg(not(feature = "clipboard"))]
    let _ = text;
}

/// Error from an async query execution.
#[derive(Debug)]
pub(super) struct QueryError {
    /// Human-readable error message.
    pub(super) message: String,
    /// Structured error details with optional span info.
    pub(super) details: Vec<trawl_client::ErrorDetail>,
}

/// Result of an async query execution.
#[derive(Debug)]
pub(super) struct QueryResult {
    /// Query execution result.
    pub(super) result: Result<QueryResponse, QueryError>,
    /// Execution duration.
    pub(super) duration: Duration,
}

/// Result of a mutation operation (save/delete saved query).
#[derive(Debug)]
pub(super) enum MutationResult {
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
    /// Schedule was set on a saved query.
    ScheduleSet { saved_query_id: i64 },
    /// Dashboard snapshot received from server (Err on failure — preserves cached data).
    DashboardUpdate(Result<Box<trawl_client::DashboardSnapshot>, String>),
    /// Report runs loaded for a saved query.
    RunsLoaded {
        saved_id: i64,
        runs: Vec<trawl_client::ReportRunSummary>,
        total: usize,
    },
    /// A specific run's result data loaded.
    RunResultLoaded {
        result: trawl_api::value::QueryResult,
    },
    /// Mutation failed.
    Error { message: String },
}

/// Main TUI application state.
pub struct App {
    /// HTTP client for API calls.
    pub client: HttpClient,
    /// The query tab (editor + results).
    pub tab: Tab,
    /// Which top-level navigation tab is active.
    pub main_tab: MainTab,
    /// Which pane has focus.
    pub focus: Focus,
    /// Saved focus state for the Query tab (preserved across tab switches).
    query_focus: Focus,
    /// Panel state for non-Query tabs (schema tree, history/saved selection).
    pub panel: PanelState,
    /// Active popup (if any).
    pub popup: Option<Popup>,
    /// Cached schema response (fetched at startup).
    pub schema_cache: Option<SchemaResponse>,
    /// Cached history response (fetched at startup).
    pub history_cache: Option<HistoryResponse>,
    /// Cached saved queries (fetched at startup).
    pub saved_cache: Option<ListSavedResponse>,
    /// Whether to quit the application.
    pub should_quit: bool,
    /// Whether live tail mode is active.
    pub live_mode: bool,
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
    /// Cache of sample values for (field, optional service) pairs.
    #[allow(dead_code)] // Populated in phase 6 detail pane
    pub sample_values_cache: std::collections::HashMap<(String, Option<String>), Vec<String>>,
    /// Channel for receiving driver commands from the unix socket.
    driver_rx: Option<mpsc::UnboundedReceiver<DriverCommand>>,
    /// Path to the driver socket (for cleanup on exit).
    driver_socket_path: Option<PathBuf>,
    /// Pending execute waiter: the socket task blocks on this until
    /// the matching tab's query completes.
    driver_execute_waiter: Option<ExecuteWaiter>,
    /// Server version string (fetched from health endpoint at startup).
    pub server_version: Option<String>,
    /// Admin dashboard state (permissions, polling, cache).
    pub dashboard: DashboardState,
    /// Active color theme.
    pub theme: theme::Theme,
    /// Cached layout areas from last render (for mouse hit-testing).
    pub layout: LayoutAreas,
    /// Whether mouse capture is enabled (from config).
    pub mouse_enabled: bool,
}

impl App {
    /// Create a new app with the given client.
    pub fn new(client: HttpClient) -> Self {
        let (query_tx, query_rx) = mpsc::unbounded_channel();
        let (mutation_tx, mutation_rx) = mpsc::unbounded_channel();

        Self {
            client,
            tab: Tab::new(),
            main_tab: MainTab::Query,
            focus: Focus::Editor,
            query_focus: Focus::Editor,
            panel: PanelState::new(),
            popup: None,
            schema_cache: None,
            history_cache: None,
            saved_cache: None,
            should_quit: false,
            live_mode: false,
            timezone: "local".to_owned(),
            results_search: None,
            max_live_events: 1000,
            live_task: None,
            query_rx,
            query_tx,
            mutation_rx,
            mutation_tx,
            sample_values_cache: std::collections::HashMap::new(),
            driver_rx: None,
            driver_socket_path: None,
            driver_execute_waiter: None,
            server_version: None,
            dashboard: DashboardState::new(),
            theme: theme::dark(),
            layout: LayoutAreas::default(),
            mouse_enabled: false,
        }
    }

    /// Get the query tab.
    pub fn active_tab(&self) -> &Tab {
        &self.tab
    }

    /// Get the query tab mutably.
    pub fn active_tab_mut(&mut self) -> &mut Tab {
        &mut self.tab
    }

    /// Execute a query in the background.
    pub fn execute_query(&mut self) {
        // Stop live streaming if active (user is running a new/edited query).
        if self.live_mode {
            self.stop_live_stream();
        }

        // Auto-format the editor before executing (best-effort, no-op on
        // parse failure). Uses the same reformat path as Ctrl+F.
        self.format_editor_query();

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

            let _ = tx.send(QueryResult { result, duration });
        });

        // Store handle for cancellation.
        self.active_tab_mut().query_task = Some(handle);
    }

    /// Format the editor query in-place using the canonical DSL formatter.
    ///
    /// No-op if the editor is empty or the query fails to parse.
    /// Saves an undo snapshot so `Ctrl+Z` reverts the format.
    pub(super) fn format_editor_query(&mut self) {
        let tab = self.active_tab_mut();
        let text = tab.editor.text();
        if text.trim().is_empty() {
            return;
        }
        if let Some(formatted) = trawl_core::format::reformat(&text)
            && formatted != text
        {
            tab.editor.save_snapshot();
            tab.editor.clear();
            tab.editor.insert_text(&formatted);
            tab.mark_editor_dirty();
        }
    }

    /// Cancel the currently running query on the active tab (if any).
    pub(super) fn cancel_query(&mut self) {
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

    /// Fetch report runs for a saved query (async, results arrive via mutation channel).
    fn fetch_runs(&self, saved_id: i64) {
        let client = self.client.clone();
        let mutation_tx = self.mutation_tx.clone();
        tokio::spawn(async move {
            let result = match client.list_report_runs(saved_id, Some(50), None).await {
                Ok(resp) => MutationResult::RunsLoaded {
                    saved_id,
                    runs: resp.runs,
                    total: resp.total,
                },
                Err(e) => {
                    tracing::error!("failed to fetch runs for saved query {saved_id}: {e}");
                    MutationResult::Error {
                        message: format!("Failed to fetch runs: {e}"),
                    }
                }
            };
            let _ = mutation_tx.send(result);
        });
    }

    /// Fetch a specific run's result data (async, results arrive via mutation channel).
    fn fetch_run_result(&self, saved_id: i64, run_id: i64) {
        let client = self.client.clone();
        let mutation_tx = self.mutation_tx.clone();
        tokio::spawn(async move {
            let result = match client.get_report_run(saved_id, run_id).await {
                Ok(resp) => {
                    if let Some(qr) = resp.result {
                        MutationResult::RunResultLoaded { result: qr }
                    } else if resp.summary.row_count == Some(0) {
                        // Successful run with 0 rows — show empty result, not an error.
                        MutationResult::RunResultLoaded {
                            result: trawl_api::value::QueryResult::empty(),
                        }
                    } else {
                        MutationResult::Error {
                            message: "Run has no result data".to_owned(),
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("failed to fetch run result: {e}");
                    MutationResult::Error {
                        message: format!("Failed to fetch run result: {e}"),
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
                    // Switch to Saved tab to show the new query
                    self.main_tab = MainTab::Saved;
                    self.focus = Focus::Panel;
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
                    if let Some(name) = select_name
                        && let Some(idx) = saved.queries.iter().position(|q| q.name == name)
                    {
                        self.panel.saved_selected = idx;
                    }

                    self.saved_cache = Some(saved);

                    // Reset selection if it's now out of bounds
                    if let Some(cache) = &self.saved_cache {
                        let selected = self.panel.saved_selected;
                        if selected >= cache.queries.len() && !cache.queries.is_empty() {
                            self.panel.saved_selected = cache.queries.len().saturating_sub(1);
                        }
                    }
                }
                MutationResult::ScheduleSet { saved_query_id } => {
                    tracing::info!("schedule set for saved query {saved_query_id}");
                    self.refresh_saved_cache(None);
                }
                MutationResult::DashboardUpdate(result) => {
                    match result {
                        Ok(snapshot) => {
                            self.dashboard.cache = Some(*snapshot);
                            self.dashboard.last_error = None;
                        }
                        Err(msg) => {
                            self.dashboard.last_error = Some(msg);
                        }
                    }
                    self.dashboard.clear_inflight();
                }
                MutationResult::RunsLoaded {
                    saved_id,
                    runs,
                    total,
                } => {
                    tracing::debug!("loaded {} runs for saved query {saved_id}", runs.len());
                    self.panel.saved_detail = Some(state::SavedDetailState {
                        saved_id,
                        runs,
                        total_runs: total,
                        run_selected: 0,
                        run_scroll: 0,
                        result: None,
                        result_scroll: 0,
                        loading: false,
                    });
                    self.panel.saved_focus = state::SavedFocus::Detail;
                }
                MutationResult::RunResultLoaded { result } => {
                    tracing::debug!("loaded run result data");
                    if let Some(ref mut detail) = self.panel.saved_detail {
                        detail.result = Some(result);
                        detail.result_scroll = 0;
                    }
                    self.panel.saved_focus = state::SavedFocus::RunResults;
                }
                MutationResult::Error { message } => {
                    tracing::error!("mutation error: {message}");
                    self.popup = Some(Popup::Error { message });
                }
            }
        }
    }

    /// Poll the dashboard endpoint when the admin user is viewing the Dashboard tab.
    fn poll_dashboard(&mut self) {
        if !self.dashboard.is_admin || self.main_tab != MainTab::Dashboard {
            return;
        }
        // Reset inflight guard if stuck >10s (e.g. spawned task panicked).
        if !self
            .dashboard
            .check_inflight_timeout(Duration::from_secs(10))
        {
            return;
        }
        // 10 ticks × 100ms poll interval = ~1s refresh
        if !self.dashboard.tick() {
            return;
        }
        self.dashboard.set_inflight();

        let client = self.client.clone();
        let tx = self.mutation_tx.clone();
        tokio::spawn(async move {
            let result = client
                .dashboard()
                .await
                .map(Box::new)
                .map_err(|e| e.to_string());
            let _ = tx.send(MutationResult::DashboardUpdate(result));
        });
    }

    /// Poll for query results and update the tab.
    pub fn poll_query_results(&mut self) {
        while let Ok(query_result) = self.query_rx.try_recv() {
            tracing::info!("received query result");

            let tab = &mut self.tab;
            tab.query_task = None; // Query finished, clear the handle.

            match query_result.result {
                Ok(response) => {
                    // Auto-switch to line chart view for timechart queries.
                    let is_timechart = response.result.columns.iter().any(|c| c.name == "_time");
                    if is_timechart && tab.chart_view == ChartView::Table {
                        tab.chart_view = ChartView::LineChart;
                    }

                    tab.result = Some(response);
                    #[allow(clippy::cast_possible_truncation)] // Query duration < u64::MAX ms
                    let duration_ms = query_result.duration.as_millis() as u64;
                    tab.status = TabStatus::Success { duration_ms };
                    tab.scroll_offset = 0;
                    tab.horizontal_scroll_offset = 0;
                    tab.selected_row = None;
                    tab.column_config = Some(ColumnConfig::init(
                        &tab.result.as_ref().unwrap().result.columns,
                    ));
                    tab.validation_errors.clear();

                    // Notify driver execute waiter.
                    if self.driver_execute_waiter.is_some() {
                        let waiter = self.driver_execute_waiter.take().unwrap();
                        let mut data = query_response_to_data(self.tab.result.as_ref().unwrap());
                        data.duration_ms = Some(duration_ms);
                        let _ = waiter.reply.send(DriverResponse::ok_with(data));
                    }
                }
                Err(ref err) => {
                    // Notify driver execute waiter of failure.
                    if self.driver_execute_waiter.is_some() {
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
    #[allow(clippy::too_many_lines)]
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
            // F1 → help popup
            (KeyModifiers::NONE, KeyCode::F(1)) => {
                if matches!(self.popup, Some(Popup::Help { .. })) {
                    self.popup = None;
                } else {
                    self.popup = Some(Popup::Help { scroll: 0 });
                }
                return;
            }
            // Tab switching: Alt+1 through Alt+4
            (KeyModifiers::ALT, KeyCode::Char('1')) => {
                self.switch_to_main_tab(MainTab::Query);
                return;
            }
            (KeyModifiers::ALT, KeyCode::Char('2')) => {
                self.switch_to_main_tab(MainTab::History);
                return;
            }
            (KeyModifiers::ALT, KeyCode::Char('3')) => {
                self.switch_to_main_tab(MainTab::Schema);
                return;
            }
            (KeyModifiers::ALT, KeyCode::Char('4')) => {
                self.switch_to_main_tab(MainTab::Saved);
                return;
            }
            (KeyModifiers::ALT, KeyCode::Char('5')) if self.dashboard.is_admin => {
                self.switch_to_main_tab(MainTab::Dashboard);
                return;
            }
            // Toggle live tail: F9
            (KeyModifiers::NONE, KeyCode::F(9)) => {
                self.toggle_live_mode();
                return;
            }
            // Command palette: Ctrl+P
            (KeyModifiers::CONTROL, KeyCode::Char('p')) => {
                let items = palette::build_palette_items(
                    self.dashboard.is_admin,
                    self.saved_cache.as_ref(),
                    self.history_cache.as_ref(),
                    self.panel.schema.as_ref(),
                );
                let filtered = palette::refilter("", &items);
                self.popup = Some(Popup::CommandPalette {
                    input: String::new(),
                    cursor: 0,
                    selected: 0,
                    scroll: 0,
                    items,
                    filtered,
                    ghost: None,
                });
                return;
            }
            // Esc: on panel tabs, switch back to Query; on Query, cancel running query
            (KeyModifiers::NONE, KeyCode::Esc) if self.main_tab != MainTab::Query => {
                // If schema filter is active, close filter first
                if self.main_tab == MainTab::Schema
                    && self.panel.schema.as_ref().is_some_and(|s| s.filter_active)
                {
                    if let Some(schema) = self.panel.schema.as_mut() {
                        schema.filter_active = false;
                        schema.filter.clear();
                    }
                    return;
                }
                // Saved tab: navigate focus back before leaving the tab
                if self.main_tab == MainTab::Saved {
                    match self.panel.saved_focus {
                        state::SavedFocus::RunResults => {
                            // Back to detail view, clear loaded result
                            if let Some(ref mut detail) = self.panel.saved_detail {
                                detail.result = None;
                                detail.result_scroll = 0;
                            }
                            self.panel.saved_focus = state::SavedFocus::Detail;
                            return;
                        }
                        state::SavedFocus::Detail => {
                            // Back to list, clear detail state
                            self.panel.saved_detail = None;
                            self.panel.saved_focus = state::SavedFocus::List;
                            return;
                        }
                        state::SavedFocus::List => {} // fall through to switch to Query
                    }
                }
                self.switch_to_main_tab(MainTab::Query);
                return;
            }
            (KeyModifiers::NONE, KeyCode::Esc)
                if matches!(self.active_tab().status, TabStatus::Running { .. }) =>
            {
                self.cancel_query();
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

        // Route to tab-specific key handlers.
        match self.main_tab {
            MainTab::Query => match self.focus {
                Focus::Editor => self.handle_editor_key(key),
                Focus::Results => self.handle_results_key(key),
                Focus::Panel => self.focus = Focus::Editor, // shouldn't happen on Query tab
            },
            MainTab::History => self.handle_panel_history_key(key),
            MainTab::Schema => self.handle_panel_schema_key(key),
            MainTab::Saved => self.handle_panel_saved_key(key),
            MainTab::Dashboard => {} // Read-only dashboard — no interactive keys
        }
    }

    /// Switch to a main tab, updating focus appropriately.
    ///
    /// Saves the current Query focus (Editor/Results) on switch-away and
    /// restores it on switch-back, so users don't lose context.
    pub(super) fn switch_to_main_tab(&mut self, tab: MainTab) {
        // Save Query focus before switching away.
        if self.main_tab == MainTab::Query && tab != MainTab::Query {
            self.query_focus = self.focus;
        }

        self.main_tab = tab;
        match tab {
            MainTab::Query => self.focus = self.query_focus,
            _ => self.focus = Focus::Panel,
        }
    }
}

/// Run the TUI application.
#[allow(clippy::too_many_lines)] // orchestration entry point — splitting adds indirection without clarity
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
    if config.ui.enable_mouse {
        crossterm::execute!(
            stdout,
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableMouseCapture
        )?;
    } else {
        crossterm::execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Fetch schema, history, saved queries, service schemas, server version, and permissions in parallel.
    tracing::info!("fetching startup data");
    let (
        schema_result,
        history_result,
        saved_result,
        services_result,
        health_result,
        whoami_result,
    ) = tokio::join!(
        client.schema(),
        client.history(Some(100), None),
        client.list_saved(),
        client.schema_services(),
        client.health(),
        client.whoami(),
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

    let schema_browser = match services_result {
        Ok(resp) => {
            tracing::info!("schema services fetched: {} services", resp.services.len());
            Some(SchemaBrowser::new(resp.services))
        }
        Err(e) => {
            tracing::warn!("failed to fetch schema services: {}", e);
            None
        }
    };

    let server_version = match health_result {
        Ok(h) => {
            tracing::info!("health check ok, server version: {:?}", h.version);
            h.version
        }
        Err(e) => {
            tracing::warn!("failed to fetch health: {}", e);
            None
        }
    };

    // Create app with schema, history, and saved queries.
    let mut app = App::new(client);
    app.mouse_enabled = config.ui.enable_mouse;
    app.max_live_events = config.tail.max_events;
    app.timezone = config.ui.timezone.clone();
    app.theme = theme::resolve(&config.ui.theme);
    app.schema_cache = schema;
    app.history_cache = history;
    app.saved_cache = saved;
    app.panel.schema = schema_browser;
    app.server_version = server_version;
    let is_admin = whoami_result
        .as_ref()
        .is_ok_and(|w| w.permissions.iter().any(|p| p == "server_manage"));
    app.dashboard.is_admin = is_admin;
    if is_admin {
        tracing::info!("admin privileges detected — Dashboard tab enabled");
    } else if let Err(e) = &whoami_result {
        tracing::warn!("failed to fetch permissions: {e} — Dashboard tab disabled");
    }

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
    if app.mouse_enabled {
        crossterm::execute!(
            terminal.backend_mut(),
            DisableMouseCapture,
            DisableBracketedPaste,
            LeaveAlternateScreen
        )?;
    } else {
        crossterm::execute!(
            terminal.backend_mut(),
            DisableBracketedPaste,
            LeaveAlternateScreen
        )?;
    }
    terminal.show_cursor()?;

    result
}

/// Main event loop.
fn run_event_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> Result<(), CliError>
where
    CliError: From<B::Error>,
{
    let mut iteration = 0u64;
    loop {
        iteration += 1;
        if iteration.is_multiple_of(10) {
            tracing::debug!("event loop iteration {}", iteration);
        }

        // Run debounced real-time validation on the active tab.
        app.active_tab_mut().maybe_validate();

        // Poll for query results from background tasks.
        app.poll_query_results();

        // Poll for mutation results (save/delete operations).
        app.poll_mutations();

        // Poll for dashboard snapshot updates.
        app.poll_dashboard();

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
                Event::Mouse(mouse) if app.mouse_enabled => {
                    app.handle_mouse(mouse);
                }
                Event::Paste(text) if app.focus == Focus::Editor => {
                    let tab = app.active_tab_mut();
                    tab.editor.save_snapshot();
                    tab.editor.delete_selection();
                    tab.editor.insert_text(&text);
                    tab.mark_editor_dirty();
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
    use trawl_client::{ListSavedResponse, SavedQueryResponse};
    use trawl_engine::value::{Column, Value};

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
            result: trawl_engine::value::QueryResult {
                columns: cols,
                rows,
            },
            truncated: false,
            pagination: trawl_client::PaginationMeta {
                limit: 10000,
                offset: 0,
                returned,
            },
            degraded_fields: Vec::new(),
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

    // --- Help popup tests ---

    #[test]
    fn key_f1_toggles_help_popup() {
        let mut app = test_app();
        assert!(app.popup.is_none());
        app.handle_key(key(KeyCode::F(1)));
        assert!(matches!(app.popup, Some(Popup::Help { scroll: 0 })));
        app.handle_key(key(KeyCode::F(1)));
        assert!(app.popup.is_none());
    }

    // --- Tab switching tests ---

    #[test]
    fn key_alt_1_switches_to_query_tab() {
        let mut app = test_app();
        app.main_tab = MainTab::History;
        app.focus = Focus::Panel;
        app.handle_key(key_mod(KeyCode::Char('1'), KeyModifiers::ALT));
        assert_eq!(app.main_tab, MainTab::Query);
        assert_eq!(app.focus, Focus::Editor);
    }

    #[test]
    fn key_alt_2_switches_to_history_tab() {
        let mut app = test_app();
        app.handle_key(key_mod(KeyCode::Char('2'), KeyModifiers::ALT));
        assert_eq!(app.main_tab, MainTab::History);
        assert_eq!(app.focus, Focus::Panel);
    }

    #[test]
    fn key_alt_3_switches_to_schema_tab() {
        let mut app = test_app();
        app.handle_key(key_mod(KeyCode::Char('3'), KeyModifiers::ALT));
        assert_eq!(app.main_tab, MainTab::Schema);
        assert_eq!(app.focus, Focus::Panel);
    }

    #[test]
    fn key_alt_4_switches_to_saved_tab() {
        let mut app = test_app();
        app.handle_key(key_mod(KeyCode::Char('4'), KeyModifiers::ALT));
        assert_eq!(app.main_tab, MainTab::Saved);
        assert_eq!(app.focus, Focus::Panel);
    }

    #[test]
    fn tab_on_panel_switches_to_query() {
        let mut app = test_app();
        app.main_tab = MainTab::History;
        app.focus = Focus::Panel;
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.main_tab, MainTab::Query);
        assert_eq!(app.focus, Focus::Editor);
    }

    #[test]
    fn focus_preserved_across_tab_switch() {
        let mut app = test_app();
        // Start in Results focus
        app.focus = Focus::Results;
        // Switch to History
        app.handle_key(key_mod(KeyCode::Char('2'), KeyModifiers::ALT));
        assert_eq!(app.main_tab, MainTab::History);
        assert_eq!(app.focus, Focus::Panel);
        // Switch back to Query — should restore Results focus
        app.handle_key(key_mod(KeyCode::Char('1'), KeyModifiers::ALT));
        assert_eq!(app.main_tab, MainTab::Query);
        assert_eq!(app.focus, Focus::Results);
    }

    #[test]
    fn key_esc_on_panel_tab_switches_to_query() {
        let mut app = test_app();
        app.main_tab = MainTab::History;
        app.focus = Focus::Panel;
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.main_tab, MainTab::Query);
        assert_eq!(app.focus, Focus::Editor);
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
        // F1 should NOT open help while popup is active
        app.handle_key(key(KeyCode::F(1)));
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
        app.tab.result = Some(make_query_response(
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
                result: Ok(response),
                duration: Duration::from_millis(50),
            })
            .unwrap();

        app.poll_query_results();

        assert!(app.tab.result.is_some());
        assert!(matches!(app.tab.status, TabStatus::Success { .. }));
    }

    #[test]
    fn poll_query_result_sets_error_status() {
        let mut app = test_app();
        app.query_tx
            .send(QueryResult {
                result: Err(QueryError {
                    message: "something broke".to_owned(),
                    details: Vec::new(),
                }),
                duration: Duration::from_millis(10),
            })
            .unwrap();

        app.poll_query_results();

        assert!(matches!(
            app.tab.status,
            TabStatus::Error { ref message, .. } if message == "something broke"
        ));
    }

    #[test]
    fn poll_query_result_resets_scroll() {
        let mut app = test_app();
        app.tab.scroll_offset = 42;
        app.tab.horizontal_scroll_offset = 7;

        let response = make_query_response(vec!["x"], vec![vec![Value::Integer(1)]]);
        app.query_tx
            .send(QueryResult {
                result: Ok(response),
                duration: Duration::from_millis(1),
            })
            .unwrap();

        app.poll_query_results();

        assert_eq!(app.tab.scroll_offset, 0);
        assert_eq!(app.tab.horizontal_scroll_offset, 0);
    }

    #[test]
    fn poll_query_result_timechart_auto_switches_view() {
        let mut app = test_app();
        assert_eq!(app.tab.chart_view, ChartView::Table);

        let response = make_query_response(
            vec!["_time", "count"],
            vec![vec![
                Value::String("2025-01-01T00:00:00Z".into()),
                Value::Integer(10),
            ]],
        );
        app.query_tx
            .send(QueryResult {
                result: Ok(response),
                duration: Duration::from_millis(1),
            })
            .unwrap();

        app.poll_query_results();

        assert_eq!(app.tab.chart_view, ChartView::LineChart);
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

        // SavedQueryCreated switches to Saved tab
        assert_eq!(app.main_tab, MainTab::Saved);
        assert_eq!(app.focus, Focus::Panel);
    }

    #[test]
    fn poll_mutation_cache_refreshed() {
        let mut app = test_app();
        let saved = ListSavedResponse {
            queries: vec![SavedQueryResponse {
                id: 1,
                name: "test query".to_owned(),
                query: "_severity=error".to_owned(),
                created_at: "2025-01-01T00:00:00Z".to_owned(),
                updated_at: "2025-01-01T00:00:00Z".to_owned(),
                schedule: None,
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
                    schedule: None,
                },
                SavedQueryResponse {
                    id: 2,
                    name: "beta".to_owned(),
                    query: "b".to_owned(),
                    created_at: String::new(),
                    updated_at: String::new(),
                    schedule: None,
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

        assert_eq!(app.panel.saved_selected, 1);
    }

    #[test]
    fn poll_mutation_error_shows_popup() {
        let mut app = test_app();
        app.mutation_tx
            .send(MutationResult::Error {
                message: "kaboom".to_owned(),
            })
            .unwrap();

        app.poll_mutations();
        assert!(matches!(app.popup, Some(Popup::Error { .. })));
    }

    #[test]
    fn error_popup_dismissed_by_esc() {
        let mut app = test_app();
        app.popup = Some(Popup::Error {
            message: "oops".to_owned(),
        });
        app.handle_key(key(KeyCode::Esc));
        assert!(app.popup.is_none());
    }

    #[test]
    fn error_popup_dismissed_by_enter() {
        let mut app = test_app();
        app.popup = Some(Popup::Error {
            message: "oops".to_owned(),
        });
        app.handle_key(key(KeyCode::Enter));
        assert!(app.popup.is_none());
    }

    #[test]
    fn error_popup_blocks_global_keys() {
        let mut app = test_app();
        app.popup = Some(Popup::Error {
            message: "oops".to_owned(),
        });
        // F1 should NOT open help — popup blocks it
        app.handle_key(key(KeyCode::F(1)));
        assert!(matches!(app.popup, Some(Popup::Error { .. })));
    }

    #[test]
    fn multiple_results_last_wins() {
        let mut app = test_app();

        // Send two results — the last one should be the final state.
        for i in 0..2 {
            let response =
                make_query_response(vec!["idx"], vec![vec![Value::Integer(i64::from(i))]]);
            app.query_tx
                .send(QueryResult {
                    result: Ok(response),
                    duration: Duration::from_millis(1),
                })
                .unwrap();
        }

        app.poll_query_results();

        assert!(app.tab.result.is_some());
        // Last result (idx=1) should be the one stored.
        let rows = &app.tab.result.as_ref().unwrap().result.rows;
        assert_eq!(rows[0][0], Value::Integer(1));
    }
}
