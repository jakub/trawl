// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Over-budget DSL never reaches `DuckDB` (ADR-0024).
//!
//! `trawl-core` proves that the bind-time expansion budget refuses the
//! right queries; the parity tests prove both validation doors say the
//! same thing. What is left is the claim the whole defence rests on: a
//! refusal happens in the emitter, so the binder the budget exists to
//! protect is never asked to bind anything.
//!
//! "Never asked" is a negative, so it is counted rather than argued.
//! `executor::prepare_probe` counts every statement the query and export
//! lanes hand `DuckDB`, and each case here asserts the count did not move
//! across the call. The counter is process-global, so every test in this
//! file takes one lock: the positive control deliberately binds, and
//! under the plain `cargo test` harness — where these run as threads of
//! one process rather than nextest's separate processes — a concurrent
//! refusal case must not see its increment.

mod common;

use std::sync::{Mutex, MutexGuard, PoisonError};

use trawl_core::complexity::MAX_PIPELINE_STAGES;
use trawl_core::schema::FieldTypes;
use trawl_engine::error::EngineError;
use trawl_engine::executor::{Executor, prepare_probe};

/// Serializes this file's tests against the shared bind counter.
static COUNTER: Mutex<()> = Mutex::new(());

fn counter_lock() -> MutexGuard<'static, ()> {
    COUNTER.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The two shapes the budget refuses, one per limit.
///
/// The severity chain is ADR-0024's worked example: each link reads the
/// previous one through a severity set, which the emitter writes as
/// twelve copies of its subject, so eight links multiply the seed
/// expression far past 512 while the text stays a couple of hundred
/// bytes. The stage case is the other limit, one over 128.
fn refused_queries() -> Vec<(&'static str, String)> {
    use std::fmt::Write as _;
    let points = (0..12).map(|i| (i * 2 + 1).to_string()).collect::<Vec<_>>();
    let mut chain = String::from("* | let a0 = _severity + _severity");
    for i in 1..=8 {
        let _ = write!(chain, ", a{i} = sev(a{}) in ({})", i - 1, points.join(","));
    }
    vec![
        ("severity chain", chain),
        (
            "129 stages",
            format!("*{}", " | head 1".repeat(MAX_PIPELINE_STAGES + 1)),
        ),
    ]
}

/// A refusal must be the emitter's, and it must be free.
fn assert_refused_without_binding(case: &str, before: u64, outcome: &Result<(), EngineError>) {
    let error = outcome.as_ref().expect_err(case);
    assert!(
        matches!(error, EngineError::Emit(_)),
        "{case}: expected an emit-class refusal, got {error:?}"
    );
    assert_eq!(
        prepare_probe::count(),
        before,
        "{case}: an over-budget query reached DuckDB's binder"
    );
}

/// A source that reaches no parquet at all, for the hot-only lane.
fn cold_start_source(dir: &tempfile::TempDir) -> String {
    format!("{}/**/*.parquet", dir.path().display())
}

#[test]
fn the_batch_lane_refuses_over_budget_dsl_without_binding() {
    let _lock = counter_lock();
    let exec = Executor::new().expect("executor");
    let glob = common::fixture_glob();
    for (case, dsl) in refused_queries() {
        let before = prepare_probe::count();
        let outcome = exec
            .run_query(&dsl, &glob, &FieldTypes::new(), 100, 0)
            .map(|_| ());
        assert_refused_without_binding(case, before, &outcome);
    }
}

#[test]
fn the_hot_lanes_refuse_over_budget_dsl_without_binding() {
    let _lock = counter_lock();
    let exec = Executor::new().expect("executor");
    let glob = common::fixture_glob();
    let hot = common::fixture_glob_json();
    let empty = tempfile::tempdir().expect("temp dir");
    // Both hot shapes: the union over a corpus that exists, and the
    // cold-start fallback that would otherwise re-emit hot-only.
    for source in [glob, cold_start_source(&empty)] {
        for (case, dsl) in refused_queries() {
            let before = prepare_probe::count();
            let outcome = exec
                .run_query_with_hot(
                    &dsl,
                    &source,
                    &hot,
                    &FieldTypes::new(),
                    &FieldTypes::new(),
                    100,
                    0,
                )
                .map(|_| ());
            assert_refused_without_binding(case, before, &outcome);
        }
    }
}

#[test]
fn the_export_lanes_refuse_over_budget_dsl_without_binding() {
    let _lock = counter_lock();
    let exec = Executor::new().expect("executor");
    let glob = common::fixture_glob();
    let hot = common::fixture_glob_json();
    let out = tempfile::tempdir().expect("temp dir");
    let path = out.path().join("export.parquet");
    for (case, dsl) in refused_queries() {
        let before = prepare_probe::count();
        let cold = exec.export_parquet(&dsl, &glob, &FieldTypes::new(), &path, 100);
        assert_refused_without_binding(case, before, &cold);

        let before = prepare_probe::count();
        let union = exec.export_parquet_with_hot(
            &dsl,
            &glob,
            &hot,
            &FieldTypes::new(),
            &FieldTypes::new(),
            &path,
            100,
        );
        assert_refused_without_binding(case, before, &union);
    }
    assert!(
        !path.exists(),
        "a refused export must not leave a file behind"
    );
}

/// The counter is only evidence if it moves when a bind happens.
///
/// Without this, wiring it to nothing would make every case above pass.
#[test]
fn the_bind_counter_counts_an_admitted_query() {
    let _lock = counter_lock();
    let exec = Executor::new().expect("executor");
    let glob = common::fixture_glob();
    let before = prepare_probe::count();
    exec.run_query("* | head 1", &glob, &FieldTypes::new(), 100, 0)
        .expect("an in-budget query runs");
    assert!(
        prepare_probe::count() > before,
        "an admitted query binds at least one statement"
    );
}
