// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTTPS transport via axum over `tokio-rustls`.
//!
//! The accept loop is manual because every connection is TLS-terminated
//! with `tokio-rustls` before hyper serves it. That also puts the peer
//! address into the request extensions, and lets the shutdown signal call
//! `graceful_shutdown` per connection so idle keep-alives close instead of
//! waiting out the drain timeout.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use axum::Router;
use axum::extract::Request;
use axum::http::{HeaderValue, Method, header};
use axum::middleware;
use axum::response::Response;
use axum::routing::{delete, get, post, put};
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
use ulid::Ulid;

/// ULID-based request ID stored in request extensions for tracing and response headers.
#[derive(Clone, Debug)]
struct RequestId(String);

use crate::config::{DEFAULT_INGEST_MAX_BODY_BYTES, ServerConfig};
use crate::handlers;
use crate::ingest;
use crate::policy::{normalize_auth_errors, require_trawl_grant};
use crate::rate_limit::{RateLimitState, rate_limit_middleware};
use crate::shutdown::{ShutdownRx, shutdown_observed, shutdown_signal};
use crate::state::{AppState, HttpConfig};
use crate::tls;

/// Build the axum router with all routes and middleware.
#[allow(clippy::too_many_lines)]
pub fn router(state: AppState, http: &HttpConfig) -> Router {
    let max_body = http.max_request_body_bytes;
    let max_conns = http.max_concurrent_requests;
    let cors_origins = &http.cors_allowed_origins;
    let ingest_enabled = state.ingest.wal_writer.is_some();
    let interactive_rate_state = RateLimitState::interactive(&http.rate_limit);
    let ingest_rate_state = RateLimitState::ingest(&http.rate_limit, &interactive_rate_state);
    let bearer_state = state.auth.bearer_state.clone();

    // Query routes. Onion (first .layer() = innermost): body limit →
    // envelope normalization → require_bearer_only (fleet-auth authn) →
    // require_trawl_grant (mandatory trawl policy) → rate limit → handler.
    let authenticated = Router::new()
        .route("/query", post(handlers::query))
        .route("/validate", post(handlers::validate_query))
        .route("/schema", get(handlers::schema))
        .route("/schema/services", get(handlers::schema_services))
        .route("/schema/fields", get(handlers::catalog_fields))
        .route("/schema/repin", post(handlers::schema_repin))
        .route("/schema/repin/status", get(handlers::schema_repin_status))
        .route("/schema/repin/cancel", post(handlers::schema_repin_cancel))
        // `?name=` rather than a path segment: a catalog key may contain `/`.
        .route("/schema/field", get(handlers::catalog_field))
        .route("/schema/conflicts", get(handlers::catalog_conflicts))
        .route("/schema/values/{field}", get(handlers::field_values))
        .route("/schema/gc-pins", post(handlers::schema_gc_pins))
        // Same `?name=` reason as `/schema/field`; both verbs, one path.
        .route(
            "/schema/field/ack",
            post(handlers::ack_degraded_field).delete(handlers::clear_degraded_field_ack),
        )
        .route("/queries", get(handlers::queries))
        .route("/queries/{id}", delete(handlers::cancel_query))
        .route("/stats", get(handlers::stats))
        .route("/dashboard", get(handlers::dashboard))
        .route("/dashboard/stream", get(handlers::dashboard_stream))
        .route("/whoami", get(handlers::whoami))
        .route("/history", get(handlers::history))
        .route(
            "/saved",
            get(handlers::list_saved).post(handlers::create_saved),
        )
        .route(
            "/saved/{id}",
            put(handlers::update_saved).delete(handlers::delete_saved),
        )
        .route(
            "/saved/{id}/schedule",
            put(handlers::set_schedule)
                .get(handlers::get_schedule)
                .delete(handlers::delete_schedule),
        )
        .route("/runs", get(handlers::list_all_runs))
        .route("/runs/stats", get(handlers::runs_stats))
        .route("/saved/{id}/run", post(handlers::trigger_run))
        .route("/saved/{id}/runs", get(handlers::list_report_runs))
        .route("/saved/{id}/runs/{run_id}", get(handlers::get_report_run))
        .route("/export", post(handlers::export))
        .route("/stream", get(handlers::stream_query))
        .layer(middleware::from_fn(rate_limit_middleware))
        // Outside the rate limit middleware, so the state is in extensions
        // before it runs: interactive routes get the `default_rpm` buckets.
        .layer(axum::Extension(interactive_rate_state))
        .layer(middleware::from_fn(require_trawl_grant))
        .layer(middleware::from_fn_with_state(
            bearer_state.clone(),
            fleet_auth::require_bearer_only,
        ))
        .layer(middleware::from_fn(normalize_auth_errors))
        .layer(RequestBodyLimitLayer::new(max_body));

    // Ingest route: same auth stack, separate (larger) body limit.
    let ingest_routes = if ingest_enabled {
        let ingest_body_limit = http
            .ingest_max_body_bytes
            .unwrap_or(DEFAULT_INGEST_MAX_BODY_BYTES);
        Router::new()
            .route("/ingest", post(ingest::handler::ingest))
            .layer(middleware::from_fn(rate_limit_middleware))
            // Ingest gets the shipper-sized `ingest_rpm` buckets — a separate
            // bucket map, so the ceiling never applies to the query routes,
            // and only for keys holding `Permission::Ingest` (the handler's
            // own check runs downstream of the limiter). Everyone else stays
            // on the interactive buckets.
            .layer(axum::Extension(ingest_rate_state))
            .layer(middleware::from_fn(require_trawl_grant))
            .layer(middleware::from_fn_with_state(
                bearer_state,
                fleet_auth::require_bearer_only,
            ))
            .layer(middleware::from_fn(normalize_auth_errors))
            .layer(RequestBodyLimitLayer::new(ingest_body_limit))
    } else {
        Router::new()
    };

    let mut app = Router::new()
        .nest("/api/v1", authenticated)
        .nest("/api/v1", ingest_routes)
        .route("/api/v1/health", get(handlers::health))
        .route("/metrics", get(handlers::prometheus_metrics))
        // -- security hardening layers (first .layer() = innermost) --
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
                let request_id = request
                    .extensions()
                    .get::<RequestId>()
                    .map_or("unknown", |r| r.0.as_str());
                let peer_addr = request
                    .extensions()
                    .get::<SocketAddr>()
                    .copied()
                    .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0)));
                let user_agent = request
                    .headers()
                    .get("user-agent")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                tracing::info_span!(
                    "http_request",
                    request_id,
                    peer_addr = %peer_addr,
                    method = %request.method(),
                    path = %request.uri().path(),
                    user_agent = %user_agent,
                )
            })
            .on_response(
                |response: &axum::http::Response<_>, latency: Duration, _span: &tracing::Span| {
                    tracing::debug!(
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
    .layer(middleware::from_fn(request_id_middleware))
    .layer(middleware::from_fn(connection_gauge_middleware))
    .with_state(state)
}

/// Generate a ULID request ID, stash it in extensions, and set `X-Request-Id` on the response.
async fn request_id_middleware(mut request: Request, next: middleware::Next) -> Response {
    let id = Ulid::generate().to_string();
    request.extensions_mut().insert(RequestId(id.clone()));

    let mut response = next.run(request).await;

    if let Ok(val) = HeaderValue::from_str(&id) {
        response.headers_mut().insert("x-request-id", val);
    }

    response
}

/// Track active HTTP connections via the `trawl_active_connections` gauge.
async fn connection_gauge_middleware(request: Request, next: middleware::Next) -> Response {
    metrics::gauge!(crate::metrics::ACTIVE_CONNECTIONS).increment(1.0);
    let response = next.run(request).await;
    metrics::gauge!(crate::metrics::ACTIVE_CONNECTIONS).decrement(1.0);
    response
}

/// Build the TLS acceptor for a serve path, warning when the certificate was
/// auto-generated.
fn build_tls_acceptor(
    config: &ServerConfig,
    state_dir: &Path,
) -> Result<TlsAcceptor, crate::error::ServerError> {
    let (tls_config, self_signed) = tls::build_server_config(
        config.tls_cert_path.as_deref(),
        config.tls_key_path.as_deref(),
        state_dir,
    )
    .map_err(|e| crate::error::ServerError::Internal(format!("TLS setup failed: {e}")))?;

    if self_signed {
        tracing::warn!(
            event_type = "lifecycle",
            "using auto-generated self-signed certificate — clients must use --insecure or trust the cert"
        );
    }

    Ok(TlsAcceptor::from(tls_config))
}

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
    state_dir: &Path,
    external_shutdown: Option<ShutdownRx>,
) -> Result<(), crate::error::ServerError> {
    let addr = &config.http_addr;

    // Build TLS config (loads or auto-generates cert).
    let tls_acceptor = build_tls_acceptor(config, state_dir)?;

    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| crate::error::ServerError::Internal(format!("failed to bind {addr}: {e}")))?;

    // Report the bound port, including when a disposable instance asks the
    // OS for a port with :0. The listener owns it before it is advertised.
    let local_addr = listener
        .local_addr()
        .map_err(|e| crate::error::ServerError::Internal(format!("listener local_addr: {e}")))?;
    tracing::info!(event_type = "lifecycle", addr = %local_addr, "HTTPS server listening");

    accept_loop(
        listener,
        tls_acceptor,
        state,
        http,
        config,
        external_shutdown,
    )
    .await
}

/// Serve over a listener the caller already bound.
///
/// Same TLS setup, accept loop and shutdown drain as [`serve`]. The one
/// difference is where the socket comes from: `config.http_addr` is NOT
/// consulted on this path, it is informational only, and the address logged
/// at startup is the listener's own `local_addr`. That is the point — a
/// caller binding port 0 to get a free port keeps the socket it tested.
///
/// This entry exists for callers that must own the bound socket, which today
/// means the test fixture; production binds through [`serve`]. A caller that
/// adopts a listener therefore decides where that socket is bound, and
/// nothing on this path checks that decision against `config.http_addr`.
pub async fn serve_with_listener(
    listener: std::net::TcpListener,
    state: AppState,
    http: &HttpConfig,
    config: &ServerConfig,
    state_dir: &Path,
    external_shutdown: Option<ShutdownRx>,
) -> Result<(), crate::error::ServerError> {
    let tls_acceptor = build_tls_acceptor(config, state_dir)?;

    let local_addr = listener
        .local_addr()
        .map_err(|e| crate::error::ServerError::Internal(format!("listener local_addr: {e}")))?;

    // A std listener is blocking by default, and tokio does not change that
    // for us. Registering a blocking socket with the reactor turns the accept
    // loop into a 100%-CPU spin, so set nonblocking here rather than trusting
    // every caller to remember.
    listener
        .set_nonblocking(true)
        .map_err(|e| crate::error::ServerError::Internal(format!("set_nonblocking failed: {e}")))?;
    let listener = TcpListener::from_std(listener).map_err(|e| {
        crate::error::ServerError::Internal(format!("failed to adopt listener: {e}"))
    })?;

    tracing::info!(event_type = "lifecycle", addr = %local_addr, "HTTPS server listening");

    accept_loop(
        listener,
        tls_acceptor,
        state,
        http,
        config,
        external_shutdown,
    )
    .await
}

/// The shared tail of both serve paths: cert hot-reload, the TLS accept loop,
/// and the graceful shutdown drain.
#[allow(clippy::too_many_lines)] // accept loop + shutdown drain are cohesive
async fn accept_loop(
    listener: TcpListener,
    tls_acceptor: TlsAcceptor,
    state: AppState,
    http: &HttpConfig,
    config: &ServerConfig,
    external_shutdown: Option<ShutdownRx>,
) -> Result<(), crate::error::ServerError> {
    let drain_secs = http.shutdown_drain_secs;
    let pool = state.query.pool.clone();
    let app = router(state, http);

    // Watch channel for cert hot-reload. The accept loop reads the latest
    // acceptor from the receiver before each TLS handshake.
    let (tls_tx, tls_rx) = tokio::sync::watch::channel(tls_acceptor);

    // Spawn cert file watcher if reload is enabled and cert paths are configured.
    let reload_interval = config.tls_reload_interval_secs;
    if reload_interval > 0
        && let (Some(cert), Some(key)) = (&config.tls_cert_path, &config.tls_key_path)
    {
        let cert = cert.clone();
        let key = key.clone();
        tokio::spawn(tls::cert_reload_task(
            cert,
            key,
            Duration::from_secs(reload_interval),
            tls_tx,
        ));
    }

    // Shutdown coordination: use the caller's channel (from monitor, or a
    // test) or spawn our own signal listener for the non-monitor path.
    let mut accept_rx = if let Some(ext) = external_shutdown {
        ext
    } else {
        let (tx, rx) = crate::shutdown::shutdown_channel();
        tokio::spawn(async move {
            shutdown_signal().await;
            let _ = tx.send(true);
        });
        rx
    };

    // Track spawned connection tasks for graceful drain.
    let mut connections = JoinSet::new();

    // One receiver the connection tasks clone from. It is a separate handle
    // because the accept arm below borrows `accept_rx` mutably for the whole
    // `select!`, and a clone taken after the flag was set still observes it.
    let conn_rx = accept_rx.clone();

    // Accept loop — runs until shutdown signal.
    loop {
        tokio::select! {
            result = listener.accept() => {
                let (tcp_stream, peer_addr) = result.map_err(|e| {
                    crate::error::ServerError::Internal(format!("accept error: {e}"))
                })?;

                let tls_acceptor = tls_rx.borrow().clone();
                let tower_service = app.clone();

                let mut conn_shutdown = conn_rx.clone();
                connections.spawn(async move {
                    // Accept-loop diagnostics carry PREAUTH_TRANSPORT_TARGET,
                    // not this module's path: a bare TCP connect-and-close
                    // provokes one, so persisting them would make an
                    // unauthenticated connection flood a durable-write
                    // amplifier. The target is in `UNMETERED_TARGETS`, so the
                    // WAL layer refuses it while stdout keeps it.
                    let tls_stream = match tls_acceptor.accept(tcp_stream).await {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::warn!(target: crate::telemetry::PREAUTH_TRANSPORT_TARGET, event_type = "tls_handshake_failed", peer = %peer_addr, error = %e, "TLS handshake failed");
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
                                tracing::debug!(target: crate::telemetry::PREAUTH_TRANSPORT_TARGET, event_type = "connection_error", peer = %peer_addr, error = %e, "connection error");
                            }
                        }
                        () = shutdown_observed(&mut conn_shutdown) => {
                            conn.as_mut().graceful_shutdown();
                            if let Err(e) = conn.await {
                                tracing::debug!(target: crate::telemetry::PREAUTH_TRANSPORT_TARGET, event_type = "connection_error", peer = %peer_addr, error = %e, "connection error during shutdown");
                            }
                        }
                    }
                });
            }
            () = shutdown_observed(&mut accept_rx) => {
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
