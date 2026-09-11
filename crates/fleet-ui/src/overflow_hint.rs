// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A scroll instruction that appears only while columns extend beyond a viewport.

use leptos::{html, prelude::*};
use leptos_use::{
    UseMutationObserverOptions, use_mutation_observer_with_options, use_resize_observer,
};

/// Place beside the scroll region so the instruction cannot affect its width.
/// Both observers disconnect when the component unmounts.
#[component]
pub fn OverflowHint(viewport: NodeRef<html::Div>) -> impl IntoView {
    let overflowing = RwSignal::new(false);
    let measure = move || {
        if let Some(viewport) = viewport.get_untracked() {
            let next = viewport.scroll_width() > viewport.client_width();
            if overflowing.get_untracked() != next {
                overflowing.set(next);
            }
        }
    };
    let _ = use_resize_observer(viewport, move |_, _| measure());
    let _ = use_mutation_observer_with_options(
        viewport,
        move |_, _| measure(),
        UseMutationObserverOptions::default()
            .child_list(true)
            .subtree(true)
            .character_data(true),
    );

    view! {
        <Show when=move || overflowing.get()>
            <p class="overflow-hint">"Scroll horizontally for more columns."</p>
        </Show>
    }
}
