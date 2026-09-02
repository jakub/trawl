// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! trawl-client: shared client library for communicating with trawld.
//!
//! Used by `trawl-cli` (CLI and TUI modes) and by the server's own
//! integration tests. Wraps the daemon's HTTP API: bearer authentication,
//! query submission, catalog reads, ingest, export, and SSE streaming.

pub mod client;
pub mod error;
pub mod types;

pub use client::{HttpClient, RepinStart};
pub use error::ClientError;
pub use types::{
    ActiveQuerySnapshot, CancelResponse, CatalogConflictRow, CatalogConflictsResponse,
    CatalogFieldResponse, CatalogFieldServiceRow, CatalogFieldSummary, CatalogFieldsResponse,
    CompletedQuerySnapshot, CreateSavedRequest, DashboardSnapshot, DegradedVerdict,
    DeleteSavedResponse, ErrorCode, ErrorDetail, ErrorEnvelope, ErrorResponse, ErrorSpan,
    ExportFormat, ExportRequest, FieldValuesResponse, HealthResponse, HealthStatus,
    HistoryEntryResponse, HistoryResponse, IngestEventError, IngestResponse,
    ListReportRunsResponse, ListSavedResponse, PaginationMeta, QueriesResponse, QueryRequest,
    QueryResponse, QueryStatus, RepinJobResponse, RepinLiveness, RepinRequest, RepinResponse,
    RepinStatusResponse, ReportRunResponse, ReportRunSummary, SavedQueryResponse,
    SchemaColumnResponse, SchemaResponse, StatsResponse, StreamEvent, UpdateSavedRequest,
    ValidationResponse, WhoAmIResponse,
};
