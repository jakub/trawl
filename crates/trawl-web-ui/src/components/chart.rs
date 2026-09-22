// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Chart/>` — the Visualization tab's uPlot chart (ADR-0038).
//!
//! Every decision about WHAT to draw lives in [`crate::chart_hint`],
//! [`crate::series`] and [`crate::categorical`], which are
//! native-testable; this module is the wasm half: the chart type
//! picker, the uPlot data arrays, the mount/update/destroy lifecycle,
//! and the sentences around the canvas.

use leptos::prelude::*;
use trawl_api::PaginationMeta;
use trawl_api::value::QueryResult;
use wasm_bindgen::{JsCast, JsValue};

use crate::categorical::detect;
use crate::chart_hint::{Hint, chart_hint, final_stage_is_timechart};
use crate::fetch_plan::coverage_refusal;
use crate::interop::uplot::{ChartHandle, ChartKind, Opts, create_chart};
use crate::series::{CatPoints, Lane, SeriesSet, caption, cat_points};

/// Everything that lives in the chart OPTIONS rather than its data: the
/// y-series labels, the ordinal x labels, and the kind. A data-only
/// update is safe only while all three hold.
type MountedOpts = (Vec<String>, Option<Vec<String>>, &'static str);

/// One of the three shapes the Visualization tab draws. Session state,
/// never part of the search URL (ADR-0027).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChartType {
    Line,
    Column,
    Bar,
}

/// The picker's order, and the order of the `fits` array.
const TYPES: [ChartType; 3] = [ChartType::Line, ChartType::Column, ChartType::Bar];

/// Why Line does not fit a result.
const LINE_NEEDS: &str = "Line needs a timechart result.";
/// Why Column and Bar do not fit a result.
const BARS_NEED: &str = "Column and Bar need a stats … by result with one group and one metric.";

impl ChartType {
    /// How the uPlot bridge draws this type.
    pub fn as_kind(self) -> ChartKind {
        match self {
            Self::Line => ChartKind::Line,
            Self::Column => ChartKind::Column,
            Self::Bar => ChartKind::Bar,
        }
    }

    /// The value published as `data-chart-type` on the chart host.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Line => "line",
            Self::Column => "column",
            Self::Bar => "bar",
        }
    }

    /// The picker's label, and this type's slot in `fits`.
    fn label(self) -> &'static str {
        match self {
            Self::Line => "Line",
            Self::Column => "Column",
            Self::Bar => "Bar",
        }
    }

    fn slot(self) -> usize {
        match self {
            Self::Line => 0,
            Self::Column => 1,
            Self::Bar => 2,
        }
    }
}

/// The type actually drawn: the reader's choice while it fits the result
/// on screen, otherwise the shape's default.
///
/// A choice that does not fit is never overwritten — the reader picked
/// Column for a `stats … by` and a `timechart` arrived; when the next
/// `stats … by` lands, Column is still what they asked for.
fn effective_type(
    selected: Option<ChartType>,
    default: ChartType,
    fits: &[Option<&'static str>; 3],
) -> ChartType {
    selected
        .filter(|ty| fits[ty.slot()].is_none())
        .unwrap_or(default)
}

/// What the tab shows for the current snapshot.
#[derive(Clone, PartialEq)]
enum Outcome {
    /// Neither a result nor a refusal: no frame has arrived, or the
    /// stream died. The sentence is the component's own.
    Note(String),
    /// A result the ladder will not draw, and why.
    Refused(Hint),
    /// Series aligned on the bucket grid, for Line.
    Lines(SeriesSet),
    /// One value per group on an ordinal axis, with the metric column's
    /// name, for Column and Bar.
    Groups(CatPoints, String),
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

/// uPlot `AlignedData` for Column and Bar: one ordinal x per group and
/// one y series. The group text rides in `Opts::x_labels`, not in the
/// data.
fn groups_data(points: &CatPoints) -> JsValue {
    let aligned = js_sys::Array::new();
    let xs = js_sys::Array::new_with_length(idx(points.values.len()));
    for i in 0..points.values.len() {
        // Ordinal positions, bounded by `GROUP_CAP`.
        #[allow(clippy::cast_precision_loss)]
        xs.set(idx(i), JsValue::from_f64(i as f64));
    }
    aligned.push(&xs);
    aligned.push(&column(&points.values));
    aligned.into()
}

/// One radio group per mounted picker. A `name` shared by two groups
/// would make them one group, so the counter hands each instance its
/// own — cheaper and more certain than reasoning about which tab bodies
/// can be mounted together.
static PICKER_SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The three chart types, with the reasons the ones that do not fit the
/// result on screen are disabled.
///
/// Native `input type=radio` rather than `role="radio"` buttons: the
/// browser then owns arrow-key movement inside the group, the single tab
/// stop, and the checked state, none of which this component would get
/// for free otherwise. The segmented look rides the labels
/// (`input:checked + .seg-opt` in main.css).
#[component]
fn ChartTypePicker(
    selected: RwSignal<Option<ChartType>>,
    #[prop(into)] default: Signal<ChartType>,
    #[prop(into)] fits: Signal<[Option<&'static str>; 3]>,
) -> impl IntoView {
    let group = format!(
        "chart-type-{}",
        PICKER_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    view! {
        // `role` overrides the fieldset's own `group`, so the control
        // announces the exclusive choice it is; the legend names it for
        // a reader who turns styles off.
        <fieldset class="chart-types seg seg-sm" role="radiogroup" aria-label="Chart type">
            <legend class="sr-only">"Chart type"</legend>
            {TYPES.iter().copied().map(|ty| {
                // The checked option is the type the chart DRAWS, which
                // is the default while the stored choice does not fit.
                let checked = move || effective_type(selected.get(), default.get(), &fits.get()) == ty;
                let reason = move || fits.get()[ty.slot()];
                let id = format!("{group}-{}", ty.as_str());
                view! {
                    <input
                        type="radio"
                        class="chart-type-input"
                        id=id.clone()
                        name=group.clone()
                        value=ty.as_str()
                        prop:checked=checked
                        disabled=move || reason().is_some()
                        title=reason
                        on:change=move |_| selected.set(Some(ty))
                    />
                    // The reason again on the label, which is the part
                    // a pointer can hover: the input itself is clipped.
                    <label class="seg-opt" for=id title=reason>{ty.label()}</label>
                }
            }).collect_view()}
        </fieldset>
    }
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
    /// The reader's chart type for this session, shared by both lanes.
    /// `None` follows the result's shape.
    chart_type: RwSignal<Option<ChartType>>,
    /// Open the Events tab: the refusal's alternative, and the caption's
    /// route to what the chart left out.
    on_events: Callback<()>,
    #[prop(optional, into)] failure: Signal<Option<&'static str>>,
    #[prop(optional)] on_retry: Option<Callback<()>>,
) -> impl IntoView {
    // Which types the result on screen admits. Line wants a result the
    // query left on a bucket axis — the LAST aggregation must be the
    // timechart, or there is no `_time` column to draw on; Column and
    // Bar want the shape the Events tab's categorical chart already
    // draws.
    let line_fits = Memo::new(move |_| final_stage_is_timechart(&query.get()));
    let cat_shape = Memo::new(move |_| {
        snapshot
            .get()
            .and_then(|result| detect(&query.get(), &result))
    });
    let fits = Memo::new(move |_| {
        let lines = (!line_fits.get()).then_some(LINE_NEEDS);
        let bars = cat_shape.get().is_none().then_some(BARS_NEED);
        [lines, bars, bars]
    });
    // The default follows the result's shape. Line for a `timechart`,
    // Column for a `stats … by`, and Line for anything else — whose
    // refusal is the one that names a fix.
    let default_type = Memo::new(move |_| {
        if line_fits.get() {
            ChartType::Line
        } else if cat_shape.get().is_some() {
            ChartType::Column
        } else {
            ChartType::Line
        }
    });
    let effective =
        Memo::new(move |_| effective_type(chart_type.get(), default_type.get(), &fits.get()));

    let outcome = Memo::new(move |_| {
        if let Some(message) = failure.get() {
            return Outcome::Note(message.to_owned());
        }
        let Some(result) = snapshot.get() else {
            return Outcome::Note("Waiting for the first live aggregation snapshot.".to_owned());
        };
        let query = query.get();
        let coverage = coverage.get();
        if effective.get() == ChartType::Line {
            return match chart_hint(&query, &result, lane, coverage.as_ref()) {
                Ok(set) => Outcome::Lines(set),
                Err(hint) => Outcome::Refused(hint),
            };
        }
        let Some(shape) = cat_shape.get() else {
            // The Line ladder's sentence names the fix — `NoTime` even
            // points at Column — so a result with nothing to group by
            // gets the more specific refusal, not the picker's reason.
            return match chart_hint(&query, &result, lane, coverage.as_ref()) {
                Err(hint) => Outcome::Refused(hint),
                Ok(_) => Outcome::Refused(Hint {
                    message: BARS_NEED.to_owned(),
                    offers_events: true,
                }),
            };
        };
        let points = cat_points(&shape, &result);
        // The coverage rung stays LAST, whatever the type: a shape
        // refusal names a fix in the query, and "this is only part of
        // the result" is the answer only once the shape is chartable.
        match coverage.as_ref().and_then(coverage_refusal) {
            Some(message) => Outcome::Refused(Hint {
                message,
                offers_events: true,
            }),
            None => Outcome::Groups(points, shape.metric_name),
        }
    });

    // The line under a chart that drew fewer series or groups than the
    // result holds. A legend of six over a result of fourteen is a
    // picture that lies by omission without it (ADR-0038).
    let caption_line = Memo::new(move |_| match outcome.get() {
        Outcome::Lines(set) => {
            let narrow = (!set.group_fields.is_empty()).then(|| set.group_fields.join(", "));
            caption(
                set.series.len(),
                set.total_series,
                "series",
                narrow.as_deref(),
            )
        }
        Outcome::Groups(points, _) => {
            caption(points.labels.len(), points.total_groups, "groups", None)
        }
        Outcome::Note(_) | Outcome::Refused(_) => None,
    });

    let node_ref = NodeRef::<leptos::html::Div>::new();
    let handle: StoredValue<Option<ChartHandle>, leptos::prelude::LocalStorage> =
        StoredValue::new_local(None);

    let mounted = StoredValue::new(None::<MountedOpts>);
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
        let ty = effective.get();
        let frame = match outcome.get() {
            Outcome::Lines(set) => Some((
                lines_data(&set),
                set.series.iter().map(|(label, _)| label.clone()).collect(),
                None,
                set.xs.len(),
                set.series.len(),
            )),
            Outcome::Groups(points, metric) => Some((
                groups_data(&points),
                vec![metric],
                Some(points.labels.clone()),
                points.labels.len(),
                1,
            )),
            Outcome::Note(_) | Outcome::Refused(_) => None,
        };
        let Some((data, labels, x_labels, points, series)) = frame else {
            handle.update_value(|slot| {
                if let Some(h) = slot.take() {
                    h.destroy();
                }
            });
            mounted.set_value(None);
            drawn.set(None);
            return;
        };
        if measured_width <= 0.0 {
            return;
        }
        let html_el: web_sys::HtmlElement = (*element).clone().unchecked_into();
        let opts_key: MountedOpts = (labels.clone(), x_labels.clone(), ty.as_str());

        handle.update_value(|slot| {
            if mounted.get_value().as_ref() != Some(&opts_key) {
                if let Some(h) = slot.take() {
                    h.destroy();
                }
                mounted.set_value(Some(opts_key));
            }
            if let Some(h) = slot.as_ref() {
                h.set_data(data);
                h.resize(measured_width, 320.0);
            } else {
                // First frame — construct the chart. Line's x values are
                // epoch seconds parsed from `_time`, which the server
                // wrote at a zero offset, so the axis formats them as
                // UTC; an ordinal axis has no zone to format.
                let opts = Opts {
                    width: measured_width,
                    height: 320.0,
                    series: &labels,
                    y_label: None,
                    kind: ty.as_kind(),
                    utc: x_labels.is_none(),
                    x_labels: x_labels.as_deref(),
                    span_gaps: false,
                };
                let options = opts.to_js();
                let h = create_chart(&html_el, data, options);
                *slot = Some(h);
            }
        });
        drawn.set(Some((points, series, ty.as_str())));
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
            <ChartTypePicker selected=chart_type default=default_type fits=fits/>
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
                Outcome::Lines(_) | Outcome::Groups(..) => ().into_any(),
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
            {move || matches!(outcome.get(), Outcome::Lines(_)).then(|| view! {
                <p class="chart-note">"Metrics over time on a UTC axis, up to six series. Gaps mean no value was returned. Open Events for exact times and values."</p>
            })}
        </div>
    }
}
