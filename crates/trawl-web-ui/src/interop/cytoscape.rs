// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Thin `wasm-bindgen` wrapper over the vendored cytoscape.js bundle.
//!
//! The JS side (`crates/trawl-web-ui/vendor/src/cytoscape.ts`) exposes
//! `createGraph(container, elements, opts)` returning a handle with
//! `setElements()` / `destroy()` / `resize()` / `fit()` /
//! `onNodeClick()`. Elements are cytoscape `ElementDefinition` objects
//! with `{ group, data, classes }`.

use wasm_bindgen::prelude::*;

#[wasm_bindgen(module = "/vendor/cytoscape.js")]
extern "C" {
    pub type GraphHandle;

    #[wasm_bindgen(js_name = createGraph)]
    pub fn create_graph(
        container: &web_sys::HtmlElement,
        elements: JsValue,
        opts: JsValue,
    ) -> GraphHandle;

    #[wasm_bindgen(method, js_name = setElements)]
    pub fn set_elements(this: &GraphHandle, elements: JsValue);

    #[wasm_bindgen(method)]
    pub fn destroy(this: &GraphHandle);

    #[wasm_bindgen(method)]
    pub fn resize(this: &GraphHandle);

    #[wasm_bindgen(method)]
    pub fn fit(this: &GraphHandle);

    #[wasm_bindgen(method, js_name = onNodeClick)]
    pub fn on_node_click(this: &GraphHandle, callback: &JsValue);
}
