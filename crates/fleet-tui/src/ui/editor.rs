//! Query editor pane.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::App;
use crate::highlight::Highlighter;
use crate::state::Focus;

/// Render the editor pane.
pub fn render(app: &App, frame: &mut Frame<'_>, area: Rect) {
    // Determine border style.
    let border_style = if app.focus == Focus::Editor && app.sidebar.is_none() {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Query Editor ")
        .border_style(border_style);

    let tab = app.active_tab();

    // Create highlighter with schema information.
    let highlighter = Highlighter::new(app.schema_cache.as_ref());

    // Highlight each line.
    let highlighted_lines: Vec<_> = tab
        .editor
        .lines
        .iter()
        .map(|line| highlighter.highlight_line(line))
        .collect();

    let paragraph = Paragraph::new(highlighted_lines).block(block);

    frame.render_widget(paragraph, area);
}
