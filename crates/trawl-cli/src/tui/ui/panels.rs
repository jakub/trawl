//! Full-width panel content for non-Query tabs (History, Schema, Saved).

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use crate::tui::App;
use crate::tui::state::{Focus, MainTab, ProfiledColumn};

/// Render panel content based on the active `MainTab`.
pub fn render(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let border_style = if app.focus == Focus::Panel {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .style(Style::default().bg(Color::Black));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    match app.main_tab {
        MainTab::Schema => {
            // Schema has a filter bar above the tree.
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(1)])
                .split(inner);

            render_filter_bar(
                &app.panel.schema.filter,
                app.panel.schema.filter_active,
                frame,
                chunks[0],
            );
            render_schema_tree(app, frame, chunks[1]);
        }
        MainTab::History => render_history_list(app, frame, inner),
        MainTab::Saved => render_saved_list(app, frame, inner),
        MainTab::Query | MainTab::Dashboard => {} // Use their own layout.
    }
}

/// Render the filter input bar for schema search.
fn render_filter_bar(filter: &str, active: bool, frame: &mut Frame<'_>, area: Rect) {
    let style = if active {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let text = if filter.is_empty() && !active {
        "/ filter".to_owned()
    } else if filter.is_empty() {
        "/ █".to_owned()
    } else if active {
        format!("/ {filter}█")
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
    Service {
        name: &'a str,
        expanded: bool,
        field_count: usize,
    },
    Field {
        col: &'a ProfiledColumn,
    },
    Loading,
}

/// Flatten the schema tree into a linear list for rendering.
fn flatten_tree(app: &App) -> Vec<TreeNode<'_>> {
    let services = app.service_list_cache.as_deref().unwrap_or(&[]);
    let tree = &app.panel.schema;
    let filter = tree.filter.to_lowercase();

    let mut nodes = Vec::new();
    for svc in services {
        // Apply filter: skip services that don't match and have no matching fields.
        if !filter.is_empty() {
            let svc_matches = svc.to_lowercase().contains(&filter);
            let fields_match = app
                .schema_profile_cache
                .get(svc.as_str())
                .is_some_and(|cols| cols.iter().any(|c| c.name.to_lowercase().contains(&filter)));
            if !svc_matches && !fields_match {
                continue;
            }
        }

        let expanded = tree.expanded.contains(svc);
        let field_count = app
            .schema_profile_cache
            .get(svc.as_str())
            .map_or(0, Vec::len);

        nodes.push(TreeNode::Service {
            name: svc,
            expanded,
            field_count,
        });

        if expanded {
            if let Some(cols) = app.schema_profile_cache.get(svc.as_str()) {
                for col in cols {
                    nodes.push(TreeNode::Field { col });
                }
            } else {
                nodes.push(TreeNode::Loading);
            }
        }
    }
    nodes
}

/// Color for a data type badge.
fn type_color(data_type: &str) -> Color {
    let dt = data_type.to_uppercase();
    if dt.starts_with("VARCHAR") || dt.starts_with("TEXT") || dt.starts_with("STRING") {
        Color::Green
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
        Color::Yellow
    } else if dt.starts_with("TIMESTAMP") || dt.starts_with("DATE") || dt.starts_with("TIME") {
        Color::Cyan
    } else if dt.starts_with("BOOLEAN") || dt.starts_with("BOOL") {
        Color::Magenta
    } else {
        Color::DarkGray
    }
}

/// Render the schema tree view.
fn render_schema_tree(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let nodes = flatten_tree(app);
    let selected = app.panel.schema.selected;

    if nodes.is_empty() {
        let msg = if app
            .service_list_cache
            .as_ref()
            .is_some_and(|s| !s.is_empty())
        {
            "no matches"
        } else {
            "no services"
        };
        let paragraph = Paragraph::new(msg).style(Style::default().fg(Color::DarkGray));
        frame.render_widget(paragraph, area);
        return;
    }

    let items: Vec<ListItem<'_>> = nodes
        .iter()
        .map(|node| match node {
            TreeNode::Service {
                name,
                expanded,
                field_count,
            } => {
                let arrow = if *expanded { "▾" } else { "▸" };
                let count_str = if *field_count > 0 {
                    format!(" ({field_count})")
                } else {
                    String::new()
                };
                // Truncate name to fit
                let max_name = (area.width as usize).saturating_sub(6);
                let display_name = if name.len() > max_name {
                    &name[..max_name]
                } else {
                    name
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{arrow} "), Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        display_name.to_string(),
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(count_str, Style::default().fg(Color::DarkGray)),
                ]))
            }
            TreeNode::Field { col } => {
                let pct = col.population_pct();
                let pct_color = if pct >= 80 {
                    Color::Green
                } else if pct >= 50 {
                    Color::Yellow
                } else {
                    Color::Red
                };

                // Short type abbreviation
                let type_str = abbreviate_type(&col.data_type);
                let tc = type_color(&col.data_type);

                // Truncate field name to fit available space
                // Layout: "  name    TYPE  pct%"
                let type_width = type_str.len() + 1; // type + space
                let pct_width = 5; // " 100%"
                let prefix_width = 2; // "  "
                let max_name =
                    (area.width as usize).saturating_sub(prefix_width + type_width + pct_width + 1);
                let display_name = if col.name.len() > max_name {
                    &col.name[..max_name]
                } else {
                    &col.name
                };

                // Compute padding between name and type
                let name_pad = max_name.saturating_sub(display_name.len());

                ListItem::new(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(display_name.to_string(), Style::default().fg(Color::Cyan)),
                    Span::raw(" ".repeat(name_pad + 1)),
                    Span::styled(type_str, Style::default().fg(tc)),
                    Span::raw(" "),
                    Span::styled(format!("{pct:>3}%"), Style::default().fg(pct_color)),
                ]))
            }
            TreeNode::Loading => ListItem::new(Line::from(Span::styled(
                "  ⋯ loading...",
                Style::default().fg(Color::DarkGray),
            ))),
        })
        .collect();

    let list = List::new(items).highlight_style(
        Style::default()
            .bg(Color::DarkGray)
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

/// Render history entries in the panel.
fn render_history_list(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let selected = app.panel.history_selected;

    let Some(ref history) = app.history_cache else {
        let paragraph =
            Paragraph::new("loading history...").style(Style::default().fg(Color::DarkGray));
        frame.render_widget(paragraph, area);
        return;
    };

    if history.entries.is_empty() {
        let paragraph =
            Paragraph::new("no history yet").style(Style::default().fg(Color::DarkGray));
        frame.render_widget(paragraph, area);
        return;
    }

    #[allow(clippy::cast_possible_truncation)]
    let max_width = area.width as usize;
    let items: Vec<ListItem<'_>> = history
        .entries
        .iter()
        .map(|entry| {
            let query = entry.query.replace('\n', " ");
            let display = if query.len() > max_width {
                format!("{}…", &query[..max_width.saturating_sub(1)])
            } else {
                query
            };
            ListItem::new(Span::styled(display, Style::default().fg(Color::White)))
        })
        .collect();

    let total = items.len();
    let list = List::new(items).highlight_style(
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );

    #[allow(clippy::cast_possible_truncation)]
    let visible_height = area.height as usize;
    let offset = compute_center_offset(selected, visible_height, total);
    let mut state = ListState::default().with_offset(offset);
    state.select(Some(selected));
    frame.render_stateful_widget(list, area, &mut state);
}

// ---------------------------------------------------------------------------
// Saved queries list
// ---------------------------------------------------------------------------

/// Render saved queries in the panel.
fn render_saved_list(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let selected = app.panel.saved_selected;

    let Some(ref saved) = app.saved_cache else {
        let paragraph =
            Paragraph::new("loading saved...").style(Style::default().fg(Color::DarkGray));
        frame.render_widget(paragraph, area);
        return;
    };

    if saved.queries.is_empty() {
        let paragraph =
            Paragraph::new("no saved queries").style(Style::default().fg(Color::DarkGray));
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
                    Style::default().fg(Color::White),
                ))
            } else {
                ListItem::new(Line::from(vec![
                    Span::styled(display_name, Style::default().fg(Color::White)),
                    Span::styled(schedule_suffix, Style::default().fg(Color::DarkGray)),
                ]))
            }
        })
        .collect();

    let total = items.len();
    let list = List::new(items).highlight_style(
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );

    #[allow(clippy::cast_possible_truncation)]
    let visible_height = area.height as usize;
    let offset = compute_center_offset(selected, visible_height, total);
    let mut state = ListState::default().with_offset(offset);
    state.select(Some(selected));
    frame.render_stateful_widget(list, area, &mut state);
}
