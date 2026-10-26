// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A capture attached to an investigation (ADR-0059) appends its step at
//! admission and writes the step's terminal outcome when its work ends,
//! through the fixture's HTTPS listener with real fleet keys: an agent key
//! that writes (`query`, `investigation_read`, `investigation_write`) and a
//! human key that reads (`query`, `investigation_read`). One test per
//! outcome: success, refused, timeout and a client abort; a plan refusal
//! that records nothing; the authority, the investigation's state and the
//! purpose's bound.
//!
//! Captures resolve their window at the fixture's results clock, set onto
//! the capture corpus of `common::captured`, 2024-01-16. The process's
//! tracing events are recorded, so each test runs in its own process:
//! nextest's. The binary is in the ADR-0021 `postgres` nextest group.

mod common;

#[path = "request_limit/support.rs"]
mod support;

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use common::audit_capture::Capture;
use common::captured::{self, SLOW_PATTERN, SLOW_SERVICE, at};
use common::investigating::{
    INVESTIGATION_LOCK, attached, call, code, create, hold, hold_result, id, lock_waiters,
    lock_waits, message, outcome, read, routed, settled, steps, until_admitted,
};
use common::{MintedKey, TestServer, mint_key, trawl_perms};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use trawl_api::ErrorCode;
use trawl_api::investigations::{EntryPayload, StepOutcomeKind};
use trawl_config::CaptureConfig;
use trawl_server::config::RateLimitConfig;
use trawl_server::pool::seam::Seam;

/// Where the clock stands when a test captures: half an hour into the
/// capture corpus's hour.
const START: &str = "2024-01-16T10:30:00Z";

/// The two principals.
struct Keys {
    /// Reads and writes.
    agent: MintedKey,
    /// Reads only.
    human: MintedKey,
}

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

async fn keys(server: &TestServer) -> Keys {
    let pool = &server.fleet_pool;
    Keys {
        agent: mint_key(
            pool,
            "agent",
            &["query", "investigation_read", "investigation_write"],
        )
        .await,
        human: mint_key(pool, "human", &["query", "investigation_read"]).await,
    }
}

/// A fixture over the capture corpus with `capture`, its clock at
/// [`START`].
async fn boot(capture: CaptureConfig) -> (Arc<TestServer>, Keys) {
    let _ = telemetry();
    let server = captured::boot(RateLimitConfig::default(), capture).await;
    server.results_clock.set(at(START));
    let keys = keys(&server).await;
    (Arc::new(server), keys)
}

/// [`boot`] with the slow corpus beside the capture corpus, captures
/// under `deadline_secs` and queries under a minute.
async fn boot_slow(deadline_secs: u64) -> (Arc<TestServer>, Keys) {
    let _ = telemetry();
    let tmp = tempfile::tempdir().expect("create a temp dir");
    let data = common::seed_data_root(tmp.path());
    captured::seed_hour(&data, captured::SERVICE, &captured::corpus());
    captured::slow_corpus(&data);
    let server = common::setup_with_capture_timeout(
        tmp.path(),
        data,
        RateLimitConfig::default(),
        CaptureConfig {
            deadline_secs,
            ..CaptureConfig::default()
        },
        60,
    )
    .await;
    std::mem::forget(tmp);
    server.results_clock.set(at(START));
    let keys = keys(&server).await;
    (Arc::new(server), keys)
}

/// The capture corpus's 20 events.
fn corpus_query() -> String {
    format!("service={}", captured::SERVICE)
}

/// The slow capture: seconds of `DuckDB` execution, no row captured.
fn slow_query() -> String {
    format!("service={SLOW_SERVICE} message=/{SLOW_PATTERN}/")
}

/// Wait up to a minute for `done`, failing by `what`.
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

/// The investigation's sequence and entry count, read as the human.
async fn seq_and_entries(server: &TestServer, keys: &Keys, n: u32) -> (u64, usize) {
    let page = read(server, &keys.human, n, "").await;
    (page.investigation.seq, page.entries.len())
}

/// Start a capture of `body` as `token` on a task of its own.
fn spawn_capture(
    server: &Arc<TestServer>,
    token: &str,
    body: Value,
) -> tokio::task::JoinHandle<captured::Answer> {
    let server = Arc::clone(server);
    let token = token.to_owned();
    tokio::spawn(async move { captured::capture(&server, &token, &body).await })
}

/// The `query_start` events of query `query_id`.
fn query_starts(query_id: u64) -> usize {
    telemetry()
        .events()
        .into_iter()
        .filter(|event| {
            event.fields.get("event_type").map(String::as_str) == Some("\"query_start\"")
                && event.fields.get("query_id") == Some(&query_id.to_string())
        })
        .count()
}

/// A capture is admitted with its step already recorded: while the work
/// is held, the human reads the step with the executed text, its bounds,
/// the request and query ids and the purpose, and no outcome. Released,
/// the step reads `success` with the result, its rows and the duration,
/// at a later `updated_seq`.
#[tokio::test(flavor = "multi_thread")]
async fn an_attached_capture_appends_its_step_at_admission_and_records_success() {
    let (server, keys) = boot(CaptureConfig::default()).await;
    create(&server, &keys.agent, 1).await;
    let seams = server.state.query.pool.seams();
    let started = seams.hold(Seam::Started);

    let capturing = spawn_capture(
        &server,
        &keys.agent.token,
        attached(
            &corpus_query(),
            1,
            &json!({ "purpose": "find the first 502" }),
        ),
    );
    wait_until("the attached capture's work to start", || {
        started.arrivals() == 1
    })
    .await;

    let running = steps(&server, &keys.human, 1).await;
    assert_eq!(running.len(), 1, "{running:?}");
    let step = &running[0];
    assert_eq!((step.seq, step.updated_seq), (1, 1));
    assert_eq!(step.actor.prefix, keys.agent.prefix);
    let EntryPayload::Step {
        executed,
        bounds,
        request_id,
        query_id,
        purpose,
        outcome: None,
    } = &step.payload
    else {
        panic!("a step with no outcome yet: {step:?}");
    };
    assert!(
        executed.starts_with(
            "earliest=\"2024-01-16T09:30:00.000000Z\" latest=\"2024-01-16T10:30:00.000000Z\" "
        ) && executed.contains(&corpus_query()),
        "the executed text, with the window spliced in: {executed}"
    );
    let bounds = bounds.as_ref().expect("a live window has bounds");
    assert_eq!(
        (bounds.earliest.as_str(), bounds.latest.as_str()),
        ("2024-01-16T09:30:00.000000Z", "2024-01-16T10:30:00.000000Z")
    );
    assert_eq!(purpose.as_deref(), Some("find the first 502"));
    let query_id = query_id.expect("the step names its query");
    let request_id = request_id.clone();

    started.release();
    let (status, headers, body) = capturing.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        headers["x-request-id"].to_str().unwrap(),
        request_id,
        "the step names the request that admitted it"
    );
    let result_id = body["result"]["id"].as_str().unwrap().to_owned();
    assert!(
        body.get("activity").is_none(),
        "no seen_seq, no activity: {body}"
    );

    let done = settled(&server, &keys.human, 1, 1).await;
    let outcome = outcome(&done).unwrap();
    assert_eq!(outcome.outcome, StepOutcomeKind::Success, "{outcome:?}");
    assert_eq!(outcome.code, None);
    assert_eq!(outcome.result_id.as_deref(), Some(result_id.as_str()));
    assert_eq!(outcome.rows, Some(captured::ROWS as u64));
    assert!(outcome.duration_ms.is_some(), "{outcome:?}");
    assert!(done.updated_seq > done.seq, "{done:?}");
    let EntryPayload::Step {
        query_id: recorded, ..
    } = &done.payload
    else {
        unreachable!()
    };
    assert_eq!(*recorded, Some(query_id));
    assert_eq!(query_starts(query_id), 1);
}

/// A capture refused at admission still has its step, and the step reads
/// `refused` with the refusal's code: the installation's one running slot
/// is the human's, so the agent's attached capture is 429 `capture_busy`.
/// The refusal took no permit and never started a query. (The held capture
/// reads a window, so it holds the publication guard itself.)
#[tokio::test(flavor = "multi_thread")]
async fn an_attached_capture_refused_busy_records_refused_with_its_code() {
    let (server, keys) = boot(CaptureConfig {
        installation_running: 1,
        deadline_secs: 300,
        ..CaptureConfig::default()
    })
    .await;
    create(&server, &keys.agent, 1).await;
    let seams = server.state.query.pool.seams();
    let started = seams.hold(Seam::Started);
    let holder = spawn_capture(
        &server,
        &keys.human.token,
        json!({ "query": corpus_query(), "window": { "last": "1h" } }),
    );
    wait_until("the human's capture to start", || started.arrivals() == 1).await;
    let permits = server.state.query.pool.available_permits();

    let (status, _, body) = captured::capture(
        &server,
        &keys.agent.token,
        &attached(&corpus_query(), 1, &json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(code(&body), "capture_busy", "{body}");
    assert_eq!(
        server.state.query.pool.available_permits(),
        permits,
        "the refusal took no permit"
    );

    let step = settled(&server, &keys.human, 1, 1).await;
    let recorded = outcome(&step).unwrap();
    assert_eq!(recorded.outcome, StepOutcomeKind::Refused, "{recorded:?}");
    assert_eq!(recorded.code, Some(ErrorCode::CaptureBusy));
    assert_eq!(recorded.result_id, None);
    let EntryPayload::Step { query_id, .. } = &step.payload else {
        unreachable!()
    };
    assert_eq!(
        query_starts(query_id.expect("the step names its query")),
        0,
        "a refused capture never starts its query"
    );
    assert_eq!(server.state.query.pool.available_permits(), permits);

    started.release();
    let (status, _, body) = holder.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// A capture that runs past its deadline answers 504, and its step reads
/// `timeout` once the work stops.
#[tokio::test(flavor = "multi_thread")]
async fn an_attached_capture_past_its_deadline_records_timeout() {
    let (server, keys) = boot_slow(1).await;
    create(&server, &keys.agent, 1).await;

    let (status, _, body) = captured::capture(
        &server,
        &keys.agent.token,
        &attached(&slow_query(), 1, &json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT, "{body}");

    let step = settled(&server, &keys.human, 1, 1).await;
    let recorded = outcome(&step).unwrap();
    assert_eq!(recorded.outcome, StepOutcomeKind::Timeout, "{recorded:?}");
    assert_eq!(recorded.code, Some(ErrorCode::Timeout));
    wait_until("the timed-out work to stop", || {
        server.state.query.pool.retained() == 0
    })
    .await;
    assert!(
        captured::listed(&server, &keys.agent.token)
            .await
            .is_empty()
    );
}

/// A capture whose client goes away while its work runs records `aborted`
/// once its work stops, and leaves no row and no file. The work is held
/// at its start, so the drop lands while it runs by construction; the
/// release lets the latched cancellation stop it before the engine
/// executes. `results_abort.rs` drops a capture inside `DuckDB`'s
/// execution itself.
#[tokio::test(flavor = "multi_thread")]
async fn an_attached_capture_whose_client_goes_away_records_aborted() {
    let (server, keys) = boot_slow(60).await;
    create(&server, &keys.agent, 1).await;
    let pool = &server.state.query.pool;
    let seams = pool.seams();
    let started = seams.hold(Seam::Started);

    let dropped = spawn_capture(
        &server,
        &keys.agent.token,
        attached(&slow_query(), 1, &json!({})),
    );
    wait_until("the capture's work to start", || started.arrivals() == 1).await;
    assert!(!dropped.is_finished(), "the capture is in flight");
    let running = steps(&server, &keys.human, 1).await;
    assert_eq!(outcome(&running[0]), None, "the step is running");
    dropped.abort();
    assert!(dropped.await.unwrap_err().is_cancelled());
    wait_until("the dropped request's work to be retained", || {
        pool.retained() == 1
    })
    .await;
    started.release();

    let step = settled(&server, &keys.human, 1, 1).await;
    let recorded = outcome(&step).unwrap();
    assert_eq!(recorded.outcome, StepOutcomeKind::Aborted, "{recorded:?}");
    assert_eq!(recorded.result_id, None);
    wait_until("the abandoned work to stop", || pool.retained() == 0).await;
    assert!(
        captured::listed(&server, &keys.agent.token)
            .await
            .is_empty()
    );
    assert_eq!(captured::root_names(&server), Vec::<String>::new());
}

/// `key` as the server verifies it, for a capture called past the
/// transport, whose future a test can drop at a chosen await.
async fn verified(server: &TestServer, key: &MintedKey) -> fleet_auth::VerifiedKey {
    fleet_auth::KeyStore::from_pool(server.fleet_pool.clone())
        .verify_key(&key.token)
        .await
        .expect("a fixture token verifies")
}

/// Start the capture service on `body` as `verified`, on a task of its
/// own, past the transport.
fn spawn_service_capture(
    server: &Arc<TestServer>,
    verified: fleet_auth::VerifiedKey,
    body: &Value,
) -> tokio::task::JoinHandle<Result<trawl_api::results::CaptureResponse, String>> {
    let server = Arc::clone(server);
    let request = serde_json::from_value(body.clone()).expect("a capture request");
    tokio::spawn(async move {
        trawl_server::results::service::capture(
            &server.state,
            &verified,
            "01JBTESTREQUEST0000000000".into(),
            request,
        )
        .await
        .map_err(|e| e.to_string())
    })
}

/// A capture whose request goes away while its step's admission waits on
/// the investigation's lock still appends the step, and the admission,
/// which outlived the request, records it `aborted` at once: the request
/// that would have run the work is gone, and nothing ran.
#[tokio::test(flavor = "multi_thread")]
async fn a_capture_dropped_during_its_steps_admission_records_aborted() {
    let (server, keys) = boot(CaptureConfig::default()).await;
    create(&server, &keys.agent, 1).await;
    let agent = verified(&server, &keys.agent).await;

    let mut held = hold(&server, 1).await;
    let request = spawn_service_capture(&server, agent, &attached(&corpus_query(), 1, &json!({})));
    lock_waits(&server, INVESTIGATION_LOCK, 1).await;
    request.abort();
    assert!(
        request.await.unwrap_err().is_cancelled(),
        "the request's future was dropped during the admission"
    );
    sqlx::query("COMMIT").execute(&mut held).await.unwrap();

    let step = settled(&server, &keys.human, 1, 1).await;
    let recorded = outcome(&step).unwrap();
    assert_eq!(recorded.outcome, StepOutcomeKind::Aborted, "{recorded:?}");
    let EntryPayload::Step { query_id, .. } = &step.payload else {
        unreachable!()
    };
    assert_eq!(
        query_starts(query_id.expect("the step names its query")),
        0,
        "the dropped capture never started its query"
    );
    assert_eq!(seq_and_entries(&server, &keys, 1).await, (2, 1));
}

/// The step admission that outlives its request keeps the request's count
/// (ADR-0054) until the admission and its `aborted` write end: with a
/// count of one, a request is refused while the admission waits on the
/// investigation's lock, and admitted once the step has settled.
#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_captures_step_admission_keeps_its_request_count() {
    let (server, keys) = boot(CaptureConfig::default()).await;
    create(&server, &keys.agent, 1).await;
    let app = server.router_with(|http| http.max_concurrent_requests = 1);

    let mut held = hold(&server, 1).await;
    let request = support::send(
        &app,
        routed(
            &keys.agent.token,
            "POST",
            "/results",
            Some(&attached(&corpus_query(), 1, &json!({}))),
        ),
    );
    lock_waits(&server, INVESTIGATION_LOCK, 1).await;
    request.abort();
    assert!(
        request.await.unwrap_err().is_cancelled(),
        "the request's future was dropped during the admission"
    );
    support::assert_refused(
        support::call(
            &app,
            routed(&keys.agent.token, "GET", "/investigations", None),
        )
        .await,
        support::REGULAR_MESSAGE,
    )
    .await;

    sqlx::query("COMMIT").execute(&mut held).await.unwrap();
    let step = settled(&server, &keys.human, 1, 1).await;
    let recorded = outcome(&step).unwrap();
    assert_eq!(recorded.outcome, StepOutcomeKind::Aborted, "{recorded:?}");
    until_admitted(&app, &keys.agent.token).await;
}

/// A step admission waits for its investigation no later than the
/// capture's deadline. A capture whose admission reaches it answers 429
/// `capture_busy` with a retry of one second and appends no step, and by
/// then no session waits on the lock: Postgres ends its wait at the
/// deadline too, under its own statement timeout. One whose request went
/// away frees the request's count at the deadline, on its lease, while
/// the lock is still held, and Postgres has stopped waiting within a
/// floor of it.
#[tokio::test(flavor = "multi_thread")]
async fn a_step_admission_waits_for_its_investigation_no_later_than_the_capture_deadline() {
    // Long enough for the test to see the second admission wait and drop
    // its request before the deadline ends the wait.
    let (server, keys) = boot(CaptureConfig {
        deadline_secs: 3,
        ..CaptureConfig::default()
    })
    .await;
    create(&server, &keys.agent, 1).await;
    let held = hold(&server, 1).await;

    let (status, headers, body) = tokio::time::timeout(
        support::DEADLINE,
        captured::capture(
            &server,
            &keys.agent.token,
            &attached(&corpus_query(), 1, &json!({})),
        ),
    )
    .await
    .expect("the capture answers by its deadline, with the lock still held");
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(code(&body), "capture_busy", "{body}");
    assert_eq!(
        headers.get("retry-after").and_then(|v| v.to_str().ok()),
        Some("1")
    );
    assert_eq!(
        lock_waiters(&server, INVESTIGATION_LOCK).await,
        0,
        "the answer came while the admission still waited on the investigation"
    );

    let app = server.router_with(|http| http.max_concurrent_requests = 1);
    let sent = tokio::time::Instant::now();
    let request = support::send(
        &app,
        routed(
            &keys.agent.token,
            "POST",
            "/results",
            Some(&attached(&corpus_query(), 1, &json!({}))),
        ),
    );
    lock_waits(&server, INVESTIGATION_LOCK, 1).await;
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    until_admitted(&app, &keys.agent.token).await;
    assert!(
        sent.elapsed() >= Duration::from_secs(3),
        "the request's count came back {:?} after the capture began, before its deadline",
        sent.elapsed()
    );
    tokio::time::timeout(FLOOR, lock_waits(&server, INVESTIGATION_LOCK, 0))
        .await
        .expect("Postgres still waited on the investigation a floor after the count came back");

    drop(held);
    assert_eq!(
        seq_and_entries(&server, &keys, 1).await,
        (0, 0),
        "neither admission appended a step"
    );
}

/// A capture whose request goes away while its step's admission unlinks
/// the evidence the due-write rule pruned records the step `aborted` once
/// the unlink ends, and the file is gone: the admission builds the step's
/// completion itself and finishes without the request.
#[tokio::test(flavor = "multi_thread")]
async fn a_capture_dropped_during_its_steps_unlink_records_aborted() {
    // Unmetered: the step is polled until it settles.
    let _ = telemetry();
    let server = captured::boot(
        RateLimitConfig {
            default_rpm: 0,
            ..RateLimitConfig::default()
        },
        CaptureConfig::default(),
    )
    .await;
    server.results_clock.set(at(START));
    let keys = keys(&server).await;
    let server = Arc::new(server);
    create(&server, &keys.agent, 1).await;
    let kept = captured::captured(
        &server,
        &keys.agent.token,
        &attached(&corpus_query(), 1, &json!({})),
    )
    .await
    .result;
    settled(&server, &keys.human, 1, 1).await;
    assert_eq!(
        captured::root_names(&server),
        [format!("{}.parquet", kept.id)]
    );
    // A week and a day on, the evidence's retention has ended: the next
    // write prunes it first and unlinks its file.
    server
        .results_clock
        .set(at(START) + chrono::TimeDelta::days(8));
    let agent = verified(&server, &keys.agent).await;

    let hold = server.state.results.root.hold_next_unlink();
    let request = spawn_service_capture(&server, agent, &attached(&corpus_query(), 1, &json!({})));
    tokio::time::timeout(Duration::from_mins(1), hold.parked)
        .await
        .expect("the admission reaches its unlink")
        .expect("the unlink parks");
    request.abort();
    assert!(
        request.await.unwrap_err().is_cancelled(),
        "the request's future was dropped during the unlink"
    );
    let _ = hold.release.send(());

    let step = settled(&server, &keys.human, 1, 3).await;
    let recorded = outcome(&step).unwrap();
    assert_eq!(recorded.outcome, StepOutcomeKind::Aborted, "{recorded:?}");
    assert_eq!(captured::root_names(&server), Vec::<String>::new());
}

/// The `aborted` write of a step whose request went away keeps the
/// request's count (ADR-0054) no later than the capture's deadline
/// (ADR-0059), and is still written. With a count of one, the request
/// goes away after its step committed, while the admission unlinks pruned
/// evidence, and the investigation's lock is taken before the write: a
/// request is refused while the write waits for the lock, admitted once
/// the deadline passes with the lock still held, and the step reads
/// `aborted` once the lock is released. One line says the write ran past
/// the deadline.
#[tokio::test(flavor = "multi_thread")]
async fn an_abandoned_steps_aborted_write_frees_its_request_count_at_the_capture_deadline() {
    // Unmetered: the step is polled until it settles. Long enough for the
    // test to see the write wait inside the count before the deadline.
    let _ = telemetry();
    let server = captured::boot(
        RateLimitConfig {
            default_rpm: 0,
            ..RateLimitConfig::default()
        },
        CaptureConfig {
            deadline_secs: 5,
            ..CaptureConfig::default()
        },
    )
    .await;
    server.results_clock.set(at(START));
    let keys = keys(&server).await;
    let server = Arc::new(server);
    create(&server, &keys.agent, 1).await;
    captured::captured(
        &server,
        &keys.agent.token,
        &attached(&corpus_query(), 1, &json!({})),
    )
    .await;
    settled(&server, &keys.human, 1, 1).await;
    // A week and a day on, the evidence's retention has ended: the next
    // write prunes it first and unlinks its file, and the unlink holds.
    server
        .results_clock
        .set(at(START) + chrono::TimeDelta::days(8));
    let app = server.router_with(|http| http.max_concurrent_requests = 1);

    let unlink = server.state.results.root.hold_next_unlink();
    let request = support::send(
        &app,
        routed(
            &keys.agent.token,
            "POST",
            "/results",
            Some(&attached(&corpus_query(), 1, &json!({}))),
        ),
    );
    tokio::time::timeout(Duration::from_mins(1), unlink.parked)
        .await
        .expect("the admission reaches its unlink")
        .expect("the unlink parks");
    let mut held = hold(&server, 1).await;
    request.abort();
    assert!(
        request.await.unwrap_err().is_cancelled(),
        "the request's future was dropped during the unlink"
    );
    let _ = unlink.release.send(());
    lock_waits(&server, INVESTIGATION_LOCK, 1).await;
    support::assert_refused(
        support::call(
            &app,
            routed(&keys.agent.token, "GET", "/investigations", None),
        )
        .await,
        support::REGULAR_MESSAGE,
    )
    .await;

    until_admitted(&app, &keys.agent.token).await;
    lock_waits(&server, INVESTIGATION_LOCK, 1).await;
    let past_deadline = || {
        telemetry()
            .events()
            .into_iter()
            .filter(|event| {
                event.fields.get("event_type").map(String::as_str)
                    == Some("\"investigation_step_past_deadline\"")
            })
            .count()
    };
    assert_eq!(past_deadline(), 1, "one line says the write ran past");

    sqlx::query("COMMIT").execute(&mut held).await.unwrap();
    let step = settled(&server, &keys.human, 1, 3).await;
    let recorded = outcome(&step).unwrap();
    assert_eq!(recorded.outcome, StepOutcomeKind::Aborted, "{recorded:?}");
    assert_eq!(past_deadline(), 1);
}

/// The statement the due-write rule prunes an investigation's kept
/// results with.
const EVIDENCE_PRUNE: &str = "UPDATE captured_results AS c%";

/// The statement a write appends its log entry with.
const LOG_INSERT: &str = "INSERT INTO investigation_log%";

/// The statement a step's outcome is written with.
const STEP_FILL: &str = "UPDATE investigation_log%";

/// What a refused capture may take past its deadline: the rollback, the
/// handover and the response.
const FLOOR: Duration = Duration::from_secs(1);

/// [`boot`], unmetered so a step can be polled until it settles, with
/// captures under `deadline_secs`.
async fn boot_unmetered(deadline_secs: u64) -> (Arc<TestServer>, Keys) {
    boot_unmetered_with(CaptureConfig {
        deadline_secs,
        ..CaptureConfig::default()
    })
    .await
}

/// [`boot`], unmetered so a step can be polled until it settles.
async fn boot_unmetered_with(capture: CaptureConfig) -> (Arc<TestServer>, Keys) {
    let _ = telemetry();
    let server = captured::boot(
        RateLimitConfig {
            default_rpm: 0,
            ..RateLimitConfig::default()
        },
        capture,
    )
    .await;
    server.results_clock.set(at(START));
    let keys = keys(&server).await;
    (Arc::new(server), keys)
}

/// Keep `n` results in investigation 1 through attached captures, each
/// step settled, and answer their ids. The investigation's sequence ends
/// at `2 * n`.
async fn keep_results(server: &Arc<TestServer>, keys: &Keys, n: u64) -> Vec<String> {
    let mut kept = Vec::new();
    for step in 0..n {
        let captured = captured::captured(
            server,
            &keys.agent.token,
            &attached(&corpus_query(), 1, &json!({})),
        )
        .await;
        settled(server, &keys.human, 1, 2 * step + 1).await;
        kept.push(captured.result.id);
    }
    kept
}

/// How many of investigation 1's results are still kept.
async fn still_kept(server: &TestServer) -> i64 {
    let probe = common::fixture_pool(&server.app_db_url, 1).await;
    let kept = sqlx::query_scalar(
        "SELECT count(*) FROM captured_results WHERE investigation_id = $1 AND state = 'kept'",
    )
    .bind(id(1))
    .fetch_one(&probe)
    .await
    .unwrap();
    probe.close().await;
    kept
}

/// The backend of `session`.
async fn backend(session: &mut sqlx::PgConnection) -> i32 {
    sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(session)
        .await
        .unwrap()
}

/// The backend the one session waiting on a lock in a statement like
/// `pattern` waits for.
async fn blocker(server: &TestServer, pattern: &str) -> i32 {
    let probe = common::fixture_pool(&server.app_db_url, 1).await;
    let blocker = sqlx::query_scalar(
        "SELECT (pg_blocking_pids(pid))[1] FROM pg_stat_activity
         WHERE datname = current_database() AND wait_event_type = 'Lock' AND query LIKE $1",
    )
    .bind(pattern)
    .fetch_one(&probe)
    .await
    .unwrap();
    probe.close().await;
    blocker
}

/// A step admission waits no later than the capture's deadline however
/// many locks it waits on in turn (ADR-0059): Postgres bounds every
/// statement of the write by what remains. The due-write rule's prune
/// waits on two kept results, each held by a session of its own: the one
/// it waits on first lets go at three seconds, the other holds past the
/// five-second deadline. The capture answers 429 `capture_busy` by the
/// deadline, and by then no session waits on the prune; its request count
/// is free, and nothing was written: both results are still kept and no
/// step was appended.
#[tokio::test(flavor = "multi_thread")]
async fn a_step_admission_waiting_on_one_result_after_another_answers_by_the_capture_deadline() {
    let deadline = Duration::from_secs(5);
    let (server, keys) = boot_unmetered(deadline.as_secs()).await;
    create(&server, &keys.agent, 1).await;
    let kept = keep_results(&server, &keys, 2).await;
    // A week and a day on, the evidence's retention has ended: the next
    // write prunes both results first.
    server
        .results_clock
        .set(at(START) + chrono::TimeDelta::days(8));
    let app = server.router_with(|http| http.max_concurrent_requests = 1);
    let mut first = hold_result(&server, &kept[0]).await;
    let second = hold_result(&server, &kept[1]).await;

    let sent = tokio::time::Instant::now();
    let request = support::send(
        &app,
        routed(
            &keys.agent.token,
            "POST",
            "/results",
            Some(&attached(&corpus_query(), 1, &json!({}))),
        ),
    );
    lock_waits(&server, EVIDENCE_PRUNE, 1).await;
    tokio::time::sleep_until(sent + Duration::from_secs(3)).await;
    let waited_on = blocker(&server, EVIDENCE_PRUNE).await;
    let (mut released, mut past) = if waited_on == backend(&mut first).await {
        (first, second)
    } else {
        (second, first)
    };
    sqlx::query("COMMIT").execute(&mut released).await.unwrap();

    let response = support::finish(request).await;
    let answered = sent.elapsed();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let body = support::body_json(response).await;
    assert_eq!(code(&body), "capture_busy", "{body}");
    assert!(
        answered < deadline + FLOOR,
        "answered {answered:?} after the capture began, past its {deadline:?} deadline"
    );
    assert_eq!(
        lock_waiters(&server, EVIDENCE_PRUNE).await,
        0,
        "the answer came while the admission still waited on a result"
    );
    until_admitted(&app, &keys.agent.token).await;

    sqlx::query("COMMIT").execute(&mut past).await.unwrap();
    assert_eq!(still_kept(&server).await, 2, "the prune was rolled back");
    assert_eq!(
        seq_and_entries(&server, &keys, 1).await,
        (4, 2),
        "the admission appended no step"
    );
}

/// The statement after a wait gets only what remains of the capture's
/// deadline (ADR-0059). The due-write rule's prune waits three seconds on
/// a kept result another session holds, with the investigation's lock
/// free; then the step's log insert waits on a session holding the log
/// table in `EXCLUSIVE` mode. The capture answers 429 `capture_busy` by
/// its five-second deadline, and by then no session waits on the insert;
/// its request count is free, and nothing was written.
#[tokio::test(flavor = "multi_thread")]
async fn a_step_admissions_log_insert_waits_only_what_remains_of_the_capture_deadline() {
    use sqlx::Connection as _;
    let deadline = Duration::from_secs(5);
    let (server, keys) = boot_unmetered(deadline.as_secs()).await;
    create(&server, &keys.agent, 1).await;
    let kept = keep_results(&server, &keys, 1).await;
    server
        .results_clock
        .set(at(START) + chrono::TimeDelta::days(8));
    let app = server.router_with(|http| http.max_concurrent_requests = 1);
    let mut result = hold_result(&server, &kept[0]).await;
    let mut log = sqlx::PgConnection::connect(&server.app_db_url)
        .await
        .unwrap();
    sqlx::query("BEGIN").execute(&mut log).await.unwrap();
    sqlx::query("LOCK TABLE investigation_log IN EXCLUSIVE MODE")
        .execute(&mut log)
        .await
        .unwrap();

    let sent = tokio::time::Instant::now();
    let request = support::send(
        &app,
        routed(
            &keys.agent.token,
            "POST",
            "/results",
            Some(&attached(&corpus_query(), 1, &json!({}))),
        ),
    );
    lock_waits(&server, EVIDENCE_PRUNE, 1).await;
    tokio::time::sleep_until(sent + Duration::from_secs(3)).await;
    sqlx::query("COMMIT").execute(&mut result).await.unwrap();
    lock_waits(&server, LOG_INSERT, 1).await;

    let response = support::finish(request).await;
    let answered = sent.elapsed();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let body = support::body_json(response).await;
    assert_eq!(code(&body), "capture_busy", "{body}");
    assert!(
        answered < deadline + FLOOR,
        "answered {answered:?} after the capture began, past its {deadline:?} deadline"
    );
    assert_eq!(
        lock_waiters(&server, LOG_INSERT).await,
        0,
        "the answer came while the admission still waited on the log"
    );
    until_admitted(&app, &keys.agent.token).await;

    sqlx::query("COMMIT").execute(&mut log).await.unwrap();
    assert_eq!(still_kept(&server).await, 1, "the prune was rolled back");
    assert_eq!(
        seq_and_entries(&server, &keys, 1).await,
        (2, 1),
        "the admission appended no step"
    );
}

/// The `aborted` write of a step whose request went away waits no later
/// than the capture's deadline on the step's own log row too, and past it
/// the outcome is still written, once the row is free (ADR-0059). The
/// request goes away after its step committed, while the admission unlinks
/// pruned evidence, and another session holds the step's row: the write
/// waits on it, one line says it ran past the deadline, the step is still
/// running, and it reads `aborted` once the row is released.
#[tokio::test(flavor = "multi_thread")]
async fn an_abandoned_steps_write_held_on_its_log_row_is_written_once_the_row_is_free() {
    use sqlx::Connection as _;
    let (server, keys) = boot_unmetered(3).await;
    create(&server, &keys.agent, 1).await;
    keep_results(&server, &keys, 1).await;
    server
        .results_clock
        .set(at(START) + chrono::TimeDelta::days(8));
    let agent = verified(&server, &keys.agent).await;

    let unlink = server.state.results.root.hold_next_unlink();
    let request = spawn_service_capture(&server, agent, &attached(&corpus_query(), 1, &json!({})));
    tokio::time::timeout(Duration::from_mins(1), unlink.parked)
        .await
        .expect("the admission reaches its unlink")
        .expect("the unlink parks");
    let mut row = sqlx::PgConnection::connect(&server.app_db_url)
        .await
        .unwrap();
    sqlx::query("BEGIN").execute(&mut row).await.unwrap();
    let locked = sqlx::query(
        "SELECT 1 FROM investigation_log WHERE investigation_id = $1 AND seq = 3 FOR UPDATE",
    )
    .bind(id(1))
    .execute(&mut row)
    .await
    .unwrap();
    assert_eq!(
        locked.rows_affected(),
        1,
        "the step committed before its unlink"
    );
    request.abort();
    assert!(
        request.await.unwrap_err().is_cancelled(),
        "the request's future was dropped during the unlink"
    );
    let _ = unlink.release.send(());
    lock_waits(&server, STEP_FILL, 1).await;

    let past_deadline = || {
        telemetry()
            .events()
            .into_iter()
            .filter(|event| {
                event.fields.get("event_type").map(String::as_str)
                    == Some("\"investigation_step_past_deadline\"")
            })
            .count()
    };
    wait_until("the write to pass its deadline", || past_deadline() == 1).await;
    lock_waits(&server, STEP_FILL, 1).await;
    let running = steps(&server, &keys.human, 1).await;
    assert!(
        outcome(&running[1]).is_none(),
        "the step settled while its row was held: {running:?}"
    );

    sqlx::query("COMMIT").execute(&mut row).await.unwrap();
    let step = settled(&server, &keys.human, 1, 3).await;
    let recorded = outcome(&step).unwrap();
    assert_eq!(recorded.outcome, StepOutcomeKind::Aborted, "{recorded:?}");
    assert_eq!(past_deadline(), 1);
}

/// The request's count an abandoned step's admission keeps goes back at
/// the capture's deadline whatever the admission awaits then (ADR-0059).
/// With a count of one, the request goes away after its step committed,
/// while the admission unlinks pruned evidence, and the unlink stays
/// parked past the deadline: a request is refused before the deadline,
/// one is admitted once it passes with the unlink still parked and the
/// step still running, and the step reads `aborted` once the unlink is
/// released.
#[tokio::test(flavor = "multi_thread")]
async fn an_abandoned_admissions_request_count_goes_back_at_the_capture_deadline_whatever_it_awaits()
 {
    let deadline = Duration::from_secs(3);
    let (server, keys) = boot_unmetered(deadline.as_secs()).await;
    create(&server, &keys.agent, 1).await;
    keep_results(&server, &keys, 1).await;
    server
        .results_clock
        .set(at(START) + chrono::TimeDelta::days(8));
    let app = server.router_with(|http| http.max_concurrent_requests = 1);

    let unlink = server.state.results.root.hold_next_unlink();
    let sent = tokio::time::Instant::now();
    let request = support::send(
        &app,
        routed(
            &keys.agent.token,
            "POST",
            "/results",
            Some(&attached(&corpus_query(), 1, &json!({}))),
        ),
    );
    tokio::time::timeout(Duration::from_mins(1), unlink.parked)
        .await
        .expect("the admission reaches its unlink")
        .expect("the unlink parks");
    request.abort();
    assert!(
        request.await.unwrap_err().is_cancelled(),
        "the request's future was dropped during the unlink"
    );
    support::assert_refused(
        support::call(
            &app,
            routed(&keys.agent.token, "GET", "/investigations", None),
        )
        .await,
        support::REGULAR_MESSAGE,
    )
    .await;

    until_admitted(&app, &keys.agent.token).await;
    assert!(
        sent.elapsed() >= deadline,
        "the count came back {:?} after the capture began, before its {deadline:?} deadline",
        sent.elapsed()
    );
    let running = steps(&server, &keys.human, 1).await;
    assert!(
        outcome(&running[1]).is_none(),
        "the step settled while its admission's unlink was parked: {running:?}"
    );

    let _ = unlink.release.send(());
    let step = settled(&server, &keys.human, 1, 3).await;
    let recorded = outcome(&step).unwrap();
    assert_eq!(recorded.outcome, StepOutcomeKind::Aborted, "{recorded:?}");
}

/// Detached step outcomes are written by one writer, in turn (ADR-0059).
/// Six attached captures are admitted, their steps committed, and wait
/// behind the agent's one running slot, which an unattached capture holds;
/// another session holds the investigation's lock, and the six requests go
/// away in order. Their `aborted` writes wait on the lock one at a time:
/// one session waits, the pool still serves a read, and once the lock is
/// released the six read `aborted` in the order their requests went away.
#[tokio::test(flavor = "multi_thread")]
async fn abandoned_steps_are_written_in_turn_by_one_writer() {
    const ABANDONED: u64 = 6;
    let (server, keys) = boot_unmetered_with(CaptureConfig {
        // Long enough that neither the queued captures nor the writer's
        // attempt reaches it while the test watches.
        deadline_secs: 120,
        per_key_running: 1,
        queue_depth: ABANDONED,
        ..CaptureConfig::default()
    })
    .await;
    create(&server, &keys.agent, 1).await;
    let app = server.router_with(|_| {});
    let agent = verified(&server, &keys.agent).await.id;
    let seams = server.state.query.pool.seams();
    let started = seams.hold(Seam::Started);
    let holder = support::send(
        &app,
        routed(
            &keys.agent.token,
            "POST",
            "/results",
            Some(&json!({ "query": corpus_query(), "window": { "last": "1h" } })),
        ),
    );
    wait_until("the holder's capture to start", || started.arrivals() == 1).await;

    let mut abandoned = Vec::new();
    for queued in 1..=ABANDONED {
        abandoned.push(support::send(
            &app,
            routed(
                &keys.agent.token,
                "POST",
                "/results",
                Some(&attached(&corpus_query(), 1, &json!({}))),
            ),
        ));
        wait_until("the attached capture to queue behind the holder", || {
            server.state.results.gate.queued(agent) == queued
        })
        .await;
    }
    let mut held = hold(&server, 1).await;
    for request in abandoned {
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
    }

    let give_up = tokio::time::Instant::now() + Duration::from_secs(30);
    while lock_waiters(&server, INVESTIGATION_LOCK).await == 0 {
        assert!(
            tokio::time::Instant::now() < give_up,
            "no write waited on the investigation"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        lock_waiters(&server, INVESTIGATION_LOCK).await,
        1,
        "more than one write waits on the investigation"
    );
    let running = tokio::time::timeout(Duration::from_secs(5), steps(&server, &keys.human, 1))
        .await
        .expect("a read is served while the writes wait");
    assert_eq!(running.len(), 6, "{running:?}");
    assert!(
        running.iter().all(|step| outcome(step).is_none()),
        "{running:?}"
    );

    sqlx::query("COMMIT").execute(&mut held).await.unwrap();
    let mut written = Vec::new();
    for seq in 1..=ABANDONED {
        let step = settled(&server, &keys.human, 1, seq).await;
        let recorded = outcome(&step).unwrap();
        assert_eq!(recorded.outcome, StepOutcomeKind::Aborted, "{recorded:?}");
        written.push(step.updated_seq);
    }
    assert_eq!(
        written,
        (ABANDONED + 1..=2 * ABANDONED).collect::<Vec<_>>(),
        "the outcomes were written out of turn"
    );

    started.release();
    assert_eq!(support::finish(holder).await.status(), StatusCode::OK);
}

/// The step writer has `installation_running + queue_depth` places, two
/// here, and a step holds one from before its append until its outcome is
/// written (ADR-0059). An attached capture on investigation 3 runs, held
/// at its start; one on investigation 1 waits behind it and goes away
/// while another session holds investigation 1's lock, so its `aborted`
/// write waits for the writer. An attached capture on investigation 2,
/// which nothing locks, finds no place: it answers 429 `capture_busy` with
/// a retry of one second at its deadline, and investigation 2 holds no
/// step. Once the lock and the held capture are released, the outcomes
/// land and the same capture succeeds.
#[tokio::test(flavor = "multi_thread")]
async fn an_attached_capture_waits_for_a_place_among_the_unwritten_outcomes() {
    const DEADLINE_SECS: u64 = 5;
    let (server, keys) = boot_unmetered_with(CaptureConfig {
        deadline_secs: DEADLINE_SECS,
        per_key_running: 1,
        installation_running: 1,
        queue_depth: 1,
        ..CaptureConfig::default()
    })
    .await;
    for n in 1..=3 {
        create(&server, &keys.agent, n).await;
    }
    let app = server.router_with(|_| {});
    let agent = verified(&server, &keys.agent).await.id;
    let seams = server.state.query.pool.seams();
    let started = seams.hold(Seam::Started);
    let holder = support::send(
        &app,
        routed(
            &keys.agent.token,
            "POST",
            "/results",
            Some(&attached(&corpus_query(), 3, &json!({}))),
        ),
    );
    wait_until("the held capture to start", || started.arrivals() == 1).await;
    let abandoned = support::send(
        &app,
        routed(
            &keys.agent.token,
            "POST",
            "/results",
            Some(&attached(&corpus_query(), 1, &json!({}))),
        ),
    );
    wait_until("the attached capture to queue behind the held one", || {
        server.state.results.gate.queued(agent) == 1
    })
    .await;
    let mut held = hold(&server, 1).await;
    abandoned.abort();
    assert!(abandoned.await.unwrap_err().is_cancelled());
    lock_waits(&server, INVESTIGATION_LOCK, 1).await;

    let sent = tokio::time::Instant::now();
    let (status, headers, body) = tokio::time::timeout(
        support::DEADLINE,
        captured::capture(
            &server,
            &keys.agent.token,
            &attached(&corpus_query(), 2, &json!({})),
        ),
    )
    .await
    .expect("the capture answers by its deadline");
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(code(&body), "capture_busy", "{body}");
    assert_eq!(
        headers.get("retry-after").and_then(|v| v.to_str().ok()),
        Some("1")
    );
    assert!(
        sent.elapsed() >= Duration::from_secs(DEADLINE_SECS),
        "the capture was refused {:?} after it was sent, before its deadline",
        sent.elapsed()
    );
    assert_eq!(
        seq_and_entries(&server, &keys, 2).await,
        (0, 0),
        "a capture without a place appended a step"
    );

    sqlx::query("COMMIT").execute(&mut held).await.unwrap();
    let step = settled(&server, &keys.human, 1, 1).await;
    assert_eq!(outcome(&step).unwrap().outcome, StepOutcomeKind::Aborted);
    started.release();
    support::finish(holder).await;
    settled(&server, &keys.human, 3, 1).await;

    let answer = captured::captured(
        &server,
        &keys.agent.token,
        &attached(&corpus_query(), 2, &json!({})),
    )
    .await;
    assert_eq!(answer.step_seq, Some(1));
    let step = settled(&server, &keys.human, 2, 1).await;
    assert_eq!(outcome(&step).unwrap().outcome, StepOutcomeKind::Success);
}

/// A capture refused while its text and bounds are planned runs nothing,
/// so it records nothing: a window wider than the capture's span, and a
/// query holding a NUL.
#[tokio::test(flavor = "multi_thread")]
async fn a_capture_refused_by_its_plan_records_no_step() {
    let (server, keys) = boot(CaptureConfig::default()).await;
    create(&server, &keys.agent, 1).await;

    let mut wide = attached(&corpus_query(), 1, &json!({}));
    wide["window"] = json!({ "last": "400d" });
    let (status, _, body) = captured::capture(&server, &keys.agent.token, &wide).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let mut nul = attached(&corpus_query(), 1, &json!({}));
    nul["query"] = json!("service=x\u{0}");
    let (status, _, body) = captured::capture(&server, &keys.agent.token, &nul).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    assert_eq!(seq_and_entries(&server, &keys, 1).await, (0, 0));
}

/// Attaching needs `investigation_write`: the human's attached capture is
/// 403 and records nothing, before its text is judged, so a query holding
/// a NUL is 403 too. Revoking the agent's write permission between two
/// attached captures refuses the second, and the investigation still
/// reads with its creator's snapshot.
#[tokio::test(flavor = "multi_thread")]
async fn attaching_needs_write_authority_read_live_from_the_role() {
    let (server, keys) = boot(CaptureConfig::default()).await;
    create(&server, &keys.agent, 1).await;

    let (status, _, body) = captured::capture(
        &server,
        &keys.human.token,
        &attached(&corpus_query(), 1, &json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let mut nul = attached(&corpus_query(), 1, &json!({}));
    nul["query"] = json!("service=x\u{0}");
    let (status, _, body) = captured::capture(&server, &keys.human.token, &nul).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(seq_and_entries(&server, &keys, 1).await, (0, 0));

    let first = captured::captured(
        &server,
        &keys.agent.token,
        &attached(&corpus_query(), 1, &json!({})),
    )
    .await;
    assert_eq!(first.result.captured_rows, captured::ROWS as u64);
    settled(&server, &keys.human, 1, 1).await;

    let removed = fleet_auth::KeyStore::from_pool(server.fleet_pool.clone())
        .remove_role_permissions("role-agent", &trawl_perms(&["investigation_write"]))
        .await
        .expect("remove the permission");
    assert_eq!(removed, 1);
    let (status, _, body) = captured::capture(
        &server,
        &keys.agent.token,
        &attached(&corpus_query(), 1, &json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let page = read(&server, &keys.human, 1, "").await;
    assert_eq!(page.investigation.created_by.name, "agent");
    assert_eq!(page.investigation.created_by.prefix, keys.agent.prefix);
    assert_eq!(
        steps(&server, &keys.human, 1).await.len(),
        1,
        "the refused capture recorded no step"
    );
}

/// An unknown investigation is 404, a closed one 409
/// `investigation_closed` naming the reopen, and a purpose over its bound
/// 400 naming the rule; none records a step.
#[tokio::test(flavor = "multi_thread")]
async fn an_attachment_names_an_open_investigation_and_a_bounded_purpose() {
    let (server, keys) = boot(CaptureConfig::default()).await;

    let (status, _, body) = captured::capture(
        &server,
        &keys.agent.token,
        &attached(&corpus_query(), 7, &json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(message(&body), "investigation not found", "{body}");

    create(&server, &keys.agent, 1).await;
    let (status, body) = call(
        &server,
        &keys.agent.token,
        Method::PATCH,
        &format!("/investigations/{}", id(1)),
        Some(&json!({ "expected_seq": 0, "status": "closed" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _, body) = captured::capture(
        &server,
        &keys.agent.token,
        &attached(&corpus_query(), 1, &json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(code(&body), "investigation_closed", "{body}");
    assert!(message(&body).contains("reopen"), "{body}");
    assert_eq!(seq_and_entries(&server, &keys, 1).await, (1, 1));

    create(&server, &keys.agent, 2).await;
    let long = "p".repeat(trawl_api::investigations::PURPOSE_MAX_BYTES + 1);
    let (status, _, body) = captured::capture(
        &server,
        &keys.agent.token,
        &attached(&corpus_query(), 2, &json!({ "purpose": long })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        message(&body),
        "purpose is 1025 bytes, above the limit of 1024 bytes"
    );
    assert_eq!(seq_and_entries(&server, &keys, 2).await, (0, 0));
}
