// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::SqlValue;

/// Map system field names to their storage column names.
///
/// `timestamp` and `@timestamp` → `_time` (ADR-0009: `_time` is the
/// physical event-time column; the pre-cutover names remain as aliases so
/// `sort timestamp` and `| table @timestamp` keep working). Everything
/// else passes through.
pub fn map_field_name(name: &str) -> &str {
    crate::schema::resolve_field_alias(name)
}

/// Double-quote a field name for safe use in SQL.
///
/// Any embedded double-quotes are escaped by doubling them.
pub(crate) fn quote_field(name: &str) -> String {
    let mapped = map_field_name(name);
    format!("\"{}\"", mapped.replace('"', "\"\""))
}

/// Coerce a string filter value to the most specific `SqlValue`.
///
/// Tries `i64`, then `f64`, falls back to `String`.
///
/// This is the NO-PIN branch of the comparison rules (ADR-0011 slice A):
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

    #[test]
    fn quote_field_maps_at_timestamp() {
        assert_eq!(quote_field("@timestamp"), "\"_time\"");
    }

    #[test]
    fn quote_field_maps_timestamp_alias() {
        assert_eq!(quote_field("timestamp"), "\"_time\"");
    }

    #[test]
    fn quote_field_time_passthrough() {
        assert_eq!(quote_field("_time"), "\"_time\"");
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
        // 2^63 overflows i64 but parses as f64
        assert_eq!(
            coerce_filter_value("9999999999999999999"),
            SqlValue::Float(9_999_999_999_999_999_999.0)
        );
    }
}
