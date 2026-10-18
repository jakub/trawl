// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The listener's deadlines, protocol and bookkeeping, through the
//! production TLS listener (ADR-0054, amended 2026-10-17).
//!
//! Every case boots a second server over the fixture's state through
//! `serve_with_listener_and_timeouts`, with deadlines short enough to wait
//! out, and talks to it over sockets of its own: a bare TCP stream for the
//! handshake cases, a rustls client stream with hand-written HTTP/1.1 for
//! the rest. The accept loop's own bookkeeping, how many connection tasks
//! it holds and how many handshakes it dropped at their deadline, is read
//! through the probe the seam reports to, so a case can tell a deadline
//! from a parse refusal and a reaped task from one still held.

mod common;

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::time::Instant;
use tokio_rustls::client::TlsStream;
use trawl_server::transport::http::{ConnectionTimeouts, ListenerProbe};

use crate::common::TestServer;

/// Deadlines a case can wait out several times over.
const SHORT: ConnectionTimeouts = ConnectionTimeouts {
    tls_handshake: Duration::from_millis(500),
    header_read: Duration::from_millis(500),
};

/// Added to a deadline before a case calls the server late: a failure
/// deadline for a loaded machine, never an ordering.
const SLACK: Duration = Duration::from_secs(5);

/// The least a deadline-driven close may take from the client's clock,
/// which starts a little after the server's: a close much sooner is a
/// refusal, not a deadline.
fn no_sooner_than(deadline: Duration) -> Duration {
    deadline.mul_f32(0.8)
}

/// The start of a TLS record carrying a `ClientHello`: a handshake record
/// of 512 bytes, a `ClientHello` of 508, the client version, and nothing
/// more. rustls reads the whole record before it parses, so a server that
/// receives only this waits for the rest.
const PARTIAL_CLIENT_HELLO: [u8; 11] = [
    0x16, 0x03, 0x01, 0x02, 0x00, 0x01, 0x00, 0x01, 0xfc, 0x03, 0x03,
];

/// A second server over `server`'s state at `timeouts`, with its probe.
async fn spawn(
    server: &TestServer,
    timeouts: ConnectionTimeouts,
) -> (String, Arc<ListenerProbe>, tokio::task::JoinHandle<()>) {
    let probe = Arc::new(ListenerProbe::default());
    let (url, task) = server
        .spawn_server_with_timeouts(timeouts, Arc::clone(&probe), None)
        .await;
    (url, probe, task)
}

/// `host:port` of an `https://` URL.
fn authority(url: &str) -> &str {
    url.strip_prefix("https://").expect("an https:// URL")
}

/// A rustls client config that trusts the fixture certificate and offers
/// exactly `alpn`, in order; an empty list offers no ALPN at all.
fn client_config(alpn: &[&[u8]]) -> Arc<rustls::ClientConfig> {
    use rustls::pki_types::pem::PemObject as _;

    let (cert_path, _) = common::ensure_test_cert();
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from_pem_file(&cert_path).unwrap())
        .unwrap();
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    Arc::new(config)
}

/// A TLS connection to `url` offering `alpn`, or the handshake's error.
async fn tls(url: &str, alpn: &[&[u8]]) -> std::io::Result<TlsStream<TcpStream>> {
    let authority = authority(url);
    let host = authority.rsplit_once(':').expect("a URL with a port").0;
    let tcp = TcpStream::connect(authority).await?;
    tokio_rustls::TlsConnector::from(client_config(alpn))
        .connect(
            rustls::pki_types::ServerName::try_from(host.to_owned()).unwrap(),
            tcp,
        )
        .await
}

/// A TLS connection to `url` speaking HTTP/1.1.
async fn h1(url: &str) -> TlsStream<TcpStream> {
    let stream = tls(url, &[b"http/1.1"]).await.expect("TLS handshake");
    assert_eq!(stream.get_ref().1.alpn_protocol(), Some(&b"http/1.1"[..]));
    stream
}

/// Read until the peer closes or the connection fails, within `within`.
/// `Ok` holds everything read; `Err` means the connection was still open
/// when `within` ran out.
async fn closed_within<S: tokio::io::AsyncRead + Unpin>(
    stream: &mut S,
    within: Duration,
) -> Result<Vec<u8>, Vec<u8>> {
    let mut seen = Vec::new();
    let deadline = Instant::now() + within;
    loop {
        let mut buf = [0_u8; 4096];
        match tokio::time::timeout_at(deadline, stream.read(&mut buf)).await {
            Ok(Ok(0) | Err(_)) => return Ok(seen),
            Ok(Ok(n)) => seen.extend_from_slice(&buf[..n]),
            Err(_elapsed) => return Err(seen),
        }
    }
}

/// One HTTP/1.1 response: its status, its header block (lower-cased
/// names, as sent otherwise) and its body, which must be delimited by
/// `content-length`.
async fn read_response<S: tokio::io::AsyncRead + Unpin>(stream: &mut S) -> (u16, String, Vec<u8>) {
    let mut raw = Vec::new();
    let head_end = loop {
        if let Some(at) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
        let mut buf = [0_u8; 4096];
        let n = tokio::time::timeout(SLACK, stream.read(&mut buf))
            .await
            .expect("a response head before the deadline")
            .expect("read the response head");
        assert!(n > 0, "the connection closed before a response head");
        raw.extend_from_slice(&buf[..n]);
    };
    let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
    let mut lines = head.lines();
    let status_line = lines.next().expect("a status line");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("a status code in {status_line:?}"));
    let headers: String = lines
        .map(|line| match line.split_once(':') {
            Some((name, value)) => format!("{}:{}\n", name.to_ascii_lowercase(), value),
            None => format!("{line}\n"),
        })
        .collect();
    assert!(
        !headers.contains("transfer-encoding: chunked"),
        "this reader needs a content-length: {headers}"
    );
    let length: usize = headers
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .map_or(0, |value| value.trim().parse().expect("a content-length"));
    let mut body = raw[head_end..].to_vec();
    while body.len() < length {
        let mut buf = [0_u8; 4096];
        let n = tokio::time::timeout(SLACK, stream.read(&mut buf))
            .await
            .expect("the body before the deadline")
            .expect("read the body");
        assert!(n > 0, "the connection closed inside the body");
        body.extend_from_slice(&buf[..n]);
    }
    body.truncate(length);
    (status, headers, body)
}

/// Poll `done` every 5 ms until it holds, or fail after [`SLACK`].
async fn wait_for(mut done: impl FnMut() -> bool, what: &str) {
    let reached = tokio::time::timeout(SLACK, async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(reached.is_ok(), "{what}: not reached within {SLACK:?}");
}

/// A health request on a fresh reqwest client, which closes its
/// connection when it is dropped.
async fn health_on_a_fresh_connection(url: &str) -> u16 {
    let client = common::harness_client_builder().build().unwrap();
    client
        .get(format!("{url}/api/v1/health"))
        .send()
        .await
        .expect("a fresh connection is served")
        .status()
        .as_u16()
}

/// A connection that sends part of a `ClientHello` and then nothing is
/// dropped at the handshake deadline, counted as a handshake timeout, not
/// refused by the parser.
#[tokio::test(flavor = "multi_thread")]
async fn listener_drops_slow_handshake() {
    let server = common::setup().await;
    let (url, probe, task) = spawn(&server, SHORT).await;

    let mut tcp = TcpStream::connect(authority(&url)).await.unwrap();
    tcp.write_all(&PARTIAL_CLIENT_HELLO).await.unwrap();
    let started = Instant::now();
    let closed = closed_within(&mut tcp, SHORT.tls_handshake + SLACK).await;
    assert!(
        closed.is_ok(),
        "the server kept a stalled handshake open past its deadline"
    );
    assert!(
        started.elapsed() >= no_sooner_than(SHORT.tls_handshake),
        "closed after {:?}, before the {:?} deadline: a refusal, not a timeout",
        started.elapsed(),
        SHORT.tls_handshake
    );
    wait_for(|| probe.handshake_timeouts() == 1, "one handshake timeout").await;
    task.abort();
}

/// What a stalled handshake held, its connection task and its socket, is
/// given back when it is dropped: the tasks are reaped, and the server
/// goes on serving. The listener has no handshake count of its own, so the
/// slot here is the task.
#[tokio::test(flavor = "multi_thread")]
async fn listener_releases_handshake_slot_after_timeout() {
    let server = common::setup().await;
    let (url, probe, task) = spawn(&server, SHORT).await;

    let mut stalled = Vec::new();
    for _ in 0..3 {
        let mut tcp = TcpStream::connect(authority(&url)).await.unwrap();
        tcp.write_all(&PARTIAL_CLIENT_HELLO).await.unwrap();
        stalled.push(tcp);
    }
    wait_for(
        || probe.handshake_timeouts() == 3,
        "three handshake timeouts",
    )
    .await;
    wait_for(
        || probe.tracked_connections() == 0,
        "the dropped handshakes' tasks reaped",
    )
    .await;
    for mut tcp in stalled {
        assert!(
            closed_within(&mut tcp, SLACK).await.is_ok(),
            "a dropped handshake's socket is closed"
        );
    }

    assert!(!task.is_finished(), "the serve loop ended");
    let status = health_on_a_fresh_connection(&url).await;
    assert!((200..600).contains(&status), "served after the timeouts");
    task.abort();
}

/// The shutdown signal ends a handshake in progress at once, so a peer
/// stalled in its handshake never holds the drain.
#[tokio::test(flavor = "multi_thread")]
async fn listener_shutdown_interrupts_handshake() {
    let server = common::setup().await;
    let long = ConnectionTimeouts {
        tls_handshake: Duration::from_secs(60),
        header_read: Duration::from_secs(60),
    };
    let probe = Arc::new(ListenerProbe::default());
    let (shutdown_tx, shutdown_rx) = trawl_server::shutdown::shutdown_channel();
    let (url, task) = server
        .spawn_server_with_timeouts(long, Arc::clone(&probe), Some(shutdown_rx))
        .await;

    // The readiness poll's connection answered before the server reaped its
    // task, so until that count reaches zero a count of one could still be
    // the readiness task, with the stalled connection sitting unaccepted in
    // the backlog. Shutdown would then close it without the handshake's
    // shutdown arm ever running.
    wait_for(
        || probe.tracked_connections() == 0,
        "the readiness connection's task reaped",
    )
    .await;

    let mut tcp = TcpStream::connect(authority(&url)).await.unwrap();
    tcp.write_all(&PARTIAL_CLIENT_HELLO).await.unwrap();
    // The readiness task is gone, so a tracked connection is this one,
    // accepted and in its handshake.
    wait_for(
        || probe.tracked_connections() == 1,
        "the stalled handshake tracked",
    )
    .await;

    let signalled = Instant::now();
    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(SLACK, task)
        .await
        .expect("the server stopped within the slack, not the handshake deadline")
        .expect("the serve task did not panic");
    assert!(
        closed_within(&mut tcp, SLACK).await.is_ok(),
        "the stalled handshake's socket is closed"
    );
    assert!(
        signalled.elapsed() < long.tls_handshake,
        "shutdown waited out the handshake deadline"
    );
    assert_eq!(probe.handshake_timeouts(), 0, "shutdown, not a timeout");
}

/// A request whose headers keep arriving one byte at a time is dropped at
/// the header deadline all the same: progress does not move it.
#[tokio::test(flavor = "multi_thread")]
async fn listener_drops_partial_headers_despite_byte_progress() {
    let server = common::setup().await;
    let (url, _probe, task) = spawn(&server, SHORT).await;

    let mut stream = h1(&url).await;
    stream
        .write_all(b"GET /api/v1/health HTTP/1.1\r\nhost: localhost\r\nx-slow: ")
        .await
        .unwrap();
    let started = Instant::now();
    let trickle = Duration::from_millis(50);
    let mut trickled = 0_usize;
    let closed_after = loop {
        // One byte of a header that never ends, then a short wait for the
        // server's close. A write error after the close is the close.
        if stream.write_all(b"a").await.is_err() {
            break started.elapsed();
        }
        trickled += 1;
        let mut buf = [0_u8; 64];
        match tokio::time::timeout(trickle, stream.read(&mut buf)).await {
            Ok(Ok(0) | Err(_)) => break started.elapsed(),
            Ok(Ok(n)) => panic!(
                "the server answered a request with no complete head: {:?}",
                String::from_utf8_lossy(&buf[..n])
            ),
            Err(_still_open) => {}
        }
        assert!(
            started.elapsed() < SHORT.header_read + SLACK,
            "headers still being read after {:?} and {trickled} bytes of progress",
            started.elapsed()
        );
    };
    assert!(
        closed_after >= no_sooner_than(SHORT.header_read),
        "closed after {closed_after:?}, before the {:?} deadline",
        SHORT.header_read
    );
    assert!(
        trickled >= 4,
        "only {trickled} bytes arrived before the close: no progress was made past the deadline"
    );
    task.abort();
}

/// After one complete exchange, a connection left idle is closed at the
/// header deadline: hyper arms the timer again for the next request.
#[tokio::test(flavor = "multi_thread")]
async fn listener_closes_idle_keep_alive() {
    let server = common::setup().await;
    let (url, _probe, task) = spawn(&server, SHORT).await;

    let mut stream = h1(&url).await;
    stream
        .write_all(b"GET /api/v1/health HTTP/1.1\r\nhost: localhost\r\n\r\n")
        .await
        .unwrap();
    let (status, headers, _body) = read_response(&mut stream).await;
    assert!((200..600).contains(&status), "{headers}");
    assert!(
        !headers.contains("connection: close"),
        "the first exchange kept the connection open: {headers}"
    );

    let idle_from = Instant::now();
    let closed = closed_within(&mut stream, SHORT.header_read + SLACK).await;
    assert!(closed.is_ok(), "an idle keep-alive connection stayed open");
    assert!(
        closed.unwrap().is_empty(),
        "the server wrote on an idle connection"
    );
    assert!(
        idle_from.elapsed() >= no_sooner_than(SHORT.header_read),
        "closed after {:?} idle, before the {:?} deadline",
        idle_from.elapsed(),
        SHORT.header_read
    );
    task.abort();
}

/// A server-sent event stream that sends nothing stays open past several
/// header deadlines: no timer runs while a response is being written.
#[tokio::test(flavor = "multi_thread")]
async fn listener_keeps_quiet_sse_open() {
    let server = common::setup().await;
    let (url, _probe, task) = spawn(&server, SHORT).await;

    let mut stream = h1(&url).await;
    let request = format!(
        "GET /api/v1/stream?query=_severity%3Derror HTTP/1.1\r\nhost: localhost\r\n\
         authorization: Bearer {}\r\n\r\n",
        server.analyst_token
    );
    stream.write_all(request.as_bytes()).await.unwrap();

    // The head, without the body reader: the stream has no length.
    let mut raw = Vec::new();
    while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
        let mut buf = [0_u8; 4096];
        let n = tokio::time::timeout(SLACK, stream.read(&mut buf))
            .await
            .expect("the stream's head")
            .expect("read the head");
        assert!(n > 0, "closed before the head");
        raw.extend_from_slice(&buf[..n]);
    }
    let head = String::from_utf8_lossy(&raw).to_ascii_lowercase();
    assert!(head.starts_with("http/1.1 200"), "{head}");
    assert!(head.contains("content-type: text/event-stream"), "{head}");

    // Quiet for four header deadlines: SSE's own keep-alive comment comes
    // every 15 s, far past this.
    let quiet = SHORT.header_read * 4;
    let still_open = closed_within(&mut stream, quiet).await;
    assert!(
        still_open.is_err(),
        "the stream was closed within {quiet:?}: {:?}",
        String::from_utf8_lossy(&still_open.unwrap())
    );
    task.abort();
}

/// An ingest body that takes longer than the header deadline to send is
/// read whole and answered: the timer stops once the headers are in.
#[tokio::test(flavor = "multi_thread")]
async fn listener_accepts_ingest_body_longer_than_header_timeout() {
    let server = common::setup().await;
    let (url, _probe, task) = spawn(&server, SHORT).await;

    let chunks: Vec<Vec<u8>> = (0..6)
        .map(|n| {
            let event = serde_json::json!({
                "service": "listener-timeouts",
                "message": format!("chunk {n}"),
            });
            let mut line = serde_json::to_vec(&event).unwrap();
            line.push(b'\n');
            line
        })
        .collect();
    let length: usize = chunks.iter().map(Vec::len).sum();

    let mut stream = h1(&url).await;
    let head = format!(
        "POST /api/v1/ingest HTTP/1.1\r\nhost: localhost\r\nauthorization: Bearer {}\r\n\
         content-type: application/x-ndjson\r\ncontent-length: {length}\r\n\r\n",
        server.ingest_token
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    // Six chunks 200 ms apart: 1.2 s of body, past two header deadlines.
    let pause = SHORT.header_read * 2 / 5;
    let started = Instant::now();
    for chunk in &chunks {
        tokio::time::sleep(pause).await;
        stream
            .write_all(chunk)
            .await
            .expect("the body is still being read");
    }
    assert!(
        started.elapsed() > SHORT.header_read * 2,
        "the body was sent within the header deadline, so nothing was proved"
    );

    let (status, headers, body) = read_response(&mut stream).await;
    assert_eq!(status, 200, "{headers}\n{}", String::from_utf8_lossy(&body));
    task.abort();
}

/// ALPN offers HTTP/1.1 alone: a client that also offers `h2` gets
/// HTTP/1.1, and a client that offers only `h2` is refused at the
/// handshake.
#[tokio::test(flavor = "multi_thread")]
async fn listener_negotiates_http11_only() {
    let server = common::setup().await;
    let (url, _probe, task) = spawn(&server, ConnectionTimeouts::DEFAULT).await;

    let mut both = tls(&url, &[b"h2", b"http/1.1"])
        .await
        .expect("a client offering both protocols connects");
    assert_eq!(
        both.get_ref().1.alpn_protocol(),
        Some(&b"http/1.1"[..]),
        "HTTP/1.1 is selected over h2"
    );
    both.write_all(b"GET /api/v1/health HTTP/1.1\r\nhost: localhost\r\n\r\n")
        .await
        .unwrap();
    let (status, _headers, _body) = read_response(&mut both).await;
    assert!((200..600).contains(&status));

    let h2_only = tls(&url, &[b"h2"]).await;
    assert!(
        h2_only.is_err(),
        "a client that offers only h2 completed a handshake"
    );
    task.abort();
}

/// A connection task is reaped when its connection ends, without waiting
/// for shutdown or for the next accept.
#[tokio::test(flavor = "multi_thread")]
async fn listener_reaps_completed_connections() {
    let server = common::setup().await;
    let (url, probe, task) = spawn(&server, ConnectionTimeouts::DEFAULT).await;

    for _ in 0..5 {
        let status = health_on_a_fresh_connection(&url).await;
        assert!((200..600).contains(&status));
    }
    // Every client is gone, so every connection task ends; the loop reaps
    // them with no new connection to prompt it.
    wait_for(
        || probe.tracked_connections() == 0,
        "five finished connection tasks reaped",
    )
    .await;
    assert!(!task.is_finished(), "the serve loop ended");
    task.abort();
}
