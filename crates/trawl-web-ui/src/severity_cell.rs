// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Severity presentation, keyed off `_severity` alone (ADR-0013 §9).

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use trawl_api::value::Value;

#[must_use]
pub fn severity_columns<'a>(
    names: impl IntoIterator<Item = &'a str>,
    declared: &[String],
) -> Vec<usize> {
    names
        .into_iter()
        .enumerate()
        .filter(|(_, name)| trawl_core::severity::renders_as_severity(name, declared))
        .map(|(i, _)| i)
        .collect()
}

/// The column is BIGINT on the wire; a string is tolerated only because
/// a hot-buffer row can carry the JSON shape before conformance renders
/// it.
#[must_use]
pub fn severity_number(v: &Value) -> Option<u8> {
    use trawl_core::severity::Dialect;
    match v {
        Value::Integer(n) => trawl_core::severity::reading_number(*n, Dialect::Otel),
        Value::String(s) => trawl_core::severity::reading_text(s, Dialect::Otel),
        _ => None,
    }
}

/// A `_severity` cell's display text: the `OTel` short name (`17` →
/// `error`, `18` → `error2`), or the raw value where there is no reading.
#[must_use]
pub fn severity_display(v: &Value) -> String {
    severity_number(v)
        .and_then(trawl_core::severity::otel_name)
        .map_or_else(|| trawl_api::display::value_to_string(v), str::to_owned)
}

/// The name a filter-rail value of a severity field shows: the `OTel`
/// short name for a ladder number (`17` → `error`, `18` → `error2`), or
/// `None` where the value is shown as it is.
///
/// The rail keys a value by its text, so a number arrives here as its
/// digits. Only a number's own spelling reads as a rung: any other string
/// the sender wrote (`err`, `error`, `017`) is shown verbatim, which keeps
/// two values the rail counts apart from reading as one name.
#[must_use]
pub fn severity_facet_name(value: &str) -> Option<&'static str> {
    facet_rung(value).and_then(trawl_core::severity::token_text)
}

/// The level ink a severity field's rail value takes, matching the
/// table's pills: `error` for the error and fatal bands, `warn` for the
/// warn band, `None` for every other level and for any value that does
/// not show a name, so info, debug and trace stay in plain ink.
#[must_use]
pub fn severity_facet_ink(value: &str) -> Option<&'static str> {
    let band = facet_rung(value)
        .and_then(|n| u8::try_from(n).ok())
        .and_then(trawl_core::severity::band_name)?;
    match band {
        "error" | "fatal" => Some("error"),
        "warn" => Some("warn"),
        _ => None,
    }
}

/// Orders a severity field's rail values as a scale, most severe first.
///
/// A value that is not a number keeps its place after the numbers, in
/// the order it arrived (count order). Only the order changes: the rail
/// counts and caps the values before this runs.
pub fn order_severity_facet(values: &mut [(String, u32)]) {
    values.sort_by_key(|(v, _)| std::cmp::Reverse(v.parse::<i64>().ok()));
}

/// The number a rail value spells exactly, if it spells one.
fn facet_rung(value: &str) -> Option<i64> {
    value.parse::<i64>().ok().filter(|n| n.to_string() == value)
}

#[must_use]
pub fn severity_class(v: &Value) -> &'static str {
    match severity_number(v).and_then(trawl_core::severity::band_name) {
        Some("error" | "fatal") => "lvl lvl-error",
        Some("warn") => "lvl lvl-warn",
        Some("info") => "lvl lvl-info",
        Some("debug" | "trace") => "lvl lvl-debug",
        _ => "lvl",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_severity_cell_renders_its_otel_short_name() {
        assert_eq!(severity_display(&Value::Integer(17)), "error");
        assert_eq!(severity_display(&Value::Integer(18)), "error2");
        assert_eq!(severity_display(&Value::Integer(13)), "warn");
        assert_eq!(severity_display(&Value::Integer(1)), "trace");
        assert_eq!(severity_display(&Value::Integer(24)), "fatal4");
        assert_eq!(severity_display(&Value::Integer(99)), "99");
        assert_eq!(severity_display(&Value::Null), "NULL");
    }

    #[test]
    fn a_rail_value_names_its_rung_and_keeps_every_other_value() {
        assert_eq!(severity_facet_name("9"), Some("info"));
        assert_eq!(severity_facet_name("13"), Some("warn"));
        assert_eq!(severity_facet_name("17"), Some("error"));
        assert_eq!(severity_facet_name("18"), Some("error2"));
        assert_eq!(severity_facet_name("1"), Some("trace"));
        assert_eq!(severity_facet_name("0"), None);
        assert_eq!(severity_facet_name("25"), None);
        assert_eq!(severity_facet_name("-1"), None);
        assert_eq!(severity_facet_name("err"), None);
        assert_eq!(severity_facet_name("error"), None);
        assert_eq!(severity_facet_name("017"), None, "not the spelling of 17");
        assert_eq!(severity_facet_name("+17"), None, "not the spelling of 17");
        assert_eq!(severity_facet_name(""), None);
    }

    #[test]
    fn a_rail_value_takes_level_ink_only_where_the_table_does() {
        assert_eq!(severity_facet_ink("17"), Some("error"));
        assert_eq!(severity_facet_ink("20"), Some("error"));
        assert_eq!(severity_facet_ink("21"), Some("error"));
        assert_eq!(severity_facet_ink("24"), Some("error"));
        assert_eq!(severity_facet_ink("13"), Some("warn"));
        assert_eq!(severity_facet_ink("16"), Some("warn"));
        assert_eq!(severity_facet_ink("9"), None);
        assert_eq!(severity_facet_ink("5"), None);
        assert_eq!(severity_facet_ink("1"), None);
        assert_eq!(severity_facet_ink("25"), None);
        assert_eq!(
            severity_facet_ink("error"),
            None,
            "shown verbatim, so plain"
        );
        assert_eq!(severity_facet_ink("017"), None, "shown verbatim, so plain");
    }

    #[test]
    fn rail_values_order_most_severe_first() {
        let mut values: Vec<(String, u32)> = [
            ("9", 42),
            ("gold", 7),
            ("13", 4),
            ("1", 2),
            ("17", 2),
            ("err", 1),
        ]
        .into_iter()
        .map(|(v, c)| (v.to_owned(), c))
        .collect();
        order_severity_facet(&mut values);
        let order: Vec<&str> = values.iter().map(|(v, _)| v.as_str()).collect();
        assert_eq!(order, ["17", "13", "9", "1", "gold", "err"]);
        assert_eq!(values[2], ("9".to_owned(), 42), "counts travel with values");
    }

    #[test]
    fn a_severity_cell_colors_by_band() {
        assert_eq!(severity_class(&Value::Integer(17)), "lvl lvl-error");
        assert_eq!(severity_class(&Value::Integer(20)), "lvl lvl-error");
        assert_eq!(severity_class(&Value::Integer(21)), "lvl lvl-error");
        assert_eq!(severity_class(&Value::Integer(13)), "lvl lvl-warn");
        assert_eq!(severity_class(&Value::Integer(9)), "lvl lvl-info");
        assert_eq!(severity_class(&Value::Integer(5)), "lvl lvl-debug");
        assert_eq!(severity_class(&Value::Integer(99)), "lvl");
        assert_eq!(severity_class(&Value::Null), "lvl");
    }

    #[test]
    fn a_string_severity_reads_through_the_same_ladder() {
        assert_eq!(severity_number(&Value::String("error".into())), Some(17));
        assert_eq!(severity_number(&Value::String("error2".into())), Some(18));
        assert_eq!(severity_number(&Value::String("err".into())), Some(17));
        assert_eq!(severity_number(&Value::String("17".into())), Some(17));
        assert_eq!(severity_number(&Value::String(" error ".into())), Some(17));
        assert_eq!(severity_number(&Value::String("gold".into())), None);
        assert_eq!(severity_number(&Value::String("1.5".into())), None);
        assert_eq!(severity_number(&Value::String("25".into())), None);
        assert_eq!(severity_display(&Value::String("gold".into())), "gold");
        assert_eq!(severity_display(&Value::String("17".into())), "error");
    }

    #[test]
    fn an_integer_severity_reads_the_ladder_and_nothing_else() {
        for n in 1..=24i64 {
            assert_eq!(
                severity_number(&Value::Integer(n)),
                u8::try_from(n).ok(),
                "ladder {n}"
            );
        }
        assert_eq!(severity_number(&Value::Integer(0)), None);
        assert_eq!(severity_number(&Value::Integer(25)), None);
        assert_eq!(severity_number(&Value::Integer(-1)), None);
        assert_eq!(severity_number(&Value::Integer(i64::MAX)), None);
        assert_eq!(severity_number(&Value::Float(17.0)), None);
        assert_eq!(severity_number(&Value::Boolean(true)), None);
        assert_eq!(severity_number(&Value::Null), None);
    }

    #[test]
    fn declared_severity_columns_join_the_name_keyed_one() {
        let cols = ["_time", "severity", "_severity", "s"];
        assert_eq!(severity_columns(cols, &[]), vec![2]);
        assert_eq!(
            severity_columns(cols, &["s".to_owned()]),
            vec![2, 3],
            "a declared column joins the name-keyed one"
        );
        assert_eq!(
            severity_columns(["_time", "message"], &["s".to_owned()]),
            Vec::<usize>::new(),
            "a declared name the result does not carry matches nothing"
        );
    }
}
