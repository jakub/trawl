// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! [`AppState::build_session_cookie`] (login) and
//! [`AppState::build_clear_cookie`] (logout, the session extractor's
//! expired-cookie path, `auth::me` on an upstream 401) emit one attribute
//! set, because browsers ignore a clear directive whose
//! `Domain`/`Path`/`SameSite` don't match issuance.

use std::sync::{Arc, Weak};
use std::time::Duration;

use axum::http::HeaderValue;
use axum::http::header::InvalidHeaderValue;
use fleet_auth::{
    DEFAULT_COOKIE_NAME, PublicOrigins, SameSite, SessionKey, build_clear_cookie_header,
    build_session_cookie_header,
};
use reqwest::Client;
use tokio::task::JoinHandle;

use crate::body_budget::BodyBudget;
use crate::config::{ProxyLimits, ResolvedConfig};
use crate::error::ProxyError;
use crate::upstream::Upstream;

#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    cookie_key: SessionKey,
    upstream: Upstream,
    upstream_url: String,
    session_ttl_secs: u64,
    allow_insecure_cookies: bool,
    shared_domain: Option<String>,
    public_origins: PublicOrigins,
    limits: ProxyLimits,
    body_budget: BodyBudget,
}

impl AppState {
    /// Starts no task: the caller starts the re-read with
    /// [`Self::spawn_upstream_ca_reread`].
    pub fn from_config(cfg: ResolvedConfig) -> Result<Self, reqwest::Error> {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let upstream = Upstream::new(
            cfg.upstream_tls,
            cfg.upstream_connect,
            cfg.limits.upstream_head_timeout,
        )?;

        Ok(Self {
            inner: Arc::new(Inner {
                cookie_key: cfg.cookie_key,
                upstream,
                upstream_url: cfg.upstream_url,
                session_ttl_secs: cfg.session_ttl_secs,
                allow_insecure_cookies: cfg.allow_insecure_cookies,
                shared_domain: cfg.shared_domain,
                public_origins: cfg.public_origins,
                body_budget: BodyBudget::new(cfg.limits.body_budget_bytes),
                limits: cfg.limits,
            }),
        })
    }

    #[must_use]
    pub fn limits(&self) -> &ProxyLimits {
        &self.inner.limits
    }

    #[must_use]
    pub fn body_budget(&self) -> &BodyBudget {
        &self.inner.body_budget
    }

    pub async fn send_to_upstream(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, ProxyError> {
        tokio::time::timeout(self.inner.limits.upstream_head_timeout, request.send())
            .await
            .map_err(|_elapsed| ProxyError::UpstreamHeadTimeout)?
            .map_err(ProxyError::Network)
    }

    #[must_use]
    pub fn cookie_key(&self) -> &SessionKey {
        &self.inner.cookie_key
    }

    #[must_use]
    pub fn cookie_name(&self) -> &'static str {
        DEFAULT_COOKIE_NAME
    }

    #[must_use]
    pub fn public_origins(&self) -> &PublicOrigins {
        &self.inner.public_origins
    }

    #[must_use]
    pub fn shared_domain(&self) -> Option<&str> {
        self.inner.shared_domain.as_deref()
    }

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

    pub fn build_clear_cookie(&self) -> Result<HeaderValue, InvalidHeaderValue> {
        build_clear_cookie_header(
            self.cookie_name(),
            !self.inner.allow_insecure_cookies,
            SameSite::Lax,
            self.shared_domain(),
        )
        .parse()
    }

    /// The client one request uses to reach trawld, from start to end,
    /// streamed response bodies included. A handler takes it once, after
    /// its local checks, so a request that fails those never waits on the
    /// pin file.
    pub async fn upstream_client(&self) -> Result<Client, ProxyError> {
        self.inner.upstream.client().await
    }

    #[must_use]
    pub fn spawn_upstream_ca_reread(&self, interval: Duration) -> Option<JoinHandle<()>> {
        if !self.inner.upstream.is_pinned() {
            return None;
        }
        let state: Weak<Inner> = Arc::downgrade(&self.inner);
        Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                let Some(inner) = state.upgrade() else {
                    break;
                };
                inner.upstream.reread().await;
            }
        }))
    }

    pub async fn reread_upstream_ca(&self) {
        self.inner.upstream.reread().await;
    }

    #[must_use]
    pub fn upstream_url(&self) -> &str {
        &self.inner.upstream_url
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
            .field("upstream", &self.inner.upstream)
            .field("upstream_url", &self.inner.upstream_url)
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
    use crate::config::UpstreamTls;

    #[tokio::test]
    async fn the_platform_roots_start_no_reread_task() {
        assert!(
            state(false)
                .spawn_upstream_ca_reread(Duration::from_millis(1))
                .is_none()
        );
    }

    #[tokio::test]
    async fn the_reread_task_ends_with_its_state() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::from_config(ResolvedConfig {
            upstream_tls: UpstreamTls::PinnedCa {
                path: dir.path().join("absent.pem"),
                roots: None,
            },
            ..config(false)
        })
        .unwrap();
        let task = state
            .spawn_upstream_ca_reread(Duration::from_millis(10))
            .expect("a pin starts a task");
        drop(state);
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("the task ended")
            .expect("the task did not panic");
    }

    fn config(cookie_secure: bool) -> ResolvedConfig {
        ResolvedConfig {
            bind_addr: "127.0.0.1:8090".into(),
            upstream_url: "https://127.0.0.1:5514".into(),
            session_ttl_secs: 3_600,
            allow_insecure_cookies: !cookie_secure,
            upstream_tls: UpstreamTls::System,
            upstream_connect: None,
            cookie_key: SessionKey::from_bytes([0x42; fleet_auth::KEY_LEN]),
            shared_domain: None,
            public_origins: PublicOrigins::parse(["http://127.0.0.1:8090"])
                .expect("fixture origin parses"),
            limits: ProxyLimits::from_server(None),
        }
    }

    fn state(cookie_secure: bool) -> AppState {
        AppState::from_config(config(cookie_secure)).unwrap()
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
