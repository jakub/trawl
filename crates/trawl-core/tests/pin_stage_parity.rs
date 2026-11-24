// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The pin-aware pipeline coverage matrix (ADR-0001 discipline): every
//! [`CompareForm`] and [`PatternForm`] variant must be exercised in both
//! pipeline lanes (the `| where` SQL the emitter renders, executed
//! against `DuckDB`, and the pin-aware streaming evaluator) by at least
//! one matrix cell, and the two lanes must agree on every cell.

use std::collections::BTreeSet;
use std::io::Write as _;

use duckdb::Connection;
use serde_json::{Map, Value};

use trawl_core::ast::{FloatLiteral, LiteralValue, PipeStage};
use trawl_core::compare::{self, CompareForm, PatternForm};
use trawl_core::emitter::{self, SqlValue};
use trawl_core::eval::{EvalValue, PatternTable};
use trawl_core::parser;
use trawl_core::pattern::Preparer;
use trawl_core::pin_scope::PinScope;
use trawl_core::schema::{CanonicalType, FieldTypes};
use trawl_core::stream::{StageResult, StreamPlan, compile_stream_plan};

fn apply_stage(
    stage: &mut trawl_core::stream::CompiledStage,
    event: &mut trawl_core::row::Row,
    ctx: &trawl_core::context::EvalContext,
) -> StageResult {
    trawl_core::stream::apply_stage(stage, event, ctx)
        .expect("a compiled plan prepares every pattern")
}

fn eval_expr_with_pins(
    condition: &trawl_core::ast::Spanned<trawl_core::ast::Expr>,
    event: &trawl_core::row::Row,
    pins: &PinScope,
    ctx: &trawl_core::context::EvalContext,
) -> EvalValue {
    let patterns = PatternTable::prepare([condition], &mut Preparer::new())
        .expect("the condition's patterns prepare");
    trawl_core::eval::eval_expr_with_pins(condition, event, pins, &patterns, ctx)
        .expect("every pattern is prepared")
}

fn bind_params(params: &[SqlValue]) -> Vec<Box<dyn duckdb::ToSql>> {
    params
        .iter()
        .map(|v| -> Box<dyn duckdb::ToSql> {
            match v {
                SqlValue::String(s) => Box::new(s.clone()),
                SqlValue::Int(i) => Box::new(*i),
                SqlValue::Float(f) => Box::new(*f),
                SqlValue::Bool(b) => Box::new(*b),
                SqlValue::Timestamp(at) => Box::new(duckdb::types::Value::Timestamp(
                    duckdb::types::TimeUnit::Microsecond,
                    at.and_utc().timestamp_micros(),
                )),
            }
        })
        .collect()
}

fn form_name(form: &CompareForm) -> &'static str {
    match form {
        CompareForm::Native(_) => "Native",
        CompareForm::Conformed { .. } => "Conformed",
        CompareForm::Text(_) => "Text",
        CompareForm::TextOrNumeric(_) => "TextOrNumeric",
        CompareForm::NumericOnText(_) => "NumericOnText",
        CompareForm::SeverityBand { .. } => "SeverityBand",
        CompareForm::SeverityExact(_) => "SeverityExact",
    }
}

fn pattern_name(form: PatternForm) -> &'static str {
    match form {
        PatternForm::Native => "Native",
        PatternForm::BigIntText => "BigIntText",
        PatternForm::BooleanText => "BooleanText",
        PatternForm::DoubleText => "DoubleText",
        PatternForm::Rfc3339Text => "Rfc3339Text",
        PatternForm::SeverityText => "SeverityText",
    }
}

/// Returns the agreed boolean answer (`None` = UNKNOWN / filtered).
fn run_cell(
    conn: &Connection,
    dsl: &str,
    event: &Map<String, Value>,
    ft: &FieldTypes,
) -> Option<bool> {
    let query = parser::parse(dsl).expect("dsl parses");
    let condition = query
        .pipeline
        .iter()
        .find_map(|s| match &s.node {
            PipeStage::Where(w) => Some(w.condition.clone()),
            _ => None,
        })
        .expect("dsl has a where stage");
    let eval_result = match eval_expr_with_pins(
        &condition,
        &trawl_core::row::from_json(event),
        &PinScope::root(ft),
        &trawl_core::context::EvalContext::capture(),
    ) {
        EvalValue::Bool(b) => Some(b),
        EvalValue::Null => None,
        other => panic!("comparison answered {other:?} for {dsl:?}"),
    };

    let mut tmp = tempfile::Builder::new()
        .suffix(".ndjson")
        .tempfile()
        .unwrap();
    writeln!(tmp, "{}", Value::Object(event.clone())).unwrap();
    tmp.flush().unwrap();
    let source = tmp.path().to_str().unwrap().to_owned();
    let emitted = emitter::emit_with_pins(
        &query,
        &source,
        ft,
        &emitter::InputColumns::unprobed(),
        trawl_core::context::EvalContext::capture(),
    )
    .expect("emit succeeds");

    let count_sql = format!("SELECT count(*)::BIGINT FROM ({}) AS _sub", emitted.sql);
    let params = bind_params(&emitted.params);
    let param_refs: Vec<&dyn duckdb::ToSql> = params.iter().map(AsRef::as_ref).collect();
    let count: i64 = conn
        .query_row(&count_sql, param_refs.as_slice(), |row| row.get(0))
        .unwrap_or_else(|e| {
            panic!(
                "pinned where must not error: {e}\ndsl: {dsl:?}\nsql: {}\nparams: {:?}",
                emitted.sql, emitted.params
            )
        });
    let sql_result = count > 0;

    assert_eq!(
        eval_result == Some(true),
        sql_result,
        "lane divergence\ndsl: {dsl:?}\nevent: {event:?}\neval: {eval_result:?}\nsql: {}\nparams: {:?}",
        emitted.sql,
        emitted.params
    );
    eval_result
}

/// Unlike [`run_cell`], which evaluates the `where` condition alone at root
/// scope, this applies every earlier stage, so a stage that renames the
/// row's keys is exercised.
fn run_pipeline_cell(
    conn: &Connection,
    dsl: &str,
    event: &Map<String, Value>,
    ft: &FieldTypes,
) -> bool {
    let query = parser::parse(dsl).expect("dsl parses");

    let plan =
        compile_stream_plan(&query.pipeline, &PinScope::root(ft), None).expect("plan compiles");
    let StreamPlan::PassThrough(mut stages) = plan else {
        panic!("{dsl:?} must compile to a per-event plan");
    };
    let mut streamed = trawl_core::row::from_json(event);
    let mut live_result = true;
    for stage in &mut stages {
        match apply_stage(
            stage,
            &mut streamed,
            &trawl_core::context::EvalContext::capture(),
        ) {
            StageResult::Pass => {}
            StageResult::Filtered | StageResult::Done => {
                live_result = false;
                break;
            }
        }
    }

    let mut tmp = tempfile::Builder::new()
        .suffix(".ndjson")
        .tempfile()
        .unwrap();
    writeln!(tmp, "{}", Value::Object(event.clone())).unwrap();
    tmp.flush().unwrap();
    let source = tmp.path().to_str().unwrap().to_owned();
    let emitted = emitter::emit_with_pins(
        &query,
        &source,
        ft,
        &emitter::InputColumns::unprobed(),
        trawl_core::context::EvalContext::capture(),
    )
    .expect("emit succeeds");
    let count_sql = format!("SELECT count(*)::BIGINT FROM ({}) AS _sub", emitted.sql);
    let params = bind_params(&emitted.params);
    let param_refs: Vec<&dyn duckdb::ToSql> = params.iter().map(AsRef::as_ref).collect();
    let count: i64 = conn
        .query_row(&count_sql, param_refs.as_slice(), |row| row.get(0))
        .unwrap_or_else(|e| {
            panic!(
                "pinned pipeline must not error: {e}\ndsl: {dsl:?}\nsql: {}",
                emitted.sql
            )
        });
    let batch_result = count > 0;

    assert_eq!(
        live_result, batch_result,
        "lane divergence\ndsl: {dsl:?}\nevent: {event:?}\nlive: {live_result}\nsql: {}",
        emitted.sql
    );
    batch_result
}

fn run_let_cell(
    conn: &Connection,
    dsl: &str,
    event: &Map<String, Value>,
    ft: &FieldTypes,
    outputs: &[&str],
) {
    let query = parser::parse(dsl).expect("dsl parses");

    let plan =
        compile_stream_plan(&query.pipeline, &PinScope::root(ft), None).expect("plan compiles");
    let StreamPlan::PassThrough(mut stages) = plan else {
        panic!("{dsl:?} must compile to a per-event plan");
    };
    let mut streamed = trawl_core::row::from_json(event);
    for stage in &mut stages {
        assert_eq!(
            apply_stage(
                stage,
                &mut streamed,
                &trawl_core::context::EvalContext::capture()
            ),
            StageResult::Pass,
            "{dsl:?} must not filter the event"
        );
    }

    let mut tmp = tempfile::Builder::new()
        .suffix(".ndjson")
        .tempfile()
        .unwrap();
    writeln!(tmp, "{}", Value::Object(event.clone())).unwrap();
    tmp.flush().unwrap();
    let source = tmp.path().to_str().unwrap().to_owned();
    let emitted = emitter::emit_with_pins(
        &query,
        &source,
        ft,
        &emitter::InputColumns::unprobed(),
        trawl_core::context::EvalContext::capture(),
    )
    .expect("emit succeeds");
    let row_sql = format!("SELECT to_json(_sub) FROM ({}) AS _sub", emitted.sql);
    let params = bind_params(&emitted.params);
    let param_refs: Vec<&dyn duckdb::ToSql> = params.iter().map(AsRef::as_ref).collect();
    let row_json: String = conn
        .query_row(&row_sql, param_refs.as_slice(), |row| row.get(0))
        .unwrap_or_else(|e| panic!("pinned let must not error: {e}\ndsl: {dsl:?}\nsql: {row_sql}"));
    let batch: Map<String, Value> = serde_json::from_str(&row_json).expect("row is a JSON object");

    let streamed = trawl_core::row::to_json(streamed);
    for name in outputs {
        let batch_value = batch.get(*name).unwrap_or(&Value::Null);
        let streamed_value = streamed.get(*name).unwrap_or(&Value::Null);
        assert_eq!(
            streamed_value, batch_value,
            "lane divergence on {name:?}\ndsl: {dsl:?}\nevent: {event:?}\nbatch row: {batch:?}"
        );
    }
}

fn event_with(field: &str, value: Value) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(field.into(), value);
    m.insert("message".into(), Value::String("hello".into()));
    m
}

#[test]
fn compare_form_coverage_in_both_pipeline_lanes() {
    let conn = Connection::open_in_memory().unwrap();
    let mut seen: BTreeSet<&'static str> = BTreeSet::new();

    // (pin, dsl comparison, op+literal for form resolution, event value —
    // a wire shape whose ndjson inference is the pin's physical type)
    #[allow(clippy::type_complexity)]
    let cells: &[(
        CanonicalType,
        &str,
        trawl_core::ast::FilterOp,
        LiteralValue,
        Value,
    )] = &[
        (
            CanonicalType::Varchar,
            "* | where f == 200",
            trawl_core::ast::FilterOp::Eq,
            LiteralValue::Int(200),
            Value::from("200"),
        ),
        (
            CanonicalType::Varchar,
            "* | where f == \"accepted\"",
            trawl_core::ast::FilterOp::Eq,
            LiteralValue::String("accepted".into()),
            Value::from("accepted"),
        ),
        (
            CanonicalType::Varchar,
            "* | where f > 400",
            trawl_core::ast::FilterOp::Gt,
            LiteralValue::Int(400),
            Value::from("404"),
        ),
        (
            CanonicalType::Varchar,
            "* | where f > \"alpha\"",
            trawl_core::ast::FilterOp::Gt,
            LiteralValue::String("alpha".into()),
            Value::from("beta"),
        ),
        (
            CanonicalType::BigInt,
            "* | where f > 400",
            trawl_core::ast::FilterOp::Gt,
            LiteralValue::Int(400),
            Value::from(404),
        ),
        (
            CanonicalType::Double,
            "* | where f > 1.5",
            trawl_core::ast::FilterOp::Gt,
            LiteralValue::Float(FloatLiteral::new(1.5, "1.5")),
            Value::from(2.5),
        ),
        (
            CanonicalType::Boolean,
            "* | where f == true",
            trawl_core::ast::FilterOp::Eq,
            LiteralValue::Bool(true),
            Value::from(true),
        ),
        (
            CanonicalType::Severity,
            "* | where f == \"error\"",
            trawl_core::ast::FilterOp::Eq,
            LiteralValue::String("error".into()),
            Value::from(18),
        ),
        (
            CanonicalType::Severity,
            "* | where f >= \"warn\"",
            trawl_core::ast::FilterOp::Gte,
            LiteralValue::String("warn".into()),
            Value::from(17),
        ),
    ];

    for (pin, dsl, op, literal, value) in cells {
        let form = compare::compare_form_bound(Some(*pin), *op, literal)
            .expect("matrix literals bind under their pin")
            .expect("matrix literals are never null");
        seen.insert(form_name(&form));
        let mut ft = FieldTypes::new();
        ft.insert("f", *pin);
        run_cell(&conn, dsl, &event_with("f", value.clone()), &ft);
    }

    let expected: BTreeSet<&'static str> = [
        "Native",
        "Conformed",
        "Text",
        "TextOrNumeric",
        "NumericOnText",
        "SeverityBand",
        "SeverityExact",
    ]
    .into_iter()
    .collect();
    assert_eq!(
        seen, expected,
        "every CompareForm variant must be exercised in both pipeline lanes"
    );
}

#[test]
fn pattern_form_coverage_in_both_pipeline_lanes() {
    let conn = Connection::open_in_memory().unwrap();
    let mut seen: BTreeSet<&'static str> = BTreeSet::new();

    // TIMESTAMP rides parquet (a bare date string would need a conformed
    // column); the ndjson pins infer their physical type directly.
    let cells: &[(CanonicalType, &str, Value)] = &[
        (
            CanonicalType::Varchar,
            "* | where f matches \"^2\"",
            Value::from("200"),
        ),
        (
            CanonicalType::BigInt,
            "* | where f matches \"^4\"",
            Value::from(404),
        ),
        (
            CanonicalType::Double,
            "* | where f like \"200._\"",
            Value::from(200.5),
        ),
        (
            CanonicalType::Boolean,
            "* | where f like \"tru%\"",
            Value::from(true),
        ),
        // SEVERITY renders the OTel short name, so a band's prefix glob
        // is exactly its four rungs.
        (
            CanonicalType::Severity,
            "* | where f like \"warn%\"",
            Value::from(15),
        ),
    ];
    for (pin, dsl, value) in cells {
        seen.insert(pattern_name(compare::pattern_form(Some(*pin))));
        let mut ft = FieldTypes::new();
        ft.insert("f", *pin);
        let got = run_cell(&conn, dsl, &event_with("f", value.clone()), &ft);
        assert_eq!(got, Some(true), "{dsl} must match its canonical text");
    }

    {
        seen.insert(pattern_name(compare::pattern_form(Some(
            CanonicalType::Timestamp,
        ))));
        let mut ft = FieldTypes::new();
        ft.insert("f", CanonicalType::Timestamp);
        let query = parser::parse("* | where f matches \"T03:30\"").expect("parses");
        let condition = match &query.pipeline[0].node {
            PipeStage::Where(w) => w.condition.clone(),
            _ => unreachable!(),
        };
        let event = event_with("f", Value::from("2026-01-15T09:00:00+05:30"));
        let eval_result = eval_expr_with_pins(
            &condition,
            &trawl_core::row::from_json(&event),
            &PinScope::root(&ft),
            &trawl_core::context::EvalContext::capture(),
        );
        assert_eq!(eval_result, EvalValue::Bool(true));

        let tmp = tempfile::Builder::new()
            .suffix(".parquet")
            .tempfile()
            .unwrap();
        let source = tmp.path().to_str().unwrap().to_owned();
        conn.execute_batch(&format!(
            "COPY (SELECT TIMESTAMP '2026-01-15 03:30:00' AS f, 'hello' AS message) \
             TO '{source}' (FORMAT PARQUET)"
        ))
        .unwrap();
        let emitted = emitter::emit_with_pins(
            &query,
            &source,
            &ft,
            &emitter::InputColumns::unprobed(),
            trawl_core::context::EvalContext::capture(),
        )
        .expect("emit succeeds");
        let params = bind_params(&emitted.params);
        let param_refs: Vec<&dyn duckdb::ToSql> = params.iter().map(AsRef::as_ref).collect();
        let count: i64 = conn
            .query_row(
                &format!("SELECT count(*)::BIGINT FROM ({}) AS _sub", emitted.sql),
                param_refs.as_slice(),
                |row| row.get(0),
            )
            .expect("timestamp pattern must not error");
        assert_eq!(count, 1, "batch matches the RFC 3339 UTC instant text");
    }

    let expected: BTreeSet<&'static str> = [
        "Native",
        "BigIntText",
        "BooleanText",
        "DoubleText",
        "Rfc3339Text",
        "SeverityText",
    ]
    .into_iter()
    .collect();
    assert_eq!(
        seen, expected,
        "every PatternForm variant must be exercised in both pipeline lanes"
    );
}

#[test]
fn float_literal_above_2_53_binds_its_source_token_in_both_lanes() {
    let conn = Connection::open_in_memory().unwrap();
    let mut ft = FieldTypes::new();
    ft.insert("f", CanonicalType::Varchar);

    assert_eq!(
        "9007199254740993.0".parse::<f64>().unwrap().to_bits(),
        "9007199254740992.0".parse::<f64>().unwrap().to_bits(),
        "premise: f64 collapses these neighbours"
    );

    let stored = event_with("f", Value::from("9007199254740993"));
    let cells: &[(&str, bool)] = &[
        ("* | where f == 9007199254740993.0", true),
        ("* | where f == \"9007199254740993.0\"", true),
        ("* | where f == 9007199254740992.0", false),
        ("* | where f == \"9007199254740992.0\"", false),
        ("* | where f > 9007199254740992.0", true),
        ("* | where f > \"9007199254740992.0\"", true),
        ("* | where f > 9007199254740993.0", false),
        ("* | where f > \"9007199254740993.0\"", false),
    ];
    for (dsl, expected) in cells {
        assert_eq!(
            run_cell(&conn, dsl, &stored, &ft),
            Some(*expected),
            "{dsl} against {stored:?}"
        );
    }
}

#[test]
fn negative_literals_bind_pin_aware_in_both_lanes() {
    let conn = Connection::open_in_memory().unwrap();
    let mut ft = FieldTypes::new();
    ft.insert("f", CanonicalType::Varchar);

    let stored = event_with("f", Value::from("-200"));
    let cells: &[(&str, bool)] = &[
        ("* | where f > -400", true),
        ("* | where f > \"-400\"", true),
        ("* | where f < -400", false),
        ("* | where f < \"-400\"", false),
        ("* | where f == -200", true),
        ("* | where f == \"-200.0\"", true),
        ("* | where f == -200.0", true),
        ("* | where f != -200", false),
        ("* | where f in (-400, -200)", true),
        ("* | where f in (-400, 200)", false),
        ("* | where f > -200.5", true),
        ("* | where f > \"-200.5\"", true),
        ("* | where f < -199.5", true),
    ];
    for (dsl, expected) in cells {
        assert_eq!(
            run_cell(&conn, dsl, &stored, &ft),
            Some(*expected),
            "{dsl} against {stored:?}"
        );
    }

    let unreadable = event_with("f", Value::from("accepted"));
    assert_eq!(
        run_cell(&conn, "* | where f > -400", &unreadable, &ft),
        None
    );
    assert_eq!(
        run_cell(&conn, "* | where f == -400", &unreadable, &ft),
        Some(false)
    );
}

#[test]
fn let_sibling_references_bind_column_then_alias_in_both_lanes() {
    let conn = Connection::open_in_memory().unwrap();
    let mut ft = FieldTypes::new();
    ft.insert("a", CanonicalType::BigInt);

    let event = event_with("a", Value::from(5));

    run_let_cell(&conn, "* | let a = 1, b = a", &event, &ft, &["a", "b"]);
    run_let_cell(&conn, "* | let a = a + 1, b = a", &event, &ft, &["a", "b"]);
    run_let_cell(
        &conn,
        "* | let ms = 1000, total = ms * 2",
        &event,
        &ft,
        &["ms", "total"],
    );
    run_let_cell(&conn, "* | let x = a, y = x", &event, &ft, &["x", "y"]);
    // "Carries the column" is DuckDB's own case-insensitive binding, not
    // an exact key match: `A` names the row's `a`, so the target shadows
    // it and the sibling still reads the original value. Only `b` is
    // compared, because the emitted exclusion list compares names as
    // strings: the `a` column survives a target spelled `A` and DuckDB
    // disambiguates the alias to `A_1`, so the lanes project different
    // column names while agreeing on the value this rule is about.
    run_let_cell(&conn, "* | let A = 1, b = A", &event, &ft, &["b"]);
}

#[test]
fn mixed_case_aliases_resolve_in_both_lanes() {
    let conn = Connection::open_in_memory().unwrap();
    let mut ft = FieldTypes::new();
    ft.insert("status", CanonicalType::Varchar);

    let event = event_with("status", Value::from("404"));

    assert!(run_pipeline_cell(
        &conn,
        "* | where Status > 400",
        &event,
        &ft
    ));
    assert!(!run_pipeline_cell(
        &conn,
        "* | where Status > 500",
        &event,
        &ft
    ));
    assert!(run_pipeline_cell(
        &conn,
        "* | rename status as St | where St > 400",
        &event,
        &ft
    ));
    assert!(!run_pipeline_cell(
        &conn,
        "* | rename status as St | where St > 500",
        &event,
        &ft
    ));
    assert!(run_pipeline_cell(
        &conn,
        "* | let S2 = status | where S2 > 400",
        &event,
        &ft
    ));
    assert!(!run_pipeline_cell(
        &conn,
        "* | let S2 = status | where S2 > 500",
        &event,
        &ft
    ));
    assert!(run_pipeline_cell(
        &conn,
        "* | rename status as St | where St in (404)",
        &event,
        &ft
    ));
    assert!(run_pipeline_cell(
        &conn,
        "* | rename status as St | where St matches /^404$/",
        &event,
        &ft
    ));
    assert!(run_pipeline_cell(
        &conn,
        "* | rename status as St | where St == \"404\"",
        &event,
        &ft
    ));
    assert!(!run_pipeline_cell(
        &conn,
        "* | rename status as St | where St == \"405\"",
        &event,
        &ft
    ));
    assert!(run_pipeline_cell(
        &conn,
        "* | let S2 = status | where S2 == \"404\"",
        &event,
        &ft
    ));
    // ...and the other direction: a stage makes a mixed-case key and a
    // later stage names it in a different case. `DuckDB` binds its own
    // `"St"` column for `st`/`sT`, so the live lane must bind the row's
    // key the same way instead of missing and dropping the event.
    for dsl in [
        "* | rename status as St | where st > 400",
        "* | rename status as St | where sT > 400",
        "* | let S2 = status | where s2 > 400",
        "* | rename Status as st | where st > 400",
    ] {
        assert!(run_pipeline_cell(&conn, dsl, &event, &ft), "{dsl}");
    }
    assert!(!run_pipeline_cell(
        &conn,
        "* | rename status as St | where st > 500",
        &event,
        &ft
    ));
}

#[test]
fn sev_subject_binds_through_the_severity_rules_in_both_lanes() {
    let conn = Connection::open_in_memory().unwrap();
    let unpinned = FieldTypes::new();

    let cells: &[(&str, Value, Option<bool>)] = &[
        (
            r#"* | where sev(level) == "error""#,
            Value::String("error2".into()),
            Some(true),
        ),
        (
            r#"* | where sev(level) == "error""#,
            Value::String("warn".into()),
            Some(false),
        ),
        (
            r#"* | where sev(level) >= "error""#,
            Value::String("error2".into()),
            Some(true),
        ),
        (
            r#"* | where sev(level) >= "error""#,
            Value::String("warning".into()),
            Some(false),
        ),
        (
            r#"* | where "error" <= sev(level)"#,
            Value::String("fatal".into()),
            Some(true),
        ),
        (
            r#"* | where sev(level) == "error2""#,
            Value::String("error2".into()),
            Some(true),
        ),
        (
            r#"* | where sev(level) == "error2""#,
            Value::String("error".into()),
            Some(false),
        ),
        (
            "* | where sev(level) > 0",
            Value::String("info".into()),
            Some(true),
        ),
        (
            r#"* | where sev(level) == "error""#,
            Value::String("gold".into()),
            None,
        ),
        (
            r#"* | where sev(level) != "error""#,
            Value::String("gold".into()),
            None,
        ),
        (
            r#"* | where sev(level) != "error""#,
            Value::String("warn".into()),
            Some(true),
        ),
        (
            r#"* | where sev(level) in ("warn", "fatal")"#,
            Value::String("fatal2".into()),
            Some(true),
        ),
        (
            r#"* | where sev(level) in ("warn", "fatal")"#,
            Value::String("error".into()),
            Some(false),
        ),
        (
            r#"* | where sev(level) matches "^warn$""#,
            Value::String("warning".into()),
            Some(true),
        ),
        (
            r#"* | where sev(level) matches "^error""#,
            Value::String("error3".into()),
            Some(true),
        ),
        (
            r#"* | where sev(level) == "error""#,
            Value::Number(18.into()),
            Some(true),
        ),
        (
            r#"* | where sev(level) == "error""#,
            Value::Bool(true),
            None,
        ),
    ];

    for (dsl, value, expected) in cells {
        let event = event_with("level", value.clone());
        assert_eq!(
            run_cell(&conn, dsl, &event, &unpinned),
            *expected,
            "{dsl} over {value:?}"
        );
    }
}

#[test]
fn sev_out_of_ladder_points_agree_in_both_lanes() {
    let conn = Connection::open_in_memory().unwrap();
    let unpinned = FieldTypes::new();

    let cells: &[(&str, Value, Option<bool>)] = &[
        (
            "* | where sev(level) in (99, 101, 250)",
            Value::String("error".into()),
            Some(false),
        ),
        (
            "* | where sev(level) in (99, 101, 250)",
            Value::String("gold".into()),
            None,
        ),
        (
            "* | where not (sev(level) in (99, 101, 250))",
            Value::String("error".into()),
            Some(true),
        ),
        (
            "* | where not (sev(level) in (99, 101, 250))",
            Value::String("gold".into()),
            None,
        ),
        (
            r#"* | where sev(level) in ("error", 99, 101, 250)"#,
            Value::String("error3".into()),
            Some(true),
        ),
        (
            r#"* | where sev(level) in ("error", 99, 101, 250)"#,
            Value::String("warn".into()),
            Some(false),
        ),
        (
            r#"* | where sev(level) in ("error", 99, 101, 250)"#,
            Value::String("gold".into()),
            None,
        ),
        (
            "* | where sev(level) != 99",
            Value::String("error".into()),
            Some(true),
        ),
        (
            "* | where sev(level) != 99",
            Value::String("gold".into()),
            None,
        ),
    ];

    for (dsl, value, expected) in cells {
        let event = event_with("level", value.clone());
        assert_eq!(
            run_cell(&conn, dsl, &event, &unpinned),
            *expected,
            "{dsl} over {value:?}"
        );
    }
}

#[test]
fn sev_subject_sets_and_reversed_operands_agree_in_both_lanes() {
    let conn = Connection::open_in_memory().unwrap();
    let unpinned = FieldTypes::new();

    let cells: &[(&str, Value, Option<bool>)] = &[
        (
            r#"* | where sev(level) in ("error", "err", "error2")"#,
            Value::String("error4".into()),
            Some(true),
        ),
        (
            r#"* | where sev(level) in ("error", "err", "error2")"#,
            Value::String("warn".into()),
            Some(false),
        ),
        (
            r#"* | where sev(level) in ("warn", 17)"#,
            Value::String("warn3".into()),
            Some(true),
        ),
        (
            r#"* | where sev(level) in ("warn", 17)"#,
            Value::String("error".into()),
            Some(true),
        ),
        (
            r#"* | where sev(level) in ("warn", 17)"#,
            Value::String("error2".into()),
            Some(false),
        ),
        (
            r#"* | where sev(level) in ("warn", 99)"#,
            Value::String("warn".into()),
            Some(true),
        ),
        (
            r#"* | where sev(level) in ("warn", 99)"#,
            Value::String("fatal".into()),
            Some(false),
        ),
        (
            r#"* | where sev(level) in ("warn", "error")"#,
            Value::String("gold".into()),
            None,
        ),
        (
            r#"* | where "error" == sev(level)"#,
            Value::String("error3".into()),
            Some(true),
        ),
        (
            r#"* | where "error" == sev(level)"#,
            Value::String("warn".into()),
            Some(false),
        ),
        (
            r#"* | where "error" != sev(level)"#,
            Value::String("warn".into()),
            Some(true),
        ),
        (
            r#"* | where "error" != sev(level)"#,
            Value::String("error".into()),
            Some(false),
        ),
        (
            r#"* | where "error" != sev(level)"#,
            Value::String("gold".into()),
            None,
        ),
    ];

    for (dsl, value, expected) in cells {
        let event = event_with("level", value.clone());
        assert_eq!(
            run_cell(&conn, dsl, &event, &unpinned),
            *expected,
            "{dsl} over {value:?}"
        );
    }
}

#[test]
fn sev_subject_carries_its_dialect_in_both_lanes() {
    let conn = Connection::open_in_memory().unwrap();
    let unpinned = FieldTypes::new();
    let cells: &[(&str, Value, Option<bool>)] = &[
        (
            r#"* | where sev(level, "syslog") == "error""#,
            Value::Number(3.into()),
            Some(true),
        ),
        (
            r#"* | where sev(level, "otel") == "error""#,
            Value::Number(3.into()),
            Some(false),
        ),
        (
            r#"* | where sev(level, "syslog") == "error""#,
            Value::String("error".into()),
            Some(true),
        ),
        (
            r#"* | where sev(level, "syslog") == "error""#,
            Value::Number(9.into()),
            None,
        ),
    ];
    for (dsl, value, expected) in cells {
        let event = event_with("level", value.clone());
        assert_eq!(
            run_cell(&conn, dsl, &event, &unpinned),
            *expected,
            "{dsl} over {value:?}"
        );
    }
}

#[test]
fn case_variant_projections_agree_across_lanes() {
    let conn = Connection::open_in_memory().unwrap();
    let field_types = FieldTypes::new();
    let rows = [
        serde_json::json!({"a": 5, "b": "x", "message": "mx", "_time": "2026-08-15T00:00:00Z"}),
        serde_json::json!({"a": 7, "b": "x", "message": "my", "_time": "2026-08-15T00:00:01Z"}),
    ];
    let groups: [(&[&str], &[&str]); 2] = [
        (
            &["| let A = a", "| let A = 1", "| rename a as A"],
            &[
                "| where a > 0",
                "| where A > 0",
                "| stats count() by a",
                "| stats count() by A",
                "| stats sum(a) as s",
                "| stats first(a) as f",
                "| stats dc(a) as d",
                "| table a",
                "| table A, message",
                "| drop a",
                "| dedup a",
                "| top 5 a",
            ],
        ),
        (
            &[
                "| rename a as B",
                "| let B = a",
                r#"| extract "(?P<B>[a-z]+)" from message"#,
                r#"| extract "(?P<B>ZZZ+)" from message"#,
                r#"| extract "m(?P<B>ZZ)?" from message"#,
                r#"| extract "(?P<B>mx)" from message"#,
                r#"| extract "m(?P<B>z*)" from message"#,
                r#"| extract "m(?P<B>z*)" from message | extract "(?P<B>q*)" from message"#,
            ],
            &[
                "| table B, message",
                "| table b, message",
                "| stats count() by B",
                "| stats count() by b",
                "| stats first(B) as fb",
                "| stats dc(b) as db",
                "| dedup B",
                "| top 5 b",
                "| drop b",
            ],
        ),
    ];

    for (projections, readers) in groups {
        for projection in projections {
            for reader in readers {
                let writes_one_value = projection.contains("= 1")
                    || projection.contains("ZZ")
                    || projection.contains("z*");
                if reader.starts_with("| dedup") && writes_one_value {
                    continue;
                }
                let dsl = format!("* {projection} {reader}");
                let query = parser::parse(&dsl).expect("dsl parses");
                let live = live_rows(&query, &rows, &field_types, &dsl);

                let mut tmp = tempfile::Builder::new()
                    .suffix(".ndjson")
                    .tempfile()
                    .unwrap();
                for row in &rows {
                    writeln!(tmp, "{row}").unwrap();
                }
                tmp.flush().unwrap();
                let emitted = emitter::emit_with_pins(
                    &query,
                    tmp.path().to_str().unwrap(),
                    &field_types,
                    &emitter::InputColumns::unprobed(),
                    trawl_core::context::EvalContext::capture(),
                )
                .expect("emit succeeds");
                let rows_sql = format!("SELECT to_json(_sub) FROM ({}) AS _sub", emitted.sql);
                let params = bind_params(&emitted.params);
                let param_refs: Vec<&dyn duckdb::ToSql> =
                    params.iter().map(AsRef::as_ref).collect();
                let mut stmt = conn.prepare(&rows_sql).expect("sql prepares");
                let batch: Vec<Map<String, Value>> = stmt
                    .query_map(param_refs.as_slice(), |row| row.get::<_, String>(0))
                    .unwrap_or_else(|error| panic!("{dsl}: sql must run: {error}\n{rows_sql}"))
                    .map(|row| serde_json::from_str(&row.unwrap()).unwrap())
                    .collect();

                assert_eq!(
                    comparable(&live),
                    comparable(&batch),
                    "lane divergence\ndsl: {dsl}\nlive: {live:?}\nbatch: {batch:?}"
                );
            }
        }
    }
}

fn live_rows(
    query: &trawl_core::ast::Query,
    rows: &[Value],
    field_types: &FieldTypes,
    dsl: &str,
) -> Vec<Map<String, Value>> {
    let plan = compile_stream_plan(&query.pipeline, &PinScope::root(field_types), None)
        .unwrap_or_else(|error| panic!("{dsl}: plan must compile: {error}"));
    let anchor = trawl_core::context::EvalContext::capture();
    let feed = |stages: &mut [trawl_core::stream::CompiledStage],
                event: &mut trawl_core::row::Row| {
        stages
            .iter_mut()
            .all(|stage| apply_stage(stage, event, &anchor) == StageResult::Pass)
    };
    match plan {
        StreamPlan::PassThrough(mut stages) => rows
            .iter()
            .filter_map(|row| {
                let mut event = trawl_core::row::from_json(row.as_object().unwrap());
                feed(&mut stages, &mut event).then(|| trawl_core::row::to_json(event))
            })
            .collect(),
        StreamPlan::Aggregate {
            pre_stages: mut pre,
            aggregation: mut aggregate,
            ..
        } => {
            for row in rows {
                let mut event = trawl_core::row::from_json(row.as_object().unwrap());
                if feed(&mut pre, &mut event) {
                    aggregate.feed_event(&event, &anchor);
                }
            }
            aggregate
                .snapshot()
                .1
                .into_iter()
                .map(trawl_core::row::to_json)
                .collect()
        }
    }
}

fn comparable(rows: &[Map<String, Value>]) -> BTreeSet<Vec<(String, String)>> {
    rows.iter()
        .map(|row| {
            let mut cells: Vec<(String, String)> = row
                .iter()
                .filter(|(key, _)| key.as_str() != "_time")
                .map(|(key, value)| {
                    let text = match value {
                        Value::String(text) => text.clone(),
                        other => other.to_string(),
                    };
                    let text = text
                        .parse::<f64>()
                        .map_or(text, |number| number.to_string());
                    (trawl_core::schema::catalog_key(key), text)
                })
                .collect();
            cells.sort();
            cells
        })
        .collect()
}

#[test]
fn sev_let_target_adopts_the_declared_pin_in_both_lanes() {
    let conn = Connection::open_in_memory().unwrap();
    let unpinned = FieldTypes::new();

    let event = event_with("level", Value::String("error2".into()));
    run_let_cell(&conn, "* | let s = sev(level)", &event, &unpinned, &["s"]);
    run_let_cell(
        &conn,
        r#"* | let s = sev(level, "syslog")"#,
        &event_with("level", Value::Number(3.into())),
        &unpinned,
        &["s"],
    );

    assert!(run_pipeline_cell(
        &conn,
        r#"* | let s = sev(level) | where s == "error""#,
        &event,
        &unpinned
    ));
    assert!(run_pipeline_cell(
        &conn,
        r#"* | let s = sev(level) | where s >= "error""#,
        &event,
        &unpinned
    ));
    assert!(!run_pipeline_cell(
        &conn,
        r#"* | let s = sev(level) | where s == "warn""#,
        &event,
        &unpinned
    ));
}

#[test]
fn sev_subject_refuses_an_unknown_token_in_both_lanes() {
    let unpinned = FieldTypes::new();
    for dsl in [
        r#"* | where sev(level) == "bogus""#,
        r#"* | where "bogus" == sev(level)"#,
        r#"* | where sev(level) >= "gold""#,
        r#"* | where sev(level) in ("error", "spicy")"#,
        r#"* | let hot = sev(level) == "spicy""#,
    ] {
        let query = parser::parse(dsl).expect("parses");
        let emit_err = emitter::emit_with_pins(
            &query,
            "/data/*.parquet",
            &unpinned,
            &emitter::InputColumns::unprobed(),
            trawl_core::context::EvalContext::capture(),
        )
        .expect_err("batch must refuse");
        let stream_err = compile_stream_plan(&query.pipeline, &PinScope::unpinned(), None)
            .expect_err("live must refuse");
        assert!(
            emit_err.to_string().contains("unknown severity value"),
            "{dsl}: {emit_err}"
        );
        assert!(
            stream_err.to_string().contains("unknown severity value"),
            "{dsl}: {stream_err}"
        );
    }
}
