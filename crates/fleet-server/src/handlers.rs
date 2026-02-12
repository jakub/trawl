//! HTTP request handlers for the fleet API.

use axum::extract::State;
use axum::{Extension, Json};
use fleet_auth::keys::VerifiedKey;
use fleet_auth::roles::Permission;
use fleet_engine::value::{QueryResult, SchemaColumn};
use serde::{Deserialize, Serialize};

use crate::error::ServerError;
use crate::state::{AppState, CachedSchema, SCHEMA_CACHE_TTL_SECS};

// -- request/response types --------------------------------------------------

/// Query request body.
#[derive(Debug, Deserialize)]
pub struct QueryRequest {
    /// The fleet DSL query string.
    pub query: String,
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
pub async fn query(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Json(req): Json<QueryRequest>,
) -> Result<Json<QueryResult>, ServerError> {
    if !verified.role.has_permission(Permission::Query) {
        return Err(ServerError::Unauthorized("insufficient permissions".into()));
    }

    tracing::info!(
        user = %verified.name,
        role = %verified.role,
        query = %req.query,
        "executing query"
    );

    let query_id = state.tracker.start(&verified, &req.query);
    let timeout = std::time::Duration::from_secs(state.timeout_secs);

    let result = state.pool.execute(&req.query, timeout).await;

    match result {
        Ok(qr) => {
            let rows = qr.row_count();
            state.tracker.complete(query_id, rows);
            tracing::info!(
                user = %verified.name,
                rows,
                query_id,
                "query complete"
            );
            Ok(Json(qr))
        }
        Err(ServerError::Timeout) => {
            state.tracker.timeout(query_id);
            tracing::warn!(
                user = %verified.name,
                query = %req.query,
                query_id,
                timeout_secs = state.timeout_secs,
                "query timed out"
            );
            Err(ServerError::Timeout)
        }
        Err(e) => {
            // SECURITY: use safe_message() to redact database internals
            // from tracker history and logs.
            let safe_msg = e.safe_message();
            state.tracker.fail(query_id, &safe_msg);
            match &e {
                ServerError::Engine(
                    fleet_engine::error::EngineError::Parse(_)
                    | fleet_engine::error::EngineError::Emit(_),
                ) => {
                    tracing::warn!(
                        user = %verified.name,
                        query = %req.query,
                        query_id,
                        error = %safe_msg,
                        "query failed: bad request"
                    );
                }
                _ => {
                    // Log the raw error for operator debugging; the safe
                    // (redacted) version is what reaches the client and tracker.
                    tracing::error!(
                        user = %verified.name,
                        query = %req.query,
                        query_id,
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
/// Results are cached for [`SCHEMA_CACHE_TTL_SECS`] seconds.
pub async fn schema(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
) -> Result<Json<SchemaResponse>, ServerError> {
    if !verified.role.has_permission(Permission::SchemaRead) {
        return Err(ServerError::Unauthorized("insufficient permissions".into()));
    }

    // Hold the mutex for the full check-then-refresh cycle to prevent
    // thundering herd: only one request refreshes while others wait.
    let mut cache = state.schema_cache.lock().await;

    if let Some(cached) = &*cache {
        if cached.cached_at.elapsed().as_secs() < SCHEMA_CACHE_TTL_SECS {
            tracing::debug!(user = %verified.name, "serving schema from cache");
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

    tracing::info!(user = %verified.name, "refreshing schema cache");
    let start = std::time::Instant::now();
    let result = state.pool.describe_schema().await?;
    let elapsed = start.elapsed().as_millis();

    tracing::info!(
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
        active: state.tracker.active(),
        recent: state.tracker.recent(),
    }))
}

/// Response for the queries endpoint.
#[derive(Debug, Serialize)]
pub struct QueriesResponse {
    pub active: Vec<crate::tracker::ActiveQuerySnapshot>,
    pub recent: Vec<crate::tracker::CompletedQuery>,
}
