// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<HistoryPage/>` — full-page query history browser.
//!
//! Lives inside the Search app shell at `/search/history`.
//! Pulls from `GET /api/v1/history` (paginated) and lets the user:
//! - filter rows client-side by substring on the query text
//! - click a row to reload the query into the search editor
//! - "Save as Net" via a modal dialog → `POST /api/v1/saved`
//!
//! URL params: `hpage=N` drives pagination (separate from the search
//! page's `?page=` so switching sections leaves a clean history URL).

use leptos::prelude::*;
use leptos_router::NavigateOptions;
use leptos_router::hooks::{use_navigate, use_query_map};
use trawl_api::HistoryEntryResponse;

use crate::api;
use crate::components::save_as_net_modal::SaveAsNetModal;
use crate::state::query::{Mode, RangeSpec, navigator, report_refusal};
use fleet_ui::time::format_duration;
use fleet_ui::{Btn, LoadState, Loaded, Pager, SearchInput, ToastBus, ToastKind, Variant, When};

/// Rows per page — the server caps at 1000 but 50 matches the results
/// table's page size, so the paginator feels familiar.
const PAGE_SIZE: usize = 50;

#[component]
#[allow(clippy::too_many_lines)] // page-level component: header + table + footer
pub fn HistoryPage() -> impl IntoView {
    let bus = expect_context::<ToastBus>();
    let qm = use_query_map();
    let hpage = Memo::new(move |_| {
        qm.get()
            .get("hpage")
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0)
    });
    let filter = RwSignal::new(String::new());

    let resource = LocalResource::new(move || async move {
        api::history(PAGE_SIZE, hpage.get() * PAGE_SIZE).await
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
        bus.push(
            ToastKind::Info,
            "Export",
            Some("History export is landing soon.".into()),
        );
    });
    let on_clear = Callback::new(move |()| {
        bus.push(
            ToastKind::Info,
            "Clear history",
            Some("Server-side history clearing is landing soon.".into()),
        );
    });

    let on_prev = {
        let goto_hpage = goto_hpage.clone();
        Callback::new(move |()| {
            let cur = hpage.get_untracked();
            if cur > 0 {
                goto_hpage(
                    &format!("/search/history?hpage={}", cur - 1),
                    NavigateOptions {
                        replace: true,
                        ..Default::default()
                    },
                );
            }
        })
    };
    let on_next = {
        let goto_hpage = goto_hpage.clone();
        Callback::new(move |()| {
            let cur = hpage.get_untracked();
            goto_hpage(
                &format!("/search/history?hpage={}", cur + 1),
                NavigateOptions {
                    replace: true,
                    ..Default::default()
                },
            );
        })
    };

    view! {
        <div class="page">
            <div class="page-hd compact">
                <div>
                    <h1>"Search history"</h1>
                    <p class="sub">"Every query you've run. Re-cast the net anytime or create a scheduled search."</p>
                </div>
                <div class="actions">
                    <SearchInput value=filter placeholder="Filter history…"/>
                    <Btn variant=Variant::Secondary on_click=on_export>"Export"</Btn>
                    <Btn variant=Variant::Secondary on_click=on_clear>"Clear history"</Btn>
                </div>
            </div>

            <div class="tbl">
                <div class="tbl-hd">
                    <div style="flex:0 0 72px">"When"</div>
                    <div style="flex:3; min-width:0">"Query"</div>
                    <div style="flex:0 0 60px; text-align:right">"Events"</div>
                    <div style="flex:0 0 60px; text-align:right">"Duration"</div>
                    <div style="flex:0 0 72px; text-align:right"></div>
                </div>
                <div class="tbl-body">
                <Loaded
                    state=Signal::derive(move || LoadState::from_resource(resource.get()))
                    label="history"
                    render=Box::new(move |resp: trawl_api::HistoryResponse| {
                        let needle = filter.get().to_lowercase();
                        let filtered: Vec<HistoryEntryResponse> = resp
                            .entries
                            .iter()
                            .filter(|h| needle.is_empty() || h.query.to_lowercase().contains(&needle))
                            .cloned()
                            .collect();

                        if filtered.is_empty() {
                            return view! {
                                <div class="tbl-row" style="cursor:default">
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

                        filtered.into_iter().map(|h| {
                            let q_for_row = h.query.clone();
                            let q_for_save = h.query.clone();
                            let on_rerun = on_rerun.clone();
                            let events = format_with_commas(h.row_count as u64);
                            let duration = format_duration(h.duration_ms);
                            // `<When>` ticks off fleet-ui's shared 30s
                            // clock, so the label doesn't freeze at page
                            // load.
                            let executed_at = h.executed_at.clone();
                            view! {
                                <div class="tbl-row">
                                    <div style="flex:0 0 72px; color:var(--ink-3)" class="mono">
                                        <When ts=executed_at/>
                                    </div>
                                    <div style="flex:3; min-width:0" class="mono path">
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
                                    </div>
                                    <div style="flex:0 0 60px; text-align:right" class="mono">
                                        {events}
                                    </div>
                                    <div style="flex:0 0 60px; text-align:right" class="mono">
                                        {duration}
                                    </div>
                                    <div style="flex:0 0 72px; text-align:right">
                                        <button
                                            type="button"
                                            class="link"
                                            on:click=move |_| on_save_as_net(q_for_save.clone())
                                        >"Save as Net"</button>
                                    </div>
                                </div>
                            }
                        }).collect::<Vec<_>>().into_any()
                    })
                />
                </div>

                {move || {
                    let Some(Ok(resp)) = resource.get() else {
                        return ().into_any();
                    };
                    let cur = hpage.get();
                    let total = resp.total;
                    let last_page_idx = total.saturating_sub(1) / PAGE_SIZE;
                    let can_prev = cur > 0;
                    let can_next = cur < last_page_idx;
                    let summary = if total == 0 {
                        "0 entries".to_string()
                    } else {
                        let first = cur * PAGE_SIZE + 1;
                        let last = (cur * PAGE_SIZE + resp.entries.len()).min(total);
                        format!("{first}–{last} of {total}")
                    };
                    view! {
                        <Pager
                            summary=summary
                            can_prev=Signal::from(can_prev)
                            can_next=Signal::from(can_next)
                            on_prev=on_prev
                            on_next=on_next
                        />
                    }.into_any()
                }}
            </div>

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
