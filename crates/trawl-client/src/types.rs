// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Wire types for the trawl daemon HTTP API.
//!
//! Response types and shared request/enum types are re-exported from
//! `trawl-api`. Internal request types used only by the client
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

#[derive(Serialize)]
pub(crate) struct SetScheduleRequestRef<'a> {
    pub interval: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_runs: Option<u64>,
    pub enabled: bool,
}

// -- streaming types ---------------------------------------------------------

/// A single event from a live query stream.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A log event as a named-field map (from SSE stream).
    Event(serde_json::Map<String, serde_json::Value>),
    /// A result row (columnar, for non-streaming use).
    Row(Vec<trawl_engine::value::Value>),
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

// -- re-exports from trawl-api -----------------------------------------------

pub use trawl_api::{
    ActiveQuerySnapshot, CancelResponse, CompletedQuerySnapshot, CreateSavedRequest,
    DashboardSnapshot, DeleteSavedResponse, DeleteScheduleResponse, ErrorCode, ErrorDetail,
    ErrorEnvelope, ErrorResponse, ErrorSpan, ExportFormat, ExportRequest, FieldValuesResponse,
    HealthResponse, HealthStatus, HistoryEntryResponse, HistoryResponse, IngestEventError,
    IngestResponse, ListReportRunsResponse, ListSavedResponse, PaginationMeta, QueriesResponse,
    QueryRequest, QueryResponse, QueryStatus, ReportRunResponse, ReportRunSummary,
    SavedQueryResponse, ScheduleResponse, SchemaColumnResponse, SchemaResponse, SetScheduleRequest,
    StatsResponse, UpdateSavedRequest, ValidationResponse, WhoAmIResponse,
};
