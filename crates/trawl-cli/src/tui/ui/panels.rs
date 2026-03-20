// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Full-width panel content for non-Query tabs (History, Schema, Saved).

use std::collections::HashSet;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use crate::tui::App;
use crate::tui::state::{Focus, MainTab, SchemaBrowser};
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
                let cols = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
                    .split(inner);

                // Left side: filter bar + tree.
                let left = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(1), Constraint::Min(1)])
                    .split(cols[0]);

                render_filter_bar(&schema.filter, schema.filter_active, theme, frame, left[0]);
                render_schema_tree(schema, theme, frame, left[1]);

                // Right side: detail pane.
                super::schema::render_detail_pane(schema, frame, cols[1], theme);
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
                    let cols = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
                        .split(inner);

                    render_history_list(app, theme, frame, cols[0]);
                    super::history::render_detail_pane(app, frame, cols[1]);
                }
            } else {
                let paragraph = Paragraph::new("loading history...")
                    .style(Style::default().fg(theme.text_muted));
                frame.render_widget(paragraph, inner);
            }
        }
        MainTab::Saved => render_saved_list(app, theme, frame, inner),
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
// Schema tree view (placeholder — phase 6 rewrites this properly)
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

    #[allow(clippy::cast_possible_truncation)]
    let visible_height = area.height as usize;
    let offset = compute_center_offset(selected, visible_height, nodes.len());
    let mut state = ListState::default().with_offset(offset);
    state.select(Some(selected));
    frame.render_stateful_widget(list, area, &mut state);
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

// ---------------------------------------------------------------------------
// Saved queries list
// ---------------------------------------------------------------------------

/// Render saved queries in the panel.
fn render_saved_list(app: &App, theme: &Theme, frame: &mut Frame<'_>, area: Rect) {
    let selected = app.panel.saved_selected;

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

    #[allow(clippy::cast_possible_truncation)]
    let max_width = area.width as usize;
    let items: Vec<ListItem<'_>> = saved
        .queries
        .iter()
        .map(|entry| {
            // Show schedule indicator if the query has one.
            let schedule_suffix = entry
                .schedule
                .as_ref()
                .map(|s| format!(" [{}]", s.interval))
                .unwrap_or_default();
            let name_budget = max_width.saturating_sub(schedule_suffix.len());
            let display_name = if entry.name.len() > name_budget {
                format!("{}…", &entry.name[..name_budget.saturating_sub(1)])
            } else {
                entry.name.clone()
            };

            if schedule_suffix.is_empty() {
                ListItem::new(Span::styled(
                    display_name,
                    Style::default().fg(theme.text_primary),
                ))
            } else {
                ListItem::new(Line::from(vec![
                    Span::styled(display_name, Style::default().fg(theme.text_primary)),
                    Span::styled(schedule_suffix, Style::default().fg(theme.text_muted)),
                ]))
            }
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
    fn relative_time_right_justified() {
        let now = chrono::Utc::now();
        let past = now - chrono::Duration::minutes(5);
        let result = format_relative_time(&past.to_rfc3339());
        assert_eq!(result, "  5m");
    }
}
