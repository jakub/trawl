// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Histogram/>` — 80px strip rendering a stacked ok/err bar chart of
//! every event in the result, above the snapshot table of a raw-event
//! query.
//!
//! The strip's input is the count trawld answers in the same response as
//! the page (`QueryResponse::histogram`): 48 half-open UTC bins from the
//! first usable `_time` of the whole result to past its last, with each
//! bin's errors (`_severity` at 17 or above, ADR-0013 §9). The browser
//! never buckets rows. Each page shows its own execution's count, so a
//! page turn redraws the strip from the response that answered it.
//!
//! What it shows, in order:
//!
//! 1. While the page's request is pending (the page's whole `loading`
//!    test, not only the transport flag), the shared loading hint, with no
//!    bars and no caption. A retained earlier response is never drawn.
//! 2. A failed request: the quiet `—`. The results table reports it.
//! 3. A result with no rows collapses the strip; the table says so.
//! 4. Otherwise `crate::histogram::strip` decides: no histogram is the
//!    `—`, an unavailable one says why, a ready one that breaks an
//!    invariant is the `—`, and anything else draws.
//!
//! A drawn chart carries a one-line key above the bars: what they count
//! (every event in the result, and those without a usable time), how long
//! one bucket is, that the bins are UTC, and what the colours mean. A
//! result without `_severity` gets one swatch. When the query kept only
//! some events (`head`, `tail`, `dedup`), a second line says so, and the
//! strip grows by that line. `crate::histogram` builds every string.
//!
//! The x axis reads the bins' integer bounds: round UTC clock times
//! inside the series, each placed at its own position
//! (`crate::histogram::clock_ticks`). The tick step depends on how wide
//! the axis is, so the axis width is measured after the bars render and
//! whenever the strip resizes, and only the axis re-renders when it
//! changes.
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

use wasm_bindgen::JsCast;
use web_sys::HtmlElement;

use crate::histogram::{
    Drawn, ERROR_KEY, NO_SEVERITY_KEY, OTHER_KEY, Series, Strip, TickAlign, clock_ticks, pct,
    scale_top, strip as strip_for, tip_shift,
};
use crate::state::search_session::{ExecutedFailure, ExecutedResponse};

/// Pixels a shifted tip keeps clear of each strip edge. Offsets are whole
/// pixels but the flex layout is fractional: the bar's left and width,
/// the tip's width and the strip's width can each be half a pixel off,
/// and the two halvings in `tip_shift` floor. Clamping into a strip inset
/// by this much on both sides keeps that error inside the real strip.
const TIP_EDGE: i32 = 3;

#[component]
pub fn Histogram(
    rows: LocalResource<Result<ExecutedResponse, ExecutedFailure>>,
    /// The page's `loading` test: true until the response on screen
    /// answers the request the page shows.
    #[prop(into)]
    pending: Signal<bool>,
) -> impl IntoView {
    let strip = NodeRef::<leptos::html::Div>::new();
    // The x axis's measured width in pixels; `None` until it is laid out.
    let axis_width = RwSignal::new(None::<f64>);
    // Every bar moves when the strip changes width, including the one
    // whose tip is on screen. The observer disconnects with the
    // component through leptos-use.
    let _ = leptos_use::use_resize_observer(strip, move |_, _| {
        if let Some(strip) = strip.get_untracked() {
            place_all_tips(&strip);
            measure_axis(&strip, axis_width);
        }
    });
    // One state for the render branch and both classes, so they can
    // never disagree: pending wins before the retained response is read,
    // so an earlier response, an empty one included, is never drawn.
    let state = Memo::new(move |_| {
        if pending.get() {
            return LoadState::Loading;
        }
        rows.with(|r| match r {
            None => LoadState::Loading,
            Some(Err(failure)) => LoadState::Error(failure.to_string()),
            Some(Ok(resp)) => {
                LoadState::Ready(strip_for(resp.pagination.total, resp.histogram.as_ref()))
            }
        })
    });
    view! {
        <div
            class="histo"
            // Only an accepted, empty result collapses the strip; a
            // pending, failed or never-run search keeps its band, and a
            // page past the end of a non-empty result keeps its bars.
            class:histo-empty=move || state.with(|s| matches!(s, LoadState::Ready(Strip::Collapsed)))
            class:histo-kept=move || {
                state.with(|s| {
                    matches!(s, LoadState::Ready(Strip::Drawn(Drawn { kept_some: Some(_), .. })))
                })
            }
            node_ref=strip
        >
            <Loaded
                state=state
                // Deliberate quiet-error override: the results table
                // already reports the query failure.
                error=Box::new(|_| view! { <div class="histo-hint">"—"</div> }.into_any())
                render=Box::new(move |view: Strip| match view {
                    Strip::Collapsed => ().into_any(),
                    Strip::Dash => view! { <div class="histo-hint">"—"</div> }.into_any(),
                    Strip::Reason(copy) => view! { <div class="histo-hint">{copy}</div> }.into_any(),
                    Strip::Drawn(drawn) => bars(drawn, strip, axis_width).into_any(),
                })
            />
        </div>
    }
}

/// The key, the scale, the bars and the axis of a drawn strip.
fn bars(
    drawn: Drawn,
    strip: NodeRef<leptos::html::Div>,
    axis_width: RwSignal<Option<f64>>,
) -> impl IntoView {
    let Drawn {
        series,
        caption,
        severity,
        kept_some,
    } = drawn;
    let top = scale_top(series.tallest());
    let axis_series = series.clone();
    // New buckets or labels replace every bar, and the strip keeps its
    // size, so the observer stays quiet. Place the new tips and measure
    // the new axis once they are laid out. Nothing cancels the frame, and
    // switching tab or leaving the page can dispose the strip before it
    // runs; a disposed ref reads as `None` and the frame does nothing.
    request_animation_frame(move || {
        if let Some(strip) = strip.try_get_untracked().flatten() {
            place_all_tips(&strip);
            measure_axis(&strip, axis_width);
        }
    });
    let key = if severity {
        view! {
            <span class="histo-swatch">
                <span class="ok" aria-hidden="true"></span>
                {OTHER_KEY}
            </span>
            <span class="histo-swatch">
                <span class="err" aria-hidden="true"></span>
                {ERROR_KEY}
            </span>
        }
        .into_any()
    } else {
        view! {
            <span class="histo-swatch">
                <span class="ok" aria-hidden="true"></span>
                {NO_SEVERITY_KEY}
            </span>
        }
        .into_any()
    };
    let bars = (0..series.buckets.len())
        .map(|i| {
            let b = series.buckets[i];
            let ok_h = pct(b.ok, top);
            let err_h = pct(b.err, top);
            let tip = series.tip(i);
            let label = tip.clone();
            view! {
                <div
                    class="bar"
                    tabindex="0"
                    role="img"
                    aria-label=label
                    on:mouseenter=move |ev| place_tip(&ev)
                    on:focus=move |ev| place_tip(&ev)
                >
                    <div class="tip" aria-hidden="true">{tip}</div>
                    <div class="ok" style=format!("height:{ok_h:.1}%")></div>
                    <div class="err" style=format!("height:{err_h:.1}%")></div>
                </div>
            }
        })
        .collect::<Vec<_>>();
    view! {
        <div class="histo-key">
            <span class="histo-scope">{caption}</span>
            {key}
        </div>
        {kept_some.map(|line| view! { <div class="histo-note">{line}</div> })}
        <div class="yax">
            <span class="y-top">{top}</span>
            {(top >= 2).then(|| view! { <span class="y-mid">{top / 2}</span> })}
            <span class="y-zero">"0"</span>
        </div>
        <div class="bars">{bars}</div>
        <div class="xax">
            {move || axis_width.get().map(|width| axis_ticks(&axis_series, width))}
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

/// The x axis's tick labels for an axis `width` pixels wide, each at its
/// tick's position.
fn axis_ticks(series: &Series, width: f64) -> impl IntoView + use<> {
    clock_ticks(series, width)
        .into_iter()
        .map(|tick| {
            let class = match tick.align {
                TickAlign::Center => "tick",
                TickAlign::Start => "tick tick-start",
                TickAlign::End => "tick tick-end",
            };
            let left = format!("left:{:.4}%", tick.fraction * 100.0);
            view! { <span class=class style=left>{tick.label}</span> }
        })
        .collect_view()
}

/// Read the x axis's width into `axis_width`, when it is laid out and
/// differs from the last reading. A disposed strip has no signal left to
/// set, and the write is skipped.
fn measure_axis(strip: &web_sys::Element, axis_width: RwSignal<Option<f64>>) {
    let Some(axis) = strip.query_selector(".xax").ok().flatten() else {
        return;
    };
    let width = f64::from(axis.client_width());
    if width > 0.0 && axis_width.try_get_untracked() != Some(Some(width)) {
        let _ = axis_width.try_set(Some(width));
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
    // Bound the tip to the strip first, so a label longer than the strip
    // wraps instead of overflowing. Reading `offset_width` after setting
    // it forces layout, so the width measured below is the wrapped one.
    let room = strip.client_width() - 2 * TIP_EDGE;
    let style = tip.style();
    let _ = style.set_property("--tip-max", &format!("{room}px"));
    let shift = tip_shift(
        bar_left - TIP_EDGE,
        bar.offset_width(),
        tip.offset_width(),
        room,
    );
    let _ = style.set_property("--tip-shift", &format!("{shift}px"));
}
