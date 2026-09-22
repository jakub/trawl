// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<CatChart/>` — a bar per group for a `stats <metric> by <field>`
//! result.
//!
//! Plain divs, not uPlot. uPlot's category axis would need an
//! `xLabels` callback plumbed through the opts bridge and a draw hook
//! for the per-bar value labels. The Visualization tab draws the same
//! shape as Column or Bar through uPlot instead (ADR-0038); this list
//! stays beside the exact table because it is the table's companion,
//! not a chart of its own. A list of divs costs no dependency and reads
//! the same tokens.
//!
//! The chart is `aria-hidden`: every number in it is already in the
//! exact table beside it, which is the accessible representation. A
//! screen reader that read both would read the page twice.

use crate::categorical::CatShape;
use leptos::prelude::*;
use trawl_api::display::value_to_string;
use trawl_api::value::{QueryResult, Value};

#[component]
// A component owns its props: both are moved into the row closures that
// build the list, which clippy cannot see through the iterator.
#[allow(clippy::needless_pass_by_value)]
pub fn CatChart(shape: CatShape, result: QueryResult) -> impl IntoView {
    let title = format!("{} by {}", shape.metric_name, shape.group_name);
    let signed = shape.signed;
    let bars = result
        .rows
        .iter()
        .filter_map(|row| {
            let label = value_to_string(row.get(shape.group)?);
            let value = row.get(shape.metric)?;
            let is_null = matches!(value, Value::Null);
            let negative = matches!(value, Value::Integer(i) if *i < 0)
                || matches!(value, Value::Float(f) if *f < 0.0);
            let pct = shape.percent(value);
            let text = if is_null {
                "—".to_string()
            } else {
                value_to_string(value)
            };
            Some(view! {
                <li class:null=is_null class:neg=negative>
                    <span class="cat-lb">{label}</span>
                    <span class="cat-track">
                        // No fill for a null: an absent measurement is
                        // not a zero-length bar, it is no bar.
                        {(!is_null).then(|| view! {
                            <i style=format!("width:{pct:.2}%")></i>
                        })}
                    </span>
                    <span class="cat-val">{text}</span>
                </li>
            })
        })
        .collect::<Vec<_>>();

    view! {
        <div class="cat-chart">
            <p class="cat-title">{title}</p>
            <ol class="cat-bars" class:signed=signed aria-hidden="true">
                {bars}
            </ol>
        </div>
    }
}
