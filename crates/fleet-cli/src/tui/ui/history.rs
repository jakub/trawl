//! Query history sidebar (F3).

use ratatui::Frame;
use ratatui::layout::Alignment;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use crate::tui::App;

/// Render the history sidebar.
pub fn render(app: &App, frame: &mut Frame<'_>) {
    let area = centered_rect(80, 80, frame.area());

    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Query History (F3) ")
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black).fg(Color::White));

    if let Some(history) = &app.history_cache {
        if history.entries.is_empty() {
            let text = Line::from("No query history yet");
            let paragraph = Paragraph::new(text)
                .block(block)
                .alignment(Alignment::Center);
            frame.render_widget(paragraph, area);
            return;
        }

        // Build list of history entries
        let items: Vec<ListItem<'_>> = history
            .entries
            .iter()
            .map(|entry| {
                // Format: "[timestamp] query (duration, rows, status)"
                let status_style = match entry.status {
                    fleet_client::QueryStatus::Success => Style::default().fg(Color::Green),
                    fleet_client::QueryStatus::Error => Style::default().fg(Color::Red),
                    fleet_client::QueryStatus::Timeout => Style::default().fg(Color::Yellow),
                };

                let line = Line::from(vec![
                    Span::styled(
                        format!("[{}] ", &entry.executed_at[11..19]), // HH:MM:SS
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(
                        truncate_query(&entry.query, 60),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::raw(" "),
                    Span::styled(
                        format!(
                            "({}ms, {} rows, {})",
                            entry.duration_ms, entry.row_count, entry.status
                        ),
                        status_style,
                    ),
                ]);
                ListItem::new(line)
            })
            .collect();

        let footer = Line::from(vec![
            Span::raw("Total: "),
            Span::styled(
                history.total.to_string(),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" queries | Showing most recent "),
            Span::styled(
                history.entries.len().to_string(),
                Style::default().fg(Color::Cyan),
            ),
        ]);

        let list = List::new(items)
            .block(block.title_bottom(footer).borders(Borders::ALL))
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("► ");

        let selected = app.sidebar.as_ref().map_or(0, |sb| sb.history_selected);
        let mut list_state = ListState::default().with_selected(Some(selected));

        frame.render_stateful_widget(list, area, &mut list_state);
    } else {
        // History not loaded
        let text = Line::from("History not available");
        let paragraph = Paragraph::new(text)
            .block(block)
            .alignment(Alignment::Center);

        frame.render_widget(paragraph, area);
    }
}

use super::common::{centered_rect, truncate_query};
