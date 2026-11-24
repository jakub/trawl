// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Axum middleware: [`require_session`] for cookie auth, [`require_bearer`]
//! for `Authorization: Bearer flt_...` auth (ADR-0030).

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::error::AuthError;
use crate::session::{self, SessionConfig, SessionKey};
use crate::store::KeyStore;

#[derive(Clone)]
pub struct SessionState {
    store: KeyStore,
    session_key: Arc<SessionKey>,
    config: Arc<SessionConfig>,
}

impl SessionState {
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

    #[must_use]
    pub fn store(&self) -> &KeyStore {
        &self.store
    }

    #[must_use]
    pub fn session_key(&self) -> &SessionKey {
        &self.session_key
    }

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

/// Missing/malformed/decrypt-fail/expired cookie → 401 (no cookie cleared
/// — clearing the shared `fleet_session` cookie from one app's error
/// handler logs the user out of every sibling app).
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

    if !verified.has_any_permission(state.config.app_namespace()) {
        tracing::info!(
            app = state.config.app_namespace(),
            name = %verified.name,
            "auth: session valid but no permission in app namespace (403 no-grant)"
        );
        return no_grant_response(&verified.name, state.config.app_namespace());
    }

    req.extensions_mut().insert(verified);
    next.run(req).await
}

/// This middleware authenticates, it does not authorise. Forgetting the
/// permission guard on a downstream route accepts any valid fleet token from
/// any sibling app.
pub async fn require_bearer(
    State(state): State<SessionState>,
    req: Request,
    next: Next,
) -> Response {
    verify_bearer(&state.store, req, next).await
}

#[derive(Clone)]
pub struct BearerState {
    store: KeyStore,
}

impl BearerState {
    #[must_use]
    pub fn new(store: KeyStore) -> Self {
        Self { store }
    }

    #[must_use]
    pub fn store(&self) -> &KeyStore {
        &self.store
    }
}

impl std::fmt::Debug for BearerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BearerState")
            .field("store", &"<KeyStore>")
            .finish()
    }
}

/// Like [`require_bearer`], this authenticates but does not authorise.
pub async fn require_bearer_only(
    State(state): State<BearerState>,
    req: Request,
    next: Next,
) -> Response {
    verify_bearer(&state.store, req, next).await
}

async fn verify_bearer(store: &KeyStore, mut req: Request, next: Next) -> Response {
    let Some(token) = extract_bearer(req.headers()) else {
        tracing::warn!("auth: missing or malformed bearer header");
        return unauthorized_json("missing or malformed bearer token");
    };

    let verified = match store.verify_key(token).await {
        Ok(v) => v,
        Err(err) => return classify_verify_error(err, "bearer"),
    };

    req.extensions_mut().insert(verified);
    next.run(req).await
}

/// Returns the LAST match (not the first) when the same name appears more
/// than once. Pick the last value as a deterministic tiebreak so behaviour
/// is stable across clients.
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

fn extract_bearer(headers: &HeaderMap<HeaderValue>) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    if token.is_empty() { None } else { Some(token) }
}

pub(crate) fn error_response(status: StatusCode, kind: &str, detail: &str) -> Response {
    let body = serde_json::json!({ "error": kind, "detail": detail }).to_string();
    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
}

pub(crate) fn unauthorized_json(message: &str) -> Response {
    error_response(StatusCode::UNAUTHORIZED, "unauthorized", message)
}

/// Distinct from 401 so operators can alarm separately on "auth backend
/// down" and "wrong credentials": collapsed into 401, a Postgres blip reads
/// as a brute-force attack.
pub(crate) fn service_unavailable_json(detail: &str) -> Response {
    error_response(StatusCode::SERVICE_UNAVAILABLE, "unavailable", detail)
}

/// `path` is one of "session", "bearer" or "login": it names the auth path in
/// the log line and picks the 401 detail wording, so any other tag reads as a
/// session failure.
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
        AuthError::Schema(err) => {
            tracing::error!(target: "auth.backend", %path, ?err, "auth: schema error");
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

pub(crate) fn no_grant_response(name: &str, app: &str) -> Response {
    let body = format!(
        "<!doctype html>\n\
         <html lang=\"en\"><head><meta charset=\"utf-8\">\
         <title>403 Forbidden</title></head>\
         <body>\n\
         <h1>403 Forbidden</h1>\n\
         <p>You are authenticated as <strong>{name}</strong> but have no access in <strong>{app}</strong>.</p>\n\
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
