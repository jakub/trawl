// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The input-column descriptor changes nothing for a query whose fields
//! are all present (ADR-0062).
//!
//! A field the input does not carry reads as NULL, and the emitter learns
//! which fields those are from a schema probe of the selected sources. A
//! query that reads only fields its input carries must emit exactly the
//! statement it emitted before the probe existed, on every lane. That is
//! read off the statement each lane actually prepared
//! ([`prepare_probe::last_statement`]), recorded as snapshots on the tree
//! before the descriptor landed.
//!
//! The fixture carries every field the queries read, in cold parquet and in
//! the hot snapshot alike, with a non-null value on every event, so no
//! query in that test reads an absent field or a null-only hot key.
//!
//! The probe itself is bounded too: one `DESCRIBE` of the resolved cold
//! reader per request, inside the lane's cancellation latch and booked as
//! `probe` ([`schema_probe`]).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use trawl_core::schema::{CanonicalType, FieldTypes};
use trawl_engine::cancel::CancelLatch;
use trawl_engine::error::EngineError;
use trawl_engine::executor::{Executor, RowCap, hot_columns_of, prepare_probe, schema_probe};
use trawl_engine::timing::{PhaseClock, PhaseTotals, QueryPhase};

/// Present-field queries, one per stage the resolver rewrites a read in.
/// Every one reads only fields the fixture carries.
const QUERIES: &[&str] = &[
    "login",
    "login host=web1 status>=500",
    "service=api OR host!=web2",
    r#"earliest="2024-01-15T10:00:00Z" host=web*"#,
    "* | where status >= 500 and duration > 1.5",
    r#"* | where lower(host) == "web1" or isnull(service)"#,
    "* | let slow = duration > 1.0, code = status * 2 | table host, slow, code",
    "host=web1 | table _time, host, status, message",
    "* | fields host, status",
    "* | rename host as server | table server, status",
    "* | stats count() as n, avg(duration) as d by host",
    "* | timechart span=1h count() by host",
    "* | pivot count() on service by host",
    "* | dedup host | table host",
    "* | sort -status, host | table host, status",
    "* | top 2 host",
    "* | eventstats max(duration) as peak by host | where duration < peak",
    r#"* | extract "user=(?P<who>[a-z]+)" from message | table who"#,
    "* | extract kv | stats count() by user",
];

/// The queries the Parquet export runs: every query without a Rust tail,
/// since an export refuses `extract kv` before it binds anything.
fn export_queries() -> impl Iterator<Item = &'static str> {
    QUERIES
        .iter()
        .copied()
        .filter(|dsl| !dsl.contains("extract kv"))
}

/// One event, written to cold parquet or to the hot snapshot.
struct Event {
    hour: u32,
    second: u32,
    host: &'static str,
    service: &'static str,
    status: i64,
    duration: f64,
    message: &'static str,
}

const COLD: &[Event] = &[
    Event {
        hour: 10,
        second: 1,
        host: "web1",
        service: "api",
        status: 200,
        duration: 0.5,
        message: "login ok user=alice",
    },
    Event {
        hour: 10,
        second: 2,
        host: "web2",
        service: "auth",
        status: 503,
        duration: 2.5,
        message: "login failed user=bob",
    },
    Event {
        hour: 11,
        second: 3,
        host: "web1",
        service: "api",
        status: 500,
        duration: 1.75,
        message: "timeout user=carol",
    },
];

const HOT: &[Event] = &[
    Event {
        hour: 12,
        second: 4,
        host: "web3",
        service: "auth",
        status: 404,
        duration: 0.25,
        message: "login missing user=dave",
    },
    Event {
        hour: 12,
        second: 5,
        host: "web1",
        service: "api",
        status: 502,
        duration: 3.0,
        message: "disk full user=erin",
    },
];

/// The fixture on disk.
struct Fixture {
    dir: tempfile::TempDir,
    /// The cold source in the server's list shape, one hour per element.
    cold: String,
    /// A list source no element of which reaches a file: the cold start.
    cold_start: String,
    hot: PathBuf,
}

impl Fixture {
    fn build() -> Self {
        let dir = tempfile::tempdir().expect("fixture tempdir");
        let conn = duckdb::Connection::open_in_memory().expect("fixture database");
        for hour in [10, 11] {
            let hour_dir = dir.path().join("cold").join(hour.to_string());
            std::fs::create_dir_all(&hour_dir).expect("hour directory");
            let events: Vec<&Event> = COLD.iter().filter(|e| e.hour == hour).collect();
            write_cold_file(&conn, &hour_dir.join("part.parquet"), &events);
        }
        std::fs::create_dir_all(dir.path().join("empty").join("10")).expect("empty hour");

        let hot = dir.path().join("hot.ndjson");
        let mut text = String::new();
        for event in HOT {
            let instant = format!("2024-01-15T{:02}:00:{:02}Z", event.hour, event.second);
            let line = serde_json::json!({
                "_time": instant,
                "_ingested": instant,
                "_raw": event.message,
                "message": event.message,
                "host": event.host,
                "service": event.service,
                "status": event.status,
                "duration": event.duration,
            });
            writeln!(text, "{line}").expect("format hot line");
        }
        std::fs::write(&hot, text).expect("hot snapshot");

        let root = dir.path().display();
        let cold = format!("['{root}/cold/10/*.parquet', '{root}/cold/11/*.parquet']");
        let cold_start = format!("['{root}/empty/10/*.parquet']");
        Self {
            dir,
            cold,
            cold_start,
            hot,
        }
    }

    fn hot_source(&self) -> &str {
        self.hot.to_str().expect("utf-8 hot path")
    }

    /// A statement with this fixture's temp directory spelled `<dir>`, so
    /// the snapshot does not depend on where the run put its files.
    fn redact(&self, sql: &str) -> String {
        sql.replace(&self.dir.path().display().to_string(), "<dir>")
    }
}

fn write_cold_file(conn: &duckdb::Connection, path: &Path, events: &[&Event]) {
    let rows: Vec<String> = events
        .iter()
        .map(|e| {
            let ts = format!("TIMESTAMP '2024-01-15 {:02}:00:{:02}'", e.hour, e.second);
            format!(
                "({ts}, {ts}, '{msg}', '{msg}', '{host}', '{service}', CAST({status} AS BIGINT), \
                 CAST({duration} AS DOUBLE))",
                msg = e.message,
                host = e.host,
                service = e.service,
                status = e.status,
                duration = e.duration,
            )
        })
        .collect();
    conn.execute_batch(&format!(
        "COPY (SELECT * FROM (VALUES {}) AS t(_time, _ingested, _raw, message, host, service, \
         status, duration)) TO '{}' (FORMAT PARQUET)",
        rows.join(", "),
        path.display()
    ))
    .expect("cold parquet file");
}

/// The catalog the server would hold for this corpus.
fn catalog() -> FieldTypes {
    let mut pins = FieldTypes::new();
    pins.insert("host", CanonicalType::Varchar);
    pins.insert("status", CanonicalType::BigInt);
    pins.insert("duration", CanonicalType::Double);
    pins
}

/// Every hot event carries every catalog field, so the hot pins are the
/// whole catalog, as the server cuts them.
fn hot_pins() -> FieldTypes {
    catalog()
}

/// The statement this thread last handed `DuckDB`, after `run` succeeded.
fn prepared<E: std::fmt::Debug>(
    lane: &str,
    dsl: &str,
    run: impl FnOnce() -> Result<(), E>,
) -> String {
    if let Err(e) = run() {
        panic!("{lane}: `{dsl}` must answer over present fields: {e:?}");
    }
    prepare_probe::last_statement().expect("the lane prepared a statement")
}

/// Every query's prepared statement on one lane, one block per query.
fn lane_statements<'q>(
    fixture: &Fixture,
    lane: &str,
    queries: impl Iterator<Item = &'q str>,
    mut run: impl FnMut(&str) -> String,
) -> String {
    let mut out = String::new();
    for dsl in queries {
        let sql = fixture.redact(&run(dsl));
        writeln!(out, "-- {lane}: {dsl}\n{sql}\n").expect("format statement");
    }
    out
}

#[test]
fn present_field_sql_is_unchanged() {
    let fixture = Fixture::build();
    let exec = Executor::new().expect("executor");
    let pins = catalog();
    let hot_pins = hot_pins();

    let cold_only = lane_statements(&fixture, "cold-only", QUERIES.iter().copied(), |dsl| {
        prepared("cold-only", dsl, || {
            exec.run_query(dsl, &fixture.cold, &pins, usize::MAX, 0)
                .map(drop)
        })
    });
    insta::assert_snapshot!("cold_only", cold_only);

    let union = lane_statements(&fixture, "hot+cold", QUERIES.iter().copied(), |dsl| {
        prepared("hot+cold", dsl, || {
            exec.run_query_with_hot(
                dsl,
                &fixture.cold,
                fixture.hot_source(),
                &hot_columns_of(fixture.hot_source()),
                &hot_pins,
                &pins,
                usize::MAX,
                0,
            )
            .map(drop)
        })
    });
    insta::assert_snapshot!("hot_and_cold", union);

    let hot_only = lane_statements(&fixture, "hot-only", QUERIES.iter().copied(), |dsl| {
        prepared("hot-only", dsl, || {
            exec.run_query_with_hot(
                dsl,
                &fixture.cold_start,
                fixture.hot_source(),
                &hot_columns_of(fixture.hot_source()),
                &hot_pins,
                &pins,
                usize::MAX,
                0,
            )
            .map(drop)
        })
    });
    insta::assert_snapshot!("hot_only", hot_only);

    let out = fixture.dir.path().join("export.parquet");
    let export = lane_statements(&fixture, "export", export_queries(), |dsl| {
        prepared("export", dsl, || {
            exec.export_parquet(dsl, &fixture.cold, &pins, &out, usize::MAX)
        })
    });
    insta::assert_snapshot!("export", export);

    let export_hot = lane_statements(&fixture, "export hot+cold", export_queries(), |dsl| {
        prepared("export hot+cold", dsl, || {
            exec.export_parquet_with_hot(
                dsl,
                &fixture.cold,
                fixture.hot_source(),
                &hot_columns_of(fixture.hot_source()),
                &hot_pins,
                &pins,
                &out,
                usize::MAX,
            )
        })
    });
    insta::assert_snapshot!("export_hot_and_cold", export_hot);
}

// -- the probe itself ---------------------------------------------------------
//
// The emitter learns what the input carries from one `DESCRIBE` of the
// resolved cold reader per request (ADR-0062), counted and recorded by
// `executor::schema_probe`. The tests below read that record on the thread
// that ran the lane, so parallel tests cannot see each other's probes.

/// The lanes one request can take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lane {
    /// The query lane with no hot buffer: embedded `--data`, `from saved`.
    Cold,
    /// The query lane over cold files and a hot snapshot.
    HotAndCold,
    /// The query lane over a hot snapshot and a source no file is behind.
    ColdStart,
    /// The Parquet export with no hot buffer.
    Export,
    /// The Parquet export over cold files and a hot snapshot.
    ExportHotAndCold,
}

impl Lane {
    const ALL: [Self; 5] = [
        Self::Cold,
        Self::HotAndCold,
        Self::ColdStart,
        Self::Export,
        Self::ExportHotAndCold,
    ];

    /// The lanes whose source reaches cold files, so the probe has a
    /// reader to describe.
    const OVER_COLD_FILES: [Self; 4] = [
        Self::Cold,
        Self::HotAndCold,
        Self::Export,
        Self::ExportHotAndCold,
    ];

    fn reads_hot(self) -> bool {
        matches!(
            self,
            Self::HotAndCold | Self::ColdStart | Self::ExportHotAndCold
        )
    }

    fn is_export(self) -> bool {
        matches!(self, Self::Export | Self::ExportHotAndCold)
    }
}

/// What one request on a lane reads.
struct Request<'a> {
    fixture: &'a Fixture,
    /// The hot snapshot file, for the lanes that read one.
    hot: &'a Path,
    hot_pins: &'a FieldTypes,
    pins: &'a FieldTypes,
}

/// What one request did.
struct Measured {
    outcome: Result<(), EngineError>,
    totals: PhaseTotals,
    /// `DESCRIBE`s this thread handed `DuckDB` during the request.
    describes: u64,
}

impl<'a> Request<'a> {
    fn new(fixture: &'a Fixture, pins: &'a FieldTypes, hot_pins: &'a FieldTypes) -> Self {
        Request {
            fixture,
            hot: &fixture.hot,
            hot_pins,
            pins,
        }
    }

    fn source(&self, lane: Lane) -> &str {
        if lane == Lane::ColdStart {
            &self.fixture.cold_start
        } else {
            &self.fixture.cold
        }
    }

    /// Run `dsl` on `lane` under `cancel`, booking its phases on `clock`.
    fn run_on(
        &self,
        exec: &Executor,
        lane: Lane,
        dsl: &str,
        cancel: &CancelLatch,
        clock: &PhaseClock,
    ) -> (Result<(), EngineError>, Option<PathBuf>) {
        let lanes = exec.cancellable(cancel).timed(clock);
        let hot = self.hot.to_str().expect("utf-8 hot path");
        let hot_columns = hot_columns_of(self.hot);
        let source = self.source(lane);
        let out = lane.is_export().then(|| {
            let out = self.fixture.dir.path().join(format!("{lane:?}.parquet"));
            let _ = std::fs::remove_file(&out);
            out
        });
        let cap = RowCap::Refuse(usize::MAX);
        let outcome = match lane {
            Lane::Cold => lanes.run_query(dsl, source, self.pins, cap, 0).map(drop),
            Lane::HotAndCold | Lane::ColdStart => lanes
                .run_query_with_hot(
                    dsl,
                    source,
                    hot,
                    &hot_columns,
                    self.hot_pins,
                    self.pins,
                    cap,
                    0,
                )
                .map(drop),
            Lane::Export => lanes.export_parquet(
                dsl,
                source,
                self.pins,
                out.as_deref().expect("an export path"),
                usize::MAX,
            ),
            Lane::ExportHotAndCold => lanes.export_parquet_with_hot(
                dsl,
                source,
                hot,
                &hot_columns,
                self.hot_pins,
                self.pins,
                out.as_deref().expect("an export path"),
                usize::MAX,
            ),
        };
        (outcome, out)
    }

    /// Run `dsl` on `lane` on this thread and count its probes.
    fn measure(&self, exec: &Executor, lane: Lane, dsl: &str) -> Measured {
        let clock = PhaseClock::new();
        let before = schema_probe::count_on_this_thread();
        let (outcome, _) = self.run_on(exec, lane, dsl, &CancelLatch::never(), &clock);
        let describes = schema_probe::count_on_this_thread() - before;
        let totals = clock.close_at(Instant::now()).expect("the clock records");
        Measured {
            outcome,
            totals,
            describes,
        }
    }
}

/// The `DESCRIBE` the probe hands `DuckDB` for `source`: the emitter's own
/// reader for it, verbatim.
fn describe_of(source: &str) -> String {
    let reader = trawl_core::emitter::source_reader(source).expect("the source has a reader");
    format!("DESCRIBE SELECT * FROM {reader}")
}

/// A hot snapshot like the fixture's, every event also carrying `ghost`
/// as JSON `null` and nothing else.
fn null_only_hot(fixture: &Fixture) -> PathBuf {
    let text = std::fs::read_to_string(&fixture.hot).expect("the fixture's hot snapshot");
    let mut out = String::new();
    for line in text.lines() {
        let mut event: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(line).expect("a hot event");
        event.insert("ghost".to_owned(), serde_json::Value::Null);
        writeln!(out, "{}", serde_json::Value::Object(event)).expect("format hot line");
    }
    let path = fixture.dir.path().join("hot-null-only.ndjson");
    std::fs::write(&path, out).expect("hot snapshot");
    path
}

/// One probe per request, whatever it reads, booked as `probe` and never a
/// bind attempt; none for a query that reads only the envelope's
/// timestamps; and none for a query admission refuses.
#[test]
fn schema_probe_is_bounded() {
    let fixture = Fixture::build();
    let exec = Executor::new().expect("executor");
    let pins = catalog();
    let hot_pins = hot_pins();
    let request = Request::new(&fixture, &pins, &hot_pins);
    one_probe_per_request(&exec, &request);
    no_probe_for_the_envelope(&exec, &request);
    a_null_only_hot_key_asks_once(&exec, &fixture);
    no_probe_past_admission(&exec, &request);
}

/// A query that reads a field the input carries: one DESCRIBE of the
/// resolved cold reader, on every lane that has one. The cold start's
/// source reaches no file, which resolution already proved, so it has
/// nothing to describe.
fn one_probe_per_request(exec: &Executor, request: &Request<'_>) {
    for lane in Lane::ALL {
        let run = request.measure(exec, lane, "host=web1");
        if let Err(e) = &run.outcome {
            panic!("{lane:?}: a present-field query answers: {e:?}");
        }
        if lane == Lane::ColdStart {
            assert_eq!(run.describes, 0, "{lane:?}: no file to describe");
            assert_eq!(run.totals.get(QueryPhase::Probe), None, "{lane:?}");
            assert_eq!(run.totals.attempts, 2, "{lane:?}: the union, then hot-only");
            continue;
        }
        assert_eq!(run.describes, 1, "{lane:?}: one probe");
        assert_eq!(
            schema_probe::last_statement().as_deref(),
            Some(describe_of(&request.fixture.cold).as_str()),
            "{lane:?}: the probe describes the resolved cold reader"
        );
        assert!(
            run.totals.get(QueryPhase::Probe).is_some(),
            "{lane:?}: booked as probe: {:?}",
            run.totals
        );
        assert_eq!(
            run.totals.attempts, 1,
            "{lane:?}: the probe is not a bind attempt"
        );
    }

    // However many fields the input lacks, one probe. What the query
    // answers is the absent-field oracle's to check.
    for lane in Lane::OVER_COLD_FILES {
        let run = request.measure(
            exec,
            lane,
            "f=x g!=y NOT h=z | where isnull(i) | let j = k | table host, f, j",
        );
        assert_eq!(
            run.describes, 1,
            "{lane:?}: one probe for five absent names"
        );
        assert!(run.totals.attempts <= 1, "{lane:?}: {:?}", run.totals);
    }
}

/// A query that reads only the envelope's timestamps, or nothing at
/// all, needs no description of its input.
fn no_probe_for_the_envelope(exec: &Executor, request: &Request<'_>) {
    for dsl in [
        "*",
        "* | head 100",
        "last=1h",
        r#"earliest="2024-01-15T10:00:00Z" | stats count()"#,
        "* | timechart span=1h count()",
        "* | table _time, _ingested",
    ] {
        for lane in Lane::ALL {
            let run = request.measure(exec, lane, dsl);
            if let Err(e) = &run.outcome {
                panic!("{lane:?}: `{dsl}` answers: {e:?}");
            }
            assert_eq!(
                run.describes, 0,
                "{lane:?}: `{dsl}` reads only the envelope"
            );
        }
    }
}

/// An envelope-only query asks once after all when the hot snapshot holds
/// a key that was only ever `null` and no hot pin types: without the cold
/// description, `*` types that key as JSON over the cold side. A pin
/// types the key, and the query asks nothing again.
fn a_null_only_hot_key_asks_once(exec: &Executor, fixture: &Fixture) {
    let pins = catalog();
    let hot_pins = hot_pins();
    let ghost_hot = null_only_hot(fixture);
    let ghostly = Request {
        hot: &ghost_hot,
        ..Request::new(fixture, &pins, &hot_pins)
    };
    for lane in Lane::ALL {
        let run = ghostly.measure(exec, lane, "* | head 100");
        if let Err(e) = &run.outcome {
            panic!("{lane:?}: `* | head 100` answers beside a null-only key: {e:?}");
        }
        let expected = u64::from(lane.reads_hot() && lane != Lane::ColdStart);
        assert_eq!(
            run.describes, expected,
            "{lane:?}: an unpinned null-only hot key asks once where there is a cold reader"
        );
    }
    let mut ghost_pins = catalog();
    ghost_pins.insert("ghost", CanonicalType::BigInt);
    let pinned = Request {
        hot: &ghost_hot,
        ..Request::new(fixture, &ghost_pins, &ghost_pins)
    };
    for lane in Lane::ALL {
        let run = pinned.measure(exec, lane, "* | head 100");
        if let Err(e) = &run.outcome {
            panic!("{lane:?}: `* | head 100` answers beside a pinned null-only key: {e:?}");
        }
        assert_eq!(run.describes, 0, "{lane:?}: the pin types the key");
    }
}

/// Admission refuses before anything reaches `DuckDB` (ADR-0024), the
/// probe included.
fn no_probe_past_admission(exec: &Executor, request: &Request<'_>) {
    let over_budget = format!(
        "host=web1{}",
        " | head 1".repeat(trawl_core::complexity::MAX_PIPELINE_STAGES + 1)
    );
    for lane in Lane::ALL {
        let run = request.measure(exec, lane, &over_budget);
        assert!(
            matches!(run.outcome, Err(EngineError::Emit(_))),
            "{lane:?}: {:?}",
            run.outcome
        );
        assert_eq!(run.describes, 0, "{lane:?}: refused before the probe");
    }
}

/// A probe parked inside `DuckDB` holds its lane: the lane does not return
/// while the probe is parked, a cancellation that lands during the park
/// stops the lane before its statement binds, and the whole park is booked
/// as `probe`.
///
/// This is the engine half. The permit and the deadline belong to the
/// server's pool, which runs the lane inside them, so a lane that has not
/// returned still holds both.
#[test]
fn parked_probe_keeps_its_permit() {
    const PARK: Duration = Duration::from_millis(30);
    const WAIT: Duration = Duration::from_secs(30);
    let fixture = Fixture::build();
    let exec = Executor::new().expect("executor");
    let pins = catalog();
    let request = Request::new(&fixture, &pins, &pins);
    let marker = fixture.dir.path().display().to_string();

    for lane in Lane::OVER_COLD_FILES {
        let (parked_tx, parked_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let _park = schema_probe::park_when(&marker, move || {
            parked_tx.send(()).expect("the test waits for the park");
            release_rx
                .lock()
                .expect("unpoisoned")
                .recv_timeout(WAIT)
                .expect("one park per request, and the test releases it");
        });
        let flag = Arc::new(AtomicBool::new(false));
        let latch = CancelLatch::new(Arc::clone(&flag));
        let clock = PhaseClock::new();
        let worker_exec = exec.try_clone().expect("a connection for the worker");

        let (outcome, out) = std::thread::scope(|scope| {
            let request = &request;
            let (latch, clock) = (&latch, &clock);
            let worker =
                scope.spawn(move || request.run_on(&worker_exec, lane, "host=web1", latch, clock));
            parked_rx.recv_timeout(WAIT).expect("the probe parks");
            let (partial, _) = clock.snapshot().expect("the clock records");
            assert_eq!(
                partial.active.map(|(phase, _)| phase),
                Some(QueryPhase::Probe),
                "{lane:?}: parked inside the probe"
            );
            flag.store(true, Ordering::SeqCst);
            std::thread::sleep(PARK);
            assert!(
                !worker.is_finished(),
                "{lane:?}: the lane does not return while its probe is parked"
            );
            release_tx.send(()).expect("the worker waits");
            worker.join().expect("the worker finishes")
        });

        assert!(
            matches!(outcome, Err(EngineError::Cancelled)),
            "{lane:?}: a latch set during the park cancels the lane: {outcome:?}"
        );
        let totals = clock.close_at(Instant::now()).expect("the clock records");
        assert_eq!(totals.attempts, 0, "{lane:?}: the statement never bound");
        let probe = totals.get(QueryPhase::Probe).expect("probe booked");
        assert!(
            probe >= u64::try_from(PARK.as_micros()).expect("fits"),
            "{lane:?}: the park is booked as probe: {probe}us"
        );
        if let Some(out) = out {
            assert!(!out.exists(), "{lane:?}: a cancelled export writes nothing");
        }
    }
}
