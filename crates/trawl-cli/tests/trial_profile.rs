// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The reserved `-p trial` profile, observed on the real `trawl` binary.
//!
//! Each test builds a finished trial directory under a temp
//! `XDG_STATE_HOME`, with a temp `HOME` and the inherited `TRAWL_*`
//! environment stripped, and serves the trial's certificate from a real
//! rustls listener on `127.0.0.1`. `validate` with a token goes to the
//! server, so a success proves the URL, the token, and the CA all came
//! from the trial directory; a refusal proves nothing was sent.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

const TOKEN: &str = "flt_trialprofiletesttoken";
const VALID_BODY: &str = r#"{"valid":true,"errors":[]}"#;
const QUERY: &str = "* | head 1";

/// A self-signed certificate for `127.0.0.1` and `localhost`: the shape
/// the trial generates. Returns the PEM and the server identity.
fn self_signed() -> (String, Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned(), "localhost".to_owned()])
            .expect("self-signed pair");
    (
        cert.pem(),
        vec![cert.der().clone()],
        PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into(),
    )
}

/// What the listener saw: every accepted TCP connection, and the head of
/// every request that completed a handshake.
#[derive(Default)]
struct Seen {
    connections: usize,
    requests: Vec<String>,
}

/// A TLS listener that answers every request as a valid `validate`.
async fn serve(
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> (u16, Arc<Mutex<Seen>>) {
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
    let port = listener.local_addr().expect("local addr").port();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let record = Arc::clone(&seen);
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            record.lock().unwrap().connections += 1;
            let acceptor = acceptor.clone();
            let record = Arc::clone(&record);
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let head = read_head(&mut tls).await;
                record.lock().unwrap().requests.push(head);
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{VALID_BODY}",
                    VALID_BODY.len()
                );
                let _ = tls.write_all(response.as_bytes()).await;
                let _ = tls.shutdown().await;
            });
        }
    });
    (port, seen)
}

async fn read_head<S: AsyncReadExt + Unpin>(stream: &mut S) -> String {
    let mut head = Vec::new();
    let mut buf = [0u8; 4096];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => head.extend_from_slice(&buf[..n]),
        }
    }
    String::from_utf8_lossy(&head).into_owned()
}

/// A temp `HOME` and `XDG_STATE_HOME` holding a finished trial.
struct Trial {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    state_home: PathBuf,
}

impl Trial {
    fn new(api_port: u16, ca_pem: &str) -> Self {
        use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};

        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let state_home = tmp.path().join("state");
        let dir = state_home.join("trawl/trial");
        std::fs::create_dir(&home).unwrap();
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
        let private = |name: &str, bytes: &[u8]| {
            let path = dir.join(name);
            std::fs::write(&path, bytes).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        };
        private("state.json", state.to_string().as_bytes());
        private("operator.token", format!("{TOKEN}\n").as_bytes());
        std::fs::write(dir.join("ca.pem"), ca_pem).unwrap();
        std::fs::set_permissions(dir.join("ca.pem"), std::fs::Permissions::from_mode(0o644))
            .unwrap();

        Self {
            _tmp: tmp,
            home,
            state_home,
        }
    }

    /// Run `trawl` against this trial's environment, plus `env`.
    async fn trawl(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_trawl"));
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("TRAWL_") {
                cmd.env_remove(key);
            }
        }
        cmd.env("HOME", &self.home)
            .env("XDG_STATE_HOME", &self.state_home)
            .args(args)
            .envs(env.iter().copied());
        tokio::task::spawn_blocking(move || cmd.output().expect("spawn trawl"))
            .await
            .expect("join")
    }

    fn config(&self, toml: &str) -> PathBuf {
        let path = self.home.join("config.toml");
        std::fs::write(&path, toml).unwrap();
        path
    }
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_refused(output: &Output, fragment: &str, case: &str) {
    let stderr = stderr_of(output);
    assert!(
        !output.status.success(),
        "{case}: must fail, stderr: {stderr}"
    );
    assert!(
        stderr.contains(fragment),
        "{case}: stderr must name {fragment:?}: {stderr}"
    );
    assert!(output.stdout.is_empty(), "{case}: stdout must stay empty");
    assert!(
        !stderr.contains(TOKEN),
        "{case}: the token leaked: {stderr}"
    );
}

/// `-p trial` connects to `https://127.0.0.1:<recorded api port>` with the
/// operator token, trusting only the trial's certificate, and leaves the
/// user's config alone.
#[tokio::test]
async fn trial_profile_connects_with_the_trial_url_token_and_ca() {
    let (pem, chain, key) = self_signed();
    let (port, seen) = serve(chain, key).await;
    let trial = Trial::new(port, &pem);

    let output = trial.trawl(&["-p", "trial", "validate", QUERY], &[]).await;
    assert!(
        output.status.success(),
        "-p trial validate failed: {}",
        stderr_of(&output)
    );
    assert_eq!(output.stdout, b"valid\n");
    assert!(output.stderr.is_empty(), "stderr: {}", stderr_of(&output));

    let seen = seen.lock().unwrap();
    assert_eq!(seen.requests.len(), 1, "one request reached the trial");
    let head = seen.requests[0].to_ascii_lowercase();
    assert!(head.starts_with("post /api/v1/validate "), "{head}");
    assert!(
        head.contains(&format!(
            "authorization: bearer {}",
            TOKEN.to_ascii_lowercase()
        )),
        "the operator token from the trial directory: {head}"
    );

    assert!(
        !trial.home.join(".config").exists(),
        "-p trial never writes ~/.config/trawl"
    );
}

/// Another certificate on the API port: `-p trial` refuses to send the
/// token anywhere.
#[tokio::test]
async fn trial_profile_refuses_a_different_certificate_on_the_port() {
    let (_, chain, key) = self_signed();
    let (port, seen) = serve(chain, key).await;
    let (trial_pem, _, _) = self_signed();
    let trial = Trial::new(port, &trial_pem);

    let output = trial.trawl(&["-p", "trial", "validate", QUERY], &[]).await;
    assert!(
        !output.status.success(),
        "a foreign certificate was accepted"
    );
    assert!(!stderr_of(&output).contains(TOKEN));
    let seen = seen.lock().unwrap();
    assert!(
        seen.connections > 0,
        "the refusal happened at the handshake"
    );
    assert!(
        seen.requests.is_empty(),
        "no request may complete against a foreign certificate"
    );
}

/// Case name, extra flags, extra environment, expected refusal.
type Case<'a> = (&'a str, Vec<&'a str>, Vec<(&'a str, &'a str)>, &'a str);

/// Every override that could point `-p trial` somewhere else is refused
/// before any connection.
#[tokio::test]
async fn trial_profile_refuses_overrides_before_connecting() {
    let (pem, chain, key) = self_signed();
    let (port, seen) = serve(chain, key).await;
    let trial = Trial::new(port, &pem);
    let own_trial = trial.config("[profiles.trial]\nurl = \"https://elsewhere:5514\"\n");
    let own_trial = own_trial.to_str().unwrap();

    let cases: [Case<'_>; 8] = [
        (
            "TRAWL_URL",
            vec![],
            vec![("TRAWL_URL", "https://elsewhere:5514")],
            "TRAWL_URL is set",
        ),
        (
            "empty TRAWL_URL",
            vec![],
            vec![("TRAWL_URL", "")],
            "TRAWL_URL is set",
        ),
        (
            "TRAWL_TOKEN",
            vec![],
            vec![("TRAWL_TOKEN", "flt_other")],
            "TRAWL_TOKEN is set",
        ),
        (
            "empty TRAWL_TOKEN",
            vec![],
            vec![("TRAWL_TOKEN", "")],
            "TRAWL_TOKEN is set",
        ),
        (
            "--url",
            vec!["--url", "https://elsewhere:5514"],
            vec![],
            "--url was given",
        ),
        (
            "--token",
            vec!["--token", "flt_other"],
            vec![],
            "--token was given",
        ),
        (
            "[profiles.trial]",
            vec!["-c", own_trial],
            vec![],
            "defines [profiles.trial]",
        ),
        (
            "--insecure",
            vec!["--insecure"],
            vec![],
            "ca_cert and insecure are both on",
        ),
    ];
    for (case, flags, env, fragment) in cases {
        let mut args = vec!["-p", "trial"];
        args.extend(flags);
        args.extend(["validate", QUERY]);
        let output = trial.trawl(&args, &env).await;
        assert_refused(&output, fragment, case);
    }

    // TRAWL_INSECURE, and TRAWL_PROFILE selecting the reserved name.
    let output = trial
        .trawl(
            &["validate", QUERY],
            &[("TRAWL_PROFILE", "trial"), ("TRAWL_INSECURE", "true")],
        )
        .await;
    assert_refused(
        &output,
        "ca_cert and insecure are both on",
        "TRAWL_INSECURE",
    );
    assert!(
        !stderr_of(&output).contains("warning: insecure is on"),
        "the refusal comes before the insecure warning"
    );

    assert_eq!(
        seen.lock().unwrap().connections,
        0,
        "a refused invocation must not connect"
    );
}

/// A directory above the trial that another user could rename entries in
/// is refused by name, before any connection.
#[tokio::test]
async fn trial_profile_refuses_a_loose_ancestor_before_connecting() {
    use std::os::unix::fs::PermissionsExt as _;

    let (pem, chain, key) = self_signed();
    let (port, seen) = serve(chain, key).await;
    let trial = Trial::new(port, &pem);
    std::fs::set_permissions(&trial.state_home, std::fs::Permissions::from_mode(0o777)).unwrap();

    let output = trial.trawl(&["-p", "trial", "validate", QUERY], &[]).await;
    assert_refused(&output, "no sticky bit", "loose ancestor");
    assert!(
        stderr_of(&output).contains(&trial.state_home.display().to_string()),
        "the refusal names the directory: {}",
        stderr_of(&output)
    );
    assert_eq!(seen.lock().unwrap().connections, 0);
}

#[tokio::test]
async fn trial_profile_without_a_trial_names_trial_up() {
    let trial = Trial::new(1, "unused");
    std::fs::remove_dir_all(trial.state_home.join("trawl")).unwrap();

    let output = trial.trawl(&["-p", "trial", "validate", QUERY], &[]).await;
    assert_refused(&output, "trawl trial up", "no trial");
    assert!(
        !trial.state_home.join("trawl").exists(),
        "-p trial creates nothing"
    );
}

/// Trial verbs dispatch before the config file is read, so a broken
/// config cannot block them. A remote `DOCKER_HOST` stops the verbs that
/// need Docker at preflight, before any subprocess, so no engine is asked;
/// `status` and `key` work from the trial directory alone.
#[tokio::test]
async fn trial_verbs_do_not_read_the_config_file() {
    let trial = Trial::new(1, "unused");
    let broken = trial.config("this is [not toml");
    let broken = broken.to_str().unwrap();

    let output = trial.trawl(&["-c", broken, "validate", QUERY], &[]).await;
    assert!(
        stderr_of(&output).contains("failed to parse"),
        "control: the broken config breaks ordinary commands: {}",
        stderr_of(&output)
    );

    let remote = [("DOCKER_HOST", "tcp://example.invalid:2375")];
    for verb in [&["up"][..], &["stop"], &["down", "--yes"]] {
        let mut args = vec!["-c", broken, "trial"];
        args.extend(verb);
        let output = trial.trawl(&args, &remote).await;
        let stderr = stderr_of(&output);
        assert!(!output.status.success(), "{verb:?}");
        assert!(
            stderr.contains("DOCKER_HOST points at a tcp:// address"),
            "{verb:?}: {stderr}"
        );
        assert!(!stderr.contains("failed to parse"), "{verb:?}: {stderr}");
    }

    let output = trial
        .trawl(&["-c", broken, "trial", "status"], &remote)
        .await;
    assert!(output.status.success(), "{}", stderr_of(&output));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.starts_with("Trial 0123456789abcdef0123456789abcdef\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains("unknown: DOCKER_HOST points at"),
        "{stdout}"
    );

    let output = trial.trawl(&["-c", broken, "trial", "key"], &remote).await;
    assert!(output.status.success(), "{}", stderr_of(&output));
    assert_eq!(output.stdout, format!("{TOKEN}\n").as_bytes());
    assert!(output.stderr.is_empty(), "{}", stderr_of(&output));
}
