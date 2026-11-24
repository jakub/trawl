// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Evidence (ADR-0059).
//!
//! The process's tracing events are recorded, so each test runs in its own
//! process: nextest's. The binary is in the ADR-0021 `postgres` nextest group.

mod common;

#[path = "request_limit/support.rs"]
mod support;

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use common::audit_capture::Capture;
use common::captured::{self, at};
use common::investigating::{
    INVESTIGATION_LOCK, PROMOTION_LOCK, attached, call, code, create, hold, hold_result, id,
    lock_waiters, lock_waits, message, outcome, read, routed, settled, steps, until_admitted,
};
use common::{MintedKey, TestServer, mint_key};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use trawl_api::ErrorCode;
use trawl_api::investigations::{
    AppendEntry, EntryPayload, InvestigationId, StepOutcome, StepOutcomeKind,
};
use trawl_api::results::{
    CaptureKind, ColumnFacts, Correlation, ResultColumn, ResultShape, ResultSource, ResultState,
    SightingsDenominators, SightingsFacts, StoredAs,
};
use trawl_api::value::ColumnType;
use trawl_config::{CaptureConfig, InvestigationConfig};
use trawl_server::config::RateLimitConfig;
use trawl_server::pool::seam::Seam;
use trawl_server::store::results::fault::InsertFault;
use trawl_server::store::{
    CaptureInsert, FactsDocument, KeptInsert, KeptStep, NewCapture, RetentionRule,
};

/// Where the clock stands when a test captures: half an hour into the
/// capture corpus's hour.
const START: &str = "2024-01-16T10:30:00Z";

struct Keys {
    agent: MintedKey,
    human: MintedKey,
}

/// Installed once: the server logs from every tokio worker and blocking
/// thread.
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

async fn boot(
    capture: CaptureConfig,
    investigation: InvestigationConfig,
) -> (Arc<TestServer>, Keys) {
    boot_timing_out(capture, investigation, 60).await
}

async fn boot_timing_out(
    capture: CaptureConfig,
    investigation: InvestigationConfig,
    timeout_secs: u64,
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
        timeout_secs,
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

async fn boot_default() -> (Arc<TestServer>, Keys) {
    boot(CaptureConfig::default(), InvestigationConfig::default()).await
}

fn corpus_query() -> String {
    format!("service={}", captured::SERVICE)
}

/// `[capture] max_file_bytes` at its default: what an attached capture
/// reserves.
fn max_file_bytes() -> u64 {
    CaptureConfig::default().max_file_bytes
}

fn investigation(n: u32) -> InvestigationId {
    InvestigationId::parse(&id(n)).unwrap()
}

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

fn spawn_capture(
    server: &Arc<TestServer>,
    token: &str,
    body: Value,
) -> tokio::task::JoinHandle<captured::Answer> {
    let server = Arc::clone(server);
    let token = token.to_owned();
    tokio::spawn(async move { captured::capture(&server, &token, &body).await })
}

fn events_of(name: &str) -> Vec<common::audit_capture::Captured> {
    let quoted = format!("\"{name}\"");
    telemetry()
        .events()
        .into_iter()
        .filter(|event| event.fields.get("event_type") == Some(&quoted))
        .collect()
}

fn query_starts(query_id: u64) -> usize {
    events_of("query_start")
        .into_iter()
        .filter(|event| event.fields.get("query_id") == Some(&query_id.to_string()))
        .count()
}

fn step_query_id(step: &trawl_api::investigations::LogEntry) -> u64 {
    let EntryPayload::Step { query_id, .. } = &step.payload else {
        panic!("not a step: {step:?}");
    };
    query_id.expect("the step names its query")
}

async fn evidence(server: &TestServer, n: u32) -> (u64, u64) {
    let store = &server.state.storage.investigations;
    (
        store.evidence_usage(&investigation(n)).await.unwrap(),
        store.installation_evidence_usage().await.unwrap(),
    )
}

fn budget_refusal(budget: &str, used: u64, limit: u64) -> String {
    format!(
        "keeping this result would exceed {budget}: {used} of {limit} bytes are counted \
         against it, kept evidence and reservations together; evidence frees as it ages out, \
         or when an operator with server_manage deletes an investigation"
    )
}

fn evidence_refusal_scopes() -> Vec<String> {
    events_of("capture_refused")
        .into_iter()
        .filter(|event| event.field("reason") == "\"evidence_budget_exceeded\"")
        .map(|event| event.field("scope"))
        .collect()
}

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

    let step = settled(&server, &keys.human, 1, 1).await;
    let recorded = outcome(&step).unwrap();
    assert_eq!(recorded.outcome, StepOutcomeKind::Success);
    assert_eq!(recorded.result_id.as_deref(), Some(result.id.as_str()));
    assert_eq!(step.updated_seq, 2);
    assert_eq!(read(&server, &keys.human, 1, "").await.investigation.seq, 2);
    assert_eq!(kept_rows(&server, 1).await, 1);
}

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
    assert_eq!(
        message(&body),
        budget_refusal("this investigation's evidence budget", 0, limit),
        "{body}"
    );
    assert_eq!(evidence_refusal_scopes(), ["\"investigation\""]);
    captured::assert_nothing_held(&server, permits, "a refused attached capture").await;

    let step = settled(&server, &keys.human, 1, 1).await;
    let recorded = outcome(&step).unwrap();
    assert_eq!(recorded.outcome, StepOutcomeKind::Refused, "{recorded:?}");
    assert_eq!(recorded.code, Some(ErrorCode::EvidenceBudgetExceeded));
    assert_eq!(query_starts(step_query_id(&step)), 0);
    assert_eq!(kept_rows(&server, 1).await, 0);
    assert!(captured::root_names(&server).is_empty());
}

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
    assert_eq!(
        message(&body),
        budget_refusal("the installation's evidence budget", 0, limit),
        "{body}"
    );
    assert_eq!(evidence_refusal_scopes(), ["\"installation\""]);
    captured::assert_nothing_held(&server, permits, "a refused attached capture").await;
    let step = settled(&server, &keys.human, 1, 1).await;
    let recorded = outcome(&step).unwrap();
    assert_eq!(recorded.outcome, StepOutcomeKind::Refused, "{recorded:?}");
    assert_eq!(recorded.code, Some(ErrorCode::EvidenceBudgetExceeded));
    assert_eq!(query_starts(step_query_id(&step)), 0);
}

fn attached_correlation_reads(focus: &str) -> [(&'static str, Value); 3] {
    let investigation = json!({ "id": id(1) });
    [
        (
            "context",
            json!({ "result_id": focus, "row": 0, "investigation": investigation }),
        ),
        (
            "sightings",
            json!({ "literal": "event", "window": { "last": "1h" }, "investigation": investigation }),
        ),
        (
            "compare",
            json!({
                "filter": corpus_query(),
                "foreground": { "last": "30m" },
                "group_by": ["host"],
                "investigation": investigation,
            }),
        ),
    ]
}

async fn assert_budget_refuses_correlation_reads(budget: impl FnOnce(u64) -> InvestigationConfig) {
    let limit = max_file_bytes() - 1;
    let (server, keys) = boot(CaptureConfig::default(), budget(limit)).await;
    let focus = captured::captured(
        &server,
        &keys.agent.token,
        &json!({ "query": corpus_query(), "window": { "last": "1h" } }),
    )
    .await
    .result
    .id;
    create(&server, &keys.agent, 1).await;
    let permits = server.state.query.pool.available_permits();

    for (read, body) in attached_correlation_reads(&focus) {
        let (status, _, answer) =
            captured::correlate(&server, &keys.agent.token, read, &body).await;
        assert_eq!(status, StatusCode::CONFLICT, "{read}: {answer}");
        assert_eq!(code(&answer), "evidence_budget_exceeded", "{answer}");
        assert!(
            message(&answer).contains(&format!("0 of {limit} bytes")),
            "{answer}"
        );
        captured::assert_nothing_held(&server, permits, read).await;

        // The step was appended before the refusal answered; an earlier
        // step's outcome took a seq of its own, so the newest step is read.
        let seq = steps(&server, &keys.human, 1)
            .await
            .last()
            .expect("the refused read appended its step")
            .seq;
        let step = settled(&server, &keys.human, 1, seq).await;
        let recorded = outcome(&step).unwrap();
        assert_eq!(
            recorded.outcome,
            StepOutcomeKind::Refused,
            "{read}: {recorded:?}"
        );
        assert_eq!(recorded.code, Some(ErrorCode::EvidenceBudgetExceeded));
        assert_eq!(query_starts(step_query_id(&step)), 0, "{read}");
    }
    assert_eq!(steps(&server, &keys.human, 1).await.len(), 3);
    assert_eq!(kept_rows(&server, 1).await, 0);
    assert_eq!(captured::root_names(&server), [format!("{focus}.parquet")]);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_investigations_budget_refuses_an_attached_correlation_read_before_any_permit() {
    assert_budget_refuses_correlation_reads(|limit| InvestigationConfig {
        evidence_per_investigation_bytes: limit,
        ..InvestigationConfig::default()
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_installations_budget_refuses_an_attached_correlation_read_before_any_permit() {
    assert_budget_refuses_correlation_reads(|limit| InvestigationConfig {
        evidence_installation_bytes: limit,
        ..InvestigationConfig::default()
    })
    .await;
}

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

#[tokio::test(flavor = "multi_thread")]
async fn a_pending_reservation_is_counted_not_called_kept() {
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
    let hold = server.state.results.store.faults().hold_next_insert();
    let first = spawn_capture(
        &server,
        &keys.agent.token,
        attached(&corpus_query(), 1, &json!({})),
    );
    tokio::time::timeout(Duration::from_mins(1), hold.parked)
        .await
        .expect("the first capture reaches its kept insert")
        .expect("the insert parks");
    assert_eq!(kept_rows(&server, 1).await, 0, "the first is not kept yet");
    assert_eq!(evidence(&server, 1).await, (0, 0));
    assert_eq!(
        server.state.results.gate.evidence_reserved(),
        max_file_bytes()
    );

    let (status, _, body) = captured::capture(
        &server,
        &keys.agent.token,
        &attached(&corpus_query(), 1, &json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(code(&body), "evidence_budget_exceeded", "{body}");
    assert_eq!(
        message(&body),
        budget_refusal(
            "this investigation's evidence budget",
            max_file_bytes(),
            limit
        ),
        "the count is the first capture's reservation: {body}"
    );
    assert!(!message(&body).contains("already kept"), "{body}");
    assert_eq!(kept_rows(&server, 1).await, 0, "still nothing kept");

    hold.release.send(()).unwrap();
    let (status, _, body) = first.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["state"], "kept", "{body}");
    assert_eq!(kept_rows(&server, 1).await, 1);
    let file_bytes = body["result"]["file_bytes"].as_u64().unwrap();
    assert_eq!(evidence(&server, 1).await, (file_bytes, file_bytes));
}

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
    assert_eq!(fenced[0].field("error_class"), "\"unavailable_other\"");
    assert!(
        !format!("{fenced:?}").contains("injected store fault"),
        "the insert's error text was logged: {fenced:?}"
    );

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

#[tokio::test(flavor = "multi_thread")]
async fn a_kept_insert_past_the_ceiling_answers_pruned_even_when_fenced() {
    let (server, keys) = boot_default().await;
    // Created 31 days before the capture: past its 30-day ceiling.
    server
        .results_clock
        .set(at(START) - chrono::TimeDelta::days(31));
    create(&server, &keys.agent, 1).await;
    create(&server, &keys.agent, 2).await;
    server.results_clock.set(at(START));

    let unfenced = captured::captured(
        &server,
        &keys.agent.token,
        &attached(&corpus_query(), 1, &json!({})),
    )
    .await;
    assert_eq!(unfenced.result.state, ResultState::Pruned);
    assert!(unfenced.result.file_bytes > 0, "the capture wrote a file");
    assert!(captured::root_names(&server).is_empty());

    server
        .state
        .results
        .store
        .faults()
        .fail_next_insert(InsertFault::Committed);
    let fenced = captured::captured(
        &server,
        &keys.agent.token,
        &attached(&corpus_query(), 2, &json!({})),
    )
    .await;
    let events = events_of("results_commit_fenced");
    assert!(
        events
            .iter()
            .any(|event| event.field("result_id") == fenced.result.id
                && event.field("fence") == "\"id_taken\""),
        "{events:?}"
    );
    assert_eq!(fenced.result.state, ResultState::Pruned);
    assert!(
        captured::root_names(&server).is_empty(),
        "the pruned row's file is unlinked"
    );
    assert_eq!(evidence(&server, 2).await, (0, 0));
    let step = settled(&server, &keys.human, 2, 1).await;
    assert_eq!(outcome(&step).unwrap().outcome, StepOutcomeKind::Success);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fenced_kept_insert_whose_read_back_fails_answers_the_failure() {
    let (server, keys) = boot_default().await;
    create(&server, &keys.agent, 1).await;
    let faults = server.state.results.store.faults();
    faults.fail_next_insert(InsertFault::Committed);
    faults.fail_read_back_of_next_insert();

    let (status, _, body) = captured::capture(
        &server,
        &keys.agent.token,
        &attached(&corpus_query(), 1, &json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(code(&body), "service_unavailable", "{body}");
    let unread = events_of("results_commit_unreadable");
    assert_eq!(unread.len(), 1, "{unread:?}");

    assert_eq!(kept_rows(&server, 1).await, 1);
    let step = settled(&server, &keys.human, 1, 1).await;
    let recorded = outcome(&step).unwrap();
    assert_eq!(recorded.outcome, StepOutcomeKind::Success);
    let id = recorded
        .result_id
        .clone()
        .expect("the step names its result");
    assert_eq!(captured::root_names(&server), [format!("{id}.parquet")]);
}

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
        kind: trawl_api::results::CaptureKind::Search,
        correlation: None,
    };
    let success = StepOutcome {
        outcome: StepOutcomeKind::Success,
        code: None,
        result_id: Some(again.id.clone()),
        rows: Some(0),
        total_rows: None,
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
    let first_step = steps(&server, &keys.human, 1).await[0].seq;
    assert_eq!(first["step_seq"], json!(first_step), "{first}");
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
    let steps = steps(&server, &keys.human, 1).await;
    assert_eq!(steps.len(), 2);
    assert_eq!(second.step_seq, Some(steps[1].seq));

    let (status, _, unattached) = captured::capture(
        &server,
        &keys.agent.token,
        &json!({ "query": corpus_query(), "window": { "last": "1h" } }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{unattached}");
    assert!(unattached.get("step_seq").is_none(), "{unattached}");
}

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

async fn temporary(server: &TestServer, key: &MintedKey) -> trawl_api::results::CapturedResult {
    let result = captured::captured(
        server,
        &key.token,
        &json!({ "query": corpus_query(), "window": { "last": "1h" } }),
    )
    .await
    .result;
    assert_eq!(result.state, ResultState::Temporary);
    result
}

async fn keep(server: &TestServer, key: &MintedKey, n: u32, result: &str) -> (StatusCode, Value) {
    call(
        server,
        &key.token,
        Method::POST,
        &format!("/investigations/{}/entries", id(n)),
        Some(&json!({
            "kind": "keep",
            "result_id": result,
            "rows": [0],
            "reason": "the first 502",
        })),
    )
    .await
}

async fn note(server: &TestServer, key: &MintedKey, n: u32, text: &str) -> (StatusCode, Value) {
    call(
        server,
        &key.token,
        Method::POST,
        &format!("/investigations/{}/entries", id(n)),
        Some(&json!({ "kind": "note", "text": text })),
    )
    .await
}

fn file_of(server: &TestServer, id: &str) -> (std::path::PathBuf, u64, u64) {
    use std::os::unix::fs::MetadataExt as _;
    let path = server
        .state
        .results
        .root
        .path()
        .join(format!("{id}.parquet"));
    let meta = std::fs::metadata(&path).expect("the result's file");
    (path, meta.len(), meta.ino())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_keep_citing_the_callers_temporary_result_promotes_it_in_place() {
    let (server, keys) = boot_default().await;
    create(&server, &keys.agent, 1).await;
    let result = temporary(&server, &keys.agent).await;
    let before = file_of(&server, &result.id);
    let key_id = captured::key_id(&server, &keys.agent.token).await;
    let now = server.results_clock.now();
    let usage = server.state.results.store.usage(key_id, now).await.unwrap();
    assert_eq!((usage.results, usage.bytes), (1, result.file_bytes));
    assert_eq!(
        captured::listed(&server, &keys.agent.token).await,
        std::slice::from_ref(&result.id)
    );

    let (status, body) = keep(&server, &keys.agent, 1, &result.id).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let row = server
        .state
        .results
        .store
        .get(&result.id, key_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (row.state, row.expires_at, row.investigation_id),
        (ResultState::Kept, None, Some(investigation(1)))
    );
    assert_eq!(
        file_of(&server, &result.id),
        before,
        "the file is untouched"
    );
    assert_eq!(
        captured::root_names(&server),
        [format!("{}.parquet", result.id)]
    );
    let usage = server.state.results.store.usage(key_id, now).await.unwrap();
    assert_eq!((usage.results, usage.bytes), (0, 0), "out of the quota");
    assert!(
        captured::listed(&server, &keys.agent.token)
            .await
            .is_empty()
    );
    assert_eq!(
        evidence(&server, 1).await,
        (result.file_bytes, result.file_bytes)
    );

    let (status, body) = keep(&server, &keys.agent, 1, &result.id).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        evidence(&server, 1).await,
        (result.file_bytes, result.file_bytes),
        "a second keep charges nothing"
    );
    let (status, _, page) = captured::page(&server, &keys.human.token, &result.id, "").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the human reads it as evidence: {page}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_citation_that_cannot_be_promoted_is_refused_without_an_oracle() {
    let (server, keys) = boot_default().await;
    create(&server, &keys.agent, 1).await;
    create(&server, &keys.agent, 2).await;
    let refusal = |result: &str| {
        format!("rows[0] cites result {result}, which is not kept evidence of this investigation")
    };

    let absent = format!("{:032x}", 0xdead_u32);
    let (status, body) = keep(&server, &keys.agent, 1, &absent).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(message(&body), refusal(&absent), "{body}");

    let humans = temporary(&server, &keys.human).await;
    let (status, body) = keep(&server, &keys.agent, 1, &humans.id).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(message(&body), refusal(&humans.id), "{body}");

    let elsewhere = temporary(&server, &keys.agent).await;
    let (status, body) = keep(&server, &keys.agent, 2, &elsewhere.id).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = keep(&server, &keys.agent, 1, &elsewhere.id).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        message(&body),
        format!(
            "rows[0] cites result {}, which is already kept by another investigation",
            elsewhere.id
        )
    );

    let expired = temporary(&server, &keys.agent).await;
    let deadline = chrono::DateTime::parse_from_rfc3339(expired.expires_at.as_deref().unwrap())
        .unwrap()
        .with_timezone(&chrono::Utc);
    server.results_clock.set(deadline);
    let (status, body) = keep(&server, &keys.agent, 1, &expired.id).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(message(&body), refusal(&expired.id), "{body}");

    assert_eq!(read(&server, &keys.human, 1, "").await.investigation.seq, 0);
    assert_eq!(evidence(&server, 1).await.0, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_result_whose_file_is_missing_cannot_become_evidence() {
    let (server, keys) = boot_default().await;
    create(&server, &keys.agent, 1).await;
    let result = temporary(&server, &keys.agent).await;
    let store = &server.state.results.store;
    assert!(store.mark_file_missing(&result.id).await.unwrap());

    let (status, body) = keep(&server, &keys.agent, 1, &result.id).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        message(&body),
        format!(
            "rows[0] cites result {}, which is not kept evidence of this investigation",
            result.id
        ),
        "{body}"
    );
    assert_eq!(evidence(&server, 1).await, (0, 0), "nothing charged");
    assert_eq!(server.state.results.gate.evidence_reserved(), 0);
    let key_id = captured::key_id(&server, &keys.agent.token).await;
    let row = store.get(&result.id, key_id).await.unwrap().unwrap();
    assert_eq!(
        (row.state, row.investigation_id, row.file_missing),
        (ResultState::Temporary, None, true)
    );
    let page = read(&server, &keys.human, 1, "").await;
    assert_eq!(page.investigation.seq, 0, "nothing written");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_promotion_over_the_budget_leaves_the_result_temporary() {
    let (server, keys) = boot(
        CaptureConfig::default(),
        InvestigationConfig {
            evidence_per_investigation_bytes: 1,
            ..InvestigationConfig::default()
        },
    )
    .await;
    create(&server, &keys.agent, 1).await;
    let result = temporary(&server, &keys.agent).await;
    assert!(result.file_bytes > 1);

    let (status, body) = keep(&server, &keys.agent, 1, &result.id).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(code(&body), "evidence_budget_exceeded", "{body}");
    assert!(message(&body).contains("0 of 1 bytes"), "{body}");
    let key_id = captured::key_id(&server, &keys.agent.token).await;
    let row = server
        .state
        .results
        .store
        .get(&result.id, key_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (row.state, row.investigation_id),
        (ResultState::Temporary, None)
    );
    assert_eq!(
        captured::listed(&server, &keys.agent.token).await,
        std::slice::from_ref(&result.id)
    );
    assert_eq!(read(&server, &keys.human, 1, "").await.investigation.seq, 0);
}

const CORRELATION_FILE_BYTES: u64 = 4096;

fn correlation_capture(n: u32, key_id: i64, investigation: Option<InvestigationId>) -> NewCapture {
    let created_at = at(START);
    NewCapture {
        id: format!("{:032x}", 0x5167_0000_u32 + n),
        key_id,
        created_at,
        expires_at: created_at + chrono::TimeDelta::minutes(15),
        query: "10.0.4.17".into(),
        executed_query: "\"10.0.4.17\"".into(),
        source: ResultSource::Stored,
        shape: ResultShape::Events,
        earliest: None,
        latest: None,
        columns: vec![ResultColumn {
            name: "_time".into(),
            ty: ColumnType::Timestamp,
            stored_as: StoredAs::Native,
        }],
        column_facts: vec![ColumnFacts {
            nulls: 0,
            distinct: 1,
            distinct_capped: false,
            values: None,
            min: None,
            max: None,
        }],
        captured_rows: 1,
        total_rows: 1,
        wire_bytes: 30,
        file_bytes: CORRELATION_FILE_BYTES,
        digest: Some([7; 32]),
        format_version: 1,
        investigation_id: investigation,
        kind: CaptureKind::Sightings,
        correlation: Some(
            FactsDocument::encode(Correlation::Sightings(SightingsFacts {
                literal: "10.0.4.17".into(),
                matches_total: 1,
                captured: 1,
                attribution_fields: vec!["client_ip".into()],
                groups: vec![],
                fields: vec![],
                fields_omitted: 0,
                raw_only_rows: 0,
                denominators: SightingsDenominators {
                    field_counts_over: 1,
                    service_counts_over: 1,
                },
                follow_ups: vec![],
            }))
            .unwrap(),
        ),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_kept_correlation_result_counts_its_facts_document_in_the_evidence_budget() {
    let limit = CORRELATION_FILE_BYTES;
    let (server, keys) = boot(
        CaptureConfig::default(),
        InvestigationConfig {
            evidence_per_investigation_bytes: limit,
            ..InvestigationConfig::default()
        },
    )
    .await;
    create(&server, &keys.agent, 1).await;
    let key_id = captured::key_id(&server, &keys.agent.token).await;
    let row = correlation_capture(1, key_id, Some(investigation(1)));
    let stored = CORRELATION_FILE_BYTES + row.metadata_bytes();
    assert!(row.metadata_bytes() > 0);
    assert_eq!(
        server
            .state
            .results
            .store
            .insert_kept_unlogged(&row)
            .await
            .unwrap(),
        CaptureInsert::Inserted
    );
    assert_eq!(evidence(&server, 1).await, (stored, stored));

    let (status, _, body) = captured::capture(
        &server,
        &keys.agent.token,
        &attached(&corpus_query(), 1, &json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(code(&body), "evidence_budget_exceeded", "{body}");
    assert!(
        message(&body).contains(&format!("{stored} of {limit} bytes")),
        "{body}"
    );
    assert_eq!(kept_rows(&server, 1).await, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_promotion_of_a_correlation_result_charges_its_facts_document() {
    let facts = correlation_capture(0, 0, None).metadata_bytes();
    let limit = CORRELATION_FILE_BYTES + facts / 2;
    let (server, keys) = boot(
        CaptureConfig::default(),
        InvestigationConfig {
            evidence_per_investigation_bytes: limit,
            ..InvestigationConfig::default()
        },
    )
    .await;
    create(&server, &keys.agent, 1).await;
    let key_id = captured::key_id(&server, &keys.agent.token).await;
    let row = correlation_capture(2, key_id, None);
    let store = &server.state.results.store;
    assert_eq!(store.insert(&row).await.unwrap(), CaptureInsert::Inserted);

    let (status, body) = keep(&server, &keys.agent, 1, &row.id).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(code(&body), "evidence_budget_exceeded", "{body}");
    assert!(
        message(&body).contains(&format!("0 of {limit} bytes")),
        "{body}"
    );
    let got = store.get(&row.id, key_id).await.unwrap().unwrap();
    assert_eq!(
        (got.state, got.investigation_id, got.metadata_bytes),
        (ResultState::Temporary, None, facts)
    );
    assert_eq!(evidence(&server, 1).await, (0, 0));
    assert_eq!(read(&server, &keys.human, 1, "").await.investigation.seq, 0);

    server.results_clock.set(row.expires_at);
    let (status, _, body) = captured::page(&server, &keys.agent.token, &row.id, "").await;
    assert_eq!(status, StatusCode::GONE, "{body}");
    assert_eq!(code(&body), "result_gone", "{body}");
    let got = store.get(&row.id, key_id).await.unwrap().unwrap();
    assert_eq!(
        (got.state, got.correlation, got.metadata_bytes),
        (ResultState::Expired, None, 0)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_expiry_that_loses_the_row_to_a_promotion_leaves_its_file() {
    let (server, keys) = boot_default().await;
    create(&server, &keys.agent, 1).await;
    let result = temporary(&server, &keys.agent).await;
    let before = file_of(&server, &result.id);
    let deadline = chrono::DateTime::parse_from_rfc3339(result.expires_at.as_deref().unwrap())
        .unwrap()
        .with_timezone(&chrono::Utc);

    let hold = server.state.results.store.faults().hold_next_expiry();
    server.results_clock.set(deadline);
    let reading = {
        let server = Arc::clone(&server);
        let (token, id) = (keys.agent.token.clone(), result.id.clone());
        tokio::spawn(async move { captured::page(&server, &token, &id, "").await })
    };
    tokio::time::timeout(Duration::from_mins(1), hold.parked)
        .await
        .expect("the read reaches its expiry")
        .expect("the expiry parks");

    server.results_clock.set(at(START));
    let (status, body) = keep(&server, &keys.agent, 1, &result.id).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let _ = hold.release.send(());
    let (status, _, body) = reading.await.unwrap();
    assert_eq!(
        status,
        StatusCode::GONE,
        "the read answers what it found: {body}"
    );

    assert_eq!(file_of(&server, &result.id), before, "the file stays");
    server
        .results_clock
        .set(deadline + chrono::TimeDelta::days(1));
    let (status, _, page) = captured::page(&server, &keys.human.token, &result.id, "").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "kept evidence has no deadline: {page}"
    );
}

async fn verified(server: &TestServer, key: &MintedKey) -> fleet_auth::VerifiedKey {
    fleet_auth::KeyStore::from_pool(server.fleet_pool.clone())
        .verify_key(&key.token)
        .await
        .expect("a fixture token verifies")
}

fn keep_entry(result: &str) -> AppendEntry {
    serde_json::from_value(json!({
        "kind": "keep",
        "result_id": result,
        "rows": [0],
        "reason": "the first 502",
    }))
    .unwrap()
}

async fn hold_result_row(server: &TestServer, id: &str) -> sqlx::PgConnection {
    use sqlx::Connection as _;
    let mut conn = sqlx::PgConnection::connect(&server.app_db_url)
        .await
        .unwrap();
    sqlx::query("BEGIN").execute(&mut conn).await.unwrap();
    sqlx::query("SELECT 1 FROM captured_results WHERE id = $1 FOR UPDATE")
        .bind(id)
        .execute(&mut conn)
        .await
        .unwrap();
    conn
}

#[tokio::test(flavor = "multi_thread")]
async fn a_promotion_whose_request_goes_away_holds_its_charge_until_it_commits() {
    let (server, keys) = boot(
        CaptureConfig::default(),
        InvestigationConfig {
            evidence_installation_bytes: max_file_bytes(),
            ..InvestigationConfig::default()
        },
    )
    .await;
    create(&server, &keys.agent, 1).await;
    create(&server, &keys.agent, 2).await;
    let result = temporary(&server, &keys.agent).await;
    let bytes = result.file_bytes;
    let gate = &server.state.results.gate;
    let agent = verified(&server, &keys.agent).await;

    let mut held = hold_result_row(&server, &result.id).await;
    let request = {
        let server = Arc::clone(&server);
        let entry = keep_entry(&result.id);
        tokio::spawn(async move {
            trawl_server::investigations::service::append(&server.state, &agent, &id(1), entry)
                .await
        })
    };
    lock_waits(&server, "UPDATE captured_results%", 1).await;
    assert_eq!(gate.evidence_reserved(), bytes, "the promotion is charged");
    request.abort();
    assert!(
        request.await.unwrap_err().is_cancelled(),
        "the request's future was dropped mid-transaction"
    );
    assert_eq!(
        gate.evidence_reserved(),
        bytes,
        "the charge outlives the request"
    );

    let (status, _, body) = captured::capture(
        &server,
        &keys.agent.token,
        &attached(&corpus_query(), 2, &json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(code(&body), "evidence_budget_exceeded", "{body}");
    assert!(
        message(&body).contains(&format!("{bytes} of {} bytes", max_file_bytes())),
        "the usage counts the held charge: {body}"
    );

    sqlx::query("COMMIT").execute(&mut held).await.unwrap();
    let key_id = captured::key_id(&server, &keys.agent.token).await;
    let give_up = std::time::Instant::now() + Duration::from_mins(1);
    loop {
        let row = server
            .state
            .results
            .store
            .get(&result.id, key_id)
            .await
            .unwrap()
            .unwrap();
        if row.state == ResultState::Kept && gate.evidence_reserved() == 0 {
            assert_eq!(row.investigation_id, Some(investigation(1)));
            break;
        }
        assert!(
            std::time::Instant::now() < give_up,
            "the promotion never committed and settled: {row:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(evidence(&server, 1).await, (bytes, bytes));
    let page = read(&server, &keys.human, 1, "").await;
    assert_eq!(page.investigation.seq, 1, "{page:?}");
    assert!(
        matches!(&page.entries[0].payload, EntryPayload::Keep { result_id, .. } if *result_id == result.id),
        "{page:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_promotion_whose_commit_is_in_doubt_holds_its_charge_for_the_grace() {
    let (server, keys) = boot(
        CaptureConfig::default(),
        InvestigationConfig {
            evidence_installation_bytes: max_file_bytes(),
            ..InvestigationConfig::default()
        },
    )
    .await;
    create(&server, &keys.agent, 1).await;
    create(&server, &keys.agent, 2).await;
    let result = temporary(&server, &keys.agent).await;
    let bytes = result.file_bytes;
    let gate = &server.state.results.gate;
    let faults = server.state.results.store.faults();
    faults.fail_next_citing(InsertFault::Committed);
    let grace = faults.hold_next_grace();

    let (status, body) = keep(&server, &keys.agent, 1, &result.id).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(gate.evidence_reserved(), bytes, "the charge is held");
    tokio::time::timeout(Duration::from_mins(1), grace.parked)
        .await
        .expect("the grace begins")
        .expect("the grace parks");
    let (status, _, body) = captured::capture(
        &server,
        &keys.agent.token,
        &attached(&corpus_query(), 2, &json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        message(&body).contains(&format!("{} of {} bytes", 2 * bytes, max_file_bytes())),
        "the usage counts the committed row and the held charge: {body}"
    );

    let _ = grace.release.send(());
    tokio::time::timeout(Duration::from_mins(1), grace.settled)
        .await
        .expect("the promotion settles")
        .expect("the settle reports");
    assert_eq!(gate.evidence_reserved(), 0, "the charge settled");
    let settled = events_of("investigation_promotion_settled");
    assert_eq!(settled.len(), 1, "{settled:?}");
    assert_eq!(settled[0].field("outcome"), "\"committed\"");
    assert_eq!(evidence(&server, 1).await, (bytes, bytes));
    let page = read(&server, &keys.human, 1, "").await;
    assert_eq!(page.investigation.seq, 1, "{page:?}");
}

fn routed_keep(token: &str, n: u32, result: &str) -> axum::http::Request<axum::body::Body> {
    routed(
        token,
        "POST",
        &format!("/investigations/{}/entries", id(n)),
        Some(&serde_json::to_value(keep_entry(result)).unwrap()),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_promotion_whose_request_goes_away_keeps_its_request_count() {
    let (server, keys) = boot_default().await;
    create(&server, &keys.agent, 1).await;
    let result = temporary(&server, &keys.agent).await;
    let app = server.router_with(|http| http.max_concurrent_requests = 1);

    let mut held = hold(&server, 1).await;
    let request = support::send(&app, routed_keep(&keys.agent.token, 1, &result.id));
    lock_waits(&server, INVESTIGATION_LOCK, 1).await;
    request.abort();
    assert!(
        request.await.unwrap_err().is_cancelled(),
        "the request's future was dropped while the promotion waited"
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
    until_admitted(&app, &keys.agent.token).await;
    let page = read(&server, &keys.human, 1, "").await;
    assert_eq!(page.investigation.seq, 1, "{page:?}");
    assert!(
        matches!(&page.entries[0].payload, EntryPayload::Keep { result_id, .. } if *result_id == result.id),
        "{page:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_promotion_waits_for_its_investigation_no_later_than_its_deadline() {
    // Long enough for the capture before it, and for the test to see the
    // second promotion wait and drop its request before the deadline.
    let (server, keys) =
        boot_timing_out(CaptureConfig::default(), InvestigationConfig::default(), 3).await;
    create(&server, &keys.agent, 1).await;
    let result = temporary(&server, &keys.agent).await;
    let held = hold(&server, 1).await;

    let (status, body) =
        tokio::time::timeout(support::DEADLINE, keep(&server, &keys.agent, 1, &result.id))
            .await
            .expect("the keep answers by its deadline, with the lock still held");
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(code(&body), "capture_busy", "{body}");
    assert_eq!(server.state.results.gate.evidence_reserved(), 0);
    assert_eq!(
        lock_waiters(&server, INVESTIGATION_LOCK).await,
        0,
        "the answer came while the promotion still waited on the investigation"
    );

    let app = server.router_with(|http| http.max_concurrent_requests = 1);
    let request = support::send(&app, routed_keep(&keys.agent.token, 1, &result.id));
    lock_waits(&server, INVESTIGATION_LOCK, 1).await;
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    until_admitted(&app, &keys.agent.token).await;
    assert_eq!(
        lock_waiters(&server, INVESTIGATION_LOCK).await,
        0,
        "the request's count came back while the promotion still waited on the investigation"
    );

    drop(held);
    let page = read(&server, &keys.human, 1, "").await;
    assert_eq!(page.investigation.seq, 0, "neither keep wrote: {page:?}");
    let (status, _, page) = captured::page(&server, &keys.agent.token, &result.id, "").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["result"]["state"], "temporary", "{page}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_promotions_lock_waits_together_end_at_its_deadline() {
    const BUDGET: Duration = Duration::from_secs(5);
    // How long the investigation's lock is held of the budget: most of
    // it, while a fresh timeout of the whole budget on the result would
    // outlast the deadline by as much.
    const ON_THE_INVESTIGATION: Duration = Duration::from_secs(3);
    let (server, keys) = boot_timing_out(
        CaptureConfig::default(),
        InvestigationConfig::default(),
        BUDGET.as_secs(),
    )
    .await;
    create(&server, &keys.agent, 1).await;
    let result = temporary(&server, &keys.agent).await;
    let cited = hold_result(&server, &result.id).await;

    let mut investigation = hold(&server, 1).await;
    let started = tokio::time::Instant::now();
    let keeping = {
        let server = Arc::clone(&server);
        let token = keys.agent.token.clone();
        let result = result.id.clone();
        tokio::spawn(async move {
            let request = json!({
                "kind": "keep", "result_id": result, "rows": [0], "reason": "the first 502",
            });
            call(
                &server,
                &token,
                Method::POST,
                &format!("/investigations/{}/entries", id(1)),
                Some(&request),
            )
            .await
        })
    };
    lock_waits(&server, INVESTIGATION_LOCK, 1).await;
    tokio::time::sleep_until(started + ON_THE_INVESTIGATION).await;
    sqlx::query("COMMIT")
        .execute(&mut investigation)
        .await
        .unwrap();
    lock_waits(&server, PROMOTION_LOCK, 1).await;
    let (status, body) = tokio::time::timeout(support::DEADLINE, keeping)
        .await
        .expect("the keep answers")
        .unwrap();
    let answered = started.elapsed();
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(code(&body), "capture_busy", "{body}");
    assert_eq!(
        lock_waiters(&server, PROMOTION_LOCK).await,
        0,
        "the keep answered while its promotion still waited on the result"
    );
    assert!(
        answered < BUDGET + Duration::from_millis(1_500),
        "the waits outlasted the deadline: answered after {answered:?}"
    );
    assert_eq!(server.state.results.gate.evidence_reserved(), 0);

    let app = server.router_with(|http| http.max_concurrent_requests = 1);
    let mut investigation = hold(&server, 1).await;
    let started = tokio::time::Instant::now();
    let request = support::send(&app, routed_keep(&keys.agent.token, 1, &result.id));
    lock_waits(&server, INVESTIGATION_LOCK, 1).await;
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    tokio::time::sleep_until(started + ON_THE_INVESTIGATION).await;
    sqlx::query("COMMIT")
        .execute(&mut investigation)
        .await
        .unwrap();
    lock_waits(&server, PROMOTION_LOCK, 1).await;
    until_admitted(&app, &keys.agent.token).await;
    assert!(
        started.elapsed() >= BUDGET,
        "the request's count came back {:?} after the keep began, before its deadline",
        started.elapsed()
    );
    tokio::time::timeout(
        Duration::from_secs(1),
        lock_waits(&server, PROMOTION_LOCK, 0),
    )
    .await
    .expect("Postgres still waited on the result a second after the count came back");

    drop(cited);
    let page = read(&server, &keys.human, 1, "").await;
    assert_eq!(page.investigation.seq, 0, "neither keep wrote: {page:?}");
    let (status, _, page) = captured::page(&server, &keys.agent.token, &result.id, "").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["result"]["state"], "temporary", "{page}");
}

async fn boot_with_room_for_one() -> (Arc<TestServer>, Keys, trawl_api::results::CapturedResult) {
    let bytes = {
        let (server, keys) = boot_default().await;
        temporary(&server, &keys.agent).await.file_bytes
    };
    let (server, keys) = boot(
        CaptureConfig::default(),
        InvestigationConfig {
            evidence_per_investigation_bytes: bytes,
            ..InvestigationConfig::default()
        },
    )
    .await;
    create(&server, &keys.agent, 1).await;
    let result = temporary(&server, &keys.agent).await;
    assert_eq!(
        result.file_bytes, bytes,
        "a capture of the corpus is the same size every time"
    );
    (server, keys, result)
}

async fn kept_once(server: &TestServer, keys: &Keys, result: &trawl_api::results::CapturedResult) {
    let key_id = captured::key_id(server, &keys.agent.token).await;
    let row = server
        .state
        .results
        .store
        .get(&result.id, key_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (row.state, row.investigation_id),
        (ResultState::Kept, Some(investigation(1)))
    );
    assert_eq!(
        evidence(server, 1).await,
        (result.file_bytes, result.file_bytes),
        "the bytes are charged once"
    );
    assert_eq!(server.state.results.gate.evidence_reserved(), 0);
    let page = read(server, &keys.human, 1, "").await;
    assert_eq!(page.investigation.seq, 2, "{page:?}");
    assert!(
        page.entries.iter().all(
            |entry| matches!(&entry.payload, EntryPayload::Keep { result_id, .. } if *result_id == result.id)
        ),
        "{page:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_keep_that_resolves_before_another_promotion_commits_cites_it_at_no_charge() {
    let (server, keys, result) = boot_with_room_for_one().await;
    let gate = &server.state.results.gate;
    let agent = verified(&server, &keys.agent).await;
    let append = |entry: AppendEntry| {
        let server = Arc::clone(&server);
        let agent = agent.clone();
        tokio::spawn(async move {
            trawl_server::investigations::service::append(&server.state, &agent, &id(1), entry)
                .await
                .map(|response| response.entry.seq)
                .map_err(|e| e.to_string())
        })
    };

    let mut held = hold_result_row(&server, &result.id).await;
    let first = append(keep_entry(&result.id));
    lock_waits(&server, "UPDATE captured_results%", 1).await;
    assert_eq!(
        gate.evidence_reserved(),
        result.file_bytes,
        "the first promotion is charged"
    );
    let second = append(keep_entry(&result.id));
    wait_until(
        "the second keep to be refused and wait for the first",
        || second.is_finished() || gate.promotions_waiting() == 1,
    )
    .await;

    sqlx::query("COMMIT").execute(&mut held).await.unwrap();
    let first = first.await.unwrap();
    let second = second.await.unwrap();
    assert!(first.is_ok(), "{first:?}");
    assert!(
        second.is_ok(),
        "the second keep cites the result at no charge: {second:?}"
    );
    assert_eq!(gate.promotions_waiting(), 0);
    kept_once(&server, &keys, &result).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_finding_refused_beside_a_keep_of_one_of_its_results_promotes_the_rest() {
    let bytes = {
        let (server, keys) = boot_default().await;
        temporary(&server, &keys.agent).await.file_bytes
    };
    let (server, keys) = boot(
        CaptureConfig::default(),
        InvestigationConfig {
            evidence_per_investigation_bytes: 2 * bytes,
            ..InvestigationConfig::default()
        },
    )
    .await;
    create(&server, &keys.agent, 1).await;
    let first = temporary(&server, &keys.agent).await;
    let second = temporary(&server, &keys.agent).await;
    assert_eq!(
        (first.file_bytes, second.file_bytes),
        (bytes, bytes),
        "a capture of the corpus is the same size every time"
    );
    let gate = &server.state.results.gate;
    let agent = verified(&server, &keys.agent).await;
    let append = |entry: AppendEntry| {
        let server = Arc::clone(&server);
        let agent = agent.clone();
        tokio::spawn(async move {
            trawl_server::investigations::service::append(&server.state, &agent, &id(1), entry)
                .await
                .map(|response| response.entry.seq)
                .map_err(|e| e.to_string())
        })
    };
    let finding: AppendEntry = serde_json::from_value(json!({
        "kind": "finding",
        "statement": "the 502s start with the deploy",
        "status": "supported",
        "references": [
            { "result_id": first.id, "row": 0 },
            { "result_id": second.id, "row": 0 },
        ],
    }))
    .unwrap();

    let mut held = hold_result_row(&server, &first.id).await;
    let keeping = append(keep_entry(&first.id));
    lock_waits(&server, "UPDATE captured_results%", 1).await;
    assert_eq!(gate.evidence_reserved(), bytes, "the keep is charged");
    let finding = append(finding);
    wait_until("the finding to be refused and wait for the keep", || {
        finding.is_finished() || gate.promotions_waiting() == 1
    })
    .await;

    sqlx::query("COMMIT").execute(&mut held).await.unwrap();
    let keeping = keeping.await.unwrap();
    let finding = finding.await.unwrap();
    assert!(keeping.is_ok(), "{keeping:?}");
    assert!(
        finding.is_ok(),
        "the finding promotes the result the keep left: {finding:?}"
    );
    assert_eq!(gate.promotions_waiting(), 0);
    assert_eq!(gate.evidence_reserved(), 0);
    let key_id = captured::key_id(&server, &keys.agent.token).await;
    for result in [&first, &second] {
        let row = server
            .state
            .results
            .store
            .get(&result.id, key_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (row.state, row.investigation_id),
            (ResultState::Kept, Some(investigation(1))),
            "{}",
            result.id
        );
    }
    assert_eq!(
        evidence(&server, 1).await,
        (2 * bytes, 2 * bytes),
        "each result's bytes are charged once"
    );
    let page = read(&server, &keys.human, 1, "").await;
    assert_eq!(page.investigation.seq, 2, "{page:?}");
    assert!(
        page.entries
            .iter()
            .any(|entry| matches!(&entry.payload, EntryPayload::Finding { .. })),
        "{page:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn two_concurrent_keeps_of_one_result_charge_its_bytes_once() {
    let (server, keys, result) = boot_with_room_for_one().await;
    let ((first, first_body), (second, second_body)) = tokio::join!(
        keep(&server, &keys.agent, 1, &result.id),
        keep(&server, &keys.agent, 1, &result.id)
    );
    assert_eq!(first, StatusCode::CREATED, "{first_body}");
    assert_eq!(second, StatusCode::CREATED, "{second_body}");
    kept_once(&server, &keys, &result).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_note_on_a_due_investigation_unlinks_its_pruned_evidence() {
    let (server, keys) = boot_default().await;
    create(&server, &keys.agent, 1).await;
    let kept = captured::captured(
        &server,
        &keys.agent.token,
        &attached(&corpus_query(), 1, &json!({})),
    )
    .await
    .result;
    let retain_until = read(&server, &keys.human, 1, "")
        .await
        .investigation
        .retain_until;
    let later = at(START) + chrono::TimeDelta::days(8);
    assert!(
        at(&retain_until) < later,
        "the evidence is due by {later}: {retain_until}"
    );

    server.results_clock.set(later);
    let (status, body) = note(&server, &keys.agent, 1, "back after a week").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(
        captured::root_names(&server).is_empty(),
        "the pruned evidence's file is gone"
    );
    let (status, _, page) = captured::page(&server, &keys.human.token, &kept.id, "").await;
    assert_eq!(status, StatusCode::GONE, "{page}");
    let head = read(&server, &keys.human, 1, "").await.investigation;
    assert_eq!(
        at(&head.retain_until),
        later + chrono::TimeDelta::days(7),
        "the note slid the deadline after the prune"
    );
    assert_eq!(evidence(&server, 1).await.0, 0);
}

async fn boot_with_a_minute_of_room_for_one(
    budget: impl FnOnce(&mut InvestigationConfig, u64),
) -> (Arc<TestServer>, Keys, trawl_api::results::CapturedResult) {
    let bytes = {
        let (server, keys) = boot_default().await;
        temporary(&server, &keys.agent).await.file_bytes
    };
    let mut investigation = InvestigationConfig {
        evidence_idle_secs: 60,
        ..InvestigationConfig::default()
    };
    budget(&mut investigation, bytes);
    let (server, keys) = boot(CaptureConfig::default(), investigation).await;
    create(&server, &keys.agent, 1).await;
    let result = temporary(&server, &keys.agent).await;
    assert_eq!(
        result.file_bytes, bytes,
        "a capture of the corpus is the same size every time"
    );
    (server, keys, result)
}

async fn row_state(server: &TestServer, id: &str) -> String {
    let pool = common::fixture_pool(&server.app_db_url, 1).await;
    let state = sqlx::query_scalar("SELECT state FROM captured_results WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    pool.close().await;
    state
}

async fn keep_until_due(server: &TestServer, keys: &Keys, result: &str) {
    let (status, body) = keep(server, &keys.agent, 1, result).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let retain_until = read(server, &keys.human, 1, "")
        .await
        .investigation
        .retain_until;
    server.results_clock.set(at(&retain_until));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_due_investigations_evidence_does_not_count_against_its_budget() {
    let (server, keys, old) = boot_with_a_minute_of_room_for_one(|config, bytes| {
        config.evidence_per_investigation_bytes = bytes;
    })
    .await;
    let fresh = temporary(&server, &keys.agent).await;
    keep_until_due(&server, &keys, &old.id).await;
    assert_eq!(row_state(&server, &old.id).await, "kept", "not swept");

    let (status, body) = keep(&server, &keys.agent, 1, &fresh.id).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(row_state(&server, &old.id).await, "pruned");
    assert_eq!(row_state(&server, &fresh.id).await, "kept");
    let only_fresh = [format!("{}.parquet", fresh.id)];
    wait_until("the dead evidence's file to go", || {
        captured::root_names(&server) == only_fresh
    })
    .await;
    assert_eq!(
        evidence(&server, 1).await,
        (fresh.file_bytes, fresh.file_bytes)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_due_investigations_evidence_does_not_count_against_the_installations_budget() {
    let (server, keys, old) = boot_with_a_minute_of_room_for_one(|config, bytes| {
        config.evidence_installation_bytes = bytes;
    })
    .await;
    let fresh = temporary(&server, &keys.agent).await;
    keep_until_due(&server, &keys, &old.id).await;
    create(&server, &keys.agent, 2).await;

    let (status, body) = keep(&server, &keys.agent, 2, &fresh.id).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(row_state(&server, &fresh.id).await, "kept");
    assert_eq!(row_state(&server, &old.id).await, "pruned");
    let only_fresh = [format!("{}.parquet", fresh.id)];
    wait_until("the dead evidence's file to go", || {
        captured::root_names(&server) == only_fresh
    })
    .await;
    assert_eq!(
        evidence(&server, 2).await,
        (fresh.file_bytes, fresh.file_bytes)
    );
}

const PRUNE_LOCK: &str = "SELECT i.id FROM investigations AS i%";

async fn installation_kept(server: &TestServer) -> i64 {
    let pool = common::fixture_pool(&server.app_db_url, 1).await;
    let bytes = sqlx::query_scalar(
        "SELECT COALESCE(sum(file_bytes), 0)::bigint FROM captured_results
         WHERE state = 'kept'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    pool.close().await;
    bytes
}

#[tokio::test(flavor = "multi_thread")]
async fn a_write_dated_before_the_deadline_never_lifts_the_installation_over_its_budget() {
    let (server, keys, old) = boot_with_a_minute_of_room_for_one(|config, bytes| {
        config.evidence_installation_bytes = bytes;
    })
    .await;
    let fresh = temporary(&server, &keys.agent).await;
    let (status, body) = keep(&server, &keys.agent, 1, &old.id).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let deadline = at(&read(&server, &keys.human, 1, "")
        .await
        .investigation
        .retain_until);
    // Made half a minute in, investigation 2 is live at that deadline.
    server
        .results_clock
        .set(at(START) + chrono::TimeDelta::seconds(30));
    create(&server, &keys.agent, 2).await;

    server
        .results_clock
        .set(deadline - chrono::TimeDelta::microseconds(1));
    let mut held = hold(&server, 1).await;
    let noting = {
        let (server, token) = (Arc::clone(&server), keys.agent.token.clone());
        tokio::spawn(async move {
            call(
                &server,
                &token,
                Method::POST,
                &format!("/investigations/{}/entries", id(1)),
                Some(&json!({ "kind": "note", "text": "still looking" })),
            )
            .await
        })
    };
    lock_waits(&server, INVESTIGATION_LOCK, 1).await;

    server.results_clock.set(deadline);
    let keeping = {
        let (server, token, fresh) = (
            Arc::clone(&server),
            keys.agent.token.clone(),
            fresh.id.clone(),
        );
        tokio::spawn(async move {
            call(
                &server,
                &token,
                Method::POST,
                &format!("/investigations/{}/entries", id(2)),
                Some(&json!({
                    "kind": "keep",
                    "result_id": fresh,
                    "rows": [0],
                    "reason": "the first 502",
                })),
            )
            .await
        })
    };
    let waited = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if keeping.is_finished() {
                return false;
            }
            if lock_waiters(&server, PRUNE_LOCK).await == 1 {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the keep answers, or its pass waits on investigation 1");
    sqlx::query("COMMIT").execute(&mut held).await.unwrap();

    let (status, body) = noting.await.unwrap();
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (kept, body) = keeping.await.unwrap();
    let used = installation_kept(&server).await;
    assert!(
        used <= i64::try_from(old.file_bytes).unwrap(),
        "the installation keeps {used} bytes against a budget of {} ({kept}: {body})",
        old.file_bytes
    );
    assert!(waited, "the keep's pass waited to prune investigation 1");
    match kept {
        StatusCode::CONFLICT => {
            assert_eq!(code(&body), "evidence_budget_exceeded", "{body}");
            assert_eq!(row_state(&server, &old.id).await, "kept");
            assert_eq!(row_state(&server, &fresh.id).await, "temporary");
        }
        StatusCode::CREATED => {
            assert_eq!(row_state(&server, &old.id).await, "pruned");
            assert_eq!(row_state(&server, &fresh.id).await, "kept");
        }
        other => panic!("{other}: {body}"),
    }
}
