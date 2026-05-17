// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Axum middleware: [`require_session`] for cookie auth, [`require_bearer`]
//! for `Authorization: Bearer flt_...` auth (ADR-0030).
//!
//! Both insert a [`VerifiedKey`] into request extensions on success.
//! Handlers extract it with `axum::Extension<VerifiedKey>`.
//!
//! Wiring pattern (callers):
//!
//! ```ignore
//! use axum::middleware::from_fn_with_state;
//! use fleet_auth::{SessionState, require_session, require_bearer};
//!
//! let state = SessionState::new(keystore, session_key, session_config);
//!
//! let web_router = Router::new()
//!     .route("/dashboard", get(dashboard))
//!     .route_layer(from_fn_with_state(state.clone(), require_session));
//!
//! let api_router = Router::new()
//!     .route("/ingest", post(ingest))
//!     .route_layer(from_fn_with_state(state.clone(), require_bearer));
//! ```

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::error::AuthError;
use crate::session::{self, SessionConfig, SessionKey};
use crate::store::KeyStore;

/// Shared per-app state for [`require_session`], [`require_bearer`], and
/// the login/logout handler factories.
///
/// Cheap to `Clone` — `KeyStore` wraps `Arc`-backed Postgres pool and cache,
/// and the session key + config are wrapped in [`Arc`] explicitly so the
/// secret material is shared rather than duplicated.
#[derive(Clone)]
pub struct SessionState {
    store: KeyStore,
    session_key: Arc<SessionKey>,
    config: Arc<SessionConfig>,
}

impl SessionState {
    /// Construct a `SessionState`. Validates the embedded [`SessionConfig`]
    /// up front so misconfigurations surface at startup, not on first
    /// request.
    ///
    /// # Errors
    /// Returns the underlying [`AuthError`] when `config.validate()` fails
    /// (e.g. empty `app_namespace`, empty `cookie_name`, malformed
    /// `post_login_redirect`).
    pub fn new(
        store: KeyStore,
        session_key: Arc<SessionKey>,
        config: Arc<SessionConfig>,
    ) -> Result<Self, AuthError> {
        config.validate()?;
        Ok(Self {
            store,
            session_key,
            config,
        })
    }

    /// Borrow the underlying [`KeyStore`].
    #[must_use]
    pub fn store(&self) -> &KeyStore {
        &self.store
    }

    /// Borrow the shared [`SessionKey`].
    #[must_use]
    pub fn session_key(&self) -> &SessionKey {
        &self.session_key
    }

    /// Borrow the shared [`SessionConfig`].
    #[must_use]
    pub fn config(&self) -> &SessionConfig {
        &self.config
    }
}

impl std::fmt::Debug for SessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionState")
            .field("config", &self.config)
            .field("session_key", &"<redacted>")
            .field("store", &"<KeyStore>")
            .finish()
    }
}

/// Axum middleware: require a valid session cookie + namespace grant.
///
/// Flow: parse Cookie header → find `config.cookie_name` → decrypt → check
/// expiry → `KeyStore::verify_key` → confirm the key has a role in
/// `config.app_namespace` → insert [`VerifiedKey`] into request extensions
/// → call inner.
///
/// Failure mapping:
/// - Missing/malformed/decrypt-fail/expired cookie → 401 (no cookie cleared
///   — clearing the shared `fleet_session` cookie from one app's error
///   handler logs the user out of every sibling app).
/// - `verify_key` failure (revoked, retyped, expired-at-DB) → 401.
/// - Valid session but no grant for `config.app_namespace` → branded 403
///   HTML page, cookie left intact so the user can navigate back to a
///   sibling app where they DO have a grant.
pub async fn require_session(
    State(state): State<SessionState>,
    mut req: Request,
    next: Next,
) -> Response {
    let Some(cookie_header) = req
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
    else {
        return unauthorized_json("missing session cookie");
    };

    let Some(cookie_value) = find_cookie(cookie_header, state.config.cookie_name()) else {
        return unauthorized_json("missing session cookie");
    };

    let Ok(payload) = session::decrypt(&state.session_key, cookie_value) else {
        tracing::warn!("auth: session decrypt failed (tampered or wrong key)");
        return unauthorized_json("invalid session");
    };

    let now = chrono::Utc::now().timestamp();
    if session::is_expired(&payload, now) {
        tracing::info!(
            exp = payload.exp.as_unix_seconds(),
            now,
            "auth: session expired"
        );
        return unauthorized_json("session expired");
    }

    let verified = match state.store.verify_key(payload.token.as_str()).await {
        Ok(v) => v,
        Err(err) => return classify_verify_error(err, "session"),
    };

    if verified.role_for(state.config.app_namespace()).is_none() {
        tracing::info!(
            app = state.config.app_namespace(),
            name = %verified.name,
            "auth: session valid but no grant in app namespace (403 no-grant)"
        );
        return no_grant_response(&verified.name, state.config.app_namespace());
    }

    req.extensions_mut().insert(verified);
    next.run(req).await
}

/// Axum middleware: require a valid `Authorization: Bearer flt_...` header.
///
/// Flow: parse Authorization header → extract bearer token →
/// `KeyStore::verify_key` → insert [`VerifiedKey`] into request extensions
/// → call inner.
///
/// # Important — does NOT check the app namespace grant
///
/// Unlike [`require_session`], this layer accepts *any* verified key
/// regardless of which app(s) the key has grants in. This is intentional
/// (ADR-0030: cross-app service principals must be able to call API
/// routes), but it means **mounting this layer on a route is NOT
/// sufficient authorisation** — every consumer MUST also gate the route
/// with its own role guard that inspects `verified.role_for(app)` and
/// rejects requests without a grant.
///
/// In other words: this middleware authenticates, it does not authorise.
/// Forgetting the role guard on a downstream route accepts any valid
/// fleet token from any sibling app. The integration test
/// `bearer_does_not_enforce_namespace` locks this behaviour in.
pub async fn require_bearer(
    State(state): State<SessionState>,
    mut req: Request,
    next: Next,
) -> Response {
    let Some(token) = extract_bearer(req.headers()) else {
        tracing::warn!("auth: missing or malformed bearer header");
        return unauthorized_json("missing or malformed bearer token");
    };

    let verified = match state.store.verify_key(token).await {
        Ok(v) => v,
        Err(err) => return classify_verify_error(err, "bearer"),
    };

    req.extensions_mut().insert(verified);
    next.run(req).await
}

/// Parse a Cookie header and return the value of `name`, or None if absent.
///
/// Returns the LAST match (not the first) when the same name appears more
/// than once. RFC 6265 §5.4 doesn't fully define ordering when multiple
/// cookies with the same name exist at different domain scopes
/// (parent-domain vs subdomain) — browsers vary. Pick the last value as a
/// deterministic tiebreak so behaviour is stable across clients. Practical
/// impact is minimal because `fleet_session` is always set at `path=/` and
/// any domain-scope collision implies a misconfigured `Domain=` attribute,
/// which an operator should fix upstream.
///
/// No URL-decoding — session cookie values are base64url, which is
/// cookie-safe.
fn find_cookie<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    let mut found = None;
    for pair in header.split(';') {
        let pair = pair.trim();
        if let Some((k, v)) = pair.split_once('=')
            && k == name
        {
            found = Some(v);
        }
    }
    found
}

/// Extract the token from an `Authorization: Bearer <token>` header.
///
/// Case-insensitive on the scheme name (RFC 7235 §2.1). Rejects empty
/// tokens and any non-Bearer scheme. Borrows from the header — no
/// per-request alloc on the bearer hot path.
fn extract_bearer(headers: &HeaderMap<HeaderValue>) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    if token.is_empty() { None } else { Some(token) }
}

/// Build a small `{"error": ..., "detail": ...}` JSON body for an error
/// response. Uses `serde_json` so `detail` can never break JSON framing —
/// crucial because callers pass arbitrary user-facing strings, and a future
/// caller that forwards user-controlled bytes here would otherwise turn this
/// into a JSON-injection footgun.
pub(crate) fn error_response(status: StatusCode, kind: &str, detail: &str) -> Response {
    let body = serde_json::json!({ "error": kind, "detail": detail }).to_string();
    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
}

pub(crate) fn unauthorized_json(message: &str) -> Response {
    error_response(StatusCode::UNAUTHORIZED, "unauthorized", message)
}

/// 503 Service Unavailable for transient backend failures in the auth path.
///
/// Distinct from 401 so operators can alarm separately on "auth backend
/// down" vs "wrong credentials" — collapsing the two during a Postgres
/// blip used to send on-call hunting for a brute-force attack while the
/// DB was actually just rebooting. Paired with `tracing::error!(target:
/// "auth.backend", ...)` at the call site for filterable alerting.
pub(crate) fn service_unavailable_json(detail: &str) -> Response {
    error_response(StatusCode::SERVICE_UNAVAILABLE, "unavailable", detail)
}

/// Classify a [`KeyStore::verify_key`] failure into the right HTTP response.
///
/// Centralised so the three auth paths (session, bearer, login) agree on
/// status codes + tracing targets:
///
/// - `Database` / `Migration` / `Hash` / `TokenGeneration` → 503 with
///   `tracing::error!(target: "auth.backend", ...)`, so operators can
///   alarm on backend health independently of 401 spikes.
/// - `InvalidKey` / `MalformedToken` → 401 with `tracing::warn!` —
///   legitimate auth failure.
/// - Anything else → 500 + `tracing::error!`. Shouldn't happen in this
///   call path (`verify_key` doesn't produce admin-only variants), so a
///   loud signal beats a silent one.
///
/// `path` is a short tag ("session", "bearer", "login") that gets folded
/// into the log line so the same KDF panic looks different depending on
/// which surface it hit.
pub(crate) fn classify_verify_error(err: AuthError, path: &str) -> Response {
    match err {
        AuthError::Database(err) => {
            tracing::error!(target: "auth.backend", %path, ?err, "auth: db error");
            service_unavailable_json("auth backend unavailable")
        }
        AuthError::Migration(err) => {
            tracing::error!(target: "auth.backend", %path, ?err, "auth: migration error");
            service_unavailable_json("auth backend unavailable")
        }
        AuthError::Hash(err) => {
            tracing::error!(target: "auth.backend", %path, err, "auth: hash worker failed");
            service_unavailable_json("auth backend unavailable")
        }
        AuthError::TokenGeneration(err) => {
            tracing::error!(target: "auth.backend", %path, err, "auth: token generation exhausted");
            service_unavailable_json("auth backend unavailable")
        }
        err @ (AuthError::InvalidKey(_) | AuthError::MalformedToken(_)) => {
            tracing::warn!(%path, ?err, "auth: invalid or revoked key");
            let detail = if path == "bearer" {
                "invalid bearer token"
            } else if path == "login" {
                "invalid api key"
            } else {
                "invalid session"
            };
            unauthorized_json(detail)
        }
        err => {
            tracing::error!(%path, ?err, "auth: unexpected verify_key error");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "auth check failed",
            )
        }
    }
}

/// Build the no-grant 403 HTML body.
///
/// Cookie is NOT cleared (per ADR-0030: the user can navigate back to a
/// sibling app where they DO have a grant without re-authenticating).
pub(crate) fn no_grant_response(name: &str, app: &str) -> Response {
    let body = format!(
        "<!doctype html>\n\
         <html lang=\"en\"><head><meta charset=\"utf-8\">\
         <title>403 Forbidden</title></head>\
         <body>\n\
         <h1>403 Forbidden</h1>\n\
         <p>You are authenticated as <strong>{name}</strong> but have no role assigned in <strong>{app}</strong>.</p>\n\
         <p>Contact your operator to request access.</p>\n\
         </body></html>\n",
        name = html_escape(name),
        app = html_escape(app),
    );
    (
        StatusCode::FORBIDDEN,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
        .into_response()
}

/// Escape the five HTML special chars. Defence in depth — `app_namespace`
/// is validated at config time and `name` comes from the operator-controlled
/// `api_keys.name` column, but rendering raw user-supplied strings into HTML
/// without escaping is the kind of mistake one only makes once.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_cookie_picks_named_pair() {
        assert_eq!(
            find_cookie("foo=bar; fleet_session=abc; baz=qux", "fleet_session"),
            Some("abc")
        );
        assert_eq!(
            find_cookie("fleet_session=xyz", "fleet_session"),
            Some("xyz")
        );
        assert_eq!(find_cookie("x=1;y=2", "fleet_session"), None);
        assert_eq!(find_cookie("", "fleet_session"), None);
    }

    #[test]
    fn find_cookie_handles_whitespace() {
        assert_eq!(
            find_cookie("  foo=bar ; fleet_session=value  ;  x=y", "fleet_session"),
            Some("value"),
        );
    }

    #[test]
    fn extract_bearer_case_insensitive() {
        let mut h = HeaderMap::new();
        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer flt_tok"),
        );
        assert_eq!(extract_bearer(&h), Some("flt_tok"));

        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("bearer flt_tok"),
        );
        assert_eq!(extract_bearer(&h), Some("flt_tok"));

        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("BEARER flt_tok"),
        );
        assert_eq!(extract_bearer(&h), Some("flt_tok"));
    }

    #[test]
    fn extract_bearer_rejects_empty_and_non_bearer() {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer "));
        assert_eq!(extract_bearer(&h), None);

        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer   "));
        assert_eq!(extract_bearer(&h), None);

        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Basic dXNlcjpwYXNz"),
        );
        assert_eq!(extract_bearer(&h), None);

        let empty = HeaderMap::new();
        assert_eq!(extract_bearer(&empty), None);
    }

    #[test]
    fn html_escape_handles_specials() {
        assert_eq!(
            html_escape("alice<script>alert('xss')</script>&\""),
            "alice&lt;script&gt;alert(&#x27;xss&#x27;)&lt;/script&gt;&amp;&quot;",
        );
        assert_eq!(html_escape("plain"), "plain");
    }

    #[tokio::test]
    async fn no_grant_response_body_html_escaped_no_set_cookie() {
        let response = no_grant_response("ali<ce", "trawl");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
        assert!(
            !response.headers().contains_key(header::SET_COOKIE),
            "no-grant 403 must NOT clear the cookie (ADR-0030)"
        );
        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = std::str::from_utf8(&body_bytes).unwrap();
        assert!(body.contains("ali&lt;ce"), "got: {body}");
        assert!(body.contains("trawl"), "got: {body}");
        assert!(!body.contains("ali<ce"), "raw < must be escaped");
    }

    #[tokio::test]
    async fn unauthorized_json_has_application_json() {
        let response = unauthorized_json("session expired");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        assert!(
            !response.headers().contains_key(header::SET_COOKIE),
            "401 must NOT clear the shared cookie"
        );
        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = std::str::from_utf8(&body_bytes).unwrap();
        assert!(body.contains(r#""error":"unauthorized""#));
        assert!(body.contains(r#""detail":"session expired""#));
    }
}
