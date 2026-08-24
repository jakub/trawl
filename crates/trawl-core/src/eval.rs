// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! In-memory expression evaluator for streaming pipeline stages.
//!
//! Evaluates `Expr` AST nodes against a `serde_json::Map` event
//! rather than emitting SQL. This is the runtime analog of
//! `emitter/expr.rs`.

use std::borrow::Cow;

use crate::ast::{BinaryOp, Expr, FilterOp, FloatLiteral, LiteralValue, Spanned, UnaryOp};
use crate::compare;
use crate::emitter::SqlValue;
use crate::pin_match::{self, NullReadPolicy};
use crate::pin_scope::{PinScope, PinnedSubject};
use crate::row::Row;
use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use serde_json::Value;

/// Result of evaluating an expression against an event.
#[derive(Debug, Clone, PartialEq)]
pub enum EvalValue {
    Null,
    Bool(bool),
    Int(i64),
    /// An unsigned integer ABOVE `i64::MAX` — the one JSON number shape
    /// no signed integer can hold.
    ///
    /// It exists for IDENTITY and nothing else. Every value-domain
    /// operation reads it exactly as it read the `f64` this used to
    /// become at the row's door — arithmetic, comparison, `typeof`
    /// (`DOUBLE`), truthiness, `tonumber`, the accumulators — because
    /// that is what a field carrying `18446744073709551615` has always
    /// answered. What it does NOT do is round on the way THROUGH: the
    /// wire egress, `cell_text`, `cell_key` and the pinned read
    /// reproduce the digits the sender sent, so a pass-through field
    /// survives, `dedup` cannot merge two ids one apart, and a group is
    /// still a group.
    ///
    /// Give it no novel semantics. A rule that treats it as anything but
    /// "a double with its digits kept" is a rule the JSON row never had.
    UInt(u64),
    Float(f64),
    Str(String),
    Array(Vec<EvalValue>),
    /// A `DuckDB` TIMESTAMP: the wall-clock instant its `AS TIMESTAMP`
    /// cast produces, or one of the two INFINITIES no calendar date can
    /// express.
    ///
    /// The payload is [`compare::Instant`], whose derived `Ord` IS
    /// `DuckDB`'s TIMESTAMP ordering (`-infinity` below every date,
    /// `infinity` above — probed). Carrying a bare `NaiveDateTime` meant
    /// the infinities had nowhere to live: a stored one read as NULL live
    /// while batch compared it happily. Never unwrap the finite arm
    /// inside a comparison; that is what [`EvalValue::as_finite`] is for,
    /// and it exists so a calendar function cannot invent its own
    /// infinity rule.
    Timestamp(compare::Instant),
}

impl EvalValue {
    /// The LEGACY logical predicate: `Null` and `false` are falsy, every
    /// other value — including a non-empty string, a timestamp and a list
    /// — is true.
    ///
    /// This is NOT `DuckDB`'s boolean domain. `DuckDB` CASTS a condition
    /// and REFUSES a string outside its vocabulary (probed:
    /// `an_if_condition_is_a_boolean_cast_not_truthiness`), which is what
    /// [`read_condition`] mirrors and what `if`/`case` now read their
    /// condition through. This predicate stays behind `and`/`or`/`not` and
    /// the streaming `where` gate on purpose: those decide whether a LIVE
    /// ALERT fires, and #105 was not licensed to change that.
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

    /// Try to extract a numeric value as f64.
    #[allow(clippy::cast_precision_loss)]
    fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Int(n) => Some(*n as f64),
            // The rounding the JSON row did at its door, kept in the one
            // place every numeric rule reads through.
            Self::UInt(n) => Some(*n as f64),
            Self::Float(n) => Some(*n),
            _ => None,
        }
    }

    /// Try to coerce to a string representation.
    fn as_str_repr(&self) -> Option<String> {
        match self {
            Self::Str(s) => Some(s.clone()),
            Self::Int(n) => Some(n.to_string()),
            // `tostring()`/`concat()` render what the EXPRESSION read,
            // which was the rounded double.
            #[allow(clippy::cast_precision_loss)]
            Self::UInt(n) => Some(duckdb_double_to_string(*n as f64)),
            Self::Float(n) => Some(duckdb_double_to_string(*n)),
            Self::Bool(b) => Some(b.to_string()),
            Self::Timestamp(instant) => Some(instant.cast_text()),
            Self::Null | Self::Array(_) => None,
        }
    }

    /// This value as an INSTANT, infinities included.
    ///
    /// A string is read by [`compare::literal_timestamp`] — the
    /// `TRY_CAST(text AS TIMESTAMP)` a bound VARCHAR parameter gets, the
    /// one probe-pinned owner of that syntax. `eval` used to keep a
    /// SECOND parser here, which accepted a malformed offset
    /// (`+ab:cd`) the engine rejects and could not read an infinity at
    /// all; deleting it is ADR-0017 §1.
    pub(crate) fn as_instant(&self) -> Option<compare::Instant> {
        match self {
            Self::Timestamp(instant) => Some(*instant),
            Self::Str(s) => compare::literal_timestamp(s),
            _ => None,
        }
    }

    /// This value as a FINITE instant — the door every calendar function
    /// reads through.
    ///
    /// An infinity is `None` here on purpose: `date_part` and `date_diff`
    /// answer NULL for one (probed, every unit), and routing them through
    /// this door makes that one rule instead of eleven.
    pub(crate) fn as_finite(&self) -> Option<NaiveDateTime> {
        match self.as_instant()? {
            compare::Instant::At(at) => Some(at),
            compare::Instant::Infinity | compare::Instant::NegInfinity => None,
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
/// 2. Negative zero KEEPS its sign: `-0.0` → `"-0.0"`. (A SQL *literal*
///    `-0.0` renders `0.0`, but only because the parser constant-folds it to
///    positive zero — a computed or stored `-0.0` renders signed, probe-pinned
///    in `trawl-engine/tests/duckdb_probe.rs`.)
/// 3. Scientific notation kicks in at the SAME magnitude thresholds as Rust's
///    `{:?}` (`>= 1e16` and `< 1e-4`), so we lean on Debug for the switch-over.
/// 4. The exponent ALWAYS carries a sign and is zero-padded to a minimum of two
///    digits (`e+05`, `e-05`, `e+16`, `e+100`), where Rust `{:?}` emits `e16` /
///    `e-5` (no sign, no pad).
/// 5. Specials render lowercase: `inf`, `-inf`, `nan` — and a NaN KEEPS
///    its sign bit like any other value, so a negative one renders
///    `-nan` (probed in `a_rendered_nan_keeps_its_sign`). Both spellings
///    pass the DOUBLE pin's round-trip guard, so both are values a
///    conformed column really stores.
///
/// This is the single renderer behind `tostring()`, `concat()`/`||`, and any
/// other `CAST(… AS VARCHAR)` over a float in the batch path; mirroring it in
/// streaming eval closes the #22-class batch-vs-live divergence. It is also
/// the DOUBLE pin's glob/regex text — re-exported as
/// [`crate::compare::canonical_double_text`], so a pattern over a
/// DOUBLE-pinned column matches the same string in both engines.
pub(crate) fn duckdb_double_to_string(x: f64) -> String {
    if x.is_nan() {
        // The sign bit, not the ordering: `-nan < 0.0` is false, so the
        // infinity test below would not have caught it.
        return if x.is_sign_negative() { "-nan" } else { "nan" }.to_string();
    }
    if x.is_infinite() {
        return if x < 0.0 { "-inf" } else { "inf" }.to_string();
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

impl From<EvalValue> for Value {
    fn from(v: EvalValue) -> Self {
        match v {
            EvalValue::Null => Value::Null,
            EvalValue::Bool(b) => Value::Bool(b),
            EvalValue::Int(n) => Value::Number(n.into()),
            // Verbatim: this is the whole reason the variant exists.
            EvalValue::UInt(n) => Value::Number(n.into()),
            EvalValue::Float(n) => {
                serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number)
            }
            EvalValue::Str(s) => Value::String(s),
            EvalValue::Array(a) => Value::Array(a.into_iter().map(Value::from).collect()),
            // Serialize timestamps in DuckDB's own cast text so the event
            // map round-trips correctly through serde_json — byte-identical
            // to the old rendering for every finite instant, and the words
            // `infinity`/`-infinity` for the two that had no rendering at
            // all before.
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
                    // Above `i64::MAX`: kept exactly, read as a double.
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

/// Evaluate an expression AST node against an event map.
///
/// The documented PIN-BLIND door: every comparison stays literal-driven,
/// exactly as before ADR-0011 slice A′ — embedded mode's behavior, and the
/// zero-cost path when no catalog exists. Catalog-backed callers go
/// through [`eval_expr_with_pins`].
pub fn eval_expr(expr: &Spanned<Expr>, event: &Row) -> EvalValue {
    static EMPTY: std::sync::LazyLock<PinScope> = std::sync::LazyLock::new(PinScope::unpinned);
    eval_expr_with_pins(expr, event, &EMPTY)
}

/// Evaluate an expression with the catalog's pin scope typing bare
/// field-vs-literal comparisons (ADR-0011 slice A′).
///
/// `pins` is the scope in force at THIS stage of the pipeline (see
/// [`crate::pin_scope::PinScope`]); the same rule table the SQL emitter
/// renders decides how each comparison binds, wherever the walk meets one
/// — inside `if()` conditions, under `not`/`and`/`or`, in `| let` values.
/// An empty scope short-circuits every pinned check, so the pin-blind
/// path stays zero-cost.
pub fn eval_expr_with_pins(expr: &Spanned<Expr>, event: &Row, pins: &PinScope) -> EvalValue {
    match &expr.node {
        Expr::Literal(lit) => eval_literal(lit),
        Expr::FieldRef(name) => {
            // Bound the way `DuckDB` binds a column reference — and the
            // way the PINNED read beside it already binds: a bare
            // `let b = A` over a row carrying `a` reads that column
            // rather than answering NULL, so a reference does not change
            // meaning with the pin.
            let mapped = name.as_str();
            bind_event_key(event, mapped)
                .and_then(|key| event.get(key))
                .cloned()
                .unwrap_or(EvalValue::Null)
        }
        Expr::Binary { lhs, op, rhs } => try_pinned_comparison(lhs, *op, rhs, event, pins)
            .unwrap_or_else(|| {
                eval_binary(
                    &eval_expr_with_pins(lhs, event, pins),
                    *op,
                    &eval_expr_with_pins(rhs, event, pins),
                )
            }),
        Expr::Unary { op, operand } => eval_unary(*op, eval_expr_with_pins(operand, event, pins)),
        Expr::FunctionCall { name, args } => {
            let evaluated: Vec<EvalValue> = args
                .iter()
                .map(|a| eval_expr_with_pins(a, event, pins))
                .collect();
            eval_scalar_fn(name, &evaluated).unwrap_or(EvalValue::Null)
        }
        Expr::InList { expr: target, list } => {
            if let Some(answer) = try_pinned_in_list(target, list, event, pins) {
                return answer;
            }
            let target_val = eval_expr_with_pins(target, event, pins);
            if matches!(target_val, EvalValue::Null) {
                return EvalValue::Null;
            }
            // SQL's `IN` is three-valued: a TRUE anywhere wins, but an
            // element that answers UNKNOWN makes a non-match UNKNOWN
            // rather than FALSE — `x IN (a, b)` with a NULL `b` and no
            // match is NULL, and collapsing it to FALSE inverts under
            // `NOT`.
            let mut unknown = false;
            for item in list {
                let item_val = eval_expr_with_pins(item, event, pins);
                match eval_eq(&target_val, &item_val) {
                    EvalValue::Bool(true) => return EvalValue::Bool(true),
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
    }
}

/// A bare literal, exactly as the SQL lane's door takes it — the AST value,
/// so a float literal keeps its source token rather than a re-rendered
/// double (ADR-0011 ruling #6, [`crate::ast::FloatLiteral`]) — including
/// the sign fold for a negative number, which the parser hands over as
/// `Unary{Neg, Literal}`. The two doors must adopt the same shapes or the
/// batch and live lanes answer one query differently
/// (`emitter::expr::bare_literal`).
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

/// Bind a field reference to the key the ROW actually carries, the way
/// `DuckDB` binds a column reference: an exact match wins, and any
/// ASCII-case-insensitive match binds otherwise.
///
/// Both halves are load-bearing, in both directions. Ingest ASCII-folds
/// every key it writes, so `where Status>400` over a stored `status`
/// finds its value only case-insensitively — but the pipeline lanes key
/// rows by USER-CHOSEN names carried VERBATIM (`rename status as St`
/// gives the row an `St` key, in the SQL result columns and in
/// [`crate::stream::apply_stage`] alike), so a later `where st>400` —
/// which `DuckDB` resolves to that `St` column — must find it too, and a
/// fold-only lookup would miss. Exact-first keeps a row carrying two
/// case-variant keys reading the one the reference names.
///
/// A residual ambiguity (several case-variants, none exact) has no
/// `DuckDB` answer to mirror — it errors — so this picks the
/// lexicographically-first variant: deterministic regardless of map
/// order, and the same tie-break ingest's own fold-collision rule uses.
pub(crate) fn bind_event_key<'e>(event: &'e Row, name: &str) -> Option<&'e str> {
    if let Some((key, _)) = event.get_key_value(name) {
        return Some(key.as_str());
    }
    event
        .keys()
        .filter(|k| k.eq_ignore_ascii_case(name))
        .min()
        .map(String::as_str)
}

/// The event value a pinned comparison reads, non-null.
///
/// The name is alias-resolved (`timestamp` → `_time`) and then bound
/// against the row's own spelling by [`bind_event_key`]. Binding — not
/// non-nullness — decides which key is read: a bound key holding JSON
/// null is that field's own NULL (UNKNOWN), never a reason to read a
/// differently-cased sibling.
fn pinned_event_value<'e>(event: &'e Row, name: &str) -> Option<&'e EvalValue> {
    let key = bind_event_key(event, name)?;
    event.get(key).filter(|v| !matches!(v, EvalValue::Null))
}

/// Truth → `EvalValue`: UNKNOWN is SQL NULL, which the existing
/// `not`/`and`/`or`/`is_truthy` machinery already propagates three-valued.
fn truth_to_eval(truth: Option<bool>) -> EvalValue {
    truth.map_or(EvalValue::Null, EvalValue::Bool)
}

/// The value a pinned SUBJECT reads for one event — the column's own
/// value, or the pin-declaring call's result — or `None` for a NULL
/// subject (an absent field, a JSON null, a call with no reading).
fn subject_value(
    subject: &PinnedSubject<'_>,
    event: &Row,
    pins: &PinScope,
) -> Option<serde_json::Value> {
    let cell = match subject {
        PinnedSubject::Field(name) => pinned_event_value(event, name)?.clone(),
        // Evaluated by the ordinary expression path — the same value
        // `| let s = sev(level)` would project, so a comparison and a
        // projection can never read one event two ways.
        PinnedSubject::Call(call) => eval_expr_with_pins(call, event, pins),
    };
    if matches!(cell, EvalValue::Null) {
        return None;
    }
    Some(pin_read(&cell))
}

/// The projection a pinned read gives one cell — the shape
/// [`crate::pin_match`] conforms and compares, which is still the JSON
/// domain (retyping the search-stage matcher would change what it reads
/// off the firehose, and cost a conversion per event to do it).
///
/// Every arm is the value the row CARRIED before rows were typed, with
/// exactly one deliberate difference: a non-finite double projects as its
/// `DuckDB` TEXT (`inf`, `-inf`, `nan`, `-nan`) instead of vanishing.
/// JSON cannot spell one, so the old row held `null` there and a
/// DOUBLE-pinned `| where d > 1` answered UNKNOWN over a stored infinity
/// the batch query compares happily. The text is not a workaround: it is
/// what the conformed column HOLDS, and both
/// [`crate::compare::try_cast_double`] and the SQL `TRY_CAST` read it
/// back as the same double.
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
        // to: byte-identical to the old JSON string for a finite one, and
        // a word for an infinity — which both `literal_timestamp` and
        // `conformed_timestamp` read back as the instant it is, so a
        // TIMESTAMP-pinned comparison over one answers rather than
        // nulling.
        EvalValue::Timestamp(instant) => serde_json::Value::String(instant.cast_text()),
        other => serde_json::Value::from(other.clone()),
    }
}

/// The in-memory mirror of the emitter's pinned-comparison arm (ADR-0011
/// slice A′, widened by ADR-0013 ruling 9): detect a pinned-subject-vs-
/// literal comparison and answer it through the shared comparison core.
/// `None` falls through to literal-driven evaluation — same structural
/// scope as the SQL side (`emitter::expr::try_pinned_comparison`), which
/// consumes the SAME classifier, so the two lanes adopt and decline
/// exactly the same shapes.
///
/// There is deliberately NO empty-scope fast path in front of the
/// classifier: a pin-declaring call carries the FUNCTION's pin, and
/// `sev()` has to bind identically over a corpus with no catalog at all
/// (embedded `--data`) or the two lanes would split there.
///
/// NULL policy is the pipeline's (strict): an absent field, a JSON null,
/// or a value the conform would null out is UNKNOWN for every operator —
/// no `!=` widening.
fn try_pinned_comparison(
    lhs: &Spanned<Expr>,
    op: BinaryOp,
    rhs: &Spanned<Expr>,
    event: &Row,
    pins: &PinScope,
) -> Option<EvalValue> {
    // Pattern operators: the subject is the LEFT operand only — the
    // right operand is the pattern.
    if matches!(op, BinaryOp::Matches | BinaryOp::Like | BinaryOp::ILike) {
        let (subject, pin) = pins.subject_pin(lhs)?;
        let Expr::Literal(LiteralValue::String(pattern)) = &rhs.node else {
            return None;
        };
        let form = crate::compare::pattern_form(Some(pin));
        let Some(value) = subject_value(&subject, event, pins) else {
            return Some(EvalValue::Null);
        };
        // No canonical text (a BIGINT pin over "4.5") is a NULL pattern
        // target in batch — UNKNOWN, not false.
        let Some(text) = pin_match::pattern_text(&value, form) else {
            return Some(EvalValue::Null);
        };
        let subject = EvalValue::Str(text);
        let pattern = EvalValue::Str(pattern.clone());
        return Some(match op {
            BinaryOp::Matches => eval_matches(&subject, &pattern),
            BinaryOp::Like => eval_like(&subject, &pattern, false),
            _ => eval_like(&subject, &pattern, true),
        });
    }

    let filter_op = match op {
        BinaryOp::Eq => FilterOp::Eq,
        BinaryOp::Ne => FilterOp::Ne,
        BinaryOp::Gt => FilterOp::Gt,
        BinaryOp::Gte => FilterOp::Gte,
        BinaryOp::Lt => FilterOp::Lt,
        BinaryOp::Lte => FilterOp::Lte,
        _ => return None,
    };
    let (subject, pin, filter_op, literal) = match (pins.subject_pin(lhs), pins.subject_pin(rhs)) {
        (Some((subject, pin)), _) => (subject, pin, filter_op, bare_literal(&rhs.node)?),
        // `400 < status` is `status > 400`.
        (None, Some((subject, pin))) => {
            let flipped = match filter_op {
                FilterOp::Gt => FilterOp::Lt,
                FilterOp::Gte => FilterOp::Lte,
                FilterOp::Lt => FilterOp::Gt,
                FilterOp::Lte => FilterOp::Gte,
                other => other,
            };
            (subject, pin, flipped, bare_literal(&lhs.node)?)
        }
        (None, None) => return None,
    };
    // A literal the rule table refuses (an unknown severity token) cannot
    // reach here: the compiler in front of BOTH eval lanes —
    // `stream::compile_stream_plan`, which every SSE stage and every
    // `rust_stages` batch tail is compiled through — resolves the same
    // form and rejects it. Falling through to the generic path is the
    // honest defensive answer, not a second semantics.
    let form = crate::compare::compare_form_bound(Some(pin), filter_op, &literal)
        .ok()
        .flatten()?;
    // Native(String) is the VARCHAR pin's lexical rule — the stored text
    // compares as text, whatever JSON shape the wire value took (the SQL
    // side's generic emission compares the VARCHAR column against a
    // string parameter, which is the same lexical answer). Any other
    // Native form is degenerate under a pin and falls through to
    // literal-driven evaluation, mirroring the SQL side's fall-through.
    match &form {
        crate::compare::CompareForm::Native(SqlValue::String(_)) => {}
        crate::compare::CompareForm::Native(_) => return None,
        _ => {}
    }
    let coerced = pin_match::coerce_form(form);
    let compare_op = pin_match::CompareOp::from_filter(filter_op)?;
    let Some(value) = subject_value(&subject, event, pins) else {
        // Plain SQL null propagation — strict, no `!=` widening.
        return Some(EvalValue::Null);
    };
    Some(truth_to_eval(pin_match::compare_values(
        &value,
        compare_op,
        &coerced,
        NullReadPolicy::Unknown,
    )))
}

/// The in-memory mirror of the emitter's pinned IN-list arm: a pinned
/// bare-field target with an all-literal list routes each element through
/// the equality rule, OR-combined in SQL's three-valued logic.
fn try_pinned_in_list(
    target: &Spanned<Expr>,
    list: &[Spanned<Expr>],
    event: &Row,
    pins: &PinScope,
) -> Option<EvalValue> {
    let (subject, pin) = pins.subject_pin(target)?;
    let forms: Vec<crate::compare::CompareForm> = list
        .iter()
        .map(|item| {
            let element = bare_literal(&item.node)?;
            crate::compare::compare_form_bound(Some(pin), FilterOp::Eq, &element)
                .ok()
                .flatten()
        })
        .collect::<Option<_>>()?;
    let Some(value) = subject_value(&subject, event, pins) else {
        return Some(EvalValue::Null);
    };
    let truth = pin_match::or_any(forms.into_iter().map(|form| {
        pin_match::compare_values(
            &value,
            pin_match::CompareOp::Eq,
            &pin_match::coerce_form(form),
            NullReadPolicy::Unknown,
        )
    }));
    Some(truth_to_eval(truth))
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

/// `+ - *`, integer-exact where both operands are integers.
///
/// `int_op` is CHECKED and an overflow is `Null`: `DuckDB` raises
/// `Out of Range Error` on every one of these (probed:
/// `integer_arithmetic_overflow_is_an_error_never_a_wrap`), a streaming
/// lane cannot raise a per-event error, and ADR-0017 §5's ratified rule
/// is that eval nulls where batch errors. The unchecked form was a debug
/// PANIC — a whole SSE subscription killed by one adversarial event —
/// and a silent wrap in release. The DOUBLE arm is deliberately
/// unchecked: `DuckDB` saturates a DOUBLE overflow to infinity rather
/// than erroring, so IEEE is the mirror there.
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

/// `/` — TRUE division, in DOUBLE, for every numeric pair.
///
/// `DuckDB`'s `/` has no integer form: `5 / 2` is `2.5` and both
/// operands go through DOUBLE, so a dividend above 2^53 comes back
/// rounded (probed: `integer_division_is_true_division_through_double`).
/// Division by zero is IEEE — `±inf`, or NaN for `0 / 0` — never the
/// NULL this used to answer, and `i64::MIN / -1` is an ordinary value
/// rather than the overflow the same pair raises under `%`.
fn eval_div(lhs: &EvalValue, rhs: &EvalValue) -> EvalValue {
    match (lhs.as_f64(), rhs.as_f64()) {
        (Some(a), Some(b)) => EvalValue::Float(a / b),
        _ => EvalValue::Null,
    }
}

/// `%` — the one arithmetic operator that still splits on the operand
/// types.
///
/// All-integer stays integral and `checked_rem` covers BOTH shapes
/// `DuckDB` refuses to answer with a number: `x % 0` is NULL, and
/// `i64::MIN % -1` is an overflow ERROR (probed:
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
        // The three timestamp shapes, spelled out so the two operand
        // orders are symmetric BY CONSTRUCTION: two instants compare as
        // instants, and a string is coerced — only ever the string side.
        // A text with no reading is NULL, never a lexical comparison:
        // ADR-0017 §2 withdrew that fallback, which invented an ordering
        // `DuckDB` does not have and inverted under `NOT`.
        (EvalValue::Timestamp(a), EvalValue::Timestamp(b)) => EvalValue::Bool(a == b),
        (EvalValue::Timestamp(a), EvalValue::Str(text))
        | (EvalValue::Str(text), EvalValue::Timestamp(a)) => {
            compare::literal_timestamp(text).map_or(EvalValue::Null, |b| EvalValue::Bool(*a == b))
        }
        // cross-type numeric comparison
        _ => match (lhs.as_f64(), rhs.as_f64()) {
            (Some(a), Some(b)) => {
                EvalValue::Bool(compare::double_total_cmp(a, b) == std::cmp::Ordering::Equal)
            }
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
        // `Instant`'s derived order IS DuckDB's TIMESTAMP order, so the
        // infinities sort where the engine sorts them. The string side is
        // coerced and only the string side; no reading is UNKNOWN, not a
        // lexical guess (ADR-0017 §2).
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
            #[allow(clippy::cast_precision_loss)]
            EvalValue::UInt(n) => EvalValue::Float(-(n as f64)),
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
            #[allow(clippy::cast_precision_loss)]
            EvalValue::UInt(n) => EvalValue::Float(*n as f64),
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
        // The spellings `DuckDB` gives the values THIS lane can hold, probed
        // in `trawl-engine/tests/duckdb_probe.rs`
        // (`typeof_spells_a_bound_dsl_literal_by_its_bound_type`). An integer
        // is BIGINT, not INTEGER: the emitter binds every DSL literal as a
        // parameter, and a bound `i64` arrives as BIGINT — `INTEGER` is what a
        // literal written into the SQL TEXT answers, which the batch lane
        // never produces. Two spellings stay divergent on purpose (the NULL
        // type and lists, pinned in `trawl-core/tests/scalar_parity.rs`); #105
        // ruled only on this one.
        "typeof" => args.first().map_or(EvalValue::Null, |v| {
            EvalValue::Str(
                match v {
                    EvalValue::Null => "NULL",
                    EvalValue::Bool(_) => "BOOLEAN",
                    EvalValue::Int(_) => "BIGINT",
                    // A field above `i64::MAX` read as a DOUBLE before
                    // this variant existed, and still does.
                    EvalValue::UInt(_) | EvalValue::Float(_) => "DOUBLE",
                    EvalValue::Str(_) => "VARCHAR",
                    EvalValue::Array(_) => "ARRAY",
                    EvalValue::Timestamp(_) => "TIMESTAMP",
                }
                .to_string(),
            )
        }),
        // now() returns a Timestamp (timezone-naive wall-clock UTC)
        "now" => EvalValue::Timestamp(compare::Instant::At(chrono::Utc::now().naive_utc())),
        // conditional
        "case" => {
            let pairs = args.len() / 2;
            for i in 0..pairs {
                match read_condition(&args[i * 2]) {
                    ConditionRead::True => return Some(args[i * 2 + 1].clone()),
                    // FALSE and NULL alike move to the next arm.
                    ConditionRead::NotTaken => {}
                    // An unreadable condition is NOT "this arm does not
                    // match" — `DuckDB` errors, so the whole call is NULL.
                    // Read in arm ORDER, so an earlier match returns before
                    // this one is ever looked at, which is what `DuckDB`'s
                    // per-arm short circuit does (probed:
                    // `a_case_reads_its_arms_in_order_and_stops_at_the_first_true`).
                    ConditionRead::Unreadable => return Some(EvalValue::Null),
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

        // The severity ladder function (ADR-0013 slice 2, ruling 9) — the
        // in-memory half of the ONE kernel, read straight off the
        // `EvalValue` rather than through a `serde_json` round trip: a
        // string reads as text, an integer as a number in the requested
        // dialect, and every other shape (float, bool, timestamp, array,
        // NULL) has no reading. An unreadable value is `Null`, NEVER
        // `None` — `sev()` is a known function whatever it is handed.
        "sev" => {
            let dialect = match args.get(1) {
                Some(EvalValue::Str(token)) => crate::severity::Dialect::from_token(token),
                // No dialect argument is OTel; a token the compile-time
                // gate would have refused has no reading at all.
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

/// What `DuckDB` makes of an `IF`/`CASE WHEN` condition.
enum ConditionRead {
    /// A true condition: take this branch.
    True,
    /// FALSE or SQL NULL — `DuckDB` treats both as not-taken and moves to
    /// the else branch (or the next `CASE` arm).
    NotTaken,
    /// No boolean reading at all. `DuckDB` raises a Conversion error, so
    /// the whole call is NULL under the ratified
    /// eval-nulls-where-batch-errors rule.
    Unreadable,
}

/// Read a condition the way `DuckDB` casts one — the domain probed by
/// `an_if_condition_is_a_boolean_cast_not_truthiness`, NOT
/// [`EvalValue::is_truthy`].
///
/// Strings go through the one owner of that cast vocabulary
/// ([`crate::compare::try_cast_boolean`]) — closed, case-insensitive and
/// UNTRIMMED, so `' true '` has no reading. Numbers read as `!= 0`,
/// which makes both zeros false and NaN true; a timestamp or a list has
/// no cast to BOOLEAN at all.
fn read_condition(value: &EvalValue) -> ConditionRead {
    let taken = match value {
        EvalValue::Bool(b) => *b,
        EvalValue::Null => return ConditionRead::NotTaken,
        EvalValue::Int(n) => *n != 0,
        EvalValue::UInt(n) => *n != 0,
        // An exact zero test, both signs: `-0.0` is false and NaN — which
        // no ordering comparison would call true — is true.
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

/// `ceil(x)` — DOUBLE whatever the argument was.
///
/// `CEIL` returns DOUBLE over a BIGINT argument as well as over a DOUBLE
/// one (probed: `ceil_floor_and_round_split_their_return_type_on_the_
/// argument_type`), so an integer argument widens rather than passing
/// through, and a fractional one may not be truncated into an `i64` —
/// `ceil(-0.5)` is `-0.0`, a value no integer can carry.
#[allow(clippy::cast_precision_loss)]
fn eval_ceil(args: &[EvalValue]) -> EvalValue {
    args.first().map_or(EvalValue::Null, |v| match v {
        EvalValue::Int(n) => EvalValue::Float(*n as f64),
        EvalValue::UInt(n) => EvalValue::Float(*n as f64),
        EvalValue::Float(n) => EvalValue::Float(n.ceil()),
        _ => EvalValue::Null,
    })
}

/// `floor(x)` — DOUBLE whatever the argument was, exactly like
/// [`eval_ceil`].
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

/// `round(x [, precision])` — the one of the three rounding scalars that
/// KEEPS an integer argument integral.
///
/// `ROUND` over a BIGINT returns BIGINT while `CEIL`/`FLOOR` widen to
/// DOUBLE (probed: `ceil_floor_and_round_split_their_return_type_on_the_
/// argument_type`), so the integer arm passes through unchanged and only
/// the DOUBLE arm — including the precision-0 case, which used to answer
/// an integer — stays DOUBLE.
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

// ── date/time scalar function implementations ──────────────────────

/// `tonumber(x)` — mirrors `TRY_CAST(x AS DOUBLE)`.
#[allow(clippy::cast_precision_loss)]
fn eval_tonumber(args: &[EvalValue]) -> EvalValue {
    match args.first() {
        Some(EvalValue::Int(n)) => EvalValue::Float(*n as f64),
        Some(EvalValue::UInt(n)) => EvalValue::Float(*n as f64),
        Some(EvalValue::Float(n)) => EvalValue::Float(*n),
        // A boolean HAS a DOUBLE reading — `TRY_CAST(true AS DOUBLE)` is
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
    // NULL for an infinity, every unit — probed
    // (`the_date_scalars_answer_for_an_infinity`), and one door rather
    // than eleven arms.
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
        // The one probe-pinned reading, owned by `compare` — this arm
        // used to sum seconds and a fraction and rounded twice.
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
    // An infinity truncates to ITSELF, for every unit (probed) — so the
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
        // hour/minute/second share the truncators used by date_diff.
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
    // NULL whenever EITHER side is infinite, both signs, both
    // positions — probed.
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

/// Rewrite a user's format into the chrono spelling that MEANS what
/// `DuckDB` means by it.
///
/// Exactly one specifier differs in UNIT rather than in syntax: a bare
/// `%f` is a six-digit MICROSECOND field to `DuckDB` (probed:
/// `percent_f_is_six_digit_microseconds`) and an unscaled NANOSECOND
/// count to chrono — nine digits out, and a digit run read as
/// nanoseconds in. chrono's fixed-width `%6f` is the same field
/// `DuckDB` writes, so the translation is `%f` → `%6f` and nothing else.
///
/// `%%` is an ESCAPED percent, not a specifier: the `f` in `%%f` is a
/// literal letter and must survive untouched, which is why this walks
/// the format instead of replacing text.
///
/// This is eval's internal SPELLING of the user's format —
/// `emitter::validate_format_literal` still judges the text the user
/// wrote, since that is the text the batch lane sends to the engine.
///
/// Borrowed when there is nothing to rewrite, which is every format
/// without an `%f` in it.
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
            // The one unit difference.
            Some('f') => out.push_str("%6f"),
            // An escaped percent: both characters are literal, and the
            // NEXT character is not a specifier letter.
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
    // An infinity renders as the WORD for EVERY format (probed), so the
    // format is never applied to one.
    let ts = match args[0].as_instant() {
        Some(compare::Instant::At(at)) => at,
        Some(infinite) => return EvalValue::Str(infinite.cast_text()),
        None => return EvalValue::Null,
    };
    let EvalValue::Str(fmt) = &args[1] else {
        return EvalValue::Null;
    };
    // The chrono spelling of what the user asked for — `%f` means
    // microseconds here, as it does to the engine.
    let fmt = duckdb_strftime_format(fmt);
    let fmt = fmt.as_ref();
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
/// `1900-01-01 00:00:00` base: a year-only format yields `…-01-01 00:00:00`, a
/// date-only format yields midnight, a time-only format yields `1900-01-01`, and
/// a date with an INCOMPLETE time (`%Y-%m-%d %H`) keeps the hour and zero-fills
/// minute/second. We mirror this exactly by parsing once into a
/// `chrono::format::Parsed`, then resolving each half — letting chrono resolve
/// derived fields first (ISO week, ordinal `%j`, `%s` epoch, am/pm) and injecting
/// the `1900-01-01 00:00:00` defaults only for components that are genuinely
/// missing (see `resolve_date`/`resolve_time`).
///
/// `chrono::format::parse` matches the whole input against the whole format, so
/// trailing or insufficient input still rejects (both paths `Null`, as `DuckDB`
/// does), and an unparseable value returns `Null` — matching the `TRY_STRPTIME`
/// the batch emitter uses.
///
/// Codes that interact with the base injection or with time zones can diverge
/// from `DuckDB` and follow chrono per ADR-0001's parity contract: a *bare*
/// two-digit year (`%y` alone) resolves to `Null` — the injected `1900` base year
/// conflicts with the parsed mod-100 — and offset codes (`%z`/`%Z`) keep the
/// wall-clock time chrono parses rather than normalizing to UTC.
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
    // The same translation as `strftime`'s, so one format spells one
    // thing in both directions.
    //
    // RESIDUAL, one-directional: chrono's `%6f` is FIXED width, where
    // `DuckDB` reads a variable-length fraction (`.5` is half a second,
    // probed). A run of other than six digits therefore has no reading
    // here and this returns NULL, where the engine returns the instant —
    // an under-read, replacing a WRONG one (chrono's `%f` read those
    // same digits as nanoseconds, so `.5` used to parse as five
    // nanoseconds). Pinned by
    // `current_strptime_reads_only_a_six_digit_fraction`.
    let fmt = duckdb_strftime_format(fmt);
    let fmt = fmt.as_ref();
    let mut parsed = chrono::format::Parsed::new();
    if chrono::format::parse(&mut parsed, s, chrono::format::StrftimeItems::new(fmt)).is_err() {
        return EvalValue::Null;
    }
    // A fully-specified datetime (or a `%s` epoch) resolves directly.
    if let Ok(dt) = parsed.to_naive_datetime_with_offset(0) {
        return EvalValue::Timestamp(compare::Instant::At(dt));
    }
    match (resolve_date(&mut parsed), resolve_time(&mut parsed)) {
        (Some(date), Some(time)) => EvalValue::Timestamp(compare::Instant::At(date.and_time(time))),
        _ => EvalValue::Null,
    }
}

/// Resolve a `NaiveDate` from a partially-filled `Parsed`, supplying the date
/// components `DuckDB` would take from its `1900-01-01` base. chrono is tried
/// first so derived fields (ISO week, ordinal `%j`) win; the base defaults are
/// then injected most-significant first, retrying after each. `set_*` no-ops when
/// the field already holds a value, so a year-only or ordinal-only input is never
/// over-determined with a stray month/day. `None` means the present fields are
/// contradictory (e.g. Feb 30) — the caller nulls, as `DuckDB` does.
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

/// Resolve a `NaiveTime` from a partially-filled `Parsed`, zero-filling the
/// components a format omits (`DuckDB`'s `00:00:00` base). Mirrors `resolve_date`:
/// chrono first (so am/pm and fractional seconds resolve), then hour→minute→second
/// defaults with a retry after each. `None` means a present time field is out of
/// range (a value chrono itself rejects) — the caller nulls. Note `%I` without
/// `%p` does NOT null: the `set_hour(0)` default supplies the missing am/pm half,
/// so it resolves to the 24-hour reading (matching `DuckDB`).
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

    // ── pinned where/let comparisons (ADR-0011 slice A′) ─────────────

    mod pinned {
        use super::*;
        use crate::parser;
        use crate::pin_scope::PinScope;
        use crate::schema::CanonicalType as CT;
        use crate::schema::FieldTypes;

        /// Evaluate the sole `where` condition of `dsl` against `event`
        /// under `pins`.
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
            eval_expr_with_pins(&cond, &crate::row::from_json(&event), &PinScope::root(&ft))
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
            // No reading → UNKNOWN, not false — `NOT` cannot invert it.
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

        /// Strict null policy: `!=` over an absent/null field is UNKNOWN
        /// (plain SQL null propagation), NOT the search stage's widening.
        #[test]
        fn ne_over_absent_field_is_unknown() {
            for event in ["{}", r#"{"status": null}"#] {
                assert_eq!(
                    eval_where("* | where status != 200", event, VARCHAR_STATUS),
                    EvalValue::Null,
                    "{event}"
                );
            }
            // Present, unreadable value still answers TRUE (text differs).
            assert_eq!(
                eval_where(
                    "* | where status != 200",
                    r#"{"status": "accepted"}"#,
                    VARCHAR_STATUS
                ),
                EvalValue::Bool(true)
            );
        }

        /// A conform-nulled value under a typed pin is a stored NULL:
        /// UNKNOWN for every operator, `!=` included (Strict, unlike the
        /// search lane's `OR col IS NULL`).
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
            // A readable spelling still compares in the pin's domain.
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

        /// Quote provenance is discarded: `status > "400"` IS `status > 400`.
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

        /// Pattern ops against a typed pin match the canonical text form.
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
            // The stored value is the conformed one: "0404" reads 404.
            assert_eq!(
                eval_where(
                    "* | where dur matches \"^4\"",
                    r#"{"dur": "0404"}"#,
                    BIGINT_DUR
                ),
                EvalValue::Bool(true)
            );
            // No reading → NULL target → UNKNOWN.
            assert_eq!(
                eval_where(
                    "* | where dur matches \"^4\"",
                    r#"{"dur": "4.5"}"#,
                    BIGINT_DUR
                ),
                EvalValue::Null
            );
        }

        /// A pinned comparison binds the row's key the way `DuckDB` binds
        /// a column reference — either case direction, because a pipeline
        /// stage can MAKE a mixed-case key (`rename status as St`) that a
        /// later stage names differently (`where st>400`).
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
            // An exact key wins over a case-variant sibling...
            assert_eq!(
                eval_where(
                    "* | where status > 400",
                    r#"{"Status": "500", "status": "200"}"#,
                    VARCHAR_STATUS
                ),
                EvalValue::Bool(false)
            );
            // ...and binding, not non-nullness, picks the key: an exact
            // key holding null is that field's own NULL.
            assert_eq!(
                eval_where(
                    "* | where status > 400",
                    r#"{"Status": "500", "status": null}"#,
                    VARCHAR_STATUS
                ),
                EvalValue::Null
            );
        }

        /// The PIN-BLIND read binds the same way, so a reference does not
        /// change meaning with the pin: `lower(Status)` and a bare
        /// `let b = Status` reach the row's `status` exactly as the pinned
        /// arm beside them does.
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
            // An exact key still wins over a case-variant sibling.
            assert_eq!(
                eval_where(
                    "* | where status == \"exact\"",
                    r#"{"Status": "variant", "status": "exact"}"#,
                    &[]
                ),
                EvalValue::Bool(true)
            );
        }

        /// A VARCHAR pin makes even a numeric wire value pattern-matchable
        /// as its stored text (pin-blind eval answered NULL for non-Str).
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

        /// Unpinned fields and the pin-blind door stay literal-driven: a
        /// string value ordered against an int literal has no numeric
        /// reading there (pre-existing behavior, unchanged by pins).
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
            // The documented pin-blind door: eval_expr never consults
            // pins, so the same comparison the pinned walk answers TRUE
            // stays literal-driven here.
            let query = parser::parse("* | where status > 400").expect("parses");
            let cond = match &query.pipeline[0].node {
                crate::ast::PipeStage::Where(w) => w.condition.clone(),
                _ => unreachable!(),
            };
            let event: Map<String, Value> = serde_json::from_str(r#"{"status": "404"}"#).unwrap();
            assert_eq!(
                eval_expr(&cond, &crate::row::from_json(&event)),
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
    /// Zero aliases (ADR-0013 §6): `@timestamp` reads the key it spells,
    /// and `_time` is not that key.
    fn field_ref_timestamp_is_not_an_alias_for_time() {
        let ev = event(&json!({"_time": "2026-01-01T00:00:00Z"}));
        assert_eq!(eval_expr(&field("@timestamp"), &ev), EvalValue::Null);
        let ev = event(&json!({"@timestamp": "2026-01-01T00:00:00Z"}));
        assert_eq!(
            eval_expr(&field("@timestamp"), &ev),
            EvalValue::Str("2026-01-01T00:00:00Z".to_string())
        );
    }

    #[test]
    fn field_ref_time_alias() {
        let ev = event(&json!({"_time": "2026-01-01T00:00:00Z"}));
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
        // TRUE division: `/` has no integer form in DuckDB.
        let expr = binary(lit_int(10), BinaryOp::Div, lit_int(4));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Float(2.5));
    }

    #[test]
    fn div_by_zero_int() {
        for (dividend, want) in [(10_i64, f64::INFINITY), (-10_i64, f64::NEG_INFINITY)] {
            let expr = binary(lit_int(dividend), BinaryOp::Div, lit_int(0));
            assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Float(want));
        }
        let expr = binary(lit_int(0), BinaryOp::Div, lit_int(0));
        let EvalValue::Float(value) = eval_expr(&expr, &empty_event()) else {
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
            // The one `%` with no integer answer — an ERROR in DuckDB,
            // not the NULL that `% 0` is, but the same NULL here.
            (i64::MIN, BinaryOp::Mod, -1),
        ] {
            let expr = binary(lit_int(lhs), op, lit_int(rhs));
            assert_eq!(
                eval_expr(&expr, &empty_event()),
                EvalValue::Null,
                "{lhs:?} {op:?} {rhs:?}"
            );
        }
    }

    #[test]
    fn nan_compares_as_duckdb_orders_it() {
        // NaN is equal to itself and greater than everything else.
        let nan = || binary(lit_int(0), BinaryOp::Div, lit_int(0));
        for (op, want) in [
            (BinaryOp::Eq, true),
            (BinaryOp::Ne, false),
            (BinaryOp::Gte, true),
            (BinaryOp::Lt, false),
        ] {
            let expr = binary(nan(), op, nan());
            assert_eq!(
                eval_expr(&expr, &empty_event()),
                EvalValue::Bool(want),
                "NaN {op:?} NaN"
            );
        }
        let expr = binary(nan(), BinaryOp::Gt, lit_float(f64::MAX));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
        // …while the two zeros tie.
        let expr = binary(lit_float(-0.0), BinaryOp::Eq, lit_float(0.0));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Bool(true));
    }

    #[test]
    fn div_floats() {
        let expr = binary(lit_float(10.0), BinaryOp::Div, lit_float(4.0));
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Float(2.5));
    }

    #[test]
    fn div_by_zero_float() {
        let expr = binary(lit_float(10.0), BinaryOp::Div, lit_float(0.0));
        assert_eq!(
            eval_expr(&expr, &empty_event()),
            EvalValue::Float(f64::INFINITY)
        );
    }

    #[test]
    fn modulo_by_zero_float() {
        // A DOUBLE operand takes the IEEE path, where `% 0` is NaN.
        let expr = binary(lit_int(5), BinaryOp::Mod, lit_float(0.0));
        let EvalValue::Float(value) = eval_expr(&expr, &empty_event()) else {
            panic!("5 % 0.0 must be a float");
        };
        assert!(value.is_nan());
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
        // Event with far-future timestamp: `_time > now()` should be true.
        let ev = event(&json!({"_time": "2999-12-31 23:59:59"}));
        let expr = binary(field("_time"), BinaryOp::Gt, call("now", vec![]));
        assert_eq!(eval_expr(&expr, &ev), EvalValue::Bool(true));
    }

    #[test]
    fn where_timestamp_gt_now_for_past_event() {
        // Event with past timestamp: `_time > now()` should be false.
        let ev = event(&json!({"_time": "2000-01-01 00:00:00"}));
        let expr = binary(field("_time"), BinaryOp::Gt, call("now", vec![]));
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
            // A stored/computed -0.0 renders SIGNED (only a SQL literal
            // `-0.0` folds to positive zero) — probe-pinned.
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
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Float(2.0));
    }

    #[test]
    fn fn_ceil_widens_an_integer_argument() {
        // CEIL(BIGINT) is DOUBLE in DuckDB, so an integer argument may not
        // pass through as one.
        let expr = call("ceil", vec![lit_int(5)]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Float(5.0));
    }

    #[test]
    fn fn_ceil_keeps_negative_zero() {
        // `-0.0`, the value the retired i64 truncation could not carry.
        let expr = call("ceil", vec![lit_float(-0.5)]);
        let EvalValue::Float(value) = eval_expr(&expr, &empty_event()) else {
            panic!("ceil(-0.5) must be a float");
        };
        assert!(value == 0.0 && value.is_sign_negative(), "{value}");
    }

    #[test]
    fn fn_ceiling_alias() {
        let expr = call("ceiling", vec![lit_float(1.2)]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Float(2.0));
    }

    #[test]
    fn fn_floor() {
        let expr = call("floor", vec![lit_float(1.8)]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Float(1.0));
    }

    #[test]
    fn fn_floor_widens_an_integer_argument() {
        let expr = call("floor", vec![lit_int(-5)]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Float(-5.0));
    }

    #[test]
    fn fn_round_no_precision() {
        // ROUND(DOUBLE) is DOUBLE — only an INTEGER argument stays integral.
        let expr = call("round", vec![lit_float(1.6)]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Float(2.0));
    }

    #[test]
    fn fn_round_explicit_zero_precision_stays_double() {
        let expr = call("round", vec![lit_float(2.5), lit_int(0)]);
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Float(3.0));
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
    fn fn_if_reads_its_condition_as_duckdb_casts_it() {
        for (condition, want) in [
            (lit_str("true"), Some("yes")),
            (lit_str("YES"), Some("yes")),
            // Read as FALSE, where truthiness took the THEN branch.
            (lit_str("0"), Some("no")),
            (lit_str("no"), Some("no")),
            (lit_int(0), Some("no")),
            (lit_int(-1), Some("yes")),
            (lit_float(-0.0), Some("no")),
            (lit_null(), Some("no")),
            // No boolean reading: DuckDB errors, so the call is NULL —
            // NOT the else branch, which would answer a question DuckDB
            // refuses.
            (lit_str("nonempty"), None),
            (lit_str(" true "), None),
            (lit_str(""), None),
            (lit_str("2"), None),
        ] {
            let expr = call("if", vec![condition, lit_str("yes"), lit_str("no")]);
            let expected = want.map_or(EvalValue::Null, |text| EvalValue::Str(text.to_string()));
            assert_eq!(eval_expr(&expr, &empty_event()), expected);
        }
    }

    #[test]
    fn fn_case_stops_at_the_first_true_arm_and_nulls_on_an_unreadable_one() {
        // An earlier match returns before the unreadable arm is read…
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
        assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Int(1));

        // …and an unreadable arm reached in order nulls the WHOLE call,
        // whether the arm before it was false or NULL.
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
            assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Null);
        }
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
            EvalValue::Str("BIGINT".to_string())
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
        assert!(
            ts >= compare::Instant::At(before),
            "now() timestamp should be >= start"
        );
        assert!(
            ts <= compare::Instant::At(after),
            "now() timestamp should be <= end"
        );
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

    /// The syntax the string door reads is `compare::literal_timestamp`'s
    /// (probe-pinned), not a parser of eval's own: these cases used to
    /// exercise the deleted `as_timestamp`, and they hold unchanged
    /// through its replacement.
    #[test]
    fn as_finite_reads_the_literal_timestamp_syntax() {
        for (text, want) in [
            ("2026-01-15T14:30:00Z", "2026-01-15 14:30:00"),
            ("2026-01-15 14:30:00", "2026-01-15 14:30:00"),
            ("2026-01-15", "2026-01-15 00:00:00"),
            // An offset is DISCARDED — the wall-clock cast a bound
            // string parameter gets.
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
            // The MALFORMED offset eval's own parser used to accept by
            // stripping it unvalidated; the engine rejects it.
            "2026-01-15 10:20:30+ab:cd",
            // Byte 'len - 6' lands mid-emoji — the old stripper sliced
            // there and panicked.
            "🦀🦀",
            "err: 🦀🦀",
        ] {
            let value = EvalValue::Str(text.to_string());
            assert!(value.as_finite().is_none(), "{text:?}");
            assert!(value.as_instant().is_none(), "{text:?}");
        }
    }

    /// An INFINITY is an instant but not a finite one — the split every
    /// calendar function reads through.
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
        // The two instants that had no JSON rendering at all before now
        // egress as the words DuckDB casts them to.
        assert_eq!(
            Value::from(EvalValue::Timestamp(compare::Instant::Infinity)),
            json!("infinity")
        );
        assert_eq!(
            Value::from(EvalValue::Timestamp(compare::Instant::NegInfinity)),
            json!("-infinity")
        );
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
    fn fn_tonumber_from_bool() {
        // TRY_CAST(bool AS DOUBLE) has a reading in both directions.
        for (input, want) in [(true, 1.0), (false, 0.0)] {
            let expr = call("tonumber", vec![lit_bool(input)]);
            assert_eq!(eval_expr(&expr, &empty_event()), EvalValue::Float(want));
        }
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
            eval_expr(&expr, &empty_event()),
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
            eval_expr(&expr, &empty_event()),
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
            eval_expr(&expr, &empty_event()),
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
            eval_expr(&expr, &empty_event()),
            EvalValue::Timestamp(compare::Instant::At(ndt("2026-07-13 00:00:00")))
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
            EvalValue::Timestamp(compare::Instant::At(ndt("2026-03-15 10:20:30")))
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

    // Partial formats fill omitted components from DuckDB's 1900-01-01 00:00:00
    // base (parity asserted against real DuckDB in tests/scalar_parity.rs).
    #[test]
    fn fn_strptime_partial_formats_fill_like_duckdb() {
        for (input, fmt, want) in [
            ("2023", "%Y", "2023-01-01 00:00:00"),
            ("2023-11", "%Y-%m", "2023-11-01 00:00:00"),
            ("11-07", "%m-%d", "1900-11-07 00:00:00"),
            ("2023-11-07 14", "%Y-%m-%d %H", "2023-11-07 14:00:00"),
            // unchanged date-only / time-only cases stay exact
            ("2023-11-07", "%Y-%m-%d", "2023-11-07 00:00:00"),
            ("14:30:00", "%H:%M:%S", "1900-01-01 14:30:00"),
        ] {
            let expr = call("strptime", vec![lit_str(input), lit_str(fmt)]);
            assert_eq!(
                eval_expr(&expr, &empty_event()),
                EvalValue::Timestamp(compare::Instant::At(ndt(want))),
                "strptime({input:?}, {fmt:?})"
            );
        }
    }

    /// The date scalars over an infinity, one arm per PROBE row
    /// (`the_date_scalars_answer_for_an_infinity`): `date_part` NULLs for
    /// every unit, `date_trunc` passes the infinity through for every
    /// unit, `date_diff` NULLs from either side, and `strftime` renders
    /// the word whatever the format asks for.
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
                    eval_expr(&expr, &empty_event()),
                    EvalValue::Null,
                    "date_part({unit:?}, {word})"
                );
            }
            for unit in crate::emitter::DATE_UNITS {
                let expr = call("date_trunc", vec![lit_str(unit), ts()]);
                assert_eq!(
                    eval_expr(&expr, &empty_event()),
                    EvalValue::Timestamp(instant),
                    "date_trunc({unit:?}, {word})"
                );
            }
            for fmt in ["%Y-%m-%d %H:%M:%S", "%Y", "%j"] {
                let expr = call("strftime", vec![ts(), lit_str(fmt)]);
                assert_eq!(
                    eval_expr(&expr, &empty_event()),
                    EvalValue::Str(word.to_string()),
                    "strftime({word}, {fmt:?})"
                );
            }
            let finite = || lit_str("2026-01-15 09:00:00");
            for (start, end) in [(ts(), finite()), (finite(), ts()), (ts(), ts())] {
                let expr = call("date_diff", vec![lit_str("day"), start, end]);
                assert_eq!(
                    eval_expr(&expr, &empty_event()),
                    EvalValue::Null,
                    "date_diff over {word}"
                );
            }
        }
    }

    /// `Instant`'s order IS `DuckDB`'s, so an infinity compares rather than
    /// nulling — in both operand orders and against a plain text.
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

    /// The translation rewrites the ONE specifier whose unit differs and
    /// leaves an escaped percent alone.
    #[test]
    fn the_format_translation_touches_only_a_bare_percent_f() {
        for (input, want) in [
            ("%f", "%6f"),
            ("%Y-%m-%d %H:%M:%S.%f", "%Y-%m-%d %H:%M:%S.%6f"),
            // An escaped percent is a literal, so its `f` is a letter.
            ("%%f", "%%f"),
            ("x%%fy", "x%%fy"),
            // …and a real specifier AFTER an escaped one still rewrites.
            ("%%%f", "%%%6f"),
            // Untouched formats come back borrowed.
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

    /// `%f` renders six digits, zero-FILLED — the engine's field, not
    /// chrono's nanosecond count (probe:
    /// `percent_f_is_six_digit_microseconds`).
    #[test]
    fn fn_strftime_percent_f_is_microseconds() {
        for (text, want) in [
            ("2026-01-15 09:00:00.123456", "123456"),
            ("2026-01-15 09:00:00.500000", "500000"),
            ("2026-01-15 09:00:00", "000000"),
        ] {
            let ts = EvalValue::Timestamp(compare::Instant::At(ndt(text)));
            let answer = eval_scalar_fn("strftime", &[ts, EvalValue::Str("%f".into())]);
            assert_eq!(answer, Some(EvalValue::Str(want.to_string())), "{text:?}");
        }
    }

    /// The one-directional residual the fixed-width mirror leaves: a
    /// fraction of other than six digits has no reading here, where the
    /// engine reads it. It replaces a WRONG answer — chrono's `%f` read
    /// those digits as nanoseconds, so `.5` parsed as five of them — and
    /// is not scheduled to flip in #105.
    #[test]
    fn current_strptime_reads_only_a_six_digit_fraction() {
        let parse = |text: &str| {
            eval_scalar_fn(
                "strptime",
                &[
                    EvalValue::Str(text.to_string()),
                    EvalValue::Str("%Y-%m-%d %H:%M:%S.%f".into()),
                ],
            )
        };
        assert_eq!(
            parse("2026-01-15 09:00:00.500000"),
            Some(EvalValue::Timestamp(compare::Instant::At(ndt(
                "2026-01-15 09:00:00.5"
            )))),
            "six digits read exactly"
        );
        // DuckDB reads these as .5 and .123; the mirror under-reads.
        for text in ["2026-01-15 09:00:00.5", "2026-01-15 09:00:00.123"] {
            assert_eq!(parse(text), Some(EvalValue::Null), "{text:?}");
        }
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

    // ── sev(): the ladder function (ADR-0013 slice 2, ruling 9) ────

    /// The eval lane reads the SAME kernel the SQL lane's expression is
    /// generated from, straight off the `EvalValue`.
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
            // No reading is NULL, never an error and never a guess.
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
                eval_expr(&call("sev", vec![lit_str(text)]), &empty_event()),
                expected,
                "sev({text:?})"
            );
        }
        assert_eq!(
            eval_expr(&call("sev", vec![lit_int(17)]), &empty_event()),
            EvalValue::Int(17)
        );
        assert_eq!(
            eval_expr(&call("sev", vec![lit_int(25)]), &empty_event()),
            EvalValue::Null
        );
        // A shape that names no rung — a float, a boolean, a missing
        // field — has no reading.
        assert_eq!(
            eval_expr(&call("sev", vec![lit_float(17.0)]), &empty_event()),
            EvalValue::Null
        );
        assert_eq!(
            eval_expr(&call("sev", vec![lit_bool(true)]), &empty_event()),
            EvalValue::Null
        );
        assert_eq!(
            eval_expr(&call("sev", vec![field("level")]), &empty_event()),
            EvalValue::Null
        );
    }

    /// The dialect governs NUMERICS alone: syslog inverts 0-7, and a word
    /// reads the same in both.
    #[test]
    fn fn_sev_dialect_inverts_numerics_only() {
        let sev = |arg: Spanned<Expr>, dialect: &str| {
            eval_expr(&call("sev", vec![arg, lit_str(dialect)]), &empty_event())
        };
        assert_eq!(sev(lit_int(3), "syslog"), EvalValue::Int(17));
        assert_eq!(sev(lit_int(3), "otel"), EvalValue::Int(3));
        assert_eq!(sev(lit_str("3"), "syslog"), EvalValue::Int(17));
        assert_eq!(sev(lit_int(0), "syslog"), EvalValue::Int(24));
        assert_eq!(sev(lit_int(8), "syslog"), EvalValue::Null);
        assert_eq!(sev(lit_str("error"), "syslog"), EvalValue::Int(17));
        // Case-insensitive, and an unknown dialect reads nothing (the
        // stream compiler refuses it before an event ever arrives).
        assert_eq!(sev(lit_int(3), "SYSLOG"), EvalValue::Int(17));
        assert_eq!(sev(lit_int(3), "rfc5424"), EvalValue::Null);
    }

    /// Reading a real event column, in both wire shapes.
    #[test]
    fn fn_sev_over_event_columns() {
        let ev = event(&json!({"level": "warn", "syslog_severity": 3}));
        assert_eq!(
            eval_expr(&call("sev", vec![field("level")]), &ev),
            EvalValue::Int(13)
        );
        assert_eq!(
            eval_expr(
                &call("sev", vec![field("syslog_severity"), lit_str("syslog")]),
                &ev
            ),
            EvalValue::Int(17)
        );
    }

    // coverage test — every non-aggregate KNOWN_FUNCTION must return Some(_)
    // from eval_scalar_fn. Fails CI if a scalar is added to the emitter but not eval.
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
