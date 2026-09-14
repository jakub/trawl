// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Tabs/>` — generic tab strip, rendered as a WAI-ARIA tablist
//! (ADR-0028).
//!
//! One component, two class families, selected by [`TabsStyle`]:
//!
//! - [`TabsStyle::Workspace`] — trawl's search-workspace strip
//!   (`.tabs > .tablist > button.t.active`, weight 500, optional
//!   per-tab count chip).
//! - [`TabsStyle::Drawer`] — the drawer strip
//!   (`.sd-tabs > .tablist > button.tb.on`, weight 600, optional
//!   trailing meta text). [`Drawer`](crate::Drawer) composes this
//!   internally.
//!
//! Both families render `<button type="button" role="tab">`; they
//! differ in class and in the active-class idiom only, because each
//! matches its own CSS rules and merging them would point one strip at
//! the other's font weight. Tab identity is a `&'static str` id; apps
//! with typed tab enums adapt at the call site (a two-line id ↔ enum
//! map), keeping app semantics in the app (ADR-0002).
//!
//! ## What the strip owns
//!
//! The `role="tablist"` node wraps the tab buttons and nothing else.
//! The flex spacer, the workspace family's `trailing` action slot and
//! the drawer family's `meta` text are siblings of that node, inside
//! the outer `.tabs` / `.sd-tabs` container: a Save link announced as a
//! tab is worse than one announced as a button. An `items`-less strip
//! (the field case drawer uses the container purely as a meta bar)
//! renders the `.tablist` div without `role` or `aria-label`, because a
//! named tablist holding no tabs is a defect an assistive technology
//! would report.
//!
//! `label` is required and names the strip ("Results", "Service
//! details"): a screen reader announcing "tab list" with no name leaves
//! two strips on one page indistinguishable.
//!
//! ## Keyboard
//!
//! Arrow Right/Left wrap, Home and End jump to the ends
//! ([`crate::roving::horizontal_nav`]), each moving FOCUS only. The
//! strip's single tab stop is derived from SELECTION — `aria-selected`
//! and `tabindex` are one predicate over
//! [`resolve_selected`](crate::roving::resolve_selected), so the strip
//! needs no focus state of its own — which is the deliberate deviation
//! from the APG that [`crate::roving`] documents. That one predicate is
//! also why an `active` id matching no tab selects the FIRST tab
//! instead of none: these ids come out of the URL (`?ntab=bogus`), and
//! selecting none would leave every tab at `tabindex="-1"`, a strip no
//! keyboard can enter, under a pane already showing the first tab.
//! Activation is manual: Enter and Space are the native button's own
//! click, and an arrow never runs `on_change`, so arrowing across
//! trawl's `?ntab=` strip does not rewrite the URL under the user.

use leptos::html::Div;
use leptos::prelude::*;
use leptos::web_sys;
use wasm_bindgen::JsCast;

use crate::roving::{horizontal_nav, next_index, resolve_selected};

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
/// `on_change` fires with the clicked tab's id. `label` names the
/// tablist for assistive technology and is required (see the module
/// docs). `meta` renders the drawer strip's trailing `.meta` text
/// (service drawer's "N events · size · M fields"); ignored by the
/// workspace family. (`meta` is a reactive optional — `MaybeProp` — so
/// live counts tick, while static `String`s still convert via `into`
/// and [`Drawer`](crate::Drawer) forwards its own optional straight
/// through.)
///
/// `trailing` is the workspace family's right-aligned action slot,
/// rendered after the flex spacer and outside the tablist (trawl's
/// results Save/Export links); ignored by the drawer family, which has
/// `meta` in that position.
#[component]
pub fn Tabs(
    #[prop(optional)] style: TabsStyle,
    items: Vec<TabItem>,
    #[prop(into)] label: String,
    #[prop(into)] active: Signal<String>,
    on_change: Callback<String>,
    #[prop(into, optional)] meta: MaybeProp<String>,
    #[prop(optional)] trailing: Option<Children>,
) -> impl IntoView {
    let list_ref = NodeRef::<Div>::new();
    let ids: Vec<&'static str> = items.iter().map(|i| i.id).collect();
    // A strip with no tabs is a bare container: naming an empty tablist
    // would announce a widget that has nothing in it.
    let named = !ids.is_empty();
    let role = named.then_some("tablist");
    let aria_label = named.then_some(label);
    // One predicate for `aria-selected`, the tabindex and the active
    // class: an id that matches no tab resolves to the first one, so
    // the strip always has exactly one tab stop.
    let selected = {
        let ids = ids.clone();
        Signal::derive(move || resolve_selected(&active.get(), &ids))
    };
    let on_keydown = tab_keydown(list_ref, ids, active);

    match style {
        TabsStyle::Workspace => view! {
            <div class="tabs">
                <div
                    class="tablist"
                    role=role
                    aria-label=aria_label
                    node_ref=list_ref
                    on:keydown=on_keydown
                >
                    {items.into_iter().enumerate().map(|(i, item)| {
                        let id = item.id;
                        view! {
                            <button
                                type="button"
                                class="t"
                                class:active=move || selected.get() == Some(i)
                                role="tab"
                                aria-selected=move || (selected.get() == Some(i)).to_string()
                                tabindex=move || if selected.get() == Some(i) { "0" } else { "-1" }
                                on:click=move |_| on_change.run(id.to_string())
                            >
                                <span>{item.label}</span>
                                {move || item.count.get().map(|c| view! { <span class="c">{c}</span> })}
                            </button>
                        }
                    }).collect_view()}
                </div>
                <div class="sp"></div>
                // One group, not N siblings: at narrow widths the strip
                // wraps, and loose actions wrapped one at a time — an
                // "Export" alone on a second line beside an empty first.
                {trailing.map(|t| view! { <div class="tabs-actions">{t()}</div> })}
            </div>
        }
        .into_any(),
        TabsStyle::Drawer => view! {
            <div class="sd-tabs">
                <div
                    class="tablist"
                    role=role
                    aria-label=aria_label
                    node_ref=list_ref
                    on:keydown=on_keydown
                >
                    {items.into_iter().enumerate().map(|(i, item)| {
                        let id = item.id;
                        view! {
                            <button
                                type="button"
                                class=move || if selected.get() == Some(i) { "tb on" } else { "tb" }
                                role="tab"
                                aria-selected=move || (selected.get() == Some(i)).to_string()
                                tabindex=move || if selected.get() == Some(i) { "0" } else { "-1" }
                                on:click=move |_| on_change.run(id.to_string())
                            >{item.label}</button>
                        }
                    }).collect_view()}
                </div>
                <span class="sp"></span>
                {move || meta.get().map(|m| view! { <span class="meta">{m}</span> })}
            </div>
        }
        .into_any(),
    }
}

/// The tablist's arrow walk: move focus, never selection.
///
/// The current position is read from the DOM — the index of
/// `document.activeElement` among the queried `[role="tab"]` list —
/// rather than from a signal, because focus may sit on a tab the user
/// arrowed to and never activated, which by construction is not the
/// selected one. When focus is somewhere else entirely (a keypress
/// routed here from the container), the walk starts from the selected
/// tab.
fn tab_keydown(
    list_ref: NodeRef<Div>,
    ids: Vec<&'static str>,
    active: Signal<String>,
) -> impl Fn(web_sys::KeyboardEvent) + 'static {
    move |e: web_sys::KeyboardEvent| {
        let Some(nav) = horizontal_nav(&e.key()) else {
            return;
        };
        e.prevent_default();
        let Some(list) = list_ref.get_untracked() else {
            return;
        };
        let Ok(tabs) = list.query_selector_all(r#"[role="tab"]"#) else {
            return;
        };
        let len = usize::try_from(tabs.length()).unwrap_or(0);
        let focused = leptos::prelude::document().active_element();
        let current = (0..len)
            .find(|i| {
                u32::try_from(*i)
                    .ok()
                    .and_then(|i| tabs.get(i))
                    .zip(focused.as_ref())
                    .is_some_and(|(node, el)| el.is_same_node(Some(&node)))
            })
            .or_else(|| resolve_selected(&active.get_untracked(), &ids))
            .unwrap_or(0);
        let Some(next) = next_index(current, len, nav) else {
            return;
        };
        // Focus moves synchronously off this event, so a held arrow key
        // repeats at the browser's own rate instead of racing an effect.
        if let Some(el) = u32::try_from(next)
            .ok()
            .and_then(|i| tabs.get(i))
            .and_then(|n| n.dyn_into::<web_sys::HtmlElement>().ok())
        {
            let _ = el.focus();
        }
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
