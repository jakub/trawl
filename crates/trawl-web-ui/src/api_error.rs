// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The failure type every `api` call answers with, and the one
//! classifier that reads a failed answer into it ([`http_failure`],
//! ADR-0066).
//!
//! The transport in `api` is wasm-only; this module is not, so the
//! classifier and the query-error classification run under plain
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

    /// Any 401: the session failed, not the request. The envelope that
    /// came with it, when there was a valid one, is kept, but it never
    /// changes the classification and never becomes the display.
    #[error("unauthorized")]
    Unauthorized {
        /// The server's envelope, unabridged. Boxed so every result that
        /// can carry this error stays small.
        envelope: Option<Box<ErrorEnvelope>>,
    },

    /// A failed answer whose body was absent, unreadable, or not the
    /// envelope: an HTML error page, a legacy body, a code this build
    /// does not know. Only the status is kept; raw body text is never
    /// stored or shown. [`http_failure`] is the only producer.
    #[error("server returned {0}")]
    Status(u16),

    /// A failed answer that carried the server's envelope and is none of
    /// the classifications below. The whole envelope is kept: code,
    /// message, details and conflict. Display is the message, which
    /// trawld writes with its internals redacted (ADR-0040) or trawl-web
    /// fixes, and which the SPA renders as escaped text.
    #[error("{}", envelope.message)]
    Server {
        /// The HTTP status the envelope came with.
        status: u16,
        /// The server's envelope, unabridged. Boxed so every result that
        /// can carry this error stays small.
        envelope: Box<ErrorEnvelope>,
    },

    /// The server refused the query text itself: a [query
    /// error](Kept::Query). The whole envelope rides along, `details`
    /// and their byte spans included, because the query error notice
    /// quotes the sent text under each span (ADR-0039). Display is the
    /// summary alone, the same text [`ApiError::Server`] would show, so a
    /// caller that only prints the error reads what it read before.
    #[error("{}", envelope.message)]
    Query {
        /// The HTTP status the envelope came with.
        status: u16,
        /// The server's envelope, unabridged. Boxed so every result that
        /// can carry this error stays small.
        envelope: Box<ErrorEnvelope>,
    },

    /// The server refused a read because the events it has stored cannot
    /// yet be counted exactly once: it is still loading data from before
    /// a restart, or finishing an interrupted storage rollup
    /// (`corpus_recovering`, ADR-0041). The message is the server's fixed
    /// sentence for the reason, with no counts or paths, so it renders
    /// verbatim. The search page names this state rather than reading it
    /// as an empty result or a generic failure.
    #[error("{}", envelope.message)]
    Recovering {
        /// The HTTP status the refusal came with.
        status: u16,
        /// The server's envelope, unabridged.
        envelope: Box<ErrorEnvelope>,
    },

    /// The server ran the query and it used up the memory or scratch space
    /// trawld gives queries (`query_resource_exhausted`, ADR-0056). The
    /// server is healthy and the query text is fine, so this is neither a
    /// [query error](Kept::Query) nor a retryable failure: running it again
    /// as sent exhausts the same budget. The message is the server's fixed
    /// sentence, with no path, size or engine text, so it renders verbatim.
    #[error("{}", envelope.message)]
    ResourceExhausted {
        /// The HTTP status the refusal came with.
        status: u16,
        /// The server's envelope, unabridged.
        envelope: Box<ErrorEnvelope>,
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
            Self::Status(status)
            | Self::Server { status, .. }
            | Self::Query { status, .. }
            | Self::Recovering { status, .. }
            | Self::ResourceExhausted { status, .. } => Some(*status),
            // The one status this enum spells as a word rather than a
            // number.
            Self::Unauthorized { .. } => Some(401),
            // A refusal never reached the network, so its fate is not
            // unknown — but it carries no status either.
            Self::Network(_) | Self::Decode(_) | Self::Refused(_) => None,
        }
    }

    /// The server's envelope, whatever this failure was classified as,
    /// when the answer carried a valid one.
    #[must_use]
    pub fn envelope(&self) -> Option<&ErrorEnvelope> {
        match self {
            Self::Unauthorized { envelope } => envelope.as_deref(),
            Self::Server { envelope, .. }
            | Self::Query { envelope, .. }
            | Self::Recovering { envelope, .. }
            | Self::ResourceExhausted { envelope, .. } => Some(envelope),
            Self::Network(_) | Self::Status(_) | Self::Decode(_) | Self::Refused(_) => None,
        }
    }

    /// The envelope's code, when the answer carried one. Copy that keys
    /// on a refusal keys on this and the status, never on the message.
    #[must_use]
    pub fn code(&self) -> Option<&ErrorCode> {
        self.envelope().map(|envelope| &envelope.code)
    }

    /// The server's envelope when this failure is a query error, else
    /// `None`.
    #[must_use]
    pub fn query_error(&self) -> Option<&ErrorEnvelope> {
        match self {
            Self::Query { envelope, .. } => Some(envelope.as_ref()),
            _ => None,
        }
    }

    /// The server's sentence when this failure is a `corpus_recovering`
    /// refusal, else `None`.
    #[must_use]
    pub fn recovering(&self) -> Option<&str> {
        match self {
            Self::Recovering { envelope, .. } => Some(&envelope.message),
            _ => None,
        }
    }

    /// The server's sentence when this failure is a
    /// `query_resource_exhausted` refusal, else `None`.
    #[must_use]
    pub fn resource_exhausted(&self) -> Option<&str> {
        match self {
            Self::ResourceExhausted { envelope, .. } => Some(&envelope.message),
            _ => None,
        }
    }
}

/// How [`http_failure`] classifies an envelope with a given code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kept {
    /// The query text itself is wrong, which is what a query error is
    /// (ADR-0039): the server read the text and refused it before
    /// running anything.
    Query,
    /// The corpus is not settled after a restart or an interrupted
    /// rollup (ADR-0041). Nothing is wrong with the query or the caller,
    /// and the same request can succeed once the server catches up.
    Recovering,
    /// The query used up the memory or scratch space trawld gives queries
    /// (ADR-0056). The server is up and the text is fine, so the underline,
    /// the automatic retry and the empty-result claim all stay off, and the
    /// server's sentence is shown as a notice.
    ResourceExhausted,
    /// About the run, the caller or the server: [`ApiError::Server`],
    /// which takes the generic failure copy with its Retry.
    Server,
}

/// Classify an error code for [`http_failure`].
///
/// No wildcard arm: a code added to the wire enum has to be classified
/// here before this crate compiles again.
#[must_use]
pub fn kept_as(code: &ErrorCode) -> Kept {
    match code {
        ErrorCode::ParseError | ErrorCode::ValidationError => Kept::Query,
        ErrorCode::CorpusRecovering => Kept::Recovering,
        ErrorCode::QueryResourceExhausted => Kept::ResourceExhausted,
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
        | ErrorCode::ServiceUnavailable
        | ErrorCode::HotBufferFull
        | ErrorCode::IngestBatchTooLarge
        | ErrorCode::PreviewTooLarge
        | ErrorCode::RequestTooLarge
        | ErrorCode::UnsupportedEncoding
        | ErrorCode::RequestLimitReached
        | ErrorCode::ResultGone
        | ErrorCode::ResultUnavailable
        | ErrorCode::ResultCorrupt
        | ErrorCode::CaptureBusy
        | ErrorCode::CaptureQuotaExceeded
        | ErrorCode::CaptureSourceTooLarge
        | ErrorCode::FocusIncomplete
        | ErrorCode::FocusAmbiguous
        | ErrorCode::WindowsOverlap
        | ErrorCode::CorrelationInvalid
        | ErrorCode::SequenceConflict
        | ErrorCode::InvestigationExists
        | ErrorCode::InvestigationClosed
        | ErrorCode::EvidenceBudgetExceeded
        | ErrorCode::InvalidName
        | ErrorCode::DuplicateName => Kept::Server,
    }
}

/// Read a failed answer: `status`, and its `body` when one was read
/// (`None` is a body that was absent or could not be read).
///
/// A 401 is [`ApiError::Unauthorized`] before anything else is looked
/// at, and keeps a valid envelope without being changed by it. Any
/// other status with a valid envelope keeps the whole envelope, as a
/// [query error](ApiError::Query), a [recovering](ApiError::Recovering)
/// or [resource](ApiError::ResourceExhausted) refusal, or else
/// [`ApiError::Server`]. A body that is absent, unreadable, not the
/// envelope, or an envelope with a code this build does not know is the
/// bare [`ApiError::Status`].
#[must_use]
pub fn http_failure(status: u16, body: Option<&str>) -> ApiError {
    let envelope = body
        .and_then(|body| serde_json::from_str::<ErrorResponse>(body).ok())
        .map(|ErrorResponse { error }| Box::new(error));
    if status == 401 {
        return ApiError::Unauthorized { envelope };
    }
    let Some(envelope) = envelope else {
        return ApiError::Status(status);
    };
    match kept_as(&envelope.code) {
        Kept::Query => ApiError::Query { status, envelope },
        Kept::Recovering => ApiError::Recovering { status, envelope },
        Kept::ResourceExhausted => ApiError::ResourceExhausted { status, envelope },
        Kept::Server => ApiError::Server { status, envelope },
    }
}

/// The net-name form's sentence for a name trawld refuses as blank or
/// as carrying control or invisible formatting characters.
const INVALID_NAME: &str =
    "Name must not be blank or contain control or invisible formatting characters.";

/// The net-name form's sentence for a name another net already holds.
const DUPLICATE_NAME: &str = "A net with this name already exists. Choose another name.";

/// What the net-name form says about a failed create or rename.
///
/// Fixed copy keys on the pair of status and code, never on the
/// message (ADR-0066): (400, `invalid_name`) and (409,
/// `duplicate_name`) get the form's guidance whatever trawld's wording
/// is. Every other failure gets a generic sentence that quotes nothing
/// the server or the browser wrote.
#[must_use]
pub fn saved_name_refusal(error: &ApiError) -> String {
    match (error.http_status(), error.code()) {
        (Some(400), Some(ErrorCode::InvalidName)) => return INVALID_NAME.to_owned(),
        (Some(409), Some(ErrorCode::DuplicateName)) => return DUPLICATE_NAME.to_owned(),
        _ => {}
    }
    match error {
        ApiError::Network(_) => {
            "Could not reach the server, so the net may not have been saved.".to_owned()
        }
        ApiError::Decode(_) => {
            "Could not read the server's answer. Reload to see whether the net was saved."
                .to_owned()
        }
        // Both are this client's own fixed sentences.
        ApiError::Unauthorized { .. } | ApiError::Refused(_) => error.to_string(),
        ApiError::Status(status)
        | ApiError::Server { status, .. }
        | ApiError::Query { status, .. }
        | ApiError::Recovering { status, .. }
        | ApiError::ResourceExhausted { status, .. } => {
            format!("Could not save the net (server returned {status}).")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ApiError, DUPLICATE_NAME, INVALID_NAME, Kept, http_failure, kept_as, saved_name_refusal,
    };
    use trawl_api::investigations::SequenceConflict;
    use trawl_api::{ErrorCode, ErrorDetail, ErrorEnvelope, ErrorResponse, ErrorSpan};

    /// Every wire code, spelled out. The match in `kept_as` has no
    /// wildcard, so a new code already fails to compile there; this list
    /// makes the test say how each existing code is kept.
    const EVERY_CODE: [(ErrorCode, Kept); 38] = [
        (ErrorCode::ParseError, Kept::Query),
        (ErrorCode::ValidationError, Kept::Query),
        (ErrorCode::ExecutionError, Kept::Server),
        (ErrorCode::ResultTooLarge, Kept::Server),
        (ErrorCode::AuthError, Kept::Server),
        (ErrorCode::Forbidden, Kept::Server),
        (ErrorCode::BadRequest, Kept::Server),
        (ErrorCode::NotFound, Kept::Server),
        (ErrorCode::Timeout, Kept::Server),
        (ErrorCode::IngestError, Kept::Server),
        (ErrorCode::RateLimited, Kept::Server),
        (ErrorCode::TooManyStreams, Kept::Server),
        (ErrorCode::InternalError, Kept::Server),
        (ErrorCode::ServiceUnavailable, Kept::Server),
        (ErrorCode::HotBufferFull, Kept::Server),
        (ErrorCode::IngestBatchTooLarge, Kept::Server),
        (ErrorCode::CorpusRecovering, Kept::Recovering),
        (ErrorCode::PreviewTooLarge, Kept::Server),
        (ErrorCode::RequestTooLarge, Kept::Server),
        (ErrorCode::UnsupportedEncoding, Kept::Server),
        (ErrorCode::RequestLimitReached, Kept::Server),
        (ErrorCode::QueryResourceExhausted, Kept::ResourceExhausted),
        (ErrorCode::ResultGone, Kept::Server),
        (ErrorCode::ResultUnavailable, Kept::Server),
        (ErrorCode::ResultCorrupt, Kept::Server),
        (ErrorCode::CaptureBusy, Kept::Server),
        (ErrorCode::CaptureQuotaExceeded, Kept::Server),
        (ErrorCode::CaptureSourceTooLarge, Kept::Server),
        (ErrorCode::FocusIncomplete, Kept::Server),
        (ErrorCode::FocusAmbiguous, Kept::Server),
        (ErrorCode::WindowsOverlap, Kept::Server),
        (ErrorCode::CorrelationInvalid, Kept::Server),
        (ErrorCode::SequenceConflict, Kept::Server),
        (ErrorCode::InvestigationExists, Kept::Server),
        (ErrorCode::InvestigationClosed, Kept::Server),
        (ErrorCode::EvidenceBudgetExceeded, Kept::Server),
        (ErrorCode::InvalidName, Kept::Server),
        (ErrorCode::DuplicateName, Kept::Server),
    ];

    #[test]
    fn every_code_is_kept_as_its_class() {
        for (code, expected) in EVERY_CODE {
            assert_eq!(kept_as(&code), expected, "{code:?}");
        }
    }

    /// An envelope with every optional part filled: a detail with a span,
    /// label and hint, and the conflict pair.
    fn full_envelope(code: ErrorCode, message: &str) -> ErrorEnvelope {
        ErrorEnvelope {
            code,
            message: message.to_owned(),
            details: vec![ErrorDetail {
                message: "the detail".to_owned(),
                span: Some(ErrorSpan { start: 3, end: 7 }),
                label: Some("stats".to_owned()),
                hint: Some("the hint".to_owned()),
            }],
            conflict: Some(SequenceConflict {
                expected: 4,
                current: 9,
            }),
        }
    }

    fn body_of(envelope: &ErrorEnvelope) -> String {
        serde_json::to_string(&ErrorResponse {
            error: envelope.clone(),
        })
        .expect("an envelope serializes")
    }

    /// The envelope an error kept, compared with the one that was sent
    /// field by field through its wire form.
    fn assert_kept_whole(err: &ApiError, sent: &ErrorEnvelope) {
        let kept = err.envelope().expect("the envelope is kept");
        assert_eq!(
            serde_json::to_value(kept).expect("serializes"),
            serde_json::to_value(sent).expect("serializes"),
            "{err:?}"
        );
        assert_eq!(err.code(), Some(&sent.code));
    }

    /// The classifier (ADR-0066): a 401 is the session whatever its body
    /// says, any other valid envelope is kept whole under its class, and
    /// a body that is not the envelope is the bare status.
    #[test]
    fn http_failure_preserves_envelope_and_classification() {
        // 401: an envelope is kept but changes nothing; garbage or no
        // body is the same classification with no envelope.
        let auth = full_envelope(ErrorCode::AuthError, "session expired");
        let err = http_failure(401, Some(&body_of(&auth)));
        assert!(matches!(err, ApiError::Unauthorized { .. }), "{err:?}");
        assert_kept_whole(&err, &auth);
        assert_eq!(err.http_status(), Some(401));
        assert_eq!(err.to_string(), "unauthorized");
        // Even a code that would otherwise be a query error.
        let parse = full_envelope(ErrorCode::ParseError, "bad");
        assert!(matches!(
            http_failure(401, Some(&body_of(&parse))),
            ApiError::Unauthorized { .. }
        ));
        for body in [Some("<html>401</html>"), Some(r#"{"error":"nope"}"#), None] {
            let err = http_failure(401, body);
            assert!(
                matches!(err, ApiError::Unauthorized { envelope: None }),
                "{body:?}: {err:?}"
            );
            assert_eq!(err.http_status(), Some(401));
        }

        // 403 with an envelope: a server refusal, every part of it kept.
        let forbidden = full_envelope(ErrorCode::Forbidden, "cross-origin request rejected");
        let err = http_failure(403, Some(&body_of(&forbidden)));
        assert!(
            matches!(err, ApiError::Server { status: 403, .. }),
            "{err:?}"
        );
        assert_kept_whole(&err, &forbidden);
        assert_eq!(err.to_string(), "cross-origin request rejected");
        let kept = err.envelope().expect("kept");
        assert_eq!(kept.details.len(), 1);
        assert_eq!(kept.details[0].label.as_deref(), Some("stats"));
        assert_eq!(
            kept.conflict,
            Some(SequenceConflict {
                expected: 4,
                current: 9
            })
        );

        // No envelope: an HTML error page, no body at all, a legacy
        // body, and a code this build does not know.
        for (status, body) in [
            (500, Some("<html><body>Internal Server Error</body></html>")),
            (502, None),
            (502, Some("")),
            (400, Some(r#"{"error":"bad request"}"#)),
            (
                503,
                Some(r#"{"error":{"code":"brand_new_code","message":"nope"}}"#),
            ),
        ] {
            let err = http_failure(status, body);
            assert!(
                matches!(err, ApiError::Status(s) if s == status),
                "{status} {body:?}: {err:?}"
            );
            assert!(err.envelope().is_none());
            assert_eq!(err.to_string(), format!("server returned {status}"));
        }

        // The query error path (ADR-0039) and the two named refusals keep
        // their classes, with the whole envelope.
        for code in [ErrorCode::ParseError, ErrorCode::ValidationError] {
            let sent = full_envelope(code, "the query is wrong");
            let err = http_failure(400, Some(&body_of(&sent)));
            assert!(
                matches!(err, ApiError::Query { status: 400, .. }),
                "{err:?}"
            );
            assert_kept_whole(&err, &sent);
            assert!(err.query_error().is_some());
        }
        let sent = full_envelope(ErrorCode::CorpusRecovering, "still loading");
        let err = http_failure(503, Some(&body_of(&sent)));
        assert!(
            matches!(err, ApiError::Recovering { status: 503, .. }),
            "{err:?}"
        );
        assert_kept_whole(&err, &sent);
        assert_eq!(err.recovering(), Some("still loading"));
        let sent = full_envelope(ErrorCode::QueryResourceExhausted, "out of room");
        let err = http_failure(503, Some(&body_of(&sent)));
        assert!(
            matches!(err, ApiError::ResourceExhausted { status: 503, .. }),
            "{err:?}"
        );
        assert_kept_whole(&err, &sent);
        assert_eq!(err.resource_exhausted(), Some("out of room"));
    }

    /// The server's fixed `query_resource_exhausted` sentence is kept as its
    /// own failure: not a query error (no spans, no underline), not the
    /// generic server error, and not a recovering refusal.
    #[test]
    fn a_resource_exhausted_refusal_keeps_its_message_as_its_own_failure() {
        let message = "query could not complete with the available query resources; narrow the \
                       time range or simplify the query";
        let body = format!(
            r#"{{"error":{{"code":"query_resource_exhausted","message":"{message}","details":[{{"message":"x","span":{{"start":0,"end":1}}}}]}}}}"#
        );
        let err = http_failure(503, Some(&body));
        assert!(
            matches!(&err, ApiError::ResourceExhausted { status: 503, .. }),
            "{err:?}"
        );
        assert_eq!(err.http_status(), Some(503));
        assert!(err.query_error().is_none());
        assert!(err.recovering().is_none());
        assert_eq!(err.resource_exhausted(), Some(message));
        assert_eq!(err.to_string(), message);
    }

    /// Only the `query_resource_exhausted` refusal answers with its
    /// sentence: a recovering refusal, a generic 503 and a query error do
    /// not, so the search page's notice never mounts for them.
    #[test]
    fn only_the_resource_exhausted_refusal_reads_as_one() {
        for body in [
            r#"{"error":{"code":"corpus_recovering","message":"recovering"}}"#,
            r#"{"error":{"code":"service_unavailable","message":"service unavailable"}}"#,
            r#"{"error":{"code":"parse_error","message":"bad","details":[]}}"#,
        ] {
            let err = http_failure(503, Some(body));
            assert_eq!(err.resource_exhausted(), None, "{body}");
        }
    }

    /// Both of the server's fixed `corpus_recovering` sentences, as it
    /// writes them. Neither is a query error, and neither is folded into
    /// the generic server error: the search page names the state.
    #[test]
    fn a_corpus_recovering_refusal_keeps_its_message_as_recovering() {
        for message in [
            "Search is unavailable while the server finishes loading data from before its restart.",
            "Search is unavailable while the server finishes an interrupted storage rollup.",
        ] {
            let body =
                format!(r#"{{"error":{{"code":"corpus_recovering","message":"{message}"}}}}"#);
            let err = http_failure(503, Some(&body));
            assert!(
                matches!(&err, ApiError::Recovering { status: 503, .. }),
                "{err:?}"
            );
            assert_eq!(err.http_status(), Some(503));
            assert_eq!(err.recovering(), Some(message));
            assert!(err.query_error().is_none());
            assert_eq!(err.to_string(), message);
        }
    }

    /// Only `corpus_recovering` reads as recovering. A plain 503, the
    /// generic `service_unavailable` a hot snapshot I/O failure answers,
    /// stays a server error with its Retry.
    #[test]
    fn service_unavailable_is_not_recovering() {
        let body = r#"{"error":{"code":"service_unavailable","message":"service unavailable"}}"#;
        let err = http_failure(503, Some(body));
        assert!(
            matches!(err, ApiError::Server { status: 503, .. }),
            "{err:?}"
        );
        assert_eq!(err.recovering(), None);
        for err in [
            http_failure(503, None),
            http_failure(401, None),
            ApiError::Network("offline".to_owned()),
        ] {
            assert_eq!(err.recovering(), None, "{err:?}");
        }
    }

    /// The server's own parse-error answer for the issue's sample query,
    /// byte for byte, keeps every field of its detail.
    #[test]
    fn a_parse_error_keeps_its_whole_envelope() {
        let body = r#"{"error":{"code":"parse_error","message":"found 'h', expected '(' or ')'","details":[{"message":"found 'h', expected '(' or ')'","span":{"start":43,"end":44},"label":"stats","hint":"close the call"}]}}"#;
        let err = http_failure(400, Some(body));
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
        let err = http_failure(400, Some(body));
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
        let envelope = http_failure(400, Some(body))
            .query_error()
            .cloned()
            .expect("a validation error is a query error");
        assert!(envelope.details.is_empty());
    }

    /// Any other envelope is a server error that shows its message.
    #[test]
    fn another_envelope_is_a_server_error_with_its_message() {
        let body = r#"{"error":{"code":"execution_error","message":"query execution failed"}}"#;
        let err = http_failure(500, Some(body));
        assert!(
            matches!(&err, ApiError::Server { status: 500, .. }),
            "{err:?}"
        );
        assert_eq!(err.code(), Some(&ErrorCode::ExecutionError));
        assert!(err.query_error().is_none());
        assert_eq!(err.to_string(), "query execution failed");
    }

    /// A body that is not the envelope, including one whose code is not
    /// on the wire list, falls back to the bare status.
    #[test]
    fn a_non_envelope_body_falls_back_to_the_status() {
        for body in [
            None,
            Some(""),
            Some("<html>502 Bad Gateway</html>"),
            Some(r#"{"error":"failed"}"#),
            Some(r#"{"error":{"code":"unavailable","message":"nope"}}"#),
        ] {
            let err = http_failure(500, body);
            assert!(matches!(err, ApiError::Status(500)), "{body:?}: {err:?}");
            assert_eq!(err.to_string(), "server returned 500");
        }
    }

    fn refusal(status: u16, code: ErrorCode, message: &str) -> ApiError {
        http_failure(
            status,
            Some(&body_of(&ErrorEnvelope::simple(code, message))),
        )
    }

    /// The form's guidance keys on the pair of status and code, whatever
    /// the message says.
    #[test]
    fn saved_name_refusal_gives_fixed_copy_for_the_two_name_pairs() {
        for message in [
            "name must not be blank or contain control or invisible formatting characters",
            "a reworded sentence",
            "",
        ] {
            assert_eq!(
                saved_name_refusal(&refusal(400, ErrorCode::InvalidName, message)),
                INVALID_NAME,
                "{message:?}"
            );
            assert_eq!(
                saved_name_refusal(&refusal(409, ErrorCode::DuplicateName, message)),
                DUPLICATE_NAME,
                "{message:?}"
            );
        }
    }

    /// A mismatched pair, trawld's old `bad_request` answers with their
    /// exact text, and any other failure take the generic copy, which
    /// quotes nothing the server sent.
    #[test]
    fn saved_name_refusal_is_generic_for_everything_else() {
        let hostile = "<img src=x onerror=alert(1)> /var/lib/trawl/secret";
        let cases = [
            refusal(409, ErrorCode::InvalidName, hostile),
            refusal(400, ErrorCode::DuplicateName, hostile),
            refusal(
                400,
                ErrorCode::BadRequest,
                "name must not be blank or contain control or invisible formatting characters",
            ),
            refusal(
                409,
                ErrorCode::BadRequest,
                "a saved query with this name already exists",
            ),
            refusal(500, ErrorCode::InternalError, hostile),
            refusal(403, ErrorCode::Forbidden, hostile),
            refusal(400, ErrorCode::ParseError, hostile),
            http_failure(502, Some("<html>bad gateway</html>")),
            http_failure(503, None),
        ];
        for err in &cases {
            let status = err.http_status().expect("each case answered");
            let copy = saved_name_refusal(err);
            assert_eq!(
                copy,
                format!("Could not save the net (server returned {status})."),
                "{err:?}"
            );
            assert_ne!(copy, INVALID_NAME);
            assert_ne!(copy, DUPLICATE_NAME);
        }

        let session = refusal(401, ErrorCode::AuthError, hostile);
        assert_eq!(saved_name_refusal(&session), "unauthorized");
        for (err, gone) in [
            (ApiError::Network(hostile.to_owned()), hostile),
            (ApiError::Decode(hostile.to_owned()), hostile),
        ] {
            let copy = saved_name_refusal(&err);
            assert!(!copy.contains(gone), "{copy}");
            assert!(!copy.contains("onerror"), "{copy}");
        }
    }
}
