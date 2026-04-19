// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Histogram/>` — 78px strip rendering a stacked ok/err bar chart
//! over the current page's results.
//!
//! Time bucketing leans on `crate::histogram::bucketize`. Timestamps
//! come from any `_time` / `time` / `timestamp` column when present;
//! "is error" is `level == "error"` when a `level` column is present.
//! Anything missing → render the "no histogram on this page" hint.

use leptos::prelude::*;
use trawl_api::QueryResponse;
use trawl_api::value::Value;

use crate::api::ApiError;
use crate::histogram::{Bucket, bucketize};

const N_BUCKETS: usize = 48;

#[component]
pub fn Histogram(rows: LocalResource<Result<QueryResponse, ApiError>>) -> impl IntoView {
    view! {
        <div class="histo">
            {move || match rows.get() {
                None => view! {
                    <div class="histo-hint">"loading…"</div>
                }.into_any(),
                Some(Err(_)) => view! {
                    <div class="histo-hint">"—"</div>
                }.into_any(),
                Some(Ok(resp)) => {
                    let buckets = build_buckets(&resp);
                    if buckets.is_empty() {
                        return view! {
                            <div class="histo-hint">"no time series for this query"</div>
                        }.into_any();
                    }
                    let max = buckets.iter().map(|b| b.ok + b.err).max().unwrap_or(1).max(1);
                    view! {
                        <div class="bars">
                            {buckets.into_iter().enumerate().map(|(i, b)| {
                                let total = b.ok + b.err;
                                let ok_h = pct(b.ok, max);
                                let err_h = pct(b.err, max);
                                let tip = format!(
                                    "bucket {i} · {total} events{}",
                                    if b.err > 0 { format!(" · {} errors", b.err) } else { String::new() }
                                );
                                view! {
                                    <div class="bar" title=tip>
                                        <div class="ok" style=format!("height:{ok_h:.1}%")></div>
                                        <div class="err" style=format!("height:{err_h:.1}%")></div>
                                    </div>
                                }
                            }).collect::<Vec<_>>()}
                        </div>
                    }.into_any()
                }
            }}
        </div>
    }
}

fn build_buckets(resp: &QueryResponse) -> Vec<Bucket> {
    let cols = &resp.result.columns;
    let time_idx = cols.iter().position(|c| {
        matches!(
            c.name.as_str(),
            "_time" | "time" | "timestamp" | "@timestamp"
        )
    });
    let level_idx = cols.iter().position(|c| c.name == "level");
    let Some(ti) = time_idx else {
        return Vec::new();
    };

    let mut events = Vec::with_capacity(resp.result.rows.len());
    for row in &resp.result.rows {
        let Some(t) = row.get(ti).and_then(value_to_seconds) else {
            continue;
        };
        let is_err = level_idx
            .and_then(|li| row.get(li))
            .and_then(value_as_str)
            .is_some_and(|s| s.eq_ignore_ascii_case("error"));
        events.push((t, is_err));
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
            chrono::DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|dt| dt.timestamp() as f64)
        }
        _ => None,
    }
}

fn value_as_str(v: &Value) -> Option<&str> {
    if let Value::String(s) = v {
        Some(s.as_str())
    } else {
        None
    }
}

fn pct(n: u32, max: u32) -> f64 {
    (f64::from(n) / f64::from(max)) * 100.0
}
