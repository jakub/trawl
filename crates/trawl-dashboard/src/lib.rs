// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared ratatui dashboard rendering for trawl.
//!
//! Pure function — takes a [`DashboardSnapshot`] and draws widgets to a
//! ratatui [`Frame`]. No I/O, no state mutation, fully testable via
//! `TestBackend` + insta snapshots.
//!
//! Used by both the server's terminal monitor and the TUI client's admin
//! dashboard tab; the web UI reads the same [`DashboardSnapshot`] over SSE.

use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Rect};
use ratatui::macros::{horizontal, line, vertical};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, Gauge, Paragraph, Row, Table};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use trawl_api::DashboardSnapshot;

/// Rendering options for the dashboard.
#[derive(Debug, Clone)]
pub struct DashboardOptions {
    /// Footer text to display. `None` omits the footer row entirely.
    pub footer_text: Option<String>,
}

impl Default for DashboardOptions {
    fn default() -> Self {
        Self {
            footer_text: Some(" q or ctrl-c to quit".to_owned()),
        }
    }
}

/// Standard panel block with dark gray border.
fn panel_block(title: &str) -> Block<'_> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
}

/// Highlighted panel block (for active queries).
fn panel_block_highlight(title: &str) -> Block<'_> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
}

/// Render the dashboard into the given area.
///
/// The caller controls placement: the server passes `frame.area()` for
/// full-screen rendering, while the TUI passes a sub-region below the
/// tab bar and above the status bar.
pub fn render_dashboard(
    snapshot: &DashboardSnapshot,
    frame: &mut Frame<'_>,
    area: Rect,
    options: &DashboardOptions,
) {
    let footer_height = u16::from(options.footer_text.is_some());

    let [header, body, footer] = vertical![==2, >=10, ==footer_height].areas(area);

    render_header(snapshot, frame, header);
    render_body(snapshot, frame, body);
    if let Some(ref text) = options.footer_text {
        render_footer(text, frame, footer);
    }
}

/// Header: title bar + hostname/addr/uptime/health.
fn render_header(snapshot: &DashboardSnapshot, frame: &mut Frame<'_>, area: Rect) {
    let [title_row, info_row] = vertical![==1, ==1].areas(area);

    let title = format!(" trawld v{} ", snapshot.version);
    let title_line = line![Span::styled(
        title,
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )];
    frame.render_widget(Paragraph::new(title_line), title_row);

    let health_style = if snapshot.healthy {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::Red)
    };
    let health_text = if snapshot.healthy { "OK" } else { "DEGRADED" };

    let info = line![
        Span::raw(" host: "),
        Span::styled(&snapshot.hostname, Style::default().fg(Color::White)),
        Span::raw("  addr: "),
        Span::styled(&snapshot.listen_addr, Style::default().fg(Color::White)),
        Span::raw("  uptime: "),
        Span::styled(
            format_duration(Duration::from_secs(snapshot.uptime_secs)),
            Style::default().fg(Color::White),
        ),
        Span::raw("  health: "),
        Span::styled(health_text, health_style),
    ];
    frame.render_widget(Paragraph::new(info), info_row);
}

/// Body: two-column grid with panels.
///
/// `Flex::Start` is spelled out so the panels stay top-aligned however tall
/// the terminal is, instead of spreading into the spare rows.
fn render_body(snapshot: &DashboardSnapshot, frame: &mut Frame<'_>, area: Rect) {
    let [row1_area, row2_area, row3_area, row4_area, row5_area] =
        vertical![==5, ==6, ==5, ==3, >=4]
            .flex(Flex::Start)
            .areas(area);

    // -- row 1: executor pool | hot buffer --
    let [exec_area, hot_area] = horizontal![==2/5, ==3/5].areas(row1_area);
    render_executor_pool(snapshot, frame, exec_area);
    render_hot_buffer(snapshot, frame, hot_area);

    // -- row 2: HTTP ingest | syslog ingest --
    if snapshot.syslog_enabled {
        let [http_area, syslog_area] = horizontal![==2/5, ==3/5].areas(row2_area);
        render_http_ingest(snapshot, frame, http_area);
        render_syslog_ingest(snapshot, frame, syslog_area);
    } else {
        render_http_ingest(snapshot, frame, row2_area);
    }

    // -- row 3: query throughput | data pipeline --
    let [qtp_area, pipeline_area] = horizontal![==2/5, ==3/5].areas(row3_area);
    render_query_throughput(snapshot, frame, qtp_area);
    render_data_pipeline(snapshot, frame, pipeline_area);

    // -- row 4: SSE | scheduler --
    let [sse_area, sched_area] = horizontal![==2/5, ==3/5].areas(row4_area);
    render_sse(snapshot, frame, sse_area);
    render_scheduler(snapshot, frame, sched_area);

    // -- row 5: recent + active queries --
    render_queries(snapshot, frame, row5_area);
}

/// Executor pool panel with gauge.
#[allow(clippy::cast_precision_loss)] // gauge ratio — pool capacity is small
fn render_executor_pool(snapshot: &DashboardSnapshot, frame: &mut Frame<'_>, area: Rect) {
    let block = panel_block(" EXECUTOR POOL ");

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height < 2 {
        return;
    }

    let [info_row, gauge_row] = vertical![==1, ==1].areas(inner);

    let active = snapshot.pool_active.min(snapshot.pool_capacity);
    // Retained permits are a subset of the active ones, so they are a
    // suffix on the same figure rather than a second count (ADR-0024).
    // Nothing retained is the ordinary state, and a permanent "(0
    // retained)" would teach an operator to stop reading the line.
    let retained = if snapshot.pool_retained == 0 {
        String::new()
    } else {
        format!(" ({} retained)", snapshot.pool_retained)
    };
    // The suffix can outgrow the narrow layout's panel, where a raw clip
    // would read as a broken word ("(2 retain"). Truncating says so.
    let line = format!("active: {}/{}{retained}", active, snapshot.pool_capacity);
    let info = format!(
        " {}",
        truncate_query(&line, usize::from(info_row.width).saturating_sub(1))
    );
    frame.render_widget(Paragraph::new(info), info_row);

    let ratio = if snapshot.pool_capacity > 0 {
        active as f64 / snapshot.pool_capacity as f64
    } else {
        0.0
    };
    let gauge = Gauge::default()
        .ratio(ratio.min(1.0))
        .gauge_style(Style::default().fg(Color::Cyan));
    frame.render_widget(gauge, gauge_row);
}

/// Hot buffer fill gauges.
#[allow(clippy::cast_precision_loss)] // percentage display — precision is irrelevant
fn render_hot_buffer(snapshot: &DashboardSnapshot, frame: &mut Frame<'_>, area: Rect) {
    let block = panel_block(" HOT BUFFER ");

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height < 3 {
        return;
    }

    let [event_row, byte_row, batch_row] = vertical![==1, ==1, ==1].areas(inner);

    let event_pct = if snapshot.hot_buffer_max_events > 0 {
        (snapshot.hot_buffer_events as f64 / snapshot.hot_buffer_max_events as f64) * 100.0
    } else {
        0.0
    };
    let event_line = format!(
        " events: {} / {} ({:.1}%)",
        format_number(snapshot.hot_buffer_events as u64),
        format_number(snapshot.hot_buffer_max_events as u64),
        event_pct,
    );
    frame.render_widget(Paragraph::new(event_line), event_row);

    let byte_pct = if snapshot.hot_buffer_max_bytes > 0 {
        (snapshot.hot_buffer_bytes as f64 / snapshot.hot_buffer_max_bytes as f64) * 100.0
    } else {
        0.0
    };
    let byte_line = format!(
        " bytes:  {} / {} ({:.1}%)",
        format_bytes(snapshot.hot_buffer_bytes as u64),
        format_bytes(snapshot.hot_buffer_max_bytes as u64),
        byte_pct,
    );
    frame.render_widget(Paragraph::new(byte_line), byte_row);

    let batch_line = format!(" batches: {}", snapshot.hot_buffer_batches);
    frame.render_widget(Paragraph::new(batch_line), batch_row);
}

/// Query throughput panel.
fn render_query_throughput(snapshot: &DashboardSnapshot, frame: &mut Frame<'_>, area: Rect) {
    let block = panel_block(" QUERY THROUGHPUT ");

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height < 2 {
        return;
    }

    let [total_row, err_row] = vertical![==1, ==1].areas(inner);

    let total_line = format!(
        " total: {}  {:.1} q/s",
        format_number(snapshot.total_queries),
        snapshot.query_rate,
    );
    frame.render_widget(Paragraph::new(total_line), total_row);

    let err_line = format!(
        " errors: {}  timeouts: {}",
        snapshot.query_errors, snapshot.query_timeouts,
    );
    frame.render_widget(Paragraph::new(err_line), err_row);
}

/// HTTP ingest throughput panel.
fn render_http_ingest(snapshot: &DashboardSnapshot, frame: &mut Frame<'_>, area: Rect) {
    let block = panel_block(" HTTP INGEST ");

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height < 2 {
        return;
    }

    let [events_row, rejected_row] = vertical![==1, ==1].areas(inner);

    let events_line = format!(
        " events: {}  ~{:.0} ev/s",
        format_number(snapshot.ingest_events),
        snapshot.ingest_rate,
    );
    frame.render_widget(Paragraph::new(events_line), events_row);

    let rejected_line = format!(" rejected: {}", snapshot.ingest_rejected);
    frame.render_widget(Paragraph::new(rejected_line), rejected_row);
}

/// Syslog ingest throughput panel.
fn render_syslog_ingest(snapshot: &DashboardSnapshot, frame: &mut Frame<'_>, area: Rect) {
    let block = panel_block(" SYSLOG INGEST ");

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height < 3 {
        return;
    }

    let [totals_row, breakdown_row, errors_row] = vertical![==1, ==1, ==1].areas(inner);

    let total = snapshot.syslog_events_udp + snapshot.syslog_events_tcp;
    let totals_line = format!(
        " events: {}  ~{:.0} ev/s",
        format_number(total),
        snapshot.syslog_rate,
    );
    frame.render_widget(Paragraph::new(totals_line), totals_row);

    let breakdown_line = format!(
        " udp: {}  tcp: {}",
        format_number(snapshot.syslog_events_udp),
        format_number(snapshot.syslog_events_tcp),
    );
    frame.render_widget(Paragraph::new(breakdown_line), breakdown_row);

    let errors_line = format!(
        " errors: {}  dropped: {}  tcp conns: {}",
        snapshot.syslog_parse_errors, snapshot.syslog_dropped, snapshot.syslog_tcp_connections,
    );
    frame.render_widget(Paragraph::new(errors_line), errors_row);
}

/// WAL, compaction, and parquet storage panel.
fn render_data_pipeline(snapshot: &DashboardSnapshot, frame: &mut Frame<'_>, area: Rect) {
    let block = panel_block(" DATA PIPELINE ");

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height < 2 {
        return;
    }

    let [wal_row, parquet_row] = vertical![==1, ==1].areas(inner);

    let compacted_ago = match snapshot.last_compaction_secs {
        Some(secs) => format!(
            "compacted: {} ago",
            format_duration(std::time::Duration::from_secs(secs))
        ),
        None => "compacted: never".to_owned(),
    };
    let wal_line = format!(
        " wal: {} files ({})  {}",
        snapshot.wal_files,
        format_bytes(snapshot.wal_bytes),
        compacted_ago,
    );
    frame.render_widget(Paragraph::new(wal_line), wal_row);

    let errors_suffix = if snapshot.compaction_errors > 0 {
        format!("  errors: {}", snapshot.compaction_errors)
    } else {
        String::new()
    };
    let parquet_line = format!(
        " parquet: {} files ({})  runs: {}{}",
        format_number(snapshot.parquet_files),
        format_bytes(snapshot.parquet_bytes),
        format_number(snapshot.compaction_runs),
        errors_suffix,
    );
    frame.render_widget(Paragraph::new(parquet_line), parquet_row);
}

/// SSE connections panel.
fn render_sse(snapshot: &DashboardSnapshot, frame: &mut Frame<'_>, area: Rect) {
    let block = panel_block(" SSE STREAMS ");

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height < 1 {
        return;
    }

    let line = format!(" active: {}/{}", snapshot.sse_active, snapshot.sse_max);
    frame.render_widget(Paragraph::new(line), inner);
}

/// Scheduler status panel.
fn render_scheduler(snapshot: &DashboardSnapshot, frame: &mut Frame<'_>, area: Rect) {
    let block = panel_block(" SCHEDULER ");

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height < 1 {
        return;
    }

    let line = if snapshot.scheduler_enabled {
        format!(" enabled: {} schedules", snapshot.scheduler_schedules)
    } else {
        " disabled".to_owned()
    };
    frame.render_widget(Paragraph::new(line), inner);
}

/// Recent and active queries panels.
fn render_queries(snapshot: &DashboardSnapshot, frame: &mut Frame<'_>, area: Rect) {
    let has_active = !snapshot.active_queries.is_empty();
    // max 4 rows + 2 for border, capped at u16::MAX (can't overflow in practice)
    let active_height: u16 = if has_active {
        u16::try_from(snapshot.active_queries.len().min(4) + 2).unwrap_or(u16::MAX)
    } else {
        0
    };

    let active_len = if has_active { active_height } else { 0 };
    let [recent_area, active_area] = vertical![>=4, ==active_len].areas(area);

    render_recent_queries(snapshot, frame, recent_area);
    if has_active {
        render_active_queries(snapshot, frame, active_area);
    }
}

/// Recent completed queries table.
fn render_recent_queries(snapshot: &DashboardSnapshot, frame: &mut Frame<'_>, area: Rect) {
    let block = panel_block(" RECENT QUERIES ");

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || snapshot.recent_queries.is_empty() {
        if inner.height > 0 {
            frame.render_widget(Paragraph::new(" no queries yet"), inner);
        }
        return;
    }

    let max_rows = inner.height as usize;
    let queries: Vec<&_> = snapshot.recent_queries.iter().take(max_rows).collect();
    let max_query_width = inner.width.saturating_sub(30) as usize;

    let rows: Vec<Row<'_>> = queries
        .iter()
        .map(|q| {
            let status_style = if q.error.is_some() || q.timed_out {
                Style::default().fg(Color::Red)
            } else {
                Style::default().fg(Color::Green)
            };
            let status = if q.timed_out {
                "TMO".to_owned()
            } else if q.error.is_some() {
                "ERR".to_owned()
            } else {
                "OK".to_owned()
            };
            let rows_str = q
                .rows
                .map_or_else(|| "-".to_owned(), |r| format_number(r as u64));
            let query_text = truncate_query(&q.query, max_query_width);

            Row::new(vec![
                format!("#{}", q.id),
                format!("{}ms", q.duration_ms),
                format!("{rows_str} rows"),
                status,
                query_text,
            ])
            .style(status_style)
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(7),
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Length(4),
            Constraint::Min(10),
        ],
    );

    frame.render_widget(table, inner);
}

/// Active queries table (only shown when non-empty).
fn render_active_queries(snapshot: &DashboardSnapshot, frame: &mut Frame<'_>, area: Rect) {
    let block = panel_block_highlight(" ACTIVE QUERIES ");

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 {
        return;
    }

    let max_rows = inner.height as usize;
    let max_query_width = inner.width.saturating_sub(30) as usize;

    let rows: Vec<Row<'_>> = snapshot
        .active_queries
        .iter()
        .take(max_rows)
        .map(|q| {
            let elapsed = format_duration_short(q.running_ms);
            let query_text = truncate_query(&q.query, max_query_width);
            Row::new(vec![
                format!("#{}", q.id),
                q.user.clone(),
                format!("running {elapsed}"),
                query_text,
            ])
            .style(Style::default().fg(Color::Yellow))
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(7),
            Constraint::Length(10),
            Constraint::Length(14),
            Constraint::Min(10),
        ],
    );

    frame.render_widget(table, inner);
}

/// Footer: help text.
fn render_footer(text: &str, frame: &mut Frame<'_>, area: Rect) {
    let footer = line![Span::styled(text, Style::default().fg(Color::DarkGray),)];
    frame.render_widget(Paragraph::new(footer), area);
}

// -- formatting helpers (public for reuse) ------------------------------------

/// Format a `Duration` as human-readable (e.g. "2h 34m", "5m 12s").
pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3600 {
        let hours = secs / 3600;
        let mins = (secs % 3600) / 60;
        format!("{hours}h {mins:02}m")
    } else if secs >= 60 {
        let mins = secs / 60;
        let s = secs % 60;
        format!("{mins}m {s:02}s")
    } else {
        format!("{secs}s")
    }
}

/// Format milliseconds as short duration (e.g. "1.2s", "340ms").
#[allow(clippy::cast_precision_loss)] // display only
pub fn format_duration_short(ms: u64) -> String {
    if ms >= 1000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{ms}ms")
    }
}

/// Format a number with thousands separators.
pub fn format_number(n: u64) -> String {
    if n < 1000 {
        return n.to_string();
    }
    let s = n.to_string();
    let mut result = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(ch);
    }
    result.chars().rev().collect()
}

/// Format bytes as human-readable (e.g. "4.2 MB", "128 KB").
#[allow(clippy::cast_precision_loss)] // display only — sub-byte precision is meaningless
pub fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * 1024 * 1024;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Truncate a query string to fit within `max_width` display columns,
/// appending "..." if needed. Uses unicode display width, not byte length.
pub fn truncate_query(query: &str, max_width: usize) -> String {
    let collapsed: String = query.split_whitespace().collect::<Vec<_>>().join(" ");
    if UnicodeWidthStr::width(collapsed.as_str()) <= max_width {
        return collapsed;
    }
    let suffix = if max_width > 3 { "..." } else { "" };
    let target = max_width.saturating_sub(suffix.len());
    let mut result = String::new();
    let mut width = 0;
    for ch in collapsed.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width > target {
            break;
        }
        result.push(ch);
        width += ch_width;
    }
    result.push_str(suffix);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use trawl_api::{ActiveQuerySnapshot, CompletedQuerySnapshot, DashboardSnapshot};

    fn test_snapshot() -> DashboardSnapshot {
        DashboardSnapshot {
            hostname: "test-host".into(),
            listen_addr: "127.0.0.1:5514".into(),
            uptime_secs: 9240, // 2h 34m
            version: "0.1.0".into(),
            healthy: true,
            pool_capacity: 4,
            pool_active: 1,
            pool_retained: 0,
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
            syslog_enabled: true,
            syslog_events_udp: 1_247_829,
            syslog_events_tcp: 89_341,
            syslog_rate: 530.0,
            syslog_parse_errors: 12,
            syslog_dropped: 3,
            syslog_tcp_connections: 2,
            wal_files: 12,
            wal_bytes: 4_404_019, // ~4.2 MB
            wal_measurement: trawl_api::StorageMeasurement {
                status: trawl_api::StorageMeasurementStatus::Complete,
                sample_age_secs: Some(2),
            },
            last_compaction_secs: Some(3),
            compaction_runs: 1247,
            compaction_errors: 0,
            parquet_files: 847,
            parquet_bytes: 13_312_000_000, // ~12.4 GB
            parquet_measurement: trawl_api::StorageMeasurement {
                status: trawl_api::StorageMeasurementStatus::Complete,
                sample_age_secs: Some(2),
            },
            sse_active: 2,
            sse_max: 32,
            scheduler_enabled: true,
            scheduler_schedules: 3,
            recent_queries: vec![
                CompletedQuerySnapshot {
                    id: 1247,
                    user: "admin".into(),
                    query: "_severity=error last=1h | stats count() by service".into(),
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
    fn render_dashboard_full() {
        let snapshot = test_snapshot();
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                render_dashboard(&snapshot, f, area, &DashboardOptions::default());
            })
            .unwrap();
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
        terminal
            .draw(|f| {
                let area = f.area();
                render_dashboard(&snapshot, f, area, &DashboardOptions::default());
            })
            .unwrap();
        insta::assert_snapshot!(terminal.backend().to_string());
    }

    #[test]
    fn render_dashboard_narrow() {
        let snapshot = test_snapshot();
        let backend = TestBackend::new(60, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                render_dashboard(&snapshot, f, area, &DashboardOptions::default());
            })
            .unwrap();
        insta::assert_snapshot!(terminal.backend().to_string());
    }

    /// Nothing retained is the ordinary state: the pool line carries no
    /// suffix at all (ADR-0024).
    #[test]
    fn render_dashboard_retained_zero() {
        let snapshot = test_snapshot();
        assert_eq!(snapshot.pool_retained, 0);
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                render_dashboard(&snapshot, f, area, &DashboardOptions::default());
            })
            .unwrap();
        let rendered = terminal.backend().to_string();
        assert!(!rendered.contains("retained"));
        insta::assert_snapshot!(rendered);
    }

    /// A retained permit is a suffix on the active figure, not a second
    /// count: three permits are held, one of them by work whose request
    /// already answered.
    #[test]
    fn render_dashboard_retained_nonzero() {
        let mut snapshot = test_snapshot();
        snapshot.pool_active = 3;
        snapshot.pool_retained = 1;
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                render_dashboard(&snapshot, f, area, &DashboardOptions::default());
            })
            .unwrap();
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("active: 3/4 (1 retained)"));
        insta::assert_snapshot!(rendered);
    }

    /// The suffix still has to fit the narrow layout.
    #[test]
    fn render_dashboard_retained_narrow() {
        let mut snapshot = test_snapshot();
        snapshot.pool_active = 4;
        snapshot.pool_retained = 2;
        let backend = TestBackend::new(60, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                render_dashboard(&snapshot, f, area, &DashboardOptions::default());
            })
            .unwrap();
        insta::assert_snapshot!(terminal.backend().to_string());
    }

    #[test]
    fn render_dashboard_no_footer() {
        let snapshot = test_snapshot();
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let opts = DashboardOptions { footer_text: None };
        terminal
            .draw(|f| {
                let area = f.area();
                render_dashboard(&snapshot, f, area, &opts);
            })
            .unwrap();
        insta::assert_snapshot!(terminal.backend().to_string());
    }

    #[test]
    fn format_number_thousands() {
        assert_eq!(format_number(0), "0");
        assert_eq!(format_number(999), "999");
        assert_eq!(format_number(1000), "1,000");
        assert_eq!(format_number(12_847), "12,847");
        assert_eq!(format_number(847_293), "847,293");
        assert_eq!(format_number(1_000_000), "1,000,000");
    }

    #[test]
    fn format_bytes_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(4_404_019), "4.2 MB");
        assert_eq!(format_bytes(104_857_600), "100.0 MB");
        assert_eq!(format_bytes(1_073_741_824), "1.0 GB");
    }

    #[test]
    fn format_duration_display() {
        assert_eq!(format_duration(Duration::from_secs(45)), "45s");
        assert_eq!(format_duration(Duration::from_secs(125)), "2m 05s");
        assert_eq!(format_duration(Duration::from_mins(154)), "2h 34m");
    }

    #[test]
    fn truncate_query_ascii() {
        assert_eq!(truncate_query("short", 10), "short");
        assert_eq!(
            truncate_query("a really long query string", 10),
            "a reall..."
        );
        assert_eq!(truncate_query("abc", 3), "abc");
        assert_eq!(truncate_query("abcd", 3), "abc");
    }

    #[test]
    fn truncate_query_multibyte() {
        // CJK characters are 2 display columns wide.
        assert_eq!(truncate_query("日本語テスト", 12), "日本語テスト");
        assert_eq!(truncate_query("日本語テスト", 10), "日本語...");
    }

    #[test]
    fn truncate_query_collapses_whitespace() {
        assert_eq!(truncate_query("a  b   c", 20), "a b c");
    }

    #[test]
    fn truncate_query_zero_width() {
        assert_eq!(truncate_query("abc", 0), "");
    }
}
