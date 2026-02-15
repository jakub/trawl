//! Results table pane.

use fleet_engine::value::Value;
use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{
    Block, Borders, Cell, Padding, Paragraph, Row, Scrollbar, ScrollbarOrientation, ScrollbarState,
    Table,
};

use crate::app::App;
use crate::state::Focus;

/// Render the results pane.
#[allow(clippy::too_many_lines)] // Table rendering + scrollbars requires detailed logic
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
        let available_width = area.width.saturating_sub(2); // -2 for borders
        let max_cols_on_screen = (available_width as usize) / col_width;
        let max_cols_on_screen = max_cols_on_screen.max(3); // Show at least 3 columns

        // Calculate visible column range based on horizontal scroll
        let total_cols = result.columns.len();
        let h_scroll = if total_cols <= max_cols_on_screen {
            0 // All columns fit, no scrolling needed
        } else {
            tab.horizontal_scroll_offset
                .min(total_cols.saturating_sub(max_cols_on_screen))
        };
        let visible_cols = max_cols_on_screen.min(total_cols - h_scroll);

        // Build header with visible columns
        let header_row = Row::new(
            result
                .columns
                .iter()
                .skip(h_scroll)
                .take(visible_cols)
                .map(|col| Cell::from(col.name.as_str()))
                .collect::<Vec<_>>(),
        )
        .style(
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(Color::Yellow),
        );

        // Calculate visible row range based on vertical scroll
        let max_visible_rows = area.height.saturating_sub(4) as usize; // -4 for borders and header
        let total_rows = result.row_count();
        let v_scroll = if total_rows <= max_visible_rows {
            0 // All rows fit, no scrolling needed
        } else {
            tab.scroll_offset
                .min(total_rows.saturating_sub(max_visible_rows))
        };

        // Build data rows with visible columns
        let data_rows: Vec<Row<'_>> = result
            .rows
            .iter()
            .skip(v_scroll)
            .take(max_visible_rows)
            .map(|row_data| {
                Row::new(
                    row_data
                        .iter()
                        .skip(h_scroll)
                        .take(visible_cols)
                        .map(|value| Cell::from(value_to_string(value)))
                        .collect::<Vec<_>>(),
                )
            })
            .collect();

        let widths: Vec<Constraint> = (0..visible_cols).map(|_| Constraint::Length(20)).collect();

        let title = format!(
            " Results ({} rows, cols {}-{}/{}{}) ",
            total_rows,
            h_scroll + 1,
            (h_scroll + visible_cols).min(total_cols),
            total_cols,
            if response.truncated {
                ", truncated"
            } else {
                ""
            }
        );

        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(border_style)
            .padding(Padding::horizontal(1));

        let table = Table::default()
            .rows(data_rows)
            .header(header_row)
            .block(block)
            .widths(widths);

        frame.render_widget(table, area);

        // Render vertical scrollbar if needed
        if total_rows > max_visible_rows {
            let mut scrollbar_state = ScrollbarState::new(total_rows).position(v_scroll);

            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("↑"))
                .end_symbol(Some("↓"));

            frame.render_stateful_widget(
                scrollbar,
                area.inner(ratatui::layout::Margin {
                    vertical: 1,
                    horizontal: 0,
                }),
                &mut scrollbar_state,
            );
        }

        // Render horizontal scrollbar if needed
        if total_cols > max_cols_on_screen {
            let mut scrollbar_state = ScrollbarState::new(total_cols).position(h_scroll);

            let scrollbar = Scrollbar::new(ScrollbarOrientation::HorizontalBottom)
                .begin_symbol(Some("←"))
                .end_symbol(Some("→"));

            frame.render_stateful_widget(
                scrollbar,
                area.inner(ratatui::layout::Margin {
                    vertical: 0,
                    horizontal: 1,
                }),
                &mut scrollbar_state,
            );
        }
    } else {
        // No results yet — show placeholder.
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Results ")
            .border_style(border_style)
            .padding(Padding::horizontal(1));

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
