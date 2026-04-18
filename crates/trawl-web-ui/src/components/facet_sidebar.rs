// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<FacetSidebar/>` — field → top values + counts, derived from the
//! current page of query results.
//!
//! Display-only in v1: clicking a value does nothing. Each field is a
//! collapsible `<details>` group defaulting to open.

use leptos::prelude::*;
use trawl_api::QueryResponse;

use crate::api::ApiError;
use crate::facets::compute_facets;

#[component]
pub fn FacetSidebar(rows: LocalResource<Result<QueryResponse, ApiError>>) -> impl IntoView {
    view! {
        <aside class="facets">
            {move || match rows.get() {
                None => view! { <p class="facets-hint">"loading…"</p> }.into_any(),
                Some(Err(_)) => view! { <p class="facets-hint">"—"</p> }.into_any(),
                Some(Ok(resp)) => {
                    let facets = compute_facets(&resp.result);
                    if facets.is_empty() {
                        view! { <p class="facets-hint">"no facetable fields"</p> }.into_any()
                    } else {
                        view! {
                            <ul class="facet-list">
                                {facets.into_iter().map(|(field, values)| {
                                    view! {
                                        <li class="facet-group">
                                            <details open=true>
                                                <summary>{field}</summary>
                                                <ul class="facet-values">
                                                    {values.into_iter().map(|(v, c)| {
                                                        view! {
                                                            <li class="facet-value">
                                                                <span class="v">{v}</span>
                                                                <span class="c">{c}</span>
                                                            </li>
                                                        }
                                                    }).collect::<Vec<_>>()}
                                                </ul>
                                            </details>
                                        </li>
                                    }
                                }).collect::<Vec<_>>()}
                            </ul>
                        }.into_any()
                    }
                }
            }}
        </aside>
    }
}
