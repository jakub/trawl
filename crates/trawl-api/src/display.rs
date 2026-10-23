// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pure-logic display helpers for rendering [`QueryResult`] data.
//!
//! Shared by the terminal UI (`trawl-cli`) and the browser SPA
//! (`trawl-web-ui`). No I/O, no ratatui, no wasm-bindgen — safe to
//! compile on both native and `wasm32-unknown-unknown`.

use std::collections::HashMap;

use crate::value::{ColumnType, QueryResult, Value};

/// A cell of a `timestamp` column, or of a list of them, as the CLI table
/// and the TUI show it: each timestamp through [`timestamp_text`], a list
/// the way `Value`'s `Display` writes one.
///
/// `None` for any other cell, which each surface renders its own way.
#[must_use]
pub fn timestamp_cell_text(ty: &ColumnType, value: &Value) -> Option<String> {
    match (ty, value) {
        (ColumnType::Timestamp, Value::String(text)) => Some(timestamp_text(text)),
        (ColumnType::List { items }, Value::Array(_)) if holds_timestamps(items) => {
            Some(element_text(ty, value))
        }
        _ => None,
    }
}

fn holds_timestamps(ty: &ColumnType) -> bool {
    match ty {
        ColumnType::Timestamp => true,
        ColumnType::List { items } => holds_timestamps(items),
        _ => false,
    }
}

/// A list element the way `Value`'s `Display` writes one, with the
/// timestamps in it shown through [`timestamp_text`].
fn element_text(ty: &ColumnType, value: &Value) -> String {
    match (ty, value) {
        (ColumnType::Timestamp, Value::String(text)) => timestamp_text(text),
        (ColumnType::List { items }, Value::Array(elements)) => {
            let shown: Vec<String> = elements.iter().map(|e| element_text(items, e)).collect();
            format!("[{}]", shown.join(", "))
        }
        _ => value.to_string(),
    }
}

/// A wire timestamp as the terminal shows it: the wall-clock reading
/// without the `T`, the offset or the fraction's trailing zeros, so
/// `2024-01-15T10:00:00.120000+02:00` shows as `2024-01-15 10:00:00.12`.
///
/// The server renders a result in the reader's offset, so the wall clock
/// is the reading the reader asked for. Text that is not a wire timestamp
/// (`infinity`, or anything else) shows as it stands.
#[must_use]
pub fn timestamp_text(text: &str) -> String {
    if !text.is_ascii() {
        return text.to_owned();
    }
    let Some((date, rest)) = text.split_once('T') else {
        return text.to_owned();
    };
    let clock = if let Some(clock) = rest.strip_suffix('Z') {
        clock
    } else {
        match rest.len().checked_sub(6) {
            Some(at) if matches!(rest.as_bytes()[at], b'+' | b'-') => &rest[..at],
            _ => return text.to_owned(),
        }
    };
    match clock.split_once('.') {
        Some((whole, fraction)) => {
            let fraction = fraction.trim_end_matches('0');
            if fraction.is_empty() {
                format!("{date} {whole}")
            } else {
                format!("{date} {whole}.{fraction}")
            }
        }
        None => format!("{date} {clock}"),
    }
}

/// Convert a [`Value`] to a string for display.
///
/// Floats are rendered to 2 decimal places for readability; callers that
/// need full precision (CSV export, etc.) should format the numeric
/// variants themselves.
#[must_use]
pub fn value_to_string(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        Value::Boolean(b) => b.to_string(),
        Value::Integer(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) => format!("{f:.2}"),
        Value::String(s) => s.clone(),
        Value::Array(_) => value.to_string(),
    }
}

/// Detect if a query result came from a timechart/`time_bucket` query.
///
/// Timechart queries always produce a `_time` column. We check any
/// position because `UNION ALL BY NAME` can reorder columns.
#[must_use]
pub fn is_timechart_result(result: &QueryResult) -> bool {
    result.columns.iter().any(|col| col.name == "_time")
}

/// Downsample a series to fit within `target_width` buckets.
///
/// When there are more data points than buckets, groups points into
/// buckets and takes the max of each bucket (preserving peaks).
/// When data already fits, returns it as-is.
#[must_use]
#[allow(clippy::cast_precision_loss)] // typical target widths won't overflow f64
pub fn downsample(values: &[u64], target_width: usize) -> Vec<u64> {
    if target_width == 0 || values.is_empty() {
        return vec![];
    }
    if values.len() <= target_width {
        return values.to_vec();
    }

    let mut result = Vec::with_capacity(target_width);
    let bucket_size_f = values.len() as f64 / target_width as f64;

    for i in 0..target_width {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let start = (i as f64 * bucket_size_f) as usize;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let end = ((i + 1) as f64 * bucket_size_f) as usize;
        let end = end.min(values.len());

        let max_in_bucket = values[start..end].iter().max().copied().unwrap_or(0);
        result.push(max_in_bucket);
    }

    result
}

/// Convert a [`Value`] to `u64` for chart rendering.
///
/// Negative numbers clamp to 0; strings are parsed as floats (best
/// effort); nulls, booleans, and arrays return 0. A [`Value::UInt`] is
/// already this function's own width, so it passes through exactly.
#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
fn value_to_u64(value: &Value) -> u64 {
    match value {
        Value::Integer(i) => (*i).max(0) as u64,
        Value::UInt(u) => *u,
        Value::Float(f) => f.max(0.0) as u64,
        Value::String(s) => s.parse::<f64>().unwrap_or(0.0).max(0.0) as u64,
        _ => 0,
    }
}

/// Whether a cell reads as a group label rather than a measurement.
///
/// Text is a label. A number is a measurement whatever its width, which
/// is why an oversized unsigned carries the [`Value::UInt`] variant
/// rather than its digits as a string: classified by variant, a 20-digit
/// `VARCHAR` label and a 20-digit measurement never trade places.
fn is_group_label(cell: Option<&Value>) -> bool {
    matches!(cell, Some(Value::String(_)))
}

/// A series' total, for ranking the top six only.
///
/// Wider than the points it adds: a metric column can hold values near
/// `u64::MAX` (an id, a byte count), and two of them overflow a `u64`
/// accumulator — a panic under overflow checks, and a wrap to zero
/// without them, which ranks the largest series last and drops it. The
/// total is never displayed, so its width costs nothing.
fn series_total(values: &[u64]) -> u128 {
    values.iter().copied().map(u128::from).sum()
}

/// Extract `(label, values)` tuples from a timechart [`QueryResult`].
///
/// Three shapes are recognized:
/// - **Single series**: `[_time, metric]` → `[("metric", [v1, v2, ...])]`
/// - **Group-by**: `[_time, group_label, metric]` → one series per
///   distinct group label
/// - **Multi-agg**: `[_time, metric_a, metric_b, ...]` (all numeric) →
///   one series per metric column
///
/// "Numeric" is decided by variant, so a [`Value::UInt`] measurement
/// charts like any other number and a digit string of any width stays a
/// label.
///
/// Series are capped to the top 6 by total value, then sorted
/// alphabetically by label for stable rendering. Returns
/// `(series, total_count)` where `total_count` is the number of
/// distinct series before any truncation.
#[must_use]
pub fn extract_series(result: &QueryResult) -> (Vec<(String, Vec<u64>)>, usize) {
    const MAX_SERIES: usize = 6;

    let Some(time_col) = result.columns.iter().position(|c| c.name == "_time") else {
        return (vec![], 0);
    };

    let other_cols: Vec<usize> = (0..result.columns.len())
        .filter(|&i| i != time_col)
        .collect();

    if other_cols.len() == 1 {
        let metric_idx = other_cols[0];
        let metric_name = &result.columns[metric_idx].name;
        let values: Vec<u64> = result
            .rows
            .iter()
            .map(|row| value_to_u64(&row[metric_idx]))
            .collect();
        (vec![(metric_name.clone(), values)], 1)
    } else {
        if result.rows.is_empty() {
            return (vec![], 0);
        }

        let string_cols: Vec<usize> = other_cols
            .iter()
            .copied()
            .filter(|&i| result.rows.iter().any(|row| is_group_label(row.get(i))))
            .collect();
        let numeric_cols: Vec<usize> = other_cols
            .iter()
            .copied()
            .filter(|&i| !string_cols.contains(&i))
            .collect();

        if string_cols.len() == 1 && numeric_cols.len() == 1 {
            let group_idx = string_cols[0];
            let metric_idx = numeric_cols[0];

            let mut series_map: HashMap<String, Vec<u64>> = HashMap::new();
            for row in &result.rows {
                let label = value_to_string(&row[group_idx]);
                let value = value_to_u64(&row[metric_idx]);
                series_map.entry(label).or_default().push(value);
            }

            let mut series: Vec<(String, Vec<u64>)> = series_map.into_iter().collect();
            let total = series.len();
            if series.len() > MAX_SERIES {
                series.sort_by_key(|s| std::cmp::Reverse(series_total(&s.1)));
                series.truncate(MAX_SERIES);
            }
            series.sort_by(|a, b| a.0.cmp(&b.0));
            (series, total)
        } else if string_cols.is_empty() {
            let mut series: Vec<(String, Vec<u64>)> = numeric_cols
                .iter()
                .map(|&col_idx| {
                    let label = result.columns[col_idx].name.clone();
                    let values: Vec<u64> = result
                        .rows
                        .iter()
                        .map(|row| value_to_u64(&row[col_idx]))
                        .collect();
                    (label, values)
                })
                .collect();
            let total = series.len();
            if series.len() > MAX_SERIES {
                series.sort_by_key(|s| std::cmp::Reverse(series_total(&s.1)));
                series.truncate(MAX_SERIES);
            }
            series.sort_by(|a, b| a.0.cmp(&b.0));
            (series, total)
        } else {
            (vec![], 0)
        }
    }
}

/// Ranking the top six survives a series whose points sum past
/// `u64::MAX`.
///
/// Seven metric columns force the cap, and two `UInt` points at
/// `2^63` sum to `2^64` — one past what a `u64` accumulator holds. Under
/// overflow checks that panicked; without them it wrapped to zero, so
/// the largest series ranked last and was the one dropped. The ranking
/// total is not a measurement anyone reads, so it is free to be wider
/// than the values it adds up.
#[cfg(test)]
#[test]
fn series_ranking_survives_unsigned_totals() {
    let mut columns = vec![crate::value::Column::new(
        "_time",
        crate::value::ColumnType::Timestamp,
    )];
    columns
        .extend((0..7).map(|i| {
            crate::value::Column::new(format!("m{i}"), crate::value::ColumnType::Integer)
        }));

    // Six small metrics and one enormous one, twice over.
    let row = |t: &str| {
        let mut cells = vec![Value::String(t.to_owned())];
        cells.extend((0..6).map(|i| Value::Integer(i64::from(i) + 1)));
        cells.push(Value::UInt(9_223_372_036_854_775_808));
        cells
    };
    let result = QueryResult {
        columns,
        rows: vec![row("2026-01-01 00:00:00"), row("2026-01-01 00:05:00")],
    };

    let (series, total) = extract_series(&result);
    assert_eq!(total, 7, "every metric column counts before the cap");
    assert_eq!(series.len(), 6, "the cap keeps six");
    let labels: Vec<&str> = series.iter().map(|s| s.0.as_str()).collect();
    assert!(
        labels.contains(&"m6"),
        "the largest series must survive the cap, not be dropped by it: {labels:?}"
    );
    assert_eq!(
        series.iter().find(|s| s.0 == "m6").expect("m6 survives").1,
        vec![9_223_372_036_854_775_808, 9_223_372_036_854_775_808]
    );
}

/// An oversized unsigned measurement is still a measurement.
///
/// `| timechart first(request_id), count()` over a `request_id` past
/// `i64::MAX` puts a [`Value::UInt`] in a measurement column. Classified
/// as a label it would collapse the whole chart into one series named
/// after the number and drop the second metric — a plausible chart of the
/// wrong thing. The variant is what keeps that from happening.
#[cfg(test)]
#[test]
fn uint_metric_is_not_a_group() {
    let result = QueryResult {
        columns: vec![
            crate::value::Column::new("_time", crate::value::ColumnType::Timestamp),
            crate::value::Column::new("first_request_id", crate::value::ColumnType::Integer),
            crate::value::Column::new("count", crate::value::ColumnType::Integer),
        ],
        rows: vec![vec![
            Value::String("2026-01-01 00:00:00".to_owned()),
            Value::UInt(u64::MAX),
            Value::Integer(1),
        ]],
    };

    let (series, total) = extract_series(&result);
    assert_eq!(total, 2, "both measurement columns must chart");
    let labels: Vec<&str> = series.iter().map(|s| s.0.as_str()).collect();
    assert_eq!(labels, vec!["count", "first_request_id"]);
    assert_eq!(series[0].1, vec![1]);
    assert_eq!(series[1].1, vec![u64::MAX]);
}

/// A digit string is a genuine label whatever its width.
///
/// Text is text: a `VARCHAR` status code and a 20-digit `VARCHAR`
/// identifier both group the chart. Reading either as a measurement — the
/// heuristic a string landing forces — would silently drop the grouping.
#[cfg(test)]
#[test]
fn digit_string_label_stays_a_group() {
    let grouped_by = |label: &str| {
        let result = QueryResult {
            columns: vec![
                crate::value::Column::new("_time", crate::value::ColumnType::Timestamp),
                crate::value::Column::new("status", crate::value::ColumnType::String),
                crate::value::Column::new("count", crate::value::ColumnType::Integer),
            ],
            rows: vec![
                vec![
                    Value::String("2026-01-01 00:00:00".to_owned()),
                    Value::String(label.to_owned()),
                    Value::Integer(7),
                ],
                vec![
                    Value::String("2026-01-01 00:05:00".to_owned()),
                    Value::String(label.to_owned()),
                    Value::Integer(9),
                ],
            ],
        };
        extract_series(&result)
    };

    let (series, total) = grouped_by("404");
    assert_eq!(total, 1, "the grouped branch must still select");
    assert_eq!(series[0].0, "404");
    assert_eq!(series[0].1, vec![7, 9]);

    // Twenty digits, past `i64::MAX`, and still a label — the case the
    // variant settles and a digit-shape heuristic could not.
    let (series, total) = grouped_by("10000000000000000000");
    assert_eq!(total, 1, "a wide VARCHAR label is still a label");
    assert_eq!(series[0].0, "10000000000000000000");
    assert_eq!(series[0].1, vec![7, 9]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Column;

    fn col(name: &str) -> Column {
        Column::new(name, crate::value::ColumnType::String)
    }

    #[test]
    fn extract_series_multi_series() {
        let result = QueryResult {
            columns: vec![col("_time"), col("service"), col("count")],
            rows: vec![
                vec![
                    Value::String("2024-01-01 00:00:00".into()),
                    Value::String("nginx".into()),
                    Value::Integer(42),
                ],
                vec![
                    Value::String("2024-01-01 00:00:00".into()),
                    Value::String("api".into()),
                    Value::Integer(17),
                ],
                vec![
                    Value::String("2024-01-01 00:05:00".into()),
                    Value::String("nginx".into()),
                    Value::Integer(38),
                ],
                vec![
                    Value::String("2024-01-01 00:05:00".into()),
                    Value::String("api".into()),
                    Value::Integer(22),
                ],
            ],
        };

        let (series, total) = extract_series(&result);
        assert_eq!(total, 2);
        assert_eq!(series.len(), 2);
        assert_eq!(series[0].0, "api");
        assert_eq!(series[0].1, vec![17, 22]);
        assert_eq!(series[1].0, "nginx");
        assert_eq!(series[1].1, vec![42, 38]);
    }

    #[test]
    fn extract_series_single_series() {
        let result = QueryResult {
            columns: vec![col("_time"), col("count")],
            rows: vec![
                vec![
                    Value::String("2024-01-01 00:00:00".into()),
                    Value::Integer(10),
                ],
                vec![
                    Value::String("2024-01-01 00:05:00".into()),
                    Value::Integer(20),
                ],
            ],
        };

        let (series, total) = extract_series(&result);
        assert_eq!(total, 1);
        assert_eq!(series.len(), 1);
        assert_eq!(series[0].0, "count");
        assert_eq!(series[0].1, vec![10, 20]);
    }

    #[test]
    fn extract_series_caps_to_top_6() {
        let mut rows = Vec::new();
        for i in 0..10 {
            rows.push(vec![
                Value::String("2024-01-01 00:00:00".into()),
                Value::String(format!("svc-{i}")),
                Value::Integer((i + 1) * 100),
            ]);
        }

        let result = QueryResult {
            columns: vec![col("_time"), col("service"), col("count")],
            rows,
        };

        let (series, total) = extract_series(&result);
        assert_eq!(total, 10);
        assert_eq!(series.len(), 6);
        let labels: Vec<&str> = series.iter().map(|s| s.0.as_str()).collect();
        assert_eq!(
            labels,
            vec!["svc-4", "svc-5", "svc-6", "svc-7", "svc-8", "svc-9"]
        );
    }

    #[test]
    fn extract_series_multi_agg() {
        let result = QueryResult {
            columns: vec![col("_time"), col("avg_rssi"), col("avg_noise")],
            rows: vec![
                vec![
                    Value::String("2024-01-01 00:00:00".into()),
                    Value::Integer(65),
                    Value::Integer(90),
                ],
                vec![
                    Value::String("2024-01-01 00:05:00".into()),
                    Value::Integer(70),
                    Value::Integer(85),
                ],
                vec![
                    Value::String("2024-01-01 00:10:00".into()),
                    Value::Integer(60),
                    Value::Integer(92),
                ],
            ],
        };

        let (series, total) = extract_series(&result);
        assert_eq!(total, 2);
        assert_eq!(series.len(), 2);
        assert_eq!(series[0].0, "avg_noise");
        assert_eq!(series[0].1, vec![90, 85, 92]);
        assert_eq!(series[1].0, "avg_rssi");
        assert_eq!(series[1].1, vec![65, 70, 60]);
    }

    #[test]
    fn extract_series_multi_agg_three_cols() {
        let result = QueryResult {
            columns: vec![
                col("_time"),
                col("avg_rssi"),
                col("avg_noise"),
                col("avg_snr"),
            ],
            rows: vec![
                vec![
                    Value::String("2024-01-01 00:00:00".into()),
                    Value::Integer(65),
                    Value::Integer(90),
                    Value::Integer(25),
                ],
                vec![
                    Value::String("2024-01-01 00:05:00".into()),
                    Value::Integer(70),
                    Value::Integer(85),
                    Value::Integer(30),
                ],
            ],
        };

        let (series, total) = extract_series(&result);
        assert_eq!(total, 3);
        assert_eq!(series.len(), 3);
        assert_eq!(series[0].0, "avg_noise");
        assert_eq!(series[1].0, "avg_rssi");
        assert_eq!(series[2].0, "avg_snr");
    }

    #[test]
    fn extract_series_multi_agg_capped() {
        let cols: Vec<Column> = std::iter::once(col("_time"))
            .chain((0..8).map(|i| col(&format!("metric_{i}"))))
            .collect();
        let row: Vec<Value> = std::iter::once(Value::String("2024-01-01 00:00:00".into()))
            .chain((0..8).map(|i| Value::Integer((i + 1) * 10)))
            .collect();

        let result = QueryResult {
            columns: cols,
            rows: vec![row],
        };

        let (series, total) = extract_series(&result);
        assert_eq!(total, 8);
        assert_eq!(series.len(), 6);
        let labels: Vec<&str> = series.iter().map(|s| s.0.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "metric_2", "metric_3", "metric_4", "metric_5", "metric_6", "metric_7"
            ]
        );
    }

    #[test]
    fn extract_series_group_by_still_works() {
        let result = QueryResult {
            columns: vec![col("_time"), col("service"), col("count")],
            rows: vec![
                vec![
                    Value::String("2024-01-01 00:00:00".into()),
                    Value::String("nginx".into()),
                    Value::Integer(42),
                ],
                vec![
                    Value::String("2024-01-01 00:00:00".into()),
                    Value::String("api".into()),
                    Value::Integer(17),
                ],
            ],
        };

        let (series, total) = extract_series(&result);
        assert_eq!(total, 2);
        assert_eq!(series[0].0, "api");
        assert_eq!(series[0].1, vec![17]);
        assert_eq!(series[1].0, "nginx");
        assert_eq!(series[1].1, vec![42]);
    }

    #[test]
    fn is_timechart_result_detects_time_column() {
        let with_time = QueryResult {
            columns: vec![col("service"), col("_time"), col("count")],
            rows: vec![],
        };
        assert!(is_timechart_result(&with_time));

        let without_time = QueryResult {
            columns: vec![col("service"), col("count")],
            rows: vec![],
        };
        assert!(!is_timechart_result(&without_time));
    }

    #[test]
    fn value_to_string_renders_all_variants() {
        assert_eq!(value_to_string(&Value::Null), "NULL");
        assert_eq!(value_to_string(&Value::Boolean(true)), "true");
        assert_eq!(value_to_string(&Value::Integer(42)), "42");
        assert_eq!(value_to_string(&Value::Float(1.23)), "1.23");
        assert_eq!(value_to_string(&Value::Float(1.0)), "1.00");
        assert_eq!(value_to_string(&Value::String("hi".into())), "hi");
        assert_eq!(
            value_to_string(&Value::Array(vec![Value::Integer(1), Value::Integer(2)])),
            "[1, 2]"
        );
    }

    /// The terminal shows a wire timestamp as the wall clock it carries,
    /// the look it had before the wire spelled an offset: no `T`, no
    /// offset, no trailing fraction zeros.
    #[test]
    fn timestamps_show_as_the_wall_clock() {
        for (wire, shown) in [
            ("2024-01-15T10:00:00.000000Z", "2024-01-15 10:00:00"),
            ("2024-01-15T10:00:00.120000+02:00", "2024-01-15 10:00:00.12"),
            (
                "1969-12-31T23:59:59.999999-08:00",
                "1969-12-31 23:59:59.999999",
            ),
            ("+10000-01-01T00:00:00.000000Z", "+10000-01-01 00:00:00"),
            ("infinity", "infinity"),
            ("-infinity", "-infinity"),
            ("not a timestamp", "not a timestamp"),
            ("2024-01-15T10:00:00", "2024-01-15T10:00:00"),
        ] {
            assert_eq!(timestamp_text(wire), shown, "{wire}");
        }
    }

    /// Only a `timestamp` cell, alone or in a list, has a timestamp look;
    /// every other cell is left to the surface.
    #[test]
    fn timestamp_cells_are_picked_by_column_type() {
        let wire = Value::String("2024-01-15T10:00:00.000000Z".into());
        assert_eq!(
            timestamp_cell_text(&ColumnType::Timestamp, &wire).as_deref(),
            Some("2024-01-15 10:00:00")
        );
        assert_eq!(timestamp_cell_text(&ColumnType::String, &wire), None);
        assert_eq!(
            timestamp_cell_text(&ColumnType::Timestamp, &Value::Null),
            None
        );
        let list = Value::Array(vec![wire.clone(), Value::Null]);
        assert_eq!(
            timestamp_cell_text(&ColumnType::list(ColumnType::Timestamp), &list).as_deref(),
            Some("[2024-01-15 10:00:00, ]")
        );
        let nested = Value::Array(vec![list.clone()]);
        assert_eq!(
            timestamp_cell_text(
                &ColumnType::list(ColumnType::list(ColumnType::Timestamp)),
                &nested
            )
            .as_deref(),
            Some("[[2024-01-15 10:00:00, ]]")
        );
        assert_eq!(
            timestamp_cell_text(&ColumnType::list(ColumnType::String), &list),
            None
        );
        assert_eq!(
            timestamp_cell_text(&ColumnType::Float, &Value::Float(1.0)),
            None
        );
    }

    #[test]
    fn downsample_noop_when_within_target() {
        let data = vec![1, 2, 3, 4, 5];
        assert_eq!(downsample(&data, 10), data);
        assert_eq!(downsample(&data, 5), data);
    }

    #[test]
    fn downsample_empty_or_zero_target() {
        assert!(downsample(&[], 10).is_empty());
        assert!(downsample(&[1, 2, 3], 0).is_empty());
    }

    #[test]
    fn downsample_preserves_peaks() {
        let data = vec![1, 10, 2, 3, 20, 4, 5, 30, 6, 7];
        let result = downsample(&data, 5);
        assert_eq!(result.len(), 5);
        assert!(result.contains(&30));
    }
}
