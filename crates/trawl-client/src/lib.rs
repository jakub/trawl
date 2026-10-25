// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! trawl-client: shared client library for communicating with trawld.
//!
//! Used by `trawl-cli` (CLI and TUI modes) and by the server's own
//! integration tests. Wraps the daemon's HTTP API: bearer authentication,
//! query submission, captured results, catalog reads, ingest and its
//! preview, export, and SSE streaming.

pub mod client;
pub mod error;
pub mod types;

pub use client::{
    HttpClient, OriginProbe, ProbeResponse, RepinCancel, RepinCeilings, RepinStart, TlsTrust,
};
pub use error::{ClientError, NetworkError, NetworkKind};
pub use types::{
    ActiveQuerySnapshot, COMPARE_DEFAULT_MIN_COUNT, COMPARE_MAX_GROUP_FIELDS,
    CONTEXT_DEFAULT_SECONDS, CONTEXT_DEFAULT_SIDE_ROWS, CONTEXT_MAX_SIDE_ROWS,
    CONTEXT_SAME_TIME_ROWS, CONTEXT_STATEMENT, CancelResponse, CaptureKind, CaptureRequest,
    CaptureResponse, CaptureWindow, CapturedResult, CatalogConflictRow, CatalogConflictsResponse,
    CatalogFieldResponse, CatalogFieldServiceRow, CatalogFieldSummary, CatalogFieldsResponse,
    ClearHistoryResponse, ColumnFacts, CompareFacts, CompareRequest, CompareWarning,
    CompareWarningCode, CompareWindow, CompletedQuerySnapshot, ContextFacts, ContextGroups,
    ContextRequest, ContextScope, Correlation, CorrelationResponse, CreateSavedRequest,
    DashboardSnapshot, DegradedVerdict, DeleteSavedResponse, ErrorCode, ErrorDetail, ErrorEnvelope,
    ErrorResponse, ErrorSpan, ExportFormat, ExportRequest, FieldAck, FieldChangeKind,
    FieldChangeWire, FieldSightings, FieldValuesResponse, FocusRow, GcPinCandidate, GcPinsResponse,
    HealthResponse, HealthStatus, HistoryEntryResponse, HistoryResponse, IngestEventError,
    IngestResponse, ListReportRunsResponse, ListSavedResponse, MAX_PREVIEW_EVENTS,
    PLACEHOLDER_PEER, PaginationMeta, PreviewDerivation, PreviewEvent, PreviewLineage, PreviewPeer,
    PreviewResponse, QueriesResponse, QueryActiveEntry, QueryRecentEntry, QueryRequest,
    QueryResponse, QueryStatus, RateUnit, RepinCancelOutcome, RepinCancelResponse,
    RepinJobResponse, RepinLiveness, RepinRequest, RepinResponse, RepinStatusResponse,
    ReportRunResponse, ReportRunSummary, ResultCell, ResultColumn, ResultList, ResultPage,
    ResultShape, ResultSource, ResultState, RowRange, SavedQueryResponse, SchemaColumnResponse,
    SchemaResponse, ScopeUsed, SeverityLineage, SeveritySourceSpec, SightingGroup,
    SightingsDenominators, SightingsFacts, SightingsRequest, StatsResponse, StoredAs, StreamEvent,
    TimeLineage, UpdateSavedRequest, ValidationResponse, ValueCount, WhoAmIResponse,
};
