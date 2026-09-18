// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<HistoryPage/>` lives in the Search app shell at `/search/history`.
//!
//! `GET /api/v1/history` searches all stored query text for the current key
//! before pagination. Enter/Search applies the draft; typing leaves the
//! applied view in place. Rows can reopen a query in the Search editor or
//! open Save as Net's modal for `POST /api/v1/saved`.
//!
//! `hq` records the applied filter, and `hpage` records its requested page.
//! History uses `hpage` separately from Search's `page` parameter so changing
//! sections leaves a clean History URL. Export serializes the loaded applied
//! page as CSV or JSON. Confirmed Clear deletes every history row for the key,
//! including entries outside the filter.

use crate::api;
use crate::components::save_as_net_modal::SaveAsNetModal;
use crate::download::trigger_download;
use crate::history_export::{HistoryExportFormat, serialize_history};
use crate::search_url::{
    HistoryView, PAGE_SIZE, admit_history_view, build_history_url, read_history_view,
};
use crate::state::query::{Mode, RangeSpec, navigator, report_refusal};
use fleet_ui::{
    Btn, ConfirmModal, ConfirmState, OffsetPager, PageTotal, PageWindow, ToastBus, ToastKind,
    Variant, When,
};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::NavigateOptions;
use leptos_router::hooks::{use_location, use_navigate};
use std::num::NonZeroUsize;
use trawl_api::HistoryResponse;

#[derive(Clone)]
enum HistoryLoad {
    Idle,
    Refused(&'static str),
    Pending {
        view: HistoryView,
        retained: Option<HistoryResponse>,
    },
    Ready {
        view: HistoryView,
        response: HistoryResponse,
    },
    Failed {
        view: HistoryView,
        error: String,
        retained: Option<HistoryResponse>,
    },
}
impl HistoryLoad {
    fn view(&self) -> Option<&HistoryView> {
        match self {
            Self::Pending { view, .. } | Self::Ready { view, .. } | Self::Failed { view, .. } => {
                Some(view)
            }
            Self::Idle | Self::Refused(_) => None,
        }
    }
    fn loaded(&self) -> Option<(&HistoryView, &HistoryResponse)> {
        match self {
            Self::Ready { view, response }
            | Self::Pending {
                view,
                retained: Some(response),
            }
            | Self::Failed {
                view,
                retained: Some(response),
                ..
            } => Some((view, response)),
            _ => None,
        }
    }
}

#[component]
#[allow(clippy::too_many_lines)]
pub fn HistoryPage() -> impl IntoView {
    let table_viewport = NodeRef::<leptos::html::Div>::new();
    let bus = expect_context::<ToastBus>();
    let location = use_location();
    let size = NonZeroUsize::new(PAGE_SIZE).expect("history page size is nonzero");
    let draft = RwSignal::new(String::new());
    let validation = RwSignal::new(None::<&'static str>);
    let composing = RwSignal::new(false);
    let state = RwSignal::new(HistoryLoad::Idle);
    let raw_owner = RwSignal::new(None::<String>);
    let generation = RwSignal::new(0_u64);
    let navigation = RwSignal::new(0_u64);
    let refresh = RwSignal::new(0_u64);
    let clearing = RwSignal::new(false);
    let confirm_clear = RwSignal::new(ConfirmState::<()>::default());
    Effect::new(move |_| {
        let raw = location.search.get();
        refresh.track();
        let changed_url = raw_owner.get_untracked().as_ref() != Some(&raw);
        let requested = read_history_view(&raw);
        if changed_url {
            navigation.update(|value| *value = value.wrapping_add(1));
            raw_owner.set(Some(raw.clone()));
            draft.set(
                requested
                    .as_ref()
                    .map_or_else(|_| String::new(), |view| view.filter.clone()),
            );
            validation.set(None);
            clearing.set(false);
            confirm_clear.update(ConfirmState::cancel);
        }
        let token = generation.get_untracked().wrapping_add(1);
        generation.set(token);
        let view = match requested {
            Ok(view) => view,
            Err(error) => {
                state.set(HistoryLoad::Refused(error));
                return;
            }
        };
        let retained = state.with_untracked(|old| {
            old.loaded()
                .filter(|(old_view, _)| **old_view == view)
                .map(|(_, response)| response.clone())
        });
        state.set(HistoryLoad::Pending {
            view: view.clone(),
            retained: retained.clone(),
        });
        spawn_local(async move {
            let response = match PageWindow::checked_offset(view.page, size) {
                Ok(offset) => api::history(PAGE_SIZE, offset, &view.filter).await,
                Err(_) => Err(api::ApiError::Refused(
                    "This history page is too large to request.",
                )),
            };
            // Guard every result field and the interval before a URL effect.
            if generation.try_get_untracked() != Some(token)
                || location.search.get_untracked() != raw
                || location.pathname.get_untracked() != "/search/history"
            {
                return;
            }
            state.set(match response {
                Ok(response) => HistoryLoad::Ready { view, response },
                Err(error) => HistoryLoad::Failed {
                    view,
                    error: error.to_string(),
                    retained,
                },
            });
        });
    });
    let current_url =
        Signal::derive(move || raw_owner.get().as_ref() == Some(&location.search.get()));
    let ready = Signal::derive(move || {
        current_url.get() && !clearing.get() && matches!(state.get(), HistoryLoad::Ready { .. })
    });
    let busy = Signal::derive(move || {
        !current_url.get() || matches!(state.get(), HistoryLoad::Idle | HistoryLoad::Pending { .. })
    });
    let loaded = Signal::derive(move || {
        if !current_url.get() {
            return None;
        }
        state.with(|load| {
            load.loaded()
                .map(|(view, response)| (view.clone(), response.clone()))
        })
    });
    let export_format = RwSignal::new(HistoryExportFormat::Csv);
    let export_disabled = Signal::derive(move || {
        !ready.get()
            || loaded
                .get()
                .is_none_or(|(_, response)| response.entries.is_empty())
    });
    let edited = Signal::derive(move || {
        state.with(|load| load.view().is_some_and(|view| view.filter != draft.get()))
    });
    // Capture navigators here: navigate() panics outside the Router context,
    // including inside deferred callbacks that run after component setup.
    let goto_search = navigator();
    let goto_history = use_navigate();
    let on_rerun = move |query: String| {
        report_refusal(
            bus,
            goto_search(&query, 0, Mode::Snapshot, &[], &RangeSpec::default(), false),
        );
    };
    // Save as Net retains its existing modal and stored-query behavior.
    let save_target = RwSignal::new(None::<String>);
    let on_modal_close = Callback::new(move |_saved: bool| save_target.set(None));
    let refetch = Callback::new(move |()| {
        if clearing.get_untracked() {
            return;
        }
        let Some(view) = state.with_untracked(|load| load.view().cloned()) else {
            return;
        };
        let retained =
            state.with_untracked(|load| load.loaded().map(|(_, response)| response.clone()));
        // Disable actions and retire an earlier read in the submission callback,
        // before the reactive effect starts the replacement request.
        generation.update(|value| *value = value.wrapping_add(1));
        state.set(HistoryLoad::Pending { view, retained });
        refresh.update(|value| *value = value.wrapping_add(1));
    });
    let apply = {
        let navigate = goto_history.clone();
        Callback::new(move |(text, clear_filter): (String, bool)| {
            if clearing.get_untracked() {
                return;
            }
            let old = state.with_untracked(|load| load.view().cloned());
            let page = if clear_filter {
                0
            } else {
                old.as_ref()
                    .filter(|view| view.filter == text)
                    .map_or(0, |view| view.page)
            };
            match admit_history_view(&text, page) {
                Err(error) => validation.set(Some(error)),
                Ok(view) => {
                    validation.set(None);
                    draft.set(text);
                    if old.as_ref() == Some(&view) && current_url.get_untracked() {
                        refetch.run(());
                    } else {
                        navigate(&build_history_url(&view), NavigateOptions::default());
                    }
                }
            }
        })
    };
    let retry = refetch;
    let recover = {
        let navigate = goto_history.clone();
        Callback::new(move |()| {
            navigate(
                "/search/history",
                NavigateOptions {
                    replace: true,
                    ..Default::default()
                },
            );
        })
    };
    let on_page = {
        let navigate = goto_history.clone();
        Callback::new(move |page: usize| {
            if !ready.get_untracked() {
                return;
            }
            let Some(view) = state.with_untracked(|load| load.view().cloned()) else {
                return;
            };
            if let Ok(next) = admit_history_view(&view.filter, page) {
                draft.set(next.filter.clone());
                validation.set(None);
                navigate(
                    &build_history_url(&next),
                    NavigateOptions {
                        replace: true,
                        ..Default::default()
                    },
                );
            }
        })
    };
    let on_export = Callback::new(move |()| {
        if export_disabled.get_untracked() {
            return;
        }
        let Some((_, response)) = loaded.get_untracked() else {
            return;
        };
        let format = export_format.get_untracked();
        let result = serialize_history(&response.entries, format)
            .map_err(|error| error.to_string())
            .and_then(|bytes| trigger_download(&bytes, format.filename(), format.mime()));
        if let Err(error) = result {
            bus.push(ToastKind::Error, "Export failed", Some(error));
        }
    });
    let on_clear = Callback::new(move |()| {
        if !clearing.get_untracked() {
            confirm_clear.update(|value| value.request(()));
        }
    });
    let on_confirm_clear = {
        let navigate = goto_history.clone();
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
            let token = navigation.get_untracked();
            let raw = location.search.get_untracked();
            let navigate = navigate.clone();
            spawn_local(async move {
                let result = api::clear_history().await;
                if navigation.try_get_untracked() != Some(token)
                    || location.search.get_untracked() != raw
                    || location.pathname.get_untracked() != "/search/history"
                {
                    return;
                }
                match result {
                    Ok(response) => {
                        // Retire pre-deletion reads and data before navigation.
                        // Failed refreshes must never restore pre-clear rows.
                        generation.update(|value| *value = value.wrapping_add(1));
                        state.set(HistoryLoad::Idle);
                        draft.set(String::new());
                        validation.set(None);
                        navigate(
                            "/search/history",
                            NavigateOptions {
                                replace: true,
                                ..Default::default()
                            },
                        );
                        if raw.is_empty() {
                            refresh.update(|value| *value = value.wrapping_add(1));
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
    view! {
        <div class="page history-page">
            <div class="page-hd compact">
                <div><h1>"Search history"</h1><p class="sub">"Click to re-run a previous search, or save as a Net for scheduled runs."</p></div>
                <div class="actions">
                    <select class="btn-sec" aria-label="History export format" disabled=move || export_disabled.get()
                        on:change=move |event| export_format.set(if event_target_value(&event) == "json" { HistoryExportFormat::Json } else { HistoryExportFormat::Csv })>
                        <option value="csv">"CSV"</option><option value="json">"JSON"</option>
                    </select>
                    <Btn variant=Variant::Secondary disabled=export_disabled on_click=on_export>"Export this page"</Btn>
                    <Btn variant=Variant::Secondary disabled=clearing on_click=on_clear>"Clear history"</Btn>
                </div>
            </div>
            <div class="page-split"><section class="list-sheet" aria-labelledby="history-sheet-title">
                <div class="list-sheet-hd">
                    <h2 id="history-sheet-title" class="list-sheet-ttl">"Recent searches"</h2>
                    <form class="history-search" on:submit=move |event| {
                        event.prevent_default();
                        // Ignore implicit IME submits until compositionend.
                        // Once it clears the flag, both pointer and keyboard
                        // Search activation use the same admitted apply action.
                        if !composing.get_untracked() { apply.run((draft.get_untracked(), false)); }
                    }>
                        <label for="history-filter">"Search query text"</label>
                        <div class="actions">
                            <input id="history-filter" class="inp" placeholder="Filter history…" prop:value=move || draft.get()
                                disabled=move || clearing.get() aria-describedby="history-filter-help history-filter-error"
                                aria-invalid=move || validation.get().is_some().to_string()
                                on:input=move |event| { draft.set(event_target_value(&event)); validation.set(None); }
                                on:compositionstart=move |_| composing.set(true) on:compositionend=move |_| composing.set(false)
                                on:keydown=move |event| { if event.key() == "Enter" && event.is_composing() { event.prevent_default(); } }/>
                            <button class="btn" type="submit" disabled=move || clearing.get()>"Search"</button>
                            <button class="btn-sec" type="button" disabled=move || clearing.get() on:click=move |_| apply.run((String::new(), true))>"Clear filter"</button>
                        </div>
                        <p id="history-filter-help" class="sub">{move || if edited.get() { "Edited. Press Enter or Search to apply." } else { "Search all query history for your current key." }}</p>
                        <div id="history-filter-error" class="error" role="alert">{move || validation.get()}</div>
                    </form>
                </div>
                <fleet_ui::OverflowHint viewport=table_viewport/>
                <div node_ref=table_viewport class="tbl fleet-table-frame tbl-scroll" role="region" aria-label="Search history"
                    tabindex="0" style="--list-min-width:560px" aria-busy=move || (busy.get() || clearing.get()).to_string()>
                    <div class="tbl-body">
                        {move || match if current_url.get() { state.get() } else { HistoryLoad::Idle } {
                            HistoryLoad::Refused(error) => view! { <div class="load-hint error" role="alert">{error}<div class="load-recovery"><Btn variant=Variant::Secondary on_click=recover>"Reset history view"</Btn></div></div> }.into_any(),
                            HistoryLoad::Failed { error, retained, .. } => view! { <div class="load-hint error" role="alert">{if retained.is_some() { format!("Couldn't refresh history: {error}. Previously loaded rows remain below.") } else { format!("Couldn't load history: {error}") }}<div class="load-recovery"><Btn variant=Variant::Secondary disabled=clearing on_click=retry>"Retry"</Btn></div></div> }.into_any(),
                            HistoryLoad::Idle | HistoryLoad::Pending { retained: None, .. } => view! { <div class="load-hint" role="status">"Loading history…"</div> }.into_any(),
                            HistoryLoad::Pending { retained: Some(_), .. } => view! { <div class="load-hint" role="status">"Refreshing history…"</div> }.into_any(),
                            HistoryLoad::Ready { .. } => ().into_any(),
                        }}
                        {move || {
                            let Some((view, response)) = loaded.get() else { return ().into_any(); };
                            if response.entries.is_empty() {
                                let message = if response.total > 0 { "This page is outside the matching history. Use Previous to return to the last page." }
                                    else if view.filter.is_empty() { "No history" } else { "No matching queries" };
                                return view! { <div class="tbl-empty">{message}</div> }.into_any();
                            }
                            let rows = response.entries.into_iter().map(|entry| {
                                let query = entry.query.clone();
                                let saved_query = entry.query.clone();
                                let rerun = on_rerun.clone();
                                view! { <tr class="tbl-row">
                                    <td class="mono path" style="min-width:0">
                                        // A command, not a destination: ADR-0027
                                        // refuses stored queries that exceed the
                                        // link bound before rerunning. An anchor
                                        // cannot report that refusal.
                                        <button type="button" class="row-stretch" on:click=move |_| rerun(query.clone())>{entry.query}</button>
                                    </td>
                                    // When uses Fleet's shared 30-second clock,
                                    // so relative timestamps do not freeze at load.
                                    <td style="color:var(--ink-3)"><When ts=entry.executed_at/></td>
                                    <td style="text-align:right">{format_with_commas(entry.row_count as u64)}</td>
                                    <td style="text-align:right"><button type="button" class="link" on:click=move |_| save_target.set(Some(saved_query.clone()))>"Save as Net"</button></td>
                                </tr> }
                            }).collect::<Vec<_>>();
                            view! { <table class="fleet-table history-table" aria-label="Search history">
                                <thead><tr><th scope="col">"Executed query"</th><th scope="col" style="width:92px">"When"</th><th scope="col" style="width:80px; text-align:right">"Rows"</th><th scope="col" style="width:100px; text-align:right">"Action"</th></tr></thead>
                                <tbody>{rows}</tbody>
                            </table> }.into_any()
                        }}
                    </div>
                    {move || {
                        let Some((view, response)) = loaded.get() else { return ().into_any(); };
                        let window = Signal::derive(move || PageWindow::new(view.page, size, response.entries.len(), PageTotal::Known(response.total), !ready.get()).expect("history view has an admitted offset"));
                        view! { <OffsetPager window=window on_page=on_page/> }.into_any()
                    }}
                </div>
            </section></div>
            <Show when=move || confirm_clear.get().is_open()>
                <ConfirmModal title="Clear history"
                    message="Delete all query history for your current key, across every page, including entries that do not match the current filter? Saved queries will remain. A query running now may add a new history entry after the clear.".to_owned()
                    confirm_label="Clear all history" confirm_variant=Variant::Danger on_confirm=on_confirm_clear
                    on_cancel=Callback::new(move |()| confirm_clear.update(ConfirmState::cancel))/>
            </Show>
            {move || save_target.get().map(|query| view! { <SaveAsNetModal query=query on_close=on_modal_close/> })}
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
