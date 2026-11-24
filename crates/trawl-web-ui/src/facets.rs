// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `Float` is skipped (usually continuous — facets would be noise),
//! `Array` can't be keyed, and `Null`-only columns are dropped.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use trawl_api::value::{QueryResult, Value};

pub const FACET_TOP_N: usize = 10;

/// One field's facet: `(column_name, sorted (value, count) list,
/// distinct values)`. The list holds at most [`FACET_TOP_N`] values; the
/// distinct count is taken before that cap, so the rail can say how many
/// values the page holds, not how many it lists.
pub type FieldFacet = (String, Vec<(String, u32)>, usize);

#[must_use]
pub fn compute_facets(result: &QueryResult) -> Vec<FieldFacet> {
    let mut out = Vec::with_capacity(result.columns.len());
    for (i, col) in result.columns.iter().enumerate() {
        if trawl_core::schema::is_non_dimension(&col.name) || is_one_off_time(&result.rows, i) {
            continue;
        }
        if let Some((top, distinct)) = facet_column(&result.rows, i) {
            out.push((col.name.clone(), top, distinct));
        }
    }
    out
}

#[must_use]
pub fn is_aggregation_shape(query: &str) -> bool {
    if query.trim().is_empty() {
        return false;
    }
    trawl_core::parser::parse(query).is_ok_and(|ast| ast.has_aggregation())
}

/// What the filter rail has to show for one settled answer (ADR-0044).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RailPage {
    Countable(Vec<FieldFacet>),
    /// The page's caller also settles a failed query and a malformed link here.
    NothingToCount,
    Aggregate,
}

impl RailPage {
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

/// `query` is the query that PRODUCED `result` — the response's own
/// effective query for a snapshot, the stream's effective query in
/// live — never the editor's draft.
///
/// The aggregation shape is read first, because `top`, `rare` and
/// `pivot` reach [`Capabilities`] as sources it cannot vouch for, and
/// the reader should hear that the result is an aggregate, not that it
/// has nothing to count.
///
/// [`Capabilities`]: crate::result_actions::Capabilities
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
        .filter(|(field, _, _)| provenance.input_field(field))
        .collect();
    if groups.is_empty() {
        RailPage::NothingToCount
    } else {
        RailPage::Countable(groups)
    }
}

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

fn facet_column(rows: &[Vec<Value>], idx: usize) -> Option<(Vec<(String, u32)>, usize)> {
    let mut counts: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    for row in rows {
        let Some(cell) = row.get(idx) else { continue };
        let key = match cell {
            Value::String(s) => s.clone(),
            Value::Integer(i) => i.to_string(),
            Value::UInt(u) => u.to_string(),
            Value::Boolean(b) => b.to_string(),
            Value::Null | Value::Float(_) | Value::Array(_) => continue,
        };
        *counts.entry(key).or_insert(0) += 1;
    }

    if counts.is_empty() {
        return None;
    }

    let distinct = counts.len();
    let mut sorted: Vec<(String, u32)> = counts.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    sorted.truncate(FACET_TOP_N);
    Some((sorted, distinct))
}

#[cfg(test)]
#[test]
fn uint_values_are_faceted() {
    use crate::result_actions::Capabilities;
    use trawl_api::value::Column;

    let col = |name: &str| Column::new(name, trawl_api::value::ColumnType::Integer);

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

    let capabilities = Capabilities::for_query("*");
    assert!(capabilities.include("request_id", &Value::UInt(u64::MAX)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use trawl_api::value::Column;

    /// Facets classify by cell variant, never by column type, so the type
    /// here is not under test.
    fn col(name: &str) -> Column {
        Column::new(name, trawl_api::value::ColumnType::String)
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
        assert_eq!(facets[0].1[0].0, "v0");
        assert_eq!(facets[0].1[0].1, 15);
        assert_eq!(facets[0].1[9].0, "v9");
        assert_eq!(facets[0].2, 15);
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
        assert!(!is_aggregation_shape(""));
        assert!(!is_aggregation_shape("   "));
        assert!(!is_aggregation_shape("| | |"));
    }

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
            facets.iter().any(
                |(field, values, _)| field == "count" && values.contains(&("2".to_string(), 2))
            ),
            "{facets:?}"
        );
    }

    fn s(v: &str) -> Value {
        Value::String(v.to_owned())
    }

    fn fields(facets: &[FieldFacet]) -> Vec<&str> {
        facets.iter().map(|(f, _, _)| f.as_str()).collect()
    }

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

        let shouted = QueryResult {
            columns: vec![col("_TIME"), col("_Raw")],
            rows: vec![vec![s("a"), s("b")], vec![s("a"), s("b")]],
        };
        assert!(compute_facets(&shouted).is_empty());
    }

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

    #[test]
    #[allow(clippy::too_many_lines)]
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
            let frame = serde_json::from_value(serde_json::json!({
                "columns": [
                    {"name": "_raw", "type": "string"},
                    {"name": "_time", "type": "timestamp"},
                    {"name": "host", "type": "string"},
                    {"name": "level", "type": "string"},
                    {"name": "status", "type": "integer"},
                    {"name": "timestamp", "type": "string"},
                ],
                "rows": [[
                    format!("{level} {host}"),
                    time,
                    host,
                    level,
                    status.to_string(),
                    sender,
                ]],
            }))
            .expect("a valid data frame");
            ring.push(frame);
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
