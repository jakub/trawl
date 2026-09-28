// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawl doctor` target and key selection, observed on the real binary
//! (ADR-0047).
//!
//! Every run strips the inherited `TRAWL_*` environment and gets a temp
//! `HOME`, so the default config path is one the test wrote or none at all.
//! Tests cannot set the environment in-process (`unsafe_code = forbid`), so
//! every case is a subprocess.
//!
//! Where a case must not reach a server, a plain TCP listener stands where a
//! wrong resolution would send the doctor, and records every connection and
//! every `Authorization` header it sees. Where a case must reach one, a
//! [`Stub`] answers over real TLS (rustls, certificates from an rcgen CA, as
//! in `tests/ca_cert.rs`) or plain HTTP, and records every handshake and
//! request. Every stub asserts that what it saw is within
//! [`trawl_cli::doctor::REQUESTS`].
//!
//! Under `--url` the doctor trusts system roots only. On Linux those come
//! from `rustls-native-certs`, which reads `SSL_CERT_FILE` when it is set,
//! so a `--url` case that needs a verified connection sets that variable to
//! the test CA on the subprocess alone. Every other run removes it.

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

const SAVED_TOKEN: &str = "flt_savedservertokenvalue";

/// What a [`Recorder`] saw.
#[derive(Debug, Default)]
struct Seen {
    connections: usize,
    authorization_headers: usize,
}

/// A plain TCP listener on `127.0.0.1` that records, answers nothing, and
/// closes.
struct Recorder {
    port: u16,
    seen: Arc<Mutex<Seen>>,
}

impl Recorder {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let record = Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                record.lock().unwrap().connections += 1;
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                    .ok();
                let mut head = Vec::new();
                let mut buf = [0u8; 4096];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => head.extend_from_slice(&buf[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
                if head.contains("\nauthorization:") {
                    record.lock().unwrap().authorization_headers += 1;
                }
            }
        });
        Self { port, seen }
    }

    fn url(&self, scheme: &str) -> String {
        format!("{scheme}://127.0.0.1:{}", self.port)
    }

    fn assert_untouched(&self, case: &str) {
        let seen = self.seen.lock().unwrap();
        assert_eq!(seen.connections, 0, "{case}: nothing may connect");
        assert_eq!(seen.authorization_headers, 0, "{case}: no key may be sent");
    }
}

/// A temp `HOME` (and `XDG_STATE_HOME`) for one test.
struct Sandbox {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    state_home: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let state_home = tmp.path().join("state");
        std::fs::create_dir(&home).unwrap();
        Self {
            _tmp: tmp,
            home,
            state_home,
        }
    }

    /// Write the default config file, `~/.config/trawl/config.toml`.
    fn default_config(&self, toml: &str) -> PathBuf {
        let dir = self.home.join(".config/trawl");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, toml).unwrap();
        path
    }

    fn file(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.home.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn trawl(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_trawl"));
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("TRAWL_") {
                cmd.env_remove(key);
            }
        }
        cmd.env_remove("SSL_CERT_FILE")
            .env_remove("SSL_CERT_DIR")
            .env("HOME", &self.home)
            .env("XDG_STATE_HOME", &self.state_home)
            .args(args)
            .envs(env.iter().copied());
        cmd.output().expect("spawn trawl")
    }
}

/// A port with nothing listening on it.
fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("addr").port()
}

// ── TLS material ─────────────────────────────────────────────────────────

/// A self-signed CA that can issue server certificates.
fn ca(name: &str) -> CertifiedIssuer<'static, KeyPair> {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    params.distinguished_name.push(DnType::CommonName, name);
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    CertifiedIssuer::self_signed(params, KeyPair::generate().expect("CA key")).expect("CA cert")
}

/// A server certificate for `127.0.0.1`, issued by `issuer`.
fn leaf(issuer: &CertifiedIssuer<'static, KeyPair>) -> Identity {
    let key = KeyPair::generate().expect("leaf key");
    let mut params = CertificateParams::new(vec!["127.0.0.1".to_owned()]).expect("leaf params");
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let cert = params.signed_by(&key, issuer).expect("sign leaf");
    Identity {
        chain: vec![cert.der().clone()],
        key: PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
    }
}

struct Identity {
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
}

// ── the recording stub ───────────────────────────────────────────────────

/// One request a [`Stub`] received.
#[derive(Debug, Clone)]
struct Request {
    method: String,
    path: String,
    /// The `Authorization` header's value, if one was sent.
    authorization: Option<String>,
}

/// What a [`Stub`] saw.
#[derive(Debug, Default)]
struct StubSeen {
    /// One entry per TLS connection: whether its handshake completed.
    handshakes: Vec<bool>,
    requests: Vec<Request>,
}

/// An answer: status and JSON body. The status [`STALL`] sends `200`
/// headers and the body's first byte, then holds the connection open. The
/// status [`BROKEN`] sends `200` headers and part of the body, then closes.
type Route = (&'static str, u16, String);

/// A route status that stalls after the headers, past the doctor's 10 s
/// request deadline.
const STALL: u16 = 0;

/// A route status whose headers declare 100 bytes of body, then send 10 of
/// them and close the connection.
const BROKEN: u16 = 1;

/// A listener on `127.0.0.1` that speaks TLS (or plain HTTP), answers each
/// routed path with a fixed status and body, and records everything.
struct Stub {
    port: u16,
    seen: Arc<Mutex<StubSeen>>,
}

impl Stub {
    fn tls(identity: Identity, routes: Vec<Route>) -> Self {
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_no_client_auth()
        .with_single_cert(identity.chain, identity.key)
        .expect("server certificate");
        Self::start(Some(Arc::new(config)), routes)
    }

    fn plain(routes: Vec<Route>) -> Self {
        Self::start(None, routes)
    }

    fn start(tls: Option<Arc<rustls::ServerConfig>>, routes: Vec<Route>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let seen = Arc::new(Mutex::new(StubSeen::default()));
        let record = Arc::clone(&seen);
        let routes = Arc::new(routes);
        std::thread::spawn(move || {
            for tcp in listener.incoming() {
                let Ok(tcp) = tcp else { return };
                let (tls, routes, record) = (tls.clone(), Arc::clone(&routes), Arc::clone(&record));
                std::thread::spawn(move || serve(tcp, tls, &routes, &record));
            }
        });
        Self { port, seen }
    }

    fn url(&self, scheme: &str) -> String {
        format!("{scheme}://127.0.0.1:{}", self.port)
    }

    fn requests(&self) -> Vec<Request> {
        self.seen.lock().unwrap().requests.clone()
    }

    /// The handshake outcomes, once at least `count` have been recorded.
    fn handshakes(&self, count: usize) -> Vec<bool> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let seen = self.seen.lock().unwrap();
            if seen.handshakes.len() >= count || Instant::now() > deadline {
                return seen.handshakes.clone();
            }
            drop(seen);
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn assert_no_authorization(&self, case: &str) {
        for request in self.requests() {
            assert!(
                request.authorization.is_none(),
                "{case}: {} {} carried an Authorization header",
                request.method,
                request.path
            );
        }
    }
}

/// Every request a stub saw is one the doctor declares it may send.
impl Drop for Stub {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        for request in self.requests() {
            assert!(
                trawl_cli::doctor::REQUESTS
                    .iter()
                    .any(|(method, path)| request.method == *method && request.path == *path),
                "{} {} is not in trawl_cli::doctor::REQUESTS",
                request.method,
                request.path
            );
        }
    }
}

fn serve(
    mut tcp: TcpStream,
    tls: Option<Arc<rustls::ServerConfig>>,
    routes: &[Route],
    seen: &Mutex<StubSeen>,
) {
    tcp.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let Some(config) = tls else {
        answer(&mut tcp, routes, seen);
        return;
    };
    let mut conn = rustls::ServerConnection::new(config).expect("server connection");
    let mut completed = true;
    while conn.is_handshaking() {
        match conn.complete_io(&mut tcp) {
            Ok((0, 0)) | Err(_) => {
                completed = false;
                break;
            }
            Ok(_) => {}
        }
    }
    seen.lock().unwrap().handshakes.push(completed);
    if completed {
        let mut stream = rustls::StreamOwned::new(conn, tcp);
        answer(&mut stream, routes, seen);
        stream.conn.send_close_notify();
        let _ = stream.flush();
    }
}

/// Read one request, record it, and answer it from `routes`.
fn answer<S: std::io::Read + std::io::Write>(
    stream: &mut S,
    routes: &[Route],
    seen: &Mutex<StubSeen>,
) {
    let mut head = Vec::new();
    let mut buf = [0u8; 4096];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => head.extend_from_slice(&buf[..n]),
        }
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split(' ');
    let method = request_line.next().unwrap_or_default().to_owned();
    let path = request_line.next().unwrap_or_default().to_owned();
    let authorization = lines.find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("authorization")
            .then(|| value.trim().to_owned())
    });
    seen.lock().unwrap().requests.push(Request {
        method,
        path: path.clone(),
        authorization,
    });
    let (status, body) = routes.iter().find(|(route, _, _)| *route == path).map_or(
        (404, r#"{"error":"not found"}"#.to_owned()),
        |(_, status, body)| (*status, body.clone()),
    );
    if status == STALL {
        let head =
            "HTTP/1.1 200 Stub\r\ncontent-type: application/json\r\ncontent-length: 100\r\n\r\n{";
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.flush();
        std::thread::sleep(Duration::from_secs(30));
        return;
    }
    if status == BROKEN {
        let head = "HTTP/1.1 200 Stub\r\ncontent-type: application/json\r\ncontent-length: 100\r\nconnection: close\r\n\r\n{\"status\":";
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.flush();
        return;
    }
    // Every 3xx points back at this origin's whoami, the worst case: a
    // client that followed it would send the key to the same server.
    let location = if (300..400).contains(&status) {
        format!("location: {WHOAMI_PATH}\r\n")
    } else {
        String::new()
    };
    let response = format!(
        "HTTP/1.1 {status} Stub\r\ncontent-type: application/json\r\n{location}content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

const HEALTH_PATH: &str = "/api/v1/health";
const WHOAMI_PATH: &str = "/api/v1/whoami";

fn healthy() -> Route {
    (
        HEALTH_PATH,
        200,
        format!(
            r#"{{"status":"ok","checks":{{"duckdb":"ok","ingest_capacity":"ok"}},"version":"{}"}}"#,
            env!("CARGO_PKG_VERSION")
        ),
    )
}

fn whoami(permissions: &str) -> Route {
    (
        WHOAMI_PATH,
        200,
        format!(
            r#"{{"prefix":"pfx12345","name":"ops-key","kind":"human","roles":["reader"],"permissions":[{permissions}]}}"#
        ),
    )
}

/// The checks of a report by id: `(outcome, reason, blocked_by)`.
fn rows(report: &serde_json::Value) -> Vec<(String, String, Option<String>, Option<String>)> {
    report["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .map(|check| {
            (
                check["id"].as_str().unwrap().to_owned(),
                check["outcome"].as_str().unwrap().to_owned(),
                check["reason"].as_str().map(str::to_owned),
                check["blocked_by"].as_str().map(str::to_owned),
            )
        })
        .collect()
}

fn check_by_id<'a>(report: &'a serde_json::Value, id: &str) -> &'a serde_json::Value {
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["id"] == id)
        .unwrap_or_else(|| panic!("no {id} check in {report}"))
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// A usage error: exit 2, nothing on stdout, and none of `secrets` echoed.
fn assert_usage_error(output: &Output, fragment: &str, secrets: &[&str], case: &str) {
    let all = text(output);
    assert_eq!(output.status.code(), Some(2), "{case}: exit 2, got {all}");
    assert!(
        output.stdout.is_empty(),
        "{case}: stdout stays empty: {all}"
    );
    assert!(
        all.contains(fragment),
        "{case}: must name {fragment:?}: {all}"
    );
    for secret in secrets {
        assert!(!all.contains(secret), "{case}: {secret:?} leaked: {all}");
    }
}

/// The JSON report of a run that got past selection.
fn report(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|e| panic!("stdout is not a JSON report ({e}): {}", text(output)))
}

fn connection_config(report: &serde_json::Value) -> &serde_json::Value {
    let check = &report["checks"][0];
    assert_eq!(check["id"], "connection.config");
    check
}

/// Each ambient variable refuses the run by name, never by value, even when
/// it is empty, malformed, or a flag would shadow it. clap never reads these
/// variables for `doctor`, so a value it would reject (a non-bool
/// `TRAWL_INSECURE`) is refused by name like any other.
#[test]
fn doctor_refuses_ambient_env() {
    const PLANTED: &str = "flt_plantedambientvalue";
    let home = Sandbox::new();
    let recorder = Recorder::start();
    let url = recorder.url("https");
    for name in [
        "TRAWL_URL",
        "TRAWL_PROFILE",
        "TRAWL_TOKEN",
        "TRAWL_INSECURE",
    ] {
        let values: &[&str] = if name == "TRAWL_INSECURE" {
            &[PLANTED, "", "true", "false"]
        } else {
            &[PLANTED, ""]
        };
        for value in values {
            let case = format!("{name}={value:?}");
            for args in [
                vec!["doctor", "--url", url.as_str()],
                // A flag that shadows the variable does not hide it.
                vec!["doctor", "--url", url.as_str(), "--insecure"],
                // Neither does an argv error: the variable is never parsed.
                vec!["doctor", "--no-such-flag"],
            ] {
                let output = home.trawl(&args, &[(name, value)]);
                let all = text(&output);
                assert_eq!(output.status.code(), Some(2), "{case} {args:?}: {all}");
                assert!(output.stdout.is_empty(), "{case} {args:?}: {all}");
                assert!(!all.contains(PLANTED), "{case} {args:?}: {all}");
                if !args.contains(&"--no-such-flag") {
                    assert!(
                        all.contains(&format!("{name} is set")),
                        "{case} {args:?}: {all}"
                    );
                }
            }
        }
    }
    recorder.assert_untouched("ambient environment");
}

/// No `--help` prints an environment value, for `doctor` or any other
/// command, even when a variable carries credentials or clap would reject
/// it.
#[test]
fn help_never_prints_environment_values() {
    const SENTINEL: &str = "s3ntinelvalue";
    let home = Sandbox::new();
    let env = [
        ("TRAWL_URL", "https://user:s3ntinelvalue@localhost:1"),
        ("TRAWL_PROFILE", "s3ntinelvalue"),
        ("TRAWL_TOKEN", "flt_s3ntinelvalue"),
        ("TRAWL_INSECURE", "s3ntinelvalue"),
    ];
    for args in [
        vec!["--help"],
        vec!["doctor", "--help"],
        vec!["help", "doctor"],
        vec!["query", "--help"],
    ] {
        let output = home.trawl(&args, &env);
        let all = text(&output);
        assert_eq!(output.status.code(), Some(0), "{args:?}: {all}");
        assert!(!all.contains(SENTINEL), "{args:?} printed a value: {all}");
        for (name, _) in env {
            assert!(all.contains(&format!("[env: {name}]")), "{args:?}: {all}");
        }
    }
}

/// Other commands still read the environment as before: `TRAWL_URL` and
/// `TRAWL_TOKEN` select the server a query goes to, and a malformed
/// `TRAWL_INSECURE` is still clap's own usage error.
#[test]
fn other_commands_still_read_the_environment() {
    let home = Sandbox::new();
    let port = closed_port();
    let output = home.trawl(
        &["query", "* | head 1"],
        &[
            ("TRAWL_URL", &format!("https://127.0.0.1:{port}")),
            ("TRAWL_TOKEN", "flt_querytoken"),
        ],
    );
    let all = text(&output);
    assert_eq!(output.status.code(), Some(1), "{all}");
    assert!(
        all.contains(&format!(
            "connection failed for API https://127.0.0.1:{port}"
        )),
        "the query went to TRAWL_URL: {all}"
    );

    let output = home.trawl(
        &["validate", "* | head 1"],
        &[("TRAWL_INSECURE", "garbage")],
    );
    let all = text(&output);
    assert_eq!(output.status.code(), Some(2), "{all}");
    assert!(all.contains("for '--insecure'"), "{all}");
}

/// Selection errors on the command line: all exit 2, before anything is
/// read or contacted.
#[test]
fn doctor_selection_usage_errors() {
    let home = Sandbox::new();
    let recorder = Recorder::start();
    let url = recorder.url("https");
    let key = home.file("key", "flt_keyfilevalue\n");
    let key = key.to_str().unwrap();
    let config = home.default_config(&format!("[profiles.prod]\nurl = \"{url}\"\n"));
    let config = config.to_str().unwrap();

    let cases: &[(&str, Vec<&str>, &str)] = &[
        ("no target", vec!["doctor"], "give --url or --profile"),
        (
            "both targets",
            vec!["doctor", "--url", &url, "-p", "prod"],
            "--url and --profile",
        ),
        (
            "--token with --url",
            vec!["doctor", "--url", &url, "--token", "flt_flagvalue"],
            "--token cannot be used",
        ),
        (
            "--token with --profile",
            vec!["doctor", "-p", "prod", "--token", "flt_flagvalue"],
            "--token cannot be used",
        ),
        (
            "-c with --url",
            vec!["doctor", "--url", &url, "-c", config],
            "-c cannot be used with --url",
        ),
        (
            "two key sources",
            vec![
                "doctor",
                "--url",
                &url,
                "--token-env",
                "SOME_KEY",
                "--token-file",
                key,
            ],
            "--token-env and --token-file",
        ),
        (
            "malformed --url",
            vec!["doctor", "--url", "localhost:5514"],
            "--url must be an http or https URL",
        ),
        (
            "plain http --web-url off loopback",
            vec!["doctor", "--url", &url, "--web-url", "http://trawl.example"],
            "--web-url must use https",
        ),
    ];
    for (case, args, fragment) in cases {
        let output = home.trawl(args, &[]);
        assert_usage_error(
            &output,
            fragment,
            &["flt_flagvalue", "flt_keyfilevalue"],
            case,
        );
    }
    recorder.assert_untouched("selection usage errors");
}

/// A URL carrying userinfo would reach the server as a Basic
/// `Authorization` header. `--url` and `--web-url` refuse it as a usage
/// error; a profile's url fails `connection.config`. The userinfo never
/// appears in any output.
#[test]
fn doctor_refuses_url_credentials() {
    let home = Sandbox::new();
    let recorder = Recorder::start();
    let port = recorder.port;
    let forms = [
        (
            "username only",
            format!("https://tr4wluser@127.0.0.1:{port}"),
        ),
        (
            "user and password",
            format!("https://tr4wluser:pa55word@127.0.0.1:{port}"),
        ),
    ];
    let secrets = ["tr4wluser", "pa55word"];
    for (form, with_creds) in &forms {
        let output = home.trawl(&["doctor", "--url", with_creds], &[]);
        assert_usage_error(&output, "--url carries credentials", &secrets, form);

        let output = home.trawl(
            &[
                "doctor",
                "--url",
                &recorder.url("https"),
                "--web-url",
                with_creds,
            ],
            &[],
        );
        assert_usage_error(&output, "--web-url carries credentials", &secrets, form);

        home.default_config(&format!("[profiles.creds]\nurl = \"{with_creds}\"\n"));
        let output = home.trawl(&["doctor", "-p", "creds", "--format", "json"], &[]);
        assert_eq!(output.status.code(), Some(1), "{form}: {}", text(&output));
        let report = report(&output);
        let check = connection_config(&report);
        assert_eq!(check["outcome"], "failed", "{form}");
        assert_eq!(check["reason"], "URL carries credentials", "{form}");
        assert!(
            check["source"]
                .as_str()
                .unwrap()
                .contains("[profiles.creds]"),
            "{form}: names the profile"
        );
        assert!(report["target"]["origin"].is_null());
        for secret in secrets {
            assert!(!text(&output).contains(secret), "{form}: {secret} leaked");
        }
    }
    recorder.assert_untouched("credentials in a URL");
}

/// Request paths are appended to the target URL, so a query or fragment
/// would carry every request, the keyed `whoami` included, to wherever the
/// URL's path points. `--url` and `--web-url` refuse one as a usage error
/// naming the flag; a profile's url fails `connection.config` naming the
/// profile. Empty and percent-encoded forms are refused alike, the URL's
/// text never appears in any output, and nothing reaches the listener the
/// URLs name.
#[test]
fn doctor_refuses_url_query_and_fragment() {
    let home = Sandbox::new();
    let recorder = Recorder::start();
    let base = recorder.url("https");
    let key = home.file("key", &format!("{PROFILE_TOKEN}\n"));
    let key = key.to_str().unwrap();
    let forms = [
        ("fragment", format!("{base}/x#fr4gment")),
        ("bare fragment", format!("{base}#fr4gment")),
        ("query", format!("{base}/x?qu3ry=1")),
        ("bare query", format!("{base}?qu3ry=1")),
        ("empty query", format!("{base}/x?")),
        ("empty fragment", format!("{base}/x#")),
        ("encoded fragment", format!("{base}/x%23fr4gment")),
        ("encoded query", format!("{base}/x%3Fqu3ry=1")),
    ];
    let secrets = ["fr4gment", "qu3ry", "/x"];
    for (form, raw) in &forms {
        let output = home.trawl(
            &[
                "doctor",
                "--url",
                raw,
                "--token-file",
                key,
                "--format",
                "json",
            ],
            &[],
        );
        assert_usage_error(&output, "--url has a query or fragment", &secrets, form);

        let output = home.trawl(&["doctor", "--url", &base, "--web-url", raw], &[]);
        assert_usage_error(&output, "--web-url has a query or fragment", &secrets, form);

        home.default_config(&format!(
            "[profiles.odd]\nurl = \"{raw}\"\ntoken = \"{PROFILE_TOKEN}\"\n"
        ));
        let output = home.trawl(&["doctor", "-p", "odd", "--format", "json"], &[]);
        assert_eq!(output.status.code(), Some(1), "{form}: {}", text(&output));
        let report = report(&output);
        let check = connection_config(&report);
        assert_eq!(check["outcome"], "failed", "{form}");
        assert_eq!(check["reason"], "URL has a query or fragment", "{form}");
        assert!(
            check["source"].as_str().unwrap().contains("[profiles.odd]"),
            "{form}: names the profile"
        );
        assert!(report["target"]["origin"].is_null(), "{form}");
        let all = text(&output);
        for secret in secrets.iter().chain([&PROFILE_TOKEN]) {
            assert!(!all.contains(secret), "{form}: {secret} leaked: {all}");
        }
    }
    recorder.assert_untouched("a query or fragment in a URL");
}

/// Under a profile, every flag that would change its URL, key, or trust is
/// refused by name.
#[test]
fn doctor_profile_refuses_overrides() {
    let home = Sandbox::new();
    let recorder = Recorder::start();
    let url = recorder.url("https");
    home.default_config(&format!(
        "[profiles.prod]\nurl = \"{url}\"\ntoken = \"{SAVED_TOKEN}\"\n"
    ));
    let key = home.file("key", "flt_keyfilevalue\n");
    let key = key.to_str().unwrap();
    for profile in ["prod", "trial"] {
        let cases: [(&str, Vec<&str>); 5] = [
            ("--url", vec!["--url", &url]),
            ("--token", vec!["--token", "flt_flagvalue"]),
            ("--insecure", vec!["--insecure"]),
            ("--token-env", vec!["--token-env", "SOME_KEY"]),
            ("--token-file", vec!["--token-file", key]),
        ];
        for (flag, extra) in cases {
            let mut args = vec!["doctor", "-p", profile];
            args.extend(extra);
            let output = home.trawl(&args, &[]);
            let case = format!("-p {profile} {flag}");
            assert_usage_error(
                &output,
                flag,
                &["flt_flagvalue", "flt_keyfilevalue", SAVED_TOKEN],
                &case,
            );
        }
    }
    recorder.assert_untouched("profile overrides");
}

/// `--url` never reads config.toml: neither the `[server].token` nor the
/// `insecure` saved there is used. The server, trusted through system roots
/// (`SSL_CERT_FILE` names the test CA), answers health, and sees no
/// `Authorization` header: identity is `not_configured`, and the run passes.
#[test]
fn doctor_url_ignores_saved_token() {
    let home = Sandbox::new();
    let authority = ca("trawl doctor test CA");
    let roots = home.file("roots.pem", &authority.pem());
    let stub = Stub::tls(leaf(&authority), vec![healthy(), whoami(r#""query""#)]);
    let url = stub.url("https");
    home.default_config(&format!(
        "[server]\nurl = \"{url}\"\ntoken = \"{SAVED_TOKEN}\"\ninsecure = true\n"
    ));

    let output = home.trawl(
        &["doctor", "--url", &url, "--format", "json"],
        &[("SSL_CERT_FILE", roots.to_str().unwrap())],
    );
    assert!(!text(&output).contains(SAVED_TOKEN), "{}", text(&output));
    let report = report(&output);
    let check = connection_config(&report);
    assert_eq!(check["outcome"], "complete", "{}", text(&output));
    assert_eq!(check["source"], "--url flag");
    assert_eq!(
        check["detail"], "trust: system roots; key: none selected",
        "neither the saved token nor the saved insecure is used"
    );
    assert_eq!(report["target"]["origin"], url);
    assert_eq!(report["vantage"], "client");
    let tls = check_by_id(&report, "api.tls");
    assert_eq!(tls["outcome"], "complete", "{report}");
    assert_eq!(tls["detail"], "verified under system roots");
    let identity = check_by_id(&report, "api.identity");
    assert_eq!(identity["outcome"], "not_configured");
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));

    let requests = stub.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(requests[0].path, HEALTH_PATH);
    stub.assert_no_authorization("saved token");
}

/// Case name, extra flags, extra environment, expected reason suffix.
type KeyCase<'a> = (&'a str, Vec<&'a str>, Vec<(&'a str, &'a str)>, &'a str);

/// `--token-env` and `--token-file` select the key; a missing or empty
/// source fails `connection.config` naming the flag, not the source's name
/// or path.
#[test]
fn doctor_url_key_sources() {
    let home = Sandbox::new();
    let recorder = Recorder::start();
    let url = recorder.url("https");
    // A run whose key resolves goes on to contact its target, so those runs
    // aim at a port with nothing on it and the recorder sees only failures.
    let closed = format!("https://127.0.0.1:{}", closed_port());
    let good = home.file("good.key", "flt_keyfilevalue\n");
    let empty = home.file("empty.key", "\n");
    let absent = home.home.join("absent.key");

    let run_at = |target: &str, extra: &[&str], env: &[(&str, &str)]| {
        let mut args = vec!["doctor", "--url", target, "--format", "json"];
        args.extend_from_slice(extra);
        home.trawl(&args, env)
    };
    let run = |extra: &[&str], env: &[(&str, &str)]| run_at(&url, extra, env);

    let output = run_at(&closed, &["--token-file", good.to_str().unwrap()], &[]);
    let check = connection_config(&report(&output)).clone();
    assert_eq!(check["outcome"], "complete");
    assert!(
        check["detail"]
            .as_str()
            .unwrap()
            .ends_with("key: key from --token-file")
    );
    assert!(!text(&output).contains("flt_keyfilevalue"));

    let output = run_at(
        &closed,
        &["--token-env", "DOCTOR_KEY"],
        &[("DOCTOR_KEY", "flt_envvalue")],
    );
    let check = connection_config(&report(&output)).clone();
    assert_eq!(check["outcome"], "complete");
    assert!(
        check["detail"]
            .as_str()
            .unwrap()
            .ends_with("key: key from --token-env")
    );
    assert!(!text(&output).contains("flt_envvalue"));

    let failures: [KeyCase<'_>; 4] = [
        (
            "unset variable",
            vec!["--token-env", "DOCTOR_KEY_ABSENT"],
            vec![],
            "is not set",
        ),
        (
            "empty variable",
            vec!["--token-env", "DOCTOR_KEY"],
            vec![("DOCTOR_KEY", "  ")],
            "is empty",
        ),
        (
            "missing file",
            vec!["--token-file", absent.to_str().unwrap()],
            vec![],
            "does not exist",
        ),
        (
            "empty file",
            vec!["--token-file", empty.to_str().unwrap()],
            vec![],
            "is empty",
        ),
    ];
    for (case, extra, env, fragment) in failures {
        let output = run(&extra, &env);
        assert_eq!(output.status.code(), Some(1), "{case}: {}", text(&output));
        let report = report(&output);
        let check = connection_config(&report);
        assert_eq!(check["outcome"], "failed", "{case}");
        assert!(
            check["reason"].as_str().unwrap().ends_with(fragment),
            "{case}: {check}"
        );
        assert!(
            check["source"]
                .as_str()
                .unwrap()
                .starts_with("key from --token-")
        );
        let all = text(&output);
        assert!(
            !all.contains("DOCTOR_KEY"),
            "{case}: the name is not echoed"
        );
        assert!(
            !all.contains(&*home.home.to_string_lossy()),
            "{case}: no path"
        );
        assert_eq!(report["verdict"], "fail");
    }
    recorder.assert_untouched("key sources");
}

/// A missing config file, a missing profile, or a profile with no url fails
/// `connection.config` naming what is missing. `[server]` points at the
/// recorder, so a fallback to it would show as a connection.
#[test]
fn doctor_missing_profile_fails_without_contact() {
    let home = Sandbox::new();
    let recorder = Recorder::start();
    let url = recorder.url("https");

    // No config file at all.
    let output = home.trawl(&["doctor", "-p", "prod", "--format", "json"], &[]);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    let report_json = report(&output);
    let check = connection_config(&report_json);
    assert_eq!(check["outcome"], "failed");
    assert_eq!(check["reason"], "the config file does not exist");
    assert_eq!(check["source"], "config file ~/.config/trawl/config.toml");
    assert!(report_json["target"]["origin"].is_null());
    assert_eq!(
        report_json["target"]["source"],
        "CLI profile `prod` in ~/.config/trawl/config.toml"
    );

    // A named config file that does not exist.
    let absent = home.home.join("absent.toml");
    let output = home.trawl(
        &[
            "doctor",
            "-p",
            "prod",
            "-c",
            absent.to_str().unwrap(),
            "--format",
            "json",
        ],
        &[],
    );
    let check = connection_config(&report(&output)).clone();
    assert_eq!(check["reason"], "the config file does not exist");
    assert_eq!(check["source"], format!("config file {}", absent.display()));

    // `lab` resolves and goes on to contact its own url, a closed port, so
    // the recorder at `[server].url` still sees nothing.
    let lab = format!("https://127.0.0.1:{}", closed_port());
    home.default_config(&format!(
        "[server]\nurl = \"{url}\"\ntoken = \"{SAVED_TOKEN}\"\n\n\
         [profiles.lab]\nurl = \"{lab}\"\n\n[profiles.nourl]\ninsecure = false\n"
    ));
    let cases = [
        ("prod", "the config file has no [profiles.prod]"),
        ("nourl", "the profile sets no url"),
    ];
    for (profile, reason) in cases {
        let output = home.trawl(&["doctor", "-p", profile, "--format", "json"], &[]);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{profile}: {}",
            text(&output)
        );
        let report = report(&output);
        let check = connection_config(&report);
        assert_eq!(check["outcome"], "failed", "{profile}");
        assert_eq!(check["reason"], reason, "{profile}");
        assert!(report["target"]["origin"].is_null());
        assert!(!text(&output).contains(SAVED_TOKEN));
    }
    let output = home.trawl(&["doctor", "-p", "prod", "--format", "table"], &[]);
    let table = text(&output);
    assert!(table.contains("pick one of: lab, nourl"), "{table}");

    // A profile without a token of its own selects no key: [server].token
    // is never inherited.
    let output = home.trawl(&["doctor", "-p", "lab", "--format", "json"], &[]);
    let check = connection_config(&report(&output)).clone();
    assert_eq!(check["outcome"], "complete", "{}", text(&output));
    assert!(
        check["detail"]
            .as_str()
            .unwrap()
            .ends_with("key: none selected")
    );

    recorder.assert_untouched("missing profile");
}

/// `-p trial` resolves through the trial's reserved profile rules: the
/// trial's recorded port, its operator key, and its pinned certificate. The
/// trial's API is a TLS stub serving that certificate, so the pin verifies,
/// and only then does the operator key reach `whoami`.
#[test]
fn doctor_trial_profile() {
    const TRIAL_TOKEN: &str = "flt_trialoperatortoken";
    let home = Sandbox::new();
    let (cert, identity) = trial_certificate();
    let stub = Stub::tls(
        identity,
        vec![healthy(), whoami(r#""query","server_manage""#)],
    );
    let trial_dir = write_trial(&home.state_home, stub.port, TRIAL_TOKEN, &cert);
    home.default_config(&format!(
        "[server]\nurl = \"https://elsewhere.example:5514\"\ntoken = \"{SAVED_TOKEN}\"\ninsecure = true\n"
    ));

    let output = home.trawl(&["doctor", "-p", "trial", "--format", "json"], &[]);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    let report_json = report(&output);
    let check = connection_config(&report_json);
    assert_eq!(check["outcome"], "complete");
    assert_eq!(
        check["detail"],
        "trust: pinned CA from the trial's certificate; key: the trial's operator key"
    );
    assert_eq!(report_json["target"]["source"], "trial profile (-p trial)");
    assert_eq!(
        report_json["target"]["origin"],
        format!("https://127.0.0.1:{}", stub.port)
    );
    assert_eq!(
        check_by_id(&report_json, "api.tls")["detail"],
        "verified under the pinned CA"
    );
    let identity = check_by_id(&report_json, "api.identity");
    assert_eq!(identity["outcome"], "complete");
    assert_eq!(identity["source"], "the trial's operator key");
    let all = text(&output);
    assert!(
        !all.contains(TRIAL_TOKEN) && !all.contains(SAVED_TOKEN) && !all.contains("pfx12345"),
        "{all}"
    );
    let requests = stub.requests();
    let paths: Vec<&str> = requests.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(paths, [HEALTH_PATH, WHOAMI_PATH]);
    assert_eq!(requests[0].authorization, None, "health carries no key");
    assert_eq!(
        requests[1].authorization.as_deref(),
        Some(format!("Bearer {TRIAL_TOKEN}").as_str()),
        "whoami carries the trial's operator key and nothing else"
    );
    let contacted = requests.len();

    // A user profile named `trial` is refused through the trial's own rule.
    let own = home.file("own.toml", "[profiles.trial]\nurl = \"https://x:1\"\n");
    let output = home.trawl(
        &[
            "doctor",
            "-p",
            "trial",
            "-c",
            own.to_str().unwrap(),
            "--format",
            "json",
        ],
        &[],
    );
    let check = connection_config(&report(&output)).clone();
    assert_eq!(check["outcome"], "failed");
    assert!(
        check["reason"]
            .as_str()
            .unwrap()
            .contains("[profiles.trial]")
    );

    // No trial: connection.config fails and names the fix.
    std::fs::remove_dir_all(&trial_dir).unwrap();
    let output = home.trawl(&["doctor", "-p", "trial", "--format", "json"], &[]);
    assert_eq!(output.status.code(), Some(1));
    let check = connection_config(&report(&output)).clone();
    assert_eq!(check["reason"], "there is no trial on this machine");
    assert_eq!(check["next_action"], "start one with `trawl trial up`");

    assert_eq!(
        stub.requests().len(),
        contacted,
        "a refused or missing trial contacts nothing"
    );
}

/// The shape of certificate `trawld` generates for a trial: self-signed,
/// pinned as its own root.
fn trial_certificate() -> (String, Identity) {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).expect("self-signed");
    let identity = Identity {
        chain: vec![cert.der().clone()],
        key: PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into(),
    };
    (cert.pem(), identity)
}

/// A finished trial directory under `state_home`, in the shape
/// `tests/trial_profile.rs` builds, pinning `ca_pem`. Returns the trial
/// directory.
fn write_trial(state_home: &Path, api_port: u16, token: &str, ca_pem: &str) -> PathBuf {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};

    let dir = state_home.join("trawl/trial");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .unwrap();
    let state = serde_json::json!({
        "schema": 1,
        "trial_id": "0123456789abcdef0123456789abcdef",
        "project": "trawl-trial",
        "cli_version": "0.9.0",
        "created_at": "2026-09-25T12:00:00Z",
        "engine_id": "ENGINE:ID",
        "ports": {"api": api_port, "web": 18090},
        "images": {
            "trawl": {"reference": "ghcr.io/jakub/trawl:0.9.0", "id": "sha256:aa", "repo_digest": null},
            "postgres": {"reference": "postgres:18", "id": "sha256:bb", "repo_digest": null},
            "trawl_overridden": false
        },
        "phases": {"database": true, "fleet_migrated": true, "tls": true, "services_verified": true},
        "tls": {"sha256_fingerprint": "AB:CD"},
        "keys": {
            "operator": {"name": "trial-operator", "prefix": "pfx00001"},
            "ingest": {"name": "trial-ingest", "prefix": "pfx00002"}
        },
        "samples": {"state": "skipped"}
    });
    let write = |name: &str, bytes: &[u8], mode: u32| {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    };
    write("state.json", state.to_string().as_bytes(), 0o600);
    write("operator.token", format!("{token}\n").as_bytes(), 0o600);
    write("ca.pem", ca_pem.as_bytes(), 0o644);
    dir
}

/// Other commands keep exiting as before: success is 0, any error is 1.
#[test]
fn other_commands_keep_their_exit_status() {
    let home = Sandbox::new();
    let ok = home.trawl(&["validate", "* | head 1"], &[]);
    assert_eq!(ok.status.code(), Some(0), "{}", text(&ok));
    let bad = home.trawl(&["validate", "* | nosuchcommand"], &[]);
    assert_eq!(bad.status.code(), Some(1), "{}", text(&bad));
    let refused = home.trawl(&["-p", "trial", "driver", "status"], &[]);
    assert_eq!(refused.status.code(), Some(1), "{}", text(&refused));
}

const PROFILE_TOKEN: &str = "flt_profiletokenvalue";

/// Write `[profiles.NAME]` with `url`, `token`, and one trust line.
fn profile(home: &Sandbox, name: &str, url: &str, trust: &str) {
    home.default_config(&format!(
        "[profiles.{name}]\nurl = \"{url}\"\ntoken = \"{PROFILE_TOKEN}\"\n{trust}\n"
    ));
}

/// Wait out any late handshake, then return them all.
fn settled_handshakes(stub: &Stub) -> Vec<bool> {
    stub.handshakes(1);
    std::thread::sleep(Duration::from_millis(200));
    stub.handshakes(1)
}

/// A certificate the client does not trust fails `api.tls`, naming the
/// trust mode and pointing at `ca_cert`; health is blocked by it, and
/// identity by health. From the server's end, the handshake failed and no
/// request arrived, so no bearer header was ever sent. Checked under system
/// roots (`--url`) and under a pin to another CA (a profile).
#[test]
fn doctor_untrusted_cert_sends_no_key() {
    let home = Sandbox::new();
    let server_ca = ca("trawl doctor server CA");
    let other_ca = ca("trawl doctor other CA");
    let key = home.file("key", &format!("{PROFILE_TOKEN}\n"));
    let other_pem = home.file("other.pem", &other_ca.pem());

    let cases = [
        (
            "--url under system roots",
            "system roots",
            "in a CLI profile",
        ),
        (
            "profile pinned to another CA",
            "the pinned CA",
            "point ca_cert",
        ),
    ];
    for (case, mode, next) in cases {
        let stub = Stub::tls(leaf(&server_ca), vec![healthy(), whoami(r#""query""#)]);
        let output = if mode == "system roots" {
            home.trawl(
                &[
                    "doctor",
                    "--url",
                    &stub.url("https"),
                    "--token-file",
                    key.to_str().unwrap(),
                    "--format",
                    "json",
                ],
                &[],
            )
        } else {
            profile(
                &home,
                "pinned",
                &stub.url("https"),
                &format!("ca_cert = \"{}\"", other_pem.display()),
            );
            home.trawl(&["doctor", "-p", "pinned", "--format", "json"], &[])
        };
        assert_eq!(output.status.code(), Some(1), "{case}: {}", text(&output));
        let report = report(&output);
        let tls = check_by_id(&report, "api.tls");
        assert_eq!(tls["outcome"], "failed", "{case}");
        assert_eq!(
            tls["reason"],
            format!("certificate not trusted under {mode}"),
            "{case}"
        );
        let next_action = tls["next_action"].as_str().unwrap();
        assert!(
            next_action.contains("ca_cert") && next_action.contains(next),
            "{case}: {next_action}"
        );
        assert_eq!(
            rows(&report)[1..],
            [
                (
                    "api.transport".to_owned(),
                    "complete".to_owned(),
                    None,
                    None
                ),
                (
                    "api.tls".to_owned(),
                    "failed".to_owned(),
                    Some(format!("certificate not trusted under {mode}")),
                    None
                ),
                (
                    "api.health".to_owned(),
                    "not_sampled".to_owned(),
                    Some("blocked".to_owned()),
                    Some("api.tls".to_owned())
                ),
                (
                    "api.identity".to_owned(),
                    "not_sampled".to_owned(),
                    Some("blocked".to_owned()),
                    Some("api.health".to_owned())
                ),
            ],
            "{case}"
        );
        assert!(!text(&output).contains(PROFILE_TOKEN), "{case}");

        assert_eq!(
            settled_handshakes(&stub),
            [false],
            "{case}: one handshake, failed, and no retry"
        );
        assert!(
            stub.requests().is_empty(),
            "{case}: no request reached the server"
        );
    }
}

/// `insecure` and a plain `http` URL each fail `api.tls`, which blocks
/// health and, through it, identity. The one request the server sees is
/// the unkeyed health probe that decided `api.transport`; no
/// `Authorization` header arrives.
#[test]
fn doctor_insecure_never_sends_key() {
    let home = Sandbox::new();
    let server_ca = ca("trawl doctor server CA");
    let key = home.file("key", &format!("{PROFILE_TOKEN}\n"));
    let key = key.to_str().unwrap();

    let cases = [
        ("profile insecure = true", "certificate not verified"),
        ("--url --insecure", "certificate not verified"),
        ("--url over plain http", "connection is not TLS"),
    ];
    for (case, reason) in cases {
        let routes = vec![healthy(), whoami(r#""query""#)];
        let stub = if case.contains("plain http") {
            Stub::plain(routes)
        } else {
            Stub::tls(leaf(&server_ca), routes)
        };
        let output = match case {
            "profile insecure = true" => {
                profile(&home, "lax", &stub.url("https"), "insecure = true");
                home.trawl(&["doctor", "-p", "lax", "--format", "json"], &[])
            }
            "--url --insecure" => home.trawl(
                &[
                    "doctor",
                    "--url",
                    &stub.url("https"),
                    "--insecure",
                    "--token-file",
                    key,
                    "--format",
                    "json",
                ],
                &[],
            ),
            _ => home.trawl(
                &[
                    "doctor",
                    "--url",
                    &stub.url("http"),
                    "--token-file",
                    key,
                    "--format",
                    "json",
                ],
                &[],
            ),
        };
        assert_eq!(output.status.code(), Some(1), "{case}: {}", text(&output));
        let report = report(&output);
        assert_eq!(
            rows(&report)[1..],
            [
                (
                    "api.transport".to_owned(),
                    "complete".to_owned(),
                    None,
                    None
                ),
                (
                    "api.tls".to_owned(),
                    "failed".to_owned(),
                    Some(reason.to_owned()),
                    None
                ),
                (
                    "api.health".to_owned(),
                    "not_sampled".to_owned(),
                    Some("blocked".to_owned()),
                    Some("api.tls".to_owned())
                ),
                (
                    "api.identity".to_owned(),
                    "not_sampled".to_owned(),
                    Some("blocked".to_owned()),
                    Some("api.health".to_owned())
                ),
            ],
            "{case}"
        );
        assert!(!text(&output).contains(PROFILE_TOKEN), "{case}");
        let requests = stub.requests();
        let paths: Vec<&str> = requests.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, [HEALTH_PATH], "{case}: only the unkeyed probe");
        stub.assert_no_authorization(case);
    }
}

/// The key goes only to a server that answered the unkeyed probe with
/// trawl's health body. Under verified TLS, a redirect, a 404, a 503 that
/// is not a health body, a 200 that is not one, and a 200 health body
/// without trawl's `checks` map or `version` all fail `api.health`, and
/// `api.identity` is blocked by it. The server sees the one unkeyed probe:
/// no `whoami`, no `Authorization` header.
#[test]
fn doctor_sends_key_only_after_trawl_health() {
    let cases: [(&str, Option<Route>, String); 9] = [
        (
            "301",
            Some((HEALTH_PATH, 301, String::new())),
            "redirect refused".to_owned(),
        ),
        (
            "302",
            Some((HEALTH_PATH, 302, String::new())),
            "redirect refused".to_owned(),
        ),
        ("404", None, "HTTP 404 is not a health answer".to_owned()),
        (
            "503 foreign body",
            Some((HEALTH_PATH, 503, "<html>maintenance</html>".to_owned())),
            "HTTP 503 is not a health answer".to_owned(),
        ),
        (
            "200 foreign JSON",
            Some((HEALTH_PATH, 200, r#"{"hello":"world"}"#.to_owned())),
            "the answer is not a trawl health response".to_owned(),
        ),
        (
            "200 unknown status",
            Some((HEALTH_PATH, 200, r#"{"status":"fine"}"#.to_owned())),
            "the answer is not a trawl health response".to_owned(),
        ),
        (
            "200 status alone",
            Some((HEALTH_PATH, 200, r#"{"status":"ok"}"#.to_owned())),
            "not a trawl health answer".to_owned(),
        ),
        (
            "200 without version",
            Some((
                HEALTH_PATH,
                200,
                r#"{"status":"ok","checks":{"duckdb":"ok"}}"#.to_owned(),
            )),
            "not a trawl health answer".to_owned(),
        ),
        (
            "200 without checks",
            Some((
                HEALTH_PATH,
                200,
                r#"{"status":"ok","version":"0.9.0"}"#.to_owned(),
            )),
            "not a trawl health answer".to_owned(),
        ),
    ];
    assert_health_withholds_key(cases);
}

/// trawld answers health with exactly 200 or 503, and pairs 200 with `ok`
/// or `degraded` and 503 with `unavailable`. A valid, signed health body
/// under another 2xx, or under the status it is not paired with, fails
/// `api.health` and blocks `api.identity`: no `whoami`, no `Authorization`
/// header.
#[test]
fn doctor_health_status_must_match_its_body() {
    let signed = |status: &str| {
        format!(r#"{{"status":"{status}","checks":{{"duckdb":"ok"}},"version":"0.9.0"}}"#)
    };
    let disagree = || "status and body disagree".to_owned();
    assert_health_withholds_key([
        (
            "201 valid body",
            Some((HEALTH_PATH, 201, signed("ok"))),
            "HTTP 201 is not a health answer".to_owned(),
        ),
        ("503 ok", Some((HEALTH_PATH, 503, signed("ok"))), disagree()),
        (
            "503 degraded",
            Some((HEALTH_PATH, 503, signed("degraded"))),
            disagree(),
        ),
        (
            "200 unavailable",
            Some((HEALTH_PATH, 200, signed("unavailable"))),
            disagree(),
        ),
    ]);
}

/// For each `(case, health route, reason)`: under verified TLS, the doctor
/// fails `api.health` with `reason`, blocks `api.identity`, exits 1, and
/// the stub sees only the unkeyed probe.
fn assert_health_withholds_key<const N: usize>(cases: [(&str, Option<Route>, String); N]) {
    let home = Sandbox::new();
    let server_ca = ca("trawl doctor server CA");
    let pem = home.file("ca.pem", &server_ca.pem());
    for (case, health, reason) in cases {
        let mut routes = vec![whoami(r#""query""#)];
        routes.extend(health);
        let stub = Stub::tls(leaf(&server_ca), routes);
        profile(
            &home,
            "prod",
            &stub.url("https"),
            &format!("ca_cert = \"{}\"", pem.display()),
        );
        let output = home.trawl(&["doctor", "-p", "prod", "--format", "json"], &[]);
        assert_eq!(output.status.code(), Some(1), "{case}: {}", text(&output));
        let report = report(&output);
        assert_eq!(
            rows(&report)[1..],
            [
                (
                    "api.transport".to_owned(),
                    "complete".to_owned(),
                    None,
                    None
                ),
                ("api.tls".to_owned(), "complete".to_owned(), None, None),
                (
                    "api.health".to_owned(),
                    "failed".to_owned(),
                    Some(reason),
                    None
                ),
                (
                    "api.identity".to_owned(),
                    "not_sampled".to_owned(),
                    Some("blocked".to_owned()),
                    Some("api.health".to_owned())
                ),
            ],
            "{case}"
        );
        assert!(!text(&output).contains(PROFILE_TOKEN), "{case}");
        let requests = stub.requests();
        let paths: Vec<&str> = requests.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, [HEALTH_PATH], "{case}: only the unkeyed probe");
        stub.assert_no_authorization(case);
    }
}

/// The three answers trawld sends, 200 with `ok` or `degraded` and 503
/// with `unavailable`, each complete `api.health` and send the key to
/// `whoami`.
#[test]
fn doctor_accepts_the_health_answers_trawld_sends() {
    for (http, status) in [(200, "ok"), (200, "degraded"), (503, "unavailable")] {
        let body = format!(
            r#"{{"status":"{status}","checks":{{"duckdb":"ok"}},"version":"{}"}}"#,
            env!("CARGO_PKG_VERSION")
        );
        let (output, stub) = doctor_against(vec![(HEALTH_PATH, http, body), whoami(r#""query""#)]);
        let case = format!("{http} {status}");
        let report = report(&output);
        let health = check_by_id(&report, "api.health");
        assert_eq!(health["outcome"], "complete", "{case}: {health}");
        assert_eq!(
            health["detail"],
            format!(
                "status: {status}; server version {}",
                env!("CARGO_PKG_VERSION")
            ),
            "{case}"
        );
        assert_eq!(
            check_by_id(&report, "api.identity")["outcome"],
            "complete",
            "{case}"
        );
        let requests = stub.requests();
        let paths: Vec<&str> = requests.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, [HEALTH_PATH, WHOAMI_PATH], "{case}");
        assert_eq!(requests[0].authorization, None, "{case}");
        assert!(requests[1].authorization.is_some(), "{case}");
    }
}

/// A 503 keeps its per-check body. Rows are sorted by name: `ok` is
/// complete (the unknown `corpus` included), `error` and `refusing` fail, an
/// unrecognized value fails and is shown when it is a plain identifier, and
/// a name that is not one becomes `api.health._invalid` without being
/// echoed. A server check named `_invalid` is not one either, so it cannot
/// collide with that row. The run exits 1.
#[test]
fn doctor_health_rows_map_values() {
    let home = Sandbox::new();
    let server_ca = ca("trawl doctor server CA");
    let pem = home.file("ca.pem", &server_ca.pem());
    let body = r#"{"status":"unavailable","checks":{"duckdb":"error","corpus":"ok","auth_db":"ok","data_path":"recovering","storage_db":"Weird Value!","ingest_capacity":"refusing","Bad-Key":"ok","_invalid":"ok"},"version":"9.9.9"}"#;
    let stub = Stub::tls(
        leaf(&server_ca),
        vec![(HEALTH_PATH, 503, body.to_owned()), whoami(r#""query""#)],
    );
    profile(
        &home,
        "prod",
        &stub.url("https"),
        &format!("ca_cert = \"{}\"", pem.display()),
    );
    let output = home.trawl(&["doctor", "-p", "prod", "--format", "json"], &[]);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    let report = report(&output);
    let health: Vec<(String, String, Option<String>)> = rows(&report)
        .into_iter()
        .filter(|(id, ..)| id.starts_with("api.health"))
        .map(|(id, outcome, reason, _)| (id, outcome, reason))
        .collect();
    let own = |id: &str, outcome: &str, reason: Option<&str>| {
        (id.to_owned(), outcome.to_owned(), reason.map(str::to_owned))
    };
    assert_eq!(
        health,
        [
            own("api.health", "complete", None),
            own("api.health.auth_db", "complete", None),
            own("api.health.corpus", "complete", None),
            own(
                "api.health.data_path",
                "failed",
                Some("reported recovering, a value this CLI does not know")
            ),
            own("api.health.duckdb", "failed", Some("reported error")),
            own(
                "api.health.ingest_capacity",
                "failed",
                Some("reported refusing")
            ),
            own(
                "api.health.storage_db",
                "failed",
                Some("unrecognized value")
            ),
            own(
                "api.health._invalid",
                "failed",
                Some("the server reported a check name that is not [a-z][a-z0-9_]{0,63}")
            ),
        ]
    );
    assert_eq!(
        check_by_id(&report, "api.health")["detail"],
        "status: unavailable; server version 9.9.9"
    );
    assert_eq!(check_by_id(&report, "api.identity")["outcome"], "complete");
    assert_eq!(report["verdict"], "fail");
    let all = text(&output);
    assert!(!all.contains("Bad-Key") && !all.contains("Weird"), "{all}");
    assert!(
        report["notes"][0]
            .as_str()
            .unwrap()
            .ends_with("the server reports version 9.9.9"),
        "{report}"
    );
}

/// A server that echoes the key back never gets it into the report. The
/// stub's whoami puts the bearer token in the key's name and in a
/// permission (with a control character spliced in), and the key's prefix
/// (the 8 characters after `flt_`) in the name; its health answer uses the
/// prefix as a check name and a piece of the key as a value and a version.
/// Neither the key nor its prefix appears in the text or the JSON report.
#[test]
fn doctor_redacts_the_key_from_remote_fields() {
    let home = Sandbox::new();
    let server_ca = ca("trawl doctor server CA");
    let pem = home.file("ca.pem", &server_ca.pem());
    let body = PROFILE_TOKEN.strip_prefix("flt_").unwrap();
    let prefix = &body[..8];
    let spliced = format!("{}\\u001b{}", &PROFILE_TOKEN[..9], &PROFILE_TOKEN[9..]);
    let health = format!(
        r#"{{"status":"ok","checks":{{"duckdb":"ok","{prefix}":"ok","auth_db":"{body}"}},"version":"{PROFILE_TOKEN}"}}"#
    );
    let who = format!(
        r#"{{"prefix":"{prefix}","name":"{PROFILE_TOKEN} owner {prefix}","kind":"service","roles":[],"permissions":["query","{spliced}"]}}"#
    );
    let stub = Stub::tls(
        leaf(&server_ca),
        vec![(HEALTH_PATH, 200, health), (WHOAMI_PATH, 200, who)],
    );
    profile(
        &home,
        "prod",
        &stub.url("https"),
        &format!("ca_cert = \"{}\"", pem.display()),
    );
    for format in ["json", "table"] {
        let output = home.trawl(&["doctor", "-p", "prod", "--format", format], &[]);
        let all = text(&output);
        assert!(!all.contains(prefix), "{format}: the prefix leaked: {all}");
        assert!(!all.contains(body), "{format}: the key leaked: {all}");
        assert!(
            all.contains("[redacted] owner [redacted]"),
            "{format}: {all}"
        );
        if format == "json" {
            let report = report(&output);
            assert_eq!(check_by_id(&report, "api.identity")["outcome"], "complete");
            assert_eq!(
                check_by_id(&report, "api.identity")["detail"],
                "name: [redacted] owner [redacted]; kind: service; permissions: query; 1 unrecognized"
            );
            assert_eq!(
                check_by_id(&report, "api.health.auth_db")["reason"],
                "unrecognized value"
            );
            assert_eq!(
                check_by_id(&report, "api.health._invalid")["outcome"],
                "failed"
            );
        }
    }
}

/// `api.identity`'s detail uses trawl's closed vocabulary. A server that
/// splits the key into pieces too short to redact (7 characters, one
/// fewer than the shortest redacted run), one per permission, gets none
/// of them into the report: only exact permission names are shown, and
/// the rest are counted. The name is the one field shown as sent, capped
/// at 32 characters. The key base64-encoded in the name is not caught:
/// that is the residual risk the redaction site documents, bounded by
/// the cap, and this test pins it rather than promising otherwise.
#[test]
fn doctor_identity_shows_only_trawl_vocabulary() {
    let fragments: Vec<String> = PROFILE_TOKEN
        .as_bytes()
        .chunks(7)
        .map(|piece| String::from_utf8(piece.to_vec()).unwrap())
        .collect();
    let permissions = std::iter::once("\"ingest\"".to_owned())
        .chain(fragments.iter().map(|piece| format!("\"{piece}\"")))
        .chain(["\"query\"".to_owned(), "\"QUERY\"".to_owned()])
        .collect::<Vec<_>>()
        .join(",");
    // base64 of PROFILE_TOKEN.
    let encoded = "Zmx0X3Byb2ZpbGV0b2tlbnZhbHVl";
    let who = format!(
        r#"{{"prefix":"pfx12345","name":"key copy: {encoded}","kind":"human","roles":[],"permissions":[{permissions}]}}"#
    );
    let (output, _stub) = doctor_against(vec![healthy(), (WHOAMI_PATH, 200, who)]);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    let all = text(&output);
    for piece in &fragments {
        assert!(
            !all.contains(piece.as_str()),
            "fragment {piece} leaked: {all}"
        );
    }
    let report = report(&output);
    let identity = check_by_id(&report, "api.identity");
    assert_eq!(identity["outcome"], "complete");
    assert_eq!(
        identity["detail"],
        format!(
            "name: key copy: {}…; kind: human; permissions: query, ingest; {} unrecognized",
            &encoded[..22],
            fragments.len() + 1
        )
    );
}

/// `kind` is one of trawl's principal kinds or the answer is not trawl's:
/// a whoami whose `kind` is the key fails `api.identity` as a foreign
/// answer, and neither the report nor its text quotes the value.
#[test]
fn doctor_unknown_kind_is_a_foreign_answer() {
    let who = format!(
        r#"{{"prefix":"pfx12345","name":"ops-key","kind":"{PROFILE_TOKEN}","roles":[],"permissions":["query"]}}"#
    );
    let (output, _stub) = doctor_against(vec![healthy(), (WHOAMI_PATH, 200, who)]);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    let all = text(&output);
    assert!(!all.contains("profiletoken"), "the key leaked: {all}");
    let report = report(&output);
    let identity = check_by_id(&report, "api.identity");
    assert_eq!(identity["outcome"], "failed");
    assert_eq!(
        identity["reason"],
        "the answer is not a trawl whoami response"
    );
    assert!(identity["detail"].is_null(), "{identity}");
}

/// Run the doctor against a verified-TLS stub serving `routes`, through a
/// profile that pins the stub's CA and holds [`PROFILE_TOKEN`].
fn doctor_against(routes: Vec<Route>) -> (Output, Stub) {
    let home = Sandbox::new();
    let server_ca = ca("trawl doctor server CA");
    let pem = home.file("ca.pem", &server_ca.pem());
    let stub = Stub::tls(leaf(&server_ca), routes);
    profile(
        &home,
        "prod",
        &stub.url("https"),
        &format!("ca_cert = \"{}\"", pem.display()),
    );
    let output = home.trawl(&["doctor", "-p", "prod", "--format", "json"], &[]);
    (output, stub)
}

/// The `api.*` rows of a report, without the ones per health check.
fn api_rows(report: &serde_json::Value) -> Vec<(String, String, Option<String>, Option<String>)> {
    rows(report)
        .into_iter()
        .filter(|(id, ..)| id.starts_with("api.") && !id.starts_with("api.health."))
        .collect()
}

fn own_row(
    id: &str,
    outcome: &str,
    reason: Option<&str>,
    blocked_by: Option<&str>,
) -> (String, String, Option<String>, Option<String>) {
    (
        id.to_owned(),
        outcome.to_owned(),
        reason.map(str::to_owned),
        blocked_by.map(str::to_owned),
    )
}

/// A health answer whose headers arrived under verified TLS and whose body
/// then stalled proves the transport and the certificate; only the health
/// answer went unread. So `api.transport` and `api.tls` are complete,
/// `api.health` is `not_sampled` with `timed_out`, identity is blocked,
/// and no key is sent. The run is incomplete, exit 3.
#[test]
fn doctor_body_stall_is_not_a_transport_timeout() {
    let (output, stub) = doctor_against(vec![
        (HEALTH_PATH, STALL, String::new()),
        whoami(r#""query""#),
    ]);
    assert_eq!(output.status.code(), Some(3), "{}", text(&output));
    let report = report(&output);
    assert_eq!(
        api_rows(&report),
        [
            own_row("api.transport", "complete", None, None),
            own_row("api.tls", "complete", None, None),
            own_row("api.health", "not_sampled", Some("timed_out"), None),
            own_row(
                "api.identity",
                "not_sampled",
                Some("blocked"),
                Some("api.health")
            ),
        ]
    );
    let paths: Vec<String> = stub.requests().into_iter().map(|r| r.path).collect();
    assert_eq!(paths, [HEALTH_PATH]);
    stub.assert_no_authorization("body stall");
}

/// A health answer whose headers arrived under verified TLS and whose body
/// the server then cut short proves the transport and the certificate, and
/// the broken body is an observed failure: `api.health` fails with
/// `response body broken`, not a timeout and not a failed connection.
/// Identity is blocked, no key is sent, and the run fails, exit 1. A
/// whoami body cut short fails `api.identity` the same way.
#[test]
fn doctor_body_cut_short_is_not_a_broken_connection() {
    let (output, stub) = doctor_against(vec![
        (HEALTH_PATH, BROKEN, String::new()),
        whoami(r#""query""#),
    ]);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    let health_report = report(&output);
    assert_eq!(
        api_rows(&health_report),
        [
            own_row("api.transport", "complete", None, None),
            own_row("api.tls", "complete", None, None),
            own_row("api.health", "failed", Some("response body broken"), None),
            own_row(
                "api.identity",
                "not_sampled",
                Some("blocked"),
                Some("api.health")
            ),
        ]
    );
    let paths: Vec<String> = stub.requests().into_iter().map(|r| r.path).collect();
    assert_eq!(paths, [HEALTH_PATH]);
    stub.assert_no_authorization("health body cut short");

    let (output, _stub) = doctor_against(vec![healthy(), (WHOAMI_PATH, BROKEN, String::new())]);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    let report = report(&output);
    assert_eq!(
        api_rows(&report),
        [
            own_row("api.transport", "complete", None, None),
            own_row("api.tls", "complete", None, None),
            own_row("api.health", "complete", None, None),
            own_row("api.identity", "failed", Some("response body broken"), None),
        ]
    );
}

/// trawld's whoami answers exactly 200. Another 2xx with a valid body is
/// not an identity: `api.identity` fails with `unexpected status` and
/// names the status.
#[test]
fn doctor_identity_requires_exactly_200() {
    let body = r#"{"prefix":"pfx12345","name":"ops-key","kind":"human","roles":[],"permissions":["query"]}"#;
    for status in [201, 202] {
        let (output, _stub) =
            doctor_against(vec![healthy(), (WHOAMI_PATH, status, body.to_owned())]);
        assert_eq!(output.status.code(), Some(1), "{status}: {}", text(&output));
        let report = report(&output);
        let identity = check_by_id(&report, "api.identity");
        assert_eq!(identity["outcome"], "failed", "{status}");
        assert_eq!(identity["reason"], "unexpected status", "{status}");
        assert_eq!(
            identity["detail"],
            format!("GET /api/v1/whoami answered HTTP {status}")
        );
    }
}

/// A 200 health or whoami body past the client's cap is not read: the
/// check fails with `response too large`, and the run finishes with a
/// report rather than a crash. An oversized health body blocks identity,
/// so no key goes out. A 503 health body past the cap is too large as
/// well, not an HTTP 503 answer: none of it was judged.
#[test]
fn doctor_oversized_bodies_are_too_large() {
    let pad = "x".repeat(70 * 1024);
    let health = format!(
        r#"{{"status":"ok","checks":{{"duckdb":"ok"}},"version":"{}","pad":"{pad}"}}"#,
        env!("CARGO_PKG_VERSION")
    );
    let (output, stub) = doctor_against(vec![(HEALTH_PATH, 200, health), whoami(r#""query""#)]);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    let health_report = report(&output);
    assert_eq!(
        api_rows(&health_report),
        [
            own_row("api.transport", "complete", None, None),
            own_row("api.tls", "complete", None, None),
            own_row("api.health", "failed", Some("response too large"), None),
            own_row(
                "api.identity",
                "not_sampled",
                Some("blocked"),
                Some("api.health")
            ),
        ]
    );
    stub.assert_no_authorization("oversized health");

    let unavailable = format!(
        r#"{{"status":"unavailable","checks":{{"duckdb":"error"}},"version":"{}","pad":"{pad}"}}"#,
        env!("CARGO_PKG_VERSION")
    );
    let (output, stub) =
        doctor_against(vec![(HEALTH_PATH, 503, unavailable), whoami(r#""query""#)]);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    let unavailable_report = report(&output);
    let health = check_by_id(&unavailable_report, "api.health");
    assert_eq!(health["outcome"], "failed");
    assert_eq!(health["reason"], "response too large");
    stub.assert_no_authorization("oversized 503 health");

    let who = format!(
        r#"{{"prefix":"pfx12345","name":"ops-key","kind":"human","roles":[],"permissions":["query"],"pad":"{pad}"}}"#
    );
    let (output, _stub) = doctor_against(vec![healthy(), (WHOAMI_PATH, 200, who)]);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    let report = report(&output);
    let identity = check_by_id(&report, "api.identity");
    assert_eq!(identity["outcome"], "failed");
    assert_eq!(identity["reason"], "response too large");
}

/// A valid error envelope with `code`, padded with whitespace to 64 KiB
/// (the health and whoami cap) and followed by trailing garbage. Its first
/// 64 KiB parse as the envelope, so only the cap can refuse it.
fn envelope_past_the_cap(code: &str) -> String {
    let mut body = format!(r#"{{"error":{{"code":"{code}","message":"slow down","details":[]}}}}"#);
    let cap = 64 * 1024;
    body.push_str(&" ".repeat(cap - body.len()));
    body.push_str("trailing garbage");
    body
}

/// A non-success health or whoami body past the cap is too large, whatever
/// its status and whatever error envelope its first bytes hold: an
/// oversized 429 is not `rate_limited`, and the run fails, exit 1.
#[test]
fn doctor_oversized_error_bodies_are_too_large() {
    for (status, code) in [
        (429, "rate_limited"),
        (500, "internal_error"),
        (502, "internal_error"),
    ] {
        let (output, stub) = doctor_against(vec![
            (HEALTH_PATH, status, envelope_past_the_cap(code)),
            whoami(r#""query""#),
        ]);
        assert_eq!(output.status.code(), Some(1), "{status}: {}", text(&output));
        let report = report(&output);
        let health = check_by_id(&report, "api.health");
        assert_eq!(health["outcome"], "failed", "{status}");
        assert_eq!(health["reason"], "response too large", "{status}");
        stub.assert_no_authorization("oversized health error");
    }
    for (status, code) in [
        (401, "auth_error"),
        (403, "forbidden"),
        (429, "rate_limited"),
    ] {
        let (output, _stub) = doctor_against(vec![
            healthy(),
            (WHOAMI_PATH, status, envelope_past_the_cap(code)),
        ]);
        assert_eq!(output.status.code(), Some(1), "{status}: {}", text(&output));
        let report = report(&output);
        let identity = check_by_id(&report, "api.identity");
        assert_eq!(identity["outcome"], "failed", "{status}");
        assert_eq!(identity["reason"], "response too large", "{status}");
    }
}

/// The versioned JSON document for a pass, a fail, and an incomplete run,
/// and the text form of the fail. Ports and the CLI's own version are
/// replaced by placeholders, so the snapshots hold only the contract.
#[test]
fn doctor_json_contract() {
    let home = Sandbox::new();
    let server_ca = ca("trawl doctor server CA");
    let pem = home.file("ca.pem", &server_ca.pem());
    let normalize = |stub: &Stub, output: &[u8]| {
        String::from_utf8(output.to_vec())
            .unwrap()
            .replace(&format!("127.0.0.1:{}", stub.port), "127.0.0.1:PORT")
            .replace(env!("CARGO_PKG_VERSION"), "CLI_VERSION")
    };
    let failing_health = r#"{"status":"unavailable","checks":{"duckdb":"error","ingest_capacity":"ok"},"version":"0.0.1-other"}"#;
    let unauthorized =
        r#"{"error":{"code":"unauthorized","message":"invalid API key","details":[]}}"#;
    let cases: [(&str, Vec<Route>, i32); 3] = [
        ("pass", vec![healthy(), whoami(r#""query""#)], 0),
        (
            "fail",
            vec![
                (HEALTH_PATH, 503, failing_health.to_owned()),
                (WHOAMI_PATH, 401, unauthorized.to_owned()),
            ],
            1,
        ),
        (
            "incomplete",
            vec![
                healthy(),
                (
                    WHOAMI_PATH,
                    429,
                    r#"{"error":{"code":"rate_limited","message":"slow down","details":[]}}"#
                        .to_owned(),
                ),
            ],
            3,
        ),
    ];
    for (name, routes, exit) in cases {
        let stub = Stub::tls(leaf(&server_ca), routes);
        profile(
            &home,
            "prod",
            &stub.url("https"),
            &format!("ca_cert = \"{}\"", pem.display()),
        );
        let output = home.trawl(&["doctor", "-p", "prod", "--format", "json"], &[]);
        assert_eq!(
            output.status.code(),
            Some(exit),
            "{name}: {}",
            text(&output)
        );
        let all = text(&output);
        assert!(
            !all.contains(PROFILE_TOKEN) && !all.contains("pfx12345"),
            "{name}: {all}"
        );
        insta::assert_snapshot!(
            format!("doctor_json_contract_{name}"),
            normalize(&stub, &output.stdout)
        );
        if name == "fail" {
            let output = home.trawl(&["doctor", "-p", "prod", "--format", "table"], &[]);
            assert_eq!(output.status.code(), Some(exit), "{}", text(&output));
            insta::assert_snapshot!("doctor_text_fail", normalize(&stub, &output.stdout));
        }
        for request in stub.requests() {
            match request.path.as_str() {
                HEALTH_PATH => assert_eq!(request.authorization, None, "{name}"),
                _ => assert_eq!(
                    request.authorization.as_deref(),
                    Some(format!("Bearer {PROFILE_TOKEN}").as_str()),
                    "{name}"
                ),
            }
        }
    }
}
