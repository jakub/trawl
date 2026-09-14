// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Drawer/>` — detail inspector over the `sd-*` classes, in either of
//! two presentations.
//!
//! Owns the shell: the host element (scrim, with outside-click
//! dismissal via `NodeRef` identity), the panel, the header row (title
//! slot + actions slot + close affordance), the tab strip
//! ([`Tabs`](crate::tabs::Tabs) in its `Drawer` style), and the
//! scrolling body. Pane content, tab-switching state, and the title's
//! inner markup stay app-owned.
//!
//! **Presentation.** `docked` picks it. Undocked (the default) is the
//! right-slide overlay: a scrim host, a `Capture`
//! [`overlay`](crate::overlay) layer, initial focus into the panel and
//! restore to the opener on close. Docked is the same header, tabs and
//! body rendered in flow beside its list: the host collapses to
//! `display: contents`, there is no scrim, and no overlay layer is
//! registered (ADR-0032). The children mount once and stay mounted when
//! `docked` flips — only the layer registration and the two host
//! classes change.
//!
//! **Escape** is bound at window level so it fires regardless of focus;
//! a scrim-bound `on:keydown` only dispatches once focus has entered the
//! drawer subtree, which leaves Esc dead on a freshly-opened drawer.
//! Undocked it is gated on topmost-layer arbitration, so a drawer
//! sitting beneath an open modal ignores Escape (the modal owns it).
//! Docked there is no layer to arbitrate with, so focus does it: Escape
//! closes a docked drawer only while focus is inside its panel and no
//! overlay is open at all. `on_escape` overrides the default close
//! behaviour for drawers that need a pre-close step, such as cancelling
//! an in-flight inline rename.

use leptos::ev;
use leptos::html::Div;
use leptos::prelude::*;
use leptos::web_sys;
use leptos_use::{use_event_listener, use_window};
use wasm_bindgen::JsCast;

use crate::icon::{Icon, IconView};
use crate::overlay::{FocusPolicy, OverlayLayer, focus_initial, has_layers, push_overlay_with};
use crate::tabs::{TabItem, Tabs, TabsStyle};

/// Drawer shell. `title` fills `.sd-ttl`; `actions` fills `.sd-actions`
/// ahead of the built-in close button (pass `<Btn>`s); `children` fills
/// `.sd-body`. Slots use the same boxed-closure idiom as
/// [`Shell`](crate::Shell)'s footer. `meta` renders trailing text in the
/// tab strip (service drawer's event/size/field summary), outside the
/// tablist.
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
    /// Render in flow beside the list instead of over a scrim: no
    /// backdrop, no overlay layer, Escape only from inside (ADR-0032).
    /// Defaults to `false`, the overlay presentation.
    #[prop(into, optional)]
    docked: Signal<bool>,
    /// Optional `id` on the panel, so a docked panel can be the target
    /// of an `aria-controls` or an in-page link.
    #[prop(optional)]
    panel_id: Option<&'static str>,
    /// Optional extra class on the panel, for app-owned styling of one
    /// docked instance. Composed beside `sd-drawer`, never instead of it.
    #[prop(optional)]
    panel_class: Option<&'static str>,
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

    let layer = overlay_presentation(docked, panel_ref);

    let escape = on_escape.unwrap_or(on_close);
    let _ = use_event_listener(use_window(), ev::keydown, move |e| {
        if e.key() != "Escape" {
            return;
        }
        let mine = if docked.get() {
            // No layer of its own: a docked panel is page furniture, so
            // it answers Escape only while it holds focus, and only
            // while nothing is stacked over the page (an open modal or
            // menu owns the key).
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
        // Docked, the host paints nothing and dismisses nothing.
        if docked.get() {
            return;
        }
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
            class:sd-scrim=move || !docked.get()
            class:sd-host=move || docked.get()
            tabindex="-1"
            node_ref=scrim_ref
            on:mousedown=on_scrim_mousedown
        >
            // A complementary-landmark element is not an allowed host
            // for role="dialog" (audit finding A05), so the panel is a
            // plain div. The `.sd-drawer` class carries every pixel, so
            // nothing else moves.
            <div class="sd-drawer"
                class:sd-docked=move || docked.get()
                // An optional app class, added beside `sd-drawer`
                // through the class list; a second `class=` attribute
                // would overwrite it instead.
                class=(panel_class.unwrap_or_default(), panel_class.is_some())
                id=panel_id
                role="dialog"
                aria-labelledby=title_id.clone()
                tabindex="-1"
                node_ref=panel_ref
            >
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

/// The overlay registration a drawer holds while it is **not** docked,
/// and the whole of the difference between the two presentations.
///
/// The layer is owned here rather than by
/// [`use_overlay_layer_with`](crate::overlay::use_overlay_layer_with)
/// because `docked` flips at runtime — a panel docks past its breakpoint
/// and goes back over a scrim below it — while a layer that hook
/// registers lives exactly as long as the component. The registration is
/// otherwise the same one: `FocusPolicy::Capture`, so the drawer takes
/// initial focus on open and restores the opener on close but never
/// Tab-traps. It is non-modal by design (the background stays
/// interactive, and a modal may stack over a live drawer), which is also
/// why the panel renders `role="dialog"` without `aria-modal`.
///
/// Returns the live layer for the caller's Escape guard.
fn overlay_presentation(
    docked: Signal<bool>,
    panel_ref: NodeRef<Div>,
) -> StoredValue<Option<OverlayLayer>> {
    let layer = StoredValue::new(None::<OverlayLayer>);
    let opener = StoredValue::new(None::<web_sys::Element>);
    let focus_done = StoredValue::new(false);

    let acquire = move || {
        if layer.get_value().is_some() {
            return;
        }
        // The opener: whatever was focused when the layer registered.
        opener.set_value(document().active_element());
        layer.set_value(Some(push_overlay_with(FocusPolicy::Capture)));
        focus_done.set_value(false);
    };
    let release = move || {
        if let Some(live) = layer.get_value() {
            live.release();
            layer.set_value(None);
            opener.set_value(None);
            focus_done.set_value(false);
        }
    };

    // An overlay drawer registers synchronously at mount, exactly where
    // `use_overlay_layer_with` did: the stack is LIFO, so its order has
    // to match mount order, and `Shell` reads `has_layers()` to leave the
    // palette chord inert from the first keystroke on.
    if !docked.get_untracked() {
        acquire();
    }

    // Reading the panel NodeRef subscribes the effect to it as well as to
    // `docked`, so initial focus lands as soon as the panel exists; the
    // `focus_done` latch keeps a later re-run from stealing focus back.
    Effect::new(move |_| {
        let panel = panel_ref.get();
        if docked.get() {
            release();
            return;
        }
        acquire();
        if !focus_done.get_value()
            && let Some(el) = panel
            && layer.get_value().is_some_and(OverlayLayer::owns_focus)
        {
            focus_initial(&web_sys::Element::from(el));
            focus_done.set_value(true);
        }
    });

    on_cleanup(move || {
        // Ownership must be read before release: a drawer unmounting
        // beneath a live modal doesn't own focus and must not restore,
        // and a docked drawer holds no layer to restore from at all.
        let Some(live) = layer.get_value() else {
            return;
        };
        let owned = live.owns_focus();
        live.release();
        layer.set_value(None);
        if owned
            && let Some(back) = opener
                .get_value()
                .and_then(|el| el.dyn_into::<web_sys::HtmlElement>().ok())
            && back.is_connected()
        {
            let _ = back.focus();
        }
    });

    layer
}
