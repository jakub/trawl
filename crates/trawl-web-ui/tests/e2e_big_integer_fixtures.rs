// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The big-integer fixtures `big-integers.spec.ts` serves, produced by
//! the result wire's own encoder (issue #214).
//!
//! A big integer is the case a hand-typed fixture gets wrong: a JSON
//! number past 2^53 reads back as a different number in any JavaScript
//! tool that touches it, and a decimal string written by hand can drift
//! from the canonical spelling the decoder insists on. So these bodies
//! are never typed. Each test below builds the response from `Value`s,
//! encodes it with `trawl_api`'s serde, and requires the committed file
//! to be exactly that text.
//!
//! To change a fixture, change the rows here and rewrite the files:
//!
//! ```sh
//! TRAWL_E2E_WRITE_FIXTURES=1 cargo nextest run -p trawl-web-ui --test e2e_big_integer_fixtures
//! ```

use std::path::PathBuf;

use trawl_api::value::{Column, ColumnType, QueryResult, Value};
use trawl_api::{PaginationMeta, QueryExecution, QueryResponse};

/// 2^53 + 1: the first integer an IEEE double cannot hold.
const ABOVE_2_53: i64 = 9_007_199_254_740_993;
/// 2^63: the first integer past `i64`, so it lands as `UInt`.
const TWO_63: u64 = 9_223_372_036_854_775_808;

/// The cells in ascending order, null first, as the grid's sort puts
/// them: the order `big-integers.spec.ts` reads back.
const ASCENDING: [&str; 5] = [
    "-9007199254740993",
    "9007199254740993",
    "9223372036854775807",
    "9223372036854775808",
    "18446744073709551615",
];

fn instant(hour: u32, minute: u32) -> Value {
    Value::String(format!("2026-09-01T{hour:02}:{minute:02}:00.000000Z"))
}

fn text(s: &str) -> Value {
    Value::String(s.to_owned())
}

fn response(result: QueryResult) -> QueryResponse {
    let rows = result.rows.len();
    QueryResponse {
        execution: Some(QueryExecution {
            started_at: "2026-09-15T12:34:56Z".to_owned(),
            duration_ms: 125,
        }),
        result,
        pagination: PaginationMeta {
            limit: 50,
            offset: 0,
            returned: rows,
            total: rows,
        },
        degraded_fields: Vec::new(),
        severity_columns: Vec::new(),
    }
}

/// A plain search: six events, one `bytes` cell each, out of order, so
/// neither sort direction is the order they arrived in.
fn rows_fixture() -> QueryResponse {
    response(QueryResult {
        columns: vec![
            Column::new("_time", ColumnType::Timestamp),
            Column::new("host", ColumnType::String),
            Column::new("bytes", ColumnType::Integer),
            Column::new("message", ColumnType::String),
        ],
        rows: vec![
            vec![
                instant(10, 0),
                text("web-01"),
                Value::UInt(u64::MAX),
                text("u64 max"),
            ],
            vec![
                instant(10, 1),
                text("web-02"),
                Value::Integer(ABOVE_2_53),
                text("2^53 + 1"),
            ],
            vec![
                instant(10, 2),
                text("web-03"),
                Value::Null,
                text("no bytes"),
            ],
            vec![
                instant(10, 3),
                text("web-04"),
                Value::UInt(TWO_63),
                text("2^63"),
            ],
            vec![
                instant(10, 4),
                text("web-05"),
                Value::Integer(-ABOVE_2_53),
                text("negative"),
            ],
            vec![
                instant(10, 5),
                text("web-06"),
                Value::Integer(i64::MAX),
                text("i64 max"),
            ],
        ],
    })
}

/// `stats count() by host`: one big count per group, for the exact
/// table and the categorical chart.
fn stats_by_fixture() -> QueryResponse {
    response(QueryResult {
        columns: vec![
            Column::new("host", ColumnType::String),
            Column::new("count", ColumnType::Integer),
        ],
        rows: vec![
            vec![text("web-01"), Value::UInt(u64::MAX)],
            vec![text("web-02"), Value::Integer(ABOVE_2_53)],
            vec![text("web-03"), Value::UInt(TWO_63)],
            vec![text("web-04"), Value::Integer(i64::MAX)],
        ],
    })
}

/// `timechart span=1h count()`: four hourly buckets of big counts, for
/// the series chart.
fn timechart_fixture() -> QueryResponse {
    response(QueryResult {
        columns: vec![
            Column::new("_time", ColumnType::Timestamp),
            Column::new("count", ColumnType::Integer),
        ],
        rows: vec![
            vec![instant(0, 0), Value::Integer(ABOVE_2_53)],
            vec![instant(1, 0), Value::Integer(i64::MAX)],
            vec![instant(2, 0), Value::UInt(TWO_63)],
            vec![instant(3, 0), Value::UInt(u64::MAX)],
        ],
    })
}

fn wire_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("e2e/harness/wire")
        .join(format!("{name}.json"))
}

/// The committed file is the encoder's output, byte for byte, and it
/// decodes back to the same cells.
fn golden(name: &str, fixture: &QueryResponse) {
    let encoded = serde_json::to_string_pretty(fixture).expect("the fixture encodes") + "\n";
    let path = wire_path(name);
    if std::env::var_os("TRAWL_E2E_WRITE_FIXTURES").is_some() {
        std::fs::write(&path, &encoded).expect("write the fixture");
    }
    let committed =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert!(
        committed == encoded,
        "e2e/harness/wire/{name}.json is not what the encoder writes. Rewrite it with \
         TRAWL_E2E_WRITE_FIXTURES=1 cargo nextest run -p trawl-web-ui --test e2e_big_integer_fixtures\n\
         --- expected ---\n{encoded}"
    );
    let decoded: QueryResponse = serde_json::from_str(&committed).expect("the fixture decodes");
    assert_eq!(decoded.result, fixture.result);
}

/// Every integer cell is a quoted decimal string: no JSON number in the
/// file is past 2^53, so no JavaScript reader can round one.
fn assert_no_bare_big_number(name: &str) {
    let committed = std::fs::read_to_string(wire_path(name)).unwrap();
    let mut digits = 0usize;
    let mut quoted = false;
    for c in committed.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                digits = 0;
            }
            '0'..='9' if !quoted => {
                digits += 1;
                assert!(
                    digits < 16,
                    "{name}.json carries a bare JSON number of 16+ digits"
                );
            }
            _ => digits = 0,
        }
    }
}

#[test]
fn the_big_integer_rows_fixture_is_the_encoders_output() {
    let fixture = rows_fixture();
    golden("query-big-integers", &fixture);
    assert_no_bare_big_number("query-big-integers");

    // Each magnitude lands in the variant the SPA classifies by: the two
    // past `i64::MAX` as `UInt`, the rest as `Integer`.
    let bytes: Vec<&Value> = fixture.result.rows.iter().map(|r| &r[2]).collect();
    assert!(matches!(bytes[0], Value::UInt(u64::MAX)));
    assert!(matches!(bytes[3], Value::UInt(TWO_63)));
    assert!(matches!(bytes[1], Value::Integer(ABOVE_2_53)));

    let mut ascending: Vec<String> = bytes
        .iter()
        .filter(|v| !matches!(v, Value::Null))
        .map(ToString::to_string)
        .collect();
    ascending.sort_by_key(|s| s.parse::<i128>().unwrap());
    assert_eq!(ascending, ASCENDING);
}

#[test]
fn the_big_integer_stats_by_fixture_is_the_encoders_output() {
    golden("query-big-integers-stats-by", &stats_by_fixture());
    assert_no_bare_big_number("query-big-integers-stats-by");
}

#[test]
fn the_big_integer_timechart_fixture_is_the_encoders_output() {
    golden("query-big-integers-timechart", &timechart_fixture());
    assert_no_bare_big_number("query-big-integers-timechart");
}
