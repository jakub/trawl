//! Shared wire types for the fleet HTTP API.
//!
//! These types are used by both `fleet-server` (serialization) and
//! `fleet-client` (deserialization) to ensure the API contract stays
//! in sync across crates.

use fleet_engine::value::{QueryResult, SchemaColumn};
use serde::{Deserialize, Serialize};
use std::fmt;

// -- common enums ------------------------------------------------------------

/// Health status reported by the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthStatus {
    /// Service is healthy.
    Ok,
}

/// Outcome status of a completed query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QueryStatus {
    /// Query completed successfully.
    Success,
    /// Query failed with an error.
    Error,
    /// Query exceeded the configured timeout.
    Timeout,
}

impl fmt::Display for QueryStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Success => f.write_str("success"),
            Self::Error => f.write_str("error"),
            Self::Timeout => f.write_str("timeout"),
        }
    }
}

impl std::str::FromStr for QueryStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "success" => Ok(Self::Success),
            "error" => Ok(Self::Error),
            "timeout" => Ok(Self::Timeout),
            other => Err(format!("unknown query status: {other}")),
        }
    }
}

/// Export output format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
    /// RFC 4180 CSV.
    Csv,
}

impl fmt::Display for ExportFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Csv => f.write_str("csv"),
        }
    }
}

// -- error -------------------------------------------------------------------

/// Standard error response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
    /// Human-readable error message.
    pub error: String,
}

// -- request types -----------------------------------------------------------

/// Request body for query execution (`POST /api/v1/query`) and
/// validation (`POST /api/v1/validate`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryRequest {
    /// The fleet DSL query string.
    pub query: String,
    /// Optional limit for pagination (defaults to server's `max_result_rows`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    /// Optional offset for pagination (defaults to 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,
}

/// Request body for creating a saved query (`POST /api/v1/saved`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSavedRequest {
    /// Display name for the saved query.
    pub name: String,
    /// The fleet DSL query string.
    pub query: String,
}

/// Request body for updating a saved query (`PUT /api/v1/saved/{id}`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateSavedRequest {
    /// The new fleet DSL query string.
    pub query: String,
}

/// Request body for exporting query results (`POST /api/v1/export`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportRequest {
    /// The fleet DSL query string.
    pub query: String,
    /// Optional row limit (defaults to server's `max_export_rows`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

// -- health ------------------------------------------------------------------

/// Health check response from the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    /// Daemon health status.
    pub status: HealthStatus,
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
    /// Query outcome.
    pub status: QueryStatus,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip a type through JSON serialization.
    fn roundtrip<T: Serialize + for<'de> Deserialize<'de> + std::fmt::Debug>(value: &T) -> T {
        let json = serde_json::to_string(value).expect("serialize");
        serde_json::from_str(&json).expect("deserialize")
    }

    #[test]
    fn query_status_roundtrip() {
        for status in [
            QueryStatus::Success,
            QueryStatus::Error,
            QueryStatus::Timeout,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let parsed: QueryStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(status, parsed);
        }
    }

    #[test]
    fn query_status_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&QueryStatus::Success).unwrap(),
            "\"success\""
        );
        assert_eq!(
            serde_json::to_string(&QueryStatus::Error).unwrap(),
            "\"error\""
        );
        assert_eq!(
            serde_json::to_string(&QueryStatus::Timeout).unwrap(),
            "\"timeout\""
        );
    }

    #[test]
    fn health_status_roundtrip() {
        let json = serde_json::to_string(&HealthStatus::Ok).unwrap();
        assert_eq!(json, "\"ok\"");
        let parsed: HealthStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, HealthStatus::Ok);
    }

    #[test]
    fn export_format_display() {
        assert_eq!(ExportFormat::Csv.to_string(), "csv");
    }

    #[test]
    fn error_response_roundtrip() {
        let resp = ErrorResponse {
            error: "something broke".into(),
        };
        let rt = roundtrip(&resp);
        assert_eq!(rt.error, "something broke");
    }

    #[test]
    fn query_request_roundtrip() {
        let req = QueryRequest {
            query: "level:error | stats count()".into(),
            limit: Some(100),
            offset: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        // skip_serializing_if means offset should be absent
        assert!(!json.contains("offset"));
        let rt: QueryRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.query, req.query);
        assert_eq!(rt.limit, Some(100));
        assert_eq!(rt.offset, None);
    }

    #[test]
    fn query_request_defaults() {
        let json = r#"{"query":"*"}"#;
        let req: QueryRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.query, "*");
        assert_eq!(req.limit, None);
        assert_eq!(req.offset, None);
    }

    #[test]
    fn history_entry_uses_query_status() {
        let entry = HistoryEntryResponse {
            id: 1,
            query: "* | head 5".into(),
            executed_at: "2026-01-01T00:00:00Z".into(),
            duration_ms: 42,
            row_count: 5,
            status: QueryStatus::Success,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"success\""));
        let rt: HistoryEntryResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.status, QueryStatus::Success);
    }

    #[test]
    fn health_response_format() {
        let resp = HealthResponse {
            status: HealthStatus::Ok,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"ok\""));
    }
}
