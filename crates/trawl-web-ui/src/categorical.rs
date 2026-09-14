// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shape detection for the categorical bar chart beside an aggregate
//! result, and the group columns the exact table offers a search on.
//!
//! Both answers are read off the executed DSL and the response together,
//! never off the response alone: "which column is the group" is a
//! question only the `stats … by <field>` stage can answer, and a column
//! the query did not group by is a generated metric that no filter can
//! name (functional finding F02).
//!
//! At the crate root rather than under `components/`, which is
//! wasm-gated, so these decisions are tested on native
//! `cargo nextest run -p trawl-web-ui`.
//!
//! On native, only the tests consume these items — `#[allow(dead_code)]`
//! at module scope silences the bin-crate dead-code warning. Matches the
//! `facets.rs` / `query_merge.rs` pattern.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use trawl_api::value::{QueryResult, Value};
use trawl_core::ast::PipeStage;

/// A `stats <metric> by <field>` result the categorical chart can draw:
/// one group column, one numeric metric column, and the scale to draw
/// the bars against.
#[derive(Debug, Clone, PartialEq)]
pub struct CatShape {
    /// Column index of the grouped field.
    pub group: usize,
    /// Column index of the generated metric.
    pub metric: usize,
    pub group_name: String,
    pub metric_name: String,
    /// Whether any value is negative, which makes the track two-sided.
    pub signed: bool,
    /// The largest magnitude in the metric column; `0.0` when every
    /// value is zero or null.
    pub max_abs: f64,
}

impl CatShape {
    /// Bar length for one metric value, as a percentage of the track.
    ///
    /// A signed track is drawn from its midpoint, so each side gets half
    /// the width. Zero when there is no scale to draw against, which is
    /// what an all-zero column has.
    #[must_use]
    pub fn percent(&self, value: &Value) -> f64 {
        let Some(v) = numeric(value) else {
            return 0.0;
        };
        if self.max_abs <= 0.0 {
            return 0.0;
        }
        let full = if self.signed { 50.0 } else { 100.0 };
        v.abs() / self.max_abs * full
    }
}

/// The `CatShape` of this result, or `None` when the pair is anything
/// else.
///
/// `None` is the common answer and not a failure: the exact table is the
/// accessible representation and renders alone. A chart is offered only
/// for the one shape it can draw honestly — a single grouped field, a
/// single generated metric, and cells that are all numbers or nulls.
#[must_use]
pub fn detect(query: &str, result: &QueryResult) -> Option<CatShape> {
    let stats = last_stats(query)?;
    if stats.group_by.len() != 1 || stats.aggregations.len() != 1 || result.columns.len() != 2 {
        return None;
    }
    // An empty page has no scale and nothing to draw.
    if result.rows.is_empty() {
        return None;
    }
    let names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
    // Two columns of the same name cannot be told apart, and guessing
    // which one the reader means is how a chart ends up labelled with
    // the wrong field.
    if names[0] == names[1] {
        return None;
    }
    let group = names.iter().position(|n| *n == stats.group_by[0])?;
    let metric = 1 - group;

    let mut signed = false;
    let mut max_abs = 0.0_f64;
    for row in &result.rows {
        if row.len() != result.columns.len() {
            return None;
        }
        let cell = &row[metric];
        if matches!(cell, Value::Null) {
            continue;
        }
        let Some(v) = numeric(cell) else {
            // A string, boolean or list in the metric column: not a
            // magnitude, so there is no bar to draw for it.
            return None;
        };
        if v < 0.0 {
            signed = true;
        }
        if v.is_finite() {
            max_abs = max_abs.max(v.abs());
        }
    }

    Some(CatShape {
        group,
        metric,
        group_name: names[group].to_owned(),
        metric_name: names[metric].to_owned(),
        signed,
        max_abs,
    })
}

/// Indices of the result columns the query actually grouped by.
///
/// These are the only columns the exact table offers a search on: every
/// other column is a generated metric, and a filter naming one would
/// advertise a field the corpus has never held (ADR-0025, F02). Any
/// pipeline whose last stage is not `stats` groups by nothing.
#[must_use]
pub fn group_columns(query: &str, columns: &[String]) -> Vec<usize> {
    let Some(stats) = last_stats(query) else {
        return Vec::new();
    };
    columns
        .iter()
        .enumerate()
        .filter(|(_, name)| stats.group_by.iter().any(|g| g == *name))
        .map(|(i, _)| i)
        .collect()
}

/// The query's last `stats` stage, if that is what it ends with.
///
/// The LAST stage, because a later `sort` or `head` does not change what
/// the columns mean but an earlier `stats` followed by another one does:
/// only the final aggregation names the columns the response carries.
fn last_stats(query: &str) -> Option<trawl_core::ast::StatsStage> {
    let ast = trawl_core::parser::parse(query).ok()?;
    match &ast.pipeline.last()?.node {
        PipeStage::Stats(stats) => Some(stats.clone()),
        _ => None,
    }
}

/// A metric cell as an `f64`, or `None` when it is not a number.
#[allow(clippy::cast_precision_loss)]
fn numeric(value: &Value) -> Option<f64> {
    match value {
        // Bar length is a ratio a few hundred pixels wide, so losing the
        // low bits of an i64 past 2^53 cannot move a bar by a pixel.
        Value::Integer(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trawl_api::value::Column;

    const BY_STATUS: &str = "* | stats count() by status";

    fn result(names: &[&str], rows: Vec<Vec<Value>>) -> QueryResult {
        QueryResult {
            columns: names
                .iter()
                .map(|n| Column {
                    name: (*n).to_string(),
                })
                .collect(),
            rows,
        }
    }

    fn int_rows() -> Vec<Vec<Value>> {
        vec![
            vec![Value::String("200".into()), Value::Integer(940)],
            vec![Value::String("404".into()), Value::Integer(150)],
        ]
    }

    #[test]
    fn detect_reads_the_grouped_field_and_its_metric() {
        let shape = detect(BY_STATUS, &result(&["status", "count"], int_rows())).unwrap();
        assert_eq!(shape.group, 0);
        assert_eq!(shape.metric, 1);
        assert_eq!(shape.group_name, "status");
        assert_eq!(shape.metric_name, "count");
        assert!(!shape.signed);
        assert!((shape.max_abs - 940.0).abs() < f64::EPSILON);
    }

    #[test]
    fn detect_finds_the_group_whichever_column_it_is() {
        // The emitter is free to order the columns; the group is named
        // by the query, never by position.
        let shape = detect(
            BY_STATUS,
            &result(
                &["count", "status"],
                vec![vec![Value::Integer(12), Value::String("500".into())]],
            ),
        )
        .unwrap();
        assert_eq!(shape.group, 1);
        assert_eq!(shape.metric, 0);
    }

    #[test]
    fn detect_accepts_an_aliased_metric() {
        let shape = detect(
            "* | stats count() as n by status",
            &result(&["status", "n"], int_rows()),
        )
        .unwrap();
        assert_eq!(shape.metric_name, "n");
    }

    #[test]
    fn detect_tolerates_null_metrics() {
        let shape = detect(
            BY_STATUS,
            &result(
                &["status", "count"],
                vec![
                    vec![Value::String("200".into()), Value::Integer(940)],
                    vec![Value::String("301".into()), Value::Null],
                ],
            ),
        )
        .unwrap();
        assert!((shape.max_abs - 940.0).abs() < f64::EPSILON);
        assert!((shape.percent(&Value::Null) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn detect_marks_a_negative_column_signed_and_halves_the_track() {
        let shape = detect(
            BY_STATUS,
            &result(
                &["status", "count"],
                vec![
                    vec![Value::String("200".into()), Value::Integer(940)],
                    vec![Value::String("503".into()), Value::Integer(-940)],
                ],
            ),
        )
        .unwrap();
        assert!(shape.signed);
        // A two-sided track draws each side from the midpoint.
        assert!((shape.percent(&Value::Integer(-940)) - 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn detect_handles_an_all_zero_column_without_dividing_by_it() {
        let shape = detect(
            BY_STATUS,
            &result(
                &["status", "count"],
                vec![
                    vec![Value::String("200".into()), Value::Integer(0)],
                    vec![Value::String("404".into()), Value::Integer(0)],
                ],
            ),
        )
        .unwrap();
        assert!((shape.max_abs - 0.0).abs() < f64::EPSILON);
        assert!((shape.percent(&Value::Integer(0)) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn detect_scales_large_values_against_the_largest() {
        let shape = detect(
            BY_STATUS,
            &result(
                &["status", "count"],
                vec![
                    vec![Value::String("200".into()), Value::Float(2.5e9)],
                    vec![Value::String("404".into()), Value::Float(5.0e8)],
                ],
            ),
        )
        .unwrap();
        assert!((shape.percent(&Value::Float(5.0e8)) - 20.0).abs() < 1e-9);
        assert!((shape.percent(&Value::Float(2.5e9)) - 100.0).abs() < 1e-9);
    }

    #[test]
    fn detect_refuses_an_empty_page() {
        assert_eq!(
            detect(BY_STATUS, &result(&["status", "count"], vec![])),
            None
        );
    }

    #[test]
    fn detect_refuses_a_non_numeric_metric() {
        assert_eq!(
            detect(
                BY_STATUS,
                &result(
                    &["status", "count"],
                    vec![vec![
                        Value::String("200".into()),
                        Value::String("many".into())
                    ]],
                ),
            ),
            None
        );
        assert_eq!(
            detect(
                BY_STATUS,
                &result(
                    &["status", "count"],
                    vec![vec![Value::String("200".into()), Value::Boolean(true)]],
                ),
            ),
            None
        );
    }

    #[test]
    fn detect_refuses_duplicate_column_names() {
        assert_eq!(
            detect(
                BY_STATUS,
                &result(
                    &["status", "status"],
                    vec![vec![Value::String("200".into()), Value::Integer(1)]],
                ),
            ),
            None
        );
    }

    #[test]
    fn detect_refuses_a_row_that_is_not_as_wide_as_the_header() {
        assert_eq!(
            detect(
                BY_STATUS,
                &result(
                    &["status", "count"],
                    vec![vec![Value::String("200".into())]],
                ),
            ),
            None
        );
    }

    #[test]
    fn detect_refuses_a_result_that_is_not_two_columns() {
        assert_eq!(
            detect(
                "* | stats count() by status, host",
                &result(
                    &["status", "host", "count"],
                    vec![vec![
                        Value::String("200".into()),
                        Value::String("web-01".into()),
                        Value::Integer(3),
                    ]],
                ),
            ),
            None
        );
    }

    #[test]
    fn detect_refuses_more_than_one_aggregation() {
        assert_eq!(
            detect(
                "* | stats count(), avg(latency_ms) by status",
                &result(&["status", "count"], int_rows()),
            ),
            None
        );
    }

    #[test]
    fn detect_refuses_pipelines_that_are_not_a_grouped_stats() {
        for q in [
            "* | top 10 status",
            "* | timechart span=1h count()",
            "* | stats count()",
            "status=200",
            "",
            "| | |",
        ] {
            assert_eq!(
                detect(q, &result(&["status", "count"], int_rows())),
                None,
                "{q}"
            );
        }
    }

    #[test]
    fn detect_refuses_a_stats_that_is_not_the_last_stage() {
        // A later stage renames or drops columns, so the grouping the
        // response carries is no longer the one this stats named.
        assert_eq!(
            detect(
                "* | stats count() by status | sort count",
                &result(&["status", "count"], int_rows()),
            ),
            None
        );
    }

    #[test]
    fn group_columns_names_only_the_grouped_fields() {
        let columns = vec!["status".to_string(), "count".to_string()];
        assert_eq!(group_columns(BY_STATUS, &columns), vec![0]);
    }

    #[test]
    fn group_columns_covers_every_grouped_field() {
        let columns = vec![
            "status".to_string(),
            "host".to_string(),
            "count".to_string(),
        ];
        assert_eq!(
            group_columns("* | stats count() by status, host", &columns),
            vec![0, 1]
        );
    }

    #[test]
    fn group_columns_is_empty_for_any_other_pipeline() {
        let columns = vec!["status".to_string(), "count".to_string()];
        for q in ["* | top 10 status", "* | stats count()", "status=200", ""] {
            assert!(group_columns(q, &columns).is_empty(), "{q}");
        }
    }
}
