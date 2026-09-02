// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pure-logic display helpers for rendering [`QueryResult`] data.
//!
//! Shared by the terminal UI (`trawl-cli`) and the browser SPA
//! (`trawl-web-ui`). No I/O, no ratatui, no wasm-bindgen — safe to
//! compile on both native and `wasm32-unknown-unknown`.

use std::collections::HashMap;

use crate::value::{QueryResult, Value};

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
/// effort); nulls, booleans, and arrays return 0.
#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
fn value_to_u64(value: &Value) -> u64 {
    match value {
        Value::Integer(i) => (*i).max(0) as u64,
        Value::Float(f) => f.max(0.0) as u64,
        Value::String(s) => s.parse::<f64>().unwrap_or(0.0).max(0.0) as u64,
        _ => 0,
    }
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
            .filter(|&i| {
                result
                    .rows
                    .iter()
                    .any(|row| matches!(row.get(i), Some(Value::String(_))))
            })
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
                series.sort_by(|a, b| {
                    let sum_b: u64 = b.1.iter().sum();
                    let sum_a: u64 = a.1.iter().sum();
                    sum_b.cmp(&sum_a)
                });
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
                series.sort_by(|a, b| {
                    let sum_b: u64 = b.1.iter().sum();
                    let sum_a: u64 = a.1.iter().sum();
                    sum_b.cmp(&sum_a)
                });
                series.truncate(MAX_SERIES);
            }
            series.sort_by(|a, b| a.0.cmp(&b.0));
            (series, total)
        } else {
            (vec![], 0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Column;

    fn col(name: &str) -> Column {
        Column {
            name: name.to_owned(),
        }
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
