// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<ActionsMenu/>` — `⋯` overflow menu for table rows.
//!
//! This module owns the trigger button (`.btn-icon`, accessible name
//! "Actions"), the open/close state, the positioning wrapper
//! (`.actions-wrap`) and the entries. Everything else — the overlay
//! layer, Escape, outside dismissal, roving tabindex, the arrow walk
//! and restore-by-cause — is the shared contract in
//! [`crate::menu`], which the topbar's account menu mounts too, so the
//! two menus cannot drift apart (ADR-0028).
//!
//! Clicks on the trigger and the items stop propagation: every current
//! call site nests the menu inside a clickable table row, so the panel
//! is mounted with `stop_click_propagation`.

use leptos::html::{Button, Div};
use leptos::prelude::*;
use leptos::web_sys;

use crate::menu::{MenuEntry, MenuItem, MenuPanel};

/// One entry in the menu. `danger` renders the destructive (red)
/// treatment. The menu closes, and focus returns to the trigger, before
/// `on_click` runs.
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
    let trigger_ref = NodeRef::<Button>::new();

    let entries: Vec<MenuEntry> = items
        .into_iter()
        .map(|item| {
            MenuEntry::Item(MenuItem {
                label: Signal::stored(item.label.to_string()),
                danger: item.danger,
                on_activate: item.on_click,
            })
        })
        .collect();

    view! {
        <div class="actions-wrap" node_ref=wrap_ref>
            <button
                class="btn-icon"
                type="button"
                node_ref=trigger_ref
                aria-label="Actions"
                aria-haspopup="menu"
                aria-expanded=move || open.get().to_string()
                on:click=move |e: web_sys::MouseEvent| {
                    // Don't bubble into the host row's click handler,
                    // and keep re-click a toggle.
                    e.stop_propagation();
                    open.update(|o| *o = !*o);
                }
            >"⋯"</button>
            <Show when=move || open.get()>
                <MenuPanel
                    panel_class="actions-menu"
                    menu_label="Actions"
                    entries=entries.clone()
                    stop_click_propagation=true
                    open=open
                    wrap_ref=wrap_ref
                    trigger_ref=trigger_ref
                />
            </Show>
        </div>
    }
}
