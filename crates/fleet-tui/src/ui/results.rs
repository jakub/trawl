//! Results table pane.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::App;
use crate::state::Focus;

/// Render the results pane.
pub fn render(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let border_style = if app.focus == Focus::Results && app.sidebar.is_none() {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Results ")
        .border_style(border_style);

    // Placeholder content.
    let text = if app.active_tab().result.is_some() {
        Line::from("query results will appear here")
    } else {
        Line::from("no results yet — execute a query with ctrl+enter")
    };

    let paragraph = Paragraph::new(text).block(block);
    frame.render_widget(paragraph, area);
}
