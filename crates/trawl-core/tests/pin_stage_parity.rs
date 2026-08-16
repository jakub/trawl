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
use serde_json::{Map, Value, json};

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
        // SEVERITY (ADR-0013): the band form for an equality-class token,
        // the exact form for an ordered one.
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
        "SeverityText",
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
    // "Carries the column" is DuckDB's own case-insensitive binding, not
    // an exact key match: `A` names the row's `a`, so the target shadows
    // it and the sibling still reads the ORIGINAL value. Only `b` is
    // compared — the emitted exclusion list compares names as STRINGS, so
    // the `a` column survives a target spelled `A` and DuckDB
    // disambiguates the alias to `A_1`; that naming residual is the
    // pre-existing name-set divergence class, not this rule.
    run_let_cell(&conn, "* | let A = 1, b = A", &event, &ft, &["b"]);
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

/// The batch `let` projection excludes every input column whose name
/// ASCII-folds to a target's, so `| let A = 1` over a row carrying `a`
/// leaves ONE column. The live lane has to leave one too: emitting both
/// spellings puts a column in the stream the equivalent query never
/// returns, and a downstream reference binds to whichever it meets first.
///
/// This compares the whole column SET, not named outputs — the defect was
/// an extra column, which a per-name comparison cannot see.
#[test]
fn let_case_variant_target_leaves_one_column_in_both_lanes() {
    let conn = Connection::open_in_memory().unwrap();
    let ft = FieldTypes::new();

    for (dsl, event) in [
        ("* | let A = 1", json!({"a": 5, "message": "hello"})),
        ("* | let A = 1, b = A", json!({"a": 5, "message": "hello"})),
        // …and the ordinary no-collision case still keeps both columns
        ("* | let n = 1", json!({"a": 5, "message": "hello"})),
    ] {
        let event: Map<String, Value> = event.as_object().unwrap().clone();
        let query = parser::parse(dsl).expect("dsl parses");

        let plan =
            compile_stream_plan(&query.pipeline, &PinScope::root(&ft)).expect("plan compiles");
        let StreamPlan::PassThrough(mut stages) = plan else {
            panic!("{dsl:?} must compile to a per-event plan");
        };
        let mut streamed = event.clone();
        for stage in &mut stages {
            assert_eq!(apply_stage(stage, &mut streamed), StageResult::Pass);
        }

        let mut tmp = tempfile::Builder::new()
            .suffix(".ndjson")
            .tempfile()
            .unwrap();
        writeln!(tmp, "{}", Value::Object(event.clone())).unwrap();
        tmp.flush().unwrap();
        let source = tmp.path().to_str().unwrap().to_owned();
        let emitted = emitter::emit_with_pins(&query, &source, &ft).expect("emit succeeds");
        let row_sql = format!("SELECT to_json(_sub) FROM ({}) AS _sub", emitted.sql);
        let params = bind_params(&emitted.params);
        let param_refs: Vec<&dyn duckdb::ToSql> = params.iter().map(AsRef::as_ref).collect();
        let row_json: String = conn
            .query_row(&row_sql, param_refs.as_slice(), |row| row.get(0))
            .unwrap_or_else(|e| panic!("let must not error: {e}\ndsl: {dsl:?}\nsql: {row_sql}"));
        let batch: Map<String, Value> =
            serde_json::from_str(&row_json).expect("row is a JSON object");

        let batch_keys: BTreeSet<&String> = batch.keys().collect();
        let live_keys: BTreeSet<&String> = streamed.keys().collect();
        assert_eq!(
            live_keys, batch_keys,
            "column-set divergence\ndsl: {dsl:?}\nbatch: {batch:?}\nlive: {streamed:?}"
        );
    }
}

/// `count(<non-null literal>)` is the same aggregate as `count()` in both
/// lanes — SQL counts one row per input row, and the live accumulator
/// counts rows — so the live refusal of computed arguments must not
/// swallow it. Executed against `DuckDB` rather than reasoned about,
/// because "COUNT(1) counts rows" is an engine fact.
#[test]
fn count_over_a_constant_counts_rows_in_both_lanes() {
    let conn = Connection::open_in_memory().unwrap();
    let ft = FieldTypes::new();
    let rows = [json!({"a": 1}), json!({"a": 2}), json!({"a": 3})];

    let query = parser::parse("* | stats count(1)").expect("dsl parses");

    // live lane: feed every row through the compiled accumulator.
    let plan = compile_stream_plan(&query.pipeline, &PinScope::root(&ft)).expect("plan compiles");
    let StreamPlan::Aggregate {
        aggregation: mut agg,
        ..
    } = plan
    else {
        panic!("stats must compile to an aggregate plan");
    };
    for row in &rows {
        agg.feed_event(row.as_object().unwrap());
    }
    let (_columns, live_rows) = agg.snapshot();
    let live_count = live_rows
        .first()
        .and_then(|r| r.get("count"))
        .cloned()
        .unwrap_or(Value::Null);

    // batch lane: the same query over the same rows.
    let mut tmp = tempfile::Builder::new()
        .suffix(".ndjson")
        .tempfile()
        .unwrap();
    for row in &rows {
        writeln!(tmp, "{row}").unwrap();
    }
    tmp.flush().unwrap();
    let source = tmp.path().to_str().unwrap().to_owned();
    let emitted = emitter::emit_with_pins(&query, &source, &ft).expect("emit succeeds");
    let params = bind_params(&emitted.params);
    let param_refs: Vec<&dyn duckdb::ToSql> = params.iter().map(AsRef::as_ref).collect();
    let batch_count: i64 = conn
        .query_row(&emitted.sql, param_refs.as_slice(), |row| row.get(0))
        .expect("count(1) must execute");

    assert_eq!(batch_count, 3, "COUNT(1) counts every row");
    assert_eq!(
        live_count,
        Value::from(batch_count),
        "count(1) must agree across lanes"
    );
}

/// THE CLASS GUARD for mixed-case projection targets (#78 rounds 1-3).
///
/// A projection may WRITE a key the row did not have — `| let A = 1` over
/// a lowercase-keyed row — and every read after it must then agree with
/// `DuckDB`, which binds identifiers case-insensitively. Three consecutive
/// review rounds found instances of exactly one class: the live lane doing
/// an EXACT-key lookup where the batch lane binds. So this generates the
/// cross product — a case-variant projection, then a stage that reads
/// either spelling — and asserts both lanes agree, executing the batch SQL
/// rather than reasoning about it.
///
/// Two comparisons are deliberately loose, and only these two:
///
/// - column NAMES fold through `catalog_key`, because a group-by output is
///   spelled as the ROW's bound key in batch (`A`) and as the query wrote
///   it live (`a`) — one column under `DuckDB`'s own rule;
/// - VALUES compare through the live lane's own scalar rendering, because
///   this lane stringifies every group key (`"5"` where batch returns `5`)
///   REGARDLESS of case, and its numeric accumulators are `f64` where SQL
///   returns an integer (`12.0` vs `12`) — pre-existing typing
///   differences of their own, which would otherwise mask the binding
///   class this guard exists for. A missing or wrong value still fails:
///   an unbound read yields the empty string or null, never the number.
///
/// Row order is not compared: neither lane promises one for groups.
#[test]
fn case_variant_projections_agree_across_lanes() {
    let conn = Connection::open_in_memory().unwrap();
    let ft = FieldTypes::new();
    // `_time` is present because `dedup` orders by it, and the rows differ
    // only in the key column so `dedup` never has to CHOOSE between two
    // rows of one group — which lane keeps which representative is a
    // streaming-vs-batch question (live keeps the first arrival, batch the
    // most recent) and not this guard's subject. A dedup key that failed
    // to bind still fails here: it would collapse both rows into one.
    let rows = [
        json!({"a": 5, "b": "x", "message": "m", "_time": "2026-08-15T00:00:00Z"}),
        json!({"a": 7, "b": "x", "message": "m", "_time": "2026-08-15T00:00:01Z"}),
    ];

    // (projection that writes a mixed-case key, reader naming either spelling)
    let projections = ["| let A = a", "| let A = 1", "| rename a as A"];
    let readers = [
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
    ];

    for projection in projections {
        for reader in readers {
            let dsl = format!("* {projection} {reader}");
            let query = parser::parse(&dsl).expect("dsl parses");

            let live = live_rows(&query, &rows, &ft, &dsl);

            // batch lane
            let mut tmp = tempfile::Builder::new()
                .suffix(".ndjson")
                .tempfile()
                .unwrap();
            for row in &rows {
                writeln!(tmp, "{row}").unwrap();
            }
            tmp.flush().unwrap();
            let source = tmp.path().to_str().unwrap().to_owned();
            let emitted = emitter::emit_with_pins(&query, &source, &ft).expect("emit succeeds");
            let rows_sql = format!("SELECT to_json(_sub) FROM ({}) AS _sub", emitted.sql);
            let params = bind_params(&emitted.params);
            let param_refs: Vec<&dyn duckdb::ToSql> = params.iter().map(AsRef::as_ref).collect();
            let mut stmt = conn.prepare(&rows_sql).expect("sql prepares");
            let batch: Vec<Map<String, Value>> = stmt
                .query_map(param_refs.as_slice(), |row| row.get::<_, String>(0))
                .unwrap_or_else(|e| panic!("{dsl}: sql must run: {e}\n{rows_sql}"))
                .map(|r| serde_json::from_str(&r.unwrap()).unwrap())
                .collect();

            assert_eq!(
                comparable(&live),
                comparable(&batch),
                "lane divergence\ndsl: {dsl}\nlive: {live:?}\nbatch: {batch:?}"
            );
        }
    }
}

/// Run `query`'s pipeline over `rows` in the LIVE lane, whichever plan
/// shape it compiles to.
fn live_rows(
    query: &trawl_core::ast::Query,
    rows: &[Value],
    ft: &FieldTypes,
    dsl: &str,
) -> Vec<Map<String, Value>> {
    let plan = compile_stream_plan(&query.pipeline, &PinScope::root(ft))
        .unwrap_or_else(|e| panic!("{dsl}: plan must compile: {e}"));
    let feed = |stages: &mut [trawl_core::stream::CompiledStage], ev: &mut Map<String, Value>| {
        stages
            .iter_mut()
            .all(|stage| apply_stage(stage, ev) == StageResult::Pass)
    };
    match plan {
        StreamPlan::PassThrough(mut stages) => rows
            .iter()
            .filter_map(|row| {
                let mut ev: Map<String, Value> = row.as_object().unwrap().clone();
                feed(&mut stages, &mut ev).then_some(ev)
            })
            .collect(),
        StreamPlan::Aggregate {
            pre_stages: mut pre,
            aggregation: mut agg,
            ..
        } => {
            for row in rows {
                let mut ev: Map<String, Value> = row.as_object().unwrap().clone();
                if feed(&mut pre, &mut ev) {
                    agg.feed_event(&ev);
                }
            }
            agg.snapshot().1
        }
    }
}

/// One comparable shape per row: folded column name -> the live lane's own
/// scalar rendering, as an order-insensitive set.
///
/// `_time` is fixture plumbing for `dedup`'s ordering and the two lanes
/// render an instant differently by design (`DuckDB`'s TIMESTAMP text vs
/// the wire string) — a separate, documented difference, not this guard's
/// subject.
fn comparable(rows: &[Map<String, Value>]) -> BTreeSet<Vec<(String, String)>> {
    rows.iter()
        .map(|row| {
            let mut cells: Vec<(String, String)> = row
                .iter()
                .filter(|(k, _)| k.as_str() != "_time")
                .map(|(k, v)| {
                    let text = match v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    // `12.0` and `12` are one number.
                    let text = text.parse::<f64>().map_or(text, |n| n.to_string());
                    (trawl_core::schema::catalog_key(k), text)
                })
                .collect();
            cells.sort();
            cells
        })
        .collect()
}
