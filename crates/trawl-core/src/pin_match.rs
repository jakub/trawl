// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The shared in-memory pin-aware comparison core (ADR-0011).

use serde_json::Value;

use crate::compare;
use crate::compare::{CompareForm, PatternForm};
use crate::emitter::SqlValue;
use crate::schema::CanonicalType;

/// One SQL truth value: `Some(true)`, `Some(false)`, or `None` = UNKNOWN.
pub(crate) type Truth = Option<bool>;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompareOp {
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
}

impl CompareOp {
    pub(crate) fn from_filter(op: crate::ast::FilterOp) -> Self {
        use crate::ast::FilterOp;
        match op {
            FilterOp::Eq => Self::Eq,
            FilterOp::Ne => Self::Ne,
            FilterOp::Gt => Self::Gt,
            FilterOp::Gte => Self::Gte,
            FilterOp::Lt => Self::Lt,
            FilterOp::Lte => Self::Lte,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum NullReadPolicy {
    NeMatches,
    Unknown,
}

#[derive(Clone, Debug)]
pub(crate) enum CoercedValue {
    Int(i64),
    Float(f64),
    Str(String),
    /// The literal's reading in the one comparison space, scaled by 10^6
    /// and exact. `None` is a LITERAL with no reading: UNKNOWN, not FALSE.
    NumericOnText(Option<i128>),
    /// A number's STORED text is `read_json`'s inference rendered — `"200.0"`
    /// for a wire `200` that shared a batch with a fractional value — while
    /// this matcher only ever sees the wire JSON.
    TextOrNumeric {
        text: String,
        number: Option<i128>,
    },
    /// The typed-pin form: the column the SQL compares is the conformed
    /// one, so this matcher reads the wire value's own conformed value
    /// first and compares that, in the pin's domain.
    Conformed {
        pin: CanonicalType,
        literal: PinLiteral,
    },
    SeverityBand {
        lo: u8,
        hi: u8,
    },
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum PinLiteral {
    Int(i64),
    Double(f64),
    Bool(bool),
    Time(compare::Instant),
    /// `DuckDB` cannot compare this literal with the pinned column and raises
    /// an ERROR, which returns no rows. UNKNOWN matches nothing, and `NOT`
    /// cannot invert it into a match the failed query never had.
    Unreadable,
}

pub(crate) fn coerce_form(form: CompareForm) -> CoercedValue {
    match form {
        CompareForm::Native(SqlValue::Int(i)) => CoercedValue::Int(i),
        CompareForm::Native(SqlValue::Float(f)) => CoercedValue::Float(f),
        CompareForm::NumericOnText(literal) => {
            CoercedValue::NumericOnText(compare::decimal_micros(&literal))
        }
        CompareForm::TextOrNumeric(literal) => CoercedValue::TextOrNumeric {
            number: compare::decimal_micros(&literal),
            text: literal,
        },
        CompareForm::Native(SqlValue::String(s)) | CompareForm::Text(s) => CoercedValue::Str(s),
        // coerce_filter_value never yields Bool; keep the match total.
        CompareForm::Native(SqlValue::Bool(b)) => CoercedValue::Str(b.to_string()),
        // Nor a Timestamp: the emitter mints `SqlValue::Timestamp` for
        // exactly one thing — `now()`'s statement anchor (ADR-0017 §3) —
        // and an anchor is never a comparison literal.
        CompareForm::Native(SqlValue::Timestamp(_)) => {
            debug_assert!(false, "an anchor is not a comparison literal");
            CoercedValue::Conformed {
                pin: CanonicalType::Timestamp,
                literal: PinLiteral::Unreadable,
            }
        }
        CompareForm::Conformed { pin, literal } => CoercedValue::Conformed {
            pin,
            literal: pin_literal(pin, &literal),
        },
        CompareForm::SeverityBand { lo, hi } => CoercedValue::SeverityBand { lo, hi },
        CompareForm::SeverityExact(n) => CoercedValue::Conformed {
            pin: CanonicalType::Severity,
            literal: PinLiteral::Int(n),
        },
    }
}

fn pin_literal(pin: CanonicalType, literal: &SqlValue) -> PinLiteral {
    match (pin, literal) {
        // Numeric columns take numeric literals directly, and DuckDB
        // promotes BIGINT to DOUBLE to meet a fractional one (so an id
        // above 2^53 collapses onto its neighbour — in both engines
        // alike, which is why the comparison stays here rather than in
        // the exact DECIMAL space the VARCHAR rungs use).
        // A BOOLEAN column meeting a number casts ITSELF to the number,
        // so the three numeric pins take a numeric literal alike.
        (
            CanonicalType::BigInt
            | CanonicalType::Double
            | CanonicalType::Boolean
            | CanonicalType::Severity,
            SqlValue::Int(i),
        ) => PinLiteral::Int(*i),
        (
            CanonicalType::BigInt
            | CanonicalType::Double
            | CanonicalType::Boolean
            | CanonicalType::Severity,
            SqlValue::Float(f),
        ) => PinLiteral::Double(*f),
        // A BOOLEAN column casts a string literal through the WIDE
        // vocabulary: `TRUE`, `yes` and `1` all match a stored `true`,
        // where the same texts STORED conform to NULL.
        (CanonicalType::Boolean, SqlValue::String(s)) => {
            compare::try_cast_boolean(s).map_or(PinLiteral::Unreadable, PinLiteral::Bool)
        }
        (CanonicalType::Boolean, SqlValue::Bool(b)) => PinLiteral::Bool(*b),
        // A TIMESTAMP column casts a string literal WALL-CLOCK, so an
        // offset spelled in the literal is ignored where the same offset
        // in a STORED value shifts the instant.
        (CanonicalType::Timestamp, SqlValue::String(s)) => {
            compare::literal_timestamp(s).map_or(PinLiteral::Unreadable, PinLiteral::Time)
        }
        (CanonicalType::Varchar, _) => {
            debug_assert!(false, "the VARCHAR pin takes the text rules, not a conform");
            PinLiteral::Unreadable
        }
        _ => PinLiteral::Unreadable,
    }
}

pub(crate) fn and_all(items: impl IntoIterator<Item = Truth>) -> Truth {
    let mut unknown = false;
    for item in items {
        match item {
            Some(false) => return Some(false),
            None => unknown = true,
            Some(true) => {}
        }
    }
    if unknown { None } else { Some(true) }
}

pub(crate) fn or_any(items: impl IntoIterator<Item = Truth>) -> Truth {
    let mut unknown = false;
    for item in items {
        match item {
            Some(true) => return Some(true),
            None => unknown = true,
            Some(false) => {}
        }
    }
    if unknown { None } else { Some(false) }
}

pub(crate) fn compare_values(
    event_val: &Value,
    op: CompareOp,
    filter_val: &CoercedValue,
    null_read: NullReadPolicy,
) -> Truth {
    match filter_val {
        CoercedValue::Int(fv) => {
            if let Some(ev) = extract_i64(event_val) {
                Some(apply_ord(ev.cmp(fv), op))
            } else if let Some(ev) = extract_f64(event_val) {
                #[allow(clippy::cast_precision_loss)]
                Some(apply_f64(ev, *fv as f64, op))
            } else {
                Some(false)
            }
        }
        CoercedValue::Float(fv) => {
            if let Some(ev) = extract_f64(event_val) {
                Some(apply_f64(ev, *fv, op))
            } else {
                Some(false)
            }
        }
        // The TRY_CAST rung: the column is VARCHAR under the pin, so the
        // batch side casts the value's TEXT form — and DuckDB's cast
        // domain is wider than Rust's number parser (whitespace, `_`
        // separators, `0404`), which `compare::decimal_micros` mirrors, or
        // the stream silently drops rows the batch query returns. A text
        // outside the domain is NULL, so UNKNOWN — the one place a
        // non-null event value can still be UNKNOWN — and a LITERAL
        // outside it (`fv` is `None`) makes every row UNKNOWN, exactly as
        // the SQL's NULL right-hand side does.
        CoercedValue::NumericOnText(fv) => (*fv).and_then(|literal| {
            compare::decimal_micros(&json_to_string(event_val))
                .map(|ev| apply_ord(ev.cmp(&literal), op))
        }),
        CoercedValue::Str(fv) => {
            let ev = json_to_string(event_val);
            Some(apply_ord(ev.as_str().cmp(fv.as_str()), op))
        }
        CoercedValue::Conformed { pin, literal } => match conformed_reading(event_val, *pin) {
            Some(reading) => compare_conformed(reading, op, *literal),
            None => match null_read {
                NullReadPolicy::NeMatches => (op == CompareOp::Ne).then_some(true),
                NullReadPolicy::Unknown => None,
            },
        },
        CoercedValue::SeverityBand { lo, hi } => {
            match conformed_reading(event_val, CanonicalType::Severity) {
                Some(Conformed::Severity(n)) => {
                    let inside = n >= *lo && n <= *hi;
                    Some(if op == CompareOp::Ne { !inside } else { inside })
                }
                _ => match null_read {
                    NullReadPolicy::NeMatches => (op == CompareOp::Ne).then_some(true),
                    NullReadPolicy::Unknown => None,
                },
            }
        }
        // Both arms are total here, mirroring the SQL's COALESCEs: a value
        // with no numeric reading falls back to the text answer alone
        // rather than poisoning the predicate with UNKNOWN.
        CoercedValue::TextOrNumeric { text, number } => {
            let ev = json_to_string(event_val);
            let text_eq = ev.as_str() == text.as_str();
            let number_eq =
                number.and_then(|literal| compare::decimal_micros(&ev).map(|read| read == literal));
            match op {
                CompareOp::Eq => Some(text_eq || number_eq.unwrap_or(false)),
                CompareOp::Ne => Some(!text_eq && number_eq.is_none_or(|eq| !eq)),
                ordered => {
                    debug_assert!(false, "TextOrNumeric is an equality-class form");
                    Some(apply_ord(ev.as_str().cmp(text.as_str()), ordered))
                }
            }
        }
    }
}

pub(crate) fn extract_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

pub(crate) fn extract_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug)]
enum Conformed {
    Int(i64),
    Double(f64),
    Bool(bool),
    Time(compare::Instant),
    /// A `SeverityNumber` in 1-24 (ADR-0013): stored as a `BIGINT`, but
    /// rendered as its `OTel` short name.
    Severity(u8),
}

impl Conformed {
    fn text(self) -> String {
        match self {
            Self::Int(i) => i.to_string(),
            Self::Double(d) => compare::canonical_double_text(d),
            Self::Bool(b) => b.to_string(),
            Self::Time(t) => t.pattern_text(),
            Self::Severity(n) => crate::severity::otel_name(n)
                .expect("a conformed severity is in the ladder")
                .to_owned(),
        }
    }
}

/// Conform one wire value to a pin, exactly as the batch lanes conform the
/// column it lands in.
fn conformed_reading(v: &Value, pin: CanonicalType) -> Option<Conformed> {
    match pin {
        CanonicalType::BigInt => match v {
            // A JSON integer is exact and needs no guard; every other
            // number is read through the text the conform would see.
            Value::Number(n) => n.as_i64().or_else(|| {
                n.as_f64()
                    .and_then(|f| compare::conformed_bigint(&compare::canonical_double_text(f)))
            }),
            Value::String(s) => compare::conformed_bigint(s),
            _ => None,
        }
        .map(Conformed::Int),
        CanonicalType::Double => match v {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => compare::try_cast_double(s),
            _ => None,
        }
        .map(Conformed::Double),
        CanonicalType::Boolean => match v {
            Value::Bool(b) => Some(*b),
            Value::String(s) => compare::conformed_boolean(s),
            _ => None,
        }
        .map(Conformed::Bool),
        CanonicalType::Timestamp => match v {
            Value::String(s) => compare::conformed_timestamp(s),
            _ => None,
        }
        .map(Conformed::Time),
        CanonicalType::Severity => {
            crate::severity::reading(v, crate::severity::Dialect::Otel).map(Conformed::Severity)
        }
        CanonicalType::Varchar => None,
    }
}

/// A SEVERITY reading is the `SeverityNumber` alone, whose name only a
/// severity column's renderer restores, so it has no cell here.
pub(crate) fn conformed_cell(v: &Value, pin: CanonicalType) -> Option<crate::eval::EvalValue> {
    use crate::eval::EvalValue;
    Some(match conformed_reading(v, pin)? {
        Conformed::Int(i) => EvalValue::Int(i),
        Conformed::Double(d) => EvalValue::Float(d),
        Conformed::Bool(b) => EvalValue::Bool(b),
        Conformed::Time(t) => EvalValue::Timestamp(t),
        Conformed::Severity(_) => return None,
    })
}

#[allow(clippy::cast_precision_loss)] // mirrors DuckDB's own BIGINT → DOUBLE promotion
fn compare_conformed(reading: Conformed, op: CompareOp, literal: PinLiteral) -> Truth {
    match (reading, literal) {
        (Conformed::Int(value), PinLiteral::Int(lit)) => Some(apply_ord(value.cmp(&lit), op)),
        (Conformed::Int(value), PinLiteral::Double(lit)) => Some(apply_f64(value as f64, lit, op)),
        (Conformed::Double(value), PinLiteral::Int(lit)) => Some(apply_f64(value, lit as f64, op)),
        (Conformed::Double(value), PinLiteral::Double(lit)) => Some(apply_f64(value, lit, op)),
        (Conformed::Bool(value), PinLiteral::Bool(lit)) => Some(apply_ord(value.cmp(&lit), op)),
        (Conformed::Bool(value), PinLiteral::Int(lit)) => {
            Some(apply_ord(i64::from(value).cmp(&lit), op))
        }
        (Conformed::Bool(value), PinLiteral::Double(lit)) => {
            Some(apply_f64(f64::from(u8::from(value)), lit, op))
        }
        (Conformed::Time(value), PinLiteral::Time(lit)) => Some(apply_ord(value.cmp(&lit), op)),
        (Conformed::Severity(value), PinLiteral::Int(lit)) => {
            Some(apply_ord(i64::from(value).cmp(&lit), op))
        }
        (Conformed::Severity(value), PinLiteral::Double(lit)) => {
            Some(apply_f64(f64::from(value), lit, op))
        }
        (_, PinLiteral::Unreadable) => None,
        (_, _) => {
            debug_assert!(false, "pin_literal resolves a literal per pin");
            None
        }
    }
}

pub(crate) fn pattern_text(v: &Value, form: PatternForm) -> Option<String> {
    let pin = match form {
        PatternForm::Native => return Some(json_to_string(v)),
        PatternForm::BigIntText => CanonicalType::BigInt,
        PatternForm::DoubleText => CanonicalType::Double,
        PatternForm::BooleanText => CanonicalType::Boolean,
        PatternForm::Rfc3339Text => CanonicalType::Timestamp,
        PatternForm::SeverityText => CanonicalType::Severity,
    };
    conformed_reading(v, pin).map(Conformed::text)
}

pub(crate) fn json_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

pub(crate) fn apply_ord(ord: std::cmp::Ordering, op: CompareOp) -> bool {
    match op {
        CompareOp::Eq => ord.is_eq(),
        CompareOp::Ne => !ord.is_eq(),
        CompareOp::Gt => ord.is_gt(),
        CompareOp::Gte => ord.is_ge(),
        CompareOp::Lt => ord.is_lt(),
        CompareOp::Lte => ord.is_le(),
    }
}

/// Not Rust's IEEE operators: `DuckDB` orders DOUBLE totally, so every
/// NaN is equal to every other NaN and greater than every real value.
fn apply_f64(a: f64, b: f64, op: CompareOp) -> bool {
    apply_ord(compare::double_total_cmp(a, b), op)
}
