// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTTP ingest is admitted against hot-buffer capacity (ADR-0043).
//!
//! Every test boots a real server with small hot-buffer caps and fills it
//! over HTTP. The fixture runs no compaction loop, so nothing drains and the
//! buffer holds exactly what the test acknowledged.
//!
//! Its own test binary because it installs the production subscriber as
//! the global one, as `http_failure.rs` does, to count the WARN lines a
//! refusal produces. Tests tell their lines apart by request id.

mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use common::{HotBufferKnobs, TestServer, setup_with_hot_buffer};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::util::SubscriberInitExt as _;
use trawl_client::HttpClient;
use trawl_server::hot_buffer::{Charge, external_ceiling};
use trawl_server::telemetry::{self, LogSinks};

/// The compaction interval every fixture here boots with. Not a default,
/// so a `Retry-After` equal to it came from the config.
const INTERVAL_SECS: u64 = 37;

/// Room for `max_events` events and bytes enough that only the event
/// dimension binds.
fn event_caps(max_events: usize) -> HotBufferKnobs {
    HotBufferKnobs {
        max_events,
        max_bytes: 16 * 1024 * 1024,
        compaction_interval_secs: INTERVAL_SECS,
    }
}

// -- the production subscriber, captured ------------------------------------

/// Everything the global subscriber writes to stdout.
#[derive(Clone, Default)]
struct Stdout(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Stdout {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Stdout {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// The production subscriber under the default directives, installed once
/// for the whole binary, writing to a captured stdout.
fn stdout() -> &'static Stdout {
    static STDOUT: OnceLock<Stdout> = OnceLock::new();
    STDOUT.get_or_init(|| {
        let stdout = Stdout::default();
        let (subscriber, _) = telemetry::build_subscriber(
            telemetry::DEFAULT_LOG_FILTER,
            LogSinks {
                stdout: Some(stdout.clone()),
                wal: None,
                file_log: false,
            },
        );
        subscriber.init();
        stdout
    })
}

/// Drop ANSI SGR sequences, so a line reads the same with or without
/// colour.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Every stdout line that mentions `request_id`, colour removed.
fn lines_for(request_id: &str) -> Vec<String> {
    let bytes = stdout().0.lock().unwrap().clone();
    String::from_utf8(bytes)
        .unwrap()
        .lines()
        .map(strip_ansi)
        .filter(|line| line.contains(request_id))
        .collect()
}

/// A field's value on a formatted stdout line: `name="quoted"` or
/// `name=bare`.
fn field(line: &str, name: &str) -> Option<String> {
    let at = line.find(&format!(" {name}="))? + name.len() + 2;
    let rest = &line[at..];
    if let Some(quoted) = rest.strip_prefix('"') {
        quoted.find('"').map(|end| quoted[..end].to_owned())
    } else {
        Some(rest.split_whitespace().next().unwrap_or("").to_owned())
    }
}

/// The level a formatted stdout line was logged at.
fn level(line: &str) -> &'static str {
    ["ERROR", "WARN", "INFO", "DEBUG", "TRACE"]
        .into_iter()
        .find(|level| line.split_whitespace().any(|word| word == *level))
        .unwrap_or("none")
}

/// The WARN and ERROR lines one request produced.
fn loud_lines_for(request_id: &str) -> Vec<String> {
    lines_for(request_id)
        .into_iter()
        .filter(|line| matches!(level(line), "WARN" | "ERROR"))
        .collect()
}

// -- requests -----------------------------------------------------------------

/// `count` ndjson events for `service`, with ids `{prefix}-{n}`, and those
/// ids.
fn events(service: &str, prefix: &str, count: usize) -> (Vec<u8>, Vec<String>) {
    events_padded(service, prefix, count, 0)
}

/// Like [`events`], each carrying a `pad` field of `pad` bytes.
fn events_padded(service: &str, prefix: &str, count: usize, pad: usize) -> (Vec<u8>, Vec<String>) {
    let padding = "p".repeat(pad);
    let mut body = Vec::new();
    let mut ids = Vec::new();
    for n in 0..count {
        let id = format!("{prefix}-{n}");
        let event = serde_json::json!({
            "service": service,
            "id": id,
            "message": "admission probe",
            "pad": padding,
        });
        serde_json::to_writer(&mut body, &event).unwrap();
        body.push(b'\n');
        ids.push(id);
    }
    (body, ids)
}

/// POST `body` to the ingest endpoint as the fixture's ingest key.
async fn post(server: &TestServer, body: Vec<u8>, gzip: bool) -> reqwest::Response {
    let mut request = common::harness_client_builder()
        .build()
        .unwrap()
        .post(format!("{}/api/v1/ingest", server.url))
        .bearer_auth(&server.ingest_token)
        .header("content-type", "application/x-ndjson");
    if gzip {
        request = request.header("content-encoding", "gzip");
    }
    request.body(body).send().await.unwrap()
}

/// POST `body` and require a 200 that accepted every event in it.
async fn acknowledge(server: &TestServer, body: Vec<u8>, expected: usize) {
    let response = post(server, body, false).await;
    assert_eq!(response.status(), 200, "{:?}", response.text().await);
    let answer: trawl_api::IngestResponse = response.json().await.unwrap();
    assert_eq!(answer.accepted, expected, "{answer:?}");
    assert_eq!(answer.rejected, 0, "{answer:?}");
}

fn request_id_of(response: &reqwest::Response) -> String {
    response
        .headers()
        .get("x-request-id")
        .expect("every response carries X-Request-Id")
        .to_str()
        .unwrap()
        .to_owned()
}

/// Read a refusal: its status, its `Retry-After`, its error code and its
/// request id.
async fn refusal(response: reqwest::Response) -> (u16, Option<String>, String, String) {
    let status = response.status().as_u16();
    let request_id = request_id_of(&response);
    let retry_after = response
        .headers()
        .get("retry-after")
        .map(|value| value.to_str().unwrap().to_owned());
    let body: serde_json::Value = response.json().await.unwrap();
    let code = body["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    (status, retry_after, code, request_id)
}

fn hot(server: &TestServer) -> &trawl_server::hot_buffer::HotBuffer {
    server
        .state
        .query
        .hot_buffer
        .as_deref()
        .expect("ingest is enabled")
}

fn wal_dir(server: &TestServer) -> PathBuf {
    server
        .state
        .ingest
        .wal_writer
        .as_ref()
        .expect("ingest is enabled")
        .dir()
        .to_path_buf()
}

/// Every path under `dir`, recursively, with its size.
fn listing(dir: &Path) -> BTreeSet<(PathBuf, u64)> {
    let mut out = BTreeSet::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let entry = entry.unwrap();
            let meta = entry.metadata().unwrap();
            if meta.is_dir() {
                stack.push(entry.path());
            }
            out.insert((entry.path(), meta.len()));
        }
    }
    out
}

/// Fill a buffer whose event cap is `max_events` to exactly the HTTP
/// ceiling with `service` events in batches of `batch`, and return the
/// acknowledged ids.
async fn fill_to_ceiling(
    server: &TestServer,
    max_events: usize,
    service: &str,
    batch: usize,
) -> Vec<String> {
    let ceiling = external_ceiling(Charge {
        events: max_events,
        bytes: usize::MAX,
    })
    .events;
    let mut acked = Vec::new();
    let mut round = 0;
    while acked.len() < ceiling {
        let count = batch.min(ceiling - acked.len());
        let (body, ids) = events(service, &format!("{service}-fill{round}"), count);
        acknowledge(server, body, count).await;
        acked.extend(ids);
        round += 1;
    }
    assert_eq!(hot(server).charged().events, ceiling);
    acked
}

/// Every `id` a query for `service` returns.
async fn searchable_ids(server: &TestServer, service: &str) -> Vec<String> {
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();
    let result = client
        .query_paginated(&format!("service={service} last=1h"), Some(10_000), None)
        .await
        .unwrap();
    let column = result
        .result
        .columns
        .iter()
        .position(|c| c.name == "id")
        .expect("an id column");
    result
        .result
        .rows
        .iter()
        .map(|row| match &row[column] {
            trawl_api::value::Value::String(id) => id.clone(),
            other => panic!("id is a string, got {other:?}"),
        })
        .collect()
}

// -- AC2 ------------------------------------------------------------------------

/// Filling the buffer over HTTP ends in refusals, never in eviction: every
/// event any request was acknowledged for stays searchable, and nothing
/// from a refused request is.
#[tokio::test(flavor = "multi_thread")]
async fn refused_ingest_keeps_every_acknowledged_event_searchable() {
    stdout();
    // 64 events: the HTTP ceiling is 60.
    let server = setup_with_hot_buffer(event_caps(64)).await;
    let service = "adm-ac2";
    let mut acked = Vec::new();
    let mut refused = Vec::new();

    // Eight batches of seven fit (56); the ninth would reach 63.
    for round in 0..9 {
        let (body, ids) = events(service, &format!("r{round}"), 7);
        let response = post(&server, body, false).await;
        if round < 8 {
            assert_eq!(response.status(), 200, "round {round}");
            let answer: trawl_api::IngestResponse = response.json().await.unwrap();
            assert_eq!(answer.accepted, 7);
            acked.extend(ids);
        } else {
            let (status, retry_after, code, _) = refusal(response).await;
            assert_eq!((status, code.as_str()), (503, "hot_buffer_full"));
            assert_eq!(retry_after.as_deref(), Some("37"));
            refused.extend(ids);
        }
    }
    // What still fits is admitted: exactly the ceiling.
    let (body, ids) = events(service, "tail", 4);
    acknowledge(&server, body, 4).await;
    acked.extend(ids);
    assert_eq!(hot(&server).charged().events, 60);
    // Then nothing is, and the refusal comes before any parsing.
    let (body, ids) = events(service, "late", 1);
    let (status, _, code, _) = refusal(post(&server, body, false).await).await;
    assert_eq!((status, code.as_str()), (503, "hot_buffer_full"));
    refused.extend(ids);

    let found: BTreeSet<String> = searchable_ids(&server, service).await.into_iter().collect();
    let acked: BTreeSet<String> = acked.into_iter().collect();
    assert_eq!(acked.len(), 60);
    assert_eq!(found, acked, "every acknowledged event, and only those");
    for id in &refused {
        assert!(!found.contains(id), "refused {id} became searchable");
    }
}

// -- AC4 ------------------------------------------------------------------------

/// A request that fits the ceiling but not the free space is 503
/// `hot_buffer_full` with `Retry-After` equal to the configured compaction
/// interval, and it writes nothing to the WAL.
#[tokio::test(flavor = "multi_thread")]
async fn full_buffer_is_503_with_retry_after_and_writes_nothing() {
    stdout();
    let server = setup_with_hot_buffer(event_caps(64)).await;
    // 56 of 60: a request of 7 passes the early check and is refused at
    // the reservation, after parsing.
    for round in 0..8 {
        let (body, _) = events("adm-ac4", &format!("r{round}"), 7);
        acknowledge(&server, body, 7).await;
    }
    let before = listing(&wal_dir(&server));
    assert!(
        !before.is_empty(),
        "the acknowledged batches are in the WAL"
    );
    let charged = hot(&server).charged();

    let (body, _) = events("adm-ac4", "refused", 7);
    let (status, retry_after, code, _) = refusal(post(&server, body, false).await).await;
    assert_eq!(status, 503);
    assert_eq!(code, "hot_buffer_full");
    assert_eq!(retry_after, Some(INTERVAL_SECS.to_string()));
    assert_eq!(listing(&wal_dir(&server)), before, "the WAL is unchanged");
    assert_eq!(hot(&server).charged(), charged, "nothing stays reserved");

    // With no free space left at all, the early refusal answers the same.
    let (body, _) = events("adm-ac4", "fill", 4);
    acknowledge(&server, body, 4).await;
    let before = listing(&wal_dir(&server));
    let (body, _) = events("adm-ac4", "early", 1);
    let (status, retry_after, code, _) = refusal(post(&server, body, false).await).await;
    assert_eq!((status, code.as_str()), (503, "hot_buffer_full"));
    assert_eq!(retry_after, Some(INTERVAL_SECS.to_string()));
    assert_eq!(listing(&wal_dir(&server)), before, "the WAL is unchanged");
}

// -- AC5 ------------------------------------------------------------------------

/// The `ingest_batch_too_large` series on `/metrics`.
async fn too_large_rejections(server: &TestServer) -> u64 {
    let body = common::harness_client_builder()
        .build()
        .unwrap()
        .get(format!("{}/metrics", server.url))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    body.lines()
        .find(|line| {
            line.starts_with("trawl_ingest_events_rejected_total")
                && line.contains("reason=\"ingest_batch_too_large\"")
        })
        .map_or(0, |line| {
            line.rsplit(' ').next().unwrap().trim().parse().unwrap()
        })
}

/// A request larger than the HTTP ceiling is 413 `ingest_batch_too_large`
/// with no `Retry-After`, on an empty buffer, on either dimension. Its
/// valid events count under that reason, and nothing is written.
#[tokio::test(flavor = "multi_thread")]
async fn oversized_request_is_413_without_retry_after_on_either_dimension() {
    stdout();

    // Events: a cap of 16 admits 15 per request.
    let server = setup_with_hot_buffer(event_caps(16)).await;
    let before = listing(&wal_dir(&server));
    let counted = too_large_rejections(&server).await;
    let (body, _) = events("adm-ac5", "events", 16);
    let (status, retry_after, code, _) = refusal(post(&server, body, false).await).await;
    assert_eq!((status, code.as_str()), (413, "ingest_batch_too_large"));
    assert_eq!(retry_after, None);
    assert_eq!(hot(&server).charged(), Charge::ZERO, "the buffer was empty");
    assert_eq!(listing(&wal_dir(&server)), before, "nothing was written");
    assert_eq!(too_large_rejections(&server).await - counted, 16);
    // One event fewer fits.
    let (body, _) = events("adm-ac5", "fits", 15);
    acknowledge(&server, body, 15).await;

    // Bytes: a cap of 4096 admits 3840 serialized bytes per request, and
    // three events padded to 1 KiB each (about twice that serialized, the
    // pad rides in `_raw` too) exceed it on bytes alone. One fits.
    let server = setup_with_hot_buffer(HotBufferKnobs {
        max_events: 1_000,
        max_bytes: 4096,
        compaction_interval_secs: INTERVAL_SECS,
    })
    .await;
    let before = listing(&wal_dir(&server));
    let (body, _) = events_padded("adm-ac5", "bytes", 3, 1024);
    let (status, retry_after, code, _) = refusal(post(&server, body, false).await).await;
    assert_eq!((status, code.as_str()), (413, "ingest_batch_too_large"));
    assert_eq!(retry_after, None);
    assert_eq!(hot(&server).charged(), Charge::ZERO, "the buffer was empty");
    assert_eq!(listing(&wal_dir(&server)), before, "nothing was written");
    let (body, _) = events_padded("adm-ac5", "one", 1, 1024);
    acknowledge(&server, body, 1).await;
}

// -- AC6 ------------------------------------------------------------------------

/// With no free space, a body is refused before it is decompressed: a
/// broken gzip stream is 503, not the 400 its decompression would earn.
#[tokio::test(flavor = "multi_thread")]
async fn full_buffer_refuses_before_decompression() {
    const BROKEN: &[u8] = b"\x1f\x8bnot-gzip";
    stdout();
    let server = setup_with_hot_buffer(event_caps(64)).await;

    // On an empty buffer the same body is a decompression 400.
    let response = post(&server, BROKEN.to_vec(), true).await;
    assert_eq!(response.status(), 400, "{:?}", response.text().await);

    fill_to_ceiling(&server, 64, "adm-ac6", 20).await;
    let (status, retry_after, code, _) = refusal(post(&server, BROKEN.to_vec(), true).await).await;
    assert_eq!((status, code.as_str()), (503, "hot_buffer_full"));
    assert_eq!(retry_after, Some(INTERVAL_SECS.to_string()));
}

// -- AC7 ------------------------------------------------------------------------

/// A refusal never waits on the publication gate: with the gate's write
/// side held, as compaction holds it to drain, both refusal points answer
/// within two seconds.
#[tokio::test(flavor = "multi_thread")]
async fn refusal_does_not_wait_on_the_publication_gate() {
    stdout();
    let server = setup_with_hot_buffer(event_caps(64)).await;
    for round in 0..8 {
        let (body, _) = events("adm-ac7", &format!("r{round}"), 7);
        acknowledge(&server, body, 7).await;
    }
    let publication = hot(&server).publication();
    let held = publication.write().await;

    // Refused at the reservation (56 + 7 > 60).
    let (body, _) = events("adm-ac7", "reserve", 7);
    let response = tokio::time::timeout(Duration::from_secs(2), post(&server, body, false))
        .await
        .expect("the reservation refusal must not wait for the gate");
    assert_eq!(refusal(response).await.0, 503);

    drop(held);
    let (body, _) = events("adm-ac7", "fill", 4);
    acknowledge(&server, body, 4).await;
    let held = publication.write().await;

    // Refused early, before parsing (60 of 60).
    let (body, _) = events("adm-ac7", "early", 1);
    let response = tokio::time::timeout(Duration::from_secs(2), post(&server, body, false))
        .await
        .expect("the early refusal must not wait for the gate");
    assert_eq!(refusal(response).await.0, 503);
    drop(held);
}

// -- AC8 ------------------------------------------------------------------------

/// Each 503 is exactly one WARN, the failure observer's `http_failure`
/// with `cause_kind` `hot_buffer_full`, and the handler adds no line of
/// its own. A 413 is the client's error and logs no WARN at all.
#[tokio::test(flavor = "multi_thread")]
async fn each_503_is_one_warn_http_failure_and_a_413_is_none() {
    stdout();
    let server = setup_with_hot_buffer(event_caps(64)).await;
    for round in 0..8 {
        let (body, _) = events("adm-ac8", &format!("r{round}"), 7);
        acknowledge(&server, body, 7).await;
    }

    let mut refused = Vec::new();
    // The reservation refusal, then (once full) the early one.
    let (body, _) = events("adm-ac8", "reserve", 7);
    refused.push(refusal(post(&server, body, false).await).await);
    let (body, _) = events("adm-ac8", "fill", 4);
    acknowledge(&server, body, 4).await;
    let (body, _) = events("adm-ac8", "early", 1);
    refused.push(refusal(post(&server, body, false).await).await);

    for (status, _, code, request_id) in &refused {
        assert_eq!((*status, code.as_str()), (503, "hot_buffer_full"));
        let loud = loud_lines_for(request_id);
        assert_eq!(loud.len(), 1, "one WARN for {request_id}: {loud:#?}");
        let line = &loud[0];
        assert_eq!(level(line), "WARN", "{line}");
        assert_eq!(
            field(line, "event_type").as_deref(),
            Some("http_failure"),
            "{line}"
        );
        assert_eq!(
            field(line, "cause_kind").as_deref(),
            Some("hot_buffer_full"),
            "{line}"
        );
        assert_eq!(field(line, "status").as_deref(), Some("503"), "{line}");
        assert_eq!(
            field(line, "route").as_deref(),
            Some("/api/v1/ingest"),
            "{line}"
        );
    }

    // A 413, on a buffer small enough that one request cannot fit.
    let small = setup_with_hot_buffer(event_caps(16)).await;
    let (body, _) = events("adm-ac8", "oversized", 16);
    let (status, _, code, request_id) = refusal(post(&small, body, false).await).await;
    assert_eq!((status, code.as_str()), (413, "ingest_batch_too_large"));
    let loud = loud_lines_for(&request_id);
    assert!(loud.is_empty(), "a 413 logs no WARN: {loud:#?}");
}

// -- AC9 (HTTP half) ----------------------------------------------------------

/// The self-telemetry reserve is entitled by the code path, never by the
/// event: HTTP events that call themselves `service=trawld` stop at the
/// HTTP ceiling, with the reserve still free.
#[tokio::test(flavor = "multi_thread")]
async fn a_trawld_service_name_over_http_gets_no_reserve() {
    stdout();
    let server = setup_with_hot_buffer(event_caps(64)).await;
    let acked = fill_to_ceiling(&server, 64, "trawld", 20).await;
    assert_eq!(acked.len(), 60, "the HTTP ceiling, not the full cap");

    let (body, _) = events("trawld", "spoof", 1);
    let (status, _, code, _) = refusal(post(&server, body, false).await).await;
    assert_eq!((status, code.as_str()), (503, "hot_buffer_full"));
    let charged = hot(&server).charged();
    assert_eq!(charged.events, 60);
    assert!(
        charged.events < 64,
        "the reserve stays free for self-telemetry: {charged:?}"
    );

    // A spoofed request that would fit only inside the reserve is refused
    // too, from a partly filled buffer.
    let server = setup_with_hot_buffer(event_caps(64)).await;
    let (body, _) = events("trawld", "partial", 58);
    acknowledge(&server, body, 58).await;
    let (body, _) = events("trawld", "into-reserve", 4);
    let (status, _, code, _) = refusal(post(&server, body, false).await).await;
    assert_eq!((status, code.as_str()), (503, "hot_buffer_full"));
    assert_eq!(hot(&server).charged().events, 58);
}
