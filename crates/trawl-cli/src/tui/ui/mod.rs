//! UI rendering dispatch.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};

use crate::tui::App;
use crate::tui::state::Focus;

pub mod common;
pub mod editor;
pub mod help;
pub mod history;
pub mod popup;
pub mod results;
pub mod saved;
pub mod schema;
pub mod sidebar;
pub mod status;
pub mod tabs;

/// Width of the activity bar (icon strip) in columns.
const ACTIVITY_BAR_WIDTH: u16 = 3;

/// Main render function — dispatches to submodules based on app state.
pub fn render(app: &mut App, frame: &mut Frame<'_>) {
    // Split the screen into tab bar, main content area, and status bar.
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),      // Tab bar
            Constraint::Percentage(40), // Editor row
            Constraint::Percentage(55), // Results row
            Constraint::Length(1),      // Status bar
        ])
        .split(frame.area());

    // Compute the left panel width: activity bar + optional sidebar panel.
    let sidebar_open = app.sidebar.is_some();
    let left_width = if sidebar_open {
        ACTIVITY_BAR_WIDTH + app.sidebar_width
    } else {
        ACTIVITY_BAR_WIDTH
    };

    // Tab bar and status bar aligned with editor/results (offset by sidebar width).
    let tab_area = Rect {
        x: outer[0].x + left_width,
        y: outer[0].y,
        width: outer[0].width.saturating_sub(left_width),
        height: outer[0].height,
    };
    tabs::render(app, frame, tab_area);

    let status_area = Rect {
        x: outer[3].x + left_width,
        y: outer[3].y,
        width: outer[3].width.saturating_sub(left_width),
        height: outer[3].height,
    };
    status::render(app, frame, status_area);

    // Left panel spans all four rows (tab bar + editor + results + status bar).
    let left_area = Rect {
        x: outer[0].x,
        y: outer[0].y,
        width: left_width.min(outer[0].width),
        height: outer[0].height + outer[1].height + outer[2].height + outer[3].height,
    };

    let editor_area = Rect {
        x: outer[1].x + left_width,
        y: outer[1].y,
        width: outer[1].width.saturating_sub(left_width),
        height: outer[1].height,
    };

    let results_area = Rect {
        x: outer[2].x + left_width,
        y: outer[2].y,
        width: outer[2].width.saturating_sub(left_width),
        height: outer[2].height,
    };

    // Render the activity bar + sidebar panel.
    sidebar::render(app, frame, left_area);

    // Render editor and results panes.
    editor::render(app, frame, editor_area);
    results::render(app, frame, results_area);

    // Render popup overlay (if any) — renders on top of everything.
    popup::render(app, frame);

    // Set cursor position based on focus (adjusted for scroll offset).
    if app.focus == Focus::Editor && app.popup.is_none() {
        let tab = app.active_tab();
        let (row, col) = tab.editor.cursor;
        let scroll_row = tab.editor.scroll_row;
        let scroll_col = tab.editor.scroll_col;
        // +2 for x: border (1) + horizontal padding (1)
        // +1 for y: border (1) only
        #[allow(clippy::cast_possible_truncation)] // Terminal coordinates are always < u16::MAX
        let x = editor_area.x + col.saturating_sub(scroll_col) as u16 + 2;
        #[allow(clippy::cast_possible_truncation)]
        let y = editor_area.y + row.saturating_sub(scroll_row) as u16 + 1;
        frame.set_cursor_position((x, y));
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use trawl_client::QueryResponse;
    use trawl_engine::value::{Column, Value};

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
            result: trawl_engine::value::QueryResult {
                columns: cols,
                rows,
            },
            truncated: false,
            pagination: trawl_client::PaginationMeta {
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
