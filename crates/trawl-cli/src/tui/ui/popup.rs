// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Popup overlays (confirmations, text input).

use ratatui::Frame;
use ratatui::layout::Alignment;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::tui::App;
use crate::tui::state::Popup;
use crate::tui::theme::Theme;

/// Render the active popup (if any).
pub fn render(app: &App, frame: &mut Frame<'_>) {
    if let Some(popup) = &app.popup {
        let theme = &app.theme;
        match popup {
            Popup::Help { scroll } => {
                crate::tui::ui::help::render(app, frame, *scroll);
            }
            Popup::ConfirmDelete { name, .. } => {
                render_confirm_delete(frame, theme, name);
            }
            Popup::SaveQuery { editor } => {
                render_save_query(frame, theme, editor);
            }
            Popup::EventDetail { row_index, scroll } => {
                render_event_detail(app, frame, *row_index, *scroll);
            }
            Popup::Error { message } => {
                render_error(frame, theme, message);
            }
            Popup::SetSchedule { name, editor, .. } => {
                render_set_schedule(frame, theme, name, editor);
            }
            Popup::ColumnPicker { selected, scroll } => {
                render_column_picker(app, frame, *selected, *scroll);
            }
            Popup::CommandPalette { .. } => {
                super::palette::render(app, frame);
            }
        }
    }
}

/// Render confirmation dialog for deleting a saved query.
fn render_confirm_delete(frame: &mut Frame<'_>, theme: &Theme, name: &str) {
    let area = centered_rect(60, 35, frame.area());

    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Confirm Delete ")
        .borders(Borders::ALL)
        .style(Style::default().bg(theme.surface).fg(theme.status_error));

    let text = vec![
        Line::from(""),
        Line::from(""),
        Line::from(Span::styled(
            "Delete saved query?",
            Style::default()
                .fg(theme.status_error)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            name,
            Style::default()
                .fg(theme.status_warning)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(""),
        Line::from(Span::styled(
            "Press Y or Enter to confirm",
            Style::default().fg(theme.text_muted),
        )),
        Line::from(Span::styled(
            "Any other key to cancel",
            Style::default().fg(theme.text_muted),
        )),
        Line::from(""),
    ];

    let paragraph = Paragraph::new(text)
        .block(block)
        .alignment(Alignment::Center)
        .wrap(ratatui::widgets::Wrap { trim: false });

    frame.render_widget(paragraph, area);
}

/// Render text input dialog for saving a query.
fn render_save_query(
    frame: &mut Frame<'_>,
    theme: &Theme,
    editor: &crate::tui::state::SimpleEditor,
) {
    let area = centered_rect(60, 35, frame.area());
    let input = editor.text();

    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Save Query ")
        .borders(Borders::ALL)
        .style(Style::default().bg(theme.surface).fg(theme.text_accent));

    let text = vec![
        Line::from(""),
        Line::from(""),
        Line::from(Span::styled(
            "Enter a name for this query:",
            Style::default().fg(theme.text_primary),
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!("> {input}\u{2588}"),
            Style::default()
                .fg(theme.text_accent)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(""),
        Line::from(Span::styled(
            "Press Enter to save",
            Style::default().fg(theme.text_muted),
        )),
        Line::from(Span::styled(
            "Esc to cancel",
            Style::default().fg(theme.text_muted),
        )),
        Line::from(""),
    ];

    let paragraph = Paragraph::new(text)
        .block(block)
        .alignment(Alignment::Center)
        .wrap(ratatui::widgets::Wrap { trim: false });

    frame.render_widget(paragraph, area);

    // Position the cursor inside the input field.
    let cursor_col = editor.cursor.1;
    #[allow(clippy::cast_possible_truncation)]
    let cursor_x =
        area.x + (area.width / 2).saturating_sub((input.len() as u16) / 2) + 2 + cursor_col as u16;
    let cursor_y = area.y + 5; // Row of the input line within the popup
    frame.set_cursor_position((cursor_x, cursor_y));
}

/// Render the event detail popup (key-value view of a single row).
///
/// Groups well-known fields first, then remaining non-null fields alphabetically.
/// Null fields are hidden to reduce noise.
fn render_event_detail(app: &App, frame: &mut Frame<'_>, row_index: usize, scroll: usize) {
    use trawl_api::value::WELL_KNOWN_LOG_FIELDS;
    use trawl_engine::value::Value;

    let area = centered_rect(70, 80, frame.area());
    frame.render_widget(Clear, area);

    let tab = app.active_tab();
    let Some(response) = &tab.result else {
        return;
    };

    let result = &response.result;
    let Some(row_data) = result.rows.get(row_index) else {
        return;
    };

    // Collect non-null fields into (name, value_string) pairs.
    let total_fields = result.columns.len();
    let fields: Vec<(&str, String)> = result
        .columns
        .iter()
        .enumerate()
        .filter_map(|(i, col)| {
            let value = row_data.get(i)?;
            if matches!(value, Value::Null) {
                return None;
            }
            Some((col.name.as_str(), super::results::value_to_string(value)))
        })
        .collect();
    let visible_fields = fields.len();

    // Partition into well-known (in priority order) and others (alphabetical).
    let mut well_known: Vec<(&str, &str)> = Vec::new();
    for &wk in WELL_KNOWN_LOG_FIELDS {
        if let Some((_, val)) = fields.iter().find(|(name, _)| *name == wk) {
            well_known.push((wk, val.as_str()));
        }
    }
    let mut others: Vec<(&str, &str)> = fields
        .iter()
        .filter(|(name, _)| !WELL_KNOWN_LOG_FIELDS.contains(name))
        .map(|(name, val)| (*name, val.as_str()))
        .collect();
    others.sort_by_key(|(name, _)| *name);

    let theme = &app.theme;
    let title = format!(
        " Event Detail (row {}, {visible_fields}/{total_fields} fields) ",
        row_index + 1
    );
    let block = Block::default()
        .title(title)
        .title_bottom(" ↑↓: scroll | []: prev/next row | Esc: close ")
        .borders(Borders::ALL)
        .style(Style::default().bg(theme.surface).fg(theme.text_primary));

    // Find max name width across all visible fields for alignment.
    let max_name_len = fields.iter().map(|(n, _)| n.len()).max().unwrap_or(0);

    let divider = Line::from(Span::styled(
        "─".repeat(area.width.saturating_sub(4) as usize),
        Style::default().fg(theme.border_unfocused),
    ));

    // Build lines: well-known section → divider → other fields.
    let mut lines: Vec<Line<'_>> = Vec::new();

    if !well_known.is_empty() {
        lines.push(Line::from(""));
        for (name, val) in &well_known {
            lines.push(field_line(name, val, max_name_len, theme));
        }
    }

    if !others.is_empty() {
        if well_known.is_empty() {
            lines.push(Line::from(""));
        } else {
            lines.push(divider);
        }
        for (name, val) in &others {
            lines.push(field_line(name, val, max_name_len, theme));
        }
    }

    lines.push(Line::from(""));

    let max_scroll = lines.len().saturating_sub(1);
    let clamped_scroll = scroll.min(max_scroll);

    #[allow(clippy::cast_possible_truncation)]
    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(ratatui::widgets::Wrap { trim: false })
        .scroll((clamped_scroll as u16, 0));

    frame.render_widget(paragraph, area);
}

/// Build a single key-value line for the event detail popup.
fn field_line<'a>(name: &str, value: &str, max_name_len: usize, theme: &Theme) -> Line<'a> {
    Line::from(vec![
        Span::styled(
            format!("{name:>max_name_len$}  "),
            Style::default()
                .fg(theme.text_accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(value.to_owned()),
    ])
}

/// Render an error message popup.
fn render_error(frame: &mut Frame<'_>, theme: &Theme, message: &str) {
    let area = centered_rect(60, 30, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Error ")
        .borders(Borders::ALL)
        .style(Style::default().bg(theme.surface).fg(theme.status_error));

    let text = vec![
        Line::from(""),
        Line::from(""),
        Line::from(Span::styled(
            message,
            Style::default().fg(theme.status_error),
        )),
        Line::from(""),
        Line::from(""),
        Line::from(Span::styled(
            "Press Esc to dismiss",
            Style::default().fg(theme.text_muted),
        )),
        Line::from(""),
    ];

    let paragraph = Paragraph::new(text)
        .block(block)
        .alignment(Alignment::Center)
        .wrap(ratatui::widgets::Wrap { trim: false });

    frame.render_widget(paragraph, area);
}

/// Render text input dialog for scheduling a saved query.
fn render_set_schedule(
    frame: &mut Frame<'_>,
    theme: &Theme,
    name: &str,
    editor: &crate::tui::state::SimpleEditor,
) {
    let area = centered_rect(60, 40, frame.area());
    let input = editor.text();

    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Set Schedule ")
        .borders(Borders::ALL)
        .style(Style::default().bg(theme.surface).fg(theme.text_accent));

    let text = vec![
        Line::from(""),
        Line::from(Span::styled(
            name,
            Style::default()
                .fg(theme.status_warning)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Interval (e.g. 5m, 1h, 24h):",
            Style::default().fg(theme.text_primary),
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!("> {input}\u{2588}"),
            Style::default()
                .fg(theme.text_accent)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Enter to schedule  |  Esc to cancel",
            Style::default().fg(theme.text_muted),
        )),
        Line::from(""),
    ];

    let paragraph = Paragraph::new(text)
        .block(block)
        .alignment(Alignment::Center)
        .wrap(ratatui::widgets::Wrap { trim: false });

    frame.render_widget(paragraph, area);

    // Position cursor inside the input field.
    let cursor_col = editor.cursor.1;
    #[allow(clippy::cast_possible_truncation)]
    let cursor_x =
        area.x + (area.width / 2).saturating_sub((input.len() as u16) / 2) + 2 + cursor_col as u16;
    let cursor_y = area.y + 6; // Row of the input line within the popup
    frame.set_cursor_position((cursor_x, cursor_y));
}

/// Render column picker popup — checklist for toggling visibility and pinning.
fn render_column_picker(app: &App, frame: &mut Frame<'_>, selected: usize, scroll: usize) {
    let area = centered_rect(50, 60, frame.area());
    frame.render_widget(Clear, area);

    let theme = &app.theme;
    let tab = app.active_tab();

    let Some(config) = &tab.column_config else {
        return;
    };
    let Some(response) = &tab.result else {
        return;
    };

    let block = Block::default()
        .title(" Column Picker ")
        .title_bottom(" Space: toggle | p: pin | Esc: close ")
        .borders(Borders::ALL)
        .style(Style::default().bg(theme.surface).fg(theme.text_primary));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let visible_height = inner.height as usize;
    let total = config.columns.len();

    let max_scroll = total.saturating_sub(visible_height);
    let clamped_scroll = scroll.min(max_scroll);

    let mut lines: Vec<Line<'_>> = Vec::with_capacity(visible_height);
    for i in clamped_scroll..total.min(clamped_scroll + visible_height) {
        let entry = &config.columns[i];
        let name = &response.result.columns[i].name;

        let checkbox = if entry.hidden { "[ ]" } else { "[x]" };
        let pin_label = if entry.pinned { "  (pinned)" } else { "" };
        let label = format!(" {checkbox} {name}{pin_label}");

        let style = if i == selected {
            Style::default()
                .bg(theme.text_accent)
                .fg(theme.surface)
                .add_modifier(Modifier::BOLD)
        } else if entry.hidden {
            Style::default().fg(theme.text_muted)
        } else {
            Style::default().fg(theme.text_primary)
        };

        lines.push(Line::from(Span::styled(label, style)));
    }

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, inner);
}

use super::common::centered_rect;
