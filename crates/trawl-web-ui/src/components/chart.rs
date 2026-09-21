// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Chart/>` — uPlot line chart driven by aggregation snapshots.

use leptos::prelude::*;
use trawl_api::display::extract_series;
use trawl_api::value::{QueryResult, Value};
use wasm_bindgen::JsValue;

use crate::fetch_plan::coverage_refusal;
use crate::interop::uplot::{ChartHandle, Opts, create_chart};
use trawl_api::PaginationMeta;

/// uPlot `AlignedData` is `[xs, ys1, ys2, ...]` where every inner array
/// is equal length and all values are `f64`. xs are row indices, not
/// instants: the server emits `_time` as a string, snapshot rows arrive
/// in order, and the chart only has to show relative shape.
/// The third element is the number of plotted points per series: what
/// the chart actually drew, which the canvas itself does not say.
fn snapshot_to_aligned(result: &QueryResult) -> (JsValue, Vec<String>, usize) {
    let (series, _total) = extract_series(result);
    if series.is_empty() {
        return (js_sys::Array::new().into(), vec![], 0);
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

    (aligned.into(), labels, series_len)
}

#[component]
pub fn Chart(
    #[prop(into)] snapshot: Signal<Option<QueryResult>>,
    #[prop(into)] query: Signal<String>,
    /// How much of the result this snapshot carries, where the answer is
    /// measured: `None` for a live stream, which owns no window.
    ///
    /// Required, not optional. The chart draws a whole result or says
    /// why not (ADR-0037), and a default would let the next caller skip
    /// that gate by saying nothing.
    #[prop(into)]
    coverage: Signal<Option<PaginationMeta>>,
    #[prop(optional, into)] failure: Signal<Option<&'static str>>,
    #[prop(optional)] on_retry: Option<Callback<()>>,
) -> impl IntoView {
    let hint = Memo::new(move |_| {
        failure
            .get()
            .map(ToOwned::to_owned)
            .or_else(|| match snapshot.get() {
                None => Some("Waiting for the first live aggregation snapshot.".to_owned()),
                Some(result) => {
                    let coverage = coverage.get();
                    chart_hint(&result, &query.get(), coverage.as_ref())
                }
            })
    });
    let node_ref = NodeRef::<leptos::html::Div>::new();
    let handle: StoredValue<Option<ChartHandle>, leptos::prelude::LocalStorage> =
        StoredValue::new_local(None);

    let mounted_labels = StoredValue::new(Vec::<String>::new());
    // The plotted series length, published on the host element. A canvas
    // is opaque: without this, "the chart drew every row" is a claim no
    // test can read back from the DOM.
    let points = RwSignal::new(None::<usize>);
    let width = RwSignal::new(0.0_f64);
    // Measure the content box, excluding the chart host's padding. The
    // observer disconnects with the component through leptos-use.
    let _ = leptos_use::use_resize_observer(node_ref, move |entries, _| {
        if let Some(entry) = entries.first() {
            let next = entry.content_rect().width();
            if (next - width.get_untracked()).abs() >= 0.5 {
                width.set(next);
            }
        }
    });

    Effect::new(move |_| {
        let Some(element) = node_ref.get() else {
            return;
        };
        let measured_width = width.get();
        let result = snapshot.get();
        let Some(result) = result.filter(|_| hint.get().is_none()) else {
            handle.update_value(|slot| {
                if let Some(h) = slot.take() {
                    h.destroy();
                }
            });
            mounted_labels.set_value(Vec::new());
            points.set(None);
            return;
        };
        if measured_width <= 0.0 {
            return;
        }
        let (data, labels, plotted) = snapshot_to_aligned(&result);
        let html_el: web_sys::HtmlElement = (*element).clone().unchecked_into();

        handle.update_value(|slot| {
            // Series labels belong to the chart options, so data-only updates
            // are safe only while the series stay the same.
            if mounted_labels.get_value() != labels {
                if let Some(h) = slot.take() {
                    h.destroy();
                }
                mounted_labels.set_value(labels.clone());
            }
            if let Some(h) = slot.as_ref() {
                h.set_data(data);
                h.resize(measured_width, 320.0);
            } else {
                // First snapshot — construct the chart. x values here are
                // row indices, not real instants, so `utc` stays off.
                let opts = Opts {
                    width: measured_width,
                    height: 320.0,
                    series: &labels,
                    y_label: None,
                    bars: false,
                    utc: false,
                };
                let options = opts.to_js();
                let _ = js_sys::Reflect::set(&options, &"rowIndex".into(), &JsValue::TRUE);
                let h = create_chart(&html_el, data, options);
                *slot = Some(h);
            }
        });
        points.set(Some(plotted));
    });

    on_cleanup(move || {
        handle.update_value(|slot| {
            if let Some(h) = slot.take() {
                h.destroy();
            }
        });
    });

    view! {
        <div class="visualization">
            // A failure is an alert, a waiting-for-data hint is a
            // status: the live stream's error must announce itself here
            // the way it does over the raw table.
            {move || hint.get().map(|hint| if failure.get().is_some() {
                view! { <p class="results-empty" role="alert">{hint}</p> }.into_any()
            } else {
                view! { <p class="results-empty" role="status">{hint}</p> }.into_any()
            })}
            {move || failure.get().and(on_retry).map(|retry| view! {
                <button type="button" class="btn-sec" on:click=move |_| retry.run(())>"Retry live stream"</button>
            })}
            <div class="chart" node_ref=node_ref data-points=move || points.get().map(|n| n.to_string())></div>
            {move || hint.get().is_none().then(|| view! {
                <p class="chart-note">"Count metrics by result position, up to six series. Open Events for exact times and values."</p>
            })}
        </div>
    }
}

use wasm_bindgen::JsCast;

// The shared extractor converts metrics to unsigned counts. Refuse values
// that conversion would truncate or replace with zero.
//
// The rungs are ordered, and the coverage one comes LAST on purpose. A
// grouped or lossy result has something actionable to say about its own
// shape; "this is only part of the result" is the answer only once the
// shape itself is chartable.
fn chart_hint(
    result: &QueryResult,
    query: &str,
    coverage: Option<&PaginationMeta>,
) -> Option<String> {
    // Column values cannot identify numeric group keys. Use parsed query
    // stages before interpreting any numeric column as a metric. Refuse
    // grouped results until this chart can align groups by actual time.
    let parsed = trawl_core::parser::parse(query).ok();
    let grouped = parsed.as_ref().is_some_and(|ast| {
        ast.pipeline.iter().any(|stage| match &stage.node {
            trawl_core::ast::PipeStage::Timechart(stage) => !stage.group_by.is_empty(),
            trawl_core::ast::PipeStage::Stats(stage) => !stage.group_by.is_empty(),
            trawl_core::ast::PipeStage::Pivot(_)
            | trawl_core::ast::PipeStage::Top(_)
            | trawl_core::ast::PipeStage::Rare(_) => true,
            _ => false,
        })
    });
    if grouped {
        return Some(
            "Grouped results are not supported by this chart. Use timechart without by, or open Events for exact group times and values."
                .to_owned(),
        );
    }
    if result.rows.is_empty() {
        return Some("No rows returned for this visualization.".to_owned());
    }
    if !result.columns.iter().any(|c| c.name == "_time") {
        return Some(
            "Visualization requires a _time column and non-negative integer metrics. Use timechart count() or open Events for these results."
                .to_owned(),
        );
    }
    let mut groups = 0;
    let mut metrics = 0;
    for (i, col) in result.columns.iter().enumerate() {
        if col.name == "_time" {
            continue;
        }
        if result
            .rows
            .iter()
            .all(|r| matches!(r.get(i), Some(Value::String(_))))
        {
            groups += 1;
        } else if result
            .rows
            .iter()
            .all(|r| matches!(r.get(i), Some(Value::Integer(n)) if *n >= 0))
        {
            metrics += 1;
        } else {
            return Some(
                "This chart supports non-negative integer metrics only. Open Events for fractional, negative, null, or mixed values."
                    .to_owned(),
            );
        }
    }
    if groups > 0 {
        return Some(
            "This result includes non-metric columns. Use an ungrouped timechart query, or open Events for these rows."
                .to_owned(),
        );
    }
    let timechart = parsed.as_ref().is_some_and(|ast| ast.pipeline.iter().any(|stage| {
        matches!(&stage.node, trawl_core::ast::PipeStage::Timechart(chart) if chart.group_by.is_empty())
    }));
    if metrics > 1 && !timechart {
        return Some(
            "These numeric columns may include group keys. Use an ungrouped timechart query, or open Events for the exact values."
                .to_owned(),
        );
    }
    if metrics == 0 {
        return Some(
            "Visualization requires _time and non-negative integer metrics. Open Events for these results."
                .to_owned(),
        );
    }
    coverage.and_then(coverage_refusal)
}
