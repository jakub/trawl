// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Thin `wasm-bindgen` wrapper over the vendored @paper-design/shaders
//! bundle (same shape as trawl-web-ui's `interop/uplot.rs`).
//!
//! The JS side (`crates/fleet-ui/vendor/src/paper-shaders.ts`) exposes
//! `createShader(parent, {shader, colors, uniforms, speed})` returning
//! a handle with `setUniforms()` / `setSpeed()` / `dispose()` — or
//! `null` when WebGL2 is unavailable, which wasm-bindgen surfaces as
//! `None` here. Colors are plain `#rrggbb` strings; the wrapper does
//! the vec4 conversion.
//!
//! CONSUMER CONTRACT (ADR-0012): `module = "/vendor/paper-shaders.js"`
//! resolves at RUNTIME against the consumer's dist root. Every app
//! that mounts [`crate::Atmosphere`] must copy the bundle into place
//! via a Trunk directive in its own `index.html` — in-repo:
//!
//! ```html
//! <link data-trunk rel="copy-file" href="../fleet-ui/vendor/paper-shaders.js" data-target-path="vendor"/>
//! ```
//!
//! (coastwatch uses the same line with its cross-repo relative path,
//! the `fleet-ui.css` model). A missing directive is a runtime
//! module-load failure with no compile-time signal.

use wasm_bindgen::JsValue;
use wasm_bindgen::prelude::*;

use super::palette;
use crate::theme::Theme;

/// Options bag for [`create_shader`]. Built here rather than at call
/// sites so the field names the JS side reads by string key have one
/// Rust home.
#[derive(Debug)]
pub struct ShaderOpts {
    pub theme: Theme,
    pub speed: f64,
}

impl ShaderOpts {
    #[must_use]
    pub fn to_js(&self) -> JsValue {
        let obj = js_sys::Object::new();
        let set = |k: &str, v: &JsValue| {
            let _ = js_sys::Reflect::set(&obj, &k.into(), v);
        };
        set("shader", &JsValue::from_str(palette::SHADER));
        set("colors", &colors_array(self.theme).into());
        set("uniforms", &knob_uniforms());
        set("speed", &JsValue::from_f64(self.speed));
        obj.into()
    }
}

/// The `{colors}` update bag for [`set_uniforms`] — re-colors the
/// mounted mesh in place on theme flips (never a remount).
#[must_use]
pub fn theme_update(theme: Theme) -> JsValue {
    let obj = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&obj, &"colors".into(), &colors_array(theme).into());
    obj.into()
}

fn colors_array(theme: Theme) -> js_sys::Array {
    let arr = js_sys::Array::new();
    for stop in palette::colors(theme) {
        arr.push(&JsValue::from_str(stop));
    }
    arr
}

/// The mesh-gradient texture knobs, all sourced from the single knobs
/// site in [`palette`].
fn knob_uniforms() -> JsValue {
    let obj = js_sys::Object::new();
    let set = |k: &str, v: f64| {
        let _ = js_sys::Reflect::set(&obj, &k.into(), &JsValue::from_f64(v));
    };
    set("u_distortion", palette::DISTORTION);
    set("u_swirl", palette::SWIRL);
    set("u_grainMixer", palette::GRAIN_MIXER);
    set("u_grainOverlay", palette::GRAIN_OVERLAY);
    obj.into()
}

#[wasm_bindgen(module = "/vendor/paper-shaders.js")]
extern "C" {
    pub type ShaderHandle;

    /// Mount a shader into `parent`; `None` means WebGL2 is
    /// unavailable — keep the CSS `var(--bg)` floor and never retry.
    #[wasm_bindgen(js_name = createShader)]
    pub fn create_shader(parent: &web_sys::HtmlElement, opts: JsValue) -> Option<ShaderHandle>;

    /// Push a partial `{colors, uniforms}` update into the mounted
    /// shader in place.
    #[wasm_bindgen(method, js_name = setUniforms)]
    pub fn set_uniforms(this: &ShaderHandle, update: JsValue);

    /// Set the animation speed; 0 stops the rAF loop entirely.
    #[wasm_bindgen(method, js_name = setSpeed)]
    pub fn set_speed(this: &ShaderHandle, speed: f64);

    /// Tear down the mount and remove its canvas from the DOM.
    #[wasm_bindgen(method)]
    pub fn dispose(this: &ShaderHandle);
}
