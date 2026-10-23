// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The server's `X-Request-Id` on every error that saw a response
//! (ADR-0057, AC11), and `Retry-After` on a server error.
//!
//! Each test serves one connection from a rustls listener on `127.0.0.1`
//! whose self-signed certificate the client pins as its CA, as
//! `upload_cut_off.rs` does. The listener writes one canned response, or
//! hangs up with a reset.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;
use trawl_api::{ErrorEnvelope, ErrorResponse};
use trawl_client::{CaptureRequest, ClientError, ErrorCode, HttpClient, NetworkKind, TlsTrust};

const TOKEN: &str = "flt_requestidtesttoken";

/// A ULID, the shape trawld's request ids take.
const REQUEST_ID: &str = "01JBQ7ZK3M8X2V5N9R4T6W0YCD";

/// A result id the client accepts.
const RESULT_ID: &str = "0123456789abcdef0123456789abcdef";

/// Bound on every exchange, so a hang fails the test instead of wedging CI.
const EXCHANGE: Duration = Duration::from_secs(30);

/// What the listener does once it has read a request head.
enum Answer {
    /// Hang up with a reset and no response.
    Reset,
    /// Write this response and close cleanly.
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
            // A zero linger turns the close below into a reset.
            tcp.set_zero_linger().expect("zero linger");
        }
        let mut tls = acceptor.accept(tcp).await.expect("TLS handshake");
        let (head, mut body_read) = read_head(&mut tls).await;
        let length = content_length(&head);
        let mut buf = vec![0u8; 64 * 1024];
        while body_read < length {
            let n = tls.read(&mut buf).await.expect("body read");
            assert!(n > 0, "the client closed mid-body");
            body_read += n;
        }
        match answer {
            Answer::Reset => drop(tls),
            Answer::Respond(response) => {
                // A client that stops at its body cap hangs up under the
                // rest, so a failed write is the test's subject, not a
                // listener fault.
                if tls.write_all(&response).await.is_ok() {
                    let _ = tls.shutdown().await;
                }
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

/// A whole HTTP/1.1 response: `status` (`"400 Bad Request"`), the extra
/// `headers`, and `body` with its length.
fn response(status: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    use std::fmt::Write as _;
    let mut head = format!("HTTP/1.1 {status}\r\ncontent-type: application/json\r\n");
    for (name, value) in headers {
        write!(head, "{name}: {value}\r\n").expect("a String takes every write");
    }
    write!(
        head,
        "content-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )
    .expect("a String takes every write");
    let mut out = head.into_bytes();
    out.extend_from_slice(body);
    out
}

/// The error envelope trawld would send for `code`.
fn envelope(code: ErrorCode, message: &str) -> Vec<u8> {
    serde_json::to_vec(&ErrorResponse {
        error: ErrorEnvelope::simple(code, message),
    })
    .expect("envelope encodes")
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

fn capture_request() -> CaptureRequest {
    CaptureRequest {
        query: "_service=nginx last=1h".to_owned(),
        window: None,
    }
}

/// A 400 refusal names the request it refused, and the rendered error
/// quotes it.
#[tokio::test]
async fn a_400_carries_the_request_id() {
    let body = envelope(
        ErrorCode::BadRequest,
        "add earliest= or last= beside latest=",
    );
    let canned = response("400 Bad Request", &[("x-request-id", REQUEST_ID)], &body);
    let (client, served) = serve_one(Answer::Respond(canned)).await;
    let error = exchange(client.capture(&capture_request()), served)
        .await
        .expect_err("a 400 is an error");
    let ClientError::Server {
        status,
        error: envelope,
        request_id,
        retry_after_secs,
    } = &error
    else {
        panic!("a 400 is a server error: {error:?}");
    };
    assert_eq!(*status, 400);
    assert_eq!(envelope.code, ErrorCode::BadRequest);
    assert_eq!(request_id.as_deref(), Some(REQUEST_ID));
    assert_eq!(*retry_after_secs, None);
    assert_eq!(error.request_id(), Some(REQUEST_ID));
    assert_eq!(
        error.to_string(),
        format!(
            "server error (HTTP 400): add earliest= or last= beside latest= (request {REQUEST_ID})"
        )
    );
}

/// A 500 carries its id the same way.
#[tokio::test]
async fn a_500_carries_the_request_id() {
    let body = envelope(ErrorCode::ResultCorrupt, "the result's file does not match");
    let canned = response(
        "500 Internal Server Error",
        &[("x-request-id", REQUEST_ID)],
        &body,
    );
    let (client, served) = serve_one(Answer::Respond(canned)).await;
    let error = exchange(client.result_page(RESULT_ID, 0, 10, None), served)
        .await
        .expect_err("a 500 is an error");
    assert!(
        matches!(&error, ClientError::Server { status: 500, .. }),
        "{error:?}"
    );
    assert_eq!(error.error_code(), Some(&ErrorCode::ResultCorrupt));
    assert_eq!(error.request_id(), Some(REQUEST_ID));
    assert!(error.to_string().contains(REQUEST_ID), "{error}");
}

/// A server error without the header names no id, and its text is the
/// one it always was.
#[tokio::test]
async fn a_server_error_without_the_header_names_no_id() {
    let body = envelope(ErrorCode::BadRequest, "bad window");
    let canned = response("400 Bad Request", &[], &body);
    let (client, served) = serve_one(Answer::Respond(canned)).await;
    let error = exchange(client.capture(&capture_request()), served)
        .await
        .expect_err("a 400 is an error");
    assert_eq!(error.request_id(), None);
    assert_eq!(error.to_string(), "server error (HTTP 400): bad window");
}

/// A 429 carries the numeric `Retry-After` beside the id.
#[tokio::test]
async fn a_429_carries_retry_after() {
    let body = envelope(ErrorCode::CaptureBusy, "a capture of this key is running");
    let canned = response(
        "429 Too Many Requests",
        &[("x-request-id", REQUEST_ID), ("retry-after", "7")],
        &body,
    );
    let (client, served) = serve_one(Answer::Respond(canned)).await;
    let error = exchange(client.capture(&capture_request()), served)
        .await
        .expect_err("a 429 is an error");
    let ClientError::Server {
        status,
        retry_after_secs,
        ..
    } = &error
    else {
        panic!("a 429 is a server error: {error:?}");
    };
    assert_eq!(*status, 429);
    assert_eq!(*retry_after_secs, Some(7));
    assert_eq!(error.error_code(), Some(&ErrorCode::CaptureBusy));
    assert_eq!(error.request_id(), Some(REQUEST_ID));
}

/// The HTTP-date form of `Retry-After`, and anything else that is not
/// digits alone, reads as no wait at all.
#[tokio::test]
async fn a_retry_after_that_is_not_seconds_is_none() {
    for value in [
        "Wed, 21 Oct 2026 07:28:00 GMT",
        "+7",
        "7.5",
        "soon",
        "4294967296",
    ] {
        let body = envelope(ErrorCode::RateLimited, "slow down");
        let canned = response("429 Too Many Requests", &[("retry-after", value)], &body);
        let (client, served) = serve_one(Answer::Respond(canned)).await;
        let error = exchange(client.list_results(), served)
            .await
            .expect_err("a 429 is an error");
        assert!(
            matches!(
                &error,
                ClientError::Server {
                    status: 429,
                    retry_after_secs: None,
                    ..
                }
            ),
            "{value:?} gave {error:?}"
        );
    }
}

/// A 200 whose body is not JSON is a parse failure that names the
/// response's id, on the uncapped path and the capped one.
#[tokio::test]
async fn a_200_that_does_not_parse_carries_the_request_id() {
    let canned = response(
        "200 OK",
        &[("x-request-id", REQUEST_ID)],
        b"<html>a proxy</html>",
    );
    let (client, served) = serve_one(Answer::Respond(canned.clone())).await;
    let error = exchange(client.list_results(), served)
        .await
        .expect_err("HTML is not a result list");
    assert!(
        matches!(&error, ClientError::Parse { request_id: Some(id), .. } if id == REQUEST_ID),
        "{error:?}"
    );
    assert_eq!(error.request_id(), Some(REQUEST_ID));

    let (client, served) = serve_one(Answer::Respond(canned)).await;
    let error = exchange(client.result_page(RESULT_ID, 0, 10, None), served)
        .await
        .expect_err("HTML is not a result page");
    assert!(
        matches!(&error, ClientError::Parse { request_id: Some(id), .. } if id == REQUEST_ID),
        "{error:?}"
    );
}

/// A 200 body past the endpoint's cap is too large, and still names the
/// response's id. whoami's cap is 64 KiB.
#[tokio::test]
async fn a_200_past_the_cap_carries_the_request_id() {
    let body = vec![b' '; 128 * 1024];
    let canned = response("200 OK", &[("x-request-id", REQUEST_ID)], &body);
    let (client, served) = serve_one(Answer::Respond(canned)).await;
    let error = exchange(client.whoami(), served)
        .await
        .expect_err("a body past the cap is refused");
    assert!(
        matches!(&error, ClientError::TooLarge { request_id: Some(id), .. } if id == REQUEST_ID),
        "{error:?}"
    );
    assert_eq!(error.request_id(), Some(REQUEST_ID));
}

/// A reset before any response is a network error, and there is no id to
/// name.
#[tokio::test]
async fn a_reset_names_no_request_id() {
    let (client, served) = serve_one(Answer::Reset).await;
    let error = exchange(client.list_results(), served)
        .await
        .expect_err("a reset has no answer");
    assert!(matches!(&error, ClientError::Network(_)), "{error:?}");
    assert_ne!(error.network_kind(), None);
    assert_ne!(error.network_kind(), Some(NetworkKind::UploadCutOff));
    assert_eq!(error.request_id(), None);
}
