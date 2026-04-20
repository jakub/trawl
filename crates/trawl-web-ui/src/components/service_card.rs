// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<ServiceCard/>` — one clickable card per service on the Schema
//! page. Click the card → `on_open`. The hover-revealed Search / Tail
//! quick-actions emit `on_search` / `on_tail`. All three carry the
//! service name so the page can route without re-deriving it.
//!
//! Data comes straight from `/api/v1/schema/services` —
//! `ServiceSchema` covers events, storage, column stats, and
//! `daily_event_counts` for the sparkline.

use leptos::prelude::*;
use leptos::web_sys;
use trawl_api::ServiceSchema;

use super::service_card_fmt::{
    cov_pct, date_range, format_avg_coverage, format_bytes, format_count, is_healthy, type_pill,
};
use super::sparkline::Sparkline;

/// Number of field rows shown on a card before the "+N more" row.
const TOP_FIELDS: usize = 5;

#[component]
#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
pub fn ServiceCard(
    svc: ServiceSchema,
    compact: Signal<bool>,
    active: Signal<bool>,
    /// Today as `YYYY-MM-DD UTC` — so the whole grid agrees on what
    /// "recent ingest" means without each card hitting `Date` on its own.
    today: String,
    /// Yesterday as `YYYY-MM-DD UTC`.
    yesterday: String,
    on_open: Callback<String>,
    on_search: Callback<String>,
    on_tail: Callback<String>,
) -> impl IntoView {
    let name = svc.name.clone();
    let field_count = svc.columns.len();
    let events_label = format_count(svc.total_events);
    let storage_label = format_bytes(svc.total_bytes);
    let coverage_label = format_avg_coverage(&svc.columns);
    let sub = date_range(&svc);
    let healthy = is_healthy(&svc, &today, &yesterday);
    let spark_data: Vec<u64> = svc
        .daily_event_counts
        .iter()
        .rev()
        .take(30)
        .map(|d| d.count)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let spark_color = if healthy {
        "var(--amber)"
    } else {
        "var(--red)"
    };

    let top_fields: Vec<(String, &'static str, String, u32)> = svc
        .columns
        .iter()
        .take(TOP_FIELDS)
        .map(|c| {
            let (pill_class, pill_label) = type_pill(&c.data_type);
            let cov = cov_pct(c.null_count, c.total_count);
            (c.name.clone(), pill_class, pill_label, cov)
        })
        .collect();
    let more = field_count.saturating_sub(TOP_FIELDS);

    let card_class = Memo::new(move |_| {
        let mut cls = String::from("sc-card");
        if compact.get() {
            cls.push_str(" compact");
        }
        if active.get() {
            cls.push_str(" active");
        }
        cls
    });
    let dot_class = if healthy { "sd-dot" } else { "sd-dot errors" };

    let name_open = name.clone();
    let name_search = name.clone();
    let name_tail = name;

    view! {
        <div
            class=move || card_class.get()
            on:click=move |_| on_open.run(name_open.clone())
        >
            <div class="sc-hd">
                <div class="sc-title">
                    <span class=dot_class></span>
                    <span class="name">{svc.name.clone()}</span>
                    <span class="sp"></span>
                    <Sparkline data=spark_data color=spark_color w=96 h=20/>
                </div>
                {(!sub.is_empty()).then_some(view! { <div class="sc-sub">{sub}</div> })}
            </div>

            <div class="sc-stats">
                <div><span>"Events"</span><span class="v">{events_label}</span></div>
                <div><span>"Storage"</span><span class="v">{storage_label}</span></div>
                <div><span>"Fields"</span><span class="v">{field_count}</span></div>
                <div><span>"Avg cov"</span><span class="v">{coverage_label}</span></div>
            </div>

            <div class="sc-fields">
                <div class="sc-fields-hd">"Top fields"</div>
                {top_fields.into_iter().map(|(fname, pill_class, pill_label, cov)| {
                    let cov_style = format!("width:{cov}%");
                    view! {
                        <div class="sc-f">
                            <span class="fn">{fname}</span>
                            <span class=format!("tp {pill_class}")>{pill_label}</span>
                            <span class="cov-mini"><span style=cov_style></span></span>
                        </div>
                    }
                }).collect::<Vec<_>>()}
                {(more > 0).then_some(view! {
                    <div class="sc-more">{format!("+{more} more")}</div>
                })}
            </div>

            <div class="sc-quickact">
                <span class="hint">"click to inspect"</span>
                <span class="sp"></span>
                <span
                    class="qa"
                    title="Search this service"
                    on:click=move |e: web_sys::MouseEvent| {
                        e.stop_propagation();
                        on_search.run(name_search.clone());
                    }
                >
                    <SearchIcon/>
                    "Search"
                </span>
                <span
                    class="qa"
                    title="Live tail"
                    on:click=move |e: web_sys::MouseEvent| {
                        e.stop_propagation();
                        on_tail.run(name_tail.clone());
                    }
                >
                    <ZapIcon/>
                    "Tail"
                </span>
            </div>
        </div>
    }
}

#[component]
fn SearchIcon() -> impl IntoView {
    view! {
        <svg width="11" height="11" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5">
            <circle cx="7" cy="7" r="4.5"/>
            <path d="m10.5 10.5 3 3"/>
        </svg>
    }
}

#[component]
fn ZapIcon() -> impl IntoView {
    view! {
        <svg width="11" height="11" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round">
            <path d="M9 1 3 9h5l-1 6 6-8h-5z"/>
        </svg>
    }
}
