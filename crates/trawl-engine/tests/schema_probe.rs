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
//! query here reads an absent field or a null-only hot key.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use trawl_core::schema::{CanonicalType, FieldTypes};
use trawl_engine::executor::{Executor, hot_columns_of, prepare_probe};

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
