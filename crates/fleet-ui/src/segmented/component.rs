// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `<Segmented>` component. Wasm-only (pulls leptos) — the pure
//! [`segmented_class`](super::class::segmented_class) composition and the
//! [`SegmentedOption`] identity it renders live in [`super::class`] so
//! their CSS-class contract is native-testable.

use leptos::prelude::*;

use super::class::{SegmentedOption, segmented_class};
use crate::button::Size;

/// Exclusive-choice pill strip. `active` is the id of the selected
/// option; `on_change` fires with the clicked option's id. `full`
/// stretches the strip to the container width with equal-width options
/// (the export modal's format row).
#[component]
pub fn Segmented(
    options: Vec<SegmentedOption>,
    #[prop(into)] active: Signal<String>,
    on_change: Callback<String>,
    #[prop(optional)] size: Size,
    #[prop(default = false)] full: bool,
) -> impl IntoView {
    let class = segmented_class(size, full);

    view! {
        <div class=class>
            {options.into_iter().map(|opt| {
                let id = opt.id;
                view! {
                    <button
                        class="seg-opt"
                        class:on=move || active.get() == id
                        // The visual `on` state is invisible to assistive
                        // tech; a segmented option is a toggle button, so
                        // the selection must also ride aria-pressed.
                        aria-pressed=move || if active.get() == id { "true" } else { "false" }
                        on:click=move |_| on_change.run(id.to_string())
                    >{opt.label}</button>
                }
            }).collect_view()}
        </div>
    }
}
