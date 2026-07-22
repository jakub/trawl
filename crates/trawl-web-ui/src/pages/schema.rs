// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<SchemaPage/>` — sortable services table over
//! `/api/v1/schema/services` with a slide-out drawer inspector for
//! each service.
//!
//! URL params:
//! - `svc=<name>` — opens the drawer on the named service; clearing
//!   closes the drawer.
//! - `stab=overview|fields|tail` — which drawer tab is active
//!   (default: overview).
//!
//! Density comes from the global statusbar toggle
//! (`<html data-density>`) like every other table — the old
//! page-local Comfy/Compact control is gone.

use leptos::prelude::*;
use leptos::web_sys;
use leptos_router::NavigateOptions;
use leptos_router::hooks::{use_navigate, use_query_map};
use trawl_api::ServiceSchema;

use crate::api;
use crate::components::service_card_fmt::{
    avg_cov_permille, format_avg_coverage, format_bytes, format_count, is_healthy,
    today_yesterday_utc,
};
use crate::components::service_drawer::ServiceDrawer;
use crate::components::sort_th::sort_th;
use crate::state::query::{Mode, RangeSpec, navigator};
use fleet_ui::{
    Icon, IconView, LoadState, Loaded, Pager, SearchInput, Sparkline, StatusDot, StatusTone,
};

/// Days of `daily_event_counts` history shown in the activity sparkline.
const SPARK_DAYS: usize = 30;

/// Sort key for the services table. Clicking the active header flips
/// direction; a fresh key starts at its natural direction (name
/// ascending, everything else descending).
#[derive(Clone, Copy, PartialEq, Eq)]
enum SvcSort {
    Name,
    Earliest,
    Latest,
    Events,
    Storage,
    Fields,
    Coverage,
}

impl SvcSort {
    fn default_desc(self) -> bool {
        !matches!(self, Self::Name)
    }
}

#[component]
#[allow(clippy::too_many_lines)]
pub fn SchemaPage() -> impl IntoView {
    let qm = use_query_map();
    let svc_selected = Memo::new(move |_| qm.get().get("svc"));
    let tab_param = Memo::new(move |_| {
        qm.get()
            .get("stab")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "overview".to_string())
    });
    let tab_sig: Signal<String> = Signal::derive(move || tab_param.get());

    let filter = RwSignal::new(String::new());
    let sort = RwSignal::new((SvcSort::Name, false));

    let services = LocalResource::new(|| async move { api::schema_services().await });

    // Captured up-front — `use_navigate()` panics outside the router
    // reactive context (i.e. inside deferred callbacks).
    let nav = use_navigate();
    let goto_search = navigator();

    let push_svc = {
        let nav = nav.clone();
        move |name: Option<&str>, stab: &str| {
            let url = match name {
                Some(n) => {
                    let n_enc = js_sys::encode_uri_component(n)
                        .as_string()
                        .unwrap_or_else(|| n.to_string());
                    format!("/search/schema?svc={n_enc}&stab={stab}")
                }
                None => "/search/schema".to_string(),
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

    let on_open: Callback<String> = {
        let push = push_svc.clone();
        Callback::new(move |name: String| push(Some(&name), "overview"))
    };
    let on_tail: Callback<String> = {
        let push = push_svc.clone();
        Callback::new(move |name: String| push(Some(&name), "tail"))
    };
    let on_tab_change: Callback<String> = {
        let push = push_svc.clone();
        Callback::new(move |stab: String| {
            let name = svc_selected.get_untracked();
            push(name.as_deref(), &stab);
        })
    };
    let on_close: Callback<()> = {
        let push = push_svc.clone();
        Callback::new(move |()| push(None, "overview"))
    };

    let on_search: Callback<String> = {
        let goto = goto_search.clone();
        Callback::new(move |name: String| {
            let q = format!(r#"service="{}""#, name.replace('"', ""));
            goto(&q, 0, Mode::Snapshot, &[], &RangeSpec::default(), false);
        })
    };

    let on_use_field: Callback<String> = {
        let goto = goto_search.clone();
        Callback::new(move |field: String| {
            // Wildcard match — `field=*` selects every event carrying
            // the field, which is what the user usually wants after
            // drilling in from a schema view.
            let q = format!("{field}=*");
            goto(&q, 0, Mode::Snapshot, &[], &RangeSpec::default(), false);
        })
    };

    let (today, yesterday) = today_yesterday_utc();

    view! {
        <div class="page">
            <div class="page-hd compact">
                <div>
                    <h1>"Schema"</h1>
                    <p class="sub">"Click a service to inspect fields, ingest rate, and tail live."</p>
                </div>
                <div class="actions">
                    <SearchInput value=filter placeholder="filter services…"/>
                </div>
            </div>

            <div class="tbl">
                <div class="tbl-hd">
                    {sort_th(sort, SvcSort::Name, SvcSort::Name.default_desc(), "Service", "flex:2; min-width:0")}
                    <div class="th" style="flex:0 0 110px">"Activity"</div>
                    {sort_th(sort, SvcSort::Earliest, SvcSort::Earliest.default_desc(), "Earliest", "flex:0 0 88px")}
                    {sort_th(sort, SvcSort::Latest, SvcSort::Latest.default_desc(), "Latest", "flex:0 0 88px")}
                    {sort_th(sort, SvcSort::Events, SvcSort::Events.default_desc(), "Events", "flex:0 0 64px; justify-content:flex-end")}
                    {sort_th(sort, SvcSort::Storage, SvcSort::Storage.default_desc(), "Storage", "flex:0 0 80px; justify-content:flex-end")}
                    {sort_th(sort, SvcSort::Fields, SvcSort::Fields.default_desc(), "Fields", "flex:0 0 56px; justify-content:flex-end")}
                    {sort_th(sort, SvcSort::Coverage, SvcSort::Coverage.default_desc(), "Avg cov", "flex:0 0 68px; justify-content:flex-end")}
                    <div style="flex:0 0 64px"></div>
                </div>
                <div class="tbl-body">
                    <Loaded
                        state=Signal::derive(move || LoadState::from_resource(services.get()))
                        label="schema"
                        render=Box::new(move |resp: trawl_api::ServiceSchemaResponse| {
                            let needle = filter.get().to_lowercase();
                            let mut visible: Vec<ServiceSchema> = resp.services.iter()
                                .filter(|s| {
                                    if needle.is_empty() { return true; }
                                    if s.name.to_lowercase().contains(&needle) { return true; }
                                    s.columns.iter().any(|c| c.name.to_lowercase().contains(&needle))
                                })
                                .cloned()
                                .collect();
                            if visible.is_empty() {
                                return view! {
                                    <div class="tbl-empty">
                                        {if resp.services.is_empty() {
                                            "no services yet — ingest some logs and they'll appear here"
                                        } else {
                                            "no services match that filter"
                                        }}
                                    </div>
                                }.into_any();
                            }

                            let (key, desc) = sort.get();
                            visible.sort_by(|a, b| {
                                let ord = match key {
                                    SvcSort::Name => {
                                        a.name.to_lowercase().cmp(&b.name.to_lowercase())
                                    }
                                    // ISO dates compare correctly as strings;
                                    // None (no ingest yet) sorts before any date.
                                    SvcSort::Earliest => a.earliest_date.cmp(&b.earliest_date),
                                    SvcSort::Latest => a.latest_date.cmp(&b.latest_date),
                                    SvcSort::Events => a.total_events.cmp(&b.total_events),
                                    SvcSort::Storage => a.total_bytes.cmp(&b.total_bytes),
                                    SvcSort::Fields => a.columns.len().cmp(&b.columns.len()),
                                    SvcSort::Coverage => avg_cov_permille(&a.columns)
                                        .cmp(&avg_cov_permille(&b.columns)),
                                };
                                if desc { ord.reverse() } else { ord }
                            });

                            let count = visible.len();
                            let today = today.clone();
                            let yesterday = yesterday.clone();
                            let rows = visible.into_iter().map(|svc| {
                                let name = svc.name.clone();
                                let healthy = is_healthy(&svc, &today, &yesterday);
                                let dot_tone = if healthy {
                                    StatusTone::Success
                                } else {
                                    StatusTone::Error
                                };
                                let spark_color = if healthy {
                                    "var(--accent)"
                                } else {
                                    "var(--red)"
                                };
                                let spark_data: Vec<u64> = svc
                                    .daily_event_counts
                                    .iter()
                                    .rev()
                                    .take(SPARK_DAYS)
                                    .map(|d| d.count)
                                    .collect::<Vec<_>>()
                                    .into_iter()
                                    .rev()
                                    .collect();
                                let earliest = svc
                                    .earliest_date
                                    .clone()
                                    .unwrap_or_else(|| "—".to_string());
                                let latest = svc
                                    .latest_date
                                    .clone()
                                    .unwrap_or_else(|| "—".to_string());
                                let events_label = format_count(svc.total_events);
                                let storage_label = format_bytes(svc.total_bytes);
                                let field_count = svc.columns.len();
                                let coverage_label = format_avg_coverage(&svc.columns);

                                let name_for_active = name.clone();
                                let name_open = name.clone();
                                let name_search = name.clone();
                                let name_tail = name.clone();
                                view! {
                                    <div
                                        class="tbl-row"
                                        class:active=move || {
                                            svc_selected.get().as_deref()
                                                == Some(name_for_active.as_str())
                                        }
                                        on:click=move |_| on_open.run(name_open.clone())
                                    >
                                        <div class="svc-cell" style="flex:2; min-width:0">
                                            <StatusDot tone=dot_tone/>
                                            <span class="mono name">{name}</span>
                                        </div>
                                        <div style="flex:0 0 110px">
                                            <Sparkline data=spark_data color=spark_color w=96 h=16/>
                                        </div>
                                        <div class="mono" style="flex:0 0 88px">{earliest}</div>
                                        <div class="mono" style="flex:0 0 88px">{latest}</div>
                                        <div class="num" style="flex:0 0 64px">{events_label}</div>
                                        <div class="num" style="flex:0 0 80px">{storage_label}</div>
                                        <div class="num" style="flex:0 0 56px">{field_count}</div>
                                        <div class="num" style="flex:0 0 68px">{coverage_label}</div>
                                        <div class="row-act" style="flex:0 0 64px">
                                            <span
                                                class="qa"
                                                title="Search this service"
                                                on:click=move |e: web_sys::MouseEvent| {
                                                    e.stop_propagation();
                                                    on_search.run(name_search.clone());
                                                }
                                            >
                                                <IconView icon=Icon::Search size=12 stroke_width=1.5/>
                                            </span>
                                            <span
                                                class="qa"
                                                title="Live tail"
                                                on:click=move |e: web_sys::MouseEvent| {
                                                    e.stop_propagation();
                                                    on_tail.run(name_tail.clone());
                                                }
                                            >
                                                <IconView icon=Icon::Bolt size=12 stroke_width=1.5/>
                                            </span>
                                        </div>
                                    </div>
                                }
                            }).collect::<Vec<_>>();
                            view! {
                                {rows}
                                // Summary-only Pager — the table is unpaginated.
                                <Pager summary=format!(
                                    "{count} service{}",
                                    if count == 1 { "" } else { "s" },
                                )/>
                            }.into_any()
                        })
                    />
                </div>
            </div>

            {move || {
                // Drawer mounts only when `?svc=X` is set AND the name
                // resolves to a service in the current snapshot. If the
                // user lands on a dead `?svc=foo`, we silently ignore
                // it rather than popping an error modal.
                let Some(selected) = svc_selected.get() else { return ().into_any(); };
                let Some(Ok(resp)) = services.get() else { return ().into_any(); };
                let Some(svc) = resp.services.iter().find(|s| s.name == selected).cloned()
                    else { return ().into_any(); };
                view! {
                    <ServiceDrawer
                        svc=svc
                        tab=tab_sig
                        on_close=on_close
                        on_tab_change=on_tab_change
                        on_search=on_search
                        on_use_field=on_use_field
                    />
                }.into_any()
            }}
        </div>
    }
}
