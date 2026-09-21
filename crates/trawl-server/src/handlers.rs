// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTTP request handlers for the trawl API.

use std::collections::HashMap;

use axum::extract::rejection::QueryRejection;
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
    GlobalRunSummary, HealthResponse, HealthStatus, HistoryEntryResponse, HistoryResponse,
    ListAllRunsResponse, ListReportRunsResponse, ListSavedResponse, PaginationMeta,
    QueriesResponse, QueryExecution, QueryRequest, QueryResponse, QueryStatus, ReportRunResponse,
    ReportRunSummary, RetainedWorkSnapshot, RunsSortDir, RunsSortKey, RunsStatsResponse,
    SavedQueryResponse, ScheduleResponse, SchemaColumnResponse, SchemaResponse, SetScheduleRequest,
    StatsResponse, UpdateSavedRequest, ValidationResponse, WhoAmIResponse,
};
use trawl_engine::value::{QueryResult, Value};

use std::collections::BTreeMap;

use crate::bus::{EventBus, EventSubscriber as _};
use crate::error::ServerError;
use crate::policy::{Permission, TrawlAuthz as _};
use crate::pool::PoolDebugInfo;
use crate::query_log::{HotBufferDebug, QueryLogEntry, ResultDebug, SourceDebug, TimingDebug};
use crate::report_window::{ScheduleWindow, format_window_bound};
use crate::scheduler::execute_scheduled_query;
use crate::state::{AppState, CachedFieldValues};
use crate::store::{
    HistoryEntry, ManualRunClaim, ReportRun, RunStatus, SavedQuery, Schedule, ScheduleWithStats,
    format_interval, parse_duration_secs, parse_interval,
};

// -- handlers ----------------------------------------------------------------

/// `POST /api/v1/query` — execute a DSL query against the configured data source.
///
/// Requires a valid bearer token (injected by auth middleware).
#[allow(clippy::too_many_lines)]
pub async fn query(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Json(req): Json<QueryRequest>,
) -> Result<Json<QueryResponse>, ServerError> {
    if !verified.has_permission(Permission::Query) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }

    // One absolute budget for the whole request (ADR-0024), stamped
    // before tracking, admission and source resolution: every wait below
    // spends from this instant, so no phase gets a fresh timeout.
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

    // Resolve the timezone from the request (default to UTC when absent)
    // BEFORE anything opens a tracker entry. This is request validation
    // like the offset check above, and its `?` return has no outcome path
    // to finish tracking through: opening the entry first would leave an
    // invalid timezone's id active forever, since nothing sweeps them.
    let utc_offset_secs = req
        .timezone
        .as_deref()
        .map(trawl_engine::timezone::resolve_utc_offset)
        .transpose()
        .map_err(ServerError::BadRequest)?
        .unwrap_or(0);

    // One id from the pool's counter keys both the tracker entry and the
    // pool's interrupt map, so cancel-by-id interrupts the query the client
    // sees. Allocated before query_start so every lifecycle event correlates
    // on query_id without carrying the query text.
    let query_id = state.query.pool.allocate_query_id();
    state.query.tracker.start(query_id, &verified, &req.query);

    // Default-filter lifecycle events carry metadata only — never the raw
    // DSL, which can contain customer identifiers or incident indicators.
    // Full text lives in authenticated history, the tracker, the opt-in
    // query debug log, and the DEBUG-only query_text event below.
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

    // Check for `| from saved` — if present, resolve to parquet source
    // and execute with the pre-computed source instead of normal glob scan.
    //
    // Admission runs over the pipeline the caller TYPED, before the
    // `from saved` stage is sliced off (ADR-0024): resolving first would
    // let an over-cap original be admitted as an under-cap suffix. Its
    // failure takes the same route the engine's own parse/emit failures
    // take below, so the lifecycle events and the query log are the ones
    // this shape has always produced.
    //
    // Resolution reads postgres, so it spends the caller's budget like any
    // other wait (ADR-0024) and its failures are the handler's, not the
    // caller's to catch: escaping by `?` here would leave the tracker entry
    // this request already opened running forever. Both land as a refusing
    // outcome and finish tracking through the one path below. Expiry is
    // the pre-start capacity refusal, since no work was ever started and a
    // timeout history row would claim otherwise.
    let admitted = match crate::admission::check_dsl(&req.query) {
        Err(refusal) => Err(refusal),
        Ok(()) => match deadline
            .run(try_resolve_from_saved(&state, &verified, &req.query))
            .await
        {
            Ok(resolved) => resolved,
            Err(crate::deadline::Expired) => Err(ServerError::ServiceUnavailable(
                crate::error::CAPACITY_NOT_STARTED.to_owned(),
            )),
        },
    };

    let (outcome, degraded_fields) = match admitted {
        Err(refusal) => (
            crate::pool::ExecuteOutcome {
                result: Err(refusal),
                debug: None,
                severity_columns: Vec::new(),
            },
            Vec::new(),
        ),
        Ok(Some(resolved)) => {
            // Both halves of what the caller is actually reading: the
            // stages they typed, and the saved query whose recorded run
            // produced the rows those stages run over. Nothing stamps a
            // report run at write time, so a degraded pin the saved query
            // bound would otherwise go unmentioned.
            let halves = [resolved.saved_dsl.as_str(), resolved.remaining_dsl.as_str()];
            let degraded = degraded_fields_for(&state, halves);
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
                        utc_offset_secs,
                        crate::pool::WorkContext::key(
                            crate::pool::WorkKind::FromSaved,
                            verified.id,
                        )
                        .with_user(&verified.name),
                    )
                    .await,
                degraded,
            )
        }
        Ok(None) => {
            let degraded = degraded_fields_for(&state, [req.query.as_str()]);
            (
                state
                    .query
                    .pool
                    .execute(
                        query_id,
                        &req.query,
                        deadline,
                        capture_debug,
                        utc_offset_secs,
                        crate::pool::WorkContext::key(crate::pool::WorkKind::Query, verified.id)
                            .with_user(&verified.name),
                    )
                    .await,
                degraded,
            )
        }
    };

    let duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
    let duration_secs = start.elapsed().as_secs_f64();

    // Which result columns render as OTel tokens, decided by the executing
    // task under the pins the rows were produced with, not by the catalog as
    // it stands now: history I/O and logging sit between execution and the
    // response, and a repin landing in that window must not retype the
    // answer's presentation. The walk is a client-shaped cost (a regex
    // compiled per `extract` stage), so it runs inside the query's permit on
    // the blocking pool, never here on a reactor thread (see
    // `crate::pool::severity_columns_for`).
    let severity_columns = outcome.severity_columns;

    match outcome.result {
        Ok(qr) => {
            let total = qr.row_count();
            let paginated = qr.paginate(offset, limit);
            let returned = paginated.row_count();

            state.query.tracker.complete(query_id, total);

            // Record the query in history under the authoritative fleet
            // keystore id. Best-effort: a history-store write failure must
            // not fail the query, but log it so a broken store (pg down,
            // constraint trouble) is visible.
            //
            // Under the request's own deadline (ADR-0024), because the
            // rows are already in hand: a wedged history store must not
            // hold a finished answer past the budget the caller was
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
                    tracing::warn!(
                        event_type = "history_error",
                        key_id = verified.id,
                        error = %e,
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
            }))
        }
        Err(ServerError::Timeout) => {
            state.query.tracker.timeout(query_id);

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
                None,
                duration_ms,
                Some("query timed out"),
            );

            Err(ServerError::Timeout)
        }
        Err(e) => {
            // SECURITY: use safe_message() to redact database internals
            // from tracker history and logs.
            let safe_msg = e.safe_message();
            state.query.tracker.fail(query_id, &safe_msg);

            metrics::counter!(crate::metrics::QUERIES_TOTAL, "status" => "error").increment(1);
            metrics::histogram!(crate::metrics::QUERY_DURATION).record(duration_secs);

            // Default-filter failure events carry the class only. Neither
            // `safe_msg` nor the raw error is safe to persist here:
            // safe_message() deliberately preserves parser/emitter text
            // (which quotes the user's own tokens and format strings), and
            // the raw database error embeds the generated SQL plus the
            // values it choked on. Both live on in the client response, the
            // tracker, the opt-in query debug log, and the DEBUG event below.
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
                None,
                duration_ms,
                Some(&safe_msg),
            );

            Err(e)
        }
    }
}

/// `GET /metrics` — prometheus scrape endpoint (unauthenticated).
///
/// Collects process metrics and trawl gauges on each scrape, then
/// renders the prometheus text exposition format. The gauge collection
/// (which may walk the filesystem on cache miss) runs in `spawn_blocking`
/// to avoid stalling the async executor.
pub async fn prometheus_metrics(State(state): State<AppState>) -> impl IntoResponse {
    let hot_buffer = state.query.hot_buffer.clone();
    let fallback_glob = state.query.pool.fallback_glob().to_owned();
    // Read here, on the reactor: the closure below deliberately holds no
    // pool state, so the registry lock never travels onto the blocking
    // pool behind a filesystem walk.
    let retained_permits = state.query.pool.retained();
    let wal_dir = state
        .ingest
        .wal_writer
        .as_ref()
        .map(|w| w.dir().to_path_buf());
    let _ = tokio::task::spawn_blocking(move || {
        #[cfg(target_os = "linux")]
        metrics_process::Collector::default().collect();
        crate::metrics::collect_gauges(
            hot_buffer.as_ref(),
            &fallback_glob,
            wal_dir.as_deref(),
            retained_permits,
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

/// `GET /api/v1/health` — unauthenticated health check with subsystem probes.
pub async fn health(State(state): State<AppState>) -> (StatusCode, Json<HealthResponse>) {
    // Probe duckdb, the fleet keystore and the app-state store concurrently.
    //
    // `/health` is unauthenticated and unthrottled — it sits outside
    // `require_bearer_only` and `rate_limit_middleware` — so it must not amplify a
    // burst of probes onto the small, shared keystore pool that bearer
    // verification depends on. `AuthState::ping_cached` memoises the ping for a
    // few seconds and serialises refreshes, collapsing any burst into at most
    // one in-flight connection. It is also timeout-bounded, so a slow/downed
    // keystore reports an unhealthy auth subsystem (non-critical → `Degraded` →
    // HTTP 200) instead of stalling this liveness path.
    let pool = state.query.pool.clone();
    let (duckdb_join, auth_result, storage_result) = tokio::join!(
        tokio::spawn(async move { pool.ping().await }),
        state.auth.ping_cached(),
        state.storage.ping_cached(),
    );

    // Data path: check that the base directory exists and is readable.
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

    let duckdb_ok = duckdb_join
        .map_err(|e| format!("task join error: {e}"))
        .and_then(|r| r.map_err(|e| e.to_string()));

    let mut checks = HashMap::with_capacity(4);
    let duckdb_healthy = duckdb_ok.is_ok();
    let auth_healthy = auth_result.is_ok();
    let storage_healthy = storage_result.is_ok();
    let data_healthy = data_result.is_ok();
    let check_value = |healthy| String::from(if healthy { "ok" } else { "error" });

    checks.insert("duckdb".into(), check_value(duckdb_healthy));
    checks.insert("auth_db".into(), check_value(auth_healthy));
    checks.insert("storage_db".into(), check_value(storage_healthy));
    checks.insert("data_path".into(), check_value(data_healthy));

    metrics::gauge!("trawl_health_check", "subsystem" => "duckdb").set(if duckdb_healthy {
        1.0
    } else {
        0.0
    });
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

    let status = derive_health_status(duckdb_healthy, auth_healthy, storage_healthy, data_healthy);
    let http_status = match status {
        HealthStatus::Ok | HealthStatus::Degraded => StatusCode::OK,
        HealthStatus::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
    };

    (
        http_status,
        Json(HealthResponse {
            status,
            checks: Some(checks),
            version: Some(trawl_core::version::PKG_VERSION.to_owned()),
        }),
    )
}

/// Derive overall health status from individual subsystem results.
///
/// - All green → `Ok`
/// - Any non-critical (`auth_db`, `storage_db`, `data_path`) fails →
///   `Degraded` (HTTP 200 — the query path can still serve)
/// - Any critical (duckdb) fails → `Unavailable`
#[allow(clippy::fn_params_excessive_bools)] // subsystem flags, call sites are named
fn derive_health_status(
    duckdb_ok: bool,
    auth_ok: bool,
    storage_ok: bool,
    data_ok: bool,
) -> HealthStatus {
    if !duckdb_ok {
        return HealthStatus::Unavailable;
    }
    if !auth_ok || !storage_ok || !data_ok {
        return HealthStatus::Degraded;
    }
    HealthStatus::Ok
}

/// Query parameters for `GET /api/v1/schema`.
#[derive(Debug, Deserialize)]
pub struct SchemaParams {
    /// Only fields observed for this service.
    pub service: Option<String>,
    /// Lift the `last_seen` retention window (show aged-out fields too).
    pub all: Option<bool>,
}

/// `GET /api/v1/schema` — the data schema, served from the field catalog.
///
/// Columns are a `SELECT` over `field_types` LEFT JOIN `field_services`
/// (ADR-0009) — the write-time type authority, never a `DESCRIBE`.
/// By default fields whose most recent observation predates the retention
/// horizon (the longest age any env still keeps data for, per
/// [`crate::retention::maximum_enabled_age_secs`]; an env that keeps its
/// data forever lifts it) are hidden; `?all=true` lifts the window, and a
/// never-observed pin (e.g. the envelope seed) is always shown.
/// `?service=` scopes the listing to fields that service has carried.
///
/// Corpus facts (dates, sizes, services, file count) stay a TTL-cached
/// filesystem walk; `cached` reports whether those came from the cache,
/// not the columns.
///
/// The unscoped column set is TTL-cached too (same TTL): it aggregates
/// `field_services` across every service, and the service axis is
/// client-chosen and unbounded while this endpoint is what autocomplete
/// polls. A `?service=` listing is served straight from postgres — it is
/// bounded by the pin cap through `field_services (service, field)`, and
/// caching per client-chosen service name would be an unbounded cache.
pub async fn schema(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Query(params): Query<SchemaParams>,
) -> Result<Json<SchemaResponse>, ServerError> {
    if !verified.has_permission(Permission::SchemaRead) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }

    // Hot buffer stats are cheap atomics — always read fresh (never cached).
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
        //
        // A request whose SELECT began before a flip may still return
        // pre-flip columns in its own response (its postgres snapshot
        // legitimately predates the commit, and the corpus-facts walk below
        // can delay that response's arrival) — ordinary concurrent-read
        // semantics; the stale entry it stamps cannot be served to any
        // later request.
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
            let fallback_glob = state.query.pool.fallback_glob().clone();
            let (earliest_date, latest_date, total_bytes, services, file_count) =
                tokio::task::spawn_blocking(move || collect_catalog_metadata(&fallback_glob))
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

/// The `/api/v1/schema` column set: the catalog listing, mapped to the wire
/// type and sorted into query-result display order (envelope first,
/// metadata last, custom fields alphabetical in between).
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

/// Read hot buffer event count and byte size (cheap atomic loads).
fn hot_buffer_stats(state: &AppState) -> (Option<u64>, Option<u64>) {
    state.query.hot_buffer.as_ref().map_or((None, None), |buf| {
        (
            Some(buf.event_count() as u64),
            Some(buf.byte_count() as u64),
        )
    })
}

/// Parse parquet file paths to extract corpus facts: earliest/latest date,
/// total bytes, distinct services, and the parquet file count.
///
/// Path structure: `{base}/{env}/{YYYY-MM-DD}/{HH}/{service}.parquet`, or
/// `{base}/{env}/{YYYY-MM-DD}/{service}.parquet` for a daily rollup — hence
/// the ancestor walk for the date component rather than a fixed depth.
fn collect_catalog_metadata(
    fallback_glob: &str,
) -> (Option<String>, Option<String>, u64, Vec<String>, u64) {
    use std::collections::BTreeSet;
    use std::path::Path;

    let base = fallback_glob
        .find('*')
        .map_or(fallback_glob, |pos| &fallback_glob[..pos]);
    let base = Path::new(base.trim_end_matches('/'));

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
                && is_date_dir(name)
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

/// Check if a directory name looks like YYYY-MM-DD.
fn is_date_dir(name: &str) -> bool {
    name.len() == 10
        && name.as_bytes()[4] == b'-'
        && name.as_bytes()[7] == b'-'
        && name[..4].bytes().all(|b| b.is_ascii_digit())
        && name[5..7].bytes().all(|b| b.is_ascii_digit())
        && name[8..10].bytes().all(|b| b.is_ascii_digit())
}

/// One unit of retained work as a given reader may see it (ADR-0024).
///
/// The id, the kind and the timing are facts about the server's capacity
/// and go to every reader the route already admits.
///
/// Interactive work, a query some key submitted, carries its user and
/// its DSL to every reader of this route, which is what `active` and
/// `recent` have always done for the same query. Retention is a phase of
/// one request's life, not a second visibility class: hiding the text here
/// while `recent` shows it for the same id would be an incoherence, not a
/// protection. `Permission::Query` is the gate on all three lists.
///
/// System work (a scheduled run, a health probe) is the exception: it has
/// no human owner and its DSL is the server's own, so only an operator
/// (`ServerManage`) sees its text. Owner ids never leave the server in
/// either case, and viewing is not cancellation. That authority is still
/// the exact keystore id (see `check_cancel_authority`).
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

/// May this key ask for that work to stop?
///
/// Admin (`ServerManage`) can cancel any work, including the server's own
/// (a scheduled run, a health probe). A `QueryCancel` holder can cancel
/// work it submitted, and ownership is the exact keystore id: display
/// names are mutable and non-unique, so two keys sharing a name must stay
/// isolated from each other. Keys holding neither permission are rejected
/// outright, with a different sentence — telling them nothing about
/// whether the id exists.
fn check_cancel_authority(
    verified: &VerifiedKey,
    tracker_owner: Option<i64>,
    pool_owner: Option<crate::pool::WorkOwner>,
) -> Result<(), ServerError> {
    if verified.has_permission(Permission::ServerManage) {
        return Ok(());
    }
    if !verified.has_permission(Permission::QueryCancel) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }
    let mine = tracker_owner == Some(verified.id)
        || pool_owner == Some(crate::pool::WorkOwner::Key(verified.id));
    if mine {
        Ok(())
    } else {
        Err(ServerError::Forbidden("cannot cancel this query".into()))
    }
}

/// `DELETE /api/v1/queries/{id}` — cancel a running query by ID.
///
/// Admin (`ServerManage`) can cancel any query. `QueryCancel` holders can cancel
/// their own queries only. Keys without `QueryCancel` are rejected outright.
pub async fn cancel_query(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Path(query_id): Path<u64>,
) -> Result<Json<CancelResponse>, ServerError> {
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
/// Performs syntax and semantic validation (function names, arity, regex patterns)
/// but does not check field existence (which would require schema introspection).
///
/// This route already runs the two halves [`crate::admission::check_dsl`]
/// runs, in the same order and from the same owners — so an over-budget
/// pipeline reports ADR-0024's sentence here too. It reports it as this
/// route's own answer (`valid: false` with the refusal as a detail)
/// rather than as an error status: telling a client its query is invalid
/// IS the successful outcome of a validation call, and routing the
/// refusal through `check_dsl` would turn every parse error into a 400.
pub async fn validate_query(
    Extension(verified): Extension<VerifiedKey>,
    Json(req): Json<QueryRequest>,
) -> Result<Json<ValidationResponse>, ServerError> {
    if !verified.has_permission(Permission::Validate) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }

    let ast = match trawl_core::parser::parse(&req.query) {
        Ok(ast) => ast,
        Err(errors) => {
            return Ok(Json(ValidationResponse {
                valid: false,
                errors: errors
                    .iter()
                    .map(crate::error::parse_error_to_detail)
                    .collect(),
                formatted: None,
            }));
        }
    };

    if let Err(e) = trawl_core::emitter::validate_pipeline(&ast.pipeline) {
        return Ok(Json(ValidationResponse {
            valid: false,
            errors: e
                .to_parse_errors(req.query.len())
                .iter()
                .map(crate::error::parse_error_to_detail)
                .collect(),
            formatted: None,
        }));
    }

    let formatted = trawl_core::format::format_query(&ast);
    Ok(Json(ValidationResponse {
        valid: true,
        errors: vec![],
        formatted: Some(formatted),
    }))
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
///
/// Available to any authenticated user. No permission check needed — if the
/// token passed auth middleware, the user is entitled to know their own identity and grants.
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

/// `GET /api/v1/dashboard` — full dashboard snapshot (admin only).
///
/// Returns the latest [`DashboardSnapshot`] collected by the background
/// snapshot collector. Available even when the terminal monitor is disabled.
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
/// Per-connection interval loop over the snapshot cache refreshed by the
/// background collector — no dedicated push channel. Each snapshot is
/// emitted as a named `stats` SSE event. Before the collector's first tick
/// the loop silently skips (never a 503 mid-stream: `EventSource` treats a
/// non-200 as terminal, whereas an empty wait of ≤1s self-heals).
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

    // Bound concurrent dashboard streams. The owned permit is held by the
    // stream future and auto-released when the client disconnects.
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
///
/// Pure cache read (two in-process caches, no postgres and no file I/O).
/// Returns 503 if the background refresh hasn't completed yet.
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

/// Format a UTC instant as the wire's ISO 8601 string.
fn iso8601(dt: chrono::DateTime<chrono::Utc>) -> String {
    dt.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

/// Convert a `?since_secs=` window into an absolute instant.
///
/// `since_secs` is request-controlled, so every step here is total:
/// `TimeDelta::seconds` and `DateTime - TimeDelta` are both panicking
/// constructors that a large enough query string reaches. Any window that
/// reaches past the unix epoch saturates there — older than any row a
/// catalog can hold, so the filter still means "everything", and the value
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

/// Query parameters for `GET /api/v1/schema/fields`.
#[derive(Debug, Deserialize)]
pub struct CatalogFieldsParams {
    /// Only fields observed for this service.
    pub service: Option<String>,
    /// Only fields observed within the last N seconds.
    pub since_secs: Option<u64>,
    /// Maximum fields returned (default 500, clamped to the pin cap).
    pub limit: Option<i64>,
}

/// `GET /api/v1/schema/fields` — the pinned-field listing with aggregated
/// observation and conflict evidence (`trawl schema fields`).
pub async fn catalog_fields(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Query(params): Query<CatalogFieldsParams>,
) -> Result<Json<trawl_api::CatalogFieldsResponse>, ServerError> {
    if !verified.has_permission(Permission::SchemaRead) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }

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

/// The degraded fields the DSL binds, sorted — the incomplete-results
/// notice (ADR-0011).
///
/// `texts` is every DSL the answer depends on: the query as typed, plus —
/// for `from saved` — the saved query whose run produced the stored rows.
///
/// Reads the in-process set the schema-refresh tick maintains, because the
/// query path never touches postgres. The empty check comes first so a
/// healthy install pays a lock acquisition and nothing else, never a parse.
///
/// Fields bound, not fields returned: a `where` on a degraded field that
/// projects it away is exactly the incomplete case
/// ([`trawl_core::field_refs`]).
fn degraded_fields_for<'a>(
    state: &AppState,
    texts: impl IntoIterator<Item = &'a str>,
) -> Vec<String> {
    let snapshot = std::sync::Arc::clone(&state.query.degraded_fields.lock());
    if snapshot.fields.is_empty() {
        return Vec::new();
    }
    let mut named: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for dsl in texts {
        named.extend(trawl_core::field_refs::referenced_fields_in(dsl));
    }
    named
        .into_iter()
        .filter(|f| snapshot.fields.contains(f))
        .collect()
}

/// Read a pin's stored `DuckDB` spelling back as a canonical type.
///
/// The column is `CHECK`-constrained to the canonical spellings, so the
/// fallback is unreachable short of a hand-edited catalog; `VARCHAR` is the
/// honest answer there — it is the one pin under which nothing further can
/// be shelved.
fn current_pin(duckdb_type: &str) -> trawl_core::schema::CanonicalType {
    trawl_core::schema::CanonicalType::from_catalog(duckdb_type)
        .unwrap_or(trawl_core::schema::CanonicalType::Varchar)
}

/// Verdicts for whichever of `pins` the analyzer finds degraded (ADR-0011).
/// Absent from the map = healthy, which is the common case and costs one
/// aggregate read.
///
/// Two page-keyed queries, never a join into the listing SQL: the same trap
/// [`crate::store::CatalogStore::list_fields`] documents for its conflict
/// evidence applies here — a grouped subquery over the whole
/// `field_conflict_stats` table has no predicate a planner can push down,
/// and that table's service axis is client-chosen. The second query runs
/// only for the fields that came back degraded (usually none).
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

/// Query parameters for `GET /api/v1/schema/field`.
///
/// The field name travels as a query parameter, never a path segment: a
/// catalog key is any ASCII-folded client JSON key ≤255 bytes — it may
/// contain `/`, `?`, or `%`, which a path segment cannot carry reliably.
#[derive(Debug, Deserialize)]
pub struct CatalogFieldParams {
    /// Field name (ASCII-folded before lookup, mirroring ingest's fold).
    pub name: String,
    /// Maximum service observations returned
    /// (default [`DEFAULT_FIELD_SERVICES_LIMIT`], max
    /// [`MAX_FIELD_SERVICES_LIMIT`]).
    pub limit: Option<i64>,
    /// Opaque cursor from a previous response's `services_cursor`.
    pub after: Option<String>,
}

/// Default page size for the field detail's service observations.
const DEFAULT_FIELD_SERVICES_LIMIT: i64 = 100;

/// Hard ceiling for the field detail's `?limit=`.
///
/// `field_services` rows are ever-observed and their service axis is
/// client-chosen — a common envelope field accumulates one row per service
/// name a sender ever invented, none of which spends a pin slot. So the
/// detail's response size is capped here regardless of what the caller asks
/// for, and the rest is reached by paging.
const MAX_FIELD_SERVICES_LIMIT: i64 = 1000;

/// `GET /api/v1/schema/field?name=` — one field's pin, one page of its
/// per-service observations, and its retained conflict evidence
/// (`trawl schema field`).
pub async fn catalog_field(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Query(params): Query<CatalogFieldParams>,
) -> Result<Json<trawl_api::CatalogFieldResponse>, ServerError> {
    if !verified.has_permission(Permission::SchemaRead) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }

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

/// Render a stored acknowledgement onto the wire.
fn ack_to_wire(ack: crate::store::DegradedAck) -> trawl_api::FieldAck {
    trawl_api::FieldAck {
        acked_at: iso8601(ack.acked_at),
        acked_by: ack.acked_by,
        note: ack.note,
        evidence_through: u64::try_from(ack.evidence_through).unwrap_or(0),
    }
}

/// The field an ack route acts on, as a query parameter for the reason
/// [`CatalogFieldParams`] documents: a catalog key can carry `/`, `?` or `%`.
#[derive(Debug, Deserialize)]
pub struct FieldAckParams {
    /// Field name (ASCII-folded before lookup, mirroring ingest's fold).
    pub name: String,
}

/// Request body for `POST /api/v1/schema/field/ack`.
#[derive(Debug, Default, Deserialize)]
pub struct FieldAckRequest {
    /// Why the operator is accepting the pin as it stands. Optional, capped
    /// at [`MAX_ACK_NOTE_BYTES`], stored verbatim and never logged.
    #[serde(default)]
    pub note: Option<String>,
}

/// Byte cap for an acknowledgement note, mirroring the migration's CHECK.
///
/// Checked here as well as there so an over-long note is a 400 naming the
/// limit rather than a constraint violation surfacing as a store error, and
/// so the refusal costs no transaction.
const MAX_ACK_NOTE_BYTES: usize = 1024;

/// `POST /api/v1/schema/field/ack?name=` — acknowledge a degraded verdict
/// (issue #111). `SchemaWrite`-gated: it changes what every read surface
/// says about the field.
///
/// The ack covers the evidence that exists right now and no more, so the
/// badge returns the moment the pin shelves another batch. 404 = no such
/// pin; 409 = the field's evidence does not meet the degraded threshold, so
/// there is no verdict to acknowledge and installing a high-water would
/// swallow the evidence that raises the badge for the first time.
pub async fn ack_degraded_field(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Query(params): Query<FieldAckParams>,
    Json(req): Json<FieldAckRequest>,
) -> Result<axum::response::Response, ServerError> {
    if !verified.has_permission(Permission::SchemaWrite) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }
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
            // operator prose, and a durable log line is not where the
            // operator chose to put it. Whether they wrote one is the part
            // an audit reader needs.
            //
            // `advanced = false` is the interleaving where this call read
            // less evidence than a concurrent ack had already acknowledged:
            // the stored row keeps the other operator's name, note and
            // timestamp, and this event is the only record that the request
            // happened at all.
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
        // The refusal wears the same `{"error": {...}}` envelope every other
        // error on this API does; only the status distinguishes it.
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
/// re-raising the badge if the evidence still indicts the pin.
///
/// Idempotent: 204 whether or not a row was there, because "this field is
/// not acknowledged" is the state the caller asked for either way. Only an
/// unpinned field refuses (404) — the caller has the wrong name, which no
/// amount of retrying fixes.
pub async fn clear_degraded_field_ack(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Query(params): Query<FieldAckParams>,
) -> Result<axum::response::Response, ServerError> {
    if !verified.has_permission(Permission::SchemaWrite) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }
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

/// Query parameters for `GET /api/v1/schema/conflicts`.
#[derive(Debug, Deserialize)]
pub struct CatalogConflictsParams {
    /// Only conflicts for this field.
    pub field: Option<String>,
    /// Only conflicts from this service.
    pub service: Option<String>,
    /// Only conflicts recorded within the last N seconds.
    pub since_secs: Option<u64>,
    /// Maximum rows returned (default 100, max 1000).
    pub limit: Option<i64>,
}

/// Ceiling for `GET /api/v1/schema/conflicts` `?limit=`.
const MAX_CONFLICT_LIST_LIMIT: i64 = 1000;

/// `GET /api/v1/schema/conflicts` — the schema-health dashboard listing
/// (`trawl schema conflicts --last 7d`).
pub async fn catalog_conflicts(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Query(params): Query<CatalogConflictsParams>,
) -> Result<Json<trawl_api::CatalogConflictsResponse>, ServerError> {
    if !verified.has_permission(Permission::SchemaRead) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }

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
    Path(field): Path<String>,
    Query(params): Query<FieldValuesParams>,
) -> Result<Json<FieldValuesResponse>, ServerError> {
    if !verified.has_permission(Permission::SchemaRead) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }

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

    // Cache miss — sample from parquet.
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

/// Query parameters for the field values endpoint.
#[derive(Debug, Deserialize)]
pub struct FieldValuesParams {
    pub limit: Option<usize>,
    /// Optional service name to scope values to a single service's files.
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

    // The verified key carries the authoritative fleet keystore id.
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

/// Numeric query parameters for History's existing Axum/Serde admission.
///
/// Raw History admission selects only these pairs before invoking this schema.
#[derive(Debug, Deserialize)]
pub struct HistoryParams {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

/// Convert a [`HistoryEntry`] into a [`HistoryEntryResponse`].
///
/// Timestamps are `DateTime<Utc>` in the domain and RFC 3339 strings on the
/// wire — formatting happens here, at the handler boundary.
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

    // Single bulk-join statement: schedule + latest run + run count arrive
    // with the saved queries, so the round-trip count is independent of how
    // many saved queries the user has.
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
    Extension(verified): Extension<VerifiedKey>,
    Json(req): Json<CreateSavedRequest>,
) -> Result<Json<SavedQueryResponse>, ServerError> {
    if !verified.has_permission(Permission::SavedQuery) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }

    let key_id = verified.id;

    // Admit the DSL before it is stored (ADR-0024): a saved query is run
    // later by a schedule, and a row nothing can execute is worth
    // refusing at the one moment a human is there to read the reason.
    crate::admission::check_dsl(&req.query)?;

    // DuplicateName → 409, InvalidName → 400 via the StoreError table.
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
    Extension(verified): Extension<VerifiedKey>,
    Path(id): Path<i64>,
    Json(req): Json<UpdateSavedRequest>,
) -> Result<Json<SavedQueryResponse>, ServerError> {
    if !verified.has_permission(Permission::SavedQuery) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }

    let key_id = verified.id;

    // Same door as create, and before the store call, so a refused update
    // leaves the stored row exactly as it was. The transactional
    // schedule-window check inside `update_checked` is untouched: it
    // proves a different thing, under the saved-query row lock.
    crate::admission::check_dsl(&req.query)?;

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
    Path(id): Path<i64>,
) -> Result<Json<DeleteSavedResponse>, ServerError> {
    if !verified.has_permission(Permission::SavedQuery) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }

    let key_id = verified.id;

    // The store collects parquet result paths and deletes the rows in one
    // transaction (FK CASCADE wipes runs); we unlink the files after commit.
    let run_paths = state.storage.saved.delete(id, key_id).await?;

    cleanup_run_parquet_files(&state, &run_paths);

    Ok(Json(DeleteSavedResponse { deleted: true }))
}

/// Convert a [`SavedQuery`] into a [`SavedQueryResponse`] (without schedule).
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

/// Build a [`ScheduleResponse`] from a schedule with pre-fetched run stats.
fn build_schedule_response(
    schedule: &Schedule,
    latest_run: Option<ReportRun>,
    total_runs: u64,
) -> ScheduleResponse {
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
        total_runs,
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
        // (ADR-0018 ruling 14: an edit does not reset coverage, so
        // switching back resumes where it stopped), which is exactly why
        // reading the column alone would report a fixed-window or
        // query-mode schedule as covered up to some instant nothing
        // claims.
        covered_through: match schedule.window {
            Some(ScheduleWindow::SinceLast) => schedule.covered_through.map(format_window_bound),
            Some(ScheduleWindow::Fixed { .. }) | None => None,
        },
        next_fire_at: format_window_bound(schedule.next_fire_at),
    }
}

/// Build a [`ScheduleResponse`] from a bulk-join [`ScheduleWithStats`] row.
fn schedule_response_from_stats(stats: ScheduleWithStats) -> ScheduleResponse {
    build_schedule_response(&stats.schedule, stats.latest_run, stats.total_runs)
}

/// Convert a [`ReportRun`] into a [`ReportRunSummary`].
///
/// Timestamps are `DateTime<Utc>` in the domain and RFC 3339 strings on the
/// wire — formatting happens here, at the handler boundary.
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
    }
}

// -- repin handlers ----------------------------------------------------------

/// Wire shape of one repin job row.
///
/// `requires_force` asks the same force decision the two live gates ask
/// (`repin::force_refusal`), over this row's own persisted numbers: a dry
/// run terminates `succeeded` by design, so the verdict has to ride the
/// report or an operator learns about the refusal from the request that was
/// meant to do the work. Never a second condition, which would be free to
/// drift from the gate.
///
/// It is absent until the scan has recorded its plan (`planned_at`): a
/// claimed job's counts are zeros that mean "not measured yet", and a poll
/// in that window would otherwise read a confident `false` off a row that is
/// about to refuse. Absent means "not known yet", not "no".
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
    // The store validates the plan shape. An unfinished scan has no verdict.
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
/// `SchemaWrite`-gated: the one data-mutating schema action.
///
/// The HTTP status carries the verdict: 200 = dry-run report, 202 =
/// rewrite started (poll `/schema/repin/status`), 409 = the scan projected
/// nulled values and no force flag was passed — the body is the plan the
/// refusal is based on (a second concurrent repin also 409s, but with the
/// error envelope). Validation refusals (unpinned field, envelope field,
/// unknown target type, same-type without force, a dialect on a
/// non-`SEVERITY` target) are 400s; a query-only node answers 503 — it owns
/// nothing under the data root.
pub async fn schema_repin(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Json(req): Json<trawl_api::RepinRequest>,
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

    let outcome = engine
        .start(
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
            Some(&verified.name),
        )
        .await?;
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

/// `GET /api/v1/schema/repin/status` — the running job if any, else the
/// newest job of any status. `SchemaRead`-gated on purpose (ADR-0011:
/// read-only surfaces show state without offering the trigger), and served
/// on query-only nodes too — the job rows live in postgres.
pub async fn schema_repin_status(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
) -> Result<Json<trawl_api::RepinStatusResponse>, ServerError> {
    if !verified.has_permission(Permission::SchemaRead) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }
    let job = state.storage.repin.latest().await?;
    Ok(Json(trawl_api::RepinStatusResponse {
        job: job.map(repin_job_to_wire),
    }))
}

/// `POST /api/v1/schema/gc-pins` reclaims pin slots held by fields
/// nothing writes any more (#110). `SchemaWrite`-gated, like the repin
/// trigger: both mutate the catalog, and neither is a read.
///
/// Dry and real runs both answer 200 with the same report; `dry_run` and
/// `deleted` tell them apart. A refusal is a 409 through the ordinary
/// error envelope: a repin owns the data root, or the corpus could not be
/// read well enough to prove any pin dead. A query-only node answers 503,
/// because proving a pin dead means reading parquet footers and it owns
/// none.
///
/// The engine's `run` spawns its own task internally (a disconnect must
/// not split the postgres commit from the cache eviction), so this handler
/// awaits it directly rather than spawning a second time.
pub async fn schema_gc_pins(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Json(req): Json<trawl_api::GcPinsRequest>,
) -> Result<Json<trawl_api::GcPinsResponse>, ServerError> {
    if !verified.has_permission(Permission::SchemaWrite) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }
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

// -- schedule handlers -------------------------------------------------------

/// Remove parquet files for deleted report runs (best-effort, logs warnings on failure).
fn cleanup_run_parquet_files(state: &AppState, relative_paths: &[String]) {
    if relative_paths.is_empty() {
        return;
    }
    let base = state.query.pool.base_dir();
    for path in relative_paths {
        crate::scheduler::remove_result_file(base, path);
    }
}

/// Try to resolve a `| from saved` stage from the DSL query.
///
/// Returns `Ok(Some(resolved))` if the query starts with `| from saved`,
/// `Ok(None)` if it's a normal query, or `Err(...)` if resolution fails
/// (e.g. saved query not found, no successful runs).
async fn try_resolve_from_saved(
    state: &AppState,
    verified: &VerifiedKey,
    dsl: &str,
) -> Result<Option<crate::from_saved::ResolvedFromSaved>, ServerError> {
    // Best-effort parse — if it fails, let the normal execution path
    // handle the parse error with proper diagnostics.
    let Ok(ast) = trawl_core::parser::parse(dsl) else {
        return Ok(None);
    };

    // Only intercept if the first pipe stage is `from saved`.
    let Some(from_saved) = ast.from_saved_stage() else {
        return Ok(None);
    };

    let key_id = verified.id;

    // The span end of the first pipeline stage tells us where to slice
    // the remaining DSL.
    let stage_span_end = ast.pipeline[0].span.end;

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

/// What the `lag` field accepts, spelled out for a rejection message.
const DURATION_GRAMMAR: &str = "a duration: a number and one of s, m, h, d, w";

/// What the `window` field accepts. The duration half carries the same 60s
/// floor as the schedule interval, which is why the store's own message
/// about a short one says "interval".
const WINDOW_GRAMMAR: &str = "\"since_last\" or a duration: a number and one of s, m, h, d, w";

/// Turn a duration-grammar rejection into a 400 that names the request
/// field and what that field would have taken.
///
/// The store's message says what was wrong with the value. What it cannot
/// say is which of two duration fields carried it, and an operator who gets
/// `invalid interval format: "11y"` back from a request with both a
/// `window` and a `lag` has to guess.
fn invalid_duration(field: &str, grammar: &str, e: &crate::store::StoreError) -> ServerError {
    ServerError::BadRequest(format!("invalid {field}: {e}; {field} takes {grammar}"))
}

/// `PUT /api/v1/saved/{id}/schedule` — create or update a schedule.
pub async fn set_schedule(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Path(saved_id): Path<i64>,
    Json(req): Json<SetScheduleRequest>,
) -> Result<Json<ScheduleResponse>, ServerError> {
    if !verified.has_permission(Permission::SavedQuery) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }

    let key_id = verified.id;
    let interval_secs = parse_interval(&req.interval)
        .map_err(|e| ServerError::BadRequest(format!("invalid interval: {e}")))?;
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

    // Freshly created/updated schedules can already have runs (updates);
    // fetch the stats the response carries.
    let (_, latest_run, total_runs) = state
        .storage
        .schedule
        .get_schedule_with_stats(saved_id, key_id)
        .await?
        .map_or((None, None, 0), |(s, lr, tr)| (Some(s), lr, tr));

    Ok(Json(build_schedule_response(
        &schedule, latest_run, total_runs,
    )))
}

/// `GET /api/v1/saved/{id}/schedule` — get schedule for a saved query.
pub async fn get_schedule(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Path(saved_id): Path<i64>,
) -> Result<Json<ScheduleResponse>, ServerError> {
    if !verified.has_permission(Permission::SavedQuery) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }

    let key_id = verified.id;

    let (schedule, latest_run, total_runs) = state
        .storage
        .schedule
        .get_schedule_with_stats(saved_id, key_id)
        .await?
        .ok_or_else(|| ServerError::NotFound("no schedule for this saved query".into()))?;

    Ok(Json(build_schedule_response(
        &schedule, latest_run, total_runs,
    )))
}

/// `DELETE /api/v1/saved/{id}/schedule` — delete a schedule.
pub async fn delete_schedule(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Path(saved_id): Path<i64>,
) -> Result<Json<DeleteScheduleResponse>, ServerError> {
    if !verified.has_permission(Permission::SavedQuery) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }

    let key_id = verified.id;

    // The store collects parquet result paths and deletes the schedule (and
    // its cascaded runs) in one transaction; we unlink files after commit.
    let run_paths = state
        .storage
        .schedule
        .delete_schedule(saved_id, key_id)
        .await?;

    cleanup_run_parquet_files(&state, &run_paths);

    Ok(Json(DeleteScheduleResponse { deleted: true }))
}

/// Query params for listing report runs.
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
    Path(saved_id): Path<i64>,
    Query(params): Query<ListRunsParams>,
) -> Result<Json<ListReportRunsResponse>, ServerError> {
    if !verified.has_permission(Permission::SavedQuery) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }

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

/// Global Runs parameters, separate from per-net history.
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

/// `POST /api/v1/saved/{id}/run` — trigger an immediate report run for a saved query.
///
/// Bypasses the scheduler interval check. Requires a schedule to be attached
/// (the run is stored under that schedule's history), and that schedule must
/// be in query mode: a windowed one owns what its reports cover, so a manual
/// run is a 409 naming the mode and the route that shows where coverage has
/// reached (ADR-0018 ruling 6). Returns the run summary immediately with
/// status "running" — execution continues in the background.
pub async fn trigger_run(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Path(saved_id): Path<i64>,
) -> Result<Json<ReportRunSummary>, ServerError> {
    if !verified.has_permission(Permission::SavedQuery) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }

    let key_id = verified.id;

    // One transaction: lock the saved query and read the DSL from it, lock
    // the schedule, refuse a coverage mode, enforce max_runs, claim the
    // run. The ownership check rides the first lock, so this handler takes
    // no snapshot of its own: what the run executes is what the claim
    // recorded. Concurrent triggers cannot exceed the cap or double-claim,
    // and a window added mid-request either lands before the lock (and
    // refuses this run) or waits behind it.
    let claimed = match state
        .storage
        .schedule
        .claim_manual_run(saved_id, key_id)
        .await?
    {
        ManualRunClaim::Started(claimed) => claimed,
        ManualRunClaim::NoSchedule => {
            return Err(ServerError::BadRequest(
                "attach a schedule before triggering a run".into(),
            ));
        }
        // 409, not 400: the request is well formed and will be fine again
        // if the operator drops the window. The message names the mode it
        // found and the route that answers "where has coverage reached",
        // which is what someone asking for a manual run actually wants.
        ManualRunClaim::CoverageMode(window) => {
            return Err(ServerError::Conflict(format!(
                "schedule uses coverage mode \"{window}\"; manual runs are disabled for \
                 windowed schedules; watch GET /api/v1/saved/{saved_id}/schedule \
                 (covered_through, next_fire_at)"
            )));
        }
        ManualRunClaim::MaxRunsReached => {
            return Err(ServerError::BadRequest(
                "max runs reached for this net".into(),
            ));
        }
        ManualRunClaim::AlreadyRunning => {
            return Err(ServerError::BadRequest(
                "a run is already in progress for this net".into(),
            ));
        }
    };

    // Return the summary immediately, execute in background.
    let summary = ReportRunSummary {
        id: claimed.run_id,
        query: claimed.query.clone(),
        status: RunStatus::Running.as_str().to_string(),
        started_at: chrono::Utc::now().to_rfc3339(),
        finished_at: None,
        duration_ms: None,
        row_count: None,
        error_message: None,
        result_path: None,
        // A manual run is query mode by construction: a schedule that owns
        // a window refuses one (ADR-0018 ruling 6), so there are no bounds
        // to report here.
        window_start: None,
        window_end: None,
        window_truncated: None,
        window_kind: None,
    };

    let schedule_store = state.storage.schedule.clone();
    let pool = state.query.pool.clone();
    let run_id = claimed.run_id;
    let query = claimed.query;
    let query_name = claimed.query_name;
    let timeout_secs = state.query.timeout_secs;

    tokio::spawn(async move {
        execute_scheduled_query(
            schedule_store,
            pool,
            run_id,
            &query,
            &query_name,
            0,
            timeout_secs,
        )
        .await;
    });

    Ok(Json(summary))
}

/// `GET /api/v1/saved/{id}/runs/{run_id}` — get a single report run with result data.
///
/// Prefers parquet result files (via `result_path`) over the zstd JSON blob.
/// A run has a blob instead of a file in two cases: its result had no rows
/// (there is no schema to write a parquet from), or the parquet write failed
/// and the scheduler fell back. Either way the blob carries the column names,
/// so a zero-row run answers with its columns and an empty row list rather
/// than a null result.
pub async fn get_report_run(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Path((saved_id, run_id)): Path<(i64, i64)>,
) -> Result<Json<ReportRunResponse>, ServerError> {
    if !verified.has_permission(Permission::SavedQuery) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }

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

    // Pre-fetch the blob (cheap if NULL in db). A genuine absence is `Ok(None)`;
    // a StoreError here is a live db fault and must surface as 5xx, not empty result.
    let result_blob = state
        .storage
        .schedule
        .get_run_result(run_id, key_id)
        .await?;

    // Try parquet result first, fall back to the zstd blob.
    let result = if let Some(ref result_path) = run.result_path {
        let base_dir = state.query.pool.base_dir().to_owned();
        let max_rows = state.query.pool.max_result_rows();
        let full_path = format!("{}/{}", base_dir.trim_end_matches('/'), result_path);
        let path = std::path::PathBuf::from(full_path);

        match tokio::task::spawn_blocking(move || {
            let executor = trawl_engine::executor::Executor::new()?;
            executor.read_parquet_to_result(&path, max_rows)
        })
        .await
        {
            Ok(Ok(qr)) => Some(qr),
            Ok(Err(e)) => {
                tracing::warn!(
                    event_type = "report_run_parquet_read_failed",
                    run_id,
                    error = %e,
                    "failed to read parquet result, trying the result blob"
                );
                crate::scheduler::decode_result_blob(result_blob)
            }
            Err(e) => {
                tracing::warn!(
                    event_type = "report_run_parquet_task_failed",
                    run_id,
                    error = %e,
                    "parquet read task panicked, trying the result blob"
                );
                crate::scheduler::decode_result_blob(result_blob)
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
///
/// Bypasses `max_result_rows` in favor of `max_export_rows` to support larger downloads.
#[allow(clippy::too_many_lines)]
pub async fn export(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Query(params): Query<ExportParams>,
    Json(req): Json<ExportRequest>,
) -> Result<impl IntoResponse, ServerError> {
    if !verified.has_permission(Permission::Export) {
        return Err(ServerError::Forbidden("insufficient permissions".into()));
    }

    // One absolute budget for the export, stamped at authenticated entry
    // like the query handler's (ADR-0024).
    let deadline =
        crate::deadline::Deadline::after(std::time::Duration::from_secs(state.query.timeout_secs));

    let format = params.format.unwrap_or(trawl_api::ExportFormat::Csv);

    let max_export_rows = state.query.max_export_rows;
    let limit = req.limit.unwrap_or(max_export_rows).min(max_export_rows);

    // One id keys the whole export lifecycle and the pool's interrupt map,
    // so the events correlate without carrying the query text.
    let query_id = state.query.pool.allocate_query_id();

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

    // One admission door for every lane (ADR-0024): the parquet export
    // takes a different route through the pool than CSV and JSON do, so
    // asking here is what makes all three refuse the same text.
    crate::admission::check_dsl(&req.query)?;

    let start = std::time::Instant::now();

    // Parquet export uses DuckDB's native COPY TO — no need to materialize
    // the result set in memory.
    if format == trawl_api::ExportFormat::Parquet {
        let bytes = state
            .query
            .pool
            .export_parquet(
                query_id,
                &req.query,
                limit,
                deadline,
                crate::pool::WorkContext::key(crate::pool::WorkKind::Export, verified.id)
                    .with_user(&verified.name),
            )
            .await?;
        let duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

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

    // CSV and JSON exports: execute query and render in memory.
    let capture_debug = state.query.query_log.is_some();
    // Exports use UTC — timezone conversion is a display concern for
    // interactive queries, not bulk data exports.
    let outcome = state
        .query
        .pool
        .execute(
            query_id,
            &req.query,
            deadline,
            capture_debug,
            0,
            crate::pool::WorkContext::key(crate::pool::WorkKind::Export, verified.id)
                .with_user(&verified.name),
        )
        .await;
    let duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

    let result = match outcome.result {
        Ok(qr) => qr,
        Err(e) => {
            let error_msg = e.safe_message();
            write_query_log(
                &state,
                &verified,
                &req.query,
                outcome.debug.as_ref(),
                None,
                duration_ms,
                Some(&error_msg),
            );
            return Err(e);
        }
    };

    let limited = result.paginate(0, limit);

    let (content_type, filename, body) = match format {
        trawl_api::ExportFormat::Csv => (
            "text/csv; charset=utf-8".to_owned(),
            "attachment; filename=\"export.csv\"".to_owned(),
            generate_csv(&limited).into_bytes(),
        ),
        trawl_api::ExportFormat::Json => (
            "application/x-ndjson".to_owned(),
            "attachment; filename=\"export.ndjson\"".to_owned(),
            generate_ndjson(&limited).into_bytes(),
        ),
        trawl_api::ExportFormat::Parquet => {
            return Err(ServerError::Internal(
                "parquet export reached unexpected code path".into(),
            ));
        }
    };

    tracing::info!(
        event_type = "export_complete",
        user = %verified.name,
        query_id,
        query_len = req.query.len(),
        %format,
        rows = limited.row_count(),
        bytes = body.len(),
        duration_ms,
        "export complete"
    );

    write_query_log(
        &state,
        &verified,
        &req.query,
        outcome.debug.as_ref(),
        Some(&limited),
        duration_ms,
        None,
    );

    Ok((
        [
            (header::CONTENT_TYPE, content_type),
            (header::CONTENT_DISPOSITION, filename),
        ],
        body,
    ))
}

/// Query parameters for the export endpoint.
#[derive(Debug, Deserialize)]
pub struct ExportParams {
    pub format: Option<trawl_api::ExportFormat>,
}

// -- query debug log helpers -------------------------------------------------

/// Maximum number of sample rows included in a query log entry.
const QUERY_LOG_SAMPLE_SIZE: usize = 5;

/// Write a query debug log entry if the query log is active.
///
/// No-op when `state.query.query_log` is `None`. Infallible — errors
/// are logged via tracing and never propagated.
#[allow(clippy::too_many_arguments)]
fn write_query_log(
    state: &AppState,
    verified: &VerifiedKey,
    dsl: &str,
    debug: Option<&PoolDebugInfo>,
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
        timing_ms: TimingDebug {
            pool_wait: debug.pool_wait_ms,
            total: duration_ms,
        },
        error: error.map(String::from),
    };

    log.write(&entry);
}

/// Build a result debug summary with column names and sample rows.
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
                .map(|(col, val)| (col.clone(), value_to_json(val)))
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

/// Convert a trawl `Value` to a `serde_json::Value` for debug log output.
fn value_to_json(value: &Value) -> serde_json::Value {
    match value {
        Value::Null => serde_json::Value::Null,
        Value::Boolean(b) => serde_json::Value::Bool(*b),
        Value::Integer(i) => serde_json::json!(i),
        Value::Float(f) => serde_json::json!(f),
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Array(arr) => serde_json::Value::Array(arr.iter().map(value_to_json).collect()),
    }
}

/// Generate RFC 4180-compliant CSV from query results.
///
/// Quotes fields containing commas, newlines, or quotes.
/// Escapes quotes by doubling them.
///
/// Uses `Cow<str>` internally to avoid allocations when no
/// transformation is needed (the common case for most cells).
fn generate_csv(result: &QueryResult) -> String {
    // Pre-allocate: ~50 bytes per cell average.
    let estimated = result.rows.len() * result.columns.len() * 50;
    let mut csv = String::with_capacity(estimated);

    // Header row.
    for (i, col) in result.columns.iter().enumerate() {
        if i > 0 {
            csv.push(',');
        }
        csv.push_str(&quote_csv_field(&col.name));
    }
    csv.push('\n');

    // Data rows.
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

/// Generate newline-delimited JSON from query results.
///
/// Each row is serialized as a JSON object with column names as keys.
/// Values use the custom `Serialize` impl on `Value` which maps directly
/// to JSON primitives.
fn generate_ndjson(result: &QueryResult) -> String {
    let mut buf = String::new();
    for row in &result.rows {
        let mut map = serde_json::Map::with_capacity(result.columns.len());
        for (col, val) in result.columns.iter().zip(row.iter()) {
            map.insert(
                col.name.clone(),
                serde_json::to_value(val).expect("Value serialization is infallible"),
            );
        }
        // serde_json::to_string on a Map is infallible for our Value types.
        buf.push_str(&serde_json::to_string(&map).expect("Map serialization is infallible"));
        buf.push('\n');
    }
    buf
}

/// Quote a CSV field if it contains special characters (comma, newline, quote).
/// Escape quotes by doubling them. Returns borrowed when no quoting needed.
fn quote_csv_field(s: &str) -> Cow<'_, str> {
    let needs_quoting = s.contains(',') || s.contains('\n') || s.contains('"');

    if needs_quoting || s.is_empty() {
        let escaped = s.replace('"', "\"\"");
        Cow::Owned(format!("\"{escaped}\""))
    } else {
        Cow::Borrowed(s)
    }
}

/// Convert a `Value` to a string for CSV export.
/// Returns borrowed for string values that don't need sanitization.
fn value_to_string(value: &Value) -> Cow<'_, str> {
    match value {
        Value::Null => Cow::Borrowed(""),
        Value::Boolean(b) => Cow::Owned(b.to_string()),
        Value::Integer(i) => Cow::Owned(i.to_string()),
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

/// Build an SSE snapshot event from the current aggregation state,
/// applying post-stages to each row.
///
/// `ctx` is the one instant this snapshot evaluates `now()` at, for every
/// post-stage row alike (ADR-0017 §3) — a distinct type from the per-event
/// context the feeding loop samples, so the two cannot be swapped at a call
/// site. The caller samples it at the start of the snapshot attempt, before
/// any row is taken, so the sample point does not depend on how many rows
/// survive.
fn emit_agg_snapshot(
    aggregation: &trawl_core::stream::CompiledAggregation,
    post_stages: &mut [trawl_core::stream::CompiledStage],
    ctx: &trawl_core::stream::SnapshotContext,
) -> Event {
    let (columns, rows) = trawl_core::stream::emit_snapshot(aggregation, post_stages, ctx);
    let rows: Vec<_> = rows.into_iter().map(trawl_core::row::to_json).collect();

    let payload = serde_json::json!({
        "columns": columns,
        "rows": rows,
    });
    Event::default().event("snapshot").data(payload.to_string())
}

/// The snapshot instant, sampled here and nowhere deeper: trawl-core takes
/// a context as data and never reaches for a clock.
fn snapshot_context() -> trawl_core::stream::SnapshotContext {
    trawl_core::stream::SnapshotContext::new(trawl_core::context::EvalContext::capture())
}

/// `GET /api/v1/stream` — stream live events via Server-Sent Events (SSE).
///
/// Subscribes to the event bus and filters incoming events in-memory
/// using [`trawl_core::filter::CompiledFilter`]. Each matching event is streamed
/// as an SSE `data` event. Requires ingest to be enabled (event bus available).
#[allow(clippy::too_many_lines)]
pub async fn stream_query(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Query(params): Query<StreamParams>,
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

    // Bound concurrent SSE connections. The owned permit is held by the
    // stream future and auto-released when the client disconnects.
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

    // The shared admission door first (ADR-0024), so a live tail and a
    // batch query refuse the same text with the same sentence rather than
    // with whichever refusal their own compiler reaches first.
    crate::admission::check_dsl(&query_dsl)?;

    // Parse and compile the filter once upfront.
    let ast = trawl_core::parser::parse(&query_dsl)
        .map_err(|errors| ServerError::BadRequest(format!("{errors:?}")))?;
    // Rejects whatever the SQL emitter rejects (e.g. `_severity=eror`)
    // instead of opening a live-looking stream that can never match an
    // event. One catalog snapshot feeds both the search-stage filter and the
    // pipeline plan, so /query and /stream cannot disagree on a pinned
    // comparison and the stream cannot disagree with itself (ADR-0011). The
    // snapshot is held for the stream's life, so a mid-stream repin only
    // takes effect on reconnect.
    let pin_snapshot = state.query.field_catalog.all();
    let filter = trawl_core::filter::CompiledFilter::compile(&ast.search, &pin_snapshot)
        .map_err(|e| ServerError::BadRequest(e.to_string()))?;

    // Compile the pipeline stages for streaming evaluation, with the same
    // snapshot as the pin scope's root: where/let adopt the catalog.
    let stream_plan = trawl_core::stream::compile_stream_plan(
        &ast.pipeline,
        &trawl_core::pin_scope::PinScope::root(&pin_snapshot),
    )
    .map_err(|e| ServerError::BadRequest(e.to_string()))?;

    let Some(ref bus) = state.ingest.event_bus else {
        return Err(ServerError::BadRequest(
            "streaming requires ingest to be enabled".into(),
        ));
    };

    let mut subscriber = EventBus::subscribe(bus.as_ref());

    let event_stream: std::pin::Pin<
        Box<dyn tokio_stream::Stream<Item = Result<Event, Infallible>> + Send>,
    > = Box::pin(async_stream::stream! {
        let _permit = sse_permit; // hold until stream ends

        match stream_plan {
            trawl_core::stream::StreamPlan::PassThrough(mut stages) => {
                let mut stream_done = false;

                loop {
                    if stream_done {
                        break;
                    }

                    match subscriber.recv().await {
                        Ok(batch) => {
                            for event in &batch.events {
                                // One instant per event (ADR-0017 §3),
                                // sampled here and handed to the single
                                // door that owns both the search-stage
                                // window and the pipeline stages, so the
                                // filter and the `now()` in a `| where`
                                // cannot read different clocks.
                                let ctx = trawl_core::context::EvalContext::capture();
                                match trawl_core::stream::accept_event(
                                    &filter,
                                    &mut stages,
                                    event,
                                    &ctx,
                                ) {
                                    trawl_core::stream::LiveOutcome::Emit(row) => {
                                        let json = serde_json::to_string(
                                            &trawl_core::row::to_json(row),
                                        )
                                        .unwrap_or_default();
                                        yield Ok(Event::default().event("data").data(json));
                                    }
                                    trawl_core::stream::LiveOutcome::Filtered => {}
                                    trawl_core::stream::LiveOutcome::Done => {
                                        stream_done = true;
                                        break;
                                    }
                                }
                            }
                        }
                        Err(crate::bus::RecvError::Lagged(n)) => {
                            tracing::warn!(
                                event_type = "stream_lagged",
                                missed = n,
                                "stream subscriber fell behind"
                            );
                            let payload = serde_json::json!({ "missed": n }).to_string();
                            yield Ok(Event::default().event("lagged").data(payload));
                        }
                        Err(crate::bus::RecvError::Closed) => {
                            break;
                        }
                    }
                }
            }
            trawl_core::stream::StreamPlan::Aggregate {
                mut pre_stages,
                mut aggregation,
                mut post_stages,
            } => {
                // Emit aggregation snapshots periodically: every 500ms or
                // after 100 matching events, whichever comes first.
                const SNAPSHOT_INTERVAL: std::time::Duration =
                    std::time::Duration::from_millis(500);
                const SNAPSHOT_EVENT_THRESHOLD: u64 = 100;

                let mut events_since_snapshot: u64 = 0;
                let mut snapshot_timer = tokio::time::interval(SNAPSHOT_INTERVAL);
                snapshot_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                // Skip the first immediate tick.
                snapshot_timer.tick().await;

                loop {
                    tokio::select! {
                        result = subscriber.recv() => {
                            match result {
                                Ok(batch) => {
                                    for event in &batch.events {
                                        // Same per-event instant, same
                                        // single door: the filter, the
                                        // pre-stages and the timechart
                                        // bucket's absent-`_time`
                                        // fallback all read it.
                                        let ctx =
                                            trawl_core::context::EvalContext::capture();
                                        if trawl_core::stream::accept_event_into_aggregate(
                                            &filter,
                                            &mut pre_stages,
                                            &mut aggregation,
                                            event,
                                            &ctx,
                                        ) {
                                            events_since_snapshot += 1;
                                        }
                                    }

                                    if events_since_snapshot >= SNAPSHOT_EVENT_THRESHOLD {
                                        let snapshot = emit_agg_snapshot(
                                            &aggregation,
                                            &mut post_stages,
                                            &snapshot_context(),
                                        );
                                        yield Ok(snapshot);
                                        events_since_snapshot = 0;
                                        snapshot_timer.reset();
                                    }
                                }
                                Err(crate::bus::RecvError::Lagged(n)) => {
                                    tracing::warn!(
                                        event_type = "stream_lagged",
                                        missed = n,
                                        "stream subscriber fell behind"
                                    );
                                    let payload = serde_json::json!({ "missed": n }).to_string();
                                    yield Ok(Event::default().event("lagged").data(payload));
                                }
                                Err(crate::bus::RecvError::Closed) => {
                                    break;
                                }
                            }
                        }
                        _ = snapshot_timer.tick() => {
                            // Time-based snapshot: only emit if new events arrived.
                            if events_since_snapshot > 0 {
                                let snapshot = emit_agg_snapshot(
                                    &aggregation,
                                    &mut post_stages,
                                    &snapshot_context(),
                                );
                                yield Ok(snapshot);
                                events_since_snapshot = 0;
                            }
                        }
                    }
                }
            }
        }
    });

    Ok(Sse::new(event_stream).keep_alive(KeepAlive::default()))
}

/// Query parameters for the stream endpoint.
#[derive(Debug, Deserialize)]
pub struct StreamParams {
    /// The DSL query to filter live events.
    pub query: String,
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// A key holding exactly `permissions`, under a display name two
    /// keys are free to share.
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

    /// An interactive query shows the same user and DSL to every reader
    /// of this route, whichever key submitted it (ADR-0024).
    ///
    /// That is what `active` and `recent` already do for the same id, and
    /// a retained entry is the same request one phase later: a reader who
    /// can watch a query run, and read its text in history when it timed
    /// out, learns nothing new from the retained line. Cancellation is the
    /// authority that stays keyed to the exact id.
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

    /// System work (a scheduled run, a health probe) has no human owner,
    /// so its DSL reaches an operator and nobody else. No `query`
    /// permission holder can ever match `WorkOwner::System`.
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

    /// Wire shape: an entry a reader may not see carries no null-valued
    /// keys at all, so the absence is not a field a client must special
    /// case. System work below `ServerManage` is the entry that hides
    /// anything.
    #[test]
    fn a_filtered_retained_entry_omits_the_fields_it_hides() {
        let work = retained_query(crate::pool::WorkOwner::System);
        let snap = retained_snapshot(&work, &viewer(8, "twin", &["query"]));
        let json = serde_json::to_string(&snap).expect("serialize");
        assert!(!json.contains("\"user\":"), "{json}");
        assert!(!json.contains("\"query\":"), "{json}");
        assert!(json.contains("\"retained_ms\":1500"), "{json}");
    }

    /// Cancellation authority, unchanged by the retained lifecycle
    /// (ADR-0024): an operator reaches everything including the server's
    /// own work, a `QueryCancel` holder reaches only what its own key id
    /// owns, and a key with neither permission is told nothing about the
    /// id at all.
    #[test]
    fn cancel_authority_is_the_key_id_and_admin_reaches_system_work() {
        use crate::pool::WorkOwner;

        let admin = viewer(99, "ops", &["query", "server_manage"]);
        let owner = viewer(7, "twin", &["query", "query_cancel"]);
        let twin = viewer(8, "twin", &["query", "query_cancel"]);
        let reader = viewer(9, "reader", &["query"]);

        // Work still tracked, and work whose request already answered:
        // either record proves ownership.
        assert!(check_cancel_authority(&owner, Some(7), None).is_ok());
        assert!(check_cancel_authority(&owner, None, Some(WorkOwner::Key(7))).is_ok());

        let denied = check_cancel_authority(&twin, Some(7), Some(WorkOwner::Key(7)))
            .expect_err("a twin name is not ownership");
        assert!(matches!(&denied, ServerError::Forbidden(m) if m == "cannot cancel this query"));

        // System work has no human owner: no QueryCancel holder matches it.
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

    /// A retention config with a global age and per-env overrides.
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

        // An "effectively never" override is what an operator reaches for
        // instead of 0, and no value of it may panic the handler.
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
                trawl_engine::value::Column {
                    name: "host".to_owned(),
                },
                trawl_engine::value::Column {
                    name: "count".to_owned(),
                },
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
            columns: vec![trawl_engine::value::Column {
                name: "cmd".to_owned(),
            }],
            rows: vec![
                vec![Value::String("=evil()".to_owned())],
                vec![Value::String("safe".to_owned())],
            ],
        };
        let csv = generate_csv(&result);
        assert!(csv.contains("'=evil()"));
        assert!(csv.contains("safe"));
    }

    // -- health status derivation ────────────────────────────────────────

    #[test]
    fn health_all_ok() {
        assert_eq!(
            derive_health_status(true, true, true, true),
            HealthStatus::Ok
        );
    }

    #[test]
    fn health_auth_down_is_degraded() {
        assert_eq!(
            derive_health_status(true, false, true, true),
            HealthStatus::Degraded
        );
    }

    #[test]
    fn health_storage_down_is_degraded() {
        // App-state store loss is non-critical: queries still serve, so the
        // wire contract is Degraded + HTTP 200 (never a liveness failure).
        assert_eq!(
            derive_health_status(true, true, false, true),
            HealthStatus::Degraded
        );
    }

    #[test]
    fn health_data_path_down_is_degraded() {
        assert_eq!(
            derive_health_status(true, true, true, false),
            HealthStatus::Degraded
        );
    }

    #[test]
    fn health_all_noncritical_down_is_degraded() {
        assert_eq!(
            derive_health_status(true, false, false, false),
            HealthStatus::Degraded
        );
    }

    #[test]
    fn health_duckdb_down_is_unavailable() {
        assert_eq!(
            derive_health_status(false, true, true, true),
            HealthStatus::Unavailable
        );
    }

    #[test]
    fn health_all_down_is_unavailable() {
        assert_eq!(
            derive_health_status(false, false, false, false),
            HealthStatus::Unavailable
        );
    }

    #[test]
    fn health_duckdb_and_noncritical_down_is_unavailable() {
        assert_eq!(
            derive_health_status(false, false, true, true),
            HealthStatus::Unavailable
        );
    }
}
