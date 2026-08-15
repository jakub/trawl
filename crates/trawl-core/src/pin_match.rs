// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The shared in-memory pin-aware comparison core (ADR-0011 slice A′).
//!
//! One rule table ([`crate::compare`]) resolves how a field-vs-literal
//! comparison binds under a catalog pin; this module EVALUATES the resolved
//! form against a live JSON value, mirroring what the emitted SQL answers
//! over the conformed column. Two consumers: the search-stage matcher
//! ([`crate::filter`]) and the pipeline expression evaluator
//! ([`crate::eval`]) — the semantics exist in exactly two places, the SQL
//! the emitter renders and this one Rust mirror, proven equivalent by the
//! parity suites.
//!
//! Two boundaries are deliberately NOT in here (the slice-A′ prep rulings):
//!
//! - [`crate::compare::CompareForm::Native`] never reaches the shared
//!   apply — the lanes legitimately bind different literals (search
//!   coerces text, `| where` binds the parsed AST literal).
//! - The caller decides what a NULL/absent value answers BEFORE calling
//!   in: the search stage's `!=` carries `OR field IS NULL` while the
//!   pipeline's must not (plain SQL null propagation), so NULL policy
//!   stays per-lane.

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
    /// The comparison subset of [`crate::ast::FilterOp`]; `None` for the
    /// pattern operators, which never reach the comparison core.
    pub(crate) fn from_filter(op: crate::ast::FilterOp) -> Option<Self> {
        use crate::ast::FilterOp;
        match op {
            FilterOp::Eq => Some(Self::Eq),
            FilterOp::Ne => Some(Self::Ne),
            FilterOp::Gt => Some(Self::Gt),
            FilterOp::Gte => Some(Self::Gte),
            FilterOp::Lt => Some(Self::Lt),
            FilterOp::Lte => Some(Self::Lte),
            FilterOp::Glob | FilterOp::Regex => None,
        }
    }
}

/// What a stored NULL — a value the conform's round-trip guard nulled out
/// — answers under `!=`. The same per-lane split as the SQL side's
/// `emitter::compare::NullPolicy`: the search stage's emitted `!=` carries
/// `OR col IS NULL` (a stored NULL matches), while the pipeline's keeps
/// plain SQL null propagation (UNKNOWN for every operator).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum NullReadPolicy {
    /// Search-stage rule: a stored NULL matches `!=`.
    NeMatches,
    /// Pipeline rule: a stored NULL is UNKNOWN, `!=` included.
    Unknown,
}

/// A filter value coerced to the most specific numeric type.
///
/// Mirrors the coercion in `emitter::fields::coerce_filter_value()`, plus
/// the pinned form that binds differently ([`CoercedValue::NumericOnText`]).
#[derive(Clone, Debug)]
pub(crate) enum CoercedValue {
    Int(i64),
    Float(f64),
    Str(String),
    /// The VARCHAR-pinned ordered-numeric form: the literal's reading in
    /// the one comparison space ([`crate::conform::decimal_reading`]),
    /// scaled by 10^6 and exact — so a stored value with no reading is
    /// NULL (UNKNOWN, not FALSE), and so is a LITERAL with none, which is
    /// what `None` here means. Kept apart from [`CoercedValue::Float`] so
    /// the unpinned literal-driven path stays byte-identical.
    NumericOnText(Option<i128>),
    /// The VARCHAR-pinned equality form for a numeric literal: the SQL
    /// side is `(col = ? OR COALESCE(dec(col) = dec(?), FALSE))` (and its
    /// complement for `!=`), because a number's STORED text is
    /// `read_json`'s inference rendered — `"200.0"` for a wire `200` that
    /// shared a batch with a fractional value — while this matcher only
    /// ever sees the wire JSON. The numeric reading is the half the two
    /// sides can agree on; see [`crate::compare`].
    TextOrNumeric {
        text: String,
        number: Option<i128>,
    },
    /// The TYPED-pin form: the column the SQL compares is the CONFORMED
    /// one, so this matcher reads the wire value's own conformed value
    /// first and compares that, in the pin's domain.
    ///
    /// Without it every value the round-trip guard nulls out answered
    /// differently on the two sides — a wire `1.5` under a BIGINT pin is
    /// NULL in both batch lanes, so `duration>1` is UNKNOWN there and was
    /// TRUE here.
    Conformed {
        pin: CanonicalType,
        literal: PinLiteral,
    },
}

/// A literal read into a TYPED pin's own domain, once per compiled filter.
///
/// The emitter binds the literal exactly as the unpinned path does and
/// leaves the cast to `DuckDB`, which resolves it against the pinned
/// COLUMN's type — a string literal against a BOOLEAN column takes the
/// cast's wide vocabulary (`flag=TRUE` matches), and against a TIMESTAMP
/// column the wall-clock parse (`compare::literal_timestamp`). All probed.
#[derive(Clone, Copy, Debug)]
pub(crate) enum PinLiteral {
    Int(i64),
    Double(f64),
    Bool(bool),
    Time(compare::Instant),
    /// `DuckDB` cannot compare the two at all — a word against a numeric
    /// column, a number against a TIMESTAMP one — and raises a conversion
    /// or binder ERROR, which returns no rows at all. UNKNOWN is the
    /// closest total answer: it matches nothing, and `NOT` cannot invert
    /// it into a match the failed query never had.
    Unreadable,
}

/// Map a resolved [`CompareForm`] onto the matcher's coercion vocabulary.
///
/// One rule table, two consumers (ADR-0011 slice A): [`crate::compare`]
/// decides how the literal binds, this translates the decision into the
/// evaluator's terms:
///
/// - `Native` — today's literal-driven coercion, verbatim.
/// - `Text` — string comparison against the event value's text form,
///   mirroring the SQL side's `col = '200'` on the VARCHAR column.
/// - `NumericOnText` — comparison over the value's text form in the one
///   comparison space, read through [`compare::decimal_micros`] so the
///   domain is `DuckDB`'s cast domain; a value outside it mirrors
///   `TRY_CAST(col AS DECIMAL(38,6))` degrading to NULL: UNKNOWN, so `NOT`
///   leaves it unmatched.
/// - `TextOrNumeric` — the equality-class form under a VARCHAR pin: the
///   value's text form OR its DECIMAL reading, mirroring the SQL side's
///   `COALESCE`d two-armed predicate.
///
/// The literal's own reading is taken HERE, once per compiled filter,
/// from the same text the SQL binds — the batch side casts that string
/// with the identical expression, so a literal the space cannot read
/// (`nan`, `1e40`) is `None` on both sides rather than a special case on
/// either.
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
        CompareForm::Conformed { pin, literal } => CoercedValue::Conformed {
            pin,
            literal: pin_literal(pin, &literal),
        },
    }
}

/// Read a bound literal the way `DuckDB` reads it against a column of the
/// pinned type — the other half of [`CoercedValue::Conformed`].
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
        // Everything else is a comparison DuckDB refuses outright: a word
        // against a numeric column, a number against a TIMESTAMP one.
        (CanonicalType::Varchar, _) => {
            debug_assert!(false, "the VARCHAR pin takes the text rules, not a conform");
            PinLiteral::Unreadable
        }
        _ => PinLiteral::Unreadable,
    }
}

/// SQL `AND` over a sequence: FALSE if any FALSE, else UNKNOWN if any
/// UNKNOWN, else TRUE. Short-circuits on the first FALSE, like `all()`.
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

/// SQL `OR` over a sequence: TRUE if any TRUE, else UNKNOWN if any
/// UNKNOWN, else FALSE. Short-circuits on the first TRUE, like `any()`.
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

/// Compare a non-null JSON event value against a coerced filter value.
///
/// Implements type promotion matching `DuckDB`'s implicit casting:
/// - Int filter: try to extract event value as i64 (number or string parse)
/// - Float filter: try to extract event value as f64
/// - String filter: compare as strings (convert event value to string if needed)
/// - `NumericOnText` filter: the DECIMAL(38,6) reading of the value's
///   text form — a text outside `DuckDB`'s cast domain is NULL, so the
///   comparison is UNKNOWN
/// - `TextOrNumeric` filter: the value's text form OR that same DECIMAL
///   reading, both `COALESCE`d exactly as the SQL is
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
                // Promote filter value to f64 for mixed comparison.
                #[allow(clippy::cast_precision_loss)]
                Some(apply_f64(ev, *fv as f64, op))
            } else {
                // String filter value that happened to parse as int —
                // fall back to string comparison.
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
        // The typed-pin rung: the SQL compares the CONFORMED column, so
        // read the wire value's conformed value and compare THAT. A value
        // with no reading is the NULL the guard wrote — the caller's lane
        // decides what a stored NULL answers under `!=` (the search
        // stage's emitted form carries `OR col IS NULL`; the pipeline's
        // does not), so it answers exactly as an absent key does there.
        CoercedValue::Conformed { pin, literal } => match conformed_reading(event_val, *pin) {
            Some(reading) => compare_conformed(reading, op, *literal),
            None => match null_read {
                NullReadPolicy::NeMatches => (op == CompareOp::Ne).then_some(true),
                NullReadPolicy::Unknown => None,
            },
        },
        // The two-armed equality rung. The wire text is only the stored
        // text when `read_json` did not widen the column, so the numeric
        // reading carries the cases where it did (`200` stored `"200.0"`).
        // Both arms are total here, mirroring the SQL's COALESCEs: a value
        // with no numeric reading falls back to the text answer alone
        // rather than poisoning the predicate with UNKNOWN.
        CoercedValue::TextOrNumeric { text, number } => {
            let ev = json_to_string(event_val);
            let text_eq = ev.as_str() == text.as_str();
            // UNKNOWN when EITHER side has no reading, which is what
            // `dec(col) = dec(?)` answers when either cast is NULL.
            let number_eq =
                number.and_then(|literal| compare::decimal_micros(&ev).map(|read| read == literal));
            match op {
                CompareOp::Eq => Some(text_eq || number_eq.unwrap_or(false)),
                CompareOp::Ne => Some(!text_eq && number_eq.is_none_or(|eq| !eq)),
                // Ordered ops never resolve to this form (`compare_form`
                // sends them to `NumericOnText`); fall back to the text
                // comparison rather than inventing an ordering.
                ordered => {
                    debug_assert!(false, "TextOrNumeric is an equality-class form");
                    Some(apply_ord(ev.as_str().cmp(text.as_str()), ordered))
                }
            }
        }
    }
}

/// Try to extract an i64 from a JSON value (number or parseable string).
pub(crate) fn extract_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Try to extract an f64 from a JSON value (number or parseable string).
pub(crate) fn extract_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// What a TYPED-pinned column HOLDS for one wire value — the reading
/// [`crate::conform`] stored, which is what a comparison and a pattern
/// must both see.
///
/// `None` is the NULL the guard wrote: the value is in `_raw`, the column
/// is empty, and every comparison over it is UNKNOWN.
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
    /// The conformed value's own text — `CAST(col AS VARCHAR)` on the SQL
    /// side for the three scalar pins, `strftime` for TIMESTAMP, the
    /// `OTel` short-name table for SEVERITY.
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
///
/// Conformance casts the value's TEXT form under a round-trip guard, so
/// the readings here are text readings too, and each is total on exactly
/// the shapes that guard admits:
///
/// - DOUBLE: a JSON number is its own double (the conform is the bare cast
///   — the round trip through text is the identity), and a JSON string
///   goes through `DuckDB`'s cast domain ([`compare::try_cast_double`], so
///   `"200"` is the same `200.0` the conform wrote);
/// - BIGINT: a JSON integer is itself, and every other numeric or string
///   shape goes through the guarded reading
///   ([`compare::conformed_bigint`], so `"0404"` reads `404` while
///   `"1.5"` and a fractional JSON number have no reading at all — the
///   cast would round them, so the conform stores NULL);
/// - BOOLEAN: a JSON bool is itself (`to_json` renders it as the very text
///   the guard demands), and a string must BE `true`/`false`
///   ([`compare::conformed_boolean`], so `"TRUE"` has no reading);
/// - TIMESTAMP: only a JSON string has a reading at all — an epoch numeral
///   is not a timestamp to any cast, probed — and it goes through
///   [`compare::conformed_timestamp`], which APPLIES a zone offset the way
///   the conform's `TIMESTAMPTZ` rung does (ADR-0011 ruling #1), so
///   `"…T09:00:00+05:30"` reads `03:30`, the hour the corpus holds.
///
/// A JSON bool under a numeric pin, and a number under the BOOLEAN pin,
/// have no reading either: `'true'` is not a number to any cast, and the
/// BOOLEAN cast's vocabulary stops at `1`/`0`, so `'200'` is NULL to it.
/// (Before the conform went text-first, a numeric under a BOOLEAN pin read
/// TRUE in a JSON-inferred hot column and NULL everywhere else — the
/// state-dependence ADR-0011 removed.) An array or object — stringified at
/// ingest, so never a pinned column's live shape — is likewise NULL.
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
        // The SEVERITY pin is the BIGINT reading inside the 1-24 ladder
        // guard — a number outside it conforms to NULL, exactly as the
        // corpus holds it.
        CanonicalType::Severity => match v {
            Value::Number(n) => n
                .as_i64()
                .map(|i| i.to_string())
                .or_else(|| n.as_f64().map(compare::canonical_double_text))
                .and_then(|t| compare::conformed_severity(&t)),
            Value::String(s) => compare::conformed_severity(s),
            _ => None,
        }
        .map(Conformed::Severity),
        // The VARCHAR pin has no conform — its comparisons take the text
        // rules and its patterns match the column directly.
        CanonicalType::Varchar => None,
    }
}

/// Compare a conformed reading against the literal in the pin's own
/// domain — the promotions `DuckDB` performs between the pinned COLUMN's
/// type and the bound parameter's, each one probed.
#[allow(clippy::cast_precision_loss)] // mirrors DuckDB's own BIGINT → DOUBLE promotion
fn compare_conformed(reading: Conformed, op: CompareOp, literal: PinLiteral) -> Truth {
    match (reading, literal) {
        (Conformed::Int(value), PinLiteral::Int(lit)) => Some(apply_ord(value.cmp(&lit), op)),
        (Conformed::Int(value), PinLiteral::Double(lit)) => Some(apply_f64(value as f64, lit, op)),
        (Conformed::Double(value), PinLiteral::Int(lit)) => Some(apply_f64(value, lit as f64, op)),
        (Conformed::Double(value), PinLiteral::Double(lit)) => Some(apply_f64(value, lit, op)),
        (Conformed::Bool(value), PinLiteral::Bool(lit)) => Some(apply_ord(value.cmp(&lit), op)),
        // A BOOLEAN column meeting a NUMBER casts itself to the number.
        (Conformed::Bool(value), PinLiteral::Int(lit)) => {
            Some(apply_ord(i64::from(value).cmp(&lit), op))
        }
        (Conformed::Bool(value), PinLiteral::Double(lit)) => {
            Some(apply_f64(f64::from(u8::from(value)), lit, op))
        }
        (Conformed::Time(value), PinLiteral::Time(lit)) => Some(apply_ord(value.cmp(&lit), op)),
        // A SEVERITY column is a BIGINT to every comparison; only its
        // TEXT rendering is special.
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

/// The text a glob/regex matches for one event value, under the pin's
/// pattern form — the in-memory mirror of the SQL side's `pattern_target`.
///
/// `None` is a NULL pattern target: every typed pin can produce one, for a
/// value the conform would also null out, which is exactly what the corpus
/// already holds.
///
/// Each typed reading mirrors what [`crate::conform`] stored, not what the
/// wire carried — the wire text is only the same string when the value
/// already reads as its pin, and the divergences are silent (a live tail
/// firing on events the equivalent batch query drops). Conformance casts
/// the value's TEXT form under a round-trip guard, so the readings here are
/// text readings too, and each is total on exactly the shapes that guard
/// admits — see [`conformed_reading`] for the per-pin rules, established by
/// execution probes in `trawl-engine/tests/duckdb_probe.rs`.
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

/// Convert a JSON value to its string representation for comparison.
pub(crate) fn json_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
        // Arrays and objects: serialize to JSON string.
        other => other.to_string(),
    }
}

/// Apply an ordering-based comparison operator.
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

/// Apply a comparison on f64 values.
///
/// Uses exact IEEE 754 operators to match `DuckDB`'s behavior.
/// `NaN` comparisons return false, matching SQL semantics.
#[allow(clippy::float_cmp)] // intentional: must match DuckDB's exact comparison
fn apply_f64(a: f64, b: f64, op: CompareOp) -> bool {
    match op {
        CompareOp::Eq => a == b,
        CompareOp::Ne => a != b,
        CompareOp::Gt => a > b,
        CompareOp::Gte => a >= b,
        CompareOp::Lt => a < b,
        CompareOp::Lte => a <= b,
    }
}
