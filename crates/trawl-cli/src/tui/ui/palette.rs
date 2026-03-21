// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Command palette overlay rendering.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};

use crate::tui::App;
use crate::tui::palette::{FilteredItem, PaletteCategory, PaletteItem};
use crate::tui::state::Popup;
use crate::tui::theme::Theme;

use super::common::centered_rect;

/// Render the command palette overlay.
pub fn render(app: &App, frame: &mut Frame<'_>) {
    let Some(Popup::CommandPalette {
        ref input,
        cursor,
        selected,
        scroll: _,
        ref items,
        ref filtered,
        ref ghost,
    }) = app.popup
    else {
        return;
    };

    let theme = &app.theme;
    let area = centered_rect(65, 75, frame.area());

    // Clear the area under the popup.
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Command Palette ")
        .borders(Borders::ALL)
        .title_bottom(
            Line::from(" ↑↓ navigate | Enter select | Tab complete | Esc close ")
                .alignment(Alignment::Center),
        )
        .style(Style::default().bg(theme.surface).fg(theme.text_primary));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Split inner area: input line (1), separator (1), item list (rest).
    let chunks = Layout::vertical([
        Constraint::Length(1), // input
        Constraint::Length(1), // separator
        Constraint::Min(1),    // items
    ])
    .split(inner);

    // -- input line with ghost text --
    let input_line = Line::from(vec![
        Span::styled("> ", Style::default().fg(theme.text_accent)),
        Span::raw(input),
        Span::styled(
            ghost.as_deref().unwrap_or(""),
            Style::default().fg(theme.text_muted),
        ),
    ]);
    frame.render_widget(Paragraph::new(input_line), chunks[0]);

    // Position cursor in the input (before ghost text).
    #[allow(clippy::cast_possible_truncation)]
    let cursor_x = chunks[0].x + 2 + cursor as u16; // ">" + space + cursor offset
    frame.set_cursor_position((
        cursor_x.min(chunks[0].right().saturating_sub(1)),
        chunks[0].y,
    ));

    // -- separator line --
    let sep = "─".repeat(chunks[1].width as usize);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            sep,
            Style::default().fg(theme.border_unfocused),
        ))),
        chunks[1],
    );

    // -- item list --
    let list_area = chunks[2];
    let visible_height = list_area.height as usize;

    // Detect column mode: input contains a dot.
    let column_mode_service = input.find('.').map(|pos| &input[..pos]);

    // Build display lines from filtered items.
    let list_width = list_area.width as usize;
    let display = build_display_lines(
        input,
        items,
        filtered,
        theme,
        column_mode_service,
        list_width,
    );

    // Compute scroll offset based on selected item position.
    // We need to find the line index of the selected item.
    let selected_line_idx = find_selected_line(&display, selected);
    let scroll_offset = compute_scroll(selected_line_idx, visible_height, display.len());

    // Render visible lines, highlighting the selected item.
    let visible_lines: Vec<Line<'_>> = display
        .iter()
        .skip(scroll_offset)
        .take(visible_height)
        .map(|dl| {
            if dl.filtered_index == Some(selected) {
                dl.line
                    .clone()
                    .patch_style(Style::default().bg(theme.surface_highlight))
            } else {
                dl.line.clone()
            }
        })
        .collect();

    frame.render_widget(Paragraph::new(visible_lines), list_area);

    // Scrollbar (if content exceeds visible height).
    if display.len() > visible_height {
        let mut scrollbar_state = ScrollbarState::new(display.len().saturating_sub(visible_height))
            .position(scroll_offset);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            list_area,
            &mut scrollbar_state,
        );
    }
}

/// A single display line in the palette (may be a header, item, or detail).
struct DisplayLine<'a> {
    line: Line<'a>,
    /// If this line represents a selectable item, its index in the filtered list.
    filtered_index: Option<usize>,
}

/// Build the display lines from filtered items.
fn build_display_lines<'a>(
    input: &str,
    items: &'a [PaletteItem],
    filtered: &[FilteredItem],
    theme: &Theme,
    column_mode_service: Option<&str>,
    width: usize,
) -> Vec<DisplayLine<'a>> {
    let mut lines = Vec::new();
    let is_filtered = !input.is_empty();

    // In column mode with no matches, show a "no matching service" or "no columns" message.
    if filtered.is_empty() {
        let msg = if column_mode_service.is_some() {
            "  No matching service"
        } else {
            "  No matches"
        };
        lines.push(DisplayLine {
            line: Line::from(Span::styled(msg, Style::default().fg(theme.text_muted))),
            filtered_index: None,
        });
        return lines;
    }

    // Column mode: show a service header, then flat column list.
    if let Some(service_name) = column_mode_service {
        lines.push(DisplayLine {
            line: Line::from(Span::styled(
                format!("  {service_name} — Columns"),
                Style::default()
                    .fg(theme.table_header)
                    .add_modifier(Modifier::BOLD),
            )),
            filtered_index: None,
        });

        for (fi, entry) in filtered.iter().enumerate() {
            let item = &items[entry.item_index];
            lines.push(DisplayLine {
                line: render_item_line(item, &entry.match_positions, theme, width),
                filtered_index: Some(fi),
            });
        }
        return lines;
    }

    if is_filtered {
        // Flat list sorted by score (already sorted in `filtered`).
        for (fi, entry) in filtered.iter().enumerate() {
            let item = &items[entry.item_index];
            lines.push(DisplayLine {
                line: render_item_line(item, &entry.match_positions, theme, width),
                filtered_index: Some(fi),
            });
            // Detail line for saved/history items.
            if let Some(ref detail) = item.detail
                && matches!(
                    item.category,
                    PaletteCategory::SavedQuery | PaletteCategory::History
                )
            {
                lines.push(DisplayLine {
                    line: render_detail_line(detail, theme),
                    filtered_index: None,
                });
            }
        }
    } else {
        // Grouped by category with headers.
        let mut current_category: Option<PaletteCategory> = None;

        for (fi, entry) in filtered.iter().enumerate() {
            let item = &items[entry.item_index];

            // Insert category header when category changes.
            if current_category != Some(item.category) {
                if current_category.is_some() {
                    // Blank separator between categories.
                    lines.push(DisplayLine {
                        line: Line::from(""),
                        filtered_index: None,
                    });
                }
                lines.push(DisplayLine {
                    line: Line::from(Span::styled(
                        format!("  {}", item.category.header()),
                        Style::default()
                            .fg(theme.table_header)
                            .add_modifier(Modifier::BOLD),
                    )),
                    filtered_index: None,
                });
                current_category = Some(item.category);
            }

            lines.push(DisplayLine {
                line: render_item_line(item, &[], theme, width),
                filtered_index: Some(fi),
            });

            // Detail line for saved/history.
            if let Some(ref detail) = item.detail
                && matches!(
                    item.category,
                    PaletteCategory::SavedQuery | PaletteCategory::History
                )
            {
                lines.push(DisplayLine {
                    line: render_detail_line(detail, theme),
                    filtered_index: None,
                });
            }
        }
    }

    lines
}

/// Render a single palette item as a `Line`.
///
/// `width` is the available line width — used to right-align shortcuts/hints.
fn render_item_line<'a>(
    item: &'a PaletteItem,
    match_positions: &[u32],
    theme: &Theme,
    width: usize,
) -> Line<'a> {
    let mut spans = Vec::new();
    let indent = 2; // "  " prefix

    // Selection indicator (populated by the caller via styling).
    spans.push(Span::raw("  "));

    // Label with match highlighting.
    let label_len = item.label.chars().count();
    if match_positions.is_empty() {
        spans.push(Span::styled(
            item.label.as_str(),
            Style::default().fg(theme.text_primary),
        ));
    } else {
        // Build spans with highlighted matched characters.
        for (i, ch) in item.label.chars().enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            let is_match = match_positions.contains(&(i as u32));
            let style = if is_match {
                Style::default()
                    .fg(theme.text_accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text_primary)
            };
            spans.push(Span::styled(String::from(ch), style));
        }
    }

    // Right-aligned shortcut or detail hint.
    let hint: Option<&str> = item.shortcut.as_deref().or_else(|| {
        if item.category == PaletteCategory::Service {
            item.detail.as_deref()
        } else {
            None
        }
    });

    if let Some(hint_text) = hint {
        let used = indent + label_len;
        let hint_len = hint_text.len();
        // pad = total width - used - hint - 1 trailing space
        let pad = width.saturating_sub(used + hint_len + 1);
        if pad > 0 {
            spans.push(Span::raw(" ".repeat(pad)));
        }
        spans.push(Span::styled(
            hint_text.to_string(),
            Style::default().fg(theme.text_muted),
        ));
    }

    Line::from(spans)
}

/// Render a detail line (indented, dimmed).
fn render_detail_line<'a>(detail: &'a str, theme: &Theme) -> Line<'a> {
    // Truncate long detail text.
    let max_len = 60;
    let display = if detail.len() > max_len {
        format!("    {}…", &detail[..max_len])
    } else {
        format!("    {detail}")
    };
    Line::from(Span::styled(display, Style::default().fg(theme.text_muted)))
}

/// Find the display line index for the selected filtered item.
fn find_selected_line(display: &[DisplayLine<'_>], selected: usize) -> usize {
    display
        .iter()
        .position(|dl| dl.filtered_index == Some(selected))
        .unwrap_or(0)
}

/// Compute scroll offset to keep the selected line centered in the viewport.
fn compute_scroll(selected_line: usize, visible_height: usize, total_lines: usize) -> usize {
    if visible_height == 0 || total_lines <= visible_height {
        return 0;
    }
    let half = visible_height / 2;
    let max_scroll = total_lines.saturating_sub(visible_height);
    selected_line.saturating_sub(half).min(max_scroll)
}
