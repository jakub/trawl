// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Modal/>` — the dialog primitive every fleet modal composes.
//!
//! Owns the machinery every dialog needs:
//!
//! - **Scrim + panel frame** — `modal-scrim > modal[.modal-sm]` with the
//!   `m-hd` (icon chip / title / close) header, `m-body` (children) and
//!   `m-ft` (footer slot) regions. All classes ship in fleet-ui.css.
//! - **Outside-click dismissal** — mousedown target compared against a
//!   `NodeRef` for the scrim element (identity, not class-name
//!   string-matching), so clicks and drag-selections inside the panel
//!   never dismiss.
//! - **Window-level Escape** — bound to `window` rather than the scrim
//!   so it fires regardless of focus; a scrim-bound `on:keydown` only
//!   dispatches once focus is inside the dialog subtree, which leaves
//!   Esc dead on a freshly-opened modal. Gated on
//!   [`overlay`](crate::overlay) topmost-layer arbitration so a modal
//!   opened over a live drawer takes Escape without the drawer also
//!   closing.
//! - **Window-level Cmd/Ctrl+Enter** — opt-in via `on_submit`, for
//!   dialogs with a primary action (export, save-as-net).
//!
//! What stays with the caller: the footer buttons (pass `<Btn>`s via
//! `footer`), body content, and any autofocus/validation logic.

use leptos::ev;
use leptos::html::Div;
use leptos::prelude::*;
use leptos::web_sys;
use leptos_use::{use_event_listener, use_window};
use wasm_bindgen::JsCast;

use crate::icon::{Icon, IconView};

/// Dialog primitive. `children` fills `m-body`; `footer` fills `m-ft`
/// (pass `Box::new(|| view! { … }.into_any())`, same idiom as
/// [`Shell`](crate::Shell)'s footer slot).
///
/// `role` should be `"dialog"` (default) or `"alertdialog"` for
/// confirm-style interruptions. `narrow` adds `modal-sm` (380px vs
/// 460px). `icon` renders the accent header chip (`m-hd .ic`).
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

    // Window-level keys: Escape cancels; Cmd/Ctrl+Enter submits when the
    // dialog has a primary action. use_event_listener registers an
    // on_cleanup hook internally, so the listener disposes when the
    // component unmounts; the returned cleanup handle is discarded
    // intentionally. The topmost-layer guard (see crate::overlay) keeps
    // these keys from also firing on a drawer stacked beneath this modal.
    //
    // FocusPolicy::Trap earns the aria-modal="true" below: initial focus
    // lands inside the panel, Tab/Shift+Tab cycle within it, and focus
    // restores to the opener on close.
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
        // Identity comparison: only dismiss when the click landed on the
        // scrim element itself, not on a descendant. Robust against
        // future class additions that would break a
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
                    // A named native button: the glyph carries no text,
                    // so aria-label is the whole accessible name, and
                    // Enter/Space have to reach the cancel callback the
                    // way Escape already does.
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
