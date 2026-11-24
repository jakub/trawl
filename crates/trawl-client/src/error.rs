// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fmt;

use trawl_api::{ErrorCode, ErrorDetail, ErrorEnvelope};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error(
        "network error: {0}{suffix}",
        suffix = RequestSuffix(.0.request_id.as_deref())
    )]
    Network(NetworkError),

    #[error(
        "server error (HTTP {status}): {}{}",
        error.message,
        RequestSuffix(request_id.as_deref())
    )]
    Server {
        status: u16,
        error: ErrorEnvelope,
        request_id: Option<String>,
        /// The response's `Retry-After` in seconds, when it carried the
        /// numeric form. The HTTP-date form, a value past `u32::MAX`, or
        /// anything else, is `None`. A `u32`, so the error stays small
        /// enough to return by value.
        retry_after_secs: Option<u32>,
    },

    #[error(
        "response parse error: {message}{}",
        RequestSuffix(request_id.as_deref())
    )]
    Parse {
        /// Never quotes the body.
        message: String,
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
        cap: usize,
        request_id: Option<String>,
    },

    /// A URL the client refuses to build on. The reason never quotes the
    /// URL.
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
}

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
    Other,
}

/// The message names the API origin at most, never URL userinfo, paths,
/// query parameters, a certificate, or the TLS library's reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkError {
    pub kind: NetworkKind,
    pub message: String,
    pub request_id: Option<String>,
}

impl NetworkError {
    pub fn new(kind: NetworkKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            request_id: None,
        }
    }

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
    pub(crate) fn parse(message: impl Into<String>) -> Self {
        Self::Parse {
            message: message.into(),
            request_id: None,
        }
    }

    /// The server's `X-Request-Id` for the response this error came from
    /// (ADR-0040). Read only as 1 to [`crate::MAX_REQUEST_ID_BYTES`] visible
    /// ASCII bytes: a longer or other header is absent.
    pub fn request_id(&self) -> Option<&str> {
        match self {
            Self::Server { request_id, .. }
            | Self::Parse { request_id, .. }
            | Self::TooLarge { request_id, .. } => request_id.as_deref(),
            Self::Network(error) => error.request_id.as_deref(),
            Self::InvalidCa(_) | Self::InvalidUrl(_) => None,
        }
    }

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

    pub fn network_kind(&self) -> Option<NetworkKind> {
        match self {
            Self::Network(error) => Some(error.kind),
            _ => None,
        }
    }

    pub fn error_envelope(&self) -> Option<&ErrorEnvelope> {
        match self {
            Self::Server { error, .. } => Some(error),
            _ => None,
        }
    }

    pub fn error_details(&self) -> &[ErrorDetail] {
        match self {
            Self::Server { error, .. } => &error.details,
            _ => &[],
        }
    }

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

    type Build = fn(Option<&str>) -> ClientError;

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
