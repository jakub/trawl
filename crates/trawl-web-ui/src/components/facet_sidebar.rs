// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<FacetSidebar/>` — the filter rail (the glossary's term; the code
//! keeps its old name).
//!
//! One `<details class="facet-panel">` serves both layouts, so the value
//! search text and the group state survive a breakpoint crossing.
//!
//! At 900px and wider the rail follows the page (ADR-0044). It opens for
//! a countable page and closes, to a 32px strip, for any other settled
//! answer: idle, zero rows, rows with nothing to count, an aggregation, a
//! failed query or a malformed link. A press on the `<summary>` is a
//! hand choice, which holds for the browser session through
//! [`FilterRailChoice`]. Below 900px the rail is a disclosure above the
//! results that starts closed and opens only by hand, with its own
//! component-local state.
//!
//! One writer each. The summary's `click` writes the state, at both
//! widths: it cancels the native toggle and flips the narrow state or
//! the wide choice. The `open` binding writes the DOM. `toggle` records
//! nothing, because it also fires for the automatic changes and arrives
//! a frame late. Below 900px it only takes up an open the browser made
//! by itself, as find-in-page does for a match. A closed wide rail's
//! content is `inert`, which find-in-page and text fragments skip, so
//! the browser cannot open the wide rail that way.
//!
//! The `<summary>` is the rail's one control: a vertical strip when the
//! rail is closed, and the header row (chevron, "Filters", the active
//! count) when it is open, with "Clear all" beside it once a filter is
//! set. Below the header sit a filter input and the per-field
//! collapsible groups, with proportional value bars and include/exclude
//! actions the row reveals on hover or focus (opacity, never
//! `display: none`, so the buttons keep their place in the tab order —
//! ADR-0029). Pressing `+` / `⊘` on a value adds an include / exclude
//! `Filter` to the shared filters signal; the parent owns state and
//! re-runs the query via URL navigation. An open rail with nothing to
//! count shows one hint in place of the input and the groups.

use std::collections::HashMap;

use fleet_ui::{Icon, IconView, LoadState, Loaded, SearchInput};
use leptos::prelude::*;
use leptos::{ev, web_sys};
use leptos_use::{
    UseEventListenerOptions, use_event_listener_with_options, use_media_query, use_window,
};
use trawl_api::value::QueryResult;
use wasm_bindgen::JsCast;

use crate::facets::RailPage;
use crate::state::filter_rail::FilterRailChoice;
use crate::state::query::{Filter, FilterOp};

#[component]
#[allow(clippy::too_many_lines)] // facet markup tree is one cohesive view
pub fn FacetSidebar(
    /// The rows on screen, from whichever source the page's mode makes
    /// active: the snapshot page, or the live ring. Read here for its
    /// loading and error states only; the groups come from `page`.
    #[prop(into)]
    state: Signal<LoadState<QueryResult>>,
    /// Current filters — read to paint selected/excluded value rows.
    #[prop(into)]
    filters: Signal<Vec<Filter>>,
    /// True while the link's structured state could not be read. The
    /// rail then shows its header and nothing else: facets are counted
    /// off rows, and a query that was refused has no rows to count —
    /// including a response that was already in flight when the URL
    /// turned unreadable (ADR-0027). "Clear all" goes with them, since
    /// clearing filters navigates.
    #[prop(into)]
    suppressed: Signal<bool>,
    /// Hide row-derived controls for a pending response, an aggregation,
    /// or an unknown source. URL filter count and Clear all remain usable.
    #[prop(into)]
    rows_suppressed: Signal<bool>,
    /// The last settled answer's page: the groups to render, or the hint
    /// for a page with nothing to count. `None` before any answer.
    #[prop(into)]
    page: Signal<Option<RailPage>>,
    /// Whether that page is countable, which opens the wide rail in
    /// automatic mode. Separate from `page` so the open state is not
    /// recomputed on every live frame.
    #[prop(into)]
    countable: Signal<Option<bool>>,
    /// Called when the user clicks `+` or `⊘` on a facet value.
    on_add: Callback<Filter>,
    /// Called when the user clicks "clear all" in the header.
    on_clear: Callback<()>,
) -> impl IntoView {
    // Per-field collapse state — closed groups stash here. New fields
    // start expanded.
    let collapsed: RwSignal<HashMap<String, bool>> = RwSignal::new(HashMap::new());
    // Free-text filter applied across value names within a group.
    let needle = RwSignal::new(String::new());
    // Per-field expansion (show top 5 vs all matching values).
    let expanded: RwSignal<HashMap<String, bool>> = RwSignal::new(HashMap::new());

    // Keep one facet tree mounted across both layouts. Filters themselves
    // remain URL-owned; this disclosure only controls their presentation.
    // 899.98px is the codebase's one spelling of "narrow" (`Shell` and
    // both stylesheets query it), so the disclosure and the layout it
    // presents can never disagree by a pixel.
    let compact = use_media_query("(max-width: 899.98px)");
    // The narrow disclosure's own state, written by a narrow press on the
    // `<summary>`, the focus reopen below, and the `toggle` reconcile for
    // an open the browser made by itself. The wide rail never reads it.
    let open = RwSignal::new(false);
    let panel = NodeRef::<leptos::html::Details>::new();
    let summary = NodeRef::<leptos::html::Summary>::new();
    // Narrowing with focus in the rail's content opens the disclosure,
    // so the focused control is not hidden inside a closed one. The
    // `<summary>` is the part a closed disclosure still shows, so focus
    // on it leaves the disclosure as it is.
    Effect::new(move |_| {
        if compact.get()
            && let Some(panel) = panel.get()
            && let Some(active) = document().active_element()
            && panel.contains(Some(&active))
            && !summary
                .get()
                .is_some_and(|summary| summary.is_same_node(Some(&active)))
        {
            open.set(true);
        }
    });

    // The wide rail: a hand choice for the session, else the page.
    let choice = expect_context::<FilterRailChoice>();
    let wide_open = Memo::new(move |_| {
        choice
            .held()
            .unwrap_or_else(|| countable.get() == Some(true))
    });
    let rail_open = Memo::new(move |_| {
        if compact.get() {
            open.get()
        } else {
            wide_open.get()
        }
    });
    let hint = Memo::new(move |_| page.with(|page| page.as_ref().and_then(RailPage::hint)));
    let body_shown =
        move || !suppressed.get() && !rows_suppressed.get() && countable.get() == Some(true);

    // Whether keyboard focus is somewhere in the rail. Set on the way in,
    // cleared when focus moves to an element outside it or a pointer
    // presses outside it. Focus that drops to `body` with no destination
    // clears nothing: a query unmounts the groups the moment it is sent,
    // which drops a focused group header to `body`, and the rail is still
    // the reader's place until the answer settles.
    let focus_inside = StoredValue::new(false);
    let outside_rail = move |target: Option<web_sys::EventTarget>| {
        target
            .and_then(|target| target.dyn_into::<web_sys::Node>().ok())
            .is_some_and(|target| {
                panel
                    .get_untracked()
                    .is_some_and(|panel| !panel.contains(Some(&target)))
            })
    };
    // A press on content that takes no focus, such as the top bar's
    // title, also leaves `body` active, so the focus events alone cannot
    // tell it from the unmount above. The press itself says the reader
    // went elsewhere. Captured at the window, so no handler that stops
    // the press can hide it; the hook removes the listener with the rail.
    let _ = use_event_listener_with_options(
        use_window(),
        ev::pointerdown,
        move |event: web_sys::PointerEvent| {
            if outside_rail(event.target()) {
                focus_inside.set_value(false);
            }
        },
        UseEventListenerOptions::default().capture(true),
    );
    // If the wide rail closes while the reader's focus is in it, move
    // focus to the `<summary>`, the one control a closed rail keeps. On
    // the close edge only; the narrow disclosure has its own rule above.
    Effect::new(move |was_open: Option<bool>| {
        let is_open = rail_open.get();
        if was_open == Some(true)
            && !is_open
            && !compact.get_untracked()
            && let Some(panel) = panel.get_untracked()
            && let Some(summary) = summary.get_untracked()
        {
            let document = document();
            // Focus still in the rail is stranded whatever the flag says.
            // The flag only decides whether an idle `body` holds focus
            // that dropped there from the rail.
            let dropped = focus_inside.get_value();
            let stranded = document.active_element().map_or(dropped, |active| {
                panel.contains(Some(&active))
                    || (dropped
                        && document
                            .body()
                            .is_some_and(|body| body.is_same_node(Some(&active))))
            });
            if stranded {
                let _ = summary.focus();
            }
        }
        is_open
    });

    view! {
        <details
            class="facet-panel"
            node_ref=panel
            open=move || rail_open.get()
            // Not a record of presses: the `<summary>` click records
            // those at both widths. `toggle` is dispatched a frame after
            // the change it reports, when the width may have crossed the
            // breakpoint, and it fires for the binding's own writes too.
            // Its one job is narrow. When the DOM disagrees with what the
            // binding last rendered, the browser opened or closed the
            // disclosure by itself (find-in-page opens one to show a
            // match), and the narrow state takes that up, or the next
            // press would look dead. It never writes the wide choice: the
            // first automatic open would become a hand choice.
            on:toggle=move |_| {
                if compact.get_untracked()
                    && let Some(panel) = panel.get_untracked()
                {
                    let shown = panel.has_attribute("open");
                    if shown != rail_open.get_untracked() {
                        open.set(shown);
                    }
                }
            }
            on:focusin=move |_| focus_inside.set_value(true)
            on:focusout=move |event: web_sys::FocusEvent| {
                if outside_rail(event.related_target()) {
                    focus_inside.set_value(false);
                }
            }
        >
        <summary
            node_ref=summary
            // The one writer of the rail's state at both widths. The
            // press cancels the native toggle and flips the state its
            // width owns: the narrow disclosure's own, or the wide hand
            // choice. The `open` binding above is the only writer of the
            // DOM, so it renders either.
            on:click=move |event: web_sys::MouseEvent| {
                event.prevent_default();
                if compact.get_untracked() {
                    open.update(|open| *open = !*open);
                } else {
                    choice.choose(!wide_open.get_untracked());
                }
            }
        >
            <span class="facet-chev" aria-hidden="true">
                <IconView icon=Icon::Chevron size=12 stroke_width=1.5/>
            </span>
            "Filters"
            <span class="facet-count">{move || {
                let count = filters.get().len();
                if count == 0 || suppressed.get() { String::new() }
                else { format!("{count} active") }
            }}</span>
        </summary>
        // A closed wide rail's content is inert, so the browser cannot
        // open the rail by itself. Find-in-page and a link's text
        // fragment show a match inside a closed `<details>` by opening
        // it, and that open would bypass the hand choice: the rail would
        // show open while its state says closed, and the next press
        // would look dead. Inert content is not searched. The narrow
        // disclosure stays searchable, and `on:toggle` follows what the
        // browser opens there.
        <aside
            class="facets"
            aria-label="Search filters"
            inert=move || !compact.get() && !rail_open.get()
        >
            <div class="phead">
                <Show when=move || !filters.get().is_empty() && !suppressed.get()>
                    <button
                        type="button"
                        class="clear"
                        on:click=move |_| on_clear.run(())
                    >"Clear all"</button>
                </Show>
            </div>
            // A settled page with nothing to count says so. A malformed
            // link shows the header only: suppression outranks the hint.
            {move || {
                (!suppressed.get())
                    .then(|| hint.get())
                    .flatten()
                    .map(|hint| view! { <p class="facets-hint">{hint}</p> })
            }}
            // Every gate hides the value search with the groups: it
            // filters names that are not being shown.
            <Show when=body_shown>
                <SearchInput value=needle placeholder="Filter field values"/>
            </Show>
            // Suppression is total: an unreadable link has no active
            // source, so the rail shows its header and nothing else —
            // not even the loading hint the state would otherwise
            // render (ADR-0027).
            <Show when=body_shown>
            <Loaded
                state=state
                // Deliberate quiet-error override: the results table
                // already reports the query failure, and repeating it in
                // the facet rail is noise.
                error=Box::new(|_| view! { <p class="facets-hint">"—"</p> }.into_any())
                // The groups were counted once, with the page's verdict;
                // this only applies the value search to them.
                render=Box::new(move |_: QueryResult| {
                    let Some(RailPage::Countable(facets)) = page.get() else {
                        return ().into_any();
                    };
                    let q = needle.get().to_lowercase();
                    let active = filters.get();
                    facets.into_iter().map(|(field, values)| {
                        let max = values.iter().map(|(_, c)| *c).max().unwrap_or(1).max(1);
                        let total = values.len();
                        let filtered: Vec<_> = if q.is_empty() {
                            values
                        } else {
                            values.into_iter()
                                .filter(|(v, _)| v.to_lowercase().contains(&q))
                                .collect()
                        };
                        let show_all = expanded.get().get(&field).copied().unwrap_or(false);
                        let visible: Vec<_> = if show_all {
                            filtered.clone()
                        } else {
                            filtered.iter().take(5).cloned().collect()
                        };
                        let extra = filtered.len().saturating_sub(visible.len());
                        let is_collapsed = collapsed.get().get(&field).copied().unwrap_or(false);
                        let field_for_toggle = field.clone();
                        let field_for_more = field.clone();
                        let more_label = format!("Show {extra} more values for {field}");
                        // The count is a span INSIDE the button, and a
                        // name is the concatenation of what the button
                        // contains: `_time` beside `8` read as `_time8`.
                        // Naming the button explicitly puts the two back
                        // in words; the visible markup is unchanged.
                        let group_label = format!("{field}, {total} values");
                        let active = active.clone();
                        view! {
                            <div class="g" class:collapsed=move || is_collapsed>
                                <button
                                    type="button"
                                    class="g-hd"
                                    aria-label=group_label
                                    aria-expanded=move || (!is_collapsed).to_string()
                                    on:click=move |_| collapsed.update(|c| {
                                        let cur = c.get(&field_for_toggle).copied().unwrap_or(false);
                                        c.insert(field_for_toggle.clone(), !cur);
                                    })
                                >
                                    <span class="chev" aria-hidden="true"><IconView icon=Icon::Chevron size=10 stroke_width=1.5/></span>
                                    <span class="name">{field.clone()}</span>
                                    <span class="cnt">{total}</span>
                                </button>
                                <div class="vals">
                                    {visible.into_iter().map(|(v, c)| {
                                        let pct = (f64::from(c) / f64::from(max)) * 100.0;
                                        let title = v.clone();
                                        let state = match_filter_state(&active, &field, &v);
                                        let field_for_inc = field.clone();
                                        let field_for_exc = field.clone();
                                        let value_for_inc = v.clone();
                                        let value_for_exc = v.clone();
                                        let inc_label = format!("Include {field} = {v}");
                                        let exc_label = format!("Exclude {field} = {v}");
                                        view! {
                                            <div
                                                class="v"
                                                class:selected=move || state == FilterState::Included
                                                class:excluded=move || state == FilterState::Excluded
                                            >
                                                <div
                                                    class="bar"
                                                    style=format!("width: {pct:.1}%")
                                                ></div>
                                                <span class="n" title=title>{v}</span>
                                                <span class="c">{c}</span>
                                                <span class="act">
                                                    <button
                                                        type="button"
                                                        class="op"
                                                        aria-label=inc_label
                                                        // The glyph says
                                                        // nothing on sight, so
                                                        // the hover tooltip is
                                                        // the pointer user's
                                                        // only reading of it.
                                                        // The verb alone: the
                                                        // field and value are
                                                        // on the row already.
                                                        title="Include"
                                                        on:click=move |_| {
                                                            on_add.run(Filter {
                                                                field: field_for_inc.clone(),
                                                                value: value_for_inc.clone(),
                                                                op: FilterOp::Include,
                                                            });
                                                        }
                                                    ><span aria-hidden="true">"+"</span></button>
                                                    <button
                                                        type="button"
                                                        class="op"
                                                        aria-label=exc_label
                                                        title="Exclude"
                                                        on:click=move |_| {
                                                            on_add.run(Filter {
                                                                field: field_for_exc.clone(),
                                                                value: value_for_exc.clone(),
                                                                op: FilterOp::Exclude,
                                                            });
                                                        }
                                                    ><span aria-hidden="true">"⊘"</span></button>
                                                </span>
                                            </div>
                                        }
                                    }).collect::<Vec<_>>()}
                                    {(extra > 0).then(|| view! {
                                        <button
                                            type="button"
                                            class="more"
                                            aria-label=more_label
                                            on:click=move |_| expanded.update(|e| {
                                                e.insert(field_for_more.clone(), true);
                                            })
                                        >
                                            {format!("+ {extra} more")}
                                        </button>
                                    })}
                                </div>
                            </div>
                        }
                    }).collect::<Vec<_>>().into_any()
                })
            />
            </Show>
        </aside>
        </details>
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterState {
    None,
    Included,
    Excluded,
}

fn match_filter_state(active: &[Filter], field: &str, value: &str) -> FilterState {
    for f in active {
        if f.field == field && f.value == value {
            return match f.op {
                FilterOp::Include => FilterState::Included,
                FilterOp::Exclude => FilterState::Excluded,
            };
        }
    }
    FilterState::None
}
