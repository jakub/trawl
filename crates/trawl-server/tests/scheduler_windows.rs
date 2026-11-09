// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Scheduler-owned report windows, end to end (ADR-0018 rulings 6-14).
//!
//! Real postgres, real `DuckDB`, real fleet keystore, and an injected clock:
//! every window test drives `poll_and_execute` with explicit instants
//! instead of waiting for them. Coverage is a claim about hours, so a
//! wall-clock test could only prove the parts that fit inside a few
//! seconds — which is none of the interesting ones. One test spawns the
//! real loop instead, over the shared server fixture, to prove its start
//! leaves a fresh manual claim alone.
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
use trawl_server::pool::{ExecutorPool, WorkContext, WorkKind};
use trawl_server::report_window::{
    ScheduleWindow, WindowKind, format_window_bound, truncate_to_micros,
};
use trawl_server::scheduler::poll_and_execute;
use trawl_server::store::{
    FinishOutcome, ReportRun, RunClaim, RunOrigin, RunStatus, SavedQueryStore, ScheduleStore,
    StorageState,
};

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
        pool: ExecutorPool::for_test(
            data_dir.to_str().expect("utf-8 data dir").to_owned(),
            2,
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

/// Read a result cell back as the instant it renders: the result wire's
/// RFC 3339 with its offset.
fn cell_time(cell: &Value) -> DateTime<Utc> {
    let Value::String(text) = cell else {
        panic!("expected a rendered timestamp, got {cell:?}");
    };
    DateTime::parse_from_rfc3339(text)
        .unwrap_or_else(|e| panic!("unparseable result timestamp {text:?}: {e}"))
        .with_timezone(&Utc)
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
/// bounds, it carries its (empty) result as a typed empty parquet file, and
/// it is the run `latest_successful_run` returns.
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

/// A scheduled attempt that cannot get a permit fails THAT run and
/// nothing else (ADR-0024).
///
/// The pool is held exclusively, so the attempt spends its whole budget
/// queueing and never starts work. That is a capacity refusal, not an
/// execution timeout: the run row carries the fixed capacity sentence,
/// and the schedule stays enabled on its ordinary cadence, because a
/// server that is busy for a minute is not an operator's mistake to
/// repair.
#[tokio::test]
async fn a_scheduled_run_refused_for_capacity_fails_only_that_attempt() {
    let h = harness().await;
    let sq = h.schedule("busy", DSL, None, 3600, 0).await;

    // Every permit in one hand: no lane can acquire one until it drops.
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

    // Later scheduling is untouched: same enabled schedule, next fire on
    // the ordinary cadence, and the following tick succeeds.
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
            0,
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

/// Renaming affects lookup only; recorded legacy paths stay readable, while
/// new runs use their ID and cannot interpret display-name punctuation as a path.
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

// ---------------------------------------------------------------------------
// the real loop's start
// ---------------------------------------------------------------------------

/// Wait under the hang guard until `ready` holds, polling every 20 ms.
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

/// Every run of `saved_id` as `(id, status, finished_at set, error_message)`.
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

/// Starting the scheduler cannot interrupt a fresh claim: a manual run
/// claimed after boot, whose work is parked at `Seam::Started`, stays
/// `running` through the loop's first complete iteration, and then
/// finishes `success`.
///
/// The real loop runs, over the shared fixture that boots through
/// `boot::prepare_corpus`. The target net A fires a day out, so the loop
/// never claims it. The sentinel net B is created after A and is due, and
/// `list_enabled_schedules` orders by id, so B getting its run shows the
/// iteration's claim pass reached past A. Stopping and joining the loop is
/// the barrier that the iteration finished. The default poll interval
/// keeps report-run retention out of the first iteration.
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

// ---------------------------------------------------------------------------
// retention against a live run (#350, ADR-0018 amendment 2026-11-06)
// ---------------------------------------------------------------------------

/// Retention during a run keeps the schedule claimed, in sequence: a claim,
/// retention at limits that would match the running row, a second claim
/// refused as already running, then a finish that persists over a result
/// file that is still there.
///
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

/// Retention below the cap no longer refills it (ADR-0018 amended
/// 2026-11-06). Under a keep of one, a schedule capped at three runs claims
/// exactly three across the retention passes, then refuses every later
/// boundary. Its cursor stays on the first boundary it refused, so raising
/// the cap would resume there.
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

/// The same transition on the real scheduled path: a due 60 s schedule's
/// run is parked at `Seam::Started`, retention runs at limits that would
/// match it, the schedule stays claimed, and once released the one run
/// finishes `success` with its result file in place.
///
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
