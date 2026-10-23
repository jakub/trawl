// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Client error types.

use std::fmt;

use trawl_api::{ErrorCode, ErrorDetail, ErrorEnvelope};

/// Errors from the trawl client library.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// HTTP transport failure.
    #[error(
        "network error: {0}{suffix}",
        suffix = RequestSuffix(.0.request_id.as_deref())
    )]
    Network(NetworkError),

    /// Server returned a non-success status with structured error info.
    #[error(
        "server error (HTTP {status}): {}{}",
        error.message,
        RequestSuffix(request_id.as_deref())
    )]
    Server {
        /// HTTP status code.
        status: u16,
        /// Structured error envelope from the server.
        error: ErrorEnvelope,
        /// The response's `X-Request-Id`, when it carried one.
        request_id: Option<String>,
        /// The response's `Retry-After` in seconds, when it carried the
        /// numeric form. The HTTP-date form, a value past `u32::MAX`, or
        /// anything else, is `None`. A `u32`, so the error stays small
        /// enough to return by value.
        retry_after_secs: Option<u32>,
    },

    /// Failed to parse the server's response.
    #[error(
        "response parse error: {message}{}",
        RequestSuffix(request_id.as_deref())
    )]
    Parse {
        /// What did not parse. Never quotes the body.
        message: String,
        /// The response's `X-Request-Id`, when the failure came from a
        /// response that carried one. `None` for a failure with no
        /// response behind it, such as a stream frame or a request body
        /// that did not serialize.
        request_id: Option<String>,
    },

    /// A pinned CA bundle holds no usable certificate. The reason never
    /// quotes the bundle's bytes.
    #[error("invalid CA certificate: {0}")]
    InvalidCa(String),

    /// A response body larger than the endpoint's cap. Only the first
    /// `cap` bytes were read; the rest never was.
    #[error(
        "response too large: more than {cap} bytes{}",
        RequestSuffix(request_id.as_deref())
    )]
    TooLarge {
        /// The most bytes the endpoint's body may hold.
        cap: usize,
        /// The response's `X-Request-Id`, when it carried one.
        request_id: Option<String>,
    },

    /// A URL the client refuses to build on. The reason never quotes the
    /// URL.
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
}

/// What kind of transport failure a [`NetworkError`] is.
///
/// A typed classification, so a caller can tell an untrusted certificate
/// from a closed port without reading the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkKind {
    /// The TLS handshake failed because the server's certificate did not
    /// verify under the client's trust.
    UntrustedCertificate,
    /// No connection opened: nothing listening, a reset, or a handshake that
    /// failed for a reason other than the certificate.
    Connect,
    /// No response headers arrived within the request's deadline.
    Timeout,
    /// The response headers arrived, so the server answered, but its body
    /// did not finish within the request's deadline.
    BodyTimeout,
    /// The response headers arrived, so the server answered, but reading
    /// its body failed for a reason other than the deadline: the peer
    /// closed before the declared length, or sent a malformed chunk.
    BodyRead,
    /// The server answered with a redirect the client refused to follow.
    Redirect,
    /// The server reset or closed the connection while a request that
    /// carries a body was in flight, and no response was read.
    UploadCutOff,
    /// Any other transport failure.
    Other,
}

/// A transport failure: its kind and a sanitized message.
///
/// The message names the API origin at most, never URL userinfo, paths,
/// query parameters, a certificate, or the TLS library's reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkError {
    /// What kind of failure this is.
    pub kind: NetworkKind,
    /// The sanitized, human-readable message.
    pub message: String,
    /// The `X-Request-Id` of the response whose body failed, when the
    /// failure struck after the headers arrived and they carried one.
    /// `None` for a failure with no response behind it.
    pub request_id: Option<String>,
}

impl NetworkError {
    /// A failure of `kind` described by `message`.
    pub fn new(kind: NetworkKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            request_id: None,
        }
    }

    /// This failure, naming `id` as the request id of the response whose
    /// body it struck.
    #[must_use]
    pub(crate) fn answered_by(mut self, id: Option<String>) -> Self {
        self.request_id = id;
        self
    }
}

impl fmt::Display for NetworkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// Renders ` (request <id>)` after an error's message when the response it
/// came from named a request id, so a CLI user can quote it.
struct RequestSuffix<'a>(Option<&'a str>);

impl fmt::Display for RequestSuffix<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(id) => write!(f, " (request {id})"),
            None => Ok(()),
        }
    }
}

impl ClientError {
    /// A parse failure with no response behind it.
    pub(crate) fn parse(message: impl Into<String>) -> Self {
        Self::Parse {
            message: message.into(),
            request_id: None,
        }
    }

    /// The server's `X-Request-Id` for the response this error came from.
    ///
    /// The id is the server's correlation token for its own logs
    /// (ADR-0040): quoting it lets an operator find the request's failure
    /// event. The client never generates one. Present for every error that
    /// saw response headers carrying it ([`Self::Server`], a response body
    /// that did not parse, [`Self::TooLarge`], and a [`Self::Network`] that
    /// struck the body after the headers arrived); `None` for any other
    /// [`Self::Network`], [`Self::InvalidCa`] and [`Self::InvalidUrl`],
    /// which have no response.
    pub fn request_id(&self) -> Option<&str> {
        match self {
            Self::Server { request_id, .. }
            | Self::Parse { request_id, .. }
            | Self::TooLarge { request_id, .. } => request_id.as_deref(),
            Self::Network(error) => error.request_id.as_deref(),
            Self::InvalidCa(_) | Self::InvalidUrl(_) => None,
        }
    }

    /// This error, naming `id` as its response's request id when it is a
    /// response error that names none yet. For decode steps that only see
    /// a body, so the caller holding the response attaches its id.
    pub(crate) fn answered_by(mut self, id: Option<&str>) -> Self {
        if let Self::Server { request_id, .. }
        | Self::Parse { request_id, .. }
        | Self::TooLarge { request_id, .. } = &mut self
            && request_id.is_none()
        {
            *request_id = id.map(str::to_owned);
        }
        self
    }

    /// The kind of transport failure, if this is a network error.
    pub fn network_kind(&self) -> Option<NetworkKind> {
        match self {
            Self::Network(error) => Some(error.kind),
            _ => None,
        }
    }

    /// Get the structured error envelope (if this is a server error).
    pub fn error_envelope(&self) -> Option<&ErrorEnvelope> {
        match self {
            Self::Server { error, .. } => Some(error),
            _ => None,
        }
    }

    /// Get the error details (spans, diagnostics). Empty for non-server errors.
    pub fn error_details(&self) -> &[ErrorDetail] {
        match self {
            Self::Server { error, .. } => &error.details,
            _ => &[],
        }
    }

    /// Get the machine-readable error code (if this is a server error).
    pub fn error_code(&self) -> Option<&ErrorCode> {
        match self {
            Self::Server { error, .. } => Some(&error.code),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "01JBQ7ZK3M8X2V5N9R4T6W0YCD";

    /// Builds one variant, with or without a request id.
    type Build = fn(Option<&str>) -> ClientError;

    /// Every variant that carries a request id renders it once, as the
    /// ` (request <id>)` suffix, and renders none without one.
    #[test]
    fn display_ends_with_the_request_id_when_there_is_one() {
        let server = |request_id: Option<&str>| ClientError::Server {
            status: 500,
            error: ErrorEnvelope::simple(ErrorCode::ResultCorrupt, "broken"),
            request_id: request_id.map(str::to_owned),
            retry_after_secs: None,
        };
        let parse = |request_id: Option<&str>| ClientError::Parse {
            message: "bad json".to_owned(),
            request_id: request_id.map(str::to_owned),
        };
        let too_large = |request_id: Option<&str>| ClientError::TooLarge {
            cap: 10,
            request_id: request_id.map(str::to_owned),
        };
        let network = |request_id: Option<&str>| {
            ClientError::Network(
                NetworkError::new(NetworkKind::BodyRead, "response body broken")
                    .answered_by(request_id.map(str::to_owned)),
            )
        };
        let cases: [(Build, &str); 4] = [
            (server, "server error (HTTP 500): broken"),
            (parse, "response parse error: bad json"),
            (too_large, "response too large: more than 10 bytes"),
            (network, "network error: response body broken"),
        ];
        for (build, text) in cases {
            assert_eq!(build(None).to_string(), text);
            assert_eq!(
                build(Some(ID)).to_string(),
                format!("{text} (request {ID})")
            );
        }
    }

    /// The variants with no response behind them render no suffix.
    #[test]
    fn display_of_a_response_less_error_has_no_suffix() {
        assert_eq!(
            ClientError::Network(NetworkError::new(NetworkKind::Connect, "refused")).to_string(),
            "network error: refused"
        );
        assert_eq!(
            ClientError::parse("no response").to_string(),
            "response parse error: no response"
        );
        assert_eq!(
            ClientError::InvalidCa("empty".to_owned()).to_string(),
            "invalid CA certificate: empty"
        );
        assert_eq!(
            ClientError::InvalidUrl("userinfo".to_owned()).to_string(),
            "invalid URL: userinfo"
        );
    }
}
