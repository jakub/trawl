// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Unified server error type with HTTP status mapping.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use trawl_engine::error::EngineError;

use crate::store::StoreError;

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

    /// Internal server error (task panics, unexpected failures).
    #[error("internal error: {0}")]
    Internal(String),
}

impl ServerError {
    /// Return a sanitized error message safe for logs and tracker history.
    ///
    /// Database internals and internal error details are redacted to prevent
    /// information disclosure. Parse/emit errors (client mistakes) are preserved.
    pub fn safe_message(&self) -> String {
        match self {
            Self::Engine(EngineError::Database(_)) => "query execution failed".to_owned(),
            Self::Store(StoreError::Unavailable(_) | StoreError::Migration(_)) => {
                "app-state store unavailable".to_owned()
            }
            Self::Internal(_) => "internal error".to_owned(),
            Self::Ingest(_) => "ingest error".to_owned(),
            Self::BadRequest(_) => "bad request".to_owned(),
            Self::NotFound(_) => "not found".to_owned(),
            Self::Forbidden(_) => "forbidden".to_owned(),
            Self::RateLimited => "rate limit exceeded".to_owned(),
            Self::TooManyStreams => "too many concurrent streams".to_owned(),
            Self::ServiceUnavailable(_) => "service unavailable".to_owned(),
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
            Self::Engine(EngineError::Database(_)) => "database",
            Self::Engine(EngineError::ResultTooLarge(_)) => "result_too_large",
            Self::Engine(EngineError::ColdDataUnread) => "cold_data_unread",
            Self::Engine(EngineError::Io(_)) => "io",
            Self::Store(_) => "store",
            Self::Unauthorized(_) => "unauthorized",
            Self::Forbidden(_) => "forbidden",
            Self::BadRequest(_) => "bad_request",
            Self::NotFound(_) => "not_found",
            Self::Conflict(_) => "conflict",
            Self::Timeout => "timeout",
            Self::Ingest(_) => "ingest",
            Self::RateLimited => "rate_limited",
            Self::TooManyStreams => "too_many_streams",
            Self::ServiceUnavailable(_) => "service_unavailable",
            Self::Internal(_) => "internal",
        }
    }
}

impl From<fleet_auth::AuthError> for ServerError {
    /// Map fleet-auth keystore failures onto trawl's error contract.
    ///
    /// Backend trouble (pg down, migration state, hash-worker panics) is a
    /// 503 without postgres detail; credential failures stay an opaque 401.
    /// Anything else in this path is a bug — surface as 500.
    fn from(err: fleet_auth::AuthError) -> Self {
        use fleet_auth::AuthError as E;
        match err {
            E::Database(e) => {
                tracing::error!(target: "auth.backend", error = %e, "fleet auth backend error");
                Self::ServiceUnavailable("auth backend unavailable".into())
            }
            E::Migration(e) => {
                tracing::error!(target: "auth.backend", error = %e, "fleet auth migration error");
                Self::ServiceUnavailable("auth backend unavailable".into())
            }
            E::Hash(e) | E::TokenGeneration(e) => {
                tracing::error!(target: "auth.backend", error = %e, "fleet auth worker error");
                Self::ServiceUnavailable("auth backend unavailable".into())
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
            Self::Engine(EngineError::Emit(e)) => (
                StatusCode::BAD_REQUEST,
                ErrorEnvelope::simple(ErrorCode::ValidationError, e.to_string()),
            ),
            Self::Engine(EngineError::ResultTooLarge(n)) => (
                StatusCode::BAD_REQUEST,
                ErrorEnvelope::simple(
                    ErrorCode::ResultTooLarge,
                    format!("result exceeded {n} row limit"),
                ),
            ),
            // Database/IO errors are server-side — don't leak details.
            Self::Engine(EngineError::Database(_) | EngineError::Io(_)) => (
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
                | StoreError::InvalidName { .. } => (
                    StatusCode::BAD_REQUEST,
                    ErrorEnvelope::simple(ErrorCode::BadRequest, e.to_string()),
                ),
            },
            Self::Unauthorized(msg) => (
                StatusCode::UNAUTHORIZED,
                ErrorEnvelope::simple(ErrorCode::Unauthorized, msg.clone()),
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
            Self::Internal(_) => {
                tracing::error!(event_type = "internal_error", error = %self, "internal server error");
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

    #[test]
    fn unauthorized_maps_to_401() {
        let err = ServerError::Unauthorized("missing token".into());
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
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
}
