// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! UI rendering dispatch.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};

use crate::tui::App;
use crate::tui::state::{Focus, MainTab};

pub mod common;
pub mod editor;
pub mod help;
pub mod history;
pub mod panels;
pub mod popup;
pub mod results;
pub mod saved;
pub mod schema;
pub mod status;
pub mod tabs;

/// Main render function — dispatches to submodules based on app state.
pub fn render(app: &mut App, frame: &mut Frame<'_>) {
    match app.main_tab {
        MainTab::Query => render_query_layout(app, frame),
        MainTab::Dashboard => render_dashboard_layout(app, frame),
        _ => render_panel_layout(app, frame),
    }
}

/// Render the Query tab: tab bar, editor, [validation hint], results, status bar.
fn render_query_layout(app: &mut App, frame: &mut Frame<'_>) {
    let has_validation_hint = !app.active_tab().validation_errors.is_empty();

    // Build layout conditionally to avoid affecting percentage distribution
    // when there is no hint line.
    let (editor_area, hint_area, results_area, status_area) = if has_validation_hint {
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),      // Tab bar
                Constraint::Percentage(40), // Editor
                Constraint::Length(1),      // Validation hint
                Constraint::Percentage(55), // Results
                Constraint::Length(1),      // Status bar
            ])
            .split(frame.area());
        tabs::render(app, frame, outer[0]);
        (outer[1], Some(outer[2]), outer[3], outer[4])
    } else {
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),      // Tab bar
                Constraint::Percentage(40), // Editor
                Constraint::Percentage(55), // Results
                Constraint::Length(1),      // Status bar
            ])
            .split(frame.area());
        tabs::render(app, frame, outer[0]);
        (outer[1], None, outer[2], outer[3])
    };

    editor::render(app, frame, editor_area);

    // Render validation hint bar if there are errors.
    if let Some(hint) = hint_area {
        editor::render_validation_hint(app, frame, hint);
    }

    // Update visible row count for scroll calculations (borders + header = 4 rows overhead).
    #[allow(clippy::cast_possible_truncation)]
    let results_visible = results_area.height.saturating_sub(4) as usize;
    let search_adjust = usize::from(app.results_search.is_some());
    app.active_tab_mut().last_visible_rows = results_visible.saturating_sub(search_adjust).max(1);

    results::render(app, frame, results_area);
    status::render(app, frame, status_area);

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

/// Render the Dashboard tab: tab bar, dashboard content, status bar.
fn render_dashboard_layout(app: &mut App, frame: &mut Frame<'_>) {
    use ratatui::style::{Color, Style};
    use ratatui::widgets::Paragraph;

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // Tab bar (1 row + 1 spacer)
            Constraint::Min(1),    // Dashboard content (full height)
            Constraint::Length(1), // Status bar
        ])
        .split(frame.area());

    tabs::render(app, frame, outer[0]);

    if let Some(ref snapshot) = app.dashboard.cache {
        let footer = app
            .dashboard
            .last_error
            .as_deref()
            .map(|e| format!(" [stale] {e}"));
        let opts = trawl_dashboard::DashboardOptions {
            footer_text: footer,
        };
        trawl_dashboard::render_dashboard(snapshot, frame, outer[1], &opts);
    } else if let Some(ref err) = app.dashboard.last_error {
        let msg = Paragraph::new(format!(" Error: {err}")).style(Style::default().fg(Color::Red));
        frame.render_widget(msg, outer[1]);
    } else {
        let msg =
            Paragraph::new(" Loading dashboard...").style(Style::default().fg(Color::DarkGray));
        frame.render_widget(msg, outer[1]);
    }

    status::render(app, frame, outer[2]);

    // Render popup overlay (if any).
    popup::render(app, frame);
}

/// Render a non-Query tab: tab bar, full-width panel content, status bar.
fn render_panel_layout(app: &mut App, frame: &mut Frame<'_>) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // Tab bar (1 row + 1 spacer)
            Constraint::Min(1),    // Panel content (full height)
            Constraint::Length(1), // Status bar
        ])
        .split(frame.area());

    tabs::render(app, frame, outer[0]);
    panels::render(app, frame, outer[1]);
    status::render(app, frame, outer[2]);

    // Render popup overlay (if any).
    popup::render(app, frame);
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
        for ch in "level=error".chars() {
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
        app.tab.result = Some(response);
        app.tab.status = TabStatus::Success { duration_ms: 42 };
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| super::render(&mut app, f)).unwrap();
        insta::assert_snapshot!(terminal.backend().to_string());
    }

    #[test]
    fn render_with_error_status() {
        let mut app = test_app();
        app.tab.status = TabStatus::Error {
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

    #[test]
    fn render_narrow_terminal_with_results() {
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
            ],
        );
        app.tab.result = Some(response);
        app.tab.status = TabStatus::Success { duration_ms: 5 };
        // Narrow terminal: exercises responsive tab labels and status hints
        let backend = TestBackend::new(50, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| super::render(&mut app, f)).unwrap();
        insta::assert_snapshot!(terminal.backend().to_string());
    }
}
