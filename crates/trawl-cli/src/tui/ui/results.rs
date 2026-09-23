// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Results table pane.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Bar, BarChart as BarChartWidget, BarGroup, Block, Borders, Cell, Chart, Dataset,
    GraphType, Padding, Paragraph, Row, Scrollbar, ScrollbarOrientation, ScrollbarState, Sparkline,
    Table,
};
use std::collections::HashSet;
use trawl_api::display::{downsample, extract_series, is_timechart_result, value_to_string};
use trawl_engine::value::Value;

use crate::tui::App;
use crate::tui::state::{ChartView, ColumnConfig, Focus, TabStatus};
use crate::tui::theme::Theme;

/// Build the results pane frame title from the current tab status.
fn pane_title(status: &TabStatus, theme: &Theme) -> Line<'static> {
    let (text, color) = match status {
        TabStatus::Idle => return Line::from(" Results "),
        TabStatus::Running { .. } => (" Running... ", theme.status_warning),
        TabStatus::Success { duration_ms } => {
            return Line::from(format!(" Results ({duration_ms}ms) "));
        }
        TabStatus::Error { .. } => (" Error ", theme.status_error),
    };
    Line::from(Span::styled(text, Style::default().fg(color)))
}

/// Render the results pane (table/sparkline + optional search bar).
pub fn render(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let theme = &app.theme;

    // Split off a search bar row at the bottom if search is active.
    let (results_area, search_area) = if app.results_search.is_some() {
        let [content, bar] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(area);
        (content, Some(bar))
    } else {
        (area, None)
    };

    let tab = app.active_tab();

    if let TabStatus::Error {
        ref message,
        ref details,
    } = tab.status
    {
        render_error_display(app, frame, results_area, tab, message, details);
    } else if let Some(response) = &tab.result {
        let result = &response.result;
        let is_timechart = is_timechart_result(result);

        match tab.chart_view {
            ChartView::LineChart if is_timechart => {
                render_line_chart(app, frame, results_area, result);
            }
            ChartView::Sparkline if is_timechart => {
                render_sparkline(app, frame, results_area, result);
            }
            ChartView::BarChart if is_bar_chartable(result) => {
                render_bar_chart(app, frame, results_area, result);
            }
            _ => {
                render_table(app, frame, results_area, response);
            }
        }
    } else {
        render_placeholder(app, frame, results_area);
    }

    if let (Some(search), Some(bar_area)) = (&app.results_search, search_area) {
        render_search_bar(frame, theme, search, bar_area);
    }
}

/// Render the search input bar at the bottom of the results pane.
fn render_search_bar(
    frame: &mut Frame<'_>,
    theme: &Theme,
    search: &crate::tui::state::ResultsSearch,
    area: Rect,
) {
    let match_info = if search.query.is_empty() {
        String::new()
    } else if search.matches.is_empty() {
        " (no matches)".to_owned()
    } else {
        format!(" ({}/{})", search.current_match + 1, search.matches.len())
    };

    let cursor = if search.input_active { "█" } else { "" };

    let line = Line::from(vec![
        Span::styled("/", Style::default().fg(theme.search_match_active)),
        Span::styled(
            format!("{}{cursor}", search.query),
            Style::default().fg(theme.text_primary),
        ),
        Span::styled(match_info, Style::default().fg(theme.text_muted)),
    ]);

    let paragraph = Paragraph::new(line)
        .style(Style::default().bg(theme.surface))
        .alignment(Alignment::Left);
    frame.render_widget(paragraph, area);
}

/// Render the results as a table with pinned/scrollable column regions.
#[allow(clippy::too_many_lines)] // Table rendering + column regions + scrollbars
fn render_table(
    app: &App,
    frame: &mut Frame<'_>,
    area: Rect,
    response: &trawl_client::QueryResponse,
) {
    let theme = &app.theme;
    let border_style = if app.focus == Focus::Results {
        Style::default().fg(theme.border_focused)
    } else {
        Style::default().fg(theme.border_unfocused)
    };

    let tab = app.active_tab();
    let result = &response.result;
    let config = tab.column_config.as_ref();

    // Row range comes first: column width sampling reads from it. 2 borders
    // plus the header row account for 3 of the 4; the 4th is left spare.
    let max_visible_rows = area.height.saturating_sub(4) as usize;
    let total_rows = result.row_count();
    let v_scroll = if total_rows <= max_visible_rows {
        0
    } else {
        tab.scroll_offset
            .min(total_rows.saturating_sub(max_visible_rows))
    };

    let available_width = area.width.saturating_sub(4) as usize; // borders + padding

    // Widths for every column, not just the displayed ones.
    let all_widths = compute_column_widths(result, v_scroll, available_width, config);
    let total_cols = result.columns.len();

    let pinned = config.map_or_else(Vec::new, ColumnConfig::pinned_indices);
    let scrollable = config.map_or_else(
        || (0..total_cols).collect::<Vec<_>>(),
        ColumnConfig::scrollable_indices,
    );

    let visible_count = config.map_or(total_cols, ColumnConfig::visible_count);
    if visible_count == 0 {
        render_all_hidden_placeholder(app, frame, area);
        return;
    }

    let pinned_width: usize = pinned
        .iter()
        .enumerate()
        .map(|(i, &col_idx)| {
            let w = all_widths[col_idx] as usize;
            if i < pinned.len().saturating_sub(1) {
                w + 3 // column spacing
            } else {
                w
            }
        })
        .sum();

    // Separator takes 2 chars (" ┃") when both regions are non-empty.
    let separator_width = if !pinned.is_empty() && !scrollable.is_empty() {
        2
    } else {
        0
    };

    let scrollable_budget = available_width
        .saturating_sub(pinned_width)
        .saturating_sub(separator_width);

    let h_scroll = tab
        .horizontal_scroll_offset
        .min(scrollable.len().saturating_sub(1));

    // Find how many scrollable columns fit starting from h_scroll
    let mut cols_width_sum = 0usize;
    let mut visible_scrollable = 0;
    for &col_idx in scrollable.iter().skip(h_scroll) {
        let next = cols_width_sum + all_widths[col_idx] as usize + 5;
        if next > scrollable_budget && visible_scrollable > 0 {
            break;
        }
        cols_width_sum = next;
        visible_scrollable += 1;
    }
    let visible_scrollable = visible_scrollable
        .max(usize::from(!scrollable.is_empty()))
        .min(scrollable.len().saturating_sub(h_scroll));

    let display_cols: Vec<usize> = pinned
        .iter()
        .copied()
        .chain(
            scrollable
                .iter()
                .skip(h_scroll)
                .take(visible_scrollable)
                .copied(),
        )
        .collect();
    let total_display = display_cols.len();
    let pinned_count = pinned.len();

    // Column mode cursor (original index)
    let col_cursor = config.and_then(|c| c.selected);

    let header_cells: Vec<Cell<'_>> = display_cols
        .iter()
        .map(|&col_idx| {
            let cell = Cell::from(result.columns[col_idx].name.as_str());
            if col_cursor == Some(col_idx) {
                cell.style(
                    Style::default()
                        .bg(theme.text_accent)
                        .fg(theme.surface)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                cell
            }
        })
        .collect();
    let header_row = Row::new(header_cells).style(
        Style::default()
            .add_modifier(Modifier::BOLD)
            .fg(theme.table_header),
    );

    // Build search match sets for highlighting.
    let (match_cells, current_match_cell) = if let Some(ref search) = app.results_search {
        let set: HashSet<(usize, usize)> = search.matches.iter().copied().collect();
        let current = search.matches.get(search.current_match).copied();
        (set, current)
    } else {
        (HashSet::new(), None)
    };

    let selected_row = tab.selected_row;
    let data_rows: Vec<Row<'_>> = result
        .rows
        .iter()
        .skip(v_scroll)
        .take(max_visible_rows)
        .enumerate()
        .map(|(display_idx, row_data)| {
            let abs_row = v_scroll + display_idx;
            let cells: Vec<Cell<'_>> = display_cols
                .iter()
                .map(|&col_idx| {
                    let value = &row_data[col_idx];
                    let width = all_widths[col_idx] as usize;
                    let text = if trawl_core::severity::renders_as_severity(
                        &result.columns[col_idx].name,
                        &response.severity_columns,
                    ) {
                        // A severity cell shows its OTel token, so `17` reads
                        // `error`, the same vocabulary that filters it
                        // (ADR-0013 §6).
                        severity_cell_text(value)
                    } else {
                        value_to_string(value)
                    };
                    let truncated = truncate_with_ellipsis(&text, width);
                    let cell = Cell::from(truncated);
                    if current_match_cell == Some((abs_row, col_idx)) {
                        cell.style(
                            Style::default()
                                .bg(theme.search_match_active)
                                .fg(theme.surface),
                        )
                    } else if match_cells.contains(&(abs_row, col_idx)) {
                        cell.style(
                            Style::default()
                                .bg(theme.search_match_other)
                                .fg(theme.search_match_active)
                                .add_modifier(Modifier::BOLD),
                        )
                    } else {
                        cell
                    }
                })
                .collect();
            let row = Row::new(cells);
            if selected_row == Some(abs_row) {
                row.style(
                    Style::default()
                        .bg(theme.surface_highlight)
                        .fg(theme.text_primary),
                )
            } else {
                row
            }
        })
        .collect();

    let widths: Vec<Constraint> = display_cols
        .iter()
        .map(|&col_idx| Constraint::Length(all_widths[col_idx]))
        .collect();

    let pinned_info = if pinned_count > 0 {
        format!(", {pinned_count} pinned")
    } else {
        String::new()
    };
    let hidden_count = total_cols.saturating_sub(visible_count);
    let hidden_info = if hidden_count > 0 {
        format!(", {hidden_count} hidden")
    } else {
        String::new()
    };
    let title = match tab.status {
        TabStatus::Running { .. } | TabStatus::Error { .. } => pane_title(&tab.status, theme),
        _ => {
            let scroll_start = pinned_count + h_scroll + 1;
            let scroll_end = (pinned_count + h_scroll + visible_scrollable).min(visible_count);
            Line::from(format!(
                " Results ({total_rows} rows, cols {scroll_start}-{scroll_end}/{visible_count}{pinned_info}{hidden_info}) "
            ))
        }
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(border_style)
        .padding(Padding::horizontal(1));

    let table = Table::default()
        .rows(data_rows)
        .header(header_row)
        .block(block)
        .widths(&widths)
        .column_spacing(3);

    frame.render_widget(table, area);

    // Store column header x-ranges for mouse hit-testing and overlay dividers.
    let inner_x = area.x + 2; // border + padding
    let mut cumulative_x = inner_x;
    let mut header_ranges: Vec<(u16, u16, usize)> = Vec::with_capacity(total_display);

    let has_h_scrollbar = scrollable.len() > visible_scrollable;
    let divider_style = Style::default().fg(theme.border_unfocused);
    let y_start = area.y + 1;
    let y_end = area.y + area.height - 1 - u16::from(has_h_scrollbar);

    for (i, &col_idx) in display_cols.iter().enumerate() {
        let col_w = all_widths[col_idx];
        let x_start = cumulative_x;
        let x_end = cumulative_x + col_w;
        header_ranges.push((x_start, x_end, col_idx));
        cumulative_x += col_w;

        if i < total_display - 1 {
            let is_pin_boundary = i + 1 == pinned_count && pinned_count > 0;
            let divider_char = if is_pin_boundary { '┃' } else { '│' };
            let divider_x = cumulative_x + 1; // middle of 3-char gap
            let buf = frame.buffer_mut();
            for y in y_start..y_end {
                if divider_x < area.x + area.width - 1 {
                    buf[(divider_x, y)]
                        .set_char(divider_char)
                        .set_style(if is_pin_boundary {
                            Style::default().fg(theme.text_muted)
                        } else {
                            divider_style
                        });
                }
            }
            cumulative_x += 3; // column spacing
        }
    }

    if total_rows > max_visible_rows {
        let mut scrollbar_state = ScrollbarState::new(total_rows)
            .position(v_scroll)
            .viewport_content_length(max_visible_rows);

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

    // The pinned region never scrolls, so the horizontal bar's scale counts
    // only the scrollable columns.
    if has_h_scrollbar {
        let mut scrollbar_state = ScrollbarState::new(scrollable.len()).position(h_scroll);

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

    // Mouse click detection needs these ranges back on `App`, but rendering
    // only holds `&App`. The UI dispatcher, which does hold `&mut App`,
    // drains the thread-local via `take_header_ranges` right after this call.
    HEADER_RANGES.with(|cell| {
        *cell.borrow_mut() = header_ranges;
    });
}

/// Render placeholder when all columns are hidden.
fn render_all_hidden_placeholder(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let theme = &app.theme;
    let border_style = if app.focus == Focus::Results {
        Style::default().fg(theme.border_focused)
    } else {
        Style::default().fg(theme.border_unfocused)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Results ")
        .border_style(border_style)
        .padding(Padding::horizontal(1));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let text = Paragraph::new("All columns hidden — press H to restore")
        .alignment(Alignment::Center)
        .style(Style::default().fg(theme.text_muted));
    frame.render_widget(text, inner);
}

std::thread_local! {
    /// Column header x-ranges from the last `render_table` pass.
    static HEADER_RANGES: std::cell::RefCell<Vec<(u16, u16, usize)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Take the column header ranges computed during the last `render_table` call.
///
/// Returns the ranges and clears the thread-local. Called by the UI dispatcher
/// to populate `LayoutAreas::column_header_ranges`.
pub fn take_header_ranges() -> Vec<(u16, u16, usize)> {
    HEADER_RANGES.with(|cell| std::mem::take(&mut *cell.borrow_mut()))
}

/// Render placeholder when no results are available.
fn render_placeholder(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let theme = &app.theme;
    let border_style = if app.focus == Focus::Results {
        Style::default().fg(theme.border_focused)
    } else {
        Style::default().fg(theme.border_unfocused)
    };

    let tab = app.active_tab();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(pane_title(&tab.status, theme))
        .border_style(border_style)
        .padding(Padding::horizontal(1));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let dim = Style::default().fg(theme.text_muted);
    let client_version = trawl_core::version::PKG_VERSION;
    let server_version = app.server_version.as_deref().unwrap_or("\u{2014}");

    // Pad every label out to the longest one ("Command Palette", 15 chars)
    // so all the colons line up in one column. The block is then centered on
    // the longest line, shifted one column left on purpose.
    let colon_col: usize = 15; // offset of ":" within each line
    let longest_line = "Command Palette: https://trawl.sh".len(); // 33 chars
    let w = inner.width as usize;
    // Both subtractions saturate. The `- 1` is the deliberate one-column
    // shift, and on a pane too narrow to hold the longest line the halved
    // width is already 0, so plain subtraction underflowed: a panic in
    // debug, and `" ".repeat(usize::MAX)` in release. That is any terminal
    // under 39 columns, since the block eats two borders and two of
    // padding.
    let left_pad = w.saturating_sub(longest_line) / 2;
    let left_pad = left_pad.saturating_sub(1);
    let pad = " ".repeat(left_pad);

    let label = |name: &str, val: &str| -> String {
        let spacing = colon_col - name.len();
        format!("{pad}{}{name}: {val}", " ".repeat(spacing))
    };

    // "trawl" title: center it on the colon column (visually near the middle).
    let title_pad = left_pad + colon_col.saturating_sub(3);
    let title_line = format!("{}{}", " ".repeat(title_pad), "trawl");

    let content: Vec<Line<'_>> = vec![
        Line::from(Span::styled(
            title_line,
            Style::default()
                .fg(theme.text_accent)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(label("Command Palette", "Ctrl+P"), dim)),
        Line::from(Span::styled(label("Tabs", "Alt+<num>"), dim)),
        Line::from(Span::styled(label("Quit", "Ctrl+Q"), dim)),
        Line::from(Span::styled(label("Docs", "https://trawl.sh"), dim)),
        Line::from(""),
        Line::from(Span::styled(label("Client", client_version), dim)),
        Line::from(Span::styled(label("Server", server_version), dim)),
    ];

    // Vertically center by prepending empty lines.
    #[allow(clippy::cast_possible_truncation)] // content is always 9 lines
    let content_height = content.len() as u16;
    let top_pad = (inner.height.saturating_sub(content_height)) / 2;
    let mut lines: Vec<Line<'_>> = (0..top_pad).map(|_| Line::from("")).collect();
    lines.extend(content);

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, inner);
}

/// Render error details with span highlighting in the results pane.
///
/// Shows the error message in red, then the query text with the error
/// span underlined and a caret line below it.
#[allow(clippy::too_many_arguments)]
fn render_error_display(
    app: &App,
    frame: &mut Frame<'_>,
    area: Rect,
    tab: &crate::tui::state::Tab,
    message: &str,
    details: &[trawl_client::ErrorDetail],
) {
    let theme = &app.theme;
    let border_style = if app.focus == Focus::Results {
        Style::default().fg(theme.border_focused)
    } else {
        Style::default().fg(theme.border_unfocused)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(pane_title(&tab.status, theme))
        .border_style(border_style)
        .padding(Padding::horizontal(1));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let query_text = tab.editor.text();
    let mut lines: Vec<Line<'_>> = Vec::new();

    lines.push(Line::from(Span::styled(
        format!("error: {message}"),
        Style::default()
            .fg(theme.status_error)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));

    // A validation error's one detail has no span and repeats the message
    // above, so a list with no span renders the message alone.
    if !details.iter().any(|d| d.span.is_some()) || query_text.is_empty() {
        // No span info — just show the message.
        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, inner);
        return;
    }

    // Render each detail with span highlighting.
    for detail in details {
        if let Some(ref span) = detail.span {
            let start = span.start.min(query_text.len());
            let end = span.end.min(query_text.len()).max(start);

            // Find the line containing the span.
            let mut line_start = 0;
            let mut line_end = query_text.len();
            for (i, ch) in query_text.char_indices() {
                if ch == '\n' {
                    if i < start {
                        line_start = i + 1;
                    }
                    if i >= end && line_end == query_text.len() {
                        line_end = i;
                    }
                }
            }

            let query_line = &query_text[line_start..line_end];
            let col_start = start - line_start;
            let col_end = (end - line_start).min(query_line.len());
            let underline_len = (col_end - col_start).max(1);

            // Query text with error span highlighted.
            let before = &query_line[..col_start];
            let error_region = &query_line[col_start..col_end];
            let after = &query_line[col_end..];

            lines.push(Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(before.to_owned(), Style::default().fg(theme.text_primary)),
                Span::styled(
                    error_region.to_owned(),
                    Style::default()
                        .fg(theme.status_error)
                        .add_modifier(Modifier::UNDERLINED),
                ),
                Span::styled(after.to_owned(), Style::default().fg(theme.text_primary)),
            ]));

            lines.push(Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::raw(" ".repeat(col_start)),
                Span::styled(
                    "^".repeat(underline_len),
                    Style::default().fg(theme.status_error),
                ),
            ]));

            lines.push(Line::from(Span::styled(
                format!("  {}", detail.message),
                Style::default().fg(theme.text_muted),
            )));

            if let Some(ref label) = detail.label {
                lines.push(Line::from(Span::styled(
                    format!("  while parsing: {label}"),
                    Style::default().fg(theme.text_muted),
                )));
            }

            lines.push(Line::from(""));
        } else {
            // No span — just the message.
            lines.push(Line::from(Span::styled(
                format!("  {}", detail.message),
                Style::default().fg(theme.text_muted),
            )));
            lines.push(Line::from(""));
        }
    }

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, inner);
}

/// Detect if a result is suitable for bar chart visualization.
///
/// Requires at least one string column (label) and one numeric column (value),
/// and must not be a timechart result (those get line charts).
///
/// Numeric is decided by variant, `Value::UInt` included: a
/// `stats first(request_id) by service` over an id past `i64::MAX` is a
/// metric like any other, and reading it as text would leave the view
/// with no value column and refuse to draw.
pub fn is_bar_chartable(result: &trawl_engine::value::QueryResult) -> bool {
    if is_timechart_result(result) || result.rows.is_empty() {
        return false;
    }
    let first_row = &result.rows[0];
    let has_string = first_row.iter().any(|v| matches!(v, Value::String(_)));
    let has_numeric = first_row
        .iter()
        .any(|v| matches!(v, Value::Integer(_) | Value::UInt(_) | Value::Float(_)));
    has_string && has_numeric
}

/// Render braille line chart for timechart results using `Chart` widget.
#[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
fn render_line_chart(
    app: &App,
    frame: &mut Frame<'_>,
    area: Rect,
    result: &trawl_engine::value::QueryResult,
) {
    let theme = &app.theme;
    let border_style = if app.focus == Focus::Results {
        Style::default().fg(theme.border_focused)
    } else {
        Style::default().fg(theme.border_unfocused)
    };

    let (series, total_series) = extract_series(result);

    if series.is_empty() {
        render_placeholder(app, frame, area);
        return;
    }

    let time_info = extract_time_metadata(result);

    let title = if series.len() == 1 {
        let label = &series[0].0;
        if let Some((ref start, ref end, ref span)) = time_info {
            format!(" {label} \u{2022} {start} to {end} \u{2022} span: {span} ")
        } else {
            format!(" {label} over time ")
        }
    } else if let Some((ref start, ref end, ref span)) = time_info {
        format!(" timechart \u{2022} {start} to {end} \u{2022} span: {span} ")
    } else {
        " timechart by series ".to_owned()
    };

    let series_info = if series.len() < total_series {
        format!(
            " top {} of {} series  \u{2022}  'v' to toggle view ",
            series.len(),
            total_series,
        )
    } else {
        format!(" {} series  \u{2022}  'v' to toggle view ", series.len())
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title_bottom(series_info)
        .border_style(border_style);

    // Compute per-series data points and global bounds
    let colors = &theme.chart_series;
    let n_points = series.iter().map(|(_, v)| v.len()).max().unwrap_or(0);
    let x_max = (n_points.saturating_sub(1)) as f64;

    // Downsample series to fit within available width (braille gives 2x resolution)
    let inner_width = area.width.saturating_sub(12) as usize; // borders + y-axis labels
    let target = inner_width.saturating_mul(2).max(1);

    let mut all_points: Vec<Vec<(f64, f64)>> = Vec::with_capacity(series.len());
    let mut y_max: f64 = 0.0;

    for (_, values) in &series {
        let ds = downsample(values, target);
        let ds_max = (ds.len().saturating_sub(1)) as f64;
        let pts: Vec<(f64, f64)> = ds
            .iter()
            .enumerate()
            .map(|(i, &v)| {
                let x = if ds_max > 0.0 {
                    i as f64 / ds_max * x_max
                } else {
                    0.0
                };
                let y = v as f64;
                if y > y_max {
                    y_max = y;
                }
                (x, y)
            })
            .collect();
        all_points.push(pts);
    }

    // Ensure y_max is non-zero for axis rendering
    if y_max == 0.0 {
        y_max = 1.0;
    }

    let datasets: Vec<Dataset<'_>> = series
        .iter()
        .zip(all_points.iter())
        .enumerate()
        .map(|(idx, ((label, _), pts))| {
            Dataset::default()
                .name(label.as_str())
                .marker(symbols::Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(colors[idx % colors.len()]))
                .data(pts)
        })
        .collect();

    // Y-axis labels: 0, mid, max
    let y_mid = y_max / 2.0;
    let y_labels = vec![
        Span::raw("0"),
        Span::raw(format_axis_value(y_mid)),
        Span::raw(format_axis_value(y_max)),
    ];

    // X-axis labels: start and end time (or point indices)
    let x_labels = if let Some((ref start, ref end, _)) = time_info {
        vec![Span::raw(start.clone()), Span::raw(end.clone())]
    } else {
        vec![Span::raw("0"), Span::raw(format!("{n_points}"))]
    };

    // For multi-series charts, render the block separately so the inline
    // color legend sits inside the border, above the chart.
    if series.len() > 1 {
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(1)])
            .split(inner);

        let legend_spans: Vec<Span<'_>> = series
            .iter()
            .enumerate()
            .flat_map(|(idx, (label, _))| {
                let color = colors[idx % colors.len()];
                let mut spans = Vec::with_capacity(3);
                if idx > 0 {
                    spans.push(Span::raw("  "));
                }
                spans.push(Span::styled("\u{25A0} ", Style::default().fg(color)));
                spans.push(Span::styled(
                    label.as_str().to_owned(),
                    Style::default().fg(color),
                ));
                spans
            })
            .collect();

        let legend = Paragraph::new(Line::from(legend_spans)).alignment(Alignment::Center);
        frame.render_widget(legend, chunks[0]);

        let chart = Chart::new(datasets)
            .x_axis(
                Axis::default()
                    .style(Style::default().fg(theme.text_muted))
                    .bounds([0.0, x_max.max(1.0)])
                    .labels(x_labels),
            )
            .y_axis(
                Axis::default()
                    .style(Style::default().fg(theme.text_muted))
                    .bounds([0.0, y_max])
                    .labels(y_labels),
            )
            .legend_position(None);
        frame.render_widget(chart, chunks[1]);
    } else {
        let chart = Chart::new(datasets)
            .block(block)
            .x_axis(
                Axis::default()
                    .style(Style::default().fg(theme.text_muted))
                    .bounds([0.0, x_max.max(1.0)])
                    .labels(x_labels),
            )
            .y_axis(
                Axis::default()
                    .style(Style::default().fg(theme.text_muted))
                    .bounds([0.0, y_max])
                    .labels(y_labels),
            )
            .legend_position(None);
        frame.render_widget(chart, area);
    }
}

/// Format a numeric value for axis labels (compact representation).
fn format_axis_value(v: f64) -> String {
    if v >= 1_000_000.0 {
        format!("{:.1}M", v / 1_000_000.0)
    } else if v >= 1_000.0 {
        format!("{:.1}K", v / 1_000.0)
    } else if (v - v.floor()).abs() < f64::EPSILON {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let i = v as u64;
        format!("{i}")
    } else {
        format!("{v:.1}")
    }
}

/// Render horizontal bar chart for aggregation results.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn render_bar_chart(
    app: &App,
    frame: &mut Frame<'_>,
    area: Rect,
    result: &trawl_engine::value::QueryResult,
) {
    let theme = &app.theme;
    let border_style = if app.focus == Focus::Results {
        Style::default().fg(theme.border_focused)
    } else {
        Style::default().fg(theme.border_unfocused)
    };

    // Find first string column (label) and first numeric column (value)
    let first_row = &result.rows[0];
    let label_col = first_row
        .iter()
        .position(|v| matches!(v, Value::String(_)))
        .unwrap_or(0);
    let value_col = first_row
        .iter()
        .position(|v| matches!(v, Value::Integer(_) | Value::UInt(_) | Value::Float(_)))
        .unwrap_or(1);

    let metric_name = &result.columns[value_col].name;
    let label_name = &result.columns[label_col].name;

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {metric_name} by {label_name} "))
        .title_bottom(" 'v' to toggle view ")
        .border_style(border_style);

    let colors = &theme.chart_series;
    let max_bars = 20.min(result.rows.len());

    // Pre-compute labels and values so we can pad labels to a uniform width,
    // ensuring all bars start at the same x position (consistent y-axis).
    let entries: Vec<(String, u64, usize)> = result
        .rows
        .iter()
        .take(max_bars)
        .enumerate()
        .map(|(idx, row)| {
            let label = value_to_string(&row[label_col]);
            // A bar's height is a `u64`, which is `Value::UInt`'s own
            // width, so an oversized unsigned draws at its real
            // magnitude. A negative or a NaN clamps to the bottom of the
            // axis, as it always has.
            let value = match &row[value_col] {
                Value::Integer(i) => (*i).max(0) as u64,
                Value::UInt(u) => *u,
                Value::Float(f) => f.max(0.0) as u64,
                _ => 0,
            };
            (label, value, idx)
        })
        .collect();

    // Compute fixed label width: max(name_len) + space + max(value_len) + separator
    let max_name_len = entries.iter().map(|(l, _, _)| l.len()).max().unwrap_or(0);
    let max_value_len = entries
        .iter()
        .map(|(_, v, _)| format!("{v}").len())
        .max()
        .unwrap_or(0);

    let bars: Vec<Bar<'_>> = entries
        .iter()
        .map(|(label, value, idx)| {
            // Pad name to max width, right-align value — creates a uniform "y-axis" edge
            let value_str = format!("{value}");
            let name_pad = max_name_len.saturating_sub(label.len());
            let val_pad = max_value_len.saturating_sub(value_str.len());
            let padded = format!(
                "{label}{}{}{value_str} ",
                " ".repeat(name_pad),
                " ".repeat(val_pad + 2),
            );
            Bar::default()
                .value(*value)
                .label(Line::from(padded))
                .text_value(String::new()) // hide value text inside the bar
                .style(Style::default().fg(colors[idx % colors.len()]))
        })
        .collect();

    let group = BarGroup::default().bars(&bars);

    let barchart = BarChartWidget::default()
        .block(block)
        .data(group)
        .direction(Direction::Horizontal)
        .bar_width(1)
        .bar_gap(0)
        .label_style(Style::default().fg(theme.text_muted));

    frame.render_widget(barchart, area);
}

/// Render sparkline visualization for timechart results.
fn render_sparkline(
    app: &App,
    frame: &mut Frame<'_>,
    area: Rect,
    result: &trawl_engine::value::QueryResult,
) {
    let theme = &app.theme;
    let border_style = if app.focus == Focus::Results {
        Style::default().fg(theme.border_focused)
    } else {
        Style::default().fg(theme.border_unfocused)
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
            .title_bottom(format!(" {} points  •  'v' to toggle view ", values.len()))
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
            .style(Style::default().fg(theme.chart_series[0]))
            .max(max_val);

        frame.render_widget(sparkline, sparkline_area);

        // Render y-axis labels (max at top, 0 at bottom - sparkline is filled area from 0)
        let max_label =
            Paragraph::new(format!("{max_val:>7}")).style(Style::default().fg(theme.text_muted));
        frame.render_widget(
            max_label,
            Rect {
                x: inner.x,
                y: inner.y,
                width: y_axis_width,
                height: 1,
            },
        );

        let zero_label = Paragraph::new("      0").style(Style::default().fg(theme.text_muted));
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
                Paragraph::new(x_axis_text).style(Style::default().fg(theme.text_muted));
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

/// Render y-axis labels and series name for one sparkline row.
///
/// When `row_height >= 3`, shows max at top, min at bottom, and the series label
/// vertically centered. Otherwise falls back to label-only.
#[allow(clippy::cast_possible_truncation)]
fn render_series_label(
    frame: &mut Frame<'_>,
    theme: &Theme,
    area: Rect,
    label: &str,
    min_val: u64,
    max_val: u64,
    series_color: ratatui::style::Color,
) {
    let w = area.width as usize;
    let display_label = if label.len() > w {
        format!("{}…", &label[..w - 1])
    } else {
        label.to_owned()
    };

    if area.height >= 3 {
        frame.render_widget(
            Paragraph::new(format!("{max_val:>w$}")).style(Style::default().fg(theme.text_muted)),
            Rect { height: 1, ..area },
        );

        frame.render_widget(
            Paragraph::new(format!("{min_val:>w$}")).style(Style::default().fg(theme.text_muted)),
            Rect {
                y: area.y + area.height - 1,
                height: 1,
                ..area
            },
        );

        frame.render_widget(
            Paragraph::new(format!("{display_label:>w$}")).style(Style::default().fg(series_color)),
            Rect {
                y: area.y + area.height / 2,
                height: 1,
                ..area
            },
        );
    } else {
        // Too short for y-axis labels — just show the series name
        frame.render_widget(
            Paragraph::new(format!("{display_label:>w$}")).style(Style::default().fg(series_color)),
            area,
        );
    }
}

/// Render multiple sparklines stacked vertically for multi-series timechart.
#[allow(clippy::cast_possible_truncation)]
fn render_stacked_sparklines(
    app: &App,
    frame: &mut Frame<'_>,
    area: Rect,
    series: &[(String, Vec<u64>)],
    total_series: usize,
    border_style: Style,
    time_info: Option<(String, String, String)>,
) {
    let theme = &app.theme;
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
        .padding(Padding::new(1, 1, 1, 0));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Calculate per-series height: 1 row for x-axis labels, 1-row gaps between series
    let gap_rows = series.len().saturating_sub(1);
    let available_height = inner.height.saturating_sub(1) as usize; // 1 row for x-axis
    let total_chart_rows = available_height.saturating_sub(gap_rows);
    let base_height = if series.is_empty() {
        1
    } else {
        total_chart_rows / series.len()
    };
    let remainder = if series.is_empty() {
        0
    } else {
        total_chart_rows % series.len()
    };

    let label_width: u16 = 20;
    let colors = &theme.chart_series;

    let mut y_offset: u16 = 0;
    for (idx, (label, values)) in series.iter().enumerate() {
        // Distribute remainder rows: first `remainder` series get +1 height
        let row_height = base_height + usize::from(idx < remainder);

        let sparkline_area = Rect {
            x: inner.x,
            y: inner.y + y_offset,
            width: inner.width.saturating_sub(label_width),
            height: row_height as u16,
        };

        // Downsample to fit available width (preserves peaks via max-per-bucket)
        let display_data = downsample(values, sparkline_area.width as usize);

        // Both bounds come from this slice, so `max - min` and `v - min`
        // cannot underflow however wide the values are — a full-`u64`
        // metric rebases to a full-`u64` range, and ratatui scales that
        // through `u128`.
        let max_val = display_data.iter().max().copied().unwrap_or(0);
        let min_val = display_data.iter().min().copied().unwrap_or(0);
        let range = (max_val - min_val).max(1);

        // Rebase to 0..range so sparkline uses full vertical extent
        let rebased: Vec<u64> = display_data.iter().map(|v| v - min_val).collect();

        let color = colors[idx % colors.len()];

        let sparkline = Sparkline::default()
            .data(&rebased)
            .style(Style::default().fg(color))
            .max(range);

        frame.render_widget(sparkline, sparkline_area);

        // Y-axis labels + centered series name in right-side label area
        let label_w = label_width - 2;
        let label_area = Rect {
            x: inner.x + inner.width.saturating_sub(label_w),
            y: inner.y + y_offset,
            width: label_w,
            height: row_height as u16,
        };
        render_series_label(frame, theme, label_area, label, min_val, max_val, color);

        // Advance y_offset: row height + 1 gap row (except after last series)
        y_offset += row_height as u16;
        if idx < series.len() - 1 {
            y_offset += 1;
        }
    }

    // Render x-axis time labels (1 row at bottom)
    if let Some((start, end, _)) = time_info {
        let sparkline_width = inner.width.saturating_sub(label_width) as usize;
        let start_len = start.len();
        let end_len = end.len();
        let padding = sparkline_width.saturating_sub(start_len + end_len);

        let x_axis_text = format!("{start}{}{end}", " ".repeat(padding));
        let x_axis_label = Paragraph::new(x_axis_text).style(Style::default().fg(theme.text_muted));
        frame.render_widget(
            x_axis_label,
            Rect {
                x: inner.x,
                y: inner.y + y_offset,
                width: inner.width.saturating_sub(label_width),
                height: 1,
            },
        );
    }
}

/// Compute adaptive column widths based on header + first N rows of data.
///
/// Samples up to `SAMPLE_ROWS` visible rows starting from `v_scroll`, taking
/// the max display width per column, clamped to `MIN_COL` and to each
/// column's equal share of `available_width` (itself capped at `MAX_COL`).
/// Respects `width_override` from `ColumnConfig`; hidden columns get width 0.
fn compute_column_widths(
    result: &trawl_engine::value::QueryResult,
    v_scroll: usize,
    available_width: usize,
    config: Option<&ColumnConfig>,
) -> Vec<u16> {
    const MIN_COL: usize = 8;
    const MAX_COL: usize = 60;
    const CELL_PADDING: usize = 5; // ratatui column_spacing(3) + 2 for borders
    const SAMPLE_ROWS: usize = 50;

    let visible_count = config
        .map_or(result.columns.len(), ColumnConfig::visible_count)
        .max(1);
    let per_col_budget = (available_width / visible_count).saturating_sub(CELL_PADDING);
    let max_col = per_col_budget.clamp(MIN_COL, MAX_COL);

    result
        .columns
        .iter()
        .enumerate()
        .map(|(col_idx, col)| {
            if let Some(cfg) = config {
                if cfg.columns.get(col_idx).is_some_and(|e| e.hidden) {
                    return 0;
                }
                if let Some(w) = cfg.columns.get(col_idx).and_then(|e| e.width_override) {
                    return w;
                }
            }

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
            let width = max_width.clamp(MIN_COL, max_col) as u16;
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

/// Extract time metadata from timechart result.
///
/// Returns (`start_time`, `end_time`, `span_interval`) as formatted strings.
fn extract_time_metadata(
    result: &trawl_engine::value::QueryResult,
) -> Option<(String, String, String)> {
    if result.rows.is_empty() {
        return None;
    }

    // Find the _time column by name (UNION ALL BY NAME can reorder columns)
    let time_col = result.columns.iter().position(|c| c.name == "_time")?;

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

/// A `_severity` cell as the TUI shows it: the shared `OTel` token, or the
/// grid's ordinary rendering where the ladder has no reading for the value.
fn severity_cell_text(value: &trawl_engine::value::Value) -> String {
    crate::cli::severity_token(value).map_or_else(|| value_to_string(value), str::to_owned)
}

/// Both timechart views survive metrics at the top of the `u64` range.
///
/// The stacked sparkline rebases each series by subtracting its own
/// minimum and hands the range to ratatui as a scale. Run under the
/// debug profile's overflow checks, this pins that arithmetic — and the
/// shared `extract_series` ranking behind it — against a metric column
/// holding `u64::MAX`.
#[cfg(test)]
#[test]
fn timechart_sparklines_survive_unsigned_magnitudes() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use trawl_engine::value::{Column, QueryResult};

    let col = |name: &str| Column {
        name: name.to_owned(),
    };
    let result = QueryResult {
        columns: vec![col("_time"), col("m0"), col("m1")],
        rows: vec![
            vec![
                Value::String("2026-01-01 00:00:00".to_owned()),
                Value::Integer(0),
                Value::UInt(u64::MAX),
            ],
            vec![
                Value::String("2026-01-01 00:05:00".to_owned()),
                Value::UInt(u64::MAX),
                Value::Integer(0),
            ],
        ],
    };

    // The samples reach the renderers intact: a conversion that dropped
    // or zeroed the unsigned cells would still leave non-empty series
    // (the fixture carries integers too), and both draws below would
    // succeed over zeros.
    let (series, _) = extract_series(&result);
    assert!(
        series.contains(&("m0".to_owned(), vec![0, u64::MAX]))
            && series.contains(&("m1".to_owned(), vec![u64::MAX, 0])),
        "extract_series must preserve both unsigned samples: {series:?}"
    );

    for view in [ChartView::Sparkline, ChartView::LineChart] {
        let mut app = crate::tui::tests::test_app();
        let returned = result.rows.len();
        app.tab.result = Some(trawl_client::QueryResponse {
            execution: None,
            result: result.clone(),
            pagination: trawl_client::PaginationMeta {
                limit: 10_000,
                offset: 0,
                returned,
                total: returned,
            },
            degraded_fields: Vec::new(),
            severity_columns: Vec::new(),
        });
        app.tab.chart_view = view;

        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| crate::tui::ui::render(&mut app, f))
            .unwrap_or_else(|e| panic!("{view:?} must render: {e}"));
    }
}

/// A `stats first(request_id) by service` over an id past `i64::MAX`
/// draws a real bar.
///
/// Two ways it used to go wrong at once: the classifier saw no numeric
/// column and refused to draw at all, or the per-row conversion fell
/// through its wildcard and drew every bar at zero. Rendered through the
/// whole UI, because the view is chosen by `is_bar_chartable` and drawn
/// by `render_bar_chart` — the bug lived in the seam between them.
#[cfg(test)]
#[test]
fn bar_chart_renders_uint_metric() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use trawl_engine::value::{Column, QueryResult};

    let result = QueryResult {
        columns: vec![
            Column {
                name: "service".to_owned(),
            },
            Column {
                name: "first_request_id".to_owned(),
            },
        ],
        rows: vec![vec![
            Value::String("nginx".to_owned()),
            Value::UInt(u64::MAX),
        ]],
    };
    assert!(
        is_bar_chartable(&result),
        "an unsigned metric column must still offer a bar chart"
    );

    let mut app = crate::tui::tests::test_app();
    let returned = result.rows.len();
    app.tab.result = Some(trawl_client::QueryResponse {
        execution: None,
        result,
        pagination: trawl_client::PaginationMeta {
            limit: 10_000,
            offset: 0,
            returned,
            total: returned,
        },
        degraded_fields: Vec::new(),
        severity_columns: Vec::new(),
    });
    app.tab.chart_view = ChartView::BarChart;

    let backend = TestBackend::new(120, 20);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|f| crate::tui::ui::render(&mut app, f))
        .expect("render");
    let screen = terminal.backend().to_string();

    assert!(
        screen.contains("18446744073709551615"),
        "the bar label must carry the whole number:\n{screen}"
    );
    assert!(
        screen.contains('\u{2588}'),
        "the bar must have a height, not sit at zero:\n{screen}"
    );
}

#[cfg(test)]
mod severity_tests {
    use super::severity_cell_text;
    use trawl_engine::value::Value;

    /// The TUI shows the token, injectively — `error2` is not `error`.
    #[test]
    fn a_severity_cell_shows_its_otel_token() {
        assert_eq!(severity_cell_text(&Value::Integer(17)), "error");
        assert_eq!(severity_cell_text(&Value::Integer(18)), "error2");
        assert_eq!(severity_cell_text(&Value::Integer(13)), "warn");
        // No reading: the grid's own rendering, never a guess.
        assert_eq!(severity_cell_text(&Value::Integer(99)), "99");
        assert_eq!(severity_cell_text(&Value::String("gold".into())), "gold");
    }
}

#[cfg(test)]
mod placeholder_tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::tui::tests::test_app;

    /// The placeholder centres its help block on a 33-column line, and the
    /// centring did the shift-one-left with a plain subtraction: below 39
    /// terminal columns the halved width is already 0, so it underflowed.
    /// Debug builds panicked, release builds asked for `usize::MAX` spaces.
    ///
    /// 20x6 is well inside a real tmux split. A fresh app has no results,
    /// so this is the placeholder pane, and the whole UI is drawn rather
    /// than the one function: a narrow terminal narrows every pane at once.
    #[test]
    fn placeholder_renders_in_a_terminal_too_narrow_to_centre_in() {
        for (width, height) in [(20, 6), (1, 1), (38, 10), (39, 10)] {
            let mut app = test_app();
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|f| crate::tui::ui::render(&mut app, f))
                .unwrap_or_else(|e| panic!("{width}x{height} must render: {e}"));
        }
    }
}
