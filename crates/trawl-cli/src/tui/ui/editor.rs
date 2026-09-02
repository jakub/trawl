// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Query editor pane with soft word wrapping.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Padding, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};

use crate::tui::App;
use crate::tui::highlight::Highlighter;
use crate::tui::state::{Focus, WrapMap};

/// Render the editor pane.
#[allow(clippy::too_many_lines)] // Block chrome, wrap map, overlay inputs and scrollbar
pub fn render(app: &mut App, frame: &mut Frame<'_>, area: Rect) {
    let theme = &app.theme;

    let border_style = if app.focus == Focus::Editor {
        Style::default().fg(theme.border_focused)
    } else {
        Style::default().fg(theme.border_unfocused)
    };

    // Copied out before the mutable tab borrow below, then passed to the build helpers.
    let selection_bg = theme.surface_highlight;
    let error_fg = theme.status_error;
    let ghost_style = Style::new()
        .fg(theme.text_muted)
        .add_modifier(Modifier::DIM);
    let indent_style = Style::new()
        .fg(theme.text_muted)
        .add_modifier(Modifier::DIM);

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

    // Visible area inside the block. Borders cost 2 rows and 2 cols; horizontal
    // padding another 2 cols.
    let visible_rows = area.height.saturating_sub(2) as usize;
    let visible_cols = area.width.saturating_sub(4) as usize;

    let tab = app.active_tab_mut();
    tab.editor.update_wrap_map(visible_cols);

    tab.editor.ensure_cursor_visible(visible_rows, visible_cols);

    let tab = app.active_tab();
    let scroll_row = tab.editor.scroll_row; // in visual-line units when wrapping
    let selection = tab.editor.selection_range();

    let highlighter = Highlighter::new(app.schema_cache.as_ref(), &app.theme.syntax);

    let error_regions = compute_error_regions(&tab.editor.lines, &tab.validation_errors);

    let cursor_row = tab.editor.cursor.0;
    let cursor_col = tab.editor.cursor.1;
    let ghost_text = tab.ghost.as_ref().map(|g| g.ghost_text.clone());

    // Build visual lines from logical lines using the wrap map.
    let visual_lines: Vec<Line<'static>> = if let Some(ref wm) = tab.editor.wrap_map {
        build_wrapped_lines(
            &tab.editor.lines,
            wm,
            scroll_row,
            visible_rows,
            &highlighter,
            &error_regions,
            selection,
            cursor_row,
            cursor_col,
            ghost_text.as_deref(),
            ghost_style,
            selection_bg,
            error_fg,
            indent_style,
        )
    } else {
        // Only a single-line editor skips the wrap map, so the tab editor never
        // reaches this branch.
        build_unwrapped_lines(
            &tab.editor.lines,
            scroll_row,
            visible_rows,
            &highlighter,
            &error_regions,
            selection,
            cursor_row,
            cursor_col,
            ghost_text.as_deref(),
            ghost_style,
            selection_bg,
            error_fg,
        )
    };

    let paragraph = Paragraph::new(visual_lines).block(block);
    frame.render_widget(paragraph, area);

    let total_visual = tab
        .editor
        .wrap_map
        .as_ref()
        .map_or(tab.editor.lines.len(), WrapMap::total_visual_lines);

    if total_visual > visible_rows {
        let mut scrollbar_state = ScrollbarState::new(total_visual).position(scroll_row);

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

/// Build visual lines with wrapping applied.
///
/// Iterates logical lines, highlights each, applies overlays, then splits
/// at wrap break points. Only returns the visual lines visible in the
/// current scroll window.
#[allow(clippy::too_many_arguments)]
fn build_wrapped_lines(
    lines: &[String],
    wm: &WrapMap,
    scroll_row: usize,
    visible_rows: usize,
    highlighter: &Highlighter<'_>,
    error_regions: &[(usize, usize, usize)],
    selection: Option<((usize, usize), (usize, usize))>,
    cursor_row: usize,
    cursor_col: usize,
    ghost_text: Option<&str>,
    ghost_style: Style,
    selection_bg: Color,
    error_fg: Color,
    indent_style: Style,
) -> Vec<Line<'static>> {
    let end_vrow = scroll_row + visible_rows;
    let mut result = Vec::with_capacity(visible_rows);
    let mut vrow_offset = 0; // running visual row counter

    for (logical_row, line) in lines.iter().enumerate() {
        let breaks = wm.breaks_for(logical_row);
        let num_visual = breaks.len();
        let vrow_start = vrow_offset;
        let vrow_end = vrow_offset + num_visual;

        // Skip logical lines entirely above or below the viewport.
        if vrow_end <= scroll_row || vrow_start >= end_vrow {
            vrow_offset = vrow_end;
            continue;
        }

        let mut styled_line = highlighter.highlight_line(line);

        for &(err_row, err_col_start, err_col_end) in error_regions {
            if err_row == logical_row {
                styled_line = apply_error_style(styled_line, err_col_start, err_col_end, error_fg);
            }
        }

        if let Some(((sel_start_row, sel_start_col), (sel_end_row, sel_end_col))) = selection
            && logical_row >= sel_start_row
            && logical_row <= sel_end_row
        {
            let line_len = line.chars().count();
            let sel_start = if logical_row == sel_start_row {
                sel_start_col
            } else {
                0
            };
            let sel_end = if logical_row == sel_end_row {
                sel_end_col
            } else {
                line_len
            };
            styled_line = apply_selection_style(styled_line, sel_start, sel_end, selection_bg);
        } else if logical_row == cursor_row
            && let Some(ghost) = ghost_text
        {
            // Ghost text (only when no selection active on this line).
            styled_line = splice_ghost_text(styled_line, cursor_col, ghost, ghost_style);
        }

        let visual_lines = split_line_at_wraps(
            styled_line,
            breaks,
            WrapMap::continuation_indent(),
            indent_style,
        );

        for (seg_idx, vline) in visual_lines.into_iter().enumerate() {
            let abs_vrow = vrow_start + seg_idx;
            if abs_vrow >= scroll_row && abs_vrow < end_vrow {
                result.push(vline);
            }
            if result.len() >= visible_rows {
                return result;
            }
        }

        vrow_offset = vrow_end;
    }

    result
}

/// Build visual lines without wrapping, for the no-wrap-map fallback.
#[allow(clippy::too_many_arguments)]
fn build_unwrapped_lines(
    lines: &[String],
    scroll_row: usize,
    visible_rows: usize,
    highlighter: &Highlighter<'_>,
    error_regions: &[(usize, usize, usize)],
    selection: Option<((usize, usize), (usize, usize))>,
    cursor_row: usize,
    cursor_col: usize,
    ghost_text: Option<&str>,
    ghost_style: Style,
    selection_bg: Color,
    error_fg: Color,
) -> Vec<Line<'static>> {
    let end_row = (scroll_row + visible_rows).min(lines.len());
    lines[scroll_row..end_row]
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let abs_row = scroll_row + i;
            let mut styled_line = highlighter.highlight_line(line);

            for &(err_row, err_col_start, err_col_end) in error_regions {
                if err_row == abs_row {
                    styled_line =
                        apply_error_style(styled_line, err_col_start, err_col_end, error_fg);
                }
            }

            if let Some(((sel_start_row, sel_start_col), (sel_end_row, sel_end_col))) = selection
                && abs_row >= sel_start_row
                && abs_row <= sel_end_row
            {
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
                return apply_selection_style(styled_line, sel_start, sel_end, selection_bg);
            }

            if abs_row == cursor_row
                && let Some(ghost) = ghost_text
            {
                styled_line = splice_ghost_text(styled_line, cursor_col, ghost, ghost_style);
            }

            styled_line
        })
        .collect()
}

/// Split a highlighted line into multiple visual lines at wrap break offsets.
///
/// `breaks` is a slice of char offsets (always starting with 0).
/// The first visual line renders as-is. Continuation lines get `indent`
/// spaces prepended in `indent_style`.
fn split_line_at_wraps(
    line: Line<'static>,
    breaks: &[usize],
    indent: usize,
    indent_style: Style,
) -> Vec<Line<'static>> {
    // Single visual line (no wrapping needed).
    if breaks.len() <= 1 {
        return vec![line];
    }

    let mut visual_lines = Vec::with_capacity(breaks.len());
    let spans = line.spans;

    // For each segment between consecutive breaks, extract the relevant spans.
    for (seg_idx, window) in breaks.windows(2).enumerate() {
        let seg_start = window[0];
        let seg_end = window[1];
        let mut seg_spans = extract_span_range(&spans, seg_start, seg_end);
        if seg_idx > 0 {
            let indent_span = Span::styled(" ".repeat(indent), indent_style);
            seg_spans.insert(0, indent_span);
        }
        visual_lines.push(Line::from(seg_spans));
    }

    // Last segment: from last break to end of line.
    let last_break = *breaks.last().unwrap_or(&0);
    let mut last_spans = extract_span_range(&spans, last_break, usize::MAX);
    if breaks.len() > 1 {
        let indent_span = Span::styled(" ".repeat(indent), indent_style);
        last_spans.insert(0, indent_span);
    }
    visual_lines.push(Line::from(last_spans));

    visual_lines
}

/// Extract spans covering the char range `[range_start, range_end)` from a span list.
///
/// Splits spans that straddle the range boundaries. Returns owned spans.
fn extract_span_range(
    spans: &[Span<'static>],
    range_start: usize,
    range_end: usize,
) -> Vec<Span<'static>> {
    let mut result = Vec::new();
    let mut col = 0usize;

    for span in spans {
        let span_chars = span.content.chars().count();
        let span_end = col + span_chars;

        if span_end <= range_start || col >= range_end {
            // Entirely outside range — skip.
            col = span_end;
            continue;
        }

        // Compute overlap in char offsets relative to this span.
        let rel_start = range_start.saturating_sub(col);
        let rel_end = range_end.saturating_sub(col).min(span_chars);

        if rel_start == 0 && rel_end >= span_chars {
            // Entire span is within range.
            result.push(span.clone());
        } else {
            // Partial span — slice by char offsets.
            let text = span.content.as_ref();
            let byte_start = char_to_byte(text, rel_start);
            let byte_end = char_to_byte(text, rel_end);
            if byte_start < byte_end {
                result.push(Span::styled(
                    text[byte_start..byte_end].to_owned(),
                    span.style,
                ));
            }
        }

        col = span_end;
    }

    result
}

/// Apply selection background to a highlighted line within the given char column range.
fn apply_selection_style(
    line: Line<'static>,
    sel_start: usize,
    sel_end: usize,
    bg: Color,
) -> Line<'static> {
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
            result.push(Span::styled(span.content, span.style.bg(bg)));
        } else {
            // Partially overlapping — split the span by char offset
            let text = span.content.to_string();
            let rel_start = sel_start.saturating_sub(col);
            let rel_end = sel_end.saturating_sub(col).min(span_chars);

            let byte_start = char_to_byte(&text, rel_start);
            let byte_end = char_to_byte(&text, rel_end);

            if byte_start > 0 {
                result.push(Span::styled(text[..byte_start].to_owned(), span.style));
            }
            result.push(Span::styled(
                text[byte_start..byte_end].to_owned(),
                span.style.bg(bg),
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
///
/// A char offset past the end yields `s.len()`, which is how callers slice
/// through to the end of a span.
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

    let mut regions = Vec::new();
    for error in errors {
        let span_start = error.span.start;
        let span_end = error.span.end;

        // Walk lines to find which rows the span covers.
        let mut byte_offset = 0;
        for (row, line) in lines.iter().enumerate() {
            let line_byte_start = byte_offset;
            let line_byte_end = byte_offset + line.len();

            if line_byte_end > span_start && line_byte_start < span_end {
                // Char-column range of the overlap within this line.
                let byte_start_in_line = span_start.saturating_sub(line_byte_start);
                let byte_end_in_line = (span_end - line_byte_start).min(line.len());

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
fn apply_error_style(
    line: Line<'static>,
    err_start: usize,
    err_end: usize,
    fg: Color,
) -> Line<'static> {
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
                span.style.fg(fg).add_modifier(Modifier::UNDERLINED),
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
                span.style.fg(fg).add_modifier(Modifier::UNDERLINED),
            ));
            if byte_end < text.len() {
                result.push(Span::styled(text[byte_end..].to_owned(), span.style));
            }
        }

        col = span_end;
    }

    Line::from(result)
}

/// Splice a ghost-text span into a line at the given char column.
///
/// Splits the existing spans at the cursor position and inserts a dimmed
/// ghost text span. The ghost text extends the visual line but doesn't
/// affect cursor positioning.
fn splice_ghost_text(
    line: Line<'static>,
    cursor_col: usize,
    ghost: &str,
    ghost_style: Style,
) -> Line<'static> {
    let mut result: Vec<Span<'static>> = Vec::new();
    let mut col = 0;
    let mut inserted = false;

    for span in line.spans {
        let span_chars = span.content.chars().count();
        let span_end = col + span_chars;

        if !inserted && cursor_col >= col && cursor_col <= span_end {
            // Cursor is inside (or at boundary of) this span — split it.
            let rel = cursor_col - col;
            let text = span.content.to_string();
            let byte_split = char_to_byte(&text, rel);

            if byte_split > 0 {
                result.push(Span::styled(text[..byte_split].to_owned(), span.style));
            }
            result.push(Span::styled(ghost.to_owned(), ghost_style));
            if byte_split < text.len() {
                result.push(Span::styled(text[byte_split..].to_owned(), span.style));
            }
            inserted = true;
        } else {
            result.push(span);
        }

        col = span_end;
    }

    // If cursor is past all spans (at the very end of the line), append ghost.
    if !inserted {
        result.push(Span::styled(ghost.to_owned(), ghost_style));
    }

    Line::from(result)
}

/// Render a one-line validation hint bar showing the first error message.
pub fn render_validation_hint(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let tab = app.active_tab();
    let theme = &app.theme;
    if let Some(error) = tab.validation_errors.first() {
        let mut spans = vec![
            Span::styled(
                " ! ",
                Style::default()
                    .fg(theme.status_error)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(&error.message, Style::default().fg(theme.text_muted)),
        ];

        if let Some(ref hint) = error.hint {
            spans.push(Span::styled(
                format!("  ({hint})"),
                Style::default().fg(theme.status_warning),
            ));
        }

        let line = Line::from(spans);
        frame.render_widget(Paragraph::new(line), area);
    }
}
