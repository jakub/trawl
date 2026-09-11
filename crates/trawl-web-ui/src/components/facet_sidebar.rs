// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<FacetSidebar/>` — 224px Splunk-style filter rail.
//!
//! Header (title, plus "Clear all" once a filter is set), a filter
//! input, plus per-field collapsible groups with proportional value
//! bars and include/exclude actions the row reveals on hover or focus
//! (opacity, never `display: none`, so the buttons keep their place in
//! the tab order — ADR-0029). Pressing `+` / `⊘` on a value adds an
//! include / exclude `Filter` to the shared filters signal; the parent
//! owns state and re-runs the query via URL navigation.

use std::collections::HashMap;

use fleet_ui::{Icon, IconView, LoadState, Loaded, SearchInput};
use leptos::prelude::*;
use leptos_use::use_media_query;
use trawl_api::QueryResponse;

use crate::api::ApiError;
use crate::facets::compute_facets;
use crate::state::query::{Filter, FilterOp};

#[component]
#[allow(clippy::too_many_lines)] // facet markup tree is one cohesive view
pub fn FacetSidebar(
    rows: LocalResource<Result<QueryResponse, ApiError>>,
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
    let compact = use_media_query("(max-width: 900px)");
    let open = RwSignal::new(false);
    let panel = NodeRef::<leptos::html::Details>::new();
    Effect::new(move |_| {
        if compact.get()
            && let Some(panel) = panel.get()
            && let Some(active) = document().active_element()
            && panel.contains(Some(&active))
        {
            open.set(true);
        }
    });

    view! {
        <details
            class="facet-panel"
            node_ref=panel
            open=move || !compact.get() || open.get()
            on:toggle=move |_| {
                if compact.get() && let Some(panel) = panel.get() {
                    open.set(panel.has_attribute("open"));
                }
            }
        >
        <summary>
            "Filters"
            <span class="facet-count">{move || {
                let count = filters.get().len();
                if count == 0 || suppressed.get() { String::new() }
                else { format!("{count} active") }
            }}</span>
        </summary>
        <aside class="facets" aria-label="Search filters">
            <div class="phead">
                <div class="ttl">"Filters"</div>
                <Show when=move || !filters.get().is_empty() && !suppressed.get()>
                    <button
                        type="button"
                        class="clear"
                        on:click=move |_| on_clear.run(())
                    >"Clear all"</button>
                </Show>
            </div>
            <SearchInput value=needle placeholder="Filter field values"/>
            <Loaded
                state=Signal::derive(move || LoadState::from_resource(rows.get()))
                // Deliberate quiet-error override: the results table
                // already reports the query failure, and repeating it in
                // the facet rail is noise.
                error=Box::new(|_| view! { <p class="facets-hint">"—"</p> }.into_any())
                render=Box::new(move |resp: QueryResponse| {
                    if suppressed.get() {
                        return ().into_any();
                    }
                    let facets = compute_facets(&resp.result);
                    if facets.is_empty() {
                        return ().into_any();
                    }
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
