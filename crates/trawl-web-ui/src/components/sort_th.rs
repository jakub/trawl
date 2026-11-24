// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use leptos::prelude::*;

use crate::sort_label::{sort_arrow, sort_button_name};

pub fn sort_th<K>(
    sort: RwSignal<(K, bool)>,
    key: K,
    default_desc: bool,
    label: &'static str,
    style: &'static str,
) -> impl IntoView
where
    K: Copy + PartialEq + Send + Sync + 'static,
{
    let direction = move || {
        let (k, desc) = sort.get();
        (k == key).then_some(desc)
    };
    view! {
        <div
            class="th sortable"
            class:active=move || direction().is_some()
            style=style
        >
            <button
                type="button"
                aria-label=move || sort_button_name(label, direction())
                on:click=move |_| {
                    sort.update(|s| {
                        if s.0 == key {
                            s.1 = !s.1;
                        } else {
                            *s = (key, default_desc);
                        }
                    });
                }
            >
                {label}
                <span class="dir" aria-hidden="true">{move || sort_arrow(direction())}</span>
            </button>
        </div>
    }
}

pub fn table_sort_th<K>(
    sort: RwSignal<(K, bool)>,
    key: K,
    default_desc: bool,
    label: &'static str,
    style: &'static str,
) -> impl IntoView
where
    K: Copy + PartialEq + Send + Sync + 'static,
{
    view! {
        <th scope="col" style=style aria-sort=move || {
            let (active, desc) = sort.get();
            (active == key).then_some(if desc { "descending" } else { "ascending" })
        }>
            {sort_th(sort, key, default_desc, label, "")}
        </th>
    }
}
