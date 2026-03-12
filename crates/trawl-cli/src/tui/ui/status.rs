//! Status bar.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::tui::App;
use crate::tui::state::{Focus, MainTab, TabStatus};

/// Render the status bar.
pub fn render(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let width = area.width as usize;
    let tab = app.active_tab();

    let mut spans = Vec::new();

    // Left: status (skip when on Query tab — it's shown in the results title now)
    if app.main_tab != MainTab::Query {
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
    }

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
        Focus::Panel => " [panel] ",
    };
    spans.push(Span::raw(focus_text));

    // Context-sensitive hints (responsive)
    let hints = get_context_hints(app, width);

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

/// Generate context-sensitive keybinding hints (responsive to terminal width).
fn get_context_hints(app: &App, width: usize) -> String {
    // Panel tab hints
    if app.focus == Focus::Panel {
        if app.main_tab == MainTab::Schema && app.panel.schema.filter_active {
            return if width >= 55 {
                "type to filter | Enter: confirm | Esc: cancel".to_owned()
            } else {
                "Enter: confirm | Esc: cancel".to_owned()
            };
        }
        return match app.main_tab {
            MainTab::Schema => {
                if width >= 75 {
                    "↑↓: navigate | →: expand | ←: collapse | /: filter | Esc: query".to_owned()
                } else if width >= 55 {
                    "↑↓ navigate | →← expand/collapse | / filter | Esc".to_owned()
                } else {
                    "↑↓ navigate | Esc".to_owned()
                }
            }
            MainTab::History => {
                if width >= 75 {
                    "↑↓: navigate | Enter: load query | Esc: query".to_owned()
                } else if width >= 55 {
                    "↑↓ navigate | Enter load | Esc".to_owned()
                } else {
                    "Enter load | Esc".to_owned()
                }
            }
            MainTab::Saved => {
                if width >= 75 {
                    "↑↓: navigate | Enter: load | s: schedule | Del: delete | Esc: query".to_owned()
                } else if width >= 55 {
                    "↑↓ navigate | Enter load | s sched | Del | Esc".to_owned()
                } else {
                    "Enter load | Esc".to_owned()
                }
            }
            MainTab::Query => String::new(),
        };
    }

    // Live mode hints
    if app.live_mode {
        return if width >= 75 {
            "F9: stop live tail | F1: help | Ctrl+Q: quit".to_owned()
        } else if width >= 55 {
            "F9 stop | F1 help | ^Q quit".to_owned()
        } else {
            "F9 stop | ^Q quit".to_owned()
        };
    }

    // Results search mode hints
    if let Some(ref search) = app.results_search {
        return if search.input_active {
            if width >= 55 {
                "type to search | Enter: confirm | Esc: cancel".to_owned()
            } else {
                "Enter: confirm | Esc: cancel".to_owned()
            }
        } else if width >= 55 {
            "n: next | N: prev | /: new search | Esc: close".to_owned()
        } else {
            "n/N: next/prev | Esc".to_owned()
        };
    }

    // Focus-specific hints
    match app.focus {
        Focus::Editor => {
            if app.active_tab().editor.text().trim().is_empty() {
                if width >= 75 {
                    "M-2: history | M-4: saved | F1: help".to_owned()
                } else if width >= 55 {
                    "M-2 history | M-4 saved | F1 help".to_owned()
                } else {
                    "F1 help".to_owned()
                }
            } else if width >= 75 {
                "Shift+Enter: execute | Ctrl+S: save | Ctrl+L: clear".to_owned()
            } else if width >= 55 {
                "\u{23ce}: execute | ^S save | ^L clear".to_owned()
            } else {
                "\u{23ce} execute".to_owned()
            }
        }
        Focus::Results => {
            if app.active_tab().result.is_some() {
                if width >= 75 {
                    "↑↓: select | Enter: detail | /: search | Tab: editor".to_owned()
                } else if width >= 55 {
                    "↑↓ select | Enter detail | / search | Tab".to_owned()
                } else {
                    "↑↓ select | Tab".to_owned()
                }
            } else if width >= 55 {
                "Tab: editor | Shift+Enter: execute query".to_owned()
            } else {
                "Tab editor".to_owned()
            }
        }
        Focus::Panel => String::new(),
    }
}
