// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A 32 000-term flat chain parses and drops on a tokio worker's 2 MiB
//! stack in each stage position that takes an expression.
//!
//! Only the drop is covered in every position. The complexity check and
//! validation run on the `where` query alone: on the `let` and `stats`
//! queries they recurse through `score_expr` and overflow this stack, as
//! the batch and live lanes do on all three.

use trawl_core::complexity::check_pipeline_complexity;
use trawl_core::emitter::validate_pipeline;

const TOKIO_WORKER_STACK: usize = 2 * 1024 * 1024;
const TERMS: usize = 32_000;

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

    std::thread::Builder::new()
        .stack_size(TOKIO_WORKER_STACK)
        .spawn(move || {
            for (case, dsl) in &cases {
                phase(case, "parse");
                let query = trawl_core::parser::parse(dsl).expect("parses");

                if *case == "where" {
                    phase(case, "check_pipeline_complexity");
                    assert!(check_pipeline_complexity(&query.pipeline).is_ok());

                    phase(case, "validate_pipeline");
                    assert!(validate_pipeline(&query.pipeline).is_ok());
                }

                phase(case, "drop");
                drop(query);
            }
        })
        .expect("spawn")
        .join()
        .expect("every case must run and drop on a 2 MiB stack");
}
