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
use crate::state::{Focus, Sidebar, Tab, TabStatus};
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
    /// Handle to the live streaming task (if active).
    live_task: Option<tokio::task::JoinHandle<()>>,
    /// Channel for receiving query results from background tasks.
    query_rx: mpsc::UnboundedReceiver<QueryResult>,
    /// Sender for spawning queries.
    query_tx: mpsc::UnboundedSender<QueryResult>,
}

impl App {
    /// Create a new app with the given client.
    pub fn new(client: HttpClient) -> Self {
        let (query_tx, query_rx) = mpsc::unbounded_channel();

        Self {
            client,
            tabs: vec![Tab::new(0)],
            active_tab_idx: 0,
            focus: Focus::Editor,
            sidebar: None,
            schema_cache: None,
            history_cache: None,
            saved_cache: None,
            should_quit: false,
            live_mode: false,
            live_task: None,
            query_rx,
            query_tx,
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
            _ => {}
        }

        // If sidebar is open, don't process focus-specific keys.
        if self.sidebar.is_some() {
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
                if let Some(result) = &tab.result {
                    let max_scroll = result.result.row_count().saturating_sub(1);
                    tab.scroll_offset = (tab.scroll_offset + 1).min(max_scroll);
                }
            }
            (KeyModifiers::NONE, KeyCode::PageUp) => {
                let tab = self.active_tab_mut();
                tab.scroll_offset = tab.scroll_offset.saturating_sub(10);
            }
            (KeyModifiers::NONE, KeyCode::PageDown) => {
                let tab = self.active_tab_mut();
                if let Some(result) = &tab.result {
                    let max_scroll = result.result.row_count().saturating_sub(1);
                    tab.scroll_offset = (tab.scroll_offset + 10).min(max_scroll);
                }
            }
            (KeyModifiers::NONE, KeyCode::Home) => {
                self.active_tab_mut().scroll_offset = 0;
            }
            (KeyModifiers::NONE, KeyCode::End) => {
                let tab = self.active_tab_mut();
                if let Some(result) = &tab.result {
                    tab.scroll_offset = result.result.row_count().saturating_sub(1);
                }
            }
            // Horizontal scrolling
            (KeyModifiers::NONE, KeyCode::Left) => {
                let tab = self.active_tab_mut();
                tab.horizontal_scroll_offset = tab.horizontal_scroll_offset.saturating_sub(1);
            }
            (KeyModifiers::NONE, KeyCode::Right) => {
                let tab = self.active_tab_mut();
                if let Some(result) = &tab.result {
                    let max_scroll = result.result.columns.len().saturating_sub(1);
                    tab.horizontal_scroll_offset =
                        (tab.horizontal_scroll_offset + 1).min(max_scroll);
                }
            }
            _ => {}
        }
    }

    /// Toggle a sidebar (close if already open, open otherwise).
    fn toggle_sidebar(&mut self, sidebar: Sidebar) {
        if self.sidebar == Some(sidebar) {
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

    // Fetch schema before starting (blocks, but only ~100ms).
    tracing::info!("fetching schema, history, and saved queries");
    let schema = client.schema().await.ok(); // Ignore errors, optional
    let history = client.history(Some(100), None).await.ok(); // Last 100 queries
    let saved = client.list_saved().await.ok(); // Saved queries
    if let Some(ref s) = schema {
        tracing::info!("schema fetched: {} columns", s.columns.len());
    }
    if let Some(ref h) = history {
        tracing::info!("history fetched: {} entries", h.entries.len());
    }
    if let Some(ref sq) = saved {
        tracing::info!("saved queries fetched: {} entries", sq.queries.len());
    }

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
