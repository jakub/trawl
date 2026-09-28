// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `[web] upstream_ca_path` and `upstream_connect_addr` against a real
//! rustls upstream.
//!
//! Each connection test starts a TLS server on `127.0.0.1` that answers
//! `GET /api/v1/whoami`, writes a CA to a file, and resolves a `[web]`
//! section naming it through `ResolvedConfig::from_parsed` and
//! `AppState::from_config`, the path `trawl-web` starts through. The
//! request is a browser login through the router, which asks the upstream
//! `/whoami` before it issues a cookie. A refusal is checked from both
//! ends: the login fails, and the server sees its handshake fail. A plain
//! connection error would pass the first check but not the second.
//!
//! The configuration rules for `upstream_connect_addr` are checked at
//! resolution, with no upstream: a refused pair never builds a client.
//!
//! The reload tests change the pin file under a running proxy, and the
//! upstream can change the certificate it serves. Every response closes
//! its connection, so each request makes a fresh handshake and a pooled
//! connection cannot hide a change of trust.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};
use tokio_rustls::TlsAcceptor;
use tower::ServiceExt;
use trawl_config::WebConfig;
use trawl_web::config::{
    ConfigError, ConnectAddrError, ResolvedConfig, UpstreamTls, UpstreamUrlError,
};
use trawl_web::routes;
use trawl_web::state::AppState;

const WHOAMI_BODY: &str = r#"{"name":"alice","roles":["operator"],"permissions":["trawl:query"]}"#;

/// A self-signed CA that can issue server certificates.
fn ca(name: &str) -> CertifiedIssuer<'static, KeyPair> {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    params.distinguished_name.push(DnType::CommonName, name);
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    CertifiedIssuer::self_signed(params, KeyPair::generate().expect("CA key")).expect("CA cert")
}

/// A server certificate for `sans`, issued by `issuer`.
fn leaf(issuer: &CertifiedIssuer<'static, KeyPair>, sans: &[&str]) -> Identity {
    let key = KeyPair::generate().expect("leaf key");
    let mut params =
        CertificateParams::new(sans.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
            .expect("leaf params");
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let cert = params.signed_by(&key, issuer).expect("sign leaf");
    Identity {
        chain: vec![cert.der().clone()],
        key: PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
    }
}

struct Identity {
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
}

/// A TLS upstream on `127.0.0.1` that reports every handshake outcome.
struct Upstream {
    port: u16,
    handshakes: mpsc::UnboundedReceiver<Result<(), String>>,
    /// The certificate the next handshake presents.
    identity: watch::Sender<TlsAcceptor>,
}

fn acceptor(identity: Identity) -> TlsAcceptor {
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("protocol versions")
    .with_no_client_auth()
    .with_single_cert(identity.chain, identity.key)
    .expect("server certificate");
    TlsAcceptor::from(Arc::new(config))
}

impl Upstream {
    async fn start(identity: Identity) -> Self {
        let (identity, current) = watch::channel(acceptor(identity));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("local addr").port();
        let (tx, handshakes) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let acceptor = current.borrow().clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    match acceptor.accept(tcp).await {
                        Ok(mut tls) => {
                            let _ = tx.send(Ok(()));
                            answer_whoami(&mut tls).await;
                        }
                        Err(e) => {
                            let _ = tx.send(Err(e.to_string()));
                        }
                    }
                });
            }
        });
        Self {
            port,
            handshakes,
            identity,
        }
    }

    /// Serve `identity` from the next handshake on, as trawld does after
    /// its certificate is re-issued.
    fn rotate(&self, identity: Identity) {
        self.identity.send_replace(acceptor(identity));
    }

    /// Whether the upstream has seen no connection since the last outcome
    /// a test read.
    fn saw_nothing(&mut self) -> bool {
        matches!(
            self.handshakes.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        )
    }

    /// The next handshake outcome the upstream saw.
    async fn handshake(&mut self) -> Result<(), String> {
        tokio::time::timeout(Duration::from_secs(10), self.handshakes.recv())
            .await
            .expect("the upstream saw no handshake")
            .expect("listener task ended")
    }
}

/// Read one request head and answer it as trawld's `/whoami` would.
async fn answer_whoami<S: AsyncReadExt + AsyncWriteExt + Unpin>(stream: &mut S) {
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => head.extend_from_slice(&buf[..n]),
        }
    }
    assert!(
        head.starts_with(b"GET /api/v1/whoami "),
        "unexpected request: {}",
        String::from_utf8_lossy(&head)
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{WHOAMI_BODY}",
        WHOAMI_BODY.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// The pin file [`pinned_state`] writes, in the test's directory.
const CA_FILE: &str = "upstream-ca.pem";

/// The proxy's state for an upstream at `upstream_url` pinned to `ca_pem`,
/// resolved the way `trawl-web` resolves it at startup.
fn pinned_state(dir: &tempfile::TempDir, upstream_url: String, ca_pem: &str) -> AppState {
    pinned_state_via(dir, upstream_url, None, ca_pem)
}

/// [`pinned_state`], dialing `connect_addr` when set.
fn pinned_state_via(
    dir: &tempfile::TempDir,
    upstream_url: String,
    connect_addr: Option<String>,
    ca_pem: &str,
) -> AppState {
    let ca_path = dir.path().join(CA_FILE);
    std::fs::write(&ca_path, ca_pem).expect("write CA");
    let web = WebConfig {
        upstream_url: Some(upstream_url),
        upstream_connect_addr: connect_addr,
        upstream_ca_path: Some(ca_path),
        public_origins: vec!["https://trawl.example.com".to_owned()],
        ..WebConfig::default()
    };
    let resolved = ResolvedConfig::from_parsed(&web, None).expect("resolve config");
    assert!(
        matches!(
            resolved.upstream_tls,
            UpstreamTls::PinnedCa { roots: Some(_), .. }
        ),
        "{:?}",
        resolved.upstream_tls
    );
    AppState::from_config(resolved).expect("build state")
}

/// A browser login through the proxy's router.
async fn login(state: AppState) -> StatusCode {
    login_response(state).await.status()
}

async fn login_response(state: AppState) -> axum::response::Response {
    let request = Request::builder()
        .method("POST")
        .uri("/api/auth/login")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"api_key":"flt_test_not_real"}"#))
        .expect("request");
    routes::build(state)
        .oneshot(request)
        .await
        .expect("router answers")
}

/// A bearer client's `GET` through the generic `/api/v1/*` forwarder.
async fn forwarded(state: AppState, path: &str) -> axum::response::Response {
    let request = Request::builder()
        .uri(path)
        .header(header::AUTHORIZATION, "Bearer flt_test_not_real")
        .body(Body::empty())
        .expect("request");
    routes::build(state)
        .oneshot(request)
        .await
        .expect("router answers")
}

async fn body_text(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("read body");
    String::from_utf8(bytes.to_vec()).expect("UTF-8 body")
}

async fn expect_accepted(upstream: &mut Upstream, state: AppState) {
    assert_eq!(
        login(state).await,
        StatusCode::OK,
        "pinned CA must be accepted"
    );
    upstream
        .handshake()
        .await
        .expect("upstream saw a good handshake");
}

async fn expect_refused(upstream: &mut Upstream, state: AppState) {
    let status = login(state).await;
    assert!(
        status.is_server_error(),
        "the certificate must be refused, got {status}"
    );
    let seen = upstream.handshake().await;
    assert!(
        seen.is_err(),
        "the upstream completed a handshake the proxy should have refused"
    );
}

#[tokio::test]
async fn pinned_ca_reaches_its_own_upstream() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ca_a = ca("trawl test CA A");
    let mut upstream = Upstream::start(leaf(&ca_a, &["localhost", "127.0.0.1"])).await;

    let by_ip = pinned_state(
        &dir,
        format!("https://127.0.0.1:{}", upstream.port),
        &ca_a.pem(),
    );
    expect_accepted(&mut upstream, by_ip).await;

    let by_name = pinned_state(
        &dir,
        format!("https://localhost:{}", upstream.port),
        &ca_a.pem(),
    );
    expect_accepted(&mut upstream, by_name).await;
}

#[tokio::test]
async fn pinned_ca_refuses_an_upstream_from_another_ca() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ca_a = ca("trawl test CA A");
    let ca_b = ca("trawl test CA B");
    let mut upstream = Upstream::start(leaf(&ca_b, &["localhost", "127.0.0.1"])).await;

    let state = pinned_state(
        &dir,
        format!("https://127.0.0.1:{}", upstream.port),
        &ca_a.pem(),
    );
    expect_refused(&mut upstream, state).await;
}

/// The pin replaces the chain check only: the hostname is still verified.
#[tokio::test]
async fn pinned_ca_still_verifies_the_upstream_hostname() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ca_a = ca("trawl test CA A");
    let mut upstream = Upstream::start(leaf(&ca_a, &["other.example"])).await;
    let state = pinned_state(
        &dir,
        format!("https://127.0.0.1:{}", upstream.port),
        &ca_a.pem(),
    );
    expect_refused(&mut upstream, state).await;
}

/// The shape `trawld` generates for itself: a self-signed end-entity
/// certificate, pinned as its own root.
#[tokio::test]
async fn pinned_self_signed_upstream_certificate() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])
            .expect("self-signed pair");
    let identity = Identity {
        chain: vec![cert.der().clone()],
        key: PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into(),
    };
    let mut upstream = Upstream::start(identity).await;
    let state = pinned_state(
        &dir,
        format!("https://127.0.0.1:{}", upstream.port),
        &cert.pem(),
    );
    expect_accepted(&mut upstream, state).await;

    let other = ca("trawl test CA B");
    let state = pinned_state(
        &dir,
        format!("https://127.0.0.1:{}", upstream.port),
        &other.pem(),
    );
    expect_refused(&mut upstream, state).await;
}

/// A pin replaces the platform roots; it does not add to them.
///
/// Every other test here uses a CA no platform store trusts, so a pin that
/// merged its roots into the platform store would pass them all. This one
/// makes the upstream's issuer a platform root. Under the workspace's
/// reqwest features the platform store is `rustls-platform-verifier`, which
/// on Linux loads `rustls-native-certs`, and that reads only `SSL_CERT_FILE`
/// when the variable is set. The process environment must not be mutated,
/// so the proxy runs in a re-executed copy of this test with the variable
/// set on its `Command`, and the upstream stays here to see both handshakes.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn pinned_ca_excludes_the_platform_roots() {
    const CHILD_URL: &str = "TRAWL_TEST_PLATFORM_ROOTS_URL";
    const CHILD_PIN: &str = "TRAWL_TEST_PLATFORM_ROOTS_PIN";
    if let Some(url) = std::env::var_os(CHILD_URL) {
        let url = url.into_string().expect("UTF-8 URL");
        let pin = std::fs::read_to_string(std::env::var_os(CHILD_PIN).expect("pin path"))
            .expect("read the pin");
        // The platform path trusts the upstream: the variable took effect.
        let web = WebConfig {
            upstream_url: Some(url.clone()),
            public_origins: vec!["https://trawl.example.com".to_owned()],
            ..WebConfig::default()
        };
        let resolved = ResolvedConfig::from_parsed(&web, None).expect("resolve config");
        assert!(
            matches!(resolved.upstream_tls, UpstreamTls::System),
            "{:?}",
            resolved.upstream_tls
        );
        let system = AppState::from_config(resolved).expect("build state");
        assert_eq!(
            login(system).await,
            StatusCode::OK,
            "the platform roots must trust the upstream"
        );
        // A pin to another CA refuses the same upstream.
        let dir = tempfile::tempdir().expect("tempdir");
        let status = login(pinned_state(&dir, url, &pin)).await;
        assert!(
            status.is_server_error(),
            "a pin must not fall back to the platform roots, got {status}"
        );
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let platform = ca("trawl test platform CA");
    let other = ca("trawl test CA B");
    let platform_file = dir.path().join("platform-roots.pem");
    let pin_file = dir.path().join("pin.pem");
    std::fs::write(&platform_file, platform.pem()).expect("write platform roots");
    std::fs::write(&pin_file, other.pem()).expect("write pin");
    let mut upstream = Upstream::start(leaf(&platform, &["127.0.0.1"])).await;

    let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"));
    child
        .args([
            "--exact",
            "pinned_ca_excludes_the_platform_roots",
            "--nocapture",
        ])
        .env(CHILD_URL, format!("https://127.0.0.1:{}", upstream.port))
        .env(CHILD_PIN, &pin_file)
        .env("SSL_CERT_FILE", &platform_file)
        .env_remove("SSL_CERT_DIR");
    let output = tokio::task::spawn_blocking(move || child.output())
        .await
        .expect("join the child")
        .expect("re-execute the test binary");
    assert!(
        output.status.success(),
        "child failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    upstream
        .handshake()
        .await
        .expect("the upstream saw the platform client's handshake");
    assert!(
        upstream.handshake().await.is_err(),
        "the upstream completed a handshake the pinned proxy should have refused"
    );
}

/// `upstream_connect_addr` needs a name for TLS to verify: an upstream URL
/// whose host is an IP literal refuses at startup, as do the other shapes
/// under which the address could not mean what it says.
#[test]
fn connect_addr_requires_dns_host() {
    let resolve = |upstream_url: &str, connect_addr: &str| {
        let web = WebConfig {
            upstream_url: Some(upstream_url.to_owned()),
            upstream_connect_addr: Some(connect_addr.to_owned()),
            public_origins: vec!["https://trawl.example.com".to_owned()],
            ..WebConfig::default()
        };
        ResolvedConfig::from_parsed(&web, None).expect_err("the pair must refuse")
    };

    for (upstream_url, connect_addr) in [
        ("https://127.0.0.1:5514", "127.0.0.1:5514"),
        ("https://10.0.0.7:5514", "127.0.0.1:5514"),
        ("https://[::1]:5514", "[::1]:5514"),
    ] {
        let error = resolve(upstream_url, connect_addr);
        assert!(
            matches!(
                error,
                ConfigError::UpstreamConnectAddr(ConnectAddrError::IpHost)
            ),
            "{upstream_url} via {connect_addr} gave {error:?}"
        );
        let message = error.to_string();
        assert!(message.contains("upstream_connect_addr"), "got: {message}");
        assert!(message.contains("DNS name"), "got: {message}");
    }

    // The URL's port and the address's port must agree.
    let error = resolve("https://trawl.test:5514", "127.0.0.1:5515");
    assert!(
        matches!(
            error,
            ConfigError::UpstreamConnectAddr(ConnectAddrError::PortMismatch {
                url_port: 5514,
                addr_port: 5515
            })
        ),
        "{error:?}"
    );

    // A plain-http upstream refuses whatever the connect address says.
    let error = resolve("http://trawl.test:5514", "127.0.0.1:5514");
    assert!(
        matches!(
            &error,
            ConfigError::UpstreamUrl(UpstreamUrlError::NotHttps { scheme }) if scheme == "http"
        ),
        "{error:?}"
    );

    // Only an IP address and port: never a name, never a bare address.
    for connect_addr in [
        "trawl.test:5514",
        "localhost:5514",
        "127.0.0.1",
        "not an address",
    ] {
        let error = resolve("https://trawl.test:5514", connect_addr);
        assert!(
            matches!(
                error,
                ConfigError::UpstreamConnectAddr(ConnectAddrError::Malformed)
            ),
            "{connect_addr:?} gave {error:?}"
        );
    }
}

/// The upstream the proxy dials for `trawl.test` over loopback, with the
/// certificate `identity`: `upstream_url` names the host TLS verifies, and
/// `upstream_connect_addr` says where the connection goes. `trawl.test`
/// is a reserved name no resolver answers, so reaching it at all proves
/// the connect address was used.
async fn via_connect_addr(
    dir: &tempfile::TempDir,
    identity: Identity,
    ca_pem: &str,
) -> (Upstream, AppState) {
    let upstream = Upstream::start(identity).await;
    let state = pinned_state_via(
        dir,
        format!("https://trawl.test:{}", upstream.port),
        Some(format!("127.0.0.1:{}", upstream.port)),
        ca_pem,
    );
    (upstream, state)
}

#[tokio::test]
async fn connect_addr_verifies_dns_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ca_a = ca("trawl test CA A");
    let (mut upstream, state) =
        via_connect_addr(&dir, leaf(&ca_a, &["trawl.test"]), &ca_a.pem()).await;
    expect_accepted(&mut upstream, state).await;
}

/// The leaf covers the address the proxy dials, but not the name in
/// `upstream_url`: the name is what TLS verifies.
#[tokio::test]
async fn connect_addr_wrong_name_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ca_a = ca("trawl test CA A");
    let (mut upstream, state) = via_connect_addr(
        &dir,
        leaf(&ca_a, &["other.test", "localhost", "127.0.0.1"]),
        &ca_a.pem(),
    )
    .await;
    expect_refused(&mut upstream, state).await;
}

/// The 503 a request gets while the pin file has never held a usable
/// certificate.
const CA_UNAVAILABLE_BODY: &str = r#"{"error":"upstream certificate not available"}"#;

async fn expect_unavailable(response: axum::response::Response) {
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        response.headers().get(header::SET_COOKIE).is_none(),
        "a missing pin must not touch the session cookie"
    );
    assert_eq!(body_text(response).await, CA_UNAVAILABLE_BODY);
}

/// trawld writes its generated certificate on its first start, which may
/// come after trawl-web's. The interval is an hour, so the load below is
/// the one a request makes.
#[tokio::test]
async fn late_ca_is_loaded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ca_path = dir.path().join("tls").join("cert.pem");
    let ca_a = ca("trawl test CA A");
    let mut upstream = Upstream::start(leaf(&ca_a, &["localhost", "127.0.0.1"])).await;
    let web = WebConfig {
        upstream_url: Some(format!("https://127.0.0.1:{}", upstream.port)),
        upstream_ca_path: Some(ca_path.clone()),
        public_origins: vec!["https://trawl.example.com".to_owned()],
        ..WebConfig::default()
    };
    let resolved = ResolvedConfig::from_parsed(&web, None).expect("an absent pin still resolves");
    assert!(
        matches!(
            resolved.upstream_tls,
            UpstreamTls::PinnedCa { roots: None, .. }
        ),
        "{:?}",
        resolved.upstream_tls
    );
    let state = AppState::from_config(resolved).expect("trawl-web starts without the file");
    let _reread = state.spawn_upstream_ca_reread(Duration::from_secs(3600));

    let health = routes::build(state.clone())
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("router answers");
    assert_eq!(health.status(), StatusCode::OK);
    assert_eq!(body_text(health).await, "ok");

    expect_unavailable(login_response(state.clone()).await).await;
    expect_unavailable(forwarded(state.clone(), "/api/v1/whoami").await).await;
    assert!(
        upstream.saw_nothing(),
        "trawl-web dialed trawld with no certificate to verify it"
    );

    std::fs::create_dir_all(ca_path.parent().expect("parent")).expect("create tls dir");
    std::fs::write(&ca_path, ca_a.pem()).expect("write CA");
    expect_accepted(&mut upstream, state.clone()).await;

    let response = forwarded(state, "/api/v1/whoami").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_text(response).await, WHOAMI_BODY);
}

/// The next handshake outcome that is a success, skipping the refusals of
/// earlier attempts.
async fn next_good_handshake(upstream: &mut Upstream) {
    while upstream.handshake().await.is_err() {}
}

/// An operator re-issues trawld's certificate from a new CA and replaces
/// the pin file. Requests never trigger a read while a client exists, so
/// the reload below is the interval task's.
#[tokio::test]
async fn rotated_ca_is_reloaded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ca_a = ca("trawl test CA A");
    let ca_b = ca("trawl test CA B");
    let mut upstream = Upstream::start(leaf(&ca_a, &["localhost", "127.0.0.1"])).await;
    let state = pinned_state(
        &dir,
        format!("https://127.0.0.1:{}", upstream.port),
        &ca_a.pem(),
    );
    let interval = Duration::from_millis(100);
    let _reread = state
        .spawn_upstream_ca_reread(interval)
        .expect("a pin starts the re-read task");
    expect_accepted(&mut upstream, state.clone()).await;

    upstream.rotate(leaf(&ca_b, &["localhost", "127.0.0.1"]));
    expect_refused(&mut upstream, state.clone()).await;

    std::fs::write(dir.path().join(CA_FILE), ca_b.pem()).expect("write CA B");
    // One interval is the promise; the bound is generous so a loaded test
    // host cannot turn scheduling delay into a failure.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if login(state.clone()).await == StatusCode::OK {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the rotated CA was never loaded"
        );
        tokio::time::sleep(interval / 4).await;
    }
    next_good_handshake(&mut upstream).await;
}

/// A replacement that does not parse, and a file that disappears, keep
/// the last good roots rather than returning to 503.
#[tokio::test]
async fn unparseable_ca_change_keeps_last_good() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ca_a = ca("trawl test CA A");
    let mut upstream = Upstream::start(leaf(&ca_a, &["localhost", "127.0.0.1"])).await;
    let state = pinned_state(
        &dir,
        format!("https://127.0.0.1:{}", upstream.port),
        &ca_a.pem(),
    );
    expect_accepted(&mut upstream, state.clone()).await;

    let ca_path = dir.path().join(CA_FILE);
    std::fs::write(
        &ca_path,
        "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
    )
    .expect("write garbage");
    state.reread_upstream_ca().await;
    state.reread_upstream_ca().await;
    expect_accepted(&mut upstream, state.clone()).await;

    std::fs::remove_file(&ca_path).expect("remove the pin");
    state.reread_upstream_ca().await;
    expect_accepted(&mut upstream, state).await;
}
