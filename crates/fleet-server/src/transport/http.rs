//! HTTPS transport via axum over `tokio-rustls`.
//!
//! Uses a manual TLS accept loop with hyper for per-connection control
//! and future mTLS support.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::Request;
use axum::http::{HeaderValue, Method, header};
use axum::middleware;
use axum::routing::{get, post};
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::TcpListener;
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tower::Service;
use tower::limit::ConcurrencyLimitLayer;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::cors::CorsLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;

use crate::auth::auth_middleware;
use crate::config::ServerConfig;
use crate::handlers;
use crate::shutdown::shutdown_signal;
use crate::state::AppState;
use crate::tls;

/// Build the axum router with all routes and middleware.
pub fn router(state: AppState) -> Router {
    let max_body = state.max_request_body_bytes;
    let max_conns = state.max_concurrent_requests;
    let cors_origins = state.cors_allowed_origins.clone();

    // Routes that require authentication — nested under /api/v1 so the
    // auth middleware is structurally bound to this subtree.
    let authenticated = Router::new()
        .route("/query", post(handlers::query))
        .route("/schema", get(handlers::schema))
        .route("/queries", get(handlers::queries))
        .layer(middleware::from_fn(auth_middleware));

    // Shared key store injected into extensions for the auth middleware.
    let key_store = Arc::clone(&state.key_store);

    let mut app = Router::new()
        .nest("/api/v1", authenticated)
        .route("/api/v1/health", get(handlers::health))
        // -- security hardening layers (outermost applied first) --
        .layer(CatchPanicLayer::new())
        .layer(RequestBodyLimitLayer::new(max_body))
        .layer(ConcurrencyLimitLayer::new(max_conns))
        .layer(SetResponseHeaderLayer::overriding(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=63072000; includeSubDomains"),
        ));

    // Only add CORS headers when origins are explicitly configured.
    // Empty list = no CORS layer = browser same-origin policy denies cross-origin.
    if !cors_origins.is_empty() {
        let origins: Vec<HeaderValue> =
            cors_origins.iter().filter_map(|o| o.parse().ok()).collect();
        app = app.layer(
            CorsLayer::new()
                .allow_origin(origins)
                .allow_methods([Method::GET, Method::POST])
                .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
        );
    }

    app.layer(
        TraceLayer::new_for_http()
            .make_span_with(|request: &axum::http::Request<_>| {
                tracing::info_span!(
                    "http_request",
                    method = %request.method(),
                    path = %request.uri().path(),
                )
            })
            .on_response(
                |response: &axum::http::Response<_>, latency: Duration, _span: &tracing::Span| {
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
    .layer(axum::Extension(key_store))
    .with_state(state)
}

/// Start the HTTPS server with graceful shutdown.
///
/// Binds a TCP listener, wraps connections in TLS via `tokio-rustls`,
/// and serves each connection through hyper + axum. On shutdown signal,
/// stops accepting new connections and drains in-flight requests up to
/// `shutdown_drain_secs`.
pub async fn serve(
    state: AppState,
    config: &ServerConfig,
) -> Result<(), crate::error::ServerError> {
    let drain_secs = state.shutdown_drain_secs;
    let addr = &config.http_addr;

    // Build TLS config (loads or auto-generates cert).
    let (tls_config, self_signed) = tls::build_server_config(
        config.tls_cert_path.as_deref(),
        config.tls_key_path.as_deref(),
    )
    .map_err(|e| crate::error::ServerError::Internal(format!("TLS setup failed: {e}")))?;

    if self_signed {
        tracing::warn!(
            "using auto-generated self-signed certificate — clients must use --insecure or trust the cert"
        );
    }

    let tls_acceptor = TlsAcceptor::from(tls_config);
    let pool = state.pool.clone();
    let app = router(state);

    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| crate::error::ServerError::Internal(format!("failed to bind {addr}: {e}")))?;

    tracing::info!(addr = %addr, "HTTPS server listening");

    // Watch channel for cert hot-reload. The accept loop reads the latest
    // acceptor from the receiver before each TLS handshake.
    let (tls_tx, tls_rx) = tokio::sync::watch::channel(tls_acceptor);

    // Spawn cert file watcher if reload is enabled and cert paths are configured.
    let reload_interval = config.tls_reload_interval_secs;
    if reload_interval > 0 {
        if let (Some(cert), Some(key)) = (&config.tls_cert_path, &config.tls_key_path) {
            let cert = cert.clone();
            let key = key.clone();
            tokio::spawn(tls::cert_reload_task(
                cert,
                key,
                Duration::from_secs(reload_interval),
                tls_tx,
            ));
        }
    }

    // Shutdown coordination: Notify fires on SIGINT/SIGTERM.
    let notify = Arc::new(tokio::sync::Notify::new());
    let n_signal = Arc::clone(&notify);

    tokio::spawn(async move {
        shutdown_signal().await;
        n_signal.notify_waiters();
    });

    // Track spawned connection tasks for graceful drain.
    let mut connections = JoinSet::new();

    // Accept loop — runs until shutdown signal.
    let n_accept = Arc::clone(&notify);
    loop {
        tokio::select! {
            result = listener.accept() => {
                let (tcp_stream, peer_addr) = result.map_err(|e| {
                    crate::error::ServerError::Internal(format!("accept error: {e}"))
                })?;

                let tls_acceptor = tls_rx.borrow().clone();
                let tower_service = app.clone();

                connections.spawn(async move {
                    let tls_stream = match tls_acceptor.accept(tcp_stream).await {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::debug!(peer = %peer_addr, error = %e, "TLS handshake failed");
                            return;
                        }
                    };

                    let io = TokioIo::new(tls_stream);

                    let hyper_service =
                        hyper::service::service_fn(move |req: Request<Incoming>| {
                            let mut svc = tower_service.clone();
                            async move { svc.call(req).await }
                        });

                    if let Err(e) =
                        hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                            .serve_connection_with_upgrades(io, hyper_service)
                            .await
                    {
                        tracing::debug!(peer = %peer_addr, error = %e, "connection error");
                    }
                });
            }
            () = n_accept.notified() => {
                tracing::info!("shutdown: stopping accept loop");
                break;
            }
        }
    }

    // Interrupt active DuckDB queries so they don't block the drain.
    pool.cancel_all();

    // Drain in-flight connections with a deadline.
    tracing::info!(
        drain_secs,
        connections = connections.len(),
        "shutdown: draining in-flight connections"
    );

    let drain = async { while connections.join_next().await.is_some() {} };

    if tokio::time::timeout(Duration::from_secs(drain_secs), drain)
        .await
        .is_err()
    {
        tracing::warn!(
            remaining = connections.len(),
            "shutdown drain timeout exceeded, aborting remaining connections"
        );
        connections.abort_all();
    }

    tracing::info!("HTTPS server stopped");
    Ok(())
}
