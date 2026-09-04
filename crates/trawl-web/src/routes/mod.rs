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
        // Like every route below, these are cookie-authenticated, and the
        // ADR-0016 origin guard reaches them through the `Auth` extractor
        // rather than through anything written here.
        .route("/api/v1/stream", get(stream::forward))
        .route("/api/v1/dashboard/stream", get(stream::forward_dashboard))
        // Block /ingest before it can match the generic forwarder.
        .route("/api/v1/ingest", any(proxy::block_ingest))
        .route("/api/v1/{*path}", any(proxy::forward))
        // Keep unknown API namespaces out of the SPA fallback. The
        // wildcard needs at least one segment, so bare `/api` is
        // routed explicitly — otherwise it alone would fall through
        // to the SPA and answer an API probe with index.html.
        .route("/api", any(proxy::not_found))
        .route("/api/{*path}", any(proxy::not_found));

    // Attach the SPA fallback. `TRAWL_WEB_SPA_DIR` wins when set
    // (hot-iterate flow); otherwise the embedded bundle takes over.
    let router = if let Ok(dir) = std::env::var(ENV_SPA_DIR) {
        let path = PathBuf::from(shellexpand::tilde(&dir).into_owned());
        tracing::info!(spa_dir = %path.display(), "SPA static-file fallback (env override)");
        router.fallback(move |req: Request| {
            let path = path.clone();
            async move {
                assets::serve_from_dir(
                    &path,
                    req.uri().path(),
                    req.headers(),
                    req.method() == axum::http::Method::HEAD,
                )
                .await
            }
        })
    } else {
        tracing::info!("SPA fallback: embedded bundle");
        router.fallback(|req: Request| async move {
            assets::serve_embedded(
                req.uri().path(),
                req.headers(),
                req.method() == axum::http::Method::HEAD,
            )
        })
    };

    router
        .layer(csp)
        .layer(hsts)
        .layer(xcto)
        .layer(refp)
        .layer(xfo)
        .with_state(state)
}
