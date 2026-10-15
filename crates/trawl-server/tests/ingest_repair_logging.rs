// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What an HTTP ingest request logs about its repairs (ADR-0055): one
//! `ingest_complete` INFO for every finalized request, carrying the key and
//! every code, and one `ingest_repairs` WARN only when an actionable repair
//! landed in an acknowledged group.
//!
//! Its own test binary because `.init()` installs the global subscriber,
//! exactly as `trawld` does: the production subscriber from
//! `telemetry::build_subscriber` under [`telemetry::DEFAULT_LOG_FILTER`],
//! with a stdout capture and a telemetry WAL layer. Tests tell their lines
//! apart by request id and their metric series by service name.
//!
//! The events earn only codes whose meaning is settled: routine
//! `env.defaulted`, `host.from_peer`, `time.from_ingest` (no time field at
//! all) and `field.name_case_folded`; actionable `time.out_of_range`,
//! `field.name_too_long` and `field.name_case_collision`.

mod common;

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::TestServer;
use fleet_auth::{KeyStore, PrincipalKind};
use serde_json::{Value, json};
use tower::ServiceExt as _;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::util::SubscriberInitExt as _;
use trawl_server::ingest::producer::Derivation;
use trawl_server::ingest::wal::WalWriter;
use trawl_server::telemetry::{self, LogSinks, WalHandle, WalLayer};

const ENV: &str = "default";

// -- the production subscriber, captured ------------------------------------

/// Everything the global subscriber writes to stdout.
#[derive(Clone, Default)]
struct Stdout(Arc<Mutex<Vec<u8>>>);

impl Write for Stdout {
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

struct Sinks {
    stdout: Stdout,
    wal: WalLayer,
    wal_dir: tempfile::TempDir,
}

/// The production subscriber under the default directives, installed once
/// for the whole binary.
fn sinks() -> &'static Sinks {
    static SINKS: OnceLock<Sinks> = OnceLock::new();
    SINKS.get_or_init(|| {
        let wal_dir = tempfile::tempdir().unwrap();
        let handle = WalHandle::new();
        handle.set(Arc::new(WalWriter::new(wal_dir.path().to_path_buf())), ENV);
        let wal = WalLayer::new_with_buffer_cap(
            handle,
            &[ENV.to_owned()],
            ENV,
            Arc::new(Derivation::defaults()),
            trawl_config::DEFAULT_TELEMETRY_BUFFER_MAX_BYTES,
        );
        let stdout = Stdout::default();
        let (subscriber, _) = telemetry::build_subscriber(
            telemetry::DEFAULT_LOG_FILTER,
            LogSinks {
                stdout_ansi: false,
                stdout: Some(stdout.clone()),
                wal: Some(wal.clone()),
                file_log: false,
            },
        );
        subscriber.init();
        Sinks {
            stdout,
            wal,
            wal_dir,
        }
    })
}

/// Every stdout line so far.
fn stdout_lines() -> Vec<String> {
    let bytes = sinks().stdout.0.lock().unwrap().clone();
    String::from_utf8(bytes)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// A field's value on a formatted stdout line: `name="quoted"` or
/// `name=bare`. The first span field follows `{`, every other field a
/// space; the request span's fields come first.
fn field(line: &str, name: &str) -> Option<String> {
    let key = format!("{name}=");
    let at = line
        .match_indices(&key)
        .find(|(at, _)| matches!(line[..*at].chars().last(), Some(' ' | '{')))?
        .0
        + key.len();
    let rest = &line[at..];
    if let Some(quoted) = rest.strip_prefix('"') {
        quoted.find('"').map(|end| quoted[..end].to_owned())
    } else {
        Some(
            rest.split(|c: char| c.is_whitespace() || c == '}')
                .next()
                .unwrap_or("")
                .to_owned(),
        )
    }
}

/// Every record the telemetry WAL holds so far.
fn wal_records() -> Vec<Value> {
    // Tests flush concurrently. Without this lock one test could find the
    // queue already drained by another whose WAL write has not landed yet.
    static FLUSH: Mutex<()> = Mutex::new(());
    let _flush = FLUSH.lock().unwrap();
    sinks().wal.flush();
    read_wal(&sinks().wal_dir.path().join(ENV))
}

fn read_wal(dir: &Path) -> Vec<Value> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "ndjson"))
        .flat_map(|path| {
            std::fs::read_to_string(path)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The level a formatted stdout line was logged at.
fn level(line: &str) -> &'static str {
    ["ERROR", "WARN", "INFO", "DEBUG", "TRACE"]
        .into_iter()
        .find(|level| line.split_whitespace().any(|word| word == *level))
        .unwrap_or("none")
}

// -- one request's repair lines, on both sinks --------------------------------

const COMPLETE: &str = "ingest_complete";
const REPAIRS: &str = "ingest_repairs";

/// One request's lines of `event_type`: stdout, then persisted.
fn lines_of(request_id: &str, event_type: &str) -> (Vec<String>, Vec<Value>) {
    let stdout = stdout_lines()
        .into_iter()
        .filter(|line| field(line, "event_type").as_deref() == Some(event_type))
        .filter(|line| field(line, "request_id").as_deref() == Some(request_id))
        .collect();
    let persisted = wal_records()
        .into_iter()
        .filter(|record| record["event_type"] == event_type)
        .filter(|record| record["request_id"] == request_id)
        .collect();
    (stdout, persisted)
}

/// The request's one line of `event_type` on each sink, at `level`.
fn one_line(request_id: &str, event_type: &str, level_name: &str) -> (String, Value) {
    let (mut stdout, mut persisted) = lines_of(request_id, event_type);
    assert_eq!(stdout.len(), 1, "{event_type} on stdout: {stdout:#?}");
    assert_eq!(persisted.len(), 1, "{event_type} persisted: {persisted:#?}");
    let (line, record) = (stdout.remove(0), persisted.remove(0));
    assert_eq!(level(&line), level_name, "{line}");
    assert_eq!(record["level"], level_name.to_lowercase(), "{record}");
    (line, record)
}

/// The request wrote no `ingest_repairs` line on either sink.
fn assert_no_repair_warning(request_id: &str) {
    let (stdout, persisted) = lines_of(request_id, REPAIRS);
    assert!(stdout.is_empty(), "{stdout:#?}");
    assert!(persisted.is_empty(), "{persisted:#?}");
}

/// The repair fields logged as integers. Persisted telemetry pins a field's
/// type, so each must reach stdout unquoted and the record as a number.
const NUMERIC_FIELDS: &[&str] = &[
    "key_id",
    "repairs",
    "repairs_routine",
    "repairs_actionable",
    "repair_detail_omitted_pairs",
    "repair_detail_omitted_occurrences",
    "actionable_detail_omitted_pairs",
    "actionable_detail_omitted_occurrences",
];

/// Each `(field, value)` holds on the stdout line and on the persisted
/// record alike, and each of [`NUMERIC_FIELDS`] keeps its integer type.
fn assert_fields(line: &str, record: &Value, expected: &[(&str, &str)]) {
    for (name, want) in expected {
        assert_eq!(field(line, name).as_deref(), Some(*want), "{name}: {line}");
        if NUMERIC_FIELDS.contains(name) {
            assert!(
                !line.contains(&format!("{name}=\"")),
                "{name} is quoted on stdout: {line}"
            );
            let integer = if *name == "key_id" {
                record[*name].is_i64()
            } else {
                record[*name].is_u64()
            };
            assert!(integer, "{name} is not persisted as an integer: {record}");
        }
        let stored = match &record[*name] {
            Value::String(text) => text.clone(),
            Value::Null => panic!("{name} is not persisted: {record}"),
            other => other.to_string(),
        };
        assert_eq!(stored, *want, "{name}: {record}");
    }
}

/// The WARN never claims data was lost: `time.out_of_range` keeps the value
/// it flags.
fn assert_says_inspect_not_lost(line: &str, record: &Value) {
    let message = record["message"].as_str().unwrap();
    assert!(message.contains("need inspection"), "{message}");
    for text in [line.to_lowercase(), message.to_lowercase()] {
        for word in ["lost", "dropped", "discarded"] {
            assert!(!text.contains(word), "{word}: {text}");
        }
    }
}

// -- metrics ------------------------------------------------------------------

/// A rendered counter series' value, or `None` when it does not exist.
fn series(rendered: &str, series: &str) -> Option<u64> {
    rendered.lines().find_map(|line| {
        let (name, value) = line.rsplit_once(' ')?;
        (name == series).then(|| value.parse().unwrap())
    })
}

fn repairs_series(code: &str, service: &str) -> Option<u64> {
    series(
        &common::test_metrics_handle().render(),
        &format!("trawl_ingest_repairs_total{{code=\"{code}\",service=\"{service}\"}}"),
    )
}

fn severity_unmapped_series(service: &str) -> Option<u64> {
    series(
        &common::test_metrics_handle().render(),
        &format!("trawl_severity_unmapped_total{{service=\"{service}\"}}"),
    )
}

/// No series of either counter names `service`.
fn assert_no_series_for(service: &str) {
    let needle = format!("service=\"{service}\"");
    let rendered = common::test_metrics_handle().render();
    let hits: Vec<&str> = rendered
        .lines()
        .filter(|line| line.contains(&needle))
        .collect();
    assert!(hits.is_empty(), "{hits:#?}");
}

// -- requests -----------------------------------------------------------------

/// An ingest key named `name`: its token and its stable id.
async fn mint(server: &TestServer, name: &str) -> (String, i64) {
    let created = KeyStore::from_pool(server.fleet_pool.clone())
        .create_key(
            name,
            PrincipalKind::Service,
            &common::roles(&["trawl-ingest"]),
            None,
        )
        .await
        .unwrap();
    (created.plaintext_token.to_string(), created.info.id)
}

fn ndjson(events: &[Value]) -> String {
    events
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

/// POST `events` as `token` and answer the status and the request id.
async fn post(server: &TestServer, token: &str, events: &[Value]) -> (u16, String) {
    let response = common::harness_client_builder()
        .build()
        .unwrap()
        .post(format!("{}/api/v1/ingest", server.url))
        .bearer_auth(token)
        .header("content-type", "application/x-ndjson")
        .body(ndjson(events))
        .send()
        .await
        .unwrap();
    let request_id = response
        .headers()
        .get("x-request-id")
        .expect("every response carries X-Request-Id")
        .to_str()
        .unwrap()
        .to_owned();
    (response.status().as_u16(), request_id)
}

/// A timestamp inside the plausible window.
fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// More than ten years past: kept, but earns `time.out_of_range`.
const ANCIENT: &str = "2001-02-03T04:05:06Z";

/// A field name past `MAX_FIELD_NAME_BYTES`: earns `field.name_too_long`.
fn long_name() -> String {
    "n".repeat(300)
}

/// A counter stored as a string fails [`assert_fields`], so the integer
/// fields cannot regress to text while the value assertions stay green.
#[test]
#[should_panic(expected = "not persisted as an integer")]
fn assert_fields_rejects_a_counter_stored_as_text() {
    assert_fields(
        "x{repairs_actionable=3}",
        &json!({ "repairs_actionable": "3" }),
        &[("repairs_actionable", "3")],
    );
}

/// No `env`, `host` or time field: `env.defaulted`, `host.from_peer` and
/// `time.from_ingest`, all routine.
fn routine(service: &str) -> Value {
    json!({ "service": service, "message": "routine" })
}

/// Fully addressed and timed: earns nothing on its own.
fn addressed(service: &str) -> Value {
    json!({ "service": service, "env": "prod", "host": "h", "_time": now() })
}

// -- the acceptance criteria ----------------------------------------------------

/// Routine repairs only: no WARN on either sink, and one INFO that carries
/// the key, the request, every count and the `env.defaulted` count.
#[tokio::test(flavor = "multi_thread")]
async fn routine_only_request_writes_one_info_and_no_warning() {
    sinks();
    let server = common::setup().await;
    let (token, key_id) = mint(&server, "routine-sender").await;
    let service = "rl-routine";
    let mut folded = routine(service);
    folded["Mixed"] = json!(1);
    let (status, request_id) = post(
        &server,
        &token,
        &[routine(service), routine(service), folded],
    )
    .await;
    assert_eq!(status, 200);

    assert_no_repair_warning(&request_id);
    let (line, record) = one_line(&request_id, COMPLETE, "INFO");
    assert_fields(
        &line,
        &record,
        &[
            ("user", "routine-sender"),
            ("key_id", &key_id.to_string()),
            ("request_id", &request_id),
            ("accepted", "3"),
            ("repairs", "10"),
            ("repairs_routine", "10"),
            ("repairs_actionable", "0"),
            (
                "repair_codes",
                "host.from_peer:3, env.defaulted:3, time.from_ingest:3, \
                 field.name_case_folded:1",
            ),
            (
                "repair_detail",
                "rl-routine/env.defaulted:3, rl-routine/host.from_peer:3, \
                 rl-routine/time.from_ingest:3, rl-routine/field.name_case_folded:1",
            ),
            ("repair_detail_omitted_pairs", "0"),
            ("repair_detail_omitted_occurrences", "0"),
        ],
    );
    assert_eq!(repairs_series("env.defaulted", service), Some(3));
}

/// Actionable repairs only: one WARN with the key, the request and the
/// actionable per-code totals, worded as a call to inspect, plus one INFO.
#[tokio::test(flavor = "multi_thread")]
async fn actionable_only_request_warns_once() {
    sinks();
    let server = common::setup().await;
    let (token, key_id) = mint(&server, "actionable-sender").await;
    let service = "rl-actionable";
    let mut ancient = addressed(service);
    ancient["_time"] = json!(ANCIENT);
    let mut long = addressed(service);
    long[long_name()] = json!("kept in _raw");
    let mut collision = addressed(service);
    collision["abc"] = json!(1);
    collision["ABC"] = json!(2);
    let (status, request_id) = post(&server, &token, &[ancient, long, collision]).await;
    assert_eq!(status, 200);

    let (line, record) = one_line(&request_id, REPAIRS, "WARN");
    assert_fields(
        &line,
        &record,
        &[
            ("user", "actionable-sender"),
            ("key_id", &key_id.to_string()),
            ("request_id", &request_id),
            ("repairs_actionable", "3"),
            ("repairs_routine", "0"),
            (
                "actionable_codes",
                "time.out_of_range:1, field.name_too_long:1, field.name_case_collision:1",
            ),
            (
                "actionable_detail",
                "rl-actionable/field.name_case_collision:1, \
                 rl-actionable/field.name_too_long:1, rl-actionable/time.out_of_range:1",
            ),
            ("actionable_detail_omitted_pairs", "0"),
            ("actionable_detail_omitted_occurrences", "0"),
        ],
    );
    assert_says_inspect_not_lost(&line, &record);

    let (line, record) = one_line(&request_id, COMPLETE, "INFO");
    assert_fields(
        &line,
        &record,
        &[
            ("key_id", &key_id.to_string()),
            ("repairs", "3"),
            ("repairs_routine", "0"),
            ("repairs_actionable", "3"),
        ],
    );
    // The WARN comes first.
    let lines = stdout_lines();
    let position = |event_type: &str| {
        lines
            .iter()
            .position(|line| {
                field(line, "event_type").as_deref() == Some(event_type)
                    && field(line, "request_id").as_deref() == Some(request_id.as_str())
            })
            .unwrap()
    };
    assert!(position(REPAIRS) < position(COMPLETE));
}

/// Both classes: the WARN's totals and detail hold only actionable codes and
/// carry the routine count; the INFO lists every code.
#[tokio::test(flavor = "multi_thread")]
async fn mixed_request_warns_on_actionable_codes_and_completes_with_all() {
    sinks();
    let server = common::setup().await;
    let (token, key_id) = mint(&server, "mixed-sender").await;
    let service = "rl-mixed";
    let mut actionable = addressed(service);
    actionable["_time"] = json!(ANCIENT);
    actionable[long_name()] = json!(1);
    let (status, request_id) = post(
        &server,
        &token,
        &[routine(service), routine(service), actionable],
    )
    .await;
    assert_eq!(status, 200);

    let (line, record) = one_line(&request_id, REPAIRS, "WARN");
    assert_fields(
        &line,
        &record,
        &[
            ("key_id", &key_id.to_string()),
            ("repairs_actionable", "2"),
            ("repairs_routine", "6"),
            (
                "actionable_codes",
                "time.out_of_range:1, field.name_too_long:1",
            ),
            (
                "actionable_detail",
                "rl-mixed/field.name_too_long:1, rl-mixed/time.out_of_range:1",
            ),
            ("actionable_detail_omitted_pairs", "0"),
            ("actionable_detail_omitted_occurrences", "0"),
        ],
    );
    for routine_code in ["env.defaulted", "host.from_peer", "time.from_ingest"] {
        assert!(!line.contains(routine_code), "{routine_code}: {line}");
        assert!(
            !record["actionable_codes"]
                .as_str()
                .unwrap()
                .contains(routine_code)
        );
        assert!(
            !record["actionable_detail"]
                .as_str()
                .unwrap()
                .contains(routine_code)
        );
    }

    let (line, record) = one_line(&request_id, COMPLETE, "INFO");
    assert_fields(
        &line,
        &record,
        &[
            ("repairs", "8"),
            ("repairs_routine", "6"),
            ("repairs_actionable", "2"),
            (
                "repair_codes",
                "host.from_peer:2, env.defaulted:2, time.from_ingest:2, \
                 time.out_of_range:1, field.name_too_long:1",
            ),
            (
                "repair_detail",
                "rl-mixed/env.defaulted:2, rl-mixed/host.from_peer:2, \
                 rl-mixed/time.from_ingest:2, rl-mixed/field.name_too_long:1, \
                 rl-mixed/time.out_of_range:1",
            ),
        ],
    );
}

/// A clean request: one INFO with zero counts and empty detail, no WARN,
/// and no repair series.
#[tokio::test(flavor = "multi_thread")]
async fn clean_request_completes_with_zero_repairs() {
    sinks();
    let server = common::setup().await;
    let (token, key_id) = mint(&server, "clean-sender").await;
    let service = "rl-clean";
    let (status, request_id) =
        post(&server, &token, &[addressed(service), addressed(service)]).await;
    assert_eq!(status, 200);

    assert_no_repair_warning(&request_id);
    let (line, record) = one_line(&request_id, COMPLETE, "INFO");
    assert_fields(
        &line,
        &record,
        &[
            ("key_id", &key_id.to_string()),
            ("accepted", "2"),
            ("repairs", "0"),
            ("repairs_routine", "0"),
            ("repairs_actionable", "0"),
            ("repair_codes", ""),
            ("repair_detail", ""),
            ("repair_detail_omitted_pairs", "0"),
            ("repair_detail_omitted_occurrences", "0"),
        ],
    );
    assert_no_series_for(service);
}

/// Twenty services: eight detail pairs in order, exact omitted counts, and a
/// one-off actionable code that no listed pair outranks still in the
/// per-code totals of both lines.
#[tokio::test(flavor = "multi_thread")]
async fn detail_is_bounded_and_totals_stay_exact() {
    sinks();
    let server = common::setup().await;
    let (token, _) = mint(&server, "bounded-sender").await;
    // Each event lacks only `env`, except s01's, which also lack `host`:
    // s01 then ties s02 at 11 on `env.defaulted` (the service breaks it)
    // and itself at 11 on `host.from_peer` (the code breaks it).
    let counts: [u64; 20] = [
        12, 11, 11, 10, 9, 9, 8, 7, 7, 6, 5, 4, 3, 3, 2, 2, 1, 1, 1, 1,
    ];
    let mut events = Vec::new();
    for (index, count) in counts.iter().enumerate() {
        let service = format!("rl-bound-s{index:02}");
        for _ in 0..*count {
            let mut event = json!({ "service": service, "host": "h", "_time": now() });
            if index == 1 {
                event.as_object_mut().unwrap().remove("host");
            }
            events.push(event);
        }
    }
    // s19's one event also carries a name too long: one actionable repair.
    events.last_mut().unwrap()[long_name()] = json!(1);
    let (status, request_id) = post(&server, &token, &events).await;
    assert_eq!(status, 200);

    // 113 env.defaulted + 11 host.from_peer + 1 field.name_too_long, over
    // 22 pairs. The eight listed hold 81 occurrences.
    let (line, record) = one_line(&request_id, COMPLETE, "INFO");
    assert_fields(
        &line,
        &record,
        &[
            ("accepted", "113"),
            ("repairs", "125"),
            ("repairs_routine", "124"),
            ("repairs_actionable", "1"),
            (
                "repair_codes",
                "host.from_peer:11, env.defaulted:113, field.name_too_long:1",
            ),
            (
                "repair_detail",
                "rl-bound-s00/env.defaulted:12, rl-bound-s01/env.defaulted:11, \
                 rl-bound-s02/env.defaulted:11, rl-bound-s01/host.from_peer:11, \
                 rl-bound-s03/env.defaulted:10, rl-bound-s04/env.defaulted:9, \
                 rl-bound-s05/env.defaulted:9, rl-bound-s06/env.defaulted:8",
            ),
            ("repair_detail_omitted_pairs", "14"),
            ("repair_detail_omitted_occurrences", "44"),
        ],
    );

    let (line, record) = one_line(&request_id, REPAIRS, "WARN");
    assert_fields(
        &line,
        &record,
        &[
            ("repairs_actionable", "1"),
            ("repairs_routine", "124"),
            ("actionable_codes", "field.name_too_long:1"),
            ("actionable_detail", "rl-bound-s19/field.name_too_long:1"),
            ("actionable_detail_omitted_pairs", "0"),
            ("actionable_detail_omitted_occurrences", "0"),
        ],
    );
}

/// More than eight actionable pairs: the WARN's own detail is bounded, its
/// omitted counts are exact and nonzero, and a one-off actionable code past
/// the bound stays in its per-code totals.
#[tokio::test(flavor = "multi_thread")]
async fn actionable_detail_is_bounded_and_totals_stay_exact() {
    sinks();
    let server = common::setup().await;
    let (token, _) = mint(&server, "actionable-bounded-sender").await;
    // Service s{i} sends 10 - i ancient events, each earning only
    // `time.out_of_range`; s09's one event also carries a name too long.
    let mut events = Vec::new();
    for index in 0..10_u64 {
        let service = format!("rl-abound-s{index:02}");
        for _ in 0..10 - index {
            let mut event = addressed(&service);
            event["_time"] = json!(ANCIENT);
            events.push(event);
        }
    }
    events.last_mut().unwrap()[long_name()] = json!(1);
    let (status, request_id) = post(&server, &token, &events).await;
    assert_eq!(status, 200);

    // 55 time.out_of_range + 1 field.name_too_long, over 11 pairs. The eight
    // listed hold 52 occurrences; s08's 2 and s09's two 1s are omitted.
    let detail = "rl-abound-s00/time.out_of_range:10, rl-abound-s01/time.out_of_range:9, \
                  rl-abound-s02/time.out_of_range:8, rl-abound-s03/time.out_of_range:7, \
                  rl-abound-s04/time.out_of_range:6, rl-abound-s05/time.out_of_range:5, \
                  rl-abound-s06/time.out_of_range:4, rl-abound-s07/time.out_of_range:3";
    let (line, record) = one_line(&request_id, REPAIRS, "WARN");
    assert_fields(
        &line,
        &record,
        &[
            ("repairs_actionable", "56"),
            ("repairs_routine", "0"),
            (
                "actionable_codes",
                "time.out_of_range:55, field.name_too_long:1",
            ),
            ("actionable_detail", detail),
            ("actionable_detail_omitted_pairs", "3"),
            ("actionable_detail_omitted_occurrences", "4"),
        ],
    );

    let (line, record) = one_line(&request_id, COMPLETE, "INFO");
    assert_fields(
        &line,
        &record,
        &[
            ("accepted", "55"),
            ("repairs", "56"),
            (
                "repair_codes",
                "time.out_of_range:55, field.name_too_long:1",
            ),
            ("repair_detail", detail),
            ("repair_detail_omitted_pairs", "3"),
            ("repair_detail_omitted_occurrences", "4"),
        ],
    );
}

/// Two keys with one display name and a third flooding actionable repairs,
/// sending at once: every line names the key that sent it, and each
/// actionable request has its own WARN.
#[tokio::test(flavor = "multi_thread")]
async fn keys_are_attributed_per_request() {
    sinks();
    let server = Arc::new(common::setup().await);
    let (first_twin, first_id) = mint(&server, "twin-sender").await;
    let (second_twin, second_id) = mint(&server, "twin-sender").await;
    let (flooder, flooder_id) = mint(&server, "flooder").await;
    assert_ne!(first_id, second_id, "names are not identities");

    let actionable = |service: &str| {
        let mut event = addressed(service);
        event["_time"] = json!(ANCIENT);
        event
    };
    // (token, key id, name, events, actionable?)
    let mut plan: Vec<(String, i64, &str, Vec<Value>, bool)> = Vec::new();
    for round in 0..4 {
        plan.push((
            first_twin.clone(),
            first_id,
            "twin-sender",
            if round % 2 == 0 {
                vec![routine("rl-keys-a")]
            } else {
                vec![actionable("rl-keys-a")]
            },
            round % 2 == 1,
        ));
        plan.push((
            flooder.clone(),
            flooder_id,
            "flooder",
            (0..20).map(|_| actionable("rl-keys-flood")).collect(),
            true,
        ));
        plan.push((
            second_twin.clone(),
            second_id,
            "twin-sender",
            if round % 2 == 0 {
                vec![actionable("rl-keys-b")]
            } else {
                vec![routine("rl-keys-b")]
            },
            round % 2 == 0,
        ));
    }

    let mut sends = tokio::task::JoinSet::new();
    for (token, key_id, name, events, actionable) in plan {
        let server = Arc::clone(&server);
        sends.spawn(async move {
            let (status, request_id) = post(&server, &token, &events).await;
            assert_eq!(status, 200);
            (request_id, key_id, name, actionable, events.len())
        });
    }
    let sent = sends.join_all().await;
    assert_eq!(sent.len(), 12);

    for (request_id, key_id, name, actionable, events) in sent {
        let key_id = key_id.to_string();
        let (line, record) = one_line(&request_id, COMPLETE, "INFO");
        assert_fields(&line, &record, &[("key_id", &key_id), ("user", name)]);
        if actionable {
            let (line, record) = one_line(&request_id, REPAIRS, "WARN");
            assert_fields(
                &line,
                &record,
                &[
                    ("key_id", &key_id),
                    ("user", name),
                    ("repairs_actionable", &events.to_string()),
                ],
            );
        } else {
            assert_no_repair_warning(&request_id);
        }
    }
}

/// The same service in two envs, one group's WAL write failing: the failed
/// group's repairs and unmapped severities reach neither the metrics nor
/// the log lines, the written group's do, and the answer is still the WAL
/// failure. When every group fails, nothing is counted.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // two requests, stated in order
async fn wal_failure_counts_only_acknowledged_groups() {
    sinks();
    let server = common::setup().await;
    let (token, key_id) = mint(&server, "wal-sender").await;

    // Allow a second env and make its WAL directory unwritable: a regular
    // file sits where `lab/` must be created.
    let mut state = server.state.clone();
    state.ingest.envs = Arc::from(vec!["prod".to_owned(), "lab".to_owned()]);
    let wal_dir = state
        .ingest
        .wal_writer
        .as_ref()
        .expect("ingest is enabled")
        .dir()
        .to_path_buf();
    std::fs::create_dir_all(&wal_dir).unwrap();
    std::fs::write(wal_dir.join("lab"), b"not a directory").unwrap();
    let app = server.router_over(state, |_| {});

    let send = |events: Vec<Value>| {
        let mut request = Request::post("/api/v1/ingest")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/x-ndjson")
            .body(Body::from(ndjson(&events)))
            .unwrap();
        request
            .extensions_mut()
            .insert(std::net::SocketAddr::from(([127, 0, 0, 1], 40_302)));
        app.clone().oneshot(request)
    };
    let request_id = |response: &axum::response::Response| {
        response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .to_owned()
    };
    // The redacted 500, from the lab group's failed write.
    let wal_failure = async |response: axum::response::Response| {
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let request_id = request_id(&response);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["code"], "internal_error", "{body}");
        let (failed, _) = lines_of(&request_id, "wal_write_failed");
        assert_eq!(failed.len(), 1, "{failed:#?}");
        assert_eq!(field(&failed[0], "batch_env").as_deref(), Some("lab"));
        request_id
    };

    // prod: 2 events, each `time.out_of_range` with an unmapped severity.
    // lab: 3 events, each `field.name_too_long` with an unmapped severity.
    let service = "rl-wal-dual";
    let mut events = Vec::new();
    for _ in 0..2 {
        let mut event = addressed(service);
        event["_time"] = json!(ANCIENT);
        event["level"] = json!("gold");
        events.push(event);
    }
    for _ in 0..3 {
        let mut event = addressed(service);
        event["env"] = json!("lab");
        event["level"] = json!("gold");
        event[long_name()] = json!(1);
        events.push(event);
    }
    let partial = wal_failure(send(events).await.unwrap()).await;

    assert_eq!(repairs_series("time.out_of_range", service), Some(2));
    assert_eq!(repairs_series("field.name_too_long", service), None);
    assert_eq!(severity_unmapped_series(service), Some(2));
    let (line, record) = one_line(&partial, REPAIRS, "WARN");
    assert_fields(
        &line,
        &record,
        &[
            ("key_id", &key_id.to_string()),
            ("repairs_actionable", "2"),
            ("actionable_codes", "time.out_of_range:2"),
            ("actionable_detail", "rl-wal-dual/time.out_of_range:2"),
        ],
    );
    let (line, record) = one_line(&partial, COMPLETE, "INFO");
    assert_fields(
        &line,
        &record,
        &[
            ("accepted", "2"),
            ("services", "1"),
            ("repairs", "2"),
            ("repairs_actionable", "2"),
            ("repair_codes", "time.out_of_range:2"),
            ("repair_detail", "rl-wal-dual/time.out_of_range:2"),
        ],
    );

    // Every group fails.
    let lab_only = "rl-wal-lab-only";
    let events = (0..3)
        .map(|_| {
            let mut event = addressed(lab_only);
            event["env"] = json!("lab");
            event["_time"] = json!(ANCIENT);
            event["level"] = json!("gold");
            event[long_name()] = json!(1);
            event
        })
        .collect();
    let total = wal_failure(send(events).await.unwrap()).await;

    assert_no_series_for(lab_only);
    assert_no_repair_warning(&total);
    let (line, record) = one_line(&total, COMPLETE, "INFO");
    assert_fields(
        &line,
        &record,
        &[
            ("key_id", &key_id.to_string()),
            ("accepted", "0"),
            ("services", "0"),
            ("repairs", "0"),
            ("repair_codes", ""),
            ("repair_detail", ""),
        ],
    );
}
