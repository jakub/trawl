// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The request body limit, judged where a handler reads the body.
//!
//! Each authenticated router bounds its request bodies by one setting:
//! `[server] max_request_body_bytes` for the interactive routes,
//! `[ingest] max_body_bytes` for `/ingest`. The configured value is the
//! only limit; axum's own default extractor limit is off.
//!
//! The limit is lazy. [`limit_body`] wraps the request body, and nothing
//! reads it until the handler's extractor does, after authentication and
//! the grant, which read headers only. The extractor's failure is answered
//! only after the handler's own authority check, which every handler makes
//! before it looks at what its extractors returned (ADR-0059). So a bad
//! token is 401 and a key without the route's permission 403 however large
//! the body, and only a key that may use the route learns the body was too
//! large. An interactive extractor still reads up to the limit before the
//! handler's check, as it reads any body (ADR-0049); `/ingest` checks its
//! permission before it reads.
//!
//! A body whose announced `Content-Length` is past the limit fails its
//! first read, before a byte of it is read; one without fails once a read
//! takes it past the limit. Either failure is [`OverLimit`], which
//! [`crate::shape::shaped`] answers as 413 `request_too_large` naming the
//! setting and its value. trawld still hangs up on an oversized upload
//! without reading the rest of it.

use axum::body::{Body, HttpBody as _};
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use tokio_stream::StreamExt as _;

use crate::error::ServerError;

/// The body limit a router enforces.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BodyLimit {
    /// The setting's name as an operator writes it in the config.
    pub(crate) setting: &'static str,
    /// The limit, in bytes.
    pub(crate) bytes: usize,
}

/// A body past its router's limit: the error a read of a limited body
/// fails with, found again under the extractor's rejection by
/// [`OverLimit::find`].
#[derive(Debug)]
pub(crate) struct OverLimit(BodyLimit);

impl std::fmt::Display for OverLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "request body exceeds {} ({} bytes)",
            self.0.setting, self.0.bytes
        )
    }
}

impl std::error::Error for OverLimit {}

impl OverLimit {
    /// The [`OverLimit`] that caused `error`, if one did: an extractor's
    /// rejection keeps the body's error as its source.
    pub(crate) fn find<'e>(error: &'e (dyn std::error::Error + 'static)) -> Option<&'e Self> {
        let mut cause = Some(error);
        while let Some(error) = cause {
            if let Some(over) = error.downcast_ref::<Self>() {
                return Some(over);
            }
            cause = error.source();
        }
        None
    }
}

impl From<&OverLimit> for ServerError {
    fn from(over: &OverLimit) -> Self {
        Self::RequestTooLarge {
            setting: over.0.setting,
            limit: over.0.bytes,
        }
    }
}

/// Bound the request's body by `limit`, lazily: the body is replaced by
/// one that fails its first read with [`OverLimit`] when the announced
/// length is past the limit, and otherwise counts what is read and fails
/// once that passes the limit. Nothing here reads the body.
pub(crate) async fn limit_body(
    State(limit): State<BodyLimit>,
    request: Request,
    next: Next,
) -> Response {
    next.run(request.map(|body| limited(body, limit))).await
}

/// `body`, bounded by `limit`.
fn limited(body: Body, limit: BodyLimit) -> Body {
    let announced = body.size_hint().lower();
    if announced > u64::try_from(limit.bytes).unwrap_or(u64::MAX) {
        // The body is dropped unread.
        return Body::from_stream(tokio_stream::once(Err::<axum::body::Bytes, _>(OverLimit(
            limit,
        ))));
    }
    let mut remaining = limit.bytes;
    Body::from_stream(body.into_data_stream().map(move |chunk| {
        let chunk = chunk.map_err(axum::BoxError::from)?;
        remaining = remaining
            .checked_sub(chunk.len())
            .ok_or_else(|| axum::BoxError::from(OverLimit(limit)))?;
        Ok::<_, axum::BoxError>(chunk)
    }))
}
