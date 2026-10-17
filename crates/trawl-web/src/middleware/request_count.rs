// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The proxy's one count of requests in progress (ADR-0054, amended
//! 2026-10-17).
//!
//! trawl-web buffers each request body before it calls trawld, so the
//! number of requests in progress is what bounds its memory. The count is
//! [`PROXY_MAX_REQUESTS`], fixed, and covers every route except
//! `/healthz`, which the probes must always reach. It is taken with a
//! try-acquire and never waited for: a request over the count is refused
//! at once with a 503 `request_limit_reached` that names trawl-web as the
//! hop. The count is held while the handler's future runs, until the
//! response head is produced, the future is dropped or a panic is caught
//! inside it. A response body is not counted, so a server-sent event
//! stream stops counting once its head is out.
//!
//! The constant is twice trawld's default `max_concurrent_requests`, so
//! browser traffic meets trawld's own refusal first at the defaults, and
//! this count is the proxy's memory backstop.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse as _, Response};
use tokio::sync::Semaphore;

use crate::error::ProxyError;

/// Requests in progress the proxy admits at once, `/healthz` aside.
pub const PROXY_MAX_REQUESTS: usize = 64;

/// The count, shared by every route it covers.
///
/// Built once, before any layer: axum clones each layer per route and per
/// method, and every clone must share this one.
#[derive(Clone, Debug)]
pub struct RequestCount(Arc<Semaphore>);

impl RequestCount {
    /// A count of `size` requests in progress.
    #[must_use]
    pub fn new(size: usize) -> Self {
        Self(Arc::new(Semaphore::new(size)))
    }
}

/// Take the count for the request, or refuse it at once.
///
/// The permit lives on this future's stack: it is released when `next`
/// returns the response head, when the future is dropped because the
/// client went away, and when a panic unwinds through it.
pub async fn count_request(
    State(count): State<RequestCount>,
    request: Request,
    next: Next,
) -> Response {
    let Ok(permit) = Arc::clone(&count.0).try_acquire_owned() else {
        return ProxyError::RequestLimitReached.into_response();
    };
    let response = next.run(request).await;
    drop(permit);
    response
}
