// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Full-width panel content for non-Query tabs (History, Schema, Saved).

use std::collections::HashSet;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState,
};

use crate::tui::App;
use crate::tui::state::{Focus, LoadedRun, MainTab, SavedFocus, SchemaBrowser};
use crate::tui::theme::Theme;
use trawl_api::zone::{DisplayZone, Presented, present, present_human};
use trawl_core::sanitize::sanitize_display_text;
use trawl_core::text_layout::{cells, fit_cells, pad_cells};

/// Render panel content based on the active `MainTab`.
pub fn render(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let theme = &app.theme;

    let border_style = if app.focus == Focus::Panel {
        Style::default().fg(theme.border_focused)
    } else {
        Style::default().fg(theme.border_unfocused)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .style(Style::default().bg(theme.surface));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    match app.main_tab {
        MainTab::Schema => {
            if let Some(ref schema) = app.panel.schema {
                // Horizontal split: tree (55%) | detail pane (45%).
                let [tree_col, detail_col] =
                    Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
                        .areas(inner);

                // Left side: filter bar + tree.
                let [filter_area, tree_area] =
                    Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(tree_col);

                render_filter_bar(
                    &schema.filter,
                    schema.filter_active,
                    theme,
                    frame,
                    filter_area,
                );
                render_schema_tree(schema, theme, frame, tree_area);

                // Right side: detail pane (1-col left padding clears the scrollbar).
                let detail_inset = Rect {
                    x: detail_col.x + 1,
                    width: detail_col.width.saturating_sub(1),
                    ..detail_col
                };
                super::schema::render_detail_pane(schema, frame, detail_inset, theme);
            } else {
                let paragraph = Paragraph::new("loading schema...")
                    .style(Style::default().fg(theme.text_muted));
                frame.render_widget(paragraph, inner);
            }
        }
        MainTab::History => {
            if let Some(ref history) = app.history_cache {
                if history.entries.is_empty() {
                    let paragraph = Paragraph::new("no history yet")
                        .style(Style::default().fg(theme.text_muted));
                    frame.render_widget(paragraph, inner);
                } else {
                    // Horizontal split: list (55%) | detail pane (45%).
                    let [list_col, detail_col] = Layout::horizontal([
                        Constraint::Percentage(55),
                        Constraint::Percentage(45),
                    ])
                    .areas(inner);

                    render_history_list(app, theme, frame, list_col);
                    super::history::render_detail_pane(app, frame, detail_col);
                }
            } else {
                let paragraph = Paragraph::new("loading history...")
                    .style(Style::default().fg(theme.text_muted));
                frame.render_widget(paragraph, inner);
            }
        }
        MainTab::Saved => render_saved_panel(app, theme, frame, inner),
        MainTab::Query | MainTab::Dashboard => {} // Use their own layout.
    }
}

/// Render the filter input bar for schema search.
fn render_filter_bar(filter: &str, active: bool, theme: &Theme, frame: &mut Frame<'_>, area: Rect) {
    let style = if active {
        Style::default().fg(theme.text_accent)
    } else {
        Style::default().fg(theme.text_muted)
    };

    let text = if filter.is_empty() && !active {
        "/ filter".to_owned()
    } else if filter.is_empty() {
        "/ \u{2588}".to_owned()
    } else if active {
        format!("/ {}\u{2588}", sanitize_display_text(filter))
    } else {
        format!("/ {}", sanitize_display_text(filter))
    };

    let paragraph = Paragraph::new(Line::from(Span::styled(text, style)));
    frame.render_widget(paragraph, area);
}

// ---------------------------------------------------------------------------
// Schema tree view
// ---------------------------------------------------------------------------

/// A node in the flattened schema tree (computed at render time).
#[derive(Debug)]
enum TreeNode<'a> {
    /// Common fields section header.
    CommonHeader { field_count: usize },
    /// A common field (present across many services).
    CommonField { name: &'a str, data_type: &'a str },
    /// A service row.
    Service {
        name: &'a str,
        expanded: bool,
        field_count: usize,
    },
    /// A field within a specific service (unique to that service).
    ServiceField {
        name: &'a str,
        data_type: &'a str,
        /// Non-null coverage display string (e.g. "99%", "< 1%", "0%").
        coverage: Option<(String, Color)>,
    },
}

/// Flatten the schema tree into a linear list for rendering.
fn flatten_tree<'a>(schema: &'a SchemaBrowser, theme: &Theme) -> Vec<TreeNode<'a>> {
    let filter = schema.filter.to_lowercase();
    let common_names: HashSet<&str> = schema
        .common_fields
        .iter()
        .map(|f| f.name.as_str())
        .collect();

    let mut nodes = Vec::new();

    // Common fields section.
    if !schema.common_fields.is_empty() {
        let visible_common: Vec<_> = schema
            .common_fields
            .iter()
            .filter(|f| filter.is_empty() || f.name.to_lowercase().contains(&filter))
            .collect();

        if !visible_common.is_empty() || filter.is_empty() {
            nodes.push(TreeNode::CommonHeader {
                field_count: visible_common.len(),
            });
            for f in &visible_common {
                nodes.push(TreeNode::CommonField {
                    name: &f.name,
                    data_type: &f.data_type,
                });
            }
        }
    }

    // Per-service rows.
    for svc in &schema.services {
        let unique_fields: Vec<_> = svc
            .columns
            .iter()
            .filter(|c| !common_names.contains(c.name.as_str()))
            .collect();

        if !filter.is_empty() {
            let svc_matches = svc.name.to_lowercase().contains(&filter);
            let fields_match = unique_fields
                .iter()
                .any(|c| c.name.to_lowercase().contains(&filter));
            if !svc_matches && !fields_match {
                continue;
            }
        }

        let expanded = schema.expanded.contains(&svc.name);
        nodes.push(TreeNode::Service {
            name: &svc.name,
            expanded,
            field_count: unique_fields.len(),
        });

        if expanded {
            for col in &unique_fields {
                let coverage = if col.total_count > 0 {
                    let non_null = col.total_count - col.null_count;
                    let pct_100 = (non_null * 100) / col.total_count;
                    if non_null == 0 {
                        Some(("  0%".to_owned(), theme.status_error))
                    } else if pct_100 == 0 {
                        // Non-zero but rounds to 0% → show "< 1%"
                        Some(("< 1%".to_owned(), theme.status_error))
                    } else {
                        #[allow(clippy::cast_possible_truncation)]
                        let pct = pct_100.min(100) as u8;
                        let color = if pct >= 80 {
                            theme.status_success
                        } else if pct >= 50 {
                            theme.status_warning
                        } else {
                            theme.status_error
                        };
                        Some((format!("{pct:>3}%"), color))
                    }
                } else {
                    None
                };
                nodes.push(TreeNode::ServiceField {
                    name: &col.name,
                    data_type: &col.data_type,
                    coverage,
                });
            }
        }
    }
    nodes
}

/// Color for a data type badge.
fn type_color(data_type: &str, theme: &Theme) -> Color {
    let dt = data_type.to_uppercase();
    if dt.starts_with("VARCHAR") || dt.starts_with("TEXT") || dt.starts_with("STRING") {
        theme.status_success
    } else if dt.starts_with("BIGINT")
        || dt.starts_with("INTEGER")
        || dt.starts_with("INT")
        || dt.starts_with("DOUBLE")
        || dt.starts_with("FLOAT")
        || dt.starts_with("HUGEINT")
        || dt.starts_with("SMALLINT")
        || dt.starts_with("TINYINT")
        || dt.starts_with("UBIGINT")
        || dt.starts_with("UINTEGER")
        || dt.starts_with("USMALLINT")
        || dt.starts_with("UTINYINT")
    {
        theme.status_warning
    } else if dt.starts_with("TIMESTAMP") || dt.starts_with("DATE") || dt.starts_with("TIME") {
        theme.text_accent
    } else if dt.starts_with("BOOLEAN") || dt.starts_with("BOOL") {
        theme.status_info
    } else {
        theme.text_primary
    }
}

/// Render the schema tree view.
#[allow(clippy::too_many_lines)]
fn render_schema_tree(schema: &SchemaBrowser, theme: &Theme, frame: &mut Frame<'_>, area: Rect) {
    let nodes = flatten_tree(schema, theme);
    let selected = schema.selected;

    if nodes.is_empty() {
        let msg = if schema.services.is_empty() {
            "no services"
        } else {
            "no matches"
        };
        let paragraph = Paragraph::new(msg).style(Style::default().fg(theme.text_muted));
        frame.render_widget(paragraph, area);
        return;
    }

    let total = nodes.len();
    #[allow(clippy::cast_possible_truncation)]
    let visible_height = area.height as usize;
    let needs_scrollbar = total > visible_height;

    // Shrink list area when scrollbar is visible to avoid text overlap.
    let list_area = if needs_scrollbar {
        Rect {
            width: area.width.saturating_sub(2), // 1 gap + 1 scrollbar track
            ..area
        }
    } else {
        area
    };

    let items: Vec<ListItem<'_>> = nodes
        .iter()
        .map(|node| ListItem::new(tree_line(node, theme, usize::from(list_area.width))))
        .collect();
    let list = List::new(items).highlight_style(
        Style::default()
            .bg(theme.surface_highlight)
            .add_modifier(Modifier::BOLD),
    );

    let offset = compute_center_offset(selected, visible_height, total);
    let mut state = ListState::default().with_offset(offset);
    state.select(Some(selected));
    frame.render_stateful_widget(list, list_area, &mut state);

    if needs_scrollbar {
        let mut sb_state = ScrollbarState::new(total).position(offset);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("\u{2191}"))
            .end_symbol(Some("\u{2193}"));
        frame.render_stateful_widget(scrollbar, area, &mut sb_state);
    }
}

/// One schema tree row, fitted to `width` display cells.
///
/// Names come from senders, so each is cut by display cells after the
/// row's fixed parts (indent, arrow, type and coverage columns, count) are
/// reserved. A width too narrow for the fixed parts leaves the name empty.
fn tree_line(node: &TreeNode<'_>, theme: &Theme, width: usize) -> Line<'static> {
    match node {
        TreeNode::CommonHeader { field_count } => Line::from(Span::styled(
            format!("\u{2605} common ({field_count})"),
            Style::default()
                .fg(theme.text_muted)
                .add_modifier(Modifier::BOLD),
        )),
        TreeNode::CommonField { name, data_type } => {
            // Layout: "  " + name + " " + type.
            let type_str = abbreviate_type(data_type);
            let tc = type_color(data_type, theme);
            let max_name = width.saturating_sub(2 + 1 + cells(&type_str));
            Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    fit_cells(name, max_name),
                    Style::default().fg(theme.text_accent),
                ),
                Span::raw(" "),
                Span::styled(type_str, Style::default().fg(tc)),
            ])
        }
        TreeNode::ServiceField {
            name,
            data_type,
            coverage,
        } => {
            let type_str = abbreviate_type(data_type);
            let tc = type_color(data_type, theme);

            // Column layout: "   " + name (padded) + " " + type (4) + " " + coverage (4) + " "
            // A fallback type label wider than its column takes its cells
            // from the name.
            let indent = 3usize;
            let type_width = cells(&type_str).max(4);
            let cov_width = 4usize;
            let trailing = 1usize; // padding before detail pane border
            let overhead = indent + 1 + type_width + 1 + cov_width + trailing;
            let max_name = width.saturating_sub(overhead);
            let type_cell = pad_cells(&type_str, type_width);

            let mut spans = vec![
                Span::raw(" ".repeat(indent)),
                Span::styled(
                    pad_cells(name, max_name),
                    Style::default().fg(theme.text_accent),
                ),
                Span::raw(" "),
                Span::styled(type_cell, Style::default().fg(tc)),
            ];
            if let Some((ref label, color)) = *coverage {
                spans.push(Span::raw(" "));
                spans.push(Span::styled(label.clone(), Style::default().fg(color)));
            }
            Line::from(spans)
        }
        TreeNode::Service {
            name,
            expanded,
            field_count,
        } => {
            let arrow = if *expanded { "\u{25be} " } else { "\u{25b8} " };
            let count_str = if *field_count > 0 {
                format!(" ({field_count})")
            } else {
                String::new()
            };
            let max_name = width.saturating_sub(cells(arrow) + cells(&count_str));
            Line::from(vec![
                Span::styled(arrow, Style::default().fg(theme.text_muted)),
                Span::styled(
                    fit_cells(name, max_name),
                    Style::default()
                        .fg(theme.text_primary)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(count_str, Style::default().fg(theme.text_muted)),
            ])
        }
    }
}

/// Compute scroll offset for center-locked scrolling.
///
/// The selected item stays at the vertical midpoint of the visible area,
/// with context visible above and below.
fn compute_center_offset(selected: usize, visible_height: usize, total: usize) -> usize {
    if total <= visible_height {
        return 0;
    }
    let max_offset = total.saturating_sub(visible_height);
    let ideal = selected.saturating_sub(visible_height / 2);
    ideal.min(max_offset)
}

/// Abbreviate a `DuckDB` type name for compact display.
fn abbreviate_type(data_type: &str) -> String {
    let dt = data_type.to_uppercase();
    if dt.starts_with("VARCHAR") {
        "STR".to_owned()
    } else if dt.starts_with("BIGINT") || dt.starts_with("HUGEINT") {
        "I64".to_owned()
    } else if dt.starts_with("INTEGER") || dt.starts_with("INT") {
        "I32".to_owned()
    } else if dt.starts_with("DOUBLE") || dt.starts_with("FLOAT") {
        "F64".to_owned()
    } else if dt.starts_with("TIMESTAMP") {
        "TS".to_owned()
    } else if dt.starts_with("BOOLEAN") || dt.starts_with("BOOL") {
        "BOOL".to_owned()
    } else if dt.starts_with("DATE") {
        "DATE".to_owned()
    } else {
        fit_cells(data_type, 6)
    }
}

// ---------------------------------------------------------------------------
// History list
// ---------------------------------------------------------------------------

/// Render history entries in the left pane (relative time + truncated query).
fn render_history_list(app: &App, theme: &Theme, frame: &mut Frame<'_>, area: Rect) {
    let selected = app.panel.history_selected;

    // Caller handles None / empty — we always have entries here.
    let history = app.history_cache.as_ref().expect("history_cache populated");

    let items: Vec<ListItem<'_>> = history
        .entries
        .iter()
        .map(|entry| ListItem::new(history_line(entry, theme, usize::from(area.width))))
        .collect();

    let total = items.len();
    let list = List::new(items).highlight_style(
        Style::default()
            .bg(theme.surface_highlight)
            .add_modifier(Modifier::BOLD),
    );

    #[allow(clippy::cast_possible_truncation)]
    let visible_height = area.height as usize;
    let offset = compute_center_offset(selected, visible_height, total);
    let mut state = ListState::default().with_offset(offset);
    state.select(Some(selected));
    frame.render_stateful_widget(list, area, &mut state);

    if total > visible_height {
        let mut sb_state = ScrollbarState::new(total).position(offset);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("\u{2191}"))
            .end_symbol(Some("\u{2193}"));
        frame.render_stateful_widget(
            scrollbar,
            area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut sb_state,
        );
    }
}

/// One history row: `"{4-char time}  {query…}  "`, the query flattened to
/// one line and cut to what the time, gap and trailing room leave of `width`.
/// The trailing room also clears the scrollbar drawn over the last column.
fn history_line(
    entry: &trawl_api::HistoryEntryResponse,
    theme: &Theme,
    width: usize,
) -> Line<'static> {
    const TIME_WIDTH: usize = 4;
    const GAP: usize = 2;
    const TRAILING: usize = 2; // padding before detail pane

    let max_query = width.saturating_sub(TIME_WIDTH + GAP + TRAILING);
    let time_str = format_relative_time(&entry.executed_at);
    let query = entry.query.replace('\n', " ");
    let display = format!("{}  ", fit_cells(&query, max_query));
    Line::from(vec![
        Span::styled(time_str, Style::default().fg(theme.text_muted)),
        Span::raw("  "),
        Span::styled(display, Style::default().fg(theme.text_primary)),
    ])
}

/// Format an ISO 8601 timestamp as a compact relative time from now.
///
/// Returns a 4-char right-justified string like `" 30m"`, `"  4h"`, `"  2d"`.
fn format_relative_time(iso_timestamp: &str) -> String {
    use chrono::{DateTime, Utc};

    let Ok(then) = iso_timestamp.parse::<DateTime<Utc>>() else {
        return "????".to_owned();
    };

    let secs = Utc::now().signed_duration_since(then).num_seconds().max(0);

    let label = if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else if secs < 604_800 {
        format!("{}d", secs / 86400)
    } else {
        format!("{}w", secs / 604_800)
    };

    format!("{label:>4}")
}

/// Render a report run's covered window as `[start .. end)` in `zone`.
///
/// Bounds are half-open, so the closing bracket is deliberate. Each bound is
/// a [`format_instant`], with its own offset, seconds and any fractional
/// part, since schedule edits can re-anchor a boundary between minutes. When
/// both bounds converted and show the same date, the end drops its date,
/// which is the common case for a tiled window; its offset stays, so nothing
/// is hidden.
fn format_run_window(start: &str, end: &str, zone: DisplayZone) -> String {
    let from = format_instant(start, zone);
    let to = format_instant(end, zone);
    let to = match (
        converted_date(start, &from, zone),
        converted_date(end, &to, zone),
    ) {
        (Some((from_date, _)), Some((to_date, to_rest))) if from_date == to_date => to_rest,
        _ => &to,
    };
    format!("[{from} .. {to})")
}

/// `shown`, the [`format_instant`] reading of `wire`, split into its date and
/// the rest, or `None` unless `zone` converted `wire`: text passed through,
/// an infinity and an instant the zone could not place keep their whole
/// reading.
fn converted_date<'a>(wire: &str, shown: &'a str, zone: DisplayZone) -> Option<(&'a str, &'a str)> {
    matches!(present(wire, zone), Some(Presented::Converted(_)))
        .then(|| shown.split_once(' '))
        .flatten()
}

/// A wire timestamp in `zone`, with its offset, seconds and any fraction.
/// Text that is not a wire timestamp shows as sent, sanitized, rather than
/// guessed at, so an unexpected wire value is visible and cannot drive the
/// terminal.
fn format_instant(wire: &str, zone: DisplayZone) -> String {
    present_human(wire, zone).unwrap_or_else(|| sanitize_display_text(wire))
}

/// The schedule's window mode as a badge suffix for the saved-query list:
/// `[1h]` in query mode, `[1h since_last]` or `[1h window 2h]` when the
/// schedule owns the report window (ADR-0018 ruling 6).
fn format_schedule_badge(sched: &trawl_api::ScheduleResponse) -> String {
    match sched.window.as_deref() {
        None => format!(" [{}]", sched.interval),
        Some("since_last") => format!(" [{} since_last]", sched.interval),
        Some(window) => format!(" [{} window {window}]", sched.interval),
    }
}

/// The schedule's report-window detail line, or `None` in query mode, where
/// the saved DSL carries its own time bounds and the scheduler owns nothing.
///
/// A zero lag is left out: it is the default and printing `lag 0s` would spend
/// a line saying nothing. `covered through` is the `since_last` watermark, so
/// it is absent for a fixed window and for a schedule that has not yet had a
/// successful run; when present it reads in `zone`.
fn format_schedule_window(
    sched: &trawl_api::ScheduleResponse,
    zone: DisplayZone,
) -> Option<String> {
    let window = sched.window.as_deref()?;
    let mut parts = vec![format!("window {window}")];

    if let Some(lag) = sched.lag.as_deref()
        && sched.lag_secs != Some(0)
    {
        parts.push(format!("lag {lag}"));
    }

    if let Some(covered) = sched.covered_through.as_deref() {
        parts.push(format!("covered through {}", format_instant(covered, zone)));
    }

    Some(parts.join(", "))
}

// ---------------------------------------------------------------------------
// Saved queries list
// ---------------------------------------------------------------------------

/// Render the saved tab as a two-pane layout: list (left) + detail/runs (right).
#[allow(clippy::too_many_lines)]
fn render_saved_panel(app: &App, theme: &Theme, frame: &mut Frame<'_>, area: Rect) {
    let Some(ref saved) = app.saved_cache else {
        let paragraph =
            Paragraph::new("loading saved...").style(Style::default().fg(theme.text_muted));
        frame.render_widget(paragraph, area);
        return;
    };

    if saved.queries.is_empty() {
        let paragraph =
            Paragraph::new("no saved queries").style(Style::default().fg(theme.text_muted));
        frame.render_widget(paragraph, area);
        return;
    }

    // Two-pane horizontal split: list (45%) | detail (55%).
    let [list_col, detail_col] =
        Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(area);

    render_saved_list(app, theme, frame, list_col);
    render_saved_detail(app, theme, frame, detail_col);
}

/// Fit a name into terminal columns without splitting a grapheme. Ratatui's
/// grapheme iterator uses the same segmentation as the list renderer. The
/// name is sanitized first, so U+FFFD is what gets measured and cut.
fn fit_saved_name(name: &str, columns: usize) -> String {
    use unicode_width::UnicodeWidthStr as _;

    let name = sanitize_display_text(name);
    if columns == 0 {
        return String::new();
    }
    if name.width() <= columns {
        return name;
    }
    let span = Span::raw(name.as_str());
    let mut remaining = columns - "…".width();
    let mut fitted = String::new();
    for grapheme in span.styled_graphemes(Style::default()) {
        let width = grapheme.symbol.width();
        if width > remaining {
            break;
        }
        fitted.push_str(grapheme.symbol);
        remaining -= width;
    }
    fitted.push('…');
    fitted
}

/// Render the left-pane saved query list.
fn render_saved_list(app: &App, theme: &Theme, frame: &mut Frame<'_>, area: Rect) {
    let selected = app.panel.saved_selected;
    let is_focused = app.panel.saved_focus == SavedFocus::List;

    let Some(ref saved) = app.saved_cache else {
        return;
    };

    #[allow(clippy::cast_possible_truncation)]
    let max_width = area.width as usize;
    let items: Vec<ListItem<'_>> = saved
        .queries
        .iter()
        .map(|entry| {
            let mut suffixes = Vec::new();
            if let Some(ref sched) = entry.schedule {
                suffixes.push(Span::styled(
                    sanitize_display_text(&format_schedule_badge(sched)),
                    Style::default().fg(theme.text_accent),
                ));
                if let Some(ref last_run) = sched.last_run {
                    let (icon, color) = match last_run.status.as_str() {
                        "success" => (" \u{2713}", theme.status_success),
                        "error" => (" \u{2717}", theme.status_error),
                        "running" => (" \u{25cf}", theme.status_warning),
                        _ => (" ?", theme.text_muted),
                    };
                    suffixes.push(Span::styled(icon, Style::default().fg(color)));
                }
            }

            // Compute suffix width for name budget.
            let suffix_width: usize = suffixes.iter().map(Span::width).sum();
            let name_budget = max_width.saturating_sub(suffix_width);
            let display_name = fit_saved_name(&entry.name, name_budget);

            let mut spans = vec![Span::styled(
                display_name,
                Style::default().fg(theme.text_primary),
            )];
            spans.extend(suffixes);
            ListItem::new(Line::from(spans))
        })
        .collect();

    let total = items.len();
    let highlight = if is_focused {
        Style::default()
            .bg(theme.surface_highlight)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(theme.text_accent)
            .add_modifier(Modifier::BOLD)
    };
    let list = List::new(items).highlight_style(highlight);

    #[allow(clippy::cast_possible_truncation)]
    let visible_height = area.height as usize;
    let offset = compute_center_offset(selected, visible_height, total);
    let mut state = ListState::default().with_offset(offset);
    state.select(Some(selected));
    frame.render_stateful_widget(list, area, &mut state);

    if total > visible_height {
        let mut sb_state = ScrollbarState::new(total).position(offset);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("\u{2191}"))
            .end_symbol(Some("\u{2193}"));
        frame.render_stateful_widget(
            scrollbar,
            area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut sb_state,
        );
    }
}

/// Render the right-pane detail view for the selected saved query.
#[allow(clippy::too_many_lines)]
fn render_saved_detail(app: &App, theme: &Theme, frame: &mut Frame<'_>, area: Rect) {
    // Inset 1 col from the left to visually separate from the list pane.
    let area = Rect {
        x: area.x + 1,
        width: area.width.saturating_sub(1),
        ..area
    };

    match app.panel.saved_focus {
        SavedFocus::List => {
            // Show a hint when no detail pane is active.
            let hint = Paragraph::new("Enter/\u{2192} to view run history")
                .style(Style::default().fg(theme.text_muted));
            frame.render_widget(hint, area);
        }
        SavedFocus::Detail => {
            render_saved_detail_inner(app, theme, frame, area);
        }
        SavedFocus::RunResults => {
            render_saved_run_results(app, theme, frame, area);
        }
    }
}

/// The schedule's run budget for the detail `runs` line: the lifetime claim
/// count alone without a cap, `N/M` under a cap, and `spent N/M` once the
/// budget is spent. N can exceed M after the cap is lowered; it shows as is.
fn run_budget(sched: &trawl_api::ScheduleResponse) -> String {
    match sched.max_runs {
        None => sched.total_runs.to_string(),
        Some(max) if sched.exhausted => format!("spent {}/{max}", sched.total_runs),
        Some(max) => format!("{}/{max}", sched.total_runs),
    }
}

/// Render the detail view: query info header + run history sub-list.
#[allow(clippy::too_many_lines)]
fn render_saved_detail_inner(app: &App, theme: &Theme, frame: &mut Frame<'_>, area: Rect) {
    let Some(ref saved) = app.saved_cache else {
        return;
    };
    // The header names the saved query whose runs the detail holds.
    let entry = match app.panel.saved_detail {
        Some(ref detail) => saved.queries.iter().find(|q| q.id == detail.saved_id),
        None => saved.queries.get(app.panel.saved_selected),
    };
    let Some(entry) = entry else {
        return;
    };

    let mut lines: Vec<Line<'static>> = Vec::new();

    // -- name --
    lines.push(Line::from(Span::styled(
        sanitize_display_text(&entry.name),
        Style::default()
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD),
    )));

    // -- separator --
    #[allow(clippy::cast_possible_truncation)]
    let sep_width = area.width.min(40) as usize;
    lines.push(Line::from(Span::styled(
        "\u{2500}".repeat(sep_width),
        Style::default().fg(theme.text_muted),
    )));

    // -- query DSL --
    let label_style = Style::default().fg(theme.text_muted);
    let value_style = Style::default().fg(theme.text_primary);
    lines.push(Line::from(vec![
        Span::styled("query  ", label_style),
        Span::styled(
            sanitize_display_text(&entry.query.replace('\n', " ")),
            value_style,
        ),
    ]));

    // -- schedule info --
    if let Some(ref sched) = entry.schedule {
        let enabled_str = if sched.enabled { "enabled" } else { "paused" };
        lines.push(Line::from(vec![
            Span::styled("sched  ", label_style),
            Span::styled(
                format!(
                    "every {} ({})",
                    sanitize_display_text(&sched.interval),
                    enabled_str
                ),
                value_style,
            ),
        ]));
        // Continuation lines under the `sched` label: what the scheduler owns
        // of the report window, then the fire cursor every schedule has.
        if let Some(window) = format_schedule_window(sched, app.display_zone) {
            lines.push(Line::from(vec![
                Span::styled("       ", label_style),
                Span::styled(sanitize_display_text(&window), value_style),
            ]));
        }
        lines.push(Line::from(vec![
            Span::styled("       ", label_style),
            Span::styled(
                format!(
                    "next fire {}",
                    format_instant(&sched.next_fire_at, app.display_zone)
                ),
                value_style,
            ),
        ]));
        lines.push(Line::from(vec![
            Span::styled("runs   ", label_style),
            Span::styled(run_budget(sched), value_style),
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::styled("sched  ", label_style),
            Span::styled("none", Style::default().fg(theme.text_muted)),
        ]));
    }

    lines.push(Line::default());

    // -- runs sub-list header --
    let Some(ref detail) = app.panel.saved_detail else {
        lines.push(Line::from(Span::styled(
            "loading runs...",
            Style::default().fg(theme.text_muted),
        )));
        let p = Paragraph::new(lines);
        frame.render_widget(p, area);
        return;
    };

    // A pending request with no runs yet is the runs request; a run-result
    // request leaves the loaded runs on screen.
    if detail.pending.is_some() && detail.runs.is_empty() {
        lines.push(Line::from(Span::styled(
            "loading runs...",
            Style::default().fg(theme.text_muted),
        )));
        let p = Paragraph::new(lines);
        frame.render_widget(p, area);
        return;
    }

    if detail.runs.is_empty() {
        lines.push(Line::from(Span::styled(
            "no runs yet",
            Style::default().fg(theme.text_muted),
        )));
        let p = Paragraph::new(lines);
        frame.render_widget(p, area);
        return;
    }

    let header_lines = lines.len();
    lines.push(Line::from(Span::styled(
        format!(
            "runs ({} kept)  \u{2191}\u{2193} navigate  Enter view  q query",
            detail.total_runs
        ),
        Style::default().fg(theme.text_muted),
    )));

    // Render the header as a paragraph, then the run list below it.
    let header_height = header_lines + 1; // +1 for the runs header
    #[allow(clippy::cast_possible_truncation)]
    let header_h = (header_height as u16).min(area.height);
    let [header_area, list_area] =
        Layout::vertical([Constraint::Length(header_h), Constraint::Min(1)]).areas(area);

    let header_p = Paragraph::new(lines);
    frame.render_widget(header_p, header_area);

    let run_items: Vec<ListItem<'_>> = detail
        .runs
        .iter()
        .map(|run| {
            let (icon, icon_color) = match run.status.as_str() {
                "success" => ("\u{2713}", theme.status_success),
                "error" => ("\u{2717}", theme.status_error),
                "running" => ("\u{25cf}", theme.status_warning),
                "timeout" => ("\u{25cb}", theme.status_warning),
                _ => ("?", theme.text_muted),
            };

            let time_str = format_relative_time(&run.started_at);
            let rows_str = run
                .row_count
                .map(|r| format!(" {r} rows"))
                .unwrap_or_default();
            let dur_str = run
                .duration_ms
                .map(|d| format!(" {d}ms"))
                .unwrap_or_default();

            let mut spans = vec![
                Span::styled(format!("{icon} "), Style::default().fg(icon_color)),
                Span::styled(
                    format!("#{:<5}", run.id),
                    Style::default().fg(theme.text_accent),
                ),
                Span::styled(time_str, Style::default().fg(theme.text_muted)),
                Span::styled(dur_str, Style::default().fg(theme.text_primary)),
                Span::styled(rows_str, Style::default().fg(theme.text_muted)),
            ];

            // The window trails everything else: a narrow pane truncates the
            // Line from the right, and the status, id and time are what an
            // operator scans first. A run with no window (query mode, or one
            // from before the schedule grew a window) renders nothing here —
            // silence is how "no window" differs from `window_truncated:
            // Some(false)`, which shows its bounds with no marker.
            if let Some(ref kind) = run.window_kind {
                spans.push(Span::styled(
                    format!("  {}", sanitize_display_text(kind)),
                    Style::default().fg(theme.text_muted),
                ));
            }
            // Keep the coverage-loss warning ahead of bounds that may be clipped.
            if run.window_truncated == Some(true) {
                spans.push(Span::styled(
                    " TRUNCATED",
                    Style::default().fg(theme.status_warning),
                ));
            }
            if let (Some(start), Some(end)) = (&run.window_start, &run.window_end) {
                spans.push(Span::styled(
                    format!(" {}", format_run_window(start, end, app.display_zone)),
                    Style::default().fg(theme.text_muted),
                ));
            }

            ListItem::new(Line::from(spans))
        })
        .collect();

    let run_total = run_items.len();
    let run_list = List::new(run_items).highlight_style(
        Style::default()
            .bg(theme.surface_highlight)
            .add_modifier(Modifier::BOLD),
    );

    #[allow(clippy::cast_possible_truncation)]
    let visible_height = list_area.height as usize;
    let offset = compute_center_offset(detail.run_selected, visible_height, run_total);
    let mut state = ListState::default().with_offset(offset);
    state.select(Some(detail.run_selected));
    frame.render_stateful_widget(run_list, list_area, &mut state);

    if run_total > visible_height {
        let mut sb_state = ScrollbarState::new(run_total).position(offset);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("\u{2191}"))
            .end_symbol(Some("\u{2193}"));
        frame.render_stateful_widget(
            scrollbar,
            list_area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut sb_state,
        );
    }
}

/// Render run results in a simple table view.
#[allow(clippy::too_many_lines)]
fn render_saved_run_results(app: &App, theme: &Theme, frame: &mut Frame<'_>, area: Rect) {
    let Some(ref detail) = app.panel.saved_detail else {
        return;
    };
    let Some(LoadedRun { run_id, ref result }) = detail.result else {
        let p = Paragraph::new("loading result...").style(Style::default().fg(theme.text_muted));
        frame.render_widget(p, area);
        return;
    };

    let run_info = detail.runs.iter().find(|run| run.id == run_id);
    let title_str = if let Some(run) = run_info {
        let time_str = format_relative_time(&run.started_at);
        format!(
            "Run #{} ({}, {} rows)  Esc back  q query",
            run.id,
            time_str.trim(),
            result.rows.len()
        )
    } else {
        format!("{} rows", result.rows.len())
    };

    let mut lines: Vec<Line<'static>> = Vec::new();

    // -- title --
    lines.push(Line::from(Span::styled(
        title_str,
        Style::default()
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD),
    )));

    // -- the rows on screen (windowed by scroll offset) --
    // After the title: the header, the separator and the footer.
    #[allow(clippy::cast_possible_truncation)]
    let visible_rows = (area.height as usize).saturating_sub(lines.len() + 3);
    let total_rows = result.rows.len();
    let start = detail.result_scroll.min(total_rows);
    let end = (start + visible_rows).min(total_rows);

    // -- column headers --
    let col_widths = run_result_widths(result, start..end, app.display_zone);
    let header_spans: Vec<Span<'static>> = result
        .columns
        .iter()
        .zip(&col_widths)
        .map(|(column, &width)| {
            Span::styled(
                run_result_cell(&column.name, width),
                Style::default()
                    .fg(theme.text_accent)
                    .add_modifier(Modifier::BOLD),
            )
        })
        .collect();
    lines.push(Line::from(header_spans));

    // -- separator --
    #[allow(clippy::cast_possible_truncation)]
    let sep_width = area.width as usize;
    lines.push(Line::from(Span::styled(
        "\u{2500}".repeat(sep_width),
        Style::default().fg(theme.text_muted),
    )));

    // -- data rows --
    for row in &result.rows[start..end] {
        let row_spans: Vec<Span<'static>> = row
            .iter()
            .zip(&result.columns)
            .zip(&col_widths)
            .map(|((val, column), &width)| {
                Span::styled(
                    run_result_cell(&run_result_text(column, val, app.display_zone), width),
                    Style::default().fg(theme.text_primary),
                )
            })
            .collect();
        lines.push(Line::from(row_spans));
    }

    // -- scroll indicator --
    if total_rows > visible_rows {
        lines.push(Line::from(Span::styled(
            format!(
                "rows {}-{} of {} (\u{2191}\u{2193} scroll)",
                start + 1,
                end,
                total_rows,
            ),
            Style::default().fg(theme.text_muted),
        )));
    }

    let p = Paragraph::new(lines);
    frame.render_widget(p, area);
}

/// The widest a run result column gets, in display cells.
const RUN_RESULT_COLUMN_CAP: usize = 30;

/// Display-cell widths of the run result columns.
///
/// Widths sample the header and the first 100 rows, so a wider value further
/// down renders cut. Each sample is measured as it would render at the cap.
///
/// A timestamp column is the exception. Its offset is at the end, and a cut
/// there would hide the zone the instant reads in, so each instant is
/// measured whole, and the rows on screen (`visible`) are measured with the
/// sample.
fn run_result_widths(
    result: &trawl_api::value::QueryResult,
    visible: std::ops::Range<usize>,
    zone: DisplayZone,
) -> Vec<usize> {
    let fitted = |text: &str| cells(&fit_cells(text, RUN_RESULT_COLUMN_CAP));
    let sample = result.rows.iter().take(100);
    let on_screen = result.rows.get(visible).unwrap_or_default();
    result
        .columns
        .iter()
        .enumerate()
        .map(|(i, column)| {
            let width = fitted(&column.name);
            if column.ty == trawl_api::value::ColumnType::Timestamp {
                sample
                    .clone()
                    .chain(on_screen)
                    .filter_map(|row| row.get(i))
                    .map(|val| cells(&run_result_text(column, val, zone)))
                    .fold(width, usize::max)
            } else {
                sample
                    .clone()
                    .filter_map(|row| row.get(i))
                    .map(|val| fitted(&run_result_text(column, val, zone)))
                    .fold(width, usize::max)
            }
        })
        .collect()
}

/// A run result cell as the table shows it: an instant in `zone` with its
/// own offset, any other cell through [`format_value`].
fn run_result_text(
    column: &trawl_api::value::Column,
    val: &trawl_api::value::Value,
    zone: DisplayZone,
) -> String {
    trawl_api::zone::present_cell(&column.ty, val, zone).unwrap_or_else(|| format_value(val))
}

/// One run result cell: `text` cut and padded to `width` display cells, then
/// the two-cell gap before the next column.
fn run_result_cell(text: &str, width: usize) -> String {
    let mut cell = pad_cells(text, width);
    cell.push_str("  ");
    cell
}

/// Format a `Value` for display in the run results table.
fn format_value(val: &trawl_api::value::Value) -> String {
    match val {
        trawl_api::value::Value::Null => "NULL".to_owned(),
        trawl_api::value::Value::Boolean(b) => b.to_string(),
        trawl_api::value::Value::Integer(i) => i.to_string(),
        trawl_api::value::Value::UInt(u) => u.to_string(),
        trawl_api::value::Value::Float(f) => format!("{f:.2}"),
        trawl_api::value::Value::String(s) => s.clone(),
        trawl_api::value::Value::Array(arr) => {
            let inner: Vec<String> = arr.iter().map(format_value).collect();
            format!("[{}]", inner.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_time_seconds() {
        let now = chrono::Utc::now();
        let recent = now - chrono::Duration::seconds(30);
        let result = format_relative_time(&recent.to_rfc3339());
        assert_eq!(result.len(), 4);
        assert!(result.trim().ends_with('s'));
    }

    #[test]
    fn relative_time_minutes() {
        let now = chrono::Utc::now();
        let past = now - chrono::Duration::minutes(45);
        let result = format_relative_time(&past.to_rfc3339());
        assert_eq!(result, " 45m");
    }

    #[test]
    fn relative_time_hours() {
        let now = chrono::Utc::now();
        let past = now - chrono::Duration::hours(4);
        let result = format_relative_time(&past.to_rfc3339());
        assert_eq!(result, "  4h");
    }

    #[test]
    fn relative_time_days() {
        let now = chrono::Utc::now();
        let past = now - chrono::Duration::days(3);
        let result = format_relative_time(&past.to_rfc3339());
        assert_eq!(result, "  3d");
    }

    #[test]
    fn relative_time_weeks() {
        let now = chrono::Utc::now();
        let past = now - chrono::Duration::weeks(2);
        let result = format_relative_time(&past.to_rfc3339());
        assert_eq!(result, "  2w");
    }

    #[test]
    fn relative_time_invalid_fallback() {
        assert_eq!(format_relative_time("garbage"), "????");
    }

    fn zone(text: &str) -> DisplayZone {
        text.parse().unwrap()
    }

    #[test]
    fn run_window_same_day_drops_the_end_date() {
        assert_eq!(
            format_run_window("2026-03-14T02:00:00Z", "2026-03-14T03:00:00Z", zone("UTC")),
            "[2026-03-14 02:00:00 +00:00 .. 03:00:00 +00:00)"
        );
    }

    #[test]
    fn run_window_across_midnight_keeps_the_end_date() {
        assert_eq!(
            format_run_window("2026-03-14T23:30:00Z", "2026-03-15T00:30:00Z", zone("UTC")),
            "[2026-03-14 23:30:00 +00:00 .. 2026-03-15 00:30:00 +00:00)"
        );
    }

    /// One UTC day, but midnight at `+05:30` falls inside it: the dates as
    /// displayed differ, so the end keeps its date.
    #[test]
    fn run_window_crossing_midnight_in_the_display_zone_keeps_both_dates() {
        assert_eq!(
            format_run_window(
                "2026-03-14T18:00:00Z",
                "2026-03-14T19:00:00Z",
                zone("+05:30")
            ),
            "[2026-03-14 23:30:00 +05:30 .. 2026-03-15 00:30:00 +05:30)"
        );
    }

    /// A window inside one displayed day drops the end's date and keeps
    /// both offsets.
    #[test]
    fn run_window_on_one_displayed_day_drops_the_end_date() {
        assert_eq!(
            format_run_window(
                "2026-03-14T02:00:00Z",
                "2026-03-14T03:00:00Z",
                zone("+05:30")
            ),
            "[2026-03-14 07:30:00 +05:30 .. 08:30:00 +05:30)"
        );
    }

    /// An offset-bearing bound is read as the instant it names and shown in
    /// the display zone, so two runs are always read on the same clock.
    #[test]
    fn run_window_shows_an_offset_bound_in_the_display_zone() {
        assert_eq!(
            format_run_window(
                "2026-03-14T09:00:00+05:30",
                "2026-03-14T10:00:00+05:30",
                zone("UTC")
            ),
            "[2026-03-14 03:30:00 +00:00 .. 04:30:00 +00:00)"
        );
    }

    #[test]
    fn schedule_bounds_preserve_subminute_precision() {
        assert_eq!(
            format_run_window(
                "2026-03-14T02:00:17.123456Z",
                "2026-03-14T03:00:17.123457Z",
                zone("+05:30")
            ),
            "[2026-03-14 07:30:17.123456 +05:30 .. 08:30:17.123457 +05:30)"
        );
        assert_eq!(
            format_instant("2026-03-14T04:00:17.123456+01:00", zone("+05:30")),
            "2026-03-14 08:30:17.123456 +05:30"
        );
    }

    /// A bound that is not a wire timestamp shows as sent, sanitized, and the
    /// other bound keeps its date: no guessed instant, no shared day.
    #[test]
    fn run_window_passes_through_an_unparseable_bound() {
        assert_eq!(
            format_run_window("garbage", "2026-03-14T03:00:00Z", zone("UTC")),
            "[garbage .. 2026-03-14 03:00:00 +00:00)"
        );
        assert_eq!(
            format_run_window("2026-03-14T02:00:00Z", "", zone("UTC")),
            "[2026-03-14 02:00:00 +00:00 .. )"
        );
        // Text that only looks like a reading on the same day keeps its
        // date beside a converted bound.
        assert_eq!(
            format_run_window("2026-03-14T02:00:00Z", "2026-03-14 later", zone("UTC")),
            "[2026-03-14 02:00:00 +00:00 .. 2026-03-14 later)"
        );
        assert_eq!(
            format_run_window("2026-03-14\u{202E}T02", "2026-03-14T03:00:00Z", zone("UTC")),
            "[2026-03-14\u{FFFD}T02 .. 2026-03-14 03:00:00 +00:00)"
        );
        assert_eq!(
            format_run_window("2026-03-14T02:00:00Z", "\u{202E}soon", zone("UTC")),
            "[2026-03-14 02:00:00 +00:00 .. \u{FFFD}soon)"
        );
    }

    /// Set only in the child process, which reads the run window under the
    /// `TZ` the parent chose.
    const BERLIN_CHILD: &str = "TRAWL_PANELS_BERLIN_CHILD";

    /// Under `TZ=Europe/Berlin` with the display zone `local`, a window over
    /// the fall-back night keeps each bound's own offset, and since both
    /// read 2026-10-25 the end drops only its date. The machine zone is per
    /// process, so this test binary re-runs itself with the ignored
    /// [`tui_instants_dst_child_under_berlin`] selected and `TZ` set. The
    /// parent fails if the child fails or never ran.
    #[test]
    fn tui_instants_keep_their_dst_offsets() {
        const CHILD: &str = "tui::ui::panels::tests::tui_instants_dst_child_under_berlin";
        let output =
            std::process::Command::new(std::env::current_exe().expect("locate this test binary"))
                .args([CHILD, "--exact", "--ignored", "--test-threads=1"])
                .env("TZ", "Europe/Berlin")
                .env(BERLIN_CHILD, "1")
                .output()
                .expect("run the child");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "the Berlin child failed or did not run; status {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            output.status,
        );
    }

    /// The subprocess entry point. Without [`BERLIN_CHILD`] it does nothing,
    /// so a run with `--run-ignored` passes it untouched.
    #[test]
    #[ignore = "entry point for the Berlin child process; the test above runs it"]
    fn tui_instants_dst_child_under_berlin() {
        if std::env::var_os(BERLIN_CHILD).is_none() {
            return;
        }
        assert_eq!(
            format_run_window(
                "2026-10-25T00:30:00Z",
                "2026-10-25T01:30:00Z",
                DisplayZone::Local
            ),
            "[2026-10-25 02:30:00 +02:00 .. 02:30:00 +01:00)"
        );
    }

    /// A schedule on a one-hour interval, with the ADR-0018 window fields
    /// under the test's control.
    fn schedule(
        window: Option<&str>,
        lag: Option<(&str, u64)>,
        covered_through: Option<&str>,
    ) -> trawl_api::ScheduleResponse {
        trawl_api::ScheduleResponse {
            id: 1,
            saved_query_id: 1,
            interval: "1h".to_owned(),
            interval_secs: 3600,
            max_runs: None,
            enabled: true,
            created_at: "2026-03-01T00:00:00Z".to_owned(),
            updated_at: "2026-03-01T00:00:00Z".to_owned(),
            last_run: None,
            total_runs: 0,
            exhausted: false,
            window: window.map(ToOwned::to_owned),
            lag: lag.map(|(text, _)| text.to_owned()),
            lag_secs: lag.map(|(_, secs)| secs),
            covered_through: covered_through.map(ToOwned::to_owned),
            next_fire_at: "2026-03-14T04:00:00Z".to_owned(),
        }
    }

    #[test]
    fn schedule_window_tiled_with_lag_and_watermark() {
        let sched = schedule(
            Some("since_last"),
            Some(("5m", 300)),
            Some("2026-03-14T03:00:00Z"),
        );
        assert_eq!(
            format_schedule_window(&sched, zone("+05:30")).unwrap(),
            "window since_last, lag 5m, covered through 2026-03-14 08:30:00 +05:30"
        );
    }

    /// A zero lag is the default in force, not a value worth a line.
    #[test]
    fn schedule_window_omits_a_zero_lag() {
        let sched = schedule(Some("2h"), Some(("0s", 0)), None);
        assert_eq!(
            format_schedule_window(&sched, zone("UTC")).unwrap(),
            "window 2h"
        );
    }

    /// A fixed window keeps no watermark, so there is nothing to print.
    #[test]
    fn schedule_window_omits_an_absent_watermark() {
        let sched = schedule(Some("2h"), Some(("5m", 300)), None);
        assert_eq!(
            format_schedule_window(&sched, zone("UTC")).unwrap(),
            "window 2h, lag 5m"
        );
    }

    #[test]
    fn schedule_window_is_none_in_query_mode() {
        assert!(format_schedule_window(&schedule(None, None, None), zone("UTC")).is_none());
    }

    #[test]
    fn schedule_badge_names_the_window_mode() {
        assert_eq!(format_schedule_badge(&schedule(None, None, None)), " [1h]");
        assert_eq!(
            format_schedule_badge(&schedule(Some("since_last"), None, None)),
            " [1h since_last]"
        );
        assert_eq!(
            format_schedule_badge(&schedule(Some("2h"), None, None)),
            " [1h window 2h]"
        );
    }

    #[test]
    fn instant_passes_through_an_unparseable_value() {
        assert_eq!(
            format_instant("2026-03-14T04:00:00Z", zone("+05:30")),
            "2026-03-14 09:30:00 +05:30"
        );
        assert_eq!(format_instant("soon", zone("+05:30")), "soon");
        assert_eq!(
            format_instant("\u{202E}soon", zone("+05:30")),
            "\u{FFFD}soon"
        );
    }

    #[test]
    fn relative_time_right_justified() {
        let now = chrono::Utc::now();
        let past = now - chrono::Duration::minutes(5);
        let result = format_relative_time(&past.to_rfc3339());
        assert_eq!(result, "  5m");
    }
}

#[cfg(test)]
mod saved_name_tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use unicode_width::UnicodeWidthStr as _;

    /// A saved name is sanitized before it is measured, so U+FFFD counts as
    /// the cell it takes on the fast path and on the clipping path.
    #[test]
    fn saved_name_sanitizes_before_fit() {
        assert_eq!(fit_saved_name("nm\u{202e}x", 10), "nm\u{fffd}x");
        assert_eq!(fit_saved_name("nl\nx", 10), "nl\u{fffd}x");
        assert_eq!(fit_saved_name("ab\u{200b}cdefgh", 5), "ab\u{fffd}c…");
        assert_eq!(fit_saved_name("sh\u{00ad}yz", 4), "sh\u{fffd}…");
    }

    #[test]
    fn saved_name_column_boundaries_preserve_graphemes() {
        for (name, columns, expected) in [
            ("日本語", 0, ""),
            ("日本語", 1, "…"),
            ("日本語", 2, "…"),
            ("日本語", 3, "日…"),
            ("日本語", 4, "日…"),
            ("日本語", 6, "日本語"),
            ("éabc", 2, "é…"),
            ("e\u{301}abc", 2, "e\u{301}…"),
            ("🙂🙂", 3, "🙂…"),
            ("👍🏽ab", 3, "👍🏽…"),
            ("🇯🇵ab", 3, "🇯🇵…"),
            // A legacy name can contain a joiner even though new writes
            // refuse it. U+200D is drawn as U+FFFD, which splits the
            // sequence, so the name is cut after the first emoji.
            ("👩\u{200d}💻ab", 3, "👩…"),
        ] {
            let actual = fit_saved_name(name, columns);
            assert_eq!(actual, expected, "{name:?}, columns={columns}");
            assert!(actual.width() <= columns);
        }
    }

    #[test]
    fn saved_list_renders_long_unicode_names_in_narrow_columns() {
        for (name, width, expected) in [
            ("日本語".repeat(20), 4, "日… "),
            ("é".repeat(20), 4, "ééé…"),
            ("e\u{301}".repeat(20), 4, "e\u{301}e\u{301}e\u{301}…"),
            ("🙂".repeat(20), 4, "🙂… "),
            ("👩\u{200d}💻".repeat(20), 4, "👩\u{fffd}…"),
        ] {
            let mut app = crate::tui::tests::test_app();
            app.saved_cache = Some(trawl_api::ListSavedResponse {
                queries: vec![trawl_api::SavedQueryResponse {
                    id: 1,
                    name,
                    query: "*".to_owned(),
                    created_at: "2026-01-01T00:00:00Z".to_owned(),
                    updated_at: "2026-01-01T00:00:00Z".to_owned(),
                    schedule: None,
                }],
            });
            let mut terminal = Terminal::new(TestBackend::new(width, 1)).unwrap();
            terminal
                .draw(|frame| render_saved_list(&app, &app.theme, frame, frame.area()))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let mut actual = String::new();
            let mut x = 0;
            while x < width {
                let symbol = buffer[(x, 0)].symbol();
                actual.push_str(symbol);
                x += u16::try_from(symbol.width().max(1)).unwrap();
            }
            assert_eq!(actual, expected);
        }
    }
}

#[cfg(test)]
mod unicode_tests {
    use std::fmt::Write as _;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::text::Line;
    use trawl_api::value::{Column, ColumnType, QueryResult, Value};
    use trawl_core::text_layout::{MARKER, cells};

    use super::*;
    use crate::tui::state::{CommonField, SavedDetailState};
    use crate::tui::tests::test_app;

    /// Sender-controlled names a cut must not split: accented, CJK, a
    /// combining mark, an emoji with its variation selector, and a bidi
    /// override with a control character.
    const FIXTURES: &[&str] = &[
        "résumé_ñ",
        "日本語フィールド",
        "e\u{301}e\u{301}e\u{301}_field",
        "❤\u{FE0F}❤\u{FE0F}_love",
        "a\u{202E}b\u{7}c_bidi",
    ];

    /// Widths from nothing to past every fixture's natural width.
    const WIDTHS: std::ops::RangeInclusive<u16> = 0..=48;

    fn text_of(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn draw(width: u16, height: u16, render: impl FnOnce(&mut Frame<'_>)) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal.draw(render).expect("render");
        terminal
    }

    fn schema() -> SchemaBrowser {
        let column = |name: &str, data_type: &str| trawl_api::ServiceColumnStats {
            name: name.to_owned(),
            data_type: data_type.to_owned(),
            null_count: 1,
            total_count: 4,
            min_value: None,
            max_value: None,
            compressed_bytes: 0,
        };
        let services: Vec<_> = FIXTURES
            .iter()
            .map(|name| trawl_api::ServiceSchema {
                name: (*name).to_owned(),
                columns: FIXTURES
                    .iter()
                    .map(|field| column(field, "VARCHAR"))
                    .chain([column("日本", "DÉCIMAL(18,3)")])
                    .collect(),
                earliest_date: None,
                latest_date: None,
                file_count: 1,
                total_bytes: 1,
                total_events: 4,
                daily_event_counts: Vec::new(),
                degraded_fields: Vec::new(),
            })
            .collect();
        let mut schema = SchemaBrowser::new(services);
        schema.common_fields = FIXTURES
            .iter()
            .map(|name| CommonField {
                name: format!("common_{name}"),
                data_type: "TIMESTAMP".to_owned(),
                service_count: FIXTURES.len(),
                null_count: 0,
                total_count: 4,
                min_value: None,
                max_value: None,
            })
            .collect();
        schema.expanded = FIXTURES.iter().map(|name| (*name).to_owned()).collect();
        schema
    }

    /// Every schema tree row keeps its name inside the cells left after the
    /// row's fixed parts, and a service field's name column is exactly that
    /// wide so the type column lines up.
    #[test]
    fn unicode_panels_survive_resize_schema_tree() {
        let schema = schema();
        let theme = test_app().theme;
        let nodes = flatten_tree(&schema, &theme);
        for width in WIDTHS.map(usize::from) {
            for node in &nodes {
                let line = tree_line(node, &theme, width);
                let name = line.spans.get(1).map_or(0, |span| cells(&span.content));
                match node {
                    TreeNode::CommonHeader { .. } => {}
                    TreeNode::CommonField { data_type, .. } => {
                        let budget = width.saturating_sub(3 + cells(&abbreviate_type(data_type)));
                        assert!(name <= budget, "{node:?} at {width}");
                    }
                    TreeNode::ServiceField { data_type, .. } => {
                        let type_width = cells(&abbreviate_type(data_type)).max(4);
                        let budget = width.saturating_sub(10 + type_width);
                        assert_eq!(name, budget, "{node:?} at {width}");
                    }
                    TreeNode::Service { field_count, .. } => {
                        let count = format!(" ({field_count})").len();
                        assert!(
                            name <= width.saturating_sub(2 + count),
                            "{node:?} at {width}"
                        );
                    }
                }
                // Once the fixed parts fit, the whole row does.
                if width >= 16 + 2 {
                    assert!(cells(&text_of(&line)) <= width, "{node:?} at {width}");
                }
            }
            for height in [3, 40] {
                draw(u16::try_from(width).unwrap(), height, |frame| {
                    render_schema_tree(&schema, &theme, frame, frame.area());
                });
            }
        }
    }

    /// The fallback type label is cut by display cells, not bytes.
    #[test]
    fn unicode_panels_survive_resize_type_label() {
        assert_eq!(abbreviate_type("DÉCIMAL(18,3)"), format!("DÉCIM{MARKER}"));
        assert_eq!(abbreviate_type("日本語型"), format!("日本{MARKER}"));
        assert_eq!(abbreviate_type("LIST"), "LIST");
    }

    fn history_app(queries: &[&str]) -> crate::tui::App {
        let mut app = test_app();
        app.history_cache = Some(trawl_api::HistoryResponse {
            entries: queries
                .iter()
                .enumerate()
                .map(|(id, query)| trawl_api::HistoryEntryResponse {
                    id: i64::try_from(id).unwrap(),
                    query: (*query).to_owned(),
                    executed_at: "garbage".to_owned(),
                    duration_ms: 1,
                    row_count: 1,
                    status: trawl_api::QueryStatus::Success,
                })
                .collect(),
            total: queries.len(),
        });
        app
    }

    fn history_queries() -> Vec<String> {
        FIXTURES
            .iter()
            .map(|name| format!("service=\"{name}\"\n| where {name} ~ /x/"))
            .collect()
    }

    /// A history row's query keeps inside what the time, the gap and the
    /// trailing room leave, and the row inside the pane once those fit.
    #[test]
    fn unicode_panels_survive_resize_history() {
        let queries = history_queries();
        let queries: Vec<&str> = queries.iter().map(String::as_str).collect();
        let app = history_app(&queries);
        for width in WIDTHS {
            for entry in &app.history_cache.as_ref().unwrap().entries {
                let line = history_line(entry, &app.theme, usize::from(width));
                // The query, then its two trailing cells.
                let query = cells(&line.spans[2].content) - 2;
                assert!(
                    query <= usize::from(width).saturating_sub(4 + 2 + 2),
                    "{width}"
                );
                if width >= 8 {
                    assert!(cells(&text_of(&line)) <= usize::from(width), "{width}");
                }
            }
            for height in [2, 20] {
                draw(width, height, |frame| {
                    render_history_list(&app, &app.theme, frame, frame.area());
                });
            }
        }
    }

    #[test]
    fn unicode_panels_survive_resize_history_snapshot() {
        let queries = history_queries();
        let queries: Vec<&str> = queries.iter().map(String::as_str).collect();
        let app = history_app(&queries);
        let mut frames = String::new();
        for width in [13, 17, 24] {
            let terminal = draw(width, 5, |frame| {
                render_history_list(&app, &app.theme, frame, frame.area());
            });
            writeln!(frames, "width {width}:\n{}", terminal.backend()).unwrap();
        }
        insta::assert_snapshot!(frames);
    }

    #[test]
    fn unicode_panels_survive_resize_schema_snapshot() {
        let schema = schema();
        let theme = test_app().theme;
        let mut frames = String::new();
        for width in [15, 19, 26] {
            let terminal = draw(width, 14, |frame| {
                render_schema_tree(&schema, &theme, frame, frame.area());
            });
            writeln!(frames, "width {width}:\n{}", terminal.backend()).unwrap();
        }
        insta::assert_snapshot!(frames);
    }

    fn run_app(columns: &[&str], rows: Vec<Vec<Value>>) -> crate::tui::App {
        let mut app = test_app();
        app.panel.saved_detail = Some(SavedDetailState {
            saved_id: 1,
            runs: Vec::new(),
            total_runs: 0,
            run_selected: 0,
            run_scroll: 0,
            result: Some(LoadedRun {
                run_id: 7,
                result: QueryResult {
                    columns: columns
                        .iter()
                        .map(|name| Column::new(*name, ColumnType::String))
                        .collect(),
                    rows,
                },
            }),
            result_scroll: 0,
            pending: None,
        });
        app
    }

    /// Saved-run headers and cells are each exactly their column's width
    /// plus the gap, and no column passes the cap.
    #[test]
    fn unicode_panels_survive_resize_saved_run() {
        let rows = FIXTURES
            .iter()
            .map(|name| {
                vec![
                    Value::String((*name).to_owned()),
                    Value::String(name.repeat(10)),
                ]
            })
            .collect();
        let header = FIXTURES.concat();
        let app = run_app(&["日本語フィールド", &header], rows);
        let result = &app
            .panel
            .saved_detail
            .as_ref()
            .unwrap()
            .result
            .as_ref()
            .unwrap()
            .result;
        let widths = run_result_widths(result, 0..0, DisplayZone::Utc);
        assert!(
            widths.iter().all(|&w| w <= RUN_RESULT_COLUMN_CAP),
            "{widths:?}"
        );
        assert_eq!(widths[0], 16);
        for (i, column) in result.columns.iter().enumerate() {
            for budget in 0..=RUN_RESULT_COLUMN_CAP + 2 {
                assert_eq!(cells(&run_result_cell(&column.name, budget)), budget + 2);
            }
            for row in &result.rows {
                let cell = run_result_cell(&format_value(&row[i]), widths[i]);
                assert_eq!(cells(&cell), widths[i] + 2, "{cell:?}");
            }
        }
        for width in WIDTHS {
            for height in [3, 12] {
                draw(width, height, |frame| {
                    render_saved_run_results(&app, &app.theme, frame, frame.area());
                });
            }
        }
    }

    /// Columns holding CJK, accented and combining text, with a header and a
    /// value past the 30-cell cap: on every rendered row each column starts
    /// on the same cell.
    #[test]
    fn unicode_result_columns_align() {
        let rows = vec![
            vec![
                Value::String("日本語フィールド".into()),
                Value::String("résumé_ñ".into()),
                Value::String("7001".into()),
            ],
            vec![
                Value::String("web".into()),
                Value::String("x".repeat(50)),
                Value::String("7022".into()),
            ],
            vec![
                Value::String("e\u{301}e\u{301}".into()),
                Value::String("❤\u{FE0F} ok".into()),
                Value::String("7333".into()),
            ],
        ];
        let long_header = "message_header_that_is_well_over_thirty_cells";
        let app = run_app(&["service", long_header, "count"], rows);
        let terminal = draw(90, 8, |frame| {
            render_saved_run_results(&app, &app.theme, frame, frame.area());
        });
        let screen = terminal.backend().buffer();

        // Rows as text, one symbol per drawn grapheme (a wide symbol hides
        // the cell after it).
        let rows: Vec<String> = screen
            .content
            .chunks(usize::from(screen.area.width))
            .map(|row| {
                let mut text = String::new();
                let mut hidden = 0usize;
                for cell in row {
                    if hidden == 0 {
                        text.push_str(cell.symbol());
                    }
                    hidden = hidden.max(cells(cell.symbol())).saturating_sub(1);
                }
                text
            })
            .collect();
        let all = rows.join("\n");

        // The header and the three data rows, by their last column.
        let starts = [
            ("count", "message_"),
            ("7001", "résumé"),
            ("7022", "xxx"),
            ("7333", "❤"),
        ];
        let column_of = |row: &str, value: &str| cells(&row[..row.find(value).expect("value")]);
        let mut boundaries = Vec::new();
        for (third, second) in starts {
            let row = rows
                .iter()
                .find(|row| row.contains(third))
                .unwrap_or_else(|| panic!("row with {third}:\n{all}"));
            boundaries.push((column_of(row, second), column_of(row, third)));
        }
        // `service` is 16 cells wide (the CJK value), the second column is
        // capped at 30, each followed by a two-cell gap.
        assert!(
            boundaries.iter().all(|&b| b == (18, 50)),
            "{boundaries:?}\n{all}"
        );
        assert!(
            all.contains(&format!("message_header_that_is_well_o{MARKER}")),
            "{all}"
        );
        insta::assert_snapshot!(all);
    }

    /// A halfwidth sound mark takes a cell of its own where ratatui draws
    /// it, so a column holding `ｶﾞ` pads it as two cells. The next column's
    /// start is read off the buffer's own cell positions, which ratatui
    /// lays out with its own widths, not with the kit's.
    #[test]
    fn unicode_result_columns_align_halfwidth_sound_marks() {
        let rows = vec![
            vec![Value::String("ｶﾞ".into()), Value::String("A1".into())],
            vec![Value::String("web".into()), Value::String("B2".into())],
            vec![Value::String("ﾊﾟﾊﾟﾊﾟﾊﾟﾊﾟ".into()), Value::String("C3".into())],
        ];
        let app = run_app(&["service", "Next"], rows);
        let terminal = draw(40, 8, |frame| {
            render_saved_run_results(&app, &app.theme, frame, frame.area());
        });
        let buffer = terminal.backend().buffer();
        let width = usize::from(buffer.area.width);
        let start_of = |marker: &str| {
            buffer
                .content
                .chunks(width)
                .find_map(|row| row.iter().position(|cell| cell.symbol() == marker))
                .unwrap_or_else(|| panic!("no {marker:?} in\n{buffer:?}"))
        };
        let starts: Vec<usize> = ["N", "A", "B", "C"].into_iter().map(start_of).collect();
        // `ﾊﾟﾊﾟﾊﾟﾊﾟﾊﾟ` is ten cells, then the two-cell gap.
        assert_eq!(starts, [12, 12, 12, 12], "{buffer:?}");
    }
}
