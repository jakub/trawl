// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pin-aware comparison rules for search-stage field filters (ADR-0011
//! slice A).
//!
//! One rule table, two consumers: the SQL emitter's field-filter arm
//! ([`crate::emitter`]) renders these forms to SQL, and the in-memory
//! [`crate::filter::CompiledFilter`] evaluates the same forms against JSON
//! events — batch/live parity is part of the contract, so the decision of
//! *how* a comparison binds under a catalog pin lives here and nowhere
//! else.
//!
//! The rules (the ADR-0011 slice A table):
//!
//! | pinned type | operation | form |
//! |---|---|---|
//! | unpinned | all | [`CompareForm::Native`] — literal-driven, unchanged |
//! | VARCHAR | `=` / `!=` / IN element | [`CompareForm::Text`] — compare as text (`'200'`) |
//! | VARCHAR | ordered + numeric literal | [`CompareForm::NumericOnText`] — `TRY_CAST(col AS DOUBLE)`; non-numeric values NULL out |
//! | VARCHAR | ordered + non-numeric literal | [`CompareForm::Native`] — lexical, unchanged |
//! | VARCHAR | glob / regex | [`PatternForm::Native`] — unchanged |
//! | `TIMESTAMP` | glob / regex | [`PatternForm::Rfc3339Text`] — the canonical RFC 3339 UTC-microsecond text |
//! | `DOUBLE` | glob / regex | [`PatternForm::DoubleText`] — `DuckDB`'s DOUBLE rendering (`200.0`, `1e-07`) |
//! | `BIGINT` | glob / regex | [`PatternForm::BigIntText`] — the conformed integer's decimal text (`"0404"` globs as `404`) |
//! | `BOOLEAN` | glob / regex | [`PatternForm::BooleanText`] — `true`/`false` (`"TRUE"` globs as `true`) |
//! | typed pins | everything else | [`CompareForm::Native`] — unchanged (already correct) |
//!
//! "Numeric literal" is decided by content (the same i64-then-f64 ladder as
//! [`coerce_filter_value`]): the AST discards quote provenance, so
//! `status>"400"` is indistinguishable from `status>400`. Documented, not
//! fixed here (fixing it is a parser change, out of slice-A scope).
//!
//! The ordered-numeric rung is `DuckDB`'s cast domain, not Rust's float
//! parser: [`try_cast_double`] and [`double_cmp`] are the live mirrors of
//! `TRY_CAST(col AS DOUBLE)` and of `DuckDB`'s total DOUBLE ordering, and
//! every widening they carry (whitespace, `_` separators, NaN ordering)
//! is a value batch would return and the stream would otherwise drop.
//!
//! `DOUBLE` uniformly for the ordered-numeric rule, never `BIGINT`:
//! `TRY_CAST('1.5' AS BIGINT)` ROUNDS to 2 (pinned by
//! `trawl-engine/tests/duckdb_probe.rs`), so a per-literal-type domain
//! would make `dur>1` and `dur>1.5` disagree about a stored `"1.5"`.

use crate::ast::FilterOp;
use crate::emitter::SqlValue;
use crate::emitter::coerce_filter_value;
use crate::schema::CanonicalType;

/// How one search-stage comparison binds its literal.
#[derive(Debug, Clone, PartialEq)]
pub enum CompareForm {
    /// Today's literal-driven binding — the unpinned/typed-pin default.
    Native(SqlValue),
    /// Compare as text: the literal binds as a string (`'200'`).
    Text(String),
    /// Ordered numeric comparison over a VARCHAR column:
    /// `TRY_CAST(col AS DOUBLE) op ?` with the literal bound as DOUBLE —
    /// non-numeric stored values become NULL and don't match.
    NumericOnText(f64),
}

/// How one glob/regex pattern binds its column.
///
/// A pattern needs ONE text per pinned type, or batch and live disagree:
/// the SQL side matches the CONFORMED value's rendering (the column is
/// `TRY_CAST` to its pin on the hot branch and already typed on disk)
/// while the live matcher only ever sees the wire JSON. Stringifying the
/// wire value is that same text only when the value already reads as its
/// pin — a JSON int under a BIGINT pin, a JSON bool under a BOOLEAN pin.
/// For every other shape the two diverge, so each typed pin renders the
/// value's own cast reading:
///
/// - `"0404"` conforms to the BIGINT `404`, so `status=0*` matches the
///   wire text and misses the column, and `"accepted"` conforms to NULL
///   where the wire text matches `a*`. [`Self::BigIntText`] renders
///   [`try_cast_bigint`]/[`double_to_bigint`] instead.
/// - `"TRUE"` conforms to the BOOLEAN `true`, whose rendering is
///   lowercase. [`Self::BooleanText`] renders the value's boolean reading
///   ([`try_cast_boolean`]/[`double_to_boolean`]).
/// - `DuckDB` renders a TIMESTAMP space-separated and zoneless
///   (`2026-01-15 09:00:00`) where the event carries RFC 3339
///   (`2026-01-15T09:00:00.000000Z`), so `_time=/T09:/` would match live
///   and miss in batch. [`Self::Rfc3339Text`] pins both sides to the wire
///   form (see [`TIMESTAMP_PATTERN_SQL_FORMAT`] and
///   [`canonical_timestamp_text`]).
/// - `DuckDB` renders a DOUBLE with a mandatory fraction and a signed,
///   two-digit exponent (`200.0`, `0.0`, `1e-07`,
///   `1.2345678901234568e+17`) where the same wire value stringifies as
///   `200` / `0` / `1e-7` / `123456789012345680` — the conformed column
///   is DOUBLE whatever the wire number looked like, so `dur=/^200$/`
///   would match live and miss in batch. [`Self::DoubleText`] renders the
///   value's DOUBLE reading on the live side too (see
///   [`canonical_double_text`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternForm {
    /// Match against the column directly (unpinned or VARCHAR pin).
    Native,
    /// The BIGINT pin's canonical text: the conformed integer in decimal —
    /// `CAST(col AS VARCHAR)` on the SQL side (the column already IS
    /// BIGINT), the value's own `TRY_CAST` reading rendered through
    /// `i64`'s `Display` on the live side.
    BigIntText,
    /// The BOOLEAN pin's canonical text — `true`/`false`, lowercase:
    /// `CAST(col AS VARCHAR)` on the SQL side, the value's own `TRY_CAST`
    /// reading rendered through `bool`'s `Display` on the live side.
    BooleanText,
    /// The TIMESTAMP pin's canonical text: RFC 3339, UTC, always six
    /// fractional digits — `strftime` on the SQL side,
    /// [`canonical_timestamp_text`] on the live side.
    Rfc3339Text,
    /// The DOUBLE pin's canonical text: `DuckDB`'s own DOUBLE rendering —
    /// `CAST(col AS VARCHAR)` on the SQL side (the column already IS
    /// DOUBLE), [`canonical_double_text`] over the value's DOUBLE reading
    /// on the live side.
    DoubleText,
}

/// The `DuckDB` `strftime` format producing a TIMESTAMP pin's canonical
/// pattern text — RFC 3339 with a fixed six-digit fraction and a literal
/// `Z`, the same shape ingest canonicalizes `_time`/`_ingested` into.
///
/// `%f` is `DuckDB`'s zero-padded microsecond field; the pairing of this
/// format with [`canonical_timestamp_text`] is executed in
/// `trawl-engine/tests/duckdb_probe.rs`, not assumed.
pub const TIMESTAMP_PATTERN_SQL_FORMAT: &str = "%Y-%m-%dT%H:%M:%S.%fZ";

/// Render a live event's value in the TIMESTAMP pin's canonical pattern
/// text — the in-memory mirror of
/// `strftime(col, TIMESTAMP_PATTERN_SQL_FORMAT)`.
///
/// `None` means the value has no timestamp reading, which is what the
/// batch side stores: conformance `TRY_CAST`s the field to TIMESTAMP, so
/// a value with no reading is NULL on disk and `strftime` of NULL is NULL
/// — UNKNOWN, never a false pattern miss that `NOT` could invert.
///
/// The reading mirrors `DuckDB`'s `TRY_CAST(… AS TIMESTAMP)`, which is a
/// WALL-CLOCK parse: `T` or space separator, optional fractional seconds
/// (truncated to microseconds), `-` or `/` date separators, and an
/// optional zone suffix that is **ignored**, not applied
/// (`09:00:00+05:30` stores `09:00:00`, probe-pinned). Epoch numerals are
/// not timestamps to `DuckDB` and are not read as such here.
#[must_use]
pub fn canonical_timestamp_text(value: &str) -> Option<String> {
    let wall = wall_clock(value)?;
    Some(wall.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string())
}

/// The wall clock `DuckDB` would store for this text, or `None`.
///
/// Only LEADING whitespace is trimmed up front: a separator with nothing
/// after it (`"2026-01-15 "`, `"2026-01-15T"`) is not a date to `DuckDB`,
/// so the split has to see the separator before any trimming can hide it.
fn wall_clock(value: &str) -> Option<chrono::NaiveDateTime> {
    let text = value.trim_start();
    let (date, time) = match text.find(['T', ' ']) {
        Some(idx) => (&text[..idx], Some(text[idx + 1..].trim())),
        None => (text, None),
    };
    let date = chrono::NaiveDate::parse_from_str(&date.replace('/', "-"), "%Y-%m-%d").ok()?;
    let Some(time) = time else {
        return Some(date.into());
    };
    // A trailing zone designator is dropped, not applied.
    let time = strip_zone(time);
    let parsed = chrono::NaiveTime::parse_from_str(time, "%H:%M:%S%.f")
        .or_else(|_| chrono::NaiveTime::parse_from_str(time, "%H:%M"))
        .ok()?;
    Some(date.and_time(parsed))
}

/// Strip a trailing `Z` or `±HH[:MM]` offset from a time-of-day text.
fn strip_zone(time: &str) -> &str {
    if let Some(rest) = time.strip_suffix('Z') {
        return rest;
    }
    match time.rfind(['+', '-']) {
        Some(idx) => &time[..idx],
        None => time,
    }
}

/// Resolve the binding for a comparison (`=`, `!=`, ordered) or an IN-list
/// element under the field's pin.
///
/// `op` [`FilterOp::Glob`]/[`FilterOp::Regex`] never reach here (patterns
/// resolve through [`pattern_form`]); they fall through to the native
/// branch defensively.
#[must_use]
pub fn compare_form(pin: Option<CanonicalType>, op: FilterOp, literal: &str) -> CompareForm {
    if pin != Some(CanonicalType::Varchar) {
        return CompareForm::Native(coerce_filter_value(literal));
    }
    match op {
        FilterOp::Eq | FilterOp::Ne => CompareForm::Text(literal.to_owned()),
        FilterOp::Gt | FilterOp::Gte | FilterOp::Lt | FilterOp::Lte => numeric_literal(literal)
            .map_or_else(
                || CompareForm::Native(coerce_filter_value(literal)),
                CompareForm::NumericOnText,
            ),
        FilterOp::Glob | FilterOp::Regex => CompareForm::Native(coerce_filter_value(literal)),
    }
}

/// Resolve the binding for a glob/regex pattern under the field's pin.
///
/// Exhaustive on purpose: a new [`CanonicalType`] must state which text
/// its patterns match, because "whatever `CAST(col AS VARCHAR)` says" is
/// only a *live-mirrorable* answer for types the wire form already
/// stringifies to identically.
#[must_use]
pub fn pattern_form(pin: Option<CanonicalType>) -> PatternForm {
    match pin {
        None | Some(CanonicalType::Varchar) => PatternForm::Native,
        Some(CanonicalType::Timestamp) => PatternForm::Rfc3339Text,
        Some(CanonicalType::Double) => PatternForm::DoubleText,
        Some(CanonicalType::BigInt) => PatternForm::BigIntText,
        Some(CanonicalType::Boolean) => PatternForm::BooleanText,
    }
}

/// The BIGINT `DuckDB` reads out of a stored text under
/// `TRY_CAST(col AS BIGINT)` — the live mirror of the BIGINT pin's pattern
/// target, and deliberately NOT `str::parse::<i64>`.
///
/// `DuckDB`'s VARCHAR → BIGINT domain is wider than an integer parse in
/// four ways (all executed in `trawl-engine/tests/duckdb_probe.rs`), and
/// each one costs a live match otherwise — batch renders a number, the
/// stream matches the raw text or nothing:
///
/// - leading/trailing ASCII whitespace is trimmed and `_` separators are
///   accepted between digits, exactly as for DOUBLE ([`try_cast_double`]);
/// - `0x`/`0b` prefixes are read as hex/binary — but ONLY as an exact
///   prefix, so no sign and no surrounding whitespace (`' 0x10 '` and
///   `'-0x10'` are both NULL to `DuckDB`);
/// - fractional and exponent forms ROUND rather than fail, half AWAY FROM
///   ZERO (`'1.5'` → 2, `'2.5'` → 3, `'-2.5'` → -3) — unlike the DOUBLE →
///   BIGINT rung, which rounds half to EVEN ([`double_to_bigint`]);
/// - the result must fit BIGINT: an integer-syntax literal too large is
///   NULL, never a saturated approximation.
///
/// Residual: the fractional rung reads through `f64`, so a fractional text
/// within a half-ULP of ±2^63 (`'9223372036854775807.4'`) is refused where
/// `DuckDB`'s exact decimal rounding keeps it. Integer texts — the shape a
/// BIGINT-pinned field actually carries — are exact across the whole range.
#[must_use]
pub fn try_cast_bigint(text: &str) -> Option<i64> {
    // Radix prefixes bind on the RAW text: DuckDB reads '0x10' but not
    // ' 0x10 ', so trimming has to come after this.
    for (prefix, radix) in [("0x", 16u32), ("0X", 16), ("0b", 2), ("0B", 2)] {
        if let Some(digits) = text.strip_prefix(prefix) {
            // `from_str_radix` would take a sign here; `DuckDB` does not.
            if digits.starts_with(['+', '-']) {
                return None;
            }
            return i64::from_str_radix(digits, radix).ok();
        }
    }
    let trimmed = text.trim_matches(is_c_space);
    let normalized = if trimmed.contains('_') {
        std::borrow::Cow::Owned(strip_digit_separators(trimmed)?)
    } else {
        std::borrow::Cow::Borrowed(trimmed)
    };
    if let Ok(i) = normalized.parse::<i64>() {
        return Some(i);
    }
    // Integer syntax `i64` cannot hold is out of BIGINT range, and the f64
    // rung below would answer with a saturated approximation instead of
    // the NULL `DuckDB` writes.
    if !normalized.contains(['.', 'e', 'E']) {
        return None;
    }
    bigint_in_range(normalized.parse::<f64>().ok()?.round())
}

/// The BIGINT `DuckDB` reads out of a stored DOUBLE under
/// `TRY_CAST(col AS BIGINT)` — a JSON number that `read_json` typed DOUBLE
/// (any fractional wire value) still conforms to the BIGINT pin.
///
/// Rounds half to EVEN — `DuckDB`'s DOUBLE → BIGINT cast, which is NOT the
/// half-away-from-zero rounding of its VARCHAR → BIGINT cast (`2.5` is 2
/// from a double and 3 from the text `'2.5'`; both probed). Out of range,
/// infinite and NaN inputs are the NULL the cast writes.
#[must_use]
pub fn double_to_bigint(x: f64) -> Option<i64> {
    bigint_in_range(x.round_ties_even())
}

/// A rounded double narrowed to BIGINT, or `None` outside its range.
/// ±2^63 is exactly representable, so the bounds are exact.
#[allow(clippy::cast_possible_truncation)] // guarded by the range check
fn bigint_in_range(rounded: f64) -> Option<i64> {
    (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0)
        .contains(&rounded)
        .then_some(rounded as i64)
}

/// The BOOLEAN `DuckDB` reads out of a stored text under
/// `TRY_CAST(col AS BOOLEAN)` — the live mirror of the BOOLEAN pin's
/// pattern target.
///
/// A closed, case-insensitive vocabulary with NO whitespace trimming
/// (`' true '` is NULL, unlike the numeric casts) and no numeric texts
/// beyond `1`/`0` (`'2'`, `'1.0'` and `'on'`/`'off'` are all NULL) — every
/// member and every rejection probed by execution.
#[must_use]
pub fn try_cast_boolean(text: &str) -> Option<bool> {
    const TRUE_WORDS: [&str; 5] = ["true", "t", "yes", "y", "1"];
    const FALSE_WORDS: [&str; 5] = ["false", "f", "no", "n", "0"];
    if TRUE_WORDS.iter().any(|w| text.eq_ignore_ascii_case(w)) {
        return Some(true);
    }
    if FALSE_WORDS.iter().any(|w| text.eq_ignore_ascii_case(w)) {
        return Some(false);
    }
    None
}

/// The BOOLEAN `DuckDB` reads out of a stored number: zero is false and
/// everything else — negative, fractional, NaN — is true. Total, so a
/// numeric wire value under a BOOLEAN pin never NULLs out.
#[must_use]
#[allow(clippy::float_cmp)] // intentional: mirrors DuckDB's zero test
pub fn double_to_boolean(x: f64) -> bool {
    x != 0.0
}

/// Render a DOUBLE in the pattern text `DuckDB`'s `CAST(col AS VARCHAR)`
/// produces — the live mirror of the DOUBLE pin's pattern target, and the
/// same renderer `tostring()` uses in streaming eval (one renderer, so
/// `dur=/^200$/` and `tostring(dur)` cannot disagree about `200.0`).
///
/// The rendering rules (shortest round-trip digits, a mandatory `.0` on
/// integral values, sign-stripped zero, signed two-digit exponents,
/// lowercase `inf`/`nan`) are documented on the renderer itself and
/// executed against `DuckDB` in `trawl-engine/tests/duckdb_probe.rs`.
#[must_use]
pub fn canonical_double_text(x: f64) -> String {
    crate::eval::duckdb_double_to_string(x)
}

/// The DOUBLE `DuckDB` reads out of a stored text under
/// `TRY_CAST(col AS DOUBLE)` — the live mirror of the ordered-numeric
/// rung, and deliberately NOT `str::parse::<f64>`.
///
/// `DuckDB`'s cast domain is strictly wider than Rust's float parser in
/// two ways (both executed in `trawl-engine/tests/duckdb_probe.rs`), and
/// both widen it in the direction that costs live matches: batch returns
/// the row, the stream drops it.
///
/// - ASCII whitespace is trimmed off BOTH ends — space, `\t`, `\n`,
///   `\r`, `\x0b`, `\x0c` (C `isspace`, so `\x0b` too, which
///   [`char::is_ascii_whitespace`] excludes), and never a non-ASCII
///   space like U+00A0. `' 200'` is 200 to `DuckDB`.
/// - `_` digit separators are accepted strictly BETWEEN ASCII digits:
///   `'200_000'` is 200000 and `'1e1_0'` is 1e10, while `'_200'`,
///   `'200_'`, `'1__0'`, `'1._5'` and `'1_e3'` are NULL.
///
/// Everything else the two engines already agree on, so the normalized
/// text goes to Rust's parser verbatim: `nan`/`inf`/`infinity` in any
/// case and sign, `+5`, `1.`, `.5`, `1e3`, `00200`, and out-of-range
/// exponents saturating to ±inf / ±0. `None` is the NULL `TRY_CAST`
/// writes — UNKNOWN, never a false FALSE that `NOT` could invert.
#[must_use]
pub fn try_cast_double(text: &str) -> Option<f64> {
    let trimmed = text.trim_matches(is_c_space);
    if trimmed.contains('_') {
        return strip_digit_separators(trimmed)?.parse().ok();
    }
    trimmed.parse().ok()
}

/// C `isspace` over ASCII — what `DuckDB` strips before a numeric cast.
fn is_c_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c')
}

/// Remove `_` digit separators, or `None` if any sits somewhere
/// `DuckDB` refuses it (anywhere but between two ASCII digits).
fn strip_digit_separators(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    for (idx, ch) in text.char_indices() {
        if ch != '_' {
            out.push(ch);
            continue;
        }
        // Byte-indexed neighbours: a multi-byte char's trailing byte is
        // never an ASCII digit, so a non-digit neighbour is rejected
        // whatever its encoding.
        let prev = idx.checked_sub(1).map(|i| bytes[i]);
        let next = bytes.get(idx + 1).copied();
        if !prev.is_some_and(|b| b.is_ascii_digit()) || !next.is_some_and(|b| b.is_ascii_digit()) {
            return None;
        }
    }
    Some(out)
}

/// `DuckDB`'s ordering over DOUBLE, which is TOTAL: NaN sits above every
/// other value (including `inf`) and equals itself, while `-0.0` equals
/// `0.0`. Rust's own operators answer FALSE to every NaN comparison, so
/// a stored `'nan'` under the ordered-numeric rung matches `dur>1` in
/// batch and would miss in the stream without this.
#[must_use]
pub fn double_cmp(a: f64, b: f64) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    // partial_cmp is None only when a NaN is involved.
    a.partial_cmp(&b).unwrap_or(match (a.is_nan(), b.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        _ => Ordering::Less,
    })
}

/// Whether the literal's content is numeric, under the same i64-then-f64
/// ladder [`coerce_filter_value`] uses — so the pinned and unpinned paths
/// agree on what counts as a number.
#[allow(clippy::cast_precision_loss)] // i64 → f64: same collapse DuckDB's DOUBLE comparison applies
fn numeric_literal(s: &str) -> Option<f64> {
    if let Ok(i) = s.parse::<i64>() {
        return Some(i as f64);
    }
    s.parse::<f64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORDERED: [FilterOp; 4] = [FilterOp::Gt, FilterOp::Gte, FilterOp::Lt, FilterOp::Lte];
    const EQ_CLASS: [FilterOp; 2] = [FilterOp::Eq, FilterOp::Ne];
    const TYPED_PINS: [CanonicalType; 4] = [
        CanonicalType::BigInt,
        CanonicalType::Double,
        CanonicalType::Timestamp,
        CanonicalType::Boolean,
    ];

    /// Literal shapes the matrix runs over: (literal, native coercion).
    fn literal_shapes() -> Vec<(&'static str, SqlValue)> {
        vec![
            ("200", SqlValue::Int(200)),
            ("-1", SqlValue::Int(-1)),
            ("0", SqlValue::Int(0)),
            // i64 overflow falls to the f64 rung, mirroring coerce_filter_value.
            (
                "9999999999999999999",
                SqlValue::Float(9_999_999_999_999_999_999.0),
            ),
            ("1.5", SqlValue::Float(1.5)),
            ("-0.5", SqlValue::Float(-0.5)),
            ("accepted", SqlValue::String("accepted".to_owned())),
            ("", SqlValue::String(String::new())),
        ]
    }

    fn is_numeric(native: &SqlValue) -> bool {
        matches!(native, SqlValue::Int(_) | SqlValue::Float(_))
    }

    fn as_f64(native: &SqlValue) -> f64 {
        #[allow(clippy::cast_precision_loss)]
        match native {
            SqlValue::Int(i) => *i as f64,
            SqlValue::Float(f) => *f,
            _ => unreachable!("only numeric shapes"),
        }
    }

    // ── unpinned: everything stays literal-driven ─────────────────────

    #[test]
    fn unpinned_is_native_for_every_op_and_literal() {
        for (lit, native) in literal_shapes() {
            for op in ORDERED.iter().chain(EQ_CLASS.iter()) {
                assert_eq!(
                    compare_form(None, *op, lit),
                    CompareForm::Native(native.clone()),
                    "unpinned {op:?} {lit:?}"
                );
            }
        }
        assert_eq!(pattern_form(None), PatternForm::Native);
    }

    // ── VARCHAR pin ───────────────────────────────────────────────────

    #[test]
    fn varchar_eq_class_binds_text_for_every_literal() {
        for (lit, _) in literal_shapes() {
            for op in EQ_CLASS {
                assert_eq!(
                    compare_form(Some(CanonicalType::Varchar), op, lit),
                    CompareForm::Text(lit.to_owned()),
                    "varchar {op:?} {lit:?}"
                );
            }
        }
    }

    #[test]
    fn varchar_ordered_numeric_literal_compares_numerically_on_text() {
        for (lit, native) in literal_shapes() {
            if !is_numeric(&native) {
                continue;
            }
            for op in ORDERED {
                assert_eq!(
                    compare_form(Some(CanonicalType::Varchar), op, lit),
                    CompareForm::NumericOnText(as_f64(&native)),
                    "varchar {op:?} {lit:?}"
                );
            }
        }
    }

    #[test]
    fn varchar_ordered_non_numeric_literal_stays_lexical() {
        for lit in ["accepted", "", "1.5.6", "10a"] {
            for op in ORDERED {
                assert_eq!(
                    compare_form(Some(CanonicalType::Varchar), op, lit),
                    CompareForm::Native(SqlValue::String(lit.to_owned())),
                    "varchar {op:?} {lit:?}"
                );
            }
        }
    }

    #[test]
    fn varchar_patterns_stay_native() {
        assert_eq!(
            pattern_form(Some(CanonicalType::Varchar)),
            PatternForm::Native
        );
    }

    // ── typed pins ────────────────────────────────────────────────────

    #[test]
    fn typed_pins_keep_native_comparisons() {
        for pin in TYPED_PINS {
            for (lit, native) in literal_shapes() {
                for op in ORDERED.iter().chain(EQ_CLASS.iter()) {
                    assert_eq!(
                        compare_form(Some(pin), *op, lit),
                        CompareForm::Native(native.clone()),
                        "{pin:?} {op:?} {lit:?}"
                    );
                }
            }
        }
    }

    /// Every typed pin gets its OWN pattern text — none of them can share
    /// "stringify the wire value", because each conforms values the wire
    /// spells differently (`"0404"` → 404, `"TRUE"` → true, `200` → 200.0,
    /// `…T09:00:00Z` → the six-digit RFC 3339 form).
    #[test]
    fn typed_pins_each_take_their_own_pattern_text() {
        for pin in TYPED_PINS {
            let expected = match pin {
                CanonicalType::Timestamp => PatternForm::Rfc3339Text,
                CanonicalType::Double => PatternForm::DoubleText,
                CanonicalType::BigInt => PatternForm::BigIntText,
                CanonicalType::Boolean => PatternForm::BooleanText,
                CanonicalType::Varchar => unreachable!("not a typed pin"),
            };
            assert_eq!(pattern_form(Some(pin)), expected, "{pin:?}");
        }
    }

    /// The DOUBLE pin's pattern text is `DuckDB`'s DOUBLE rendering, NOT
    /// the wire number's stringification — every expectation here is the
    /// string `CAST(v AS VARCHAR)` returns over a DOUBLE column (executed
    /// side by side in `trawl-engine/tests/duckdb_probe.rs`).
    #[test]
    fn canonical_double_text_mirrors_duckdb_rendering() {
        let cases = [
            // The reported divergence: a wire `200` stores as 200.0, and
            // `dur=/^200$/` must miss on both sides, not just in batch.
            (200.0, "200.0"),
            (0.0, "0.0"),
            // A stored -0.0 renders signed; only a SQL literal `-0.0`
            // folds to positive zero before it is ever rendered.
            (-0.0, "-0.0"),
            (-3.0, "-3.0"),
            (1.5, "1.5"),
            (1e-7, "1e-07"),
            (1e16, "1e+16"),
            (1.234_567_890_123_456_8e17, "1.2345678901234568e+17"),
            (1e100, "1e+100"),
            (1e-300, "1e-300"),
            (0.0001, "0.0001"),
            (f64::INFINITY, "inf"),
            (f64::NEG_INFINITY, "-inf"),
            (f64::NAN, "nan"),
        ];
        for (input, expected) in cases {
            assert_eq!(canonical_double_text(input), expected, "{input}");
        }
    }

    // ── the TIMESTAMP pin's canonical pattern text ────────────────────

    /// Every shape that has a reading renders the ONE canonical text —
    /// `T` separator, six fractional digits, `Z`. Each expectation here is
    /// the string `DuckDB`'s `strftime(TRY_CAST(v AS TIMESTAMP), fmt)`
    /// returns for the same input (executed in
    /// `trawl-engine/tests/duckdb_probe.rs`).
    #[test]
    fn canonical_timestamp_text_mirrors_duckdb_rendering() {
        let cases = [
            // The wire form ingest canonicalizes _time into.
            ("2026-01-15T09:00:00.000000Z", "2026-01-15T09:00:00.000000Z"),
            // Zone-suffixed and separator variants of the same wall clock.
            ("2026-01-15T09:00:00Z", "2026-01-15T09:00:00.000000Z"),
            ("2026-01-15 09:00:00", "2026-01-15T09:00:00.000000Z"),
            ("2026-01-15T09:00", "2026-01-15T09:00:00.000000Z"),
            ("  2026-01-15 09:00:00  ", "2026-01-15T09:00:00.000000Z"),
            // A zone offset is DROPPED, not applied — DuckDB stores the
            // wall clock, so 09:00 stays 09:00.
            ("2026-01-15T09:00:00+05:30", "2026-01-15T09:00:00.000000Z"),
            ("2026-01-15T09:00:00-08:00", "2026-01-15T09:00:00.000000Z"),
            ("2026-01-15T09:00:00+02", "2026-01-15T09:00:00.000000Z"),
            // Fractions pad to six digits and truncate beyond them.
            ("2026-01-15T09:00:00.123Z", "2026-01-15T09:00:00.123000Z"),
            ("2026-01-15T09:00:00.1234567", "2026-01-15T09:00:00.123456Z"),
            (
                "2026-01-15T09:00:00.0000000000Z",
                "2026-01-15T09:00:00.000000Z",
            ),
            // Date-only and slash dates land at midnight.
            ("2026-01-15", "2026-01-15T00:00:00.000000Z"),
            ("2026/01/15", "2026-01-15T00:00:00.000000Z"),
            ("2026/01/15 09:00:00", "2026-01-15T09:00:00.000000Z"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                canonical_timestamp_text(input).as_deref(),
                Some(expected),
                "{input:?}"
            );
        }
    }

    /// No reading → `None`, the UNKNOWN that mirrors the NULL conformance
    /// wrote for the same value. Epoch numerals are not timestamps to
    /// `DuckDB` and must not become one here.
    #[test]
    fn canonical_timestamp_text_is_none_without_a_reading() {
        for input in [
            "yesterday-ish",
            "",
            "1737000000",
            "1737000000123",
            "accepted",
            // A separator with no time after it: not a date to DuckDB
            // either, so trailing whitespace must not be trimmed away
            // before the split.
            "2026-01-15T",
            "2026-01-15 ",
        ] {
            assert_eq!(canonical_timestamp_text(input), None, "{input:?}");
        }
    }

    // ── the stored-value cast domain mirrors DuckDB ───────────────────

    /// Every expectation here is the value `DuckDB`'s
    /// `TRY_CAST(v AS DOUBLE)` returns for the same text (executed side
    /// by side in `trawl-engine/tests/duckdb_probe.rs`).
    #[test]
    fn try_cast_double_mirrors_duckdb_cast_domain() {
        // ASCII whitespace is trimmed off both ends, `\x0b` included.
        for text in [
            " 200",
            "200 ",
            "\t200\n",
            "\r200\r",
            "\x0b200\x0c",
            "  200  ",
        ] {
            assert_eq!(try_cast_double(text), Some(200.0), "{text:?}");
        }
        // Non-ASCII spaces are not whitespace to DuckDB.
        for text in ["\u{a0}200", "\u{2000}200", "2 00", " ", ""] {
            assert_eq!(try_cast_double(text), None, "{text:?}");
        }
        // `_` separators, accepted only between ASCII digits.
        assert_eq!(try_cast_double("200_000"), Some(200_000.0));
        assert_eq!(try_cast_double("1_000.5"), Some(1000.5));
        assert_eq!(try_cast_double("1_0"), Some(10.0));
        assert_eq!(try_cast_double("-1_0"), Some(-10.0));
        assert_eq!(try_cast_double("1.0_0"), Some(1.0));
        assert_eq!(try_cast_double("1e1_0"), Some(1e10));
        assert_eq!(try_cast_double(" 1_0 "), Some(10.0));
        for text in [
            "_200", "200_", "1__0", "1._5", "1.5_", "1_.5", "1_e3", "1e_3", "_",
        ] {
            assert_eq!(try_cast_double(text), None, "{text:?}");
        }
        // Shapes both engines already agree on, unchanged.
        assert_eq!(try_cast_double("+5"), Some(5.0));
        assert_eq!(try_cast_double("1."), Some(1.0));
        assert_eq!(try_cast_double(".5"), Some(0.5));
        assert_eq!(try_cast_double("1e3"), Some(1000.0));
        assert_eq!(try_cast_double("00200"), Some(200.0));
        assert_eq!(try_cast_double("1e400"), Some(f64::INFINITY));
        assert!(try_cast_double("nan").is_some_and(f64::is_nan));
        assert!(try_cast_double("-NAN").is_some_and(f64::is_nan));
        assert_eq!(try_cast_double("infinity"), Some(f64::INFINITY));
        for text in ["0x10", "1,000", "1d", "true", "1.5e2.5", "1-"] {
            assert_eq!(try_cast_double(text), None, "{text:?}");
        }
    }

    /// Every expectation here is the value `DuckDB`'s
    /// `TRY_CAST(v AS BIGINT)` returns for the same text (executed side by
    /// side in `trawl-engine/tests/duckdb_probe.rs`).
    #[test]
    fn try_cast_bigint_mirrors_duckdb_cast_domain() {
        // Plain integers, including the leading-zero text whose stored
        // value is spelled differently — the reported divergence.
        assert_eq!(try_cast_bigint("404"), Some(404));
        assert_eq!(try_cast_bigint("0404"), Some(404));
        assert_eq!(try_cast_bigint("00200"), Some(200));
        assert_eq!(try_cast_bigint("+5"), Some(5));
        assert_eq!(try_cast_bigint("-0"), Some(0));
        assert_eq!(try_cast_bigint("9223372036854775807"), Some(i64::MAX));
        assert_eq!(try_cast_bigint("-9223372036854775808"), Some(i64::MIN));
        // Whitespace and `_` separators, as for the DOUBLE domain.
        assert_eq!(try_cast_bigint(" 404 "), Some(404));
        assert_eq!(try_cast_bigint("\t404\n"), Some(404));
        assert_eq!(try_cast_bigint("404_000"), Some(404_000));
        assert_eq!(try_cast_bigint("1e1_0"), Some(10_000_000_000));
        // Fractional / exponent texts ROUND, half AWAY FROM ZERO.
        assert_eq!(try_cast_bigint("1.5"), Some(2));
        assert_eq!(try_cast_bigint("1.4"), Some(1));
        assert_eq!(try_cast_bigint("2.5"), Some(3));
        assert_eq!(try_cast_bigint("-2.5"), Some(-3));
        assert_eq!(try_cast_bigint(".5"), Some(1));
        assert_eq!(try_cast_bigint("1e3"), Some(1000));
        assert_eq!(try_cast_bigint("1_0.5"), Some(11));
        // Radix prefixes bind on the RAW text: no sign, no whitespace.
        assert_eq!(try_cast_bigint("0x10"), Some(16));
        assert_eq!(try_cast_bigint("0B101"), Some(5));
        for text in ["-0x10", " 0x10 ", "0x+10", "0xzz", "0x1.5", "0o17"] {
            assert_eq!(try_cast_bigint(text), None, "{text:?}");
        }
        // No reading, and out of range — NULL, never a saturated value.
        for text in [
            "accepted",
            "",
            "true",
            "nan",
            "inf",
            "1,000",
            "4 04",
            "9223372036854775808",
            "-9223372036854775809",
            "1e19",
            "1e400",
        ] {
            assert_eq!(try_cast_bigint(text), None, "{text:?}");
        }
    }

    /// The DOUBLE rung rounds half to EVEN where the VARCHAR rung rounds
    /// half AWAY FROM ZERO — two casts, two roundings (both probe-pinned).
    #[test]
    fn double_to_bigint_rounds_half_to_even() {
        assert_eq!(double_to_bigint(1.5), Some(2));
        assert_eq!(double_to_bigint(2.5), Some(2));
        assert_eq!(double_to_bigint(3.5), Some(4));
        assert_eq!(double_to_bigint(-2.5), Some(-2));
        assert_eq!(double_to_bigint(0.4), Some(0));
        assert_eq!(double_to_bigint(200.0), Some(200));
        for x in [1e19, 1e300, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(double_to_bigint(x), None, "{x}");
        }
    }

    /// A closed, case-insensitive vocabulary with NO trimming and no
    /// numeric texts beyond `1`/`0` (probe-pinned).
    #[test]
    fn try_cast_boolean_mirrors_duckdb_vocabulary() {
        for text in ["true", "TRUE", "tRuE", "t", "T", "yes", "yEs", "Y", "1"] {
            assert_eq!(try_cast_boolean(text), Some(true), "{text:?}");
        }
        for text in ["false", "FALSE", "f", "F", "no", "nO", "N", "0"] {
            assert_eq!(try_cast_boolean(text), Some(false), "{text:?}");
        }
        for text in [
            "on", "off", "", " true ", "\ttrue\n", "accepted", "2", "-1", "1.0", "01", "+1",
            "true1",
        ] {
            assert_eq!(try_cast_boolean(text), None, "{text:?}");
        }
        // The numeric reading is TOTAL: zero is false, everything else —
        // negative, fractional, NaN — is true.
        assert!(!double_to_boolean(0.0));
        assert!(!double_to_boolean(-0.0));
        for x in [1.0, 2.0, -1.0, 1.5, f64::NAN, f64::INFINITY] {
            assert!(double_to_boolean(x), "{x}");
        }
    }

    /// `DuckDB` orders DOUBLE totally: NaN above everything and equal to
    /// itself, `-0.0` equal to `0.0` (probe-pinned).
    #[test]
    fn double_cmp_puts_nan_on_top() {
        use std::cmp::Ordering;
        let nan = f64::NAN;
        assert_eq!(double_cmp(nan, nan), Ordering::Equal);
        for other in [1.0, 0.0, -1e308, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(double_cmp(nan, other), Ordering::Greater, "{other}");
            assert_eq!(double_cmp(other, nan), Ordering::Less, "{other}");
        }
        assert_eq!(double_cmp(-0.0, 0.0), Ordering::Equal);
        assert_eq!(double_cmp(f64::INFINITY, 1e308), Ordering::Greater);
        assert_eq!(double_cmp(1.0, 2.0), Ordering::Less);
    }

    // ── the numeric-literal ladder mirrors coerce_filter_value ────────

    #[test]
    fn numeric_detection_follows_the_coercion_ladder() {
        // i64 rung.
        assert_eq!(numeric_literal("42"), Some(42.0));
        assert_eq!(numeric_literal("-7"), Some(-7.0));
        // f64 rung (i64 overflow, fractions).
        assert_eq!(numeric_literal("1.5"), Some(1.5));
        assert_eq!(
            numeric_literal("9999999999999999999"),
            Some(9_999_999_999_999_999_999.0)
        );
        // Non-numeric.
        assert_eq!(numeric_literal("accepted"), None);
        assert_eq!(numeric_literal(""), None);
        assert_eq!(numeric_literal("1.5s"), None);
        // Whitespace is NOT trimmed — mirrors coerce_filter_value, which
        // would bind " 200 " as a string.
        assert_eq!(numeric_literal(" 200 "), None);
    }
}
