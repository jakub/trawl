// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Property test: scalar `let` expressions must produce the same result
//! through `eval_scalar_fn` (streaming path) and `DuckDB` SQL (batch path).

use std::io::Write;

use duckdb::Connection;
use duckdb::types::{Type, Value as DuckValue};
use serde_json::{Map, Value};
use trawl_core::ast::PipeStage;
use trawl_core::context::EvalContext;
use trawl_core::emitter::{self, DATE_PART_UNITS, DATE_UNITS, SqlValue};
use trawl_core::eval::{EvalValue, PatternTable, eval_expr};
use trawl_core::parser;
use trawl_core::pattern::Preparer;

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

/// The microseconds are nonzero on purpose: a whole-second anchor would
/// pass `strftime(now(), "…%f")` even if the anchor lost its sub-second
/// precision on the way into either lane.
const NOW_ANCHOR_TEXT: &str = "2026-02-03T03:04:05.123456Z";

/// The anchor as `DuckDB` renders it (`CAST(TIMESTAMP AS VARCHAR)`): a
/// space separator and exactly the microseconds it holds.
const NOW_ANCHOR_RENDERED: &str = "2026-02-03 03:04:05.123456";

fn now_anchor() -> EvalContext {
    EvalContext::at(
        chrono::DateTime::parse_from_rfc3339(NOW_ANCHOR_TEXT)
            .expect("anchor fixture infrastructure failure: NOW_ANCHOR_TEXT is not RFC 3339")
            .with_timezone(&chrono::Utc),
    )
}

const TS_VALS: &[&str] = &[
    "2026-01-15 10:20:30",
    "2026-06-01 00:00:00",
    "2026-12-31 23:59:59",
    "2000-01-01 00:00:00",
    "2024-03-15 08:45:00",
    "2023-11-07 17:30:45",
];

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
// base) the streaming evaluator and DuckDB's STRPTIME agree on. Only formats
// with exact streaming/batch parity belong here.
const STRPTIME_PARTIALS: &[(&str, &str)] = &[
    ("2023", "%Y"),                   // year-only -> 2023-01-01 00:00:00
    ("2023-11", "%Y-%m"),             // year-month -> 2023-11-01 00:00:00
    ("11-07", "%m-%d"),               // bare month-day -> 1900-11-07 00:00:00
    ("2023-11-07 14", "%Y-%m-%d %H"), // date + incomplete time -> ...14:00:00
];

const INT_VALS: &[i64] = &[0, 1, 42, 100, -5, 200, 500];

// Float DSL literals spanning the DuckDB CAST(DOUBLE AS VARCHAR) presentation
// rules: integer-valued (.0 suffix), plain decimals, magnitudes that flip into
// sci-notation on render (the value decides, so the literal stays plain: the
// DSL float grammar requires `digits.digits`, no `e` notation), negatives,
// and zero. Drives the tostring float-text parity.
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

// A deleted generator arm is a skip moved to generation time, which ADR-0017
// §5 outlaws. Two arms can share a head symbol (`strptime` over a full vs. a
// partial format, `tostring` over an int vs. a float) and are distinct
// coverage classes, so the guard checks arm labels as well as function names.

const SCALAR_FAMILY_ARMS: usize = 16;
const STRING_FN_ARMS: usize = 8;
const TYPEOF_ARG_ARMS: usize = 4;
const NUMERIC_ARG_ARMS: usize = 2;
const SEV_ARG_ARMS: usize = 5;
const SEV_DIALECT_ARMS: usize = 3;
const CONCAT_ARG_ARMS: usize = 3;
const SUBSTR_WINDOW_ARMS: usize = 17;
const STRPTIME_SHAPE_ARMS: usize = 4;
const NOW_SHAPE_ARMS: usize = 4;

/// (selector name, the constant the generator uses, the value it must hold).
const REQUIRED_SELECTOR_ARMS: &[(&str, usize, usize)] = &[
    ("random_scalar_expr family", SCALAR_FAMILY_ARMS, 16),
    ("random_string_fn", STRING_FN_ARMS, 8),
    ("random_numeric_fn", NUMERIC_FN_ARMS, 4),
    ("random_numeric_fn argument", NUMERIC_ARG_ARMS, 2),
    ("random_typeof argument", TYPEOF_ARG_ARMS, 4),
    ("random_sev argument", SEV_ARG_ARMS, 5),
    ("random_sev dialect", SEV_DIALECT_ARMS, 3),
    ("random_concat argument", CONCAT_ARG_ARMS, 3),
    ("random_substr window", SUBSTR_WINDOW_ARMS, 17),
    ("random_strptime shape", STRPTIME_SHAPE_ARMS, 4),
    ("random_now shape", NOW_SHAPE_ARMS, 4),
];

struct Generated {
    arms: Vec<&'static str>,
    expression: String,
}

impl Generated {
    fn new(arm: &'static str, expression: String) -> Self {
        Self {
            arms: vec![arm],
            expression,
        }
    }

    fn and(mut self, arm: &'static str) -> Self {
        self.arms.push(arm);
        self
    }
}

const REQUIRED_GENERATOR_ARMS: &[&str] = &[
    "coalesce",
    "concat/arg_int",
    "concat/arg_null",
    "concat/arg_string",
    "conditional/cond_false",
    "conditional/cond_true",
    "date_diff",
    "date_part",
    "date_trunc",
    "now/bare",
    "now/date_part",
    "now/rendered",
    "now/self_equality",
    "numeric_fn/abs",
    "numeric_fn/arg_float",
    "numeric_fn/arg_int",
    "numeric_fn/ceil",
    "numeric_fn/floor",
    "numeric_fn/round",
    "sev/arg_field_level",
    "sev/arg_field_sev_num",
    "sev/arg_field_status",
    "sev/arg_integer_literal",
    "sev/arg_text_literal",
    "sev/dialect_default",
    "sev/dialect_otel",
    "sev/dialect_syslog",
    "strftime",
    "string_fn/contains",
    "string_fn/length",
    "string_fn/lower",
    "string_fn/ltrim",
    "string_fn/replace",
    "string_fn/rtrim",
    "string_fn/trim",
    "string_fn/upper",
    "strptime/full_format",
    "strptime/partial_format",
    "strptime/unparseable",
    "substr/start_and_len",
    "substr/start_only",
    "tonumber/digit_separator_text",
    "tonumber/integer_text",
    "tostring/float",
    "tostring/int",
    "typeof",
    "typeof/arg_bool",
    "typeof/arg_float",
    "typeof/arg_int",
    "typeof/arg_string",
];

const REQUIRED_RANDOM_CALL_NAMES: &[&str] = &[
    "abs",
    "ceil",
    "coalesce",
    "concat",
    "contains",
    "date_diff",
    "date_part",
    "date_trunc",
    "floor",
    "if",
    "length",
    "lower",
    "ltrim",
    "now",
    "replace",
    "round",
    "rtrim",
    "sev",
    "strftime",
    "strptime",
    "substr",
    "tonumber",
    "tostring",
    "trim",
    "typeof",
    "upper",
];

fn leading_call_name(expression: &str) -> &str {
    let (name, _) = expression
        .split_once('(')
        .unwrap_or_else(|| panic!("generated expression is not a call: {expression:?}"));
    name
}

/// Generate a scalar DSL expression (no field refs that could be absent
/// from the fixed event), labelled with the arms that produced it.
fn random_scalar_expr(rng: &mut Rng) -> Option<Generated> {
    match rng.range(SCALAR_FAMILY_ARMS) {
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
        15 => Some(random_now(rng)),
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

fn random_string_fn(rng: &mut Rng) -> Generated {
    match rng.range(STRING_FN_ARMS) {
        0 => Generated::new("string_fn/lower", format!("lower({})", str_lit(rng))),
        1 => Generated::new("string_fn/upper", format!("upper({})", str_lit(rng))),
        2 => Generated::new("string_fn/length", format!("length({})", str_lit(rng))),
        3 => Generated::new("string_fn/trim", format!("trim({})", str_lit(rng))),
        4 => Generated::new("string_fn/ltrim", format!("ltrim({})", str_lit(rng))),
        5 => Generated::new("string_fn/rtrim", format!("rtrim({})", str_lit(rng))),
        6 => Generated::new(
            "string_fn/contains",
            format!("contains({}, {})", str_lit(rng), str_lit(rng)),
        ),
        7 => Generated::new(
            "string_fn/replace",
            format!(
                "replace({}, {}, {})",
                str_lit(rng),
                str_lit(rng),
                str_lit(rng)
            ),
        ),
        _ => unreachable!(),
    }
}

const NUMERIC_FN_ARMS: usize = 4;

fn random_numeric_fn(rng: &mut Rng) -> Generated {
    let (arg_arm, argument) = if rng.range(NUMERIC_ARG_ARMS) == 0 {
        ("numeric_fn/arg_int", int_lit(rng))
    } else {
        ("numeric_fn/arg_float", float_lit(rng))
    };
    let (arm, expression) = match rng.range(NUMERIC_FN_ARMS) {
        0 => ("numeric_fn/abs", format!("abs({argument})")),
        1 => ("numeric_fn/round", format!("round({argument})")),
        2 => ("numeric_fn/ceil", format!("ceil({argument})")),
        3 => ("numeric_fn/floor", format!("floor({argument})")),
        _ => unreachable!(),
    };
    Generated::new(arm, expression).and(arg_arm)
}

fn random_conditional(rng: &mut Rng) -> Generated {
    let (arm, cond) = if rng.bool() {
        ("conditional/cond_true", "true")
    } else {
        ("conditional/cond_false", "false")
    };
    let a = str_lit(rng);
    let b = str_lit(rng);
    Generated::new(arm, format!("if({cond}, {a}, {b})"))
}

fn random_date_part(rng: &mut Rng) -> Generated {
    let unit = rng.pick(DATE_PART_UNITS);
    let ts = ts_lit(rng);
    Generated::new("date_part", format!("date_part(\"{unit}\", {ts})"))
}

fn random_date_trunc(rng: &mut Rng) -> Generated {
    let unit = rng.pick(DATE_UNITS);
    let ts = ts_lit(rng);
    Generated::new("date_trunc", format!("date_trunc(\"{unit}\", {ts})"))
}

fn random_date_diff(rng: &mut Rng) -> Generated {
    let unit = rng.pick(DATE_UNITS);
    let start = ts_lit(rng);
    let end = ts_lit(rng);
    Generated::new(
        "date_diff",
        format!("date_diff(\"{unit}\", {start}, {end})"),
    )
}

fn random_strftime(rng: &mut Rng) -> Generated {
    let ts = ts_lit(rng);
    // Only test C-strftime formats chrono and DuckDB agree on
    let fmt = rng.pick(&["%Y-%m-%d", "%H:%M:%S", "%Y-%m-%d %H:%M:%S", "%m/%d/%Y"]);
    Generated::new("strftime", format!("strftime({ts}, \"{fmt}\")"))
}

fn random_strptime(rng: &mut Rng) -> Generated {
    if rng.range(STRPTIME_SHAPE_ARMS) == 0 {
        return Generated::new(
            "strptime/unparseable",
            "strptime(\"not-a-date\", \"%Y-%m-%d %H:%M:%S\")".to_string(),
        );
    }
    if rng.range(STRPTIME_SHAPE_ARMS) == 0 {
        let (input, fmt) = rng.pick(STRPTIME_PARTIALS);
        return Generated::new(
            "strptime/partial_format",
            format!("strftime(strptime(\"{input}\", \"{fmt}\"), \"%Y-%m-%d %H:%M:%S\")"),
        );
    }
    let ts = rng.pick(TS_VALS);
    Generated::new(
        "strptime/full_format",
        format!("strftime(strptime(\"{ts}\", \"%Y-%m-%d %H:%M:%S\"), \"%Y-%m-%d %H:%M:%S\")"),
    )
}

fn random_tonumber(rng: &mut Rng) -> Generated {
    if rng.bool() {
        let s = rng.pick(&["1_000", "1_0.0_5", "_1000"]);
        Generated::new(
            "tonumber/digit_separator_text",
            format!("tonumber(\"{s}\")"),
        )
    } else {
        let n = rng.pick(INT_VALS).unsigned_abs();
        Generated::new("tonumber/integer_text", format!("tonumber(\"{n}\")"))
    }
}

fn random_tostring(rng: &mut Rng) -> Generated {
    if rng.bool() {
        Generated::new("tostring/int", format!("tostring({})", int_lit(rng)))
    } else {
        Generated::new("tostring/float", format!("tostring({})", float_lit(rng)))
    }
}

fn random_substr(rng: &mut Rng) -> Generated {
    let s = str_lit(rng);
    #[allow(clippy::cast_possible_wrap)]
    let start = rng.range(SUBSTR_WINDOW_ARMS) as i64 - 8; // -8..=8
    if rng.bool() {
        Generated::new("substr/start_only", format!("substr({s}, {start})"))
    } else {
        #[allow(clippy::cast_possible_wrap)]
        let len = rng.range(SUBSTR_WINDOW_ARMS) as i64 - 8; // -8..=8
        Generated::new(
            "substr/start_and_len",
            format!("substr({s}, {start}, {len})"),
        )
    }
}

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
    // match would read them as severities in batch alone.
    "\u{130}NFO",
    "\u{131}",
    "\u{212a}",
    "\u{ff29}\u{ff2e}\u{ff26}\u{ff2f}",
    "\u{ff11}\u{ff17}",
];

/// `sev(x[, dialect])` over literals and over the event's own columns: the
/// SQL lane reads the column's text form while eval reads the wire JSON, so
/// a field arm is the one that proves the two readings agree.
fn random_sev(rng: &mut Rng) -> Generated {
    let (arg_arm, arg) = match rng.range(SEV_ARG_ARMS) {
        0 => (
            "sev/arg_text_literal",
            format!("\"{}\"", rng.pick(SEV_TEXTS)),
        ),
        1 => (
            "sev/arg_integer_literal",
            rng.pick(&[0_i64, 1, 3, 7, 8, 17, 24, 25, -1]).to_string(),
        ),
        2 => ("sev/arg_field_level", "level".to_string()),
        3 => ("sev/arg_field_sev_num", "sev_num".to_string()),
        4 => ("sev/arg_field_status", "status".to_string()),
        _ => unreachable!(),
    };
    let (dialect_arm, expression) = match rng.range(SEV_DIALECT_ARMS) {
        0 => ("sev/dialect_default", format!("sev({arg})")),
        1 => ("sev/dialect_otel", format!("sev({arg}, \"otel\")")),
        2 => ("sev/dialect_syslog", format!("sev({arg}, \"syslog\")")),
        _ => unreachable!(),
    };
    Generated::new(arg_arm, expression).and(dialect_arm)
}

/// Eval spells a value by the type its bound literal arrives as, so every
/// shape here agrees. The two that diverge, the NULL type and a list, are not
/// bound literals and are pinned by
/// `current_typeof_null_and_list_spellings_are_pinned`.
fn random_typeof(rng: &mut Rng) -> Generated {
    let (arg_arm, argument) = match rng.range(TYPEOF_ARG_ARMS) {
        0 => ("typeof/arg_string", str_lit(rng)),
        1 => ("typeof/arg_int", int_lit(rng)),
        2 => ("typeof/arg_float", float_lit(rng)),
        3 => (
            "typeof/arg_bool",
            if rng.bool() { "true" } else { "false" }.to_string(),
        ),
        _ => unreachable!(),
    };
    Generated::new("typeof", format!("typeof({argument})")).and(arg_arm)
}

fn random_coalesce(rng: &mut Rng) -> Generated {
    let a = str_lit(rng);
    let b = str_lit(rng);
    Generated::new("coalesce", format!("coalesce({a}, {b})"))
}

fn random_concat(rng: &mut Rng) -> Generated {
    let n = 2 + rng.range(3);
    let mut arms = Vec::new();
    let args: Vec<String> = (0..n)
        .map(|_| match rng.range(CONCAT_ARG_ARMS) {
            0 => {
                arms.push("concat/arg_string");
                str_lit(rng)
            }
            1 => {
                arms.push("concat/arg_int");
                int_lit(rng)
            }
            2 => {
                arms.push("concat/arg_null");
                "null".to_string()
            }
            _ => unreachable!(),
        })
        .collect();
    Generated {
        arms,
        expression: format!("concat({})", args.join(", ")),
    }
}

fn random_now(rng: &mut Rng) -> Generated {
    match rng.range(NOW_SHAPE_ARMS) {
        0 => Generated::new("now/bare", "now()".to_string()),
        1 => Generated::new("now/self_equality", "now() == now()".to_string()),
        2 => Generated::new("now/rendered", "tostring(now())".to_string()),
        3 => {
            let unit = rng.pick(DATE_PART_UNITS);
            Generated::new("now/date_part", format!("date_part(\"{unit}\", now())"))
        }
        _ => unreachable!(),
    }
}

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
    m.insert("sev_num".into(), Value::Number(3.into()));
    m.insert("sev_word".into(), Value::String("warn".into()));
    m
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

#[derive(Debug, PartialEq)]
enum SqlOutcome {
    Value(SqlCell),
    Errored(String),
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

fn eval_scalar(dsl: &str, event: &Map<String, Value>, anchor: EvalContext) -> EvalValue {
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
    let patterns = PatternTable::prepare([expr], &mut Preparer::new())
        .unwrap_or_else(|error| panic!("{dsl:?}: the patterns prepare: {error}"));
    eval_expr(expr, &trawl_core::row::from_json(event), &patterns, &anchor)
        .unwrap_or_else(|error| panic!("{dsl:?}: {error}"))
}

fn sql_scalar_result(
    conn: &Connection,
    dsl: &str,
    event: &Map<String, Value>,
    anchor: EvalContext,
) -> SqlOutcome {
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

    let emitted = emitter::emit(&query, tmp_path, anchor)
        .unwrap_or_else(|error| panic!("generated grammar failed to emit {dsl:?}: {error:?}"));

    let params = bind_params(&emitted.params);
    let param_refs: Vec<&dyn duckdb::ToSql> = params.iter().map(AsRef::as_ref).collect();

    let mut stmt = match conn.prepare(&emitted.sql) {
        Ok(stmt) => stmt,
        Err(error) => return SqlOutcome::Errored(error.to_string()),
    };
    let mut rows = match stmt.query(param_refs.as_slice()) {
        Ok(rows) => rows,
        Err(error) => return SqlOutcome::Errored(error.to_string()),
    };
    // A fetch-time `Err` is a real DuckDB execution error and classifies as
    // one, exactly like the prepare/query failures above. Against duckdb-rs
    // 1.10505.0, `query()` materializes the result and every runtime error
    // surfaces there, so this arm is unreachable today.
    let row = match rows.next() {
        Ok(Some(row)) => row,
        Err(error) => return SqlOutcome::Errored(error.to_string()),
        Ok(None) => panic!(
            "query infrastructure failure: generated scalar query returned an EMPTY \
             result set — the one-row fixture did not come back: {dsl:?}"
        ),
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

/// Explicitly permitted representation widenings between streaming and batch.
/// There is deliberately no string/numeric coercion and no timestamp/text arm.
///
/// Caveat: `sql` is duckdb-rs's `Type`, converted from the result column's
/// Arrow `DataType`, and that conversion erases both the time unit and the time
/// zone (`DataType::Timestamp(_, _) => Type::Timestamp` covers TIMESTAMP and
/// TIMESTAMPTZ alike). This table therefore cannot distinguish them, so the
/// TIMESTAMP that ADR-0017 §3 requires of a bound `now()` is asserted
/// textually instead, by comparing `typeof(now())` as a string in
/// `now_reads_the_statements_anchor_in_both_lanes`.
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
        EvalValue::UInt(_) => matches!(sql, Type::UBigInt | Type::HugeInt),
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

/// An instant as the MICROSECOND count duckdb-rs hands back for it.
///
/// The two infinities come back as i64 sentinels rather than as counts:
/// `i64::MAX`, and `-i64::MAX` for the negative one, which is not
/// `i64::MIN` (probed: `an_infinity_timestamp_cell_is_an_i64_sentinel`).
/// A finite instant must be a whole number of microseconds, so a
/// sub-microsecond eval value fails rather than being rounded into
/// agreement.
fn timestamp_micros(instant: trawl_core::compare::Instant) -> Option<i64> {
    match instant {
        trawl_core::compare::Instant::Infinity => Some(i64::MAX),
        trawl_core::compare::Instant::NegInfinity => Some(-i64::MAX),
        trawl_core::compare::Instant::At(at) => {
            let utc = at.and_utc();
            utc.timestamp_subsec_nanos()
                .is_multiple_of(1_000)
                .then(|| utc.timestamp_micros())
        }
    }
}

fn scalar_values_match(eval: &EvalValue, sql: &DuckValue) -> bool {
    match (eval, sql) {
        (EvalValue::Null, DuckValue::Null) => true,
        (EvalValue::Bool(a), DuckValue::Boolean(b)) => a == b,
        (EvalValue::Int(a), value) => integer_value(value) == Some(*a),
        (EvalValue::UInt(a), DuckValue::UBigInt(b)) => a == b,
        (EvalValue::UInt(a), DuckValue::HugeInt(b)) => i128::from(*a) == *b,
        (EvalValue::Float(a), DuckValue::Float(b)) => a.to_bits() == f64::from(*b).to_bits(),
        (EvalValue::Float(a), DuckValue::Double(b)) => a.to_bits() == b.to_bits(),
        (EvalValue::Str(a), DuckValue::Text(b)) => a == b,
        (EvalValue::Timestamp(a), DuckValue::Timestamp(unit, b)) => {
            timestamp_micros(*a) == Some(unit.to_micros(*b))
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

fn assert_parity_case(
    conn: &Connection,
    event: &Map<String, Value>,
    label: &str,
    expression: &str,
) -> Option<SkipCategory> {
    let dsl = format!("* | let x = {expression}");
    let anchor = now_anchor();
    let eval_result = eval_scalar(&dsl, event, anchor);
    match sql_scalar_result(conn, &dsl, event, anchor) {
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
        SqlOutcome::Errored(message) => {
            assert_eq!(
                eval_result,
                EvalValue::Null,
                "BATCH ERRORED but streaming did NOT null ({label})\n\
                 dsl: {dsl:?}\n\
                 eval: {eval_result:?}\n\
                 duckdb error: {message}"
            );
            None
        }
        SqlOutcome::Skip(category) => Some(category),
    }
}

/// Assert that `DuckDB` errored AND that it errored for the intended reason.
///
/// A bare `Errored` assertion keeps a pin green when `DuckDB` starts failing for
/// a different reason (a binder change, a renamed function, a new overflow
/// path), which is the silent drift this harness exists to catch. Every pinned
/// divergence test therefore names a distinguishing substring of today's error.
fn assert_sql_errored(outcome: &SqlOutcome, expected_substring: &str, dsl: &str) {
    match outcome {
        SqlOutcome::Errored(message) => assert!(
            message.contains(expected_substring),
            "BATCH ERRORED FOR THE WRONG REASON\n\
             dsl: {dsl:?}\n\
             wanted substring: {expected_substring:?}\n\
             actual error: {message}"
        ),
        other => panic!(
            "expected a DuckDB error containing {expected_substring:?} for {dsl:?}, got {other:?}"
        ),
    }
}

#[derive(Clone, Copy)]
struct GeneratedCase {
    family: &'static str,
    expression: &'static str,
}

const REQUIRED_FAMILY_CASE_COUNTS: &[(&str, usize)] = &[
    ("operators", 27),
    ("numeric", 7),
    ("coalesce", 5),
    ("conditional", 4),
    ("strftime", 4),
    ("date_part", 8),
    ("timestamp_comparison", 4),
    ("json", 7),
    ("tonumber", 7),
    ("tostring", 8),
    ("now", 11),
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
            // Integer TRUE division and its IEEE edges. `0 / 0` and
            // `5 % 0.0` are deliberately absent: both are NaN, the matcher
            // compares floats BIT-exactly, and a NaN's bit pattern is the
            // platform's business. They are pinned by
            // `division_by_zero_matches_duckdbs_ieee_answer` instead, which
            // compares the RENDERED value.
            "5 / 2",
            "-5 / 2",
            "1 / 0",
            "-1 / 0",
            "5 % 0",
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
            // A NaN renders with its SIGN. Read through `tonumber`, whose
            // cast domain (`compare::try_cast_double`, and TRY_CAST in
            // batch) carries the sign from the TEXT — deliberately not
            // `tostring(0 / 0)`, whose computed NaN takes its sign from
            // the hardware and would pin this platform.
            r#"tostring(tonumber("nan"))"#,
            r#"tostring(tonumber("-nan"))"#,
        ],
    );
    add(
        "now",
        &[
            "now()",
            "now() == now()",
            "typeof(now())",
            "tostring(now())",
            r#"strftime(now(), "%Y-%m-%d %H:%M:%S.%f")"#,
            r#"date_diff("second", now(), now())"#,
            r#"date_part("year", now())"#,
            r#"date_trunc("second", now())"#,
            "if(true, now(), now())",
            "coalesce(now(), now())",
            "sev(now())",
        ],
    );
    cases
}

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

    let eval = EvalValue::Timestamp(trawl_core::compare::Instant::At(
        NaiveDateTime::parse_from_str("2026-01-15 10:20:30.000001", "%Y-%m-%d %H:%M:%S%.f")
            .unwrap(),
    ));
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
fn random_generator_surface_is_complete() {
    use std::collections::BTreeSet;

    for &(selector, actual, required) in REQUIRED_SELECTOR_ARMS {
        assert_eq!(
            actual, required,
            "the {selector} selector now offers {actual} arms, not {required}. \
             Deleting a generator arm is a SKIP MOVED TO GENERATION TIME, which \
             ADR-0017 §5 outlaws. Restore the arm; or, if it was removed because \
             it now diverges, add a named `current_*_child_105` test asserting \
             TODAY'S divergence and update this constant in the SAME commit."
        );
    }

    let mut rng = Rng::new(0x5EED_5CA1);
    let mut observed_arms = BTreeSet::new();
    let mut observed_names = BTreeSet::new();
    for _ in 0..20_000 {
        let generated = random_scalar_expr(&mut rng)
            .expect("scalar generator infrastructure failure: no family selected");
        observed_arms.extend(generated.arms.iter().copied());
        observed_names.insert(leading_call_name(&generated.expression).to_string());
    }

    let required_arms: BTreeSet<&str> = REQUIRED_GENERATOR_ARMS.iter().copied().collect();
    assert_eq!(
        required_arms.len(),
        REQUIRED_GENERATOR_ARMS.len(),
        "REQUIRED_GENERATOR_ARMS holds a duplicate label — every arm needs its OWN \
         discriminant, or two coverage classes share one slot and either can be \
         deleted for free"
    );
    assert_eq!(
        observed_arms, required_arms,
        "the random scalar generator's ARM SET drifted. An arm that DISAPPEARED is \
         a skip moved to generation time, which ADR-0017 §5 outlaws — and it is \
         invisible to the function-name check below whenever a sibling arm emits \
         the same head symbol (`strptime/partial_format` vs `strptime/full_format`, \
         `tostring/float` vs `tostring/int`). What to do: RESTORE the arm; or, if \
         it was removed because it now diverges, PIN the divergence in a named \
         `current_*_child_105` test asserting TODAY'S behaviour and update \
         REQUIRED_GENERATOR_ARMS in the SAME commit. An arm that APPEARED is new \
         coverage and belongs in the constant."
    );

    let required_names: BTreeSet<String> = REQUIRED_RANDOM_CALL_NAMES
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    assert_eq!(
        observed_names, required_names,
        "the random scalar generator's function surface drifted. A name that \
         DISAPPEARED was deleted from a `random_*` arm — pin the divergence in a \
         named `current_*_child_105` test (ADR-0017 §5) and update \
         REQUIRED_RANDOM_CALL_NAMES in the same commit; a name that APPEARED is \
         new coverage and belongs in the constant."
    );
}

#[test]
fn generated_scalar_families_are_complete_and_match() {
    use std::collections::BTreeMap;

    let conn = utc_connection();
    let event = fixed_event();
    let cases = generated_cases();
    let mut generated_counts = BTreeMap::new();
    for case in &cases {
        *generated_counts.entry(case.family).or_insert(0) += 1;
    }
    assert_eq!(
        generated_counts,
        REQUIRED_FAMILY_CASE_COUNTS
            .iter()
            .copied()
            .collect::<BTreeMap<_, _>>(),
        "generated scalar case coverage drifted. If you ADDED or deliberately \
         retired cases, update REQUIRED_FAMILY_CASE_COUNTS in the same commit. \
         If a case was DELETED to make the suite green, that is a skip moved to \
         generation time: put it back and pin the divergence in a named \
         `current_*_child_105` test instead (ADR-0017 §5)."
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
fn a_malformed_offset_has_no_timestamp_reading() {
    let conn = utc_connection();
    let event = fixed_event();
    for offset in ["+ab:cd", "+5:30"] {
        let dsl = format!(
            r#"* | let x = strptime("2026-01-15 10:20:30", "%Y-%m-%d %H:%M:%S") == "2026-01-15 10:20:30{offset}""#
        );
        // Stated as the errored/null shape, not through
        // [`assert_parity_case`]: that helper answers `None` for an
        // agreement too, so it would stay green the day a malformed
        // offset gained a reading in both lanes, which is the whole
        // claim under test.
        assert_eq!(
            eval_scalar(&dsl, &event, now_anchor()),
            EvalValue::Null,
            "streaming eval must null where batch errors for {dsl:?}"
        );
        let expected = format!(r#""2026-01-15 10:20:30{offset}" has a timestamp that is not UTC"#);
        assert_sql_errored(
            &sql_scalar_result(&conn, &dsl, &event, now_anchor()),
            &expected,
            &dsl,
        );
    }
}

#[test]
fn a_failed_timestamp_coercion_is_null_in_both_operand_orders() {
    let conn = utc_connection();
    let event = fixed_event();
    for expression in [
        r#"strptime("2026-01-15 10:20:30", "%Y-%m-%d %H:%M:%S") < "zzz""#,
        r#""zzz" > strptime("2026-01-15 10:20:30", "%Y-%m-%d %H:%M:%S")"#,
        r#"strptime("2026-01-15 10:20:30", "%Y-%m-%d %H:%M:%S") == "zzz""#,
    ] {
        let dsl = format!("* | let x = {expression}");
        assert_eq!(
            eval_scalar(&dsl, &event, now_anchor()),
            EvalValue::Null,
            "streaming eval must null where batch errors for {dsl:?}"
        );
        assert_sql_errored(
            &sql_scalar_result(&conn, &dsl, &event, now_anchor()),
            r#"invalid timestamp field format: "zzz""#,
            &dsl,
        );
    }
}

#[test]
fn now_is_one_instant_per_unit_of_output() {
    let conn = utc_connection();
    let event = fixed_event();
    let anchor = now_anchor();
    let dsl = "* | let x = now() == now()";
    assert_eq!(
        eval_scalar(dsl, &event, anchor),
        EvalValue::Bool(true),
        "two now() calls in one expression must read one instant"
    );
    assert_eq!(
        sql_scalar_result(&conn, dsl, &event, anchor),
        SqlOutcome::Value(SqlCell {
            logical_type: Type::Boolean,
            value: DuckValue::Boolean(true),
        })
    );
}

/// `assert_parity_case` proves the two lanes agree; it cannot prove they
/// agree on the instant the caller handed them, and a bug that dropped the
/// anchor's microseconds in the emitter and in `eval` alike would still
/// agree. Every expectation here is written from the fixture, so the
/// anchor's sub-second digits have to survive the trip through the bound
/// parameter.
#[test]
fn now_reads_the_statements_anchor_in_both_lanes() {
    use duckdb::types::TimeUnit;

    let conn = utc_connection();
    let event = fixed_event();
    let anchor = now_anchor();
    let micros = anchor.now_utc().timestamp_micros();
    let whole_second_micros = micros - i64::from(anchor.now_utc().timestamp_subsec_micros());
    let whole_second = chrono::DateTime::from_timestamp_micros(whole_second_micros)
        .expect("fixture infrastructure failure: the anchor is not a representable instant")
        .naive_utc();

    let instant = |micros: i64| {
        SqlOutcome::Value(SqlCell {
            logical_type: Type::Timestamp,
            value: DuckValue::Timestamp(TimeUnit::Microsecond, micros),
        })
    };
    let text = |value: &str| {
        SqlOutcome::Value(SqlCell {
            logical_type: Type::Text,
            value: DuckValue::Text(value.to_string()),
        })
    };
    let bigint = |value: i64| {
        SqlOutcome::Value(SqlCell {
            logical_type: Type::BigInt,
            value: DuckValue::BigInt(value),
        })
    };

    for (expression, eval_want, sql_want) in [
        ("now()", anchor.now_value(), instant(micros)),
        (
            "if(true, now(), now())",
            anchor.now_value(),
            instant(micros),
        ),
        (
            "coalesce(now(), now())",
            anchor.now_value(),
            instant(micros),
        ),
        (
            r#"date_trunc("second", now())"#,
            EvalValue::Timestamp(trawl_core::compare::Instant::At(whole_second)),
            instant(whole_second_micros),
        ),
        // The type, spelled: the one thing `logical_type_matches` cannot
        // see, because duckdb-rs's `Type` erases the TIMESTAMPTZ split.
        // `TIMESTAMP` is what the explicit `CAST(? AS TIMESTAMP)` in the
        // emitted SQL makes it; a bare `now()` would say `TIMESTAMP WITH
        // TIME ZONE` here.
        (
            "typeof(now())",
            EvalValue::Str("TIMESTAMP".to_string()),
            text("TIMESTAMP"),
        ),
        (
            "tostring(now())",
            EvalValue::Str(NOW_ANCHOR_RENDERED.to_string()),
            text(NOW_ANCHOR_RENDERED),
        ),
        (
            r#"strftime(now(), "%Y-%m-%d %H:%M:%S.%f")"#,
            EvalValue::Str(NOW_ANCHOR_RENDERED.to_string()),
            text(NOW_ANCHOR_RENDERED),
        ),
        (
            r#"date_diff("second", now(), now())"#,
            EvalValue::Int(0),
            bigint(0),
        ),
        (
            r#"date_part("year", now())"#,
            EvalValue::Int(2026),
            bigint(2026),
        ),
    ] {
        let dsl = format!("* | let x = {expression}");
        assert_eq!(
            eval_scalar(&dsl, &event, anchor),
            eval_want,
            "streaming must answer the anchor it was handed for {dsl:?}"
        );
        assert_eq!(
            sql_scalar_result(&conn, &dsl, &event, anchor),
            sql_want,
            "batch must answer the anchor it bound for {dsl:?}"
        );
    }
}

/// The ladder reads its argument through `to_json(...)`, so a bound
/// TIMESTAMP parameter has to stay typeable there. If it stopped being, the
/// batch lane would error and the null-where-batch-errors rule would keep
/// every parity assertion green while `sev()` over any timestamp-valued
/// expression failed for real users.
#[test]
fn sev_over_the_now_anchor_is_null_never_a_binder_error() {
    let conn = utc_connection();
    let event = fixed_event();
    let anchor = now_anchor();
    let dsl = "* | let x = sev(now())";

    assert_eq!(eval_scalar(dsl, &event, anchor), EvalValue::Null);
    assert_eq!(
        sql_scalar_result(&conn, dsl, &event, anchor),
        SqlOutcome::Value(SqlCell {
            logical_type: Type::BigInt,
            value: DuckValue::Null,
        }),
        "the anchor must survive `to_json` as a value, not a binder error"
    );
}

#[test]
fn tonumber_reads_a_boolean_the_way_try_cast_double_does() {
    let conn = utc_connection();
    let event = fixed_event();
    for expression in ["tonumber(true)", "tonumber(false)"] {
        assert_eq!(
            assert_parity_case(&conn, &event, "tonumber over a boolean", expression),
            None
        );
    }
}

#[test]
fn integer_division_is_true_division_like_duckdb() {
    let conn = utc_connection();
    let event = fixed_event();
    for expression in [
        "5 / 2",
        "-5 / 2",
        "4 / 2",
        "9007199254740993 / 1",
        "status / 3",
    ] {
        assert_eq!(
            assert_parity_case(&conn, &event, "integer true division", expression),
            None
        );
    }
}

#[test]
fn integer_overflow_nulls_where_duckdb_errors() {
    let conn = utc_connection();
    let event = fixed_event();
    for (dsl, duckdb_error) in [
        (
            "* | let x = 9223372036854775807 + 1",
            "Overflow in addition of INT64",
        ),
        (
            "* | let x = -9223372036854775807 - 2",
            "Overflow in subtraction of INT64",
        ),
        (
            "* | let x = 9223372036854775807 * 2",
            "Overflow in multiplication of INT64",
        ),
        // The one remainder with no integer answer. DuckDB calls it a
        // DIVISION overflow; `/` over the same pair is an ordinary DOUBLE.
        (
            "* | let x = (-9223372036854775807 - 1) % -1",
            "Overflow in division of",
        ),
    ] {
        assert_eq!(
            eval_scalar(dsl, &event, now_anchor()),
            EvalValue::Null,
            "streaming eval must null where batch errors for {dsl:?}"
        );
        assert_sql_errored(
            &sql_scalar_result(&conn, dsl, &event, now_anchor()),
            duckdb_error,
            dsl,
        );
    }
}

#[test]
fn an_if_or_case_condition_takes_duckdbs_boolean_domain() {
    let conn = utc_connection();
    let event = fixed_event();
    for function in ["if", "case"] {
        for condition in [
            r#""true""#,
            r#""TRUE""#,
            r#""t""#,
            r#""yes""#,
            r#""1""#,
            r#""0""#,
            r#""no""#,
            r#""nonempty""#,
            r#"" true ""#,
            r#""""#,
            r#""2""#,
            "0",
            "1",
            "-1",
            "0.0",
            "1.5",
            "true",
            "false",
            "null",
            "service",
            "status",
            r#"strptime("2024-01-01 00:00:00", "%Y-%m-%d %H:%M:%S")"#,
        ] {
            assert_eq!(
                assert_parity_case(
                    &conn,
                    &event,
                    function,
                    &format!("{function}({condition}, 1, 2)")
                ),
                None
            );
        }
    }

    for expression in [
        r#"case(true, 1, "nonempty", 2, 3)"#,
        r#"case(false, 1, "nonempty", 2, 3)"#,
        r#"case(null, 1, "nonempty", 2, 3)"#,
        r#"case("nonempty", 1, true, 2, 3)"#,
    ] {
        assert_eq!(
            assert_parity_case(&conn, &event, "case arm order", expression),
            None
        );
    }
}

/// The value both lanes give `expression`. Both must give a value:
/// `assert_parity_case` alone passes when `DuckDB` errors and the evaluator
/// nulls, which would hide a keyword one lane cannot read.
fn agreed_value(
    conn: &Connection,
    event: &Map<String, Value>,
    label: &str,
    expression: &str,
) -> EvalValue {
    assert_eq!(assert_parity_case(conn, event, label, expression), None);
    let dsl = format!("* | let x = {expression}");
    match sql_scalar_result(conn, &dsl, event, now_anchor()) {
        SqlOutcome::Value(_) => {}
        SqlOutcome::Errored(message) => panic!("{dsl:?}: DuckDB errored: {message}"),
        SqlOutcome::Skip(category) => panic!("{dsl:?}: skipped as {category:?}"),
    }
    eval_scalar(&dsl, event, now_anchor())
}

fn kleene_and(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}

fn kleene_or(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }
}

fn kleene_value(value: Option<bool>) -> EvalValue {
    value.map_or(EvalValue::Null, EvalValue::Bool)
}

#[test]
fn boolean_case_lane_parity() {
    let conn = utc_connection();
    let event = fixed_event();
    let operands = [("true", Some(true)), ("false", Some(false)), ("null", None)];
    let spell = |word: &str, capital: bool| {
        if capital {
            word.to_ascii_uppercase()
        } else {
            word.to_string()
        }
    };

    // (template, expected) with `{not}`, `{and}`, `{or}` standing for the
    // operator words and `{a}`, `{b}`, `{c}` for the operands.
    let mut rows: Vec<(String, EvalValue)> = Vec::new();
    for (a, va) in operands {
        rows.push((format!("{{not}} {a}"), kleene_value(va.map(|x| !x))));
        for (b, vb) in operands {
            rows.push((format!("{a} {{and}} {b}"), kleene_value(kleene_and(va, vb))));
            rows.push((format!("{a} {{or}} {b}"), kleene_value(kleene_or(va, vb))));
            for (c, vc) in operands {
                let not_a = va.map(|x| !x);
                rows.push((
                    format!("{{not}} {a} {{or}} {b} {{and}} {c}"),
                    kleene_value(kleene_or(not_a, kleene_and(vb, vc))),
                ));
                rows.push((
                    format!("{{not}} ({a} {{or}} {b}) {{and}} {c}"),
                    kleene_value(kleene_and(kleene_or(va, vb).map(|x| !x), vc)),
                ));
            }
        }
    }
    rows.push((
        r#"{not} status == 200 {or} service == "nginx" {and} null"#.to_string(),
        EvalValue::Null,
    ));
    rows.push((
        r#"{not} status == 500 {or} service == "nginx" {and} null"#.to_string(),
        EvalValue::Bool(true),
    ));
    rows.push((
        r#"{not} (status == 200 {or} service == "x") {and} host == "web-1""#.to_string(),
        EvalValue::Bool(false),
    ));

    for (index, (template, expected)) in rows.iter().enumerate() {
        let choices = [
            [false; 3],
            [true; 3],
            [index & 1 != 0, index & 2 != 0, index & 4 != 0],
        ];
        for [not, and, or] in choices {
            let expression = template
                .replace("{not}", &spell("not", not))
                .replace("{and}", &spell("and", and))
                .replace("{or}", &spell("or", or));
            assert_eq!(
                agreed_value(&conn, &event, "boolean keyword case", &expression),
                *expected,
                "{expression}"
            );
        }
    }
}

#[test]
fn ceil_floor_and_round_return_duckdbs_argument_driven_shapes() {
    let conn = utc_connection();
    let event = fixed_event();
    for expression in [
        "ceil(-1.5)",
        "floor(-1.5)",
        "ceil(0.0)",
        "floor(0.0)",
        "round(-1.5)",
        "ceil(5)",
        "ceil(-5)",
        "floor(5)",
        "floor(-5)",
        "round(5)",
        "round(0)",
        // An EXPLICIT zero precision takes the same DOUBLE arm as no
        // precision at all.
        "round(1.5, 0)",
        "round(2.5, 0)",
    ] {
        assert_eq!(
            assert_parity_case(&conn, &event, "numeric rounding shape", expression),
            None
        );
    }
}

#[test]
fn division_by_zero_matches_duckdbs_ieee_answer() {
    let conn = utc_connection();
    let event = fixed_event();
    for (dsl, want) in [
        ("* | let x = 1 / 0", "inf"),
        ("* | let x = -1 / 0", "-inf"),
        ("* | let x = 0 / 0", "NaN"),
        ("* | let x = 5 % 0.0", "NaN"),
        ("* | let x = 5.0 % 0", "NaN"),
    ] {
        // Rendered, not compared: `NaN != NaN` under `PartialEq`, so an
        // `assert_eq!` against a NaN can never hold and a bit-compare would
        // pin the platform's NaN payload. `{}` on f64 gives exactly
        // `inf` / `-inf` / `NaN` on both sides.
        let EvalValue::Float(eval_value) = eval_scalar(dsl, &event, now_anchor()) else {
            panic!("expected a streaming DOUBLE for {dsl:?}");
        };
        assert_eq!(format!("{eval_value}"), want, "streaming, for {dsl:?}");
        match sql_scalar_result(&conn, dsl, &event, now_anchor()) {
            SqlOutcome::Value(SqlCell {
                logical_type: Type::Double,
                value: DuckValue::Double(value),
            }) => assert_eq!(format!("{value}"), want, "batch, for {dsl:?}"),
            other => panic!("expected a DOUBLE result for {dsl:?}, got {other:?}"),
        }
    }

    let modulo = "* | let x = 5 % 0";
    assert_eq!(eval_scalar(modulo, &event, now_anchor()), EvalValue::Null);
    assert_eq!(
        sql_scalar_result(&conn, modulo, &event, now_anchor()),
        SqlOutcome::Value(SqlCell {
            logical_type: Type::BigInt,
            value: DuckValue::Null,
        })
    );
    assert_eq!(
        assert_parity_case(&conn, &event, "operators integer modulo by zero", "5 % 0"),
        None
    );
}

#[test]
fn the_epoch_reading_agrees_at_every_magnitude() {
    // Both lanes divide one microsecond count once
    // (`the_epoch_reading_is_the_micro_count_divided_once`); summing seconds
    // and a fraction instead rounds to an adjacent double. The far fixture is
    // the case that exposes it: at that magnitude an f64 ulp spans 32
    // microseconds, so the engine's answer has no fraction left while the
    // summed form invents `…799.00003`.
    let conn = utc_connection();
    let event = fixed_event();
    for expression in [
        r#"date_part("epoch", event_ts_far)"#,
        r#"date_part("epoch", event_ts)"#,
        r#"date_part("epoch", strptime("1970-01-01 00:00:00", "%Y-%m-%d %H:%M:%S"))"#,
        // Pre-1970, at second resolution. A fractional pre-1970 instant
        // is pinned bit-exactly by the probe instead: reaching one
        // through `strptime` would exercise `%f`'s own divergence
        // (chrono reads nanoseconds where DuckDB reads microseconds,
        // pinned separately) rather than the epoch reading.
        r#"date_part("epoch", strptime("1969-12-31 23:59:59", "%Y-%m-%d %H:%M:%S"))"#,
        r#"date_part("epoch", strptime("0001-01-01 00:00:00", "%Y-%m-%d %H:%M:%S"))"#,
    ] {
        assert_eq!(
            assert_parity_case(&conn, &event, "date_part epoch", expression),
            None
        );
    }
}

#[test]
fn typeof_spells_a_value_the_way_the_bound_literal_arrives() {
    // The emitter binds every literal, so an integer reaches DuckDB as BIGINT.
    // `INTEGER` is what a literal written into the SQL text answers, which
    // this lane never produces
    // (`typeof_spells_a_bound_dsl_literal_by_its_bound_type`).
    let conn = utc_connection();
    let event = fixed_event();
    for expression in [
        "typeof(1)",
        "typeof(-1)",
        "typeof(1.5)",
        "typeof(true)",
        r#"typeof("x")"#,
        "typeof(status)",
    ] {
        assert_eq!(
            assert_parity_case(&conn, &event, "typeof spelling", expression),
            None
        );
    }
}

#[test]
fn current_typeof_null_and_list_spellings_are_pinned() {
    let conn = utc_connection();
    let event = fixed_event();
    for (dsl, eval, sql) in [
        ("* | let x = typeof(null)", "NULL", "\"NULL\""),
        (
            r#"* | let x = typeof(json_keys("{\"a\":1}"))"#,
            "ARRAY",
            "VARCHAR[]",
        ),
    ] {
        assert_eq!(
            eval_scalar(dsl, &event, now_anchor()),
            EvalValue::Str(eval.to_string())
        );
        assert_eq!(
            sql_scalar_result(&conn, dsl, &event, now_anchor()),
            SqlOutcome::Value(SqlCell {
                logical_type: Type::Text,
                value: DuckValue::Text(sql.to_string()),
            })
        );
    }
}

#[test]
fn the_hostile_timestamp_corpus_agrees_in_both_lanes() {
    // `+99:99` is accepted on both sides: it is syntactically a complete
    // offset and the wall-clock cast throws it away without range-checking
    // it, a documented property of the reader rather than a gap here.
    let conn = utc_connection();
    let event = fixed_event();
    for text in ["+99:99", "+5:30", "+0530", "Z", " UTC", "+00:00", ""] {
        let expression = format!(
            r#"strptime("2026-01-15 10:20:30", "%Y-%m-%d %H:%M:%S") == "2026-01-15 10:20:30{text}""#
        );
        assert_eq!(
            assert_parity_case(&conn, &event, "hostile timestamp text", &expression),
            None
        );
    }
    for (base, text) in [
        ("1970-01-01 00:00:00", "epoch"),
        ("2026-01-15 10:20:30", "infinity"),
        ("2026-01-15 10:20:30", "-infinity"),
    ] {
        let expression = format!(r#"strptime("{base}", "%Y-%m-%d %H:%M:%S") == "{text}""#);
        assert_eq!(
            assert_parity_case(&conn, &event, "timestamp keyword", &expression),
            None
        );
    }
}

#[test]
fn percent_f_is_microseconds_in_both_lanes() {
    // `%f` is a six-digit microsecond field to DuckDB and an unscaled
    // nanosecond count to chrono (chrono would print `000123456` for
    // `123456`, and read a `.5` as five nanoseconds), so one translation
    // spells the user's format the way the engine means it, in both
    // directions.
    let conn = utc_connection();
    let event = fixed_event();
    for expression in [
        r#"strftime(strptime("2024-12-30 23:05:07.123456", "%Y-%m-%d %H:%M:%S.%f"), "%f")"#,
        r#"strftime(strptime("2024-12-30 23:05:07.123456", "%Y-%m-%d %H:%M:%S.%f"), "%Y-%m-%d %H:%M:%S.%f")"#,
        r#"strftime(strptime("2024-12-30 23:05:07", "%Y-%m-%d %H:%M:%S"), "%f")"#,
        r#"strftime(strptime("2024-12-30 23:05:07", "%Y-%m-%d %H:%M:%S"), "%%f")"#,
    ] {
        assert_eq!(
            assert_parity_case(&conn, &event, "percent f", expression),
            None
        );
    }
}

#[test]
fn accepted_bare_percent_y_strptime_residual_is_pinned() {
    let conn = utc_connection();
    let event = fixed_event();
    let dsl = r#"* | let x = tostring(strptime("24", "%y"))"#;
    assert_eq!(eval_scalar(dsl, &event, now_anchor()), EvalValue::Null);
    assert_eq!(
        sql_scalar_result(&conn, dsl, &event, now_anchor()),
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
        eval_scalar(dsl, &event, now_anchor()),
        EvalValue::Str("2024-12-30 23:05:07".to_string())
    );
    assert_eq!(
        sql_scalar_result(&conn, dsl, &event, now_anchor()),
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
        eval_scalar(dsl, &event, now_anchor()),
        EvalValue::Str("Mon Dec 30 23:05:07 2024".to_string())
    );
    assert_eq!(
        sql_scalar_result(&conn, dsl, &event, now_anchor()),
        SqlOutcome::Value(SqlCell {
            logical_type: Type::Text,
            value: DuckValue::Text("2024-12-30 23:05:07".to_string()),
        })
    );
}

#[test]
fn json_extract_returns_json_text_in_both_lanes() {
    let conn = utc_connection();
    let event = fixed_event();
    for expression in [
        r#"json_extract("{\"a\":1}", "$.a")"#,
        r#"json_extract("{\"a\":1.5}", "$.a")"#,
        r#"json_extract("{\"a\":\"x\"}", "$.a")"#,
        r#"json_extract_string("{\"a\":\"x\"}", "$.a")"#,
        r#"json_extract("{\"a\":true}", "$.a")"#,
        r#"json_extract("{\"a\":null}", "$.a")"#,
        r#"json_extract("{\"a\":1}", "$.missing")"#,
        r#"json_extract("[1,2]", "$")"#,
        r#"tostring(json_extract("[1,2]", "$"))"#,
        r#"tostring(json_extract("{\"a\":1}", "$.a"))"#,
        r#"tonumber(json_extract("{\"a\":1.5}", "$.a"))"#,
    ] {
        assert_eq!(
            assert_parity_case(&conn, &event, "json_extract text", expression),
            None
        );
    }
}

#[test]
fn duckdb_conditionals_and_coalesce_short_circuit() {
    // Evaluation order may change only if these DuckDB probes move.
    //
    // Only `error(...)` is load-bearing: DuckDB answers `inf` for `1 / 0`
    // rather than raising (see `division_by_zero_matches_duckdbs_ieee_answer`),
    // so an untaken `1 / 0` branch returns 42 under fully eager evaluation too
    // and proves nothing about short-circuiting.
    let conn = utc_connection();
    for sql in [
        "SELECT IF(false, error('untaken'), 42)",
        "SELECT CASE WHEN false THEN error('untaken') ELSE 42 END",
        "SELECT COALESCE(42, error('untaken'))",
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
        let generated = random_scalar_expr(&mut rng)
            .expect("scalar generator infrastructure failure: no family selected");

        // One implementation of the batch/eval contract: `assert_parity_case`
        // owns both the value match and the batch-errored/eval-nulls rule.
        // Comparing the eval result's `serde_json::Value` form to `Value::Null`
        // instead would be strictly weaker, since `From<EvalValue> for Value`
        // maps `Float(NaN)` and `Float(±inf)` onto `Value::Null` and would
        // accept an eval NaN/inf where DuckDB errored.
        match assert_parity_case(
            &conn,
            &event,
            &format!("iteration {i}"),
            &generated.expression,
        ) {
            Some(SkipCategory::HugeIntOutsideI64) => hugeint_skips += 1,
            Some(SkipCategory::Blob) => blob_skips += 1,
            None => passed += 1,
        }
    }

    eprintln!(
        "scalar parity: {passed} passed, hugeint_skips={hugeint_skips}, blob_skips={blob_skips}"
    );
    assert_eq!(hugeint_skips, 0, "HugeInt skip ceiling exceeded");
    assert_eq!(blob_skips, 0, "Blob skip ceiling exceeded");
}

#[test]
fn batch_errors_imply_streaming_null() {
    let conn = utc_connection();
    let event = fixed_event();
    for (dsl, duckdb_error) in [
        (
            r#"* | let x = replace("abc", "a", 5)"#,
            "'replace(STRING_LITERAL, STRING_LITERAL, BIGINT)'",
        ),
        ("* | let x = service + 1", "'+(VARCHAR, UNKNOWN)'"),
        (
            r#"* | let x = round("abc")"#,
            r#"the function call "round(STRING_LITERAL)""#,
        ),
    ] {
        assert_sql_errored(
            &sql_scalar_result(&conn, dsl, &event, now_anchor()),
            duckdb_error,
            dsl,
        );
        // `EvalValue::Null`, never the `serde_json::Value` form: that
        // conversion maps `Float(NaN)`/`Float(inf)` onto `Value::Null` too,
        // so the weaker form would accept a non-null eval here.
        assert_eq!(
            eval_scalar(dsl, &event, now_anchor()),
            EvalValue::Null,
            "streaming eval must yield Null where batch errors for {dsl:?}"
        );
    }
}

#[test]
fn strftime_over_strptime_binds_params_in_order() {
    let conn = utc_connection();
    let event = fixed_event();
    let dsl = r#"* | let x = strftime(strptime("2023-11-07 17:30:45", "%Y-%m-%d %H:%M:%S"), "%Y")"#;

    assert_eq!(
        sql_scalar_result(&conn, dsl, &event, now_anchor()),
        SqlOutcome::Value(SqlCell {
            logical_type: Type::Text,
            value: DuckValue::Text("2023".to_string()),
        }),
        "batch strftime(strptime(...)) must not error or null"
    );

    // Streaming path (eval). Compared as an `EvalValue`, never through its
    // `serde_json::Value` form: `From<EvalValue> for Value` maps BOTH `Str` and
    // `Timestamp` onto `Value::String`, so the serde form could not tell a
    // rendered string from a rendered instant.
    assert_eq!(
        eval_scalar(dsl, &event, now_anchor()),
        EvalValue::Str("2023".to_string()),
        "streaming strftime(strptime(...)) must not error or null"
    );
}

#[test]
fn strptime_partial_formats_match_duckdb() {
    let conn = utc_connection();
    let event = fixed_event();
    for (dsl, want) in [
        (
            r#"* | let x = strftime(strptime("2023-11-07", "%Y-%m-%d"), "%Y-%m-%d %H:%M:%S")"#,
            "2023-11-07 00:00:00",
        ),
        (
            r#"* | let x = strftime(strptime("14:30:00", "%H:%M:%S"), "%Y-%m-%d %H:%M:%S")"#,
            "1900-01-01 14:30:00",
        ),
        (
            r#"* | let x = strftime(strptime("2023", "%Y"), "%Y-%m-%d %H:%M:%S")"#,
            "2023-01-01 00:00:00",
        ),
        (
            r#"* | let x = strftime(strptime("2023-11", "%Y-%m"), "%Y-%m-%d %H:%M:%S")"#,
            "2023-11-01 00:00:00",
        ),
        (
            r#"* | let x = strftime(strptime("11-07", "%m-%d"), "%Y-%m-%d %H:%M:%S")"#,
            "1900-11-07 00:00:00",
        ),
        (
            r#"* | let x = strftime(strptime("2023-11-07 14", "%Y-%m-%d %H"), "%Y-%m-%d %H:%M:%S")"#,
            "2023-11-07 14:00:00",
        ),
    ] {
        assert_eq!(
            sql_scalar_result(&conn, dsl, &event, now_anchor()),
            SqlOutcome::Value(SqlCell {
                logical_type: Type::Text,
                value: DuckValue::Text(want.to_string()),
            }),
            "batch strptime partial-format must fill like DuckDB for {dsl:?}"
        );

        assert_eq!(
            eval_scalar(dsl, &event, now_anchor()),
            EvalValue::Str(want.to_string()),
            "streaming strptime partial-format must match DuckDB for {dsl:?}"
        );
    }
}

fn text(value: &str) -> EvalValue {
    EvalValue::Str(value.to_string())
}

fn keys(names: &[&str]) -> EvalValue {
    EvalValue::Array(names.iter().map(|name| text(name)).collect())
}

/// One document of the guard's parity table: (label, DSL document
/// literal, then the expected value of `json(doc, "$")`,
/// `json_extract(doc, "$")`, `json(doc, "$.k")`, `json_extract(doc, "$.k")`,
/// `json_keys(doc)` and `json_array_length(doc)`).
type JsonDocumentCase = (&'static str, &'static str, [EvalValue; 6]);

#[allow(clippy::too_many_lines)]
fn json_document_cases() -> Vec<JsonDocumentCase> {
    let null = EvalValue::Null;
    vec![
        (
            "plain text",
            r#""plain text line""#,
            std::array::from_fn(|_| null.clone()),
        ),
        (
            "zero-byte string",
            r#""""#,
            std::array::from_fn(|_| null.clone()),
        ),
        (
            "trailing text",
            r#""{\"k\":1} x""#,
            std::array::from_fn(|_| null.clone()),
        ),
        (
            "unterminated object",
            r#""{""#,
            std::array::from_fn(|_| null.clone()),
        ),
        ("SQL null", "null", std::array::from_fn(|_| null.clone())),
        (
            "the JSON string \"\"",
            r#""\"\"""#,
            [
                text(""),
                text(r#""""#),
                null.clone(),
                null.clone(),
                keys(&[]),
                EvalValue::Int(0),
            ],
        ),
        (
            "object",
            r#""{\"k\":1}""#,
            [
                text(r#"{"k":1}"#),
                text(r#"{"k":1}"#),
                text("1"),
                text("1"),
                keys(&["k"]),
                EvalValue::Int(0),
            ],
        ),
        (
            "object holding a JSON null",
            r#""{\"k\":null}""#,
            [
                text(r#"{"k":null}"#),
                text(r#"{"k":null}"#),
                // `json_extract_string` reads a JSON null as SQL null;
                // `json_extract` keeps the JSON text.
                null.clone(),
                text("null"),
                keys(&["k"]),
                EvalValue::Int(0),
            ],
        ),
        (
            "object holding a string",
            r#""{\"k\":\"v\"}""#,
            [
                text(r#"{"k":"v"}"#),
                text(r#"{"k":"v"}"#),
                text("v"),
                text(r#""v""#),
                keys(&["k"]),
                EvalValue::Int(0),
            ],
        ),
        (
            "array",
            r#""[1,2,3]""#,
            [
                text("[1,2,3]"),
                text("[1,2,3]"),
                null.clone(),
                null.clone(),
                keys(&[]),
                EvalValue::Int(3),
            ],
        ),
        (
            "string",
            r#""\"str\"""#,
            [
                text("str"),
                text(r#""str""#),
                null.clone(),
                null.clone(),
                keys(&[]),
                EvalValue::Int(0),
            ],
        ),
        (
            "number",
            r#""42""#,
            [
                text("42"),
                text("42"),
                null.clone(),
                null.clone(),
                keys(&[]),
                EvalValue::Int(0),
            ],
        ),
        (
            "JSON null",
            r#""null""#,
            [
                null.clone(),
                text("null"),
                null.clone(),
                null.clone(),
                keys(&[]),
                EvalValue::Int(0),
            ],
        ),
    ]
}

#[test]
fn a_document_that_is_not_json_is_null_in_both_lanes() {
    let conn = utc_connection();
    let event = fixed_event();
    for (label, doc, [root_string, root_json, k_string, k_json, doc_keys, length]) in
        json_document_cases()
    {
        for (expression, expected) in [
            (format!(r#"json({doc}, "$")"#), &root_string),
            (format!(r#"json_extract_string({doc}, "$")"#), &root_string),
            (format!(r#"json_extract({doc}, "$")"#), &root_json),
            (format!(r#"json({doc}, "$.k")"#), &k_string),
            (format!(r#"json_extract_string({doc}, "$.k")"#), &k_string),
            (format!(r#"json_extract({doc}, "$.k")"#), &k_json),
            (format!("json_keys({doc})"), &doc_keys),
            (format!("json_array_length({doc})"), &length),
        ] {
            assert_eq!(
                &agreed_value(&conn, &event, label, &expression),
                expected,
                "{label}: {expression}"
            );
        }
    }
}

#[test]
fn an_error_inside_the_json_document_still_raises() {
    let conn = utc_connection();
    let event = fixed_event();
    let overflow = "tostring(9223372036854775807 + 1)";
    let conversion = r#"if("nonempty", "{}", "[]")"#;
    for (document, duckdb_error) in [
        (overflow, "Overflow in addition of INT64"),
        (conversion, "Could not convert string 'nonempty' to BOOL"),
    ] {
        for call in [
            format!(r#"json({document}, "$")"#),
            format!(r#"json_extract_string({document}, "$")"#),
            format!(r#"json_extract({document}, "$")"#),
            format!("json_keys({document})"),
            format!("json_array_length({document})"),
        ] {
            let dsl = format!("* | let x = {call}");
            assert_sql_errored(
                &sql_scalar_result(&conn, &dsl, &event, now_anchor()),
                duckdb_error,
                &dsl,
            );
            assert_eq!(
                eval_scalar(&dsl, &event, now_anchor()),
                EvalValue::Null,
                "streaming eval must null where batch errors for {dsl:?}"
            );
        }
    }
}

fn batch_value(conn: &Connection, event: &Map<String, Value>, expression: &str) -> DuckValue {
    let dsl = format!("* | let x = {expression}");
    match sql_scalar_result(conn, &dsl, event, now_anchor()) {
        SqlOutcome::Value(cell) => cell.value,
        other => panic!("{dsl:?}: expected a batch value, got {other:?}"),
    }
}

fn duck_text(value: &str) -> DuckValue {
    DuckValue::Text(value.to_string())
}

fn eval_value(event: &Map<String, Value>, expression: &str) -> EvalValue {
    eval_scalar(&format!("* | let x = {expression}"), event, now_anchor())
}

#[test]
fn current_json_keys_order_residual_is_pinned() {
    let conn = utc_connection();
    let event = fixed_event();
    let expression = r#"json_keys("{\"b\":1,\"a\":2}")"#;
    assert_eq!(
        batch_value(&conn, &event, expression),
        DuckValue::List(vec![duck_text("b"), duck_text("a")])
    );
    assert_eq!(eval_value(&event, expression), keys(&["a", "b"]));
}

#[test]
fn current_json_duplicate_key_residual_is_pinned() {
    let conn = utc_connection();
    let event = fixed_event();
    let doc = r#""{\"a\":1,\"a\":2}""#;
    let path = format!(r#"json({doc}, "$.a")"#);
    assert_eq!(batch_value(&conn, &event, &path), duck_text("1"));
    assert_eq!(eval_value(&event, &path), text("2"));
    let listed = format!("json_keys({doc})");
    assert_eq!(
        batch_value(&conn, &event, &listed),
        DuckValue::List(vec![duck_text("a"), duck_text("a")])
    );
    assert_eq!(eval_value(&event, &listed), keys(&["a"]));
}

#[test]
fn current_lenient_json_document_residual_is_pinned() {
    let conn = utc_connection();
    let event = fixed_event();
    for (doc, batch_keys) in [
        (r#""[1,2,]""#, vec![]),
        (r#""{\"a\":NaN}""#, vec![duck_text("a")]),
        (r#""{\"a\":1e999}""#, vec![duck_text("a")]),
    ] {
        let listed = format!("json_keys({doc})");
        assert_eq!(
            batch_value(&conn, &event, &listed),
            DuckValue::List(batch_keys),
            "{listed}"
        );
        assert_eq!(eval_value(&event, &listed), EvalValue::Null, "{listed}");
    }
    let nan = r#"json("{\"a\":NaN}", "$.a")"#;
    assert_eq!(batch_value(&conn, &event, nan), duck_text("NaN"));
    assert_eq!(eval_value(&event, nan), EvalValue::Null);
}

#[test]
fn current_non_text_json_document_residual_is_pinned() {
    let conn = utc_connection();
    let event = fixed_event();
    let root = r#"json(42, "$")"#;
    assert_eq!(batch_value(&conn, &event, root), duck_text("42"));
    assert_eq!(eval_value(&event, root), EvalValue::Null);
    let listed = "json_keys(42)";
    assert_eq!(batch_value(&conn, &event, listed), DuckValue::List(vec![]));
    assert_eq!(eval_value(&event, listed), EvalValue::Null);
}

fn dsl_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// One row of the accepted-path table: (JSON document, path, expected
/// `json`/`json_extract_string` value, expected `json_extract` value),
/// `None` for SQL null. Each target is a scalar, an array or a
/// single-key object, so key order cannot decide the answer.
type JsonPathCase = (
    &'static str,
    &'static str,
    Option<&'static str>,
    Option<&'static str>,
);

const JSON_PATH_CASES: &[JsonPathCase] = &[
    (r#"{"k":1}"#, "$", Some(r#"{"k":1}"#), Some(r#"{"k":1}"#)),
    (r#"{"k":1}"#, "", Some(r#"{"k":1}"#), Some(r#"{"k":1}"#)),
    (r#"{"k":"v"}"#, "$.k", Some("v"), Some(r#""v""#)),
    (r#"{"a":{"b":2}}"#, "$.a.b", Some("2"), Some("2")),
    (
        r#"{"user-agent":"curl"}"#,
        "$.user-agent",
        Some("curl"),
        Some(r#""curl""#),
    ),
    (r#"{"@t":"x"}"#, "$.@t", Some("x"), Some(r#""x""#)),
    (r#"{"é":1}"#, "$.é", Some("1"), Some("1")),
    (
        r#"{"a.b":1,"a":{"b":2}}"#,
        r#"$."a.b""#,
        Some("1"),
        Some("1"),
    ),
    (r#"{"a b":[1]}"#, r#"$."a b""#, Some("[1]"), Some("[1]")),
    (
        r#"{"q\"x":true}"#,
        r#"$."q\"x""#,
        Some("true"),
        Some("true"),
    ),
    ("[7,8]", "$[0]", Some("7"), Some("7")),
    ("[7,8]", "$[01]", Some("8"), Some("8")),
    ("[7,8,9]", "$[-1]", Some("9"), Some("9")),
    ("[7,8,9]", "$[#-2]", Some("8"), Some("8")),
    (r#"{"m":[[1,2]]}"#, "$.m[0][1]", Some("2"), Some("2")),
    ("[7,8]", "$[0000000000000000001]", Some("8"), Some("8")),
    ("[7,8]", "$[-0000000000000000001]", Some("8"), Some("8")),
    ("[7,8]", "$[#-0000000000000000001]", Some("8"), Some("8")),
    (r#"{"0":1}"#, "$[0]", None, None),
    ("[7,8]", "$[2]", None, None),
    ("[7,8]", "$[-3]", None, None),
    (r#"{"k":[1]}"#, "/k", Some("[1]"), Some("[1]")),
    (r#"{"m":[[1,2]]}"#, "/m/0/1", Some("2"), Some("2")),
    (r#"{"a/b":1}"#, "/a~1b", Some("1"), Some("1")),
    (r#"{"a~b":1}"#, "/a~0b", Some("1"), Some("1")),
    (r#"{"":1}"#, "/", Some("1"), Some("1")),
    (r#"{"k":1}"#, "/", None, None),
    (r#"{"*":1}"#, "/*", Some("1"), Some("1")),
    (r#"{"0":"o"}"#, "/0", Some("o"), Some(r#""o""#)),
    ("[5,6]", "/0", Some("5"), Some("5")),
    ("[5,6]", "/01", None, None),
    (
        r#"{"level":"info"}"#,
        "level",
        Some("info"),
        Some(r#""info""#),
    ),
    (
        r#"{"user-agent":"curl"}"#,
        "user-agent",
        Some("curl"),
        Some(r#""curl""#),
    ),
    (r#"{"k":null}"#, "$.k", None, Some("null")),
];

#[test]
fn every_accepted_json_path_selects_the_same_value_in_both_lanes() {
    let conn = utc_connection();
    let event = fixed_event();
    let value = |want: Option<&str>| want.map_or(EvalValue::Null, text);
    for &(doc, path, string, json) in JSON_PATH_CASES {
        trawl_core::json_path::parse(path)
            .unwrap_or_else(|e| panic!("{path:?}: the grammar refused it: {e:?}"));
        let (doc, path_literal) = (dsl_string(doc), dsl_string(path));
        for (call, want) in [
            (format!("json({doc}, {path_literal})"), value(string)),
            (
                format!("json_extract_string({doc}, {path_literal})"),
                value(string),
            ),
            (format!("json_extract({doc}, {path_literal})"), value(json)),
        ] {
            assert_eq!(
                agreed_value(&conn, &event, "accepted JSON path", &call),
                want,
                "{call}"
            );
        }
    }
}
