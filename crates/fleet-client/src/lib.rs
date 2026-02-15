//! fleet-client: shared client library for communicating with fleetd.
//!
//! Used by fleet-cli (CLI and TUI modes). Handles connection
//! management, authentication, query submission, and result streaming
//! across all supported transports.

pub mod connection;
pub mod error;

pub use connection::{
    ActiveQuerySnapshot, CancelResponse, CompletedQuerySnapshot, DeleteSavedResponse,
    HealthResponse, HistoryEntryResponse, HistoryResponse, HttpClient, ListSavedResponse,
    PaginationMeta, QueriesResponse, QueryResponse, SavedQueryResponse, SchemaColumnResponse,
    SchemaResponse, ValidationResponse,
};
pub use error::ClientError;
