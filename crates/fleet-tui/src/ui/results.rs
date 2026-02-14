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

    if let Some(response) = &tab.result {
        let result = &response.result;

        // Build table header - style the Row, not individual Cells
        let header_cells: Vec<Cell<'_>> = result
            .columns
            .iter()
            .map(|col| Cell::from(col.name.clone()))
            .collect();
        let header = Row::new(header_cells).style(
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(Color::Yellow),
        );

        // Build table rows
        let rows: Vec<Row<'_>> = result
            .rows
            .iter()
            .skip(tab.scroll_offset)
            .take(area.height.saturating_sub(4) as usize)
            .map(|row_data| {
                let cells: Vec<Cell<'_>> = row_data
                    .iter()
                    .map(|value| Cell::from(value_to_string(value)))
                    .collect();
                Row::new(cells)
            })
            .collect();

        // Fixed width columns
        let widths: Vec<Constraint> = result
            .columns
            .iter()
            .map(|_| Constraint::Length(15))
            .collect();

        let title = format!(
            " Results ({} rows{}) ",
            result.rows.len(),
            if response.truncated {
                ", truncated"
            } else {
                ""
            }
        );

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
