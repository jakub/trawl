// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Atmosphere/>` — the WebGL mesh-gradient backdrop
//! (jakub/coastwatch#308).
//!
//! Lifecycle follows trawl-web-ui's `components/chart.rs` idiom:
//! `NodeRef` div + `StoredValue<Option<ShaderHandle>>` + one `Effect`
//! that constructs on first run and pushes uniform updates thereafter,
//! with `on_cleanup` disposing the mount. Theme flips re-color the
//! SAME mount via `setUniforms` — never a remount.
//!
//! Degradation is silent by design:
//! - WebGL2 unavailable → `create_shader` returns `None`; a `failed`
//!   latch stops further attempts and the `.atmosphere` CSS floor
//!   (`background: var(--bg)`, theme-reactive) is all that paints.
//! - `prefers-reduced-motion: reduce` → speed 0; the vendored package
//!   stops its rAF loop entirely at speed 0, so a static frame costs
//!   nothing per frame.
//! - a LOST WebGL context is a permanent static floor BY DESIGN: the
//!   vendored wrapper hides the dead canvas so the `.atmosphere` CSS
//!   floor shows through, and there is deliberately no
//!   `webglcontextrestored` remount — for a decorative backdrop, a
//!   recovery dance is more machinery than the pixels are worth
//!   (ADR-0012 consequences; not a bug to file later).
//!
//! The host `<div class="atmosphere">` is `aria-hidden` decoration —
//! fixed, full-viewport, `z-index: -1`, `pointer-events: none` (see
//! the `.atmosphere` section of `fleet-ui.css`). Consumers compose
//! content ABOVE it, e.g. coastwatch's login card.
//!
//! Mounting this costs a consumer nothing but the component: the
//! vendored JS rides along as a compile-time wasm-bindgen snippet —
//! see the module doc of [`super::interop`].

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
    // Latched on a failed construction (WebGL2 unavailable): the
    // capability does not come back, so theme flips must not retry.
    let failed = StoredValue::new(false);
    let reduced_motion = use_media_query("(prefers-reduced-motion: reduce)");

    // Mount on first run; re-color in place when the theme changes.
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
                // Untracked: the reduced-motion Effect below owns
                // reactivity on the media query — tracking it here
                // too would be redundant.
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

    // prefers-reduced-motion, live: 0 stops the rAF loop entirely.
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
