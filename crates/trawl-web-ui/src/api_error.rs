// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The failure type every `api` call answers with, and the pure half of
//! reading a non-2xx body into it.
//!
//! The transport in `api` is wasm-only; this module is not, so the body
//! decoder and the query-error classification run under plain
//! `cargo test`. `api` re-exports [`ApiError`] and adds the one
//! conversion that needs the browser, from `gloo_net::Error`.
//!
//! Only the wasm32 build consumes these items outside the tests.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use trawl_api::{ErrorCode, ErrorEnvelope, ErrorResponse};

#[derive(Debug, Clone, thiserror::Error)]
pub enum ApiError {
    #[error("network: {0}")]
    Network(String),

    #[error("unauthorized")]
    Unauthorized,

    #[error("server returned {0}")]
    Status(u16),

    /// A non-2xx whose body carried the server's own error envelope. The
    /// message is server-written and safe by construction (trawld's
    /// envelope never quotes DSL or generated SQL), and the repin modal
    /// renders it verbatim rather than inventing copy for a 400/403/503
    /// it cannot classify.
    #[error("{message}")]
    Server {
        /// The HTTP status the message came with.
        status: u16,
        /// The envelope's human-readable summary.
        message: String,
    },

    /// The server refused the query text itself: a [query
    /// error](is_query_error). The whole envelope rides along, `details`
    /// and their byte spans included, because the query error notice
    /// quotes the sent text under each span (ADR-0039). Display is the
    /// summary alone, the same text [`ApiError::Server`] would show, so a
    /// caller that only prints the error reads what it read before.
    #[error("{}", envelope.message)]
    Query {
        /// The HTTP status the envelope came with.
        status: u16,
        /// The server's envelope, unabridged.
        envelope: ErrorEnvelope,
    },

    #[error("decode: {0}")]
    Decode(String),

    /// The client refused to send the request at all — nothing left the
    /// browser. The empty query is the case that matters: the server
    /// reads it as every row (ADR-0027), so a door that would post it
    /// answers this instead.
    #[error("{0}")]
    Refused(&'static str),
}

impl ApiError {
    /// The HTTP status this failure carries, when it carries one at all.
    ///
    /// `None` is not a status class: a network or decode failure means
    /// the request's fate is unknown, which is exactly what callers who
    /// branch on definitiveness (`repin_flow::is_pre_claim_failure`)
    /// have to tell apart from a server that answered.
    #[must_use]
    pub fn http_status(&self) -> Option<u16> {
        match self {
            Self::Status(status) | Self::Server { status, .. } | Self::Query { status, .. } => {
                Some(*status)
            }
            // The one status this enum spells as a word rather than a
            // number.
            Self::Unauthorized => Some(401),
            // A refusal never reached the network, so its fate is not
            // unknown — but it carries no status either.
            Self::Network(_) | Self::Decode(_) | Self::Refused(_) => None,
        }
    }

    /// The server's envelope when this failure is a query error, else
    /// `None`.
    #[must_use]
    pub fn query_error(&self) -> Option<&ErrorEnvelope> {
        match self {
            Self::Query { envelope, .. } => Some(envelope),
            _ => None,
        }
    }
}

/// Whether an error code says the query text itself is wrong, which
/// is what a query error is (ADR-0039): the server read the text and
/// refused it before running anything. Every other code is about the
/// run, the caller or the server, and keeps the generic failure copy
/// with its Retry.
///
/// No wildcard arm: a code added to the wire enum has to be classified
/// here before this crate compiles again.
#[must_use]
pub fn is_query_error(code: &ErrorCode) -> bool {
    match code {
        ErrorCode::ParseError | ErrorCode::ValidationError => true,
        ErrorCode::ExecutionError
        | ErrorCode::ResultTooLarge
        | ErrorCode::AuthError
        | ErrorCode::Forbidden
        | ErrorCode::BadRequest
        | ErrorCode::NotFound
        | ErrorCode::Timeout
        | ErrorCode::IngestError
        | ErrorCode::RateLimited
        | ErrorCode::TooManyStreams
        | ErrorCode::InternalError
        | ErrorCode::ServiceUnavailable => false,
    }
}

/// Read a non-2xx body. A query error keeps its whole envelope
/// ([`ApiError::Query`]); any other envelope keeps its summary
/// ([`ApiError::Server`]); a body that is not the envelope at all falls
/// back to the bare status.
#[must_use]
pub fn decode_error_body(status: u16, body: &str) -> ApiError {
    match serde_json::from_str::<ErrorResponse>(body) {
        Ok(ErrorResponse { error }) if is_query_error(&error.code) => ApiError::Query {
            status,
            envelope: error,
        },
        Ok(ErrorResponse { error }) => ApiError::Server {
            status,
            message: error.message,
        },
        Err(_) => ApiError::Status(status),
    }
}

#[cfg(test)]
mod tests {
    use super::{ApiError, decode_error_body, is_query_error};
    use trawl_api::ErrorCode;

    /// Every wire code, spelled out. The match in `is_query_error` has no
    /// wildcard, so a new code already fails to compile there; this list
    /// makes the test say which way each existing code is classified.
    const EVERY_CODE: [(ErrorCode, bool); 14] = [
        (ErrorCode::ParseError, true),
        (ErrorCode::ValidationError, true),
        (ErrorCode::ExecutionError, false),
        (ErrorCode::ResultTooLarge, false),
        (ErrorCode::AuthError, false),
        (ErrorCode::Forbidden, false),
        (ErrorCode::BadRequest, false),
        (ErrorCode::NotFound, false),
        (ErrorCode::Timeout, false),
        (ErrorCode::IngestError, false),
        (ErrorCode::RateLimited, false),
        (ErrorCode::TooManyStreams, false),
        (ErrorCode::InternalError, false),
        (ErrorCode::ServiceUnavailable, false),
    ];

    #[test]
    fn only_parse_and_validation_errors_are_query_errors() {
        for (code, expected) in EVERY_CODE {
            assert_eq!(is_query_error(&code), expected, "{code:?}");
        }
    }

    /// The server's own parse-error answer for the issue's sample query,
    /// byte for byte, keeps every field of its detail.
    #[test]
    fn a_parse_error_keeps_its_whole_envelope() {
        let body = r#"{"error":{"code":"parse_error","message":"found 'h', expected '(' or ')'","details":[{"message":"found 'h', expected '(' or ')'","span":{"start":43,"end":44},"label":"stats","hint":"close the call"}]}}"#;
        let err = decode_error_body(400, body);
        assert_eq!(err.http_status(), Some(400));
        assert_eq!(err.to_string(), "found 'h', expected '(' or ')'");
        let envelope = err.query_error().expect("a parse error is a query error");
        assert_eq!(envelope.code, ErrorCode::ParseError);
        assert_eq!(envelope.details.len(), 1);
        let detail = &envelope.details[0];
        assert_eq!(detail.message, "found 'h', expected '(' or ')'");
        let span = detail.span.as_ref().expect("the span survives");
        assert_eq!((span.start, span.end), (43, 44));
        assert_eq!(detail.label.as_deref(), Some("stats"));
        assert_eq!(detail.hint.as_deref(), Some("close the call"));
    }

    #[test]
    fn a_validation_error_keeps_its_spanless_detail() {
        let body = r#"{"error":{"code":"validation_error","message":"unknown function: countt (did you mean 'count'?)","details":[{"message":"unknown function: countt","hint":"did you mean 'count'?"}]}}"#;
        let err = decode_error_body(400, body);
        let envelope = err
            .query_error()
            .expect("a validation error is a query error");
        assert_eq!(envelope.code, ErrorCode::ValidationError);
        assert_eq!(envelope.details[0].message, "unknown function: countt");
        assert!(envelope.details[0].span.is_none());
        assert_eq!(
            envelope.details[0].hint.as_deref(),
            Some("did you mean 'count'?")
        );
    }

    /// A validation error with no details at all (a refusal found while
    /// the query ran) is still a query error, with an empty list.
    #[test]
    fn a_detail_free_validation_error_is_still_a_query_error() {
        let body = r#"{"error":{"code":"validation_error","message":"timechart on 'x' is not a timestamp: VARCHAR"}}"#;
        let envelope = decode_error_body(400, body)
            .query_error()
            .cloned()
            .expect("a validation error is a query error");
        assert!(envelope.details.is_empty());
    }

    /// Any other envelope keeps only its summary, as it always has.
    #[test]
    fn another_envelope_is_a_server_error_with_its_summary() {
        let body = r#"{"error":{"code":"execution_error","message":"query execution failed"}}"#;
        let err = decode_error_body(500, body);
        assert!(
            matches!(&err, ApiError::Server { status: 500, message } if message == "query execution failed"),
            "{err:?}"
        );
        assert!(err.query_error().is_none());
        assert_eq!(err.to_string(), "query execution failed");
    }

    /// A body that is not the envelope, including one whose code is not
    /// on the wire list, falls back to the bare status.
    #[test]
    fn a_non_envelope_body_falls_back_to_the_status() {
        for body in [
            "",
            "<html>502 Bad Gateway</html>",
            r#"{"error":"failed"}"#,
            r#"{"error":{"code":"unavailable","message":"nope"}}"#,
        ] {
            let err = decode_error_body(500, body);
            assert!(matches!(err, ApiError::Status(500)), "{body:?}: {err:?}");
            assert_eq!(err.to_string(), "server returned 500");
        }
    }
}
