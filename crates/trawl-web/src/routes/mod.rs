// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Handler modules, composed into the final `axum::Router` in `build()`.

pub mod auth;
pub mod proxy;
pub mod stream;

use std::path::PathBuf;

use axum::Router;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::routing::{any, get, post};
use tower_http::services::ServeDir;

use crate::assets;
use crate::middleware::security_headers;
use crate::state::AppState;

/// Env var: when set, serves the SPA dist directory as a static-file
/// fallback, overriding the embedded bundle. Dev affordance that lets
/// engineers iterate on the SPA (`trunk build`) without rebuilding
/// `trawl-web`.
pub const ENV_SPA_DIR: &str = "TRAWL_WEB_SPA_DIR";

/// Compose the proxy's top-level `Router` from its sub-modules.
pub fn build(state: AppState) -> Router {
    let (csp, hsts, xcto, refp, xfo) = security_headers::layers();

    let router = Router::new()
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
        .route("/api/v1/dashboard/stream", get(stream::forward_dashboard))
        // Block /ingest before it can match the generic forwarder.
        .route("/api/v1/ingest", any(proxy::block_ingest))
        .route("/api/v1/{*path}", any(proxy::forward))
        // Coastwatch intel proxy — strips /api/intel prefix before forwarding.
        .route("/api/intel/v1/{*path}", any(proxy::forward_intel));

    // Attach the SPA fallback. `TRAWL_WEB_SPA_DIR` wins when set
    // (hot-iterate flow); otherwise the embedded bundle takes over.
    let router = if let Ok(dir) = std::env::var(ENV_SPA_DIR) {
        let path = PathBuf::from(shellexpand::tilde(&dir).into_owned());
        let index = path.join("index.html");
        tracing::info!(spa_dir = %path.display(), "SPA static-file fallback (env override)");
        // Classic SPA fallback: serve the requested file if it exists,
        // otherwise hand back index.html so the client-side router can
        // interpret the path.
        let serve = ServeDir::new(&path)
            .append_index_html_on_directories(true)
            .fallback(tower_http::services::ServeFile::new(index));
        router.fallback_service(serve)
    } else {
        tracing::info!("SPA fallback: embedded bundle");
        router.fallback(|req: Request| async move { assets::serve(req.uri().path()) })
    };

    router
        .layer(csp)
        .layer(hsts)
        .layer(xcto)
        .layer(refp)
        .layer(xfo)
        .with_state(state)
}
