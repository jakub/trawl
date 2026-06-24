// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Property test: scalar `let` expressions must produce the same result
//! through `eval_scalar_fn` (streaming path) and `DuckDB` SQL (batch path).
//!
//! Generates random scalar DSL expressions (`let x = <expr>`), evaluates
//! them against a fixed event via both paths, and asserts type-aware
//! normalized equality. Excludes `now()` (nondeterministic).
//!
//! Modelled on `tests/filter_parity.rs`.

use std::io::Write;

use duckdb::Connection;
use serde_json::{Map, Value};
use trawl_core::ast::PipeStage;
use trawl_core::emitter::{self, DATE_PART_UNITS, DATE_UNITS, SqlValue};
use trawl_core::eval::{EvalValue, eval_expr, parse_timestamp, timestamp_to_duckdb_text};
use trawl_core::parser;

// ── Deterministic RNG (splitmix64, different seed from filter_parity) ─

struct Rng {
    state: u64,
}

impl Rng {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    #[allow(clippy::cast_possible_truncation)]
    fn range(&mut self, n: usize) -> usize {
        (self.next_u64() as usize) % n
    }

    fn bool(&mut self) -> bool {
        self.next_u64().is_multiple_of(2)
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.range(items.len())]
    }
}

// ── Timestamp fixtures ────────────────────────────────────────────────

const TS_VALS: &[&str] = &[
    "2026-01-15 10:20:30",
    "2026-06-01 00:00:00",
    "2026-12-31 23:59:59",
    "2000-01-01 00:00:00",
    "2024-03-15 08:45:00",
    "2023-11-07 17:30:45",
];

// Date/time unit allowlists are imported from the emitter (see `use` above) so
// the generator can never silently drift from the real allowlist: if the
// emitter grows a unit, this test exercises it automatically. `date_part`'s
// "epoch" is filtered out at generation time (float precision diverges between
// eval and the DuckDB text roundtrip) — see `random_date_part`.

// Include non-ASCII values so the parity harness exercises byte-vs-character
// divergence in scalar fns like length() (DuckDB LENGTH counts characters).
const STRING_VALS: &[&str] = &[
    "nginx",
    "error",
    "web-1",
    "hello",
    "UPPER",
    "  spaced  ",
    "café",
    "日本語",
];

// Partial strptime formats whose component fill (from the `1900-01-01 00:00:00`
// base) the streaming evaluator and DuckDB's STRPTIME agree on. Shared by the
// generative property test so each filled case is checked against real DuckDB
// every run — mirrors the deterministic `strptime_partial_formats_match_duckdb`
// table. Only formats with exact streaming/batch parity belong here.
const STRPTIME_PARTIALS: &[(&str, &str)] = &[
    ("2023", "%Y"),                   // year-only -> 2023-01-01 00:00:00
    ("2023-11", "%Y-%m"),             // year-month -> 2023-11-01 00:00:00
    ("11-07", "%m-%d"),               // bare month-day -> 1900-11-07 00:00:00
    ("2023-11-07 14", "%Y-%m-%d %H"), // date + incomplete time -> ...14:00:00
];

const INT_VALS: &[i64] = &[0, 1, 42, 100, -5, 200, 500];

// Float DSL literals spanning the DuckDB CAST(DOUBLE AS VARCHAR) presentation
// rules: integer-valued (.0 suffix), plain decimals, magnitudes that flip into
// sci-notation on render (the VALUE decides, so the literal stays plain —
// the DSL float grammar requires `digits.digits`, no `e` notation), negatives,
// and zero. Drives the F1 tostring/concat float-text parity.
const FLOAT_LITS: &[&str] = &[
    "0.0",
    "1.0",
    "2.0",
    "1.5",
    "-1.5",
    "-42.75",
    "0.1",
    "123.456",
    "100000.0",
    "10000000000000000.0", // 1e16 -> renders "1e+16"
    "15000000000000000.0", // 1.5e16 -> renders "1.5e+16"
    "0.0001",
    "0.00009999",   // 9.999e-5 -> renders "9.999e-05"
    "0.00001",      // 1e-5 -> renders "1e-05"
    "0.0000000001", // 1e-10 -> renders "1e-10"
];

// ── DSL expression generation ─────────────────────────────────────────

/// Generate a scalar DSL expression string (no `now()`, no field refs
/// that could be absent from the fixed event).
fn random_scalar_expr(rng: &mut Rng) -> Option<String> {
    match rng.range(14) {
        0 => Some(random_string_fn(rng)),
        1 => Some(random_numeric_fn(rng)),
        2 => Some(random_conditional(rng)),
        3 => Some(random_date_part(rng)),
        4 => Some(random_date_trunc(rng)),
        5 => Some(random_date_diff(rng)),
        6 => Some(random_strftime(rng)),
        7 => Some(random_strptime(rng)),
        8 => Some(random_tonumber(rng)),
        9 => Some(random_tostring(rng)),
        10 => Some(random_typeof(rng)),
        11 => Some(random_coalesce(rng)),
        12 => Some(random_concat(rng)),
        13 => Some(random_substr(rng)),
        _ => unreachable!(),
    }
}

fn ts_lit(rng: &mut Rng) -> String {
    let ts = rng.pick(TS_VALS);
    format!("strptime(\"{ts}\", \"%Y-%m-%d %H:%M:%S\")")
}

fn str_lit(rng: &mut Rng) -> String {
    let s = rng.pick(STRING_VALS);
    format!("\"{s}\"")
}

fn int_lit(rng: &mut Rng) -> String {
    rng.pick(INT_VALS).to_string()
}

fn float_lit(rng: &mut Rng) -> String {
    (*rng.pick(FLOAT_LITS)).to_string()
}

fn random_string_fn(rng: &mut Rng) -> String {
    match rng.range(8) {
        0 => format!("lower({})", str_lit(rng)),
        1 => format!("upper({})", str_lit(rng)),
        2 => format!("length({})", str_lit(rng)),
        3 => format!("trim({})", str_lit(rng)),
        4 => format!("ltrim({})", str_lit(rng)),
        5 => format!("rtrim({})", str_lit(rng)),
        6 => format!("contains({}, {})", str_lit(rng), str_lit(rng)),
        7 => format!(
            "replace({}, {}, {})",
            str_lit(rng),
            str_lit(rng),
            str_lit(rng)
        ),
        _ => unreachable!(),
    }
}

fn random_numeric_fn(rng: &mut Rng) -> String {
    let i = int_lit(rng);
    match rng.range(4) {
        0 => format!("abs({i})"),
        1 => format!("ceil({i})"),
        2 => format!("floor({i})"),
        3 => format!("round({i})"),
        _ => unreachable!(),
    }
}

fn random_conditional(rng: &mut Rng) -> String {
    let cond = if rng.bool() { "true" } else { "false" };
    let a = str_lit(rng);
    let b = str_lit(rng);
    format!("if({cond}, {a}, {b})")
}

fn random_date_part(rng: &mut Rng) -> String {
    // Exclude "epoch": its float result diverges between eval and the DuckDB
    // text roundtrip. Everything else in the emitter allowlist is fair game,
    // so a newly-added unit flows in here without a test edit.
    let units: Vec<&&str> = DATE_PART_UNITS.iter().filter(|u| **u != "epoch").collect();
    let unit = rng.pick(&units);
    let ts = ts_lit(rng);
    format!("date_part(\"{unit}\", {ts})")
}

fn random_date_trunc(rng: &mut Rng) -> String {
    let unit = rng.pick(DATE_UNITS);
    let ts = ts_lit(rng);
    format!("date_trunc(\"{unit}\", {ts})")
}

fn random_date_diff(rng: &mut Rng) -> String {
    let unit = rng.pick(DATE_UNITS);
    let start = ts_lit(rng);
    let end = ts_lit(rng);
    format!("date_diff(\"{unit}\", {start}, {end})")
}

fn random_strftime(rng: &mut Rng) -> String {
    let ts = ts_lit(rng);
    // Only test C-strftime formats chrono and DuckDB agree on
    let fmt = rng.pick(&["%Y-%m-%d", "%H:%M:%S", "%Y-%m-%d %H:%M:%S", "%m/%d/%Y"]);
    format!("strftime({ts}, \"{fmt}\")")
}

fn random_strptime(rng: &mut Rng) -> String {
    if rng.range(4) == 0 {
        // Unparseable input against a valid format: batch (TRY_STRPTIME) and
        // streaming both yield NULL, so the two paths agree on a data-parse
        // failure (TRY_STRPTIME nulls instead of erroring the whole query).
        return "strptime(\"not-a-date\", \"%Y-%m-%d %H:%M:%S\")".to_string();
    }
    if rng.range(4) == 0 {
        // Partial format: streaming fills omitted components from the
        // 1900-01-01 00:00:00 base exactly like DuckDB. Render through strftime
        // so the filled timestamp is compared as a string.
        let (input, fmt) = rng.pick(STRPTIME_PARTIALS);
        return format!("strftime(strptime(\"{input}\", \"{fmt}\"), \"%Y-%m-%d %H:%M:%S\")");
    }
    let ts = rng.pick(TS_VALS);
    // strptime returns a Timestamp — wrap it in strftime to get a string for comparison
    format!("strftime(strptime(\"{ts}\", \"%Y-%m-%d %H:%M:%S\"), \"%Y-%m-%d %H:%M:%S\")")
}

fn random_tonumber(rng: &mut Rng) -> String {
    if rng.bool() {
        // Digit-separator strings: `1_000`/`1_0.0_5` parse to a value in BOTH
        // paths; `_1000` is unparseable in both (DuckDB returns NULL -> skipped).
        // Proves the underscore-strip rule stays in DuckDB TRY_CAST parity.
        let s = rng.pick(&["1_000", "1_0.0_5", "_1000"]);
        format!("tonumber(\"{s}\")")
    } else {
        // Use integer string literals so the result is exact
        let n = rng.pick(INT_VALS).unsigned_abs();
        format!("tonumber(\"{n}\")")
    }
}

fn random_tostring(rng: &mut Rng) -> String {
    // Mix ints AND floats: float text rendering (1.0 -> "1.0", 1e16 -> "1e+16")
    // is the F1 divergence this exercises. The String arm of values_match does
    // an exact compare, so DuckDB CAST(DOUBLE AS VARCHAR) must byte-match eval.
    if rng.bool() {
        format!("tostring({})", int_lit(rng))
    } else {
        format!("tostring({})", float_lit(rng))
    }
}

/// `substr(s, start [, len])` with negative/zero starts and out-of-range
/// lengths — the F2 window-semantics blind spot. start in -8..=8, optional
/// len in -8..=8 (incl. 0). Closes the gap that let `DuckDB`'s from-end /
/// leftward-window behaviour drift from the streaming evaluator.
fn random_substr(rng: &mut Rng) -> String {
    let s = str_lit(rng);
    #[allow(clippy::cast_possible_wrap)]
    let start = rng.range(17) as i64 - 8; // -8..=8
    if rng.bool() {
        format!("substr({s}, {start})")
    } else {
        #[allow(clippy::cast_possible_wrap)]
        let len = rng.range(17) as i64 - 8; // -8..=8
        format!("substr({s}, {start}, {len})")
    }
}

fn random_typeof(rng: &mut Rng) -> String {
    // typeof on strings only: DuckDB returns BIGINT for integer literals (it
    // widens all integer literals to BIGINT) while eval returns INTEGER.
    // This is a documented eval/DuckDB divergence for typeof — only test
    // varchar inputs where both agree on "VARCHAR".
    format!("typeof({})", str_lit(rng))
}

fn random_coalesce(rng: &mut Rng) -> String {
    let a = str_lit(rng);
    let b = str_lit(rng);
    format!("coalesce({a}, {b})")
}

/// 2–4 concat args, each a string/int literal or a bare `null`. Exercises
/// `DuckDB`'s CONCAT NULL-skipping and CAST-to-VARCHAR join against the
/// streaming evaluator (the #22 batch-vs-live drift this fix closes).
/// (Floats omitted: `DuckDB` float→text rendering differs from Rust's.)
fn random_concat(rng: &mut Rng) -> String {
    let n = 2 + rng.range(3); // 2..=4 args
    let args: Vec<String> = (0..n)
        .map(|_| match rng.range(3) {
            0 => str_lit(rng),
            1 => int_lit(rng),
            2 => "null".to_string(),
            _ => unreachable!(),
        })
        .collect();
    format!("concat({})", args.join(", "))
}

// ── Event fixture (all fields present to avoid binder errors) ─────────

fn fixed_event() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("service".into(), Value::String("nginx".into()));
    m.insert("level".into(), Value::String("error".into()));
    m.insert("status".into(), Value::Number(200.into()));
    m.insert("host".into(), Value::String("web-1".into()));
    m
}

// ── SQL execution ──────────────────────────────────────────────────────

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

/// Outcome of running a `let x = <expr>` query through the batch (`DuckDB`) path.
#[derive(Debug, PartialEq)]
enum SqlOutcome {
    /// `DuckDB` produced a value for `x` (including a SQL `NULL` -> `Value::Null`).
    Value(Value),
    /// `DuckDB` raised an error (prepare/query failed). The streaming evaluator
    /// cannot error, so its contract is to yield `Null` wherever batch errors —
    /// the property loop asserts that rather than silently skipping.
    Errored,
    /// Not a `DuckDB`-comparison case (parse/emit failure, empty result, or a
    /// column type the harness can't faithfully read back).
    Skip,
}

/// Run a `let x = <expr>` query and return the outcome of the computed `x`
/// column. A `DuckDB` error is surfaced as [`SqlOutcome::Errored`] (NOT skipped),
/// so the harness can no longer hide a batch error behind a silent skip.
fn sql_scalar_result(conn: &Connection, dsl: &str, event: &Map<String, Value>) -> SqlOutcome {
    let Ok(query) = parser::parse(dsl) else {
        return SqlOutcome::Skip;
    };

    let Ok(mut tmp) = tempfile::Builder::new().suffix(".ndjson").tempfile() else {
        return SqlOutcome::Skip;
    };
    let ev = Value::Object(event.clone());
    if writeln!(tmp, "{ev}").and_then(|()| tmp.flush()).is_err() {
        return SqlOutcome::Skip;
    }
    let Some(tmp_path) = tmp.path().to_str() else {
        return SqlOutcome::Skip;
    };

    let Ok(emitted) = emitter::emit(&query, tmp_path) else {
        return SqlOutcome::Skip;
    };

    let params = bind_params(&emitted.params);
    let param_refs: Vec<&dyn duckdb::ToSql> = params.iter().map(AsRef::as_ref).collect();

    // A prepare/query failure is a real DuckDB ERROR, not a skip.
    let Ok(mut stmt) = conn.prepare(&emitted.sql) else {
        return SqlOutcome::Errored;
    };
    let Ok(mut rows) = stmt.query(param_refs.as_slice()) else {
        return SqlOutcome::Errored;
    };
    let Ok(Some(row)) = rows.next() else {
        // Empty result set: nothing to compare.
        return SqlOutcome::Skip;
    };

    // Find the `x` column index, use ValueRef to inspect the DuckDB type
    // so we don't mistakenly cast VARCHAR "500" → i64 500.
    let ncols = row.as_ref().column_count();
    for col_idx in (0..ncols).rev() {
        let Ok(name) = row.as_ref().column_name(col_idx) else {
            return SqlOutcome::Skip;
        };
        if name == "x" {
            use duckdb::types::ValueRef;
            let Ok(vr) = row.get_ref(col_idx) else {
                return SqlOutcome::Skip;
            };
            let val = match vr {
                ValueRef::Null => Value::Null,
                ValueRef::Boolean(b) => Value::Bool(b),
                ValueRef::TinyInt(n) => Value::Number(i64::from(n).into()),
                ValueRef::SmallInt(n) => Value::Number(i64::from(n).into()),
                ValueRef::Int(n) => Value::Number(i64::from(n).into()),
                ValueRef::BigInt(n) => Value::Number(n.into()),
                ValueRef::HugeInt(n) => match i64::try_from(n) {
                    Ok(n) => Value::Number(n.into()),
                    Err(_) => return SqlOutcome::Skip,
                },
                ValueRef::UTinyInt(n) => Value::Number(i64::from(n).into()),
                ValueRef::USmallInt(n) => Value::Number(i64::from(n).into()),
                ValueRef::UInt(n) => Value::Number(i64::from(n).into()),
                ValueRef::UBigInt(n) => match i64::try_from(n) {
                    Ok(n) => Value::Number(n.into()),
                    Err(_) => return SqlOutcome::Skip,
                },
                ValueRef::Float(f) => {
                    serde_json::Number::from_f64(f64::from(f)).map_or(Value::Null, Value::Number)
                }
                ValueRef::Double(f) => {
                    serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number)
                }
                ValueRef::Text(bytes) => match std::str::from_utf8(bytes) {
                    Ok(s) => Value::String(s.to_string()),
                    Err(_) => return SqlOutcome::Skip,
                },
                ValueRef::Blob(_) => return SqlOutcome::Skip, // skip binary
                // Timestamp-like values: render as string via Display
                _ => match row.get::<_, String>(col_idx) {
                    Ok(s) => Value::String(s),
                    Err(_) => return SqlOutcome::Skip,
                },
            };
            return SqlOutcome::Value(val);
        }
    }
    SqlOutcome::Skip
}

// ── Value comparison ──────────────────────────────────────────────────

/// Normalize `EvalValue` to a `Value` for comparison.
fn normalize_eval(v: &EvalValue) -> Value {
    match v {
        EvalValue::Bool(b) => Value::Bool(*b),
        EvalValue::Int(n) => Value::Number((*n).into()),
        EvalValue::Float(n) => serde_json::Number::from_f64(*n).map_or(Value::Null, Value::Number),
        EvalValue::Str(s) => Value::String(s.clone()),
        EvalValue::Timestamp(ts) => Value::String(timestamp_to_duckdb_text(ts)),
        // Null and Array are both unmatchable in this test
        EvalValue::Null | EvalValue::Array(_) => Value::Null,
    }
}

/// Compare eval result vs `DuckDB` result with type-aware normalization.
///
/// - `Null` vs `Null` → match
/// - Timestamp strings compared as `NaiveDateTime` (ignoring sub-second
///   precision differences from `DuckDB` text rendering)
/// - Numbers compared with epsilon tolerance for floats
/// - Booleans: `DuckDB` returns bools as strings (true/false) or bool values
fn values_match(eval_val: &Value, sql_val: &Value) -> bool {
    match (eval_val, sql_val) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Bool(a), Value::String(b)) => a.to_string() == *b,
        (Value::String(a), Value::Bool(b)) => *a == b.to_string(),
        (Value::Number(a), Value::Number(b)) => {
            match (a.as_i64(), b.as_i64()) {
                (Some(ai), Some(bi)) => ai == bi,
                _ => {
                    match (a.as_f64(), b.as_f64()) {
                        (Some(af), Some(bf)) => {
                            // epsilon comparison for floats
                            (af - bf).abs() < 1e-6_f64.max(af.abs() * 1e-9)
                        }
                        _ => false,
                    }
                }
            }
        }
        (Value::Number(a), Value::String(b)) => {
            // DuckDB may return numeric types as strings
            if let Ok(bf) = b.parse::<f64>() {
                a.as_f64().is_some_and(|af| (af - bf).abs() < 1e-6)
            } else {
                false
            }
        }
        (Value::String(a), Value::String(b)) => {
            // Try timestamp comparison first (ignores sub-second diff).
            // Truncate to whole seconds via unix timestamp integer comparison
            // to avoid importing Timelike/Datelike traits in this test file.
            if let (Some(ta), Some(tb)) = (parse_timestamp(a), parse_timestamp(b)) {
                ta.and_utc().timestamp() == tb.and_utc().timestamp()
            } else {
                a == b
            }
        }
        // DuckDB TYPEOF returns VARCHAR; eval returns Str. Already handled above.
        _ => false,
    }
}

// ── Property test ──────────────────────────────────────────────────────

#[test]
fn scalar_eval_matches_sql_parity() {
    let conn = Connection::open_in_memory().unwrap();
    let mut rng = Rng::new(0xCAFE_BABE);
    let event = fixed_event();
    let mut passed = 0u32;
    let mut skipped = 0u32;

    for i in 0..500 {
        let Some(expr_str) = random_scalar_expr(&mut rng) else {
            skipped += 1;
            continue;
        };

        let dsl = format!("* | let x = {expr_str}");

        // ── Streaming path (eval_expr) ── computed first so it is available to
        // assert against a DuckDB error (the streaming contract is: eval can't
        // error, so it must yield Null wherever batch errors).
        let Ok(query) = parser::parse(&dsl) else {
            skipped += 1;
            continue;
        };

        // Extract the first assignment's RHS expression from the let stage.
        let let_stage = query.pipeline.iter().find_map(|s| {
            if let PipeStage::Let(ls) = &s.node {
                Some(ls)
            } else {
                None
            }
        });
        let Some(ls) = let_stage else {
            skipped += 1;
            continue;
        };
        let Some((_, expr)) = ls.assignments.first() else {
            skipped += 1;
            continue;
        };

        let eval_result = eval_expr(expr, &event);
        let eval_normalized = normalize_eval(&eval_result);

        // ── Batch path (DuckDB) ──
        match sql_scalar_result(&conn, &dsl, &event) {
            SqlOutcome::Skip => {
                skipped += 1;
                continue;
            }
            SqlOutcome::Errored => {
                // The harness no longer hides batch errors: assert eval also
                // yields Null. A non-Null eval here is a genuine divergence.
                assert_eq!(
                    eval_normalized,
                    Value::Null,
                    "BATCH ERRORED but streaming did NOT null (iteration {i})\n\
                     dsl: {dsl:?}\n\
                     eval: {eval_normalized:?}"
                );
                passed += 1;
                continue;
            }
            SqlOutcome::Value(sql_result) => {
                assert!(
                    values_match(&eval_normalized, &sql_result),
                    "SCALAR PARITY MISMATCH (iteration {i})\n\
                     dsl: {dsl:?}\n\
                     eval: {eval_normalized:?}\n\
                     sql:  {sql_result:?}"
                );
            }
        }

        passed += 1;
    }

    eprintln!("scalar parity: {passed} passed, {skipped} skipped");
    assert!(
        passed >= 350,
        "too many skipped iterations: {skipped} skipped, {passed} passed"
    );
}

/// The harness must surface a `DuckDB` error as [`SqlOutcome::Errored`] (not a
/// silent skip), and the streaming contract is that eval yields `Null` wherever
/// batch errors. These type-mismatch expressions error in the `DuckDB` binder;
/// eval — being permissive — nulls. This proves both halves: the harness no longer
/// hides batch errors, and the streaming path honours the null-where-batch-errors
/// contract for these cases.
#[test]
fn batch_errors_imply_streaming_null() {
    let conn = Connection::open_in_memory().unwrap();
    let event = fixed_event();
    for dsl in [
        r#"* | let x = replace("abc", "a", 5)"#, // wrong-type replace arg
        "* | let x = service + 1",               // varchar + int
        r#"* | let x = round("abc")"#,           // round of varchar
    ] {
        assert_eq!(
            sql_scalar_result(&conn, dsl, &event),
            SqlOutcome::Errored,
            "expected DuckDB to error for {dsl:?}"
        );

        let query = parser::parse(dsl).unwrap();
        let ls = query
            .pipeline
            .iter()
            .find_map(|s| {
                if let PipeStage::Let(ls) = &s.node {
                    Some(ls)
                } else {
                    None
                }
            })
            .unwrap();
        let (_, expr) = ls.assignments.first().unwrap();
        assert_eq!(
            normalize_eval(&eval_expr(expr, &event)),
            Value::Null,
            "streaming eval must yield Null where batch errors for {dsl:?}"
        );
    }
}

/// Regression: `strftime` over a nested `strptime` with literal args.
///
/// Both args carry bound params, so the old emitter text-swap (`a[1], a[0]`)
/// misbound the `?` placeholders against `emit_expr`'s DSL-order param push,
/// yielding NULL/error in batch. Emitting in DSL order keeps them aligned;
/// both paths must produce `"2023"`.
#[test]
fn strftime_over_strptime_binds_params_in_order() {
    let conn = Connection::open_in_memory().unwrap();
    let event = fixed_event();
    // Full datetime so chrono's NaiveDateTime::parse_from_str succeeds (date-only
    // formats are a separate chrono-vs-DuckDB divergence, not the param bug here).
    let dsl = r#"* | let x = strftime(strptime("2023-11-07 17:30:45", "%Y-%m-%d %H:%M:%S"), "%Y")"#;

    // Batch path (real DuckDB).
    assert_eq!(
        sql_scalar_result(&conn, dsl, &event),
        SqlOutcome::Value(Value::String("2023".to_string())),
        "batch strftime(strptime(...)) must not error or null"
    );

    // Streaming path (eval).
    let query = parser::parse(dsl).unwrap();
    let ls = query
        .pipeline
        .iter()
        .find_map(|s| {
            if let PipeStage::Let(ls) = &s.node {
                Some(ls)
            } else {
                None
            }
        })
        .unwrap();
    let (_, expr) = ls.assignments.first().unwrap();
    let eval_result = eval_expr(expr, &event);
    assert_eq!(
        normalize_eval(&eval_result),
        Value::String("2023".to_string())
    );
}

/// Regression: `strptime` with a PARTIAL format must fill components the same way
/// `DuckDB` does — a date-only format yields `00:00:00`, a time-only format yields
/// the `1900-01-01` base. Before the cascade in `eval_strptime`, chrono's
/// `NaiveDateTime::parse_from_str` rejected both (it needs a full datetime), so
/// streaming silently nulled while batch returned a timestamp. Each case is
/// rendered back through `strftime` so we compare the FILLED value against real
/// `DuckDB`.
#[test]
fn strptime_partial_formats_match_duckdb() {
    let conn = Connection::open_in_memory().unwrap();
    let event = fixed_event();
    for (dsl, want) in [
        (
            // date-only -> midnight
            r#"* | let x = strftime(strptime("2023-11-07", "%Y-%m-%d"), "%Y-%m-%d %H:%M:%S")"#,
            "2023-11-07 00:00:00",
        ),
        (
            // time-only -> 1900-01-01 base
            r#"* | let x = strftime(strptime("14:30:00", "%H:%M:%S"), "%Y-%m-%d %H:%M:%S")"#,
            "1900-01-01 14:30:00",
        ),
        (
            // year-only -> month/day fill to 1, time to midnight
            r#"* | let x = strftime(strptime("2023", "%Y"), "%Y-%m-%d %H:%M:%S")"#,
            "2023-01-01 00:00:00",
        ),
        (
            // year-month -> day fills to 1
            r#"* | let x = strftime(strptime("2023-11", "%Y-%m"), "%Y-%m-%d %H:%M:%S")"#,
            "2023-11-01 00:00:00",
        ),
        (
            // bare month-day -> year fills to the 1900 base
            r#"* | let x = strftime(strptime("11-07", "%m-%d"), "%Y-%m-%d %H:%M:%S")"#,
            "1900-11-07 00:00:00",
        ),
        (
            // date + INCOMPLETE time -> minute/second fill to 0 (not dropped)
            r#"* | let x = strftime(strptime("2023-11-07 14", "%Y-%m-%d %H"), "%Y-%m-%d %H:%M:%S")"#,
            "2023-11-07 14:00:00",
        ),
    ] {
        assert_eq!(
            sql_scalar_result(&conn, dsl, &event),
            SqlOutcome::Value(Value::String(want.to_string())),
            "batch strptime partial-format must fill like DuckDB for {dsl:?}"
        );

        let query = parser::parse(dsl).unwrap();
        let ls = query
            .pipeline
            .iter()
            .find_map(|s| {
                if let PipeStage::Let(ls) = &s.node {
                    Some(ls)
                } else {
                    None
                }
            })
            .unwrap();
        let (_, expr) = ls.assignments.first().unwrap();
        assert_eq!(
            normalize_eval(&eval_expr(expr, &event)),
            Value::String(want.to_string()),
            "streaming strptime partial-format must match DuckDB for {dsl:?}"
        );
    }
}
