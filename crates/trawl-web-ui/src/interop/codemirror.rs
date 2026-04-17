// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Thin `wasm-bindgen` wrapper over the vendored `CodeMirror` 6 bundle.
//!
//! The JS side (crates/trawl-web-ui/vendor/src/codemirror.ts) exposes a
//! single factory, `createEditor(parent, initial, opts)`, returning a
//! handle with `destroy()` / `setDoc()`. Everything DSL-specific —
//! diagnostics, autocomplete candidates — is fed through the `opts`
//! callbacks so trawl-core remains the single source of truth for DSL
//! semantics.

use wasm_bindgen::prelude::*;

#[wasm_bindgen(module = "/vendor/codemirror.js")]
extern "C" {
    pub type EditorHandle;

    #[wasm_bindgen(js_name = createEditor)]
    pub fn create_editor(
        parent: &web_sys::HtmlElement,
        initial: &str,
        opts: JsValue,
    ) -> EditorHandle;

    #[wasm_bindgen(method)]
    pub fn destroy(this: &EditorHandle);

    #[wasm_bindgen(method, js_name = setDoc)]
    pub fn set_doc(this: &EditorHandle, text: &str);
}
