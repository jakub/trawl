// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The issue-82 cost probe: what each severity-predicate SHAPE actually
//! costs, executed against the bundled `DuckDB`.
//!
//! # The finding, and it is not the one issue #82 assumed
//!
//! Issue #82 reasoned from SQL TEXT SIZE: a `sev()` subject is over a
//! kilobyte, the pre-#82 rendering repeated it once per band, so collapsing
//! the bands into one `IN` over ladder points should be the cheaper shape.
//! The text does shrink — 5.68x for a six-band list — but the execution
//! goes the OTHER way, by a lot: `DuckDB` evaluates a repeated `BETWEEN`
//! subject far more cheaply than it evaluates the same subject under an
//! `IN` list, so the `IN` rendering is 3.6x slower for six bands and 62x
//! slower for a single band.
//!
//! The shape that is actually fast is the third one this probe times:
//! keep RANGES, but merge the ladder points into MINIMAL CONTIGUOUS ones.
//! Six contiguous bands then collapse to a single `BETWEEN 1 AND 24` — the
//! subject written once AND 5.5x faster than the pre-#82 rendering.
//!
//! Not a correctness test and not run in CI — `#[ignore]`d, because a
//! wall-clock ratio on a shared machine is evidence for a human reading a
//! PR, not a gate. Run it explicitly:
//!
//! ```sh
//! cargo test -p trawl-engine --test severity_set_bench --release -- --ignored --nocapture
//! ```
//!
//! # Why it times SHAPES rather than two checkouts
//!
//! All four predicates are built here from the ONE subject builder
//! (`conform::severity_reading_sql_bind_once`), so the pre-#82 shape (the
//! subject repeated once per band, ranges OR'd) and the post-#82 shape
//! (the subject written once, ladder points in an `IN`) are measured in
//! the SAME process, against the SAME corpus, with the same engine build.
//! Timing two git checkouts instead would compare two binaries built
//! minutes apart, which is how a 60x "regression" that is really a
//! build/cache artifact gets into a PR body.
//!
//! `emitted_shapes_match_the_hand_built_ones` is the guard that keeps this
//! honest: the post-#82 strings the emitter actually produces are asserted
//! equal to the hand-built ones this bench times.
//!
//! The corpus is 1M rows of a realistic token distribution over a VARCHAR
//! `level` column — the shape a sender that never adopted `_severity`
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

fn conn() -> Connection {
    Connection::open_in_memory().expect("in-memory duckdb")
}

/// The `sev(level)` subject every shape below compares — the expensive
/// thing, over a kilobyte of `list_transform` SQL.
fn subject() -> String {
    severity_reading_sql_bind_once(&untyped_text(r#""level""#), Dialect::Otel)
}

/// The pre-#82 rendering: one `BETWEEN` per band, OR'd, each carrying its
/// OWN copy of the subject.
fn ranges_sql(bands: &[(i64, i64)]) -> String {
    let subject = subject();
    bands
        .iter()
        .map(|(lo, hi)| format!("{subject} BETWEEN {lo} AND {hi}"))
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// The post-#82 rendering: the subject once, every ladder point the bands
/// cover inlined into one `IN`.
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

    let shapes = [
        ("one band  before (BETWEEN)", ranges_sql(&ONE_BAND)),
        ("one band  after  (IN)", points_sql(&ONE_BAND)),
        ("six bands before (OR of BETWEENs)", ranges_sql(&SIX_BANDS)),
        ("six bands after  (IN)", points_sql(&SIX_BANDS)),
        ("six bands MERGED (one BETWEEN)", ranges_sql(&[(1, 24)])),
        (
            "two disjoint before (2 BETWEEN)",
            ranges_sql(&[(13, 16), (21, 24)]),
        ),
        (
            "two disjoint after  (IN)",
            points_sql(&[(13, 16), (21, 24)]),
        ),
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
    println!(
        "  one band   speedup   {:.2}x  (SQL {:.2}x smaller)",
        ratio(&measured[0], &measured[1]),
        sql_ratio(&measured[0], &measured[1])
    );
    println!(
        "  six bands  speedup   {:.2}x  (SQL {:.2}x smaller)",
        ratio(&measured[2], &measured[3]),
        sql_ratio(&measured[2], &measured[3])
    );

    // Not a threshold assertion — the numbers are the deliverable. Only
    // the sanity of the corpus is asserted, so a probe that silently
    // measured an empty file cannot be read as a win.
    assert!(
        measured[0].rows > 0,
        "the one-band predicate must match rows"
    );
    assert_eq!(
        measured[0].rows, measured[1].rows,
        "both one-band shapes must match the same rows"
    );
    assert_eq!(
        measured[2].rows, measured[3].rows,
        "both six-band shapes must match the same rows"
    );
    assert!(
        measured[2].rows > measured[0].rows,
        "six bands must match more rows than one"
    );
}

/// The guard that makes the bench above honest: the hand-built post-#82
/// shapes it times are EXACTLY what the emitter produces for the
/// equivalent DSL.
#[test]
fn emitted_shapes_match_the_hand_built_ones() {
    let emitted = |dsl: &str| {
        let query = trawl_core::parser::parse(dsl).expect("dsl parses");
        let sql = trawl_core::emitter::emit(&query, "X")
            .expect("emit succeeds")
            .sql;
        let (_, predicate) = sql.split_once("WHERE ").expect("a where clause");
        // Strip exactly the ONE wrapping paren pair the pipeline arm adds
        // — trimming greedily would eat the `IN (…)` list's own closer.
        let predicate = predicate.trim();
        predicate
            .strip_prefix('(')
            .and_then(|p| p.strip_suffix(')'))
            .expect("the pinned arm parenthesizes its clause")
            .to_owned()
    };
    assert_eq!(
        emitted(r#"* | where sev(level) == "error""#),
        points_sql(&ONE_BAND)
    );
    assert_eq!(
        emitted(r#"* | where sev(level) in ("trace", "debug", "info", "warn", "error", "fatal")"#),
        points_sql(&SIX_BANDS)
    );
}
