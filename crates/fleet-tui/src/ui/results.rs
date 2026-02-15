//! Results table pane.

use fleet_engine::value::Value;
use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{
    Block, Borders, Cell, Padding, Paragraph, Row, Scrollbar, ScrollbarOrientation, ScrollbarState,
    Sparkline, Table,
};
use std::collections::HashMap;

use crate::app::App;
use crate::state::{ChartView, Focus};

/// Render the results pane.
pub fn render(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let tab = app.active_tab();

    if let Some(response) = &tab.result {
        let result = &response.result;
        let is_timechart = is_timechart_result(result);

        // Dispatch rendering based on view mode
        #[allow(clippy::match_same_arms)] // Bar/line chart renderers not yet implemented
        match tab.chart_view {
            ChartView::Table => render_table(app, frame, area, response),
            ChartView::Sparkline if is_timechart => {
                render_sparkline(app, frame, area, result);
            }
            ChartView::BarChart if is_timechart => {
                // TODO: implement bar chart rendering
                render_table(app, frame, area, response);
            }
            ChartView::LineChart if is_timechart => {
                // TODO: implement line chart rendering
                render_table(app, frame, area, response);
            }
            // Fallback to table for non-timechart queries
            _ => render_table(app, frame, area, response),
        }
    } else {
        render_placeholder(app, frame, area);
    }
}

/// Render the results as a table.
#[allow(clippy::too_many_lines)] // Table rendering + scrollbars requires detailed logic
fn render_table(
    app: &App,
    frame: &mut Frame<'_>,
    area: Rect,
    response: &fleet_client::QueryResponse,
) {
    let border_style = if app.focus == Focus::Results && app.sidebar.is_none() {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let tab = app.active_tab();
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
}

/// Render placeholder when no results are available.
fn render_placeholder(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let border_style = if app.focus == Focus::Results && app.sidebar.is_none() {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Results ")
        .border_style(border_style)
        .padding(Padding::horizontal(1));

    let text = Line::from("no results yet — execute a query with F5");
    let paragraph = Paragraph::new(text).block(block);
    frame.render_widget(paragraph, area);
}

/// Render sparkline visualization for timechart results.
fn render_sparkline(
    app: &App,
    frame: &mut Frame<'_>,
    area: Rect,
    result: &fleet_engine::value::QueryResult,
) {
    let border_style = if app.focus == Focus::Results && app.sidebar.is_none() {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let series = extract_series(result);

    if series.is_empty() {
        // Fallback to placeholder if extraction failed
        render_placeholder(app, frame, area);
        return;
    }

    // Single series: render one large sparkline
    if series.len() == 1 {
        let (label, values) = &series[0];
        let max_val = values.iter().max().copied().unwrap_or(100);

        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" {label} over time "))
            .title_bottom(format!(
                " {} points  •  max: {max_val}  •  'v' to toggle view ",
                values.len(),
            ))
            .border_style(border_style)
            .padding(Padding::horizontal(1));

        let sparkline = Sparkline::default()
            .data(values)
            .style(Style::default().fg(Color::Cyan))
            .max(max_val);

        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(sparkline, inner);
    } else {
        render_stacked_sparklines(app, frame, area, &series, border_style);
    }
}

/// Render multiple sparklines stacked vertically for multi-series timechart.
fn render_stacked_sparklines(
    _app: &App,
    frame: &mut Frame<'_>,
    area: Rect,
    series: &[(String, Vec<u64>)],
    border_style: Style,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" timechart by series ")
        .title_bottom(format!(" {} series  •  'v' to toggle view ", series.len()))
        .border_style(border_style)
        .padding(Padding::horizontal(1));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Calculate per-series height (leave 2 rows for labels)
    let available_height = inner.height.saturating_sub(2) as usize;
    let row_height = if series.is_empty() {
        1
    } else {
        available_height / series.len()
    };

    let colors = [
        Color::Cyan,
        Color::Yellow,
        Color::Magenta,
        Color::Green,
        Color::Red,
        Color::Blue,
    ];

    #[allow(clippy::cast_possible_truncation)] // Area height fits in u16
    for (idx, (label, values)) in series.iter().enumerate() {
        let y_offset = (idx * row_height) as u16;
        let sparkline_area = Rect {
            x: inner.x,
            y: inner.y + y_offset,
            width: inner.width.saturating_sub(20), // leave space for label
            height: row_height as u16,
        };

        let label_area = Rect {
            x: inner.x + inner.width.saturating_sub(18),
            y: inner.y + y_offset,
            width: 18,
            height: row_height as u16,
        };

        let max_val = values.iter().max().copied().unwrap_or(100);
        let color = colors[idx % colors.len()];

        let sparkline = Sparkline::default()
            .data(values)
            .style(Style::default().fg(color))
            .max(max_val);

        frame.render_widget(sparkline, sparkline_area);

        // Render label
        let label_text = format!("{label} (max:{max_val})");
        let paragraph = Paragraph::new(label_text).style(Style::default().fg(color));
        frame.render_widget(paragraph, label_area);
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

/// Detect if a query result is from a timechart query.
///
/// Timechart queries always have `_time` as the first column.
#[allow(dead_code)] // Used when chart rendering is implemented
fn is_timechart_result(result: &fleet_engine::value::QueryResult) -> bool {
    result
        .columns
        .first()
        .is_some_and(|col| col.name == "_time")
}

/// Convert a Value to u64 for chart rendering.
#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
fn value_to_u64(value: &Value) -> u64 {
    match value {
        Value::Integer(i) => (*i).max(0) as u64,
        Value::Float(f) => f.max(0.0) as u64,
        _ => 0, // Null, Boolean, String all map to 0
    }
}

/// Extract (label, values) tuples from timechart result.
///
/// Single series: [(_time, count)] → [("count", [v1, v2, ...])]
/// Multi series: [(_time, service, count)] → [("nginx", [v1, v2, ...]), ("apache", [...])]
fn extract_series(result: &fleet_engine::value::QueryResult) -> Vec<(String, Vec<u64>)> {
    if result.columns.len() == 2 {
        // Single series: _time, metric
        let metric_name = &result.columns[1].name;
        let values: Vec<u64> = result
            .rows
            .iter()
            .map(|row| value_to_u64(&row[1]))
            .collect();
        vec![(metric_name.clone(), values)]
    } else if result.columns.len() == 3 {
        // Multi series: _time, group_by, metric
        // Group rows by series label
        let mut series_map: HashMap<String, Vec<u64>> = HashMap::new();

        for row in &result.rows {
            let label = value_to_string(&row[1]);
            let value = value_to_u64(&row[2]);
            series_map.entry(label).or_default().push(value);
        }

        let mut series: Vec<(String, Vec<u64>)> = series_map.into_iter().collect();
        // Sort by label for consistent ordering
        series.sort_by(|a, b| a.0.cmp(&b.0));
        series
    } else {
        // Unsupported format, fallback to empty
        vec![]
    }
}
