//! HTTP request handlers for the fleet API.

use axum::extract::State;
use axum::{Extension, Json};
use fleet_auth::keys::VerifiedKey;
use fleet_engine::value::{QueryResult, Value};
use serde::{Deserialize, Serialize};

use crate::error::ServerError;
use crate::state::AppState;

// -- request/response types --------------------------------------------------

/// Query request body.
#[derive(Debug, Deserialize)]
pub struct QueryRequest {
    /// The fleet DSL query string.
    pub query: String,
}

/// Query response body.
#[derive(Debug, Serialize)]
pub struct QueryResponse {
    /// Column names.
    pub columns: Vec<String>,
    /// Row data (each row is a list of JSON values).
    pub rows: Vec<Vec<serde_json::Value>>,
    /// Total number of rows returned.
    pub row_count: usize,
}

impl From<QueryResult> for QueryResponse {
    fn from(result: QueryResult) -> Self {
        let row_count = result.row_count();
        Self {
            columns: result.columns.into_iter().map(|c| c.name).collect(),
            rows: result
                .rows
                .into_iter()
                .map(|row| row.into_iter().map(value_to_json).collect())
                .collect(),
            row_count,
        }
    }
}

/// Health check response body.
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub version: &'static str,
    pub uptime_secs: u64,
}

// -- handlers ----------------------------------------------------------------

/// `POST /api/v1/query` — execute a DSL query against the configured data source.
///
/// Requires a valid bearer token (injected by auth middleware).
pub async fn query(
    State(state): State<AppState>,
    Extension(verified): Extension<VerifiedKey>,
    Json(req): Json<QueryRequest>,
) -> Result<Json<QueryResponse>, ServerError> {
    tracing::info!(
        user = %verified.name,
        role = %verified.role,
        query = %req.query,
        "executing query"
    );

    let result = state.pool.execute(&req.query).await?;

    tracing::info!(
        user = %verified.name,
        rows = result.row_count(),
        "query complete"
    );

    Ok(Json(result.into()))
}

/// `GET /api/v1/health` — unauthenticated health check.
#[allow(clippy::unused_async)] // axum requires async handlers
pub async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        uptime_secs: state.start_time.elapsed().as_secs(),
    })
}

// -- helpers -----------------------------------------------------------------

fn value_to_json(val: Value) -> serde_json::Value {
    match val {
        Value::Null => serde_json::Value::Null,
        Value::Boolean(b) => serde_json::Value::Bool(b),
        Value::Integer(i) => serde_json::json!(i),
        Value::Float(f) => serde_json::json!(f),
        Value::String(s) => serde_json::Value::String(s),
    }
}
