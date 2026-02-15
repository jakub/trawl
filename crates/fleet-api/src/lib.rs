//! Shared wire types for the fleet HTTP API.
//!
//! These types are used by both `fleet-server` (serialization) and
//! `fleet-client` (deserialization) to ensure the API contract stays
//! in sync across crates.

use fleet_engine::value::{QueryResult, SchemaColumn};
use serde::{Deserialize, Serialize};

// -- health ------------------------------------------------------------------

/// Health check response from the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    /// Status string, typically `"ok"`.
    pub status: String,
}

// -- schema ------------------------------------------------------------------

/// Schema introspection response from the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaResponse {
    /// Column descriptors (name + type).
    pub columns: Vec<SchemaColumnResponse>,
    /// Number of parquet files matching the configured glob.
    pub file_count: u64,
    /// Whether this result was served from cache.
    pub cached: bool,
}

/// A single column in the schema response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaColumnResponse {
    /// Column name.
    pub name: String,
    /// Column data type (e.g. "VARCHAR", "TIMESTAMP").
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

// -- query -------------------------------------------------------------------

/// Query response with pagination metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaginationMeta {
    /// The limit applied to this response.
    pub limit: usize,
    /// The offset applied to this response.
    pub offset: usize,
    /// The number of rows actually returned.
    pub returned: usize,
}

// -- validation --------------------------------------------------------------

/// Response from the validate endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationResponse {
    /// Whether the query is valid.
    pub valid: bool,
    /// Validation error messages (empty if valid).
    pub errors: Vec<String>,
}

// -- queries (active/recent) -------------------------------------------------

/// Active and recent queries response from the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueriesResponse {
    /// Currently executing queries.
    pub active: Vec<ActiveQuerySnapshot>,
    /// Recently completed queries (most recent first).
    pub recent: Vec<CompletedQuerySnapshot>,
}

/// Snapshot of a currently executing query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveQuerySnapshot {
    /// Monotonic query ID.
    pub id: u64,
    /// Authenticated user name.
    pub user: String,
    /// User's role.
    pub role: String,
    /// The DSL query string.
    pub query: String,
    /// How long the query has been running (ms).
    pub running_ms: u64,
}

/// A completed query from recent history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletedQuerySnapshot {
    /// Monotonic query ID.
    pub id: u64,
    /// Authenticated user name.
    pub user: String,
    /// The DSL query string.
    pub query: String,
    /// Execution duration in milliseconds.
    pub duration_ms: u64,
    /// Row count (if successful).
    pub rows: Option<usize>,
    /// Error message (if failed).
    pub error: Option<String>,
    /// Whether the query exceeded the timeout.
    pub timed_out: bool,
}

/// Response from the cancel query endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelResponse {
    /// Whether the query was found and cancelled.
    pub cancelled: bool,
    /// The query ID that was requested for cancellation.
    pub query_id: u64,
}

// -- stats -------------------------------------------------------------------

/// Response from the stats endpoint (admin only).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatsResponse {
    /// Server uptime in seconds.
    pub uptime_secs: u64,
    /// Total queries executed since startup.
    pub total_queries: u64,
    /// Currently executing queries.
    pub active_queries: usize,
    /// Available connection pool slots.
    pub pool_available: usize,
    /// Total connection pool capacity.
    pub pool_capacity: usize,
}

/// Response from the field values endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldValuesResponse {
    /// The field name that was sampled.
    pub field: String,
    /// Distinct values found.
    pub values: Vec<String>,
    /// Whether this result was served from cache.
    pub cached: bool,
}

// -- history -----------------------------------------------------------------

/// Response from the history endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryResponse {
    /// Query history entries (most recent first).
    pub entries: Vec<HistoryEntryResponse>,
    /// Total number of history entries for this user.
    pub total: usize,
}

/// A single query history entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntryResponse {
    /// History entry ID.
    pub id: i64,
    /// The DSL query string.
    pub query: String,
    /// When the query was executed (ISO 8601 UTC).
    pub executed_at: String,
    /// Execution duration in milliseconds.
    pub duration_ms: u64,
    /// Number of rows returned.
    pub row_count: usize,
    /// Query status ("success", "error", "timeout").
    pub status: String,
}

// -- saved queries -----------------------------------------------------------

/// Response from the list saved queries endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListSavedResponse {
    /// Saved queries (sorted by name).
    pub queries: Vec<SavedQueryResponse>,
}

/// A single saved query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedQueryResponse {
    /// Saved query ID.
    pub id: i64,
    /// Query name.
    pub name: String,
    /// The DSL query string.
    pub query: String,
    /// When the query was created (ISO 8601 UTC).
    pub created_at: String,
    /// When the query was last updated (ISO 8601 UTC).
    pub updated_at: String,
}

/// Response from the delete saved query endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteSavedResponse {
    /// Whether the query was successfully deleted.
    pub deleted: bool,
}

// -- ingest ------------------------------------------------------------------

/// Response from the ingest endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestResponse {
    /// Number of records accepted.
    pub accepted: usize,
}
