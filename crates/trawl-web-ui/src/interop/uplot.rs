// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Data is the standard uPlot `AlignedData`: `[xs, series1, series2, ...]`
//! where every inner array is the same length and xs are unix timestamps
//! (seconds).

use wasm_bindgen::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChartKind {
    Line,
    Column,
    /// Horizontal bars: Column rotated, categories down the left edge.
    Bar,
}

impl ChartKind {
    fn as_js(self) -> &'static str {
        match self {
            Self::Line => "line",
            Self::Column => "column",
            Self::Bar => "bar",
        }
    }
}

/// Rust mirror of `ChartOpts` in `vendor/src/uplot.ts`; the JS side reads
/// these field names by string key.
pub struct Opts<'a> {
    pub width: f64,
    pub height: f64,
    /// One label per y-series; the x-series is named by the wrapper.
    pub series: &'a [String],
    pub y_label: Option<&'a str>,
    pub kind: ChartKind,
    /// Format the time axis as UTC. Set when x values were derived from
    /// trawld's `_time` strings, which are UTC.
    pub utc: bool,
    /// `Some` switches the x scale to ordinal: one label per category and
    /// xs are `0..n-1`. `None` keeps a time x scale.
    pub x_labels: Option<&'a [String]>,
    pub span_gaps: bool,
    /// Let the bridge install its own `ResizeObserver` on the host. Set
    /// only by a caller that never calls [`ChartHandle::resize`]: one
    /// owner per redraw, or a layout change draws the chart twice.
    pub observe_resize: bool,
}

impl Opts<'_> {
    #[must_use]
    pub fn to_js(&self) -> JsValue {
        let obj = js_sys::Object::new();
        let set = |k: &str, v: &JsValue| {
            let _ = js_sys::Reflect::set(&obj, &k.into(), v);
        };
        set("width", &JsValue::from_f64(self.width));
        set("height", &JsValue::from_f64(self.height));

        let series = js_sys::Array::new();
        for label in self.series {
            series.push(&JsValue::from_str(label));
        }
        set("series", &series.into());

        if let Some(label) = self.y_label {
            set("yLabel", &JsValue::from_str(label));
        }
        set("kind", &JsValue::from_str(self.kind.as_js()));
        if self.utc {
            set("utc", &JsValue::TRUE);
        }
        if let Some(labels) = self.x_labels {
            let x_labels = js_sys::Array::new();
            for label in labels {
                x_labels.push(&JsValue::from_str(label));
            }
            set("xLabels", &x_labels.into());
        }
        set("spanGaps", &JsValue::from_bool(self.span_gaps));
        set("observeResize", &JsValue::from_bool(self.observe_resize));
        obj.into()
    }
}

#[wasm_bindgen(module = "/vendor/uplot.js")]
extern "C" {
    pub type ChartHandle;

    #[wasm_bindgen(js_name = createChart)]
    pub fn create_chart(parent: &web_sys::HtmlElement, data: JsValue, opts: JsValue)
    -> ChartHandle;

    #[wasm_bindgen(method, js_name = setData)]
    pub fn set_data(this: &ChartHandle, data: JsValue);

    #[wasm_bindgen(method)]
    pub fn destroy(this: &ChartHandle);

    #[wasm_bindgen(method)]
    pub fn resize(this: &ChartHandle, width: f64, height: f64);
}
