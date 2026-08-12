// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The slice-A′ coverage matrix (ADR-0001 discipline): every
//! [`CompareForm`] and [`PatternForm`] variant must be exercised in BOTH
//! pipeline lanes — the `| where` SQL the emitter renders, executed
//! against `DuckDB`, and the pin-aware streaming evaluator — by at least
//! one matrix cell, and the two lanes must agree on every cell.
//!
//! The matrix accumulates which variant each cell resolved to and asserts
//! full coverage at the end, so a new `CompareForm`/`PatternForm` variant
//! fails this test until the matrix names a cell that reaches it in both
//! lanes.

use std::collections::BTreeSet;
use std::io::Write as _;

use duckdb::Connection;
use serde_json::{Map, Value};

use trawl_core::ast::PipeStage;
use trawl_core::compare::{self, CompareForm, PatternForm};
use trawl_core::emitter::{self, SqlValue};
use trawl_core::eval::{EvalValue, eval_expr_with_pins};
use trawl_core::parser;
use trawl_core::pin_scope::PinScope;
use trawl_core::schema::{CanonicalType, FieldTypes};

fn bind_params(params: &[SqlValue]) -> Vec<Box<dyn duckdb::ToSql>> {
    params
        .iter()
        .map(|v| -> Box<dyn duckdb::ToSql> {
            match v {
                SqlValue::String(s) => Box::new(s.clone()),
                SqlValue::Int(i) => Box::new(*i),
                SqlValue::Float(f) => Box::new(*f),
                SqlValue::Bool(b) => Box::new(*b),
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
    }
}

fn pattern_name(form: PatternForm) -> &'static str {
    match form {
        PatternForm::Native => "Native",
        PatternForm::BigIntText => "BigIntText",
        PatternForm::BooleanText => "BooleanText",
        PatternForm::DoubleText => "DoubleText",
        PatternForm::Rfc3339Text => "Rfc3339Text",
    }
}

/// Run one `| where` cell through both lanes and assert agreement.
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
    let eval_result = match eval_expr_with_pins(&condition, event, &PinScope::root(ft)) {
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
    let emitted = emitter::emit_with_pins(&query, &source, ft).expect("emit succeeds");

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

fn event_with(field: &str, value: Value) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(field.into(), value);
    m.insert("message".into(), Value::String("hello".into()));
    m
}

/// Every `CompareForm` variant reaches both lanes through some `| where`
/// comparison cell, and every cell agrees.
#[test]
fn compare_form_coverage_in_both_pipeline_lanes() {
    let conn = Connection::open_in_memory().unwrap();
    let mut seen: BTreeSet<&'static str> = BTreeSet::new();

    // (pin, dsl comparison, op+literal for form resolution, event value —
    // a wire shape whose ndjson inference IS the pin's physical type)
    #[allow(clippy::type_complexity)]
    let cells: &[(
        CanonicalType,
        &str,
        trawl_core::ast::FilterOp,
        SqlValue,
        Value,
    )] = &[
        // VARCHAR: TextOrNumeric (eq + numeric), Text (eq + word),
        // NumericOnText (ordered + numeric), Native (ordered + word).
        (
            CanonicalType::Varchar,
            "* | where f == 200",
            trawl_core::ast::FilterOp::Eq,
            SqlValue::Int(200),
            Value::from("200"),
        ),
        (
            CanonicalType::Varchar,
            "* | where f == \"accepted\"",
            trawl_core::ast::FilterOp::Eq,
            SqlValue::String("accepted".into()),
            Value::from("accepted"),
        ),
        (
            CanonicalType::Varchar,
            "* | where f > 400",
            trawl_core::ast::FilterOp::Gt,
            SqlValue::Int(400),
            Value::from("404"),
        ),
        (
            CanonicalType::Varchar,
            "* | where f > \"alpha\"",
            trawl_core::ast::FilterOp::Gt,
            SqlValue::String("alpha".into()),
            Value::from("beta"),
        ),
        // Typed pins: Conformed, one per pin.
        (
            CanonicalType::BigInt,
            "* | where f > 400",
            trawl_core::ast::FilterOp::Gt,
            SqlValue::Int(400),
            Value::from(404),
        ),
        (
            CanonicalType::Double,
            "* | where f > 1.5",
            trawl_core::ast::FilterOp::Gt,
            SqlValue::Float(1.5),
            Value::from(2.5),
        ),
        (
            CanonicalType::Boolean,
            "* | where f == true",
            trawl_core::ast::FilterOp::Eq,
            SqlValue::Bool(true),
            Value::from(true),
        ),
    ];

    for (pin, dsl, op, literal, value) in cells {
        let form = compare::compare_form_bound(Some(*pin), *op, literal);
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
    ]
    .into_iter()
    .collect();
    assert_eq!(
        seen, expected,
        "every CompareForm variant must be exercised in both pipeline lanes"
    );
}

/// Every `PatternForm` variant reaches both lanes through some pattern
/// operator (`matches`/LIKE/ILIKE) cell, and every cell agrees.
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
    ];
    for (pin, dsl, value) in cells {
        seen.insert(pattern_name(compare::pattern_form(Some(*pin))));
        let mut ft = FieldTypes::new();
        ft.insert("f", *pin);
        let got = run_cell(&conn, dsl, &event_with("f", value.clone()), &ft);
        assert_eq!(got, Some(true), "{dsl} must match its canonical text");
    }

    // TIMESTAMP: over a real TIMESTAMP column, wire value with an offset.
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
        let eval_result = eval_expr_with_pins(&condition, &event, &PinScope::root(&ft));
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
        let emitted = emitter::emit_with_pins(&query, &source, &ft).expect("emit succeeds");
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
    ]
    .into_iter()
    .collect();
    assert_eq!(
        seen, expected,
        "every PatternForm variant must be exercised in both pipeline lanes"
    );
}
