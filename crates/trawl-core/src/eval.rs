// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! In-memory expression evaluator for streaming pipeline stages.
//!
//! Evaluates `Expr` AST nodes against a `serde_json::Map` event
//! rather than emitting SQL. This is the runtime analog of
//! `emitter/expr.rs`.

use crate::ast::{BinaryOp, Expr, LiteralValue, Spanned, UnaryOp};
use crate::emitter::map_field_name;
use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use serde_json::{Map, Value};

/// Result of evaluating an expression against an event.
#[derive(Debug, Clone, PartialEq)]
pub enum EvalValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Array(Vec<EvalValue>),
    /// Timezone-naive timestamp, mirroring `DuckDB`'s `AS TIMESTAMP` cast
    /// which discards any offset and keeps wall-clock components.
    Timestamp(NaiveDateTime),
}

impl EvalValue {
    /// Truthiness check (SQL-style: `Null` and `false` are falsy).
    pub fn is_truthy(&self) -> bool {
        match self {
            Self::Null => false,
            Self::Bool(b) => *b,
            Self::Int(n) => *n != 0,
            Self::Float(n) => *n != 0.0,
            Self::Str(s) => !s.is_empty(),
            Self::Array(a) => !a.is_empty(),
            Self::Timestamp(_) => true,
        }
    }

    /// Try to extract a numeric value as f64.
    #[allow(clippy::cast_precision_loss)]
    fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Int(n) => Some(*n as f64),
            Self::Float(n) => Some(*n),
            _ => None,
        }
    }

    /// Try to coerce to a string representation.
    fn as_str_repr(&self) -> Option<String> {
        match self {
            Self::Str(s) => Some(s.clone()),
            Self::Int(n) => Some(n.to_string()),
            Self::Float(n) => Some(duckdb_double_to_string(*n)),
            Self::Bool(b) => Some(b.to_string()),
            Self::Timestamp(ts) => Some(timestamp_to_duckdb_text(ts)),
            Self::Null | Self::Array(_) => None,
        }
    }

    /// Try to parse self as a `NaiveDateTime` (`DuckDB` ISO set).
    ///
    /// Accepts: T or space separator, optional fractional seconds,
    /// optional offset (discarded to match `AS TIMESTAMP` semantics),
    /// date-only (→ midnight). Unparseable → `None`.
    pub(crate) fn as_timestamp(&self) -> Option<NaiveDateTime> {
        match self {
            Self::Timestamp(ts) => Some(*ts),
            Self::Str(s) => parse_timestamp(s),
            _ => None,
        }
    }
}

/// Render a `NaiveDateTime` in `DuckDB`'s canonical text format.
///
/// `DuckDB` represents timestamps as `"YYYY-MM-DD HH:MM:SS[.ffffff]"` with
/// trailing fractional-second zeros trimmed. This must byte-match `DuckDB`
/// output for the parity tests to pass.
pub fn timestamp_to_duckdb_text(ts: &NaiveDateTime) -> String {
    // Sub-second micros straight off the naive time — no need to build a
    // DateTime<Utc> just to read them.
    let micros = ts.nanosecond() / 1000;
    if micros == 0 {
        ts.format("%Y-%m-%d %H:%M:%S").to_string()
    } else {
        // Format full 6-digit microsecond precision, then trim trailing zeros.
        let full = format!("{}.{micros:06}", ts.format("%Y-%m-%d %H:%M:%S"));
        full.trim_end_matches('0').to_string()
    }
}

/// Render an `f64` to text exactly as `DuckDB`'s `CAST(DOUBLE AS VARCHAR)` does.
///
/// `DuckDB` (verified against v1.5.4 / engine 1.10504.0, the version trawl
/// ships) uses the shortest round-tripping decimal digit sequence — the same
/// digits Rust's `{:?}` Debug formatter produces — then applies fixed
/// presentation rules that diverge from Rust's `Display`/`Debug`:
///
/// 1. Integer-valued doubles always carry a `.0` suffix (`1.0`, not `1`).
/// 2. Negative zero loses its sign: `-0.0` → `"0.0"`.
/// 3. Scientific notation kicks in at the SAME magnitude thresholds as Rust's
///    `{:?}` (`>= 1e16` and `< 1e-4`), so we lean on Debug for the switch-over.
/// 4. The exponent ALWAYS carries a sign and is zero-padded to a minimum of two
///    digits (`e+05`, `e-05`, `e+16`, `e+100`), where Rust `{:?}` emits `e16` /
///    `e-5` (no sign, no pad).
/// 5. Specials render lowercase: `inf`, `-inf`, `nan`.
///
/// This is the single renderer behind `tostring()`, `concat()`/`||`, and any
/// other `CAST(… AS VARCHAR)` over a float in the batch path; mirroring it in
/// streaming eval closes the #22-class batch-vs-live divergence.
fn duckdb_double_to_string(x: f64) -> String {
    if x.is_nan() {
        return "nan".to_string();
    }
    if x.is_infinite() {
        return if x < 0.0 { "-inf" } else { "inf" }.to_string();
    }
    // Both +0.0 and -0.0 compare equal to 0.0; DuckDB strips the sign.
    if x == 0.0 {
        return "0.0".to_string();
    }

    // Rust Debug already gives shortest-roundtrip digits, the `.0` suffix on
    // integer-valued doubles, and the same sci-notation thresholds as DuckDB.
    let s = format!("{x:?}");
    let Some(e_pos) = s.find('e') else {
        return s;
    };

    // Rewrite the exponent to DuckDB form: always-signed, min two digits.
    let (mantissa, exp) = s.split_at(e_pos);
    let exp = &exp[1..]; // drop the 'e'
    let (sign, mag) = match exp.strip_prefix('-') {
        Some(rest) => ('-', rest),
        None => ('+', exp.strip_prefix('+').unwrap_or(exp)),
    };
    // `{:02}` zero-pads to 2 digits; 3-digit exponents pass through unpadded.
    let mag: u32 = mag.parse().unwrap_or(0);
    format!("{mantissa}e{sign}{mag:02}")
}

/// Parse a timestamp string using `DuckDB`'s practical ISO set.
///
/// Accepts T or space separator, optional fractional seconds (up to 6 digits),
/// optional UTC offset (discarded — mirrors `CAST AS TIMESTAMP` semantics),
/// and date-only (→ midnight).
pub fn parse_timestamp(s: &str) -> Option<NaiveDateTime> {
    // Try datetime formats (T and space separators, with/without fractional secs).
    // Strip optional trailing offset (+HH:MM, -HH:MM, Z) before matching
    // naive formats so offsets are silently discarded.
    const DT_FMTS: &[&str] = &[
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
    ];

    let s = strip_offset(s).unwrap_or(s);

    for fmt in DT_FMTS {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some(dt);
        }
    }

    // Date-only → midnight.
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Some(d.and_hms_opt(0, 0, 0).expect("midnight is valid"));
    }

    None
}

/// Strip a trailing UTC offset from a timestamp string, returning a borrowed
/// slice of the input when an offset was found, or `None` if the string doesn't
/// appear to carry one (the no-op case allocates nothing).
fn strip_offset(s: &str) -> Option<&str> {
    let s = s.trim();
    // Check for trailing 'Z'
    if let Some(base) = s.strip_suffix('Z') {
        return Some(base);
    }
    // Check for trailing +HH:MM or -HH:MM (exactly 6 bytes at end).
    // Use checked indexing: a non-char-boundary slice (e.g. when the byte at
    // `len - 6` is a UTF-8 continuation byte of a multi-byte sequence) makes
    // `s.get` return None rather than panicking. A valid offset is pure ASCII,
    // so when `get` yields Some, `len - 6` is guaranteed a char boundary and
    // the head slice below cannot panic either.
    if let Some(tail) = s.len().checked_sub(6).and_then(|i| s.get(i..)) {
        let bytes = tail.as_bytes();
        let sign = bytes[0];
        if (sign == b'+' || sign == b'-') && bytes[3] == b':' {
            return s.get(..s.len() - 6);
        }
    }
    None
}

impl From<EvalValue> for Value {
    fn from(v: EvalValue) -> Self {
        match v {
            EvalValue::Null => Value::Null,
            EvalValue::Bool(b) => Value::Bool(b),
            EvalValue::Int(n) => Value::Number(n.into()),
            EvalValue::Float(n) => {
                serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number)
            }
            EvalValue::Str(s) => Value::String(s),
            EvalValue::Array(a) => Value::Array(a.into_iter().map(Value::from).collect()),
            // Serialize timestamps in DuckDB canonical text format so the event
            // map round-trips correctly through serde_json.
            EvalValue::Timestamp(ts) => Value::String(timestamp_to_duckdb_text(&ts)),
        }
    }
}

impl From<&Value> for EvalValue {
    fn from(v: &Value) -> Self {
        match v {
            Value::Bool(b) => Self::Bool(*b),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Self::Int(i)
                } else if let Some(f) = n.as_f64() {
                    Self::Float(f)
                } else {
                    Self::Null
                }
            }
            Value::String(s) => Self::Str(s.clone()),
            Value::Array(a) => Self::Array(a.iter().map(Self::from).collect()),
            Value::Null | Value::Object(_) => Self::Null,
        }
    }
}

/// Evaluate an expression AST node against an event map.
pub fn eval_expr(expr: &Spanned<Expr>, event: &Map<String, Value>) -> EvalValue {
    match &expr.node {
        Expr::Literal(lit) => eval_literal(lit),
        Expr::FieldRef(name) => {
            let mapped = map_field_name(name);
            event.get(mapped).map_or(EvalValue::Null, EvalValue::from)
        }
        Expr::Binary { lhs, op, rhs } => {
            eval_binary(&eval_expr(lhs, event), *op, &eval_expr(rhs, event))
        }
        Expr::Unary { op, operand } => eval_unary(*op, eval_expr(operand, event)),
        Expr::FunctionCall { name, args } => {
            let evaluated: Vec<EvalValue> = args.iter().map(|a| eval_expr(a, event)).collect();
            eval_scalar_fn(name, &evaluated).unwrap_or(EvalValue::Null)
        }
        Expr::InList { expr: target, list } => {
            let target_val = eval_expr(target, event);
            if matches!(target_val, EvalValue::Null) {
                return EvalValue::Null;
            }
            for item in list {
                let item_val = eval_expr(item, event);
                if eval_eq(&target_val, &item_val) == EvalValue::Bool(true) {
                    return EvalValue::Bool(true);
                }
            }
            EvalValue::Bool(false)
        }
    }
}

fn eval_literal(lit: &LiteralValue) -> EvalValue {
    match lit {
        LiteralValue::Null => EvalValue::Null,
        LiteralValue::Bool(b) => EvalValue::Bool(*b),
        LiteralValue::Int(n) => EvalValue::Int(*n),
        LiteralValue::Float(n) => EvalValue::Float(*n),
        LiteralValue::String(s) => EvalValue::Str(s.clone()),
    }
}

// ── binary operations ──────────────────────────────────────────────

fn eval_binary(lhs: &EvalValue, op: BinaryOp, rhs: &EvalValue) -> EvalValue {
    // short-circuit logical ops (they handle Null specially)
    match op {
        BinaryOp::And => return eval_and(lhs, rhs),
        BinaryOp::Or => return eval_or(lhs, rhs),
        _ => {}
    }

    // null propagation for everything else
    if matches!(lhs, EvalValue::Null) || matches!(rhs, EvalValue::Null) {
        return EvalValue::Null;
    }

    match op {
        BinaryOp::Add => eval_arithmetic(lhs, rhs, |a, b| a + b, |a, b| a + b),
        BinaryOp::Sub => eval_arithmetic(lhs, rhs, |a, b| a - b, |a, b| a - b),
        BinaryOp::Mul => eval_arithmetic(lhs, rhs, |a, b| a * b, |a, b| a * b),
        BinaryOp::Div => eval_div(lhs, rhs),
        BinaryOp::Mod => eval_mod(lhs, rhs),
        BinaryOp::Eq => eval_eq(lhs, rhs),
        BinaryOp::Ne => match eval_eq(lhs, rhs) {
            EvalValue::Bool(b) => EvalValue::Bool(!b),
            other => other,
        },
        BinaryOp::Gt => eval_cmp(lhs, rhs, |o| o == std::cmp::Ordering::Greater),
        BinaryOp::Gte => eval_cmp(lhs, rhs, |o| {
            o == std::cmp::Ordering::Greater || o == std::cmp::Ordering::Equal
        }),
        BinaryOp::Lt => eval_cmp(lhs, rhs, |o| o == std::cmp::Ordering::Less),
        BinaryOp::Lte => eval_cmp(lhs, rhs, |o| {
            o == std::cmp::Ordering::Less || o == std::cmp::Ordering::Equal
        }),
        BinaryOp::Matches => eval_matches(lhs, rhs),
        BinaryOp::Like => eval_like(lhs, rhs, false),
        BinaryOp::ILike => eval_like(lhs, rhs, true),
        BinaryOp::And | BinaryOp::Or => unreachable!(),
    }
}

fn eval_and(lhs: &EvalValue, rhs: &EvalValue) -> EvalValue {
    // SQL three-valued logic: false AND anything = false
    if matches!(lhs, EvalValue::Bool(false)) {
        return EvalValue::Bool(false);
    }
    if matches!(rhs, EvalValue::Bool(false)) {
        return EvalValue::Bool(false);
    }
    if matches!(lhs, EvalValue::Null) || matches!(rhs, EvalValue::Null) {
        return EvalValue::Null;
    }
    EvalValue::Bool(lhs.is_truthy() && rhs.is_truthy())
}

fn eval_or(lhs: &EvalValue, rhs: &EvalValue) -> EvalValue {
    // SQL three-valued logic: true OR anything = true
    if lhs.is_truthy() {
        return EvalValue::Bool(true);
    }
    if rhs.is_truthy() {
        return EvalValue::Bool(true);
    }
    if matches!(lhs, EvalValue::Null) || matches!(rhs, EvalValue::Null) {
        return EvalValue::Null;
    }
    EvalValue::Bool(false)
}

fn eval_arithmetic(
    lhs: &EvalValue,
    rhs: &EvalValue,
    int_op: impl FnOnce(i64, i64) -> i64,
    float_op: impl FnOnce(f64, f64) -> f64,
) -> EvalValue {
    match (lhs, rhs) {
        (EvalValue::Int(a), EvalValue::Int(b)) => EvalValue::Int(int_op(*a, *b)),
        _ => match (lhs.as_f64(), rhs.as_f64()) {
            (Some(a), Some(b)) => EvalValue::Float(float_op(a, b)),
            _ => EvalValue::Null,
        },
    }
}

fn eval_div(lhs: &EvalValue, rhs: &EvalValue) -> EvalValue {
    match (lhs, rhs) {
        (EvalValue::Int(a), EvalValue::Int(b)) => {
            if *b == 0 {
                EvalValue::Null
            } else {
                EvalValue::Int(a / b)
            }
        }
        _ => match (lhs.as_f64(), rhs.as_f64()) {
            (Some(a), Some(b)) => {
                if b == 0.0 {
                    EvalValue::Null
                } else {
                    EvalValue::Float(a / b)
                }
            }
            _ => EvalValue::Null,
        },
    }
}

fn eval_mod(lhs: &EvalValue, rhs: &EvalValue) -> EvalValue {
    match (lhs, rhs) {
        (EvalValue::Int(a), EvalValue::Int(b)) => {
            if *b == 0 {
                EvalValue::Null
            } else {
                EvalValue::Int(a % b)
            }
        }
        _ => match (lhs.as_f64(), rhs.as_f64()) {
            (Some(a), Some(b)) => {
                if b == 0.0 {
                    EvalValue::Null
                } else {
                    EvalValue::Float(a % b)
                }
            }
            _ => EvalValue::Null,
        },
    }
}

fn eval_eq(lhs: &EvalValue, rhs: &EvalValue) -> EvalValue {
    match (lhs, rhs) {
        (EvalValue::Int(a), EvalValue::Int(b)) => EvalValue::Bool(a == b),
        (EvalValue::Bool(a), EvalValue::Bool(b)) => EvalValue::Bool(a == b),
        (EvalValue::Str(a), EvalValue::Str(b)) => EvalValue::Bool(a == b),
        (EvalValue::Timestamp(a), EvalValue::Timestamp(b)) => EvalValue::Bool(a == b),
        // Str vs Timestamp: coerce Str to Timestamp; fall back to Str-vs-Str on
        // parse failure so non-timestamp strings don't regress.
        (EvalValue::Timestamp(_), EvalValue::Str(_))
        | (EvalValue::Str(_), EvalValue::Timestamp(_)) => {
            match (lhs.as_timestamp(), rhs.as_timestamp()) {
                (Some(a), Some(b)) => EvalValue::Bool(a == b),
                // Str that doesn't parse as timestamp: fall through to str repr
                _ => match (lhs.as_str_repr(), rhs.as_str_repr()) {
                    (Some(a), Some(b)) => EvalValue::Bool(a == b),
                    _ => EvalValue::Null,
                },
            }
        }
        // cross-type numeric comparison
        _ => match (lhs.as_f64(), rhs.as_f64()) {
            (Some(a), Some(b)) => EvalValue::Bool(a == b),
            // string coercion: compare as strings if one side is a string
            _ => match (lhs.as_str_repr(), rhs.as_str_repr()) {
                (Some(a), Some(b)) => EvalValue::Bool(a == b),
                _ => EvalValue::Null,
            },
        },
    }
}

fn eval_cmp(
    lhs: &EvalValue,
    rhs: &EvalValue,
    pred: impl FnOnce(std::cmp::Ordering) -> bool,
) -> EvalValue {
    let ordering = match (lhs, rhs) {
        (EvalValue::Int(a), EvalValue::Int(b)) => Some(a.cmp(b)),
        (EvalValue::Str(a), EvalValue::Str(b)) => Some(a.cmp(b)),
        (EvalValue::Timestamp(a), EvalValue::Timestamp(b)) => Some(a.cmp(b)),
        // Str vs Timestamp: coerce Str to Timestamp for ordering; fall back to
        // Str-vs-Str so non-timestamp strings don't regress.
        (EvalValue::Timestamp(_), EvalValue::Str(_))
        | (EvalValue::Str(_), EvalValue::Timestamp(_)) => {
            match (lhs.as_timestamp(), rhs.as_timestamp()) {
                (Some(a), Some(b)) => Some(a.cmp(&b)),
                _ => match (lhs.as_str_repr(), rhs.as_str_repr()) {
                    (Some(a), Some(b)) => Some(a.cmp(&b)),
                    _ => None,
                },
            }
        }
        _ => match (lhs.as_f64(), rhs.as_f64()) {
            (Some(a), Some(b)) => a.partial_cmp(&b), // both are values, not refs
            _ => None,
        },
    };
    ordering.map_or(EvalValue::Null, |o| EvalValue::Bool(pred(o)))
}

fn eval_matches(lhs: &EvalValue, rhs: &EvalValue) -> EvalValue {
    match (lhs, rhs) {
        (EvalValue::Str(text), EvalValue::Str(pattern)) => {
            // compile regex on the fly — for hot-path use, the caller
            // should pre-compile via `CompiledStage`
            regex::Regex::new(pattern).map_or(EvalValue::Bool(false), |re| {
                EvalValue::Bool(re.is_match(text))
            })
        }
        _ => EvalValue::Null,
    }
}

/// Evaluate SQL LIKE/ILIKE pattern matching.
///
/// `%` matches any sequence, `_` matches a single character.
fn eval_like(lhs: &EvalValue, rhs: &EvalValue, case_insensitive: bool) -> EvalValue {
    match (lhs, rhs) {
        (EvalValue::Str(text), EvalValue::Str(pattern)) => {
            let mut regex_str = String::from("^");
            for ch in pattern.chars() {
                match ch {
                    '%' => regex_str.push_str(".*"),
                    '_' => regex_str.push('.'),
                    // escape regex metacharacters
                    '.' | '^' | '$' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|'
                    | '\\' => {
                        regex_str.push('\\');
                        regex_str.push(ch);
                    }
                    _ => regex_str.push(ch),
                }
            }
            regex_str.push('$');
            if case_insensitive {
                regex_str.insert_str(0, "(?i)");
            }
            regex::Regex::new(&regex_str).map_or(EvalValue::Bool(false), |re| {
                EvalValue::Bool(re.is_match(text))
            })
        }
        _ => EvalValue::Null,
    }
}

// ── unary operations ───────────────────────────────────────────────

fn eval_unary(op: UnaryOp, operand: EvalValue) -> EvalValue {
    match op {
        UnaryOp::Not => match operand {
            EvalValue::Null => EvalValue::Null,
            EvalValue::Bool(b) => EvalValue::Bool(!b),
            other => EvalValue::Bool(!other.is_truthy()),
        },
        UnaryOp::Neg => match operand {
            // checked_neg returns None on i64::MIN; null out rather than
            // panic (debug) / wrap (release), consistent with div/mod guards.
            EvalValue::Int(n) => n.checked_neg().map_or(EvalValue::Null, EvalValue::Int),
            EvalValue::Float(n) => EvalValue::Float(-n),
            _ => EvalValue::Null,
        },
    }
}

// ── scalar functions ───────────────────────────────────────────────

/// Evaluate a scalar function call.
///
/// Returns `Some(value)` if the function name is known and handled,
/// `None` if it is unknown (distinct from `Some(Null)` which means the
/// function evaluated to SQL NULL). The call site maps `None →
/// EvalValue::Null` so existing behaviour is preserved; the `Option`
/// wrapper exists so the coverage test can detect un-implemented scalars.
#[allow(clippy::too_many_lines)]
fn eval_scalar_fn(name: &str, args: &[EvalValue]) -> Option<EvalValue> {
    let v = match name {
        // string
        "lower" => unary_str(args, str::to_lowercase),
        "upper" => unary_str(args, str::to_uppercase),
        #[allow(clippy::cast_possible_wrap)]
        "length" | "len" => args.first().map_or(EvalValue::Null, |v| match v {
            // DuckDB's LENGTH() (emitted by emitter::functions) returns the
            // character count, not the byte count, so use chars().count() to
            // keep the streaming evaluator in parity with the batch path.
            EvalValue::Str(s) => EvalValue::Int(s.chars().count() as i64),
            _ => EvalValue::Null,
        }),
        "trim" => unary_str(args, |s| s.trim().to_string()),
        "ltrim" => unary_str(args, |s| s.trim_start().to_string()),
        "rtrim" => unary_str(args, |s| s.trim_end().to_string()),
        "replace" => {
            if args.len() != 3 {
                return Some(EvalValue::Null);
            }
            match (&args[0], &args[1], &args[2]) {
                (EvalValue::Str(s), EvalValue::Str(from), EvalValue::Str(to)) => {
                    EvalValue::Str(s.replace(from.as_str(), to.as_str()))
                }
                _ => EvalValue::Null,
            }
        }
        "substr" => eval_substr(args),
        "contains" => {
            if args.len() != 2 {
                return Some(EvalValue::Null);
            }
            match (&args[0], &args[1]) {
                (EvalValue::Str(s), EvalValue::Str(sub)) => {
                    EvalValue::Bool(s.contains(sub.as_str()))
                }
                _ => EvalValue::Null,
            }
        }
        "startswith" => {
            if args.len() != 2 {
                return Some(EvalValue::Null);
            }
            match (&args[0], &args[1]) {
                (EvalValue::Str(s), EvalValue::Str(pre)) => {
                    EvalValue::Bool(s.starts_with(pre.as_str()))
                }
                _ => EvalValue::Null,
            }
        }
        "endswith" => {
            if args.len() != 2 {
                return Some(EvalValue::Null);
            }
            match (&args[0], &args[1]) {
                (EvalValue::Str(s), EvalValue::Str(suf)) => {
                    EvalValue::Bool(s.ends_with(suf.as_str()))
                }
                _ => EvalValue::Null,
            }
        }
        "split" => {
            if args.len() != 3 {
                return Some(EvalValue::Null);
            }
            match (&args[0], &args[1], &args[2]) {
                (EvalValue::Str(s), EvalValue::Str(delim), EvalValue::Int(idx)) => {
                    let parts: Vec<&str> = s.split(delim.as_str()).collect();
                    usize::try_from(*idx)
                        .ok()
                        .and_then(|i| parts.get(i))
                        .map_or(EvalValue::Null, |p| EvalValue::Str((*p).to_string()))
                }
                _ => EvalValue::Null,
            }
        }
        "concat" => {
            // DuckDB CONCAT() *ignores* NULL args (unlike `||`, which
            // propagates NULL) and casts each non-null arg to VARCHAR before
            // joining — so CONCAT(NULL) == '' and CONCAT('x','-',NULL) == 'x-'.
            // `as_str_repr` mirrors that CAST-to-VARCHAR (incl. Timestamp →
            // DuckDB text); Null/Array yield None and are skipped. (Array can
            // never legally reach CONCAT — DuckDB rejects it at bind time and
            // the emitter errors too — so skipping it can't produce a value
            // DuckDB wouldn't.)
            let result: String = args.iter().filter_map(EvalValue::as_str_repr).collect();
            EvalValue::Str(result)
        }

        // numeric
        "abs" => args.first().map_or(EvalValue::Null, |v| match v {
            // checked_abs returns None on i64::MIN; null out rather than
            // panic (debug) / wrap (release), consistent with div/mod guards.
            EvalValue::Int(n) => n.checked_abs().map_or(EvalValue::Null, EvalValue::Int),
            EvalValue::Float(n) => EvalValue::Float(n.abs()),
            _ => EvalValue::Null,
        }),
        "ceil" | "ceiling" => eval_ceil(args),
        "floor" => eval_floor(args),
        "round" => eval_round(args),

        // conditional / type
        "if" => {
            if args.len() != 3 {
                return Some(EvalValue::Null);
            }
            if args[0].is_truthy() {
                args[1].clone()
            } else {
                args[2].clone()
            }
        }
        "coalesce" => {
            for arg in args {
                if !matches!(arg, EvalValue::Null) {
                    return Some(arg.clone());
                }
            }
            EvalValue::Null
        }
        "isnull" => args.first().map_or(EvalValue::Null, |v| {
            EvalValue::Bool(matches!(v, EvalValue::Null))
        }),
        "isnotnull" => args.first().map_or(EvalValue::Null, |v| {
            EvalValue::Bool(!matches!(v, EvalValue::Null))
        }),
        "typeof" => args.first().map_or(EvalValue::Null, |v| {
            EvalValue::Str(
                match v {
                    EvalValue::Null => "NULL",
                    EvalValue::Bool(_) => "BOOLEAN",
                    EvalValue::Int(_) => "INTEGER",
                    EvalValue::Float(_) => "DOUBLE",
                    EvalValue::Str(_) => "VARCHAR",
                    EvalValue::Array(_) => "ARRAY",
                    EvalValue::Timestamp(_) => "TIMESTAMP",
                }
                .to_string(),
            )
        }),
        // now() returns a Timestamp (timezone-naive wall-clock UTC)
        "now" => EvalValue::Timestamp(chrono::Utc::now().naive_utc()),
        // conditional
        "case" => {
            let pairs = args.len() / 2;
            for i in 0..pairs {
                if args[i * 2].is_truthy() {
                    return Some(args[i * 2 + 1].clone());
                }
            }
            // odd arg count → last arg is default
            if args.len() % 2 == 1 {
                args[args.len() - 1].clone()
            } else {
                EvalValue::Null
            }
        }
        // json
        "json" | "json_extract_string" => eval_json_extract_string(args),
        "json_extract" => eval_json_extract(args),
        "json_valid" => args.first().map_or(EvalValue::Null, |v| match v {
            EvalValue::Str(s) => {
                EvalValue::Bool(serde_json::from_str::<serde_json::Value>(s).is_ok())
            }
            _ => EvalValue::Null,
        }),
        "json_keys" => args.first().map_or(EvalValue::Null, |v| match v {
            EvalValue::Str(s) => serde_json::from_str::<serde_json::Value>(s)
                .ok()
                .and_then(|val| {
                    val.as_object().map(|obj| {
                        EvalValue::Array(obj.keys().map(|k| EvalValue::Str(k.clone())).collect())
                    })
                })
                .unwrap_or(EvalValue::Null),
            _ => EvalValue::Null,
        }),
        #[allow(clippy::cast_possible_wrap)]
        "json_array_length" => args.first().map_or(EvalValue::Null, |v| match v {
            EvalValue::Str(s) => serde_json::from_str::<serde_json::Value>(s)
                .ok()
                .and_then(|val| val.as_array().map(|arr| EvalValue::Int(arr.len() as i64)))
                .unwrap_or(EvalValue::Null),
            _ => EvalValue::Null,
        }),

        // date/time scalar functions
        "tonumber" => eval_tonumber(args),
        "tostring" => eval_tostring(args),
        "date_part" => eval_date_part(args),
        "date_trunc" => eval_date_trunc(args),
        "date_diff" => eval_date_diff(args),
        "strftime" => eval_strftime(args),
        "strptime" => eval_strptime(args),

        _ => return None,
    };
    Some(v)
}

/// Convert a `JSONPath` like `$.foo.bar[0]` to a JSON Pointer like `/foo/bar/0`.
fn jsonpath_to_pointer(path: &str) -> String {
    let stripped = path.strip_prefix('$').unwrap_or(path);
    let mut result = String::new();
    for part in stripped.split('.') {
        if part.is_empty() {
            continue;
        }
        // handle bracket notation: "items[0]" -> "items" + "0"
        if let Some(bracket_pos) = part.find('[') {
            let field = &part[..bracket_pos];
            if !field.is_empty() {
                result.push('/');
                result.push_str(field);
            }
            if let Some(idx_str) = part[bracket_pos + 1..].strip_suffix(']') {
                result.push('/');
                result.push_str(idx_str);
            }
        } else {
            result.push('/');
            result.push_str(part);
        }
    }
    result
}

fn eval_json_extract_string(args: &[EvalValue]) -> EvalValue {
    if args.len() != 2 {
        return EvalValue::Null;
    }
    match (&args[0], &args[1]) {
        (EvalValue::Str(json_str), EvalValue::Str(path)) => {
            let pointer = jsonpath_to_pointer(path);
            serde_json::from_str::<serde_json::Value>(json_str)
                .ok()
                .and_then(|val| {
                    val.pointer(&pointer).map(|v| match v {
                        serde_json::Value::String(s) => EvalValue::Str(s.clone()),
                        other => EvalValue::Str(other.to_string()),
                    })
                })
                .unwrap_or(EvalValue::Null)
        }
        _ => EvalValue::Null,
    }
}

fn eval_json_extract(args: &[EvalValue]) -> EvalValue {
    if args.len() != 2 {
        return EvalValue::Null;
    }
    match (&args[0], &args[1]) {
        (EvalValue::Str(json_str), EvalValue::Str(path)) => {
            let pointer = jsonpath_to_pointer(path);
            serde_json::from_str::<serde_json::Value>(json_str)
                .ok()
                .and_then(|val| val.pointer(&pointer).map(json_val_to_eval_val))
                .unwrap_or(EvalValue::Null)
        }
        _ => EvalValue::Null,
    }
}

fn json_val_to_eval_val(v: &serde_json::Value) -> EvalValue {
    match v {
        serde_json::Value::Null => EvalValue::Null,
        serde_json::Value::Bool(b) => EvalValue::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                EvalValue::Int(i)
            } else {
                EvalValue::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(s) => EvalValue::Str(s.clone()),
        serde_json::Value::Array(arr) => {
            EvalValue::Array(arr.iter().map(json_val_to_eval_val).collect())
        }
        serde_json::Value::Object(_) => EvalValue::Str(v.to_string()),
    }
}

fn unary_str(args: &[EvalValue], f: impl FnOnce(&str) -> String) -> EvalValue {
    args.first().map_or(EvalValue::Null, |v| match v {
        EvalValue::Str(s) => EvalValue::Str(f(s)),
        _ => EvalValue::Null,
    })
}

#[allow(clippy::cast_possible_truncation)]
fn eval_ceil(args: &[EvalValue]) -> EvalValue {
    args.first().map_or(EvalValue::Null, |v| match v {
        EvalValue::Int(n) => EvalValue::Int(*n),
        EvalValue::Float(n) => EvalValue::Int(n.ceil() as i64),
        _ => EvalValue::Null,
    })
}

#[allow(clippy::cast_possible_truncation)]
fn eval_floor(args: &[EvalValue]) -> EvalValue {
    args.first().map_or(EvalValue::Null, |v| match v {
        EvalValue::Int(n) => EvalValue::Int(*n),
        EvalValue::Float(n) => EvalValue::Int(n.floor() as i64),
        _ => EvalValue::Null,
    })
}

/// `substr(s, start [, len])` — mirrors `DuckDB`'s `SUBSTRING` semantics.
///
/// 1-based and CHARACTER-based (multibyte UTF-8 counts as one char). NOT
/// `PostgreSQL` semantics. Negative `start` counts from the end (`-1` = last
/// char); `start == 0` stays a position before the first char (NOT clamped to
/// 1). Negative `len` is a real LEFTWARD window exclusive of `start`. The
/// resulting inclusive 1-based window `[lo, hi]` is clamped to `[1, char_len]`;
/// `lo > hi` yields the empty string. NULL propagates from any arg.
///
/// All bound arithmetic is done in `i64` (bounds can legitimately go negative
/// or exceed the string length before clamping) — do NOT cast to `usize` until
/// after the `lo > hi` guard, or out-of-range inputs underflow-panic.
fn eval_substr(args: &[EvalValue]) -> EvalValue {
    if args.len() < 2 || args.len() > 3 {
        return EvalValue::Null;
    }
    let EvalValue::Str(s) = &args[0] else {
        return EvalValue::Null;
    };
    let EvalValue::Int(start) = args[1] else {
        return EvalValue::Null;
    };

    let chars: Vec<char> = s.chars().collect();
    let n = i64::try_from(chars.len()).unwrap_or(i64::MAX);

    // Negative start counts from the end; 0 stays a position before the first
    // char. Saturating add: an adversarial start near i64::MIN must not overflow
    // before clamping (panics in debug, wraps in release).
    let start = if start < 0 {
        n.saturating_add(start).saturating_add(1)
    } else {
        start
    };

    // Inclusive 1-based window [lo, hi]. Saturating arithmetic throughout —
    // out-of-range start/len bounds clamp to the string range below.
    let (lo, hi) = match args.get(2) {
        None => (start, n),
        Some(EvalValue::Int(len)) => match len.cmp(&0) {
            std::cmp::Ordering::Equal => return EvalValue::Str(String::new()),
            // Leftward, exclusive of start itself.
            std::cmp::Ordering::Less => (start.saturating_add(*len), start.saturating_sub(1)),
            std::cmp::Ordering::Greater => (start, start.saturating_add(*len).saturating_sub(1)),
        },
        Some(_) => return EvalValue::Null,
    };

    let lo = lo.max(1);
    let hi = hi.min(n);
    if lo > hi {
        return EvalValue::Str(String::new());
    }
    // 0-based slice [lo-1 .. hi]; both are now in 1..=n so the casts are safe.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    EvalValue::Str(chars[(lo - 1) as usize..hi as usize].iter().collect())
}

#[allow(clippy::cast_possible_truncation)]
fn eval_round(args: &[EvalValue]) -> EvalValue {
    if args.is_empty() || args.len() > 2 {
        return EvalValue::Null;
    }
    let val = match &args[0] {
        EvalValue::Int(n) => return EvalValue::Int(*n),
        EvalValue::Float(n) => *n,
        _ => return EvalValue::Null,
    };
    let precision = if args.len() == 2 {
        match &args[1] {
            EvalValue::Int(n) => *n,
            _ => return EvalValue::Null,
        }
    } else {
        0
    };

    if precision == 0 {
        EvalValue::Int(val.round() as i64)
    } else {
        let factor = 10_f64.powi(precision as i32);
        EvalValue::Float((val * factor).round() / factor)
    }
}

// ── date/time scalar function implementations ──────────────────────

/// `tonumber(x)` — mirrors `TRY_CAST(x AS DOUBLE)`.
#[allow(clippy::cast_precision_loss)]
fn eval_tonumber(args: &[EvalValue]) -> EvalValue {
    match args.first() {
        Some(EvalValue::Int(n)) => EvalValue::Float(*n as f64),
        Some(EvalValue::Float(n)) => EvalValue::Float(*n),
        Some(EvalValue::Str(s)) => strip_digit_separators(s.trim())
            .parse::<f64>()
            .map_or(EvalValue::Null, EvalValue::Float),
        None | Some(_) => EvalValue::Null,
    }
}

/// Remove ASCII digit separators (`_`) the way `DuckDB`'s `TRY_CAST(... AS
/// DOUBLE)` does: a `_` is honored ONLY when it has an ASCII digit immediately
/// before AND immediately after it. Any other `_` is left in place so the
/// subsequent `f64::parse` rejects it (returning `Null`), exactly matching
/// `DuckDB`: `1_000` -> `1000`, `1_0.0_5` -> `10.05`, but `_1000` / `1000_` /
/// `1__000` / `1_e3` / `1,000` all stay unparseable.
fn strip_digit_separators(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    for (i, ch) in s.char_indices() {
        // `_` is single-byte ASCII, so `i` is a valid index into `bytes` and
        // the neighbour checks stay on byte boundaries.
        if ch == '_'
            && i > 0
            && bytes[i - 1].is_ascii_digit()
            && bytes.get(i + 1).is_some_and(u8::is_ascii_digit)
        {
            continue; // drop the honored separator
        }
        out.push(ch);
    }
    out
}

/// `tostring(x)` — mirrors `CAST(x AS VARCHAR)`.
fn eval_tostring(args: &[EvalValue]) -> EvalValue {
    match args.first() {
        None | Some(EvalValue::Null | EvalValue::Array(_)) => EvalValue::Null,
        Some(v) => v.as_str_repr().map_or(EvalValue::Null, EvalValue::Str),
    }
}

/// `date_part(unit, ts)` — mirrors `DATE_PART(unit, ts)`.
fn eval_date_part(args: &[EvalValue]) -> EvalValue {
    if args.len() != 2 {
        return EvalValue::Null;
    }
    let EvalValue::Str(unit) = &args[0] else {
        return EvalValue::Null;
    };
    let Some(ts) = args[1].as_timestamp() else {
        return EvalValue::Null;
    };
    match unit.to_lowercase().as_str() {
        "year" => EvalValue::Int(i64::from(ts.year())),
        "quarter" => EvalValue::Int(i64::from((ts.month() - 1) / 3 + 1)),
        "month" => EvalValue::Int(i64::from(ts.month())),
        "week" => EvalValue::Int(i64::from(ts.iso_week().week())),
        "day" => EvalValue::Int(i64::from(ts.day())),
        "hour" => EvalValue::Int(i64::from(ts.hour())),
        "minute" => EvalValue::Int(i64::from(ts.minute())),
        "second" => EvalValue::Int(i64::from(ts.second())),
        // DuckDB: Sunday=0 … Saturday=6, exactly num_days_from_sunday().
        "dow" => EvalValue::Int(i64::from(ts.weekday().num_days_from_sunday())),
        "doy" => EvalValue::Int(i64::from(ts.ordinal())),
        "epoch" => {
            // seconds since Unix epoch as float (matches DuckDB EPOCH semantics)
            let utc = ts.and_utc();
            #[allow(clippy::cast_precision_loss)]
            let epoch_secs =
                utc.timestamp() as f64 + f64::from(utc.timestamp_subsec_micros()) / 1_000_000.0;
            EvalValue::Float(epoch_secs)
        }
        _ => EvalValue::Null,
    }
}

/// `date_trunc(unit, ts)` — mirrors `DATE_TRUNC(unit, ts)`.
fn eval_date_trunc(args: &[EvalValue]) -> EvalValue {
    if args.len() != 2 {
        return EvalValue::Null;
    }
    let EvalValue::Str(unit) = &args[0] else {
        return EvalValue::Null;
    };
    let Some(ts) = args[1].as_timestamp() else {
        return EvalValue::Null;
    };
    let truncated = match unit.to_lowercase().as_str() {
        "year" => NaiveDate::from_ymd_opt(ts.year(), 1, 1).and_then(|d| d.and_hms_opt(0, 0, 0)),
        "quarter" => {
            let q_start_month = ((ts.month() - 1) / 3) * 3 + 1;
            NaiveDate::from_ymd_opt(ts.year(), q_start_month, 1)
                .and_then(|d| d.and_hms_opt(0, 0, 0))
        }
        "month" => {
            NaiveDate::from_ymd_opt(ts.year(), ts.month(), 1).and_then(|d| d.and_hms_opt(0, 0, 0))
        }
        "week" => {
            // DuckDB truncates week to Monday (ISO 8601); num_days_from_monday()
            // is Mon=0 … Sun=6, exactly the offset back to Monday.
            let days_since_monday = u64::from(ts.weekday().num_days_from_monday());
            ts.date()
                .checked_sub_days(chrono::Days::new(days_since_monday))
                .and_then(|d| d.and_hms_opt(0, 0, 0))
        }
        "day" => ts.date().and_hms_opt(0, 0, 0),
        // hour/minute/second share the truncators used by date_diff.
        "hour" => Some(trunc_to_hour(ts)),
        "minute" => Some(trunc_to_minute(ts)),
        "second" => Some(trunc_to_second(ts)),
        _ => None,
    };
    truncated.map_or(EvalValue::Null, EvalValue::Timestamp)
}

/// `date_diff(unit, start, end)` — mirrors `DATE_DIFF(unit, start, end)`.
///
/// Replicates `DuckDB`'s boundary-crossing count (not floored elapsed time).
/// Example: `date_diff('hour', '…23:59:59', '…00:00:01')` = 1 (crosses the
/// hour boundary once), not 0 (which floored division would produce).
#[allow(clippy::cast_possible_truncation)]
fn eval_date_diff(args: &[EvalValue]) -> EvalValue {
    if args.len() != 3 {
        return EvalValue::Null;
    }
    let EvalValue::Str(unit) = &args[0] else {
        return EvalValue::Null;
    };
    let Some(start) = args[1].as_timestamp() else {
        return EvalValue::Null;
    };
    let Some(end) = args[2].as_timestamp() else {
        return EvalValue::Null;
    };
    let count: i64 = match unit.to_lowercase().as_str() {
        "year" => i64::from(end.year() - start.year()),
        "quarter" => {
            let start_q = i64::from(start.year()) * 4 + i64::from((start.month() - 1) / 3);
            let end_q = i64::from(end.year()) * 4 + i64::from((end.month() - 1) / 3);
            end_q - start_q
        }
        "month" => {
            let start_m = i64::from(start.year()) * 12 + i64::from(start.month() - 1);
            let end_m = i64::from(end.year()) * 12 + i64::from(end.month() - 1);
            end_m - start_m
        }
        "week" => {
            // DuckDB counts weeks as days/7 (integer division truncating towards
            // zero, i.e. DATE_DIFF('day', start, end) / 7). It does NOT
            // use ISO-week-boundary crossing; this matches the verified
            // DuckDB 1.x behaviour confirmed via parity tests.
            let start_trunc = start.date().and_hms_opt(0, 0, 0).unwrap_or(start);
            let end_trunc = end.date().and_hms_opt(0, 0, 0).unwrap_or(end);
            let days = end_trunc.signed_duration_since(start_trunc).num_days();
            days / 7
        }
        "day" => {
            // Truncate to day midnight, count days between truncated values.
            let start_trunc = start.date().and_hms_opt(0, 0, 0).unwrap_or(start);
            let end_trunc = end.date().and_hms_opt(0, 0, 0).unwrap_or(end);
            let diff = end_trunc.signed_duration_since(start_trunc);
            diff.num_days()
        }
        "hour" => {
            // Boundary-crossing: truncate both to hour, count hours between.
            let start_trunc = trunc_to_hour(start);
            let end_trunc = trunc_to_hour(end);
            let diff = end_trunc.signed_duration_since(start_trunc);
            diff.num_hours()
        }
        "minute" => {
            let start_trunc = trunc_to_minute(start);
            let end_trunc = trunc_to_minute(end);
            let diff = end_trunc.signed_duration_since(start_trunc);
            diff.num_minutes()
        }
        "second" => {
            let start_trunc = trunc_to_second(start);
            let end_trunc = trunc_to_second(end);
            let diff = end_trunc.signed_duration_since(start_trunc);
            diff.num_seconds()
        }
        _ => return EvalValue::Null,
    };
    EvalValue::Int(count)
}

fn trunc_to_hour(ts: NaiveDateTime) -> NaiveDateTime {
    ts.date().and_hms_opt(ts.hour(), 0, 0).unwrap_or(ts)
}

fn trunc_to_minute(ts: NaiveDateTime) -> NaiveDateTime {
    ts.date()
        .and_hms_opt(ts.hour(), ts.minute(), 0)
        .unwrap_or(ts)
}

fn trunc_to_second(ts: NaiveDateTime) -> NaiveDateTime {
    ts.date()
        .and_hms_opt(ts.hour(), ts.minute(), ts.second())
        .unwrap_or(ts)
}

/// `strftime(ts, fmt)` — DSL arg order is (ts, fmt); mirrors `STRFTIME(fmt, ts)`.
fn eval_strftime(args: &[EvalValue]) -> EvalValue {
    if args.len() != 2 {
        return EvalValue::Null;
    }
    let Some(ts) = args[0].as_timestamp() else {
        return EvalValue::Null;
    };
    let EvalValue::Str(fmt) = &args[1] else {
        return EvalValue::Null;
    };
    // A `fmt` is fully user-controlled. chrono turns an invalid/incompatible
    // specifier (e.g. `%Q`) into `Item::Error`, whose `Display` returns
    // `fmt::Error` — `ts.format(fmt).to_string()` would then PANIC ("a Display
    // implementation returned an error unexpectedly"). Detect the error item up
    // front and return Null instead. Invalid format LITERALS are now rejected at
    // emit/compile time in BOTH paths (see emitter::validate_format_literal),
    // so this guard is belt-and-suspenders: it defends against a non-literal
    // (field-ref) format that can't be checked upfront.
    if chrono::format::StrftimeItems::new(fmt)
        .any(|item| matches!(item, chrono::format::Item::Error))
    {
        return EvalValue::Null;
    }
    EvalValue::Str(ts.format(fmt).to_string())
}

/// `strptime(str, fmt)` — parse a string to `Timestamp` using chrono format.
///
/// `DuckDB`'s `STRPTIME` fills the components a format omits from a
/// `1900-01-01 00:00:00` base: a date-only format yields midnight, a time-only
/// format yields `1900-01-01`. chrono's `NaiveDateTime::parse_from_str` requires
/// BOTH halves, so we cascade full-datetime → date-only (→ `00:00:00`) →
/// time-only (→ `1900-01-01`) to mirror `DuckDB` for those cases. Each tier uses
/// chrono's own parser, which rejects trailing input, so a full-datetime string
/// never spuriously matches a date-only format (both paths `Null`, as `DuckDB`
/// does). Returns `Null` on unparseable input, matching the `TRY_STRPTIME` the
/// batch emitter now uses.
///
/// Residual divergence: other partial formats — year-only (`%Y`), year-month, a
/// bare month-day, or a date plus an INCOMPLETE time (`%Y-%m-%d %H`) — are not
/// matched here (the date-only tier drops the stray time fields `DuckDB` would
/// keep). These are documented in the DSL reference and tracked as a follow-up;
/// they are rare in practice and the common date-only/time-only cases are exact.
fn eval_strptime(args: &[EvalValue]) -> EvalValue {
    if args.len() != 2 {
        return EvalValue::Null;
    }
    let EvalValue::Str(s) = &args[0] else {
        return EvalValue::Null;
    };
    let EvalValue::Str(fmt) = &args[1] else {
        return EvalValue::Null;
    };
    let base_date = NaiveDate::from_ymd_opt(1900, 1, 1).expect("1900-01-01 is a valid date");
    NaiveDateTime::parse_from_str(s, fmt)
        .or_else(|_| NaiveDate::parse_from_str(s, fmt).map(|d| d.and_time(NaiveTime::MIN)))
        .or_else(|_| NaiveTime::parse_from_str(s, fmt).map(|t| base_date.and_time(t)))
        .map_or(EvalValue::Null, EvalValue::Timestamp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{BinaryOp, Expr, LiteralValue, Spanned, UnaryOp};
    use serde_json::json;

    fn span<T>(node: T) -> Spanned<T> {
        Spanned { node, span: 0..0 }
    }

    fn lit_int(n: i64) -> Spanned<Expr> {
        span(Expr::Literal(LiteralValue::Int(n)))
    }

    fn lit_float(n: f64) -> Spanned<Expr> {
        span(Expr::Literal(LiteralValue::Float(n)))
    }

    fn lit_str(s: &str) -> Spanned<Expr> {
        span(Expr::Literal(LiteralValue::String(s.to_string())))
    }

    fn lit_bool(b: bool) -> Spanned<Expr> {
        span(Expr::Literal(LiteralValue::Bool(b)))
    }

    fn lit_null() -> Spanned<Expr> {
        span(Expr::Literal(LiteralValue::Null))
    }

    fn field(name: &str) -> Spanned<Expr> {
        span(Expr::FieldRef(name.to_string()))
    }

    fn binary(lhs: Spanned<Expr>, op: BinaryOp, rhs: Spanned<Expr>) -> Spanned<Expr> {
        span(Expr::Binary {
            lhs: Box::new(lhs),
            op,
            rhs: Box::new(rhs),
        })
    }

    fn unary(op: UnaryOp, operand: Spanned<Expr>) -> Spanned<Expr> {
        span(Expr::Unary {
            op,
            operand: Box::new(operand),
        })
    }

    fn call(name: &str, args: Vec<Spanned<Expr>>) -> Spanned<Expr> {
        span(Expr::FunctionCall {
            name: name.to_string(),
            args,
        })
    }

    fn in_list(expr: Spanned<Expr>, list: Vec<Spanned<Expr>>) -> Spanned<Expr> {
        span(Expr::InList {
            expr: Box::new(expr),
            list,
        })
    }

    fn empty_event() -> Map<String, Value> {
        Map::new()
    }

    fn event(pairs: &Value) -> Map<String, Value> {
        pairs.as_object().unwrap().clone()
    }

    // ── literals ───────────────────────────────────────────────────

    #[test]
    fn literal_int() {
        assert_eq!(eval_expr(&lit_int(42), &empty_event()), EvalValue::Int(42));
    }

    #[test]
    fn literal_float() {
        assert_eq!(
            eval_expr(&lit_float(1.5), &empty_event()),
            EvalValue::Float(1.5)
        );
    }

    #[test]
    fn literal_string() {
        assert_eq!(
            eval_expr(&lit_str("hello"), &empty_event()),
            EvalValue::Str("hello".to_string())
        );
    }

    #[test]
    fn literal_bool() {
        assert_eq!(
            eval_expr(&lit_bool(true), &empty_event()),
            EvalValue::Bool(true)
        );
    }

    #[test]
    fn literal_null() {
        assert_eq!(eval_expr(&lit_null(), &empty_event()), EvalValue::Null);
    }

    // ── field refs ─────────────────────────────────────────────────

    #[test]
    fn field_ref_present() {
        let ev = event(&json!({"host": "web-1"}));
        assert_eq!(
            eval_expr(&field("host"), &ev),
            EvalValue::Str("web-1".to_string())
        );
    }

    #[test]
    fn field_ref_missing() {
        assert_eq!(eval_expr(&field("host"), &empty_event()), EvalValue::Null);
    }

    #[test]
    fn field_ref_timestamp_mapping() {
        let ev = event(&json!({"timestamp": "2026-01-01T00:00:00Z"}));
        assert_eq!(
            eval_expr(&field("@timestamp"), &ev),
            EvalValue::Str("2026-01-01T00:00:00Z".to_string())
        );
    }

    #[test]
    fn field_ref_time_alias() {
        let ev = event(&json!({"timestamp": "2026-01-01T00:00:00Z"}));
        assert_eq!(
            eval_expr(&field("_time"), &ev),
            EvalValue::Str("2026-01-01T00:00:00Z".to_string())
        );
    }

    #[test]
    fn field_ref_numeric() {
        let ev = event(&json!({"status": 200}));
        assert_eq!(eval_expr(&field("status"), &ev), EvalValue::Int(200));
    }

    // ── arithmetic ─────────────────────────────────────────────────

    #[test]
    fn add_ints() {
        let expr = binary(lit_int(2), BinaryOp::Add, lit_int(3));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(5));
    }

    #[test]
    fn add_int_float() {
        let expr = binary(lit_int(2), BinaryOp::Add, lit_float(1.5));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Float(3.5));
    }

    #[test]
    fn sub_ints() {
        let expr = binary(lit_int(10), BinaryOp::Sub, lit_int(3));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(7));
    }

    #[test]
    fn mul_ints() {
        let expr = binary(lit_int(4), BinaryOp::Mul, lit_int(5));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(20));
    }

    #[test]
    fn div_ints() {
        let expr = binary(lit_int(10), BinaryOp::Div, lit_int(3));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(3));
    }

    #[test]
    fn div_by_zero_int() {
        let expr = binary(lit_int(10), BinaryOp::Div, lit_int(0));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Null);
    }

    #[test]
    fn div_floats() {
        let expr = binary(lit_float(10.0), BinaryOp::Div, lit_float(4.0));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Float(2.5));
    }

    #[test]
    fn div_by_zero_float() {
        let expr = binary(lit_float(10.0), BinaryOp::Div, lit_float(0.0));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Null);
    }

    #[test]
    fn modulo_ints() {
        let expr = binary(lit_int(10), BinaryOp::Mod, lit_int(3));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(1));
    }

    #[test]
    fn modulo_by_zero() {
        let expr = binary(lit_int(10), BinaryOp::Mod, lit_int(0));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Null);
    }

    #[test]
    fn arithmetic_null_propagation() {
        let expr = binary(lit_int(5), BinaryOp::Add, lit_null());
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Null);
    }

    #[test]
    fn arithmetic_with_field() {
        let ev = event(&json!({"duration": 100}));
        let expr = binary(field("duration"), BinaryOp::Mul, lit_int(1000));
        assert_eq!(eval_expr(&expr, &ev), EvalValue::Int(100_000));
    }

    // ── comparison ─────────────────────────────────────────────────

    #[test]
    fn eq_ints() {
        let expr = binary(lit_int(5), BinaryOp::Eq, lit_int(5));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn eq_int_float_cross() {
        let expr = binary(lit_int(5), BinaryOp::Eq, lit_float(5.0));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn ne_ints() {
        let expr = binary(lit_int(5), BinaryOp::Ne, lit_int(3));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn gt_ints() {
        let expr = binary(lit_int(5), BinaryOp::Gt, lit_int(3));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn gt_false() {
        let expr = binary(lit_int(3), BinaryOp::Gt, lit_int(5));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(false));
    }

    #[test]
    fn gte_equal() {
        let expr = binary(lit_int(5), BinaryOp::Gte, lit_int(5));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn lt_ints() {
        let expr = binary(lit_int(3), BinaryOp::Lt, lit_int(5));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn lte_equal() {
        let expr = binary(lit_int(5), BinaryOp::Lte, lit_int(5));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn cmp_strings() {
        let expr = binary(lit_str("apple"), BinaryOp::Lt, lit_str("banana"));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn cmp_null_propagation() {
        let expr = binary(lit_int(5), BinaryOp::Gt, lit_null());
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Null);
    }

    // ── Timestamp vs Str coercion in eval_cmp/eval_eq ──────────────

    #[test]
    fn timestamp_gt_str_past_is_true() {
        // now() > a past timestamp string: should be true
        let expr = binary(
            call("now", vec![]),
            BinaryOp::Gt,
            lit_str("2000-01-01 00:00:00"),
        );
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn timestamp_lt_str_future_is_true() {
        let expr = binary(
            call("now", vec![]),
            BinaryOp::Lt,
            lit_str("2999-12-31 23:59:59"),
        );
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn timestamp_eq_str_same_instant() {
        // strptime produces Timestamp; compare to the same string
        let expr = binary(
            call(
                "strptime",
                vec![lit_str("2026-01-15 10:20:30"), lit_str("%Y-%m-%d %H:%M:%S")],
            ),
            BinaryOp::Eq,
            lit_str("2026-01-15 10:20:30"),
        );
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn where_timestamp_gt_now_for_future_event() {
        // Event with far-future timestamp: `timestamp > now()` should be true.
        let ev = event(&json!({"timestamp": "2999-12-31 23:59:59"}));
        let expr = binary(field("timestamp"), BinaryOp::Gt, call("now", vec![]));
        assert_eq!(eval_expr(&expr, &ev), EvalValue::Bool(true));
    }

    #[test]
    fn where_timestamp_gt_now_for_past_event() {
        // Event with past timestamp: `timestamp > now()` should be false.
        let ev = event(&json!({"timestamp": "2000-01-01 00:00:00"}));
        let expr = binary(field("timestamp"), BinaryOp::Gt, call("now", vec![]));
        assert_eq!(eval_expr(&expr, &ev), EvalValue::Bool(false));
    }

    // ── logical ops ────────────────────────────────────────────────

    #[test]
    fn and_true_true() {
        let expr = binary(lit_bool(true), BinaryOp::And, lit_bool(true));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn and_true_false() {
        let expr = binary(lit_bool(true), BinaryOp::And, lit_bool(false));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(false));
    }

    #[test]
    fn and_false_null() {
        // false AND null = false (SQL short-circuit)
        let expr = binary(lit_bool(false), BinaryOp::And, lit_null());
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(false));
    }

    #[test]
    fn and_null_true() {
        let expr = binary(lit_null(), BinaryOp::And, lit_bool(true));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Null);
    }

    #[test]
    fn or_true_null() {
        let expr = binary(lit_bool(true), BinaryOp::Or, lit_null());
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn or_false_false() {
        let expr = binary(lit_bool(false), BinaryOp::Or, lit_bool(false));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(false));
    }

    #[test]
    fn or_null_false() {
        let expr = binary(lit_null(), BinaryOp::Or, lit_bool(false));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Null);
    }

    // ── matches ────────────────────────────────────────────────────

    #[test]
    fn matches_regex() {
        let expr = binary(
            lit_str("prod-web-01"),
            BinaryOp::Matches,
            lit_str("prod-.*"),
        );
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn matches_no_match() {
        let expr = binary(
            lit_str("staging-01"),
            BinaryOp::Matches,
            lit_str("^prod-.*"),
        );
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(false));
    }

    // ── like / ilike ────────────────────────────────────────────────

    #[test]
    fn like_percent_wildcard() {
        let expr = binary(lit_str("prod-web-01"), BinaryOp::Like, lit_str("prod-%"));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn like_no_match() {
        let expr = binary(lit_str("staging-01"), BinaryOp::Like, lit_str("prod-%"));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(false));
    }

    #[test]
    fn like_underscore_wildcard() {
        let expr = binary(lit_str("a1"), BinaryOp::Like, lit_str("a_"));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn like_case_sensitive() {
        let expr = binary(lit_str("PROD-01"), BinaryOp::Like, lit_str("prod-%"));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(false));
    }

    #[test]
    fn ilike_case_insensitive() {
        let expr = binary(lit_str("PROD-01"), BinaryOp::ILike, lit_str("prod-%"));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn like_null_propagation() {
        let expr = binary(lit_null(), BinaryOp::Like, lit_str("prod-%"));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Null);
    }

    // ── unary ──────────────────────────────────────────────────────

    #[test]
    fn not_true() {
        let expr = unary(UnaryOp::Not, lit_bool(true));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(false));
    }

    #[test]
    fn not_false() {
        let expr = unary(UnaryOp::Not, lit_bool(false));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn not_null() {
        let expr = unary(UnaryOp::Not, lit_null());
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Null);
    }

    #[test]
    fn neg_int() {
        let expr = unary(UnaryOp::Neg, lit_int(42));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(-42));
    }

    #[test]
    fn neg_float() {
        let expr = unary(UnaryOp::Neg, lit_float(1.5));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Float(-1.5));
    }

    // ── in list ────────────────────────────────────────────────────

    #[test]
    fn in_list_found() {
        let expr = in_list(lit_int(200), vec![lit_int(200), lit_int(301), lit_int(404)]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn in_list_not_found() {
        let expr = in_list(lit_int(500), vec![lit_int(200), lit_int(301)]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(false));
    }

    #[test]
    fn in_list_null_target() {
        let expr = in_list(lit_null(), vec![lit_int(200)]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Null);
    }

    // ── scalar functions: string ───────────────────────────────────

    #[test]
    fn fn_lower() {
        let expr = call("lower", vec![lit_str("HELLO")]);
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("hello".to_string())
        );
    }

    #[test]
    fn fn_upper() {
        let expr = call("upper", vec![lit_str("hello")]);
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("HELLO".to_string())
        );
    }

    #[test]
    fn fn_length() {
        let expr = call("length", vec![lit_str("hello")]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(5));
    }

    #[test]
    fn fn_length_counts_chars_not_bytes() {
        // DuckDB LENGTH() returns character count; "café" is 4 chars / 5 bytes.
        let expr = call("length", vec![lit_str("café")]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(4));
    }

    #[test]
    fn fn_len_alias() {
        let expr = call("len", vec![lit_str("hi")]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(2));
    }

    #[test]
    fn fn_trim() {
        let expr = call("trim", vec![lit_str("  hello  ")]);
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("hello".to_string())
        );
    }

    #[test]
    fn fn_ltrim() {
        let expr = call("ltrim", vec![lit_str("  hello  ")]);
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("hello  ".to_string())
        );
    }

    #[test]
    fn fn_rtrim() {
        let expr = call("rtrim", vec![lit_str("  hello  ")]);
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("  hello".to_string())
        );
    }

    #[test]
    fn fn_replace() {
        let expr = call(
            "replace",
            vec![lit_str("foo bar foo"), lit_str("foo"), lit_str("baz")],
        );
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("baz bar baz".to_string())
        );
    }

    #[test]
    fn fn_substr_two_args() {
        let expr = call("substr", vec![lit_str("hello world"), lit_int(7)]);
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("world".to_string())
        );
    }

    #[test]
    fn fn_substr_three_args() {
        let expr = call(
            "substr",
            vec![lit_str("hello world"), lit_int(1), lit_int(5)],
        );
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("hello".to_string())
        );
    }

    #[test]
    fn fn_substr_null() {
        let expr = call("substr", vec![lit_null(), lit_int(1)]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Null);
    }

    #[test]
    fn substr_duckdb_window_semantics() {
        // Verified against DuckDB v1.5.4: negative/zero start, negative len,
        // out-of-range bounds, UTF-8 char counting. (start, Some(len)) -> out.
        let cases: &[(&str, i64, Option<i64>, &str)] = &[
            ("abcdef", -1, Some(3), "f"),
            ("abcdef", 0, Some(3), "ab"),
            ("abcdef", -2, Some(4), "ef"),
            ("abcdef", 1, Some(3), "abc"),
            ("abcdef", 2, Some(100), "bcdef"),
            ("abcdef", -5, Some(3), "bcd"),
            ("abcdef", 3, Some(0), ""),
            ("abcdef", 3, Some(-1), "b"),
            ("abcdef", -5, Some(7), "bcdef"),
            ("abcdef", 0, Some(1), ""),
            ("abcdef", 0, Some(7), "abcdef"),
            ("abcdef", -10, Some(5), "a"),
            ("abcdef", -10, Some(11), "abcdef"),
            ("abcdef", 3, Some(-2), "ab"),
            ("abcdef", 4, Some(-2), "bc"),
            ("abcdef", 1, Some(-1), ""),
            ("abcdef", 10, Some(3), ""),
            ("abcdef", 7, Some(3), ""),
            ("abcdef", 5, Some(-3), "bcd"),
            ("abcdef", 6, Some(-3), "cde"),
            ("abcdef", -1, Some(-3), "cde"),
            ("abcdef", 0, Some(-3), ""),
            ("abcdef", 2, Some(-5), "a"),
            ("abcdef", 4, Some(-5), "abc"),
            ("abcdef", 3, Some(-100), "ab"),
            ("abcdef", 3, None, "cdef"),
            ("abcdef", -2, None, "ef"),
            ("abcdef", 0, None, "abcdef"),
            ("abcdef", -100, Some(3), ""),
            ("abcdef", -6, Some(1), "a"),
            ("abcdef", -7, Some(2), "a"),
            ("héllo", 1, Some(3), "hél"),
            ("héllo", -2, Some(2), "lo"),
            ("", 1, Some(3), ""),
        ];
        for (s, start, len, want) in cases {
            let mut args = vec![EvalValue::Str((*s).to_string()), EvalValue::Int(*start)];
            if let Some(l) = len {
                args.push(EvalValue::Int(*l));
            }
            assert_eq!(
                eval_substr(&args),
                EvalValue::Str((*want).to_string()),
                "substr({s:?}, {start}, {len:?})"
            );
        }
        // NULL propagation.
        assert_eq!(
            eval_substr(&[EvalValue::Null, EvalValue::Int(1), EvalValue::Int(3)]),
            EvalValue::Null
        );
        assert_eq!(
            eval_substr(&[
                EvalValue::Str("x".into()),
                EvalValue::Null,
                EvalValue::Int(3)
            ]),
            EvalValue::Null
        );
        assert_eq!(
            eval_substr(&[
                EvalValue::Str("x".into()),
                EvalValue::Int(1),
                EvalValue::Null
            ]),
            EvalValue::Null
        );
    }

    #[test]
    fn abs_and_neg_null_on_i64_min_overflow() {
        // abs(i64::MIN) and -(i64::MIN) overflow; both must null out, not panic.
        let abs_expr = call("abs", vec![lit_int(i64::MIN)]);
        assert_eq!(eval_expr(&abs_expr, &empty_event()), EvalValue::Null);

        let neg_expr = unary(UnaryOp::Neg, lit_int(i64::MIN));
        assert_eq!(eval_expr(&neg_expr, &empty_event()), EvalValue::Null);

        // Sanity: ordinary values still work.
        let ok = call("abs", vec![lit_int(-5)]);
        assert_eq!(eval_expr(&ok, &empty_event()), EvalValue::Int(5));
    }

    #[test]
    fn substr_extreme_bounds_do_not_panic() {
        // i64::MIN/MAX start and len must clamp via saturating arithmetic, not
        // overflow-panic (debug) / wrap (release). The exact output is not the
        // contract here — surviving the math is.
        for (start, len) in [
            (i64::MIN, i64::MAX),
            (i64::MAX, i64::MAX),
            (i64::MIN, i64::MIN),
            (i64::MAX, i64::MIN),
        ] {
            assert!(matches!(
                eval_substr(&[
                    EvalValue::Str("abcdef".into()),
                    EvalValue::Int(start),
                    EvalValue::Int(len),
                ]),
                EvalValue::Str(_)
            ));
        }
        // Positive in-range len still works.
        assert_eq!(
            eval_substr(&[
                EvalValue::Str("abcdef".into()),
                EvalValue::Int(1),
                EvalValue::Int(i64::MAX),
            ]),
            EvalValue::Str("abcdef".into())
        );
    }

    #[test]
    fn duckdb_double_text_matches_cast_to_varchar() {
        // Verified against DuckDB v1.5.4 CAST(DOUBLE AS VARCHAR).
        let cases: &[(f64, &str)] = &[
            (1.0, "1.0"),
            (2.0, "2.0"),
            (0.0, "0.0"),
            (-0.0, "0.0"),
            (1.5, "1.5"),
            (-1.5, "-1.5"),
            (-42.75, "-42.75"),
            (0.1, "0.1"),
            (0.7, "0.7"),
            (1.1, "1.1"),
            (100.1, "100.1"),
            (123.456, "123.456"),
            (1.0 / 3.0, "0.3333333333333333"),
            (2.0 / 3.0, "0.6666666666666666"),
            (std::f64::consts::PI, "3.141592653589793"),
            (std::f64::consts::SQRT_2, "1.4142135623730951"),
            (0.123_456_789_012_345_68, "0.12345678901234568"),
            (0.300_000_000_000_000_04, "0.30000000000000004"),
            (100_000.0, "100000.0"),
            (1_000_000_000_000_000.0, "1000000000000000.0"),
            (9_999_000_000_000_000.0, "9999000000000000.0"),
            (1e16, "1e+16"),
            (1.5e16, "1.5e+16"),
            (1e17, "1e+17"),
            (1e20, "1e+20"),
            (1.234_567_89e30, "1.23456789e+30"),
            (-1.234_567_89e30, "-1.23456789e+30"),
            (1e100, "1e+100"),
            (1.797_693_134_862_315_7e308, "1.7976931348623157e+308"),
            (0.001, "0.001"),
            (0.0001, "0.0001"),
            (0.000_123_4, "0.0001234"),
            (9.999e-5, "9.999e-05"),
            (1e-5, "1e-05"),
            (1.234e-5, "1.234e-05"),
            (1e-7, "1e-07"),
            (1e-10, "1e-10"),
            (1e-99, "1e-99"),
            (1e-100, "1e-100"),
            (1e-308, "1e-308"),
            (5e-324, "5e-324"),
            (1.234_567_89e-30, "1.23456789e-30"),
            (f64::INFINITY, "inf"),
            (f64::NEG_INFINITY, "-inf"),
            (f64::NAN, "nan"),
        ];
        for (x, want) in cases {
            assert_eq!(&duckdb_double_to_string(*x), want, "duckdb_double({x})");
        }
    }

    #[test]
    fn fn_tostring_float_renders_duckdb_way() {
        // tostring(1.0) -> "1.0" (DuckDB), not "1" (Rust to_string).
        let expr = call("tostring", vec![lit_float(1.0)]);
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("1.0".to_string())
        );
    }

    #[test]
    fn fn_concat_float_renders_duckdb_way() {
        let expr = call("concat", vec![lit_str("x="), lit_float(0.1)]);
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("x=0.1".to_string())
        );
        let expr = call("concat", vec![lit_str("n="), lit_float(2.0)]);
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("n=2.0".to_string())
        );
    }

    #[test]
    fn fn_concat_mixed_types() {
        // DuckDB casts each arg to VARCHAR before joining (int/float/bool/ts).
        let expr = call(
            "concat",
            vec![lit_str("n="), lit_int(42), lit_str("/"), lit_bool(true)],
        );
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("n=42/true".to_string())
        );
    }

    #[test]
    fn fn_concat_skips_null() {
        // DuckDB CONCAT *ignores* NULL args (unlike `||`): CONCAT('a',NULL,'b')=='ab'.
        let expr = call("concat", vec![lit_str("a"), lit_null(), lit_str("b")]);
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("ab".to_string())
        );
    }

    #[test]
    fn fn_concat_trailing_null() {
        // CONCAT('x','-',NULL) == 'x-' (the #22 batch-vs-live drift case).
        let expr = call("concat", vec![lit_str("x"), lit_str("-"), lit_null()]);
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("x-".to_string())
        );
    }

    #[test]
    fn fn_concat_all_null_is_empty_string() {
        // DuckDB CONCAT(NULL) == '' (empty string), NOT NULL.
        let expr = call("concat", vec![lit_null(), lit_null()]);
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str(String::new())
        );
    }

    // ── scalar functions: numeric ──────────────────────────────────

    #[test]
    fn fn_abs_positive() {
        let expr = call("abs", vec![lit_int(-42)]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(42));
    }

    #[test]
    fn fn_abs_float() {
        let expr = call("abs", vec![lit_float(-1.5)]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Float(1.5));
    }

    #[test]
    fn fn_ceil() {
        let expr = call("ceil", vec![lit_float(1.2)]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(2));
    }

    #[test]
    fn fn_ceiling_alias() {
        let expr = call("ceiling", vec![lit_float(1.2)]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(2));
    }

    #[test]
    fn fn_floor() {
        let expr = call("floor", vec![lit_float(1.8)]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(1));
    }

    #[test]
    fn fn_round_no_precision() {
        let expr = call("round", vec![lit_float(1.6)]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(2));
    }

    #[test]
    fn fn_round_with_precision() {
        let expr = call("round", vec![lit_float(1.456), lit_int(2)]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Float(1.46));
    }

    #[test]
    fn fn_round_int_passthrough() {
        let expr = call("round", vec![lit_int(42)]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(42));
    }

    // ── scalar functions: conditional ──────────────────────────────

    #[test]
    fn fn_if_true() {
        let expr = call("if", vec![lit_bool(true), lit_str("yes"), lit_str("no")]);
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("yes".to_string())
        );
    }

    #[test]
    fn fn_if_false() {
        let expr = call("if", vec![lit_bool(false), lit_str("yes"), lit_str("no")]);
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("no".to_string())
        );
    }

    #[test]
    fn fn_coalesce_first_non_null() {
        let expr = call("coalesce", vec![lit_null(), lit_int(42), lit_int(99)]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(42));
    }

    #[test]
    fn fn_coalesce_all_null() {
        let expr = call("coalesce", vec![lit_null(), lit_null()]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Null);
    }

    #[test]
    fn fn_isnull_null() {
        let expr = call("isnull", vec![lit_null()]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn fn_isnull_not_null() {
        let expr = call("isnull", vec![lit_int(5)]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(false));
    }

    #[test]
    fn fn_isnotnull() {
        let expr = call("isnotnull", vec![lit_str("hi")]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn fn_typeof_int() {
        let expr = call("typeof", vec![lit_int(5)]);
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("INTEGER".to_string())
        );
    }

    #[test]
    fn fn_typeof_string() {
        let expr = call("typeof", vec![lit_str("hi")]);
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("VARCHAR".to_string())
        );
    }

    #[test]
    fn fn_typeof_null() {
        let expr = call("typeof", vec![lit_null()]);
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("NULL".to_string())
        );
    }

    // now() returns a Timestamp, so this asserts Timestamp(_) rather than a
    // string/int representation.
    #[test]
    fn fn_now_returns_timestamp() {
        let before = chrono::Utc::now().naive_utc();
        let expr = call("now", vec![]);
        let result = eval_expr(&expr, &empty_event());
        let after = chrono::Utc::now().naive_utc();
        let EvalValue::Timestamp(ts) = result else {
            panic!("expected Timestamp, got {result:?}");
        };
        assert!(ts >= before, "now() timestamp should be >= start");
        assert!(ts <= after, "now() timestamp should be <= end");
    }

    // ── Timestamp variant ──────────────────────────────────────────

    #[test]
    fn timestamp_to_duckdb_text_no_frac() {
        let ts = chrono::NaiveDateTime::parse_from_str("2026-01-15 14:30:00", "%Y-%m-%d %H:%M:%S")
            .unwrap();
        assert_eq!(timestamp_to_duckdb_text(&ts), "2026-01-15 14:30:00");
    }

    #[test]
    fn timestamp_to_duckdb_text_with_micros() {
        let ts = chrono::NaiveDateTime::parse_from_str(
            "2026-01-15 14:30:00.123456",
            "%Y-%m-%d %H:%M:%S%.f",
        )
        .unwrap();
        assert_eq!(timestamp_to_duckdb_text(&ts), "2026-01-15 14:30:00.123456");
    }

    #[test]
    fn timestamp_to_duckdb_text_trims_trailing_zeros() {
        let ts = chrono::NaiveDateTime::parse_from_str(
            "2026-01-15 14:30:00.100000",
            "%Y-%m-%d %H:%M:%S%.f",
        )
        .unwrap();
        // DuckDB trims trailing zeros: .100000 → .1
        assert_eq!(timestamp_to_duckdb_text(&ts), "2026-01-15 14:30:00.1");
    }

    #[test]
    fn as_timestamp_from_t_separator() {
        let v = EvalValue::Str("2026-01-15T14:30:00Z".to_string());
        let ts = v.as_timestamp().unwrap();
        assert_eq!(ts.to_string(), "2026-01-15 14:30:00");
    }

    #[test]
    fn as_timestamp_from_space_separator() {
        let v = EvalValue::Str("2026-01-15 14:30:00".to_string());
        let ts = v.as_timestamp().unwrap();
        assert_eq!(ts.to_string(), "2026-01-15 14:30:00");
    }

    #[test]
    fn as_timestamp_date_only_is_midnight() {
        let v = EvalValue::Str("2026-01-15".to_string());
        let ts = v.as_timestamp().unwrap();
        assert_eq!(ts.to_string(), "2026-01-15 00:00:00");
    }

    #[test]
    fn as_timestamp_discards_offset() {
        // Offset +02:00 is discarded, wall-clock components are kept.
        let v = EvalValue::Str("2026-01-15T14:30:00+02:00".to_string());
        let ts = v.as_timestamp().unwrap();
        assert_eq!(ts.to_string(), "2026-01-15 14:30:00");
    }

    #[test]
    fn as_timestamp_unparseable_is_none() {
        let v = EvalValue::Str("not-a-date".to_string());
        assert!(v.as_timestamp().is_none());
    }

    #[test]
    fn as_timestamp_non_utf8_boundary_does_not_panic() {
        // Regression: strip_offset's `&s[s.len() - 6..]` byte-sliced into the
        // middle of a multi-byte UTF-8 sequence and panicked. "🦀🦀" is 8 bytes;
        // index 2 (len - 6) is a continuation byte. This is reachable from
        // ingested log field values via Str<->Timestamp coercion, so it must
        // return None gracefully, not panic.
        let v = EvalValue::Str("🦀🦀".to_string());
        assert!(v.as_timestamp().is_none());

        // A field value whose byte at len - 6 lands mid-emoji.
        let v = EvalValue::Str("err: 🦀🦀".to_string());
        assert!(v.as_timestamp().is_none());
    }

    #[test]
    fn timestamp_is_truthy() {
        let ts = chrono::NaiveDateTime::parse_from_str("2026-01-15 00:00:00", "%Y-%m-%d %H:%M:%S")
            .unwrap();
        assert!(EvalValue::Timestamp(ts).is_truthy());
    }

    #[test]
    fn timestamp_to_json_is_duckdb_text() {
        let ts = chrono::NaiveDateTime::parse_from_str("2026-01-15 14:30:00", "%Y-%m-%d %H:%M:%S")
            .unwrap();
        let json_val = Value::from(EvalValue::Timestamp(ts));
        assert_eq!(json_val, json!("2026-01-15 14:30:00"));
    }

    // ── EvalValue conversions ──────────────────────────────────────

    #[test]
    fn eval_value_to_json_roundtrip() {
        let cases = vec![
            (EvalValue::Null, Value::Null),
            (EvalValue::Bool(true), Value::Bool(true)),
            (EvalValue::Int(42), json!(42)),
            (EvalValue::Float(1.5), json!(1.5)),
            (EvalValue::Str("hi".to_string()), json!("hi")),
        ];
        for (eval_val, expected_json) in cases {
            assert_eq!(Value::from(eval_val), expected_json);
        }
    }

    #[test]
    fn json_to_eval_value() {
        let cases = vec![
            (json!(null), EvalValue::Null),
            (json!(true), EvalValue::Bool(true)),
            (json!(42), EvalValue::Int(42)),
            (json!(1.5), EvalValue::Float(1.5)),
            (json!("hi"), EvalValue::Str("hi".to_string())),
        ];
        for (json_val, expected_eval) in cases {
            assert_eq!(EvalValue::from(&json_val), expected_eval);
        }
    }

    // ── truthiness ─────────────────────────────────────────────────

    #[test]
    fn truthiness() {
        assert!(!EvalValue::Null.is_truthy());
        assert!(!EvalValue::Bool(false).is_truthy());
        assert!(EvalValue::Bool(true).is_truthy());
        assert!(!EvalValue::Int(0).is_truthy());
        assert!(EvalValue::Int(1).is_truthy());
        assert!(!EvalValue::Str(String::new()).is_truthy());
        assert!(EvalValue::Str("x".to_string()).is_truthy());
    }

    // ── complex expressions (integration-style) ────────────────────

    #[test]
    fn complex_where_condition() {
        // status >= 400 and status < 500
        let ev = event(&json!({"status": 404}));
        let expr = binary(
            binary(field("status"), BinaryOp::Gte, lit_int(400)),
            BinaryOp::And,
            binary(field("status"), BinaryOp::Lt, lit_int(500)),
        );
        assert_eq!(eval_expr(&expr, &ev), EvalValue::Bool(true));
    }

    #[test]
    fn complex_let_expression() {
        // duration * 1000 + 50
        let ev = event(&json!({"duration": 2}));
        let expr = binary(
            binary(field("duration"), BinaryOp::Mul, lit_int(1000)),
            BinaryOp::Add,
            lit_int(50),
        );
        assert_eq!(eval_expr(&expr, &ev), EvalValue::Int(2050));
    }

    #[test]
    fn nested_function_call() {
        // upper(lower("HeLLo"))
        let expr = call("upper", vec![call("lower", vec![lit_str("HeLLo")])]);
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("HELLO".to_string())
        );
    }

    #[test]
    fn if_with_field_condition() {
        // if(isnotnull(message), length(message), 0)
        let ev = event(&json!({"message": "hello world"}));
        let expr = call(
            "if",
            vec![
                call("isnotnull", vec![field("message")]),
                call("length", vec![field("message")]),
                lit_int(0),
            ],
        );
        assert_eq!(eval_expr(&expr, &ev), EvalValue::Int(11));
    }

    #[test]
    fn if_with_null_field() {
        let ev = event(&json!({"host": "web-1"}));
        let expr = call(
            "if",
            vec![
                call("isnotnull", vec![field("message")]),
                call("length", vec![field("message")]),
                lit_int(0),
            ],
        );
        assert_eq!(eval_expr(&expr, &ev), EvalValue::Int(0));
    }

    // ── date/time scalar functions ─────────────────────────────────

    fn ts(s: &str) -> Spanned<Expr> {
        lit_str(s)
    }

    fn ndt(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
            .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f"))
            .unwrap_or_else(|e| panic!("bad ndt str {s}: {e}"))
    }

    // tonumber
    #[test]
    fn fn_tonumber_from_str() {
        let expr = call("tonumber", vec![lit_str("1.23")]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Float(1.23));
    }

    #[test]
    fn fn_tonumber_from_int() {
        let expr = call("tonumber", vec![lit_int(42)]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Float(42.0));
    }

    #[test]
    fn fn_tonumber_from_float() {
        let expr = call("tonumber", vec![lit_float(1.5)]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Float(1.5));
    }

    #[test]
    fn fn_tonumber_non_numeric_str() {
        let expr = call("tonumber", vec![lit_str("nope")]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Null);
    }

    #[test]
    fn fn_tonumber_honors_inner_digit_separators() {
        // `_` flanked by ASCII digits on both sides is dropped (DuckDB TRY_CAST).
        let expr = call("tonumber", vec![lit_str("1_000")]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Float(1000.0));

        let expr = call("tonumber", vec![lit_str("1_0.0_5")]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Float(10.05));
    }

    #[test]
    fn fn_tonumber_rejects_misplaced_digit_separators() {
        // Each of these keeps a `_` that lacks an ASCII digit on BOTH sides, so
        // f64::parse fails -> Null, exactly like DuckDB TRY_CAST.
        for bad in ["_1000", "1000_", "1__000", "1_e3", "1,000"] {
            let expr = call("tonumber", vec![lit_str(bad)]);
            assert_eq!(
                eval_expr(&expr, &empty_event()),
                EvalValue::Null,
                "tonumber({bad:?}) should be Null"
            );
        }
    }

    // tostring
    #[test]
    fn fn_tostring_int() {
        let expr = call("tostring", vec![lit_int(42)]);
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("42".to_string())
        );
    }

    #[test]
    fn fn_tostring_null() {
        let expr = call("tostring", vec![lit_null()]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Null);
    }

    #[test]
    fn fn_tostring_timestamp() {
        let expr = call(
            "tostring",
            vec![call(
                "strptime",
                vec![lit_str("2026-01-15 14:30:00"), lit_str("%Y-%m-%d %H:%M:%S")],
            )],
        );
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("2026-01-15 14:30:00".to_string())
        );
    }

    // date_part
    #[test]
    fn fn_date_part_year() {
        let expr = call(
            "date_part",
            vec![lit_str("year"), ts("2026-03-15 10:20:30")],
        );
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(2026));
    }

    #[test]
    fn fn_date_part_month() {
        let expr = call(
            "date_part",
            vec![lit_str("month"), ts("2026-03-15 10:20:30")],
        );
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(3));
    }

    #[test]
    fn fn_date_part_day() {
        let expr = call("date_part", vec![lit_str("day"), ts("2026-03-15 10:20:30")]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(15));
    }

    #[test]
    fn fn_date_part_hour() {
        let expr = call(
            "date_part",
            vec![lit_str("hour"), ts("2026-03-15 10:20:30")],
        );
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(10));
    }

    #[test]
    fn fn_date_part_dow_sunday() {
        // 2026-03-15 is a Sunday → DOW=0
        let expr = call("date_part", vec![lit_str("dow"), ts("2026-03-15 00:00:00")]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(0));
    }

    #[test]
    fn fn_date_part_quarter() {
        let expr = call(
            "date_part",
            vec![lit_str("quarter"), ts("2026-07-01 00:00:00")],
        );
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(3));
    }

    #[test]
    fn fn_date_part_unknown_unit() {
        let expr = call(
            "date_part",
            vec![lit_str("millennium"), ts("2026-01-01 00:00:00")],
        );
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Null);
    }

    // date_trunc
    #[test]
    fn fn_date_trunc_year() {
        let expr = call(
            "date_trunc",
            vec![lit_str("year"), ts("2026-07-15 10:20:30")],
        );
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Timestamp(ndt("2026-01-01 00:00:00"))
        );
    }

    #[test]
    fn fn_date_trunc_month() {
        let expr = call(
            "date_trunc",
            vec![lit_str("month"), ts("2026-07-15 10:20:30")],
        );
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Timestamp(ndt("2026-07-01 00:00:00"))
        );
    }

    #[test]
    fn fn_date_trunc_day() {
        let expr = call(
            "date_trunc",
            vec![lit_str("day"), ts("2026-07-15 10:20:30")],
        );
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Timestamp(ndt("2026-07-15 00:00:00"))
        );
    }

    #[test]
    fn fn_date_trunc_hour() {
        let expr = call(
            "date_trunc",
            vec![lit_str("hour"), ts("2026-07-15 10:20:30")],
        );
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Timestamp(ndt("2026-07-15 10:00:00"))
        );
    }

    #[test]
    fn fn_date_trunc_week_to_monday() {
        // 2026-07-15 is a Wednesday → week should truncate to Monday 2026-07-13
        let expr = call(
            "date_trunc",
            vec![lit_str("week"), ts("2026-07-15 10:20:30")],
        );
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Timestamp(ndt("2026-07-13 00:00:00"))
        );
    }

    // date_diff — boundary-crossing cases
    #[test]
    fn fn_date_diff_hour_boundary_crossing() {
        // canonical case: 23:59:59 → 00:00:00 next day = 1 hour boundary crossed
        let expr = call(
            "date_diff",
            vec![
                lit_str("hour"),
                ts("2026-01-01 23:59:59"),
                ts("2026-01-02 00:00:00"),
            ],
        );
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(1));
    }

    #[test]
    fn fn_date_diff_day() {
        let expr = call(
            "date_diff",
            vec![
                lit_str("day"),
                ts("2026-01-01 00:00:00"),
                ts("2026-01-08 00:00:00"),
            ],
        );
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(7));
    }

    #[test]
    fn fn_date_diff_month() {
        let expr = call(
            "date_diff",
            vec![
                lit_str("month"),
                ts("2026-01-15 00:00:00"),
                ts("2026-03-15 00:00:00"),
            ],
        );
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(2));
    }

    #[test]
    fn fn_date_diff_negative() {
        // end before start → negative count
        let expr = call(
            "date_diff",
            vec![
                lit_str("day"),
                ts("2026-01-08 00:00:00"),
                ts("2026-01-01 00:00:00"),
            ],
        );
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(-7));
    }

    #[test]
    fn fn_date_diff_week_boundary() {
        // Mon 2026-07-13 → Mon 2026-07-20 = 1 week
        let expr = call(
            "date_diff",
            vec![
                lit_str("week"),
                ts("2026-07-13 23:59:59"),
                ts("2026-07-20 00:00:00"),
            ],
        );
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(1));
    }

    // strftime
    #[test]
    fn fn_strftime_basic() {
        let expr = call(
            "strftime",
            vec![ts("2026-03-15 10:20:30"), lit_str("%Y-%m-%d")],
        );
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("2026-03-15".to_string())
        );
    }

    #[test]
    fn fn_strftime_full() {
        let expr = call(
            "strftime",
            vec![ts("2026-03-15 10:20:30"), lit_str("%Y-%m-%d %H:%M:%S")],
        );
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("2026-03-15 10:20:30".to_string())
        );
    }

    #[test]
    fn fn_strftime_invalid_specifier_returns_null_not_panic() {
        // `%Q` is not a chrono specifier; chrono yields Item::Error whose
        // Display returns fmt::Error, so the old `.to_string()` panicked.
        // Remote-triggerable via the SSE streaming path (#22 reviewer finding).
        let expr = call("strftime", vec![ts("2026-03-15 10:20:30"), lit_str("%Q")]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Null);
    }

    #[test]
    fn fn_strftime_trailing_percent_returns_null_not_panic() {
        // A dangling `%` is also an Item::Error in chrono.
        let expr = call("strftime", vec![ts("2026-03-15 10:20:30"), lit_str("%Y-%")]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Null);
    }

    // strptime
    #[test]
    fn fn_strptime_success() {
        let expr = call(
            "strptime",
            vec![lit_str("2026-03-15 10:20:30"), lit_str("%Y-%m-%d %H:%M:%S")],
        );
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Timestamp(ndt("2026-03-15 10:20:30"))
        );
    }

    #[test]
    fn fn_strptime_failure_returns_null() {
        let expr = call(
            "strptime",
            vec![lit_str("not-a-date"), lit_str("%Y-%m-%d %H:%M:%S")],
        );
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Null);
    }

    // typeof(now())
    #[test]
    fn fn_typeof_now_is_timestamp() {
        let expr = call("typeof", vec![call("now", vec![])]);
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Str("TIMESTAMP".to_string())
        );
    }

    // coverage test — every non-aggregate KNOWN_FUNCTION must return Some(_)
    // from eval_scalar_fn. Fails CI if a scalar is added to the emitter but not eval.
    #[test]
    fn all_scalar_known_functions_return_some() {
        use crate::emitter::is_aggregate_function;
        use crate::parser::suggest::KNOWN_FUNCTIONS;
        // Generous arg list: enough variety that arity/type checks don't block.
        let ts_val = EvalValue::Timestamp(ndt("2026-01-15 10:20:30"));
        let generous_args = vec![
            ts_val.clone(),
            EvalValue::Str("year".to_string()),
            EvalValue::Int(1),
            EvalValue::Float(1.0),
            EvalValue::Bool(true),
        ];
        for &func in KNOWN_FUNCTIONS {
            if is_aggregate_function(func) {
                continue; // aggregates are not in eval_scalar_fn
            }
            let result = eval_scalar_fn(func, &generous_args);
            assert!(
                result.is_some(),
                "eval_scalar_fn({func:?}, ...) returned None — \
                 add it to eval_scalar_fn or the coverage test will keep failing"
            );
        }
    }
}
