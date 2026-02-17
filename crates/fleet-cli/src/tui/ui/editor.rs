//! Query editor pane.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Padding, Paragraph};

use crate::tui::App;
use crate::tui::highlight::Highlighter;
use crate::tui::state::Focus;

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

    // Create highlighter with schema information.
    let highlighter = Highlighter::new(app.schema_cache.as_ref());

    // Highlight visible lines only.
    let end_row = (scroll_row + visible_rows).min(tab.editor.lines.len());
    let highlighted_lines: Vec<_> = tab.editor.lines[scroll_row..end_row]
        .iter()
        .map(|line| highlighter.highlight_line(line))
        .collect();

    #[allow(clippy::cast_possible_truncation)] // scroll_col bounded by terminal width
    let paragraph = Paragraph::new(highlighted_lines)
        .block(block)
        .scroll((0, scroll_col as u16));

    frame.render_widget(paragraph, area);
}
