// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

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
use trawl_api::zone::DisplayZone;
use trawl_client::{
    ClientError, HistoryResponse, HttpClient, ListSavedResponse, QueryResponse, SchemaResponse,
    WhoAmIResponse,
};

use self::driver::{DriverCommand, DriverResponse, ExecuteWaiter, query_response_to_data};
use self::state::{
    ChartView, ColumnConfig, DashboardState, Focus, LayoutAreas, MainTab, PanelState, Popup,
    ResultsSearch, SchemaBrowser, SimpleEditor, Tab, TabStatus,
};
use crate::CliError;
use crate::config::Config;

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

fn clipboard_set(text: &str) {
    #[cfg(feature = "clipboard")]
    if let Ok(mut cb) = arboard::Clipboard::new() {
        let _ = cb.set_text(text.to_owned());
    }
    #[cfg(not(feature = "clipboard"))]
    let _ = text;
}

#[derive(Debug)]
pub(super) struct QueryError {
    pub(super) message: String,
    pub(super) details: Vec<trawl_client::ErrorDetail>,
    /// The query text the run sent: what the details' spans index.
    pub(super) query: String,
}

#[derive(Debug)]
pub(super) struct QueryResult {
    pub(super) result: Result<QueryResponse, QueryError>,
    pub(super) duration: Duration,
    pub(super) capped: Vec<trawl_api::stream::SnapshotCap>,
    pub(super) generation: state::RunGeneration,
    pub(super) source: state::RunSource,
}

#[derive(Debug)]
pub(super) enum MutationResult {
    SavedQueryCreated {
        name: String,
    },
    SavedQueryDeleted {
        name: String,
    },
    CacheRefreshed {
        saved: ListSavedResponse,
        select_name: Option<String>,
    },
    ScheduleSet {
        saved_query_id: i64,
    },
    DashboardUpdate(Result<Box<trawl_client::DashboardSnapshot>, String>),
    RunsLoaded {
        token: state::RequestToken,
        saved_id: i64,
        outcome: Result<(Vec<trawl_client::ReportRunSummary>, usize), String>,
    },
    RunResultLoaded {
        token: state::RequestToken,
        saved_id: i64,
        run_id: i64,
        outcome: Result<trawl_api::value::QueryResult, String>,
    },
    Error {
        message: String,
    },
}

pub struct App {
    pub client: HttpClient,
    pub tab: Tab,
    pub main_tab: MainTab,
    pub focus: Focus,
    query_focus: Focus,
    pub panel: PanelState,
    pub popup: Option<Popup>,
    pub schema_cache: Option<SchemaResponse>,
    pub history_cache: Option<HistoryResponse>,
    pub saved_cache: Option<ListSavedResponse>,
    pub should_quit: bool,
    pub live_mode: bool,
    pub display_zone: DisplayZone,
    pub results_search: Option<ResultsSearch>,
    max_live_events: usize,
    query_rx: mpsc::UnboundedReceiver<QueryResult>,
    query_tx: mpsc::UnboundedSender<QueryResult>,
    mutation_rx: mpsc::UnboundedReceiver<MutationResult>,
    mutation_tx: mpsc::UnboundedSender<MutationResult>,
    #[allow(dead_code)]
    pub sample_values_cache: std::collections::HashMap<(String, Option<String>), Vec<String>>,
    driver_rx: Option<mpsc::UnboundedReceiver<DriverCommand>>,
    driver_socket_path: Option<PathBuf>,
    pub server_version: Option<String>,
    pub dashboard: DashboardState,
    pub theme: theme::Theme,
    pub layout: LayoutAreas,
    pub mouse_enabled: bool,
}

impl App {
    pub fn new(client: HttpClient, display_zone: DisplayZone) -> Self {
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
            display_zone,
            results_search: None,
            max_live_events: 1000,
            query_rx,
            query_tx,
            mutation_rx,
            mutation_tx,
            sample_values_cache: std::collections::HashMap::new(),
            driver_rx: None,
            driver_socket_path: None,
            server_version: None,
            dashboard: DashboardState::new(),
            theme: theme::dark(),
            layout: LayoutAreas::default(),
            mouse_enabled: false,
        }
    }

    pub fn active_tab(&self) -> &Tab {
        &self.tab
    }

    pub fn active_tab_mut(&mut self) -> &mut Tab {
        &mut self.tab
    }

    pub fn execute_query(&mut self) {
        self.start_batch(None);
    }

    /// The run is taken before a waiting driver execute is answered, so a
    /// client that already left still releases the tab.
    fn end_run(&mut self, reason: &str) {
        match std::mem::take(&mut self.tab.execution) {
            state::Execution::Idle => {}
            state::Execution::Batch { handle, reply, .. } => {
                handle.abort();
                if let Some(reply) = reply {
                    reply.answer(DriverResponse::err(reason));
                }
            }
            state::Execution::Live { handle, .. } => {
                handle.abort();
                self.live_mode = false;
            }
        }
    }

    fn start_batch(&mut self, reply: Option<ExecuteWaiter>) {
        if self.active_tab().editor.text().trim().is_empty() {
            if let Some(reply) = reply {
                reply.answer(DriverResponse::err("empty query"));
            }
            return;
        }

        self.end_run("replaced");

        self.format_editor_query();

        let query = self.active_tab().editor.text().trim().to_owned();
        let generation = self.tab.next_generation();

        let client = self.client.clone();
        let tx = self.query_tx.clone();

        let handle = tokio::spawn(async move {
            tracing::info!("background task started for query: {}", query);
            let start = Instant::now();
            let result = client
                .query_paginated(&query, None, None)
                .await
                .map_err(|e| QueryError {
                    message: e.to_string(),
                    details: e.error_details().to_vec(),
                    query: query.clone(),
                });
            let duration = start.elapsed();
            tracing::info!(
                "query completed in {:?}, result: {:?}",
                duration,
                result.is_ok()
            );

            let _ = tx.send(QueryResult {
                result,
                duration,
                capped: Vec::new(),
                generation,
                source: state::RunSource::Batch,
            });
        });

        let tab = self.active_tab_mut();
        tab.execution = state::Execution::Batch {
            generation,
            handle,
            reply,
        };
        tab.status = TabStatus::Running {
            start: Instant::now(),
        };
    }

    pub(super) fn format_editor_query(&mut self) {
        let tab = self.active_tab_mut();
        let text = tab.editor.text();
        if text.trim().is_empty() {
            return;
        }
        let Ok(query) = trawl_core::parser::parse(&text) else {
            return;
        };
        let formatted = match trawl_core::format::checked_format_query(&query) {
            Ok(formatted) => formatted,
            Err(refusal) => {
                tab.status = TabStatus::Error {
                    message: format!("not formatted: {refusal}"),
                    details: Vec::new(),
                    query: None,
                };
                return;
            }
        };
        if formatted != text {
            tab.editor.save_snapshot();
            tab.editor.clear();
            tab.editor.insert_text(&formatted);
            tab.mark_editor_dirty();
        }
    }

    pub(super) fn cancel_query(&mut self) {
        if !self.active_tab().execution.is_batch() {
            return;
        }
        self.end_run("cancelled");
        self.active_tab_mut().status = TabStatus::Error {
            message: "cancelled".to_owned(),
            details: Vec::new(),
            query: None,
        };
        tracing::info!("query cancelled by user");
    }

    pub(super) fn clear_tab(&mut self) {
        self.end_run("cleared");
        self.active_tab_mut().clear();
    }

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

    fn fetch_runs(&self, saved_id: i64, token: state::RequestToken) {
        let client = self.client.clone();
        let mutation_tx = self.mutation_tx.clone();
        tokio::spawn(async move {
            let outcome = match client.list_report_runs(saved_id, Some(50), None).await {
                Ok(resp) => Ok((resp.runs, resp.total)),
                Err(e) => {
                    tracing::error!("failed to fetch runs for saved query {saved_id}: {e}");
                    Err(format!("Failed to fetch runs: {e}"))
                }
            };
            let _ = mutation_tx.send(MutationResult::RunsLoaded {
                token,
                saved_id,
                outcome,
            });
        });
    }

    fn fetch_run_result(&self, saved_id: i64, run_id: i64, token: state::RequestToken) {
        let client = self.client.clone();
        let mutation_tx = self.mutation_tx.clone();
        tokio::spawn(async move {
            let outcome = match client.get_report_run(saved_id, run_id).await {
                Ok(resp) => {
                    if let Some(qr) = resp.result {
                        Ok(qr)
                    } else if resp.summary.row_count == Some(0) {
                        Ok(trawl_api::value::QueryResult::empty())
                    } else {
                        Err("Run has no result data".to_owned())
                    }
                }
                Err(e) => {
                    tracing::error!("failed to fetch run result: {e}");
                    Err(run_result_error_message(&e))
                }
            };
            let _ = mutation_tx.send(MutationResult::RunResultLoaded {
                token,
                saved_id,
                run_id,
                outcome,
            });
        });
    }

    fn pending_saved_detail(
        &mut self,
        token: state::RequestToken,
        saved_id: i64,
    ) -> Option<&mut state::SavedDetailState> {
        self.panel
            .saved_detail
            .as_mut()
            .filter(|d| d.pending == Some(token) && d.saved_id == saved_id)
    }

    fn apply_runs_loaded(
        &mut self,
        token: state::RequestToken,
        saved_id: i64,
        outcome: Result<(Vec<trawl_client::ReportRunSummary>, usize), String>,
    ) {
        let Some(detail) = self.pending_saved_detail(token, saved_id) else {
            tracing::debug!("ignoring stale runs response for saved query {saved_id}");
            return;
        };
        match outcome {
            Ok((runs, total)) => {
                tracing::debug!("loaded {} runs for saved query {saved_id}", runs.len());
                detail.runs = runs;
                detail.total_runs = total;
                detail.pending = None;
                self.panel.saved_focus = state::SavedFocus::Detail;
            }
            // An empty run list would read as "no runs yet", so the failed
            // detail closes instead.
            Err(message) => {
                self.panel.saved_detail = None;
                self.panel.saved_focus = state::SavedFocus::List;
                self.popup = Some(Popup::Error { message });
            }
        }
    }

    fn apply_run_result_loaded(
        &mut self,
        token: state::RequestToken,
        saved_id: i64,
        run_id: i64,
        outcome: Result<trawl_api::value::QueryResult, String>,
    ) {
        let Some(detail) = self.pending_saved_detail(token, saved_id) else {
            tracing::debug!("ignoring stale result response for run {run_id}");
            return;
        };
        detail.pending = None;
        match outcome {
            Ok(result) => {
                tracing::debug!("loaded result data for run {run_id}");
                detail.result = Some(state::LoadedRun { run_id, result });
                detail.result_scroll = 0;
                self.panel.saved_focus = state::SavedFocus::RunResults;
            }
            Err(message) => self.popup = Some(Popup::Error { message }),
        }
    }

    pub fn poll_mutations(&mut self) {
        while let Ok(mutation_result) = self.mutation_rx.try_recv() {
            match mutation_result {
                MutationResult::SavedQueryCreated { name } => {
                    tracing::info!("saved query created: {name}");
                    self.refresh_saved_cache(Some(name.clone()));
                    self.main_tab = MainTab::Saved;
                    self.focus = Focus::Panel;
                }
                MutationResult::SavedQueryDeleted { name } => {
                    tracing::info!("saved query deleted: {name}");
                    self.refresh_saved_cache(None);
                }
                MutationResult::CacheRefreshed { saved, select_name } => {
                    tracing::debug!(
                        "updating saved queries cache with {} queries",
                        saved.queries.len()
                    );

                    let prior_id = self
                        .saved_cache
                        .as_ref()
                        .and_then(|c| c.queries.get(self.panel.saved_selected))
                        .map(|q| q.id);
                    let index = select_name
                        .and_then(|name| saved.queries.iter().position(|q| q.name == name))
                        .or_else(|| {
                            let id = prior_id?;
                            saved.queries.iter().position(|q| q.id == id)
                        })
                        .unwrap_or(self.panel.saved_selected);
                    self.saved_cache = Some(saved);
                    self.select_saved(index);
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
                    token,
                    saved_id,
                    outcome,
                } => self.apply_runs_loaded(token, saved_id, outcome),
                MutationResult::RunResultLoaded {
                    token,
                    saved_id,
                    run_id,
                    outcome,
                } => self.apply_run_result_loaded(token, saved_id, run_id, outcome),
                MutationResult::Error { message } => {
                    tracing::error!("mutation error: {message}");
                    self.popup = Some(Popup::Error { message });
                }
            }
        }
    }

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

    pub fn poll_query_results(&mut self) {
        while let Ok(query_result) = self.query_rx.try_recv() {
            let event = (query_result.generation, query_result.source);
            if self.tab.execution.current() != Some(event) {
                tracing::debug!("ignoring obsolete query event");
                continue;
            }
            tracing::info!("received query result");

            let reply = match event.1 {
                state::RunSource::Batch => match std::mem::take(&mut self.tab.execution) {
                    state::Execution::Batch { reply, .. } => reply,
                    _ => unreachable!("current() matched a batch"),
                },
                state::RunSource::Live => None,
            };

            self.show_query_result(query_result, reply);
        }
    }

    fn show_query_result(&mut self, query_result: QueryResult, reply: Option<ExecuteWaiter>) {
        let tab = &mut self.tab;
        match query_result.result {
            Ok(response) => {
                let is_timechart = response.result.columns.iter().any(|c| c.name == "_time");
                if is_timechart && tab.chart_view == ChartView::Table {
                    tab.chart_view = ChartView::LineChart;
                }

                tab.column_config = Some(ColumnConfig::init(&response.result.columns));
                tab.result = Some(response);
                tab.live_capped = query_result.capped;
                #[allow(clippy::cast_possible_truncation)]
                let duration_ms = query_result.duration.as_millis() as u64;
                tab.status = TabStatus::Success { duration_ms };
                tab.scroll_offset = 0;
                tab.horizontal_scroll_offset = 0;
                tab.selected_row = None;
                tab.validation_errors.clear();

                if let (Some(reply), Some(result)) = (reply, tab.result.as_ref()) {
                    let mut data = query_response_to_data(result);
                    data.duration_ms = Some(duration_ms);
                    reply.answer(DriverResponse::ok_with(data));
                }
            }
            Err(err) => {
                if let Some(reply) = reply {
                    reply.answer(DriverResponse::err(&err.message));
                }
                tab.status = TabStatus::Error {
                    message: err.message,
                    details: err.details,
                    query: Some(err.query),
                };
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    pub fn handle_key(&mut self, key: event::KeyEvent) {
        if self.popup.is_some() {
            self.handle_popup_key(key);
            return;
        }

        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('q')) => {
                self.should_quit = true;
                return;
            }
            (KeyModifiers::NONE, KeyCode::F(1)) => {
                if matches!(self.popup, Some(Popup::Help { .. })) {
                    self.popup = None;
                } else {
                    self.popup = Some(Popup::Help { scroll: 0 });
                }
                return;
            }
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
            (KeyModifiers::NONE, KeyCode::F(9)) => {
                self.toggle_live_mode();
                return;
            }
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
            (KeyModifiers::NONE, KeyCode::Esc) if self.main_tab != MainTab::Query => {
                if self.main_tab == MainTab::Schema
                    && self.panel.schema.as_ref().is_some_and(|s| s.filter_active)
                {
                    if let Some(schema) = self.panel.schema.as_mut() {
                        schema.filter_active = false;
                        schema.filter.clear();
                    }
                    return;
                }
                if self.main_tab == MainTab::Saved {
                    match self.panel.saved_focus {
                        state::SavedFocus::RunResults => {
                            if let Some(ref mut detail) = self.panel.saved_detail {
                                detail.result = None;
                                detail.result_scroll = 0;
                            }
                            self.panel.saved_focus = state::SavedFocus::Detail;
                            return;
                        }
                        state::SavedFocus::Detail => {
                            self.panel.saved_detail = None;
                            self.panel.saved_focus = state::SavedFocus::List;
                            return;
                        }
                        state::SavedFocus::List => {}
                    }
                }
                self.switch_to_main_tab(MainTab::Query);
                return;
            }
            // Keyed on the run, not the status: a refused Ctrl+F replaces
            // the Running status while the request is still in flight.
            (KeyModifiers::NONE, KeyCode::Esc) if self.active_tab().execution.is_batch() => {
                self.cancel_query();
                return;
            }
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

        match self.main_tab {
            MainTab::Query => match self.focus {
                Focus::Editor => self.handle_editor_key(key),
                Focus::Results => self.handle_results_key(key),
                Focus::Panel => self.focus = Focus::Editor,
            },
            MainTab::History => self.handle_panel_history_key(key),
            MainTab::Schema => self.handle_panel_schema_key(key),
            MainTab::Saved => self.handle_panel_saved_key(key),
            MainTab::Dashboard => {}
        }
    }

    pub(super) fn switch_to_main_tab(&mut self, tab: MainTab) {
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

fn apply_whoami_result(app: &mut App, whoami_result: Result<WhoAmIResponse, ClientError>) {
    match whoami_result {
        Ok(whoami) => {
            app.dashboard.is_admin = whoami
                .permissions
                .iter()
                .any(|permission| permission == "server_manage");
            if app.dashboard.is_admin {
                tracing::info!("admin privileges detected — Dashboard tab enabled");
            }
        }
        Err(error) => {
            app.dashboard.is_admin = false;
            tracing::warn!("failed to fetch permissions: {error} — Dashboard tab disabled");
        }
    }
}

#[allow(clippy::too_many_lines)]
pub async fn run(
    config: &Config,
    display_zone: DisplayZone,
    conn: crate::cli::ConnectionParams,
    driver_path: Option<&Path>,
) -> Result<(), CliError> {
    let client = conn.client()?;

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
            match h.status {
                trawl_api::HealthStatus::Ok | trawl_api::HealthStatus::Degraded => {
                    tracing::info!("health check ok, server version: {:?}", h.version);
                }
                trawl_api::HealthStatus::Unavailable => {
                    tracing::warn!(
                        "server reports health status unavailable, server version: {:?}",
                        h.version
                    );
                }
            }
            h.version
        }
        Err(e) => {
            tracing::warn!("failed to fetch health: {}", e);
            None
        }
    };

    let mut app = App::new(client, display_zone);
    app.mouse_enabled = config.ui.enable_mouse;
    app.max_live_events = config.tail.max_events;
    app.theme = theme::resolve(&config.ui.theme);
    app.schema_cache = schema;
    app.history_cache = history;
    app.saved_cache = saved;
    app.panel.schema = schema_browser;
    app.server_version = server_version;
    apply_whoami_result(&mut app, whoami_result);

    if let Some(path) = driver_path {
        app.start_driver(path);
    }

    let result = run_event_loop(&mut terminal, &mut app);

    app.cleanup_driver();

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

        app.active_tab_mut().maybe_validate();

        app.poll_query_results();

        app.poll_mutations();

        app.poll_dashboard();

        app.poll_driver_commands(terminal);

        terminal.draw(|f| ui::render(app, f))?;

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

        if app.should_quit {
            break;
        }
    }

    Ok(())
}

#[cfg(test)]
#[test]
fn live_event_large_unsigned_exact() {
    let frame: trawl_engine::value::QueryResult = serde_json::from_str(
        r#"{"columns":[{"name":"request_id","type":"integer"},{"name":"service","type":"string"}],"rows":[["18446744073709551615","nginx"]]}"#,
    )
    .expect("a valid data frame");
    let mut buffer = state::LiveBuffer::new(10);
    buffer.push_frame(&frame);

    let response = buffer.to_query_response();
    let idx = response
        .result
        .columns
        .iter()
        .position(|c| c.name == "request_id")
        .expect("request_id column");
    assert_eq!(
        response.result.columns[idx].ty,
        trawl_engine::value::ColumnType::Integer
    );
    assert_eq!(
        response.result.rows[0][idx],
        trawl_engine::value::Value::UInt(u64::MAX),
        "a live cell must carry the number, never a rounded double or a null"
    );
}

#[cfg(test)]
#[test]
fn snapshot_large_unsigned_exact() {
    let frame: trawl_api::stream::SnapshotFrame = serde_json::from_str(
        r#"{"columns":[{"name":"service","type":"string"},{"name":"total","type":"integer"}],"rows":[["nginx","18446744073709551615"]]}"#,
    )
    .expect("a valid snapshot frame");
    let mut buffer = state::LiveBuffer::new(10);
    buffer.replace_with_snapshot(&frame.result, &frame.capped);

    let response = buffer.to_query_response();
    assert_eq!(
        response.result.rows[0][1],
        trawl_engine::value::Value::UInt(u64::MAX),
        "a snapshot cell must carry the number, never a rounded double or a null"
    );
}

/// A 409 on a run-result fetch is the server saying the run succeeded and
/// its stored result is gone, so it is shown verbatim: prefixing it with
/// "Failed to fetch run result" would re-assert a failure the run did not
/// have.
fn run_result_error_message(e: &ClientError) -> String {
    match e {
        ClientError::Server {
            status: 409, error, ..
        } => error.message.clone(),
        other => format!("Failed to fetch run result: {other}"),
    }
}

#[cfg(test)]
#[test]
fn unavailable_result_toast() {
    const SENTENCE: &str = "report run 42 succeeded, but its stored result is unavailable; \
                            no older run was substituted";

    let conflict = ClientError::Server {
        status: 409,
        error: trawl_api::ErrorEnvelope::simple(trawl_api::ErrorCode::BadRequest, SENTENCE),
        request_id: None,
        retry_after_secs: None,
    };
    assert_eq!(run_result_error_message(&conflict), SENTENCE);

    let network = ClientError::Network(trawl_client::NetworkError::new(
        trawl_client::NetworkKind::Connect,
        "connection refused",
    ));
    assert!(
        run_result_error_message(&network).starts_with("Failed to fetch run result:"),
        "a transport failure is a failure to fetch"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use trawl_client::{ListSavedResponse, SavedQueryResponse};
    use trawl_engine::value::{Column, Value};

    pub(crate) fn test_app() -> App {
        let client = HttpClient::new_insecure("https://localhost:0", "flt_test_not_real").unwrap();
        App::new(client, DisplayZone::Utc)
    }

    fn key(code: KeyCode) -> event::KeyEvent {
        event::KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn key_mod(code: KeyCode, modifiers: KeyModifiers) -> event::KeyEvent {
        event::KeyEvent::new(code, modifiers)
    }

    fn make_query_response(columns: Vec<&str>, rows: Vec<Vec<Value>>) -> QueryResponse {
        let cols = columns
            .into_iter()
            .enumerate()
            .map(|(idx, name)| {
                let mut fold = trawl_engine::value::TypeFold::new();
                for row in &rows {
                    fold.observe_value(&row[idx]);
                }
                Column::new(
                    name,
                    fold.finish()
                        .unwrap_or(trawl_engine::value::ColumnType::String),
                )
            })
            .collect();
        let returned = rows.len();
        QueryResponse {
            execution: None,
            result: trawl_engine::value::QueryResult {
                columns: cols,
                rows,
            },
            pagination: trawl_client::PaginationMeta {
                limit: 10000,
                offset: 0,
                returned,
                total: returned,
            },
            degraded_fields: Vec::new(),
            severity_columns: Vec::new(),
            histogram: None,
        }
    }

    fn whoami(roles: &[&str], permissions: &[&str]) -> WhoAmIResponse {
        WhoAmIResponse {
            prefix: "abcd1234".to_owned(),
            name: "test-key".to_owned(),
            kind: trawl_api::PrincipalKind::Human,
            roles: roles.iter().map(|role| (*role).to_owned()).collect(),
            permissions: permissions
                .iter()
                .map(|permission| (*permission).to_owned())
                .collect(),
        }
    }

    fn rendered_app(app: &mut App) -> String {
        let backend = ratatui::backend::TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| ui::render(app, frame)).unwrap();
        terminal.backend().to_string()
    }

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

    #[test]
    fn key_f1_toggles_help_popup() {
        let mut app = test_app();
        assert!(app.popup.is_none());
        app.handle_key(key(KeyCode::F(1)));
        assert!(matches!(app.popup, Some(Popup::Help { scroll: 0 })));
        app.handle_key(key(KeyCode::F(1)));
        assert!(app.popup.is_none());
    }

    #[test]
    fn whoami_permissions_drive_dashboard_tab_false_true_false() {
        let mut app = test_app();

        apply_whoami_result(&mut app, Ok(whoami(&["trawl-admin"], &["query"])));
        assert!(!app.dashboard.is_admin);
        assert!(!rendered_app(&mut app).contains("M-5 Dashboard"));
        app.handle_key(key_mod(KeyCode::Char('5'), KeyModifiers::ALT));
        assert_eq!(app.main_tab, MainTab::Query);

        apply_whoami_result(
            &mut app,
            Ok(whoami(&["arbitrary-role"], &["server_manage"])),
        );
        assert!(app.dashboard.is_admin);
        assert!(rendered_app(&mut app).contains("M-5 Dashboard"));
        app.handle_key(key_mod(KeyCode::Char('5'), KeyModifiers::ALT));
        assert_eq!(app.main_tab, MainTab::Dashboard);

        app.switch_to_main_tab(MainTab::Query);
        apply_whoami_result(&mut app, Ok(whoami(&["trawl-admin"], &["schema_read"])));
        assert!(!app.dashboard.is_admin);
        assert_eq!(app.main_tab, MainTab::Query);
        assert!(!rendered_app(&mut app).contains("M-5 Dashboard"));
        app.handle_key(key_mod(KeyCode::Char('5'), KeyModifiers::ALT));
        assert_eq!(app.main_tab, MainTab::Query);
    }

    #[test]
    fn whoami_error_clears_dashboard_access() {
        let mut app = test_app();
        apply_whoami_result(
            &mut app,
            Ok(whoami(&["server-operator"], &["server_manage"])),
        );
        assert!(app.dashboard.is_admin);

        apply_whoami_result(
            &mut app,
            Err(ClientError::Network(trawl_client::NetworkError::new(
                trawl_client::NetworkKind::Connect,
                "whoami unavailable",
            ))),
        );
        assert!(!app.dashboard.is_admin);
        assert_eq!(app.main_tab, MainTab::Query);
        assert!(!rendered_app(&mut app).contains("M-5 Dashboard"));
        app.handle_key(key_mod(KeyCode::Char('5'), KeyModifiers::ALT));
        assert_eq!(app.main_tab, MainTab::Query);
    }

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
        app.focus = Focus::Results;
        app.handle_key(key_mod(KeyCode::Char('2'), KeyModifiers::ALT));
        assert_eq!(app.main_tab, MainTab::History);
        assert_eq!(app.focus, Focus::Panel);
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

    #[test]
    fn key_ctrl_q_sets_quit() {
        let mut app = test_app();
        app.handle_key(key_mod(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert!(app.should_quit);
    }

    #[test]
    fn key_ctrl_s_opens_save_popup() {
        let mut app = test_app();
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

    fn test_schedule(
        window: Option<&str>,
        lag: Option<(&str, u64)>,
    ) -> trawl_api::ScheduleResponse {
        trawl_api::ScheduleResponse {
            id: 1,
            saved_query_id: 5,
            interval: "1h".to_owned(),
            interval_secs: 3600,
            max_runs: None,
            enabled: true,
            created_at: "2026-03-01T00:00:00Z".to_owned(),
            updated_at: "2026-03-01T00:00:00Z".to_owned(),
            last_run: None,
            total_runs: 0,
            exhausted: false,
            window: window.map(ToOwned::to_owned),
            lag: lag.map(|(text, _)| text.to_owned()),
            lag_secs: lag.map(|(_, secs)| secs),
            covered_through: None,
            next_fire_at: "2026-03-14T04:00:00Z".to_owned(),
        }
    }

    fn schedule_popup(schedule: Option<&trawl_api::ScheduleResponse>) -> App {
        let mut app = test_app();
        app.popup = Some(Popup::SetSchedule(Box::new(state::ScheduleForm::new(
            5,
            "nightly errors".to_owned(),
            schedule,
        ))));
        app
    }

    fn schedule_form(app: &App) -> &state::ScheduleForm {
        match app.popup {
            Some(Popup::SetSchedule(ref form)) => form,
            _ => panic!("expected SetSchedule popup"),
        }
    }

    #[test]
    fn schedule_popup_tab_cycles_the_three_rows() {
        use state::ScheduleField;

        let mut app = schedule_popup(None);
        assert_eq!(schedule_form(&app).focus, ScheduleField::Interval);

        app.handle_key(key(KeyCode::Tab));
        assert_eq!(schedule_form(&app).focus, ScheduleField::Window);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(schedule_form(&app).focus, ScheduleField::Lag);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(schedule_form(&app).focus, ScheduleField::Interval);

        app.handle_key(key(KeyCode::BackTab));
        assert_eq!(schedule_form(&app).focus, ScheduleField::Lag);
        app.handle_key(key(KeyCode::Up));
        assert_eq!(schedule_form(&app).focus, ScheduleField::Window);
        app.handle_key(key(KeyCode::Down));
        assert_eq!(schedule_form(&app).focus, ScheduleField::Lag);
    }

    #[test]
    fn schedule_popup_types_into_the_focused_row() {
        let mut app = schedule_popup(None);
        for ch in "1h".chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
        app.handle_key(key(KeyCode::Tab));
        for ch in "2h".chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }

        let form = schedule_form(&app);
        assert_eq!(form.interval.text(), "1h");
        assert_eq!(form.window.text(), "2h");
        assert_eq!(form.lag.text(), "");
    }

    #[test]
    fn schedule_popup_sends_a_lag_without_a_window() {
        let mut app = schedule_popup(None);
        for ch in "1h".chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Tab));
        for ch in "5m".chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }

        let request = schedule_form(&app).submission().expect("a request");
        assert_eq!(request.interval, "1h");
        assert_eq!(request.window, None);
        assert_eq!(request.lag.as_deref(), Some("5m"));
    }

    #[test]
    fn schedule_popup_refuses_an_empty_interval() {
        let app = schedule_popup(None);
        assert!(schedule_form(&app).submission().is_none());
    }

    #[test]
    fn schedule_popup_prefills_from_the_schedule_in_force() {
        let sched = test_schedule(Some("since_last"), Some(("5m", 300)));
        let app = schedule_popup(Some(&sched));
        let form = schedule_form(&app);
        assert_eq!(form.interval.text(), "1h");
        assert_eq!(form.window.text(), "since_last");
        assert_eq!(form.lag.text(), "5m");
        let request = form.submission().expect("a request");
        assert_eq!(request.interval, "1h");
        assert_eq!(request.window.as_deref(), Some("since_last"));
        assert_eq!(request.lag.as_deref(), Some("5m"));
    }

    #[test]
    fn schedule_popup_prefills_a_zero_lag_as_empty() {
        let sched = test_schedule(Some("2h"), Some(("0s", 0)));
        let app = schedule_popup(Some(&sched));
        assert_eq!(schedule_form(&app).lag.text(), "");
    }

    #[test]
    fn schedule_popup_esc_cancels() {
        let mut app = schedule_popup(None);
        app.handle_key(key(KeyCode::Esc));
        assert!(app.popup.is_none());
    }

    fn budget_schedule(max_runs: Option<u64>, enabled: bool) -> trawl_api::ScheduleResponse {
        trawl_api::ScheduleResponse {
            max_runs,
            enabled,
            ..test_schedule(Some("since_last"), Some(("5m", 300)))
        }
    }

    fn retype_focused(app: &mut App, text: &str) {
        for _ in 0..16 {
            app.handle_key(key(KeyCode::Backspace));
        }
        for ch in text.chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
    }

    /// The popup edits only interval, window and lag, but the PUT replaces
    /// the whole schedule. The form sends back the cap and the paused state
    /// it loaded.
    #[test]
    fn schedule_popup_keeps_max_runs_and_enabled() {
        let cases = [
            (Some(budget_schedule(Some(60), false)), Some(60), false),
            (Some(budget_schedule(None, true)), None, true),
            (Some(budget_schedule(Some(0), true)), Some(0), true),
            (Some(budget_schedule(None, false)), None, false),
            (None, None, true),
        ];
        for (schedule, max_runs, enabled) in cases {
            let mut app = schedule_popup(schedule.as_ref());
            let loaded = schedule_form(&app).submission();
            if schedule.is_some() {
                let loaded = loaded.expect("a prefilled interval");
                assert_eq!(loaded.max_runs, max_runs, "loaded from {schedule:?}");
                assert_eq!(loaded.enabled, enabled, "loaded from {schedule:?}");
            }

            retype_focused(&mut app, "15m");
            let request = schedule_form(&app).submission().expect("a request");
            assert_eq!(request.interval, "15m");
            assert_eq!(request.max_runs, max_runs, "after an edit of {schedule:?}");
            assert_eq!(request.enabled, enabled, "after an edit of {schedule:?}");
        }
    }

    async fn recording_schedule_server(
        saved_id: i64,
        answer: trawl_api::ScheduleResponse,
    ) -> (
        String,
        tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
    ) {
        let (seen_tx, seen_rx) = tokio::sync::mpsc::unbounded_channel();
        let route = axum::routing::put(move |body: axum::body::Bytes| {
            let seen_tx = seen_tx.clone();
            let answer = answer.clone();
            async move {
                let request: serde_json::Value =
                    serde_json::from_slice(&body).expect("a JSON body");
                let _ = seen_tx.send(request);
                axum::Json(answer)
            }
        });
        let app = axum::Router::new().route(&format!("/api/v1/saved/{saved_id}/schedule"), route);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move { axum::serve(listener, app).await });
        (url, seen_rx)
    }

    #[tokio::test]
    async fn schedule_popup_sends_the_kept_budget() {
        let sched = trawl_api::ScheduleResponse {
            saved_query_id: 7,
            ..budget_schedule(Some(60), false)
        };
        let (url, mut seen) = recording_schedule_server(7, sched.clone()).await;
        let client = HttpClient::new_insecure(&url, "flt_test_not_real").expect("client");
        let mut app = App::new(client, DisplayZone::Utc);
        app.saved_cache = Some(ListSavedResponse {
            queries: vec![SavedQueryResponse {
                schedule: Some(sched),
                ..saved_entry(7, "net")
            }],
        });
        app.switch_to_main_tab(MainTab::Saved);
        app.handle_key(key(KeyCode::Char('s')));
        assert_eq!(schedule_form(&app).saved_id, 7);

        retype_focused(&mut app, "15m");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.popup.is_none(), "Enter submits and closes the popup");

        let body = tokio::time::timeout(Duration::from_secs(10), seen.recv())
            .await
            .expect("the PUT reaches the server")
            .expect("a body");
        assert_eq!(body["interval"], "15m", "{body}");
        assert_eq!(body["max_runs"], 60, "{body}");
        assert_eq!(body["enabled"], false, "{body}");
        assert_eq!(body["window"], "since_last", "{body}");
        assert_eq!(body["lag"], "5m", "{body}");
    }

    #[test]
    fn popup_blocks_global_keys() {
        let mut app = test_app();
        app.popup = Some(Popup::SaveQuery {
            editor: SimpleEditor::new_single_line(),
        });
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

    #[test]
    fn key_ctrl_left_moves_word_left() {
        let mut app = test_app();
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

    #[test]
    fn editor_keys_reach_editor() {
        let mut app = test_app();
        assert_eq!(app.focus, Focus::Editor);
        app.handle_key(key(KeyCode::Char('h')));
        app.handle_key(key(KeyCode::Char('i')));
        assert_eq!(app.active_tab().editor.text(), "hi");
    }

    #[test]
    fn results_keys_select_rows() {
        let mut app = test_app();
        app.focus = Focus::Results;
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
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.active_tab().selected_row, None);
    }

    fn event(
        run: (state::RunGeneration, state::RunSource),
        result: Result<QueryResponse, QueryError>,
    ) -> QueryResult {
        QueryResult {
            result,
            duration: Duration::from_millis(1),
            capped: Vec::new(),
            generation: run.0,
            source: run.1,
        }
    }

    fn tagged(app: &App, result: Result<QueryResponse, QueryError>) -> QueryResult {
        event(
            app.tab.execution.current().expect("a run is current"),
            result,
        )
    }

    fn ensure_query(app: &mut App) {
        if app.tab.editor.text().trim().is_empty() {
            app.tab.editor.insert_text("* | stats count() by host");
        }
    }

    fn run_batch(app: &mut App) {
        ensure_query(app);
        app.execute_query();
        assert!(app.tab.execution.is_batch(), "{:?}", app.tab.execution);
    }

    fn run_live(app: &mut App) {
        ensure_query(app);
        app.handle_key(key(KeyCode::F(9)));
        assert!(
            matches!(app.tab.execution, state::Execution::Live { .. }),
            "{:?}",
            app.tab.execution
        );
    }

    #[tokio::test]
    async fn poll_query_result_updates_tab() {
        let mut app = test_app();
        run_batch(&mut app);
        let response = make_query_response(
            vec!["host", "count"],
            vec![vec![Value::String("web-1".into()), Value::Integer(42)]],
        );
        app.query_tx.send(tagged(&app, Ok(response))).unwrap();

        app.poll_query_results();

        assert!(app.tab.result.is_some());
        assert!(matches!(app.tab.status, TabStatus::Success { .. }));
        assert!(
            matches!(app.tab.execution, state::Execution::Idle),
            "the result ends its batch"
        );
    }

    #[tokio::test]
    async fn live_capped_snapshot_marks_results_title() {
        use trawl_api::stream::{SnapshotCap, SnapshotCapKind};

        const MARK: &str = " partial: groups > 10000 ";
        let capped = vec![SnapshotCap {
            cap: SnapshotCapKind::Groups,
            limit: 10_000,
        }];
        let snapshot = |app: &App, capped: Vec<SnapshotCap>| QueryResult {
            capped,
            ..tagged(
                app,
                Ok(make_query_response(
                    vec!["host", "count"],
                    vec![vec![Value::String("web-1".into()), Value::Integer(42)]],
                )),
            )
        };
        let title_row = |app: &mut App, width: u16| {
            let backend = ratatui::backend::TestBackend::new(width, 24);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| ui::render(app, frame)).unwrap();
            let area = app
                .layout
                .results
                .expect("the query layout has a results pane");
            let buffer = terminal.backend().buffer().clone();
            let cells: Vec<_> = (area.x..area.x + area.width)
                .map(|x| buffer[(x, area.y)].clone())
                .collect();
            (cells, terminal.backend().to_string())
        };

        for width in [100_u16, 40] {
            let mut app = test_app();
            run_live(&mut app);
            app.query_tx.send(snapshot(&app, capped.clone())).unwrap();
            app.poll_query_results();
            assert!(
                matches!(app.tab.status, TabStatus::Success { .. }),
                "a capped snapshot is a result: {:?}",
                app.tab.status
            );

            let (cells, screen) = title_row(&mut app, width);
            let text: String = cells.iter().map(ratatui::buffer::Cell::symbol).collect();
            let at = text
                .find(MARK)
                .unwrap_or_else(|| panic!("width {width}: no mark in {text:?}"));
            let start = text[..at].chars().count();
            for cell in &cells[start..start + MARK.chars().count()] {
                assert_eq!(
                    cell.fg, app.theme.status_warning,
                    "width {width}: the mark is a warning: {text:?}"
                );
            }
            assert!(
                screen.contains("web-1"),
                "width {width}: rows render\n{screen}"
            );

            app.query_tx.send(snapshot(&app, Vec::new())).unwrap();
            app.poll_query_results();
            let (cells, _) = title_row(&mut app, width);
            let text: String = cells.iter().map(ratatui::buffer::Cell::symbol).collect();
            assert!(
                !text.contains("partial"),
                "width {width}: an uncapped snapshot clears the mark: {text:?}"
            );
        }
    }

    #[tokio::test]
    async fn live_capped_snapshot_marks_every_placeholder_title() {
        use trawl_api::stream::{SnapshotCap, SnapshotCapKind};

        const MARK: &str = " partial: groups > 10000 ";
        let capped = || {
            vec![SnapshotCap {
                cap: SnapshotCapKind::Groups,
                limit: 10_000,
            }]
        };
        let title = |app: &mut App| {
            let backend = ratatui::backend::TestBackend::new(100, 24);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| ui::render(app, frame)).unwrap();
            let area = app
                .layout
                .results
                .expect("the query layout has a results pane");
            let buffer = terminal.backend().buffer().clone();
            let cells: Vec<_> = (area.x..area.x + area.width)
                .map(|x| buffer[(x, area.y)].clone())
                .collect();
            (cells, terminal.backend().to_string())
        };
        let assert_marked = |app: &mut App, case: &str, shows: &str| {
            let (cells, screen) = title(app);
            let text: String = cells.iter().map(ratatui::buffer::Cell::symbol).collect();
            let at = text
                .find(MARK)
                .unwrap_or_else(|| panic!("{case}: no mark in {text:?}\n{screen}"));
            let start = text[..at].chars().count();
            for cell in &cells[start..start + MARK.chars().count()] {
                assert_eq!(
                    cell.fg, app.theme.status_warning,
                    "{case}: the mark is a warning: {text:?}"
                );
            }
            assert!(screen.contains(shows), "{case}: shows {shows:?}\n{screen}");
        };

        for view in [ChartView::LineChart, ChartView::Sparkline] {
            let mut app = test_app();
            run_live(&mut app);
            app.query_tx
                .send(QueryResult {
                    capped: capped(),
                    ..tagged(
                        &app,
                        Ok(make_query_response(vec!["_time", "host", "count"], vec![])),
                    )
                })
                .unwrap();
            app.poll_query_results();
            assert_eq!(
                app.tab.chart_view,
                ChartView::LineChart,
                "a timechart opens as a line chart"
            );
            app.tab.chart_view = view;
            assert_marked(&mut app, &format!("{view:?}"), "Command Palette");
        }

        let mut app = test_app();
        run_live(&mut app);
        app.query_tx
            .send(QueryResult {
                capped: capped(),
                ..tagged(
                    &app,
                    Ok(make_query_response(
                        vec!["host", "count"],
                        vec![vec![Value::String("web-1".into()), Value::Integer(42)]],
                    )),
                )
            })
            .unwrap();
        app.poll_query_results();
        for column in &mut app.tab.column_config.as_mut().unwrap().columns {
            column.hidden = true;
        }
        assert_marked(&mut app, "all columns hidden", "All columns hidden");
    }

    #[tokio::test]
    async fn live_capped_snapshot_marks_results_title_while_the_next_query_runs() {
        use trawl_api::stream::{SnapshotCap, SnapshotCapKind};

        const MARK: &str = " partial: groups > 10000 ";
        let mut app = test_app();
        run_live(&mut app);
        app.query_tx
            .send(QueryResult {
                capped: vec![SnapshotCap {
                    cap: SnapshotCapKind::Groups,
                    limit: 10_000,
                }],
                ..tagged(
                    &app,
                    Ok(make_query_response(
                        vec!["host", "count"],
                        vec![vec![Value::String("web-1".into()), Value::Integer(42)]],
                    )),
                )
            })
            .unwrap();
        app.poll_query_results();
        assert!(app.live_mode);

        app.execute_query();
        assert!(!app.live_mode, "the batch query stops the live stream");
        assert!(
            matches!(app.tab.status, TabStatus::Running { .. }),
            "{:?}",
            app.tab.status
        );

        let backend = ratatui::backend::TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| ui::render(&mut app, frame)).unwrap();
        let area = app
            .layout
            .results
            .expect("the query layout has a results pane");
        let buffer = terminal.backend().buffer().clone();
        let cells: Vec<_> = (area.x..area.x + area.width)
            .map(|x| buffer[(x, area.y)].clone())
            .collect();
        let screen = terminal.backend().to_string();
        let text: String = cells.iter().map(ratatui::buffer::Cell::symbol).collect();
        assert!(
            screen.contains("web-1"),
            "the capped rows stay drawn\n{screen}"
        );
        assert!(text.contains("Running..."), "{text:?}");
        let at = text
            .find(MARK)
            .unwrap_or_else(|| panic!("no mark in {text:?}\n{screen}"));
        let start = text[..at].chars().count();
        for cell in &cells[start..start + MARK.chars().count()] {
            assert_eq!(
                cell.fg, app.theme.status_warning,
                "the mark is a warning: {text:?}"
            );
        }
    }

    #[tokio::test]
    async fn live_capped_snapshot_marks_results_title_across_a_live_restart() {
        use trawl_api::stream::{SnapshotCap, SnapshotCapKind};

        const MARK: &str = " partial: groups > 10000 ";
        let mut app = test_app();
        run_live(&mut app);
        app.query_tx
            .send(QueryResult {
                capped: vec![SnapshotCap {
                    cap: SnapshotCapKind::Groups,
                    limit: 10_000,
                }],
                ..tagged(
                    &app,
                    Ok(make_query_response(
                        vec!["host", "count"],
                        vec![vec![Value::String("web-1".into()), Value::Integer(42)]],
                    )),
                )
            })
            .unwrap();
        app.poll_query_results();

        app.handle_key(key(KeyCode::F(9)));
        app.handle_key(key(KeyCode::F(9)));
        assert!(app.live_mode, "the second F9 restarted the stream");

        let backend = ratatui::backend::TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| ui::render(&mut app, frame)).unwrap();
        let area = app
            .layout
            .results
            .expect("the query layout has a results pane");
        let buffer = terminal.backend().buffer().clone();
        let text: String = (area.x..area.x + area.width)
            .map(|x| buffer[(x, area.y)].symbol().to_owned())
            .collect();
        let screen = terminal.backend().to_string();
        assert!(
            screen.contains("web-1"),
            "the capped rows stay drawn\n{screen}"
        );
        assert!(
            text.contains(MARK),
            "the capped rows keep their mark: {text:?}\n{screen}"
        );
    }

    #[tokio::test]
    async fn poll_query_result_sets_error_status() {
        let mut app = test_app();
        run_batch(&mut app);
        app.query_tx
            .send(tagged(
                &app,
                Err(QueryError {
                    message: "something broke".to_owned(),
                    details: Vec::new(),
                    query: "* | stats count() by host".to_owned(),
                }),
            ))
            .unwrap();

        app.poll_query_results();

        assert!(matches!(
            app.tab.status,
            TabStatus::Error { ref message, ref query, .. }
                if message == "something broke"
                    && query.as_deref() == Some("* | stats count() by host")
        ));
    }

    async fn held_parse_error_server(
        span: (usize, usize),
    ) -> (
        String,
        tokio::sync::mpsc::UnboundedReceiver<String>,
        std::sync::Arc<tokio::sync::Notify>,
    ) {
        use axum::http::{StatusCode, header};

        let (seen_tx, seen_rx) = tokio::sync::mpsc::unbounded_channel();
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let gate = std::sync::Arc::clone(&release);
        let route = axum::routing::post(move |body: axum::body::Bytes| {
            let seen_tx = seen_tx.clone();
            let gate = std::sync::Arc::clone(&gate);
            async move {
                let request: trawl_api::QueryRequest =
                    serde_json::from_slice(&body).expect("a query request");
                let _ = seen_tx.send(request.query);
                gate.notified().await;
                let envelope = trawl_client::ErrorResponse {
                    error: trawl_client::ErrorEnvelope {
                        code: trawl_client::ErrorCode::ParseError,
                        message: "parse error: unclosed group".to_owned(),
                        details: vec![trawl_client::ErrorDetail {
                            message: "unclosed group".to_owned(),
                            span: Some(trawl_client::ErrorSpan {
                                start: span.0,
                                end: span.1,
                            }),
                            label: None,
                            hint: Some("close the group".to_owned()),
                        }],
                        conflict: None,
                    },
                };
                (
                    StatusCode::BAD_REQUEST,
                    [(header::CONTENT_TYPE, "application/json")],
                    serde_json::to_vec(&envelope).expect("envelope"),
                )
            }
        });
        let app = axum::Router::new().route("/api/v1/query", route);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move { axum::serve(listener, app).await });
        (url, seen_rx, release)
    }

    #[tokio::test]
    async fn tui_error_excerpt_uses_submitted_query() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        const SENT: &str = "service=web | stats count( by host";
        const DRAFT: &str = "service=api | head 5";
        let span = (27, 29);
        assert_eq!(&SENT[span.0..span.1], "by");

        let (url, mut seen, release) = held_parse_error_server(span).await;
        let client = HttpClient::new_insecure(&url, "flt_test_not_real").expect("client");
        let mut app = App::new(client, DisplayZone::Utc);
        app.tab.editor.insert_text(SENT);
        app.execute_query();

        let bound = Duration::from_secs(10);
        let received = tokio::time::timeout(bound, seen.recv())
            .await
            .expect("the query reaches the server")
            .expect("a query");
        assert_eq!(received, SENT, "the run sends the editor's query");

        app.tab.editor.clear();
        app.tab.editor.insert_text(DRAFT);
        assert!(matches!(app.tab.status, TabStatus::Running { .. }));
        release.notify_one();

        tokio::time::timeout(bound, async {
            while app.tab.execution.is_batch() {
                app.poll_query_results();
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the held answer arrives");

        match &app.tab.status {
            TabStatus::Error { query, details, .. } => {
                assert_eq!(query.as_deref(), Some(SENT));
                assert_eq!(details.len(), 1);
            }
            other => panic!("expected the parse error, got {other:?}"),
        }
        assert_eq!(
            app.tab.editor.text(),
            DRAFT,
            "the draft stays in the editor"
        );

        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
        terminal
            .draw(|f| crate::tui::ui::render(&mut app, f))
            .expect("render");
        let screen = terminal.backend().to_string();
        let rows: Vec<&str> = screen.lines().collect();
        let excerpt_rows: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.contains(SENT))
            .map(|(at, _)| at)
            .collect();
        assert_eq!(
            excerpt_rows.len(),
            1,
            "the sent query is quoted once, in the error pane:\n{screen}"
        );
        let excerpt_row = rows[excerpt_rows[0]];
        let query_column = excerpt_row
            .chars()
            .collect::<Vec<_>>()
            .windows(SENT.len())
            .position(|w| w.iter().copied().eq(SENT.chars()))
            .expect("the excerpt row holds the query");
        let caret_row: Vec<char> = rows[excerpt_rows[0] + 1].chars().collect();
        let first_caret = caret_row
            .iter()
            .position(|&c| c == '^')
            .unwrap_or_else(|| panic!("a caret row under the excerpt:\n{screen}"));
        assert_eq!(first_caret, query_column + span.0, "{screen}");
        assert_eq!(
            caret_row.iter().filter(|&&c| c == '^').count(),
            2,
            "two carets mark `by`:\n{screen}"
        );
    }

    fn buffer_row(buffer: &ratatui::buffer::Buffer, y: u16) -> Vec<String> {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol().to_owned())
            .collect()
    }

    fn find_cells(buffer: &ratatui::buffer::Buffer, fragment: &str) -> Option<(u16, u16)> {
        let wanted: Vec<String> = fragment.chars().map(String::from).collect();
        (0..buffer.area.height).find_map(|y| {
            let row = buffer_row(buffer, y);
            row.windows(wanted.len())
                .position(|cells| cells == wanted.as_slice())
                .map(|x| (u16::try_from(x).expect("a column"), y))
        })
    }

    fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| buffer_row(buffer, y).concat())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn terminal_loaded_query_keeps_raw_text() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        // An unclosed group does not parse, so the run sends the text as
        // typed: the formatter never rewrites it. No outer whitespace and
        // no CR, which the run trims and the editor drops.
        const HOSTILE: &str =
            "msg=\"ab\u{202e}cd\u{200b}ef\u{00ad}gh\" | stats count( by host\n| head 5";
        const DRAWN: &str = "ab\u{fffd}cd\u{fffd}ef\u{fffd}gh";
        const FORBIDDEN: [char; 3] = ['\u{202e}', '\u{200b}', '\u{00ad}'];
        assert!(trawl_core::parser::parse(HOSTILE).is_err());
        assert_eq!(HOSTILE.trim(), HOSTILE);
        let assert_clean = |screen: &str, surface: &str| {
            for forbidden in FORBIDDEN {
                assert!(
                    !screen.contains(forbidden),
                    "{surface}: U+{:04X} drawn:\n{screen}",
                    u32::from(forbidden)
                );
            }
        };

        let (url, mut seen, release) = held_parse_error_server((0, 1)).await;
        let client = HttpClient::new_insecure(&url, "flt_test_not_real").expect("client");
        let mut app = App::new(client, DisplayZone::Utc);
        let mut shared = saved_entry(7, "qxzv");
        shared.query = HOSTILE.to_owned();
        app.saved_cache = Some(ListSavedResponse {
            queries: vec![shared],
        });

        app.handle_key(key_mod(KeyCode::Char('p'), KeyModifiers::CONTROL));
        for ch in "qxzv".chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
        {
            let Some(Popup::CommandPalette {
                ref filtered,
                ref items,
                selected,
                ..
            }) = app.popup
            else {
                panic!("the palette is open");
            };
            let action = &items[filtered[selected].item_index].action;
            assert!(
                matches!(action, palette::PaletteAction::LoadQuery(q) if q == HOSTILE),
                "the palette selects the saved query: {action:?}"
            );
        }
        app.handle_key(key(KeyCode::Enter));
        assert!(app.popup.is_none());
        assert_eq!(app.focus, Focus::Editor);
        assert_eq!(
            app.tab.editor.text(),
            HOSTILE,
            "the buffer holds the saved text"
        );

        // The cursor sits right after the U+202E, ASCII before it, with no
        // wrap, ghost or selection on the line.
        app.tab.editor.cursor = (0, 8);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("terminal");
        terminal
            .draw(|f| crate::tui::ui::render(&mut app, f))
            .expect("render");
        let buffer = terminal.backend().buffer().clone();
        let screen = buffer_text(&buffer);
        assert_clean(&screen, "editor");
        let (start, row) = find_cells(&buffer, DRAWN)
            .unwrap_or_else(|| panic!("editor does not draw {DRAWN:?}:\n{screen}"));
        let editor = app.layout.editor.expect("the editor is laid out");
        assert!(
            editor.contains(ratatui::layout::Position::new(start, row)),
            "the fragment is in the editor {editor:?}, at ({start}, {row}):\n{screen}"
        );
        let replacement = start + 2;
        assert_eq!(buffer[(replacement, row)].symbol(), "\u{fffd}");
        assert_eq!(
            terminal.get_cursor_position().expect("cursor"),
            ratatui::layout::Position::new(replacement + 1, row),
            "the cursor is the cell right after the drawn U+FFFD:\n{screen}"
        );
        assert_eq!(app.tab.editor.text(), HOSTILE, "drawing leaves the buffer");

        app.history_cache = Some(trawl_api::HistoryResponse {
            entries: vec![trawl_api::HistoryEntryResponse {
                id: 1,
                query: HOSTILE.to_owned(),
                executed_at: "2026-03-18T14:30:00Z".to_owned(),
                duration_ms: 3,
                row_count: 0,
                status: trawl_client::QueryStatus::Success,
            }],
            total: 1,
        });
        app.panel.history_selected = 0;
        let mut pane = Terminal::new(TestBackend::new(80, 12)).expect("terminal");
        pane.draw(|f| crate::tui::ui::history::render_detail_pane(&app, f, f.area()))
            .expect("render");
        let history = buffer_text(pane.backend().buffer());
        assert_clean(&history, "history pane");
        assert!(
            history.contains(DRAWN),
            "history pane does not draw {DRAWN:?}:\n{history}"
        );

        app.tab.editor.cursor = (1, 8);
        app.tab.ghost = Some(autocomplete::Completion {
            ghost_text: "gq\u{202e}gz".to_owned(),
            insert_text: String::new(),
            replace_len: 0,
            cursor_offset: None,
        });
        app.tab.validation_errors = vec![trawl_core::parser::ParseError {
            message: "mq\u{200b}mz".to_owned(),
            span: 0..1,
            label: None,
            hint: Some("hq\u{00ad}hz".to_owned()),
        }];
        terminal
            .draw(|f| crate::tui::ui::render(&mut app, f))
            .expect("render");
        let screen = buffer_text(terminal.backend().buffer());
        assert_clean(&screen, "ghost and validation");
        for (surface, fragment) in [
            ("ghost text", "gq\u{fffd}gz"),
            ("validation message", "mq\u{fffd}mz"),
            ("validation hint", "hq\u{fffd}hz"),
        ] {
            assert!(
                screen.contains(fragment),
                "{surface} does not draw {fragment:?}:\n{screen}"
            );
        }
        assert_eq!(app.tab.editor.text(), HOSTILE, "drawing leaves the buffer");

        app.tab.ghost = None;
        app.tab.validation_errors.clear();
        app.execute_query();
        let received = tokio::time::timeout(Duration::from_secs(10), seen.recv())
            .await
            .expect("the query reaches the server")
            .expect("a query");
        assert_eq!(received, HOSTILE, "the run sends the saved text");
        release.notify_one();
    }

    #[tokio::test]
    async fn poll_query_result_resets_scroll() {
        let mut app = test_app();
        run_batch(&mut app);
        app.tab.scroll_offset = 42;
        app.tab.horizontal_scroll_offset = 7;

        let response = make_query_response(vec!["x"], vec![vec![Value::Integer(1)]]);
        app.query_tx.send(tagged(&app, Ok(response))).unwrap();

        app.poll_query_results();

        assert_eq!(app.tab.scroll_offset, 0);
        assert_eq!(app.tab.horizontal_scroll_offset, 0);
    }

    #[tokio::test]
    async fn poll_query_result_timechart_auto_switches_view() {
        let mut app = test_app();
        run_batch(&mut app);
        assert_eq!(app.tab.chart_view, ChartView::Table);

        let response = make_query_response(
            vec!["_time", "count"],
            vec![vec![
                Value::String("2025-01-01T00:00:00Z".into()),
                Value::Integer(10),
            ]],
        );
        app.query_tx.send(tagged(&app, Ok(response))).unwrap();

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
        app.handle_key(key(KeyCode::F(1)));
        assert!(matches!(app.popup, Some(Popup::Error { .. })));
    }

    #[tokio::test]
    async fn multiple_results_last_wins() {
        let mut app = test_app();
        run_live(&mut app);

        for i in 0..2 {
            let response =
                make_query_response(vec!["idx"], vec![vec![Value::Integer(i64::from(i))]]);
            app.query_tx.send(tagged(&app, Ok(response))).unwrap();
        }

        app.poll_query_results();

        assert!(app.tab.result.is_some());
        let rows = &app.tab.result.as_ref().unwrap().result.rows;
        assert_eq!(rows[0][0], Value::Integer(1));
    }

    /// Swaps in a task that never ends: the real task's request to the dummy
    /// server fails and finishes by itself, which would pass for an abort.
    fn hold_pending(handle: &mut tokio::task::JoinHandle<()>) -> tokio::task::AbortHandle {
        let held = tokio::spawn(std::future::pending::<()>());
        let abort = held.abort_handle();
        std::mem::replace(handle, held).abort();
        abort
    }

    fn batch_abort_handle(app: &mut App) -> tokio::task::AbortHandle {
        match &mut app.tab.execution {
            state::Execution::Batch { handle, .. } => hold_pending(handle),
            other => panic!("expected a batch run, got {other:?}"),
        }
    }

    fn live_abort_handle(app: &mut App) -> tokio::task::AbortHandle {
        match &mut app.tab.execution {
            state::Execution::Live { handle, .. } => hold_pending(handle),
            other => panic!("expected a live run, got {other:?}"),
        }
    }

    async fn assert_finishes(task: &tokio::task::AbortHandle) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !task.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the aborted task finishes");
    }

    /// A query whose formatted text would be over the query limit: 40 000
    /// tabs in a quoted string each print as the two bytes `\t`.
    fn too_long_to_format() -> String {
        format!("\"{}\"", "\t".repeat(40_000))
    }

    #[test]
    fn format_refusal_keeps_the_buffer() {
        let mut app = test_app();
        let query = too_long_to_format();
        app.tab.editor.insert_text("service=web");
        app.tab.editor.save_snapshot();
        app.tab.editor.clear();
        app.tab.editor.insert_text(&query);
        let before = format!("{:?}", app.tab.editor);
        let mut untouched = app.tab.editor.clone();

        app.format_editor_query();

        assert_eq!(app.tab.editor.text(), query);
        assert_eq!(
            format!("{:?}", app.tab.editor),
            before,
            "the editor and its undo history are unchanged"
        );
        match &app.tab.status {
            TabStatus::Error {
                message,
                details,
                query,
            } => {
                assert_eq!(
                    message,
                    "not formatted: the formatted query would be 80002 bytes, \
                     over the 65536-byte query limit"
                );
                assert!(details.is_empty());
                assert_eq!(query, &None, "a local refusal sent no query");
            }
            other => panic!("expected the refusal on the tab status, got {other:?}"),
        }

        for _ in 0..3 {
            app.tab.editor.undo();
            untouched.undo();
            assert_eq!(
                app.tab.editor.text(),
                untouched.text(),
                "undo steps back through the history the edits made"
            );
        }
        assert_eq!(app.tab.editor.text(), "service=web");
    }

    #[test]
    fn format_rewrites_a_query_it_can_write_faithfully() {
        let mut app = test_app();
        app.tab.editor.insert_text("service=web  |  head 10");

        app.format_editor_query();

        assert_eq!(app.tab.editor.text(), "service=web\n| head 10");
        assert!(matches!(app.tab.status, TabStatus::Idle));
    }

    #[tokio::test]
    async fn execute_after_format_refusal_runs_the_query_as_typed() {
        let mut app = test_app();
        let query = too_long_to_format();
        app.tab.editor.insert_text(&query);
        let before = format!("{:?}", app.tab.editor);

        app.execute_query();

        assert_eq!(app.tab.editor.text(), query);
        assert_eq!(format!("{:?}", app.tab.editor), before);
        assert!(
            matches!(app.tab.status, TabStatus::Running { .. }),
            "{:?}",
            app.tab.status
        );
    }

    #[tokio::test]
    async fn format_refusal_during_a_run_keeps_it_cancellable() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = test_app();
        app.tab.editor.insert_text("service=web");
        app.execute_query();
        assert!(app.tab.execution.is_batch());

        app.tab.editor.clear();
        app.tab.editor.insert_text(&too_long_to_format());
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
        assert!(
            matches!(&app.tab.status, TabStatus::Error { message, .. } if message.starts_with("not formatted: ")),
            "{:?}",
            app.tab.status
        );

        let in_flight = batch_abort_handle(&mut app);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            matches!(app.tab.execution, state::Execution::Idle),
            "Esc ends the run"
        );
        assert_finishes(&in_flight).await;
        assert!(
            matches!(&app.tab.status, TabStatus::Error { message, .. } if message == "cancelled"),
            "{:?}",
            app.tab.status
        );
    }

    struct Driver {
        tx: mpsc::UnboundedSender<DriverCommand>,
        terminal: Terminal<ratatui::backend::TestBackend>,
    }

    impl Driver {
        fn attach(app: &mut App) -> Self {
            let (tx, rx) = mpsc::unbounded_channel();
            app.driver_rx = Some(rx);
            let terminal = Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
            Self { tx, terminal }
        }

        fn send_execute(
            &mut self,
            app: &mut App,
            reply: tokio::sync::oneshot::Sender<DriverResponse>,
        ) {
            self.tx
                .send(DriverCommand {
                    request: driver::DriverRequest::Execute { timeout_ms: 10_000 },
                    reply,
                })
                .unwrap();
            app.poll_driver_commands(&mut self.terminal);
        }

        fn execute(&mut self, app: &mut App) -> tokio::sync::oneshot::Receiver<DriverResponse> {
            let (reply, answer) = tokio::sync::oneshot::channel();
            self.send_execute(app, reply);
            answer
        }
    }

    fn assert_unanswered(answer: &mut tokio::sync::oneshot::Receiver<DriverResponse>, case: &str) {
        match answer.try_recv() {
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
            Ok(response) => panic!("{case}: answered early with {response:?}"),
            Err(closed) => panic!("{case}: reply dropped unanswered: {closed:?}"),
        }
    }

    fn assert_answered_err(
        answer: &mut tokio::sync::oneshot::Receiver<DriverResponse>,
        expected: &str,
        case: &str,
    ) {
        match answer.try_recv() {
            Ok(response) => {
                assert!(!response.ok, "{case}: {response:?}");
                assert_eq!(response.error.as_deref(), Some(expected), "{case}");
            }
            Err(e) => panic!("{case}: expected {expected:?}, got {e:?}"),
        }
    }

    fn assert_admitted(app: &App, case: &str) {
        assert!(
            app.tab.execution.reply_pending(),
            "{case}: the execute was admitted: {:?}",
            app.tab.execution
        );
    }

    fn current_generation(app: &App) -> state::RunGeneration {
        app.tab.execution.current().expect("a run is current").0
    }

    fn palette_clear(app: &mut App) {
        app.handle_key(key_mod(KeyCode::Char('p'), KeyModifiers::CONTROL));
        for ch in "Clear Editor".chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
        let Some(Popup::CommandPalette {
            ref filtered,
            ref items,
            selected,
            ..
        }) = app.popup
        else {
            panic!("the palette is open");
        };
        let action = &items[filtered[selected].item_index].action;
        assert!(
            matches!(
                action,
                palette::PaletteAction::RunAction(palette::ActionKind::ClearEditor)
            ),
            "the palette selects Clear Editor: {action:?}"
        );
        app.handle_key(key(KeyCode::Enter));
        assert!(app.popup.is_none());
    }

    #[tokio::test]
    async fn driver_execute_terminal_transitions() {
        type Transition = fn(&mut App);
        let cases: [(&str, &str, Transition); 9] = [
            ("Esc", "cancelled", |app| app.handle_key(key(KeyCode::Esc))),
            ("editor Ctrl+C", "cancelled", |app| {
                app.focus = Focus::Editor;
                app.handle_key(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL));
            }),
            ("results Ctrl+C", "cancelled", |app| {
                app.focus = Focus::Results;
                app.handle_key(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL));
            }),
            ("results Ctrl+C while typing a search", "cancelled", |app| {
                app.focus = Focus::Results;
                let mut search = state::ResultsSearch::new();
                search.input_active = true;
                app.results_search = Some(search);
                app.handle_key(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL));
            }),
            ("results Ctrl+C in column mode", "cancelled", |app| {
                app.focus = Focus::Results;
                let mut columns = state::ColumnConfig::init(&[]);
                columns.selected = Some(0);
                app.tab.column_config = Some(columns);
                app.handle_key(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL));
            }),
            ("Ctrl+L", "cleared", |app| {
                app.handle_key(key_mod(KeyCode::Char('l'), KeyModifiers::CONTROL));
            }),
            ("palette clear", "cleared", palette_clear),
            ("F5", "replaced", |app| app.handle_key(key(KeyCode::F(5)))),
            ("F9", "replaced by live stream", |app| {
                app.handle_key(key(KeyCode::F(9)));
            }),
        ];

        for (case, expected, transition) in cases {
            let mut app = test_app();
            let mut driver = Driver::attach(&mut app);
            ensure_query(&mut app);
            let mut answer = driver.execute(&mut app);
            assert_unanswered(&mut answer, case);
            assert_admitted(&app, case);

            transition(&mut app);
            assert_answered_err(&mut answer, expected, case);
            assert!(!app.tab.execution.reply_pending(), "{case}");

            app.focus = Focus::Editor;
            ensure_query(&mut app);
            let mut next = driver.execute(&mut app);
            assert_unanswered(&mut next, case);
            assert_admitted(&app, &format!("{case}, then"));
        }

        let mut app = test_app();
        let mut driver = Driver::attach(&mut app);
        ensure_query(&mut app);
        let mut answer = driver.execute(&mut app);
        app.tab.editor.selection_anchor = Some((0, 0));
        assert!(app.tab.editor.selected_text().is_some());
        app.handle_key(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_unanswered(&mut answer, "Ctrl+C over a selection");
        assert_admitted(&app, "Ctrl+C over a selection");
    }

    #[tokio::test]
    async fn query_generation_rejects_queued_results() {
        type Transition = fn(&mut App);
        let cases: [(&str, &str, Transition); 3] = [
            ("Esc", "cancelled", |app| app.handle_key(key(KeyCode::Esc))),
            ("Ctrl+L", "cleared", |app| {
                app.handle_key(key_mod(KeyCode::Char('l'), KeyModifiers::CONTROL));
            }),
            ("F5", "replaced", |app| app.handle_key(key(KeyCode::F(5)))),
        ];

        for (case, expected, transition) in cases {
            let mut app = test_app();
            let mut driver = Driver::attach(&mut app);
            ensure_query(&mut app);
            let mut first = driver.execute(&mut app);
            let first_run = app.tab.execution.current().unwrap();
            app.query_tx
                .send(event(
                    first_run,
                    Ok(make_query_response(
                        vec!["x"],
                        vec![vec![Value::Integer(1)]],
                    )),
                ))
                .unwrap();

            transition(&mut app);
            ensure_query(&mut app);
            let mut second = driver.execute(&mut app);
            let second_generation = current_generation(&app);
            assert_ne!(second_generation, first_run.0, "{case}");

            app.poll_query_results();

            assert_answered_err(&mut first, expected, case);
            assert_unanswered(&mut second, case);
            assert_admitted(&app, case);
            assert_eq!(current_generation(&app), second_generation, "{case}");
            assert!(
                matches!(app.tab.status, TabStatus::Running { .. }),
                "{case}: {:?}",
                app.tab.status
            );
            assert!(
                app.tab.result.is_none(),
                "{case}: the old rows are not drawn"
            );
        }
    }

    #[tokio::test]
    async fn driver_execute_answers_with_its_own_result() {
        let mut app = test_app();
        let mut driver = Driver::attach(&mut app);
        ensure_query(&mut app);
        let mut answer = driver.execute(&mut app);
        app.query_tx
            .send(tagged(
                &app,
                Ok(make_query_response(
                    vec!["host"],
                    vec![
                        vec![Value::String("a".to_owned())],
                        vec![Value::String("b".to_owned())],
                    ],
                )),
            ))
            .unwrap();
        app.poll_query_results();
        let response = answer.try_recv().expect("the result answers the execute");
        assert!(response.ok, "{response:?}");
        assert_eq!(response.data.row_count, Some(2));
        assert_eq!(response.data.columns, Some(vec!["host".to_owned()]));
        assert_eq!(
            response.data.rows,
            Some(vec![
                vec![serde_json::json!("a")],
                vec![serde_json::json!("b")]
            ])
        );
        assert_eq!(response.data.duration_ms, Some(1));
        assert!(matches!(app.tab.execution, state::Execution::Idle));

        let mut answer = driver.execute(&mut app);
        assert_admitted(&app, "after a success");
        app.query_tx
            .send(tagged(
                &app,
                Err(QueryError {
                    message: "something broke".to_owned(),
                    details: Vec::new(),
                    query: "* | stats count() by host".to_owned(),
                }),
            ))
            .unwrap();
        app.poll_query_results();
        assert_answered_err(&mut answer, "something broke", "a failed batch");
        assert!(matches!(app.tab.execution, state::Execution::Idle));
    }

    #[tokio::test]
    async fn live_and_batch_ownership() {
        let mut app = test_app();
        let mut driver = Driver::attach(&mut app);
        run_live(&mut app);
        let snapshot = tagged(
            &app,
            Ok(make_query_response(
                vec!["x"],
                vec![vec![Value::Integer(1)]],
            )),
        );
        app.query_tx.send(snapshot).unwrap();
        let mut answer = driver.execute(&mut app);
        assert!(!app.live_mode, "the batch stops the stream");
        app.poll_query_results();
        assert!(
            matches!(app.tab.status, TabStatus::Running { .. }),
            "{:?}",
            app.tab.status
        );
        assert!(app.tab.result.is_none(), "the snapshot is not drawn");
        assert_unanswered(&mut answer, "after a stale snapshot");
        assert_admitted(&app, "after a stale snapshot");
        app.handle_key(key(KeyCode::Esc));
        assert_answered_err(&mut answer, "cancelled", "Esc after a stale snapshot");

        let mut app = test_app();
        run_live(&mut app);
        let generation = current_generation(&app);
        let snapshot = tagged(
            &app,
            Ok(make_query_response(
                vec!["x"],
                vec![vec![Value::Integer(1)]],
            )),
        );
        app.query_tx.send(snapshot).unwrap();
        app.poll_query_results();
        assert!(app.tab.result.is_some(), "the current snapshot is drawn");
        assert!(matches!(app.tab.status, TabStatus::Success { .. }));
        assert!(
            matches!(app.tab.execution, state::Execution::Live { generation: g, .. } if g == generation),
            "{:?}",
            app.tab.execution
        );

        let mut app = test_app();
        let mut driver = Driver::attach(&mut app);
        ensure_query(&mut app);
        let mut answer = driver.execute(&mut app);
        let batch_run = app.tab.execution.current().unwrap();
        let batch = batch_abort_handle(&mut app);
        app.handle_key(key(KeyCode::F(9)));
        assert_answered_err(&mut answer, "replaced by live stream", "F9");
        assert!(
            matches!(app.tab.execution, state::Execution::Live { .. }),
            "{:?}",
            app.tab.execution
        );
        assert!(
            matches!(app.tab.status, TabStatus::Idle),
            "nothing is left running: {:?}",
            app.tab.status
        );
        assert_finishes(&batch).await;
        app.query_tx
            .send(event(
                batch_run,
                Ok(make_query_response(
                    vec!["x"],
                    vec![vec![Value::Integer(1)]],
                )),
            ))
            .unwrap();
        app.poll_query_results();
        assert!(
            app.tab.result.is_none(),
            "the batch's late rows are dropped"
        );
        assert!(
            matches!(app.tab.execution, state::Execution::Live { .. }),
            "{:?}",
            app.tab.execution
        );
    }

    #[tokio::test]
    async fn driver_timeout_releases_ownership() {
        let mut app = test_app();
        let mut driver = Driver::attach(&mut app);
        ensure_query(&mut app);
        let mut first = driver.execute(&mut app);
        let first_generation = current_generation(&app);
        let mut refused = driver.execute(&mut app);
        assert_answered_err(
            &mut refused,
            "another execute is already pending",
            "a second execute",
        );
        assert_unanswered(&mut first, "the first execute");
        assert_eq!(current_generation(&app), first_generation);
        drop(first);
        let mut next = driver.execute(&mut app);
        assert_unanswered(&mut next, "after the first client left");
        assert_admitted(&app, "after the first client left");
        assert_ne!(current_generation(&app), first_generation);

        let mut app = test_app();
        let mut driver = Driver::attach(&mut app);
        ensure_query(&mut app);
        let (reply, answer) = tokio::sync::oneshot::channel();
        drop(answer);
        driver.send_execute(&mut app, reply);
        assert!(
            matches!(app.tab.execution, state::Execution::Idle),
            "{:?}",
            app.tab.execution
        );
        assert!(matches!(app.tab.status, TabStatus::Idle));

        let mut app = test_app();
        let mut driver = Driver::attach(&mut app);
        ensure_query(&mut app);
        drop(driver.execute(&mut app));
        app.handle_key(key(KeyCode::Esc));
        assert!(
            matches!(app.tab.execution, state::Execution::Idle),
            "{:?}",
            app.tab.execution
        );
        let mut next = driver.execute(&mut app);
        assert_unanswered(&mut next, "after the cancel");
        assert_admitted(&app, "after the cancel");

        let mut app = test_app();
        let mut driver = Driver::attach(&mut app);
        ensure_query(&mut app);
        drop(driver.execute(&mut app));
        let result = tagged(
            &app,
            Ok(make_query_response(
                vec!["x"],
                vec![vec![Value::Integer(1)]],
            )),
        );
        app.query_tx.send(result).unwrap();
        app.poll_query_results();
        assert!(
            matches!(app.tab.execution, state::Execution::Idle),
            "{:?}",
            app.tab.execution
        );
        assert!(app.tab.result.is_some(), "the result is drawn");
        let mut next = driver.execute(&mut app);
        assert_unanswered(&mut next, "after the result");
        assert_admitted(&app, "after the result");
    }

    #[tokio::test]
    async fn ctrl_c_cancels_after_format_refusal() {
        for focus in [Focus::Editor, Focus::Results] {
            let mut app = test_app();
            app.tab.editor.insert_text("service=web");
            app.handle_key(key(KeyCode::F(5)));
            let in_flight = batch_abort_handle(&mut app);

            app.tab.editor.clear();
            app.tab.editor.insert_text(&too_long_to_format());
            app.handle_key(key_mod(KeyCode::Char('f'), KeyModifiers::CONTROL));
            assert!(
                matches!(&app.tab.status, TabStatus::Error { message, .. } if message.starts_with("not formatted: ")),
                "{focus:?}: {:?}",
                app.tab.status
            );

            app.focus = focus;
            app.handle_key(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL));
            assert!(
                matches!(app.tab.execution, state::Execution::Idle),
                "{focus:?}: {:?}",
                app.tab.execution
            );
            assert_finishes(&in_flight).await;
            assert!(
                matches!(&app.tab.status, TabStatus::Error { message, .. } if message == "cancelled"),
                "{focus:?}: {:?}",
                app.tab.status
            );
        }
    }

    #[tokio::test]
    async fn ctrl_l_stops_live() {
        type Clear = fn(&mut App);
        let cases: [(&str, Clear); 2] = [
            ("Ctrl+L", |app| {
                app.handle_key(key_mod(KeyCode::Char('l'), KeyModifiers::CONTROL));
            }),
            ("palette clear", palette_clear),
        ];
        for (case, clear) in cases {
            let mut app = test_app();
            run_live(&mut app);
            let stream = live_abort_handle(&mut app);
            clear(&mut app);
            assert!(!app.live_mode, "{case}");
            assert!(
                matches!(app.tab.execution, state::Execution::Idle),
                "{case}: {:?}",
                app.tab.execution
            );
            assert_finishes(&stream).await;
        }
    }

    #[tokio::test]
    async fn empty_run_replaces_nothing() {
        let mut app = test_app();
        run_live(&mut app);
        let generation = current_generation(&app);
        app.tab.editor.clear();
        app.handle_key(key(KeyCode::F(5)));
        assert!(app.live_mode);
        assert!(
            matches!(app.tab.execution, state::Execution::Live { generation: g, .. } if g == generation),
            "{:?}",
            app.tab.execution
        );

        let mut app = test_app();
        let mut driver = Driver::attach(&mut app);
        ensure_query(&mut app);
        let mut answer = driver.execute(&mut app);
        let generation = current_generation(&app);
        app.tab.editor.clear();
        app.handle_key(key(KeyCode::F(5)));
        assert_eq!(current_generation(&app), generation);
        assert_unanswered(&mut answer, "after an empty F5");
        assert_admitted(&app, "after an empty F5");

        let mut app = test_app();
        let mut driver = Driver::attach(&mut app);
        ensure_query(&mut app);
        app.handle_key(key(KeyCode::F(5)));
        let generation = current_generation(&app);
        app.tab.editor.clear();
        let mut answer = driver.execute(&mut app);
        assert_answered_err(&mut answer, "empty query", "an empty driver execute");
        assert!(app.tab.execution.is_batch());
        assert_eq!(current_generation(&app), generation);
    }

    #[tokio::test]
    async fn driver_socket_timeout_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.sock");
        let mut app = test_app();
        app.driver_rx = Some(driver::spawn_listener(&path).unwrap());
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
        ensure_query(&mut app);
        let execute = |timeout_ms| {
            let path = path.clone();
            tokio::spawn(async move {
                driver::send_command(&path, &driver::DriverRequest::Execute { timeout_ms })
                    .await
                    .unwrap()
            })
        };
        let bound = Duration::from_secs(10);

        let first = execute(200);
        tokio::time::timeout(bound, async {
            while !first.is_finished() {
                app.poll_driver_commands(&mut terminal);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the first execute times out");
        let response = first.await.unwrap();
        assert!(!response.ok, "{response:?}");
        assert_eq!(
            response.error.as_deref(),
            Some("timeout waiting for response")
        );
        assert!(
            app.tab.execution.is_batch(),
            "the batch outlives its client"
        );
        let detached = current_generation(&app);

        let second = execute(10_000);
        tokio::time::timeout(bound, async {
            while current_generation(&app) == detached && !second.is_finished() {
                app.poll_driver_commands(&mut terminal);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the second execute reaches the app");
        assert!(
            !second.is_finished(),
            "the second execute was admitted, not answered: {:?}",
            second.await
        );
        assert!(app.tab.execution.reply_pending());
        app.handle_key(key(KeyCode::Esc));
        let response = tokio::time::timeout(bound, second)
            .await
            .expect("the cancel answers the second execute")
            .unwrap();
        assert!(!response.ok, "{response:?}");
        assert_eq!(response.error.as_deref(), Some("cancelled"));
    }

    fn saved_entry(id: i64, name: &str) -> SavedQueryResponse {
        SavedQueryResponse {
            id,
            name: name.to_owned(),
            query: format!("service={name}"),
            created_at: String::new(),
            updated_at: String::new(),
            schedule: None,
        }
    }

    fn run_summary(id: i64) -> trawl_client::ReportRunSummary {
        trawl_client::ReportRunSummary {
            id,
            query: "service=web".to_owned(),
            status: "success".to_owned(),
            started_at: "2026-03-14T03:00:00Z".to_owned(),
            finished_at: None,
            duration_ms: Some(5),
            row_count: Some(1),
            error_message: None,
            result_path: None,
            window_start: None,
            window_end: None,
            window_truncated: None,
            window_kind: None,
            origin: None,
        }
    }

    fn saved_xy_app() -> App {
        let mut app = test_app();
        app.saved_cache = Some(ListSavedResponse {
            queries: vec![saved_entry(1, "X"), saved_entry(2, "Y")],
        });
        app.switch_to_main_tab(MainTab::Saved);
        app
    }

    fn pending_token(app: &App) -> state::RequestToken {
        app.panel
            .saved_detail
            .as_ref()
            .and_then(|d| d.pending)
            .expect("a Saved request is pending")
    }

    fn send_runs(app: &mut App, token: state::RequestToken, saved_id: i64, run_ids: &[i64]) {
        app.mutation_tx
            .send(MutationResult::RunsLoaded {
                token,
                saved_id,
                outcome: Ok((
                    run_ids.iter().copied().map(run_summary).collect(),
                    run_ids.len(),
                )),
            })
            .unwrap();
        app.poll_mutations();
    }

    fn loaded_run_ids(app: &App) -> Vec<i64> {
        app.panel
            .saved_detail
            .as_ref()
            .map(|d| d.runs.iter().map(|r| r.id).collect())
            .unwrap_or_default()
    }

    /// These tests never await, so the spawned fetches never run on the
    /// current-thread runtime and only the injected responses arrive.
    #[tokio::test]
    async fn saved_detail_response_order() {
        let mut app = saved_xy_app();
        app.handle_key(key(KeyCode::Enter));
        let x = pending_token(&app);
        app.handle_key(key(KeyCode::Left));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Enter));
        let y = pending_token(&app);
        assert_ne!(x, y);
        send_runs(&mut app, x, 1, &[101]);
        assert_eq!(pending_token(&app), y);
        assert!(loaded_run_ids(&app).is_empty());
        send_runs(&mut app, y, 2, &[201]);
        let detail = app.panel.saved_detail.as_ref().unwrap();
        assert_eq!((detail.saved_id, detail.pending), (2, None));
        assert_eq!(loaded_run_ids(&app), [201]);
        assert_eq!(app.panel.saved_focus, state::SavedFocus::Detail);

        let mut app = saved_xy_app();
        app.handle_key(key(KeyCode::Enter));
        let x = pending_token(&app);
        app.handle_key(key(KeyCode::Left));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Enter));
        let y = pending_token(&app);
        send_runs(&mut app, y, 2, &[201]);
        let before = app.panel.saved_detail.clone();
        send_runs(&mut app, x, 1, &[101]);
        assert_eq!(app.panel.saved_detail, before);
        assert_eq!(loaded_run_ids(&app), [201]);
        assert_eq!(app.panel.saved_focus, state::SavedFocus::Detail);

        let (mut app, x1, y, x3) = saved_xyx_app();
        send_runs(&mut app, y, 2, &[201]);
        send_runs(&mut app, x3, 1, &[103]);
        let before = app.panel.saved_detail.clone();
        send_runs(&mut app, x1, 1, &[101]);
        assert_eq!(app.panel.saved_detail, before);
        assert_eq!(loaded_run_ids(&app), [103]);
        assert_eq!(pending_token_or_none(&app), None);

        let (mut app, x1, y, x3) = saved_xyx_app();
        send_runs(&mut app, y, 2, &[201]);
        app.mutation_tx
            .send(MutationResult::RunsLoaded {
                token: x1,
                saved_id: 1,
                outcome: Err("Failed to fetch runs: boom".to_owned()),
            })
            .unwrap();
        app.poll_mutations();
        assert!(app.popup.is_none(), "a stale runs failure opens no popup");
        let detail = app.panel.saved_detail.as_ref().unwrap();
        assert_eq!((detail.saved_id, detail.pending), (1, Some(x3)));
        assert!(detail.runs.is_empty());
        send_runs(&mut app, x3, 1, &[103]);
        assert_eq!(loaded_run_ids(&app), [103]);
        assert_eq!(pending_token_or_none(&app), None);

        let mut app = saved_xy_app();
        app.handle_key(key(KeyCode::Enter));
        let x = pending_token(&app);
        app.mutation_tx
            .send(MutationResult::RunsLoaded {
                token: x,
                saved_id: 1,
                outcome: Err("Failed to fetch runs: boom".to_owned()),
            })
            .unwrap();
        app.poll_mutations();
        assert!(matches!(app.popup, Some(Popup::Error { .. })));
        assert!(app.panel.saved_detail.is_none());
        assert_eq!(app.panel.saved_focus, state::SavedFocus::List);
    }

    fn saved_xyx_app() -> (
        App,
        state::RequestToken,
        state::RequestToken,
        state::RequestToken,
    ) {
        let mut app = saved_xy_app();
        app.handle_key(key(KeyCode::Enter));
        let x1 = pending_token(&app);
        app.handle_key(key(KeyCode::Left));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Enter));
        let y = pending_token(&app);
        app.handle_key(key(KeyCode::Left));
        app.handle_key(key(KeyCode::Up));
        app.handle_key(key(KeyCode::Enter));
        let x3 = pending_token(&app);
        assert_ne!(x1, x3);
        (app, x1, y, x3)
    }

    fn pending_token_or_none(app: &App) -> Option<state::RequestToken> {
        app.panel.saved_detail.as_ref().and_then(|d| d.pending)
    }

    fn send_result(
        app: &mut App,
        token: state::RequestToken,
        saved_id: i64,
        run_id: i64,
        outcome: Result<trawl_api::value::QueryResult, String>,
    ) {
        app.mutation_tx
            .send(MutationResult::RunResultLoaded {
                token,
                saved_id,
                run_id,
                outcome,
            })
            .unwrap();
        app.poll_mutations();
    }

    fn one_row_result() -> trawl_api::value::QueryResult {
        make_query_response(vec!["count"], vec![vec![Value::Integer(7)]]).result
    }

    #[tokio::test]
    async fn saved_run_result_response_order() {
        let mut app = saved_xy_app();
        app.handle_key(key(KeyCode::Enter));
        let runs = pending_token(&app);
        send_runs(&mut app, runs, 1, &[101, 102]);

        app.handle_key(key(KeyCode::Enter));
        let first = pending_token(&app);
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Enter));
        let second = pending_token(&app);
        assert_ne!(first, second);

        let before = app.panel.saved_detail.clone();
        for stale in [
            Ok(one_row_result()),
            Ok(trawl_api::value::QueryResult::empty()),
            Err("Failed to fetch run result: boom".to_owned()),
        ] {
            send_result(&mut app, first, 1, 101, stale);
            assert_eq!(app.panel.saved_detail, before);
            assert_eq!(app.panel.saved_focus, state::SavedFocus::Detail);
            assert!(app.popup.is_none(), "a stale response opens no popup");
        }

        send_result(
            &mut app,
            second,
            1,
            102,
            Err("Failed to fetch run result: boom".to_owned()),
        );
        assert!(matches!(app.popup, Some(Popup::Error { .. })));
        assert_eq!(pending_token_or_none(&app), None);
        assert_eq!(app.panel.saved_focus, state::SavedFocus::Detail);
        app.popup = None;

        app.handle_key(key(KeyCode::Enter));
        let third = pending_token(&app);
        send_result(&mut app, third, 1, 102, Ok(one_row_result()));
        let detail = app.panel.saved_detail.as_ref().unwrap();
        assert_eq!(detail.pending, None);
        assert_eq!(
            detail.result,
            Some(state::LoadedRun {
                run_id: 102,
                result: one_row_result(),
            })
        );
        assert_eq!(app.panel.saved_focus, state::SavedFocus::RunResults);
    }

    fn refresh_cache(app: &mut App, entries: Vec<SavedQueryResponse>) {
        app.mutation_tx
            .send(MutationResult::CacheRefreshed {
                saved: ListSavedResponse { queries: entries },
                select_name: None,
            })
            .unwrap();
        app.poll_mutations();
    }

    fn mouse_at(kind: event::MouseEventKind, row: u16) -> event::MouseEvent {
        event::MouseEvent {
            kind,
            column: 2,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[tokio::test]
    async fn saved_navigation_invalidates_requests() {
        use event::{MouseButton, MouseEventKind};
        use state::SavedFocus;
        type Action = fn(&mut App, &dyn Fn(u16) -> u16);

        // The list's first row sits one line below the panel's top border.
        let panel = ratatui::layout::Rect::new(0, 3, 80, 20);
        let row_of = |index: u16| panel.y + 1 + index;

        let dropped: [(&str, Action); 7] = [
            ("Left", |app, _| app.handle_key(key(KeyCode::Left))),
            ("Esc", |app, _| app.handle_key(key(KeyCode::Esc))),
            ("click on Y", |app, row_of| {
                app.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), row_of(1)));
            }),
            ("wheel down", |app, row_of| {
                app.handle_mouse(mouse_at(MouseEventKind::ScrollDown, row_of(0)));
            }),
            ("delete of X", |app, _| {
                refresh_cache(app, vec![saved_entry(2, "Y")]);
            }),
            ("empty cache", |app, _| refresh_cache(app, Vec::new())),
            ("refresh that replaces X's id", |app, _| {
                refresh_cache(app, vec![saved_entry(3, "X"), saved_entry(2, "Y")]);
            }),
        ];
        for (name, action) in dropped {
            let mut app = saved_xy_app();
            app.layout.panel = Some(panel);
            app.handle_key(key(KeyCode::Enter));
            let x = pending_token(&app);
            action(&mut app, &row_of);
            assert!(app.panel.saved_detail.is_none(), "{name} drops the detail");
            assert_eq!(app.panel.saved_focus, SavedFocus::List, "{name}");
            let selected = app.panel.saved_selected;
            send_runs(&mut app, x, 1, &[101]);
            assert!(
                app.panel.saved_detail.is_none(),
                "{name}: late runs apply nothing"
            );
            assert_eq!(app.panel.saved_focus, SavedFocus::List, "{name}");
            assert_eq!(app.panel.saved_selected, selected, "{name}");
            assert!(app.popup.is_none(), "{name}");
        }

        let kept: [(&str, Action); 3] = [
            ("tab switch", |app, _| app.handle_key(key(KeyCode::Tab))),
            ("click on X", |app, row_of| {
                app.handle_mouse(mouse_at(MouseEventKind::Down(MouseButton::Left), row_of(0)));
            }),
            ("cache reorder", |app, _| {
                refresh_cache(app, vec![saved_entry(2, "Y"), saved_entry(1, "X")]);
            }),
        ];
        for (name, action) in kept {
            let mut app = saved_xy_app();
            app.layout.panel = Some(panel);
            app.handle_key(key(KeyCode::Enter));
            let x = pending_token(&app);
            action(&mut app, &row_of);
            assert_eq!(
                pending_token_or_none(&app),
                Some(x),
                "{name} keeps the detail"
            );
            let selected = &app.saved_cache.as_ref().unwrap().queries[app.panel.saved_selected];
            assert_eq!(selected.id, 1, "{name} keeps X selected");
            send_runs(&mut app, x, 1, &[101]);
            assert_eq!(
                loaded_run_ids(&app),
                [101],
                "{name}: the matching response applies"
            );
            assert_eq!(app.panel.saved_focus, SavedFocus::Detail, "{name}");
        }

        let mut app = saved_xy_app();
        app.handle_key(key(KeyCode::Enter));
        app.mutation_tx
            .send(MutationResult::CacheRefreshed {
                saved: ListSavedResponse {
                    queries: vec![saved_entry(1, "X"), saved_entry(2, "Y")],
                },
                select_name: Some("Y".to_owned()),
            })
            .unwrap();
        app.poll_mutations();
        assert_eq!(app.panel.saved_selected, 1);
        assert!(app.panel.saved_detail.is_none());
    }

    fn screen(app: &mut App) -> String {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 24)).unwrap();
        terminal.draw(|f| ui::render(app, f)).unwrap();
        terminal.backend().to_string()
    }

    fn saved_view_copy(app: &App) -> App {
        let mut copy = test_app();
        copy.saved_cache.clone_from(&app.saved_cache);
        copy.main_tab = app.main_tab;
        copy.focus = app.focus;
        copy.panel = app.panel.clone();
        copy
    }

    fn q_prefill(app: &App) -> String {
        let mut probe = saved_view_copy(app);
        probe.handle_key(key(KeyCode::Char('q')));
        probe.tab.editor.text()
    }

    #[tokio::test]
    async fn saved_header_and_q_share_identity() {
        use handlers::saved_run_query;

        let mut app = saved_xy_app();
        app.handle_key(key(KeyCode::Enter));
        let x = pending_token(&app);
        send_runs(&mut app, x, 1, &[101, 102]);
        refresh_cache(&mut app, vec![saved_entry(2, "Y"), saved_entry(1, "X")]);
        send_runs(&mut app, x, 1, &[999]);
        let text = screen(&mut app);
        assert!(text.contains("query  service=X"), "{text}");
        assert!(!text.contains("query  service=Y"), "{text}");
        assert_eq!(q_prefill(&app), saved_run_query("X", Some(101)));

        // `select_saved` keeps the list index on X, so point it at Y by hand:
        // the header and `q` must not read the list index at all.
        let mut probe = saved_view_copy(&app);
        probe.panel.saved_selected = 0;
        let text = screen(&mut probe);
        assert!(text.contains("query  service=X"), "{text}");
        assert_eq!(q_prefill(&probe), saved_run_query("X", Some(101)));

        app.handle_key(key(KeyCode::Enter));
        let first = pending_token(&app);
        app.handle_key(key(KeyCode::Down));
        send_result(&mut app, first, 1, 101, Ok(one_row_result()));
        assert_eq!(app.panel.saved_focus, state::SavedFocus::RunResults);
        assert_eq!(app.panel.saved_detail.as_ref().unwrap().run_selected, 1);
        refresh_cache(&mut app, vec![saved_entry(1, "X"), saved_entry(2, "Y")]);
        send_result(&mut app, first, 1, 102, Ok(one_row_result()));
        let text = screen(&mut app);
        assert!(text.contains("Run #101"), "{text}");
        assert!(!text.contains("Run #102"), "{text}");
        assert_eq!(q_prefill(&app), saved_run_query("X", Some(101)));
    }

    #[test]
    fn query_advice_survives_main_tab_round_trip() {
        let mut app = test_app();
        app.tab
            .editor
            .insert_text("service=trawld event_type in (a, b)");
        app.tab.mark_editor_dirty();
        app.tab.last_edit_time = None;
        assert!(app.tab.maybe_validate());

        let draw = |app: &mut App| {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
            terminal
                .draw(|f| crate::tui::ui::render(app, f))
                .expect("render");
            buffer_text(terminal.backend().buffer())
        };
        let before = draw(&mut app);
        assert!(
            before.contains("advice: in the search stage, in (…) is not a list test"),
            "{before}"
        );

        app.switch_to_main_tab(MainTab::Saved);
        let away = draw(&mut app);
        assert!(!away.contains("advice:"), "the panel layout has no editor");

        app.switch_to_main_tab(MainTab::Query);
        assert!(!app.tab.validation_dirty, "no edit happened");
        let after = draw(&mut app);
        assert_eq!(before, after, "the same advice returns with the query tab");
    }

    fn tick_validation_past_debounce(app: &mut App) -> bool {
        if let Some(last) = app.tab.last_edit_time {
            app.tab.last_edit_time = last.checked_sub(Duration::from_millis(400));
        }
        app.tab.maybe_validate()
    }

    fn history_with(query: &str) -> HistoryResponse {
        HistoryResponse {
            entries: vec![trawl_api::HistoryEntryResponse {
                id: 1,
                query: query.to_owned(),
                executed_at: "2026-03-18T14:30:00Z".to_owned(),
                duration_ms: 3,
                row_count: 0,
                status: trawl_client::QueryStatus::Success,
            }],
            total: 1,
        }
    }

    fn load_from_history(app: &mut App, query: &str) {
        app.history_cache = Some(history_with(query));
        app.panel.history_selected = 0;
        app.switch_to_main_tab(MainTab::History);
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.tab.editor.text(), query, "history loads the text");
    }

    fn load_from_palette(app: &mut App, name: &str, query: &str) {
        let mut entry = saved_entry(9, name);
        entry.query = query.to_owned();
        app.saved_cache = Some(ListSavedResponse {
            queries: vec![entry],
        });
        app.handle_key(key_mod(KeyCode::Char('p'), KeyModifiers::CONTROL));
        for ch in name.chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert!(app.popup.is_none(), "the palette closes");
        assert_eq!(app.tab.editor.text(), query, "the palette loads the text");
    }

    #[test]
    fn query_advice_follows_loaded_queries() {
        const ADVISED: &str = r#""10.244.0.83" -service=coredns"#;
        const BROKEN: &str = r#"* | where m matches "^\s+at ""#;
        let advised = |app: &App| {
            let report = app.tab.draft_advice().expect("advice for the loaded text");
            assert_eq!(report.findings.len(), 1);
            assert_eq!(
                report.findings[0].kind,
                trawl_core::advice::AdviceKind::NegatedFieldShape
            );
            assert!(app.tab.validation_errors.is_empty());
        };
        let broken = |app: &App| {
            assert!(app.tab.draft_advice().is_none(), "errors only");
            assert_eq!(app.tab.validation_errors.len(), 1);
            assert_eq!(app.tab.validation_errors[0].span, 22..24);
        };

        let mut app = test_app();
        load_from_history(&mut app, ADVISED);
        assert!(
            tick_validation_past_debounce(&mut app),
            "history load validates"
        );
        advised(&app);
        load_from_history(&mut app, BROKEN);
        assert!(
            tick_validation_past_debounce(&mut app),
            "history load validates"
        );
        broken(&app);

        let mut app = test_app();
        load_from_palette(&mut app, "qadv", ADVISED);
        assert!(
            tick_validation_past_debounce(&mut app),
            "palette load validates"
        );
        advised(&app);
        load_from_palette(&mut app, "qbrk", BROKEN);
        assert!(
            tick_validation_past_debounce(&mut app),
            "palette load validates"
        );
        broken(&app);

        let mut app = test_app();
        load_from_history(&mut app, BROKEN);
        assert!(tick_validation_past_debounce(&mut app));
        app.saved_cache = Some(ListSavedResponse {
            queries: vec![saved_entry(7, "nightly")],
        });
        app.panel.saved_selected = 0;
        app.panel.saved_focus = state::SavedFocus::Detail;
        app.switch_to_main_tab(MainTab::Saved);
        app.handle_key(key(KeyCode::Char('q')));
        assert_eq!(
            app.tab.editor.text(),
            handlers::saved_run_query("nightly", None)
        );
        assert!(
            tick_validation_past_debounce(&mut app),
            "the prefill validates"
        );
        assert!(app.tab.validation_errors.is_empty());
    }
}
