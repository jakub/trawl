// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawld --doctor` tests of the certificate and listener checks.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::io;
use std::net::{Ipv6Addr, SocketAddr};
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

struct Pair {
    cert_pem: String,
    key_pem: String,
    der: CertificateDer<'static>,
    key_der: PrivateKeyDer<'static>,
    planted: Vec<String>,
}

impl Pair {
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

    fn write(&self, dir: &Path, name: &str) -> (PathBuf, PathBuf) {
        let cert = dir.join(format!("{name}.crt"));
        let key = dir.join(format!("{name}.key"));
        std::fs::write(&cert, &self.cert_pem).unwrap();
        std::fs::write(&key, &self.key_pem).unwrap();
        (cert, key)
    }

    fn serving(&self) -> rustls::ServerConfig {
        trawl_server::tls::serving_config(vec![self.der.clone()], self.key_der.clone_key())
            .expect("trawld serves the pair")
    }
}

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

fn address_values(addr: SocketAddr) -> Vec<String> {
    let port = addr.port();
    vec![
        format!("127.0.0.1:{port}"),
        format!("0.0.0.0:{port}"),
        format!(":{port}"),
    ]
}

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

fn row<'a>(report: &'a Report, id: &str) -> &'a Check {
    report
        .checks()
        .iter()
        .find(|check| check.id == id)
        .unwrap_or_else(|| panic!("no {id} row in {report:#?}"))
}

fn outcome<'a>(report: &'a Report, id: &str) -> (Outcome, Option<&'a str>) {
    let check = row(report, id);
    (check.outcome, check.reason.as_deref())
}

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

fn config_for(dir: &Path, http_addr: String, tls: (PathBuf, PathBuf)) -> PathBuf {
    let mut config = DoctorConfig::in_dir(dir);
    config.http_addr = http_addr;
    config.tls = Some(tls);
    write_doctor_config(dir, &config)
}

fn fixture_addr(server: &common::TestServer) -> SocketAddr {
    server
        .url
        .strip_prefix("https://")
        .expect("an https fixture")
        .parse()
        .expect("a socket address")
}

fn tls_listener(config: rustls::ServerConfig, response: Vec<u8>) -> SocketAddr {
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    tls_listener_on(socket, config, response)
}

fn tls_listener_on(
    socket: std::net::TcpListener,
    config: rustls::ServerConfig,
    response: Vec<u8>,
) -> SocketAddr {
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    socket.set_nonblocking(true).unwrap();
    let socket = tokio::net::TcpListener::from_std(socket).unwrap();
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

#[tokio::test(flavor = "multi_thread")]
async fn doctor_listener_leaf_pin_dns_cert() {
    let server = common::setup().await;
    let dir = tempfile::tempdir().unwrap();
    let operator = Pair::new("trawl-private-subject", &["trawl.lab.example"]);
    let (cert, key) = operator.write(dir.path(), "operator");

    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = socket.local_addr().unwrap();
    let serve = serve_trawld(
        &server,
        socket,
        (&cert, &key),
        &dir.path().join("second-listener-state"),
    );

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

fn serve_trawld(
    server: &common::TestServer,
    socket: std::net::TcpListener,
    (cert, key): (&Path, &Path),
    state_dir: &Path,
) -> tokio::task::JoinHandle<Result<(), trawl_server::error::ServerError>> {
    let mut server_config: trawl_server::config::ServerConfig = toml::from_str("").unwrap();
    server_config.tls_cert_path = Some(cert.to_owned());
    server_config.tls_key_path = Some(key.to_owned());
    let http_config = trawl_server::state::HttpConfig {
        max_request_body_bytes: 1 << 20,
        max_concurrent_requests: 16,
        shutdown_drain_secs: 1,
        cors_allowed_origins: Vec::new(),
        ingest_max_body_bytes: None,
        rate_limit: trawl_server::config::RateLimitConfig::default(),
    };
    let state = server.state.clone();
    let state_dir = state_dir.to_owned();
    tokio::spawn(async move {
        trawl_server::transport::http::serve_with_listener(
            socket,
            state,
            &http_config,
            &server_config,
            &state_dir,
            None,
        )
        .await
    })
}

fn dual_loopback() -> Option<(std::net::TcpListener, std::net::TcpListener)> {
    for _ in 0..64 {
        let v4 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = v4.local_addr().unwrap().port();
        match std::net::TcpListener::bind(SocketAddr::from((Ipv6Addr::LOCALHOST, port))) {
            Ok(v6) => return Some((v4, v6)),
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => {}
            Err(error)
                if error.kind() == io::ErrorKind::AddrNotAvailable
                    || error.raw_os_error() == Some(EAFNOSUPPORT) =>
            {
                return None;
            }
            Err(error) => panic!("binding [::1]: {error}"),
        }
    }
    panic!("no port was free on both loopback addresses");
}

/// Linux's `EAFNOSUPPORT`: what binding an IPv6 address gives on a host
/// with IPv6 turned off.
const EAFNOSUPPORT: i32 = 97;

fn dual_values(port: u16) -> Vec<String> {
    vec![
        format!("localhost:{port}"),
        format!("127.0.0.1:{port}"),
        format!("[::1]:{port}"),
        format!(":{port}"),
        "::1".to_owned(),
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn doctor_listener_probes_every_resolved_address() {
    let Some((v4, v6)) = dual_loopback() else {
        eprintln!(
            "SKIPPED doctor_listener_probes_every_resolved_address: this host has no IPv6 \
             loopback (::1), so no name resolves to both loopback families"
        );
        return;
    };
    let port = v4.local_addr().unwrap().port();
    let resolved: Vec<SocketAddr> = tokio::net::lookup_host(format!("localhost:{port}"))
        .await
        .unwrap()
        .collect();
    for family in [
        SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port)),
        SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
    ] {
        assert!(
            resolved.contains(&family),
            "localhost resolves to {resolved:?}, not both loopback families"
        );
    }
    drop((v4, v6));

    let server = common::setup().await;
    let dir = tempfile::tempdir().unwrap();
    let operator = Pair::new("multi-private-subject", &["trawl.lab.example"]);
    let foreign = Pair::new("foreign-private-subject", &["trawl.lab.example"]);
    let (cert, key) = operator.write(dir.path(), "operator");
    let tls = (cert.clone(), key.clone());
    let mut planted = operator.planted.clone();
    planted.extend(foreign.planted.clone());

    for trawld_on_v6 in [false, true] {
        let (v4, v6) = dual_loopback().unwrap();
        let port = v4.local_addr().unwrap().port();
        let (ours, theirs) = if trawld_on_v6 { (v6, v4) } else { (v4, v6) };
        let serve = serve_trawld(
            &server,
            ours,
            (&cert, &key),
            &dir.path().join(format!("state-{trawld_on_v6}")),
        );
        tls_listener_on(theirs, foreign.serving(), http(200, b"{}"));
        let config = config_for(dir.path(), format!("localhost:{port}"), tls.clone());
        let mut planted = planted.clone();
        planted.extend(dual_values(port));
        let report = doctor(&config, planted_env(dir.path()), &planted).await;
        assert!(!serve.is_finished(), "the trawld listener stopped");
        assert_eq!(
            outcome(&report, IDENTITY),
            (Outcome::Complete, None),
            "trawld on v6: {trawld_on_v6}"
        );
        let detail = row(&report, IDENTITY).detail.clone().unwrap_or_default();
        assert!(
            detail.contains("at one of the 2 addresses the listener address resolves to"),
            "{detail}"
        );
        assert_eq!(outcome(&report, HEALTH), (Outcome::Complete, None));
        assert!(
            keyed(&report).contains(&("duckdb", Outcome::Complete, None)),
            "{report:#?}"
        );
        serve.abort();
    }

    let (v4, v6) = dual_loopback().unwrap();
    let port = v4.local_addr().unwrap().port();
    for socket in [v4, v6] {
        tls_listener_on(socket, foreign.serving(), http(200, b"{}"));
    }
    let config = config_for(dir.path(), format!("localhost:{port}"), tls.clone());
    let mut shown = planted.clone();
    shown.extend(dual_values(port));
    let report = doctor(&config, planted_env(dir.path()), &shown).await;
    assert_eq!(
        outcome(&report, IDENTITY),
        (Outcome::NotSampled, Some("ambiguous_address"))
    );
    assert_eq!(row(&report, HEALTH).blocked_by.as_deref(), Some(IDENTITY));

    let (v4, v6) = dual_loopback().unwrap();
    let port = v4.local_addr().unwrap().port();
    drop((v4, v6));
    let config = config_for(dir.path(), format!("localhost:{port}"), tls);
    let mut shown = planted;
    shown.extend(dual_values(port));
    let report = doctor(&config, planted_env(dir.path()), &shown).await;
    assert_eq!(
        outcome(&report, IDENTITY),
        (Outcome::NotSampled, Some("not_listening"))
    );
}

/// A file replaced between the doctor's two reads is
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
        let addr = tls_listener(config, http(200, b"{}"));
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

type Keyed = &'static [(&'static str, Outcome, Option<&'static str>)];

const UNKNOWN: Option<&str> = Some("reported a value this doctor does not know");

const UNAVAILABLE: (Outcome, Option<&str>) =
    (Outcome::Failed, Some("trawld reports itself unavailable"));

struct HealthCase {
    response: Vec<u8>,
    health: (Outcome, Option<&'static str>),
    keyed: Keyed,
}

/// A degraded answer with every kind of value: known good and failing,
/// recovering, unknown but quotable, and unknown and not shown. Its names
/// trawld does not report are a name that is not an identifier, an
/// identifier-shaped secret, and a fingerprint-shaped one.
const DEGRADED: &[u8] = br#"{"status":"degraded","version":"0.0.0","checks":{
    "duckdb":"ok","auth_db":"error","ingest_capacity":"refusing",
    "corpus":"rollup_pending","storage_db":"novel_state","data_path":"Private Value",
    "Bad-Key-Private":"ok","private_secret":"ok",
    "5e884898da28047151d0e56f8dc6292773603d0d6aabbdd62a11ef721d1542d8":"ok"}}"#;

const UNKNOWN_NAMES: [&str; 3] = [
    "Bad-Key-Private",
    "private_secret",
    "5e884898da28047151d0e56f8dc6292773603d0d6aabbdd62a11ef721d1542d8",
];

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
                ("storage_db", Outcome::Failed, UNKNOWN),
                (
                    "_invalid",
                    Outcome::Failed,
                    Some("trawld reported a check name this doctor does not know"),
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
            UNAVAILABLE,
            &[
                ("auth_db", Outcome::Complete, None),
                ("duckdb", Outcome::Failed, Some("reported error")),
            ],
        ),
        // Unavailable with nothing that fails: no row of its own fails, so
        // the health check's own row must.
        case(
            http(
                503,
                br#"{"status":"unavailable","version":"0.0.0","checks":{}}"#,
            ),
            UNAVAILABLE,
            &[],
        ),
        case(
            http(
                503,
                br#"{"status":"unavailable","version":"0.0.0","checks":{"duckdb":"ok","auth_db":"ok"}}"#,
            ),
            UNAVAILABLE,
            &[
                ("auth_db", Outcome::Complete, None),
                ("duckdb", Outcome::Complete, None),
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

/// A real trawld whose boot rollup recovery kept a marker, which it reports
/// as `corpus` `rollup_pending`: nginx's marker in an older release's
/// format, which carries no identities, so recovery keeps it unresolved
/// (case d) and touches no file.
async fn trawld_with_a_pending_rollup(dir: &Path) -> common::TestServer {
    let data = common::seed_data_root(dir);
    let day = Path::new(&data).join("prod/2024-01-15");
    let hourly = day.join("10/nginx.parquet");
    std::fs::write(day.join(".rollup-nginx"), format!("{}\n", hourly.display())).unwrap();
    common::setup_observing_boot(dir, data, None, &mut |_| {}).await
}

#[tokio::test(flavor = "multi_thread")]
async fn doctor_listener_health_rows() {
    let dir = tempfile::tempdir().unwrap();
    let pair = Pair::new("health-private-subject", &["localhost"]);
    let tls = pair.write(dir.path(), "health");
    for case in health_cases() {
        let addr = tls_listener(pair.serving(), case.response);
        let mut planted = pair.planted.clone();
        planted.extend(address_values(addr));
        planted.push("Private Value".to_owned());
        planted.extend(UNKNOWN_NAMES.map(str::to_owned));
        let config = config_for(dir.path(), addr.to_string(), tls.clone());
        let report = doctor(&config, planted_env(dir.path()), &planted).await;
        assert_eq!(outcome(&report, IDENTITY), (Outcome::Complete, None));
        assert_eq!(outcome(&report, HEALTH), case.health);
        assert_eq!(keyed(&report), case.keyed);
    }

    let addr = tls_listener(pair.serving(), http(200, DEGRADED));
    let config = config_for(dir.path(), addr.to_string(), tls.clone());
    let report = doctor(&config, planted_env(dir.path()), &pair.planted).await;
    let detail = |key: &str| {
        row(&report, &format!("{HEALTH}.{key}"))
            .detail
            .clone()
            .unwrap_or_default()
    };
    assert_eq!(detail("storage_db"), "the value is not shown");
    assert_eq!(detail("_invalid"), "3 such name(s), not shown");
    assert_eq!(detail("data_path"), "the value is not shown");
    assert_eq!(detail("corpus"), "reported rollup_pending");

    let server = common::setup().await;
    let held = server.state.engine_sample.hold();
    server
        .state
        .engine_sample
        .publish_for_test(trawl_server::engine_sample::EngineCheck::Error);
    let report = doctor_against(&server, dir.path()).await;
    drop(held);
    assert_eq!(outcome(&report, HEALTH), UNAVAILABLE);
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

/// trawld's request-limit refusal, under the regular or the probe
/// message, is a capacity refusal and not a foreign answer (ADR-0054).
#[tokio::test(flavor = "multi_thread")]
async fn doctor_listener_request_limit_refusal_is_not_sampled() {
    let dir = tempfile::tempdir().unwrap();
    let pair = Pair::new("limit-private-subject", &["localhost"]);
    let tls = pair.write(dir.path(), "limit");
    let regular: &[u8] = br#"{"error":{"code":"request_limit_reached","message":"trawld is at its HTTP request limit ([server] max_concurrent_requests); the request was not processed; retry later with backoff"}}"#;
    let probe: &[u8] = br#"{"error":{"code":"request_limit_reached","message":"trawld is at its HTTP probe allowance; the request was not processed; retry later with backoff","details":[]}}"#;
    let run = async |response: Vec<u8>| {
        let addr = tls_listener(pair.serving(), response);
        let mut planted = pair.planted.clone();
        planted.extend(address_values(addr));
        let config = config_for(dir.path(), addr.to_string(), tls.clone());
        doctor(&config, planted_env(dir.path()), &planted).await
    };

    for body in [regular, probe] {
        let report = run(http(503, body)).await;
        assert_eq!(outcome(&report, IDENTITY), (Outcome::Complete, None));
        assert_eq!(
            outcome(&report, HEALTH),
            (Outcome::NotSampled, Some("request_limit_reached"))
        );
        let health = row(&report, HEALTH);
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
        assert_eq!(keyed(&report), [], "no per-check rows are invented");
        for check in report.checks() {
            let next = check.next_action.as_deref().unwrap_or_default();
            assert!(!next.contains("another service"), "{check:?}");
        }
    }

    for response in [
        http(200, regular),
        http(503, br#"{"error":{"code":"internal_error","message":"x"}}"#),
        http(503, b"<html>503 Service Unavailable</html>"),
    ] {
        let report = run(response).await;
        assert_eq!(
            outcome(&report, HEALTH),
            (
                Outcome::Failed,
                Some("the health answer is not trawld's health body")
            )
        );
        assert!(
            row(&report, HEALTH)
                .next_action
                .as_deref()
                .is_some_and(|next| next.contains("another service")),
            "{report:?}"
        );
        assert_eq!(keyed(&report), []);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn doctor_listener_never_shows_an_unknown_health_value() {
    let dir = tempfile::tempdir().unwrap();
    let pair = Pair::new("unknown-private-subject", &["localhost"]);
    let tls = pair.write(dir.path(), "unknown");
    let body = br#"{"status":"degraded","version":"0.0.0","checks":{
        "duckdb":"ok","auth_db":"private_secret","corpus":"private_secret"}}"#;
    let addr = tls_listener(pair.serving(), http(200, body));
    let mut planted = pair.planted.clone();
    planted.extend(address_values(addr));
    planted.push("private_secret".to_owned());
    let config = config_for(dir.path(), addr.to_string(), tls);
    let report = doctor(&config, planted_env(dir.path()), &planted).await;
    assert_eq!(outcome(&report, HEALTH), (Outcome::Complete, None));
    assert_eq!(
        keyed(&report),
        [
            ("auth_db", Outcome::Failed, UNKNOWN),
            ("corpus", Outcome::Failed, UNKNOWN),
            ("duckdb", Outcome::Complete, None),
        ]
    );
    for key in ["auth_db", "corpus"] {
        assert_eq!(
            row(&report, &format!("{HEALTH}.{key}")).detail.as_deref(),
            Some("the value is not shown")
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn doctor_listener_interrupted_health_is_not_sampled() {
    let dir = tempfile::tempdir().unwrap();
    let pair = Pair::new("interrupted-private-subject", &["localhost"]);
    let tls = pair.write(dir.path(), "interrupted");
    let whole = br#"{"status":"ok","version":"0.0.0","checks":{"duckdb":"ok"}}"#;
    let mut unterminated = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ntransfer-encoding: chunked\r\n\r\n\
         {:x}\r\n",
        whole.len()
    )
    .into_bytes();
    unterminated.extend_from_slice(whole);
    unterminated.extend_from_slice(b"\r\n");
    let mut short = http(200, whole);
    short.truncate(short.len() - 10);
    let mut short_404 = http(404, whole);
    short_404.truncate(short_404.len() - 10);

    let interrupted = (Outcome::NotSampled, Some("interrupted"));
    let cases = [
        ("nothing after the handshake", Vec::new(), interrupted),
        (
            "a head cut short",
            b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n".to_vec(),
            interrupted,
        ),
        ("a body cut short of its length", short, interrupted),
        (
            "a chunked body with no last chunk",
            unterminated,
            interrupted,
        ),
        (
            "a status trawld never sends, its body cut short",
            short_404,
            (
                Outcome::Failed,
                Some("the health endpoint answered with a status trawld does not send"),
            ),
        ),
        (
            "bytes that are not HTTP",
            b"SSH-2.0-OpenSSH_9.9\r\n\r\n".to_vec(),
            (
                Outcome::Failed,
                Some(
                    "the listener proved its certificate but answered with something other \
                     than HTTP",
                ),
            ),
        ),
    ];
    for (case, response, expected) in cases {
        let addr = tls_listener(pair.serving(), response);
        let mut planted = pair.planted.clone();
        planted.extend(address_values(addr));
        let config = config_for(dir.path(), addr.to_string(), tls.clone());
        let report = doctor(&config, planted_env(dir.path()), &planted).await;
        assert_eq!(
            outcome(&report, IDENTITY),
            (Outcome::Complete, None),
            "{case}"
        );
        assert_eq!(outcome(&report, HEALTH), expected, "{case}");
        assert!(keyed(&report).is_empty(), "{case}: {report:#?}");
    }
}

#[derive(Debug, Clone, Copy)]
enum Peer {
    Close,
    Reset,
    Answer(&'static [u8]),
}

/// A plain TCP listener on loopback that reads the client's first TLS
/// record, its hello, whole on every connection, so a close sends a FIN
/// and not a reset, and then does what `peer` says.
async fn cut_off_listener(peer: Peer) -> SocketAddr {
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = socket.accept().await {
            let mut header = [0_u8; 5];
            if stream.read_exact(&mut header).await.is_err() {
                continue;
            }
            let mut hello = vec![0_u8; usize::from(u16::from_be_bytes([header[3], header[4]]))];
            if stream.read_exact(&mut hello).await.is_err() {
                continue;
            }
            match peer {
                Peer::Close => drop(stream),
                Peer::Reset => {
                    stream.set_zero_linger().unwrap();
                    drop(stream);
                }
                Peer::Answer(bytes) => {
                    let _ = stream.write_all(bytes).await;
                    let _ = stream.shutdown().await;
                }
            }
        }
    });
    addr
}

#[tokio::test(flavor = "multi_thread")]
async fn doctor_listener_cut_off_handshake_is_not_sampled() {
    let dir = tempfile::tempdir().unwrap();
    let pair = Pair::new("cut-off-private-subject", &["localhost"]);
    let tls = pair.write(dir.path(), "cut-off");
    let interrupted = (Outcome::NotSampled, Some("interrupted"));
    let cases = [
        ("closed after the hello", Peer::Close, interrupted),
        ("reset after the hello", Peer::Reset, interrupted),
        (
            "a plaintext HTTP answer to the hello",
            Peer::Answer(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\n\r\n"),
            (
                Outcome::Failed,
                Some("the listener did not complete a TLS handshake"),
            ),
        ),
    ];
    for (case, peer, expected) in cases {
        let addr = cut_off_listener(peer).await;
        let mut planted = pair.planted.clone();
        planted.extend(address_values(addr));
        let config = config_for(dir.path(), addr.to_string(), tls.clone());
        let report = doctor(&config, planted_env(dir.path()), &planted).await;
        assert_eq!(
            outcome(&report, MATERIAL),
            (Outcome::Complete, None),
            "{case}"
        );
        assert_eq!(outcome(&report, IDENTITY), expected, "{case}: {report:#?}");
        assert_eq!(
            row(&report, HEALTH).blocked_by.as_deref(),
            Some(IDENTITY),
            "{case}"
        );
    }
}

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

#[tokio::test(flavor = "multi_thread")]
async fn doctor_fails_a_directory_at_the_old_key_path() {
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("tls/key.pem");
    std::fs::create_dir_all(&old).unwrap();
    let path = write_doctor_config(dir.path(), &DoctorConfig::in_dir(dir.path()));
    let report = doctor(&path, planted_env(dir.path()), &[]).await;
    assert_eq!(
        outcome(&report, MATERIAL),
        (
            Outcome::Failed,
            Some(
                "a directory is at tls/key.pem, where an older trawld kept its key, and boot \
                 cannot remove it"
            )
        ),
        "{report:#?}"
    );
    assert_eq!(row(&report, IDENTITY).blocked_by.as_deref(), Some(MATERIAL));
    assert!(old.is_dir(), "the doctor removed nothing");

    std::fs::remove_dir(&old).unwrap();
    std::fs::write(&old, b"an old key").unwrap();
    let report = doctor(&path, planted_env(dir.path()), &[]).await;
    assert_eq!(
        outcome(&report, MATERIAL),
        (Outcome::Complete, Some("will_initialize"))
    );
    assert!(old.is_file(), "the doctor removed nothing");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn doctor_fails_a_pair_the_start_cannot_create() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    std::fs::create_dir(&state).unwrap();
    let config = DoctorConfig {
        data_path: state.join("data"),
        ingest: false,
        ..DoctorConfig::in_dir(dir.path())
    };
    let path = write_doctor_config(dir.path(), &config);
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o500)).unwrap();
    let privileged = std::fs::create_dir(state.join("probe")).is_ok();
    let report = doctor(&path, planted_env(dir.path()), &[]).await;
    if privileged {
        std::fs::remove_dir(state.join("probe")).unwrap();
    }
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
    if privileged {
        assert_eq!(
            outcome(&report, MATERIAL),
            (Outcome::Complete, Some("will_initialize"))
        );
    } else {
        assert_eq!(
            outcome(&report, MATERIAL),
            (
                Outcome::Failed,
                Some(
                    "the running user cannot create a generated TLS directory in the directory \
                     above it"
                )
            ),
            "{report:#?}"
        );
        assert_eq!(row(&report, IDENTITY).blocked_by.as_deref(), Some(MATERIAL));
    }
    for generated in ["tls", "tls-key"] {
        assert!(!state.join(generated).exists(), "{generated} was created");
    }
}
