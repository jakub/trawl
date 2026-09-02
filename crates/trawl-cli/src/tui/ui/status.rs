// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Status bar.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::tui::App;
use crate::tui::state::{Focus, MainTab, TabStatus};

/// Render the status bar.
pub fn render(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let theme = &app.theme;
    let width = area.width as usize;
    let tab = app.active_tab();

    let mut spans = Vec::new();

    // Left: status (skip on Query tab — shown in results title, and Dashboard — has own indicator)
    if app.main_tab != MainTab::Query && app.main_tab != MainTab::Dashboard {
        let status_span = match &tab.status {
            TabStatus::Idle => Span::styled("idle", Style::default().fg(theme.status_idle)),
            TabStatus::Running { .. } => Span::styled(
                "running...",
                Style::default()
                    .fg(theme.status_warning)
                    .add_modifier(Modifier::BOLD),
            ),
            TabStatus::Success { duration_ms } => Span::styled(
                format!("success ({duration_ms}ms)"),
                Style::default().fg(theme.status_success),
            ),
            TabStatus::Error { message, .. } => Span::styled(
                format!("error: {message}"),
                Style::default().fg(theme.status_error),
            ),
        };
        spans.push(status_span);
    }

    if app.live_mode {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            "[LIVE]",
            Style::default()
                .fg(theme.status_info)
                .add_modifier(Modifier::BOLD),
        ));
    }

    let focus_text = match app.focus {
        Focus::Editor => " [editor] ",
        Focus::Results => " [results] ",
        Focus::Panel => " [panel] ",
    };
    spans.push(Span::raw(focus_text));

    let hints = get_context_hints(app, width);

    let left_len: usize = spans.iter().map(|s| s.content.len()).sum();

    #[allow(clippy::cast_possible_truncation)] // Terminal width is always < u16::MAX
    let padding_len = area.width.saturating_sub((left_len + hints.len()) as u16) as usize;

    spans.push(Span::raw(" ".repeat(padding_len)));
    spans.push(Span::styled(hints, Style::default().fg(theme.text_muted)));

    let line = Line::from(spans);
    let paragraph = Paragraph::new(line).style(Style::default().bg(theme.surface));
    frame.render_widget(paragraph, area);
}

/// Generate context-sensitive keybinding hints (responsive to terminal width).
fn get_context_hints(app: &App, width: usize) -> String {
    if app.focus == Focus::Panel {
        if app.main_tab == MainTab::Schema
            && app.panel.schema.as_ref().is_some_and(|s| s.filter_active)
        {
            return if width >= 55 {
                "type to filter | Enter: confirm | Esc: cancel".to_owned()
            } else {
                "Enter: confirm | Esc: cancel".to_owned()
            };
        }
        return match app.main_tab {
            MainTab::Schema => {
                if width >= 70 {
                    "↑↓: navigate | →: expand | ←: collapse | Enter: use field | /: filter"
                        .to_owned()
                } else if width >= 55 {
                    "↑↓ navigate | →← expand/collapse | Enter: insert | / filter".to_owned()
                } else {
                    "↑↓ navigate | Enter: insert".to_owned()
                }
            }
            MainTab::History => {
                if width >= 55 {
                    "↑↓: navigate | Enter: load query".to_owned()
                } else {
                    "Enter: load".to_owned()
                }
            }
            MainTab::Saved => {
                if width >= 65 {
                    "↑↓: navigate | Enter: load | s: schedule | Del: delete".to_owned()
                } else if width >= 45 {
                    "↑↓ navigate | Enter load | s sched | Del".to_owned()
                } else {
                    "Enter: load".to_owned()
                }
            }
            MainTab::Query | MainTab::Dashboard => String::new(),
        };
    }

    if app.live_mode {
        return if width >= 75 {
            "F9: stop live tail | F1: help | Ctrl+Q: quit".to_owned()
        } else if width >= 55 {
            "F9 stop | F1 help | ^Q quit".to_owned()
        } else {
            "F9 stop | ^Q quit".to_owned()
        };
    }

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

    match app.focus {
        Focus::Editor => {
            if width >= 75 {
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
                    "↑↓: select | Enter: detail | /: search | ←→: scroll | Tab: editor".to_owned()
                } else if width >= 55 {
                    "↑↓ select | Enter detail | / search | Tab".to_owned()
                } else {
                    "↑↓ select | Tab".to_owned()
                }
            } else if width >= 55 {
                "Tab: editor".to_owned()
            } else {
                "Tab".to_owned()
            }
        }
        Focus::Panel => String::new(),
    }
}
