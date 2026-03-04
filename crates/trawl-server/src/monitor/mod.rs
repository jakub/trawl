//! Live terminal monitor for trawld.
//!
//! Replaces the default stdout log tail when running interactively (TTY
//! detected). Renders a ratatui dashboard that refreshes on a configurable
//! interval, showing executor pool utilization, hot buffer fill, query and
//! ingest throughput, and recent/active queries.
//!
//! Activation is automatic via TTY detection; `--no-monitor` forces log mode.

pub mod state;
pub mod ui;

use std::io;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::Notify;

use crate::state::AppState;

/// RAII guard that restores terminal state on drop (even on panic).
struct TerminalGuard;

impl TerminalGuard {
    fn new() -> io::Result<Self> {
        enable_raw_mode()?;
        crossterm::execute!(io::stdout(), EnterAlternateScreen)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = crossterm::execute!(io::stdout(), LeaveAlternateScreen);
    }
}

/// Run the monitor dashboard until shutdown is signalled.
///
/// Sets up the terminal in raw/alternate-screen mode, ticks at
/// `refresh_ms` intervals, and restores the terminal on exit.
/// Notifies `shutdown` on ctrl-c so the HTTP server can drain.
pub async fn run(
    app_state: AppState,
    listen_addr: &str,
    sse_max: usize,
    scheduler_enabled: bool,
    refresh_ms: u64,
    shutdown: Arc<Notify>,
) -> io::Result<()> {
    let _guard = TerminalGuard::new()?;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let mut monitor = state::from_app_state(app_state, listen_addr, sse_max, scheduler_enabled);

    let mut interval = tokio::time::interval(Duration::from_millis(refresh_ms));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = interval.tick() => {
                // Drain any pending terminal events (ctrl-c, q).
                // Raw mode swallows SIGINT, so we must read key events directly.
                while event::poll(Duration::ZERO)? {
                    if let Event::Key(key) = event::read()? {
                        let is_quit = matches!(key.code, KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL))
                            || matches!(key.code, KeyCode::Char('q'));
                        if is_quit {
                            // Signal the HTTP server to shut down.
                            shutdown.notify_waiters();
                            return Ok(());
                        }
                    }
                }

                let snapshot = monitor.snapshot();
                terminal.draw(|f| ui::render(&snapshot, f))?;
            }
        }
    }
}
