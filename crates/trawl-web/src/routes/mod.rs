// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Handler modules, composed into the final `axum::Router` in `build()`.

pub mod auth;

use axum::Router;
use axum::http::StatusCode;
use axum::routing::{get, post};

use crate::state::AppState;

/// Compose the proxy's top-level `Router` from its sub-modules.
pub fn build(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { (StatusCode::OK, "ok") }))
        .route("/login", post(auth::login))
        .route("/logout", post(auth::logout))
        .route("/me", get(auth::me))
        .with_state(state)
}
