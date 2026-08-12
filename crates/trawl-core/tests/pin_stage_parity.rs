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

use trawl_core::ast::{FloatLiteral, LiteralValue, PipeStage};
use trawl_core::compare::{self, CompareForm, PatternForm};
use trawl_core::emitter::{self, SqlValue};
use trawl_core::eval::{EvalValue, eval_expr_with_pins};
use trawl_core::parser;
use trawl_core::pin_scope::PinScope;
use trawl_core::schema::{CanonicalType, FieldTypes};
use trawl_core::stream::{StageResult, StreamPlan, apply_stage, compile_stream_plan};

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

/// Run a WHOLE pipeline through both lanes and assert the event either
/// survives in both or in neither — unlike [`run_cell`], which evaluates
/// the `where` condition alone at root scope, this applies every earlier
/// stage, so a stage that RENAMES the row's keys is exercised.
fn run_pipeline_cell(
    conn: &Connection,
    dsl: &str,
    event: &Map<String, Value>,
    ft: &FieldTypes,
) -> bool {
    let query = parser::parse(dsl).expect("dsl parses");

    let plan = compile_stream_plan(&query.pipeline, &PinScope::root(ft)).expect("plan compiles");
    let StreamPlan::PassThrough(mut stages) = plan else {
        panic!("{dsl:?} must compile to a per-event plan");
    };
    let mut streamed = event.clone();
    let mut live_result = true;
    for stage in &mut stages {
        match apply_stage(stage, &mut streamed) {
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
    let emitted = emitter::emit_with_pins(&query, &source, ft).expect("emit succeeds");
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

/// Run one `| let` pipeline through both lanes and assert the named
/// output columns carry the same value.
fn run_let_cell(
    conn: &Connection,
    dsl: &str,
    event: &Map<String, Value>,
    ft: &FieldTypes,
    outputs: &[&str],
) {
    let query = parser::parse(dsl).expect("dsl parses");

    // live lane: compile the plan under the catalog root and apply it.
    let plan = compile_stream_plan(&query.pipeline, &PinScope::root(ft)).expect("plan compiles");
    let StreamPlan::PassThrough(mut stages) = plan else {
        panic!("{dsl:?} must compile to a per-event plan");
    };
    let mut streamed = event.clone();
    for stage in &mut stages {
        assert_eq!(
            apply_stage(stage, &mut streamed),
            StageResult::Pass,
            "{dsl:?} must not filter the event"
        );
    }

    // batch lane: emit the SQL and read the single row back as JSON.
    let mut tmp = tempfile::Builder::new()
        .suffix(".ndjson")
        .tempfile()
        .unwrap();
    writeln!(tmp, "{}", Value::Object(event.clone())).unwrap();
    tmp.flush().unwrap();
    let source = tmp.path().to_str().unwrap().to_owned();
    let emitted = emitter::emit_with_pins(&query, &source, ft).expect("emit succeeds");
    let row_sql = format!("SELECT to_json(_sub) FROM ({}) AS _sub", emitted.sql);
    let params = bind_params(&emitted.params);
    let param_refs: Vec<&dyn duckdb::ToSql> = params.iter().map(AsRef::as_ref).collect();
    let row_json: String = conn
        .query_row(&row_sql, param_refs.as_slice(), |row| row.get(0))
        .unwrap_or_else(|e| panic!("pinned let must not error: {e}\ndsl: {dsl:?}\nsql: {row_sql}"));
    let batch: Map<String, Value> = serde_json::from_str(&row_json).expect("row is a JSON object");

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
        LiteralValue,
        Value,
    )] = &[
        // VARCHAR: TextOrNumeric (eq + numeric), Text (eq + word),
        // NumericOnText (ordered + numeric), Native (ordered + word).
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
        // Typed pins: Conformed, one per pin.
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
    ];

    for (pin, dsl, op, literal, value) in cells {
        let form = compare::compare_form_bound(Some(*pin), *op, literal)
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

/// A pipeline float literal above 2^53 binds the digits the user WROTE, in
/// both lanes, and answers the same whether or not it was quoted.
///
/// `f64` cannot name `9007199254740993`: it parses as the adjacent
/// `…992`. Binding the re-rendered double would make the unquoted literal
/// match the neighbouring identifier while the quoted spelling — carried
/// verbatim as a string — matched the right one, contradicting the
/// quote-insensitivity the bound door promises (ADR-0011 ruling #6).
#[test]
fn float_literal_above_2_53_binds_its_source_token_in_both_lanes() {
    let conn = Connection::open_in_memory().unwrap();
    let mut ft = FieldTypes::new();
    ft.insert("f", CanonicalType::Varchar);

    // The premise: the two literals below are ONE f64.
    assert_eq!(
        "9007199254740993.0".parse::<f64>().unwrap().to_bits(),
        "9007199254740992.0".parse::<f64>().unwrap().to_bits(),
        "premise: f64 collapses these neighbours"
    );

    let stored = event_with("f", Value::from("9007199254740993"));
    // (dsl, expected answer) — quoted and unquoted spellings of each.
    let cells: &[(&str, bool)] = &[
        ("* | where f == 9007199254740993.0", true),
        ("* | where f == \"9007199254740993.0\"", true),
        // The adjacent identifier is a different number, not a rounding.
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

/// A NEGATIVE numeric literal binds pin-aware in both lanes, and answers
/// the same whether or not it was quoted.
///
/// The parser hands `-400` over as `Unary{Neg, Literal(Int)}`, never as a
/// signed literal, so a door that only took `Expr::Literal` left the whole
/// negative class on the pin-blind path: against a VARCHAR pin that is a
/// `DuckDB` binder error (`VARCHAR` vs `BIGINT`) or a conversion error on
/// the first non-numeric row — exactly what slice A′ removes — while the
/// quoted spelling of the same number bound pin-aware and answered.
#[test]
fn negative_literals_bind_pin_aware_in_both_lanes() {
    let conn = Connection::open_in_memory().unwrap();
    let mut ft = FieldTypes::new();
    ft.insert("f", CanonicalType::Varchar);

    let stored = event_with("f", Value::from("-200"));
    // (dsl, expected answer) — quoted and unquoted spellings of each.
    let cells: &[(&str, bool)] = &[
        ("* | where f > -400", true),
        ("* | where f > \"-400\"", true),
        ("* | where f < -400", false),
        ("* | where f < \"-400\"", false),
        // A VARCHAR pin's `=` reads the number, not the spelling.
        ("* | where f == -200", true),
        ("* | where f == \"-200.0\"", true),
        ("* | where f == -200.0", true),
        ("* | where f != -200", false),
        ("* | where f in (-400, -200)", true),
        ("* | where f in (-400, 200)", false),
        // Ordered against a negative FLOAT literal, both spellings.
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

    // A row the pin-blind path could not even read: the ordered form is
    // UNKNOWN (no numeric reading), not the conversion error it used to
    // raise, and equality still answers through the text half.
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

/// `| let` resolves a sibling reference column-then-alias in BOTH lanes:
/// an input COLUMN wins (so an overwrite never feeds the assignment
/// beside it), and only a name resolving to no column binds the LATERAL
/// COLUMN ALIAS the sibling just defined.
///
/// The batch lane has no choice — the stage desugars to one projection
/// (`COLUMNS(c -> c NOT IN (targets)), (expr) AS tgt, …`) and `DuckDB`
/// binds the names in it — so the live lane is the one that must not
/// drift. `PinScope::advance` stays strictly parallel on top of this: an
/// alias-bound sibling is unpinned in both lanes.
#[test]
fn let_sibling_references_bind_column_then_alias_in_both_lanes() {
    let conn = Connection::open_in_memory().unwrap();
    let mut ft = FieldTypes::new();
    ft.insert("a", CanonicalType::BigInt);

    let event = event_with("a", Value::from(5));

    // The row carries `a`: the input column wins over the alias, so the
    // sibling reads the ORIGINAL `a`.
    run_let_cell(&conn, "* | let a = 1, b = a", &event, &ft, &["a", "b"]);
    // An overwrite does not feed the assignment beside it either.
    run_let_cell(&conn, "* | let a = a + 1, b = a", &event, &ft, &["a", "b"]);
    // The row carries no `ms`: the alias binds, in both lanes. This is
    // the ordinary shape — `let` targets are usually new names.
    run_let_cell(
        &conn,
        "* | let ms = 1000, total = ms * 2",
        &event,
        &ft,
        &["ms", "total"],
    );
    // Same, chained through a bare alias of a real column.
    run_let_cell(&conn, "* | let x = a, y = x", &event, &ft, &["x", "y"]);
    // The residual is the row-vs-relation gap, not the alias: a column
    // the corpus carries but THIS row leaves absent reads NULL in batch
    // while the live lane, seeing no key, binds the alias.
}

/// A pinned comparison reads the row under the spelling the ROW uses.
///
/// Ingest ASCII-folds every key it writes, so `where Status>400` over a
/// stored `status` needs the fold — but the pipeline lanes carry
/// user-chosen names VERBATIM (`rename status as St` keys the live event
/// `St` and names the SQL result column `"St"`), so a mixed-case alias
/// must not be folded away into a lookup that misses and drops the row.
#[test]
fn mixed_case_aliases_resolve_in_both_lanes() {
    let conn = Connection::open_in_memory().unwrap();
    let mut ft = FieldTypes::new();
    ft.insert("status", CanonicalType::Varchar);

    let event = event_with("status", Value::from("404"));

    // The ingest-written key needs the fold.
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
    // A pipeline alias is verbatim, and the pin rides along with it.
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
    // Same through the IN-list and pattern arms.
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
    // ...and through plain equality against a string literal — the shape a
    // `rename`/`let` of an envelope pin is most often read back with.
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
    // ...and the OTHER direction: a stage MAKES a mixed-case key and a
    // later stage names it in a different case. `DuckDB` binds its own
    // `"St"` column for `st`/`sT`, so the live lane must bind the row's
    // key the same way instead of missing and dropping the event.
    for dsl in [
        "* | rename status as St | where st > 400",
        "* | rename status as St | where sT > 400",
        "* | let S2 = status | where s2 > 400",
        // The source of a rename binds case-insensitively too: `Status`
        // is the ingest-folded `status` column in both lanes.
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
