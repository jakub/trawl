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
//! | typed pins | glob / regex | [`PatternForm::CastText`] — `CAST(col AS VARCHAR)` first |
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternForm {
    /// Match against the column directly (unpinned or VARCHAR pin).
    Native,
    /// `CAST(col AS VARCHAR)` first — the column is pinned to a
    /// non-VARCHAR type, so glob/regex match its text form.
    CastText,
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
            assert_eq!(pattern_form(Some(pin)), PatternForm::CastText, "{pin:?}");
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
