// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<ActionsMenu/>` — `⋯` overflow menu for table rows (issue #31).
//!
//! Owns its trigger button (`.btn-icon`), its open/close state, and —
//! the ADR-0003-sanctioned behaviour improvement over the hand-rolled
//! nets-page menu — its dismissal: the open panel registers with the
//! [`overlay`](crate::overlay) arbitration stack, so window-level
//! Escape closes it only while it is the topmost overlay, and a
//! window-level `mousedown` outside the trigger+panel wrapper closes
//! it (outside-click). The outside-click check uses `Node::contains`
//! on the wrapper `NodeRef` — unlike [`Modal`](crate::Modal)'s
//! `is_same_node` scrim identity check — because the menu has interior
//! `.item` children and no scrim, and because excluding the trigger
//! keeps a trigger re-click a clean toggle instead of a
//! close-then-reopen.
//!
//! Clicks on the trigger and the items stop propagation: every current
//! call site nests the menu inside a clickable table row.

use leptos::ev;
use leptos::html::Div;
use leptos::prelude::*;
use leptos::web_sys;
use leptos_use::{use_event_listener, use_window};
use wasm_bindgen::JsCast;

/// One entry in the menu. `danger` renders the destructive (red)
/// treatment. The menu closes before `on_click` runs.
#[derive(Clone, Debug)]
pub struct ActionItem {
    pub label: &'static str,
    pub danger: bool,
    pub on_click: Callback<()>,
}

impl ActionItem {
    /// A regular entry.
    #[must_use]
    pub fn new(label: &'static str, on_click: Callback<()>) -> Self {
        Self {
            label,
            danger: false,
            on_click,
        }
    }

    /// A destructive entry (`.item.danger`).
    #[must_use]
    pub fn danger(label: &'static str, on_click: Callback<()>) -> Self {
        Self {
            label,
            danger: true,
            on_click,
        }
    }
}

/// Overflow menu: `⋯` trigger + dropdown panel. The component owns the
/// positioning context (`.actions-wrap`), so call sites just drop it
/// into the row cell.
#[component]
pub fn ActionsMenu(items: Vec<ActionItem>) -> impl IntoView {
    let open = RwSignal::new(false);
    let wrap_ref = NodeRef::<Div>::new();

    view! {
        <div class="actions-wrap" node_ref=wrap_ref>
            <button
                class="btn-icon"
                on:click=move |e: web_sys::MouseEvent| {
                    // Don't bubble into the host row's click handler,
                    // and keep re-click a toggle.
                    e.stop_propagation();
                    open.update(|o| *o = !*o);
                }
            >"⋯"</button>
            <Show when=move || open.get()>
                <MenuPanel items=items.clone() open=open wrap_ref=wrap_ref/>
            </Show>
        </div>
    }
}

/// The open panel. A separate component so the overlay layer and the
/// window listeners exist exactly while the menu is open — a closed
/// menu must not sit on the overlay stack shadowing Escape for the
/// layers beneath it.
#[component]
fn MenuPanel(
    items: Vec<ActionItem>,
    open: RwSignal<bool>,
    wrap_ref: NodeRef<Div>,
) -> impl IntoView {
    let layer = crate::overlay::use_overlay_layer();

    // Window-level Escape, gated on topmost-layer arbitration (same
    // contract as Modal/Drawer): a ConfirmModal stacked above the menu
    // takes Escape without the menu also closing.
    let _ = use_event_listener(use_window(), ev::keydown, move |e| {
        if !layer.is_topmost() {
            return;
        }
        if e.key() == "Escape" {
            e.prevent_default();
            open.set(false);
        }
    });

    // Outside-click: any mousedown outside the trigger+panel wrapper
    // dismisses. Mousedown (not click) so drag-selections that end
    // outside don't dismiss, matching Modal's scrim behaviour.
    let _ = use_event_listener(
        use_window(),
        ev::mousedown,
        move |e: web_sys::MouseEvent| {
            let Some(wrap) = wrap_ref.get_untracked() else {
                return;
            };
            let Some(target) = e.target() else { return };
            let Some(node) = target.dyn_ref::<web_sys::Node>() else {
                return;
            };
            if !wrap.contains(Some(node)) {
                open.set(false);
            }
        },
    );

    view! {
        <div class="actions-menu">
            {items.into_iter().map(|item| {
                let class = if item.danger { "item danger" } else { "item" };
                let cb = item.on_click;
                view! {
                    <div
                        class=class
                        on:click=move |e: web_sys::MouseEvent| {
                            e.stop_propagation();
                            open.set(false);
                            cb.run(());
                        }
                    >{item.label}</div>
                }
            }).collect_view()}
        </div>
    }
}
