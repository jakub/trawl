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
//! expectation counts hold as they would without the relay. wiremock's
//! log keeps each request's method, path and headers, in a header map
//! whose names match without regard to case. The front itself counts the
//! connections it accepted and the handshakes that failed, which a request
//! log cannot show: a refused certificate never becomes a request.
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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

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
        Self::named("trawl-web test CA")
    }

    /// A fresh CA whose subject's common name is `common_name`, so a test
    /// can plant a value in it that no output may show.
    pub fn named(common_name: &str) -> Arc<Self> {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        params
            .distinguished_name
            .push(DnType::CommonName, common_name);
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
/// there. The front counts both: [`TlsFront::connections`] and
/// [`TlsFront::handshake_failures`].
pub struct TlsFront {
    addr: SocketAddr,
    ca: Arc<TestCa>,
    counts: Arc<FrontCounts>,
    accept: JoinHandle<()>,
}

/// What a [`TlsFront`] has seen.
#[derive(Debug, Default)]
struct FrontCounts {
    /// Connections accepted.
    connections: AtomicUsize,
    /// Accepted connections whose TLS handshake did not complete.
    handshake_failures: AtomicUsize,
    /// Accepted connections whose handshake or relay is still running.
    open: AtomicUsize,
}

/// Marks one accepted connection finished when its task ends, however it
/// ends.
struct OpenConnection(Arc<FrontCounts>);

impl Drop for OpenConnection {
    fn drop(&mut self) {
        self.0.open.fetch_sub(1, Ordering::SeqCst);
    }
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
        let counts = Arc::new(FrontCounts::default());
        let seen = Arc::clone(&counts);
        let accept = tokio::spawn(async move {
            // The relays belong to this task, so aborting it on drop ends
            // every open connection too.
            let mut relays = JoinSet::new();
            while let Ok((client, _)) = listener.accept().await {
                while relays.try_join_next().is_some() {}
                seen.connections.fetch_add(1, Ordering::SeqCst);
                seen.open.fetch_add(1, Ordering::SeqCst);
                let open = OpenConnection(Arc::clone(&seen));
                let acceptor = acceptor.clone();
                // `open` moves into the task and is dropped when it ends.
                relays.spawn(async move {
                    let Ok(mut tls) = acceptor.accept(client).await else {
                        open.0.handshake_failures.fetch_add(1, Ordering::SeqCst);
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
            counts,
            accept,
        }
    }

    /// How many connections the front has accepted.
    pub fn connections(&self) -> usize {
        self.counts.connections.load(Ordering::SeqCst)
    }

    /// How many accepted connections ended before their TLS handshake
    /// completed, such as a client refusing the front's certificate.
    pub fn handshake_failures(&self) -> usize {
        self.counts.handshake_failures.load(Ordering::SeqCst)
    }

    /// Wait until every connection the front accepted has finished, its
    /// handshake failed or its relay ended, so the counts no longer move
    /// for connections already made. Call it after the client is gone.
    ///
    /// # Panics
    /// When a connection is still open after 10 s.
    pub async fn settle(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.counts.open.load(Ordering::SeqCst) > 0 {
            assert!(
                Instant::now() < deadline,
                "a connection to the TLS front is still open: {:?}",
                self.counts
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
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
            .field("counts", &self.counts)
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

/// Collects one formatted `field=value` line per event.
///
/// Same shape as fleet-auth's guard-log capture: where a log line is the
/// deliverable, the test reads what was recorded rather than trusting that
/// the code meant to record it.
#[derive(Clone, Default)]
struct CaptureLayer {
    lines: Arc<std::sync::Mutex<Vec<String>>>,
}

struct FieldWriter<'a>(&'a mut String);

impl tracing::field::Visit for FieldWriter<'_> {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        use std::fmt::Write as _;
        let _ = write!(self.0, "{}={value} ", field.name());
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write as _;
        let _ = write!(self.0, "{}={value:?} ", field.name());
    }
}

impl<S> tracing_subscriber::Layer<S> for CaptureLayer
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut line = format!("{} ", event.metadata().level());
        event.record(&mut FieldWriter(&mut line));
        self.lines.lock().expect("capture mutex").push(line);
    }
}

/// Serializes the capture tests against each other.
///
/// `tracing` caches each callsite's `Interest` process-wide and rebuilds
/// that cache when a subscriber registers or dies. Two capture tests
/// running at once can leave a callsite cached as "never interested" for
/// the thread about to emit, so the event vanishes and the test reads "it
/// did not log", the exact failure it exists to catch, arriving at random.
/// Poisoning is ignored on purpose: one panicking test must not cascade
/// into the others.
static CAPTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Run `f` under a capturing subscriber on this thread, returning its
/// result and every line it logged. `f` must not move work to another
/// thread: the subscriber is this thread's default only.
pub fn captured_logs<T>(f: impl FnOnce() -> T) -> (T, Vec<String>) {
    use tracing_subscriber::layer::SubscriberExt as _;

    let _serialized = CAPTURE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let capture = CaptureLayer::default();
    let lines = Arc::clone(&capture.lines);
    let subscriber = tracing_subscriber::registry().with(capture);
    let _guard = tracing::subscriber::set_default(subscriber);
    let outcome = f();
    let recorded = lines.lock().expect("capture mutex").clone();
    (outcome, recorded)
}
