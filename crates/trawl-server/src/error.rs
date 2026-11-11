// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Unified server error type with HTTP status mapping.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use trawl_engine::error::EngineError;

use crate::hot_buffer::Charge;
use crate::publication::CorpusUnsettled;
use crate::report_window::{MaterializeError, PlanError, WindowPolicyError};
use crate::results::bounds::BoundsError;
use crate::results::charge::EvidenceScope;
use crate::results::service::{CaptureOverflow, GoneReason};
use crate::store::StoreError;
use crate::transport::request_limit::Allowance;

/// The message of [`ServerError::CapacityNotStarted`], the one reason a
/// unit of pool work refused before it ever started (ADR-0024): the
/// request's budget was gone, or its cancellation was latched, while the
/// work was still queued behind capacity.
///
/// Fixed text, no interpolation: it is a capacity fact about the server,
/// not a fact about the query, and it travels into the query tracker,
/// the scheduler's run rows and the client response unchanged. A refusal
/// here is 503, never the 504 an execution timeout earns — nothing ran.
pub const CAPACITY_NOT_STARTED: &str = "server at capacity: the query was not started";

/// The whole answer to a query that exhausted the query database's spill
/// cap or memory limit (ADR-0056), or its regex memory (ADR-0063). Fixed
/// text: the server is healthy, and narrowing or simplifying the query is
/// what helps. It names no path, size, pattern or `DuckDB` text, so the
/// wire, the query tracker and a run row can all carry it unredacted.
pub const QUERY_RESOURCE_EXHAUSTED: &str = "query could not complete with the available query \
     resources; narrow the time range or simplify the query";

/// The whole answer to a query whose Rust tail reached a pattern it had
/// not prepared ([`EngineError::MissingPreparedPattern`], ADR-0063). Fixed
/// text that names no pattern: the fault is the server's, not the query's.
pub const MISSING_PREPARED_PATTERN: &str =
    "internal error: the server did not prepare a pattern this query reads";

/// The whole answer to a read refused while the corpus is not settled
/// (ADR-0041). Fixed per reason: no counts, paths or marker names, so the
/// wire, the query tracker and a run row can all carry it unredacted.
const fn corpus_recovering_message(reason: CorpusUnsettled) -> &'static str {
    match reason {
        CorpusUnsettled::RestartBacklog => {
            "Search is unavailable while the server finishes loading data from before its restart."
        }
        CorpusUnsettled::RollupPending => {
            "Search is unavailable while the server finishes an interrupted storage rollup."
        }
    }
}

/// The refusal when `[server] max_concurrent_requests` requests are already
/// in progress (ADR-0054). It names the setting and never its value: the
/// refusal reaches callers who have not authenticated, and the value would
/// tell one how many slow uploads fill the server.
pub const REQUEST_LIMIT_REACHED: &str = "trawld is at its HTTP request limit \
     ([server] max_concurrent_requests); the request was not processed; retry later with backoff";

/// The refusal when the probe allowance is in use (ADR-0054). It names the
/// allowance, not `max_concurrent_requests`, which health and `/metrics`
/// never use.
pub const PROBE_ALLOWANCE_REACHED: &str = "trawld is at its HTTP probe allowance; \
     the request was not processed; retry later with backoff";

/// The refusal when the control allowance is in use (ADR-0054). It names the
/// allowance, not `max_concurrent_requests`, which the query list and
/// cancellation never use.
pub const CONTROL_ALLOWANCE_REACHED: &str = "trawld is at its HTTP control allowance; \
     the request was not processed; retry later with backoff";

/// The refusal when every query preparation slot stayed in use until the
/// request's deadline (ADR-0063). It names the allowance and never its size,
/// as the request-limit refusals do (ADR-0054).
pub const PREPARATION_ALLOWANCE_EXHAUSTED: &str = "trawld is at its query preparation allowance; \
     the query was not started; retry later with backoff";

/// The fixed sentence for a refusal charged to `allowance`.
const fn request_limit_message(allowance: Allowance) -> &'static str {
    match allowance {
        Allowance::Regular => REQUEST_LIMIT_REACHED,
        Allowance::Probe => PROBE_ALLOWANCE_REACHED,
        Allowance::Control => CONTROL_ALLOWANCE_REACHED,
    }
}

/// The 413 sentence: what the request charged, if it was parsed, against
/// what one request may charge. Both are counts the sender can act on
/// (split the batch), and neither quotes the request's content.
fn ingest_batch_too_large_message(requested: Option<Charge>, ceiling: Charge) -> String {
    let limit = format!(
        "{} events or {} serialized bytes per request",
        ceiling.events, ceiling.bytes
    );
    match requested {
        Some(requested) => format!(
            "ingest batch too large: {} events and {} serialized bytes exceed the limit of \
             {limit}; split the batch",
            requested.events, requested.bytes
        ),
        None => format!("ingest batch too large: the hot buffer admits at most {limit}"),
    }
}

/// Server errors, mapped to HTTP responses via [`IntoResponse`].
///
/// Each error has one public message ([`ServerError::safe_message`]),
/// which the wire and every record of the failure carry. The free-text
/// variants (`Unauthorized`, `Forbidden`, `BadRequest`, `NotFound`,
/// `Conflict`, `Ingest` and `ServiceUnavailable`) disclose their text as
/// written, so their constructors never interpolate an error's `Display`
/// or an absolute host path (ADR-0040, amended 2026-11-08).
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// Query engine failure (parse, emit, or `DuckDB` execution).
    #[error("{0}")]
    Engine(#[from] EngineError),

    /// App-state store failure (history, saved queries, schedules, runs).
    /// Mapped per the ADR-0004 table: unavailability → 503 (redacted),
    /// conflict → 409, not-found → 404, validation → 400. The store's two
    /// investigation conflicts become their own variants on conversion
    /// (`From<StoreError>`), so they carry their own codes.
    #[error("{0}")]
    Store(StoreError),

    /// Missing or malformed Authorization header.
    #[error("unauthorized: {0}")]
    Unauthorized(String),

    /// Authenticated, but the key resolves no usable trawl permission.
    /// Produced by the mandatory policy middleware.
    #[error("forbidden: {0}")]
    Forbidden(String),

    /// Bad request (invalid input, duplicate name, etc.).
    #[error("bad request: {0}")]
    BadRequest(String),

    /// Resource not found (or unauthorized access).
    #[error("not found: {0}")]
    NotFound(String),

    /// The request cannot run against the state the server is in right
    /// now, and would be fine once that changes (409). Pin gc raises it
    /// when a repin owns the data root, when the corpus cannot be read
    /// well enough to prove a pin dead, and when the purge's outcome is
    /// unknown. The message is the operator's instruction, so unlike a
    /// store error it reaches the wire and the records intact.
    #[error("conflict: {0}")]
    Conflict(String),

    /// A schedule window and a saved query's own text cannot both say
    /// what a report covers (ADR-0018 rulings 7 and 12). A 400 whose
    /// message names both sides: the operator picks which one to drop,
    /// and the server has no basis for choosing.
    #[error("{0}")]
    WindowPolicy(#[from] WindowPolicyError),

    /// A due schedule could not be planned into a window. The numbers
    /// come from a schedule row and the daemon's own config, never from
    /// the request, so this is broken state and not bad input: 500, with
    /// the detail logged rather than returned.
    #[error("{0}")]
    WindowPlan(#[from] PlanError),

    /// A planned window could not be put onto the saved query. Write-time
    /// policy exists to make this unreachable, so reaching it means a
    /// stored schedule and its saved DSL disagree: 500, same treatment.
    #[error("{0}")]
    WindowMaterialize(#[from] MaterializeError),

    /// Query execution exceeded the configured timeout.
    #[error("query timed out")]
    Timeout,

    /// A live stream's patterns need more regex memory than one query may
    /// hold, or than the server's regex reservation can hold while other
    /// work holds it (ADR-0063). Raised by stream setup. The batch tail's
    /// refusal is [`EngineError::RegexResources`], and both answer the 503
    /// [`QUERY_RESOURCE_EXHAUSTED`].
    #[error("{}", trawl_core::pattern::PatternError::Resources(*.0))]
    RegexResources(trawl_core::pattern::RegexRefusal),

    /// No query preparation slot came free before the request's deadline
    /// (ADR-0063, amending ADR-0054): nothing of the query ran. 503
    /// `query_resource_exhausted` with [`PREPARATION_ALLOWANCE_EXHAUSTED`]
    /// and no `Retry-After`. A door that keeps a timing account ends it as a
    /// capacity refusal.
    #[error("{}", PREPARATION_ALLOWANCE_EXHAUSTED)]
    PreparationUnavailable,

    /// Ingest validation error (bad ndjson, missing fields).
    #[error("ingest error: {0}")]
    Ingest(String),

    /// A gzip ingest body that decompresses past `[ingest] max_body_bytes`
    /// (413, code `ingest_error`). Decompression stopped one byte past the
    /// limit, so the message gives the limit and no decompressed size. A
    /// retry of the same body cannot succeed; the sender must send smaller
    /// batches.
    #[error(
        "gzip body decompresses past the {limit} byte limit \
         ([ingest] max_body_bytes); send smaller batches"
    )]
    IngestBodyTooLarge {
        /// `[ingest] max_body_bytes`.
        limit: usize,
    },

    /// A request body larger than the route's configured body limit
    /// (413, code `request_too_large`). The handler answers it after its
    /// authority check, from the failed read of its body
    /// ([`crate::transport::body_limit`]); the message names the setting
    /// and the exact value it enforced, nothing from the request.
    #[error("request body exceeds {setting} ({limit} bytes)")]
    RequestTooLarge {
        /// The setting's name as an operator writes it in the config.
        setting: &'static str,
        /// The limit enforced, in bytes.
        limit: usize,
    },

    /// Rate limit exceeded (429). `retry_after_secs` is the wait the limiter
    /// computed, rounded up to whole seconds and at least 1; it becomes the
    /// `Retry-After` header.
    #[error("rate limit exceeded")]
    RateLimited { retry_after_secs: u64 },

    /// Too many concurrent SSE streams (429).
    #[error("too many concurrent streams")]
    TooManyStreams,

    /// Service temporarily unavailable (startup, data not ready).
    #[error("service unavailable: {0}")]
    ServiceUnavailable(String),

    /// Work refused before it started, for want of capacity (ADR-0024):
    /// the request's deadline passed, or its cancellation was latched,
    /// before any of its work ran. 503 `service_unavailable` with
    /// [`CAPACITY_NOT_STARTED`] and no `Retry-After`. Its timing outcome is
    /// a capacity refusal: the pool books it where it refuses, and
    /// [`crate::query_timing::Outcome::of_error`] reads it back for a cancel
    /// that ended a route's own wait.
    #[error("service unavailable: {}", CAPACITY_NOT_STARTED)]
    CapacityNotStarted,

    /// The auth backend could not answer a keystore call (503). The
    /// driver's typed kind rides along for [`ServerError::cause_kind`]; the
    /// postgres detail itself is logged at DEBUG where the error is
    /// converted, and never reaches the wire or a default-level event.
    #[error("service unavailable: auth backend unavailable")]
    AuthBackend(CauseKind),

    /// The hot buffer has no free space for an ingest request (503 with
    /// `Retry-After`, ADR-0043). Nothing from the request was written, so
    /// the sender can retry it whole once compaction drains.
    #[error("hot buffer full")]
    HotBufferFull {
        /// The `Retry-After` value: the compaction interval.
        retry_after_secs: u64,
    },

    /// An ingest request larger than the hot buffer admits for any one
    /// request (413, ADR-0043). It can never fit, so a retry of the same
    /// request cannot succeed; the sender must split it.
    #[error("{}", ingest_batch_too_large_message(*.requested, *.ceiling))]
    IngestBatchTooLarge {
        /// The request's parsed charge, or `None` when it was refused
        /// before parsing because the ceiling admits no request at all.
        requested: Option<Charge>,
        /// The most one request may charge.
        ceiling: Charge,
    },

    /// An ingest preview sample holds more positions than one preview
    /// reads (413, code `preview_too_large`, ADR-0049). The sample is
    /// refused whole, never truncated. The message gives the limit and
    /// nothing from the sample.
    #[error("the sample holds more than {limit} events; preview at most {limit} at a time")]
    PreviewTooLarge {
        /// `trawl_api::ingest_preview::MAX_PREVIEW_EVENTS`.
        limit: usize,
    },

    /// A capture whose window selects more stored parquet than
    /// `[capture] source_max_bytes` (400, code `capture_source_too_large`,
    /// ADR-0057). It is measured from the protected selection before the
    /// engine runs, so nothing was scanned. The message gives both counts
    /// and a fixed remedy chosen by `kind`, and nothing else: no path, no
    /// partition, no query text, no service name. The `a` and `b` in the
    /// window remedy are fixed example text.
    #[error(
        "the window selects {selected_bytes} bytes of stored data, above the capture limit \
         of {max_bytes} bytes; {kind}"
    )]
    CaptureSourceTooLarge {
        /// The bytes the window's selection measured.
        selected_bytes: u64,
        /// `[capture] source_max_bytes`.
        max_bytes: u64,
        /// What the measured bytes came from, which picks the remedy.
        kind: CaptureSourceKind,
    },

    /// A capture's bounds could not be resolved (400, code `bad_request`,
    /// ADR-0057): a window and a time clause together, a span over the
    /// limit, `latest=` alone, a value that names no instant. Refused
    /// before anything is taken. The message names the fix and quotes only
    /// the caller's own values.
    #[error("{0}")]
    CaptureBounds(BoundsError),

    /// A capture that cannot be kept whole (400, code `result_too_large`,
    /// ADR-0057): a whole-shaped result over the capture's row or wire-size
    /// cap, whose prefix would answer a different question, or a file over
    /// `[capture] max_file_bytes`. Fixed text, the counts and the hint.
    #[error("{0}")]
    CaptureTooLarge(CaptureOverflow),

    /// The key's capture queue is full, the installation runs as many
    /// captures as it may, or the deadline passed while the capture waited
    /// (429 with `Retry-After`, code `capture_busy`, ADR-0057). Refused
    /// before any executor permit.
    #[error("too many captures are running; retry after the Retry-After interval")]
    CaptureBusy {
        /// Whole seconds until a slot should free, at least 1.
        retry_after_secs: u64,
    },

    /// The key's or the installation's capture quota cannot take one more
    /// result (429 with `Retry-After`, code `capture_quota_exceeded`,
    /// ADR-0057). Refused before any executor permit.
    #[error(
        "the capture quota is full; retry after the Retry-After interval, when a result expires"
    )]
    CaptureQuotaExceeded {
        /// Whole seconds until the first temporary result in the full scope
        /// expires, at least 1.
        retry_after_secs: u64,
    },

    /// A captured result past its deadline, pruned by quota, or evidence
    /// of a deleted investigation (410, code `result_gone`, ADR-0057), for
    /// as long as its tombstone lasts, or evidence whose investigation's
    /// retention ended (ADR-0059), for as long as the investigation exists.
    /// Fixed text per reason.
    #[error("{}", .reason.message())]
    ResultGone {
        /// Why the result is gone.
        reason: GoneReason,
    },

    /// A captured result whose row is committed and whose file is missing
    /// (409, code `result_unavailable`, ADR-0057). Never an empty page.
    #[error("the captured result's file is missing; its rows cannot be read")]
    ResultUnavailable,

    /// A captured result whose file does not match its recorded blake3
    /// digest (500, code `result_corrupt`, ADR-0057). Fixed text: no path,
    /// no digest.
    #[error("the captured result's file does not match its recorded digest")]
    ResultCorrupt,

    /// A `context` focus row lacks a value the read needs: `_time`, or an
    /// `env` or `host` its scope constrains (400, code `focus_incomplete`,
    /// ADR-0058). The message names the field; the read is never widened
    /// to make up for it.
    #[error("{0}")]
    FocusIncomplete(String),

    /// A `context` focus row holds two columns of a name the read needs, so
    /// which value is the focus cannot be decided (400, code
    /// `focus_ambiguous`, ADR-0058). The message names the column.
    #[error("{0}")]
    FocusAmbiguous(String),

    /// A `compare` foreground and baseline window overlap (400, code
    /// `windows_overlap`, ADR-0058). Windows that touch are accepted.
    #[error("{0}")]
    WindowsOverlap(String),

    /// A correlation read's input cannot be composed into a query (400,
    /// code `correlation_invalid`, ADR-0058): a filter with pipe stages or
    /// time clauses, a refused group field, an empty or oversized literal.
    /// The message names the input and quotes only the caller's own
    /// values, never the composed text or its SQL.
    #[error("{0}")]
    CorrelationInvalid(String),

    /// An overwrite of an investigation carried an `expected_seq` that is
    /// no longer its sequence (409, code `sequence_conflict`, ADR-0059).
    /// The envelope's `conflict` member carries both numbers, and the
    /// client re-reads and retries. Appends never raise it.
    #[error(
        "the investigation changed: the request expected sequence {expected} and it is now \
         {current}; read it again and retry"
    )]
    SequenceConflict {
        /// The `expected_seq` the request carried.
        expected: u64,
        /// The investigation's sequence now.
        current: u64,
    },

    /// A create-if-absent named an investigation id that exists with
    /// another creator or other fields (409, code `investigation_exists`,
    /// ADR-0059). Fixed text: nothing about the existing investigation.
    #[error(
        "an investigation with this id already exists with a different creator or different \
         fields; create it under a new id"
    )]
    InvestigationExists,

    /// The investigation is closed and accepts nothing new until a writer
    /// reopens it (409, code `investigation_closed`, ADR-0059).
    #[error("the investigation is closed; reopen it before adding work, keeps, findings or notes")]
    InvestigationClosed,

    /// Keeping a result would take an investigation's or the installation's
    /// evidence past its `[investigation]` budget (409, code
    /// `evidence_budget_exceeded`, ADR-0059). Refused before any executor
    /// permit. Not a 429: evidence frees only as it ages out or an operator
    /// deletes an investigation, so a retry soon after would fail again.
    /// The message names the budget and gives both counts, and nothing
    /// else.
    #[error(
        "keeping this result would exceed {budget}: {used} of {limit} bytes are counted \
         against it, kept evidence and reservations together; evidence frees as it ages out, \
         or when an operator with server_manage deletes an investigation",
        budget = .scope.budget()
    )]
    EvidenceBudgetExceeded {
        /// The budget that refused.
        scope: EvidenceScope,
        /// Evidence bytes counted against that budget: kept evidence and
        /// the reservations not yet kept, together.
        used: u64,
        /// That budget, in bytes.
        limit: u64,
    },

    /// The request body carries a `Content-Encoding` the route does not
    /// decode (415, code `unsupported_encoding`). The ingest preview reads
    /// uncompressed samples only (ADR-0049). The message never quotes the
    /// header's value.
    #[error("compressed bodies are not accepted here; send the sample uncompressed")]
    UnsupportedEncoding,

    /// A corpus read refused because the corpus is not settled: WAL from
    /// before a restart is not yet proven covered, or a rollup is
    /// unresolved (503, no `Retry-After`, ADR-0041). The message is fixed
    /// per reason.
    #[error("{}", corpus_recovering_message(*.0))]
    CorpusRecovering(CorpusUnsettled),

    /// The hot buffer's snapshot could not be built, so a read refuses
    /// rather than answer without the newest events (503, ADR-0041). The
    /// I/O error's kind rides along for [`ServerError::cause_kind`]; its
    /// text can quote event values or paths and reaches neither the
    /// message nor the wire.
    #[error("hot buffer snapshot failed")]
    HotSnapshot(#[source] std::io::Error),

    /// The data root's env directories could not be listed, so a compare
    /// refuses rather than report retention reach over envs it could not
    /// see (503). Like [`Self::HotSnapshot`], the I/O error's kind rides
    /// along for [`ServerError::cause_kind`]; its text can quote paths and
    /// reaches neither the message nor the wire.
    #[error("environment listing failed")]
    EnvListing(#[source] std::io::Error),

    /// The engine sample reads `error` (ADR-0056): its latest probe
    /// returned a `DuckDB` error other than `Out of Memory`, or no probe
    /// succeeded within the success window. Health builds its 503 itself
    /// and records this as the request's failure. Fixed text: the sample's
    /// own error stays in the sampler.
    #[error("query engine sample failed")]
    EngineSampleFailed,

    /// The request was refused at the edge because its allowance had no
    /// request in progress free (503, no `Retry-After`, `Cache-Control:
    /// no-store`, ADR-0054). No handler ran and the body was never read.
    /// The message is fixed per allowance and never carries the configured
    /// value.
    #[error("{}", request_limit_message(*.0))]
    RequestLimitReached(Allowance),

    /// Internal server error (unexpected failures).
    #[error("internal error: {0}")]
    Internal(String),

    /// Work on the request path panicked and the panic was caught (500).
    ///
    /// Carries a fixed label for what panicked and nothing else: a panic's
    /// payload can quote anything the panicking code had in hand, so it
    /// reaches no error, no log field and no response (ADR-0040). Build it
    /// from a caught unwind by dropping the payload, or from a joined task
    /// with [`ServerError::from_join`].
    #[error("{0} panicked")]
    Panicked(&'static str),
}

/// What a [`ServerError::CaptureSourceTooLarge`] measured. A closed set,
/// so its remedy is fixed text and never describes the query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureSourceKind {
    /// The stored files a capture's window selects. A `service=` filter
    /// narrows that selection, so the remedy names it.
    Window,
    /// A `from saved` capture's stored run. Only a narrower window helps.
    StoredRun,
}

impl std::fmt::Display for CaptureSourceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Window => "narrow the window, or pin the services with service=a or service=a,b",
            Self::StoredRun => "narrow the window",
        })
    }
}

/// What sat underneath a server failure, read off a typed source.
///
/// [`ServerError::error_class`] says which failure the server produced;
/// this says what it came from, when a typed source is there to say so:
/// an I/O error kind, a `DuckDB` error variant, a database driver error.
/// It is built only from those types and never from display text, which
/// can hold generated SQL, event values, the user's DSL and file paths
/// (ADR-0040). A closed set of literals, so it is as safe to persist as
/// the class beside it.
///
/// Two values carry no source. [`CauseKind::None`] means the class already
/// names the whole cause: a timeout, a refusal, a caller's mistake.
/// [`CauseKind::Unknown`] means a server fault whose producer kept no typed
/// kind, and so points at a producer that should.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CauseKind {
    /// The class is the whole cause; nothing typed lies beneath it.
    None,
    /// A server fault whose source kept no typed kind.
    Unknown,
    /// [`std::io::ErrorKind::NotFound`].
    IoNotFound,
    /// [`std::io::ErrorKind::PermissionDenied`].
    IoPermissionDenied,
    /// [`std::io::ErrorKind::AlreadyExists`].
    IoAlreadyExists,
    /// [`std::io::ErrorKind::StorageFull`].
    IoStorageFull,
    /// [`std::io::ErrorKind::ReadOnlyFilesystem`].
    IoReadOnlyFilesystem,
    /// [`std::io::ErrorKind::TimedOut`].
    IoTimedOut,
    /// [`std::io::ErrorKind::Interrupted`].
    IoInterrupted,
    /// [`std::io::ErrorKind::UnexpectedEof`].
    IoUnexpectedEof,
    /// [`std::io::ErrorKind::InvalidData`].
    IoInvalidData,
    /// [`std::io::ErrorKind::OutOfMemory`].
    IoOutOfMemory,
    /// Any other I/O error kind.
    IoOther,
    /// `DuckDB` itself reported the failure (`duckdb::Error::DuckDBFailure`).
    /// Its error code is not typed reliably by the driver, so it stops here.
    DuckdbFailure,
    /// A value could not be converted between `DuckDB` and Rust types.
    DuckdbConversion,
    /// Any other `duckdb::Error` variant: a misuse of the driver's API.
    DuckdbOther,
    /// `DuckDB` ran out of the query database's spill cap or memory limit
    /// (its `Out of Memory` exception type, ADR-0056).
    DuckdbResourceExhausted,
    /// A query's patterns charged more regex memory than one query may
    /// hold ([`trawl_core::pattern::RegexRefusal::Budget`], ADR-0063).
    RegexBudgetExhausted,
    /// The server-wide regex reservation could not hold a query's charge
    /// beside what other work held
    /// ([`trawl_core::pattern::RegexRefusal::Reservation`], ADR-0063).
    RegexReservationExhausted,
    /// The postgres pool had no connection to give within its timeout.
    PostgresPoolTimedOut,
    /// The postgres pool was closed.
    PostgresPoolClosed,
    /// Postgres answered with an error of its own.
    PgServer,
    /// The connection to postgres failed at the I/O layer.
    PgIo,
    /// The connection to postgres failed at the TLS layer.
    PgTls,
    /// The postgres wire protocol went wrong.
    PgProtocol,
    /// A value or row could not be encoded or decoded.
    PgDecode,
    /// A background worker of the driver crashed.
    PgWorkerCrashed,
    /// A migration failed to apply.
    PgMigrate,
    /// The database's schema history is not one this binary can run on.
    PgSchema,
    /// Any other driver error.
    PgOther,
    /// The auth keystore's hashing or token-generation worker failed.
    AuthWorker,
    /// The hot buffer refused an ingest request for lack of free space
    /// (ADR-0043): compaction has not drained what is already admitted.
    HotBufferFull,
    /// A read was refused because WAL written before a restart is not yet
    /// proven covered by the hot buffer or Parquet (ADR-0041).
    RestartBacklog,
    /// A read was refused because a rollup is unresolved or the rollup
    /// scan failed (ADR-0026, ADR-0041).
    RollupPending,
    /// A request was refused at the edge because trawld was at its count of
    /// requests in progress, its probe allowance or its control allowance
    /// (ADR-0054).
    RequestLimitReached,
    /// Health answered 503 because the engine sample read `error`
    /// (ADR-0056).
    EngineSampleFailed,
    /// No query preparation slot came free before the request's deadline
    /// (ADR-0063).
    PreparationAllowanceExhausted,
}

impl CauseKind {
    /// Every kind, for closed-set checks and for consumers that enumerate.
    pub const ALL: [Self; 37] = [
        Self::None,
        Self::Unknown,
        Self::IoNotFound,
        Self::IoPermissionDenied,
        Self::IoAlreadyExists,
        Self::IoStorageFull,
        Self::IoReadOnlyFilesystem,
        Self::IoTimedOut,
        Self::IoInterrupted,
        Self::IoUnexpectedEof,
        Self::IoInvalidData,
        Self::IoOutOfMemory,
        Self::IoOther,
        Self::DuckdbFailure,
        Self::DuckdbConversion,
        Self::DuckdbOther,
        Self::DuckdbResourceExhausted,
        Self::RegexBudgetExhausted,
        Self::RegexReservationExhausted,
        Self::PostgresPoolTimedOut,
        Self::PostgresPoolClosed,
        Self::PgServer,
        Self::PgIo,
        Self::PgTls,
        Self::PgProtocol,
        Self::PgDecode,
        Self::PgWorkerCrashed,
        Self::PgMigrate,
        Self::PgSchema,
        Self::PgOther,
        Self::AuthWorker,
        Self::HotBufferFull,
        Self::RestartBacklog,
        Self::RollupPending,
        Self::RequestLimitReached,
        Self::EngineSampleFailed,
        Self::PreparationAllowanceExhausted,
    ];

    /// The fixed `snake_case` literal this kind is recorded as.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Unknown => "unknown",
            Self::IoNotFound => "io_not_found",
            Self::IoPermissionDenied => "io_permission_denied",
            Self::IoAlreadyExists => "io_already_exists",
            Self::IoStorageFull => "io_storage_full",
            Self::IoReadOnlyFilesystem => "io_read_only_filesystem",
            Self::IoTimedOut => "io_timed_out",
            Self::IoInterrupted => "io_interrupted",
            Self::IoUnexpectedEof => "io_unexpected_eof",
            Self::IoInvalidData => "io_invalid_data",
            Self::IoOutOfMemory => "io_out_of_memory",
            Self::IoOther => "io_other",
            Self::DuckdbFailure => "duckdb_failure",
            Self::DuckdbConversion => "duckdb_conversion",
            Self::DuckdbOther => "duckdb_other",
            Self::DuckdbResourceExhausted => "duckdb_resource_exhausted",
            Self::RegexBudgetExhausted => "regex_budget_exhausted",
            Self::RegexReservationExhausted => "regex_reservation_exhausted",
            Self::PostgresPoolTimedOut => "pg_pool_timed_out",
            Self::PostgresPoolClosed => "pg_pool_closed",
            Self::PgServer => "pg_server",
            Self::PgIo => "pg_io",
            Self::PgTls => "pg_tls",
            Self::PgProtocol => "pg_protocol",
            Self::PgDecode => "pg_decode",
            Self::PgWorkerCrashed => "pg_worker_crashed",
            Self::PgMigrate => "pg_migrate",
            Self::PgSchema => "pg_schema",
            Self::PgOther => "pg_other",
            Self::AuthWorker => "auth_worker",
            Self::HotBufferFull => "hot_buffer_full",
            Self::RestartBacklog => "restart_backlog",
            Self::RollupPending => "rollup_pending",
            Self::RequestLimitReached => "request_limit_reached",
            Self::EngineSampleFailed => "engine_sample_failed",
            Self::PreparationAllowanceExhausted => "preparation_allowance_exhausted",
        }
    }

    /// The kind of a regex memory refusal, from the bound that refused.
    fn of_regex(refusal: trawl_core::pattern::RegexRefusal) -> Self {
        use trawl_core::pattern::RegexRefusal;
        match refusal {
            RegexRefusal::Budget => Self::RegexBudgetExhausted,
            RegexRefusal::Reservation => Self::RegexReservationExhausted,
        }
    }

    /// The kind of an I/O error, from its [`std::io::ErrorKind`] alone.
    /// Public so the `trawld` binary classifies its own I/O failures.
    pub fn of_io(err: &std::io::Error) -> Self {
        use std::io::ErrorKind as K;
        match err.kind() {
            K::NotFound => Self::IoNotFound,
            K::PermissionDenied => Self::IoPermissionDenied,
            K::AlreadyExists => Self::IoAlreadyExists,
            K::StorageFull => Self::IoStorageFull,
            K::ReadOnlyFilesystem => Self::IoReadOnlyFilesystem,
            K::TimedOut => Self::IoTimedOut,
            K::Interrupted => Self::IoInterrupted,
            K::UnexpectedEof => Self::IoUnexpectedEof,
            K::InvalidData => Self::IoInvalidData,
            K::OutOfMemory => Self::IoOutOfMemory,
            _ => Self::IoOther,
        }
    }

    /// The kind of a `DuckDB` driver error, from its variant alone.
    pub(crate) fn of_duckdb(err: &duckdb::Error) -> Self {
        use duckdb::Error as E;
        match err {
            E::DuckDBFailure(..) => Self::DuckdbFailure,
            E::FromSqlConversionFailure(..)
            | E::ToSqlConversionFailure(_)
            | E::IntegralValueOutOfRange(..)
            | E::UnsignedIntegralValueOutOfRange(..)
            | E::Utf8Error(_)
            | E::InvalidColumnType(..)
            | E::ArrowTypeToDuckdbType(..) => Self::DuckdbConversion,
            _ => Self::DuckdbOther,
        }
    }

    /// The kind of a database driver error, from its variant alone.
    fn of_sqlx(err: &sqlx::Error) -> Self {
        use sqlx::Error as E;
        match err {
            E::PoolTimedOut => Self::PostgresPoolTimedOut,
            E::PoolClosed => Self::PostgresPoolClosed,
            E::Database(_) => Self::PgServer,
            E::Io(_) => Self::PgIo,
            E::Tls(_) => Self::PgTls,
            E::Protocol(_) => Self::PgProtocol,
            E::TypeNotFound { .. }
            | E::ColumnIndexOutOfBounds { .. }
            | E::ColumnNotFound(_)
            | E::ColumnDecode { .. }
            | E::Encode(_)
            | E::Decode(_) => Self::PgDecode,
            E::WorkerCrashed => Self::PgWorkerCrashed,
            E::Migrate(_) => Self::PgMigrate,
            _ => Self::PgOther,
        }
    }

    /// The kind of a fleet-auth schema check failure.
    fn of_auth_schema(err: &fleet_auth::SchemaError) -> Self {
        use fleet_auth::SchemaError as E;
        match err {
            E::Migration(_) => Self::PgMigrate,
            E::Database(e) => Self::of_sqlx(e),
            _ => Self::PgSchema,
        }
    }

    /// The kind of a trawl app-state schema check failure.
    fn of_store_schema(err: &crate::store::migrations::SchemaError) -> Self {
        use crate::store::migrations::SchemaError as E;
        match err {
            E::LegacyHistory { .. } | E::UntrackedSchema => Self::PgSchema,
            E::Migration(_) => Self::PgMigrate,
            E::Database(e) => Self::of_sqlx(e),
        }
    }
}

impl ServerError {
    /// The public message: the one string a client reads as the
    /// envelope's `message` and every record of the failure stores, which
    /// are the query tracker's recent entries, report run rows, repin job
    /// rows and the capture tracker's outcome (ADR-0040, amended
    /// 2026-11-08).
    ///
    /// It is the message `rendered()` sends, so the wire and the records
    /// cannot disagree. `rendered()` decides it per variant, with no
    /// wildcard arm at the `ServerError`, `EngineError` or `StoreError`
    /// level, so a new variant does not compile until its disclosure is
    /// written. Provenance decides what it may hold: text trawl wrote,
    /// values the request carried, and permitted domain facts such as
    /// configured limits, counts, sequence numbers and names relative to
    /// the data root. Never the `Display` text of `DuckDB`, an `io::Error`
    /// or the store driver, an absolute host path, generated SQL or a
    /// panic payload: a variant that carries those answers a fixed
    /// sentence. Structured details from the request, such as parse spans,
    /// emit hints and the `conflict` member, stay on the wire.
    ///
    /// Not for default-level telemetry: parse and emit text quotes the
    /// caller's own tokens. Events carry [`Self::error_class`] and
    /// [`Self::cause_kind`] instead.
    pub fn safe_message(&self) -> String {
        self.rendered().1.message
    }

    /// Return a stable, content-free classification of this error.
    ///
    /// SECURITY: default-filter telemetry logs this instead of any message.
    /// The public message ([`safe_message`](Self::safe_message)) goes to
    /// the client and to the failure's own records, which only key holders
    /// read, but not into telemetry: it keeps parse/emit text, which
    /// quotes the user's own tokens and format strings, and an error's raw
    /// `Display` is worse (`Self::Engine(EngineError::Database)`'s embeds
    /// the generated SQL and the values it choked on). The class comes from
    /// a closed set of literals, so it is safe to store in the retained
    /// `service=trawld` corpus and stable enough to alarm on.
    pub fn error_class(&self) -> &'static str {
        match self {
            Self::Engine(EngineError::Parse(_)) => "parse",
            Self::Engine(EngineError::Emit(_)) => "emit",
            Self::Engine(EngineError::Refused { .. }) => "refused",
            Self::Engine(EngineError::IntegerOutOfRange { .. }) => "integer_out_of_range",
            // A budget refusal ends the query database's open at startup,
            // so no request carries it; it reads as the engine it came from.
            Self::Engine(EngineError::Database(_) | EngineError::BudgetNotApplied { .. }) => {
                "database"
            }
            Self::Engine(EngineError::ResultTooLarge(_) | EngineError::AggregationCapped(_))
            | Self::CaptureTooLarge(_) => "result_too_large",
            Self::Engine(EngineError::ColdDataUnread) => "cold_data_unread",
            Self::Engine(EngineError::MissingPreparedPattern) => "missing_prepared_pattern",
            Self::Engine(EngineError::Cancelled) => "cancelled",
            Self::Engine(EngineError::Io(_)) => "io",
            // Only the run writer raises it, and the scheduler stores the
            // result as a blob instead; no request carries it.
            Self::Engine(EngineError::NotParquetStorable { .. }) => "not_parquet_storable",
            Self::Store(_) => "store",
            Self::Unauthorized(_) => "unauthorized",
            Self::Forbidden(_) => "forbidden",
            Self::BadRequest(_) => "bad_request",
            Self::NotFound(_) => "not_found",
            Self::Conflict(_) => "conflict",
            Self::WindowPolicy(_) => "window_policy",
            Self::WindowPlan(_) => "window_plan",
            Self::WindowMaterialize(_) => "window_materialize",
            Self::Timeout => "timeout",
            Self::Ingest(_) | Self::IngestBodyTooLarge { .. } => "ingest",
            Self::RateLimited { .. } => "rate_limited",
            Self::TooManyStreams => "too_many_streams",
            // An auth backend outage is the same 503 it was before the
            // driver's kind rode along; `cause_kind` tells it apart.
            // A full hot buffer is a 503 like any other refusal to serve;
            // `cause_kind` names it, as it does an auth backend outage.
            // An unsettled corpus, a failed hot snapshot and a failed env
            // listing are too: the reason and the I/O kind are cause kinds,
            // not classes. So is a
            // request refused at the request limit (ADR-0054), a query
            // that exhausted the query database's budgets, and a failed
            // engine sample (ADR-0056), and a query refused for its regex
            // memory or a preparation slot (ADR-0063).
            Self::Engine(EngineError::ResourceExhausted | EngineError::RegexResources(_))
            | Self::RegexResources(_)
            | Self::EngineSampleFailed
            | Self::ServiceUnavailable(_)
            | Self::CapacityNotStarted
            | Self::AuthBackend(_)
            | Self::HotBufferFull { .. }
            | Self::CorpusRecovering(_)
            | Self::HotSnapshot(_)
            | Self::EnvListing(_)
            | Self::RequestLimitReached(_)
            | Self::PreparationUnavailable => "service_unavailable",
            Self::IngestBatchTooLarge { .. } => "ingest_batch_too_large",
            Self::PreviewTooLarge { .. } => "preview_too_large",
            Self::CaptureSourceTooLarge { .. } => "capture_source_too_large",
            Self::CaptureBounds(_) => "capture_bounds",
            Self::CaptureBusy { .. } => "capture_busy",
            Self::CaptureQuotaExceeded { .. } => "capture_quota_exceeded",
            Self::ResultGone { .. } => "result_gone",
            Self::ResultUnavailable => "result_unavailable",
            Self::ResultCorrupt => "result_corrupt",
            Self::FocusIncomplete(_) => "focus_incomplete",
            Self::FocusAmbiguous(_) => "focus_ambiguous",
            Self::WindowsOverlap(_) => "windows_overlap",
            Self::CorrelationInvalid(_) => "correlation_invalid",
            Self::SequenceConflict { .. } => "sequence_conflict",
            Self::InvestigationExists => "investigation_exists",
            Self::InvestigationClosed => "investigation_closed",
            Self::EvidenceBudgetExceeded { .. } => "evidence_budget_exceeded",
            Self::RequestTooLarge { .. } => "request_too_large",
            Self::UnsupportedEncoding => "unsupported_encoding",
            Self::Internal(_) => "internal",
            Self::Panicked(_) => "panic",
        }
    }

    /// Return what this failure came from, read off its typed source.
    ///
    /// SECURITY: like [`error_class`](Self::error_class), this is a closed
    /// set of literals ([`CauseKind`]) and never a rendering of the error.
    /// It inspects only enum variants and I/O error kinds.
    pub fn cause_kind(&self) -> CauseKind {
        match self {
            Self::Engine(EngineError::Database(e)) => CauseKind::of_duckdb(e),
            Self::Engine(EngineError::ResourceExhausted) => CauseKind::DuckdbResourceExhausted,
            Self::Engine(EngineError::RegexResources(refusal)) | Self::RegexResources(refusal) => {
                CauseKind::of_regex(*refusal)
            }
            Self::Engine(EngineError::Io(e)) | Self::HotSnapshot(e) | Self::EnvListing(e) => {
                CauseKind::of_io(e)
            }
            Self::Store(StoreError::Unavailable(e)) => CauseKind::of_sqlx(e),
            Self::Store(StoreError::Migration(e)) => CauseKind::of_store_schema(e),
            Self::AuthBackend(kind) => *kind,
            Self::HotBufferFull { .. } => CauseKind::HotBufferFull,
            Self::CorpusRecovering(CorpusUnsettled::RestartBacklog) => CauseKind::RestartBacklog,
            Self::CorpusRecovering(CorpusUnsettled::RollupPending) => CauseKind::RollupPending,
            // One kind for every allowance: the metric label, not the
            // failure record, tells them apart.
            Self::RequestLimitReached(_) => CauseKind::RequestLimitReached,
            Self::EngineSampleFailed => CauseKind::EngineSampleFailed,
            Self::PreparationUnavailable => CauseKind::PreparationAllowanceExhausted,
            // Both were built from a string and kept no typed source.
            Self::ServiceUnavailable(_) | Self::Internal(_) => CauseKind::Unknown,
            // A pre-start capacity refusal is a pressure outcome the class
            // names in full.
            Self::CapacityNotStarted
            | Self::Engine(_)
            | Self::Store(_)
            | Self::Unauthorized(_)
            | Self::Forbidden(_)
            | Self::BadRequest(_)
            | Self::NotFound(_)
            | Self::Conflict(_)
            | Self::WindowPolicy(_)
            | Self::WindowPlan(_)
            | Self::WindowMaterialize(_)
            | Self::Timeout
            | Self::Ingest(_)
            | Self::IngestBodyTooLarge { .. }
            | Self::RateLimited { .. }
            | Self::TooManyStreams
            | Self::IngestBatchTooLarge { .. }
            | Self::PreviewTooLarge { .. }
            | Self::CaptureSourceTooLarge { .. }
            | Self::CaptureBounds(_)
            | Self::CaptureTooLarge(_)
            | Self::CaptureBusy { .. }
            | Self::CaptureQuotaExceeded { .. }
            | Self::ResultGone { .. }
            | Self::ResultUnavailable
            // The class names the whole cause: the bytes on disk are not
            // the bytes the capture wrote.
            | Self::ResultCorrupt
            | Self::FocusIncomplete(_)
            | Self::FocusAmbiguous(_)
            | Self::WindowsOverlap(_)
            | Self::CorrelationInvalid(_)
            | Self::SequenceConflict { .. }
            | Self::InvestigationExists
            | Self::InvestigationClosed
            | Self::EvidenceBudgetExceeded { .. }
            | Self::RequestTooLarge { .. }
            | Self::UnsupportedEncoding
            | Self::Panicked(_) => CauseKind::None,
        }
    }

    /// Answer for a blocking or spawned task that did not return.
    ///
    /// A panic becomes [`ServerError::Panicked`] labelled `what`, and the
    /// payload is dropped unread: `JoinError`'s `Display` quotes it, so
    /// the error is never formatted. A cancellation is an ordinary
    /// internal error, since nothing panicked.
    pub(crate) fn from_join(what: &'static str, e: tokio::task::JoinError) -> Self {
        match e.try_into_panic() {
            Ok(_payload) => Self::Panicked(what),
            Err(_cancelled) => Self::Internal(format!("{what} task was cancelled")),
        }
    }
}

/// Fixed text for a background task that did not return, for callers that
/// carry or log the failure as a string on a persisted target (compaction,
/// boot conformance, the periodic and shutdown task owners). The payload is
/// dropped unread, as in [`ServerError::from_join`]: it can quote event
/// values.
pub fn join_failure_text(what: &str, e: tokio::task::JoinError) -> String {
    match e.try_into_panic() {
        Ok(_payload) => format!("{what} task panicked"),
        Err(_cancelled) => format!("{what} task was cancelled"),
    }
}

impl From<trawl_core::correlate::BuildError> for ServerError {
    /// A correlation builder's refusal as the read's answer: overlapping
    /// windows by name, a stopped build as the cancellation it is (never a
    /// refusal of the input), every other refusal, the reparse proof
    /// included, as `correlation_invalid`. The builder's message names the
    /// input and never quotes the composed text.
    fn from(e: trawl_core::correlate::BuildError) -> Self {
        match e {
            trawl_core::correlate::BuildError::WindowsOverlap => {
                Self::WindowsOverlap(e.to_string())
            }
            trawl_core::correlate::BuildError::Cancelled => Self::Engine(EngineError::Cancelled),
            _ => Self::CorrelationInvalid(e.to_string()),
        }
    }
}

impl From<StoreError> for ServerError {
    /// A store error, except the two investigation conflicts the wire
    /// names with codes of their own (ADR-0059): a stale overwrite is
    /// [`Self::SequenceConflict`] with both numbers, and a write to a
    /// closed investigation is [`Self::InvestigationClosed`].
    fn from(err: StoreError) -> Self {
        match err {
            StoreError::SequenceConflict { expected, current } => {
                Self::SequenceConflict { expected, current }
            }
            StoreError::InvestigationClosed => Self::InvestigationClosed,
            other => Self::Store(other),
        }
    }
}

impl From<crate::store::WindowWriteError> for ServerError {
    /// Split a checked window write back into the two answers it already
    /// carries. Neither half gains or loses anything here: the store fault
    /// keeps the SQLSTATE mapping every other store error gets, and the
    /// policy refusal keeps its 400 and its message.
    fn from(err: crate::store::WindowWriteError) -> Self {
        use crate::store::WindowWriteError as E;
        match err {
            E::Store(e) => Self::from(e),
            E::Policy(e) => Self::WindowPolicy(e),
        }
    }
}

impl From<crate::store::DueClaimError> for ServerError {
    /// A manual run's claim fails the same four ways a scheduled one does,
    /// and each already has a considered wire treatment: the store fault
    /// keeps its SQLSTATE mapping and redaction, the policy refusal keeps
    /// its 400 naming both sides, and a plan or materialize failure is a
    /// redacted 500 because its inputs are stored state, not the request.
    fn from(err: crate::store::DueClaimError) -> Self {
        use crate::store::DueClaimError as E;
        match err {
            E::Store(e) => Self::from(e),
            E::Plan(e) => Self::WindowPlan(e),
            E::Policy(e) => Self::WindowPolicy(e),
            E::Materialize(e) => Self::WindowMaterialize(e),
        }
    }
}

impl From<fleet_auth::AuthError> for ServerError {
    /// Map fleet-auth keystore failures onto trawl's error contract.
    ///
    /// Backend trouble (pg down, migration state, hash-worker panics) is a
    /// 503 without postgres detail; credential failures stay an opaque 401.
    /// Anything else in this path is a bug — surface as 500. The backend
    /// 503 keeps the driver's typed kind, so the failure record can say
    /// which trouble it was without quoting it.
    fn from(err: fleet_auth::AuthError) -> Self {
        use fleet_auth::AuthError as E;
        if let Some((kind, what, source)) = auth_backend_cause(&err) {
            return Self::auth_backend_fault(kind, what, source);
        }
        match err {
            E::InvalidKey(_) | E::MalformedToken(_) => {
                Self::Unauthorized("authentication failed".into())
            }
            other => Self::Internal(format!("unexpected fleet-auth error: {other}")),
        }
    }
}

/// The backend trouble in a keystore failure, if it is one: the driver's
/// typed kind, a fixed label, and the inner error whose text belongs at
/// DEBUG only. `None` for a credential failure or any other variant.
///
/// It emits nothing, so a caller that swallows the failure can log its own
/// event with the class of [`ServerError::AuthBackend`] and this kind,
/// without the `auth.backend` event the [`From`] conversion writes.
pub(crate) fn auth_backend_cause(
    err: &fleet_auth::AuthError,
) -> Option<(CauseKind, &'static str, &dyn std::fmt::Display)> {
    use fleet_auth::AuthError as E;
    match err {
        E::Database(e) => Some((CauseKind::of_sqlx(e), "fleet auth backend error", e)),
        E::Migration(e) => Some((CauseKind::PgMigrate, "fleet auth migration error", e)),
        E::Schema(e) => Some((CauseKind::of_auth_schema(e), "fleet auth schema error", e)),
        E::Hash(e) | E::TokenGeneration(e) => {
            Some((CauseKind::AuthWorker, "fleet auth worker error", e))
        }
        _ => None,
    }
}

pub(crate) fn parse_error_to_detail(e: &trawl_core::parser::ParseError) -> trawl_api::ErrorDetail {
    trawl_api::ErrorDetail {
        message: e.message.clone(),
        span: Some(trawl_api::ErrorSpan {
            start: e.span.start,
            end: e.span.end,
        }),
        label: e.label.clone(),
        hint: e.hint.clone(),
    }
}

impl ServerError {
    /// The wire code this error answers with: the `code` of the envelope
    /// its response carries. An investigation step records it as the code
    /// its work ended with (ADR-0059).
    #[must_use]
    pub fn code(&self) -> trawl_api::ErrorCode {
        self.rendered().1.code
    }

    /// The status and the envelope this error answers with, and the
    /// disclosure projection: the envelope's `message` is the public
    /// message every record stores ([`Self::safe_message`]). Pure: the
    /// log of a fault is [`Self::log_fault`]'s, written once per response.
    ///
    /// No arm is a wildcard, here or in the nested `Store` match, and the
    /// lint below keeps it so: a new variant is a compile error until its
    /// disclosure is decided.
    #[allow(clippy::too_many_lines)] // exhaustive error table is cohesive
    #[deny(clippy::wildcard_enum_match_arm)]
    fn rendered(&self) -> (StatusCode, trawl_api::ErrorEnvelope) {
        use trawl_api::{ErrorCode, ErrorEnvelope};

        match self {
            Self::Engine(EngineError::Parse(errors)) => {
                let details: Vec<_> = errors.iter().map(parse_error_to_detail).collect();
                let message = errors
                    .first()
                    .map_or("parse error".to_owned(), |e| e.message.clone());
                (
                    StatusCode::BAD_REQUEST,
                    ErrorEnvelope {
                        code: ErrorCode::ParseError,
                        message,
                        details,
                        conflict: None,
                    },
                )
            }
            // The summary keeps the whole sentence, hint included, for a
            // client that reads only `message`. The one detail carries the
            // sentence and the hint apart, so a client that renders details
            // shows the hint once. No span: the emitter knows which
            // function is wrong but not where it sits in the text, and a
            // span over the whole query would put a caret under everything
            // (ADR-0039).
            Self::Engine(EngineError::Emit(e)) => (
                StatusCode::BAD_REQUEST,
                ErrorEnvelope {
                    code: ErrorCode::ValidationError,
                    message: e.to_string(),
                    details: vec![trawl_api::ErrorDetail {
                        message: e.message(),
                        span: None,
                        label: None,
                        hint: e.hint(),
                    }],
                    conflict: None,
                },
            ),
            // The engine proved the query wrong by asking `DuckDB` a
            // question of its own, which makes it the caller's mistake
            // and not the server's: same 400 class as an emitter
            // refusal, and the same sentence, which is trawl-authored
            // and quotes only the caller's own tokens.
            Self::Engine(EngineError::Refused { message }) => (
                StatusCode::BAD_REQUEST,
                ErrorEnvelope::simple(ErrorCode::ValidationError, message.clone()),
            ),
            // A result cell no wire integer holds (ADR-0035, amended
            // 2026-10-22). The query ran; its answer has no exact spelling,
            // and only the caller can choose a cast, so it is the caller's
            // to fix: a 400 under the execution code, with the
            // trawl-authored sentence naming the column and the casts.
            Self::Engine(err @ EngineError::IntegerOutOfRange { .. }) => (
                StatusCode::BAD_REQUEST,
                ErrorEnvelope::simple(ErrorCode::ExecutionError, err.to_string()),
            ),
            Self::Engine(EngineError::ResultTooLarge(n)) => (
                StatusCode::BAD_REQUEST,
                ErrorEnvelope::simple(
                    ErrorCode::ResultTooLarge,
                    format!("result exceeded {n} row limit"),
                ),
            ),
            // The same contract as a row cap: the whole answer would not
            // fit, here inside one aggregation (ADR-0001). The sentence is
            // trawl-authored and names only the cap and its limit.
            Self::Engine(err @ EngineError::AggregationCapped(_)) => (
                StatusCode::BAD_REQUEST,
                ErrorEnvelope::simple(ErrorCode::ResultTooLarge, err.to_string()),
            ),
            // Database/IO errors answer without their details, which the
            // log names by class and cause, with the text at DEBUG only.
            //
            // A cancellation caught at the bind-to-execute boundary answers
            // here too, deliberately: cancelling a running query has always
            // surfaced as `DuckDB`'s interrupted execution, and the latch
            // exists to catch the same request when the interrupt was
            // swallowed during a bind (ADR-0024). One request, one answer —
            // the latch does not earn the caller a different status than the
            // interrupt it backs up. `error_class` still tells the two apart
            // for telemetry. The run writer's refusal never reaches a
            // request (the scheduler stores a blob instead); were it to,
            // it is the server's fault, not the query's.
            Self::Engine(
                EngineError::Database(_)
                | EngineError::BudgetNotApplied { .. }
                | EngineError::Io(_)
                | EngineError::Cancelled
                | EngineError::NotParquetStorable { .. },
            ) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorEnvelope::simple(ErrorCode::ExecutionError, "query execution failed"),
            ),
            // The query ran out of the query database's spill cap or memory
            // limit (ADR-0056). The server is fine and the same query would
            // fail the same way, so there is no `Retry-After`; the fixed
            // sentence tells the caller what helps and names no path, size
            // or `DuckDB` text.
            //
            // A query refused for its regex memory answers the same way
            // (ADR-0063): the same query asks for the same memory, and the
            // sentence names no pattern. Stream setup and the batch tail
            // raise it from two places.
            Self::Engine(EngineError::ResourceExhausted | EngineError::RegexResources(_))
            | Self::RegexResources(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorEnvelope::simple(ErrorCode::QueryResourceExhausted, QUERY_RESOURCE_EXHAUSTED),
            ),
            // A server invariant breach, never the query's fault, so a 500
            // rather than the 400 that would blame the query. The fixed
            // sentence names no pattern; `log_fault` records the class.
            Self::Engine(EngineError::MissingPreparedPattern) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorEnvelope::simple(ErrorCode::InternalError, MISSING_PREPARED_PATTERN),
            ),
            // The read raced a file move while cold data exists — transient
            // and retryable, never a silent empty 200 (ADR-0008).
            Self::Engine(EngineError::ColdDataUnread) => (
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorEnvelope::simple(
                    ErrorCode::ServiceUnavailable,
                    "cold data temporarily unreadable; retry the query",
                ),
            ),
            // App-state store errors follow the ADR-0004 table. Backend
            // trouble is a 503 with pg diagnostics redacted from the wire
            // (their text is logged at DEBUG only); conflicts are 409 with
            // their domain message; ownership misses are opaque 404s.
            Self::Store(e) => match e {
                StoreError::Unavailable(_) | StoreError::Migration(_) | StoreError::LockHeld => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    ErrorEnvelope::simple(
                        ErrorCode::ServiceUnavailable,
                        "app-state store unavailable",
                    ),
                ),
                // The purge is neither committed nor rolled back as far as
                // this process knows, and 503 is the honest answer: the
                // request did not complete, and retrying is safe only after
                // the operator has looked. The message says so; it names no
                // pg diagnostics.
                // Same 503 for the pre-commit bound, and the same reason to
                // say more than "store unavailable": the operator's next
                // move differs from a plain outage. This one adds that
                // nothing was reclaimed, which is provable — the dropped
                // transaction rolled back. A deadline-bounded write that gave
                // up before its commit says the same; the admissions that
                // bound one answer `capture_busy` instead.
                StoreError::PurgeCommitUnknown
                | StoreError::PurgePrepareTimeout
                | StoreError::WaitExpired => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    ErrorEnvelope::simple(ErrorCode::ServiceUnavailable, e.to_string()),
                ),
                StoreError::DuplicateName { .. } => (
                    StatusCode::CONFLICT,
                    ErrorEnvelope::simple(ErrorCode::DuplicateName, e.to_string()),
                ),
                StoreError::ScheduleExists { .. } | StoreError::RepinAlreadyRunning => (
                    StatusCode::CONFLICT,
                    ErrorEnvelope::simple(ErrorCode::BadRequest, e.to_string()),
                ),
                StoreError::NotFound { resource, .. } => (
                    StatusCode::NOT_FOUND,
                    ErrorEnvelope::simple(
                        ErrorCode::NotFound,
                        format!("{resource} not found or unauthorized"),
                    ),
                ),
                // Every key with the read grant sees every investigation,
                // so a miss means the id names none.
                StoreError::InvestigationNotFound => (
                    StatusCode::NOT_FOUND,
                    ErrorEnvelope::simple(ErrorCode::NotFound, "investigation not found"),
                ),
                // `From<StoreError>` lifts these two into their own
                // variants; a `Store` built around one by hand still
                // answers the same code.
                StoreError::SequenceConflict { expected, current } => (
                    StatusCode::CONFLICT,
                    ErrorEnvelope {
                        conflict: Some(trawl_api::investigations::SequenceConflict {
                            expected: *expected,
                            current: *current,
                        }),
                        ..ErrorEnvelope::simple(ErrorCode::SequenceConflict, e.to_string())
                    },
                ),
                StoreError::InvestigationClosed => (
                    StatusCode::CONFLICT,
                    ErrorEnvelope::simple(ErrorCode::InvestigationClosed, e.to_string()),
                ),
                StoreError::Validation(_)
                | StoreError::RepinPinVanished { .. }
                | StoreError::InvalidInterval { .. }
                | StoreError::IntervalTooShort { .. }
                | StoreError::DurationTooLong { .. }
                | StoreError::LagWithoutWindow { .. } => (
                    StatusCode::BAD_REQUEST,
                    ErrorEnvelope::simple(ErrorCode::BadRequest, e.to_string()),
                ),
                StoreError::InvalidName { .. } => (
                    StatusCode::BAD_REQUEST,
                    ErrorEnvelope::simple(ErrorCode::InvalidName, e.to_string()),
                ),
            },
            Self::Unauthorized(msg) => (
                StatusCode::UNAUTHORIZED,
                ErrorEnvelope::simple(ErrorCode::AuthError, msg.clone()),
            ),
            Self::Forbidden(msg) => (
                StatusCode::FORBIDDEN,
                ErrorEnvelope::simple(ErrorCode::Forbidden, msg.clone()),
            ),
            Self::Ingest(msg) => (
                StatusCode::BAD_REQUEST,
                ErrorEnvelope::simple(ErrorCode::IngestError, msg.clone()),
            ),
            // 413 like the router's wire-size refusal, but inside the
            // envelope: `ingest_error` is the code for an ingest body the
            // server will not read, and `ingest_batch_too_large` is the hot
            // buffer's ceiling, which this is not.
            Self::IngestBodyTooLarge { .. } => (
                StatusCode::PAYLOAD_TOO_LARGE,
                ErrorEnvelope::simple(ErrorCode::IngestError, self.to_string()),
            ),
            Self::BadRequest(msg) => (
                StatusCode::BAD_REQUEST,
                ErrorEnvelope::simple(ErrorCode::BadRequest, msg.clone()),
            ),
            Self::NotFound(msg) => (
                StatusCode::NOT_FOUND,
                ErrorEnvelope::simple(ErrorCode::NotFound, msg.clone()),
            ),
            // 409 with the domain message, the same rendering the store's
            // own conflicts get: there is no ErrorCode::Conflict, and the
            // status is what a client branches on.
            Self::Conflict(msg) => (
                StatusCode::CONFLICT,
                ErrorEnvelope::simple(ErrorCode::BadRequest, msg.clone()),
            ),
            // Both sides of the conflict are in the message, and both are
            // the operator's: nothing to redact.
            Self::WindowPolicy(e) => (
                StatusCode::BAD_REQUEST,
                ErrorEnvelope::simple(ErrorCode::BadRequest, e.to_string()),
            ),
            // Planning and materialization read stored state, so a failure
            // here is the server's to fix. The detail goes to the log.
            Self::WindowPlan(_) | Self::WindowMaterialize(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorEnvelope::simple(
                    ErrorCode::InternalError,
                    "report window could not be resolved",
                ),
            ),
            Self::Timeout => (
                StatusCode::GATEWAY_TIMEOUT,
                ErrorEnvelope::simple(ErrorCode::Timeout, "query timed out"),
            ),
            Self::RateLimited { .. } => (
                StatusCode::TOO_MANY_REQUESTS,
                ErrorEnvelope::simple(ErrorCode::RateLimited, "rate limit exceeded"),
            ),
            Self::TooManyStreams => (
                StatusCode::TOO_MANY_REQUESTS,
                ErrorEnvelope::simple(ErrorCode::TooManyStreams, "too many concurrent streams"),
            ),
            Self::ServiceUnavailable(msg) => (
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorEnvelope::simple(ErrorCode::ServiceUnavailable, msg.clone()),
            ),
            // No `Retry-After`: nothing says when a permit frees, and the
            // caller's own deadline is what ran out.
            Self::CapacityNotStarted => (
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorEnvelope::simple(ErrorCode::ServiceUnavailable, CAPACITY_NOT_STARTED),
            ),
            Self::AuthBackend(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorEnvelope::simple(ErrorCode::ServiceUnavailable, "auth backend unavailable"),
            ),
            // `Retry-After` is added below, from the variant.
            Self::HotBufferFull { .. } => (
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorEnvelope::simple(
                    ErrorCode::HotBufferFull,
                    "hot buffer full: nothing from this request was written; retry it after \
                     compaction drains",
                ),
            ),
            // No `Retry-After`: nothing says when a backlog drains or a
            // rollup finishes. The reason is the failure record's cause
            // kind, and the sentence already says it in words.
            Self::CorpusRecovering(reason) => (
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorEnvelope::simple(
                    ErrorCode::CorpusRecovering,
                    corpus_recovering_message(*reason),
                ),
            ),
            // Never a cold-only answer in place of the newest events. The
            // I/O error's text stays off the wire; its kind is recorded.
            Self::HotSnapshot(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorEnvelope::simple(
                    ErrorCode::ServiceUnavailable,
                    "recent events could not be read for this query",
                ),
            ),
            // No answer that vouches for envs it could not list. The I/O
            // error's text stays off the wire; its kind is recorded.
            Self::EnvListing(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorEnvelope::simple(ErrorCode::ServiceUnavailable, "environment listing failed"),
            ),
            Self::EngineSampleFailed => (
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorEnvelope::simple(ErrorCode::ServiceUnavailable, "query engine unavailable"),
            ),
            // Every preparation slot stayed in use until the deadline
            // (ADR-0063). No `Retry-After`, as for the request limit
            // (ADR-0054): nothing predicts when a slot frees.
            Self::PreparationUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorEnvelope::simple(
                    ErrorCode::QueryResourceExhausted,
                    PREPARATION_ALLOWANCE_EXHAUSTED,
                ),
            ),
            // No `Retry-After`: nothing predicts when a request in progress
            // ends, and a fixed hint would make senders retry together
            // (ADR-0054). `Cache-Control: no-store` is added below.
            Self::RequestLimitReached(allowance) => (
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorEnvelope::simple(
                    ErrorCode::RequestLimitReached,
                    request_limit_message(*allowance),
                ),
            ),
            // A client error with no `Retry-After`: the same request can
            // never fit.
            Self::IngestBatchTooLarge { .. } => (
                StatusCode::PAYLOAD_TOO_LARGE,
                ErrorEnvelope::simple(ErrorCode::IngestBatchTooLarge, self.to_string()),
            ),
            // Fixed text and the limit: nothing from the sample.
            Self::PreviewTooLarge { .. } => (
                StatusCode::PAYLOAD_TOO_LARGE,
                ErrorEnvelope::simple(ErrorCode::PreviewTooLarge, self.to_string()),
            ),
            // The caller's to fix by narrowing the window, so a 400 and no
            // `Retry-After`: the same window selects the same files. Fixed
            // text and two counts.
            Self::CaptureSourceTooLarge { .. } => (
                StatusCode::BAD_REQUEST,
                ErrorEnvelope::simple(ErrorCode::CaptureSourceTooLarge, self.to_string()),
            ),
            // The caller's to fix, and the message says how; nothing was
            // taken.
            Self::CaptureBounds(e) => (
                StatusCode::BAD_REQUEST,
                ErrorEnvelope::simple(ErrorCode::BadRequest, e.to_string()),
            ),
            // The code a row-capped `/query` answers, with the capture's
            // own hint.
            Self::CaptureTooLarge(_) => (
                StatusCode::BAD_REQUEST,
                ErrorEnvelope::simple(ErrorCode::ResultTooLarge, self.to_string()),
            ),
            // `Retry-After` is added below, from the variant.
            Self::CaptureBusy { .. } => (
                StatusCode::TOO_MANY_REQUESTS,
                ErrorEnvelope::simple(ErrorCode::CaptureBusy, self.to_string()),
            ),
            Self::CaptureQuotaExceeded { .. } => (
                StatusCode::TOO_MANY_REQUESTS,
                ErrorEnvelope::simple(ErrorCode::CaptureQuotaExceeded, self.to_string()),
            ),
            Self::ResultGone { .. } => (
                StatusCode::GONE,
                ErrorEnvelope::simple(ErrorCode::ResultGone, self.to_string()),
            ),
            Self::ResultUnavailable => (
                StatusCode::CONFLICT,
                ErrorEnvelope::simple(ErrorCode::ResultUnavailable, self.to_string()),
            ),
            // The read path logs the result's id where it finds the
            // mismatch; the wire names neither a path nor a digest.
            Self::ResultCorrupt => (
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorEnvelope::simple(ErrorCode::ResultCorrupt, self.to_string()),
            ),
            // A correlation read's input, the caller's to fix; nothing was
            // taken.
            Self::FocusIncomplete(message) => (
                StatusCode::BAD_REQUEST,
                ErrorEnvelope::simple(ErrorCode::FocusIncomplete, message),
            ),
            Self::FocusAmbiguous(message) => (
                StatusCode::BAD_REQUEST,
                ErrorEnvelope::simple(ErrorCode::FocusAmbiguous, message),
            ),
            Self::WindowsOverlap(message) => (
                StatusCode::BAD_REQUEST,
                ErrorEnvelope::simple(ErrorCode::WindowsOverlap, message),
            ),
            Self::CorrelationInvalid(message) => (
                StatusCode::BAD_REQUEST,
                ErrorEnvelope::simple(ErrorCode::CorrelationInvalid, message),
            ),
            // The one envelope with a `conflict` member: the client reads
            // the current sequence from it instead of parsing the message.
            Self::SequenceConflict { expected, current } => (
                StatusCode::CONFLICT,
                ErrorEnvelope {
                    conflict: Some(trawl_api::investigations::SequenceConflict {
                        expected: *expected,
                        current: *current,
                    }),
                    ..ErrorEnvelope::simple(ErrorCode::SequenceConflict, self.to_string())
                },
            ),
            Self::InvestigationExists => (
                StatusCode::CONFLICT,
                ErrorEnvelope::simple(ErrorCode::InvestigationExists, self.to_string()),
            ),
            Self::InvestigationClosed => (
                StatusCode::CONFLICT,
                ErrorEnvelope::simple(ErrorCode::InvestigationClosed, self.to_string()),
            ),
            Self::EvidenceBudgetExceeded { .. } => (
                StatusCode::CONFLICT,
                ErrorEnvelope::simple(ErrorCode::EvidenceBudgetExceeded, self.to_string()),
            ),
            // Fixed text, the setting and its value: nothing from the request.
            Self::RequestTooLarge { .. } => (
                StatusCode::PAYLOAD_TOO_LARGE,
                ErrorEnvelope::simple(ErrorCode::RequestTooLarge, self.to_string()),
            ),
            Self::UnsupportedEncoding => (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                ErrorEnvelope::simple(ErrorCode::UnsupportedEncoding, self.to_string()),
            ),
            // `self` renders as the fixed label only: a panic's payload
            // never made it into the variant.
            Self::Internal(_) | Self::Panicked(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorEnvelope::simple(ErrorCode::InternalError, "internal server error"),
            ),
        }
    }

    /// Log a fault its response redacts: a store's diagnostics, a report
    /// window's failure, an internal error.
    ///
    /// The ERROR event carries the class and the cause kind, never the
    /// error's text, which can quote the saved query, paths and driver
    /// diagnostics. The text goes to a DEBUG event beside it, which the
    /// default filter drops (ADR-0040, amended 2026-11-08).
    fn log_fault(&self) {
        let error_class = self.error_class();
        let cause_kind = self.cause_kind().as_str();
        match self {
            Self::Store(StoreError::Unavailable(source)) => {
                tracing::error!(
                    target: "storage.backend",
                    error_class,
                    cause_kind,
                    "app-state store error"
                );
                tracing::debug!(
                    target: "storage.backend",
                    error = %source,
                    "app-state store error"
                );
            }
            Self::Store(StoreError::Migration(source)) => {
                tracing::error!(
                    target: "storage.backend",
                    error_class,
                    cause_kind,
                    "app-state store migration error"
                );
                tracing::debug!(
                    target: "storage.backend",
                    error = %source,
                    "app-state store migration error"
                );
            }
            Self::WindowPlan(_) | Self::WindowMaterialize(_) => {
                tracing::error!(
                    event_type = "report_window_error",
                    error_class,
                    cause_kind,
                    "report window could not be resolved"
                );
                tracing::debug!(
                    event_type = "report_window_error",
                    error = %self,
                    "report window could not be resolved"
                );
            }
            Self::Engine(EngineError::MissingPreparedPattern) => {
                tracing::error!(
                    event_type = "missing_prepared_pattern",
                    error_class,
                    cause_kind,
                    "internal server error: a query's tail had no prepared pattern"
                );
                tracing::debug!(
                    event_type = "missing_prepared_pattern",
                    error = %self,
                    "internal server error: a query's tail had no prepared pattern"
                );
            }
            Self::Internal(_) | Self::Panicked(_) => {
                tracing::error!(
                    event_type = "internal_error",
                    error_class,
                    cause_kind,
                    "internal server error"
                );
                tracing::debug!(
                    event_type = "internal_error",
                    error = %self,
                    "internal server error"
                );
            }
            _ => {}
        }
    }

    /// The auth backend's 503, logged where a keystore failure converts:
    /// the class and the driver's kind at ERROR, and the driver's text,
    /// which the wire never carries, at DEBUG only (ADR-0040, amended
    /// 2026-11-08).
    fn auth_backend_fault(
        kind: CauseKind,
        what: &'static str,
        source: &dyn std::fmt::Display,
    ) -> Self {
        let error = Self::AuthBackend(kind);
        tracing::error!(
            target: "auth.backend",
            error_class = error.error_class(),
            cause_kind = kind.as_str(),
            "{what}"
        );
        tracing::debug!(target: "auth.backend", error = %source, "{what}");
        error
    }
}

impl IntoResponse for ServerError {
    fn into_response(self) -> Response {
        // The request's failure record takes the class and cause kind,
        // never this error's text (ADR-0040).
        crate::transport::failure::record_error(&self);
        self.log_fault();
        let (status, envelope) = self.rendered();

        let body = trawl_api::ErrorResponse { error: envelope };
        let mut response = (status, axum::Json(body)).into_response();
        match self {
            Self::HotBufferFull { retry_after_secs }
            | Self::RateLimited { retry_after_secs }
            | Self::CaptureBusy { retry_after_secs }
            | Self::CaptureQuotaExceeded { retry_after_secs } => {
                response.headers_mut().insert(
                    axum::http::header::RETRY_AFTER,
                    axum::http::HeaderValue::from(retry_after_secs),
                );
            }
            // The refusal is the edge's answer for every route, the ingest
            // preview included, whose own no-store layer sits inside the
            // count and never sees it.
            Self::RequestLimitReached(_) => {
                response.headers_mut().insert(
                    axum::http::header::CACHE_CONTROL,
                    axum::http::HeaderValue::from_static("no-store"),
                );
            }
            _ => {}
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stopped correlation build is a cancellation, never `correlation_invalid`.
    #[test]
    fn a_cancelled_correlation_build_is_a_cancellation() {
        assert!(matches!(
            ServerError::from(trawl_core::correlate::BuildError::Cancelled),
            ServerError::Engine(EngineError::Cancelled)
        ));
    }

    #[tokio::test]
    async fn authentication_failures_use_the_current_opaque_wire_code() {
        for error in [
            fleet_auth::AuthError::InvalidKey("private-rejection-reason".into()),
            fleet_auth::AuthError::MalformedToken("private-token-value".into()),
        ] {
            let response = ServerError::from(error).into_response();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            let bytes = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["error"]["code"], "auth_error");
            assert_eq!(body["error"]["message"], "authentication failed");
            assert!(!String::from_utf8_lossy(&bytes).contains("private-"));
        }
    }

    /// A batch aggregation past a cap answers like a row cap: 400
    /// `result_too_large`, with a message that names the cap and its limit.
    #[tokio::test]
    async fn aggregation_cap_answers_result_too_large() {
        let err = ServerError::Engine(EngineError::AggregationCapped(
            trawl_core::stream::AggregationCap::Groups,
        ));
        assert_eq!(err.error_class(), "result_too_large");
        assert_eq!(err.cause_kind(), CauseKind::None);
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["code"], "result_too_large");
        assert_eq!(
            body["error"]["message"],
            "result too large: the aggregation has more than 10000 groups; narrow the query"
        );
    }

    /// A tail stage with no prepared pattern is the server's fault: a 500
    /// with a fixed sentence and its own class, never the 400 that blames
    /// the query, and nothing of any pattern on the wire.
    #[tokio::test]
    async fn a_missing_prepared_pattern_answers_as_a_server_fault() {
        let err = ServerError::Engine(EngineError::MissingPreparedPattern);
        assert_eq!(err.error_class(), "missing_prepared_pattern");
        assert_eq!(err.cause_kind(), CauseKind::None);
        // The tracker stores the sentence the client reads, not the
        // engine's own wording of the invariant it broke.
        assert_eq!(err.safe_message(), MISSING_PREPARED_PATTERN);
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["code"], "internal_error");
        assert_eq!(body["error"]["message"], MISSING_PREPARED_PATTERN);
        assert!(body["error"].get("details").is_none(), "{body}");
    }

    /// Parse and emit a query the emitter refuses, and return its refusal.
    fn emit_error(query: &str) -> trawl_core::emitter::EmitError {
        let parsed = trawl_core::parser::parse(query).expect("the query parses");
        trawl_core::emitter::emit(
            &parsed,
            "/data/**/*.parquet",
            trawl_core::context::EvalContext::capture(),
        )
        .expect_err("the emitter refuses the query")
    }

    /// Parse and emit a query the emitter refuses, and read the envelope
    /// the server answers with.
    async fn emit_refusal(query: &str) -> (StatusCode, serde_json::Value) {
        let response = ServerError::Engine(EngineError::Emit(emit_error(query))).into_response();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    /// A validation error carries one spanless detail: the sentence and
    /// the did-you-mean apart, so a client shows the hint once. The
    /// summary `message` is the sentence it always was, hint included.
    #[tokio::test]
    async fn an_unknown_function_with_a_near_miss_details_its_message_and_hint() {
        let (status, body) = emit_refusal("* | stats countt(x) by host").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let error = &body["error"];
        assert_eq!(error["code"], "validation_error");
        assert_eq!(
            error["message"],
            "unknown function: countt (did you mean 'count'?)"
        );
        let details = error["details"].as_array().expect("details is an array");
        assert_eq!(details.len(), 1);
        assert_eq!(details[0]["message"], "unknown function: countt");
        assert_eq!(details[0]["hint"], "did you mean 'count'?");
        assert!(
            details[0]
                .get("span")
                .is_none_or(serde_json::Value::is_null)
        );
        assert!(
            details[0]
                .get("label")
                .is_none_or(serde_json::Value::is_null)
        );
    }

    /// With no suggestion within reach the detail still carries the
    /// sentence, and no hint.
    #[tokio::test]
    async fn an_unknown_function_without_a_near_miss_details_only_its_message() {
        let (status, body) = emit_refusal("* | stats nosuchfunc(x) by host").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let error = &body["error"];
        assert_eq!(error["code"], "validation_error");
        assert_eq!(error["message"], "unknown function: nosuchfunc");
        let details = error["details"].as_array().expect("details is an array");
        assert_eq!(details.len(), 1);
        assert_eq!(details[0]["message"], "unknown function: nosuchfunc");
        assert!(
            details[0]
                .get("hint")
                .is_none_or(serde_json::Value::is_null)
        );
        assert!(
            details[0]
                .get("span")
                .is_none_or(serde_json::Value::is_null)
        );
    }

    /// A refusal from `DuckDB`'s own verdict stays a bare summary: the
    /// engine's sentence names no function the client could point at.
    #[tokio::test]
    async fn a_refused_query_keeps_an_empty_details_list() {
        let response = ServerError::Engine(EngineError::Refused {
            message: "no such column".into(),
        })
        .into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["code"], "validation_error");
        assert_eq!(body["error"]["message"], "no such column");
        assert!(
            body["error"]
                .get("details")
                .is_none_or(|d| d.as_array().is_some_and(Vec::is_empty))
        );
    }

    #[test]
    fn internal_maps_to_500() {
        let err = ServerError::Internal("something broke".into());
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    /// The correlation reads' refusals (ADR-0058): 400 with their own code
    /// and class, no `Retry-After`, no cause, and the message as built.
    #[tokio::test]
    async fn correlation_refusals_are_400s_with_their_own_codes() {
        let cases = [
            (
                ServerError::FocusIncomplete("the focus row has no `host`".into()),
                "focus_incomplete",
            ),
            (
                ServerError::FocusAmbiguous("the focus row has two `env` columns".into()),
                "focus_ambiguous",
            ),
            (
                ServerError::WindowsOverlap("the foreground and baseline windows overlap".into()),
                "windows_overlap",
            ),
            (
                ServerError::CorrelationInvalid("the literal is empty".into()),
                "correlation_invalid",
            ),
        ];
        for (error, code) in cases {
            assert_eq!(error.error_class(), code);
            assert_eq!(error.cause_kind(), CauseKind::None);
            assert_eq!(error.safe_message(), error.to_string());
            let message = error.to_string();
            let response = error.into_response();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{code}");
            assert!(
                response
                    .headers()
                    .get(axum::http::header::RETRY_AFTER)
                    .is_none()
            );
            let body: serde_json::Value =
                serde_json::from_str(&body_string(response).await).unwrap();
            assert_eq!(body["error"]["code"], code);
            assert_eq!(body["error"]["message"], message);
        }
    }

    /// A builder's refusal maps by kind: overlapping windows by name, every
    /// other one, the reparse proof included, as `correlation_invalid`, each
    /// with the builder's caller-facing message.
    #[test]
    fn a_build_error_maps_overlap_by_name_and_the_rest_as_invalid() {
        use trawl_core::correlate::BuildError;

        let overlap = ServerError::from(BuildError::WindowsOverlap);
        assert!(matches!(&overlap, ServerError::WindowsOverlap(_)));
        assert_eq!(overlap.to_string(), BuildError::WindowsOverlap.to_string());

        for refusal in [
            BuildError::EmptyScope,
            BuildError::EmptyLiteral,
            BuildError::Nul { what: "literal" },
            BuildError::LiteralTooLong { bytes: 2048 },
            BuildError::FilterParse {
                message: "unexpected '|'".into(),
            },
            BuildError::FilterShape {
                reason: "has pipe stages".into(),
            },
            BuildError::Field {
                name: "_raw".into(),
                reason: "is not a group field",
            },
            BuildError::TooManyGroups { count: 3 },
            BuildError::MinCount,
            BuildError::Window {
                which: "baseline",
                reason: "is empty",
            },
            BuildError::Reparse,
            BuildError::TooLong { bytes: 70_000 },
        ] {
            let message = refusal.to_string();
            let error = ServerError::from(refusal);
            assert!(
                matches!(&error, ServerError::CorrelationInvalid(m) if *m == message),
                "{error:?}"
            );
            assert_eq!(error.error_class(), "correlation_invalid");
        }
        assert_eq!(
            ServerError::from(BuildError::Reparse).to_string(),
            "the composed query does not read back as built; the read was not run"
        );
    }

    /// The class is what default telemetry persists, so it must be a fixed
    /// literal — never a rendering of the user's own query text.
    #[test]
    fn error_class_never_carries_user_content() {
        let parse = ServerError::Engine(EngineError::Parse(vec![trawl_core::parser::ParseError {
            message: "unexpected 'zz_secret_token'".into(),
            span: 0..3,
            label: None,
            hint: None,
        }]));
        assert_eq!(parse.error_class(), "parse");
        assert!(parse.safe_message().contains("zz_secret_token"));

        let db = ServerError::Engine(EngineError::Database(duckdb::Error::InvalidColumnName(
            "zz_secret_column".into(),
        )));
        assert_eq!(db.error_class(), "database");
        assert!(!db.to_string().is_empty());

        assert_eq!(ServerError::Timeout.error_class(), "timeout");
        // Pin gc's refusals carry an operator instruction and data-root
        // relative names, never the absolute root; the class stays a
        // literal either way.
        let root = std::path::Path::new("/var/lib/trawl/data");
        let mut problems = crate::catalog::gc::Problems::new(root);
        problems.note(&root.join("prod/x.parquet"), "not a regular file");
        let conflict = ServerError::Conflict(problems.refusal().expect("one problem refuses"));
        assert_eq!(conflict.error_class(), "conflict");
        let public = conflict.safe_message();
        assert!(public.contains("prod/x.parquet"), "{public}");
        assert!(!public.contains("/var/lib/trawl"), "{public}");
        assert_eq!(
            ServerError::Internal("dsn leaked".into()).error_class(),
            "internal"
        );
        assert_eq!(ServerError::Panicked("query worker").error_class(), "panic");
        assert_eq!(
            ServerError::EngineSampleFailed.error_class(),
            "service_unavailable"
        );
    }

    /// The cause kind is persisted beside the class, so it is held to the
    /// same standard: a closed set of fixed `snake_case` literals, one per
    /// variant, and `ALL` names every variant exactly once.
    #[test]
    fn cause_kind_is_a_closed_snake_case_set() {
        // Exhaustive on purpose: a new variant fails to compile here until
        // it is counted, and the count below then holds `ALL` to it.
        let variants = CauseKind::ALL
            .iter()
            .filter(|kind| match kind {
                CauseKind::None
                | CauseKind::Unknown
                | CauseKind::IoNotFound
                | CauseKind::IoPermissionDenied
                | CauseKind::IoAlreadyExists
                | CauseKind::IoStorageFull
                | CauseKind::IoReadOnlyFilesystem
                | CauseKind::IoTimedOut
                | CauseKind::IoInterrupted
                | CauseKind::IoUnexpectedEof
                | CauseKind::IoInvalidData
                | CauseKind::IoOutOfMemory
                | CauseKind::IoOther
                | CauseKind::DuckdbFailure
                | CauseKind::DuckdbConversion
                | CauseKind::DuckdbOther
                | CauseKind::DuckdbResourceExhausted
                | CauseKind::RegexBudgetExhausted
                | CauseKind::RegexReservationExhausted
                | CauseKind::PostgresPoolTimedOut
                | CauseKind::PostgresPoolClosed
                | CauseKind::PgServer
                | CauseKind::PgIo
                | CauseKind::PgTls
                | CauseKind::PgProtocol
                | CauseKind::PgDecode
                | CauseKind::PgWorkerCrashed
                | CauseKind::PgMigrate
                | CauseKind::PgSchema
                | CauseKind::PgOther
                | CauseKind::AuthWorker
                | CauseKind::HotBufferFull
                | CauseKind::RestartBacklog
                | CauseKind::RollupPending
                | CauseKind::RequestLimitReached
                | CauseKind::EngineSampleFailed
                | CauseKind::PreparationAllowanceExhausted => true,
            })
            .count();
        assert_eq!(variants, 37, "ALL lists every variant");

        let mut seen = std::collections::HashSet::new();
        for kind in CauseKind::ALL {
            let s = kind.as_str();
            assert!(!s.is_empty(), "{kind:?} has an empty literal");
            assert!(
                s.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
                    && !s.starts_with('_')
                    && !s.ends_with('_'),
                "{kind:?} renders as {s:?}, not snake_case"
            );
            assert!(seen.insert(s), "{s:?} is used twice");
        }
    }

    /// Representative typed sources land on their kinds, and a failure
    /// with nothing typed beneath it says so instead of guessing.
    #[test]
    fn cause_kind_reads_typed_sources() {
        use std::io::{Error as IoError, ErrorKind};
        let cases: Vec<(ServerError, CauseKind)> = vec![
            (
                ServerError::Engine(EngineError::Io(IoError::from(ErrorKind::NotFound))),
                CauseKind::IoNotFound,
            ),
            (
                ServerError::Engine(EngineError::Io(IoError::from(ErrorKind::StorageFull))),
                CauseKind::IoStorageFull,
            ),
            (
                ServerError::Engine(EngineError::Io(IoError::from(ErrorKind::WouldBlock))),
                CauseKind::IoOther,
            ),
            (
                ServerError::Engine(EngineError::Database(duckdb::Error::InvalidColumnName(
                    "zz_secret_column".into(),
                ))),
                CauseKind::DuckdbOther,
            ),
            (
                ServerError::Engine(EngineError::Database(duckdb::Error::DuckDBFailure(
                    duckdb::ffi::Error::new(1),
                    Some("zz_secret_sql".into()),
                ))),
                CauseKind::DuckdbFailure,
            ),
            (
                ServerError::Engine(EngineError::ResourceExhausted),
                CauseKind::DuckdbResourceExhausted,
            ),
            (
                ServerError::EngineSampleFailed,
                CauseKind::EngineSampleFailed,
            ),
            (
                ServerError::Store(StoreError::Unavailable(sqlx::Error::PoolTimedOut)),
                CauseKind::PostgresPoolTimedOut,
            ),
            (
                ServerError::Store(StoreError::Unavailable(sqlx::Error::Io(IoError::from(
                    ErrorKind::ConnectionRefused,
                )))),
                CauseKind::PgIo,
            ),
            (
                ServerError::from(fleet_auth::AuthError::Database(sqlx::Error::PoolClosed)),
                CauseKind::PostgresPoolClosed,
            ),
            (
                ServerError::from(fleet_auth::AuthError::Hash("zz_worker".into())),
                CauseKind::AuthWorker,
            ),
            (ServerError::Timeout, CauseKind::None),
            (ServerError::Engine(EngineError::Cancelled), CauseKind::None),
            (ServerError::Panicked("query worker"), CauseKind::None),
            (ServerError::CapacityNotStarted, CauseKind::None),
            (
                ServerError::Internal("zz_detail".into()),
                CauseKind::Unknown,
            ),
            (
                ServerError::HotBufferFull {
                    retry_after_secs: 10,
                },
                CauseKind::HotBufferFull,
            ),
            (
                ServerError::IngestBatchTooLarge {
                    requested: None,
                    ceiling: Charge::ZERO,
                },
                CauseKind::None,
            ),
        ];
        for (err, expected) in cases {
            assert_eq!(err.cause_kind(), expected, "wrong cause kind for {err:?}");
        }
    }

    /// The auth backend's 503 keeps the answer it always gave while the
    /// driver's kind rides along for the failure record, and the record
    /// stores the sentence the client reads.
    #[tokio::test]
    async fn an_auth_backend_failure_answers_the_same_redacted_503() {
        let err = ServerError::from(fleet_auth::AuthError::Database(sqlx::Error::PoolTimedOut));
        assert_eq!(err.error_class(), "service_unavailable");
        assert_eq!(err.safe_message(), "auth backend unavailable");
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_string(response).await;
        assert!(body.contains("auth backend unavailable"), "got: {body}");
        assert!(!body.to_lowercase().contains("pool"), "got: {body}");
    }

    /// A caught panic is a 500 with the same redacted body an internal
    /// error gets, and its own class.
    #[tokio::test]
    async fn a_panic_is_a_redacted_500_with_its_own_class() {
        let err = ServerError::Panicked("query worker");
        assert_eq!(err.error_class(), "panic");
        assert_eq!(err.safe_message(), "internal server error");
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = body_string(response).await;
        assert!(body.contains("\"internal_error\""), "got: {body}");
        assert!(body.contains("internal server error"), "got: {body}");
    }

    /// A joined task that panicked answers Panicked and drops the payload;
    /// one that was cancelled is an ordinary internal error.
    #[tokio::test]
    async fn from_join_splits_a_panic_from_a_cancellation() {
        let panicked = tokio::task::spawn(async { panic!("zz_join_payload_sentinel") })
            .await
            .expect_err("the task panics");
        let err = ServerError::from_join("probe", panicked);
        assert!(matches!(err, ServerError::Panicked("probe")), "got {err:?}");
        assert!(!format!("{err} {err:?}").contains("zz_join_payload_sentinel"));

        let pending = tokio::task::spawn(std::future::pending::<()>());
        pending.abort();
        let cancelled = pending.await.expect_err("the task is cancelled");
        let err = ServerError::from_join("probe", cancelled);
        assert!(matches!(err, ServerError::Internal(_)), "got {err:?}");
        assert_eq!(err.error_class(), "internal");
    }

    #[test]
    fn safe_message_redacts_database_errors() {
        let err = ServerError::Engine(EngineError::Database(duckdb::Error::InvalidColumnName(
            "secret_column".into(),
        )));
        assert_eq!(err.safe_message(), "query execution failed");
    }

    #[test]
    fn safe_message_preserves_parse_errors() {
        let err = ServerError::Engine(EngineError::Parse(vec![trawl_core::parser::ParseError {
            message: "bad syntax".into(),
            span: 0..3,
            label: None,
            hint: None,
        }]));
        assert!(err.safe_message().contains("bad syntax"));
    }

    /// A free-text 503 discloses its text as written, on the wire and in
    /// the records alike, so a query refused one says why. Provenance
    /// decides disclosure, not the characters (ADR-0040, amended
    /// 2026-11-08): the projection passes the text whole, whatever it
    /// holds, and keeping a backend's own words out is its constructors'
    /// rule. Backend outages have variants of their own, with fixed
    /// sentences.
    #[tokio::test]
    async fn a_free_text_503_discloses_what_its_constructor_wrote() {
        let sentence = "dashboard data not yet available";
        let err = ServerError::ServiceUnavailable(sentence.into());
        assert_eq!(err.safe_message(), sentence);
        let body: serde_json::Value =
            serde_json::from_str(&body_string(err.into_response()).await).unwrap();
        assert_eq!(body["error"]["message"], sentence);

        // Nothing filters it: a constructor that passed a DSN would
        // disclose the DSN, which is why none may.
        let unfiltered = ServerError::ServiceUnavailable("pg://user:pw@host down".into());
        assert_eq!(unfiltered.safe_message(), "pg://user:pw@host down");

        for (outage, sentence) in [
            (
                ServerError::Store(StoreError::Unavailable(sqlx::Error::Protocol(
                    "pg://user:pw@host down".into(),
                ))),
                "app-state store unavailable",
            ),
            (
                ServerError::from(fleet_auth::AuthError::Database(sqlx::Error::Protocol(
                    "pg://user:pw@host down".into(),
                ))),
                "auth backend unavailable",
            ),
        ] {
            assert_eq!(outage.safe_message(), sentence);
        }
    }

    /// The pre-start capacity refusal is a typed variant (ADR-0040,
    /// amended 2026-11-08), and it answers what the string-matched 503 it
    /// replaced answered: 503 `service_unavailable` with the fixed
    /// sentence and no `Retry-After`, the class `service_unavailable`, and
    /// no cause beneath it. Read back from the error, its timing outcome is
    /// the capacity refusal its producers book.
    #[tokio::test]
    async fn typed_capacity_refusal_preserves_protocol() {
        use crate::query_timing::Outcome;

        let err = ServerError::CapacityNotStarted;
        assert_eq!(err.error_class(), "service_unavailable");
        assert_eq!(err.cause_kind(), CauseKind::None);
        assert_eq!(err.cause_kind().as_str(), "none");
        assert_eq!(err.code(), trawl_api::ErrorCode::ServiceUnavailable);
        assert_eq!(err.safe_message(), CAPACITY_NOT_STARTED);
        assert_eq!(
            err.to_string(),
            "service unavailable: server at capacity: the query was not started"
        );
        // The pool, the expired `from saved` lookup and capture planning
        // book `CapacityRefused` where they refuse. A cancel by id that
        // ends a request in one of the route's own waits books it only
        // through `of_error` (ADR-0024, Amendment 2026-11-09).
        assert_eq!(Outcome::of_error(&err), Outcome::CapacityRefused);

        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        for header in [
            axum::http::header::RETRY_AFTER,
            axum::http::header::CACHE_CONTROL,
        ] {
            assert!(response.headers().get(&header).is_none(), "{header}");
        }
        let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "error": {
                    "code": "service_unavailable",
                    "message": CAPACITY_NOT_STARTED,
                }
            })
        );
    }

    #[test]
    fn safe_message_redacts_internal_errors() {
        let err = ServerError::Internal("db connection string leaked".into());
        assert_eq!(err.safe_message(), "internal server error");

        // An evidence refusal is a domain fact, not an internal error: its
        // safe message is its whole text, which names the budget and calls
        // the count what it is.
        for (scope, budget) in [
            (
                EvidenceScope::Investigation,
                "this investigation's evidence budget",
            ),
            (
                EvidenceScope::Installation,
                "the installation's evidence budget",
            ),
        ] {
            let err = ServerError::EvidenceBudgetExceeded {
                scope,
                used: 10,
                limit: 20,
            };
            let message = err.safe_message();
            assert_eq!(
                message,
                format!(
                    "keeping this result would exceed {budget}: 10 of 20 bytes are counted \
                     against it, kept evidence and reservations together; evidence frees as it \
                     ages out, or when an operator with server_manage deletes an investigation"
                )
            );
            assert!(!message.contains("already kept"), "{message}");
            assert_eq!(err.error_class(), "evidence_budget_exceeded");
            assert_eq!(err.into_response().status(), StatusCode::CONFLICT);
        }
    }

    /// A conflict is the state, not the request: 409 with the message
    /// intact, because it tells the operator what to do about it.
    #[tokio::test]
    async fn conflict_maps_to_409_with_its_message() {
        let err = ServerError::Conflict("a repin owns the data root".into());
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = body_string(response).await;
        assert!(body.contains("a repin owns the data root"), "got: {body}");
    }

    /// An unknown purge commit is a store error like any other as far as
    /// telemetry is concerned (the class is content-free and stays
    /// `store`), but on the wire it is a 503 that says what happened: the
    /// operator has to go and look, and a redacted "store unavailable"
    /// would not tell them to.
    #[tokio::test]
    async fn purge_commit_unknown_is_a_store_class_503_with_its_message() {
        let err = ServerError::Store(StoreError::PurgeCommitUnknown);
        assert_eq!(err.error_class(), "store");
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_string(response).await;
        assert!(body.contains("unknown"), "got: {body}");
    }

    #[test]
    fn service_unavailable_maps_to_503() {
        let err = ServerError::ServiceUnavailable("not ready".into());
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    /// A query refused for its regex memory (ADR-0063) answers the 503
    /// contract a query over the `DuckDB` budgets answers: the code and the
    /// fixed sentence, no `Retry-After`, and the class `service_unavailable`.
    /// Its cause kind names the regex bound that refused it, whether stream
    /// setup or the batch tail raised it. Nothing of the pattern reaches
    /// the wire, the tracker or the record.
    #[tokio::test]
    async fn a_regex_memory_refusal_is_the_resource_503_with_its_cause() {
        use trawl_core::pattern::RegexRefusal;
        for (refusal, kind, literal) in [
            (
                RegexRefusal::Budget,
                CauseKind::RegexBudgetExhausted,
                "regex_budget_exhausted",
            ),
            (
                RegexRefusal::Reservation,
                CauseKind::RegexReservationExhausted,
                "regex_reservation_exhausted",
            ),
        ] {
            for err in [
                ServerError::RegexResources(refusal),
                ServerError::Engine(EngineError::RegexResources(refusal)),
            ] {
                let label = format!("{err:?}");
                assert_eq!(err.error_class(), "service_unavailable", "{label}");
                assert_eq!(err.cause_kind(), kind, "{label}");
                assert_eq!(kind.as_str(), literal);
                assert_eq!(err.safe_message(), QUERY_RESOURCE_EXHAUSTED, "{label}");
                assert_eq!(err.code(), trawl_api::ErrorCode::QueryResourceExhausted);
                let response = err.into_response();
                assert_eq!(
                    response.status(),
                    StatusCode::SERVICE_UNAVAILABLE,
                    "{label}"
                );
                assert!(
                    response
                        .headers()
                        .get(axum::http::header::RETRY_AFTER)
                        .is_none(),
                    "{label}"
                );
                let body: serde_json::Value =
                    serde_json::from_str(&body_string(response).await).unwrap();
                assert_eq!(
                    body,
                    serde_json::json!({
                        "error": {
                            "code": "query_resource_exhausted",
                            "message": QUERY_RESOURCE_EXHAUSTED,
                        }
                    }),
                    "{label}"
                );
            }
        }
    }

    /// A query refused a preparation slot is the resource 503 (ADR-0063):
    /// `query_resource_exhausted`, the fixed allowance sentence, no
    /// `Retry-After`, the class `service_unavailable` and its own cause.
    #[tokio::test]
    async fn a_preparation_refusal_is_the_resource_503_with_its_cause() {
        let err = ServerError::PreparationUnavailable;
        assert_eq!(err.error_class(), "service_unavailable");
        assert_eq!(err.cause_kind(), CauseKind::PreparationAllowanceExhausted);
        assert_eq!(err.cause_kind().as_str(), "preparation_allowance_exhausted");
        assert_eq!(err.safe_message(), PREPARATION_ALLOWANCE_EXHAUSTED);
        assert_eq!(err.code(), trawl_api::ErrorCode::QueryResourceExhausted);
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            response
                .headers()
                .get(axum::http::header::RETRY_AFTER)
                .is_none()
        );
        let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "error": {
                    "code": "query_resource_exhausted",
                    "message": PREPARATION_ALLOWANCE_EXHAUSTED,
                }
            })
        );
    }

    /// A full hot buffer is a 503 whose `Retry-After` is the variant's
    /// interval, classed as a refusal to serve and named by its cause.
    #[tokio::test]
    async fn hot_buffer_full_is_a_503_with_retry_after() {
        let err = ServerError::HotBufferFull {
            retry_after_secs: 37,
        };
        assert_eq!(err.error_class(), "service_unavailable");
        assert_eq!(err.cause_kind(), CauseKind::HotBufferFull);
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response.headers().get(axum::http::header::RETRY_AFTER),
            Some(&axum::http::HeaderValue::from_static("37"))
        );
        let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(body["error"]["code"], "hot_buffer_full");
    }

    /// A corpus that is not settled refuses reads with one 503 per reason
    /// (ADR-0041): classed like any refusal to serve, the reason as its
    /// cause kind, and a fixed sentence that names no count, path or
    /// marker. No `Retry-After`: nothing tells the server when a backlog
    /// drains or a rollup finishes.
    #[tokio::test]
    async fn corpus_recovering_is_a_503_naming_its_reason_without_retry_after() {
        for reason in CorpusUnsettled::ALL {
            // Exhaustive on purpose: a new reason needs its own row here.
            let (kind, message) = match reason {
                CorpusUnsettled::RestartBacklog => (
                    CauseKind::RestartBacklog,
                    "Search is unavailable while the server finishes loading data from before \
                     its restart.",
                ),
                CorpusUnsettled::RollupPending => (
                    CauseKind::RollupPending,
                    "Search is unavailable while the server finishes an interrupted storage \
                     rollup.",
                ),
            };
            let err = ServerError::CorpusRecovering(reason);
            assert_eq!(err.error_class(), "service_unavailable");
            assert_eq!(err.cause_kind(), kind);
            assert_eq!(kind.as_str(), reason.label());
            // The tracker and a run row get the same sentence the wire does.
            assert_eq!(err.safe_message(), message);
            let response = err.into_response();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert!(
                response
                    .headers()
                    .get(axum::http::header::RETRY_AFTER)
                    .is_none(),
                "{reason:?} must not send Retry-After"
            );
            let body: serde_json::Value =
                serde_json::from_str(&body_string(response).await).unwrap();
            assert_eq!(body["error"]["code"], "corpus_recovering");
            assert_eq!(body["error"]["message"], message);
            assert!(body["error"].get("details").is_none());
        }
    }

    /// A hot snapshot that cannot be built refuses the read instead of
    /// answering without the newest events (ADR-0041): a 503 classed like
    /// any refusal to serve, its cause kind read off the I/O error, and a
    /// fixed body that quotes nothing the error carried.
    #[tokio::test]
    async fn a_hot_snapshot_failure_is_a_redacted_503_with_its_io_kind() {
        use std::io::{Error as IoError, ErrorKind};
        for (io_kind, expected) in [
            (ErrorKind::InvalidData, CauseKind::IoInvalidData),
            (ErrorKind::StorageFull, CauseKind::IoStorageFull),
            (ErrorKind::WouldBlock, CauseKind::IoOther),
        ] {
            let err = ServerError::HotSnapshot(IoError::new(
                io_kind,
                "zz_secret_value at /var/lib/trawl/zz_path",
            ));
            assert_eq!(err.error_class(), "service_unavailable");
            assert_eq!(err.cause_kind(), expected);
            assert!(!err.safe_message().contains("zz_"), "{err:?}");
            let response = err.into_response();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert!(
                response
                    .headers()
                    .get(axum::http::header::RETRY_AFTER)
                    .is_none()
            );
            let body = body_string(response).await;
            assert!(!body.contains("zz_"), "got: {body}");
            let body: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(body["error"]["code"], "service_unavailable");
        }
    }

    /// A data root whose envs cannot be listed refuses the compare instead
    /// of reporting retention reach over none: a 503 classed like any
    /// refusal to serve, its cause kind read off the I/O error, and a fixed
    /// body that quotes nothing the error carried.
    #[tokio::test]
    async fn an_env_listing_failure_is_a_redacted_503_with_its_io_kind() {
        use std::io::{Error as IoError, ErrorKind};
        for (io_kind, expected) in [
            (ErrorKind::PermissionDenied, CauseKind::IoPermissionDenied),
            (ErrorKind::NotADirectory, CauseKind::IoOther),
        ] {
            let err = ServerError::EnvListing(IoError::new(io_kind, "/var/lib/trawl/zz_path"));
            assert_eq!(err.error_class(), "service_unavailable");
            assert_eq!(err.cause_kind(), expected);
            assert_eq!(err.safe_message(), "environment listing failed");
            let response = err.into_response();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert!(
                response
                    .headers()
                    .get(axum::http::header::RETRY_AFTER)
                    .is_none()
            );
            let body = body_string(response).await;
            assert!(!body.contains("zz_"), "got: {body}");
            let body: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(body["error"]["code"], "service_unavailable");
            assert_eq!(body["error"]["message"], "environment listing failed");
        }
    }

    /// A query that exhausted the query database's budgets is a 503
    /// `query_resource_exhausted` with the fixed sentence and no
    /// `Retry-After`, everywhere it is told (ADR-0056).
    #[tokio::test]
    async fn an_exhausted_query_budget_is_a_fixed_503() {
        let err = ServerError::Engine(EngineError::ResourceExhausted);
        assert_eq!(err.error_class(), "service_unavailable");
        assert_eq!(err.cause_kind(), CauseKind::DuckdbResourceExhausted);
        assert_eq!(err.cause_kind().as_str(), "duckdb_resource_exhausted");
        assert_eq!(err.safe_message(), QUERY_RESOURCE_EXHAUSTED);
        assert_eq!(
            EngineError::ResourceExhausted.to_string(),
            QUERY_RESOURCE_EXHAUSTED,
            "the engine's own rendering says the same"
        );
        assert_eq!(
            QUERY_RESOURCE_EXHAUSTED,
            "query could not complete with the available query resources; narrow the time \
             range or simplify the query"
        );
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            response
                .headers()
                .get(axum::http::header::RETRY_AFTER)
                .is_none()
        );
        let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(body["error"]["code"], "query_resource_exhausted");
        assert_eq!(body["error"]["message"], QUERY_RESOURCE_EXHAUSTED);
    }

    /// An oversized ingest request is a 413 with no `Retry-After`, and its
    /// message gives the sender the numbers to split by.
    #[tokio::test]
    async fn ingest_batch_too_large_is_a_413_without_retry_after() {
        let ceiling = Charge {
            events: 60,
            bytes: 960,
        };
        let err = ServerError::IngestBatchTooLarge {
            requested: Some(Charge {
                events: 61,
                bytes: 700,
            }),
            ceiling,
        };
        assert_eq!(err.cause_kind(), CauseKind::None);
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert!(
            response
                .headers()
                .get(axum::http::header::RETRY_AFTER)
                .is_none()
        );
        let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(body["error"]["code"], "ingest_batch_too_large");
        let message = body["error"]["message"].as_str().unwrap();
        for number in ["61", "700", "60", "960"] {
            assert!(message.contains(number), "{number} missing: {message}");
        }

        let unparsed = ServerError::IngestBatchTooLarge {
            requested: None,
            ceiling: Charge::ZERO,
        }
        .into_response();
        assert_eq!(unparsed.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    /// The ingest preview's two refusals: fixed text, a class and no cause
    /// beneath it.
    #[tokio::test]
    async fn preview_refusals_are_413_and_415_with_their_codes() {
        let too_large = ServerError::PreviewTooLarge { limit: 500 };
        assert_eq!(too_large.error_class(), "preview_too_large");
        assert_eq!(too_large.cause_kind(), CauseKind::None);
        let response = too_large.into_response();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(body["error"]["code"], "preview_too_large");
        assert!(body["error"]["message"].as_str().unwrap().contains("500"));

        let encoding = ServerError::UnsupportedEncoding;
        assert_eq!(encoding.error_class(), "unsupported_encoding");
        assert_eq!(encoding.cause_kind(), CauseKind::None);
        let response = encoding.into_response();
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(body["error"]["code"], "unsupported_encoding");
    }

    /// A capture refused for its source budget (ADR-0057): 400 with its own
    /// code and no `Retry-After`, a message giving both counts and the fix,
    /// and no cause beneath it. A window's fix names the service filter; a
    /// stored run's does not, since a filter cannot shrink it. Neither
    /// message holds a path, a service name or query text.
    #[tokio::test]
    async fn a_capture_source_refusal_is_a_400_naming_both_counts() {
        const HINT: &str = "pin the services with service=a or service=a,b";
        let cases = [
            (
                CaptureSourceKind::Window,
                "the window selects 2147483648 bytes of stored data, above the capture limit of \
                 1073741824 bytes; narrow the window, or pin the services with service=a or \
                 service=a,b",
            ),
            (
                CaptureSourceKind::StoredRun,
                "the window selects 2147483648 bytes of stored data, above the capture limit of \
                 1073741824 bytes; narrow the window",
            ),
        ];
        for (kind, expected) in cases {
            let too_large = ServerError::CaptureSourceTooLarge {
                selected_bytes: 2_147_483_648,
                max_bytes: 1_073_741_824,
                kind,
            };
            assert_eq!(too_large.error_class(), "capture_source_too_large");
            assert_eq!(too_large.cause_kind(), CauseKind::None);
            assert_eq!(too_large.safe_message(), too_large.to_string());
            let response = too_large.into_response();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{kind:?}");
            assert!(
                response
                    .headers()
                    .get(axum::http::header::RETRY_AFTER)
                    .is_none(),
                "{kind:?}"
            );
            let body: serde_json::Value =
                serde_json::from_str(&body_string(response).await).unwrap();
            assert_eq!(
                body["error"]["code"], "capture_source_too_large",
                "{kind:?}"
            );
            let message = body["error"]["message"].as_str().unwrap();
            assert_eq!(message, expected, "{kind:?}");
            assert!(message.contains("2147483648"), "{kind:?}");
            assert!(message.contains("1073741824"), "{kind:?}");
            assert_eq!(
                message.contains(HINT),
                kind == CaptureSourceKind::Window,
                "{kind:?}"
            );
            assert!(
                !message.contains('/') && !message.contains('\\'),
                "{kind:?} carries no path separator"
            );
            for query_text in ["last=", "from saved", "SELECT", "parquet"] {
                assert!(
                    !message.contains(query_text),
                    "{kind:?} carries {query_text}"
                );
            }
        }
    }

    /// Every investigation answer (ADR-0059) is a 409 with its own code and
    /// class, no `Retry-After` and no cause, and a fixed message that names
    /// no path and no SQL. Only `sequence_conflict` carries `conflict`, and
    /// the store's own conflicts keep their `bad_request` code.
    #[tokio::test]
    async fn investigation_answers_are_409s_with_their_own_codes() {
        let cases = [
            (
                ServerError::SequenceConflict {
                    expected: 4,
                    current: 6,
                },
                "sequence_conflict",
            ),
            (ServerError::InvestigationExists, "investigation_exists"),
            (ServerError::InvestigationClosed, "investigation_closed"),
            (
                ServerError::EvidenceBudgetExceeded {
                    scope: EvidenceScope::Installation,
                    used: 1_073_000_000,
                    limit: 1_073_741_824,
                },
                "evidence_budget_exceeded",
            ),
        ];
        for (error, code) in cases {
            assert_eq!(error.error_class(), code);
            assert_eq!(error.cause_kind(), CauseKind::None);
            assert_eq!(error.safe_message(), error.to_string());
            let message = error.to_string();
            for leak in ['/', '\\'] {
                assert!(!message.contains(leak), "{message}");
            }
            assert!(!message.to_lowercase().contains("select"), "{message}");
            let response = error.into_response();
            assert_eq!(response.status(), StatusCode::CONFLICT, "{code}");
            assert!(
                response
                    .headers()
                    .get(axum::http::header::RETRY_AFTER)
                    .is_none(),
                "{code}"
            );
            let body: serde_json::Value =
                serde_json::from_str(&body_string(response).await).unwrap();
            assert_eq!(body["error"]["code"], code);
            assert_eq!(body["error"]["message"], message);
            if code == "sequence_conflict" {
                assert_eq!(
                    body["error"]["conflict"],
                    serde_json::json!({"expected": 4, "current": 6})
                );
                assert!(message.contains('4') && message.contains('6'), "{message}");
            } else {
                assert!(body["error"].get("conflict").is_none(), "{body}");
            }
        }
        let budget = ServerError::EvidenceBudgetExceeded {
            scope: EvidenceScope::Investigation,
            used: 10,
            limit: 20,
        }
        .to_string();
        assert!(budget.contains("10 of 20 bytes"), "{budget}");
        assert!(budget.contains("server_manage"), "{budget}");

        let store_conflict = ServerError::Conflict("a repin owns the data root".into());
        let response = store_conflict.into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(body["error"]["code"], "bad_request");
        assert!(body["error"].get("conflict").is_none(), "{body}");
    }

    /// Every capture and captured-result answer (ADR-0057): its status, its
    /// code, its class, `Retry-After` exactly on the two 429s, and a fixed
    /// message that names no path.
    #[tokio::test]
    async fn capture_and_result_answers_map_to_their_statuses_and_codes() {
        use crate::results::bounds::BoundsError;
        use crate::results::service::{CaptureOverflow, GoneReason};

        let cases = [
            (
                ServerError::CaptureBounds(BoundsError::LatestAlone),
                StatusCode::BAD_REQUEST,
                "bad_request",
                "capture_bounds",
                None,
            ),
            (
                ServerError::CaptureTooLarge(CaptureOverflow::Rows {
                    rows: 6000,
                    max_rows: 5000,
                }),
                StatusCode::BAD_REQUEST,
                "result_too_large",
                "result_too_large",
                None,
            ),
            (
                ServerError::CaptureBusy {
                    retry_after_secs: 7,
                },
                StatusCode::TOO_MANY_REQUESTS,
                "capture_busy",
                "capture_busy",
                Some("7"),
            ),
            (
                ServerError::CaptureQuotaExceeded {
                    retry_after_secs: 900,
                },
                StatusCode::TOO_MANY_REQUESTS,
                "capture_quota_exceeded",
                "capture_quota_exceeded",
                Some("900"),
            ),
            (
                ServerError::ResultUnavailable,
                StatusCode::CONFLICT,
                "result_unavailable",
                "result_unavailable",
                None,
            ),
            (
                ServerError::ResultCorrupt,
                StatusCode::INTERNAL_SERVER_ERROR,
                "result_corrupt",
                "result_corrupt",
                None,
            ),
        ];
        let gone = [
            GoneReason::Expired,
            GoneReason::Pruned,
            GoneReason::RetentionEnded,
            GoneReason::InvestigationDeleted,
        ]
        .map(|reason| {
            (
                ServerError::ResultGone { reason },
                StatusCode::GONE,
                "result_gone",
                "result_gone",
                None,
            )
        });
        for (error, status, code, class, retry_after) in cases.into_iter().chain(gone) {
            assert_eq!(error.error_class(), class);
            assert_eq!(error.cause_kind(), CauseKind::None);
            assert_eq!(error.safe_message(), error.to_string());
            let message = error.to_string();
            assert!(!message.contains('/'), "{message}");
            let response = error.into_response();
            assert_eq!(response.status(), status, "{code}");
            assert_eq!(
                response
                    .headers()
                    .get(axum::http::header::RETRY_AFTER)
                    .map(|v| v.to_str().unwrap().to_owned())
                    .as_deref(),
                retry_after,
                "{code}"
            );
            let body: serde_json::Value =
                serde_json::from_str(&body_string(response).await).unwrap();
            assert_eq!(body["error"]["code"], code);
            assert_eq!(body["error"]["message"], message);
        }
        assert!(
            ServerError::CaptureTooLarge(CaptureOverflow::WireBytes {
                max_wire_bytes: 4_194_304,
            })
            .to_string()
            .ends_with("aggregate further or narrow the window")
        );
    }

    /// A body-limit refusal: 413 with its own code, a message naming the
    /// setting and the number it enforced, and no cause beneath it.
    #[tokio::test]
    async fn a_body_limit_refusal_is_413_naming_the_setting_and_limit() {
        let too_large = ServerError::RequestTooLarge {
            setting: "[server] max_request_body_bytes",
            limit: 131_072,
        };
        assert_eq!(too_large.error_class(), "request_too_large");
        assert_eq!(too_large.cause_kind(), CauseKind::None);
        assert_eq!(too_large.safe_message(), too_large.to_string());
        let response = too_large.into_response();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(body["error"]["code"], "request_too_large");
        assert_eq!(
            body["error"]["message"],
            "request body exceeds [server] max_request_body_bytes (131072 bytes)"
        );
    }

    /// A request-limit refusal (ADR-0054): 503 with its own code and the
    /// fixed sentence for its allowance, classed like every refusal to
    /// serve, `Cache-Control: no-store` and no `Retry-After`. Every
    /// allowance is covered: the match below is exhaustive, and `ALL` is
    /// pinned to the three of them.
    #[tokio::test]
    async fn a_request_limit_refusal_is_a_no_store_503_without_retry_after() {
        assert_eq!(
            Allowance::ALL,
            [Allowance::Regular, Allowance::Probe, Allowance::Control]
        );
        for allowance in Allowance::ALL {
            let message = match allowance {
                Allowance::Regular => REQUEST_LIMIT_REACHED,
                Allowance::Probe => PROBE_ALLOWANCE_REACHED,
                Allowance::Control => CONTROL_ALLOWANCE_REACHED,
            };
            let err = ServerError::RequestLimitReached(allowance);
            assert_eq!(err.error_class(), "service_unavailable");
            assert_eq!(err.cause_kind(), CauseKind::RequestLimitReached);
            assert_eq!(err.cause_kind().as_str(), "request_limit_reached");
            assert_eq!(err.to_string(), message);
            assert_eq!(err.safe_message(), message);
            let response = err.into_response();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            let headers = response.headers();
            assert!(
                headers.get(axum::http::header::RETRY_AFTER).is_none(),
                "{allowance:?} must not send Retry-After"
            );
            assert_eq!(
                headers.get(axum::http::header::CACHE_CONTROL),
                Some(&axum::http::HeaderValue::from_static("no-store"))
            );
            let body: serde_json::Value =
                serde_json::from_str(&body_string(response).await).unwrap();
            assert_eq!(body["error"]["code"], "request_limit_reached");
            assert_eq!(body["error"]["message"], message);
            assert!(body["error"].get("details").is_none());
        }
    }

    /// The three sentences, word for word. None carries a digit, so no
    /// configured value can be in one. The regular one names the setting;
    /// the probe and control ones name their allowance and not the setting.
    #[test]
    fn the_request_limit_messages_are_fixed_and_carry_no_value() {
        assert_eq!(
            Allowance::ALL,
            [Allowance::Regular, Allowance::Probe, Allowance::Control]
        );
        for allowance in Allowance::ALL {
            let expected = match allowance {
                Allowance::Regular => {
                    "trawld is at its HTTP request limit ([server] max_concurrent_requests); the \
                     request was not processed; retry later with backoff"
                }
                Allowance::Probe => {
                    "trawld is at its HTTP probe allowance; the request was not processed; retry \
                     later with backoff"
                }
                Allowance::Control => {
                    "trawld is at its HTTP control allowance; the request was not processed; retry \
                     later with backoff"
                }
            };
            let message = request_limit_message(allowance);
            assert_eq!(message, expected, "{allowance:?}");
            assert!(
                !message.bytes().any(|b| b.is_ascii_digit()),
                "a digit in {message:?}"
            );
        }
        assert_eq!(
            request_limit_message(Allowance::Regular),
            REQUEST_LIMIT_REACHED
        );
        assert_eq!(
            request_limit_message(Allowance::Probe),
            PROBE_ALLOWANCE_REACHED
        );
        assert_eq!(
            request_limit_message(Allowance::Control),
            CONTROL_ALLOWANCE_REACHED
        );
        assert!(!PROBE_ALLOWANCE_REACHED.contains("max_concurrent_requests"));
        assert!(!CONTROL_ALLOWANCE_REACHED.contains("max_concurrent_requests"));
    }

    /// An engine refusal is the caller's mistake, not the server's: 400,
    /// with the sentence intact.
    ///
    /// The engine raises it after proving the query wrong against
    /// `DuckDB` — a `timechart` bucketing a column that is not a
    /// timestamp — so the text is trawl-authored and quotes only the
    /// caller's own tokens. Redacting it would leave a 400 that says
    /// nothing, and classing it 500 would blame the server for a query
    /// no source could have answered.
    #[tokio::test]
    async fn engine_refusal_is_a_bad_request_with_its_message() {
        let message = "timechart on 'hostname' is not a timestamp: VARCHAR";
        let err = ServerError::Engine(EngineError::Refused {
            message: message.to_string(),
        });
        assert_eq!(err.error_class(), "refused");
        assert_eq!(
            err.safe_message(),
            message,
            "there is nothing in it to redact, and the tracker's history row \
             would otherwise say a query failed without saying why"
        );

        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_string(response).await;
        assert!(body.contains(message), "got: {body}");
        assert!(
            body.contains("validation_error"),
            "the same code an emitter refusal carries: {body}"
        );
    }

    // -- StoreError → HTTP table (ADR-0004: 503 / 409 / 404 / 400) ─────────

    /// Render a response body to a string for envelope assertions.
    async fn body_string(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn store_error_http_table() {
        let cases: Vec<(ServerError, StatusCode)> = vec![
            (
                ServerError::Store(StoreError::Unavailable(sqlx::Error::PoolTimedOut)),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                ServerError::Store(StoreError::LockHeld),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                ServerError::Store(StoreError::DuplicateName { name: "x".into() }),
                StatusCode::CONFLICT,
            ),
            (
                ServerError::Store(StoreError::ScheduleExists { saved_query_id: 1 }),
                StatusCode::CONFLICT,
            ),
            (
                ServerError::Store(StoreError::NotFound {
                    id: 9,
                    resource: "saved query",
                }),
                StatusCode::NOT_FOUND,
            ),
            (
                ServerError::Store(StoreError::Validation("bad".into())),
                StatusCode::BAD_REQUEST,
            ),
            (
                ServerError::Store(StoreError::InvalidInterval { input: "5x".into() }),
                StatusCode::BAD_REQUEST,
            ),
            (
                ServerError::Store(StoreError::IntervalTooShort { secs: 3 }),
                StatusCode::BAD_REQUEST,
            ),
            (
                ServerError::Store(StoreError::InvalidName { name: "a b".into() }),
                StatusCode::BAD_REQUEST,
            ),
            (
                ServerError::Store(StoreError::InvestigationNotFound),
                StatusCode::NOT_FOUND,
            ),
            (
                ServerError::Store(StoreError::SequenceConflict {
                    expected: 1,
                    current: 2,
                }),
                StatusCode::CONFLICT,
            ),
            (
                ServerError::Store(StoreError::InvestigationClosed),
                StatusCode::CONFLICT,
            ),
        ];
        for (err, expected) in cases {
            let desc = format!("{err:?}");
            let response = err.into_response();
            assert_eq!(response.status(), expected, "wrong status for {desc}");
        }
    }

    /// The store's investigation conflicts convert to the variants whose
    /// codes the wire names (ADR-0059); every other store error stays a
    /// store error.
    #[tokio::test]
    async fn store_investigation_conflicts_convert_to_their_own_codes() {
        let stale = ServerError::from(StoreError::SequenceConflict {
            expected: 4,
            current: 6,
        });
        assert!(
            matches!(
                stale,
                ServerError::SequenceConflict {
                    expected: 4,
                    current: 6
                }
            ),
            "{stale:?}"
        );
        let body: serde_json::Value =
            serde_json::from_str(&body_string(stale.into_response()).await).unwrap();
        assert_eq!(body["error"]["code"], "sequence_conflict");
        assert_eq!(
            body["error"]["conflict"],
            serde_json::json!({"expected": 4, "current": 6})
        );

        let closed = ServerError::from(StoreError::InvestigationClosed);
        assert!(
            matches!(closed, ServerError::InvestigationClosed),
            "{closed:?}"
        );
        let missing = ServerError::from(StoreError::InvestigationNotFound);
        assert!(
            matches!(
                missing,
                ServerError::Store(StoreError::InvestigationNotFound)
            ),
            "{missing:?}"
        );
        let body = body_string(missing.into_response()).await;
        assert!(body.contains("\"not_found\""), "got: {body}");
    }

    /// Postgres diagnostics must never reach the wire: the 503 body is the
    /// fixed redacted envelope regardless of the underlying sqlx error.
    #[tokio::test]
    async fn store_unavailable_body_is_redacted() {
        let err = ServerError::Store(StoreError::Unavailable(sqlx::Error::PoolTimedOut));
        let response = err.into_response();
        let body = body_string(response).await;
        assert!(body.contains("app-state store unavailable"), "got: {body}");
        assert!(
            !body.to_lowercase().contains("pool") && !body.to_lowercase().contains("postgres"),
            "pg diagnostics leaked: {body}"
        );
    }

    #[test]
    fn safe_message_redacts_store_backend_errors() {
        let err = ServerError::Store(StoreError::Unavailable(sqlx::Error::PoolTimedOut));
        assert_eq!(err.safe_message(), "app-state store unavailable");
    }

    /// ADR-0018 ruling 7: the conflict is the operator's to resolve, so
    /// the refusal is a 400 that keeps both sides on the wire.
    #[tokio::test]
    async fn a_window_policy_refusal_is_a_400_naming_both_sides() {
        let err = ServerError::from(WindowPolicyError::TimeClause {
            window: crate::report_window::ScheduleWindow::SinceLast,
            clause: trawl_core::ast::TimeClause::Last,
        });
        assert_eq!(err.error_class(), "window_policy");
        let message = err.safe_message();
        assert!(
            message.contains("since_last") && message.contains("last="),
            "got: {message}"
        );
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_string(response).await;
        assert!(body.contains("since_last"), "got: {body}");
    }

    /// A claim failure keeps the wire treatment its leaf already has: no
    /// second, differently-mapped route to the same answer.
    #[test]
    fn a_claim_failure_maps_to_its_leafs_own_variant() {
        use crate::store::DueClaimError;
        assert!(matches!(
            ServerError::from(DueClaimError::Store(StoreError::Unavailable(
                sqlx::Error::PoolTimedOut
            ))),
            ServerError::Store(StoreError::Unavailable(_))
        ));
        assert!(matches!(
            ServerError::from(DueClaimError::Plan(PlanError::Arithmetic)),
            ServerError::WindowPlan(PlanError::Arithmetic)
        ));
        assert!(matches!(
            ServerError::from(DueClaimError::Policy(WindowPolicyError::FromSaved {
                window: crate::report_window::ScheduleWindow::SinceLast,
            })),
            ServerError::WindowPolicy(_)
        ));
        assert!(matches!(
            ServerError::from(DueClaimError::Materialize(
                MaterializeError::SourceFromSaved
            )),
            ServerError::WindowMaterialize(_)
        ));
    }

    /// Planning and materialization read a schedule row and the saved DSL,
    /// never the request, so their failures are 500s and the DSL text they
    /// can quote stays off the wire.
    #[tokio::test]
    async fn window_plan_and_materialize_failures_are_redacted_500s() {
        let plan = ServerError::from(PlanError::Arithmetic);
        assert_eq!(plan.error_class(), "window_plan");
        assert_eq!(plan.safe_message(), "report window could not be resolved");
        assert_eq!(
            plan.into_response().status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );

        let materialize = ServerError::from(MaterializeError::SourceUnparseable {
            message: "unexpected 'zz_secret_token'".to_owned(),
        });
        assert_eq!(materialize.error_class(), "window_materialize");
        assert_eq!(
            materialize.safe_message(),
            "report window could not be resolved"
        );
        let response = materialize.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = body_string(response).await;
        assert!(!body.contains("zz_secret_token"), "got: {body}");
    }

    // -- One public message (ADR-0040, amended 2026-11-08) ────────────────

    /// The status, headers and decoded body an error answers with.
    async fn answer(error: ServerError) -> (StatusCode, axum::http::HeaderMap, serde_json::Value) {
        let response = error.into_response();
        let status = response.status();
        let headers = response.headers().clone();
        let body = serde_json::from_str(&body_string(response).await).unwrap();
        (status, headers, body)
    }

    /// Writes the disclosure witness and its sample table from one list of
    /// rows, a pattern and a sample per variant. The witness matches with
    /// no wildcard at the `ServerError`, `EngineError` or `StoreError`
    /// level, so a new variant fails to compile until its row exists, and
    /// the row is what puts its sample in the table.
    macro_rules! disclosure_rows {
        (
            server { $($pat:pat => $sample:expr,)+ }
            engine { $($engine_pat:pat => $engine_sample:expr,)+ }
            store { $($store_pat:pat => $store_sample:expr,)+ }
        ) => {
            /// The row `error` belongs to, through `Engine` and `Store`.
            #[deny(clippy::wildcard_enum_match_arm)]
            fn variant_name(error: &ServerError) -> &'static str {
                match error {
                    ServerError::Engine(engine) => match engine {
                        $($engine_pat => stringify!($engine_pat),)+
                    },
                    ServerError::Store(store) => match store {
                        $($store_pat => stringify!($store_pat),)+
                    },
                    $($pat => stringify!($pat),)+
                }
            }

            /// One sample of every variant, with the row it was written in.
            fn every_variant() -> Vec<(&'static str, ServerError)> {
                vec![
                    $((stringify!($pat), $sample),)+
                    $((stringify!($engine_pat), ServerError::Engine($engine_sample)),)+
                    $((stringify!($store_pat), ServerError::Store($store_sample)),)+
                ]
            }
        };
    }

    disclosure_rows! {
        server {
            ServerError::Unauthorized(_) => {
                ServerError::Unauthorized("authentication failed".into())
            },
            ServerError::Forbidden(_) => {
                ServerError::Forbidden("no trawl grant for this key".into())
            },
            ServerError::BadRequest(_) => {
                ServerError::BadRequest("invalid service name: a/b".into())
            },
            ServerError::NotFound(_) => ServerError::NotFound("field not pinned: status".into()),
            ServerError::Conflict(_) => ServerError::Conflict("a repin owns the data root".into()),
            ServerError::WindowPolicy(_) => {
                ServerError::WindowPolicy(WindowPolicyError::TimeClause {
                    window: crate::report_window::ScheduleWindow::SinceLast,
                    clause: trawl_core::ast::TimeClause::Last,
                })
            },
            ServerError::WindowPlan(_) => ServerError::WindowPlan(PlanError::InvalidConfig),
            ServerError::WindowMaterialize(_) => {
                ServerError::WindowMaterialize(MaterializeError::BodyChanged)
            },
            ServerError::Timeout => ServerError::Timeout,
            ServerError::RegexResources(_) => {
                ServerError::RegexResources(trawl_core::pattern::RegexRefusal::Budget)
            },
            ServerError::PreparationUnavailable => ServerError::PreparationUnavailable,
            ServerError::Ingest(_) => ServerError::Ingest("line 3: missing _time".into()),
            ServerError::IngestBodyTooLarge { .. } => {
                ServerError::IngestBodyTooLarge { limit: 1_048_576 }
            },
            ServerError::RequestTooLarge { .. } => ServerError::RequestTooLarge {
                setting: "[server] max_request_body_bytes",
                limit: 131_072,
            },
            ServerError::RateLimited { .. } => ServerError::RateLimited { retry_after_secs: 2 },
            ServerError::TooManyStreams => ServerError::TooManyStreams,
            ServerError::ServiceUnavailable(_) => {
                ServerError::ServiceUnavailable("service schema not yet available".into())
            },
            ServerError::CapacityNotStarted => ServerError::CapacityNotStarted,
            ServerError::AuthBackend(_) => ServerError::AuthBackend(CauseKind::PgIo),
            ServerError::HotBufferFull { .. } => ServerError::HotBufferFull { retry_after_secs: 5 },
            ServerError::IngestBatchTooLarge { .. } => ServerError::IngestBatchTooLarge {
                requested: None,
                ceiling: Charge::ZERO,
            },
            ServerError::PreviewTooLarge { .. } => ServerError::PreviewTooLarge { limit: 500 },
            ServerError::CaptureSourceTooLarge { .. } => ServerError::CaptureSourceTooLarge {
                selected_bytes: 20,
                max_bytes: 10,
                kind: CaptureSourceKind::Window,
            },
            ServerError::CaptureBounds(_) => ServerError::CaptureBounds(BoundsError::LatestAlone),
            ServerError::CaptureTooLarge(_) => ServerError::CaptureTooLarge(CaptureOverflow::Rows {
                rows: 6000,
                max_rows: 5000,
            }),
            ServerError::CaptureBusy { .. } => ServerError::CaptureBusy { retry_after_secs: 7 },
            ServerError::CaptureQuotaExceeded { .. } => {
                ServerError::CaptureQuotaExceeded { retry_after_secs: 900 }
            },
            ServerError::ResultGone { .. } => ServerError::ResultGone {
                reason: GoneReason::Expired,
            },
            ServerError::ResultUnavailable => ServerError::ResultUnavailable,
            ServerError::ResultCorrupt => ServerError::ResultCorrupt,
            ServerError::FocusIncomplete(_) => {
                ServerError::FocusIncomplete("the focus row has no `host`".into())
            },
            ServerError::FocusAmbiguous(_) => {
                ServerError::FocusAmbiguous("the focus row has two `env` columns".into())
            },
            ServerError::WindowsOverlap(_) => {
                ServerError::WindowsOverlap("the foreground and baseline windows overlap".into())
            },
            ServerError::CorrelationInvalid(_) => {
                ServerError::CorrelationInvalid("the literal is empty".into())
            },
            ServerError::SequenceConflict { .. } => ServerError::SequenceConflict {
                expected: 4,
                current: 6,
            },
            ServerError::InvestigationExists => ServerError::InvestigationExists,
            ServerError::InvestigationClosed => ServerError::InvestigationClosed,
            ServerError::EvidenceBudgetExceeded { .. } => {
                ServerError::EvidenceBudgetExceeded {
                    scope: EvidenceScope::Investigation,
                    used: 10,
                    limit: 20,
                }
            },
            ServerError::UnsupportedEncoding => ServerError::UnsupportedEncoding,
            ServerError::CorpusRecovering(_) => {
                ServerError::CorpusRecovering(CorpusUnsettled::RollupPending)
            },
            ServerError::HotSnapshot(_) => {
                ServerError::HotSnapshot(std::io::Error::other("snapshot"))
            },
            ServerError::EnvListing(_) => ServerError::EnvListing(std::io::Error::other("listing")),
            ServerError::EngineSampleFailed => ServerError::EngineSampleFailed,
            ServerError::RequestLimitReached(_) => {
                ServerError::RequestLimitReached(Allowance::Probe)
            },
            ServerError::Internal(_) => ServerError::Internal("task was cancelled".into()),
            ServerError::Panicked(_) => ServerError::Panicked("query worker"),
        }
        engine {
            EngineError::Parse(_) => EngineError::Parse(vec![trawl_core::parser::ParseError {
                message: "unexpected token".into(),
                span: 0..3,
                label: None,
                hint: None,
            }]),
            EngineError::Emit(_) => {
                EngineError::Emit(emit_error("* | stats nosuchfunc(x) by host"))
            },
            EngineError::Database(_) => {
                EngineError::Database(duckdb::Error::InvalidColumnName("column".into()))
            },
            EngineError::ResourceExhausted => EngineError::ResourceExhausted,
            EngineError::RegexResources(_) => {
                EngineError::RegexResources(trawl_core::pattern::RegexRefusal::Reservation)
            },
            EngineError::ResultTooLarge(_) => EngineError::ResultTooLarge(5000),
            EngineError::AggregationCapped(_) => {
                EngineError::AggregationCapped(trawl_core::stream::AggregationCap::Groups)
            },
            EngineError::ColdDataUnread => EngineError::ColdDataUnread,
            EngineError::Refused { .. } => EngineError::Refused {
                message: "timechart on 'host' is not a timestamp: VARCHAR".into(),
            },
            EngineError::IntegerOutOfRange { .. } => EngineError::IntegerOutOfRange {
                column: "total".into(),
            },
            EngineError::NotParquetStorable { .. } => EngineError::NotParquetStorable {
                reason: "a mixed column".into(),
            },
            EngineError::BudgetNotApplied { .. } => EngineError::BudgetNotApplied {
                setting: "memory_limit",
                configured: "1GiB".into(),
                applied: "2GiB".into(),
            },
            EngineError::MissingPreparedPattern => EngineError::MissingPreparedPattern,
            EngineError::Io(_) => EngineError::Io(std::io::Error::other("io")),
            EngineError::Cancelled => EngineError::Cancelled,
        }
        store {
            StoreError::Unavailable(_) => StoreError::Unavailable(sqlx::Error::PoolTimedOut),
            StoreError::Migration(_) => {
                StoreError::Migration(crate::store::migrations::SchemaError::UntrackedSchema)
            },
            StoreError::LockHeld => StoreError::LockHeld,
            StoreError::DuplicateName { .. } => StoreError::DuplicateName { name: "x".into() },
            StoreError::ScheduleExists { .. } => StoreError::ScheduleExists { saved_query_id: 1 },
            StoreError::InvestigationNotFound => StoreError::InvestigationNotFound,
            StoreError::SequenceConflict { .. } => StoreError::SequenceConflict {
                expected: 1,
                current: 2,
            },
            StoreError::InvestigationClosed => StoreError::InvestigationClosed,
            StoreError::NotFound { .. } => StoreError::NotFound {
                id: 9,
                resource: "saved query",
            },
            StoreError::Validation(_) => StoreError::Validation("expires_at is in the past".into()),
            StoreError::InvalidInterval { .. } => {
                StoreError::InvalidInterval { input: "5x".into() }
            },
            StoreError::IntervalTooShort { .. } => StoreError::IntervalTooShort { secs: 3 },
            StoreError::DurationTooLong { .. } => StoreError::DurationTooLong { secs: u64::MAX },
            StoreError::LagWithoutWindow { .. } => StoreError::LagWithoutWindow { lag_secs: 60 },
            StoreError::InvalidName { .. } => StoreError::InvalidName { name: "a\tb".into() },
            StoreError::RepinAlreadyRunning => StoreError::RepinAlreadyRunning,
            StoreError::PurgeCommitUnknown => StoreError::PurgeCommitUnknown,
            StoreError::PurgePrepareTimeout => StoreError::PurgePrepareTimeout,
            StoreError::WaitExpired => StoreError::WaitExpired,
            StoreError::RepinPinVanished { .. } => StoreError::RepinPinVanished {
                field: "status".into(),
                expected: "BIGINT",
                found: None,
            },
        }
    }

    /// Every variant's disclosure is decided at compile time: the witness
    /// above names each one with no wildcard at any level, and each sample
    /// is the variant its row names, so no sample stands in for another
    /// and every row's sample reaches the projection tests below.
    #[test]
    fn every_variant_has_a_decided_disclosure() {
        let mut names = std::collections::HashSet::new();
        for (row, error) in every_variant() {
            assert_eq!(variant_name(&error), row, "the sample in row {row}");
            assert!(names.insert(row), "{row} has two rows");
            assert!(!error.safe_message().is_empty(), "{row} discloses nothing");
        }
    }

    /// The decoded wire `message` is the public message every record
    /// stores, for every variant: one projection, not two policies.
    #[tokio::test]
    async fn wire_and_stored_messages_are_one_projection() {
        for (row, error) in every_variant() {
            let stored = error.safe_message();
            let code = serde_json::to_value(error.code()).unwrap();
            let (status, _, body) = answer(error).await;
            assert_eq!(body["error"]["message"], stored, "{row}");
            assert_eq!(body["error"]["code"], code, "{row}");
            assert!(
                status.is_client_error() || status.is_server_error(),
                "{row} answers {status}"
            );
        }
    }

    /// Errors that carry internals answer their fixed sentence, wherever
    /// the planted path, SQL, driver text or panic label sat inside them.
    #[tokio::test]
    async fn internal_markers_do_not_cross_the_disclosure_boundary() {
        use crate::store::migrations::SchemaError;

        const PATH: &str = "/var/lib/trawl/data/MARKER-path/prod/x.parquet";
        const SQL: &str = "SELECT MARKER_sql FROM read_parquet('/var/lib/trawl/data/**')";
        const DRIVER: &str = "MARKER-driver: password authentication failed for user trawl";
        const PANIC: &str = "MARKER-panic-label";
        // A plan error has no field to plant in. Its own sentence is the
        // detail that stays in the log, so it is the marker.
        let plan_detail = PlanError::Arithmetic.to_string();

        let cases: Vec<(ServerError, String)> = vec![
            (
                ServerError::Engine(EngineError::Database(duckdb::Error::DuckDBFailure(
                    duckdb::ffi::Error::new(1),
                    Some(SQL.into()),
                ))),
                SQL.into(),
            ),
            (
                ServerError::Engine(EngineError::Database(duckdb::Error::InvalidColumnName(
                    PATH.into(),
                ))),
                PATH.into(),
            ),
            (
                ServerError::Engine(EngineError::Io(std::io::Error::other(PATH))),
                PATH.into(),
            ),
            (
                ServerError::Engine(EngineError::BudgetNotApplied {
                    setting: "temp_directory",
                    configured: PATH.into(),
                    applied: SQL.into(),
                }),
                "MARKER".into(),
            ),
            (
                ServerError::Engine(EngineError::NotParquetStorable {
                    reason: PATH.into(),
                }),
                PATH.into(),
            ),
            (
                ServerError::Store(StoreError::Unavailable(sqlx::Error::Protocol(
                    DRIVER.into(),
                ))),
                DRIVER.into(),
            ),
            (
                ServerError::Store(StoreError::Migration(SchemaError::Database(
                    sqlx::Error::Protocol(DRIVER.into()),
                ))),
                DRIVER.into(),
            ),
            (
                ServerError::HotSnapshot(std::io::Error::other(PATH)),
                PATH.into(),
            ),
            (
                ServerError::EnvListing(std::io::Error::other(PATH)),
                PATH.into(),
            ),
            (
                ServerError::Internal(format!("{DRIVER} at {PATH} running {SQL}")),
                "MARKER".into(),
            ),
            (ServerError::Panicked(PANIC), PANIC.into()),
            (ServerError::WindowPlan(PlanError::Arithmetic), plan_detail),
            (
                ServerError::WindowMaterialize(MaterializeError::SourceUnparseable {
                    message: SQL.into(),
                }),
                SQL.into(),
            ),
            (
                ServerError::WindowMaterialize(MaterializeError::OutputUnparseable {
                    message: PATH.into(),
                }),
                PATH.into(),
            ),
            (
                ServerError::WindowMaterialize(MaterializeError::BoundsNotReadBack {
                    start: PATH.into(),
                    end: SQL.into(),
                }),
                "MARKER".into(),
            ),
        ];
        for (error, marker) in cases {
            let label = format!("{error:?}");
            // The marker is in the error, so its absence below is the
            // projection's doing.
            assert!(
                error.to_string().contains(&marker) || label.contains(&marker),
                "the marker was planted in {label}"
            );
            let stored = error.safe_message();
            let (_, headers, body) = answer(error).await;
            let wire = body["error"]["message"].as_str().unwrap().to_owned();
            for (place, text) in [
                ("public message", stored),
                ("wire message", wire),
                ("body", body.to_string()),
            ] {
                assert!(!text.contains(&marker), "{place} of {label}: {text}");
                assert!(!text.contains("/var/lib"), "{place} of {label}: {text}");
            }
            for value in headers.values() {
                assert!(!value.to_str().unwrap().contains("MARKER"), "{label}");
            }
        }
    }

    /// What a caller needs to act on survives the projection whole: parse
    /// spans and emit hints, configured limits, counts, sequence numbers
    /// and the specialized 409 codes, each with its status, code, details,
    /// `conflict` member and headers, and with the same text in the record.
    #[tokio::test]
    #[allow(clippy::too_many_lines)] // one flat table of facts, one row per answer
    async fn approved_domain_facts_remain_actionable() {
        use serde_json::json;

        struct Fact {
            error: ServerError,
            status: StatusCode,
            code: &'static str,
            message: &'static str,
            details: Option<serde_json::Value>,
            conflict: Option<serde_json::Value>,
            retry_after: Option<&'static str>,
            no_store: bool,
        }
        let fact = |error, status, code, message| Fact {
            error,
            status,
            code,
            message,
            details: None,
            conflict: None,
            retry_after: None,
            no_store: false,
        };

        let facts = vec![
            Fact {
                details: Some(json!([
                    {
                        "message": "unexpected token 'zz_tok'",
                        "span": {"start": 4, "end": 10},
                        "label": "pipeline",
                        "hint": "did you mean 'stats'?",
                    },
                    {"message": "unclosed string", "span": {"start": 12, "end": 14}},
                ])),
                ..fact(
                    ServerError::Engine(EngineError::Parse(vec![
                        trawl_core::parser::ParseError {
                            message: "unexpected token 'zz_tok'".into(),
                            span: 4..10,
                            label: Some("pipeline".into()),
                            hint: Some("did you mean 'stats'?".into()),
                        },
                        trawl_core::parser::ParseError {
                            message: "unclosed string".into(),
                            span: 12..14,
                            label: None,
                            hint: None,
                        },
                    ])),
                    StatusCode::BAD_REQUEST,
                    "parse_error",
                    "unexpected token 'zz_tok'",
                )
            },
            Fact {
                details: Some(json!([
                    {"message": "unknown function: countt", "hint": "did you mean 'count'?"},
                ])),
                ..fact(
                    ServerError::Engine(EngineError::Emit(emit_error(
                        "* | stats countt(x) by host",
                    ))),
                    StatusCode::BAD_REQUEST,
                    "validation_error",
                    "unknown function: countt (did you mean 'count'?)",
                )
            },
            fact(
                ServerError::RequestTooLarge {
                    setting: "[server] max_request_body_bytes",
                    limit: 131_072,
                },
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_too_large",
                "request body exceeds [server] max_request_body_bytes (131072 bytes)",
            ),
            fact(
                ServerError::IngestBodyTooLarge { limit: 1_048_576 },
                StatusCode::PAYLOAD_TOO_LARGE,
                "ingest_error",
                "gzip body decompresses past the 1048576 byte limit ([ingest] max_body_bytes); \
                 send smaller batches",
            ),
            fact(
                ServerError::IngestBatchTooLarge {
                    requested: Some(Charge {
                        events: 61,
                        bytes: 700,
                    }),
                    ceiling: Charge {
                        events: 60,
                        bytes: 960,
                    },
                },
                StatusCode::PAYLOAD_TOO_LARGE,
                "ingest_batch_too_large",
                "ingest batch too large: 61 events and 700 serialized bytes exceed the limit of \
                 60 events or 960 serialized bytes per request; split the batch",
            ),
            fact(
                ServerError::Engine(EngineError::ResultTooLarge(5000)),
                StatusCode::BAD_REQUEST,
                "result_too_large",
                "result exceeded 5000 row limit",
            ),
            fact(
                ServerError::CaptureSourceTooLarge {
                    selected_bytes: 20,
                    max_bytes: 10,
                    kind: CaptureSourceKind::Window,
                },
                StatusCode::BAD_REQUEST,
                "capture_source_too_large",
                "the window selects 20 bytes of stored data, above the capture limit of 10 \
                 bytes; narrow the window, or pin the services with service=a or service=a,b",
            ),
            fact(
                ServerError::CaptureSourceTooLarge {
                    selected_bytes: 20,
                    max_bytes: 10,
                    kind: CaptureSourceKind::StoredRun,
                },
                StatusCode::BAD_REQUEST,
                "capture_source_too_large",
                "the window selects 20 bytes of stored data, above the capture limit of 10 \
                 bytes; narrow the window",
            ),
            fact(
                ServerError::EvidenceBudgetExceeded {
                    scope: EvidenceScope::Investigation,
                    used: 10,
                    limit: 20,
                },
                StatusCode::CONFLICT,
                "evidence_budget_exceeded",
                "keeping this result would exceed this investigation's evidence budget: 10 of 20 \
                 bytes are counted against it, kept evidence and reservations together; evidence \
                 frees as it ages out, or when an operator with server_manage deletes an \
                 investigation",
            ),
            fact(
                ServerError::EvidenceBudgetExceeded {
                    scope: EvidenceScope::Installation,
                    used: 10,
                    limit: 20,
                },
                StatusCode::CONFLICT,
                "evidence_budget_exceeded",
                "keeping this result would exceed the installation's evidence budget: 10 of 20 \
                 bytes are counted against it, kept evidence and reservations together; evidence \
                 frees as it ages out, or when an operator with server_manage deletes an \
                 investigation",
            ),
            Fact {
                conflict: Some(json!({"expected": 4, "current": 6})),
                ..fact(
                    ServerError::SequenceConflict {
                        expected: 4,
                        current: 6,
                    },
                    StatusCode::CONFLICT,
                    "sequence_conflict",
                    "the investigation changed: the request expected sequence 4 and it is now \
                     6; read it again and retry",
                )
            },
            Fact {
                conflict: Some(json!({"expected": 1, "current": 2})),
                ..fact(
                    ServerError::Store(StoreError::SequenceConflict {
                        expected: 1,
                        current: 2,
                    }),
                    StatusCode::CONFLICT,
                    "sequence_conflict",
                    "the investigation is at sequence 2, not the expected 1",
                )
            },
            fact(
                ServerError::InvestigationExists,
                StatusCode::CONFLICT,
                "investigation_exists",
                "an investigation with this id already exists with a different creator or \
                 different fields; create it under a new id",
            ),
            fact(
                ServerError::InvestigationClosed,
                StatusCode::CONFLICT,
                "investigation_closed",
                "the investigation is closed; reopen it before adding work, keeps, findings or \
                 notes",
            ),
            fact(
                ServerError::ResultUnavailable,
                StatusCode::CONFLICT,
                "result_unavailable",
                "the captured result's file is missing; its rows cannot be read",
            ),
            // A generic 409 keeps `bad_request`, with its remedy as written.
            fact(
                ServerError::Conflict("a repin owns the data root; retry after it finishes".into()),
                StatusCode::CONFLICT,
                "bad_request",
                "a repin owns the data root; retry after it finishes",
            ),
            fact(
                ServerError::Store(StoreError::RepinAlreadyRunning),
                StatusCode::CONFLICT,
                "bad_request",
                "a repin job is already running (one at a time, install-wide)",
            ),
            fact(
                ServerError::Store(StoreError::DuplicateName { name: "x".into() }),
                StatusCode::CONFLICT,
                "duplicate_name",
                "a saved query with this name already exists",
            ),
            fact(
                ServerError::Store(StoreError::InvalidName { name: "a b".into() }),
                StatusCode::BAD_REQUEST,
                "invalid_name",
                "name must not be blank or contain control or invisible formatting characters",
            ),
            fact(
                ServerError::BadRequest("offset + limit exceeds max_result_rows (1000)".into()),
                StatusCode::BAD_REQUEST,
                "bad_request",
                "offset + limit exceeds max_result_rows (1000)",
            ),
            fact(
                ServerError::Store(StoreError::IntervalTooShort { secs: 3 }),
                StatusCode::BAD_REQUEST,
                "bad_request",
                "schedule interval 3s is below minimum of 60s",
            ),
            fact(
                ServerError::Store(StoreError::NotFound {
                    id: 9,
                    resource: "saved query",
                }),
                StatusCode::NOT_FOUND,
                "not_found",
                "saved query not found or unauthorized",
            ),
            fact(
                ServerError::WindowPolicy(WindowPolicyError::TimeClause {
                    window: crate::report_window::ScheduleWindow::SinceLast,
                    clause: trawl_core::ast::TimeClause::Last,
                }),
                StatusCode::BAD_REQUEST,
                "bad_request",
                "schedule window \"since_last\" conflicts with the saved query's last= time \
                 clause; remove one side",
            ),
            Fact {
                retry_after: Some("37"),
                ..fact(
                    ServerError::HotBufferFull {
                        retry_after_secs: 37,
                    },
                    StatusCode::SERVICE_UNAVAILABLE,
                    "hot_buffer_full",
                    "hot buffer full: nothing from this request was written; retry it after \
                     compaction drains",
                )
            },
            Fact {
                no_store: true,
                ..fact(
                    ServerError::RequestLimitReached(Allowance::Regular),
                    StatusCode::SERVICE_UNAVAILABLE,
                    "request_limit_reached",
                    REQUEST_LIMIT_REACHED,
                )
            },
        ];

        for fact in facts {
            let label = format!("{:?}", fact.error);
            assert_eq!(fact.error.safe_message(), fact.message, "{label}");
            if fact.code == "evidence_budget_exceeded" {
                assert!(!fact.message.contains("already kept"), "{label}");
            }
            let (status, headers, body) = answer(fact.error).await;
            assert_eq!(status, fact.status, "{label}");
            let error = &body["error"];
            assert_eq!(error["code"], fact.code, "{label}");
            assert_eq!(error["message"], fact.message, "{label}");
            assert_eq!(error.get("details"), fact.details.as_ref(), "{label}");
            assert_eq!(error.get("conflict"), fact.conflict.as_ref(), "{label}");
            assert_eq!(
                headers
                    .get(axum::http::header::RETRY_AFTER)
                    .map(|v| v.to_str().unwrap()),
                fact.retry_after,
                "{label}"
            );
            assert_eq!(
                headers.get(axum::http::header::CACHE_CONTROL).is_some(),
                fact.no_store,
                "{label}"
            );
        }
    }

    /// One event a fault logged: its level and every field, the message
    /// among them.
    #[derive(Debug)]
    struct LoggedEvent {
        level: tracing::Level,
        fields: std::collections::HashMap<String, String>,
    }

    struct EventFields<'a>(&'a mut std::collections::HashMap<String, String>);

    impl tracing::field::Visit for EventFields<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0.insert(field.name().to_owned(), format!("{value:?}"));
        }
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.0.insert(field.name().to_owned(), value.to_owned());
        }
    }

    #[derive(Clone, Default)]
    struct EventLog(std::sync::Arc<std::sync::Mutex<Vec<LoggedEvent>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for EventLog {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = std::collections::HashMap::new();
            event.record(&mut EventFields(&mut fields));
            self.0.lock().unwrap().push(LoggedEvent {
                level: *event.metadata().level(),
                fields,
            });
        }
    }

    /// Every event `work` emits, at every level.
    fn events_of(work: impl FnOnce()) -> Vec<LoggedEvent> {
        use tracing_subscriber::layer::SubscriberExt;

        let log = EventLog::default();
        tracing::subscriber::with_default(tracing_subscriber::registry().with(log.clone()), work);
        std::mem::take(&mut *log.0.lock().unwrap())
    }

    /// A fault's default-level events say which failure it was and what
    /// it came from, and never its text: the planted marker reaches DEBUG
    /// only (ADR-0040, amended 2026-11-08). `log_fault` runs where the
    /// response is built, and the auth backend logs where its error
    /// converts, so each is driven through that path.
    #[test]
    fn default_fault_events_carry_no_error_text() {
        use crate::store::migrations::SchemaError;
        use tracing::Level;

        const PATH: &str = "/var/lib/trawl/data/MARKER-fault/prod/x.parquet";
        const PANIC: &str = "MARKER-panic-label";

        // The response path, for every fault `log_fault` logs. A unit
        // fault's own sentence is the text that stays at DEBUG.
        let faults: Vec<(ServerError, String)> = vec![
            (
                ServerError::Store(StoreError::Unavailable(sqlx::Error::Protocol(PATH.into()))),
                PATH.into(),
            ),
            (
                ServerError::Store(StoreError::Migration(SchemaError::Database(
                    sqlx::Error::Protocol(PATH.into()),
                ))),
                PATH.into(),
            ),
            (
                ServerError::WindowMaterialize(MaterializeError::SourceUnparseable {
                    message: PATH.into(),
                }),
                PATH.into(),
            ),
            (
                ServerError::WindowPlan(PlanError::Arithmetic),
                PlanError::Arithmetic.to_string(),
            ),
            (
                ServerError::Engine(EngineError::MissingPreparedPattern),
                EngineError::MissingPreparedPattern.to_string(),
            ),
            (
                ServerError::Internal(format!("failed to read parquet temp file {PATH}")),
                PATH.into(),
            ),
            (ServerError::Panicked(PANIC), PANIC.into()),
        ];
        let mut cases: Vec<(String, &'static str, &'static str, String, Vec<LoggedEvent>)> =
            Vec::new();
        for (fault, marker) in faults {
            let label = format!("{fault:?}");
            let (class, cause) = (fault.error_class(), fault.cause_kind().as_str());
            let events = events_of(|| drop(fault.into_response()));
            cases.push((label, class, cause, marker, events));
        }
        // The conversion path, for every keystore failure that logs.
        for (failure, marker) in [
            (
                fleet_auth::AuthError::Database(sqlx::Error::Protocol(PATH.into())),
                PATH,
            ),
            (fleet_auth::AuthError::Hash(PATH.into()), PATH),
            (fleet_auth::AuthError::TokenGeneration(PATH.into()), PATH),
        ] {
            let label = format!("{failure:?}");
            let mut converted = None;
            let events = events_of(|| converted = Some(ServerError::from(failure)));
            let converted = converted.expect("the failure converts");
            let (class, cause) = (converted.error_class(), converted.cause_kind().as_str());
            cases.push((label, class, cause, marker.to_owned(), events));
        }

        for (label, class, cause, marker, events) in cases {
            let loud: Vec<_> = events
                .iter()
                .filter(|e| [Level::INFO, Level::WARN, Level::ERROR].contains(&e.level))
                .collect();
            assert!(
                loud.iter().any(|e| e.level == Level::ERROR),
                "{label} logs its fault at ERROR: {events:?}"
            );
            for event in &loud {
                assert_eq!(
                    event.fields.get("error_class").map(String::as_str),
                    Some(class),
                    "{label}: {event:?}"
                );
                assert_eq!(
                    event.fields.get("cause_kind").map(String::as_str),
                    Some(cause),
                    "{label}: {event:?}"
                );
                assert!(!event.fields.contains_key("error"), "{label}: {event:?}");
                for (field, value) in &event.fields {
                    assert!(
                        !value.contains(&marker) && !value.contains("MARKER"),
                        "{label}: {field} carries the error's text at {}: {value}",
                        event.level
                    );
                }
            }
            assert!(
                events.iter().any(|e| e.level == Level::DEBUG
                    && e.fields.get("error").is_some_and(|v| v.contains(&marker))),
                "{label}: the error's text is kept at DEBUG: {events:?}"
            );
        }
    }
}
