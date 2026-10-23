// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Capture admission (ADR-0057): a key's queue, the installation's running
//! slots and the key's quota each refuse with 429 and a `Retry-After`,
//! and a refused capture takes nothing. A capture whose client goes away,
//! or whose deadline passes mid-write, keeps its slots and its reservation
//! until its work stops, then leaves nothing behind.
//!
//! Captures are held at the executor pool's seams. A capture held while
//! another is refused reads a recorded report (`| from saved`), which
//! takes no publication guard, so [`captured::assert_nothing_held`] can
//! prove the refusal took neither an executor permit nor the guard.
//!
//! A refused capture's timing account (ADR-0046) is proved here too, from
//! the process's tracing events. The binary is in the ADR-0021 `postgres`
//! nextest group.

mod common;

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use common::TestServer;
use common::audit_capture::{Capture, Captured};
use common::captured::{self, at};
use reqwest::StatusCode;
use serde_json::json;
use trawl_api::value::Value;
use trawl_config::CaptureConfig;
use trawl_server::config::RateLimitConfig;
use trawl_server::pool::seam::Seam;
use trawl_server::results::ResultsClock;
use trawl_server::store::{FinishOutcome, RunClaim, RunStatus};

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

/// A capture deadline no loaded machine reaches, for a test where the
/// deadline must decide nothing.
const UNHURRIED_SECS: u64 = 300;

/// [`boot`], with `capture`'s deadline and the query timeout both
/// [`UNHURRIED_SECS`]: for a test that holds a seam or a queue while the
/// deadline runs and asserts nothing about the deadline itself.
async fn boot_unhurried(capture: CaptureConfig) -> Arc<TestServer> {
    boot_with_timeout(
        CaptureConfig {
            deadline_secs: UNHURRIED_SECS,
            ..capture
        },
        UNHURRIED_SECS,
    )
    .await
}

/// A fixture over the capture corpus with `capture`, its clock at
/// [`CAPTURED_AT`], and a recorded report for the analyst and the admin.
async fn boot(capture: CaptureConfig) -> Arc<TestServer> {
    boot_with_timeout(capture, common::DEFAULT_TEST_TIMEOUT_SECS).await
}

/// [`boot`], with queries that time out after `timeout_secs`, which also
/// caps every capture's deadline.
async fn boot_with_timeout(capture: CaptureConfig, timeout_secs: u64) -> Arc<TestServer> {
    let (server, data) =
        captured::boot_with_data_timeout(RateLimitConfig::default(), capture, timeout_secs).await;
    server.results_clock.set(at(CAPTURED_AT));
    record_report(&server, &data, &server.analyst_token, "analyst_report").await;
    record_report(&server, &data, &server.admin_token, "admin_report").await;
    Arc::new(server)
}

/// Record a finished run of saved query `name`, owned by `token`'s key,
/// whose file holds two rows, through the stores the scheduler records
/// with.
async fn record_report(server: &TestServer, data: &str, token: &str, name: &str) {
    let client = trawl_client::HttpClient::new_insecure(&server.url, token).unwrap();
    let saved = client
        .create_saved(name, "service=nginx | stats count() by host")
        .await
        .unwrap();
    let schedule = client
        .set_schedule(saved.id, "1h", None, true, Some("since_last"), None)
        .await
        .unwrap();
    let store = &server.state.storage.schedule;
    let RunClaim::Started(run_id) = store
        .claim_run(schedule.id, saved.id, "service=nginx", None, None)
        .await
        .unwrap()
    else {
        panic!("the schedule has no run yet");
    };
    let relative = format!("scheduled/run_{run_id}.parquet");
    std::fs::create_dir_all(Path::new(data).join("scheduled")).unwrap();
    let report = trawl_api::value::QueryResult {
        columns: vec![
            trawl_api::value::Column::new("host", trawl_api::value::ColumnType::String),
            trawl_api::value::Column::new("n", trawl_api::value::ColumnType::Integer),
        ],
        rows: vec![
            vec![Value::String("web-1".into()), Value::Integer(3)],
            vec![Value::String("web-2".into()), Value::Integer(4)],
        ],
    };
    let rows = trawl_engine::executor::Executor::new()
        .unwrap()
        .write_query_result_to_parquet(&report, &Path::new(data).join(&relative))
        .unwrap();
    assert_eq!(
        store
            .finish_run(
                run_id,
                RunStatus::Success,
                1,
                Some(rows),
                None,
                None,
                Some(&relative)
            )
            .await
            .unwrap(),
        FinishOutcome::Persisted
    );
}

fn report_body(name: &str) -> serde_json::Value {
    json!({ "query": format!("| from saved {name}") })
}

fn events_body() -> serde_json::Value {
    json!({
        "query": format!("service={}", captured::SERVICE),
        "window": { "earliest": "2024-01-16T10:00:00Z", "latest": "2024-01-16T10:01:00Z" },
    })
}

/// Start a capture on a task of its own.
fn spawn_capture(
    server: &Arc<TestServer>,
    token: &str,
    body: serde_json::Value,
) -> tokio::task::JoinHandle<captured::Answer> {
    let server = Arc::clone(server);
    let token = token.to_owned();
    tokio::spawn(async move { captured::capture(&server, &token, &body).await })
}

/// Wait up to a minute for `done`, failing by `what`. A loaded machine has
/// run these fixtures twenty times slower than an idle one.
async fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let give_up = std::time::Instant::now() + Duration::from_mins(1);
    while !done() {
        assert!(
            std::time::Instant::now() < give_up,
            "waited a minute for {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// `answer` is a 429 with `code` and a `Retry-After` of whole seconds;
/// returns them.
fn retry_after(answer: &captured::Answer, code: &str, what: &str) -> u64 {
    let (status, headers, body) = answer;
    assert_eq!(*status, StatusCode::TOO_MANY_REQUESTS, "{what}: {body}");
    assert_eq!(body["error"]["code"], code, "{what}: {body}");
    headers
        .get("retry-after")
        .unwrap_or_else(|| panic!("{what}: no Retry-After"))
        .to_str()
        .unwrap()
        .parse()
        .unwrap_or_else(|e| panic!("{what}: Retry-After is not whole seconds: {e}"))
}

fn request_id(answer: &captured::Answer) -> String {
    answer.1["x-request-id"].to_str().unwrap().to_owned()
}

/// The one `query_timing` event of request `request_id`.
async fn timing_of(request_id: &str) -> Captured {
    let events = || telemetry().of_type("query_timing", "request_id", request_id);
    wait_until("the refused capture's timing event", || {
        !events().is_empty()
    })
    .await;
    // A second event would land with the first: give it the time to.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let events = events();
    assert_eq!(events.len(), 1, "one account per capture: {events:?}");
    events.into_iter().next().unwrap()
}

/// With the key's one running capture held, four more of its captures
/// wait and the fifth is refused at once with `capture_busy`, taking no
/// permit and no guard. Released, the five all finish.
///
/// The queued captures take their turns one after another, each inside
/// its own deadline, and a queued capture whose deadline passes is refused
/// as busy too. A loaded machine has taken longer than the fixture's ten
/// seconds over the five, so every capture here gets [`UNHURRIED_SECS`]
/// and only the queue decides the answers.
#[tokio::test(flavor = "multi_thread")]
async fn a_full_key_queue_refuses_the_next_capture() {
    let server = boot_unhurried(CaptureConfig::default()).await;
    let key = captured::key_id(&server, &server.analyst_token).await;
    let gate = &server.state.results.gate;
    let seams = server.state.query.pool.seams();
    let started = seams.hold(Seam::Started);
    let analyst = server.analyst_token.clone();

    let mut admitted = vec![spawn_capture(
        &server,
        &analyst,
        report_body("analyst_report"),
    )];
    wait_until("the first capture to start", || started.arrivals() == 1).await;
    for _ in 0..4 {
        admitted.push(spawn_capture(
            &server,
            &analyst,
            report_body("analyst_report"),
        ));
    }
    wait_until("four captures to queue", || gate.queued(key) == 4).await;
    let permits = server.state.query.pool.available_permits();

    let answer = captured::capture(&server, &analyst, &report_body("analyst_report")).await;
    let after = retry_after(&answer, "capture_busy", "the fifth waiting capture");
    assert!(after >= 1, "Retry-After {after}");
    captured::assert_nothing_held(&server, permits, "a capture refused by its queue").await;
    assert_eq!(gate.queued(key), 4, "the refused capture never queued");
    assert_eq!(
        started.arrivals(),
        1,
        "the queue holds the rest before the pool"
    );

    started.release();
    for capture in admitted {
        let (status, _, body) = capture.await.unwrap();
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    assert_eq!(captured::listed(&server, &analyst).await.len(), 5);
}

/// With two keys each running a capture, the installation's two slots are
/// taken and a third key is refused at once with `capture_busy`, taking
/// no permit and no guard. The two running captures are held while the
/// refusal is made, so they get [`UNHURRIED_SECS`].
#[tokio::test(flavor = "multi_thread")]
async fn a_full_installation_refuses_a_third_key() {
    let server = boot_unhurried(CaptureConfig::default()).await;
    let seams = server.state.query.pool.seams();
    let started = seams.hold(Seam::Started);
    let running = [
        spawn_capture(
            &server,
            &server.analyst_token,
            report_body("analyst_report"),
        ),
        spawn_capture(&server, &server.admin_token, report_body("admin_report")),
    ];
    wait_until("two keys' captures to start", || started.arrivals() == 2).await;
    let permits = server.state.query.pool.available_permits();

    let answer = captured::capture(&server, &server.reader_token, &events_body()).await;
    let after = retry_after(&answer, "capture_busy", "a third key's capture");
    assert!(after >= 1, "Retry-After {after}");
    captured::assert_nothing_held(&server, permits, "a capture refused by the installation").await;
    assert_eq!(started.arrivals(), 2);

    started.release();
    for capture in running {
        let (status, _, body) = capture.await.unwrap();
        assert_eq!(status, StatusCode::OK, "{body}");
    }
}

/// A key holding `per_key_results` captures is refused with
/// `capture_quota_exceeded`, and `Retry-After` is the seconds until its
/// earliest capture expires on the fixture clock.
#[tokio::test(flavor = "multi_thread")]
async fn a_full_key_quota_names_its_earliest_expiry() {
    let server = boot(CaptureConfig {
        per_key_results: 2,
        ..CaptureConfig::default()
    })
    .await;
    let analyst = &server.analyst_token;
    for clock in ["2024-01-16T10:30:00Z", "2024-01-16T10:35:00Z"] {
        server.results_clock.set(at(clock));
        captured::captured(&server, analyst, &events_body()).await;
    }
    server.results_clock.set(at("2024-01-16T10:40:00Z"));
    let permits = server.state.query.pool.available_permits();

    let answer = captured::capture(&server, analyst, &events_body()).await;
    let after = retry_after(&answer, "capture_quota_exceeded", "a third capture");
    // The 10:30 capture expires at 10:45, five minutes from now.
    assert_eq!(after, 300);
    captured::assert_nothing_held(&server, permits, "a capture refused by its quota").await;
    assert_eq!(captured::listed(&server, analyst).await.len(), 2);

    // Another key's quota is its own.
    captured::captured(&server, &server.reader_token, &events_body()).await;
}

/// A capture whose client goes away while its work is held keeps its lane
/// slot, its installation slot and its quota reservation until the work
/// stops. Then all three are free, no row and no file remain, and the
/// key's next capture is admitted at once. The work is held while the
/// refusals are made, so the captures get [`UNHURRIED_SECS`].
#[tokio::test(flavor = "multi_thread")]
async fn an_abandoned_capture_stays_charged_until_its_work_stops() {
    let server = boot_unhurried(CaptureConfig {
        per_key_results: 1,
        installation_running: 1,
        ..CaptureConfig::default()
    })
    .await;
    let key = captured::key_id(&server, &server.analyst_token).await;
    let pool = &server.state.query.pool;
    let permits = pool.available_permits();
    let seams = pool.seams();
    let started = seams.hold(Seam::Started);
    let analyst = server.analyst_token.clone();

    let abandoned = spawn_capture(&server, &analyst, report_body("analyst_report"));
    wait_until("the capture to start", || started.arrivals() == 1).await;
    abandoned.abort();
    assert!(abandoned.await.unwrap_err().is_cancelled());
    wait_until("the server to retain the abandoned work", || {
        pool.retained() == 1
    })
    .await;

    // Still charged: the key's reservation fills its quota, and the
    // installation's one slot is taken.
    let answer = captured::capture(&server, &analyst, &report_body("analyst_report")).await;
    retry_after(
        &answer,
        "capture_quota_exceeded",
        "the key while it is held",
    );
    let answer = captured::capture(&server, &server.reader_token, &events_body()).await;
    retry_after(&answer, "capture_busy", "another key while it is held");
    assert_eq!(pool.available_permits(), permits - 1, "its permit is held");

    started.release();
    wait_until("the abandoned work to stop", || pool.retained() == 0).await;
    captured::assert_nothing_held(&server, permits, "an abandoned capture").await;
    assert!(captured::root_names(&server).is_empty(), "no file");
    assert!(
        captured::listed(&server, &analyst).await.is_empty(),
        "no row"
    );

    let answer = captured::capture(&server, &analyst, &report_body("analyst_report")).await;
    assert_eq!(answer.0, StatusCode::OK, "admitted at once: {}", answer.2);
    assert_eq!(server.state.results.gate.queued(key), 0);
    assert_eq!(started.arrivals(), 2);
}

/// Hold a capture of the corpus inside its write, its deadline two
/// seconds, until the deadline answers it: 504 with a stage file in the
/// root. `prepare` runs on the staged file's path while the write is held.
/// Then release the write and wait for its work to stop and the root to
/// empty, and check that nothing is held and no row was committed.
async fn past_the_deadline_mid_write(prepare: impl FnOnce(&Path)) {
    let server = boot(CaptureConfig {
        deadline_secs: 2,
        ..CaptureConfig::default()
    })
    .await;
    let pool = &server.state.query.pool;
    let permits = pool.available_permits();
    let seams = pool.seams();
    let writing = seams.hold(Seam::Writing);

    let capture = spawn_capture(&server, &server.analyst_token, events_body());
    wait_until("the capture to stage its file", || writing.arrivals() == 1).await;
    let staged = captured::root_names(&server);
    assert!(
        staged.len() == 1 && staged[0].starts_with(".stage-"),
        "one stage file while the write is held: {staged:?}"
    );
    prepare(&server.state.results.root.path().join(&staged[0]));
    let (status, headers, body) = capture.await.unwrap();
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{body}");
    assert_eq!(body["error"]["code"], "timeout", "{body}");
    assert!(headers.get("x-request-id").is_some());

    writing.release();
    wait_until("the held work to stop", || pool.retained() == 0).await;
    wait_until("the root to empty", || {
        captured::root_names(&server).is_empty()
    })
    .await;
    captured::assert_nothing_held(&server, permits, "a capture past its deadline").await;
    assert!(
        captured::listed(&server, &server.analyst_token)
            .await
            .is_empty(),
        "no row"
    );
}

/// A capture whose deadline passes while its file is half written answers
/// 504. Released, the write finishes and publishes into a capture nobody
/// will commit, and that file goes too: the root ends empty.
#[tokio::test(flavor = "multi_thread")]
async fn a_deadline_mid_write_leaves_nothing_in_the_root() {
    past_the_deadline_mid_write(|_| {}).await;
}

/// The same, when the held write then fails: the stage is removed on the
/// write's error path, not only by a publish.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_that_fails_past_its_deadline_removes_its_stage() {
    // The engine writes the staged file in place: a read-only one refuses
    // the write, while the root still lets the stage be unlinked.
    past_the_deadline_mid_write(|stage| {
        let mut permissions = std::fs::metadata(stage).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(stage, permissions).unwrap();
    })
    .await;
}

/// A refused capture still accounts its timing, once (ADR-0046): a busy
/// refusal as `capacity_refused` with its admission wait booked, a bounds
/// refusal as `error` with its DSL check booked, neither with work
/// started. The first capture is held while the refusals are made, so it
/// gets [`UNHURRIED_SECS`].
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_capture_accounts_its_timing_once() {
    telemetry();
    let server = boot_unhurried(CaptureConfig {
        installation_running: 1,
        ..CaptureConfig::default()
    })
    .await;
    let seams = server.state.query.pool.seams();
    let started = seams.hold(Seam::Started);
    let running = spawn_capture(
        &server,
        &server.analyst_token,
        report_body("analyst_report"),
    );
    wait_until("the capture to start", || started.arrivals() == 1).await;

    let busy = captured::capture(&server, &server.reader_token, &events_body()).await;
    retry_after(&busy, "capture_busy", "a second key");
    let timing = timing_of(&request_id(&busy)).await;
    assert!(timing.field("kind").contains("capture"), "{timing:?}");
    assert!(
        timing.field("outcome").contains("capacity_refused"),
        "{timing:?}"
    );
    assert_eq!(timing.field("work_started"), "false", "{timing:?}");
    timing.field("query_dsl_check_us");
    timing.field("query_pool_wait_us");

    let body = json!({
        "query": format!("service={}", captured::SERVICE),
        "window": { "last": "8d" },
    });
    let bounds = captured::capture(&server, &server.reader_token, &body).await;
    assert_eq!(bounds.0, StatusCode::BAD_REQUEST, "{}", bounds.2);
    let timing = timing_of(&request_id(&bounds)).await;
    assert!(timing.field("kind").contains("capture"), "{timing:?}");
    assert!(timing.field("outcome").contains("error"), "{timing:?}");
    assert_eq!(timing.field("work_started"), "false", "{timing:?}");
    timing.field("query_dsl_check_us");
    assert!(
        !timing.fields.contains_key("query_pool_wait_us"),
        "a bounds refusal never reached admission: {timing:?}"
    );

    started.release();
    let (status, _, body) = running.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// A capture whose deadline passes while its bounds resolve is refused
/// before admission with 503, as `/query` refuses work its deadline never
/// let start, and accounts as `capacity_refused` with no admission wait.
///
/// Resolution reads the results clock, so a thread holding the fixture
/// clock's lock parks the capture there, its deadline already stamped.
/// The capture blocks its tokio worker meanwhile, as a slow resolution
/// would, and that can stall the runtime's timers. So the holder times
/// the hold on its own thread, and the test's tasks wait only for the
/// answer.
#[tokio::test(flavor = "multi_thread")]
async fn a_deadline_spent_resolving_refuses_before_admission() {
    telemetry();
    let server = boot(CaptureConfig {
        deadline_secs: 1,
        ..CaptureConfig::default()
    })
    .await;
    let key = captured::key_id(&server, &server.analyst_token).await;
    let permits = server.state.query.pool.available_permits();
    let ResultsClock::Fixed(clock) = &server.results_clock else {
        panic!("the fixture's results clock is fixed");
    };
    let clock = Arc::clone(clock);
    let counted = Arc::clone(&server.state.total_queries);
    let entered = counted.load(Ordering::Relaxed);
    let (locked, on_locked) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let _held = clock.lock();
        locked.send(()).unwrap();
        // A capture counts itself once its deadline is stamped, before it
        // resolves; from then on its one second is running.
        let give_up = std::time::Instant::now() + Duration::from_mins(1);
        while counted.load(Ordering::Relaxed) == entered {
            assert!(
                std::time::Instant::now() < give_up,
                "the capture never arrived"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(1_500));
    });
    on_locked.recv().unwrap();

    let capture = spawn_capture(&server, &server.analyst_token, events_body());
    let answer = capture.await.unwrap();
    holder.join().expect("the holder saw the capture arrive");
    let (status, _, body) = &answer;
    assert_eq!(*status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["code"], "service_unavailable", "{body}");
    let timing = timing_of(&request_id(&answer)).await;
    assert!(
        timing.field("outcome").contains("capacity_refused"),
        "{timing:?}"
    );
    assert_eq!(timing.field("work_started"), "false", "{timing:?}");
    timing.field("query_dsl_check_us");
    assert!(
        !timing.fields.contains_key("query_pool_wait_us"),
        "the refusal came before admission: {timing:?}"
    );
    captured::assert_nothing_held(&server, permits, "a capture refused after resolving").await;
    assert_eq!(server.state.results.gate.queued(key), 0);
    assert!(
        captured::listed(&server, &server.analyst_token)
            .await
            .is_empty(),
        "no row"
    );
}
