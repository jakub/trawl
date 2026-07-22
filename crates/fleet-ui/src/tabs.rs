// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Tabs/>` — generic tab strip (issue #28).
//!
//! One component, two class families, selected by [`TabsStyle`]:
//!
//! - [`TabsStyle::Workspace`] — trawl's search-workspace strip
//!   (`.tabs > div.t.active`, weight 500, optional per-tab count chip).
//! - [`TabsStyle::Drawer`] — the drawer strip both trawl drawers
//!   copy-pasted (`.sd-tabs > span.tb.on`, weight 600, optional
//!   trailing meta text). [`Drawer`](crate::Drawer) composes this
//!   internally.
//!
//! The two families render byte-identical markup to the hand-rolled
//! strips they replace — including the element tags (`div` vs `span`)
//! and the class-toggle idioms. Tab identity is a `&'static str` id;
//! apps with typed tab enums adapt at the call site (a two-line
//! id ↔ enum map), keeping app semantics in the app (ADR-0002).
//!
//! For exclusive-choice pill strips that aren't view tabs (format
//! pickers, density toggles), use [`Segmented`](crate::segmented)
//! instead — a third strip idiom with its own `.seg` family.

use leptos::prelude::*;

/// One tab in the strip. `count` drives the workspace count chip
/// (`.t .c`, e.g. the Events row count) — it renders only while the
/// signal is `Some`, and is ignored by the drawer family (no chip in
/// that design).
#[derive(Debug, Clone)]
pub struct TabItem {
    pub id: &'static str,
    pub label: &'static str,
    pub count: Signal<Option<usize>>,
}

impl TabItem {
    /// Tab without a count chip.
    #[must_use]
    pub fn new(id: &'static str, label: &'static str) -> Self {
        Self {
            id,
            label,
            count: Signal::derive(|| None),
        }
    }

    /// Tab with a reactive count chip (workspace family only).
    #[must_use]
    pub fn with_count(
        id: &'static str,
        label: &'static str,
        count: impl Into<Signal<Option<usize>>>,
    ) -> Self {
        Self {
            id,
            label,
            count: count.into(),
        }
    }
}

/// Which class family the strip renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TabsStyle {
    /// `.tabs > div.t(.active)` — workspace strip.
    #[default]
    Workspace,
    /// `.sd-tabs > span.tb(.on)` — drawer strip.
    Drawer,
}

/// Generic tab strip. `active` is the id of the selected tab;
/// `on_change` fires with the clicked tab's id. `meta` renders the
/// drawer strip's trailing `.meta` text (service drawer's
/// "N events · size · M fields"); ignored by the workspace family.
/// (`meta` is a reactive optional — `MaybeProp` — so live counts tick
/// (issue #33 D7) while static Strings still convert via `into`, and
/// [`Drawer`](crate::Drawer) forwards its own optional straight
/// through.)
///
/// `trailing` is the workspace family's right-aligned action slot,
/// rendered after the flex spacer (trawl's results Save/Export links);
/// ignored by the drawer family, which has `meta` in that position.
#[component]
pub fn Tabs(
    #[prop(optional)] style: TabsStyle,
    items: Vec<TabItem>,
    #[prop(into)] active: Signal<String>,
    on_change: Callback<String>,
    #[prop(into, optional)] meta: MaybeProp<String>,
    #[prop(optional)] trailing: Option<Children>,
) -> impl IntoView {
    match style {
        TabsStyle::Workspace => view! {
            <div class="tabs">
                {items.into_iter().map(|item| {
                    let id = item.id;
                    view! {
                        <div
                            class="t"
                            class:active=move || active.get() == id
                            on:click=move |_| on_change.run(id.to_string())
                        >
                            <span>{item.label}</span>
                            {move || item.count.get().map(|c| view! { <span class="c">{c}</span> })}
                        </div>
                    }
                }).collect_view()}
                <div class="sp"></div>
                {trailing.map(|t| t())}
            </div>
        }
        .into_any(),
        TabsStyle::Drawer => view! {
            <div class="sd-tabs">
                {items.into_iter().map(|item| {
                    let id = item.id;
                    view! {
                        <span
                            class=move || if active.get() == id { "tb on" } else { "tb" }
                            on:click=move |_| on_change.run(id.to_string())
                        >{item.label}</span>
                    }
                }).collect_view()}
                <span class="sp"></span>
                {move || meta.get().map(|m| view! { <span class="meta">{m}</span> })}
            </div>
        }
        .into_any(),
    }
}

/// Map a URL-backed tab signal's empty default to a concrete tab id.
///
/// URL-synced tab signals (trawl's `?ntab=…` / `?stab=…`) read back
/// the empty string before the user touches the strip. [`Tabs`] and
/// [`Drawer`](crate::Drawer) compare ids verbatim, so that initial
/// empty value matches no tab. This derives the effective id — the
/// raw value, or `default` while it is empty — for use as both the
/// strip's `active` and the app's pane switch, keeping the
/// empty→default mapping (generic tab-strip behaviour) out of every
/// call site.
#[must_use]
pub fn effective_active(active: Signal<String>, default: &'static str) -> Signal<String> {
    Signal::derive(move || {
        let t = active.get();
        if t.is_empty() { default.to_string() } else { t }
    })
}
