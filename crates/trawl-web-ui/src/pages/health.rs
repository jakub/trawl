// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Health, capacity and query management over the existing server reports.
use crate::dashboard_state::DashboardPhase;
use crate::state::stats_stream::SharedDashboard;
use crate::{
    api, perms,
    service_card_fmt::{format_bytes, format_count, format_exact, format_uptime},
};
use fleet_ui::{Badge, ConfirmModal, ConfirmState, Tone};
use leptos::{prelude::*, task::spawn_local};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use trawl_api::{
    HealthResponse, HealthStatus, QueriesResponse, StatsResponse, StorageMeasurement,
    StorageMeasurementStatus,
};

/// Human names for the checks the daemon ships. A key with no entry
/// renders verbatim: the page does not invent a name for a subsystem it
/// has not met.
const CHECK_NAMES: &[(&str, &str)] = &[
    ("auth_db", "Authentication database"),
    ("data_path", "Data path"),
    ("duckdb", "Query engine"),
    ("ingest_capacity", "Ingest capacity"),
    ("storage_db", "Catalog database"),
];

/// The friendly name for a check key, or `None` when there is none.
fn check_name(key: &str) -> Option<&'static str> {
    CHECK_NAMES
        .iter()
        .find(|(known, _)| *known == key)
        .map(|(_, name)| *name)
}

/// The badge for one check result: `ok` is healthy, `ingest_capacity`'s
/// `refusing` is a warning (ingest is pushed back while queries still
/// serve, ADR-0043), and any other value is a failure shown verbatim.
fn check_badge(key: &str, result: &str) -> (Tone, String) {
    match (key, result) {
        (_, "ok") => (Tone::Success, "Healthy".to_owned()),
        ("ingest_capacity", "refusing") => (Tone::Warn, "Refusing".to_owned()),
        _ => (Tone::Danger, result.to_owned()),
    }
}

/// The checks card's title.
///
/// A report that did not arrive is never called healthy: a transport
/// failure and a permission refusal both reach here as `Err`, and
/// claiming health on either would be a state the page cannot see
/// (ADR-0025).
fn health_title(report: Option<&Result<HealthResponse, String>>) -> &'static str {
    match report {
        None => "Health checks",
        Some(Err(_)) => "Health report unavailable",
        Some(Ok(report)) => match report.status {
            HealthStatus::Ok => "Server is healthy",
            HealthStatus::Degraded => "Server is degraded",
            HealthStatus::Unavailable => "Server is unavailable",
        },
    }
}

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
            <header class="health-heading"><div><h1>"Health"</h1></div>
                <button class="btn health-refresh" on:click=move |_| refresh.update(|n| *n += 1)>"Refresh"</button>
            </header>
            <div class="health-cards">
            <section class="health-section" aria-labelledby="health-checks-title">
                {move || {
                    let title = health_title(health.get().as_ref());
                    view! {
                        <h2 id="health-checks-title" class="health-card-ttl">
                            {title}
                        </h2>
                    }
                }}
                {move || match health.get() {
                    None => view! { <p role="status">"Loading health report..."</p> }.into_any(),
                    Some(Err(error)) => view! { <p role="alert">{error}</p> }.into_any(),
                    Some(Ok(report)) => {
                        let mut checks = report.checks.unwrap_or_default().into_iter().collect::<Vec<_>>();
                        checks.sort_by(|a, b| a.0.cmp(&b.0));
                        view! {
                            <dl class="health-checks">{checks.into_iter().map(|(key, result)| {
                                // An unknown key IS the name: naming a
                                // check the daemon has not shipped would
                                // be a guess dressed as a fact.
                                let friendly = check_name(&key);
                                let (tone, label) = check_badge(&key, &result);
                                let failed = tone == Tone::Danger;
                                view! {
                                    <div class="health-check">
                                        <dt>
                                            <strong class="health-check-name">
                                                {friendly.map_or_else(|| key.clone(), ToOwned::to_owned)}
                                            </strong>
                                            {friendly.map(|_| view! {
                                                <span class="mono health-check-key">{key.clone()}</span>
                                            })}
                                        </dt>
                                        <dd class:health-check-error=failed>
                                            <Badge tone=tone>{label}</Badge>
                                        </dd>
                                    </div>
                                }
                            }).collect_view()}</dl>
                            <dl class="health-facts"><div><dt>"Overall state"</dt><dd>{match report.status { HealthStatus::Ok => "Healthy", HealthStatus::Degraded => "Degraded", HealthStatus::Unavailable => "Unavailable" }}</dd></div>
                                <div><dt>"Version"</dt><dd>{report.version.unwrap_or_else(|| "Not reported".into())}</dd></div>
                            </dl>
                        }.into_any()
                    }
                }}
            </section>
            <Show when=move || me.get().is_some_and(|m| perms::is_trawl_admin(&m.permissions))>
                <section class="health-capacity" aria-labelledby="health-capacity-title">
                    <h2 id="health-capacity-title">"Capacity"</h2>
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
                    <p class="health-live-state" class:sr-only=move || dashboard.get().phase == DashboardPhase::Live role="status">{move || dashboard.get().phase.label()}</p>
                    {move || dashboard.get().snapshot.map(|s| view! {
                        <dl class="health-facts">
                            <div><dt>"Host"</dt><dd>{s.hostname}</dd></div>
                            <div><dt>"HTTP ingest rate"</dt><dd>{format!("{:.1} events/s", s.ingest_rate)}</dd></div>
                            <div><dt>"Query rate"</dt><dd>{format!("{:.1} queries/s", s.query_rate)}</dd></div>
                            <div><dt>"Hot buffer events"</dt><dd>{format!("{} / {}", format_count(s.hot_buffer_events as u64), format_count(s.hot_buffer_max_events as u64))}</dd></div>
                            <div><dt>"Hot buffer memory"</dt><dd>{format!("{} / {}", format_bytes(s.hot_buffer_bytes as u64), format_bytes(s.hot_buffer_max_bytes as u64))}</dd></div>
                            <div><dt>"Executors occupied"</dt><dd>{format!("{} / {}", s.pool_active, s.pool_capacity)}</dd></div>
                        </dl>
                    })}
                </section>
            </Show>
            </div>
            <Show when=move || me.get().is_some_and(|m| perms::is_trawl_admin(&m.permissions))><HealthDiagnostics/></Show>
            <Show when=move || me.get().is_some_and(|m| perms::can_query(&m.permissions))><HealthQueries/></Show>
        </div>
    }
}

/// Render the server's sample age unchanged, including when the stream stops.
fn storage_reading(files: u64, bytes: u64, measurement: StorageMeasurement) -> String {
    match (measurement.status, measurement.sample_age_secs) {
        (StorageMeasurementStatus::NotConfigured, _) => "Not configured".into(),
        (StorageMeasurementStatus::NotSampled, _) => "Awaiting measurement".into(),
        (StorageMeasurementStatus::Failed, None) => {
            "Measurement unavailable; collection failed".into()
        }
        (status, Some(age)) => {
            let state = if status == StorageMeasurementStatus::Failed {
                "Collection failed; last complete totals"
            } else {
                "Complete measurement"
            };
            format!(
                "{state}: {} files, {}. Sample age: {age}s at this snapshot.",
                format_exact(files),
                format_bytes(bytes)
            )
        }
        // The producer requires an age on complete samples. Do not display
        // numeric placeholders as measured if a malformed response violates it.
        (StorageMeasurementStatus::Complete, None) => "Measurement unavailable".into(),
    }
}

#[component]
fn HealthDiagnostics() -> impl IntoView {
    let dashboard = expect_context::<SharedDashboard>();
    view! {
        <div class="health-diagnostics">
            <section class="health-ingestion" aria-labelledby="health-ingestion-title">
                <h2 id="health-ingestion-title">"Ingestion"</h2>
                <p class="health-diagnostic-state" class:sr-only=move || dashboard.get().phase == DashboardPhase::Live>{move || dashboard.get().phase.label()}</p>
                {move || dashboard.get().snapshot.map(|s| view! {
                    <p class="health-note">"Counters are since process startup. HTTP counters exclude syslog."</p>
                    <dl class="health-facts">
                        <div><dt>"HTTP rejected events"</dt><dd>{format_exact(s.ingest_rejected)}</dd></div>
                        <div><dt>"Syslog configuration"</dt><dd>{if s.syslog_enabled { "Enabled" } else { "Disabled" }}</dd></div>
                    </dl>
                    {s.syslog_enabled.then(|| view! {
                        <dl class="health-facts">
                            <div><dt>"UDP received"</dt><dd>{format_exact(s.syslog_events_udp)}</dd></div>
                            <div><dt>"TCP received"</dt><dd>{format_exact(s.syslog_events_tcp)}</dd></div>
                            <div><dt>"Syslog rate"</dt><dd>{format!("{:.1} events/s", s.syslog_rate)}</dd></div>
                            <div><dt>"Parse errors"</dt><dd>{format_exact(s.syslog_parse_errors)}</dd></div>
                            <div><dt>"Backpressure drops"</dt><dd>{format_exact(s.syslog_dropped)}</dd></div>
                            <div><dt>"Active TCP connections"</dt><dd>{format_exact(s.syslog_tcp_connections)}</dd></div>
                        </dl>
                    })}
                    <p class="health-note">"Configured enablement does not test listener health. Received messages do not prove persistence."</p>
                })}
                <p class="health-note"><a href="https://trawl.sh/operate/ingestion/" target="_blank" rel="noopener noreferrer">"Ingestion guidance"</a></p>
            </section>
            <section class="health-storage" aria-labelledby="health-storage-title">
                <h2 id="health-storage-title">"Storage"</h2>
                <p class="health-diagnostic-state" class:sr-only=move || dashboard.get().phase == DashboardPhase::Live>{move || dashboard.get().phase.label()}</p>
                {move || dashboard.get().snapshot.map(|s| view! {
                    <div class="health-storage-source" data-source="wal">
                        <h3>"WAL"</h3>
                        <p>{storage_reading(s.wal_files, s.wal_bytes, s.wal_measurement)}</p>
                        <p class="health-note">"Includes active WAL files; these are not counts of compaction-eligible files."</p>
                    </div>
                    <div class="health-storage-source" data-source="parquet">
                        <h3>"Ingested Parquet"</h3>
                        <p>{storage_reading(s.parquet_files, s.parquet_bytes, s.parquet_measurement)}</p>
                        <p class="health-note">"Excludes saved report files. Sample age is relative to this dashboard snapshot, separate from live-update status."</p>
                    </div>
                    <dl class="health-facts">
                        <div><dt>"Successful compaction cycles"</dt><dd>{format_exact(s.compaction_runs)}</dd></div>
                        <div><dt>"Compaction error tally"</dt><dd>{format_exact(s.compaction_errors)}</dd></div>
                        <div><dt>"Last successful cycle"</dt><dd>{s.last_compaction_secs.map_or_else(|| "No successful cycle reported since startup".to_owned(), |age| format!("{age}s ago"))}</dd></div>
                    </dl>
                    <p class="health-note">"Compaction counters are since process startup. The error tally includes failed cycles and loss/error tallies; it can accompany successful cycles and exceed their count. A successful cycle can have no eligible work."</p>
                })}
                <p class="health-note"><a href="https://trawl.sh/architecture/recovery/" target="_blank" rel="noopener noreferrer">"Storage recovery"</a>" · "<a href="https://trawl.sh/operate/retention/" target="_blank" rel="noopener noreferrer">"Retention guidance"</a></p>
            </section>
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
            state: "running".into(),
            elapsed: format!("{} ms", format_exact(q.running_ms)),
        }
    });
    let recent = response.recent.into_iter().map(|entry| {
        let q = entry.snapshot;
        let state = if q.timed_out {
            "timeout"
        } else if q.error.is_some() {
            "error"
        } else {
            "success"
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
    let table_viewport = NodeRef::<leptos::html::Div>::new();
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
        <section class="health-queries tbl fleet-table-frame" aria-labelledby="health-queries-title">
            <div class="tbl-hd list-sheet-hd health-heading"><h2 id="health-queries-title" class="list-sheet-ttl">"Queries"</h2>
                <button class="btn health-queries-refresh" disabled=move || pending.get() on:click=move |_| refresh.update(|n| *n += 1)>"Refresh queries"</button>
            </div>
            {move || outcome.get().map(|text| view! { <p class="health-cancel-outcome" role="status">{text}</p> })}
            <fleet_ui::OverflowHint viewport=table_viewport/>
            <div node_ref=table_viewport class="health-query-scroll tbl-scroll" tabindex="0" role="region" aria-label="Queries table">
            <div class="tbl-body">
            {move || match rows.get() {
                None => view! { <p class="tbl-empty" role="status">"Loading queries..."</p> }.into_any(),
                Some(Err(error)) => view! { <p class="tbl-empty" role="alert">{error}</p> }.into_any(),
                Some(Ok(rows)) if rows.is_empty() => view! { <p class="tbl-empty">"No active or recent queries."</p> }.into_any(),
                Some(Ok(rows)) => {
                    let epoch = generation.load(Ordering::SeqCst);
                    view! {
                        <table class="health-query-table fleet-table">
                        <thead><tr><th scope="col">"Query"</th><th scope="col">"User"</th><th scope="col">"State"</th><th scope="col">"Elapsed"</th><th scope="col">"Action"</th></tr></thead>
                        <tbody>{rows.into_iter().map(|row| {
                            let (id, own) = (row.id, row.own);
                            let active = row.state == "running";
                            view! { <tr data-query-id=id data-own=own.to_string()>
                                <td><code>{row.query}</code></td><td>{row.user}</td><td><Badge tone=crate::tone_vocab::run_badge_tone(&row.state)>{crate::tone_vocab::run_status_label(&row.state).to_owned()}</Badge></td><td>{row.elapsed}</td>
                                <td><Show when=move || active && me.get().is_some_and(|m| perms::can_cancel_query(&m.permissions, own))>
                                    <button class="btn health-query-cancel" disabled=move || pending.get() on:click=move |_| confirm.update(|s| s.request((id, own, epoch)))>"Cancel"</button>
                                </Show></td>
                            </tr> }
                        }).collect_view()}</tbody>
                    </table> }.into_any()
                }
            }}
            </div></div>
            <Show when=move || confirm.get().is_open()>
                <ConfirmModal title="Cancel query" message="Request cancellation of this query? Work may already have finished.".to_string() confirm_label="Cancel query"
                    on_confirm=on_confirm on_cancel=Callback::new(move |()| confirm.update(ConfirmState::cancel))/>
            </Show>
        </section>
    }
}
