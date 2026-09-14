// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<HistoryPage/>` — full-page query history browser.
//!
//! Lives inside the Search app shell at `/search/history`.
//! Pulls from `GET /api/v1/history` (paginated) and lets the user:
//! - filter rows client-side by substring on the query text
//! - click a row to reload the query into the search editor
//! - "Save as net" via a modal dialog → `POST /api/v1/saved`
//! - export the filtered loaded page as CSV or JSON
//! - confirm deletion of every history row owned by the current key
//!
//! URL params: `hpage=N` drives pagination (separate from the search
//! page's `?page=` so switching sections leaves a clean history URL).

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::NavigateOptions;
use leptos_router::hooks::{use_location, use_navigate};
use trawl_api::HistoryEntryResponse;

use crate::api;
use crate::components::save_as_net_modal::SaveAsNetModal;
use crate::download::trigger_download;
use crate::history_export::{HistoryExportFormat, serialize_history};
use crate::search_url::{PAGE_SIZE, read_history_page};
use crate::state::query::{Mode, RangeSpec, navigator, report_refusal};
use fleet_ui::{
    Btn, ConfirmModal, ConfirmState, LoadState, Loaded, OffsetPager, PageTotal, PageWindow,
    SearchInput, ToastBus, ToastKind, Variant, When,
};
use std::num::NonZeroUsize;

#[component]
#[allow(clippy::too_many_lines)] // page-level component: header + table + footer
pub fn HistoryPage() -> impl IntoView {
    let table_viewport = NodeRef::<leptos::html::Div>::new();
    let bus = expect_context::<ToastBus>();
    let location = use_location();
    let hpage = Memo::new(move |_| read_history_page(&location.search.get()));
    let filter = RwSignal::new(String::new());
    let size = NonZeroUsize::new(PAGE_SIZE).expect("history page size is nonzero");

    let loading = RwSignal::new(true);
    let load_generation = RwSignal::new(0_u64);
    let loaded_page = RwSignal::new(None::<usize>);
    let resource = LocalResource::new(move || {
        // Track the URL directly: a display memo can consume a queued change
        // while the resource is awaiting the previous page.
        let requested = read_history_page(&location.search.get());
        let generation = load_generation.get_untracked().wrapping_add(1);
        load_generation.set(generation);
        loading.set(true);
        async move {
            let response = async {
                let page = requested.map_err(api::ApiError::Refused)?;
                let offset = PageWindow::checked_offset(page, size).map_err(|_| {
                    api::ApiError::Refused("This history page is too large to request.")
                })?;
                api::history(PAGE_SIZE, offset)
                    .await
                    .map(|resp| (page, resp))
            }
            .await;
            if load_generation.try_get_untracked() == Some(generation) {
                loaded_page.set(response.as_ref().ok().map(|(page, _)| *page));
                loading.set(false);
            }
            response
        }
    });

    let filtered_rows = Signal::derive(move || {
        let Some(Ok((_, resp))) = resource.get() else {
            return Vec::new();
        };
        let needle = filter.get().to_lowercase();
        resp.entries
            .iter()
            .filter(|h| needle.is_empty() || h.query.to_lowercase().contains(&needle))
            .cloned()
            .collect::<Vec<HistoryEntryResponse>>()
    });
    let export_format = RwSignal::new(HistoryExportFormat::Csv);
    let clearing = RwSignal::new(false);
    let confirm_clear = RwSignal::new(ConfirmState::<()>::default());
    let export_disabled = Signal::derive(move || {
        clearing.get()
            || loading.get()
            || loaded_page.get() != hpage.get().ok()
            || filtered_rows.get().is_empty()
    });

    // Captured up-front — navigate() panics outside the <Router> context
    // (i.e. inside any deferred callback).
    let goto_search = navigator();
    let goto_hpage = use_navigate();

    let on_rerun = {
        let goto_search = goto_search.clone();
        move |q: String| {
            // A stored query long enough to bust the link bound is
            // refused rather than rerun into a banner (ADR-0027).
            report_refusal(
                bus,
                goto_search(&q, 0, Mode::Snapshot, &[], &RangeSpec::default(), false),
            );
        }
    };

    // Modal state: `Some(query)` when the save dialog is open for that
    // query; `None` when closed.
    let save_target = RwSignal::new(None::<String>);
    let on_save_as_net = move |q: String| save_target.set(Some(q));
    let on_modal_close: Callback<bool> = Callback::new(move |_saved| save_target.set(None));

    let on_export = Callback::new(move |()| {
        if export_disabled.get_untracked() {
            return;
        }
        let format = export_format.get_untracked();
        let result = serialize_history(&filtered_rows.get_untracked(), format)
            .map_err(|e| e.to_string())
            .and_then(|bytes| trigger_download(&bytes, format.filename(), format.mime()));
        if let Err(error) = result {
            bus.push(ToastKind::Error, "Export failed", Some(error));
        }
    });
    let on_clear = Callback::new(move |()| {
        if !clearing.get_untracked() {
            confirm_clear.update(|state| state.request(()));
        }
    });
    let on_confirm_clear = {
        let goto_hpage = goto_hpage.clone();
        Callback::new(move |()| {
            if clearing.get_untracked()
                || confirm_clear
                    .try_update(ConfirmState::take)
                    .flatten()
                    .is_none()
            {
                return;
            }
            clearing.set(true);
            let goto_hpage = goto_hpage.clone();
            spawn_local(async move {
                let result = api::clear_history().await;
                // The request may finish after History has unmounted.
                if clearing.is_disposed() {
                    return;
                }
                match result {
                    Ok(response) => {
                        loading.set(true);
                        filter.set(String::new());
                        // The resource tracks raw search identity. Canonicalizing
                        // any query string triggers its own offset-zero read; only
                        // the already-canonical URL needs an explicit refetch.
                        let already_canonical = location.search.get_untracked().is_empty();
                        goto_hpage(
                            "/search/history",
                            NavigateOptions {
                                replace: true,
                                ..Default::default()
                            },
                        );
                        if already_canonical {
                            resource.refetch();
                        }
                        bus.push(
                            ToastKind::Success,
                            "History cleared",
                            Some(format!("Deleted {} history entries.", response.deleted)),
                        );
                    }
                    Err(error) => {
                        bus.push(ToastKind::Error, "Clear failed", Some(error.to_string()));
                    }
                }
                clearing.set(false);
            });
        })
    };

    let on_page = Callback::new(move |page: usize| {
        if clearing.get_untracked() {
            return;
        }
        loading.set(true);
        goto_hpage(
            &format!("/search/history?hpage={page}"),
            NavigateOptions {
                replace: true,
                ..Default::default()
            },
        );
    });

    view! {
        <div class="page history-page">
            <div class="page-hd compact">
                <div>
                    <h1>"Search history"</h1>
                    <p class="sub">"Every query you've run. Re-cast the net anytime or create a scheduled search."</p>
                </div>
                <div class="actions">
                    <select
                        class="btn-sec"
                        aria-label="History export format"
                        disabled=move || export_disabled.get()
                        on:change=move |event| export_format.set(
                            if event_target_value(&event) == "json" {
                                HistoryExportFormat::Json
                            } else {
                                HistoryExportFormat::Csv
                            }
                        )
                    >
                        <option value="csv">"CSV"</option>
                        <option value="json">"JSON"</option>
                    </select>
                    <Btn variant=Variant::Secondary disabled=export_disabled on_click=on_export>"Export this page"</Btn>
                    <Btn variant=Variant::Secondary disabled=clearing on_click=on_clear>"Clear history"</Btn>
                </div>
            </div>

            <div class="page-split">
            <section class="list-sheet" aria-labelledby="history-sheet-title">
                <div class="list-sheet-hd">
                    <h2 id="history-sheet-title" class="list-sheet-ttl">
                        "Recent searches"<span class="cnt">{move || filtered_rows.get().len()}</span>
                    </h2>
                    <SearchInput value=filter placeholder="Filter history…"/>
                </div>
            <fleet_ui::OverflowHint viewport=table_viewport/>
                <div node_ref=table_viewport class="tbl fleet-table-frame tbl-scroll" role="region" aria-label="Search history" tabindex="0" style="--list-min-width:560px">
                <div class="tbl-body">
                <Loaded
                    state=Signal::derive(move || LoadState::from_resource(
                        resource.get().map(|result| result.map(|(_, resp)| resp))
                    ))
                    label="history"
                    retry=Callback::new(move |()| { resource.set(None); resource.refetch(); })
                    render=Box::new(move |resp: trawl_api::HistoryResponse| {
                        let filtered = filtered_rows.get();

                        if filtered.is_empty() {
                            return view! {
                                    <div class="tbl-empty">
                                    <span class="mono" style="color:var(--ink-3)">
                                        {if resp.entries.is_empty() {
                                            "No queries yet — run one in /search to see it here"
                                        } else {
                                            "No history rows match that filter"
                                        }}
                                    </span>
                                </div>
                            }.into_any();
                        }

                            let rows = filtered.into_iter().map(|h| {
                            let q_for_row = h.query.clone();
                            let q_for_save = h.query.clone();
                            let on_rerun = on_rerun.clone();
                            let events = format_with_commas(h.row_count as u64);
                            // `<When>` ticks off fleet-ui's shared 30s
                            // clock, so the label doesn't freeze at page
                            // load.
                            let executed_at = h.executed_at.clone();
                            view! {
                                    <tr class="tbl-row">
                                        <td style="min-width:0" class="mono path">
                                        // A command, not a place: the rerun
                                        // goes through the navigator, which
                                        // REFUSES a stored query long enough
                                        // to bust the link bound (ADR-0027),
                                        // and an anchor has no way to say no.
                                        <button
                                            type="button"
                                            class="row-stretch"
                                            on:click=move |_| on_rerun(q_for_row.clone())
                                        >
                                            {h.query.clone()}
                                        </button>
                                        </td>
                                        <td style="color:var(--ink-3)" class="mono">
                                        <When ts=executed_at/>
                                        </td>
                                        <td style="text-align:right" class="mono">
                                        {events}
                                        </td>
                                        <td style="text-align:right">
                                        <button
                                            type="button"
                                            class="link"
                                            on:click=move |_| on_save_as_net(q_for_save.clone())
                                        >"Save as net"</button>
                                        </td>
                                    </tr>
                            }
                            }).collect::<Vec<_>>();
                            view! {
                                    <table class="fleet-table history-table" aria-label="Search history">
                                        <thead><tr>
                                            <th scope="col">"Executed query"</th>
                                            <th scope="col" style="width:92px">"When"</th>
                                            <th scope="col" style="width:80px; text-align:right">"Rows"</th>
                                            <th scope="col" style="width:100px; text-align:right">"Action"</th>
                                        </tr></thead>
                                        <tbody>{rows}</tbody></table>
                            }.into_any()
                    })
                />
                </div>

                {move || {
                    let Some(Ok((fetched_page, resp))) = resource.get() else {
                        return ().into_any();
                    };
                    let window = Signal::derive(move || PageWindow::new(
                        fetched_page, size, resp.entries.len(), PageTotal::Known(resp.total),
                        clearing.get() || loading.get() || hpage.get() != Ok(fetched_page),
                    ).expect("history response follows an admitted offset"));
                    view! { <OffsetPager window=window on_page=on_page/> }.into_any()
                }}
            </div>
            </section>
            </div>

            <Show when=move || confirm_clear.get().is_open()>
                <ConfirmModal
                    title="Clear history"
                    message="Delete all query history for your current key, across every page? Saved queries will remain. A query running now may add a new history entry after the clear.".to_owned()
                    confirm_label="Clear all history"
                    confirm_variant=Variant::Danger
                    on_confirm=on_confirm_clear
                    on_cancel=Callback::new(move |()| confirm_clear.update(ConfirmState::cancel))
                />
            </Show>

            {move || save_target.get().map(|q| view! {
                <SaveAsNetModal query=q on_close=on_modal_close/>
            })}
        </div>
    }
}

fn format_with_commas(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}
