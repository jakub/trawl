// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Query editor pane.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Padding, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};

use crate::tui::App;
use crate::tui::highlight::Highlighter;
use crate::tui::state::Focus;

/// Selection highlight style.
const SELECTION_STYLE: Style = Style::new()
    .bg(Color::DarkGray)
    .add_modifier(Modifier::empty());

/// Error underline style (red underline, preserves existing fg).
const ERROR_STYLE: Style = Style::new()
    .fg(Color::Red)
    .add_modifier(Modifier::UNDERLINED);

/// Render the editor pane.
pub fn render(app: &mut App, frame: &mut Frame<'_>, area: Rect) {
    // Determine border style.
    let border_style = if app.focus == Focus::Editor {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    // Add [LIVE] indicator if streaming
    let title = if app.live_mode {
        " Query Editor [LIVE] "
    } else {
        " Query Editor "
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(border_style)
        .padding(Padding::horizontal(1));

    // Compute visible area inside the block (minus borders + padding).
    // Borders: 1 top + 1 bottom = 2 rows, 1 left + 1 right = 2 cols
    // Padding: 1 left + 1 right = 2 cols (horizontal only)
    let visible_rows = area.height.saturating_sub(2) as usize;
    let visible_cols = area.width.saturating_sub(4) as usize; // 2 border + 2 padding

    // Ensure cursor is visible within viewport.
    let tab = app.active_tab_mut();
    tab.editor.ensure_cursor_visible(visible_rows, visible_cols);

    let tab = app.active_tab();
    let scroll_row = tab.editor.scroll_row;
    let scroll_col = tab.editor.scroll_col;
    let selection = tab.editor.selection_range();

    // Create highlighter with schema information.
    let highlighter = Highlighter::new(app.schema_cache.as_ref());

    // Convert validation error byte spans to (row, col_start, col_end) tuples.
    let error_regions = compute_error_regions(&tab.editor.lines, &tab.validation_errors);

    // Highlight visible lines and apply error + selection overlays.
    let end_row = (scroll_row + visible_rows).min(tab.editor.lines.len());
    let highlighted_lines: Vec<Line<'static>> = tab.editor.lines[scroll_row..end_row]
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let abs_row = scroll_row + i;
            let mut styled_line = highlighter.highlight_line(line);

            // Apply error overlay (red underline on error spans).
            for &(err_row, err_col_start, err_col_end) in &error_regions {
                if err_row == abs_row {
                    styled_line = apply_error_style(styled_line, err_col_start, err_col_end);
                }
            }

            if let Some(((sel_start_row, sel_start_col), (sel_end_row, sel_end_col))) = selection
                && abs_row >= sel_start_row
                && abs_row <= sel_end_row
            {
                // This line is (partially) selected
                let line_len = line.chars().count();
                let sel_start = if abs_row == sel_start_row {
                    sel_start_col
                } else {
                    0
                };
                let sel_end = if abs_row == sel_end_row {
                    sel_end_col
                } else {
                    line_len
                };
                return apply_selection_style(styled_line, sel_start, sel_end);
            }

            styled_line
        })
        .collect();

    #[allow(clippy::cast_possible_truncation)] // scroll_col bounded by terminal width
    let paragraph = Paragraph::new(highlighted_lines)
        .block(block)
        .scroll((0, scroll_col as u16));

    frame.render_widget(paragraph, area);

    // Render vertical scrollbar when content exceeds visible area
    let total_lines = tab.editor.lines.len();
    if total_lines > visible_rows {
        let mut scrollbar_state = ScrollbarState::new(total_lines).position(scroll_row);

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
}

/// Apply selection background to a highlighted line within the given char column range.
fn apply_selection_style(line: Line<'static>, sel_start: usize, sel_end: usize) -> Line<'static> {
    if sel_start >= sel_end {
        return line;
    }

    let mut result: Vec<Span<'static>> = Vec::new();
    let mut col = 0; // char offset

    for span in line.spans {
        let span_chars = span.content.chars().count();
        let span_end = col + span_chars;

        if span_end <= sel_start || col >= sel_end {
            // Entirely outside selection
            result.push(span);
        } else if col >= sel_start && span_end <= sel_end {
            // Entirely inside selection
            result.push(Span::styled(
                span.content,
                span.style.bg(SELECTION_STYLE.bg.unwrap_or(Color::DarkGray)),
            ));
        } else {
            // Partially overlapping — split the span by char offset
            let text = span.content.to_string();
            let rel_start = sel_start.saturating_sub(col);
            let rel_end = sel_end.saturating_sub(col).min(span_chars);

            // Convert char offsets to byte offsets for slicing
            let byte_start = char_to_byte(&text, rel_start);
            let byte_end = char_to_byte(&text, rel_end);

            if byte_start > 0 {
                result.push(Span::styled(text[..byte_start].to_owned(), span.style));
            }
            result.push(Span::styled(
                text[byte_start..byte_end].to_owned(),
                span.style.bg(SELECTION_STYLE.bg.unwrap_or(Color::DarkGray)),
            ));
            if byte_end < text.len() {
                result.push(Span::styled(text[byte_end..].to_owned(), span.style));
            }
        }

        col = span_end;
    }

    Line::from(result)
}

/// Convert a char offset to a byte offset within a string.
fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map_or(s.len(), |(byte_idx, _)| byte_idx)
}

/// Convert validation error byte spans to per-line `(row, col_start, col_end)` char regions.
fn compute_error_regions(
    lines: &[String],
    errors: &[trawl_core::parser::ParseError],
) -> Vec<(usize, usize, usize)> {
    if errors.is_empty() {
        return Vec::new();
    }

    // Build a byte-offset-to-(row, char_col) mapping.
    let mut regions = Vec::new();
    for error in errors {
        let span_start = error.span.start;
        let span_end = error.span.end;

        // Walk lines to find which rows the span covers.
        let mut byte_offset = 0;
        for (row, line) in lines.iter().enumerate() {
            let line_byte_start = byte_offset;
            // +1 for the newline character (except last line)
            let line_byte_end = byte_offset + line.len();

            // Check if this line overlaps with the error span.
            if line_byte_end > span_start && line_byte_start < span_end {
                // Compute char-column range within this line.
                let byte_start_in_line = span_start.saturating_sub(line_byte_start);
                let byte_end_in_line = (span_end - line_byte_start).min(line.len());

                // Convert byte offsets to char offsets.
                let col_start = line[..byte_start_in_line].chars().count();
                let col_end = line[..byte_end_in_line].chars().count();

                if col_start < col_end {
                    regions.push((row, col_start, col_end));
                } else {
                    // Zero-width span: highlight at least 1 char.
                    regions.push((row, col_start, col_start + 1));
                }
            }

            // +1 for the newline separator between lines.
            byte_offset = line_byte_end + 1;
        }
    }

    regions
}

/// Apply error styling (red underline) to a line within the given char column range.
fn apply_error_style(line: Line<'static>, err_start: usize, err_end: usize) -> Line<'static> {
    if err_start >= err_end {
        return line;
    }

    let mut result: Vec<Span<'static>> = Vec::new();
    let mut col = 0;

    for span in line.spans {
        let span_chars = span.content.chars().count();
        let span_end = col + span_chars;

        if span_end <= err_start || col >= err_end {
            // Entirely outside error region
            result.push(span);
        } else if col >= err_start && span_end <= err_end {
            // Entirely inside error region
            result.push(Span::styled(
                span.content,
                span.style
                    .fg(ERROR_STYLE.fg.unwrap_or(Color::Red))
                    .add_modifier(Modifier::UNDERLINED),
            ));
        } else {
            // Partially overlapping — split the span
            let text = span.content.to_string();
            let rel_start = err_start.saturating_sub(col);
            let rel_end = err_end.saturating_sub(col).min(span_chars);

            let byte_start = char_to_byte(&text, rel_start);
            let byte_end = char_to_byte(&text, rel_end);

            if byte_start > 0 {
                result.push(Span::styled(text[..byte_start].to_owned(), span.style));
            }
            result.push(Span::styled(
                text[byte_start..byte_end].to_owned(),
                span.style
                    .fg(ERROR_STYLE.fg.unwrap_or(Color::Red))
                    .add_modifier(Modifier::UNDERLINED),
            ));
            if byte_end < text.len() {
                result.push(Span::styled(text[byte_end..].to_owned(), span.style));
            }
        }

        col = span_end;
    }

    Line::from(result)
}

/// Render a one-line validation hint bar showing the first error message.
pub fn render_validation_hint(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let tab = app.active_tab();
    if let Some(error) = tab.validation_errors.first() {
        let mut spans = vec![
            Span::styled(
                " ! ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::styled(&error.message, Style::default().fg(Color::DarkGray)),
        ];

        if let Some(ref hint) = error.hint {
            spans.push(Span::styled(
                format!("  ({hint})"),
                Style::default().fg(Color::Yellow),
            ));
        }

        let line = Line::from(spans);
        frame.render_widget(Paragraph::new(line), area);
    }
}
