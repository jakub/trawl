//! HTTP transport via axum.

use std::sync::Arc;

use axum::Router;
use axum::middleware;
use axum::routing::{get, post};
use tokio::net::TcpListener;

use crate::auth::auth_middleware;
use crate::handlers;
use crate::shutdown::shutdown_signal;
use crate::state::AppState;

/// Build the axum router with all routes and middleware.
pub fn router(state: AppState) -> Router {
    // Routes that require authentication.
    let authenticated = Router::new()
        .route("/api/v1/query", post(handlers::query))
        .layer(middleware::from_fn(auth_middleware));

    // Routes that are public (no auth required).
    let public = Router::new().route("/api/v1/health", get(handlers::health));

    // Merge route groups.
    // Auth db path is injected into extensions so the auth middleware can find it.
    let auth_db_path = Arc::clone(&state.auth_db_path);

    Router::new()
        .merge(authenticated)
        .merge(public)
        .layer(axum::Extension(auth_db_path))
        .with_state(state)
}

/// Start the HTTP server on the configured address with graceful shutdown.
pub async fn serve(state: AppState, addr: &str) -> Result<(), crate::error::ServerError> {
    let app = router(state);

    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| crate::error::ServerError::Internal(format!("failed to bind {addr}: {e}")))?;

    tracing::info!(addr = %addr, "HTTP server listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| crate::error::ServerError::Internal(format!("server error: {e}")))?;

    tracing::info!("HTTP server stopped");
    Ok(())
}
