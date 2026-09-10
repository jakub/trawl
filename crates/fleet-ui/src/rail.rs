// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Rail/>` — left navigation rail. Each app supplies its own item
//! slice and computes its own "active item" signal (typically from
//! the current pathname); fleet-ui owns the look and the link
//! plumbing only.

use leptos::prelude::*;
use leptos_router::components::A;

use crate::icon::{Icon, IconView};

fn item_class(active: bool) -> &'static str {
    if active { "it active" } else { "it" }
}

/// A single item in the rail. `path` is the route navigated to on
/// click; `id` is the discriminant the parent compares against the
/// `active` signal to drive the accent-bar styling.
///
/// `badge` is an optional count chip (unread stories, pending
/// editions…). Like `ModeTab.active`, it's a plain value — callers
/// rebuild the item Vec reactively when the count changes. Zero counts
/// are hidden: `Some(0)` renders no chip, same as `None`.
#[derive(Debug, Clone)]
pub struct RailItem {
    pub id: String,
    pub label: String,
    pub icon: Icon,
    pub path: String,
    pub badge: Option<u64>,
}

#[component]
pub fn Rail(
    #[prop(into)] items: Signal<Vec<RailItem>>,
    #[prop(into)] active: Signal<String>,
    /// Bottom slot rendered inside `<div class="bot">`, used by trawl's Help
    /// link. Omitting it removes the `.bot` div.
    /// `optional_no_strip` keeps the `Option` wrapper so `Shell` can
    /// forward its own `Option<Children>` straight through.
    #[prop(optional_no_strip)]
    bottom: Option<Children>,
) -> impl IntoView {
    view! {
        <nav class="rail">
            {move || {
                items.get().into_iter().map(|item| {
                    let id = item.id.clone();
                    let label_attr = item.label.clone();
                    view! {
                        <A
                            href=item.path
                            attr:class=move || item_class(active.get() == id)
                            attr:title=label_attr
                        >
                            <IconView icon=item.icon size=20 stroke_width=1.4/>
                            <span class="lb">{item.label}</span>
                            {item.badge.filter(|n| *n > 0).map(|n| view! { <span class="badge">{n}</span> })}
                        </A>
                    }
                }).collect::<Vec<_>>()
            }}
            {bottom.map(|b| view! { <div class="bot">{b()}</div> })}
        </nav>
    }
}
