// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<ConfirmModal/>` — reusable confirmation dialog (delete,
//! destructive actions).
//!
//! Ported from trawl-web-ui with one substitution: the inline
//! `class="btn-danger"` / `"btn-pri"` ternary is replaced by the typed
//! `<Btn variant=Variant::Danger | Primary>` component shipped in this
//! crate. The cancel button likewise switches to `<Btn variant=Variant::Secondary>`.
//!
//! The inline `CloseIcon` SVG stays here — the typed `Icon` enum
//! arrives in coastwatch#38.

use leptos::prelude::*;
use leptos::web_sys;
use wasm_bindgen::JsCast;

use crate::button::{Btn, Variant};

#[component]
pub fn ConfirmModal(
    title: &'static str,
    message: String,
    confirm_label: &'static str,
    #[prop(default = true)] danger: bool,
    on_confirm: Callback<()>,
    on_cancel: Callback<()>,
) -> impl IntoView {
    let cancel = move || on_cancel.run(());
    let cancel_key = cancel;
    let cancel_scrim = cancel;
    let cancel_btn = cancel;

    let on_keydown = move |e: web_sys::KeyboardEvent| {
        if e.key() == "Escape" {
            e.prevent_default();
            cancel_key();
        }
    };

    let confirm_variant = if danger {
        Variant::Danger
    } else {
        Variant::Primary
    };

    view! {
        <div
            class="modal-scrim"
            on:mousedown=move |e: web_sys::MouseEvent| {
                if let Some(target) = e.target()
                    && let Some(el) = target.dyn_ref::<web_sys::Element>()
                    && el.class_name().contains("modal-scrim")
                {
                    cancel_scrim();
                }
            }
            on:keydown=on_keydown
        >
            <div class="modal modal-sm" role="alertdialog" aria-modal="true">
                <div class="m-hd">
                    <span class="t">{title}</span>
                    <span class="x" title="Close (Esc)" on:click=move |_| cancel_btn()>
                        <CloseIcon/>
                    </span>
                </div>

                <div class="m-body">
                    <p style="margin:0; font-size:13px; color:var(--ink-2)">{message}</p>
                </div>

                <div class="m-ft">
                    <div></div>
                    <Btn
                        variant=Variant::Secondary
                        on_click=Callback::new(move |()| on_cancel.run(()))
                    >
                        "Cancel"
                    </Btn>
                    <Btn
                        variant=confirm_variant
                        on_click=Callback::new(move |()| on_confirm.run(()))
                    >
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
