// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The captured-result readers stop at their body cap whatever the status.
//!
//! `result_page` and `result_cell` promise that no body is read past
//! `RESULT_PAGE_BODY_CAP` / `RESULT_CELL_BODY_CAP`. A misconfigured endpoint
//! or a reverse proxy can answer a 429 or a 500 with a body of any size, so
//! the promise has to hold for an error status as much as for a 200.
//!
//! Each test serves one connection from a rustls listener on `127.0.0.1`
//! whose self-signed certificate the client pins, as `results_routes.rs`
//! does. The listener answers with a chunked body longer than the cap and
//! stops writing the moment a write fails, which is when the client has
//! dropped the connection. The number of body bytes it got out before then
//! is how far the client read, give or take what the kernel buffered.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpSocket;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;
use trawl_client::client::{RESULT_CELL_BODY_CAP, RESULT_PAGE_BODY_CAP};
use trawl_client::{ClientError, ErrorCode, HttpClient, TlsTrust};

const TOKEN: &str = "flt_resultbodycaptesttoken";

/// A result id the client accepts.
const RESULT_ID: &str = "0123456789abcdef0123456789abcdef";

/// Bound on every exchange, so a hang fails the test instead of wedging CI.
const EXCHANGE: Duration = Duration::from_secs(60);

/// The size of one chunk the listener writes.
const CHUNK: usize = 64 * 1024;

/// The most body bytes the listener offers: four caps, so a client that
/// reads to the end has read far past anything an early stop explains.
const OFFERED: usize = 4 * RESULT_PAGE_BODY_CAP;

/// How far past the cap the listener may get out before the client's
/// close reaches it: the chunk that crosses the cap, plus what the kernel
/// held in the send and receive buffers of the loopback connection (the
/// receive buffer autotunes to a few MiB). A client that buffered the body
/// whole sees the listener finish all of [`OFFERED`], 48 MiB further on.
const RUNOFF: usize = 12 * 1024 * 1024;

const _: () = assert!(RESULT_PAGE_BODY_CAP == RESULT_CELL_BODY_CAP);
const CAP: usize = RESULT_PAGE_BODY_CAP;

/// What the listener answers with.
struct Answer {
    status: &'static str,
    /// Extra header lines, each ending in CRLF.
    headers: &'static str,
}

/// A self-signed certificate for `127.0.0.1`: its PEM, for the client to
/// pin, and the server identity.
fn self_signed() -> (
    Vec<u8>,
    Vec<CertificateDer<'static>>,
    PrivateKeyDer<'static>,
) {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).expect("self-signed pair");
    (
        cert.pem().into_bytes(),
        vec![cert.der().clone()],
        PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into(),
    )
}

/// Serve one connection: read the request head, answer `answer` with a
/// chunked body of [`OFFERED`] filler bytes, stopping at the first failed
/// write. The task hands back how many body bytes it wrote.
fn serve_endless(answer: &Answer) -> (HttpClient, JoinHandle<usize>) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (pem, chain, key) = self_signed();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("protocol versions")
    .with_no_client_auth()
    .with_single_cert(chain, key)
    .expect("server certificate");
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let socket = TcpSocket::new_v4().expect("socket");
    // A small send buffer keeps the listener's lead over the client short.
    socket
        .set_send_buffer_size(u32::try_from(CHUNK).expect("chunk fits"))
        .expect("sndbuf");
    socket.bind("127.0.0.1:0".parse().unwrap()).expect("bind");
    let listener = socket.listen(1).expect("listen");
    let address: SocketAddr = listener.local_addr().expect("local addr");
    let response = format!(
        "HTTP/1.1 {}\r\ncontent-type: application/json\r\n{}\
         transfer-encoding: chunked\r\nconnection: close\r\n\r\n",
        answer.status, answer.headers
    );
    let served = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.expect("accept");
        let mut tls = acceptor.accept(tcp).await.expect("TLS handshake");
        let mut head = Vec::new();
        let mut buf = [0u8; 4096];
        while !head.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = tls.read(&mut buf).await.expect("head read");
            assert!(n > 0, "the client closed mid-head");
            head.extend_from_slice(&buf[..n]);
        }
        if tls.write_all(response.as_bytes()).await.is_err() {
            return 0;
        }
        let mut frame = format!("{CHUNK:x}\r\n").into_bytes();
        frame.extend(std::iter::repeat_n(b'x', CHUNK));
        frame.extend_from_slice(b"\r\n");
        let mut written = 0;
        while written < OFFERED {
            if tls.write_all(&frame).await.is_err() || tls.flush().await.is_err() {
                return written;
            }
            written += CHUNK;
        }
        let _ = tls.write_all(b"0\r\n\r\n").await;
        let _ = tls.shutdown().await;
        written
    });
    let client = HttpClient::with_trust(
        format!("https://{address}"),
        TOKEN,
        &TlsTrust::PinnedCa(pem),
    )
    .expect("client");
    (client, served)
}

/// Run `call` against the listener, both bounded by [`EXCHANGE`]. Returns
/// the call's outcome and the body bytes the listener wrote.
async fn exchange<T>(
    call: impl std::future::Future<Output = Result<T, ClientError>>,
    served: JoinHandle<usize>,
) -> (Result<T, ClientError>, usize) {
    let (result, written) = tokio::time::timeout(EXCHANGE, async { tokio::join!(call, served) })
        .await
        .expect("the exchange finished in time");
    (result, written.expect("the listener ran to completion"))
}

/// Assert the client stopped reading near the cap: the listener's own
/// writes failed before it had offered everything, and got no further than
/// the cap plus [`RUNOFF`].
fn assert_stopped_at_cap(written: usize) {
    assert!(
        written <= CAP + RUNOFF,
        "the client read {written} body bytes of a body capped at {CAP}"
    );
}

/// The `Server` error an error status over the cap must become: the
/// fallback envelope, the status and both headers kept.
fn assert_fallback_server_error(
    error: &ClientError,
    want_status: u16,
    want_request_id: Option<&str>,
    want_retry_after: Option<u32>,
) {
    let ClientError::Server {
        status,
        error,
        request_id,
        retry_after_secs,
    } = error
    else {
        panic!("an error status is a Server error, got {error:?}");
    };
    assert_eq!(*status, want_status);
    assert_eq!(error.code, ErrorCode::InternalError);
    assert_eq!(error.message, "unknown error");
    assert_eq!(request_id.as_deref(), want_request_id);
    assert_eq!(*retry_after_secs, want_retry_after);
}

const INTERNAL: Answer = Answer {
    status: "500 Internal Server Error",
    headers: "",
};

const THROTTLED: Answer = Answer {
    status: "429 Too Many Requests",
    headers: "retry-after: 7\r\nx-request-id: req-429-over-cap\r\n",
};

/// A 500 whose body outruns the cap is a `Server` error on the fallback
/// envelope, and the page reader stops reading at the cap.
#[tokio::test]
async fn result_page_error_body_over_the_cap_stops_at_the_cap() {
    let (client, served) = serve_endless(&INTERNAL);
    let (result, written) = exchange(client.result_page(RESULT_ID, 0, 10, None), served).await;
    assert_fallback_server_error(&result.expect_err("a 500 fails"), 500, None, None);
    assert_stopped_at_cap(written);
}

/// A 429 keeps its status, request id and `Retry-After` when its body
/// outruns the cap: the headers were read before the body.
#[tokio::test]
async fn result_page_throttle_body_over_the_cap_keeps_its_headers() {
    let (client, served) = serve_endless(&THROTTLED);
    let (result, written) = exchange(client.result_page(RESULT_ID, 0, 10, None), served).await;
    assert_fallback_server_error(
        &result.expect_err("a 429 fails"),
        429,
        Some("req-429-over-cap"),
        Some(7),
    );
    assert_stopped_at_cap(written);
}

/// The cell reader holds the same line for a 500.
#[tokio::test]
async fn result_cell_error_body_over_the_cap_stops_at_the_cap() {
    let (client, served) = serve_endless(&INTERNAL);
    let (result, written) = exchange(client.result_cell(RESULT_ID, 0, 0, None, None), served).await;
    assert_fallback_server_error(&result.expect_err("a 500 fails"), 500, None, None);
    assert_stopped_at_cap(written);
}

/// The cell reader holds the same line for a 429.
#[tokio::test]
async fn result_cell_throttle_body_over_the_cap_keeps_its_headers() {
    let (client, served) = serve_endless(&THROTTLED);
    let (result, written) = exchange(client.result_cell(RESULT_ID, 0, 0, None, None), served).await;
    assert_fallback_server_error(
        &result.expect_err("a 429 fails"),
        429,
        Some("req-429-over-cap"),
        Some(7),
    );
    assert_stopped_at_cap(written);
}

/// A success body over the cap stays `TooLarge`: the error-status rule does
/// not soften the success path.
#[tokio::test]
async fn success_body_over_the_cap_is_too_large() {
    let ok = || Answer {
        status: "200 OK",
        headers: "x-request-id: req-200-over-cap\r\n",
    };
    let (client, served) = serve_endless(&ok());
    let (result, written) = exchange(client.result_page(RESULT_ID, 0, 10, None), served).await;
    match result.expect_err("an over-cap page fails") {
        ClientError::TooLarge { cap, request_id } => {
            assert_eq!(cap, RESULT_PAGE_BODY_CAP);
            assert_eq!(request_id.as_deref(), Some("req-200-over-cap"));
        }
        other => panic!("expected TooLarge, got {other:?}"),
    }
    assert_stopped_at_cap(written);

    let (client, served) = serve_endless(&ok());
    let (result, written) = exchange(client.result_cell(RESULT_ID, 0, 0, None, None), served).await;
    assert!(
        matches!(
            result,
            Err(ClientError::TooLarge { cap, .. }) if cap == RESULT_CELL_BODY_CAP
        ),
        "{result:?}"
    );
    assert_stopped_at_cap(written);
}
