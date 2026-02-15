//! HTTP request handlers for the fleet API.

use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::{Extension, Json};
use fleet_api::{
    CancelResponse, DeleteSavedResponse, FieldValuesResponse, HealthResponse, HistoryEntryResponse,
    HistoryResponse, ListSavedResponse, PaginationMeta, QueriesResponse, QueryResponse,
    SavedQueryResponse, SchemaColumnResponse, SchemaResponse, StatsResponse, ValidationResponse,
};
use fleet_auth::HistoryEntry;
use fleet_auth::SavedQuery;
use fleet_auth::keys::VerifiedKey;
use fleet_auth::roles::Permission;
use fleet_engine::value::{QueryResult, Value};
use serde::Deserialize;
use std::convert::Infallible;
use std::time::Duration;

use crate::error::ServerError;
use crate::state::{AppState, CachedFieldValues, CachedSchema};

// -- request types -----------------------------------------------------------

/// Query request body.
#[derive(Debug, Deserialize)]
pub struct QueryRequest {
    /// The fleet DSL query string.
    pub query: String,
    /// Optional limit for pagination (defaults to `max_result_rows`).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Optional offset for pagination (defaults to 0).
    #[serde(default)]
    pub offset: Option<usize>,
}

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

    let start = std::time::Instant::now();
    let result = state.query.pool.execute(&req.query, timeout).await;
    let duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

    match result {
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
                total_rows = total,
                returned_rows = returned,
                query_id,
                duration_ms,
                "query complete"
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
            tracing::warn!(
                event_type = "query_timeout",
                user = %verified.name,
                query = %req.query,
                query_id,
                duration_ms,
                timeout_secs = state.query.timeout_secs,
                "query timed out"
            );
            Err(ServerError::Timeout)
        }
        Err(e) => {
            // SECURITY: use safe_message() to redact database internals
            // from tracker history and logs.
            let safe_msg = e.safe_message();
            state.query.tracker.fail(query_id, &safe_msg);
            match &e {
                ServerError::Engine(
                    fleet_engine::error::EngineError::Parse(_)
                    | fleet_engine::error::EngineError::Emit(_),
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
            Err(e)
        }
    }
}

/// `GET /api/v1/health` — unauthenticated health check.
#[allow(clippy::unused_async)] // axum requires async handlers
pub async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".into(),
    })
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

    // Hold the mutex for the full check-then-refresh cycle to prevent
    // thundering herd: only one request refreshes while others wait.
    let mut cache = state.query.schema_cache.lock().await;

    if let Some(cached) = &*cache {
        if cached.cached_at.elapsed().as_secs() < state.query.schema_cache_ttl_secs {
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
            }));
        }
    }

    tracing::info!(event_type = "schema_refresh", user = %verified.name, "refreshing schema cache");
    let start = std::time::Instant::now();
    let result = state.query.pool.describe_schema().await?;
    let elapsed = start.elapsed().as_millis();

    tracing::info!(
        event_type = "schema_complete",
        user = %verified.name,
        columns = result.columns.len(),
        file_count = result.file_count,
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
    };

    *cache = Some(CachedSchema {
        result,
        cached_at: std::time::Instant::now(),
    });

    Ok(Json(response))
}

/// `GET /api/v1/queries` — view active and recent queries (admin only).
#[allow(clippy::unused_async)]
pub async fn queries(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
) -> Result<Json<QueriesResponse>, ServerError> {
    if !verified.role.has_permission(Permission::ServerManage) {
        return Err(ServerError::Unauthorized("insufficient permissions".into()));
    }

    Ok(Json(QueriesResponse {
        active: state.query.tracker.active(),
        recent: state.query.tracker.recent(),
    }))
}

/// `DELETE /api/v1/queries/{id}` — cancel a running query by ID.
///
/// Requires either `ServerManage` permission (admin) or ownership of the query.
pub async fn cancel_query(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Path(query_id): Path<u64>,
) -> Result<Json<CancelResponse>, ServerError> {
    // admin can cancel anything, users can only cancel their own
    let can_cancel = if verified.role.has_permission(Permission::ServerManage) {
        true
    } else {
        state
            .query
            .tracker
            .active()
            .iter()
            .find(|q| q.id == query_id)
            .is_some_and(|q| q.user == verified.name)
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
    if !verified.role.has_permission(Permission::Query) {
        return Err(ServerError::Unauthorized("insufficient permissions".into()));
    }

    let result = fleet_core::parser::parse(&req.query).and_then(|ast| {
        fleet_core::emitter::validate_pipeline(&ast.pipeline).map_err(|e| {
            vec![fleet_core::parser::ParseError {
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
            errors: errors.into_iter().map(|e| e.to_string()).collect(),
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
        if let Some(cached) = cache.get(&field) {
            if cached.cached_at.elapsed().as_secs() < cache_ttl {
                return Ok(Json(FieldValuesResponse {
                    field: field.clone(),
                    values: cached.values.clone(),
                    cached: true,
                }));
            }
        }
    }

    // Cache miss — sample from parquet.
    let glob = state.query.pool.fallback_glob().to_string();
    let field_clone = field.clone();

    let values = tokio::task::spawn_blocking(move || {
        let executor = fleet_engine::executor::Executor::new()?;
        executor.sample_field_values(&glob, &field_clone, limit)
    })
    .await
    .map_err(|e| ServerError::Internal(format!("task panicked: {e}")))?
    .map_err(ServerError::from)?;

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
        status: entry.status,
    }
}

/// `GET /api/v1/saved` — list user's saved queries.
pub async fn list_saved(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
) -> Result<Json<ListSavedResponse>, ServerError> {
    if !verified.role.has_permission(Permission::Query) {
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

    Ok(Json(ListSavedResponse {
        queries: queries.into_iter().map(saved_query_response).collect(),
    }))
}

/// `POST /api/v1/saved` — create a new saved query.
pub async fn create_saved(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Json(req): Json<CreateSavedRequest>,
) -> Result<Json<SavedQueryResponse>, ServerError> {
    if !verified.role.has_permission(Permission::Query) {
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
            fleet_auth::AuthError::DuplicateName { name } => {
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
    if !verified.role.has_permission(Permission::Query) {
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
            fleet_auth::AuthError::NotFound { .. } => {
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
    if !verified.role.has_permission(Permission::Query) {
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
            fleet_auth::AuthError::NotFound { .. } => {
                ServerError::NotFound("saved query not found or unauthorized".into())
            }
            e => ServerError::Internal(format!("failed to delete saved query: {e}")),
        })?;

    Ok(Json(DeleteSavedResponse { deleted: true }))
}

/// Request body for creating a saved query.
#[derive(Debug, Deserialize)]
pub struct CreateSavedRequest {
    pub name: String,
    pub query: String,
}

/// Request body for updating a saved query.
#[derive(Debug, Deserialize)]
pub struct UpdateSavedRequest {
    pub query: String,
}

/// Convert a [`SavedQuery`] into a [`SavedQueryResponse`].
fn saved_query_response(saved: SavedQuery) -> SavedQueryResponse {
    SavedQueryResponse {
        id: saved.id,
        name: saved.name,
        query: saved.query,
        created_at: saved.created_at,
        updated_at: saved.updated_at,
    }
}

/// `POST /api/v1/export` — export query results as CSV.
///
/// Bypasses `max_result_rows` in favor of `max_export_rows` to support larger downloads.
pub async fn export(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Query(params): Query<ExportParams>,
    Json(req): Json<ExportRequest>,
) -> Result<impl IntoResponse, ServerError> {
    if !verified.role.has_permission(Permission::Query) {
        return Err(ServerError::Unauthorized("insufficient permissions".into()));
    }

    // Validate format (only CSV for now).
    let format = params.format.as_deref().unwrap_or("csv");
    if format != "csv" {
        return Err(ServerError::BadRequest(
            "only CSV format is supported".into(),
        ));
    }

    // Get max_export_rows from state.
    let max_export_rows = state.query.max_export_rows;
    let limit = req.limit.unwrap_or(max_export_rows).min(max_export_rows);

    tracing::info!(
        event_type = "export_start",
        user = %verified.name,
        role = %verified.role,
        query = %req.query,
        limit,
        "executing export"
    );

    let timeout = std::time::Duration::from_secs(state.query.timeout_secs);
    let result = state.query.pool.execute(&req.query, timeout).await?;

    // Limit rows to max_export_rows.
    let limited = result.paginate(0, limit);

    // Generate CSV.
    let csv = generate_csv(&limited);

    // Return CSV response with proper headers.
    Ok((
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"export.csv\"",
            ),
        ],
        csv,
    ))
}

/// Query parameters for the export endpoint.
#[derive(Debug, Deserialize)]
pub struct ExportParams {
    pub format: Option<String>,
}

/// Request body for the export endpoint.
#[derive(Debug, Deserialize)]
pub struct ExportRequest {
    pub query: String,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Generate RFC 4180-compliant CSV from query results.
///
/// Quotes fields containing commas, newlines, or quotes.
/// Escapes quotes by doubling them.
fn generate_csv(result: &QueryResult) -> String {
    let mut csv = String::new();

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
            csv.push_str(&quote_csv_field(&value_to_string(value)));
        }
        csv.push('\n');
    }

    csv
}

/// Quote a CSV field if it contains special characters (comma, newline, quote).
/// Escape quotes by doubling them.
fn quote_csv_field(s: &str) -> String {
    let needs_quoting = s.contains(',') || s.contains('\n') || s.contains('"');

    if needs_quoting || s.is_empty() {
        let escaped = s.replace('"', "\"\"");
        format!("\"{escaped}\"")
    } else {
        s.to_owned()
    }
}

/// Convert a Value to a string for CSV export.
fn value_to_string(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Boolean(b) => b.to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::String(s) => s.clone(),
    }
}

/// `GET /api/v1/stream` — stream query results via Server-Sent Events (SSE).
///
/// Re-executes the query at regular intervals and streams new results.
/// Used for live tail mode in the TUI (F9 toggle).
pub async fn stream_query(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Query(params): Query<StreamParams>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, ServerError> {
    if !verified.role.has_permission(Permission::Query) {
        return Err(ServerError::Unauthorized("insufficient permissions".into()));
    }

    let query_dsl = params.query.clone();
    let interval_secs = params.interval.unwrap_or(5).clamp(1, 60);

    tracing::info!(
        event_type = "stream_start",
        user = %verified.name,
        query = %query_dsl,
        interval = interval_secs,
        "starting SSE stream"
    );

    // Create SSE event stream using async-stream for cleaner async code.
    let pool = state.query.pool.clone();
    let timeout = Duration::from_secs(state.query.timeout_secs);

    let event_stream = async_stream::stream! {
        let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));

        loop {
            interval.tick().await;

            match pool.execute(&query_dsl, timeout).await {
                Ok(result) => {
                    // Emit each row as a data event.
                    for row in &result.rows {
                        if let Ok(json) = serde_json::to_string(&row) {
                            yield Ok(Event::default().event("data").data(json));
                        }
                    }
                }
                Err(e) => {
                    let error_msg = e.safe_message();
                    tracing::warn!(
                        event_type = "stream_query_error",
                        error = %error_msg,
                        "stream query failed"
                    );
                    let error_json = serde_json::json!({ "error": error_msg }).to_string();
                    yield Ok(Event::default().event("error").data(error_json));
                }
            }
        }
    };

    Ok(Sse::new(event_stream).keep_alive(KeepAlive::default()))
}

/// Query parameters for the stream endpoint.
#[derive(Debug, Deserialize)]
pub struct StreamParams {
    /// The DSL query to execute repeatedly.
    pub query: String,
    /// Interval in seconds (1-60, default 5).
    #[serde(default)]
    pub interval: Option<u64>,
}
