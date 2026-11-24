// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use leptos::prelude::*;

use super::geometry::spark_path;

#[component]
#[allow(clippy::needless_pass_by_value)]
pub fn Sparkline(
    /// Sample values, left-to-right chronological.
    data: Vec<u64>,
    #[prop(into, default = "var(--accent)".to_string())] color: String,
    #[prop(default = 120)] w: u32,
    #[prop(default = 22)] h: u32,
) -> impl IntoView {
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
