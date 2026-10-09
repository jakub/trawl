// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Connections and HTTP/2 streams through the production TLS listener,
//! and the clients the body-work tests share.
//!
//! The clients are explicit about connections: an HTTP/1.1 request written
//! by hand over a TLS stream of its own, and an `h2` connection whose
//! streams the test opens and resets one by one. reqwest is used only
//! where any fresh connection will do.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::http::{self, StatusCode};
use tokio::io::AsyncWriteExt as _;
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use trawl_server::transport::request_limit::body_work::{BodyWork, HoldProbe};

use crate::common::{self, TestServer};
use crate::support::{DEADLINE, REGULAR_MESSAGE};

/// AC14: through the production TLS listener, separate connections share
/// one count, and N+1 concurrent HTTP/2 streams on one connection at
/// limit N meet exactly one refusal. The count recovers on the same
/// connection afterwards.
///
/// Each held request is ingest or preview work held at the state's seam,
/// which reports its entry, so a request is known to hold the count
/// before the next one is sent.
#[tokio::test(flavor = "multi_thread")]
async fn request_count_spans_connections_and_h2_streams() {
    const N: u8 = 2;
    let server = common::setup().await;
    let (url, task) = server
        .spawn_server_with(|http| http.max_concurrent_requests = usize::from(N), vec![])
        .await;

    // Separate connections: one HTTP/1.1 and one HTTP/2 connection each
    // hold one request, and a third connection is refused.
    let mut parse = Held::next(&server, BodyWork::IngestParse);
    let mut report = Held::next(&server, BodyWork::Preview);
    let first = h1_send(&url, &Post::ingest(&server, "connection one")).await;
    let parse_probe = parse.entered().await;
    let second = h2_connect(&url).await;
    let (preview, _preview_stream) =
        h2_send(&second, &url, &Post::preview(&server, "connection two")).await;
    let report_probe = report.entered().await;
    assert_fresh_refused(&url, &Post::ingest(&server, "connection three")).await;

    parse.release();
    report.release();
    let (status, body) = h2_answer(preview).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    until_holders(&parse_probe, 0, "the HTTP/1.1 connection's request").await;
    until_holders(&report_probe, 0, "the HTTP/2 connection's request").await;
    drop(first);

    // One connection: N streams held, the N+1th refused.
    let connection = h2_connect(&url).await;
    let mut parse = Held::next(&server, BodyWork::IngestParse);
    let mut report = Held::next(&server, BodyWork::Preview);
    let (ingest, _ingest_stream) =
        h2_send(&connection, &url, &Post::ingest(&server, "stream one")).await;
    let parse_probe = parse.entered().await;
    let (preview, _preview_stream) =
        h2_send(&connection, &url, &Post::preview(&server, "stream two")).await;
    let report_probe = report.entered().await;
    let (refused, _refused_stream) =
        h2_send(&connection, &url, &Post::ingest(&server, "stream three")).await;
    let (status, body) = h2_answer(refused).await;
    assert_h2_refused(status, &body);

    // `/metrics` is a control request, so a scrape on the same connection
    // passes the full regular count and reads it.
    let (status, scrape) = h2_answer(h2_get(&connection, &url, "/metrics").await).await;
    assert_eq!(status, StatusCode::OK);
    let regular = regular_in_progress(&String::from_utf8_lossy(&scrape));
    // The recorder is process-wide: other tests' requests only add to it.
    assert!(
        regular >= f64::from(N),
        "regular in progress {regular}, held {N}"
    );

    parse.release();
    report.release();
    for (name, answer) in [("stream one", ingest), ("stream two", preview)] {
        let (status, body) = h2_answer(answer).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{name}: {}",
            String::from_utf8_lossy(&body)
        );
    }
    until_holders(&parse_probe, 0, "stream one").await;
    until_holders(&report_probe, 0, "stream two").await;

    // The count recovers on the same connection: N streams at once are
    // all admitted.
    let mut answers = Vec::new();
    for n in 0..N {
        let label = format!("recovered {n}");
        answers.push(h2_send(&connection, &url, &Post::ingest(&server, &label)).await);
    }
    for (answer, _stream) in answers {
        let (status, body) = h2_answer(answer).await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    }
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
    let mut stream = tls(url, b"http/1.1").await;
    let host = url.strip_prefix("https://").unwrap();
    let encoding = if post.gzip {
        "content-encoding: gzip\r\n"
    } else {
        ""
    };
    let head = format!(
        "POST {path} HTTP/1.1\r\nhost: {host}\r\nauthorization: Bearer {token}\r\n\
         content-type: application/x-ndjson\r\n{encoding}content-length: {length}\r\n\r\n",
        path = post.path,
        token = post.token,
        length = post.body.len(),
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(&post.body).await.unwrap();
    stream.flush().await.unwrap();
    stream
}

/// One HTTP/2 connection, its driver spawned.
pub async fn h2_connect(url: &str) -> h2::client::SendRequest<Bytes> {
    let stream = tls(url, b"h2").await;
    let (sender, connection) = h2::client::handshake(stream)
        .await
        .expect("HTTP/2 handshake with the test server");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    sender
}

/// Open a stream on `sender`'s connection carrying `post` whole. The stream
/// stays open for a reset until its answer is read.
pub async fn h2_send(
    sender: &h2::client::SendRequest<Bytes>,
    url: &str,
    post: &Post,
) -> (h2::client::ResponseFuture, h2::SendStream<Bytes>) {
    let mut request = http::Request::builder()
        .method("POST")
        .uri(format!("{url}{}", post.path))
        .header("authorization", format!("Bearer {}", post.token))
        .header("content-type", "application/x-ndjson");
    if post.gzip {
        request = request.header("content-encoding", "gzip");
    }
    let mut ready = sender
        .clone()
        .ready()
        .await
        .expect("the connection takes another stream");
    let (response, mut stream) = ready
        .send_request(request.body(()).unwrap(), false)
        .expect("open a stream");
    stream
        .send_data(Bytes::from(post.body.clone()), true)
        .expect("send the request body");
    (response, stream)
}

/// Open a stream on `sender`'s connection carrying a GET of `path`.
pub async fn h2_get(
    sender: &h2::client::SendRequest<Bytes>,
    url: &str,
    path: &str,
) -> h2::client::ResponseFuture {
    let request = http::Request::builder()
        .method("GET")
        .uri(format!("{url}{path}"))
        .body(())
        .unwrap();
    let mut ready = sender
        .clone()
        .ready()
        .await
        .expect("the connection takes another stream");
    ready.send_request(request, true).expect("open a stream").0
}

/// A stream's status and whole body, within the deadline.
pub async fn h2_answer(response: h2::client::ResponseFuture) -> (StatusCode, Bytes) {
    tokio::time::timeout(DEADLINE, async {
        let response = response.await.expect("the stream answered");
        let status = response.status();
        let mut body = response.into_body();
        let mut bytes = Vec::new();
        while let Some(chunk) = body.data().await {
            let chunk = chunk.expect("the answer's body");
            body.flow_control()
                .release_capacity(chunk.len())
                .expect("release flow-control capacity");
            bytes.extend_from_slice(&chunk);
        }
        (status, Bytes::from(bytes))
    })
    .await
    .expect("the stream answered before the deadline")
}

/// Assert an HTTP/2 answer is the regular refusal.
pub fn assert_h2_refused(status: StatusCode, body: &[u8]) {
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
    assert_h2_refused(status, &body);
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
