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

    let mut spans = Vec::new();

    // Left: status
    let status_span = match &tab.status {
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
    spans.push(status_span);

    // Live mode indicator
    if app.live_mode {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            "[LIVE 5s]",
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ));
    }

    // Focus indicator
    let focus_text = match app.focus {
        Focus::Editor => " [editor] ",
        Focus::Results => " [results] ",
    };
    spans.push(Span::raw(focus_text));

    // Context-sensitive hints
    let hints = get_context_hints(app);

    // Calculate left side length for padding
    let left_len: usize = spans.iter().map(|s| s.content.len()).sum();

    // Calculate padding
    #[allow(clippy::cast_possible_truncation)] // Terminal width is always < u16::MAX
    let padding_len = area.width.saturating_sub((left_len + hints.len()) as u16) as usize;

    // Add padding and hints
    spans.push(Span::raw(" ".repeat(padding_len)));
    spans.push(Span::styled(hints, Style::default().fg(Color::DarkGray)));

    let line = Line::from(spans);
    let paragraph = Paragraph::new(line).style(Style::default().bg(Color::Black));
    frame.render_widget(paragraph, area);
}

/// Generate context-sensitive keybinding hints.
fn get_context_hints(app: &App) -> String {
    use crate::state::Sidebar;

    // If sidebar is open, show sidebar-specific hints
    if let Some(sidebar) = app.sidebar {
        return match sidebar {
            Sidebar::Help => "esc: close help".to_owned(),
            Sidebar::Schema => "esc: close schema".to_owned(),
            Sidebar::History => "↑↓: navigate | enter: load query | esc: close".to_owned(),
            Sidebar::Saved => "esc: close saved queries".to_owned(),
        };
    }

    // Live mode hints
    if app.live_mode {
        return "F9: stop live tail | F1: help | ctrl+q: quit".to_owned();
    }

    // Focus-specific hints
    match app.focus {
        Focus::Editor => {
            if app.active_tab().editor.text().trim().is_empty() {
                "F3: history | F4: saved | F1: help".to_owned()
            } else {
                "F5: execute | ctrl+l: clear | F1: help".to_owned()
            }
        }
        Focus::Results => {
            if app.active_tab().result.is_some() {
                "↑↓←→: scroll | tab: editor | F9: live tail".to_owned()
            } else {
                "tab: editor | F5: execute query".to_owned()
            }
        }
    }
}
