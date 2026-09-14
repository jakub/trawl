// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Sidebar/>` — one labelled navigation list (ADR-0032). Replaces the
//! icon rail and the topbar mode tabs: each app supplies its own groups
//! and computes its own "active item" signal (typically from the current
//! pathname); fleet-ui owns the look and the link plumbing only.
//!
//! Groups are data, not a fixed taxonomy: a group with no label renders
//! its items with no heading, a labelled one renders a small-caps
//! heading and names the run for assistive technology and for the
//! command palette. The collapsed presentation is icon-only — every
//! link keeps its `title`, and the visible label becomes screen-reader
//! text, so nothing loses its accessible name.
//!
//! [`RailItem`] and [`SidebarGroup`] are pure `&'static`-shaped data and
//! build on every target (the consumer's route table is native code);
//! the component itself is wasm32-only.

#[cfg(target_arch = "wasm32")]
use leptos::prelude::*;
#[cfg(target_arch = "wasm32")]
use leptos_router::components::A;

use crate::icon::Icon;
#[cfg(target_arch = "wasm32")]
use crate::icon::IconView;

#[cfg(target_arch = "wasm32")]
fn item_class(active: bool) -> &'static str {
    if active { "it active" } else { "it" }
}

/// A single destination. `path` is the route navigated to on click; `id`
/// is the discriminant the parent compares against the `active` signal
/// to drive the accent treatment.
///
/// `badge` is an optional count chip (unread stories, pending
/// editions…) — a plain value, so callers rebuild the item Vec
/// reactively when the count changes. Zero counts are hidden:
/// `Some(0)` renders no chip, same as `None`.
#[derive(Debug, Clone)]
pub struct RailItem {
    pub id: String,
    pub label: String,
    pub icon: Icon,
    pub path: String,
    pub badge: Option<u64>,
}

/// A labelled run of destinations. `label: None` renders the items with
/// no heading (trawl's first group); `Some` renders a small-caps heading
/// and names the group for assistive technology and the command palette.
#[derive(Debug, Clone)]
pub struct SidebarGroup {
    pub label: Option<String>,
    pub items: Vec<RailItem>,
}

#[cfg(target_arch = "wasm32")]
#[component]
pub fn Sidebar(
    #[prop(into)] brand: String,
    #[prop(into)] brand_accent: String,
    #[prop(into)] groups: Signal<Vec<SidebarGroup>>,
    #[prop(into)] active: Signal<String>,
    /// Collapsed (icon-only) presentation. `None` when the consumer
    /// provides no `UiPrefs`: then no collapse control renders.
    /// `optional_no_strip` keeps the `Option` so `Shell` forwards its own
    /// optional pref straight through.
    #[prop(optional_no_strip)]
    collapsed: Option<Signal<bool>>,
    #[prop(optional_no_strip)] on_toggle_collapse: Option<Callback<()>>,
    /// True inside the <900px overlay: always expanded, no collapse control.
    #[prop(into, optional)]
    overlay: Signal<bool>,
    /// Bottom-pinned slot (trawl's Help link), rendered inside `.bot`.
    /// A [`ViewFn`] rather than `Children` because the shell mounts the
    /// sidebar twice — docked and as the compact overlay — and a
    /// `FnOnce` slot cannot be rendered by both; `optional_no_strip`
    /// keeps the `Option` so `Shell` forwards its own slot through.
    #[prop(optional_no_strip)]
    bottom: Option<ViewFn>,
) -> impl IntoView {
    // The collapse control exists only when the consumer persists the
    // state; without a toggle callback the button would advertise a
    // capability the shell cannot deliver (ADR-0025).
    let collapsible = collapsed.is_some() && on_toggle_collapse.is_some();
    let has_bottom = bottom.is_some();

    view! {
        <nav
            class="rail"
            class:collapsed=move || collapsed.is_some_and(|c| c.get()) && !overlay.get()
            class:overlay=move || overlay.get()
            id="fleet-sidebar"
            aria-label="Primary"
        >
            <div class="brand">
                <span>{brand}</span><span class="accent">{brand_accent}</span>
            </div>

            {move || {
                groups.get().into_iter().map(|group| {
                    let heading = group.label.clone();
                    view! {
                        <div
                            class="grp"
                            role=group.label.as_ref().map(|_| "group")
                            aria-label=group.label
                        >
                            {heading.map(|label| view! {
                                <span class="grp-lb" aria-hidden="true">{label}</span>
                            })}
                            {group.items.into_iter().map(|item| {
                                let id = item.id.clone();
                                let current = item.id;
                                let label_attr = item.label.clone();
                                view! {
                                    <A
                                        href=item.path
                                        attr:class=move || item_class(active.get() == id)
                                        attr:title=label_attr
                                        attr:aria-current=move || {
                                            (active.get() == current).then_some("page")
                                        }
                                    >
                                        <IconView icon=item.icon size=18 stroke_width=1.5/>
                                        <span class="lb">{item.label}</span>
                                        {item.badge
                                            .filter(|n| *n > 0)
                                            .map(|n| view! { <span class="badge">{n}</span> })}
                                    </A>
                                }
                            }).collect::<Vec<_>>()}
                        </div>
                    }
                }).collect::<Vec<_>>()
            }}

            {(has_bottom || collapsible).then(|| view! {
                <div class="bot">
                    {bottom.map(|slot| slot.run())}
                    {move || (collapsible && !overlay.get()).then(|| {
                        let is_collapsed = collapsed.is_some_and(|c| c.get());
                        let name = if is_collapsed { "Expand sidebar" } else { "Collapse sidebar" };
                        view! {
                            <button
                                type="button"
                                class="it collapse"
                                aria-label=name
                                title=name
                                on:click=move |_| {
                                    if let Some(toggle) = on_toggle_collapse {
                                        toggle.run(());
                                    }
                                }
                            >
                                <IconView icon=Icon::PanelLeft size=18 stroke_width=1.5/>
                                <span class="lb">{if is_collapsed { "Expand" } else { "Collapse" }}</span>
                            </button>
                        }
                    })}
                </div>
            })}
        </nav>
    }
}
