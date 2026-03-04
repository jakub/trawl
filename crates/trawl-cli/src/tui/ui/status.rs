//! Status bar.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::tui::App;
use crate::tui::state::{Focus, TabStatus};

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
        TabStatus::Error { message, .. } => {
            Span::styled(format!("error: {message}"), Style::default().fg(Color::Red))
        }
    };
    spans.push(status_span);

    // Live mode indicator
    if app.live_mode {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            "[LIVE]",
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ));
    }

    // Focus indicator
    let focus_text = match app.focus {
        Focus::Editor => " [editor] ",
        Focus::Results => " [results] ",
        Focus::Sidebar => " [sidebar] ",
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
    use crate::tui::state::SidebarSection;

    // If sidebar is focused, show sidebar-specific hints.
    if app.focus == Focus::Sidebar
        && let Some(ref sb) = app.sidebar
    {
        if sb.section == SidebarSection::Schema && sb.schema.filter_active {
            return "type to filter | Enter: confirm | Esc: cancel".to_owned();
        }
        return match sb.section {
            SidebarSection::Schema => {
                "↑↓: navigate | →: expand | ←: collapse | /: filter | [/]: sections".to_owned()
            }
            SidebarSection::History => {
                "↑↓: navigate | enter: load query | [/]: sections".to_owned()
            }
            SidebarSection::Saved => {
                "↑↓: navigate | enter: load | s: schedule | del: delete | [/]: sections".to_owned()
            }
            SidebarSection::Reports => {
                "↑↓: navigate | enter: load last run | [/]: sections".to_owned()
            }
        };
    }

    // Live mode hints
    if app.live_mode {
        return "F9: stop live tail | F1: help | ctrl+q: quit".to_owned();
    }

    // Results search mode hints
    if let Some(ref search) = app.results_search {
        return if search.input_active {
            "type to search | Enter: confirm | Esc: cancel".to_owned()
        } else {
            "n: next | N: prev | /: new search | Esc: close".to_owned()
        };
    }

    // Focus-specific hints
    match app.focus {
        Focus::Editor => {
            if app.active_tab().editor.text().trim().is_empty() {
                "F3: history | F4: saved | F1: help".to_owned()
            } else {
                "F5: execute | ctrl+s: save | ctrl+l: clear".to_owned()
            }
        }
        Focus::Results => {
            if app.active_tab().result.is_some() {
                "↑↓: select | Enter: detail | /: search | tab: editor".to_owned()
            } else {
                "tab: editor | F5: execute query".to_owned()
            }
        }
        // Sidebar hints handled by early return above; fallback for safety.
        Focus::Sidebar => "tab: editor | [/]: sections | esc: close".to_owned(),
    }
}
