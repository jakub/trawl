// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Health, capacity and query management over the existing server reports.
use crate::state::stats_stream::SharedDashboard;
use crate::{
    api, perms,
    service_card_fmt::{format_bytes, format_count, format_exact, format_uptime},
};
use fleet_ui::{ConfirmModal, ConfirmState};
use leptos::{prelude::*, task::spawn_local};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use trawl_api::{HealthResponse, QueriesResponse, StatsResponse};

fn read_error(error: &api::ApiError) -> String {
    match error.http_status() {
        Some(401 | 403) => "You do not have permission to read this report.".into(),
        Some(status) => {
            format!("Report unavailable, server returned {status}. Refresh to try again.")
        }
        None => "Could not read this report. Refresh to try again.".into(),
    }
}

#[component]
#[allow(clippy::too_many_lines)] // Each report has independent loading and failure state.
pub fn HealthPage() -> impl IntoView {
    let me = expect_context::<RwSignal<Option<api::MeResponse>>>();
    let dashboard = expect_context::<SharedDashboard>();
    let health = RwSignal::new(None::<Result<HealthResponse, String>>);
    let capacity = RwSignal::new(None::<Result<StatsResponse, String>>);
    let refresh = RwSignal::new(0_u64);
    let generation = Arc::new(AtomicU64::new(0));
    let cleanup_generation = generation.clone();
    on_cleanup(move || {
        cleanup_generation.fetch_add(1, Ordering::SeqCst);
    });
    Effect::new(move |_| {
        let identity = me.get();
        refresh.get();
        let epoch = generation.fetch_add(1, Ordering::SeqCst) + 1;
        health.set(None);
        capacity.set(None);
        let Some(identity) = identity else {
            return;
        };
        let health_generation = generation.clone();
        spawn_local(async move {
            let result = api::health().await.map_err(|e| read_error(&e));
            if health_generation.load(Ordering::SeqCst) == epoch {
                health.set(Some(result));
            }
        });
        if perms::is_trawl_admin(&identity.permissions) {
            let stats_generation = generation.clone();
            spawn_local(async move {
                let result = api::stats().await.map_err(|e| read_error(&e));
                if stats_generation.load(Ordering::SeqCst) == epoch {
                    capacity.set(Some(result));
                }
            });
        }
    });
    view! {
        <div class="health-page">
            <header class="health-heading"><div><h1>"Health"</h1><p>"Server checks and operations"</p></div>
                <button class="btn health-refresh" on:click=move |_| refresh.update(|n| *n += 1)>"Refresh"</button>
            </header>
            <section class="health-section" aria-labelledby="health-checks-title">
                <h2 id="health-checks-title">"Health"</h2>
                {move || match health.get() {
                    None => view! { <p role="status">"Loading health report..."</p> }.into_any(),
                    Some(Err(error)) => view! { <p role="alert">{error}</p> }.into_any(),
                    Some(Ok(report)) => {
                        let mut checks = report.checks.unwrap_or_default().into_iter().collect::<Vec<_>>();
                        checks.sort_by(|a, b| a.0.cmp(&b.0));
                        view! {
                            <dl class="health-facts"><div><dt>"Overall state"</dt><dd>{match report.status { trawl_api::HealthStatus::Ok => "ok", trawl_api::HealthStatus::Degraded => "degraded", trawl_api::HealthStatus::Unavailable => "unavailable" }}</dd></div>
                                <div><dt>"Version"</dt><dd>{report.version.unwrap_or_else(|| "Not reported".into())}</dd></div>
                            </dl>
                            <dl class="health-checks">{checks.into_iter().map(|(name, result)| view! {
                                <div><dt>{name}</dt><dd class:health-check-error=result != "ok">{result.clone()}</dd></div>
                            }).collect_view()}</dl>
                        }.into_any()
                    }
                }}
            </section>
            <Show when=move || me.get().is_some_and(|m| perms::is_trawl_admin(&m.permissions))>
                <section class="health-capacity" aria-labelledby="health-capacity-title">
                    <h2 id="health-capacity-title">"Capacity"</h2><p class="health-note">"Snapshot at last refresh"</p>
                    {move || match capacity.get() {
                        None => view! { <p role="status">"Loading capacity..."</p> }.into_any(),
                        Some(Err(error)) => view! { <p role="alert">{error}</p> }.into_any(),
                        Some(Ok(s)) => view! { <dl class="health-facts">
                            <div><dt>"Uptime"</dt><dd>{format_uptime(s.uptime_secs)}</dd></div>
                            <div><dt>"Queries since startup"</dt><dd>{format_exact(s.total_queries)}</dd></div>
                            <div><dt>"Active queries"</dt><dd>{s.active_queries}</dd></div>
                            <div><dt>"Executors available"</dt><dd>{format!("{} / {}", s.pool_available, s.pool_capacity)}</dd></div>
                            <div><dt>"Retained work"</dt><dd>{s.pool_retained}</dd></div>
                        </dl><p class="health-note">"Retained work uses occupied executors; it is included in pool usage."</p> }.into_any(),
                    }}
                </section>
                <section class="health-live" aria-labelledby="health-live-title">
                    <h2 id="health-live-title">"Live operations"</h2>
                    <p class="health-live-state" role="status">{move || dashboard.get().phase.label()}</p>
                    {move || dashboard.get().snapshot.map(|s| view! {
                        <dl class="health-facts">
                            <div><dt>"Host"</dt><dd>{s.hostname}</dd></div>
                            <div><dt>"Ingest rate"</dt><dd>{format!("{:.1} events/s", s.ingest_rate)}</dd></div>
                            <div><dt>"Query rate"</dt><dd>{format!("{:.1} queries/s", s.query_rate)}</dd></div>
                            <div><dt>"Hot buffer events"</dt><dd>{format!("{} / {}", format_count(s.hot_buffer_events as u64), format_count(s.hot_buffer_max_events as u64))}</dd></div>
                            <div><dt>"Hot buffer memory"</dt><dd>{format!("{} / {}", format_bytes(s.hot_buffer_bytes as u64), format_bytes(s.hot_buffer_max_bytes as u64))}</dd></div>
                            <div><dt>"Executors occupied"</dt><dd>{format!("{} / {}", s.pool_active, s.pool_capacity)}</dd></div>
                        </dl>
                    })}
                </section>
            </Show>
            <Show when=move || me.get().is_some_and(|m| perms::can_query(&m.permissions))><HealthQueries/></Show>
        </div>
    }
}

#[derive(Clone)]
struct QueryRow {
    id: u64,
    own: bool,
    user: String,
    query: String,
    state: String,
    elapsed: String,
}

fn query_rows(response: QueriesResponse) -> Vec<QueryRow> {
    let active = response.active.into_iter().map(|entry| {
        let q = entry.snapshot;
        QueryRow {
            id: q.id,
            own: entry.own,
            user: q.user,
            query: q.query,
            state: "Active".into(),
            elapsed: format!("{} ms", format_exact(q.running_ms)),
        }
    });
    let recent = response.recent.into_iter().map(|entry| {
        let q = entry.snapshot;
        let state = if q.timed_out {
            "Timed out"
        } else if q.error.is_some() {
            "Failed"
        } else {
            "Completed"
        };
        QueryRow {
            id: q.id,
            own: entry.own,
            user: q.user,
            query: q.query,
            state: state.into(),
            elapsed: format!("{} ms", format_exact(q.duration_ms)),
        }
    });
    active.chain(recent).collect()
}

#[component]
#[allow(clippy::too_many_lines)] // The confirmation consumes the same row and generation the list rendered.
fn HealthQueries() -> impl IntoView {
    let me = expect_context::<RwSignal<Option<api::MeResponse>>>();
    let rows = RwSignal::new(None::<Result<Vec<QueryRow>, String>>);
    let refresh = RwSignal::new(0_u64);
    let outcome = RwSignal::new(None::<String>);
    let pending = RwSignal::new(false);
    let confirm = RwSignal::new(ConfirmState::<(u64, bool, u64)>::default());
    let generation = Arc::new(AtomicU64::new(0));
    let cleanup = generation.clone();
    on_cleanup(move || {
        cleanup.fetch_add(1, Ordering::SeqCst);
    });
    let read_generation = generation.clone();
    Effect::new(move |_| {
        let identity = me.get();
        refresh.get();
        let epoch = read_generation.fetch_add(1, Ordering::SeqCst) + 1;
        rows.set(None);
        confirm.update(ConfirmState::cancel);
        let Some(identity) = identity else {
            return;
        };
        if !perms::can_query(&identity.permissions) {
            return;
        }
        let guard = read_generation.clone();
        spawn_local(async move {
            let result = api::queries()
                .await
                .map(query_rows)
                .map_err(|e| read_error(&e));
            if guard.load(Ordering::SeqCst) == epoch {
                rows.set(Some(result));
            }
        });
    });
    Effect::new(move |_| {
        me.get();
        outcome.set(None);
        pending.set(false);
    });
    let confirm_generation = generation.clone();
    let on_confirm = Callback::new(move |()| {
        let Some((id, own, epoch)) = confirm.try_update(ConfirmState::take).flatten() else {
            return;
        };
        if pending.get_untracked() || confirm_generation.load(Ordering::SeqCst) != epoch {
            return;
        }
        if !me.get_untracked().is_some_and(|m| {
            perms::can_query(&m.permissions) && perms::can_cancel_query(&m.permissions, own)
        }) {
            return;
        }
        pending.set(true);
        outcome.set(None);
        let guard = confirm_generation.clone();
        spawn_local(async move {
            let result = api::cancel_query(id).await;
            if guard.load(Ordering::SeqCst) != epoch {
                return;
            }
            pending.set(false);
            outcome.set(Some(match result {
                Ok(answer) if answer.query_id != id => {
                    "Cancellation outcome unknown. Refresh queries before trying again.".into()
                }
                Ok(answer) if answer.cancelled => "Cancellation requested".into(),
                Ok(_) => "No work was cancelled".into(),
                // A proxy 5xx can follow a DELETE that reached the daemon.
                // It does not prove whether cancellation took effect.
                Err(error) if matches!(error.http_status(), None | Some(500..=599)) => {
                    "Cancellation outcome unknown. Refresh queries before trying again.".into()
                }
                Err(error) => format!(
                    "Cancellation failed, server returned {}.",
                    error.http_status().unwrap_or_default()
                ),
            }));
            refresh.update(|n| *n += 1);
        });
    });
    view! {
        <section class="health-queries" aria-labelledby="health-queries-title">
            <div class="health-heading"><h2 id="health-queries-title">"Queries"</h2>
                <button class="btn health-queries-refresh" disabled=move || pending.get() on:click=move |_| refresh.update(|n| *n += 1)>"Refresh queries"</button>
            </div>
            <p class="health-note">"Active and recent queries at last refresh"</p>
            {move || outcome.get().map(|text| view! { <p class="health-cancel-outcome" role="status">{text}</p> })}
            {move || match rows.get() {
                None => view! { <p role="status">"Loading queries..."</p> }.into_any(),
                Some(Err(error)) => view! { <p role="alert">{error}</p> }.into_any(),
                Some(Ok(rows)) if rows.is_empty() => view! { <p>"No active or recent queries."</p> }.into_any(),
                Some(Ok(rows)) => {
                    let epoch = generation.load(Ordering::SeqCst);
                    view! { <div class="health-query-scroll"><table class="health-query-table">
                        <thead><tr><th>"Query"</th><th>"User"</th><th>"State"</th><th>"Elapsed"</th><th>"Action"</th></tr></thead>
                        <tbody>{rows.into_iter().map(|row| {
                            let (id, own) = (row.id, row.own);
                            view! { <tr data-query-id=id data-own=own.to_string()>
                                <td><span class="health-query-id">{format!("#{id}")}</span><code>{row.query}</code></td><td>{row.user}</td><td>{row.state}</td><td>{row.elapsed}</td>
                                <td><Show when=move || me.get().is_some_and(|m| perms::can_cancel_query(&m.permissions, own))>
                                    <button class="btn health-query-cancel" disabled=move || pending.get() on:click=move |_| confirm.update(|s| s.request((id, own, epoch)))>"Cancel"</button>
                                </Show></td>
                            </tr> }
                        }).collect_view()}</tbody>
                    </table></div> }.into_any()
                }
            }}
            <Show when=move || confirm.get().is_open()>
                <ConfirmModal title="Cancel query" message="Request cancellation of this query? Work may already have finished.".to_string() confirm_label="Cancel query"
                    on_confirm=on_confirm on_cancel=Callback::new(move |()| confirm.update(ConfirmState::cancel))/>
            </Show>
        </section>
    }
}
