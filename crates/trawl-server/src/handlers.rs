//! HTTP request handlers for the trawl API.

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, KeepAliveStream, Sse};
use axum::{Extension, Json};
use serde::Deserialize;
use std::borrow::Cow;
use std::convert::Infallible;
use trawl_api::{
    CancelResponse, CreateSavedRequest, DeleteSavedResponse, DeleteScheduleResponse, ExportRequest,
    FieldValuesResponse, HealthResponse, HealthStatus, HistoryEntryResponse, HistoryResponse,
    ListReportRunsResponse, ListSavedResponse, PaginationMeta, QueriesResponse, QueryRequest,
    QueryResponse, QueryStatus, ReportRunResponse, ReportRunSummary, SavedQueryResponse,
    ScheduleResponse, SchemaColumnResponse, SchemaResponse, SetScheduleRequest, StatsResponse,
    UpdateSavedRequest, ValidationResponse,
};
use trawl_auth::keys::VerifiedKey;
use trawl_auth::roles::Permission;
use trawl_auth::schedule::{ReportRun, format_interval, parse_interval};
use trawl_auth::{HistoryEntry, SavedQuery};
use trawl_engine::value::{QueryResult, Value};

use std::collections::BTreeMap;

use crate::bus::{EventBus, EventSubscriber as _};
use crate::error::ServerError;
use crate::pool::PoolDebugInfo;
use crate::query_log::{HotBufferDebug, QueryLogEntry, ResultDebug, SourceDebug, TimingDebug};
use crate::state::{AppState, CachedFieldValues, CachedSchema};

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
    if !verified.role.has_permission(Permission::Query) {
        return Err(ServerError::Unauthorized("insufficient permissions".into()));
    }

    // Increment total query counter for stats.
    state
        .total_queries
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    // Validate pagination parameters.
    let max_rows = state.query.pool.max_result_rows();
    let limit = req.limit.unwrap_or(max_rows).min(max_rows);
    let offset = req.offset.unwrap_or(0);

    if offset + limit > max_rows {
        return Err(ServerError::Ingest(format!(
            "offset + limit exceeds max_result_rows ({max_rows})"
        )));
    }

    tracing::info!(
        event_type = "query_start",
        user = %verified.name,
        role = %verified.role,
        query = %req.query,
        limit,
        offset,
        "executing query"
    );

    let query_id = state.query.tracker.start(&verified, &req.query);
    let timeout = std::time::Duration::from_secs(state.query.timeout_secs);

    // Resolve timezone from request (default to UTC when absent).
    let utc_offset_secs = req
        .timezone
        .as_deref()
        .map(trawl_engine::timezone::resolve_utc_offset)
        .transpose()
        .map_err(ServerError::BadRequest)?
        .unwrap_or(0);

    let role_str = verified.role.to_string();
    let start = std::time::Instant::now();
    let capture_debug = state.query.query_log.is_some();
    let outcome = state
        .query
        .pool
        .execute(&req.query, timeout, capture_debug, utc_offset_secs)
        .await;
    let duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
    let duration_secs = start.elapsed().as_secs_f64();

    match outcome.result {
        Ok(qr) => {
            let total = qr.row_count();
            let truncated = total >= max_rows;
            let paginated = qr.paginate(offset, limit);
            let returned = paginated.row_count();

            state.query.tracker.complete(query_id, total);

            // Auto-save successful queries to history (per user preference).
            if let Ok(key_id) = state
                .auth
                .key_store
                .lock()
                .get_key_id_by_prefix(&verified.prefix)
            {
                let _ = state.auth.history.lock().record_query(
                    key_id,
                    &req.query,
                    duration_ms,
                    total,
                    "success",
                );
            }

            tracing::info!(
                event_type = "query_complete",
                user = %verified.name,
                query = %req.query,
                total_rows = total,
                returned_rows = returned,
                query_id,
                duration_ms,
                "query complete"
            );

            metrics::counter!(crate::metrics::QUERIES_TOTAL, "role" => role_str.clone(), "status" => "success").increment(1);
            metrics::histogram!(crate::metrics::QUERY_DURATION, "role" => role_str.clone())
                .record(duration_secs);

            // Write query debug log entry (success).
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
                result: paginated,
                truncated,
                pagination: PaginationMeta {
                    limit,
                    offset,
                    returned,
                },
            }))
        }
        Err(ServerError::Timeout) => {
            state.query.tracker.timeout(query_id);

            metrics::counter!(crate::metrics::QUERIES_TOTAL, "role" => role_str.clone(), "status" => "timeout").increment(1);
            metrics::histogram!(crate::metrics::QUERY_DURATION, "role" => role_str.clone())
                .record(duration_secs);

            tracing::warn!(
                event_type = "query_timeout",
                user = %verified.name,
                query = %req.query,
                query_id,
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

            metrics::counter!(crate::metrics::QUERIES_TOTAL, "role" => role_str.clone(), "status" => "error").increment(1);
            metrics::histogram!(crate::metrics::QUERY_DURATION, "role" => role_str)
                .record(duration_secs);
            match &e {
                ServerError::Engine(
                    trawl_engine::error::EngineError::Parse(_)
                    | trawl_engine::error::EngineError::Emit(_),
                ) => {
                    tracing::warn!(
                        event_type = "query_failed",
                        error_type = "parse",
                        user = %verified.name,
                        query = %req.query,
                        query_id,
                        duration_ms,
                        error = %safe_msg,
                        "query failed: bad request"
                    );
                }
                _ => {
                    // Log the raw error for operator debugging; the safe
                    // (redacted) version is what reaches the client and tracker.
                    tracing::error!(
                        event_type = "query_failed",
                        error_type = "engine",
                        user = %verified.name,
                        query = %req.query,
                        query_id,
                        duration_ms,
                        error = %e,
                        safe_error = %safe_msg,
                        "query failed: engine error"
                    );
                }
            }

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
    let _ = tokio::task::spawn_blocking(move || {
        metrics_process::Collector::default().collect();
        crate::metrics::collect_gauges(hot_buffer.as_ref(), &fallback_glob);
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
    // Run duckdb ping (async) concurrently with synchronous checks.
    let pool = state.query.pool.clone();
    let duckdb_result = tokio::spawn(async move { pool.ping().await });

    // Auth db: synchronous, runs under the parking_lot mutex.
    let auth_result = state.auth.key_store.lock().ping();

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

    // Await duckdb result.
    let duckdb_ok = duckdb_result
        .await
        .map_err(|e| format!("task join error: {e}"))
        .and_then(|r| r.map_err(|e| e.to_string()));

    // Build checks map.
    let mut checks = HashMap::with_capacity(3);
    let duckdb_healthy = duckdb_ok.is_ok();
    let auth_healthy = auth_result.is_ok();
    let data_healthy = data_result.is_ok();

    checks.insert(
        "duckdb".into(),
        duckdb_ok.map_or_else(|e| format!("error: {e}"), |()| "ok".into()),
    );
    checks.insert(
        "auth_db".into(),
        auth_result.map_or_else(|e| format!("error: {e}"), |()| "ok".into()),
    );
    checks.insert(
        "data_path".into(),
        data_result.map_or_else(|e| format!("error: {e}"), |()| "ok".into()),
    );

    // Emit prometheus gauges.
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
    metrics::gauge!("trawl_health_check", "subsystem" => "data_path").set(if data_healthy {
        1.0
    } else {
        0.0
    });

    let status = derive_health_status(duckdb_healthy, auth_healthy, data_healthy);
    let http_status = match status {
        HealthStatus::Ok | HealthStatus::Degraded => StatusCode::OK,
        HealthStatus::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
    };

    (
        http_status,
        Json(HealthResponse {
            status,
            checks: Some(checks),
        }),
    )
}

/// Derive overall health status from individual subsystem results.
///
/// - All green → `Ok`
/// - Any non-critical (`auth_db`, `data_path`) fails → `Degraded`
/// - Any critical (duckdb) fails → `Unavailable`
fn derive_health_status(duckdb_ok: bool, auth_ok: bool, data_ok: bool) -> HealthStatus {
    if !duckdb_ok {
        return HealthStatus::Unavailable;
    }
    if !auth_ok || !data_ok {
        return HealthStatus::Degraded;
    }
    HealthStatus::Ok
}

/// `GET /api/v1/schema` — introspect the data source schema.
///
/// Returns column names and types from the configured parquet data.
/// Results are cached for `schema_cache_ttl_secs` seconds (default: 60).
pub async fn schema(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
) -> Result<Json<SchemaResponse>, ServerError> {
    if !verified.role.has_permission(Permission::SchemaRead) {
        return Err(ServerError::Unauthorized("insufficient permissions".into()));
    }

    // Hot buffer stats are cheap atomics — always read fresh (never cached).
    let (hot_events, hot_bytes) = hot_buffer_stats(&state);

    // Hold the mutex for the full check-then-refresh cycle to prevent
    // thundering herd: only one request refreshes while others wait.
    let mut cache = state.query.schema_cache.lock().await;

    if let Some(cached) = &*cache
        && cached.cached_at.elapsed().as_secs() < state.query.schema_cache_ttl_secs
    {
        tracing::debug!(event_type = "schema_cache_hit", user = %verified.name, "serving schema from cache");
        return Ok(Json(SchemaResponse {
            columns: cached
                .result
                .columns
                .iter()
                .cloned()
                .map(SchemaColumnResponse::from)
                .collect(),
            file_count: cached.result.file_count,
            cached: true,
            earliest_date: cached.earliest_date.clone(),
            latest_date: cached.latest_date.clone(),
            total_bytes: Some(cached.total_bytes),
            services: Some(cached.services.clone()),
            hot_buffer_events: hot_events,
            hot_buffer_bytes: hot_bytes,
        }));
    }

    tracing::info!(event_type = "schema_refresh", user = %verified.name, "refreshing schema cache");
    let start = std::time::Instant::now();
    let result = state.query.pool.describe_schema().await?;

    // Walk parquet files for catalog metadata (dates, services, sizes).
    let fallback_glob = state.query.pool.fallback_glob().clone();
    let (earliest_date, latest_date, total_bytes, services) =
        tokio::task::spawn_blocking(move || collect_catalog_metadata(&fallback_glob))
            .await
            .unwrap_or_default();

    let elapsed = start.elapsed().as_millis();

    tracing::info!(
        event_type = "schema_complete",
        user = %verified.name,
        columns = result.columns.len(),
        file_count = result.file_count,
        services = services.len(),
        duration_ms = elapsed,
        "schema introspection complete"
    );

    let response = SchemaResponse {
        columns: result
            .columns
            .iter()
            .cloned()
            .map(SchemaColumnResponse::from)
            .collect(),
        file_count: result.file_count,
        cached: false,
        earliest_date: earliest_date.clone(),
        latest_date: latest_date.clone(),
        total_bytes: Some(total_bytes),
        services: Some(services.clone()),
        hot_buffer_events: hot_events,
        hot_buffer_bytes: hot_bytes,
    };

    *cache = Some(CachedSchema {
        result,
        cached_at: std::time::Instant::now(),
        earliest_date,
        latest_date,
        total_bytes,
        services,
    });

    Ok(Json(response))
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

/// Parse parquet file paths to extract catalog metadata.
///
/// Path structure: `{base}/{YYYY-MM-DD}/{service}.parquet`
/// or `{base}/{YYYY-MM-DD}/{HH}/{service}.parquet`.
fn collect_catalog_metadata(
    fallback_glob: &str,
) -> (Option<String>, Option<String>, u64, Vec<String>) {
    use std::collections::BTreeSet;
    use std::path::Path;

    let base = fallback_glob
        .find('*')
        .map_or(fallback_glob, |pos| &fallback_glob[..pos]);
    let base = Path::new(base.trim_end_matches('/'));

    if !base.is_dir() {
        return (None, None, 0, Vec::new());
    }

    let Ok(entries) = crate::metrics::walk_parquet_files(base) else {
        return (None, None, 0, Vec::new());
    };

    let mut dates: BTreeSet<String> = BTreeSet::new();
    let mut services: BTreeSet<String> = BTreeSet::new();
    let mut total_bytes: u64 = 0;

    for (path, size) in &entries {
        total_bytes += size;

        // Extract service name from filename stem.
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            services.insert(stem.to_owned());
        }

        // Walk ancestors looking for a YYYY-MM-DD directory component.
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

    (earliest, latest, total_bytes, services)
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

/// `GET /api/v1/queries` — view active and recent queries.
#[allow(clippy::unused_async)]
pub async fn queries(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
) -> Result<Json<QueriesResponse>, ServerError> {
    if !verified.role.has_permission(Permission::Query) {
        return Err(ServerError::Unauthorized("insufficient permissions".into()));
    }

    Ok(Json(QueriesResponse {
        active: state.query.tracker.active(),
        recent: state.query.tracker.recent(),
    }))
}

/// `DELETE /api/v1/queries/{id}` — cancel a running query by ID.
///
/// Admin (`ServerManage`) can cancel any query. `QueryCancel` holders can cancel
/// their own queries only. Roles without `QueryCancel` are rejected outright.
pub async fn cancel_query(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Path(query_id): Path<u64>,
) -> Result<Json<CancelResponse>, ServerError> {
    // admin can cancel any query, QueryCancel holders can cancel their own
    let can_cancel = if verified.role.has_permission(Permission::ServerManage) {
        true
    } else if verified.role.has_permission(Permission::QueryCancel) {
        state
            .query
            .tracker
            .active()
            .iter()
            .find(|q| q.id == query_id)
            .is_some_and(|q| q.user == verified.name)
    } else {
        return Err(ServerError::Unauthorized("insufficient permissions".into()));
    };

    if !can_cancel {
        return Err(ServerError::Unauthorized("cannot cancel this query".into()));
    }

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
/// but does NOT check field existence (which would require schema introspection).
pub async fn validate_query(
    Extension(verified): Extension<VerifiedKey>,
    Json(req): Json<QueryRequest>,
) -> Result<Json<ValidationResponse>, ServerError> {
    if !verified.role.has_permission(Permission::Validate) {
        return Err(ServerError::Unauthorized("insufficient permissions".into()));
    }

    let result = trawl_core::parser::parse(&req.query).and_then(|ast| {
        trawl_core::emitter::validate_pipeline(&ast.pipeline).map_err(|e| {
            vec![trawl_core::parser::ParseError {
                message: e.to_string(),
                span: 0..req.query.len(),
                label: Some("validation error".to_string()),
            }]
        })
    });

    match result {
        Ok(()) => Ok(Json(ValidationResponse {
            valid: true,
            errors: vec![],
        })),
        Err(errors) => Ok(Json(ValidationResponse {
            valid: false,
            errors: errors
                .iter()
                .map(|e| trawl_api::ErrorDetail {
                    message: e.message.clone(),
                    span: Some(trawl_api::ErrorSpan {
                        start: e.span.start,
                        end: e.span.end,
                    }),
                    label: e.label.clone(),
                })
                .collect(),
        })),
    }
}

/// `GET /api/v1/stats` — server statistics and metrics (admin only).
pub async fn stats(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
) -> Result<Json<StatsResponse>, ServerError> {
    if !verified.role.has_permission(Permission::ServerManage) {
        return Err(ServerError::Unauthorized("insufficient permissions".into()));
    }

    Ok(Json(StatsResponse {
        uptime_secs: state.start_time.elapsed().as_secs(),
        total_queries: state
            .total_queries
            .load(std::sync::atomic::Ordering::Relaxed),
        active_queries: state.query.tracker.active().len(),
        pool_available: state.query.pool.available_permits(),
        pool_capacity: state.query.pool.capacity(),
    }))
}

/// `GET /api/v1/schema/values/{field}` — sample distinct values for autocomplete.
pub async fn field_values(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Path(field): Path<String>,
    Query(params): Query<FieldValuesParams>,
) -> Result<Json<FieldValuesResponse>, ServerError> {
    if !verified.role.has_permission(Permission::SchemaRead) {
        return Err(ServerError::Unauthorized("insufficient permissions".into()));
    }

    let limit = params.limit.unwrap_or(10).min(100); // cap at 100
    let cache_ttl = state.query.schema_cache_ttl_secs;

    // Check cache.
    {
        let cache = state.query.field_values_cache.lock().await;
        if let Some(cached) = cache.get(&field)
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
    let glob = state.query.pool.fallback_glob().to_string();
    let field_clone = field.clone();

    let values = tokio::task::spawn_blocking(move || {
        let executor = trawl_engine::executor::Executor::new()?;
        executor.sample_field_values(&glob, &field_clone, limit)
    })
    .await
    .map_err(|e| ServerError::Internal(format!("task panicked: {e}")))?
    .map_err(ServerError::from)?;

    let duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

    tracing::info!(
        event_type = "field_values_complete",
        user = %verified.name,
        field = %field,
        values_count = values.len(),
        duration_ms,
        "field values sampled"
    );

    // Update cache.
    state.query.field_values_cache.lock().await.insert(
        field.clone(),
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
}

/// `GET /api/v1/history` — retrieve user's query history with pagination.
pub async fn history(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Query(params): Query<HistoryParams>,
) -> Result<Json<HistoryResponse>, ServerError> {
    if !verified.role.has_permission(Permission::Query) {
        return Err(ServerError::Unauthorized("insufficient permissions".into()));
    }

    // Get the key_id for this user's prefix.
    let key_id = state
        .auth
        .key_store
        .lock()
        .get_key_id_by_prefix(&verified.prefix)
        .map_err(|e| ServerError::Internal(format!("failed to lookup key_id: {e}")))?;

    let limit = params.limit.unwrap_or(100).min(1000);
    let offset = params.offset.unwrap_or(0);

    let page = state
        .auth
        .history
        .lock()
        .get_user_history(key_id, limit, offset)
        .map_err(|e| ServerError::Internal(format!("history query failed: {e}")))?;

    Ok(Json(HistoryResponse {
        entries: page
            .entries
            .into_iter()
            .map(history_entry_response)
            .collect(),
        total: page.total,
    }))
}

/// Query parameters for the history endpoint.
#[derive(Debug, Deserialize)]
pub struct HistoryParams {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

/// Convert a [`HistoryEntry`] into a [`HistoryEntryResponse`].
fn history_entry_response(entry: HistoryEntry) -> HistoryEntryResponse {
    HistoryEntryResponse {
        id: entry.id,
        query: entry.query,
        executed_at: entry.executed_at,
        duration_ms: entry.duration_ms,
        row_count: entry.row_count,
        status: entry
            .status
            .parse::<QueryStatus>()
            .unwrap_or(QueryStatus::Error),
    }
}

/// `GET /api/v1/saved` — list user's saved queries.
pub async fn list_saved(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
) -> Result<Json<ListSavedResponse>, ServerError> {
    if !verified.role.has_permission(Permission::SavedQuery) {
        return Err(ServerError::Unauthorized("insufficient permissions".into()));
    }

    let key_id = state
        .auth
        .key_store
        .lock()
        .get_key_id_by_prefix(&verified.prefix)
        .map_err(|e| ServerError::Internal(format!("failed to lookup key_id: {e}")))?;

    let queries = state
        .auth
        .saved
        .lock()
        .list(key_id)
        .map_err(|e| ServerError::Internal(format!("failed to list saved queries: {e}")))?;

    // Enrich saved queries with schedule info.
    let schedule_store = state.auth.schedule.lock();
    let responses: Vec<SavedQueryResponse> = queries
        .into_iter()
        .map(|sq| {
            let schedule = schedule_store
                .get_schedule_for_saved_query(sq.id, key_id)
                .ok()
                .flatten()
                .map(|s| build_schedule_response(&schedule_store, s));
            SavedQueryResponse {
                id: sq.id,
                name: sq.name,
                query: sq.query,
                created_at: sq.created_at,
                updated_at: sq.updated_at,
                schedule,
            }
        })
        .collect();
    drop(schedule_store);

    Ok(Json(ListSavedResponse { queries: responses }))
}

/// `POST /api/v1/saved` — create a new saved query.
pub async fn create_saved(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Json(req): Json<CreateSavedRequest>,
) -> Result<Json<SavedQueryResponse>, ServerError> {
    if !verified.role.has_permission(Permission::SavedQuery) {
        return Err(ServerError::Unauthorized("insufficient permissions".into()));
    }

    let key_id = state
        .auth
        .key_store
        .lock()
        .get_key_id_by_prefix(&verified.prefix)
        .map_err(|e| ServerError::Internal(format!("failed to lookup key_id: {e}")))?;

    let saved = state
        .auth
        .saved
        .lock()
        .create(key_id, &req.name, &req.query)
        .map_err(|e| match e {
            trawl_auth::AuthError::DuplicateName { name } => {
                ServerError::BadRequest(format!("a saved query named '{name}' already exists"))
            }
            e => ServerError::Internal(format!("failed to create saved query: {e}")),
        })?;

    Ok(Json(saved_query_response(saved)))
}

/// `PUT /api/v1/saved/{id}` — update an existing saved query.
pub async fn update_saved(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Path(id): Path<i64>,
    Json(req): Json<UpdateSavedRequest>,
) -> Result<Json<SavedQueryResponse>, ServerError> {
    if !verified.role.has_permission(Permission::SavedQuery) {
        return Err(ServerError::Unauthorized("insufficient permissions".into()));
    }

    let key_id = state
        .auth
        .key_store
        .lock()
        .get_key_id_by_prefix(&verified.prefix)
        .map_err(|e| ServerError::Internal(format!("failed to lookup key_id: {e}")))?;

    let saved = state
        .auth
        .saved
        .lock()
        .update(id, key_id, &req.query)
        .map_err(|e| match e {
            trawl_auth::AuthError::NotFound { .. } => {
                ServerError::NotFound("saved query not found or unauthorized".into())
            }
            e => ServerError::Internal(format!("failed to update saved query: {e}")),
        })?;

    Ok(Json(saved_query_response(saved)))
}

/// `DELETE /api/v1/saved/{id}` — delete a saved query.
pub async fn delete_saved(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Path(id): Path<i64>,
) -> Result<Json<DeleteSavedResponse>, ServerError> {
    if !verified.role.has_permission(Permission::SavedQuery) {
        return Err(ServerError::Unauthorized("insufficient permissions".into()));
    }

    let key_id = state
        .auth
        .key_store
        .lock()
        .get_key_id_by_prefix(&verified.prefix)
        .map_err(|e| ServerError::Internal(format!("failed to lookup key_id: {e}")))?;

    state
        .auth
        .saved
        .lock()
        .delete(id, key_id)
        .map_err(|e| match e {
            trawl_auth::AuthError::NotFound { .. } => {
                ServerError::NotFound("saved query not found or unauthorized".into())
            }
            e => ServerError::Internal(format!("failed to delete saved query: {e}")),
        })?;

    Ok(Json(DeleteSavedResponse { deleted: true }))
}

/// Convert a [`SavedQuery`] into a [`SavedQueryResponse`] (without schedule).
fn saved_query_response(saved: SavedQuery) -> SavedQueryResponse {
    SavedQueryResponse {
        id: saved.id,
        name: saved.name,
        query: saved.query,
        created_at: saved.created_at,
        updated_at: saved.updated_at,
        schedule: None,
    }
}

/// Build a [`ScheduleResponse`] from a schedule, enriched with run metadata.
fn build_schedule_response(
    store: &trawl_auth::ScheduleStore,
    schedule: trawl_auth::Schedule,
) -> ScheduleResponse {
    let last_run = store
        .latest_run(schedule.id)
        .ok()
        .flatten()
        .map(report_run_summary);
    let total_runs = store.count_runs(schedule.id).unwrap_or(0);

    ScheduleResponse {
        id: schedule.id,
        saved_query_id: schedule.saved_query_id,
        interval: format_interval(schedule.interval_secs),
        interval_secs: schedule.interval_secs,
        max_runs: schedule.max_runs,
        enabled: schedule.enabled,
        created_at: schedule.created_at,
        updated_at: schedule.updated_at,
        last_run,
        total_runs,
    }
}

/// Convert a [`ReportRun`] into a [`ReportRunSummary`].
fn report_run_summary(run: ReportRun) -> ReportRunSummary {
    ReportRunSummary {
        id: run.id,
        query: run.query,
        status: run.status,
        started_at: run.started_at,
        finished_at: run.finished_at,
        duration_ms: run.duration_ms,
        row_count: run.row_count,
        error_message: run.error_message,
    }
}

// -- schedule handlers -------------------------------------------------------

/// Resolve `key_id` from verified token prefix.
fn resolve_key_id(state: &AppState, verified: &VerifiedKey) -> Result<i64, ServerError> {
    state
        .auth
        .key_store
        .lock()
        .get_key_id_by_prefix(&verified.prefix)
        .map_err(|e| ServerError::Internal(format!("failed to lookup key_id: {e}")))
}

/// `PUT /api/v1/saved/{id}/schedule` — create or update a schedule.
pub async fn set_schedule(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Path(saved_id): Path<i64>,
    Json(req): Json<SetScheduleRequest>,
) -> Result<Json<ScheduleResponse>, ServerError> {
    if !verified.role.has_permission(Permission::SavedQuery) {
        return Err(ServerError::Unauthorized("insufficient permissions".into()));
    }

    let key_id = resolve_key_id(&state, &verified)?;
    let interval_secs = parse_interval(&req.interval)
        .map_err(|e| ServerError::BadRequest(format!("invalid interval: {e}")))?;

    // Verify saved query ownership.
    state
        .auth
        .saved
        .lock()
        .list(key_id)
        .map_err(|e| ServerError::Internal(format!("failed to list saved queries: {e}")))?
        .iter()
        .find(|sq| sq.id == saved_id)
        .ok_or_else(|| ServerError::NotFound("saved query not found or unauthorized".into()))?;

    let schedule_store = state.auth.schedule.lock();

    // Try update first, fall back to create.
    let schedule = match schedule_store.get_schedule_for_saved_query(saved_id, key_id) {
        Ok(Some(existing)) => schedule_store
            .update_schedule(
                existing.id,
                key_id,
                interval_secs,
                req.max_runs,
                req.enabled,
            )
            .map_err(|e| ServerError::Internal(format!("failed to update schedule: {e}")))?,
        Ok(None) => schedule_store
            .create_schedule(saved_id, key_id, interval_secs, req.max_runs)
            .map_err(|e| ServerError::Internal(format!("failed to create schedule: {e}")))?,
        Err(e) => {
            return Err(ServerError::Internal(format!(
                "failed to check schedule: {e}"
            )));
        }
    };

    let response = build_schedule_response(&schedule_store, schedule);
    drop(schedule_store);

    Ok(Json(response))
}

/// `GET /api/v1/saved/{id}/schedule` — get schedule for a saved query.
pub async fn get_schedule(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Path(saved_id): Path<i64>,
) -> Result<Json<ScheduleResponse>, ServerError> {
    if !verified.role.has_permission(Permission::SavedQuery) {
        return Err(ServerError::Unauthorized("insufficient permissions".into()));
    }

    let key_id = resolve_key_id(&state, &verified)?;
    let schedule_store = state.auth.schedule.lock();

    let schedule = schedule_store
        .get_schedule_for_saved_query(saved_id, key_id)
        .map_err(|e| ServerError::Internal(format!("failed to get schedule: {e}")))?
        .ok_or_else(|| ServerError::NotFound("no schedule for this saved query".into()))?;

    let response = build_schedule_response(&schedule_store, schedule);
    drop(schedule_store);

    Ok(Json(response))
}

/// `DELETE /api/v1/saved/{id}/schedule` — delete a schedule.
pub async fn delete_schedule(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Path(saved_id): Path<i64>,
) -> Result<Json<DeleteScheduleResponse>, ServerError> {
    if !verified.role.has_permission(Permission::SavedQuery) {
        return Err(ServerError::Unauthorized("insufficient permissions".into()));
    }

    let key_id = resolve_key_id(&state, &verified)?;

    state
        .auth
        .schedule
        .lock()
        .delete_schedule(saved_id, key_id)
        .map_err(|e| match e {
            trawl_auth::AuthError::NotFound { .. } => {
                ServerError::NotFound("schedule not found or unauthorized".into())
            }
            e => ServerError::Internal(format!("failed to delete schedule: {e}")),
        })?;

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
    if !verified.role.has_permission(Permission::SavedQuery) {
        return Err(ServerError::Unauthorized("insufficient permissions".into()));
    }

    let key_id = resolve_key_id(&state, &verified)?;
    let schedule_store = state.auth.schedule.lock();

    let runs = schedule_store
        .list_runs(saved_id, key_id, params.limit, params.offset)
        .map_err(|e| ServerError::Internal(format!("failed to list runs: {e}")))?;

    let total = schedule_store
        .count_runs_for_saved_query(saved_id, key_id)
        .map_err(|e| ServerError::Internal(format!("failed to count runs: {e}")))?;

    drop(schedule_store);

    Ok(Json(ListReportRunsResponse {
        runs: runs.into_iter().map(report_run_summary).collect(),
        total,
    }))
}

/// `GET /api/v1/saved/{id}/runs/{run_id}` — get a single report run with result data.
pub async fn get_report_run(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Path((saved_id, run_id)): Path<(i64, i64)>,
) -> Result<Json<ReportRunResponse>, ServerError> {
    if !verified.role.has_permission(Permission::SavedQuery) {
        return Err(ServerError::Unauthorized("insufficient permissions".into()));
    }

    let key_id = resolve_key_id(&state, &verified)?;
    let schedule_store = state.auth.schedule.lock();

    let run = schedule_store
        .get_run(run_id, key_id)
        .map_err(|e| ServerError::Internal(format!("failed to get run: {e}")))?
        .ok_or_else(|| ServerError::NotFound("report run not found or unauthorized".into()))?;

    // Verify the run belongs to the correct saved query.
    if run.saved_query_id != saved_id {
        return Err(ServerError::NotFound(
            "report run not found for this saved query".into(),
        ));
    }

    // Decompress result blob if present.
    let result = schedule_store
        .get_run_result(run_id, key_id)
        .map_err(|e| ServerError::Internal(format!("failed to get run result: {e}")))?
        .and_then(|compressed| {
            let decompressed = zstd::decode_all(compressed.as_slice()).ok()?;
            serde_json::from_slice::<QueryResult>(&decompressed).ok()
        });

    drop(schedule_store);

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
    if !verified.role.has_permission(Permission::Export) {
        return Err(ServerError::Unauthorized("insufficient permissions".into()));
    }

    let format = params.format.unwrap_or(trawl_api::ExportFormat::Csv);

    // Get max_export_rows from state.
    let max_export_rows = state.query.max_export_rows;
    let limit = req.limit.unwrap_or(max_export_rows).min(max_export_rows);

    tracing::info!(
        event_type = "export_start",
        user = %verified.name,
        role = %verified.role,
        query = %req.query,
        %format,
        limit,
        "executing export"
    );

    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(state.query.timeout_secs);

    // Parquet export uses DuckDB's native COPY TO — no need to materialize
    // the result set in memory.
    if format == trawl_api::ExportFormat::Parquet {
        let bytes = state
            .query
            .pool
            .export_parquet(&req.query, limit, timeout)
            .await?;
        let duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

        tracing::info!(
            event_type = "export_complete",
            user = %verified.name,
            query = %req.query,
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
        .execute(&req.query, timeout, capture_debug, 0)
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

    // Limit rows to max_export_rows.
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
        trawl_api::ExportFormat::Parquet => unreachable!("handled above"),
    };

    tracing::info!(
        event_type = "export_complete",
        user = %verified.name,
        query = %req.query,
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
        role: verified.role.to_string(),
        dsl: dsl.to_owned(),
        source: SourceDebug {
            computed: debug.computed_source.clone(),
            globs: debug.glob_count,
            service_filter: debug.service_filter.clone(),
            time_filter_secs: debug.time_filter_secs,
            is_fallback: debug.is_fallback,
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

/// Prefix cell values that could trigger formula injection in spreadsheets.
///
/// See OWASP CSV injection guidelines. Only string values need
/// sanitization — numeric values like `-42` are legitimately negative.
/// Returns borrowed when no prefix is needed.
fn sanitize_csv_formula(s: &str) -> Cow<'_, str> {
    if s.starts_with(['=', '+', '-', '@', '\t', '|']) {
        Cow::Owned(format!("'{s}"))
    } else {
        Cow::Borrowed(s)
    }
}

/// Apply pipeline stages to an event. Returns `true` if the event passes,
/// `false` if filtered or done.
fn apply_stages(
    stages: &mut [trawl_core::stream::CompiledStage],
    event: &mut serde_json::Map<String, serde_json::Value>,
) -> bool {
    for stage in stages.iter_mut() {
        match trawl_core::stream::apply_stage(stage, event) {
            trawl_core::stream::StageResult::Pass => {}
            trawl_core::stream::StageResult::Filtered | trawl_core::stream::StageResult::Done => {
                return false;
            }
        }
    }
    true
}

/// Build an SSE snapshot event from the current aggregation state,
/// applying post-stages to each row.
fn emit_agg_snapshot(
    aggregation: &trawl_core::stream::CompiledAggregation,
    post_stages: &mut [trawl_core::stream::CompiledStage],
) -> Event {
    let (columns, rows) = aggregation.snapshot();
    let filtered_rows: Vec<_> = rows
        .into_iter()
        .filter_map(|mut row| {
            for stage in post_stages.iter_mut() {
                match trawl_core::stream::apply_stage(stage, &mut row) {
                    trawl_core::stream::StageResult::Pass => {}
                    trawl_core::stream::StageResult::Filtered
                    | trawl_core::stream::StageResult::Done => return None,
                }
            }
            Some(row)
        })
        .collect();

    let payload = serde_json::json!({
        "columns": columns,
        "rows": filtered_rows,
    });
    Event::default().event("snapshot").data(payload.to_string())
}

/// `GET /api/v1/stream` — stream live events via Server-Sent Events (SSE).
///
/// Subscribes to the event bus and filters incoming events in-memory
/// using [`CompiledFilter`]. Each matching event is streamed individually
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
    if !verified.role.has_permission(Permission::Stream) {
        return Err(ServerError::Unauthorized("insufficient permissions".into()));
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

    tracing::info!(
        event_type = "stream_start",
        user = %verified.name,
        query = %query_dsl,
        "starting SSE stream"
    );

    // Parse and compile the filter once upfront.
    let ast = trawl_core::parser::parse(&query_dsl)
        .map_err(|errors| ServerError::BadRequest(format!("{errors:?}")))?;
    let filter = trawl_core::filter::CompiledFilter::compile(&ast.search);

    // Compile the pipeline stages for streaming evaluation.
    let stream_plan = trawl_core::stream::compile_stream_plan(&ast.pipeline)
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
                            let now = chrono::Utc::now();
                            for event in &batch.events {
                                if !filter.matches_at(event, now) {
                                    continue;
                                }

                                let mut event = event.clone();
                                let mut pass = true;
                                for stage in &mut stages {
                                    match trawl_core::stream::apply_stage(stage, &mut event) {
                                        trawl_core::stream::StageResult::Pass => {}
                                        trawl_core::stream::StageResult::Filtered => {
                                            pass = false;
                                            break;
                                        }
                                        trawl_core::stream::StageResult::Done => {
                                            stream_done = true;
                                            pass = false;
                                            break;
                                        }
                                    }
                                }

                                if pass {
                                    let json = serde_json::to_string(&event).unwrap_or_default();
                                    yield Ok(Event::default().event("data").data(json));
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
                                    let now = chrono::Utc::now();
                                    for event in &batch.events {
                                        if !filter.matches_at(event, now) {
                                            continue;
                                        }

                                        // Apply pre-stages and feed accumulator.
                                        let mut event = event.clone();
                                        if apply_stages(&mut pre_stages, &mut event) {
                                            aggregation.feed_event(&event);
                                            events_since_snapshot += 1;
                                        }
                                    }

                                    // Emit snapshot if event threshold reached.
                                    if events_since_snapshot >= SNAPSHOT_EVENT_THRESHOLD {
                                        let snapshot = emit_agg_snapshot(
                                            &aggregation,
                                            &mut post_stages,
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
        assert_eq!(derive_health_status(true, true, true), HealthStatus::Ok);
    }

    #[test]
    fn health_auth_down_is_degraded() {
        assert_eq!(
            derive_health_status(true, false, true),
            HealthStatus::Degraded
        );
    }

    #[test]
    fn health_data_path_down_is_degraded() {
        assert_eq!(
            derive_health_status(true, true, false),
            HealthStatus::Degraded
        );
    }

    #[test]
    fn health_both_noncritical_down_is_degraded() {
        assert_eq!(
            derive_health_status(true, false, false),
            HealthStatus::Degraded
        );
    }

    #[test]
    fn health_duckdb_down_is_unavailable() {
        assert_eq!(
            derive_health_status(false, true, true),
            HealthStatus::Unavailable
        );
    }

    #[test]
    fn health_all_down_is_unavailable() {
        assert_eq!(
            derive_health_status(false, false, false),
            HealthStatus::Unavailable
        );
    }

    #[test]
    fn health_duckdb_and_noncritical_down_is_unavailable() {
        assert_eq!(
            derive_health_status(false, false, true),
            HealthStatus::Unavailable
        );
    }
}
