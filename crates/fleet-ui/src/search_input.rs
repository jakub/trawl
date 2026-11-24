// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<SearchInput/>` — icon-adorned filter input.

use leptos::prelude::*;

use crate::icon::{Icon, IconView};

#[component]
pub fn SearchInput(
    value: RwSignal<String>,
    #[prop(optional)] placeholder: &'static str,
) -> impl IntoView {
    view! {
        <div class="inp-wrap">
            <IconView icon=Icon::Search size=12 stroke_width=1.5/>
            <input
                placeholder=placeholder
                prop:value=move || value.get()
                on:input=move |e| value.set(event_target_value(&e))
            />
        </div>
    }
}
