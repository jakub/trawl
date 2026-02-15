//! Schema browser sidebar (F2).

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph};

use crate::tui::App;

/// Render the schema browser sidebar.
pub fn render(app: &App, frame: &mut Frame<'_>) {
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

        let list = List::new(items).block(block.title_bottom(footer).borders(Borders::ALL));

        frame.render_widget(list, area);
    } else {
        // Schema not loaded yet
        let text = Line::from("Schema not available");
        let paragraph = Paragraph::new(text)
            .block(block)
            .alignment(Alignment::Center);

        frame.render_widget(paragraph, area);
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}
