// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<ExactTable/>` — the exact numbers behind an aggregate result.
//!
//! The same sortable `<table class="results-table">` the raw results
//! use, minus two things an aggregation has no honest version of:
//!
//! - **No expansion column.** An aggregate row has no underlying event
//!   to reveal; every value it holds is already on the row.
//! - **No Include on a generated metric.** `count`, `avg(…)` and their
//!   aliases name nothing the corpus holds, so a filter on one would
//!   advertise a capability that does not exist (ADR-0025, functional
//!   finding F02). Only the columns the query grouped by carry a
//!   "search this group" control, because those name real fields.
//!
//! Which columns those are comes from [`crate::categorical::group_columns`],
//! read off the query the response was executed for, never the editor
//! or the URL, so a page that has moved on cannot relabel these rows.
//!
//! While a request is in flight the rows on screen belong to the
//! PREVIOUS query. Every group control carries that query with it, so a
//! press files the filter against the query the row came from, never
//! the one still pending; the cells stay live throughout.

use crate::api::{ApiError, PAGE_SIZE};
use crate::categorical::group_columns;
use crate::fetch_plan::cap_line;
use crate::result_actions::{Capabilities, sorted_page};
use crate::state::query::{Filter, FilterOp};
use crate::state::search_session::{ExecutedQuery, ExecutedResponse};
use fleet_ui::{LoadState, Loaded, OffsetPager, PageTotal, PageWindow};
use leptos::prelude::*;
use trawl_api::QueryResponse;
use trawl_api::display::value_to_string;
use trawl_api::value::Value;

#[component]
pub fn ExactTable(
    #[prop(into)] page: Signal<usize>,
    rows: LocalResource<Result<ExecutedResponse, ApiError>>,
    #[prop(into)] busy: Signal<bool>,
    /// The column the reader sorted on, owned by the page: the bars
    /// beside this table slice the same sorted list, so the order
    /// cannot live privately in here.
    sort: RwSignal<Option<(usize, bool)>>,
    on_paginate: Callback<usize>,
    /// Called by a group column's "search this group" control, with the
    /// query the row came from.
    on_add_filter: Callback<(ExecutedQuery, Filter)>,
) -> impl IntoView {
    view! {
        // Same region, same name, same id as the raw results table: the
        // skip link and every spec that reaches for the results reach
        // this table on an aggregate page too.
        <div id="search-results" class="results" role="region" aria-label="Search results" tabindex="0">
            <Loaded
                state=Signal::derive(move || LoadState::from_resource(rows.get()))
                label="results"
                retry=Callback::new(move |()| { rows.set(None); rows.refetch(); })
                render=Box::new(move |resp: ExecutedResponse| view! {
                    <ExactTableBody
                        resp=resp.response
                        executed_query=resp.query
                        page=page
                        busy=busy
                        sort=sort
                        on_paginate=on_paginate
                        on_add_filter=on_add_filter
                    />
                }.into_any())
            />
        </div>
    }
}

#[component]
fn ExactTableBody(
    resp: QueryResponse,
    /// The query this response was executed for: the only thing that
    /// knows which columns were grouped and which were generated.
    executed_query: ExecutedQuery,
    page: Signal<usize>,
    busy: Signal<bool>,
    sort: RwSignal<Option<(usize, bool)>>,
    on_paginate: Callback<usize>,
    on_add_filter: Callback<(ExecutedQuery, Filter)>,
) -> impl IntoView {
    let columns: Vec<String> = resp.result.columns.iter().map(|c| c.name.clone()).collect();
    let rows_data = resp.result.rows.clone();
    // Every row the fetch brought back. Under `FetchPlan::Whole` this is
    // the whole result up to the fetch ceiling, and it is what the pager
    // counts: above the ceiling the pager describes the rows in hand and
    // the gap between them and `total` is the cap line's to say.
    let fetched = resp.result.rows.len();
    let cap = cap_line(resp.pagination.total, fetched);
    let group_cols = group_columns(&executed_query.effective, &columns);
    let capabilities = Capabilities::for_query(&executed_query.effective);

    let header_cells = columns
        .iter()
        .cloned()
        .enumerate()
        .map(|(i, name)| {
            // Same contract as the raw table: the visible text is the
            // column so the accessible name contains its own label
            // (WCAG 2.5.3), and the direction rides `aria-sort` on the
            // cell rather than being said twice.
            let sort_label = format!("Sort by {name}");
            view! {
                <th
                    class="sortable"
                    class:sorted=move || sort.get().is_some_and(|(col, _)| col == i)
                    aria-sort=move || {
                        sort.get()
                            .filter(|(col, _)| *col == i)
                            .map(|(_, asc)| if asc { "ascending" } else { "descending" })
                    }
                >
                    <button
                        type="button"
                        class="th-sort"
                        aria-label=sort_label
                        on:click=move |_| sort.update(|cur| {
                            *cur = match *cur {
                                Some((col, asc)) if col == i => Some((i, !asc)),
                                _ => Some((i, false)),
                            };
                        })
                    >
                        <span>{name}</span>
                        <span class="sort" aria-hidden="true">{move || {
                            sort.get()
                                .filter(|(col, _)| *col == i)
                                .map_or("·", |(_, asc)| if asc { "▲" } else { "▼" })
                        }}</span>
                    </button>
                </th>
            }
        })
        .collect::<Vec<_>>();

    // The page is cut here, not by the request: an aggregation arrives
    // whole and the reader walks it in the browser. Sorted whole first,
    // so a re-sort changes WHICH rows this page holds rather than
    // shuffling the ones already on it.
    let snapshot = rows_data.clone();
    let indices = Memo::new(move |_| sorted_page(&snapshot, sort.get(), page.get(), PAGE_SIZE));

    let col_count = columns.len();
    let body_rows = rows_data.clone();
    let body_cols = columns.clone();
    let window = Signal::derive(move || {
        let page = page.get();
        PageWindow::new(
            page,
            std::num::NonZeroUsize::new(PAGE_SIZE).expect("query page size is nonzero"),
            fetched
                .saturating_sub(page.saturating_mul(PAGE_SIZE))
                .min(PAGE_SIZE),
            PageTotal::Known(fetched),
            busy.get(),
        )
    });

    view! {
        <>
            <div class="results-table-wrap">
                <table class="results-table exact">
                    <thead>
                        <tr>{header_cells}</tr>
                    </thead>
                    <tbody>
                        // Closure, not a bare block: the rows must
                        // re-run when the sort or the page moves.
                        {move || {
                            let indices = indices.get();
                            if indices.is_empty() {
                                // An empty result, or a link naming a page
                                // past the fetched rows: either way there
                                // is nothing on this page to draw.
                                return view! {
                                    <tr>
                                        <td class="results-empty-cell" colspan=col_count>
                                            "No fish in this net yet"
                                        </td>
                                    </tr>
                                }.into_any();
                            }
                            indices.into_iter().map(|i| {
                                let cells = body_rows[i]
                                    .iter()
                                    .enumerate()
                                    .map(|(ci, v)| cell(ci, v, &body_cols, &group_cols, &capabilities, &executed_query, on_add_filter))
                                    .collect::<Vec<_>>();
                                view! { <tr>{cells}</tr> }
                            }).collect::<Vec<_>>().into_any()
                        }}
                    </tbody>
                </table>
            </div>
            <Loaded
                state=Signal::derive(move || match window.get() {
                    Ok(window) => LoadState::Ready(window),
                    Err(_) => LoadState::Error("This result page extends past the supported row range.".to_string()),
                })
                label="pagination"
                render=Box::new(move |window: PageWindow| view! {
                    <OffsetPager
                        window=Signal::from(window)
                        on_page=on_paginate
                    />
                }.into_any())
            />
            // Outside the slice on purpose: this line describes the
            // whole result, not the page. It appears only when the
            // execution produced more rows than one fetch could carry.
            {cap.map(|line| view! { <p class="results-cap">{line}</p> })}
        </>
    }
    .into_any()
}

/// One body cell: a search control on a grouped field, plain text on a
/// generated metric or on a value no filter can name (a null group, an
/// array).
#[allow(clippy::too_many_arguments)]
fn cell(
    ci: usize,
    value: &Value,
    columns: &[String],
    group_cols: &[usize],
    capabilities: &Capabilities,
    executed_query: &ExecutedQuery,
    on_add_filter: Callback<(ExecutedQuery, Filter)>,
) -> AnyView {
    let text = value_to_string(value);
    if !group_cols.contains(&ci) {
        return view! { <td>{text}</td> }.into_any();
    }
    let Some(field) = columns.get(ci).cloned() else {
        return view! { <td>{text}</td> }.into_any();
    };
    if !capabilities.include(&field, value) {
        return view! { <td>{text}</td> }.into_any();
    }
    let label = format!("Search {field} = {text}");
    let filter = StoredValue::new(Filter {
        field,
        value: text.clone(),
        op: FilterOp::Include,
    });
    let query = executed_query.clone();
    view! {
        <td>
            <button
                type="button"
                class="grp-search"
                aria-label=label
                on:click=move |_| on_add_filter.run((query.clone(), filter.get_value()))
            >{text}</button>
        </td>
    }
    .into_any()
}
