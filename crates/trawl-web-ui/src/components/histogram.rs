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
//!
//! A bar's tooltip is centred on it, then measured against the strip and
//! shifted sideways (`--tip-shift`) so an edge bar's tip never runs past
//! the strip, where `.search-col` clips it. The shift is
//! `crate::histogram::tip_shift`. A bar's tip is placed when the bar is
//! hovered or focused, and every tip is placed again after the bars
//! render and whenever the strip resizes. A tip already on screen then
//! follows a window resize or a live refresh without the pointer or the
//! focus having to move.

use fleet_ui::{LoadState, Loaded};
use leptos::prelude::*;
use trawl_api::QueryResponse;
use trawl_api::value::Value;

use wasm_bindgen::JsCast;
use web_sys::HtmlElement;

use crate::histogram::{Series, axis_labels, bucket_time, bucketize_series, tip_shift};

const N_BUCKETS: usize = 48;

/// Pixels a shifted tip keeps clear of each strip edge. Offsets are whole
/// pixels but the flex layout is fractional: the bar's left and width,
/// the tip's width and the strip's width can each be half a pixel off,
/// and the two halvings in `tip_shift` floor. Clamping into a strip inset
/// by this much on both sides keeps that error inside the real strip.
const TIP_EDGE: i32 = 3;

#[component]
pub fn Histogram(
    rows: LocalResource<
        Result<
            crate::state::search_session::ExecutedResponse,
            crate::state::search_session::ExecutedFailure,
        >,
    >,
) -> impl IntoView {
    let strip = NodeRef::<leptos::html::Div>::new();
    // Every bar moves when the strip changes width, including the one
    // whose tip is on screen. The observer disconnects with the
    // component through leptos-use.
    let _ = leptos_use::use_resize_observer(strip, move |_, _| {
        if let Some(strip) = strip.get_untracked() {
            place_all_tips(&strip);
        }
    });
    view! {
        <div class="histo" node_ref=strip>
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
                    // New buckets or labels replace every bar, and the
                    // strip keeps its size, so the observer stays quiet.
                    // Place the new tips once they are laid out. Nothing
                    // cancels the frame, and switching tab or leaving the
                    // page can dispose the strip before it runs; a
                    // disposed ref reads as `None` and the frame does
                    // nothing.
                    request_animation_frame(move || {
                        if let Some(strip) = strip.try_get_untracked().flatten() {
                            place_all_tips(&strip);
                        }
                    });
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
                                    <div
                                        class="bar"
                                        tabindex="0"
                                        role="img"
                                        aria-label=tip
                                        on:mouseenter=move |ev| place_tip(&ev)
                                        on:focus=move |ev| place_tip(&ev)
                                    >
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

/// Measure the entered or focused bar's tip against the strip and set the
/// tip's `--tip-shift`, which the stylesheet adds to its centring
/// transform.
///
/// Measured at the moment it is shown rather than once at render: the
/// strip's width follows the viewport and the tip's width its label.
fn place_tip(ev: &web_sys::Event) {
    let Some(bar) = ev
        .current_target()
        .and_then(|t| t.dyn_into::<HtmlElement>().ok())
    else {
        return;
    };
    let Some(strip) = bar.closest(".histo").ok().flatten() else {
        return;
    };
    place_bar_tip(&bar, &strip);
}

/// Place every bar's tip in `strip`. Placing all of them, rather than
/// looking for the one on screen, needs no guess about which tips hover
/// and focus are showing (they can be two different bars), and it costs a
/// few offset reads for each of 48 bars.
fn place_all_tips(strip: &web_sys::Element) {
    let Some(bars) = strip.query_selector(".bars").ok().flatten() else {
        return;
    };
    let mut next = bars.first_element_child();
    while let Some(bar) = next {
        next = bar.next_element_sibling();
        if let Ok(bar) = bar.dyn_into::<HtmlElement>() {
            place_bar_tip(&bar, strip);
        }
    }
}

/// Measure one bar's tip against the strip and set its `--tip-shift`.
fn place_bar_tip(bar: &HtmlElement, strip: &web_sys::Element) {
    let Some(tip) = bar
        .query_selector(".tip")
        .ok()
        .flatten()
        .and_then(|t| t.dyn_into::<HtmlElement>().ok())
    else {
        return;
    };
    // `.histo` is the nearest positioned ancestor, so it is the bar's
    // offsetParent and one `offset_left` is the bar's position in the
    // strip. Walk the chain regardless, so a positioned wrapper added
    // between them later still measures from the strip.
    let mut bar_left = 0;
    let mut node = bar.clone();
    loop {
        bar_left += node.offset_left();
        match node.offset_parent() {
            Some(parent) if parent == *strip => break,
            Some(parent) => match parent.dyn_into::<HtmlElement>() {
                Ok(parent) => node = parent,
                Err(_) => return,
            },
            // Not laid out, or the strip is not an ancestor that
            // positions it: nothing to measure against.
            None => return,
        }
    }
    let shift = tip_shift(
        bar_left - TIP_EDGE,
        bar.offset_width(),
        tip.offset_width(),
        strip.client_width() - 2 * TIP_EDGE,
    );
    let _ = tip
        .style()
        .set_property("--tip-shift", &format!("{shift}px"));
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
        Value::UInt(u) => Some(*u as f64),
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
