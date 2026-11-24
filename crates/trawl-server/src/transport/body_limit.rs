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
//! The limit is lazy: nothing reads the body until the handler's body
//! extractor does, behind its authority extractor (ADR-0059).
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

#[derive(Clone, Copy, Debug)]
pub(crate) struct BodyLimit {
    pub(crate) setting: &'static str,
    pub(crate) bytes: usize,
}

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

pub(crate) async fn limit_body(
    State(limit): State<BodyLimit>,
    request: Request,
    next: Next,
) -> Response {
    next.run(request.map(|body| limited(body, limit))).await
}

fn limited(body: Body, limit: BodyLimit) -> Body {
    let announced = body.size_hint().lower();
    if announced > u64::try_from(limit.bytes).unwrap_or(u64::MAX) {
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
