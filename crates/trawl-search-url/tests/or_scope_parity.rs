// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Search and Send keep the same rows for an OR search.
//!
//! Search runs [`effective_query`]: the chips and the absolute range merged
//! into the text. Send runs [`capture_parts`]: the range-free text, with the
//! window spliced back by trawld as a global `earliest=`/`latest=` prefix.
//! Both texts are parsed and emitted by trawl-core and executed by `DuckDB`
//! over the same events, and must keep the same events: the chips and the
//! range narrow every OR group, not only the first.

use chrono::{DateTime, Utc};
use duckdb::Connection;

use trawl_core::context::EvalContext;
use trawl_core::emitter::{self, SqlValue};
use trawl_search_url::query_merge::{
    CaptureParts, Filter, FilterOp, RangeSpec, capture_parts, effective_query,
};

const FROM: &str = "2026-09-15T12:00:00Z";
const TO: &str = "2026-09-15T12:30:00Z";

/// One web event inside the range, one db event a day before it, and one
/// db event inside it on another host.
const EVENTS: &str = "SELECT * FROM (VALUES \
    (1, TIMESTAMP '2026-09-15 12:10:00', 'web', 'a'), \
    (2, TIMESTAMP '2026-09-14 12:10:00', 'db', 'a'), \
    (3, TIMESTAMP '2026-09-15 12:20:00', 'db', 'b')) \
    AS t(id, _time, service, host)";

fn range() -> RangeSpec {
    RangeSpec::Absolute {
        from: FROM.into(),
        to: TO.into(),
    }
}

/// The text trawld runs for a capture: the window spliced in front of the
/// range-free text as absolute microsecond bounds, the spelling of
/// `materialize_window` in trawl-server's `report_window.rs`.
fn as_trawld_runs(parts: &CaptureParts) -> String {
    let window = parts.window.as_ref().expect("the capture carries a window");
    format!(
        "earliest=\"{}\" latest=\"{}\" {}",
        window.earliest, window.latest, parts.query
    )
}

fn ids(conn: &Connection, source: &str, dsl: &str) -> Vec<i64> {
    let query = trawl_core::parser::parse(dsl).unwrap_or_else(|e| panic!("{dsl:?}: {e:?}"));
    let anchor: DateTime<Utc> = "2026-09-16T00:00:00Z".parse().unwrap();
    let emitted = emitter::emit(&query, source, EvalContext::at(anchor))
        .unwrap_or_else(|e| panic!("{dsl:?}: {e:?}"));
    let params: Vec<Box<dyn duckdb::ToSql>> = emitted
        .params
        .iter()
        .map(|v| -> Box<dyn duckdb::ToSql> {
            match v {
                SqlValue::String(s) => Box::new(s.clone()),
                SqlValue::Int(i) => Box::new(*i),
                SqlValue::Float(f) => Box::new(*f),
                SqlValue::Bool(b) => Box::new(*b),
                SqlValue::Timestamp(at) => Box::new(duckdb::types::Value::Timestamp(
                    duckdb::types::TimeUnit::Microsecond,
                    at.and_utc().timestamp_micros(),
                )),
            }
        })
        .collect();
    let refs: Vec<&dyn duckdb::ToSql> = params.iter().map(AsRef::as_ref).collect();
    let sql = format!("SELECT id FROM ({}) AS _sub ORDER BY id", emitted.sql);
    let mut stmt = conn
        .prepare(&sql)
        .unwrap_or_else(|e| panic!("{dsl:?}: {e}\n{sql}"));
    stmt.query_map(refs.as_slice(), |row| row.get::<_, i64>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn assert_search_and_send_keep(base: &str, filters: &[Filter], expected: &[i64]) {
    let conn = Connection::open_in_memory().unwrap();
    let file = tempfile::Builder::new()
        .suffix(".parquet")
        .tempfile()
        .unwrap();
    let source = file.path().to_str().unwrap().to_owned();
    conn.execute_batch(&format!("COPY ({EVENTS}) TO '{source}' (FORMAT PARQUET)"))
        .unwrap();

    let search = effective_query(base, filters, &range());
    let parts = capture_parts(base, filters, &range(), None).expect("a capture request");
    let send = as_trawld_runs(&parts);
    let shown = ids(&conn, &source, &search);
    let kept = ids(&conn, &source, &send);
    assert_eq!(shown, expected, "Search ran {search:?}");
    assert_eq!(kept, expected, "Send ran {send:?}");
}

#[test]
fn an_absolute_range_bounds_every_or_group_in_search_and_send() {
    assert_search_and_send_keep("service=web OR service=db", &[], &[1, 3]);
}

#[test]
fn a_chip_and_a_range_narrow_every_or_group_in_search_and_send() {
    let chip = Filter {
        field: "host".into(),
        value: "a".into(),
        op: FilterOp::Include,
    };
    assert_search_and_send_keep("service=web OR service=db", &[chip], &[1]);
}
