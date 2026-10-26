// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The investigation routes end to end (ADR-0059), through the fixture's
//! HTTPS listener with real fleet keys: an agent key that writes, a human
//! key that only reads, an operator key with `server_manage`, a key with
//! `query` alone and one with every investigation permission but `query`.
//!
//! Kept evidence is made in two halves: the agent captures over HTTP, then
//! the store's fixture promotion moves the capture into the investigation
//! inside its lock, with no log entry, so each test's sequences count its
//! own writes alone. Promotion through a keep, and attached captures, have
//! their own suites (`investigations_evidence`, `investigations_attach`).
//! Captures resolve their window at the fixture's results clock, set onto
//! the capture corpus of `common::captured`, 2024-01-16.
//!
//! A test that asserts a warning records the process's tracing events, so
//! each test runs in its own process: nextest's. The binary is in the
//! ADR-0021 `postgres` nextest group.

mod common;

#[path = "../../fleet-auth/tests/support/answer_hold.rs"]
mod answer_hold;

use std::sync::OnceLock;

use chrono::{DateTime, TimeDelta, Utc};
use common::audit_capture::Capture;
use common::captured::{self, at};
use common::investigating::{call, code, create, creation, id, message, read, routed};
use common::{MintedKey, TestServer, mint_key};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use trawl_api::investigations::{
    AppendResponse, EntryKind, EntryPayload, Investigation, InvestigationId, InvestigationList,
    InvestigationStatus,
};
use trawl_api::results::ResultState;
use trawl_config::CaptureConfig;
use trawl_server::config::RateLimitConfig;
use trawl_server::investigations::service::{ACTIVITY_LIMIT, READ_MAX_LIMIT};
use trawl_server::results::service::GoneReason;
use trawl_server::store::{InvestigationStore, RetentionRule, StoreActor};

/// The instant the fixture's clock starts at: half an hour into the
/// capture corpus's hour.
const START: &str = "2024-01-16T10:30:00Z";

/// The keys a test speaks as.
struct Keys {
    /// Reads and writes.
    agent: MintedKey,
    /// Reads only.
    human: MintedKey,
    /// Writes, extends and deletes.
    operator: MintedKey,
    /// `query` alone.
    query_only: MintedKey,
    /// Every investigation permission and `server_manage`, but no `query`.
    no_query: MintedKey,
}

/// The tracing events of the whole process. Installed once, by the test
/// that asserts a warning: the server logs from every tokio worker.
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

async fn boot() -> (TestServer, Keys) {
    let server = captured::boot(
        RateLimitConfig {
            default_rpm: 0,
            ..RateLimitConfig::default()
        },
        CaptureConfig::default(),
    )
    .await;
    server.results_clock.set(at(START));
    let pool = &server.fleet_pool;
    let keys = Keys {
        agent: mint_key(
            pool,
            "agent",
            &[
                "query",
                "schema_read",
                "validate",
                "investigation_read",
                "investigation_write",
            ],
        )
        .await,
        human: mint_key(pool, "human", &["query", "investigation_read"]).await,
        operator: mint_key(
            pool,
            "operator",
            &[
                "query",
                "investigation_read",
                "investigation_write",
                "server_manage",
            ],
        )
        .await,
        query_only: mint_key(pool, "query-only", &["query"]).await,
        no_query: mint_key(
            pool,
            "no-query",
            &["investigation_read", "investigation_write", "server_manage"],
        )
        .await,
    };
    (server, keys)
}

/// [`call`] with a raw request body sent as `application/json`, which need
/// not be JSON, and the answer's text unparsed: a plain-text answer must
/// fail the caller's assertion, not this helper.
async fn call_raw(
    server: &TestServer,
    token: &str,
    method: Method,
    path: &str,
    body: Option<&str>,
) -> (StatusCode, String) {
    let client = common::harness_client_builder().build().unwrap();
    let mut request = client
        .request(method, format!("{}/api/v1{path}", server.url))
        .bearer_auth(token);
    if let Some(body) = body {
        request = request
            .header("content-type", "application/json")
            .body(body.to_owned());
    }
    let response = request.send().await.unwrap();
    let status = response.status();
    (status, response.text().await.unwrap())
}

/// How long a refusal of a request whose body never ends may take. A
/// failure bound, never an ordering device: a refusal that needs no byte
/// of the body is answered at once, and a route that waits on the body
/// never answers.
const STALLED_ANSWER_WITHIN: std::time::Duration = std::time::Duration::from_secs(10);

/// `method path` as `token` with a JSON body that never ends: a chunk that
/// announces 16 bytes and sends 8, then nothing, on a connection the
/// client keeps open. The status and body, or `None` when no answer came
/// while the body was open.
async fn stalled(
    server: &TestServer,
    token: &str,
    method: &Method,
    path: &str,
) -> Option<(StatusCode, String)> {
    let host = server.url.strip_prefix("https://").unwrap();
    let request = format!(
        "{method} /api/v1{path} HTTP/1.1\r\nhost: {host}\r\n\
         authorization: Bearer {token}\r\ncontent-type: application/json\r\n\
         transfer-encoding: chunked\r\n\r\n10\r\n{{\"expected\""
    );
    let response =
        common::raw_https_exchange_within(&server.url, request.as_bytes(), STALLED_ANSWER_WITHIN)
            .await?;
    Some((
        StatusCode::from_u16(response.status).unwrap(),
        String::from_utf8_lossy(&response.body).into_owned(),
    ))
}

/// The ADR error envelope `text` is, or a failure naming what came instead.
fn envelope(text: &str, what: &str) -> Value {
    serde_json::from_str(text)
        .unwrap_or_else(|e| panic!("{what}: the answer is not the JSON envelope ({e}): {text}"))
}

/// Append `entry` to investigation `n` as `key`.
async fn append(
    server: &TestServer,
    key: &MintedKey,
    n: u32,
    entry: &Value,
) -> (StatusCode, Value) {
    call(
        server,
        &key.token,
        Method::POST,
        &format!("/investigations/{}/entries", id(n)),
        Some(entry),
    )
    .await
}

/// [`append`] that must answer 201.
async fn appended(server: &TestServer, key: &MintedKey, n: u32, entry: &Value) -> AppendResponse {
    let (status, body) = append(server, key, n, entry).await;
    assert_eq!(status, StatusCode::CREATED, "{entry}: {body}");
    serde_json::from_value(body).expect("an append response")
}

/// A capture of the corpus's 20 rows as `key`; its result id.
async fn capture(server: &TestServer, key: &MintedKey) -> String {
    let query = format!("service={} last=1h", captured::SERVICE);
    let result = captured::captured(server, &key.token, &json!({ "query": query }))
        .await
        .result;
    assert_eq!(result.captured_rows, captured::ROWS as u64);
    result.id
}

/// A capture as `key` promoted into investigation `n`: kept evidence.
async fn kept(server: &TestServer, key: &MintedKey, n: u32) -> String {
    let result = capture(server, key).await;
    let key_id = captured::key_id(server, &key.token).await;
    let promoted = server
        .state
        .storage
        .results
        .promote_unlogged(
            &result,
            key_id,
            &InvestigationId::parse(&id(n)).unwrap(),
            server.results_clock.now(),
        )
        .await
        .expect("promote");
    assert!(promoted, "the capture is promoted");
    result
}

/// The store's actor for `key`, for writes a test makes directly.
async fn store_actor(server: &TestServer, key: &MintedKey) -> StoreActor {
    StoreActor {
        key_id: captured::key_id(server, &key.token).await,
        prefix: key.prefix.clone(),
        name: key.name.clone(),
    }
}

fn rule() -> RetentionRule {
    RetentionRule {
        idle: TimeDelta::days(7),
    }
}

fn note(text: &str) -> Value {
    json!({ "kind": "note", "text": text })
}

fn instant(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text)
        .unwrap()
        .with_timezone(&Utc)
}

// -- authority ----------------------------------------------------------------

/// Every route against an id no investigation has, as every key: a key
/// without the route's authority gets 403, never 404, so it learns nothing
/// about which ids exist; a key with it reaches the lookup and gets 404.
#[tokio::test(flavor = "multi_thread")]
async fn every_route_refuses_a_key_without_its_authority_before_any_lookup() {
    let (server, keys) = boot().await;
    let unknown = id(0xdead);
    let routes: Vec<(&str, Method, String, Option<Value>, u8)> = vec![
        // (name, method, path, body, authority: 0 read, 1 write, 2 manage)
        ("list", Method::GET, "/investigations".into(), None, 0),
        (
            "read",
            Method::GET,
            format!("/investigations/{unknown}?since=0"),
            None,
            0,
        ),
        (
            "append",
            Method::POST,
            format!("/investigations/{unknown}/entries"),
            Some(note("n")),
            1,
        ),
        (
            "patch",
            Method::PATCH,
            format!("/investigations/{unknown}"),
            Some(json!({ "expected_seq": 0, "status": "closed" })),
            1,
        ),
        (
            "finding status",
            Method::PUT,
            format!("/investigations/{unknown}/findings/1/status"),
            Some(json!({ "expected_seq": 1, "status": "supported" })),
            1,
        ),
        (
            "retention",
            Method::PUT,
            format!("/investigations/{unknown}/retention"),
            Some(json!({ "expected_seq": 0, "retain_until": "2030-01-01T00:00:00Z" })),
            2,
        ),
        (
            "delete",
            Method::DELETE,
            format!("/investigations/{unknown}"),
            None,
            2,
        ),
    ];
    // (key, authority it holds: None for none)
    let principals: [(&str, &str, Option<u8>); 6] = [
        ("grantless", &server.coastwatch_only_token, None),
        ("query only", &keys.query_only.token, None),
        ("no query", &keys.no_query.token, None),
        ("human", &keys.human.token, Some(0)),
        ("agent", &keys.agent.token, Some(1)),
        ("operator", &keys.operator.token, Some(2)),
    ];
    for (who, token, holds) in principals {
        for (route, method, path, body, needs) in &routes {
            let (status, answer) = call(&server, token, method.clone(), path, body.as_ref()).await;
            let allowed = holds.is_some_and(|holds| holds >= *needs);
            let expected = match (allowed, *route) {
                (false, _) => StatusCode::FORBIDDEN,
                (true, "list") => StatusCode::OK,
                (true, _) => StatusCode::NOT_FOUND,
            };
            assert_eq!(status, expected, "{who} {route}: {answer}");
            if expected == StatusCode::NOT_FOUND {
                assert_eq!(code(&answer), "not_found", "{who} {route}: {answer}");
            }
            if expected == StatusCode::FORBIDDEN {
                assert_eq!(code(&answer), "forbidden", "{who} {route}: {answer}");
            }
        }
        // Create on a fresh id: 403 without write, 201 with it.
        let fresh = format!(
            "/investigations/{:032x}",
            0xc0de_u32 + u32::from(holds.unwrap_or(9))
        );
        let (status, answer) = call(&server, token, Method::PUT, &fresh, Some(&creation())).await;
        let expected = if holds.is_some_and(|holds| holds >= 1) {
            StatusCode::CREATED
        } else {
            StatusCode::FORBIDDEN
        };
        assert_eq!(status, expected, "{who} create: {answer}");
    }
}

/// A malformed id is refused for its shape, after the authority check: 400
/// for a writer, 403 for a key without the authority, whatever it sent.
#[tokio::test(flavor = "multi_thread")]
async fn a_malformed_id_is_judged_only_after_the_authority() {
    let (server, keys) = boot().await;
    let (status, answer) = call(
        &server,
        &keys.agent.token,
        Method::POST,
        "/investigations/not-an-id/entries",
        Some(&note("n")),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
    assert!(message(&answer).contains("32 lowercase hex"), "{answer}");
    let (status, _) = call(
        &server,
        &keys.human.token,
        Method::POST,
        "/investigations/not-an-id/entries",
        Some(&json!({ "not": "an entry" })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "the shape is not judged first"
    );
}

/// Authority first, shape second, lookup third, on every investigation
/// route: a key without the route's authority gets the 403 envelope for a
/// path, query or body that does not parse (an id that is not UTF-8 once
/// decoded, a segment or parameter of the wrong type, a body that is not
/// JSON or not the route's shape), so it never learns whether its request
/// parsed. A key with the authority gets the 400 envelope for the same
/// request, never axum's plain-text rejection.
///
/// On a route that reads a body, a key without the authority gets the 403
/// envelope before the body is read at all: a body that never ends is
/// refused while it is still open.
#[allow(clippy::too_many_lines)]
#[tokio::test(flavor = "multi_thread")]
async fn a_key_without_the_authority_never_learns_whether_its_request_parsed() {
    let (server, keys) = boot().await;
    let unknown = id(0xdead);
    let valid = |body: &Value| Some(body.to_string());
    let creation = creation();
    let entry = note("n");
    let patch = json!({ "expected_seq": 0, "status": "closed" });
    let finding = json!({ "expected_seq": 1, "status": "supported" });
    let extension = json!({ "expected_seq": 0, "retain_until": "2030-01-01T00:00:00Z" });
    // (route, method, path, a body that parses, authority: 0 read, 1 write,
    // 2 manage)
    let routes: Vec<(&str, Method, String, Option<String>, u8)> = vec![
        (
            "read",
            Method::GET,
            format!("/investigations/{unknown}"),
            None,
            0,
        ),
        (
            "create",
            Method::PUT,
            format!("/investigations/{unknown}"),
            valid(&creation),
            1,
        ),
        (
            "append",
            Method::POST,
            format!("/investigations/{unknown}/entries"),
            valid(&entry),
            1,
        ),
        (
            "patch",
            Method::PATCH,
            format!("/investigations/{unknown}"),
            valid(&patch),
            1,
        ),
        (
            "finding status",
            Method::PUT,
            format!("/investigations/{unknown}/findings/1/status"),
            valid(&finding),
            1,
        ),
        (
            "retention",
            Method::PUT,
            format!("/investigations/{unknown}/retention"),
            valid(&extension),
            2,
        ),
        (
            "delete",
            Method::DELETE,
            format!("/investigations/{unknown}"),
            None,
            2,
        ),
    ];
    // (what is malformed, route, method, path, body)
    let mut cases: Vec<(String, Method, String, Option<String>, u8)> = Vec::new();
    for (route, method, path, body, needs) in &routes {
        // The id: invalid UTF-8 once decoded.
        let bad_id = path.replacen(&unknown, "%FF", 1);
        cases.push((
            format!("{route}: id %FF"),
            method.clone(),
            bad_id,
            body.clone(),
            *needs,
        ));
        if body.is_some() {
            for (what, raw) in [("not JSON", "{"), ("the wrong shape", "[1]")] {
                cases.push((
                    format!("{route}: a body that is {what}"),
                    method.clone(),
                    path.clone(),
                    Some(raw.to_owned()),
                    *needs,
                ));
            }
        }
    }
    for (what, tail) in [("since", "?since=abc"), ("limit", "?limit=abc")] {
        cases.push((
            format!("read: {what}=abc"),
            Method::GET,
            format!("/investigations/{unknown}{tail}"),
            None,
            0,
        ));
    }
    cases.push((
        "finding status: a seq that is not a number".into(),
        Method::PUT,
        format!("/investigations/{unknown}/findings/abc/status"),
        valid(&finding),
        1,
    ));
    // (key, authority it holds: None for none)
    let principals: [(&str, &str, Option<u8>); 6] = [
        ("grantless", &server.coastwatch_only_token, None),
        ("query only", &keys.query_only.token, None),
        ("no query", &keys.no_query.token, None),
        ("human", &keys.human.token, Some(0)),
        ("agent", &keys.agent.token, Some(1)),
        ("operator", &keys.operator.token, Some(2)),
    ];
    for (who, token, holds) in principals {
        for (what, method, path, body, needs) in &cases {
            let label = format!("{who}, {what}");
            let (status, text) =
                call_raw(&server, token, method.clone(), path, body.as_deref()).await;
            if holds.is_some_and(|holds| holds >= *needs) {
                assert_eq!(status, StatusCode::BAD_REQUEST, "{label}: {text}");
                assert_eq!(
                    code(&envelope(&text, &label)),
                    "bad_request",
                    "{label}: {text}"
                );
            } else {
                assert_eq!(status, StatusCode::FORBIDDEN, "{label}: {text}");
                assert_eq!(
                    code(&envelope(&text, &label)),
                    "forbidden",
                    "{label}: {text}"
                );
            }
        }
        // A body that never ends, from a key without the route's
        // authority. A key with it is left out: its route rightly waits
        // for the body.
        for (route, method, path, body, needs) in &routes {
            if body.is_none() || holds.is_some_and(|holds| holds >= *needs) {
                continue;
            }
            let label = format!("{who}, {route}: a body that never ends");
            let Some((status, text)) = stalled(&server, token, method, path).await else {
                panic!("{label}: no answer while the body was open");
            };
            assert_eq!(status, StatusCode::FORBIDDEN, "{label}: {text}");
            assert_eq!(
                code(&envelope(&text, &label)),
                "forbidden",
                "{label}: {text}"
            );
        }
    }
}

/// The same two-sided check for the evidence readers and the capture route:
/// a key without `query` gets the 403 envelope whatever its path, query
/// string or body, a key with it the 400 envelope. A capture whose body
/// never ends gets the 403 from a key without `query` while it is open.
#[allow(clippy::too_many_lines)]
#[tokio::test(flavor = "multi_thread")]
async fn the_results_routes_judge_query_authority_before_request_shape() {
    let (server, keys) = boot().await;
    let result = "0".repeat(32);
    let cases: Vec<(&str, Method, String, Option<&str>)> = vec![
        ("page: id %FF", Method::GET, "/results/%FF".into(), None),
        (
            "page: offset=abc",
            Method::GET,
            format!("/results/{result}?offset=abc"),
            None,
        ),
        (
            "page: limit=abc",
            Method::GET,
            format!("/results/{result}?limit=abc"),
            None,
        ),
        (
            "cell: id %FF",
            Method::GET,
            "/results/%FF/cells/0/0".into(),
            None,
        ),
        (
            "cell: row=abc",
            Method::GET,
            format!("/results/{result}/cells/abc/0"),
            None,
        ),
        (
            "cell: column=abc",
            Method::GET,
            format!("/results/{result}/cells/0/abc"),
            None,
        ),
        (
            "cell: offset=abc",
            Method::GET,
            format!("/results/{result}/cells/0/0?offset=abc"),
            None,
        ),
        (
            "cell: length=abc",
            Method::GET,
            format!("/results/{result}/cells/0/0?length=abc"),
            None,
        ),
        (
            "capture: a body that is not JSON",
            Method::POST,
            "/results".into(),
            Some("{"),
        ),
        (
            "capture: a body of the wrong shape",
            Method::POST,
            "/results".into(),
            Some("[1]"),
        ),
    ];
    // (key, holds `query`)
    let principals: [(&str, &str, bool); 6] = [
        ("grantless", &server.coastwatch_only_token, false),
        ("no query", &keys.no_query.token, false),
        ("query only", &keys.query_only.token, true),
        ("human", &keys.human.token, true),
        ("agent", &keys.agent.token, true),
        ("operator", &keys.operator.token, true),
    ];
    for (who, token, query) in principals {
        for (what, method, path, body) in &cases {
            let label = format!("{who}, {what}");
            let (status, text) = call_raw(&server, token, method.clone(), path, *body).await;
            if query {
                assert_eq!(status, StatusCode::BAD_REQUEST, "{label}: {text}");
                assert_eq!(
                    code(&envelope(&text, &label)),
                    "bad_request",
                    "{label}: {text}"
                );
            } else {
                assert_eq!(status, StatusCode::FORBIDDEN, "{label}: {text}");
                assert_eq!(
                    code(&envelope(&text, &label)),
                    "forbidden",
                    "{label}: {text}"
                );
            }
        }
        if !query {
            let label = format!("{who}, capture: a body that never ends");
            let Some((status, text)) = stalled(&server, token, &Method::POST, "/results").await
            else {
                panic!("{label}: no answer while the body was open");
            };
            assert_eq!(status, StatusCode::FORBIDDEN, "{label}: {text}");
            assert_eq!(
                code(&envelope(&text, &label)),
                "forbidden",
                "{label}: {text}"
            );
        }
    }
}

/// The agent key writes, the human key reads the same investigation and its
/// log under each writer's attribution, and the human key cannot write.
#[tokio::test(flavor = "multi_thread")]
async fn the_agent_writes_and_the_human_reads_the_same_investigation() {
    let (server, keys) = boot().await;
    let created = create(&server, &keys.agent, 1).await;
    assert_eq!(created.seq, 0);
    assert_eq!(created.created_by.prefix, keys.agent.prefix);
    assert_eq!(created.created_by.name, "agent");
    let result = kept(&server, &keys.agent, 1).await;
    appended(
        &server,
        &keys.agent,
        1,
        &json!({ "kind": "keep", "result_id": result, "rows": [3, 4], "reason": "the first 502s" }),
    )
    .await;
    appended(
        &server,
        &keys.agent,
        1,
        &json!({
            "kind": "finding",
            "statement": "the 502s start with the deploy",
            "status": "supported",
            "references": [{ "result_id": result, "row": 3 }],
        }),
    )
    .await;

    let (status, body) = call(
        &server,
        &keys.human.token,
        Method::GET,
        "/investigations",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let list: InvestigationList = serde_json::from_value(body).unwrap();
    assert_eq!(list.investigations.len(), 1);
    assert_eq!(list.investigations[0].id.as_str(), id(1));

    let page = read(&server, &keys.human, 1, "").await;
    assert_eq!(page.investigation.seq, 2);
    assert_eq!(page.entries.len(), 2);
    assert!(
        page.entries
            .iter()
            .all(|e| e.actor.prefix == keys.agent.prefix)
    );
    assert!(matches!(&page.entries[0].payload, EntryPayload::Keep { rows, .. } if rows == &[3, 4]));
    assert!(matches!(
        &page.entries[1].payload,
        EntryPayload::Finding { .. }
    ));
    // The wire carries no key id anywhere.
    let raw = call(
        &server,
        &keys.human.token,
        Method::GET,
        &format!("/investigations/{}", id(1)),
        None,
    )
    .await
    .1
    .to_string();
    assert!(!raw.contains("key_id"), "{raw}");

    // The human reads the kept rows the finding cites.
    let (status, _, body) =
        captured::page(&server, &keys.human.token, &result, "?offset=3&limit=1").await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // And cannot write.
    let (status, body) = append(&server, &keys.human, 1, &note("mine")).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
}

/// Revoking the agent's key fails its next append, and the investigation
/// still reads with the creator's name snapshot.
#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_writer_cannot_append_and_its_investigation_still_reads() {
    let (server, keys) = boot().await;
    create(&server, &keys.agent, 1).await;
    appended(&server, &keys.agent, 1, &note("before")).await;
    fleet_auth::KeyStore::from_pool(server.fleet_pool.clone())
        .revoke_key(&keys.agent.prefix)
        .await
        .expect("revoke");

    let (status, body) = append(&server, &keys.agent, 1, &note("after")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

    let page = read(&server, &keys.human, 1, "").await;
    assert_eq!(page.investigation.created_by.name, "agent");
    assert_eq!(page.investigation.created_by.prefix, keys.agent.prefix);
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].actor.name, "agent");
}

// -- create -------------------------------------------------------------------

/// The same creator with the same request gets the same investigation, with
/// nothing advanced; another request or another creator is 409.
#[tokio::test(flavor = "multi_thread")]
async fn create_is_create_if_absent_on_the_creator_and_the_payload() {
    let (server, keys) = boot().await;
    let created = create(&server, &keys.agent, 1).await;
    let path = format!("/investigations/{}", id(1));

    server.results_clock.set(at(START) + TimeDelta::minutes(5));
    let (status, body) = call(
        &server,
        &keys.agent.token,
        Method::PUT,
        &path,
        Some(&creation()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let replayed: Investigation = serde_json::from_value(body).unwrap();
    assert_eq!(
        replayed, created,
        "a replay advances nothing, the clock included"
    );

    let other = json!({ "title": "502s on checkout", "question": "Something else?" });
    let (status, body) = call(&server, &keys.agent.token, Method::PUT, &path, Some(&other)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(code(&body), "investigation_exists", "{body}");

    let (status, body) = call(
        &server,
        &keys.operator.token,
        Method::PUT,
        &path,
        Some(&creation()),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(code(&body), "investigation_exists", "{body}");

    // The evidence clock starts from the configured idle and ceiling.
    let config = &server.state.results.investigation;
    let start = at(START);
    let until = start + TimeDelta::seconds(i64::try_from(config.evidence_idle_secs).unwrap());
    let ceiling = start + TimeDelta::seconds(i64::try_from(config.evidence_ceiling_secs).unwrap());
    assert_eq!(instant(&created.retain_until), until);
    assert_eq!(instant(&created.retain_ceiling), ceiling);
}

/// The create bounds: each text field at its limit and one byte over, a
/// blank title, NUL, and an unknown field.
#[tokio::test(flavor = "multi_thread")]
async fn create_refuses_fields_out_of_bounds() {
    let (server, keys) = boot().await;
    let put = |n: u32, body: Value| {
        let server = &server;
        let token = keys.agent.token.clone();
        async move {
            call(
                server,
                &token,
                Method::PUT,
                &format!("/investigations/{}", id(n)),
                Some(&body),
            )
            .await
        }
    };
    let (status, body) = put(1, json!({ "title": "t".repeat(200), "question": "q".repeat(2048), "scope": "s".repeat(2048) })).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    for (n, bad, rule) in [
        (
            2,
            json!({ "title": "t".repeat(201), "question": "q" }),
            "title is 201 bytes",
        ),
        (
            3,
            json!({ "title": "t", "question": "q".repeat(2049) }),
            "question is 2049 bytes",
        ),
        (
            4,
            json!({ "title": "t", "question": "q", "scope": "s".repeat(2049) }),
            "scope is 2049 bytes",
        ),
        (
            5,
            json!({ "title": " ", "question": "q" }),
            "title is empty",
        ),
        (
            6,
            json!({ "title": "t\u{0}", "question": "q" }),
            "title contains a NUL",
        ),
        (
            7,
            json!({ "title": "t", "question": "q", "owner": "me" }),
            "invalid investigation",
        ),
    ] {
        let (status, body) = put(n, bad).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{rule}: {body}");
        assert_eq!(code(&body), "bad_request", "{body}");
        assert!(message(&body).contains(rule), "{rule}: {body}");
    }
}

// -- read ---------------------------------------------------------------------

/// A read pages the log by `since` and `limit`, with `more` and
/// `through_seq` pointing at the next page.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_pages_the_log_since_a_sequence() {
    let (server, keys) = boot().await;
    create(&server, &keys.agent, 1).await;
    for n in 1..=5 {
        appended(&server, &keys.agent, 1, &note(&format!("note {n}"))).await;
    }
    let first = read(&server, &keys.human, 1, "?since=0&limit=2").await;
    assert_eq!(
        first.entries.iter().map(|e| e.seq).collect::<Vec<_>>(),
        [1, 2]
    );
    assert!(first.more);
    assert_eq!(first.through_seq, 2);
    let rest = read(
        &server,
        &keys.human,
        1,
        &format!("?since={}", first.through_seq),
    )
    .await;
    assert_eq!(
        rest.entries.iter().map(|e| e.seq).collect::<Vec<_>>(),
        [3, 4, 5]
    );
    assert!(!rest.more);
    assert_eq!(rest.through_seq, 5);
    let nothing = read(&server, &keys.human, 1, "?since=5").await;
    assert!(nothing.entries.is_empty());
    assert_eq!(nothing.through_seq, 5);
    read(&server, &keys.human, 1, &format!("?limit={READ_MAX_LIMIT}")).await;

    for query in [
        format!("?limit={}", READ_MAX_LIMIT + 1),
        "?limit=0".into(),
        "?since=-1".into(),
        "?cursor=3".into(),
    ] {
        let (status, body) = call(
            &server,
            &keys.human.token,
            Method::GET,
            &format!("/investigations/{}{query}", id(1)),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body}");
    }
}

// -- append -------------------------------------------------------------------

/// Concurrent appends by two writers both commit, each at its own sequence.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_appends_by_two_writers_both_commit() {
    let (server, keys) = boot().await;
    create(&server, &keys.agent, 1).await;
    let (by_agent, by_operator) = (note("agent"), note("operator"));
    let (a, b) = tokio::join!(
        appended(&server, &keys.agent, 1, &by_agent),
        appended(&server, &keys.operator, 1, &by_operator),
    );
    let mut seqs = [a.entry.seq, b.entry.seq];
    seqs.sort_unstable();
    assert_eq!(seqs, [1, 2]);
    assert_eq!(read(&server, &keys.human, 1, "").await.investigation.seq, 2);
}

/// Each append bound at its limit and one over, through the route.
#[tokio::test(flavor = "multi_thread")]
async fn appends_are_held_to_every_bound() {
    let (server, keys) = boot().await;
    create(&server, &keys.agent, 1).await;
    let result = kept(&server, &keys.agent, 1).await;
    let refs = |n: usize| -> Vec<Value> {
        (0..n)
            .map(|_| json!({ "result_id": result, "row": 0 }))
            .collect()
    };

    let at_limits = [
        note(&"n".repeat(4096)),
        json!({ "kind": "keep", "result_id": result, "rows": vec![0; 200], "reason": "r".repeat(1024) }),
        json!({
            "kind": "finding",
            "statement": "s".repeat(4096),
            "status": "hypothesis",
            "references": refs(32),
            "counterevidence": refs(32),
            "remaining_question": "q".repeat(1024),
        }),
        // A hypothesis may cite nothing yet.
        json!({ "kind": "finding", "statement": "s", "status": "hypothesis", "references": [] }),
    ];
    for entry in &at_limits {
        appended(&server, &keys.agent, 1, entry).await;
    }

    let over = [
        (note(&"n".repeat(4097)), "text is 4097 bytes"),
        (note("a\u{0}b"), "text contains a NUL"),
        (note(""), "text is empty"),
        (
            json!({ "kind": "keep", "result_id": result, "rows": vec![0; 201], "reason": "r" }),
            "rows has 201 items",
        ),
        (
            json!({ "kind": "keep", "result_id": result, "rows": [0], "reason": "r".repeat(1025) }),
            "reason is 1025 bytes",
        ),
        (
            json!({ "kind": "keep", "result_id": result, "rows": [], "reason": "r" }),
            "names no rows",
        ),
        (
            json!({ "kind": "keep", "result_id": "XYZ", "rows": [0], "reason": "r" }),
            "the keep names a result id that is not 32 lowercase hex digits",
        ),
        (
            json!({ "kind": "finding", "statement": "s".repeat(4097), "status": "supported", "references": [] }),
            "statement is 4097 bytes",
        ),
        (
            json!({ "kind": "finding", "statement": "s", "status": "supported", "references": refs(33) }),
            "references has 33 items",
        ),
        (
            json!({ "kind": "finding", "statement": "s", "status": "supported", "references": [], "counterevidence": refs(33) }),
            "counterevidence has 33 items",
        ),
        (
            json!({ "kind": "finding", "statement": "s", "status": "supported", "references": [], "remaining_question": "q".repeat(1025) }),
            "remaining_question is 1025 bytes",
        ),
        (
            json!({ "kind": "step", "executed": "x", "request_id": "r" }),
            "invalid entry",
        ),
        (
            json!({ "kind": "note", "text": "t", "actor": "someone" }),
            "invalid entry",
        ),
    ];
    for (entry, rule) in &over {
        let (status, body) = append(&server, &keys.agent, 1, entry).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{rule}: {body}");
        assert!(message(&body).contains(rule), "{rule}: {body}");
    }
    let seq = read(&server, &keys.human, 1, "").await.investigation.seq;
    assert_eq!(
        seq,
        at_limits.len() as u64,
        "no refused append advanced the sequence"
    );
}

/// A citation names a row of a result this investigation may cite: another
/// key's temporary result and an absent one are each 400 naming the
/// citation, in the same words; another investigation's evidence is 409;
/// a row past the result's rows is 400. (A citation of the caller's own
/// temporary result promotes it: `investigations_evidence.rs`.)
#[tokio::test(flavor = "multi_thread")]
async fn a_citation_must_name_a_row_of_this_investigations_kept_evidence() {
    let (server, keys) = boot().await;
    create(&server, &keys.agent, 1).await;
    create(&server, &keys.agent, 2).await;
    let attached = kept(&server, &keys.agent, 1).await;
    let elsewhere = kept(&server, &keys.agent, 2).await;
    let humans = capture(&server, &keys.human).await;
    let absent = "f".repeat(32);
    let last = captured::ROWS as u64 - 1;

    appended(
        &server,
        &keys.agent,
        1,
        &json!({ "kind": "keep", "result_id": attached, "rows": [0, last], "reason": "edges" }),
    )
    .await;
    for result in [&humans, &absent] {
        let (status, body) = append(
            &server,
            &keys.agent,
            1,
            &json!({ "kind": "keep", "result_id": result, "rows": [0], "reason": "r" }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(
            message(&body),
            format!(
                "rows[0] cites result {result}, which is not kept evidence of this investigation"
            ),
        );
    }
    let (status, body) = append(
        &server,
        &keys.agent,
        1,
        &json!({ "kind": "keep", "result_id": elsewhere, "rows": [0], "reason": "r" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        message(&body),
        format!("rows[0] cites result {elsewhere}, which is already kept by another investigation"),
    );
    let (status, body) = append(
        &server,
        &keys.agent,
        1,
        &json!({ "kind": "keep", "result_id": attached, "rows": [0, last + 1], "reason": "r" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        message(&body),
        format!("rows[1] cites row 20 of result {attached}, which holds 20 rows")
    );
    let (status, body) = append(
        &server,
        &keys.agent,
        1,
        &json!({
            "kind": "finding",
            "statement": "s",
            "status": "refuted",
            "references": [{ "result_id": attached, "row": last }],
            "counterevidence": [{ "result_id": elsewhere, "row": 0 }],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        message(&body).starts_with("counterevidence[0] cites result"),
        "{body}"
    );
    let seq = read(&server, &keys.human, 1, "").await.investigation.seq;
    assert_eq!(seq, 1, "no refused citation appended");
}

/// An append with a `seen_seq` carries the other actors' entries since it,
/// never the caller's own, bounded with `more`; one without carries none.
#[tokio::test(flavor = "multi_thread")]
async fn an_append_carries_the_other_actors_activity_since_its_seen_seq() {
    let (server, keys) = boot().await;
    create(&server, &keys.agent, 1).await;
    let first = appended(&server, &keys.agent, 1, &note("agent 1")).await;
    assert!(first.activity.is_none(), "no seen_seq, no activity");
    appended(&server, &keys.operator, 1, &note("operator 2")).await;
    appended(&server, &keys.agent, 1, &note("agent 3")).await;
    appended(&server, &keys.operator, 1, &note("operator 4")).await;

    let answer = appended(
        &server,
        &keys.agent,
        1,
        &json!({ "kind": "note", "text": "agent 5", "seen_seq": 1 }),
    )
    .await;
    let activity = answer.activity.expect("activity");
    assert_eq!(
        activity.entries.iter().map(|e| e.seq).collect::<Vec<_>>(),
        [2, 4]
    );
    assert!(
        activity
            .entries
            .iter()
            .all(|e| e.actor.prefix == keys.operator.prefix)
    );
    assert!(!activity.more);
    assert_eq!(activity.through_seq, 5);

    // More than the allowance: cut, with `more` and the last entry's seq.
    let operator = store_actor(&server, &keys.operator).await;
    let store = &server.state.storage.investigations;
    let investigation = InvestigationId::parse(&id(1)).unwrap();
    for n in 0..=ACTIVITY_LIMIT {
        store
            .append(
                &investigation,
                &operator,
                &EntryPayload::Note {
                    text: format!("bulk {n}"),
                },
                server.results_clock.now(),
                rule(),
            )
            .await
            .unwrap();
    }
    let answer = appended(
        &server,
        &keys.agent,
        1,
        &json!({ "kind": "note", "text": "agent", "seen_seq": 5 }),
    )
    .await;
    let activity = answer.activity.expect("activity");
    assert_eq!(activity.entries.len() as u64, ACTIVITY_LIMIT);
    assert!(activity.more);
    assert_eq!(activity.through_seq, 5 + ACTIVITY_LIMIT);
}

/// The append commits before its activity is read, so a read that fails
/// answers the written entry with no activity and `more` set, through the
/// caller's `seen_seq`, and a warning: the caller reads the investigation
/// for the rest and never writes the entry again (ADR-0059). The read's
/// session runs through the answer-holding proxy, which holds Postgres's
/// answer to the activity statement; the test terminates that session
/// before letting the answer through, so the read meets a closed
/// connection after the commit landed.
#[tokio::test(flavor = "multi_thread")]
async fn an_append_whose_activity_read_fails_answers_the_written_entry() {
    use sqlx::Connection as _;
    use tower::ServiceExt as _;
    let capture = telemetry();
    let (server, keys) = boot().await;
    let created = create(&server, &keys.agent, 1).await;
    let other = appended(&server, &keys.operator, 1, &note("operator")).await;

    let base: sqlx::postgres::PgConnectOptions = server.app_db_url.parse().unwrap();
    let proxy = answer_hold::Proxy::start(&base).await;
    #[allow(clippy::disallowed_methods)] // one session, through the proxy
    let through = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with(proxy.options(&base))
        .await
        .unwrap();
    let held: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&through)
        .await
        .unwrap();
    let mut state = server.state.clone();
    state.storage.investigations = InvestigationStore::new(through);
    let app = server.router_over(state, |_| {});

    // Only the activity statement compares the actor's key id.
    proxy.hold_next(b"actor_key_id <> $3");
    let entry = json!({ "kind": "note", "text": "agent", "seen_seq": created.seq });
    let path = format!("/investigations/{}/entries", id(1));
    let answer = tokio::spawn(async move {
        app.oneshot(routed(&keys.agent.token, "POST", &path, Some(&entry)))
            .await
            .unwrap()
    });
    proxy.holding().await;
    let mut probe = sqlx::PgConnection::connect(&server.app_db_url)
        .await
        .unwrap();
    let terminated: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1, 10000)")
        .bind(held)
        .fetch_one(&mut probe)
        .await
        .unwrap();
    assert!(terminated, "the activity read's session ended");
    proxy.release();

    let response = answer.await.unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let answer: AppendResponse = serde_json::from_slice(&body).expect("an append response");
    assert_eq!(answer.entry.seq, other.entry.seq + 1);
    assert_eq!(
        answer.entry.payload,
        EntryPayload::Note {
            text: "agent".into()
        }
    );
    let activity = answer.activity.expect("activity");
    assert!(activity.entries.is_empty(), "{activity:?}");
    assert!(activity.more, "the caller is sent to the read");
    assert_eq!(activity.through_seq, created.seq);

    let log = read(&server, &keys.human, 1, "").await;
    assert_eq!(log.investigation.seq, answer.entry.seq);
    let written = log
        .entries
        .iter()
        .filter(|e| {
            e.payload
                == EntryPayload::Note {
                    text: "agent".into(),
                }
        })
        .count();
    assert_eq!(written, 1, "the entry is written once");
    // The capture records each field's `Debug`, so a string is quoted.
    let warned = capture
        .events()
        .iter()
        .filter(|e| {
            e.fields
                .get("event_type")
                .is_some_and(|t| t == "\"investigation_activity_failed\"")
        })
        .map(|e| e.field("error_class"))
        .collect::<Vec<_>>();
    assert_eq!(warned, ["\"store\""], "one warning names the failed read");
}

// -- overwrites ----------------------------------------------------------------

/// A stale close is 409 `sequence_conflict` with both sequences; a current
/// one closes. A closed investigation refuses notes, keeps, findings, a
/// title and a question, and accepts a reopen.
#[tokio::test(flavor = "multi_thread")]
async fn overwrites_check_the_sequence_and_a_closed_investigation_takes_only_a_reopen() {
    let (server, keys) = boot().await;
    create(&server, &keys.agent, 1).await;
    let result = kept(&server, &keys.agent, 1).await;
    appended(&server, &keys.operator, 1, &note("operator")).await;
    let target = format!("/investigations/{}", id(1));
    let patch = |body: Value| {
        let server = &server;
        let token = keys.agent.token.clone();
        let target = target.clone();
        async move { call(server, &token, Method::PATCH, &target, Some(&body)).await }
    };

    let (status, body) = patch(json!({ "expected_seq": 0, "status": "closed" })).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(code(&body), "sequence_conflict", "{body}");
    assert_eq!(
        body["error"]["conflict"],
        json!({ "expected": 0, "current": 1 })
    );

    let (status, body) = patch(json!({ "expected_seq": 1, "title": "renamed" })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let renamed: Investigation = serde_json::from_value(body).unwrap();
    assert_eq!((renamed.title.as_str(), renamed.seq), ("renamed", 2));
    assert_eq!(renamed.updated_by.prefix, keys.agent.prefix);

    let (status, body) = patch(json!({ "expected_seq": 2, "status": "closed" })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let closed: Investigation = serde_json::from_value(body).unwrap();
    assert_eq!(closed.status, InvestigationStatus::Closed);

    for entry in [
        note("late"),
        json!({ "kind": "keep", "result_id": result, "rows": [0], "reason": "r" }),
        json!({ "kind": "finding", "statement": "s", "status": "supported", "references": [] }),
    ] {
        let (status, body) = append(&server, &keys.agent, 1, &entry).await;
        assert_eq!(status, StatusCode::CONFLICT, "{entry}: {body}");
        assert_eq!(code(&body), "investigation_closed", "{body}");
        assert!(message(&body).contains("reopen"), "{body}");
    }
    for body in [
        json!({ "expected_seq": 3, "title": "t" }),
        json!({ "expected_seq": 3, "question": "q" }),
    ] {
        let (status, answer) = patch(body).await;
        assert_eq!(status, StatusCode::CONFLICT, "{answer}");
        assert_eq!(code(&answer), "investigation_closed", "{answer}");
    }
    let (status, body) = patch(json!({ "expected_seq": 3, "status": "open" })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "open");
    assert_eq!(body["seq"], 4);

    // A patch changes exactly one field.
    let (status, body) = patch(json!({ "expected_seq": 4 })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(message(&body).contains("changes nothing"), "{body}");
    let (status, body) =
        patch(json!({ "expected_seq": 4, "title": "t", "status": "closed" })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(message(&body).contains("more than one field"), "{body}");
    let (status, body) = patch(json!({ "expected_seq": 4, "title": "t".repeat(201) })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let page = read(&server, &keys.human, 1, "?since=1").await;
    let kinds: Vec<_> = page.entries.iter().map(|e| e.payload.kind()).collect();
    assert_eq!(
        kinds,
        [EntryKind::Title, EntryKind::Status, EntryKind::Status]
    );
}

/// A finding's status is overwritten under the sequence; a seq that names
/// no finding is 404.
#[tokio::test(flavor = "multi_thread")]
async fn a_finding_status_overwrites_only_a_finding() {
    let (server, keys) = boot().await;
    create(&server, &keys.agent, 1).await;
    appended(&server, &keys.agent, 1, &note("not a finding")).await;
    let finding = appended(
        &server,
        &keys.agent,
        1,
        &json!({ "kind": "finding", "statement": "s", "status": "hypothesis", "references": [] }),
    )
    .await
    .entry;
    let put = |seq: u64, body: Value| {
        let server = &server;
        let token = keys.agent.token.clone();
        async move {
            call(
                server,
                &token,
                Method::PUT,
                &format!("/investigations/{}/findings/{seq}/status", id(1)),
                Some(&body),
            )
            .await
        }
    };

    let (status, body) = put(1, json!({ "expected_seq": 2, "status": "supported" })).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "the note is no finding: {body}"
    );
    let (status, body) = put(99, json!({ "expected_seq": 2, "status": "supported" })).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = put(
        finding.seq,
        json!({ "expected_seq": 1, "status": "supported" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        body["error"]["conflict"],
        json!({ "expected": 1, "current": 2 })
    );
    let (status, body) = put(
        finding.seq,
        json!({ "expected_seq": 2, "status": "refuted" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let written: AppendResponse = serde_json::from_value(body).unwrap();
    assert_eq!(written.entry.seq, 3);
    assert!(matches!(
        written.entry.payload,
        EntryPayload::FindingStatus { finding_seq: 2, .. }
    ));
    let (status, body) = put(finding.seq, json!({ "expected_seq": 3, "status": "maybe" })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = call(
        &server,
        &keys.agent.token,
        Method::PUT,
        &format!("/investigations/{}/findings/two/status", id(1)),
        Some(&json!({ "expected_seq": 3, "status": "supported" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

// -- retention and deletion -------------------------------------------------------

/// An extension needs `server_manage`, never shortens, and raises the
/// ceiling past the configured one.
#[tokio::test(flavor = "multi_thread")]
async fn only_server_manage_extends_and_an_extension_never_shortens() {
    let (server, keys) = boot().await;
    let created = create(&server, &keys.agent, 1).await;
    let path = format!("/investigations/{}/retention", id(1));
    let extend = |token: String, body: Value| {
        let server = &server;
        let path = path.clone();
        async move { call(server, &token, Method::PUT, &path, Some(&body)).await }
    };
    let ceiling = instant(&created.retain_ceiling);
    let later = (ceiling + TimeDelta::days(30)).to_rfc3339();

    let (status, body) = extend(
        keys.agent.token.clone(),
        json!({ "expected_seq": 0, "retain_until": later }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let earlier = (instant(&created.retain_until) - TimeDelta::hours(1)).to_rfc3339();
    let (status, body) = extend(
        keys.operator.token.clone(),
        json!({ "expected_seq": 0, "retain_until": earlier }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(message(&body).contains("never shortens"), "{body}");
    let past = (at(START) - TimeDelta::hours(1)).to_rfc3339();
    let (status, body) = extend(
        keys.operator.token.clone(),
        json!({ "expected_seq": 0, "retain_until": past }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(message(&body).contains("in the past"), "{body}");
    let (status, body) = extend(
        keys.operator.token.clone(),
        json!({ "expected_seq": 0, "retain_until": "next week" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = extend(
        keys.operator.token.clone(),
        json!({ "expected_seq": 7, "retain_until": later }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        body["error"]["conflict"],
        json!({ "expected": 7, "current": 0 })
    );

    let (status, body) = extend(
        keys.operator.token.clone(),
        json!({ "expected_seq": 0, "retain_until": later }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let extended: Investigation = serde_json::from_value(body).unwrap();
    assert_eq!(
        instant(&extended.retain_until),
        ceiling + TimeDelta::days(30)
    );
    assert_eq!(
        instant(&extended.retain_ceiling),
        ceiling + TimeDelta::days(30)
    );
    assert_eq!(extended.seq, 1);
    assert_eq!(extended.updated_by.prefix, keys.operator.prefix);
}

/// A delete needs `server_manage`, refuses while admitted work runs, and
/// once it goes, unlinks the evidence file and leaves nothing to read.
#[tokio::test(flavor = "multi_thread")]
async fn a_delete_unlinks_the_evidence_and_waits_for_running_work() {
    let (server, keys) = boot().await;
    create(&server, &keys.agent, 1).await;
    let result = kept(&server, &keys.agent, 1).await;
    let file = format!("{result}.parquet");
    assert!(captured::root_names(&server).contains(&file));
    let path = format!("/investigations/{}", id(1));

    let (status, body) = call(&server, &keys.agent.token, Method::DELETE, &path, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let store = &server.state.storage.investigations;
    let investigation = InvestigationId::parse(&id(1)).unwrap();
    let step = store
        .begin_step(
            &investigation,
            &store_actor(&server, &keys.agent).await,
            &EntryPayload::Step {
                executed: "service=x".into(),
                bounds: None,
                request_id: "req".into(),
                query_id: None,
                purpose: None,
                outcome: None,
            },
            server.results_clock.now(),
            rule(),
            trawl_server::deadline::Deadline::after(std::time::Duration::from_mins(1)),
        )
        .await
        .unwrap()
        .entry;
    let (status, body) = call(&server, &keys.operator.token, Method::DELETE, &path, None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(message(&body).contains("still running"), "{body}");
    assert!(
        captured::root_names(&server).contains(&file),
        "nothing went"
    );

    let done = trawl_api::investigations::StepOutcome {
        outcome: trawl_api::investigations::StepOutcomeKind::Failed,
        code: None,
        result_id: None,
        rows: None,
        duration_ms: None,
    };
    assert!(
        store
            .complete_step_until(
                &investigation,
                step.seq,
                &done,
                server.results_clock.now(),
                rule(),
                trawl_server::deadline::Deadline::after(std::time::Duration::from_mins(1)),
            )
            .await
            .unwrap()
    );
    let (status, body) = call(&server, &keys.operator.token, Method::DELETE, &path, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(body, Value::Null);
    assert!(
        !captured::root_names(&server).contains(&file),
        "the evidence file is unlinked"
    );

    let (status, body) = call(&server, &keys.human.token, Method::GET, &path, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = call(&server, &keys.operator.token, Method::DELETE, &path, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    // The result reads gone, not as a page of a missing file.
    let (status, _, body) = captured::page(&server, &keys.agent.token, &result, "").await;
    assert_eq!(status, StatusCode::GONE, "{body}");
}

/// Evidence of a deleted investigation reads gone because the
/// investigation was deleted, not as a quota prune. That holds for evidence
/// the delete pruned and for evidence the retention had pruned before the
/// delete: neither has an investigation left, and no finding cites it.
/// Only the owner can still read such a result, within its tombstone; an
/// investigation reader finds no result.
#[tokio::test(flavor = "multi_thread")]
async fn a_result_whose_investigation_was_deleted_reads_gone_for_that_reason() {
    const DELETED: &str =
        "the investigation that kept this result was deleted, so its file was removed";
    let (server, keys) = boot().await;
    create(&server, &keys.agent, 1).await;
    let pruned_by_delete = kept(&server, &keys.agent, 1).await;
    let ended = create(&server, &keys.agent, 2).await;
    let pruned_before = kept(&server, &keys.agent, 2).await;
    let (server, keys) = (&server, &keys);
    let delete = |n: u32| async move {
        let path = format!("/investigations/{}", id(n));
        let (status, body) = call(server, &keys.operator.token, Method::DELETE, &path, None).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    };
    let reads_deleted = |result: &str| {
        let result = result.to_owned();
        async move {
            let (status, _, body) = captured::page(server, &keys.agent.token, &result, "").await;
            assert_eq!(status, StatusCode::GONE, "{body}");
            assert_eq!(code(&body), "result_gone", "{body}");
            assert_eq!(message(&body), DELETED, "{body}");
            let (status, _, body) = captured::page(server, &keys.human.token, &result, "").await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        }
    };

    delete(1).await;
    reads_deleted(&pruned_by_delete).await;

    let past = instant(&ended.retain_until) + TimeDelta::seconds(1);
    server.results_clock.set(past);
    let pruned = server
        .state
        .storage
        .results
        .prune_evidence(past)
        .await
        .unwrap();
    assert_eq!(pruned.len(), 1, "investigation 2's evidence");
    let (status, _, body) = captured::page(server, &keys.agent.token, &pruned_before, "").await;
    assert_eq!(status, StatusCode::GONE, "{body}");
    assert_eq!(
        message(&body),
        GoneReason::RetentionEnded.message(),
        "{body}"
    );
    delete(2).await;
    reads_deleted(&pruned_before).await;
}

// -- evidence readers ------------------------------------------------------------

/// Another key with `investigation_read` pages a kept result it did not
/// capture, reads it as pruned once its retention ends, and still cannot
/// tell another key's temporary result from no result. The owner still
/// pages its own temporary result, and a key without `investigation_read`
/// keeps the owner-only rule.
#[tokio::test(flavor = "multi_thread")]
async fn every_investigation_reader_pages_kept_evidence_and_only_kept_evidence() {
    let (server, keys) = boot().await;
    let created = create(&server, &keys.agent, 1).await;
    let evidence = kept(&server, &keys.agent, 1).await;
    let temporary = capture(&server, &keys.agent).await;
    let absent = "e".repeat(32);

    let (status, _, body) =
        captured::page(&server, &keys.human.token, &evidence, "?offset=0&limit=5").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["page"]["rows"].as_array().unwrap().len(), 5);
    let (status, _, body) = captured::cell(&server, &keys.human.token, &evidence, 0, 0, "").await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, _, foreign) = captured::page(&server, &keys.human.token, &temporary, "").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{foreign}");
    let (_, _, missing) = captured::page(&server, &keys.human.token, &absent, "").await;
    assert_eq!(
        foreign, missing,
        "another key's temporary result reads as no result"
    );

    let (status, _, body) = captured::page(&server, &keys.query_only.token, &evidence, "").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "without investigation_read: {body}"
    );

    let (status, _, body) = captured::page(&server, &keys.agent.token, &temporary, "").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the owner's temporary result: {body}"
    );

    // Past the investigation's retention the evidence is pruned: 410, for
    // as long as the investigation exists, tombstone age or not.
    let past = instant(&created.retain_until) + TimeDelta::seconds(1);
    server.results_clock.set(past);
    let pruned = server
        .state
        .storage
        .results
        .prune_evidence(past)
        .await
        .unwrap();
    assert_eq!(pruned.len(), 1);
    for later in [past, past + TimeDelta::days(400)] {
        server.results_clock.set(later);
        for key in [&keys.human, &keys.agent] {
            let (status, _, body) = captured::page(&server, &key.token, &evidence, "").await;
            assert_eq!(status, StatusCode::GONE, "{body}");
            assert_eq!(code(&body), "result_gone", "{body}");
            assert_eq!(
                message(&body),
                GoneReason::RetentionEnded.message(),
                "{body}"
            );
        }
    }
}

/// A kept result whose investigation's retention ended reads gone for that
/// reason on the spot, with no sweep between: the read prunes that
/// investigation's evidence under its lock, as a write would, and unlinks
/// the file. Evidence whose investigation changed since stays readable.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_past_the_retention_ends_the_evidence_on_the_spot() {
    let (server, keys) = boot().await;
    let ended = create(&server, &keys.agent, 1).await;
    let evidence = kept(&server, &keys.agent, 1).await;
    create(&server, &keys.agent, 2).await;
    let sliding = kept(&server, &keys.agent, 2).await;
    let past = instant(&ended.retain_until) + TimeDelta::seconds(1);
    let agent = store_actor(&server, &keys.agent).await;
    server
        .state
        .storage
        .investigations
        .append(
            &InvestigationId::parse(&id(2)).unwrap(),
            &agent,
            &EntryPayload::Note {
                text: "still looking".into(),
            },
            past - TimeDelta::hours(1),
            rule(),
        )
        .await
        .unwrap();
    server.results_clock.set(past);
    let root = &server.state.results.root;
    assert!(root.path_of(&evidence).unwrap().exists());

    let (status, _, body) = captured::page(&server, &keys.human.token, &evidence, "").await;
    assert_eq!(status, StatusCode::GONE, "{body}");
    assert_eq!(code(&body), "result_gone", "{body}");
    assert_eq!(
        message(&body),
        GoneReason::RetentionEnded.message(),
        "{body}"
    );
    assert!(
        !root.path_of(&evidence).unwrap().exists(),
        "the read left the file"
    );
    let row = server
        .state
        .storage
        .results
        .get(
            &evidence,
            captured::key_id(&server, &keys.agent.token).await,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (row.state, row.gone_at),
        (ResultState::Pruned, Some(instant(&ended.retain_until)))
    );

    let (status, _, body) = captured::page(&server, &keys.human.token, &sliding, "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
}
