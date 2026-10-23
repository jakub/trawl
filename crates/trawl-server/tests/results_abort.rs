// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A capture whose client walks away mid-execution (ADR-0057, "Admission
//! is per key"), through `trawl-client` over a real TLS listener: the
//! transport drops the handler, the pool latches cancellation and
//! interrupts `DuckDB`, the engine never finishes, and the executor
//! permit, the admission slot and the quota reservation stay charged
//! until the work physically stops.
//!
//! Two transports carry each sequence. The fixture's own listener is the
//! production shape, HTTP/1.1 alone. The second is
//! `serve_with_listener_h2_for_tests`, which negotiates HTTP/2. This
//! listener exists only in tests. trawld serves HTTP/1.1 only (ADR-0054);
//! this proves the router and the client under HTTP/2, not the production
//! edge.
//!
//! Two sequences, one leg of each per transport:
//!
//! - Before execution. The work is parked at the pool's `Started` seam,
//!   past its work-start transition and before the engine runs, so the
//!   drop lands on work that is physically running. Released, it stops at
//!   the latch on the engine's bind-to-execute boundary: `DuckDB` never
//!   executes. These legs prove the handler's abandonment and the held
//!   charges.
//! - During execution. The capture runs a slow query: a regex that no
//!   message of a one-row-group corpus matches and that RE2 answers only by
//!   searching every byte, seconds of `DuckDB` execution. The client drops
//!   while `DuckDB` executes it, and the work is held at the pool's
//!   `Returned` seam, once the engine returned and before the slot lets go
//!   of anything. These legs prove that the drop's interrupt stops `DuckDB`
//!   mid-execution and that every charge is still held after the engine
//!   stopped, until the work releases it. The engine had not returned
//!   before the drop. It booked an `execute` phase. It ended `failed`, a
//!   database error, where the latch's stop is `cancelled` and the query
//!   left alone completes, as its control shows.
//!
//! The capture limits are tightened so each charge is visible from
//! outside: one result per key, so the held reservation refuses the same
//! key's next capture at its quota pre-check, and one capture
//! installation-wide, so the held slot refuses another key's.
//!
//! The mid-execution legs read the pool's own events, so each installs a
//! process-wide subscriber: nextest runs every test in its own process.
//! The binary is in the ADR-0021 `postgres` nextest group.

mod common;

use std::path::Path;
use std::time::Duration;

use common::TestServer;
use common::audit_capture::{Capture, Captured};
use common::captured::{self, at};
use tracing_subscriber::layer::SubscriberExt;
use trawl_api::{ErrorCode, QueriesResponse};
use trawl_client::{CaptureRequest, CaptureWindow, ClientError, HttpClient, TlsTrust};
use trawl_config::CaptureConfig;
use trawl_server::config::RateLimitConfig;
use trawl_server::pool::WorkKind;
use trawl_server::pool::seam::Seam;

/// The service of the [`slow_corpus`], which no other seed row has.
const SLOW_SERVICE: &str = "slowcorpus";

/// Events in the [`slow_corpus`]: one Parquet row group, so one thread
/// scans all of it.
const SLOW_ROWS: u32 = 32_768;

/// A regex no [`slow_corpus`] message matches. On hex text its run of 60
/// exhausts RE2's DFA state cache, and the search costs about a
/// microsecond a byte: seconds of execution over a few megabytes.
const SLOW_PATTERN: &str = "[0-9a][0-9a-f]{60}z";

/// How long a mid-execution leg waits past `Started` before it drops the
/// client: long past the bind of one file, and a quarter or less of the
/// slow query's execution, which its control holds to.
const INTO_EXECUTE: Duration = Duration::from_millis(500);

/// A fixture over the capture corpus whose limits make each charge
/// observable: one result per key, one capture installation-wide.
async fn boot() -> TestServer {
    let server = captured::boot(
        RateLimitConfig::default(),
        CaptureConfig {
            per_key_results: 1,
            per_key_running: 1,
            installation_running: 1,
            ..CaptureConfig::default()
        },
    )
    .await;
    server.results_clock.set(at("2024-01-16T10:30:00Z"));
    server
}

/// Write [`SLOW_ROWS`] events of [`SLOW_SERVICE`] with 128-character hex
/// messages, all distinct, as one Parquet file in the [`captured::corpus`]
/// hour, with the shared fixture's columns.
fn slow_corpus(data: &str) {
    let hour = Path::new(data).join("prod/2024-01-16/10");
    std::fs::create_dir_all(&hour).expect("create the corpus hour");
    let conn = duckdb::Connection::open_in_memory().expect("open duckdb");
    conn.execute_batch(&format!(
        "COPY (SELECT TIMESTAMP '2024-01-16 10:00:00' + to_microseconds(i) AS _time, \
                TIMESTAMP '2024-01-16 10:00:10' + to_microseconds(i) AS _ingested, \
                NULL::VARCHAR AS _raw, NULL::VARCHAR AS _repairs, 'prod' AS env, \
                '{SLOW_SERVICE}' AS service, 'web01' AS host, 9::BIGINT AS severity, \
                'info' AS severity_text, \
                md5(i::VARCHAR) || md5((i * 7)::VARCHAR) || md5((i * 11)::VARCHAR) \
                    || md5((i * 13)::VARCHAR) AS message \
         FROM range({SLOW_ROWS}) t(i)) TO '{}' (FORMAT PARQUET)",
        hour.join(format!("{SLOW_SERVICE}.parquet")).display()
    ))
    .expect("write the slow corpus");
}

/// [`boot`]'s fixture with the [`slow_corpus`] beside the capture corpus.
/// Queries and captures may run a minute, so the control's slow capture
/// completes under any load. The tempdir is leaked so it outlives the
/// server.
async fn boot_slow() -> TestServer {
    let tmp = tempfile::tempdir().expect("create a temp dir");
    let data = common::seed_data_root(tmp.path());
    captured::seed_hour(&data, captured::SERVICE, &captured::corpus());
    slow_corpus(&data);
    let server = common::setup_with_capture_timeout(
        tmp.path(),
        data,
        RateLimitConfig::default(),
        CaptureConfig {
            per_key_results: 1,
            per_key_running: 1,
            installation_running: 1,
            deadline_secs: 60,
            ..CaptureConfig::default()
        },
        60,
    )
    .await;
    std::mem::forget(tmp);
    server.results_clock.set(at("2024-01-16T10:30:00Z"));
    server
}

/// Record every event this process logs, from every thread.
fn record_events() -> Capture {
    let events = Capture::default();
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(events.clone()))
        .expect("this test owns the process's tracing subscriber");
    events
}

/// The recorded events of `event_type` whose fields carry every
/// `(name, value)` of `matching`. A string field is recorded through
/// `Debug`, so it is matched quoted.
fn recorded(events: &Capture, event_type: &str, matching: &[(&str, &str)]) -> Vec<Captured> {
    let quoted = |value: &str| format!("{value:?}");
    events
        .events()
        .into_iter()
        .filter(|event| {
            event.fields.get("event_type") == Some(&quoted(event_type))
                && matching
                    .iter()
                    .all(|(name, value)| event.fields.get(*name) == Some(&quoted(value)))
        })
        .collect()
}

/// The listener at `url` answers `version`.
async fn assert_answers_over(url: &str, version: reqwest::Version, what: &str) {
    let response = common::harness_client_builder()
        .build()
        .unwrap()
        .get(format!("{url}/api/v1/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.version(), version, "{what}");
}

/// A `trawl-client` for `url` as `token`, trusting the fixture's
/// certificate alone, with verification on.
fn client(url: &str, token: &str) -> HttpClient {
    let (cert, _) = common::ensure_test_cert();
    let pem = std::fs::read(cert).expect("read the shared test certificate");
    HttpClient::with_trust(url, token, &TlsTrust::PinnedCa(pem)).expect("build the client")
}

/// The capture every request here makes: the 20-event corpus, last hour.
fn request() -> CaptureRequest {
    CaptureRequest {
        query: format!("service={}", captured::SERVICE),
        window: Some(CaptureWindow {
            last: Some("1h".to_owned()),
            ..CaptureWindow::default()
        }),
    }
}

/// The slow capture: a [`SLOW_PATTERN`] search of the [`slow_corpus`],
/// last hour. It captures no row.
fn slow_request() -> CaptureRequest {
    CaptureRequest {
        query: format!("service={SLOW_SERVICE} message=/{SLOW_PATTERN}/"),
        window: Some(CaptureWindow {
            last: Some("1h".to_owned()),
            ..CaptureWindow::default()
        }),
    }
}

/// `answer` is a 429 refusal with `code`.
fn assert_refused<T: std::fmt::Debug>(
    answer: Result<T, ClientError>,
    code: &ErrorCode,
    what: &str,
) {
    match answer {
        Err(ClientError::Server { status, error, .. }) => {
            assert_eq!(status, 429, "{what}: {error:?}");
            assert_eq!(&error.code, code, "{what}: {error:?}");
        }
        other => panic!("{what}: expected a 429 {code:?}, got {other:?}"),
    }
}

/// Every entry in the results root, sorted. The root's lock is a `flock`
/// on the directory itself, so an idle root is empty.
fn results_root_entries(server: &TestServer) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(server.state.results.root.path())
        .expect("list the results root")
        .map(|entry| {
            entry
                .expect("a results root entry")
                .file_name()
                .into_string()
                .expect("a UTF-8 entry name")
        })
        .collect();
    names.sort();
    names
}

/// Poll `cond` until it holds, failing after a generous bound.
async fn until(what: &str, cond: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while !cond() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for: {what}"));
}

/// `queries` lists query `id` in recent history as abandoned: no rows, the
/// abandonment's error, and not a timeout.
fn assert_abandoned(queries: &QueriesResponse, id: u64, what: &str) {
    let recent = queries
        .recent
        .iter()
        .find(|q| q.snapshot.id == id)
        .unwrap_or_else(|| panic!("{what}: query {id} is in recent history: {queries:?}"));
    assert_eq!(recent.snapshot.rows, None, "{what}: {recent:?}");
    assert_eq!(
        recent.snapshot.error.as_deref(),
        Some(trawl_server::tracker::ABANDONED),
        "{what}: {recent:?}"
    );
    assert!(!recent.snapshot.timed_out, "{what}: {recent:?}");
}

/// The abort sequence against the listener at `url`, over the fixture's
/// state: drop a capture's request while its work is parked at `Started`,
/// see every charge held, release the work, see every charge freed.
#[allow(clippy::too_many_lines)] // one sequence, each step asserted in order
async fn a_dropped_capture_holds_its_charges_until_its_work_stops(
    server: &TestServer,
    url: &str,
    protocol: &str,
) {
    let pool = &server.state.query.pool;
    let permits = pool.available_permits();
    let seams = pool.seams();
    let started = seams.hold(Seam::Started);
    let finished = seams.watch(Seam::Finished);
    let released = seams.watch(Seam::Released);
    let analyst = client(url, &server.analyst_token);
    let reader = client(url, &server.reader_token);
    let before = server.state.query.tracker.active().len();

    // The client's request future is dropped while the work is running.
    let dropped = tokio::spawn({
        let analyst = analyst.clone();
        async move { analyst.capture(&request()).await }
    });
    until("the capture's work started", || started.arrivals() == 1).await;
    assert!(
        !dropped.is_finished(),
        "{protocol}: the capture is in flight"
    );
    dropped.abort();
    assert!(
        dropped
            .await
            .expect_err("the request was aborted")
            .is_cancelled(),
        "{protocol}: the client future was dropped, not answered"
    );

    // The transport dropped the handler: its request guard latched the
    // cancellation and booked the still-running work as retained.
    until("the dropped request's work is retained", || {
        pool.retained() == 1
    })
    .await;
    let retained = pool.retained_work();
    assert_eq!(retained.len(), 1, "{protocol}: {retained:?}");
    assert_eq!(retained[0].kind, WorkKind::Capture, "{protocol}");
    assert!(retained[0].started, "{protocol}: the work had started");
    let id = retained[0].id;

    // Every charge is held while the work exists.
    assert_eq!(
        pool.available_permits(),
        permits - 1,
        "{protocol}: the executor permit is held"
    );
    assert_refused(
        analyst.capture(&request()).await,
        &ErrorCode::CaptureQuotaExceeded,
        &format!("{protocol}: the same key, its one result reserved"),
    );
    assert_refused(
        reader.capture(&request()).await,
        &ErrorCode::CaptureBusy,
        &format!("{protocol}: another key, the installation's one slot held"),
    );
    assert_eq!(
        pool.available_permits(),
        permits - 1,
        "{protocol}: the refusals took no permit"
    );
    assert_eq!(finished.arrivals(), 0, "{protocol}");
    assert_eq!(released.arrivals(), 0, "{protocol}: nothing released yet");

    // Let the work go: the latched cancellation stops the engine before
    // it executes, so the capture never reaches its file write.
    started.release();
    until("the work stopped and released its slot", || {
        released.arrivals() == 1
    })
    .await;
    assert_eq!(
        finished.arrivals(),
        0,
        "{protocol}: the cancelled work never finished"
    );
    until("the permit returns", || {
        pool.available_permits() == permits && pool.retained() == 0
    })
    .await;
    assert_eq!(
        results_root_entries(server),
        Vec::<String>::new(),
        "{protocol}: no staged or published file, nothing at all"
    );
    assert!(
        analyst
            .list_results()
            .await
            .expect("list the analyst's results")
            .results
            .is_empty(),
        "{protocol}: no row was inserted"
    );

    // The request is gone and so is its work: nothing it tracked is
    // still running, and its history says it was abandoned.
    let queries = analyst.queries().await.expect("list the queries");
    assert!(
        !queries.active.iter().any(|q| q.snapshot.id == id),
        "{protocol}: the dropped capture is not active: {:?}",
        queries.active
    );
    assert_eq!(
        server.state.query.tracker.active().len(),
        before,
        "{protocol}: the tracker holds no entry for the dropped capture"
    );
    assert_abandoned(&queries, id, protocol);

    // Free: the same key's next capture is admitted, then another key's.
    let next = analyst
        .capture(&request())
        .await
        .unwrap_or_else(|e| panic!("{protocol}: the same key is admitted again: {e:?}"));
    assert_eq!(next.result.captured_rows, captured::ROWS as u64);
    let other = reader
        .capture(&request())
        .await
        .unwrap_or_else(|e| panic!("{protocol}: the installation slot is free: {e:?}"));
    assert_eq!(other.result.captured_rows, captured::ROWS as u64);
    assert_eq!(finished.arrivals(), 2, "{protocol}");
    assert_eq!(pool.available_permits(), permits, "{protocol}");
}

/// The mid-execution sequence against the listener at `url`, over the
/// fixture's state: drop a slow capture's request while `DuckDB` executes
/// it, see the interrupted engine return with every charge still held,
/// release the work, see every charge freed.
#[allow(clippy::too_many_lines)] // one sequence, each step asserted in order
async fn a_capture_dropped_while_duckdb_executes_is_interrupted_and_holds_its_charges(
    server: &TestServer,
    url: &str,
    protocol: &str,
    events: &Capture,
) {
    let pool = &server.state.query.pool;
    let permits = pool.available_permits();
    let seams = pool.seams();
    let started = seams.watch(Seam::Started);
    let returned = seams.hold(Seam::Returned);
    let finished = seams.watch(Seam::Finished);
    let released = seams.watch(Seam::Released);
    let analyst = client(url, &server.analyst_token);
    let reader = client(url, &server.reader_token);

    // The client's request future is dropped while DuckDB executes: the
    // work started, the bind of one file is long over, and the engine has
    // not returned.
    let dropped = tokio::spawn({
        let analyst = analyst.clone();
        async move { analyst.capture(&slow_request()).await }
    });
    until("the capture's work started", || started.arrivals() == 1).await;
    tokio::time::sleep(INTO_EXECUTE).await;
    assert_eq!(
        returned.arrivals(),
        0,
        "{protocol}: the engine is still executing when the client drops"
    );
    assert!(
        !dropped.is_finished(),
        "{protocol}: the capture is in flight"
    );
    dropped.abort();
    assert!(
        dropped
            .await
            .expect_err("the request was aborted")
            .is_cancelled(),
        "{protocol}: the client future was dropped, not answered"
    );

    // The interrupt stopped DuckDB, and the work is held where the engine
    // handed its answer back: nothing it holds is released yet.
    until("the interrupted engine returned", || {
        returned.arrivals() == 1
    })
    .await;
    until("the dropped request's work is retained", || {
        pool.retained() == 1
    })
    .await;
    let retained = pool.retained_work();
    assert_eq!(retained.len(), 1, "{protocol}: {retained:?}");
    assert_eq!(retained[0].kind, WorkKind::Capture, "{protocol}");
    assert!(retained[0].started, "{protocol}: the work had started");
    assert_eq!(
        pool.available_permits(),
        permits - 1,
        "{protocol}: the executor permit is held after the engine stopped"
    );
    assert_refused(
        analyst.capture(&request()).await,
        &ErrorCode::CaptureQuotaExceeded,
        &format!("{protocol}: the same key, its one result still reserved"),
    );
    assert_refused(
        reader.capture(&request()).await,
        &ErrorCode::CaptureBusy,
        &format!("{protocol}: another key, the installation's one slot still held"),
    );
    assert_eq!(
        pool.available_permits(),
        permits - 1,
        "{protocol}: the refusals took no permit"
    );
    assert_eq!(finished.arrivals(), 0, "{protocol}");
    assert_eq!(released.arrivals(), 0, "{protocol}: nothing released yet");

    // Let the work go: it never reaches its file write, and its slot
    // releases everything.
    returned.release();
    until("the work stopped and released its slot", || {
        released.arrivals() == 1
    })
    .await;
    assert_eq!(
        finished.arrivals(),
        0,
        "{protocol}: the interrupted work never finished"
    );
    until("the permit returns", || {
        pool.available_permits() == permits && pool.retained() == 0
    })
    .await;

    // How the engine ended, from the slot's own account of the permit it
    // gave back: DuckDB executed, and failed, where the latch on the
    // bind-to-execute boundary would have ended it `cancelled` before any
    // execution and the query left alone completes.
    let reclaimed = recorded(
        events,
        "query_permit_reclaimed",
        &[("kind", WorkKind::Capture.as_str())],
    );
    assert_eq!(reclaimed.len(), 1, "{protocol}: {reclaimed:?}");
    let reclaimed = &reclaimed[0];
    assert_eq!(
        reclaimed.field("physical_outcome"),
        format!("{:?}", "failed"),
        "{protocol}: the engine returned DuckDB's interrupt: {reclaimed:?}"
    );
    assert!(
        reclaimed.fields.contains_key("query_execute_us"),
        "{protocol}: DuckDB was executing: {reclaimed:?}"
    );
    assert!(
        !reclaimed.fields.contains_key("query_write_us"),
        "{protocol}: no file was written: {reclaimed:?}"
    );
    assert_eq!(
        results_root_entries(server),
        Vec::<String>::new(),
        "{protocol}: no staged or published file, nothing at all"
    );
    assert!(
        analyst
            .list_results()
            .await
            .expect("list the analyst's results")
            .results
            .is_empty(),
        "{protocol}: no row was inserted"
    );

    // Free: the same key's next capture is admitted, then another key's.
    let next = analyst
        .capture(&request())
        .await
        .unwrap_or_else(|e| panic!("{protocol}: the same key is admitted again: {e:?}"));
    assert_eq!(next.result.captured_rows, captured::ROWS as u64);
    let other = reader
        .capture(&request())
        .await
        .unwrap_or_else(|e| panic!("{protocol}: the installation slot is free: {e:?}"));
    assert_eq!(other.result.captured_rows, captured::ROWS as u64);
    assert_eq!(finished.arrivals(), 2, "{protocol}");
    assert_eq!(pool.available_permits(), permits, "{protocol}");
}

/// HTTP/1.1, the production listener: a dropped capture request
/// interrupts its work and holds every charge until the work stops.
#[tokio::test(flavor = "multi_thread")]
async fn over_http1_a_dropped_capture_holds_its_charges_until_its_work_stops() {
    let server = boot().await;
    assert_answers_over(
        &server.url,
        reqwest::Version::HTTP_11,
        "the production listener answers HTTP/1.1 to a client that offers h2",
    )
    .await;

    a_dropped_capture_holds_its_charges_until_its_work_stops(&server, &server.url, "HTTP/1.1")
        .await;
}

/// HTTP/2, the test-only listener: the same router and the same client
/// behave the same way when the request is one stream on a shared
/// connection.
#[tokio::test(flavor = "multi_thread")]
async fn over_http2_a_dropped_capture_holds_its_charges_until_its_work_stops() {
    let server = boot().await;
    let (url, _serve) = server.spawn_server_h2_for_tests().await;
    assert_answers_over(
        &url,
        reqwest::Version::HTTP_2,
        "the test-only listener negotiates HTTP/2",
    )
    .await;

    a_dropped_capture_holds_its_charges_until_its_work_stops(&server, &url, "HTTP/2").await;
}

/// HTTP/1.1, the production listener: a capture dropped while `DuckDB`
/// executes it interrupts the engine and holds every charge until its
/// work releases them.
#[tokio::test(flavor = "multi_thread")]
async fn over_http1_a_capture_dropped_while_duckdb_executes_is_interrupted_and_holds_its_charges() {
    let events = record_events();
    let server = boot_slow().await;
    assert_answers_over(
        &server.url,
        reqwest::Version::HTTP_11,
        "the production listener answers HTTP/1.1 to a client that offers h2",
    )
    .await;

    a_capture_dropped_while_duckdb_executes_is_interrupted_and_holds_its_charges(
        &server,
        &server.url,
        "HTTP/1.1",
        &events,
    )
    .await;
}

/// HTTP/2, the test-only listener: the same mid-execution sequence when
/// the request is one stream on a shared connection.
#[tokio::test(flavor = "multi_thread")]
async fn over_http2_a_capture_dropped_while_duckdb_executes_is_interrupted_and_holds_its_charges() {
    let events = record_events();
    let server = boot_slow().await;
    let (url, _serve) = server.spawn_server_h2_for_tests().await;
    assert_answers_over(
        &url,
        reqwest::Version::HTTP_2,
        "the test-only listener negotiates HTTP/2",
    )
    .await;

    a_capture_dropped_while_duckdb_executes_is_interrupted_and_holds_its_charges(
        &server, &url, "HTTP/2", &events,
    )
    .await;
}

/// The control for the mid-execution legs: left alone, the slow capture
/// runs its query to completion and answers. Their `failed` is the
/// interrupt's, not the query's own, and `DuckDB` executes it for at least
/// four times [`INTO_EXECUTE`], so their drop lands inside the execution.
#[tokio::test(flavor = "multi_thread")]
async fn an_undropped_slow_capture_runs_its_query_to_completion() {
    let events = record_events();
    let server = boot_slow().await;
    let pool = &server.state.query.pool;
    let seams = pool.seams();
    let finished = seams.watch(Seam::Finished);
    let analyst = client(&server.url, &server.analyst_token);

    let captured = analyst
        .capture(&slow_request())
        .await
        .expect("the slow capture answers");
    assert_eq!(captured.result.captured_rows, 0, "no message matches");
    assert_eq!(finished.arrivals(), 1, "the work reached its file write");

    let accounts = recorded(
        &events,
        "query_timing",
        &[("kind", "capture"), ("outcome", "success")],
    );
    assert_eq!(accounts.len(), 1, "{accounts:?}");
    let execute_us: u128 = accounts[0]
        .field("query_execute_us")
        .parse()
        .expect("whole microseconds");
    assert!(
        execute_us >= 4 * INTO_EXECUTE.as_micros(),
        "DuckDB executed the slow query for {execute_us}us, too briefly for the legs to drop \
         inside it: grow the slow corpus"
    );
}

/// The control: with nothing dropped, the same capture through the same
/// client runs to its file write, publishes one file and inserts its row,
/// so the abort's "no file, no row" is not the capture's default.
#[tokio::test(flavor = "multi_thread")]
async fn an_undropped_capture_finishes_and_keeps_its_row() {
    let server = boot().await;
    let pool = &server.state.query.pool;
    let permits = pool.available_permits();
    let seams = pool.seams();
    let finished = seams.watch(Seam::Finished);
    let analyst = client(&server.url, &server.analyst_token);

    let captured = analyst
        .capture(&request())
        .await
        .expect("the capture answers");
    assert_eq!(captured.result.captured_rows, captured::ROWS as u64);
    assert_eq!(finished.arrivals(), 1, "the work reached its file write");
    assert_eq!(
        results_root_entries(&server),
        vec![format!("{}.parquet", captured.result.id)],
        "one published file, no stage left"
    );
    let listed = analyst
        .list_results()
        .await
        .expect("list the analyst's results");
    assert_eq!(
        listed
            .results
            .iter()
            .map(|r| r.id.as_str())
            .collect::<Vec<_>>(),
        vec![captured.result.id.as_str()],
        "the row was inserted"
    );
    assert_eq!(pool.available_permits(), permits);
    assert_eq!(pool.retained(), 0);
}
