// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The boot steps that make the corpus ready to serve (ADR-0041 slice 2),
//! through `boot::prepare_corpus` over a real `AppState`: rollup-marker
//! recovery, then conformance, then WAL hydration; and, on every node, the
//! marking of report runs a process that ended left `running` (ADR-0018,
//! amended 2026-11-06).
//!
//! Each corpus test plants WAL or markers in its own directories before the
//! fixture boots, observes the state before the boot through the fixture's
//! hook, and then checks what the boot left. Each report-run test plants a
//! run over a first boot and reboots over the same databases. No fixture
//! spawns compaction.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::captured::{self, Restartable, Unserved};
use common::{HotBufferKnobs, TestServer, seed_data_root, setup_observing_boot};
use trawl_server::bus::{EventBus as _, EventSubscriber as _, LocalSubscriber};
use trawl_server::config::{CaptureConfig, RateLimitConfig};
use trawl_server::publication::CorpusUnsettled;
use trawl_server::report_window::ScheduleWindow;
use trawl_server::state::AppState;
use trawl_server::store::ManualRunClaim;

/// Every planted event's service, so counts see no seed row.
const SERVICE: &str = "boothydrated";

/// A WAL file in the live writer's name and line format, planted under
/// `dir/wal/prod`, holding `count` events of [`SERVICE`].
fn plant_wal(dir: &Path, count: usize) -> PathBuf {
    let time = (chrono::Utc::now() - chrono::Duration::minutes(5))
        .to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
    let mut ndjson = Vec::new();
    for seq in 0..count {
        let event = serde_json::json!({
            "_time": time,
            "_ingested": time,
            "env": "prod",
            "service": SERVICE,
            "seq": seq,
            "message": format!("planted {seq}"),
        });
        serde_json::to_writer(&mut ndjson, &event).unwrap();
        ndjson.push(b'\n');
    }
    let writer = trawl_server::ingest::wal::WalWriter::new(dir.join("wal"));
    writer.ensure_dir().unwrap();
    writer.write("prod", SERVICE, &ndjson).unwrap()
}

fn client(server: &TestServer) -> trawl_client::HttpClient {
    trawl_client::HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap()
}

/// `stats count()` over `dsl` through the query API.
async fn count(server: &TestServer, dsl: &str) -> i64 {
    let dsl = format!("{dsl} | stats count()");
    let result = client(server)
        .query_paginated(&dsl, None, None)
        .await
        .unwrap_or_else(|e| panic!("{dsl}: {e}"));
    match &result.result.rows[0][0] {
        trawl_api::value::Value::Integer(n) => *n,
        other => panic!("{dsl}: count must be an integer, got {other:?}"),
    }
}

/// A query answers 503 `corpus_recovering`, with no `Retry-After`.
async fn assert_query_refused(server: &TestServer) {
    let response = common::harness_client_builder()
        .build()
        .unwrap()
        .post(format!("{}/api/v1/query", server.url))
        .bearer_auth(&server.analyst_token)
        .json(&serde_json::json!({ "query": "service=nginx" }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    assert!(
        response.headers().get("retry-after").is_none(),
        "no retry hint for a corpus refusal"
    );
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "corpus_recovering", "{body}");
}

/// `/api/v1/health`: HTTP 200 with `status` and `checks.corpus`.
async fn health(server: &TestServer) -> (String, String) {
    let response = common::harness_client_builder()
        .build()
        .unwrap()
        .get(format!("{}/api/v1/health", server.url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    (
        body["status"].as_str().unwrap().to_owned(),
        body["checks"]["corpus"].as_str().unwrap().to_owned(),
    )
}

/// The gate an ingest node's state reads through.
fn unsettled(state: &AppState) -> Option<CorpusUnsettled> {
    state.query.pool.publication().unsettled()
}

/// Boot hydrates WAL that survived a restart: a query counts every planted
/// event exactly once, before and after the compaction pass that drains
/// it, the gate settles without a coverage proof, and nothing reaches the
/// app's event bus, which was subscribed before the boot. The same
/// subscriber then receives an event ingested over HTTP, so its silence
/// during the boot is no dead subscription.
#[tokio::test(flavor = "multi_thread")]
async fn boot_hydration_counts_planted_wal_once_and_publishes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let data = seed_data_root(dir.path());
    plant_wal(dir.path(), 7);
    plant_wal(dir.path(), 5);

    let mut subscriber: Option<LocalSubscriber> = None;
    let mut born = None;
    let server = setup_observing_boot(dir.path(), data.clone(), None, &mut |state| {
        born = Some(unsettled(state));
        subscriber = Some(state.ingest.event_bus.as_ref().unwrap().subscribe());
    })
    .await;
    assert_eq!(
        born,
        Some(Some(CorpusUnsettled::RestartBacklog)),
        "an ingest node's gate is born starting"
    );
    assert_eq!(unsettled(&server.state), None, "everything fitted");

    let hot = server.state.query.hot_buffer.clone().unwrap();
    assert_eq!(hot.event_count(), 12);
    assert_eq!(count(&server, &format!("service={SERVICE}")).await, 12);
    let mut subscriber = subscriber.unwrap();
    assert!(
        tokio::time::timeout(Duration::ZERO, subscriber.recv())
            .await
            .is_err(),
        "hydration publishes nothing to the event bus"
    );
    assert_eq!(
        server
            .state
            .ingest
            .total_events
            .load(std::sync::atomic::Ordering::Relaxed),
        0,
        "hydration is not ingested traffic"
    );

    // The hydrated batch ids are the ones compaction drains.
    trawl_server::ingest::compaction::compact_once(
        &dir.path().join("wal"),
        Path::new(&data),
        Duration::ZERO,
        false,
        Some(&hot),
        500,
        "2GB",
        None,
    )
    .await
    .unwrap();
    assert_eq!(hot.event_count(), 0, "the pass drained both batches");
    assert_eq!(count(&server, &format!("service={SERVICE}")).await, 12);

    // The positive control: live ingest reaches the same subscriber.
    let response = common::harness_client_builder()
        .build()
        .unwrap()
        .post(format!("{}/api/v1/ingest", server.url))
        .bearer_auth(&server.ingest_token)
        .header("content-type", "application/x-ndjson")
        .body(r#"{"service":"busprobe","message":"live"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let batch = tokio::time::timeout(Duration::from_secs(10), subscriber.recv())
        .await
        .expect("the subscriber receives live ingest")
        .expect("the bus is open and the subscriber has not lagged");
    assert_eq!(&*batch.service, "busprobe");
    assert_eq!(batch.events.len(), 1);
}

/// WAL the caps cannot hold is overhang: reads refuse `restart_backlog`
/// and health degrades at HTTP 200, so probes do not restart the server.
#[tokio::test(flavor = "multi_thread")]
async fn boot_hydration_above_the_caps_leaves_overhang() {
    let dir = tempfile::tempdir().unwrap();
    let data = seed_data_root(dir.path());
    plant_wal(dir.path(), 5);

    let knobs = HotBufferKnobs {
        max_events: 2,
        max_bytes: 1 << 20,
        compaction_interval_secs: 3600,
    };
    let server = setup_observing_boot(dir.path(), data, Some(knobs), &mut |_| {}).await;
    assert_eq!(
        unsettled(&server.state),
        Some(CorpusUnsettled::RestartBacklog)
    );
    assert_eq!(
        server
            .state
            .query
            .hot_buffer
            .as_ref()
            .unwrap()
            .event_count(),
        0
    );
    assert_query_refused(&server).await;
    assert_eq!(
        health(&server).await,
        ("degraded".to_owned(), "restart_backlog".to_owned())
    );
}

/// The seed's nginx hourly file, its day, and a daily file holding the
/// same rows, as an interrupted rollup leaves them.
fn plant_rollup(data: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let day = data.join("prod/2024-01-15");
    let hourly = day.join("10/nginx.parquet");
    std::fs::copy(&hourly, day.join("nginx.parquet")).unwrap();
    let marker = day.join(".rollup-nginx");
    std::fs::write(&marker, hourly.to_string_lossy().as_bytes()).unwrap();
    (day, hourly, marker)
}

/// A pending rollup marker is recovered by the boot, before the listener:
/// the hourly input the daily file already holds is retired, and each row
/// is counted once.
#[tokio::test(flavor = "multi_thread")]
async fn rollup_boot_recovery_recovers_a_pending_marker_before_serving() {
    let dir = tempfile::tempdir().unwrap();
    let data = seed_data_root(dir.path());
    let (day, hourly, marker) = plant_rollup(Path::new(&data));

    let server = setup_observing_boot(dir.path(), data, None, &mut |state| {
        assert!(
            state
                .query
                .pool
                .publication()
                .unsettled_reasons()
                .contains(CorpusUnsettled::RollupPending),
            "the gate's boot scan registered the marker"
        );
    })
    .await;
    assert!(!marker.exists(), "recovered before the fixture served");
    assert!(!hourly.exists(), "the hourly input was retired");
    assert!(day.join("nginx.parquet").is_file());
    assert_eq!(unsettled(&server.state), None);
    assert_eq!(count(&server, "service=nginx").await, 2);
    assert_eq!(health(&server).await, ("ok".to_owned(), "ok".to_owned()));
}

/// A rollup recovery that fails does not stop the boot. The marker stays,
/// reads refuse `rollup_pending`, and health names it.
#[tokio::test(flavor = "multi_thread")]
async fn rollup_boot_recovery_failure_refuses_reads_as_rollup_pending() {
    let dir = tempfile::tempdir().unwrap();
    let data = seed_data_root(dir.path());
    let day = Path::new(&data).join("prod/2024-01-15");
    // An hourly input that can be neither removed nor set aside.
    let stuck = day.join("10/stuck.parquet");
    for path in [stuck.clone(), day.join("10/stuck.parquet.merged")] {
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("child"), b"occupied").unwrap();
    }
    std::fs::copy(day.join("10/nginx.parquet"), day.join("stuck.parquet")).unwrap();
    let marker = day.join(".rollup-stuck");
    std::fs::write(&marker, stuck.to_string_lossy().as_bytes()).unwrap();

    let server = setup_observing_boot(dir.path(), data, None, &mut |_| {}).await;
    assert!(marker.exists(), "the marker waits for a compaction pass");
    assert_eq!(
        unsettled(&server.state),
        Some(CorpusUnsettled::RollupPending)
    );
    assert_query_refused(&server).await;
    assert_eq!(
        health(&server).await,
        ("degraded".to_owned(), "rollup_pending".to_owned())
    );
}

/// A query-only node has no hot buffer and a settled gate: it never
/// hydrates, so nothing holds its reads.
#[tokio::test(flavor = "multi_thread")]
async fn query_only_state_is_born_settled() {
    let dir = tempfile::tempdir().unwrap();
    let server = common::setup_in_dir_with_ingest(dir.path(), false).await;
    assert!(server.state.query.hot_buffer.is_none());
    assert_eq!(unsettled(&server.state), None);
    assert_eq!(count(&server, "service=nginx").await, 2);
}

// ---------------------------------------------------------------------------
// Stale report runs (ADR-0018, amended 2026-11-06): every run a process that
// ended left `running` reads `error` once `boot::prepare_corpus` returns,
// whatever node type or `[scheduler] enabled` the boot runs under. These
// tests reboot over the same databases (`common::captured::Restartable`) so
// a run planted under one boot is stale for the next.
// ---------------------------------------------------------------------------

/// What `cleanup_stale_runs` writes into a run it marks.
const INTERRUPTED: &str = "interrupted by server restart";

/// A saved query owned by the analyst key, with a schedule created through
/// the store door whose fire cursor starts at `fires_at` (query mode when
/// `window` is `None`). Returns the saved query and schedule ids.
async fn net(
    server: &TestServer,
    name: &str,
    window: Option<ScheduleWindow>,
    fires_at: chrono::DateTime<chrono::Utc>,
) -> (i64, i64) {
    let saved = client(server)
        .create_saved(name, "service=nginx | stats count()")
        .await
        .unwrap();
    let key_id = captured::key_id(server, &server.analyst_token).await;
    let schedule = server
        .state
        .storage
        .schedule
        .create_schedule(saved.id, key_id, 3600, None, window, 0, fires_at)
        .await
        .unwrap();
    (saved.id, schedule.id)
}

/// Claim a manual run on `saved_id` through the store and execute nothing:
/// a `running` row no process owns, which is what a process that ended
/// mid-run leaves.
async fn plant_running(server: &TestServer, saved_id: i64) -> i64 {
    let key_id = captured::key_id(server, &server.analyst_token).await;
    match server
        .state
        .storage
        .schedule
        .claim_manual_run(saved_id, key_id, 24)
        .await
        .unwrap()
    {
        ManualRunClaim::Started(claimed) => claimed.run_id,
        other => panic!("the planted claim must start: {other:?}"),
    }
}

/// The app-state database, for what no API reports byte for byte.
async fn app_db(app_db_url: &str) -> sqlx::PgPool {
    common::fixture_pool(app_db_url, 1).await
}

/// A report run's status, error message, and whether `finished_at` is set.
async fn run_state(db: &sqlx::PgPool, run_id: i64) -> (String, Option<String>, bool) {
    sqlx::query_as(
        "SELECT status, error_message, finished_at IS NOT NULL
         FROM report_runs WHERE id = $1",
    )
    .bind(run_id)
    .fetch_one(db)
    .await
    .unwrap()
}

/// A report run as Postgres renders it, every column byte for byte.
async fn run_json(db: &sqlx::PgPool, run_id: i64) -> String {
    sqlx::query_scalar("SELECT row_to_json(r)::text FROM report_runs r WHERE id = $1")
        .bind(run_id)
        .fetch_one(db)
        .await
        .unwrap()
}

/// A schedule as Postgres renders it, every column byte for byte.
async fn schedule_json(db: &sqlx::PgPool, schedule_id: i64) -> String {
    sqlx::query_scalar("SELECT row_to_json(s)::text FROM schedules s WHERE id = $1")
        .bind(schedule_id)
        .fetch_one(db)
        .await
        .unwrap()
}

/// Assert `run_id` reads what the boot's stale-run marking writes.
async fn assert_interrupted(db: &sqlx::PgPool, run_id: i64) {
    assert_eq!(
        run_state(db, run_id).await,
        ("error".to_owned(), Some(INTERRUPTED.to_owned()), true),
        "run {run_id} must be marked interrupted by the boot"
    );
}

/// Wait until `run_id` leaves `running`, under the hang guard, and return
/// the status it finished with.
async fn finished_status(db: &sqlx::PgPool, run_id: i64) -> String {
    let deadline = std::time::Instant::now() + trawl_server::test_timing::HANG_GUARD;
    loop {
        let (status, _, _) = run_state(db, run_id).await;
        if status != "running" {
            return status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "run {run_id} was still running after the hang guard"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Reboot `fixture` over `server` and require the boot to prepare.
async fn prepared(fixture: &Restartable, server: TestServer) -> Unserved {
    match fixture.try_reboot(server).await {
        Ok(unserved) => unserved,
        Err(refused) => panic!("the reboot was refused: {}", refused.error),
    }
}

/// One cell of AC1: plant a stale run over a first boot, reboot as the
/// cell's node, and check the row before the listener has served anything.
/// With the scheduler on, the real loop is spawned between preparation and
/// serve, as trawld's `main` does; the target schedule's fire cursor is a
/// day out, so the loop never claims it. Then a manual run on the same
/// schedule is accepted and finishes `success`.
async fn stale_run_marked_before_serving(ingest_enabled: bool, scheduler_enabled: bool) {
    let (mut fixture, server) =
        Restartable::boot(RateLimitConfig::default(), CaptureConfig::default(), |_| {}).await;
    let tomorrow = chrono::Utc::now() + chrono::Duration::days(1);
    let (saved_id, _) = net(&server, "stale_cell", None, tomorrow).await;
    let stale = plant_running(&server, saved_id).await;

    fixture.ingest_enabled = ingest_enabled;
    fixture.scheduler.enabled = scheduler_enabled;
    let unserved = prepared(&fixture, server).await;
    assert_eq!(
        unserved.state.query.hot_buffer.is_some(),
        ingest_enabled,
        "the reboot is the cell's node type"
    );
    let db = app_db(&unserved.app_db_url).await;
    assert_interrupted(&db, stale).await;

    let scheduler = scheduler_enabled.then(|| {
        let (stop, stopped) = tokio::sync::watch::channel(false);
        let handle = trawl_server::scheduler::spawn_scheduler(
            unserved.state.storage.schedule.clone(),
            unserved.state.auth.key_store.clone(),
            unserved.state.query.pool.clone(),
            std::sync::Arc::clone(&unserved.state.results),
            fixture.scheduler.clone(),
            common::DEFAULT_TEST_TIMEOUT_SECS,
            stopped,
        );
        (stop, handle)
    });
    let server = unserved.serve().await;

    let run = client(&server)
        .trigger_run(saved_id)
        .await
        .expect("the manual run is accepted, not refused as already running");
    assert_ne!(run.id, stale);
    assert_eq!(finished_status(&db, run.id).await, "success");
    assert_interrupted(&db, stale).await;

    if let Some((stop, handle)) = scheduler {
        stop.send(true).unwrap();
        tokio::time::timeout(trawl_server::test_timing::HANG_GUARD, handle)
            .await
            .expect("the scheduler loop stops")
            .expect("the scheduler loop did not panic");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn boot_marks_stale_report_runs_before_serving_on_a_query_only_node_with_the_scheduler_off() {
    stale_run_marked_before_serving(false, false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn boot_marks_stale_report_runs_before_serving_on_an_ingest_node_with_the_scheduler_on() {
    stale_run_marked_before_serving(true, true).await;
}

/// The row count of every UPDATE statement on `report_runs`, in order, as
/// the test-only trigger [`record_report_run_updates`] recorded them.
async fn recorded_updates(db: &sqlx::PgPool) -> Vec<i64> {
    sqlx::query_scalar("SELECT changed FROM report_run_updates ORDER BY seq")
        .fetch_all(db)
        .await
        .unwrap()
}

/// Install a statement-level trigger that records how many rows each UPDATE
/// on `report_runs` changed, zero included: a statement-level trigger fires
/// even when the statement matches no row.
async fn record_report_run_updates(db: &sqlx::PgPool) {
    for statement in [
        "CREATE TABLE report_run_updates (seq bigserial PRIMARY KEY, changed bigint NOT NULL)",
        "CREATE FUNCTION record_report_run_update() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
             INSERT INTO report_run_updates (changed) SELECT count(*) FROM changed;
             RETURN NULL;
         END $$",
        "CREATE TRIGGER record_report_run_update AFTER UPDATE ON report_runs
         REFERENCING NEW TABLE AS changed
         FOR EACH STATEMENT EXECUTE FUNCTION record_report_run_update()",
    ] {
        sqlx::query(statement).execute(db).await.unwrap();
    }
}

/// A second boot over the same database changes no row: the boot's
/// UPDATE changes one row the first time and none the second, the marked
/// run reads byte for byte what the first boot wrote, and a run that had
/// already finished is untouched by both.
#[tokio::test(flavor = "multi_thread")]
async fn stale_report_run_recovery_is_idempotent() {
    let (mut fixture, server) =
        Restartable::boot(RateLimitConfig::default(), CaptureConfig::default(), |_| {}).await;
    let tomorrow = chrono::Utc::now() + chrono::Duration::days(1);
    let (stale_saved, _) = net(&server, "stale_twice", None, tomorrow).await;
    let (done_saved, _) = net(&server, "already_failed", None, tomorrow).await;
    let stale = plant_running(&server, stale_saved).await;
    let done = plant_running(&server, done_saved).await;
    let flipped = server
        .state
        .storage
        .schedule
        .fail_run_if_running(done, 5, "failed before the restart")
        .await
        .unwrap();
    assert_eq!(flipped, trawl_server::store::FlipOutcome::FlippedToError);

    let db = app_db(&server.app_db_url).await;
    let done_before = run_json(&db, done).await;
    record_report_run_updates(&db).await;
    fixture.ingest_enabled = false;
    fixture.scheduler.enabled = false;

    let server = fixture.restart(server).await;
    assert_eq!(
        recorded_updates(&db).await,
        [1],
        "the first boot marks one run"
    );
    assert_interrupted(&db, stale).await;
    let marked = run_json(&db, stale).await;

    let server = fixture.restart(server).await;
    assert_eq!(
        recorded_updates(&db).await,
        [1, 0],
        "the second boot's marking changes no row"
    );
    assert_eq!(run_json(&db, stale).await, marked);
    assert_eq!(run_json(&db, done).await, done_before);
    drop(server);
}

/// Marking a run moves neither the watermark nor the fire cursor (ruling
/// 9), for a `since_last` and a fixed-window schedule. Both cursors are set
/// to real instants first, so a NULL-equals-NULL pass is impossible.
#[tokio::test(flavor = "multi_thread")]
async fn stale_report_run_recovery_moves_no_cursor() {
    let (fixture, server) =
        Restartable::boot(RateLimitConfig::default(), CaptureConfig::default(), |_| {}).await;
    let now = chrono::Utc::now();
    let db = app_db(&server.app_db_url).await;
    let mut planted = Vec::new();
    for (name, window) in [
        ("since_last_net", ScheduleWindow::SinceLast),
        ("fixed_net", ScheduleWindow::Fixed { secs: 7200 }),
    ] {
        let (saved_id, schedule_id) = net(&server, name, Some(window), now).await;
        sqlx::query("UPDATE schedules SET covered_through = $1, next_fire_at = $2 WHERE id = $3")
            .bind(now - chrono::Duration::hours(2))
            .bind(now + chrono::Duration::hours(1))
            .bind(schedule_id)
            .execute(&db)
            .await
            .unwrap();
        let run = plant_running(&server, saved_id).await;
        let (covered, fires): (
            Option<chrono::DateTime<chrono::Utc>>,
            Option<chrono::DateTime<chrono::Utc>>,
        ) = sqlx::query_as("SELECT covered_through, next_fire_at FROM schedules WHERE id = $1")
            .bind(schedule_id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert!(
            covered.is_some() && fires.is_some(),
            "{name}: both cursors set"
        );
        planted.push((
            name,
            schedule_id,
            run,
            schedule_json(&db, schedule_id).await,
        ));
    }

    let unserved = prepared(&fixture, server).await;
    for (name, schedule_id, run, before) in &planted {
        assert_interrupted(&db, *run).await;
        assert_eq!(
            &schedule_json(&db, *schedule_id).await,
            before,
            "{name}: the marking moved a cursor"
        );
    }
    drop(unserved);
}

/// A real database failure in the marking refuses the boot: the error names
/// the step, the boot never serves and its port accepts no connection, and
/// the planted run is still `running` because the UPDATE rolled back.
#[tokio::test(flavor = "multi_thread")]
async fn boot_refuses_when_stale_report_run_recovery_fails() {
    let (fixture, server) =
        Restartable::boot(RateLimitConfig::default(), CaptureConfig::default(), |_| {}).await;
    let tomorrow = chrono::Utc::now() + chrono::Duration::days(1);
    let (saved_id, _) = net(&server, "refused_boot", None, tomorrow).await;
    let stale = plant_running(&server, saved_id).await;
    let db = app_db(&server.app_db_url).await;
    for statement in [
        "CREATE FUNCTION refuse_report_run_update() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN RAISE EXCEPTION 'planted report_runs failure'; END $$",
        "CREATE TRIGGER refuse_report_run_update BEFORE UPDATE ON report_runs
         FOR EACH STATEMENT EXECUTE FUNCTION refuse_report_run_update()",
    ] {
        sqlx::query(statement).execute(&db).await.unwrap();
    }

    let Err(refused) = fixture.try_reboot(server).await else {
        panic!("the boot must refuse when the stale-run marking fails");
    };
    assert!(
        refused.error.contains("stale report-run recovery"),
        "the error names the step: {}",
        refused.error
    );
    assert!(
        std::net::TcpStream::connect(&refused.addr).is_err(),
        "the refused boot's port accepts no connection"
    );
    assert_eq!(
        run_state(&db, stale).await,
        ("running".to_owned(), None, false),
        "the failed marking rolled back"
    );
}
