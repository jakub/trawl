// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The investigation methods (ADR-0059) on the wire: the method, path and
//! query string each sends, its body, and how each reads the answer's
//! status and body.
//!
//! Each test serves one connection from a rustls listener on `127.0.0.1`
//! whose self-signed certificate the client pins as its CA, as
//! `results_routes.rs` does. The listener records the request it read and
//! writes one canned answer with the status the test names.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;
use trawl_client::{
    Activity, Actor, AppendEntry, AppendResponse, ClientError, CreateInvestigation, EntryPayload,
    ErrorCode, ErrorEnvelope, ErrorResponse, FindingStatus, FindingStatusUpdate, HttpClient,
    Investigation, InvestigationId, InvestigationList, InvestigationRead, InvestigationStatus,
    InvestigationSummary, LogEntry, PatchInvestigation, RetentionExtension, SequenceConflict,
    TlsTrust,
};

const TOKEN: &str = "flt_investigationroutestesttoken";

/// An investigation id the client accepts.
const ID: &str = "0123456789abcdef0123456789abcdef";

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

    /// The body decoded as JSON.
    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("a JSON body")
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

/// Serve one connection: read the request, answer `status` (a status line
/// such as `201 Created`) with `body` and the extra `headers` lines.
/// Returns a client that trusts the listener's certificate as its pinned
/// CA, and the serving task, which hands back what it read.
async fn serve_one(
    status: &'static str,
    headers: &'static str,
    body: Vec<u8>,
) -> (HttpClient, JoinHandle<Seen>) {
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
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
             {headers}connection: close\r\n\r\n",
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

/// [`serve_one`] answering `200 OK` with `value` as JSON.
async fn serve_ok(value: &impl serde::Serialize) -> (HttpClient, JoinHandle<Seen>) {
    serve_one("200 OK", "", serde_json::to_vec(value).unwrap()).await
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
/// the call's answer, success or not, and what the listener read.
async fn exchange<T>(
    call: impl std::future::Future<Output = Result<T, ClientError>>,
    served: JoinHandle<Seen>,
) -> (Result<T, ClientError>, Seen) {
    let (result, seen) = tokio::time::timeout(EXCHANGE, async { tokio::join!(call, served) })
        .await
        .expect("the exchange finished in time");
    let seen = seen.expect("the listener ran to completion");
    assert_eq!(
        seen.header("authorization"),
        Some(format!("Bearer {TOKEN}").as_str()),
        "{}",
        seen.head
    );
    (result, seen)
}

fn agent() -> Actor {
    Actor {
        prefix: "AbCd_-12".to_owned(),
        name: "agent".to_owned(),
    }
}

fn investigation() -> Investigation {
    Investigation {
        id: InvestigationId::parse(ID).unwrap(),
        title: "502s on checkout".to_owned(),
        question: "Why does checkout return 502?".to_owned(),
        scope: String::new(),
        status: InvestigationStatus::Open,
        seq: 4,
        created_at: "2026-10-24T09:00:00.000000Z".to_owned(),
        updated_at: "2026-10-24T09:05:00.000000Z".to_owned(),
        retain_until: "2026-10-31T09:05:00.000000Z".to_owned(),
        retain_ceiling: "2026-11-23T09:00:00.000000Z".to_owned(),
        created_by: agent(),
        updated_by: agent(),
    }
}

fn note(seq: u64, text: &str) -> LogEntry {
    LogEntry {
        seq,
        updated_seq: seq,
        actor: agent(),
        at: "2026-10-24T09:05:00.000000Z".to_owned(),
        payload: EntryPayload::Note {
            text: text.to_owned(),
        },
    }
}

/// `list_investigations` reads `/api/v1/investigations` and decodes it.
#[tokio::test]
async fn list_reads_the_collection() {
    let answer = InvestigationList {
        investigations: vec![InvestigationSummary {
            id: investigation().id,
            title: "t".to_owned(),
            status: InvestigationStatus::Open,
            seq: 4,
            created_at: "2026-10-24T09:00:00.000000Z".to_owned(),
            updated_at: "2026-10-24T09:05:00.000000Z".to_owned(),
            created_by: agent(),
            updated_by: agent(),
        }],
    };
    let (client, served) = serve_ok(&answer).await;
    let (decoded, seen) = exchange(client.list_investigations(), served).await;
    assert_eq!(seen.request_line(), "GET /api/v1/investigations HTTP/1.1");
    assert!(seen.body.is_empty());
    assert_eq!(decoded.unwrap(), answer);
}

/// `create_investigation` puts the fields at the id's path and reads 201
/// as created and 200 as the existing investigation.
#[tokio::test]
async fn create_puts_the_fields_and_reads_created_from_the_status() {
    let request = CreateInvestigation {
        title: "502s on checkout".to_owned(),
        question: "Why does checkout return 502?".to_owned(),
        scope: "checkout in prod".to_owned(),
    };
    for (status, created) in [("201 Created", true), ("200 OK", false)] {
        let body = serde_json::to_vec(&investigation()).unwrap();
        let (client, served) = serve_one(status, "", body).await;
        let (decoded, seen) = exchange(client.create_investigation(ID, &request), served).await;
        assert_eq!(
            seen.request_line(),
            format!("PUT /api/v1/investigations/{ID} HTTP/1.1")
        );
        assert_eq!(
            serde_json::from_slice::<CreateInvestigation>(&seen.body).unwrap(),
            request
        );
        assert_eq!(decoded.unwrap(), (created, investigation()), "{status}");
    }
}

/// `read_investigation` always sends `since` and sends `limit` only when
/// given.
#[tokio::test]
async fn read_sends_since_and_the_limit_only_when_given() {
    let answer = InvestigationRead {
        investigation: investigation(),
        entries: vec![note(4, "n")],
        through_seq: 4,
        more: true,
    };
    let (client, served) = serve_ok(&answer).await;
    let (decoded, seen) = exchange(client.read_investigation(ID, 3, Some(10)), served).await;
    assert_eq!(
        seen.request_line(),
        format!("GET /api/v1/investigations/{ID}?since=3&limit=10 HTTP/1.1")
    );
    assert_eq!(decoded.unwrap(), answer);

    let (client, served) = serve_ok(&answer).await;
    let (_, seen) = exchange(client.read_investigation(ID, 0, None), served).await;
    assert_eq!(
        seen.request_line(),
        format!("GET /api/v1/investigations/{ID}?since=0 HTTP/1.1")
    );
}

/// `append_entry` posts the tagged entry, `seen_seq` included, and decodes
/// the entry written with its activity.
#[tokio::test]
async fn append_posts_the_tagged_entry_and_reads_the_activity() {
    let answer = AppendResponse {
        entry: note(6, "mine"),
        activity: Some(Activity {
            entries: vec![note(5, "theirs")],
            through_seq: 5,
            more: false,
        }),
    };
    let entry = AppendEntry::Note {
        text: "mine".to_owned(),
        seen_seq: Some(4),
    };
    let body = serde_json::to_vec(&answer).unwrap();
    let (client, served) = serve_one("201 Created", "", body).await;
    let (decoded, seen) = exchange(client.append_entry(ID, &entry), served).await;
    assert_eq!(
        seen.request_line(),
        format!("POST /api/v1/investigations/{ID}/entries HTTP/1.1")
    );
    assert_eq!(
        seen.json(),
        serde_json::json!({"kind": "note", "text": "mine", "seen_seq": 4})
    );
    assert_eq!(decoded.unwrap(), answer);
}

/// `patch_investigation` patches the investigation with the one field and
/// the expected sequence.
#[tokio::test]
async fn patch_sends_the_expected_sequence_and_one_field() {
    let request = PatchInvestigation {
        expected_seq: 4,
        title: None,
        question: None,
        status: Some(InvestigationStatus::Closed),
    };
    let (client, served) = serve_ok(&investigation()).await;
    let (decoded, seen) = exchange(client.patch_investigation(ID, &request), served).await;
    assert_eq!(
        seen.request_line(),
        format!("PATCH /api/v1/investigations/{ID} HTTP/1.1")
    );
    assert_eq!(
        seen.json(),
        serde_json::json!({"expected_seq": 4, "status": "closed"})
    );
    assert_eq!(decoded.unwrap(), investigation());
}

/// `set_finding_status` addresses the finding by its sequence in the path.
#[tokio::test]
async fn finding_status_addresses_the_finding_by_its_sequence() {
    let answer = AppendResponse {
        entry: LogEntry {
            payload: EntryPayload::FindingStatus {
                finding_seq: 5,
                status: FindingStatus::Refuted,
            },
            ..note(7, "")
        },
        activity: None,
    };
    let request = FindingStatusUpdate {
        expected_seq: 6,
        status: FindingStatus::Refuted,
    };
    let (client, served) = serve_ok(&answer).await;
    let (decoded, seen) = exchange(client.set_finding_status(ID, 5, &request), served).await;
    assert_eq!(
        seen.request_line(),
        format!("PUT /api/v1/investigations/{ID}/findings/5/status HTTP/1.1")
    );
    assert_eq!(
        seen.json(),
        serde_json::json!({"expected_seq": 6, "status": "refuted"})
    );
    assert_eq!(decoded.unwrap(), answer);
}

/// `extend_retention` puts the deadline at the retention path.
#[tokio::test]
async fn extend_puts_the_deadline() {
    let request = RetentionExtension {
        expected_seq: 4,
        retain_until: "2026-12-01T00:00:00Z".to_owned(),
    };
    let (client, served) = serve_ok(&investigation()).await;
    let (decoded, seen) = exchange(client.extend_retention(ID, &request), served).await;
    assert_eq!(
        seen.request_line(),
        format!("PUT /api/v1/investigations/{ID}/retention HTTP/1.1")
    );
    assert_eq!(
        serde_json::from_slice::<RetentionExtension>(&seen.body).unwrap(),
        request
    );
    assert_eq!(decoded.unwrap(), investigation());
}

/// `delete_investigation` sends DELETE and takes 204 with no body as done.
#[tokio::test]
async fn delete_takes_no_content_as_done() {
    let (client, served) = serve_one("204 No Content", "", Vec::new()).await;
    let (decoded, seen) = exchange(client.delete_investigation(ID), served).await;
    assert_eq!(
        seen.request_line(),
        format!("DELETE /api/v1/investigations/{ID} HTTP/1.1")
    );
    decoded.unwrap();
}

/// A 409 `sequence_conflict` decodes with its `conflict` member, its
/// request id and no retry wait; a 409 on the delete route reads the same
/// way.
#[tokio::test]
async fn a_sequence_conflict_carries_expected_and_current() {
    let envelope = ErrorResponse {
        error: ErrorEnvelope {
            conflict: Some(SequenceConflict {
                expected: 4,
                current: 6,
            }),
            ..ErrorEnvelope::simple(ErrorCode::SequenceConflict, "the investigation moved")
        },
    };
    let request = PatchInvestigation {
        expected_seq: 4,
        title: None,
        question: None,
        status: Some(InvestigationStatus::Closed),
    };
    let (client, served) = serve_one(
        "409 Conflict",
        "x-request-id: req-409\r\n",
        serde_json::to_vec(&envelope).unwrap(),
    )
    .await;
    let (refused, _) = exchange(client.patch_investigation(ID, &request), served).await;
    let Err(ClientError::Server {
        status,
        error,
        request_id,
        retry_after_secs,
    }) = refused
    else {
        panic!("expected a refusal, got {refused:?}");
    };
    assert_eq!(status, 409);
    assert_eq!(error.code, ErrorCode::SequenceConflict);
    assert_eq!(
        error.conflict,
        Some(SequenceConflict {
            expected: 4,
            current: 6
        })
    );
    assert_eq!(request_id.as_deref(), Some("req-409"));
    assert_eq!(retry_after_secs, None);

    let closed = ErrorResponse {
        error: ErrorEnvelope::simple(ErrorCode::InvestigationClosed, "closed"),
    };
    let (client, served) =
        serve_one("409 Conflict", "", serde_json::to_vec(&closed).unwrap()).await;
    let (refused, _) = exchange(client.delete_investigation(ID), served).await;
    assert!(
        matches!(
            &refused,
            Err(ClientError::Server { status: 409, error, .. })
                if error.code == ErrorCode::InvestigationClosed
        ),
        "{refused:?}"
    );
}

/// A malformed id is refused by every method before any connection: the
/// listener sees nothing.
#[tokio::test]
async fn a_malformed_id_is_refused_before_any_connection() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (pem, _, _) = self_signed();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let client = client_for(listener.local_addr().expect("local addr"), pem);
    let create = CreateInvestigation {
        title: "t".to_owned(),
        question: "q".to_owned(),
        scope: String::new(),
    };
    let entry = AppendEntry::Note {
        text: "n".to_owned(),
        seen_seq: None,
    };
    let patch = PatchInvestigation {
        expected_seq: 0,
        title: None,
        question: None,
        status: Some(InvestigationStatus::Closed),
    };
    let finding = FindingStatusUpdate {
        expected_seq: 0,
        status: FindingStatus::Supported,
    };
    let extension = RetentionExtension {
        expected_seq: 0,
        retain_until: "2026-12-01T00:00:00Z".to_owned(),
    };
    for id in [
        "ABC",
        "0123456789abcdef0123456789abcde",
        "0123456789ABCDEF0123456789ABCDEF",
        "0123456789abcdef/../../../saved/1",
        "",
    ] {
        let refusals = [
            client.create_investigation(id, &create).await.err(),
            client.read_investigation(id, 0, None).await.err(),
            client.append_entry(id, &entry).await.err(),
            client.patch_investigation(id, &patch).await.err(),
            client.set_finding_status(id, 1, &finding).await.err(),
            client.extend_retention(id, &extension).await.err(),
            client.delete_investigation(id).await.err(),
        ];
        for refusal in refusals {
            assert!(
                matches!(&refusal, Some(ClientError::InvalidUrl(_))),
                "{id:?} gave {refusal:?}"
            );
        }
    }
    let accepted = tokio::time::timeout(Duration::from_millis(200), listener.accept()).await;
    assert!(accepted.is_err(), "the listener saw a connection");
}
