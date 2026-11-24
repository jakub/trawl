// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::future::{self, Future};

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderValue, header};
use fleet_auth::{SessionPayload, session};

use crate::error::ProxyError;
use crate::state::AppState;

#[derive(Debug)]
pub struct Session(pub SessionPayload);

impl Session {
    #[must_use]
    pub fn token(&self) -> &str {
        self.0.token.as_str()
    }

    /// Absolute unix-second expiry timestamp of the session.
    #[must_use]
    pub fn exp(&self) -> i64 {
        self.0.exp.as_unix_seconds()
    }
}

impl FromRequestParts<AppState> for Session {
    type Rejection = ProxyError;

    fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        future::ready(session_from_parts(parts, state))
    }
}

fn session_from_parts(parts: &Parts, state: &AppState) -> Result<Session, ProxyError> {
    // The origin guard (ADR-0016), first, so no cookie state can reach the
    // answer. `fleet_auth::check_origin` has already logged the rejection.
    fleet_auth::check_origin(&parts.headers, state.public_origins(), "session")
        .map_err(|_| ProxyError::OriginMismatch)?;

    let cookie_header = parts
        .headers
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .ok_or(ProxyError::Unauthorized)?;

    let cookie_value =
        find_cookie(cookie_header, state.cookie_name()).ok_or(ProxyError::Unauthorized)?;

    let payload =
        session::decrypt(state.cookie_key(), cookie_value).map_err(|_| ProxyError::Unauthorized)?;

    let now = chrono::Utc::now().timestamp();
    if session::is_expired(&payload, now) {
        let clear_cookie = state
            .build_clear_cookie()
            .map_err(|e| ProxyError::Internal(e.to_string()))?;
        return Err(ProxyError::ExpiredSession { clear_cookie });
    }

    Ok(Session(payload))
}

#[derive(Debug)]
pub enum Auth {
    Session(Session),
    Bearer(String),
}

impl Auth {
    #[must_use]
    pub fn token(&self) -> &str {
        match self {
            Self::Session(s) => s.token(),
            Self::Bearer(t) => t.as_str(),
        }
    }
}

impl FromRequestParts<AppState> for Auth {
    type Rejection = ProxyError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // This branch never reads the cookie, so a session the browser happens to
        // be sending alongside is not spendable here, and a foreign page cannot
        // supply the token itself: trawl-web grants no preflight (it sends no
        // `Access-Control-Allow-*` header anywhere).
        if let Some(token) = extract_bearer(parts.headers.get(header::AUTHORIZATION)) {
            return Ok(Self::Bearer(token));
        }
        Session::from_request_parts(parts, state)
            .await
            .map(Self::Session)
    }
}

fn extract_bearer(header: Option<&HeaderValue>) -> Option<String> {
    let value = header?.to_str().ok()?;
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

/// No URL-decoding — session cookie values are base64url, which is cookie-safe.
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
            "leading/trailing whitespace around pairs should be tolerated"
        );
    }

    #[test]
    fn find_cookie_ignores_legacy_trawl_session() {
        assert_eq!(
            find_cookie("trawl_session=old; fleet_session=new", "fleet_session"),
            Some("new")
        );
    }

    #[test]
    fn extract_bearer_case_insensitive() {
        let hv = |s: &str| Some(HeaderValue::from_str(s).unwrap());
        assert_eq!(
            extract_bearer(hv("Bearer flt_tok").as_ref()),
            Some("flt_tok".to_owned())
        );
        assert_eq!(
            extract_bearer(hv("bearer flt_tok").as_ref()),
            Some("flt_tok".to_owned())
        );
        assert_eq!(
            extract_bearer(hv("BEARER flt_tok").as_ref()),
            Some("flt_tok".to_owned())
        );
    }

    #[test]
    fn extract_bearer_rejects_empty_and_non_bearer() {
        let hv = |s: &str| Some(HeaderValue::from_str(s).unwrap());
        assert_eq!(extract_bearer(hv("Bearer ").as_ref()), None);
        assert_eq!(extract_bearer(hv("Bearer   ").as_ref()), None);
        assert_eq!(extract_bearer(hv("Basic dXNlcjpwYXNz").as_ref()), None);
        assert_eq!(extract_bearer(None), None);
    }
}
