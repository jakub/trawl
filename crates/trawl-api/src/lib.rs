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

pub mod csv;
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
    /// Authenticated but missing the required app permission (403), including
    /// a fleet key with no trawl grant.
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
    /// Per-subsystem check results (`"ok"` or `"error"`).
    ///
    /// The daemon always fills this; optional so a body that omits it parses.
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
    /// Fields the query bound whose catalog pin the analyzer calls degraded
    /// (ADR-0011): results may be missing values that pin shelved. Includes
    /// fields the query filtered on and projected away; absent from the wire
    /// when empty, which is the healthy case.
    ///
    /// Stale by at most one schema-refresh tick, and carried by
    /// `/api/v1/query` only: the SSE stream keeps its compile-time snapshot
    /// semantics, and embedded `--data` mode has no catalog to consult.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub degraded_fields: Vec<String>,
    /// Result columns holding `OTel` severity numbers other than the
    /// envelope's `_severity`: `sev()` output, or a column that took the
    /// slot's pin (ADR-0013).
    ///
    /// Response-level and presentational, like `degraded_fields`. A
    /// per-`Column` field would have to be filled at ~25 construction sites,
    /// and it would say nothing about the values, since every format still
    /// carries the number. Renderers that display tokens (the CLI table, the
    /// TUI, the SPA's results grid) read it; json/csv/SSE ignore it, so an
    /// arithmetic consumer is untouched. Absent from the wire when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub severity_columns: Vec<String>,
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
    pub active: Vec<QueryActiveEntry>,
    /// Recently completed queries (most recent first).
    pub recent: Vec<QueryRecentEntry>,
    /// Work that still holds a pool permit after its request answered
    /// (ADR-0024). Disjoint from `active`: a request whose work is
    /// retained is listed here and not there. The same request may also
    /// appear in `recent`, where it recorded its outcome.
    ///
    /// `default` because a server predating retained accounting sends no
    /// such field.
    #[serde(default)]
    pub retained: Vec<RetainedWorkSnapshot>,
}

/// An active query with ownership relative to the authenticated reader.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryActiveEntry {
    #[serde(flatten)]
    pub snapshot: ActiveQuerySnapshot,
    /// Whether the reader's verified key submitted this query.
    pub own: bool,
}

/// A recent query with ownership relative to the authenticated reader.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryRecentEntry {
    #[serde(flatten)]
    pub snapshot: CompletedQuerySnapshot,
    /// Whether the reader's verified key submitted this query.
    pub own: bool,
}

/// One unit of physical work that outlived the request that started it.
///
/// The request stopped waiting (it timed out, or its caller walked away);
/// the `DuckDB` bind or scan it started did not stop with it, and keeps
/// its permit until it does. Owner key ids never appear here — `user` and
/// `query` are populated only for a reader who may see them, and are
/// omitted from the wire otherwise.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetainedWorkSnapshot {
    /// The pool id, the same id `DELETE /queries/{id}` cancels.
    pub id: u64,
    /// Which door the work came through: `query`, `from_saved`, `export`,
    /// `scheduled`, `ping` or `sample`.
    pub kind: String,
    /// Whether the work passed its work-start transition. A held permit
    /// alone does not prove it started.
    pub started: bool,
    /// How long the work has outlived its request (ms).
    pub retained_ms: u64,
    /// The submitting key's display name, for a reader entitled to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// The DSL, for a reader entitled to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
}

/// Snapshot of a currently executing query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveQuerySnapshot {
    /// Monotonic query ID.
    pub id: u64,
    /// Authenticated user name.
    pub user: String,
    /// The key's role names, comma-joined and sorted (`"none"` when the
    /// key holds no roles).
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
    /// Held permits whose request already answered (ADR-0024): a subset
    /// of the permits `pool_capacity - pool_available` counts, not an
    /// addition to them.
    pub pool_retained: usize,
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
/// (display/audit; roles are cross-app bundles, not app-scoped, ADR-0006).
/// `permissions` is the server-resolved permission union for this server's
/// app namespace (`"trawl"`), in canonical order; only permissions this
/// server recognizes appear.
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
    /// Recognized trawl permissions resolved for this key on this server
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
    /// The subset of `pool_active` held by work no request is waiting on
    /// any more (ADR-0024).
    pub pool_retained: usize,

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
    /// Fields this service has itself conflicted on and which the analyzer
    /// calls degraded (ADR-0011). Stamped from the schema-refresh tick's
    /// in-process snapshot, so it is stale by at most one tick, like the
    /// query notice. A client cannot compute it: carrying a degraded field's
    /// column is not evidence that this service is what degraded it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub degraded_fields: Vec<String>,
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

// -- field catalog (schema-health surfaces) -----------------------------------

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
    /// Total rows nulled across the retained evidence: the trimmed window's
    /// sum, not a lifetime total (see [`DegradedVerdict`]).
    pub rows_nulled: u64,
    /// The analyzer's verdict, present only when the pin is degraded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<DegradedVerdict>,
}

/// The analyzer's verdict on a degraded field (ADR-0011): the pin has been
/// shelving values for long enough, and in enough volume, that an operator
/// should decide whether to repin it.
///
/// Structured facts only, so the consumer writes the words. A stored sentence
/// would fix the phrasing of every present and future surface (CLI table,
/// SPA drawer, an eventual auto-repin's decision record) at the moment the
/// evidence was recorded.
///
/// The verdict is advisory and sender-influenceable by construction: a
/// single misbehaving producer can raise it (there is deliberately no
/// multi-sender gate — a homelab commonly has exactly one producer per
/// field). What it can never do is act: repinning stays behind
/// `schema_write` and a human.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DegradedVerdict {
    /// Earliest conflict evidence for the field (ISO 8601 UTC).
    pub since: String,
    /// Distinct services in the evidence. Displayed, never a gate.
    pub services: u64,
    /// Distinct conflict episodes across every service.
    pub episodes: u64,
    /// Lifetime rows the pin has nulled: durable, and therefore usually
    /// larger than the sibling `rows_nulled` fields, which sum only the
    /// evidence rows still inside the per-field recency window.
    pub rows_shelved: u64,
    /// A bounded sample of the values that were shelved, newest evidence
    /// first. A sample, never a manifest — every value stays in `_raw`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub samples: Vec<String>,
    /// The type the evidence suggests repinning to (a `DuckDB` spelling).
    pub suggested_to: String,
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
    /// One page of per-service observations, most recent first. The service
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
    /// The analyzer's verdict, present only when the pin is degraded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<DegradedVerdict>,
    /// The operator's acknowledgement of the badge, if one stands.
    ///
    /// Independent of `verdict`, and deliberately so: an ack that has been
    /// overtaken by newer evidence appears here beside a re-raised verdict.
    /// The two together are the story ("acknowledged on Tuesday, still
    /// shelving values on Thursday"), and filtering one out would hide it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ack: Option<FieldAck>,
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
    /// A bounded sample of the values this cast nulled. Empty for the repin
    /// rewrite, which counts its nulls without materialising them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub samples: Vec<String>,
    /// When the conflict was recorded (ISO 8601 UTC).
    pub at: String,
}

/// An operator's acknowledgement of a degraded verdict (issue #111): the
/// body of a successful `POST /api/v1/schema/field/ack`, and the `ack` key
/// on the field detail.
///
/// Acknowledging suppresses the badge for the evidence that existed when it
/// was written, and nothing further: `evidence_through` is the episode count
/// the ack covers, so the next conflict episode raises the badge again.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldAck {
    /// When the acknowledgement was written or last advanced (ISO 8601 UTC).
    pub acked_at: String,
    /// The acknowledging key's stable prefix. Not its display name: this row
    /// outlives renames and rotations.
    pub acked_by: String,
    /// The operator's note, verbatim as they wrote it (≤1024 bytes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Conflict episodes the acknowledgement covers.
    pub evidence_through: u64,
}

// -- repin (ADR-0011) --------------------------------------------------------

/// Request body for `POST /api/v1/schema/repin`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepinRequest {
    /// The field to repin (folded to the catalog's ASCII-lowercase key).
    pub field: String,
    /// Target catalog type (`BIGINT`, `DOUBLE`, `TIMESTAMP`, `BOOLEAN`,
    /// `VARCHAR`, `SEVERITY`; case-insensitive).
    pub to: String,
    /// Which dialect the corpus's numerals are read in for a `SEVERITY`
    /// target: `otel` (default) or `syslog`. The two ladders overlap over
    /// 1-7 with opposite meanings, so no value-shape rule can tell them
    /// apart; the operator asserts provenance. A dialect with any other
    /// target is a 400, since it would be silently ignored otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dialect: Option<String>,
    /// Scan and report only — no mutation.
    #[serde(default)]
    pub dry_run: bool,
    /// Accept a lossy projection (`projected_nulls > 0`), or run a
    /// resurrection-only pass when `to` equals the current pin.
    #[serde(default)]
    pub force: bool,
    /// The most rows the forced rewrite may null before the cutover is
    /// refused. Absent means the server derives one from this job's own
    /// scan (10% headroom over a floor of 10 rows), which is the ordinary
    /// case: an operator forcing a repin accepts roughly the plan they read,
    /// not a number they computed. A ceiling without `force` is a 400.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_nulled_rows: Option<u64>,
    /// The same bound for dialect-ambiguous numerals, consulted only where
    /// ambiguity binds (a `SEVERITY` target that did not assert `syslog`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ambiguous_rows: Option<u64>,
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
    /// The asserted numeral dialect, present exactly for a `SEVERITY`
    /// target. Absent on every other target: a backfilled `otel` would
    /// report an assertion nobody made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dialect: Option<String>,
    /// Rows whose numeral reads as a different severity in each dialect
    /// (the 1-7 overlap): only provenance can settle them. Counted
    /// whatever the dialect; what the dialect governs is the force gate.
    #[serde(default)]
    pub ambiguous_numerals: u64,
    /// Up to five distinct sanitised samples of values the new pin cannot
    /// read at all, `_raw` resurrection included.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unmapped_samples: Vec<String>,
    /// Scan-time liveness, when something is still writing the field.
    /// Presence is the verdict; the consumer writes the words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub liveness: Option<RepinLiveness>,
    /// Whether an executing request with this job's flags would be refused
    /// for want of a force flag.
    ///
    /// A dry run succeeds by design, so a plan carrying loss or dialect
    /// ambiguity has to say so here, or the operator meets the refusal only
    /// on the request meant to do the work. The server computes it from this
    /// row's numbers through the same decision the two live gates ask, so a
    /// dry run cannot promise an outcome the execution would not reach.
    ///
    /// Absent until the scan has recorded its plan: a claimed job's counts
    /// are zeros meaning "not measured yet", and answering `false` there
    /// would promise a clean run for a job that is about to refuse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_force: Option<bool>,
    /// Why force is required, in the words the refusal itself uses. Present
    /// exactly when `requires_force` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_force_reason: Option<String>,
    /// When an operator asked for this job to stop (ISO 8601 UTC), if any.
    /// Written with `cancelled_by` and never overwritten, so it names the
    /// first asker. A `running` row carrying it is a cancel in flight: the
    /// job is walking to its next file boundary. A `failed` row carrying it
    /// is the crash state — the process died between the request and any
    /// boundary observing it, and recovery may not infer `cancelled` from a
    /// request nothing acted on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_requested_at: Option<String>,
    /// Display name of the key that asked. Same identity source as
    /// `requested_by`, so the row is coherent about who did what.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancelled_by: Option<String>,
    /// The loss ceiling the request stated, echoed back. Absent when the
    /// request stated none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_nulled_rows: Option<u64>,
    /// The ambiguity ceiling the request stated, echoed back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ambiguous_rows: Option<u64>,
    /// The loss ceiling the job is held to: the stated value when there was
    /// one, else the scan-derived default. Resolved once, at plan time, so
    /// it is absent before a plan is recorded and on unforced jobs. A
    /// planned forced job always records both accepted ceilings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_max_nulled_rows: Option<u64>,
    /// The ambiguity ceiling the job is held to, resolved the same way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_max_ambiguous_rows: Option<u64>,
}

/// Evidence that a repin's subject is still being written.
///
/// A repin translates history. A field a live sender still feeds keeps
/// arriving in the ingest-time reading, so a syslog-dialect rewrite leaves
/// a discontinuity at the cutover instant; the fix for the live half is
/// `[ingest] severity_from`, not another repin. Facts only, presence being
/// the verdict (the `Option<DegradedVerdict>` shape).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepinLiveness {
    /// Newest observation of the field (ISO 8601 UTC).
    pub last_seen: String,
    /// One service behind that observation.
    pub service: String,
}

/// Response body for `POST /api/v1/schema/repin`. The HTTP status carries
/// the verdict: 200 = dry-run report, 202 = rewrite started, 409 = lossy
/// without force (the job is terminal `refused_needs_force` and this body
/// is the plan the refusal is based on).
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

/// What `POST /api/v1/schema/repin/cancel` answered (#109).
///
/// The three variants are exclusive because the server decides them under
/// one lock: a job is either still stoppable, past the point where there is
/// anything left to unwind, or absent. The HTTP status carries the same
/// verdict (202 / 409 / 404), and a client that reads both must find them
/// agreeing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepinCancelOutcome {
    /// The request is accepted. The job stops at its next file boundary.
    Cancelling,
    /// The job latched its point of no return first: the corpus is being
    /// swapped and the job will complete. Not queued for later.
    PastPointOfNoReturn,
    /// No repin job is running on this node.
    NoJobRunning,
}

/// Response body for `POST /api/v1/schema/repin/cancel`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepinCancelResponse {
    /// The verdict, mirroring the HTTP status.
    pub outcome: RepinCancelOutcome,
    /// The verdict in words. The accepted one quotes the latency contract:
    /// what "cancelling" promises, and what it does not.
    pub detail: String,
    /// The job the verdict is about, when there is one. Absent for
    /// `no_job_running`, and absent when the store could not be read — a
    /// row this handler failed to fetch never changes the verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<RepinJobResponse>,
}

/// Request body for `POST /api/v1/schema/gc-pins`.
///
/// Reclaims pin slots held by fields nothing writes any more. A pin is a
/// scarce install-wide resource (`MAX_PINNED_FIELDS`), and a typo'd or
/// retired sender field otherwise holds its slot forever.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GcPinsRequest {
    /// Scan and report only — no mutation.
    #[serde(default)]
    pub dry_run: bool,
    /// How long a field must have gone unobserved to be dead. Defaults
    /// server-side to 30 days; `0` is accepted literally (the standing
    /// parquet footers are the second, independent proof). The server
    /// raises it to the retention window when that is longer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub older_than_secs: Option<u64>,
}

/// Response from `POST /api/v1/schema/gc-pins`, the same shape for a dry
/// run and a real one — `dry_run` and `deleted` are what tell them apart.
///
/// Every number the operator reads is the server's own: the effective
/// window is decided once, in `catalog::gc`, and reported here. No client
/// recomputes it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcPinsResponse {
    /// Whether this run stopped after the scan.
    pub dry_run: bool,
    /// The one instant the run is anchored to (RFC 3339 UTC): cutoff,
    /// audit events and this report all read it.
    pub decided_at: String,
    /// The requested window in seconds, after the server default applied.
    pub requested_older_than_secs: u64,
    /// The retention window in seconds, when a finite retention horizon
    /// exists. A pin cannot be called dead over a span shorter than the
    /// corpus trawl still keeps, and the field is absent when the global
    /// `max_age_days` or any `[retention.env.*]` entry is 0, since no
    /// finite age then bounds what the corpus still holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_floor_secs: Option<u64>,
    /// The window actually applied: the larger of the two above.
    pub effective_older_than_secs: u64,
    /// Pins that passed the observation axis, before the footer scan.
    pub pins_examined: u64,
    /// Parquet files whose schema the run read.
    pub files_scanned: u64,
    /// Would-delete on a dry run, deleted on a real one. Field-sorted.
    pub candidates: Vec<GcPinCandidate>,
    /// Rows actually deleted; always 0 on a dry run.
    pub deleted: u64,
}

/// One pin the run judged dead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcPinCandidate {
    /// The field name (catalog spelling: ASCII-lowercase).
    pub field: String,
    /// The pin being reclaimed (a catalog type spelling).
    #[serde(rename = "type")]
    pub data_type: String,
    /// Newest observation of the field (ISO 8601 UTC), absent when the
    /// pin was never observed at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<String>,
    /// How many services ever carried it.
    pub services: i64,
}

// -- history -----------------------------------------------------------------

/// Response after clearing the authenticated key's query history.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClearHistoryResponse {
    /// Number of history rows deleted by this request.
    pub deleted: u64,
}

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
    /// The report window this schedule covers, normalized: the literal
    /// `"since_last"` or a duration such as `"2h"`. Absent means query
    /// mode, where the saved DSL runs verbatim (ADR-0018 ruling 6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    /// Late-arrival allowance as a duration string, present exactly when
    /// `window` is. A windowed schedule with no lag reports `"0s"`, which
    /// is the value in force, not an absence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lag: Option<String>,
    /// The same allowance in seconds, for a client that does arithmetic on
    /// it rather than printing it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lag_secs: Option<u64>,
    /// The `since_last` watermark, the instant the next window starts
    /// from (RFC 3339, UTC, microseconds).
    ///
    /// It says nothing about whether a run has happened. A fresh tiling
    /// schedule is seeded at the origin of the coverage it owes,
    /// `next_fire_at - interval - lag`, before it has ever run; only after
    /// a success is it the end of the newest window a run covered. Read it
    /// as where coverage resumes, never as proof of a run. `total_runs`
    /// and `last_run` are what answer that.
    ///
    /// Absent for a fixed window and for query mode, neither of which
    /// claims coverage. A schedule that used to tile keeps its watermark
    /// stored across a mode change, so switching back to `since_last`
    /// resumes from it rather than from the new anchor. It is not reported
    /// while the schedule is in a mode that does not mean it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub covered_through: Option<String>,
    /// The planned next fire instant (RFC 3339, UTC, microseconds). Always
    /// present: every schedule has a fire cursor, windowed or not, and it
    /// is what an operator watches when manual runs are refused.
    pub next_fire_at: String,
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
    /// The report window this schedule should cover (ADR-0018 ruling 6).
    ///
    /// Two spellings. `"since_last"` tiles: each run covers
    /// `[the previous run's window end, this fire - lag)`, so consecutive
    /// runs cover consecutive intervals and a failed run's gap is healed by
    /// the next success. A duration such as `"2h"` is a fixed trailing
    /// span, re-measured from every fire and never healing anything.
    ///
    /// Absent is query mode: the saved DSL runs verbatim and the run row
    /// records no bounds. A window against a saved query that carries its
    /// own `last=`/`earliest=`/`latest=`, or reads `from saved`, is a 400
    /// naming both sides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    /// Late-arrival allowance, a duration such as `"5m"` (default `"0s"`).
    ///
    /// It shifts BOTH window bounds back, so it delays coverage rather than
    /// widening it: an event that landed after the boundary it belongs to
    /// is still inside the window that covers it. Only meaningful with a
    /// `window` — a lag without one is a 400, since query mode has no
    /// bounds to shift.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lag: Option<String>,
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
    /// Inclusive lower bound of the window this run covered (RFC 3339, UTC,
    /// microseconds). All four `window_*` fields are absent together for a
    /// run claimed in Query text mode. Changing the schedule mode later
    /// does not add a window to an existing run (ADR-0018 ruling 11).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_start: Option<String>,
    /// Exclusive upper bound of the covered window. Windows are half-open
    /// `[start, end)`, so tiled runs cannot double-count a boundary event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_end: Option<String>,
    /// Whether the window was clamped forward past an uncovered catch-up
    /// gap. `Some(false)` is the positive claim that the run covers
    /// everything it owed; absent means there was no window to owe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_truncated: Option<bool>,
    /// The mode the run was claimed under: `"since_last"` or `"fixed"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_kind: Option<String>,
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
/// When every event is valid, `rejected` and `errors` are omitted from the
/// JSON body, so a decoder has to default them.
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
                verdict: None,
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
                samples: vec!["n/a".into()],
                at: "2026-08-02T11:00:00Z".into(),
            }],
            verdict: Some(DegradedVerdict {
                since: "2026-08-01T10:00:00Z".into(),
                services: 1,
                episodes: 4,
                rows_shelved: 120,
                samples: vec!["n/a".into(), "pending".into()],
                suggested_to: "VARCHAR".into(),
            }),
            ack: Some(FieldAck {
                acked_at: "2026-08-03T09:00:00Z".into(),
                acked_by: "tkl_abc123".into(),
                note: Some("sender is being fixed".into()),
                evidence_through: 4,
            }),
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
        assert_eq!(rt.conflicts[0].samples, vec!["n/a".to_owned()]);
        let verdict = rt.verdict.expect("the verdict survives the wire");
        assert_eq!(verdict.rows_shelved, 120);
        assert_eq!(verdict.suggested_to, "VARCHAR");
        // Verdict and ack ride together: an acknowledgement overtaken by
        // new evidence is exactly this shape.
        let ack = rt.ack.expect("the acknowledgement survives the wire");
        assert_eq!(ack.acked_by, "tkl_abc123");
        assert_eq!(ack.evidence_through, 4);
        assert_eq!(ack.note.as_deref(), Some("sender is being fixed"));
    }

    /// An unacknowledged field carries no `ack` key at all, and an ack
    /// without a note carries no `note` key. Absence is the current wire
    /// representation for an unset acknowledgement or note.
    #[test]
    fn an_unacknowledged_field_carries_no_ack_key() {
        let resp = CatalogFieldResponse {
            name: "duration".into(),
            data_type: "BIGINT".into(),
            pinned_from: None,
            pinned_at: "2026-08-01T10:00:00Z".into(),
            services: Vec::new(),
            services_cursor: None,
            conflicts: Vec::new(),
            verdict: None,
            ack: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("\"ack\""), "{json}");

        let json = serde_json::to_string(&FieldAck {
            acked_at: "2026-08-03T09:00:00Z".into(),
            acked_by: "tkl_abc123".into(),
            note: None,
            evidence_through: 4,
        })
        .unwrap();
        assert!(!json.contains("\"note\""), "{json}");
    }

    /// A repin request states its ceilings or states nothing. A body from a
    /// client that predates them deserializes with both absent, and one
    /// written without them puts no key on the wire, so the server reads
    /// "derive from the scan" rather than a zero somebody meant as
    /// "unlimited".
    #[test]
    fn repin_ceilings_are_absent_unless_the_request_states_them() {
        let older: RepinRequest =
            serde_json::from_str(r#"{"field":"status","to":"VARCHAR","force":true}"#).unwrap();
        assert_eq!(older.max_nulled_rows, None);
        assert_eq!(older.max_ambiguous_rows, None);

        let json = serde_json::to_string(&older).unwrap();
        assert!(!json.contains("max_nulled_rows"), "{json}");
        assert!(!json.contains("max_ambiguous_rows"), "{json}");

        let stated = RepinRequest {
            max_nulled_rows: Some(500),
            ..older
        };
        let rt: RepinRequest = serde_json::from_str(&serde_json::to_string(&stated).unwrap())
            .expect("the stated ceiling survives the wire");
        assert_eq!(rt.max_nulled_rows, Some(500));
        assert_eq!(rt.max_ambiguous_rows, None);
    }

    /// The job row carries both pairs: what the request asked for, and what
    /// the job is held to. The accepted pair is what the CLI binds a second
    /// request to, so it has to survive the round trip intact.
    #[test]
    fn repin_job_reports_requested_and_accepted_ceilings() {
        let json = serde_json::to_string(&RepinJobResponse {
            id: 7,
            field: "status".into(),
            from_type: "BIGINT".into(),
            to_type: "VARCHAR".into(),
            dry_run: true,
            force: true,
            status: "succeeded".into(),
            requested_by: Some("ops".into()),
            started_at: "2026-09-01T10:00:00Z".into(),
            finished_at: Some("2026-09-01T10:00:04Z".into()),
            error: None,
            files_total: 2,
            rows_carrying: 400,
            projected_nulls: 12,
            resurrectable: 0,
            affected_bytes: 8192,
            files_done: 0,
            rows_rewritten: 0,
            rows_nulled: 0,
            rows_resurrected: 0,
            dialect: None,
            ambiguous_numerals: 0,
            unmapped_samples: Vec::new(),
            liveness: None,
            requires_force: Some(false),
            requires_force_reason: None,
            max_nulled_rows: None,
            max_ambiguous_rows: Some(3),
            accepted_max_nulled_rows: Some(22),
            accepted_max_ambiguous_rows: Some(3),
            cancel_requested_at: None,
            cancelled_by: None,
        })
        .unwrap();
        // An unstated request ceiling is absent, not zero.
        assert!(!json.contains("\"max_nulled_rows\""), "{json}");

        let rt: RepinJobResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.max_nulled_rows, None);
        assert_eq!(rt.max_ambiguous_rows, Some(3));
        assert_eq!(rt.accepted_max_nulled_rows, Some(22));
        assert_eq!(rt.accepted_max_ambiguous_rows, Some(3));
    }

    /// A healthy field carries no `verdict` key and no `samples` key at all:
    /// absent is the only encoding of "not degraded".
    #[test]
    fn a_healthy_field_serialises_without_the_new_keys() {
        let resp = CatalogFieldsResponse {
            fields: vec![CatalogFieldSummary {
                name: "duration".into(),
                data_type: "BIGINT".into(),
                pinned_from: None,
                pinned_at: "2026-08-01T10:00:00Z".into(),
                service_count: 1,
                row_count: 5,
                first_seen: None,
                last_seen: None,
                conflict_count: 0,
                rows_nulled: 0,
                verdict: None,
            }],
            pinned_total: 1,
            pin_capacity: 10_000,
            truncated: false,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("verdict"), "{json}");

        // A body without the key still deserialises.
        let old = r#"{"fields":[{"name":"duration","type":"BIGINT",
            "pinned_at":"2026-08-01T10:00:00Z","service_count":1,"row_count":5,
            "conflict_count":0,"rows_nulled":0}],
            "pinned_total":1,"pin_capacity":10000,"truncated":false}"#;
        let parsed: CatalogFieldsResponse = serde_json::from_str(old).unwrap();
        assert!(parsed.fields[0].verdict.is_none());
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
                samples: Vec::new(),
                at: "2026-08-02T11:00:00Z".into(),
            }],
            truncated: true,
        };
        let rt = roundtrip(&resp);
        assert_eq!(rt.conflicts.len(), 1);
        assert_eq!(rt.conflicts[0].rows_nulled, 3);
        assert!(rt.truncated);
    }

    /// The `/api/v1/schema/services` wire shape is the TUI and SPA contract:
    /// types come from the catalog, and the JSON keys are fixed. Golden-pin
    /// every key.
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
                // Empty is the healthy install; the pinned key set below is
                // what proves it stays off the wire there.
                degraded_fields: Vec::new(),
            }],
            cached: true,
            hot_buffer_events: Some(1),
            hot_buffer_bytes: Some(2),
        };
        let json: serde_json::Value = serde_json::to_value(&resp).unwrap();

        // serde_json::Value sorts object keys, so pin the sorted key sets:
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
            query: "_severity=error | stats count()".into(),
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
    fn successful_ingest_omits_zero_rejections() {
        // A successful batch omits its zero rejection count and empty errors.
        let json = r#"{"accepted": 5}"#;
        let resp: IngestResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.accepted, 5);
        assert_eq!(resp.rejected, 0);
        assert!(resp.errors.is_empty());
        assert_eq!(
            serde_json::to_value(&resp).unwrap(),
            serde_json::from_str::<serde_json::Value>(json).unwrap()
        );
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
        let checks = HashMap::from([
            ("duckdb".into(), "ok".into()),
            ("auth_db".into(), "error".into()),
            ("storage_db".into(), "ok".into()),
            ("data_path".into(), "ok".into()),
        ]);
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
        assert_eq!(rt.checks, resp.checks);
        assert_eq!(rt.version, resp.version);
    }

    #[test]
    fn health_response_without_checks_deserializes() {
        // A response without the `checks` field still parses.
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
        // Roles are cross-app bundles: every role name travels, unfiltered;
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
                degraded_fields: vec!["duration".into()],
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
        assert_eq!(rt.services[0].degraded_fields, vec!["duration".to_owned()]);
        assert!(rt.cached);
        assert_eq!(rt.hot_buffer_events, Some(42));
    }

    /// A body with no `degraded_fields` key is indistinguishable from a
    /// healthy one: absent on the way in reads as "nothing degraded", and
    /// empty on the way out never reaches the wire at all.
    #[test]
    fn service_schema_degraded_fields_defaults_and_omits() {
        let healthy = serde_json::json!({
            "name": "nginx",
            "columns": [],
            "file_count": 0,
            "total_bytes": 0,
            "total_events": 0,
        });
        let parsed: ServiceSchema = serde_json::from_value(healthy).unwrap();
        assert!(
            parsed.degraded_fields.is_empty(),
            "a body with no degraded_fields key is a healthy service, not an error"
        );

        let json = serde_json::to_value(&parsed).unwrap();
        assert!(
            json.as_object().unwrap().get("degraded_fields").is_none(),
            "an empty list is omitted entirely: {json}"
        );

        let badged = ServiceSchema {
            degraded_fields: vec!["duration".into(), "status".into()],
            ..parsed
        };
        let rt: ServiceSchema = serde_json::from_value(serde_json::to_value(&badged).unwrap())
            .expect("badged service round-trips");
        assert_eq!(
            rt.degraded_fields,
            vec!["duration".to_owned(), "status".to_owned()]
        );
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

    /// One fully populated snapshot, shared by the tests that need a
    /// complete wire shape to take a field away from.
    fn dashboard_fixture() -> DashboardSnapshot {
        DashboardSnapshot {
            hostname: "test-host".into(),
            listen_addr: "127.0.0.1:5514".into(),
            uptime_secs: 9240,
            version: "0.1.0".into(),
            healthy: true,
            pool_capacity: 4,
            pool_active: 1,
            pool_retained: 0,
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
                query: "_severity=error".into(),
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
        }
    }

    #[test]
    fn dashboard_snapshot_roundtrip() {
        let snapshot = dashboard_fixture();
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

    /// Retained work is reported explicitly. An omitted measurement must not
    /// silently turn into a claim that no executor remains busy.
    #[test]
    fn retained_counts_are_required_and_roundtrip() {
        let without_retained = |value: &serde_json::Value| {
            let mut json = value.clone();
            let removed = json
                .as_object_mut()
                .expect("a response object")
                .remove("pool_retained");
            assert!(removed.is_some(), "the field has to be there to remove");
            json
        };

        let stats = StatsResponse {
            uptime_secs: 60,
            total_queries: 3,
            active_queries: 1,
            pool_available: 3,
            pool_capacity: 4,
            pool_retained: 2,
        };
        let encoded = serde_json::to_value(&stats).unwrap();
        let decoded: StatsResponse = serde_json::from_value(encoded.clone()).unwrap();
        assert_eq!(decoded.pool_retained, 2);
        assert!(serde_json::from_value::<StatsResponse>(without_retained(&encoded)).is_err());

        let snapshot = DashboardSnapshot {
            pool_retained: 2,
            ..dashboard_fixture()
        };
        let encoded = serde_json::to_value(&snapshot).unwrap();
        let decoded: DashboardSnapshot = serde_json::from_value(encoded.clone()).unwrap();
        assert_eq!(decoded.pool_retained, 2);
        assert!(serde_json::from_value::<DashboardSnapshot>(without_retained(&encoded)).is_err());
    }
}
