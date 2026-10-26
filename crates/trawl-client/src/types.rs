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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<&'a str>,
}

#[derive(Serialize)]
pub(crate) struct ExportRequestRef<'a> {
    pub query: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// Body for `POST /api/v1/schema/field/ack`. The note is the operator's
/// own prose, so it travels borrowed and verbatim.
#[derive(Serialize)]
pub(crate) struct FieldAckRequestRef<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<&'a str>,
}

#[derive(Serialize)]
pub(crate) struct SetScheduleRequestRef<'a> {
    pub interval: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_runs: Option<u64>,
    pub enabled: bool,
    /// `"since_last"` or a duration; omitted for query mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<&'a str>,
    /// Late-arrival allowance; only meaningful beside a window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lag: Option<&'a str>,
}

// -- streaming types ---------------------------------------------------------

/// A single event from a live query stream.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// One event a pass-through stream emitted: a one-row result whose
    /// typed columns are the fields that event carries.
    Event(trawl_api::value::QueryResult),
    /// An aggregation snapshot replacing the entire result set.
    Snapshot {
        /// The aggregation's rows so far, typed.
        result: trawl_api::value::QueryResult,
        /// The caps that dropped input since the stream started, so the
        /// rows are partial. Empty when nothing was dropped (ADR-0001).
        capped: Vec<trawl_api::stream::SnapshotCap>,
    },
    /// Server-side error message.
    Error(String),
    /// Back-pressure notification: the subscriber fell behind and
    /// missed `n` event batches from the bus.
    Lagged(u64),
}

// -- re-exports from trawl-api -----------------------------------------------

pub use trawl_api::capabilities::{
    CapabilitiesResponse, CapabilityBounds, CaptureAdmission, CaptureQuotas, DslCapabilities,
};
pub use trawl_api::ingest_preview::{
    FieldChangeKind, FieldChangeWire, MAX_PREVIEW_EVENTS, PLACEHOLDER_PEER, PreviewDerivation,
    PreviewEvent, PreviewLineage, PreviewPeer, PreviewResponse, SeverityLineage,
    SeveritySourceSpec, TimeLineage,
};
pub use trawl_api::investigations::{
    Activity, Actor, AppendEntry, AppendResponse, Attach, CreateInvestigation, EntryKind,
    EntryPayload, EvidenceRef, FindingStatus, FindingStatusUpdate, Investigation, InvestigationId,
    InvestigationList, InvestigationRead, InvestigationStatus, InvestigationSummary, LogEntry,
    PatchInvestigation, RetentionExtension, SequenceConflict, StepBounds, StepOutcome,
    StepOutcomeKind,
};
pub use trawl_api::results::{
    COMPARE_DEFAULT_MIN_COUNT, COMPARE_MAX_GROUP_FIELDS, CONTEXT_DEFAULT_SECONDS,
    CONTEXT_DEFAULT_SIDE_ROWS, CONTEXT_MAX_SIDE_ROWS, CONTEXT_SAME_TIME_ROWS, CONTEXT_STATEMENT,
    CaptureKind, CaptureRequest, CaptureResponse, CaptureWindow, CapturedResult, ColumnFacts,
    CompareFacts, CompareRequest, CompareWarning, CompareWarningCode, CompareWindow, ContextFacts,
    ContextGroups, ContextRequest, ContextScope, Correlation, CorrelationResponse, FieldSightings,
    FocusRow, RateUnit, ResultCell, ResultColumn, ResultList, ResultPage, ResultShape,
    ResultSource, ResultState, RowRange, ScopeUsed, SightingGroup, SightingsDenominators,
    SightingsFacts, SightingsRequest, StoredAs, ValueCount,
};
pub use trawl_api::{
    ActiveQuerySnapshot, CancelResponse, CatalogConflictRow, CatalogConflictsResponse,
    CatalogFieldResponse, CatalogFieldServiceRow, CatalogFieldSummary, CatalogFieldsResponse,
    ClearHistoryResponse, CompletedQuerySnapshot, CreateSavedRequest, DashboardSnapshot,
    DegradedVerdict, DeleteSavedResponse, DeleteScheduleResponse, ErrorCode, ErrorDetail,
    ErrorEnvelope, ErrorResponse, ErrorSpan, ExportFormat, ExportRequest, FieldAck,
    FieldValuesResponse, GcPinCandidate, GcPinsRequest, GcPinsResponse, GlobalRunSummary,
    HealthResponse, HealthStatus, HistoryEntryResponse, HistoryResponse, IngestEventError,
    IngestResponse, ListAllRunsResponse, ListReportRunsResponse, ListSavedResponse, PaginationMeta,
    QueriesResponse, QueryActiveEntry, QueryRecentEntry, QueryRequest, QueryResponse, QueryStatus,
    RepinCancelOutcome, RepinCancelResponse, RepinJobResponse, RepinLiveness, RepinRequest,
    RepinResponse, RepinStatusResponse, ReportRunResponse, ReportRunSummary, RunsStatsResponse,
    SavedQueryResponse, ScheduleResponse, SchemaColumnResponse, SchemaResponse, ServiceColumnStats,
    ServiceSchema, ServiceSchemaResponse, SetScheduleRequest, StatsResponse, UpdateSavedRequest,
    ValidationResponse, WhoAmIResponse,
};
