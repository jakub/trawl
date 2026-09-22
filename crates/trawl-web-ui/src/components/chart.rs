// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Chart/>` — the Visualization tab's uPlot chart (ADR-0038).
//!
//! Every decision about WHAT to draw lives in [`crate::chart_hint`] and
//! [`crate::series`], which are native-testable; this module is the
//! wasm half: the uPlot data arrays, the mount/update/destroy lifecycle,
//! and the sentences around the canvas.

use leptos::prelude::*;
use trawl_api::PaginationMeta;
use trawl_api::value::QueryResult;
use wasm_bindgen::{JsCast, JsValue};

use crate::chart_hint::{Hint, chart_hint};
use crate::interop::uplot::{ChartHandle, ChartKind, Opts, create_chart};
use crate::series::{Lane, SeriesSet, caption};

/// What the tab shows for the current snapshot.
#[derive(Clone, PartialEq)]
enum Outcome {
    /// Neither a result nor a refusal: no frame has arrived, or the
    /// stream died. The sentence is the component's own.
    Note(String),
    /// A result the ladder will not draw, and why.
    Refused(Hint),
    /// The aligned series to draw.
    Draw(SeriesSet),
}

/// A JS array index. Lengths here are bounded by
/// [`crate::series::MAX_INSTANTS`], far below `u32::MAX`.
fn idx(i: usize) -> u32 {
    u32::try_from(i).unwrap_or(u32::MAX)
}

/// One y column for uPlot. A gap is JS `null`: never `NaN`, which uPlot
/// folds into the y scale, and never a hole left by `new_with_length`,
/// which reads back as `undefined`.
fn column(values: &[Option<f64>]) -> js_sys::Array {
    let array = js_sys::Array::new_with_length(idx(values.len()));
    for (i, value) in values.iter().enumerate() {
        array.set(idx(i), value.map_or(JsValue::NULL, JsValue::from_f64));
    }
    array
}

/// uPlot `AlignedData` for a line chart: `[xs, ys1, ys2, …]`, every
/// inner array the same length. xs are epoch seconds, drawn on a UTC
/// axis.
fn lines_data(set: &SeriesSet) -> JsValue {
    let aligned = js_sys::Array::new();
    let xs = js_sys::Array::new_with_length(idx(set.xs.len()));
    for (i, x) in set.xs.iter().enumerate() {
        xs.set(idx(i), JsValue::from_f64(*x));
    }
    aligned.push(&xs);
    for (_, values) in &set.series {
        aligned.push(&column(values));
    }
    aligned.into()
}

#[component]
#[allow(clippy::too_many_lines)] // one component: the ladder's verdict, the canvas lifecycle, the sentences
pub fn Chart(
    #[prop(into)] snapshot: Signal<Option<QueryResult>>,
    /// The DSL that produced `snapshot`, never the text being typed: the
    /// series, the group fields and the bucket width are all read off
    /// it, so a stale pair draws a labelled lie.
    #[prop(into)]
    query: Signal<String>,
    /// How much of the result this snapshot carries, where the answer is
    /// measured: `None` for a live stream, which owns no window.
    ///
    /// Required, not optional. The chart draws a whole result or says
    /// why not (ADR-0037), and a default would let the next caller skip
    /// that gate by saying nothing.
    #[prop(into)]
    coverage: Signal<Option<PaginationMeta>>,
    /// Which execution produced the snapshot, and so which bucket width
    /// its rows were cut with.
    lane: Lane,
    /// Open the Events tab: the refusal's alternative, and the caption's
    /// route to what the chart left out.
    on_events: Callback<()>,
    #[prop(optional, into)] failure: Signal<Option<&'static str>>,
    #[prop(optional)] on_retry: Option<Callback<()>>,
) -> impl IntoView {
    let outcome = Memo::new(move |_| {
        if let Some(message) = failure.get() {
            return Outcome::Note(message.to_owned());
        }
        let Some(result) = snapshot.get() else {
            return Outcome::Note("Waiting for the first live aggregation snapshot.".to_owned());
        };
        match chart_hint(&query.get(), &result, lane, coverage.get().as_ref()) {
            Ok(set) => Outcome::Draw(set),
            Err(hint) => Outcome::Refused(hint),
        }
    });

    // The line under a chart that drew fewer series than the result
    // holds. A legend of six over a result of fourteen is a picture that
    // lies by omission without it (ADR-0038).
    let caption_line = Memo::new(move |_| match outcome.get() {
        Outcome::Draw(set) => {
            let narrow = (!set.group_fields.is_empty()).then(|| set.group_fields.join(", "));
            caption(
                set.series.len(),
                set.total_series,
                "series",
                narrow.as_deref(),
            )
        }
        Outcome::Note(_) | Outcome::Refused(_) => None,
    });

    let node_ref = NodeRef::<leptos::html::Div>::new();
    let handle: StoredValue<Option<ChartHandle>, leptos::prelude::LocalStorage> =
        StoredValue::new_local(None);

    let mounted_labels = StoredValue::new(Vec::<String>::new());
    // What the canvas drew, published on the host element. A canvas is
    // opaque: without these, "the chart drew every bucket of all four
    // series" is a claim no test can read back from the DOM.
    let drawn = RwSignal::new(None::<(usize, usize, &'static str)>);
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
        let Outcome::Draw(set) = outcome.get() else {
            handle.update_value(|slot| {
                if let Some(h) = slot.take() {
                    h.destroy();
                }
            });
            mounted_labels.set_value(Vec::new());
            drawn.set(None);
            return;
        };
        if measured_width <= 0.0 {
            return;
        }
        let labels: Vec<String> = set.series.iter().map(|(label, _)| label.clone()).collect();
        let data = lines_data(&set);
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
                // First frame — construct the chart. x values are epoch
                // seconds parsed from `_time`, which the server wrote at
                // a zero offset, so the axis formats them as UTC.
                let opts = Opts {
                    width: measured_width,
                    height: 320.0,
                    series: &labels,
                    y_label: None,
                    kind: ChartKind::Line,
                    utc: true,
                    x_labels: None,
                    span_gaps: false,
                };
                let options = opts.to_js();
                let h = create_chart(&html_el, data, options);
                *slot = Some(h);
            }
        });
        drawn.set(Some((set.xs.len(), labels.len(), "line")));
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
            // A failure is an alert, a waiting-for-data note is a
            // status: the live stream's error must announce itself here
            // the way it does over the raw table.
            {move || match outcome.get() {
                Outcome::Note(message) => if failure.get().is_some() {
                    view! { <p class="results-empty" role="alert">{message}</p> }.into_any()
                } else {
                    view! { <p class="results-empty" role="status">{message}</p> }.into_any()
                },
                Outcome::Refused(hint) => view! {
                    <p class="results-empty" role="status">{hint.message}</p>
                    {hint.offers_events.then(|| view! {
                        <button type="button" class="btn-sec" on:click=move |_| on_events.run(())>"Open Events"</button>
                    })}
                }.into_any(),
                Outcome::Draw(_) => ().into_any(),
            }}
            {move || failure.get().and(on_retry).map(|retry| view! {
                <button type="button" class="btn-sec" on:click=move |_| retry.run(())>"Retry live stream"</button>
            })}
            <div
                class="chart"
                node_ref=node_ref
                data-points=move || drawn.get().map(|(points, _, _)| points.to_string())
                data-series=move || drawn.get().map(|(_, series, _)| series.to_string())
                data-chart-type=move || drawn.get().map(|(_, _, kind)| kind)
            ></div>
            {move || caption_line.get().map(|text| view! {
                <p class="chart-caption" role="status">{text}</p>
                <button type="button" class="btn-lnk" on:click=move |_| on_events.run(())>"Open Events"</button>
            })}
            {move || matches!(outcome.get(), Outcome::Draw(_)).then(|| view! {
                <p class="chart-note">"Metrics over time on a UTC axis, up to six series. Gaps mean no value was returned. Open Events for exact times and values."</p>
            })}
        </div>
    }
}
