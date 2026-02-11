//! HTTP transport via axum.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::http::{HeaderValue, Method, header};
use axum::middleware;
use axum::routing::{get, post};
use tokio::net::TcpListener;
use tower::limit::ConcurrencyLimitLayer;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::cors::CorsLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;

use crate::auth::auth_middleware;
use crate::handlers;
use crate::shutdown::shutdown_signal;
use crate::state::AppState;

/// Build the axum router with all routes and middleware.
pub fn router(state: AppState) -> Router {
    let max_body = state.max_request_body_bytes;
    let max_conns = state.max_concurrent_requests;

    // Routes that require authentication.
    let authenticated = Router::new()
        .route("/api/v1/query", post(handlers::query))
        .route("/api/v1/schema", get(handlers::schema))
        .route("/api/v1/queries", get(handlers::queries))
        .layer(middleware::from_fn(auth_middleware));

    // Routes that are public (no auth required).
    let public = Router::new().route("/api/v1/health", get(handlers::health));

    // Auth db path is injected into extensions so the auth middleware can find it.
    let auth_db_path = Arc::clone(&state.auth_db_path);

    Router::new()
        .merge(authenticated)
        .merge(public)
        // -- security hardening layers (outermost applied first) --
        .layer(CatchPanicLayer::new())
        .layer(RequestBodyLimitLayer::new(max_body))
        .layer(ConcurrencyLimitLayer::new(max_conns))
        .layer(SetResponseHeaderLayer::overriding(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(
            CorsLayer::new()
                .allow_methods([Method::GET, Method::POST])
                .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
        )
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|request: &axum::http::Request<_>| {
                    tracing::info_span!(
                        "http_request",
                        method = %request.method(),
                        path = %request.uri().path(),
                    )
                })
                .on_response(
                    |response: &axum::http::Response<_>,
                     latency: Duration,
                     _span: &tracing::Span| {
                        tracing::info!(
                            status = response.status().as_u16(),
                            latency_ms = latency.as_millis(),
                            "response"
                        );
                    },
                )
                .on_failure(
                    |error: tower_http::classify::ServerErrorsFailureClass,
                     latency: Duration,
                     _span: &tracing::Span| {
                        tracing::error!(
                            error = %error,
                            latency_ms = latency.as_millis(),
                            "request failed"
                        );
                    },
                ),
        )
        .layer(axum::Extension(auth_db_path))
        .with_state(state)
}

/// Start the HTTP server on the configured address with graceful shutdown.
///
/// On shutdown signal, stops accepting new connections and waits up to
/// `shutdown_drain_secs` for in-flight requests to complete before
/// forcing exit.
pub async fn serve(state: AppState, addr: &str) -> Result<(), crate::error::ServerError> {
    let drain_secs = state.shutdown_drain_secs;
    let app = router(state);

    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| crate::error::ServerError::Internal(format!("failed to bind {addr}: {e}")))?;

    tracing::info!(addr = %addr, "HTTP server listening");

    // Use a Notify to share the shutdown signal between graceful drain
    // and the hard deadline.
    let notify = Arc::new(tokio::sync::Notify::new());
    let n1 = Arc::clone(&notify);

    tokio::spawn(async move {
        shutdown_signal().await;
        n1.notify_waiters();
    });

    let n_graceful = Arc::clone(&notify);
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(async move { n_graceful.notified().await });

    let n_deadline = Arc::clone(&notify);
    let deadline = async move {
        n_deadline.notified().await;
        tracing::info!(drain_secs, "shutdown: draining in-flight requests");
        tokio::time::sleep(Duration::from_secs(drain_secs)).await;
        tracing::warn!("shutdown drain timeout exceeded, forcing exit");
    };

    tokio::select! {
        result = server => {
            result.map_err(|e| crate::error::ServerError::Internal(format!("server error: {e}")))?;
        }
        () = deadline => {}
    }

    tracing::info!("HTTP server stopped");
    Ok(())
}
