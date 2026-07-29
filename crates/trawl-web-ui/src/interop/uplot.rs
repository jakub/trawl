// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Thin `wasm-bindgen` wrapper over the vendored uPlot bundle.
//!
//! The JS side (crates/trawl-web-ui/vendor/src/uplot.ts) exposes
//! `createChart(parent, data, opts)` returning a handle with
//! `setData()` / `destroy()` / `resize()`. Data is the standard uPlot
//! `AlignedData`: `[xs, series1, series2, ...]` where every inner array
//! is the same length and xs are unix timestamps (seconds).

use wasm_bindgen::prelude::*;

/// Rust mirror of `ChartOpts` in `vendor/src/uplot.ts`. Built here rather
/// than at each call site so the two chart surfaces (the search-page
/// snapshot line and the service drawer's ingest bars) can't drift on
/// field names the JS side reads by string key.
pub struct Opts<'a> {
    pub width: f64,
    pub height: f64,
    /// One label per y-series; the x-series is named by the wrapper.
    pub series: &'a [String],
    pub y_label: Option<&'a str>,
    /// Column chart instead of a line.
    pub bars: bool,
    /// Format the time axis as UTC. Set when x values were derived from
    /// trawld's already-timezone-shifted `_time` strings.
    pub utc: bool,
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
        if self.bars {
            set("kind", &JsValue::from_str("bars"));
        }
        if self.utc {
            set("utc", &JsValue::TRUE);
        }
        obj.into()
    }
}

#[wasm_bindgen(module = "/vendor/uplot.js")]
extern "C" {
    pub type ChartHandle;

    /// Mount a chart into `parent`, seeded with `data` and sized/labeled
    /// by `opts`. Returns a handle whose methods mutate the chart in
    /// place.
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
