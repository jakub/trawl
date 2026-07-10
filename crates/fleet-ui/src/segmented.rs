// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Segmented/>` — exclusive-choice pill strip (issue #31).
//!
//! Unifies trawl's three hand-rolled segmented controls (the export
//! modal's `.fmt-btn` row, the schema page's `.seg-mini` density
//! toggle, and the date-range popover's mini tab strip) onto ONE
//! canonical treatment (ADR-0003): a bordered pill group with a single
//! amber-wash active style, `.seg > button.seg-opt(.on)` in
//! fleet-ui.css.
//!
//! Option identity is a `&'static str` id; apps with typed enums adapt
//! at the call site (a two-line id ↔ enum map), keeping app semantics
//! in the app (ADR-0002) — same contract as [`Tabs`](crate::tabs).
//! The size axis reuses [`Size`](crate::button::Size): `Default` for
//! modal-scale controls, `Sm` for toolbar-scale (schema's density
//! toggle); `Xs` renders as `Sm` (no third scale in the design).

use leptos::prelude::*;

use crate::button::Size;

/// One option in the strip.
#[derive(Debug, Clone)]
pub struct SegmentedOption {
    pub id: &'static str,
    pub label: &'static str,
}

impl SegmentedOption {
    #[must_use]
    pub fn new(id: &'static str, label: &'static str) -> Self {
        Self { id, label }
    }
}

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
    let mut class = String::from("seg");
    if !matches!(size, Size::Default) {
        class.push_str(" seg-sm");
    }
    if full {
        class.push_str(" seg-full");
    }

    view! {
        <div class=class>
            {options.into_iter().map(|opt| {
                let id = opt.id;
                view! {
                    <button
                        class="seg-opt"
                        class:on=move || active.get() == id
                        on:click=move |_| on_change.run(id.to_string())
                    >{opt.label}</button>
                }
            }).collect_view()}
        </div>
    }
}
