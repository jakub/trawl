// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawld --doctor` tests of the certificate and listener checks.
//!
//! The listener is a real trawld wherever trawld can show the state under
//! test: `common`'s [`TestServer`](crate::common::TestServer), a second
//! trawld listener over its state serving an operator certificate, or the
//! fixture behind a TCP relay that replaces the certificate file while the
//! doctor's probe is in flight. States trawld never produces (a health
//! value it does not send, a status it does not answer, a certificate it
//! cannot sign for) come from a TLS listener in this test that serves with
//! trawld's own `rustls` config ([`trawl_server::tls::serving_config`]).
//!
//! Every run is checked with [`assert_no_values`] for the certificates'
//! subjects, names and SHA-256 fingerprints, and the listener addresses.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use trawl_api::doctor::{Check, Outcome, Report};

use crate::common;
use crate::support::{
    DoctorConfig, PLANTED_APP_URL, PLANTED_FLEET_URL, assert_no_values, planted_env, report,
    run_doctor, write_doctor_config,
};

const MATERIAL: &str = "server.tls.material";
const IDENTITY: &str = "server.listener.identity";
const HEALTH: &str = "server.listener.health";

/// A self-signed pair, and what of it the report must never show.
struct Pair {
    cert_pem: String,
    key_pem: String,
    der: CertificateDer<'static>,
    key_der: PrivateKeyDer<'static>,
    /// The subject's common name, the SANs, and the SHA-256 fingerprint.
    planted: Vec<String>,
}

impl Pair {
    /// A fresh pair whose subject is `subject` and whose SANs are `names`.
    fn new(subject: &str, names: &[&str]) -> Self {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(
            names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>(),
        )
        .unwrap();
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, subject);
        let cert = params.self_signed(&key).unwrap();
        let der = cert.der().clone();
        let mut planted = vec![subject.to_owned(), fingerprint(&der)];
        planted.extend(names.iter().map(|name| (*name).to_owned()));
        Self {
            cert_pem: cert.pem(),
            key_pem: key.serialize_pem(),
            key_der: PrivateKeyDer::Pkcs8(key.serialize_der().into()),
            der,
            planted,
        }
    }

    /// The shared fixture pair every [`common::TestServer`] serves.
    fn fixture() -> Self {
        let (cert, key) = common::ensure_test_cert();
        let cert_pem = std::fs::read_to_string(cert).unwrap();
        let key_pem = std::fs::read_to_string(key).unwrap();
        let der = CertificateDer::from_pem_slice(cert_pem.as_bytes()).unwrap();
        let key_der = PrivateKeyDer::from_pem_slice(key_pem.as_bytes()).unwrap();
        let planted = vec![
            "rcgen self signed cert".to_owned(),
            fingerprint(&der),
            "localhost".to_owned(),
            "127.0.0.1".to_owned(),
        ];
        Self {
            cert_pem,
            key_pem,
            der,
            key_der,
            planted,
        }
    }

    /// Write the pair as `dir/<name>.crt` and `dir/<name>.key`.
    fn write(&self, dir: &Path, name: &str) -> (PathBuf, PathBuf) {
        let cert = dir.join(format!("{name}.crt"));
        let key = dir.join(format!("{name}.key"));
        std::fs::write(&cert, &self.cert_pem).unwrap();
        std::fs::write(&key, &self.key_pem).unwrap();
        (cert, key)
    }

    /// trawld's own listener config for this pair.
    fn serving(&self) -> rustls::ServerConfig {
        trawl_server::tls::serving_config(vec![self.der.clone()], self.key_der.clone_key())
            .expect("trawld serves the pair")
    }
}

/// The SHA-256 fingerprint of `der`, as lowercase hex.
fn fingerprint(der: &[u8]) -> String {
    let rustls::SupportedCipherSuite::Tls13(suite) =
        rustls::crypto::ring::cipher_suite::TLS13_AES_128_GCM_SHA256
    else {
        unreachable!("a TLS 1.3 suite")
    };
    suite
        .common
        .hash_provider
        .hash(der)
        .as_ref()
        .iter()
        .fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// The strings the report must not show of a listener at `addr`.
fn address_values(addr: SocketAddr) -> Vec<String> {
    let port = addr.port();
    vec![
        format!("127.0.0.1:{port}"),
        format!("0.0.0.0:{port}"),
        format!(":{port}"),
    ]
}

/// Run the doctor over `config` in both formats with `env`, check both
/// outputs for every planted value, and return the JSON report.
async fn doctor(config: &Path, env: Vec<(&'static str, OsString)>, planted: &[String]) -> Report {
    let config = config.to_owned();
    let planted: Vec<String> = planted
        .iter()
        .cloned()
        .chain([PLANTED_FLEET_URL.to_owned(), PLANTED_APP_URL.to_owned()])
        .collect();
    tokio::task::spawn_blocking(move || {
        let planted: Vec<&str> = planted.iter().map(String::as_str).collect();
        let mut json = None;
        for format in ["table", "json"] {
            let args: [OsString; 5] = [
                "--doctor".into(),
                "--config".into(),
                config.clone().into_os_string(),
                "--format".into(),
                format.into(),
            ];
            let (code, stdout, stderr) = run_doctor(&args, &env);
            assert_no_values(&stdout, &stderr, &planted);
            assert!(matches!(code, 0 | 1 | 3), "exit {code}: {stderr}\n{stdout}");
            if format == "json" {
                let parsed = report(&stdout);
                assert_eq!(
                    parsed.verdict().exit_code(),
                    u8::try_from(code).unwrap(),
                    "{stdout}"
                );
                json = Some(parsed);
            }
        }
        json.expect("a JSON run")
    })
    .await
    .expect("the doctor run")
}

/// The row `id`.
fn row<'a>(report: &'a Report, id: &str) -> &'a Check {
    report
        .checks()
        .iter()
        .find(|check| check.id == id)
        .unwrap_or_else(|| panic!("no {id} row in {report:#?}"))
}

/// `id`'s outcome and reason.
fn outcome<'a>(report: &'a Report, id: &str) -> (Outcome, Option<&'a str>) {
    let check = row(report, id);
    (check.outcome, check.reason.as_deref())
}

/// The `server.listener.health.<key>` rows, as (key, outcome, reason).
fn keyed(report: &Report) -> Vec<(&str, Outcome, Option<&str>)> {
    report
        .checks()
        .iter()
        .filter_map(|check| {
            let key = check.id.strip_prefix("server.listener.health.")?;
            Some((key, check.outcome, check.reason.as_deref()))
        })
        .collect()
}

/// A doctor configuration in `dir` whose listener is `http_addr`, serving
/// the pair at `tls`.
fn config_for(dir: &Path, http_addr: String, tls: (PathBuf, PathBuf)) -> PathBuf {
    let mut config = DoctorConfig::in_dir(dir);
    config.http_addr = http_addr;
    config.tls = Some(tls);
    write_doctor_config(dir, &config)
}

/// The fixture's listener address.
fn fixture_addr(server: &common::TestServer) -> SocketAddr {
    server
        .url
        .strip_prefix("https://")
        .expect("an https fixture")
        .parse()
        .expect("a socket address")
}

/// A TLS listener on loopback that answers every request with `response`,
/// with `config` as its `rustls` server config.
async fn tls_listener(config: rustls::ServerConfig, response: Vec<u8>) -> SocketAddr {
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    let response = Arc::new(response);
    tokio::spawn(async move {
        while let Ok((tcp, _)) = socket.accept().await {
            let (acceptor, response) = (acceptor.clone(), Arc::clone(&response));
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let mut request = Vec::new();
                let mut buf = [0_u8; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match tls.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                let _ = tls.write_all(&response).await;
                let _ = tls.shutdown().await;
            });
        }
    });
    addr
}

/// An HTTP/1.1 response with `status` and a JSON `body`.
fn http(status: u16, body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
         connection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

/// An operator certificate that names only a DNS host passes while the
/// probe dials loopback: the listener is a real trawld listener over the
/// fixture's state, serving that certificate, and the configured address
/// is the wildcard, from the file and then from `TRAWL_HTTP_ADDR`.
#[tokio::test(flavor = "multi_thread")]
async fn doctor_listener_leaf_pin_dns_cert() {
    let server = common::setup().await;
    let dir = tempfile::tempdir().unwrap();
    let operator = Pair::new("trawl-private-subject", &["trawl.lab.example"]);
    let (cert, key) = operator.write(dir.path(), "operator");

    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = socket.local_addr().unwrap();
    let mut server_config: trawl_server::config::ServerConfig = toml::from_str("").unwrap();
    server_config.tls_cert_path = Some(cert.clone());
    server_config.tls_key_path = Some(key.clone());
    let http_config = trawl_server::state::HttpConfig {
        max_request_body_bytes: 1 << 20,
        max_concurrent_requests: 16,
        shutdown_drain_secs: 1,
        cors_allowed_origins: Vec::new(),
        ingest_max_body_bytes: None,
        rate_limit: trawl_server::config::RateLimitConfig::default(),
    };
    let state = server.state.clone();
    let state_dir = dir.path().join("second-listener-state");
    let serve = tokio::spawn(async move {
        trawl_server::transport::http::serve_with_listener(
            socket,
            state,
            &http_config,
            &server_config,
            &state_dir,
            None,
        )
        .await
    });

    let mut planted = operator.planted.clone();
    planted.extend(address_values(addr));
    let wildcard = format!("0.0.0.0:{}", addr.port());
    let from_file = config_for(dir.path(), wildcard.clone(), (cert.clone(), key.clone()));
    let from_env_dir = dir.path().join("env");
    std::fs::create_dir(&from_env_dir).unwrap();
    let from_env = config_for(&from_env_dir, "127.0.0.1:1".to_owned(), (cert, key));

    let mut env_with_addr = planted_env(&from_env_dir);
    env_with_addr.push(("TRAWL_HTTP_ADDR", wildcard.into()));
    for (config, env, source) in [
        (
            &from_file,
            planted_env(dir.path()),
            "[server] http_addr in ",
        ),
        (
            &from_env,
            env_with_addr,
            "TRAWL_HTTP_ADDR from the environment",
        ),
    ] {
        let report = doctor(config, env, &planted).await;
        assert!(!serve.is_finished(), "the operator listener stopped");
        assert_eq!(outcome(&report, MATERIAL), (Outcome::Complete, None));
        assert_eq!(outcome(&report, IDENTITY), (Outcome::Complete, None));
        let identity = row(&report, IDENTITY);
        let detail = identity.detail.as_deref().unwrap_or_default();
        assert!(
            detail.contains("dialed on loopback for the wildcard address"),
            "{identity:?}"
        );
        assert!(
            identity
                .source
                .as_deref()
                .is_some_and(|shown| shown.starts_with(source)),
            "{identity:?}"
        );
        assert_eq!(outcome(&report, HEALTH), (Outcome::Complete, None));
        // The real trawld answers for every subsystem, all well.
        let rows = keyed(&report);
        for key in [
            "auth_db",
            "corpus",
            "data_path",
            "duckdb",
            "ingest_capacity",
            "storage_db",
        ] {
            assert!(
                rows.contains(&(key, Outcome::Complete, None)),
                "{key}: {rows:?}"
            );
        }
    }
    serve.abort();
}

/// The listener serving another certificate than the file fails the
/// identity check. A file replaced between the doctor's two reads is
/// `material_changed`, and that is deterministic: the relay in front of the
/// fixture replaces the file when the probe's connection arrives, before it
/// dials trawld, so the replacement always lands after the first read and
/// before the second.
#[tokio::test(flavor = "multi_thread")]
async fn doctor_listener_stale_cert_fails() {
    let server = common::setup().await;
    let upstream = fixture_addr(&server);
    let fixture_pair = Pair::fixture();
    let dir = tempfile::tempdir().unwrap();

    // The file names another certificate than the one trawld serves.
    let stale = Pair::new("stale-private-subject", &["localhost"]);
    let mut planted = stale.planted.clone();
    planted.extend(fixture_pair.planted.clone());
    planted.extend(address_values(upstream));
    let config = config_for(
        dir.path(),
        upstream.to_string(),
        stale.write(dir.path(), "stale"),
    );
    let report = doctor(&config, planted_env(dir.path()), &planted).await;
    assert_eq!(outcome(&report, MATERIAL), (Outcome::Complete, None));
    assert_eq!(
        outcome(&report, IDENTITY),
        (
            Outcome::Failed,
            Some("the listener serves another certificate than the one on disk")
        )
    );
    let health = row(&report, HEALTH);
    assert_eq!(
        (health.reason.as_deref(), health.blocked_by.as_deref()),
        (Some("blocked"), Some(IDENTITY))
    );
    assert_eq!(report.verdict().exit_code(), 1);

    // A relay in front of trawld. While armed, each connection it accepts
    // first swaps the configured pair between the served one and
    // `replacement`, then passes the connection on.
    let (cert, key) = fixture_pair.write(dir.path(), "served");
    let replacement = Pair::new("replacement-private-subject", &["localhost"]);
    planted.extend(replacement.planted.clone());
    let armed = Arc::new(AtomicBool::new(false));
    let swaps = Arc::new(AtomicUsize::new(0));
    let relay = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let relay_addr = relay.local_addr().unwrap();
    planted.extend(address_values(relay_addr));
    {
        let (armed, swaps, cert, key) = (
            Arc::clone(&armed),
            Arc::clone(&swaps),
            cert.clone(),
            key.clone(),
        );
        let pairs = [
            (fixture_pair.cert_pem.clone(), fixture_pair.key_pem.clone()),
            (replacement.cert_pem.clone(), replacement.key_pem.clone()),
        ];
        tokio::spawn(async move {
            while let Ok((mut inbound, _)) = relay.accept().await {
                if armed.load(Ordering::SeqCst) {
                    let (new_cert, new_key) =
                        &pairs[(swaps.fetch_add(1, Ordering::SeqCst) + 1) % 2];
                    for (path, pem) in [(&cert, new_cert), (&key, new_key)] {
                        let staged = path.with_extension("next");
                        std::fs::write(&staged, pem).unwrap();
                        std::fs::rename(&staged, path).unwrap();
                    }
                }
                let mut outbound = tokio::net::TcpStream::connect(upstream).await.unwrap();
                tokio::spawn(async move {
                    let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                });
            }
        });
    }
    let config = config_for(dir.path(), relay_addr.to_string(), (cert.clone(), key));

    // Unarmed, the relay changes nothing: trawld's certificate is proven.
    let report = doctor(&config, planted_env(dir.path()), &planted).await;
    assert_eq!(outcome(&report, IDENTITY), (Outcome::Complete, None));
    assert_eq!(outcome(&report, HEALTH), (Outcome::Complete, None));

    // Armed, the pair changes under each run's probe: the table run reads
    // the served pair and finds the replacement after, the JSON run the
    // reverse. The JSON run's pin is not what trawld serves, and the change
    // still wins: a comparison against changing material proves nothing.
    armed.store(true, Ordering::SeqCst);
    let report = doctor(&config, planted_env(dir.path()), &planted).await;
    armed.store(false, Ordering::SeqCst);
    assert_eq!(swaps.load(Ordering::SeqCst), 2, "one swap per run's probe");
    assert_eq!(
        std::fs::read_to_string(&cert).unwrap(),
        fixture_pair.cert_pem,
        "two swaps put the served pair back"
    );
    assert_eq!(outcome(&report, MATERIAL), (Outcome::Complete, None));
    assert_eq!(
        outcome(&report, IDENTITY),
        (Outcome::NotSampled, Some("material_changed"))
    );
    let health = row(&report, HEALTH);
    assert_eq!(
        (health.reason.as_deref(), health.blocked_by.as_deref()),
        (Some("blocked"), Some(IDENTITY))
    );
}

/// A listener that presents the certificate on disk but cannot sign with
/// its key is refused, over TLS 1.2 and TLS 1.3: the pinned leaf alone
/// proves nothing without the handshake signature.
#[tokio::test(flavor = "multi_thread")]
async fn doctor_listener_refuses_the_leaf_without_its_key() {
    let dir = tempfile::tempdir().unwrap();
    let pinned = Pair::new("pinned-private-subject", &["localhost"]);
    let impostor = Pair::new("impostor-private-subject", &["localhost"]);
    let tls = pinned.write(dir.path(), "pinned");
    let mut planted = pinned.planted.clone();
    planted.extend(impostor.planted.clone());

    for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
        let signer = rustls::crypto::ring::default_provider()
            .key_provider
            .load_private_key(impostor.key_der.clone_key())
            .unwrap();
        let presented = Arc::new(rustls::sign::CertifiedKey::new(
            vec![pinned.der.clone()],
            signer,
        ));
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(&[version])
        .unwrap()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(Presents(presented)));
        let addr = tls_listener(config, http(200, b"{}")).await;
        let mut planted = planted.clone();
        planted.extend(address_values(addr));
        let config = config_for(dir.path(), addr.to_string(), tls.clone());
        let report = doctor(&config, planted_env(dir.path()), &planted).await;
        assert_eq!(outcome(&report, MATERIAL), (Outcome::Complete, None));
        assert_eq!(
            outcome(&report, IDENTITY),
            (
                Outcome::Failed,
                Some(
                    "the listener presented the certificate on disk but did not prove it holds \
                     its key"
                )
            ),
            "{version:?}"
        );
        assert_eq!(row(&report, HEALTH).blocked_by.as_deref(), Some(IDENTITY));
    }
}

/// Serves one certificate and key, whatever the hello asks.
#[derive(Debug)]
struct Presents(Arc<rustls::sign::CertifiedKey>);

impl rustls::server::ResolvesServerCert for Presents {
    fn resolve(
        &self,
        _hello: rustls::server::ClientHello<'_>,
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        Some(Arc::clone(&self.0))
    }
}

/// The per-key rows a health answer should give, as (key, outcome, reason).
type Keyed = &'static [(&'static str, Outcome, Option<&'static str>)];

/// The reason of a value no doctor knows.
const UNKNOWN: Option<&str> = Some("reported a value this doctor does not know");

/// A health answer from a listener in this test, and the rows it gives.
struct HealthCase {
    response: Vec<u8>,
    health: (Outcome, Option<&'static str>),
    keyed: Keyed,
}

/// A degraded answer with every kind of value: known good and failing,
/// recovering, unknown but quotable, unknown and not shown, and a name that
/// is not an identifier.
const DEGRADED: &[u8] = br#"{"status":"degraded","version":"0.0.0","checks":{
    "duckdb":"ok","auth_db":"error","ingest_capacity":"refusing",
    "corpus":"rollup_pending","wal":"novel_state","data_path":"Private Value",
    "Bad-Key-Private":"ok"}}"#;

/// The answers trawld never sends, or sends only in states a fixture does
/// not reach, and what the doctor must make of each.
fn health_cases() -> Vec<HealthCase> {
    let case = |response, health, keyed| HealthCase {
        response,
        health,
        keyed,
    };
    vec![
        case(
            http(200, DEGRADED),
            (Outcome::Complete, None),
            &[
                ("auth_db", Outcome::Failed, Some("reported error")),
                ("corpus", Outcome::NotSampled, Some("recovering")),
                ("data_path", Outcome::Failed, UNKNOWN),
                ("duckdb", Outcome::Complete, None),
                (
                    "ingest_capacity",
                    Outcome::Failed,
                    Some("reported refusing"),
                ),
                ("wal", Outcome::Failed, UNKNOWN),
                (
                    "_invalid",
                    Outcome::Failed,
                    Some("trawld reported a check name that is not [a-z][a-z0-9_]{0,63}"),
                ),
            ],
        ),
        case(
            http(
                200,
                br#"{"status":"degraded","version":"0.0.0","checks":{"corpus":"restart_backlog"}}"#,
            ),
            (Outcome::Complete, None),
            &[("corpus", Outcome::NotSampled, Some("recovering"))],
        ),
        case(
            http(
                503,
                br#"{"status":"unavailable","version":"0.0.0","checks":{"duckdb":"error","auth_db":"ok"}}"#,
            ),
            (Outcome::Complete, None),
            &[
                ("auth_db", Outcome::Complete, None),
                ("duckdb", Outcome::Failed, Some("reported error")),
            ],
        ),
        case(
            http(
                503,
                br#"{"error":{"code":"corpus_recovering","message":"not yet"}}"#,
            ),
            (Outcome::NotSampled, Some("recovering")),
            &[],
        ),
        case(
            http(200, b"<html>not trawld</html>"),
            (
                Outcome::Failed,
                Some("the health answer is not trawld's health body"),
            ),
            &[],
        ),
        case(
            http(404, b"{}"),
            (
                Outcome::Failed,
                Some("the health endpoint answered with a status trawld does not send"),
            ),
            &[],
        ),
        case(
            http(200, &vec![b' '; 70 * 1024]),
            (Outcome::NotSampled, Some("too_large")),
            &[],
        ),
    ]
}

/// Run the doctor against `server`, which serves the fixture pair, and
/// return its report.
async fn doctor_against(server: &common::TestServer, dir: &Path) -> Report {
    let fixture_pair = Pair::fixture();
    let upstream = fixture_addr(server);
    let mut planted = fixture_pair.planted.clone();
    planted.extend(address_values(upstream));
    let config = config_for(
        dir,
        upstream.to_string(),
        fixture_pair.write(dir, "fixture"),
    );
    doctor(&config, planted_env(dir), &planted).await
}

/// A real trawld whose rollup recovery failed at boot, which it reports as
/// `corpus` `rollup_pending` (the `boot_corpus` suite's stuck rollup).
async fn trawld_with_a_pending_rollup(dir: &Path) -> common::TestServer {
    let data = common::seed_data_root(dir);
    let day = Path::new(&data).join("prod/2024-01-15");
    let stuck = day.join("10/stuck.parquet");
    for path in [stuck.clone(), day.join("10/stuck.parquet.merged")] {
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("child"), b"occupied").unwrap();
    }
    std::fs::copy(day.join("10/nginx.parquet"), day.join("stuck.parquet")).unwrap();
    std::fs::write(
        day.join(".rollup-stuck"),
        stuck.to_string_lossy().as_bytes(),
    )
    .unwrap();
    common::setup_observing_boot(dir, data, None, &mut |_| {}).await
}

/// Each reported health value maps as specified. A real trawld whose
/// `DuckDB` probe fails answers 503 and keeps its body: `duckdb` fails, the
/// rest still report. A real trawld with an unresolved rollup reports
/// `corpus` `rollup_pending`, which is `not_sampled`/`recovering`. Values
/// and answers trawld does not send come from a listener in this test that
/// serves with trawld's own TLS config.
#[tokio::test(flavor = "multi_thread")]
async fn doctor_listener_health_rows() {
    let dir = tempfile::tempdir().unwrap();
    let pair = Pair::new("health-private-subject", &["localhost"]);
    let tls = pair.write(dir.path(), "health");
    for case in health_cases() {
        let addr = tls_listener(pair.serving(), case.response).await;
        let mut planted = pair.planted.clone();
        planted.extend(address_values(addr));
        planted.extend(["Private Value".to_owned(), "Bad-Key-Private".to_owned()]);
        let config = config_for(dir.path(), addr.to_string(), tls.clone());
        let report = doctor(&config, planted_env(dir.path()), &planted).await;
        assert_eq!(outcome(&report, IDENTITY), (Outcome::Complete, None));
        assert_eq!(outcome(&report, HEALTH), case.health);
        assert_eq!(keyed(&report), case.keyed);
    }

    // The quotable unknown value is quoted; the other is not shown.
    let addr = tls_listener(pair.serving(), http(200, DEGRADED)).await;
    let config = config_for(dir.path(), addr.to_string(), tls.clone());
    let report = doctor(&config, planted_env(dir.path()), &pair.planted).await;
    let detail = |key: &str| {
        row(&report, &format!("{HEALTH}.{key}"))
            .detail
            .clone()
            .unwrap_or_default()
    };
    assert_eq!(detail("wal"), "reported novel_state");
    assert_eq!(detail("data_path"), "the value is not shown");

    // A real trawld whose DuckDB probe cannot run in time answers 503.
    let server = common::setup().await;
    let seams = server.state.query.pool.seams();
    let held = seams.hold(trawl_server::pool::seam::Seam::Started);
    let report = doctor_against(&server, dir.path()).await;
    held.release();
    drop(seams);
    assert_eq!(outcome(&report, HEALTH), (Outcome::Complete, None));
    let health = row(&report, HEALTH);
    assert!(
        health
            .detail
            .as_deref()
            .is_some_and(|shown| shown.contains("status: unavailable; HTTP 503")),
        "{health:?}"
    );
    let rows = keyed(&report);
    assert!(
        rows.contains(&("duckdb", Outcome::Failed, Some("reported error"))),
        "{rows:?}"
    );
    assert!(
        rows.contains(&("auth_db", Outcome::Complete, None)),
        "{rows:?}"
    );

    // A real trawld whose rollup recovery failed reports it pending.
    let boot_dir = tempfile::tempdir().unwrap();
    let server = trawld_with_a_pending_rollup(boot_dir.path()).await;
    let report = doctor_against(&server, dir.path()).await;
    assert_eq!(outcome(&report, HEALTH), (Outcome::Complete, None));
    let rows = keyed(&report);
    assert!(
        rows.contains(&("corpus", Outcome::NotSampled, Some("recovering"))),
        "{rows:?}"
    );
    assert!(
        rows.contains(&("duckdb", Outcome::Complete, None)),
        "{rows:?}"
    );
}

/// With auto TLS and no generated pair yet, the doctor only opens a TCP
/// connection: nothing there is `not_listening`; something there is
/// `no_material`, and receives not one byte. Nothing is generated.
#[tokio::test(flavor = "multi_thread")]
async fn doctor_listener_without_material_only_connects() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = DoctorConfig::in_dir(dir.path());
    let path = write_doctor_config(dir.path(), &config);
    let report = doctor(&path, planted_env(dir.path()), &[]).await;
    assert_eq!(
        outcome(&report, MATERIAL),
        (Outcome::Complete, Some("will_initialize"))
    );
    assert_eq!(
        outcome(&report, IDENTITY),
        (Outcome::NotSampled, Some("not_listening"))
    );

    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    let received = tokio::spawn(async move {
        let mut bytes = Vec::new();
        for _ in 0..2 {
            let (mut stream, _) = socket.accept().await.unwrap();
            stream.read_to_end(&mut bytes).await.unwrap();
        }
        bytes
    });
    config.http_addr = addr.to_string();
    let path = write_doctor_config(dir.path(), &config);
    let report = doctor(&path, planted_env(dir.path()), &address_values(addr)).await;
    assert_eq!(
        outcome(&report, IDENTITY),
        (Outcome::NotSampled, Some("no_material"))
    );
    assert!(
        received.await.unwrap().is_empty(),
        "the doctor sent bytes to a listener it had nothing to compare with"
    );
    for generated in ["tls", "tls-key"] {
        assert!(
            !dir.path().join(generated).exists(),
            "{generated} was created"
        );
    }
}
