// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Scheduler-owned report windows, end to end (ADR-0018 rulings 6-14).
//!
//! Real postgres, real `DuckDB`, real fleet keystore, and an injected clock:
//! every test drives `poll_and_execute` with explicit instants instead of
//! waiting for them. Coverage is a claim about hours, so a wall-clock test
//! could only prove the parts that fit inside a few seconds — which is
//! none of the interesting ones.
//!
//! The fixture is one service (`fx`) whose events sit on and around the
//! window boundaries: the instant T0 itself, one microsecond either side
//! of it, and the same again at T0+2h. Half-open windows `[start, end)`
//! are what those events test — a boundary event has to land in exactly
//! one of two consecutive runs, and a microsecond on the wrong side of a
//! bound is a lost or duplicated log line in production.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, TimeZone as _, Utc};
use fleet_auth::{KeyStore, PrincipalKind};
use sqlx::PgPool;
use trawl_api::value::Value;
use trawl_server::config::SchedulerConfig;
use trawl_server::deadline::Deadline;
use trawl_server::pool::ExecutorPool;
use trawl_server::report_window::{
    ScheduleWindow, WindowKind, format_window_bound, truncate_to_micros,
};
use trawl_server::scheduler::poll_and_execute;
use trawl_server::store::{ReportRun, RunStatus, SavedQueryStore, ScheduleStore, StorageState};

/// The saved DSL every windowed schedule here runs.
const DSL: &str = "service=fx | table _time";

/// The service and env the fixture parquet is written under.
const SERVICE: &str = "fx";
const ENV: &str = "lab";
const DATE: &str = "2026-03-14";

/// A query timeout no test should ever reach.
const TIMEOUT_SECS: u64 = 30;

// ---------------------------------------------------------------------------
// instants
// ---------------------------------------------------------------------------

/// The instant every window in this file is measured from: 03:00 UTC, an
/// hour into the fixture's day so that `T0 - 1h` is still inside it.
fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 3, 14, 3, 0, 0).unwrap()
}

/// `T0 + offset`.
fn at(offset: Duration) -> DateTime<Utc> {
    t0() + offset
}

fn hours(n: i64) -> Duration {
    Duration::hours(n)
}

fn mins(n: i64) -> Duration {
    Duration::minutes(n)
}

fn micros(n: i64) -> Duration {
    Duration::microseconds(n)
}

/// Every `_time` the fixture carries, as offsets from T0.
///
/// The three outside the tiled range (`-2h`, `+2h`, `+3h`) are there to be
/// EXCLUDED: a window that swallowed a neighbour would still look right
/// against a fixture that only held events it was supposed to match.
fn fixture_offsets() -> Vec<Duration> {
    vec![
        hours(-2),
        hours(-1),
        hours(-1) + micros(1),
        mins(-30),
        micros(-1),
        Duration::zero(),
        micros(1),
        mins(30),
        hours(1),
        hours(1) + mins(30),
        hours(2) - micros(1),
        hours(2),
        hours(3),
    ]
}

/// The fixture instants inside `[start, end)`, sorted — what a run over
/// that window must return, computed from the fixture rather than
/// hand-listed per test.
fn expected_rows(start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<DateTime<Utc>> {
    let mut times: Vec<DateTime<Utc>> = fixture_offsets()
        .into_iter()
        .map(at)
        .filter(|t| *t >= start && *t < end)
        .collect();
    times.sort_unstable();
    times
}

// ---------------------------------------------------------------------------
// fixture parquet
// ---------------------------------------------------------------------------

/// Write the fixture corpus under the ADR-0009 layout,
/// `data/{env}/{date}/{HH}/{service}.parquet`.
///
/// One file per partition hour, which is what the query planner globs: a
/// flat root is only reachable through a whole-root `**` the planner
/// deliberately never emits.
fn write_fixture(data_dir: &Path) {
    let conn = duckdb::Connection::open_in_memory().expect("open duckdb");
    // The session zone every conform and every bound cast reads under.
    conn.execute_batch("SET TimeZone='UTC'").expect("set zone");
    conn.execute_batch(
        "CREATE TABLE logs (
            _time TIMESTAMP,
            _ingested TIMESTAMP,
            _raw VARCHAR,
            env VARCHAR,
            service VARCHAR,
            host VARCHAR,
            message VARCHAR
        )",
    )
    .expect("create fixture table");

    for offset in fixture_offsets() {
        let text = format_window_bound(at(offset));
        conn.execute_batch(&format!(
            "INSERT INTO logs VALUES (
                 CAST('{text}' AS TIMESTAMP), CAST('{text}' AS TIMESTAMP),
                 'raw {text}', '{ENV}', '{SERVICE}', 'host-1', 'event at {text}')"
        ))
        .expect("insert fixture row");
    }

    for hour in 0..24_u32 {
        let dir = data_dir.join(ENV).join(DATE).join(format!("{hour:02}"));
        std::fs::create_dir_all(&dir).expect("create partition dir");
        let path = dir.join(format!("{SERVICE}.parquet"));
        let written: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM logs WHERE date_part('hour', _time) = {hour}"),
                [],
                |r| r.get(0),
            )
            .expect("count partition rows");
        if written == 0 {
            std::fs::remove_dir(&dir).expect("drop an empty partition dir");
            continue;
        }
        conn.execute_batch(&format!(
            "COPY (SELECT * FROM logs WHERE date_part('hour', _time) = {hour})
             TO '{}' (FORMAT PARQUET)",
            path.display()
        ))
        .expect("write fixture parquet");
    }
}

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

/// Everything one scheduler test drives: both databases, a live key, a
/// `DuckDB` pool over a private data root, and the scheduler config the tick
/// reads `max_catchup_intervals` from.
struct Harness {
    _dir: tempfile::TempDir,
    data_dir: PathBuf,
    pool: ExecutorPool,
    key_store: KeyStore,
    key_id: i64,
    saved: SavedQueryStore,
    schedules: ScheduleStore,
    /// A second pool onto the same app-state database, for the one test
    /// that has to write a saved query the write-time gate refuses.
    app_pool: PgPool,
    config: SchedulerConfig,
    _fleet_pool: PgPool,
    // Holds the app-state advisory lock and the pool the stores were built
    // from; dropping it would close them mid-test.
    _storage: StorageState,
}

async fn harness() -> Harness {
    let fleet_db_url = common::create_fleet_database().await;
    let app_db_url = common::create_app_database().await;

    let fleet_pool = common::fleet_pool(&fleet_db_url).await;
    let key_store = common::fleet_keystore(&fleet_pool).await;
    // trawl-analyst resolves both `query` and `saved_query`, the two
    // permissions `owning_key_is_usable` demands of a schedule's owner.
    let key = key_store
        .create_key(
            "scheduler-key",
            PrincipalKind::Service,
            &common::roles(&["trawl-analyst"]),
            None,
        )
        .await
        .expect("mint the schedule-owning key");

    let storage = StorageState::from_pool(common::app_pool(&app_db_url).await)
        .await
        .expect("boot the app-state database");

    let dir = tempfile::tempdir().expect("tempdir");
    let data_dir = dir.path().join("data");
    write_fixture(&data_dir);

    Harness {
        pool: ExecutorPool::new(
            data_dir.to_str().expect("utf-8 data dir").to_owned(),
            2,
            100_000,
            None,
        ),
        data_dir,
        _dir: dir,
        key_id: key.info.id,
        key_store,
        saved: storage.saved.clone(),
        schedules: storage.schedule.clone(),
        app_pool: common::app_pool(&app_db_url).await,
        config: SchedulerConfig {
            max_catchup_intervals: 24,
            ..SchedulerConfig::default()
        },
        _fleet_pool: fleet_pool,
        _storage: storage,
    }
}

impl Harness {
    /// Create a saved query and its schedule, returning the saved-query id.
    async fn schedule(
        &self,
        name: &str,
        dsl: &str,
        window: Option<ScheduleWindow>,
        interval_secs: u64,
        lag_secs: u64,
    ) -> i64 {
        let saved = self
            .saved
            .create(self.key_id, name, dsl)
            .await
            .expect("create saved query");
        self.schedules
            .create_schedule(
                saved.id,
                self.key_id,
                interval_secs,
                None,
                window,
                lag_secs,
                t0(),
            )
            .await
            .expect("create schedule");
        saved.id
    }

    /// One scheduler tick at `now`, awaiting the executions it spawned.
    ///
    /// The daemon drops these handles; a test that did the same would race
    /// its own assertions against a run still writing its parquet.
    async fn tick(&self, now: DateTime<Utc>) {
        for handle in poll_and_execute(
            &self.schedules,
            &self.key_store,
            &self.pool,
            &self.config,
            TIMEOUT_SECS,
            truncate_to_micros(now),
        )
        .await
        {
            handle.await.expect("a scheduled execution must not panic");
        }
    }

    /// Every run of a saved query, oldest first.
    async fn runs(&self, saved_query_id: i64) -> Vec<ReportRun> {
        let mut runs = self
            .schedules
            .list_runs(saved_query_id, self.key_id, 100, 0)
            .await
            .expect("list runs");
        runs.reverse();
        runs
    }

    /// The schedule's cursor and watermark.
    async fn cursor(&self, saved_query_id: i64) -> (DateTime<Utc>, Option<DateTime<Utc>>) {
        let schedule = self
            .schedules
            .get_schedule_for_saved_query(saved_query_id, self.key_id)
            .await
            .expect("read schedule")
            .expect("schedule exists");
        (schedule.next_fire_at, schedule.covered_through)
    }

    /// The `_time` values a run actually stored, read back out of its own
    /// parquet through the engine that wrote it.
    fn run_rows(&self, run: &ReportRun) -> Vec<DateTime<Utc>> {
        let relative = run
            .result_path
            .as_deref()
            .expect("a run with rows writes a parquet result");
        let executor = trawl_engine::executor::Executor::new().expect("engine");
        let result = executor
            .read_parquet_to_result(&self.data_dir.join(relative), 10_000)
            .expect("read the run's parquet");
        let mut times: Vec<DateTime<Utc>> =
            result.rows.iter().map(|row| cell_time(&row[0])).collect();
        times.sort_unstable();
        times
    }
}

/// Read a result cell back as the instant it renders.
///
/// The engine renders a TIMESTAMP as `YYYY-MM-DD HH:MM:SS[.ffffff]` with
/// trailing zeros trimmed, so both spellings have to parse.
fn cell_time(cell: &Value) -> DateTime<Utc> {
    let Value::String(text) = cell else {
        panic!("expected a rendered timestamp, got {cell:?}");
    };
    chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f")
        .unwrap_or_else(|e| panic!("unparseable result timestamp {text:?}: {e}"))
        .and_utc()
}

/// The DSL a windowed run of [`DSL`] must have executed and stored.
fn resolved(start: DateTime<Utc>, end: DateTime<Utc>) -> String {
    format!(
        "earliest=\"{}\" latest=\"{}\" {DSL}",
        format_window_bound(start),
        format_window_bound(end)
    )
}

/// The window a run recorded, as the tuple the assertions compare.
fn window_of(run: &ReportRun) -> (Option<DateTime<Utc>>, Option<DateTime<Utc>>, Option<bool>) {
    (run.window_start, run.window_end, run.window_truncated)
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

/// Consecutive `since_last` runs tile: one run's end is the next one's
/// start, the bounds sit on the planned fires rather than on the tick that
/// noticed them, and the event at exactly T0 lands in exactly one run
/// (ADR-0018 rulings 6, 10, 14).
#[tokio::test]
async fn since_last_fires_tile_and_a_boundary_event_lands_once() {
    let h = harness().await;
    let sq = h
        .schedule("tiling", DSL, Some(ScheduleWindow::SinceLast), 3600, 0)
        .await;

    // Deliberately late, and by a different amount each time: the fire is
    // planned from the cursor, so lateness must not reach the bounds.
    h.tick(at(Duration::seconds(3))).await;
    h.tick(at(hours(1) + Duration::seconds(7))).await;
    h.tick(at(hours(2) + Duration::seconds(9))).await;

    let runs = h.runs(sq).await;
    assert_eq!(runs.len(), 3, "one run per boundary");

    let tiles = [
        (at(hours(-1)), t0()),
        (t0(), at(hours(1))),
        (at(hours(1)), at(hours(2))),
    ];
    for (run, (start, end)) in runs.iter().zip(tiles) {
        assert_eq!(run.status, RunStatus::Success);
        assert_eq!(
            window_of(run),
            (Some(start), Some(end), Some(false)),
            "windows tile on the planned fires, not on the ticks"
        );
        assert_eq!(run.window_kind, Some(WindowKind::SinceLast));
        assert_eq!(
            run.query,
            resolved(start, end),
            "the run stores the text it executed"
        );
    }

    assert_eq!(
        h.cursor(sq).await,
        (at(hours(3)), Some(at(hours(2)))),
        "the cursor is one interval past the last fire; the watermark is its end"
    );

    let rows: Vec<Vec<DateTime<Utc>>> = runs.iter().map(|run| h.run_rows(run)).collect();
    for (rows, (start, end)) in rows.iter().zip(tiles) {
        assert_eq!(*rows, expected_rows(start, end), "rows of [{start}, {end})");
    }
    assert!(
        !rows[0].contains(&t0()) && rows[1].contains(&t0()),
        "the event at exactly T0 belongs to the window that STARTS there"
    );
    let covered: Vec<DateTime<Utc>> = rows.concat();
    let mut deduped = covered.clone();
    deduped.sort_unstable();
    deduped.dedup();
    assert_eq!(
        covered.len(),
        deduped.len(),
        "the tiles are pairwise disjoint"
    );
    assert_eq!(
        deduped,
        expected_rows(at(hours(-1)), at(hours(2))),
        "and together they cover the whole span exactly once"
    );
}

/// A failed run leaves the watermark where it was, so the next success
/// covers its own interval AND the failed one's (ruling 9). Editing the
/// saved DSL in between does not reset the watermark (ruling 14).
#[tokio::test]
async fn a_failed_run_holds_the_watermark_and_the_next_success_covers_both_intervals() {
    let h = harness().await;
    let sq = h
        .schedule("healing", DSL, Some(ScheduleWindow::SinceLast), 3600, 0)
        .await;

    h.tick(at(Duration::seconds(3))).await;
    assert_eq!(
        h.cursor(sq).await.1,
        Some(t0()),
        "the first run covered up to T0"
    );

    // Parses and materializes, then the emitter refuses it: `count` is both
    // the group key and the aggregate's output name.
    h.saved
        .update_checked(sq, h.key_id, "service=fx | stats count() by count", None)
        .await
        .expect("edit the saved DSL");
    h.tick(at(hours(1) + Duration::seconds(7))).await;

    let runs = h.runs(sq).await;
    assert_eq!(runs.len(), 2);
    assert_eq!(
        runs[1].status,
        RunStatus::Error,
        "the edited DSL fails to emit"
    );
    assert_eq!(
        h.cursor(sq).await,
        (at(hours(2)), Some(t0())),
        "the cursor moved past the failed boundary; the watermark did not"
    );

    h.saved
        .update_checked(sq, h.key_id, DSL, None)
        .await
        .expect("restore the saved DSL");
    h.tick(at(hours(2) + Duration::seconds(9))).await;

    let runs = h.runs(sq).await;
    assert_eq!(
        runs.len(),
        3,
        "the missed interval coalesces, never backfills"
    );
    let healed = &runs[2];
    assert_eq!(healed.status, RunStatus::Success);
    assert_eq!(
        window_of(healed),
        (Some(t0()), Some(at(hours(2))), Some(false)),
        "one window covering both intervals, and complete"
    );
    let rows = h.run_rows(healed);
    assert!(
        rows.contains(&at(mins(30))) && rows.contains(&at(hours(1) + mins(30))),
        "the healed window carries events from both intervals: {rows:?}"
    );
    assert_eq!(rows, expected_rows(t0(), at(hours(2))));
    assert_eq!(h.cursor(sq).await, (at(hours(3)), Some(at(hours(2)))));
}

/// A FIRST run that fails must not cost its interval either.
///
/// The watermark is seeded at creation with the origin the schedule owes
/// coverage from, so a failed first run leaves that origin standing and the
/// next success covers both intervals. Without the seed the watermark would
/// still be unset here, the planner would fall back to ruling 14, and run 2
/// would cover only `[T0, T0+1h)`: the first hour dropped, `truncated`
/// false, nothing anywhere admitting the loss.
#[tokio::test]
async fn first_failed_since_last_run_is_healed_by_next_success() {
    let h = harness().await;
    let sq = h
        .schedule("first-fail", DSL, Some(ScheduleWindow::SinceLast), 3600, 0)
        .await;
    assert_eq!(
        h.cursor(sq).await,
        (t0(), Some(at(hours(-1)))),
        "a since_last schedule owes coverage from its first window's start"
    );

    // Parses and materializes, then the emitter refuses it: `count` is both
    // the group key and the aggregate's output name.
    h.saved
        .update_checked(sq, h.key_id, "service=fx | stats count() by count", None)
        .await
        .expect("break the saved DSL before the first run");
    h.tick(at(Duration::seconds(3))).await;

    let runs = h.runs(sq).await;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, RunStatus::Error, "the first run fails");
    assert_eq!(
        h.cursor(sq).await,
        (at(hours(1)), Some(at(hours(-1)))),
        "the cursor moved past the failed boundary; the origin did not"
    );

    h.saved
        .update_checked(sq, h.key_id, DSL, None)
        .await
        .expect("restore the saved DSL");
    h.tick(at(hours(1) + Duration::seconds(7))).await;

    let runs = h.runs(sq).await;
    assert_eq!(
        runs.len(),
        2,
        "the missed interval coalesces, never backfills"
    );
    let healed = &runs[1];
    assert_eq!(healed.status, RunStatus::Success);
    assert_eq!(
        window_of(healed),
        (Some(at(hours(-1))), Some(at(hours(1))), Some(false)),
        "one complete window over the failed interval and its own"
    );
    let rows = h.run_rows(healed);
    assert!(
        rows.contains(&at(mins(-30))),
        "the interval the failed run owed is covered, not dropped: {rows:?}"
    );
    assert_eq!(rows, expected_rows(at(hours(-1)), at(hours(1))));
    assert_eq!(h.cursor(sq).await, (at(hours(2)), Some(at(hours(1)))));
}

/// A gap wider than `max_catchup_intervals` clamps forward and says so, on
/// the run row and on the counter, rather than handing one run a day and a
/// half of corpus or wedging the schedule (ruling 9).
#[tokio::test]
async fn a_thirty_interval_gap_coalesces_into_one_clamped_run() {
    // Installed before the tick that increments it: a counter recorded with
    // no recorder in place is silently dropped.
    let metrics = common::test_metrics_handle();
    let h = harness().await;
    let sq = h
        .schedule("clamped", DSL, Some(ScheduleWindow::SinceLast), 3600, 0)
        .await;

    h.tick(t0()).await;
    h.tick(at(hours(30))).await;

    let runs = h.runs(sq).await;
    assert_eq!(runs.len(), 2, "thirty missed boundaries make ONE run");
    assert_eq!(
        window_of(&runs[1]),
        (Some(at(hours(6))), Some(at(hours(30))), Some(true)),
        "the start is clamped to 24 intervals back from the fire, and flagged"
    );
    assert_eq!(
        h.cursor(sq).await,
        (at(hours(31)), Some(at(hours(30)))),
        "the schedule resumes on its own cadence, not on the gap"
    );

    let scrape = metrics.render();
    let value = scrape.lines().find_map(|line| {
        line.strip_prefix(trawl_server::metrics::SCHEDULER_WINDOW_TRUNCATED_TOTAL)?
            .strip_prefix(' ')?
            .trim()
            .parse::<f64>()
            .ok()
    });
    assert_eq!(
        value,
        Some(1.0),
        "the clamp is counted exactly once: {scrape}"
    );
}

/// Ruling 13: a window that held no events is still a run. It records its
/// bounds, it carries its (empty) result as a zstd blob because a zero-row
/// parquet has no schema to be written from, and it is the run
/// `latest_successful_run` returns.
///
/// The older run's file is still on disk, which is exactly the trap: before
/// this, `run=latest` filtered on `result_path IS NOT NULL` and answered
/// from that older file, reporting a superseded window as the current one.
#[tokio::test]
async fn a_zero_row_run_records_its_window_and_is_the_latest_success() {
    let h = harness().await;
    let sq = h
        .schedule(
            "quiet",
            DSL,
            Some(ScheduleWindow::Fixed { secs: 300 }),
            3600,
            0,
        )
        .await;

    // [T0-5m, T0) holds the event one microsecond before T0.
    h.tick(t0()).await;
    // [T0+55m, T0+1h) holds nothing: the fixture's neighbours are at +30m
    // and at +1h, and +1h is the exclusive end.
    h.tick(at(hours(1))).await;

    let runs = h.runs(sq).await;
    assert_eq!(runs.len(), 2, "one run per planned fire");
    assert_eq!(h.run_rows(&runs[0]), vec![at(micros(-1))]);

    let quiet = &runs[1];
    assert_eq!(quiet.status, RunStatus::Success, "empty is not failure");
    assert_eq!(quiet.row_count, Some(0));
    assert_eq!(
        window_of(quiet),
        (Some(at(mins(55))), Some(at(hours(1))), Some(false)),
        "the run says what it covered, even having found nothing"
    );
    assert_eq!(quiet.window_kind, Some(WindowKind::Fixed));
    assert!(
        quiet.result_path.is_none(),
        "a zero-row result has no schema to write a parquet from: {quiet:?}"
    );
    assert!(
        h.schedules
            .get_run_result(quiet.id, h.key_id)
            .await
            .expect("read the run's result blob")
            .is_some(),
        "the empty result is persisted as a blob, so `run=latest` has \
         something to resolve"
    );

    let latest = h
        .schedules
        .latest_successful_run(sq)
        .await
        .expect("read the latest successful run")
        .expect("there are two successes");
    assert_eq!(
        latest.id, quiet.id,
        "the newest success is the answer, file or no file"
    );
}

/// A fixed trailing window is re-measured from every planned fire and keeps
/// no watermark, so it never heals a gap: missed fires are simply missed,
/// and `lag` delays coverage rather than widening it (ruling 6).
#[tokio::test]
async fn fixed_windows_trail_each_planned_fire_and_never_catch_up() {
    let h = harness().await;
    let sq = h
        .schedule(
            "trailing",
            DSL,
            Some(ScheduleWindow::Fixed { secs: 7200 }),
            3600,
            300,
        )
        .await;

    h.tick(t0()).await;
    h.tick(at(hours(3) + Duration::seconds(2))).await;

    let runs = h.runs(sq).await;
    assert_eq!(
        runs.len(),
        2,
        "the fires at T0+1h and T0+2h are gone, not queued"
    );
    assert_eq!(
        window_of(&runs[0]),
        (
            Some(at(-mins(5) - hours(2))),
            Some(at(-mins(5))),
            Some(false)
        ),
        "the lag shifts BOTH bounds back"
    );
    assert_eq!(runs[0].window_kind, Some(WindowKind::Fixed));
    assert_eq!(
        window_of(&runs[1]),
        (
            Some(at(hours(3) - mins(5) - hours(2))),
            Some(at(hours(3) - mins(5))),
            Some(false)
        ),
        "the second window trails the planned fire T0+3h, not the tick"
    );
    assert_eq!(
        h.cursor(sq).await,
        (at(hours(4)), None),
        "a fixed schedule never keeps a watermark"
    );
}

/// `report_runs.query` is reproducible by paste (ruling 11): re-parsing it
/// yields the run's own bounds, and re-executing it standalone yields the
/// run's own rows.
#[tokio::test]
async fn a_windowed_run_query_reexecutes_standalone_with_identical_bounds() {
    let h = harness().await;
    let sq = h
        .schedule("audit", DSL, Some(ScheduleWindow::SinceLast), 3600, 0)
        .await;
    h.tick(at(Duration::seconds(3))).await;
    h.tick(at(hours(1) + Duration::seconds(7))).await;

    let run = h.runs(sq).await.remove(1);
    let parsed = trawl_core::parser::parse(&run.query).expect("the stored text parses");
    assert_eq!(
        parsed.search.earliest.map(|b| b.node),
        run.window_start.map(format_window_bound),
        "the pasted query's lower bound IS the run's"
    );
    assert_eq!(
        parsed.search.latest.map(|b| b.node),
        run.window_end.map(format_window_bound)
    );

    let outcome = h
        .pool
        .execute(
            h.pool.allocate_query_id(),
            &run.query,
            Deadline::after(StdDuration::from_secs(TIMEOUT_SECS)),
            false,
            0,
        )
        .await;
    let result = outcome.result.expect("the stored text executes");
    let mut replayed: Vec<DateTime<Utc>> =
        result.rows.iter().map(|row| cell_time(&row[0])).collect();
    replayed.sort_unstable();
    assert_eq!(
        replayed,
        h.run_rows(&run),
        "a paste of the stored query reproduces the stored report"
    );
}

/// Without a window the saved DSL is executed verbatim and the run claims
/// no coverage at all — three NULL bound columns, not a window of zero
/// width (ruling 6). The cadence is still the planned one.
#[tokio::test]
async fn query_mode_schedules_fire_on_planned_boundaries_with_no_window() {
    let h = harness().await;
    let sq = h.schedule("verbatim", DSL, None, 3600, 0).await;

    h.tick(at(Duration::seconds(5))).await;
    h.tick(at(hours(1) + Duration::seconds(40))).await;

    let runs = h.runs(sq).await;
    assert_eq!(runs.len(), 2);
    for run in &runs {
        assert_eq!(run.status, RunStatus::Success);
        assert_eq!(window_of(run), (None, None, None));
        assert_eq!(run.window_kind, None);
        assert_eq!(
            run.query, DSL,
            "query mode executes the saved text verbatim"
        );
    }
    assert_eq!(
        h.cursor(sq).await,
        (at(hours(2)), None),
        "query mode fires on the cursor and keeps no watermark"
    );
}

/// A windowed schedule whose saved DSL grew its own time clause claims
/// nothing and moves nothing, so the tick fails it again on every poll
/// until an operator repairs it (rulings 7 and 11).
///
/// The tick swallows the refusal into a log, so the evidence here is the
/// state it left; that the claim itself answers the typed policy error is
/// `claim_due_run_refuses_a_query_that_owns_its_own_window` in
/// `tests/store_pg.rs`.
#[tokio::test]
async fn a_materialize_failure_is_loud_and_leaves_the_cursor() {
    let h = harness().await;
    let sq = h
        .schedule("conflicted", DSL, Some(ScheduleWindow::SinceLast), 3600, 0)
        .await;
    // Write the forbidden pair the only way that is still possible: past
    // the write-time gate, straight at the column. `update_checked` refuses
    // this text under this schedule, so a tick that still refuses it is
    // proof the claim re-reads stored state rather than trusting the gate.
    sqlx::query("UPDATE saved_queries SET query = $1 WHERE id = $2")
        .bind("service=fx last=1h | table _time")
        .bind(sq)
        .execute(&h.app_pool)
        .await
        .expect("plant the forbidden pair");

    h.tick(at(Duration::seconds(3))).await;
    assert!(h.runs(sq).await.is_empty(), "nothing may be claimed");
    assert_eq!(
        h.cursor(sq).await,
        (t0(), Some(at(hours(-1)))),
        "the cursor stays on the refused boundary, and so does the origin \
         the schedule was created owing coverage from"
    );

    // Still refused on the next poll, and still with nothing written.
    h.tick(at(hours(5))).await;
    assert!(h.runs(sq).await.is_empty());
    assert_eq!(h.cursor(sq).await, (t0(), Some(at(hours(-1)))));
}
