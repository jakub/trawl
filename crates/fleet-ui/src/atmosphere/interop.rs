// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Thin `wasm-bindgen` wrapper over the vendored @paper-design/shaders
//! bundle (same shape as trawl-web-ui's `interop/uplot.rs`).

use wasm_bindgen::JsValue;
use wasm_bindgen::prelude::*;

use super::palette;
use crate::theme::Theme;

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

    #[wasm_bindgen(js_name = createShader)]
    pub fn create_shader(parent: &web_sys::HtmlElement, opts: JsValue) -> Option<ShaderHandle>;

    #[wasm_bindgen(method, js_name = setUniforms)]
    pub fn set_uniforms(this: &ShaderHandle, update: JsValue);

    #[wasm_bindgen(method, js_name = setSpeed)]
    pub fn set_speed(this: &ShaderHandle, speed: f64);

    #[wasm_bindgen(method)]
    pub fn dispose(this: &ShaderHandle);
}
