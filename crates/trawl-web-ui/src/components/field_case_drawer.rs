// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `<FieldCaseDrawer/>` — the field case file (ADR-0011 slice C2).
//!
//! A PEER of [`ServiceDrawer`](super::service_drawer::ServiceDrawer),
//! never a child: `fleet_ui::Drawer` does not nest (its Escape
//! arbitration assumes one drawer layer), so the Schema page swaps
//! between the two and this one mounts from `?field=` alone — which is
//! what makes `/search/schema?field=<name>` work as a deep link with no
//! service context and no services snapshot loaded.
//!
//! Everything rendered here is server-decided. The pin, the per-service
//! observations, the conflict evidence and the analyzer's verdict all
//! come from one `GET /api/v1/schema/field?name=` response; nothing is
//! re-derived client-side, and a field with no verdict gets no repin
//! affordance and no command hint at all.
//!
//! Field names, service names and conflict SAMPLES are client-chosen
//! text. They are rendered in leptos TEXT positions only — never
//! `inner_html`, never string-built markup — and each display copy goes
//! through `trawl_core::sanitize::sanitize_display_text` first, so a
//! bidi override in a sample cannot reorder the line beneath it.

use leptos::prelude::*;
use leptos::task::spawn_local;
use trawl_api::{CatalogConflictRow, CatalogFieldResponse, DegradedVerdict};
use trawl_core::sanitize::sanitize_display_text;

use crate::api;
use crate::api::ApiError;
use crate::repin_hint::{REPIN_HINT_REFUSED, repin_command_hint};
use fleet_ui::{Badge, Btn, Drawer, Icon, IconView, LoadMore, LoadState, Loaded, Tone, Variant};

/// Service observations per page. The service axis is client-chosen and
/// never pruned, so the detail route pages it; this is the server's own
/// default, spelled out because the cursor round-trip depends on it.
const SERVICES_PAGE: usize = 100;

/// The field case file. `back` carries the service the drawer was
/// reached through (the `?svc=` return context) — absent on a bare deep
/// link, in which case there is no back affordance and Escape closes.
#[component]
#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
pub fn FieldCaseDrawer(
    field: String,
    back: Option<String>,
    on_back: Callback<()>,
    on_close: Callback<()>,
) -> impl IntoView {
    // Page one. `reload` re-runs the fetcher, which is how the inline
    // retry recovers from a 503 or a dropped connection.
    let reload = RwSignal::new(0_u32);
    let field_for_page_one = field.clone();
    let detail = LocalResource::new(move || {
        let field = field_for_page_one.clone();
        let _ = reload.get();
        async move { api::catalog_field(&field, None, SERVICES_PAGE).await }
    });

    // Paged "services carrying this field". Seeded from page one, so a
    // retry or a fresh field starts the list over rather than appending
    // to a previous field's rows.
    let services = RwSignal::new(Vec::<trawl_api::CatalogFieldServiceRow>::new());
    let cursor = RwSignal::new(None::<String>);
    let more_busy = RwSignal::new(false);
    let more_error = RwSignal::new(None::<String>);

    Effect::new(move |_| {
        if let Some(Ok(resp)) = detail.get() {
            services.set(resp.services.clone());
            cursor.set(resp.services_cursor.clone());
            more_error.set(None);
        }
    });

    let field_for_more = field.clone();
    let on_load_more = Callback::new(move |()| {
        let Some(after) = cursor.get_untracked() else {
            return;
        };
        if more_busy.get_untracked() {
            return;
        }
        more_busy.set(true);
        more_error.set(None);
        let field = field_for_more.clone();
        spawn_local(async move {
            match api::catalog_field(&field, Some(&after), SERVICES_PAGE).await {
                Ok(resp) => {
                    let next = resp.services_cursor;
                    services.update(|rows| {
                        for row in resp.services {
                            // Exact-name dedupe: a page boundary can
                            // repeat a row, and page one's facts win.
                            if !rows.iter().any(|r| r.service == row.service) {
                                rows.push(row);
                            }
                        }
                    });
                    cursor.set(next);
                }
                // The rows already on screen stay; the failure is
                // reported inline and the button is the retry.
                Err(e) => more_error.set(Some(e.to_string())),
            }
            more_busy.set(false);
        });
    });

    let shown_field = sanitize_display_text(&field);
    let title_field = shown_field.clone();
    let missing_field = shown_field.clone();
    let back_for_title = back.clone();
    let back_for_copy = back.clone();
    // Escape is back-or-close: from a service drawer it returns there,
    // from a deep link it closes the page's only drawer.
    let has_back = back.is_some();
    let escape = Callback::new(move |()| {
        if has_back {
            on_back.run(());
        } else {
            on_close.run(());
        }
    });

    view! {
        <Drawer
            // No tabs: the case file is one surface. The strip renders
            // as the meta bar below the header.
            tabs=vec![]
            active_tab=Signal::derive(String::new)
            on_tab_change=Callback::new(|_: String| {})
            on_close=on_close
            on_escape=escape
            close_size=12
            meta="Field case file".to_string()
            title=Box::new(move || view! {
                {back_for_title.map(|svc| {
                    let label = format!("Back to {}", sanitize_display_text(&svc));
                    view! {
                        <button
                            type="button"
                            class="fc-back"
                            title=label.clone()
                            aria-label=label
                            on:click=move |_| on_back.run(())
                        >
                            <IconView icon=Icon::Chevron size=14 stroke_width=1.5/>
                        </button>
                    }
                })}
                <span class="name mono">{title_field.clone()}</span>
            }.into_any())
        >
            <Loaded
                state=Signal::derive(move || {
                    LoadState::from_resource_with_missing(
                        detail.get(),
                        |e| matches!(e, ApiError::Status(404)),
                        |e| match e {
                            ApiError::Status(503) => {
                                "the catalog store is unavailable".to_string()
                            }
                            other => other.to_string(),
                        },
                    )
                })
                label="field"
                // A name with no pin is not a failure and never a blank
                // drawer: it is a case file that says so.
                missing=Box::new(move || view! {
                    <div class="fc-case">
                        <div class="fc-note">
                            <p>
                                "No pin for "
                                <span class="mono">{missing_field.clone()}</span>
                                "."
                            </p>
                            <p>
                                "The catalog types a field the first time a batch carries a \
                                 value for it, so an unpinned name has never been ingested — \
                                 or is spelled differently. Names are ASCII-lowercased at \
                                 ingest, and its values, if any, stay findable in "
                                <span class="mono">"_raw"</span>
                                "."
                            </p>
                        </div>
                    </div>
                }.into_any())
                error=Box::new(move |msg: String| view! {
                    <div class="fc-case">
                        <div class="fc-err" role="alert">{format!("Couldn't load field: {msg}")}</div>
                        <div class="sfd-actions">
                            <Btn
                                variant=Variant::Secondary
                                on_click=Callback::new(move |()| reload.update(|n| *n += 1))
                            >
                                "Retry"
                            </Btn>
                        </div>
                    </div>
                }.into_any())
                render=Box::new(move |resp: CatalogFieldResponse| {
                    let pinned_from = resp.pinned_from.clone()
                        .map_or_else(|| "\u{2014}".to_string(), |s| sanitize_display_text(&s));
                    let data_type = resp.data_type.clone();
                    let pinned_at = resp.pinned_at.clone();
                    let verdict = resp.verdict.clone();
                    let conflicts = resp.conflicts.clone();
                    let hint = verdict.as_ref().map(|v| {
                        repin_command_hint(&resp.name, &v.suggested_to)
                    });
                    let scope_copy = back_for_copy.clone().map_or_else(
                        || "A repin rewrites this field across the entire corpus \u{2014} every \
                            service, every environment, every day \u{2014} not just the service \
                            you reached it from.".to_string(),
                        |svc| format!(
                            "A repin rewrites this field across the entire corpus \u{2014} every \
                             service, every environment, every day \u{2014} not just {}.",
                            sanitize_display_text(&svc),
                        ),
                    );

                    view! {
                        <div class="fc-case">
                            <div class="fc-sec">
                                <div class="fc-lb">"Pin"</div>
                                <div class="sfd-kv"><span>"Type"</span><span>{data_type}</span></div>
                                <div class="sfd-kv"><span>"Pinned by"</span><span>{pinned_from}</span></div>
                                <div class="sfd-kv"><span>"Pinned at"</span><span>{pinned_at}</span></div>
                            </div>

                            {verdict.map_or_else(
                                || view! {
                                    <div class="fc-sec">
                                        <div class="fc-lb">"Health"</div>
                                        <div class="fc-note">
                                            "This pin is not degraded. The catalog sees no \
                                             sustained pattern of values it has to shelve, so \
                                             there is nothing to repin."
                                        </div>
                                    </div>
                                }.into_any(),
                                |v| verdict_block(&v),
                            )}

                            {(!conflicts.is_empty()).then(|| conflicts_block(&conflicts))}

                            <div class="fc-sec">
                                <div class="fc-lb">"Services carrying this field"</div>
                                {move || services.get().into_iter().map(|row| {
                                    let service = sanitize_display_text(&row.service);
                                    let rows_label = super::service_card_fmt::format_count(row.row_count);
                                    view! {
                                        <div class="fc-svc">
                                            <span class="mono nm">{service}</span>
                                            <span class="dt">{row.first_seen}" \u{2192} "{row.last_seen}</span>
                                            <span class="ct">{rows_label}" rows"</span>
                                        </div>
                                    }
                                }).collect::<Vec<_>>()}
                                {move || more_error.get().map(|msg| view! {
                                    <div class="fc-err" role="alert">
                                        {format!("Couldn't load more services: {msg}")}
                                    </div>
                                })}
                                <LoadMore
                                    has_more=Signal::derive(move || cursor.get().is_some())
                                    busy=more_busy
                                    empty=Signal::derive(move || services.get().is_empty())
                                    empty_text="No service has been observed carrying this field"
                                    end_text="All observed services shown"
                                    on_load=on_load_more
                                />
                            </div>

                            <div class="fc-foot">
                                <p class="fc-note">{scope_copy}</p>
                                {hint.map(|hint| match hint {
                                    Some(cmd) => view! {
                                        <>
                                            <div class="fc-lb">"Remedy"</div>
                                            <pre class="fc-cmd">{cmd}</pre>
                                        </>
                                    }.into_any(),
                                    None => view! {
                                        <p class="fc-note">{REPIN_HINT_REFUSED}</p>
                                    }.into_any(),
                                })}
                            </div>
                        </div>
                    }.into_any()
                })
            />
        </Drawer>
    }
}

/// The analyzer's verdict, as facts. The words are written here rather
/// than stored server-side (ADR-0011 slice C ruling 5), and `rows
/// shelved` is labelled LIFETIME because it deliberately disagrees with
/// the per-conflict `rows nulled` below it, which sums only the evidence
/// still inside the per-field recency window.
fn verdict_block(v: &DegradedVerdict) -> AnyView {
    let samples: Vec<String> = v.samples.iter().map(|s| sanitize_display_text(s)).collect();
    let since = v.since.clone();
    let services = v.services;
    let episodes = v.episodes;
    let rows_shelved = v.rows_shelved;
    let suggested = v.suggested_to.clone();
    view! {
        <div class="fc-sec">
            <div class="fc-lb">
                "Health "
                <Badge tone=Tone::Warn>"degraded"</Badge>
            </div>
            <div class="sfd-kv"><span>"Shelving since"</span><span>{since}</span></div>
            <div class="sfd-kv">
                <span>"Services with conflict evidence"</span><span>{services}</span>
            </div>
            <div class="sfd-kv"><span>"Episodes"</span><span>{episodes}</span></div>
            <div class="sfd-kv">
                <span>"Rows shelved (lifetime)"</span><span>{rows_shelved}</span>
            </div>
            <div class="sfd-kv"><span>"Suggested type"</span><span>{suggested}</span></div>
            {(!samples.is_empty()).then(|| view! {
                <div class="fc-samples">
                    <div class="fc-lb2">"Values the pin shelved"</div>
                    <ul>
                        {samples.into_iter()
                            .map(|s| view! { <li class="mono">{s}</li> })
                            .collect::<Vec<_>>()}
                    </ul>
                </div>
            })}
        </div>
    }
    .into_any()
}

/// Retained conflict evidence, newest first. Per-row samples sit inside
/// a collapsed `<details>` — the verdict's sample list leads, and this
/// is the long tail behind it.
fn conflicts_block(conflicts: &[CatalogConflictRow]) -> AnyView {
    let rows = conflicts
        .iter()
        .map(|c| {
            let service = sanitize_display_text(&c.service);
            let cast = format!("{} \u{2192} {}", c.observed_type, c.expected_type);
            let rows_nulled = c.rows_nulled;
            let at = c.at.clone();
            let samples: Vec<String> = c.samples.iter().map(|s| sanitize_display_text(s)).collect();
            view! {
                <div class="fc-conflict">
                    <div class="hd">
                        <span class="mono nm">{service}</span>
                        <span class="cast mono">{cast}</span>
                        <span class="ct">{rows_nulled}" rows nulled"</span>
                        <span class="dt">{at}</span>
                    </div>
                    {(!samples.is_empty()).then(|| view! {
                        <details>
                            <summary>"sample values"</summary>
                            <ul>
                                {samples.into_iter()
                                    .map(|s| view! { <li class="mono">{s}</li> })
                                    .collect::<Vec<_>>()}
                            </ul>
                        </details>
                    })}
                </div>
            }
        })
        .collect::<Vec<_>>();
    view! {
        <div class="fc-sec">
            <div class="fc-lb">"Recent conflicts"</div>
            {rows}
        </div>
    }
    .into_any()
}
