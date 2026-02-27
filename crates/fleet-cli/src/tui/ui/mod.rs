//! UI rendering dispatch.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};

use crate::tui::App;
use crate::tui::state::{Focus, SidebarSection};

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

    // Render editor pane (needs &mut for scroll adjustment).
    editor::render(app, frame, chunks[1]);

    // Render results pane.
    results::render(app, frame, chunks[2]);

    // Render status bar.
    status::render(app, frame, chunks[3]);

    // Render sidebar overlay (if any).
    // TODO: full sidebar renderer will be implemented in ui/sidebar.rs
    if let Some(ref sb) = app.sidebar {
        match sb.section {
            SidebarSection::Schema => { /* TODO: schema tree render */ }
            SidebarSection::History => history::render(app, frame),
            SidebarSection::Saved => saved::render(app, frame),
        }
    }

    // Render popup overlay (if any) — renders on top of everything.
    popup::render(app, frame);

    // Set cursor position based on focus (adjusted for scroll offset).
    if app.focus == Focus::Editor {
        let tab = app.active_tab();
        let (row, col) = tab.editor.cursor;
        let scroll_row = tab.editor.scroll_row;
        let scroll_col = tab.editor.scroll_col;
        // +2 for x: border (1) + horizontal padding (1)
        // +1 for y: border (1) only
        #[allow(clippy::cast_possible_truncation)] // Terminal coordinates are always < u16::MAX
        let x = chunks[1].x + col.saturating_sub(scroll_col) as u16 + 2;
        #[allow(clippy::cast_possible_truncation)]
        let y = chunks[1].y + row.saturating_sub(scroll_row) as u16 + 1;
        frame.set_cursor_position((x, y));
    }
}

#[cfg(test)]
mod tests {
    use fleet_client::QueryResponse;
    use fleet_engine::value::{Column, Value};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::tui::state::{Popup, TabStatus};
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
        let mut app = test_app();
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| super::render(&mut app, f)).unwrap();
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
        terminal.draw(|f| super::render(&mut app, f)).unwrap();
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
        terminal.draw(|f| super::render(&mut app, f)).unwrap();
        insta::assert_snapshot!(terminal.backend().to_string());
    }

    #[test]
    fn render_with_error_status() {
        let mut app = test_app();
        app.tabs[0].status = TabStatus::Error {
            message: "parse error: unexpected token".to_owned(),
            details: Vec::new(),
        };
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| super::render(&mut app, f)).unwrap();
        insta::assert_snapshot!(terminal.backend().to_string());
    }

    #[test]
    fn render_with_help_popup() {
        let mut app = test_app();
        app.popup = Some(Popup::Help { scroll: 0 });
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| super::render(&mut app, f)).unwrap();
        insta::assert_snapshot!(terminal.backend().to_string());
    }

    #[test]
    fn render_with_save_popup() {
        use crate::tui::state::SimpleEditor;
        let mut app = test_app();
        let mut editor = SimpleEditor::new_single_line();
        editor.insert_text("my query");
        app.popup = Some(Popup::SaveQuery { editor });
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| super::render(&mut app, f)).unwrap();
        insta::assert_snapshot!(terminal.backend().to_string());
    }
}
