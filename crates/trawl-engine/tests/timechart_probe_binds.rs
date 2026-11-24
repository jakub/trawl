// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `executor::prepare_probe` counts statements handed to `DuckDB`, and the
//! counter is process-global: this file is its own binary with one test in
//! it, so no other test's binds land between its counts.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use trawl_core::schema::FieldTypes;
use trawl_engine::cancel::CancelLatch;
use trawl_engine::error::EngineError;
use trawl_engine::executor::{Executor, RowCap, prepare_probe};

fn fixture(dir: &tempfile::TempDir) -> String {
    let path = dir
        .path()
        .join("buckets.parquet")
        .to_str()
        .expect("temp path is valid UTF-8")
        .to_string();
    duckdb::Connection::open_in_memory()
        .expect("in-memory duckdb")
        .execute_batch(&format!(
            "COPY (SELECT TIMESTAMP '2026-01-01 00:05:00' AS good) \
             TO '{path}' (FORMAT PARQUET)"
        ))
        .expect("fixture writes");
    path
}

#[test]
fn cancelled_during_probe_never_binds_main() {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = fixture(&dir);
    let exec = Executor::new().expect("executor");
    let dsl = "* | timechart on good span=1d count()";

    let flag = Arc::new(AtomicBool::new(false));
    let latch = CancelLatch::new(Arc::clone(&flag));

    let before = prepare_probe::count();
    exec.cancellable(&latch)
        .run_query(dsl, &source, &FieldTypes::new(), RowCap::Refuse(usize::MAX))
        .expect("a TIMESTAMP bucket source runs");
    assert!(
        prepare_probe::count() >= before + 2,
        "one bind for the probe, one for the statement"
    );

    flag.store(true, Ordering::SeqCst);
    let before = prepare_probe::count();
    let outcome = exec.cancellable(&latch).run_query(
        dsl,
        &source,
        &FieldTypes::new(),
        RowCap::Refuse(usize::MAX),
    );
    assert!(
        matches!(outcome, Err(EngineError::Cancelled)),
        "expected a cancellation, got {outcome:?}"
    );
    assert_eq!(
        prepare_probe::count(),
        before,
        "a latched lane hands DuckDB nothing to bind, probes included"
    );
}
