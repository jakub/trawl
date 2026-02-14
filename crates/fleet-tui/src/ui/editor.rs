//! Query editor pane.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders};

use crate::app::App;
use crate::state::Focus;

/// Render the editor pane.
pub fn render(app: &mut App, frame: &mut Frame<'_>, area: Rect) {
    // Determine border style before borrowing tab mutably.
    let border_style = if app.focus == Focus::Editor && app.sidebar.is_none() {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Query Editor ")
        .border_style(border_style);

    let tab = app.active_tab_mut();
    tab.editor.set_block(block);

    frame.render_widget(&tab.editor, area);
}
