//! Query editor pane.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph};

use crate::tui::App;
use crate::tui::highlight::Highlighter;
use crate::tui::state::Focus;

/// Selection highlight style.
const SELECTION_STYLE: Style = Style::new()
    .bg(Color::DarkGray)
    .add_modifier(Modifier::empty());

/// Render the editor pane.
pub fn render(app: &mut App, frame: &mut Frame<'_>, area: Rect) {
    // Determine border style.
    let border_style = if app.focus == Focus::Editor && app.sidebar.is_none() {
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

    // Highlight visible lines and apply selection overlay.
    let end_row = (scroll_row + visible_rows).min(tab.editor.lines.len());
    let highlighted_lines: Vec<Line<'static>> = tab.editor.lines[scroll_row..end_row]
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let abs_row = scroll_row + i;
            let styled_line = highlighter.highlight_line(line);

            if let Some(((sel_start_row, sel_start_col), (sel_end_row, sel_end_col))) = selection {
                if abs_row >= sel_start_row && abs_row <= sel_end_row {
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
            }

            styled_line
        })
        .collect();

    #[allow(clippy::cast_possible_truncation)] // scroll_col bounded by terminal width
    let paragraph = Paragraph::new(highlighted_lines)
        .block(block)
        .scroll((0, scroll_col as u16));

    frame.render_widget(paragraph, area);
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
