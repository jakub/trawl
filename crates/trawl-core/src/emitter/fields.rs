// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::SqlValue;

/// Double-quote a field name for safe use in SQL.
///
/// Any embedded double-quotes are escaped by doubling them. The name is
/// otherwise verbatim: the DSL has zero aliases (ADR-0013 §6), so the
/// name you type is the column in DESCRIBE is the identifier in the SQL.
pub(crate) fn quote_field(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Coerce a string filter value to the most specific `SqlValue`.
///
/// Tries `i64`, then `f64`, falls back to `String`.
///
/// This is the no-pin branch of the comparison rules (ADR-0011):
/// unpinned fields, embedded mode, and every typed-pin case the rule
/// table leaves unchanged route through here via
/// [`crate::compare::compare_form`], which owns the decision of when a
/// catalog pin overrides this literal-driven coercion.
pub(crate) fn coerce_filter_value(s: &str) -> SqlValue {
    if let Ok(i) = s.parse::<i64>() {
        return SqlValue::Int(i);
    }
    if let Ok(f) = s.parse::<f64>() {
        return SqlValue::Float(f);
    }
    SqlValue::String(s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_field_escapes_embedded_quotes() {
        assert_eq!(quote_field(r#"field"name"#), r#""field""name""#);
    }

    /// Zero aliases: `timestamp` and `@timestamp` are ordinary sender
    /// field names and quote as themselves (ADR-0013 §6).
    #[test]
    fn quote_field_is_verbatim() {
        assert_eq!(quote_field("@timestamp"), "\"@timestamp\"");
        assert_eq!(quote_field("timestamp"), "\"timestamp\"");
        assert_eq!(quote_field("_time"), "\"_time\"");
        assert_eq!(quote_field("level"), "\"level\"");
    }

    #[test]
    fn quote_field_simple() {
        assert_eq!(quote_field("host"), "\"host\"");
    }

    // ── coerce_filter_value ─────────────────────────────────────────────

    #[test]
    fn coerce_integer() {
        assert_eq!(coerce_filter_value("42"), SqlValue::Int(42));
    }

    #[test]
    fn coerce_negative_integer() {
        assert_eq!(coerce_filter_value("-1"), SqlValue::Int(-1));
    }

    #[test]
    fn coerce_zero() {
        assert_eq!(coerce_filter_value("0"), SqlValue::Int(0));
    }

    #[test]
    fn coerce_float() {
        assert_eq!(coerce_filter_value("1.23"), SqlValue::Float(1.23));
    }

    #[test]
    fn coerce_negative_float() {
        assert_eq!(coerce_filter_value("-0.5"), SqlValue::Float(-0.5));
    }

    #[test]
    fn coerce_string_fallback() {
        assert_eq!(
            coerce_filter_value("hello"),
            SqlValue::String("hello".to_string())
        );
    }

    #[test]
    fn coerce_empty_string() {
        assert_eq!(coerce_filter_value(""), SqlValue::String(String::new()));
    }

    #[test]
    fn coerce_i64_overflow_becomes_float() {
        // past i64::MAX, so the i64 parse fails and the f64 one wins
        assert_eq!(
            coerce_filter_value("9999999999999999999"),
            SqlValue::Float(9_999_999_999_999_999_999.0)
        );
    }
}
