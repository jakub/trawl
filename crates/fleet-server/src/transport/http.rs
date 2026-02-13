//! HTTPS transport via axum over `tokio-rustls`.
//!
//! Uses a manual TLS accept loop with hyper for per-connection control
//! and future mTLS support.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
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

/// Monotonic request counter for correlating events within a single request.
static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

use crate::auth::auth_middleware;
use crate::config::{DEFAULT_INGEST_MAX_BODY_BYTES, ServerConfig};
use crate::handlers;
use crate::ingest;
use crate::rate_limit::{RateLimitState, rate_limit_middleware};
use crate::shutdown::shutdown_signal;
use crate::state::{AppState, HttpConfig};
use crate::tls;

/// Build the axum router with all routes and middleware.
pub fn router(state: AppState, http: &HttpConfig) -> Router {
    let max_body = http.max_request_body_bytes;
    let max_conns = http.max_concurrent_requests;
    let cors_origins = &http.cors_allowed_origins;
    let ingest_enabled = state.ingest.wal_writer.is_some();
    let rate_state = RateLimitState::from_config(&http.rate_limit);

    // Query routes: body limit → auth → rate limit (axum onion: first layer = innermost).
    let authenticated = Router::new()
        .route("/query", post(handlers::query))
        .route("/schema", get(handlers::schema))
        .route("/queries", get(handlers::queries))
        .layer(middleware::from_fn(rate_limit_middleware))
        .layer(middleware::from_fn(auth_middleware))
        .layer(RequestBodyLimitLayer::new(max_body));

    // Shared key store injected into extensions for the auth middleware.
    let key_store = Arc::clone(&state.auth.key_store);

    // Ingest route: body limit → auth → rate limit.
    let ingest_routes = if ingest_enabled {
        let ingest_body_limit = http
            .ingest_max_body_bytes
            .unwrap_or(DEFAULT_INGEST_MAX_BODY_BYTES);
        Router::new()
            .route("/ingest", post(ingest::handler::ingest))
            .layer(middleware::from_fn(rate_limit_middleware))
            .layer(middleware::from_fn(auth_middleware))
            .layer(RequestBodyLimitLayer::new(ingest_body_limit))
    } else {
        Router::new()
    };

    let mut app = Router::new()
        .nest("/api/v1", authenticated)
        .nest("/api/v1", ingest_routes)
        .route("/api/v1/health", get(handlers::health))
        // -- security hardening layers (outermost applied first) --
        .layer(CatchPanicLayer::new())
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
                let request_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
                let peer_addr = request
                    .extensions()
                    .get::<SocketAddr>()
                    .copied()
                    .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0)));
                tracing::info_span!(
                    "http_request",
                    request_id,
                    peer_addr = %peer_addr,
                    method = %request.method(),
                    path = %request.uri().path(),
                )
            })
            .on_response(
                |response: &axum::http::Response<_>, latency: Duration, _span: &tracing::Span| {
                    tracing::info!(
                        event_type = "http_response",
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
                        event_type = "http_failure",
                        error = %error,
                        latency_ms = latency.as_millis(),
                        "request failed"
                    );
                },
            ),
    )
    .layer(axum::Extension(rate_state))
    .layer(axum::Extension(key_store))
    .with_state(state)
}

#[allow(clippy::too_many_lines)] // accept loop + shutdown drain are cohesive
/// Start the HTTPS server with graceful shutdown.
///
/// Binds a TCP listener, wraps connections in TLS via `tokio-rustls`,
/// and serves each connection through hyper + axum. On shutdown signal,
/// stops accepting new connections and drains in-flight requests up to
/// `shutdown_drain_secs`.
pub async fn serve(
    state: AppState,
    http: &HttpConfig,
    config: &ServerConfig,
) -> Result<(), crate::error::ServerError> {
    let drain_secs = http.shutdown_drain_secs;
    let addr = &config.http_addr;

    // Build TLS config (loads or auto-generates cert).
    let (tls_config, self_signed) = tls::build_server_config(
        config.tls_cert_path.as_deref(),
        config.tls_key_path.as_deref(),
    )
    .map_err(|e| crate::error::ServerError::Internal(format!("TLS setup failed: {e}")))?;

    if self_signed {
        tracing::warn!(
            event_type = "lifecycle",
            "using auto-generated self-signed certificate — clients must use --insecure or trust the cert"
        );
    }

    let tls_acceptor = TlsAcceptor::from(tls_config);
    let pool = state.query.pool.clone();
    let app = router(state, http);

    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| crate::error::ServerError::Internal(format!("failed to bind {addr}: {e}")))?;

    tracing::info!(event_type = "lifecycle", addr = %addr, "HTTPS server listening");

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

                let n_conn = Arc::clone(&notify);
                connections.spawn(async move {
                    let tls_stream = match tls_acceptor.accept(tcp_stream).await {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::warn!(event_type = "tls_handshake_failed", peer = %peer_addr, error = %e, "TLS handshake failed");
                            return;
                        }
                    };

                    let io = TokioIo::new(tls_stream);

                    let hyper_service =
                        hyper::service::service_fn(move |mut req: Request<Incoming>| {
                            req.extensions_mut().insert(peer_addr);
                            let mut svc = tower_service.clone();
                            async move { svc.call(req).await }
                        });

                    let builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
                    let conn_fut = builder.serve_connection_with_upgrades(io, hyper_service);
                    let mut conn = std::pin::pin!(conn_fut);

                    // Poll the connection, but initiate graceful shutdown
                    // when the server-wide signal fires. This closes idle
                    // keep-alive connections instead of waiting for the
                    // drain timeout.
                    tokio::select! {
                        result = conn.as_mut() => {
                            if let Err(e) = result {
                                tracing::debug!(event_type = "connection_error", peer = %peer_addr, error = %e, "connection error");
                            }
                        }
                        () = n_conn.notified() => {
                            conn.as_mut().graceful_shutdown();
                            if let Err(e) = conn.await {
                                tracing::debug!(event_type = "connection_error", peer = %peer_addr, error = %e, "connection error during shutdown");
                            }
                        }
                    }
                });
            }
            () = n_accept.notified() => {
                tracing::info!(event_type = "lifecycle", "shutdown: stopping accept loop");
                break;
            }
        }
    }

    // Interrupt active DuckDB queries so they don't block the drain.
    pool.cancel_all();

    // Drain in-flight connections with a deadline.
    tracing::info!(
        event_type = "lifecycle",
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
            event_type = "lifecycle",
            remaining = connections.len(),
            "shutdown drain timeout exceeded, aborting remaining connections"
        );
        connections.abort_all();
    }

    tracing::info!(event_type = "lifecycle", "HTTPS server stopped");
    Ok(())
}
