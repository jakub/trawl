// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Live terminal monitor for trawld.

pub mod state;
pub mod ui;

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::state::AppState;

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

/// The [`RateTracker`](state::RateTracker) inside needs regular ticks to
/// produce meaningful EMA-smoothed rates, so on-demand computation isn't viable.
pub fn spawn_snapshot_collector(
    app_state: AppState,
    listen_addr: String,
    sse_max: usize,
    scheduler_enabled: bool,
    syslog_enabled: bool,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut monitor = state::from_app_state(
            app_state.clone(),
            &listen_addr,
            sse_max,
            scheduler_enabled,
            syslog_enabled,
        );
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;

            if monitor.scheduler_enabled() {
                match app_state.storage.schedule.count_enabled_schedules().await {
                    Ok(count) => {
                        monitor.set_schedule_count(usize::try_from(count).unwrap_or_default());
                    }
                    Err(e) => {
                        tracing::debug!(
                            event_type = "monitor_schedule_count_error",
                            error = %e,
                            "failed to refresh enabled-schedule count"
                        );
                        let fault = crate::error::ServerError::from(e);
                        tracing::warn!(
                            event_type = "monitor_schedule_count_error",
                            error_class = fault.error_class(),
                            cause_kind = fault.cause_kind().as_str(),
                            "failed to refresh enabled-schedule count"
                        );
                    }
                }
            }

            let snapshot = monitor.snapshot();
            *app_state.dashboard_snapshot.lock() = Some(snapshot.to_dashboard_snapshot());
        }
    })
}

pub async fn run(
    app_state: AppState,
    refresh_ms: u64,
    shutdown: crate::shutdown::ShutdownTx,
) -> io::Result<()> {
    let _guard = TerminalGuard::new()?;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let mut interval = tokio::time::interval(Duration::from_millis(refresh_ms));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = interval.tick() => {
                // Raw mode swallows SIGINT, so we must read key events directly.
                while event::poll(Duration::ZERO)? {
                    if let Event::Key(key) = event::read()? {
                        let is_quit = matches!(key.code, KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL))
                            || matches!(key.code, KeyCode::Char('q'));
                        if is_quit {
                            let _ = shutdown.send(true);
                            return Ok(());
                        }
                    }
                }

                let snapshot = app_state.dashboard_snapshot.lock().clone();
                if let Some(ref ds) = snapshot {
                    let opts = trawl_dashboard::DashboardOptions::default();
                    terminal.draw(|f| {
                        trawl_dashboard::render_dashboard(ds, f, f.area(), &opts);
                    })?;
                }
            }
        }
    }
}
