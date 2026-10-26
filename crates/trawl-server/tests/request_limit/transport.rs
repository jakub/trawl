// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Connections through the production TLS listener, and the clients the
//! body-work tests share.
//!
//! The clients are explicit about connections: each HTTP/1.1 request is
//! written by hand over a TLS stream of its own, and read back from it.
//! The listener speaks HTTP/1.1 alone (ADR-0054, amended 2026-10-17), so
//! one connection is one request in progress at most. reqwest is used only
//! where any fresh connection will do.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::http::StatusCode;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use trawl_server::transport::request_limit::body_work::{BodyWork, HoldProbe};

use crate::common::{self, TestServer};
use crate::support::{DEADLINE, REGULAR_MESSAGE};

/// Serializes every `/metrics` scrape in this test binary. The recorder is
/// global under the plain `cargo test` harness, and a scrape sets
/// `trawl_query_permits_retained` from its own server's pool just before it
/// renders, so a scrape from any fixture can overwrite the sample another
/// fixture is about to read. A test holds this across its scrape and every
/// assertion on what the scrape rendered.
pub static SCRAPE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// AC14: through the production TLS listener, separate connections share
/// one count: N connections each holding a request at limit N meet exactly
/// one refusal on the N+1th, and the count recovers afterwards.
///
/// Each held request is ingest or preview work held at the state's seam,
/// which reports its entry, so a request is known to hold the count
/// before the next one is sent.
#[tokio::test(flavor = "multi_thread")]
async fn request_count_spans_connections() {
    const N: u8 = 2;
    let server = common::setup().await;
    let (url, task) = server
        .spawn_server_with(|http| http.max_concurrent_requests = usize::from(N), vec![])
        .await;

    // Two connections each hold one request, and a third is refused.
    let mut parse = Held::next(&server, BodyWork::IngestParse);
    let mut report = Held::next(&server, BodyWork::Preview);
    let mut first = h1_send(&url, &Post::ingest(&server, "connection one")).await;
    let parse_probe = parse.entered().await;
    let mut second = h1_send(&url, &Post::preview(&server, "connection two")).await;
    let report_probe = report.entered().await;
    assert_fresh_refused(&url, &Post::ingest(&server, "connection three")).await;

    // `/metrics` is a probe request, so a scrape passes the full regular
    // count and reads it.
    let scrape = SCRAPE.lock().await;
    let (status, scrape_body) = h1_get(&url, "/metrics").await;
    assert_eq!(status, StatusCode::OK);
    let regular = regular_in_progress(&String::from_utf8_lossy(&scrape_body));
    drop(scrape);
    // The recorder is process-wide: other tests' requests only add to it.
    assert!(
        regular >= f64::from(N),
        "regular in progress {regular}, held {N}"
    );

    parse.release();
    report.release();
    for (name, stream) in [
        ("connection one", &mut first),
        ("connection two", &mut second),
    ] {
        let (status, body) = h1_answer(stream).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{name}: {}",
            String::from_utf8_lossy(&body)
        );
    }
    until_holders(&parse_probe, 0, "the first connection's request").await;
    until_holders(&report_probe, 0, "the second connection's request").await;
    drop(first);
    drop(second);

    // The count recovers: N connections at once are all admitted.
    let mut answers = Vec::new();
    for n in 0..N {
        let label = format!("recovered {n}");
        answers.push(h1_send(&url, &Post::ingest(&server, &label)).await);
    }
    for mut stream in answers {
        let (status, body) = h1_answer(&mut stream).await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    }
    task.abort();
}

/// A request refused at the count is answered from its headers alone,
/// before its body is sent, and the listener then hangs up without
/// reading the body (ADR-0054): the refusal is the whole exchange.
#[tokio::test(flavor = "multi_thread")]
async fn refused_request_is_answered_before_its_body() {
    let server = common::setup().await;
    let (url, task) = server
        .spawn_server_with(|http| http.max_concurrent_requests = 1, vec![])
        .await;

    let mut parse = Held::next(&server, BodyWork::IngestParse);
    let held = h1_send(&url, &Post::ingest(&server, "held")).await;
    parse.entered().await;

    let mut refused = h1_open(&url, &Post::ingest(&server, "refused")).await;
    let (status, body) = h1_answer(&mut refused).await;
    assert_refused(status, &body);
    let closed = tokio::time::timeout(DEADLINE, async {
        let mut buf = [0_u8; 64];
        loop {
            match refused.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    })
    .await;
    assert!(
        closed.is_ok(),
        "the listener kept the refused connection open for its body"
    );

    parse.release();
    drop(held);
    task.abort();
}

/// A key without the route's permission is refused on its request head,
/// so a body it never finishes holds no count (ADR-0059): with the limit
/// at 1, a `POST /api/v1/query` from the ingest key, whose chunked body is
/// begun and never ended, gets its 403 while the body is still open, and a
/// request on a new connection is then admitted.
///
/// The count is released before the refusal's head is written, so reading
/// the 403 orders the second request after the release.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_unfinished_body_holds_no_count() {
    let server = common::setup().await;
    let (url, task) = server
        .spawn_server_with(|http| http.max_concurrent_requests = 1, vec![])
        .await;

    let host = url.strip_prefix("https://").unwrap();
    let mut stalled = tls(&url, b"http/1.1").await;
    stalled
        .write_all(
            format!(
                "POST /api/v1/query HTTP/1.1\r\nhost: {host}\r\n\
                 authorization: Bearer {token}\r\ncontent-type: application/json\r\n\
                 transfer-encoding: chunked\r\n\r\n10\r\n{{\"query\"",
                token = server.ingest_token,
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    stalled.flush().await.unwrap();

    // The body is still open: the client writes nothing more and never
    // half-closes the connection.
    let (status, body) = h1_answer(&mut stalled).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let body: serde_json::Value = serde_json::from_slice(&body).expect("a JSON refusal");
    assert_eq!(body["error"]["code"], "forbidden", "{body}");

    assert_fresh_admitted(&url, &Post::ingest(&server, "after the refusal")).await;
    drop(stalled);
    task.abort();
}

/// `trawl_http_requests_in_progress{allowance="regular"}` in a scrape.
fn regular_in_progress(scrape: &str) -> f64 {
    scrape
        .lines()
        .find_map(|line| {
            line.strip_prefix("trawl_http_requests_in_progress{allowance=\"regular\"} ")
        })
        .unwrap_or_else(|| panic!("no regular in-progress gauge in:\n{scrape}"))
        .trim()
        .parse()
        .unwrap()
}

/// One POST, sent the same way over any of the clients here.
#[derive(Clone)]
pub struct Post {
    pub path: &'static str,
    pub token: String,
    pub gzip: bool,
    pub body: Vec<u8>,
}

impl Post {
    /// A real ingest request: gzip ndjson, as the fixture's ingest key.
    pub fn ingest(server: &TestServer, label: &str) -> Self {
        use std::io::Write as _;
        let event = serde_json::json!({
            "service": "request-limit",
            "message": label,
        });
        let mut ndjson = serde_json::to_vec(&event).unwrap();
        ndjson.push(b'\n');
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&ndjson).unwrap();
        Self {
            path: "/api/v1/ingest",
            token: server.ingest_token.clone(),
            gzip: true,
            body: encoder.finish().unwrap(),
        }
    }

    /// An ingest preview of one event, as the fixture's admin key.
    pub fn preview(server: &TestServer, label: &str) -> Self {
        let event = serde_json::json!({
            "service": "request-limit",
            "message": label,
        });
        let mut body = serde_json::to_vec(&event).unwrap();
        body.push(b'\n');
        Self {
            path: "/api/v1/ingest/preview",
            token: server.admin_token.clone(),
            gzip: false,
            body,
        }
    }

    /// The request head for this post, `content-length` included.
    fn head(&self, url: &str) -> String {
        let host = url.strip_prefix("https://").unwrap();
        let encoding = if self.gzip {
            "content-encoding: gzip\r\n"
        } else {
            ""
        };
        format!(
            "POST {path} HTTP/1.1\r\nhost: {host}\r\nauthorization: Bearer {token}\r\n\
             content-type: application/x-ndjson\r\n{encoding}content-length: {length}\r\n\r\n",
            path = self.path,
            token = self.token,
            length = self.body.len(),
        )
    }
}

/// A TLS connection to `url` (`https://host:port`) that trusts the fixture
/// certificate and offers `alpn` alone.
pub async fn tls(url: &str, alpn: &[u8]) -> TlsStream<TcpStream> {
    use rustls::pki_types::pem::PemObject as _;

    let authority = url.strip_prefix("https://").expect("an https:// URL");
    let host = authority.rsplit_once(':').expect("a URL with a port").0;
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
    config.alpn_protocols = vec![alpn.to_vec()];
    let tcp = TcpStream::connect(authority).await.unwrap();
    let stream = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(
            rustls::pki_types::ServerName::try_from(host.to_owned()).unwrap(),
            tcp,
        )
        .await
        .expect("TLS handshake with the test server");
    assert_eq!(
        stream.get_ref().1.alpn_protocol(),
        Some(alpn),
        "the server negotiated the protocol offered"
    );
    stream
}

/// Write `post` whole as one HTTP/1.1 request on a connection of its own,
/// and hand back the open connection without reading the answer.
pub async fn h1_send(url: &str, post: &Post) -> TlsStream<TcpStream> {
    let mut stream = h1_open(url, post).await;
    stream.write_all(&post.body).await.unwrap();
    stream.flush().await.unwrap();
    stream
}

/// Write `post`'s head alone, `content-length` included, on a connection
/// of its own, and hand back the open connection: the server now waits
/// for the body, or answers without it.
pub async fn h1_open(url: &str, post: &Post) -> TlsStream<TcpStream> {
    let mut stream = tls(url, b"http/1.1").await;
    stream.write_all(post.head(url).as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    stream
}

/// A GET of `path` on a connection of its own, answered whole.
pub async fn h1_get(url: &str, path: &str) -> (StatusCode, Bytes) {
    let mut stream = tls(url, b"http/1.1").await;
    let host = url.strip_prefix("https://").unwrap();
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\nhost: {host}\r\n\r\n").as_bytes())
        .await
        .unwrap();
    h1_answer(&mut stream).await
}

/// The HTTP/1.1 response on `stream`: its status and whole body, within
/// the deadline. The body must be delimited by `content-length`, as every
/// answer here is.
pub async fn h1_answer(stream: &mut TlsStream<TcpStream>) -> (StatusCode, Bytes) {
    tokio::time::timeout(DEADLINE, async {
        let mut raw = Vec::new();
        let head_end = loop {
            if let Some(at) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                break at + 4;
            }
            let mut buf = [0_u8; 4096];
            let n = stream.read(&mut buf).await.expect("read the response head");
            assert!(n > 0, "the connection closed before a response head");
            raw.extend_from_slice(&buf[..n]);
        };
        let head = String::from_utf8_lossy(&raw[..head_end]).to_ascii_lowercase();
        let status: u16 = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse().ok())
            .unwrap_or_else(|| panic!("a status line in {head:?}"));
        assert!(
            !head.contains("transfer-encoding: chunked"),
            "this reader needs a content-length: {head}"
        );
        let length: usize = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .map_or(0, |value| value.trim().parse().expect("a content-length"));
        let mut body = raw[head_end..].to_vec();
        while body.len() < length {
            let mut buf = [0_u8; 4096];
            let n = stream.read(&mut buf).await.expect("read the body");
            assert!(n > 0, "the connection closed inside the body");
            body.extend_from_slice(&buf[..n]);
        }
        body.truncate(length);
        (StatusCode::from_u16(status).unwrap(), Bytes::from(body))
    })
    .await
    .expect("the request was answered before the deadline")
}

/// Assert an answer is the regular refusal.
pub fn assert_refused(status: StatusCode, body: &[u8]) {
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let body: serde_json::Value = serde_json::from_slice(body).expect("a JSON refusal");
    assert_eq!(body["error"]["code"], "request_limit_reached", "{body}");
    assert_eq!(body["error"]["message"], REGULAR_MESSAGE, "{body}");
}

/// `post` on a fresh reqwest connection, answered whole.
pub async fn fresh_post(url: &str, post: &Post) -> (StatusCode, Bytes) {
    let client = common::harness_client_builder()
        .timeout(DEADLINE)
        .build()
        .unwrap();
    let mut request = client
        .post(format!("{url}{}", post.path))
        .bearer_auth(&post.token)
        .header("content-type", "application/x-ndjson");
    if post.gzip {
        request = request.header("content-encoding", "gzip");
    }
    let response = request.body(post.body.clone()).send().await.unwrap();
    let status = StatusCode::from_u16(response.status().as_u16()).unwrap();
    (status, response.bytes().await.unwrap())
}

/// Assert `post` on a fresh connection is the regular refusal.
pub async fn assert_fresh_refused(url: &str, post: &Post) {
    let (status, body) = fresh_post(url, post).await;
    assert_refused(status, &body);
}

/// Assert `post` on a fresh connection is admitted and answered 200.
pub async fn assert_fresh_admitted(url: &str, post: &Post) {
    let (status, body) = fresh_post(url, post).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
}

/// A pending hold on `work` in `server`'s state.
pub struct Held {
    entered: Option<std::sync::mpsc::Receiver<HoldProbe>>,
    release: std::sync::mpsc::Sender<()>,
}

impl Held {
    /// Hold the next `work` the server runs.
    pub fn next(server: &TestServer, work: BodyWork) -> Self {
        let (entered, release) = server.state.ingest.body_work_holds.hold_next(work);
        Self {
            entered: Some(entered),
            release,
        }
    }

    /// Wait until the held work is entered and answer its probe.
    pub async fn entered(&mut self) -> HoldProbe {
        let entered = self.entered.take().expect("entered once");
        tokio::task::spawn_blocking(move || entered.recv_timeout(DEADLINE))
            .await
            .unwrap()
            .expect("the held work was entered before the deadline")
    }

    /// Let the held work go on.
    pub fn release(self) {
        let _ = self.release.send(());
    }
}

/// Wait until the count `probe` watches has exactly `holders` owners.
///
/// The count is shared through `Arc`s with no event of its own, so this
/// polls it. The deadline is a failure deadline: the poll orders nothing.
pub async fn until_holders(probe: &HoldProbe, holders: usize, what: &str) {
    let reached = tokio::time::timeout(DEADLINE, async {
        while probe.holders() != holders {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(
        reached.is_ok(),
        "{what}: the count kept {} holders, never {holders}",
        probe.holders()
    );
}
