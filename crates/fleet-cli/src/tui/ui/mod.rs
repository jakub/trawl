//! UI rendering dispatch.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};

use crate::tui::App;
use crate::tui::state::{Focus, Sidebar};

pub mod common;
pub mod editor;
pub mod help;
pub mod history;
pub mod popup;
pub mod results;
pub mod saved;
pub mod schema;
pub mod status;
pub mod tabs;

/// Main render function — dispatches to submodules based on app state.
pub fn render(app: &App, frame: &mut Frame<'_>) {
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

    // Render popup overlay (if any) — renders on top of everything.
    popup::render(app, frame);

    // Set cursor position based on focus.
    if app.sidebar.is_none() && app.focus == Focus::Editor {
        // Show cursor in editor.
        let tab = app.active_tab();
        let (row, col) = tab.editor.cursor;
        #[allow(clippy::cast_possible_truncation)] // Terminal coordinates are always < u16::MAX
        // +2 for x: border (1) + horizontal padding (1)
        // +1 for y: border (1) only
        frame.set_cursor_position((chunks[1].x + col as u16 + 2, chunks[1].y + row as u16 + 1));
    }
}

#[cfg(test)]
mod tests {
    use fleet_client::QueryResponse;
    use fleet_engine::value::{Column, Value};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::tui::state::{Popup, Sidebar, TabStatus};
    use crate::tui::tests::test_app;

    /// Build a successful `QueryResponse` with the given columns and rows.
    fn make_query_response(columns: Vec<&str>, rows: Vec<Vec<Value>>) -> QueryResponse {
        let cols = columns
            .into_iter()
            .map(|name| Column {
                name: name.to_owned(),
            })
            .collect();
        let returned = rows.len();
        QueryResponse {
            result: fleet_engine::value::QueryResult {
                columns: cols,
                rows,
            },
            truncated: false,
            pagination: fleet_client::PaginationMeta {
                limit: 10000,
                offset: 0,
                returned,
            },
        }
    }

    #[test]
    fn render_empty_app() {
        let app = test_app();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| super::render(&app, f)).unwrap();
        insta::assert_snapshot!(terminal.backend().to_string());
    }

    #[test]
    fn render_with_query_text() {
        let mut app = test_app();
        for ch in "level:error".chars() {
            app.active_tab_mut().editor.insert_char(ch);
        }
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| super::render(&app, f)).unwrap();
        insta::assert_snapshot!(terminal.backend().to_string());
    }

    #[test]
    fn render_with_results() {
        let mut app = test_app();
        let response = make_query_response(
            vec!["host", "service", "count"],
            vec![
                vec![
                    Value::String("web-1".into()),
                    Value::String("nginx".into()),
                    Value::Integer(42),
                ],
                vec![
                    Value::String("web-2".into()),
                    Value::String("api".into()),
                    Value::Integer(17),
                ],
                vec![
                    Value::String("db-1".into()),
                    Value::String("postgres".into()),
                    Value::Integer(3),
                ],
                vec![
                    Value::String("web-3".into()),
                    Value::String("redis".into()),
                    Value::Integer(99),
                ],
                vec![
                    Value::String("lb-1".into()),
                    Value::String("haproxy".into()),
                    Value::Integer(7),
                ],
            ],
        );
        app.tabs[0].result = Some(response);
        app.tabs[0].status = TabStatus::Success { duration_ms: 42 };
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| super::render(&app, f)).unwrap();
        insta::assert_snapshot!(terminal.backend().to_string());
    }

    #[test]
    fn render_with_error_status() {
        let mut app = test_app();
        app.tabs[0].status = TabStatus::Error {
            message: "parse error: unexpected token".to_owned(),
        };
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| super::render(&app, f)).unwrap();
        insta::assert_snapshot!(terminal.backend().to_string());
    }

    #[test]
    fn render_with_help_sidebar() {
        let mut app = test_app();
        app.sidebar = Some(Sidebar::Help);
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| super::render(&app, f)).unwrap();
        insta::assert_snapshot!(terminal.backend().to_string());
    }

    #[test]
    fn render_with_save_popup() {
        let mut app = test_app();
        app.popup = Some(Popup::SaveQuery {
            input: "my query".to_owned(),
        });
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| super::render(&app, f)).unwrap();
        insta::assert_snapshot!(terminal.backend().to_string());
    }
}
