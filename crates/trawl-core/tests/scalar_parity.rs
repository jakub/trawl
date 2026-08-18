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
use duckdb::types::{Type, Value as DuckValue};
use serde_json::{Map, Value};
use trawl_core::ast::PipeStage;
use trawl_core::emitter::{self, DATE_PART_UNITS, DATE_UNITS, SqlValue};
use trawl_core::eval::{EvalValue, eval_expr};
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
    match rng.range(15) {
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
        14 => Some(random_sev(rng)),
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
    match rng.range(2) {
        0 => format!("abs({i})"),
        1 => format!("round({i})"),
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

/// Severity texts spanning the reading kernel's rungs: band tokens with
/// their case and whitespace variants, an exact `OTel` short name, the
/// numeric strings both dialects read differently, the spellings
/// `TRY_CAST` reads and the kernel does not (`1.5`, `1e1`, `0x10`), and
/// values with no reading at all.
const SEV_TEXTS: &[&str] = &[
    "error",
    "ERR",
    " error ",
    "error2",
    "warn",
    "0",
    "1",
    "7",
    "8",
    "17",
    "24",
    "25",
    "0404",
    "007",
    "+17",
    "1.5",
    "1e1",
    "0x10",
    "gold",
    "",
    // Unicode case folding: `DuckDB`'s `lower()` folds these onto token
    // letters and the kernel's ASCII fold does not, so an ungated token
    // match read them as severities in batch alone.
    "\u{130}NFO",
    "\u{131}",
    "\u{212a}",
    "\u{ff29}\u{ff2e}\u{ff26}\u{ff2f}",
    "\u{ff11}\u{ff17}",
];

/// `sev(x[, dialect])` over literals AND over the event's own columns —
/// the SQL lane reads the column's TEXT form while eval reads the wire
/// JSON, so a field arm is the one that proves the two readings agree.
fn random_sev(rng: &mut Rng) -> String {
    let arg = match rng.range(5) {
        0 => format!("\"{}\"", rng.pick(SEV_TEXTS)),
        1 => rng.pick(&[0_i64, 1, 3, 7, 8, 17, 24, 25, -1]).to_string(),
        2 => "level".to_string(),
        3 => "sev_num".to_string(),
        4 => "status".to_string(),
        _ => unreachable!(),
    };
    match rng.range(3) {
        0 => format!("sev({arg})"),
        1 => format!("sev({arg}, \"otel\")"),
        2 => format!("sev({arg}, \"syslog\")"),
        _ => unreachable!(),
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
    m.insert(
        "event_ts".into(),
        Value::String("2024-12-30 23:05:07.123456".into()),
    );
    m.insert(
        "event_ts_far".into(),
        Value::String("9999-12-31 23:59:59.000016".into()),
    );
    // A severity-bearing pair, one word and one numeral, so `sev()` reads
    // a real column in both shapes.
    m.insert("sev_num".into(), Value::Number(3.into()));
    m.insert("sev_word".into(), Value::String("warn".into()));
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
    /// `DuckDB` produced a value for `x`, with its declared logical type.
    Value(SqlCell),
    /// `DuckDB` raised an error (prepare/query failed). The streaming evaluator
    /// cannot error, so its contract is to yield `Null` wherever batch errors —
    /// the property loop asserts that rather than silently skipping.
    Errored,
    /// The only value shapes the harness deliberately cannot read faithfully.
    Skip(SkipCategory),
}

#[derive(Debug, PartialEq)]
struct SqlCell {
    logical_type: Type,
    value: DuckValue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkipCategory {
    HugeIntOutsideI64,
    Blob,
}

fn infra_or_panic<T, E: std::fmt::Display>(result: Result<T, E>, operation: &str) -> T {
    result.unwrap_or_else(|error| panic!("{operation} infrastructure failure: {error}"))
}

fn parse_generated(dsl: &str) -> trawl_core::ast::Query {
    parser::parse(dsl)
        .unwrap_or_else(|error| panic!("generated grammar rejected {dsl:?}: {error:?}"))
}

fn utc_connection() -> Connection {
    let conn = infra_or_panic(Connection::open_in_memory(), "DuckDB connection");
    infra_or_panic(
        conn.execute_batch(trawl_core::conform::SESSION_TIME_ZONE_SQL),
        "DuckDB UTC session setup",
    );
    conn
}

fn eval_scalar(dsl: &str, event: &Map<String, Value>) -> EvalValue {
    let query = parse_generated(dsl);
    let let_stage = query
        .pipeline
        .iter()
        .find_map(|stage| {
            if let PipeStage::Let(let_stage) = &stage.node {
                Some(let_stage)
            } else {
                None
            }
        })
        .unwrap_or_else(|| panic!("generated scalar query has no let stage: {dsl:?}"));
    let (_, expr) = let_stage
        .assignments
        .first()
        .unwrap_or_else(|| panic!("generated scalar query has no assignment: {dsl:?}"));
    eval_expr(expr, event)
}

/// Run a `let x = <expr>` query and return the outcome of the computed `x`
/// column. A `DuckDB` error is surfaced as [`SqlOutcome::Errored`] (NOT skipped),
/// so the harness can no longer hide a batch error behind a silent skip.
fn sql_scalar_result(conn: &Connection, dsl: &str, event: &Map<String, Value>) -> SqlOutcome {
    let query = parse_generated(dsl);

    let mut tmp = infra_or_panic(
        tempfile::Builder::new().suffix(".ndjson").tempfile(),
        "tempfile creation",
    );
    let ev = Value::Object(event.clone());
    infra_or_panic(
        writeln!(tmp, "{ev}").and_then(|()| tmp.flush()),
        "fixture write",
    );
    let tmp_path = tmp
        .path()
        .to_str()
        .expect("fixture path infrastructure failure: path is not UTF-8");

    let emitted = emitter::emit(&query, tmp_path)
        .unwrap_or_else(|error| panic!("generated grammar failed to emit {dsl:?}: {error:?}"));

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
        panic!("query infrastructure failure: generated scalar query returned no row: {dsl:?}");
    };

    // Find the `x` column index, use ValueRef to inspect the DuckDB type
    // so we don't mistakenly cast VARCHAR "500" → i64 500.
    let ncols = row.as_ref().column_count();
    for col_idx in (0..ncols).rev() {
        let name = infra_or_panic(row.as_ref().column_name(col_idx), "column-name read");
        if name == "x" {
            use duckdb::types::ValueRef;
            let vr = infra_or_panic(row.get_ref(col_idx), "column read");
            if matches!(vr, ValueRef::Blob(_)) {
                return SqlOutcome::Skip(SkipCategory::Blob);
            }
            if let ValueRef::HugeInt(n) = vr
                && i64::try_from(n).is_err()
            {
                return SqlOutcome::Skip(SkipCategory::HugeIntOutsideI64);
            }
            if let ValueRef::Text(bytes) = vr {
                std::str::from_utf8(bytes)
                    .expect("column UTF-8 infrastructure failure: DuckDB returned invalid text");
            }
            let logical_type = Type::from(&row.as_ref().column_type(col_idx));
            return SqlOutcome::Value(SqlCell {
                logical_type,
                value: vr.to_owned(),
            });
        }
    }
    panic!("query infrastructure failure: generated scalar query has no x column: {dsl:?}")
}

// ── Value comparison ──────────────────────────────────────────────────

/// Explicitly permitted representation widenings between streaming and batch.
/// There is deliberately no string/numeric coercion and no timestamp/text arm.
fn logical_type_matches(eval: &EvalValue, sql: &Type) -> bool {
    match eval {
        EvalValue::Null => true,
        EvalValue::Bool(_) => matches!(sql, Type::Boolean),
        EvalValue::Int(_) => matches!(
            sql,
            Type::TinyInt
                | Type::SmallInt
                | Type::Int
                | Type::BigInt
                | Type::HugeInt
                | Type::UTinyInt
                | Type::USmallInt
                | Type::UInt
                | Type::UBigInt
        ),
        EvalValue::Float(_) => matches!(sql, Type::Float | Type::Double),
        EvalValue::Str(_) => matches!(sql, Type::Text),
        EvalValue::Array(_) => matches!(sql, Type::List(_) | Type::Array(_, _)),
        EvalValue::Timestamp(_) => matches!(sql, Type::Timestamp),
    }
}

fn integer_value(value: &DuckValue) -> Option<i64> {
    match value {
        DuckValue::TinyInt(n) => Some(i64::from(*n)),
        DuckValue::SmallInt(n) => Some(i64::from(*n)),
        DuckValue::Int(n) => Some(i64::from(*n)),
        DuckValue::BigInt(n) => Some(*n),
        DuckValue::HugeInt(n) => i64::try_from(*n).ok(),
        DuckValue::UTinyInt(n) => Some(i64::from(*n)),
        DuckValue::USmallInt(n) => Some(i64::from(*n)),
        DuckValue::UInt(n) => Some(i64::from(*n)),
        DuckValue::UBigInt(n) => i64::try_from(*n).ok(),
        _ => None,
    }
}

fn scalar_values_match(eval: &EvalValue, sql: &DuckValue) -> bool {
    match (eval, sql) {
        (EvalValue::Null, DuckValue::Null) => true,
        (EvalValue::Bool(a), DuckValue::Boolean(b)) => a == b,
        (EvalValue::Int(a), value) => integer_value(value) == Some(*a),
        (EvalValue::Float(a), DuckValue::Float(b)) => a.to_bits() == f64::from(*b).to_bits(),
        (EvalValue::Float(a), DuckValue::Double(b)) => a.to_bits() == b.to_bits(),
        (EvalValue::Str(a), DuckValue::Text(b)) => a == b,
        (EvalValue::Timestamp(a), DuckValue::Timestamp(unit, b)) => {
            a.and_utc().timestamp_subsec_nanos().is_multiple_of(1_000)
                && a.and_utc().timestamp_micros() == unit.to_micros(*b)
        }
        (EvalValue::Array(a), DuckValue::List(b) | DuckValue::Array(b)) => {
            a.len() == b.len()
                && a.iter()
                    .zip(b)
                    .all(|(eval_item, sql_item)| scalar_values_match(eval_item, sql_item))
        }
        _ => false,
    }
}

fn values_match(eval: &EvalValue, sql: &SqlCell) -> bool {
    logical_type_matches(eval, &sql.logical_type) && scalar_values_match(eval, &sql.value)
}

fn normalize_eval(v: &EvalValue) -> Value {
    v.clone().into()
}

fn assert_parity_case(
    conn: &Connection,
    event: &Map<String, Value>,
    label: &str,
    expression: &str,
) -> Option<SkipCategory> {
    let dsl = format!("* | let x = {expression}");
    let eval_result = eval_scalar(&dsl, event);
    match sql_scalar_result(conn, &dsl, event) {
        SqlOutcome::Value(sql_result) => {
            assert!(
                values_match(&eval_result, &sql_result),
                "SCALAR PARITY MISMATCH ({label})\n\
                 dsl: {dsl:?}\n\
                 eval: {eval_result:?}\n\
                 sql:  {sql_result:?}"
            );
            None
        }
        SqlOutcome::Errored => {
            assert_eq!(
                eval_result,
                EvalValue::Null,
                "BATCH ERRORED but streaming did NOT null ({label})\n\
                 dsl: {dsl:?}\n\
                 eval: {eval_result:?}"
            );
            None
        }
        SqlOutcome::Skip(category) => Some(category),
    }
}

#[derive(Clone, Copy)]
struct GeneratedCase {
    family: &'static str,
    expression: &'static str,
}

const REQUIRED_GENERATED_FAMILIES: &[&str] = &[
    "operators",
    "numeric",
    "coalesce",
    "conditional",
    "strftime",
    "date_part",
    "timestamp_comparison",
    "json",
    "tonumber",
    "tostring",
];

const REQUIRED_FAMILY_CASE_COUNTS: &[(&str, usize)] = &[
    ("operators", 22),
    ("numeric", 7),
    ("coalesce", 5),
    ("conditional", 4),
    ("strftime", 4),
    ("date_part", 8),
    ("timestamp_comparison", 4),
    ("json", 7),
    ("tonumber", 7),
    ("tostring", 6),
];

#[allow(clippy::too_many_lines)]
fn generated_cases() -> Vec<GeneratedCase> {
    let mut cases = Vec::new();
    let mut add = |family, expressions: &[&'static str]| {
        cases.extend(
            expressions
                .iter()
                .map(|expression| GeneratedCase { family, expression }),
        );
    };

    add(
        "operators",
        &[
            "1 + 2",
            "-5 - -2",
            "3 * -2",
            "9223372036854775806 + 1",
            "-9223372036854775807 - 1",
            "1 + 2.5",
            "-5.0 / 2",
            "5 / 2.0",
            "-5 % 2",
            "5 % -2",
            "-5 % -2",
            "1 == 1",
            "1 != 2",
            "1 < 2.0",
            "2 <= 2",
            "3 > 2",
            "3 >= 3.0",
            "true and false",
            "true or false",
            "true and null",
            "false or null",
            "not false",
        ],
    );
    add(
        "numeric",
        &[
            "abs(-5)",
            "abs(0)",
            "abs(-1.5)",
            "round(-5)",
            "round(0)",
            "round(-1.25, 1)",
            "round(1.25, 1)",
        ],
    );
    add(
        "coalesce",
        &[
            "coalesce(null, 1)",
            "coalesce(1, null, 2)",
            "coalesce(null, null, null)",
            "coalesce(null, 1.5, 2)",
            "coalesce(null, null, \"fallback\")",
        ],
    );
    add(
        "conditional",
        &[
            "if(1, \"yes\", \"no\")",
            "if(0, \"yes\", \"no\")",
            "case(1, \"yes\", \"no\")",
            "case(0, \"yes\", \"no\")",
        ],
    );
    add(
        "strftime",
        &[
            r#"strftime(strptime("2024-12-30 23:05:07.123456", "%Y-%m-%d %H:%M:%S.%f"), "%j")"#,
            r#"strftime(strptime("2024-12-30 23:05:07", "%Y-%m-%d %H:%M:%S"), "%U")"#,
            r#"strftime(strptime("2024-12-30 23:05:07", "%Y-%m-%d %H:%M:%S"), "%W")"#,
            r#"strftime(strptime("2024-12-30 23:05:07", "%Y-%m-%d %H:%M:%S"), "%p")"#,
        ],
    );
    add(
        "date_part",
        &[
            r#"date_part("quarter", strptime("2024-12-31 23:59:59", "%Y-%m-%d %H:%M:%S"))"#,
            r#"date_part("quarter", strptime("2025-01-01 00:00:00", "%Y-%m-%d %H:%M:%S"))"#,
            r#"date_part("week", strptime("2020-12-31 00:00:00", "%Y-%m-%d %H:%M:%S"))"#,
            r#"date_part("week", strptime("2021-01-01 00:00:00", "%Y-%m-%d %H:%M:%S"))"#,
            r#"date_part("week", strptime("2021-01-04 00:00:00", "%Y-%m-%d %H:%M:%S"))"#,
            r#"date_part("dow", strptime("2024-12-29 00:00:00", "%Y-%m-%d %H:%M:%S"))"#,
            r#"date_part("doy", strptime("2024-12-31 00:00:00", "%Y-%m-%d %H:%M:%S"))"#,
            r#"date_part("doy", strptime("2025-01-01 00:00:00", "%Y-%m-%d %H:%M:%S"))"#,
        ],
    );
    add(
        "timestamp_comparison",
        &[
            r#"strptime("2024-12-30 23:05:07", "%Y-%m-%d %H:%M:%S") == "2024-12-30 23:05:07""#,
            r#""2024-12-30 23:05:07" == strptime("2024-12-30 23:05:07", "%Y-%m-%d %H:%M:%S")"#,
            r#"strptime("2024-12-30 23:05:07", "%Y-%m-%d %H:%M:%S") < "2024-12-31 00:00:00""#,
            r#""2024-12-31 00:00:00" > strptime("2024-12-30 23:05:07", "%Y-%m-%d %H:%M:%S")"#,
        ],
    );
    add(
        "json",
        &[
            r#"json_valid("{\"a\":1}")"#,
            r#"json_valid("not-json")"#,
            r#"json_extract_string("{\"a\":{\"b\":\"x\"}}", "$.a.b")"#,
            r#"json_extract_string("{\"a\":1}", "$.missing")"#,
            r#"json_extract("{\"a\":1}", "$.missing")"#,
            r#"json_keys("{\"a\":1,\"b\":2}")"#,
            r#"json_array_length("[1,2,3]")"#,
        ],
    );
    add(
        "tonumber",
        &[
            "tonumber(null)",
            "tonumber(42)",
            "tonumber(-1.5)",
            r#"tonumber("42.5")"#,
            r#"tonumber("not-a-number")"#,
            r#"tonumber(strptime("2024-12-30 23:05:07", "%Y-%m-%d %H:%M:%S"))"#,
            r#"tonumber(json_extract("[1,2]", "$"))"#,
        ],
    );
    add(
        "tostring",
        &[
            "tostring(null)",
            "tostring(true)",
            "tostring(42)",
            "tostring(-1.5)",
            r#"tostring("text")"#,
            r#"tostring(strptime("2024-12-30 23:05:07", "%Y-%m-%d %H:%M:%S"))"#,
        ],
    );
    cases
}

// ── Property test ──────────────────────────────────────────────────────

#[test]
#[should_panic(expected = "forced infrastructure failure")]
fn infrastructure_failure_panics() {
    let _: () = infra_or_panic::<(), _>(Err("forced"), "forced");
}

#[test]
#[should_panic(expected = "generated grammar rejected")]
fn generated_parse_rejection_panics() {
    let _ = parse_generated("* | let x = 1 +");
}

#[test]
fn timestamp_comparison_is_microsecond_exact() {
    use chrono::NaiveDateTime;
    use duckdb::types::TimeUnit;

    let eval = EvalValue::Timestamp(
        NaiveDateTime::parse_from_str("2026-01-15 10:20:30.000001", "%Y-%m-%d %H:%M:%S%.f")
            .unwrap(),
    );
    let sql = SqlCell {
        logical_type: Type::Timestamp,
        value: DuckValue::Timestamp(
            TimeUnit::Microsecond,
            NaiveDateTime::parse_from_str("2026-01-15 10:20:30", "%Y-%m-%d %H:%M:%S")
                .unwrap()
                .and_utc()
                .timestamp_micros(),
        ),
    };

    assert!(
        !values_match(&eval, &sql),
        "the retired whole-second matcher would incorrectly accept this case"
    );
}

#[test]
fn generated_scalar_families_are_complete_and_match() {
    use std::collections::{BTreeMap, BTreeSet};

    let conn = utc_connection();
    let event = fixed_event();
    let cases = generated_cases();
    let generated_families: BTreeSet<_> = cases.iter().map(|case| case.family).collect();
    let required_families: BTreeSet<_> = REQUIRED_GENERATED_FAMILIES.iter().copied().collect();
    assert_eq!(
        generated_families, required_families,
        "generated scalar family manifest drifted"
    );
    let mut generated_counts = BTreeMap::new();
    for case in &cases {
        *generated_counts.entry(case.family).or_insert(0) += 1;
    }
    assert_eq!(
        generated_counts,
        REQUIRED_FAMILY_CASE_COUNTS.iter().copied().collect(),
        "generated scalar case coverage drifted"
    );

    let mut hugeint_skips = 0u32;
    let mut blob_skips = 0u32;
    for case in cases {
        match assert_parity_case(&conn, &event, case.family, case.expression) {
            Some(SkipCategory::HugeIntOutsideI64) => hugeint_skips += 1,
            Some(SkipCategory::Blob) => blob_skips += 1,
            None => {}
        }
    }
    assert_eq!(hugeint_skips, 0, "HugeInt skip ceiling exceeded");
    assert_eq!(blob_skips, 0, "Blob skip ceiling exceeded");
}

#[test]
fn current_second_timestamp_parser_accepts_nondigit_offset_child_105() {
    // #105 deletes eval's second timestamp parser in favour of the probed owner.
    let conn = utc_connection();
    let event = fixed_event();
    let dsl = r#"* | let x = strptime("2026-01-15 10:20:30", "%Y-%m-%d %H:%M:%S") == "2026-01-15 10:20:30+ab:cd""#;
    assert_eq!(eval_scalar(dsl, &event), EvalValue::Bool(true));
    assert_eq!(sql_scalar_result(&conn, dsl, &event), SqlOutcome::Errored);
}

#[test]
fn current_timestamp_lexical_fallback_is_pinned_both_orders_child_105() {
    // #105 withdraws the lexical fallback; failed coercion becomes NULL.
    let conn = utc_connection();
    let event = fixed_event();
    for dsl in [
        r#"* | let x = strptime("2026-01-15 10:20:30", "%Y-%m-%d %H:%M:%S") < "zzz""#,
        r#"* | let x = "zzz" > strptime("2026-01-15 10:20:30", "%Y-%m-%d %H:%M:%S")"#,
    ] {
        assert_eq!(eval_scalar(dsl, &event), EvalValue::Bool(true));
        assert_eq!(sql_scalar_result(&conn, dsl, &event), SqlOutcome::Errored);
    }
}

#[test]
fn current_now_is_sampled_per_call_child_106() {
    // #106 anchors now() once per output unit. DuckDB already anchors per statement.
    let conn = utc_connection();
    let event = fixed_event();
    let dsl = "* | let x = now() == now()";
    let saw_per_call_difference =
        (0..100).any(|_| eval_scalar(dsl, &event) == EvalValue::Bool(false));
    assert!(saw_per_call_difference);
    assert_eq!(
        sql_scalar_result(&conn, dsl, &event),
        SqlOutcome::Value(SqlCell {
            logical_type: Type::Boolean,
            value: DuckValue::Boolean(true),
        })
    );
}

#[test]
fn current_tonumber_bool_is_null_child_105() {
    // #105 makes tonumber(bool) mirror DuckDB's TRY_CAST.
    let conn = utc_connection();
    let event = fixed_event();
    let dsl = "* | let x = tonumber(true)";
    assert_eq!(eval_scalar(dsl, &event), EvalValue::Null);
    assert_eq!(
        sql_scalar_result(&conn, dsl, &event),
        SqlOutcome::Value(SqlCell {
            logical_type: Type::Double,
            value: DuckValue::Double(1.0),
        })
    );
}

#[test]
fn current_integer_division_truncates_child_105() {
    // #105 changes Int/Int division to DuckDB true division.
    let conn = utc_connection();
    let event = fixed_event();
    for (dsl, eval, sql) in [
        ("* | let x = 5 / 2", 2, 2.5),
        ("* | let x = -5 / 2", -2, -2.5),
    ] {
        assert_eq!(eval_scalar(dsl, &event), EvalValue::Int(eval));
        assert_eq!(
            sql_scalar_result(&conn, dsl, &event),
            SqlOutcome::Value(SqlCell {
                logical_type: Type::Double,
                value: DuckValue::Double(sql),
            })
        );
    }
}

#[test]
fn current_integer_overflow_panics_or_wraps_child_105() {
    // #105 makes every integer arithmetic overflow yield NULL in every build profile.
    let conn = utc_connection();
    let event = fixed_event();
    for (dsl, release_wrap) in [
        ("* | let x = 9223372036854775807 + 1", i64::MIN),
        ("* | let x = -9223372036854775807 - 2", i64::MAX),
        ("* | let x = 9223372036854775807 * 2", -2),
    ] {
        let eval = std::panic::catch_unwind(|| eval_scalar(dsl, &event));
        if cfg!(debug_assertions) {
            assert!(
                eval.is_err(),
                "debug eval must expose today's overflow panic for {dsl:?}"
            );
        } else {
            assert_eq!(eval.unwrap(), EvalValue::Int(release_wrap));
        }
        assert_eq!(sql_scalar_result(&conn, dsl, &event), SqlOutcome::Errored);
    }
}

#[test]
fn current_string_truthiness_is_pinned_child_105() {
    // #105 adopts DuckDB's boolean condition domain.
    let conn = utc_connection();
    let event = fixed_event();
    for function in ["if", "case"] {
        let dsl = if function == "if" {
            r#"* | let x = if("nonempty", 1, 2)"#
        } else {
            r#"* | let x = case("nonempty", 1, 2)"#
        };
        assert_eq!(eval_scalar(dsl, &event), EvalValue::Int(1));
        assert_eq!(sql_scalar_result(&conn, dsl, &event), SqlOutcome::Errored);
    }
}

#[test]
fn current_ceil_floor_and_round_types_are_pinned_child_105() {
    // #105 makes these return DuckDB's DOUBLE shape for DOUBLE inputs.
    let conn = utc_connection();
    let event = fixed_event();
    for (dsl, eval, sql) in [
        ("* | let x = ceil(-1.5)", -1, -1.0),
        ("* | let x = floor(-1.5)", -2, -2.0),
        ("* | let x = ceil(0.0)", 0, 0.0),
        ("* | let x = floor(0.0)", 0, 0.0),
        ("* | let x = round(-1.5)", -2, -2.0),
    ] {
        assert_eq!(eval_scalar(dsl, &event), EvalValue::Int(eval));
        assert_eq!(
            sql_scalar_result(&conn, dsl, &event),
            SqlOutcome::Value(SqlCell {
                logical_type: Type::Double,
                value: DuckValue::Double(sql),
            })
        );
    }
}

#[test]
fn current_date_part_epoch_precision_is_pinned_child_105() {
    // #105 owns the epoch value domain. The addition and division forms round
    // this far instant to adjacent f64 values.
    let conn = utc_connection();
    let event = fixed_event();
    let dsl = r#"* | let x = date_part("epoch", event_ts_far)"#;
    let eval_rounded = 253_402_300_799.000_03;
    let sql_rounded = 253_402_300_799.0;
    assert_eq!(eval_scalar(dsl, &event), EvalValue::Float(eval_rounded));
    assert_eq!(
        sql_scalar_result(&conn, dsl, &event),
        SqlOutcome::Value(SqlCell {
            logical_type: Type::Double,
            value: DuckValue::Double(sql_rounded),
        })
    );
}

#[test]
fn current_typeof_integer_spelling_is_pinned_child_105() {
    // #105 reconciles EvalValue::Int's INTEGER spelling with DuckDB BIGINT.
    let conn = utc_connection();
    let event = fixed_event();
    let dsl = "* | let x = typeof(1)";
    assert_eq!(
        eval_scalar(dsl, &event),
        EvalValue::Str("INTEGER".to_string())
    );
    assert_eq!(
        sql_scalar_result(&conn, dsl, &event),
        SqlOutcome::Value(SqlCell {
            logical_type: Type::Text,
            value: DuckValue::Text("BIGINT".to_string()),
        })
    );
}

#[test]
fn hostile_timestamp_corpus_is_pinned_child_105() {
    // #105 replaces all of these with the one probe-pinned timestamp domain.
    let conn = utc_connection();
    let event = fixed_event();
    for (text, eval, sql) in [
        ("+99:99", true, Some(true)),
        ("+5:30", false, None),
        ("+0530", false, Some(true)),
        ("Z", true, Some(true)),
        (" UTC", false, Some(true)),
    ] {
        let dsl = format!(
            r#"* | let x = strptime("2026-01-15 10:20:30", "%Y-%m-%d %H:%M:%S") == "2026-01-15 10:20:30{text}""#
        );
        assert_eq!(eval_scalar(&dsl, &event), EvalValue::Bool(eval));
        let expected = sql.map_or(SqlOutcome::Errored, |value| {
            SqlOutcome::Value(SqlCell {
                logical_type: Type::Boolean,
                value: DuckValue::Boolean(value),
            })
        });
        assert_eq!(sql_scalar_result(&conn, &dsl, &event), expected);
    }
    for (text, eval, sql) in [("epoch", false, true), ("infinity", false, false)] {
        let base = if text == "epoch" {
            "1970-01-01 00:00:00"
        } else {
            "2026-01-15 10:20:30"
        };
        let dsl = format!(r#"* | let x = strptime("{base}", "%Y-%m-%d %H:%M:%S") == "{text}""#);
        assert_eq!(eval_scalar(&dsl, &event), EvalValue::Bool(eval));
        assert_eq!(
            sql_scalar_result(&conn, &dsl, &event),
            SqlOutcome::Value(SqlCell {
                logical_type: Type::Boolean,
                value: DuckValue::Boolean(sql),
            })
        );
    }
}

#[test]
fn current_percent_f_precision_is_pinned_child_105() {
    // #105 reconciles `%f` with the DuckDB value domain.
    let conn = utc_connection();
    let event = fixed_event();
    let percent_f = r#"* | let x = strftime(strptime("2024-12-30 23:05:07.123456", "%Y-%m-%d %H:%M:%S.%f"), "%f")"#;
    assert_eq!(
        eval_scalar(percent_f, &event),
        EvalValue::Str("000123456".to_string())
    );
    assert_eq!(
        sql_scalar_result(&conn, percent_f, &event),
        SqlOutcome::Value(SqlCell {
            logical_type: Type::Text,
            value: DuckValue::Text("123456".to_string()),
        })
    );
}

#[test]
fn accepted_bare_percent_y_strptime_residual_is_pinned() {
    let conn = utc_connection();
    let event = fixed_event();
    let dsl = r#"* | let x = tostring(strptime("24", "%y"))"#;
    assert_eq!(eval_scalar(dsl, &event), EvalValue::Null);
    assert_eq!(
        sql_scalar_result(&conn, dsl, &event),
        SqlOutcome::Value(SqlCell {
            logical_type: Type::Text,
            value: DuckValue::Text("2024-01-01 00:00:00".to_string()),
        })
    );
}

#[test]
fn accepted_percent_z_strptime_wall_clock_residual_is_pinned() {
    let conn = utc_connection();
    let event = fixed_event();
    let dsl = r#"* | let x = strftime(strptime("2024-12-30 23:05:07 +0530", "%Y-%m-%d %H:%M:%S %z"), "%Y-%m-%d %H:%M:%S")"#;
    assert_eq!(
        eval_scalar(dsl, &event),
        EvalValue::Str("2024-12-30 23:05:07".to_string())
    );
    assert_eq!(
        sql_scalar_result(&conn, dsl, &event),
        SqlOutcome::Value(SqlCell {
            logical_type: Type::Text,
            value: DuckValue::Text("2024-12-30 17:35:07".to_string()),
        })
    );
}

#[test]
fn accepted_locale_percent_c_residual_is_pinned() {
    let conn = utc_connection();
    let event = fixed_event();
    let dsl = r#"* | let x = strftime(strptime("2024-12-30 23:05:07", "%Y-%m-%d %H:%M:%S"), "%c")"#;
    assert_eq!(
        eval_scalar(dsl, &event),
        EvalValue::Str("Mon Dec 30 23:05:07 2024".to_string())
    );
    assert_eq!(
        sql_scalar_result(&conn, dsl, &event),
        SqlOutcome::Value(SqlCell {
            logical_type: Type::Text,
            value: DuckValue::Text("2024-12-30 23:05:07".to_string()),
        })
    );
}

#[test]
fn current_json_value_and_array_rendering_are_pinned_child_105() {
    // #105 decides the value-domain answer for JSON-backed EvalValue variants.
    let conn = utc_connection();
    let event = fixed_event();
    for (dsl, eval, sql) in [
        (
            r#"* | let x = json_extract("{\"a\":1}", "$.a")"#,
            EvalValue::Int(1),
            "1",
        ),
        (
            r#"* | let x = tostring(json_extract("[1,2]", "$"))"#,
            EvalValue::Null,
            "[1,2]",
        ),
    ] {
        assert_eq!(eval_scalar(dsl, &event), eval);
        assert_eq!(
            sql_scalar_result(&conn, dsl, &event),
            SqlOutcome::Value(SqlCell {
                logical_type: Type::Text,
                value: DuckValue::Text(sql.to_string()),
            })
        );
    }
}

#[test]
fn duckdb_conditionals_and_coalesce_short_circuit() {
    // #105 may change evaluation order only if these DuckDB probes move.
    let conn = utc_connection();
    for sql in [
        "SELECT IF(false, error('untaken'), 42)",
        "SELECT CASE WHEN false THEN error('untaken') ELSE 42 END",
        "SELECT COALESCE(42, error('untaken'))",
        "SELECT IF(false, 1 / 0, 42)",
        "SELECT CASE WHEN false THEN 1 / 0 ELSE 42 END",
        "SELECT COALESCE(42, 1 / 0)",
    ] {
        let value: f64 = infra_or_panic(
            conn.query_row(sql, [], |row| row.get(0)),
            "DuckDB short-circuit probe",
        );
        assert_eq!(
            value.to_bits(),
            42.0_f64.to_bits(),
            "DuckDB stopped short-circuiting {sql:?}"
        );
    }
}

#[test]
fn scalar_eval_matches_sql_parity() {
    let conn = utc_connection();
    let mut rng = Rng::new(0xCAFE_BABE);
    let event = fixed_event();
    let mut passed = 0u32;
    let mut hugeint_skips = 0u32;
    let mut blob_skips = 0u32;

    for i in 0..500 {
        let expr_str = random_scalar_expr(&mut rng)
            .expect("scalar generator infrastructure failure: no family selected");

        let dsl = format!("* | let x = {expr_str}");

        // ── Streaming path (eval_expr) ── computed first so it is available to
        // assert against a DuckDB error (the streaming contract is: eval can't
        // error, so it must yield Null wherever batch errors).
        let eval_result = eval_scalar(&dsl, &event);
        let eval_normalized = normalize_eval(&eval_result);

        // ── Batch path (DuckDB) ──
        match sql_scalar_result(&conn, &dsl, &event) {
            SqlOutcome::Skip(SkipCategory::HugeIntOutsideI64) => {
                hugeint_skips += 1;
                continue;
            }
            SqlOutcome::Skip(SkipCategory::Blob) => {
                blob_skips += 1;
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
                    values_match(&eval_result, &sql_result),
                    "SCALAR PARITY MISMATCH (iteration {i})\n\
                     dsl: {dsl:?}\n\
                     eval: {eval_normalized:?}\n\
                     sql:  {sql_result:?}"
                );
            }
        }

        passed += 1;
    }

    eprintln!(
        "scalar parity: {passed} passed, hugeint_skips={hugeint_skips}, blob_skips={blob_skips}"
    );
    assert_eq!(hugeint_skips, 0, "HugeInt skip ceiling exceeded");
    assert_eq!(blob_skips, 0, "Blob skip ceiling exceeded");
}

/// The harness must surface a `DuckDB` error as [`SqlOutcome::Errored`] (not a
/// silent skip), and the streaming contract is that eval yields `Null` wherever
/// batch errors. These type-mismatch expressions error in the `DuckDB` binder;
/// eval — being permissive — nulls. This proves both halves: the harness no longer
/// hides batch errors, and the streaming path honours the null-where-batch-errors
/// contract for these cases.
#[test]
fn batch_errors_imply_streaming_null() {
    let conn = utc_connection();
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
    let conn = utc_connection();
    let event = fixed_event();
    // Full datetime so chrono's NaiveDateTime::parse_from_str succeeds (date-only
    // formats are a separate chrono-vs-DuckDB divergence, not the param bug here).
    let dsl = r#"* | let x = strftime(strptime("2023-11-07 17:30:45", "%Y-%m-%d %H:%M:%S"), "%Y")"#;

    // Batch path (real DuckDB).
    assert_eq!(
        sql_scalar_result(&conn, dsl, &event),
        SqlOutcome::Value(SqlCell {
            logical_type: Type::Text,
            value: DuckValue::Text("2023".to_string()),
        }),
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
    let conn = utc_connection();
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
            SqlOutcome::Value(SqlCell {
                logical_type: Type::Text,
                value: DuckValue::Text(want.to_string()),
            }),
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
