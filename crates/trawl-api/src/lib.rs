// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared wire types for the trawl HTTP API.
//!
//! These types are used by both `trawl-server` (serialization) and
//! `trawl-client` (deserialization) to ensure the API contract stays
//! in sync across crates.

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};

pub mod display;
pub mod value;

use crate::value::{QueryResult, SchemaColumn};

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
    /// Authenticated but not authorized for this app (403) — e.g. a fleet
    /// key with no trawl grant.
    Forbidden,
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
    /// Optional new name (must match `[a-zA-Z0-9_-]+`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
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
    /// Canonically formatted query (present only when valid).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub formatted: Option<String>,
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
    /// The key's role names, comma-joined and sorted (`"none"` when the
    /// key holds no roles). Wire field name kept from the one-role era.
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

/// Identity discriminator orthogonal to roles.
///
/// Distinguishes interactive principals (humans logging into the UI) from
/// non-interactive ones (services calling the API). Has no effect on
/// authorization on its own — apps can use it for richer audit context
/// or to gate features (e.g. require human consent for destructive ops).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PrincipalKind {
    /// Interactive principal — a person using the UI or CLI.
    Human,
    /// Non-interactive principal — a service or automation account.
    Service,
}

impl PrincipalKind {
    /// String representation used on the wire and in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Service => "service",
        }
    }
}

impl fmt::Display for PrincipalKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for PrincipalKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "human" => Ok(Self::Human),
            "service" => Ok(Self::Service),
            other => Err(format!("unknown principal kind: {other}")),
        }
    }
}

/// Response from the whoami endpoint — token identity, roles, and the
/// resolved trawl permission set.
///
/// `roles` carries the names of every data-defined role the key holds
/// (display/audit; roles are cross-app bundles and are NOT app-scoped —
/// ADR-0006). `permissions` is the server-resolved permission union for
/// THIS server's app namespace (`"trawl"`), in canonical order; only
/// permissions this server recognizes appear.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhoAmIResponse {
    /// Key prefix (stable 8-char fingerprint). Intended as an immutable
    /// actor identifier for downstream audit logging — `name` can change,
    /// `prefix` cannot.
    pub prefix: String,
    /// Key name (human-readable label).
    pub name: String,
    /// Whether the underlying principal is a human or a service.
    pub kind: PrincipalKind,
    /// Names of every role attached to this key, sorted.
    pub roles: Vec<String>,
    /// Recognized trawl permissions resolved for this key on THIS server
    /// (e.g. `"query"`, `"server_manage"`), in canonical order. Empty when
    /// the key holds no recognized trawl permission.
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

    // -- syslog --
    /// Whether the syslog listener is enabled.
    #[serde(default)]
    pub syslog_enabled: bool,
    /// Total events received via UDP syslog.
    #[serde(default)]
    pub syslog_events_udp: u64,
    /// Total events received via TCP syslog.
    #[serde(default)]
    pub syslog_events_tcp: u64,
    /// EMA-smoothed syslog event rate (events/sec).
    #[serde(default)]
    pub syslog_rate: f64,
    /// Total unparseable syslog messages.
    #[serde(default)]
    pub syslog_parse_errors: u64,
    /// Total syslog events dropped due to backpressure.
    #[serde(default)]
    pub syslog_dropped: u64,
    /// Current active syslog TCP connections.
    #[serde(default)]
    pub syslog_tcp_connections: u64,

    // -- WAL --
    /// Pending WAL file count.
    #[serde(default)]
    pub wal_files: u64,
    /// Total byte size of pending WAL files.
    #[serde(default)]
    pub wal_bytes: u64,

    // -- compaction --
    /// Seconds since last successful compaction (None if never run).
    #[serde(default)]
    pub last_compaction_secs: Option<u64>,
    /// Total successful compaction cycles since startup.
    #[serde(default)]
    pub compaction_runs: u64,
    /// Total failed compaction cycles since startup.
    #[serde(default)]
    pub compaction_errors: u64,

    // -- storage --
    /// Total parquet files on disk.
    #[serde(default)]
    pub parquet_files: u64,
    /// Total byte size of parquet files.
    #[serde(default)]
    pub parquet_bytes: u64,

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

// -- service schema (rich per-service metadata) ------------------------------

/// Rich per-service schema response, powered by background parquet metadata scan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceSchemaResponse {
    /// Per-service schema and statistics.
    pub services: Vec<ServiceSchema>,
    /// Whether this result was served from cache.
    pub cached: bool,
    /// Current hot buffer event count (always fresh, never cached).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hot_buffer_events: Option<u64>,
    /// Current hot buffer byte size (always fresh, never cached).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hot_buffer_bytes: Option<u64>,
}

/// Schema and statistics for a single service.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceSchema {
    /// Service name (derived from parquet filename stem).
    pub name: String,
    /// Per-column statistics from parquet metadata.
    pub columns: Vec<ServiceColumnStats>,
    /// Earliest date directory containing data for this service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub earliest_date: Option<String>,
    /// Latest date directory containing data for this service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_date: Option<String>,
    /// Number of parquet files for this service.
    pub file_count: u64,
    /// Total bytes on disk for this service.
    pub total_bytes: u64,
    /// Total row count across all files (from parquet metadata `num_rows`).
    pub total_events: u64,
    /// Per-day event counts for sparkline rendering.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub daily_event_counts: Vec<DailyCount>,
}

/// Per-column statistics extracted from parquet row group metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceColumnStats {
    /// Column name.
    pub name: String,
    /// Column data type (e.g. "VARCHAR", "TIMESTAMP").
    #[serde(rename = "type")]
    pub data_type: String,
    /// Exact null count from parquet metadata.
    pub null_count: u64,
    /// Total values (rows) across all row groups.
    pub total_count: u64,
    /// Minimum value (stringified from parquet column stats).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_value: Option<String>,
    /// Maximum value (stringified from parquet column stats).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_value: Option<String>,
    /// Total compressed size of this column on disk (bytes).
    pub compressed_bytes: u64,
}

/// A single day's event count for sparkline rendering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyCount {
    /// Date in YYYY-MM-DD format.
    pub date: String,
    /// Number of events on this date.
    pub count: u64,
}

// -- field catalog (schema-health surfaces, #51) ------------------------------

/// Response from `GET /api/v1/schema/fields` — the pinned-field listing
/// with aggregated observation and conflict evidence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogFieldsResponse {
    /// Pinned fields (envelope-first display order).
    pub fields: Vec<CatalogFieldSummary>,
    /// Total pins in the catalog (unfiltered).
    pub pinned_total: u64,
    /// The pin-count ceiling the total is measured against.
    pub pin_capacity: u64,
    /// Whether the listing was cut short by the (clamped) limit.
    pub truncated: bool,
}

/// One pinned field with its aggregates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogFieldSummary {
    /// Field name.
    pub name: String,
    /// Pinned `DuckDB` type (e.g. "BIGINT").
    #[serde(rename = "type")]
    pub data_type: String,
    /// Which service's batch set the pin (`_declared` for the envelope).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_from: Option<String>,
    /// When the pin was written (ISO 8601 UTC).
    pub pinned_at: String,
    /// Distinct services that ever carried the field.
    pub service_count: u64,
    /// Cumulative rows across all services' observations.
    pub row_count: u64,
    /// Earliest observation (ISO 8601 UTC; absent when never observed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_seen: Option<String>,
    /// Most recent observation (ISO 8601 UTC; absent when never observed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<String>,
    /// Conflict evidence rows currently retained.
    pub conflict_count: u64,
    /// Total rows nulled across the retained evidence.
    pub rows_nulled: u64,
}

/// Response from `GET /api/v1/schema/field?name=` — one field's pin,
/// per-service observations, and recent conflict evidence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogFieldResponse {
    /// Field name (catalog spelling — ASCII-lowercase).
    pub name: String,
    /// Pinned `DuckDB` type.
    #[serde(rename = "type")]
    pub data_type: String,
    /// Which service's batch set the pin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_from: Option<String>,
    /// When the pin was written (ISO 8601 UTC).
    pub pinned_at: String,
    /// One PAGE of per-service observations, most recent first. The service
    /// axis is client-chosen and never pruned, so the detail never carries
    /// the whole history — page with `services_cursor`.
    pub services: Vec<CatalogFieldServiceRow>,
    /// Opaque cursor for the next page of `services` (pass as `?after=`).
    /// Absent on the last page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub services_cursor: Option<String>,
    /// Retained conflict evidence, most recent first. Bounded by the
    /// catalog's per-field evidence cap, so it needs no cursor.
    pub conflicts: Vec<CatalogConflictRow>,
}

/// One service's observation of a field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogFieldServiceRow {
    /// Service name.
    pub service: String,
    /// First observation (ISO 8601 UTC).
    pub first_seen: String,
    /// Most recent observation (ISO 8601 UTC).
    pub last_seen: String,
    /// Cumulative rows in batches that wrote the field.
    pub row_count: u64,
}

/// Response from `GET /api/v1/schema/conflicts` — the schema-health
/// dashboard listing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogConflictsResponse {
    /// Conflict evidence, most recent first.
    pub conflicts: Vec<CatalogConflictRow>,
    /// Whether the listing was cut short by the (clamped) limit.
    pub truncated: bool,
}

/// One recorded conflict: a batch column whose conforming cast nulled rows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogConflictRow {
    /// Field name.
    pub field: String,
    /// Service whose batch disagreed with the pin.
    pub service: String,
    /// The `DuckDB` type the batch actually carried.
    pub observed_type: String,
    /// The pinned type the values were cast to.
    pub expected_type: String,
    /// Rows whose value the cast nulled (recoverable from `_raw`).
    pub rows_nulled: u64,
    /// When the conflict was recorded (ISO 8601 UTC).
    pub at: String,
}

// -- repin (ADR-0011 slice B) ------------------------------------------------

/// Request body for `POST /api/v1/schema/repin`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepinRequest {
    /// The field to repin (folded to the catalog's ASCII-lowercase key).
    pub field: String,
    /// Target candidate-ladder type (`BIGINT`, `DOUBLE`, `TIMESTAMP`,
    /// `BOOLEAN`, `VARCHAR`; case-insensitive).
    pub to: String,
    /// Scan and report only — no mutation.
    #[serde(default)]
    pub dry_run: bool,
    /// Accept a lossy projection (`projected_nulls > 0`), or run a
    /// resurrection-only pass when `to` equals the current pin.
    #[serde(default)]
    pub force: bool,
}

/// One repin job — the dry-run report and the progress/outcome record are
/// the same shape (they are the same postgres row).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepinJobResponse {
    /// Job id.
    pub id: i64,
    /// The repinned field.
    pub field: String,
    /// The pin at claim time.
    pub from_type: String,
    /// The target pin.
    pub to_type: String,
    /// Whether the job stopped after the scan.
    pub dry_run: bool,
    /// Whether a lossy projection was explicitly accepted.
    pub force: bool,
    /// `running`, `succeeded`, `failed`, `refused_needs_force`, `blocked`.
    pub status: String,
    /// Requesting key's display name.
    pub requested_by: Option<String>,
    /// Claim instant (ISO 8601 UTC).
    pub started_at: String,
    /// Terminal instant (ISO 8601 UTC).
    pub finished_at: Option<String>,
    /// Terminal error text, when any.
    pub error: Option<String>,
    /// Scan plan: affected files.
    pub files_total: u64,
    /// Scan plan: rows carrying a stored value for the field.
    pub rows_carrying: u64,
    /// Scan plan: stored values the new pin cannot keep.
    pub projected_nulls: u64,
    /// Scan plan: shelved values `_raw` gives back under the new pin.
    pub resurrectable: u64,
    /// Scan plan: bytes across the affected files (held twice until the
    /// job's final sweep).
    pub affected_bytes: u64,
    /// Progress: affected files rewritten so far.
    pub files_done: u64,
    /// Outcome: rows written through the rewrite.
    pub rows_rewritten: u64,
    /// Outcome: stored values the rewrite nulled.
    pub rows_nulled: u64,
    /// Outcome: values resurrected from `_raw`.
    pub rows_resurrected: u64,
}

/// Response body for `POST /api/v1/schema/repin`. The HTTP status carries
/// the verdict: 200 = dry-run report, 202 = rewrite started, 409 = lossy
/// without force (the job is terminal `refused_needs_force` and this body
/// IS the plan the refusal is based on).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepinResponse {
    /// The job row.
    pub job: RepinJobResponse,
}

/// Response from `GET /api/v1/schema/repin/status`: the running job if
/// any, else the newest job of any status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepinStatusResponse {
    /// The job, or `None` when no repin has ever run.
    pub job: Option<RepinJobResponse>,
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
    /// Filesystem path to the parquet result file (relative to data dir).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_path: Option<String>,
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

/// A report run enriched with the owning saved query's name and ID.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalRunSummary {
    /// Saved query ("net") ID.
    pub net_id: i64,
    /// Saved query name.
    pub net_name: String,
    /// Run details.
    #[serde(flatten)]
    pub run: ReportRunSummary,
}

/// Paginated list of runs across all saved queries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListAllRunsResponse {
    /// Runs (most recent first).
    pub runs: Vec<GlobalRunSummary>,
    /// Total number of runs for this user.
    pub total: usize,
}

/// Aggregate statistics across all report runs for a user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunsStatsResponse {
    /// Total number of runs (all statuses).
    pub total_runs: u64,
    /// Number of successful runs.
    pub success_count: u64,
    /// Number of failed runs.
    pub error_count: u64,
    /// Number of timed-out runs.
    pub timeout_count: u64,
    /// Average execution duration in milliseconds (None if no completed runs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_duration_ms: Option<u64>,
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
    fn catalog_fields_response_roundtrip() {
        let resp = CatalogFieldsResponse {
            fields: vec![CatalogFieldSummary {
                name: "duration".into(),
                data_type: "BIGINT".into(),
                pinned_from: Some("nginx".into()),
                pinned_at: "2026-08-01T10:00:00Z".into(),
                service_count: 2,
                row_count: 5,
                first_seen: Some("2026-08-01T10:00:00Z".into()),
                last_seen: Some("2026-08-02T10:00:00Z".into()),
                conflict_count: 1,
                rows_nulled: 1,
            }],
            pinned_total: 12,
            pin_capacity: 10_000,
            truncated: false,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(
            json.contains("\"type\":\"BIGINT\""),
            "the type key follows the schema-response convention: {json}"
        );
        let rt = roundtrip(&resp);
        assert_eq!(rt.fields.len(), 1);
        assert_eq!(rt.fields[0].name, "duration");
        assert_eq!(rt.fields[0].data_type, "BIGINT");
        assert_eq!(rt.fields[0].service_count, 2);
        assert_eq!(rt.pinned_total, 12);
        assert_eq!(rt.pin_capacity, 10_000);
        assert!(!rt.truncated);
    }

    #[test]
    fn catalog_field_response_roundtrip() {
        let resp = CatalogFieldResponse {
            name: "duration".into(),
            data_type: "BIGINT".into(),
            pinned_from: Some("nginx".into()),
            pinned_at: "2026-08-01T10:00:00Z".into(),
            services: vec![CatalogFieldServiceRow {
                service: "nginx".into(),
                first_seen: "2026-08-01T10:00:00Z".into(),
                last_seen: "2026-08-02T10:00:00Z".into(),
                row_count: 5,
            }],
            services_cursor: Some("2026-08-02T10:00:00.000000Z|nginx".into()),
            conflicts: vec![CatalogConflictRow {
                field: "duration".into(),
                service: "envoy".into(),
                observed_type: "VARCHAR".into(),
                expected_type: "BIGINT".into(),
                rows_nulled: 1,
                at: "2026-08-02T11:00:00Z".into(),
            }],
        };
        let rt = roundtrip(&resp);
        assert_eq!(rt.name, "duration");
        assert_eq!(
            rt.services_cursor.as_deref(),
            Some("2026-08-02T10:00:00.000000Z|nginx"),
            "the page cursor survives the wire"
        );
        assert_eq!(rt.services.len(), 1);
        assert_eq!(rt.services[0].service, "nginx");
        assert_eq!(rt.conflicts.len(), 1);
        assert_eq!(rt.conflicts[0].expected_type, "BIGINT");
    }

    #[test]
    fn catalog_conflicts_response_roundtrip() {
        let resp = CatalogConflictsResponse {
            conflicts: vec![CatalogConflictRow {
                field: "duration".into(),
                service: "envoy".into(),
                observed_type: "VARCHAR".into(),
                expected_type: "BIGINT".into(),
                rows_nulled: 3,
                at: "2026-08-02T11:00:00Z".into(),
            }],
            truncated: true,
        };
        let rt = roundtrip(&resp);
        assert_eq!(rt.conflicts.len(), 1);
        assert_eq!(rt.conflicts[0].rows_nulled, 3);
        assert!(rt.truncated);
    }

    /// The `/api/v1/schema/services` wire shape is the TUI and SPA contract
    /// for the #51 cutover: types now come from the catalog, but the JSON
    /// keys must stay byte-identical. Golden-pin every key.
    #[test]
    fn service_schema_wire_keys_are_pinned() {
        let resp = ServiceSchemaResponse {
            services: vec![ServiceSchema {
                name: "nginx".into(),
                columns: vec![ServiceColumnStats {
                    name: "status".into(),
                    data_type: "BIGINT".into(),
                    null_count: 0,
                    total_count: 1,
                    min_value: Some("1".into()),
                    max_value: Some("2".into()),
                    compressed_bytes: 10,
                }],
                earliest_date: Some("2026-01-01".into()),
                latest_date: Some("2026-01-02".into()),
                file_count: 1,
                total_bytes: 10,
                total_events: 1,
                daily_event_counts: vec![DailyCount {
                    date: "2026-01-01".into(),
                    count: 1,
                }],
            }],
            cached: true,
            hot_buffer_events: Some(1),
            hot_buffer_bytes: Some(2),
        };
        let json: serde_json::Value = serde_json::to_value(&resp).unwrap();

        // serde_json::Value sorts object keys, so pin the sorted key SETS —
        // key names are the wire contract, their order is not.
        let top: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            top,
            vec![
                "cached",
                "hot_buffer_bytes",
                "hot_buffer_events",
                "services"
            ]
        );

        let svc: Vec<&str> = json["services"][0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            svc,
            vec![
                "columns",
                "daily_event_counts",
                "earliest_date",
                "file_count",
                "latest_date",
                "name",
                "total_bytes",
                "total_events"
            ]
        );

        let col: Vec<&str> = json["services"][0]["columns"][0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            col,
            vec![
                "compressed_bytes",
                "max_value",
                "min_value",
                "name",
                "null_count",
                "total_count",
                "type"
            ]
        );
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
            prefix: "abcd1234".into(),
            name: "dev-key".into(),
            kind: PrincipalKind::Human,
            roles: vec!["trawl-admin".into()],
            permissions: vec!["query".into(), "schema_read".into(), "server_manage".into()],
        };
        let rt = roundtrip(&resp);
        assert_eq!(rt.prefix, "abcd1234");
        assert_eq!(rt.name, "dev-key");
        assert_eq!(rt.kind, PrincipalKind::Human);
        assert_eq!(rt.roles, vec!["trawl-admin".to_owned()]);
        assert_eq!(rt.permissions.len(), 3);
        assert!(rt.permissions.contains(&"server_manage".to_owned()));
    }

    #[test]
    fn whoami_multi_role_roundtrip() {
        // Roles are cross-app bundles: ALL role names travel, unfiltered;
        // permissions stay trawl-scoped.
        let resp = WhoAmIResponse {
            prefix: "abcd1234".into(),
            name: "siem-bot".into(),
            kind: PrincipalKind::Service,
            roles: vec!["coastwatch-siem_consumer".into(), "trawl-analyst".into()],
            permissions: vec!["query".into(), "schema_read".into()],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"roles\""));
        assert!(
            !json.contains("assignments"),
            "the (app, role) assignments wire field is retired"
        );
        let rt = roundtrip(&resp);
        assert_eq!(rt.kind, PrincipalKind::Service);
        assert_eq!(
            rt.roles,
            vec![
                "coastwatch-siem_consumer".to_owned(),
                "trawl-analyst".to_owned()
            ]
        );
        assert_eq!(
            rt.permissions,
            vec!["query".to_owned(), "schema_read".to_owned()]
        );
    }

    #[test]
    fn principal_kind_wire_format_is_lowercase() {
        let json = serde_json::to_string(&PrincipalKind::Human).unwrap();
        assert_eq!(json, "\"human\"");
        let parsed: PrincipalKind = serde_json::from_str("\"service\"").unwrap();
        assert_eq!(parsed, PrincipalKind::Service);
    }

    #[test]
    fn service_schema_response_roundtrip() {
        let resp = ServiceSchemaResponse {
            services: vec![ServiceSchema {
                name: "nginx".into(),
                columns: vec![ServiceColumnStats {
                    name: "status".into(),
                    data_type: "INTEGER".into(),
                    null_count: 5,
                    total_count: 1000,
                    min_value: Some("100".into()),
                    max_value: Some("599".into()),
                    compressed_bytes: 4096,
                }],
                earliest_date: Some("2026-01-01".into()),
                latest_date: Some("2026-03-18".into()),
                file_count: 100,
                total_bytes: 1_048_576,
                total_events: 500_000,
                daily_event_counts: vec![DailyCount {
                    date: "2026-03-18".into(),
                    count: 12_345,
                }],
            }],
            cached: true,
            hot_buffer_events: Some(42),
            hot_buffer_bytes: Some(8192),
        };
        let rt = roundtrip(&resp);
        assert_eq!(rt.services.len(), 1);
        assert_eq!(rt.services[0].name, "nginx");
        assert_eq!(rt.services[0].columns.len(), 1);
        assert_eq!(rt.services[0].columns[0].null_count, 5);
        assert_eq!(rt.services[0].total_events, 500_000);
        assert_eq!(rt.services[0].daily_event_counts.len(), 1);
        assert!(rt.cached);
        assert_eq!(rt.hot_buffer_events, Some(42));
    }

    #[test]
    fn service_schema_response_empty_omits_optional() {
        let resp = ServiceSchemaResponse {
            services: vec![],
            cached: false,
            hot_buffer_events: None,
            hot_buffer_bytes: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("hot_buffer"));
        let rt: ServiceSchemaResponse = serde_json::from_str(&json).unwrap();
        assert!(rt.services.is_empty());
        assert!(!rt.cached);
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
            syslog_enabled: true,
            syslog_events_udp: 1_247_829,
            syslog_events_tcp: 89_341,
            syslog_rate: 530.0,
            syslog_parse_errors: 12,
            syslog_dropped: 3,
            syslog_tcp_connections: 2,
            wal_files: 12,
            wal_bytes: 4_404_019,
            last_compaction_secs: Some(3),
            compaction_runs: 1247,
            compaction_errors: 0,
            parquet_files: 847,
            parquet_bytes: 13_312_000_000,
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
        assert!(rt.syslog_enabled);
        assert_eq!(rt.syslog_events_udp, 1_247_829);
        assert_eq!(rt.wal_files, 12);
        assert_eq!(rt.compaction_runs, 1247);
        assert_eq!(rt.parquet_files, 847);
        assert_eq!(rt.recent_queries.len(), 1);
        assert_eq!(rt.active_queries.len(), 1);
    }
}
