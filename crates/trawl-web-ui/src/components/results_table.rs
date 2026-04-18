// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<ResultsTable/>` — paginated snapshot results table.

use leptos::prelude::*;
use trawl_api::QueryResponse;
use trawl_api::display::value_to_string;

use crate::api::{ApiError, PAGE_SIZE};
use crate::state::query::go_to;

#[component]
pub fn ResultsTable(
    #[prop(into)] executed_q: Signal<String>,
    #[prop(into)] page: Signal<usize>,
    rows: LocalResource<Result<QueryResponse, ApiError>>,
) -> impl IntoView {
    view! {
        <div class="results">
            {move || match rows.get() {
                None => view! { <div class="results-loading">"loading…"</div> }.into_any(),
                Some(Ok(resp)) => view! {
                    <ResultsTableBody resp=resp executed_q=executed_q page=page/>
                }.into_any(),
                Some(Err(err)) => view! {
                    <div class="results-error">
                        {format!("query failed: {err}")}
                    </div>
                }.into_any(),
            }}
        </div>
    }
}

#[component]
fn ResultsTableBody(
    resp: QueryResponse,
    executed_q: Signal<String>,
    page: Signal<usize>,
) -> impl IntoView {
    let columns: Vec<String> = resp.result.columns.iter().map(|c| c.name.clone()).collect();
    let rows_data = resp.result.rows.clone();
    let returned = resp.pagination.returned;
    let truncated = resp.truncated;

    if columns.is_empty() {
        return view! {
            <div class="results-empty">"no results yet — type a query and press ⌘⏎"</div>
        }
        .into_any();
    }

    let has_rows = !rows_data.is_empty();
    let cur_page = page.get();
    let can_prev = cur_page > 0;
    let can_next = returned == PAGE_SIZE;

    let on_prev = move |_| {
        if can_prev {
            go_to(&executed_q.get_untracked(), cur_page - 1, true);
        }
    };
    let on_next = move |_| {
        if can_next {
            go_to(&executed_q.get_untracked(), cur_page + 1, true);
        }
    };

    view! {
        <>
            <div class="results-table-wrap">
                <table class="results-table">
                    <thead>
                        <tr>
                            {columns.iter().cloned().map(|name| view! {
                                <th>{name}</th>
                            }).collect::<Vec<_>>()}
                        </tr>
                    </thead>
                    <tbody>
                        {if has_rows {
                            rows_data.iter().map(|row| {
                                let cells = row.iter().map(|v| {
                                    let s = value_to_string(v);
                                    view! { <td>{s}</td> }
                                }).collect::<Vec<_>>();
                                view! { <tr>{cells}</tr> }.into_any()
                            }).collect::<Vec<_>>()
                        } else {
                            let cols_len = columns.len();
                            vec![view! {
                                <tr>
                                    <td class="results-empty-cell" colspan=cols_len>
                                        "no rows"
                                    </td>
                                </tr>
                            }.into_any()]
                        }}
                    </tbody>
                </table>
            </div>
            <footer class="results-footer">
                <span class="results-summary">
                    {format!(
                        "page {} · showing {} {}",
                        cur_page + 1,
                        returned,
                        if returned == 1 { "row" } else { "rows" },
                    )}
                    {if truncated { " (truncated)" } else { "" }}
                </span>
                <div class="results-pager">
                    <button class="btn-sm" disabled=!can_prev on:click=on_prev>"← prev"</button>
                    <button class="btn-sm" disabled=!can_next on:click=on_next>"next →"</button>
                </div>
            </footer>
        </>
    }
    .into_any()
}
