// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared request-scoped state.
//!
//! `AppState` is cloned into every handler via axum's `State` extractor.
//! Heavy values (reqwest client, session key) live behind `Arc` so clones
//! are cheap.
//!
//! `AppState` is also the single source of truth for session cookie
//! headers: [`AppState::build_session_cookie`] and
//! [`AppState::build_clear_cookie`] guarantee that every issue site and
//! every clear site (login, logout, extractor, error mapping, proxy 401
//! paths) emits the exact same attribute set — browsers reject clear
//! directives whose `Domain`/`Path`/`SameSite` don't match issuance.

use std::sync::Arc;

use axum::http::HeaderValue;
use axum::http::header::InvalidHeaderValue;
use fleet_auth::{
    DEFAULT_COOKIE_NAME, SameSite, SessionKey, build_clear_cookie_header,
    build_session_cookie_header,
};
use reqwest::Client;

use crate::config::ResolvedConfig;

/// Handler-visible runtime state. `Arc`-internals ensure cloning is O(1).
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    cookie_key: SessionKey,
    http: Client,
    upstream_url: String,
    coastwatch_url: Option<String>,
    session_ttl_secs: u64,
    allow_insecure_cookies: bool,
    shared_domain: Option<String>,
}

impl AppState {
    /// Build state from resolved config. Constructs the reqwest client
    /// once (connection pooling + keep-alive are implicit). Installs the
    /// rustls `ring` crypto provider on first call (idempotent — ignored
    /// if another provider is already installed).
    ///
    /// # Errors
    /// Propagates `reqwest::Error` if the client can't be built.
    pub fn from_config(cfg: ResolvedConfig) -> Result<Self, reqwest::Error> {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let http = Client::builder()
            .danger_accept_invalid_certs(cfg.insecure_upstream_tls)
            .build()?;

        Ok(Self {
            inner: Arc::new(Inner {
                cookie_key: cfg.cookie_key,
                http,
                upstream_url: cfg.upstream_url,
                coastwatch_url: cfg.coastwatch_url,
                session_ttl_secs: cfg.session_ttl_secs,
                allow_insecure_cookies: cfg.allow_insecure_cookies,
                shared_domain: cfg.shared_domain,
            }),
        })
    }

    #[must_use]
    pub fn cookie_key(&self) -> &SessionKey {
        &self.inner.cookie_key
    }

    /// Name of the session cookie: the fleet-wide `fleet_session`
    /// (hardcoded — the shared name IS the SSO contract, not a config knob).
    #[must_use]
    pub fn cookie_name(&self) -> &'static str {
        DEFAULT_COOKIE_NAME
    }

    /// Parent domain for the SSO cookie's `Domain=` attribute, or `None`
    /// in standalone mode.
    #[must_use]
    pub fn shared_domain(&self) -> Option<&str> {
        self.inner.shared_domain.as_deref()
    }

    /// Build the `Set-Cookie` header value for a live session cookie.
    ///
    /// `SameSite=Lax` is hardcoded — a correctness requirement for
    /// parent-domain SSO (ADR-0030); `Strict` would silently break
    /// cross-subdomain navigation.
    #[must_use]
    pub fn build_session_cookie(&self, value: String) -> String {
        build_session_cookie_header(
            self.cookie_name(),
            value,
            self.inner.session_ttl_secs,
            !self.inner.allow_insecure_cookies,
            SameSite::Lax,
            self.shared_domain(),
        )
    }

    /// Build the `Set-Cookie` header value that clears the session cookie.
    ///
    /// Attributes are guaranteed to match [`Self::build_session_cookie`]
    /// (same name, `SameSite`, `Path`, `Secure`, `Domain`) — a mismatched
    /// clear is silently ignored by browsers.
    ///
    /// Returns a validated [`HeaderValue`] so callers carry a typed clear
    /// directive rather than a stringly-typed one that could fail to parse
    /// (and be silently dropped) at response-assembly time.
    ///
    /// # Errors
    /// Returns [`InvalidHeaderValue`] if the serialized cookie isn't a valid
    /// header value — impossible for the fully-controlled attribute set, but
    /// surfaced explicitly rather than degrading to a missing `Set-Cookie`.
    pub fn build_clear_cookie(&self) -> Result<HeaderValue, InvalidHeaderValue> {
        build_clear_cookie_header(
            self.cookie_name(),
            !self.inner.allow_insecure_cookies,
            SameSite::Lax,
            self.shared_domain(),
        )
        .parse()
    }

    #[must_use]
    pub fn http(&self) -> &Client {
        &self.inner.http
    }

    #[must_use]
    pub fn upstream_url(&self) -> &str {
        &self.inner.upstream_url
    }

    #[must_use]
    pub fn coastwatch_url(&self) -> Option<&str> {
        self.inner.coastwatch_url.as_deref()
    }

    #[must_use]
    pub fn session_ttl_secs(&self) -> u64 {
        self.inner.session_ttl_secs
    }

    #[must_use]
    pub fn allow_insecure_cookies(&self) -> bool {
        self.inner.allow_insecure_cookies
    }
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("upstream_url", &self.inner.upstream_url)
            .field("coastwatch_url", &self.inner.coastwatch_url)
            .field("session_ttl_secs", &self.inner.session_ttl_secs)
            .field("allow_insecure_cookies", &self.inner.allow_insecure_cookies)
            .field("shared_domain", &self.inner.shared_domain)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn state(cookie_secure: bool) -> AppState {
        AppState::from_config(ResolvedConfig {
            bind_addr: "127.0.0.1:8090".into(),
            upstream_url: "http://127.0.0.1:5514".into(),
            coastwatch_url: None,
            session_ttl_secs: 3_600,
            allow_insecure_cookies: !cookie_secure,
            insecure_upstream_tls: false,
            cookie_key: SessionKey::from_bytes([0x42; fleet_auth::KEY_LEN]),
            shared_domain: None,
        })
        .unwrap()
    }

    fn scope_attributes(header: &str) -> BTreeSet<String> {
        header
            .split(';')
            .skip(1)
            .map(str::trim)
            .filter(|attribute| !attribute.starts_with("Max-Age="))
            .map(str::to_owned)
            .collect()
    }

    fn assert_issue_and_clear_scope_match(state: &AppState) -> (String, String) {
        let issued = state.build_session_cookie("encrypted-value".into());
        let cleared = state
            .build_clear_cookie()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(scope_attributes(&issued), scope_attributes(&cleared));
        assert!(issued.starts_with("fleet_session=encrypted-value;"));
        assert!(cleared.starts_with("fleet_session=;"));
        (issued, cleared)
    }

    #[test]
    fn localhost_cookie_shape_is_insecure_and_host_only() {
        let (issued, cleared) = assert_issue_and_clear_scope_match(&state(false));
        for header in [&issued, &cleared] {
            assert!(header.contains("HttpOnly"), "got: {header}");
            assert!(header.contains("SameSite=Lax"), "got: {header}");
            assert!(header.contains("Path=/"), "got: {header}");
            assert!(!header.contains("Secure"), "got: {header}");
            assert!(
                !header.to_ascii_lowercase().contains("domain="),
                "got: {header}"
            );
        }
    }

    #[test]
    fn tailscale_cookie_shape_is_secure_and_host_only() {
        let (issued, cleared) = assert_issue_and_clear_scope_match(&state(true));
        for header in [&issued, &cleared] {
            assert!(header.contains("HttpOnly"), "got: {header}");
            assert!(header.contains("SameSite=Lax"), "got: {header}");
            assert!(header.contains("Path=/"), "got: {header}");
            assert!(header.contains("Secure"), "got: {header}");
            assert!(
                !header.to_ascii_lowercase().contains("domain="),
                "got: {header}"
            );
        }
    }
}
