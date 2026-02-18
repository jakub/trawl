//! Schema browser sidebar (F2).

use ratatui::Frame;
use ratatui::layout::Alignment;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use crate::tui::App;

/// Render the schema browser sidebar.
pub fn render(app: &App, frame: &mut Frame<'_>, scroll: usize) {
    let area = centered_rect(60, 80, frame.area());

    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Schema Browser (F2) ")
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black).fg(Color::White));

    if let Some(schema) = &app.schema_cache {
        // Build list of columns with name and type
        let items: Vec<ListItem<'_>> = schema
            .columns
            .iter()
            .map(|col| {
                let line = Line::from(vec![
                    Span::styled(format!("{:30}", col.name), Style::default().fg(Color::Cyan)),
                    Span::styled(&col.data_type, Style::default().fg(Color::Yellow)),
                ]);
                ListItem::new(line)
            })
            .collect();

        let footer = Line::from(vec![
            Span::raw("Total: "),
            Span::styled(
                schema.columns.len().to_string(),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" columns | "),
            Span::styled(
                schema.file_count.to_string(),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" parquet files"),
            if schema.cached {
                Span::styled(" (cached)", Style::default().fg(Color::DarkGray))
            } else {
                Span::raw("")
            },
        ]);

        // Clamp scroll to valid range
        let max_scroll = schema.columns.len().saturating_sub(1);
        let clamped = scroll.min(max_scroll);

        let list = List::new(items)
            .block(block.title_bottom(footer).borders(Borders::ALL))
            .highlight_style(Style::default().bg(Color::DarkGray));

        let mut state = ListState::default().with_offset(clamped);

        frame.render_stateful_widget(list, area, &mut state);
    } else {
        // Schema not loaded yet
        let text = Line::from("Schema not available");
        let paragraph = Paragraph::new(text)
            .block(block)
            .alignment(Alignment::Center);

        frame.render_widget(paragraph, area);
    }
}

use super::common::centered_rect;
