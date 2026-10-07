// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! An ingest preview stores nothing of its sample (ADR-0049, #200 AC5).
//!
//! The source scan in `ingest_preview_touches_nothing.rs` says what the
//! preview code may not name. This binary watches a real trawld instead:
//! the WAL, the hot buffer, the counters, the live stream, trawld's own
//! persisted telemetry and its log lines, around previews whose samples
//! carry a canary in every input position.
//!
//! Its own test binary because it installs trawld's subscriber as the
//! process's global one, with a WAL sink for self-telemetry and a captured
//! stdout, the way `query_timing_roundtrip.rs` and `ingest_admission.rs`
//! do. A process has one global subscriber, installed once here.

mod common;

use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use common::{TestServer, setup_with_rate_limit};
use serde_json::{Value, json};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::util::SubscriberInitExt as _;
use trawl_server::config::{IngestConfig, RateLimitConfig};
use trawl_server::hot_buffer::Charge;
use trawl_server::ingest::producer::Derivation;
use trawl_server::telemetry::{self, LogSinks, WalHandle, WalLayer};

/// Every canary starts with this, in some ASCII case. No line trawld
/// writes about these requests may contain it, in any case.
const CANARY: &str = "cnry";

/// trawld's default directives with debug logging on for trawld and for
/// tower-http, so the request lines a preview may produce are all there to
/// be read.
const DEBUG_FILTER: &str = "trawl_server=debug,tower_http=debug,trawld=info,fleet_auth=info,\
                            auth.backend=info,storage.backend=info,preauth.transport=info";

/// Neither rate bucket binds the test's requests.
const UNLIMITED: RateLimitConfig = RateLimitConfig {
    default_rpm: 0,
    ingest_rpm: 0,
};

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

/// The subscriber's two sinks, as the test reads them.
struct Telemetry {
    stdout: Stdout,
    handle: WalHandle,
    layer: WalLayer,
}

/// trawld's subscriber under [`DEBUG_FILTER`], with a WAL sink and a
/// captured stdout, installed once for the whole binary. The WAL sink
/// writes nothing until [`WalHandle::set`] names a server's WAL writer.
fn telemetry() -> &'static Telemetry {
    static TELEMETRY: OnceLock<Telemetry> = OnceLock::new();
    TELEMETRY.get_or_init(|| {
        let defaults = IngestConfig::default();
        let handle = WalHandle::new();
        let layer = WalLayer::new_with_buffer_cap(
            handle.clone(),
            &defaults.effective_envs(),
            &defaults.default_env,
            Arc::new(Derivation::defaults()),
            defaults.telemetry_buffer_max_bytes,
        );
        let stdout = Stdout::default();
        let (subscriber, _) = telemetry::build_subscriber(
            DEBUG_FILTER,
            LogSinks {
                stdout_ansi: false,
                stdout: Some(stdout.clone()),
                wal: Some(layer.clone()),
                file_log: false,
            },
        );
        subscriber.init();
        Telemetry {
            stdout,
            handle,
            layer,
        }
    })
}

/// Everything captured on stdout so far.
fn captured_stdout() -> String {
    String::from_utf8(telemetry().stdout.0.lock().unwrap().clone()).unwrap()
}

/// Whether `text` holds a canary, in any ASCII case.
fn has_canary(text: &str) -> bool {
    text.to_ascii_lowercase().contains(CANARY)
}

// -- requests -----------------------------------------------------------------

fn client() -> reqwest::Client {
    common::harness_client_builder().build().unwrap()
}

/// POST `body` to the preview as the admin key, `query` appended verbatim,
/// with any extra headers.
async fn preview(
    server: &TestServer,
    query: &str,
    headers: &[(&str, &str)],
    body: impl Into<reqwest::Body>,
) -> reqwest::Response {
    let mut request = client()
        .post(format!("{}/api/v1/ingest/preview{query}", server.url))
        .bearer_auth(&server.admin_token)
        .header("content-type", "application/x-ndjson");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request.body(body).send().await.unwrap()
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

/// POST NDJSON to real ingest as the ingest key, requiring every event in
/// it accepted.
async fn ingest(server: &TestServer, events: &[Value]) {
    let body: Vec<String> = events.iter().map(Value::to_string).collect();
    let response = client()
        .post(format!("{}/api/v1/ingest", server.url))
        .bearer_auth(&server.ingest_token)
        .header("content-type", "application/x-ndjson")
        .body(body.join("\n"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let answer: trawl_api::IngestResponse = response.json().await.unwrap();
    assert_eq!((answer.accepted, answer.rejected), (events.len(), 0));
}

async fn metrics_text(server: &TestServer) -> String {
    client()
        .get(format!("{}/metrics", server.url))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

// -- what a preview must leave alone --------------------------------------------

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

/// Every line of every file under the WAL directory.
fn wal_lines(server: &TestServer) -> Vec<String> {
    listing(&wal_dir(server))
        .into_iter()
        .filter(|(path, _)| path.is_file())
        .flat_map(|(path, _)| {
            std::fs::read_to_string(&path)
                .unwrap()
                .lines()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Everything a preview must leave unchanged.
#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    wal: BTreeSet<(PathBuf, u64)>,
    charged: Charge,
    /// The ingest, repair, reject and unmapped-severity series on
    /// `/metrics`, verbatim. A set: the exporter renders in no fixed order.
    counters: BTreeSet<String>,
    /// `/stats`, less its uptime.
    stats: Value,
    /// The monitor's ingest totals.
    totals: (u64, u64),
}

const COUNTER_SERIES: [&str; 5] = [
    "trawl_ingest_events_total",
    "trawl_ingest_events_rejected_total",
    "trawl_ingest_repairs_total",
    "trawl_ingest_profile_reject_total",
    "trawl_severity_unmapped_total",
];

async fn snapshot(server: &TestServer) -> Snapshot {
    let counters = metrics_text(server)
        .await
        .lines()
        .filter(|line| COUNTER_SERIES.iter().any(|name| line.starts_with(name)))
        .map(str::to_owned)
        .collect();
    let mut stats: Value = client()
        .get(format!("{}/api/v1/stats", server.url))
        .bearer_auth(&server.admin_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    stats
        .as_object_mut()
        .unwrap()
        .remove("uptime_secs")
        .expect("stats carries its uptime");
    let ingest = &server.state.ingest;
    Snapshot {
        wal: listing(&wal_dir(server)),
        charged: server
            .state
            .query
            .hot_buffer
            .as_deref()
            .expect("ingest is enabled")
            .charged(),
        counters,
        stats,
        totals: (
            ingest
                .total_events
                .load(std::sync::atomic::Ordering::Relaxed),
            ingest
                .total_rejected
                .load(std::sync::atomic::Ordering::Relaxed),
        ),
    }
}

// -- the samples ------------------------------------------------------------------

/// A sample with a canary in every input position: keys and values, top
/// level and nested, `_raw`, `service`, `env`, `host`, `message`, and each
/// time and severity source. It holds a clean acceptance, a repaired one,
/// a fold, and rejections that quote their input.
fn canary_sample() -> String {
    let lines = [
        json!({"service": "cnry-svc-clean", "env": "prod", "host": "cnry-host-1",
               "_time": "2026-01-01T00:00:00Z", "severity": "info",
               "message": "cnry-msg-clean", "cnry_key_top": "cnry-val-top"}),
        json!({"service": "cnry-svc-repaired", "message": "cnry-msg-repaired",
               "_time": "cnry-time-unparseable", "timestamp": "cnry-time-ts",
               "@timestamp": "cnry-time-at", "severity": "cnry-sev-unmapped",
               "severity_text": "cnry-sevtext", "level": "cnry-level",
               "_raw": "cnry-raw-proposed", "_cnry_reserved": "cnry-val-reserved",
               "CNRY_Fold": "cnry-val-fold",
               "cnry_nest": {"cnry_inner": ["cnry-val-nested", {"cnry_deep": 1}]}}),
        json!({"service": "cnry bad svc", "message": "cnry-msg-badsvc"}),
        json!({"service": "cnry-svc-env", "env": "cnry-env", "host": "cnry-host-2"}),
        json!({"service": "cnry-svc-badenv", "env": "CNRY ENV"}),
        json!({"message": "cnry-msg-noservice", "host": "cnry-host-3"}),
        json!(["cnry-not-an-object"]),
    ];
    let mut body: Vec<String> = lines.iter().map(Value::to_string).collect();
    body.push("cnry-not-json".to_owned());
    body.join("\n")
}

/// A canary line repeated to `count` positions.
fn repeated(count: usize) -> String {
    let line = json!({"service": "cnry-svc-many", "message": "cnry-msg-many"}).to_string();
    vec![line; count].join("\n")
}

/// The one sample, gzip-compressed.
fn gzipped(body: &str) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(body.as_bytes()).unwrap();
    encoder.finish().unwrap()
}

/// Read the stream until `needle` arrives, returning everything read.
async fn read_stream_until(response: &mut reqwest::Response, needle: &str) -> String {
    let mut bytes: Vec<u8> = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let chunk = response
                .chunk()
                .await
                .unwrap()
                .expect("the stream ended early");
            bytes.extend_from_slice(&chunk);
            let text = String::from_utf8_lossy(&bytes);
            if let Some(at) = text.find(needle)
                && text[at..].contains("\n\n")
            {
                return;
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "{needle:?} never arrived: {}",
            String::from_utf8_lossy(&bytes)
        )
    });
    String::from_utf8(bytes).unwrap()
}

// -- AC5 ------------------------------------------------------------------------

/// Point the global subscriber's WAL sink at `server`.
fn bind_telemetry(server: &TestServer) -> &'static Telemetry {
    let telemetry = telemetry();
    telemetry.handle.set(
        Arc::clone(server.state.ingest.wal_writer.as_ref().unwrap()),
        &IngestConfig::default().default_env,
    );
    telemetry
        .layer
        .set_hot_buffer(Arc::clone(server.state.query.hot_buffer.as_ref().unwrap()));
    telemetry
}

/// A real ingest, accepted with repairs and an unmapped severity beside a
/// rejection, so every counter series a preview must leave alone exists
/// before one runs.
async fn seed_counters(server: &TestServer) {
    let response = client()
        .post(format!("{}/api/v1/ingest", server.url))
        .bearer_auth(&server.ingest_token)
        .body(
            [
                json!({"service": "seed", "level": "gold", "message": "seed"}),
                json!({"message": "seed without a service"}),
            ]
            .map(|event| event.to_string())
            .join("\n"),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
}

/// Send the canary sample as an accepted-repaired-rejected report and as
/// every refusal (body limit, position limit, gzip, a framing error, a bad
/// `peer_ip`), returning each response's request id.
async fn send_canary_previews(server: &TestServer) -> Vec<String> {
    use trawl_api::ingest_preview::{MAX_PREVIEW_EVENTS, PreviewEvent, PreviewResponse};

    let sample = canary_sample();
    let mut request_ids = Vec::new();

    let response = preview(server, "", &[], sample.clone()).await;
    assert_eq!(response.status(), 200);
    request_ids.push(request_id_of(&response));
    let report: PreviewResponse = response.json().await.unwrap();
    assert_eq!((report.accepted, report.rejected), (2, 6), "{report:?}");
    let repaired = report
        .events
        .iter()
        .find_map(|event| match event {
            PreviewEvent::Accepted { repairs, .. } if !repairs.is_empty() => Some(repairs.clone()),
            _ => None,
        })
        .expect("a repaired event");
    for code in [
        "time.from_ingest",
        "host.from_peer",
        "field.reserved_prefix",
    ] {
        assert!(repaired.iter().any(|r| r == code), "{code} in {repaired:?}");
    }

    // Every refusal carries the canaries too. The body limit's comes over
    // a raw connection: the sample in one chunk of exactly one byte past
    // the fixture's 128 KiB limit, never terminated, so trawld reads every
    // canary before it refuses and nothing is left unwritten to race the
    // 413 (see `common::raw_https_exchange`).
    let mut oversized = format!("{sample}\n{}", "x".repeat(128 * 1024));
    oversized.truncate(128 * 1024 + 1);
    let host = server.url.strip_prefix("https://").unwrap();
    let mut request = format!(
        "POST /api/v1/ingest/preview HTTP/1.1\r\nhost: {host}\r\n\
         authorization: Bearer {}\r\ncontent-type: application/x-ndjson\r\n\
         transfer-encoding: chunked\r\n\r\n{:x}\r\n",
        server.admin_token,
        oversized.len(),
    )
    .into_bytes();
    request.extend_from_slice(oversized.as_bytes());
    let response = common::raw_https_exchange(&server.url, &request).await;
    assert_eq!(response.status, 413, "body limit");
    request_ids.push(
        response
            .header("x-request-id")
            .expect("every response carries X-Request-Id")
            .to_owned(),
    );
    let text = String::from_utf8(response.body).unwrap();
    assert!(
        !has_canary(&text),
        "body limit: a refusal quotes no sample: {text}"
    );

    let framing = format!("[{}", json!({"service": "cnry-svc-framing"}));
    for (what, query, headers, body, status) in [
        (
            "position limit",
            "",
            &[][..],
            repeated(MAX_PREVIEW_EVENTS + 1).into_bytes(),
            413,
        ),
        (
            "gzip",
            "",
            &[("content-encoding", "gzip")],
            gzipped(&sample),
            415,
        ),
        ("framing", "", &[], framing.into_bytes(), 400),
        (
            "peer_ip",
            "?peer_ip=cnry-peer",
            &[],
            sample.clone().into_bytes(),
            400,
        ),
    ] {
        let response = preview(server, query, headers, body).await;
        assert_eq!(response.status(), status, "{what}");
        request_ids.push(request_id_of(&response));
        let text = response.text().await.unwrap();
        assert!(
            !has_canary(&text),
            "{what}: a refusal quotes no sample: {text}"
        );
    }
    request_ids
}

/// Require that the tail, read up to the barrier, carried no canary and no
/// event but the barrier and trawld's own telemetry (the baseline flush
/// published that).
fn assert_only_the_barrier_streamed(streamed: &str) {
    assert!(!has_canary(streamed), "the tail saw a sample: {streamed}");
    let others: Vec<Value> = streamed
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str::<Value>(data).unwrap())
        .filter(|event| event["service"] != "trawld")
        .collect();
    assert_eq!(
        others.len(),
        1,
        "only the barrier reached the tail: {others:#?}"
    );
    assert_eq!(others[0]["message"], "preview-barrier");
}

/// Previews with a canary in every input position, accepted, repaired and
/// rejected, plus refused ones (body limit, position limit, gzip, a
/// framing error, a bad `peer_ip`), change nothing a real ingest changes,
/// reach no stream subscriber, and leave no canary in trawld's persisted
/// telemetry or its log lines.
#[tokio::test(flavor = "multi_thread")]
async fn preview_with_canaries_everywhere_stores_and_logs_none_of_them() {
    let server = setup_with_rate_limit(UNLIMITED).await;
    let telemetry = bind_telemetry(&server);
    seed_counters(&server).await;

    // A live tail of everything, open before the previews. Opening it is
    // logged, so the startup and stream-open telemetry is flushed after
    // it: the baseline is taken with nothing pending, and no flush runs
    // again until after the snapshot that must equal it, since a flush
    // can bump trawld's own repairs counter.
    let mut stream = client()
        .get(format!("{}/api/v1/stream", server.url))
        .query(&[("query", "")])
        .bearer_auth(&server.analyst_token)
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), 200);
    telemetry.layer.flush_cycle().await;
    let before = snapshot(&server).await;
    for name in [
        "trawl_ingest_events_total",
        "trawl_ingest_events_rejected_total",
        "trawl_ingest_repairs_total",
        "trawl_severity_unmapped_total",
    ] {
        assert!(
            before.counters.iter().any(|line| line.starts_with(name)),
            "{name} is at the baseline, so its comparison is live: {:#?}",
            before.counters
        );
    }

    let request_ids = send_canary_previews(&server).await;
    assert_eq!(
        snapshot(&server).await,
        before,
        "a preview changed what ingest changes"
    );

    // Barrier: a real event reaches the tail, and nothing reached it first.
    ingest(
        &server,
        &[json!({"service": "barrier", "host": "h", "message": "preview-barrier"})],
    )
    .await;
    assert_only_the_barrier_streamed(&read_stream_until(&mut stream, "preview-barrier").await);
    drop(stream);

    // Persist everything the requests logged, then read it all back.
    telemetry.layer.flush_cycle().await;
    let wal = wal_lines(&server);
    let persisted: Vec<&String> = wal.iter().filter(|line| has_canary(line)).collect();
    assert!(persisted.is_empty(), "persisted canaries: {persisted:#?}");
    let stdout = captured_stdout();
    let logged: Vec<&str> = stdout.lines().filter(|line| has_canary(line)).collect();
    assert!(logged.is_empty(), "logged canaries: {logged:#?}");

    // The capture saw the previews: each one's request lines are on
    // stdout and in the persisted telemetry, debug lines included.
    for request_id in &request_ids {
        assert!(
            stdout.lines().any(|line| line.contains(request_id)),
            "{request_id} on stdout"
        );
        assert!(
            wal.iter()
                .any(|line| line.contains(request_id) && line.contains("http_response")),
            "{request_id}'s debug response line persisted"
        );
    }
}
