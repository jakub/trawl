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
