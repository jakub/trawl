// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! One admission contract, both validation doors (ADR-0024).

use trawl_core::ast::{PipeStage, Spanned};
use trawl_core::complexity::{MAX_LATERAL_EXPANSION, MAX_PIPELINE_STAGES, MAX_STAGE_FIELDS};
use trawl_core::context::EvalContext;
use trawl_core::parser;
use trawl_core::pin_scope::PinScope;
use trawl_core::schema::{CanonicalType, FieldTypes};

fn anchor() -> EvalContext {
    EvalContext::at(
        chrono::DateTime::parse_from_rfc3339("2026-08-24T12:00:00Z")
            .expect("literal is RFC 3339")
            .with_timezone(&chrono::Utc),
    )
}

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

fn sql_sentence(dsl: &str, pins: &FieldTypes) -> String {
    let query = parser::parse(dsl).expect("dsl parses");
    let empty = FieldTypes::new();
    let text = trawl_core::emitter::emit_with_pins(
        &query,
        "src",
        pins,
        &trawl_core::emitter::InputColumns::unprobed(),
        anchor(),
    )
    .expect_err("the SQL lane must refuse")
    .to_string();
    let stripped = text
        .strip_prefix("unsupported operation: ")
        .unwrap_or_else(|| panic!("{dsl}: the emitter's own prefix is missing from {text:?}"))
        .to_string();

    for other in [
        trawl_core::emitter::emit(&query, "src", anchor()),
        trawl_core::emitter::emit_with_pins(
            &query,
            "src",
            &empty,
            &trawl_core::emitter::InputColumns::unprobed(),
            anchor(),
        ),
        trawl_core::emitter::emit_with_hot_source(
            &query,
            "src",
            "hot",
            &empty,
            pins,
            &trawl_core::emitter::InputColumns::unprobed(),
            anchor(),
        ),
        trawl_core::emitter::emit_hot_only(
            &query,
            "hot",
            &empty,
            pins,
            &trawl_core::emitter::InputColumns::unprobed(),
            anchor(),
        ),
        trawl_core::emitter::emit_hot_only(
            &query,
            "hot",
            &empty,
            &empty,
            &trawl_core::emitter::InputColumns::unprobed(),
            anchor(),
        ),
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

fn stream_sentence(dsl: &str, pins: &FieldTypes) -> String {
    let stages = pipeline(dsl);
    let text = trawl_core::stream::compile_stream_plan(&stages, &PinScope::root(pins), None)
        .expect_err("the stream lane must refuse")
        .to_string();
    let blind = trawl_core::stream::compile_stream_plan(&stages, &PinScope::unpinned(), None)
        .expect_err("the stream lane must refuse pin-blind too")
        .to_string();
    assert_eq!(text, blind, "{dsl}: the stream lane read the catalog");
    text
}

fn over_budget_let(n: usize) -> String {
    let args = vec!["x"; n].join(", ");
    format!("* | let x = abs(y), z = coalesce({args})")
}

fn matrix() -> Vec<String> {
    vec![
        over_budget_let(513),
        over_budget_let(513).replace("| let", "| eval"),
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
        format!(
            "* | extract kv | {}",
            over_budget_let(513).replace("* | ", "")
        ),
        format!(
            "* | head 5 | extract kv | sort dur | {}",
            over_budget_let(513).replace("* | ", "")
        ),
        format!("*{}", " | head 1".repeat(MAX_PIPELINE_STAGES + 1)),
        format!(
            "| from saved daily{}",
            " | head 1".repeat(MAX_PIPELINE_STAGES)
        ),
        format!(
            "* | let x = abs(y), z = coalesce({}) | stats count() by count",
            vec!["x"; 513].join(", ")
        ),
        format!("* | sort {}", wide_field_list()),
        format!("* | extract kv | dedup {}", wide_field_list()),
    ]
}

fn wide_field_list() -> String {
    (0..=MAX_STAGE_FIELDS)
        .map(|i| format!("f{i}"))
        .collect::<Vec<_>>()
        .join(", ")
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
                || sql.contains(&MAX_PIPELINE_STAGES.to_string())
                || sql.contains(&format!("limit of {MAX_STAGE_FIELDS} ")),
            "{dsl}: {sql}"
        );
    }
}

#[test]
fn the_same_pipelines_are_admitted_one_reference_short() {
    let pins = populated_pins();
    let query = parser::parse(&over_budget_let(512)).expect("dsl parses");
    trawl_core::emitter::emit_with_pins(
        &query,
        "src",
        &pins,
        &trawl_core::emitter::InputColumns::unprobed(),
        anchor(),
    )
    .expect("512 references are admitted by the SQL lane");

    let stages = pipeline(&over_budget_let(512));
    trawl_core::stream::compile_stream_plan(&stages, &PinScope::root(&pins), None)
        .expect("512 references are admitted by the stream lane");

    let long = format!("*{}", " | head 1".repeat(MAX_PIPELINE_STAGES));
    let query = parser::parse(&long).expect("dsl parses");
    trawl_core::emitter::emit_with_pins(
        &query,
        "src",
        &pins,
        &trawl_core::emitter::InputColumns::unprobed(),
        anchor(),
    )
    .expect("128 stages are admitted");
    trawl_core::stream::compile_stream_plan(&pipeline(&long), &PinScope::unpinned(), None)
        .expect("128 stages are admitted");
}

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

    // A BARE alias of one reading, not a reading inside a severity set: `sev()`
    // alone spends most of the 512 budget, so anything multiplying it is refused.
    // This case proves the CATALOG never moves the line, not where the line is.
    let dsl = "* | let s = sev(level), t = s";
    for scope in [PinScope::unpinned(), PinScope::root(&pins)] {
        trawl_core::stream::compile_stream_plan(&pipeline(dsl), &scope, None)
            .expect("an in-budget severity chain streams under any catalog");
    }
    let query = parser::parse(dsl).expect("parses");
    for map in [&empty, &pins] {
        trawl_core::emitter::emit_with_pins(
            &query,
            "src",
            map,
            &trawl_core::emitter::InputColumns::unprobed(),
            anchor(),
        )
        .expect("an in-budget severity chain emits under any catalog");
    }
}
