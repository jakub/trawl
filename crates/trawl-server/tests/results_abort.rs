// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A capture whose client walks away mid-execution (ADR-0057, "Admission
//! is per key"), through `trawl-client` over a real TLS listener: the
//! transport drops the handler, the pool latches cancellation and the
//! engine never finishes, and the executor permit, the admission slot and
//! the quota reservation stay charged until the work physically stops.
//!
//! Two transports carry the same sequence. The fixture's own listener is
//! the production shape, HTTP/1.1 alone. The second is
//! `serve_with_listener_h2_for_tests`, which negotiates HTTP/2. This
//! listener exists only in tests. trawld serves HTTP/1.1 only (ADR-0054);
//! this proves the router and the client under HTTP/2, not the production
//! edge.
//!
//! The work is parked at the pool's `Started` seam, past its work-start
//! transition and before the engine runs, so the drop lands on work that
//! is physically running. The capture limits are tightened so each charge
//! is visible from outside: one result per key, so the held reservation
//! refuses the same key's next capture at its quota pre-check, and one
//! capture installation-wide, so the held slot refuses another key's.
//!
//! The binary is in the ADR-0021 `postgres` nextest group.

mod common;

use std::time::Duration;

use common::TestServer;
use common::captured::{self, at};
use trawl_api::ErrorCode;
use trawl_client::{CaptureRequest, CaptureWindow, ClientError, HttpClient, TlsTrust};
use trawl_config::CaptureConfig;
use trawl_server::config::RateLimitConfig;
use trawl_server::pool::WorkKind;
use trawl_server::pool::seam::Seam;

/// A fixture over the capture corpus whose limits make each charge
/// observable: one result per key, one capture installation-wide.
async fn boot() -> TestServer {
    let server = captured::boot(
        RateLimitConfig::default(),
        CaptureConfig {
            per_key_results: 1,
            per_key_running: 1,
            installation_running: 1,
            ..CaptureConfig::default()
        },
    )
    .await;
    server.results_clock.set(at("2024-01-16T10:30:00Z"));
    server
}

/// A `trawl-client` for `url` as `token`, trusting the fixture's
/// certificate alone, with verification on.
fn client(url: &str, token: &str) -> HttpClient {
    let (cert, _) = common::ensure_test_cert();
    let pem = std::fs::read(cert).expect("read the shared test certificate");
    HttpClient::with_trust(url, token, &TlsTrust::PinnedCa(pem)).expect("build the client")
}

/// The capture every request here makes: the 20-event corpus, last hour.
fn request() -> CaptureRequest {
    CaptureRequest {
        query: format!("service={}", captured::SERVICE),
        window: Some(CaptureWindow {
            last: Some("1h".to_owned()),
            ..CaptureWindow::default()
        }),
    }
}

/// `answer` is a 429 refusal with `code`.
fn assert_refused<T: std::fmt::Debug>(
    answer: Result<T, ClientError>,
    code: &ErrorCode,
    what: &str,
) {
    match answer {
        Err(ClientError::Server { status, error, .. }) => {
            assert_eq!(status, 429, "{what}: {error:?}");
            assert_eq!(&error.code, code, "{what}: {error:?}");
        }
        other => panic!("{what}: expected a 429 {code:?}, got {other:?}"),
    }
}

/// Every entry in the results root, sorted. The root's lock is a `flock`
/// on the directory itself, so an idle root is empty.
fn results_root_entries(server: &TestServer) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(server.state.results.root.path())
        .expect("list the results root")
        .map(|entry| {
            entry
                .expect("a results root entry")
                .file_name()
                .into_string()
                .expect("a UTF-8 entry name")
        })
        .collect();
    names.sort();
    names
}

/// Poll `cond` until it holds, failing after a generous bound.
async fn until(what: &str, cond: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while !cond() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for: {what}"));
}

/// The abort sequence against the listener at `url`, over the fixture's
/// state: drop a capture's request while its work is parked at `Started`,
/// see every charge held, release the work, see every charge freed.
async fn a_dropped_capture_holds_its_charges_until_its_work_stops(
    server: &TestServer,
    url: &str,
    protocol: &str,
) {
    let pool = &server.state.query.pool;
    let permits = pool.available_permits();
    let seams = pool.seams();
    let started = seams.hold(Seam::Started);
    let finished = seams.watch(Seam::Finished);
    let released = seams.watch(Seam::Released);
    let analyst = client(url, &server.analyst_token);
    let reader = client(url, &server.reader_token);

    // The client's request future is dropped while the work is running.
    let dropped = tokio::spawn({
        let analyst = analyst.clone();
        async move { analyst.capture(&request()).await }
    });
    until("the capture's work started", || started.arrivals() == 1).await;
    assert!(
        !dropped.is_finished(),
        "{protocol}: the capture is in flight"
    );
    dropped.abort();
    assert!(
        dropped
            .await
            .expect_err("the request was aborted")
            .is_cancelled(),
        "{protocol}: the client future was dropped, not answered"
    );

    // The transport dropped the handler: its request guard latched the
    // cancellation and booked the still-running work as retained.
    until("the dropped request's work is retained", || {
        pool.retained() == 1
    })
    .await;
    let retained = pool.retained_work();
    assert_eq!(retained.len(), 1, "{protocol}: {retained:?}");
    assert_eq!(retained[0].kind, WorkKind::Capture, "{protocol}");
    assert!(retained[0].started, "{protocol}: the work had started");

    // Every charge is held while the work exists.
    assert_eq!(
        pool.available_permits(),
        permits - 1,
        "{protocol}: the executor permit is held"
    );
    assert_refused(
        analyst.capture(&request()).await,
        &ErrorCode::CaptureQuotaExceeded,
        &format!("{protocol}: the same key, its one result reserved"),
    );
    assert_refused(
        reader.capture(&request()).await,
        &ErrorCode::CaptureBusy,
        &format!("{protocol}: another key, the installation's one slot held"),
    );
    assert_eq!(
        pool.available_permits(),
        permits - 1,
        "{protocol}: the refusals took no permit"
    );
    assert_eq!(finished.arrivals(), 0, "{protocol}");
    assert_eq!(released.arrivals(), 0, "{protocol}: nothing released yet");

    // Let the work go: the latched cancellation stops the engine before
    // it executes, so the capture never reaches its file write.
    started.release();
    until("the work stopped and released its slot", || {
        released.arrivals() == 1
    })
    .await;
    assert_eq!(
        finished.arrivals(),
        0,
        "{protocol}: the cancelled work never finished"
    );
    until("the permit returns", || {
        pool.available_permits() == permits && pool.retained() == 0
    })
    .await;
    assert_eq!(
        results_root_entries(server),
        Vec::<String>::new(),
        "{protocol}: no staged or published file, nothing at all"
    );
    assert!(
        analyst
            .list_results()
            .await
            .expect("list the analyst's results")
            .results
            .is_empty(),
        "{protocol}: no row was inserted"
    );

    // Free: the same key's next capture is admitted, then another key's.
    let next = analyst
        .capture(&request())
        .await
        .unwrap_or_else(|e| panic!("{protocol}: the same key is admitted again: {e:?}"));
    assert_eq!(next.result.captured_rows, captured::ROWS as u64);
    let other = reader
        .capture(&request())
        .await
        .unwrap_or_else(|e| panic!("{protocol}: the installation slot is free: {e:?}"));
    assert_eq!(other.result.captured_rows, captured::ROWS as u64);
    assert_eq!(finished.arrivals(), 2, "{protocol}");
    assert_eq!(pool.available_permits(), permits, "{protocol}");
}

/// HTTP/1.1, the production listener: a dropped capture request
/// interrupts its work and holds every charge until the work stops.
#[tokio::test(flavor = "multi_thread")]
async fn over_http1_a_dropped_capture_holds_its_charges_until_its_work_stops() {
    let server = boot().await;

    let response = common::harness_client_builder()
        .build()
        .unwrap()
        .get(format!("{}/api/v1/health", server.url))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.version(),
        reqwest::Version::HTTP_11,
        "the production listener answers HTTP/1.1 to a client that offers h2"
    );

    a_dropped_capture_holds_its_charges_until_its_work_stops(&server, &server.url, "HTTP/1.1")
        .await;
}

/// HTTP/2, the test-only listener: the same router and the same client
/// behave the same way when the request is one stream on a shared
/// connection.
#[tokio::test(flavor = "multi_thread")]
async fn over_http2_a_dropped_capture_holds_its_charges_until_its_work_stops() {
    let server = boot().await;
    let (url, _serve) = server.spawn_server_h2_for_tests().await;

    let response = common::harness_client_builder()
        .build()
        .unwrap()
        .get(format!("{url}/api/v1/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.version(),
        reqwest::Version::HTTP_2,
        "the test-only listener negotiates HTTP/2"
    );

    a_dropped_capture_holds_its_charges_until_its_work_stops(&server, &url, "HTTP/2").await;
}

/// The control: with nothing dropped, the same capture through the same
/// client runs to its file write, publishes one file and inserts its row,
/// so the abort's "no file, no row" is not the capture's default.
#[tokio::test(flavor = "multi_thread")]
async fn an_undropped_capture_finishes_and_keeps_its_row() {
    let server = boot().await;
    let pool = &server.state.query.pool;
    let permits = pool.available_permits();
    let seams = pool.seams();
    let finished = seams.watch(Seam::Finished);
    let analyst = client(&server.url, &server.analyst_token);

    let captured = analyst
        .capture(&request())
        .await
        .expect("the capture answers");
    assert_eq!(captured.result.captured_rows, captured::ROWS as u64);
    assert_eq!(finished.arrivals(), 1, "the work reached its file write");
    assert_eq!(
        results_root_entries(&server),
        vec![format!("{}.parquet", captured.result.id)],
        "one published file, no stage left"
    );
    let listed = analyst
        .list_results()
        .await
        .expect("list the analyst's results");
    assert_eq!(
        listed
            .results
            .iter()
            .map(|r| r.id.as_str())
            .collect::<Vec<_>>(),
        vec![captured.result.id.as_str()],
        "the row was inserted"
    );
    assert_eq!(pool.available_permits(), permits);
    assert_eq!(pool.retained(), 0);
}
