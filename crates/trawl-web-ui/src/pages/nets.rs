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
use trawl_api::SavedQueryResponse;

use crate::api;
use crate::components::net_drawer::NetDrawer;
use crate::components::sort_th::table_sort_th;
use crate::state::query::{Mode, RangeSpec, navigator, report_refusal};
use fleet_ui::time::{time_ago, time_until};
use fleet_ui::{
    ActionItem, ActionsMenu, ConfirmModal, ConfirmState, LoadState, Loaded, Pager, SearchInput,
    StatusDot, ToastBus, ToastKind,
};

/// Sort key for the nets table. Name starts ascending; the two
/// timestamp keys start descending (most recent first).
#[derive(Clone, Copy, PartialEq, Eq)]
enum NetSort {
    Name,
    LastRun,
    Created,
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

    let nets = LocalResource::new(move || {
        let _ = refresh.get();
        async move { api::list_saved().await }
    });

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
    let now_ms = move || js_sys::Date::now() as i64;

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
                NetSort::Created => a.created_at.cmp(&b.created_at),
            };
            if desc { ord.reverse() } else { ord }
        });
    };

    view! {
        <div class="page">
            <div class="page-hd compact">
                <div>
                    <h1>"Nets"</h1>
                    <p class="sub">"Manage saved queries, attach schedules, and inspect run history."</p>
                </div>
                <div class="actions">
                    <SearchInput value=filter placeholder="Filter nets…"/>
                </div>
            </div>

            <fleet_ui::OverflowHint viewport=table_viewport/>
                <div node_ref=table_viewport class="tbl fleet-table-frame tbl-scroll" role="region" aria-label="Saved queries" tabindex="0" style="--list-min-width:760px">
                <div class="tbl-body">
                    <Loaded
                        state=Signal::derive(move || LoadState::from_resource(nets.get()))
                        label="nets"
                        retry=Callback::new(move |()| { nets.set(None); nets.refetch(); })
                        render=Box::new(move |resp: trawl_api::ListSavedResponse| {
                                let now = now_ms();
                                let needle = filter.get().to_lowercase();
                                let mut visible: Vec<&SavedQueryResponse> = resp.queries.iter()
                                    .filter(|q| {
                                        if needle.is_empty() { return true; }
                                        q.name.to_lowercase().contains(&needle)
                                            || q.query.to_lowercase().contains(&needle)
                                    })
                                    .collect();
                                if visible.is_empty() {
                                    return view! {
                                        <div class="tbl-empty">
                                            {if resp.queries.is_empty() {
                                                "No nets yet — save a query from the search page to get started"
                                            } else {
                                                "No nets match that filter"
                                            }}
                                        </div>
                                    }.into_any();
                                }
                                sort_nets(&mut visible);
                                let count = visible.len();
                                let rows = visible.into_iter().map(|net| {
                                    let id = net.id;
                                    let name = net.name.clone();
                                    let query_text = net.query.clone();
                                    let created = time_ago(&net.created_at, now);
                                    let name_for_delete = net.name.clone();
                                    let name_for_trigger = net.name.clone();
                                    let query_for_run = net.query.clone();
                                    let on_run_in_search = on_run_in_search.clone();

                                    let sched_badge = match &net.schedule {
                                        Some(s) if s.enabled => {
                                            view! {
                                                <span class="sched-badge active">{format!("⏰ {}", s.interval)}</span>
                                            }.into_any()
                                        }
                                        Some(s) => {
                                            view! {
                                                <span class="sched-badge disabled">{format!("⏸ {}", s.interval)}</span>
                                            }.into_any()
                                        }
                                        None => view! {
                                            <span style="color:var(--ink-3)">"—"</span>
                                        }.into_any(),
                                    };

                                    let last_run_view = match net.schedule.as_ref() {
                                        Some(sched) => {
                                            match sched.last_run.as_ref() {
                                                Some(run) => {
                                                    let when = time_ago(&run.started_at, now);
                                                    let tone = crate::components::run_status_tone(&run.status);
                                                    let next_run_label = if sched.enabled {
                                                        #[allow(clippy::cast_possible_truncation)]
                                                        let started_ms = js_sys::Date::parse(&run.started_at) as i64;
                                                        let next_ms =
                                                            started_ms + (sched.interval_secs.cast_signed() * 1000);
                                                        Some(time_until(next_ms, now))
                                                    } else {
                                                        None
                                                    };
                                                    view! {
                                                        <span>
                                                            <StatusDot tone=tone/>
                                                                <span class="run-status">{run.status.clone()}</span>
                                                            " "
                                                            <span class="mono" style="color:var(--ink-2)">{when}</span>
                                                            {next_run_label.map(|label| view! {
                                                                <span class="next-run" style="margin-left:6px; font-size:10px; color:var(--ink-3)">{label}</span>
                                                            })}
                                                        </span>
                                                    }.into_any()
                                                }
                                                None => {
                                                    if sched.enabled {
                                                        view! {
                                                            <span class="mono" style="color:var(--ink-3); font-size:10px">"Pending…"</span>
                                                        }.into_any()
                                                    } else {
                                                        view! {
                                                            <span style="color:var(--ink-3)">"—"</span>
                                                        }.into_any()
                                                    }
                                                }
                                            }
                                        }
                                        None => view! {
                                            <span style="color:var(--ink-3)">"—"</span>
                                        }.into_any(),
                                    };

                                    // The drawer is a place with a URL, so
                                    // the row's one control is a link built
                                    // by the same producer `push_net` uses;
                                    // `prop:replace` below is that call's
                                    // `replace: true`.
                                    let href = format!("/jobs/nets?net={id}&ntab=query");

                                    view! {
                                            <tr class="tbl-row">
                                                <td class="mono">
                                                <a class="row-stretch" href=href prop:replace=true>
                                                    {name.clone()}
                                                </a>
                                                </td>
                                                <td style="min-width:0" class="mono path">{query_text}</td>
                                                <td>{sched_badge}</td>
                                                <td>{last_run_view}</td>
                                                <td class="mono">{created}</td>
                                            // `row-menu` lifts the trigger above
                                            // the row control's stretched
                                            // pseudo-element; the base
                                            // `.actions-menu` rule belongs to
                                            // fleet-ui and stays there.
                                                <td class="row-menu">
                                                // fleet_ui::ActionsMenu owns the ⋯ trigger, the
                                                // open state, and Escape/outside-click dismissal
                                                // via the overlay stack.
                                                <ActionsMenu items=vec![
                                                    ActionItem::new("▶ Open in search", {
                                                        let q = query_for_run.clone();
                                                        let run = on_run_in_search.clone();
                                                        Callback::new(move |()| run(q.clone()))
                                                    }),
                                                    ActionItem::new("⏱ Trigger run", {
                                                        let name = name_for_trigger.clone();
                                                        let trigger = on_trigger_run;
                                                        Callback::new(move |()| trigger(id, name.clone()))
                                                    }),
                                                    ActionItem::danger("Delete", {
                                                        let name = name_for_delete.clone();
                                                        Callback::new(move |()| {
                                                            confirm_delete.update(|c| c.request((id, name.clone())));
                                                        })
                                                    }),
                                                ]/>
                                                </td>
                                            </tr>
                                    }
                                }).collect_view();
                                view! {
                                    <table class="fleet-table nets-table" aria-label="Saved queries">
                                        <thead><tr>
                                            {table_sort_th(sort, NetSort::Name, NetSort::Name.default_desc(), "Name", "")}
                                            <th scope="col" class="th">"Query"</th>
                                            <th scope="col" class="th" style="width:100px">"Schedule"</th>
                                            {table_sort_th(sort, NetSort::LastRun, NetSort::LastRun.default_desc(), "Last run", "width:160px")}
                                            {table_sort_th(sort, NetSort::Created, NetSort::Created.default_desc(), "Created", "width:110px")}
                                            <th scope="col" style="width:60px"><span class="sr-only">Actions</span></th>
                                        </tr></thead>
                                        <tbody>{rows}</tbody></table>
                                    // Summary-only Pager: this table is
                                    // unpaginated, so no prev/next controls.
                                    <Pager summary=format!("{count} net{}", if count == 1 { "" } else { "s" })/>
                                }.into_any()
                        })
                    />
                </div>
            </div>

            // Drawer — rendered when ?net=<id> is present
            {move || {
                let id = net_selected.get()?;
                let resp = nets.get()?.ok()?;
                let net = resp.queries.iter().find(|q| q.id == id)?.clone();
                Some(view! {
                    <NetDrawer
                        net=net
                        tab=tab_sig
                        on_close=on_close
                        on_tab_change=on_tab_change
                        on_search=on_search
                        on_refresh=on_refresh
                    />
                })
            }}

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
