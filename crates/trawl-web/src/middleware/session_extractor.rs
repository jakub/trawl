// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `Session` extractor: decrypts the session cookie, rejects expired /
//! missing / tampered cookies with 401, and makes the payload available
//! to handlers via `State<AppState>` + `Session`.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;

use crate::error::ProxyError;
use crate::session::{self, SESSION_COOKIE, SessionPayload};
use crate::state::AppState;

/// A decrypted, non-expired session, injected into handlers.
///
/// The inner `SessionPayload` carries the bearer token (already unwrapped
/// from `Zeroizing` at handler time — still scrubbed on drop) plus the
/// identity fields needed for `/me` responses and authorization checks.
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
        self.0.exp
    }
}

impl FromRequestParts<AppState> for Session {
    type Rejection = ProxyError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let cookie_header = parts
            .headers
            .get(axum::http::header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .ok_or(ProxyError::Unauthorized)?;

        let cookie_value =
            find_cookie(cookie_header, SESSION_COOKIE).ok_or(ProxyError::Unauthorized)?;

        let payload = session::decrypt(state.cookie_key(), cookie_value)
            .map_err(|_| ProxyError::Unauthorized)?;

        let now = chrono::Utc::now().timestamp();
        if session::is_expired(&payload, now) {
            return Err(ProxyError::Unauthorized);
        }

        Ok(Session(payload))
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
            find_cookie("foo=bar; trawl_session=abc; baz=qux", "trawl_session"),
            Some("abc")
        );
        assert_eq!(
            find_cookie("trawl_session=xyz", "trawl_session"),
            Some("xyz")
        );
        assert_eq!(find_cookie("x=1;y=2", "trawl_session"), None);
        assert_eq!(find_cookie("", "trawl_session"), None);
    }

    #[test]
    fn find_cookie_handles_whitespace() {
        assert_eq!(
            find_cookie("  foo=bar ; trawl_session=value  ;  x=y", "trawl_session"),
            Some("value"),
            "leading/trailing whitespace around pairs should be tolerated"
        );
    }
}
