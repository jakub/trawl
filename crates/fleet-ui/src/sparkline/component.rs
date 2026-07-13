// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `<Sparkline>` component. Wasm-only (pulls leptos) — the pure
//! point/scale math it renders lives in [`super::geometry`] so the
//! degenerate-input and scaling behaviour is native-testable.

use leptos::prelude::*;

use super::geometry::spark_path;

/// Inline SVG line + fill area. Data is a slice of non-negative counts
/// (e.g. `daily_event_counts`). Auto-scales to the max value; renders a
/// flat line if all values are zero. Color comes from the caller so the
/// parent can tint by status.
#[component]
#[allow(clippy::needless_pass_by_value)] // Leptos prop ergonomics
pub fn Sparkline(
    /// Sample values, left-to-right chronological.
    data: Vec<u64>,
    /// Stroke/fill color (CSS). Fill is applied at 10% opacity.
    #[prop(into, default = "var(--accent)".to_string())]
    color: String,
    #[prop(default = 120)] w: u32,
    #[prop(default = 22)] h: u32,
) -> impl IntoView {
    // Render nothing for degenerate inputs — the parent card will
    // collapse the slot. An empty <svg> would eat layout space.
    let Some(path) = spark_path(&data, f64::from(w), f64::from(h)) else {
        return view! { <svg class="sc-spark" width=w height=h viewBox=format!("0 0 {w} {h}")></svg> }
            .into_any();
    };
    let stroke = color.clone();

    view! {
        <svg class="sc-spark" width=w height=h viewBox=format!("0 0 {w} {h}")>
            <polygon points=path.fill fill=stroke opacity="0.10"/>
            <polyline points=path.line fill="none" stroke=color stroke-width="1.25" stroke-linejoin="round"/>
        </svg>
    }
    .into_any()
}
