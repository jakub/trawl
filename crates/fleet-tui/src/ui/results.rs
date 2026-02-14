//! Results table pane.

use fleet_engine::value::Value;
use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};

use crate::app::App;
use crate::state::Focus;

/// Render the results pane.
pub fn render(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let border_style = if app.focus == Focus::Results && app.sidebar.is_none() {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let tab = app.active_tab();

    if let Some(_response) = &tab.result {
        // TEMPORARY: Hardcoded test table to debug rendering issue
        let header = Row::new(vec![
            Cell::from("Test Col 1"),
            Cell::from("Test Col 2"),
            Cell::from("Test Col 3"),
        ])
        .style(
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(Color::Yellow),
        );

        let rows = vec![
            Row::new(vec![
                Cell::from("Row 1 Val 1"),
                Cell::from("Row 1 Val 2"),
                Cell::from("Row 1 Val 3"),
            ]),
            Row::new(vec![
                Cell::from("Row 2 Val 1"),
                Cell::from("Row 2 Val 2"),
                Cell::from("Row 2 Val 3"),
            ]),
        ];

        let widths = vec![
            Constraint::Length(15),
            Constraint::Length(15),
            Constraint::Length(15),
        ];

        let title = " Results (test) ".to_string();

        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(border_style);

        let table = Table::default()
            .rows(rows)
            .header(header)
            .block(block)
            .widths(widths);

        frame.render_widget(table, area);
    } else {
        // No results yet — show placeholder.
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Results ")
            .border_style(border_style);

        let text = Line::from("no results yet — execute a query with F5");
        let paragraph = Paragraph::new(text).block(block);
        frame.render_widget(paragraph, area);
    }
}

/// Convert a Value to a string for display.
#[allow(dead_code)] // Temporarily unused during testing
fn value_to_string(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        Value::Boolean(b) => b.to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Float(f) => format!("{f:.2}"),
        Value::String(s) => s.clone(),
    }
}
