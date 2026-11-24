// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Drawer/>` — detail inspector over the `sd-*` classes, in either of
//! two presentations.
//!
//! **Escape** is bound at window level so it fires regardless of focus;
//! a scrim-bound `on:keydown` only dispatches once focus has entered the
//! drawer subtree, which leaves Esc dead on a freshly-opened drawer.

use leptos::ev;
use leptos::html::Div;
use leptos::prelude::*;
use leptos::web_sys;
use leptos_use::{use_event_listener, use_window};
use wasm_bindgen::JsCast;

use crate::icon::{Icon, IconView};
use crate::overlay::{
    FocusPolicy, OverlayLayer, focus_initial, has_layers, push_overlay_beneath, push_overlay_with,
};
use crate::tabs::{TabItem, Tabs, TabsStyle};

#[component]
pub fn Drawer(
    tabs: Vec<TabItem>,
    #[prop(into)] tabs_label: String,
    /// Required, and the only source of the name: the title slot is
    /// visual and never names the dialog (ADR-0028).
    #[prop(into)]
    label: Signal<String>,
    #[prop(into)] active_tab: Signal<String>,
    on_tab_change: Callback<String>,
    on_close: Callback<()>,
    #[prop(optional, into)] on_escape: Option<Callback<()>>,
    #[prop(into, optional)] meta: MaybeProp<String>,
    #[prop(into, optional)] docked: Signal<bool>,
    #[prop(optional)] panel_id: Option<&'static str>,
    #[prop(optional)] panel_class: Option<&'static str>,
    #[prop(default = 14)] close_size: u16,
    title: Children,
    #[prop(optional)] actions: Option<Children>,
    children: Children,
) -> impl IntoView {
    let scrim_ref = NodeRef::<Div>::new();
    let panel_ref = NodeRef::<Div>::new();

    let layer = overlay_presentation(docked, panel_ref);

    let escape = on_escape.unwrap_or(on_close);
    let _ = use_event_listener(use_window(), ev::keydown, move |e| {
        if e.key() != "Escape" {
            return;
        }
        let mine = if docked.get() {
            !has_layers()
                && panel_ref
                    .get()
                    .is_some_and(|panel| panel.contains(document().active_element().as_deref()))
        } else {
            layer.get_value().is_some_and(OverlayLayer::is_topmost)
        };
        if mine {
            e.prevent_default();
            escape.run(());
        }
    });

    let on_scrim_mousedown = move |e: web_sys::MouseEvent| {
        if docked.get() {
            return;
        }
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
            class:sd-scrim=move || !docked.get()
            class:sd-host=move || docked.get()
            tabindex="-1"
            node_ref=scrim_ref
            on:mousedown=on_scrim_mousedown
        >
            <div class="sd-drawer"
                class:sd-docked=move || docked.get()
                // An optional app class, added beside `sd-drawer`
                // through the class list; a second `class=` attribute
                // would overwrite it instead.
                class=(panel_class.unwrap_or_default(), panel_class.is_some())
                id=panel_id
                role="dialog"
                aria-label=move || label.get()
                tabindex="-1"
                node_ref=panel_ref
            >
                <div class="sd-hd">
                    <div class="sd-ttl">{title()}</div>
                    <div class="sd-actions">
                        {actions.map(|a| a())}
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

/// The layer is owned here rather than by
/// [`use_overlay_layer_with`](crate::overlay::use_overlay_layer_with)
/// because `docked` flips at runtime, while a layer that hook registers
/// lives exactly as long as the component.
fn overlay_presentation(
    docked: Signal<bool>,
    panel_ref: NodeRef<Div>,
) -> StoredValue<Option<OverlayLayer>> {
    let layer = StoredValue::new(None::<OverlayLayer>);
    let opener = StoredValue::new(None::<web_sys::Element>);
    let focus_done = StoredValue::new(false);

    let acquire = move |at_mount: bool| {
        if layer.get_value().is_some() {
            return;
        }
        // At mount the drawer is the newest overlay and goes on top,
        // where `use_overlay_layer_with` put it. Undocking later goes in
        // beneath everything stacked since, so an open modal keeps the
        // page's focus and Escape.
        layer.set_value(Some(if at_mount {
            push_overlay_with(FocusPolicy::Capture)
        } else {
            push_overlay_beneath(FocusPolicy::Capture)
        }));
    };
    let release = move || {
        if let Some(live) = layer.get_value() {
            live.release();
            layer.set_value(None);
        }
    };

    // The opener is recorded for BOTH presentations. A docked panel
    // holds no layer, so the layer's own restore never runs for it.
    opener.set_value(document().active_element());

    // An overlay drawer registers synchronously at mount: the stack is
    // LIFO, so its order has to match mount order, and `Shell` reads
    // `has_layers()` to leave the palette chord inert from the first
    // keystroke on.
    if !docked.get_untracked() {
        acquire(true);
    }

    // Reading the panel NodeRef subscribes the effect to it as well as to
    // `docked`, so initial focus lands as soon as the panel exists; the
    // `focus_done` latch keeps a later re-run from stealing focus back,
    // and a presentation flip never resets it.
    Effect::new(move |_| {
        let panel = panel_ref.get();
        if docked.get() {
            release();
            // A panel that mounts docked takes no initial focus, and an
            // undock later is a resize, not an open.
            focus_done.set_value(true);
            return;
        }
        acquire(false);
        if !focus_done.get_value()
            && let Some(el) = panel
            && layer.get_value().is_some_and(OverlayLayer::owns_focus)
        {
            focus_initial(&web_sys::Element::from(el));
            focus_done.set_value(true);
        }
    });

    let restore_opener = move || {
        if let Some(back) = opener
            .get_value()
            .and_then(|el| el.dyn_into::<web_sys::HtmlElement>().ok())
            && back.is_connected()
        {
            let _ = back.focus();
        }
    };

    on_cleanup(move || {
        // Ownership must be read before release: a drawer unmounting
        // beneath a live modal doesn't own focus and must not restore.
        let Some(live) = layer.get_value() else {
            // Docked. Restore only if focus is still inside the closing
            // panel or has fallen to <body>, where leaving it alone loses
            // the keyboard. Anywhere else, focus was moved deliberately.
            let active = document().active_element();
            let inside = panel_ref
                .get_untracked()
                .is_some_and(|panel| panel.contains(active.as_deref()));
            let orphaned = active
                .as_ref()
                .is_none_or(|el| el.tag_name().eq_ignore_ascii_case("body"));
            if inside || orphaned {
                restore_opener();
            }
            return;
        };
        let owned = live.owns_focus();
        live.release();
        layer.set_value(None);
        if owned {
            restore_opener();
        }
    });

    layer
}
