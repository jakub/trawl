// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use leptos::prelude::*;

use super::class::{SegmentedOption, segmented_class};
use crate::button::Size;

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
                        aria-pressed=move || if active.get() == id { "true" } else { "false" }
                        on:click=move |_| on_change.run(id.to_string())
                    >{opt.label}</button>
                }
            }).collect_view()}
        </div>
    }
}
