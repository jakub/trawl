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

use std::any::Any;
#[cfg(any(test, feature = "test-support"))]
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use axum::Router;
use axum::extract::{DefaultBodyLimit, MatchedPath, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware;
use axum::response::{IntoResponse as _, Response};
use axum::routing::{delete, get, post, put};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tower::Service;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::cors::CorsLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;
use ulid::Ulid;

/// ULID-based request ID stored in request extensions for tracing and response headers.
#[derive(Clone, Debug)]
pub struct RequestId(pub(crate) String);

use super::failure;
use super::request_limit::{self, RequestLimit};
use crate::config::{DEFAULT_INGEST_MAX_BODY_BYTES, ServerConfig};
use crate::error::ServerError;
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
    let ingest_enabled = state.ingest.wal_writer.is_some();
    let interactive_rate_state = RateLimitState::interactive(&http.rate_limit);
    let ingest_rate_state = RateLimitState::ingest(&http.rate_limit, &interactive_rate_state);
    let bearer_state = state.auth.bearer_state.clone();

    // Query routes. Onion (first .layer() = innermost): preview no-store →
    // body-limit envelope → body limit → axum's default limit off →
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
        .route("/history", get(handlers::history).delete(handlers::clear_history))
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
        .route("/stream", get(handlers::stream_query));
    // The ingest preview lives here, not beside `/ingest`: it spends the
    // interactive bucket and body limit, and exists only where ingest does
    // (ADR-0049). Its no-store header is set by `no_store_on_preview`
    // below, outside every refusal this router can answer with.
    let authenticated = if ingest_enabled {
        authenticated.route("/ingest/preview", post(ingest::preview::preview))
    } else {
        authenticated
    };
    let authenticated = authenticated
        // Innermost, so a failure recorded after it is the handler's.
        .route_layer(middleware::from_fn(failure::mark_handler))
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
        // The configured limit is the only one: see `envelope_body_limit`.
        .layer(DefaultBodyLimit::disable())
        .layer(RequestBodyLimitLayer::new(max_body))
        .layer(middleware::from_fn_with_state(
            BodyLimit {
                setting: "[server] max_request_body_bytes",
                bytes: max_body,
            },
            envelope_body_limit,
        ))
        // Outermost, so the body limit's 413, the auth layers' 401 and the
        // grant layer's 403 for the preview are covered too.
        .layer(middleware::from_fn(no_store_on_preview));

    // Ingest route: same auth stack and body-limit layers, separate
    // (larger) body limit.
    let ingest_routes = if ingest_enabled {
        let ingest_body_limit = http
            .ingest_max_body_bytes
            .unwrap_or(DEFAULT_INGEST_MAX_BODY_BYTES);
        Router::new()
            .route("/ingest", post(ingest::handler::ingest))
            .route_layer(middleware::from_fn(failure::mark_handler))
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
            .layer(DefaultBodyLimit::disable())
            .layer(RequestBodyLimitLayer::new(ingest_body_limit))
            .layer(middleware::from_fn_with_state(
                BodyLimit {
                    setting: "[ingest] max_body_bytes",
                    bytes: ingest_body_limit,
                },
                envelope_body_limit,
            ))
    } else {
        Router::new()
    };

    let app = Router::new()
        .route("/api/v1/health", get(handlers::health))
        .route("/metrics", get(handlers::prometheus_metrics))
        // Only the two routes above: the nested routers mark their own
        // handlers, inside their rate limiters.
        .route_layer(middleware::from_fn(failure::mark_handler))
        .nest("/api/v1", authenticated)
        .nest("/api/v1", ingest_routes);

    with_edge_layers(app, http).with_state(state)
}

/// The body limit a router enforces, as [`envelope_body_limit`] names it.
#[derive(Clone, Copy)]
struct BodyLimit {
    /// The setting's name as an operator writes it in the config.
    setting: &'static str,
    /// The exact value the router's `RequestBodyLimitLayer` enforces.
    bytes: usize,
}

/// Answer a body-limit refusal with the error envelope, code
/// `request_too_large`, naming the setting and its value.
///
/// Mounted directly outside a `RequestBodyLimitLayer`. That layer refuses
/// in two places, and both answer `text/plain`. An announced
/// `Content-Length` past the limit is refused before the inner service
/// runs (tower-http 0.6.11 `limit/service.rs:55`, `create_error_response`
/// in `limit/body.rs:83`). A chunked body is refused by the handler's body
/// extractor once its `Limited` wrapper reads past the limit, as axum's
/// `LengthLimitError` rejection (axum-core 0.5.6 `extract/rejection.rs:40`).
/// That 413 comes back out through the auth layers, so this sits outside
/// them too.
///
/// A 413 that is already `application/json` is a handler's own refusal
/// (`preview_too_large`, a gzip `ingest_error`, `ingest_batch_too_large`)
/// and passes untouched. `DefaultBodyLimit::disable()` sits directly
/// inside the limit layer: otherwise axum's extractors wrap the body in
/// their own hidden 2 MiB limit (axum-core 0.5.6
/// `ext_traits/request.rs:319`), whose plain-text 413 this would mislabel
/// as the configured setting, and which refuses ingest bodies far under
/// `[ingest] max_body_bytes`.
///
/// The request body is never touched: trawld still hangs up on an
/// oversized upload without reading the rest of it.
async fn envelope_body_limit(
    State(limit): State<BodyLimit>,
    request: Request,
    next: middleware::Next,
) -> Response {
    let response = next.run(request).await;
    if response.status() != StatusCode::PAYLOAD_TOO_LARGE || is_json(&response) {
        return response;
    }
    // A fresh response, so the body, `Content-Type` and `Content-Length`
    // are replaced together and no stale length survives.
    ServerError::RequestTooLarge {
        setting: limit.setting,
        limit: limit.bytes,
    }
    .into_response()
}

/// Whether `response` declares an `application/json` body.
fn is_json(response: &Response) -> bool {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|essence| essence.trim().eq_ignore_ascii_case("application/json"))
}

/// The ingest preview's route as the router matched it, prefix included.
const INGEST_PREVIEW_ROUTE: &str = "/api/v1/ingest/preview";

/// Mark every response to the ingest preview `Cache-Control: no-store`.
///
/// A report quotes the sample, and a refusal is no less the route's
/// answer, so the header goes on whatever comes back, from the handler or
/// from any layer inside this one (ADR-0049). No other route is touched.
///
/// The route is recognised by its [`MatchedPath`], which the outer router
/// sets before any per-route layer runs. The request's own URI cannot be
/// used here: `nest` strips `/api/v1` from it before these layers see it.
async fn no_store_on_preview(request: Request, next: middleware::Next) -> Response {
    let preview = request
        .extensions()
        .get::<MatchedPath>()
        .is_some_and(|matched| matched.as_str() == INGEST_PREVIEW_ROUTE);
    let mut response = next.run(request).await;
    if preview {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}

/// Wrap `app` in trawld's edge layers, the ones every route shares.
///
/// Onion, innermost first: panic catcher → request count → `nosniff` and
/// HSTS headers → CORS (only with configured origins) → request span
/// (`TraceLayer`) → failure observer → request id → connection gauge.
///
/// The request count ([`request_limit::count_request`], ADR-0054) sits
/// outside the panic catcher and every layer of the nested routers, so it
/// also bounds how many authentication checks and body reads run at once,
/// and a caught panic still ends its count. Every layer outside it wraps
/// its refusal: the refusal carries a request id, `nosniff`, HSTS and CORS
/// headers, and gets its one `http_failure`. A CORS preflight the CORS
/// layer answers never reaches it. The count is built once, here, before
/// any layer: axum clones each layer per route and per method, and every
/// clone shares this one.
///
/// The failure observer sits directly inside `request_id_middleware`: the
/// request id exists when it starts, and every other layer runs inside the
/// record it scopes. It is outside the `TraceLayer` on purpose, so its one
/// event per 5xx is emitted after the request span has closed and carries
/// only the fields it names (ADR-0040). The `TraceLayer` therefore logs no
/// failure of its own.
///
/// Public so a test can mount a route of its own under exactly the layers
/// production uses; [`router`] is the one production caller.
pub fn with_edge_layers<S>(app: Router<S>, http: &HttpConfig) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let cors_origins = &http.cors_allowed_origins;
    let limit = RequestLimit::new(http.max_concurrent_requests);
    let mut app = app
        // -- security hardening layers (first .layer() = innermost) --
        .layer(CatchPanicLayer::custom(panic_response))
        .layer(middleware::from_fn_with_state(
            limit,
            request_limit::count_request,
        ))
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
            // The failure observer outside this layer owns the one
            // `http_failure` per 5xx.
            .on_failure(()),
    )
    .layer(middleware::from_fn(failure::failure_observer))
    .layer(middleware::from_fn(request_id_middleware))
    .layer(middleware::from_fn(connection_gauge_middleware))
}

/// The response for a panic the outer catcher caught: the same 500 and
/// plain-text body `CatchPanicLayer`'s default gives, minus the default's
/// log line, which quotes the panic's payload.
///
/// The payload is dropped unread; it can quote anything the panicking code
/// had in hand (ADR-0040). What the request's failure event needs, that a
/// panic was caught, goes into its record instead.
fn panic_response(_payload: Box<dyn Any + Send + 'static>) -> Response {
    failure::record_panic();
    let mut response = Response::new(axum::body::Body::from("Service panicked"));
    *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
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
        Incoming::new(listener),
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
    let incoming = Incoming::new(adopt_listener(listener)?);
    accept_loop(
        incoming,
        tls_acceptor,
        state,
        http,
        config,
        external_shutdown,
    )
    .await
}

/// [`serve_with_listener`], with the first `accept()` calls answering
/// `faults`, one each, in order, before any real accept.
///
/// The seam behind the accept loop's error handling: an error such as
/// EMFILE cannot be provoked on demand from a real listener. The faults
/// belong to this one server; nothing about them is process-wide.
#[cfg(feature = "test-support")]
pub async fn serve_with_listener_failing_accepts(
    listener: std::net::TcpListener,
    faults: Vec<std::io::Error>,
    state: AppState,
    http: &HttpConfig,
    config: &ServerConfig,
    state_dir: &Path,
    external_shutdown: Option<ShutdownRx>,
) -> Result<(), crate::error::ServerError> {
    let tls_acceptor = build_tls_acceptor(config, state_dir)?;
    let mut incoming = Incoming::new(adopt_listener(listener)?);
    incoming.faults.extend(faults);
    accept_loop(
        incoming,
        tls_acceptor,
        state,
        http,
        config,
        external_shutdown,
    )
    .await
}

/// Register a listener the caller bound with the runtime, and log the
/// address it listens on.
fn adopt_listener(
    listener: std::net::TcpListener,
) -> Result<TcpListener, crate::error::ServerError> {
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
    Ok(listener)
}

/// How long the accept loop waits after a failed `accept()` before it
/// accepts again. Long enough that a process out of file descriptors does
/// not spin on the error; short enough that it serves again soon after
/// descriptors free up.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);

/// The listener the accept loop reads.
///
/// With `test-support`, a queue of errors the next `accept()` calls answer
/// before the listener is asked: see [`serve_with_listener_failing_accepts`].
struct Incoming {
    listener: TcpListener,
    #[cfg(any(test, feature = "test-support"))]
    faults: VecDeque<std::io::Error>,
}

impl Incoming {
    fn new(listener: TcpListener) -> Self {
        Self {
            listener,
            #[cfg(any(test, feature = "test-support"))]
            faults: VecDeque::new(),
        }
    }

    async fn accept(&mut self) -> std::io::Result<(TcpStream, SocketAddr)> {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(fault) = self.faults.pop_front() {
            return Err(fault);
        }
        self.listener.accept().await
    }
}

/// The shared tail of both serve paths: cert hot-reload, the TLS accept loop,
/// and the graceful shutdown drain.
#[allow(clippy::too_many_lines)] // accept loop + shutdown drain are cohesive
async fn accept_loop(
    mut incoming: Incoming,
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
    // The same reason: the accept arm waits out its backoff on this one.
    let mut backoff_rx = accept_rx.clone();

    // Accept loop — runs until shutdown signal.
    loop {
        tokio::select! {
            result = incoming.accept() => {
                // A failed accept, such as EMFILE when the process is out
                // of file descriptors, ends nothing: log it, wait out the
                // backoff, and accept again. Under the pre-auth target, like
                // every accept-loop diagnostic, because a connection flood
                // can provoke it.
                let (tcp_stream, peer_addr) = match result {
                    Ok(accepted) => accepted,
                    Err(e) => {
                        tracing::warn!(target: crate::telemetry::PREAUTH_TRANSPORT_TARGET, event_type = "accept_failed", error = %e, "accept failed; accepting again after a backoff");
                        tokio::select! {
                            () = tokio::time::sleep(ACCEPT_ERROR_BACKOFF) => continue,
                            () = shutdown_observed(&mut backoff_rx) => {
                                tracing::info!(event_type = "lifecycle", "shutdown: stopping accept loop");
                                break;
                            }
                        }
                    }
                };

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
                        hyper::service::service_fn(move |mut req: Request<hyper::body::Incoming>| {
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
