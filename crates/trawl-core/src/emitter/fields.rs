// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::SqlValue;

pub(crate) fn quote_field(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// A repeat would be a second column of one name. `DuckDB` renames it
/// `host_1` inside a CTE, a column the live lane never makes (it keeps
/// one `host`) and the relation walk never records.
pub(crate) fn first_of_each<'a>(names: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    let mut seen = std::collections::HashSet::new();
    names
        .into_iter()
        .filter(|name| seen.insert(crate::schema::catalog_key(name)))
        .collect()
}

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

    #[test]
    fn first_of_each_keeps_the_first_spelling_of_a_folded_name() {
        assert_eq!(
            first_of_each(["id", "host", "HOST", "f", "id", "Ü", "ü"]),
            ["id", "host", "f", "Ü", "ü"],
            "an ASCII fold only, as DuckDB folds identifiers"
        );
    }

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
        assert_eq!(
            coerce_filter_value("9999999999999999999"),
            SqlValue::Float(9_999_999_999_999_999_999.0)
        );
    }
}
