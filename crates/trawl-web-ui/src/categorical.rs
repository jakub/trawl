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

use std::collections::HashSet;

use trawl_api::value::{QueryResult, Value};
use trawl_core::ast::{PipeStage, Spanned};
use trawl_core::projection::agg_output_name;
use trawl_core::schema::{TIME, catalog_key};

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
    // `by STATUS` groups the catalog's `status`, and the response names
    // the column as the catalog does; fold both sides, as `group_columns`
    // does, so the two never disagree about which column is the group.
    let group = names
        .iter()
        .position(|n| catalog_key(n) == catalog_key(&stats.group_by[0]))?;
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

/// Indices of the result columns the query actually grouped by AND can
/// still be searched for.
///
/// These are the only columns the exact table offers a search on: every
/// other column is a generated metric, and a filter naming one would
/// advertise a field the corpus has never held (ADR-0025, F02). Any
/// pipeline whose last stage is not `stats` groups by nothing.
///
/// Being grouped by is not enough on its own. A search prepends
/// `<field>="<value>"` BEFORE the first pipe, so the name has to be one
/// the corpus holds at that point — and `stats count() as n by service
/// | stats count() by n` groups by `n`, a number the first aggregation
/// invented. Provenance is established by walking the stages ahead of
/// the final `stats`: a name any of them minted is not a field, and a
/// pipeline that mints names nobody can enumerate ahead of the data
/// leaves no name searchable at all.
#[must_use]
pub fn group_columns(query: &str, columns: &[String]) -> Vec<usize> {
    let Ok(ast) = trawl_core::parser::parse(query) else {
        return Vec::new();
    };
    let Some((last, earlier)) = ast.pipeline.split_last() else {
        return Vec::new();
    };
    let PipeStage::Stats(stats) = &last.node else {
        return Vec::new();
    };
    let Some(minted) = minted_before(earlier) else {
        return Vec::new();
    };
    // `by HOST` groups the catalog's `host`, and the response names the
    // column as the catalog does, so both sides fold before they meet.
    columns
        .iter()
        .enumerate()
        .filter(|(_, name)| {
            stats
                .group_by
                .iter()
                .any(|g| catalog_key(g) == catalog_key(name))
        })
        .filter(|(_, name)| !minted.contains(&catalog_key(name)))
        .map(|(i, _)| i)
        .collect()
}

/// Every column name the stages ahead of the final `stats` mint, or
/// `None` when a stage mints names that cannot be known without the
/// data.
///
/// Names fold through [`catalog_key`], so a `stats count() as Total`
/// disqualifies a later group by `total`: the two are one column.
///
/// `extract`, `pivot` and `from saved` are the unknowable three. An
/// extraction's fields come out of a regex or a key/value scan of the
/// message, a pivot's columns are the VALUES of its `on` field, and a
/// saved run carries whatever schema it was written with. None of them
/// can be enumerated from the query text, and a name that might have
/// been minted is a name whose provenance is not established — so the
/// whole result withholds the search rather than guessing.
fn minted_before(stages: &[Spanned<PipeStage>]) -> Option<HashSet<String>> {
    fn mint(set: &mut HashSet<String>, name: &str) {
        set.insert(catalog_key(name));
    }
    let mut minted = HashSet::new();
    for stage in stages {
        match &stage.node {
            PipeStage::Stats(s) => {
                for agg in &s.aggregations {
                    mint(&mut minted, &agg_output_name(agg));
                }
            }
            PipeStage::EventStats(s) => {
                for agg in &s.aggregations {
                    mint(&mut minted, &agg_output_name(agg));
                }
            }
            PipeStage::Timechart(t) => {
                mint(&mut minted, TIME);
                for agg in &t.aggregations {
                    mint(&mut minted, &agg_output_name(agg));
                }
            }
            // `top`/`rare` desugar to a frequency column they always
            // spell `count` and can never rename.
            PipeStage::Top(_) | PipeStage::Rare(_) => mint(&mut minted, "count"),
            PipeStage::Let(l) => {
                for (name, _) in &l.assignments {
                    mint(&mut minted, name);
                }
            }
            // The new name of a rename is the computed one; the old name
            // is gone from the output either way.
            PipeStage::Rename(r) => {
                for (_, to) in &r.renames {
                    mint(&mut minted, to);
                }
            }
            PipeStage::Extract(_) | PipeStage::Pivot(_) | PipeStage::FromSaved(_) => return None,
            // Filtering, ordering, projecting and deduplicating stages
            // pass names through; none of them invents one.
            PipeStage::Where(_)
            | PipeStage::Sort(_)
            | PipeStage::Limit(_)
            | PipeStage::Table(_)
            | PipeStage::Drop(_)
            | PipeStage::Dedup(_)
            | PipeStage::Tail(_)
            | PipeStage::Sample(_) => {}
        }
    }
    Some(minted)
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
        Value::UInt(u) => Some(*u as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

/// The bars are measured against the WHOLE fetched result, not the page
/// on screen.
///
/// `detect` builds `max_abs` from every row it is handed, and under
/// `FetchPlan::Whole` that is every group the execution produced. So a
/// row's width is a property of the result, and paging cannot change it.
/// Detecting over a page instead would rescale the bars on every turn,
/// which is the defect this pins.
///
/// At module scope rather than inside `mod tests`, so
/// `cargo nextest run -p trawl-web-ui categorical::scale_is_taken_from_the_fetched_result`
/// selects this one test by its path.
#[cfg(test)]
#[test]
fn scale_is_taken_from_the_fetched_result() {
    use trawl_api::value::Column;

    let query = "* | stats count() by status";
    let columns: Vec<Column> = ["status", "count"]
        .iter()
        .map(|n| Column {
            name: (*n).to_string(),
        })
        .collect();
    // 60 groups, the largest on row 55 — past the first page, which is
    // exactly the row a page-scoped scale would never see.
    let rows: Vec<Vec<Value>> = (0..60)
        .map(|i| {
            vec![
                Value::String(format!("{}", 200 + i)),
                Value::Integer(if i == 55 { 900 } else { 9 }),
            ]
        })
        .collect();
    let whole = QueryResult {
        columns: columns.clone(),
        rows: rows.clone(),
    };
    let first_page = QueryResult {
        columns,
        rows: rows[..50].to_vec(),
    };

    let shape = detect(query, &whole).expect("a stats-by result with one numeric metric");
    assert!((shape.max_abs - 900.0).abs() < 1e-9);

    // One scale, read once, answering for rows on either page: row 0 is
    // drawn on page 0 and row 55 on page 1, and neither width depends on
    // which slice was handed to the chart.
    let row_0 = shape.percent(&rows[0][shape.metric]);
    let row_55 = shape.percent(&rows[55][shape.metric]);
    assert!((row_0 - 1.0).abs() < 1e-9, "row 0 is 9 of 900: {row_0}");
    assert!((row_55 - 100.0).abs() < 1e-9, "row 55 fills the track");

    // The contrast: a scale detected over the first page alone makes the
    // same row-0 value fill the whole track.
    let page_shape = detect(query, &first_page).expect("the page is a stats-by result too");
    assert!((page_shape.max_abs - 9.0).abs() < 1e-9);
    let page_row_0 = page_shape.percent(&rows[0][page_shape.metric]);
    assert!((page_row_0 - 100.0).abs() < 1e-9, "{page_row_0}");
    assert!((page_row_0 - row_0).abs() > 1.0, "{page_row_0} vs {row_0}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use trawl_api::value::Column;

    const BY_STATUS: &str = "* | stats count() by status";

    /// An empty answer only counts as a refusal when the query parsed:
    /// a syntax error withholds the same way and would prove nothing.
    fn withheld(query: &str, columns: &[String]) -> bool {
        assert!(
            trawl_core::parser::parse(query).is_ok(),
            "the query must parse: {query}"
        );
        group_columns(query, columns).is_empty()
    }

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
    fn detect_folds_the_group_name_like_the_catalog() {
        let shape = detect(
            "* | stats count() by STATUS",
            &result(&["status", "count"], int_rows()),
        )
        .unwrap();
        assert_eq!(
            shape.group, 0,
            "the grouping key folds to the column the response carries"
        );
        assert_eq!(shape.group_name, "status");
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
        // The grouping key folds to the catalog name the column carries.
        assert_eq!(
            group_columns("* | stats count() by STATUS", &columns),
            vec![0]
        );
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
    fn group_columns_withholds_a_group_by_on_an_earlier_metric() {
        // `n` is a count the first aggregation invented. A search on it
        // would prepend `n="940"` ahead of the first pipe, where no such
        // field has ever existed.
        let columns = vec!["n".to_string(), "count".to_string()];
        assert!(withheld(
            "* | stats count() as n by service | stats count() by n",
            &columns
        ));
    }

    #[test]
    fn group_columns_withholds_an_alias_that_shadows_a_real_field() {
        // `service` is a real field, but this pipeline's `service`
        // column holds the first stage's counts, so the values in it
        // are not service names.
        let columns = vec!["service".to_string(), "count".to_string()];
        assert!(withheld(
            "* | stats count() as service by host | stats count() by service",
            &columns
        ));
    }

    #[test]
    fn group_columns_folds_case_when_it_matches_a_minted_name() {
        let columns = vec!["total".to_string(), "count".to_string()];
        assert!(withheld(
            "* | stats count() as Total by service | stats count() by total",
            &columns
        ));
    }

    #[test]
    fn group_columns_withholds_a_computed_field() {
        let columns = vec!["bucket".to_string(), "count".to_string()];
        assert!(withheld(
            "* | let bucket = lower(service) | stats count() by bucket",
            &columns
        ));
    }

    #[test]
    fn group_columns_withholds_everything_after_an_unenumerable_stage() {
        // An extraction's field names come out of the data, so no name
        // after one has established provenance — including a real one.
        let columns = vec!["service".to_string(), "count".to_string()];
        assert!(withheld(
            "* | extract \"(?P<ip>[0-9.]+)\" from message | stats count() by service",
            &columns
        ));
    }

    #[test]
    fn group_columns_survives_stages_that_only_pass_names_through() {
        // Filtering and ordering mint nothing, so the field the last
        // stage groups by is still the corpus's own.
        let columns = vec!["status".to_string(), "count".to_string()];
        assert_eq!(
            group_columns(
                "service=nginx | sort _time | stats count() by status",
                &columns
            ),
            vec![0]
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
