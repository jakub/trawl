// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Sidebar/>` — one labelled navigation list (ADR-0032).

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

#[derive(Debug, Clone)]
pub struct RailItem {
    pub id: String,
    pub label: String,
    pub icon: Icon,
    pub path: String,
    pub badge: Option<u64>,
}

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
    #[prop(optional_no_strip)] collapsed: Option<Signal<bool>>,
    #[prop(optional_no_strip)] on_toggle_collapse: Option<Callback<()>>,
    #[prop(into, optional)] overlay: Signal<bool>,
    #[prop(optional_no_strip)] bottom: Option<ViewFn>,
) -> impl IntoView {
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
                                        // `A` marks itself `aria-current="page"` on a PREFIX match unless this is
                                        // set, which would put Search and History both on "page" at
                                        // /search/history.
                                        exact=true
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
