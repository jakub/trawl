// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A flat chain as long as `MAX_QUERY_LEN` admits runs the whole query
//! path on a tokio worker's 2 MiB stack: parse, the complexity check,
//! validation, the batch lane over a real Parquet file, the live lane
//! over one event, and the drop of every tree (#405).

use trawl_core::complexity::check_pipeline_complexity;
use trawl_core::context::EvalContext;
use trawl_core::emitter::validate_pipeline;
use trawl_core::pin_scope::PinScope;
use trawl_core::row;
use trawl_core::schema::FieldTypes;
use trawl_core::stream::{StreamPlan, apply_stage, compile_stream_plan};
use trawl_engine::executor::Executor;

const TOKIO_WORKER_STACK: usize = 2 * 1024 * 1024;
const TERMS: usize = 32_000;

/// A one-file parquet source `DuckDB` writes from `select`.
fn parquet(select: &str) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.parquet");
    duckdb::Connection::open_in_memory()
        .unwrap()
        .execute_batch(&format!(
            "COPY ({select}) TO '{}' (FORMAT PARQUET)",
            path.display()
        ))
        .unwrap();
    let source = path.display().to_string();
    (dir, source)
}

/// Names the phase about to run, so a stack overflow, which aborts the
/// process with no unwinding, still says where it died.
fn phase(case: &str, phase: &str) {
    eprintln!("case={case} phase={phase}");
}

#[test]
fn flat_chains_run_on_a_worker_stack() {
    let chain = vec!["1"; TERMS].join("+");
    let cases = [
        ("where", format!("* | where {chain} > 0")),
        ("let", format!("* | let x = {chain}")),
        ("stats", format!("* | stats sum({chain})")),
    ];
    let (_dir, source) = parquet("SELECT 'x' AS message");

    std::thread::Builder::new()
        .stack_size(TOKIO_WORKER_STACK)
        .spawn(move || {
            for (case, dsl) in &cases {
                phase(case, "parse");
                let query = trawl_core::parser::parse(dsl).expect("parses");

                phase(case, "check_pipeline_complexity");
                let complexity = check_pipeline_complexity(&query.pipeline);
                eprintln!("case={case} complexity={:?}", complexity.is_ok());

                phase(case, "validate_pipeline");
                let validated = validate_pipeline(&query.pipeline);
                eprintln!("case={case} validate={:?}", validated.is_ok());

                phase(case, "run_query");
                let result = Executor::new().expect("executor").run_query(
                    dsl,
                    &source,
                    &FieldTypes::new(),
                    usize::MAX,
                );
                eprintln!(
                    "case={case} run_query={:?}",
                    result
                        .as_ref()
                        .map(|r| r.rows.len())
                        .map_err(ToString::to_string)
                );

                phase(case, "compile_stream_plan");
                let plan = compile_stream_plan(&query.pipeline, &PinScope::unpinned(), None);
                eprintln!(
                    "case={case} compile_stream_plan={:?}",
                    plan.as_ref().map(|_| ()).map_err(ToString::to_string)
                );

                phase(case, "evaluate");
                let anchor = EvalContext::capture();
                let mut event = row::from_json(
                    serde_json::json!({"message": "x"})
                        .as_object()
                        .expect("object"),
                );
                let mut plan = plan;
                match &mut plan {
                    Ok(StreamPlan::PassThrough(stages)) => {
                        for stage in stages.iter_mut() {
                            let verdict = apply_stage(stage, &mut event, &anchor);
                            eprintln!("case={case} stage={verdict:?}");
                        }
                    }
                    Ok(StreamPlan::Aggregate {
                        pre_stages,
                        aggregation,
                        post_stages: _,
                    }) => {
                        for stage in pre_stages.iter_mut() {
                            let verdict = apply_stage(stage, &mut event, &anchor);
                            eprintln!("case={case} stage={verdict:?}");
                        }
                        aggregation.feed_event(&event, &anchor);
                        eprintln!("case={case} groups={}", aggregation.snapshot().1.len());
                    }
                    Err(_) => {}
                }

                phase(case, "drop");
                drop(plan);
                drop(result);
                drop(event);
                drop(query);
            }
        })
        .expect("spawn")
        .join()
        .expect("every case must run and drop on a 2 MiB stack");
}
