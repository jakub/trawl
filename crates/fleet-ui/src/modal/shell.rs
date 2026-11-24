// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Modal/>` — the dialog primitive every fleet modal composes.
//!
//! Escape is bound to `window` rather than the scrim so it fires regardless
//! of focus; a scrim-bound `on:keydown` only dispatches once focus is inside
//! the dialog subtree, which leaves Esc dead on a freshly-opened modal.

use leptos::ev;
use leptos::html::Div;
use leptos::prelude::*;
use leptos::web_sys;
use leptos_use::{use_event_listener, use_window};
use wasm_bindgen::JsCast;

use crate::icon::{Icon, IconView};

#[component]
pub fn Modal(
    title: &'static str,
    #[prop(optional)] icon: Option<Icon>,
    #[prop(default = "dialog")] role: &'static str,
    #[prop(default = false)] narrow: bool,
    on_cancel: Callback<()>,
    #[prop(optional, into)] on_submit: Option<Callback<()>>,
    footer: Children,
    children: Children,
) -> impl IntoView {
    thread_local! { static NEXT_TITLE_ID: std::cell::Cell<u64> = const { std::cell::Cell::new(0) }; }
    let title_id = NEXT_TITLE_ID.with(|next| {
        let id = next.get();
        next.set(id + 1);
        format!("fleet-modal-title-{id}")
    });
    let scrim_ref = NodeRef::<Div>::new();
    let panel_ref = NodeRef::<Div>::new();

    let layer =
        crate::overlay::use_overlay_layer_with(crate::overlay::FocusPolicy::Trap, move || {
            panel_ref.get().map(web_sys::Element::from)
        });
    let _ = use_event_listener(use_window(), ev::keydown, move |e| {
        if !layer.is_topmost() {
            return;
        }
        match e.key().as_str() {
            "Escape" => {
                e.prevent_default();
                on_cancel.run(());
            }
            "Enter" if e.meta_key() || e.ctrl_key() => {
                if let Some(submit) = on_submit {
                    e.prevent_default();
                    submit.run(());
                }
            }
            _ => {}
        }
    });

    let on_scrim_mousedown = move |e: web_sys::MouseEvent| {
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

    let panel_class = if narrow { "modal modal-sm" } else { "modal" };

    view! {
        <div
            class="modal-scrim"
            node_ref=scrim_ref
            on:mousedown=on_scrim_mousedown
        >
            <div class=panel_class role=role aria-labelledby=title_id.clone() aria-modal="true" tabindex="-1" node_ref=panel_ref>
                <div class="m-hd">
                    {icon.map(|ic| view! {
                        <span class="ic"><IconView icon=ic size=12 stroke_width=1.5/></span>
                    })}
                    <span class="t" id=title_id.clone()>{title}</span>
                    <button
                        type="button"
                        class="x"
                        aria-label="Close dialog"
                        title="Close (Esc)"
                        on:click=move |_| on_cancel.run(())
                    >
                        <IconView icon=Icon::Close size=12 stroke_width=1.5/>
                    </button>
                </div>

                <div class="m-body">{children()}</div>

                <div class="m-ft">{footer()}</div>
            </div>
        </div>
    }
}
