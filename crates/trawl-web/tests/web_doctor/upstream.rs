// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `proxy.upstream.trust` and `proxy.upstream.health`.
//!
//! Each test runs the doctor against real rustls upstreams: a
//! [`TlsFront`] that counts the connections it accepted and the handshakes
//! that failed, relaying to a wiremock server whose log keeps every request
//! the probe sent. Each run is one report, so the counts are exact: one
//! probe is one connection and one request, and a refused certificate is one
//! failed handshake and no request.
//!
//! The upstream's URL and host, the connect address, the CA's subject and
//! the leaf's names are planted: none may reach the output. The CA's subject
//! and the leaf's DNS names carry [`SECRET`] as well.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use trawl_api::doctor::health::HEALTH_CHECK_NAMES;
use trawl_api::doctor::{Outcome, Report, reason};
use wiremock::{MockServer, ResponseTemplate};

use crate::support::{
    HEALTH_PATH, Observed, PLANTED_CA_SUBJECT, SECRET, WebDoctorConfig, assert_anonymous_probes,
    doctor, healthy_answer, healthy_upstream, home_env, report, row, run_web_doctor, serve_health,
    verdict, write_config, write_key,
};
use crate::test_support::{LOOPBACK_SANS, TestCa, TlsFront, TlsUpstream};

/// The subject of every test CA here.
const CA_SUBJECT: &str = "trawl doctor private-secret upstream CA";

/// The subject rcgen gives a leaf that names none: every [`TlsFront`]'s.
const LEAF_SUBJECT: &str = "rcgen self signed cert";

/// A DNS name the upstream's certificate covers, reached through
/// `upstream_connect_addr`.
const UPSTREAM_NAME: &str = "upstream.private-secret.test";

/// A DNS name no test's URL uses.
const OTHER_NAME: &str = "elsewhere.private-secret.test";

/// A test's home directory and its configuration, with the planted values
/// every run checks the output for.
struct Setup {
    home: tempfile::TempDir,
    config: WebDoctorConfig,
    /// The environment besides `HOME`.
    env: Vec<(&'static str, OsString)>,
    planted: Vec<String>,
}

impl Setup {
    /// A proxy in a fresh home that dials `url` and pins `ca`.
    fn pinned(url: String, ca: &Arc<TestCa>) -> Self {
        let home = tempfile::tempdir().expect("home");
        let mut config = WebDoctorConfig::in_dir(home.path());
        config.upstream_url = Some(url);
        config.upstream_ca_path = Some(ca.path().to_owned());
        Self {
            home,
            config,
            env: Vec::new(),
            planted: [CA_SUBJECT, PLANTED_CA_SUBJECT, LEAF_SUBJECT]
                .into_iter()
                .chain(LOOPBACK_SANS)
                .map(str::to_owned)
                .collect(),
        }
    }

    fn plant(&mut self, value: impl Into<String>) {
        self.planted.push(value.into());
    }

    /// Run the doctor once, as JSON, and return its exit status and report.
    /// The run happens off the runtime's threads, so the fronts keep
    /// serving while it waits.
    async fn run(&self) -> (i32, Report) {
        let observed = self.run_format("json").await;
        assert!(observed.stderr.is_empty(), "{}", observed.stderr);
        let parsed = report(&observed.stdout);
        assert_eq!(
            i32::from(parsed.verdict().exit_code()),
            observed.code,
            "the exit status is the verdict's: {}",
            observed.stdout
        );
        (observed.code, parsed)
    }

    /// Run the doctor once in `format`, checked for every planted value.
    async fn run_format(&self, format: &'static str) -> Observed {
        let config_path = write_config(self.home.path(), &self.config);
        let mut env = home_env(self.home.path());
        env.extend(self.env.iter().cloned());
        let mut planted = self.config.planted();
        planted.extend(self.planted.iter().cloned());
        tokio::task::spawn_blocking(move || {
            let planted: Vec<&str> = planted.iter().map(String::as_str).collect();
            run_web_doctor(&args(&config_path, format), &env, &planted)
        })
        .await
        .expect("the doctor run")
    }
}

fn args(config: &Path, format: &str) -> [OsString; 5] {
    [
        "--doctor".into(),
        "--config".into(),
        config.as_os_str().to_owned(),
        "--format".into(),
        format.into(),
    ]
}

/// Every row passed or was not configured; trust and health completed, and
/// each check trawld reports has a row that completed.
fn assert_passes(code: i32, report: &Report) {
    assert_eq!(code, 0, "{report:?}");
    for check in report.checks() {
        assert!(
            matches!(check.outcome, Outcome::Complete | Outcome::NotConfigured),
            "{check:?}"
        );
    }
    assert_eq!(
        verdict(report, "proxy.upstream.trust"),
        (Outcome::Complete, None)
    );
    assert_eq!(
        verdict(report, "proxy.upstream.health"),
        (Outcome::Complete, None)
    );
    assert_eq!(
        row(report, "proxy.upstream.health").detail.as_deref(),
        Some("status: ok; HTTP 200")
    );
    for name in HEALTH_CHECK_NAMES {
        assert_eq!(
            verdict(report, &format!("proxy.upstream.health.{name}")),
            (Outcome::Complete, None)
        );
    }
}

/// The health row failed with `why`, and the run exits 1.
fn assert_health_failed(code: i32, report: &Report, why: &str) {
    assert_eq!(code, 1, "{report:?}");
    assert_eq!(
        verdict(report, "proxy.upstream.health"),
        (Outcome::Failed, Some(why)),
        "{report:?}"
    );
    assert!(
        !report
            .checks()
            .iter()
            .any(|check| check.id.starts_with("proxy.upstream.health.")),
        "a failed probe has no per-check rows: {report:?}"
    );
}

/// A pinned CA, a real rustls upstream serving trawld's health answer, and
/// a whole, correct configuration with a 32-byte key file: every check
/// passes, in both forms, and each run is one connection and one request.
#[tokio::test(flavor = "multi_thread")]
async fn web_doctor_pinned_upstream_passes() {
    let upstream = healthy_upstream().await;
    let mut setup = Setup::pinned(upstream.url(), upstream.front().ca());
    let key = setup.home.path().join("secrets/cookie.key");
    setup.config.cookie_secret_path = Some(write_key(&key, 32));

    let (code, report) = setup.run().await;
    assert_passes(code, &report);
    assert_eq!(
        verdict(&report, "proxy.cookie_key"),
        (Outcome::Complete, None)
    );
    let trust = row(&report, "proxy.upstream.trust");
    let ca_path = upstream.ca_path().to_string_lossy();
    assert!(
        trust
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains(ca_path.as_ref())),
        "the trust row names the pinned file: {trust:?}"
    );

    let table = setup.run_format("table").await;
    assert_eq!(table.code, 0, "{}", table.stdout);
    assert!(table.stderr.is_empty(), "{}", table.stderr);
    assert!(table.stdout.contains("(exit 0)"), "{}", table.stdout);

    upstream.front().settle().await;
    assert_eq!(upstream.front().connections(), 2);
    assert_eq!(upstream.front().handshake_failures(), 0);
    assert_anonymous_probes(upstream.mock(), 2).await;
}

/// `upstream_connect_addr`: the URL names a DNS name the leaf covers and no
/// resolver knows, the connection goes to the address, and TLS verifies
/// the name.
#[tokio::test(flavor = "multi_thread")]
async fn web_doctor_connect_addr_passes() {
    let ca = TestCa::named(CA_SUBJECT);
    let mock = MockServer::start().await;
    serve_health(&mock, healthy_answer()).await;
    let front = TlsFront::issued_by(&ca, *mock.address(), &[UPSTREAM_NAME]).await;
    let mut setup = Setup::pinned(format!("https://{UPSTREAM_NAME}:{}", front.port()), &ca);
    setup.config.upstream_connect_addr = Some(format!("127.0.0.1:{}", front.port()));
    setup.plant(UPSTREAM_NAME);

    let (code, report) = setup.run().await;
    assert_passes(code, &report);
    let trust = row(&report, "proxy.upstream.trust");
    assert!(
        trust
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("upstream_connect_addr")),
        "{trust:?}"
    );

    front.settle().await;
    assert_eq!(front.connections(), 1);
    assert_eq!(front.handshake_failures(), 0);
    assert_anonymous_probes(&mock, 1).await;
}

/// A leaf for another name, from the pinned CA: the trust itself resolves,
/// and the probe fails on the certificate, once. One connection, one failed
/// handshake, no retry, no request. The URL's name is checked whether it is
/// an IP literal or a DNS name dialled through `upstream_connect_addr`.
#[tokio::test(flavor = "multi_thread")]
async fn web_doctor_wrong_name_fails_once() {
    for through_connect_addr in [false, true] {
        let ca = TestCa::named(CA_SUBJECT);
        let mock = MockServer::start().await;
        serve_health(&mock, healthy_answer()).await;
        let front = TlsFront::issued_by(&ca, *mock.address(), &[OTHER_NAME]).await;
        let mut setup = if through_connect_addr {
            let url = format!("https://{UPSTREAM_NAME}:{}", front.port());
            let mut setup = Setup::pinned(url, &ca);
            setup.config.upstream_connect_addr = Some(format!("127.0.0.1:{}", front.port()));
            setup.plant(UPSTREAM_NAME);
            setup
        } else {
            Setup::pinned(front.url(), &ca)
        };
        setup.plant(OTHER_NAME);
        let label = if through_connect_addr {
            "a DNS name through upstream_connect_addr"
        } else {
            "an IP literal"
        };

        let (code, report) = setup.run().await;
        assert_eq!(
            verdict(&report, "proxy.upstream.trust"),
            (Outcome::Complete, None),
            "{label}"
        );
        assert_health_failed(code, &report, reason::CERTIFICATE_NOT_TRUSTED);

        front.settle().await;
        assert_eq!(front.connections(), 1, "{label}: {front:?}");
        assert_eq!(front.handshake_failures(), 1, "{label}: {front:?}");
        assert_anonymous_probes(&mock, 0).await;
    }
}

/// A session key in the environment, as fleet-dev sets it; it carries
/// nothing the output may show.
const ENV_KEY: &str = "ZG9jdG9yLXByb2JlLWFub255bW91cy1rZXktMzJieXQ=";

/// The probe carries nothing of a session or a key, whatever the proxy
/// holds: a key file, a key in the environment, and an upstream URL with
/// a trailing slash. It sends exactly `GET /api/v1/health`, once.
#[tokio::test(flavor = "multi_thread")]
async fn web_doctor_probe_is_anonymous() {
    let upstream = healthy_upstream().await;
    let mut setup = Setup::pinned(format!("{}/", upstream.url()), upstream.front().ca());
    let key = setup.home.path().join("secrets/cookie.key");
    setup.config.cookie_secret_path = Some(write_key(&key, 32));
    setup
        .env
        .push((fleet_auth::ENV_SESSION_AEAD_KEY, ENV_KEY.into()));
    setup.plant(ENV_KEY);

    let (code, report) = setup.run().await;
    assert_passes(code, &report);

    upstream.front().settle().await;
    assert_eq!(upstream.front().connections(), 1);
    assert_anonymous_probes(upstream.mock(), 1).await;
}

/// A URL with a user name and password fails trust with a fixed sentence
/// before anything is sent: health is blocked, and the upstream the URL
/// names sees no connection.
#[tokio::test(flavor = "multi_thread")]
async fn web_doctor_userinfo_refused() {
    let upstream = healthy_upstream().await;
    let url = format!(
        "https://doctor:{SECRET}@127.0.0.1:{}",
        upstream.front().port()
    );
    let setup = Setup::pinned(url, upstream.front().ca());

    let (code, report) = setup.run().await;
    assert_eq!(code, 1, "{report:?}");
    assert_eq!(
        verdict(&report, "proxy.upstream.trust"),
        (
            Outcome::Failed,
            Some("the upstream URL carries a user name or password")
        )
    );
    let health = row(&report, "proxy.upstream.health");
    assert_eq!(
        (health.outcome, health.reason.as_deref()),
        (Outcome::NotSampled, Some(reason::BLOCKED))
    );
    assert_eq!(health.blocked_by.as_deref(), Some("proxy.upstream.trust"));

    upstream.front().settle().await;
    assert_eq!(upstream.front().connections(), 0);
    assert_anonymous_probes(upstream.mock(), 0).await;
}

/// The upstream answers the probe with a 307 to a second listener the pin
/// also trusts: the probe fails `redirect_refused`, and the second listener
/// sees no connection at all.
#[tokio::test(flavor = "multi_thread")]
async fn web_doctor_redirect_refused() {
    let ca = TestCa::named(CA_SUBJECT);
    let upstream = TlsUpstream::issued_by(&ca).await;
    let elsewhere = TlsUpstream::issued_by(&ca).await;
    serve_health(
        upstream.mock(),
        ResponseTemplate::new(307)
            .insert_header("location", format!("{}{HEALTH_PATH}", elsewhere.url())),
    )
    .await;
    serve_health(elsewhere.mock(), healthy_answer()).await;
    let mut setup = Setup::pinned(upstream.url(), &ca);
    setup.plant(elsewhere.url());

    let (code, report) = setup.run().await;
    assert_health_failed(code, &report, reason::REDIRECT_REFUSED);
    assert_eq!(
        row(&report, "proxy.upstream.health").detail.as_deref(),
        Some("HTTP 307")
    );

    upstream.front().settle().await;
    elsewhere.front().settle().await;
    assert_eq!(upstream.front().connections(), 1);
    assert_anonymous_probes(upstream.mock(), 1).await;
    assert_eq!(elsewhere.front().connections(), 0);
    assert_anonymous_probes(elsewhere.mock(), 0).await;
}

/// trawld's request-limit refusal, under either allowance's message, is a
/// capacity refusal and not a foreign answer: `proxy.upstream.health` is
/// `not_sampled`/`request_limit_reached` with the shared text and a retry
/// with backoff, no per-check row is invented, no row says another service
/// answers, and the run is incomplete. One probe, no retry. The same code
/// under a 200, and an unrelated 503 envelope, still fail as answers that
/// are not trawld's (ADR-0054).
#[tokio::test(flavor = "multi_thread")]
async fn web_doctor_request_limit_refusal_is_not_sampled() {
    let regular = r#"{"error":{"code":"request_limit_reached","message":"trawld is at its HTTP request limit ([server] max_concurrent_requests); the request was not processed; retry later with backoff"}}"#;
    let control = r#"{"error":{"code":"request_limit_reached","message":"trawld is at its HTTP control allowance; the request was not processed; retry later with backoff","details":[]}}"#;
    let ca = TestCa::named(CA_SUBJECT);
    let answer = |status: u16, body: &str| {
        ResponseTemplate::new(status)
            .insert_header("cache-control", "no-store")
            .set_body_raw(body.to_owned(), "application/json")
    };

    for body in [regular, control] {
        let upstream = TlsUpstream::issued_by(&ca).await;
        serve_health(upstream.mock(), answer(503, body)).await;
        let setup = Setup::pinned(upstream.url(), &ca);
        let (code, report) = setup.run().await;
        assert_eq!(code, 3, "{report:?}");
        assert_eq!(
            verdict(&report, "proxy.upstream.health"),
            (Outcome::NotSampled, Some(reason::REQUEST_LIMIT_REACHED))
        );
        let health = row(&report, "proxy.upstream.health");
        assert_eq!(
            health.detail.as_deref(),
            Some("probe refused: trawld at its request limit")
        );
        assert!(
            health
                .next_action
                .as_deref()
                .is_some_and(|next| next.contains("retry with backoff")),
            "{health:?}"
        );
        for check in report.checks() {
            assert!(
                !check.id.starts_with("proxy.upstream.health."),
                "no per-check rows are invented: {check:?}"
            );
            let next = check.next_action.as_deref().unwrap_or_default();
            assert!(!next.contains("another service"), "{check:?}");
        }
        upstream.front().settle().await;
        assert_eq!(upstream.front().connections(), 1);
        assert_anonymous_probes(upstream.mock(), 1).await;
    }

    // Controls: the code under a status trawld does not pair it with, and
    // an unrelated envelope or body under a 503, are still not trawld's.
    for (status, body) in [
        (200, regular),
        (503, r#"{"error":{"code":"internal_error","message":"x"}}"#),
        (503, "<html>503 Service Unavailable</html>"),
    ] {
        let upstream = TlsUpstream::issued_by(&ca).await;
        serve_health(upstream.mock(), answer(status, body)).await;
        let setup = Setup::pinned(upstream.url(), &ca);
        let (code, report) = setup.run().await;
        assert_health_failed(
            code,
            &report,
            "the health answer is not trawld's health body",
        );
        assert!(
            row(&report, "proxy.upstream.health")
                .next_action
                .as_deref()
                .is_some_and(|next| next.contains("another service")),
            "{report:?}"
        );
    }
}

/// Nothing accepts connections at the upstream's address. The port is held
/// by a socket bound without listening for the whole run, so no other
/// process can take it and answer: a connection to it is refused.
#[tokio::test(flavor = "multi_thread")]
async fn web_doctor_upstream_down() {
    let held = tokio::net::TcpSocket::new_v4().expect("a socket");
    held.bind("127.0.0.1:0".parse().unwrap()).expect("bind");
    let port = held.local_addr().expect("the held port").port();
    let ca = TestCa::named(CA_SUBJECT);
    let setup = Setup::pinned(format!("https://127.0.0.1:{port}"), &ca);

    let (code, report) = setup.run().await;
    assert_eq!(
        verdict(&report, "proxy.upstream.trust"),
        (Outcome::Complete, None)
    );
    assert_health_failed(code, &report, reason::CONNECTION_REFUSED);
    drop(held);
}

/// The pin names a file trawld has not written yet: trust could not look,
/// health is blocked on it, and the run is incomplete, exit 3.
#[tokio::test(flavor = "multi_thread")]
async fn web_doctor_ca_not_present_is_incomplete() {
    let ca = TestCa::named(CA_SUBJECT);
    let mut setup = Setup::pinned("https://127.0.0.1:1".to_owned(), &ca);
    let absent: PathBuf = setup.home.path().join("state/tls/ca.pem");
    setup.config.upstream_ca_path = Some(absent.clone());

    let (code, report) = setup.run().await;
    assert_eq!(code, 3, "{report:?}");
    let trust = row(&report, "proxy.upstream.trust");
    assert_eq!(
        (trust.outcome, trust.reason.as_deref()),
        (Outcome::NotSampled, Some(reason::CA_NOT_PRESENT))
    );
    assert_eq!(
        trust.next_action.as_deref(),
        Some("start trawld; it writes this certificate")
    );
    assert!(
        trust
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains(absent.to_string_lossy().as_ref())),
        "{trust:?}"
    );
    let health = row(&report, "proxy.upstream.health");
    assert_eq!(
        (health.outcome, health.reason.as_deref()),
        (Outcome::NotSampled, Some(reason::BLOCKED))
    );
    assert_eq!(health.blocked_by.as_deref(), Some("proxy.upstream.trust"));
}

/// With no pin, building the probe's client loads the platform's roots,
/// from `SSL_CERT_FILE` when it is set. There it names a FIFO with no
/// writer, as a CA bundle on a stalled mount would hold the read: the
/// doctor bounds that load, trust still reports the platform roots, the
/// probe could not look, and the run is incomplete, exit 3. The FIFO's
/// path is planted: the report does not name it.
#[cfg(unix)]
#[test]
fn web_doctor_platform_roots_stall_is_incomplete() {
    let home = tempfile::tempdir().expect("home");
    let config = WebDoctorConfig::in_dir(home.path());
    let config_path = write_config(home.path(), &config);
    let bundle = home.path().join("stalled-private-secret-bundle.pem");
    nix::unistd::mkfifo(&bundle, nix::sys::stat::Mode::S_IRWXU).expect("mkfifo");
    let mut env = home_env(home.path());
    env.push(("SSL_CERT_FILE", bundle.as_os_str().to_owned()));
    let mut planted = config.planted();
    planted.push(bundle.to_string_lossy().into_owned());

    let (code, report) = doctor(&config_path, &env, &planted);
    assert_eq!(code, 3, "{report:?}");
    assert_eq!(
        verdict(&report, "proxy.upstream.trust"),
        (Outcome::Complete, None)
    );
    assert_eq!(
        verdict(&report, "proxy.upstream.health"),
        (Outcome::NotSampled, Some(reason::TIMED_OUT))
    );
}
