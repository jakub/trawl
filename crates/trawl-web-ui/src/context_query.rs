// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The "Show context" query builder: a ±30s window around one result row.
//!
//! Pure and ungated so its tests run natively; the one caller (the results
//! table's row actions) is wasm32-only. The window names the canonical
//! `_time` column, because the DSL has no aliases (ADR-0013 §6): naming
//! `@timestamp` here would filter on an ordinary sender field most events
//! never carry, and Show context would silently return nothing.

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

/// A new snapshot search with an explicit effective window.
#[derive(Clone, Debug)]
pub(crate) struct SearchNavigation {
    pub query: String,
    pub range: crate::query_merge::RangeSpec,
}

/// Build a same-host search and an absolute ±30-second picker range.
pub(crate) fn build_context_query(row: &[Value], columns: &[String]) -> Option<SearchNavigation> {
    let ti = find_col(columns, &["_time"])?;
    let ts_raw = value_to_string(row.get(ti)?);
    let ts = fleet_ui::time::parse_timestamp(&ts_raw)?;
    let from = ts.checked_sub_signed(chrono::Duration::seconds(30))?;
    let to = ts.checked_add_signed(chrono::Duration::seconds(30))?;
    let query = find_col(columns, &["host"])
        .and_then(|hi| row.get(hi))
        .and_then(|v| match v {
            Value::String(s) if !s.is_empty() => Some(s),
            _ => None,
        })
        .map_or_else(|| "*".to_string(), |h| format!("host=\"{}\"", escape_dq(h)));
    Some(SearchNavigation {
        query,
        range: crate::query_merge::RangeSpec::Absolute {
            from: from.to_rfc3339(),
            to: to.to_rfc3339(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn historical_context_has_its_own_absolute_window() {
        let nav = build_context_query(
            &[
                Value::String("2026-01-01T12:00:00Z".into()),
                Value::String("we\"ird\\host".into()),
            ],
            &["_time".into(), "host".into()],
        )
        .unwrap();
        assert_eq!(
            nav.range,
            crate::query_merge::RangeSpec::Absolute {
                from: "2026-01-01T11:59:30+00:00".into(),
                to: "2026-01-01T12:00:30+00:00".into()
            }
        );
        let effective = crate::search_url::mode_query(
            crate::search_url::Mode::Snapshot,
            &nav.query,
            &[],
            &nav.range,
        );
        assert!(!effective.contains("last="));
        assert!(effective.contains(r#"_time>="2026-01-01T11:59:30+00:00""#));
        assert!(effective.contains(r#"_time<="2026-01-01T12:00:30+00:00""#));
        assert!(trawl_core::parser::parse(&effective).is_ok());
        assert!(nav.query.starts_with("host="));
    }
    #[test]
    fn sender_timestamp_and_null_host_are_not_canonical_metadata() {
        assert!(
            build_context_query(
                &[Value::String("2026-01-01T12:00:00Z".into())],
                &["timestamp".into()]
            )
            .is_none()
        );
        let nav = build_context_query(
            &[Value::String("2026-01-01T12:00:00Z".into()), Value::Null],
            &["_time".into(), "host".into()],
        )
        .unwrap();
        assert_eq!(nav.query, "*");
    }
}
