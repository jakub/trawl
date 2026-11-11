// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! One admission contract at every door that takes DSL (ADR-0024).
//!
//! The bind-time expansion budget and the field-list limit live in
//! `trawl-core` and are proven there. What this file proves is reach: the
//! same over-budget text is
//! refused with the same sentence whether it arrives as a query, an
//! export, a live tail, a validation call or a saved-query write, and a
//! refused write leaves the store exactly as it was. A call that emit
//! would refuse (an unknown name, a wrong argument count, a bad literal
//! argument) reaches the same doors, with its hint as a detail. The last
//! tests cover the row that is already out there — a saved query stored
//! before this door existed, which the scheduler must refuse per attempt
//! without turning the schedule off.

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

/// A call to `match`, which is an operator: the shared call check refuses
/// it in every stage, with the hint that names the operator.
const MATCH_CALL_DSL: &str = r#"last=1h | where match(message, "x")"#;

/// The sentence and the hint `trawl-core` refuses [`MATCH_CALL_DSL`] with.
const MATCH_CALL_SENTENCE: &str = "unknown function: match";
const MATCH_CALL_HINT: &str = "matches is an operator, not a function: where message matches /re/";

/// A `dedup` naming one field over the field-list limit of 64.
fn wide_dedup_dsl() -> String {
    let fields = (0..65).map(|i| format!("f{i}")).collect::<Vec<_>>();
    format!("* | dedup {}", fields.join(", "))
}

/// The whole sentence `trawl-core` refuses [`wide_dedup_dsl`] with.
const WIDE_SENTENCE: &str = "pipeline stage 1 (`dedup`) names 65 fields, over the limit of 64 for one `sort` or `dedup` stage";

fn raw_client() -> reqwest::Client {
    common::harness_client_builder()
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
    every_entry_point_refuses(&over_budget_dsl(), SENTENCE, None).await;
}

/// The field-list limit reaches the same doors, with its whole sentence.
#[tokio::test(flavor = "multi_thread")]
async fn every_entry_point_refuses_a_wide_field_list() {
    every_entry_point_refuses(&wide_dedup_dsl(), WIDE_SENTENCE, None).await;
}

/// A call the run would refuse is refused before it runs, at every door
/// (#435). `match` is refused everywhere, with its hint as a detail. A
/// wrong argument count and a bad call after `extract kv`, which once
/// passed validation and failed only when the run compiled, are invalid
/// at `/validate`.
#[tokio::test(flavor = "multi_thread")]
async fn every_entry_point_refuses_an_invalid_call() {
    every_entry_point_refuses(MATCH_CALL_DSL, MATCH_CALL_SENTENCE, Some(MATCH_CALL_HINT)).await;

    let server = common::setup().await;
    let http = raw_client();
    for (dsl, sentence) in [
        (
            "last=1h | where length() > 0",
            "invalid aggregation: length() requires exactly one argument",
        ),
        (
            "last=1h | extract kv | let r = nosuchfn(a)",
            "unknown function: nosuchfn",
        ),
    ] {
        assert_validate_refuses(
            &http,
            &server.url,
            &server.analyst_token,
            dsl,
            sentence,
            None,
        )
        .await;
    }
}

/// Send `dsl` to every door and require each to refuse it as a
/// `validation_error` that carries `sentence` and, as a detail, `hint`.
async fn every_entry_point_refuses(dsl: &str, sentence: &str, hint: Option<&str>) {
    let server = common::setup().await;
    let http = raw_client();
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
        .query(&[("query", dsl)])
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
            message.contains(sentence),
            "{door} carries the shared sentence, got: {message}"
        );
        let value: serde_json::Value = serde_json::from_str(&body).expect("JSON body");
        let error = &value["error"];
        assert_eq!(error["code"], "validation_error", "{door}: {body}");
        assert_eq!(
            error["details"][0]["hint"].as_str(),
            hint,
            "{door} carries the hint as a detail: {body}"
        );
    }

    // Validation reports the same refusal as its own successful answer:
    // being told the query is invalid is what a client asked this route
    // for, so it is a 200 whose body says no.
    assert_validate_refuses(&http, &server.url, token, dsl, sentence, hint).await;
}

/// `/validate` answers `dsl` with HTTP 200, `valid: false` and one error
/// that carries `sentence` and, as its own field, `hint`.
async fn assert_validate_refuses(
    http: &reqwest::Client,
    url: &str,
    token: &str,
    dsl: &str,
    sentence: &str,
    hint: Option<&str>,
) {
    let response = http
        .post(format!("{url}/api/v1/validate"))
        .bearer_auth(token)
        .json(&serde_json::json!({ "query": dsl }))
        .send()
        .await
        .expect("validate request");
    let status = response.status();
    let validation: serde_json::Value = response.json().await.expect("validation response");
    assert_eq!(status, 200, "{dsl}: validate answers 200: {validation}");
    assert_eq!(validation["valid"], serde_json::json!(false), "{dsl}");
    let errors = validation["errors"].as_array().expect("an errors array");
    assert_eq!(errors.len(), 1, "{dsl}: one refusal: {validation}");
    let message = errors[0]["message"].as_str().expect("an error message");
    assert!(
        message.contains(sentence),
        "{dsl}: validate carries the shared sentence, got: {message}"
    );
    assert_eq!(
        errors[0]["hint"].as_str(),
        hint,
        "{dsl}: validate carries the hint apart: {validation}"
    );
}

/// A time clause neither lane can hold is a parse error over that clause
/// (ADR-0017 and ADR-0061, amendments of 2026-11-08), and every door says
/// so the same way: `/query` and stream setup answer 400 with one body,
/// and `/validate`, whose refusal is its own 200 answer, carries that
/// body's detail as its error. Before the parser owned these refusals the
/// stream opened a live tail on `last=1h earliest="…"`, which `/query`
/// refused, and dropped a bound it could not read.
#[tokio::test(flavor = "multi_thread")]
async fn time_clause_refusals_match_across_doors() {
    let server = common::setup().await;
    let http = raw_client();
    let token = &server.analyst_token;

    let unreadable = "cannot read this time bound as a timestamp";
    let with_last = "cannot combine 'last=' with 'earliest='/'latest='";
    // (query, the clause the refusal covers, its sentence)
    let cases = [
        (
            r#"earliest="yesterday" service=api"#,
            r#"earliest="yesterday""#,
            unreadable,
        ),
        (
            r#"service=api latest="10000-01-01""#,
            r#"latest="10000-01-01""#,
            unreadable,
        ),
        (
            r#"service=api latest="300000-01-01""#,
            r#"latest="300000-01-01""#,
            unreadable,
        ),
        (
            r#"earliest="bad" earliest="2026-03-14""#,
            r#"earliest="bad""#,
            unreadable,
        ),
        (
            r#"last=1h earliest="2026-03-14""#,
            r#"earliest="2026-03-14""#,
            with_last,
        ),
        (
            r#"last=1h latest="2026-03-14" | stats count()"#,
            r#"latest="2026-03-14""#,
            with_last,
        ),
    ];

    let answer = |response: reqwest::Response| async move {
        let status = response.status();
        let body = response.text().await.expect("response body");
        let value: serde_json::Value = serde_json::from_str(&body)
            .unwrap_or_else(|e| panic!("body is not JSON ({e}): {body}"));
        (status, value)
    };
    for (dsl, clause, sentence) in cases {
        let at = dsl.find(clause).expect("the clause is in the text");
        let span = serde_json::json!({ "start": at, "end": at + clause.len() });

        let (query_status, query) = answer(
            http.post(format!("{}/api/v1/query", server.url))
                .bearer_auth(token)
                .json(&serde_json::json!({ "query": dsl }))
                .send()
                .await
                .expect("query request"),
        )
        .await;
        let (stream_status, stream) = answer(
            http.get(format!("{}/api/v1/stream", server.url))
                .query(&[("query", dsl)])
                .bearer_auth(token)
                .send()
                .await
                .expect("stream request"),
        )
        .await;
        let (validate_status, validation) = answer(
            http.post(format!("{}/api/v1/validate", server.url))
                .bearer_auth(token)
                .json(&serde_json::json!({ "query": dsl }))
                .send()
                .await
                .expect("validate request"),
        )
        .await;

        assert_eq!(query_status, 400, "{dsl}: /query refuses: {query}");
        assert_eq!(stream_status, 400, "{dsl}: stream setup refuses: {stream}");
        assert_eq!(stream, query, "{dsl}: /query and stream answer one body");
        let error = &query["error"];
        assert_eq!(error["code"], "parse_error", "{dsl}: {query}");
        assert_eq!(error["message"], sentence, "{dsl}: {query}");
        let details = error["details"].as_array().expect("the envelope's details");
        assert_eq!(details.len(), 1, "{dsl}: one refusal: {query}");
        let detail = &details[0];
        assert_eq!(detail["message"], sentence, "{dsl}");
        assert_eq!(detail["span"], span, "{dsl}: the span covers {clause}");
        assert!(detail["hint"].is_string(), "{dsl}: a hint names the fix");

        assert_eq!(validate_status, 200, "{dsl}: {validation}");
        assert_eq!(validation["valid"], false, "{dsl}: {validation}");
        assert_eq!(
            validation["errors"],
            serde_json::json!([detail]),
            "{dsl}: /validate reports the same detail"
        );
    }
}

/// A refused write is not a partial write: the create leaves no row and
/// the update leaves the stored text alone.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_saved_write_leaves_the_store_untouched() {
    refused_saved_write_leaves_the_store_untouched(&over_budget_dsl()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_wide_saved_write_leaves_the_store_untouched() {
    refused_saved_write_leaves_the_store_untouched(&wide_dedup_dsl()).await;
}

/// A saved write whose text calls a function the run would refuse is
/// refused before it is stored (#435).
#[tokio::test(flavor = "multi_thread")]
async fn invalid_call_writes_leave_saved_queries_unchanged() {
    refused_saved_write_leaves_the_store_untouched(MATCH_CALL_DSL).await;
}

/// Create and update a saved query with `dsl`, which the door refuses,
/// and require the store to read as it did before.
async fn refused_saved_write_leaves_the_store_untouched(dsl: &str) {
    let server = common::setup().await;
    let http = raw_client();
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
        pool: ExecutorPool::for_test(
            data_dir.to_str().expect("utf-8 data dir").to_owned(),
            2,
            100_000,
            None,
        )
        .unwrap(),
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
    stored_query_fails_its_attempt_and_keeps_its_schedule(&over_budget_dsl(), SENTENCE).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stored_wide_query_fails_its_attempt_and_keeps_its_schedule() {
    stored_query_fails_its_attempt_and_keeps_its_schedule(&wide_dedup_dsl(), WIDE_SENTENCE).await;
}

/// A saved query stored before the call check existed calls `match`. Its
/// next attempt fails with the diagnostic, and its schedule survives.
#[tokio::test(flavor = "multi_thread")]
async fn a_stored_invalid_call_fails_its_attempt_and_keeps_its_schedule() {
    stored_query_fails_its_attempt_and_keeps_its_schedule(MATCH_CALL_DSL, MATCH_CALL_SENTENCE)
        .await;
}

/// Store `dsl` behind the write-time door, schedule it, run one tick, and
/// require a failed run carrying `sentence` and a schedule still enabled.
async fn stored_query_fails_its_attempt_and_keeps_its_schedule(dsl: &str, sentence: &str) {
    let h = scheduler_harness().await;
    let t0 = Utc.with_ymd_and_hms(2026, 3, 14, 3, 0, 0).unwrap();

    // Straight through the store: the write-time door is exactly what a
    // row this old never passed.
    let saved = h
        .saved
        .create(h.key_id, "legacy", dsl)
        .await
        .expect("store the refused saved query");
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
        error.contains(sentence),
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
