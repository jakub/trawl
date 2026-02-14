//! Status bar.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::state::{Focus, TabStatus};

/// Render the status bar.
pub fn render(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let tab = app.active_tab();

    // Left: status
    let status_text = match &tab.status {
        TabStatus::Idle => Span::styled("idle", Style::default().fg(Color::Gray)),
        TabStatus::Running { .. } => Span::styled(
            "running...",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        TabStatus::Success { duration_ms } => Span::styled(
            format!("success ({duration_ms}ms)"),
            Style::default().fg(Color::Green),
        ),
        TabStatus::Error { message } => {
            Span::styled(format!("error: {message}"), Style::default().fg(Color::Red))
        }
    };

    // Middle: focus indicator
    let focus_text = match app.focus {
        Focus::Editor => " [editor] ",
        Focus::Results => " [results] ",
    };

    // Right: keybindings hint
    let hints = " F5: execute | F1: help | ctrl+q: quit ";

    // Calculate padding before moving status_text.
    #[allow(clippy::cast_possible_truncation)] // Terminal width is always < u16::MAX
    let padding_len = area
        .width
        .saturating_sub((status_text.content.len() + focus_text.len() + hints.len()) as u16)
        as usize;

    let line = Line::from(vec![
        status_text,
        Span::raw(focus_text),
        Span::raw(" ".repeat(padding_len)),
        Span::styled(hints, Style::default().fg(Color::DarkGray)),
    ]);

    let paragraph = Paragraph::new(line).style(Style::default().bg(Color::Black));
    frame.render_widget(paragraph, area);
}
