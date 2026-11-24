// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! In-memory expression evaluator for the pipeline stages that run
//! outside SQL.

use std::borrow::Cow;

use crate::ast::{BinaryOp, Expr, FilterOp, FloatLiteral, LiteralValue, Spanned, UnaryOp};
use crate::compare;
use crate::context::EvalContext;
use crate::emitter::SqlValue;
use crate::pattern::{PatternError, PatternKind, literal_operand};
use crate::pin_match::{self, NullReadPolicy};
use crate::pin_scope::{PinScope, PinnedSubject};
use crate::row::Row;
use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use serde_json::Value;

pub use crate::pattern::table::PatternTable;

#[derive(Debug, Clone, PartialEq)]
pub enum EvalValue {
    Null,
    Bool(bool),
    Int(i64),
    /// An unsigned integer above `i64::MAX`. Every value-domain operation reads
    /// it as the `f64` it rounds to; the wire egress, `cell_text`, `cell_key`
    /// and the pinned read reproduce the digits the sender sent.
    UInt(u64),
    Float(f64),
    Str(String),
    Array(Vec<EvalValue>),
    /// A `DuckDB` TIMESTAMP: the wall-clock instant its `AS TIMESTAMP`
    /// cast produces, or one of the two infinities no calendar date can
    /// express. Never unwrap the finite arm inside a comparison —
    /// [`EvalValue::as_finite`] is that door.
    Timestamp(compare::Instant),
}

impl EvalValue {
    /// This is not `DuckDB`'s boolean domain, which [`read_condition`] mirrors
    /// and what `if`/`case` read their condition through.
    pub fn is_truthy(&self) -> bool {
        match self {
            Self::Null => false,
            Self::Bool(b) => *b,
            Self::Int(n) => *n != 0,
            Self::UInt(n) => *n != 0,
            Self::Float(n) => *n != 0.0,
            Self::Str(s) => !s.is_empty(),
            Self::Array(a) => !a.is_empty(),
            Self::Timestamp(_) => true,
        }
    }

    #[allow(clippy::cast_precision_loss)]
    fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Int(n) => Some(*n as f64),
            Self::UInt(n) => Some(*n as f64),
            Self::Float(n) => Some(*n),
            _ => None,
        }
    }

    fn as_str_repr(&self) -> Option<String> {
        match self {
            Self::Str(s) => Some(s.clone()),
            Self::Int(n) => Some(n.to_string()),
            #[allow(clippy::cast_precision_loss)]
            Self::UInt(n) => Some(duckdb_double_to_string(*n as f64)),
            Self::Float(n) => Some(duckdb_double_to_string(*n)),
            Self::Bool(b) => Some(b.to_string()),
            Self::Timestamp(instant) => Some(instant.cast_text()),
            Self::Null | Self::Array(_) => None,
        }
    }

    pub fn as_instant(&self) -> Option<compare::Instant> {
        match self {
            Self::Timestamp(instant) => Some(*instant),
            Self::Str(s) => compare::literal_timestamp(s),
            _ => None,
        }
    }

    /// An infinity is `None` here on purpose: `date_part` and `date_diff`
    /// answer NULL for one (probed, every unit).
    pub(crate) fn as_finite(&self) -> Option<NaiveDateTime> {
        match self.as_instant()? {
            compare::Instant::At(at) => Some(at),
            compare::Instant::Infinity | compare::Instant::NegInfinity => None,
        }
    }
}

pub fn timestamp_to_duckdb_text(ts: &NaiveDateTime) -> String {
    let micros = ts.nanosecond() / 1000;
    if micros == 0 {
        ts.format("%Y-%m-%d %H:%M:%S").to_string()
    } else {
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
/// 2. Negative zero keeps its sign: `-0.0` → `"-0.0"`.
/// 3. Scientific notation kicks in at the same magnitude thresholds as Rust's
///    `{:?}` (`>= 1e16` and `< 1e-4`), so we lean on Debug for the switch-over.
/// 4. The exponent always carries a sign and is zero-padded to a minimum of two
///    digits (`e+05`, `e-05`, `e+16`, `e+100`), where Rust `{:?}` emits `e16` /
///    `e-5` (no sign, no pad).
/// 5. Specials render lowercase: `inf`, `-inf`, `nan` — and a NaN keeps
///    its sign bit like any other value, so a negative one renders
///    `-nan` (probed in `a_rendered_nan_keeps_its_sign`).
pub(crate) fn duckdb_double_to_string(x: f64) -> String {
    if x.is_nan() {
        // The sign bit, not the ordering: `-nan < 0.0` is false, so the
        // infinity test below would not have caught it.
        return if x.is_sign_negative() { "-nan" } else { "nan" }.to_string();
    }
    if x.is_infinite() {
        return if x < 0.0 { "-inf" } else { "inf" }.to_string();
    }
    let s = format!("{x:?}");
    let Some(e_pos) = s.find('e') else {
        return s;
    };

    let (mantissa, exp) = s.split_at(e_pos);
    let exp = &exp[1..];
    let (sign, mag) = match exp.strip_prefix('-') {
        Some(rest) => ('-', rest),
        None => ('+', exp.strip_prefix('+').unwrap_or(exp)),
    };
    let mag: u32 = mag.parse().unwrap_or(0);
    format!("{mantissa}e{sign}{mag:02}")
}

impl From<EvalValue> for Value {
    fn from(v: EvalValue) -> Self {
        match v {
            EvalValue::Null => Value::Null,
            EvalValue::Bool(b) => Value::Bool(b),
            EvalValue::Int(n) => Value::Number(n.into()),
            EvalValue::UInt(n) => Value::Number(n.into()),
            EvalValue::Float(n) => {
                serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number)
            }
            EvalValue::Str(s) => Value::String(s),
            EvalValue::Array(a) => Value::Array(a.into_iter().map(Value::from).collect()),
            // DuckDB's own cast text, so the event map round-trips
            // through serde_json — and so the two infinities egress as
            // the words `infinity`/`-infinity` rather than as nothing.
            EvalValue::Timestamp(instant) => Value::String(instant.cast_text()),
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
                } else if let Some(u) = n.as_u64() {
                    Self::UInt(u)
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

/// The pin-blind door: every comparison stays literal-driven, which is
/// embedded mode's behaviour and the zero-cost path when no catalog
/// exists. Catalog-backed callers go through [`eval_expr_with_pins`].
///
/// # Errors
///
/// [`PatternError::Missing`] when a pattern operand is not in
/// `patterns`, which is never read as a non-match.
pub fn eval_expr(
    expr: &Spanned<Expr>,
    event: &Row,
    patterns: &PatternTable,
    ctx: &EvalContext,
) -> Result<EvalValue, PatternError> {
    static EMPTY: std::sync::LazyLock<PinScope> = std::sync::LazyLock::new(PinScope::unpinned);
    eval_expr_with_pins(expr, event, &EMPTY, patterns, ctx)
}

/// Evaluate an expression with the catalog's pin scope typing bare
/// field-vs-literal comparisons (ADR-0011).
pub fn eval_expr_with_pins(
    expr: &Spanned<Expr>,
    event: &Row,
    pins: &PinScope,
    patterns: &PatternTable,
    ctx: &EvalContext,
) -> Result<EvalValue, PatternError> {
    let eval = |expr| eval_expr_with_pins(expr, event, pins, patterns, ctx);
    Ok(match &expr.node {
        Expr::Literal(lit) => eval_literal(lit),
        Expr::FieldRef(name) => {
            let mapped = name.as_str();
            bind_event_key(event, mapped)
                .and_then(|key| event.get(key))
                .cloned()
                .unwrap_or(EvalValue::Null)
        }
        Expr::Binary { lhs, op, rhs } => {
            if let Some(kind) = PatternKind::of_operator(*op) {
                return eval_pattern(lhs, kind, rhs, event, pins, patterns, ctx);
            }
            match try_pinned_comparison(lhs, *op, rhs, event, pins, patterns, ctx)? {
                Some(answer) => answer,
                None => eval_binary(&eval(lhs)?, *op, &eval(rhs)?),
            }
        }
        Expr::Unary { op, operand } => eval_unary(*op, eval(operand)?),
        Expr::FunctionCall { name, args } => {
            let evaluated = args.iter().map(eval).collect::<Result<Vec<_>, _>>()?;
            eval_scalar_fn(name, &evaluated, ctx).unwrap_or(EvalValue::Null)
        }
        Expr::InList { expr: target, list } => {
            if let Some(answer) = try_pinned_in_list(target, list, event, pins, patterns, ctx)? {
                return Ok(answer);
            }
            let target_val = eval(target)?;
            if matches!(target_val, EvalValue::Null) {
                return Ok(EvalValue::Null);
            }
            // SQL's `IN` is three-valued: an element that answers UNKNOWN
            // makes a non-match UNKNOWN rather than FALSE, and collapsing it
            // to FALSE inverts under `NOT`.
            let mut unknown = false;
            for item in list {
                match eval_eq(&target_val, &eval(item)?) {
                    EvalValue::Bool(true) => return Ok(EvalValue::Bool(true)),
                    EvalValue::Bool(false) => {}
                    _ => unknown = true,
                }
            }
            if unknown {
                EvalValue::Null
            } else {
                EvalValue::Bool(false)
            }
        }
    })
}

/// The lookup comes first, so a pattern the stage did not prepare fails
/// on the first event whatever that event holds.
fn eval_pattern(
    lhs: &Spanned<Expr>,
    kind: PatternKind,
    rhs: &Spanned<Expr>,
    event: &Row,
    pins: &PinScope,
    patterns: &PatternTable,
    ctx: &EvalContext,
) -> Result<EvalValue, PatternError> {
    let pattern = patterns.get(kind, literal_operand(&rhs.node)?)?;
    if let Some((subject, pin)) = pins.subject_pin(lhs) {
        let form = crate::compare::pattern_form(Some(pin));
        let Some(value) = subject_value(&subject, event, pins, patterns, ctx)? else {
            return Ok(EvalValue::Null);
        };
        // No canonical text (a BIGINT pin over "4.5") is a NULL pattern
        // target in batch — UNKNOWN, not false.
        return pin_match::pattern_text(&value, form).map_or(Ok(EvalValue::Null), |text| {
            pattern.is_match(&text).map(EvalValue::Bool)
        });
    }
    match eval_expr_with_pins(lhs, event, pins, patterns, ctx)? {
        EvalValue::Str(text) => pattern.is_match(&text).map(EvalValue::Bool),
        _ => Ok(EvalValue::Null),
    }
}

/// A bare literal, exactly as the SQL lane's door takes it
/// (`emitter::expr::bare_literal`), including the sign fold for a negative
/// number. The two doors must adopt the same shapes or the batch and live
/// lanes answer one query differently.
pub(crate) fn bare_literal_of(expr: &Expr) -> Option<Cow<'_, LiteralValue>> {
    bare_literal(expr)
}

fn bare_literal(expr: &Expr) -> Option<Cow<'_, LiteralValue>> {
    match expr {
        Expr::Literal(lit) => Some(Cow::Borrowed(lit)),
        Expr::Unary {
            op: UnaryOp::Neg,
            operand,
        } => match &operand.node {
            Expr::Literal(LiteralValue::Int(n)) => {
                Some(Cow::Owned(LiteralValue::Int(n.checked_neg()?)))
            }
            Expr::Literal(LiteralValue::Float(f)) => Some(Cow::Owned(LiteralValue::Float(
                FloatLiteral::new(-f.value(), format!("-{}", f.text())),
            ))),
            _ => None,
        },
        _ => None,
    }
}

/// Bind a field reference to the key the row actually carries, the way
/// `DuckDB` binds a column reference: an exact match wins, and any
/// ASCII-case-insensitive match binds otherwise.
///
/// Both halves are load-bearing. Ingest ASCII-folds every key it writes, so
/// `where Status>400` over a stored `status` finds its value only
/// case-insensitively, but `rename status as St` gives the row an `St` key,
/// and a fold-only lookup would miss it.
///
/// A residual ambiguity (several case-variants, none exact) picks the
/// lexicographically-first variant, the same tie-break ingest's own
/// fold-collision rule uses.
pub fn bind_event_key<'e>(event: &'e Row, name: &str) -> Option<&'e str> {
    if let Some((key, _)) = event.get_key_value(name) {
        return Some(key.as_str());
    }
    event
        .keys()
        .filter(|k| k.eq_ignore_ascii_case(name))
        .min()
        .map(String::as_str)
}

/// Binding, not non-nullness, decides which key is read: a bound key
/// holding JSON null is that field's own NULL (UNKNOWN), never a reason to
/// read a differently-cased sibling.
fn pinned_event_value<'e>(event: &'e Row, name: &str) -> Option<&'e EvalValue> {
    let key = bind_event_key(event, name)?;
    event.get(key).filter(|v| !matches!(v, EvalValue::Null))
}

fn truth_to_eval(truth: Option<bool>) -> EvalValue {
    truth.map_or(EvalValue::Null, EvalValue::Bool)
}

fn subject_value(
    subject: &PinnedSubject<'_>,
    event: &Row,
    pins: &PinScope,
    patterns: &PatternTable,
    ctx: &EvalContext,
) -> Result<Option<serde_json::Value>, PatternError> {
    let cell = match subject {
        PinnedSubject::Field(name) => match pinned_event_value(event, name) {
            Some(cell) => cell.clone(),
            None => return Ok(None),
        },
        PinnedSubject::Call(call) => eval_expr_with_pins(call, event, pins, patterns, ctx)?,
    };
    if matches!(cell, EvalValue::Null) {
        return Ok(None);
    }
    Ok(Some(pin_read(&cell)))
}

/// A non-finite double projects as its `DuckDB` text (`inf`, `-inf`, `nan`,
/// `-nan`) rather than vanishing: JSON cannot spell one, and a `null` there
/// makes a DOUBLE-pinned `| where d > 1` answer UNKNOWN over a stored
/// infinity the batch query compares happily.
fn pin_read(cell: &EvalValue) -> serde_json::Value {
    match cell {
        EvalValue::Float(f) if !f.is_finite() => {
            serde_json::Value::String(crate::compare::canonical_double_text(*f))
        }
        // The raw `Number` the JSON row handed the matcher, digits
        // intact — `pin_match` reads a `u64` exactly, and a rounded
        // double would make a pinned comparison answer for the wrong id.
        EvalValue::UInt(n) => serde_json::Value::Number((*n).into()),
        // An instant reaches the matcher as the text `DuckDB` casts it
        // to, which for an infinity is a word — one both
        // `literal_timestamp` and `conformed_timestamp` read back, so a
        // TIMESTAMP-pinned comparison over one answers rather than
        // nulling.
        EvalValue::Timestamp(instant) => serde_json::Value::String(instant.cast_text()),
        other => serde_json::Value::from(other.clone()),
    }
}

/// A cell as the column its pin conforms it into holds it: the reading a
/// pinned `| where` takes on the live lane, as a cell of the pin's type.
///
/// `None` keeps the cell as sent, for a cell with no reading (the
/// conform's round-trip guard stores NULL for `"1.5"` under BIGINT, so
/// the value lives only in `_raw`) and for the pins with no scalar
/// reading of their own type: VARCHAR has no conform, and a SEVERITY
/// reading is the bare `SeverityNumber`, which would show the sender's
/// `"error"` as `17` on a wire that carries no severity columns.
#[must_use]
pub fn conformed_cell(cell: &EvalValue, pin: crate::schema::CanonicalType) -> Option<EvalValue> {
    if matches!(cell, EvalValue::Null) {
        return None;
    }
    pin_match::conformed_cell(&pin_read(cell), pin)
}

/// There is deliberately no empty-scope fast path in front of the
/// classifier: a pin-declaring call carries the function's pin, and
/// `sev()` has to bind identically over a corpus with no catalog at all
/// (embedded `--data`) or the two lanes would split there.
fn try_pinned_comparison(
    lhs: &Spanned<Expr>,
    op: BinaryOp,
    rhs: &Spanned<Expr>,
    event: &Row,
    pins: &PinScope,
    patterns: &PatternTable,
    ctx: &EvalContext,
) -> Result<Option<EvalValue>, PatternError> {
    let filter_op = match op {
        BinaryOp::Eq => FilterOp::Eq,
        BinaryOp::Ne => FilterOp::Ne,
        BinaryOp::Gt => FilterOp::Gt,
        BinaryOp::Gte => FilterOp::Gte,
        BinaryOp::Lt => FilterOp::Lt,
        BinaryOp::Lte => FilterOp::Lte,
        _ => return Ok(None),
    };
    let resolved = match (pins.subject_pin(lhs), pins.subject_pin(rhs)) {
        (Some((subject, pin)), _) => {
            bare_literal(&rhs.node).map(|literal| (subject, pin, filter_op, literal))
        }
        (None, Some((subject, pin))) => {
            let flipped = match filter_op {
                FilterOp::Gt => FilterOp::Lt,
                FilterOp::Gte => FilterOp::Lte,
                FilterOp::Lt => FilterOp::Gt,
                FilterOp::Lte => FilterOp::Gte,
                other => other,
            };
            bare_literal(&lhs.node).map(|literal| (subject, pin, flipped, literal))
        }
        (None, None) => None,
    };
    let Some((subject, pin, filter_op, literal)) = resolved else {
        return Ok(None);
    };
    // A literal the rule table refuses (an unknown severity token) cannot
    // reach here: `stream::compile_stream_plan`, which every SSE stage and
    // every `rust_stages` batch tail is compiled through, rejects it.
    let Some(form) = crate::compare::compare_form_bound(Some(pin), filter_op, &literal)
        .ok()
        .flatten()
    else {
        return Ok(None);
    };
    // Native(String) is the VARCHAR pin's lexical rule. Any other Native form
    // is degenerate under a pin and falls through to literal-driven
    // evaluation, mirroring the SQL side's fall-through.
    match &form {
        crate::compare::CompareForm::Native(SqlValue::String(_)) => {}
        crate::compare::CompareForm::Native(_) => return Ok(None),
        _ => {}
    }
    let coerced = pin_match::coerce_form(form);
    let compare_op = pin_match::CompareOp::from_filter(filter_op);
    let Some(value) = subject_value(&subject, event, pins, patterns, ctx)? else {
        return Ok(Some(EvalValue::Null));
    };
    Ok(Some(truth_to_eval(pin_match::compare_values(
        &value,
        compare_op,
        &coerced,
        NullReadPolicy::Unknown,
    ))))
}

fn try_pinned_in_list(
    target: &Spanned<Expr>,
    list: &[Spanned<Expr>],
    event: &Row,
    pins: &PinScope,
    patterns: &PatternTable,
    ctx: &EvalContext,
) -> Result<Option<EvalValue>, PatternError> {
    let Some((subject, pin)) = pins.subject_pin(target) else {
        return Ok(None);
    };
    let forms: Option<Vec<crate::compare::CompareForm>> = list
        .iter()
        .map(|item| {
            let element = bare_literal(&item.node)?;
            crate::compare::compare_form_bound(Some(pin), FilterOp::Eq, &element)
                .ok()
                .flatten()
        })
        .collect();
    let Some(forms) = forms else {
        return Ok(None);
    };
    let Some(value) = subject_value(&subject, event, pins, patterns, ctx)? else {
        return Ok(Some(EvalValue::Null));
    };
    let truth = pin_match::or_any(forms.into_iter().map(|form| {
        pin_match::compare_values(
            &value,
            pin_match::CompareOp::Eq,
            &pin_match::coerce_form(form),
            NullReadPolicy::Unknown,
        )
    }));
    Ok(Some(truth_to_eval(truth)))
}

fn eval_literal(lit: &LiteralValue) -> EvalValue {
    match lit {
        LiteralValue::Null => EvalValue::Null,
        LiteralValue::Bool(b) => EvalValue::Bool(*b),
        LiteralValue::Int(n) => EvalValue::Int(*n),
        LiteralValue::Float(n) => EvalValue::Float(n.value()),
        LiteralValue::String(s) => EvalValue::Str(s.clone()),
    }
}

fn eval_binary(lhs: &EvalValue, op: BinaryOp, rhs: &EvalValue) -> EvalValue {
    match op {
        BinaryOp::And => return eval_and(lhs, rhs),
        BinaryOp::Or => return eval_or(lhs, rhs),
        _ => {}
    }

    if matches!(lhs, EvalValue::Null) || matches!(rhs, EvalValue::Null) {
        return EvalValue::Null;
    }

    match op {
        BinaryOp::Add => eval_arithmetic(lhs, rhs, i64::checked_add, |a, b| a + b),
        BinaryOp::Sub => eval_arithmetic(lhs, rhs, i64::checked_sub, |a, b| a - b),
        BinaryOp::Mul => eval_arithmetic(lhs, rhs, i64::checked_mul, |a, b| a * b),
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
        BinaryOp::Matches | BinaryOp::Like | BinaryOp::ILike => {
            unreachable!("a pattern operator is evaluated by eval_pattern")
        }
        BinaryOp::And | BinaryOp::Or => unreachable!(),
    }
}

fn eval_and(lhs: &EvalValue, rhs: &EvalValue) -> EvalValue {
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

/// `int_op` is checked and an overflow is `Null`. The DOUBLE arm is
/// deliberately unchecked: `DuckDB` saturates a DOUBLE overflow to infinity
/// rather than erroring, so IEEE is the mirror there.
fn eval_arithmetic(
    lhs: &EvalValue,
    rhs: &EvalValue,
    int_op: impl FnOnce(i64, i64) -> Option<i64>,
    float_op: impl FnOnce(f64, f64) -> f64,
) -> EvalValue {
    match (lhs, rhs) {
        (EvalValue::Int(a), EvalValue::Int(b)) => {
            int_op(*a, *b).map_or(EvalValue::Null, EvalValue::Int)
        }
        _ => match (lhs.as_f64(), rhs.as_f64()) {
            (Some(a), Some(b)) => EvalValue::Float(float_op(a, b)),
            _ => EvalValue::Null,
        },
    }
}

/// `DuckDB`'s `/` has no integer form: `5 / 2` is `2.5` and both
/// operands go through DOUBLE, so a dividend above 2^53 comes back
/// rounded (probed: `integer_division_is_true_division_through_double`).
/// Division by zero is IEEE — `±inf`, or NaN for `0 / 0` — and
/// `i64::MIN / -1` is an ordinary value rather than the overflow the
/// same pair raises under `%`.
fn eval_div(lhs: &EvalValue, rhs: &EvalValue) -> EvalValue {
    match (lhs.as_f64(), rhs.as_f64()) {
        (Some(a), Some(b)) => EvalValue::Float(a / b),
        _ => EvalValue::Null,
    }
}

/// All-integer stays integral and `checked_rem` covers both shapes
/// `DuckDB` refuses to answer with a number: `x % 0` is NULL, and
/// `i64::MIN % -1` is an overflow error (probed:
/// `division_by_zero_is_an_ieee_special_and_integer_modulo_by_zero_is_null`,
/// `integer_arithmetic_overflow_is_an_error_never_a_wrap`) — so both land
/// on the same NULL. A DOUBLE operand takes the IEEE path instead, where
/// `5 % 0.0` is NaN rather than NULL; Rust's `%` is the truncated
/// remainder `DuckDB` computes, sign following the dividend.
fn eval_mod(lhs: &EvalValue, rhs: &EvalValue) -> EvalValue {
    match (lhs, rhs) {
        (EvalValue::Int(a), EvalValue::Int(b)) => {
            a.checked_rem(*b).map_or(EvalValue::Null, EvalValue::Int)
        }
        _ => match (lhs.as_f64(), rhs.as_f64()) {
            (Some(a), Some(b)) => EvalValue::Float(a % b),
            _ => EvalValue::Null,
        },
    }
}

fn eval_eq(lhs: &EvalValue, rhs: &EvalValue) -> EvalValue {
    match (lhs, rhs) {
        (EvalValue::Int(a), EvalValue::Int(b)) => EvalValue::Bool(a == b),
        (EvalValue::Bool(a), EvalValue::Bool(b)) => EvalValue::Bool(a == b),
        (EvalValue::Str(a), EvalValue::Str(b)) => EvalValue::Bool(a == b),
        // A text with no reading is NULL, never a lexical comparison: a
        // lexical fallback invents an ordering `DuckDB` does not have and
        // inverts under `NOT` (ADR-0017 §2).
        (EvalValue::Timestamp(a), EvalValue::Timestamp(b)) => EvalValue::Bool(a == b),
        (EvalValue::Timestamp(a), EvalValue::Str(text))
        | (EvalValue::Str(text), EvalValue::Timestamp(a)) => {
            compare::literal_timestamp(text).map_or(EvalValue::Null, |b| EvalValue::Bool(*a == b))
        }
        _ => match (lhs.as_f64(), rhs.as_f64()) {
            (Some(a), Some(b)) => {
                EvalValue::Bool(compare::double_total_cmp(a, b) == std::cmp::Ordering::Equal)
            }
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
        // `Instant`'s derived order is DuckDB's TIMESTAMP order, so the
        // infinities sort where the engine sorts them.
        (EvalValue::Timestamp(a), EvalValue::Timestamp(b)) => Some(a.cmp(b)),
        (EvalValue::Timestamp(a), EvalValue::Str(text)) => {
            compare::literal_timestamp(text).map(|b| a.cmp(&b))
        }
        (EvalValue::Str(text), EvalValue::Timestamp(b)) => {
            compare::literal_timestamp(text).map(|a| a.cmp(b))
        }
        _ => match (lhs.as_f64(), rhs.as_f64()) {
            (Some(a), Some(b)) => Some(compare::double_total_cmp(a, b)),
            _ => None,
        },
    };
    ordering.map_or(EvalValue::Null, |o| EvalValue::Bool(pred(o)))
}

fn eval_unary(op: UnaryOp, operand: EvalValue) -> EvalValue {
    match op {
        UnaryOp::Not => match operand {
            EvalValue::Null => EvalValue::Null,
            EvalValue::Bool(b) => EvalValue::Bool(!b),
            other => EvalValue::Bool(!other.is_truthy()),
        },
        UnaryOp::Neg => match operand {
            EvalValue::Int(n) => n.checked_neg().map_or(EvalValue::Null, EvalValue::Int),
            #[allow(clippy::cast_precision_loss)]
            EvalValue::UInt(n) => EvalValue::Float(-(n as f64)),
            EvalValue::Float(n) => EvalValue::Float(-n),
            _ => EvalValue::Null,
        },
    }
}

/// Returns `Some(value)` if the function name is known and handled,
/// `None` if it is unknown (distinct from `Some(Null)`, which means the
/// function evaluated to SQL NULL). The call site maps `None` to
/// `EvalValue::Null`; the `Option` wrapper exists so the coverage test
/// can detect un-implemented scalars.
#[allow(clippy::too_many_lines)]
fn eval_scalar_fn(name: &str, args: &[EvalValue], ctx: &EvalContext) -> Option<EvalValue> {
    let v = match name {
        "lower" => unary_str(args, str::to_lowercase),
        "upper" => unary_str(args, str::to_uppercase),
        #[allow(clippy::cast_possible_wrap)]
        "length" | "len" => args.first().map_or(EvalValue::Null, |v| match v {
            // DuckDB's LENGTH() returns the character count, not the
            // byte count.
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
            let result: String = args.iter().filter_map(EvalValue::as_str_repr).collect();
            EvalValue::Str(result)
        }

        "abs" => args.first().map_or(EvalValue::Null, |v| match v {
            EvalValue::Int(n) => n.checked_abs().map_or(EvalValue::Null, EvalValue::Int),
            #[allow(clippy::cast_precision_loss)]
            EvalValue::UInt(n) => EvalValue::Float(*n as f64),
            EvalValue::Float(n) => EvalValue::Float(n.abs()),
            _ => EvalValue::Null,
        }),
        "ceil" | "ceiling" => eval_ceil(args),
        "floor" => eval_floor(args),
        "round" => eval_round(args),

        "if" => {
            if args.len() != 3 {
                return Some(EvalValue::Null);
            }
            match read_condition(&args[0]) {
                ConditionRead::True => args[1].clone(),
                ConditionRead::NotTaken => args[2].clone(),
                ConditionRead::Unreadable => EvalValue::Null,
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
        // An integer is BIGINT, not INTEGER: the emitter binds every DSL
        // literal as a parameter, and a bound `i64` arrives as BIGINT.
        // Two spellings stay divergent on purpose: the NULL type and
        // lists, pinned in `trawl-core/tests/scalar_parity.rs`.
        "typeof" => args.first().map_or(EvalValue::Null, |v| {
            EvalValue::Str(
                match v {
                    EvalValue::Null => "NULL",
                    EvalValue::Bool(_) => "BOOLEAN",
                    EvalValue::Int(_) => "BIGINT",
                    EvalValue::UInt(_) | EvalValue::Float(_) => "DOUBLE",
                    EvalValue::Str(_) => "VARCHAR",
                    EvalValue::Array(_) => "ARRAY",
                    EvalValue::Timestamp(_) => "TIMESTAMP",
                }
                .to_string(),
            )
        }),
        "now" => ctx.now_value(),
        "case" => {
            let pairs = args.len() / 2;
            for i in 0..pairs {
                match read_condition(&args[i * 2]) {
                    ConditionRead::True => return Some(args[i * 2 + 1].clone()),
                    ConditionRead::NotTaken => {}
                    // An unreadable condition does not mean "this arm
                    // does not match": `DuckDB` errors, so the whole call
                    // is NULL. Read in arm order, so an earlier match
                    // returns before this one is looked at, which is what
                    // `DuckDB`'s per-arm short circuit does (probed:
                    // `a_case_reads_its_arms_in_order_and_stops_at_the_first_true`).
                    ConditionRead::Unreadable => return Some(EvalValue::Null),
                }
            }
            if args.len() % 2 == 1 {
                args[args.len() - 1].clone()
            } else {
                EvalValue::Null
            }
        }
        "json" | "json_extract_string" => eval_json_extract_string(args),
        "json_extract" => eval_json_extract(args),
        "json_valid" => args.first().map_or(EvalValue::Null, |v| match v {
            EvalValue::Str(s) => {
                EvalValue::Bool(serde_json::from_str::<serde_json::Value>(s).is_ok())
            }
            _ => EvalValue::Null,
        }),
        "json_keys" => args.first().map_or(EvalValue::Null, |v| match v {
            EvalValue::Str(s) => {
                serde_json::from_str::<serde_json::Value>(s)
                    .ok()
                    .map_or(EvalValue::Null, |val| {
                        EvalValue::Array(val.as_object().map_or_else(Vec::new, |obj| {
                            obj.keys().map(|k| EvalValue::Str(k.clone())).collect()
                        }))
                    })
            }
            _ => EvalValue::Null,
        }),
        #[allow(clippy::cast_possible_wrap)]
        "json_array_length" => args.first().map_or(EvalValue::Null, |v| match v {
            EvalValue::Str(s) => serde_json::from_str::<serde_json::Value>(s)
                .ok()
                .map_or(EvalValue::Null, |val| {
                    EvalValue::Int(val.as_array().map_or(0, |arr| arr.len() as i64))
                }),
            _ => EvalValue::Null,
        }),

        // An unreadable value is `Null`, never `None`: `sev()` is a known
        // function whatever it is handed.
        "sev" => {
            let dialect = match args.get(1) {
                Some(EvalValue::Str(token)) => crate::severity::Dialect::from_token(token),
                None => Some(crate::severity::Dialect::Otel),
                Some(_) => None,
            };
            let reading = dialect.and_then(|dialect| match args.first() {
                Some(EvalValue::Str(text)) => crate::severity::reading_text(text, dialect),
                Some(EvalValue::Int(n)) => crate::severity::reading_number(*n, dialect),
                _ => None,
            });
            reading.map_or(EvalValue::Null, |n| EvalValue::Int(i64::from(n)))
        }

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

/// The shared check refuses an out-of-grammar path before either lane
/// runs; one that reaches eval anyway (fuzz, an internal caller) selects
/// nothing, never the root.
fn json_select<R>(doc: &str, path: &str, read: impl FnOnce(&serde_json::Value) -> R) -> Option<R> {
    let steps = crate::json_path::parse(path).ok()?;
    let value = serde_json::from_str::<serde_json::Value>(doc).ok()?;
    crate::json_path::select(&steps, &value).map(read)
}

fn eval_json_extract_string(args: &[EvalValue]) -> EvalValue {
    if args.len() != 2 {
        return EvalValue::Null;
    }
    match (&args[0], &args[1]) {
        (EvalValue::Str(json_str), EvalValue::Str(path)) => {
            json_select(json_str, path, |v| match v {
                serde_json::Value::Null => EvalValue::Null,
                serde_json::Value::String(s) => EvalValue::Str(s.clone()),
                other => EvalValue::Str(other.to_string()),
            })
            .unwrap_or(EvalValue::Null)
        }
        _ => EvalValue::Null,
    }
}

/// `DuckDB`'s `json_extract` yields JSON, not a decoded scalar: a string
/// keeps its quotes (`"x"`), a number is its own text, and an
/// array/object is compact JSON. A missing path is SQL NULL; a path that
/// finds a JSON `null` is the text `null`, which is a value.
fn eval_json_extract(args: &[EvalValue]) -> EvalValue {
    if args.len() != 2 {
        return EvalValue::Null;
    }
    match (&args[0], &args[1]) {
        (EvalValue::Str(json_str), EvalValue::Str(path)) => {
            json_select(json_str, path, duckdb_json_text).map_or(EvalValue::Null, EvalValue::Str)
        }
        _ => EvalValue::Null,
    }
}

/// `serde_json`'s own rendering is `DuckDB`'s text for every shape but one:
/// it spells a positive exponent `e+300` where `DuckDB` spells it `e300`.
///
/// The pass is string-aware: a JSON string may contain the bytes `e+`
/// (`{"note":"cost e+300"}`), and rewriting those would corrupt the
/// document.
#[must_use]
pub fn duckdb_json_text(value: &serde_json::Value) -> String {
    let rendered = value.to_string();
    if !rendered.contains("e+") && !rendered.contains("E+") {
        return rendered;
    }
    let mut out = String::with_capacity(rendered.len());
    let mut chars = rendered.chars();
    let mut in_string = false;
    while let Some(ch) = chars.next() {
        out.push(ch);
        if in_string {
            match ch {
                // The escaped character is consumed whole, so a `\"`
                // cannot be read as the end of the string.
                '\\' => {
                    if let Some(escaped) = chars.next() {
                        out.push(escaped);
                    }
                }
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        if ch == '"' {
            in_string = true;
        } else if matches!(ch, 'e' | 'E') && chars.as_str().starts_with('+') {
            chars.next();
        }
    }
    out
}

enum ConditionRead {
    True,
    NotTaken,
    Unreadable,
}

/// Strings go through the one owner of that cast vocabulary
/// ([`crate::compare::try_cast_boolean`]) — closed, case-insensitive and
/// untrimmed, so `' true '` has no reading. Numbers read as `!= 0`,
/// which makes both zeros false and NaN true; a timestamp or a list has
/// no cast to BOOLEAN at all.
fn read_condition(value: &EvalValue) -> ConditionRead {
    let taken = match value {
        EvalValue::Bool(b) => *b,
        EvalValue::Null => return ConditionRead::NotTaken,
        EvalValue::Int(n) => *n != 0,
        EvalValue::UInt(n) => *n != 0,
        #[allow(clippy::float_cmp)]
        EvalValue::Float(n) => *n != 0.0,
        EvalValue::Str(s) => match crate::compare::try_cast_boolean(s) {
            Some(b) => b,
            None => return ConditionRead::Unreadable,
        },
        EvalValue::Timestamp(_) | EvalValue::Array(_) => return ConditionRead::Unreadable,
    };
    if taken {
        ConditionRead::True
    } else {
        ConditionRead::NotTaken
    }
}

fn unary_str(args: &[EvalValue], f: impl FnOnce(&str) -> String) -> EvalValue {
    args.first().map_or(EvalValue::Null, |v| match v {
        EvalValue::Str(s) => EvalValue::Str(f(s)),
        _ => EvalValue::Null,
    })
}

/// `CEIL` returns DOUBLE over a BIGINT argument as well as over a DOUBLE
/// one, and `ceil(-0.5)` is `-0.0`, a value no integer can carry.
#[allow(clippy::cast_precision_loss)]
fn eval_ceil(args: &[EvalValue]) -> EvalValue {
    args.first().map_or(EvalValue::Null, |v| match v {
        EvalValue::Int(n) => EvalValue::Float(*n as f64),
        EvalValue::UInt(n) => EvalValue::Float(*n as f64),
        EvalValue::Float(n) => EvalValue::Float(n.ceil()),
        _ => EvalValue::Null,
    })
}

#[allow(clippy::cast_precision_loss)]
fn eval_floor(args: &[EvalValue]) -> EvalValue {
    args.first().map_or(EvalValue::Null, |v| match v {
        EvalValue::Int(n) => EvalValue::Float(*n as f64),
        EvalValue::UInt(n) => EvalValue::Float(*n as f64),
        EvalValue::Float(n) => EvalValue::Float(n.floor()),
        _ => EvalValue::Null,
    })
}

/// `substr(s, start [, len])` — mirrors `DuckDB`'s `SUBSTRING` semantics.
///
/// 1-based and character-based (multibyte UTF-8 counts as one char), not
/// `PostgreSQL` semantics. Negative `start` counts from the end (`-1` = last
/// char); `start == 0` stays a position before the first char, not clamped
/// to 1. Negative `len` is a real leftward window exclusive of `start`. The
/// resulting inclusive 1-based window `[lo, hi]` is clamped to `[1, char_len]`;
/// `lo > hi` yields the empty string. NULL propagates from any arg.
///
/// All bound arithmetic is done in `i64` (bounds can legitimately go negative
/// or exceed the string length before clamping) — do not cast to `usize` until
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

    let start = if start < 0 {
        n.saturating_add(start).saturating_add(1)
    } else {
        start
    };

    let (lo, hi) = match args.get(2) {
        None => (start, n),
        Some(EvalValue::Int(len)) => match len.cmp(&0) {
            std::cmp::Ordering::Equal => return EvalValue::Str(String::new()),
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

/// `ROUND` over a BIGINT returns BIGINT while `CEIL`/`FLOOR` widen to
/// DOUBLE.
#[allow(clippy::cast_possible_truncation)]
fn eval_round(args: &[EvalValue]) -> EvalValue {
    if args.is_empty() || args.len() > 2 {
        return EvalValue::Null;
    }
    let val = match &args[0] {
        EvalValue::Int(n) => return EvalValue::Int(*n),
        #[allow(clippy::cast_precision_loss)]
        EvalValue::UInt(n) => *n as f64,
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
        EvalValue::Float(val.round())
    } else {
        let factor = 10_f64.powi(precision as i32);
        EvalValue::Float((val * factor).round() / factor)
    }
}

/// `tonumber(x)` — mirrors `TRY_CAST(x AS DOUBLE)`.
#[allow(clippy::cast_precision_loss)]
fn eval_tonumber(args: &[EvalValue]) -> EvalValue {
    match args.first() {
        Some(EvalValue::Int(n)) => EvalValue::Float(*n as f64),
        Some(EvalValue::UInt(n)) => EvalValue::Float(*n as f64),
        Some(EvalValue::Float(n)) => EvalValue::Float(*n),
        // A boolean has a DOUBLE reading — `TRY_CAST(true AS DOUBLE)` is
        // 1.0, not NULL (probed: `a_boolean_casts_to_double_as_one_and_zero`).
        Some(EvalValue::Bool(b)) => EvalValue::Float(if *b { 1.0 } else { 0.0 }),
        // One owner for the cast domain — whitespace trimming and `_`
        // digit separators alike (see `compare::try_cast_double`), so the
        // scalar and the DOUBLE pin's pattern text can't drift.
        Some(EvalValue::Str(s)) => {
            crate::compare::try_cast_double(s).map_or(EvalValue::Null, EvalValue::Float)
        }
        None | Some(_) => EvalValue::Null,
    }
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
    let Some(ts) = args[1].as_finite() else {
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
        // The one probe-pinned reading, owned by `compare`: summing
        // seconds and a fraction here would round twice.
        "epoch" => compare::Instant::At(ts)
            .epoch_seconds()
            .map_or(EvalValue::Null, EvalValue::Float),
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
    // An infinity truncates to itself, for every unit (probed) — so the
    // instant is read whole here and only the finite arm truncates.
    let ts = match args[1].as_instant() {
        Some(compare::Instant::At(at)) => at,
        Some(infinite) => {
            return if crate::emitter::DATE_UNITS.contains(&unit.to_lowercase().as_str()) {
                EvalValue::Timestamp(infinite)
            } else {
                EvalValue::Null
            };
        }
        None => return EvalValue::Null,
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
        "hour" => Some(trunc_to_hour(ts)),
        "minute" => Some(trunc_to_minute(ts)),
        "second" => Some(trunc_to_second(ts)),
        _ => None,
    };
    truncated.map_or(EvalValue::Null, |at| {
        EvalValue::Timestamp(compare::Instant::At(at))
    })
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
    let Some(start) = args[1].as_finite() else {
        return EvalValue::Null;
    };
    let Some(end) = args[2].as_finite() else {
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
            // zero, i.e. DATE_DIFF('day', start, end) / 7), not by
            // ISO-week-boundary crossing — confirmed by the parity tests.
            let start_trunc = start.date().and_hms_opt(0, 0, 0).unwrap_or(start);
            let end_trunc = end.date().and_hms_opt(0, 0, 0).unwrap_or(end);
            let days = end_trunc.signed_duration_since(start_trunc).num_days();
            days / 7
        }
        "day" => {
            let start_trunc = start.date().and_hms_opt(0, 0, 0).unwrap_or(start);
            let end_trunc = end.date().and_hms_opt(0, 0, 0).unwrap_or(end);
            let diff = end_trunc.signed_duration_since(start_trunc);
            diff.num_days()
        }
        "hour" => {
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

/// A bare `%f` is a six-digit microsecond field to `DuckDB` and an
/// unscaled nanosecond count to chrono. chrono's fixed-width `%6f` is the
/// same field `DuckDB` writes, so the translation is `%f` → `%6f` and
/// nothing else.
///
/// `%%` is an escaped percent, not a specifier: the `f` in `%%f` is a
/// literal letter and must survive untouched, which is why this walks
/// the format instead of replacing text.
fn duckdb_strftime_format(fmt: &str) -> Cow<'_, str> {
    if !fmt.contains("%f") {
        return Cow::Borrowed(fmt);
    }
    let mut out = String::with_capacity(fmt.len() + 1);
    let mut chars = fmt.chars();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('f') => out.push_str("%6f"),
            Some('%') => out.push_str("%%"),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    Cow::Owned(out)
}

/// `strftime(ts, fmt)` — DSL arg order is (ts, fmt); mirrors `STRFTIME(fmt, ts)`.
fn eval_strftime(args: &[EvalValue]) -> EvalValue {
    if args.len() != 2 {
        return EvalValue::Null;
    }
    // An infinity renders as the word for every format (probed), so the
    // format is never applied to one.
    let ts = match args[0].as_instant() {
        Some(compare::Instant::At(at)) => at,
        Some(infinite) => return EvalValue::Str(infinite.cast_text()),
        None => return EvalValue::Null,
    };
    let EvalValue::Str(fmt) = &args[1] else {
        return EvalValue::Null;
    };
    let fmt = duckdb_strftime_format(fmt);
    let fmt = fmt.as_ref();
    // chrono turns an invalid specifier (e.g. `%Q`) into `Item::Error`, whose
    // `Display` returns `fmt::Error`, so `ts.format(fmt).to_string()` would
    // panic. Invalid format literals are rejected at emit time, so this guard
    // covers only a non-literal (field-ref) format.
    if chrono::format::StrftimeItems::new(fmt)
        .any(|item| matches!(item, chrono::format::Item::Error))
    {
        return EvalValue::Null;
    }
    EvalValue::Str(ts.format(fmt).to_string())
}

/// `DuckDB`'s `STRPTIME` fills the components a format omits from a
/// `1900-01-01 00:00:00` base (see `resolve_date`/`resolve_time`).
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
    let fmt = duckdb_strftime_format(fmt);
    let fmt = fmt.as_ref();
    let mut parsed = chrono::format::Parsed::new();
    if chrono::format::parse(&mut parsed, s, chrono::format::StrftimeItems::new(fmt)).is_err() {
        return EvalValue::Null;
    }
    if let Ok(dt) = parsed.to_naive_datetime_with_offset(0) {
        return EvalValue::Timestamp(compare::Instant::At(dt));
    }
    match (resolve_date(&mut parsed), resolve_time(&mut parsed)) {
        (Some(date), Some(time)) => EvalValue::Timestamp(compare::Instant::At(date.and_time(time))),
        _ => EvalValue::Null,
    }
}

/// chrono is tried first so derived fields (ISO week, ordinal `%j`) win; the
/// base defaults are then injected most-significant first, retrying after
/// each. `set_*` no-ops when the field already holds a value.
fn resolve_date(parsed: &mut chrono::format::Parsed) -> Option<NaiveDate> {
    if let Ok(date) = parsed.to_naive_date() {
        return Some(date);
    }
    let _ = parsed.set_year(1900);
    if let Ok(date) = parsed.to_naive_date() {
        return Some(date);
    }
    let _ = parsed.set_month(1);
    if let Ok(date) = parsed.to_naive_date() {
        return Some(date);
    }
    let _ = parsed.set_day(1);
    parsed.to_naive_date().ok()
}

/// `%I` without `%p` does not null: the `set_hour(0)` default supplies the
/// missing am/pm half, so it resolves to the 24-hour reading (matching
/// `DuckDB`).
fn resolve_time(parsed: &mut chrono::format::Parsed) -> Option<NaiveTime> {
    if let Ok(time) = parsed.to_naive_time() {
        return Some(time);
    }
    let _ = parsed.set_hour(0);
    if let Ok(time) = parsed.to_naive_time() {
        return Some(time);
    }
    let _ = parsed.set_minute(0);
    if let Ok(time) = parsed.to_naive_time() {
        return Some(time);
    }
    let _ = parsed.set_second(0);
    parsed.to_naive_time().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{BinaryOp, Expr, LiteralValue, Spanned, UnaryOp};
    use serde_json::Map;
    use serde_json::json;

    fn span<T>(node: T) -> Spanned<T> {
        Spanned { node, span: 0..0 }
    }

    fn ctx() -> EvalContext {
        EvalContext::at(
            chrono::DateTime::parse_from_rfc3339("2026-08-24T12:00:00Z")
                .expect("literal is RFC 3339")
                .with_timezone(&chrono::Utc),
        )
    }

    fn prepared(expr: &Spanned<Expr>) -> PatternTable {
        PatternTable::prepare([expr], &mut crate::pattern::table::test_ledger())
            .expect("every pattern operand prepares")
    }

    fn eval_expr(expr: &Spanned<Expr>, event: &Row, ctx: &EvalContext) -> EvalValue {
        super::eval_expr(expr, event, &prepared(expr), ctx).expect("every pattern is prepared")
    }

    #[test]
    fn conformed_cell_is_the_pinned_columns_cell() {
        use crate::schema::CanonicalType;
        let text = |s: &str| EvalValue::Str(s.to_owned());

        assert_eq!(
            conformed_cell(&text("0404"), CanonicalType::BigInt),
            Some(EvalValue::Int(404))
        );
        assert_eq!(
            conformed_cell(&EvalValue::Int(9), CanonicalType::BigInt),
            Some(EvalValue::Int(9))
        );
        assert_eq!(conformed_cell(&text("1.5"), CanonicalType::BigInt), None);
        assert_eq!(conformed_cell(&text("n/a"), CanonicalType::BigInt), None);
        assert_eq!(
            conformed_cell(&EvalValue::UInt(u64::MAX), CanonicalType::BigInt),
            None
        );

        assert_eq!(
            conformed_cell(&text("200"), CanonicalType::Double),
            Some(EvalValue::Float(200.0))
        );
        assert_eq!(
            conformed_cell(&text("true"), CanonicalType::Boolean),
            Some(EvalValue::Bool(true))
        );
        assert_eq!(conformed_cell(&text("TRUE"), CanonicalType::Boolean), None);

        let at = chrono::NaiveDate::from_ymd_opt(2026, 1, 15)
            .unwrap()
            .and_hms_opt(3, 30, 0)
            .unwrap();
        assert_eq!(
            conformed_cell(&text("2026-01-15T09:00:00+05:30"), CanonicalType::Timestamp),
            Some(EvalValue::Timestamp(compare::Instant::At(at)))
        );
        assert_eq!(
            conformed_cell(&EvalValue::Int(1_768_448_000), CanonicalType::Timestamp),
            None
        );

        assert_eq!(conformed_cell(&text("200"), CanonicalType::Varchar), None);
        assert_eq!(
            conformed_cell(&text("error"), CanonicalType::Severity),
            None
        );
        assert_eq!(
            conformed_cell(&EvalValue::Null, CanonicalType::BigInt),
            None
        );
    }

    mod pinned {
        use super::*;
        use crate::parser;
        use crate::pin_scope::PinScope;
        use crate::schema::CanonicalType as CT;
        use crate::schema::FieldTypes;

        fn eval_where(dsl: &str, event: &str, pins: &[(&str, CT)]) -> EvalValue {
            let mut ft = FieldTypes::new();
            for (field, ty) in pins {
                ft.insert(field, *ty);
            }
            let query = parser::parse(dsl).expect("dsl parses");
            let cond = query
                .pipeline
                .iter()
                .find_map(|s| match &s.node {
                    crate::ast::PipeStage::Where(w) => Some(w.condition.clone()),
                    _ => None,
                })
                .expect("dsl has a where stage");
            let event: Map<String, Value> = serde_json::from_str(event).expect("valid event");
            eval_expr_with_pins(
                &cond,
                &crate::row::from_json(&event),
                &PinScope::root(&ft),
                &prepared(&cond),
                &ctx(),
            )
            .expect("every pattern is prepared")
        }

        const VARCHAR_STATUS: &[(&str, CT)] = &[("status", CT::Varchar)];
        const BIGINT_DUR: &[(&str, CT)] = &[("dur", CT::BigInt)];

        #[test]
        fn varchar_ordered_numeric_compares_in_decimal_space() {
            let dsl = "* | where status > 400";
            assert_eq!(
                eval_where(dsl, r#"{"status": "404"}"#, VARCHAR_STATUS),
                EvalValue::Bool(true)
            );
            assert_eq!(
                eval_where(dsl, r#"{"status": "200"}"#, VARCHAR_STATUS),
                EvalValue::Bool(false)
            );
            assert_eq!(
                eval_where(dsl, r#"{"status": "accepted"}"#, VARCHAR_STATUS),
                EvalValue::Null
            );
            assert_eq!(
                eval_where(
                    "* | where not (status > 400)",
                    r#"{"status": "accepted"}"#,
                    VARCHAR_STATUS
                ),
                EvalValue::Null
            );
        }

        #[test]
        fn varchar_eq_numeric_matches_text_or_reading() {
            let dsl = "* | where status == 200";
            for value in [r#""200""#, "200", "200.0", r#""0200""#] {
                assert_eq!(
                    eval_where(dsl, &format!(r#"{{"status": {value}}}"#), VARCHAR_STATUS),
                    EvalValue::Bool(true),
                    "{value}"
                );
            }
            assert_eq!(
                eval_where(dsl, r#"{"status": "accepted"}"#, VARCHAR_STATUS),
                EvalValue::Bool(false)
            );
        }

        #[test]
        fn ne_over_absent_field_is_unknown() {
            for event in ["{}", r#"{"status": null}"#] {
                assert_eq!(
                    eval_where("* | where status != 200", event, VARCHAR_STATUS),
                    EvalValue::Null,
                    "{event}"
                );
            }
            assert_eq!(
                eval_where(
                    "* | where status != 200",
                    r#"{"status": "accepted"}"#,
                    VARCHAR_STATUS
                ),
                EvalValue::Bool(true)
            );
        }

        #[test]
        fn typed_pin_unreadable_value_is_unknown_even_for_ne() {
            assert_eq!(
                eval_where("* | where dur != 1", r#"{"dur": "1.5"}"#, BIGINT_DUR),
                EvalValue::Null
            );
            assert_eq!(
                eval_where("* | where dur > 1", r#"{"dur": "1.5"}"#, BIGINT_DUR),
                EvalValue::Null
            );
            assert_eq!(
                eval_where("* | where dur > 1", r#"{"dur": "0404"}"#, BIGINT_DUR),
                EvalValue::Bool(true)
            );
        }

        #[test]
        fn reversed_operands_bind_the_field_rule() {
            assert_eq!(
                eval_where(
                    "* | where 400 < status",
                    r#"{"status": "404"}"#,
                    VARCHAR_STATUS
                ),
                EvalValue::Bool(true)
            );
            assert_eq!(
                eval_where(
                    "* | where 400 < status",
                    r#"{"status": "200"}"#,
                    VARCHAR_STATUS
                ),
                EvalValue::Bool(false)
            );
        }

        #[test]
        fn quoted_numeric_literal_binds_content() {
            assert_eq!(
                eval_where(
                    "* | where status > \"400\"",
                    r#"{"status": "404"}"#,
                    VARCHAR_STATUS
                ),
                EvalValue::Bool(true)
            );
        }

        #[test]
        fn matches_over_typed_pin_targets_canonical_text() {
            assert_eq!(
                eval_where(
                    "* | where dur matches \"^4\"",
                    r#"{"dur": 404}"#,
                    BIGINT_DUR
                ),
                EvalValue::Bool(true)
            );
            assert_eq!(
                eval_where(
                    "* | where dur matches \"^4\"",
                    r#"{"dur": 200}"#,
                    BIGINT_DUR
                ),
                EvalValue::Bool(false)
            );
            assert_eq!(
                eval_where(
                    "* | where dur matches \"^4\"",
                    r#"{"dur": "0404"}"#,
                    BIGINT_DUR
                ),
                EvalValue::Bool(true)
            );
            assert_eq!(
                eval_where(
                    "* | where dur matches \"^4\"",
                    r#"{"dur": "4.5"}"#,
                    BIGINT_DUR
                ),
                EvalValue::Null
            );
        }

        #[test]
        fn pinned_lookup_binds_the_rows_key_case_insensitively() {
            for event in [r#"{"status": "404"}"#, r#"{"Status": "404"}"#] {
                for dsl in ["* | where status > 400", "* | where StAtUs > 400"] {
                    assert_eq!(
                        eval_where(dsl, event, VARCHAR_STATUS),
                        EvalValue::Bool(true),
                        "{dsl} over {event}"
                    );
                }
            }
            assert_eq!(
                eval_where(
                    "* | where status > 400",
                    r#"{"Status": "500", "status": "200"}"#,
                    VARCHAR_STATUS
                ),
                EvalValue::Bool(false)
            );
            assert_eq!(
                eval_where(
                    "* | where status > 400",
                    r#"{"Status": "500", "status": null}"#,
                    VARCHAR_STATUS
                ),
                EvalValue::Null
            );
        }

        #[test]
        fn unpinned_lookup_binds_the_rows_key_case_insensitively() {
            assert_eq!(
                eval_where(
                    "* | where lower(Status) == \"ok\"",
                    r#"{"status": "OK"}"#,
                    &[]
                ),
                EvalValue::Bool(true)
            );
            assert_eq!(
                eval_where(
                    "* | where status == \"exact\"",
                    r#"{"Status": "variant", "status": "exact"}"#,
                    &[]
                ),
                EvalValue::Bool(true)
            );
        }

        #[test]
        fn matches_over_varchar_pin_reads_the_stored_text() {
            assert_eq!(
                eval_where(
                    "* | where status matches \"^2\"",
                    r#"{"status": 200}"#,
                    VARCHAR_STATUS
                ),
                EvalValue::Bool(true)
            );
        }

        #[test]
        fn in_list_routes_each_element_through_eq() {
            let dsl = "* | where status in (200, \"accepted\")";
            for value in [r#""200""#, "200", "200.0", r#""accepted""#] {
                assert_eq!(
                    eval_where(dsl, &format!(r#"{{"status": {value}}}"#), VARCHAR_STATUS),
                    EvalValue::Bool(true),
                    "{value}"
                );
            }
            assert_eq!(
                eval_where(dsl, r#"{"status": "404"}"#, VARCHAR_STATUS),
                EvalValue::Bool(false)
            );
            assert_eq!(eval_where(dsl, "{}", VARCHAR_STATUS), EvalValue::Null);
        }

        #[test]
        fn unpinned_field_falls_through_to_literal_driven_eval() {
            assert_eq!(
                eval_where(
                    "* | where other > 400",
                    r#"{"other": "404"}"#,
                    VARCHAR_STATUS
                ),
                EvalValue::Null
            );
            let query = parser::parse("* | where status > 400").expect("parses");
            let cond = match &query.pipeline[0].node {
                crate::ast::PipeStage::Where(w) => w.condition.clone(),
                _ => unreachable!(),
            };
            let event: Map<String, Value> = serde_json::from_str(r#"{"status": "404"}"#).unwrap();
            assert_eq!(
                eval_expr(&cond, &crate::row::from_json(&event), &ctx()),
                EvalValue::Null
            );
        }
    }

    fn lit_int(n: i64) -> Spanned<Expr> {
        span(Expr::Literal(LiteralValue::Int(n)))
    }

    fn lit_float(n: f64) -> Spanned<Expr> {
        span(Expr::Literal(LiteralValue::Float(n.into())))
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

    fn empty_event() -> Row {
        Row::new()
    }

    fn event(pairs: &Value) -> Row {
        crate::row::from_json(pairs.as_object().unwrap())
    }

    #[test]
    fn literal_int() {
        assert_eq!(
            eval_expr(&lit_int(42), &empty_event(), &ctx()),
            EvalValue::Int(42)
        );
    }

    #[test]
    fn literal_float() {
        assert_eq!(
            eval_expr(&lit_float(1.5), &empty_event(), &ctx()),
            EvalValue::Float(1.5)
        );
    }

    #[test]
    fn literal_string() {
        assert_eq!(
            eval_expr(&lit_str("hello"), &empty_event(), &ctx()),
            EvalValue::Str("hello".to_string())
        );
    }

    #[test]
    fn literal_bool() {
        assert_eq!(
            eval_expr(&lit_bool(true), &empty_event(), &ctx()),
            EvalValue::Bool(true)
        );
    }

    #[test]
    fn literal_null() {
        assert_eq!(
            eval_expr(&lit_null(), &empty_event(), &ctx()),
            EvalValue::Null
        );
    }

    #[test]
    fn field_ref_present() {
        let ev = event(&json!({"host": "web-1"}));
        assert_eq!(
            eval_expr(&field("host"), &ev, &ctx()),
            EvalValue::Str("web-1".to_string())
        );
    }

    #[test]
    fn field_ref_missing() {
        assert_eq!(
            eval_expr(&field("host"), &empty_event(), &ctx()),
            EvalValue::Null
        );
    }

    #[test]
    fn field_ref_timestamp_is_not_an_alias_for_time() {
        let ev = event(&json!({"_time": "2026-01-01T00:00:00Z"}));
        assert_eq!(
            eval_expr(&field("@timestamp"), &ev, &ctx()),
            EvalValue::Null
        );
        let ev = event(&json!({"@timestamp": "2026-01-01T00:00:00Z"}));
        assert_eq!(
            eval_expr(&field("@timestamp"), &ev, &ctx()),
            EvalValue::Str("2026-01-01T00:00:00Z".to_string())
        );
    }

    #[test]
    fn field_ref_time_alias() {
        let ev = event(&json!({"_time": "2026-01-01T00:00:00Z"}));
        assert_eq!(
            eval_expr(&field("_time"), &ev, &ctx()),
            EvalValue::Str("2026-01-01T00:00:00Z".to_string())
        );
    }

    #[test]
    fn field_ref_numeric() {
        let ev = event(&json!({"status": 200}));
        assert_eq!(
            eval_expr(&field("status"), &ev, &ctx()),
            EvalValue::Int(200)
        );
    }

    #[test]
    fn add_ints() {
        let expr = binary(lit_int(2), BinaryOp::Add, lit_int(3));
        assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Int(5));
    }

    #[test]
    fn add_int_float() {
        let expr = binary(lit_int(2), BinaryOp::Add, lit_float(1.5));
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Float(3.5)
        );
    }

    #[test]
    fn sub_ints() {
        let expr = binary(lit_int(10), BinaryOp::Sub, lit_int(3));
        assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Int(7));
    }

    #[test]
    fn mul_ints() {
        let expr = binary(lit_int(4), BinaryOp::Mul, lit_int(5));
        assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Int(20));
    }

    #[test]
    fn div_ints() {
        let expr = binary(lit_int(10), BinaryOp::Div, lit_int(4));
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Float(2.5)
        );
    }

    #[test]
    fn div_by_zero_int() {
        for (dividend, want) in [(10_i64, f64::INFINITY), (-10_i64, f64::NEG_INFINITY)] {
            let expr = binary(lit_int(dividend), BinaryOp::Div, lit_int(0));
            assert_eq!(
                eval_expr(&expr, &empty_event(), &ctx()),
                EvalValue::Float(want)
            );
        }
        let expr = binary(lit_int(0), BinaryOp::Div, lit_int(0));
        let EvalValue::Float(value) = eval_expr(&expr, &empty_event(), &ctx()) else {
            panic!("0 / 0 must be a float");
        };
        assert!(value.is_nan());
    }

    #[test]
    fn int_arithmetic_overflow_is_null() {
        for (lhs, op, rhs) in [
            (i64::MAX, BinaryOp::Add, 1),
            (i64::MIN, BinaryOp::Sub, 1),
            (i64::MAX, BinaryOp::Mul, 2),
            (i64::MIN, BinaryOp::Mod, -1),
        ] {
            let expr = binary(lit_int(lhs), op, lit_int(rhs));
            assert_eq!(
                eval_expr(&expr, &empty_event(), &ctx()),
                EvalValue::Null,
                "{lhs:?} {op:?} {rhs:?}"
            );
        }
    }

    #[test]
    fn nan_compares_as_duckdb_orders_it() {
        let nan = || binary(lit_int(0), BinaryOp::Div, lit_int(0));
        for (op, want) in [
            (BinaryOp::Eq, true),
            (BinaryOp::Ne, false),
            (BinaryOp::Gte, true),
            (BinaryOp::Lt, false),
        ] {
            let expr = binary(nan(), op, nan());
            assert_eq!(
                eval_expr(&expr, &empty_event(), &ctx()),
                EvalValue::Bool(want),
                "NaN {op:?} NaN"
            );
        }
        let expr = binary(nan(), BinaryOp::Gt, lit_float(f64::MAX));
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Bool(true)
        );
        let expr = binary(lit_float(-0.0), BinaryOp::Eq, lit_float(0.0));
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Bool(true)
        );
    }

    #[test]
    fn div_floats() {
        let expr = binary(lit_float(10.0), BinaryOp::Div, lit_float(4.0));
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Float(2.5)
        );
    }

    #[test]
    fn div_by_zero_float() {
        let expr = binary(lit_float(10.0), BinaryOp::Div, lit_float(0.0));
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Float(f64::INFINITY)
        );
    }

    #[test]
    fn modulo_by_zero_float() {
        let expr = binary(lit_int(5), BinaryOp::Mod, lit_float(0.0));
        let EvalValue::Float(value) = eval_expr(&expr, &empty_event(), &ctx()) else {
            panic!("5 % 0.0 must be a float");
        };
        assert!(value.is_nan());
    }

    #[test]
    fn modulo_ints() {
        let expr = binary(lit_int(10), BinaryOp::Mod, lit_int(3));
        assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Int(1));
    }

    #[test]
    fn modulo_by_zero() {
        let expr = binary(lit_int(10), BinaryOp::Mod, lit_int(0));
        assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Null);
    }

    #[test]
    fn arithmetic_null_propagation() {
        let expr = binary(lit_int(5), BinaryOp::Add, lit_null());
        assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Null);
    }

    #[test]
    fn arithmetic_with_field() {
        let ev = event(&json!({"duration": 100}));
        let expr = binary(field("duration"), BinaryOp::Mul, lit_int(1000));
        assert_eq!(eval_expr(&expr, &ev, &ctx()), EvalValue::Int(100_000));
    }

    #[test]
    fn eq_ints() {
        let expr = binary(lit_int(5), BinaryOp::Eq, lit_int(5));
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Bool(true)
        );
    }

    #[test]
    fn eq_int_float_cross() {
        let expr = binary(lit_int(5), BinaryOp::Eq, lit_float(5.0));
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Bool(true)
        );
    }

    #[test]
    fn ne_ints() {
        let expr = binary(lit_int(5), BinaryOp::Ne, lit_int(3));
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Bool(true)
        );
    }

    #[test]
    fn gt_ints() {
        let expr = binary(lit_int(5), BinaryOp::Gt, lit_int(3));
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Bool(true)
        );
    }

    #[test]
    fn gt_false() {
        let expr = binary(lit_int(3), BinaryOp::Gt, lit_int(5));
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Bool(false)
        );
    }

    #[test]
    fn gte_equal() {
        let expr = binary(lit_int(5), BinaryOp::Gte, lit_int(5));
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Bool(true)
        );
    }

    #[test]
    fn lt_ints() {
        let expr = binary(lit_int(3), BinaryOp::Lt, lit_int(5));
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Bool(true)
        );
    }

    #[test]
    fn lte_equal() {
        let expr = binary(lit_int(5), BinaryOp::Lte, lit_int(5));
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Bool(true)
        );
    }

    #[test]
    fn cmp_strings() {
        let expr = binary(lit_str("apple"), BinaryOp::Lt, lit_str("banana"));
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Bool(true)
        );
    }

    #[test]
    fn cmp_null_propagation() {
        let expr = binary(lit_int(5), BinaryOp::Gt, lit_null());
        assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Null);
    }

    #[test]
    fn timestamp_gt_str_past_is_true() {
        let expr = binary(
            call("now", vec![]),
            BinaryOp::Gt,
            lit_str("2000-01-01 00:00:00"),
        );
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Bool(true)
        );
    }

    #[test]
    fn timestamp_lt_str_future_is_true() {
        let expr = binary(
            call("now", vec![]),
            BinaryOp::Lt,
            lit_str("2999-12-31 23:59:59"),
        );
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Bool(true)
        );
    }

    #[test]
    fn timestamp_eq_str_same_instant() {
        let expr = binary(
            call(
                "strptime",
                vec![lit_str("2026-01-15 10:20:30"), lit_str("%Y-%m-%d %H:%M:%S")],
            ),
            BinaryOp::Eq,
            lit_str("2026-01-15 10:20:30"),
        );
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Bool(true)
        );
    }

    #[test]
    fn where_timestamp_gt_now_for_future_event() {
        let ev = event(&json!({"_time": "2999-12-31 23:59:59"}));
        let expr = binary(field("_time"), BinaryOp::Gt, call("now", vec![]));
        assert_eq!(eval_expr(&expr, &ev, &ctx()), EvalValue::Bool(true));
    }

    #[test]
    fn where_timestamp_gt_now_for_past_event() {
        let ev = event(&json!({"_time": "2000-01-01 00:00:00"}));
        let expr = binary(field("_time"), BinaryOp::Gt, call("now", vec![]));
        assert_eq!(eval_expr(&expr, &ev, &ctx()), EvalValue::Bool(false));
    }

    #[test]
    fn and_true_true() {
        let expr = binary(lit_bool(true), BinaryOp::And, lit_bool(true));
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Bool(true)
        );
    }

    #[test]
    fn and_true_false() {
        let expr = binary(lit_bool(true), BinaryOp::And, lit_bool(false));
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Bool(false)
        );
    }

    #[test]
    fn and_false_null() {
        let expr = binary(lit_bool(false), BinaryOp::And, lit_null());
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Bool(false)
        );
    }

    #[test]
    fn and_null_true() {
        let expr = binary(lit_null(), BinaryOp::And, lit_bool(true));
        assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Null);
    }

    #[test]
    fn or_true_null() {
        let expr = binary(lit_bool(true), BinaryOp::Or, lit_null());
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Bool(true)
        );
    }

    #[test]
    fn or_false_false() {
        let expr = binary(lit_bool(false), BinaryOp::Or, lit_bool(false));
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Bool(false)
        );
    }

    #[test]
    fn or_null_false() {
        let expr = binary(lit_null(), BinaryOp::Or, lit_bool(false));
        assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Null);
    }

    #[test]
    fn matches_regex() {
        let expr = binary(
            lit_str("prod-web-01"),
            BinaryOp::Matches,
            lit_str("prod-.*"),
        );
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Bool(true)
        );
    }

    #[test]
    fn matches_no_match() {
        let expr = binary(
            lit_str("staging-01"),
            BinaryOp::Matches,
            lit_str("^prod-.*"),
        );
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Bool(false)
        );
    }

    #[test]
    fn like_percent_wildcard() {
        let expr = binary(lit_str("prod-web-01"), BinaryOp::Like, lit_str("prod-%"));
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Bool(true)
        );
    }

    #[test]
    fn like_no_match() {
        let expr = binary(lit_str("staging-01"), BinaryOp::Like, lit_str("prod-%"));
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Bool(false)
        );
    }

    #[test]
    fn like_underscore_wildcard() {
        let expr = binary(lit_str("a1"), BinaryOp::Like, lit_str("a_"));
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Bool(true)
        );
    }

    #[test]
    fn like_case_sensitive() {
        let expr = binary(lit_str("PROD-01"), BinaryOp::Like, lit_str("prod-%"));
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Bool(false)
        );
    }

    #[test]
    fn ilike_case_insensitive() {
        let expr = binary(lit_str("PROD-01"), BinaryOp::ILike, lit_str("prod-%"));
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Bool(true)
        );
    }

    #[test]
    fn like_null_propagation() {
        let expr = binary(lit_null(), BinaryOp::Like, lit_str("prod-%"));
        assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Null);
    }

    #[test]
    fn a_pattern_operand_the_table_lacks_is_an_error() {
        let pinned_status = {
            let mut ft = crate::schema::FieldTypes::new();
            ft.insert("status", crate::schema::CanonicalType::BigInt);
            ft
        };
        let pinned = PinScope::root(&pinned_status);
        let ev = event(&json!({"status": 200, "host": "web-01"}));
        for op in [BinaryOp::Matches, BinaryOp::Like, BinaryOp::ILike] {
            for subject in [field("host"), field("status"), lit_null()] {
                let expr = call("coalesce", vec![binary(subject.clone(), op, lit_str("w%"))]);
                for pins in [&PinScope::unpinned(), &pinned] {
                    assert_eq!(
                        eval_expr_with_pins(&expr, &ev, pins, &PatternTable::default(), &ctx()),
                        Err(PatternError::Missing),
                        "{op:?} {subject:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_computed_pattern_operand_is_an_error() {
        let expr = binary(
            field("host"),
            BinaryOp::Matches,
            call("lower", vec![lit_str("A")]),
        );
        assert_eq!(
            super::eval_expr(
                &expr,
                &event(&json!({"host": "a"})),
                &PatternTable::default(),
                &ctx()
            ),
            Err(PatternError::Computed)
        );
    }

    #[test]
    fn not_true() {
        let expr = unary(UnaryOp::Not, lit_bool(true));
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Bool(false)
        );
    }

    #[test]
    fn not_false() {
        let expr = unary(UnaryOp::Not, lit_bool(false));
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Bool(true)
        );
    }

    #[test]
    fn not_null() {
        let expr = unary(UnaryOp::Not, lit_null());
        assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Null);
    }

    #[test]
    fn neg_int() {
        let expr = unary(UnaryOp::Neg, lit_int(42));
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Int(-42)
        );
    }

    #[test]
    fn neg_float() {
        let expr = unary(UnaryOp::Neg, lit_float(1.5));
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Float(-1.5)
        );
    }

    #[test]
    fn in_list_found() {
        let expr = in_list(lit_int(200), vec![lit_int(200), lit_int(301), lit_int(404)]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Bool(true)
        );
    }

    #[test]
    fn in_list_not_found() {
        let expr = in_list(lit_int(500), vec![lit_int(200), lit_int(301)]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Bool(false)
        );
    }

    #[test]
    fn in_list_null_target() {
        let expr = in_list(lit_null(), vec![lit_int(200)]);
        assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Null);
    }

    #[test]
    fn fn_lower() {
        let expr = call("lower", vec![lit_str("HELLO")]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Str("hello".to_string())
        );
    }

    #[test]
    fn fn_upper() {
        let expr = call("upper", vec![lit_str("hello")]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Str("HELLO".to_string())
        );
    }

    #[test]
    fn fn_length() {
        let expr = call("length", vec![lit_str("hello")]);
        assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Int(5));
    }

    #[test]
    fn fn_length_counts_chars_not_bytes() {
        let expr = call("length", vec![lit_str("café")]);
        assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Int(4));
    }

    #[test]
    fn fn_len_alias() {
        let expr = call("len", vec![lit_str("hi")]);
        assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Int(2));
    }

    #[test]
    fn fn_trim() {
        let expr = call("trim", vec![lit_str("  hello  ")]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Str("hello".to_string())
        );
    }

    #[test]
    fn fn_ltrim() {
        let expr = call("ltrim", vec![lit_str("  hello  ")]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Str("hello  ".to_string())
        );
    }

    #[test]
    fn fn_rtrim() {
        let expr = call("rtrim", vec![lit_str("  hello  ")]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
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
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Str("baz bar baz".to_string())
        );
    }

    #[test]
    fn fn_substr_two_args() {
        let expr = call("substr", vec![lit_str("hello world"), lit_int(7)]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
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
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Str("hello".to_string())
        );
    }

    #[test]
    fn fn_substr_null() {
        let expr = call("substr", vec![lit_null(), lit_int(1)]);
        assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Null);
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
        let abs_expr = call("abs", vec![lit_int(i64::MIN)]);
        assert_eq!(
            eval_expr(&abs_expr, &empty_event(), &ctx()),
            EvalValue::Null
        );

        let neg_expr = unary(UnaryOp::Neg, lit_int(i64::MIN));
        assert_eq!(
            eval_expr(&neg_expr, &empty_event(), &ctx()),
            EvalValue::Null
        );

        let ok = call("abs", vec![lit_int(-5)]);
        assert_eq!(eval_expr(&ok, &empty_event(), &ctx()), EvalValue::Int(5));
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
            (-0.0, "-0.0"),
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
        let expr = call("tostring", vec![lit_float(1.0)]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Str("1.0".to_string())
        );
    }

    #[test]
    fn fn_concat_float_renders_duckdb_way() {
        let expr = call("concat", vec![lit_str("x="), lit_float(0.1)]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Str("x=0.1".to_string())
        );
        let expr = call("concat", vec![lit_str("n="), lit_float(2.0)]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Str("n=2.0".to_string())
        );
    }

    #[test]
    fn fn_concat_mixed_types() {
        let expr = call(
            "concat",
            vec![lit_str("n="), lit_int(42), lit_str("/"), lit_bool(true)],
        );
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Str("n=42/true".to_string())
        );
    }

    #[test]
    fn fn_concat_skips_null() {
        let expr = call("concat", vec![lit_str("a"), lit_null(), lit_str("b")]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Str("ab".to_string())
        );
    }

    #[test]
    fn fn_concat_trailing_null() {
        let expr = call("concat", vec![lit_str("x"), lit_str("-"), lit_null()]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Str("x-".to_string())
        );
    }

    #[test]
    fn fn_concat_all_null_is_empty_string() {
        let expr = call("concat", vec![lit_null(), lit_null()]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Str(String::new())
        );
    }

    #[test]
    fn fn_abs_positive() {
        let expr = call("abs", vec![lit_int(-42)]);
        assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Int(42));
    }

    #[test]
    fn fn_abs_float() {
        let expr = call("abs", vec![lit_float(-1.5)]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Float(1.5)
        );
    }

    #[test]
    fn fn_ceil() {
        let expr = call("ceil", vec![lit_float(1.2)]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Float(2.0)
        );
    }

    #[test]
    fn fn_ceil_widens_an_integer_argument() {
        let expr = call("ceil", vec![lit_int(5)]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Float(5.0)
        );
    }

    #[test]
    fn fn_ceil_keeps_negative_zero() {
        let expr = call("ceil", vec![lit_float(-0.5)]);
        let EvalValue::Float(value) = eval_expr(&expr, &empty_event(), &ctx()) else {
            panic!("ceil(-0.5) must be a float");
        };
        assert!(value == 0.0 && value.is_sign_negative(), "{value}");
    }

    #[test]
    fn fn_ceiling_alias() {
        let expr = call("ceiling", vec![lit_float(1.2)]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Float(2.0)
        );
    }

    #[test]
    fn fn_floor() {
        let expr = call("floor", vec![lit_float(1.8)]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Float(1.0)
        );
    }

    #[test]
    fn fn_floor_widens_an_integer_argument() {
        let expr = call("floor", vec![lit_int(-5)]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Float(-5.0)
        );
    }

    #[test]
    fn fn_round_no_precision() {
        let expr = call("round", vec![lit_float(1.6)]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Float(2.0)
        );
    }

    #[test]
    fn fn_round_explicit_zero_precision_stays_double() {
        let expr = call("round", vec![lit_float(2.5), lit_int(0)]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Float(3.0)
        );
    }

    #[test]
    fn fn_round_with_precision() {
        let expr = call("round", vec![lit_float(1.456), lit_int(2)]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Float(1.46)
        );
    }

    #[test]
    fn fn_round_int_passthrough() {
        let expr = call("round", vec![lit_int(42)]);
        assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Int(42));
    }

    #[test]
    fn fn_if_true() {
        let expr = call("if", vec![lit_bool(true), lit_str("yes"), lit_str("no")]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Str("yes".to_string())
        );
    }

    #[test]
    fn fn_if_false() {
        let expr = call("if", vec![lit_bool(false), lit_str("yes"), lit_str("no")]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Str("no".to_string())
        );
    }

    #[test]
    fn fn_if_reads_its_condition_as_duckdb_casts_it() {
        for (condition, want) in [
            (lit_str("true"), Some("yes")),
            (lit_str("YES"), Some("yes")),
            // Read as false, where `is_truthy` would take the then branch.
            (lit_str("0"), Some("no")),
            (lit_str("no"), Some("no")),
            (lit_int(0), Some("no")),
            (lit_int(-1), Some("yes")),
            (lit_float(-0.0), Some("no")),
            (lit_null(), Some("no")),
            (lit_str("nonempty"), None),
            (lit_str(" true "), None),
            (lit_str(""), None),
            (lit_str("2"), None),
        ] {
            let expr = call("if", vec![condition, lit_str("yes"), lit_str("no")]);
            let expected = want.map_or(EvalValue::Null, |text| EvalValue::Str(text.to_string()));
            assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), expected);
        }
    }

    #[test]
    fn fn_case_stops_at_the_first_true_arm_and_nulls_on_an_unreadable_one() {
        let expr = call(
            "case",
            vec![
                lit_bool(true),
                lit_int(1),
                lit_str("nonempty"),
                lit_int(2),
                lit_int(3),
            ],
        );
        assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Int(1));

        for first in [lit_bool(false), lit_null()] {
            let expr = call(
                "case",
                vec![
                    first,
                    lit_int(1),
                    lit_str("nonempty"),
                    lit_int(2),
                    lit_int(3),
                ],
            );
            assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Null);
        }
    }

    #[test]
    fn fn_coalesce_first_non_null() {
        let expr = call("coalesce", vec![lit_null(), lit_int(42), lit_int(99)]);
        assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Int(42));
    }

    #[test]
    fn fn_coalesce_all_null() {
        let expr = call("coalesce", vec![lit_null(), lit_null()]);
        assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Null);
    }

    #[test]
    fn fn_isnull_null() {
        let expr = call("isnull", vec![lit_null()]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Bool(true)
        );
    }

    #[test]
    fn fn_isnull_not_null() {
        let expr = call("isnull", vec![lit_int(5)]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Bool(false)
        );
    }

    #[test]
    fn fn_isnotnull() {
        let expr = call("isnotnull", vec![lit_str("hi")]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Bool(true)
        );
    }

    #[test]
    fn fn_typeof_int() {
        let expr = call("typeof", vec![lit_int(5)]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Str("BIGINT".to_string())
        );
    }

    #[test]
    fn fn_typeof_string() {
        let expr = call("typeof", vec![lit_str("hi")]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Str("VARCHAR".to_string())
        );
    }

    #[test]
    fn fn_typeof_null() {
        let expr = call("typeof", vec![lit_null()]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Str("NULL".to_string())
        );
    }

    /// Asserted against a fixed context rather than bracketed between two
    /// clock reads: the anchor is truncated to microseconds, so a
    /// bracketing test would fail whenever the capture landed in the same
    /// microsecond as its lower bound.
    #[test]
    fn fn_now_returns_the_contexts_instant() {
        let at = EvalContext::at(
            chrono::DateTime::parse_from_rfc3339("2026-02-03T04:05:06.789012Z")
                .expect("a valid RFC 3339 instant")
                .into(),
        );
        let result = eval_expr(&call("now", vec![]), &empty_event(), &at);
        assert_eq!(
            result,
            EvalValue::Timestamp(compare::Instant::At(at.now_timestamp()))
        );
        assert_eq!(result, at.now_value(), "and it IS the context's own value");
    }

    #[test]
    fn two_now_calls_in_one_expression_are_one_instant() {
        let expr = binary(call("now", vec![]), BinaryOp::Eq, call("now", vec![]));
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Bool(true)
        );
    }

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
        assert_eq!(timestamp_to_duckdb_text(&ts), "2026-01-15 14:30:00.1");
    }

    #[test]
    fn as_finite_reads_the_literal_timestamp_syntax() {
        for (text, want) in [
            ("2026-01-15T14:30:00Z", "2026-01-15 14:30:00"),
            ("2026-01-15 14:30:00", "2026-01-15 14:30:00"),
            ("2026-01-15", "2026-01-15 00:00:00"),
            ("2026-01-15T14:30:00+02:00", "2026-01-15 14:30:00"),
        ] {
            let value = EvalValue::Str(text.to_string());
            assert_eq!(value.as_finite().unwrap().to_string(), want, "{text:?}");
        }
    }

    #[test]
    fn as_finite_is_none_for_a_text_with_no_reading() {
        for text in [
            "not-a-date",
            "2026-01-15 10:20:30+ab:cd",
            // Byte `len - 6` lands mid-emoji, where an offset stripper
            // slicing by byte would panic.
            "🦀🦀",
            "err: 🦀🦀",
        ] {
            let value = EvalValue::Str(text.to_string());
            assert!(value.as_finite().is_none(), "{text:?}");
            assert!(value.as_instant().is_none(), "{text:?}");
        }
    }

    #[test]
    fn as_instant_reads_an_infinity_that_as_finite_refuses() {
        for (text, want) in [
            ("infinity", compare::Instant::Infinity),
            ("-infinity", compare::Instant::NegInfinity),
            ("inf", compare::Instant::Infinity),
        ] {
            let value = EvalValue::Str(text.to_string());
            assert_eq!(value.as_instant(), Some(want), "{text:?}");
            assert!(value.as_finite().is_none(), "{text:?}");
        }
    }

    #[test]
    fn timestamp_is_truthy() {
        let ts = chrono::NaiveDateTime::parse_from_str("2026-01-15 00:00:00", "%Y-%m-%d %H:%M:%S")
            .unwrap();
        assert!(EvalValue::Timestamp(compare::Instant::At(ts)).is_truthy());
        assert!(EvalValue::Timestamp(compare::Instant::Infinity).is_truthy());
    }

    #[test]
    fn timestamp_to_json_is_duckdb_text() {
        let ts = chrono::NaiveDateTime::parse_from_str("2026-01-15 14:30:00", "%Y-%m-%d %H:%M:%S")
            .unwrap();
        let json_val = Value::from(EvalValue::Timestamp(compare::Instant::At(ts)));
        assert_eq!(json_val, json!("2026-01-15 14:30:00"));
        assert_eq!(
            Value::from(EvalValue::Timestamp(compare::Instant::Infinity)),
            json!("infinity")
        );
        assert_eq!(
            Value::from(EvalValue::Timestamp(compare::Instant::NegInfinity)),
            json!("-infinity")
        );
    }

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

    #[test]
    fn complex_where_condition() {
        let ev = event(&json!({"status": 404}));
        let expr = binary(
            binary(field("status"), BinaryOp::Gte, lit_int(400)),
            BinaryOp::And,
            binary(field("status"), BinaryOp::Lt, lit_int(500)),
        );
        assert_eq!(eval_expr(&expr, &ev, &ctx()), EvalValue::Bool(true));
    }

    #[test]
    fn complex_let_expression() {
        let ev = event(&json!({"duration": 2}));
        let expr = binary(
            binary(field("duration"), BinaryOp::Mul, lit_int(1000)),
            BinaryOp::Add,
            lit_int(50),
        );
        assert_eq!(eval_expr(&expr, &ev, &ctx()), EvalValue::Int(2050));
    }

    #[test]
    fn nested_function_call() {
        let expr = call("upper", vec![call("lower", vec![lit_str("HeLLo")])]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Str("HELLO".to_string())
        );
    }

    #[test]
    fn if_with_field_condition() {
        let ev = event(&json!({"message": "hello world"}));
        let expr = call(
            "if",
            vec![
                call("isnotnull", vec![field("message")]),
                call("length", vec![field("message")]),
                lit_int(0),
            ],
        );
        assert_eq!(eval_expr(&expr, &ev, &ctx()), EvalValue::Int(11));
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
        assert_eq!(eval_expr(&expr, &ev, &ctx()), EvalValue::Int(0));
    }

    fn ts(s: &str) -> Spanned<Expr> {
        lit_str(s)
    }

    fn ndt(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
            .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f"))
            .unwrap_or_else(|e| panic!("bad ndt str {s}: {e}"))
    }

    #[test]
    fn fn_tonumber_from_str() {
        let expr = call("tonumber", vec![lit_str("1.23")]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Float(1.23)
        );
    }

    #[test]
    fn fn_tonumber_from_int() {
        let expr = call("tonumber", vec![lit_int(42)]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Float(42.0)
        );
    }

    #[test]
    fn fn_tonumber_from_float() {
        let expr = call("tonumber", vec![lit_float(1.5)]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Float(1.5)
        );
    }

    #[test]
    fn fn_tonumber_from_bool() {
        for (input, want) in [(true, 1.0), (false, 0.0)] {
            let expr = call("tonumber", vec![lit_bool(input)]);
            assert_eq!(
                eval_expr(&expr, &empty_event(), &ctx()),
                EvalValue::Float(want)
            );
        }
    }

    #[test]
    fn fn_tonumber_non_numeric_str() {
        let expr = call("tonumber", vec![lit_str("nope")]);
        assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Null);
    }

    #[test]
    fn fn_tonumber_honors_inner_digit_separators() {
        let expr = call("tonumber", vec![lit_str("1_000")]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Float(1000.0)
        );

        let expr = call("tonumber", vec![lit_str("1_0.0_5")]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Float(10.05)
        );
    }

    #[test]
    fn fn_tonumber_rejects_misplaced_digit_separators() {
        for bad in ["_1000", "1000_", "1__000", "1_e3", "1,000"] {
            let expr = call("tonumber", vec![lit_str(bad)]);
            assert_eq!(
                eval_expr(&expr, &empty_event(), &ctx()),
                EvalValue::Null,
                "tonumber({bad:?}) should be Null"
            );
        }
    }

    #[test]
    fn fn_tostring_int() {
        let expr = call("tostring", vec![lit_int(42)]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Str("42".to_string())
        );
    }

    #[test]
    fn fn_tostring_null() {
        let expr = call("tostring", vec![lit_null()]);
        assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Null);
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
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Str("2026-01-15 14:30:00".to_string())
        );
    }

    #[test]
    fn fn_date_part_year() {
        let expr = call(
            "date_part",
            vec![lit_str("year"), ts("2026-03-15 10:20:30")],
        );
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Int(2026)
        );
    }

    #[test]
    fn fn_date_part_month() {
        let expr = call(
            "date_part",
            vec![lit_str("month"), ts("2026-03-15 10:20:30")],
        );
        assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Int(3));
    }

    #[test]
    fn fn_date_part_day() {
        let expr = call("date_part", vec![lit_str("day"), ts("2026-03-15 10:20:30")]);
        assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Int(15));
    }

    #[test]
    fn fn_date_part_hour() {
        let expr = call(
            "date_part",
            vec![lit_str("hour"), ts("2026-03-15 10:20:30")],
        );
        assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Int(10));
    }

    #[test]
    fn fn_date_part_dow_sunday() {
        // 2026-03-15 is a Sunday → DOW=0
        let expr = call("date_part", vec![lit_str("dow"), ts("2026-03-15 00:00:00")]);
        assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Int(0));
    }

    #[test]
    fn fn_date_part_quarter() {
        let expr = call(
            "date_part",
            vec![lit_str("quarter"), ts("2026-07-01 00:00:00")],
        );
        assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Int(3));
    }

    #[test]
    fn fn_date_part_unknown_unit() {
        let expr = call(
            "date_part",
            vec![lit_str("millennium"), ts("2026-01-01 00:00:00")],
        );
        assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Null);
    }

    #[test]
    fn fn_date_trunc_year() {
        let expr = call(
            "date_trunc",
            vec![lit_str("year"), ts("2026-07-15 10:20:30")],
        );
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Timestamp(compare::Instant::At(ndt("2026-01-01 00:00:00")))
        );
    }

    #[test]
    fn fn_date_trunc_month() {
        let expr = call(
            "date_trunc",
            vec![lit_str("month"), ts("2026-07-15 10:20:30")],
        );
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Timestamp(compare::Instant::At(ndt("2026-07-01 00:00:00")))
        );
    }

    #[test]
    fn fn_date_trunc_day() {
        let expr = call(
            "date_trunc",
            vec![lit_str("day"), ts("2026-07-15 10:20:30")],
        );
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Timestamp(compare::Instant::At(ndt("2026-07-15 00:00:00")))
        );
    }

    #[test]
    fn fn_date_trunc_hour() {
        let expr = call(
            "date_trunc",
            vec![lit_str("hour"), ts("2026-07-15 10:20:30")],
        );
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Timestamp(compare::Instant::At(ndt("2026-07-15 10:00:00")))
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
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Timestamp(compare::Instant::At(ndt("2026-07-13 00:00:00")))
        );
    }

    #[test]
    fn fn_date_diff_hour_boundary_crossing() {
        let expr = call(
            "date_diff",
            vec![
                lit_str("hour"),
                ts("2026-01-01 23:59:59"),
                ts("2026-01-02 00:00:00"),
            ],
        );
        assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Int(1));
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
        assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Int(7));
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
        assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Int(2));
    }

    #[test]
    fn fn_date_diff_negative() {
        let expr = call(
            "date_diff",
            vec![
                lit_str("day"),
                ts("2026-01-08 00:00:00"),
                ts("2026-01-01 00:00:00"),
            ],
        );
        assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Int(-7));
    }

    #[test]
    fn fn_date_diff_week_boundary() {
        let expr = call(
            "date_diff",
            vec![
                lit_str("week"),
                ts("2026-07-13 23:59:59"),
                ts("2026-07-20 00:00:00"),
            ],
        );
        assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Int(1));
    }

    #[test]
    fn fn_strftime_basic() {
        let expr = call(
            "strftime",
            vec![ts("2026-03-15 10:20:30"), lit_str("%Y-%m-%d")],
        );
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
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
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Str("2026-03-15 10:20:30".to_string())
        );
    }

    #[test]
    fn fn_strftime_invalid_specifier_returns_null_not_panic() {
        let expr = call("strftime", vec![ts("2026-03-15 10:20:30"), lit_str("%Q")]);
        assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Null);
    }

    #[test]
    fn fn_strftime_trailing_percent_returns_null_not_panic() {
        let expr = call("strftime", vec![ts("2026-03-15 10:20:30"), lit_str("%Y-%")]);
        assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Null);
    }

    #[test]
    fn fn_strptime_success() {
        let expr = call(
            "strptime",
            vec![lit_str("2026-03-15 10:20:30"), lit_str("%Y-%m-%d %H:%M:%S")],
        );
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Timestamp(compare::Instant::At(ndt("2026-03-15 10:20:30")))
        );
    }

    #[test]
    fn fn_strptime_failure_returns_null() {
        let expr = call(
            "strptime",
            vec![lit_str("not-a-date"), lit_str("%Y-%m-%d %H:%M:%S")],
        );
        assert_eq!(eval_expr(&expr, &empty_event(), &ctx()), EvalValue::Null);
    }

    #[test]
    fn fn_strptime_partial_formats_fill_like_duckdb() {
        for (input, fmt, want) in [
            ("2023", "%Y", "2023-01-01 00:00:00"),
            ("2023-11", "%Y-%m", "2023-11-01 00:00:00"),
            ("11-07", "%m-%d", "1900-11-07 00:00:00"),
            ("2023-11-07 14", "%Y-%m-%d %H", "2023-11-07 14:00:00"),
            ("2023-11-07", "%Y-%m-%d", "2023-11-07 00:00:00"),
            ("14:30:00", "%H:%M:%S", "1900-01-01 14:30:00"),
        ] {
            let expr = call("strptime", vec![lit_str(input), lit_str(fmt)]);
            assert_eq!(
                eval_expr(&expr, &empty_event(), &ctx()),
                EvalValue::Timestamp(compare::Instant::At(ndt(want))),
                "strptime({input:?}, {fmt:?})"
            );
        }
    }

    #[test]
    fn the_date_scalars_mirror_duckdb_over_an_infinity() {
        for (word, instant) in [
            ("infinity", compare::Instant::Infinity),
            ("-infinity", compare::Instant::NegInfinity),
        ] {
            let ts = || span(Expr::Literal(LiteralValue::String(word.to_string())));
            for unit in crate::emitter::DATE_PART_UNITS {
                let expr = call("date_part", vec![lit_str(unit), ts()]);
                assert_eq!(
                    eval_expr(&expr, &empty_event(), &ctx()),
                    EvalValue::Null,
                    "date_part({unit:?}, {word})"
                );
            }
            for unit in crate::emitter::DATE_UNITS {
                let expr = call("date_trunc", vec![lit_str(unit), ts()]);
                assert_eq!(
                    eval_expr(&expr, &empty_event(), &ctx()),
                    EvalValue::Timestamp(instant),
                    "date_trunc({unit:?}, {word})"
                );
            }
            for fmt in ["%Y-%m-%d %H:%M:%S", "%Y", "%j"] {
                let expr = call("strftime", vec![ts(), lit_str(fmt)]);
                assert_eq!(
                    eval_expr(&expr, &empty_event(), &ctx()),
                    EvalValue::Str(word.to_string()),
                    "strftime({word}, {fmt:?})"
                );
            }
            let finite = || lit_str("2026-01-15 09:00:00");
            for (start, end) in [(ts(), finite()), (finite(), ts()), (ts(), ts())] {
                let expr = call("date_diff", vec![lit_str("day"), start, end]);
                assert_eq!(
                    eval_expr(&expr, &empty_event(), &ctx()),
                    EvalValue::Null,
                    "date_diff over {word}"
                );
            }
        }
    }

    #[test]
    fn an_infinity_compares_as_duckdb_orders_it() {
        for (lhs, op, rhs, want) in [
            ("infinity", BinaryOp::Gt, "2026-01-15 09:00:00", true),
            ("-infinity", BinaryOp::Lt, "2026-01-15 09:00:00", true),
            ("2026-01-15 09:00:00", BinaryOp::Lt, "infinity", true),
            ("infinity", BinaryOp::Eq, "infinity", true),
            ("infinity", BinaryOp::Eq, "-infinity", false),
        ] {
            // The left operand is a real TIMESTAMP cell and the right is
            // the text the comparison coerces — two plain strings would
            // compare as strings, so the cell is built directly.
            let instant = EvalValue::Str(lhs.to_string()).as_instant().unwrap();
            let answer = eval_binary(
                &EvalValue::Timestamp(instant),
                op,
                &EvalValue::Str(rhs.to_string()),
            );
            assert_eq!(answer, EvalValue::Bool(want), "{lhs} {op:?} {rhs}");
        }
    }

    #[test]
    fn the_format_translation_touches_only_a_bare_percent_f() {
        for (input, want) in [
            ("%f", "%6f"),
            ("%Y-%m-%d %H:%M:%S.%f", "%Y-%m-%d %H:%M:%S.%6f"),
            ("%%f", "%%f"),
            ("x%%fy", "x%%fy"),
            ("%%%f", "%%%6f"),
            ("%Y-%m-%d", "%Y-%m-%d"),
            ("", ""),
            ("100%", "100%"),
        ] {
            assert_eq!(duckdb_strftime_format(input), want, "{input:?}");
        }
        assert!(matches!(
            duckdb_strftime_format("%Y-%m-%d"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn fn_strftime_percent_f_is_microseconds() {
        for (text, want) in [
            ("2026-01-15 09:00:00.123456", "123456"),
            ("2026-01-15 09:00:00.500000", "500000"),
            ("2026-01-15 09:00:00", "000000"),
        ] {
            let ts = EvalValue::Timestamp(compare::Instant::At(ndt(text)));
            let answer = eval_scalar_fn("strftime", &[ts, EvalValue::Str("%f".into())], &ctx());
            assert_eq!(answer, Some(EvalValue::Str(want.to_string())), "{text:?}");
        }
    }

    #[test]
    fn current_strptime_reads_only_a_six_digit_fraction() {
        let parse = |text: &str| {
            eval_scalar_fn(
                "strptime",
                &[
                    EvalValue::Str(text.to_string()),
                    EvalValue::Str("%Y-%m-%d %H:%M:%S.%f".into()),
                ],
                &ctx(),
            )
        };
        assert_eq!(
            parse("2026-01-15 09:00:00.500000"),
            Some(EvalValue::Timestamp(compare::Instant::At(ndt(
                "2026-01-15 09:00:00.5"
            )))),
            "six digits read exactly"
        );
        for text in ["2026-01-15 09:00:00.5", "2026-01-15 09:00:00.123"] {
            assert_eq!(parse(text), Some(EvalValue::Null), "{text:?}");
        }
    }

    #[test]
    fn the_json_spelling_pass_only_touches_an_exponent_sign() {
        let render = |doc: &str| {
            let value: serde_json::Value = serde_json::from_str(doc).expect("valid JSON");
            duckdb_json_text(&value)
        };
        assert_eq!(render("1e300"), "1e300");
        assert_eq!(render("-1e300"), "-1e300");
        assert_eq!(render("[1e300,2]"), "[1e300,2]");
        assert_eq!(render(r#"{"a":{"b":[1e300]}}"#), r#"{"a":{"b":[1e300]}}"#);
        assert_eq!(render("1e-7"), "1e-7");
        assert_eq!(render(r#"{"a":"cost e+300"}"#), r#"{"a":"cost e+300"}"#);
        assert_eq!(render(r#"["e+1",1e300]"#), r#"["e+1",1e300]"#);
        assert_eq!(
            render(r#"{"a":"quote \" then e+5","b":1e300}"#),
            r#"{"a":"quote \" then e+5","b":1e300}"#
        );
        assert_eq!(
            render(r#"{"a":"x","b":[1,true,null]}"#),
            r#"{"a":"x","b":[1,true,null]}"#
        );
        assert_eq!(render("[true,false,1e300]"), "[true,false,1e300]");
    }

    #[test]
    fn fn_json_extract_returns_json_text() {
        let extract = |doc: &str, path: &str| {
            eval_scalar_fn(
                "json_extract",
                &[
                    EvalValue::Str(doc.to_string()),
                    EvalValue::Str(path.to_string()),
                ],
                &ctx(),
            )
        };
        for (doc, path, want) in [
            (r#"{"a":1}"#, "$.a", Some("1")),
            (r#"{"a":1.5}"#, "$.a", Some("1.5")),
            (r#"{"a":"x"}"#, "$.a", Some(r#""x""#)),
            (r#"{"a":true}"#, "$.a", Some("true")),
            (r#"{"a":null}"#, "$.a", Some("null")),
            (r#"{"a":[1,2]}"#, "$.a", Some("[1,2]")),
            (r#"{"a":{"b":1}}"#, "$.a", Some(r#"{"b":1}"#)),
            ("[1,2]", "$", Some("[1,2]")),
            (r#"{"a":1}"#, "$.missing", None),
        ] {
            let answer = extract(doc, path);
            let expected = want.map_or(EvalValue::Null, |text| EvalValue::Str(text.to_string()));
            assert_eq!(answer, Some(expected), "json_extract({doc}, {path})");
        }
    }

    #[test]
    fn fn_typeof_now_is_timestamp() {
        let expr = call("typeof", vec![call("now", vec![])]);
        assert_eq!(
            eval_expr(&expr, &empty_event(), &ctx()),
            EvalValue::Str("TIMESTAMP".to_string())
        );
    }

    #[test]
    fn fn_sev_reads_the_ladder() {
        for (text, expected) in [
            ("error", EvalValue::Int(17)),
            ("ERR", EvalValue::Int(17)),
            (" error ", EvalValue::Int(17)),
            ("error2", EvalValue::Int(18)),
            ("17", EvalValue::Int(17)),
            ("007", EvalValue::Int(7)),
            ("+17", EvalValue::Int(17)),
            ("0", EvalValue::Null),
            ("25", EvalValue::Null),
            ("-1", EvalValue::Null),
            ("1.5", EvalValue::Null),
            ("1e1", EvalValue::Null),
            ("0x10", EvalValue::Null),
            ("gold", EvalValue::Null),
            ("", EvalValue::Null),
        ] {
            assert_eq!(
                eval_expr(&call("sev", vec![lit_str(text)]), &empty_event(), &ctx()),
                expected,
                "sev({text:?})"
            );
        }
        assert_eq!(
            eval_expr(&call("sev", vec![lit_int(17)]), &empty_event(), &ctx()),
            EvalValue::Int(17)
        );
        assert_eq!(
            eval_expr(&call("sev", vec![lit_int(25)]), &empty_event(), &ctx()),
            EvalValue::Null
        );
        assert_eq!(
            eval_expr(&call("sev", vec![lit_float(17.0)]), &empty_event(), &ctx()),
            EvalValue::Null
        );
        assert_eq!(
            eval_expr(&call("sev", vec![lit_bool(true)]), &empty_event(), &ctx()),
            EvalValue::Null
        );
        assert_eq!(
            eval_expr(&call("sev", vec![field("level")]), &empty_event(), &ctx()),
            EvalValue::Null
        );
    }

    #[test]
    fn fn_sev_dialect_inverts_numerics_only() {
        let sev = |arg: Spanned<Expr>, dialect: &str| {
            eval_expr(
                &call("sev", vec![arg, lit_str(dialect)]),
                &empty_event(),
                &ctx(),
            )
        };
        assert_eq!(sev(lit_int(3), "syslog"), EvalValue::Int(17));
        assert_eq!(sev(lit_int(3), "otel"), EvalValue::Int(3));
        assert_eq!(sev(lit_str("3"), "syslog"), EvalValue::Int(17));
        assert_eq!(sev(lit_int(0), "syslog"), EvalValue::Int(24));
        assert_eq!(sev(lit_int(8), "syslog"), EvalValue::Null);
        assert_eq!(sev(lit_str("error"), "syslog"), EvalValue::Int(17));
        assert_eq!(sev(lit_int(3), "SYSLOG"), EvalValue::Int(17));
        assert_eq!(sev(lit_int(3), "rfc5424"), EvalValue::Null);
    }

    #[test]
    fn fn_sev_over_event_columns() {
        let ev = event(&json!({"level": "warn", "syslog_severity": 3}));
        assert_eq!(
            eval_expr(&call("sev", vec![field("level")]), &ev, &ctx()),
            EvalValue::Int(13)
        );
        assert_eq!(
            eval_expr(
                &call("sev", vec![field("syslog_severity"), lit_str("syslog")]),
                &ev,
                &ctx()
            ),
            EvalValue::Int(17)
        );
    }

    #[test]
    fn all_scalar_known_functions_return_some() {
        use crate::emitter::is_aggregate_function;
        use crate::parser::suggest::KNOWN_FUNCTIONS;
        // Generous arg list: enough variety that arity/type checks don't block.
        let ts_val = EvalValue::Timestamp(compare::Instant::At(ndt("2026-01-15 10:20:30")));
        let generous_args = vec![
            ts_val.clone(),
            EvalValue::Str("year".to_string()),
            EvalValue::Int(1),
            EvalValue::Float(1.0),
            EvalValue::Bool(true),
        ];
        for &func in KNOWN_FUNCTIONS {
            if is_aggregate_function(func) {
                continue;
            }
            let result = eval_scalar_fn(func, &generous_args, &ctx());
            assert!(
                result.is_some(),
                "eval_scalar_fn({func:?}, ..., &ctx()) returned None — \
                 add it to eval_scalar_fn or the coverage test will keep failing"
            );
        }
    }
}
