// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `POST /api/v1/ingest/preview` against a real trawld (ADR-0049, #200).
//!
//! The report's contract (input order, per-position outcomes, the echoed
//! header, sender dependence), its access and bounds (permission, body
//! limit, the 500-position cap, encodings, the disabled route, the rate
//! class), `Cache-Control: no-store` on what the route answers, and parity
//! with real ingest: the same body through both routes, compared against
//! the WAL. A preview never takes a repairs metric label either.

mod common;

use std::collections::{BTreeMap, BTreeSet};

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
use trawl_server::ingest::envelope::{RejectReason, RepairCode};
use trawl_server::metrics::OVERFLOW_SERVICE_LABEL;

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

/// A preview request head as `token`, with `framing` as its body header.
fn preview_head(server: &TestServer, token: &str, framing: &str) -> String {
    let host = server.url.strip_prefix("https://").unwrap();
    format!(
        "POST /api/v1/ingest/preview HTTP/1.1\r\nhost: {host}\r\n\
         authorization: Bearer {token}\r\ncontent-type: application/x-ndjson\r\n\
         {framing}\r\n\r\n"
    )
}

/// A preview that announces `length` body bytes and sends none: the body
/// limit's `Content-Length` refusal answers on the head alone.
fn announced_body(server: &TestServer, token: &str, length: usize) -> Vec<u8> {
    preview_head(server, token, &format!("content-length: {length}")).into_bytes()
}

/// A preview whose chunked body is `body` in one chunk and never ends: the
/// limit is met only by reading it, and the server reads all of it before
/// it can refuse.
fn unterminated_chunked_body(server: &TestServer, token: &str, body: &[u8]) -> Vec<u8> {
    let mut request = preview_head(server, token, "transfer-encoding: chunked").into_bytes();
    request.extend_from_slice(format!("{:x}\r\n", body.len()).as_bytes());
    request.extend_from_slice(body);
    request
}

/// Assert a raw response may not be cached.
fn assert_raw_no_store(resp: &common::RawResponse, what: &str) {
    assert_eq!(
        resp.header("cache-control"),
        Some("no-store"),
        "{what}: Cache-Control must be no-store"
    );
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
    let admin = &server.admin_token;

    // Refused on the announced length, before a body byte is read.
    let request = announced_body(&server, admin, body.len());
    let resp = common::raw_https_exchange(&server.url, &request).await;
    assert_eq!(resp.status, 413, "Content-Length refusal");

    // No length announced: refused once the byte past the limit is read.
    let request = unterminated_chunked_body(&server, admin, body.as_bytes());
    let resp = common::raw_https_exchange(&server.url, &request).await;
    assert_eq!(resp.status, 413, "streamed refusal");
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

/// Every response to the preview is no-store, whichever layer answers it:
/// the handler, the body limit's refusals (on `Content-Length` and on a
/// streamed body), the auth
/// layer's 401 or the grant layer's 403. Another route on the same router
/// is not given the header.
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

    // A sized body is refused by the router's body limit on the header
    // alone, before any extractor reads it; an unsized one by the
    // extractor, once it reads past the limit.
    let request = announced_body(&server, admin, INTERACTIVE_BODY_LIMIT + 1);
    let resp = common::raw_https_exchange(&server.url, &request).await;
    assert_eq!(resp.status, 413, "body-limit 413");
    assert_raw_no_store(&resp, "body-limit 413");
    let oversized = "x".repeat(INTERACTIVE_BODY_LIMIT + 1);
    let request = unterminated_chunked_body(&server, admin, oversized.as_bytes());
    let resp = common::raw_https_exchange(&server.url, &request).await;
    assert_eq!(resp.status, 413, "streamed body-limit 413");
    assert_raw_no_store(&resp, "streamed body-limit 413");

    // A key with no trawl grant at all is refused by the grant layer.
    let resp = post_preview(&server, &server.coastwatch_only_token, "", "{}").await;
    let (status, code, _) = error_of(resp, "grant-layer 403").await;
    assert_eq!((status, code.as_str()), (403, "forbidden"));

    // No token at all is the auth layer's 401.
    let resp = raw_client()
        .post(format!("{}/api/v1/ingest/preview", server.url))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    assert_no_store(&resp, "401");

    // A neighbour on the same router keeps its own caching headers.
    let resp = raw_client()
        .get(format!("{}/api/v1/whoami", server.url))
        .bearer_auth(admin)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get(reqwest::header::CACHE_CONTROL),
        None,
        "whoami is not the preview"
    );
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

// -- parity with real ingest ----------------------------------------------------

/// Repair codes only a profile producer (syslog, trawld) can earn: the
/// HTTP door has a peer to fill `host` from or refuse behind, takes
/// `service` as sent, and asserts no slot.
const UNREACHABLE_REPAIRS: [&str; 3] = [
    "host.omitted",
    "service.from_profile",
    "field.producer_asserted",
];

/// Reject reasons storage decides after canonicalization, which the
/// preview never reaches: a failed WAL write and the hot buffer's two
/// admission refusals.
const UNREACHABLE_REJECTS: [&str; 3] = ["wal_failure", "hot_buffer_full", "ingest_batch_too_large"];

/// One sample covering every repair the HTTP door can make on a server
/// with no trusted relay, every per-event reject but the relay one, and
/// the index rule (a blank line). Each event that should be accepted
/// carries a unique `id`, the only key the WAL is matched on.
///
/// Times sit far from the plausibility window's edges: an hour ago for a
/// plausible time, 2001 for an implausible one.
fn plain_differential_body() -> String {
    let recent = (chrono::Utc::now() - chrono::Duration::hours(1))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let long_name = "k".repeat(300);
    let lines = [
        json!({"id": "clean", "service": "diff", "env": "prod", "host": "h1",
               "_time": recent, "severity": "info", "message": "clean"}),
        json!({"id": "filled", "service": "diff", "message": "host, env and time filled"}),
        json!({"id": "old", "service": "diff", "host": "h1", "_time": "2001-02-03T04:05:06Z"}),
        json!({"id": "bad-time", "service": "diff", "host": "h1", "timestamp": "not a time",
               "level": "gold"}),
        json!({"id": "raw", "service": "diff", "host": "h1", "_raw": "r".repeat(70_000)}),
        json!({"id": "prefix", "service": "diff", "host": "h1", "_custom": "v", "_": "gone"}),
        json!({"id": "prefix-collision", "service": "diff", "host": "h1", "_dup": "a", "dup": "b"}),
        json!({"id": "too-long", "service": "diff", "host": "h1", long_name: 1}),
        json!({"id": "folded", "service": "diff", "host": "h1", "Ctx": {"a": [1, 2]},
               "Level": "warn"}),
        json!({"id": "collision", "service": "diff-other", "host": "h1", "FOO": 1, "foo": 2}),
        json!({"message": "no service"}),
        json!({"service": 5}),
        json!({"service": ""}),
        json!({"service": "s".repeat(129)}),
        json!({"service": "bad svc"}),
        json!({"service": "diff", "env": "Not An Env"}),
        json!({"service": "diff", "env": "staging"}),
        json!(42),
    ];
    let mut body: Vec<String> = lines.iter().map(Value::to_string).collect();
    body.insert(3, String::new());
    body.push("not json".to_owned());
    body.join("\n")
}

/// Every `*.ndjson` line under the server's WAL directory, keyed by `id`.
fn wal_events_by_id(server: &TestServer) -> BTreeMap<String, serde_json::Map<String, Value>> {
    let dir = server
        .state
        .ingest
        .wal_writer
        .as_ref()
        .expect("ingest is enabled")
        .dir()
        .to_path_buf();
    let mut out = BTreeMap::new();
    let mut stack = vec![dir];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "ndjson") {
                for line in std::fs::read_to_string(&path).unwrap().lines() {
                    let event: serde_json::Map<String, Value> = serde_json::from_str(line).unwrap();
                    let id = event["id"]
                        .as_str()
                        .expect("every event has an id")
                        .to_owned();
                    assert!(
                        out.insert(id.clone(), event).is_none(),
                        "{id} twice in the WAL"
                    );
                }
            }
        }
    }
    out
}

/// Send `body` through real ingest (from 127.0.0.1) and through the
/// preview with `peer_ip=127.0.0.1` on the same server, and require the
/// same result: every accepted event equal to its WAL line field by field,
/// and the same rejected `(index, reason, message)` set. Returns the
/// repair codes and reject reasons the sample covered.
async fn assert_preview_matches_ingest(
    server: &TestServer,
    body: &str,
) -> (BTreeSet<String>, BTreeSet<String>) {
    let response = raw_client()
        .post(format!("{}/api/v1/ingest", server.url))
        .bearer_auth(&server.ingest_token)
        .header("content-type", "application/x-ndjson")
        .body(body.to_owned())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let ingest: trawl_api::IngestResponse = response.json().await.unwrap();
    let report = preview_ok(server, "?peer_ip=127.0.0.1", body).await;
    assert_eq!(report.peer.ip, "127.0.0.1", "the peer real ingest saw");
    assert_eq!(
        (report.accepted, report.rejected),
        (ingest.accepted, ingest.rejected)
    );

    let mut wal = wal_events_by_id(server);
    let mut repairs = BTreeSet::new();
    let mut rejects = BTreeSet::new();
    let mut preview_errors = BTreeSet::new();
    for event in &report.events {
        match event {
            PreviewEvent::Accepted {
                event,
                repairs: codes,
                lineage,
                ..
            } => {
                let id = event["id"].as_str().unwrap();
                let mut stored = wal
                    .remove(id)
                    .unwrap_or_else(|| panic!("{id} is not in the WAL"));
                let mut previewed = event.clone();
                assert_eq!(previewed["_ingested"], report.arrival.as_str());
                for map in [&mut stored, &mut previewed] {
                    map.remove("_ingested").expect("both stamp arrival");
                }
                if let TimeLineage::Arrival { .. } = lineage.time {
                    // Each route's own arrival filled `_time`.
                    assert_eq!(event["_time"], report.arrival.as_str(), "{id}");
                    for map in [&mut stored, &mut previewed] {
                        map.remove("_time").expect("arrival filled it");
                    }
                }
                assert_eq!(previewed, stored, "{id}: preview and WAL differ");
                repairs.extend(codes.iter().cloned());
            }
            PreviewEvent::Rejected {
                index,
                reason,
                message,
                ..
            } => {
                preview_errors.insert((*index, reason.clone(), message.clone()));
                rejects.insert(reason.clone());
            }
        }
    }
    assert!(
        wal.is_empty(),
        "WAL lines the preview did not accept: {wal:?}"
    );
    let ingest_errors: BTreeSet<_> = ingest
        .errors
        .into_iter()
        .map(|e| (e.index, e.reason, e.message))
        .collect();
    assert_eq!(preview_errors, ingest_errors, "the rejections differ");
    (repairs, rejects)
}

/// AC4: the preview's accepted events equal the WAL lines real ingest
/// wrote for the same body, field by field, and its rejections equal real
/// ingest's, across every repair code and canonicalization reject reason
/// the HTTP door can reach. The relay rejection needs a relay server, so it
/// is a second leg on one.
#[tokio::test(flavor = "multi_thread")]
async fn preview_matches_real_ingest_wal_field_by_field() {
    let plain = setup().await;
    let (mut repairs, mut rejects) =
        assert_preview_matches_ingest(&plain, &plain_differential_body()).await;

    let relay = setup_with_trusted_relays(&["127.0.0.1/32"]).await;
    let relay_body = [
        json!({"id": "relayed", "service": "diff", "host": "origin"}),
        json!({"service": "diff", "message": "no host behind a relay"}),
    ]
    .map(|line| line.to_string())
    .join("\n");
    let (relay_repairs, relay_rejects) = assert_preview_matches_ingest(&relay, &relay_body).await;
    repairs.extend(relay_repairs);
    rejects.extend(relay_rejects);

    let all_repairs: BTreeSet<String> = RepairCode::ALL
        .iter()
        .map(|code| code.as_str().to_owned())
        .collect();
    let unreachable: BTreeSet<String> = UNREACHABLE_REPAIRS.map(str::to_owned).into();
    assert!(repairs.is_disjoint(&unreachable), "{repairs:?}");
    assert_eq!(
        &repairs | &unreachable,
        all_repairs,
        "every repair code is covered or named unreachable"
    );

    let all_rejects: BTreeSet<String> = RejectReason::ALL
        .iter()
        .map(|reason| reason.as_str().to_owned())
        .collect();
    let unreachable: BTreeSet<String> = UNREACHABLE_REJECTS.map(str::to_owned).into();
    assert!(rejects.is_disjoint(&unreachable), "{rejects:?}");
    assert_eq!(
        &rejects | &unreachable,
        all_rejects,
        "every reject reason is covered or named unreachable"
    );
}

// -- the repairs label set ------------------------------------------------------

/// A preview never admits a service name to the bounded repairs label set
/// (ADR-0009's cap of 256): after 300 previews of distinct invented
/// services, each with a repair, a real ingest of a new service still gets
/// its own `trawl_ingest_repairs_total` label, not the overflow one.
///
/// The set is process-wide. Under nextest this test is its own process;
/// under `cargo test` the other tests here admit only a handful of names,
/// far from the cap, so 300 leaked previews would still cross it.
#[tokio::test(flavor = "multi_thread")]
async fn previewed_services_never_take_a_repair_label() {
    const PREVIEWED: usize = 300;
    let server = setup_with_rate_limit(RateLimitConfig {
        default_rpm: 0,
        ingest_rpm: 0,
    })
    .await;
    for n in 0..PREVIEWED {
        // No env and no host: `env.defaulted` and `host.from_peer`.
        let body = json!({"service": format!("invented-{n}")}).to_string();
        let report = preview_ok(&server, "?peer_ip=10.0.0.1", &body).await;
        let (_, repairs, _) = accepted(&report.events[0]);
        assert!(!repairs.is_empty(), "preview {n} carried a repair");
    }

    let response = raw_client()
        .post(format!("{}/api/v1/ingest", server.url))
        .bearer_auth(&server.ingest_token)
        .body(json!({"service": "label-probe", "message": "m"}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let metrics = raw_client()
        .get(format!("{}/metrics", server.url))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let repairs: Vec<&str> = metrics
        .lines()
        .filter(|line| line.starts_with("trawl_ingest_repairs_total"))
        .collect();
    assert!(
        repairs
            .iter()
            .any(|line| line.contains("service=\"label-probe\"")
                && line.contains("code=\"env.defaulted\"")),
        "the new service has its own label: {repairs:#?}"
    );
    let overflow = format!("service=\"{OVERFLOW_SERVICE_LABEL}\"");
    assert!(
        !repairs.iter().any(|line| line.contains(&overflow)),
        "{repairs:#?}"
    );
    assert!(
        !metrics.contains("invented-"),
        "no previewed service reached /metrics"
    );
}
