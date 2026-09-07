// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<ResultsTable/>` — paginated snapshot results table.
//!
//! Sans-serif column headers with a sort affordance, expandable rows that
//! reveal a `_time` / field tag detail panel with Copy _raw / Show context /
//! Find similar action buttons. Detail-row tag clicks add filters through a
//! parent-supplied callback.

use crate::api::{ApiError, PAGE_SIZE};
use crate::context_query::{build_context_query, escape_dq, find_col};
use crate::state::query::{Filter, FilterOp};
use fleet_ui::{Btn, CopyButton, LoadState, Loaded, Pager, ToastBus, ToastKind, Variant};
use leptos::prelude::*;
use std::cmp::Ordering;
use trawl_api::QueryResponse;
use trawl_api::display::value_to_string;

use crate::severity_cell::{severity_class, severity_columns, severity_display};
use trawl_api::value::Value;

#[component]
pub fn ResultsTable(
    #[prop(into)] page: Signal<usize>,
    rows: LocalResource<Result<QueryResponse, ApiError>>,
    /// Called with the new page index when prev/next is clicked. Parent
    /// captures a router navigator and translates to URL navigation.
    on_paginate: Callback<usize>,
    /// Called when a detail-row field tag is clicked — adds an include
    /// filter for that `field = value`.
    on_add_filter: Callback<Filter>,
    /// Navigate to a fresh search with the given DSL query and default
    /// filters/range. Used by "Show context" and "Find similar".
    on_navigate: Callback<String>,
) -> impl IntoView {
    let bus = expect_context::<ToastBus>();
    view! {
        <div class="results">
            <Loaded
                state=Signal::derive(move || LoadState::from_resource(rows.get()))
                label="results"
                render=Box::new(move |resp: QueryResponse| view! {
                    <ResultsTableBody
                        resp=resp
                        page=page
                        on_paginate=on_paginate
                        on_add_filter=on_add_filter
                        on_navigate=on_navigate
                        bus=bus
                    />
                }.into_any())
            />
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
    on_add_filter: Callback<Filter>,
    on_navigate: Callback<String>,
    bus: ToastBus,
) -> impl IntoView {
    let columns: Vec<String> = resp.result.columns.iter().map(|c| c.name.clone()).collect();
    let rows_data = resp.result.rows.clone();
    let returned = resp.pagination.returned;
    let truncated = resp.truncated;

    if columns.is_empty() {
        return view! {
            <div class="results-empty">"No fish in this net yet — type a query and press ⌘⏎"</div>
        }
        .into_any();
    }

    // Cells render as severity tokens for `_severity` plus the columns the
    // response declares (`sev()` output). A bare `severity` column is ordinary
    // sender data, so it renders like any other field.
    let severity_cols =
        severity_columns(columns.iter().map(String::as_str), &resp.severity_columns);
    let expanded = RwSignal::new(None::<usize>);
    let sort = RwSignal::new(None::<SortState>);

    let cols_for_header = columns.clone();
    let cols_for_view = columns.clone();
    let header_cells = cols_for_header.iter().enumerate().map(|(i, name)| {
        let name = name.clone();
        view! {
            // A real `<table>`, so direction is announced by `aria-sort`
            // on the sorted `<th>` alone (ADR-0029) and the glyph is
            // decoration. The control is the button inside, never the cell.
            <th
                class="sortable"
                class:sorted=move || sort.get().is_some_and(|s| s.col == i)
                aria-sort=move || {
                    sort.get()
                        .filter(|s| s.col == i)
                        .map(|s| if s.asc { "ascending" } else { "descending" })
                }
            >
                <button
                    type="button"
                    class="th-sort"
                    on:click=move |_| sort.update(|cur| {
                        *cur = match *cur {
                            Some(s) if s.col == i => Some(SortState { col: i, asc: !s.asc }),
                            _ => Some(SortState { col: i, asc: false }),
                        };
                    })
                >
                    <span>{name}</span>
                    <span class="sort" aria-hidden="true">{move || {
                        sort.get().filter(|s| s.col == i).map_or("·", |s| if s.asc { "▲" } else { "▼" })
                    }}</span>
                </button>
            </th>
        }
    }).collect::<Vec<_>>();

    let has_rows = !rows_data.is_empty();
    let sorted_indices = SortedIndices::new(&rows_data, sort);
    let cur_page = page.get();
    let can_prev = cur_page > 0;
    let can_next = returned == PAGE_SIZE;

    let on_prev = Callback::new(move |()| {
        if can_prev {
            on_paginate.run(cur_page - 1);
        }
    });
    let on_next = Callback::new(move |()| {
        if can_next {
            on_paginate.run(cur_page + 1);
        }
    });

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
                            // Closure, not a bare block: row rendering must
                            // re-run when the sort memo changes.
                            (move || sorted_indices.render(
                                rows_data.clone(),
                                cols_for_view.clone(),
                                severity_cols.clone(),
                                expanded,
                                on_add_filter,
                                on_navigate,
                                bus,
                            )).into_any()
                        } else {
                            let cols_len = columns.len() + 1;
                            view! {
                                <tr>
                                    <td class="results-empty-cell" colspan=cols_len>
                                        "No fish in this net yet"
                                    </td>
                                </tr>
                            }.into_any()
                        }}
                    </tbody>
                </table>
            </div>
            <Pager
                summary=format!(
                    "Page {} · showing {} {}{}",
                    cur_page + 1,
                    returned,
                    if returned == 1 { "row" } else { "rows" },
                    if truncated { " (truncated)" } else { "" },
                )
                can_prev=Signal::from(can_prev)
                can_next=Signal::from(can_next)
                on_prev=on_prev
                on_next=on_next
            />
        </>
    }
    .into_any()
}

/// Sort indirection: the table renders rows by index, applying the
/// current sort lazily on each render. Avoids cloning rows.
#[derive(Clone, Copy)]
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

    #[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
    fn render(
        &self,
        rows: Vec<Vec<Value>>,
        columns: Vec<String>,
        severity_cols: Vec<usize>,
        expanded: RwSignal<Option<usize>>,
        on_add_filter: Callback<Filter>,
        on_navigate: Callback<String>,
        bus: ToastBus,
    ) -> Vec<leptos::prelude::AnyView> {
        let indices = self.indices.get();
        indices
            .into_iter()
            .map(|i| {
                let row = rows[i].clone();
                let cols = columns.clone();
                let sev_cols = severity_cols.clone();
                view! {
                    <RowFragment
                        idx=i
                        row=row
                        columns=cols
                        severity_cols=sev_cols
                        expanded=expanded
                        on_add_filter=on_add_filter
                        on_navigate=on_navigate
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
    severity_cols: Vec<usize>,
    expanded: RwSignal<Option<usize>>,
    on_add_filter: Callback<Filter>,
    on_navigate: Callback<String>,
    bus: ToastBus,
) -> impl IntoView {
    let cells_row = row.clone();
    let cells = cells_row
        .iter()
        .enumerate()
        .map(|(ci, v)| {
            if severity_cols.contains(&ci) {
                // Display shows the token; the wire (json/csv/SSE) keeps
                // the number for arithmetic consumers.
                let s = severity_display(v);
                let cls = severity_class(v);
                view! { <td><span class=cls>{s}</span></td> }.into_any()
            } else {
                view! { <td>{value_to_string(v)}</td> }.into_any()
            }
        })
        .collect::<Vec<_>>();

    let columns_for_detail = columns.clone();
    let row_for_detail = row.clone();
    let columns_for_actions = columns.clone();
    let row_for_actions = row.clone();

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
                                let field_for_click = name.clone();
                                let value_for_click = value_text.clone();
                                view! {
                                    <span class="k">{key}</span>
                                    <span class="v">
                                        <span
                                            class="tag"
                                            on:click=move |e| {
                                                e.stop_propagation();
                                                on_add_filter.run(Filter {
                                                    field: field_for_click.clone(),
                                                    value: value_for_click.clone(),
                                                    op: FilterOp::Include,
                                                });
                                            }
                                        >{value_text}</span>
                                    </span>
                                }
                            }).collect::<Vec<_>>()}
                        </div>
                        <div class="actions">
                            <CopyRawButton
                                row=row_for_actions.clone()
                                columns=columns_for_actions.clone()
                            />
                            <ShowContextButton
                                row=row_for_actions.clone()
                                columns=columns_for_actions.clone()
                                on_navigate=on_navigate
                                bus=bus
                            />
                            <FindSimilarButton
                                row=row_for_actions.clone()
                                columns=columns_for_actions.clone()
                                on_navigate=on_navigate
                                bus=bus
                            />
                        </div>
                    </td>
                </tr>
            </Show>
        </>
    }
}

#[component]
fn CopyRawButton(row: Vec<Value>, columns: Vec<String>) -> impl IntoView {
    // The derived signal keeps `raw_or_synthesized` lazy: it runs at click
    // time, not on every render.
    let text = Signal::derive(move || raw_or_synthesized(&row, &columns));
    view! {
        <CopyButton text=text success_detail="Raw event copied to clipboard.">
            "Copy _raw"
        </CopyButton>
    }
}

#[component]
fn ShowContextButton(
    row: Vec<Value>,
    columns: Vec<String>,
    on_navigate: Callback<String>,
    bus: ToastBus,
) -> impl IntoView {
    let on_click = Callback::new(move |()| match build_context_query(&row, &columns) {
        Some(q) => on_navigate.run(q),
        None => bus.push(
            ToastKind::Info,
            "Show context",
            Some("Need a timestamp column to build a context window.".into()),
        ),
    });
    view! {
        <Btn variant=Variant::Secondary stop_propagation=true on_click=on_click>"Show context"</Btn>
    }
}

#[component]
fn FindSimilarButton(
    row: Vec<Value>,
    columns: Vec<String>,
    on_navigate: Callback<String>,
    bus: ToastBus,
) -> impl IntoView {
    let on_click = Callback::new(move |()| match build_similar_query(&row, &columns) {
        Some(q) => on_navigate.run(q),
        None => bus.push(
            ToastKind::Info,
            "Find similar",
            Some("Need a message column to find similar events.".into()),
        ),
    });
    view! {
        <Btn variant=Variant::Secondary stop_propagation=true on_click=on_click>"Find similar"</Btn>
    }
}

fn raw_or_synthesized(row: &[Value], columns: &[String]) -> String {
    if let Some(idx) = columns.iter().position(|c| c == "_raw" || c == "raw")
        && let Some(v) = row.get(idx)
    {
        return value_to_string(v);
    }
    // Fallback: join "key=value" pairs so the user still gets something
    // copy-pastable.
    columns
        .iter()
        .zip(row.iter())
        .map(|(k, v)| format!("{k}={}", value_to_string(v)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Build a phrase-match query on the first ~60 chars of the row's message.
fn build_similar_query(row: &[Value], columns: &[String]) -> Option<String> {
    let mi = find_col(columns, &["message", "msg"])?;
    let msg = value_to_string(row.get(mi)?);
    let trimmed = msg.trim();
    if trimmed.is_empty() {
        return None;
    }
    let take: String = trimmed.chars().take(60).collect();
    Some(format!("\"{}\"", escape_dq(&take)))
}

fn compare(a: Option<&Value>, b: Option<&Value>) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, _) => Ordering::Less,
        (_, None) => Ordering::Greater,
        (Some(x), Some(y)) => value_to_string(x).cmp(&value_to_string(y)),
    }
}
