//! Help overlay (F1).

use ratatui::Frame;
use ratatui::layout::Alignment;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::tui::App;

/// Render the help overlay.
pub fn render(_app: &App, frame: &mut Frame<'_>) {
    let area = centered_rect(60, 70, frame.area());

    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Help ")
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black).fg(Color::White));

    let help_text = vec![
        Line::from(""),
        Line::from(Span::styled(
            "Global Keybindings",
            Style::default().fg(Color::Yellow),
        )),
        Line::from(""),
        Line::from("  F1          - toggle help"),
        Line::from("  F2          - toggle schema browser"),
        Line::from("  F3          - toggle query history"),
        Line::from("  F4          - toggle saved queries"),
        Line::from("  F9          - toggle live tail mode"),
        Line::from("  Ctrl+Q      - quit"),
        Line::from("  Ctrl+S      - save current query"),
        Line::from("  Ctrl+T      - new tab"),
        Line::from("  Ctrl+W      - close tab"),
        Line::from("  Shift+Tab   - cycle tabs"),
        Line::from(""),
        Line::from(Span::styled("Editor", Style::default().fg(Color::Yellow))),
        Line::from(""),
        Line::from("  Tab         - switch to results"),
        Line::from("  F5          - execute query"),
        Line::from("  Ctrl+L      - clear editor"),
        Line::from(""),
        Line::from(Span::styled("Results", Style::default().fg(Color::Yellow))),
        Line::from(""),
        Line::from("  Tab         - switch to editor"),
        Line::from("  v           - cycle chart view (table/sparkline/bar/line)"),
        Line::from("  ↑/↓         - scroll rows"),
        Line::from("  ←/→         - scroll columns"),
        Line::from("  PgUp/PgDn   - page up/down"),
        Line::from("  Home/End    - jump to top/bottom"),
        Line::from(""),
        Line::from(Span::styled(
            "Query History",
            Style::default().fg(Color::Yellow),
        )),
        Line::from(""),
        Line::from("  ↑/↓         - navigate history"),
        Line::from("  Enter       - load query into editor"),
        Line::from(""),
        Line::from(Span::styled(
            "Saved Queries",
            Style::default().fg(Color::Yellow),
        )),
        Line::from(""),
        Line::from("  ↑/↓         - navigate saved queries"),
        Line::from("  Enter       - load query into editor"),
        Line::from("  Backspace   - delete saved query"),
        Line::from(""),
        Line::from(Span::styled(
            "Note: Press Esc to close any sidebar",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let paragraph = Paragraph::new(help_text)
        .block(block)
        .alignment(Alignment::Left);

    frame.render_widget(paragraph, area);
}

use super::common::centered_rect;
