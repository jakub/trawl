//! trawl-client: shared client library for communicating with trawld.
//!
//! Used by trawl-cli (CLI and TUI modes). Handles connection
//! management, authentication, query submission, and result streaming
//! across all supported transports.

pub mod client;
pub mod error;
pub mod types;

pub use client::HttpClient;
pub use error::ClientError;
pub use types::{
    ActiveQuerySnapshot, CancelResponse, CompletedQuerySnapshot, CreateSavedRequest,
    DashboardSnapshot, DeleteSavedResponse, ErrorCode, ErrorDetail, ErrorEnvelope, ErrorResponse,
    ErrorSpan, ExportFormat, ExportRequest, FieldValuesResponse, HealthResponse, HealthStatus,
    HistoryEntryResponse, HistoryResponse, IngestEventError, IngestResponse, ListSavedResponse,
    PaginationMeta, QueriesResponse, QueryRequest, QueryResponse, QueryStatus, SavedQueryResponse,
    SchemaColumnResponse, SchemaResponse, StatsResponse, StreamEvent, UpdateSavedRequest,
    ValidationResponse, WhoAmIResponse,
};
