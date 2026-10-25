// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Evidence (ADR-0059): an attached capture's result is kept from birth,
//! under the evidence budgets instead of the temporary quota, in one
//! transaction with its step's success; the response carries what other
//! keys wrote since the caller last looked.
//!
//! Through the fixture's HTTPS listener with real fleet keys: an agent key
//! that writes (`query`, `investigation_read`, `investigation_write`) and a
//! human key that reads (`query`, `investigation_read`). Captures resolve
//! their window at the fixture's results clock, set onto the capture corpus
//! of `common::captured`. The process's tracing events are recorded, so
//! each test runs in its own process: nextest's. The binary is in the
//! ADR-0021 `postgres` nextest group.

mod common;

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use common::audit_capture::Capture;
use common::captured::{self, at};
use common::investigating::{
    attached, call, code, create, id, message, outcome, read, settled, steps,
};
use common::{MintedKey, TestServer, mint_key};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use trawl_api::ErrorCode;
use trawl_api::investigations::{EntryPayload, InvestigationId, StepOutcome, StepOutcomeKind};
use trawl_api::results::ResultState;
use trawl_config::{CaptureConfig, InvestigationConfig};
use trawl_server::config::RateLimitConfig;
use trawl_server::pool::seam::Seam;
use trawl_server::store::results::fault::InsertFault;
use trawl_server::store::{KeptInsert, KeptStep, RetentionRule};

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

/// A fixture over the capture corpus with `capture` and `investigation`,
/// its clock at [`START`].
async fn boot(
    capture: CaptureConfig,
    investigation: InvestigationConfig,
) -> (Arc<TestServer>, Keys) {
    let _ = telemetry();
    let tmp = tempfile::tempdir().expect("create a temp dir");
    let data = common::seed_data_root(tmp.path());
    captured::seed_hour(&data, captured::SERVICE, &captured::corpus());
    let server = common::setup_with_evidence(
        tmp.path(),
        data,
        capture,
        investigation,
        RateLimitConfig::default(),
        60,
    )
    .await;
    std::mem::forget(tmp);
    server.results_clock.set(at(START));
    let pool = &server.fleet_pool;
    let keys = Keys {
        agent: mint_key(
            pool,
            "agent",
            &["query", "investigation_read", "investigation_write"],
        )
        .await,
        human: mint_key(pool, "human", &["query", "investigation_read"]).await,
    };
    (Arc::new(server), keys)
}

/// [`boot`] with the default limits.
async fn boot_default() -> (Arc<TestServer>, Keys) {
    boot(CaptureConfig::default(), InvestigationConfig::default()).await
}

/// The capture corpus's 20 events.
fn corpus_query() -> String {
    format!("service={}", captured::SERVICE)
}

/// `[capture] max_file_bytes` at its default: what an attached capture
/// reserves.
fn max_file_bytes() -> u64 {
    CaptureConfig::default().max_file_bytes
}

/// Investigation `n`'s id, typed.
fn investigation(n: u32) -> InvestigationId {
    InvestigationId::parse(&id(n)).unwrap()
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

/// Tracing events whose `event_type` is `name`.
fn events_of(name: &str) -> Vec<common::audit_capture::Captured> {
    let quoted = format!("\"{name}\"");
    telemetry()
        .events()
        .into_iter()
        .filter(|event| event.fields.get("event_type") == Some(&quoted))
        .collect()
}

/// The `query_start` events of query `query_id`.
fn query_starts(query_id: u64) -> usize {
    events_of("query_start")
        .into_iter()
        .filter(|event| event.fields.get("query_id") == Some(&query_id.to_string()))
        .count()
}

/// The query id step `step` names.
fn step_query_id(step: &trawl_api::investigations::LogEntry) -> u64 {
    let EntryPayload::Step { query_id, .. } = &step.payload else {
        panic!("not a step: {step:?}");
    };
    query_id.expect("the step names its query")
}

/// The committed evidence bytes of investigation `n` and of the
/// installation.
async fn evidence(server: &TestServer, n: u32) -> (u64, u64) {
    let store = &server.state.storage.investigations;
    (
        store.evidence_usage(&investigation(n)).await.unwrap(),
        store.installation_evidence_usage().await.unwrap(),
    )
}

/// Rows of `captured_results` attached to investigation `n`.
async fn kept_rows(server: &TestServer, n: u32) -> i64 {
    let pool = common::fixture_pool(&server.app_db_url, 1).await;
    let rows =
        sqlx::query_scalar("SELECT count(*) FROM captured_results WHERE investigation_id = $1")
            .bind(id(n))
            .fetch_one(&pool)
            .await
            .unwrap();
    pool.close().await;
    rows
}

// -- kept from birth -----------------------------------------------------------

/// An attached capture's result is the investigation's from its commit:
/// `kept`, no deadline, naming the investigation, one file in the root. It
/// is not the agent's temporary result, so the temporary list and the
/// capture quota do not count it, and the evidence account counts its file
/// once. The human, who reads the investigation, pages it.
#[tokio::test(flavor = "multi_thread")]
async fn an_attached_capture_is_kept_from_birth() {
    let (server, keys) = boot_default().await;
    create(&server, &keys.agent, 1).await;

    let response = captured::captured(
        &server,
        &keys.agent.token,
        &attached(&corpus_query(), 1, &json!({})),
    )
    .await;
    let result = response.result;
    assert_eq!(result.state, ResultState::Kept);
    assert_eq!(result.expires_at, None);
    assert_eq!(result.investigation_id, Some(investigation(1)));
    assert_eq!(result.captured_rows, captured::ROWS as u64);
    assert!(result.file_bytes > 0);

    let key_id = captured::key_id(&server, &keys.agent.token).await;
    let row = server
        .state
        .results
        .store
        .get(&result.id, key_id)
        .await
        .unwrap()
        .expect("the row");
    assert_eq!(
        (row.state, row.expires_at, row.investigation_id.as_ref()),
        (ResultState::Kept, None, Some(&investigation(1)))
    );
    assert_eq!(
        captured::root_names(&server),
        [format!("{}.parquet", result.id)]
    );
    assert!(
        captured::listed(&server, &keys.agent.token)
            .await
            .is_empty(),
        "a kept result is not on the temporary list"
    );
    let now = server.results_clock.now();
    let usage = server.state.results.store.usage(key_id, now).await.unwrap();
    assert_eq!((usage.results, usage.bytes), (0, 0), "no temporary quota");
    assert_eq!(
        evidence(&server, 1).await,
        (result.file_bytes, result.file_bytes)
    );

    let (status, _, page) = captured::page(&server, &keys.human.token, &result.id, "").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(
        page["page"]["rows"].as_array().map(Vec::len),
        Some(captured::ROWS),
        "{page}"
    );

    // One step, filled once, in the transaction that kept the row.
    let step = settled(&server, &keys.human, 1, 1).await;
    let recorded = outcome(&step).unwrap();
    assert_eq!(recorded.outcome, StepOutcomeKind::Success);
    assert_eq!(recorded.result_id.as_deref(), Some(result.id.as_str()));
    assert_eq!(step.updated_seq, 2);
    assert_eq!(read(&server, &keys.human, 1, "").await.investigation.seq, 2);
    assert_eq!(kept_rows(&server, 1).await, 1);
}

// -- the evidence budget -----------------------------------------------------

/// A per-investigation evidence budget below `max_file_bytes` refuses an
/// attached capture before any permit: 409 `evidence_budget_exceeded`
/// naming the usage and the budget, no query started, the step `refused`
/// with the code, and nothing held.
#[tokio::test(flavor = "multi_thread")]
async fn the_investigations_budget_refuses_an_attached_capture_before_any_permit() {
    let limit = max_file_bytes() - 1;
    let (server, keys) = boot(
        CaptureConfig::default(),
        InvestigationConfig {
            evidence_per_investigation_bytes: limit,
            ..InvestigationConfig::default()
        },
    )
    .await;
    create(&server, &keys.agent, 1).await;
    let permits = server.state.query.pool.available_permits();

    let (status, _, body) = captured::capture(
        &server,
        &keys.agent.token,
        &attached(&corpus_query(), 1, &json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(code(&body), "evidence_budget_exceeded", "{body}");
    assert!(
        message(&body).contains(&format!("0 of {limit} bytes")),
        "{body}"
    );
    captured::assert_nothing_held(&server, permits, "a refused attached capture").await;

    let step = settled(&server, &keys.human, 1, 1).await;
    let recorded = outcome(&step).unwrap();
    assert_eq!(recorded.outcome, StepOutcomeKind::Refused, "{recorded:?}");
    assert_eq!(recorded.code, Some(ErrorCode::EvidenceBudgetExceeded));
    assert_eq!(query_starts(step_query_id(&step)), 0);
    assert_eq!(kept_rows(&server, 1).await, 0);
    assert!(captured::root_names(&server).is_empty());
}

/// The installation's evidence budget refuses the same way, whichever
/// investigation the capture names.
#[tokio::test(flavor = "multi_thread")]
async fn the_installations_budget_refuses_an_attached_capture_before_any_permit() {
    let limit = max_file_bytes() - 1;
    let (server, keys) = boot(
        CaptureConfig::default(),
        InvestigationConfig {
            evidence_installation_bytes: limit,
            ..InvestigationConfig::default()
        },
    )
    .await;
    create(&server, &keys.agent, 1).await;
    let permits = server.state.query.pool.available_permits();

    let (status, _, body) = captured::capture(
        &server,
        &keys.agent.token,
        &attached(&corpus_query(), 1, &json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(code(&body), "evidence_budget_exceeded", "{body}");
    assert!(
        message(&body).contains(&format!("0 of {limit} bytes")),
        "{body}"
    );
    captured::assert_nothing_held(&server, permits, "a refused attached capture").await;
    let step = settled(&server, &keys.human, 1, 1).await;
    let recorded = outcome(&step).unwrap();
    assert_eq!(recorded.outcome, StepOutcomeKind::Refused, "{recorded:?}");
    assert_eq!(recorded.code, Some(ErrorCode::EvidenceBudgetExceeded));
    assert_eq!(query_starts(step_query_id(&step)), 0);
}

/// An attached capture in flight holds `max_file_bytes` of the budget, and
/// settles to its file's size once kept. With a budget of one and a half
/// reservations: a second capture while the first is held is refused, its
/// usage naming the first's reservation; once the first commits its small
/// file, a third is admitted.
#[tokio::test(flavor = "multi_thread")]
async fn an_attached_capture_holds_its_reservation_until_it_settles() {
    let limit = max_file_bytes() / 2 * 3;
    let (server, keys) = boot(
        CaptureConfig {
            deadline_secs: 300,
            ..CaptureConfig::default()
        },
        InvestigationConfig {
            evidence_per_investigation_bytes: limit,
            ..InvestigationConfig::default()
        },
    )
    .await;
    create(&server, &keys.agent, 1).await;
    let seams = server.state.query.pool.seams();
    let started = seams.hold(Seam::Started);
    let first = spawn_capture(
        &server,
        &keys.agent.token,
        attached(&corpus_query(), 1, &json!({})),
    );
    wait_until("the first capture's work to start", || {
        started.arrivals() == 1
    })
    .await;

    let (status, _, body) = captured::capture(
        &server,
        &keys.agent.token,
        &attached(&corpus_query(), 1, &json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(code(&body), "evidence_budget_exceeded", "{body}");
    assert!(
        message(&body).contains(&format!("{} of {limit} bytes", max_file_bytes())),
        "the usage counts the first capture's reservation: {body}"
    );

    started.release();
    let (status, _, body) = first.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    let file_bytes = body["result"]["file_bytes"].as_u64().unwrap();
    assert!(
        file_bytes + max_file_bytes() <= limit,
        "the corpus's file leaves room for another reservation: {file_bytes}"
    );

    let third = captured::captured(
        &server,
        &keys.agent.token,
        &attached(&corpus_query(), 1, &json!({})),
    )
    .await;
    assert_eq!(third.result.state, ResultState::Kept);
    assert_eq!(
        evidence(&server, 1).await.0,
        file_bytes + third.result.file_bytes
    );
}

// -- the fence ---------------------------------------------------------------

/// A kept insert whose answer is lost after it committed is fenced by the
/// same transaction, which finds the id taken and writes nothing: one kept
/// row, one step fill, the investigation advanced once for the step and
/// once for its fill.
#[tokio::test(flavor = "multi_thread")]
async fn a_fenced_kept_insert_commits_the_row_and_the_step_once() {
    let (server, keys) = boot_default().await;
    create(&server, &keys.agent, 1).await;
    server
        .state
        .results
        .store
        .faults()
        .fail_next_insert(InsertFault::Committed);

    let response = captured::captured(
        &server,
        &keys.agent.token,
        &attached(&corpus_query(), 1, &json!({})),
    )
    .await;
    assert_eq!(response.result.state, ResultState::Kept);
    let fenced = events_of("results_commit_fenced");
    assert_eq!(fenced.len(), 1, "{fenced:?}");
    assert_eq!(fenced[0].field("fence"), "\"id_taken\"");

    assert_eq!(kept_rows(&server, 1).await, 1);
    let page = read(&server, &keys.human, 1, "").await;
    assert_eq!(page.investigation.seq, 2, "the step and its one fill");
    let step = settled(&server, &keys.human, 1, 1).await;
    assert_eq!(step.updated_seq, 2);
    let recorded = outcome(&step).unwrap();
    assert_eq!(recorded.outcome, StepOutcomeKind::Success);
    assert_eq!(
        recorded.result_id.as_deref(),
        Some(response.result.id.as_str())
    );
    assert_eq!(
        captured::root_names(&server),
        [format!("{}.parquet", response.result.id)]
    );
}

/// The kept insert writes nothing unless its step is still running: a
/// step whose outcome is already written is [`KeptInsert::StepSettled`],
/// with no row, and the investigation does not move.
#[tokio::test(flavor = "multi_thread")]
async fn the_kept_insert_writes_nothing_once_its_step_has_an_outcome() {
    let (server, keys) = boot_default().await;
    create(&server, &keys.agent, 1).await;
    let response = captured::captured(
        &server,
        &keys.agent.token,
        &attached(&corpus_query(), 1, &json!({})),
    )
    .await;
    let key_id = captured::key_id(&server, &keys.agent.token).await;
    let store = &server.state.results.store;
    let kept = store
        .get(&response.result.id, key_id)
        .await
        .unwrap()
        .unwrap();
    let again = trawl_server::store::NewCapture {
        id: format!("{:032x}", 0xfeed_u32),
        key_id,
        created_at: server.results_clock.now(),
        expires_at: server.results_clock.now(),
        query: kept.query,
        executed_query: kept.executed_query,
        source: kept.source,
        shape: kept.shape,
        earliest: kept.earliest,
        latest: kept.latest,
        columns: kept.columns,
        column_facts: kept.column_facts,
        captured_rows: kept.captured_rows,
        total_rows: kept.total_rows,
        wire_bytes: kept.wire_bytes,
        file_bytes: 0,
        digest: None,
        format_version: kept.format_version,
        investigation_id: Some(investigation(1)),
    };
    let success = StepOutcome {
        outcome: StepOutcomeKind::Success,
        code: None,
        result_id: Some(again.id.clone()),
        rows: Some(0),
        duration_ms: Some(1),
    };
    let step = KeptStep {
        seq: 1,
        outcome: &success,
        retention: RetentionRule {
            idle: chrono::TimeDelta::days(7),
        },
    };
    assert_eq!(
        store.insert_kept(&again, step).await.unwrap(),
        KeptInsert::StepSettled
    );
    assert!(store.get(&again.id, key_id).await.unwrap().is_none());
    assert_eq!(read(&server, &keys.human, 1, "").await.investigation.seq, 2);

    let missing = trawl_server::store::NewCapture {
        investigation_id: Some(investigation(9)),
        ..again.clone()
    };
    assert!(matches!(
        store.insert_kept(&missing, step).await,
        Err(trawl_server::store::StoreError::InvestigationNotFound)
    ));
    assert!(matches!(
        store.insert(&again).await,
        Err(trawl_server::store::StoreError::Validation(_))
    ));
}

// -- activity ----------------------------------------------------------------

/// With `seen_seq`, an attached capture answers what other keys wrote
/// since: a colleague's note between the agent's two captures, and none of
/// the agent's own step entries. Without it, no activity. (The human key
/// only reads, so a second writing key makes the note.)
#[tokio::test(flavor = "multi_thread")]
async fn an_attached_capture_answers_the_activity_since_seen() {
    let (server, keys) = boot_default().await;
    let colleague = mint_key(
        &server.fleet_pool,
        "colleague",
        &["query", "investigation_read", "investigation_write"],
    )
    .await;
    create(&server, &keys.agent, 1).await;

    let (status, _, first) = captured::capture(
        &server,
        &keys.agent.token,
        &attached(&corpus_query(), 1, &json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert!(first.get("activity").is_none(), "{first}");
    let seen = read(&server, &keys.agent, 1, "").await.investigation.seq;
    assert_eq!(seen, 2);

    let (status, body) = call(
        &server,
        &colleague.token,
        Method::POST,
        &format!("/investigations/{}/entries", id(1)),
        Some(&json!({ "kind": "note", "text": "the 502s start at 09:58" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let second = captured::captured(
        &server,
        &keys.agent.token,
        &attached(&corpus_query(), 1, &json!({ "seen_seq": seen })),
    )
    .await;
    let activity = second.activity.expect("seen_seq asks for activity");
    assert_eq!(activity.entries.len(), 1, "{activity:?}");
    assert_eq!(activity.entries[0].seq, 3);
    assert_eq!(activity.entries[0].actor.prefix, colleague.prefix);
    assert!(matches!(
        &activity.entries[0].payload,
        EntryPayload::Note { text } if text == "the 502s start at 09:58"
    ));
    assert_eq!(activity.through_seq, 5, "the capture's step and fill");
    assert!(!activity.more);
    assert_eq!(steps(&server, &keys.human, 1).await.len(), 2);
}

/// A refused attached capture carries no activity: its answer is the
/// error.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_attached_capture_carries_no_activity() {
    let (server, keys) = boot(
        CaptureConfig::default(),
        InvestigationConfig {
            evidence_per_investigation_bytes: 1,
            ..InvestigationConfig::default()
        },
    )
    .await;
    create(&server, &keys.agent, 1).await;
    let (status, _, body) = captured::capture(
        &server,
        &keys.agent.token,
        &attached(&corpus_query(), 1, &json!({ "seen_seq": 0 })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.get("activity").is_none(), "{body}");
    assert!(body.get("error").is_some(), "{body}");
}
