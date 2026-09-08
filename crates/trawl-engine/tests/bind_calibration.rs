// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What the queries the expansion budget ADMITS actually cost `DuckDB` to
//! prepare (ADR-0024, #150).
//!
//! `MAX_LATERAL_EXPANSION = 512` is a number picked from structure, not
//! from a stopwatch, so the claim it needs is narrow and checkable: on the
//! calibration host, a query the checker admits prepares quickly enough
//! that a wedged binder is not the failure an operator meets. Each case
//! below sits at or near the budget for one family of shapes, warms the
//! connection once and then measures five prepares. EVERY sample must
//! finish under a second, not the average, because the failure this whole
//! budget exists to prevent is one bad prepare, not a bad mean.
//!
//! Not a CI gate. These are `#[ignore]`d wall-clock measurements, and a
//! shared runner would turn them into a flaky test rather than evidence.
//! The committed run lives in `visual-evidence/issue-150/bind-calibration.md`
//! and is reproduced by the wrapper that owns the watchdog:
//!
//! ```sh
//! scripts/bind-calibration
//! ```
//!
//! The wrapper runs each case in its own process under `timeout 5`, so a
//! case that does wedge is killed from outside rather than measured from
//! inside. Cases are ordered cheapest-first within a family so a cutoff
//! stops that family with the cheaper members already recorded.
//!
//! Nothing here prepares a query the checker refuses. The refused shapes
//! are proved refused in `trawl-core`'s own tests and in
//! `duckdb_probe.rs`; the eight-link severity chain is never emitted at
//! all.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

/// The ceiling every measured sample must clear.
const SAMPLE_BUDGET: Duration = Duration::from_secs(1);

/// Discarded warm-up prepares, then measured ones.
const WARMUPS: usize = 1;
const SAMPLES: usize = 5;

fn conn() -> duckdb::Connection {
    let conn = duckdb::Connection::open_in_memory().unwrap();
    conn.execute_batch(trawl_core::conform::SESSION_TIME_ZONE_SQL)
        .unwrap();
    conn
}

/// A small parquet with a declared schema, written by this harness. The
/// bind-time cost measured here is a property of the SQL: the fixture is
/// two rows so that `prepare` has a real schema to bind against and
/// nothing else.
fn fixture(conn: &duckdb::Connection, dir: &std::path::Path) -> String {
    let file = dir.join("bind.parquet");
    conn.execute_batch(&format!(
        "COPY (SELECT * FROM (VALUES \
         (TIMESTAMP '2026-01-01 00:00:00', 'nginx', 'h1', 200, 17, 'boom'), \
         (TIMESTAMP '2026-01-01 00:01:00', 'nginx', 'h2', 500, 9, 'fine')) \
         AS t(_time, service, host, status, _severity, message)) TO '{}' (FORMAT PARQUET)",
        file.display()
    ))
    .unwrap();
    file.display().to_string()
}

/// Admit, emit, warm, measure, and print one row of the evidence table.
///
/// The admission check is part of the measurement, not a precondition
/// checked elsewhere: a case that stopped being admitted would otherwise
/// quietly calibrate a query the server refuses.
fn calibrate(case: &str, dsl: &str) {
    let dir = tempfile::tempdir().unwrap();
    let conn = conn();
    let source = fixture(&conn, dir.path());
    let version: String = conn
        .query_row("SELECT version()", [], |row| row.get(0))
        .unwrap();

    let query = trawl_core::parser::parse(dsl).unwrap_or_else(|e| panic!("{case} parses: {e:?}"));
    let (verdict, stats) =
        trawl_core::complexity::check_pipeline_complexity_with_stats(&query.pipeline);
    assert!(
        verdict.is_ok(),
        "{case}: a calibration case must be ADMITTED, this one scores {} \
         against the budget of {}",
        stats.lateral_delta,
        trawl_core::complexity::MAX_LATERAL_EXPANSION
    );
    let sql =
        trawl_core::emitter::emit(&query, &source, trawl_core::context::EvalContext::capture())
            .unwrap_or_else(|e| panic!("{case} emits: {e:?}"))
            .sql;

    for _ in 0..WARMUPS {
        conn.prepare(&sql)
            .unwrap_or_else(|e| panic!("{case} prepares: {e}"));
    }
    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let started = Instant::now();
        let prepared = conn.prepare(&sql);
        let elapsed = started.elapsed();
        prepared.unwrap_or_else(|e| panic!("{case} prepares: {e}"));
        samples.push(elapsed);
    }

    let mut cells = String::new();
    for sample in &samples {
        let _ = write!(cells, " {:.1} |", sample.as_secs_f64() * 1000.0);
    }
    let worst = samples.iter().max().copied().unwrap();
    println!("dsl: {case}: {dsl}");
    println!(
        "row: | {case} | {} | {} | {} |{cells} {:.1} |",
        stats.lateral_delta,
        stats.stages,
        sql.len(),
        worst.as_secs_f64() * 1000.0
    );
    println!("engine: {version}");
    assert!(
        worst < SAMPLE_BUDGET,
        "{case}: every sample must finish under {SAMPLE_BUDGET:?}, the slowest took {worst:?}"
    );
}

/// `a{i} = a{i-1} + a{i-1}`: the shape that doubles the substituted tree
/// with each link. Six links is the deepest the budget admits.
#[test]
#[ignore = "wall-clock calibration; run through scripts/bind-calibration"]
fn addition_chain_doubling_depth_six() {
    let mut dsl = String::from("* | let a0 = status + status");
    for i in 1..=6 {
        let _ = write!(dsl, ", a{i} = a{} + a{}", i - 1, i - 1);
    }
    calibrate("addition-doubling-6", &dsl);
}

/// One reference per link instead of two. The tree still grows, just
/// linearly in the previous weight, and twenty-two links is the deepest
/// the budget admits.
#[test]
#[ignore = "wall-clock calibration; run through scripts/bind-calibration"]
fn addition_chain_single_reference_depth_twenty_two() {
    let mut dsl = String::from("* | let a0 = status + 1");
    for i in 1..=22 {
        let _ = write!(dsl, ", a{i} = a{} + 1", i - 1);
    }
    calibrate("addition-linear-22", &dsl);
}

/// The same chain built out of `stats` scalar outputs, which land in the
/// aggregate's own SELECT list and substitute exactly as `let` does.
#[test]
#[ignore = "wall-clock calibration; run through scripts/bind-calibration"]
fn scalar_stats_chain_depth_thirty_one() {
    let mut dsl = String::from("* | stats count() as n0");
    for i in 1..=31 {
        let _ = write!(dsl, ", abs(n{}) as n{i}", i - 1);
    }
    calibrate("stats-scalar-31", &dsl);
}

/// `timechart`'s scalar outputs, the same chain again beside a generated
/// time-bucket expression.
#[test]
#[ignore = "wall-clock calibration; run through scripts/bind-calibration"]
fn scalar_timechart_chain_depth_thirty_one() {
    let mut dsl = String::from("* | timechart span=1h count() as n0");
    for i in 1..=31 {
        let _ = write!(dsl, ", abs(n{}) as n{i}", i - 1);
    }
    calibrate("timechart-scalar-31", &dsl);
}

/// One severity set over the whole ladder: twelve subject copies, no
/// alias chain. The cheapest score in this file and the slowest prepare,
/// which is the point. The budget bounds substitution, and a single
/// rendering's own size is bounded by nothing here.
#[test]
#[ignore = "wall-clock calibration; run through scripts/bind-calibration"]
fn severity_full_ladder_set_one_link() {
    calibrate(
        "severity-set-12-points-1-link",
        "* | let a0 = _severity + _severity, \
         a1 = sev(a0) in (1,3,5,7,9,11,13,15,17,19,21,23)",
    );
}

/// A severity chain that IS admitted: a single-point set copies its
/// subject once per link, so two links stay inside the budget where the
/// twelve-point set does not survive one.
#[test]
#[ignore = "wall-clock calibration; run through scripts/bind-calibration"]
fn severity_single_point_chain_two_links() {
    calibrate(
        "severity-set-1-point-2-links",
        "* | let a0 = _severity + _severity, a1 = sev(a0) in (1), a2 = sev(a1) in (1)",
    );
}

/// The ordered form of the same chain: `sev(x) >= "error"` renders one
/// range, so two links are admitted here too.
#[test]
#[ignore = "wall-clock calibration; run through scripts/bind-calibration"]
fn severity_ordered_chain_two_links() {
    calibrate(
        "severity-ordered-2-links",
        "* | let a0 = _severity + _severity, a1 = sev(a0) >= \"error\", \
         a2 = sev(a1) >= \"error\"",
    );
}

/// Ordinary reuse, at the cap exactly: two hundred and fifty-six outputs
/// each naming one earlier output once scores 512 on the nose. This is
/// the everyday shape the budget must not punish, and the case that says
/// where "everyday" stops.
#[test]
#[ignore = "wall-clock calibration; run through scripts/bind-calibration"]
fn ordinary_alias_reuse_at_the_cap() {
    let mut dsl = String::from("* | let base = status + 1");
    for i in 1..=256 {
        let _ = write!(dsl, ", x{i} = base + {i}");
    }
    calibrate("alias-reuse-256", &dsl);
}

/// The remedy the refusal recommends, at the depth that provoked it: the
/// twenty-four doublings no single stage can carry, one per stage.
#[test]
#[ignore = "wall-clock calibration; run through scripts/bind-calibration"]
fn split_let_remedy_depth_twenty_four() {
    let mut dsl = String::from("* | let a0 = status + status");
    for i in 1..24 {
        let _ = write!(dsl, " | let a{i} = a{} + a{}", i - 1, i - 1);
    }
    calibrate("split-let-24", &dsl);
}

/// The other cap, at its boundary: 128 stages, no lateral expansion at
/// all, which is 128 nested CTEs for `DuckDB` to bind.
#[test]
#[ignore = "wall-clock calibration; run through scripts/bind-calibration"]
fn stage_cap_one_hundred_twenty_eight() {
    let mut dsl = String::from("*");
    for i in 0..128 {
        let _ = write!(dsl, " | let s{i} = status + {i}");
    }
    calibrate("stages-128", &dsl);
}
