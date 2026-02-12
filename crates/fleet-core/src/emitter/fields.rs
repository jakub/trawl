use super::SqlValue;

/// Map system field names to their storage column names.
///
/// `@timestamp` → `timestamp`, everything else passes through.
pub(crate) fn map_field_name(name: &str) -> &str {
    match name {
        "@timestamp" => "timestamp",
        other => other,
    }
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
    fn quote_field_maps_timestamp() {
        assert_eq!(quote_field("@timestamp"), "\"timestamp\"");
    }

    #[test]
    fn quote_field_simple() {
        assert_eq!(quote_field("host"), "\"host\"");
    }
}
