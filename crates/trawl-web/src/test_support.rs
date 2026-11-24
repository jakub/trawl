// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Test upstreams that speak real TLS under a pinned CA.
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
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::task::{JoinHandle, JoinSet};
use tokio_rustls::TlsAcceptor;
use trawl_config::WebConfig;
use wiremock::MockServer;

pub const LOOPBACK_SANS: [&str; 2] = ["localhost", "127.0.0.1"];

/// The kernel buffer, send and receive, asked for on every socket a
/// narrow fixture owns ([`TlsFront::narrow`], [`HandUpstream::start_narrow`]).
/// Linux doubles it to 128 KiB and, since it was set, never autotunes it.
pub const NARROW_SOCKET_BUFFER: u32 = 64 * 1024;

pub const WEDGE_BODY_FLOOR: usize = 16 * 1024 * 1024;

pub fn wedge_body_bytes() -> usize {
    wedge_body_bytes_for(
        std::fs::read_to_string("/proc/sys/net/ipv4/tcp_wmem")
            .ok()
            .as_deref(),
    )
}

/// `tcp_wmem`: three whitespace-separated byte counts, minimum, default and
/// maximum.
pub fn wedge_body_bytes_for(tcp_wmem: Option<&str>) -> usize {
    const MIB: usize = 1024 * 1024;
    let maximum = tcp_wmem
        .and_then(|text| text.split_whitespace().nth(2))
        .and_then(|field| field.parse::<usize>().ok());
    maximum.map_or(WEDGE_BODY_FLOOR, |maximum| {
        maximum
            .saturating_mul(4)
            .div_ceil(MIB)
            .saturating_mul(MIB)
            .max(WEDGE_BODY_FLOOR)
    })
}

/// A listener on `127.0.0.1:0`, its kernel buffers locked at `buffer`
/// bytes when one is given. Accepted sockets inherit the lock.
async fn listen_loopback(buffer: Option<u32>) -> TcpListener {
    let Some(bytes) = buffer else {
        return TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback listener");
    };
    let socket = TcpSocket::new_v4().expect("a loopback socket");
    socket.set_send_buffer_size(bytes).expect("SO_SNDBUF");
    socket.set_recv_buffer_size(bytes).expect("SO_RCVBUF");
    socket
        .bind("127.0.0.1:0".parse().expect("loopback address"))
        .expect("bind a loopback listener");
    socket.listen(128).expect("listen on loopback")
}

async fn connect_loopback(target: SocketAddr, buffer: Option<u32>) -> std::io::Result<TcpStream> {
    let Some(bytes) = buffer else {
        return TcpStream::connect(target).await;
    };
    let socket = if target.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
    socket.set_send_buffer_size(bytes)?;
    socket.set_recv_buffer_size(bytes)?;
    socket.connect(target).await
}

pub struct TestCa {
    issuer: CertifiedIssuer<'static, KeyPair>,
    path: PathBuf,
    _dir: tempfile::TempDir,
}

impl TestCa {
    pub fn generate() -> Arc<Self> {
        Self::named("trawl-web test CA")
    }

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

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn pem(&self) -> String {
        self.issuer.pem()
    }

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

pub struct TlsFront {
    addr: SocketAddr,
    ca: Arc<TestCa>,
    counts: Arc<FrontCounts>,
    accept: JoinHandle<()>,
}

#[derive(Debug, Default)]
struct FrontCounts {
    connections: AtomicUsize,
    handshake_failures: AtomicUsize,
    open: AtomicUsize,
}

struct OpenConnection(Arc<FrontCounts>);

impl Drop for OpenConnection {
    fn drop(&mut self) {
        self.0.open.fetch_sub(1, Ordering::SeqCst);
    }
}

impl TlsFront {
    pub async fn start(target: SocketAddr) -> Self {
        Self::with_sans(target, &LOOPBACK_SANS).await
    }

    pub async fn with_sans(target: SocketAddr, sans: &[&str]) -> Self {
        Self::issued_by(&TestCa::generate(), target, sans).await
    }

    pub async fn issued_by(ca: &Arc<TestCa>, target: SocketAddr, sans: &[&str]) -> Self {
        Self::build(ca, target, sans, None).await
    }

    pub async fn narrow(target: SocketAddr) -> Self {
        Self::build(
            &TestCa::generate(),
            target,
            &LOOPBACK_SANS,
            Some(NARROW_SOCKET_BUFFER),
        )
        .await
    }

    async fn build(
        ca: &Arc<TestCa>,
        target: SocketAddr,
        sans: &[&str],
        buffer: Option<u32>,
    ) -> Self {
        let acceptor = TlsAcceptor::from(Arc::new(ca.server_config(sans)));
        let listener = listen_loopback(buffer).await;
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
                relays.spawn(async move {
                    let Ok(mut tls) = acceptor.accept(client).await else {
                        open.0.handshake_failures.fetch_add(1, Ordering::SeqCst);
                        return;
                    };
                    let Ok(mut plain) = connect_loopback(target, buffer).await else {
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

    pub fn connections(&self) -> usize {
        self.counts.connections.load(Ordering::SeqCst)
    }

    pub fn handshake_failures(&self) -> usize {
        self.counts.handshake_failures.load(Ordering::SeqCst)
    }

    pub fn open_connections(&self) -> usize {
        self.counts.open.load(Ordering::SeqCst)
    }

    /// Wait until every connection the front accepted has finished. Call it
    /// after the client is gone.
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

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    pub fn url(&self) -> String {
        format!("https://{}", self.addr)
    }

    pub fn ca(&self) -> &Arc<TestCa> {
        &self.ca
    }

    pub fn ca_path(&self) -> &Path {
        self.ca.path()
    }

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

#[derive(Debug)]
pub struct TlsUpstream {
    front: TlsFront,
    mock: MockServer,
}

impl TlsUpstream {
    pub async fn start() -> Self {
        Self::issued_by(&TestCa::generate()).await
    }

    pub async fn issued_by(ca: &Arc<TestCa>) -> Self {
        let mock = MockServer::start().await;
        let front = TlsFront::issued_by(ca, *mock.address(), &LOOPBACK_SANS).await;
        Self { front, mock }
    }

    /// Its own `uri()` is the plain listener behind the front, which no
    /// proxy under test should dial.
    pub fn mock(&self) -> &MockServer {
        &self.mock
    }

    pub fn front(&self) -> &TlsFront {
        &self.front
    }

    pub fn url(&self) -> String {
        self.front.url()
    }

    pub fn ca_path(&self) -> &Path {
        self.front.ca_path()
    }

    pub fn web_config(&self) -> WebConfig {
        self.front.web_config()
    }
}

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

/// `tracing` caches each callsite's `Interest` process-wide and rebuilds
/// that cache when a subscriber registers or dies. Two capture tests
/// running at once can leave a callsite cached as "never interested" for
/// the thread about to emit, so the event vanishes.
static CAPTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// `f` must not move work to another thread: the subscriber is this
/// thread's default only.
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

pub struct HandUpstream {
    front: TlsFront,
    seen: Arc<std::sync::Mutex<Vec<String>>>,
    hold: tokio::sync::watch::Sender<bool>,
    accept: JoinHandle<()>,
}

impl HandUpstream {
    pub const EVENT_INTERVAL: Duration = Duration::from_millis(25);

    pub const FLOOD_PATH: &'static str = "/api/v1/flood";

    pub async fn start() -> Self {
        Self::start_with(None).await
    }

    pub async fn start_narrow() -> Self {
        Self::start_with(Some(NARROW_SOCKET_BUFFER)).await
    }

    async fn start_with(buffer: Option<u32>) -> Self {
        let plain = listen_loopback(buffer).await;
        let plain_addr = plain.local_addr().expect("hand upstream address");
        let front = TlsFront::build(&TestCa::generate(), plain_addr, &LOOPBACK_SANS, buffer).await;
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (hold, holding) = tokio::sync::watch::channel(false);
        let log = Arc::clone(&seen);
        let accept = tokio::spawn(async move {
            let mut answers = JoinSet::new();
            while let Ok((stream, _)) = plain.accept().await {
                while answers.try_join_next().is_some() {}
                let log = Arc::clone(&log);
                let holding = holding.clone();
                answers.spawn(Self::answer(stream, log, holding));
            }
        });
        Self {
            front,
            seen,
            hold,
            accept,
        }
    }

    pub fn hold(&self) {
        self.hold.send_replace(true);
    }

    pub fn release(&self) {
        self.hold.send_replace(false);
    }

    pub fn seen(&self) -> Vec<String> {
        self.seen.lock().expect("hand upstream log").clone()
    }

    pub async fn wait_for_requests(&self, n: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.seen().len() < n {
            assert!(
                Instant::now() < deadline,
                "only {} of {n} requests reached the hand upstream: {:?}",
                self.seen().len(),
                self.seen()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    pub fn web_config(&self) -> WebConfig {
        self.front.web_config()
    }

    async fn answer(
        mut stream: TcpStream,
        log: Arc<std::sync::Mutex<Vec<String>>>,
        mut holding: tokio::sync::watch::Receiver<bool>,
    ) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let mut raw = Vec::new();
        let head_end = loop {
            if let Some(at) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                break at + 4;
            }
            let mut buf = [0_u8; 4096];
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => raw.extend_from_slice(&buf[..n]),
            }
        };
        let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
        let mut lines = head.lines();
        let request_line = lines.next().unwrap_or_default();
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_owned();
        let path = parts.next().unwrap_or_default().to_owned();
        let bare = path.split('?').next().unwrap_or_default().to_owned();

        if bare == Self::FLOOD_PATH {
            log.lock()
                .expect("hand upstream log")
                .push(format!("{method} {bare}"));
            Self::flood(stream).await;
            return;
        }

        let length: usize = lines
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse().ok())
            .unwrap_or(0);
        let mut body_read = raw.len() - head_end;
        while body_read < length {
            let mut buf = [0_u8; 4096];
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => body_read += n,
            }
        }
        log.lock()
            .expect("hand upstream log")
            .push(format!("{method} {bare}"));

        if bare == "/api/v1/whoami" {
            let body =
                r#"{"name":"alice","roles":["trawl-analyst"],"permissions":["query","stream"]}"#;
            let _ = stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await;
            return;
        }
        if bare == "/api/v1/stream" || bare == "/api/v1/dashboard/stream" {
            if stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                      cache-control: no-cache\r\ntransfer-encoding: chunked\r\n\r\n",
                )
                .await
                .is_err()
            {
                return;
            }
            let mut n = 0_u64;
            loop {
                tokio::time::sleep(Self::EVENT_INTERVAL).await;
                let event = format!("data: {n}\n\n");
                let chunk = format!("{:x}\r\n{event}\r\n", event.len());
                if stream.write_all(chunk.as_bytes()).await.is_err() {
                    return;
                }
                n += 1;
            }
        }
        let _ = holding.wait_for(|holding| !*holding).await;
        let body = "{}";
        let _ = stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await;
    }
}

impl HandUpstream {
    /// Answer [`Self::FLOOD_PATH`]: the head goes out before the request
    /// body is read, and the body is never read.
    async fn flood(mut stream: TcpStream) {
        use tokio::io::AsyncWriteExt as _;

        if stream
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\n\
                  transfer-encoding: chunked\r\n\r\n",
            )
            .await
            .is_err()
        {
            return;
        }
        let data = vec![b'x'; 64 * 1024];
        let mut chunk = format!("{:x}\r\n", data.len()).into_bytes();
        chunk.extend_from_slice(&data);
        chunk.extend_from_slice(b"\r\n");
        while stream.write_all(&chunk).await.is_ok() {}
    }
}

impl Drop for HandUpstream {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

impl std::fmt::Debug for HandUpstream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HandUpstream")
            .field("front", &self.front)
            .field("seen", &self.seen())
            .finish_non_exhaustive()
    }
}
