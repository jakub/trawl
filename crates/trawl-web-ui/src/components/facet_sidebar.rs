// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<FacetSidebar/>` — 224px Splunk-style filter rail.
//!
//! Header (Filters · count · clear all) + filter input + per-field
//! collapsible groups with proportional value bars and hover-only
//! include/exclude actions. Clicking `+` / `⊘` on a value adds an
//! include / exclude `Filter` to the shared filters signal; the parent
//! owns state and re-runs the query via URL navigation.

use std::collections::HashMap;

use leptos::prelude::*;
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

    view! {
        <aside class="facets">
            <div class="phead">
                <div class="ttl">"Filters"</div>
                <Show when=move || !filters.get().is_empty()>
                    <div
                        class="clear"
                        on:click=move |_| on_clear.run(())
                    >"clear all"</div>
                </Show>
            </div>
            <div class="fsearch">
                <FsearchIcon/>
                <input
                    placeholder="filter field values"
                    on:input=move |e| needle.set(event_target_value(&e))
                />
            </div>
            {move || match rows.get() {
                None => view! { <p class="facets-hint">"loading…"</p> }.into_any(),
                Some(Err(_)) => view! { <p class="facets-hint">"—"</p> }.into_any(),
                Some(Ok(resp)) => {
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
                        let active = active.clone();
                        view! {
                            <div class="g" class:collapsed=move || is_collapsed>
                                <div
                                    class="g-hd"
                                    on:click=move |_| collapsed.update(|c| {
                                        let cur = c.get(&field_for_toggle).copied().unwrap_or(false);
                                        c.insert(field_for_toggle.clone(), !cur);
                                    })
                                >
                                    <span class="chev"><ChevIcon/></span>
                                    <span class="name">{field.clone()}</span>
                                    <span class="cnt">{total}</span>
                                </div>
                                <div class="vals">
                                    {visible.into_iter().map(|(v, c)| {
                                        let pct = (f64::from(c) / f64::from(max)) * 100.0;
                                        let title = v.clone();
                                        let state = match_filter_state(&active, &field, &v);
                                        let field_for_inc = field.clone();
                                        let field_for_exc = field.clone();
                                        let value_for_inc = v.clone();
                                        let value_for_exc = v.clone();
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
                                                    <span
                                                        class="op"
                                                        title="Include"
                                                        on:click=move |e| {
                                                            e.stop_propagation();
                                                            on_add.run(Filter {
                                                                field: field_for_inc.clone(),
                                                                value: value_for_inc.clone(),
                                                                op: FilterOp::Include,
                                                            });
                                                        }
                                                    >"+"</span>
                                                    <span
                                                        class="op"
                                                        title="Exclude"
                                                        on:click=move |e| {
                                                            e.stop_propagation();
                                                            on_add.run(Filter {
                                                                field: field_for_exc.clone(),
                                                                value: value_for_exc.clone(),
                                                                op: FilterOp::Exclude,
                                                            });
                                                        }
                                                    >"⊘"</span>
                                                </span>
                                            </div>
                                        }
                                    }).collect::<Vec<_>>()}
                                    {(extra > 0).then(|| view! {
                                        <div
                                            class="more"
                                            on:click=move |_| expanded.update(|e| {
                                                e.insert(field_for_more.clone(), true);
                                            })
                                        >
                                            {format!("+ {extra} more")}
                                        </div>
                                    })}
                                </div>
                            </div>
                        }
                    }).collect::<Vec<_>>().into_any()
                }
            }}
        </aside>
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

#[component]
fn FsearchIcon() -> impl IntoView {
    view! {
        <svg width="11" height="11" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5">
            <circle cx="7" cy="7" r="4.5"/>
            <path d="m10.5 10.5 3 3"/>
        </svg>
    }
}

#[component]
fn ChevIcon() -> impl IntoView {
    view! {
        <svg width="10" height="10" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5">
            <path d="m4 6 4 4 4-4"/>
        </svg>
    }
}
