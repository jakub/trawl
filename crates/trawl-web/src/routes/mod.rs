// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Handler modules, composed into the final `axum::Router` in `build()`.

pub mod auth;
pub mod proxy;
pub mod stream;

use std::path::PathBuf;

use axum::Router;
use axum::http::StatusCode;
use axum::routing::{any, get, post};
use tower_http::services::ServeDir;

use crate::middleware::security_headers;
use crate::state::AppState;

/// Env var: when set, serves the SPA dist directory as a static-file
/// fallback. Dev-only affordance; the production binary (commit 15 in
/// the plan) will embed dist/ via `rust-embed` instead.
pub const ENV_SPA_DIR: &str = "TRAWL_WEB_SPA_DIR";

/// Compose the proxy's top-level `Router` from its sub-modules.
pub fn build(state: AppState) -> Router {
    let (csp, hsts, xcto, refp, xfo) = security_headers::layers();

    let mut router = Router::new()
        .route("/healthz", get(|| async { (StatusCode::OK, "ok") }))
        // Auth endpoints live under /api/auth/ so the SPA owns the
        // top-level /login, /logout paths as client-side routes without
        // colliding with POST-only HTTP handlers (which would 405 on GET
        // navigation and break deep-links to the login page).
        .route("/api/auth/login", post(auth::login))
        .route("/api/auth/logout", post(auth::logout))
        .route("/api/auth/me", get(auth::me))
        // SSE first — must outrank the generic forwarder (both are under /api/v1).
        .route("/api/v1/stream", get(stream::forward))
        // Block /ingest before it can match the generic forwarder.
        .route("/api/v1/ingest", any(proxy::block_ingest))
        .route("/api/v1/{*path}", any(proxy::forward));

    if let Ok(dir) = std::env::var(ENV_SPA_DIR) {
        let path = PathBuf::from(shellexpand::tilde(&dir).into_owned());
        let index = path.join("index.html");
        tracing::info!(spa_dir = %path.display(), "SPA static-file fallback enabled");
        // Classic SPA fallback: serve the requested file if it exists,
        // otherwise hand back index.html so the client-side router can
        // interpret the path. Without this, deep-linking to /search or
        // /login via the URL bar 404s before the wasm router can run.
        let serve = ServeDir::new(&path)
            .append_index_html_on_directories(true)
            .fallback(tower_http::services::ServeFile::new(index));
        router = router.fallback_service(serve);
    }

    router
        .layer(csp)
        .layer(hsts)
        .layer(xcto)
        .layer(refp)
        .layer(xfo)
        .with_state(state)
}
