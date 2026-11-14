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
use trawl_core::advice::AdviceReport;
use trawl_core::sanitize::sanitize_display_text;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

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
    // Sanitized once here; both build paths splice this copy.
    let ghost_text = tab
        .ghost
        .as_ref()
        .map(|g| sanitize_display_text(&g.ghost_text));

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
            Span::styled(
                sanitize_display_text(&error.message),
                Style::default().fg(theme.text_muted),
            ),
        ];

        if let Some(ref hint) = error.hint {
            spans.push(Span::styled(
                format!("  ({})", sanitize_display_text(hint)),
                Style::default().fg(theme.status_warning),
            ));
        }

        let line = Line::from(spans);
        frame.render_widget(Paragraph::new(line), area);
    }
}

/// What one row of the advice region shows, which picks its style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdviceRowKind {
    /// A finding's message (its first row carries the marker).
    Message,
    /// A finding's hint.
    Hint,
    /// How many findings were left out, or that rows were cut.
    Note,
}

/// One terminal row of the advice region: its text is sanitized and fits
/// the width it was laid out for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdviceRow {
    pub kind: AdviceRowKind,
    pub text: String,
}

/// The marker in front of a finding's first row.
const ADVICE_MARKER: &str = " i ";
/// Where the message and hint text start, so wrapped rows line up.
const ADVICE_INDENT: &str = "   ";

/// Lay out `report` as rows `width` cells wide, at most `max_rows` of
/// them. The region's height is the number of rows this returns, and the
/// renderer draws exactly these rows, so measurement and drawing agree.
///
/// Every message and hint is sanitized before it is measured. Text wraps
/// at whitespace by display width; a word wider than the row breaks at
/// grapheme boundaries. When the rows do not fit `max_rows`, the last one
/// kept says so.
#[must_use]
pub fn advice_rows(report: &AdviceReport, width: u16, max_rows: u16) -> Vec<AdviceRow> {
    let width = usize::from(width);
    let max_rows = usize::from(max_rows);
    if width == 0 || max_rows == 0 {
        return Vec::new();
    }

    let mut rows = Vec::new();
    for finding in &report.findings {
        push_wrapped(
            &mut rows,
            AdviceRowKind::Message,
            ADVICE_MARKER,
            &format!("advice: {}", sanitize_display_text(&finding.message)),
            width,
        );
        push_wrapped(
            &mut rows,
            AdviceRowKind::Hint,
            ADVICE_INDENT,
            &format!("hint: {}", sanitize_display_text(&finding.hint)),
            width,
        );
    }
    if report.omitted > 0 {
        let noun = if report.omitted == 1 { "note" } else { "notes" };
        push_wrapped(
            &mut rows,
            AdviceRowKind::Note,
            ADVICE_INDENT,
            &format!("and {} more advice {noun} not shown", report.omitted),
            width,
        );
    }

    if rows.len() > max_rows {
        rows.truncate(max_rows);
        if let Some(last) = rows.last_mut() {
            *last = AdviceRow {
                kind: AdviceRowKind::Note,
                text: fit_cells(&format!("{ADVICE_INDENT}… more advice below"), width),
            };
        }
    }
    rows
}

/// Wrap `body` into rows of `width` cells, the first starting with
/// `first_prefix` and the rest indented to line up under it. A row too
/// narrow for the prefix drops it.
fn push_wrapped(
    rows: &mut Vec<AdviceRow>,
    kind: AdviceRowKind,
    first_prefix: &str,
    body: &str,
    width: usize,
) {
    let prefix_width = UnicodeWidthStr::width(first_prefix);
    let (first_prefix, indent) = if width > prefix_width {
        (first_prefix, " ".repeat(prefix_width))
    } else {
        ("", String::new())
    };
    let room = width - UnicodeWidthStr::width(indent.as_str());

    for (index, line) in wrap_cells(body, room).into_iter().enumerate() {
        let prefix = if index == 0 {
            first_prefix
        } else {
            indent.as_str()
        };
        rows.push(AdviceRow {
            kind,
            text: format!("{prefix}{line}"),
        });
    }
}

/// Greedy word wrap of `text` into lines at most `width` display cells.
/// `width` is at least 1.
fn wrap_cells(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut line_width = 0;
    for word in text.split_whitespace() {
        let word_width = UnicodeWidthStr::width(word);
        let gap = usize::from(!line.is_empty());
        if line_width + gap + word_width <= width {
            if gap == 1 {
                line.push(' ');
            }
            line.push_str(word);
            line_width += gap + word_width;
            continue;
        }
        if !line.is_empty() {
            lines.push(std::mem::take(&mut line));
            line_width = 0;
        }
        if word_width <= width {
            line.push_str(word);
            line_width = word_width;
            continue;
        }
        // A word wider than the row breaks between graphemes.
        for cluster in word.graphemes(true) {
            let cluster_width = UnicodeWidthStr::width(cluster);
            if line_width + cluster_width > width && !line.is_empty() {
                lines.push(std::mem::take(&mut line));
                line_width = 0;
            }
            line.push_str(cluster);
            line_width += cluster_width;
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// The longest prefix of `text` that fits `width` cells.
fn fit_cells(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for cluster in text.graphemes(true) {
        let cluster_width = UnicodeWidthStr::width(cluster);
        if used + cluster_width > width {
            break;
        }
        out.push_str(cluster);
        used += cluster_width;
    }
    out
}

/// Render the advice region: the rows [`advice_rows`] laid out for this
/// area, which the caller sized from the same call.
pub fn render_query_advice(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let Some(report) = app.active_tab().draft_advice() else {
        return;
    };
    let theme = &app.theme;
    let lines: Vec<Line<'_>> = advice_rows(report, area.width, area.height)
        .into_iter()
        .map(|row| match row.kind {
            AdviceRowKind::Message => {
                if let Some(rest) = row.text.strip_prefix(ADVICE_MARKER) {
                    Line::from(vec![
                        Span::styled(
                            ADVICE_MARKER,
                            Style::default()
                                .fg(theme.status_info)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(rest.to_owned(), Style::default().fg(theme.text_muted)),
                    ])
                } else {
                    Line::styled(row.text, Style::default().fg(theme.text_muted))
                }
            }
            AdviceRowKind::Hint => {
                Line::styled(row.text, Style::default().fg(theme.status_warning))
            }
            AdviceRowKind::Note => Line::styled(
                row.text,
                Style::default()
                    .fg(theme.text_muted)
                    .add_modifier(Modifier::DIM),
            ),
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use unicode_width::UnicodeWidthStr;

    use super::{AdviceRowKind, advice_rows};
    use crate::tui::App;
    use crate::tui::tests::test_app;

    /// An app whose query draft is `text`, validated past the debounce.
    fn app_with_draft(text: &str) -> App {
        let mut app = test_app();
        app.tab.editor.insert_text(text);
        app.tab.mark_editor_dirty();
        app.tab.last_edit_time = None;
        assert!(app.tab.maybe_validate());
        app
    }

    fn draw(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| crate::tui::ui::render(app, f)).unwrap();
        terminal.backend().to_string()
    }

    #[test]
    fn query_advice_wraps_at_80() {
        let mut app = app_with_draft(r#""10.244.0.83" -service=coredns"#);
        let screen = draw(&mut app, 80, 24);
        assert!(
            screen.contains("advice: -service=… is a text search"),
            "{screen}"
        );
        assert!(screen.contains("hint: to filter the field"));
        insta::assert_snapshot!(screen);
    }

    #[test]
    fn query_advice_yields_to_errors() {
        let mut app = app_with_draft("service=trawld event_type in (a, b)");
        assert!(app.tab.advice.is_some(), "advice is stored");
        app.tab.validation_errors = vec![trawl_core::parser::ParseError {
            message: "unknown command 'staats'".to_owned(),
            span: 0..1,
            label: None,
            hint: Some("did you mean 'stats'?".to_owned()),
        }];
        let screen = draw(&mut app, 80, 24);
        assert!(screen.contains("unknown command 'staats'"));
        assert!(
            !screen.contains("advice:"),
            "errors take precedence:\n{screen}"
        );
        insta::assert_snapshot!(screen);
    }

    #[test]
    fn query_advice_layout_handles_narrow_and_zero_areas() {
        let app = app_with_draft(r#""10.244.0.83" -service=coredns"#);
        let report = app.tab.draft_advice().expect("advice").clone();

        assert!(advice_rows(&report, 0, 10).is_empty(), "zero width");
        assert!(advice_rows(&report, 80, 0).is_empty(), "zero height");

        for width in 1..=12 {
            let rows = advice_rows(&report, width, u16::MAX);
            assert!(!rows.is_empty(), "width {width} lays out rows");
            for row in &rows {
                assert!(
                    UnicodeWidthStr::width(row.text.as_str()) <= usize::from(width),
                    "row {:?} overflows width {width}",
                    row.text
                );
            }
        }

        // A short budget keeps that many rows and says some were cut.
        let rows = advice_rows(&report, 40, 3);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[2].kind, AdviceRowKind::Note);
        assert!(rows[2].text.contains("more advice below"));

        // Whole frames, tiny and empty, draw without panicking; a frame too
        // short for any advice row leaves the region out.
        for (width, height) in [(1, 1), (3, 3), (20, 6), (12, 30)] {
            let mut app = app_with_draft(r#""10.244.0.83" -service=coredns"#);
            let _ = draw(&mut app, width, height);
        }
        // The region itself, handed an empty area, draws nothing.
        let mut terminal = Terminal::new(TestBackend::new(10, 5)).unwrap();
        terminal
            .draw(|f| super::render_query_advice(&app, f, ratatui::layout::Rect::default()))
            .unwrap();
        assert_eq!(
            terminal
                .backend()
                .to_string()
                .trim_matches([' ', '"', '\n']),
            ""
        );
        let mut app = app_with_draft(r#""10.244.0.83" -service=coredns"#);
        let screen = draw(&mut app, 40, 9);
        assert!(!screen.contains("advice"), "no room for advice:\n{screen}");
    }

    #[test]
    fn query_advice_reports_omitted_findings() {
        let draft = (0..10)
            .map(|i| format!("-f{i}=v"))
            .collect::<Vec<_>>()
            .join(" ");
        let app = app_with_draft(&draft);
        let report = app.tab.draft_advice().expect("advice").clone();
        assert_eq!(
            report.findings.len(),
            trawl_core::advice::MAX_ADVICE_FINDINGS
        );
        assert_eq!(report.omitted, 2);

        let rows = advice_rows(&report, 200, u16::MAX);
        let last = rows.last().expect("rows");
        assert_eq!(last.kind, AdviceRowKind::Note);
        assert_eq!(last.text.trim(), "and 2 more advice notes not shown");
        assert_eq!(
            rows.iter()
                .filter(|r| r.kind == AdviceRowKind::Message && r.text.starts_with(" i "))
                .count(),
            trawl_core::advice::MAX_ADVICE_FINDINGS
        );
    }
}
