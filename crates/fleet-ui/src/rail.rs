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

/// Aliased so trawl-web-ui consumers can rename their imports without
/// touching item-construction sites.
pub type RailIcon = Icon;

/// A single item in the rail. `path` is the route navigated to on
/// click; `id` is the discriminant the parent compares against the
/// `active` signal to drive the amber-bar styling.
#[derive(Debug, Clone, Copy)]
pub struct RailItem {
    pub id: &'static str,
    pub label: &'static str,
    pub icon: RailIcon,
    pub path: &'static str,
}

#[component]
pub fn Rail(
    #[prop(into)] items: Signal<&'static [RailItem]>,
    #[prop(into)] active: Signal<String>,
) -> impl IntoView {
    view! {
        <nav class="rail">
            {move || {
                items.get().iter().copied().map(|item| {
                    view! {
                        <A
                            href=item.path
                            attr:class=move || item_class(active.get() == item.id)
                            attr:title=item.label
                        >
                            <IconView icon=item.icon size=16 stroke_width=1.4/>
                            <span class="lb">{item.label}</span>
                        </A>
                    }
                }).collect::<Vec<_>>()
            }}
        </nav>
    }
}
