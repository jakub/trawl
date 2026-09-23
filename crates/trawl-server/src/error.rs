// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Unified server error type with HTTP status mapping.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use trawl_engine::error::EngineError;

use crate::report_window::{MaterializeError, PlanError, WindowPolicyError};
use crate::store::StoreError;

/// The one reason a unit of pool work refused before it ever started
/// (ADR-0024): the request's budget was gone, or its cancellation was
/// latched, while the work was still queued behind capacity.
///
/// Fixed text, no interpolation: it is a capacity fact about the server,
/// not a fact about the query, and it travels into the query tracker,
/// the scheduler's run rows and the client response unchanged. A refusal
/// here is 503, never the 504 an execution timeout earns — nothing ran.
pub const CAPACITY_NOT_STARTED: &str = "server at capacity: the query was not started";

/// Server errors, mapped to HTTP responses via [`IntoResponse`].
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// Query engine failure (parse, emit, or `DuckDB` execution).
    #[error("{0}")]
    Engine(#[from] EngineError),

    /// App-state store failure (history, saved queries, schedules, runs).
    /// Mapped per the ADR-0004 table: unavailability → 503 (redacted),
    /// conflict → 409, not-found → 404, validation → 400.
    #[error("{0}")]
    Store(#[from] StoreError),

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
    /// store error it reaches the wire intact.
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

    /// Ingest validation error (bad ndjson, missing fields).
    #[error("ingest error: {0}")]
    Ingest(String),

    /// Rate limit exceeded (429).
    #[error("rate limit exceeded")]
    RateLimited,

    /// Too many concurrent SSE streams (429).
    #[error("too many concurrent streams")]
    TooManyStreams,

    /// Service temporarily unavailable (startup, data not ready).
    #[error("service unavailable: {0}")]
    ServiceUnavailable(String),

    /// The auth backend could not answer a keystore call (503). The
    /// driver's typed kind rides along for [`ServerError::cause_kind`]; the
    /// postgres detail itself is logged where the error is converted and
    /// never reaches the wire.
    #[error("service unavailable: auth backend unavailable")]
    AuthBackend(CauseKind),

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
}

impl CauseKind {
    /// Every kind, for closed-set checks and for consumers that enumerate.
    pub const ALL: [Self; 28] = [
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
        }
    }

    /// The kind of an I/O error, from its [`std::io::ErrorKind`] alone.
    fn of_io(err: &std::io::Error) -> Self {
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
    fn of_duckdb(err: &duckdb::Error) -> Self {
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
    /// Return a sanitized error message safe for logs and tracker history.
    ///
    /// Database internals and internal error details are redacted to prevent
    /// information disclosure. Parse, emit and refusal messages (client
    /// mistakes, every one of them trawl-authored or quoting the caller's own
    /// tokens) are preserved by the catch-all below: a redacted refusal would
    /// leave the tracker, the scheduler's run rows and the client response
    /// unable to say what was refused.
    pub fn safe_message(&self) -> String {
        match self {
            Self::Engine(EngineError::Database(_)) => "query execution failed".to_owned(),
            Self::Store(StoreError::Unavailable(_) | StoreError::Migration(_)) => {
                "app-state store unavailable".to_owned()
            }
            Self::Internal(_) | Self::Panicked(_) => "internal error".to_owned(),
            // The two 500-class window errors can quote the saved DSL and
            // the parser's message; the policy refusal below them is the
            // operator's own input and keeps its text.
            Self::WindowPlan(_) | Self::WindowMaterialize(_) => {
                "report window could not be resolved".to_owned()
            }
            Self::Ingest(_) => "ingest error".to_owned(),
            Self::BadRequest(_) => "bad request".to_owned(),
            Self::NotFound(_) => "not found".to_owned(),
            Self::Forbidden(_) => "forbidden".to_owned(),
            Self::RateLimited => "rate limit exceeded".to_owned(),
            Self::TooManyStreams => "too many concurrent streams".to_owned(),
            // [`CAPACITY_NOT_STARTED`] is a fixed server-capacity sentence
            // with nothing to redact, and it is the whole answer a
            // pre-start refusal gives: erasing it to "service unavailable"
            // would leave the scheduler's run row and the query tracker
            // unable to tell a capacity refusal from any other 503.
            Self::ServiceUnavailable(msg) if msg == CAPACITY_NOT_STARTED => msg.clone(),
            Self::ServiceUnavailable(_) | Self::AuthBackend(_) => "service unavailable".to_owned(),
            other => other.to_string(),
        }
    }

    /// Return a stable, content-free classification of this error.
    ///
    /// SECURITY: default-filter telemetry logs this instead of any message.
    /// [`safe_message`](Self::safe_message) is safe to hand a *client* but not
    /// safe to persist: it deliberately preserves parse/emit text, and
    /// parser/emitter messages quote the user's own tokens and format strings
    /// (`Self::Engine(EngineError::Database)`'s raw form likewise embeds the
    /// generated SQL and the values it choked on). The class comes from a
    /// closed set of literals, so it is safe to store in the retained
    /// `service=trawld` corpus and stable enough to alarm on.
    pub fn error_class(&self) -> &'static str {
        match self {
            Self::Engine(EngineError::Parse(_)) => "parse",
            Self::Engine(EngineError::Emit(_)) => "emit",
            Self::Engine(EngineError::Refused { .. }) => "refused",
            Self::Engine(EngineError::Database(_)) => "database",
            Self::Engine(EngineError::ResultTooLarge(_)) => "result_too_large",
            Self::Engine(EngineError::ColdDataUnread) => "cold_data_unread",
            Self::Engine(EngineError::Cancelled) => "cancelled",
            Self::Engine(EngineError::Io(_)) => "io",
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
            Self::Ingest(_) => "ingest",
            Self::RateLimited => "rate_limited",
            Self::TooManyStreams => "too_many_streams",
            // An auth backend outage is the same 503 it was before the
            // driver's kind rode along; `cause_kind` tells it apart.
            Self::ServiceUnavailable(_) | Self::AuthBackend(_) => "service_unavailable",
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
            Self::Engine(EngineError::Io(e)) => CauseKind::of_io(e),
            Self::Store(StoreError::Unavailable(e)) => CauseKind::of_sqlx(e),
            Self::Store(StoreError::Migration(e)) => CauseKind::of_store_schema(e),
            Self::AuthBackend(kind) => *kind,
            // A pre-start capacity refusal is a pressure outcome the class
            // names in full. Every other 503 was built from a string and
            // kept no typed source.
            Self::ServiceUnavailable(msg) if msg == CAPACITY_NOT_STARTED => CauseKind::None,
            Self::ServiceUnavailable(_) | Self::Internal(_) => CauseKind::Unknown,
            Self::Engine(_)
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
            | Self::RateLimited
            | Self::TooManyStreams
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

impl From<crate::store::WindowWriteError> for ServerError {
    /// Split a checked window write back into the two answers it already
    /// carries. Neither half gains or loses anything here: the store fault
    /// keeps the SQLSTATE mapping every other store error gets, and the
    /// policy refusal keeps its 400 and its message.
    fn from(err: crate::store::WindowWriteError) -> Self {
        use crate::store::WindowWriteError as E;
        match err {
            E::Store(e) => Self::Store(e),
            E::Policy(e) => Self::WindowPolicy(e),
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
        match err {
            E::Database(e) => {
                tracing::error!(target: "auth.backend", error = %e, "fleet auth backend error");
                Self::AuthBackend(CauseKind::of_sqlx(&e))
            }
            E::Migration(e) => {
                tracing::error!(target: "auth.backend", error = %e, "fleet auth migration error");
                Self::AuthBackend(CauseKind::PgMigrate)
            }
            E::Schema(e) => {
                tracing::error!(target: "auth.backend", error = %e, "fleet auth schema error");
                Self::AuthBackend(CauseKind::of_auth_schema(&e))
            }
            E::Hash(e) | E::TokenGeneration(e) => {
                tracing::error!(target: "auth.backend", error = %e, "fleet auth worker error");
                Self::AuthBackend(CauseKind::AuthWorker)
            }
            E::InvalidKey(_) | E::MalformedToken(_) => {
                Self::Unauthorized("authentication failed".into())
            }
            other => Self::Internal(format!("unexpected fleet-auth error: {other}")),
        }
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

impl IntoResponse for ServerError {
    #[allow(clippy::too_many_lines)] // exhaustive error table is cohesive
    fn into_response(self) -> Response {
        use trawl_api::{ErrorCode, ErrorEnvelope};

        let (status, envelope) = match &self {
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
            Self::Engine(EngineError::ResultTooLarge(n)) => (
                StatusCode::BAD_REQUEST,
                ErrorEnvelope::simple(
                    ErrorCode::ResultTooLarge,
                    format!("result exceeded {n} row limit"),
                ),
            ),
            // Database/IO errors are server-side — don't leak details.
            //
            // A cancellation caught at the bind-to-execute boundary answers
            // here too, deliberately: cancelling a running query has always
            // surfaced as `DuckDB`'s interrupted execution, and the latch
            // exists to catch the same request when the interrupt was
            // swallowed during a bind (ADR-0024). One request, one answer —
            // the latch does not earn the caller a different status than the
            // interrupt it backs up. `error_class` still tells the two apart
            // for telemetry.
            Self::Engine(
                EngineError::Database(_) | EngineError::Io(_) | EngineError::Cancelled,
            ) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorEnvelope::simple(ErrorCode::ExecutionError, "query execution failed"),
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
            // (they are logged server-side); conflicts are 409 with their
            // domain message; ownership misses are opaque 404s.
            Self::Store(e) => match e {
                StoreError::Unavailable(source) => {
                    tracing::error!(target: "storage.backend", error = %source, "app-state store error");
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        ErrorEnvelope::simple(
                            ErrorCode::ServiceUnavailable,
                            "app-state store unavailable",
                        ),
                    )
                }
                StoreError::Migration(source) => {
                    tracing::error!(target: "storage.backend", error = %source, "app-state store migration error");
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        ErrorEnvelope::simple(
                            ErrorCode::ServiceUnavailable,
                            "app-state store unavailable",
                        ),
                    )
                }
                // The purge is neither committed nor rolled back as far as
                // this process knows, and 503 is the honest answer: the
                // request did not complete, and retrying is safe only after
                // the operator has looked. The message says so; it names no
                // pg diagnostics.
                // Same 503 for the pre-commit bound, and the same reason to
                // say more than "store unavailable": the operator's next
                // move differs from a plain outage. This one adds that
                // nothing was reclaimed, which is provable — the dropped
                // transaction rolled back.
                StoreError::PurgeCommitUnknown | StoreError::PurgePrepareTimeout => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    ErrorEnvelope::simple(ErrorCode::ServiceUnavailable, e.to_string()),
                ),
                StoreError::LockHeld => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    ErrorEnvelope::simple(
                        ErrorCode::ServiceUnavailable,
                        "app-state store unavailable",
                    ),
                ),
                StoreError::DuplicateName { .. }
                | StoreError::ScheduleExists { .. }
                | StoreError::RepinAlreadyRunning => (
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
                StoreError::Validation(_)
                | StoreError::RepinPinVanished { .. }
                | StoreError::InvalidInterval { .. }
                | StoreError::IntervalTooShort { .. }
                | StoreError::DurationTooLong { .. }
                | StoreError::LagWithoutWindow { .. }
                | StoreError::InvalidName { .. } => (
                    StatusCode::BAD_REQUEST,
                    ErrorEnvelope::simple(ErrorCode::BadRequest, e.to_string()),
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
            Self::WindowPlan(_) | Self::WindowMaterialize(_) => {
                tracing::error!(event_type = "report_window_error", error = %self, "report window could not be resolved");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ErrorEnvelope::simple(
                        ErrorCode::InternalError,
                        "report window could not be resolved",
                    ),
                )
            }
            Self::Timeout => (
                StatusCode::GATEWAY_TIMEOUT,
                ErrorEnvelope::simple(ErrorCode::Timeout, "query timed out"),
            ),
            Self::RateLimited => (
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
            Self::AuthBackend(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorEnvelope::simple(ErrorCode::ServiceUnavailable, "auth backend unavailable"),
            ),
            // `self` renders as the fixed label only: a panic's payload
            // never made it into the variant.
            Self::Internal(_) | Self::Panicked(_) => {
                tracing::error!(
                    event_type = "internal_error",
                    error_class = self.error_class(),
                    error = %self,
                    "internal server error"
                );
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ErrorEnvelope::simple(ErrorCode::InternalError, "internal server error"),
                )
            }
        };

        let body = trawl_api::ErrorResponse { error: envelope };
        (status, axum::Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Parse and emit a query the emitter refuses, and read the envelope
    /// the server answers with.
    async fn emit_refusal(query: &str) -> (StatusCode, serde_json::Value) {
        let parsed = trawl_core::parser::parse(query).expect("the query parses");
        let err = trawl_core::emitter::emit(
            &parsed,
            "/data/**/*.parquet",
            trawl_core::context::EvalContext::capture(),
        )
        .expect_err("the emitter refuses the query");
        let response = ServerError::Engine(EngineError::Emit(err)).into_response();
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
        // paths; the class stays a literal either way.
        let conflict = ServerError::Conflict("/var/lib/trawl/data/prod/x.parquet".into());
        assert_eq!(conflict.error_class(), "conflict");
        assert!(conflict.safe_message().contains("x.parquet"));
        assert_eq!(
            ServerError::Internal("dsn leaked".into()).error_class(),
            "internal"
        );
        assert_eq!(ServerError::Panicked("query worker").error_class(), "panic");
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
                | CauseKind::AuthWorker => true,
            })
            .count();
        assert_eq!(variants, 28, "ALL lists every variant");

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
            (
                ServerError::ServiceUnavailable(CAPACITY_NOT_STARTED.to_owned()),
                CauseKind::None,
            ),
            (
                ServerError::Internal("zz_detail".into()),
                CauseKind::Unknown,
            ),
        ];
        for (err, expected) in cases {
            assert_eq!(err.cause_kind(), expected, "wrong cause kind for {err:?}");
        }
    }

    /// The auth backend's 503 keeps the answer it always gave while the
    /// driver's kind rides along for the failure record.
    #[tokio::test]
    async fn an_auth_backend_failure_answers_the_same_redacted_503() {
        let err = ServerError::from(fleet_auth::AuthError::Database(sqlx::Error::PoolTimedOut));
        assert_eq!(err.error_class(), "service_unavailable");
        assert_eq!(err.safe_message(), "service unavailable");
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
        assert_eq!(err.safe_message(), "internal error");
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

    /// The pre-start capacity refusal is a fixed sentence about the
    /// server, so it survives redaction and reaches the query tracker and
    /// the run row intact. Every other 503 still generalizes: an auth or
    /// store backend's own words are not for a client.
    #[test]
    fn safe_message_keeps_the_capacity_refusal_and_generalizes_the_rest() {
        let refused = ServerError::ServiceUnavailable(CAPACITY_NOT_STARTED.to_owned());
        assert_eq!(refused.safe_message(), CAPACITY_NOT_STARTED);
        assert_eq!(refused.error_class(), "service_unavailable");

        let backend = ServerError::ServiceUnavailable("pg://user:pw@host down".into());
        assert_eq!(backend.safe_message(), "service unavailable");
    }

    #[test]
    fn safe_message_redacts_internal_errors() {
        let err = ServerError::Internal("db connection string leaked".into());
        assert_eq!(err.safe_message(), "internal error");
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
        ];
        for (err, expected) in cases {
            let desc = format!("{err:?}");
            let response = err.into_response();
            assert_eq!(response.status(), expected, "wrong status for {desc}");
        }
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
}
