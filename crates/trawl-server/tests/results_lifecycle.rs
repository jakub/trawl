// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! How a capture stops being readable (ADR-0057), on the fixture's results
//! clock: its deadline, found by a read or by the sweep; a quota lowered
//! across a restart; its file removed or changed out of band; a tombstone
//! that outlives its lifetime. Every refusal carries its request id.
//!
//! The commit of a capture's row is proved here too, through the store's
//! test-support fault seam (`store::results::fault`). An insert that
//! answers an error is settled by its fence, the same insert again: one
//! that committed leaves the fence the taken id, one whose statement never
//! arrived leaves the fence to insert the row, and one whose transaction is
//! still open makes the fence wait for it to end. Each keeps the capture
//! and answers 200. A fence that fails too leaves the commit in doubt
//! through its grace, which the seam holds open: a row that lands late
//! keeps its file, a file whose row never comes goes when the grace ends,
//! and one whose row cannot be read is left to the sweep.
//!
//! The binary is in the ADR-0021 `postgres` nextest group.

mod common;

use std::sync::OnceLock;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use common::TestServer;
use common::audit_capture::Capture;
use common::captured::{self, Restartable, at};
use reqwest::StatusCode;
use serde_json::json;
use sqlx::Connection as _;
use sqlx::postgres::PgConnection;
use trawl_api::results::{CaptureResponse, CapturedResult, ResultState};
use trawl_config::CaptureConfig;
use trawl_server::config::RateLimitConfig;
use trawl_server::pool::seam::Seam;
use trawl_server::results::service::GoneReason;
use trawl_server::results::sweep::{Sweep, sweep};
use trawl_server::store::results::fault::{CommitEnd, InsertFault};

/// Where the clock stands when a test captures.
const CAPTURED_AT: &str = "2024-01-16T10:30:00Z";

/// The tracing events of the whole process. Installed once: the server
/// logs from every tokio worker and blocking thread.
fn telemetry() -> &'static Capture {
    static CAPTURE: OnceLock<Capture> = OnceLock::new();
    CAPTURE.get_or_init(|| {
        use tracing_subscriber::prelude::*;
        let capture = Capture::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone().with_filter(
            tracing_subscriber::EnvFilter::new(trawl_server::telemetry::DEFAULT_LOG_FILTER),
        ));
        tracing::subscriber::set_global_default(subscriber).expect("no prior global subscriber");
        capture
    })
}

async fn boot(capture: CaptureConfig) -> TestServer {
    let server = captured::boot(RateLimitConfig::default(), capture).await;
    server.results_clock.set(at(CAPTURED_AT));
    server
}

/// The corpus's events, captured as the analyst at the clock's reading.
async fn events(server: &TestServer) -> CapturedResult {
    let result = captured::captured(server, &server.analyst_token, &events_body())
        .await
        .result;
    assert_eq!(result.captured_rows, captured::ROWS as u64);
    result
}

fn events_body() -> serde_json::Value {
    json!({
        "query": format!("service={}", captured::SERVICE),
        "window": { "earliest": "2024-01-16T10:00:00Z", "latest": "2024-01-16T10:01:00Z" },
    })
}

/// `answer` is a refusal with `status` and `code`, and names its request.
fn assert_refused(answer: &captured::Answer, status: StatusCode, code: &str, what: &str) {
    let (got, headers, body) = answer;
    assert_eq!(*got, status, "{what}: {body}");
    assert_eq!(body["error"]["code"], code, "{what}: {body}");
    let request_id = headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        !request_id.is_empty(),
        "{what}: the refusal names its request"
    );
}

/// A `result_gone` refusal for `reason`, with its fixed text.
fn assert_gone(answer: &captured::Answer, reason: GoneReason, what: &str) {
    assert_refused(answer, StatusCode::GONE, "result_gone", what);
    assert_eq!(answer.2["error"]["message"], reason.message(), "{what}");
}

fn deadline(result: &CapturedResult) -> DateTime<Utc> {
    at(result.expires_at.as_deref().expect("a temporary capture"))
}

fn file_exists(server: &TestServer, id: &str) -> bool {
    server
        .state
        .results
        .root
        .path_of(id)
        .expect("a result id")
        .exists()
}

/// Every row of the results table, of every key and state.
async fn table_rows(server: &TestServer) -> i64 {
    let pool = common::fixture_pool(&server.app_db_url, 1).await;
    let count = sqlx::query_scalar("SELECT count(*) FROM captured_results")
        .fetch_one(&pool)
        .await
        .unwrap();
    pool.close().await;
    count
}

/// A read at the deadline expires the capture on the spot: 410 with the
/// expired text, the row marked with its deadline as `gone_at`, the file
/// gone, and no sweep ran. The tombstone answers 410 until a minute
/// before its lifetime ends and 404 from a minute after; the sweep then
/// deletes it.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_at_the_deadline_expires_the_capture_and_its_file() {
    let server = boot(CaptureConfig::default()).await;
    let result = events(&server).await;
    let analyst = &server.analyst_token;
    let key_id = captured::key_id(&server, analyst).await;
    let due = deadline(&result);
    assert_eq!(
        due,
        at(CAPTURED_AT) + TimeDelta::seconds(900),
        "the deadline is created_at plus ttl_secs"
    );

    server.results_clock.set(due - TimeDelta::seconds(1));
    let (status, _, page) = captured::page(&server, analyst, &result.id, "").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a second before the deadline: {page}"
    );

    server.results_clock.set(due);
    assert!(file_exists(&server, &result.id), "no sweep ran");
    let answer = captured::page(&server, analyst, &result.id, "").await;
    assert_gone(&answer, GoneReason::Expired, "a page at the deadline");
    assert!(
        !file_exists(&server, &result.id),
        "the read removed the file"
    );
    let row = server
        .state
        .results
        .store
        .get(&result.id, key_id)
        .await
        .unwrap()
        .expect("the tombstone stays");
    assert_eq!(row.state, ResultState::Expired);
    assert_eq!(
        row.gone_at,
        Some(due),
        "the tombstone clock starts at the deadline"
    );
    let answer = captured::cell(&server, analyst, &result.id, 0, 0, "").await;
    assert_gone(&answer, GoneReason::Expired, "a cell after the deadline");
    assert!(
        captured::listed(&server, analyst).await.is_empty(),
        "an expired capture is not listed"
    );

    let lifetime = TimeDelta::seconds(86_400);
    server
        .results_clock
        .set(due + lifetime - TimeDelta::minutes(1));
    let answer = captured::page(&server, analyst, &result.id, "").await;
    assert_gone(
        &answer,
        GoneReason::Expired,
        "a minute before the tombstone ends",
    );

    let now = due + lifetime + TimeDelta::minutes(1);
    server.results_clock.set(now);
    let answer = captured::page(&server, analyst, &result.id, "").await;
    assert_refused(
        &answer,
        StatusCode::NOT_FOUND,
        "not_found",
        "a minute after the tombstone ends",
    );
    let swept = sweep(&server.state.results, now).await;
    assert_eq!(swept.tombstones, 1, "{swept:?}");
    assert_eq!(swept.errors, 0, "{swept:?}");
    assert_eq!(
        table_rows(&server).await,
        0,
        "the sweep deleted the tombstone"
    );
}

/// A capture nobody reads expires at the sweep: its row is marked, its
/// file goes, and a read after answers 410.
#[tokio::test(flavor = "multi_thread")]
async fn the_sweep_expires_a_capture_nobody_reads() {
    let server = boot(CaptureConfig::default()).await;
    let result = events(&server).await;
    let due = deadline(&result);

    let early = sweep(&server.state.results, due - TimeDelta::seconds(1)).await;
    assert_eq!((early.expired, early.unlinked), (0, 0), "{early:?}");
    assert!(file_exists(&server, &result.id));

    server.results_clock.set(due);
    let swept = sweep(&server.state.results, due).await;
    assert_eq!((swept.expired, swept.unlinked), (1, 1), "{swept:?}");
    assert_eq!(swept.errors, 0, "{swept:?}");
    assert!(!file_exists(&server, &result.id));
    assert!(captured::root_names(&server).is_empty());

    let answer = captured::page(&server, &server.analyst_token, &result.id, "").await;
    assert_gone(&answer, GoneReason::Expired, "a page after the sweep");
}

/// A key quota lowered while trawld is down prunes the key's oldest
/// captures at the next boot: each answers 410 with the pruned text and
/// its file is gone, while the newer ones still read.
#[tokio::test(flavor = "multi_thread")]
async fn a_quota_lowered_across_a_restart_prunes_the_oldest_capture() {
    let (mut fixture, server) = Restartable::boot(
        RateLimitConfig::default(),
        CaptureConfig::default(),
        |data| {
            captured::seed_hour(data, captured::SERVICE, &captured::corpus());
        },
    )
    .await;
    let mut ids = Vec::new();
    for minute in ["10:30", "10:31", "10:32"] {
        server
            .results_clock
            .set(at(&format!("2024-01-16T{minute}:00Z")));
        ids.push(events(&server).await.id);
    }
    let [oldest, middle, newest] = <[String; 3]>::try_from(ids).unwrap();

    fixture.capture.per_key_results = 2;
    let server = fixture.restart(server).await;

    let analyst = &server.analyst_token;
    let answer = captured::page(&server, analyst, &oldest, "").await;
    assert_gone(&answer, GoneReason::Pruned, "the oldest capture");
    assert!(!file_exists(&server, &oldest), "boot removed its file");
    for id in [&middle, &newest] {
        let (status, _, page) = captured::page(&server, analyst, id, "").await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert!(file_exists(&server, id));
    }
    assert_eq!(captured::listed(&server, analyst).await, [newest, middle]);
}

/// A committed capture whose file was removed out of band answers 409
/// `result_unavailable`, never an empty page, and the read marks its row.
#[tokio::test(flavor = "multi_thread")]
async fn a_removed_file_answers_unavailable_and_marks_the_row() {
    let server = boot(CaptureConfig::default()).await;
    let result = events(&server).await;
    let analyst = &server.analyst_token;
    let key_id = captured::key_id(&server, analyst).await;
    let path = server.state.results.root.path_of(&result.id).unwrap();
    std::fs::remove_file(&path).unwrap();

    let answer = captured::page(&server, analyst, &result.id, "").await;
    assert_refused(
        &answer,
        StatusCode::CONFLICT,
        "result_unavailable",
        "a page of a missing file",
    );
    let row = server
        .state
        .results
        .store
        .get(&result.id, key_id)
        .await
        .unwrap()
        .unwrap();
    assert!(row.file_missing, "the read marked the row");
    assert_eq!(row.state, ResultState::Temporary);
    let answer = captured::cell(&server, analyst, &result.id, 0, 0, "").await;
    assert_refused(
        &answer,
        StatusCode::CONFLICT,
        "result_unavailable",
        "a cell of a marked row",
    );
}

/// A file whose bytes changed answers 500 `result_corrupt`, never its
/// rows, and the request logs exactly one ADR-0040 `http_failure`.
#[tokio::test(flavor = "multi_thread")]
async fn a_changed_byte_answers_corrupt_with_one_failure_event() {
    let telemetry = telemetry();
    let server = boot(CaptureConfig::default()).await;
    let result = events(&server).await;
    let path = server.state.results.root.path_of(&result.id).unwrap();
    let mut bytes = std::fs::read(&path).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0x01;
    std::fs::write(&path, bytes).unwrap();

    let answer = captured::page(&server, &server.analyst_token, &result.id, "").await;
    assert_refused(
        &answer,
        StatusCode::INTERNAL_SERVER_ERROR,
        "result_corrupt",
        "a page of a changed file",
    );
    assert_eq!(
        answer.2["error"]["message"],
        "the captured result's file does not match its recorded digest"
    );
    let request_id = answer.1["x-request-id"].to_str().unwrap().to_owned();
    let failures = || telemetry.of_type("http_failure", "request_id", &request_id);
    let give_up = std::time::Instant::now() + Duration::from_secs(5);
    while failures().is_empty() && std::time::Instant::now() < give_up {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // A second event would land with the first: give it the time to.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let failures = failures();
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_eq!(failures[0].field("status"), "500");
    assert!(
        failures[0].field("route").contains("/results/"),
        "{failures:?}"
    );
    assert!(
        file_exists(&server, &result.id),
        "a corrupt file is kept for an operator"
    );
}

/// Another key's capture, an unknown id and a malformed one read alike:
/// 404, each naming its request.
#[tokio::test(flavor = "multi_thread")]
async fn ids_the_caller_does_not_own_are_not_found() {
    let server = boot(CaptureConfig::default()).await;
    let result = events(&server).await;
    let reader = &server.reader_token;
    let unknown = "0".repeat(result.id.len());
    for (what, answer) in [
        (
            "another key's page",
            captured::page(&server, reader, &result.id, "").await,
        ),
        (
            "another key's cell",
            captured::cell(&server, reader, &result.id, 0, 0, "").await,
        ),
        (
            "an unknown id",
            captured::page(&server, reader, &unknown, "").await,
        ),
        (
            "a malformed id",
            captured::page(&server, reader, "not-an-id", "").await,
        ),
    ] {
        assert_refused(&answer, StatusCode::NOT_FOUND, "not_found", what);
    }
    let (status, _, _) = captured::page(&server, &server.analyst_token, &result.id, "").await;
    assert_eq!(status, StatusCode::OK, "the owner still reads it");
}

/// An insert that committed and lost its answer: its fence finds the id
/// taken, so the capture answers 200 and keeps its file, and its
/// reservation is settled rather than counted beside the row.
#[tokio::test(flavor = "multi_thread")]
async fn an_insert_that_committed_and_lost_its_answer_keeps_the_capture() {
    let telemetry = telemetry();
    let server = boot(CaptureConfig {
        per_key_results: 2,
        ..CaptureConfig::default()
    })
    .await;
    let analyst = &server.analyst_token;
    let key_id = captured::key_id(&server, analyst).await;
    let permits = server.state.query.pool.available_permits();
    server
        .state
        .results
        .store
        .faults()
        .fail_next_insert(InsertFault::Committed);

    let result = events(&server).await;
    assert_fenced(telemetry, &result.id, "id_taken");
    captured::assert_nothing_held(&server, permits, "a capture whose answer was lost").await;
    let row = server
        .state
        .results
        .store
        .get(&result.id, key_id)
        .await
        .unwrap()
        .expect("the row committed");
    assert_eq!(row.state, ResultState::Temporary);
    assert!(file_exists(&server, &result.id), "the file stays");
    let (status, _, page) = captured::page(&server, analyst, &result.id, "").await;
    assert_eq!(status, StatusCode::OK, "{page}");

    assert_one_of_two_taken(&server).await;
}

/// The capture's fence logged one `results_commit_fenced` event, whose
/// `fence` is `outcome`: `id_taken` when the first insert's row was there,
/// `inserted` when the fence inserted it.
fn assert_fenced(telemetry: &Capture, id: &str, outcome: &str) {
    let fenced = telemetry.of_type("results_commit_fenced", "result_id", id);
    assert_eq!(fenced.len(), 1, "{fenced:?}");
    assert!(fenced[0].field("fence").contains(outcome), "{fenced:?}");
}

/// The capture a 200 answers, and the request it names.
fn answered(answer: &captured::Answer, what: &str) -> (CaptureResponse, String) {
    let (status, headers, body) = answer;
    assert_eq!(*status, StatusCode::OK, "{what}: {body}");
    let request_id = headers["x-request-id"].to_str().unwrap().to_owned();
    let response = serde_json::from_value(body.clone()).expect("a capture response");
    (response, request_id)
}

/// The key holds one of two results, its capture's row: a second capture
/// fits, so the settled reservation is not counted beside the row, and a
/// third does not.
async fn assert_one_of_two_taken(server: &TestServer) {
    events(server).await;
    let answer = captured::capture(server, &server.analyst_token, &events_body()).await;
    assert_refused(
        &answer,
        StatusCode::TOO_MANY_REQUESTS,
        "capture_quota_exceeded",
        "a third capture",
    );
    assert_eq!(table_rows(server).await, 2);
}

/// An insert that answers an error before its statement reached the
/// backend: the fence inserts the row, so the capture answers 200 with its
/// row and its file and books one timing event. The first statement,
/// arriving after the fence, meets the taken id and commits nothing.
#[tokio::test(flavor = "multi_thread")]
async fn an_insert_whose_statement_had_not_arrived_is_committed_by_its_fence() {
    let telemetry = telemetry();
    let server = boot(CaptureConfig {
        per_key_results: 2,
        ..CaptureConfig::default()
    })
    .await;
    let analyst = &server.analyst_token;
    let key_id = captured::key_id(&server, analyst).await;
    let results = &server.state.results;
    let permits = server.state.query.pool.available_permits();
    let late = results.store.faults().land_next_insert_late();

    let answer = captured::capture(&server, analyst, &events_body()).await;
    let (response, request_id) = answered(&answer, "a capture whose insert answered an error");
    let id = response.result.id;
    assert_fenced(telemetry, &id, "inserted");
    let timings = telemetry.of_type("query_timing", "request_id", &request_id);
    assert_eq!(timings.len(), 1, "{timings:?}");
    assert!(file_exists(&server, &id), "the fenced row keeps its file");
    assert!(results.in_flight.snapshot().is_empty());
    captured::assert_nothing_held(&server, permits, "a fenced capture").await;

    let _ = late.release.send(());
    assert!(
        !within("the first statement runs", late.landed)
            .await
            .unwrap(),
        "the first statement meets the fence's row and commits nothing"
    );
    let row = results
        .store
        .get(&id, key_id)
        .await
        .unwrap()
        .expect("the fence's row");
    assert_eq!(row.state, ResultState::Temporary);
    assert!(!row.file_missing, "the row's file is there");
    assert_eq!(table_rows(&server).await, 1, "one row");
    let (status, _, page) = captured::page(&server, analyst, &id, "").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_one_of_two_taken(&server).await;
}

/// The first insert's transaction is open: its statement ran and its
/// commit is on its way. The fence waits on its row's key, so the capture
/// does not answer, its file stays in flight and no row is visible. When
/// the transaction ends with COMMIT the fence finds the id taken; with
/// ROLLBACK it inserts the row itself. Either way the capture answers 200,
/// keeps its file and holds one row.
#[tokio::test(flavor = "multi_thread")]
async fn a_fence_waits_for_an_open_commit_that_commits() {
    fence_waits_for_an_open_commit(CommitEnd::Commit).await;
}

/// [`a_fence_waits_for_an_open_commit_that_commits`], with the first
/// insert's transaction rolled back.
#[tokio::test(flavor = "multi_thread")]
async fn a_fence_waits_for_an_open_commit_that_rolls_back() {
    fence_waits_for_an_open_commit(CommitEnd::Rollback).await;
}

/// Wait until a session waits on a lock that backend `pid` holds, in an
/// insert of a capture's row.
async fn fence_blocked_by(probe: &sqlx::PgPool, pid: i32) {
    loop {
        let blocked: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
             WHERE $1 = ANY(pg_blocking_pids(pid))
               AND query LIKE 'INSERT INTO captured_results%'",
        )
        .bind(pid)
        .fetch_one(probe)
        .await
        .unwrap();
        if blocked == 1 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// The open-commit test, ended with `end`. Besides the fixture's pools it
/// holds the first insert's own connection and a one-connection probe pool,
/// inside the direct-store ceiling.
async fn fence_waits_for_an_open_commit(end: CommitEnd) {
    let telemetry = telemetry();
    let server = boot(CaptureConfig {
        per_key_results: 2,
        ..CaptureConfig::default()
    })
    .await;
    let analyst = &server.analyst_token;
    let key_id = captured::key_id(&server, analyst).await;
    let results = &server.state.results;
    let permits = server.state.query.pool.available_permits();
    let mut first = PgConnection::connect(&server.app_db_url).await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut first)
        .await
        .unwrap();
    let held = results.store.faults().hold_next_commit(first);
    let probe = common::fixture_pool(&server.app_db_url, 1).await;

    let body = events_body();
    let capture = captured::capture(&server, analyst, &body);
    tokio::pin!(capture);
    tokio::select! {
        executed = held.executed => executed.expect("the first insert ran its statement"),
        answer = &mut capture => panic!("the capture answered before its insert ran: {answer:?}"),
    }
    tokio::select! {
        () = within("the fence waits on the open row", fence_blocked_by(&probe, pid)) => {}
        answer = &mut capture => {
            panic!("the capture answered while its first insert's transaction is open: {answer:?}")
        }
    }
    // Postgres holds the fence: nothing answers while the transaction is
    // open, and the file is neither committed nor unlinked.
    assert!(
        tokio::time::timeout(Duration::from_millis(300), &mut capture)
            .await
            .is_err(),
        "the capture answers only once the open transaction ends"
    );
    let id = published(&server);
    assert!(
        results.in_flight.snapshot().contains(&id),
        "the file stays in flight while the fence waits"
    );
    let visible: i64 = sqlx::query_scalar("SELECT count(*) FROM captured_results")
        .fetch_one(&probe)
        .await
        .unwrap();
    assert_eq!(
        visible, 0,
        "no row is committed while the transaction is open"
    );
    probe.close().await;

    let _ = held.end.send(end);
    let answer = within("the capture answers", &mut capture).await;
    let (response, _) = answered(&answer, "a capture fenced behind an open commit");
    assert_eq!(response.result.id, id);
    assert_eq!(
        within("the transaction ends", held.landed).await.unwrap(),
        end == CommitEnd::Commit,
        "the first insert commits exactly when its transaction does"
    );
    let outcome = match end {
        CommitEnd::Commit => "id_taken",
        CommitEnd::Rollback => "inserted",
    };
    assert_fenced(telemetry, &id, outcome);

    assert!(file_exists(&server, &id), "the row keeps its file");
    assert!(results.in_flight.snapshot().is_empty());
    let row = results
        .store
        .get(&id, key_id)
        .await
        .unwrap()
        .expect("the capture's row");
    assert_eq!(row.state, ResultState::Temporary);
    assert!(!row.file_missing, "the row's file is there");
    let (status, _, page) = captured::page(&server, analyst, &id, "").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(captured::listed(&server, analyst).await, [id]);
    captured::assert_nothing_held(&server, permits, "a capture fenced behind an open commit").await;
    assert_one_of_two_taken(&server).await;
}

/// `future`, or a failure naming `what` once it has had thirty seconds.
async fn within<T>(what: &str, future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(30), future)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for: {what}"))
}

/// The id of the one published file in the results root.
fn published(server: &TestServer) -> String {
    let names = captured::root_names(server);
    assert_eq!(names.len(), 1, "one published file: {names:?}");
    names[0]
        .strip_suffix(".parquet")
        .filter(|_| !names[0].starts_with(".stage-"))
        .unwrap_or_else(|| panic!("a published file, not a stage: {names:?}"))
        .to_owned()
}

/// An instant past every file's age floor. File times are the system
/// clock's, and the fixture's results clock stands in 2024.
fn past_every_floor() -> DateTime<Utc> {
    Utc::now() + TimeDelta::hours(1)
}

/// An insert that answers an error while its statement is still on its
/// way to a commit, and a fence that fails: the capture answers 503 with
/// its commit in doubt. Through the grace the file stays in flight, a sweep
/// leaves it, and the reservation stays charged. The late commit lands,
/// and when the grace ends the row is found and keeps its file: a page
/// reads 200 and the row is not marked missing.
#[tokio::test(flavor = "multi_thread")]
async fn an_insert_that_commits_after_its_error_keeps_its_file() {
    let server = boot(CaptureConfig {
        per_key_results: 1,
        ..CaptureConfig::default()
    })
    .await;
    let analyst = &server.analyst_token;
    let key_id = captured::key_id(&server, analyst).await;
    let results = &server.state.results;
    let late = results.store.faults().land_next_insert_late();
    results.store.faults().fail_next_insert(InsertFault::Lost);
    let grace = results.store.faults().hold_next_grace();

    let answer = captured::capture(&server, analyst, &events_body()).await;
    assert_refused(
        &answer,
        StatusCode::SERVICE_UNAVAILABLE,
        "service_unavailable",
        "a capture whose insert answered an error",
    );
    let id = published(&server);
    assert!(
        results.in_flight.snapshot().contains(&id),
        "the file stays in flight"
    );
    within("the grace begins", grace.parked)
        .await
        .expect("the capture holds its grace");
    assert_eq!(table_rows(&server).await, 0, "the insert has not landed");

    // Inside the grace, with the insert still on its way, the sweep finds a
    // file no row names and leaves it.
    let swept = sweep(results, past_every_floor()).await;
    assert_eq!(swept.orphans, 0, "{swept:?}");
    assert!(
        file_exists(&server, &id),
        "the sweep leaves a file in doubt"
    );
    // The reservation stays charged: the key's one result is taken.
    let answer = captured::capture(&server, analyst, &events_body()).await;
    assert_refused(
        &answer,
        StatusCode::TOO_MANY_REQUESTS,
        "capture_quota_exceeded",
        "a capture during the grace",
    );

    let _ = late.release.send(());
    assert!(
        within("the late insert runs", late.landed).await.unwrap(),
        "the late insert commits"
    );
    let _ = grace.release.send(());
    within("the capture settles", grace.settled)
        .await
        .expect("the capture settles");

    assert!(file_exists(&server, &id), "the late row keeps its file");
    assert!(results.in_flight.snapshot().is_empty());
    let (status, _, page) = captured::page(&server, analyst, &id, "").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let row = results
        .store
        .get(&id, key_id)
        .await
        .unwrap()
        .expect("the late row");
    assert_eq!(row.state, ResultState::Temporary);
    assert!(!row.file_missing, "the row's file is there");
    assert_eq!(captured::listed(&server, analyst).await, [id]);
}

/// An insert that never commits, and a fence that fails without
/// committing: the capture answers 503 with its commit in doubt. Through the grace the file
/// stays and the reservation is charged; when the grace ends the row is
/// still absent, so the file goes and the reservation lapses, and a sweep
/// after it finds nothing.
#[tokio::test(flavor = "multi_thread")]
async fn an_insert_that_never_commits_loses_its_file_when_the_grace_ends() {
    let server = boot(CaptureConfig {
        per_key_results: 1,
        ..CaptureConfig::default()
    })
    .await;
    let analyst = &server.analyst_token;
    let results = &server.state.results;
    let permits = server.state.query.pool.available_permits();
    results.store.faults().fail_next_insert(InsertFault::Lost);
    results.store.faults().fail_next_insert(InsertFault::Lost);
    let grace = results.store.faults().hold_next_grace();

    let answer = captured::capture(&server, analyst, &events_body()).await;
    assert_refused(
        &answer,
        StatusCode::SERVICE_UNAVAILABLE,
        "service_unavailable",
        "a capture whose insert failed",
    );
    let id = published(&server);
    within("the grace begins", grace.parked)
        .await
        .expect("the capture holds its grace");
    assert!(results.in_flight.snapshot().contains(&id));
    let swept = sweep(results, past_every_floor()).await;
    assert_eq!(swept.orphans, 0, "{swept:?}");
    assert!(
        file_exists(&server, &id),
        "the sweep leaves a file in doubt"
    );
    let answer = captured::capture(&server, analyst, &events_body()).await;
    assert_refused(
        &answer,
        StatusCode::TOO_MANY_REQUESTS,
        "capture_quota_exceeded",
        "a capture during the grace",
    );

    let _ = grace.release.send(());
    within("the capture settles", grace.settled)
        .await
        .expect("the capture settles");
    assert!(captured::root_names(&server).is_empty(), "no file");
    assert_eq!(table_rows(&server).await, 0, "no row");
    assert!(results.in_flight.snapshot().is_empty());
    assert_eq!(sweep(results, past_every_floor()).await, Sweep::default());
    captured::assert_nothing_held(&server, permits, "a capture whose insert failed").await;
    // The key's one result is free: the reservation lapsed.
    events(&server).await;
}

/// An insert whose outcome stays unknown: it and its fence fail, and
/// reading the row fails when the grace ends. The capture answers 503 and
/// leaves its file, since a committed row may still name it, and its
/// reservation lapses. The next sweep finds no row naming the file and removes it,
/// with no restart.
#[tokio::test(flavor = "multi_thread")]
async fn an_insert_whose_outcome_stays_unknown_leaves_its_file_to_the_sweep() {
    let server = boot(CaptureConfig {
        per_key_results: 1,
        ..CaptureConfig::default()
    })
    .await;
    let results = &server.state.results;
    results.store.faults().fail_next_insert(InsertFault::Lost);
    results
        .store
        .faults()
        .fail_next_insert(InsertFault::Unreadable);
    let grace = results.store.faults().hold_next_grace();

    let answer = captured::capture(&server, &server.analyst_token, &events_body()).await;
    assert_refused(
        &answer,
        StatusCode::SERVICE_UNAVAILABLE,
        "service_unavailable",
        "a capture whose insert and read-back failed",
    );
    let id = published(&server);
    within("the grace begins", grace.parked)
        .await
        .expect("the capture holds its grace");
    let _ = grace.release.send(());
    within("the capture settles", grace.settled)
        .await
        .expect("the capture settles");

    assert!(file_exists(&server, &id), "the file stays");
    assert_eq!(table_rows(&server).await, 0, "no row");
    assert!(results.in_flight.snapshot().is_empty());
    let swept = sweep(results, past_every_floor()).await;
    assert_eq!(swept.orphans, 1, "{swept:?}");
    assert!(captured::root_names(&server).is_empty());
    events(&server).await;
}

/// The read a race test holds.
#[derive(Debug, Clone, Copy)]
enum Read {
    Page,
    Cell,
}

/// What expires the capture while the read is held.
#[derive(Debug, Clone, Copy)]
enum Expiry {
    /// Another client reads the capture at its deadline.
    AnotherRead,
    /// The sweep runs at the deadline.
    Sweep,
}

/// `read` of capture `id` as the analyst.
async fn read(server: &TestServer, read: Read, id: &str) -> captured::Answer {
    match read {
        Read::Page => captured::page(server, &server.analyst_token, id, "").await,
        Read::Cell => captured::cell(server, &server.analyst_token, id, 0, 0, "").await,
    }
}

/// Wait, a bounded while, for `cond`.
async fn until(what: &str, cond: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while !cond() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for: {what}"));
}

/// A read that passed the row checks a second before the deadline is held
/// at `seam`; the clock reaches the deadline and `expiry` marks the row
/// expired and unlinks the file; then the read goes on. It answers what a
/// read after the expiry answers, 410 `expired`, and leaves the row an
/// expired tombstone, not one marked `file_missing`.
async fn a_read_racing_the_expiry(seam: Seam, kind: Read, expiry: Expiry) {
    let what = format!("{kind:?} held at {seam:?}, expired by {expiry:?}");
    let server = boot(CaptureConfig::default()).await;
    let result = events(&server).await;
    let key_id = captured::key_id(&server, &server.analyst_token).await;
    let due = deadline(&result);
    server.results_clock.set(due - TimeDelta::seconds(1));

    let seams = server.state.query.pool.seams();
    let gate = seams.hold(seam);
    let (held, ()) = tokio::join!(read(&server, kind, &result.id), async {
        until("the read reaches the seam", || gate.arrivals() == 1).await;
        server.results_clock.set(due);
        match expiry {
            Expiry::AnotherRead => {
                let answer = read(&server, kind, &result.id).await;
                assert_gone(
                    &answer,
                    GoneReason::Expired,
                    &format!("{what}: the other read"),
                );
            }
            Expiry::Sweep => {
                let swept = sweep(&server.state.results, due).await;
                assert_eq!((swept.expired, swept.unlinked), (1, 1), "{what}: {swept:?}");
            }
        }
        assert!(
            !file_exists(&server, &result.id),
            "{what}: the expiry removed the file"
        );
        gate.release();
    });
    assert_gone(
        &held,
        GoneReason::Expired,
        &format!("{what}: the held read"),
    );
    let row = server
        .state
        .results
        .store
        .get(&result.id, key_id)
        .await
        .unwrap()
        .expect("the tombstone stays");
    assert_eq!(row.state, ResultState::Expired, "{what}");
    assert!(!row.file_missing, "{what}: an expired row is not marked");
}

/// A page whose capture another read expires while the page waits to
/// check its file, or to open it, answers 410 `expired`.
#[tokio::test(flavor = "multi_thread")]
async fn a_page_racing_another_reads_expiry_answers_expired() {
    for seam in [Seam::Verifying, Seam::Opening] {
        a_read_racing_the_expiry(seam, Read::Page, Expiry::AnotherRead).await;
    }
}

/// The same race against the sweep.
#[tokio::test(flavor = "multi_thread")]
async fn a_page_racing_the_sweeps_expiry_answers_expired() {
    for seam in [Seam::Verifying, Seam::Opening] {
        a_read_racing_the_expiry(seam, Read::Page, Expiry::Sweep).await;
    }
}

/// A cell read racing another read's expiry answers 410 `expired`.
#[tokio::test(flavor = "multi_thread")]
async fn a_cell_racing_another_reads_expiry_answers_expired() {
    for seam in [Seam::Verifying, Seam::Opening] {
        a_read_racing_the_expiry(seam, Read::Cell, Expiry::AnotherRead).await;
    }
}

/// A cell read racing the sweep's expiry answers 410 `expired`.
#[tokio::test(flavor = "multi_thread")]
async fn a_cell_racing_the_sweeps_expiry_answers_expired() {
    for seam in [Seam::Verifying, Seam::Opening] {
        a_read_racing_the_expiry(seam, Read::Cell, Expiry::Sweep).await;
    }
}

/// What happens to a live capture's file, out of band, while a read is
/// held.
#[derive(Debug, Clone, Copy)]
enum Tamper {
    /// The file is removed.
    Remove,
    /// The file's bytes are replaced with bytes that are not parquet.
    Replace,
}

/// The controls for the races above: while the capture is still live, a
/// file removed out of band during a held read answers 409
/// `result_unavailable` and marks the row, and a file replaced answers 500
/// `result_corrupt`, whether the read was held before its digest check or
/// after it, when only the engine's open can notice.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_lost_out_of_band_during_a_read_is_unavailable_or_corrupt() {
    let server = boot(CaptureConfig::default()).await;
    let key_id = captured::key_id(&server, &server.analyst_token).await;
    for seam in [Seam::Verifying, Seam::Opening] {
        for kind in [Read::Page, Read::Cell] {
            for tamper in [Tamper::Remove, Tamper::Replace] {
                let what = format!("{kind:?} held at {seam:?}, file {tamper:?}d");
                let result = events(&server).await;
                let path = server.state.results.root.path_of(&result.id).unwrap();
                let seams = server.state.query.pool.seams();
                let gate = seams.hold(seam);
                let (held, ()) = tokio::join!(read(&server, kind, &result.id), async {
                    until("the read reaches the seam", || gate.arrivals() == 1).await;
                    match tamper {
                        Tamper::Remove => std::fs::remove_file(&path).unwrap(),
                        Tamper::Replace => std::fs::write(&path, b"not a parquet file").unwrap(),
                    }
                    gate.release();
                });
                drop(seams);
                let row = server
                    .state
                    .results
                    .store
                    .get(&result.id, key_id)
                    .await
                    .unwrap()
                    .expect("the row stays");
                assert_eq!(row.state, ResultState::Temporary, "{what}");
                match tamper {
                    Tamper::Remove => {
                        assert_refused(&held, StatusCode::CONFLICT, "result_unavailable", &what);
                        assert!(row.file_missing, "{what}: the read marked the row");
                    }
                    Tamper::Replace => {
                        assert_refused(
                            &held,
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "result_corrupt",
                            &what,
                        );
                        assert!(!row.file_missing, "{what}");
                    }
                }
            }
        }
    }
}
