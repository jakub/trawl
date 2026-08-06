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
//! | other typed pins | glob / regex | [`PatternForm::CastText`] — `CAST(col AS VARCHAR)` first |
//! | typed pins | everything else | [`CompareForm::Native`] — unchanged (already correct) |
//!
//! "Numeric literal" is decided by content (the same i64-then-f64 ladder as
//! [`coerce_filter_value`]): the AST discards quote provenance, so
//! `status>"400"` is indistinguishable from `status>400`. Documented, not
//! fixed here (fixing it is a parser change, out of slice-A scope).
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
/// the SQL side matches the stored typed value's rendering while the
/// live matcher matches the wire JSON. `CAST(col AS VARCHAR)` is already
/// that one text for BIGINT/DOUBLE/BOOLEAN, but never for TIMESTAMP —
/// `DuckDB` renders a TIMESTAMP space-separated and zoneless
/// (`2026-01-15 09:00:00`) where the event carries RFC 3339
/// (`2026-01-15T09:00:00.000000Z`), so `_time=/T09:/` would match live
/// and miss in batch. [`Self::Rfc3339Text`] pins both sides to the wire
/// form (see [`TIMESTAMP_PATTERN_SQL_FORMAT`] and
/// [`canonical_timestamp_text`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternForm {
    /// Match against the column directly (unpinned or VARCHAR pin).
    Native,
    /// `CAST(col AS VARCHAR)` first — the column is pinned to a
    /// non-VARCHAR type, so glob/regex match its text form.
    CastText,
    /// The TIMESTAMP pin's canonical text: RFC 3339, UTC, always six
    /// fractional digits — `strftime` on the SQL side,
    /// [`canonical_timestamp_text`] on the live side.
    Rfc3339Text,
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
#[must_use]
pub fn pattern_form(pin: Option<CanonicalType>) -> PatternForm {
    match pin {
        None | Some(CanonicalType::Varchar) => PatternForm::Native,
        Some(CanonicalType::Timestamp) => PatternForm::Rfc3339Text,
        Some(_) => PatternForm::CastText,
    }
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

    #[test]
    fn typed_pins_cast_patterns_to_text() {
        for pin in TYPED_PINS {
            let expected = if pin == CanonicalType::Timestamp {
                // TIMESTAMP has its own canonical text: DuckDB's default
                // rendering is space-separated and zoneless, which no live
                // event carries.
                PatternForm::Rfc3339Text
            } else {
                PatternForm::CastText
            };
            assert_eq!(pattern_form(Some(pin)), expected, "{pin:?}");
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
