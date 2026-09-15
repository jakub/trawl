// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Histogram/>` — 78px strip rendering a stacked ok/err bar chart
//! over the current page's results.
//!
//! Time bucketing leans on `crate::histogram::bucketize_series`, which
//! also carries the bounds the bars were laid out over: the axis
//! labels and the bar tooltips measure
//! from that one `Series`, so none of them can disagree about where a
//! bucket starts. Timestamps come from a `_time` / `time` / `timestamp`
//! / `@timestamp` column; with none of them there is nothing to bucket
//! and the strip says so.
//!
//! "is error" is `_severity >= 17` (the `OTel` error band and above,
//! ADR-0013 §9); a page carrying no `_severity` column draws every bar
//! as ok.
//!
//! The x axis reads real timestamps off the data.

use fleet_ui::{LoadState, Loaded};
use leptos::prelude::*;
use trawl_api::QueryResponse;
use trawl_api::value::Value;

use crate::api::ApiError;
use crate::histogram::{Series, axis_labels, bucket_time, bucketize_series};

const N_BUCKETS: usize = 48;

#[component]
pub fn Histogram(
    rows: LocalResource<Result<crate::state::search_session::ExecutedResponse, ApiError>>,
) -> impl IntoView {
    view! {
        <div class="histo">
            <Loaded
                state=Signal::derive(move || LoadState::from_resource(rows.get()))
                // Deliberate quiet-error override: the results table
                // already reports the query failure.
                error=Box::new(|_| view! { <div class="histo-hint">"—"</div> }.into_any())
                render=Box::new(move |resp: crate::state::search_session::ExecutedResponse| {
                    let Some(series) = build_series(&resp) else {
                        // Two different absences, two different sentences: a
                        // page with no rows at all, and a page whose rows
                        // carry no timestamp anyone can read.
                        let hint = if resp.result.rows.is_empty() {
                            "No events on this page."
                        } else {
                            "No usable timestamps in shown events."
                        };
                        return view! { <div class="histo-hint">{hint}</div> }.into_any();
                    };
                    let max = series.buckets.iter().map(|b| b.ok + b.err).max().unwrap_or(1).max(1);
                    let half = max / 2;
                    let (x_start, x_mid, x_end) = axis_labels(&series);
                    let width = series.bucket_width();
                    let min = series.min_secs;
                    view! {
                        <div class="yax">
                            <span>{max}</span>
                            <span>{half}</span>
                            <span>"0"</span>
                        </div>
                        <div class="bars">
                            {series.buckets.into_iter().enumerate().map(|(i, b)| {
                                let total = b.ok + b.err;
                                let ok_h = pct(b.ok, max);
                                let err_h = pct(b.err, max);
                                let start = bucket_start(min, width, i);
                                let tip = format!(
                                    "{} – {} · {total} events{}",
                                    bucket_time(start, false),
                                    bucket_time(start + width, true),
                                    if b.err > 0 { format!(" · {} errors", b.err) } else { String::new() }
                                );
                                view! {
                                    <div class="bar" tabindex="0" role="img" aria-label=tip>
                                        <div class="tip" aria-hidden="true">{tip.clone()}</div>
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
    }
}

#[allow(clippy::cast_precision_loss)] // bucket index is at most N_BUCKETS
fn bucket_start(min_secs: f64, width: f64, index: usize) -> f64 {
    min_secs + width * index as f64
}

fn build_series(resp: &QueryResponse) -> Option<Series> {
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
    let ti = time_idx?;

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
    bucketize_series(&events, N_BUCKETS)
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
