// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared request-scoped state.
//!
//! `AppState` is cloned into every handler via axum's `State` extractor.
//! Heavy values (reqwest client, session key) live behind `Arc` so clones
//! are cheap.

use std::sync::Arc;

use reqwest::Client;

use crate::config::ResolvedConfig;
use crate::session::SessionKey;

/// Handler-visible runtime state. `Arc`-internals ensure cloning is O(1).
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    cookie_key: SessionKey,
    http: Client,
    upstream_url: String,
    session_ttl_secs: u64,
    allow_insecure_cookies: bool,
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
                session_ttl_secs: cfg.session_ttl_secs,
                allow_insecure_cookies: cfg.allow_insecure_cookies,
            }),
        })
    }

    #[must_use]
    pub fn cookie_key(&self) -> &SessionKey {
        &self.inner.cookie_key
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
            .field("session_ttl_secs", &self.inner.session_ttl_secs)
            .field("allow_insecure_cookies", &self.inner.allow_insecure_cookies)
            .finish_non_exhaustive()
    }
}
