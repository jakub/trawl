// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<ConfirmModal/>` — reusable confirmation dialog (delete,
//! destructive actions).
//!
//! Ported from trawl-web-ui with two improvements over the original:
//! - `confirm_variant: Variant` replaces the `danger: bool` so the
//!   confirm button's appearance is encoded with the same typed enum
//!   used everywhere else, defaulting to `Variant::Danger`.
//! - Scrim dismissal compares the click target's identity against a
//!   `NodeRef` for the scrim element instead of string-matching the
//!   `class` attribute, so adding sibling classes to the scrim won't
//!   silently break dismissal.
//!
//! The inline `CloseIcon` SVG stays here — the typed `Icon` enum
//! arrives in coastwatch#38.

use leptos::ev;
use leptos::html::Div;
use leptos::prelude::*;
use leptos::web_sys;
use leptos_use::{use_event_listener, use_window};
use wasm_bindgen::JsCast;

use crate::button::{Btn, Variant};

#[component]
pub fn ConfirmModal(
    title: &'static str,
    message: String,
    confirm_label: &'static str,
    #[prop(default = Variant::Danger)] confirm_variant: Variant,
    on_confirm: Callback<()>,
    on_cancel: Callback<()>,
) -> impl IntoView {
    let scrim_ref = NodeRef::<Div>::new();

    // Window-level Escape: bound to `window` rather than the scrim div so it
    // fires regardless of focus. The previous scrim-bound `on:keydown` only
    // dispatched when the scrim itself had focus, which it never does on
    // open — meaning Esc was a no-op until the user clicked the scrim.
    // use_event_listener registers an on_cleanup hook internally, so the
    // listener disposes when the component unmounts; the returned cleanup
    // handle is discarded intentionally.
    let _ = use_event_listener(use_window(), ev::keydown, move |e| {
        if e.key() == "Escape" {
            e.prevent_default();
            on_cancel.run(());
        }
    });

    let on_scrim_mousedown = move |e: web_sys::MouseEvent| {
        // Identity comparison: only dismiss when the click landed on the
        // scrim element itself, not on a descendant. Robust against future
        // class additions that would have broken the previous
        // `class_name().contains("modal-scrim")` heuristic.
        let Some(scrim) = scrim_ref.get() else {
            return;
        };
        let Some(target) = e.target() else { return };
        let Some(el) = target.dyn_ref::<web_sys::Element>() else {
            return;
        };
        if el.is_same_node(Some(scrim.as_ref())) {
            on_cancel.run(());
        }
    };

    view! {
        <div
            class="modal-scrim"
            node_ref=scrim_ref
            on:mousedown=on_scrim_mousedown
        >
            <div class="modal modal-sm" role="alertdialog" aria-modal="true">
                <div class="m-hd">
                    <span class="t">{title}</span>
                    <span class="x" title="Close (Esc)" on:click=move |_| on_cancel.run(())>
                        <CloseIcon/>
                    </span>
                </div>

                <div class="m-body">
                    <p style="margin:0; font-size:13px; color:var(--ink-2)">{message}</p>
                </div>

                <div class="m-ft">
                    <div></div>
                    <Btn variant=Variant::Secondary on_click=on_cancel>
                        "Cancel"
                    </Btn>
                    <Btn variant=confirm_variant on_click=on_confirm>
                        {confirm_label}
                    </Btn>
                </div>
            </div>
        </div>
    }
}

#[component]
fn CloseIcon() -> impl IntoView {
    view! {
        <svg width="12" height="12" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5">
            <path d="m4 4 8 8M12 4l-8 8"/>
        </svg>
    }
}
