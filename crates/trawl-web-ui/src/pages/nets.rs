// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<NetsPage/>` — saved query ("net") management for the Jobs mode.
//!
//! URL params:
//! - `net=<id>` — opens the detail drawer for that net
//! - `ntab=query|runs` — active drawer tab (default: query)

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::NavigateOptions;
use leptos_router::hooks::{use_navigate, use_query_map};
use leptos_use::use_media_query;
use trawl_api::SavedQueryResponse;

use crate::api;
use crate::components::net_drawer::NetDrawer;
use crate::components::sort_th::table_sort_th;
use crate::schedule_edit::cadence_sentence;
use crate::state::query::{Mode, RangeSpec, navigator, report_refusal};
use fleet_ui::time::{time_ago, time_until};
use fleet_ui::{
    ActionItem, ActionsMenu, Badge, ConfirmModal, ConfirmState, LoadState, Loaded, Pager,
    SearchInput, StatusDot, ToastBus, ToastKind, Tone,
};

/// Whether a net survives the list filter: its name or its query text.
/// Named once because the sheet header counts exactly what the table
/// renders.
fn matches_filter(net: &SavedQueryResponse, needle: &str) -> bool {
    needle.is_empty()
        || net.name.to_lowercase().contains(needle)
        || net.query.to_lowercase().contains(needle)
}

/// Sort key for the nets table. Name starts ascending; the two
/// timestamp keys start descending (most recent first).
#[derive(Clone, Copy, PartialEq, Eq)]
enum NetSort {
    Name,
    LastRun,
}

impl NetSort {
    fn default_desc(self) -> bool {
        !matches!(self, Self::Name)
    }
}

#[component]
#[allow(clippy::too_many_lines)]
pub fn NetsPage() -> impl IntoView {
    let table_viewport = NodeRef::<leptos::html::Div>::new();
    let bus = expect_context::<ToastBus>();
    let qm = use_query_map();
    let net_selected: Memo<Option<i64>> =
        Memo::new(move |_| qm.get().get("net").and_then(|s| s.parse::<i64>().ok()));
    let tab_param = Memo::new(move |_| {
        qm.get()
            .get("ntab")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "query".to_string())
    });
    let tab_sig: Signal<String> = Signal::derive(move || tab_param.get());

    let filter = RwSignal::new(String::new());
    let sort = RwSignal::new((NetSort::Name, false));
    let refresh = RwSignal::new(0u64);
    let confirm_delete: RwSignal<ConfirmState<(i64, String)>> =
        RwSignal::new(ConfirmState::default());

    let (nets, refresh_error, retry) =
        crate::components::job_refresh::job_refresh(Signal::stored(true), move || {
            let _ = refresh.get();
            async move { api::list_saved().await }
        });

    // Past this width the net panel docks in the second column instead
    // of sliding over a scrim (ADR-0032). The column exists whenever the
    // URL names a net, so a missing or still-loading one reports there.
    let wide = use_media_query("(min-width: 1100px)");
    let panel_open = Signal::derive(move || net_selected.get().is_some());

    let nav = use_navigate();
    let goto_search = navigator();

    let push_net = {
        let nav = nav.clone();
        move |id: Option<i64>, ntab: &str| {
            let url = match id {
                Some(n) => format!("/jobs/nets?net={n}&ntab={ntab}"),
                None => "/jobs/nets".to_string(),
            };
            nav(
                &url,
                NavigateOptions {
                    replace: true,
                    ..Default::default()
                },
            );
        }
    };

    let on_tab_change: Callback<String> = {
        let push = push_net.clone();
        Callback::new(move |ntab: String| {
            let id = net_selected.get_untracked();
            push(id, &ntab);
        })
    };
    let on_close: Callback<()> = {
        let push = push_net.clone();
        Callback::new(move |()| push(None, "query"))
    };
    let on_refresh: Callback<()> = Callback::new(move |()| {
        refresh.update(|n| *n += 1);
    });
    let on_search: Callback<String> = {
        let goto = goto_search.clone();
        Callback::new(move |q: String| {
            report_refusal(
                bus,
                goto(&q, 0, Mode::Snapshot, &[], &RangeSpec::default(), false),
            );
        })
    };

    let do_delete = {
        move |id: i64, name: String| {
            spawn_local(async move {
                match api::delete_saved(id).await {
                    Ok(_) => {
                        bus.push(
                            ToastKind::Success,
                            "Net deleted",
                            Some(format!("'{name}' has been removed.")),
                        );
                        refresh.update(|n| *n += 1);
                    }
                    Err(e) => {
                        bus.push(ToastKind::Error, "Delete failed", Some(e.to_string()));
                    }
                }
            });
        }
    };

    let on_run_in_search = {
        let goto = goto_search.clone();
        move |q: String| {
            report_refusal(
                bus,
                goto(&q, 0, Mode::Snapshot, &[], &RangeSpec::default(), false),
            );
        }
    };

    let on_trigger_run = {
        move |id: i64, name: String| {
            spawn_local(async move {
                match api::trigger_run(id).await {
                    Ok(_) => {
                        bus.push(
                            ToastKind::Success,
                            "Run triggered",
                            Some(format!("'{name}' is executing.")),
                        );
                        refresh.update(|n| *n += 1);
                    }
                    Err(e) => {
                        bus.push(ToastKind::Error, "Trigger failed", Some(e.to_string()));
                    }
                }
            });
        }
    };

    #[allow(clippy::cast_possible_truncation)]
    let now_ms = fleet_ui::time::clock::now_ms();

    let sort_nets = move |nets: &mut [&SavedQueryResponse]| {
        let (key, desc) = sort.get();
        let last_run_ts = |n: &SavedQueryResponse| {
            n.schedule
                .as_ref()
                .and_then(|s| s.last_run.as_ref())
                .map_or("", |r| r.started_at.as_str())
                .to_string()
        };
        nets.sort_by(|a, b| {
            let ord = match key {
                NetSort::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                // ISO timestamps compare correctly as strings; nets that
                // never ran ("") sort last under the default descending.
                NetSort::LastRun => last_run_ts(a).cmp(&last_run_ts(b)),
            };
            if desc { ord.reverse() } else { ord }
        });
    };

    // The sheet header counts what the table shows, off the same
    // predicate the rows are filtered by.
    let visible_nets = Signal::derive(move || {
        let Some(Ok(resp)) = nets.get() else {
            return Vec::new();
        };
        let needle = filter.get().to_lowercase();
        let mut visible: Vec<&SavedQueryResponse> = resp
            .queries
            .iter()
            .filter(|q| matches_filter(q, &needle))
            .collect();
        sort_nets(&mut visible);
        visible.into_iter().cloned().collect::<Vec<_>>()
    });

    view! {
        <div class="page">
            <div class="page-hd compact">
                <div>
                    <h1>"Nets"</h1>
                    <p class="sub">"Manage saved queries, attach schedules, and inspect run history."</p>
                </div>
            </div>

            {move || refresh_error.get().map(|e| view! { <p role="status">{e}</p> })}
            <div class="page-split" class:has-panel=move || panel_open.get()>
            <section class="list-sheet" aria-labelledby="nets-sheet-title">
                <div class="list-sheet-hd">
                    <h2 id="nets-sheet-title" class="list-sheet-ttl">
                        "Your nets"<span class="cnt">{move || visible_nets.get().len()}</span>
                    </h2>
                    <SearchInput value=filter placeholder="Filter nets…"/>
                </div>
            <fleet_ui::OverflowHint viewport=table_viewport/>
                <div node_ref=table_viewport class="tbl fleet-table-frame tbl-scroll" role="region" aria-label="Saved queries" tabindex="0" style="--list-min-width:560px">
                <div class="tbl-body">
                    <Loaded
                        state=Signal::derive(move || LoadState::from_resource(nets.get().map(|r| r.map(|_| ()))))
                        label="nets" retry=retry render=Box::new(|()| ().into_any())
                    />
                    <table class="fleet-table nets-table" aria-label="Saved queries">
                        <thead><tr>
                            {table_sort_th(sort, NetSort::Name, NetSort::Name.default_desc(), "Name", "")}
                            <th scope="col" class="th">"Schedule"</th>
                            {table_sort_th(sort, NetSort::LastRun, NetSort::LastRun.default_desc(), "Last run", "width:180px")}
                            <th scope="col" style="width:60px"><span class="sr-only">Actions</span></th>
                        </tr></thead>
                        <tbody>
                            <For each=move || visible_nets.get() key=|net| net.id children=move |initial| {
                                let id = initial.id;
                                let net = Signal::derive(move || nets.get().and_then(Result::ok)
                                    .and_then(|r| r.queries.into_iter().find(|q| q.id == id)).unwrap_or_else(|| initial.clone()));
                                // The row owner and its ActionsMenu survive data and clock ticks.
                                // Read current values when invoking an action, not the mount-time net.
                                let run = on_run_in_search.clone();
                                let search = ActionItem::new("▶ Open in search", Callback::new(move |()| run(net.get_untracked().query)));
                                let delete = ActionItem::danger("Delete", Callback::new(move |()| {
                                    confirm_delete.update(|c| c.request((id, net.get_untracked().name)));
                                }));
                                let manual = ActionItem::new("⏱ Trigger run", Callback::new(move |()| {
                                    let current = net.get_untracked();
                                    // A window owns its coverage; never trigger it out of band,
                                    // including when the schedule changed while the menu was open.
                                    if current.schedule.and_then(|s| s.window).is_none() {
                                        on_trigger_run(id, current.name);
                                    }
                                }));
                                let regular = StoredValue::new(vec![search.clone(), manual, delete.clone()]);
                                let windowed = StoredValue::new(vec![search, delete]);
                                // The drawer is a place with a URL, so the row's one
                                // control is a link built by the same producer
                                // `push_net` uses; `prop:replace` is that call's
                                // `replace: true`.
                                let href = format!("/jobs/nets?net={id}&ntab=query");
                                view! {
                                    <tr class="tbl-row" class:active=move || net_selected.get() == Some(id)>
                                        <td class="mono"><a class="row-stretch" href=href prop:replace=true>{move || net.get().name}</a></td>
                                        // The cadence in words, with the enabled/paused
                                        // judgement beside it as a Badge rather than a
                                        // glyph the row has to explain.
                                        <td>
                                            <span class="cadence">{move || cadence_sentence(net.get().schedule.as_ref())}</span>
                                            {move || net.get().schedule.map(|s| {
                                                let (tone, label) = if s.enabled { (Tone::Success, "Enabled") } else { (Tone::Neutral, "Paused") };
                                                view! { <Badge tone=tone>{label}</Badge> }
                                            })}
                                        </td>
                                        <td>{move || {
                                            let Some(schedule) = net.get().schedule else { return view! { <span style="color:var(--ink-3)">"—"</span> }.into_any(); };
                                            let Some(run) = schedule.last_run else {
                                                return if schedule.enabled {
                                                    view! { <span class="mono" style="color:var(--ink-3); font-size:10px">"Pending…"</span> }.into_any()
                                                } else {
                                                    view! { <span style="color:var(--ink-3)">"—"</span> }.into_any()
                                                };
                                            };
                                            let next_label = schedule.enabled.then(|| {
                                                #[allow(clippy::cast_possible_truncation)]
                                                let started_ms = js_sys::Date::parse(&run.started_at) as i64;
                                                time_until(started_ms + schedule.interval_secs.cast_signed() * 1000, now_ms.get())
                                            });
                                            view! {
                                                <span><StatusDot tone=crate::components::run_status_tone(&run.status)/>
                                                    <span class="run-status">{run.status}</span> " "
                                                    <span class="mono" style="color:var(--ink-2)">{time_ago(&run.started_at, now_ms.get())}</span>
                                                    {next_label.map(|label| view! { <span class="next-run" style="margin-left:6px; font-size:10px; color:var(--ink-3)">{label}</span> })}
                                                </span>
                                            }.into_any()
                                        }}</td>
                                        // `row-menu` lifts the trigger above the row
                                        // control's stretched pseudo-element; the base
                                        // `.actions-menu` rule belongs to fleet-ui.
                                        <td class="row-menu">
                                            <Show when=move || net.get().schedule.and_then(|s| s.window).is_none()
                                                fallback=move || view! { <ActionsMenu items=windowed.get_value()/> }>
                                                <ActionsMenu items=regular.get_value()/>
                                            </Show>
                                        </td>
                                    </tr>
                                }
                            }/>
                        </tbody>
                    </table>
                    {move || (matches!(nets.get(), Some(Ok(_))) && visible_nets.get().is_empty()).then(|| view! {
                        <div class="tbl-empty">{if nets.get().and_then(Result::ok).is_some_and(|r| r.queries.is_empty()) {
                            "No nets yet — save a query from the search page to get started"
                        } else { "No nets match that filter" }}</div>
                    })}
                    // Summary-only Pager: this table is unpaginated, so no
                    // prev/next controls.
                    <Pager summary=Signal::derive(move || {
                        let count = visible_nets.get().len();
                        format!("{count} net{}", if count == 1 { "" } else { "s" })
                    })/>
                </div>
            </div>
            </section>

            // The keyed owner depends only on the requested ID, never the list.
            <For each={move || net_selected.get().into_iter().collect::<Vec<_>>()} key=|id| *id children=move |id| {
                let saved = Signal::derive(move || nets.get().and_then(Result::ok).and_then(|r| r.queries.into_iter().find(|q| q.id == id)));
                let initial = RwSignal::new(None::<SavedQueryResponse>);
                Effect::new(move |_| {
                    if initial.get_untracked().is_none() && let Some(net) = saved.get() { initial.set(Some(net)); }
                });
                view! {
                    <Show when=move || initial.get().is_some() fallback=move || {
                        let message = match nets.get() {
                            None => "Loading net…",
                            Some(Err(_)) => "Could not load this net. Please retry.",
                            Some(Ok(_)) => "Net not found. It may have been deleted.",
                        };
                        view! { <div role="status">{message} " " <a href="/jobs/nets">"Back to Nets"</a></div> }
                    }>
                        <NetDrawer net=initial.get_untracked().expect("loaded net") saved=saved tab=tab_sig on_close=on_close
                            on_tab_change=on_tab_change on_search=on_search on_refresh=on_refresh docked=wide/>
                    </Show>
                }
            }/>
            </div>

            // Delete confirmation modal — open/close plumbing via the
            // natively-tested fleet_ui::ConfirmState; the ConfirmModal
            // composition stays app-side (ADR-0002).
            <Show when=move || confirm_delete.get().is_open()>
                {move || {
                    let Some((_, del_name)) = confirm_delete.get().pending().cloned() else {
                        return ().into_any();
                    };
                    let msg = format!("Permanently delete '{del_name}' and all its run history?");
                    view! {
                        <ConfirmModal
                            title="Delete net"
                            message=msg
                            confirm_label="Delete"
                            on_confirm=Callback::new(move |()| {
                                // take() closes the dialog and yields the
                                // payload exactly once.
                                if let Some((id, name)) =
                                    confirm_delete.try_update(fleet_ui::ConfirmState::take).flatten()
                                {
                                    do_delete(id, name);
                                }
                            })
                            on_cancel=Callback::new(move |()| {
                                confirm_delete.update(fleet_ui::ConfirmState::cancel);
                            })
                        />
                    }.into_any()
                }}
            </Show>
        </div>
    }
}
