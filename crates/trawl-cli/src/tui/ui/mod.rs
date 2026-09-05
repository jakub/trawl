// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! UI rendering dispatch.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::macros::vertical;

use crate::tui::App;
use crate::tui::state::{Focus, LayoutAreas, MainTab, Popup};

pub mod common;
pub mod editor;
pub mod help;
pub mod history;
pub mod palette;
pub mod panels;
pub mod popup;
pub mod results;
pub mod saved;
pub mod schema;
pub mod status;
pub mod tabs;

/// Main render function — dispatches to submodules based on app state.
pub fn render(app: &mut App, frame: &mut Frame<'_>) {
    // Fill entire frame with surface background so unpainted cells (spacer rows,
    // gaps between widgets) use the theme color instead of the terminal default.
    use ratatui::widgets::Block;
    let bg = Block::default().style(ratatui::style::Style::default().bg(app.theme.surface));
    frame.render_widget(bg, frame.area());

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
        let [tab_bar, editor, hint, results, status] =
            vertical![==2, ==40%, ==1, ==55%, ==1].areas(frame.area());
        tabs::render(app, frame, tab_bar);
        (editor, Some(hint), results, status)
    } else {
        let [tab_bar, editor, results, status] =
            vertical![==2, ==40%, ==55%, ==1].areas(frame.area());
        tabs::render(app, frame, tab_bar);
        (editor, None, results, status)
    };

    // Populate layout areas for mouse hit-testing.
    app.layout = LayoutAreas {
        tab_bar: Rect {
            height: 2,
            ..frame.area()
        },
        editor: Some(editor_area),
        results: Some(results_area),
        panel: None,
        status: status_area,
        popup: app.popup.as_ref().map(|p| popup_area(p, frame.area())),
        column_header_ranges: Vec::new(),
    };

    editor::render(app, frame, editor_area);

    if let Some(hint) = hint_area {
        editor::render_validation_hint(app, frame, hint);
    }

    // Visible rows for scroll math: 2 borders plus the header row account for 3 of the 4
    // subtracted; the 4th is left spare, matching results::render_table.
    #[allow(clippy::cast_possible_truncation)]
    let results_visible = results_area.height.saturating_sub(4) as usize;
    let search_adjust = usize::from(app.results_search.is_some());
    app.active_tab_mut().last_visible_rows = results_visible.saturating_sub(search_adjust).max(1);

    results::render(app, frame, results_area);
    app.layout.column_header_ranges = results::take_header_ranges();
    status::render(app, frame, status_area);

    // Popup last so it paints over everything else.
    popup::render(app, frame);

    // Set cursor position based on focus (adjusted for scroll offset).
    // When wrapping is active, use visual coordinates from the wrap map.
    if app.focus == Focus::Editor && app.popup.is_none() {
        let tab = app.active_tab();
        let (vrow, vcol) = tab.editor.visual_cursor();
        let scroll_row = tab.editor.scroll_row;
        // +2 for x: border (1) + horizontal padding (1)
        // +1 for y: border (1) only
        #[allow(clippy::cast_possible_truncation)] // Terminal coordinates are always < u16::MAX
        let x = editor_area.x + vcol as u16 + 2;
        #[allow(clippy::cast_possible_truncation)]
        let y = editor_area.y + vrow.saturating_sub(scroll_row) as u16 + 1;
        frame.set_cursor_position((x, y));
    }
}

/// Render the Dashboard tab: tab bar, dashboard content, status bar.
fn render_dashboard_layout(app: &mut App, frame: &mut Frame<'_>) {
    use ratatui::style::Style;
    use ratatui::widgets::Paragraph;
    let theme = &app.theme;

    let [tab_bar, content, status_area] = vertical![==2, >=1, ==1].areas(frame.area());

    app.layout = LayoutAreas {
        tab_bar,
        editor: None,
        results: None,
        panel: Some(content),
        status: status_area,
        popup: app.popup.as_ref().map(|p| popup_area(p, frame.area())),
        column_header_ranges: Vec::new(),
    };

    tabs::render(app, frame, tab_bar);

    if let Some(ref snapshot) = app.dashboard.cache {
        let footer = app
            .dashboard
            .last_error
            .as_deref()
            .map(|e| format!(" [stale] {e}"));
        let opts = trawl_dashboard::DashboardOptions {
            footer_text: footer,
        };
        trawl_dashboard::render_dashboard(snapshot, frame, content, &opts);
    } else if let Some(ref err) = app.dashboard.last_error {
        let msg =
            Paragraph::new(format!(" Error: {err}")).style(Style::default().fg(theme.status_error));
        frame.render_widget(msg, content);
    } else {
        let msg =
            Paragraph::new(" Loading dashboard...").style(Style::default().fg(theme.text_muted));
        frame.render_widget(msg, content);
    }

    status::render(app, frame, status_area);

    popup::render(app, frame);
}

/// Render a non-Query tab: tab bar, full-width panel content, status bar.
fn render_panel_layout(app: &mut App, frame: &mut Frame<'_>) {
    let [tab_bar, panel, status_area] = vertical![==2, >=1, ==1].areas(frame.area());

    app.layout = LayoutAreas {
        tab_bar,
        editor: None,
        results: None,
        panel: Some(panel),
        status: status_area,
        popup: app.popup.as_ref().map(|p| popup_area(p, frame.area())),
        column_header_ranges: Vec::new(),
    };

    tabs::render(app, frame, tab_bar);
    panels::render(app, frame, panel);
    status::render(app, frame, status_area);

    popup::render(app, frame);
}

/// Compute the popup area for hit-testing based on the active popup type.
///
/// Uses the same percentages as each popup's render function so click-outside
/// detection is accurate.
fn popup_area(popup: &Popup, frame_area: Rect) -> Rect {
    use common::centered_rect;
    match popup {
        Popup::Help { .. } => centered_rect(60, 70, frame_area),
        Popup::EventDetail { .. } => centered_rect(70, 80, frame_area),
        Popup::ConfirmDelete { .. } | Popup::SaveQuery { .. } => centered_rect(60, 35, frame_area),
        Popup::Error { .. } => centered_rect(60, 30, frame_area),
        Popup::SetSchedule { .. } => centered_rect(70, 45, frame_area),
        Popup::ColumnPicker { .. } => centered_rect(50, 60, frame_area),
        Popup::CommandPalette { .. } => centered_rect(65, 75, frame_area),
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
            degraded_fields: Vec::new(),
            severity_columns: Vec::new(),
        }
    }

    /// A schedule on a one-hour interval. `window`/`lag` are the ADR-0018
    /// report-window fields: both absent is query mode, where the saved DSL
    /// owns its own time bounds.
    fn make_schedule(
        window: Option<&str>,
        lag: Option<(&str, u64)>,
    ) -> trawl_api::ScheduleResponse {
        trawl_api::ScheduleResponse {
            id: 3,
            saved_query_id: 7,
            interval: "1h".to_owned(),
            interval_secs: 3600,
            max_runs: None,
            enabled: true,
            created_at: "2026-03-01T00:00:00Z".to_owned(),
            updated_at: "2026-03-01T00:00:00Z".to_owned(),
            last_run: None,
            total_runs: 3,
            window: window.map(ToOwned::to_owned),
            lag: lag.map(|(text, _)| text.to_owned()),
            lag_secs: lag.map(|(_, secs)| secs),
            covered_through: window.map(|_| "2026-03-14T03:00:00Z".to_owned()),
            next_fire_at: "2026-03-14T04:00:00Z".to_owned(),
        }
    }

    /// A saved query with an optional schedule, for the Saved tab renders.
    fn make_saved(schedule: Option<trawl_api::ScheduleResponse>) -> trawl_api::SavedQueryResponse {
        trawl_api::SavedQueryResponse {
            id: 7,
            name: "nightly errors".to_owned(),
            query: "_severity>=error | stats count() by service".to_owned(),
            created_at: "2026-03-01T00:00:00Z".to_owned(),
            updated_at: "2026-03-01T00:00:00Z".to_owned(),
            schedule,
        }
    }

    /// A report run whose `started_at` is `minutes_ago` behind the clock, so
    /// the list's relative time column renders the same string on every run.
    fn make_run(id: i64, minutes_ago: i64) -> trawl_api::ReportRunSummary {
        let started = chrono::Utc::now() - chrono::Duration::minutes(minutes_ago);
        trawl_api::ReportRunSummary {
            id,
            query: "_severity>=error | stats count() by service".to_owned(),
            status: "success".to_owned(),
            started_at: started.to_rfc3339(),
            finished_at: None,
            duration_ms: Some(120),
            row_count: Some(12),
            error_message: None,
            result_path: None,
            window_start: None,
            window_end: None,
            window_truncated: None,
            window_kind: None,
        }
    }

    /// Put the app on the Saved tab with one saved query selected and its run
    /// history loaded, which is the state the detail pane renders from.
    fn saved_app(
        schedule: Option<trawl_api::ScheduleResponse>,
        runs: Vec<trawl_api::ReportRunSummary>,
    ) -> crate::tui::App {
        use crate::tui::state::{MainTab, SavedDetailState, SavedFocus};

        // Keep the pane self-consistent: the schedule's run total is the same
        // count the run list below it renders.
        let schedule = schedule.map(|mut sched| {
            sched.total_runs = u64::try_from(runs.len()).unwrap();
            sched
        });

        let mut app = test_app();
        app.main_tab = MainTab::Saved;
        app.saved_cache = Some(trawl_api::ListSavedResponse {
            queries: vec![make_saved(schedule)],
        });
        app.panel.saved_selected = 0;
        app.panel.saved_focus = SavedFocus::Detail;
        app.panel.saved_detail = Some(SavedDetailState {
            saved_id: 7,
            total_runs: runs.len(),
            runs,
            run_selected: 0,
            run_scroll: 0,
            result: None,
            result_scroll: 0,
            loading: false,
        });
        app
    }

    /// A run list carrying all three window shapes at once: a tiled run that
    /// covered everything it owed, a run clamped past a catch-up gap, and a
    /// legacy/query-mode run that has no window at all. `Some(false)` shows
    /// its bounds bare; `None` shows nothing.
    #[test]
    fn render_saved_runs_with_windows() {
        let mut normal = make_run(41, 45);
        normal.window_start = Some("2026-03-14T02:00:00Z".to_owned());
        normal.window_end = Some("2026-03-14T03:00:00Z".to_owned());
        normal.window_truncated = Some(false);
        normal.window_kind = Some("since_last".to_owned());

        let mut truncated = make_run(40, 105);
        truncated.window_start = Some("2026-03-13T23:00:00Z".to_owned());
        truncated.window_end = Some("2026-03-14T01:00:00Z".to_owned());
        truncated.window_truncated = Some(true);
        truncated.window_kind = Some("fixed".to_owned());

        let legacy = make_run(39, 165);

        let mut app = saved_app(
            Some(make_schedule(None, None)),
            vec![normal, truncated, legacy],
        );
        // Wide enough that the whole run line lands: the window trails the
        // row, so a narrower pane cuts the bounds and then the marker.
        let backend = TestBackend::new(160, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| super::render(&mut app, f)).unwrap();
        insta::assert_snapshot!(terminal.backend().to_string());
    }

    /// A run carrying the bounds of the window it covered.
    fn windowed_run(
        id: i64,
        minutes_ago: i64,
        kind: &str,
        start: &str,
        end: &str,
    ) -> trawl_api::ReportRunSummary {
        let mut run = make_run(id, minutes_ago);
        run.window_kind = Some(kind.to_owned());
        run.window_start = Some(start.to_owned());
        run.window_end = Some(end.to_owned());
        run.window_truncated = Some(false);
        run
    }

    /// A tiling schedule with a late-arrival allowance: the detail pane names
    /// the mode, the lag and the watermark the next window starts from.
    #[test]
    fn render_saved_schedule_windowed_with_lag() {
        let run = windowed_run(
            41,
            45,
            "since_last",
            "2026-03-14T02:00:00Z",
            "2026-03-14T03:00:00Z",
        );
        let mut app = saved_app(
            Some(make_schedule(Some("since_last"), Some(("5m", 300)))),
            vec![run],
        );
        let backend = TestBackend::new(160, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| super::render(&mut app, f)).unwrap();
        insta::assert_snapshot!(terminal.backend().to_string());
    }

    /// A fixed trailing window with no lag: no `lag 0s`, and no watermark,
    /// because a fixed window keeps none.
    #[test]
    fn render_saved_schedule_windowed_without_lag() {
        let run = windowed_run(
            41,
            45,
            "fixed",
            "2026-03-14T01:00:00Z",
            "2026-03-14T03:00:00Z",
        );
        let mut schedule = make_schedule(Some("2h"), Some(("0s", 0)));
        schedule.covered_through = None;
        let mut app = saved_app(Some(schedule), vec![run]);
        let backend = TestBackend::new(160, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| super::render(&mut app, f)).unwrap();
        insta::assert_snapshot!(terminal.backend().to_string());
    }

    /// Query mode: the saved DSL owns its own time bounds, so there is no
    /// window line at all, only the fire cursor every schedule has.
    #[test]
    fn render_saved_schedule_query_mode() {
        let mut app = saved_app(Some(make_schedule(None, None)), vec![make_run(41, 45)]);
        let backend = TestBackend::new(160, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| super::render(&mut app, f)).unwrap();
        insta::assert_snapshot!(terminal.backend().to_string());
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
        for ch in "_severity=error".chars() {
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

    /// A `_severity` column renders its `OTel` token (`17` → `error`,
    /// `18` → `error2`), one token per number, while a sender's own `level`
    /// column renders its text verbatim (ADR-0013 §6).
    #[test]
    fn render_with_severity_tokens() {
        let mut app = test_app();
        let response = make_query_response(
            vec!["_time", "service", "_severity", "level", "message"],
            vec![
                vec![
                    Value::String("2026-01-15T10:00:00Z".into()),
                    Value::String("nginx".into()),
                    Value::Integer(17),
                    Value::String("error".into()),
                    Value::String("bad gateway".into()),
                ],
                vec![
                    Value::String("2026-01-15T10:00:01Z".into()),
                    Value::String("nginx".into()),
                    Value::Integer(18),
                    Value::String("error".into()),
                    Value::String("upstream timeout".into()),
                ],
                vec![
                    Value::String("2026-01-15T10:00:02Z".into()),
                    Value::String("nginx".into()),
                    Value::Integer(13),
                    Value::String("warn".into()),
                    Value::String("slow response".into()),
                ],
                vec![
                    Value::String("2026-01-15T10:00:03Z".into()),
                    Value::String("game".into()),
                    Value::Null,
                    Value::String("gold".into()),
                    Value::String("loot dropped".into()),
                ],
            ],
        );
        app.tab.result = Some(response);
        app.tab.status = TabStatus::Success { duration_ms: 12 };
        let backend = TestBackend::new(100, 16);
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

    /// The set-schedule popup with the window row focused: the interval and
    /// window rows carry prefilled text, the empty lag row shows what an
    /// empty row means rather than nothing.
    #[test]
    fn render_with_schedule_popup() {
        use crate::tui::state::{ScheduleField, ScheduleForm};

        let mut app = test_app();
        let mut form = ScheduleForm::new(
            5,
            "nightly errors".to_owned(),
            Some(&make_schedule(Some("since_last"), None)),
        );
        form.focus = ScheduleField::Window;
        app.popup = Some(Popup::SetSchedule(Box::new(form)));

        let backend = TestBackend::new(90, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| super::render(&mut app, f)).unwrap();
        insta::assert_snapshot!(terminal.backend().to_string());
    }

    /// A terminal too small to hold the popup must not panic: the cursor
    /// clamp is the arithmetic that would. Narrower than ~37 columns trips a
    /// pre-existing overflow in the empty-results placeholder behind the
    /// popup, which is a separate bug and not this popup's business.
    #[test]
    fn render_schedule_popup_on_a_tiny_terminal() {
        use crate::tui::state::ScheduleForm;

        let mut app = test_app();
        app.popup = Some(Popup::SetSchedule(Box::new(ScheduleForm::new(
            5,
            "nightly errors".to_owned(),
            None,
        ))));

        let backend = TestBackend::new(42, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| super::render(&mut app, f)).unwrap();
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
