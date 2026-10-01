// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `POST /api/v1/ingest/preview` against a real trawld (ADR-0049, #200).
//!
//! The report's contract (input order, per-position outcomes, the echoed
//! header, sender dependence), its access and bounds (permission, body
//! limit, the 500-position cap, encodings, the disabled route, the rate
//! class), and `Cache-Control: no-store` on what the route answers.

mod common;

use common::{
    TestServer, roles, setup, setup_in_dir_with_ingest, setup_with_rate_limit,
    setup_with_trusted_relays,
};
use fleet_auth::{KeyStore, PrincipalKind};
use serde_json::{Value, json};
use trawl_api::ingest_preview::{
    FieldChangeKind, FieldChangeWire, MAX_PREVIEW_EVENTS, PreviewEvent, PreviewResponse,
    SeverityLineage, SeveritySourceSpec, TimeLineage,
};
use trawl_client::HttpClient;
use trawl_server::config::RateLimitConfig;

/// The fixture's interactive `max_request_body_bytes`.
const INTERACTIVE_BODY_LIMIT: usize = 128 * 1024;

fn raw_client() -> reqwest::Client {
    common::harness_client_builder().build().unwrap()
}

/// POST a sample to the preview with `token`, `query` appended verbatim.
async fn post_preview(
    server: &TestServer,
    token: &str,
    query: &str,
    body: impl Into<reqwest::Body>,
) -> reqwest::Response {
    raw_client()
        .post(format!("{}/api/v1/ingest/preview{query}", server.url))
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/x-ndjson")
        .body(body)
        .send()
        .await
        .unwrap()
}

/// Assert the response may not be cached.
fn assert_no_store(resp: &reqwest::Response, what: &str) {
    assert_eq!(
        resp.headers()
            .get(reqwest::header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("no-store"),
        "{what}: Cache-Control must be no-store"
    );
}

/// A 200 preview as the admin (`server_manage`) key, parsed.
async fn preview_ok(server: &TestServer, query: &str, body: &str) -> PreviewResponse {
    let resp = post_preview(server, &server.admin_token, query, body.to_owned()).await;
    assert_eq!(resp.status(), 200, "preview must succeed");
    assert_no_store(&resp, "200");
    resp.json().await.unwrap()
}

/// `(status, error code, message)` of an error response, after checking
/// it is no-store.
async fn error_of(resp: reqwest::Response, what: &str) -> (u16, String, String) {
    assert_no_store(&resp, what);
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    (
        status,
        body["error"]["code"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
    )
}

/// `(index, outcome, reason-or-service)` per reported position.
fn outline(report: &PreviewResponse) -> Vec<(usize, &'static str, String)> {
    report
        .events
        .iter()
        .map(|event| match event {
            PreviewEvent::Accepted { index, event, .. } => (
                *index,
                "accepted",
                event["service"].as_str().unwrap().to_owned(),
            ),
            PreviewEvent::Rejected { index, reason, .. } => (*index, "rejected", reason.clone()),
        })
        .collect()
}

fn rejected(event: &PreviewEvent) -> (usize, Option<&Value>, &str, &str, bool) {
    match event {
        PreviewEvent::Rejected {
            index,
            input,
            reason,
            message,
            host_depends_on_sender,
        } => (
            *index,
            input.as_ref(),
            reason,
            message,
            *host_depends_on_sender,
        ),
        PreviewEvent::Accepted { .. } => panic!("expected a rejection, got {event:?}"),
    }
}

/// `(event, repairs, host_depends_on_sender)` of an accepted position.
fn accepted(event: &PreviewEvent) -> (&serde_json::Map<String, Value>, &[String], bool) {
    match event {
        PreviewEvent::Accepted {
            event,
            repairs,
            host_depends_on_sender,
            ..
        } => (event, repairs, *host_depends_on_sender),
        PreviewEvent::Rejected { .. } => panic!("expected an acceptance, got {event:?}"),
    }
}

// -- the report ---------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn ndjson_sample_is_reported_in_input_order() {
    let server = setup().await;
    let body = "{\"service\":\"zeta\"}\n\n{\"message\":\"no service\"}\n{\"service\":\"alpha\"}\n{\"service\":\"zeta\"}";
    let report = preview_ok(&server, "", body).await;
    assert_eq!(
        outline(&report),
        [
            (0, "accepted", "zeta".to_owned()),
            (2, "rejected", "missing_service".to_owned()),
            (3, "accepted", "alpha".to_owned()),
            (4, "accepted", "zeta".to_owned()),
        ],
        "NDJSON positions are physical lines, blank ones counted"
    );
    assert_eq!((report.accepted, report.rejected), (3, 1));
    let PreviewEvent::Accepted { input, .. } = &report.events[2] else {
        unreachable!()
    };
    assert_eq!(input, &json!({"service": "alpha"}));
}

#[tokio::test(flavor = "multi_thread")]
async fn json_array_sample_is_reported_in_input_order() {
    let server = setup().await;
    let body = r#"[{"service":"zeta"},{"service":"alpha"},{"message":"x"},{"service":"mid"}]"#;
    let report = preview_ok(&server, "", body).await;
    assert_eq!(
        outline(&report),
        [
            (0, "accepted", "zeta".to_owned()),
            (1, "accepted", "alpha".to_owned()),
            (2, "rejected", "missing_service".to_owned()),
            (3, "accepted", "mid".to_owned()),
        ]
    );
    assert_eq!((report.accepted, report.rejected), (3, 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_ndjson_line_gets_its_position_and_parse_reason_without_input() {
    let server = setup().await;
    let body = "{\"service\":\"a\"}\nnot json at all zz-canary\n{\"service\":\"b\"}";
    let report = preview_ok(&server, "", body).await;
    let (index, input, reason, message, depends) = rejected(&report.events[1]);
    assert_eq!((index, input, reason), (1, None, "invalid_json"));
    assert!(message.starts_with("invalid JSON"), "{message}");
    assert!(!message.contains("zz-canary"), "the line is never echoed");
    assert!(!depends);
    // Nothing invented: the wire form omits `input` entirely.
    let wire = serde_json::to_value(&report.events[1]).unwrap();
    assert!(wire.get("input").is_none(), "{wire}");
    assert_eq!((report.accepted, report.rejected), (2, 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn non_object_array_element_gets_its_position_reason_and_input() {
    let server = setup().await;
    let body = r#"[{"service":"a"}, 42, null, "text"]"#;
    let report = preview_ok(&server, "", body).await;
    let expected = [json!(42), Value::Null, json!("text")];
    for (event, (index, value)) in report.events[1..]
        .iter()
        .zip([1, 2, 3].iter().zip(&expected))
    {
        let (got_index, input, reason, message, _) = rejected(event);
        assert_eq!(
            (got_index, input, reason, message),
            (*index, Some(value), "not_object", "expected JSON object")
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_json_array_is_a_whole_request_400() {
    let server = setup().await;
    let resp = post_preview(
        &server,
        &server.admin_token,
        "",
        r#"[{"service":"a"}, {"service":"#,
    )
    .await;
    let (status, code, message) = error_of(resp, "framing 400").await;
    assert_eq!((status, code.as_str()), (400, "ingest_error"));
    assert!(message.contains("invalid JSON array"), "{message}");
}

#[tokio::test(flavor = "multi_thread")]
async fn all_rejected_sample_is_200() {
    let server = setup().await;
    let report = preview_ok(&server, "", "{\"message\":\"a\"}\nnope\n{\"service\":\"\"}").await;
    assert_eq!((report.accepted, report.rejected), (0, 3));
    assert_eq!(
        outline(&report),
        [
            (0, "rejected", "missing_service".to_owned()),
            (1, "rejected", "invalid_json".to_owned()),
            (2, "rejected", "empty_service".to_owned()),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn accepted_event_carries_canonical_event_repairs_and_lineage() {
    let server = setup().await;
    let input = json!({
        "service": "api",
        "host": "web01",
        "severity": "bogus",
        "level": "warn",
        "timestamp": "not a time",
        "Ctx": {"a": 1},
        "message": "hello",
    });
    let report = preview_ok(&server, "", &input.to_string()).await;
    let PreviewEvent::Accepted {
        index,
        input: echoed,
        event,
        repairs,
        lineage,
        host_depends_on_sender,
    } = &report.events[0]
    else {
        panic!("expected an acceptance: {:?}", report.events[0]);
    };
    assert_eq!((*index, echoed), (0, &input));
    assert!(!host_depends_on_sender, "the event names its host");

    // The full canonical event, server stamps included.
    assert_eq!(event["service"], "api");
    assert_eq!(event["env"], "prod");
    assert_eq!(event["host"], "web01");
    assert_eq!(event["message"], "hello");
    assert_eq!(event["level"], "warn");
    assert_eq!(event["_severity"], 13, "warn maps to OTel WARN");
    assert_eq!(event["_ingested"], report.arrival.as_str());
    assert_eq!(event["_time"], report.arrival.as_str(), "time.from_ingest");
    assert_eq!(event["ctx"], r#"{"a":1}"#, "nested value stringified");
    assert!(event.contains_key("_raw"));
    assert!(!event.contains_key("Ctx"));

    // Repair codes, in application order: the event's own `_repairs`.
    assert_eq!(
        event["_repairs"]
            .as_str()
            .unwrap()
            .split(',')
            .collect::<Vec<_>>(),
        repairs.iter().map(String::as_str).collect::<Vec<_>>()
    );
    for code in [
        "env.defaulted",
        "time.from_ingest",
        "field.name_case_folded",
    ] {
        assert!(repairs.iter().any(|r| r == code), "{code} in {repairs:?}");
    }

    // Lineage, named.
    assert_eq!(
        lineage.time,
        TimeLineage::Arrival {
            unparseable: Some("timestamp".into())
        }
    );
    assert_eq!(
        lineage.severity,
        SeverityLineage::Field {
            field: "level".into(),
            skipped_unmappable: vec!["severity".into()],
        }
    );
    assert!(
        lineage.fields.contains(&FieldChangeWire {
            field: "Ctx".into(),
            change: FieldChangeKind::Renamed,
            to: Some("ctx".into()),
            code: Some("field.name_case_folded".into()),
        }),
        "{:?}",
        lineage.fields
    );
    assert!(
        lineage.fields.iter().any(|c| c.field == "ctx"
            && c.change == FieldChangeKind::Stringified
            && c.code.is_none()),
        "{:?}",
        lineage.fields
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rejected_event_carries_reason_and_message_and_no_event() {
    let server = setup().await;
    let report = preview_ok(&server, "", r#"{"service":"api","env":"staging"}"#).await;
    let (index, input, reason, message, _) = rejected(&report.events[0]);
    assert_eq!(
        (index, input, reason),
        (
            0,
            Some(&json!({"service": "api", "env": "staging"})),
            "env_not_allowed"
        )
    );
    assert!(
        message.contains("staging"),
        "same message as ingest: {message}"
    );
    assert!(message.contains("prod"), "the env list is named: {message}");
    let wire = serde_json::to_value(&report.events[0]).unwrap();
    for absent in ["event", "repairs", "lineage"] {
        assert!(wire.get(absent).is_none(), "{absent} in {wire}");
    }
}

// -- the header and the peer ----------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn header_echoes_producer_peer_relay_arrival_and_derivation() {
    let server = setup().await;
    let report = preview_ok(
        &server,
        "?peer_ip=%3A%3Affff%3A10.1.2.3",
        "{\"service\":\"a\"}\n{\"service\":\"b\"}",
    )
    .await;
    assert_eq!(report.producer, "http");
    assert_eq!(
        report.peer.ip, "10.1.2.3",
        "an IPv4-mapped peer reads as IPv4"
    );
    assert!(report.peer.given);
    assert!(!report.peer.trusted_relay);
    chrono::DateTime::parse_from_rfc3339(&report.arrival).expect("arrival is RFC 3339");
    for event in &report.events {
        let (event, _, _) = accepted(event);
        assert_eq!(event["_ingested"], report.arrival.as_str(), "one arrival");
    }
    assert_eq!(
        report.derivation.time_from,
        ["_time", "timestamp", "@timestamp"]
    );
    assert_eq!(
        report.derivation.severity_from,
        ["severity", "severity_text", "level"]
            .map(|field| SeveritySourceSpec {
                field: field.into(),
                dialect: "otel".into(),
            })
            .to_vec()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unparseable_peer_ip_is_400_without_echo() {
    let server = setup().await;
    for query in [
        "?peer_ip=zz-not-an-ip",
        "?peer_ip=",
        "?peer_ip=10.0.0.0%2F8",
    ] {
        let resp = post_preview(&server, &server.admin_token, query, "{\"service\":\"a\"}").await;
        let (status, code, message) = error_of(resp, query).await;
        assert_eq!((status, code.as_str()), (400, "bad_request"), "{query}");
        assert!(!message.contains("zz-not"), "{query}: {message}");
        assert!(!message.contains("10.0.0.0"), "{query}: {message}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_query_parameter_is_400() {
    let server = setup().await;
    for query in ["?peer=10.0.0.1", "?peer_ip=10.0.0.1&relay=true"] {
        let resp = post_preview(&server, &server.admin_token, query, "{\"service\":\"a\"}").await;
        let (status, code, _) = error_of(resp, query).await;
        assert_eq!((status, code.as_str()), (400, "bad_request"), "{query}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn omitted_peer_uses_the_placeholder_and_marks_sender_dependence() {
    let server = setup().await;
    let body = "{\"service\":\"a\"}\n{\"service\":\"b\",\"host\":\"web01\"}\n{\"message\":\"no service, no host\"}\n{\"message\":\"no service\",\"host\":\"web01\"}";
    let report = preview_ok(&server, "", body).await;
    assert_eq!(report.peer.ip, "192.0.2.1");
    assert!(!report.peer.given);
    assert!(!report.peer.trusted_relay);

    let (event, repairs, depends) = accepted(&report.events[0]);
    assert!(depends, "a host-less event depends on the sender");
    assert_eq!(event["host"], "192.0.2.1");
    assert!(repairs.iter().any(|r| r == "host.from_peer"));

    let (event, _, depends) = accepted(&report.events[1]);
    assert!(!depends, "an event with a host does not");
    assert_eq!(event["host"], "web01");

    let (_, _, reason, _, depends) = rejected(&report.events[2]);
    assert_eq!(reason, "missing_service");
    assert!(depends, "a rejected host-less event is marked too");

    let (_, _, _, _, depends) = rejected(&report.events[3]);
    assert!(!depends);

    // With the peer given, nothing depends on an unknown sender.
    let given = preview_ok(&server, "?peer_ip=10.0.0.7", body).await;
    assert!(given.peer.given);
    let (event, _, depends) = accepted(&given.events[0]);
    assert!(!depends);
    assert_eq!(event["host"], "10.0.0.7");
    assert!(!rejected(&given.events[2]).4);
}

/// Real ingest from the fixture's client always arrives from 127.0.0.1, so
/// each server answers for that peer both ways: a relay server rejects the
/// host-less event, a plain one fills `host` from the peer. The preview,
/// given the same peer, must say the same; given an untrusted peer on the
/// relay server, it fills.
#[tokio::test(flavor = "multi_thread")]
async fn trusted_relay_and_untrusted_peer_match_real_ingest() {
    let hostless = r#"{"service":"relayprobe","message":"m"}"#;
    let ingest = |server: &TestServer| {
        raw_client()
            .post(format!("{}/api/v1/ingest", server.url))
            .header("authorization", format!("Bearer {}", server.ingest_token))
            .header("content-type", "application/x-ndjson")
            .body(hostless)
            .send()
    };

    // Behind a relay: real ingest rejects, and so does the preview.
    let relay = setup_with_trusted_relays(&["127.0.0.1/32"]).await;
    let real: trawl_api::IngestResponse = ingest(&relay).await.unwrap().json().await.unwrap();
    assert_eq!(real.accepted, 0);
    assert_eq!(real.errors[0].reason, "host_missing_from_relay");
    let report = preview_ok(&relay, "?peer_ip=127.0.0.1", hostless).await;
    assert!(report.peer.trusted_relay);
    let (_, _, reason, message, _) = rejected(&report.events[0]);
    assert_eq!(reason, real.errors[0].reason);
    assert_eq!(message, real.errors[0].message);

    // The same event from an untrusted peer on the same server is filled.
    let report = preview_ok(&relay, "?peer_ip=10.9.8.7", hostless).await;
    assert!(!report.peer.trusted_relay);
    let (event, repairs, _) = accepted(&report.events[0]);
    assert_eq!(event["host"], "10.9.8.7");
    assert!(repairs.iter().any(|r| r == "host.from_peer"));

    // No relay: real ingest fills from the peer, and so does the preview.
    let plain = setup().await;
    let real: trawl_api::IngestResponse = ingest(&plain).await.unwrap().json().await.unwrap();
    assert_eq!((real.accepted, real.rejected), (1, 0));
    let stored = HttpClient::new_insecure(&plain.url, &plain.analyst_token)
        .unwrap()
        .query_paginated(
            "service=relayprobe host=127.0.0.1 last=1h | table host, _repairs",
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(stored.result.row_count(), 1, "real ingest filled host");
    let report = preview_ok(&plain, "?peer_ip=127.0.0.1", hostless).await;
    assert!(!report.peer.trusted_relay);
    let (event, repairs, _) = accepted(&report.events[0]);
    assert_eq!(event["host"], "127.0.0.1");
    assert!(repairs.iter().any(|r| r == "host.from_peer"));
    assert_eq!(
        stored.result.rows[0][1],
        trawl_api::value::Value::String(repairs.join(",")),
        "the same repairs real ingest stored"
    );
}

// -- access and bounds ------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn keys_without_server_manage_get_403() {
    let server = setup().await;
    // The trial's ingest key is `ingest` alone: the fixture's ingest key.
    for (who, token) in [
        ("ingest-only", &server.ingest_token),
        ("reader", &server.reader_token),
        ("analyst", &server.analyst_token),
    ] {
        let resp = post_preview(&server, token, "", "{\"service\":\"a\"}").await;
        let (status, code, _) = error_of(resp, who).await;
        assert_eq!((status, code.as_str()), (403, "forbidden"), "{who}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn server_manage_key_succeeds() {
    let server = setup().await;
    let report = preview_ok(&server, "", "{\"service\":\"a\"}").await;
    assert_eq!((report.accepted, report.rejected), (1, 0));
}

#[tokio::test(flavor = "multi_thread")]
async fn body_over_max_request_body_bytes_is_413() {
    let server = setup().await;
    let line = format!(
        "{{\"service\":\"a\",\"message\":\"{}\"}}\n",
        "x".repeat(1000)
    );
    let mut body = line.repeat(INTERACTIVE_BODY_LIMIT / line.len() + 1);
    body.truncate(INTERACTIVE_BODY_LIMIT + 1);
    assert_eq!(body.len(), INTERACTIVE_BODY_LIMIT + 1);
    let resp = post_preview(&server, &server.admin_token, "", body).await;
    assert_eq!(resp.status(), 413);
}

/// Lines of `{"service":"s"}`, `valid` of them, then `invalid` lines that
/// are not JSON, with a blank line between every two.
fn sample(valid: usize, invalid: usize) -> String {
    let mut lines = vec!["{\"service\":\"s\"}"; valid];
    lines.extend(std::iter::repeat_n("not json", invalid));
    lines.join("\n\n")
}

#[tokio::test(flavor = "multi_thread")]
async fn exactly_500_positions_is_200() {
    let server = setup().await;
    let report = preview_ok(&server, "", &sample(MAX_PREVIEW_EVENTS - 2, 2)).await;
    assert_eq!(report.events.len(), MAX_PREVIEW_EVENTS);
    assert_eq!((report.accepted, report.rejected), (498, 2));
    // Blank lines are not positions, but they are lines.
    let last = report.events.last().unwrap();
    assert_eq!(rejected(last).0, 2 * (MAX_PREVIEW_EVENTS - 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn more_than_500_positions_is_413_counting_invalid_lines_not_blank_ones() {
    let server = setup().await;
    // 499 valid + 2 invalid = 501 positions: the invalid lines count.
    let resp = post_preview(&server, &server.admin_token, "", sample(499, 2)).await;
    let (status, code, message) = error_of(resp, "preview_too_large").await;
    assert_eq!((status, code.as_str()), (413, "preview_too_large"));
    assert!(message.contains("500"), "{message}");

    // The same in an array.
    let array = format!("[{}]", vec!["{\"service\":\"s\"}"; 501].join(","));
    let resp = post_preview(&server, &server.admin_token, "", array).await;
    let (status, code, _) = error_of(resp, "array preview_too_large").await;
    assert_eq!((status, code.as_str()), (413, "preview_too_large"));
}

#[tokio::test(flavor = "multi_thread")]
async fn gzip_content_encoding_is_415() {
    let server = setup().await;
    for encoding in ["gzip", "deflate", "identity, gzip"] {
        let resp = raw_client()
            .post(format!("{}/api/v1/ingest/preview", server.url))
            .header("authorization", format!("Bearer {}", server.admin_token))
            .header("content-encoding", encoding)
            .body("{\"service\":\"a\"}")
            .send()
            .await
            .unwrap();
        let (status, code, _) = error_of(resp, encoding).await;
        assert_eq!(
            (status, code.as_str()),
            (415, "unsupported_encoding"),
            "{encoding}"
        );
    }
    // `identity` is no encoding at all.
    let resp = raw_client()
        .post(format!("{}/api/v1/ingest/preview", server.url))
        .header("authorization", format!("Bearer {}", server.admin_token))
        .header("content-encoding", "identity")
        .body("{\"service\":\"a\"}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn ingest_disabled_answers_404() {
    let tmp = tempfile::tempdir().unwrap();
    let server = setup_in_dir_with_ingest(tmp.path(), false).await;
    let resp = post_preview(&server, &server.admin_token, "", "{\"service\":\"a\"}").await;
    assert_eq!(resp.status(), 404);
}

/// Every response class the route itself produces is no-store. A 401 from
/// the shared auth layer is not the route's.
#[tokio::test(flavor = "multi_thread")]
async fn every_preview_response_is_no_store() {
    let server = setup().await;
    let admin = &server.admin_token;
    let cases = [
        ("200", admin, "", "{\"service\":\"a\"}".to_owned(), 200),
        ("all rejected", admin, "", "nope".to_owned(), 200),
        ("framing 400", admin, "", "[".to_owned(), 400),
        ("peer 400", admin, "?peer_ip=x", "{}".to_owned(), 400),
        ("param 400", admin, "?x=1", "{}".to_owned(), 400),
        ("403", &server.ingest_token, "", "{}".to_owned(), 403),
        ("413 preview_too_large", admin, "", sample(501, 0), 413),
    ];
    for (what, token, query, body, status) in cases {
        let resp = post_preview(&server, token, query, body).await;
        assert_eq!(resp.status(), status, "{what}");
        assert_no_store(&resp, what);
    }
    let resp = raw_client()
        .post(format!("{}/api/v1/ingest/preview", server.url))
        .header("authorization", format!("Bearer {admin}"))
        .header("content-encoding", "gzip")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 415);
    assert_no_store(&resp, "415");
}

/// A key holding both `server_manage` and `ingest` spends the interactive
/// bucket on the preview: with the ingest class unlimited, the preview is
/// still 429 after the interactive burst, while real ingest from the same
/// key is not.
#[tokio::test(flavor = "multi_thread")]
async fn preview_spends_the_interactive_rate_bucket() {
    let server = setup_with_rate_limit(RateLimitConfig {
        default_rpm: 2,
        ingest_rpm: 0,
    })
    .await;
    let store = KeyStore::from_pool(server.fleet_pool.clone());
    store
        .create_role(
            "manage-and-ingest",
            None,
            &common::trawl_perms(&["server_manage", "ingest"]),
        )
        .await
        .unwrap();
    let key = store
        .create_key(
            "both",
            PrincipalKind::Service,
            &roles(&["manage-and-ingest"]),
            None,
        )
        .await
        .unwrap();
    let token = key.plaintext_token.to_string();

    // Spending the ingest class first leaves the interactive burst whole.
    for i in 0..5 {
        let resp = raw_client()
            .post(format!("{}/api/v1/ingest", server.url))
            .header("authorization", format!("Bearer {token}"))
            .body(format!("{{\"service\":\"s\",\"host\":\"h\",\"n\":{i}}}"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "ingest {i} is unlimited");
    }
    let mut statuses = Vec::new();
    for _ in 0..3 {
        let resp = post_preview(&server, &token, "", "{\"service\":\"s\"}").await;
        statuses.push(resp.status().as_u16());
    }
    assert_eq!(statuses, [200, 200, 429], "the interactive burst of 2");

    // Real ingest from the same key still rides its own bucket.
    let resp = raw_client()
        .post(format!("{}/api/v1/ingest", server.url))
        .header("authorization", format!("Bearer {token}"))
        .body("{\"service\":\"s\",\"host\":\"h\"}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}
