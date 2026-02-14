//! TUI application state machine and event loop.

use color_eyre::eyre::Result;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use fleet_client::{HistoryResponse, HttpClient, QueryResponse, SchemaResponse};
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
    /// Cached history response (fetched on first F3 press).
    pub history_cache: Option<HistoryResponse>,
    /// Whether to quit the application.
    pub should_quit: bool,
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
            should_quit: false,
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
        let tab = self.active_tab_mut();
        let query = tab.editor.lines().join("\n").trim().to_owned();

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
                    tab.scroll_offset = 0; // Reset scroll to top.
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
            // Pass other keys to the textarea widget.
            _ => {
                self.active_tab_mut().editor.input(key);
            }
        }
    }

    /// Handle key events when results are focused.
    fn handle_results_key(&mut self, key: event::KeyEvent) {
        if let (KeyModifiers::NONE, KeyCode::Tab) = (key.modifiers, key.code) {
            self.focus = Focus::Editor;
        }
        // TODO: scroll, filter, export, etc.
    }

    /// Toggle a sidebar (close if already open, open otherwise).
    fn toggle_sidebar(&mut self, sidebar: Sidebar) {
        if self.sidebar == Some(sidebar) {
            self.sidebar = None;
        } else {
            self.sidebar = Some(sidebar);
        }
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
    tracing::info!("fetching schema and history");
    let schema = client.schema().await.ok(); // Ignore errors, optional
    let history = client.history(Some(100), None).await.ok(); // Last 100 queries
    if let Some(ref s) = schema {
        tracing::info!("schema fetched: {} columns", s.columns.len());
    }
    if let Some(ref h) = history {
        tracing::info!("history fetched: {} entries", h.entries.len());
    }

    // Create app with schema and history.
    let mut app = App::new(client);
    app.schema_cache = schema;
    app.history_cache = history;

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
