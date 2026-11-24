// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<Tabs/>` — generic tab strip, rendered as a WAI-ARIA tablist
//! (ADR-0028).

use leptos::html::Div;
use leptos::prelude::*;
use leptos::web_sys;
use wasm_bindgen::JsCast;

use crate::roving::{horizontal_nav, next_index, resolve_selected};

/// `count` is ignored by the drawer family (no chip in that design).
#[derive(Debug, Clone)]
pub struct TabItem {
    pub id: &'static str,
    pub label: &'static str,
    pub count: Signal<Option<usize>>,
}

impl TabItem {
    #[must_use]
    pub fn new(id: &'static str, label: &'static str) -> Self {
        Self {
            id,
            label,
            count: Signal::derive(|| None),
        }
    }

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TabsStyle {
    #[default]
    Workspace,
    Drawer,
}

/// `meta` renders the drawer strip's trailing `.meta` text; ignored by the
/// workspace family. `trailing` is the workspace family's right-aligned
/// action slot; ignored by the drawer family.
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

/// The current position is read from the DOM rather than from a signal,
/// because focus may sit on a tab the user arrowed to and never activated.
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

/// URL-synced tab signals (trawl's `?ntab=…` / `?stab=…`) read back
/// the empty string before the user touches the strip.
#[must_use]
pub fn effective_active(active: Signal<String>, default: &'static str) -> Signal<String> {
    Signal::derive(move || {
        let t = active.get();
        if t.is_empty() { default.to_string() } else { t }
    })
}
