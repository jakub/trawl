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
//! headers: [`AppState::build_session_cookie`] (login) and
//! [`AppState::build_clear_cookie`] (logout, the session extractor's
//! expired-cookie path, `auth::me` on an upstream 401) emit one attribute
//! set, because browsers ignore a clear directive whose
//! `Domain`/`Path`/`SameSite` don't match issuance.

use std::sync::Arc;

use axum::http::HeaderValue;
use axum::http::header::InvalidHeaderValue;
use fleet_auth::{
    DEFAULT_COOKIE_NAME, PublicOrigins, SameSite, SessionKey, build_clear_cookie_header,
    build_session_cookie_header,
};
use reqwest::{Client, ClientBuilder};

use crate::config::{ResolvedConfig, UpstreamConnect, UpstreamTls};

/// Handler-visible runtime state.
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
    shared_domain: Option<String>,
    public_origins: PublicOrigins,
}

impl AppState {
    /// Build state from resolved config. Constructs the reqwest client
    /// once (connection pooling + keep-alive are implicit). Installs the
    /// rustls `ring` crypto provider on first call (idempotent — ignored
    /// if another provider is already installed).
    ///
    /// The client's certificate trust comes from
    /// [`ResolvedConfig::upstream_tls`], already validated at resolution;
    /// see `upstream_client` for what each mode sets.
    ///
    /// # Errors
    /// Propagates `reqwest::Error` if the client can't be built.
    pub fn from_config(cfg: ResolvedConfig) -> Result<Self, reqwest::Error> {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let http = upstream_client(
            Client::builder(),
            cfg.upstream_tls,
            cfg.upstream_connect.as_ref(),
        )
        .build()?;

        Ok(Self {
            inner: Arc::new(Inner {
                cookie_key: cfg.cookie_key,
                http,
                upstream_url: cfg.upstream_url,
                session_ttl_secs: cfg.session_ttl_secs,
                allow_insecure_cookies: cfg.allow_insecure_cookies,
                shared_domain: cfg.shared_domain,
                public_origins: cfg.public_origins,
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

    /// The deployment's browser-visible origins, the CSRF allowlist a
    /// present `Origin` header is compared against (ADR-0016).
    ///
    /// State owns it because the guard runs per request and the list is
    /// resolved once at startup; it is non-empty by construction, so there
    /// is no "not configured" case for a handler to interpret.
    #[must_use]
    pub fn public_origins(&self) -> &PublicOrigins {
        &self.inner.public_origins
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
    pub fn session_ttl_secs(&self) -> u64 {
        self.inner.session_ttl_secs
    }

    #[must_use]
    pub fn allow_insecure_cookies(&self) -> bool {
        self.inner.allow_insecure_cookies
    }
}

/// Set the upstream trust mode and dialing rules on `builder`.
///
/// In every mode the client:
///
/// - Speaks https only. Resolution already refuses a plain-http
///   `upstream_url`; this also refuses any other cleartext hop.
/// - Follows no redirect. trawld never sends one, and a followed 3xx would
///   carry the signed-in user's key to whatever host the `Location` names
///   and the trust mode accepts: under the platform roots, any host with a
///   public certificate.
/// - Ignores proxy settings, such as `HTTPS_PROXY`. trawl-web always dials
///   trawld directly. A proxy would also resolve the host name on its own
///   side, past any connect address.
///
/// With a connect address (`[web] upstream_connect_addr`), the client dials
/// that address for the URL's host and never asks the resolver. TLS still
/// verifies the host name from the URL.
///
/// A pinned CA replaces the platform roots, with host-name verification
/// left on. A pin whose file did not exist at startup trusts no root at
/// all, so every handshake fails.
///
/// Split from [`AppState::from_config`] so a test can pass a builder with
/// its own resolver or proxy and see what the modes override.
fn upstream_client(
    builder: ClientBuilder,
    tls: UpstreamTls,
    connect: Option<&UpstreamConnect>,
) -> ClientBuilder {
    let builder = builder
        .redirect(reqwest::redirect::Policy::none())
        .https_only(true)
        .no_proxy();
    let builder = match connect {
        Some(connect) => builder.resolve(&connect.host, connect.addr),
        None => builder,
    };
    match tls {
        UpstreamTls::System => builder,
        UpstreamTls::PinnedCa { roots, .. } => builder.tls_certs_only(roots.unwrap_or_default()),
    }
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
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
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use reqwest::dns::{Name, Resolve, Resolving};
    use tokio::net::TcpListener;

    use super::*;
    use crate::test_support::TlsUpstream;

    /// Both trust modes, the pin pending so no file is needed: the rules
    /// under test do not depend on which roots are trusted.
    fn every_mode() -> [UpstreamTls; 2] {
        [
            UpstreamTls::System,
            UpstreamTls::PinnedCa {
                path: PathBuf::from("/nonexistent/ca.pem"),
                roots: None,
            },
        ]
    }

    /// Send one request to `url` with `client` and report whether
    /// `listener` saw a connection. The TLS handshake then fails, since
    /// the listener speaks no TLS; only the dial matters here.
    async fn dials(client: &Client, url: String, listener: &TcpListener) -> bool {
        let request = tokio::spawn(client.get(url).send());
        let accepted = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .is_ok_and(|accept| accept.is_ok());
        request.abort();
        accepted
    }

    /// A host resolver that answers nothing and counts how often it is
    /// asked, standing in for one that would name another machine.
    struct RefusingResolver(Arc<AtomicUsize>);

    impl Resolve for RefusingResolver {
        fn resolve(&self, _name: Name) -> Resolving {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(std::future::ready(
                Err("the host resolver was asked".into()),
            ))
        }
    }

    #[tokio::test]
    async fn a_connect_addr_is_dialed_without_asking_the_resolver() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        // Two copies of each mode: one for the control, one under test.
        for (control, tls) in every_mode().into_iter().zip(every_mode()) {
            let mode = format!("{tls:?}");
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let url = format!("https://trawl.test:{}/api/v1/whoami", addr.port());

            // Control: without a connect address the name goes to the
            // resolver, which refuses, so the injected resolver is in use.
            let asked = Arc::new(AtomicUsize::new(0));
            let base = Client::builder().dns_resolver(Arc::new(RefusingResolver(asked.clone())));
            let client = upstream_client(base, control, None).build().unwrap();
            let err = client.get(&url).send().await.expect_err("nothing resolves");
            assert!(err.is_connect(), "{mode}: {err:?}");
            assert_eq!(asked.load(Ordering::SeqCst), 1, "{mode}");

            let asked = Arc::new(AtomicUsize::new(0));
            let base = Client::builder().dns_resolver(Arc::new(RefusingResolver(asked.clone())));
            let connect = UpstreamConnect {
                host: "trawl.test".to_owned(),
                addr,
            };
            let client = upstream_client(base, tls, Some(&connect)).build().unwrap();
            assert!(
                dials(&client, url, &listener).await,
                "{mode}: the client did not dial the connect address"
            );
            assert_eq!(
                asked.load(Ordering::SeqCst),
                0,
                "{mode}: the client asked the resolver for a name it has an address for"
            );
        }
    }

    #[tokio::test]
    async fn every_mode_ignores_a_configured_proxy() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        for tls in every_mode() {
            let mode = format!("{tls:?}");
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = Client::builder().proxy(
                reqwest::Proxy::all(format!("http://{}", proxy.local_addr().unwrap())).unwrap(),
            );
            let client = upstream_client(base, tls, None).build().unwrap();

            let url = format!(
                "https://127.0.0.1:{}/api/v1/whoami",
                upstream.local_addr().unwrap().port()
            );
            assert!(
                dials(&client, url, &upstream).await,
                "{mode}: the client did not dial the upstream directly"
            );
            assert!(
                tokio::time::timeout(Duration::from_millis(200), proxy.accept())
                    .await
                    .is_err(),
                "{mode}: the client went through the proxy"
            );
        }
    }

    #[tokio::test]
    async fn every_mode_refuses_a_plain_http_request() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        for tls in every_mode() {
            let mode = format!("{tls:?}");
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let client = upstream_client(Client::builder(), tls, None)
                .build()
                .unwrap();
            let url = format!(
                "http://127.0.0.1:{}/api/v1/whoami",
                listener.local_addr().unwrap().port()
            );
            // Bounded: a client that did dial would wait forever on the
            // silent listener, and the test must fail rather than hang.
            let err = tokio::time::timeout(Duration::from_secs(5), client.get(&url).send())
                .await
                .unwrap_or_else(|_| panic!("{mode}: the client dialed a plain-http upstream"))
                .expect_err("a cleartext request must refuse");
            assert!(err.is_builder(), "{mode}: {err:?}");
            assert!(
                tokio::time::timeout(Duration::from_millis(200), listener.accept())
                    .await
                    .is_err(),
                "{mode}: the client dialed a plain-http upstream"
            );
        }
    }

    /// A pin whose file was absent at startup trusts nothing, not the
    /// platform roots and not the upstream's own CA.
    #[tokio::test]
    async fn a_pending_pin_trusts_no_upstream() {
        let upstream = TlsUpstream::start().await;
        let state = AppState::from_config(ResolvedConfig {
            upstream_url: upstream.url(),
            upstream_tls: UpstreamTls::PinnedCa {
                path: upstream.ca_path().to_owned(),
                roots: None,
            },
            ..config(false)
        })
        .expect("a pending pin still builds a client");
        let err = state
            .http()
            .get(format!("{}/api/v1/whoami", upstream.url()))
            .send()
            .await
            .expect_err("no root is trusted");
        assert!(err.is_connect(), "{err:?}");
        assert!(
            upstream
                .mock()
                .received_requests()
                .await
                .expect("recording on")
                .is_empty(),
            "a request crossed a handshake that should have failed"
        );
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
