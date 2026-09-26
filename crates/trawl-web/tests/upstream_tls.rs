// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `[web] upstream_ca_path` against a real rustls upstream.
//!
//! Each test starts a TLS server on `127.0.0.1` that answers
//! `GET /api/v1/whoami`, writes a CA to a file, and resolves a `[web]`
//! section naming it through `ResolvedConfig::from_parsed` and
//! `AppState::from_config`, the path `trawl-web` starts through. The
//! request is a browser login through the router, which asks the upstream
//! `/whoami` before it issues a cookie. A refusal is checked from both
//! ends: the login fails, and the server sees its handshake fail. A plain
//! connection error would pass the first check but not the second.

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
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;
use tower::ServiceExt;
use trawl_config::WebConfig;
use trawl_web::config::{ResolvedConfig, UpstreamTls};
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
}

impl Upstream {
    async fn start(identity: Identity) -> Self {
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_no_client_auth()
        .with_single_cert(identity.chain, identity.key)
        .expect("server certificate");
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("local addr").port();
        let (tx, handshakes) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
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
        Self { port, handshakes }
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

/// The proxy's state for an upstream at `upstream_url` pinned to `ca_pem`,
/// resolved the way `trawl-web` resolves it at startup.
fn pinned_state(dir: &tempfile::TempDir, upstream_url: String, ca_pem: &str) -> AppState {
    let ca_path = dir.path().join("upstream-ca.pem");
    std::fs::write(&ca_path, ca_pem).expect("write CA");
    let web = WebConfig {
        upstream_url: Some(upstream_url),
        upstream_ca_path: Some(ca_path),
        public_origins: vec!["https://trawl.example.com".to_owned()],
        ..WebConfig::default()
    };
    let resolved = ResolvedConfig::from_parsed(&web, None).expect("resolve config");
    assert!(
        matches!(resolved.upstream_tls, UpstreamTls::PinnedCa(_)),
        "{:?}",
        resolved.upstream_tls
    );
    AppState::from_config(resolved).expect("build state")
}

/// A browser login through the proxy's router.
async fn login(state: AppState) -> StatusCode {
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
        .status()
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
