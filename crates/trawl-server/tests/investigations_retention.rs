// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The evidence clock end to end (ADR-0059, amending ADR-0018).

mod common;

use std::sync::Arc;

use chrono::{DateTime, TimeDelta, Utc};
use common::captured::{self, Restartable, at};
use common::investigating::{attached, call, code, create, id, message, read, settled};
use common::{MintedKey, TestServer, mint_key};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use trawl_api::investigations::{EntryKind, EntryPayload, InvestigationId, StepOutcomeKind};
use trawl_config::CaptureConfig;
use trawl_server::config::RateLimitConfig;
use trawl_server::results::root::UnlinkHold;
use trawl_server::results::service::GoneReason;
use trawl_server::results::sweep::sweep;
use trawl_server::store::{RetentionRule, StoreActor};

/// Where the clock stands when a test captures: half an hour into the
/// capture corpus's hour.
const START: &str = "2024-01-16T10:30:00Z";

struct Keys {
    agent: MintedKey,
    human: MintedKey,
}

async fn boot() -> (Restartable, TestServer, Keys) {
    let (fixture, server) = Restartable::boot(
        RateLimitConfig {
            default_rpm: 0,
            ..RateLimitConfig::default()
        },
        CaptureConfig::default(),
        |data| {
            captured::seed_hour(data, captured::SERVICE, &captured::corpus());
        },
    )
    .await;
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
    (fixture, server, keys)
}

async fn kept_capture(server: &TestServer, key: &MintedKey, n: u32) -> String {
    captured::captured(
        server,
        &key.token,
        &attached(&format!("service={}", captured::SERVICE), n, &json!({})),
    )
    .await
    .result
    .id
}

async fn append(server: &TestServer, key: &MintedKey, n: u32, body: &Value) -> Value {
    let (status, answer) = call(
        server,
        &key.token,
        Method::POST,
        &format!("/investigations/{}/entries", id(n)),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    answer
}

async fn note(server: &TestServer, key: &MintedKey, n: u32, text: &str) {
    append(server, key, n, &json!({ "kind": "note", "text": text })).await;
}

async fn deadlines(server: &TestServer, keys: &Keys, n: u32) -> (DateTime<Utc>, DateTime<Utc>) {
    let head = read(server, &keys.human, n, "").await.investigation;
    (at(&head.retain_until), at(&head.retain_ceiling))
}

async fn board(server: &TestServer, keys: &Keys, n: u32) -> Vec<EntryKind> {
    let mut entries = read(server, &keys.human, n, "").await.entries;
    entries.sort_by_key(|entry| entry.seq);
    entries.iter().map(|entry| entry.payload.kind()).collect()
}

fn file_exists(server: &TestServer, id: &str) -> bool {
    server.state.results.root.path_of(id).unwrap().exists()
}

async fn assert_retention_ended(server: &TestServer, keys: &Keys, id: &str, what: &str) {
    let (status, _, body) =
        captured::page(server, &keys.human.token, id, "?offset=0&limit=1").await;
    assert_eq!(status, StatusCode::GONE, "{what}: {body}");
    assert_eq!(code(&body), "result_gone", "{what}: {body}");
    assert_eq!(
        message(&body),
        GoneReason::RetentionEnded.message(),
        "{what}: {body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn kept_evidence_outlives_a_restart_and_reads_gone_after_its_retention_ends() {
    let (fixture, server, keys) = boot().await;
    create(&server, &keys.agent, 1).await;
    let result = kept_capture(&server, &keys.agent, 1).await;
    settled(&server, &keys.human, 1, 1).await;
    append(
        &server,
        &keys.agent,
        1,
        &json!({
            "kind": "keep",
            "result_id": result,
            "rows": [0, 1],
            "reason": "the first two 502s",
        }),
    )
    .await;
    append(
        &server,
        &keys.agent,
        1,
        &json!({
            "kind": "finding",
            "statement": "the 502s start with the deploy",
            "status": "supported",
            "references": [
                { "result_id": result, "row": 0 },
                { "result_id": result, "row": 1 },
            ],
        }),
    )
    .await;
    let cited = [EntryKind::Step, EntryKind::Keep, EntryKind::Finding];

    let server = fixture.restart(server).await;
    let (status, _, page) = captured::page(&server, &keys.human.token, &result, "").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "kept evidence after a restart: {page}"
    );
    assert_eq!(
        page["page"]["rows"].as_array().map(Vec::len),
        Some(captured::ROWS),
        "{page}"
    );
    assert_eq!(board(&server, &keys, 1).await, cited);

    let (retain_until, _) = deadlines(&server, &keys, 1).await;
    let past = retain_until + TimeDelta::seconds(1);
    server.results_clock.set(past);
    let swept = sweep(&server.state.results, past).await;
    assert_eq!((swept.evidence_pruned, swept.unlinked), (1, 1), "{swept:?}");
    assert_retention_ended(&server, &keys, &result, "after the sweep").await;
    assert!(!file_exists(&server, &result), "the sweep left the file");
    assert_eq!(board(&server, &keys, 1).await, cited);
    let step = settled(&server, &keys.human, 1, 1).await;
    let EntryPayload::Step { outcome, .. } = &step.payload else {
        panic!("not a step: {step:?}");
    };
    assert_eq!(
        outcome.as_ref().map(|o| o.outcome),
        Some(StepOutcomeKind::Success)
    );

    let again = sweep(&server.state.results, past).await;
    assert_eq!((again.evidence_pruned, again.unlinked), (0, 0), "{again:?}");
    assert_retention_ended(&server, &keys, &result, "after a second sweep").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_alone_ends_evidence_past_its_retention() {
    let (_fixture, server, keys) = boot().await;
    create(&server, &keys.agent, 2).await;
    let result = kept_capture(&server, &keys.agent, 2).await;
    settled(&server, &keys.human, 2, 1).await;
    assert!(file_exists(&server, &result));

    let (retain_until, _) = deadlines(&server, &keys, 2).await;
    server
        .results_clock
        .set(retain_until + TimeDelta::seconds(1));
    assert_retention_ended(&server, &keys, &result, "the first read").await;
    assert!(!file_exists(&server, &result), "the read left the file");
    assert_eq!(board(&server, &keys, 2).await, [EntryKind::Step]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_a_write_overtakes_answers_the_retention_ended() {
    let (_fixture, server, keys) = boot().await;
    let server = Arc::new(server);
    create(&server, &keys.agent, 5).await;
    let result = kept_capture(&server, &keys.agent, 5).await;
    settled(&server, &keys.human, 5, 1).await;
    assert!(file_exists(&server, &result));

    let unlink = server.state.results.root.hold_next_unlink();
    overtaken_read(&server, &keys, 5, &result, Some(unlink)).await;
    assert!(!file_exists(&server, &result), "the note left the file");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_zero_column_read_a_write_overtakes_answers_the_retention_ended() {
    let (server, keys) = boot_empty().await;
    let server = Arc::new(server);
    create(&server, &keys.agent, 6).await;
    let captured = captured::captured(
        &server,
        &keys.agent.token,
        &attached(&format!("service={}", captured::SERVICE), 6, &json!({})),
    )
    .await
    .result;
    assert!(captured.columns.is_empty(), "{captured:?}");
    settled(&server, &keys.human, 6, 1).await;

    overtaken_read(&server, &keys, 6, &captured.id, None).await;
}

async fn boot_empty() -> (TestServer, Keys) {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let server = common::setup_with_capture(
        tmp.path(),
        data.to_str().unwrap().to_owned(),
        RateLimitConfig {
            default_rpm: 0,
            ..RateLimitConfig::default()
        },
        CaptureConfig::default(),
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
    (server, keys)
}

async fn overtaken_read(
    server: &Arc<TestServer>,
    keys: &Keys,
    n: u32,
    result: &str,
    unlink: Option<UnlinkHold>,
) {
    let (retain_until, _) = deadlines(server, keys, n).await;
    let now = retain_until + TimeDelta::seconds(1);
    server.results_clock.set(now);
    let check = server.state.results.store.faults().hold_next_evidence_end();
    let read = {
        let server = Arc::clone(server);
        let token = keys.human.token.clone();
        let result = result.to_owned();
        tokio::spawn(
            async move { captured::page(&server, &token, &result, "?offset=0&limit=1").await },
        )
    };
    tokio::time::timeout(std::time::Duration::from_mins(1), check.parked)
        .await
        .expect("the read reaches its locked check")
        .expect("the check parks");

    let write = {
        let server = Arc::clone(server);
        let token = keys.agent.token.clone();
        tokio::spawn(async move {
            let (status, body) = call(
                &server,
                &token,
                Method::POST,
                &format!("/investigations/{}/entries", id(n)),
                Some(&json!({ "kind": "note", "text": "overtakes the read" })),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED, "{body}");
        })
    };
    // The note has committed once it is answered, or, when it has a file
    // to unlink, once it reaches the unlink.
    let mut write = Some(write);
    let release_unlink = if let Some(hold) = unlink {
        tokio::time::timeout(std::time::Duration::from_mins(1), hold.parked)
            .await
            .expect("the note reaches its unlink")
            .expect("the unlink parks");
        Some(hold.release)
    } else {
        write_done(write.take().expect("the note is in flight")).await;
        None
    };
    let _ = check.release.send(());

    let (status, _, body) = tokio::time::timeout(std::time::Duration::from_mins(1), read)
        .await
        .expect("the read answers")
        .unwrap();
    assert_eq!(status, StatusCode::GONE, "{body}");
    assert_eq!(code(&body), "result_gone", "{body}");
    assert_eq!(
        message(&body),
        GoneReason::RetentionEnded.message(),
        "{body}"
    );
    if let Some(release) = release_unlink {
        let _ = release.send(());
    }
    if let Some(write) = write {
        write_done(write).await;
    }
    assert!(
        deadlines(server, keys, n).await.0 > now,
        "the note slid the deadline past the read's clock"
    );
}

async fn write_done(write: tokio::task::JoinHandle<()>) {
    tokio::time::timeout(std::time::Duration::from_mins(1), write)
        .await
        .expect("the note answers")
        .expect("the note's task did not panic");
}

#[tokio::test(flavor = "multi_thread")]
async fn each_change_slides_the_deadline_until_the_ceiling() {
    let (_fixture, server, keys) = boot().await;
    let created = at(&create(&server, &keys.agent, 3).await.created_at);
    let result = kept_capture(&server, &keys.agent, 3).await;
    settled(&server, &keys.human, 3, 1).await;
    let idle = TimeDelta::days(7);
    let (_, ceiling) = deadlines(&server, &keys, 3).await;
    assert_eq!(ceiling, created + TimeDelta::days(30));

    let day = created + TimeDelta::days(1);
    server.results_clock.set(day);
    note(&server, &keys.agent, 3, "a day in").await;
    assert_eq!(deadlines(&server, &keys, 3).await.0, day + idle);

    for days in [7, 13, 19, 25] {
        server.results_clock.set(created + TimeDelta::days(days));
        note(&server, &keys.agent, 3, "still looking").await;
    }
    let late = ceiling - TimeDelta::hours(1);
    server.results_clock.set(late);
    note(&server, &keys.agent, 3, "an hour before the ceiling").await;
    assert_eq!(deadlines(&server, &keys, 3).await.0, ceiling);
    let (status, _, page) = captured::page(&server, &keys.human.token, &result, "").await;
    assert_eq!(status, StatusCode::OK, "evidence at the ceiling: {page}");

    server.results_clock.set(ceiling + TimeDelta::seconds(1));
    note(&server, &keys.agent, 3, "past the ceiling").await;
    assert!(!file_exists(&server, &result), "the write left the file");
    assert_retention_ended(&server, &keys, &result, "past the ceiling").await;
    assert_eq!(deadlines(&server, &keys, 3).await.0, ceiling);
    assert_eq!(
        board(&server, &keys, 3).await.last(),
        Some(&EntryKind::Note)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_boot_interrupts_a_dead_step_and_leaves_the_clock() {
    let (fixture, server, keys) = boot().await;
    create(&server, &keys.agent, 4).await;
    // The step a process appended at admission and never completed: the
    // store write the capture route makes, with no work behind it.
    let now = server.results_clock.now();
    server
        .state
        .storage
        .investigations
        .begin_step(
            &InvestigationId::parse(&id(4)).unwrap(),
            &StoreActor {
                key_id: captured::key_id(&server, &keys.agent.token).await,
                prefix: keys.agent.prefix.clone(),
                name: keys.agent.name.clone(),
            },
            &EntryPayload::Step {
                executed: format!("service={}", captured::SERVICE),
                bounds: None,
                request_id: "dead-process".into(),
                query_id: None,
                purpose: None,
                outcome: None,
            },
            now,
            RetentionRule {
                idle: TimeDelta::days(7),
            },
            trawl_server::deadline::Deadline::after(std::time::Duration::from_mins(1)),
        )
        .await
        .unwrap();
    let before = read(&server, &keys.human, 4, "").await.investigation;
    server.results_clock.set(now + TimeDelta::days(2));

    let server = fixture.restart(server).await;
    let step = settled(&server, &keys.human, 4, 1).await;
    let EntryPayload::Step { outcome, .. } = &step.payload else {
        panic!("not a step: {step:?}");
    };
    assert_eq!(
        outcome.as_ref().map(|o| o.outcome),
        Some(StepOutcomeKind::Interrupted)
    );
    assert_eq!(outcome.as_ref().and_then(|o| o.total_rows), None);
    let after = read(&server, &keys.human, 4, "").await.investigation;
    assert_eq!(after.seq, before.seq + 1);
    assert_eq!(
        (after.retain_until, after.retain_ceiling),
        (before.retain_until, before.retain_ceiling)
    );
}
