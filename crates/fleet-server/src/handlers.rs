//! HTTP request handlers for the fleet API.

use axum::extract::{Path, State};
use axum::{Extension, Json};
use fleet_auth::keys::VerifiedKey;
use fleet_auth::roles::Permission;
use fleet_engine::value::{QueryResult, SchemaColumn};
use serde::{Deserialize, Serialize};

use crate::error::ServerError;
use crate::state::{AppState, CachedSchema};

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
