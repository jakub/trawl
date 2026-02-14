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

        // Build table header from column names.
        let header_cells = result
            .columns
            .iter()
            .map(|col| Cell::from(col.name.as_str()).style(Style::default().fg(Color::Yellow)));
        let header = Row::new(header_cells)
            .style(Style::default().add_modifier(Modifier::BOLD))
            .height(1);

        // Build table rows from result data.
        let rows: Vec<Row<'_>> = result
                .rows
                .iter()
                .skip(tab.scroll_offset)
                .take(area.height.saturating_sub(4) as usize) // Leave room for borders + header
                .map(|row_data| {
                    let cells = row_data.iter().map(|value| Cell::from(value_to_string(value)));
                    Row::new(cells).height(1)
                })
                .collect();

        // Calculate column widths (equal width for now).
        let col_count = result.columns.len().max(1);
        #[allow(clippy::cast_possible_truncation)] // Column count < u16::MAX
        let widths = vec![Constraint::Percentage(100 / col_count as u16); col_count];

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

        let table = Table::new(rows, widths).header(header).block(block);

        frame.render_widget(table, area);
    } else {
        // No results yet — show placeholder.
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Results ")
            .border_style(border_style);

        let text = Line::from("no results yet — execute a query with ctrl+enter");
        let paragraph = Paragraph::new(text).block(block);
        frame.render_widget(paragraph, area);
    }
}

/// Convert a Value to a string for display.
fn value_to_string(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        Value::Boolean(b) => b.to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Float(f) => format!("{f:.2}"),
        Value::String(s) => s.clone(),
    }
}
