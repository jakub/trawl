// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Scheduler-owned report windows, end to end (ADR-0018 rulings 6-14).
//!
//! Real postgres, real `DuckDB`, real fleet keystore, and an injected clock:
//! every window test drives `poll_and_execute` with explicit instants
//! instead of waiting for them. Coverage is a claim about hours, so a
//! wall-clock test could only prove the parts that fit inside a few
//! seconds — which is none of the interesting ones.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, TimeZone as _, Utc};
use fleet_auth::{KeyStore, PrincipalKind};
use sqlx::PgPool;
use tokio::task::JoinHandle;
use trawl_api::value::Value;
use trawl_server::config::SchedulerConfig;
use trawl_server::deadline::Deadline;
use trawl_server::pool::{ExecutorPool, WorkContext, WorkKind};
use trawl_server::report_window::{
    ScheduleWindow, WindowKind, format_window_bound, truncate_to_micros,
};
use trawl_server::scheduler::poll_and_execute;
use trawl_server::store::{
    CLAIM_WAIT, FinishOutcome, ReportRun, RunClaim, RunOrigin, RunStatus, SavedQueryStore,
    ScheduleStore, StorageState,
};

const DSL: &str = "service=fx | table _time";

const SERVICE: &str = "fx";
const ENV: &str = "lab";
const DATE: &str = "2026-03-14";

const TIMEOUT_SECS: u64 = 30;

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 3, 14, 3, 0, 0).unwrap()
}

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

fn expected_rows(start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<DateTime<Utc>> {
    let mut times: Vec<DateTime<Utc>> = fixture_offsets()
        .into_iter()
        .map(at)
        .filter(|t| *t >= start && *t < end)
        .collect();
    times.sort_unstable();
    times
}

/// One file per partition hour, which is what the query planner globs: a
/// flat root is only reachable through a whole-root `**` the planner
/// deliberately never emits.
fn write_fixture(data_dir: &Path) {
    let conn = duckdb::Connection::open_in_memory().expect("open duckdb");
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

struct Harness {
    _dir: tempfile::TempDir,
    data_dir: PathBuf,
    pool: ExecutorPool,
    key_store: KeyStore,
    key_id: i64,
    saved: SavedQueryStore,
    schedules: ScheduleStore,
    app_pool: PgPool,
    config: SchedulerConfig,
    _fleet_pool: PgPool,
    // Holds the app-state advisory lock and the pool the stores were built
    // from; dropping it would close them mid-test.
    _storage: StorageState,
}

async fn harness() -> Harness {
    harness_with(2).await
}

/// [`harness`] over an executor pool of `permits` permits, which allows
/// half of them, at least 1, scheduled runs in flight.
async fn harness_with(permits: usize) -> Harness {
    let fleet_db_url = common::create_fleet_database().await;
    let app_db_url = common::create_app_database().await;

    let fleet_pool = common::fleet_pool(&fleet_db_url).await;
    let key_store = common::fleet_keystore(&fleet_pool).await;
    // trawl-analyst resolves both `query` and `saved_query`, the two
    // permissions the tick's key check demands of a schedule's owner.
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
        pool: ExecutorPool::for_test(
            data_dir.to_str().expect("utf-8 data dir").to_owned(),
            permits,
            100_000,
            None,
        )
        .unwrap(),
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

    async fn schedule_at(&self, name: &str, fire: DateTime<Utc>) -> i64 {
        let saved = self
            .saved
            .create(self.key_id, name, DSL)
            .await
            .expect("create saved query");
        self.schedules
            .create_schedule(saved.id, self.key_id, 3600, None, None, 0, fire)
            .await
            .expect("create schedule");
        saved.id
    }

    /// One scheduler tick at `now` with a `timeout_secs` budget per run,
    /// handing back the executions it spawned unawaited: a test that
    /// parks their work cannot await them first.
    async fn spawn_tick(&self, timeout_secs: u64, now: DateTime<Utc>) -> Vec<JoinHandle<()>> {
        poll_and_execute(
            &self.schedules,
            &self.key_store,
            &self.pool,
            &self.config,
            timeout_secs,
            truncate_to_micros(now),
        )
        .await
    }

    async fn claim_state(
        &self,
        saved_query_id: i64,
    ) -> (DateTime<Utc>, Option<DateTime<Utc>>, u64) {
        let schedule = self
            .schedules
            .get_schedule_for_saved_query(saved_query_id, self.key_id)
            .await
            .expect("read schedule")
            .expect("schedule exists");
        (
            schedule.next_fire_at,
            schedule.covered_through,
            schedule.runs_claimed,
        )
    }

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

    async fn runs(&self, saved_query_id: i64) -> Vec<ReportRun> {
        let mut runs = self
            .schedules
            .list_runs(saved_query_id, self.key_id, 100, 0)
            .await
            .expect("list runs");
        runs.reverse();
        runs
    }

    async fn cursor(&self, saved_query_id: i64) -> (DateTime<Utc>, Option<DateTime<Utc>>) {
        let schedule = self
            .schedules
            .get_schedule_for_saved_query(saved_query_id, self.key_id)
            .await
            .expect("read schedule")
            .expect("schedule exists");
        (schedule.next_fire_at, schedule.covered_through)
    }

    fn run_rows(&self, run: &ReportRun) -> Vec<DateTime<Utc>> {
        let relative = run
            .result_path
            .as_deref()
            .expect("a successful run writes a parquet result");
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

fn cell_time(cell: &Value) -> DateTime<Utc> {
    let Value::String(text) = cell else {
        panic!("expected a rendered timestamp, got {cell:?}");
    };
    DateTime::parse_from_rfc3339(text)
        .unwrap_or_else(|e| panic!("unparseable result timestamp {text:?}: {e}"))
        .with_timezone(&Utc)
}

fn resolved(start: DateTime<Utc>, end: DateTime<Utc>) -> String {
    format!(
        "earliest=\"{}\" latest=\"{}\" {DSL}",
        format_window_bound(start),
        format_window_bound(end)
    )
}

fn window_of(run: &ReportRun) -> (Option<DateTime<Utc>>, Option<DateTime<Utc>>, Option<bool>) {
    (run.window_start, run.window_end, run.window_truncated)
}

#[tokio::test]
async fn since_last_fires_tile_and_a_boundary_event_lands_once() {
    let h = harness().await;
    let sq = h
        .schedule("tiling", DSL, Some(ScheduleWindow::SinceLast), 3600, 0)
        .await;

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
        assert_eq!(run.origin, Some(RunOrigin::Scheduled), "a tick's run");
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
        quiet.result_path.is_some(),
        "a zero-row result is a typed empty parquet file: {quiet:?}"
    );
    assert!(
        h.run_rows(quiet).is_empty(),
        "the file holds no rows: {quiet:?}"
    );
    assert!(
        h.schedules
            .get_run_result(quiet.id, h.key_id)
            .await
            .expect("read the run's result blob")
            .is_none(),
        "a run with a file keeps no blob"
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

#[tokio::test]
async fn a_scheduled_run_refused_for_capacity_fails_only_that_attempt() {
    let h = harness().await;
    let sq = h.schedule("busy", DSL, None, 3600, 0).await;

    let held = h
        .pool
        .exclusive(StdDuration::from_secs(5))
        .await
        .expect("the idle pool grants exclusivity");

    for handle in poll_and_execute(
        &h.schedules,
        &h.key_store,
        &h.pool,
        &h.config,
        1, // a one-second budget, spent entirely on the queue
        truncate_to_micros(at(Duration::seconds(3))),
    )
    .await
    {
        handle.await.expect("a refused execution must not panic");
    }
    drop(held);

    let runs = h.runs(sq).await;
    assert_eq!(runs.len(), 1, "the attempt is recorded, once");
    assert_eq!(
        runs[0].status,
        RunStatus::Error,
        "capacity is an error outcome, not an execution timeout"
    );
    let message = runs[0]
        .error_message
        .as_deref()
        .expect("a failed run records why");
    assert!(
        message.contains(trawl_server::error::CAPACITY_NOT_STARTED),
        "the fixed capacity sentence survives to the run row: {message}"
    );
    assert_eq!(runs[0].row_count, None);
    assert_eq!(runs[0].result_path, None);

    let schedule = h
        .schedules
        .get_schedule_for_saved_query(sq, h.key_id)
        .await
        .expect("read schedule")
        .expect("the schedule still exists");
    assert!(schedule.enabled, "a busy minute never disables a schedule");
    assert_eq!(schedule.next_fire_at, at(hours(1)));

    h.tick(at(hours(1) + Duration::seconds(3))).await;
    let runs = h.runs(sq).await;
    assert_eq!(runs.len(), 2);
    assert_eq!(
        runs[1].status,
        RunStatus::Success,
        "the next attempt runs normally"
    );
}

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
        parsed.search.earliest.map(|b| b.node.text().to_owned()),
        run.window_start.map(format_window_bound),
        "the pasted query's lower bound IS the run's"
    );
    assert_eq!(
        parsed.search.latest.map(|b| b.node.text().to_owned()),
        run.window_end.map(format_window_bound)
    );

    let outcome = h
        .pool
        .execute(
            h.pool.allocate_query_id(),
            &run.query,
            Deadline::after(StdDuration::from_secs(TIMEOUT_SECS)),
            false,
            WorkContext::system(WorkKind::Query),
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

    h.tick(at(hours(5))).await;
    assert!(h.runs(sq).await.is_empty());
    assert_eq!(h.cursor(sq).await, (t0(), Some(at(hours(-1)))));
}

/// The texts are planted straight in the column, below the write-time gate,
/// because that is how they reach a scheduler after an upgrade: they were
/// saved by a release that accepted them.
///
/// The expression grammar spells equality `==`, so the aggregate texts use
/// a comparison the parser accepts and reach the position check, not a
/// parse error.
#[tokio::test]
async fn stored_semantic_refusal_does_not_stop_scheduler() {
    const UNREADABLE_BOUND: &str = "cannot read this time bound as a timestamp";
    const AGGREGATE_IN_ROW: &str =
        "count() is an aggregation; use it in stats, timechart, eventstats or pivot";

    // Four schedules run in one tick: 8 permits allow 4 scheduled runs.
    let h = harness_with(8).await;
    let bound = h.schedule("stored-bound", DSL, None, 3600, 0).await;
    let row_agg = h.schedule("stored-row-agg", DSL, None, 3600, 0).await;
    let windowed_agg = h
        .schedule(
            "stored-windowed-agg",
            DSL,
            Some(ScheduleWindow::SinceLast),
            3600,
            0,
        )
        .await;
    let valid = h.schedule("valid", DSL, None, 3600, 0).await;

    let refused = [
        (
            bound,
            r#"earliest="yesterday" service=fx | table _time"#,
            UNREADABLE_BOUND,
            None,
        ),
        (
            row_agg,
            "service=fx | where count(x) > 1 | table _time",
            AGGREGATE_IN_ROW,
            None,
        ),
        (
            windowed_agg,
            "service=fx | where count(x) > 1 | table _time",
            AGGREGATE_IN_ROW,
            Some(at(hours(-1))),
        ),
    ];
    for (sq, text, _, watermark) in refused {
        sqlx::query("UPDATE saved_queries SET query = $1 WHERE id = $2")
            .bind(text)
            .bind(sq)
            .execute(&h.app_pool)
            .await
            .expect("plant the stored text");
        assert_eq!(
            h.cursor(sq).await,
            (t0(), watermark),
            "the schedule starts on its first boundary"
        );
    }

    h.tick(at(Duration::seconds(3))).await;

    for (sq, text, sentence, watermark) in refused {
        let runs = h.runs(sq).await;
        assert_eq!(runs.len(), 1, "{text}: one attempt, recorded once");
        assert_eq!(runs[0].status, RunStatus::Error, "{text}: {:?}", runs[0]);
        let message = runs[0]
            .error_message
            .as_deref()
            .unwrap_or_else(|| panic!("{text}: a failed run records why"));
        assert!(
            message.contains(sentence),
            "{text}: the refusal sentence reaches the run row: {message}"
        );
        assert_eq!(runs[0].row_count, None, "{text}");
        assert_eq!(runs[0].result_path, None, "{text}");

        let schedule = h
            .schedules
            .get_schedule_for_saved_query(sq, h.key_id)
            .await
            .expect("read schedule")
            .unwrap_or_else(|| panic!("{text}: the schedule row survives"));
        assert!(schedule.enabled, "{text}: a refusal never disables");
        assert_eq!(
            (schedule.next_fire_at, schedule.covered_through),
            (at(hours(1)), watermark),
            "{text}: the cursor moves on cadence; the watermark holds"
        );
    }

    let runs = h.runs(valid).await;
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0].status,
        RunStatus::Success,
        "the valid schedule runs in the same tick: {:?}",
        runs[0]
    );
    assert_eq!(h.run_rows(&runs[0]).len(), fixture_offsets().len());

    h.tick(at(hours(1) + Duration::seconds(3))).await;
    for (sq, text, _, watermark) in refused {
        let runs = h.runs(sq).await;
        assert_eq!(runs.len(), 2, "{text}");
        assert!(
            runs.iter().all(|run| run.status == RunStatus::Error),
            "{text}: {runs:?}"
        );
        assert_eq!(h.cursor(sq).await, (at(hours(2)), watermark), "{text}");
    }
    let runs = h.runs(valid).await;
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[1].status, RunStatus::Success);
}

#[tokio::test]
async fn readable_name_rename_preserves_legacy_and_new_run_paths() {
    let h = harness().await;
    let sq = h
        .schedule(
            "legacy",
            DSL,
            Some(ScheduleWindow::Fixed { secs: 300 }),
            3600,
            0,
        )
        .await;
    h.tick(t0()).await;
    let first = h.runs(sq).await.remove(0);
    let legacy = "scheduled/legacy/run_legacy.parquet";
    std::fs::create_dir_all(h.data_dir.join("scheduled/legacy")).unwrap();
    std::fs::rename(
        h.data_dir.join(first.result_path.unwrap()),
        h.data_dir.join(legacy),
    )
    .unwrap();
    sqlx::query("UPDATE report_runs SET result_path = $1 WHERE id = $2")
        .bind(legacy)
        .bind(first.id)
        .execute(&h.app_pool)
        .await
        .unwrap();
    let renamed = h
        .saved
        .update_checked(sq, h.key_id, DSL, Some("  ../雪 \"report\" \\ folder  "))
        .await
        .unwrap();
    assert_eq!(renamed.id, sq);
    assert_eq!(renamed.name, "../雪 \"report\" \\ folder");
    // A two-hour fire covers the fixture event just before +2h.
    h.tick(at(hours(2))).await;
    let runs = h.runs(sq).await;
    assert_eq!(runs[0].result_path.as_deref(), Some(legacy));
    assert_eq!(h.run_rows(&runs[0]), vec![at(micros(-1))]);
    for run in runs.iter().skip(1).filter(|run| run.result_path.is_some()) {
        assert_eq!(
            run.result_path.as_deref(),
            Some(format!("scheduled/run_{}.parquet", run.id).as_str())
        );
        assert!(!h.run_rows(run).is_empty());
    }
    assert!(runs.iter().skip(1).any(|run| run.result_path.is_some()));
}

async fn until<F, Fut>(what: &str, mut ready: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = std::time::Instant::now() + trawl_server::test_timing::HANG_GUARD;
    while !ready().await {
        assert!(
            std::time::Instant::now() < deadline,
            "{what}: not within the hang guard"
        );
        tokio::time::sleep(StdDuration::from_millis(20)).await;
    }
}

async fn runs_of(db: &PgPool, saved_id: i64) -> Vec<(i64, String, bool, Option<String>)> {
    sqlx::query_as(
        "SELECT id, status, finished_at IS NOT NULL, error_message
         FROM report_runs WHERE saved_query_id = $1 ORDER BY id",
    )
    .bind(saved_id)
    .fetch_all(db)
    .await
    .unwrap()
}

/// The target net A fires a day out, so the loop never claims it. The
/// sentinel net B is due, so B getting its run shows that the iteration's
/// claim pass ran, and stopping and joining the loop shows that the pass
/// finished. The manual run takes no scheduled-run lease, so B's lease is
/// free. The default poll interval keeps report-run retention out of the
/// first iteration.
#[tokio::test(flavor = "multi_thread")]
async fn scheduler_start_keeps_a_fresh_manual_claim() {
    let server = common::setup().await;
    let key_id = common::captured::key_id(&server, &server.analyst_token).await;
    let storage = &server.state.storage;
    let now = Utc::now();
    let target = storage
        .saved
        .create(
            key_id,
            "fresh_manual_target",
            "service=nginx | stats count()",
        )
        .await
        .unwrap();
    storage
        .schedule
        .create_schedule(
            target.id,
            key_id,
            3600,
            None,
            None,
            0,
            now + Duration::days(1),
        )
        .await
        .unwrap();
    let sentinel = storage
        .saved
        .create(key_id, "due_sentinel", "service=nginx | stats count()")
        .await
        .unwrap();
    storage
        .schedule
        .create_schedule(sentinel.id, key_id, 3600, None, None, 0, now)
        .await
        .unwrap();
    let db = common::fixture_pool(&server.app_db_url, 1).await;

    let seams = server.state.query.pool.seams();
    let started = seams.hold(trawl_server::pool::seam::Seam::Started);
    let manual = trawl_client::HttpClient::new_insecure(&server.url, &server.analyst_token)
        .unwrap()
        .trigger_run(target.id)
        .await
        .expect("the manual run is accepted");
    until("the manual run's work started", || async {
        started.arrivals() >= 1
    })
    .await;
    assert_eq!(
        runs_of(&db, target.id).await,
        [(manual.id, "running".to_owned(), false, None)]
    );

    let (stop, stopped) = tokio::sync::watch::channel(false);
    let scheduler = trawl_server::scheduler::spawn_scheduler(
        storage.schedule.clone(),
        KeyStore::from_pool(server.fleet_pool.clone()),
        server.state.query.pool.clone(),
        std::sync::Arc::clone(&server.state.results),
        SchedulerConfig::default(),
        common::DEFAULT_TEST_TIMEOUT_SECS,
        stopped,
    );
    until("the loop claimed the due sentinel", || async {
        !runs_of(&db, sentinel.id).await.is_empty()
    })
    .await;
    stop.send(true).unwrap();
    tokio::time::timeout(trawl_server::test_timing::HANG_GUARD, scheduler)
        .await
        .expect("the scheduler loop stops")
        .expect("the scheduler loop did not panic");

    assert_eq!(
        runs_of(&db, target.id).await,
        [(manual.id, "running".to_owned(), false, None)],
        "the loop's first iteration left the fresh claim alone"
    );

    started.release();
    until("the manual run finished", || async {
        runs_of(&db, target.id).await[0].1 != "running"
    })
    .await;
    assert_eq!(
        runs_of(&db, target.id).await,
        [(manual.id, "success".to_owned(), true, None)]
    );
    until("the sentinel's run finished", || async {
        runs_of(&db, sentinel.id)
            .await
            .iter()
            .all(|run| run.1 != "running")
    })
    .await;
    let sentinel_runs = runs_of(&db, sentinel.id).await;
    assert_eq!(sentinel_runs.len(), 1, "{sentinel_runs:?}");
    assert_eq!(sentinel_runs[0].1, "success", "{sentinel_runs:?}");
}

/// The scheduler unlinks exactly the paths retention returns. A finished
/// run's file is among them (the positive control), and the live run's
/// file, already on disk the way the executor leaves it before
/// `finish_run`, is not.
#[tokio::test]
async fn retention_during_a_run_keeps_the_schedule_claimed() {
    let h = harness().await;
    let sq = h.schedule("retention-claim", DSL, None, 3600, 0).await;
    let schedule_id = h
        .schedules
        .get_schedule_for_saved_query(sq, h.key_id)
        .await
        .expect("read schedule")
        .expect("schedule exists")
        .id;

    let claim = || async {
        match h
            .schedules
            .claim_run(schedule_id, sq, DSL, None)
            .await
            .expect("claim")
        {
            RunClaim::Started(id) => id,
            other => panic!("expected a started run, got {other:?}"),
        }
    };
    let result_file = |run_id: i64| {
        let relative = format!("scheduled/run_{run_id}.parquet");
        let file = h.data_dir.join(&relative);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"result").unwrap();
        (relative, file)
    };

    let finished_id = claim().await;
    let (finished_relative, _) = result_file(finished_id);
    h.schedules
        .finish_run(
            finished_id,
            RunStatus::Success,
            10,
            Some(1),
            None,
            None,
            Some(&finished_relative),
        )
        .await
        .expect("finish the earlier run");

    let run_id = claim().await;
    let (relative, file) = result_file(run_id);

    let (deleted, paths) = h
        .schedules
        .delete_old_runs(0, 0)
        .await
        .expect("retention runs");
    assert_eq!(deleted, 1, "only the finished run is deleted");
    assert_eq!(
        paths,
        vec![finished_relative],
        "cleanup gets the finished run's file and never the live one"
    );

    assert_eq!(
        h.schedules
            .claim_run(schedule_id, sq, DSL, None)
            .await
            .expect("second claim"),
        RunClaim::AlreadyRunning,
        "retention left the running row, so the schedule stays claimed"
    );

    let outcome = h
        .schedules
        .finish_run(
            run_id,
            RunStatus::Success,
            10,
            Some(1),
            None,
            None,
            Some(&relative),
        )
        .await
        .expect("finish");
    assert_eq!(outcome, FinishOutcome::Persisted);

    let runs = h.runs(sq).await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0].id, run_id);
    assert_eq!(runs[0].status, RunStatus::Success);
    assert!(file.exists(), "the run's result file is kept");
}

#[tokio::test]
async fn retention_below_the_cap_stops_at_the_cap() {
    let h = harness().await;
    let saved = h
        .saved
        .create(h.key_id, "capped", DSL)
        .await
        .expect("create saved query");
    let sq = saved.id;
    h.schedules
        .create_schedule(sq, h.key_id, 3600, Some(3), None, 0, t0())
        .await
        .expect("create schedule");

    let mut claimed = std::collections::BTreeSet::new();
    for boundary in 0..5 {
        h.tick(at(hours(boundary) + Duration::seconds(3))).await;
        let runs = h.runs(sq).await;
        assert!(
            runs.iter().all(|run| run.status == RunStatus::Success),
            "{runs:?}"
        );
        claimed.extend(runs.iter().map(|run| run.id));
        h.schedules
            .delete_old_runs(365, 1)
            .await
            .expect("retention runs");
        assert_eq!(h.runs(sq).await.len(), 1, "retention keeps one run");
    }

    assert_eq!(
        claimed.len(),
        3,
        "exactly the cap, across retention passes: {claimed:?}"
    );
    let schedule = h
        .schedules
        .get_schedule_for_saved_query(sq, h.key_id)
        .await
        .expect("read schedule")
        .expect("schedule exists");
    assert_eq!(
        schedule.next_fire_at,
        at(hours(3)),
        "the first refused boundary stays claimable"
    );
    assert_eq!(
        (schedule.runs_claimed, schedule.exhausted()),
        (3, true),
        "retention never changes the budget"
    );
}

/// `poll_and_execute` is called directly and its handles kept, because
/// `Harness::tick` awaits them and would deadlock on the hold.
#[tokio::test(flavor = "multi_thread")]
async fn retention_during_a_scheduled_run_keeps_its_result() {
    let h = harness().await;
    let sq = h.schedule("retention-scheduled", DSL, None, 60, 0).await;
    let schedule_id = h
        .schedules
        .get_schedule_for_saved_query(sq, h.key_id)
        .await
        .expect("read schedule")
        .expect("schedule exists")
        .id;

    let seams = h.pool.seams();
    let started = seams.hold(trawl_server::pool::seam::Seam::Started);
    let handles = poll_and_execute(
        &h.schedules,
        &h.key_store,
        &h.pool,
        &h.config,
        TIMEOUT_SECS,
        truncate_to_micros(at(Duration::seconds(1))),
    )
    .await;
    until("the scheduled run's work started", || async {
        started.arrivals() >= 1
    })
    .await;

    h.schedules
        .delete_old_runs(0, 0)
        .await
        .expect("retention runs");
    assert_eq!(
        h.schedules
            .claim_run(schedule_id, sq, DSL, None)
            .await
            .expect("second claim"),
        RunClaim::AlreadyRunning,
        "retention left the running row, so the schedule stays claimed"
    );

    started.release();
    for handle in handles {
        handle.await.expect("a scheduled execution must not panic");
    }

    let runs = h.runs(sq).await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0].status, RunStatus::Success, "{runs:?}");
    let relative = runs[0]
        .result_path
        .as_deref()
        .expect("a successful run writes a parquet result");
    assert!(
        h.data_dir.join(relative).exists(),
        "the run's result file is kept"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tick_starts_at_most_half_the_permits() {
    let h = harness_with(4).await;
    let mut nets = Vec::new();
    for i in 0..6 {
        let sq = h
            .schedule(
                &format!("capped-{i}"),
                DSL,
                Some(ScheduleWindow::SinceLast),
                3600,
                0,
            )
            .await;
        nets.push((sq, h.claim_state(sq).await));
    }

    let seams = h.pool.seams();
    let started = seams.hold(trawl_server::pool::seam::Seam::Started);
    let handles = h.spawn_tick(TIMEOUT_SECS, at(Duration::seconds(3))).await;
    assert_eq!(handles.len(), 2, "a pool of 4 permits allows 2 runs");
    until("both runs' work started", || async {
        started.arrivals() == 2
    })
    .await;
    assert_eq!(h.pool.scheduled_leases_available(), 0);

    // Every cursor is T0, so the listing's tie-break, the id, decides
    // which two the tick reached.
    for (i, (sq, before)) in nets.iter().enumerate() {
        let runs = h.runs(*sq).await;
        if i < 2 {
            assert_eq!(runs.len(), 1, "net {i}: {runs:?}");
            assert_eq!(runs[0].status, RunStatus::Running, "net {i}: {runs:?}");
        } else {
            assert!(runs.is_empty(), "net {i} has a run: {runs:?}");
            assert_eq!(
                h.claim_state(*sq).await,
                *before,
                "net {i} keeps its cursor, watermark and budget"
            );
        }
    }
    assert_eq!(started.arrivals(), 2, "no third run reached its work");

    started.release();
    for handle in handles {
        handle.await.expect("a scheduled execution must not panic");
    }
    for (sq, _) in &nets[..2] {
        assert_eq!(h.runs(*sq).await[0].status, RunStatus::Success);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_timed_out_run_keeps_its_lease_until_its_work_ends() {
    let h = harness_with(2).await;
    let newer = h.schedule_at("deferred-newer", at(hours(-1))).await;
    let first = h.schedule_at("oldest", at(hours(-3))).await;
    let older = h.schedule_at("deferred-older", at(hours(-2))).await;
    let now = at(Duration::seconds(3));

    let seams = h.pool.seams();
    let started = seams.hold(trawl_server::pool::seam::Seam::Started);
    let handles = h.spawn_tick(1, now).await;
    assert_eq!(handles.len(), 1, "a pool of 2 permits allows 1 run");
    for handle in handles {
        handle.await.expect("a scheduled execution must not panic");
    }
    until("the run's work started", || async {
        started.arrivals() == 1
    })
    .await;
    let runs = h.runs(first).await;
    assert_eq!(runs.len(), 1, "the oldest cursor runs first: {runs:?}");
    assert_eq!(runs[0].status, RunStatus::Timeout, "{runs:?}");
    assert_eq!(
        h.pool.scheduled_leases_available(),
        0,
        "the parked work keeps the run's lease"
    );

    let deferred = [
        (newer, h.claim_state(newer).await),
        (older, h.claim_state(older).await),
    ];
    let handles = h.spawn_tick(TIMEOUT_SECS, now).await;
    assert!(handles.is_empty(), "no lease is free");
    for (sq, before) in deferred {
        assert!(h.runs(sq).await.is_empty());
        assert_eq!(h.claim_state(sq).await, before);
    }

    started.release();
    until("the parked work returns the lease", || async {
        h.pool.scheduled_leases_available() == 1
    })
    .await;
    let handles = h.spawn_tick(TIMEOUT_SECS, now).await;
    assert_eq!(handles.len(), 1);
    for handle in handles {
        handle.await.expect("a scheduled execution must not panic");
    }
    let runs = h.runs(older).await;
    assert_eq!(runs.len(), 1, "the older cursor runs next: {runs:?}");
    assert_eq!(runs[0].status, RunStatus::Success, "{runs:?}");
    assert!(h.runs(newer).await.is_empty(), "the newer cursor waits");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_manual_run_takes_no_lease() {
    let server = common::setup().await;
    let key_id = common::captured::key_id(&server, &server.analyst_token).await;
    let storage = &server.state.storage;
    let saved = storage
        .saved
        .create(key_id, "manual_no_lease", "service=nginx | stats count()")
        .await
        .unwrap();
    storage
        .schedule
        .create_schedule(
            saved.id,
            key_id,
            3600,
            None,
            None,
            0,
            Utc::now() + Duration::days(1),
        )
        .await
        .unwrap();
    let db = common::fixture_pool(&server.app_db_url, 1).await;

    let pool = &server.state.query.pool;
    let leases: Vec<_> = std::iter::from_fn(|| pool.try_lease_scheduled()).collect();
    assert!(!leases.is_empty());
    assert_eq!(pool.scheduled_leases_available(), 0, "every lease is held");

    let seams = pool.seams();
    let started = seams.hold(trawl_server::pool::seam::Seam::Started);
    let manual = trawl_client::HttpClient::new_insecure(&server.url, &server.analyst_token)
        .unwrap()
        .trigger_run(saved.id)
        .await
        .expect("the manual run is accepted");
    until("the manual run's work started", || async {
        started.arrivals() == 1
    })
    .await;
    assert_eq!(
        runs_of(&db, saved.id).await,
        [(manual.id, "running".to_owned(), false, None)]
    );

    started.release();
    until("the manual run finished", || async {
        runs_of(&db, saved.id).await[0].1 != "running"
    })
    .await;
    assert_eq!(
        runs_of(&db, saved.id).await,
        [(manual.id, "success".to_owned(), true, None)]
    );
    drop(leases);
}

const MANUAL_RUN_BUSY: &str =
    "a manual run is already running; retry after the Retry-After interval";

const NGINX_DSL: &str = "service=nginx | stats count()";

/// Its hourly schedule fires a day out, so no tick claims it.
async fn server_net(
    server: &common::TestServer,
    key_id: i64,
    name: &str,
    window: Option<ScheduleWindow>,
    max_runs: Option<u64>,
) -> (i64, i64) {
    let storage = &server.state.storage;
    let saved = storage
        .saved
        .create(key_id, name, NGINX_DSL)
        .await
        .expect("create the net");
    let schedule = storage
        .schedule
        .create_schedule(
            saved.id,
            key_id,
            3600,
            max_runs,
            window,
            0,
            Utc::now() + Duration::days(1),
        )
        .await
        .expect("schedule the net");
    (saved.id, schedule.id)
}

type ClaimSnapshot = (DateTime<Utc>, Option<DateTime<Utc>>, u64, Vec<i64>);

async fn claim_snapshot(
    server: &common::TestServer,
    db: &PgPool,
    key_id: i64,
    saved_id: i64,
) -> ClaimSnapshot {
    let schedule = server
        .state
        .storage
        .schedule
        .get_schedule_for_saved_query(saved_id, key_id)
        .await
        .expect("read the schedule")
        .expect("the schedule exists");
    let runs = runs_of(db, saved_id).await.iter().map(|r| r.0).collect();
    (
        schedule.next_fire_at,
        schedule.covered_through,
        schedule.runs_claimed,
        runs,
    )
}

fn assert_busy(
    what: &str,
    outcome: Result<trawl_api::ReportRunSummary, trawl_client::ClientError>,
) -> u32 {
    match outcome {
        Err(trawl_client::ClientError::Server {
            status: 429,
            error,
            retry_after_secs,
            ..
        }) => {
            assert_eq!(error.code, trawl_api::ErrorCode::ManualRunBusy, "{what}");
            assert_eq!(error.message, MANUAL_RUN_BUSY, "{what}");
            let retry = retry_after_secs
                .unwrap_or_else(|| panic!("{what}: a busy refusal carries a numeric Retry-After"));
            assert!(retry >= 1, "{what}: Retry-After {retry}");
            retry
        }
        other => panic!("{what}: expected 429 manual_run_busy, got {other:?}"),
    }
}

fn refused(
    what: &str,
    outcome: Result<trawl_api::ReportRunSummary, trawl_client::ClientError>,
) -> (u16, String) {
    match outcome {
        Err(trawl_client::ClientError::Server { status, error, .. }) => {
            assert_ne!(status, 429, "{what}: {}", error.message);
            (status, error.message)
        }
        other => panic!("{what}: expected a refusal, got {other:?}"),
    }
}

async fn gate_drained(server: &common::TestServer) {
    until("the manual-run gate drained", || async {
        server.state.query.pool.manual_leases_in_use() == 0
    })
    .await;
}

async fn settled_runs(
    server: &common::TestServer,
    db: &PgPool,
    saved_id: i64,
) -> Vec<(i64, String, bool, Option<String>)> {
    until("the net's runs finished", || async {
        runs_of(db, saved_id)
            .await
            .iter()
            .all(|run| run.1 != "running")
    })
    .await;
    gate_drained(server).await;
    runs_of(db, saved_id).await
}

async fn lock_waiters(db: &PgPool) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_stat_activity
         WHERE datname = current_database() AND wait_event_type = 'Lock'",
    )
    .fetch_one(db)
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn manual_run_refusal_is_preclaim() {
    let server = common::setup().await;
    let key_id = common::captured::key_id(&server, &server.analyst_token).await;
    let db = common::fixture_pool(&server.app_db_url, 1).await;
    let (first, _) = server_net(&server, key_id, "preclaim_first", None, Some(5)).await;
    let (second, second_schedule) = server_net(
        &server,
        key_id,
        "preclaim_second",
        Some(ScheduleWindow::SinceLast),
        Some(5),
    )
    .await;
    // A watermark with a value, so "unchanged" can tell it from none.
    sqlx::query("UPDATE schedules SET covered_through = $1 WHERE id = $2")
        .bind(truncate_to_micros(Utc::now() - hours(2)))
        .bind(second_schedule)
        .execute(&db)
        .await
        .unwrap();

    let pool = &server.state.query.pool;
    let client =
        trawl_client::HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let seams = pool.seams();
    let started = seams.hold(trawl_server::pool::seam::Seam::Started);
    let running = client
        .trigger_run(first)
        .await
        .expect("the first manual run is accepted");
    until("the first run's work started", || async {
        started.arrivals() == 1
    })
    .await;
    assert_eq!(pool.manual_leases_in_use(), 1);

    let first_before = claim_snapshot(&server, &db, key_id, first).await;
    let second_before = claim_snapshot(&server, &db, key_id, second).await;
    assert_eq!(first_before.2, 1, "the first run spent one budget unit");
    assert_eq!(first_before.3, [running.id]);
    assert!(second_before.1.is_some());
    assert!(second_before.3.is_empty());

    let retry = assert_busy("another net", client.trigger_run(second).await);
    let limit = CLAIM_WAIT.as_secs() + common::DEFAULT_TEST_TIMEOUT_SECS;
    assert!(
        u64::from(retry) <= limit,
        "Retry-After {retry} is past the holder's estimated end"
    );
    assert_busy("the running net", client.trigger_run(first).await);

    assert_eq!(
        claim_snapshot(&server, &db, key_id, second).await,
        second_before,
        "the refusal claimed nothing on the other net"
    );
    assert_eq!(
        claim_snapshot(&server, &db, key_id, first).await,
        first_before,
        "the refusal claimed nothing on the running net"
    );
    assert_eq!(started.arrivals(), 1, "no second run reached its work");

    started.release();
    assert_eq!(
        settled_runs(&server, &db, first).await,
        [(running.id, "success".to_owned(), true, None)]
    );
    assert!(runs_of(&db, second).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn manual_runs_installation_wide_are_capped() {
    let dir = tempfile::tempdir().unwrap();
    let server = common::setup_with_query_knobs(
        dir.path(),
        common::seed_data_root(dir.path()),
        common::DEFAULT_TEST_TIMEOUT_SECS,
        common::QueryKnobs {
            max_concurrent_queries: 4,
            ..common::QueryKnobs::DEFAULT
        },
    )
    .await;
    let pool = &server.state.query.pool;
    assert_eq!(pool.manual_lease_limit(), 1, "max(1, 4/4)");
    let db = common::fixture_pool(&server.app_db_url, 1).await;

    let holder_id = common::captured::key_id(&server, &server.analyst_token).await;
    let other = common::mint_key(
        &server.fleet_pool,
        "second-runner",
        &["query", "saved_query"],
    )
    .await;
    let other_id = common::captured::key_id(&server, &other.token).await;
    let (held, _) = server_net(&server, holder_id, "capped_holder", None, None).await;
    let (refused_net, _) = server_net(&server, other_id, "capped_other", None, None).await;
    let holder =
        trawl_client::HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let second = trawl_client::HttpClient::new_insecure(&server.url, &other.token).unwrap();

    let seams = pool.seams();
    let started = seams.hold(trawl_server::pool::seam::Seam::Started);
    let running = holder
        .trigger_run(held)
        .await
        .expect("the holder's run is accepted");
    until("the holder's work started", || async {
        started.arrivals() == 1
    })
    .await;

    let before = claim_snapshot(&server, &db, other_id, refused_net).await;
    assert_busy("a second key", second.trigger_run(refused_net).await);
    assert_eq!(
        claim_snapshot(&server, &db, other_id, refused_net).await,
        before,
        "the refused key claimed nothing"
    );
    assert_eq!(pool.manual_leases_in_use(), 1);

    started.release();
    assert_eq!(
        settled_runs(&server, &db, held).await,
        [(running.id, "success".to_owned(), true, None)]
    );
    let admitted = second
        .trigger_run(refused_net)
        .await
        .expect("with the place free the second key runs");
    assert_eq!(
        settled_runs(&server, &db, refused_net).await,
        [(admitted.id, "success".to_owned(), true, None)]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_run_gates_do_not_block_each_other() {
    let server = common::setup().await;
    let key_id = common::captured::key_id(&server, &server.analyst_token).await;
    let storage = &server.state.storage;
    let db = common::fixture_pool(&server.app_db_url, 1).await;
    let (manual, _) = server_net(&server, key_id, "gates_manual", None, None).await;
    let due = storage
        .saved
        .create(key_id, "gates_due", NGINX_DSL)
        .await
        .unwrap();
    storage
        .schedule
        .create_schedule(due.id, key_id, 3600, None, None, 0, Utc::now() - hours(1))
        .await
        .unwrap();

    let pool = &server.state.query.pool;
    let leases: Vec<_> = std::iter::from_fn(|| pool.try_lease_scheduled()).collect();
    assert_eq!(pool.scheduled_leases_available(), 0, "every lease is held");

    let seams = pool.seams();
    let started = seams.hold(trawl_server::pool::seam::Seam::Started);
    let run = trawl_client::HttpClient::new_insecure(&server.url, &server.analyst_token)
        .unwrap()
        .trigger_run(manual)
        .await
        .expect("a full scheduled gate does not refuse Run now");
    until("the manual run's work started", || async {
        started.arrivals() == 1
    })
    .await;
    assert_eq!(pool.manual_leases_in_use(), pool.manual_lease_limit());

    drop(leases);
    let handles = poll_and_execute(
        &storage.schedule,
        &KeyStore::from_pool(server.fleet_pool.clone()),
        pool,
        &SchedulerConfig::default(),
        common::DEFAULT_TEST_TIMEOUT_SECS,
        truncate_to_micros(Utc::now()),
    )
    .await;
    assert_eq!(
        handles.len(),
        1,
        "a full manual gate does not defer the tick"
    );
    until("the scheduled run's work started", || async {
        started.arrivals() == 2
    })
    .await;
    let scheduled = runs_of(&db, due.id).await;
    assert_eq!(scheduled.len(), 1, "{scheduled:?}");
    assert_eq!(scheduled[0].1, "running", "{scheduled:?}");

    started.release();
    for handle in handles {
        handle.await.expect("a scheduled execution must not panic");
    }
    assert_eq!(runs_of(&db, due.id).await[0].1, "success");
    assert_eq!(
        settled_runs(&server, &db, manual).await,
        [(run.id, "success".to_owned(), true, None)]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_request_after_claim_still_runs() {
    use tower::ServiceExt as _;

    let server = common::setup().await;
    let key_id = common::captured::key_id(&server, &server.analyst_token).await;
    let (saved, _) = server_net(&server, key_id, "dropped_request", None, None).await;
    let db = common::fixture_pool(&server.app_db_url, 2).await;

    let mut lock = db.begin().await.unwrap();
    sqlx::query("SELECT id FROM saved_queries WHERE id = $1 FOR UPDATE")
        .bind(saved)
        .fetch_one(&mut *lock)
        .await
        .unwrap();

    let mut request = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/api/v1/saved/{saved}/run"))
        .header(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {}", server.analyst_token),
        )
        .body(axum::body::Body::empty())
        .unwrap();
    request
        .extensions_mut()
        .insert(std::net::SocketAddr::from(([127, 0, 0, 1], 40_444)));
    let app = server.router_with(|_| {});
    let pending = tokio::spawn(async move { app.oneshot(request).await });

    until("the claim waits on the saved query's row lock", || async {
        lock_waiters(&db).await == 1
    })
    .await;
    let pool = &server.state.query.pool;
    assert_eq!(pool.manual_leases_in_use(), 1, "the claim holds the lease");

    pending.abort();
    let dropped = pending
        .await
        .expect_err("the request was dropped before it answered");
    assert!(dropped.is_cancelled(), "{dropped:?}");
    assert!(
        runs_of(&db, saved).await.is_empty(),
        "nothing committed yet"
    );

    lock.rollback().await.unwrap();
    let runs = settled_runs(&server, &db, saved).await;
    assert_eq!(runs.len(), 1, "the committed claim ran: {runs:?}");
    assert_eq!(runs[0].1, "success", "{runs:?}");
    assert!(runs[0].2, "{runs:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn manual_claim_refusals_return_the_lease() {
    let server = common::setup().await;
    let key_id = common::captured::key_id(&server, &server.analyst_token).await;
    let storage = &server.state.storage;
    let pool = &server.state.query.pool;
    let db = common::fixture_pool(&server.app_db_url, 1).await;
    let client =
        trawl_client::HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let (good, _) = server_net(&server, key_id, "refusals_good", None, None).await;

    let unscheduled = storage
        .saved
        .create(key_id, "refusals_unscheduled", NGINX_DSL)
        .await
        .unwrap()
        .id;

    let (busy, busy_schedule) = server_net(&server, key_id, "refusals_running", None, None).await;
    let seeded = storage
        .schedule
        .claim_run(busy_schedule, busy, NGINX_DSL, None)
        .await
        .unwrap();
    assert!(matches!(seeded, RunClaim::Started(_)), "{seeded:?}");

    let (spent, _) = server_net(&server, key_id, "refusals_spent", None, Some(0)).await;

    let (empty, empty_schedule) = server_net(
        &server,
        key_id,
        "refusals_empty",
        Some(ScheduleWindow::SinceLast),
        None,
    )
    .await;
    sqlx::query("UPDATE schedules SET covered_through = $1 WHERE id = $2")
        .bind(truncate_to_micros(Utc::now() + hours(1)))
        .bind(empty_schedule)
        .execute(&db)
        .await
        .unwrap();

    let (conflicted, conflicted_schedule) = server_net(
        &server,
        key_id,
        "refusals_conflicted",
        Some(ScheduleWindow::SinceLast),
        None,
    )
    .await;
    // A windowed net with a window to read, whose stored DSL carries its
    // own time clause, past the write-time gate: the claim cannot resolve
    // its window.
    sqlx::query("UPDATE schedules SET covered_through = $1 WHERE id = $2")
        .bind(truncate_to_micros(Utc::now() - hours(1)))
        .bind(conflicted_schedule)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("UPDATE saved_queries SET query = $1 WHERE id = $2")
        .bind("service=nginx last=1h | stats count()")
        .bind(conflicted)
        .execute(&db)
        .await
        .unwrap();

    for (what, net, status, says) in [
        ("no schedule", unscheduled, 400, "attach a schedule"),
        ("already running", busy, 409, "already in progress"),
        ("spent budget", spent, 409, "max runs reached"),
        ("empty window", empty, 409, "nothing new to read"),
        ("window resolution", conflicted, 400, ""),
    ] {
        let (got, message) = refused(what, client.trigger_run(net).await);
        assert_eq!(got, status, "{what}: {message}");
        assert!(message.contains(says), "{what}: {message}");
        assert_eq!(
            pool.manual_leases_in_use(),
            0,
            "{what}: the lease was back before the refusal answered"
        );

        next_press_runs(what, &server, &db, &client, good).await;
    }

    let failed = claim_error_returns_the_lease(&server, key_id, &db).await;
    next_press_runs("a claim error", &server, &db, &client, good).await;

    assert!(runs_of(&db, unscheduled).await.is_empty());
    assert!(runs_of(&db, spent).await.is_empty());
    assert!(runs_of(&db, empty).await.is_empty());
    assert!(runs_of(&db, conflicted).await.is_empty());
    assert!(runs_of(&db, failed).await.is_empty());
    assert_eq!(runs_of(&db, busy).await.len(), 1, "only the seeded run");
}

async fn next_press_runs(
    what: &str,
    server: &common::TestServer,
    db: &PgPool,
    client: &trawl_client::HttpClient,
    good: i64,
) {
    let next = client
        .trigger_run(good)
        .await
        .unwrap_or_else(|e| panic!("{what}: the key's next run is refused: {e:?}"));
    let runs = settled_runs(server, db, good).await;
    assert!(
        runs.iter()
            .any(|run| run.0 == next.id && run.1 == "success"),
        "{what}: {runs:?}"
    );
}

async fn claim_error_returns_the_lease(
    server: &common::TestServer,
    key_id: i64,
    db: &PgPool,
) -> i64 {
    let (failed, _) = server_net(server, key_id, "refusals_store", None, None).await;
    let locker = common::fixture_pool(&server.app_db_url, 1).await;
    let mut lock = locker.begin().await.unwrap();
    sqlx::query("SELECT id FROM saved_queries WHERE id = $1 FOR UPDATE")
        .bind(failed)
        .execute(&mut *lock)
        .await
        .unwrap();
    let presser =
        trawl_client::HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let pending = tokio::spawn(async move { presser.trigger_run(failed).await });
    until("the claim waits on the saved query's row lock", || async {
        lock_waiters(db).await == 1
    })
    .await;
    let ended: bool = sqlx::query_scalar(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
         WHERE datname = current_database() AND wait_event_type = 'Lock'",
    )
    .fetch_one(db)
    .await
    .unwrap();
    assert!(ended, "the claim's connection ended");
    let (got, message) = refused("a claim error", pending.await.unwrap());
    lock.rollback().await.unwrap();
    assert_eq!(got, 503, "a claim error: {message}");
    assert_eq!(
        server.state.query.pool.manual_leases_in_use(),
        0,
        "a claim error: the lease was back before the refusal answered"
    );
    failed
}

#[tokio::test(flavor = "multi_thread")]
async fn manual_run_lease_outlives_its_request() {
    use trawl_server::pool::seam::Seam;

    let server =
        common::setup_with_query_timeout(trawl_server::config::RateLimitConfig::default(), 1).await;
    let key_id = common::captured::key_id(&server, &server.analyst_token).await;
    let pool = &server.state.query.pool;
    let db = common::fixture_pool(&server.app_db_url, 1).await;
    let client =
        trawl_client::HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    resolution_keeps_the_lease(&server, key_id, &db, &client).await;

    for seam in [Seam::Preparing, Seam::Prepared, Seam::Started] {
        let name = format!("outlives_{seam:?}");
        let (saved, _) = server_net(&server, key_id, &name, None, None).await;
        let (other, _) = server_net(&server, key_id, &format!("{name}_other"), None, None).await;

        let seams = pool.seams();
        let parked = seams.hold(seam);
        let run = client
            .trigger_run(saved)
            .await
            .unwrap_or_else(|e| panic!("{seam:?}: the run is refused: {e:?}"));
        until("the run's work parked", || async { parked.arrivals() >= 1 }).await;
        until("the run timed out", || async {
            runs_of(&db, saved).await[0].1 == "timeout"
        })
        .await;
        assert_busy(
            &format!("{seam:?}: another net"),
            client.trigger_run(other).await,
        );
        assert_eq!(
            pool.manual_leases_in_use(),
            1,
            "{seam:?}: the parked work holds run {}'s lease",
            run.id
        );

        if seam == Seam::Started {
            client
                .delete_saved(saved)
                .await
                .expect("delete the running net");
            let again = client
                .create_saved(&name, NGINX_DSL)
                .await
                .expect("re-create the net");
            client
                .set_schedule(again.id, "1h", None, true, None, None)
                .await
                .expect("schedule the re-created net");
            assert_busy("the re-created net", client.trigger_run(again.id).await);
            assert_eq!(pool.manual_leases_in_use(), 1);
        }

        parked.release();
        drop(seams);
        gate_drained(&server).await;
        assert!(
            runs_of(&db, other).await.is_empty(),
            "{seam:?}: the refused press claimed nothing"
        );
    }

    writing_keeps_the_lease().await;
    finishing_keeps_the_lease().await;
}

async fn writing_keeps_the_lease() {
    use trawl_server::pool::seam::Seam;

    let server = common::setup().await;
    let key_id = common::captured::key_id(&server, &server.analyst_token).await;
    let pool = &server.state.query.pool;
    let db = common::fixture_pool(&server.app_db_url, 1).await;
    let client =
        trawl_client::HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let (saved, _) = server_net(&server, key_id, "outlives_writing", None, None).await;
    let (other, _) = server_net(&server, key_id, "outlives_writing_other", None, None).await;

    let seams = pool.seams();
    let writing = seams.hold(Seam::RunWriting);
    let run = client
        .trigger_run(saved)
        .await
        .expect("the writing run is accepted");
    until("the run reached its result write", || async {
        writing.arrivals() == 1
    })
    .await;
    assert_eq!(
        runs_of(&db, saved).await,
        [(run.id, "running".to_owned(), false, None)]
    );
    assert_busy("writing: another net", client.trigger_run(other).await);
    assert_eq!(pool.manual_leases_in_use(), 1, "the writing frame holds it");

    writing.release();
    drop(seams);
    assert_eq!(
        settled_runs(&server, &db, saved).await,
        [(run.id, "success".to_owned(), true, None)]
    );
    assert!(runs_of(&db, other).await.is_empty());
}

async fn resolution_keeps_the_lease(
    server: &common::TestServer,
    key_id: i64,
    db: &PgPool,
    client: &trawl_client::HttpClient,
) {
    use trawl_server::pool::seam::Seam;

    let pool = &server.state.query.pool;
    let (saved, schedule) = server_net(
        server,
        key_id,
        "outlives_resolving",
        Some(ScheduleWindow::SinceLast),
        None,
    )
    .await;
    let (other, _) = server_net(server, key_id, "outlives_resolving_other", None, None).await;
    // A watermark behind now, so the claim has a window to resolve.
    sqlx::query("UPDATE schedules SET covered_through = $1 WHERE id = $2")
        .bind(truncate_to_micros(Utc::now() - hours(1)))
        .bind(schedule)
        .execute(db)
        .await
        .unwrap();
    let before = claim_snapshot(server, db, key_id, saved).await;

    let seams = pool.seams();
    let resolving = seams.hold(Seam::Resolving);
    let pressed = std::time::Instant::now();
    let outcome = tokio::time::timeout(3 * CLAIM_WAIT, client.trigger_run(saved))
        .await
        .expect("the claim stops waiting at its deadline");
    let waited = pressed.elapsed();
    match outcome {
        Err(trawl_client::ClientError::Server {
            status: 503, error, ..
        }) => assert_eq!(error.code, trawl_api::ErrorCode::ServiceUnavailable),
        other => panic!("Resolving: expected 503 service_unavailable, got {other:?}"),
    }
    assert!(
        waited >= CLAIM_WAIT,
        "Resolving: the claim waited {waited:?}"
    );
    assert_eq!(resolving.arrivals(), 1);
    assert_eq!(
        pool.manual_leases_in_use(),
        1,
        "Resolving: the parked resolution holds the lease past its claim and its request"
    );
    assert_busy("Resolving: another net", client.trigger_run(other).await);

    resolving.release();
    drop(seams);
    gate_drained(server).await;
    assert_eq!(
        claim_snapshot(server, db, key_id, saved).await,
        before,
        "Resolving: the claim that ran out claimed nothing"
    );
    assert!(runs_of(db, other).await.is_empty());
}

async fn finishing_keeps_the_lease() {
    use trawl_server::pool::seam::Seam;

    let server = common::setup().await;
    let key_id = common::captured::key_id(&server, &server.analyst_token).await;
    let pool = &server.state.query.pool;
    let db = common::fixture_pool(&server.app_db_url, 2).await;
    let client =
        trawl_client::HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let (saved, schedule) = server_net(&server, key_id, "outlives_finishing", None, None).await;
    let (other, _) = server_net(&server, key_id, "outlives_finishing_other", None, None).await;

    // Park the work until the claim, which locks the schedule row too, has
    // committed; only then lock the row the finish needs.
    let seams = pool.seams();
    let started = seams.hold(Seam::Started);
    let run = client
        .trigger_run(saved)
        .await
        .expect("the finishing run is accepted");
    until("the run's work started", || async {
        started.arrivals() == 1
    })
    .await;
    let mut lock = db.begin().await.unwrap();
    sqlx::query("SELECT id FROM schedules WHERE id = $1 FOR UPDATE")
        .bind(schedule)
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    started.release();
    until("the finish waits on the schedule's row lock", || async {
        lock_waiters(&db).await == 1
    })
    .await;
    assert_eq!(
        runs_of(&db, saved).await,
        [(run.id, "running".to_owned(), false, None)]
    );
    assert_busy("finishing: another net", client.trigger_run(other).await);
    assert_eq!(
        pool.manual_leases_in_use(),
        1,
        "the finishing frame holds it"
    );

    lock.rollback().await.unwrap();
    assert_eq!(
        settled_runs(&server, &db, saved).await,
        [(run.id, "success".to_owned(), true, None)]
    );
    assert!(runs_of(&db, other).await.is_empty());
}

/// The tick logs on the test's task, but a server logs from its workers
/// too, so the capture is the global subscriber, and a process has one:
/// under nextest each test is a process of its own, while under `cargo
/// test` this binary's tests share it. Each test reads only the events
/// naming a schedule or a key it created ([`logged`]). Those ids are local
/// to each test's databases, so the filter is exact only in a process of
/// one test, as nextest runs them.
fn tick_log() -> &'static common::audit_capture::Capture {
    static CAPTURE: std::sync::OnceLock<common::audit_capture::Capture> =
        std::sync::OnceLock::new();
    CAPTURE.get_or_init(|| {
        use tracing_subscriber::prelude::*;

        let capture = common::audit_capture::Capture::default();
        tracing::subscriber::set_global_default(
            tracing_subscriber::registry().with(
                capture
                    .clone()
                    .with_filter(tracing_subscriber::EnvFilter::new("trawl_server=info")),
            ),
        )
        .expect("this binary installs its one global subscriber here");
        capture
    })
}

fn logged(event_type: &str, field: &str, value: i64) -> Vec<common::audit_capture::Captured> {
    let event_type = format!("{event_type:?}");
    let value = value.to_string();
    tick_log()
        .events()
        .into_iter()
        .filter(|e| {
            e.fields.get("event_type") == Some(&event_type) && e.fields.get(field) == Some(&value)
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_claim_stuck_on_a_row_lock_ends_at_its_deadline() {
    tick_log();
    let h = harness_with(2).await;
    // Equal cursors: the listing's tie-break, the id, puts `stuck` first.
    let stuck = h
        .schedule("stuck", DSL, Some(ScheduleWindow::SinceLast), 3600, 0)
        .await;
    let next = h
        .schedule("next", DSL, Some(ScheduleWindow::SinceLast), 3600, 0)
        .await;
    let stuck_schedule = h
        .schedules
        .get_schedule_for_saved_query(stuck, h.key_id)
        .await
        .unwrap()
        .unwrap()
        .id;
    let now = at(Duration::seconds(3));
    let before = h.claim_state(stuck).await;
    assert!(before.1.is_some(), "a since_last schedule has a watermark");

    let mut lock = h.app_pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM saved_queries WHERE id = $1 FOR UPDATE")
        .bind(stuck)
        .execute(&mut *lock)
        .await
        .unwrap();
    let ticked = std::time::Instant::now();
    let handles = tokio::time::timeout(3 * CLAIM_WAIT, h.spawn_tick(TIMEOUT_SECS, now))
        .await
        .expect("the stuck claim ends at its deadline");
    let waited = ticked.elapsed();
    lock.rollback().await.unwrap();

    assert!(waited >= CLAIM_WAIT, "the claim waited {waited:?}");
    assert_eq!(
        handles.len(),
        1,
        "the stuck claim gave the one lease back, and the next schedule started under it"
    );
    for handle in handles {
        handle.await.expect("a scheduled execution must not panic");
    }
    let expired = logged("scheduler_error", "schedule_id", stuck_schedule);
    assert_eq!(expired.len(), 1, "{expired:?}");
    assert_eq!(expired[0].field("error_class"), "\"wait_expired\"");
    assert!(h.runs(stuck).await.is_empty());
    assert_eq!(
        h.claim_state(stuck).await,
        before,
        "cursor, watermark and run budget"
    );
    let runs = h.runs(next).await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0].status, RunStatus::Success, "{runs:?}");
    until("every lease is back", || async {
        h.pool.scheduled_leases_available() == 1
    })
    .await;

    let mut lock = h.app_pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE schedules IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .unwrap();
    let ticked = std::time::Instant::now();
    let handles = tokio::time::timeout(3 * CLAIM_WAIT, h.spawn_tick(TIMEOUT_SECS, now))
        .await
        .expect("the listing ends at its deadline");
    let waited = ticked.elapsed();
    lock.rollback().await.unwrap();
    assert!(waited >= CLAIM_WAIT, "the listing waited {waited:?}");
    assert!(handles.is_empty(), "the tick ended at its listing");
    assert!(h.runs(stuck).await.is_empty());
    assert_eq!(h.claim_state(stuck).await, before);

    h.tick(now).await;
    let runs = h.runs(stuck).await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0].status, RunStatus::Success, "{runs:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_manual_claim_stuck_on_a_row_lock_answers_503() {
    let server = common::setup().await;
    let key_id = common::captured::key_id(&server, &server.analyst_token).await;
    let (saved, _) = server_net(&server, key_id, "stuck_manual", None, None).await;
    let db = common::fixture_pool(&server.app_db_url, 2).await;
    let pool = &server.state.query.pool;
    let client =
        trawl_client::HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let before = claim_snapshot(&server, &db, key_id, saved).await;

    let mut lock = db.begin().await.unwrap();
    sqlx::query("SELECT id FROM saved_queries WHERE id = $1 FOR UPDATE")
        .bind(saved)
        .execute(&mut *lock)
        .await
        .unwrap();
    let pressed = std::time::Instant::now();
    let outcome = tokio::time::timeout(3 * CLAIM_WAIT, client.trigger_run(saved))
        .await
        .expect("the stuck claim ends at its deadline");
    let waited = pressed.elapsed();
    let held = pool.manual_leases_in_use();
    lock.rollback().await.unwrap();

    match outcome {
        Err(trawl_client::ClientError::Server {
            status: 503, error, ..
        }) => assert_eq!(error.code, trawl_api::ErrorCode::ServiceUnavailable),
        other => panic!("expected 503 service_unavailable, got {other:?}"),
    }
    assert!(waited >= CLAIM_WAIT, "the claim waited {waited:?}");
    assert_eq!(held, 0, "the lease was back before the 503 answered");
    assert_eq!(
        claim_snapshot(&server, &db, key_id, saved).await,
        before,
        "the claim that ran out claimed nothing"
    );

    let run = client
        .trigger_run(saved)
        .await
        .expect("with the lock gone the key's next press runs");
    assert_eq!(
        settled_runs(&server, &db, saved).await,
        [(run.id, "success".to_owned(), true, None)]
    );
}

async fn key_with_schedules(
    h: &Harness,
    key: &str,
    names: &[&str],
) -> (fleet_auth::CreatedKey, Vec<i64>) {
    let key = h
        .key_store
        .create_key(
            key,
            PrincipalKind::Service,
            &common::roles(&["trawl-analyst"]),
            None,
        )
        .await
        .expect("mint the schedules' owning key");
    let mut saved_ids = Vec::new();
    for name in names {
        let saved = h
            .saved
            .create(key.info.id, name, DSL)
            .await
            .expect("create saved query");
        h.schedules
            .create_schedule(saved.id, key.info.id, 3600, None, None, 0, t0())
            .await
            .expect("create schedule");
        saved_ids.push(saved.id);
    }
    (key, saved_ids)
}

async fn untouched(h: &Harness, saved_id: i64, key_id: i64) -> (DateTime<Utc>, usize) {
    let schedule = h
        .schedules
        .get_schedule_for_saved_query(saved_id, key_id)
        .await
        .expect("read schedule")
        .expect("schedule exists");
    let runs = h
        .schedules
        .list_runs(saved_id, key_id, 100, 0)
        .await
        .expect("list runs");
    (schedule.next_fire_at, runs.len())
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unusable_key_is_checked_once_per_tick() {
    tick_log();
    let h = harness().await;
    let (key, saved_ids) = key_with_schedules(&h, "revoked-owner", &["one", "two", "three"]).await;
    h.key_store
        .revoke_key(&key.info.prefix)
        .await
        .expect("revoke the owning key");
    let seams = h.pool.seams();
    let checked = seams.watch(trawl_server::pool::seam::Seam::KeyChecked);
    let now = at(Duration::seconds(3));

    let handles = h.spawn_tick(TIMEOUT_SECS, now).await;
    assert!(
        handles.is_empty(),
        "a revoked key's schedules start nothing"
    );
    assert_eq!(
        checked.arrivals(),
        1,
        "one lookup for the key's three schedules"
    );
    let skips = logged("scheduler_skip", "key_id", key.info.id);
    assert_eq!(skips.len(), 1, "{skips:?}");
    assert_eq!(skips[0].field("schedules"), "3", "{skips:?}");
    for saved_id in &saved_ids {
        assert_eq!(untouched(&h, *saved_id, key.info.id).await, (t0(), 0));
    }

    let handles = h.spawn_tick(TIMEOUT_SECS, now).await;
    assert!(handles.is_empty());
    assert_eq!(
        checked.arrivals(),
        2,
        "the next tick looks the key up again"
    );
    assert_eq!(logged("scheduler_skip", "key_id", key.info.id).len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_usable_key_is_checked_at_every_claim() {
    tick_log();
    // Two scheduled leases, so a free lease never ends the tick early.
    let h = harness_with(4).await;
    let (key, saved_ids) = key_with_schedules(&h, "revoked-mid-tick", &["first", "second"]).await;
    let (first, second) = (saved_ids[0], saved_ids[1]);
    let seams = h.pool.seams();
    let checked = seams.watch(trawl_server::pool::seam::Seam::KeyChecked);
    let now = at(Duration::seconds(3));

    let mut lock = h.app_pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM saved_queries WHERE id = $1 FOR UPDATE")
        .bind(first)
        .execute(&mut *lock)
        .await
        .unwrap();
    let revoke_mid_claim = async {
        until("the first claim waits on its row lock", || async {
            lock_waiters(&h.app_pool).await == 1
        })
        .await;
        assert_eq!(
            checked.arrivals(),
            1,
            "only the first claim was checked so far"
        );
        h.key_store
            .revoke_key(&key.info.prefix)
            .await
            .expect("revoke the owning key");
        lock.rollback().await.unwrap();
    };
    let (handles, ()) = tokio::join!(h.spawn_tick(TIMEOUT_SECS, now), revoke_mid_claim);

    assert_eq!(handles.len(), 1, "only the first claim started");
    for handle in handles {
        handle.await.expect("a scheduled execution must not panic");
    }
    assert_eq!(checked.arrivals(), 2, "each claim looked the key up");
    let runs = h
        .schedules
        .list_runs(first, key.info.id, 100, 0)
        .await
        .expect("list runs");
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0].status, RunStatus::Success, "{runs:?}");
    assert_eq!(untouched(&h, second, key.info.id).await, (t0(), 0));
    let skips = logged("scheduler_skip", "key_id", key.info.id);
    assert_eq!(skips.len(), 1, "{skips:?}");
    assert_eq!(skips[0].field("schedules"), "1", "{skips:?}");
}
