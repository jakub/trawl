// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The proxy's listener deadline and shutdown, through the production
//! serving function (ADR-0054, amended 2026-10-17).
//!
//! Each case binds a loopback port, serves the production router on it
//! through `trawl_web::listener::serve` with a deadline short enough to
//! wait out, and talks to it over a plain TCP stream with hand-written
//! HTTP/1.1. Only `/healthz` is asked, so no upstream exists.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::time::Instant;
use trawl_config::WebConfig;
use trawl_web::config::ResolvedConfig;
use trawl_web::listener::{ConnectionTimeouts, serve};
use trawl_web::routes;
use trawl_web::state::AppState;

/// A deadline a case can wait out several times over.
const SHORT: ConnectionTimeouts = ConnectionTimeouts {
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

/// The production router over a state that dials no upstream.
fn router() -> axum::Router {
    let web = WebConfig {
        upstream_url: Some("https://127.0.0.1:9".to_owned()),
        public_origins: vec!["http://127.0.0.1:8090".to_owned()],
        allow_insecure_cookies: true,
        ..WebConfig::default()
    };
    let cfg = ResolvedConfig::from_parsed(&web, None).expect("the fixture config resolves");
    routes::build(AppState::from_config(cfg).expect("the fixture state builds"))
}

/// Serve the router on a loopback port at `timeouts`: its address, the
/// sender that shuts it down, and the serve task.
async fn spawn(
    timeouts: ConnectionTimeouts,
) -> (SocketAddr, oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback port");
    let addr = listener.local_addr().expect("the bound address");
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(serve(listener, router(), timeouts, async {
        let _ = shutdown_rx.await;
    }));
    (addr, shutdown_tx, task)
}

/// Read until the peer closes or the connection fails, within `within`.
/// `Ok` holds everything read; `Err` means the connection was still open
/// when `within` ran out.
async fn closed_within(stream: &mut TcpStream, within: Duration) -> Result<Vec<u8>, Vec<u8>> {
    let mut seen = Vec::new();
    let deadline = Instant::now() + within;
    loop {
        let mut buf = [0_u8; 1024];
        match tokio::time::timeout_at(deadline, stream.read(&mut buf)).await {
            Ok(Ok(0) | Err(_)) => return Ok(seen),
            Ok(Ok(n)) => seen.extend_from_slice(&buf[..n]),
            Err(_elapsed) => return Err(seen),
        }
    }
}

/// One `/healthz` exchange on `stream`, which must stay open after it.
async fn healthz(stream: &mut TcpStream) {
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nhost: localhost\r\n\r\n")
        .await
        .unwrap();
    let mut raw = Vec::new();
    // The whole answer: the head, then the two-byte body `ok`.
    while !raw.ends_with(b"\r\n\r\nok") {
        let mut buf = [0_u8; 1024];
        let n = tokio::time::timeout(SLACK, stream.read(&mut buf))
            .await
            .expect("the health answer before the deadline")
            .expect("read the health answer");
        assert!(n > 0, "closed before the health answer");
        raw.extend_from_slice(&buf[..n]);
    }
    let head = String::from_utf8_lossy(&raw).to_ascii_lowercase();
    assert!(head.starts_with("http/1.1 200"), "{head}");
    assert!(!head.contains("connection: close"), "{head}");
}

/// A request whose headers keep arriving one byte at a time is dropped at
/// the header deadline all the same: progress does not move it.
#[tokio::test(flavor = "multi_thread")]
async fn listener_drops_partial_headers_despite_byte_progress() {
    let (addr, _shutdown, task) = spawn(SHORT).await;

    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nhost: localhost\r\nx-slow: ")
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
    let (addr, _shutdown, task) = spawn(SHORT).await;

    let mut stream = TcpStream::connect(addr).await.unwrap();
    healthz(&mut stream).await;

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

/// The shutdown future stops the listener and closes an idle keep-alive
/// connection at once, without waiting out the header deadline.
#[tokio::test(flavor = "multi_thread")]
async fn listener_shutdown_closes_idle_connections_and_returns() {
    let long = ConnectionTimeouts {
        header_read: Duration::from_secs(60),
    };
    let (addr, shutdown, task) = spawn(long).await;

    let mut stream = TcpStream::connect(addr).await.unwrap();
    healthz(&mut stream).await;

    let signalled = Instant::now();
    shutdown
        .send(())
        .expect("the listener is waiting on its shutdown");
    tokio::time::timeout(SLACK, task)
        .await
        .expect("the listener returned within the slack")
        .expect("the serve task did not panic");
    assert!(
        closed_within(&mut stream, SLACK).await.is_ok(),
        "the idle connection is closed by shutdown"
    );
    assert!(
        signalled.elapsed() < long.header_read,
        "shutdown waited out the header deadline"
    );
    assert!(
        TcpStream::connect(addr).await.is_err(),
        "the port is still accepting after shutdown"
    );
}
