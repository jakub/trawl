// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

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

#[allow(clippy::too_many_lines)]
pub fn render(app: &mut App, frame: &mut Frame<'_>, area: Rect) {
    let theme = &app.theme;

    let border_style = if app.focus == Focus::Editor {
        Style::default().fg(theme.border_focused)
    } else {
        Style::default().fg(theme.border_unfocused)
    };

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
    let ghost_text = tab
        .ghost
        .as_ref()
        .map(|g| sanitize_display_text(&g.ghost_text));

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
    let mut vrow_offset = 0;

    for (logical_row, line) in lines.iter().enumerate() {
        let breaks = wm.breaks_for(logical_row);
        let num_visual = breaks.len();
        let vrow_start = vrow_offset;
        let vrow_end = vrow_offset + num_visual;

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

/// `breaks` is a slice of char offsets (always starting with 0).
fn split_line_at_wraps(
    line: Line<'static>,
    breaks: &[usize],
    indent: usize,
    indent_style: Style,
) -> Vec<Line<'static>> {
    if breaks.len() <= 1 {
        return vec![line];
    }

    let mut visual_lines = Vec::with_capacity(breaks.len());
    let spans = line.spans;

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
            col = span_end;
            continue;
        }

        let rel_start = range_start.saturating_sub(col);
        let rel_end = range_end.saturating_sub(col).min(span_chars);

        if rel_start == 0 && rel_end >= span_chars {
            result.push(span.clone());
        } else {
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
    let mut col = 0;

    for span in line.spans {
        let span_chars = span.content.chars().count();
        let span_end = col + span_chars;

        if span_end <= sel_start || col >= sel_end {
            result.push(span);
        } else if col >= sel_start && span_end <= sel_end {
            result.push(Span::styled(span.content, span.style.bg(bg)));
        } else {
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

        let mut byte_offset = 0;
        for (row, line) in lines.iter().enumerate() {
            let line_byte_start = byte_offset;
            let line_byte_end = byte_offset + line.len();

            if line_byte_end > span_start && line_byte_start < span_end {
                let byte_start_in_line = span_start.saturating_sub(line_byte_start);
                let byte_end_in_line = (span_end - line_byte_start).min(line.len());

                let col_start = line[..byte_start_in_line].chars().count();
                let col_end = line[..byte_end_in_line].chars().count();

                if col_start < col_end {
                    regions.push((row, col_start, col_end));
                } else {
                    regions.push((row, col_start, col_start + 1));
                }
            }

            // +1 for the newline separator between lines.
            byte_offset = line_byte_end + 1;
        }
    }

    regions
}

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
            result.push(span);
        } else if col >= err_start && span_end <= err_end {
            result.push(Span::styled(
                span.content,
                span.style.fg(fg).add_modifier(Modifier::UNDERLINED),
            ));
        } else {
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

    if !inserted {
        result.push(Span::styled(ghost.to_owned(), ghost_style));
    }

    Line::from(result)
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdviceRowKind {
    Message,
    Hint,
    Note,
}

/// One terminal row of the advice region: its text is sanitized and fits
/// the width it was laid out for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdviceRow {
    pub kind: AdviceRowKind,
    pub text: String,
}

const ADVICE_MARKER: &str = " i ";
const ADVICE_INDENT: &str = "   ";

/// Lay out `report` as rows `width` cells wide, at most `max_rows` of
/// them. The region's height is the number of rows this returns, and the
/// renderer draws exactly these rows, so measurement and drawing agree.
#[must_use]
pub fn advice_rows(report: &AdviceReport, width: u16, max_rows: u16) -> Vec<AdviceRow> {
    let width = usize::from(width);
    let max_rows = usize::from(max_rows);
    if width == 0 || max_rows == 0 {
        return Vec::new();
    }

    let groups: Vec<Vec<AdviceRow>> = report
        .findings
        .iter()
        .map(|finding| {
            let mut group = Vec::new();
            push_wrapped(
                &mut group,
                AdviceRowKind::Message,
                ADVICE_MARKER,
                &format!("advice: {}", sanitize_display_text(&finding.message)),
                width,
            );
            push_wrapped(
                &mut group,
                AdviceRowKind::Hint,
                ADVICE_INDENT,
                &format!("hint: {}", sanitize_display_text(&finding.hint)),
                width,
            );
            group
        })
        .collect();

    let mut rows: Vec<AdviceRow> = Vec::new();
    let note_rows = |hidden: usize, shown: usize| {
        let mut note = Vec::new();
        if hidden > 0 {
            push_wrapped(
                &mut note,
                AdviceRowKind::Note,
                ADVICE_INDENT,
                &hidden_note(hidden, shown),
                width,
            );
        }
        note
    };
    let total: usize = groups.iter().map(Vec::len).sum::<usize>()
        + note_rows(report.omitted, report.findings.len()).len();
    if total <= max_rows {
        rows.extend(groups.into_iter().flatten());
        rows.extend(note_rows(report.omitted, report.findings.len()));
        return rows;
    }

    let room = max_rows - 1;
    let mut shown = 0;
    for group in groups {
        if rows.len() + group.len() > room {
            break;
        }
        rows.extend(group);
        shown += 1;
    }
    let hidden = report.findings.len() - shown + report.omitted;
    let note = format!(
        "{ADVICE_INDENT}{}; a taller terminal shows more",
        hidden_note(hidden, shown)
    );
    rows.push(AdviceRow {
        kind: AdviceRowKind::Note,
        text: fit_cells(&note, width),
    });
    rows
}

fn hidden_note(hidden: usize, shown: usize) -> String {
    let noun = if hidden == 1 { "note" } else { "notes" };
    if shown == 0 {
        format!("{hidden} advice {noun} not shown")
    } else {
        format!("and {hidden} more advice {noun} not shown")
    }
}

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

        let rows = advice_rows(&report, 80, 1);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, AdviceRowKind::Note);
        assert_eq!(
            rows[0].text.trim(),
            "1 advice note not shown; a taller terminal shows more"
        );

        for (width, height) in [(1, 1), (3, 3), (20, 6), (12, 30)] {
            let mut app = app_with_draft(r#""10.244.0.83" -service=coredns"#);
            let _ = draw(&mut app, width, height);
        }
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
    fn query_advice_counts_what_the_region_cuts() {
        let draft = (0..10)
            .map(|i| format!("-f{i}=v"))
            .collect::<Vec<_>>()
            .join(" ");
        let mut app = app_with_draft(&draft);
        let report = app.tab.draft_advice().expect("advice").clone();
        assert_eq!(report.omitted, 2);

        let budget = 8;
        let rows = advice_rows(&report, 80, budget);
        assert!(rows.len() <= usize::from(budget));
        let shown = rows
            .iter()
            .filter(|r| r.kind == AdviceRowKind::Message && r.text.starts_with(" i "))
            .count();
        assert!(shown >= 1, "{rows:?}");
        let last = rows.last().expect("rows");
        assert_eq!(last.kind, AdviceRowKind::Note);
        let hidden = report.findings.len() - shown + report.omitted;
        assert_eq!(
            last.text.trim(),
            format!("and {hidden} more advice notes not shown; a taller terminal shows more")
        );
        assert!(rows.iter().all(|r| !r.text.contains("below")), "{rows:?}");

        let screen = draw(&mut app, 80, 24);
        assert!(
            screen.contains(&format!("and {hidden} more advice notes not shown")),
            "{screen}"
        );
    }

    #[test]
    fn query_advice_never_cuts_a_finding_silently() {
        let many = (0..10)
            .map(|i| format!("-f{i}=v"))
            .collect::<Vec<_>>()
            .join(" ");
        for draft in ["-service=coredns", many.as_str()] {
            let app = app_with_draft(draft);
            let report = app.tab.draft_advice().expect("advice").clone();
            for width in [20, 40, 80, 200] {
                let unlimited: Vec<_> = advice_rows(&report, width, u16::MAX)
                    .into_iter()
                    .filter(|r| r.kind != AdviceRowKind::Note)
                    .collect();
                for budget in 1..=30 {
                    let rows = advice_rows(&report, width, budget);
                    assert!(rows.len() <= usize::from(budget));
                    let shown: Vec<_> = rows
                        .iter()
                        .filter(|r| r.kind != AdviceRowKind::Note)
                        .cloned()
                        .collect();
                    assert!(
                        unlimited.starts_with(&shown),
                        "width {width} budget {budget}: {rows:?}"
                    );
                    if let Some(next) = unlimited.get(shown.len()) {
                        assert!(
                            next.kind == AdviceRowKind::Message
                                && next.text.starts_with(super::ADVICE_MARKER),
                            "width {width} budget {budget} cut a finding: {rows:?}"
                        );
                    }
                    let messages = rows
                        .iter()
                        .filter(|r| r.kind == AdviceRowKind::Message && r.text.starts_with(" i "))
                        .count();
                    let hints = rows
                        .iter()
                        .filter(|r| r.kind == AdviceRowKind::Hint && r.text.contains("hint: "))
                        .count();
                    assert_eq!(messages, hints, "a finding lost its hint: {rows:?}");
                    let hidden = report.findings.len() - messages + report.omitted;
                    let notes: Vec<_> = rows
                        .iter()
                        .filter(|r| r.kind == AdviceRowKind::Note)
                        .collect();
                    if hidden == 0 {
                        assert!(notes.is_empty(), "{rows:?}");
                    } else {
                        let last = rows.last().expect("a count row");
                        assert_eq!(last.kind, AdviceRowKind::Note, "{rows:?}");
                        assert!(
                            last.text.contains(&format!("{hidden} ")),
                            "width {width} budget {budget}: {rows:?}"
                        );
                    }
                    assert!(rows.iter().all(|r| !r.text.contains(" 0 ")), "{rows:?}");
                }
            }
        }
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
