// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Saved-queries overlay, unreferenced.
//!
//! The Saved tab draws its list inline through `panels.rs` instead.
#![allow(dead_code, unused_imports)]

use ratatui::Frame;
use ratatui::layout::Alignment;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use trawl_core::sanitize::sanitize_display_text;

use crate::tui::App;

/// Render the saved-queries overlay.
pub fn render(app: &App, frame: &mut Frame<'_>) {
    let theme = &app.theme;
    let area = centered_rect(80, 80, frame.area());

    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Saved Queries (F4) ")
        .borders(Borders::ALL)
        .style(Style::default().bg(theme.surface).fg(theme.text_primary));

    if let Some(saved) = &app.saved_cache {
        if saved.queries.is_empty() {
            let text = Line::from("No saved queries yet");
            let paragraph = Paragraph::new(text)
                .block(block)
                .alignment(Alignment::Center);
            frame.render_widget(paragraph, area);
            return;
        }

        let items: Vec<ListItem<'_>> = saved
            .queries
            .iter()
            .map(|query| {
                // Format: "name - query (truncated)"
                let line = Line::from(vec![
                    Span::styled(
                        format!("{:25}", sanitize_display_text(&query.name)),
                        Style::default()
                            .fg(theme.status_success)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" - "),
                    Span::styled(
                        sanitize_display_text(&truncate_query(&query.query, 80)),
                        Style::default().fg(theme.text_accent),
                    ),
                ]);
                ListItem::new(line)
            })
            .collect();

        let footer = Line::from(vec![
            Span::raw("Total: "),
            Span::styled(
                saved.queries.len().to_string(),
                Style::default()
                    .fg(theme.status_success)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" saved queries"),
        ]);

        let list = List::new(items)
            .block(block.title_bottom(footer).borders(Borders::ALL))
            .highlight_style(
                Style::default()
                    .bg(theme.surface_highlight)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("► ");

        let selected = app.panel.saved_selected;
        let mut list_state = ListState::default().with_selected(Some(selected));

        frame.render_stateful_widget(list, area, &mut list_state);
    } else {
        let text = Line::from("Saved queries not available");
        let paragraph = Paragraph::new(text)
            .block(block)
            .alignment(Alignment::Center);

        frame.render_widget(paragraph, area);
    }
}

use super::common::centered_rect;

fn truncate_query(query: &str, max_len: usize) -> String {
    if query.len() <= max_len {
        query.to_owned()
    } else {
        let truncated: String = query.chars().take(max_len.saturating_sub(3)).collect();
        format!("{truncated}...")
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// The overlay has no caller, but it is compiled, so it draws a saved
    /// name and query through the sanitize rule like every other sink.
    #[test]
    fn dormant_saved_overlay_sanitizes_text() {
        let mut app = crate::tui::tests::test_app();
        app.saved_cache = Some(trawl_api::ListSavedResponse {
            queries: vec![trawl_api::SavedQueryResponse {
                id: 1,
                name: "sn\u{202e}sz".to_owned(),
                query: "qy\u{200b}qz\u{00ad}q2".to_owned(),
                created_at: "2026-01-01T00:00:00Z".to_owned(),
                updated_at: "2026-01-01T00:00:00Z".to_owned(),
                schedule: None,
            }],
        });
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| super::render(&app, frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        for forbidden in ['\u{202e}', '\u{200b}', '\u{00ad}'] {
            assert!(
                !screen.contains(forbidden),
                "U+{:04X} drawn: {screen}",
                u32::from(forbidden)
            );
        }
        for (field, fragment) in [
            ("name", "sn\u{fffd}sz"),
            ("query", "qy\u{fffd}qz\u{fffd}q2"),
        ] {
            assert!(
                screen.contains(fragment),
                "{field} not drawn as {fragment:?}: {screen}"
            );
        }
    }
}
