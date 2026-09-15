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
//!   (default: overview). Read through
//!   [`schema_nav::sanitize_tab`](crate::schema_nav::sanitize_tab): the
//!   value is re-concatenated into the drill-in URL below, so only a
//!   spelling from the closed vocabulary is ever carried forward.
//! - `field=<name>` — opens the field case file, which takes precedence
//!   over `svc=` and mounts independently of the services snapshot, so
//!   `/search/schema?field=<name>` is a working deep link. `svc=` is
//!   carried alongside as the back-arrow's return context.
//!
//! Exactly one drawer is mounted at a time: `fleet_ui::Drawer` arbitrates
//! Escape on the assumption of a single drawer layer, so the page swaps
//! between the two rather than nesting them. That swap is also why focus
//! is restored by hand: the originating badge is unmounted while the case
//! file is up, so the drawer shell's own opener-restore has nothing left
//! to return to.

use leptos::prelude::*;
use leptos::web_sys;
use leptos_router::NavigateOptions;
use leptos_router::hooks::{use_navigate, use_query_map};
use leptos_use::use_media_query;
use trawl_api::ServiceSchema;
use trawl_core::parser::suggest::quote_dsl_field;
use wasm_bindgen::JsCast as _;

use crate::api;
// One URL query-parameter value. A service name's charset is narrow but
// a catalog field name is any ASCII-folded client JSON key — shared with
// the query notice's case-file links so both encode a name identically.
use crate::components::enc_uri as enc;
use crate::components::field_case_drawer::FieldCaseDrawer;
use crate::components::service_card_fmt::{
    degraded_count, format_bytes, format_count, is_healthy, today_yesterday_utc,
};
use crate::components::service_drawer::ServiceDrawer;
use crate::components::sort_th::table_sort_th;
use crate::schema_nav::{BackNav, DEFAULT_SCHEMA_TAB, back_nav_stack, sanitize_tab};
use crate::state::query::{Mode, RangeSpec, navigator, report_refusal};
use fleet_ui::{
    Badge, Icon, IconView, LoadState, Loaded, Pager, SearchInput, Sparkline, StatusDot, StatusTone,
    ToastBus, Tone,
};

/// Days of `daily_event_counts` history shown in the activity sparkline.
const SPARK_DAYS: usize = 30;

/// The page heading's element id — the focus target when the case file
/// closes outright (nothing is left on screen to return focus to).
const HEADING_ID: &str = "schema-heading";

/// Move keyboard focus to `id` on the next frame — after the swap this
/// call is part of has actually rendered.
fn focus_on_next_frame(id: &'static str) {
    request_animation_frame(move || {
        if let Some(el) = document()
            .get_element_by_id(id)
            .and_then(|el| el.dyn_into::<web_sys::HtmlElement>().ok())
        {
            let _ = el.focus();
        }
    });
}

/// Whether a service survives the list filter: its own name, or any of
/// the field names it carries. Named once because the sheet header
/// counts exactly what the table renders.
fn matches_filter(svc: &ServiceSchema, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    svc.name.to_lowercase().contains(needle)
        || svc
            .columns
            .iter()
            .any(|c| c.name.to_lowercase().contains(needle))
}

/// Sort key for the services table. Clicking the active header flips
/// direction; a fresh key starts at its natural direction (name
/// ascending, everything else descending).
#[derive(Clone, Copy, PartialEq, Eq)]
enum SvcSort {
    Name,
    Events,
    Storage,
    Fields,
}

impl SvcSort {
    fn default_desc(self) -> bool {
        !matches!(self, Self::Name)
    }
}

#[component]
#[allow(clippy::too_many_lines)]
pub fn SchemaPage() -> impl IntoView {
    let table_viewport = NodeRef::<leptos::html::Div>::new();
    let qm = use_query_map();
    // An empty param is an absent one: `?svc=` / `?field=` reach here
    // from a hand-edited URL, and an empty name resolves to nothing.
    let svc_selected = Memo::new(move |_| qm.get().get("svc").filter(|s| !s.is_empty()));
    let field_selected = Memo::new(move |_| qm.get().get("field").filter(|s| !s.is_empty()));
    // Closed vocabulary at read time: the resolved tab is re-emitted into
    // the drill-in URL, so a raw `?stab=` could otherwise append a second
    // `field=` parameter to it (see `schema_nav::sanitize_tab`).
    let tab_param = Memo::new(move |_| sanitize_tab(qm.get().get("stab").as_deref()));
    let tab_sig: Signal<String> = Signal::derive(move || tab_param.get().to_string());

    // Whether to offer the repin trigger on a degraded field's case
    // file. Affordance only — the server gates `POST /schema/repin` on
    // `schema_write` and is the sole enforcement — and read-only until
    // `/me` resolves, so a slow identity fetch never flashes a button
    // the session may not have.
    let me = use_context::<RwSignal<Option<api::MeResponse>>>();
    let can_repin = Signal::derive(move || {
        me.and_then(|me| me.get())
            .is_some_and(|me| crate::perms::can_schema_write(&me.permissions))
    });

    let filter = RwSignal::new(String::new());
    let sort = RwSignal::new((SvcSort::Name, false));

    let services = LocalResource::new(|| async move { api::schema_services().await });

    // Past this width the service panel docks in the second column
    // instead of sliding over a scrim; the field case file stays an
    // overlay at every width (ADR-0032), so it never claims the column.
    let wide = use_media_query("(min-width: 1100px)");
    let panel_open = Signal::derive(move || {
        field_selected.get().is_none()
            && svc_selected.get().is_some_and(|name| {
                matches!(services.get(), Some(Ok(resp))
                    if resp.services.iter().any(|s| s.name == name))
            })
    });

    // Captured up-front — `use_navigate()` panics outside the router
    // reactive context (i.e. inside deferred callbacks).
    let nav = use_navigate();
    let goto_search = navigator();
    // Only for a refused navigation into /search: this page has no
    // toasts of its own, and a click that quietly does nothing is the
    // thing the search page's own producers stopped doing.
    let bus = expect_context::<ToastBus>();

    // The field history entries this page pushed, innermost last. The
    // case file's back affordance consults the top: popping an entry we
    // did not push would be someone else's history, and replacing one we
    // did push leaves a duplicate service entry per drill/back cycle. A
    // stack rather than one slot because a case file links to another
    // case file (the "a repin is running on X" line), so A → B → A would
    // have the third push overwrite the first. See
    // `schema_nav::back_nav_stack` for the residual a browser-initiated
    // Back leaves.
    let pushed_fields = RwSignal::new(Vec::<String>::new());
    // The field badge to hand focus back to when the service drawer
    // remounts underneath a closing case file.
    let focus_field = RwSignal::new(None::<String>);

    let push_svc = {
        let nav = nav.clone();
        move |name: Option<&str>, stab: &str| {
            let url = match name {
                Some(n) => format!(
                    "/search/schema?svc={}&stab={}",
                    enc(n),
                    sanitize_tab(Some(stab))
                ),
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

    // The `?svc=&stab=` half of a URL — the case file's return context,
    // carried on the drill-in and restored by a back that has to replace.
    let return_ctx = move || {
        svc_selected
            .get_untracked()
            .map(|svc| format!("svc={}&stab={}", enc(&svc), tab_param.get_untracked()))
    };

    // Drilling into a field case file pushes, so browser-back leaves the
    // case file for wherever the operator came from.
    let push_field = {
        let nav = nav.clone();
        move |field: &str| {
            let mut url = format!("/search/schema?field={}", enc(field));
            if let Some(ctx) = return_ctx() {
                url.push('&');
                url.push_str(&ctx);
            }
            pushed_fields.update(|stack| stack.push(field.to_string()));
            focus_field.set(Some(field.to_string()));
            nav(&url, NavigateOptions::default());
        }
    };

    // Leaving the case file by its back arrow. Origin-aware, because the
    // two cases are different history shapes: an entry we pushed is popped
    // (replacing it would leave the return URL twice over, so one browser
    // Back per drill/back cycle would go nowhere), while a deep link's
    // entry is not ours to pop and is replaced with the return context.
    let back_field = {
        let nav = nav.clone();
        move || {
            let shown = field_selected.get_untracked();
            // The decision consumes the entry it pops, so the next back
            // decides against the one below it rather than against a
            // name three drill-ins old.
            let decision = shown.as_deref().map_or(BackNav::Replace, |f| {
                let mut decision = BackNav::Replace;
                pushed_fields.update(|stack| decision = back_nav_stack(f, stack));
                decision
            });
            if decision == BackNav::Pop
                && let Some(history) = web_sys::window().and_then(|w| w.history().ok())
                && history.back().is_ok()
            {
                return;
            }
            let url = return_ctx().map_or_else(
                || "/search/schema".to_string(),
                |ctx| format!("/search/schema?{ctx}"),
            );
            nav(
                &url,
                NavigateOptions {
                    replace: true,
                    ..Default::default()
                },
            );
        }
    };

    let on_open_field: Callback<String> = Callback::new(move |field: String| push_field(&field));
    let on_field_back: Callback<()> = Callback::new(move |()| back_field());

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
        Callback::new(move |()| {
            focus_field.set(None);
            push(None, DEFAULT_SCHEMA_TAB);
        })
    };
    // Closing the case file outright (its X, the scrim, or Escape with no
    // service to go back to) clears every param, so nothing the drawer was
    // opened from is left mounted for the shell's own opener-restore to
    // find. Focus lands on the page heading rather than the document.
    let on_field_close: Callback<()> = {
        let push = push_svc.clone();
        Callback::new(move |()| {
            focus_field.set(None);
            // Every param is cleared, so nothing on screen stands on one
            // of those entries any more: what is left of them in the
            // history is not ours to pop.
            pushed_fields.set(Vec::new());
            push(None, DEFAULT_SCHEMA_TAB);
            focus_on_next_frame(HEADING_ID);
        })
    };

    let on_search: Callback<String> = {
        let goto = goto_search.clone();
        Callback::new(move |name: String| {
            let q = format!(r#"service="{}""#, name.replace('"', ""));
            report_refusal(
                bus,
                goto(&q, 0, Mode::Snapshot, &[], &RangeSpec::default(), false),
            );
        })
    };

    let on_use_field: Callback<String> = {
        let goto = goto_search.clone();
        Callback::new(move |field: String| {
            // Wildcard match — `field=*` selects every event carrying
            // the field, which is what the user usually wants after
            // drilling in from a schema view. The name goes through the
            // DSL renderer — an ordinary ingested name the bare
            // production can't spell needs backticks or the clause
            // parses as a text search (ADR-0013 ruling 7).
            let Some(field) = quote_dsl_field(&field) else {
                return;
            };
            let q = format!("{field}=*");
            report_refusal(
                bus,
                goto(&q, 0, Mode::Snapshot, &[], &RangeSpec::default(), false),
            );
        })
    };

    let (today, yesterday) = today_yesterday_utc();

    view! {
        <div class="page">
            <div class="page-hd compact">
                <div>
                    // `tabindex=-1` so the close handler above can put
                    // focus here — headings are not focusable by default.
                    <h1 id=HEADING_ID tabindex="-1">"Schema"</h1>
                    <p class="sub">"Click a service to inspect fields, ingest rate, and view live logs."</p>
                </div>
            </div>

            <div class="page-split" class:has-panel=move || panel_open.get()>
            <section class="list-sheet" aria-labelledby="schema-sheet-title">
                <div class="list-sheet-hd">
                    <h2 id="schema-sheet-title" class="list-sheet-ttl">
                        "Services"
                    </h2>
                    <SearchInput value=filter placeholder="Filter services…"/>
                </div>
            <fleet_ui::OverflowHint viewport=table_viewport/>
                <div node_ref=table_viewport class="tbl fleet-table-frame tbl-scroll" role="region" aria-label="Services table" tabindex="0" style="--list-min-width:560px">
                <div class="tbl-body">
                    <Loaded
                        state=Signal::derive(move || LoadState::from_resource(services.get()))
                        label="schema"
                        retry=Callback::new(move |()| { services.set(None); services.refetch(); })
                        render=Box::new(move |resp: trawl_api::ServiceSchemaResponse| {
                            let needle = filter.get().to_lowercase();
                            let mut visible: Vec<ServiceSchema> = resp.services.iter()
                                .filter(|s| matches_filter(s, &needle))
                                .cloned()
                                .collect();
                            if visible.is_empty() {
                                return view! {
                                    <div class="tbl-empty">
                                        {if resp.services.is_empty() {
                                            "No services yet — ingest some logs and they'll appear here"
                                        } else {
                                            "No services match that filter"
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
                                    SvcSort::Events => a.total_events.cmp(&b.total_events),
                                    SvcSort::Storage => a.total_bytes.cmp(&b.total_bytes),
                                    SvcSort::Fields => a.columns.len().cmp(&b.columns.len()),
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
                                let events_label = format_count(svc.total_events);
                                let storage_label = format_bytes(svc.total_bytes);
                                let field_count = svc.columns.len();
                                let degraded = degraded_count(&svc);

                                let name_for_active = name.clone();
                                // The row's one control is a link: the
                                // drawer is a place with a URL, built by
                                // the same encoder and default tab
                                // `push_svc` writes, and `prop:replace`
                                // below is that call's `replace: true`.
                                let href = format!(
                                    "/search/schema?svc={}&stab={}",
                                    enc(&name),
                                    sanitize_tab(Some(DEFAULT_SCHEMA_TAB)),
                                );
                                let name_search = name.clone();
                                let name_tail = name.clone();
                                let name_label_search = name.clone();
                                let name_label_tail = name.clone();
                                view! {
                                        <tr
                                        class="tbl-row"
                                        class:active=move || {
                                            svc_selected.get().as_deref()
                                                == Some(name_for_active.as_str())
                                        }
                                    >
                                            <td class="svc-cell" style="min-width:0">
                                            <a
                                                class="row-stretch"
                                                href=href
                                                prop:replace=true
                                                // Not navigation and not a
                                                // preventDefault: a drawer
                                                // opened from the table has
                                                // no pending return, and a
                                                // focus request left over
                                                // from an abandoned drill-in
                                                // would steal focus into this
                                                // service's fields.
                                                on:click=move |_| focus_field.set(None)
                                            >
                                                <span class="mono name">{name}</span>
                                            </a>
                                                <span class="freshness"><StatusDot tone=dot_tone/>" "{crate::service_card_fmt::freshness_label(&svc)}</span>
                                            // Count, not colour alone: the badge
                                            // says how many of this service's
                                            // fields the catalog calls degraded.
                                            {(degraded > 0).then(|| view! {
                                                <Badge tone=Tone::Warn>
                                                    {format!("{degraded} degraded")}
                                                </Badge>
                                            })}
                                            </td>
                                            <td>
                                            <Sparkline data=spark_data color=spark_color w=96 h=16/>
                                            </td>
                                            <td class="num">{events_label}</td>
                                            <td class="num">{storage_label}</td>
                                            <td class="num">{field_count}</td>
                                            <td class="row-act">
                                            // Commands, not places: both go
                                            // through the navigator, which
                                            // can refuse an over-bound query
                                            // (ADR-0027), and an anchor has
                                            // no way to say no.
                                            <button
                                                type="button"
                                                class="qa"
                                                aria-label=format!("Search {name_label_search}")
                                                on:click=move |_| on_search.run(name_search.clone())
                                            >
                                                <IconView icon=Icon::Search size=12 stroke_width=1.5/>
                                            </button>
                                            <button
                                                type="button"
                                                class="qa"
                                                aria-label=format!("Live tail {name_label_tail}")
                                                on:click=move |_| on_tail.run(name_tail.clone())
                                            >
                                                <IconView icon=Icon::Bolt size=12 stroke_width=1.5/>
                                            </button>
                                            </td>
                                        </tr>
                                }
                            }).collect::<Vec<_>>();
                            view! {
                                    <table class="fleet-table schema-table" aria-label="Services">
                                        <thead><tr>
                                            {table_sort_th(sort, SvcSort::Name, SvcSort::Name.default_desc(), "Service", "")}
                                            <th scope="col" class="th" style="width:130px">"Activity"</th>
                                            {table_sort_th(sort, SvcSort::Events, SvcSort::Events.default_desc(), "Events", "width:84px; text-align:right")}
                                            {table_sort_th(sort, SvcSort::Storage, SvcSort::Storage.default_desc(), "Storage", "width:100px; text-align:right")}
                                            {table_sort_th(sort, SvcSort::Fields, SvcSort::Fields.default_desc(), "Fields", "width:76px; text-align:right")}
                                            <th scope="col" style="width:84px"><span class="sr-only">Actions</span></th>
                                        </tr></thead>
                                        <tbody>{rows}</tbody></table>
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
            </section>

            {move || {
                // `?field=` wins, and mounts without consulting the
                // services resource — that independence is what makes a
                // bare `/search/schema?field=x` deep link work. A dead
                // `?svc=` alongside it costs the back arrow, nothing more.
                if let Some(field) = field_selected.get() {
                    return view! {
                        <FieldCaseDrawer
                            field=field
                            back=svc_selected.get()
                            can_repin=can_repin
                            on_open_field=on_open_field
                            on_back=on_field_back
                            on_close=on_field_close
                        />
                    }.into_any();
                }
                let Some(selected) = svc_selected.get() else { return ().into_any(); };
                let resp = match services.get() {
                    None => return view! { <p role="status">"Loading service…"</p> }.into_any(),
                    Some(Err(_)) => return view! { <p role="alert">"Could not load this service. Please retry." " " <a href="/search/schema">"Back to Schema"</a></p> }.into_any(),
                    Some(Ok(resp)) => resp,
                };
                let Some(svc) = resp.services.iter().find(|s| s.name == selected).cloned()
                    else { return view! { <p role="status">"Service not found. It may have been deleted." " " <a href="/search/schema">"Back to Schema"</a></p> }.into_any(); };
                view! {
                    <ServiceDrawer
                        svc=svc
                        tab=tab_sig
                        focus_field=focus_field
                        on_close=on_close
                        on_tab_change=on_tab_change
                        on_search=on_search
                        on_use_field=on_use_field
                        on_open_field=on_open_field
                        docked=wide
                    />
                }.into_any()
            }}
            </div>
        </div>
    }
}
