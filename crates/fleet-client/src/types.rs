//! Wire types for the fleet daemon HTTP API.
//!
//! Response types are re-exported from `fleet-api`. Internal request
//! types used only by the client are defined here.

use serde::{Deserialize, Serialize};

// -- internal types (client-only) --------------------------------------------

#[derive(Deserialize)]
pub(crate) struct ErrorResponse {
    pub error: String,
}

#[derive(Serialize)]
pub(crate) struct QueryRequestPaginated {
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,
}

#[derive(Serialize)]
pub(crate) struct ValidateRequest<'a> {
    pub query: &'a str,
}

#[derive(Serialize)]
pub(crate) struct CreateSavedRequest<'a> {
    pub name: &'a str,
    pub query: &'a str,
}

#[derive(Serialize)]
pub(crate) struct UpdateSavedRequest<'a> {
    pub query: &'a str,
}

#[derive(Serialize)]
pub(crate) struct ExportRequest<'a> {
    pub query: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

// -- streaming types ---------------------------------------------------------

/// A single event from a live query stream.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A result row.
    Row(Vec<fleet_engine::value::Value>),
    /// Server-side error message.
    Error(String),
}

// -- re-exports from fleet-api -----------------------------------------------

pub use fleet_api::{
    ActiveQuerySnapshot, CancelResponse, CompletedQuerySnapshot, DeleteSavedResponse,
    FieldValuesResponse, HealthResponse, HistoryEntryResponse, HistoryResponse, IngestResponse,
    ListSavedResponse, PaginationMeta, QueriesResponse, QueryResponse, SavedQueryResponse,
    SchemaColumnResponse, SchemaResponse, StatsResponse, ValidationResponse,
};
