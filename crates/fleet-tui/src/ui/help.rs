//! Help overlay (F1).

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::App;

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
        Line::from("  Ctrl+Q      - quit"),
        Line::from("  Ctrl+T      - new tab"),
        Line::from("  Ctrl+W      - close tab"),
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
        Line::from("  ←/→ (soon)  - scroll columns (wide tables)"),
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

/// Helper to create a centered rect.
fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}
