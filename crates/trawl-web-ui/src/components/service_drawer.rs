// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<ServiceDrawer/>` — slide-out right-side inspector for a single
//! service on the Schema page. Scrim click + `Esc` to close.
//!
//! Three tabs, URL-synced via `?stab=overview|fields|tail`:
//! - Overview: 24h ingest histogram + field-type donut + top-fields by
//!   cardinality. Cardinality comes from a `stats dc(f1), dc(f2), …`
//!   DSL query fired once per drawer open and cached for the lifetime.
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

use crate::api;
use crate::state::stream_session::{LiveSignals, RingBuffer, StreamLifecycle, start_stream};
use fleet_ui::{
    Btn, Drawer, Icon, IconView, TabItem, ToastBus, ToastKind, Variant, effective_active,
};

/// Display cap for the live-tail viewport — keeps the DOM snappy. The
/// ring underneath still holds up to `LIVE_RING_CAPACITY` events.
const TAIL_DISPLAY_MAX: usize = 150;

/// Fields shown in the Overview "Top fields by cardinality" card.
const TOP_CARDINALITY_ROWS: usize = 6;

#[component]
#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
pub fn ServiceDrawer(
    svc: ServiceSchema,
    tab: Signal<String>,
    on_close: Callback<()>,
    on_tab_change: Callback<String>,
    on_search: Callback<String>,
    on_use_field: Callback<String>,
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
    let meta_text = format!(
        "{} events · {} · {} fields",
        super::service_card_fmt::format_count(svc.total_events),
        super::service_card_fmt::format_bytes(svc.total_bytes),
        svc.columns.len()
    );

    let on_search_click = Callback::new(move |()| on_search.run(name_for_search.clone()));
    let on_tail_click = Callback::new(move |()| on_tab_change.run("tail".to_string()));

    // Drawer compares ids verbatim; map the URL signal's empty default.
    let eff_tab = effective_active(tab, "overview");

    view! {
        <Drawer
            tabs=vec![
                TabItem::new("overview", "Overview"),
                TabItem::new("fields", "Fields"),
                TabItem::new("tail", "Live tail"),
            ]
            active_tab=eff_tab
            on_tab_change=on_tab_change
            on_close=on_close
            meta=meta_text
            title=Box::new(move || view! {
                <StatusDot svc=svc_for_head.clone()/>
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
                    <IconView icon=Icon::Zap size=11 stroke_width=1.5/> " Tail live"
                </Btn>
            }.into_any())
        >
            {move || {
                if eff_tab.get() == "fields" {
                    view! {
                        <FieldsPane
                            svc=svc_for_fields.clone()
                            cardinality=cardinality_sig
                            on_use_field=on_use_field
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
    on_use_field: Callback<String>,
) -> impl IntoView {
    let svc_name = svc.name.clone();
    let histogram = LocalResource::new(move || {
        let q = format!(
            r#"service="{}" last=24h | timechart span=1h count()"#,
            svc_name.replace('"', "")
        );
        async move { api::query(&q, 0).await }
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
                {move || match cardinality.get() {
                    None => view! {
                        <div class="sc-more">"computing…"</div>
                    }.into_any(),
                    Some(Err(msg)) => view! {
                        <div class="sc-more" style="color:var(--red)">
                            {format!("couldn't compute cardinality: {msg}")}
                        </div>
                    }.into_any(),
                    Some(Ok(card_map)) => {
                        let rows = top_cardinality_rows(&columns_for_top, &card_map, TOP_CARDINALITY_ROWS);
                        if rows.is_empty() {
                            return view! {
                                <div class="sc-more">"no distinct values observed yet"</div>
                            }.into_any();
                        }
                        let max = rows.iter().map(|r| r.2).max().unwrap_or(1).max(1);
                        rows.into_iter().map(|(fname, type_label, count)| {
                            let (pill_class, pill_text) = super::service_card_fmt::type_pill(&type_label);
                            #[allow(clippy::cast_precision_loss)]
                            let pct = (count as f64 / max as f64) * 100.0;
                            let bar_style = format!("width:{pct:.1}%");
                            let label = super::service_card_fmt::format_count(count);
                            let fname_cb = fname.clone();
                            let on_use_click = move |_| on_use_field.run(fname_cb.clone());
                            view! {
                                <div class="tf topfields" on:click=on_use_click>
                                    <span class="fn">{fname}</span>
                                    <span class=format!("tp {pill_class}")>{pill_text}</span>
                                    <span class="bar-wrap"><span class="bar" style=bar_style></span></span>
                                    <span class="c">{label}</span>
                                </div>
                            }
                        }).collect::<Vec<_>>().into_any()
                    }
                }}
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

#[component]
#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
fn FieldsPane(
    svc: ServiceSchema,
    cardinality: Signal<Option<Result<HashMap<String, u64>, String>>>,
    on_use_field: Callback<String>,
) -> impl IntoView {
    let sort = RwSignal::new((SortKey::Cardinality, false)); // desc
    let expanded = RwSignal::new(None::<String>);
    let svc_name = svc.name.clone();
    let columns_owned = svc.columns.clone();

    // Header-click handler: toggle direction if same key, else select
    // with desc as default (matches sort-by-numeric expectation).
    let toggle_sort = move |key: SortKey| {
        move |_| {
            let (cur_key, cur_asc) = sort.get_untracked();
            let next = if cur_key == key {
                (key, !cur_asc)
            } else {
                // default direction per column
                let default_asc = matches!(key, SortKey::Name);
                (key, default_asc)
            };
            sort.set(next);
        }
    };

    let on_header_name = toggle_sort(SortKey::Name);
    let on_header_card = toggle_sort(SortKey::Cardinality);
    let on_header_cov = toggle_sort(SortKey::NonNull);
    let on_header_size = toggle_sort(SortKey::Storage);

    view! {
        <div class="sd-fields">
            <div class="sf-hd">
                <div on:click=on_header_name>"Field" {move || sort_caret(sort.get(), SortKey::Name)}</div>
                <div>"Type"</div>
                <div on:click=on_header_cov>"Non-null" {move || sort_caret(sort.get(), SortKey::NonNull)}</div>
                <div on:click=on_header_card>"Cardinality" {move || sort_caret(sort.get(), SortKey::Cardinality)}</div>
                <div on:click=on_header_size>"Storage" {move || sort_caret(sort.get(), SortKey::Storage)}</div>
                <div>"Sample"</div>
                <div></div>
            </div>

            {move || {
                let (key, asc) = sort.get();
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
                    if asc { ord } else { ord.reverse() }
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
                    view! {
                        <>
                            <div class=row_class on:click=toggle>
                                <div>
                                    <span class="caret">{move || if is_open_caret() { "▾" } else { "▸" }}</span>
                                    <span class="c-name">{c.name.clone()}</span>
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
    let svc_for_q = svc.clone();
    let field_for_q = field_name.clone();
    let top = LocalResource::new(move || {
        let q = format!(
            r#"service="{}" last=7d | top 10 {}"#,
            svc_for_q.replace('"', ""),
            field_for_q
        );
        async move { api::query(&q, 0).await }
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
                {move || match top.get() {
                    None => view! {
                        <div class="sc-more">"sampling…"</div>
                    }.into_any(),
                    Some(Err(e)) => view! {
                        <div class="sc-more" style="color:var(--red)">
                            {format!("couldn't sample: {e}")}
                        </div>
                    }.into_any(),
                    Some(Ok(resp)) => {
                        let rows = parse_top_values(&resp, &field_name);
                        if rows.is_empty() {
                            return view! {
                                <div class="sc-more">"no values"</div>
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
                    }
                }}
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
        if is_paused {
            lifecycle.set_value(None);
            return;
        }
        let q = format!(r#"service="{}""#, svc_name_for_stream.replace('"', ""));
        let sig = LiveSignals {
            ring,
            snapshot,
            lagged,
        };
        match start_stream(&q, sig) {
            Some(lc) => lifecycle.set_value(Some(lc)),
            None => {
                bus.push(
                    ToastKind::Error,
                    "Tail unavailable",
                    Some(format!(
                        "Couldn't open a live stream for {svc_name_for_toast}."
                    )),
                );
            }
        }
    });

    on_cleanup(move || {
        lifecycle.set_value(None);
    });

    let lagged_label = move || lagged.get().map(|n| format!("{n} events dropped"));

    view! {
        <div class="sd-tail">
            <div class="tl-bar">
                <span class=move || if paused.get() { "pulse off" } else { "pulse" }><span></span></span>
                <span class="lbl">{move || if paused.get() { "paused" } else { "tailing" }}</span>
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

            <div class="tl-stream">
                {move || {
                    let rb = ring.read();
                    let total = rb.events.len();
                    if total == 0 {
                        return view! { <div class="sc-more">"waiting for events…"</div> }.into_any();
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

fn cardinality_resource(
    svc: ServiceSchema,
) -> LocalResource<Result<HashMap<String, u64>, api::ApiError>> {
    let svc_name = svc.name;
    let fields: Vec<String> = svc.columns.into_iter().map(|c| c.name).collect();
    LocalResource::new(move || {
        let svc_name = svc_name.clone();
        let fields = fields.clone();
        async move {
            if fields.is_empty() {
                return Ok(HashMap::new());
            }
            let dc_exprs: Vec<String> = fields.iter().map(|f| format!("dc({f}) as {f}")).collect();
            let q = format!(
                r#"service="{}" last=7d | stats {}"#,
                svc_name.replace('"', ""),
                dc_exprs.join(", ")
            );
            let resp = api::query(&q, 0).await?;
            Ok(parse_cardinality(&resp))
        }
    })
}

fn parse_cardinality(resp: &QueryResponse) -> HashMap<String, u64> {
    let mut out = HashMap::new();
    let Some(row) = resp.result.rows.first() else {
        return out;
    };
    for (col, val) in resp.result.columns.iter().zip(row.iter()) {
        if let Some(n) = value_as_u64(val) {
            out.insert(col.name.clone(), n);
        }
    }
    out
}

fn parse_top_values(resp: &QueryResponse, field: &str) -> Vec<(String, u64)> {
    let cols = &resp.result.columns;
    let fi = cols
        .iter()
        .position(|c| c.name == field || c.name == "value");
    let ci = cols
        .iter()
        .position(|c| c.name == "count" || c.name.starts_with("count"));
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

fn value_as_u64(v: &Value) -> Option<u64> {
    match v {
        #[allow(clippy::cast_sign_loss)]
        Value::Integer(i) if *i >= 0 => Some(*i as u64),
        Value::Float(f) if *f >= 0.0 =>
        {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            Some(*f as u64)
        }
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn value_as_display(v: &Value) -> String {
    match v {
        Value::Null => "(null)".to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::Integer(i) => i.to_string(),
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

fn sort_caret(state: (SortKey, bool), key: SortKey) -> &'static str {
    if state.0 != key {
        return "";
    }
    if state.1 { " ▲" } else { " ▼" }
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
        <div class="ig-chart">
            {move || match resource.get() {
                None => view! {
                    <div class="sc-more">"loading…"</div>
                }.into_any(),
                Some(Err(e)) => view! {
                    <div class="sc-more" style="color:var(--red)">
                        {format!("couldn't load histogram: {e}")}
                    </div>
                }.into_any(),
                Some(Ok(resp)) => {
                    let bars = build_histogram(&resp);
                    if bars.is_empty() {
                        return view! {
                            <div class="sc-more">"no events in the last 24h"</div>
                        }.into_any();
                    }
                    let max = bars.iter().map(|b| b.1).max().unwrap_or(1).max(1);
                    view! {
                        <>
                            <div class="ig-grid">
                                <div class="ln" style="top:0"></div>
                                <div class="ln" style="top:50%"></div>
                                <div class="ln" style="top:100%"></div>
                            </div>
                            <div class="ig-bars">
                                {bars.into_iter().map(|(_label, count)| {
                                    #[allow(clippy::cast_precision_loss)]
                                    let h = (count as f64 / max as f64) * 100.0;
                                    let style = format!("height:{h:.1}%");
                                    view! { <span class="bar" style=style title=count.to_string()></span> }
                                }).collect::<Vec<_>>()}
                            </div>
                            <div class="ig-axis">
                                <span>"-24h"</span>
                                <span>"-12h"</span>
                                <span>"now"</span>
                            </div>
                        </>
                    }.into_any()
                }
            }}
        </div>
    }
}

fn build_histogram(resp: &QueryResponse) -> Vec<(String, u64)> {
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
    resp.result
        .rows
        .iter()
        .filter_map(|row| {
            let label = row.get(ti).map(value_as_display)?;
            let count = row.get(ci).and_then(value_as_u64)?;
            Some((label, count))
        })
        .collect()
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
        return view! { <div class="ft-donut">"no fields"</div> }.into_any();
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

// ───────────────────────── Status dot ─────────────────────────

#[component]
#[allow(clippy::needless_pass_by_value)]
fn StatusDot(svc: ServiceSchema) -> impl IntoView {
    let (today, yesterday) = super::service_card_fmt::today_yesterday_utc();
    let class = if super::service_card_fmt::is_healthy(&svc, &today, &yesterday) {
        "sd-dot"
    } else {
        "sd-dot errors"
    };
    view! { <span class=class></span> }
}
