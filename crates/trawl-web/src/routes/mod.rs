// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Handler modules, composed into the final `axum::Router` in `build()`.

pub mod auth;
pub mod proxy;
pub mod stream;

use axum::Router;
use axum::http::StatusCode;
use axum::routing::{any, get, post};

use crate::middleware::security_headers;
use crate::state::AppState;

/// Compose the proxy's top-level `Router` from its sub-modules.
pub fn build(state: AppState) -> Router {
    let (csp, hsts, xcto, refp, xfo) = security_headers::layers();

    Router::new()
        .route("/healthz", get(|| async { (StatusCode::OK, "ok") }))
        .route("/login", post(auth::login))
        .route("/logout", post(auth::logout))
        .route("/me", get(auth::me))
        // SSE first — must outrank the generic forwarder (both are under /api/v1).
        .route("/api/v1/stream", get(stream::forward))
        // Block /ingest before it can match the generic forwarder.
        .route("/api/v1/ingest", any(proxy::block_ingest))
        .route("/api/v1/{*path}", any(proxy::forward))
        .layer(csp)
        .layer(hsts)
        .layer(xcto)
        .layer(refp)
        .layer(xfo)
        .with_state(state)
}
