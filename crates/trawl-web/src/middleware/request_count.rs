// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The proxy's one count of requests in progress (ADR-0054).

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse as _, Response};
use tokio::sync::Semaphore;

use crate::error::ProxyError;

pub const PROXY_MAX_REQUESTS: usize = 64;

/// Built once, before any layer: axum clones each layer per route and per
/// method, and every clone must share this one.
#[derive(Clone, Debug)]
pub struct RequestCount(Arc<Semaphore>);

impl RequestCount {
    #[must_use]
    pub fn new(size: usize) -> Self {
        Self(Arc::new(Semaphore::new(size)))
    }

    #[cfg(test)]
    pub(crate) fn available(&self) -> usize {
        self.0.available_permits()
    }
}

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
