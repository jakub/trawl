// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Chart/>` — uPlot line chart driven by aggregation snapshots.

use leptos::prelude::*;
use trawl_api::display::extract_series;
use trawl_api::value::QueryResult;
use wasm_bindgen::JsValue;

use crate::interop::uplot::{ChartHandle, Opts, create_chart};

/// uPlot `AlignedData` is `[xs, ys1, ys2, ...]` where every inner array
/// is equal length and all values are `f64`. xs are row indices, not
/// instants: the server emits `_time` as a string, snapshot rows arrive
/// in order, and the chart only has to show relative shape.
fn snapshot_to_aligned(result: &QueryResult) -> (JsValue, Vec<String>) {
    let (series, _total) = extract_series(result);
    if series.is_empty() {
        return (js_sys::Array::new().into(), vec![]);
    }

    let series_len = series[0].1.len();
    let aligned = js_sys::Array::new();

    let xs = js_sys::Array::new_with_length(u32::try_from(series_len).unwrap_or(u32::MAX));
    for (i, x) in (0..series_len).enumerate() {
        // Snapshot lengths come from live-aggregated rows — bounded by
        // the server's snapshot cadence in the small thousands, so the
        // f64 precision loss at 2^53 boundary is not reachable here.
        #[allow(clippy::cast_precision_loss)]
        xs.set(
            u32::try_from(i).unwrap_or(u32::MAX),
            JsValue::from_f64(x as f64),
        );
    }
    aligned.push(&xs);

    let mut labels = Vec::with_capacity(series.len());
    for (label, values) in &series {
        labels.push(label.clone());
        let ys = js_sys::Array::new_with_length(u32::try_from(values.len()).unwrap_or(u32::MAX));
        for (i, v) in values.iter().enumerate() {
            // Aggregation values are counts/sums/averages produced by
            // DuckDB — also well below 2^53 in practice.
            #[allow(clippy::cast_precision_loss)]
            ys.set(
                u32::try_from(i).unwrap_or(u32::MAX),
                JsValue::from_f64(*v as f64),
            );
        }
        aligned.push(&ys);
    }

    (aligned.into(), labels)
}

#[component]
pub fn Chart(#[prop(into)] snapshot: Signal<Option<QueryResult>>) -> impl IntoView {
    let node_ref = NodeRef::<leptos::html::Div>::new();
    let handle: StoredValue<Option<ChartHandle>, leptos::prelude::LocalStorage> =
        StoredValue::new_local(None);

    Effect::new(move |_| {
        let Some(element) = node_ref.get() else {
            return;
        };
        let Some(result) = snapshot.get() else {
            return;
        };
        let (data, labels) = snapshot_to_aligned(&result);
        let html_el: web_sys::HtmlElement = (*element).clone().unchecked_into();

        handle.update_value(|slot| {
            if let Some(h) = slot.as_ref() {
                h.set_data(data);
            } else {
                // First snapshot — construct the chart. x values here are
                // row indices, not real instants, so `utc` stays off.
                let opts = Opts {
                    width: f64::from(html_el.client_width()),
                    height: 320.0,
                    series: &labels,
                    y_label: None,
                    bars: false,
                    utc: false,
                };
                let h = create_chart(&html_el, data, opts.to_js());
                *slot = Some(h);
            }
        });
    });

    on_cleanup(move || {
        handle.update_value(|slot| {
            if let Some(h) = slot.take() {
                h.destroy();
            }
        });
    });

    view! { <div class="chart" node_ref=node_ref></div> }
}

use wasm_bindgen::JsCast;
