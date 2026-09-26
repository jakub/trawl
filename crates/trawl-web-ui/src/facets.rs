// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Client-side facet aggregation from the current result page.
//!
//! For each column whose cells are `String`, `Integer`, `UInt`, or
//! `Boolean` values we compute the top N distinct values and their
//! counts.
//! `Float` is skipped (usually continuous — facets would be noise),
//! `Array` can't be keyed, and `Null`-only columns are dropped.
//!
//! Two kinds of column never earn a facet, because counting their values
//! groups nothing (the Filter rail in `context.md`):
//!
//! - the reserved non-dimensions, [`trawl_core::schema::NON_DIMENSION_FIELDS`]
//!   (`_time`, `_ingested`, `_raw`);
//! - a one-off time column: at least two non-null cells on screen, every
//!   one a string [`fleet_ui::time::parse_timestamp`] accepts, and no two
//!   equal. A column that is only all-distinct (hosts, request ids) or a
//!   time column with a repeat (`build_time`) stays.
//!
//! The snapshot page and the live ring both reach this module through
//! [`compute_facets`], so the two lanes cannot disagree about eligibility.
//! [`rail_page`] wraps it with the capability gate: its answer is both
//! the groups the rail renders and whether the page is countable, which
//! opens or closes the wide rail (ADR-0044).
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
/// Fields with zero non-null discrete values are filtered out, and so are
/// the columns the module docs name as never a dimension. Output is
/// ordered by column index (preserves the wire column order). Within a
/// field, values are sorted by count descending, then alphabetically on
/// ties for stable rendering.
#[must_use]
pub fn compute_facets(result: &QueryResult) -> Vec<FieldFacet> {
    let mut out = Vec::with_capacity(result.columns.len());
    for (i, col) in result.columns.iter().enumerate() {
        if trawl_core::schema::is_non_dimension(&col.name) || is_one_off_time(&result.rows, i) {
            continue;
        }
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

/// What the filter rail has to show for one settled answer (ADR-0044).
///
/// A countable page opens the wide rail and every other answer closes
/// it, until the reader opens or closes the rail by hand. An open rail
/// on a page that is not countable shows [`RailPage::hint`] in place of
/// the value search and the groups.
#[cfg_attr(
    target_arch = "wasm32",
    expect(dead_code, reason = "the Search page wires the rail to it next")
)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RailPage {
    /// At least one field the rail may count; the groups, before the value search.
    Countable(Vec<FieldFacet>),
    /// Rows the rail may not count, or none at all: an empty page, a
    /// page of reserved or one-off time columns, a source whose fields
    /// the pipeline may have rewritten. The page's caller also settles a
    /// failed query and a malformed link here.
    NothingToCount,
    /// An aggregation-shaped result (see [`is_aggregation_shape`]).
    Aggregate,
}

#[cfg_attr(
    target_arch = "wasm32",
    expect(dead_code, reason = "the Search page wires the rail to it next")
)]
impl RailPage {
    /// Whether this page opens the wide rail in automatic mode.
    #[must_use]
    pub fn is_countable(&self) -> bool {
        matches!(self, Self::Countable(_))
    }

    /// The line an open rail shows when it has nothing to count, verbatim
    /// from ADR-0044; `None` for a countable page, which shows its groups.
    #[must_use]
    pub fn hint(&self) -> Option<&'static str> {
        match self {
            Self::Countable(_) => None,
            Self::NothingToCount => Some("No field values to count."),
            Self::Aggregate => Some("Field values are not counted for an aggregate result."),
        }
    }
}

/// Decide what the filter rail shows for one answer.
///
/// `query` is the query that PRODUCED `result` — the response's own
/// effective query for a snapshot, the stream's effective query in
/// live — never the editor's draft.
///
/// The aggregation shape is read first, because `top`, `rare` and
/// `pivot` reach [`Capabilities`] as sources it cannot vouch for, and
/// the reader should hear that the result is an aggregate, not that it
/// has nothing to count. Otherwise the groups are [`compute_facets`]'s,
/// less every field the pipeline may have rewritten; that list is what
/// the rail renders, so one answer is counted once.
///
/// [`Capabilities`]: crate::result_actions::Capabilities
#[cfg_attr(
    target_arch = "wasm32",
    expect(dead_code, reason = "the Search page wires the rail to it next")
)]
#[must_use]
pub fn rail_page(query: &str, result: &QueryResult) -> RailPage {
    if is_aggregation_shape(query) {
        return RailPage::Aggregate;
    }
    let provenance = crate::result_actions::Capabilities::for_query(query);
    if !provenance.raw_facets() {
        return RailPage::NothingToCount;
    }
    let groups: Vec<FieldFacet> = compute_facets(result)
        .into_iter()
        .filter(|(field, _)| provenance.input_field(field))
        .collect();
    if groups.is_empty() {
        RailPage::NothingToCount
    } else {
        RailPage::Countable(groups)
    }
}

/// Whether every non-null cell of the column is a distinct timestamp
/// string, over at least two such cells.
///
/// Reads every row, before counting and the top-N cap, so a repeat that
/// only a later value exposes still keeps the column. Distinct is exact
/// string equality; a non-null cell that is not a string, or a string
/// the browser's timestamp parser refuses, keeps the column.
fn is_one_off_time(rows: &[Vec<Value>], idx: usize) -> bool {
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for row in rows {
        match row.get(idx) {
            None | Some(Value::Null) => {}
            Some(Value::String(text)) => {
                if fleet_ui::time::parse_timestamp(text).is_none() || !seen.insert(text) {
                    return false;
                }
            }
            Some(_) => return false,
        }
    }
    seen.len() >= 2
}

fn facet_column(rows: &[Vec<Value>], idx: usize) -> Option<Vec<(String, u32)>> {
    let mut counts: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    for row in rows {
        let Some(cell) = row.get(idx) else { continue };
        let key = match cell {
            Value::String(s) => s.clone(),
            Value::Integer(i) => i.to_string(),
            // A discrete value like any other integer: an id column past
            // `i64::MAX` keeps its facet and its include/exclude control.
            Value::UInt(u) => u.to_string(),
            Value::Boolean(b) => b.to_string(),
            // Null, Float, Array are intentionally skipped (see module docs).
            Value::Null | Value::Float(_) | Value::Array(_) => continue,
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

/// An oversized unsigned is a discrete value, so it keeps its facet.
///
/// Skipped, a column of ids past `i64::MAX` stayed in the results table
/// but vanished from the filter rail, and a column mixing widths showed
/// only half its values — a rail that quietly under-reports is worse
/// than no rail, because the reader reads it as the whole answer.
#[cfg(test)]
#[test]
fn uint_values_are_faceted() {
    use crate::result_actions::Capabilities;
    use trawl_api::value::Column;

    let col = |name: &str| Column {
        name: name.to_owned(),
    };

    // A column mixing the two integer widths facets both of them.
    let mixed = QueryResult {
        columns: vec![col("request_id")],
        rows: vec![
            vec![Value::Integer(5)],
            vec![Value::UInt(u64::MAX)],
            vec![Value::Integer(5)],
        ],
    };
    let facets = compute_facets(&mixed);
    assert_eq!(facets.len(), 1);
    assert_eq!(facets[0].0, "request_id");
    assert_eq!(
        facets[0].1,
        vec![("5".to_owned(), 2), ("18446744073709551615".to_owned(), 1)]
    );

    // An all-unsigned column still earns a facet…
    let all_uint = QueryResult {
        columns: vec![col("request_id")],
        rows: vec![
            vec![Value::UInt(u64::MAX)],
            vec![Value::UInt(9_223_372_036_854_775_808)],
        ],
    };
    let facets = compute_facets(&all_uint);
    assert_eq!(facets.len(), 1, "an all-unsigned column must still facet");
    assert_eq!(facets[0].1.len(), 2);

    // …and its values carry the include/exclude control, as any other
    // discrete cell does.
    let capabilities = Capabilities::for_query("*");
    assert!(capabilities.include("request_id", &Value::UInt(u64::MAX)));
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

    fn s(v: &str) -> Value {
        Value::String(v.to_owned())
    }

    fn fields(facets: &[FieldFacet]) -> Vec<&str> {
        facets.iter().map(|(f, _)| f.as_str()).collect()
    }

    /// An event instant, an ingest instant and the whole original event
    /// are never a dimension, even when their values repeat on screen —
    /// while the other reserved names stay useful facets.
    #[test]
    fn reserved_non_dimensions_are_never_faceted() {
        let row = |sev: i64, repairs: &str| {
            vec![
                s("2026-01-01T00:00:00Z"),
                s("2026-01-01T00:00:01Z"),
                s(r#"{"msg":"hi"}"#),
                Value::Integer(sev),
                s(repairs),
            ]
        };
        let result = QueryResult {
            columns: vec![
                col("_time"),
                col("_ingested"),
                col("_raw"),
                col("_severity"),
                col("_repairs"),
            ],
            rows: vec![row(9, "coerce"), row(9, "coerce"), row(17, "strip")],
        };
        let facets = compute_facets(&result);
        assert_eq!(fields(&facets), vec!["_severity", "_repairs"]);
        assert_eq!(facets[0].1, vec![("9".into(), 2), ("17".into(), 1)]);

        // The set is compared the way the catalog compares names.
        let shouted = QueryResult {
            columns: vec![col("_TIME"), col("_Raw")],
            rows: vec![vec![s("a"), s("b")], vec![s("a"), s("b")]],
        };
        assert!(compute_facets(&shouted).is_empty());
    }

    /// A sender's timestamp that is different on every row counts nothing
    /// useful, whatever it is called. Null cells neither count toward nor
    /// against the rule.
    #[test]
    fn one_off_time_column_is_dropped() {
        let result = QueryResult {
            columns: vec![col("timestamp"), col("level"), col("seen")],
            rows: vec![
                vec![
                    s("2026-01-01T00:00:00.000001Z"),
                    s("info"),
                    s("2026-01-01 00:00:00"),
                ],
                vec![Value::Null, s("info"), s("2026-01-01 00:00:01.5")],
                vec![
                    s("2026-01-01T00:00:00.000002+02:00"),
                    s("warn"),
                    Value::Null,
                ],
                vec![
                    s("2026-01-01T00:00:00.000003Z"),
                    s("info"),
                    s("2026-01-01 00:00:02"),
                ],
            ],
        };
        assert_eq!(fields(&compute_facets(&result)), vec!["level"]);
    }

    /// Ten hosts on ten rows are all different, but they are still hosts.
    #[test]
    fn distinct_non_time_column_is_kept() {
        let result = QueryResult {
            columns: vec![col("host"), col("request_id")],
            rows: (0..10)
                .map(|i| vec![s(&format!("web-{i}")), s(&format!("req-{i:04}"))])
                .collect(),
        };
        let facets = compute_facets(&result);
        assert_eq!(fields(&facets), vec!["host", "request_id"]);
        assert_eq!(facets[0].1.len(), FACET_TOP_N);

        // One cell that is not a time keeps an otherwise one-off column.
        let almost = QueryResult {
            columns: vec![col("when")],
            rows: vec![
                vec![s("2026-01-01T00:00:00Z")],
                vec![s("2026-01-01T00:00:01Z")],
                vec![s("yesterday")],
            ],
        };
        assert_eq!(fields(&compute_facets(&almost)), vec!["when"]);
    }

    /// A time field whose values repeat groups events: two builds.
    #[test]
    fn repeating_time_column_is_kept() {
        let result = QueryResult {
            columns: vec![col("build_time")],
            rows: vec![
                vec![s("2026-01-01T00:00:00Z")],
                vec![s("2026-02-01T00:00:00Z")],
                vec![s("2026-01-01T00:00:00Z")],
            ],
        };
        let facets = compute_facets(&result);
        assert_eq!(
            facets[0].1,
            vec![
                ("2026-01-01T00:00:00Z".into(), 2),
                ("2026-02-01T00:00:00Z".into(), 1)
            ]
        );
    }

    /// One row cannot show that a value is one-off, so it keeps its
    /// facets; only the reserved non-dimensions are dropped.
    #[test]
    fn single_row_page_keeps_its_facets() {
        let result = QueryResult {
            columns: vec![col("_time"), col("timestamp"), col("level")],
            rows: vec![vec![
                s("2026-01-01T00:00:00Z"),
                s("2026-01-01T00:00:00.123Z"),
                s("info"),
            ]],
        };
        assert_eq!(fields(&compute_facets(&result)), vec!["timestamp", "level"]);
    }

    /// A number among the times is not a time, so the column is not a
    /// one-off time column even though every cell differs.
    #[test]
    fn a_non_string_cell_defeats_one_off_classification() {
        for odd in [
            Value::Integer(1_767_225_602),
            Value::Float(1.5),
            Value::Boolean(true),
            Value::Array(vec![s("2026-01-01T00:00:02Z")]),
        ] {
            let result = QueryResult {
                columns: vec![col("timestamp")],
                rows: vec![
                    vec![s("2026-01-01T00:00:00Z")],
                    vec![s("2026-01-01T00:00:01Z")],
                    vec![odd.clone()],
                ],
            };
            assert_eq!(
                fields(&compute_facets(&result)),
                vec!["timestamp"],
                "{odd:?}"
            );
        }
    }

    /// The rule reads every row, not the top ten the rail shows: a repeat
    /// that only the eleventh distinct value exposes still keeps the field.
    #[test]
    fn a_repeat_beyond_the_top_ten_keeps_a_time_column() {
        let mut rows: Vec<Vec<Value>> = (0..11)
            .map(|i| vec![s(&format!("2026-01-01T00:00:{i:02}Z"))])
            .collect();
        rows.push(vec![s("2026-01-01T00:00:10Z")]);
        let result = QueryResult {
            columns: vec![col("timestamp")],
            rows,
        };
        let facets = compute_facets(&result);
        assert_eq!(fields(&facets), vec!["timestamp"]);
        assert_eq!(facets[0].1.len(), FACET_TOP_N);
        assert_eq!(facets[0].1[0], ("2026-01-01T00:00:10Z".into(), 2));
    }

    /// Every settled answer the rail can be shown, and what it makes of
    /// each: the one decision behind the wide rail's automatic open and
    /// close (ADR-0044). `Countable` rows name the groups the rail
    /// renders, in order.
    #[test]
    #[allow(clippy::too_many_lines)] // one table, one row per case
    fn rail_page_classifies_each_answer() {
        enum Want {
            Countable(&'static [&'static str]),
            NothingToCount,
            Aggregate,
        }
        let t = |i: usize| s(&format!("2026-01-01T00:00:{i:02}Z"));
        let cases: Vec<(&str, &str, QueryResult, Want)> = vec![
            (
                "empty result",
                "*",
                QueryResult::empty(),
                Want::NothingToCount,
            ),
            (
                "eligible groups",
                "*",
                QueryResult {
                    columns: vec![col("_time"), col("host"), col("level")],
                    rows: vec![
                        vec![t(0), s("web-1"), s("info")],
                        vec![t(1), s("web-2"), s("info")],
                    ],
                },
                Want::Countable(&["host", "level"]),
            ),
            (
                "reserved-only columns",
                "*",
                QueryResult {
                    columns: vec![col("_time"), col("_ingested"), col("_raw")],
                    rows: vec![vec![t(0), t(1), s("a")], vec![t(0), t(1), s("a")]],
                },
                Want::NothingToCount,
            ),
            (
                "one-off-time column",
                "*",
                QueryResult {
                    columns: vec![col("_time"), col("timestamp")],
                    rows: vec![vec![t(0), t(1)], vec![t(2), t(3)]],
                },
                Want::NothingToCount,
            ),
            (
                "field excluded by input_field",
                "* | let host = lower(host)",
                QueryResult {
                    columns: vec![col("host")],
                    rows: vec![vec![s("web-1")], vec![s("web-1")]],
                },
                Want::NothingToCount,
            ),
            (
                "input_field keeps the unchanged fields",
                "* | let host = lower(host)",
                QueryResult {
                    columns: vec![col("host"), col("level")],
                    rows: vec![vec![s("web-1"), s("info")], vec![s("web-1"), s("warn")]],
                },
                Want::Countable(&["level"]),
            ),
            (
                "aggregation",
                "* | stats count() by service",
                QueryResult {
                    columns: vec![col("service"), col("count")],
                    rows: vec![
                        vec![s("nginx"), Value::Integer(2)],
                        vec![s("api"), Value::Integer(2)],
                    ],
                },
                Want::Aggregate,
            ),
            (
                "aggregation with no rows",
                "* | stats count() by service",
                QueryResult::empty(),
                Want::Aggregate,
            ),
            (
                "top is an aggregation",
                "* | top 10 host",
                QueryResult {
                    columns: vec![col("host"), col("count")],
                    rows: vec![vec![s("web-1"), Value::Integer(3)]],
                },
                Want::Aggregate,
            ),
            (
                "unknown stage",
                "* | extract kv",
                QueryResult {
                    columns: vec![col("host")],
                    rows: vec![vec![s("web-1")], vec![s("web-2")]],
                },
                Want::NothingToCount,
            ),
        ];
        for (name, query, result, want) in cases {
            assert!(trawl_core::parser::parse(query).is_ok(), "{name}: {query}");
            let page = rail_page(query, &result);
            match want {
                Want::Countable(names) => {
                    let RailPage::Countable(groups) = &page else {
                        panic!("{name}: {page:?}");
                    };
                    assert_eq!(fields(groups), names, "{name}");
                    assert!(page.is_countable(), "{name}");
                }
                Want::NothingToCount => {
                    assert_eq!(page, RailPage::NothingToCount, "{name}");
                    assert!(!page.is_countable(), "{name}");
                }
                Want::Aggregate => {
                    assert_eq!(page, RailPage::Aggregate, "{name}");
                    assert!(!page.is_countable(), "{name}");
                }
            }
        }
    }

    /// The two hints are the ADR's copy, verbatim; a countable page
    /// shows its groups instead.
    #[test]
    fn rail_page_hint_copy() {
        assert_eq!(RailPage::Countable(Vec::new()).hint(), None);
        assert_eq!(
            RailPage::NothingToCount.hint(),
            Some("No field values to count.")
        );
        assert_eq!(
            RailPage::Aggregate.hint(),
            Some("Field values are not counted for an aggregate result.")
        );
    }

    /// The live ring and the snapshot page decide the rail with one
    /// function: identical events give identical facets.
    #[test]
    fn live_ring_and_snapshot_facet_identically() {
        use crate::state::stream_session_value::{RingBuffer, ring_to_result};

        let events = [
            (
                "2026-01-01T00:00:00.000001Z",
                "2026-01-01T00:00:00Z",
                "info",
                "web-1",
                200,
            ),
            (
                "2026-01-01T00:00:00.000002Z",
                "2026-01-01T00:00:01Z",
                "warn",
                "web-2",
                500,
            ),
            (
                "2026-01-01T00:00:00.000003Z",
                "2026-01-01T00:00:02Z",
                "info",
                "web-1",
                200,
            ),
        ];
        let mut ring = RingBuffer::default();
        for (time, sender, level, host, status) in events {
            let serde_json::Value::Object(map) = serde_json::json!({
                "_time": time,
                "timestamp": sender,
                "level": level,
                "host": host,
                "status": status,
                "_raw": format!("{level} {host}"),
            }) else {
                unreachable!()
            };
            ring.push(map);
        }
        let live = ring_to_result(&ring);

        let snapshot = QueryResult {
            columns: live.columns.clone(),
            rows: events
                .iter()
                .map(|(time, sender, level, host, status)| {
                    live.columns
                        .iter()
                        .map(|c| match c.name.as_str() {
                            "_time" => s(time),
                            "timestamp" => s(sender),
                            "level" => s(level),
                            "host" => s(host),
                            "status" => Value::Integer(*status),
                            "_raw" => s(&format!("{level} {host}")),
                            other => panic!("unexpected column {other}"),
                        })
                        .collect()
                })
                .collect(),
        };

        let live_facets = compute_facets(&live);
        assert_eq!(live_facets, compute_facets(&snapshot));
        let mut names = fields(&live_facets);
        names.sort_unstable();
        assert_eq!(names, vec!["host", "level", "status"]);
    }
}
