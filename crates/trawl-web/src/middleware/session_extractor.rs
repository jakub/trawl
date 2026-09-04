// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `Session` extractor: runs the ADR-0016 origin guard, decrypts the
//! session cookie, rejects expired / missing / tampered cookies with 401,
//! and makes the payload available to handlers via `State<AppState>` +
//! `Session`.
//!
//! The guard lives here, and only here, because this is the one place a
//! cookie session comes into being. A handler cannot forget to call it,
//! since a handler cannot obtain a `Session` (or a cookie-backed `Auth`)
//! without passing it first. That is not a stylistic preference: while the
//! check was a call each handler made for itself, both SSE routes had
//! simply never made it, so any page anywhere could open an `EventSource`
//! against `/api/v1/stream`, ride the victim's `fleet_session` cookie and
//! read their logs. Moving the guard here deleted that bug rather than
//! fixing it, and the same move covers every route added from now on.
//!
//! Precedence, and the reasons for it:
//! - A valid `Authorization: Bearer` header wins outright and skips the
//!   guard. Bearer clients hold no cookie, so they are not CSRF targets,
//!   and no foreign page can make a browser attach someone else's bearer
//!   token.
//! - Otherwise the guard runs BEFORE the cookie is looked up, decrypted or
//!   checked for expiry. The verdict must not depend on what the browser
//!   happens to be holding: a foreign origin sent with an expired cookie
//!   is a 403 with no `Set-Cookie`, never the expiry branch's clear
//!   directive, which would otherwise let any page log a user out.
//! - A missing, tampered or expired cookie then answers exactly as it did
//!   before the guard existed.

use std::future::{self, Future};

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderValue, header};
use fleet_auth::{SessionPayload, session};

use crate::error::ProxyError;
use crate::state::AppState;

/// A decrypted, non-expired session, injected into handlers.
///
/// The inner `SessionPayload` carries just two things handlers consume: the
/// bearer token (already unwrapped from `Zeroizing` at handler time — still
/// scrubbed on drop) for authenticating upstream calls, and `exp` for capping
/// SSE stream duration. Identity for `/me` is fetched live from upstream
/// `/whoami`, not read from cookie contents.
#[derive(Debug)]
pub struct Session(pub SessionPayload);

impl Session {
    #[must_use]
    pub fn token(&self) -> &str {
        self.0.token.as_str()
    }

    /// Absolute unix-second expiry timestamp of the session.
    ///
    /// Used by the SSE handler to cap stream duration at expiry time —
    /// preventing a request opened just before expiry from keeping its
    /// stream alive indefinitely.
    #[must_use]
    pub fn exp(&self) -> i64 {
        self.0.exp.as_unix_seconds()
    }
}

impl FromRequestParts<AppState> for Session {
    type Rejection = ProxyError;

    // Not `async fn`: nothing here awaits, and the trait asks only for a
    // future, so hand back a ready one.
    fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        future::ready(session_from_parts(parts, state))
    }
}

fn session_from_parts(parts: &Parts, state: &AppState) -> Result<Session, ProxyError> {
    // The whole-origin comparison against the deployment's configured
    // `public_origins` (ADR-0016), first, so no cookie state can reach the
    // answer. `fleet_auth::check_origin` reads the `Origin` field set and
    // nothing else: not `Host`, not the request URI's authority, not any
    // `X-Forwarded-*` header, because under a reverse proxy those are the
    // proxy's opinion and a CSRF verdict that moves with them is a verdict
    // the deployment topology can flip. It has already logged the
    // rejection with its bounded fields; here it becomes a 403 that
    // touches no cookie.
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
        // Expired is distinct from missing/tampered: the browser
        // IS presenting a cookie, it just can't be redeemed. Tell
        // it to drop the cookie so subsequent requests don't keep
        // sending a token we'll always reject.
        let clear_cookie = state
            .build_clear_cookie()
            .map_err(|e| ProxyError::Internal(e.to_string()))?;
        return Err(ProxyError::ExpiredSession { clear_cookie });
    }

    Ok(Session(payload))
}

/// Auth source for proxy handlers: either a decrypted cookie session or a
/// raw bearer token passed through verbatim.
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
        // A valid bearer header wins before the origin guard is consulted:
        // a client that authenticates by header carries no cookie for a
        // foreign page to spend, so its `Origin` is not a verdict. Every
        // other request falls through to the cookie branch, where the
        // guard runs first.
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

/// Parse a Cookie header and return the value of `name`, or None if absent.
///
/// Handles the standard `name1=v1; name2=v2; name3=v3` form. No URL-decoding
/// — session cookie values are base64url, which is cookie-safe.
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
        // A neighbouring cookie whose name merely resembles the one asked
        // for must not satisfy the lookup: find_cookie matches whole names.
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
