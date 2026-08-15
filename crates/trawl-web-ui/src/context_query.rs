// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The "Show context" query builder: a ±30s window around one result row.
//!
//! Pure + ungated so its tests run natively; the one caller (the results
//! table's row actions) is wasm32-only. The window names the canonical
//! `_time` column, because the DSL has zero aliases since ADR-0013 §6 —
//! reintroducing `@timestamp` here would emit a filter on an ordinary
//! sender field most events never carry, and Show context would silently
//! return nothing.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use trawl_api::display::value_to_string;
use trawl_api::value::Value;

/// Index of the first of `names` present in `columns`.
pub(crate) fn find_col(columns: &[String], names: &[&str]) -> Option<usize> {
    for name in names {
        if let Some(i) = columns.iter().position(|c| c == name) {
            return Some(i);
        }
    }
    None
}

/// Escape a value for embedding in a double-quoted DSL literal.
pub(crate) fn escape_dq(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Build a `_time>="<t-30s>" _time<="<t+30s>"` window around this row's
/// timestamp, narrowed to the same host when available.
pub(crate) fn build_context_query(row: &[Value], columns: &[String]) -> Option<String> {
    let ti = find_col(columns, &["_time", "time", "timestamp", "@timestamp"])?;
    let ts_raw = value_to_string(row.get(ti)?);
    let ts = fleet_ui::time::parse_timestamp(&ts_raw)?;
    let from = ts - chrono::Duration::seconds(30);
    let to = ts + chrono::Duration::seconds(30);

    let host_clause = find_col(columns, &["host", "hostname"])
        .and_then(|hi| row.get(hi))
        .map(value_to_string)
        .filter(|s| !s.is_empty())
        .map(|h| format!("host=\"{}\" ", escape_dq(&h)))
        .unwrap_or_default();

    Some(format!(
        "{host_clause}_time>=\"{}\" _time<=\"{}\"",
        from.to_rfc3339(),
        to.to_rfc3339()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use trawl_core::ast::{FilterOp, FilterValue, SearchToken};

    fn cols(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_string()).collect()
    }

    /// Every `field op value` token the built query parses into.
    fn filters(query: &str) -> Vec<(String, FilterOp, String)> {
        let parsed = trawl_core::parser::parse(query).expect("built query must parse");
        parsed
            .search
            .all_tokens()
            .filter_map(|t| match &t.node {
                SearchToken::FieldFilter(f) => match &f.value {
                    FilterValue::Literal(v) => Some((f.field.clone(), f.op, v.clone())),
                    FilterValue::List(_) => None,
                },
                _ => None,
            })
            .collect()
    }

    #[test]
    fn window_names_time_twice_and_no_retired_alias() {
        let q = build_context_query(
            &[Value::String("2026-08-14T12:00:00Z".into())],
            &cols(&["_time"]),
        )
        .expect("parseable timestamp");

        assert_eq!(
            filters(&q),
            vec![
                (
                    "_time".to_string(),
                    FilterOp::Gte,
                    "2026-08-14T11:59:30+00:00".to_string()
                ),
                (
                    "_time".to_string(),
                    FilterOp::Lte,
                    "2026-08-14T12:00:30+00:00".to_string()
                ),
            ]
        );
        assert!(!q.contains("@timestamp"), "retired alias in {q}");
        assert!(!q.contains("timestamp"), "retired alias in {q}");
    }

    #[test]
    fn timestamp_column_variants_all_produce_time_bounds() {
        for name in ["_time", "time", "timestamp", "@timestamp"] {
            let q = build_context_query(
                &[Value::String("2026-08-14 12:00:00".into())],
                &cols(&[name]),
            )
            .unwrap_or_else(|| panic!("column {name} should be found"));
            let bounds: Vec<_> = filters(&q)
                .into_iter()
                .filter(|(f, _, _)| f == "_time")
                .collect();
            assert_eq!(bounds.len(), 2, "column {name} produced {q}");
            assert_eq!(bounds[0].1, FilterOp::Gte);
            assert_eq!(bounds[1].1, FilterOp::Lte);
        }
    }

    #[test]
    fn host_is_included_and_quotes_are_escaped() {
        let q = build_context_query(
            &[
                Value::String("2026-08-14T12:00:00Z".into()),
                Value::String("we\"ird\\host".into()),
            ],
            &cols(&["_time", "host"]),
        )
        .expect("parseable timestamp");

        assert_eq!(
            filters(&q).first().cloned(),
            Some((
                "host".to_string(),
                FilterOp::Eq,
                "we\"ird\\host".to_string()
            )),
            "host filter must round-trip through the parser: {q}"
        );
    }

    #[test]
    fn empty_host_is_omitted() {
        let q = build_context_query(
            &[
                Value::String("2026-08-14T12:00:00Z".into()),
                Value::String(String::new()),
            ],
            &cols(&["_time", "host"]),
        )
        .expect("parseable timestamp");
        assert!(!q.contains("host="), "{q}");
        assert_eq!(filters(&q).len(), 2);
    }

    #[test]
    fn hostname_column_is_the_host_fallback() {
        let q = build_context_query(
            &[
                Value::String("2026-08-14T12:00:00Z".into()),
                Value::String("web-01".into()),
            ],
            &cols(&["_time", "hostname"]),
        )
        .expect("parseable timestamp");
        assert_eq!(
            filters(&q).first().cloned(),
            Some(("host".to_string(), FilterOp::Eq, "web-01".to_string())),
            "{q}"
        );
    }

    #[test]
    fn no_timestamp_column_or_unparseable_value_yields_nothing() {
        assert!(build_context_query(&[Value::String("x".into())], &cols(&["message"])).is_none());
        assert!(
            build_context_query(
                &[Value::String("not a timestamp".into())],
                &cols(&["_time"])
            )
            .is_none()
        );
    }
}
