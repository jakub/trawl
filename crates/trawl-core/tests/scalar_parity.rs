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
// "epoch" is filtered out at generation time because eval and DuckDB round the
// same instant to ADJACENT f64 values — nothing to do with text any more, the
// harness compares owned `DuckValue`s. The divergence is pinned, not hidden, by
// `current_date_part_epoch_precision_is_pinned_child_105` — see
// `random_date_part`.

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

// ── Random-generator completeness surface ─────────────────────────────
//
// `generated_cases()` carries a per-family case-count manifest; the RANDOM
// generator carried NOTHING, which is how `ceil`/`floor` were dropped from
// `random_numeric_fn` without a single test going red. A deleted generator arm
// is a skip moved to generation time — exactly what ADR-0017 §5 outlaws — so
// the random side now has two mechanical guards, both asserted by
// `random_generator_surface_is_complete`:
//
//  1. every selector's arm count is a NAMED constant checked against a required
//     value — the cheap first line, so narrowing one cannot be an invisible
//     literal edit;
//  2. every generator ARM carries a stable label, the labels drawn over a
//     deterministic sweep are checked for SET EQUALITY against
//     [`REQUIRED_GENERATOR_ARMS`], and BRANCH IDENTITY — not the head symbol of
//     the emitted text — is what that set is made of. Two arms can share a head
//     symbol (`strptime` over a full vs. a PARTIAL format, `tostring` over an
//     int vs. a FLOAT) and are distinct coverage classes: deleting either one
//     leaves the function-name set below completely unchanged, which is the F1
//     failure mode surviving one level down. It does not survive this guard; and
//  3. the set of scalar function NAMES the generator actually emits is observed
//     from the generated TEXT and checked for set equality too. A label is the
//     generator's self-report and could lie about what its arm emits; this third
//     projection reads the expression itself, so it catches an arm rewritten to
//     emit something else under the same label.

const SCALAR_FAMILY_ARMS: usize = 15;
const STRING_FN_ARMS: usize = 8;
const SEV_ARG_ARMS: usize = 5;
const SEV_DIALECT_ARMS: usize = 3;
const CONCAT_ARG_ARMS: usize = 3;
const SUBSTR_WINDOW_ARMS: usize = 17;
const STRPTIME_SHAPE_ARMS: usize = 4;

/// (selector name, the constant the generator uses, the value it must hold).
const REQUIRED_SELECTOR_ARMS: &[(&str, usize, usize)] = &[
    ("random_scalar_expr family", SCALAR_FAMILY_ARMS, 15),
    ("random_string_fn", STRING_FN_ARMS, 8),
    ("random_numeric_fn", NUMERIC_FN_ARMS, 2),
    ("random_sev argument", SEV_ARG_ARMS, 5),
    ("random_sev dialect", SEV_DIALECT_ARMS, 3),
    ("random_concat argument", CONCAT_ARG_ARMS, 3),
    ("random_substr window", SUBSTR_WINDOW_ARMS, 17),
    ("random_strptime shape", STRPTIME_SHAPE_ARMS, 4),
];

/// A generated expression together with the generator ARMS that produced it.
///
/// `arms` is the guard's unit of identity. Most arms contribute exactly one
/// label; an arm that composes INDEPENDENT sub-choices contributes one per
/// choice (`sev` picks an argument shape and a dialect; `concat` picks a kind
/// per argument), so deleting either axis is visible on its own. Duplicates are
/// fine — the guard compares SETS.
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

    /// Record a second, independent choice this arm made.
    fn and(mut self, arm: &'static str) -> Self {
        self.arms.push(arm);
        self
    }
}

/// Every coverage class the random generator can draw. One label per BRANCH,
/// so a deleted arm is a missing label even when its neighbours keep emitting
/// the same function name.
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
    "numeric_fn/abs",
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
];

/// Every scalar function `random_scalar_expr` can put at the HEAD of a
/// generated expression — the THIRD projection, read off the emitted text
/// rather than off an arm's self-reported label. `ceil`/`floor` are absent on
/// purpose — see [`NUMERIC_FN_ARMS`].
const REQUIRED_RANDOM_CALL_NAMES: &[&str] = &[
    "abs",
    "coalesce",
    "concat",
    "contains",
    "date_diff",
    "date_part",
    "date_trunc",
    "if",
    "length",
    "lower",
    "ltrim",
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

/// The function name at the head of a generated expression: every `random_*`
/// arm emits a `name(...)` call, so the text before the first `(` names it.
fn leading_call_name(expression: &str) -> &str {
    let (name, _) = expression
        .split_once('(')
        .unwrap_or_else(|| panic!("generated expression is not a call: {expression:?}"));
    name
}

// ── DSL expression generation ─────────────────────────────────────────

/// Generate a scalar DSL expression (no `now()`, no field refs that could be
/// absent from the fixed event), labelled with the arms that produced it.
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

/// Arm count of `random_numeric_fn`'s selector.
///
/// `ceil`/`floor` are DELIBERATELY absent, and this is the only place that says
/// so. Over an INTEGER argument they are a LIVE divergence — eval answers
/// `Int(5)` while `DuckDB` answers `DOUBLE 5.0` (`typeof(ceil(5))` is DOUBLE,
/// `typeof(round(5))` is BIGINT) — so generating them would turn the property
/// loop red. The divergence is PINNED rather than skipped, by
/// `current_ceil_floor_and_round_types_are_pinned_child_105`; when #105 fixes
/// the return types, restore both arms and set this back to 4.
const NUMERIC_FN_ARMS: usize = 2;

fn random_numeric_fn(rng: &mut Rng) -> Generated {
    let i = int_lit(rng);
    match rng.range(NUMERIC_FN_ARMS) {
        0 => Generated::new("numeric_fn/abs", format!("abs({i})")),
        1 => Generated::new("numeric_fn/round", format!("round({i})")),
        _ => unreachable!(),
    }
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
    // Exclude "epoch": eval and DuckDB round the same instant to adjacent f64
    // values (a value-domain divergence, not a rendering one — the harness
    // stopped comparing through text), pinned by
    // `current_date_part_epoch_precision_is_pinned_child_105`. Everything else
    // in the emitter allowlist is fair game, so a newly-added unit flows in
    // here without a test edit.
    let units: Vec<&&str> = DATE_PART_UNITS.iter().filter(|u| **u != "epoch").collect();
    let unit = rng.pick(&units);
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
        // Unparseable input against a valid format: batch (TRY_STRPTIME) and
        // streaming both yield NULL, so the two paths agree on a data-parse
        // failure (TRY_STRPTIME nulls instead of erroring the whole query).
        return Generated::new(
            "strptime/unparseable",
            "strptime(\"not-a-date\", \"%Y-%m-%d %H:%M:%S\")".to_string(),
        );
    }
    if rng.range(STRPTIME_SHAPE_ARMS) == 0 {
        // Partial format: streaming fills omitted components from the
        // 1900-01-01 00:00:00 base exactly like DuckDB. Render through strftime
        // so the filled timestamp is compared as a string. Its head symbol is
        // `strftime`, exactly like the full-format arm below — which is why the
        // completeness guard tracks THIS label and not that name.
        let (input, fmt) = rng.pick(STRPTIME_PARTIALS);
        return Generated::new(
            "strptime/partial_format",
            format!("strftime(strptime(\"{input}\", \"{fmt}\"), \"%Y-%m-%d %H:%M:%S\")"),
        );
    }
    let ts = rng.pick(TS_VALS);
    // strptime returns a Timestamp — wrap it in strftime to get a string for comparison
    Generated::new(
        "strptime/full_format",
        format!("strftime(strptime(\"{ts}\", \"%Y-%m-%d %H:%M:%S\"), \"%Y-%m-%d %H:%M:%S\")"),
    )
}

fn random_tonumber(rng: &mut Rng) -> Generated {
    if rng.bool() {
        // Digit-separator strings: `1_000`/`1_0.0_5` parse to a value in BOTH
        // paths; `_1000` is unparseable in both (DuckDB returns NULL -> skipped).
        // Proves the underscore-strip rule stays in DuckDB TRY_CAST parity.
        let s = rng.pick(&["1_000", "1_0.0_5", "_1000"]);
        Generated::new(
            "tonumber/digit_separator_text",
            format!("tonumber(\"{s}\")"),
        )
    } else {
        // Use integer string literals so the result is exact
        let n = rng.pick(INT_VALS).unsigned_abs();
        Generated::new("tonumber/integer_text", format!("tonumber(\"{n}\")"))
    }
}

fn random_tostring(rng: &mut Rng) -> Generated {
    // Mix ints AND floats: float text rendering (1.0 -> "1.0", 1e16 -> "1e+16")
    // is the F1 divergence this exercises. The String arm of values_match does
    // an exact compare, so DuckDB CAST(DOUBLE AS VARCHAR) must byte-match eval.
    // Both arms emit a `tostring` call, so only their LABELS tell them apart.
    if rng.bool() {
        Generated::new("tostring/int", format!("tostring({})", int_lit(rng)))
    } else {
        Generated::new("tostring/float", format!("tostring({})", float_lit(rng)))
    }
}

/// `substr(s, start [, len])` with negative/zero starts and out-of-range
/// lengths — the F2 window-semantics blind spot. start in -8..=8, optional
/// len in -8..=8 (incl. 0). Closes the gap that let `DuckDB`'s from-end /
/// leftward-window behaviour drift from the streaming evaluator.
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
fn random_sev(rng: &mut Rng) -> Generated {
    // Argument shape and dialect are INDEPENDENT choices, so each contributes
    // its own label: dropping `syslog`, or dropping the `level` column arm,
    // has to be visible on its own rather than masked by the other axis.
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

fn random_typeof(rng: &mut Rng) -> Generated {
    // typeof on strings only: DuckDB returns BIGINT for integer literals (it
    // widens all integer literals to BIGINT) while eval returns INTEGER.
    // This is a documented eval/DuckDB divergence for typeof — only test
    // varchar inputs where both agree on "VARCHAR".
    Generated::new("typeof", format!("typeof({})", str_lit(rng)))
}

fn random_coalesce(rng: &mut Rng) -> Generated {
    let a = str_lit(rng);
    let b = str_lit(rng);
    Generated::new("coalesce", format!("coalesce({a}, {b})"))
}

/// 2–4 concat args, each a string/int literal or a bare `null`. Exercises
/// `DuckDB`'s CONCAT NULL-skipping and CAST-to-VARCHAR join against the
/// streaming evaluator (the #22 batch-vs-live drift this fix closes).
/// (Floats omitted: `DuckDB` float→text rendering differs from Rust's.)
fn random_concat(rng: &mut Rng) -> Generated {
    let n = 2 + rng.range(3); // 2..=4 args
    // Labelled per ARGUMENT kind, so dropping the `null` argument arm — the
    // whole point of the family — cannot hide behind the surviving string/int
    // arguments of the same `concat(...)` call.
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
    /// `DuckDB` raised an error (prepare/query failed), carrying the error TEXT.
    /// The streaming evaluator cannot error, so its contract is to yield `Null`
    /// wherever batch errors — the property loop asserts that rather than
    /// silently skipping. The text is carried (not discarded) for two reasons: a
    /// mismatch prints the reason, and a pinned divergence test can assert WHICH
    /// error it pinned, so a different error cannot keep the pin green.
    Errored(String),
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
/// so the harness can no longer hide a batch error behind a silent skip — at
/// prepare, at query AND at fetch. Only genuinely impossible states (no row, no
/// `x` column, unreadable text) panic as infrastructure failures.
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

    // A prepare/query failure is a real DuckDB ERROR, not a skip — and the
    // message is kept, so a pin cannot pass for the wrong reason.
    let mut stmt = match conn.prepare(&emitted.sql) {
        Ok(stmt) => stmt,
        Err(error) => return SqlOutcome::Errored(error.to_string()),
    };
    let mut rows = match stmt.query(param_refs.as_slice()) {
        Ok(rows) => rows,
        Err(error) => return SqlOutcome::Errored(error.to_string()),
    };
    // Three OUTCOMES, never two: a fetch-time `Err` is a real DuckDB execution
    // error and must classify as one, exactly like the prepare/query failures
    // above — collapsing it into the empty-result panic would make an overflow
    // pin panic with the wrong reason instead of returning `Errored`. Measured
    // against duckdb-rs 1.10505.0 today, `query()` materializes the result and
    // every runtime error surfaces there (probed: an `error('boom')` guarded by
    // a row predicate 2M rows in still errors at `query()`), so this arm is
    // currently unreachable — which is a fact about THIS DuckDB, not a licence
    // to launder a future streaming error into the wrong outcome.
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

// ── Value comparison ──────────────────────────────────────────────────

/// Explicitly permitted representation widenings between streaming and batch.
/// There is deliberately no string/numeric coercion and no timestamp/text arm.
///
/// CAVEAT, and #106 must read it: `sql` is duckdb-rs's `Type`, converted from
/// the result column's Arrow `DataType`, and that conversion ERASES both the
/// time unit and the time zone — `DataType::Timestamp(_, _) => Type::Timestamp`
/// covers TIMESTAMP and TIMESTAMPTZ alike. This table therefore CANNOT
/// distinguish them, which is exactly the split ADR-0017 §3 / #106 call
/// observable when `now()` becomes a bound TIMESTAMP parameter. #106 must
/// assert that distinction TEXTUALLY — `typeof(now())` compared as a string —
/// and must not expect this widening table to reject a TIMESTAMPTZ.
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
/// a DIFFERENT reason (a binder change, a renamed function, a new overflow
/// path) — the silent drift this harness exists to catch. Every pinned
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

/// Per-family case counts — the ONLY thing standing between this table and an
/// F1-style silent deletion. Equal `BTreeMap`s imply equal key sets, so this
/// subsumes a separate family-name manifest; there is deliberately not one.
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

/// The random generator's own completeness guard — see the
/// "Random-generator completeness surface" block above.
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
fn current_second_timestamp_parser_accepts_nondigit_offset_child_105() {
    // #105 deletes eval's second timestamp parser in favour of the probed owner.
    let conn = utc_connection();
    let event = fixed_event();
    let dsl = r#"* | let x = strptime("2026-01-15 10:20:30", "%Y-%m-%d %H:%M:%S") == "2026-01-15 10:20:30+ab:cd""#;
    assert_eq!(eval_scalar(dsl, &event), EvalValue::Bool(true));
    assert_sql_errored(
        &sql_scalar_result(&conn, dsl, &event),
        r#""2026-01-15 10:20:30+ab:cd" has a timestamp that is not UTC"#,
        dsl,
    );
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
        assert_sql_errored(
            &sql_scalar_result(&conn, dsl, &event),
            r#"invalid timestamp field format: "zzz""#,
            dsl,
        );
    }
}

/// The two ways [`current_now_is_sampled_per_call_child_106`] can fail, told
/// apart instead of guessed at.
///
/// That pin needs ONE sample out of 100 in which two `now()` calls inside one
/// expression read different instants, so it depends on the platform clock
/// ticking between two back-to-back reads. A failure therefore has two possible
/// causes with OPPOSITE remedies: #106 landed and the pin must be flipped, or
/// this host's clock is too coarse to observe per-call sampling at all. The
/// message asks the clock which one it is, with the same read `eval` makes
/// (`eval.rs`: `chrono::Utc::now().naive_utc()`).
///
/// Measured on the development host (nanosecond `CLOCK_REALTIME`): worst case 1
/// attempt over 2000 trials, so the coarse-clock branch is not a live flake
/// here. #106 (ADR-0017 §3) owns removing the timing dependence outright.
fn per_call_now_failure_message() -> String {
    let clock_advances = (0..1_000).any(|_| {
        let first = chrono::Utc::now().naive_utc();
        let second = chrono::Utc::now().naive_utc();
        first != second
    });
    if clock_advances {
        "eval's now() no longer varies between calls in one expression, and this \
         platform's clock DOES advance between two back-to-back reads (probed \
         right here) — so what changed is the SAMPLING, not the timer. This is \
         what #106 (ADR-0017 §3) lands: flip this test to assert ONE instant per \
         unit of output instead of a per-call sample, and retire the child-106 \
         pin."
            .to_string()
    } else {
        "this platform's clock is too COARSE to observe per-call sampling: 1000 \
         back-to-back `chrono::Utc::now()` reads never differed, so two `now()` \
         calls in one expression cannot be told apart on this host and the pin \
         cannot make its observation. Nothing is known about eval's sampling from \
         this run — do NOT weaken or skip the pin to make it green. #106 \
         (ADR-0017 §3) removes the timing dependence entirely; until it lands, \
         this pin is unobservable here."
            .to_string()
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
    // Built ONLY on failure, and it names WHICH of the two causes this is — see
    // `per_call_now_failure_message`.
    assert!(
        saw_per_call_difference,
        "{}",
        per_call_now_failure_message()
    );
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
    for (dsl, release_wrap, duckdb_error) in [
        (
            "* | let x = 9223372036854775807 + 1",
            i64::MIN,
            "Overflow in addition of INT64",
        ),
        (
            "* | let x = -9223372036854775807 - 2",
            i64::MAX,
            "Overflow in subtraction of INT64",
        ),
        (
            "* | let x = 9223372036854775807 * 2",
            -2,
            "Overflow in multiplication of INT64",
        ),
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
        assert_sql_errored(&sql_scalar_result(&conn, dsl, &event), duckdb_error, dsl);
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
        assert_sql_errored(
            &sql_scalar_result(&conn, dsl, &event),
            "Could not convert string 'nonempty' to BOOL",
            dsl,
        );
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
        // INTEGER inputs — ADR-0017 §5 names them explicitly. `typeof(ceil(5))`
        // is DOUBLE while `typeof(round(5))` is BIGINT, so ceil/floor diverge
        // over an integer argument and round does not. That asymmetry is why
        // `random_numeric_fn` may still generate `round` and may NOT generate
        // `ceil`/`floor` (see NUMERIC_FN_ARMS) — the exclusion is pinned here,
        // not skipped there.
        ("* | let x = ceil(5)", 5, 5.0),
        ("* | let x = ceil(-5)", -5, -5.0),
        ("* | let x = floor(5)", 5, 5.0),
        ("* | let x = floor(-5)", -5, -5.0),
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

    // The contrast, and the reason `round` stays in the random generator:
    // `round` over an INTEGER already AGREES (BIGINT on both sides), so #105
    // must leave it alone.
    assert_eq!(
        assert_parity_case(&conn, &event, "numeric round over integer", "round(5)"),
        None
    );
}

#[test]
fn current_division_by_zero_diverges_child_105() {
    // #105 owns the division value domain, and its scope text currently claims
    // division by zero is NULL in BOTH lanes. It is not. Measured today: eval
    // nulls every division by zero, while DuckDB's `/` promotes to DOUBLE and
    // answers an IEEE special. Codifying the scope text would be a THIRD
    // semantics, which ADR-0017 §4 forbids — these are the values to fix
    // toward.
    let conn = utc_connection();
    let event = fixed_event();
    for (dsl, sql) in [
        ("* | let x = 1 / 0", "inf"),
        ("* | let x = -1 / 0", "-inf"),
        ("* | let x = 0 / 0", "NaN"),
        // Float `%` takes the same DOUBLE path as `/` — only the all-integer
        // shape below agrees.
        ("* | let x = 5 % 0.0", "NaN"),
    ] {
        assert_eq!(eval_scalar(dsl, &event), EvalValue::Null);
        // Rendered, not compared: `NaN != NaN` under `DuckValue`'s `PartialEq`,
        // so an `assert_eq!` against `DuckValue::Double(f64::NAN)` can never
        // hold. `{}` on f64 gives exactly `inf` / `-inf` / `NaN`.
        match sql_scalar_result(&conn, dsl, &event) {
            SqlOutcome::Value(SqlCell {
                logical_type: Type::Double,
                value: DuckValue::Double(value),
            }) => assert_eq!(format!("{value}"), sql, "for {dsl:?}"),
            other => panic!("expected a DOUBLE result for {dsl:?}, got {other:?}"),
        }
    }

    // The one AGREEING shape: INTEGER `%` 0 is NULL on both sides (a BIGINT
    // column holding NULL), so #105 must not "fix" it into an IEEE special.
    let modulo = "* | let x = 5 % 0";
    assert_eq!(eval_scalar(modulo, &event), EvalValue::Null);
    assert_eq!(
        sql_scalar_result(&conn, modulo, &event),
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
        ("+99:99", true, Ok(true)),
        ("+5:30", false, Err("has a timestamp that is not UTC")),
        ("+0530", false, Ok(true)),
        ("Z", true, Ok(true)),
        (" UTC", false, Ok(true)),
    ] {
        let dsl = format!(
            r#"* | let x = strptime("2026-01-15 10:20:30", "%Y-%m-%d %H:%M:%S") == "2026-01-15 10:20:30{text}""#
        );
        assert_eq!(eval_scalar(&dsl, &event), EvalValue::Bool(eval));
        let outcome = sql_scalar_result(&conn, &dsl, &event);
        match sql {
            Ok(value) => assert_eq!(
                outcome,
                SqlOutcome::Value(SqlCell {
                    logical_type: Type::Boolean,
                    value: DuckValue::Boolean(value),
                })
            ),
            Err(duckdb_error) => assert_sql_errored(&outcome, duckdb_error, &dsl),
        }
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
    //
    // Only `error(...)` is load-bearing. `1 / 0` was probed and DuckDB does NOT
    // raise on it — it answers `inf` (see
    // `current_division_by_zero_diverges_child_105`) — so an untaken `1 / 0`
    // branch returns 42 under fully EAGER evaluation too and proves nothing
    // about short-circuiting. Those three rows are deleted.
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

        // ONE implementation of the batch/eval contract: `assert_parity_case`
        // owns both the value match and the batch-errored/eval-nulls rule. The
        // bespoke copy that used to live here compared the eval result's
        // `serde_json::Value` form to `Value::Null`, which is strictly WEAKER —
        // `From<EvalValue> for Value` maps `Float(NaN)` and `Float(±inf)` onto
        // `Value::Null`, so an eval NaN/inf where DuckDB errored was silently
        // accepted.
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
    for (dsl, duckdb_error) in [
        // wrong-type replace arg
        (
            r#"* | let x = replace("abc", "a", 5)"#,
            "'replace(STRING_LITERAL, STRING_LITERAL, BIGINT)'",
        ),
        // varchar + int
        ("* | let x = service + 1", "'+(VARCHAR, UNKNOWN)'"),
        // round of varchar
        (
            r#"* | let x = round("abc")"#,
            r#"the function call "round(STRING_LITERAL)""#,
        ),
    ] {
        assert_sql_errored(&sql_scalar_result(&conn, dsl, &event), duckdb_error, dsl);
        // `EvalValue::Null`, never the `serde_json::Value` form compared to
        // `Value::Null`: that conversion maps `Float(NaN)`/`Float(inf)` onto
        // `Value::Null` too, so the weaker form would accept a non-null eval
        // here.
        assert_eq!(
            eval_scalar(dsl, &event),
            EvalValue::Null,
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

    // Streaming path (eval). Compared as an `EvalValue`, never through its
    // `serde_json::Value` form: `From<EvalValue> for Value` maps BOTH `Str` and
    // `Timestamp` onto `Value::String`, so the serde form could not tell a
    // rendered string from a rendered instant.
    assert_eq!(
        eval_scalar(dsl, &event),
        EvalValue::Str("2023".to_string()),
        "streaming strftime(strptime(...)) must not error or null"
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

        assert_eq!(
            eval_scalar(dsl, &event),
            EvalValue::Str(want.to_string()),
            "streaming strptime partial-format must match DuckDB for {dsl:?}"
        );
    }
}
