// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Histogram/>` — 78px strip rendering a stacked ok/err bar chart
//! over the current page's results.
//!
//! Time bucketing leans on `crate::histogram::bucketize`. Timestamps come
//! from a `_time` / `time` / `timestamp` / `@timestamp` column; with none
//! of them there is nothing to bucket, and the strip renders the "No time
//! series for this query" hint instead. "is error" is `_severity >= 17`
//! (the `OTel` error band and above, ADR-0013 §9); a page carrying no
//! `_severity` column draws every bar as ok.
//!
//! Y/X axis labels are rendered alongside the bars (`0` / `max/2` /
//! `max` on the left; time anchors on the bottom derived from the
//! current `RangeSpec`). Per-bar tooltips appear on hover.

use fleet_ui::{LoadState, Loaded};
use leptos::prelude::*;
use trawl_api::QueryResponse;
use trawl_api::value::Value;

use crate::api::ApiError;
use crate::histogram::{Bucket, bucketize};
use crate::state::query::RangeSpec;

const N_BUCKETS: usize = 48;

#[component]
pub fn Histogram(
    rows: LocalResource<Result<QueryResponse, ApiError>>,
    /// Current time window — drives the x-axis anchor labels.
    #[prop(into)]
    range: Signal<RangeSpec>,
) -> impl IntoView {
    view! {
        <div class="histo">
            <Loaded
                state=Signal::derive(move || LoadState::from_resource(rows.get()))
                // Deliberate quiet-error override: the results table
                // already reports the query failure.
                error=Box::new(|_| view! { <div class="histo-hint">"—"</div> }.into_any())
                render=Box::new(move |resp: QueryResponse| {
                    let buckets = build_buckets(&resp);
                    if buckets.is_empty() {
                        return view! {
                            <div class="histo-hint">"No time series for this query"</div>
                        }.into_any();
                    }
                    let max = buckets.iter().map(|b| b.ok + b.err).max().unwrap_or(1).max(1);
                    let half = max / 2;
                    let bucket_count = buckets.len();
                    let (x_start, x_mid, x_end) = x_axis_labels(&range.get(), bucket_count);
                    view! {
                        <div class="yax">
                            <span>{max}</span>
                            <span>{half}</span>
                            <span>"0"</span>
                        </div>
                        <div class="bars">
                            {buckets.into_iter().enumerate().map(|(i, b)| {
                                let total = b.ok + b.err;
                                let ok_h = pct(b.ok, max);
                                let err_h = pct(b.err, max);
                                let tip = format!(
                                    "Bucket {i} · {total} events{}",
                                    if b.err > 0 { format!(" · {} errors", b.err) } else { String::new() }
                                );
                                view! {
                                    <div class="bar">
                                        <div class="tip">{tip}</div>
                                        <div class="ok" style=format!("height:{ok_h:.1}%")></div>
                                        <div class="err" style=format!("height:{err_h:.1}%")></div>
                                    </div>
                                }
                            }).collect::<Vec<_>>()}
                        </div>
                        <div class="xax">
                            <span>{x_start}</span>
                            <span>{x_mid}</span>
                            <span>{x_end}</span>
                        </div>
                    }.into_any()
                })
            />
        </div>
        {move || rows.get().and_then(Result::ok).map(|resp| {
            let buckets = build_buckets(&resp);
            let ti = resp.result.columns.iter().position(|c| matches!(c.name.as_str(), "_time" | "time" | "timestamp" | "@timestamp"));
            let times: Vec<f64> = resp.result.rows.iter().filter_map(|row| row.get(ti?).and_then(value_to_seconds)).filter(|t| t.is_finite()).collect();
            let min = times.iter().copied().fold(f64::INFINITY, f64::min);
            let max = times.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            let width = (max - min).max(1.0) / 48.0;
            (!buckets.is_empty()).then(|| view! {
                <details class="bucket-data">
                    <summary>"Histogram bucket data"</summary>
                    <div class="bucket-scroll" tabindex="0" role="region" aria-label="Histogram bucket data">
                        <table>
                            <caption>"Current page only. Times are UTC, rounded outward to milliseconds. Counts use unrounded intervals."</caption>
                            <thead><tr><th scope="col">"Start"</th><th scope="col">"End"</th><th scope="col">"Events"</th><th scope="col">"Errors"</th></tr></thead>
                            <tbody>{buckets.into_iter().enumerate().map(|(i, b)| {
                                let start = min + f64::from(u32::try_from(i).unwrap_or(0)) * width;
                                view! { <tr><td>{bucket_time(start, false)}</td><td>{bucket_time(start + width, true)}</td><td>{b.ok + b.err}</td><td>{b.err}</td></tr> }
                            }).collect::<Vec<_>>()}</tbody>
                        </table>
                    </div>
                </details>
            })
        })}
    }
}

#[allow(clippy::cast_possible_truncation)]
fn bucket_time(seconds: f64, end: bool) -> String {
    let millis = seconds * 1000.0;
    let bound = if end { millis.ceil() } else { millis.floor() };
    chrono::DateTime::from_timestamp_millis(bound as i64).map_or_else(
        || seconds.to_string(),
        |dt| dt.format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
    )
}

fn build_buckets(resp: &QueryResponse) -> Vec<Bucket> {
    let cols = &resp.result.columns;
    let time_idx = cols.iter().position(|c| {
        matches!(
            c.name.as_str(),
            "_time" | "time" | "timestamp" | "@timestamp"
        )
    });
    // `_severity` only (ADR-0013 §9): the derived slot nothing can
    // shadow. A bare `severity` column is ordinary sender data.
    let severity_idx = crate::severity_cell::severity_column(cols.iter().map(|c| c.name.as_str()));
    let Some(ti) = time_idx else {
        return Vec::new();
    };

    let mut events = Vec::with_capacity(resp.result.rows.len());
    for row in &resp.result.rows {
        let Some(t) = row.get(ti).and_then(value_to_seconds) else {
            continue;
        };
        let is_err = crate::severity_cell::row_is_error(row, severity_idx);
        if t.is_finite() {
            events.push((t, is_err));
        }
    }
    if events.is_empty() {
        return Vec::new();
    }
    bucketize(&events, N_BUCKETS)
}

#[allow(clippy::cast_precision_loss)] // bucket math tolerates 52-bit precision
fn value_to_seconds(v: &Value) -> Option<f64> {
    match v {
        Value::Integer(n) => Some(*n as f64),
        Value::Float(f) => Some(*f),
        Value::String(s) => {
            // Accept seconds as a numeric string, or an RFC3339-ish
            // timestamp parseable by chrono.
            if let Ok(n) = s.parse::<f64>() {
                return Some(n);
            }
            fleet_ui::time::parse_timestamp(s).map(|dt| {
                dt.timestamp() as f64 + f64::from(dt.timestamp_subsec_nanos()) / 1_000_000_000.0
            })
        }
        _ => None,
    }
}

fn pct(n: u32, max: u32) -> f64 {
    (f64::from(n) / f64::from(max)) * 100.0
}

/// The three x-axis labels: the window's start on the left (`-Xm`, or the
/// compact absolute stamp), the midpoint bucket's index in the middle,
/// and `now` (or the compact end stamp) on the right.
fn x_axis_labels(range: &RangeSpec, buckets: usize) -> (String, String, String) {
    match range {
        RangeSpec::Quick(q) => {
            let label = format!("-{q}");
            let mid = format!("{}", buckets / 2);
            (label, mid, "now".to_string())
        }
        RangeSpec::Absolute { from, to } => {
            (compact_ts(from), format!("{}", buckets / 2), compact_ts(to))
        }
    }
}

fn compact_ts(s: &str) -> String {
    // Trim RFC3339 timezone suffixes and microseconds for compactness.
    // "2026-04-18T13:32:17Z" → "13:32"; "now" passes through.
    if s == "now" {
        return s.to_string();
    }
    if let Some(rest) = s.split_once('T').map(|(_, r)| r)
        && rest.len() >= 5
    {
        return rest.chars().take(5).collect();
    }
    s.chars().take(10).collect()
}
