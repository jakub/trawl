// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Drawer/>` — right-slide detail inspector (issue #28), extracted
//! from the `sd-*` convention trawl's service and net drawers
//! copy-pasted.
//!
//! Owns the shell: scrim (outside-click dismissal via `NodeRef`
//! identity), the sliding `aside` panel, the header row (title slot +
//! actions slot + close affordance), the tab strip
//! ([`Tabs`](crate::tabs::Tabs) in its `Drawer` style), and the
//! scrolling body. Pane content, tab-switching state, and the title's
//! inner markup stay app-owned.
//!
//! **Escape** is bound at window level, so it fires regardless of
//! focus (the scrim-bound `on:keydown` the trawl drawers shipped only
//! dispatched once focus entered the drawer subtree — on a
//! freshly-opened drawer Esc was a no-op). `on_escape` overrides the
//! default close behaviour for drawers that need a pre-close step —
//! trawl's net drawer cancels an in-flight inline rename first.

use leptos::ev;
use leptos::html::Div;
use leptos::prelude::*;
use leptos::web_sys;
use leptos_use::{use_event_listener, use_window};
use wasm_bindgen::JsCast;

use crate::icon::{Icon, IconView};
use crate::tabs::{TabItem, Tabs, TabsStyle};

/// Right-slide drawer shell. `title` fills `.sd-ttl`; `actions` fills
/// `.sd-actions` ahead of the built-in close button (pass `<Btn>`s);
/// `children` fills `.sd-body`. Slots use the same boxed-closure idiom
/// as [`Shell`](crate::Shell)'s footer. `meta` renders trailing text in
/// the tab strip (service drawer's event/size/field summary).
#[component]
pub fn Drawer(
    tabs: Vec<TabItem>,
    #[prop(into)] active_tab: Signal<String>,
    on_tab_change: Callback<String>,
    on_close: Callback<()>,
    #[prop(optional, into)] on_escape: Option<Callback<()>>,
    #[prop(into, optional)] meta: Option<String>,
    title: Children,
    #[prop(optional)] actions: Option<Children>,
    children: Children,
) -> impl IntoView {
    let scrim_ref = NodeRef::<Div>::new();

    // Window-level Escape (see module docs). use_event_listener
    // registers an on_cleanup hook internally; the returned handle is
    // discarded intentionally.
    let escape = on_escape.unwrap_or(on_close);
    let _ = use_event_listener(use_window(), ev::keydown, move |e| {
        if e.key() == "Escape" {
            e.prevent_default();
            escape.run(());
        }
    });

    let on_scrim_mousedown = move |e: web_sys::MouseEvent| {
        // Identity comparison, same rationale as the Modal shell.
        let Some(scrim) = scrim_ref.get() else {
            return;
        };
        let Some(target) = e.target() else { return };
        let Some(el) = target.dyn_ref::<web_sys::Element>() else {
            return;
        };
        if el.is_same_node(Some(scrim.as_ref())) {
            on_close.run(());
        }
    };

    view! {
        <div
            class="sd-scrim"
            tabindex="-1"
            node_ref=scrim_ref
            on:mousedown=on_scrim_mousedown
        >
            <aside class="sd-drawer" role="dialog" aria-modal="true">
                <div class="sd-hd">
                    <div class="sd-ttl">{title()}</div>
                    <div class="sd-actions">
                        {actions.map(|a| a())}
                        <span class="sd-x" title="Close (Esc)" on:click=move |_| on_close.run(())>
                            <IconView icon=Icon::Close size=14 stroke_width=1.5/>
                        </span>
                    </div>
                </div>

                <Tabs
                    style=TabsStyle::Drawer
                    items=tabs
                    active=active_tab
                    on_change=on_tab_change
                    meta=meta
                />

                <div class="sd-body">{children()}</div>
            </aside>
        </div>
    }
}
