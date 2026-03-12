//! Tab bar rendering.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::tui::App;
use crate::tui::state::{MainTab, TabStatus};

/// Render the tab bar at the top of the screen.
pub fn render(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let width = area.width as usize;
    let mut spans = Vec::new();

    let tabs = [
        (MainTab::Query, "Query", "M-1"),
        (MainTab::History, "History", "M-2"),
        (MainTab::Schema, "Schema", "M-3"),
        (MainTab::Saved, "Saved", "M-4"),
    ];

    for (idx, (tab, label, shortcut)) in tabs.iter().enumerate() {
        let is_active = *tab == app.main_tab;

        // For the Query tab, show the status indicator.
        let label_text = if *tab == MainTab::Query {
            let (indicator, _indicator_color) = match &app.tab.status {
                TabStatus::Idle => ("", Color::DarkGray),
                TabStatus::Running { .. } => (" ◉", Color::Yellow),
                TabStatus::Success { .. } => (" ✓", Color::Green),
                TabStatus::Error { .. } => (" ✗", Color::Red),
            };
            if width >= 75 {
                format!(" {shortcut} {label}{indicator} ")
            } else {
                format!(" {label}{indicator} ")
            }
        } else if width >= 75 {
            format!(" {shortcut} {label} ")
        } else {
            format!(" {label} ")
        };

        let style = if is_active {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM)
        };

        spans.push(Span::styled(label_text, style));

        // Separator between tabs
        if idx < tabs.len() - 1 {
            spans.push(Span::raw(" "));
        }
    }

    // Add keybinding hint on the right
    let hint = "F1 help";

    let left_len: usize = spans.iter().map(|s| s.content.len()).sum();
    let padding = width.saturating_sub(left_len + hint.len());
    spans.push(Span::raw(" ".repeat(padding)));
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
