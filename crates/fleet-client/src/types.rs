//! Wire types for the fleet daemon HTTP API.
//!
//! Response types and shared request/enum types are re-exported from
//! `fleet-api`. Internal request types used only by the client
//! (borrowing for zero-copy serialization) are defined here.

use serde::Serialize;

// -- internal types (client-only, borrow for zero-copy serialization) --------

#[derive(Serialize)]
pub(crate) struct ValidateRequest<'a> {
    pub query: &'a str,
}

#[derive(Serialize)]
pub(crate) struct CreateSavedRequestRef<'a> {
    pub name: &'a str,
    pub query: &'a str,
}

#[derive(Serialize)]
pub(crate) struct UpdateSavedRequestRef<'a> {
    pub query: &'a str,
}

#[derive(Serialize)]
pub(crate) struct ExportRequestRef<'a> {
    pub query: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

// -- streaming types ---------------------------------------------------------

/// A single event from a live query stream.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A log event as a named-field map (from SSE stream).
    Event(serde_json::Map<String, serde_json::Value>),
    /// A result row (columnar, for non-streaming use).
    Row(Vec<fleet_engine::value::Value>),
    /// An aggregation snapshot replacing the entire result set.
    Snapshot {
        /// Column names for the snapshot rows.
        columns: Vec<String>,
        /// Each row is a field map of column→value.
        rows: Vec<serde_json::Map<String, serde_json::Value>>,
    },
    /// Server-side error message.
    Error(String),
    /// Back-pressure notification: the subscriber fell behind and
    /// missed `n` event batches from the bus.
    Lagged(u64),
}

// -- re-exports from fleet-api -----------------------------------------------

pub use fleet_api::{
    ActiveQuerySnapshot, CancelResponse, CompletedQuerySnapshot, CreateSavedRequest,
    DeleteSavedResponse, ErrorCode, ErrorDetail, ErrorEnvelope, ErrorResponse, ErrorSpan,
    ExportFormat, ExportRequest, FieldValuesResponse, HealthResponse, HealthStatus,
    HistoryEntryResponse, HistoryResponse, IngestEventError, IngestResponse, ListSavedResponse,
    PaginationMeta, QueriesResponse, QueryRequest, QueryResponse, QueryStatus, SavedQueryResponse,
    SchemaColumnResponse, SchemaResponse, StatsResponse, UpdateSavedRequest, ValidationResponse,
};
