// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `GET /api/v1/capabilities` against a booted trawld (ADR-0058): any key
//! with a trawl grant reads it, a key without one is refused, and every
//! number is the configuration the server was built from, the evidence
//! policy of investigations (ADR-0059) included.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use fleet_auth::{KeyStore, PrincipalKind};
use tower::ServiceExt as _;
use trawl_api::capabilities::{
    CapabilitiesResponse, CapabilityBounds, CaptureAdmission, CaptureQuotas, EvidencePolicy,
};
use trawl_client::{ClientError, HttpClient};
use trawl_config::{CaptureConfig, InvestigationConfig};
use trawl_core::complexity::MAX_PIPELINE_STAGES;
use trawl_core::parser::MAX_QUERY_LEN;
use trawl_core::parser::suggest::{KNOWN_FUNCTIONS, KNOWN_PIPE_STAGES};
use trawl_server::config::RateLimitConfig;

use common::TestServer;

/// A fixture over the default `[capture]` limits and rate limits.
async fn setup() -> TestServer {
    setup_with(
        CaptureConfig::default(),
        RateLimitConfig::default(),
        common::DEFAULT_TEST_TIMEOUT_SECS,
    )
    .await
}

async fn setup_with(
    capture: CaptureConfig,
    rate_limit: RateLimitConfig,
    timeout_secs: u64,
) -> TestServer {
    let tmp = tempfile::tempdir().unwrap();
    let data = common::seed_data_root(tmp.path());
    let server =
        common::setup_with_capture_timeout(tmp.path(), data, rate_limit, capture, timeout_secs)
            .await;
    // The server's directories outlive this function.
    std::mem::forget(tmp);
    server
}

/// `GET {path}` as `token`, raw: the status and the JSON body.
async fn get_json(
    server: &TestServer,
    path: &str,
    token: Option<&str>,
) -> (u16, serde_json::Value) {
    let client = common::harness_client_builder().build().unwrap();
    let mut request = client.get(format!("{}{path}", server.url));
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    let body = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("{status}: the body is not JSON ({e}): {text}"));
    (status, body)
}

#[tokio::test(flavor = "multi_thread")]
async fn an_ingest_only_key_reads_the_whole_report() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.ingest_token).unwrap();

    let report = client.capabilities().await.unwrap();

    assert_eq!(report.version, trawl_core::version::PKG_VERSION);
    assert_eq!(report.dsl.stages, KNOWN_PIPE_STAGES);
    assert_eq!(report.dsl.functions, KNOWN_FUNCTIONS);
    assert!(report.dsl.stages.iter().any(|s| s == "stats"));
    assert!(report.dsl.functions.iter().any(|f| f == "count"));
    assert_eq!(report.dsl.max_query_bytes, MAX_QUERY_LEN as u64);
    assert_eq!(report.dsl.max_pipeline_stages as usize, MAX_PIPELINE_STAGES);

    let defaults = CaptureConfig::default();
    assert_eq!(
        report.bounds,
        CapabilityBounds {
            default_window_secs: defaults.default_window_secs,
            max_span_secs: defaults.max_span_secs,
            deadline_secs: defaults
                .deadline_secs
                .min(common::DEFAULT_TEST_TIMEOUT_SECS),
            max_rows: defaults.max_rows,
            max_wire_bytes: defaults.max_wire_bytes,
            page_max_rows: defaults.page_max_rows,
            ttl_secs: defaults.ttl_secs,
            query_timeout_secs: common::DEFAULT_TEST_TIMEOUT_SECS,
            max_result_rows: server.state.query.pool.max_result_rows() as u64,
        }
    );
    assert_eq!(
        report.quotas,
        CaptureQuotas {
            per_key_results: defaults.per_key_results,
            per_key_bytes: defaults.per_key_bytes,
            installation_results: defaults.installation_results,
            installation_bytes: defaults.installation_bytes,
        }
    );
    assert_eq!(
        report.admission,
        CaptureAdmission {
            per_key_running: defaults.per_key_running,
            installation_running: defaults.installation_running,
            queue_depth: defaults.queue_depth,
            rate_rpm: Some(u64::from(RateLimitConfig::default().default_rpm)),
        }
    );
    let evidence = InvestigationConfig::default();
    assert_eq!(
        report.evidence,
        EvidencePolicy {
            idle_secs: evidence.evidence_idle_secs,
            ceiling_secs: evidence.evidence_ceiling_secs,
            per_investigation_bytes: evidence.evidence_per_investigation_bytes,
            installation_bytes: evidence.evidence_installation_bytes,
        }
    );
    // The four defaults differ, so a field read from the wrong key shows.
    assert_eq!(
        report.evidence,
        EvidencePolicy {
            idle_secs: 604_800,
            ceiling_secs: 2_592_000,
            per_investigation_bytes: 64 << 20,
            installation_bytes: 1 << 30,
        }
    );
    assert_eq!(report.link_origin, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unset_link_origin_is_null_on_the_wire() {
    let server = setup().await;

    let (status, body) =
        get_json(&server, "/api/v1/capabilities", Some(&server.reader_token)).await;

    assert_eq!(status, 200, "{body}");
    let object = body.as_object().unwrap();
    assert_eq!(object.get("link_origin"), Some(&serde_json::Value::Null));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_health_block_is_what_health_answers() {
    let server = setup().await;

    let (_, health) = get_json(&server, "/api/v1/health", None).await;
    let (status, report) =
        get_json(&server, "/api/v1/capabilities", Some(&server.ingest_token)).await;

    assert_eq!(status, 200, "{report}");
    assert_eq!(report["health"], health);
    assert_eq!(health["status"], "ok", "{health}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_key_without_a_trawl_grant_is_refused() {
    let server = setup().await;
    let client = HttpClient::new_insecure(&server.url, &server.coastwatch_only_token).unwrap();

    match client.capabilities().await.unwrap_err() {
        ClientError::Server { status, .. } => assert_eq!(status, 403),
        other => panic!("expected 403, got: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_without_a_key_is_refused() {
    let server = setup().await;

    let (status, body) = get_json(&server, "/api/v1/capabilities", None).await;

    assert_eq!(status, 401, "{body}");
    assert_eq!(body["error"]["code"], "auth_error", "{body}");
}

/// Every configured number comes back exactly, and the capture deadline
/// comes back capped by the query timeout, as captures apply it.
#[tokio::test(flavor = "multi_thread")]
async fn a_non_default_config_is_reflected_exactly() {
    let capture = CaptureConfig {
        max_rows: 77,
        max_wire_bytes: 123_456,
        ttl_secs: 601,
        default_window_secs: 120,
        max_span_secs: 3_601,
        deadline_secs: 20,
        per_key_results: 3,
        per_key_bytes: 5_000_000,
        installation_results: 9,
        installation_bytes: 50_000_000,
        per_key_running: 2,
        installation_running: 3,
        queue_depth: 1,
        page_max_rows: 11,
        ..CaptureConfig::default()
    };
    let rate_limit = RateLimitConfig {
        default_rpm: 250,
        ..RateLimitConfig::default()
    };
    let server = setup_with(capture, rate_limit, 9).await;
    let client = HttpClient::new_insecure(&server.url, &server.analyst_token).unwrap();

    let report = client.capabilities().await.unwrap();

    assert_eq!(
        report.bounds,
        CapabilityBounds {
            default_window_secs: 120,
            max_span_secs: 3_601,
            deadline_secs: 9,
            max_rows: 77,
            max_wire_bytes: 123_456,
            page_max_rows: 11,
            ttl_secs: 601,
            query_timeout_secs: 9,
            max_result_rows: server.state.query.pool.max_result_rows() as u64,
        }
    );
    assert_eq!(
        report.quotas,
        CaptureQuotas {
            per_key_results: 3,
            per_key_bytes: 5_000_000,
            installation_results: 9,
            installation_bytes: 50_000_000,
        }
    );
    assert_eq!(
        report.admission,
        CaptureAdmission {
            per_key_running: 2,
            installation_running: 3,
            queue_depth: 1,
            rate_rpm: Some(250),
        }
    );
}

/// A role's `rate_rpm` replaces the class default for its key, and a key
/// on a zero default with no role rate is unlimited, as the limiter
/// resolves them.
#[tokio::test(flavor = "multi_thread")]
async fn the_rate_is_the_one_the_limiter_applies_to_the_key() {
    let server = setup_with(
        CaptureConfig::default(),
        RateLimitConfig {
            default_rpm: 0,
            ..RateLimitConfig::default()
        },
        common::DEFAULT_TEST_TIMEOUT_SECS,
    )
    .await;
    let store = KeyStore::from_pool(server.fleet_pool.clone());
    store
        .create_role("capped-ingest", Some(37), &common::trawl_perms(&["ingest"]))
        .await
        .unwrap();
    let capped = store
        .create_key(
            "capped-key",
            PrincipalKind::Service,
            &common::roles(&["capped-ingest"]),
            None,
        )
        .await
        .unwrap();

    let capped = HttpClient::new_insecure(&server.url, capped.plaintext_token.as_str())
        .unwrap()
        .capabilities()
        .await
        .unwrap();
    let unlimited = HttpClient::new_insecure(&server.url, &server.ingest_token)
        .unwrap()
        .capabilities()
        .await
        .unwrap();

    assert_eq!(capped.admission.rate_rpm, Some(37));
    assert_eq!(unlimited.admission.rate_rpm, None);
}

/// The configured origin is advertised as the state holds it. The origin
/// lives in the state rather than the HTTP config, so the router is built
/// over a clone of the fixture's state with it set.
#[tokio::test(flavor = "multi_thread")]
async fn a_configured_link_origin_is_advertised() {
    let server = setup().await;
    let mut state = server.state.clone();
    state.link_origin = Some("https://trawl.example.com:8443".into());
    let app = server.router_over(state, |_| {});

    let response = app
        .oneshot(
            Request::get("/api/v1/capabilities")
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", server.ingest_token),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let report: CapabilitiesResponse = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        report.link_origin.as_deref(),
        Some("https://trawl.example.com:8443")
    );
}

/// The configured evidence policy is advertised exactly. The policy lives
/// in the results state, so the router is built over a clone of the
/// fixture's state whose results carry it.
#[tokio::test(flavor = "multi_thread")]
async fn a_configured_evidence_policy_is_advertised() {
    use std::sync::Arc;
    use trawl_server::results::ResultsState;
    use trawl_server::results::charge::CaptureGate;

    let server = setup().await;
    let results = &server.state.results;
    let mut state = server.state.clone();
    state.results = Arc::new(ResultsState {
        root: Arc::clone(&results.root),
        store: results.store.clone(),
        gate: CaptureGate::new(
            &results.capture,
            results.store.clone(),
            Arc::clone(&results.root),
            results.clock.clone(),
        ),
        capture: results.capture.clone(),
        investigation: InvestigationConfig {
            evidence_idle_secs: 3_601,
            evidence_ceiling_secs: 86_401,
            evidence_per_investigation_bytes: 9_000_001,
            evidence_installation_bytes: 90_000_001,
        },
        clock: results.clock.clone(),
        in_flight: Arc::clone(&results.in_flight),
        steps: results.steps.clone(),
    });
    let app = server.router_over(state, |_| {});

    let response = app
        .oneshot(
            Request::get("/api/v1/capabilities")
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", server.ingest_token),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let report: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        report["evidence"],
        serde_json::json!({
            "idle_secs": 3_601,
            "ceiling_secs": 86_401,
            "per_investigation_bytes": 9_000_001,
            "installation_bytes": 90_000_001,
        })
    );
}
