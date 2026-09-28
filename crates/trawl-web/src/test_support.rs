// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Test upstreams that speak real TLS under a pinned CA.
//!
//! trawl-web verifies trawld's certificate (ADR-0048), so a test that
//! talks to an upstream does it over https, with `[web] upstream_ca_path`
//! naming the CA that issued the upstream's certificate. wiremock serves
//! plain HTTP only. A [`TlsFront`] closes that gap: it terminates TLS with
//! a leaf from a fresh [`TestCa`], and it relays each accepted connection
//! byte for byte to a plain listener, usually a wiremock [`MockServer`].
//! [`TlsUpstream`] pairs the two. Mocks mount on [`TlsUpstream::mock`],
//! and the proxy dials [`TlsUpstream::url`].
//!
//! Each accepted connection maps to one upstream connection, so wiremock
//! sees every request the proxy sends, and its request log and
//! expectation counts hold as they would without the relay.
//!
//! Keep the fixture alive for the whole test. Dropping it stops the relay
//! and deletes the CA file, and the proxy may read that file after
//! startup.
//!
//! Unit tests reach this module as `crate::test_support`. Integration
//! tests compile the same file with
//! `#[path = "../src/test_support.rs"] mod test_support;`. So the module
//! names only external crates, never `crate::`.

#![allow(dead_code)] // each test binary uses a subset of these items

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose,
};
use rustls::pki_types::PrivatePkcs8KeyDer;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinHandle, JoinSet};
use tokio_rustls::TlsAcceptor;
use trawl_config::WebConfig;
use wiremock::MockServer;

/// The names a loopback upstream answers to: its certificate covers both,
/// so a test may dial either spelling.
pub const LOOPBACK_SANS: [&str; 2] = ["localhost", "127.0.0.1"];

/// A self-signed CA, with its certificate written to `ca.pem` in a
/// directory it owns. The directory lives as long as the CA does.
pub struct TestCa {
    issuer: CertifiedIssuer<'static, KeyPair>,
    path: PathBuf,
    _dir: tempfile::TempDir,
}

impl TestCa {
    /// A fresh CA. Shared through `Arc`, so fronts it issues for keep its
    /// file alive.
    pub fn generate() -> Arc<Self> {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        params
            .distinguished_name
            .push(DnType::CommonName, "trawl-web test CA");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let issuer = CertifiedIssuer::self_signed(params, KeyPair::generate().expect("CA key"))
            .expect("CA certificate");
        let dir = tempfile::tempdir().expect("CA directory");
        let path = dir.path().join("ca.pem");
        std::fs::write(&path, issuer.pem()).expect("write ca.pem");
        Arc::new(Self {
            issuer,
            path,
            _dir: dir,
        })
    }

    /// The `ca.pem` a test pins with `[web] upstream_ca_path`.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The CA certificate as PEM.
    pub fn pem(&self) -> String {
        self.issuer.pem()
    }

    /// A rustls server config presenting a leaf for `sans`, issued by
    /// this CA.
    fn server_config(&self, sans: &[&str]) -> rustls::ServerConfig {
        let key = KeyPair::generate().expect("leaf key");
        let mut params =
            CertificateParams::new(sans.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
                .expect("leaf params");
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let leaf = params.signed_by(&key, &self.issuer).expect("sign leaf");
        rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_no_client_auth()
        .with_single_cert(
            vec![leaf.der().clone()],
            PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
        )
        .expect("server certificate")
    }
}

impl std::fmt::Debug for TestCa {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestCa")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// A TLS listener on `127.0.0.1` that relays every connection to `target`.
///
/// A connection whose handshake fails is dropped and never reaches the
/// target, so a test that expects a refused certificate sees no request
/// there.
pub struct TlsFront {
    addr: SocketAddr,
    ca: Arc<TestCa>,
    accept: JoinHandle<()>,
}

impl TlsFront {
    /// A front for `target` under a fresh CA, answering to
    /// [`LOOPBACK_SANS`].
    pub async fn start(target: SocketAddr) -> Self {
        Self::with_sans(target, &LOOPBACK_SANS).await
    }

    /// A front for `target` under a fresh CA, whose leaf names exactly
    /// `sans`: DNS names, or IP literals as IP SANs.
    pub async fn with_sans(target: SocketAddr, sans: &[&str]) -> Self {
        Self::issued_by(&TestCa::generate(), target, sans).await
    }

    /// A front for `target` whose leaf names `sans` and is issued by `ca`.
    /// Fronts that share a CA are all trusted by one pin.
    pub async fn issued_by(ca: &Arc<TestCa>, target: SocketAddr, sans: &[&str]) -> Self {
        let acceptor = TlsAcceptor::from(Arc::new(ca.server_config(sans)));
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the TLS front");
        let addr = listener.local_addr().expect("TLS front address");
        let accept = tokio::spawn(async move {
            // The relays belong to this task, so aborting it on drop ends
            // every open connection too.
            let mut relays = JoinSet::new();
            while let Ok((client, _)) = listener.accept().await {
                while relays.try_join_next().is_some() {}
                let acceptor = acceptor.clone();
                relays.spawn(async move {
                    let Ok(mut tls) = acceptor.accept(client).await else {
                        return;
                    };
                    let Ok(mut plain) = TcpStream::connect(target).await else {
                        return;
                    };
                    let _ = tokio::io::copy_bidirectional(&mut tls, &mut plain).await;
                });
            }
        });
        Self {
            addr,
            ca: Arc::clone(ca),
            accept,
        }
    }

    /// The front's port on `127.0.0.1`.
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// The upstream URL a proxy dials: `https://127.0.0.1:<port>`.
    pub fn url(&self) -> String {
        format!("https://{}", self.addr)
    }

    /// The CA that issued the front's certificate.
    pub fn ca(&self) -> &Arc<TestCa> {
        &self.ca
    }

    /// The `ca.pem` that trusts this front.
    pub fn ca_path(&self) -> &Path {
        self.ca.path()
    }

    /// The `[web]` upstream settings that reach this front: `upstream_url`
    /// and `upstream_ca_path`, everything else at its default. A test sets
    /// its own fields with `..front.web_config()`.
    pub fn web_config(&self) -> WebConfig {
        WebConfig {
            upstream_url: Some(self.url()),
            upstream_ca_path: Some(self.ca_path().to_owned()),
            ..WebConfig::default()
        }
    }
}

impl Drop for TlsFront {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

impl std::fmt::Debug for TlsFront {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsFront")
            .field("addr", &self.addr)
            .field("ca", &self.ca)
            .finish_non_exhaustive()
    }
}

/// A wiremock server reached through a [`TlsFront`].
#[derive(Debug)]
pub struct TlsUpstream {
    front: TlsFront,
    mock: MockServer,
}

impl TlsUpstream {
    /// A mock upstream under a fresh CA.
    pub async fn start() -> Self {
        Self::issued_by(&TestCa::generate()).await
    }

    /// A mock upstream whose certificate `ca` issues, so one pin trusts
    /// every upstream sharing the CA.
    pub async fn issued_by(ca: &Arc<TestCa>) -> Self {
        let mock = MockServer::start().await;
        let front = TlsFront::issued_by(ca, *mock.address(), &LOOPBACK_SANS).await;
        Self { front, mock }
    }

    /// The wiremock server: mount mocks and read its request log here.
    /// Its own `uri()` is the plain listener behind the front, which no
    /// proxy under test should dial.
    pub fn mock(&self) -> &MockServer {
        &self.mock
    }

    pub fn front(&self) -> &TlsFront {
        &self.front
    }

    /// See [`TlsFront::url`].
    pub fn url(&self) -> String {
        self.front.url()
    }

    /// See [`TlsFront::ca_path`].
    pub fn ca_path(&self) -> &Path {
        self.front.ca_path()
    }

    /// See [`TlsFront::web_config`].
    pub fn web_config(&self) -> WebConfig {
        self.front.web_config()
    }
}
