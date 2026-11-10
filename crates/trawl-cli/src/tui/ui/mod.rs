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
    use trawl_core::sanitize::sanitize_display_text;
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
            .map(|e| format!(" [stale] {}", sanitize_display_text(e)));
        let opts = trawl_dashboard::DashboardOptions {
            footer_text: footer,
        };
        trawl_dashboard::render_dashboard(snapshot, frame, content, &opts);
    } else if let Some(ref err) = app.dashboard.last_error {
        let msg = Paragraph::new(format!(" Error: {}", sanitize_display_text(err)))
            .style(Style::default().fg(theme.status_error));
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
        // Each column typed by the cells it holds, as a server would.
        let cols = columns
            .into_iter()
            .enumerate()
            .map(|(idx, name)| {
                let mut fold = trawl_engine::value::TypeFold::new();
                for row in &rows {
                    fold.observe_value(&row[idx]);
                }
                Column::new(
                    name,
                    fold.finish()
                        .unwrap_or(trawl_engine::value::ColumnType::String),
                )
            })
            .collect();
        let returned = rows.len();
        QueryResponse {
            execution: None,
            result: trawl_engine::value::QueryResult {
                columns: cols,
                rows,
            },
            pagination: trawl_client::PaginationMeta {
                limit: 10000,
                offset: 0,
                returned,
                total: returned,
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
            exhausted: false,
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
            origin: None,
        }
    }

    /// Put the app on the Saved tab with one saved query selected and its run
    /// history loaded, which is the state the detail pane renders from.
    fn saved_app(
        schedule: Option<trawl_api::ScheduleResponse>,
        runs: Vec<trawl_api::ReportRunSummary>,
    ) -> crate::tui::App {
        use crate::tui::state::{MainTab, SavedDetailState, SavedFocus};

        // Start the lifetime claim count at the kept runs, as for a schedule
        // retention has not trimmed. The lifetime count can exceed the kept
        // runs; a test that needs that sets it after, as
        // `saved_detail_shows_the_run_budget` does.
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
            pending: None,
        });
        app
    }

    /// A run list carrying all three window shapes at once: a tiled run that
    /// covered everything it owed, a run clamped past a catch-up gap, and a
    /// Query text run that has no window at all. `Some(false)` shows
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

        let query_text = make_run(39, 165);

        let mut app = saved_app(
            Some(make_schedule(None, None)),
            vec![normal, truncated, query_text],
        );
        // At +05:30 the truncated window starts and ends on 2026-03-14, so
        // its end drops the date its UTC reading would keep.
        app.display_zone = "+05:30".parse().unwrap();
        // Wide enough that the whole run line lands: the window trails the
        // row, so a narrower pane cuts the bounds and then the marker.
        let backend = TestBackend::new(180, 20);
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

    /// The detail `runs` line shows the run budget in its three states: the
    /// lifetime count alone without a cap, `N/M` under a cap, and `spent N/M`
    /// once spent. The lifetime count exceeds the one kept run, as it does
    /// after retention, and a lowered cap leaves `spent 5/3`.
    #[test]
    fn saved_detail_shows_the_run_budget() {
        use std::fmt::Write as _;

        let cases = [
            ("uncapped", None, false),
            ("capped", Some(9), false),
            ("spent", Some(3), true),
        ];
        let mut frames = String::new();
        for (state, max_runs, exhausted) in cases {
            let mut app = saved_app(Some(make_schedule(None, None)), vec![make_run(41, 45)]);
            let sched = app.saved_cache.as_mut().unwrap().queries[0]
                .schedule
                .as_mut()
                .unwrap();
            sched.total_runs = 5;
            sched.max_runs = max_runs;
            sched.exhausted = exhausted;
            let backend = TestBackend::new(160, 20);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|f| super::render(&mut app, f)).unwrap();
            let screen = terminal.backend().to_string();
            let runs_line = screen
                .lines()
                .find(|line| line.contains("runs   "))
                .unwrap()
                .trim_matches(|c: char| c == '"' || c == '\u{2502}' || c.is_whitespace())
                .to_owned();
            writeln!(frames, "{state}: {runs_line}").unwrap();
        }
        insta::assert_snapshot!(frames);
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
        app.display_zone = "+05:30".parse().unwrap();
        let backend = TestBackend::new(160, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| super::render(&mut app, f)).unwrap();
        insta::assert_snapshot!(terminal.backend().to_string());
    }

    /// At `+05:30` the history `executed` line and every instant of the saved
    /// detail (next fire, covered through, the run window) read at
    /// `+05:30`, and none says ` UTC`.
    #[test]
    fn tui_metadata_uses_the_display_zone() {
        use crate::tui::state::MainTab;

        let draw = |app: &mut crate::tui::App| {
            let mut terminal = Terminal::new(TestBackend::new(160, 20)).unwrap();
            terminal.draw(|f| super::render(app, f)).unwrap();
            terminal.backend().to_string()
        };

        let mut app = test_app();
        app.display_zone = "+05:30".parse().unwrap();
        app.main_tab = MainTab::History;
        app.history_cache = Some(trawl_api::HistoryResponse {
            entries: vec![trawl_api::HistoryEntryResponse {
                id: 1,
                query: "service=api".to_owned(),
                executed_at: "2026-03-18T14:30:00.123456Z".to_owned(),
                duration_ms: 12,
                row_count: 3,
                status: trawl_client::QueryStatus::Success,
            }],
            total: 1,
        });
        let history = draw(&mut app);
        assert!(
            history.contains("2026-03-18 20:00:00.123456 +05:30"),
            "the executed line is not at +05:30:\n{history}"
        );
        assert!(!history.contains(" UTC"), "{history}");

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
        app.display_zone = "+05:30".parse().unwrap();
        let saved = draw(&mut app);
        for reading in [
            "covered through 2026-03-14 08:30:00 +05:30",
            "next fire 2026-03-14 09:30:00 +05:30",
            "[2026-03-14 07:30:00 +05:30 .. 08:30:00 +05:30)",
        ] {
            assert!(saved.contains(reading), "{reading} missing:\n{saved}");
        }
        assert!(!saved.contains(" UTC"), "{saved}");
    }

    /// A saved run's result table shows a timestamp cell in the display zone
    /// with its offset, whole even with a fraction, and leaves a string cell
    /// that only looks like an instant as it was sent.
    #[test]
    fn saved_run_results_draw_instants_in_the_display_zone() {
        use crate::tui::state::{LoadedRun, SavedFocus};
        use trawl_api::value::{Column, ColumnType, QueryResult, Value};

        let run = windowed_run(
            41,
            45,
            "since_last",
            "2026-03-14T02:00:00Z",
            "2026-03-14T03:00:00Z",
        );
        let mut app = saved_app(None, vec![run]);
        app.display_zone = "+05:30".parse().unwrap();
        app.panel.saved_focus = SavedFocus::RunResults;
        app.panel.saved_detail.as_mut().unwrap().result = Some(LoadedRun {
            run_id: 41,
            result: QueryResult {
                columns: vec![
                    Column::new("_time", ColumnType::Timestamp),
                    Column::new("message", ColumnType::String),
                ],
                rows: vec![vec![
                    Value::String("2026-03-14T02:00:00.123456Z".to_owned()),
                    Value::String("2026-03-14T02:00:00.000000Z".to_owned()),
                ]],
            },
        });

        let mut terminal = Terminal::new(TestBackend::new(160, 20)).unwrap();
        terminal.draw(|f| super::render(&mut app, f)).unwrap();
        let drawn = terminal.backend().to_string();
        assert!(
            drawn.contains("2026-03-14 07:30:00.123456 +05:30  2026-03-14T02:00:00.000000Z"),
            "{drawn}"
        );
    }

    /// An instant with a fraction below the rows the widths sample still
    /// shows its offset once it scrolls on screen.
    #[test]
    fn a_saved_run_instant_past_the_width_sample_keeps_its_offset() {
        use crate::tui::state::{LoadedRun, SavedFocus};
        use trawl_api::value::{Column, ColumnType, QueryResult, Value};

        let mut rows = vec![vec![Value::String("2026-03-14T02:00:00.000000Z".to_owned())]; 100];
        rows.push(vec![Value::String(
            "2026-03-14T02:00:00.123456Z".to_owned(),
        )]);
        let mut app = saved_app(None, vec![make_run(41, 45)]);
        app.display_zone = "+05:30".parse().unwrap();
        app.panel.saved_focus = SavedFocus::RunResults;
        let detail = app.panel.saved_detail.as_mut().unwrap();
        detail.result = Some(LoadedRun {
            run_id: 41,
            result: QueryResult {
                columns: vec![Column::new("_time", ColumnType::Timestamp)],
                rows,
            },
        });
        detail.result_scroll = 100;

        let mut terminal = Terminal::new(TestBackend::new(160, 20)).unwrap();
        terminal.draw(|f| super::render(&mut app, f)).unwrap();
        let drawn = terminal.backend().to_string();
        assert!(
            drawn.contains("2026-03-14 07:30:00.123456 +05:30"),
            "{drawn}"
        );
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

    /// The regression sequence: Enter on X, Left, Down to Y, then X's runs
    /// arrive. The pane shows the list with Y selected and none of X's runs.
    /// The test never awaits, so the spawned fetch never runs.
    #[tokio::test]
    async fn render_saved_late_runs_after_moving_to_another_query() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut x = make_saved(None);
        x.id = 1;
        x.name = "x errors".to_owned();
        let mut y = make_saved(None);
        y.id = 2;
        y.name = "y latency".to_owned();
        y.query = "service=api | stats p95(duration)".to_owned();

        let mut app = test_app();
        app.saved_cache = Some(trawl_api::ListSavedResponse {
            queries: vec![x, y],
        });
        app.handle_key(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::ALT));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let token = app.panel.saved_detail.as_ref().unwrap().pending.unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.mutation_tx
            .send(crate::tui::MutationResult::RunsLoaded {
                token,
                saved_id: 1,
                outcome: Ok((vec![make_run(4101, 5), make_run(4102, 65)], 2)),
            })
            .unwrap();
        app.poll_mutations();

        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| super::render(&mut app, f)).unwrap();
        let screen = terminal.backend().to_string();
        assert!(
            !screen.contains("4101") && !screen.contains("4102"),
            "{screen}"
        );
        // The text snapshot cannot show the highlight, so check it here: the
        // focused list highlights the selected row in bold.
        let buffer = terminal.backend().buffer();
        let bold = |row: u16| {
            buffer[(1, row)]
                .modifier
                .contains(ratatui::style::Modifier::BOLD)
        };
        assert!(!bold(3) && bold(4), "y latency is the selected row");
        insta::assert_snapshot!(screen);
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

    /// A `timestamp` column draws each UTC instant in the display zone
    /// with its own offset, widths included; a string column holding the
    /// same text draws as sent.
    #[test]
    fn wire_timestamps_draw_in_the_display_zone() {
        let draw = |time_ty: trawl_engine::value::ColumnType, zone: &str| {
            let mut app = test_app();
            app.display_zone = zone.parse().unwrap();
            let mut response = make_query_response(
                vec!["_time", "message"],
                ["2026-01-15T04:30:00.000000Z", "2026-01-15T04:30:01.250000Z"]
                    .iter()
                    .map(|t| {
                        vec![
                            Value::String((*t).into()),
                            Value::String("request ok".into()),
                        ]
                    })
                    .collect(),
            );
            response.result.columns[0].ty = time_ty;
            app.tab.result = Some(response);
            app.tab.status = TabStatus::Success { duration_ms: 3 };
            let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
            terminal.draw(|f| super::render(&mut app, f)).unwrap();
            terminal.backend().to_string()
        };
        let india = draw(trawl_engine::value::ColumnType::Timestamp, "+05:30");
        assert!(india.contains("2026-01-15 10:00:00 +05:30"), "{india}");
        assert!(india.contains("2026-01-15 10:00:01.25 +05:30"), "{india}");
        let utc = draw(trawl_engine::value::ColumnType::Timestamp, "UTC");
        assert!(utc.contains("2026-01-15 04:30:01.25 +00:00"), "{utc}");
        let text = draw(trawl_engine::value::ColumnType::String, "+05:30");
        assert!(text.contains("2026-01-15T04:30:01.250000Z"), "{text}");
    }

    /// A pivot names its columns from data, so a timestamp column can be
    /// called `_severity`. The grid reads it by its type, in the display
    /// zone, not as a severity number.
    #[test]
    fn a_timestamp_column_named_like_severity_draws_in_the_display_zone() {
        let mut app = test_app();
        app.display_zone = "+05:30".parse().unwrap();
        let mut response = make_query_response(
            vec!["_severity", "message"],
            vec![vec![
                Value::String("2026-01-15T04:30:00.000000Z".into()),
                Value::String("request ok".into()),
            ]],
        );
        response.result.columns[0].ty = trawl_engine::value::ColumnType::Timestamp;
        app.tab.result = Some(response);
        app.tab.status = TabStatus::Success { duration_ms: 3 };
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
        terminal.draw(|f| super::render(&mut app, f)).unwrap();
        let screen = terminal.backend().to_string();
        assert!(screen.contains("2026-01-15 10:00:00 +05:30"), "{screen}");
    }

    /// An instant's text varies in length (the fraction drops its trailing
    /// zeros), so a longer reading below the first 50 rows still fits its
    /// column with its offset when the pane shows more than 50 rows.
    #[test]
    fn a_long_instant_below_fifty_visible_rows_keeps_its_offset() {
        let mut app = test_app();
        app.display_zone = "-04:00".parse().unwrap();
        let mut rows: Vec<Vec<Value>> = (0..55)
            .map(|i| {
                vec![
                    Value::String("2026-11-01T05:30:00.000000Z".into()),
                    Value::String(format!("row {i}")),
                ]
            })
            .collect();
        rows.push(vec![
            Value::String("2026-11-01T05:30:00.123456Z".into()),
            Value::String("row 55".into()),
        ]);
        let mut response = make_query_response(vec!["_time", "message"], rows);
        response.result.columns[0].ty = trawl_engine::value::ColumnType::Timestamp;
        app.tab.result = Some(response);
        app.tab.status = TabStatus::Success { duration_ms: 3 };
        let mut terminal = Terminal::new(TestBackend::new(120, 200)).unwrap();
        terminal.draw(|f| super::render(&mut app, f)).unwrap();
        let screen = terminal.backend().to_string();
        assert!(screen.contains("row 55"), "row 55 is on screen:\n{screen}");
        assert!(
            screen.contains("2026-11-01 01:30:00.123456 -04:00"),
            "the longer reading keeps its offset:\n{screen}"
        );
    }

    /// Set only in the child process: the display-zone timechart draws
    /// there, under the `TZ` the parent chose.
    const BERLIN_CHILD: &str = "TRAWL_TUI_BERLIN_CHILD";

    /// The TUI under `TZ=Europe/Berlin` with the display zone `local`, over
    /// a timechart whose two buckets are the fall-back night's two 02:30s.
    /// The machine zone is per process, so the drawing happens in a CHILD
    /// PROCESS: this test binary re-runs itself with the ignored
    /// [`utc_timechart_child_under_berlin`] selected and `TZ` set, and the
    /// child snapshots the chart. The parent fails if the child fails or
    /// never ran.
    #[test]
    fn utc_timechart_arithmetic_survives_local_display() {
        const CHILD: &str = "tui::ui::tests::utc_timechart_child_under_berlin";
        let output =
            std::process::Command::new(std::env::current_exe().expect("locate this test binary"))
                .args([
                    CHILD,
                    "--exact",
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("TZ", "Europe/Berlin")
                .env(BERLIN_CHILD, "1")
                .output()
                .expect("run the child");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "the Berlin child failed or did not run; status {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            output.status,
        );
    }

    /// The subprocess entry point. Without [`BERLIN_CHILD`] it does nothing,
    /// so a run with `--run-ignored` passes it untouched.
    #[test]
    #[ignore = "entry point for the Berlin child process; the test above runs it"]
    fn utc_timechart_child_under_berlin() {
        if std::env::var_os(BERLIN_CHILD).is_none() {
            return;
        }

        let mut app = test_app();
        app.display_zone = trawl_api::zone::DisplayZone::Local;
        let mut response = make_query_response(
            vec!["_time", "count"],
            vec![
                vec![
                    Value::String("2026-10-25T00:30:00.000000Z".into()),
                    Value::Integer(3),
                ],
                vec![
                    Value::String("2026-10-25T01:30:00.000000Z".into()),
                    Value::Integer(5),
                ],
            ],
        );
        response.result.columns[0].ty = trawl_engine::value::ColumnType::Timestamp;
        app.tab.result = Some(response);
        app.tab.status = TabStatus::Success { duration_ms: 4 };
        app.tab.chart_view = crate::tui::state::ChartView::LineChart;
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|f| super::render(&mut app, f)).unwrap();
        let screen = terminal.backend().to_string();

        // Without a zone database both readings would be UTC: say so
        // before the snapshot does.
        for reading in ["2026-10-25 02:30:00 +02:00", "2026-10-25 02:30:00 +01:00"] {
            assert!(screen.contains(reading), "{reading} missing:\n{screen}");
        }
        assert!(screen.contains("span: 1h"), "{screen}");
        insta::assert_snapshot!("utc_timechart_arithmetic_survives_local_display", screen);
    }

    /// Set only in the child process: the grid draws there under
    /// `TZ=America/New_York`.
    const NEW_YORK_CHILD: &str = "TRAWL_TUI_NEW_YORK_CHILD";

    /// The fall-back night's two 01:30s differ only in their offsets, so a
    /// grid column that cuts the offset shows the same text twice. Four
    /// columns in a 100-column terminal leave each an equal share of 19,
    /// shorter than one instant. Drawn in a child process under
    /// `TZ=America/New_York`, as [`utc_timechart_arithmetic_survives_local_display`]
    /// does for Berlin.
    #[test]
    fn grid_keeps_the_offset_of_each_instant() {
        const CHILD: &str = "tui::ui::tests::grid_offset_child_under_new_york";
        let output =
            std::process::Command::new(std::env::current_exe().expect("locate this test binary"))
                .args([
                    CHILD,
                    "--exact",
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("TZ", "America/New_York")
                .env(NEW_YORK_CHILD, "1")
                .output()
                .expect("run the child");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "the New York child failed or did not run; status {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            output.status,
        );
    }

    /// The subprocess entry point. Without [`NEW_YORK_CHILD`] it does
    /// nothing, so a run with `--run-ignored` passes it untouched.
    #[test]
    #[ignore = "entry point for the New York child process; the test above runs it"]
    fn grid_offset_child_under_new_york() {
        if std::env::var_os(NEW_YORK_CHILD).is_none() {
            return;
        }

        let mut app = test_app();
        app.display_zone = trawl_api::zone::DisplayZone::Local;
        let row = |t: &str| {
            vec![
                Value::String(t.into()),
                Value::String("nginx".into()),
                Value::Integer(3),
                Value::String("request ok".into()),
            ]
        };
        let mut response = make_query_response(
            vec!["_time", "host", "count", "message"],
            vec![
                row("2026-11-01T05:30:00.000000Z"),
                row("2026-11-01T06:30:00.000000Z"),
            ],
        );
        response.result.columns[0].ty = trawl_engine::value::ColumnType::Timestamp;
        app.tab.result = Some(response);
        app.tab.status = TabStatus::Success { duration_ms: 4 };
        let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
        terminal.draw(|f| super::render(&mut app, f)).unwrap();
        let screen = terminal.backend().to_string();

        for reading in ["2026-11-01 01:30:00 -04:00", "2026-11-01 01:30:00 -05:00"] {
            assert!(screen.contains(reading), "{reading} missing:\n{screen}");
        }
    }

    #[test]
    fn render_with_error_status() {
        let mut app = test_app();
        app.tab.status = TabStatus::Error {
            message: "parse error: unexpected token".to_owned(),
            details: Vec::new(),
            query: None,
        };
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| super::render(&mut app, f)).unwrap();
        insta::assert_snapshot!(terminal.backend().to_string());
    }

    /// A validation error's one detail has no span and restates the
    /// message, so the pane renders exactly what it renders with no
    /// details at all: the message once, and no stray detail line.
    #[test]
    fn a_spanless_detail_renders_like_no_details() {
        let render = |details: Vec<trawl_client::ErrorDetail>| {
            let mut app = test_app();
            app.tab.editor.insert_text("* | stats countt(x) by host");
            app.tab.status = TabStatus::Error {
                message: "unknown function: countt (did you mean 'count'?)".to_owned(),
                details,
                query: Some("* | stats countt(x) by host".to_owned()),
            };
            let backend = TestBackend::new(80, 24);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|f| super::render(&mut app, f)).unwrap();
            terminal.backend().to_string()
        };
        let spanless = render(vec![trawl_client::ErrorDetail {
            message: "unknown function: countt".to_owned(),
            span: None,
            label: None,
            hint: Some("did you mean 'count'?".to_owned()),
        }]);
        assert_eq!(spanless, render(Vec::new()));
        assert!(!spanless.contains("hint"));
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

        let capped = trawl_api::ScheduleResponse {
            max_runs: Some(60),
            enabled: false,
            ..make_schedule(Some("since_last"), None)
        };
        for schedule in [None, Some(&capped)] {
            let mut app = test_app();
            app.popup = Some(Popup::SetSchedule(Box::new(ScheduleForm::new(
                5,
                "nightly errors".to_owned(),
                schedule,
            ))));

            let backend = TestBackend::new(42, 8);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|f| super::render(&mut app, f)).unwrap();
        }
    }

    /// The popup shows the cap and the paused state it will send back, marked
    /// `(kept)`, so the operator sees what an edit leaves in place. With no
    /// schedule, they are the values a new schedule starts with, unmarked.
    #[test]
    fn render_schedule_popup_shows_the_kept_budget() {
        use crate::tui::state::ScheduleForm;

        const HINT: &str = "Tab next field  Enter save  Esc cancel";

        let draw = |schedule: Option<&trawl_api::ScheduleResponse>| {
            let mut app = test_app();
            app.popup = Some(Popup::SetSchedule(Box::new(ScheduleForm::new(
                5,
                "nightly errors".to_owned(),
                schedule,
            ))));
            let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
            terminal.draw(|f| super::render(&mut app, f)).unwrap();
            terminal.backend().to_string()
        };

        let capped = trawl_api::ScheduleResponse {
            max_runs: Some(60),
            enabled: false,
            ..make_schedule(Some("since_last"), None)
        };
        let screen = draw(Some(&capped));
        assert!(screen.contains("max_runs  60 (kept)"), "{screen}");
        assert!(screen.contains("enabled   no (kept)"), "{screen}");
        assert!(screen.contains(HINT), "{screen}");

        let unlimited = make_schedule(Some("since_last"), None);
        let screen = draw(Some(&unlimited));
        assert!(screen.contains("max_runs  unlimited (kept)"), "{screen}");
        assert!(screen.contains("enabled   yes (kept)"), "{screen}");

        let screen = draw(None);
        assert!(screen.contains("max_runs  unlimited "), "{screen}");
        assert!(screen.contains("enabled   yes "), "{screen}");
        assert!(!screen.contains("(kept)"), "{screen}");
        assert!(screen.contains(HINT), "{screen}");
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

    // -- sender text sink sweep ----------------------------------------------

    use ratatui::Frame;
    use ratatui::buffer::Buffer;
    use ratatui::layout::{Constraint, Layout, Margin, Rect};

    use crate::tui::App;
    use crate::tui::palette::{PaletteAction, PaletteCategory, PaletteItem, refilter};
    use crate::tui::state::{MainTab, SavedFocus};

    /// What no sink may draw: a bidi override, a zero-width space and a soft
    /// hyphen. ratatui drops control characters by itself, so ESC would
    /// prove nothing.
    const FORBIDDEN: [char; 3] = ['\u{202e}', '\u{200b}', '\u{00ad}'];

    /// Sender text for one field: a two-letter tag, then U+202E, U+200B,
    /// U+00AD and a line feed, each followed by a digit.
    fn hostile(tag: &str) -> String {
        format!("{tag}\u{202e}1\u{200b}2\u{00ad}3\n4")
    }

    /// [`hostile`] text drawn on one line: U+FFFD in place of all four.
    fn drawn(tag: &str) -> String {
        format!("{tag}\u{fffd}1\u{fffd}2\u{fffd}3\u{fffd}4")
    }

    /// [`hostile`] query text drawn where its line feed folds to a space.
    fn drawn_folded(tag: &str) -> String {
        format!("{tag}\u{fffd}1\u{fffd}2\u{fffd}3 4")
    }

    /// The first drawn line of [`hostile`] query text where its line feed
    /// breaks the line.
    fn drawn_first_line(tag: &str) -> String {
        format!("{tag}\u{fffd}1\u{fffd}2\u{fffd}3")
    }

    /// Where a sub-sink draws. A fragment is looked for there only, so the
    /// already sanitized copy of the same text elsewhere on screen (the
    /// grid behind a popup, the tree beside a detail pane) cannot stand in
    /// for it.
    #[derive(Debug, Clone, Copy)]
    enum Region {
        Screen,
        Popup,
        Editor,
        Results,
        /// The panel of a non-Query tab, or the dashboard content.
        Panel,
        /// The left column of a split panel whose left column takes this
        /// percentage.
        PanelLeft(u16),
        /// The right column of the same split.
        PanelRight(u16),
    }

    impl Region {
        fn rect(self, app: &App, screen: Rect) -> Rect {
            let panel = || app.layout.panel.expect("the panel is laid out");
            let columns = |left: u16| {
                let inner = panel().inner(Margin::new(1, 1));
                Layout::horizontal([
                    Constraint::Percentage(left),
                    Constraint::Percentage(100 - left),
                ])
                .areas::<2>(inner)
            };
            match self {
                Self::Screen => screen,
                Self::Popup => app.layout.popup.expect("a popup is open"),
                Self::Editor => app.layout.editor.expect("the editor is laid out"),
                Self::Results => app.layout.results.expect("the results are laid out"),
                Self::Panel => panel(),
                Self::PanelLeft(left) => columns(left)[0],
                Self::PanelRight(left) => columns(left)[1],
            }
        }
    }

    /// The symbols of `rect`, row after row.
    fn region_text(buffer: &Buffer, rect: Rect) -> String {
        (rect.top()..rect.bottom())
            .map(|y| {
                (rect.left()..rect.right())
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Everything a draw must leave as it was built: the caches, the popup
    /// (palette items and their actions, popup text), the schedule popup's
    /// submission, and the editor buffer, ghost and validation text.
    fn sender_state(app: &App) -> String {
        let submission = match app.popup {
            Some(Popup::SetSchedule(ref form)) => format!("{:?}", form.submission()),
            _ => String::new(),
        };
        format!(
            "{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{submission}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}",
            app.saved_cache,
            app.history_cache,
            app.schema_cache,
            app.panel.schema,
            app.panel.saved_detail,
            app.dashboard.cache,
            app.dashboard.last_error,
            app.popup,
            app.tab.result,
            app.tab.editor.text(),
            app.tab.ghost,
            app.tab.validation_errors,
            app.results_search.as_ref().map(|s| &s.query),
            app.server_version,
        )
    }

    type DrawFn = fn(&mut App, &mut Frame<'_>);

    /// One sweep case: how to build its app, how to draw it, and the
    /// literal each poisoned sub-sink must draw at 120x40.
    struct SinkCase {
        /// The surface and the branch drawn.
        name: &'static str,
        build: Box<dyn Fn() -> App>,
        draw: DrawFn,
        /// Sub-sink (with its row in the sink table), region, fragment.
        expect: Vec<(&'static str, Region, String)>,
    }

    impl SinkCase {
        /// A case drawn through the whole UI.
        fn new(
            name: &'static str,
            build: impl Fn() -> App + 'static,
            expect: Vec<(&'static str, Region, String)>,
        ) -> Self {
            Self {
                name,
                build: Box::new(build),
                draw: super::render,
                expect,
            }
        }
    }

    /// Draw `case` at 120x40 and at 40x12. Neither may draw a forbidden
    /// character or change the data behind the screen; 120x40 must draw
    /// every expected fragment in its region. Each failure names the case,
    /// the sub-sink and the size.
    fn sweep_case(case: &SinkCase) -> Vec<String> {
        let mut failures = Vec::new();
        for (width, height) in [(120, 40), (40, 12)] {
            let size = format!("{width}x{height}");
            let mut app = (case.build)();
            let built = sender_state(&app);
            let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
            terminal.draw(|f| (case.draw)(&mut app, f)).expect("render");
            let buffer = terminal.backend().buffer();
            let screen = region_text(buffer, buffer.area);
            for forbidden in FORBIDDEN {
                if screen.contains(forbidden) {
                    failures.push(format!(
                        "{}, {size}: U+{:04X} drawn:\n{screen}",
                        case.name,
                        u32::from(forbidden)
                    ));
                }
            }
            if sender_state(&app) != built {
                failures.push(format!(
                    "{}, {size}: drawing changed the data behind the screen",
                    case.name
                ));
            }
            if width == 120 {
                for (sub_sink, region, fragment) in &case.expect {
                    let text = region_text(buffer, region.rect(&app, buffer.area));
                    if !text.contains(fragment.as_str()) {
                        failures.push(format!(
                            "{}, {sub_sink}, {size}: {fragment:?} not drawn in {region:?}:\n{text}",
                            case.name
                        ));
                    }
                }
            }
        }
        failures
    }

    /// The Schema tab over two services. `sa` holds a common field `cf` and
    /// a unique field `uf`, and is expanded; `beta` shares `cf`. So the tree
    /// reads: 0 common header, 1 `cf`, 2 `sa`, 3 `uf`, 4 `beta`.
    fn schema_app(selected: usize) -> App {
        use crate::tui::state::SchemaBrowser;
        use trawl_api::{DailyCount, ServiceColumnStats, ServiceSchema};

        let column =
            |name: String, data_type: String, range: Option<(String, String)>| ServiceColumnStats {
                name,
                data_type,
                null_count: 0,
                total_count: 10,
                min_value: range.as_ref().map(|r| r.0.clone()),
                max_value: range.map(|r| r.1),
                compressed_bytes: 64,
            };
        // The unique field's range is longer than 40 bytes, so it takes the
        // two-line branch; the common field's takes the one-line branch.
        let sa = ServiceSchema {
            name: hostile("sa"),
            columns: vec![
                column(
                    hostile("cf"),
                    hostile("ct"),
                    Some((hostile("cn"), hostile("cx"))),
                ),
                column(
                    hostile("uf"),
                    hostile("ut"),
                    Some((hostile("un") + "-tail-tail", hostile("ux") + "-tail-tail")),
                ),
            ],
            earliest_date: Some(hostile("ea")),
            latest_date: Some(hostile("la")),
            file_count: 1,
            total_bytes: 128,
            total_events: 10,
            daily_event_counts: vec![
                DailyCount {
                    date: hostile("fa"),
                    count: 4,
                },
                DailyCount {
                    date: hostile("lz"),
                    count: 6,
                },
            ],
            degraded_fields: Vec::new(),
        };
        let beta = ServiceSchema {
            name: "beta".to_owned(),
            columns: vec![column(hostile("cf"), "VARCHAR".to_owned(), None)],
            earliest_date: None,
            latest_date: None,
            file_count: 1,
            total_bytes: 64,
            total_events: 5,
            daily_event_counts: Vec::new(),
            degraded_fields: Vec::new(),
        };
        let mut browser = SchemaBrowser::new(vec![sa, beta]);
        assert_eq!(browser.common_fields.len(), 1, "only cf is common");
        browser.expanded.insert(hostile("sa"));
        browser.selected = selected;
        let mut app = test_app();
        app.main_tab = MainTab::Schema;
        app.panel.schema = Some(browser);
        app
    }

    /// Rows 1, 2, 3 and 13a: the schema detail pane at every selection
    /// kind, and the filter bar.
    #[allow(clippy::too_many_lines)] // a table of cases, one sub-sink per entry
    fn schema_cases() -> Vec<SinkCase> {
        let detail = Region::PanelRight(55);
        vec![
            SinkCase::new(
                "schema detail, common header selected",
                || schema_app(0),
                vec![
                    ("row 1 common field name", detail, drawn("cf")),
                    (
                        "row 1 common field type",
                        detail,
                        format!("({})", drawn("ct")),
                    ),
                ],
            ),
            SinkCase::new(
                "schema detail, common field selected",
                || schema_app(1),
                vec![
                    ("row 2 field title", detail, drawn("cf")),
                    (
                        "row 2 common field type",
                        detail,
                        format!("type:       {}", drawn("ct")),
                    ),
                    (
                        "row 3 range min, one line",
                        detail,
                        format!("range:      {}", drawn("cn")),
                    ),
                    (
                        "row 3 range max, one line",
                        detail,
                        format!("\u{2013} {}", drawn("cx")),
                    ),
                ],
            ),
            SinkCase::new(
                "schema detail, service selected",
                || schema_app(2),
                vec![
                    ("row 1 service title", detail, drawn("sa")),
                    (
                        "row 13a earliest date",
                        detail,
                        format!("data:      {}", drawn("ea")),
                    ),
                    (
                        "row 13a latest date",
                        detail,
                        format!("\u{2192} {}", drawn("la")),
                    ),
                    (
                        "row 1 common list",
                        detail,
                        format!("common: {}", drawn("cf")),
                    ),
                    (
                        "row 1 unique list",
                        detail,
                        format!("unique: {}", drawn("uf")),
                    ),
                    ("row 2 chart first date", detail, drawn("fa")),
                    ("row 2 chart last date", detail, drawn("lz")),
                ],
            ),
            SinkCase::new(
                "schema detail, service field selected",
                || schema_app(3),
                vec![
                    ("row 2 field title", detail, drawn("uf")),
                    (
                        "row 2 field type",
                        detail,
                        format!("type:       {}", drawn("ut")),
                    ),
                    (
                        "row 2 in service",
                        detail,
                        format!("in:         {}", drawn("sa")),
                    ),
                    (
                        "row 3 range min, two lines",
                        detail,
                        format!("range:      {}-tail-tail \u{2013}", drawn("un")),
                    ),
                    (
                        "row 3 range max, two lines",
                        detail,
                        format!("            {}-tail-tail", drawn("ux")),
                    ),
                ],
            ),
            SinkCase::new(
                "schema filter, active",
                || {
                    let mut app = schema_app(0);
                    let schema = app.panel.schema.as_mut().unwrap();
                    schema.filter = hostile("sf");
                    schema.filter_active = true;
                    app
                },
                vec![(
                    "filter input",
                    Region::PanelLeft(55),
                    format!("/ {}\u{2588}", drawn("sf")),
                )],
            ),
            SinkCase::new(
                "schema filter, inactive",
                || {
                    let mut app = schema_app(0);
                    app.panel.schema.as_mut().unwrap().filter = hostile("sg");
                    app
                },
                vec![(
                    "filter input",
                    Region::PanelLeft(55),
                    format!("/ {}", drawn("sg")),
                )],
            ),
        ]
    }

    /// The Saved tab over two poisoned saved queries: the first scheduled
    /// with a window and a lag and one run, the second unscheduled with a
    /// name too long for the list.
    fn hostile_saved_app(focus: SavedFocus) -> App {
        let mut sched = make_schedule(Some("since_last"), Some(("5m", 300)));
        sched.interval = hostile("iv");
        sched.window = Some(hostile("wn"));
        sched.lag = Some(hostile("lg"));
        sched.covered_through = None;
        let mut run = make_run(41, 45);
        run.window_kind = Some(hostile("wk"));
        let mut app = saved_app(Some(sched), vec![run]);
        let queries = &mut app.saved_cache.as_mut().unwrap().queries;
        queries[0].name = hostile("sn");
        queries[0].query = hostile("sq");
        let mut long = make_saved(None);
        long.id = 8;
        long.name = format!("{}{}", hostile("sl"), "x".repeat(80));
        queries.push(long);
        app.panel.saved_focus = focus;
        app
    }

    /// Rows 8 and 13b: the saved list, its detail and a run's results.
    fn saved_cases() -> Vec<SinkCase> {
        use crate::tui::state::LoadedRun;
        use trawl_api::value::{Column, ColumnType, QueryResult, Value};

        let list = Region::PanelLeft(45);
        let detail = Region::PanelRight(45);
        vec![
            SinkCase::new(
                "saved list",
                || hostile_saved_app(SavedFocus::List),
                vec![
                    ("row 13b saved name, fits", list, drawn("sn")),
                    (
                        "row 13b saved name, clipped",
                        list,
                        format!("{}x", drawn("sl")),
                    ),
                    ("row 13b badge interval", list, format!("[{}", drawn("iv"))),
                    (
                        "row 13b badge window",
                        list,
                        format!("window {}]", drawn("wn")),
                    ),
                ],
            ),
            SinkCase::new(
                "saved detail",
                || hostile_saved_app(SavedFocus::Detail),
                vec![
                    ("row 8 saved name", detail, drawn("sn")),
                    (
                        "row 8 saved query",
                        detail,
                        format!("query  {}", drawn_folded("sq")),
                    ),
                    (
                        "row 8 schedule interval",
                        detail,
                        format!("every {}", drawn("iv")),
                    ),
                    (
                        "row 8 schedule window",
                        detail,
                        format!("window {}", drawn("wn")),
                    ),
                    ("row 8 schedule lag", detail, format!("lag {}", drawn("lg"))),
                    ("row 8 run window kind", detail, drawn("wk")),
                ],
            ),
            SinkCase::new(
                "saved run results",
                || {
                    let mut app = hostile_saved_app(SavedFocus::RunResults);
                    app.panel.saved_detail.as_mut().unwrap().result = Some(LoadedRun {
                        run_id: 41,
                        result: QueryResult {
                            columns: vec![Column::new(hostile("rh"), ColumnType::String)],
                            rows: vec![vec![Value::String(hostile("rv"))]],
                        },
                    });
                    app
                },
                vec![
                    ("run result header", detail, drawn("rh")),
                    ("run result cell", detail, drawn("rv")),
                ],
            ),
        ]
    }

    /// Rows 9 to 12 and the result search: the Query tab and the history
    /// detail pane.
    #[allow(clippy::too_many_lines)] // a table of cases, one sub-sink per entry
    fn query_cases() -> Vec<SinkCase> {
        vec![
            SinkCase::new(
                "query tab, no result",
                || {
                    let mut app = test_app();
                    app.server_version = Some(hostile("vr"));
                    app
                },
                vec![("row 9 server version", Region::Results, drawn("vr"))],
            ),
            SinkCase::new(
                "query editor",
                || {
                    let mut app = test_app();
                    app.tab.editor.insert_text(&hostile("ed"));
                    app
                },
                vec![(
                    "row 10 highlighted query",
                    Region::Editor,
                    drawn_first_line("ed"),
                )],
            ),
            SinkCase::new(
                "query editor, ghost inside the line",
                || {
                    let mut app = test_app();
                    app.tab.editor.insert_text("abc");
                    app.tab.editor.cursor = (0, 1);
                    app.tab.ghost = Some(crate::tui::autocomplete::Completion {
                        ghost_text: hostile("gm"),
                        insert_text: String::new(),
                        replace_len: 0,
                        cursor_offset: None,
                    });
                    app
                },
                vec![(
                    "row 11 ghost text",
                    Region::Editor,
                    format!("a{}bc", drawn("gm")),
                )],
            ),
            SinkCase::new(
                "query editor, ghost on an empty line",
                || {
                    let mut app = test_app();
                    app.tab.ghost = Some(crate::tui::autocomplete::Completion {
                        ghost_text: hostile("ga"),
                        insert_text: String::new(),
                        replace_len: 0,
                        cursor_offset: None,
                    });
                    app
                },
                vec![("row 11 ghost text", Region::Editor, drawn("ga"))],
            ),
            SinkCase::new(
                "query editor, validation hint",
                || {
                    let mut app = test_app();
                    app.tab.editor.insert_text("abc");
                    app.tab.validation_errors = vec![trawl_core::parser::ParseError {
                        message: hostile("vm"),
                        span: 0..1,
                        label: None,
                        hint: Some(hostile("vh")),
                    }];
                    app
                },
                vec![
                    ("row 12 validation message", Region::Screen, drawn("vm")),
                    (
                        "row 12 validation hint",
                        Region::Screen,
                        format!("({})", drawn("vh")),
                    ),
                ],
            ),
            SinkCase::new(
                "result search bar",
                || {
                    let mut app = test_app();
                    let mut search = crate::tui::state::ResultsSearch::new();
                    search.query = hostile("rq");
                    search.input_active = true;
                    app.results_search = Some(search);
                    app
                },
                vec![("search input", Region::Results, format!("/{}", drawn("rq")))],
            ),
            SinkCase::new(
                "history tab",
                || {
                    let mut app = test_app();
                    app.main_tab = MainTab::History;
                    app.history_cache = Some(trawl_api::HistoryResponse {
                        entries: vec![trawl_api::HistoryEntryResponse {
                            id: 1,
                            query: hostile("hq"),
                            executed_at: "2026-03-18T14:30:00Z".to_owned(),
                            duration_ms: 3,
                            row_count: 0,
                            status: trawl_client::QueryStatus::Success,
                        }],
                        total: 1,
                    });
                    app
                },
                vec![(
                    "row 10 history detail query",
                    Region::PanelRight(55),
                    drawn_first_line("hq"),
                )],
            ),
        ]
    }

    /// The Query tab holding one row: a well-known `message` field and a
    /// field whose name is poisoned. Both values are poisoned.
    fn event_app() -> App {
        let mut app = test_app();
        let name = hostile("en");
        app.tab.result = Some(make_query_response(
            vec!["message", &name],
            vec![vec![
                Value::String(hostile("mv")),
                Value::String(hostile("ev")),
            ]],
        ));
        app.tab.status = TabStatus::Success { duration_ms: 1 };
        app
    }

    /// The schedule popup open on a saved query whose name and schedule the
    /// server sent poisoned.
    fn hostile_schedule_app() -> App {
        use crate::tui::state::ScheduleForm;

        let mut sched = make_schedule(Some("since_last"), Some(("5m", 300)));
        sched.interval = hostile("pi");
        sched.window = Some(hostile("pw"));
        sched.lag = Some(hostile("pl"));
        let mut app = test_app();
        app.popup = Some(Popup::SetSchedule(Box::new(ScheduleForm::new(
            7,
            hostile("st"),
            Some(&sched),
        ))));
        app
    }

    /// Rows 4 to 7 and 13c, the save-name input, and help: every popup
    /// variant but the palette.
    fn popup_cases() -> Vec<SinkCase> {
        use crate::tui::state::{ColumnConfig, SimpleEditor};

        let popup = Region::Popup;
        vec![
            SinkCase::new(
                "event detail popup",
                || {
                    let mut app = event_app();
                    app.popup = Some(Popup::EventDetail {
                        row_index: 0,
                        scroll: 0,
                    });
                    app
                },
                vec![
                    ("row 4 well-known value", popup, drawn("mv")),
                    ("row 4 field name", popup, drawn("en")),
                    ("row 4 field value", popup, drawn("ev")),
                ],
            ),
            SinkCase::new(
                "column picker popup",
                || {
                    let mut app = test_app();
                    let name = hostile("cp");
                    let response =
                        make_query_response(vec![&name], vec![vec![Value::String("x".into())]]);
                    app.tab.column_config = Some(ColumnConfig::init(&response.result.columns));
                    app.tab.result = Some(response);
                    app.tab.status = TabStatus::Success { duration_ms: 1 };
                    app.popup = Some(Popup::ColumnPicker {
                        selected: 0,
                        scroll: 0,
                    });
                    app
                },
                vec![("row 5 column label", popup, format!("[x] {}", drawn("cp")))],
            ),
            SinkCase::new(
                "error popup",
                || {
                    let mut app = test_app();
                    app.popup = Some(Popup::Error {
                        message: hostile("er"),
                    });
                    app
                },
                vec![("row 6 error message", popup, drawn("er"))],
            ),
            SinkCase::new(
                "confirm-delete popup",
                || {
                    let mut app = test_app();
                    app.popup = Some(Popup::ConfirmDelete {
                        saved_id: 7,
                        name: hostile("dn"),
                    });
                    app
                },
                vec![("row 7 saved name", popup, drawn("dn"))],
            ),
            SinkCase::new(
                "schedule popup, loaded from the server",
                hostile_schedule_app,
                vec![
                    ("row 7 schedule title", popup, drawn("st")),
                    ("row 13c interval row", popup, drawn("pi")),
                    ("row 13c window row", popup, drawn("pw")),
                    ("row 13c lag row", popup, drawn("pl")),
                ],
            ),
            SinkCase::new(
                "save-query popup",
                || {
                    let mut app = test_app();
                    let mut editor = SimpleEditor::new_single_line();
                    editor.insert_text(&hostile("nv"));
                    app.popup = Some(Popup::SaveQuery { editor });
                    app
                },
                vec![("save-name input", popup, format!("> {}", drawn("nv")))],
            ),
            // Fixed text only; drawn so that every popup variant goes
            // through the sweep's no-forbidden-character check.
            SinkCase::new(
                "help popup",
                || {
                    let mut app = test_app();
                    app.popup = Some(Popup::Help { scroll: 0 });
                    app
                },
                Vec::new(),
            ),
        ]
    }

    /// A saved query, a history entry and a service, each with a poisoned
    /// label. The labels start with `k`, which no detail holds, so input
    /// `k` matches the labels alone and every label takes the matched
    /// branch.
    fn palette_items() -> Vec<PaletteItem> {
        vec![
            PaletteItem {
                label: hostile("kl"),
                detail: Some(hostile("pd")),
                category: PaletteCategory::SavedQuery,
                shortcut: None,
                action: PaletteAction::LoadQuery(hostile("pd")),
            },
            PaletteItem {
                label: hostile("kh"),
                detail: Some("3ms, 1 rows".to_owned()),
                category: PaletteCategory::History,
                shortcut: None,
                action: PaletteAction::LoadQuery(hostile("kh")),
            },
            PaletteItem {
                label: hostile("ks"),
                detail: Some(hostile("pt")),
                category: PaletteCategory::Service,
                shortcut: None,
                action: PaletteAction::DrillService(hostile("ks")),
            },
        ]
    }

    /// The palette open over the Query tab, filtered for `input`, with the
    /// item labelled `selected` selected, or the first when none is.
    fn palette_app(
        input: &str,
        items: Vec<PaletteItem>,
        selected: &str,
        ghost: Option<String>,
    ) -> App {
        // Column mode filters on the text after the dot.
        let filter = input.split_once('.').map_or(input, |(_, after)| after);
        let filtered = refilter(filter, &items);
        let selected = filtered
            .iter()
            .position(|f| items[f.item_index].label == selected)
            .unwrap_or(0);
        let mut app = test_app();
        app.popup = Some(Popup::CommandPalette {
            input: input.to_owned(),
            cursor: input.chars().count(),
            selected,
            scroll: 0,
            items,
            filtered,
            ghost,
        });
        app
    }

    /// [`palette_items`] filtered for `k`, which matches every label, so
    /// every label is drawn on the matched branch.
    fn matched_palette_app(selected: &str) -> App {
        let app = palette_app("k", palette_items(), selected, None);
        let Some(Popup::CommandPalette { ref filtered, .. }) = app.popup else {
            unreachable!("palette_app opens the palette");
        };
        assert!(
            filtered.len() == 3 && filtered.iter().all(|f| !f.match_positions.is_empty()),
            "every label matches k: {filtered:?}"
        );
        app
    }

    /// Rows 13 and 13d, and the palette input and its ghost.
    #[allow(clippy::too_many_lines)] // a table of cases, one sub-sink per entry
    fn palette_cases() -> Vec<SinkCase> {
        let popup = Region::Popup;
        let column_items = || {
            vec![PaletteItem {
                label: hostile("cl"),
                detail: Some(hostile("cd")),
                category: PaletteCategory::Service,
                shortcut: None,
                action: PaletteAction::InsertAtCursor(hostile("cl")),
            }]
        };
        vec![
            SinkCase::new(
                "palette, grouped, saved query selected",
                || palette_app("", palette_items(), &hostile("kl"), None),
                vec![
                    (
                        "row 13 saved label, unmatched, selected",
                        popup,
                        drawn("kl"),
                    ),
                    ("row 13 saved detail line", popup, drawn_folded("pd")),
                    (
                        "row 13 history label, unmatched, unselected",
                        popup,
                        drawn("kh"),
                    ),
                    (
                        "row 13 service label, unmatched, unselected",
                        popup,
                        drawn("ks"),
                    ),
                    ("row 13 service hint, unselected", popup, drawn("pt")),
                ],
            ),
            SinkCase::new(
                "palette, grouped, service selected",
                || palette_app("", palette_items(), &hostile("ks"), None),
                vec![
                    (
                        "row 13 saved label, unmatched, unselected",
                        popup,
                        drawn("kl"),
                    ),
                    (
                        "row 13 service label, unmatched, selected",
                        popup,
                        drawn("ks"),
                    ),
                    ("row 13 service hint, selected", popup, drawn("pt")),
                ],
            ),
            SinkCase::new(
                "palette, matched, saved query selected",
                || matched_palette_app(&hostile("kl")),
                vec![
                    ("row 13 saved label, matched, selected", popup, drawn("kl")),
                    ("row 13 saved detail line", popup, drawn_folded("pd")),
                    (
                        "row 13 history label, matched, unselected",
                        popup,
                        drawn("kh"),
                    ),
                    (
                        "row 13 service label, matched, unselected",
                        popup,
                        drawn("ks"),
                    ),
                    ("row 13 service hint, unselected", popup, drawn("pt")),
                ],
            ),
            SinkCase::new(
                "palette, matched, history selected",
                || matched_palette_app(&hostile("kh")),
                vec![
                    (
                        "row 13 history label, matched, selected",
                        popup,
                        drawn("kh"),
                    ),
                    (
                        "row 13 saved label, matched, unselected",
                        popup,
                        drawn("kl"),
                    ),
                    (
                        "row 13 service label, matched, unselected",
                        popup,
                        drawn("ks"),
                    ),
                ],
            ),
            SinkCase::new(
                "palette, column mode",
                move || {
                    palette_app(
                        &format!("{}.", hostile("cs")),
                        column_items(),
                        &hostile("cl"),
                        None,
                    )
                },
                vec![
                    (
                        "row 13d service header",
                        popup,
                        format!("{} \u{2014} Columns", drawn("cs")),
                    ),
                    (
                        "row 13 column label, unmatched, selected",
                        popup,
                        drawn("cl"),
                    ),
                    ("row 13 column hint, selected", popup, drawn("cd")),
                    ("palette input", popup, format!("> {}.", drawn("cs"))),
                ],
            ),
            SinkCase::new(
                "palette, input with ghost",
                || palette_app(&hostile("ip"), palette_items(), "", Some(hostile("gp"))),
                vec![
                    ("palette input", popup, format!("> {}", drawn("ip"))),
                    ("palette ghost", popup, drawn("gp")),
                ],
            ),
        ]
    }

    /// A dashboard snapshot whose drawn remote strings are poisoned.
    fn hostile_dashboard() -> trawl_client::DashboardSnapshot {
        use trawl_api::{
            Capacity, HeadroomReading, PressureEvidence, StorageMeasurement,
            StorageMeasurementStatus,
        };
        let measured = StorageMeasurement {
            status: StorageMeasurementStatus::Complete,
            sample_age_secs: Some(2),
        };
        trawl_client::DashboardSnapshot {
            hostname: hostile("dh"),
            listen_addr: hostile("da"),
            uptime_secs: 60,
            version: hostile("dv"),
            healthy: true,
            pool_capacity: 4,
            pool_active: 1,
            pool_retained: 0,
            hot_buffer_events: 10,
            hot_buffer_max_events: 100,
            hot_buffer_bytes: 10,
            hot_buffer_max_bytes: 100,
            hot_buffer_batches: 1,
            total_queries: 2,
            query_rate: 0.0,
            query_errors: 0,
            query_timeouts: 0,
            ingest_events: 0,
            ingest_rate: 0.0,
            ingest_rejected: 0,
            syslog_enabled: false,
            syslog_events_udp: 0,
            syslog_events_tcp: 0,
            syslog_rate: 0.0,
            syslog_parse_errors: 0,
            syslog_dropped: 0,
            syslog_tcp_connections: 0,
            wal_files: 0,
            wal_bytes: 0,
            wal_measurement: measured,
            last_compaction_secs: None,
            compaction_runs: 0,
            compaction_errors: 0,
            parquet_files: 0,
            parquet_bytes: 0,
            parquet_measurement: measured,
            capacity: Capacity {
                headroom: HeadroomReading {
                    measurement: measured,
                    filesystems: Vec::new(),
                },
                pressure: PressureEvidence {
                    removals_age: 0,
                    removals_disk_pressure: 0,
                    pressure_attempts: 0,
                    last_sweep: None,
                },
                environments: Vec::new(),
                growth_excluded: Vec::new(),
            },
            sse_active: 0,
            sse_max: 8,
            scheduler_enabled: false,
            scheduler_schedules: 0,
            recent_queries: vec![trawl_client::CompletedQuerySnapshot {
                id: 2,
                user: "admin".to_owned(),
                query: hostile("dc"),
                duration_ms: 3,
                rows: Some(1),
                error: None,
                timed_out: false,
            }],
            active_queries: vec![trawl_client::ActiveQuerySnapshot {
                id: 3,
                user: hostile("du"),
                role: "admin".to_owned(),
                query: hostile("dq"),
                running_ms: 40,
            }],
        }
    }

    /// The Dashboard tab with `cache` and `last_error`.
    fn dashboard_app(
        cache: Option<trawl_client::DashboardSnapshot>,
        last_error: Option<String>,
    ) -> App {
        let mut app = test_app();
        app.main_tab = MainTab::Dashboard;
        app.dashboard.is_admin = true;
        app.dashboard.cache = cache;
        app.dashboard.last_error = last_error;
        app
    }

    /// Rows 14 and 15: the dashboard fresh, stale and failed.
    fn dashboard_cases() -> Vec<SinkCase> {
        let panel = Region::Panel;
        vec![
            SinkCase::new(
                "dashboard, fresh",
                || dashboard_app(Some(hostile_dashboard()), None),
                vec![
                    ("row 15 version", panel, format!("v{}", drawn("dv"))),
                    ("row 15 hostname", panel, drawn("dh")),
                    ("row 15 listen address", panel, drawn("da")),
                    ("row 15 active user", panel, drawn("du")),
                    ("row 15 active query", panel, drawn_folded("dq")),
                    ("row 15 completed query", panel, drawn_folded("dc")),
                ],
            ),
            SinkCase::new(
                "dashboard, stale",
                || dashboard_app(Some(hostile_dashboard()), Some(hostile("df"))),
                vec![(
                    "row 14 stale footer",
                    panel,
                    format!("[stale] {}", drawn("df")),
                )],
            ),
            SinkCase::new(
                "dashboard, error without a snapshot",
                || dashboard_app(None, Some(hostile("de"))),
                vec![(
                    "row 14 error line",
                    panel,
                    format!("Error: {}", drawn("de")),
                )],
            ),
        ]
    }

    /// Every sink the TUI draws sender text through, at 120x40 and 40x12:
    /// no U+202E, U+200B or U+00AD anywhere on screen, each poisoned
    /// sub-sink's own U+FFFD fragment in its own region at 120x40, and the
    /// data behind the screen as it was built.
    #[test]
    fn terminal_tui_sender_sink_matrix() {
        let mut cases = Vec::new();
        cases.extend(schema_cases());
        cases.extend(saved_cases());
        cases.extend(query_cases());
        cases.extend(popup_cases());
        cases.extend(palette_cases());
        cases.extend(dashboard_cases());
        // The overlay has no caller, so it is drawn directly.
        cases.push(SinkCase {
            name: "saved overlay, drawn directly",
            build: Box::new(|| {
                let mut app = test_app();
                let mut saved = make_saved(None);
                saved.name = hostile("on");
                saved.query = hostile("oq");
                app.saved_cache = Some(trawl_api::ListSavedResponse {
                    queries: vec![saved],
                });
                app
            }),
            draw: |app, frame| super::saved::render(app, frame),
            expect: vec![
                ("overlay name", Region::Screen, drawn("on")),
                ("overlay query", Region::Screen, drawn("oq")),
            ],
        });

        let failures: Vec<String> = cases.iter().flat_map(sweep_case).collect();
        assert!(
            failures.is_empty(),
            "{} failure(s) over {} cases:\n\n{}",
            failures.len(),
            cases.len(),
            failures.join("\n\n")
        );

        // What was put in comes back byte for byte: the schedule popup's
        // submission and the palette's action arguments.
        let mut app = hostile_schedule_app();
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("terminal");
        terminal
            .draw(|f| super::render(&mut app, f))
            .expect("render");
        let Some(Popup::SetSchedule(ref form)) = app.popup else {
            panic!("the schedule popup is open");
        };
        let sent = form.submission().expect("an interval");
        assert_eq!(
            (sent.interval, sent.window, sent.lag),
            (hostile("pi"), Some(hostile("pw")), Some(hostile("pl"))),
            "the schedule popup sends the loaded text"
        );
        let mut app = matched_palette_app(&hostile("kl"));
        terminal
            .draw(|f| super::render(&mut app, f))
            .expect("render");
        let Some(Popup::CommandPalette { ref items, .. }) = app.popup else {
            panic!("the palette is open");
        };
        assert!(matches!(&items[0].action, PaletteAction::LoadQuery(q) if *q == hostile("pd")));
        assert!(matches!(&items[2].action, PaletteAction::DrillService(s) if *s == hostile("ks")));
    }
}
