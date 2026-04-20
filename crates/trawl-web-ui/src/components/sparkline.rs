// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Sparkline/>` — inline SVG line + fill area for per-service cards.
//! Data is a slice of non-negative counts (e.g. `daily_event_counts`).
//! Auto-scales to the max value; renders a flat line if all values are
//! zero. Color comes from the caller so the card can tint by status.

use leptos::prelude::*;

#[component]
#[allow(clippy::needless_pass_by_value)] // Leptos prop ergonomics
pub fn Sparkline(
    /// Sample values, left-to-right chronological.
    data: Vec<u64>,
    /// Stroke/fill color (CSS). Fill is applied at 10% opacity.
    #[prop(into, default = "var(--amber)".to_string())]
    color: String,
    #[prop(default = 120)] w: u32,
    #[prop(default = 22)] h: u32,
) -> impl IntoView {
    // Render nothing for degenerate inputs — the parent card will
    // collapse the slot. An empty <svg> would eat layout space.
    if data.len() < 2 {
        return view! { <svg class="sc-spark" width=w height=h viewBox=format!("0 0 {w} {h}")></svg> }
            .into_any();
    }

    #[allow(clippy::cast_precision_loss)] // sparkline scale, not a measurement
    let max = data.iter().copied().max().unwrap_or(0).max(1) as f64;
    #[allow(clippy::cast_precision_loss)] // see above; values bounded by ingest
    let last_idx = (data.len() - 1) as f64;
    #[allow(clippy::cast_precision_loss)]
    let w_f = f64::from(w);
    #[allow(clippy::cast_precision_loss)]
    let h_f = f64::from(h);

    let pts: Vec<String> = data
        .iter()
        .enumerate()
        .map(|(i, v)| {
            #[allow(clippy::cast_precision_loss)]
            let x = (i as f64 / last_idx) * w_f;
            #[allow(clippy::cast_precision_loss)]
            let y = h_f - (*v as f64 / max) * (h_f - 2.0) - 1.0;
            format!("{x:.2},{y:.2}")
        })
        .collect();
    let line = pts.join(" ");
    let fill = format!("0,{h_f} {line} {w_f},{h_f}");
    let stroke = color.clone();

    view! {
        <svg class="sc-spark" width=w height=h viewBox=format!("0 0 {w} {h}")>
            <polygon points=fill fill=stroke opacity="0.10"/>
            <polyline points=line fill="none" stroke=color stroke-width="1.25" stroke-linejoin="round"/>
        </svg>
    }
    .into_any()
}
