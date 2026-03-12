//! Monitor dashboard rendering.
//!
//! Thin wrapper over [`trawl_dashboard`] — converts the server-local
//! [`MonitorSnapshot`] to the shared [`DashboardSnapshot`] wire type
//! and delegates rendering.

use ratatui::Frame;

use trawl_dashboard::DashboardOptions;

use super::state::MonitorSnapshot;

/// Render the monitor dashboard to the given frame.
pub fn render(snapshot: &MonitorSnapshot, frame: &mut Frame<'_>) {
    let ds = snapshot.to_dashboard_snapshot();
    let opts = DashboardOptions::default();
    trawl_dashboard::render_dashboard(&ds, frame, frame.area(), &opts);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::state::MonitorSnapshot;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use trawl_api::{ActiveQuerySnapshot, CompletedQuerySnapshot};

    fn test_snapshot() -> MonitorSnapshot {
        MonitorSnapshot {
            hostname: "test-host".into(),
            listen_addr: "127.0.0.1:5514".into(),
            uptime: std::time::Duration::from_secs(9240), // 2h 34m
            version: "0.1.0",
            healthy: true,
            pool_capacity: 4,
            pool_active: 1,
            hot_buffer_events: 12_847,
            hot_buffer_max_events: 100_000,
            hot_buffer_bytes: 4_404_019,       // ~4.2 MB
            hot_buffer_max_bytes: 104_857_600, // 100 MB
            hot_buffer_batches: 23,
            total_queries: 1247,
            query_rate: 2.1,
            query_errors: 12,
            query_timeouts: 3,
            ingest_events: 847_293,
            ingest_rate: 340.0,
            ingest_rejected: 47,
            sse_active: 2,
            sse_max: 32,
            scheduler_enabled: true,
            scheduler_schedules: 3,
            recent_queries: vec![
                CompletedQuerySnapshot {
                    id: 1247,
                    user: "admin".into(),
                    query: "level=error last=1h | stats count() by service".into(),
                    duration_ms: 23,
                    rows: Some(142),
                    error: None,
                    timed_out: false,
                },
                CompletedQuerySnapshot {
                    id: 1246,
                    user: "admin".into(),
                    query: "* | stats count()".into(),
                    duration_ms: 87,
                    rows: Some(1),
                    error: None,
                    timed_out: false,
                },
                CompletedQuerySnapshot {
                    id: 1245,
                    user: "admin".into(),
                    query: "invalid query syntax".into(),
                    duration_ms: 12,
                    rows: None,
                    error: Some("parse error".into()),
                    timed_out: false,
                },
            ],
            active_queries: vec![ActiveQuerySnapshot {
                id: 1248,
                user: "admin".into(),
                role: "admin".into(),
                query: "service=nginx | timechart span=5m count()".into(),
                running_ms: 1200,
            }],
        }
    }

    #[test]
    fn render_dashboard() {
        let snapshot = test_snapshot();
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(&snapshot, f)).unwrap();
        insta::assert_snapshot!(terminal.backend().to_string());
    }

    #[test]
    fn render_dashboard_empty() {
        let mut snapshot = test_snapshot();
        snapshot.total_queries = 0;
        snapshot.query_rate = 0.0;
        snapshot.query_errors = 0;
        snapshot.query_timeouts = 0;
        snapshot.ingest_events = 0;
        snapshot.ingest_rate = 0.0;
        snapshot.ingest_rejected = 0;
        snapshot.hot_buffer_events = 0;
        snapshot.hot_buffer_bytes = 0;
        snapshot.hot_buffer_batches = 0;
        snapshot.recent_queries = vec![];
        snapshot.active_queries = vec![];
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(&snapshot, f)).unwrap();
        insta::assert_snapshot!(terminal.backend().to_string());
    }

    #[test]
    fn render_dashboard_narrow() {
        let snapshot = test_snapshot();
        let backend = TestBackend::new(60, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(&snapshot, f)).unwrap();
        insta::assert_snapshot!(terminal.backend().to_string());
    }
}
