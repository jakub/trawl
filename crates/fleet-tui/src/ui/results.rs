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

        // Calculate how many columns fit on screen (assume ~20 chars per column + borders)
        let col_width = 20;
        let max_cols = (area.width as usize).saturating_sub(2) / col_width; // -2 for borders
        let num_cols = max_cols.max(3).min(result.columns.len()); // Show at least 3, up to what fits

        let header_row = Row::new(
            result
                .columns
                .iter()
                .take(num_cols)
                .map(|col| Cell::from(col.name.as_str()))
                .collect::<Vec<_>>(),
        )
        .style(
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(Color::Yellow),
        );

        let data_rows: Vec<Row<'_>> = result
            .rows
            .iter()
            .skip(tab.scroll_offset)
            .take(area.height.saturating_sub(4) as usize)
            .map(|row_data| {
                Row::new(
                    row_data
                        .iter()
                        .take(num_cols)
                        .map(|value| Cell::from(value_to_string(value)))
                        .collect::<Vec<_>>(),
                )
            })
            .collect();

        let widths: Vec<Constraint> = (0..num_cols).map(|_| Constraint::Length(20)).collect();

        let title = format!(
            " Results ({} rows, showing {}/{} cols{}) ",
            result.rows.len(),
            num_cols,
            result.columns.len(),
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
            .rows(data_rows)
            .header(header_row)
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
fn value_to_string(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        Value::Boolean(b) => b.to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Float(f) => format!("{f:.2}"),
        Value::String(s) => s.clone(),
    }
}
