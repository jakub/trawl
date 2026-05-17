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

    let Some(cookie_value) = find_cookie(cookie_header, &state.config.cookie_name) else {
        return unauthorized_json("missing session cookie");
    };

    let Ok(payload) = session::decrypt(&state.session_key, cookie_value) else {
        return unauthorized_json("invalid session");
    };

    let now = chrono::Utc::now().timestamp();
    if session::is_expired(&payload, now) {
        return unauthorized_json("session expired");
    }

    let Ok(verified) = state.store.verify_key(payload.token.as_str()).await else {
        return unauthorized_json("invalid session");
    };

    if verified.role_for(&state.config.app_namespace).is_none() {
        return no_grant_response(&verified.name, &state.config.app_namespace);
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
/// Does NOT check namespace grants — bearer service principals legitimately
/// span apps, and the no-grant 403 HTML page is a UI affordance that
/// doesn't apply to API clients. Apps gate further with their own role
/// guards on top.
pub async fn require_bearer(
    State(state): State<SessionState>,
    mut req: Request,
    next: Next,
) -> Response {
    let Some(token) = extract_bearer(req.headers()) else {
        return unauthorized_json("missing or malformed bearer token");
    };

    let Ok(verified) = state.store.verify_key(&token).await else {
        return unauthorized_json("invalid bearer token");
    };

    req.extensions_mut().insert(verified);
    next.run(req).await
}

/// Parse a Cookie header and return the value of `name`, or None if absent.
///
/// Handles the standard `name1=v1; name2=v2; name3=v3` form. No
/// URL-decoding — session cookie values are base64url, which is cookie-safe.
fn find_cookie<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    for pair in header.split(';') {
        let pair = pair.trim();
        if let Some((k, v)) = pair.split_once('=')
            && k == name
        {
            return Some(v);
        }
    }
    None
}

/// Extract the token from an `Authorization: Bearer <token>` header.
///
/// Case-insensitive on the scheme name (RFC 7235 §2.1). Rejects empty
/// tokens and any non-Bearer scheme.
fn extract_bearer(headers: &HeaderMap<HeaderValue>) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_owned())
    }
}

fn unauthorized_json(message: &str) -> Response {
    let body = format!(r#"{{"error":"unauthorized","detail":"{message}"}}"#);
    (
        StatusCode::UNAUTHORIZED,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

/// Build the no-grant 403 HTML body.
///
/// Cookie is NOT cleared (per ADR-0030: the user can navigate back to a
/// sibling app where they DO have a grant without re-authenticating).
fn no_grant_response(name: &str, app: &str) -> Response {
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
        assert_eq!(extract_bearer(&h), Some("flt_tok".to_owned()));

        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("bearer flt_tok"),
        );
        assert_eq!(extract_bearer(&h), Some("flt_tok".to_owned()));

        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("BEARER flt_tok"),
        );
        assert_eq!(extract_bearer(&h), Some("flt_tok".to_owned()));
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
