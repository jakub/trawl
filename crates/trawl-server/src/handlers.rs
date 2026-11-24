// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::HashMap;

use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{Path, Query, RawQuery, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, KeepAliveStream, Sse};
use axum::{Extension, Json};
use fleet_auth::VerifiedKey;
use serde::Deserialize;
use std::borrow::Cow;
use std::convert::Infallible;
use trawl_api::csv::sanitize_csv_formula;
use trawl_api::{
    CancelResponse, ClearHistoryResponse, CreateSavedRequest, DashboardSnapshot,
    DeleteSavedResponse, DeleteScheduleResponse, ExportRequest, FieldValuesResponse,
    FormatErrorDetail, FormatErrorKind, GlobalRunSummary, HealthResponse, HealthStatus,
    HistoryEntryResponse, HistoryResponse, ListAllRunsResponse, ListReportRunsResponse,
    ListSavedResponse, PaginationMeta, QueriesResponse, QueryExecution, QueryRequest,
    QueryResponse, QueryStatus, ReportRunResponse, ReportRunSummary, RetainedWorkSnapshot,
    RunsSortDir, RunsSortKey, RunsStatsResponse, SavedQueryResponse, ScheduleResponse,
    SchemaColumnResponse, SchemaResponse, SetScheduleRequest, StatsResponse, UpdateSavedRequest,
    ValidationResponse, WhoAmIResponse,
};
use trawl_engine::value::{QueryResult, Value};

use std::collections::BTreeMap;

use crate::bus::EventBus;
use crate::engine_sample::EngineCheck;
use crate::error::ServerError;
use crate::policy::{Authorized, Permission, TrawlAuthz as _, needs};
use crate::pool::PoolDebugInfo;
use crate::query_log::{HotBufferDebug, QueryLogEntry, ResultDebug, SourceDebug, TimingDebug};
use crate::query_timing::{ExportFormat, Outcome, TimingGuard, TimingKind};
use crate::report_window::{ScheduleWindow, format_window_bound};
use crate::scheduler::execute_scheduled_query;
use crate::shape::shaped;
use crate::state::{AppState, CachedFieldValues};
use crate::store::{
    CLAIM_WAIT, ClaimedManualRun, HistoryEntry, ManualRunClaim, ReportRun, RunOrigin, RunStatus,
    SavedQuery, Schedule, ScheduleWithStats, StoreError, format_interval, parse_duration_secs,
    parse_interval,
};
use crate::transport::http::RequestId;
use trawl_engine::timing::QueryPhase;

/// `DuckDB` answers a `read_parquet()` over a pattern that matches nothing
/// by substituting an empty relation (`is_no_files_error` in the executor),
/// so a file deleted between the pre-execution check and the open comes back
/// as a success with NO columns at all — a shape no SELECT can otherwise
/// produce. An engine error is the other way that window can show itself.
fn guard_vanished_run_result(
    mut outcome: crate::pool::ExecuteOutcome,
    run_files: &[crate::from_saved::RunFile],
) -> crate::pool::ExecuteOutcome {
    if run_files.is_empty() {
        return outcome;
    }
    let suspect = match &outcome.result {
        Ok(qr) => qr.columns.is_empty(),
        Err(ServerError::Engine(_)) => true,
        Err(_) => false,
    };
    if suspect && let Some(conflict) = crate::from_saved::unavailable_run_conflict(run_files) {
        outcome.result = Err(conflict);
    }
    outcome
}

/// A refusal here is the read's outcome, returned as one, and leaves
/// nothing to ask again: no file was opened, so no delete can land between
/// this check and an open, and asking again would only log the same
/// refusal twice.
fn check_run_files_before_execution(
    run_files: &[crate::from_saved::RunFile],
) -> (
    Option<crate::pool::ExecuteOutcome>,
    &[crate::from_saved::RunFile],
) {
    let refused = crate::from_saved::unavailable_run_conflict(run_files).map(|conflict| {
        crate::pool::ExecuteOutcome {
            result: Err(conflict),
            debug: None,
            severity_columns: Vec::new(),
            histogram: None,
        }
    });
    let recheck = if refused.is_some() { &[] } else { run_files };
    (refused, recheck)
}

/// `POST /api/v1/query` — execute a DSL query against the configured data source.
#[allow(clippy::too_many_lines)]
pub async fn query(
    State(state): State<AppState>,
    verified: Authorized<needs::Query>,
    Extension(request_id): Extension<RequestId>,
    req: Result<Json<QueryRequest>, JsonRejection>,
) -> Result<Json<QueryResponse>, ServerError> {
    let Json(req) = shaped("request body", req)?;

    let deadline =
        crate::deadline::Deadline::after(std::time::Duration::from_secs(state.query.timeout_secs));

    state
        .total_queries
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let max_rows = state.query.pool.max_result_rows();
    let limit = req.limit.unwrap_or(max_rows).min(max_rows);
    let offset = req.offset.unwrap_or(0);

    if offset + limit > max_rows {
        return Err(ServerError::BadRequest(format!(
            "offset + limit exceeds max_result_rows ({max_rows})"
        )));
    }

    // One id keys both the tracker entry and the pool's interrupt map, so
    // cancel-by-id interrupts the query the client sees.
    //
    // `pending` puts the id in the pool's registry before the entry opens,
    // so a cancel by id reaches the query while it still waits to start.
    // Declared first, it drops after `tracked`.
    let query_id = state.query.pool.allocate_query_id();
    crate::transport::failure::record_query_id(query_id);
    let pending = state
        .query
        .pool
        .pend(query_id, crate::pool::WorkKind::Query);
    let tracked = state.query.tracker.start(query_id, &verified, &req.query);

    // Default-filter lifecycle events carry metadata only — never the raw
    // DSL, which can contain customer identifiers or incident indicators.
    tracing::info!(
        event_type = "query_start",
        user = %verified.name,
        roles = %verified.roles_display(),
        query_id,
        query_len = req.query.len(),
        limit,
        offset,
        "executing query"
    );
    tracing::debug!(
        event_type = "query_text",
        query_id,
        query = %req.query,
        "raw query text (DEBUG-only: never stored under the default filter)"
    );

    let started_at = chrono::Utc::now();
    let start = std::time::Instant::now();
    let capture_debug = state.query.query_log.is_some();

    // The query's timing account (ADR-0046): its window starts where the
    // execution record's `duration_ms` does. Dropped unwritten — the
    // caller walked away before the pool took over — it writes
    // `outcome=abandoned`.
    let timing = TimingGuard::for_request(query_id, request_id.0, TimingKind::Query, start);

    // Admission runs over the pipeline the caller TYPED, before the
    // `from saved` stage is sliced off: resolving first would let an
    // over-cap original be admitted as an under-cap suffix.
    //
    // Resolution failures are the handler's: escaping by `?` here would close
    // the tracker entry this request already opened as abandoned, not as the
    // refusal it is. Expiry is the pre-start capacity refusal, since no work
    // was ever started and a timeout history row would claim otherwise.
    let checked = {
        let _phase = timing.clock().guard(QueryPhase::DslCheck);
        crate::admission::admit(&state.query.pool, &req.query, deadline, Some(&pending)).await
    };
    let admitted = match checked {
        Err(refusal) => Err(refusal),
        Ok(query) => {
            timing.clock().enter(QueryPhase::SavedLookup);
            match pending
                .wake()
                .run(
                    deadline,
                    try_resolve_from_saved(&state, &verified, &req.query, &query),
                )
                .await
            {
                Ok(Ok(Some(resolved))) => {
                    timing.clock().exit(QueryPhase::SavedLookup);
                    timing.set_kind(TimingKind::FromSaved);
                    Ok((query, Some(resolved)))
                }
                Ok(Ok(None)) => {
                    timing.clock().discard(QueryPhase::SavedLookup);
                    Ok((query, None))
                }
                Ok(Err(refusal)) => {
                    timing.clock().exit(QueryPhase::SavedLookup);
                    Err(refusal)
                }
                Err(crate::pool::StartRefusal::Expired) => {
                    timing.clock().exit(QueryPhase::SavedLookup);
                    // Refused here, before any work: named where it is made,
                    // like the pool's own capacity refusals.
                    timing.emit_complete(Outcome::CapacityRefused, std::time::Instant::now());
                    Err(ServerError::CapacityNotStarted)
                }
                // A cancel by id reached the lookup. The same refusal, and
                // the account books it as one below.
                Err(crate::pool::StartRefusal::Cancelled) => {
                    timing.clock().exit(QueryPhase::SavedLookup);
                    Err(pending.refuse())
                }
            }
        }
    };

    let histogram_plan = match &admitted {
        Ok((query, saved)) => crate::histogram::plan(req.histogram, query, saved.is_some()),
        Err(_) => crate::histogram::HistogramPlan::Absent,
    };

    let mut run_files: Vec<crate::from_saved::RunFile> = Vec::new();
    let mut recheck: &[crate::from_saved::RunFile] = &[];

    let (outcome, degraded_fields) = match admitted {
        Err(refusal) => (
            crate::pool::ExecuteOutcome {
                result: Err(refusal),
                debug: None,
                severity_columns: Vec::new(),
                histogram: None,
            },
            Vec::new(),
        ),
        Ok((query, Some(mut resolved))) => {
            run_files = std::mem::take(&mut resolved.run_files);
            let (refused, files) = check_run_files_before_execution(&run_files);
            recheck = files;
            if let Some(refused) = refused {
                (refused, Vec::new())
            } else {
                // Both halves of what the caller is actually reading: the
                // stages they typed, and the saved query whose recorded run
                // produced the rows those stages run over. Nothing stamps a
                // report run at write time, so a degraded pin the saved query
                // bound would otherwise go unmentioned.
                let snapshot = std::sync::Arc::clone(&state.query.degraded_fields.lock());
                let degraded = degraded_fields_with_saved(
                    snapshot,
                    &query,
                    &resolved.saved_dsl,
                    deadline,
                    pending.wake(),
                )
                .await;
                (
                    state
                        .query
                        .pool
                        .execute_with_source(
                            query_id,
                            &resolved.remaining_dsl,
                            &resolved.source,
                            deadline,
                            capture_debug,
                            crate::pool::WorkContext::key(
                                crate::pool::WorkKind::FromSaved,
                                verified.id,
                            )
                            .with_user(&verified.name)
                            .with_timing(timing.timing().clone()),
                        )
                        .await,
                    degraded,
                )
            }
        }
        Ok((query, None)) => {
            let degraded = degraded_fields_for(&state, [&query]);
            let mut work = crate::pool::WorkContext::key(crate::pool::WorkKind::Query, verified.id)
                .with_user(&verified.name)
                .with_timing(timing.timing().clone());
            if let crate::histogram::HistogramPlan::Count { kept_some } = histogram_plan {
                work = work.with_histogram(crate::histogram::HistogramAsk { kept_some });
            }
            (
                state
                    .query
                    .pool
                    .execute(query_id, &req.query, deadline, capture_debug, work)
                    .await,
                degraded,
            )
        }
    };

    // Between the check above and `DuckDB`'s open there is a window a
    // delete can land in. This closes it on the way out.
    //
    // The re-check is `post` for a `from saved` read, the one query it
    // can do anything for. An account the pool already wrote (a timeout,
    // a capacity refusal) is closed, and its worker may still be inside a
    // phase: nothing more is booked on it.
    let outcome = if run_files.is_empty() || timing.emitted() {
        guard_vanished_run_result(outcome, recheck)
    } else {
        timing.clock().time(QueryPhase::Post, || {
            guard_vanished_run_result(outcome, recheck)
        })
    };

    // One instant ends both the execution record's duration and the
    // timing account's window, so the two cover the same time.
    let end = std::time::Instant::now();
    let elapsed = end.saturating_duration_since(start);
    let duration_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
    let duration_secs = elapsed.as_secs_f64();
    timing.emit_complete(
        outcome
            .result
            .as_ref()
            .map_or_else(Outcome::of_error, |_| Outcome::Success),
        end,
    );

    // Which result columns render as OTel tokens is decided by the executing
    // task under the pins the rows were produced with, not by the catalog as
    // it stands now: a repin landing between execution and the response must
    // not retype the answer's presentation.
    let severity_columns = outcome.severity_columns;
    let counted = outcome.histogram;

    match outcome.result {
        Ok(qr) => {
            let total = qr.row_count();
            let paginated = qr.paginate(offset, limit);
            let returned = paginated.row_count();
            let histogram = match histogram_plan {
                crate::histogram::HistogramPlan::Absent => None,
                crate::histogram::HistogramPlan::Refuse(reason) => {
                    Some(trawl_api::EventHistogram::Unavailable { reason })
                }
                crate::histogram::HistogramPlan::Count { .. } => counted,
            };

            tracked.complete(total);

            // Under the request's own deadline (ADR-0024): a wedged history store
            // must not hold a finished answer past the budget the caller was
            // promised. Expiry abandons the write, never the result.
            let recorded = deadline
                .run(state.storage.history.record_query(
                    verified.id,
                    &req.query,
                    duration_ms,
                    total,
                    RunStatus::Success,
                ))
                .await;
            match recorded {
                Ok(Err(e)) => {
                    tracing::debug!(
                        event_type = "history_error",
                        key_id = verified.id,
                        error = %e,
                        "failed to record query in history store"
                    );
                    let fault = ServerError::from(e);
                    tracing::warn!(
                        event_type = "history_error",
                        key_id = verified.id,
                        error_class = fault.error_class(),
                        cause_kind = fault.cause_kind().as_str(),
                        "failed to record query in history store"
                    );
                }
                Err(crate::deadline::Expired) => {
                    tracing::warn!(
                        event_type = "history_error",
                        key_id = verified.id,
                        "abandoned the history write: the query's deadline passed with the result already in hand"
                    );
                }
                Ok(Ok(_)) => {}
            }

            tracing::info!(
                event_type = "query_complete",
                user = %verified.name,
                query_id,
                query_len = req.query.len(),
                total_rows = total,
                returned_rows = returned,
                duration_ms,
                "query complete"
            );

            // SECURITY: no principal-derived label here — `/metrics` is
            // unauthenticated (see `transport::http`). Role names are
            // operator-defined and cross-app since ADR-0006, so labelling by
            // them would publish fleet role membership to any scraper and
            // make the series count combinatorial in role sets. Per-principal
            // attribution lives in the authenticated query log / history.
            metrics::counter!(crate::metrics::QUERIES_TOTAL, "status" => "success").increment(1);
            metrics::histogram!(crate::metrics::QUERY_DURATION).record(duration_secs);

            write_query_log(
                &state,
                &verified,
                &req.query,
                outcome.debug.as_ref(),
                timing.written().as_ref(),
                Some(&paginated),
                duration_ms,
                None,
            );

            Ok(Json(QueryResponse {
                execution: Some(QueryExecution {
                    started_at: started_at.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
                    duration_ms,
                }),
                result: paginated,
                pagination: PaginationMeta {
                    limit,
                    offset,
                    returned,
                    total,
                },
                degraded_fields,
                severity_columns,
                histogram,
            }))
        }
        Err(ServerError::Timeout) => {
            tracked.timeout();

            metrics::counter!(crate::metrics::QUERIES_TOTAL, "status" => "timeout").increment(1);
            metrics::histogram!(crate::metrics::QUERY_DURATION).record(duration_secs);

            tracing::warn!(
                event_type = "query_timeout",
                user = %verified.name,
                query_id,
                query_len = req.query.len(),
                duration_ms,
                timeout_secs = state.query.timeout_secs,
                "query timed out"
            );

            write_query_log(
                &state,
                &verified,
                &req.query,
                outcome.debug.as_ref(),
                timing.written().as_ref(),
                None,
                duration_ms,
                Some("query timed out"),
            );

            Err(ServerError::Timeout)
        }
        Err(e) => {
            // SECURITY: the tracker stores the public message, the string
            // the client reads, which never carries database internals
            // (ADR-0040).
            let safe_msg = e.safe_message();
            tracked.fail(&safe_msg);

            metrics::counter!(crate::metrics::QUERIES_TOTAL, "status" => "error").increment(1);
            metrics::histogram!(crate::metrics::QUERY_DURATION).record(duration_secs);

            // Default-filter failure events carry the class only. Neither
            // `safe_msg` nor the raw error belongs in them: the public
            // message keeps parser/emitter text (which quotes the user's
            // own tokens and format strings), and the raw database error
            // embeds the generated SQL plus the values it choked on.
            let error_class = e.error_class();
            match &e {
                // Every 400-class engine answer, including the refusal
                // the engine proved by asking `DuckDB` about the
                // caller's own column: a query nobody could have
                // answered is the caller's mistake, and logging it at
                // ERROR as the server's own failure buries the ones
                // that are. `error_type` is the coarse lane label the
                // three have shared since the emitter joined the
                // parser here; `error_class` tells them apart.
                ServerError::Engine(
                    trawl_engine::error::EngineError::Parse(_)
                    | trawl_engine::error::EngineError::Emit(_)
                    | trawl_engine::error::EngineError::Refused { .. },
                ) => {
                    tracing::warn!(
                        event_type = "query_failed",
                        error_type = "parse",
                        error_class,
                        user = %verified.name,
                        query_id,
                        query_len = req.query.len(),
                        duration_ms,
                        "query failed: bad request"
                    );
                }
                _ => {
                    tracing::error!(
                        event_type = "query_failed",
                        error_type = "engine",
                        error_class,
                        user = %verified.name,
                        query_id,
                        query_len = req.query.len(),
                        duration_ms,
                        "query failed: engine error"
                    );
                }
            }
            tracing::debug!(
                event_type = "query_error_text",
                query_id,
                error_class,
                error = %e,
                safe_error = %safe_msg,
                "query failure detail (DEBUG-only: never stored under the default filter)"
            );

            write_query_log(
                &state,
                &verified,
                &req.query,
                outcome.debug.as_ref(),
                timing.written().as_ref(),
                None,
                duration_ms,
                Some(&safe_msg),
            );

            Err(e)
        }
    }
}

/// `GET /metrics` — prometheus scrape endpoint (unauthenticated).
pub async fn prometheus_metrics(State(state): State<AppState>) -> impl IntoResponse {
    let hot_buffer = state.query.hot_buffer.clone();
    let publication = state.query.pool.publication();
    let data_root = std::path::PathBuf::from(state.query.pool.base_dir());
    // Read here, on the reactor: the closure below deliberately holds no
    // pool state, so the registry lock never travels onto the blocking
    // pool behind a filesystem walk.
    let query = crate::metrics::QueryGauges::read(&state.query.pool, &state.engine_sample);
    let scratch_dir = state.query.pool.scratch().path().to_path_buf();
    let wal_dir = state
        .ingest
        .wal_writer
        .as_ref()
        .map(|w| w.dir().to_path_buf());
    let repin_jobs = std::sync::Arc::clone(&state.repin_jobs);
    let _ = tokio::task::spawn_blocking(move || {
        #[cfg(target_os = "linux")]
        metrics_process::Collector::default().collect();
        crate::metrics::collect_gauges(
            hot_buffer.as_ref(),
            &publication,
            &data_root,
            wal_dir.as_deref(),
            &scratch_dir,
            query,
            &repin_jobs,
        );
    })
    .await;
    let body = state.metrics_handle.render();
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
}

/// The names of the checks `GET /api/v1/health` reports, in the order
/// [`health`] fills them.
pub use trawl_api::doctor::health::HEALTH_CHECK_NAMES;

/// `GET /api/v1/health` — unauthenticated health check with subsystem probes.
pub async fn health(State(state): State<AppState>) -> (StatusCode, Json<HealthResponse>) {
    let (http_status, report) = health_report(&state).await;

    // The engine sample is the one check whose failure answers 503, and the
    // 503 is built directly rather than through `ServerError`'s response, so
    // its typed error is recorded here: the request's `http_failure` names
    // the failed sample as its cause.
    if report.status == HealthStatus::Unavailable {
        crate::transport::failure::record_error(&ServerError::EngineSampleFailed);
    }

    (http_status, Json(report))
}

/// Shared by the health route and `GET /api/v1/capabilities`. It records no
/// request failure: that is the health route's, the one caller that answers
/// 503 for `unavailable`.
pub(crate) async fn health_report(state: &AppState) -> (StatusCode, HealthResponse) {
    let duckdb = state.engine_sample.read();

    // `/health` is unauthenticated and unthrottled, so it must not amplify a
    // burst of probes onto the small, shared keystore pool that bearer
    // verification depends on: `AuthState::ping_cached` memoises the ping for a
    // few seconds and serialises refreshes.
    let (auth_result, storage_result) =
        tokio::join!(state.auth.ping_cached(), state.storage.ping_cached());

    let base_dir = state.query.pool.base_dir().to_owned();
    let data_result = std::fs::metadata(&base_dir)
        .ok()
        .filter(std::fs::Metadata::is_dir)
        .map_or_else(
            || {
                if base_dir.is_empty() {
                    Err("base_dir is empty".to_owned())
                } else {
                    Err(format!("{base_dir} is not a readable directory"))
                }
            },
            |_| Ok(()),
        );

    let auth_healthy = auth_result.is_ok();
    let storage_healthy = storage_result.is_ok();
    let data_healthy = data_result.is_ok();
    let ingest_admitting = ingest_capacity_admitting(state.query.hot_buffer.as_deref());
    let corpus = corpus_check(&state.query.pool.publication());
    let corpus_settled = corpus == "ok";
    let check_value = |healthy| String::from(if healthy { "ok" } else { "error" });

    let values: [String; HEALTH_CHECK_NAMES.len()] = [
        String::from(duckdb.as_str()),
        check_value(auth_healthy),
        check_value(storage_healthy),
        check_value(data_healthy),
        String::from(ingest_capacity_check(ingest_admitting)),
        String::from(corpus),
    ];
    let checks: HashMap<String, String> = HEALTH_CHECK_NAMES
        .into_iter()
        .map(str::to_owned)
        .zip(values)
        .collect();

    // `slow` is a warning, not a failure: the gauge drops only on `error`.
    metrics::gauge!("trawl_health_check", "subsystem" => "duckdb").set(
        if duckdb == EngineCheck::Error {
            0.0
        } else {
            1.0
        },
    );
    metrics::gauge!("trawl_health_check", "subsystem" => "auth_db").set(if auth_healthy {
        1.0
    } else {
        0.0
    });
    metrics::gauge!("trawl_health_check", "subsystem" => "storage_db").set(if storage_healthy {
        1.0
    } else {
        0.0
    });
    metrics::gauge!("trawl_health_check", "subsystem" => "data_path").set(if data_healthy {
        1.0
    } else {
        0.0
    });
    metrics::gauge!("trawl_health_check", "subsystem" => "ingest_capacity")
        .set(if ingest_admitting { 1.0 } else { 0.0 });
    metrics::gauge!("trawl_health_check", "subsystem" => "corpus").set(if corpus_settled {
        1.0
    } else {
        0.0
    });

    let status = derive_health_status(
        duckdb,
        auth_healthy,
        storage_healthy,
        data_healthy,
        ingest_admitting,
        corpus_settled,
    );
    let http_status = match status {
        HealthStatus::Ok | HealthStatus::Degraded => StatusCode::OK,
        HealthStatus::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
    };

    (
        http_status,
        HealthResponse {
            status,
            checks: Some(checks),
            version: Some(trawl_core::version::PKG_VERSION.to_owned()),
        },
    )
}

fn ingest_capacity_admitting(hot_buffer: Option<&crate::hot_buffer::HotBuffer>) -> bool {
    hot_buffer
        .is_none_or(|buf| buf.admission_state() != crate::hot_buffer::AdmissionState::Refusing)
}

const fn ingest_capacity_check(admitting: bool) -> &'static str {
    if admitting { "ok" } else { "refusing" }
}

/// The `corpus` check's wire value: `"ok"`, or the reason corpus reads
/// refuse, by the same precedence the refusal uses (ADR-0041). Takes no
/// publication guard, so a long publication never stalls the probe.
fn corpus_check(publication: &crate::publication::PublicationGate) -> &'static str {
    publication
        .unsettled()
        .map_or("ok", crate::publication::CorpusUnsettled::label)
}

#[allow(clippy::fn_params_excessive_bools)] // subsystem flags, call sites are named
fn derive_health_status(
    duckdb: EngineCheck,
    auth_ok: bool,
    storage_ok: bool,
    data_ok: bool,
    ingest_admitting: bool,
    corpus_settled: bool,
) -> HealthStatus {
    if duckdb == EngineCheck::Error {
        return HealthStatus::Unavailable;
    }
    if duckdb == EngineCheck::Slow
        || !auth_ok
        || !storage_ok
        || !data_ok
        || !ingest_admitting
        || !corpus_settled
    {
        return HealthStatus::Degraded;
    }
    HealthStatus::Ok
}

#[derive(Debug, Deserialize)]
pub struct SchemaParams {
    pub service: Option<String>,
    /// Lift the `last_seen` retention window (show aged-out fields too).
    pub all: Option<bool>,
}

/// `GET /api/v1/schema` — the data schema, served from the field catalog.
///
/// `cached` reports whether the corpus facts came from the cache, not the
/// columns. A `?service=` listing is served straight from postgres: caching
/// per client-chosen service name would be an unbounded cache.
pub async fn schema(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    params: Result<Query<SchemaParams>, QueryRejection>,
) -> Result<Json<SchemaResponse>, ServerError> {
    if !verified.has_permission(Permission::SchemaRead) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }
    let Query(params) = shaped("query parameters", params)?;

    let (hot_events, hot_bytes) = hot_buffer_stats(&state);

    // Columns: a catalog SELECT. Postgres down → 503 (the same dependency
    // history/saved already have). Deliberately no fallback to the
    // in-process pin cache: that would fork schema truth.
    let since = if params.all == Some(true) {
        None
    } else {
        since_from_secs(state.query.retention_horizon_secs)
    };

    let columns = if params.service.is_some() {
        catalog_schema_columns(&state, params.service.clone(), since).await?
    } else {
        // Hold the mutex for the full check-then-refresh cycle, like the
        // corpus facts below: only one request runs the aggregate. The two
        // request shapes (windowed / `?all=true`) each own a slot, so
        // alternating traffic cannot evict the other shape's entry — see
        // `schema_columns_cache` in state.rs.
        let mut cache = state.query.schema_columns_cache.lock().await;
        // Read the repin generation after taking the mutex and before the
        // SELECT. After: a request that waited on the mutex would otherwise
        // validate the entry the holder just built against a generation it
        // sampled before the flip. Before: an entry whose SELECT straddles a
        // flip is then stamped with the older generation, so the next read
        // discards it rather than serving a type the corpus no longer has.
        let generation = state.query.field_catalog.repin_generation();
        let slot = &mut cache[usize::from(since.is_some())];
        let fresh = slot
            .as_ref()
            .and_then(|c| c.serve(state.query.schema_cache_ttl_secs, generation));
        if let Some(columns) = fresh {
            columns
        } else {
            let columns = catalog_schema_columns(&state, None, since).await?;
            *slot = Some(crate::state::CachedSchemaColumns {
                columns: columns.clone(),
                cached_at: std::time::Instant::now(),
                pins_generation: generation,
            });
            columns
        }
    };

    // Corpus facts: hold the mutex for the full check-then-refresh cycle to
    // prevent thundering herd — only one request walks while others wait.
    let mut cache = state.query.schema_cache.lock().await;
    let (facts, cached) = match &*cache {
        Some(facts) if facts.cached_at.elapsed().as_secs() < state.query.schema_cache_ttl_secs => {
            tracing::debug!(
                event_type = "schema_cache_hit",
                user = %verified.name,
                "serving corpus facts from cache"
            );
            (facts.clone(), true)
        }
        _ => {
            let start = std::time::Instant::now();
            let data_root = std::path::PathBuf::from(state.query.pool.base_dir());
            let (earliest_date, latest_date, total_bytes, services, file_count) =
                tokio::task::spawn_blocking(move || collect_catalog_metadata(&data_root))
                    .await
                    .unwrap_or_default();
            let facts = crate::state::CachedCorpusFacts {
                cached_at: std::time::Instant::now(),
                earliest_date,
                latest_date,
                total_bytes,
                services,
                file_count,
            };
            tracing::info!(
                event_type = "schema_facts_refresh",
                user = %verified.name,
                columns = columns.len(),
                file_count = facts.file_count,
                services = facts.services.len(),
                duration_ms = start.elapsed().as_millis(),
                "corpus facts walk complete"
            );
            *cache = Some(facts.clone());
            (facts, false)
        }
    };
    drop(cache);

    Ok(Json(SchemaResponse {
        columns,
        file_count: facts.file_count,
        cached,
        earliest_date: facts.earliest_date,
        latest_date: facts.latest_date,
        total_bytes: Some(facts.total_bytes),
        services: Some(facts.services),
        hot_buffer_events: hot_events,
        hot_buffer_bytes: hot_bytes,
    }))
}

async fn catalog_schema_columns(
    state: &AppState,
    service: Option<String>,
    since: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<Vec<SchemaColumnResponse>, ServerError> {
    let filter = crate::store::FieldListFilter {
        service,
        since,
        // Names and types only — never pay for the conflict evidence on the
        // endpoint autocomplete polls (and whose `?service=` form is
        // deliberately uncached).
        with_conflicts: false,
        ..Default::default()
    };
    let (fields, _truncated) = state.storage.catalog.list_fields(&filter).await?;
    let mut columns: Vec<SchemaColumnResponse> = fields
        .into_iter()
        .map(|f| SchemaColumnResponse {
            name: f.field,
            data_type: f.duckdb_type,
        })
        .collect();
    trawl_api::value::sort_by_display_rank(&mut columns, |c| &c.name);
    Ok(columns)
}

fn hot_buffer_stats(state: &AppState) -> (Option<u64>, Option<u64>) {
    state.query.hot_buffer.as_ref().map_or((None, None), |buf| {
        (
            Some(buf.event_count() as u64),
            Some(buf.byte_count() as u64),
        )
    })
}

/// Path structure: `{base}/{env}/{YYYY-MM-DD}/{HH}/{service}.parquet`, or
/// `{base}/{env}/{YYYY-MM-DD}/{service}.parquet` for a daily rollup — hence
/// the ancestor walk for the date component rather than a fixed depth.
fn collect_catalog_metadata(
    base: &std::path::Path,
) -> (Option<String>, Option<String>, u64, Vec<String>, u64) {
    use std::collections::BTreeSet;

    if !base.is_dir() {
        return (None, None, 0, Vec::new(), 0);
    }

    let Ok(entries) = crate::metrics::walk_parquet_files(base) else {
        return (None, None, 0, Vec::new(), 0);
    };

    let mut dates: BTreeSet<String> = BTreeSet::new();
    let mut services: BTreeSet<String> = BTreeSet::new();
    let mut total_bytes: u64 = 0;

    for (path, size) in &entries {
        total_bytes += size;

        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            services.insert(stem.to_owned());
        }

        for ancestor in path.ancestors().skip(1) {
            if let Some(name) = ancestor.file_name().and_then(|n| n.to_str())
                && crate::env_dirs::parse_date(name).is_some()
            {
                dates.insert(name.to_owned());
                break;
            }
        }
    }

    let earliest = dates.iter().next().cloned();
    let latest = dates.iter().next_back().cloned();
    let services: Vec<String> = services.into_iter().collect();
    let file_count = u64::try_from(entries.len()).unwrap_or(0);

    (earliest, latest, total_bytes, services, file_count)
}

/// One unit of retained work as a given reader may see it (ADR-0024).
pub(crate) fn retained_snapshot(
    work: &crate::pool::RetainedWork,
    verified: &VerifiedKey,
) -> RetainedWorkSnapshot {
    let entitled = verified.has_permission(Permission::ServerManage)
        || matches!(work.owner, crate::pool::WorkOwner::Key(_));
    RetainedWorkSnapshot {
        id: work.id,
        kind: work.kind.as_str().to_owned(),
        started: work.started,
        retained_ms: u64::try_from(work.retained.as_millis()).unwrap_or(u64::MAX),
        user: entitled
            .then(|| work.user.as_deref().map(ToOwned::to_owned))
            .flatten(),
        query: entitled.then(|| work.display.clone()).flatten(),
    }
}

/// `GET /api/v1/queries` — view active, recent and retained queries.
#[allow(clippy::unused_async)]
pub async fn queries(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
) -> Result<Json<QueriesResponse>, ServerError> {
    if !verified.has_permission(Permission::Query) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }

    // One lifecycle read, taken first: an id it reports as retained is
    // one whose request already recorded its outcome, so listing it as
    // active too would count the same permit twice (ADR-0024). The
    // tracker read happens after, so work that becomes retained in
    // between is merely still listed as active for one more request.
    let retained = state.query.pool.retained_work();
    let retained_ids: std::collections::HashSet<u64> = retained.iter().map(|w| w.id).collect();

    Ok(Json(QueriesResponse {
        active: state
            .query
            .tracker
            .active_for(verified.id)
            .into_iter()
            .filter(|q| !retained_ids.contains(&q.snapshot.id))
            .collect(),
        recent: state.query.tracker.recent_for(verified.id),
        retained: retained
            .iter()
            .map(|work| retained_snapshot(work, &verified))
            .collect(),
    }))
}

fn check_cancel_authority(
    verified: &VerifiedKey,
    tracker_owner: Option<i64>,
    pool_owner: Option<crate::pool::WorkOwner>,
) -> Result<(), ServerError> {
    require_cancel_grant(verified)?;
    if verified.has_permission(Permission::ServerManage) {
        return Ok(());
    }
    let mine = tracker_owner == Some(verified.id)
        || pool_owner == Some(crate::pool::WorkOwner::Key(verified.id));
    if mine {
        Ok(())
    } else {
        Err(ServerError::Forbidden("cannot cancel this query".into()))
    }
}

/// The part of the cancel authority that needs no query id: either
/// permission. The handler runs it before reading the id, so a key holding
/// neither learns nothing about whether its path parsed.
fn require_cancel_grant(verified: &VerifiedKey) -> Result<(), ServerError> {
    if verified.has_permission(Permission::ServerManage)
        || verified.has_permission(Permission::QueryCancel)
    {
        Ok(())
    } else {
        Err(ServerError::Forbidden("insufficient permissions".into()))
    }
}

/// `DELETE /api/v1/queries/{id}` — cancel a running query by ID.
pub async fn cancel_query(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    query_id: Result<Path<u64>, PathRejection>,
) -> Result<Json<CancelResponse>, ServerError> {
    require_cancel_grant(&verified)?;
    let Path(query_id) = shaped("query id", query_id)?;
    // Two records, one owner. The tracker drops a query as soon as its
    // request records an outcome; the pool keeps the work registered until
    // the physical query actually stops (ADR-0024). Asking both is what
    // keeps a query the client has already been told timed out cancellable
    // by the key that submitted it.
    check_cancel_authority(
        &verified,
        state.query.tracker.owner_key_id(query_id),
        state.query.pool.owner_of(query_id),
    )?;

    let cancelled = state.query.pool.cancel_by_id(query_id);

    tracing::info!(
        event_type = "query_cancelled",
        user = %verified.name,
        query_id,
        cancelled,
        "query cancellation requested"
    );

    Ok(Json(CancelResponse {
        cancelled,
        query_id,
    }))
}

/// `POST /api/v1/validate` — validate a DSL query without executing it.
///
/// It does not check fields against the schema, or that every lane can run
/// the query.
pub async fn validate_query(
    State(state): State<AppState>,
    _: Authorized<needs::Validate>,
    req: Result<Json<QueryRequest>, JsonRejection>,
) -> Result<Json<ValidationResponse>, ServerError> {
    let Json(req) = shaped("request body", req)?;
    let deadline =
        crate::deadline::Deadline::after(std::time::Duration::from_secs(state.query.timeout_secs));
    validate(&state.query.pool, req.query, deadline)
        .await
        .map(Json)
}

pub(crate) async fn validate(
    pool: &crate::pool::ExecutorPool,
    dsl: String,
    deadline: crate::deadline::Deadline,
) -> Result<ValidationResponse, ServerError> {
    pool.prepare(deadline, crate::prepare::Door::Validate, move |stop| {
        validation(&dsl, stop)
    })
    .await?
}

/// A parse or validation refusal is the answer (`valid: false`). A
/// cancellation is not a refusal of the text, so it is the error.
fn validation(
    dsl: &str,
    stop: &trawl_core::cancel::Stop,
) -> Result<ValidationResponse, ServerError> {
    use trawl_engine::error::EngineError;

    let invalid = |errors: &[trawl_core::parser::ParseError]| ValidationResponse {
        valid: false,
        errors: errors
            .iter()
            .map(crate::error::parse_error_to_detail)
            .collect(),
        formatted: None,
        format_error: None,
    };
    let ast = match crate::admission::check_dsl_with(dsl, stop) {
        Ok(ast) => ast,
        Err(ServerError::Engine(EngineError::Parse(errors))) => return Ok(invalid(&errors)),
        Err(ServerError::Engine(EngineError::Emit(e))) => {
            return Ok(invalid(&e.to_parse_errors(dsl.len())));
        }
        Err(other) => return Err(other),
    };
    // The formatting's reparse cannot be stopped, so this is its last
    // cancellation point.
    stop.check()
        .map_err(|trawl_core::cancel::Cancelled| ServerError::Engine(EngineError::Cancelled))?;

    let (formatted, format_error) = match trawl_core::format::checked_format_query(&ast) {
        Ok(text) => (Some(text), None),
        Err(e) => {
            let kind = match e {
                trawl_core::format::FormatError::TooLong { .. } => FormatErrorKind::TooLong,
                trawl_core::format::FormatError::Unfaithful => FormatErrorKind::Unfaithful,
            };
            let message = e.to_string();
            (None, Some(FormatErrorDetail { kind, message }))
        }
    };
    Ok(ValidationResponse {
        valid: true,
        errors: vec![],
        formatted,
        format_error,
    })
}

/// `GET /api/v1/stats` — server statistics and metrics (admin only).
pub async fn stats(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
) -> Result<Json<StatsResponse>, ServerError> {
    if !verified.has_permission(Permission::ServerManage) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }

    Ok(Json(StatsResponse {
        uptime_secs: state.start_time.elapsed().as_secs(),
        total_queries: state
            .total_queries
            .load(std::sync::atomic::Ordering::Relaxed),
        active_queries: state.query.tracker.active().len(),
        pool_available: state.query.pool.available_permits(),
        pool_capacity: state.query.pool.capacity(),
        pool_retained: state.query.pool.retained(),
    }))
}

/// `GET /api/v1/whoami` — returns identity and permissions for the current token.
pub async fn whoami(Extension(verified): Extension<VerifiedKey>) -> Json<WhoAmIResponse> {
    // Permissions are server-scoped: only recognized trawl permissions are
    // emitted, in canonical order — echoing raw keystore strings would
    // advertise gates no handler checks, and canonical order keeps the
    // golden wire tests deterministic. Role names travel unfiltered (roles
    // are cross-app bundles, not app-scoped).
    let permissions = verified
        .trawl_permissions()
        .into_iter()
        .map(|p| p.as_str().to_owned())
        .collect();

    Json(WhoAmIResponse {
        prefix: verified.prefix.clone(),
        name: verified.name.clone(),
        kind: crate::policy::wire_kind(verified.kind),
        roles: verified.roles().to_vec(),
        permissions,
    })
}

/// `GET /api/v1/capabilities` — what this server offers a client: version,
/// DSL, bounds, quotas, admission, link origin and health (ADR-0058).
pub async fn capabilities(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
) -> Json<trawl_api::capabilities::CapabilitiesResponse> {
    Json(crate::capabilities::report(&state, &verified).await)
}

/// `GET /api/v1/dashboard` — full dashboard snapshot (admin only).
pub async fn dashboard(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
) -> Result<Json<DashboardSnapshot>, ServerError> {
    if !verified.has_permission(Permission::ServerManage) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }

    let snapshot = state.dashboard_snapshot.lock().clone();
    match snapshot {
        Some(s) => Ok(Json(s)),
        None => Err(ServerError::ServiceUnavailable(
            "dashboard data not yet available".into(),
        )),
    }
}

/// `GET /api/v1/dashboard/stream` — push the dashboard snapshot every 2s via SSE (admin only).
///
/// Before the collector's first tick the loop silently skips (never a 503
/// mid-stream: `EventSource` treats a non-200 as terminal).
pub async fn dashboard_stream(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
) -> Result<
    Sse<
        KeepAliveStream<
            std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Result<Event, Infallible>> + Send>>,
        >,
    >,
    ServerError,
> {
    if !verified.has_permission(Permission::ServerManage) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }

    let permit = state
        .query
        .dashboard_sse_semaphore
        .clone()
        .try_acquire_owned()
        .map_err(|_| ServerError::TooManyStreams)?;

    let stats_stream: std::pin::Pin<
        Box<dyn tokio_stream::Stream<Item = Result<Event, Infallible>> + Send>,
    > = Box::pin(async_stream::stream! {
        let _permit = permit; // hold until the client disconnects

        const PUSH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
        let mut timer = tokio::time::interval(PUSH_INTERVAL);
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            timer.tick().await; // first tick is immediate
            let snapshot = state.dashboard_snapshot.lock().clone();
            let Some(snapshot) = snapshot else { continue };
            let Ok(json) = serde_json::to_string(&snapshot) else { continue };
            yield Ok(Event::default().event("stats").data(json));
        }
    });

    Ok(Sse::new(stats_stream).keep_alive(KeepAlive::default()))
}

/// `GET /api/v1/schema/services` — rich per-service schema from background refresh.
pub async fn schema_services(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
) -> Result<Json<trawl_api::ServiceSchemaResponse>, ServerError> {
    if !verified.has_permission(Permission::SchemaRead) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }

    let (hot_events, hot_bytes) = hot_buffer_stats(&state);

    let cache = state.query.service_schema_cache.lock().clone();
    let Some(cached) = cache else {
        return Err(ServerError::ServiceUnavailable(
            "service schema not yet available".into(),
        ));
    };
    let mut services = cached.services;

    // Stamp the degraded badge (ADR-0011) from the schema-refresh tick's
    // snapshot — the same set the query notice reads, so the two surfaces
    // cannot disagree about a field.
    //
    // Intersected with the service's current columns: `field_conflict_stats`
    // is ever-observed evidence, so a field whose data has since aged out of
    // the retained corpus would otherwise keep badging a service that no
    // longer has anything to repin. Never the inverse join — carrying the
    // column is not evidence of having conflicted on it.
    let snapshot = std::sync::Arc::clone(&state.query.degraded_fields.lock());
    if !snapshot.fields.is_empty() {
        for svc in &mut services {
            svc.degraded_fields = snapshot
                .services_of(&svc.name)
                .into_iter()
                .filter(|f| svc.columns.iter().any(|c| c.name == *f))
                .collect();
        }
    }

    Ok(Json(trawl_api::ServiceSchemaResponse {
        services,
        cached: true,
        hot_buffer_events: hot_events,
        hot_buffer_bytes: hot_bytes,
    }))
}

fn iso8601(dt: chrono::DateTime<chrono::Utc>) -> String {
    dt.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

/// `since_secs` is request-controlled, so every step here is total. Any
/// window that reaches past the unix epoch saturates there, and the value
/// stays bindable as a postgres `timestamptz` (whose floor is nearer than
/// chrono's).
fn since_from_secs(since_secs: Option<u64>) -> Option<chrono::DateTime<chrono::Utc>> {
    since_secs.map(|s| {
        i64::try_from(s)
            .ok()
            .and_then(chrono::TimeDelta::try_seconds)
            .and_then(|d| chrono::Utc::now().checked_sub_signed(d))
            .map_or(chrono::DateTime::UNIX_EPOCH, |dt| {
                dt.max(chrono::DateTime::UNIX_EPOCH)
            })
    })
}

#[derive(Debug, Deserialize)]
pub struct CatalogFieldsParams {
    pub service: Option<String>,
    pub since_secs: Option<u64>,
    pub limit: Option<i64>,
}

/// `GET /api/v1/schema/fields` — the pinned-field listing with aggregated
/// observation and conflict evidence (`trawl schema fields`).
pub async fn catalog_fields(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    params: Result<Query<CatalogFieldsParams>, QueryRejection>,
) -> Result<Json<trawl_api::CatalogFieldsResponse>, ServerError> {
    if !verified.has_permission(Permission::SchemaRead) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }
    let Query(params) = shaped("query parameters", params)?;

    let limit = params
        .limit
        .unwrap_or(500)
        .clamp(1, crate::store::MAX_PINNED_FIELDS);
    let filter = crate::store::FieldListFilter {
        service: params.service.clone(),
        since: since_from_secs(params.since_secs),
        limit,
        // This listing is the conflict evidence surface, and the second
        // query it costs is keyed on the page `limit` bounds.
        with_conflicts: true,
    };
    let (mut rows, truncated) = state.storage.catalog.list_fields(&filter).await?;
    let (pinned_total, pin_capacity) = state.storage.catalog.pin_stats().await?;

    trawl_api::value::sort_by_display_rank(&mut rows, |r| &r.field);
    let verdicts = degraded_verdicts(
        &state.storage.catalog,
        &rows
            .iter()
            .map(|r| (r.field.clone(), current_pin(&r.duckdb_type)))
            .collect::<Vec<_>>(),
    )
    .await?;
    let fields = rows
        .into_iter()
        .map(|r| trawl_api::CatalogFieldSummary {
            verdict: verdicts.get(&r.field).cloned(),
            name: r.field,
            data_type: r.duckdb_type,
            pinned_from: r.pinned_from,
            pinned_at: iso8601(r.pinned_at),
            service_count: u64::try_from(r.service_count).unwrap_or(0),
            row_count: u64::try_from(r.row_count).unwrap_or(0),
            first_seen: r.first_seen.map(iso8601),
            last_seen: r.last_seen.map(iso8601),
            conflict_count: u64::try_from(r.conflict_count).unwrap_or(0),
            rows_nulled: u64::try_from(r.rows_nulled).unwrap_or(0),
        })
        .collect();

    Ok(Json(trawl_api::CatalogFieldsResponse {
        fields,
        pinned_total: u64::try_from(pinned_total).unwrap_or(0),
        pin_capacity: u64::try_from(pin_capacity).unwrap_or(0),
        truncated,
    }))
}

/// The degraded fields the queries bind, sorted — the incomplete-results
/// notice (ADR-0011). Reads the in-process set the schema-refresh tick
/// maintains, because the query path never touches postgres.
pub(crate) fn degraded_fields_for<'a>(
    state: &AppState,
    queries: impl IntoIterator<Item = &'a trawl_core::ast::Query>,
) -> Vec<String> {
    let snapshot = std::sync::Arc::clone(&state.query.degraded_fields.lock());
    degraded_in(&snapshot, queries)
}

/// The saved text is parsed only when some field is degraded. A notice is
/// not the place to surface a failure: a text that does not parse, or a
/// parse that did not finish before `deadline` or before a cancel reached
/// `wake`, contributes nothing.
async fn degraded_fields_with_saved(
    snapshot: std::sync::Arc<crate::state::DegradedSnapshot>,
    query: &trawl_core::ast::Query,
    saved_dsl: &str,
    deadline: crate::deadline::Deadline,
    wake: &crate::pool::Wake,
) -> Vec<String> {
    if snapshot.fields.is_empty() {
        return Vec::new();
    }
    let saved = if deadline.expired() {
        None
    } else {
        let saved_dsl = saved_dsl.to_owned();
        wake.run(
            deadline,
            tokio::task::spawn_blocking(move || trawl_core::parser::parse_unbuilt(&saved_dsl)),
        )
        .await
        .ok()
        .and_then(Result::ok)
        .and_then(Result::ok)
    };
    degraded_in(&snapshot, std::iter::once(query).chain(saved.as_ref()))
}

/// Fields bound, not fields returned: a `where` on a degraded field that
/// projects it away is exactly the incomplete case
/// ([`trawl_core::field_refs`]).
fn degraded_in<'a>(
    snapshot: &crate::state::DegradedSnapshot,
    queries: impl IntoIterator<Item = &'a trawl_core::ast::Query>,
) -> Vec<String> {
    if snapshot.fields.is_empty() {
        return Vec::new();
    }
    let mut named: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for query in queries {
        named.extend(trawl_core::field_refs::referenced_fields(query));
    }
    named
        .into_iter()
        .filter(|f| snapshot.fields.contains(f))
        .collect()
}

/// The column is `CHECK`-constrained to the canonical spellings, so the
/// fallback is unreachable short of a hand-edited catalog; `VARCHAR` is the
/// one pin under which nothing further can be shelved.
fn current_pin(duckdb_type: &str) -> trawl_core::schema::CanonicalType {
    trawl_core::schema::CanonicalType::from_catalog(duckdb_type)
        .unwrap_or(trawl_core::schema::CanonicalType::Varchar)
}

/// Page-keyed queries, never a join into the listing SQL: the same trap
/// [`crate::store::CatalogStore::list_fields`] documents for its conflict
/// evidence applies here — a grouped subquery over the whole
/// `field_conflict_stats` table has no predicate a planner can push down,
/// and that table's service axis is client-chosen.
async fn degraded_verdicts(
    store: &crate::store::CatalogStore,
    pins: &[(String, trawl_core::schema::CanonicalType)],
) -> Result<std::collections::HashMap<String, trawl_api::DegradedVerdict>, ServerError> {
    use crate::catalog::analyzer;

    let names: Vec<String> = pins.iter().map(|(f, _)| f.clone()).collect();
    let aggregates: Vec<analyzer::ConflictAggregate> = store
        .conflict_aggregates(Some(&names))
        .await?
        .into_iter()
        .filter(analyzer::is_degraded)
        .collect();
    if aggregates.is_empty() {
        return Ok(std::collections::HashMap::new());
    }

    let degraded: Vec<String> = aggregates.iter().map(|a| a.field.clone()).collect();
    let evidence = store.conflict_evidence_for(&degraded).await?;
    Ok(aggregates
        .iter()
        .map(|agg| {
            let current = pins
                .iter()
                .find(|(f, _)| *f == agg.field)
                .map_or(trawl_core::schema::CanonicalType::Varchar, |(_, t)| *t);
            let rows = evidence.get(&agg.field).map_or(&[][..], Vec::as_slice);
            (agg.field.clone(), analyzer::verdict(agg, current, rows))
        })
        .collect())
}

/// The field name travels as a query parameter, never a path segment: a
/// catalog key is any ASCII-folded client JSON key ≤255 bytes — it may
/// contain `/`, `?`, or `%`, which a path segment cannot carry reliably.
#[derive(Debug, Deserialize)]
pub struct CatalogFieldParams {
    pub name: String,
    pub limit: Option<i64>,
    pub after: Option<String>,
}

const DEFAULT_FIELD_SERVICES_LIMIT: i64 = 100;

const MAX_FIELD_SERVICES_LIMIT: i64 = 1000;

/// `GET /api/v1/schema/field?name=` — one field's pin, one page of its
/// per-service observations, and its retained conflict evidence
/// (`trawl schema field`).
pub async fn catalog_field(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    params: Result<Query<CatalogFieldParams>, QueryRejection>,
) -> Result<Json<trawl_api::CatalogFieldResponse>, ServerError> {
    if !verified.has_permission(Permission::SchemaRead) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }
    let Query(params) = shaped("query parameters", params)?;

    // One DuckDB identifier has exactly one catalog spelling (ASCII-lower,
    // folded in ingest::envelope::canonicalize) — fold the lookup the same way.
    let name = params.name.to_ascii_lowercase();

    let limit = params
        .limit
        .unwrap_or(DEFAULT_FIELD_SERVICES_LIMIT)
        .clamp(1, MAX_FIELD_SERVICES_LIMIT);
    let after = match params.after.as_deref() {
        None => None,
        Some(raw) => Some(crate::store::ServiceCursor::decode(raw).ok_or_else(|| {
            ServerError::BadRequest("invalid `after` cursor: pass a `services_cursor` back".into())
        })?),
    };

    let Some(pin) = state.storage.catalog.field_pin(&name).await? else {
        return Err(ServerError::NotFound(format!("field not pinned: {name}")));
    };
    let (services, next) = state
        .storage
        .catalog
        .field_services(&name, after.as_ref(), limit)
        .await?;
    // Conflict evidence needs no cursor: `field_conflicts` is trimmed to
    // MAX_CONFLICTS_PER_FIELD newest rows per field in the writing
    // transaction, so this read is bounded by construction.
    let conflicts = state.storage.catalog.conflicts_for_field(&name).await?;
    // The verdict and the ack that suppresses it are one fact, so they come
    // from one snapshot: read separately, an ack or a repin cutover landing
    // between them publishes a pair that was never true.
    let health = state.storage.catalog.field_health_snapshot(&name).await?;
    let verdict = health
        .aggregate
        .as_ref()
        .filter(|agg| crate::catalog::analyzer::is_degraded(agg))
        .map(|agg| {
            crate::catalog::analyzer::verdict(agg, current_pin(&pin.duckdb_type), &health.evidence)
        });
    let ack = health.ack;

    Ok(Json(trawl_api::CatalogFieldResponse {
        name: pin.field,
        data_type: pin.duckdb_type,
        pinned_from: pin.pinned_from,
        pinned_at: iso8601(pin.pinned_at),
        services: services
            .into_iter()
            .map(|s| trawl_api::CatalogFieldServiceRow {
                service: s.service,
                first_seen: iso8601(s.first_seen),
                last_seen: iso8601(s.last_seen),
                row_count: u64::try_from(s.row_count).unwrap_or(0),
            })
            .collect(),
        services_cursor: next.map(|c| c.encode()),
        conflicts: conflicts
            .into_iter()
            .map(|c| trawl_api::CatalogConflictRow {
                field: name.clone(),
                service: c.service,
                observed_type: c.observed_type,
                expected_type: c.expected_type,
                rows_nulled: u64::try_from(c.rows_nulled).unwrap_or(0),
                samples: c.samples,
                at: iso8601(c.at),
            })
            .collect(),
        verdict,
        ack: ack.map(ack_to_wire),
    }))
}

fn ack_to_wire(ack: crate::store::DegradedAck) -> trawl_api::FieldAck {
    trawl_api::FieldAck {
        acked_at: iso8601(ack.acked_at),
        acked_by: ack.acked_by,
        note: ack.note,
        evidence_through: u64::try_from(ack.evidence_through).unwrap_or(0),
    }
}

#[derive(Debug, Deserialize)]
pub struct FieldAckParams {
    pub name: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct FieldAckRequest {
    #[serde(default)]
    pub note: Option<String>,
}

/// Byte cap for an acknowledgement note, mirroring the migration's CHECK.
const MAX_ACK_NOTE_BYTES: usize = 1024;

/// `POST /api/v1/schema/field/ack?name=` — acknowledge a degraded verdict.
///
/// 409 = the field's evidence does not meet the degraded threshold, so
/// there is no verdict to acknowledge and installing a high-water would
/// swallow the evidence that raises the badge for the first time.
pub async fn ack_degraded_field(
    State(state): State<AppState>,
    verified: Authorized<needs::SchemaWrite>,
    params: Result<Query<FieldAckParams>, QueryRejection>,
    req: Result<Json<FieldAckRequest>, JsonRejection>,
) -> Result<axum::response::Response, ServerError> {
    let Query(params) = shaped("query parameters", params)?;
    let Json(req) = shaped("request body", req)?;
    let name = trawl_core::schema::catalog_key(&params.name);
    let note = req.note.as_deref();
    if let Some(note) = note
        && note.len() > MAX_ACK_NOTE_BYTES
    {
        return Err(ServerError::BadRequest(format!(
            "acknowledgement note is {} bytes; the limit is {MAX_ACK_NOTE_BYTES}",
            note.len()
        )));
    }

    let outcome = state
        .storage
        .catalog
        .acknowledge_degraded_field(&name, &verified.prefix, note)
        .await?;
    match outcome {
        crate::store::AckOutcome::Acked {
            ack,
            created,
            advanced,
        } => {
            // The note is the one thing this record never carries: it is
            // operator prose. Whether they wrote one is the part an audit reader needs.
            //
            // `advanced = false` is the interleaving where this call read
            // less evidence than a concurrent ack had already acknowledged:
            // this event is the only record that the request happened at all.
            tracing::info!(
                event_type = "field_degraded_acked",
                field = %name,
                actor = %verified.name,
                actor_prefix = %verified.prefix,
                evidence_through = ack.evidence_through,
                created,
                advanced,
                note_present = note.is_some(),
                "operator acknowledged a degraded field"
            );
            Ok((StatusCode::OK, Json(ack_to_wire(ack))).into_response())
        }
        crate::store::AckOutcome::Unpinned => {
            Err(ServerError::NotFound(format!("field not pinned: {name}")))
        }
        crate::store::AckOutcome::NotDegraded => Ok((
            StatusCode::CONFLICT,
            Json(trawl_api::ErrorResponse {
                error: trawl_api::ErrorEnvelope::simple(
                    trawl_api::ErrorCode::BadRequest,
                    format!(
                        "{name} is not degraded: the badge needs evidence spanning \
                     {span_hours}h and either {rows} rows shelved or \
                     {episodes} conflict episodes. There is nothing to \
                     acknowledge.",
                        span_hours = crate::catalog::analyzer::DEGRADED_MIN_SPAN.num_hours(),
                        rows = crate::catalog::analyzer::DEGRADED_MIN_ROWS_SHELVED,
                        episodes = crate::catalog::analyzer::DEGRADED_MIN_EPISODES,
                    ),
                ),
            }),
        )
            .into_response()),
    }
}

/// `DELETE /api/v1/schema/field/ack?name=` — withdraw an acknowledgement,
/// re-raising the badge if the evidence still indicts the pin. Idempotent:
/// 204 whether or not a row was there.
pub async fn clear_degraded_field_ack(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    params: Result<Query<FieldAckParams>, QueryRejection>,
) -> Result<axum::response::Response, ServerError> {
    if !verified.has_permission(Permission::SchemaWrite) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }
    let Query(params) = shaped("query parameters", params)?;
    let name = trawl_core::schema::catalog_key(&params.name);
    if state.storage.catalog.field_pin(&name).await?.is_none() {
        return Err(ServerError::NotFound(format!("field not pinned: {name}")));
    }

    if state
        .storage
        .catalog
        .clear_degraded_ack_owned(&name)
        .await?
    {
        tracing::info!(
            event_type = "field_degraded_ack_cleared",
            field = %name,
            reason = "operator",
            actor = %verified.name,
            actor_prefix = %verified.prefix,
            "operator withdrew a degraded-field acknowledgement"
        );
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Debug, Deserialize)]
pub struct CatalogConflictsParams {
    pub field: Option<String>,
    pub service: Option<String>,
    pub since_secs: Option<u64>,
    pub limit: Option<i64>,
}

const MAX_CONFLICT_LIST_LIMIT: i64 = 1000;

/// `GET /api/v1/schema/conflicts` — the schema-health dashboard listing
/// (`trawl schema conflicts --last 7d`).
pub async fn catalog_conflicts(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    params: Result<Query<CatalogConflictsParams>, QueryRejection>,
) -> Result<Json<trawl_api::CatalogConflictsResponse>, ServerError> {
    if !verified.has_permission(Permission::SchemaRead) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }
    let Query(params) = shaped("query parameters", params)?;

    let limit = params
        .limit
        .unwrap_or(100)
        .clamp(1, MAX_CONFLICT_LIST_LIMIT);
    let field = params.field.as_deref().map(str::to_ascii_lowercase);
    let (rows, truncated) = state
        .storage
        .catalog
        .recent_conflicts(
            field.as_deref(),
            params.service.as_deref(),
            since_from_secs(params.since_secs),
            limit,
        )
        .await?;

    Ok(Json(trawl_api::CatalogConflictsResponse {
        conflicts: rows
            .into_iter()
            .map(|c| trawl_api::CatalogConflictRow {
                field: c.field,
                service: c.service,
                observed_type: c.observed_type,
                expected_type: c.expected_type,
                rows_nulled: u64::try_from(c.rows_nulled).unwrap_or(0),
                samples: c.samples,
                at: iso8601(c.at),
            })
            .collect(),
        truncated,
    }))
}

/// `GET /api/v1/schema/values/{field}` — sample distinct values for autocomplete.
pub async fn field_values(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    field: Result<Path<String>, PathRejection>,
    params: Result<Query<FieldValuesParams>, QueryRejection>,
) -> Result<Json<FieldValuesResponse>, ServerError> {
    if !verified.has_permission(Permission::SchemaRead) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }
    let Path(field) = shaped("field name", field)?;
    let Query(params) = shaped("query parameters", params)?;

    let limit = params.limit.unwrap_or(10).min(100);
    let cache_ttl = state.query.schema_cache_ttl_secs;

    // Validate service name if provided (prevent path traversal).
    if let Some(ref svc) = params.service
        && !svc
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        return Err(ServerError::BadRequest(format!(
            "invalid service name: {svc}"
        )));
    }

    let cache_key = match &params.service {
        Some(svc) => format!("{field}:{svc}"),
        None => field.clone(),
    };

    {
        let cache = state.query.field_values_cache.lock().await;
        if let Some(cached) = cache.get(&cache_key)
            && cached.cached_at.elapsed().as_secs() < cache_ttl
        {
            tracing::info!(
                event_type = "field_values_cache_hit",
                user = %verified.name,
                field = %field,
                "field values served from cache"
            );
            return Ok(Json(FieldValuesResponse {
                field: field.clone(),
                values: cached.values.clone(),
                cached: true,
            }));
        }
    }

    let start = std::time::Instant::now();

    // Through the pool: the glob is expanded inside the permit-holding task,
    // so this lane is excluded by the repin cutover like every other
    // parquet reader (ADR-0011).
    let values = state
        .query
        .pool
        .sample_field_values(
            &field,
            params.service.as_deref(),
            limit,
            crate::deadline::Deadline::after(std::time::Duration::from_secs(
                state.query.timeout_secs,
            )),
            crate::pool::WorkContext::key(crate::pool::WorkKind::Sample, verified.id)
                .with_user(&verified.name),
        )
        .await?;

    let duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

    tracing::info!(
        event_type = "field_values_complete",
        user = %verified.name,
        field = %field,
        scoped_service = params.service.as_deref().unwrap_or("*"),
        values_count = values.len(),
        duration_ms,
        "field values sampled"
    );

    state.query.field_values_cache.lock().await.insert(
        cache_key,
        CachedFieldValues {
            values: values.clone(),
            cached_at: std::time::Instant::now(),
        },
    );

    Ok(Json(FieldValuesResponse {
        field,
        values,
        cached: false,
    }))
}

#[derive(Debug, Deserialize)]
pub struct FieldValuesParams {
    pub limit: Option<usize>,
    pub service: Option<String>,
}

/// `GET /api/v1/history` — filter the key's stored query text before pagination.
pub async fn history(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    RawQuery(raw): RawQuery,
) -> Result<Json<HistoryResponse>, ServerError> {
    if !verified.has_permission(Permission::Query) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }

    let key_id = verified.id;

    // RawQuery does not decode. Permission must win over parameter errors, and
    // strict admission must see duplicate/encoding evidence before any decoder.
    let params = crate::history_params::parse(raw.as_deref().unwrap_or_default())?;
    let limit = params.limit.unwrap_or(100).min(1000);
    let offset = params.offset.unwrap_or(0);

    let page = state
        .storage
        .history
        .get_user_history(key_id, limit, offset, params.filter.as_deref())
        .await?;

    Ok(Json(HistoryResponse {
        entries: page
            .entries
            .into_iter()
            .map(history_entry_response)
            .collect(),
        total: page.total,
    }))
}

/// `DELETE /api/v1/history` clears only the authenticated key's history.
pub async fn clear_history(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
) -> Result<Json<ClearHistoryResponse>, ServerError> {
    if !verified.has_permission(Permission::Query) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }

    let deleted = state
        .storage
        .history
        .clear_user_history(verified.id)
        .await?;
    tracing::info!(
        event_type = "history_cleared",
        key_id = verified.id,
        deleted,
        "Query history cleared"
    );
    Ok(Json(ClearHistoryResponse { deleted }))
}

#[derive(Debug, Deserialize)]
pub struct HistoryParams {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

fn history_entry_response(entry: HistoryEntry) -> HistoryEntryResponse {
    HistoryEntryResponse {
        id: entry.id,
        query: entry.query,
        executed_at: entry.executed_at.to_rfc3339(),
        duration_ms: entry.duration_ms,
        row_count: entry.row_count,
        // History never records `Running`; map it to `Error` defensively.
        status: match entry.status {
            RunStatus::Success => QueryStatus::Success,
            RunStatus::Timeout => QueryStatus::Timeout,
            RunStatus::Error | RunStatus::Running => QueryStatus::Error,
        },
    }
}

/// `GET /api/v1/saved` — list user's saved queries.
pub async fn list_saved(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
) -> Result<Json<ListSavedResponse>, ServerError> {
    if !verified.has_permission(Permission::SavedQuery) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }

    let key_id = verified.id;

    let details = state.storage.saved.list_with_details(key_id).await?;

    let responses: Vec<SavedQueryResponse> = details
        .into_iter()
        .map(|d| {
            let schedule = d.schedule.map(schedule_response_from_stats);
            SavedQueryResponse {
                id: d.saved.id,
                name: d.saved.name,
                query: d.saved.query,
                created_at: d.saved.created_at.to_rfc3339(),
                updated_at: d.saved.updated_at.to_rfc3339(),
                schedule,
            }
        })
        .collect();

    Ok(Json(ListSavedResponse { queries: responses }))
}

/// `POST /api/v1/saved` — create a new saved query.
pub async fn create_saved(
    State(state): State<AppState>,
    verified: Authorized<needs::SavedQuery>,
    req: Result<Json<CreateSavedRequest>, JsonRejection>,
) -> Result<Json<SavedQueryResponse>, ServerError> {
    let Json(req) = shaped("request body", req)?;

    let key_id = verified.id;

    let deadline =
        crate::deadline::Deadline::after(std::time::Duration::from_secs(state.query.timeout_secs));
    crate::admission::admit(&state.query.pool, &req.query, deadline, None).await?;

    let saved = state
        .storage
        .saved
        .create(key_id, &req.name, &req.query)
        .await?;

    Ok(Json(saved_query_response(saved)))
}

/// `PUT /api/v1/saved/{id}` — update an existing saved query.
pub async fn update_saved(
    State(state): State<AppState>,
    verified: Authorized<needs::SavedQuery>,
    id: Result<Path<i64>, PathRejection>,
    req: Result<Json<UpdateSavedRequest>, JsonRejection>,
) -> Result<Json<SavedQueryResponse>, ServerError> {
    let Path(id) = shaped("saved query id", id)?;
    let Json(req) = shaped("request body", req)?;

    let key_id = verified.id;

    // Same door as create, and before the store call, so a refused update
    // leaves the stored row exactly as it was.
    let deadline =
        crate::deadline::Deadline::after(std::time::Duration::from_secs(state.query.timeout_secs));
    crate::admission::admit(&state.query.pool, &req.query, deadline, None).await?;

    // NotFound → 404, InvalidName → 400, DuplicateName → 409, and a new
    // DSL that contradicts the schedule's window → 400 naming both sides.
    // The store proves that under the saved-query row lock, so the pair a
    // window forbids cannot be assembled by two requests racing.
    let saved = state
        .storage
        .saved
        .update_checked(id, key_id, &req.query, req.name.as_deref())
        .await?;

    Ok(Json(saved_query_response(saved)))
}

/// `DELETE /api/v1/saved/{id}` — delete a saved query.
pub async fn delete_saved(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    id: Result<Path<i64>, PathRejection>,
) -> Result<Json<DeleteSavedResponse>, ServerError> {
    if !verified.has_permission(Permission::SavedQuery) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }
    let Path(id) = shaped("saved query id", id)?;

    let key_id = verified.id;

    let run_paths = state.storage.saved.delete(id, key_id).await?;

    cleanup_run_parquet_files(&state, &run_paths);

    Ok(Json(DeleteSavedResponse { deleted: true }))
}

fn saved_query_response(saved: SavedQuery) -> SavedQueryResponse {
    SavedQueryResponse {
        id: saved.id,
        name: saved.name,
        query: saved.query,
        created_at: saved.created_at.to_rfc3339(),
        updated_at: saved.updated_at.to_rfc3339(),
        schedule: None,
    }
}

/// The run count and `exhausted` both read off this one `schedule`, so
/// they can never pair a count with some other read's cap.
fn build_schedule_response(schedule: &Schedule, latest_run: Option<ReportRun>) -> ScheduleResponse {
    ScheduleResponse {
        id: schedule.id,
        saved_query_id: schedule.saved_query_id,
        interval: format_interval(schedule.interval_secs),
        interval_secs: schedule.interval_secs,
        max_runs: schedule.max_runs,
        enabled: schedule.enabled,
        created_at: schedule.created_at.to_rfc3339(),
        updated_at: schedule.updated_at.to_rfc3339(),
        last_run: latest_run.map(report_run_summary),
        total_runs: schedule.runs_claimed,
        exhausted: schedule.exhausted(),
        window: schedule.window.map(|w| w.to_string()),
        // The lag pair rides the window, not the stored number: query mode
        // stores a zero that changes no answer, and reporting "0s" there
        // would read as an allowance in force. `ensure_lag_has_window`
        // refuses the other combination, so the stored zero is the only
        // thing being hidden.
        lag: schedule.window.map(|_| format_interval(schedule.lag_secs)),
        lag_secs: schedule.window.map(|_| schedule.lag_secs),
        // Only a tiling schedule has a watermark that claims anything. The
        // store KEEPS the stored value across a mode change on purpose
        // (ADR-0018 ruling 14), which is why reading the column alone would report
        // a fixed-window or query-mode schedule as covered.
        covered_through: match schedule.window {
            Some(ScheduleWindow::SinceLast) => schedule.covered_through.map(format_window_bound),
            Some(ScheduleWindow::Fixed { .. }) | None => None,
        },
        next_fire_at: format_window_bound(schedule.next_fire_at),
    }
}

fn schedule_response_from_stats(stats: ScheduleWithStats) -> ScheduleResponse {
    build_schedule_response(&stats.schedule, stats.latest_run)
}

fn report_run_summary(run: ReportRun) -> ReportRunSummary {
    ReportRunSummary {
        id: run.id,
        query: run.query,
        status: run.status.as_str().to_string(),
        started_at: run.started_at.to_rfc3339(),
        finished_at: run.finished_at.map(|t| t.to_rfc3339()),
        duration_ms: run.duration_ms,
        row_count: run.row_count,
        error_message: run.error_message,
        result_path: run.result_path,
        window_start: run.window_start.map(format_window_bound),
        window_end: run.window_end.map(format_window_bound),
        window_truncated: run.window_truncated,
        window_kind: run.window_kind.map(|k| k.as_str().to_owned()),
        origin: run.origin.map(|o| o.as_str().to_owned()),
    }
}

/// `requires_force` asks the same force decision the two live gates ask
/// (`repin::force_refusal`), over this row's own persisted numbers. It is
/// absent until the scan has recorded its plan (`planned_at`): a claimed
/// job's counts are zeros that mean "not measured yet".
fn repin_job_to_wire(job: crate::store::RepinJob) -> trawl_api::RepinJobResponse {
    let clamp = |v: i64| u64::try_from(v).unwrap_or(0);
    // The pin the job targets, and the dialect it asserted. An unparseable
    // spelling is corruption in a CHECK-constrained column; VARCHAR is the
    // pin under which the ambiguity gate cannot fire, so the row reports
    // loss only rather than inventing a severity verdict.
    let to = trawl_core::schema::CanonicalType::from_catalog(&job.to_type)
        .unwrap_or(trawl_core::schema::CanonicalType::Varchar);
    let dialect = job
        .dialect
        .as_deref()
        .and_then(trawl_core::severity::Dialect::from_token);
    // The rewrite's own tally supersedes the plan's projection once it has
    // written anything — the same rule the CLI's refusal text uses.
    let nulled = if job.rows_nulled > 0 {
        clamp(job.rows_nulled)
    } else {
        clamp(job.projected_nulls)
    };
    let requires_force_reason = job.force_terms.and_then(|terms| {
        crate::repin::force_refusal(to, dialect, nulled, clamp(job.ambiguous_numerals), terms)
    });
    trawl_api::RepinJobResponse {
        requires_force: job.force_terms.map(|_| requires_force_reason.is_some()),
        requires_force_reason,
        id: job.id,
        field: job.field,
        from_type: job.from_type,
        to_type: job.to_type,
        dry_run: job.dry_run,
        force: job.force,
        status: job.status.as_str().to_owned(),
        requested_by: job.requested_by,
        started_at: iso8601(job.started_at),
        finished_at: job.finished_at.map(iso8601),
        error: job.error,
        files_total: clamp(job.files_total),
        rows_carrying: clamp(job.rows_carrying),
        projected_nulls: clamp(job.projected_nulls),
        resurrectable: clamp(job.resurrectable),
        affected_bytes: clamp(job.affected_bytes),
        files_done: clamp(job.files_done),
        rows_rewritten: clamp(job.rows_rewritten),
        rows_nulled: clamp(job.rows_nulled),
        rows_resurrected: clamp(job.rows_resurrected),
        dialect: job.dialect,
        ambiguous_numerals: clamp(job.ambiguous_numerals),
        unmapped_samples: job.unmapped_samples,
        // Presence is the verdict, so both halves must be present: a row
        // with an observation instant and no service is a partially-written
        // job row, not a liveness warning.
        liveness: job
            .field_last_seen
            .zip(job.field_last_service)
            .map(|(last_seen, service)| trawl_api::RepinLiveness {
                last_seen: iso8601(last_seen),
                service,
            }),
        // Persisted facts only (#109). A `running` row carrying these is a
        // cancel in flight; the status route computes no "cancelling"
        // pseudo-status over them.
        cancel_requested_at: job.cancel_requested_at.map(iso8601),
        cancelled_by: job.cancelled_by,
        // Both pairs ride the row unchanged: what the request stated, and
        // what the plan resolved. Absent requested bounds select defaults;
        // accepted bounds exist only for recorded forced plans.
        max_nulled_rows: job.max_nulled_rows.map(clamp),
        max_ambiguous_rows: job.max_ambiguous_rows.map(clamp),
        accepted_max_nulled_rows: job.accepted_max_nulled_rows.map(clamp),
        accepted_max_ambiguous_rows: job.accepted_max_ambiguous_rows.map(clamp),
    }
}

/// `POST /api/v1/schema/repin` — trigger a repin (ADR-0011).
pub async fn schema_repin(
    State(state): State<AppState>,
    verified: Authorized<needs::SchemaWrite>,
    req: Result<Json<trawl_api::RepinRequest>, JsonRejection>,
) -> Result<axum::response::Response, ServerError> {
    let Json(req) = shaped("request body", req)?;
    let Some(engine) = state.repin.as_ref() else {
        return Err(ServerError::ServiceUnavailable(
            "repin requires an ingest-enabled node (this node does not own \
             the data root)"
                .into(),
        ));
    };

    // Both halves of the caller's identity go on the job row: the display
    // name an operator reads, and the key's row id, which is what a status
    // read by id with `schema_write` alone is matched against.
    let requester = crate::store::Requester::new(verified.id, verified.name.clone());
    let outcome = engine
        .start_within(
            std::time::Duration::from_secs(state.query.timeout_secs),
            &req.field,
            &req.to,
            req.dialect.as_deref(),
            req.dry_run,
            req.force,
            // Stated or not: an absent ceiling means "derive one from this
            // job's own scan", and a ceiling without force is a 400 the
            // engine raises.
            crate::repin::ceiling::RequestedCeilings {
                max_nulled: req.max_nulled_rows,
                max_ambiguous: req.max_ambiguous_rows,
            },
            Some(&requester),
        )
        .await?;
    let outcome = match outcome {
        crate::repin::BoundedStart::Decided(outcome) => outcome,
        // Past the bound the job is still running, whatever it was asked
        // to do, so it answers as a started one.
        crate::repin::BoundedStart::Running(job) => crate::repin::StartOutcome::Started(job),
    };
    let (status, job) = match outcome {
        crate::repin::StartOutcome::Started(job) => (StatusCode::ACCEPTED, job),
        crate::repin::StartOutcome::Refused(job) => (StatusCode::CONFLICT, job),
        // A job an operator cancelled while this request's own ladder was
        // still running it (#109) answers 200 with the terminal row, the
        // same as a dry-run report: never a fourth status code, because 409
        // already means refused-needs-force to a body-sniffing client, and
        // `job.status` says `cancelled` plainly.
        crate::repin::StartOutcome::DryRun(job) | crate::repin::StartOutcome::Cancelled(job) => {
            (StatusCode::OK, job)
        }
    };
    Ok((
        status,
        Json(trawl_api::RepinResponse {
            job: repin_job_to_wire(job),
        }),
    )
        .into_response())
}

/// `POST /api/v1/schema/repin/cancel` — ask the running repin to stop
/// (#109). `SchemaWrite`-gated like the trigger, and 503 on a query-only
/// node for the same reason: a node that owns nothing under the data root
/// runs no job to cancel.
///
/// No request body: there is at most one running job, and naming it would
/// invite an operator to cancel a job that already ended and a newer one
/// took the slot.
///
/// The verdict comes from the registry's own lock through
/// [`crate::repin::CancelVerdict::wire`] — 202 accepted, 409 past the point
/// of no return, 404 nothing running — and this handler renders it without
/// deciding anything. The job row rides along for the two verdicts that
/// name a job, so an operator sees what was cancelled without a second
/// round trip; a store that cannot serve that row costs the body, never the
/// verdict, because the cancel has already taken effect in process.
pub async fn schema_repin_cancel(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
) -> Result<axum::response::Response, ServerError> {
    if !verified.has_permission(Permission::SchemaWrite) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }
    let Some(engine) = state.repin.as_ref() else {
        return Err(ServerError::ServiceUnavailable(
            "repin requires an ingest-enabled node (this node does not own \
             the data root)"
                .into(),
        ));
    };

    // Both halves of the caller's identity go to the engine: the display
    // name the job row records, and the key prefix the audit events name
    // beside it. A name is operator-chosen and can be reused or changed;
    // the prefix is what says which credential actually asked.
    let actor = crate::repin::CancelActor::new(verified.name.clone(), verified.prefix.clone());
    let verdict = engine.cancel(&actor);
    let (status, outcome, detail) = verdict.wire();
    let mut job = None;
    if let Some(job_id) = verdict.job_id() {
        match state.storage.repin.get(job_id).await {
            Ok(row) => job = row.map(repin_job_to_wire),
            Err(e) => tracing::error!(
                event_type = "repin_store_error",
                job_id,
                error_class = e.class(),
                "failed to read the repin job row for a cancel receipt; the \
                 verdict is unaffected"
            ),
        }
    }
    Ok((
        status,
        Json(trawl_api::RepinCancelResponse {
            outcome,
            detail: detail.to_owned(),
            job,
        }),
    )
        .into_response())
}

#[derive(Debug, Deserialize)]
pub struct RepinStatusParams {
    pub id: Option<i64>,
}

/// `GET /api/v1/schema/repin/status` — the running job if any, else the
/// newest job of any status, or with `?id=` that one job.
pub async fn schema_repin_status(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    params: Result<Query<RepinStatusParams>, QueryRejection>,
) -> Result<Json<trawl_api::RepinStatusResponse>, ServerError> {
    let reads = verified.has_permission(Permission::SchemaRead);
    if !reads && !verified.has_permission(Permission::SchemaWrite) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }
    let Query(params) =
        params.map_err(|_| ServerError::BadRequest("invalid repin status parameters".into()))?;
    let Some(id) = params.id else {
        if !reads {
            return Err(ServerError::Forbidden("insufficient permissions".into()));
        }
        let job = state.storage.repin.latest().await?;
        return Ok(Json(trawl_api::RepinStatusResponse {
            job: job.map(repin_job_to_wire),
        }));
    };
    let job = if reads {
        state.storage.repin.get(id).await?
    } else {
        state
            .storage
            .repin
            .get_requested_by(id, verified.id)
            .await?
    };
    let job = job.ok_or_else(|| ServerError::NotFound("repin job not found".into()))?;
    Ok(Json(trawl_api::RepinStatusResponse {
        job: Some(repin_job_to_wire(job)),
    }))
}

/// `POST /api/v1/schema/gc-pins` reclaims pin slots held by fields
/// nothing writes any more.
///
/// The engine's `run` spawns its own task internally (a disconnect must
/// not split the postgres commit from the cache eviction), so this handler
/// awaits it directly rather than spawning a second time.
pub async fn schema_gc_pins(
    State(state): State<AppState>,
    verified: Authorized<needs::SchemaWrite>,
    req: Result<Json<trawl_api::GcPinsRequest>, JsonRejection>,
) -> Result<Json<trawl_api::GcPinsResponse>, ServerError> {
    let Json(req) = shaped("request body", req)?;
    let Some(engine) = state.gc.as_ref() else {
        return Err(ServerError::ServiceUnavailable(
            "pin gc requires an ingest-enabled node (this node does not own \
             the data root)"
                .into(),
        ));
    };

    let actor = crate::catalog::gc::GcActor {
        name: Some(verified.name.clone()),
        key_prefix: Some(verified.prefix.clone()),
    };
    let report = engine
        .run(
            req.older_than_secs.map(std::time::Duration::from_secs),
            req.dry_run,
            actor,
        )
        .await?;
    Ok(Json(report))
}

fn cleanup_run_parquet_files(state: &AppState, relative_paths: &[String]) {
    if relative_paths.is_empty() {
        return;
    }
    let base = state.query.pool.base_dir();
    for path in relative_paths {
        crate::scheduler::remove_result_file(base, path);
    }
}

/// The `from saved` stage `query` opens with, and the byte offset in its
/// text where that stage ends, which is where the stages it feeds begin.
/// `None` for a query that reads the corpus.
pub(crate) fn saved_source(
    query: &trawl_core::ast::Query,
) -> Option<(&trawl_core::ast::FromSavedStage, usize)> {
    let from_saved = query.from_saved_stage()?;
    Some((from_saved, query.pipeline[0].span.end))
}

/// `query` is what [`crate::admission::admit`] parsed from `dsl`, so
/// nothing here parses the text again; its spans index into `dsl`.
pub(crate) async fn try_resolve_from_saved(
    state: &AppState,
    verified: &VerifiedKey,
    dsl: &str,
    query: &trawl_core::ast::Query,
) -> Result<Option<crate::from_saved::ResolvedFromSaved>, ServerError> {
    let Some((from_saved, stage_span_end)) = saved_source(query) else {
        return Ok(None);
    };

    let key_id = verified.id;

    let resolved = crate::from_saved::resolve(
        from_saved,
        dsl,
        stage_span_end,
        &state.storage.saved,
        &state.storage.schedule,
        key_id,
        state.query.pool.base_dir(),
    )
    .await?;

    Ok(Some(resolved))
}

const DURATION_GRAMMAR: &str = "a duration: a number and one of s, m, h, d, w";

/// The duration half carries the same 60s floor as the schedule interval,
/// which is why the store's own message about a short one says "interval".
const WINDOW_GRAMMAR: &str = "\"since_last\" or a duration: a number and one of s, m, h, d, w";

fn invalid_duration(field: &str, grammar: &str, e: &crate::store::StoreError) -> ServerError {
    ServerError::BadRequest(format!("invalid {field}: {e}; {field} takes {grammar}"))
}

/// `PUT /api/v1/saved/{id}/schedule` — create or update a schedule.
pub async fn set_schedule(
    State(state): State<AppState>,
    verified: Authorized<needs::SavedQuery>,
    saved_id: Result<Path<i64>, PathRejection>,
    req: Result<Json<SetScheduleRequest>, JsonRejection>,
) -> Result<Json<ScheduleResponse>, ServerError> {
    let Path(saved_id) = shaped("saved query id", saved_id)?;
    let Json(req) = shaped("request body", req)?;

    let key_id = verified.id;
    let interval_secs = parse_interval(&req.interval)
        .map_err(|e| ServerError::BadRequest(format!("invalid interval: {e}")))?;
    // The store keeps the cap as a signed 64-bit count and clamps anything
    // larger. Refuse it here, so a caller never gets back a smaller cap than
    // the one it sent.
    if req.max_runs.is_some_and(|cap| i64::try_from(cap).is_err()) {
        return Err(ServerError::BadRequest(format!(
            "invalid max_runs: at most {}",
            i64::MAX
        )));
    }
    let window = req
        .window
        .as_deref()
        .map(ScheduleWindow::parse)
        .transpose()
        .map_err(|e| invalid_duration("window", WINDOW_GRAMMAR, &e))?;
    // Absent is zero, the default ADR-0018 ruling 6 gives a window. A lag
    // WITHOUT a window is not decided here: the store owns that refusal,
    // because it is the one place both write doors pass through.
    let lag_secs = req
        .lag
        .as_deref()
        .map(parse_duration_secs)
        .transpose()
        .map_err(|e| invalid_duration("lag", DURATION_GRAMMAR, &e))?
        .unwrap_or(0);

    // Ownership, the window/query compatibility rule and the create-or-
    // update decision all happen in one transaction under the saved-query
    // row lock. Reading the pair out here and deciding afterwards would let
    // a concurrent PUT of the saved query's DSL slip a time clause under
    // the window between the check and the write.
    let schedule = state
        .storage
        .schedule
        .set_schedule_checked(
            saved_id,
            key_id,
            interval_secs,
            req.max_runs,
            req.enabled,
            window,
            lag_secs,
            chrono::Utc::now(),
        )
        .await?;

    // An updated schedule can already have runs; fetch the latest one the
    // response carries. The response reads that same row whole, never the
    // write's cap beside the re-read's count: a claim between the two
    // statements would pair them into a state no row held. A schedule gone
    // by then answers with what was written.
    let (schedule, latest_run) = state
        .storage
        .schedule
        .get_schedule_with_stats(saved_id, key_id)
        .await?
        .unwrap_or((schedule, None));

    Ok(Json(build_schedule_response(&schedule, latest_run)))
}

/// `GET /api/v1/saved/{id}/schedule` — get schedule for a saved query.
pub async fn get_schedule(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    saved_id: Result<Path<i64>, PathRejection>,
) -> Result<Json<ScheduleResponse>, ServerError> {
    if !verified.has_permission(Permission::SavedQuery) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }
    let Path(saved_id) = shaped("saved query id", saved_id)?;

    let key_id = verified.id;

    let (schedule, latest_run) = state
        .storage
        .schedule
        .get_schedule_with_stats(saved_id, key_id)
        .await?
        .ok_or_else(|| ServerError::NotFound("no schedule for this saved query".into()))?;

    Ok(Json(build_schedule_response(&schedule, latest_run)))
}

/// `DELETE /api/v1/saved/{id}/schedule` — delete a schedule.
pub async fn delete_schedule(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    saved_id: Result<Path<i64>, PathRejection>,
) -> Result<Json<DeleteScheduleResponse>, ServerError> {
    if !verified.has_permission(Permission::SavedQuery) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }
    let Path(saved_id) = shaped("saved query id", saved_id)?;

    let key_id = verified.id;

    let run_paths = state
        .storage
        .schedule
        .delete_schedule(saved_id, key_id)
        .await?;

    cleanup_run_parquet_files(&state, &run_paths);

    Ok(Json(DeleteScheduleResponse { deleted: true }))
}

#[derive(Debug, Deserialize)]
pub struct ListRunsParams {
    #[serde(default = "default_runs_limit")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
}

fn default_runs_limit() -> usize {
    20
}

/// `GET /api/v1/saved/{id}/runs` — list report runs for a saved query.
pub async fn list_report_runs(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    saved_id: Result<Path<i64>, PathRejection>,
    params: Result<Query<ListRunsParams>, QueryRejection>,
) -> Result<Json<ListReportRunsResponse>, ServerError> {
    if !verified.has_permission(Permission::SavedQuery) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }
    let Path(saved_id) = shaped("saved query id", saved_id)?;
    let Query(params) = shaped("query parameters", params)?;

    let key_id = verified.id;

    let runs = state
        .storage
        .schedule
        .list_runs(saved_id, key_id, params.limit, params.offset)
        .await?;

    let total = state
        .storage
        .schedule
        .count_runs_for_saved_query(saved_id, key_id)
        .await?;

    Ok(Json(ListReportRunsResponse {
        runs: runs.into_iter().map(report_run_summary).collect(),
        total,
    }))
}

#[derive(Debug, Deserialize)]
pub struct ListAllRunsParams {
    #[serde(default = "default_runs_limit")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
    pub sort: Option<String>,
    pub dir: Option<String>,
}

/// `GET /api/v1/runs` — list report runs across all saved queries for the user.
pub async fn list_all_runs(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    params: Result<Query<ListAllRunsParams>, QueryRejection>,
) -> Result<Json<ListAllRunsResponse>, ServerError> {
    if !verified.has_permission(Permission::SavedQuery) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }

    let Query(params) =
        params.map_err(|_| ServerError::BadRequest("invalid runs parameters".into()))?;

    let sort = params
        .sort
        .as_deref()
        .map(str::parse::<RunsSortKey>)
        .transpose()
        .map_err(|_| ServerError::BadRequest("invalid runs sort key".into()))?
        .unwrap_or_default();
    let dir = params
        .dir
        .as_deref()
        .map(str::parse::<RunsSortDir>)
        .transpose()
        .map_err(|_| ServerError::BadRequest("invalid runs sort direction".into()))?
        .unwrap_or_default();

    let key_id = verified.id;

    let runs = state
        .storage
        .schedule
        .list_all_runs(key_id, params.limit, params.offset, sort, dir)
        .await?;

    let total = state.storage.schedule.count_all_runs(key_id).await?;

    Ok(Json(ListAllRunsResponse {
        runs: runs
            .into_iter()
            .map(|(run, net_name)| GlobalRunSummary {
                net_id: run.saved_query_id,
                net_name,
                run: report_run_summary(run),
            })
            .collect(),
        total,
    }))
}

/// `GET /api/v1/runs/stats` — aggregate run statistics for the authenticated user.
pub async fn runs_stats(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
) -> Result<Json<RunsStatsResponse>, ServerError> {
    if !verified.has_permission(Permission::SavedQuery) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }

    let key_id = verified.id;

    let (total_runs, success_count, error_count, timeout_count, avg_duration_ms) =
        state.storage.schedule.runs_stats(key_id).await?;

    Ok(Json(RunsStatsResponse {
        total_runs,
        success_count,
        error_count,
        timeout_count,
        avg_duration_ms,
    }))
}

/// `POST /api/v1/saved/{id}/run` — run a saved query's schedule now.
///
/// The claim and the execution are one spawned task, which owns the lease
/// from before the claim's first statement. A request dropped while the claim
/// commits therefore cannot leave a `running` row nothing executes.
pub async fn trigger_run(
    State(state): State<AppState>,
    verified: Authorized<needs::RunSavedQuery>,
    saved_id: Result<Path<i64>, PathRejection>,
) -> Result<Json<ReportRunSummary>, ServerError> {
    let Path(saved_id) = shaped("saved query id", saved_id)?;

    let key_id = verified.id;

    // A missing or foreign id is a 404 even while the corpus is unsettled.
    // The lookup locks and claims nothing; the claim below checks
    // ownership again under its own lock, and that check decides.
    if state.storage.saved.get(saved_id, key_id).await?.is_none() {
        return Err(StoreError::NotFound {
            id: saved_id,
            resource: "saved query",
        }
        .into());
    }

    // Before the claim, which creates the run and plans its window from
    // the schedule: a run the corpus gate refused afterwards would record a
    // failure for a window nothing read (ADR-0041). No guard is held; the
    // run's own read is still the door.
    if let Some(reason) = state.query.pool.publication().unsettled() {
        return Err(ServerError::CorpusRecovering(reason));
    }

    // Before the claim too, and never waiting: a refusal has nothing to
    // undo. The estimate covers the claim and the run's timeout, and only
    // shapes a later refusal's advisory Retry-After.
    let lease = state
        .query
        .pool
        .try_lease_manual(
            key_id,
            CLAIM_WAIT + std::time::Duration::from_secs(state.query.timeout_secs),
        )
        .map_err(|busy| ServerError::ManualRunBusy {
            retry_after_secs: busy.retry_after_secs,
        })?;

    let (answer, answered) = tokio::sync::oneshot::channel();
    let schedule_store = state.storage.schedule.clone();
    let pool = state.query.pool.clone();
    let max_catchup_intervals = state.query.max_catchup_intervals;
    let timeout_secs = state.query.timeout_secs;

    // The task owns the lease before its first await. Every answer that
    // starts no run drops the lease first and answers second, so a client
    // that hears a refusal can press again at once.
    tokio::spawn(async move {
        // One transaction: lock the saved query and read the DSL from it,
        // lock the schedule, refuse or plan, claim the run. The ownership
        // check rides the first lock, so this handler takes no snapshot of
        // its own: what the run executes is what the claim recorded.
        let claim = schedule_store
            .claim_manual_run(saved_id, key_id, max_catchup_intervals, &lease)
            .await;
        let claimed = match started_manual_run(claim) {
            Ok(claimed) => claimed,
            Err(refusal) => {
                drop(lease);
                let _ = answer.send(Err(refusal));
                return;
            }
        };

        if let Some(window) = &claimed.window {
            crate::scheduler::note_truncated_window(claimed.schedule_id, claimed.run_id, window);
        }

        let summary = ReportRunSummary {
            id: claimed.run_id,
            query: claimed.query.clone(),
            status: RunStatus::Running.as_str().to_string(),
            started_at: claimed.claimed_at.to_rfc3339(),
            finished_at: None,
            duration_ms: None,
            row_count: None,
            error_message: None,
            result_path: None,
            window_start: claimed.window.map(|w| format_window_bound(w.start)),
            window_end: claimed.window.map(|w| format_window_bound(w.end)),
            window_truncated: claimed.window.map(|w| w.truncated),
            window_kind: claimed.window.map(|w| w.kind.as_str().to_owned()),
            origin: Some(RunOrigin::Manual.as_str().to_owned()),
        };
        // A request that went away no longer hears the answer. The claim
        // committed all the same, so the run executes regardless.
        let _ = answer.send(Ok(summary));

        execute_scheduled_query(
            schedule_store,
            pool,
            claimed.run_id,
            &claimed.query,
            &claimed.query_name,
            timeout_secs,
            lease,
        )
        .await;
    });

    match answered.await {
        Ok(summary) => summary.map(Json),
        Err(_) => Err(ServerError::Internal(
            "the manual run's claim ended without an answer".into(),
        )),
    }
}

fn started_manual_run(
    claim: Result<ManualRunClaim, crate::store::DueClaimError>,
) -> Result<ClaimedManualRun, ServerError> {
    match claim? {
        ManualRunClaim::Started(claimed) => Ok(claimed),
        ManualRunClaim::NoSchedule => Err(ServerError::BadRequest(
            "attach a schedule before triggering a run".into(),
        )),
        ManualRunClaim::AlreadyRunning => Err(ServerError::Conflict(
            "a run is already in progress for this net".into(),
        )),
        ManualRunClaim::MaxRunsReached => Err(ServerError::Conflict(
            "max runs reached for this net".into(),
        )),
        ManualRunClaim::EmptyWindow {
            covered_through,
            window_end,
        } => Err(ServerError::Conflict(format!(
            "nothing new to read: coverage already reaches {}, and a run now would end at {}",
            format_window_bound(covered_through),
            format_window_bound(window_end),
        ))),
    }
}

/// `GET /api/v1/saved/{id}/runs/{run_id}` — get a single report run with result data.
///
/// A run that names a file answers from that file or not at all. Falling back
/// to the blob here would answer 200 with `result: null` beside a non-zero
/// `row_count`.
pub async fn get_report_run(
    State(state): State<AppState>,
    verified: Authorized<needs::RunSavedQuery>,
    path: Result<Path<(i64, i64)>, PathRejection>,
) -> Result<Json<ReportRunResponse>, ServerError> {
    let Path((saved_id, run_id)) = shaped("saved query and run ids", path)?;

    let key_id = verified.id;

    let run = state
        .storage
        .schedule
        .get_run(run_id, key_id)
        .await?
        .ok_or_else(|| ServerError::NotFound("report run not found or unauthorized".into()))?;

    if run.saved_query_id != saved_id {
        return Err(ServerError::NotFound(
            "report run not found for this saved query".into(),
        ));
    }

    let result_blob = state
        .storage
        .schedule
        .get_run_result(run_id, key_id)
        .await?;

    let result = if let Some(ref result_path) = run.result_path {
        let max_rows = state.query.pool.max_result_rows();
        let file = crate::from_saved::run_file(run.id, state.query.pool.base_dir(), result_path);
        if let Some(conflict) =
            crate::from_saved::unavailable_run_conflict(std::slice::from_ref(&file))
        {
            return Err(conflict);
        }

        let path = file.absolute_path.clone();
        // A clone of the query database, so the read spills into the query
        // scratch directory under the query budgets (ADR-0056).
        let pool = state.query.pool.clone();
        match tokio::task::spawn_blocking(move || {
            let executor = pool.detached_executor()?;
            executor.read_parquet_to_result(&path, max_rows)
        })
        .await
        {
            Ok(Ok(qr)) => Some(qr),
            // An exhausted query budget is a query's failure, and what is
            // logged about one names no path, size or `DuckDB` text
            // (ADR-0056): the classification alone.
            Ok(Err(e @ trawl_engine::error::EngineError::ResourceExhausted)) => {
                let err = ServerError::Engine(e);
                tracing::warn!(
                    event_type = "report_run_parquet_read_failed",
                    run_id,
                    error_class = err.error_class(),
                    cause_kind = err.cause_kind().as_str(),
                    "the stored parquet result could not be read within the query budgets"
                );
                return Err(
                    crate::from_saved::unavailable_run_conflict(std::slice::from_ref(&file))
                        .unwrap_or(err),
                );
            }
            Ok(Err(e)) => {
                // The engine's text can quote DuckDB's diagnostics.
                tracing::debug!(
                    event_type = "report_run_parquet_read_failed",
                    run_id,
                    path = %file.absolute_path.display(),
                    error = %e,
                    "failed to read the stored parquet result"
                );
                let err = ServerError::Engine(e);
                tracing::warn!(
                    event_type = "report_run_parquet_read_failed",
                    run_id,
                    path = %file.absolute_path.display(),
                    error_class = err.error_class(),
                    cause_kind = err.cause_kind().as_str(),
                    "failed to read the stored parquet result"
                );
                // The delete may have landed while the read was in flight.
                return Err(
                    crate::from_saved::unavailable_run_conflict(std::slice::from_ref(&file))
                        .unwrap_or(err),
                );
            }
            Err(e) => {
                // A panicked read's JoinError quotes the panic's payload in
                // its Display, so it is classified, never formatted
                // (ADR-0040).
                let err = ServerError::from_join("report run parquet read", e);
                tracing::error!(
                    event_type = "report_run_parquet_task_failed",
                    run_id,
                    error_class = err.error_class(),
                    "the parquet read task did not complete"
                );
                return Err(err);
            }
        }
    } else {
        crate::scheduler::decode_result_blob(result_blob)
    };

    Ok(Json(ReportRunResponse {
        summary: report_run_summary(run),
        result,
    }))
}

/// `POST /api/v1/export` — export query results as CSV, JSON, or Parquet.
#[allow(clippy::too_many_lines)]
pub async fn export(
    State(state): State<AppState>,
    verified: Authorized<needs::Export>,
    Extension(request_id): Extension<RequestId>,
    params: Result<Query<ExportParams>, QueryRejection>,
    req: Result<Json<ExportRequest>, JsonRejection>,
) -> Result<impl IntoResponse, ServerError> {
    let Query(params) = shaped("query parameters", params)?;
    let Json(req) = shaped("request body", req)?;

    let deadline =
        crate::deadline::Deadline::after(std::time::Duration::from_secs(state.query.timeout_secs));

    // Counted once shaped, as a query is: refused DSL counts, a malformed
    // request does not.
    state
        .total_queries
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let format = params.format.unwrap_or(trawl_api::ExportFormat::Csv);

    let max_export_rows = state.query.max_export_rows;
    let limit = req.limit.unwrap_or(max_export_rows).min(max_export_rows);

    // `pending` puts the id in the pool's registry before the entry below opens,
    // so a cancel by id reaches the export while it still waits to start.
    // Declared first, it drops after `tracked`.
    let query_id = state.query.pool.allocate_query_id();
    crate::transport::failure::record_query_id(query_id);
    let pending = state
        .query
        .pool
        .pend(query_id, crate::pool::WorkKind::Export);
    // The entry, left in place when the transport drops this future at an
    // await, closes as abandoned. A mutex, not a cell: `refused` lives across
    // awaits, and the handler's future must stay `Send`.
    let tracked = parking_lot::Mutex::new(Some(
        state.query.tracker.start(query_id, &verified, &req.query),
    ));
    let tracked_at = std::time::Instant::now();

    tracing::info!(
        event_type = "export_start",
        user = %verified.name,
        roles = %verified.roles_display(),
        query_id,
        query_len = req.query.len(),
        %format,
        limit,
        "executing export"
    );
    tracing::debug!(
        event_type = "query_text",
        query_id,
        query = %req.query,
        "raw query text (DEBUG-only: never stored under the default filter)"
    );

    // The export's timing account (ADR-0046). Its window opens here, so
    // it covers the admission check, and closes after the body renders;
    // `export_complete.duration_ms` keeps its own narrower window below.
    let timing = TimingGuard::for_request(
        query_id,
        request_id.0,
        TimingKind::Export(match format {
            trawl_api::ExportFormat::Csv => ExportFormat::Csv,
            trawl_api::ExportFormat::Json => ExportFormat::Json,
            trawl_api::ExportFormat::Parquet => ExportFormat::Parquet,
        }),
        std::time::Instant::now(),
    );
    // Every early return writes the account with the error it returns;
    // an account the pool already wrote (a timeout, a capacity refusal)
    // stays as the pool wrote it. It closes the tracker entry with that
    // error too.
    let refused = |e: ServerError| {
        let end = std::time::Instant::now();
        timing.emit_complete(Outcome::of_error(&e), end);
        close_failed_export(
            tracked.lock().take(),
            &e,
            end.saturating_duration_since(tracked_at),
        );
        e
    };

    // One admission door for every lane (ADR-0024): the parquet export
    // takes a different route through the pool than CSV and JSON do, so
    // asking here is what makes all three refuse the same text.
    {
        let _phase = timing.clock().guard(QueryPhase::DslCheck);
        crate::admission::admit(&state.query.pool, &req.query, deadline, Some(&pending)).await
    }
    .map_err(refused)?;

    let start = std::time::Instant::now();

    if format == trawl_api::ExportFormat::Parquet {
        let (bytes, rows) = state
            .query
            .pool
            .export_parquet(
                query_id,
                &req.query,
                limit,
                deadline,
                crate::pool::WorkContext::key(crate::pool::WorkKind::Export, verified.id)
                    .with_user(&verified.name)
                    .with_timing(timing.timing().clone()),
            )
            .await
            .map_err(refused)?;
        let duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
        // The readback, this format's render, ran inside the worker.
        let ready = std::time::Instant::now();
        timing.emit_complete(Outcome::Success, ready);
        let took = ready.saturating_duration_since(tracked_at);
        close_ready_export(tracked.lock().take(), rows, took);

        tracing::info!(
            event_type = "export_complete",
            user = %verified.name,
            query_id,
            query_len = req.query.len(),
            format = "parquet",
            bytes = bytes.len(),
            duration_ms,
            "parquet export complete"
        );

        record_export_history(
            &state,
            deadline,
            verified.id,
            query_id,
            &req.query,
            took,
            rows,
        )
        .await;

        return Ok((
            [
                (
                    header::CONTENT_TYPE,
                    "application/vnd.apache.parquet".to_owned(),
                ),
                (
                    header::CONTENT_DISPOSITION,
                    "attachment; filename=\"export.parquet\"".to_owned(),
                ),
            ],
            bytes,
        ));
    }

    // CSV and JSON exports: execute query and render in memory, cut at
    // `limit` the way the Parquet lane's `LIMIT` cuts. A kv tail's input
    // cannot be cut without changing its answer, so it is bounded by
    // `max_export_rows` instead (see `RowCap::Truncate`).
    let capture_debug = state.query.query_log.is_some();
    let cap = trawl_engine::executor::RowCap::Truncate {
        rows: limit,
        tail_input: max_export_rows,
    };
    let outcome = state
        .query
        .pool
        .execute_capped(
            query_id,
            &req.query,
            cap,
            deadline,
            capture_debug,
            crate::pool::WorkContext::key(crate::pool::WorkKind::Export, verified.id)
                .with_user(&verified.name)
                .with_timing(timing.timing().clone()),
        )
        .await;
    let duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

    let result = match outcome.result {
        Ok(qr) => qr,
        Err(e) => {
            let e = refused(e);
            let error_msg = e.safe_message();
            write_query_log(
                &state,
                &verified,
                &req.query,
                outcome.debug.as_ref(),
                timing.written().as_ref(),
                None,
                duration_ms,
                Some(&error_msg),
            );
            return Err(e);
        }
    };

    let (content_type, filename, body) = match format {
        trawl_api::ExportFormat::Csv => (
            "text/csv; charset=utf-8".to_owned(),
            "attachment; filename=\"export.csv\"".to_owned(),
            timing
                .clock()
                .time(QueryPhase::Render, || generate_csv(&result).into_bytes()),
        ),
        trawl_api::ExportFormat::Json => (
            "application/x-ndjson".to_owned(),
            "attachment; filename=\"export.ndjson\"".to_owned(),
            timing
                .clock()
                .time(QueryPhase::Render, || generate_ndjson(&result).into_bytes()),
        ),
        trawl_api::ExportFormat::Parquet => {
            return Err(refused(ServerError::Internal(
                "parquet export reached unexpected code path".into(),
            )));
        }
    };
    let ready = std::time::Instant::now();
    timing.emit_complete(Outcome::Success, ready);
    let took = ready.saturating_duration_since(tracked_at);
    // The body's data rows, after the cut at `limit` (a kv tail's groups).
    let rows = result.row_count();
    close_ready_export(tracked.lock().take(), rows, took);

    tracing::info!(
        event_type = "export_complete",
        user = %verified.name,
        query_id,
        query_len = req.query.len(),
        %format,
        rows,
        bytes = body.len(),
        duration_ms,
        "export complete"
    );

    write_query_log(
        &state,
        &verified,
        &req.query,
        outcome.debug.as_ref(),
        timing.written().as_ref(),
        Some(&result),
        duration_ms,
        None,
    );

    record_export_history(
        &state,
        deadline,
        verified.id,
        query_id,
        &req.query,
        took,
        rows,
    )
    .await;

    Ok((
        [
            (header::CONTENT_TYPE, content_type),
            (header::CONTENT_DISPOSITION, filename),
        ],
        body,
    ))
}

fn close_failed_export(
    tracked: Option<crate::tracker::Tracked>,
    error: &ServerError,
    took: std::time::Duration,
) {
    let Some(tracked) = tracked else {
        return;
    };
    let status = if matches!(error, ServerError::Timeout) {
        tracked.timeout();
        "timeout"
    } else {
        tracked.fail(&error.safe_message());
        "error"
    };
    metrics::counter!(crate::metrics::QUERIES_TOTAL, "status" => status).increment(1);
    metrics::histogram!(crate::metrics::QUERY_DURATION).record(took.as_secs_f64());
}

/// Close a ready export's tracker entry and count its success. The caller
/// does this before any await: once the body exists the export succeeded,
/// even if its caller leaves while the history row is written.
fn close_ready_export(
    tracked: Option<crate::tracker::Tracked>,
    rows: usize,
    took: std::time::Duration,
) {
    let Some(tracked) = tracked else {
        return;
    };
    tracked.complete(rows);
    metrics::counter!(crate::metrics::QUERIES_TOTAL, "status" => "success").increment(1);
    metrics::histogram!(crate::metrics::QUERY_DURATION).record(took.as_secs_f64());
}

async fn record_export_history(
    state: &AppState,
    deadline: crate::deadline::Deadline,
    key_id: i64,
    query_id: u64,
    dsl: &str,
    took: std::time::Duration,
    rows: usize,
) {
    // `Deadline::run` polls its future once even at a passed deadline,
    // which would start the write.
    if deadline.expired() {
        tracing::warn!(
            event_type = "history_error",
            key_id,
            query_id,
            "skipped the history write: the export's deadline passed with the body already in hand"
        );
        return;
    }
    let duration_ms = u64::try_from(took.as_millis()).unwrap_or(u64::MAX);
    let recorded = deadline
        .run(
            state
                .storage
                .history
                .record_query(key_id, dsl, duration_ms, rows, RunStatus::Success),
        )
        .await;
    match recorded {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => tracing::warn!(
            event_type = "history_error",
            key_id,
            query_id,
            error_class = error.class(),
            "failed to record export in history store"
        ),
        Err(crate::deadline::Expired) => tracing::warn!(
            event_type = "history_error",
            key_id,
            query_id,
            "abandoned the history write: the export's deadline passed with the body already in hand"
        ),
    }
}

#[derive(Debug, Deserialize)]
pub struct ExportParams {
    pub format: Option<trawl_api::ExportFormat>,
}

const QUERY_LOG_SAMPLE_SIZE: usize = 5;

#[allow(clippy::too_many_arguments)]
fn write_query_log(
    state: &AppState,
    verified: &VerifiedKey,
    dsl: &str,
    debug: Option<&PoolDebugInfo>,
    phases: Option<&trawl_engine::timing::PhaseTotals>,
    result: Option<&QueryResult>,
    duration_ms: u64,
    error: Option<&str>,
) {
    let Some(log) = &state.query.query_log else {
        return;
    };
    let Some(debug) = debug else {
        return;
    };

    let entry = QueryLogEntry {
        ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        user: verified.name.clone(),
        role: verified.roles_display(),
        dsl: dsl.to_owned(),
        source: SourceDebug {
            computed: debug.computed_source.clone(),
            globs: debug.glob_count,
            service_filter: debug.service_filter.clone(),
            time_filter_secs: debug.time_filter_secs,
        },
        hot_buffer: HotBufferDebug {
            status: debug.hot_status,
            events: debug.hot_events,
            batches: debug.hot_batches,
            bytes: debug.hot_bytes,
        },
        sql: debug.sql.clone(),
        params: debug.params.clone(),
        result: build_result_debug(result),
        timing_ms: TimingDebug::new(phases, duration_ms),
        error: error.map(String::from),
    };

    log.write(&entry);
}

fn build_result_debug(result: Option<&QueryResult>) -> ResultDebug {
    let Some(qr) = result else {
        return ResultDebug {
            status: "error",
            columns: vec![],
            row_count: 0,
            sample: vec![],
        };
    };

    let columns: Vec<String> = qr.columns.iter().map(|c| c.name.clone()).collect();
    let sample: Vec<BTreeMap<String, serde_json::Value>> = qr
        .rows
        .iter()
        .take(QUERY_LOG_SAMPLE_SIZE)
        .map(|row| {
            columns
                .iter()
                .zip(row.iter())
                .map(|(col, val)| (col.clone(), val.to_plain_json()))
                .collect()
        })
        .collect();

    ResultDebug {
        status: "success",
        columns,
        row_count: qr.row_count(),
        sample,
    }
}

fn generate_csv(result: &QueryResult) -> String {
    let estimated = result.rows.len() * result.columns.len() * 50;
    let mut csv = String::with_capacity(estimated);

    for (i, col) in result.columns.iter().enumerate() {
        if i > 0 {
            csv.push(',');
        }
        csv.push_str(&quote_csv_field(&col.name));
    }
    csv.push('\n');

    for row in &result.rows {
        for (i, value) in row.iter().enumerate() {
            if i > 0 {
                csv.push(',');
            }
            let cell = value_to_string(value);
            csv.push_str(&quote_csv_field(&cell));
        }
        csv.push('\n');
    }

    csv
}

/// An export file is not the wire (ADR-0035): every cell goes through
/// [`Value::to_plain_json`], so integers stay JSON numbers that jq and Vector
/// read natively.
fn generate_ndjson(result: &QueryResult) -> String {
    let mut buf = String::new();
    for row in &result.rows {
        let mut map = serde_json::Map::with_capacity(result.columns.len());
        for (col, val) in result.columns.iter().zip(row.iter()) {
            map.insert(col.name.clone(), val.to_plain_json());
        }
        buf.push_str(&serde_json::to_string(&map).expect("Map serialization is infallible"));
        buf.push('\n');
    }
    buf
}

fn quote_csv_field(s: &str) -> Cow<'_, str> {
    let needs_quoting = s.contains(',') || s.contains('\n') || s.contains('"');

    if needs_quoting || s.is_empty() {
        let escaped = s.replace('"', "\"\"");
        Cow::Owned(format!("\"{escaped}\""))
    } else {
        Cow::Borrowed(s)
    }
}

fn value_to_string(value: &Value) -> Cow<'_, str> {
    match value {
        Value::Null => Cow::Borrowed(""),
        Value::Boolean(b) => Cow::Owned(b.to_string()),
        Value::Integer(i) => Cow::Owned(i.to_string()),
        Value::UInt(u) => Cow::Owned(u.to_string()),
        Value::Float(f) => Cow::Owned(f.to_string()),
        Value::String(s) => sanitize_csv_formula(s),
        Value::Array(_) => {
            let s = value.to_string();
            match sanitize_csv_formula(&s) {
                Cow::Borrowed(_) => Cow::Owned(s),
                Cow::Owned(owned) => Cow::Owned(owned),
            }
        }
    }
}

fn stream_output_pins(
    catalog: &trawl_core::schema::FieldTypes,
    pipeline: &[trawl_core::ast::Spanned<trawl_core::ast::PipeStage>],
) -> trawl_core::pin_scope::PinScope {
    let mut root = catalog.clone();
    for (name, ty) in trawl_core::schema::ENVELOPE_TYPES {
        root.insert(name, *ty);
    }
    let mut scope = trawl_core::pin_scope::PinScope::root(&root);
    for stage in pipeline {
        scope.advance(&stage.node);
    }
    scope
}

/// What a live stream evaluates each event with, compiled once before the
/// stream opens.
pub(crate) struct StreamSetup {
    filter: trawl_core::filter::CompiledFilter,
    stream_plan: trawl_core::stream::StreamPlan,
    output_pins: trawl_core::pin_scope::PinScope,
    /// The stop the filter's and the plan's patterns were prepared under.
    /// The stream's live run reads it, and its driver requests it when the
    /// stream goes away.
    stop: trawl_core::cancel::Stop,
}

/// Admit `dsl` and compile it for live evaluation against `pin_snapshot`.
/// `deadline` bounds the setup only.
pub(crate) async fn admit_stream(
    pool: &crate::pool::ExecutorPool,
    dsl: &str,
    pin_snapshot: trawl_core::schema::FieldTypes,
    deadline: crate::deadline::Deadline,
) -> Result<StreamSetup, ServerError> {
    let dsl = dsl.to_owned();
    pool.prepare(deadline, crate::prepare::Door::Stream, move |stop| {
        prepare_stream_with(&dsl, &pin_snapshot, stop)
    })
    .await?
}

pub(crate) fn prepare_stream_with(
    dsl: &str,
    pin_snapshot: &trawl_core::schema::FieldTypes,
    stop: &trawl_core::cancel::Stop,
) -> Result<StreamSetup, ServerError> {
    use trawl_core::filter::FilterError;
    use trawl_engine::error::EngineError;

    let ast = crate::admission::check_dsl_with(dsl, stop)?;
    let mut preparer = trawl_core::pattern::Preparer::new().with_stop(stop.clone());
    let filter =
        trawl_core::filter::CompiledFilter::compile_with(&ast.search, pin_snapshot, &mut preparer)
            .map_err(|e| match e {
                FilterError::RegexResources(refusal) => ServerError::RegexResources(refusal),
                FilterError::Emit(e) => ServerError::BadRequest(e.to_string()),
                FilterError::Cancelled => ServerError::Engine(EngineError::Cancelled),
            })?;
    stop.check()
        .map_err(|trawl_core::cancel::Cancelled| ServerError::Engine(EngineError::Cancelled))?;
    let stream_plan = trawl_core::stream::compile_stream_plan_with(
        &ast.pipeline,
        &trawl_core::pin_scope::PinScope::root(pin_snapshot),
        None,
        &mut preparer,
    )
    .map_err(stream_plan_refusal)?;
    let output_pins = stream_output_pins(pin_snapshot, &ast.pipeline);
    Ok(StreamSetup {
        filter,
        stream_plan,
        output_pins,
        stop: stop.clone(),
    })
}

fn stream_plan_refusal(e: trawl_core::stream::StreamPlanError) -> ServerError {
    use trawl_core::pattern::PatternError;
    use trawl_core::stream::StreamPlanError;
    match e {
        StreamPlanError::Pattern(PatternError::Resources(refusal)) => {
            ServerError::RegexResources(refusal)
        }
        StreamPlanError::Cancelled => {
            ServerError::Engine(trawl_engine::error::EngineError::Cancelled)
        }
        e => ServerError::BadRequest(e.to_string()),
    }
}

/// `GET /api/v1/stream` — stream live events via Server-Sent Events (SSE).
pub async fn stream_query(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    params: Result<Query<StreamParams>, QueryRejection>,
) -> Result<
    Sse<
        KeepAliveStream<
            std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Result<Event, Infallible>> + Send>>,
        >,
    >,
    ServerError,
> {
    if !verified.has_permission(Permission::Stream) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }
    let Query(params) = shaped("query parameters", params)?;

    // The owned permit moves into the stream's live state: it comes back when
    // the client disconnects, or, if a live job is running then, when that job
    // exits.
    let sse_permit = state
        .query
        .sse_semaphore
        .clone()
        .try_acquire_owned()
        .map_err(|_| ServerError::TooManyStreams)?;

    let query_dsl = params.query.clone();

    // Same id vocabulary as /query and /export: correlate on the id, never
    // on the DSL, which can carry customer identifiers or incident
    // indicators.
    let query_id = state.query.pool.allocate_query_id();
    crate::transport::failure::record_query_id(query_id);

    tracing::info!(
        event_type = "stream_start",
        user = %verified.name,
        query_id,
        query_len = query_dsl.len(),
        "starting SSE stream"
    );
    tracing::debug!(
        event_type = "query_text",
        query_id,
        query = %query_dsl,
        "raw query text (DEBUG-only: never stored under the default filter)"
    );

    // The snapshot is held for the stream's life, so a mid-stream repin
    // only takes effect on reconnect.
    let StreamSetup {
        filter,
        stream_plan,
        output_pins,
        stop,
    } = admit_stream(
        &state.query.pool,
        &query_dsl,
        state.query.field_catalog.all(),
        crate::deadline::Deadline::after(std::time::Duration::from_secs(state.query.timeout_secs)),
    )
    .await?;

    let Some(ref bus) = state.ingest.event_bus else {
        return Err(ServerError::BadRequest(
            "streaming requires ingest to be enabled".into(),
        ));
    };

    let subscriber = EventBus::subscribe(bus.as_ref());
    let live = crate::live::Live::new(filter, stream_plan, stop, output_pins, sse_permit);
    let event_stream: std::pin::Pin<
        Box<dyn tokio_stream::Stream<Item = Result<Event, Infallible>> + Send>,
    > = Box::pin(crate::live::frames(subscriber, live, query_id));

    Ok(Sse::new(event_stream).keep_alive(KeepAlive::default()))
}

#[derive(Debug, Deserialize)]
pub struct StreamParams {
    pub query: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Corpus facts read the configured root as a path. A root whose name
    /// holds `*` would be cut short by any glob-based recovery and read
    /// nothing.
    #[test]
    fn configured_data_root_is_passed_directly() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("data[1]*");
        let hour = root.join("prod/2026-06-20/00");
        std::fs::create_dir_all(&hour).unwrap();
        std::fs::write(hour.join("nginx.parquet"), "0123456789").unwrap();

        for root in [root.clone(), root.join("")] {
            let (earliest, latest, bytes, services, files) = collect_catalog_metadata(&root);
            assert_eq!(
                earliest.as_deref(),
                Some("2026-06-20"),
                "{}",
                root.display()
            );
            assert_eq!(latest.as_deref(), Some("2026-06-20"));
            assert_eq!((bytes, files), (10, 1));
            assert_eq!(services, ["nginx"]);
        }
    }

    #[test]
    fn stream_setup_builds_a_shared_pattern_once() {
        use trawl_core::pattern::thread_builds;
        let dsl = r#"message=/time(out)?/ | where message matches "time(out)?""#;
        let before = thread_builds();
        trawl_core::parser::parse(dsl).expect("the query parses");
        let checked = thread_builds() - before;
        assert_eq!(checked, 1, "parse checks the shared pattern once");

        let before = thread_builds();
        let setup = prepare_stream_with(
            dsl,
            &trawl_core::schema::FieldTypes::new(),
            &trawl_core::cancel::Stop::never(),
        );
        assert!(setup.is_ok());
        assert_eq!(
            thread_builds() - before,
            checked + 1,
            "admission checks it, and the one ledger builds it once"
        );
    }

    /// Seven bodies of the `(\pL{12})` class charge past the budget at the
    /// seventh.
    #[test]
    fn stream_setup_refuses_regex_memory_typed() {
        use trawl_core::pattern::RegexRefusal;
        let heavy = [
            r"(\pL{12})0",
            r"(\pL{12})1",
            r"(\pL{12})2",
            r"(\pL{12})3",
            r"(\pL{12})4",
            r"(\pL{12})5",
            r"(\pL{12})6",
        ];
        let in_filter = format!("message=/{}/", heavy.join("/,/"));
        let (searched, staged) = heavy.split_at(4);
        let in_plan = staged.iter().fold(
            format!("message=/{}/", searched.join("/,/")),
            |mut dsl, body| {
                dsl.push_str(" | where message matches /");
                dsl.push_str(body);
                dsl.push('/');
                dsl
            },
        );
        for dsl in [in_filter, in_plan] {
            let refused = prepare_stream_with(
                &dsl,
                &trawl_core::schema::FieldTypes::new(),
                &trawl_core::cancel::Stop::never(),
            )
            .err();
            assert!(
                matches!(
                    refused,
                    Some(ServerError::RegexResources(RegexRefusal::Budget))
                ),
                "{dsl}: {refused:?}"
            );
        }
    }

    #[test]
    fn stream_output_pins_hold_the_envelope_table() {
        use trawl_core::schema::{CanonicalType, FieldTypes};

        let mut catalog = FieldTypes::new();
        catalog.insert("status", CanonicalType::BigInt);
        let ast = trawl_core::parser::parse("* | rename _ingested as seen").unwrap();
        let pins = stream_output_pins(&catalog, &ast.pipeline);
        assert_eq!(pins.pin_for("_time"), Some(CanonicalType::Timestamp));
        assert_eq!(pins.pin_for("seen"), Some(CanonicalType::Timestamp));
        assert_eq!(pins.pin_for("_ingested"), None);
        assert_eq!(pins.pin_for("status"), Some(CanonicalType::BigInt));

        let pins = stream_output_pins(&FieldTypes::new(), &[]);
        assert_eq!(pins.pin_for("_time"), Some(CanonicalType::Timestamp));
    }

    #[test]
    fn history_numeric_extractor_characterization() {
        for (raw, expected) in [
            ("", Some((None, None))),
            ("?limit=5", Some((None, None))),
            ("?filter=x", Some((None, None))),
            ("%3Flimit=5", Some((None, None))),
            ("%3Ffilter=x", Some((None, None))),
            ("?%6cimit=5", Some((None, None))),
            ("limit=0&offset=001", Some((Some(0), Some(1)))),
            ("%6cimit=%31&off%73et=2", Some((Some(1), Some(2)))),
            ("limit=%2B1", Some((Some(1), None))),
            ("limit=+1", None),
            ("limit=", None),
            ("limit", None),
            ("offset=-1", None),
            ("offset=%201", None),
            ("offset=1%20", None),
            ("limit=1&limit=2", None),
            ("limit=1&%6cimit=1", None),
            ("offset=1&offset=1", None),
            ("limit=%2531", None),
            ("limit=%GG", None),
            ("limit=%FF", None),
            ("offset=184467440737095516160", None),
            ("unknown=%FF&limit=2", Some((Some(2), None))),
            ("filter=ignored&offset=3", Some((None, Some(3)))),
        ] {
            let uri = format!("/api/v1/history?{raw}").parse().unwrap();
            let actual = Query::<HistoryParams>::try_from_uri(&uri)
                .ok()
                .map(|Query(params)| (params.limit, params.offset));
            assert_eq!(actual, expected, "legacy numeric admission for {raw:?}");
        }
    }

    fn viewer(id: i64, name: &str, permissions: &[&str]) -> VerifiedKey {
        VerifiedKey::from_roles(
            id,
            "testtest",
            name,
            fleet_auth::PrincipalKind::Service,
            vec![fleet_auth::Role {
                id: 1,
                name: "test-role".into(),
                rate_rpm: None,
                permissions: permissions
                    .iter()
                    .map(|p| fleet_auth::RolePermission {
                        app: "trawl".into(),
                        permission: (*p).to_owned(),
                    })
                    .collect(),
            }],
        )
    }

    fn retained_query(owner: crate::pool::WorkOwner) -> crate::pool::RetainedWork {
        crate::pool::RetainedWork {
            id: 42,
            kind: crate::pool::WorkKind::Query,
            started: true,
            retained: std::time::Duration::from_millis(1500),
            owner,
            user: Some(std::sync::Arc::from("twin")),
            display: Some("service=nginx | stats count()".to_owned()),
        }
    }

    #[test]
    fn retained_interactive_work_keeps_its_existing_display_visibility() {
        let work = retained_query(crate::pool::WorkOwner::Key(7));

        let admin = viewer(99, "ops", &["query", "server_manage"]);
        let owner = viewer(7, "twin", &["query"]);
        let unrelated = viewer(8, "twin", &["query"]);

        for reader in [&admin, &owner, &unrelated] {
            let snap = retained_snapshot(&work, reader);
            assert_eq!(snap.id, 42);
            assert_eq!(snap.kind, "query");
            assert!(snap.started);
            assert_eq!(snap.retained_ms, 1500);
            assert_eq!(snap.user.as_deref(), Some("twin"));
            assert_eq!(snap.query.as_deref(), Some("service=nginx | stats count()"));
        }
    }

    #[test]
    fn system_retained_work_shows_no_text_below_server_manage() {
        let work = retained_query(crate::pool::WorkOwner::System);

        let analyst = retained_snapshot(&work, &viewer(7, "twin", &["query", "query_cancel"]));
        assert_eq!(analyst.user, None);
        assert_eq!(analyst.query, None);
        assert_eq!(analyst.kind, "query");

        let admin = retained_snapshot(&work, &viewer(99, "ops", &["query", "server_manage"]));
        assert_eq!(
            admin.query.as_deref(),
            Some("service=nginx | stats count()")
        );
    }

    #[test]
    fn a_filtered_retained_entry_omits_the_fields_it_hides() {
        let work = retained_query(crate::pool::WorkOwner::System);
        let snap = retained_snapshot(&work, &viewer(8, "twin", &["query"]));
        let json = serde_json::to_string(&snap).expect("serialize");
        assert!(!json.contains("\"user\":"), "{json}");
        assert!(!json.contains("\"query\":"), "{json}");
        assert!(json.contains("\"retained_ms\":1500"), "{json}");
    }

    #[test]
    fn cancel_authority_is_the_key_id_and_admin_reaches_system_work() {
        use crate::pool::WorkOwner;

        let admin = viewer(99, "ops", &["query", "server_manage"]);
        let owner = viewer(7, "twin", &["query", "query_cancel"]);
        let twin = viewer(8, "twin", &["query", "query_cancel"]);
        let reader = viewer(9, "reader", &["query"]);

        assert!(check_cancel_authority(&owner, Some(7), None).is_ok());
        assert!(check_cancel_authority(&owner, None, Some(WorkOwner::Key(7))).is_ok());

        let denied = check_cancel_authority(&twin, Some(7), Some(WorkOwner::Key(7)))
            .expect_err("a twin name is not ownership");
        assert!(matches!(&denied, ServerError::Forbidden(m) if m == "cannot cancel this query"));

        let system = check_cancel_authority(&owner, None, Some(WorkOwner::System))
            .expect_err("system work is not a key's own work");
        assert!(matches!(&system, ServerError::Forbidden(m) if m == "cannot cancel this query"));
        assert!(
            check_cancel_authority(&admin, None, Some(WorkOwner::System)).is_ok(),
            "an operator can stop the server's own work"
        );

        let ungranted = check_cancel_authority(&reader, Some(9), None)
            .expect_err("QueryCancel is the gate, ownership is only the scope");
        assert!(matches!(&ungranted, ServerError::Forbidden(m) if m == "insufficient permissions"));
    }

    fn retention_config(max_age_days: u64, envs: &[(&str, u64)]) -> crate::config::RetentionConfig {
        crate::config::RetentionConfig {
            max_age_days,
            env: envs
                .iter()
                .map(|(name, days)| {
                    (
                        (*name).to_owned(),
                        crate::config::EnvRetention {
                            max_age_days: *days,
                        },
                    )
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn since_from_secs_saturates_instead_of_panicking() {
        // `?since_secs=` is request-controlled: no value may panic the
        // handler (a 500 via CatchPanicLayer) or produce an instant
        // postgres cannot bind.
        assert_eq!(since_from_secs(None), None);

        let hour = since_from_secs(Some(3600)).expect("finite window");
        let elapsed = chrono::Utc::now() - hour;
        assert!(elapsed >= chrono::TimeDelta::seconds(3600));
        assert!(elapsed < chrono::TimeDelta::seconds(3700));

        // Each value overflows a different step: the first the
        // `DateTime - TimeDelta` subtraction, the rest `TimeDelta::seconds`
        // itself.
        for s in [
            100_000_000_000_000_u64,
            10_000_000_000_000_000,
            u64::try_from(i64::MAX).expect("i64::MAX is non-negative"),
            u64::MAX,
        ] {
            assert_eq!(
                since_from_secs(Some(s)),
                Some(chrono::DateTime::UNIX_EPOCH),
                "since_secs={s} must saturate at the epoch"
            );
        }
    }

    /// The `/api/v1/schema` window is the retention horizon, per-env
    /// entries included. This is the handler's own line
    /// (`since_from_secs(state.query.retention_horizon_secs)`) with the
    /// horizon resolved from a config instead of from `AppState`, which
    /// needs a live postgres to build.
    #[test]
    fn schema_window_is_the_retention_horizon() {
        let mixed = retention_config(90, &[("prod", 365), ("lab", 7)]);
        let horizon = crate::retention::maximum_enabled_age_secs(&mixed);
        let since = since_from_secs(horizon).expect("every env ages out");
        let elapsed = chrono::Utc::now() - since;
        assert!(
            elapsed >= chrono::TimeDelta::days(365),
            "the window is the LONGEST age any env keeps, not the shortest"
        );
        assert!(elapsed < chrono::TimeDelta::days(366));

        let keeps_forever = retention_config(90, &[("prod", 0)]);
        assert_eq!(
            since_from_secs(crate::retention::maximum_enabled_age_secs(&keeps_forever)),
            None,
            "one env keeping its data forever lifts the window for everyone"
        );

        let effectively_never = retention_config(90, &[("archive", u64::MAX)]);
        assert_eq!(
            since_from_secs(crate::retention::maximum_enabled_age_secs(
                &effectively_never
            )),
            Some(chrono::DateTime::UNIX_EPOCH),
            "a horizon past the epoch saturates there, hiding nothing"
        );
    }

    #[test]
    fn sanitize_csv_formula_prefixes_dangerous_chars() {
        assert_eq!(
            sanitize_csv_formula("=SUM(A1:A10)").as_ref(),
            "'=SUM(A1:A10)"
        );
        assert_eq!(sanitize_csv_formula("+cmd").as_ref(), "'+cmd");
        assert_eq!(sanitize_csv_formula("-cmd").as_ref(), "'-cmd");
        assert_eq!(sanitize_csv_formula("@import").as_ref(), "'@import");
        assert_eq!(sanitize_csv_formula("\tcmd").as_ref(), "'\tcmd");
        assert_eq!(sanitize_csv_formula("|cmd").as_ref(), "'|cmd");
    }

    #[test]
    fn sanitize_csv_formula_passes_safe_strings() {
        assert_eq!(sanitize_csv_formula("hello").as_ref(), "hello");
        assert_eq!(sanitize_csv_formula("200").as_ref(), "200");
        assert_eq!(sanitize_csv_formula("normal text").as_ref(), "normal text");
        assert_eq!(sanitize_csv_formula("").as_ref(), "");
    }

    #[test]
    fn value_to_string_sanitizes_strings() {
        let val = Value::String("=DROP TABLE".to_owned());
        assert_eq!(value_to_string(&val).as_ref(), "'=DROP TABLE");
    }

    #[test]
    fn value_to_string_does_not_sanitize_numbers() {
        assert_eq!(value_to_string(&Value::Integer(-42)).as_ref(), "-42");
        assert_eq!(value_to_string(&Value::Float(-1.5)).as_ref(), "-1.5");
    }

    #[test]
    fn ndjson_generation_produces_valid_output() {
        let result = QueryResult {
            columns: vec![
                trawl_engine::value::Column::new("host", trawl_engine::value::ColumnType::String),
                trawl_engine::value::Column::new("count", trawl_engine::value::ColumnType::Mixed),
            ],
            rows: vec![
                vec![Value::String("web-01".to_owned()), Value::Integer(42)],
                vec![Value::Null, Value::Float(1.5)],
            ],
        };
        let ndjson = generate_ndjson(&result);
        let lines: Vec<&str> = ndjson.trim_end().split('\n').collect();
        assert_eq!(lines.len(), 2);

        let row0: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(row0["host"], "web-01");
        assert_eq!(row0["count"], 42);

        let row1: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert!(row1["host"].is_null());
        assert_eq!(row1["count"], 1.5);
    }

    #[test]
    fn csv_generation_sanitizes_string_values() {
        let result = QueryResult {
            columns: vec![trawl_engine::value::Column::new(
                "cmd",
                trawl_engine::value::ColumnType::String,
            )],
            rows: vec![
                vec![Value::String("=evil()".to_owned())],
                vec![Value::String("safe".to_owned())],
            ],
        };
        let csv = generate_csv(&result);
        assert!(csv.contains("'=evil()"));
        assert!(csv.contains("safe"));
    }

    #[test]
    fn health_all_ok() {
        assert_eq!(
            derive_health_status(EngineCheck::Ok, true, true, true, true, true),
            HealthStatus::Ok
        );
    }

    #[test]
    fn health_auth_down_is_degraded() {
        assert_eq!(
            derive_health_status(EngineCheck::Ok, false, true, true, true, true),
            HealthStatus::Degraded
        );
    }

    #[test]
    fn health_storage_down_is_degraded() {
        assert_eq!(
            derive_health_status(EngineCheck::Ok, true, false, true, true, true),
            HealthStatus::Degraded
        );
    }

    #[test]
    fn health_data_path_down_is_degraded() {
        assert_eq!(
            derive_health_status(EngineCheck::Ok, true, true, false, true, true),
            HealthStatus::Degraded
        );
    }

    #[test]
    fn health_all_noncritical_down_is_degraded() {
        assert_eq!(
            derive_health_status(EngineCheck::Ok, false, false, false, true, true),
            HealthStatus::Degraded
        );
    }

    #[test]
    fn health_ingest_refusing_alone_is_degraded() {
        assert_eq!(
            derive_health_status(EngineCheck::Ok, true, true, true, false, true),
            HealthStatus::Degraded
        );
    }

    #[test]
    fn health_duckdb_down_while_ingest_refusing_is_unavailable() {
        assert_eq!(
            derive_health_status(EngineCheck::Error, true, true, true, false, true),
            HealthStatus::Unavailable
        );
    }

    #[test]
    fn ingest_capacity_reads_refusing_only_while_the_buffer_refuses() {
        use crate::hot_buffer::{AdmissionState, Charge, HotBuffer, HotBufferConfig, Refusal};
        use crate::ingest::producer::ProducerKind;

        assert!(ingest_capacity_admitting(None));
        assert_eq!(ingest_capacity_check(ingest_capacity_admitting(None)), "ok");

        let buf = HotBuffer::new(HotBufferConfig {
            max_events: 16,
            max_bytes: 1_600,
        });
        assert_eq!(
            ingest_capacity_check(ingest_capacity_admitting(Some(&buf))),
            "ok"
        );

        let held = buf
            .reserve(
                ProducerKind::Http,
                Charge {
                    events: 15,
                    bytes: 15,
                },
            )
            .expect("the external ceiling admits 15 of 16");
        assert_eq!(buf.admission_state(), AdmissionState::Pressure);
        assert_eq!(
            ingest_capacity_check(ingest_capacity_admitting(Some(&buf))),
            "ok"
        );

        assert_eq!(
            buf.reserve(
                ProducerKind::Http,
                Charge {
                    events: 1,
                    bytes: 1
                }
            )
            .map(|_| ()),
            Err(Refusal::Full)
        );
        assert_eq!(buf.admission_state(), AdmissionState::Refusing);
        assert_eq!(
            ingest_capacity_check(ingest_capacity_admitting(Some(&buf))),
            "refusing"
        );

        drop(held);
        assert_eq!(buf.admission_state(), AdmissionState::Open);
        assert_eq!(
            ingest_capacity_check(ingest_capacity_admitting(Some(&buf))),
            "ok"
        );
    }

    #[test]
    fn health_corpus_unsettled_alone_is_degraded() {
        assert_eq!(
            derive_health_status(EngineCheck::Ok, true, true, true, true, false),
            HealthStatus::Degraded
        );
    }

    #[test]
    fn health_duckdb_down_while_corpus_unsettled_is_unavailable() {
        assert_eq!(
            derive_health_status(EngineCheck::Error, true, true, true, true, false),
            HealthStatus::Unavailable
        );
    }

    #[test]
    fn corpus_check_reads_ok_restart_backlog_or_rollup_pending() {
        use crate::publication::PublicationGate;

        assert_eq!(corpus_check(&PublicationGate::new()), "ok");

        let gate = PublicationGate::starting();
        assert_eq!(corpus_check(&gate), "restart_backlog");
        gate.finish_hydration(true).unwrap();
        assert_eq!(corpus_check(&gate), "restart_backlog");

        let dir = tempfile::tempdir().unwrap();
        // A probe during a normal rollup: the rollup's own marker is in
        // flight, so the check does not flap to `rollup_pending`.
        let rollup = dir.path().join(".rollup-normal");
        {
            let _writer = gate.blocking_write();
            let registration = gate.mark_rollup(&rollup);
            std::fs::write(&rollup, "").unwrap();
            assert_eq!(corpus_check(&gate), "restart_backlog", "during a rollup");
            std::fs::remove_file(&rollup).unwrap();
            registration.finish();
        }
        assert_eq!(corpus_check(&gate), "restart_backlog", "after the rollup");

        let marker = dir.path().join(".rollup-svc");
        std::fs::write(&marker, "").unwrap();
        {
            let _writer = gate.blocking_write();
            drop(gate.mark_rollup(&marker));
        }
        assert_eq!(corpus_check(&gate), "rollup_pending", "precedence");
        gate.settle_overhang(&gate.blocking_write());
        assert_eq!(corpus_check(&gate), "rollup_pending");
        std::fs::remove_file(&marker).unwrap();
        assert_eq!(corpus_check(&gate), "ok");

        let _writer = gate.blocking_write();
        assert_eq!(corpus_check(&gate), "ok");
    }

    #[test]
    fn health_duckdb_slow_is_degraded() {
        assert_eq!(
            derive_health_status(EngineCheck::Slow, true, true, true, true, true),
            HealthStatus::Degraded
        );
        assert_eq!(
            derive_health_status(EngineCheck::Slow, false, true, true, false, false),
            HealthStatus::Degraded
        );
    }

    #[test]
    fn health_duckdb_down_is_unavailable() {
        assert_eq!(
            derive_health_status(EngineCheck::Error, true, true, true, true, true),
            HealthStatus::Unavailable
        );
    }

    #[test]
    fn health_all_down_is_unavailable() {
        assert_eq!(
            derive_health_status(EngineCheck::Error, false, false, false, true, true),
            HealthStatus::Unavailable
        );
    }

    #[test]
    fn health_duckdb_and_noncritical_down_is_unavailable() {
        assert_eq!(
            derive_health_status(EngineCheck::Error, false, true, true, true, true),
            HealthStatus::Unavailable
        );
    }

    #[test]
    fn guard_vanished_run_result_relabels_only_absence_after_a_suspect_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let present = dir.path().join("here.parquet");
        std::fs::write(&present, b"parquet").unwrap();
        let gone = dir.path().join("gone.parquet");
        let run_file = |path: &std::path::Path| crate::from_saved::RunFile {
            run_id: 7,
            absolute_path: path.to_path_buf(),
        };
        let outcome = |result: Result<QueryResult, ServerError>| crate::pool::ExecuteOutcome {
            result,
            debug: None,
            severity_columns: vec![],
            histogram: None,
        };
        let one_column = || QueryResult {
            columns: vec![trawl_engine::value::Column::new(
                "host",
                trawl_engine::value::ColumnType::String,
            )],
            rows: vec![vec![Value::String("web-01".to_owned())]],
        };
        let expected = crate::from_saved::unavailable_run_message(7);

        let mut suspect = outcome(Ok(QueryResult::empty()));
        suspect.severity_columns = vec!["severity".to_owned()];
        let guarded = guard_vanished_run_result(suspect, &[run_file(&gone)]);
        match &guarded.result {
            Err(ServerError::Conflict(msg)) => {
                assert_eq!(
                    *msg, expected,
                    "a zero-column success over a vanished file is the named refusal"
                );
                assert!(
                    !msg.contains(dir.path().to_str().unwrap()),
                    "the refusal never says where the file was: {msg}"
                );
            }
            other => panic!("expected the unavailable-run Conflict, got: {other:?}"),
        }
        assert!(
            guarded.debug.is_none(),
            "the relabel carries the debug slot through untouched"
        );
        assert_eq!(
            guarded.severity_columns,
            ["severity"],
            "the relabel carries the severity columns through untouched"
        );

        let failed = outcome(Err(ServerError::Engine(
            trawl_engine::error::EngineError::ColdDataUnread,
        )));
        match &guard_vanished_run_result(failed, &[run_file(&gone)]).result {
            Err(ServerError::Conflict(msg)) => assert_eq!(
                *msg, expected,
                "an engine error over a vanished file is the same refusal"
            ),
            other => panic!("expected the unavailable-run Conflict, got: {other:?}"),
        }

        let read = outcome(Ok(one_column()));
        match &guard_vanished_run_result(read, &[run_file(&gone)]).result {
            Ok(qr) => assert_eq!(
                *qr,
                one_column(),
                "a result that has columns was read, and a later absence does not unsay it"
            ),
            other => panic!("expected the result to stand, got: {other:?}"),
        }

        let empty = outcome(Ok(QueryResult::empty()));
        match &guard_vanished_run_result(empty, &[run_file(&present)]).result {
            Ok(qr) => assert!(
                qr.columns.is_empty() && qr.rows.is_empty(),
                "a file that is there keeps its empty answer"
            ),
            other => panic!("expected the empty answer to stand, got: {other:?}"),
        }

        let rejected = outcome(Err(ServerError::BadRequest("offset too large".to_owned())));
        match &guard_vanished_run_result(rejected, &[run_file(&gone)]).result {
            Err(ServerError::BadRequest(msg)) => assert_eq!(
                msg, "offset too large",
                "a non-engine error keeps its own message"
            ),
            other => panic!("expected the BadRequest to stand, got: {other:?}"),
        }

        let unrelated = outcome(Ok(QueryResult::empty()));
        match &guard_vanished_run_result(unrelated, &[]).result {
            Ok(qr) => assert!(
                qr.columns.is_empty(),
                "with no run files the outcome is returned as it came"
            ),
            other => panic!("expected the outcome to pass through, got: {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_refusal_before_execution_warns_once() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(dir.path().join("nowhere"), dir.path().join("link")).unwrap();
        let run_files = [crate::from_saved::RunFile {
            run_id: 7,
            absolute_path: dir.path().join("link").join("run_7.parquet"),
        }];

        let events = crate::test_events::events_of(|| {
            let (refused, recheck) = check_run_files_before_execution(&run_files);
            let refused = refused.expect("a run file below a dangling link is refused");
            let guarded = guard_vanished_run_result(refused, recheck);
            assert!(
                matches!(
                    guarded.result,
                    Err(ServerError::Engine(trawl_engine::error::EngineError::Io(_)))
                ),
                "the answer is still the check's own redacted error: {:?}",
                guarded.result
            );
        });
        let warned = events
            .iter()
            .filter(|e| {
                e.is_default_level()
                    && e.fields.get("event_type").map(String::as_str)
                        == Some("from_saved_run_file_unresolved")
            })
            .count();
        assert_eq!(warned, 1, "one refusal, one WARN: {events:?}");
    }

    #[cfg(unix)]
    #[test]
    fn the_recheck_turns_an_unresolved_run_file_into_an_error() {
        use axum::response::IntoResponse as _;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap().to_owned();
        std::os::unix::fs::symlink(dir.path().join("nowhere"), dir.path().join("link")).unwrap();
        std::fs::write(dir.path().join("file"), b"not a directory").unwrap();
        let outcome = |result: Result<QueryResult, ServerError>| crate::pool::ExecuteOutcome {
            result,
            debug: None,
            severity_columns: vec![],
            histogram: None,
        };

        for (case, parent) in [("dangling link", "link"), ("regular file", "file")] {
            let run_file = crate::from_saved::RunFile {
                run_id: 7,
                absolute_path: dir.path().join(parent).join("run_7.parquet"),
            };
            let suspects = [
                ("no columns", outcome(Ok(QueryResult::empty()))),
                (
                    "engine error",
                    outcome(Err(ServerError::Engine(
                        trawl_engine::error::EngineError::ColdDataUnread,
                    ))),
                ),
            ];
            for (shape, suspect) in suspects {
                let guarded = guard_vanished_run_result(suspect, std::slice::from_ref(&run_file));
                let err = match guarded.result {
                    Err(err @ ServerError::Engine(trawl_engine::error::EngineError::Io(_))) => err,
                    other => panic!("{case}, {shape}: expected the redacted error, got: {other:?}"),
                };
                for text in [format!("{err:?}"), err.to_string(), err.safe_message()] {
                    assert!(!text.contains(&root), "{case}, {shape}: {text}");
                }
                assert_eq!(
                    err.safe_message(),
                    "query execution failed",
                    "{case}, {shape}"
                );
                assert_eq!(
                    err.code(),
                    trawl_api::ErrorCode::ExecutionError,
                    "{case}, {shape}"
                );
                assert_eq!(
                    err.into_response().status(),
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "{case}, {shape}"
                );
            }
        }
    }

    mod preparation {
        use std::thread::ThreadId;
        use std::time::Duration;

        use axum::response::IntoResponse as _;
        use trawl_core::cancel::Stop;
        use trawl_core::parser::{ParseKind, ParseLog};
        use trawl_engine::error::EngineError;

        use super::super::{
            admit_stream, degraded_fields_with_saved, degraded_in, prepare_stream_with,
            stream_plan_refusal, validate, validation,
        };
        use crate::deadline::Deadline;
        use crate::error::ServerError;
        use crate::pool::{ExecutorPool, Wake};
        use crate::prepare::Door;

        fn pool(permits: usize) -> ExecutorPool {
            ExecutorPool::for_test("/nonexistent".into(), permits, 100, None).unwrap()
        }

        fn far() -> Deadline {
            Deadline::after(Duration::from_secs(60))
        }

        /// Every parse `log` recorded ran on a thread other than this one.
        /// On a current-thread runtime the test thread is the only reactor
        /// thread, so each of them ran off the reactor.
        fn assert_off_this_thread(parses: &[(ThreadId, ParseKind)]) {
            assert!(!parses.is_empty(), "the query was parsed");
            let here = std::thread::current().id();
            assert!(
                parses.iter().all(|(thread, _)| *thread != here),
                "a parse ran on the reactor: {parses:?}"
            );
        }

        #[tokio::test(flavor = "current_thread")]
        async fn validate_parses_off_the_reactor() {
            let pool = pool(2);
            let log = ParseLog::watch("zz_cp6a_validate_marker");
            let answer = validate(
                &pool,
                "zz_cp6a_validate_marker | where message matches /zz_cp6a_validate_marker(s)?/"
                    .into(),
                far(),
            )
            .await
            .expect("answered");
            assert!(answer.valid, "{answer:?}");
            let parses = log.parses();
            assert!(
                parses.iter().any(|(_, kind)| *kind == ParseKind::Unbuilt),
                "the formatting reparse was watched too: {parses:?}"
            );
            assert_off_this_thread(&parses);
        }

        #[test]
        fn a_cancelled_validation_is_never_an_invalid_query() {
            let stop = Stop::new();
            stop.request();
            for dsl in [
                "error | where message matches /time(out)?/",
                "| | | broken {{{",
            ] {
                let answer = validation(dsl, &stop);
                assert!(
                    matches!(answer, Err(ServerError::Engine(EngineError::Cancelled))),
                    "{dsl}: {answer:?}"
                );
                let status = answer.unwrap_err().into_response().status();
                assert_ne!(status, axum::http::StatusCode::BAD_REQUEST, "{dsl}");
                assert_ne!(status, axum::http::StatusCode::OK, "{dsl}");
            }
        }

        #[tokio::test]
        async fn validate_refuses_when_the_allowance_is_full() {
            let pool = pool(1);
            assert_eq!(pool.preparation().size(), 1);
            let (release, held) = std::sync::mpsc::channel::<()>();
            let holder = {
                let pool = pool.clone();
                tokio::spawn(async move {
                    pool.prepare(far(), Door::Admission, move |_stop| held.recv())
                        .await
                })
            };
            while pool.preparation().in_use() < 1 {
                tokio::task::yield_now().await;
            }

            let refused = validate(
                &pool,
                "error".into(),
                Deadline::after(Duration::from_millis(50)),
            )
            .await;
            assert!(
                matches!(refused, Err(ServerError::PreparationUnavailable)),
                "{refused:?}"
            );
            let err = refused.unwrap_err();
            assert_eq!(err.cause_kind().as_str(), "preparation_allowance_exhausted");
            assert_eq!(
                err.into_response().status(),
                axum::http::StatusCode::SERVICE_UNAVAILABLE
            );

            release.send(()).unwrap();
            holder.await.unwrap().unwrap().unwrap();
        }

        fn degraded(fields: &[&str]) -> std::sync::Arc<crate::state::DegradedSnapshot> {
            let mut snapshot = crate::state::DegradedSnapshot::default();
            snapshot.fields = fields.iter().map(|f| (*f).to_owned()).collect();
            std::sync::Arc::new(snapshot)
        }

        #[test]
        fn degraded_fields_read_the_admitted_query() {
            let query =
                trawl_core::parser::parse("error | where zz_status > 400 | stats count() by host")
                    .expect("parses");
            let snapshot = degraded(&["zz_status", "zz_other"]);
            let before = trawl_core::parser::thread_parses();
            let named = degraded_in(&snapshot, [&query]);
            assert_eq!(trawl_core::parser::thread_parses(), before, "no parse");
            assert_eq!(named, ["zz_status"]);
        }

        #[tokio::test(flavor = "current_thread")]
        async fn degraded_fields_parse_the_saved_text_off_the_reactor() {
            let query = trawl_core::parser::parse("| from saved daily | where zz_typed > 1")
                .expect("parses");
            let saved = "zz_cp6a_saved_marker | where zz_saved > 1";

            let log = ParseLog::watch("zz_cp6a_saved_marker");
            let named =
                degraded_fields_with_saved(degraded(&[]), &query, saved, far(), &Wake::default())
                    .await;
            assert!(named.is_empty());
            assert!(log.parses().is_empty(), "a healthy install parses nothing");

            let named = degraded_fields_with_saved(
                degraded(&["zz_typed", "zz_saved"]),
                &query,
                saved,
                far(),
                &Wake::default(),
            )
            .await;
            assert_eq!(named, ["zz_saved", "zz_typed"]);
            assert_off_this_thread(&log.parses());
        }

        #[tokio::test(flavor = "current_thread")]
        async fn degraded_fields_wait_for_the_saved_text_within_the_deadline() {
            let query = trawl_core::parser::parse("| from saved daily | where zz_typed > 1")
                .expect("parses");
            // Long enough that its parse cannot finish before the first poll.
            let saved = format!("zz_saved > 1{}", " | where zz_saved > 1".repeat(2_000));
            let named = degraded_fields_with_saved(
                degraded(&["zz_typed", "zz_saved"]),
                &query,
                &saved,
                crate::deadline::Deadline::after(std::time::Duration::ZERO),
                &Wake::default(),
            )
            .await;
            assert_eq!(named, ["zz_typed"]);
        }

        #[test]
        fn degraded_fields_stop_waiting_for_a_held_parse_at_the_deadline() {
            use std::sync::Arc;
            use std::sync::atomic::{AtomicBool, Ordering};
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .max_blocking_threads(1)
                .build()
                .expect("runtime");
            let (release, held) = std::sync::mpsc::channel::<()>();
            let (holding, on_holding) = std::sync::mpsc::channel::<()>();
            let released = Arc::new(AtomicBool::new(false));
            drop(runtime.spawn_blocking(move || {
                holding.send(()).expect("the test waits for the hold");
                // A hang guard: a wait the deadline never ends is released
                // here, and the assertions below then fail.
                let _ = held.recv_timeout(Duration::from_secs(10));
            }));
            on_holding.recv().expect("the blocking thread is held");
            let watchdog_released = Arc::clone(&released);
            let release_late = release.clone();
            let (done, on_done) = std::sync::mpsc::channel::<()>();
            let watchdog = std::thread::spawn(move || {
                if on_done.recv_timeout(Duration::from_secs(5)).is_err() {
                    watchdog_released.store(true, Ordering::SeqCst);
                    let _ = release_late.send(());
                }
            });

            let query = trawl_core::parser::parse("| from saved daily | where zz_typed > 1")
                .expect("parses");
            let named = runtime.block_on(async {
                degraded_fields_with_saved(
                    degraded(&["zz_typed", "zz_saved"]),
                    &query,
                    "zz_saved > 1",
                    crate::deadline::Deadline::after(Duration::from_millis(200)),
                    &Wake::default(),
                )
                .await
            });
            let returned_while_held = !released.load(Ordering::SeqCst);
            let _ = done.send(());
            let _ = release.send(());
            runtime.shutdown_timeout(Duration::from_secs(10));
            drop(watchdog.join());

            assert!(returned_while_held, "the wait outlived its deadline");
            assert_eq!(named, ["zz_typed"]);
        }

        #[test]
        fn a_cancel_ends_the_wait_for_the_saved_text() {
            use std::sync::Arc;
            use std::sync::atomic::{AtomicBool, Ordering};
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .max_blocking_threads(1)
                .build()
                .expect("runtime");
            let (release, held) = std::sync::mpsc::channel::<()>();
            let (holding, on_holding) = std::sync::mpsc::channel::<()>();
            let released = Arc::new(AtomicBool::new(false));
            drop(runtime.spawn_blocking(move || {
                holding.send(()).expect("the test waits for the hold");
                // A hang guard: a wait the cancel never ends is released
                // here, and the assertions below then fail.
                let _ = held.recv_timeout(Duration::from_secs(10));
            }));
            on_holding.recv().expect("the blocking thread is held");
            let watchdog_released = Arc::clone(&released);
            let release_late = release.clone();
            let (done, on_done) = std::sync::mpsc::channel::<()>();
            let watchdog = std::thread::spawn(move || {
                if on_done.recv_timeout(Duration::from_secs(5)).is_err() {
                    watchdog_released.store(true, Ordering::SeqCst);
                    let _ = release_late.send(());
                }
            });

            let query = trawl_core::parser::parse("| from saved daily | where zz_typed > 1")
                .expect("parses");
            let (named, cancelled, next_wait) = runtime.block_on(async {
                let pool = pool(1);
                let id = pool.allocate_query_id();
                let pending = pool.pend(id, crate::pool::WorkKind::FromSaved);
                let (named, cancelled) = tokio::join!(
                    degraded_fields_with_saved(
                        degraded(&["zz_typed", "zz_saved"]),
                        &query,
                        "zz_saved > 1",
                        far(),
                        pending.wake(),
                    ),
                    async {
                        // The parse is dispatched and waiting first.
                        tokio::task::yield_now().await;
                        pool.cancel_by_id(id)
                    },
                );
                let next_wait = pool
                    .execute_with_source(
                        id,
                        "| where zz_typed > 1",
                        "read_parquet('/nonexistent/zz.parquet')",
                        far(),
                        false,
                        crate::pool::WorkContext::system(crate::pool::WorkKind::FromSaved),
                    )
                    .await;
                (named, cancelled, next_wait.result)
            });
            let returned_while_held = !released.load(Ordering::SeqCst);
            let _ = done.send(());
            let _ = release.send(());
            runtime.shutdown_timeout(Duration::from_secs(10));
            drop(watchdog.join());

            assert!(cancelled, "the cancel reached the waiting request");
            assert!(returned_while_held, "the wait outlived the cancel");
            assert_eq!(named, ["zz_typed"]);
            assert!(
                matches!(next_wait, Err(ServerError::CapacityNotStarted)),
                "{next_wait:?}"
            );
        }

        #[tokio::test(flavor = "current_thread")]
        async fn stream_setup_parses_off_the_reactor() {
            let pool = pool(2);
            let log = ParseLog::watch("zz_cp6a_stream_marker");
            let setup = admit_stream(
                &pool,
                "zz_cp6a_stream_marker | where message matches /zz_cp6a_stream_marker(s)?/",
                trawl_core::schema::FieldTypes::new(),
                far(),
            )
            .await;
            assert!(setup.is_ok(), "set up");
            assert_off_this_thread(&log.parses());
        }

        #[test]
        fn a_cancelled_stream_setup_is_a_cancellation() {
            let stop = Stop::new();
            stop.request();
            let refused = prepare_stream_with(
                "error | where message matches /time(out)?/",
                &trawl_core::schema::FieldTypes::new(),
                &stop,
            )
            .err();
            assert!(
                matches!(refused, Some(ServerError::Engine(EngineError::Cancelled))),
                "{refused:?}"
            );
            let plan = stream_plan_refusal(trawl_core::stream::StreamPlanError::Cancelled);
            assert!(
                matches!(plan, ServerError::Engine(EngineError::Cancelled)),
                "{plan:?}"
            );
        }
    }
}
