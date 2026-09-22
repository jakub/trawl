// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! One admission contract, both validation doors (ADR-0024).
//!
//! The SQL lane refuses through `emitter::validate_pipeline`, which every
//! emit entrypoint funnels into; the streaming lane refuses through
//! `stream::compile_stream_plan`. They must carry the SAME sentence for
//! the same DSL text, modulo the emitter's own `unsupported operation: `
//! prefix — otherwise a query that a live tail rejects for its expansion
//! would be explained one way in the browser and another in the CLI.
//!
//! The check runs BEFORE each lane's own semantic refusals, so an
//! over-budget pipeline gets the expansion sentence even where the stage
//! is unstreamable, or where a later stage is a projection collision.

use trawl_core::ast::{PipeStage, Spanned};
use trawl_core::complexity::{MAX_LATERAL_EXPANSION, MAX_PIPELINE_STAGES};
use trawl_core::context::EvalContext;
use trawl_core::parser;
use trawl_core::pin_scope::PinScope;
use trawl_core::schema::{CanonicalType, FieldTypes};

/// A FIXED anchor: nothing may self-serve an evaluation context
/// (`tests/now_anchor_contract.rs`).
fn anchor() -> EvalContext {
    EvalContext::at(
        chrono::DateTime::parse_from_rfc3339("2026-08-24T12:00:00Z")
            .expect("literal is RFC 3339")
            .with_timezone(&chrono::Utc),
    )
}

/// A populated catalog snapshot: one pin of every canonical type, over the
/// names the fixtures below use.
fn populated_pins() -> FieldTypes {
    let mut pins = FieldTypes::new();
    pins.insert("status", CanonicalType::Varchar);
    pins.insert("dur", CanonicalType::BigInt);
    pins.insert("ratio", CanonicalType::Double);
    pins.insert("flag", CanonicalType::Boolean);
    pins.insert("seen", CanonicalType::Timestamp);
    pins.insert("_severity", CanonicalType::Severity);
    pins.insert("level", CanonicalType::Severity);
    pins.insert("y", CanonicalType::BigInt);
    pins.insert("a", CanonicalType::Varchar);
    pins.insert("x", CanonicalType::Varchar);
    pins
}

fn pipeline(dsl: &str) -> Vec<Spanned<PipeStage>> {
    parser::parse(dsl).expect("dsl parses").pipeline
}

/// The refusal the SQL lane carries, with the emitter's prefix stripped.
fn sql_sentence(dsl: &str, pins: &FieldTypes) -> String {
    let query = parser::parse(dsl).expect("dsl parses");
    let empty = FieldTypes::new();
    let text = trawl_core::emitter::emit_with_pins(&query, "src", pins, anchor())
        .expect_err("the SQL lane must refuse")
        .to_string();
    let stripped = text
        .strip_prefix("unsupported operation: ")
        .unwrap_or_else(|| panic!("{dsl}: the emitter's own prefix is missing from {text:?}"))
        .to_string();

    // Every emit entrypoint funnels into `validate_pipeline`, so all four
    // refuse identically, under an empty pin map and a populated one
    // alike — the check is corpus-independent by construction.
    for other in [
        trawl_core::emitter::emit(&query, "src", anchor()),
        trawl_core::emitter::emit_with_pins(&query, "src", &empty, anchor()),
        trawl_core::emitter::emit_with_hot_source(&query, "src", "hot", &empty, pins, anchor()),
        trawl_core::emitter::emit_hot_only(&query, "hot", &empty, pins, anchor()),
        trawl_core::emitter::emit_hot_only(&query, "hot", &empty, &empty, anchor()),
    ] {
        assert_eq!(
            other
                .expect_err("every emit entrypoint must refuse")
                .to_string(),
            text,
            "{dsl}: one entrypoint disagreed"
        );
    }
    stripped
}

/// The refusal the streaming lane carries.
fn stream_sentence(dsl: &str, pins: &FieldTypes) -> String {
    let stages = pipeline(dsl);
    let text = trawl_core::stream::compile_stream_plan(&stages, &PinScope::root(pins), None)
        .expect_err("the stream lane must refuse")
        .to_string();
    // Pin-blindness must not change the answer either.
    let blind = trawl_core::stream::compile_stream_plan(&stages, &PinScope::unpinned(), None)
        .expect_err("the stream lane must refuse pin-blind too")
        .to_string();
    assert_eq!(text, blind, "{dsl}: the stream lane read the catalog");
    text
}

/// A same-stage chain over `n` references — over budget at 513.
fn over_budget_let(n: usize) -> String {
    let args = vec!["x"; n].join(", ");
    format!("* | let x = abs(y), z = coalesce({args})")
}

/// The matrix: every fixture must be refused by BOTH doors with one
/// sentence.
fn matrix() -> Vec<String> {
    vec![
        // let, and its `eval` spelling.
        over_budget_let(513),
        over_budget_let(513).replace("| let", "| eval"),
        // A stage that is unstreamable on its own: the expansion sentence
        // still wins, because the budget is checked first.
        format!(
            "* | stats abs(y) as k, coalesce({}) as z",
            vec!["k"; 513].join(", ")
        ),
        format!(
            "* | timechart span=1h abs(y) as k, coalesce({}) as z",
            vec!["k"; 513].join(", ")
        ),
        format!(
            "* | eventstats abs(y) as k, coalesce({}) as z",
            vec!["k"; 513].join(", ")
        ),
        // The tail AFTER `extract kv` is part of the same pipeline: the
        // executor peels those stages off to run in Rust, and both doors
        // still have to see them.
        format!(
            "* | extract kv | {}",
            over_budget_let(513).replace("* | ", "")
        ),
        format!(
            "* | head 5 | extract kv | sort dur | {}",
            over_budget_let(513).replace("* | ", "")
        ),
        // The stage cap.
        format!("*{}", " | head 1".repeat(MAX_PIPELINE_STAGES + 1)),
        // `from saved` counts, before source resolution removes it.
        format!(
            "| from saved daily{}",
            " | head 1".repeat(MAX_PIPELINE_STAGES)
        ),
        // A pipeline that ALSO has a projection collision: the expansion
        // refusal comes first, in both lanes.
        format!(
            "* | let x = abs(y), z = coalesce({}) | stats count() by count",
            vec!["x"; 513].join(", ")
        ),
    ]
}

#[test]
fn both_doors_refuse_with_one_sentence() {
    let pins = populated_pins();
    for dsl in matrix() {
        let sql = sql_sentence(&dsl, &pins);
        let stream = stream_sentence(&dsl, &pins);
        assert_eq!(sql, stream, "{dsl}: the two doors disagreed");
        assert!(
            sql.contains(&MAX_LATERAL_EXPANSION.to_string())
                || sql.contains(&MAX_PIPELINE_STAGES.to_string()),
            "{dsl}: {sql}"
        );
    }
}

/// The same pipelines one reference short of the budget are admitted by
/// both doors — the refusal is the budget, never the shape.
#[test]
fn the_same_pipelines_are_admitted_one_reference_short() {
    let pins = populated_pins();
    let query = parser::parse(&over_budget_let(512)).expect("dsl parses");
    trawl_core::emitter::emit_with_pins(&query, "src", &pins, anchor())
        .expect("512 references are admitted by the SQL lane");

    let stages = pipeline(&over_budget_let(512));
    // The stream lane's own refusal for this pipeline is about the STAGE,
    // never the budget: `let` streams, so this one compiles outright.
    trawl_core::stream::compile_stream_plan(&stages, &PinScope::root(&pins), None)
        .expect("512 references are admitted by the stream lane");

    let long = format!("*{}", " | head 1".repeat(MAX_PIPELINE_STAGES));
    let query = parser::parse(&long).expect("dsl parses");
    trawl_core::emitter::emit_with_pins(&query, "src", &pins, anchor())
        .expect("128 stages are admitted");
    trawl_core::stream::compile_stream_plan(&pipeline(&long), &PinScope::unpinned(), None)
        .expect("128 stages are admitted");
}

/// Admission is not a promise that every admitted stage runs in every
/// lane: an under-budget `pivot` is still refused by the stream compiler,
/// with its own unsupported-stage sentence.
#[test]
fn admission_does_not_make_a_stage_streamable() {
    let dsl = "* | pivot count() on status by host";
    trawl_core::emitter::emit(&parser::parse(dsl).expect("parses"), "src", anchor())
        .expect("the SQL lane runs pivot");
    let err = trawl_core::stream::compile_stream_plan(&pipeline(dsl), &PinScope::unpinned(), None)
        .expect_err("pivot does not stream");
    assert!(
        matches!(
            err,
            trawl_core::stream::StreamPlanError::UnsupportedStage { .. }
        ),
        "{err}"
    );
}

/// The refusal is corpus-independent: the same DSL text answers the same
/// way under an empty catalog and a populated one, at every door. A check
/// that read pins would make a saved query admitted when it was written
/// and refused after a repin.
#[test]
fn the_verdict_never_depends_on_the_catalog() {
    let empty = FieldTypes::new();
    let pins = populated_pins();
    for dsl in matrix() {
        let stages = pipeline(&dsl);
        let a = trawl_core::stream::compile_stream_plan(&stages, &PinScope::root(&empty), None)
            .expect_err("refused")
            .to_string();
        let b = trawl_core::stream::compile_stream_plan(&stages, &PinScope::root(&pins), None)
            .expect_err("refused")
            .to_string();
        assert_eq!(a, b, "{dsl}");
    }

    // And the admitted side: a pinned severity chain that fits stays
    // fitting whatever the catalog says.
    //
    // It is a BARE alias of one reading, not a reading inside a severity
    // set: since the simple-`CASE` repricing, `sev()` alone weighs ~414
    // nodes, so `t = s` spends most of the 512 budget on its own and
    // anything multiplying it (a twelve-run set, a token-text pattern) is
    // refused. That refusal is the point of the repricing, and this case
    // exists to prove the CATALOG never moves the line — not to prove
    // where the line is.
    let dsl = "* | let s = sev(level), t = s";
    for scope in [PinScope::unpinned(), PinScope::root(&pins)] {
        trawl_core::stream::compile_stream_plan(&pipeline(dsl), &scope, None)
            .expect("an in-budget severity chain streams under any catalog");
    }
    let query = parser::parse(dsl).expect("parses");
    for map in [&empty, &pins] {
        trawl_core::emitter::emit_with_pins(&query, "src", map, anchor())
            .expect("an in-budget severity chain emits under any catalog");
    }
}
