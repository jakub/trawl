// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Atmosphere/>` — the WebGL mesh-gradient backdrop.

use leptos::html::Div;
use leptos::prelude::*;
use leptos_use::use_media_query;
use wasm_bindgen::JsCast;

use super::interop::{ShaderHandle, ShaderOpts, create_shader, theme_update};
use super::palette;
use crate::theme::Theme;

#[component]
pub fn Atmosphere(#[prop(into)] theme: Signal<Theme>) -> impl IntoView {
    let node_ref = NodeRef::<Div>::new();
    let handle: StoredValue<Option<ShaderHandle>, LocalStorage> = StoredValue::new_local(None);
    let failed = StoredValue::new(false);
    let reduced_motion = use_media_query("(prefers-reduced-motion: reduce)");

    Effect::new(move |_| {
        let Some(element) = node_ref.get() else {
            return;
        };
        let theme_now = theme.get();
        if failed.get_value() {
            return;
        }
        handle.update_value(|slot| {
            if let Some(h) = slot.as_ref() {
                h.set_uniforms(theme_update(theme_now));
            } else {
                let html_el: web_sys::HtmlElement = (*element).clone().unchecked_into();
                let speed = if reduced_motion.get_untracked() {
                    0.0
                } else {
                    palette::SPEED
                };
                let opts = ShaderOpts {
                    theme: theme_now,
                    speed,
                };
                match create_shader(&html_el, opts.to_js()) {
                    Some(h) => *slot = Some(h),
                    None => failed.set_value(true),
                }
            }
        });
    });

    Effect::new(move |_| {
        let reduced = reduced_motion.get();
        handle.update_value(|slot| {
            if let Some(h) = slot.as_ref() {
                h.set_speed(if reduced { 0.0 } else { palette::SPEED });
            }
        });
    });

    on_cleanup(move || {
        handle.update_value(|slot| {
            if let Some(h) = slot.take() {
                h.dispose();
            }
        });
    });

    view! { <div class="atmosphere" aria-hidden="true" node_ref=node_ref></div> }
}
