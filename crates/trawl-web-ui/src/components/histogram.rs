// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Histogram/>` — 78px strip rendering a stacked ok/err bar chart
//! over the current page's results.
//!
//! Time bucketing leans on `crate::histogram::bucketize_series`, which
//! also carries the bounds the bars were laid out over: the axis
//! labels, the bar tooltips and the accessible bucket table all measure
//! from that one `Series`, so none of them can disagree about where a
//! bucket starts. Timestamps come from a `_time` / `time` / `timestamp`
//! / `@timestamp` column; with none of them there is nothing to bucket
//! and the strip says so.
//!
//! "is error" is `_severity >= 17` (the `OTel` error band and above,
//! ADR-0013 §9); a page carrying no `_severity` column draws every bar
//! as ok.
//!
//! The x axis reads real timestamps off the data, and one caption names
//! the window the effective query ran under — which is the DSL's own
//! time clause when it carries one, not the range the picker shows
//! (ADR-0027, amended 2026-09-12).
//!
//! The caption is the one part of the strip that does not come from the
//! response, so it is the one part that can disagree with it: a
//! resource holds its previous page while the next request is in
//! flight, while `window` follows the URL at once. `pending` is what
//! keeps them honest — while a snapshot is running the caption is not
//! rendered at all, so it never names a window the bars below it were
//! not drawn over.

use fleet_ui::{LoadState, Loaded};
use leptos::prelude::*;
use trawl_api::QueryResponse;
use trawl_api::value::Value;

use crate::api::ApiError;
use crate::histogram::{Series, axis_labels, bucket_time, bucketize_series};
use crate::state::query::{EffectiveWindow, window_caption};

const N_BUCKETS: usize = 48;

#[component]
pub fn Histogram(
    rows: LocalResource<Result<crate::state::search_session::ExecutedResponse, ApiError>>,
    /// The time restriction the effective query ran under — what the
    /// caption states. Not the picker's range: the two differ whenever
    /// the query carries its own `last=`.
    #[prop(into)]
    window: Signal<EffectiveWindow>,
    /// Whether a snapshot request is in flight. The caption describes a
    /// completed response or nothing.
    #[prop(into)]
    pending: Signal<bool>,
    /// Render the caption row alone, with no bar strip and no bucket
    /// table. An aggregate page has one row per group and no events to
    /// bucket, so the strip could only ever paint the "No usable
    /// timestamps in shown events." band — a 64px report of an absence
    /// nobody asked about. The caption still names the executed window,
    /// which is the one thing the row above the table has to say.
    #[prop(optional)]
    caption_only: bool,
) -> impl IntoView {
    view! {
        {(!caption_only).then(|| view! { <div class="histo">
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
        </div> })}
        {move || rows.get().and_then(Result::ok).map(|resp| {
            let series = build_series(&resp);
            view! {
                <Show when=move || !pending.get()>
                    <p class="histo-caption">
                        {move || format!("Current page · window: {}", window_caption(&window.get()))}
                    </p>
                </Show>
                {(!caption_only).then_some(series).flatten().map(|series| {
                    let width = series.bucket_width();
                    let min = series.min_secs;
                    view! {
                        <details class="bucket-data">
                            <summary>"Histogram bucket data"</summary>
                            <div class="bucket-scroll" tabindex="0" role="region" aria-label="Histogram bucket data">
                                <table>
                                    <caption>"Times are UTC, rounded outward to milliseconds. Counts use unrounded intervals."</caption>
                                    <thead><tr><th scope="col">"Start"</th><th scope="col">"End"</th><th scope="col">"Events"</th><th scope="col">"Errors"</th></tr></thead>
                                    <tbody>{series.buckets.into_iter().enumerate().map(|(i, b)| {
                                        let start = bucket_start(min, width, i);
                                        view! { <tr><td>{bucket_time(start, false)}</td><td>{bucket_time(start + width, true)}</td><td>{b.ok + b.err}</td><td>{b.err}</td></tr> }
                                    }).collect::<Vec<_>>()}</tbody>
                                </table>
                            </div>
                        </details>
                    }
                })}
            }
        })}
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
