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
use crate::tui::state::{Focus, MainTab, SavedFocus, SchemaBrowser};
use crate::tui::theme::Theme;

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
        format!("/ {filter}\u{2588}")
    } else {
        format!("/ {filter}")
    };

    let paragraph = Paragraph::new(Line::from(Span::styled(text, style)));
    frame.render_widget(paragraph, area);
}

// ---------------------------------------------------------------------------
// Schema tree view
// ---------------------------------------------------------------------------

/// A node in the flattened schema tree (computed at render time).
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

    let items: Vec<ListItem<'_>> = nodes
        .iter()
        .map(|node| match node {
            TreeNode::CommonHeader { field_count } => ListItem::new(Line::from(Span::styled(
                format!("\u{2605} common ({field_count})"),
                Style::default()
                    .fg(theme.text_muted)
                    .add_modifier(Modifier::BOLD),
            ))),
            TreeNode::CommonField { name, data_type } => {
                let type_str = abbreviate_type(data_type);
                let tc = type_color(data_type, theme);
                ListItem::new(Line::from(vec![
                    Span::raw("  "),
                    Span::styled((*name).to_string(), Style::default().fg(theme.text_accent)),
                    Span::raw(" "),
                    Span::styled(type_str, Style::default().fg(tc)),
                ]))
            }
            TreeNode::ServiceField {
                name,
                data_type,
                coverage,
            } => {
                let type_str = abbreviate_type(data_type);
                let tc = type_color(data_type, theme);

                // Column layout: "   " + name (padded) + " " + type (4) + " " + coverage (4) + " "
                let indent = 3usize;
                let type_width = 4usize;
                let cov_width = 4usize;
                let trailing = 1usize; // padding before detail pane border
                let overhead = indent + 1 + type_width + 1 + cov_width + trailing;
                #[allow(clippy::cast_possible_truncation)]
                let max_name = (area.width as usize).saturating_sub(overhead);
                let display_name = if name.len() > max_name {
                    &name[..max_name]
                } else {
                    name
                };
                let name_pad = max_name.saturating_sub(display_name.len());

                let mut spans = vec![
                    Span::raw(" ".repeat(indent)),
                    Span::styled(
                        (*display_name).to_string(),
                        Style::default().fg(theme.text_accent),
                    ),
                    Span::raw(" ".repeat(name_pad + 1)),
                    Span::styled(format!("{type_str:<type_width$}"), Style::default().fg(tc)),
                ];
                if let Some((ref label, color)) = *coverage {
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled(label.clone(), Style::default().fg(color)));
                }
                ListItem::new(Line::from(spans))
            }
            TreeNode::Service {
                name,
                expanded,
                field_count,
            } => {
                let arrow = if *expanded { "\u{25be}" } else { "\u{25b8}" };
                let count_str = if *field_count > 0 {
                    format!(" ({field_count})")
                } else {
                    String::new()
                };
                #[allow(clippy::cast_possible_truncation)]
                let max_name = (area.width as usize).saturating_sub(6);
                let display_name = if name.len() > max_name {
                    &name[..max_name]
                } else {
                    name
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{arrow} "), Style::default().fg(theme.text_muted)),
                    Span::styled(
                        display_name.to_string(),
                        Style::default()
                            .fg(theme.text_primary)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(count_str, Style::default().fg(theme.text_muted)),
                ]))
            }
        })
        .collect();

    let list = List::new(items).highlight_style(
        Style::default()
            .bg(theme.surface_highlight)
            .add_modifier(Modifier::BOLD),
    );

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
        // Truncate long type names
        if data_type.len() > 6 {
            data_type[..6].to_owned()
        } else {
            data_type.to_owned()
        }
    }
}

// ---------------------------------------------------------------------------
// History list
// ---------------------------------------------------------------------------

/// Render history entries in the left pane (relative time + truncated query).
fn render_history_list(app: &App, theme: &Theme, frame: &mut Frame<'_>, area: Rect) {
    // Layout: "{4-char time}  {query…} "
    const TIME_WIDTH: usize = 4;
    const GAP: usize = 2;
    const TRAILING: usize = 2; // padding before detail pane

    let selected = app.panel.history_selected;

    // Caller handles None / empty — we always have entries here.
    let history = app.history_cache.as_ref().expect("history_cache populated");

    #[allow(clippy::cast_possible_truncation)]
    let max_query = (area.width as usize).saturating_sub(TIME_WIDTH + GAP + TRAILING);

    let items: Vec<ListItem<'_>> = history
        .entries
        .iter()
        .map(|entry| {
            let time_str = format_relative_time(&entry.executed_at);
            let query = entry.query.replace('\n', " ");
            let display = if query.len() > max_query {
                format!("{}…  ", &query[..max_query.saturating_sub(1)])
            } else {
                format!("{query}  ")
            };
            ListItem::new(Line::from(vec![
                Span::styled(time_str, Style::default().fg(theme.text_muted)),
                Span::raw("  "),
                Span::styled(display, Style::default().fg(theme.text_primary)),
            ]))
        })
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

/// Render a report run's covered window as `[start .. end)` in UTC.
///
/// Bounds are half-open, so the closing bracket is deliberate. Both bounds are
/// rendered with seconds and any fractional part, since schedule edits can
/// re-anchor a boundary between minutes. When both bounds share a UTC day the end
/// bound drops its date, which is the common case for a tiled window.
///
/// A bound that does not parse is passed through verbatim rather than guessed
/// at, so an unexpected wire value is visible instead of silently formatted.
fn format_run_window(start: &str, end: &str) -> String {
    use chrono::{DateTime, Utc};

    let (Ok(from), Ok(to)) = (start.parse::<DateTime<Utc>>(), end.parse::<DateTime<Utc>>()) else {
        return format!("[{start} .. {end})");
    };

    let from_text = from.format("%Y-%m-%d %H:%M:%S%.f").to_string();
    let to_text = if from.date_naive() == to.date_naive() {
        to.format("%H:%M:%S%.f").to_string()
    } else {
        to.format("%Y-%m-%d %H:%M:%S%.f").to_string()
    };

    format!("[{from_text} .. {to_text})")
}

/// Render an RFC 3339 instant in UTC, preserving seconds and fractions. A value that
/// does not parse is passed through so an unexpected wire shape is visible.
fn format_instant_utc(iso: &str) -> String {
    use chrono::{DateTime, Utc};

    iso.parse::<DateTime<Utc>>().map_or_else(
        |_| iso.to_owned(),
        |t| format!("{} UTC", t.format("%Y-%m-%d %H:%M:%S%.f")),
    )
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
/// successful run.
fn format_schedule_window(sched: &trawl_api::ScheduleResponse) -> Option<String> {
    let window = sched.window.as_deref()?;
    let mut parts = vec![format!("window {window}")];

    if let Some(lag) = sched.lag.as_deref()
        && sched.lag_secs != Some(0)
    {
        parts.push(format!("lag {lag}"));
    }

    if let Some(covered) = sched.covered_through.as_deref() {
        parts.push(format!("covered through {}", format_instant_utc(covered)));
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
                    format_schedule_badge(sched),
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
            let display_name = if entry.name.len() > name_budget {
                format!("{}…", &entry.name[..name_budget.saturating_sub(1)])
            } else {
                entry.name.clone()
            };

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

/// Render the detail view: query info header + run history sub-list.
#[allow(clippy::too_many_lines)]
fn render_saved_detail_inner(app: &App, theme: &Theme, frame: &mut Frame<'_>, area: Rect) {
    let selected_idx = app.panel.saved_selected;
    let Some(ref saved) = app.saved_cache else {
        return;
    };
    let Some(entry) = saved.queries.get(selected_idx) else {
        return;
    };

    let mut lines: Vec<Line<'static>> = Vec::new();

    // -- name --
    lines.push(Line::from(Span::styled(
        entry.name.clone(),
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
        Span::styled(entry.query.replace('\n', " "), value_style),
    ]));

    // -- schedule info --
    if let Some(ref sched) = entry.schedule {
        let enabled_str = if sched.enabled { "enabled" } else { "paused" };
        lines.push(Line::from(vec![
            Span::styled("sched  ", label_style),
            Span::styled(
                format!("every {} ({})", sched.interval, enabled_str),
                value_style,
            ),
        ]));
        // Continuation lines under the `sched` label: what the scheduler owns
        // of the report window, then the fire cursor every schedule has.
        if let Some(window) = format_schedule_window(sched) {
            lines.push(Line::from(vec![
                Span::styled("       ", label_style),
                Span::styled(window, value_style),
            ]));
        }
        lines.push(Line::from(vec![
            Span::styled("       ", label_style),
            Span::styled(
                format!("next fire {}", format_instant_utc(&sched.next_fire_at)),
                value_style,
            ),
        ]));
        lines.push(Line::from(vec![
            Span::styled("runs   ", label_style),
            Span::styled(sched.total_runs.to_string(), value_style),
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

    if detail.loading {
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
            "runs ({} total)  \u{2191}\u{2193} navigate  Enter view  q query",
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
                    format!("  {kind}"),
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
                    format!(" {}", format_run_window(start, end)),
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
    let Some(ref result) = detail.result else {
        let p = Paragraph::new("loading result...").style(Style::default().fg(theme.text_muted));
        frame.render_widget(p, area);
        return;
    };

    let run_info = detail.runs.get(detail.run_selected);
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

    // -- column headers --
    let col_names: Vec<String> = result.columns.iter().map(|c| c.name.clone()).collect();
    // Widths sample the header and the first 100 rows, so a wider value further down
    // renders truncated.
    let col_widths: Vec<usize> = col_names
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let mut w = name.len();
            for row in result.rows.iter().take(100) {
                if let Some(val) = row.get(i) {
                    w = w.max(format_value(val).len());
                }
            }
            w.min(30) // cap at 30 chars
        })
        .collect();

    let header_spans: Vec<Span<'static>> = col_names
        .iter()
        .zip(&col_widths)
        .map(|(name, &width)| {
            Span::styled(
                format!("{name:<width$}  "),
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

    // -- data rows (windowed by scroll offset) --
    #[allow(clippy::cast_possible_truncation)]
    let visible_rows = (area.height as usize).saturating_sub(lines.len() + 1); // reserve for footer
    let scroll = detail.result_scroll;
    let total_rows = result.rows.len();
    let start = scroll.min(total_rows);
    let end = (start + visible_rows).min(total_rows);

    for row in &result.rows[start..end] {
        let row_spans: Vec<Span<'static>> = row
            .iter()
            .zip(&col_widths)
            .map(|(val, &width)| {
                let display = format_value(val);
                let truncated = if display.len() > width {
                    format!("{}…", &display[..width.saturating_sub(1)])
                } else {
                    display
                };
                Span::styled(
                    format!("{truncated:<width$}  "),
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

/// Format a `Value` for display in the run results table.
fn format_value(val: &trawl_api::value::Value) -> String {
    match val {
        trawl_api::value::Value::Null => "NULL".to_owned(),
        trawl_api::value::Value::Boolean(b) => b.to_string(),
        trawl_api::value::Value::Integer(i) => i.to_string(),
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

    #[test]
    fn run_window_same_day_drops_the_end_date() {
        assert_eq!(
            format_run_window("2026-03-14T02:00:00Z", "2026-03-14T03:00:00Z"),
            "[2026-03-14 02:00:00 .. 03:00:00)"
        );
    }

    #[test]
    fn run_window_across_midnight_keeps_the_end_date() {
        assert_eq!(
            format_run_window("2026-03-14T23:30:00Z", "2026-03-15T00:30:00Z"),
            "[2026-03-14 23:30:00 .. 2026-03-15 00:30:00)"
        );
    }

    /// An offset-bearing bound is read as the instant it names and rendered in
    /// UTC, so two runs are always comparable on the same clock.
    #[test]
    fn run_window_normalizes_an_offset_to_utc() {
        assert_eq!(
            format_run_window("2026-03-14T09:00:00+05:30", "2026-03-14T10:00:00+05:30"),
            "[2026-03-14 03:30:00 .. 04:30:00)"
        );
    }

    #[test]
    fn schedule_bounds_preserve_subminute_precision() {
        assert_eq!(
            format_run_window("2026-03-14T02:00:17.123456Z", "2026-03-14T03:00:17.123457Z"),
            "[2026-03-14 02:00:17.123456 .. 03:00:17.123457)"
        );
        assert_eq!(
            format_instant_utc("2026-03-14T04:00:17.123456+01:00"),
            "2026-03-14 03:00:17.123456 UTC"
        );
    }

    #[test]
    fn run_window_passes_through_an_unparseable_bound() {
        assert_eq!(
            format_run_window("garbage", "2026-03-14T03:00:00Z"),
            "[garbage .. 2026-03-14T03:00:00Z)"
        );
        assert_eq!(
            format_run_window("2026-03-14T02:00:00Z", ""),
            "[2026-03-14T02:00:00Z .. )"
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
            format_schedule_window(&sched).unwrap(),
            "window since_last, lag 5m, covered through 2026-03-14 03:00:00 UTC"
        );
    }

    /// A zero lag is the default in force, not a value worth a line.
    #[test]
    fn schedule_window_omits_a_zero_lag() {
        let sched = schedule(Some("2h"), Some(("0s", 0)), None);
        assert_eq!(format_schedule_window(&sched).unwrap(), "window 2h");
    }

    /// A fixed window keeps no watermark, so there is nothing to print.
    #[test]
    fn schedule_window_omits_an_absent_watermark() {
        let sched = schedule(Some("2h"), Some(("5m", 300)), None);
        assert_eq!(format_schedule_window(&sched).unwrap(), "window 2h, lag 5m");
    }

    #[test]
    fn schedule_window_is_none_in_query_mode() {
        assert!(format_schedule_window(&schedule(None, None, None)).is_none());
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
    fn instant_utc_passes_through_an_unparseable_value() {
        assert_eq!(
            format_instant_utc("2026-03-14T04:00:00Z"),
            "2026-03-14 04:00:00 UTC"
        );
        assert_eq!(format_instant_utc("soon"), "soon");
    }

    #[test]
    fn relative_time_right_justified() {
        let now = chrono::Utc::now();
        let past = now - chrono::Duration::minutes(5);
        let result = format_relative_time(&past.to_rfc3339());
        assert_eq!(result, "  5m");
    }
}
