//! UI rendering dispatch.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};

use crate::app::App;
use crate::state::{Focus, Sidebar};

pub mod editor;
pub mod help;
pub mod history;
pub mod results;
pub mod saved;
pub mod schema;
pub mod status;
pub mod tabs;

/// Main render function — dispatches to submodules based on app state.
pub fn render(app: &mut App, frame: &mut Frame<'_>) {
    // Split the screen into tab bar, editor, results, and status bar.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),      // Tab bar
            Constraint::Percentage(40), // Editor
            Constraint::Percentage(55), // Results
            Constraint::Length(1),      // Status bar
        ])
        .split(frame.area());

    // Render tab bar.
    tabs::render(app, frame, chunks[0]);

    // Render editor pane.
    editor::render(app, frame, chunks[1]);

    // Render results pane.
    results::render(app, frame, chunks[2]);

    // Render status bar.
    status::render(app, frame, chunks[3]);

    // Render sidebar overlay (if any).
    if let Some(sidebar) = app.sidebar {
        match sidebar {
            Sidebar::Help => help::render(app, frame),
            Sidebar::Schema => schema::render(app, frame),
            Sidebar::History => history::render(app, frame),
            Sidebar::Saved => saved::render(app, frame),
        }
    }

    // Set cursor position based on focus.
    if app.sidebar.is_none() && app.focus == Focus::Editor {
        // Show cursor in editor.
        let tab = app.active_tab();
        let cursor = tab.editor.cursor();
        #[allow(clippy::cast_possible_truncation)] // Terminal coordinates are always < u16::MAX
        frame.set_cursor_position((
            chunks[1].x + cursor.1 as u16 + 1,
            chunks[1].y + cursor.0 as u16 + 1,
        ));
    }
}
