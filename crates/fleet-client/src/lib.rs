//! fleet-client: shared client library for communicating with fleetd.
//!
//! Used by fleet-cli (CLI and TUI modes). Handles connection
//! management, authentication, query submission, and result streaming
//! across all supported transports.

pub mod client;
pub mod error;
pub mod types;

pub use client::HttpClient;
pub use error::ClientError;
pub use types::{
    ActiveQuerySnapshot, CancelResponse, CompletedQuerySnapshot, DeleteSavedResponse,
    FieldValuesResponse, HealthResponse, HistoryEntryResponse, HistoryResponse, IngestResponse,
    ListSavedResponse, PaginationMeta, QueriesResponse, QueryResponse, SavedQueryResponse,
    SchemaColumnResponse, SchemaResponse, StatsResponse, ValidationResponse,
};
