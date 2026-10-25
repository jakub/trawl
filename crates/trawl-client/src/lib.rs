// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! trawl-client: shared client library for communicating with trawld.
//!
//! Used by `trawl-cli` (CLI and TUI modes) and by the server's own
//! integration tests. Wraps the daemon's HTTP API: bearer authentication,
//! query submission, captured results, investigations, catalog reads,
//! ingest and its preview, export, and SSE streaming.

pub mod client;
pub mod error;
pub mod types;

pub use client::{
    HttpClient, OriginProbe, ProbeResponse, RepinCancel, RepinCeilings, RepinStart, TlsTrust,
};
pub use error::{ClientError, NetworkError, NetworkKind};
pub use types::{
    ActiveQuerySnapshot, Activity, Actor, AppendEntry, AppendResponse, Attach, CancelResponse,
    CaptureRequest, CaptureResponse, CaptureWindow, CapturedResult, CatalogConflictRow,
    CatalogConflictsResponse, CatalogFieldResponse, CatalogFieldServiceRow, CatalogFieldSummary,
    CatalogFieldsResponse, ClearHistoryResponse, ColumnFacts, CompletedQuerySnapshot,
    CreateInvestigation, CreateSavedRequest, DashboardSnapshot, DegradedVerdict,
    DeleteSavedResponse, EntryKind, EntryPayload, ErrorCode, ErrorDetail, ErrorEnvelope,
    ErrorResponse, ErrorSpan, EvidenceRef, ExportFormat, ExportRequest, FieldAck, FieldChangeKind,
    FieldChangeWire, FieldValuesResponse, FindingStatus, FindingStatusUpdate, GcPinCandidate,
    GcPinsResponse, HealthResponse, HealthStatus, HistoryEntryResponse, HistoryResponse,
    IngestEventError, IngestResponse, Investigation, InvestigationId, InvestigationList,
    InvestigationRead, InvestigationStatus, InvestigationSummary, ListReportRunsResponse,
    ListSavedResponse, LogEntry, MAX_PREVIEW_EVENTS, PLACEHOLDER_PEER, PaginationMeta,
    PatchInvestigation, PreviewDerivation, PreviewEvent, PreviewLineage, PreviewPeer,
    PreviewResponse, QueriesResponse, QueryActiveEntry, QueryRecentEntry, QueryRequest,
    QueryResponse, QueryStatus, RepinCancelOutcome, RepinCancelResponse, RepinJobResponse,
    RepinLiveness, RepinRequest, RepinResponse, RepinStatusResponse, ReportRunResponse,
    ReportRunSummary, ResultCell, ResultColumn, ResultList, ResultPage, ResultShape, ResultSource,
    ResultState, RetentionExtension, SavedQueryResponse, SchemaColumnResponse, SchemaResponse,
    SequenceConflict, SeverityLineage, SeveritySourceSpec, StatsResponse, StepBounds, StepOutcome,
    StepOutcomeKind, StoredAs, StreamEvent, TimeLineage, UpdateSavedRequest, ValidationResponse,
    ValueCount, WhoAmIResponse,
};
