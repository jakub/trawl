//! Shared wire types for the trawl HTTP API.
//!
//! These types are used by both `trawl-server` (serialization) and
//! `trawl-client` (deserialization) to ensure the API contract stays
//! in sync across crates.

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use trawl_engine::value::{QueryResult, SchemaColumn};

// -- common enums ------------------------------------------------------------

/// Health status reported by the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthStatus {
    /// All subsystems are healthy.
    Ok,
    /// Non-critical subsystem(s) failed; queries still work.
    Degraded,
    /// Critical subsystem(s) failed; service is not ready.
    Unavailable,
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
    /// Newline-delimited JSON (one object per line).
    Json,
    /// Apache Parquet (columnar, Snappy-compressed).
    Parquet,
}

impl fmt::Display for ExportFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Csv => f.write_str("csv"),
            Self::Json => f.write_str("json"),
            Self::Parquet => f.write_str("parquet"),
        }
    }
}

// -- error -------------------------------------------------------------------

/// Machine-readable error code for structured error responses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// DSL syntax error (carries spans).
    ParseError,
    /// Semantic validation failure (unknown function, bad arity).
    ValidationError,
    /// Query execution failure (details redacted).
    ExecutionError,
    /// Result set exceeded the configured row limit.
    ResultTooLarge,
    /// Authentication failure.
    AuthError,
    /// Insufficient permissions.
    Unauthorized,
    /// Generic malformed input.
    BadRequest,
    /// Resource not found.
    NotFound,
    /// Query exceeded time limit.
    Timeout,
    /// Ingestion validation failure.
    IngestError,
    /// Rate limit exceeded (429).
    RateLimited,
    /// Too many concurrent SSE streams.
    TooManyStreams,
    /// Internal server error (500).
    InternalError,
    /// Service temporarily unavailable (503).
    ServiceUnavailable,
}

/// Source location within a query string.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorSpan {
    /// Byte offset of the start of the error region.
    pub start: usize,
    /// Byte offset of the end of the error region.
    pub end: usize,
}

/// A single diagnostic detail within a structured error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorDetail {
    /// Human-readable description of this specific error.
    pub message: String,
    /// Source span within the query text (if available).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<ErrorSpan>,
    /// Parser context label (e.g. "pipeline", "expression").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Contextual suggestion (e.g. "did you mean 'stats'?").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl fmt::Display for ErrorDetail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(ref span) = self.span {
            write!(f, "[{}..{}] {}", span.start, span.end, self.message)?;
        } else {
            write!(f, "{}", self.message)?;
        }
        if let Some(ref label) = self.label {
            write!(f, " (while parsing {label})")?;
        }
        if let Some(ref hint) = self.hint {
            write!(f, " ({hint})")?;
        }
        Ok(())
    }
}

/// Structured error envelope with machine-readable code and optional diagnostics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorEnvelope {
    /// Machine-readable error category.
    pub code: ErrorCode,
    /// Human-readable summary message.
    pub message: String,
    /// Detailed diagnostics (spans, sub-errors). Empty for spanless errors.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<ErrorDetail>,
}

impl ErrorEnvelope {
    /// Create a simple envelope with no diagnostic details.
    pub fn simple(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: Vec::new(),
        }
    }
}

/// Standard error response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
    /// Structured error envelope.
    pub error: ErrorEnvelope,
}

// -- request types -----------------------------------------------------------

/// Request body for query execution (`POST /api/v1/query`) and
/// validation (`POST /api/v1/validate`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryRequest {
    /// The trawl DSL query string.
    pub query: String,
    /// Optional limit for pagination (defaults to server's `max_result_rows`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    /// Optional offset for pagination (defaults to 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,
    /// Timezone for timestamp display (e.g. `"UTC"`, `"local"`, `"+05:30"`).
    /// When absent, the server uses UTC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

/// Request body for creating a saved query (`POST /api/v1/saved`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSavedRequest {
    /// Display name for the saved query.
    pub name: String,
    /// The trawl DSL query string.
    pub query: String,
}

/// Request body for updating a saved query (`PUT /api/v1/saved/{id}`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateSavedRequest {
    /// The new trawl DSL query string.
    pub query: String,
}

/// Request body for exporting query results (`POST /api/v1/export`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportRequest {
    /// The trawl DSL query string.
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
    /// Per-subsystem check results (`"ok"` or `"error: ..."`).
    ///
    /// Absent in minimal responses for backward compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks: Option<HashMap<String, String>>,
    /// Server version string (e.g. `"0.1.3"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
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
    /// Earliest date found in the partition directory structure (YYYY-MM-DD).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub earliest_date: Option<String>,
    /// Latest date found in the partition directory structure (YYYY-MM-DD).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_date: Option<String>,
    /// Total byte size of all parquet files on disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    /// Distinct service names extracted from parquet filenames.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub services: Option<Vec<String>>,
    /// Current number of events in the hot buffer (not cached).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hot_buffer_events: Option<u64>,
    /// Current byte size of the hot buffer (not cached).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hot_buffer_bytes: Option<u64>,
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
    /// Validation error details with optional span info (empty if valid).
    pub errors: Vec<ErrorDetail>,
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

// -- whoami ------------------------------------------------------------------

/// Response from the whoami endpoint — token identity and permissions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhoAmIResponse {
    /// Key name (human-readable label).
    pub name: String,
    /// Role granted by this key (e.g. "admin", "analyst", "reader", "ingest").
    pub role: String,
    /// Permissions granted by this role (e.g. "query", "`server_manage`").
    pub permissions: Vec<String>,
}

// -- dashboard ---------------------------------------------------------------

/// Full dashboard snapshot returned by the admin-only dashboard endpoint.
///
/// Contains all metrics displayed by the server's live monitor: executor pool,
/// hot buffer, query throughput, ingest rates, SSE connections, scheduler
/// status, and recent/active queries. Rates are pre-computed (EMA-smoothed)
/// by the server — clients render them directly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardSnapshot {
    // -- header --
    /// Server hostname.
    pub hostname: String,
    /// Listen address (e.g. "0.0.0.0:5514").
    pub listen_addr: String,
    /// Server uptime in seconds.
    pub uptime_secs: u64,
    /// Server version string.
    pub version: String,
    /// Whether the server considers itself healthy.
    pub healthy: bool,

    // -- executor pool --
    /// Total executor pool capacity.
    pub pool_capacity: usize,
    /// Currently active (in-use) pool slots.
    pub pool_active: usize,

    // -- hot buffer --
    /// Current event count in the hot buffer.
    pub hot_buffer_events: usize,
    /// Maximum event capacity.
    pub hot_buffer_max_events: usize,
    /// Current byte usage.
    pub hot_buffer_bytes: usize,
    /// Maximum byte capacity.
    pub hot_buffer_max_bytes: usize,
    /// Number of active batches.
    pub hot_buffer_batches: usize,

    // -- query throughput --
    /// Total queries executed since startup.
    pub total_queries: u64,
    /// EMA-smoothed query rate (queries/sec).
    pub query_rate: f64,
    /// Number of recent queries that resulted in errors.
    pub query_errors: u64,
    /// Number of recent queries that timed out.
    pub query_timeouts: u64,

    // -- ingest --
    /// Total events ingested since startup.
    pub ingest_events: u64,
    /// EMA-smoothed ingest rate (events/sec).
    pub ingest_rate: f64,
    /// Total rejected events.
    pub ingest_rejected: u64,

    // -- SSE --
    /// Active SSE streaming connections.
    pub sse_active: usize,
    /// Maximum SSE connections allowed.
    pub sse_max: usize,

    // -- scheduler --
    /// Whether the scheduler is enabled.
    pub scheduler_enabled: bool,
    /// Number of active schedules.
    pub scheduler_schedules: usize,

    // -- queries --
    /// Recently completed queries (most recent first).
    pub recent_queries: Vec<CompletedQuerySnapshot>,
    /// Currently executing queries.
    pub active_queries: Vec<ActiveQuerySnapshot>,
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
    /// Schedule attached to this saved query (if any).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<ScheduleResponse>,
}

/// Response from the delete saved query endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteSavedResponse {
    /// Whether the query was successfully deleted.
    pub deleted: bool,
}

// -- schedules ---------------------------------------------------------------

/// Schedule attached to a saved query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleResponse {
    /// Schedule ID.
    pub id: i64,
    /// The saved query this schedule belongs to.
    pub saved_query_id: i64,
    /// Human-readable interval (e.g. "5m", "1h").
    pub interval: String,
    /// Interval in seconds.
    pub interval_secs: u64,
    /// Maximum number of runs (if set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_runs: Option<u64>,
    /// Whether the schedule is active.
    pub enabled: bool,
    /// When the schedule was created (ISO 8601 UTC).
    pub created_at: String,
    /// When the schedule was last updated (ISO 8601 UTC).
    pub updated_at: String,
    /// Most recent report run summary (if any).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<ReportRunSummary>,
    /// Total number of runs executed.
    pub total_runs: u64,
}

/// Request body to create or update a schedule (`PUT /api/v1/saved/{id}/schedule`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetScheduleRequest {
    /// Interval string (e.g. "5m", "1h", "24h").
    pub interval: String,
    /// Maximum number of runs (omit or null for unlimited).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_runs: Option<u64>,
    /// Whether the schedule is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// Summary of a single report run (no result data).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportRunSummary {
    /// Run ID.
    pub id: i64,
    /// DSL query snapshot at execution time.
    pub query: String,
    /// Run status: "running", "success", "error", "timeout".
    pub status: String,
    /// When the run started (ISO 8601 UTC).
    pub started_at: String,
    /// When the run finished (ISO 8601 UTC).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    /// Execution duration in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Number of result rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_count: Option<usize>,
    /// Error message (if status is "error").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

/// Paginated list of report runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListReportRunsResponse {
    /// Report run summaries (most recent first).
    pub runs: Vec<ReportRunSummary>,
    /// Total number of runs for this schedule.
    pub total: usize,
}

/// Full report run including result data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportRunResponse {
    /// Run summary metadata.
    #[serde(flatten)]
    pub summary: ReportRunSummary,
    /// Query result (decompressed). Absent for error/running runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<QueryResult>,
}

/// Response from deleting a schedule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteScheduleResponse {
    /// Whether a schedule was deleted.
    pub deleted: bool,
}

// -- ingest ------------------------------------------------------------------

/// A per-event error from the ingest endpoint.
///
/// Reported when individual events in a batch fail validation while
/// other events in the same batch succeed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestEventError {
    /// Zero-based event index (array position or ndjson line number including blanks).
    pub index: usize,
    /// Human-readable error description.
    pub message: String,
}

/// Response from the ingest endpoint.
///
/// When all events are valid, `rejected` and `errors` are omitted from
/// the JSON response for backward compatibility.  New clients should
/// use `#[serde(default)]` on these fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestResponse {
    /// Number of records accepted and written to the WAL.
    pub accepted: usize,
    /// Number of records rejected due to per-event validation errors.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub rejected: usize,
    /// Per-event error details (one entry per rejected event).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<IngestEventError>,
}

/// Helper for `skip_serializing_if` — serde requires `&T` signature.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(n: &usize) -> bool {
    *n == 0
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
        for status in [
            HealthStatus::Ok,
            HealthStatus::Degraded,
            HealthStatus::Unavailable,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let parsed: HealthStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, status);
        }
    }

    #[test]
    fn export_format_display() {
        assert_eq!(ExportFormat::Csv.to_string(), "csv");
        assert_eq!(ExportFormat::Json.to_string(), "json");
        assert_eq!(ExportFormat::Parquet.to_string(), "parquet");
    }

    #[test]
    fn export_format_roundtrip() {
        for fmt in [ExportFormat::Csv, ExportFormat::Json, ExportFormat::Parquet] {
            let json = serde_json::to_string(&fmt).unwrap();
            let parsed: ExportFormat = serde_json::from_str(&json).unwrap();
            assert_eq!(fmt, parsed);
        }
    }

    #[test]
    fn error_response_roundtrip() {
        let resp = ErrorResponse {
            error: ErrorEnvelope::simple(ErrorCode::InternalError, "something broke"),
        };
        let rt = roundtrip(&resp);
        assert_eq!(rt.error.code, ErrorCode::InternalError);
        assert_eq!(rt.error.message, "something broke");
        assert!(rt.error.details.is_empty());
    }

    #[test]
    fn error_envelope_with_details_roundtrip() {
        let resp = ErrorResponse {
            error: ErrorEnvelope {
                code: ErrorCode::ParseError,
                message: "parse error".into(),
                details: vec![ErrorDetail {
                    message: "expected pipe stage".into(),
                    span: Some(ErrorSpan { start: 15, end: 21 }),
                    label: Some("pipeline".into()),
                    hint: None,
                }],
            },
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"parse_error\""));
        assert!(json.contains("\"start\":15"));
        let rt: ErrorResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.error.details.len(), 1);
        assert_eq!(rt.error.details[0].span.as_ref().unwrap().start, 15);
    }

    #[test]
    fn error_code_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&ErrorCode::ParseError).unwrap(),
            "\"parse_error\""
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::ResultTooLarge).unwrap(),
            "\"result_too_large\""
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::TooManyStreams).unwrap(),
            "\"too_many_streams\""
        );
    }

    #[test]
    fn error_envelope_simple_omits_empty_details() {
        let envelope = ErrorEnvelope::simple(ErrorCode::Timeout, "query timed out");
        let json = serde_json::to_string(&envelope).unwrap();
        assert!(!json.contains("details"));
    }

    #[test]
    fn query_request_roundtrip() {
        let req = QueryRequest {
            query: "level=error | stats count()".into(),
            limit: Some(100),
            offset: None,
            timezone: None,
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
    fn ingest_response_no_errors_omits_fields() {
        let resp = IngestResponse {
            accepted: 5,
            rejected: 0,
            errors: vec![],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"accepted\":5"));
        assert!(!json.contains("rejected"));
        assert!(!json.contains("errors"));
    }

    #[test]
    fn ingest_response_with_errors_roundtrip() {
        let resp = IngestResponse {
            accepted: 8,
            rejected: 2,
            errors: vec![
                IngestEventError {
                    index: 3,
                    message: "expected JSON object".into(),
                },
                IngestEventError {
                    index: 7,
                    message: "missing 'service' field".into(),
                },
            ],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"rejected\":2"));
        assert!(json.contains("\"errors\""));
        let rt: IngestResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.accepted, 8);
        assert_eq!(rt.rejected, 2);
        assert_eq!(rt.errors.len(), 2);
        assert_eq!(rt.errors[0].index, 3);
        assert_eq!(rt.errors[1].message, "missing 'service' field");
    }

    #[test]
    fn ingest_response_backward_compat_deserialize() {
        // Old servers return only {"accepted": N} — new clients must handle this.
        let json = r#"{"accepted": 5}"#;
        let resp: IngestResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.accepted, 5);
        assert_eq!(resp.rejected, 0);
        assert!(resp.errors.is_empty());
    }

    #[test]
    fn health_response_format() {
        let resp = HealthResponse {
            status: HealthStatus::Ok,
            checks: None,
            version: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"ok\""));
        // checks omitted when None
        assert!(!json.contains("checks"));
        assert!(!json.contains("version"));
    }

    #[test]
    fn health_status_variants_roundtrip() {
        for (status, expected) in [
            (HealthStatus::Ok, "\"ok\""),
            (HealthStatus::Degraded, "\"degraded\""),
            (HealthStatus::Unavailable, "\"unavailable\""),
        ] {
            let json = serde_json::to_string(&status).unwrap();
            assert_eq!(json, expected);
            let parsed: HealthStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, status);
        }
    }

    #[test]
    fn health_response_with_checks_roundtrip() {
        let mut checks = HashMap::new();
        checks.insert("duckdb".into(), "ok".into());
        checks.insert("auth_db".into(), "error: connection refused".into());
        let resp = HealthResponse {
            status: HealthStatus::Degraded,
            checks: Some(checks),
            version: Some("0.1.3".into()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"degraded\""));
        assert!(json.contains("\"checks\""));
        let rt: HealthResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.status, HealthStatus::Degraded);
        assert!(rt.checks.unwrap().contains_key("duckdb"));
    }

    #[test]
    fn health_response_without_checks_deserializes() {
        // Backward compat: old responses without `checks` field still parse.
        let json = r#"{"status":"ok"}"#;
        let resp: HealthResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.status, HealthStatus::Ok);
        assert!(resp.checks.is_none());
    }

    #[test]
    fn whoami_roundtrip() {
        let resp = WhoAmIResponse {
            name: "dev-key".into(),
            role: "admin".into(),
            permissions: vec!["query".into(), "schema_read".into(), "server_manage".into()],
        };
        let rt = roundtrip(&resp);
        assert_eq!(rt.name, "dev-key");
        assert_eq!(rt.role, "admin");
        assert_eq!(rt.permissions.len(), 3);
        assert!(rt.permissions.contains(&"server_manage".to_owned()));
    }

    #[test]
    fn dashboard_snapshot_roundtrip() {
        let snapshot = DashboardSnapshot {
            hostname: "test-host".into(),
            listen_addr: "127.0.0.1:5514".into(),
            uptime_secs: 9240,
            version: "0.1.0".into(),
            healthy: true,
            pool_capacity: 4,
            pool_active: 1,
            hot_buffer_events: 12_847,
            hot_buffer_max_events: 100_000,
            hot_buffer_bytes: 4_404_019,
            hot_buffer_max_bytes: 104_857_600,
            hot_buffer_batches: 23,
            total_queries: 1247,
            query_rate: 2.1,
            query_errors: 12,
            query_timeouts: 3,
            ingest_events: 847_293,
            ingest_rate: 340.0,
            ingest_rejected: 47,
            sse_active: 2,
            sse_max: 32,
            scheduler_enabled: true,
            scheduler_schedules: 3,
            recent_queries: vec![CompletedQuerySnapshot {
                id: 1,
                user: "admin".into(),
                query: "level=error".into(),
                duration_ms: 23,
                rows: Some(42),
                error: None,
                timed_out: false,
            }],
            active_queries: vec![ActiveQuerySnapshot {
                id: 2,
                user: "admin".into(),
                role: "admin".into(),
                query: "* | stats count()".into(),
                running_ms: 500,
            }],
        };
        let rt = roundtrip(&snapshot);
        assert_eq!(rt.hostname, "test-host");
        assert_eq!(rt.uptime_secs, 9240);
        assert_eq!(rt.pool_capacity, 4);
        assert_eq!(rt.hot_buffer_events, 12_847);
        assert_eq!(rt.total_queries, 1247);
        assert!((rt.query_rate - 2.1).abs() < f64::EPSILON);
        assert_eq!(rt.recent_queries.len(), 1);
        assert_eq!(rt.active_queries.len(), 1);
    }
}
