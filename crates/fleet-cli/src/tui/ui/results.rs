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
use std::collections::{HashMap, HashSet};

use ratatui::layout::{Alignment, Direction, Layout};
use ratatui::text::Span;

use crate::tui::App;
use crate::tui::state::{ChartView, Focus};

/// Render the results pane (table/sparkline + optional search bar).
pub fn render(app: &App, frame: &mut Frame<'_>, area: Rect) {
    // Split off a search bar row at the bottom if search is active.
    let (results_area, search_area) = if app.results_search.is_some() {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(1)])
            .split(area);
        (chunks[0], Some(chunks[1]))
    } else {
        (area, None)
    };

    let tab = app.active_tab();

    if let Some(response) = &tab.result {
        let result = &response.result;
        let is_timechart = is_timechart_result(result);

        // Dispatch rendering based on view mode
        match tab.chart_view {
            ChartView::Sparkline if is_timechart => {
                render_sparkline(app, frame, results_area, result);
            }
            // Table view, or sparkline fallback for non-timechart
            ChartView::Table | ChartView::Sparkline => {
                render_table(app, frame, results_area, response);
            }
        }
    } else {
        render_placeholder(app, frame, results_area);
    }

    // Render search bar if active.
    if let (Some(search), Some(bar_area)) = (&app.results_search, search_area) {
        render_search_bar(frame, search, bar_area);
    }
}

/// Render the search input bar at the bottom of the results pane.
fn render_search_bar(frame: &mut Frame<'_>, search: &crate::tui::state::ResultsSearch, area: Rect) {
    let match_info = if search.query.is_empty() {
        String::new()
    } else if search.matches.is_empty() {
        " (no matches)".to_owned()
    } else {
        format!(" ({}/{})", search.current_match + 1, search.matches.len())
    };

    let cursor = if search.input_active { "█" } else { "" };

    let line = Line::from(vec![
        Span::styled("/", Style::default().fg(Color::Yellow)),
        Span::styled(
            format!("{}{cursor}", search.query),
            Style::default().fg(Color::White),
        ),
        Span::styled(match_info, Style::default().fg(Color::DarkGray)),
    ]);

    let paragraph = Paragraph::new(line)
        .style(Style::default().bg(Color::Black))
        .alignment(Alignment::Left);
    frame.render_widget(paragraph, area);
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

    // Calculate visible row range first (needed for column width sampling)
    let max_visible_rows = area.height.saturating_sub(4) as usize; // -4 for borders and header
    let total_rows = result.row_count();
    let v_scroll = if total_rows <= max_visible_rows {
        0
    } else {
        tab.scroll_offset
            .min(total_rows.saturating_sub(max_visible_rows))
    };

    // Compute adaptive column widths
    let all_widths = compute_column_widths(result, v_scroll);

    // Calculate how many columns fit on screen
    let available_width = area.width.saturating_sub(4) as usize; // borders + padding
    let total_cols = result.columns.len();
    let h_scroll = tab
        .horizontal_scroll_offset
        .min(total_cols.saturating_sub(1));

    // Find how many columns fit starting from h_scroll
    let mut cols_width_sum = 0usize;
    let mut visible_cols = 0;
    for w in all_widths.iter().skip(h_scroll) {
        // +3 for cell padding/borders in ratatui Table
        let next = cols_width_sum + *w as usize + 3;
        if next > available_width && visible_cols > 0 {
            break;
        }
        cols_width_sum = next;
        visible_cols += 1;
    }
    let visible_cols = visible_cols.max(1).min(total_cols - h_scroll);

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

    // Build search match sets for highlighting.
    let (match_cells, current_match_cell) = if let Some(ref search) = app.results_search {
        let set: HashSet<(usize, usize)> = search.matches.iter().copied().collect();
        let current = search.matches.get(search.current_match).copied();
        (set, current)
    } else {
        (HashSet::new(), None)
    };

    // Build data rows with visible columns, truncating to column width
    let selected_row = tab.selected_row;
    let data_rows: Vec<Row<'_>> = result
        .rows
        .iter()
        .skip(v_scroll)
        .take(max_visible_rows)
        .enumerate()
        .map(|(display_idx, row_data)| {
            let abs_row = v_scroll + display_idx;
            let cells: Vec<Cell<'_>> = row_data
                .iter()
                .zip(all_widths.iter())
                .enumerate()
                .skip(h_scroll)
                .take(visible_cols)
                .map(|(col_idx, (value, &width))| {
                    let text = value_to_string(value);
                    let truncated = truncate_with_ellipsis(&text, width as usize);
                    let cell = Cell::from(truncated);
                    if current_match_cell == Some((abs_row, col_idx)) {
                        // Current match: bright yellow bg
                        cell.style(Style::default().bg(Color::Yellow).fg(Color::Black))
                    } else if match_cells.contains(&(abs_row, col_idx)) {
                        // Other matches: dim yellow bg
                        cell.style(
                            Style::default()
                                .bg(Color::DarkGray)
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        )
                    } else {
                        cell
                    }
                })
                .collect();
            let row = Row::new(cells);
            if selected_row == Some(abs_row) {
                row.style(Style::default().bg(Color::DarkGray).fg(Color::White))
            } else {
                row
            }
        })
        .collect();

    let widths: Vec<Constraint> = all_widths
        .iter()
        .skip(h_scroll)
        .take(visible_cols)
        .map(|&w| Constraint::Length(w))
        .collect();

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
    if total_cols > visible_cols {
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

    let (series, total_series) = extract_series(result);

    if series.is_empty() {
        // Fallback to placeholder if extraction failed
        render_placeholder(app, frame, area);
        return;
    }

    // Extract time metadata for title
    let time_info = extract_time_metadata(result);

    // Single series: render one large sparkline
    if series.len() == 1 {
        let (label, values) = &series[0];

        let title = if let Some((ref start, ref end, ref span)) = time_info {
            format!(" {label} • {start} to {end} • span: {span} ")
        } else {
            format!(" {label} over time ")
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .title_bottom(format!(" {} points  •  'v' to toggle view ", values.len(),))
            .border_style(border_style)
            .padding(Padding::horizontal(1));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        // Reserve space for y-axis labels (8 chars wide)
        let y_axis_width = 8;
        let sparkline_area = Rect {
            x: inner.x + y_axis_width,
            y: inner.y,
            width: inner.width.saturating_sub(y_axis_width),
            height: inner.height.saturating_sub(1), // Reserve 1 row for x-axis
        };

        // Downsample data to fit available width (preserves peaks via max-per-bucket)
        let display_data = downsample(values, sparkline_area.width as usize);
        let max_val = display_data.iter().max().copied().unwrap_or(100);

        let sparkline = Sparkline::default()
            .data(&display_data)
            .style(Style::default().fg(Color::Cyan))
            .max(max_val);

        frame.render_widget(sparkline, sparkline_area);

        // Render y-axis labels (max at top, 0 at bottom - sparkline is filled area from 0)
        let max_label =
            Paragraph::new(format!("{max_val:>7}")).style(Style::default().fg(Color::DarkGray));
        frame.render_widget(
            max_label,
            Rect {
                x: inner.x,
                y: inner.y,
                width: y_axis_width,
                height: 1,
            },
        );

        let zero_label = Paragraph::new("      0").style(Style::default().fg(Color::DarkGray));
        frame.render_widget(
            zero_label,
            Rect {
                x: inner.x,
                y: inner.y + sparkline_area.height.saturating_sub(1),
                width: y_axis_width,
                height: 1,
            },
        );

        // Render x-axis time labels (first and last time, properly spaced)
        if let Some((start, end, _)) = time_info {
            let available_width = sparkline_area.width as usize;
            let start_len = start.len();
            let end_len = end.len();
            let padding = available_width.saturating_sub(start_len + end_len);

            let x_axis_text = format!("{start}{}{end}", " ".repeat(padding));
            let x_axis_label =
                Paragraph::new(x_axis_text).style(Style::default().fg(Color::DarkGray));
            frame.render_widget(
                x_axis_label,
                Rect {
                    x: inner.x + y_axis_width,
                    y: inner.y + sparkline_area.height,
                    width: sparkline_area.width,
                    height: 1,
                },
            );
        }
    } else {
        render_stacked_sparklines(
            app,
            frame,
            area,
            &series,
            total_series,
            border_style,
            time_info,
        );
    }
}

/// Render multiple sparklines stacked vertically for multi-series timechart.
fn render_stacked_sparklines(
    _app: &App,
    frame: &mut Frame<'_>,
    area: Rect,
    series: &[(String, Vec<u64>)],
    total_series: usize,
    border_style: Style,
    time_info: Option<(String, String, String)>,
) {
    let title = if let Some((ref start, ref end, ref span)) = time_info {
        format!(" timechart • {start} to {end} • span: {span} ")
    } else {
        " timechart by series ".to_owned()
    };

    let series_info = if series.len() < total_series {
        format!(
            " top {} of {} series  •  'v' to toggle view ",
            series.len(),
            total_series
        )
    } else {
        format!(" {} series  •  'v' to toggle view ", series.len())
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title_bottom(series_info)
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

    // Render x-axis time labels at the bottom (using the reserved 2 rows)
    if let Some((start, end, _)) = time_info {
        let sparkline_width = inner.width.saturating_sub(20) as usize;
        let start_len = start.len();
        let end_len = end.len();
        let padding = sparkline_width.saturating_sub(start_len + end_len);

        let x_axis_text = format!("{start}{}{end}", " ".repeat(padding));
        let x_axis_label = Paragraph::new(x_axis_text).style(Style::default().fg(Color::DarkGray));
        #[allow(clippy::cast_possible_truncation)]
        let y = inner.y + (series.len() * row_height) as u16;
        frame.render_widget(
            x_axis_label,
            Rect {
                x: inner.x,
                y,
                width: inner.width.saturating_sub(20),
                height: 1,
            },
        );
    }
}

/// Compute adaptive column widths based on header + first N rows of data.
///
/// Samples up to `SAMPLE_ROWS` visible rows starting from `v_scroll`, taking
/// the max display width per column, clamped to `[MIN_COL, MAX_COL]`.
fn compute_column_widths(result: &fleet_engine::value::QueryResult, v_scroll: usize) -> Vec<u16> {
    const MIN_COL: usize = 8;
    const MAX_COL: usize = 60;
    const SAMPLE_ROWS: usize = 50;

    result
        .columns
        .iter()
        .enumerate()
        .map(|(col_idx, col)| {
            let mut max_width = col.name.len();

            for row in result.rows.iter().skip(v_scroll).take(SAMPLE_ROWS) {
                if let Some(value) = row.get(col_idx) {
                    let display_len = value_to_string(value).len();
                    if display_len > max_width {
                        max_width = display_len;
                    }
                }
            }

            #[allow(clippy::cast_possible_truncation)]
            let width = max_width.clamp(MIN_COL, MAX_COL) as u16;
            width
        })
        .collect()
}

/// Truncate a string to max width, appending `…` if truncated.
fn truncate_with_ellipsis(value: &str, max_width: usize) -> String {
    if value.chars().count() <= max_width {
        value.to_owned()
    } else {
        let truncated: String = value.chars().take(max_width.saturating_sub(1)).collect();
        format!("{truncated}\u{2026}")
    }
}

/// Convert a Value to a string for display.
pub fn value_to_string(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        Value::Boolean(b) => b.to_string(),
        Value::Integer(i) => i.to_string(),
        // display-friendly: 2dp for readability (CSV export preserves full precision)
        Value::Float(f) => format!("{f:.2}"),
        Value::String(s) => s.clone(),
        Value::Array(_) => value.to_string(),
    }
}

/// Detect if a query result is from a timechart query.
///
/// Timechart queries produce a `_time` column. We check any position
/// because `UNION ALL BY NAME` can reorder columns.
pub fn is_timechart_result(result: &fleet_engine::value::QueryResult) -> bool {
    result.columns.iter().any(|col| col.name == "_time")
}

/// Downsample data to fit within `target_width` columns.
///
/// When there are more data points than columns, groups points into buckets
/// and takes the max of each bucket (preserving peaks). When data fits or
/// is smaller, returns as-is.
#[allow(clippy::cast_precision_loss)] // terminal widths won't overflow f64
fn downsample(values: &[u64], target_width: usize) -> Vec<u64> {
    if target_width == 0 || values.is_empty() {
        return vec![];
    }
    if values.len() <= target_width {
        return values.to_vec();
    }

    let mut result = Vec::with_capacity(target_width);
    let bucket_size_f = values.len() as f64 / target_width as f64;

    for i in 0..target_width {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let start = (i as f64 * bucket_size_f) as usize;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let end = ((i + 1) as f64 * bucket_size_f) as usize;
        let end = end.min(values.len());

        let max_in_bucket = values[start..end].iter().max().copied().unwrap_or(0);
        result.push(max_in_bucket);
    }

    result
}

/// Convert a Value to u64 for chart rendering.
#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
fn value_to_u64(value: &Value) -> u64 {
    match value {
        Value::Integer(i) => (*i).max(0) as u64,
        Value::Float(f) => f.max(0.0) as u64,
        Value::String(s) => s.parse::<f64>().unwrap_or(0.0).max(0.0) as u64,
        _ => 0,
    }
}

/// Extract time metadata from timechart result.
///
/// Returns (`start_time`, `end_time`, `span_interval`) as formatted strings.
fn extract_time_metadata(
    result: &fleet_engine::value::QueryResult,
) -> Option<(String, String, String)> {
    if result.rows.is_empty() {
        return None;
    }

    // Find the _time column by name (UNION ALL BY NAME can reorder columns)
    let time_col = result.columns.iter().position(|c| c.name == "_time")?;

    // Get first and last time values
    let first_time = value_to_string(&result.rows[0][time_col]);
    let last_time = value_to_string(&result.rows[result.rows.len() - 1][time_col]);

    // Derive span from the minimum non-zero delta between consecutive time buckets.
    // Using min rather than just the first pair handles sparse data (empty buckets
    // aren't emitted by time_bucket) and group-by rows sharing the same _time.
    let span = if result.rows.len() >= 2 {
        let mut min_delta: Option<u64> = None;
        for pair in result.rows.windows(2) {
            let t1 = value_to_string(&pair[0][time_col]);
            let t2 = value_to_string(&pair[1][time_col]);
            if let (Some(s1), Some(s2)) = (parse_timestamp_secs(&t1), parse_timestamp_secs(&t2)) {
                let delta = s2.saturating_sub(s1);
                if delta > 0 {
                    min_delta = Some(min_delta.map_or(delta, |prev| prev.min(delta)));
                }
            }
        }
        min_delta.map_or("?".to_owned(), format_duration)
    } else {
        "N/A".to_owned()
    };

    // Format times (truncate if too long)
    let format_time = |s: String| {
        if s.len() > 19 {
            // Keep just date and time, drop subseconds/timezone
            s.chars().take(19).collect()
        } else {
            s
        }
    };

    Some((format_time(first_time), format_time(last_time), span))
}

/// Extract (label, values) tuples from timechart result.
///
/// Single series: [(_time, count)] → [("count", [v1, v2, ...])]
/// Multi series: [(_time, service, count)] → [("nginx", [v1, v2, ...]), ("apache", [...])]
///
/// Returns `(series, total_count)` where `total_count` is the number of distinct series
/// before any truncation (multi-series are capped to the top 6 by total value).
fn extract_series(result: &fleet_engine::value::QueryResult) -> (Vec<(String, Vec<u64>)>, usize) {
    const MAX_SERIES: usize = 6;

    // Find the _time column by name (UNION ALL BY NAME can reorder columns)
    let Some(time_col) = result.columns.iter().position(|c| c.name == "_time") else {
        return (vec![], 0);
    };

    // Non-_time columns are either metric or group_by + metric
    let other_cols: Vec<usize> = (0..result.columns.len())
        .filter(|&i| i != time_col)
        .collect();

    if other_cols.len() == 1 {
        // Single series: _time + one metric column
        let metric_idx = other_cols[0];
        let metric_name = &result.columns[metric_idx].name;
        let values: Vec<u64> = result
            .rows
            .iter()
            .map(|row| value_to_u64(&row[metric_idx]))
            .collect();
        (vec![(metric_name.clone(), values)], 1)
    } else {
        // 2+ non-time columns. Sample the first row to distinguish:
        //   - group-by mode: exactly 1 string col (group labels) + 1 numeric col (metric)
        //   - multi-agg mode: all numeric cols → each column becomes its own series
        let Some(first_row) = result.rows.first() else {
            return (vec![], 0);
        };

        let string_cols: Vec<usize> = other_cols
            .iter()
            .copied()
            .filter(|&i| matches!(first_row.get(i), Some(Value::String(_))))
            .collect();
        let numeric_cols: Vec<usize> = other_cols
            .iter()
            .copied()
            .filter(|&i| !matches!(first_row.get(i), Some(Value::String(_))))
            .collect();

        if string_cols.len() == 1 && numeric_cols.len() == 1 {
            // Group-by mode: one string col for labels, one numeric col for values
            let group_idx = string_cols[0];
            let metric_idx = numeric_cols[0];

            let mut series_map: HashMap<String, Vec<u64>> = HashMap::new();
            for row in &result.rows {
                let label = value_to_string(&row[group_idx]);
                let value = value_to_u64(&row[metric_idx]);
                series_map.entry(label).or_default().push(value);
            }

            let mut series: Vec<(String, Vec<u64>)> = series_map.into_iter().collect();
            let total = series.len();
            if series.len() > MAX_SERIES {
                series.sort_by(|a, b| {
                    let sum_b: u64 = b.1.iter().sum();
                    let sum_a: u64 = a.1.iter().sum();
                    sum_b.cmp(&sum_a)
                });
                series.truncate(MAX_SERIES);
            }
            series.sort_by(|a, b| a.0.cmp(&b.0));
            (series, total)
        } else if string_cols.is_empty() {
            // Multi-agg mode: each numeric column is its own series, labeled by column name
            let mut series: Vec<(String, Vec<u64>)> = numeric_cols
                .iter()
                .map(|&col_idx| {
                    let label = result.columns[col_idx].name.clone();
                    let values: Vec<u64> = result
                        .rows
                        .iter()
                        .map(|row| value_to_u64(&row[col_idx]))
                        .collect();
                    (label, values)
                })
                .collect();
            let total = series.len();
            if series.len() > MAX_SERIES {
                series.sort_by(|a, b| {
                    let sum_b: u64 = b.1.iter().sum();
                    let sum_a: u64 = a.1.iter().sum();
                    sum_b.cmp(&sum_a)
                });
                series.truncate(MAX_SERIES);
            }
            series.sort_by(|a, b| a.0.cmp(&b.0));
            (series, total)
        } else {
            // Ambiguous: multiple string cols or mixed in unexpected way
            (vec![], 0)
        }
    }
}

/// Parse "YYYY-MM-DD HH:MM:SS" into total seconds (for diffing, not epoch).
fn parse_timestamp_secs(s: &str) -> Option<u64> {
    // Expect at minimum "YYYY-MM-DD HH:MM:SS" (19 chars)
    if s.len() < 19 {
        return None;
    }
    let bytes = s.as_bytes();

    let year: u64 = s[0..4].parse().ok()?;
    let month: u64 = s[5..7].parse().ok()?;
    let day: u64 = s[8..10].parse().ok()?;

    // Delimiter between date and time can be ' ' or 'T'
    if bytes[10] != b' ' && bytes[10] != b'T' {
        return None;
    }

    let hour: u64 = s[11..13].parse().ok()?;
    let min: u64 = s[14..16].parse().ok()?;
    let sec: u64 = s[17..19].parse().ok()?;

    // Rough total seconds (not calendar-accurate, but fine for diffs within days)
    Some(year * 365 * 86400 + month * 30 * 86400 + day * 86400 + hour * 3600 + min * 60 + sec)
}

/// Format a duration in seconds as a human-readable string.
fn format_duration(secs: u64) -> String {
    if secs >= 86400 {
        format!("{}d", secs / 86400)
    } else if secs >= 3600 {
        format!("{}h", secs / 3600)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_engine::value::{Column, QueryResult};

    fn col(name: &str) -> Column {
        Column {
            name: name.to_owned(),
        }
    }

    #[test]
    fn extract_series_multi_series() {
        let result = QueryResult {
            columns: vec![col("_time"), col("service"), col("count")],
            rows: vec![
                vec![
                    Value::String("2024-01-01 00:00:00".into()),
                    Value::String("nginx".into()),
                    Value::Integer(42),
                ],
                vec![
                    Value::String("2024-01-01 00:00:00".into()),
                    Value::String("api".into()),
                    Value::Integer(17),
                ],
                vec![
                    Value::String("2024-01-01 00:05:00".into()),
                    Value::String("nginx".into()),
                    Value::Integer(38),
                ],
                vec![
                    Value::String("2024-01-01 00:05:00".into()),
                    Value::String("api".into()),
                    Value::Integer(22),
                ],
            ],
        };

        let (series, total) = extract_series(&result);
        assert_eq!(total, 2);
        assert_eq!(series.len(), 2);
        // Sorted alphabetically by label
        assert_eq!(series[0].0, "api");
        assert_eq!(series[0].1, vec![17, 22]);
        assert_eq!(series[1].0, "nginx");
        assert_eq!(series[1].1, vec![42, 38]);
    }

    #[test]
    fn extract_series_single_series() {
        let result = QueryResult {
            columns: vec![col("_time"), col("count")],
            rows: vec![
                vec![
                    Value::String("2024-01-01 00:00:00".into()),
                    Value::Integer(10),
                ],
                vec![
                    Value::String("2024-01-01 00:05:00".into()),
                    Value::Integer(20),
                ],
            ],
        };

        let (series, total) = extract_series(&result);
        assert_eq!(total, 1);
        assert_eq!(series.len(), 1);
        assert_eq!(series[0].0, "count");
        assert_eq!(series[0].1, vec![10, 20]);
    }

    #[test]
    fn extract_series_caps_to_top_6() {
        // Build a result with 10 services
        let mut rows = Vec::new();
        for i in 0..10 {
            rows.push(vec![
                Value::String("2024-01-01 00:00:00".into()),
                Value::String(format!("svc-{i}")),
                Value::Integer((i + 1) * 100), // svc-9 = 1000, svc-0 = 100
            ]);
        }

        let result = QueryResult {
            columns: vec![col("_time"), col("service"), col("count")],
            rows,
        };

        let (series, total) = extract_series(&result);
        assert_eq!(total, 10);
        assert_eq!(series.len(), 6);
        // Should keep the top 6 by value (svc-4 through svc-9)
        // and sort them alphabetically
        let labels: Vec<&str> = series.iter().map(|s| s.0.as_str()).collect();
        assert_eq!(
            labels,
            vec!["svc-4", "svc-5", "svc-6", "svc-7", "svc-8", "svc-9"]
        );
    }

    #[test]
    fn extract_series_multi_agg() {
        // timechart span=5m avg(rssi), avg(noise) → [_time, avg_rssi, avg_noise]
        let result = QueryResult {
            columns: vec![col("_time"), col("avg_rssi"), col("avg_noise")],
            rows: vec![
                vec![
                    Value::String("2024-01-01 00:00:00".into()),
                    Value::Integer(65),
                    Value::Integer(90),
                ],
                vec![
                    Value::String("2024-01-01 00:05:00".into()),
                    Value::Integer(70),
                    Value::Integer(85),
                ],
                vec![
                    Value::String("2024-01-01 00:10:00".into()),
                    Value::Integer(60),
                    Value::Integer(92),
                ],
            ],
        };

        let (series, total) = extract_series(&result);
        assert_eq!(total, 2);
        assert_eq!(series.len(), 2);
        // Sorted alphabetically by column name
        assert_eq!(series[0].0, "avg_noise");
        assert_eq!(series[0].1, vec![90, 85, 92]);
        assert_eq!(series[1].0, "avg_rssi");
        assert_eq!(series[1].1, vec![65, 70, 60]);
    }

    #[test]
    fn extract_series_multi_agg_three_cols() {
        // timechart with 3 aggregations
        let result = QueryResult {
            columns: vec![
                col("_time"),
                col("avg_rssi"),
                col("avg_noise"),
                col("avg_snr"),
            ],
            rows: vec![
                vec![
                    Value::String("2024-01-01 00:00:00".into()),
                    Value::Integer(65),
                    Value::Integer(90),
                    Value::Integer(25),
                ],
                vec![
                    Value::String("2024-01-01 00:05:00".into()),
                    Value::Integer(70),
                    Value::Integer(85),
                    Value::Integer(30),
                ],
            ],
        };

        let (series, total) = extract_series(&result);
        assert_eq!(total, 3);
        assert_eq!(series.len(), 3);
        assert_eq!(series[0].0, "avg_noise");
        assert_eq!(series[1].0, "avg_rssi");
        assert_eq!(series[2].0, "avg_snr");
    }

    #[test]
    fn extract_series_multi_agg_capped() {
        // 8 numeric columns → should cap to 6
        let cols: Vec<Column> = std::iter::once(col("_time"))
            .chain((0..8).map(|i| col(&format!("metric_{i}"))))
            .collect();
        let row: Vec<Value> = std::iter::once(Value::String("2024-01-01 00:00:00".into()))
            .chain((0..8).map(|i| Value::Integer((i + 1) * 10)))
            .collect();

        let result = QueryResult {
            columns: cols,
            rows: vec![row],
        };

        let (series, total) = extract_series(&result);
        assert_eq!(total, 8);
        assert_eq!(series.len(), 6);
        // Should keep the top 6 by value (metric_2 through metric_7, values 30..80)
        let labels: Vec<&str> = series.iter().map(|s| s.0.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "metric_2", "metric_3", "metric_4", "metric_5", "metric_6", "metric_7"
            ]
        );
    }

    #[test]
    fn extract_series_group_by_still_works() {
        // Existing group-by pattern with 2 cols should still work
        let result = QueryResult {
            columns: vec![col("_time"), col("service"), col("count")],
            rows: vec![
                vec![
                    Value::String("2024-01-01 00:00:00".into()),
                    Value::String("nginx".into()),
                    Value::Integer(42),
                ],
                vec![
                    Value::String("2024-01-01 00:00:00".into()),
                    Value::String("api".into()),
                    Value::Integer(17),
                ],
            ],
        };

        let (series, total) = extract_series(&result);
        assert_eq!(total, 2);
        assert_eq!(series[0].0, "api");
        assert_eq!(series[0].1, vec![17]);
        assert_eq!(series[1].0, "nginx");
        assert_eq!(series[1].1, vec![42]);
    }

    #[test]
    fn is_timechart_result_detects_time_column() {
        let with_time = QueryResult {
            columns: vec![col("service"), col("_time"), col("count")],
            rows: vec![],
        };
        assert!(is_timechart_result(&with_time));

        let without_time = QueryResult {
            columns: vec![col("service"), col("count")],
            rows: vec![],
        };
        assert!(!is_timechart_result(&without_time));
    }
}
