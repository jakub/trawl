// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawl doctor` against a real trawld (ADR-0047, #199).
//!
//! Every doctor run happens in a CHILD PROCESS: this test binary re-executes
//! itself with the ignored [`doctor_child`] test selected and the run
//! described in `DOCTOR_PG_*` variables. The child starts from an empty
//! environment plus the loader's library path, so no `TRAWL_*` variable,
//! `SSL_CERT_DIR`, or proxy setting reaches it, and the doctor's refusal of
//! the `TRAWL_*` variables is live.
//!
//! The child is what lets `--url` be tested on its real trust path. Under
//! `--url` the doctor trusts system roots only, and the harness certificate
//! is self-signed. The child sets `SSL_CERT_FILE` to that certificate, which
//! the system root loader (rustls-native-certs) reads in place of the
//! platform store. No test-only trust source exists in the doctor.
//!
//! The child writes the report as JSON and as text to a directory the parent
//! owns, then exits with the verdict's status, so each test asserts on the
//! real exit status and on both renderings of one report.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use fleet_auth::{KeyStore, PrincipalKind};
use trawl_api::doctor::{Check, Outcome, Report, Verdict};
use trawl_cli::doctor::{self, Format, REQUESTS, resolve};
use trawl_server::config::RateLimitConfig;

/// Set only in the child: the directory the child writes its report into.
const CHILD_OUT: &str = "DOCTOR_PG_CHILD_OUT";
/// The child's `--url`.
const CHILD_URL: &str = "DOCTOR_PG_URL";
/// The child's `--token-file`.
const CHILD_TOKEN_FILE: &str = "DOCTOR_PG_TOKEN_FILE";
/// The child's `--profile`.
const CHILD_PROFILE: &str = "DOCTOR_PG_PROFILE";
/// The child's `-c`.
const CHILD_CONFIG: &str = "DOCTOR_PG_CONFIG";

/// The variables clap binds for `trawl doctor`, which the doctor refuses.
/// The CLI's own drift test proves this is the set clap binds.
const REFUSED_ENV: [&str; 4] = [
    "TRAWL_URL",
    "TRAWL_PROFILE",
    "TRAWL_TOKEN",
    "TRAWL_INSECURE",
];

/// The dynamic loader's search path, the one part of the parent's
/// environment the child keeps.
const LOADER_PATH: [&str; 3] = [
    "LD_LIBRARY_PATH",
    "DYLD_LIBRARY_PATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
];

/// The subprocess entry point. Without [`CHILD_OUT`] it does nothing, so a
/// run with `--run-ignored` passes it untouched.
#[tokio::test]
#[ignore = "entry point for the doctor child process; the tests below run it"]
async fn doctor_child() {
    let Some(out) = std::env::var_os(CHILD_OUT) else {
        return;
    };
    let out = PathBuf::from(out);
    let var = |name: &str| std::env::var(name).ok();
    let invocation = resolve::Invocation {
        url: var(CHILD_URL),
        token: false,
        insecure: false,
        profile: var(CHILD_PROFILE),
        config: var(CHILD_CONFIG),
        token_env: None,
        token_file: var(CHILD_TOKEN_FILE).map(PathBuf::from),
        web_url: None,
    };
    let refused: Vec<String> = REFUSED_ENV.iter().map(|&name| name.to_owned()).collect();
    let selection = resolve::select(&invocation, &refused, |name| {
        std::env::var_os(name).is_some()
    })
    .unwrap_or_else(|refusal| panic!("the doctor refused the run: {}", refusal.message));
    let report = doctor::check(&selection).await;
    for (format, file) in [(Format::Json, "report.json"), (Format::Table, "report.txt")] {
        let mut rendered = Vec::new();
        doctor::render(&report, format, &mut rendered).expect("render the report");
        std::fs::write(out.join(file), rendered).expect("write the rendered report");
    }
    std::process::exit(i32::from(report.verdict.exit_code()));
}

/// How the child selects its target.
enum Target<'a> {
    /// `--url`, trusting system roots, with an optional `--token-file`.
    Url {
        url: &'a str,
        token_file: Option<&'a Path>,
    },
    /// `--profile NAME -c CONFIG`.
    Profile { name: &'a str, config: &'a Path },
}

/// One finished doctor run.
struct Run {
    status: i32,
    report: Report,
    json: String,
    text: String,
}

impl Run {
    fn check(&self, id: &str) -> &Check {
        self.report
            .checks
            .iter()
            .find(|check| check.id == id)
            .unwrap_or_else(|| panic!("no {id} row in:\n{}", self.text))
    }

    fn outcome(&self, id: &str) -> Outcome {
        self.check(id).outcome
    }
}

/// Run the doctor in a child process and collect its report.
///
/// `ssl_cert_file` becomes the child's `SSL_CERT_FILE`; `None` leaves the
/// system roots to the platform store.
async fn run_doctor(target: Target<'_>, ssl_cert_file: Option<&Path>) -> Run {
    let out = tempfile::tempdir().expect("create the child's output dir");
    let mut command = Command::new(std::env::current_exe().expect("locate this test binary"));
    command
        .args(["doctor_child", "--exact", "--ignored", "--test-threads=1"])
        .env_clear()
        .env(CHILD_OUT, out.path());
    // The test binary links libduckdb dynamically, and the runner points the
    // loader at it. Only the loader's search path crosses over.
    for name in LOADER_PATH {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    match target {
        Target::Url { url, token_file } => {
            command.env(CHILD_URL, url);
            if let Some(path) = token_file {
                command.env(CHILD_TOKEN_FILE, path);
            }
        }
        Target::Profile { name, config } => {
            command.env(CHILD_PROFILE, name).env(CHILD_CONFIG, config);
        }
    }
    if let Some(path) = ssl_cert_file {
        command.env("SSL_CERT_FILE", path);
    }
    let output = tokio::task::spawn_blocking(move || command.output())
        .await
        .expect("join the child")
        .expect("run the doctor child");
    let status = output
        .status
        .code()
        .expect("the doctor child exited with a status");
    let read = |file: &str| {
        std::fs::read_to_string(out.path().join(file)).unwrap_or_else(|e| {
            panic!(
                "the doctor child wrote no {file} ({e}); status {status}\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            )
        })
    };
    let json = read("report.json");
    let text = read("report.txt");
    let report: Report = serde_json::from_str(&json).expect("the JSON report parses");
    assert_eq!(
        status,
        i32::from(report.verdict.exit_code()),
        "the exit status is the verdict's:\n{text}"
    );
    Run {
        status,
        report,
        json,
        text,
    }
}

/// The certificate every harness server presents.
fn harness_ca() -> PathBuf {
    common::ensure_test_cert().0
}

/// Write `token` to a file in `dir` for `--token-file`.
fn token_file(dir: &Path, token: &str) -> PathBuf {
    let path = dir.join("key");
    std::fs::write(&path, token).expect("write the token file");
    path
}

/// Assert `run` quotes neither the token nor the key prefix in either
/// rendering.
fn assert_no_secrets(run: &Run, token: &str, prefix: &str) {
    for (format, rendered) in [("JSON", &run.json), ("text", &run.text)] {
        assert!(
            !rendered.contains(token),
            "the {format} output quotes the token"
        );
        assert!(
            !rendered.contains(prefix),
            "the {format} output quotes the key prefix"
        );
    }
}

/// Assert every `api.*` check but identity completed.
fn assert_api_reachable(run: &Run) {
    for id in [
        "connection.config",
        "api.transport",
        "api.tls",
        "api.health",
    ] {
        assert_eq!(run.outcome(id), Outcome::Complete, "{id}:\n{}", run.text);
    }
    for check in run
        .report
        .checks
        .iter()
        .filter(|check| check.id.starts_with("api.health."))
    {
        assert_eq!(
            check.outcome,
            Outcome::Complete,
            "{}:\n{}",
            check.id,
            run.text
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn doctor_pass_without_key() {
    let server = common::setup().await;
    let ca = harness_ca();

    let run = run_doctor(
        Target::Url {
            url: &server.url,
            token_file: None,
        },
        Some(&ca),
    )
    .await;
    assert_eq!(run.status, 0, "{}", run.text);
    assert_eq!(run.report.verdict, Verdict::Pass);
    assert_api_reachable(&run);
    assert!(
        run.report
            .checks
            .iter()
            .any(|check| check.id.starts_with("api.health.")),
        "the health body's checks become rows:\n{}",
        run.text
    );
    assert_eq!(run.outcome("api.identity"), Outcome::NotConfigured);

    // The contrast leg: the same run without SSL_CERT_FILE trusts only the
    // platform store, which does not hold the harness certificate. The pass
    // above therefore came from the system-trust path reading SSL_CERT_FILE.
    let untrusted = run_doctor(
        Target::Url {
            url: &server.url,
            token_file: None,
        },
        None,
    )
    .await;
    assert_eq!(untrusted.status, 1, "{}", untrusted.text);
    assert_eq!(untrusted.outcome("api.transport"), Outcome::Complete);
    assert_eq!(
        untrusted.outcome("api.tls"),
        Outcome::Failed,
        "{}",
        untrusted.text
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn doctor_identity_reports_permissions_without_secrets() {
    let server = common::setup().await;
    let store = KeyStore::from_pool(server.fleet_pool.clone());
    let key = store
        .create_key(
            "doctor-analyst",
            PrincipalKind::Service,
            &common::roles(&["trawl-analyst"]),
            None,
        )
        .await
        .expect("mint the doctor's key");
    let token = key.plaintext_token.to_string();
    let prefix = key.info.prefix.clone();
    let ca = harness_ca();
    let dir = tempfile::tempdir().expect("create a temp dir");

    // --url with --token-file, trusting system roots via SSL_CERT_FILE.
    let key_path = token_file(dir.path(), &token);
    let by_url = run_doctor(
        Target::Url {
            url: &server.url,
            token_file: Some(&key_path),
        },
        Some(&ca),
    )
    .await;

    // A profile that binds the URL, the pinned harness CA, and the key in
    // its own table.
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[profiles.pgtest]\nurl = {url:?}\ntoken = {token:?}\nca_cert = {ca:?}\n",
            url = server.url,
            ca = ca.to_str().expect("a UTF-8 certificate path"),
        ),
    )
    .expect("write the CLI config");
    let by_profile = run_doctor(
        Target::Profile {
            name: "pgtest",
            config: &config,
        },
        None,
    )
    .await;

    for run in [&by_url, &by_profile] {
        assert_eq!(run.status, 0, "{}", run.text);
        assert_eq!(run.report.verdict, Verdict::Pass);
        assert_api_reachable(run);
        let identity = run.check("api.identity");
        assert_eq!(identity.outcome, Outcome::Complete, "{}", run.text);
        let detail = identity.detail.as_deref().expect("identity has a detail");
        assert!(detail.contains("name: doctor-analyst"), "{detail}");
        assert!(detail.contains("kind: service"), "{detail}");
        for permission in ["query", "schema_read", "export"] {
            assert!(detail.contains(permission), "{permission} in {detail}");
        }
        assert_no_secrets(run, &token, &prefix);
    }
    // An analyst key cannot ingest, and the server reports ingest capacity.
    assert!(
        by_profile
            .report
            .notes
            .iter()
            .any(|note| note.contains("lacks the ingest permission")),
        "{}",
        by_profile.text
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn doctor_revoked_key_fails() {
    let server = common::setup().await;
    let store = KeyStore::from_pool(server.fleet_pool.clone());
    let key = store
        .create_key(
            "doctor-revoked",
            PrincipalKind::Service,
            &common::roles(&["trawl-reader"]),
            None,
        )
        .await
        .expect("mint the key");
    store
        .revoke_key(&key.info.prefix)
        .await
        .expect("revoke the key");
    let token = key.plaintext_token.to_string();
    let dir = tempfile::tempdir().expect("create a temp dir");
    let key_path = token_file(dir.path(), &token);

    let run = run_doctor(
        Target::Url {
            url: &server.url,
            token_file: Some(&key_path),
        },
        Some(&harness_ca()),
    )
    .await;
    assert_eq!(run.status, 1, "{}", run.text);
    assert_eq!(run.report.verdict, Verdict::Fail);
    assert_api_reachable(&run);
    let identity = run.check("api.identity");
    assert_eq!(identity.outcome, Outcome::Failed, "{}", run.text);
    assert_eq!(identity.reason.as_deref(), Some("key rejected"));
    assert_no_secrets(&run, &token, &key.info.prefix);
}

#[tokio::test(flavor = "multi_thread")]
async fn doctor_permissionless_key_fails() {
    let server = common::setup().await;
    let dir = tempfile::tempdir().expect("create a temp dir");
    // A key whose only role grants coastwatch permissions: trawld 403s it.
    let key_path = token_file(dir.path(), &server.coastwatch_only_token);

    let run = run_doctor(
        Target::Url {
            url: &server.url,
            token_file: Some(&key_path),
        },
        Some(&harness_ca()),
    )
    .await;
    assert_eq!(run.status, 1, "{}", run.text);
    assert_eq!(run.report.verdict, Verdict::Fail);
    assert_api_reachable(&run);
    let identity = run.check("api.identity");
    assert_eq!(identity.outcome, Outcome::Failed, "{}", run.text);
    assert_eq!(identity.reason.as_deref(), Some("key has no permissions"));
    assert!(!run.json.contains(&server.coastwatch_only_token));
    assert!(!run.text.contains(&server.coastwatch_only_token));
}

#[tokio::test(flavor = "multi_thread")]
async fn doctor_rate_limited_is_incomplete() {
    // One request per minute per key: the burst is one, and the next token
    // arrives a minute later, far past this test's runtime. Spending the one
    // request first makes the doctor's whoami a certain 429.
    let server = common::setup_with_rate_limit(RateLimitConfig {
        default_rpm: 1,
        ..RateLimitConfig::default()
    })
    .await;
    let spend = common::harness_client_builder()
        .build()
        .expect("build the harness client")
        .get(format!("{}/api/v1/whoami", server.url))
        .bearer_auth(&server.reader_token)
        .send()
        .await
        .expect("spend the key's one request");
    assert_eq!(spend.status(), 200, "the first request is within the burst");

    let dir = tempfile::tempdir().expect("create a temp dir");
    let key_path = token_file(dir.path(), &server.reader_token);
    let run = run_doctor(
        Target::Url {
            url: &server.url,
            token_file: Some(&key_path),
        },
        Some(&harness_ca()),
    )
    .await;
    assert_eq!(run.status, 3, "{}", run.text);
    assert_eq!(run.report.verdict, Verdict::Incomplete);
    assert_api_reachable(&run);
    let identity = run.check("api.identity");
    assert_eq!(identity.outcome, Outcome::NotSampled, "{}", run.text);
    assert_eq!(identity.reason.as_deref(), Some("rate_limited"));
}

/// Every `http_request` span trawld opens, as method and path.
#[derive(Clone, Default)]
struct RequestCapture {
    requests: Arc<Mutex<Vec<(String, String)>>>,
}

impl RequestCapture {
    fn requests(&self) -> Vec<(String, String)> {
        self.requests.lock().unwrap().clone()
    }
}

struct SpanFields<'a>(&'a mut BTreeMap<String, String>);

impl tracing::field::Visit for SpanFields<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }
}

impl<S> tracing_subscriber::Layer<S> for RequestCapture
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::span::Id,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if attrs.metadata().name() != "http_request" {
            return;
        }
        let mut fields = BTreeMap::new();
        attrs.record(&mut SpanFields(&mut fields));
        let field = |name: &str| {
            fields
                .get(name)
                .unwrap_or_else(|| panic!("http_request span without {name}: {fields:?}"))
                .clone()
        };
        let request = (field("method"), field("path"));
        self.requests.lock().unwrap().push(request);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn doctor_request_allowlist() {
    use tracing_subscriber::prelude::*;

    // The span is opened by trawld's own edge layer, so it records every
    // request the server receives, whatever route or middleware answers it.
    let capture = RequestCapture::default();
    let subscriber = tracing_subscriber::registry().with(
        capture.clone().with_filter(
            tracing_subscriber::filter::Targets::new()
                .with_target("trawl_server", tracing::Level::INFO),
        ),
    );
    tracing::subscriber::set_global_default(subscriber).expect("no prior global subscriber");

    let server = common::setup().await;
    // The fixture's readiness polls are not the doctor's; count from here.
    let before = capture.requests().len();

    let ca = harness_ca();
    let dir = tempfile::tempdir().expect("create a temp dir");
    let key_path = token_file(dir.path(), &server.analyst_token);
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[profiles.pgtest]\nurl = {url:?}\ntoken = {token:?}\nca_cert = {ca:?}\n",
            url = server.url,
            token = server.analyst_token,
            ca = ca.to_str().expect("a UTF-8 certificate path"),
        ),
    )
    .expect("write the CLI config");

    let runs = [
        run_doctor(
            Target::Url {
                url: &server.url,
                token_file: None,
            },
            Some(&ca),
        )
        .await,
        run_doctor(
            Target::Url {
                url: &server.url,
                token_file: Some(&key_path),
            },
            Some(&ca),
        )
        .await,
        run_doctor(
            Target::Profile {
                name: "pgtest",
                config: &config,
            },
            None,
        )
        .await,
    ];
    for run in &runs {
        assert_eq!(run.status, 0, "{}", run.text);
    }

    let seen: Vec<(String, String)> = capture.requests().split_off(before);
    let allowed: Vec<(String, String)> = REQUESTS
        .iter()
        .map(|&(method, path)| (method.to_owned(), path.to_owned()))
        .collect();
    for request in &seen {
        assert!(
            allowed.contains(request),
            "trawld received {request:?}, which is not in trawl_cli::doctor::REQUESTS; saw \
             {seen:?}"
        );
    }
    // No query, no ingest, no sign-in: every request is a GET, and the only
    // paths are health and whoami. Three runs, one health request each, and
    // one whoami for each of the two runs with a key.
    let count = |path: &str| {
        seen.iter()
            .filter(|(method, seen_path)| method == "GET" && seen_path == path)
            .count()
    };
    assert_eq!(count("/api/v1/health"), 3, "{seen:?}");
    assert_eq!(count("/api/v1/whoami"), 2, "{seen:?}");
    assert_eq!(seen.len(), 5, "{seen:?}");
}
