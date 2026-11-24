// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

pub mod auth;
pub mod proxy;
pub mod stream;

use std::path::PathBuf;

use axum::Router;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::routing::{any, get, post};

use crate::assets;
use crate::middleware::body_deadline;
use crate::middleware::request_count::{PROXY_MAX_REQUESTS, RequestCount};
use crate::middleware::security_headers;
use crate::state::AppState;

pub const ENV_SPA_DIR: &str = "TRAWL_WEB_SPA_DIR";

pub fn build(state: AppState) -> Router {
    build_with_count(state, RequestCount::new(PROXY_MAX_REQUESTS))
}

/// Only `/healthz` is merged in outside the count, so the probes are never
/// refused. The body deadline sits inside the count and outside every route,
/// so a stalled body gives its count back at the deadline.
pub fn build_with_count(state: AppState, count: RequestCount) -> Router {
    let (csp, hsts, xcto, refp, xfo) = security_headers::layers();

    // Auth endpoints live under /api/auth/ so the SPA owns the
    // top-level /login, /logout paths as client-side routes without
    // colliding with POST-only HTTP handlers (which would 405 on GET
    // navigation and break deep-links to the login page).
    let auth_routes = Router::new()
        .route("/api/auth/login", post(auth::login))
        .route("/api/auth/logout", post(auth::logout))
        .route("/api/auth/me", get(auth::me))
        .layer(axum::middleware::from_fn(auth::envelope_login_body_limit));

    let router = Router::new()
        .merge(auth_routes)
        .route("/api/v1/stream", get(stream::forward))
        .route("/api/v1/dashboard/stream", get(stream::forward_dashboard))
        .route("/api/v1/ingest", any(proxy::block_ingest))
        .route("/api/v1/{*path}", any(proxy::forward))
        // Keep unknown API namespaces out of the SPA fallback. The
        // wildcard needs at least one segment, so bare `/api` is
        // routed explicitly — otherwise it alone would fall through
        // to the SPA and answer an API probe with index.html.
        .route("/api", any(proxy::not_found))
        .route("/api/{*path}", any(proxy::not_found));

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

    let counted = router
        .layer(axum::middleware::from_fn_with_state(
            state.limits().body_read_timeout,
            body_deadline::bound_body_read,
        ))
        .layer(axum::middleware::from_fn_with_state(
            count,
            crate::middleware::request_count::count_request,
        ));
    let probes = Router::new().route("/healthz", get(|| async { (StatusCode::OK, "ok") }));

    counted
        .merge(probes)
        .layer(csp)
        .layer(hsts)
        .layer(xcto)
        .layer(refp)
        .layer(xfo)
        .with_state(state)
}
