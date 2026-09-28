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
    #[error("network error: {0}")]
    Network(NetworkError),

    /// Server returned a non-success status with structured error info.
    #[error("server error (HTTP {status}): {}", error.message)]
    Server {
        /// HTTP status code.
        status: u16,
        /// Structured error envelope from the server.
        error: ErrorEnvelope,
    },

    /// Failed to parse the server's response.
    #[error("response parse error: {0}")]
    Parse(String),

    /// A pinned CA bundle holds no usable certificate. The reason never
    /// quotes the bundle's bytes.
    #[error("invalid CA certificate: {0}")]
    InvalidCa(String),

    /// A response body larger than the endpoint's cap. Only the first
    /// `cap` bytes were read; the rest never was.
    #[error("response too large: more than {cap} bytes")]
    TooLarge {
        /// The most bytes the endpoint's body may hold.
        cap: usize,
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
    /// The server answered with a redirect the client refused to follow.
    Redirect,
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
}

impl NetworkError {
    /// A failure of `kind` described by `message`.
    pub fn new(kind: NetworkKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for NetworkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl ClientError {
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
