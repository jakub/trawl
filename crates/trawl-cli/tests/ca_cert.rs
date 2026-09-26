// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `ca_cert` pinning against a real rustls listener.
//!
//! Each test starts a TLS server on `127.0.0.1` that answers
//! `GET /api/v1/health`, then connects through
//! [`ConnectionParams::client`], the one constructor every command and the
//! TUI use. A refusal is checked from both ends: the client call fails, and
//! the server sees its handshake fail. A plain connection error would pass
//! the first check but not the second.

use std::sync::Arc;
use std::time::Duration;

use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;
use trawl_cli::cli::ConnectionParams;
use trawl_client::{ClientError, HealthStatus, TlsTrust};

const HEALTH_BODY: &str = r#"{"status":"ok"}"#;

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

/// A TLS listener on `127.0.0.1` that reports every handshake outcome.
struct Server {
    port: u16,
    handshakes: mpsc::UnboundedReceiver<Result<(), String>>,
}

impl Server {
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
                            answer_health(&mut tls).await;
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

    /// The next handshake outcome the server saw.
    async fn handshake(&mut self) -> Result<(), String> {
        tokio::time::timeout(Duration::from_secs(10), self.handshakes.recv())
            .await
            .expect("the server saw no handshake")
            .expect("listener task ended")
    }
}

/// Read one request head and answer it with a healthy status.
async fn answer_health<S: AsyncReadExt + AsyncWriteExt + Unpin>(stream: &mut S) {
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => head.extend_from_slice(&buf[..n]),
        }
    }
    assert!(
        head.starts_with(b"GET /api/v1/health "),
        "unexpected request: {}",
        String::from_utf8_lossy(&head)
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{HEALTH_BODY}",
        HEALTH_BODY.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn pinned(url: String, pem: &str) -> ConnectionParams {
    ConnectionParams {
        url,
        token: "flt_test_not_real".into(),
        trust: TlsTrust::PinnedCa(pem.as_bytes().to_vec()),
    }
}

async fn expect_accepted(server: &mut Server, conn: &ConnectionParams) {
    let health = conn
        .client()
        .expect("build client")
        .health()
        .await
        .expect("pinned CA must be accepted");
    assert_eq!(health.status, HealthStatus::Ok);
    server
        .handshake()
        .await
        .expect("server saw a good handshake");
}

async fn expect_refused(server: &mut Server, conn: &ConnectionParams) {
    let err = conn
        .client()
        .expect("build client")
        .health()
        .await
        .expect_err("the certificate must be refused");
    assert!(matches!(err, ClientError::Network(_)), "got {err:?}");
    let seen = server.handshake().await;
    assert!(
        seen.is_err(),
        "the server completed a handshake the client should have refused"
    );
}

#[tokio::test]
async fn pinned_ca_accepts_its_own_server() {
    let ca_a = ca("trawl test CA A");
    let mut server = Server::start(leaf(&ca_a, &["localhost", "127.0.0.1"])).await;

    let by_ip = pinned(format!("https://127.0.0.1:{}", server.port), &ca_a.pem());
    expect_accepted(&mut server, &by_ip).await;

    let by_name = pinned(format!("https://localhost:{}", server.port), &ca_a.pem());
    expect_accepted(&mut server, &by_name).await;
}

#[tokio::test]
async fn pinned_ca_refuses_a_server_from_another_ca() {
    let ca_a = ca("trawl test CA A");
    let ca_b = ca("trawl test CA B");
    let mut server = Server::start(leaf(&ca_a, &["localhost", "127.0.0.1"])).await;

    let conn = pinned(format!("https://127.0.0.1:{}", server.port), &ca_b.pem());
    expect_refused(&mut server, &conn).await;
}

/// The pin replaces the chain check only: the hostname is still verified.
#[tokio::test]
async fn pinned_ca_still_verifies_the_hostname() {
    let ca_a = ca("trawl test CA A");
    let mut server = Server::start(leaf(&ca_a, &["other.example"])).await;
    let conn = pinned(format!("https://127.0.0.1:{}", server.port), &ca_a.pem());
    expect_refused(&mut server, &conn).await;

    // A name SAN does not cover the IP literal, and the reverse.
    let mut server = Server::start(leaf(&ca_a, &["localhost"])).await;
    let conn = pinned(format!("https://127.0.0.1:{}", server.port), &ca_a.pem());
    expect_refused(&mut server, &conn).await;
}

/// A bundle with several roots trusts each of them.
#[tokio::test]
async fn pinned_bundle_trusts_every_root_in_it() {
    let ca_a = ca("trawl test CA A");
    let ca_b = ca("trawl test CA B");
    let bundle = format!("{}{}", ca_b.pem(), ca_a.pem());
    let mut server = Server::start(leaf(&ca_a, &["127.0.0.1"])).await;
    let conn = pinned(format!("https://127.0.0.1:{}", server.port), &bundle);
    expect_accepted(&mut server, &conn).await;
}

/// The shape `trawld` generates for itself: a self-signed end-entity
/// certificate, pinned as its own root.
#[tokio::test]
async fn pinned_self_signed_server_certificate() {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])
            .expect("self-signed pair");
    let identity = Identity {
        chain: vec![cert.der().clone()],
        key: PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into(),
    };
    let mut server = Server::start(identity).await;
    let conn = pinned(format!("https://127.0.0.1:{}", server.port), &cert.pem());
    expect_accepted(&mut server, &conn).await;

    let other = ca("trawl test CA B");
    let conn = pinned(format!("https://127.0.0.1:{}", server.port), &other.pem());
    expect_refused(&mut server, &conn).await;
}

/// A plain `http://` URL would skip the pin, so a pinned client refuses it
/// before connecting.
#[tokio::test]
async fn pinned_ca_refuses_plain_http() {
    let ca_a = ca("trawl test CA A");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    let conn = pinned(format!("http://127.0.0.1:{port}"), &ca_a.pem());
    let client = conn.client().expect("build client");
    tokio::select! {
        result = client.health() => {
            let err = result.expect_err("plain HTTP must be refused under a pin");
            assert!(matches!(err, ClientError::Network(_)), "got {err:?}");
        }
        _ = listener.accept() => panic!("a pinned client opened a plain connection"),
    }
}
