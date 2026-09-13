// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Client-side facet aggregation from the current result page.
//!
//! For each column whose cells are `String`, `Integer`, or `Boolean`
//! values we compute the top N distinct values and their counts.
//! `Float` is skipped (usually continuous — facets would be noise),
//! `Array` can't be keyed, and `Null`-only columns are dropped.
//!
//! Only the wasm32 build consumes these helpers — on native they exist
//! purely so their tests run under plain `cargo test`. Matches the
//! `offset.rs` pattern.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use trawl_api::value::{QueryResult, Value};

/// Max distinct values surfaced per field.
pub const FACET_TOP_N: usize = 10;

/// One field's facet: `(column_name, sorted (value, count) list)`.
pub type FieldFacet = (String, Vec<(String, u32)>);

/// Compute `(field, [(value, count)])` facets from a query result's rows.
///
/// Fields with zero non-null discrete values are filtered out. Output is
/// ordered by column index (preserves the wire column order). Within a
/// field, values are sorted by count descending, then alphabetically on
/// ties for stable rendering.
#[must_use]
pub fn compute_facets(result: &QueryResult) -> Vec<FieldFacet> {
    let mut out = Vec::with_capacity(result.columns.len());
    for (i, col) in result.columns.iter().enumerate() {
        if let Some(top) = facet_column(&result.rows, i) {
            out.push((col.name.clone(), top));
        }
    }
    out
}

/// Does this query return an aggregation-shaped result?
///
/// The filter rail is suppressed for one. `compute_facets` keys integer
/// cells, so a `stats count() by status` page facets its own `count`
/// column and the include control would build `count="42"` — a search
/// clause `effective_query` prepends to the search stage, naming a field
/// no event carries. The chart surfaces read the same answer to decide
/// whether a result is plottable.
///
/// An empty or unparseable query is not aggregation-shaped: there is no
/// pipeline to read, and the rail's ordinary behaviour is the safe one.
#[must_use]
pub fn is_aggregation_shape(query: &str) -> bool {
    if query.trim().is_empty() {
        return false;
    }
    trawl_core::parser::parse(query).is_ok_and(|ast| ast.has_aggregation())
}

fn facet_column(rows: &[Vec<Value>], idx: usize) -> Option<Vec<(String, u32)>> {
    let mut counts: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    for row in rows {
        let Some(cell) = row.get(idx) else { continue };
        let key = match cell {
            Value::String(s) => s.clone(),
            Value::Integer(i) => i.to_string(),
            Value::Boolean(b) => b.to_string(),
            // Null, Float, Array are intentionally skipped (see module docs).
            _ => continue,
        };
        *counts.entry(key).or_insert(0) += 1;
    }

    if counts.is_empty() {
        return None;
    }

    let mut sorted: Vec<(String, u32)> = counts.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    sorted.truncate(FACET_TOP_N);
    Some(sorted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use trawl_api::value::Column;

    fn col(name: &str) -> Column {
        Column {
            name: name.to_owned(),
        }
    }

    #[test]
    fn counts_strings_descending() {
        let result = QueryResult {
            columns: vec![col("service")],
            rows: vec![
                vec![Value::String("nginx".into())],
                vec![Value::String("api".into())],
                vec![Value::String("nginx".into())],
                vec![Value::String("nginx".into())],
                vec![Value::String("api".into())],
            ],
        };
        let facets = compute_facets(&result);
        assert_eq!(facets.len(), 1);
        assert_eq!(facets[0].0, "service");
        assert_eq!(facets[0].1, vec![("nginx".into(), 3), ("api".into(), 2)]);
    }

    #[test]
    fn skips_floats_and_arrays_and_nulls() {
        let result = QueryResult {
            columns: vec![col("lat"), col("tags"), col("blank")],
            rows: vec![
                vec![
                    Value::Float(1.0),
                    Value::Array(vec![Value::String("a".into())]),
                    Value::Null,
                ],
                vec![
                    Value::Float(2.0),
                    Value::Array(vec![Value::String("b".into())]),
                    Value::Null,
                ],
            ],
        };
        let facets = compute_facets(&result);
        assert!(facets.is_empty());
    }

    #[test]
    fn mixes_integer_boolean_and_string() {
        let result = QueryResult {
            columns: vec![col("status"), col("ok")],
            rows: vec![
                vec![Value::Integer(200), Value::Boolean(true)],
                vec![Value::Integer(200), Value::Boolean(false)],
                vec![Value::Integer(404), Value::Boolean(true)],
            ],
        };
        let facets = compute_facets(&result);
        assert_eq!(facets.len(), 2);
        assert_eq!(facets[0].0, "status");
        assert_eq!(facets[0].1, vec![("200".into(), 2), ("404".into(), 1)]);
        assert_eq!(facets[1].0, "ok");
        assert_eq!(facets[1].1, vec![("true".into(), 2), ("false".into(), 1)]);
    }

    #[test]
    fn caps_at_top_n() {
        let rows: Vec<Vec<Value>> = (0usize..15)
            .flat_map(|i| std::iter::repeat_n(vec![Value::String(format!("v{i}"))], 15 - i))
            .collect();
        let result = QueryResult {
            columns: vec![col("k")],
            rows,
        };
        let facets = compute_facets(&result);
        assert_eq!(facets[0].1.len(), FACET_TOP_N);
        // The 10 highest-frequency values should be v0..v9 (15..6 occurrences).
        assert_eq!(facets[0].1[0].0, "v0");
        assert_eq!(facets[0].1[0].1, 15);
        assert_eq!(facets[0].1[9].0, "v9");
    }

    #[test]
    fn alphabetic_tiebreak_on_equal_counts() {
        let result = QueryResult {
            columns: vec![col("k")],
            rows: vec![
                vec![Value::String("zebra".into())],
                vec![Value::String("apple".into())],
            ],
        };
        let facets = compute_facets(&result);
        // Both count 1 — alphabetic order wins the tie.
        assert_eq!(facets[0].1[0].0, "apple");
        assert_eq!(facets[0].1[1].0, "zebra");
    }

    #[test]
    fn stats_by_is_aggregation_shaped() {
        assert!(is_aggregation_shape("last=15m * | stats count() by status"));
        assert!(is_aggregation_shape(
            "service=web | timechart span=1h count()"
        ));
    }

    #[test]
    fn a_plain_search_on_an_integer_field_is_not_aggregation_shaped() {
        assert!(!is_aggregation_shape("last=15m status=200"));
        assert!(!is_aggregation_shape("* | head 10"));
        // Nothing to read is not a shape.
        assert!(!is_aggregation_shape(""));
        assert!(!is_aggregation_shape("   "));
        assert!(!is_aggregation_shape("| | |"));
    }

    /// Why the gate exists: an aggregation page facets its own aggregate
    /// column, and the include control would then build `count="2"` — a
    /// search-stage clause naming a field no event carries.
    #[test]
    fn an_aggregation_result_would_facet_its_aggregate_column() {
        let result = QueryResult {
            columns: vec![col("status"), col("count")],
            rows: vec![
                vec![Value::String("200".into()), Value::Integer(2)],
                vec![Value::String("404".into()), Value::Integer(2)],
            ],
        };
        let facets = compute_facets(&result);
        assert!(
            facets
                .iter()
                .any(|(field, values)| field == "count" && values.contains(&("2".to_string(), 2))),
            "{facets:?}"
        );
    }
}
