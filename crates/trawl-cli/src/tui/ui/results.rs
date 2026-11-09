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
use trawl_api::display::{
    downsample, extract_series_labelled, is_timechart_result, value_to_string,
};
use trawl_api::stream::{SnapshotCap, SnapshotCapKind};
use trawl_api::value::ColumnType;
use trawl_api::zone::{
    DisplayZone, TIMESTAMP_INFINITY, TIMESTAMP_NEG_INFINITY, present_cell, wire_micros,
};
use trawl_core::sanitize::sanitize_display_text;
use trawl_core::text_layout::{caret_layout, cells, fit_cells, locate_span};
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

/// A results title for a result whose live aggregate dropped input past a
/// cap (ADR-0001): the mark leads, in the warning colour, so a narrow pane
/// cuts the row counts before it cuts the warning. Without a cap the title
/// is unchanged. The result is still a result, so nothing here reads as an
/// error.
fn capped_title(
    title: impl Into<Line<'static>>,
    capped: &[SnapshotCap],
    theme: &Theme,
) -> Line<'static> {
    let mut title = title.into();
    if !capped.is_empty() {
        let caps: Vec<String> = capped
            .iter()
            .map(|c| format!("{} > {}", cap_label(c.cap), c.limit))
            .collect();
        let mark = format!(" partial: {} ", caps.join(", "));
        title.spans.insert(
            0,
            Span::styled(mark, Style::default().fg(theme.status_warning)),
        );
    }
    title
}

/// What a cap bounds, as the results title names it.
fn cap_label(cap: SnapshotCapKind) -> &'static str {
    match cap {
        SnapshotCapKind::Groups => "groups",
        SnapshotCapKind::DistinctValues => "distinct values",
        SnapshotCapKind::ExactValues => "exact values",
    }
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
        ref query,
    } = tab.status
    {
        render_error_display(
            app,
            frame,
            results_area,
            tab,
            message,
            details,
            query.as_deref(),
        );
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
    let all_widths = compute_column_widths(
        result,
        v_scroll,
        max_visible_rows,
        available_width,
        config,
        app.display_zone,
    );
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
            let cell = Cell::from(sanitize_display_text(&result.columns[col_idx].name));
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
                    let ty = &result.columns[col_idx].ty;
                    // A timestamp reads by its type in the display zone,
                    // whatever its column is called. Otherwise a severity
                    // cell shows its OTel token, so `17` reads `error`, the
                    // same vocabulary that filters it (ADR-0013 §6).
                    let text = match trawl_api::zone::present_cell(ty, value, app.display_zone) {
                        Some(text) => text,
                        None if trawl_core::severity::renders_as_severity(
                            &result.columns[col_idx].name,
                            &response.severity_columns,
                        ) =>
                        {
                            severity_cell_text(value)
                        }
                        None => cell_text(ty, value, app.display_zone),
                    };
                    let cell = Cell::from(fit_cells(&text, width));
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
    // A running query leaves the last result drawn until its answer
    // arrives, so the cap mark covers every arm, "Running..." included.
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
    let title = capped_title(title, &tab.live_capped, theme);

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
        .title(capped_title(
            " Results ",
            &app.active_tab().live_capped,
            theme,
        ))
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

    // The charts fall back to this placeholder when a result has no
    // series, and a live aggregate that dropped input can leave exactly
    // that, so the cap mark rides here too.
    let tab = app.active_tab();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(capped_title(
            pane_title(&tab.status, theme),
            &tab.live_capped,
            theme,
        ))
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
/// Shows the error message in red, then each detail: the line of `query`
/// its span starts on, the span underlined and a caret line below it,
/// then the detail's message and hint. `query` is the text the failed run
/// sent, which the spans index, never the editor's current draft.
///
/// Spans are UTF-8 byte offsets; [`locate_span`] resolves them and
/// [`caret_layout`] places the carets by display cells. A span that cannot
/// index `query` (reversed, past the end, or inside a character), or an
/// error with no sent query, shows the message and hint without an
/// excerpt; a span is never clamped. Every piece of text drawn here is
/// sanitized: it can hold anything the user typed or the server said.
#[allow(clippy::too_many_arguments)]
fn render_error_display(
    app: &App,
    frame: &mut Frame<'_>,
    area: Rect,
    tab: &crate::tui::state::Tab,
    message: &str,
    details: &[trawl_client::ErrorDetail],
    query: Option<&str>,
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

    let muted = Style::default().fg(theme.text_muted);
    let mut lines: Vec<Line<'_>> = Vec::new();

    lines.push(Line::from(Span::styled(
        format!("error: {}", sanitize_display_text(message)),
        Style::default()
            .fg(theme.status_error)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));

    // A validation error's one detail has no span and repeats the message
    // above, so a list with no span renders the message alone.
    if !details.iter().any(|d| d.span.is_some()) {
        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, inner);
        return;
    }

    for detail in details {
        let located = query.and_then(|query| {
            let span = detail.span.as_ref()?;
            let at = locate_span(query, span.start, span.end)?;
            Some(caret_layout(query, &at))
        });
        if let Some(layout) = located {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(layout.before, Style::default().fg(theme.text_primary)),
                Span::styled(
                    layout.covered,
                    Style::default()
                        .fg(theme.status_error)
                        .add_modifier(Modifier::UNDERLINED),
                ),
                Span::styled(layout.after, Style::default().fg(theme.text_primary)),
            ]));
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::raw(" ".repeat(layout.pad)),
                Span::styled(
                    "^".repeat(layout.carets),
                    Style::default().fg(theme.status_error),
                ),
            ]));
        }

        lines.push(Line::from(Span::styled(
            format!("  {}", sanitize_display_text(&detail.message)),
            muted,
        )));
        if let Some(label) = &detail.label {
            lines.push(Line::from(Span::styled(
                format!("  while parsing: {}", sanitize_display_text(label)),
                muted,
            )));
        }
        if let Some(hint) = &detail.hint {
            lines.push(Line::from(Span::styled(
                format!("  hint: {}", sanitize_display_text(hint)),
                muted,
            )));
        }
        lines.push(Line::from(""));
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

    let (series, total_series) = chart_series(result, app.display_zone);

    if series.is_empty() {
        render_placeholder(app, frame, area);
        return;
    }

    let time_info = extract_time_metadata(result, app.display_zone);

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
        .title(capped_title(title, &app.active_tab().live_capped, theme))
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

    let metric_name = sanitize_display_text(&result.columns[value_col].name);
    let label_name = sanitize_display_text(&result.columns[label_col].name);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(capped_title(
            format!(" {metric_name} by {label_name} "),
            &app.active_tab().live_capped,
            theme,
        ))
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
            let label = sanitize_display_text(&cell_text(
                &result.columns[label_col].ty,
                &row[label_col],
                app.display_zone,
            ));
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

    let (series, total_series) = chart_series(result, app.display_zone);

    if series.is_empty() {
        // Fallback to placeholder if extraction failed
        render_placeholder(app, frame, area);
        return;
    }

    // Extract time metadata for title
    let time_info = extract_time_metadata(result, app.display_zone);

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
            .title(capped_title(title, &app.active_tab().live_capped, theme))
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
    let display_label = fit_cells(label, w);

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
            Paragraph::new(display_label)
                .alignment(Alignment::Right)
                .style(Style::default().fg(series_color)),
            Rect {
                y: area.y + area.height / 2,
                height: 1,
                ..area
            },
        );
    } else {
        // Too short for y-axis labels — just show the series name
        frame.render_widget(
            Paragraph::new(display_label)
                .alignment(Alignment::Right)
                .style(Style::default().fg(series_color)),
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
        .title(capped_title(title, &app.active_tab().live_capped, theme))
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
/// Samples the rows from `v_scroll` on, every row the grid draws
/// (`visible_rows`) and at least `SAMPLE_ROWS`, taking the max display
/// width per column, clamped to `MIN_COL` and to each
/// column's equal share of `available_width` (itself capped at `MAX_COL`).
/// A column of instants may take the whole `available_width` instead: its
/// offset is often the only difference between two rows (a fall-back
/// night's two 01:30s), so cutting it would show one reading twice. An
/// instant's text varies in length (its fraction drops trailing zeros), so
/// a row the sample missed could be wider than its column: every drawn row
/// is sampled.
/// Widths are display cells of the text as the grid draws it: each value
/// is cut to its column's cap with [`fit_cells`] before it is measured, so
/// a value of any length costs no more than the cap to size.
/// Respects `width_override` from `ColumnConfig`; hidden columns get width 0.
fn compute_column_widths(
    result: &trawl_engine::value::QueryResult,
    v_scroll: usize,
    visible_rows: usize,
    available_width: usize,
    config: Option<&ColumnConfig>,
    zone: DisplayZone,
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
    let instant_col = available_width
        .saturating_sub(CELL_PADDING)
        .clamp(MIN_COL, MAX_COL)
        .max(max_col);

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

            let width_of = |text: &str, cap: usize| cells(&fit_cells(text, cap));
            let mut max_width = width_of(&col.name, max_col);
            let mut cap = max_col;

            for row in result
                .rows
                .iter()
                .skip(v_scroll)
                .take(SAMPLE_ROWS.max(visible_rows))
            {
                if let Some(value) = row.get(col_idx) {
                    let width = match present_cell(&col.ty, value, zone) {
                        Some(shown) => {
                            cap = instant_col;
                            width_of(&shown, instant_col)
                        }
                        None => width_of(&value_to_string(value), max_col),
                    };
                    max_width = max_width.max(width);
                }
            }

            #[allow(clippy::cast_possible_truncation)]
            let width = max_width.clamp(MIN_COL, cap) as u16;
            width
        })
        .collect()
}

/// Extract time metadata from timechart result.
///
/// Returns (`start_time`, `end_time`, `span_interval`) as formatted strings:
/// the first and last rows' instants shown in `zone`, offset included, and
/// the smallest gap between distinct instants. The chart plots rows in
/// response order, so the endpoint labels follow it too. The span reads
/// the UTC instants the rows carry, never display text, so a fall-back
/// night's two 02:30s stay an hour apart in any zone.
fn extract_time_metadata(
    result: &trawl_engine::value::QueryResult,
    zone: DisplayZone,
) -> Option<(String, String, String)> {
    if result.rows.is_empty() {
        return None;
    }

    // Find the _time column by name (UNION ALL BY NAME can reorder columns)
    let time_col = result.columns.iter().position(|c| c.name == "_time")?;
    let time_ty = &result.columns[time_col].ty;

    let mut instants: Vec<i64> = result
        .rows
        .iter()
        .filter_map(|row| match row.get(time_col)? {
            Value::String(wire) => wire_micros(wire),
            _ => None,
        })
        .collect();
    instants.sort_unstable();
    instants.dedup();
    let shown = |row: &[Value]| row.get(time_col).map(|cell| cell_text(time_ty, cell, zone));
    let (first, last) = (shown(result.rows.first()?)?, shown(result.rows.last()?)?);

    // Derive span from the minimum gap between distinct instants. Using the
    // minimum handles sparse data (empty buckets aren't emitted by
    // time_bucket) and group-by rows sharing the same _time. An infinity is
    // no bucket, so it bounds no gap.
    let span = if instants.len() >= 2 {
        let finite: Vec<i64> = instants
            .iter()
            .copied()
            .filter(|micros| !matches!(*micros, TIMESTAMP_INFINITY | TIMESTAMP_NEG_INFINITY))
            .collect();
        finite
            .windows(2)
            .filter_map(|pair| u64::try_from(i128::from(pair[1]) - i128::from(pair[0])).ok())
            .map(|micros| micros / 1_000_000)
            .filter(|&secs| secs > 0)
            .min()
            .map_or("?".to_owned(), format_duration)
    } else {
        "N/A".to_owned()
    };

    // Sanitized: a `_time` cell that is not a timestamp is whatever the
    // result holds. An instant keeps its whole text, offset included.
    Some((
        sanitize_display_text(&first),
        sanitize_display_text(&last),
        span,
    ))
}

/// A timechart's series as the TUI draws them: a `timestamp` group label
/// in `zone` through [`cell_text`], as the grid shows that column, and
/// every label sanitized. A label is a metric column's name or a grouped
/// value, so a sender can pick it, and it reaches titles, legends and axis
/// labels.
fn chart_series(
    result: &trawl_engine::value::QueryResult,
    zone: DisplayZone,
) -> (Vec<(String, Vec<u64>)>, usize) {
    let (series, total) = extract_series_labelled(result, |ty, value| cell_text(ty, value, zone));
    let series = series
        .into_iter()
        .map(|(label, values)| (sanitize_display_text(&label), values))
        .collect();
    (series, total)
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

/// A result cell as the TUI shows it: an instant in `zone` with its own
/// offset ([`present_cell`]), any other cell through [`value_to_string`].
/// The grid, its column widths, the detail popup and the search all read
/// this one text.
pub fn cell_text(ty: &ColumnType, value: &Value, zone: DisplayZone) -> String {
    present_cell(ty, value, zone).unwrap_or_else(|| value_to_string(value))
}

/// A `_severity` cell as the TUI shows it: the shared `OTel` token, or the
/// grid's ordinary rendering where the ladder has no reading for the value.
fn severity_cell_text(value: &trawl_engine::value::Value) -> String {
    crate::cli::severity_token(value).map_or_else(|| value_to_string(value), str::to_owned)
}

/// The timechart's span comes from the UTC instants: rows out of order and
/// a repeated bucket still give the smallest gap, and an infinity bounds no
/// gap. The endpoint labels are the first and last rows.
#[cfg(test)]
#[test]
fn timechart_metadata_reads_utc_instants() {
    use trawl_engine::value::{Column, QueryResult};

    let s = |t: &str| vec![Value::String(t.into()), Value::Integer(1)];
    let result = QueryResult {
        columns: vec![
            Column::new("_time", ColumnType::Timestamp),
            Column::new("count", ColumnType::Integer),
        ],
        rows: vec![
            s("2026-10-25T01:30:00.000000Z"),
            s("2026-10-25T00:30:00.000000Z"),
            s("2026-10-25T00:30:00.000000Z"),
            s("2026-10-25T03:30:00.000000Z"),
            s("infinity"),
        ],
    };
    let zone = "+02:00".parse::<DisplayZone>().unwrap();
    let (start, end, span) = extract_time_metadata(&result, zone).unwrap();
    assert_eq!(start, "2026-10-25 03:30:00 +02:00");
    assert_eq!(end, "infinity");
    assert_eq!(span, "1h");
}

/// The chart plots rows in response order, so its endpoint labels do too:
/// under `| sort -_time` the left label names the latest bucket, which is
/// the left-most point.
#[cfg(test)]
#[test]
fn timechart_labels_follow_response_order() {
    use trawl_engine::value::{Column, QueryResult};

    let s = |t: &str| vec![Value::String(t.into()), Value::Integer(1)];
    let result = QueryResult {
        columns: vec![
            Column::new("_time", ColumnType::Timestamp),
            Column::new("count", ColumnType::Integer),
        ],
        rows: vec![
            s("2026-10-25T01:30:00.000000Z"),
            s("2026-10-25T00:30:00.000000Z"),
        ],
    };
    let zone = "+02:00".parse::<DisplayZone>().unwrap();
    let (start, end, span) = extract_time_metadata(&result, zone).unwrap();
    assert_eq!(start, "2026-10-25 03:30:00 +02:00");
    assert_eq!(end, "2026-10-25 02:30:00 +02:00");
    assert_eq!(span, "1h");
}

/// A timechart grouped by a `timestamp` column names each series by its
/// instant in the display zone, as the grid shows that column, not by the
/// UTC wire text.
#[cfg(test)]
#[test]
fn timechart_timestamp_group_labels_use_the_display_zone() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use trawl_engine::value::{Column, QueryResult};

    let row = |time: &str, ingested: &str| {
        vec![
            Value::String(time.into()),
            Value::String(ingested.into()),
            Value::Integer(2),
        ]
    };
    let result = QueryResult {
        columns: vec![
            Column::new("_time", ColumnType::Timestamp),
            Column::new("_ingested", ColumnType::Timestamp),
            Column::new("count", ColumnType::Integer),
        ],
        rows: vec![
            row("2026-01-15T00:00:00.000000Z", "2026-01-15T04:30:00.000000Z"),
            row("2026-01-15T00:00:00.000000Z", "2026-01-15T05:30:00.000000Z"),
            row("2026-01-15T01:00:00.000000Z", "2026-01-15T04:30:00.000000Z"),
            row("2026-01-15T01:00:00.000000Z", "2026-01-15T05:30:00.000000Z"),
        ],
    };

    for view in [ChartView::LineChart, ChartView::Sparkline] {
        let mut app = crate::tui::tests::test_app();
        app.display_zone = "+05:30".parse().unwrap();
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

        let mut terminal = Terminal::new(TestBackend::new(160, 24)).expect("terminal");
        terminal
            .draw(|f| crate::tui::ui::render(&mut app, f))
            .expect("render");
        let screen = terminal.backend().to_string();

        assert!(!screen.contains("15T0"), "{view:?} wire label:\n{screen}");
        if view == ChartView::LineChart {
            for label in ["2026-01-15 10:00:00 +05:30", "2026-01-15 11:00:00 +05:30"] {
                assert!(
                    screen.contains(label),
                    "{view:?} {label} missing:\n{screen}"
                );
            }
        }
    }
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
    use trawl_engine::value::{Column, ColumnType, QueryResult};

    let result = QueryResult {
        columns: vec![
            Column::new("_time", ColumnType::Timestamp),
            Column::new("m0", ColumnType::Integer),
            Column::new("m1", ColumnType::Integer),
        ],
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
    let (series, _) = trawl_api::display::extract_series(&result);
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
            Column::new("service", trawl_engine::value::ColumnType::String),
            Column::new("first_request_id", trawl_engine::value::ColumnType::Integer),
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

/// AC 2 in the TUI: a response decoded from its wire text shows every
/// digit of 2^53+1, 2^63 and `u64::MAX` in the grid, and the bar chart
/// orders the three by magnitude. The TUI has no client-side row sort, so
/// the bar lengths are where it compares the numbers: a reader that went
/// through `f64` would print 9007199254740992 and could not tell the two
/// largest apart.
#[cfg(test)]
#[test]
fn big_integers_from_the_wire_render_exactly_and_in_order() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let response: trawl_client::QueryResponse = serde_json::from_str(
        r#"{
            "columns": [{"name": "host", "type": "string"},
                        {"name": "id", "type": "integer"}],
            "rows": [["a", "9007199254740993"],
                     ["b", "9223372036854775808"],
                     ["c", "18446744073709551615"]],
            "pagination": {"limit": 100, "offset": 0, "returned": 3, "total": 3}
        }"#,
    )
    .expect("the typed wire decodes");

    let draw = |view| {
        let mut app = crate::tui::tests::test_app();
        app.tab.result = Some(response.clone());
        app.tab.chart_view = view;
        let mut terminal = Terminal::new(TestBackend::new(120, 20)).expect("terminal");
        terminal
            .draw(|f| crate::tui::ui::render(&mut app, f))
            .expect("render");
        terminal.backend().to_string()
    };

    let grid = draw(ChartView::Table);
    for digits in [
        "9007199254740993",
        "9223372036854775808",
        "18446744073709551615",
    ] {
        assert!(grid.contains(digits), "{digits} missing from:\n{grid}");
    }
    assert!(!grid.contains("9007199254740992"), "{grid}");

    // One bar per host; its length is its share of the largest value.
    let bars = draw(ChartView::BarChart);
    let length = |host: &str| {
        bars.lines()
            .find(|line| line.contains(&format!("{host} ")) && line.contains("  "))
            .map_or_else(
                || panic!("no bar for {host}:\n{bars}"),
                |line| line.matches('\u{2588}').count(),
            )
    };
    let (a, b, c) = (length("a"), length("b"), length("c"));
    assert!(a < b && b < c, "bars out of order ({a}, {b}, {c}):\n{bars}");
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

#[cfg(test)]
mod error_pane_tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use trawl_core::text_layout::{SPAN_CASES, SpanCase, cells as cells_of};

    use crate::tui::state::TabStatus;
    use crate::tui::tests::test_app;

    /// Wide enough for every row of the span table: the pane clips a line
    /// past its width rather than wrapping it.
    const WIDTH: u16 = 60;
    const HEIGHT: u16 = 12;

    /// The error pane alone, drawn for one error whose one detail has
    /// `span` over `query`, as rows of text.
    fn pane(query: Option<&str>, span: Option<(usize, usize)>) -> Vec<String> {
        let mut app = test_app();
        app.tab.status = TabStatus::Error {
            message: "parse error: the error".to_owned(),
            details: vec![trawl_client::ErrorDetail {
                message: "the message".to_owned(),
                span: span.map(|(start, end)| trawl_client::ErrorSpan { start, end }),
                label: None,
                hint: Some("the hint".to_owned()),
            }],
            query: query.map(str::to_owned),
        };
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).expect("terminal");
        terminal
            .draw(|f| super::render(&app, f, f.area()))
            .expect("render");
        rows_of(terminal.backend().buffer())
    }

    /// `buffer` as rows of text. A cell hidden behind a double-width
    /// character is skipped, so each row reads as the terminal shows it.
    pub(super) fn rows_of(buffer: &ratatui::buffer::Buffer) -> Vec<String> {
        buffer
            .content
            .chunks(usize::from(buffer.area.width))
            .map(|cells| {
                let mut row = String::new();
                let mut hidden: usize = 0;
                for cell in cells {
                    if hidden == 0 {
                        row.push_str(cell.symbol());
                    }
                    hidden = hidden.max(cells_of(cell.symbol())).saturating_sub(1);
                }
                row
            })
            .collect()
    }

    /// The pane's inner rows: inside the border and its one-cell padding,
    /// trailing blanks dropped.
    fn inner(rows: &[String]) -> Vec<String> {
        rows[1..rows.len() - 1]
            .iter()
            .map(|row| {
                let inside = row
                    .strip_prefix("│ ")
                    .and_then(|r| r.strip_suffix(" │"))
                    .unwrap_or_else(|| panic!("a bordered row: {row:?}"));
                inside.trim_end().to_owned()
            })
            .collect()
    }

    /// What the pane shows for a detail with no excerpt.
    fn no_excerpt() -> Vec<String> {
        let mut rows: Vec<String> = [
            "error: parse error: the error",
            "",
            "  the message",
            "  hint: the hint",
        ]
        .map(str::to_owned)
        .to_vec();
        rows.resize(usize::from(HEIGHT) - 2, String::new());
        rows
    }

    fn case(name: &str) -> &'static SpanCase {
        SPAN_CASES
            .iter()
            .find(|case| case.name == name)
            .unwrap_or_else(|| panic!("no span case {name:?}"))
    }

    /// Every row of the shared span table, drawn through the real error
    /// pane: a row that locates shows its excerpt line and a caret line at
    /// the row's display cells; one that cannot index its text shows the
    /// message and hint with no excerpt and no caret.
    #[test]
    fn terminal_caret_layout_conformance_tui() {
        for case in SPAN_CASES {
            let rows = pane(Some(case.text), Some((case.start, case.end)));
            assert!(
                rows.iter().all(|row| !row.contains('\r')),
                "{}: {rows:#?}",
                case.name
            );
            let got = inner(&rows);
            let want = match case.expect {
                Some(expect) => {
                    let mut want: Vec<String> = vec![
                        "error: parse error: the error".to_owned(),
                        String::new(),
                        format!("  {}", expect.excerpt).trim_end().to_owned(),
                        format!("  {}{}", " ".repeat(expect.pad), "^".repeat(expect.carets)),
                        "  the message".to_owned(),
                        "  hint: the hint".to_owned(),
                    ];
                    want.resize(usize::from(HEIGHT) - 2, String::new());
                    want
                }
                None => no_excerpt(),
            };
            assert_eq!(got, want, "{}", case.name);
        }
    }

    /// An error raised before anything was sent has no query to quote: a
    /// span shows the message and hint alone.
    #[test]
    fn terminal_caret_layout_conformance_tui_without_a_query() {
        assert_eq!(inner(&pane(None, Some((0, 0)))), no_excerpt());
    }

    /// An empty sent query still gets its one caret for a valid `0..0`.
    #[test]
    fn terminal_caret_layout_conformance_tui_empty_query() {
        let rows = inner(&pane(Some(""), Some((0, 0))));
        assert_eq!(rows[2], "");
        assert_eq!(rows[3], "  ^");
        assert_eq!(rows[4], "  the message");
    }

    /// The message, label and hint can hold anything the server said, and
    /// a control or bidi character in them cannot rewrite the line.
    #[test]
    fn terminal_caret_layout_conformance_tui_sanitizes_messages() {
        let mut app = test_app();
        app.tab.status = TabStatus::Error {
            message: "bad\u{1b}[2J".to_owned(),
            details: vec![trawl_client::ErrorDetail {
                message: "a\u{202E}b".to_owned(),
                span: Some(trawl_client::ErrorSpan { start: 0, end: 1 }),
                label: Some("l\u{7}".to_owned()),
                hint: Some("try\r\nthis".to_owned()),
            }],
            query: Some("q".to_owned()),
        };
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).expect("terminal");
        terminal
            .draw(|f| super::render(&app, f, f.area()))
            .expect("render");
        let screen = terminal.backend().to_string();
        for drawn in [
            "error: bad\u{FFFD}[2J",
            "  a\u{FFFD}b",
            "  while parsing: l\u{FFFD}",
            "  hint: try\u{FFFD}\u{FFFD}this",
        ] {
            assert!(screen.contains(drawn), "{drawn:?} in:\n{screen}");
        }
    }

    /// Snapshots of representative rows, drawn whole with their border.
    fn snapshot_rows(name: &str) -> String {
        let case = case(name);
        pane(Some(case.text), Some((case.start, case.end))).join("\n")
    }

    /// The issue's query: the caret sits under the `(`, after two
    /// double-width characters.
    #[test]
    fn terminal_caret_layout_conformance_tui_cjk_snapshot() {
        insta::assert_snapshot!(snapshot_rows("cjk before the span"));
    }

    /// A CRLF query draws no `\r` and quotes the span's own line.
    #[test]
    fn terminal_caret_layout_conformance_tui_crlf_snapshot() {
        insta::assert_snapshot!(snapshot_rows("crlf query"));
    }

    /// A span running onto later lines marks only its start line.
    #[test]
    fn terminal_caret_layout_conformance_tui_multi_line_snapshot() {
        insta::assert_snapshot!(snapshot_rows("multi-line span clips to its start line"));
    }

    /// A span ending inside a character draws no excerpt and no caret.
    #[test]
    fn terminal_caret_layout_conformance_tui_invalid_span_snapshot() {
        insta::assert_snapshot!(snapshot_rows("end inside a character"));
    }
}

#[cfg(test)]
mod grid_tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use trawl_core::text_layout::{MARKER, cells};
    use trawl_engine::value::{Column, ColumnType, QueryResult, Value};

    use super::error_pane_tests::rows_of;
    use super::{DisplayZone, compute_column_widths};
    use crate::tui::state::TabStatus;
    use crate::tui::tests::test_app;

    fn result(rows: Vec<Vec<Value>>, columns: &[&str]) -> QueryResult {
        QueryResult {
            columns: columns
                .iter()
                .map(|name| Column::new(*name, ColumnType::String))
                .collect(),
            rows,
        }
    }

    /// A column is as wide as its widest value's display cells: six CJK
    /// characters take 12 cells and 18 bytes, ten accented letters built
    /// from combining marks take 10 cells and 30 bytes.
    #[test]
    fn grid_column_widths_measure_display_cells() {
        let result = result(
            vec![vec![
                Value::String("日本語日本語".into()),
                Value::String("e\u{301}".repeat(10)),
            ]],
            &["s", "t"],
        );
        assert_eq!(
            compute_column_widths(&result, 0, 1, 200, None, DisplayZone::Utc),
            [12, 10]
        );
    }

    /// A value far past the column's cap, CJK or a run of combining marks,
    /// is sized by its cut at the cap: 29 CJK characters and the marker
    /// fill 59 of the 60 cells, and one more character would not fit.
    #[test]
    fn grid_column_widths_stop_at_the_cap() {
        let result = result(
            vec![vec![
                Value::String("日".repeat(10_000)),
                Value::String(format!("x{}", "\u{301}".repeat(100_000))),
            ]],
            &["s", "t"],
        );
        let widths = compute_column_widths(&result, 0, 1, 200, None, DisplayZone::Utc);
        assert_eq!(widths[0], 59);
        assert_eq!(widths[1], 8);
    }

    /// A CJK value wider than its column is cut by display cells and
    /// marked as cut, and the column after it starts in the same place on
    /// every row.
    #[test]
    fn grid_cuts_wide_values_by_display_cells() {
        let mut app = test_app();
        let rows = vec![
            vec![
                Value::String("日".repeat(50)),
                Value::String("first".into()),
            ],
            vec![Value::String("web".into()), Value::String("second".into())],
        ];
        app.tab.result = Some(trawl_client::QueryResponse {
            execution: None,
            result: result(rows, &["service", "message"]),
            pagination: trawl_client::PaginationMeta {
                limit: 10_000,
                offset: 0,
                returned: 2,
                total: 2,
            },
            degraded_fields: Vec::new(),
            severity_columns: Vec::new(),
        });
        app.tab.status = TabStatus::Success { duration_ms: 1 };

        let mut terminal = Terminal::new(TestBackend::new(80, 16)).expect("terminal");
        terminal
            .draw(|f| crate::tui::ui::render(&mut app, f))
            .expect("render");
        let rows = rows_of(terminal.backend().buffer());
        let screen = rows.join("\n");

        let wide = rows
            .iter()
            .find(|row| row.contains('日'))
            .unwrap_or_else(|| panic!("a row with the CJK value:\n{screen}"));
        let narrow = rows
            .iter()
            .find(|row| row.contains("web"))
            .unwrap_or_else(|| panic!("a row with the short value:\n{screen}"));
        assert!(
            wide.contains(&format!("日{MARKER}")),
            "the cut is marked:\n{screen}"
        );
        assert!(
            wide.contains("first") && narrow.contains("second"),
            "{screen}"
        );

        // The cell where `message` starts, in display cells from the left.
        let column_of = |row: &str, value: &str| cells(&row[..row.find(value).expect("value")]);
        assert_eq!(
            column_of(wide, "first"),
            column_of(narrow, "second"),
            "the message column lines up:\n{screen}"
        );
        assert!(
            rows.iter().all(|row| cells(row) == 80),
            "every row fills the terminal exactly:\n{screen}"
        );
    }
}

#[cfg(test)]
mod series_label_tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::style::Color;
    use trawl_core::text_layout::{MARKER, cells};

    use super::error_pane_tests::rows_of;
    use super::render_series_label;
    use crate::tui::tests::test_app;

    /// A grouped value reaches the sparkline label from `extract_series`,
    /// so a sender picks it. Whether it fits or is cut, by a multibyte
    /// character past the label's last byte or into a zero-width area, it
    /// draws right-aligned within the area instead of panicking.
    #[test]
    fn series_labels_fit_by_display_cells() {
        let theme = test_app().theme;
        for (label, width, expect) in [
            ("aaaaaaaaaaaaaaaaéz", 18, "aaaaaaaaaaaaaaaaéz"),
            ("aaaaaaaaaaaaaaaaaéz", 18, "aaaaaaaaaaaaaaaaa…"),
            ("日本語日本語日", 18, "日本語日本語日"),
            ("日本語日本語日本語日", 18, "日本語日本語日本…"),
            ("日本語日本語日", 0, ""),
        ] {
            for height in [1, 3] {
                let mut terminal = Terminal::new(TestBackend::new(20, 3)).expect("terminal");
                terminal
                    .draw(|f| {
                        let area = Rect::new(0, 0, width, height);
                        render_series_label(f, &theme, area, label, 0, 9, Color::Reset);
                    })
                    .expect("render");
                let rows = rows_of(terminal.backend().buffer());
                let screen = rows.join("\n");
                let row = &rows[usize::from(height / 2)];
                assert_eq!(
                    cells(row.trim_end()),
                    usize::from(width),
                    "{label:?} at {width}x{height} ends on the area's edge:\n{screen}"
                );
                assert_eq!(
                    row.trim(),
                    expect,
                    "{label:?} at {width}x{height}:\n{screen}"
                );
                assert!(
                    expect == label || expect.is_empty() || expect.ends_with(MARKER),
                    "a cut is marked"
                );
            }
        }
    }
}

#[cfg(test)]
mod sender_text_tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use trawl_engine::value::{Column, ColumnType, QueryResult, Value};

    use crate::tui::App;
    use crate::tui::state::{ChartView, TabStatus};
    use crate::tui::tests::test_app;

    /// A bidi override a sender can put in a field name or a grouped value.
    const BIDI: &str = "a\u{202E}b";

    fn with_result(result: QueryResult, view: ChartView) -> App {
        let mut app = test_app();
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
        app.tab.status = TabStatus::Success { duration_ms: 1 };
        app.tab.chart_view = view;
        app
    }

    /// The results pane drawn alone, as the terminal's text.
    fn pane(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).expect("terminal");
        terminal
            .draw(|f| super::render(app, f, f.area()))
            .expect("render");
        terminal.backend().to_string()
    }

    fn assert_sanitized(surface: &str, screen: &str) {
        assert!(
            !screen.contains('\u{202E}'),
            "{surface} draws the override:\n{screen}"
        );
        assert!(
            screen.contains('\u{FFFD}'),
            "{surface} draws no replacement:\n{screen}"
        );
    }

    fn time(i: i64) -> Value {
        Value::String(format!("2026-01-01 00:0{i}:00"))
    }

    /// `timechart count by host`: the hosts are the series labels.
    fn grouped_timechart() -> QueryResult {
        let rows = (0..3_i64)
            .flat_map(|i| {
                [BIDI, "web"]
                    .map(|host| vec![time(i), Value::String(host.into()), Value::Integer(i)])
            })
            .collect();
        QueryResult {
            columns: vec![
                Column::new("_time", ColumnType::Timestamp),
                Column::new("host", ColumnType::String),
                Column::new("count", ColumnType::Integer),
            ],
            rows,
        }
    }

    /// One series, named for its metric column: the label is in the title.
    fn single_timechart() -> QueryResult {
        QueryResult {
            columns: vec![
                Column::new("_time", ColumnType::Timestamp),
                Column::new(BIDI, ColumnType::Integer),
            ],
            rows: (0..3_i64)
                .map(|i| vec![time(i), Value::Integer(i)])
                .collect(),
        }
    }

    /// The line chart's legend.
    #[test]
    fn sender_text_is_sanitized_in_the_line_chart_legend() {
        let screen = pane(&with_result(grouped_timechart(), ChartView::LineChart));
        assert_sanitized("line chart legend", &screen);
    }

    #[test]
    fn sender_text_is_sanitized_in_the_line_chart_title() {
        let screen = pane(&with_result(single_timechart(), ChartView::LineChart));
        assert_sanitized("line chart title", &screen);
    }

    /// The stacked sparklines' labels.
    #[test]
    fn sender_text_is_sanitized_in_sparkline_labels() {
        let screen = pane(&with_result(grouped_timechart(), ChartView::Sparkline));
        assert_sanitized("sparkline labels", &screen);
    }

    #[test]
    fn sender_text_is_sanitized_in_the_sparkline_title() {
        let screen = pane(&with_result(single_timechart(), ChartView::Sparkline));
        assert_sanitized("sparkline title", &screen);
    }

    /// The time axis and the title quote the first and last `_time`.
    #[test]
    fn sender_text_is_sanitized_in_chart_time_labels() {
        let mut result = grouped_timechart();
        for row in &mut result.rows {
            row[0] = Value::String(BIDI.into());
        }
        for view in [ChartView::LineChart, ChartView::Sparkline] {
            let screen = pane(&with_result(result.clone(), view));
            assert_sanitized(&format!("{view:?} time labels"), &screen);
        }
    }

    #[test]
    fn sender_text_is_sanitized_in_the_bar_chart() {
        let result = QueryResult {
            columns: vec![
                Column::new("host", ColumnType::String),
                Column::new(BIDI, ColumnType::Integer),
            ],
            rows: vec![
                vec![Value::String(BIDI.into()), Value::Integer(3)],
                vec![Value::String("web".into()), Value::Integer(5)],
            ],
        };
        let screen = pane(&with_result(result, ChartView::BarChart));
        assert_sanitized("bar chart", &screen);
        assert_eq!(
            screen.matches('\u{FFFD}').count(),
            2,
            "the title and the label:\n{screen}"
        );
    }

    #[test]
    fn sender_text_is_sanitized_in_result_headers() {
        let result = QueryResult {
            columns: vec![Column::new(BIDI, ColumnType::String)],
            rows: vec![vec![Value::String("web".into())]],
        };
        let screen = pane(&with_result(result, ChartView::Table));
        assert_sanitized("result header", &screen);
    }

    #[test]
    fn sender_text_is_sanitized_in_the_status_bar() {
        let mut app = test_app();
        app.main_tab = crate::tui::state::MainTab::History;
        app.tab.status = TabStatus::Error {
            message: format!("unknown field {BIDI}"),
            details: Vec::new(),
            query: None,
        };
        let mut terminal = Terminal::new(TestBackend::new(100, 1)).expect("terminal");
        terminal
            .draw(|f| super::super::status::render(&app, f, f.area()))
            .expect("render");
        let screen = terminal.backend().to_string();
        assert!(screen.contains("error: unknown field"), "{screen}");
        assert_sanitized("status bar", &screen);
    }
}
