// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Drawer/>` — right-slide detail inspector over the `sd-*` classes.
//!
//! Owns the shell: scrim (outside-click dismissal via `NodeRef`
//! identity), the sliding dialog panel, the header row (title slot +
//! actions slot + close affordance), the tab strip
//! ([`Tabs`](crate::tabs::Tabs) in its `Drawer` style), and the
//! scrolling body. Pane content, tab-switching state, and the title's
//! inner markup stay app-owned.
//!
//! **Escape** is bound at window level so it fires regardless of
//! focus; a scrim-bound `on:keydown` only dispatches once focus has
//! entered the drawer subtree, which leaves Esc dead on a
//! freshly-opened drawer. It is gated on
//! [`overlay`](crate::overlay) topmost-layer arbitration so a drawer
//! sitting beneath an open modal ignores Escape (the modal owns it).
//! `on_escape` overrides the default close behaviour for drawers that
//! need a pre-close step, such as cancelling an in-flight inline
//! rename.

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
/// the tab strip (service drawer's event/size/field summary), outside
/// the tablist.
#[component]
pub fn Drawer(
    tabs: Vec<TabItem>,
    /// Names the drawer's tab strip for assistive technology
    /// ("Service details", "Saved query details"). Required, and
    /// forwarded verbatim to [`Tabs`]: the drawer is the only thing
    /// that knows what its strip is a list of.
    #[prop(into)]
    tabs_label: String,
    #[prop(into)] active_tab: Signal<String>,
    on_tab_change: Callback<String>,
    on_close: Callback<()>,
    #[prop(optional, into)] on_escape: Option<Callback<()>>,
    #[prop(into, optional)] meta: MaybeProp<String>,
    /// Close-glyph size in px. A prop because the sizes in use differ
    /// (trawl's drawers pass 12, the default is 14) and the 2px delta
    /// is pixel-visible in the stroke tips; picking one size for the
    /// fleet is deliberate visual work, not a default to flip.
    #[prop(default = 14)]
    close_size: u16,
    title: Children,
    #[prop(optional)] actions: Option<Children>,
    children: Children,
) -> impl IntoView {
    thread_local! { static NEXT_TITLE_ID: std::cell::Cell<u64> = const { std::cell::Cell::new(0) }; }
    let title_id = NEXT_TITLE_ID.with(|next| {
        let id = next.get();
        next.set(id + 1);
        format!("fleet-drawer-title-{id}")
    });
    let scrim_ref = NodeRef::<Div>::new();
    let panel_ref = NodeRef::<Div>::new();

    // Window-level Escape (see module docs). use_event_listener
    // registers an on_cleanup hook internally; the returned handle is
    // discarded intentionally. The topmost-layer guard (see
    // crate::overlay) makes a background drawer ignore Escape while a
    // modal is stacked over it, so one Escape doesn't close both.
    //
    // FocusPolicy::Capture: the drawer takes initial focus on open and
    // restores the opener on close, but never Tab-traps — it is
    // non-modal by design (the background stays interactive, and a
    // modal may stack over a live drawer), which is also why the panel
    // below renders role="dialog" without aria-modal.
    let layer =
        crate::overlay::use_overlay_layer_with(crate::overlay::FocusPolicy::Capture, move || {
            panel_ref.get().map(web_sys::Element::from)
        });
    let escape = on_escape.unwrap_or(on_close);
    let _ = use_event_listener(use_window(), ev::keydown, move |e| {
        if e.key() == "Escape" && layer.is_topmost() {
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
            <div class="sd-drawer" role="dialog" aria-labelledby=title_id.clone() tabindex="-1" node_ref=panel_ref>
                <div class="sd-hd">
                    <div class="sd-ttl" id=title_id.clone()>{title()}</div>
                    <div class="sd-actions">
                        {actions.map(|a| a())}
                        // A native button, not a styled span: the close
                        // affordance has to be reachable by Tab and
                        // operable by Enter/Space. `.sd-x` already
                        // declares the full button reset (background,
                        // border, cursor, line-height, inline-flex
                        // centring) and the sheet's global
                        // `button { font: inherit }` covers the rest, so
                        // the element swap needs no CSS change.
                        <button
                            type="button"
                            class="sd-x"
                            aria-label="Close"
                            title="Close (Esc)"
                            on:click=move |_| on_close.run(())
                        >
                            <IconView icon=Icon::Close size=close_size stroke_width=1.5/>
                        </button>
                    </div>
                </div>

                <Tabs
                    style=TabsStyle::Drawer
                    items=tabs
                    label=tabs_label
                    active=active_tab
                    on_change=on_tab_change
                    meta=meta
                />

                <div class="sd-body">{children()}</div>
            </div>
        </div>
    }
}
