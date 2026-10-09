// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! An upload the server cuts off, observed through the real client.
//!
//! Each test serves one connection from a rustls listener on `127.0.0.1`
//! whose self-signed certificate the client pins as its CA, the way a
//! profile's `ca_cert` does. trawld refuses an oversized body with a 413
//! and hangs up without draining it, so a client still uploading can meet
//! the reset before it reads the answer. The resetting listener here never
//! answers at all: a 413 written first is usually read, because the kernel
//! hands over queued data before it reports the reset, so a test that
//! wrote one would race. Answering nothing is what the client sees whenever
//! the reset wins, and it makes every run the same.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;
use trawl_client::{ClientError, ErrorCode, HttpClient, NetworkKind, TlsTrust};

const TOKEN: &str = "flt_uploadcutofftesttoken";

/// The whole rendered cut-off error: no origin, path or library text.
const CUT_OFF: &str = "network error: the server closed the connection before the upload \
                       finished; the request may exceed the server's request size limit, \
                       or the server may be at its request limit";

/// Bound on every exchange, so a hang fails the test instead of wedging CI.
const EXCHANGE: Duration = Duration::from_secs(30);

/// How much of a body the resetting listener reads before it hangs up:
/// enough that an 8 MiB upload is under way, and all of a small one.
const BODY_SEEN: usize = 16 * 1024;

/// What the listener does once it has read a request.
enum Answer {
    /// Read the head and the first [`BODY_SEEN`] body bytes, then hang up
    /// with a reset and no response.
    Reset,
    /// Read the whole body, then write this response and close cleanly.
    Respond(Vec<u8>),
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

/// Serve one connection with `answer`. Returns a client that trusts the
/// listener's certificate as its pinned CA, and the serving task.
async fn serve_one(answer: Answer) -> (HttpClient, JoinHandle<()>) {
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
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address: SocketAddr = listener.local_addr().expect("local addr");
    let served = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.expect("accept");
        if matches!(answer, Answer::Reset) {
            // A zero linger turns the close below into a reset instead of
            // an orderly FIN. It is set before the handshake so no step of
            // the exchange can see the socket without it.
            tcp.set_zero_linger().expect("zero linger");
        }
        let mut tls = acceptor.accept(tcp).await.expect("TLS handshake");
        let (head, mut body_read) = read_head(&mut tls).await;
        let length = content_length(&head);
        let wanted = match &answer {
            Answer::Reset => length.min(BODY_SEEN),
            Answer::Respond(_) => length,
        };
        let mut buf = vec![0u8; 64 * 1024];
        while body_read < wanted {
            let n = tls.read(&mut buf).await.expect("body read");
            assert!(n > 0, "the client closed mid-body");
            body_read += n;
        }
        match answer {
            // Dropping the stream closes the socket under the unread
            // upload, which the zero linger makes a reset.
            Answer::Reset => drop(tls),
            Answer::Respond(response) => {
                tls.write_all(&response).await.expect("response write");
                tls.shutdown().await.expect("shutdown");
            }
        }
    });
    let client = HttpClient::with_trust(
        format!("https://{address}"),
        TOKEN,
        &TlsTrust::PinnedCa(pem),
    )
    .expect("client");
    (client, served)
}

/// Read up to the end of the request head. Returns the head and how many
/// body bytes arrived with it.
async fn read_head<S: AsyncReadExt + Unpin>(stream: &mut S) -> (String, usize) {
    let mut bytes = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&bytes[..end]).into_owned();
            return (head, bytes.len() - end - 4);
        }
        let n = stream.read(&mut buf).await.expect("head read");
        assert!(n > 0, "the client closed mid-head");
        bytes.extend_from_slice(&buf[..n]);
    }
}

/// The request's declared body length; zero without one.
fn content_length(head: &str) -> usize {
    head.lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .map_or(0, |(_, value)| {
            value.trim().parse().expect("content-length")
        })
}

/// Run `call` against the listener, both bounded by [`EXCHANGE`].
async fn exchange<T>(
    call: impl std::future::Future<Output = Result<T, ClientError>>,
    served: JoinHandle<()>,
) -> Result<T, ClientError> {
    let (result, served) = tokio::time::timeout(EXCHANGE, async { tokio::join!(call, served) })
        .await
        .expect("the exchange finished in time");
    served.expect("the listener ran to completion");
    result
}

/// An 8 MiB preview the server resets mid-upload is a cut-off upload,
/// never `request failed`.
#[tokio::test]
async fn a_reset_mid_upload_is_a_cut_off_upload() {
    let (client, served) = serve_one(Answer::Reset).await;
    let body = vec![b'x'; 8 * 1024 * 1024];
    let error = exchange(client.ingest_preview(body, None), served)
        .await
        .expect_err("a reset upload has no report");
    assert_eq!(
        error.network_kind(),
        Some(NetworkKind::UploadCutOff),
        "{error:?}"
    );
    assert_eq!(error.to_string(), CUT_OFF);
}

/// A JSON request through the shared authenticated path carries a body, so
/// a reset before its answer reads the same way.
#[tokio::test]
async fn a_reset_json_request_is_a_cut_off_upload() {
    let (client, served) = serve_one(Answer::Reset).await;
    let error = exchange(client.query_paginated("* | head 1", None, None), served)
        .await
        .expect_err("a reset request has no answer");
    assert_eq!(
        error.network_kind(),
        Some(NetworkKind::UploadCutOff),
        "{error:?}"
    );
    assert_eq!(error.to_string(), CUT_OFF);
}

/// The control: a request without a body uploads nothing, so the same
/// reset stays `request failed`.
#[tokio::test]
async fn a_reset_bodyless_request_is_not_a_cut_off_upload() {
    let (client, served) = serve_one(Answer::Reset).await;
    let error = exchange(client.stats(), served)
        .await
        .expect_err("a reset request has no answer");
    assert_eq!(error.network_kind(), Some(NetworkKind::Other), "{error:?}");
    assert_eq!(error.to_string(), "network error: request failed");
}

/// A 413 the client does read is the server's refusal, with the server's
/// own message, never a network error and never `unknown error`.
#[tokio::test]
async fn a_read_413_is_the_server_refusal() {
    let message = "request body exceeds [server] max_request_body_bytes (1024 bytes)";
    let envelope = format!(r#"{{"error":{{"code":"request_too_large","message":"{message}"}}}}"#);
    let response = format!(
        "HTTP/1.1 413 Payload Too Large\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{envelope}",
        envelope.len()
    );
    let (client, served) = serve_one(Answer::Respond(response.into_bytes())).await;
    let body = vec![b'x'; 4 * 1024];
    let error = exchange(client.ingest_preview(body, None), served)
        .await
        .expect_err("a 413 has no report");
    let ClientError::Server { status, error } = &error else {
        panic!("a read 413 is a server error: {error:?}");
    };
    assert_eq!(*status, 413);
    assert_eq!(error.code, ErrorCode::RequestTooLarge);
    assert_eq!(error.message, message);
}

/// The 503 counterpart of `a_read_413_is_the_server_refusal`: a refusal the
/// client does read is the server's own error, never the cut-off message,
/// and it names neither a 413 nor anything else the client did not read.
#[tokio::test]
async fn a_read_503_request_limit_is_the_server_refusal() {
    let message = "trawld is at its HTTP request limit ([server] max_concurrent_requests); \
                   the request was not processed; retry later with backoff";
    let envelope =
        format!(r#"{{"error":{{"code":"request_limit_reached","message":"{message}"}}}}"#);
    let response = format!(
        "HTTP/1.1 503 Service Unavailable\r\ncontent-type: application/json\r\ncache-control: no-store\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{envelope}",
        envelope.len()
    );
    let (client, served) = serve_one(Answer::Respond(response.into_bytes())).await;
    let body = vec![b'x'; 4 * 1024];
    let error = exchange(client.ingest_preview(body, None), served)
        .await
        .expect_err("a 503 has no report");
    assert_eq!(error.network_kind(), None, "{error:?}");
    let ClientError::Server { status, error } = &error else {
        panic!("a read 503 is a server error: {error:?}");
    };
    assert_eq!(*status, 503);
    assert_eq!(error.message, message);
    assert!(!error.message.contains("413"), "{}", error.message);
    // The wire code is raw JSON on purpose: this test does not depend on
    // the `ErrorCode` variant. Whatever the client maps it to, it is not
    // the cut-off message.
    assert_ne!(
        ClientError::Server {
            status: 503,
            error: error.clone()
        }
        .to_string(),
        CUT_OFF
    );
}
