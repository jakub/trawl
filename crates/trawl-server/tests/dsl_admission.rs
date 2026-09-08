// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! One admission contract at every door that takes DSL (ADR-0024).
//!
//! The bind-time expansion budget lives in `trawl-core` and is proven
//! there. What this file proves is reach: the same over-budget text is
//! refused with the same sentence whether it arrives as a query, an
//! export, a live tail, a validation call or a saved-query write, and a
//! refused write leaves the store exactly as it was. The last test covers
//! the row that is already out there — a saved query stored before this
//! door existed, which the scheduler must refuse per attempt without
//! turning the schedule off.

mod common;

use chrono::{TimeZone as _, Utc};
use fleet_auth::{KeyStore, PrincipalKind};
use sqlx::PgPool;
use trawl_server::config::SchedulerConfig;
use trawl_server::pool::ExecutorPool;
use trawl_server::scheduler::poll_and_execute;
use trawl_server::store::{RunStatus, SavedQueryStore, ScheduleStore, StorageState};

/// A pipeline over the alias-expansion budget: each assignment names the
/// previous one twice, so the binder's work doubles per link while the
/// text grows by a few bytes. Twelve links is far past 512.
fn over_budget_dsl() -> String {
    use std::fmt::Write as _;
    let mut dsl = String::from("* | let a0 = 1");
    for i in 1..=12 {
        let _ = write!(dsl, ", a{i} = a{} + a{}", i - 1, i - 1);
    }
    dsl
}

/// The part of `trawl-core`'s refusal every door must be saying.
const SENTENCE: &str = "alias-expansion budget";

fn raw_client() -> reqwest::Client {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .expect("build the test HTTP client")
}

/// The message an error response carries, or a panic naming the body.
fn envelope_message(body: &str) -> String {
    let value: serde_json::Value = serde_json::from_str(body)
        .unwrap_or_else(|e| panic!("error body is not JSON ({e}): {body}"));
    value
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("error body carries no message: {body}"))
        .to_owned()
}

// ---------------------------------------------------------------------------
// the HTTP doors
// ---------------------------------------------------------------------------

/// Query, both export formats, the SSE stream and both saved-query writes
/// refuse the same text, and every one of them says the same thing.
#[tokio::test(flavor = "multi_thread")]
async fn every_entry_point_refuses_the_same_over_budget_dsl() {
    let server = common::setup().await;
    let http = raw_client();
    let dsl = over_budget_dsl();
    let token = &server.analyst_token;

    // A saved query to aim the update at. Its own DSL is admitted.
    let created: serde_json::Value = http
        .post(format!("{}/api/v1/saved", server.url))
        .bearer_auth(token)
        .json(&serde_json::json!({ "name": "admitted", "query": "* | head 1" }))
        .send()
        .await
        .expect("create the saved query")
        .json()
        .await
        .expect("saved query response");
    let saved_id = created["id"].as_i64().expect("saved query id");

    let post = |path: String, body: serde_json::Value| {
        let http = http.clone();
        let token = token.clone();
        async move {
            http.post(path)
                .bearer_auth(token)
                .json(&body)
                .send()
                .await
                .expect("request")
        }
    };

    let query = post(
        format!("{}/api/v1/query", server.url),
        serde_json::json!({ "query": dsl }),
    )
    .await;
    let csv = post(
        format!("{}/api/v1/export?format=csv", server.url),
        serde_json::json!({ "query": dsl }),
    )
    .await;
    let parquet = post(
        format!("{}/api/v1/export?format=parquet", server.url),
        serde_json::json!({ "query": dsl }),
    )
    .await;
    let create = post(
        format!("{}/api/v1/saved", server.url),
        serde_json::json!({ "name": "refused", "query": dsl }),
    )
    .await;
    let stream = http
        .get(format!("{}/api/v1/stream", server.url))
        .query(&[("query", dsl.as_str())])
        .bearer_auth(token)
        .send()
        .await
        .expect("stream request");
    let update = http
        .put(format!("{}/api/v1/saved/{saved_id}", server.url))
        .bearer_auth(token)
        .json(&serde_json::json!({ "query": dsl }))
        .send()
        .await
        .expect("update request");

    for (door, response) in [
        ("query", query),
        ("export csv", csv),
        ("export parquet", parquet),
        ("saved create", create),
        ("stream", stream),
        ("saved update", update),
    ] {
        let status = response.status();
        let body = response.text().await.expect("response body");
        assert_eq!(status, 400, "{door} refuses with 400: {body}");
        let message = envelope_message(&body);
        assert!(
            message.contains(SENTENCE),
            "{door} carries the shared sentence, got: {message}"
        );
    }

    // Validation reports the same refusal as its own successful answer:
    // being told the query is invalid is what a client asked this route
    // for, so it is a 200 whose body says no.
    let validation: serde_json::Value = http
        .post(format!("{}/api/v1/validate", server.url))
        .bearer_auth(token)
        .json(&serde_json::json!({ "query": dsl }))
        .send()
        .await
        .expect("validate request")
        .json()
        .await
        .expect("validation response");
    assert_eq!(validation["valid"], serde_json::json!(false));
    let detail = serde_json::to_string(&validation["errors"]).expect("errors render");
    assert!(
        detail.contains(SENTENCE),
        "validate carries the shared sentence, got: {detail}"
    );
}

/// A refused write is not a partial write: the create leaves no row and
/// the update leaves the stored text alone.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_saved_write_leaves_the_store_untouched() {
    let server = common::setup().await;
    let http = raw_client();
    let dsl = over_budget_dsl();
    let token = &server.analyst_token;
    let original = "service=nginx | head 3";

    let created: serde_json::Value = http
        .post(format!("{}/api/v1/saved", server.url))
        .bearer_auth(token)
        .json(&serde_json::json!({ "name": "keeper", "query": original }))
        .send()
        .await
        .expect("create the saved query")
        .json()
        .await
        .expect("saved query response");
    let saved_id = created["id"].as_i64().expect("saved query id");

    let refused_create = http
        .post(format!("{}/api/v1/saved", server.url))
        .bearer_auth(token)
        .json(&serde_json::json!({ "name": "never-stored", "query": dsl }))
        .send()
        .await
        .expect("create request");
    assert_eq!(refused_create.status(), 400);

    let refused_update = http
        .put(format!("{}/api/v1/saved/{saved_id}", server.url))
        .bearer_auth(token)
        .json(&serde_json::json!({ "name": "renamed", "query": dsl }))
        .send()
        .await
        .expect("update request");
    assert_eq!(refused_update.status(), 400);

    let listing: serde_json::Value = http
        .get(format!("{}/api/v1/saved", server.url))
        .bearer_auth(token)
        .send()
        .await
        .expect("list request")
        .json()
        .await
        .expect("listing response");
    let queries = listing["queries"]
        .as_array()
        .expect("the listing carries an array");

    assert_eq!(queries.len(), 1, "the refused create stored nothing");
    let row = &queries[0];
    assert_eq!(row["id"].as_i64(), Some(saved_id));
    assert_eq!(
        row["name"].as_str(),
        Some("keeper"),
        "the refused update did not rename the row"
    );
    assert_eq!(
        row["query"].as_str(),
        Some(original),
        "the refused update did not rewrite the DSL"
    );
}

// ---------------------------------------------------------------------------
// the scheduler
// ---------------------------------------------------------------------------

/// Everything one scheduler tick needs, and nothing this file does not
/// assert on: no fixture corpus, because the query never reaches `DuckDB`.
struct SchedulerHarness {
    _dir: tempfile::TempDir,
    pool: ExecutorPool,
    key_store: KeyStore,
    key_id: i64,
    saved: SavedQueryStore,
    schedules: ScheduleStore,
    config: SchedulerConfig,
    _fleet_pool: PgPool,
    // Holds the app-state advisory lock and the pool the stores came from.
    _storage: StorageState,
}

async fn scheduler_harness() -> SchedulerHarness {
    let fleet_db_url = common::create_fleet_database().await;
    let app_db_url = common::create_app_database().await;

    let fleet_pool = common::fleet_pool(&fleet_db_url).await;
    let key_store = common::fleet_keystore(&fleet_pool).await;
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
    std::fs::create_dir_all(&data_dir).expect("create the data root");

    SchedulerHarness {
        pool: ExecutorPool::new(
            data_dir.to_str().expect("utf-8 data dir").to_owned(),
            2,
            100_000,
            None,
        ),
        _dir: dir,
        key_id: key.info.id,
        key_store,
        saved: storage.saved.clone(),
        schedules: storage.schedule.clone(),
        config: SchedulerConfig::default(),
        _fleet_pool: fleet_pool,
        _storage: storage,
    }
}

/// A saved query stored before the door existed still runs on a schedule.
/// The attempt fails with the diagnostic, and the schedule survives it:
/// disabling it would be the server editing an operator's configuration
/// over a query the operator can repair.
#[tokio::test(flavor = "multi_thread")]
async fn a_stored_over_budget_query_fails_its_attempt_and_keeps_its_schedule() {
    let h = scheduler_harness().await;
    let t0 = Utc.with_ymd_and_hms(2026, 3, 14, 3, 0, 0).unwrap();

    // Straight through the store: the write-time door is exactly what a
    // row this old never passed.
    let saved = h
        .saved
        .create(h.key_id, "legacy", &over_budget_dsl())
        .await
        .expect("store the over-budget saved query");
    h.schedules
        .create_schedule(saved.id, h.key_id, 3600, None, None, 0, t0)
        .await
        .expect("create the schedule");

    for handle in poll_and_execute(
        &h.schedules,
        &h.key_store,
        &h.pool,
        &h.config,
        30,
        t0 + chrono::Duration::seconds(1),
    )
    .await
    {
        handle.await.expect("a scheduled execution must not panic");
    }

    let runs = h
        .schedules
        .list_runs(saved.id, h.key_id, 10, 0)
        .await
        .expect("list runs");
    assert_eq!(runs.len(), 1, "the attempt was made and recorded");
    let run = &runs[0];
    assert_eq!(run.status, RunStatus::Error, "{run:?}");
    let error = run
        .error_message
        .as_deref()
        .expect("a failed run records why");
    assert!(
        error.contains(SENTENCE),
        "the run records the sentence: {error}"
    );

    let schedule = h
        .schedules
        .get_schedule_for_saved_query(saved.id, h.key_id)
        .await
        .expect("read the schedule")
        .expect("the schedule still exists");
    assert!(
        schedule.enabled,
        "a refused attempt does not disable the schedule"
    );
    assert!(
        schedule.next_fire_at > t0,
        "the cursor advanced to the next fire: {:?}",
        schedule.next_fire_at
    );
}
