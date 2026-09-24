// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<ServiceDrawer/>` — slide-out right-side inspector for a single
//! service on the Schema page. Scrim click + `Esc` to close.
//!
//! Three tabs, URL-synced via `?stab=overview|fields|tail`:
//! - Overview: 24h ingest histogram + field-type donut + top-fields by
//!   cardinality. Cardinality comes from a `stats dc(f1) as c0, …`
//!   DSL query fired once per drawer open and cached for the lifetime;
//!   its answer is read back by POSITION, keyed onto the field names the
//!   builder returned beside the DSL.
//! - Fields: sortable table of all columns with click-to-expand rows.
//!   Expanding a row fires `service=<name> | top 10 <field>` via DSL
//!   to render a top-values bar chart.
//! - Live tail: opens an `EventSource` filtered to `service=<name>`
//!   reusing the plumbing in `state::stream_session`. Pause closes the
//!   stream; resume re-opens it. Closing the drawer tears it down.
//!
//! Cardinality and the histogram both live at the drawer level so they
//! survive tab switches (Overview's top-fields widget and Fields' table
//! column both consume the same cached signal).

use std::collections::HashMap;

use leptos::prelude::*;
use trawl_api::value::{QueryResult, Value};
use trawl_api::{QueryResponse, ServiceColumnStats, ServiceSchema};

use wasm_bindgen::{JsCast, JsValue};

use crate::api;
use crate::components::sort_th::sort_th;
use crate::drawer_query::{decode_cardinality, value_as_u64};
use crate::fetch_plan::FetchPlan;
use crate::histogram::{Slot, align_buckets, parse_bucket_ms};
use crate::interop::uplot::{ChartHandle, ChartKind, Opts, create_chart};
use crate::state::stream_session::{LiveSignals, RingBuffer, StreamLifecycle, start_stream};
use fleet_ui::{
    Badge, Btn, Drawer, Icon, IconView, LoadState, Loaded, TabItem, ToastBus, ToastKind, Tone,
    Variant, effective_active,
};

/// Display cap for the live-tail viewport — keeps the DOM snappy. The
/// ring underneath still holds up to `LIVE_RING_CAPACITY` events.
const TAIL_DISPLAY_MAX: usize = 150;

/// Fields shown in the Overview "Top fields by cardinality" card.
const TOP_CARDINALITY_ROWS: usize = 6;

/// Ingest chart grid: 24 hourly slots, matching the `last=24h |
/// timechart span=1h` query that feeds it.
const INGEST_SLOT_MS: i64 = 3_600_000;
const INGEST_SLOTS: usize = 24;

/// Ingest chart canvas size. uPlot needs explicit pixels — height is
/// fixed by the card's slot in the overview grid; width is measured off
/// the mounted div, with this as the unmeasurable-ancestor fallback.
const INGEST_CHART_H: f64 = 132.0;
const INGEST_CHART_W: f64 = 360.0;

#[component]
#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
pub fn ServiceDrawer(
    svc: ServiceSchema,
    tab: Signal<String>,
    on_close: Callback<()>,
    on_tab_change: Callback<String>,
    on_search: Callback<String>,
    on_use_field: Callback<String>,
    /// A field whose degraded badge should take focus once this drawer
    /// has mounted — set by the page when a drill-in unmounted the badge,
    /// so returning from the case file lands where it left. Cleared by
    /// the fields pane once it has been honoured.
    focus_field: RwSignal<Option<String>>,
    /// Drill into a field's case file. The page owns the navigation; the
    /// drawer only names the field.
    on_open_field: Callback<String>,
    /// Render in flow beside the services list rather than over a scrim
    /// (ADR-0032). The page feeds its viewport query straight in; the
    /// panes mount once and stay mounted across the breakpoint.
    #[prop(into, optional)]
    docked: Signal<bool>,
) -> impl IntoView {
    let bus = expect_context::<ToastBus>();
    let svc_for_card = svc.clone();
    let svc_for_head = svc.clone();
    let svc_for_over = svc.clone();
    let svc_for_fields = svc.clone();
    let svc_for_tail = svc.clone();
    let name_for_search = svc.name.clone();

    // Shared cardinality fetch: runs once on drawer mount for this
    // service. Signal is exposed to both OverviewPane and FieldsPane so
    // neither re-fetches on tab switch.
    let cardinality = cardinality_resource(svc_for_card);
    let cardinality_sig: Signal<Option<Result<HashMap<String, u64>, String>>> =
        Signal::derive(move || {
            cardinality
                .get()
                .map(|r| r.clone().map_err(|e| e.to_string()))
        });

    // Drawer header sub-text: show date range when available.
    let sub_text = match (svc.earliest_date.as_deref(), svc.latest_date.as_deref()) {
        (Some(a), Some(b)) if a == b => a.to_string(),
        (Some(a), Some(b)) => format!("{a} → {b}"),
        _ => String::new(),
    };

    let on_search_click = Callback::new(move |()| on_search.run(name_for_search.clone()));
    let on_tail_click = Callback::new(move |()| on_tab_change.run("tail".to_string()));

    // Drawer compares ids verbatim; map the URL signal's empty default.
    let eff_tab = effective_active(tab, "overview");

    view! {
        <Drawer
            tabs=vec![
                TabItem::new("overview", "Overview"),
                TabItem::new("fields", "Fields"),
                TabItem::new("tail", "Live Tail"),
            ]
            tabs_label="Service details"
            label=svc.name.clone()
            active_tab=eff_tab
            on_tab_change=on_tab_change
            on_close=on_close
            docked=docked
            // Deliberately not the 14px default; the `close_size` prop
            // docs in fleet-ui explain why the sizes are not unified.
            close_size=12
            title=Box::new(move || view! {
                <span class="name">{svc_for_head.name.clone()}</span>
                {(!sub_text.is_empty()).then_some(view! {
                    <span class="sub">{sub_text}</span>
                })}
            }.into_any())
            actions=Box::new(move || view! {
                <Btn variant=Variant::Secondary on_click=on_search_click>
                    <IconView icon=Icon::Search size=11 stroke_width=1.5/> " Search this service"
                </Btn>
                <Btn variant=Variant::Secondary on_click=on_tail_click>
                    <IconView icon=Icon::Bolt size=11 stroke_width=1.5/> " Live Tail"
                </Btn>
            }.into_any())
        >
            {move || {
                if eff_tab.get() == "fields" {
                    view! {
                        <FieldsPane
                            svc=svc_for_fields.clone()
                            cardinality=cardinality_sig
                            on_retry_cardinality=Callback::new(move |()| { cardinality.set(None); cardinality.refetch(); })
                            focus_field=focus_field
                            on_use_field=on_use_field
                            on_open_field=on_open_field
                        />
                    }.into_any()
                } else if eff_tab.get() == "tail" {
                    view! {
                        <TailPane svc=svc_for_tail.clone() bus=bus/>
                    }.into_any()
                } else {
                    view! {
                        <OverviewPane
                            svc=svc_for_over.clone()
                            cardinality=cardinality_sig
                            on_retry_cardinality=Callback::new(move |()| { cardinality.set(None); cardinality.refetch(); })
                            on_use_field=on_use_field
                        />
                    }.into_any()
                }
            }}
        </Drawer>
    }
}

// ───────────────────────── Overview pane ─────────────────────────

#[component]
#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
fn OverviewPane(
    svc: ServiceSchema,
    cardinality: Signal<Option<Result<HashMap<String, u64>, String>>>,
    on_retry_cardinality: Callback<()>,
    on_use_field: Callback<String>,
) -> impl IntoView {
    let svc_name = svc.name.clone();
    let histogram = LocalResource::new(move || {
        let q = format!(
            r#"service="{}" last=24h | timechart span=1h count()"#,
            svc_name.replace('"', "")
        );
        async move { api::query(&q, FetchPlan::for_query(&q, 0)).await }
    });

    // Field-type donut is pure — derive from columns synchronously.
    let donut_segments = donut_breakdown(&svc.columns);
    let field_count = svc.columns.len();

    let columns_for_top = svc.columns.clone();

    view! {
        <div class="sd-overview">
            <div class="sd-card chart">
                <div class="ttl">"Ingest · last 24h"</div>
                <HistogramChart resource=histogram/>
            </div>

            <div class="sd-card types">
                <div class="ttl">"Field types"</div>
                <FieldTypeDonut segments=donut_segments total=field_count/>
            </div>

            <div class="sd-card top">
                <div class="ttl">"Top fields by cardinality"</div>
                <Loaded
                    state=Signal::derive(move || LoadState::from_resource(cardinality.get()))
                    label="cardinality"
                    retry=on_retry_cardinality
                    render=Box::new(move |card_map: HashMap<String, u64>| {
                        let rows = top_cardinality_rows(&columns_for_top, &card_map, TOP_CARDINALITY_ROWS);
                        if rows.is_empty() {
                            return view! {
                                <div class="sc-more">"No distinct values observed yet"</div>
                            }.into_any();
                        }
                        let max = rows.iter().map(|r| r.2).max().unwrap_or(1).max(1);
                        let rows = rows.into_iter().map(|(fname, type_label, count)| {
                            let (pill_class, pill_text) = super::service_card_fmt::type_pill(&type_label);
                            #[allow(clippy::cast_precision_loss)]
                            let pct = (count as f64 / max as f64) * 100.0;
                            let bar_style = format!("width:{pct:.1}%");
                            let label = super::service_card_fmt::format_count(count);
                            let fname_cb = fname.clone();
                            view! {
                                <div class="tf">
                                    // A command, not a place: it builds a
                                    // search through the navigator, which can
                                    // refuse (ADR-0027).
                                    <button
                                        type="button"
                                        class="fn row-stretch"
                                        on:click=move |_| on_use_field.run(fname_cb.clone())
                                    >{fname}</button>
                                    <span class=format!("tp {pill_class}")>{pill_text}</span>
                                    <span class="bar-wrap"><span class="bar" style=bar_style></span></span>
                                    <span class="c">{label}</span>
                                </div>
                            }
                        }).collect::<Vec<_>>();
                        // The wrapper the `.topfields .tf` rules have always
                        // asked for: the rows carried both classes and had no
                        // `.topfields` ancestor, so none of it matched.
                        view! { <div class="topfields">{rows}</div> }.into_any()
                    })
                />
            </div>
        </div>
    }
}

// ───────────────────────── Fields pane ─────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortKey {
    Name,
    Cardinality,
    NonNull,
    Storage,
}

impl SortKey {
    /// The direction a freshly-selected key starts in: ascending for
    /// the field name, descending for the numeric columns.
    fn default_desc(self) -> bool {
        !matches!(self, Self::Name)
    }
}

#[component]
#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
fn FieldsPane(
    svc: ServiceSchema,
    cardinality: Signal<Option<Result<HashMap<String, u64>, String>>>,
    on_retry_cardinality: Callback<()>,
    focus_field: RwSignal<Option<String>>,
    on_use_field: Callback<String>,
    on_open_field: Callback<String>,
) -> impl IntoView {
    // Second element is DESCENDING, matching `sort_th`'s tuple.
    let sort = RwSignal::new((SortKey::Cardinality, true));
    let expanded = RwSignal::new(None::<String>);
    let svc_name = svc.name.clone();
    let columns_owned = svc.columns.clone();
    let svc_for_degraded = svc.clone();

    // Focus return across the drawer swap. The badge's id is its position
    // in `columns`, which is the drawer's own order and survives this
    // table's client-side sorting.
    //
    // Re-run on `cardinality`, not once on mount: the row list is rebuilt
    // when the cardinality fetch lands, which would drop a focus placed
    // before it. The request is only cleared once that fetch has settled,
    // so the last attempt is the one that sticks.
    let columns_for_focus = svc.columns.clone();
    Effect::new(move |_| {
        let settled = cardinality.get().is_some();
        let Some(target) = focus_field.get() else {
            return;
        };
        if settled {
            focus_field.set(None);
        }
        let Some(index) = columns_for_focus.iter().position(|c| c.name == target) else {
            focus_field.set(None);
            return;
        };
        let id = crate::schema_nav::degraded_badge_id(index);
        request_animation_frame(move || {
            if let Some(el) = document()
                .get_element_by_id(&id)
                .and_then(|el| el.dyn_into::<web_sys::HtmlElement>().ok())
            {
                let _ = el.focus();
            }
        });
    });

    view! {
        <div class="sd-fields">
            {move || cardinality.get().and_then(Result::err).map(|msg| view! {
                <div class="load-hint error">
                    <div>{format!("Couldn't load cardinality: {msg}")}</div>
                    <div class="load-recovery"><Btn variant=Variant::Secondary on_click=on_retry_cardinality>"Retry"</Btn></div>
                </div>
            })}
            <div class="sf-hd">
                {sort_th(sort, SortKey::Name, SortKey::Name.default_desc(), "Field", "")}
                <div>"Type"</div>
                {sort_th(sort, SortKey::NonNull, SortKey::NonNull.default_desc(), "Non-null", "")}
                {sort_th(sort, SortKey::Cardinality, SortKey::Cardinality.default_desc(), "Cardinality", "")}
                {sort_th(sort, SortKey::Storage, SortKey::Storage.default_desc(), "Storage", "")}
                <div>"Sample"</div>
                <div></div>
            </div>

            {move || {
                let (key, desc) = sort.get();
                let card_map = cardinality.get()
                    .and_then(Result::ok)
                    .unwrap_or_default();
                let mut sorted = columns_owned.clone();
                sorted.sort_by(|a, b| {
                    let ord = match key {
                        SortKey::Name => a.name.cmp(&b.name),
                        SortKey::Cardinality => {
                            let ac = card_map.get(&a.name).copied().unwrap_or(0);
                            let bc = card_map.get(&b.name).copied().unwrap_or(0);
                            ac.cmp(&bc)
                        }
                        SortKey::NonNull => {
                            let af = non_null_ratio(a);
                            let bf = non_null_ratio(b);
                            af.partial_cmp(&bf).unwrap_or(std::cmp::Ordering::Equal)
                        }
                        SortKey::Storage => a.compressed_bytes.cmp(&b.compressed_bytes),
                    };
                    if desc { ord.reverse() } else { ord }
                });
                sorted.into_iter().map(|c| {
                    let fname = c.name.clone();
                    let fname_for_click = fname.clone();
                    let fname_for_detail = fname.clone();
                    let (pill_class, pill_text) = super::service_card_fmt::type_pill(&c.data_type);
                    let cov_pct = super::service_card_fmt::cov_pct(c.null_count, c.total_count);
                    let cov_style = format!("width:{cov_pct}%");
                    let card_count = card_map.get(&c.name).copied();
                    let card_label = card_count.map_or_else(|| "…".to_string(), super::service_card_fmt::format_count);
                    let storage = super::service_card_fmt::format_bytes(c.compressed_bytes);
                    let sample = sample_range(&c);
                    let is_open = {
                        let fname = fname.clone();
                        move || expanded.get().as_ref() == Some(&fname)
                    };
                    let row_class = {
                        let is_open = is_open.clone();
                        move || if is_open() { "sf-row open" } else { "sf-row" }
                    };
                    let toggle = move |_| {
                        let f = fname_for_click.clone();
                        if expanded.get_untracked().as_ref() == Some(&f) {
                            expanded.set(None);
                        } else {
                            expanded.set(Some(f));
                        }
                    };
                    let svc_name_detail = svc_name.clone();
                    let col_for_detail = c.clone();

                    let is_open_caret = is_open.clone();
                    let is_open_detail = is_open.clone();
                    let is_open_aria = is_open.clone();
                    // Membership in the server-stamped list, never a join
                    // against the install-wide degraded set.
                    let degraded = super::service_card_fmt::is_degraded_column(
                        &svc_for_degraded,
                        &c.name,
                    );
                    let fname_for_open = fname.clone();
                    // Focus-return target across the case-file swap: the
                    // column's position in the drawer's own list, never
                    // its client-chosen name.
                    let badge_id = columns_owned
                        .iter()
                        .position(|col| col.name == c.name)
                        .map(crate::schema_nav::degraded_badge_id);
                    view! {
                        <>
                            <div class=row_class>
                                <div>
                                    // The row's one control (ADR-0029): the
                                    // caret and the field name live inside
                                    // it, and it stretches over the row.
                                    <button
                                        type="button"
                                        class="row-stretch"
                                        aria-expanded=move || is_open_aria().to_string()
                                        on:click=toggle
                                    >
                                        <span class="caret" aria-hidden="true">
                                            {move || if is_open_caret() { "▾" } else { "▸" }}
                                        </span>
                                        <span class="c-name">{c.name.clone()}</span>
                                    </button>
                                    {degraded.then(|| view! {
                                        // Above the stretched control, so it
                                        // opens the case file without
                                        // expanding the row.
                                        <button
                                            type="button"
                                            class="deg-btn"
                                            id=badge_id
                                            title="Degraded pin — open the field case file"
                                            on:click=move |_| {
                                                on_open_field.run(fname_for_open.clone());
                                            }
                                        >
                                            <Badge tone=Tone::Warn>"Degraded"</Badge>
                                        </button>
                                    })}
                                </div>
                                <div><span class=format!("tp {pill_class}")>{pill_text}</span></div>
                                <div class="c-cov">
                                    <span class="cov-meter"><span class="cov-fill" style=cov_style></span></span>
                                    <span class="cov-pct">{format!("{cov_pct}%")}</span>
                                </div>
                                <div class="c-card">{card_label}</div>
                                <div class="c-size">{storage}</div>
                                <div class="c-sample">{sample}</div>
                                <div></div>
                            </div>
                            {move || if is_open_detail() {
                                view! {
                                    <FieldDetail
                                        svc=svc_name_detail.clone()
                                        col=col_for_detail.clone()
                                        card_count=card_count
                                        on_use_field=on_use_field
                                        field_name=fname_for_detail.clone()
                                    />
                                }.into_any()
                            } else {
                                ().into_any()
                            }}
                        </>
                    }
                }).collect::<Vec<_>>()
            }}
        </div>
    }
}

// ───────────────────────── Field detail (expand row) ─────────────────────────

#[component]
#[allow(clippy::needless_pass_by_value)]
fn FieldDetail(
    svc: String,
    col: ServiceColumnStats,
    card_count: Option<u64>,
    on_use_field: Callback<String>,
    field_name: String,
) -> impl IntoView {
    // Composed once: the counts do not always arrive under `count` (a
    // field of that name takes the alias form), and the reader below has
    // to be told which column the query it actually sent counts into.
    let plan = crate::drawer_query::top_values_query(&svc, &field_name);
    let count_column = plan.as_ref().map_or("count", |p| p.count_column);
    let top_dsl = plan.map(|p| p.dsl);
    let top = LocalResource::new(move || {
        let q = top_dsl.clone();
        async move {
            match q {
                Some(q) => api::query(&q, FetchPlan::for_query(&q, 0)).await,
                None => Ok(declined_query_response()),
            }
        }
    });

    let cov_pct = super::service_card_fmt::cov_pct(col.null_count, col.total_count);
    let storage = super::service_card_fmt::format_bytes(col.compressed_bytes);
    let card_label =
        card_count.map_or_else(|| "—".to_string(), super::service_card_fmt::format_count);
    let range_label = match (col.min_value.as_deref(), col.max_value.as_deref()) {
        (Some(a), Some(b)) if a == b => a.to_string(),
        (Some(a), Some(b)) => format!("{a} → {b}"),
        _ => "—".to_string(),
    };
    let name_for_use = field_name.clone();

    view! {
        <div class="sf-detail">
            <div class="sfd-col">
                <div class="lb">"Top values"</div>
                <Loaded
                    state=Signal::derive(move || LoadState::from_resource(top.get()))
                    label="values"
                    retry=Callback::new(move |()| { top.set(None); top.refetch(); })
                    render=Box::new(move |resp: QueryResponse| {
                        let rows = parse_top_values(&resp, &field_name, count_column);
                        if rows.is_empty() {
                            return view! {
                                <div class="sc-more">"No values"</div>
                            }.into_any();
                        }
                        let max = rows.iter().map(|(_, c)| *c).max().unwrap_or(1).max(1);
                        rows.into_iter().map(|(val, count)| {
                            #[allow(clippy::cast_precision_loss)]
                            let pct = (count as f64 / max as f64) * 100.0;
                            let bar_style = format!("width:{pct:.1}%");
                            let count_label = super::service_card_fmt::format_count(count);
                            let val_title = val.clone();
                            view! {
                                <div class="sfd-tv">
                                    <span class="v" title=val_title>{val}</span>
                                    <span class="bar-wrap"><span class="bar" style=bar_style></span></span>
                                    <span class="c">{count_label}</span>
                                </div>
                            }
                        }).collect::<Vec<_>>().into_any()
                    })
                />
            </div>

            <div class="sfd-col">
                <div class="lb">"Stats"</div>
                <div class="sfd-kv"><span>"Cardinality"</span><span>{card_label}</span></div>
                <div class="sfd-kv"><span>"Non-null"</span><span>{format!("{cov_pct}%")}</span></div>
                <div class="sfd-kv"><span>"Storage"</span><span>{storage}</span></div>
                <div class="sfd-kv"><span>"Range"</span><span>{range_label}</span></div>
                <div class="sfd-actions">
                    <Btn
                        variant=Variant::Secondary
                        on_click=Callback::new(move |()| on_use_field.run(name_for_use.clone()))
                    >
                        "Use in query"
                    </Btn>
                </div>
            </div>
        </div>
    }
}

// ───────────────────────── Live tail pane ─────────────────────────

#[component]
#[allow(clippy::needless_pass_by_value)]
fn TailPane(svc: ServiceSchema, bus: ToastBus) -> impl IntoView {
    let ring = RwSignal::new(RingBuffer::default());
    let snapshot = RwSignal::new(None::<QueryResult>);
    let lagged = RwSignal::new(None::<u64>);
    let paused = RwSignal::new(false);
    let failure = RwSignal::new(Some("Connecting…"));
    let retry = RwSignal::new(0_u64);

    // Lifecycle holder — drop closes the SSE. `StreamLifecycle` owns a
    // `wasm_bindgen::Closure`, which isn't Send/Sync, so we need the
    // single-threaded `LocalStorage` variant (same reason as the
    // editor / chart handles in this crate).
    let lifecycle: StoredValue<Option<StreamLifecycle>, leptos::prelude::LocalStorage> =
        StoredValue::new_local(None);
    let svc_name_for_stream = svc.name.clone();
    let svc_name_for_toast = svc.name.clone();

    Effect::new(move |_| {
        let is_paused = paused.get();
        let _ = retry.get();
        if is_paused {
            lifecycle.set_value(None);
            return;
        }
        lifecycle.set_value(None);
        failure.set(Some("Connecting…"));
        let q = format!(
            r#"service="{}""#,
            crate::context_query::escape_dq(&svc_name_for_stream)
        );
        let sig = LiveSignals {
            ring,
            snapshot,
            lagged,
            failure: Some(failure),
            frames: None,
            opened: None,
        };
        if let Some(lc) = start_stream(&q, sig) {
            lifecycle.set_value(Some(lc));
        } else {
            failure.set(Some("Live stream unavailable."));
            bus.push(
                ToastKind::Error,
                "Tail unavailable",
                Some(format!(
                    "Couldn't open a live stream for {svc_name_for_toast}."
                )),
            );
        }
    });

    on_cleanup(move || {
        lifecycle.set_value(None);
    });

    let lagged_label = move || lagged.get().map(|n| format!("{n} events dropped"));

    view! {
        <div class="sd-tail">
            <div class="tl-bar">
                <span class=move || if paused.get() || failure.get().is_some() { "pulse off" } else { "pulse" }><span></span></span>
                <span class="lbl">{move || if paused.get() { "Paused" } else if failure.get() == Some("Connecting…") { "Connecting…" } else if failure.get().is_some() { "Disconnected" } else { "Tailing" }}</span>
                <span class="sp"></span>
                {move || lagged_label().map(|l| view! {
                    <span class="lbl" style="color:var(--yellow)">{l}</span>
                })}
                <Btn
                    variant=Variant::Secondary
                    on_click=Callback::new(move |()| paused.update(|p| *p = !*p))
                >
                    {move || if paused.get() { "Resume" } else { "Pause" }}
                </Btn>
            </div>

            <Show when=move || !paused.get() && failure.get().is_some_and(|f| f != "Connecting…")>
                <p role="alert">"Live tail disconnected. Retry to reconnect."</p>
                <Btn variant=Variant::Secondary on_click=Callback::new(move |()| retry.update(|n| *n = n.wrapping_add(1)))>"Retry live stream"</Btn>
            </Show>

            <div class="tl-stream" role="region" aria-label="Live tail messages" tabindex="0">
                {move || {
                    let rb = ring.read();
                    let total = rb.events.len();
                    if total == 0 {
                        return view! { <div class="sc-more">{move || if paused.get() { "Tail paused." } else if failure.get().is_some() { "No events received." } else { "Waiting for events…" }}</div> }.into_any();
                    }
                    let skip = total.saturating_sub(TAIL_DISPLAY_MAX);
                    rb.events.iter().skip(skip).map(|ev| {
                        let ts = ev.get("_time")
                            .or_else(|| ev.get("time"))
                            .or_else(|| ev.get("timestamp"))
                            .and_then(|v| v.as_str().map(ToString::to_string))
                            .map_or_else(|| "—".to_string(), |s| short_time(&s));
                        let msg = ev.get("message")
                            .or_else(|| ev.get("_raw"))
                            .or_else(|| ev.get("msg"))
                            .and_then(|v| v.as_str().map(ToString::to_string))
                            .unwrap_or_else(|| serde_json::to_string(ev).unwrap_or_default());
                        view! {
                            <div class="tl-row">
                                <span class="ts">{ts}</span>
                                <span class="msg">{msg}</span>
                            </div>
                        }
                    }).collect::<Vec<_>>().into_any()
                }}
            </div>
        </div>
    }
}

// ───────────────────────── Helpers ─────────────────────────

fn declined_query_response() -> trawl_api::QueryResponse {
    trawl_api::QueryResponse {
        execution: None,
        result: trawl_api::value::QueryResult::empty(),
        pagination: trawl_api::PaginationMeta {
            limit: 0,
            offset: 0,
            returned: 0,
            total: 0,
        },
        degraded_fields: Vec::new(),
        severity_columns: Vec::new(),
    }
}

fn cardinality_resource(
    svc: ServiceSchema,
) -> LocalResource<Result<HashMap<String, u64>, api::ApiError>> {
    let svc_name = svc.name;
    let fields: Vec<String> = svc.columns.into_iter().map(|c| c.name).collect();
    LocalResource::new(move || {
        let svc_name = svc_name.clone();
        let fields = fields.clone();
        async move {
            let Some(q) = crate::drawer_query::cardinality_query(&svc_name, &fields) else {
                return Ok(HashMap::new());
            };
            let resp = api::query(&q.dsl, FetchPlan::for_query(&q.dsl, 0)).await?;
            // Read back against the fields the BUILDER emitted, not the
            // service's whole column list: a name it could not render is
            // absent from the response and from `q.fields` alike
            // (`drawer_query::cardinality_query`).
            Ok(decode_cardinality(&resp, &q.fields))
        }
    })
}

fn parse_top_values(resp: &QueryResponse, field: &str, count_column: &str) -> Vec<(String, u64)> {
    let cols = &resp.result.columns;
    let fi = cols
        .iter()
        .position(|c| c.name == field || c.name == "value");
    // `count_column` is whatever the query that produced this response
    // counted into, which is not always `count`
    // (`drawer_query::top_values_query`).
    let ci = cols
        .iter()
        .position(|c| c.name == count_column || c.name.starts_with(count_column));
    let (Some(fi), Some(ci)) = (fi, ci) else {
        return Vec::new();
    };
    resp.result
        .rows
        .iter()
        .filter_map(|row| {
            let v = row.get(fi)?;
            let c = row.get(ci)?;
            let label = value_as_display(v);
            let count = value_as_u64(c)?;
            Some((label, count))
        })
        .collect()
}

fn value_as_display(v: &Value) -> String {
    match v {
        Value::Null => "(null)".to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::Integer(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) => format!("{f}"),
        Value::String(s) => s.clone(),
        Value::Array(_) => "[array]".to_string(),
    }
}

fn non_null_ratio(c: &ServiceColumnStats) -> f64 {
    if c.total_count == 0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    {
        (c.total_count - c.null_count) as f64 / c.total_count as f64
    }
}

fn sample_range(c: &ServiceColumnStats) -> String {
    match (c.min_value.as_deref(), c.max_value.as_deref()) {
        (Some(a), Some(b)) if a == b => a.to_string(),
        (Some(a), Some(b)) => format!("{a} … {b}"),
        _ => String::new(),
    }
}

fn short_time(iso: &str) -> String {
    // Accept `YYYY-MM-DDTHH:MM:SS...` and chop to HH:MM:SS. Falls back
    // to whatever was passed in so tails don't turn into a wall of "—".
    if let Some(t) = iso.split('T').nth(1) {
        let t = t.split('.').next().unwrap_or(t);
        let t = t.split(['+', 'Z', '-']).next().unwrap_or(t);
        return t.to_string();
    }
    iso.to_string()
}

fn top_cardinality_rows(
    columns: &[ServiceColumnStats],
    card_map: &HashMap<String, u64>,
    n: usize,
) -> Vec<(String, String, u64)> {
    let mut rows: Vec<(String, String, u64)> = columns
        .iter()
        .filter_map(|c| {
            let cnt = *card_map.get(&c.name)?;
            if cnt == 0 {
                return None;
            }
            Some((c.name.clone(), c.data_type.clone(), cnt))
        })
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.2));
    rows.truncate(n);
    rows
}

// ───────────────────────── Histogram chart ─────────────────────────

#[component]
fn HistogramChart(resource: LocalResource<Result<QueryResponse, api::ApiError>>) -> impl IntoView {
    view! {
        <Loaded
            state=Signal::derive(move || LoadState::from_resource(resource.get()))
            label="histogram"
            retry=Callback::new(move |()| { resource.set(None); resource.refetch(); })
            render=Box::new(move |resp: QueryResponse| {
                let slots = build_histogram(&resp);
                if slots.is_empty() {
                    return view! {
                        <div class="sc-more">"No events in the last 24h"</div>
                    }.into_any();
                }
                view! { <IngestChart slots=slots/> }.into_any()
            })
        />
    }
}

/// uPlot column chart over the filled hourly grid — real time axis,
/// hover readout, and zero-anchored y scale.
#[component]
fn IngestChart(slots: Vec<Slot>) -> impl IntoView {
    let node_ref = NodeRef::<leptos::html::Div>::new();
    let total_events: u128 = slots.iter().map(|slot| u128::from(slot.count)).sum();
    let chart_label =
        format!("Hourly ingest chart. Total events in the displayed hours: {total_events}.");
    // `ChartHandle` wraps a JS object — not Send/Sync, so it needs the
    // single-threaded storage, same as the search-page chart.
    let handle: StoredValue<Option<ChartHandle>, LocalStorage> = StoredValue::new_local(None);

    Effect::new(move |_| {
        let Some(element) = node_ref.get() else {
            return;
        };
        let html_el: web_sys::HtmlElement = (*element).clone().unchecked_into();
        let data = slots_to_aligned(&slots);

        handle.update_value(|stored| {
            if let Some(h) = stored.as_ref() {
                h.set_data(data);
            } else {
                // The drawer lays out before this effect runs, so the
                // measured width is real; the fallback only guards a
                // display:none ancestor.
                let measured = f64::from(html_el.client_width());
                let labels = ["events".to_string()];
                let opts = Opts {
                    width: if measured > 0.0 {
                        measured
                    } else {
                        INGEST_CHART_W
                    },
                    height: INGEST_CHART_H,
                    series: &labels,
                    y_label: None,
                    kind: ChartKind::Column,
                    // x values were built from trawld's already-shifted
                    // display timestamps — see `align_buckets`.
                    utc: true,
                    x_labels: None,
                    span_gaps: false,
                    // The drawer never resizes the chart from Rust, so
                    // the bridge follows the host width itself.
                    observe_resize: true,
                };
                *stored = Some(create_chart(&html_el, data, opts.to_js()));
            }
        });
    });

    on_cleanup(move || {
        handle.update_value(|stored| {
            if let Some(h) = stored.take() {
                h.destroy();
            }
        });
    });

    view! { <div class="ig-chart" node_ref=node_ref role="img" aria-label=chart_label></div> }
}

/// uPlot `AlignedData`: `[xs, ys]`, xs in seconds-since-epoch.
fn slots_to_aligned(slots: &[Slot]) -> JsValue {
    let xs = js_sys::Array::new();
    let ys = js_sys::Array::new();
    for slot in slots {
        // Bucket starts and event counts are both far below 2^53.
        #[allow(clippy::cast_precision_loss)]
        {
            xs.push(&JsValue::from_f64(slot.start_ms as f64 / 1000.0));
            ys.push(&JsValue::from_f64(slot.count as f64));
        }
    }
    let aligned = js_sys::Array::new();
    aligned.push(&xs);
    aligned.push(&ys);
    aligned.into()
}

/// Pull `(bucket_start_ms, count)` out of a `timechart span=1h` response
/// and lay it on a full 24-slot hourly grid.
///
/// The server only emits rows for buckets that carry events, so filling
/// the grid is what keeps each bar at its real position on the time axis.
fn build_histogram(resp: &QueryResponse) -> Vec<Slot> {
    let cols = &resp.result.columns;
    let ti = cols.iter().position(|c| {
        matches!(
            c.name.as_str(),
            "_time" | "time" | "timestamp" | "@timestamp"
        )
    });
    let ci = cols
        .iter()
        .position(|c| c.name == "count" || c.name.starts_with("count"));
    let (Some(ti), Some(ci)) = (ti, ci) else {
        return Vec::new();
    };
    let rows: Vec<(i64, u64)> = resp
        .result
        .rows
        .iter()
        .filter_map(|row| {
            let ts = parse_bucket_ms(&row.get(ti).map(value_as_display)?)?;
            let count = row.get(ci).and_then(value_as_u64)?;
            Some((ts, count))
        })
        .collect();
    align_buckets(&rows, INGEST_SLOT_MS, INGEST_SLOTS)
}

// ───────────────────────── Field-type donut ─────────────────────────

#[derive(Clone)]
struct DonutSegment {
    label: String,
    color: &'static str,
    count: usize,
    pill_class: &'static str,
}

fn donut_breakdown(columns: &[ServiceColumnStats]) -> Vec<DonutSegment> {
    let mut bins: HashMap<&'static str, (usize, &'static str, &'static str)> = HashMap::new();
    for c in columns {
        let key = super::service_card_fmt::type_bucket(&c.data_type);
        bins.entry(key.0)
            .and_modify(|e| e.0 += 1)
            .or_insert((1, key.1, key.2));
    }
    let mut segs: Vec<DonutSegment> = bins
        .into_iter()
        .map(|(label, (count, color, pill_class))| DonutSegment {
            label: label.to_string(),
            color,
            count,
            pill_class,
        })
        .collect();
    segs.sort_by_key(|s| std::cmp::Reverse(s.count));
    segs
}

#[component]
fn FieldTypeDonut(segments: Vec<DonutSegment>, total: usize) -> impl IntoView {
    if total == 0 {
        return view! { <div class="ft-donut">"No fields"</div> }.into_any();
    }
    let r = 28.0_f64;
    let circ = 2.0 * std::f64::consts::PI * r;
    let mut acc = 0.0_f64;
    let segments_for_svg = segments.clone();
    let paths = segments_for_svg
        .into_iter()
        .map(|seg| {
            #[allow(clippy::cast_precision_loss)]
            let frac = seg.count as f64 / total as f64;
            let dash = format!("{} {}", frac * circ, circ);
            let offset = -acc * circ;
            acc += frac;
            view! {
                <circle cx="36" cy="36" r="28" fill="none"
                    stroke=seg.color stroke-width="10"
                    stroke-dasharray=dash
                    stroke-dashoffset=format!("{offset}")
                    transform="rotate(-90 36 36)"/>
            }
        })
        .collect::<Vec<_>>();

    view! {
        <div class="ft-donut">
            <svg viewBox="0 0 72 72" width="72" height="72">
                <circle cx="36" cy="36" r="28" fill="none" stroke="var(--panel-3)" stroke-width="10"/>
                {paths}
            </svg>
            <div class="ft-legend">
                {segments.into_iter().map(|seg| {
                    let sw_style = format!("background:{}", seg.color);
                    view! {
                        <div class="lg">
                            <span class="sw" style=sw_style></span>
                            <span class=format!("tp {}", seg.pill_class)>{seg.label}</span>
                            <span class="ct">{seg.count}</span>
                        </div>
                    }
                }).collect::<Vec<_>>()}
            </div>
        </div>
    }.into_any()
}
