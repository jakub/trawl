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

    let mut tabs: Vec<(MainTab, &str, &str)> = vec![
        (MainTab::Query, "Query", "M-1"),
        (MainTab::History, "History", "M-2"),
        (MainTab::Schema, "Schema", "M-3"),
        (MainTab::Saved, "Saved", "M-4"),
    ];
    if app.dashboard.is_admin {
        tabs.push((MainTab::Dashboard, "Dashboard", "M-5"));
    }

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

    let line = Line::from(spans);
    let paragraph = Paragraph::new(line).style(Style::default().bg(Color::Black));

    // Render only on the first row; the rest of the area is a spacer that
    // inherits the terminal's default background.
    let tab_row = Rect { height: 1, ..area };
    frame.render_widget(paragraph, tab_row);
}
