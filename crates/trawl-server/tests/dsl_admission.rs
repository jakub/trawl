// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! One admission contract at every door that takes DSL (ADR-0024).

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

const SENTENCE: &str = "alias-expansion budget";

const MATCH_CALL_DSL: &str = r#"last=1h | where match(message, "x")"#;

const MATCH_CALL_SENTENCE: &str = "unknown function: match";
const MATCH_CALL_HINT: &str = "matches is an operator, not a function: where message matches /re/";

fn wide_dedup_dsl() -> String {
    let fields = (0..65).map(|i| format!("f{i}")).collect::<Vec<_>>();
    format!("* | dedup {}", fields.join(", "))
}

const WIDE_SENTENCE: &str = "pipeline stage 1 (`dedup`) names 65 fields, over the limit of 64 for one `sort` or `dedup` stage";

fn raw_client() -> reqwest::Client {
    common::harness_client_builder()
        .build()
        .expect("build the test HTTP client")
}

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

#[tokio::test(flavor = "multi_thread")]
async fn every_entry_point_refuses_the_same_over_budget_dsl() {
    every_entry_point_refuses(&over_budget_dsl(), SENTENCE, None).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn every_entry_point_refuses_a_wide_field_list() {
    every_entry_point_refuses(&wide_dedup_dsl(), WIDE_SENTENCE, None).await;
}

/// Form encoding keeps only `[A-Za-z0-9*-._]` raw, so a 64 KiB query of
/// brackets or `+` would grow past http's 65 534-byte URI limit and never
/// reach the door. A URI query may carry `( ) * =` raw, and the server's
/// query extractor splits each pair at its first `=`, so only the other
/// bytes are percent-encoded.
fn stream_url(base: &str, dsl: &str) -> String {
    use std::fmt::Write as _;
    let mut url = format!("{base}/api/v1/stream?query=");
    for byte in dsl.bytes() {
        if byte.is_ascii_alphanumeric() || b"()*=._-".contains(&byte) {
            url.push(char::from(byte));
        } else {
            let _ = write!(url, "%{byte:02X}");
        }
    }
    url
}

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

async fn every_entry_point_refuses(dsl: &str, sentence: &str, hint: Option<&str>) {
    let server = common::setup().await;
    for (door, body) in every_entry_point_refuses_on(&server, dsl, sentence).await {
        let error = &body["error"];
        assert_eq!(error["code"], "validation_error", "{door}: {body}");
        assert_eq!(
            error["details"][0]["hint"].as_str(),
            hint,
            "{door} carries the hint as a detail: {body}"
        );
    }

    let http = raw_client();
    assert_validate_refuses(
        &http,
        &server.url,
        &server.analyst_token,
        dsl,
        sentence,
        hint,
    )
    .await;
}

async fn every_entry_point_refuses_on(
    server: &common::TestServer,
    dsl: &str,
    sentence: &str,
) -> Vec<(&'static str, serde_json::Value)> {
    let http = raw_client();
    let token = &server.analyst_token;

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
        .get(stream_url(&server.url, dsl))
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

    let mut answers = Vec::new();
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
        answers.push((door, serde_json::from_str(&body).expect("a JSON body")));
    }
    answers
}

async fn assert_validate_refuses(
    http: &reqwest::Client,
    url: &str,
    token: &str,
    dsl: &str,
    sentence: &str,
    hint: Option<&str>,
) -> serde_json::Value {
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
    validation
}

#[tokio::test(flavor = "multi_thread")]
async fn time_clause_refusals_match_across_doors() {
    let server = common::setup().await;
    let http = raw_client();
    let token = &server.analyst_token;

    let unreadable = "cannot read this time bound as a timestamp";
    let with_last = "cannot combine 'last=' with 'earliest='/'latest='";
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

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_saved_write_leaves_the_store_untouched() {
    refused_saved_write_leaves_the_store_untouched(&over_budget_dsl()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_wide_saved_write_leaves_the_store_untouched() {
    refused_saved_write_leaves_the_store_untouched(&wide_dedup_dsl()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_call_writes_leave_saved_queries_unchanged() {
    refused_saved_write_leaves_the_store_untouched(MATCH_CALL_DSL).await;
}

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

/// Everything one scheduler tick needs, and nothing this file does not
/// assert on: no fixture corpus, because a refused query never reaches
/// `DuckDB`, and a sound one finds no files and succeeds with no rows.
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

/// Wait until every scheduled-run lease is free. A run's preparation job
/// keeps the lease until the job's thread ends, which can come after the
/// run's handle resolves.
async fn scheduled_leases_free(pool: &ExecutorPool) {
    trawl_server::test_timing::until("every scheduled lease is free", || {
        pool.scheduled_leases_available() == pool.scheduled_lease_limit()
    })
    .await;
}

/// The harness pool leases one scheduled run at a time, so a tick claims
/// at most one due schedule, and a tick can claim none while one is still
/// due: a claim can give up, or the last run's lease can still be held.
/// So the ticks go on until every schedule has a run, under the hang
/// guard, and the caller's run counts catch a schedule that ran twice.
async fn tick_until_each_ran(h: &SchedulerHarness, scheduled: &[i64], now: chrono::DateTime<Utc>) {
    let deadline = std::time::Instant::now() + trawl_server::test_timing::HANG_GUARD;
    loop {
        let mut waiting = false;
        for id in scheduled {
            let runs = h
                .schedules
                .list_runs(*id, h.key_id, 1, 0)
                .await
                .expect("list runs");
            waiting |= runs.is_empty();
        }
        if !waiting {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "every due schedule ran within the hang guard"
        );
        scheduled_leases_free(&h.pool).await;
        let handles =
            poll_and_execute(&h.schedules, &h.key_store, &h.pool, &h.config, 30, now).await;
        if handles.is_empty() {
            tokio::task::yield_now().await;
        }
        for handle in handles {
            handle.await.expect("a scheduled execution must not panic");
        }
    }
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

#[tokio::test(flavor = "multi_thread")]
async fn a_stored_over_budget_query_fails_its_attempt_and_keeps_its_schedule() {
    stored_query_fails_its_attempt_and_keeps_its_schedule(&over_budget_dsl(), SENTENCE).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stored_wide_query_fails_its_attempt_and_keeps_its_schedule() {
    stored_query_fails_its_attempt_and_keeps_its_schedule(&wide_dedup_dsl(), WIDE_SENTENCE).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stored_invalid_call_fails_its_attempt_and_keeps_its_schedule() {
    stored_query_fails_its_attempt_and_keeps_its_schedule(MATCH_CALL_DSL, MATCH_CALL_SENTENCE)
        .await;
}

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

const DEPTH_SENTENCE: &str = "deeper than 64 levels";

/// `a == 0 or a == 1 …`: `n` comparisons, `n` levels deep.
fn or_chain(n: usize) -> String {
    (0..n)
        .map(|i| format!("a == {i}"))
        .collect::<Vec<_>>()
        .join(" or ")
}

fn deep_where_dsl() -> String {
    format!("* | where {}", or_chain(65))
}

/// A search of 30 000 nested brackets, 60 004 bytes.
fn deep_brackets_dsl() -> String {
    format!("{}a=1 {}", "(".repeat(30_000), ")".repeat(30_000))
}

/// A 32 000-term chain, 64 013 bytes: under the 64 KiB text limit, and
/// under http's URI limit as a stream URL, because `*` needs no encoding.
fn long_chain_dsl() -> String {
    format!("* | where {} > 0", vec!["1"; 32_000].join("*"))
}

fn deep_dsls() -> [(String, &'static str); 3] {
    [
        (deep_where_dsl(), "expression is deeper than 64 levels"),
        (deep_brackets_dsl(), "search is deeper than 64 levels"),
        (long_chain_dsl(), "expression is deeper than 64 levels"),
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn every_entry_point_refuses_deep_query_trees() {
    for (dsl, message) in deep_dsls() {
        // A fixture per text: the door helper names its saved query, and
        // names are unique per key.
        let server = common::setup().await;
        let client = trawl_client::HttpClient::new_insecure(&server.url, &server.analyst_token)
            .expect("build the client");
        let head = &dsl[..dsl.len().min(24)];
        let mut answers = every_entry_point_refuses_on(&server, &dsl, DEPTH_SENTENCE).await;

        let (status, _, capture) = common::captured::capture(
            &server,
            &server.analyst_token,
            &serde_json::json!({ "query": dsl }),
        )
        .await;
        assert_eq!(status, 400, "{head}…: capture refuses: {capture}");
        answers.push(("capture", capture));

        let (_, query) = &answers[0];
        let detail = &query["error"]["details"][0];
        assert_eq!(detail["message"], message, "{head}…: {query}");
        assert!(detail["hint"].is_string(), "{head}…: a hint names the fix");
        for (door, body) in &answers {
            let error = &body["error"];
            assert_eq!(error["code"], "parse_error", "{head}…: {door}: {body}");
            assert_eq!(error["message"], message, "{head}…: {door}: {body}");
            assert_eq!(
                error["details"],
                serde_json::json!([detail]),
                "{head}…: {door} carries the one detail /query does"
            );
        }
        let validation = assert_validate_refuses(
            &raw_client(),
            &server.url,
            &server.analyst_token,
            &dsl,
            message,
            detail["hint"].as_str(),
        )
        .await;
        assert_eq!(
            validation["errors"],
            serde_json::json!([detail]),
            "{head}…: /validate reports the same detail"
        );

        let ordinary = client
            .query_paginated("service=nginx | head 1", None, None)
            .await
            .expect("trawld still answers an ordinary query");
        assert_eq!(ordinary.result.row_count(), 1, "after {head}…");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_deep_saved_write_leaves_the_store_untouched() {
    for (dsl, _) in deep_dsls() {
        refused_saved_write_leaves_the_store_untouched(&dsl).await;
    }
}

/// The comments count the levels at `d` = 64; each
/// shape keeps its truth at any `d` of the same parity.
fn limit_queries(d: usize) -> [(String, &'static str); 5] {
    [
        // 64 `+` terms are 63 levels; the comparison is the 64th.
        (format!("* | where {} > 0", vec!["1"; d].join(" + ")), "*"),
        // 63 minus signs over a bracketed 1, which the grammar keeps as a
        // literal under the signs, and the comparison over them. An odd
        // run is -1, under 2.
        (format!("* | where {}(1) < 2", "-".repeat(d - 1)), "*"),
        // 63 comparisons of an absent field, which are null, and one true
        // one.
        (format!("* | where {} or 1 == 1", or_chain(d - 1)), "*"),
        // An even run of NOTs keeps the match.
        (
            format!("{}service=nginx", "NOT ".repeat(d)),
            "service=nginx",
        ),
        (
            format!("{}service=nginx {}", "(".repeat(d), ")".repeat(d)),
            "service=nginx",
        ),
    ]
}

/// The parse and every walk after it run on trawld's own threads, so this
/// is the check of trawld's base stack at the limit.
#[tokio::test(flavor = "multi_thread")]
async fn queries_at_the_tree_depth_limit_run_in_trawld() {
    let server = common::setup().await;
    let client = trawl_client::HttpClient::new_insecure(&server.url, &server.analyst_token)
        .expect("build the client");
    let http = raw_client();

    for ((deep, shallow), (over, _)) in limit_queries(64).into_iter().zip(limit_queries(65)) {
        let head = &deep[..deep.len().min(24)];
        let refused = http
            .post(format!("{}/api/v1/query", server.url))
            .bearer_auth(&server.analyst_token)
            .json(&serde_json::json!({ "query": over }))
            .send()
            .await
            .expect("query request");
        assert_eq!(refused.status(), 400, "{head}…: depth 65 is refused");
        let message = envelope_message(&refused.text().await.expect("response body"));
        assert!(message.contains(DEPTH_SENTENCE), "{head}…: {message}");

        let expected = client
            .query_paginated(shallow, None, None)
            .await
            .expect("the shallow query runs")
            .result
            .row_count();
        assert!(expected > 0, "{shallow} selects rows in the fixture corpus");
        let answer = client
            .query_paginated(&deep, None, None)
            .await
            .unwrap_or_else(|e| panic!("{head}… at depth 64 runs: {e}"));
        assert_eq!(answer.result.row_count(), expected, "{head}…");

        let stream = http
            .get(stream_url(&server.url, &deep))
            .bearer_auth(&server.analyst_token)
            .send()
            .await
            .expect("stream request");
        assert_eq!(stream.status(), 200, "{head}…: stream setup at depth 64");
        let content_type = stream
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(
            content_type.starts_with("text/event-stream"),
            "{head}…: {content_type}"
        );
        drop(stream);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stored_deep_query_fails_its_attempt_and_keeps_its_schedule() {
    let h = scheduler_harness().await;
    let t0 = Utc.with_ymd_and_hms(2026, 3, 14, 3, 0, 0).unwrap();
    let interval = 3600;

    // Straight through the store: no door this row ever passed knew the
    // limit.
    let mut deep = Vec::new();
    for (i, (dsl, message)) in deep_dsls().into_iter().enumerate() {
        let saved = h
            .saved
            .create(h.key_id, &format!("legacy-{i}"), &dsl)
            .await
            .expect("store the deep saved query");
        h.schedules
            .create_schedule(saved.id, h.key_id, interval, None, None, 0, t0)
            .await
            .expect("schedule the deep query");
        deep.push((saved.id, message));
    }
    let sound = h
        .saved
        .create(h.key_id, "sound", "* | head 1")
        .await
        .expect("store the sound saved query");
    h.schedules
        .create_schedule(sound.id, h.key_id, interval, None, None, 0, t0)
        .await
        .expect("schedule the sound query");

    let scheduled: Vec<i64> = deep.iter().map(|(id, _)| *id).chain([sound.id]).collect();
    tick_until_each_ran(&h, &scheduled, t0 + chrono::Duration::seconds(1)).await;

    for (saved_id, message) in &deep {
        let runs = h
            .schedules
            .list_runs(*saved_id, h.key_id, 10, 0)
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
            error.contains(message),
            "the run records the sentence: {error}"
        );

        let schedule = h
            .schedules
            .get_schedule_for_saved_query(*saved_id, h.key_id)
            .await
            .expect("read the schedule")
            .expect("the schedule still exists");
        assert!(
            schedule.enabled,
            "a refused attempt does not disable the schedule"
        );
        assert_eq!(
            schedule.next_fire_at,
            t0 + chrono::Duration::seconds(interval.try_into().expect("an hour")),
            "the cursor advanced one interval"
        );
    }
    let runs = h
        .schedules
        .list_runs(sound.id, h.key_id, 10, 0)
        .await
        .expect("list runs");
    assert_eq!(runs.len(), 1, "the sound query ran");
    assert_eq!(runs[0].status, RunStatus::Success, "{:?}", runs[0]);

    manual_run_of_a_stored_deep_query_fails().await;
}

/// The fixture spawns no scheduler loop, so the only run is the one this
/// asks for.
async fn manual_run_of_a_stored_deep_query_fails() {
    let server = common::setup().await;
    let client = trawl_client::HttpClient::new_insecure(&server.url, &server.analyst_token)
        .expect("build the client");

    let probe = client
        .create_saved("probe", "* | head 1")
        .await
        .expect("create a saved query");
    let app = common::app_pool(&server.app_db_url).await;
    let (key_id,): (i64,) = sqlx::query_as("SELECT key_id FROM saved_queries WHERE id = $1")
        .bind(probe.id)
        .fetch_one(&app)
        .await
        .expect("read the key id");

    let saved = server
        .state
        .storage
        .saved
        .create(key_id, "legacy", &deep_where_dsl())
        .await
        .expect("store the deep saved query");
    server
        .state
        .storage
        .schedule
        .create_schedule(saved.id, key_id, 3600, None, None, 0, Utc::now())
        .await
        .expect("schedule the deep query");

    let started = client
        .trigger_run(saved.id)
        .await
        .expect("the run is claimed");
    assert_eq!(started.status, "running");

    let deadline = std::time::Instant::now() + trawl_server::test_timing::HANG_GUARD;
    let run = loop {
        let runs = client
            .list_report_runs(saved.id, Some(10), None)
            .await
            .expect("list runs")
            .runs;
        if let Some(run) = runs.into_iter().find(|r| r.status != "running") {
            break run;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the manual run did not finish within the hang guard"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    assert_eq!(run.status, "error", "{run:?}");
    let error = run.error_message.as_deref().unwrap_or_default();
    assert!(
        error.contains("expression is deeper than 64 levels"),
        "the manual run records the sentence: {error}"
    );
}
