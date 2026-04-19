// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<ResultsTable/>` — paginated snapshot results table.
//!
//! Restyled for the v0.14 design: 10px uppercase column headers with
//! a right-aligned sort affordance, expandable rows that reveal a
//! `_time` / field tag detail panel, level pills, and click-to-filter
//! tags inside the detail panel (currently stubbed → toast).

use leptos::prelude::*;
use std::cmp::Ordering;
use trawl_api::QueryResponse;
use trawl_api::display::value_to_string;
use trawl_api::value::Value;

use crate::api::{ApiError, PAGE_SIZE};
use crate::components::toast::{ToastBus, ToastKind};

#[component]
pub fn ResultsTable(
    #[prop(into)] page: Signal<usize>,
    rows: LocalResource<Result<QueryResponse, ApiError>>,
    /// Called with the new page index when prev/next is clicked. Parent
    /// captures a router navigator and translates to URL navigation.
    on_paginate: Callback<usize>,
    bus: ToastBus,
) -> impl IntoView {
    view! {
        <div class="results">
            {move || match rows.get() {
                None => view! { <div class="results-loading">"loading…"</div> }.into_any(),
                Some(Ok(resp)) => view! {
                    <ResultsTableBody
                        resp=resp
                        page=page
                        on_paginate=on_paginate
                        bus=bus
                    />
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SortState {
    col: usize,
    asc: bool,
}

#[component]
fn ResultsTableBody(
    resp: QueryResponse,
    page: Signal<usize>,
    on_paginate: Callback<usize>,
    bus: ToastBus,
) -> impl IntoView {
    let columns: Vec<String> = resp.result.columns.iter().map(|c| c.name.clone()).collect();
    let rows_data = resp.result.rows.clone();
    let returned = resp.pagination.returned;
    let truncated = resp.truncated;

    if columns.is_empty() {
        return view! {
            <div class="results-empty">"no fish in this net yet — type a query and press ⌘⏎"</div>
        }
        .into_any();
    }

    let level_idx = columns.iter().position(|c| c == "level");
    let expanded = RwSignal::new(None::<usize>);
    let sort = RwSignal::new(None::<SortState>);

    let cols_for_header = columns.clone();
    let cols_for_view = columns.clone();
    let header_cells = cols_for_header.iter().enumerate().map(|(i, name)| {
        let name = name.clone();
        view! {
            <th
                class:sorted=move || sort.get().is_some_and(|s| s.col == i)
                on:click=move |_| sort.update(|cur| {
                    *cur = match *cur {
                        Some(s) if s.col == i => Some(SortState { col: i, asc: !s.asc }),
                        _ => Some(SortState { col: i, asc: false }),
                    };
                })
            >
                <span>{name}</span>
                <span class="sort">{move || {
                    sort.get().filter(|s| s.col == i).map_or("·", |s| if s.asc { "▲" } else { "▼" })
                }}</span>
            </th>
        }
    }).collect::<Vec<_>>();

    let has_rows = !rows_data.is_empty();
    let cur_page = page.get();
    let can_prev = cur_page > 0;
    let can_next = returned == PAGE_SIZE;

    let on_prev = move |_| {
        if can_prev {
            on_paginate.run(cur_page - 1);
        }
    };
    let on_next = move |_| {
        if can_next {
            on_paginate.run(cur_page + 1);
        }
    };

    view! {
        <>
            <div class="results-table-wrap">
                <table class="results-table">
                    <thead>
                        <tr>
                            <th class="exp-col"></th>
                            {header_cells}
                        </tr>
                    </thead>
                    <tbody>
                        {if has_rows {
                            let sorted_indices = SortedIndices::new(&rows_data, sort);
                            sorted_indices.render(rows_data.clone(), cols_for_view, level_idx, expanded, bus)
                        } else {
                            let cols_len = columns.len() + 1;
                            vec![view! {
                                <tr>
                                    <td class="results-empty-cell" colspan=cols_len>
                                        "no fish in this net yet"
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

/// Sort indirection: the table renders rows by index, applying the
/// current sort lazily on each render. Avoids cloning rows.
struct SortedIndices {
    indices: Memo<Vec<usize>>,
}

impl SortedIndices {
    fn new(rows: &[Vec<Value>], sort: RwSignal<Option<SortState>>) -> Self {
        let snapshot = rows.to_vec();
        let indices = Memo::new(move |_| {
            let mut idx: Vec<usize> = (0..snapshot.len()).collect();
            if let Some(s) = sort.get() {
                idx.sort_by(|&a, &b| {
                    let ord = compare(snapshot[a].get(s.col), snapshot[b].get(s.col));
                    if s.asc { ord } else { ord.reverse() }
                });
            }
            idx
        });
        Self { indices }
    }

    #[allow(clippy::needless_pass_by_value)] // owned vecs are cloned per row anyway
    fn render(
        &self,
        rows: Vec<Vec<Value>>,
        columns: Vec<String>,
        level_idx: Option<usize>,
        expanded: RwSignal<Option<usize>>,
        bus: ToastBus,
    ) -> Vec<leptos::prelude::AnyView> {
        let indices = self.indices.get();
        indices
            .into_iter()
            .map(|i| {
                let row = rows[i].clone();
                let cols = columns.clone();
                view! {
                    <RowFragment
                        idx=i
                        row=row
                        columns=cols
                        level_idx=level_idx
                        expanded=expanded
                        bus=bus
                    />
                }
                .into_any()
            })
            .collect()
    }
}

#[component]
fn RowFragment(
    idx: usize,
    row: Vec<Value>,
    columns: Vec<String>,
    level_idx: Option<usize>,
    expanded: RwSignal<Option<usize>>,
    bus: ToastBus,
) -> impl IntoView {
    let cells_row = row.clone();
    let cells = cells_row
        .iter()
        .enumerate()
        .map(|(ci, v)| {
            if Some(ci) == level_idx {
                let s = value_to_string(v);
                let cls = level_class(&s);
                view! { <td><span class=cls>{s}</span></td> }.into_any()
            } else {
                view! { <td>{value_to_string(v)}</td> }.into_any()
            }
        })
        .collect::<Vec<_>>();

    let columns_for_detail = columns.clone();
    let row_for_detail = row.clone();

    view! {
        <>
            <tr
                class:expanded=move || expanded.get() == Some(idx)
                on:click=move |_| expanded.update(|cur| {
                    *cur = if *cur == Some(idx) { None } else { Some(idx) };
                })
            >
                <td class="exp-col">
                    {move || if expanded.get() == Some(idx) { "▾" } else { "▸" }}
                </td>
                {cells}
            </tr>
            <Show when=move || expanded.get() == Some(idx)>
                <tr>
                    <td class="detail" colspan=columns_for_detail.len() + 1>
                        <div class="dg">
                            {columns_for_detail.iter().zip(row_for_detail.iter()).map(|(name, v)| {
                                let key = name.clone();
                                let value_text = value_to_string(v);
                                let key_for_toast = name.clone();
                                let val_for_toast = value_text.clone();
                                view! {
                                    <span class="k">{key}</span>
                                    <span class="v">
                                        <span
                                            class="tag"
                                            on:click=move |_| bus.push(
                                                ToastKind::Info,
                                                "Click-to-filter",
                                                Some(format!(
                                                    "{key_for_toast} = {val_for_toast} — coming soon"
                                                )),
                                            )
                                        >{value_text}</span>
                                    </span>
                                }
                            }).collect::<Vec<_>>()}
                        </div>
                    </td>
                </tr>
            </Show>
        </>
    }
}

fn level_class(s: &str) -> &'static str {
    match s.to_ascii_lowercase().as_str() {
        "error" | "err" | "fatal" | "critical" => "lvl lvl-error",
        "warn" | "warning" => "lvl lvl-warn",
        "info" => "lvl lvl-info",
        "debug" | "trace" => "lvl lvl-debug",
        _ => "lvl",
    }
}

fn compare(a: Option<&Value>, b: Option<&Value>) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, _) => Ordering::Less,
        (_, None) => Ordering::Greater,
        (Some(x), Some(y)) => value_to_string(x).cmp(&value_to_string(y)),
    }
}
