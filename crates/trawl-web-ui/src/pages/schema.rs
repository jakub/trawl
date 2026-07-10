// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<SchemaPage/>` — cards-grid browser over `/api/v1/schema/services`
//! with a slide-out drawer inspector for each service.
//!
//! URL params:
//! - `svc=<name>` — opens the drawer on the named service; clearing
//!   closes the drawer.
//! - `stab=overview|fields|tail` — which drawer tab is active
//!   (default: overview).
//!
//! Density (Comfy/Compact) is a session-scoped `RwSignal` — kept out
//! of the URL to reduce noise on shared links.

use leptos::prelude::*;
use leptos_router::NavigateOptions;
use leptos_router::hooks::{use_navigate, use_query_map};
use trawl_api::ServiceSchema;

use crate::api;
use crate::components::service_card::ServiceCard;
use crate::components::service_card_fmt::today_yesterday_utc;
use crate::components::service_drawer::ServiceDrawer;
use crate::state::query::{Mode, RangeSpec, navigator};
use fleet_ui::{
    Btn, Icon, IconView, LoadState, Loaded, Segmented, SegmentedOption, Size, ToastBus, ToastKind,
    Variant,
};

#[component]
#[allow(clippy::too_many_lines)]
pub fn SchemaPage() -> impl IntoView {
    let bus = expect_context::<ToastBus>();
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
    let compact = RwSignal::new(false);
    let compact_sig: Signal<bool> = Signal::derive(move || compact.get());

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

    let on_new_extractor = Callback::new(move |()| {
        bus.push(
            ToastKind::Info,
            "New extractor",
            Some("Field extractor authoring is landing soon.".into()),
        );
    });

    let (today, yesterday) = today_yesterday_utc();

    view! {
        <div class="page">
            <div class="page-hd compact">
                <div>
                    <h1>"Schema · Services"</h1>
                    <p class="sub">"Click a card to inspect fields, ingest rate, and tail live."</p>
                </div>
                <div class="actions">
                    <div class="inp-wrap">
                        <IconView icon=Icon::Search size=12 stroke_width=1.5/>
                        <input
                            placeholder="filter services…"
                            prop:value=move || filter.get()
                            on:input=move |e| filter.set(event_target_value(&e))
                        />
                    </div>
                    <Segmented
                        size=Size::Sm
                        options=vec![
                            SegmentedOption::new("comfy", "Comfy"),
                            SegmentedOption::new("compact", "Compact"),
                        ]
                        active=Signal::derive(move || {
                            if compact.get() { "compact" } else { "comfy" }.to_string()
                        })
                        on_change=Callback::new(move |id: String| compact.set(id == "compact"))
                        attr:title="Card density"
                    />
                    <Btn variant=Variant::Primary on_click=on_new_extractor>"+ New extractor"</Btn>
                </div>
            </div>

            <div class=move || if compact.get() { "sc-grid compact" } else { "sc-grid" }>
                <Loaded
                    state=Signal::derive(move || LoadState::from_resource(services.get()))
                    label="schema"
                    render=Box::new(move |resp: trawl_api::ServiceSchemaResponse| {
                        let needle = filter.get().to_lowercase();
                        let visible: Vec<ServiceSchema> = resp.services.iter()
                            .filter(|s| {
                                if needle.is_empty() { return true; }
                                if s.name.to_lowercase().contains(&needle) { return true; }
                                s.columns.iter().any(|c| c.name.to_lowercase().contains(&needle))
                            })
                            .cloned()
                            .collect();
                        if visible.is_empty() {
                            return view! {
                                <div class="sc-more" style="padding:24px">
                                    {if resp.services.is_empty() {
                                        "no services yet — ingest some logs and they'll appear here"
                                    } else {
                                        "no services match that filter"
                                    }}
                                </div>
                            }.into_any();
                        }
                        let today = today.clone();
                        let yesterday = yesterday.clone();
                        visible.into_iter().map(|svc| {
                            let name_for_active = svc.name.clone();
                            let active_sig: Signal<bool> = Signal::derive(move || {
                                svc_selected.get().as_deref() == Some(name_for_active.as_str())
                            });
                            view! {
                                <ServiceCard
                                    svc=svc
                                    compact=compact_sig
                                    active=active_sig
                                    today=today.clone()
                                    yesterday=yesterday.clone()
                                    on_open=on_open
                                    on_search=on_search
                                    on_tail=on_tail
                                />
                            }
                        }).collect::<Vec<_>>().into_any()
                    })
                />
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
