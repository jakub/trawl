//! HTTP request handlers for the fleet API.

use axum::extract::{Path, Query, State};
use axum::{Extension, Json};
use fleet_auth::HistoryEntry;
use fleet_auth::SavedQuery;
use fleet_auth::keys::VerifiedKey;
use fleet_auth::roles::Permission;
use fleet_engine::value::{QueryResult, SchemaColumn};
use serde::{Deserialize, Serialize};

use crate::error::ServerError;
use crate::state::{AppState, CachedFieldValues, CachedSchema};

// -- request/response types --------------------------------------------------

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

/// Query response with pagination metadata.
#[derive(Debug, Serialize)]
pub struct QueryResponse {
    /// The query result (columns + rows).
    #[serde(flatten)]
    pub result: QueryResult,
    /// Whether results were truncated due to `max_result_rows`.
    pub truncated: bool,
    /// Pagination metadata.
    pub pagination: PaginationMeta,
}

/// Pagination metadata for query responses.
#[derive(Debug, Serialize)]
pub struct PaginationMeta {
    /// The limit applied to this response.
    pub limit: usize,
    /// The offset applied to this response.
    pub offset: usize,
    /// The number of rows actually returned.
    pub returned: usize,
}

/// Health check response body.
///
/// Deliberately minimal — version and uptime are omitted to avoid
/// information disclosure on an unauthenticated endpoint.
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
}

/// Schema introspection response body.
#[derive(Debug, Serialize)]
pub struct SchemaResponse {
    /// Column descriptors (name + type).
    pub columns: Vec<SchemaColumnResponse>,
    /// Number of parquet files matching the configured glob.
    pub file_count: u64,
    /// Whether this result was served from cache.
    pub cached: bool,
}

/// A single column in the schema response.
#[derive(Debug, Serialize)]
pub struct SchemaColumnResponse {
    pub name: String,
    #[serde(rename = "type")]
    pub data_type: String,
}

impl From<SchemaColumn> for SchemaColumnResponse {
    fn from(col: SchemaColumn) -> Self {
        Self {
            name: col.name,
            data_type: col.data_type,
        }
    }
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
    Json(HealthResponse { status: "ok" })
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

/// Response for the queries endpoint.
#[derive(Debug, Serialize)]
pub struct QueriesResponse {
    pub active: Vec<crate::tracker::ActiveQuerySnapshot>,
    pub recent: Vec<crate::tracker::CompletedQuery>,
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

/// Response for the cancel endpoint.
#[derive(Debug, Serialize)]
pub struct CancelResponse {
    pub cancelled: bool,
    pub query_id: u64,
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

/// Response for the validate endpoint.
#[derive(Debug, Serialize)]
pub struct ValidationResponse {
    pub valid: bool,
    pub errors: Vec<String>,
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

/// Response for the stats endpoint.
#[derive(Debug, Serialize)]
pub struct StatsResponse {
    pub uptime_secs: u64,
    pub total_queries: u64,
    pub active_queries: usize,
    pub pool_available: usize,
    pub pool_capacity: usize,
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

/// Response for the field values endpoint.
#[derive(Debug, Serialize)]
pub struct FieldValuesResponse {
    pub field: String,
    pub values: Vec<String>,
    pub cached: bool,
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
            .map(HistoryEntryResponse::from)
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

/// Response for the history endpoint.
#[derive(Debug, Serialize)]
pub struct HistoryResponse {
    pub entries: Vec<HistoryEntryResponse>,
    pub total: usize,
}

/// A single history entry in the response.
#[derive(Debug, Serialize)]
pub struct HistoryEntryResponse {
    pub id: i64,
    pub query: String,
    pub executed_at: String,
    pub duration_ms: u64,
    pub row_count: usize,
    pub status: String,
}

impl From<HistoryEntry> for HistoryEntryResponse {
    fn from(entry: HistoryEntry) -> Self {
        Self {
            id: entry.id,
            query: entry.query,
            executed_at: entry.executed_at,
            duration_ms: entry.duration_ms,
            row_count: entry.row_count,
            status: entry.status,
        }
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
        queries: queries.into_iter().map(SavedQueryResponse::from).collect(),
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

    Ok(Json(SavedQueryResponse::from(saved)))
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

    Ok(Json(SavedQueryResponse::from(saved)))
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

/// Response for listing saved queries.
#[derive(Debug, Serialize)]
pub struct ListSavedResponse {
    pub queries: Vec<SavedQueryResponse>,
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

/// A single saved query in the response.
#[derive(Debug, Serialize)]
pub struct SavedQueryResponse {
    pub id: i64,
    pub name: String,
    pub query: String,
    pub created_at: String,
    pub updated_at: String,
}

/// Response for deleting a saved query.
#[derive(Debug, Serialize)]
pub struct DeleteSavedResponse {
    pub deleted: bool,
}

impl From<SavedQuery> for SavedQueryResponse {
    fn from(saved: SavedQuery) -> Self {
        Self {
            id: saved.id,
            name: saved.name,
            query: saved.query,
            created_at: saved.created_at,
            updated_at: saved.updated_at,
        }
    }
}
