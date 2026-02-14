//! Tab bar rendering.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::state::TabStatus;

/// Render the tab bar at the top of the screen.
pub fn render(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let mut spans = Vec::new();

    for (idx, tab) in app.tabs.iter().enumerate() {
        let is_active = idx == app.active_tab_idx;

        // Status indicator
        let (indicator, indicator_color) = match &tab.status {
            TabStatus::Idle => ("●", Color::DarkGray),
            TabStatus::Running { .. } => ("◉", Color::Yellow),
            TabStatus::Success { .. } => ("✓", Color::Green),
            TabStatus::Error { .. } => ("✗", Color::Red),
        };

        // Tab label
        let label = format!(" {} {} ", idx + 1, indicator);

        let style = if is_active {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(indicator_color)
                .add_modifier(Modifier::DIM)
        };

        spans.push(Span::styled(label, style));

        // Separator between tabs
        if idx < app.tabs.len() - 1 {
            spans.push(Span::raw(" "));
        }
    }

    // Add keybinding hints on the right
    let hint = format!(
        "{}  ctrl+t new  ctrl+w close",
        " ".repeat(area.width.saturating_sub(30) as usize)
    );
    spans.push(Span::styled(
        hint,
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM),
    ));

    let line = Line::from(spans);
    let paragraph = Paragraph::new(line).style(Style::default().bg(Color::Black));

    frame.render_widget(paragraph, area);
}
