// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The captured-result methods (ADR-0057) on the wire: the request each
//! sends and the answer each decodes.
//!
//! Each test serves one connection from a rustls listener on `127.0.0.1`
//! whose self-signed certificate the client pins as its CA, as
//! `upload_cut_off.rs` does. The listener records the request it read and
//! writes one canned success body, serialised from a trawl-api value.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;
use trawl_api::value::{Column, ColumnType, QueryResult, Value};
use trawl_api::{PaginationMeta, QueryExecution};
use trawl_client::{
    CONTEXT_STATEMENT, CaptureKind, CaptureRequest, CaptureResponse, CaptureWindow, CapturedResult,
    ClientError, ColumnFacts, CompareFacts, CompareRequest, CompareWindow, ContextFacts,
    ContextGroups, ContextRequest, ContextScope, Correlation, CorrelationResponse, FieldSightings,
    FocusRow, HttpClient, RateUnit, ResultCell, ResultColumn, ResultList, ResultPage, ResultShape,
    ResultSource, ResultState, RowRange, ScopeUsed, SightingGroup, SightingsDenominators,
    SightingsFacts, SightingsRequest, StoredAs, TlsTrust,
};

const TOKEN: &str = "flt_resultsroutestesttoken";

/// A result id the client accepts.
const RESULT_ID: &str = "0123456789abcdef0123456789abcdef";

/// Bound on every exchange, so a hang fails the test instead of wedging CI.
const EXCHANGE: Duration = Duration::from_secs(30);

/// What the listener read: the request head and its body.
struct Seen {
    head: String,
    body: Vec<u8>,
}

impl Seen {
    /// The request line: method, target and version.
    fn request_line(&self) -> &str {
        self.head.lines().next().expect("a request line")
    }

    /// The value of header `name`, matched case-insensitively.
    fn header(&self, name: &str) -> Option<&str> {
        self.head
            .lines()
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .find(|(n, _)| n.trim().eq_ignore_ascii_case(name))
            .map(|(_, value)| value.trim())
    }
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

/// A client that pins `pem` and addresses `address`.
fn client_for(address: SocketAddr, pem: Vec<u8>) -> HttpClient {
    HttpClient::with_trust(
        format!("https://{address}"),
        TOKEN,
        &TlsTrust::PinnedCa(pem),
    )
    .expect("client")
}

/// Serve one connection: read the request, answer `200 OK` with `body`.
/// Returns a client that trusts the listener's certificate as its pinned
/// CA, and the serving task, which hands back what it read.
async fn serve_one(body: Vec<u8>) -> (HttpClient, JoinHandle<Seen>) {
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
        let mut tls = acceptor.accept(tcp).await.expect("TLS handshake");
        let (head, mut request_body) = read_head(&mut tls).await;
        let length = content_length(&head);
        let mut buf = vec![0u8; 64 * 1024];
        while request_body.len() < length {
            let n = tls.read(&mut buf).await.expect("body read");
            assert!(n > 0, "the client closed mid-body");
            request_body.extend_from_slice(&buf[..n]);
        }
        let mut response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
             connection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(&body);
        tls.write_all(&response).await.expect("response write");
        tls.shutdown().await.expect("shutdown");
        Seen {
            head,
            body: request_body,
        }
    });
    (client_for(address, pem), served)
}

/// Read up to the end of the request head. Returns the head and the body
/// bytes that arrived with it.
async fn read_head<S: AsyncReadExt + Unpin>(stream: &mut S) -> (String, Vec<u8>) {
    let mut bytes = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&bytes[..end]).into_owned();
            return (head, bytes[end + 4..].to_vec());
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

/// Run `call` against the listener, both bounded by [`EXCHANGE`]. Returns
/// the call's answer and what the listener read.
async fn exchange<T>(
    call: impl std::future::Future<Output = Result<T, ClientError>>,
    served: JoinHandle<Seen>,
) -> (T, Seen) {
    let (result, seen) = tokio::time::timeout(EXCHANGE, async { tokio::join!(call, served) })
        .await
        .expect("the exchange finished in time");
    let seen = seen.expect("the listener ran to completion");
    (result.expect("the call succeeds"), seen)
}

/// Assert the request carried the client's bearer token.
fn assert_bearer(seen: &Seen) {
    assert_eq!(
        seen.header("authorization"),
        Some(format!("Bearer {TOKEN}").as_str()),
        "{}",
        seen.head
    );
}

/// A captured result with two columns of the same name.
fn captured() -> CapturedResult {
    CapturedResult {
        id: RESULT_ID.to_owned(),
        query: "_service=nginx last=1h".to_owned(),
        executed_query: "_service=nginx earliest=\"2026-10-23T09:00:00.000000Z\" \
                         latest=\"2026-10-23T10:00:00.000000Z\" | sort -_time, -_ingested"
            .to_owned(),
        source: ResultSource::Live,
        shape: ResultShape::Events,
        earliest: Some("2026-10-23T09:00:00.000000Z".to_owned()),
        latest: Some("2026-10-23T10:00:00.000000Z".to_owned()),
        created_at: "2026-10-23T10:00:00.250000Z".to_owned(),
        expires_at: Some("2026-10-23T10:15:00.250000Z".to_owned()),
        state: ResultState::Temporary,
        columns: vec![
            ResultColumn {
                name: "a".to_owned(),
                ty: ColumnType::Integer,
                stored_as: StoredAs::Native,
            },
            ResultColumn {
                name: "b".to_owned(),
                ty: ColumnType::String,
                stored_as: StoredAs::Native,
            },
            ResultColumn {
                name: "a".to_owned(),
                ty: ColumnType::Mixed,
                stored_as: StoredAs::WireText,
            },
        ],
        captured_rows: 2,
        total_rows: 2,
        wire_bytes: 42,
        file_bytes: 4096,
        truncated: false,
        column_facts: vec![
            ColumnFacts {
                nulls: 0,
                distinct: 2,
                distinct_capped: false,
                values: None,
                min: None,
                max: None,
            };
            3
        ],
        format_version: 1,
        kind: CaptureKind::Search,
        correlation: None,
    }
}

/// `capture` posts the request as JSON to `/api/v1/results` and decodes
/// the capture response.
#[tokio::test]
async fn capture_posts_the_request() {
    let answer = CaptureResponse {
        result: captured(),
        execution: Some(QueryExecution {
            started_at: "2026-10-23T10:00:00.000000Z".to_owned(),
            duration_ms: 12,
        }),
    };
    let (client, served) = serve_one(serde_json::to_vec(&answer).unwrap()).await;
    let request = CaptureRequest {
        query: "_service=nginx".to_owned(),
        window: Some(CaptureWindow {
            last: Some("1h".to_owned()),
            ..CaptureWindow::default()
        }),
    };
    let (decoded, seen) = exchange(client.capture(&request), served).await;
    assert_eq!(seen.request_line(), "POST /api/v1/results HTTP/1.1");
    assert_bearer(&seen);
    assert_eq!(
        serde_json::from_slice::<CaptureRequest>(&seen.body).expect("a capture request"),
        request
    );
    assert_eq!(decoded, answer);
}

/// A correlation response for `correlation`, made by a read of `kind`.
fn correlation_answer(kind: CaptureKind, correlation: Correlation) -> CorrelationResponse {
    CorrelationResponse {
        result: CapturedResult {
            kind,
            correlation: Some(correlation),
            ..captured()
        },
        execution: None,
    }
}

/// `context` posts its request to its route and decodes the correlation
/// response.
#[tokio::test]
async fn context_posts_to_its_route() {
    let context = ContextRequest {
        result_id: RESULT_ID.to_owned(),
        row: 3,
        scope: ContextScope::Env,
        seconds: 30,
        before: 10,
        after: 10,
    };
    let answer = correlation_answer(
        CaptureKind::Context,
        Correlation::Context(ContextFacts {
            focus: FocusRow {
                result_id: RESULT_ID.to_owned(),
                row: 3,
                time: "2026-10-23T09:59:00.000000Z".to_owned(),
                env: Some("prod".to_owned()),
                host: Some("web-1".to_owned()),
            },
            scope_used: ScopeUsed {
                env: Some("prod".to_owned()),
                host: None,
                seconds: 30,
            },
            groups: ContextGroups {
                earlier: RowRange { start: 0, end: 1 },
                same_time: RowRange { start: 1, end: 2 },
                later: RowRange { start: 2, end: 2 },
            },
            statement: CONTEXT_STATEMENT.to_owned(),
        }),
    );
    let (client, served) = serve_one(serde_json::to_vec(&answer).unwrap()).await;
    let (decoded, seen) = exchange(client.context(&context), served).await;
    assert_eq!(seen.request_line(), "POST /api/v1/results/context HTTP/1.1");
    assert_bearer(&seen);
    assert_eq!(
        serde_json::from_slice::<ContextRequest>(&seen.body).expect("a context request"),
        context
    );
    assert_eq!(decoded, answer);
}

/// `sightings` posts its request to its route and decodes the
/// correlation response.
#[tokio::test]
async fn sightings_posts_to_its_route() {
    let sightings = SightingsRequest {
        literal: "10.0.4.17".to_owned(),
        window: None,
        fields: vec!["client_ip".to_owned()],
    };
    let answer = correlation_answer(
        CaptureKind::Sightings,
        Correlation::Sightings(SightingsFacts {
            literal: "10.0.4.17".to_owned(),
            matches_total: 2,
            captured: 2,
            attribution_fields: vec!["client_ip".to_owned()],
            groups: vec![SightingGroup {
                env: Some("prod".to_owned()),
                service: Some("auth".to_owned()),
                count: 2,
                first_seen: "2026-10-23T09:00:00.000000Z".to_owned(),
                last_seen: "2026-10-23T09:30:00.000000Z".to_owned(),
            }],
            fields: vec![FieldSightings {
                field: "client_ip".to_owned(),
                rows: 2,
            }],
            raw_only_rows: 0,
            denominators: SightingsDenominators {
                field_counts_over: 2,
                service_counts_over: 2,
            },
            follow_ups: vec![],
        }),
    );
    let (client, served) = serve_one(serde_json::to_vec(&answer).unwrap()).await;
    let (decoded, seen) = exchange(client.sightings(&sightings), served).await;
    assert_eq!(
        seen.request_line(),
        "POST /api/v1/results/sightings HTTP/1.1"
    );
    assert_bearer(&seen);
    assert_eq!(
        serde_json::from_slice::<SightingsRequest>(&seen.body).expect("a sightings request"),
        sightings
    );
    assert_eq!(decoded, answer);
}

/// `compare` posts its request to its route and decodes the correlation
/// response.
#[tokio::test]
async fn compare_posts_to_its_route() {
    let compare = CompareRequest {
        filter: "service=auth".to_owned(),
        foreground: CaptureWindow {
            last: Some("1h".to_owned()),
            ..CaptureWindow::default()
        },
        baseline: None,
        group_by: vec!["host".to_owned()],
        min_count: 1,
    };
    let answer = correlation_answer(
        CaptureKind::Compare,
        Correlation::Compare(CompareFacts {
            foreground: CompareWindow {
                start: "2026-10-23T09:00:00.000000Z".to_owned(),
                end: "2026-10-23T10:00:00.000000Z".to_owned(),
                duration_ms: 3_600_000,
            },
            baseline: CompareWindow {
                start: "2026-10-23T08:00:00.000000Z".to_owned(),
                end: "2026-10-23T09:00:00.000000Z".to_owned(),
                duration_ms: 3_600_000,
            },
            group_by: vec!["host".to_owned()],
            min_count: 1,
            rate_unit: RateUnit::PerMinute,
            rows_ranked: 2,
            warnings: vec![],
        }),
    );
    let (client, served) = serve_one(serde_json::to_vec(&answer).unwrap()).await;
    let (decoded, seen) = exchange(client.compare(&compare), served).await;
    assert_eq!(seen.request_line(), "POST /api/v1/results/compare HTTP/1.1");
    assert_bearer(&seen);
    assert_eq!(
        serde_json::from_slice::<CompareRequest>(&seen.body).expect("a compare request"),
        compare
    );
    assert_eq!(decoded, answer);
}

/// `list_results` reads `/api/v1/results` and decodes the list.
#[tokio::test]
async fn list_results_reads_the_list() {
    let answer = ResultList {
        results: vec![captured()],
    };
    let (client, served) = serve_one(serde_json::to_vec(&answer).unwrap()).await;
    let (decoded, seen) = exchange(client.list_results(), served).await;
    assert_eq!(seen.request_line(), "GET /api/v1/results HTTP/1.1");
    assert_bearer(&seen);
    assert!(seen.body.is_empty());
    assert_eq!(decoded, answer);
}

fn page() -> ResultPage {
    ResultPage {
        result: captured(),
        positions: vec![0, 1, 2],
        page: QueryResult {
            columns: vec![
                Column::new("a", ColumnType::Integer),
                Column::new("b", ColumnType::String),
                Column::new("a", ColumnType::Mixed),
            ],
            rows: vec![vec![
                Value::Integer(200),
                Value::String("GET /".to_owned()),
                Value::Boolean(true),
            ]],
        },
        pagination: PaginationMeta {
            limit: 50,
            offset: 10,
            returned: 1,
            total: 2,
        },
    }
}

/// `result_page` sends the window and the comma-joined projection as query
/// parameters, never in the path, and decodes the page.
#[tokio::test]
async fn result_page_sends_the_projection_as_one_parameter() {
    let answer = page();
    let (client, served) = serve_one(serde_json::to_vec(&answer).unwrap()).await;
    let (decoded, seen) = exchange(
        client.result_page(RESULT_ID, 10, 50, Some(&["a", "b"])),
        served,
    )
    .await;
    assert_eq!(
        seen.request_line(),
        format!("GET /api/v1/results/{RESULT_ID}?offset=10&limit=50&columns=a%2Cb HTTP/1.1")
    );
    assert_bearer(&seen);
    assert_eq!(decoded, answer);
}

/// Without a projection, no `columns` parameter is sent; a name with
/// reserved characters is percent-encoded.
#[tokio::test]
async fn result_page_without_a_projection_sends_no_columns() {
    let (client, served) = serve_one(serde_json::to_vec(&page()).unwrap()).await;
    let (_, seen) = exchange(client.result_page(RESULT_ID, 0, 200, None), served).await;
    assert_eq!(
        seen.request_line(),
        format!("GET /api/v1/results/{RESULT_ID}?offset=0&limit=200 HTTP/1.1")
    );

    let (client, served) = serve_one(serde_json::to_vec(&page()).unwrap()).await;
    let (_, seen) = exchange(
        client.result_page(RESULT_ID, 0, 1, Some(&["a&b=c/d"])),
        served,
    )
    .await;
    assert_eq!(
        seen.request_line(),
        format!("GET /api/v1/results/{RESULT_ID}?offset=0&limit=1&columns=a%26b%3Dc%2Fd HTTP/1.1")
    );
}

fn cell() -> ResultCell {
    ResultCell {
        ty: ColumnType::String,
        offset: 5,
        length: 3,
        total_bytes: 12,
        next_offset: Some(8),
        text: "lo,".to_owned(),
    }
}

/// `result_cell` addresses the cell in the path and sends `offset` and
/// `length` only when given.
#[tokio::test]
async fn result_cell_sends_the_range_only_when_given() {
    let answer = cell();
    let (client, served) = serve_one(serde_json::to_vec(&answer).unwrap()).await;
    let (decoded, seen) = exchange(
        client.result_cell(RESULT_ID, 7, 2, Some(5), Some(100)),
        served,
    )
    .await;
    assert_eq!(
        seen.request_line(),
        format!("GET /api/v1/results/{RESULT_ID}/cells/7/2?offset=5&length=100 HTTP/1.1")
    );
    assert_bearer(&seen);
    assert_eq!(decoded, answer);

    let (client, served) = serve_one(serde_json::to_vec(&answer).unwrap()).await;
    let (_, seen) = exchange(client.result_cell(RESULT_ID, 0, 0, None, None), served).await;
    assert_eq!(
        seen.request_line(),
        format!("GET /api/v1/results/{RESULT_ID}/cells/0/0 HTTP/1.1")
    );

    let (client, served) = serve_one(serde_json::to_vec(&answer).unwrap()).await;
    let (_, seen) = exchange(client.result_cell(RESULT_ID, 0, 0, None, Some(64)), served).await;
    assert_eq!(
        seen.request_line(),
        format!("GET /api/v1/results/{RESULT_ID}/cells/0/0?length=64 HTTP/1.1")
    );
}

/// A malformed id is refused before any connection: the listener sees
/// nothing.
#[tokio::test]
async fn a_malformed_id_is_refused_before_any_connection() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (pem, _, _) = self_signed();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let client = client_for(listener.local_addr().expect("local addr"), pem);
    let malformed = [
        "ABC",
        "0123456789abcdef0123456789abcde",
        "0123456789ABCDEF0123456789ABCDEF",
        "0123456789abcdef0123456789abcdef0",
        "0123456789abcdef/../../../saved/1",
        "",
    ];
    for id in malformed {
        let page = client.result_page(id, 0, 10, None).await;
        assert!(
            matches!(&page, Err(ClientError::InvalidUrl(_))),
            "{id:?} gave {page:?}"
        );
        let cell = client.result_cell(id, 0, 0, None, None).await;
        assert!(
            matches!(&cell, Err(ClientError::InvalidUrl(_))),
            "{id:?} gave {cell:?}"
        );
        assert_eq!(cell.unwrap_err().request_id(), None);
    }
    // A connect would already sit in the accept queue: the kernel completes
    // the handshake without the listener's help.
    let accepted = tokio::time::timeout(Duration::from_millis(200), listener.accept()).await;
    assert!(accepted.is_err(), "the listener saw a connection");
}
