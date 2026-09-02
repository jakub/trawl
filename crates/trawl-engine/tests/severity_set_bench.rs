// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What each severity-predicate shape actually costs, executed against the
//! bundled `DuckDB`.
//!
//! # The finding, and it contradicts the text-size argument
//!
//! Reasoning from SQL text size predicts the wrong winner. A `sev()`
//! subject is over a kilobyte and the per-band rendering repeats it once
//! per band, so collapsing the bands into one `IN` over ladder points
//! should be cheaper. The text does shrink, 5.68x for a six-band list, but
//! execution goes the other way by a lot. `DuckDB` evaluates a repeated
//! `BETWEEN` subject far more cheaply than it evaluates the same subject
//! under an `IN` list, so the `IN` rendering is 3.6x slower for six bands
//! and 62x slower for a single band.
//!
//! What ships is the third shape this probe times: keep ranges, but merge
//! the ladder points into minimal contiguous ones. Six contiguous bands
//! then collapse to a single `BETWEEN 1 AND 24`, the subject written once
//! and ~5.9x faster than the per-band rendering. The `IN` shape stays in
//! the matrix as the falsified-premise record, so a future reader who has
//! the same idea can see it was measured and rejected.
//!
//! Not a correctness test and not run in CI: `#[ignore]`d, because a
//! wall-clock ratio on a shared machine is evidence for a human reading a
//! PR, not a gate. Run it explicitly:
//!
//! ```sh
//! cargo test -p trawl-engine --test severity_set_bench --release -- --ignored --nocapture
//! ```
//!
//! # Why it times shapes rather than two checkouts
//!
//! Every predicate is built here from the one subject builder
//! (`conform::severity_reading_sql_bind_once`), so the per-band shape (the
//! subject repeated once per band), the shipped merged-range shape and the
//! rejected `IN` one are measured in the same process, against the same
//! corpus, with the same engine build.
//! Timing two git checkouts instead would compare two binaries built
//! minutes apart, which is how a 60x "regression" that is really a
//! build/cache artifact gets into a PR body.
//!
//! `emitted_shapes_match_the_hand_built_ones` is the guard that keeps this
//! honest: the strings the emitter actually produces today are asserted
//! equal to the hand-built merged-range ones this bench times.
//!
//! The corpus is 1M rows of a realistic token distribution over a VARCHAR
//! `level` column, the shape a sender that never adopted `_severity`
//! actually writes.

use std::time::{Duration, Instant};

use duckdb::Connection;
use trawl_core::conform::{severity_reading_sql_bind_once, untyped_text};
use trawl_core::severity::Dialect;

/// A realistic homelab severity mix: mostly info, a long debug tail, and
/// errors rare enough that the predicate is genuinely selective.
const DISTRIBUTION: &[(&str, u32)] = &[
    ("info", 58),
    ("debug", 20),
    ("warn", 10),
    ("error", 6),
    ("trace", 3),
    ("notice", 2),
    ("fatal", 1),
];

const ROWS: usize = 1_000_000;
const REPETITIONS: usize = 7;

/// The six disjoint base bands, as `(lo, hi)` — what
/// `sev(level) in ("trace", "debug", "info", "warn", "error", "fatal")`
/// resolves to.
const SIX_BANDS: [(i64, i64); 6] = [(1, 4), (5, 8), (9, 12), (13, 16), (17, 20), (21, 24)];
/// One band — what `sev(level) == "error"` resolves to.
const ONE_BAND: [(i64, i64); 1] = [(17, 20)];
/// A genuinely DISJOINT selection — `sev(level) in ("warn", "fatal")`,
/// the one shape merging cannot collapse to a single range.
const DISJOINT: [(i64, i64); 2] = [(13, 16), (21, 24)];

fn conn() -> Connection {
    Connection::open_in_memory().expect("in-memory duckdb")
}

/// The `sev(level)` subject every shape below compares: the expensive
/// thing, over a kilobyte of `list_transform` SQL.
fn subject() -> String {
    severity_reading_sql_bind_once(&untyped_text(r#""level""#), Dialect::Otel)
}

/// The per-band rendering: one `BETWEEN` per band, OR'd, each carrying its
/// own copy of the subject.
fn ranges_sql(bands: &[(i64, i64)]) -> String {
    let subject = subject();
    bands
        .iter()
        .map(|(lo, hi)| format!("{subject} BETWEEN {lo} AND {hi}"))
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// The shipped rendering: the points merged back into minimal contiguous
/// ranges, so contiguous bands share one subject.
fn merged_sql(bands: &[(i64, i64)]) -> String {
    let subject = subject();
    let points: Vec<i64> = bands
        .iter()
        .flat_map(|(lo, hi)| *lo..=*hi)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let ranges = trawl_core::compare::severity_ranges(&points);
    let positive = ranges
        .iter()
        .map(|(lo, hi)| {
            if lo == hi {
                format!("{subject} = {lo}")
            } else {
                format!("{subject} BETWEEN {lo} AND {hi}")
            }
        })
        .collect::<Vec<_>>()
        .join(" OR ");
    if ranges.len() > 1 {
        format!("({positive})")
    } else {
        positive
    }
}

/// The rejected candidate, kept as the falsified-premise record: the
/// subject once, every ladder point inlined into one `IN`.
fn points_sql(bands: &[(i64, i64)]) -> String {
    let points: Vec<String> = bands
        .iter()
        .flat_map(|(lo, hi)| (*lo..=*hi).map(|n| n.to_string()))
        .collect();
    format!("{} IN ({})", subject(), points.join(", "))
}

/// Write `ROWS` rows of `(level, message)` to a parquet file, cycling the
/// distribution deterministically — the mix is what matters, not the
/// order, and a fixed corpus makes repeated runs comparable.
fn corpus(conn: &Connection, path: &std::path::Path) {
    let mut weighted: Vec<&str> = Vec::new();
    for (token, weight) in DISTRIBUTION {
        for _ in 0..*weight {
            weighted.push(token);
        }
    }
    let list = weighted
        .iter()
        .map(|t| format!("'{t}'"))
        .collect::<Vec<_>>()
        .join(", ");
    conn.execute_batch(&format!(
        "COPY (SELECT [{list}][(i % {}) + 1] AS level, \
                      'request handled' AS message \
               FROM range(0, {ROWS}) AS t(i)) \
         TO '{}' (FORMAT PARQUET)",
        weighted.len(),
        path.display()
    ))
    .expect("write corpus");
}

/// One timed measurement: the predicate's SQL length, its best elapsed
/// time and the rows it matched.
struct Measured {
    sql_len: usize,
    best: Duration,
    rows: i64,
}

/// Time `REPETITIONS` executions of one predicate over the corpus,
/// returning the MINIMUM elapsed time.
///
/// The minimum, not the mean: on a shared desktop the noise is all
/// upward, so the fastest run is the closest to the cost being measured.
fn time_predicate(conn: &Connection, source: &str, predicate: &str) -> Measured {
    let sql = format!(
        "SELECT count(*)::BIGINT FROM read_parquet('{source}', union_by_name=true) \
         WHERE ({predicate})"
    );
    // One untimed warm-up so the parquet metadata is cached for every shape.
    let mut rows: i64 = conn.query_row(&sql, [], |r| r.get(0)).expect("count runs");
    let mut best = Duration::MAX;
    for _ in 0..REPETITIONS {
        let start = Instant::now();
        rows = conn.query_row(&sql, [], |r| r.get(0)).expect("count runs");
        best = best.min(start.elapsed());
    }
    Measured {
        sql_len: predicate.len(),
        best,
        rows,
    }
}

#[test]
#[ignore = "wall-clock cost probe; run explicitly with --ignored --nocapture"]
fn severity_predicate_shape_costs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("corpus.parquet");
    let conn = conn();
    corpus(&conn, &file);
    let source = file.display().to_string();

    // Three groups of three, in a fixed order the ratio loop indexes: the
    // per-band shape (labelled `before`), the shipped merged-range one
    // (`AFTER`), the rejected `IN` one.
    let shapes = [
        ("one band  before (1x BETWEEN)", ranges_sql(&ONE_BAND)),
        ("one band  AFTER  (merged range)", merged_sql(&ONE_BAND)),
        ("one band  rejected (IN)", points_sql(&ONE_BAND)),
        (
            "six bands before (6x BETWEEN, OR'd)",
            ranges_sql(&SIX_BANDS),
        ),
        ("six bands AFTER  (merged range)", merged_sql(&SIX_BANDS)),
        ("six bands rejected (IN)", points_sql(&SIX_BANDS)),
        ("disjoint  before (2x BETWEEN)", ranges_sql(&DISJOINT)),
        ("disjoint  AFTER  (2 merged ranges)", merged_sql(&DISJOINT)),
        ("disjoint  rejected (IN)", points_sql(&DISJOINT)),
    ];

    println!("issue-82 severity set cost probe");
    println!("  rows              {ROWS}, {REPETITIONS} repetitions, best-of");
    println!("  distribution      {DISTRIBUTION:?}");
    let mut measured = Vec::new();
    for (label, predicate) in &shapes {
        let m = time_predicate(&conn, &source, predicate);
        println!(
            "  {label:34}  {:>10.2?}  ({} rows, {} B of SQL)",
            m.best, m.rows, m.sql_len
        );
        measured.push(m);
    }
    let ratio =
        |before: &Measured, after: &Measured| before.best.as_secs_f64() / after.best.as_secs_f64();
    #[allow(clippy::cast_precision_loss)]
    let sql_ratio =
        |before: &Measured, after: &Measured| before.sql_len as f64 / after.sql_len as f64;
    for (label, before, after, rejected) in [
        ("one band ", 0, 1, 2),
        ("six bands", 3, 4, 5),
        ("disjoint ", 6, 7, 8),
    ] {
        println!(
            "  {label}  merged {:.2}x vs before (SQL {:.2}x smaller) | \
             the rejected IN shape was {:.2}x vs before",
            ratio(&measured[before], &measured[after]),
            sql_ratio(&measured[before], &measured[after]),
            ratio(&measured[before], &measured[rejected])
        );
    }

    // Not a threshold assertion — the numbers are the deliverable. Only
    // the sanity of the corpus is asserted, so a probe that silently
    // measured an empty file cannot be read as a win.
    assert!(
        measured[0].rows > 0,
        "the one-band predicate must match rows"
    );
    for group in [0, 3, 6] {
        assert_eq!(
            measured[group].rows,
            measured[group + 1].rows,
            "every shape in a group must match the same rows"
        );
        assert_eq!(
            measured[group].rows,
            measured[group + 2].rows,
            "every shape in a group must match the same rows"
        );
    }
    assert!(
        measured[3].rows > measured[0].rows,
        "six bands must match more rows than one"
    );
}

/// The guard that makes the bench above honest: the hand-built
/// merged-range shapes it times are exactly what the emitter produces for
/// the equivalent DSL.
#[test]
fn emitted_shapes_match_the_hand_built_ones() {
    let emitted = |dsl: &str| {
        let query = trawl_core::parser::parse(dsl).expect("dsl parses");
        let sql =
            trawl_core::emitter::emit(&query, "X", trawl_core::context::EvalContext::capture())
                .expect("emit succeeds")
                .sql;
        let (_, predicate) = sql.split_once("WHERE ").expect("a where clause");
        predicate.trim().to_owned()
    };
    // The pipeline arm parenthesizes its clause for composition, but does
    // not double-wrap one that already arrives grouped, which is exactly
    // the multi-range case. Expect what that rule produces.
    let composed = |bands: &[(i64, i64)]| {
        let clause = merged_sql(bands);
        if clause.starts_with('(') {
            clause
        } else {
            format!("({clause})")
        }
    };
    assert_eq!(
        emitted(r#"* | where sev(level) == "error""#),
        composed(&ONE_BAND)
    );
    assert_eq!(
        emitted(r#"* | where sev(level) in ("trace", "debug", "info", "warn", "error", "fatal")"#),
        composed(&SIX_BANDS)
    );
    assert_eq!(
        emitted(r#"* | where sev(level) in ("warn", "fatal")"#),
        composed(&DISJOINT)
    );
}
