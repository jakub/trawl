// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `trawl doctor --web-url`, observed on the real binary against a real
//! `trawl-web` router (ADR-0047).
//!
//! Each test binds `127.0.0.1:0` first, so the origin the doctor probes is
//! known before `trawl-web`'s `public_origins` is written, then serves
//! `trawl_web::routes::build` over plain loopback HTTP. The router's
//! upstream is an address nothing listens on: the probes never reach
//! trawld, and a test that needed it would fail rather than pass.
//!
//! A recording layer in front of every router notes each request's method,
//! path, `Origin`, whether it carried `Authorization` or `Cookie`, and the
//! `api_key` its body held. Every test asserts that no request carried a
//! credential and that each one is in [`trawl_cli::doctor::REQUESTS`].
//!
//! The API target is a closed port in every test, so the `api.*` rows fail
//! and the verdict is `fail` throughout; the web rows are checked by id.
//! They do not depend on the API.

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::{get, post};
use trawl_config::WebConfig;
use trawl_web::config::ResolvedConfig;
use trawl_web::state::AppState;

/// One request a web origin received.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    origin: Option<String>,
    authorization: bool,
    cookie: bool,
    /// The body's `api_key`, when the body is JSON that has one.
    api_key: Option<String>,
}

type Log = Arc<Mutex<Vec<Seen>>>;

/// Record the request, then pass it on unchanged.
async fn record(State(log): State<Log>, request: Request, next: Next) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, 64 * 1024)
        .await
        .expect("request body");
    let api_key = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|json| {
            json.get("api_key")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        });
    log.lock().unwrap().push(Seen {
        method: parts.method.to_string(),
        path: parts.uri.path().to_owned(),
        origin: parts
            .headers
            .get(header::ORIGIN)
            .map(|value| value.to_str().unwrap_or("<not utf-8>").to_owned()),
        authorization: parts.headers.contains_key(header::AUTHORIZATION),
        cookie: parts.headers.contains_key(header::COOKIE),
        api_key,
    });
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

/// A router on an owned loopback listener, recording every request.
struct Origin {
    url: String,
    log: Log,
}

impl Origin {
    /// Bind first, so `app` can be built knowing the origin it serves.
    fn serve(app: impl FnOnce(&str) -> Router) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let log = Log::default();
        let app = app(&url).layer(axum::middleware::from_fn_with_state(
            Arc::clone(&log),
            record,
        ));
        listener.set_nonblocking(true).expect("nonblocking");
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("runtime");
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).expect("listener");
                axum::serve(listener, app).await.expect("serve");
            });
        });
        Self { url, log }
    }

    /// A real `trawl-web` whose `public_origins` is `origins(url)`.
    fn trawl_web(origins: impl FnOnce(&str) -> Vec<String>) -> Self {
        Self::serve(|url| {
            let web = WebConfig {
                upstream_url: Some(format!("https://127.0.0.1:{}", closed_port())),
                allow_insecure_cookies: true,
                public_origins: origins(url),
                ..WebConfig::default()
            };
            let config = ResolvedConfig::from_parsed(&web, None).expect("trawl-web config");
            trawl_web::routes::build(AppState::from_config(config).expect("trawl-web state"))
        })
    }

    /// Something that is not `trawl-web`, answering its two paths.
    fn stub(healthz: (u16, &'static str), login: (u16, &'static str)) -> Self {
        let answer = |(status, body): (u16, &'static str)| {
            move || async move { (StatusCode::from_u16(status).unwrap(), body) }
        };
        Self::serve(|_| {
            Router::new()
                .route("/healthz", get(answer(healthz)))
                .route("/api/auth/login", post(answer(login)))
        })
    }

    fn requests(&self) -> Vec<Seen> {
        self.log.lock().unwrap().clone()
    }

    /// The requests, as `METHOD path`, after checking that none of them
    /// carried a credential and that each is one the doctor declares.
    fn keyless_requests(&self, case: &str) -> Vec<String> {
        let requests = self.requests();
        for seen in &requests {
            assert!(
                trawl_cli::doctor::REQUESTS
                    .iter()
                    .any(|(method, path)| seen.method == *method && seen.path == *path),
                "{case}: {} {} is not in trawl_cli::doctor::REQUESTS",
                seen.method,
                seen.path
            );
            assert!(!seen.authorization, "{case}: Authorization sent: {seen:?}");
            assert!(!seen.cookie, "{case}: Cookie sent: {seen:?}");
            assert!(
                seen.api_key.as_deref().is_none_or(str::is_empty),
                "{case}: a non-empty api_key was sent"
            );
            if seen.path == "/api/auth/login" {
                assert_eq!(seen.api_key.as_deref(), Some(""), "{case}");
                assert_eq!(seen.origin.as_deref(), Some(self.url.as_str()), "{case}");
            }
        }
        requests
            .iter()
            .map(|seen| format!("{} {}", seen.method, seen.path))
            .collect()
    }
}

/// A port with nothing listening on it.
fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("addr").port()
}

/// Run `trawl doctor --url <closed> --web-url <web> --format json` with no
/// inherited `TRAWL_*` variables and a temp `HOME`.
fn doctor(web: &str) -> (Output, serde_json::Value) {
    let home = tempfile::tempdir().expect("tempdir");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_trawl"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("TRAWL_") {
            cmd.env_remove(key);
        }
    }
    let api = format!("https://127.0.0.1:{}", closed_port());
    let output = cmd
        .env_remove("SSL_CERT_FILE")
        .env_remove("SSL_CERT_DIR")
        .env("HOME", home.path())
        .env("XDG_STATE_HOME", PathBuf::from(home.path()).join("state"))
        .args([
            "doctor",
            "--url",
            &api,
            "--web-url",
            web,
            "--format",
            "json",
        ])
        .output()
        .expect("spawn trawl");
    let report = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not a JSON report ({e}): {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output, report)
}

/// `(outcome, reason, blocked_by)` of the check `id`.
fn outcome(report: &serde_json::Value, id: &str) -> (String, Option<String>, Option<String>) {
    let check = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["id"] == id)
        .unwrap_or_else(|| panic!("no {id} check in {report}"));
    (
        check["outcome"].as_str().unwrap().to_owned(),
        check["reason"].as_str().map(str::to_owned),
        check["blocked_by"].as_str().map(str::to_owned),
    )
}

fn notes(report: &serde_json::Value) -> Vec<String> {
    report["notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|note| note.as_str().unwrap().to_owned())
        .collect()
}

const PROBES: [&str; 2] = ["GET /healthz", "POST /api/auth/login"];

/// The web rows follow the API rows, and the API's failure does not block
/// them.
fn assert_api_failed_independently(report: &serde_json::Value, case: &str) {
    let ids: Vec<&str> = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|check| check["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        [
            "connection.config",
            "api.transport",
            "api.tls",
            "api.health",
            "api.identity",
            "web.transport",
            "web.origin"
        ],
        "{case}"
    );
    assert_eq!(outcome(report, "api.transport").0, "failed", "{case}");
}

/// `trawl-web` lists the origin: both web checks complete, and the report
/// says that this does not prove the proxy reaches trawld.
#[test]
fn doctor_web_origin_accepted() {
    let web = Origin::trawl_web(|url| vec!["https://trawl.example.com".to_owned(), url.to_owned()]);
    let (output, report) = doctor(&web.url);
    assert_api_failed_independently(&report, "accepted");
    assert_eq!(
        outcome(&report, "web.transport"),
        ("complete".to_owned(), None, None)
    );
    assert_eq!(
        outcome(&report, "web.origin"),
        ("complete".to_owned(), None, None)
    );
    assert!(
        notes(&report)
            .iter()
            .any(|note| note.contains("does not show that trawl-web reaches trawld")),
        "{report}"
    );
    // Only the closed API port fails the run.
    let failed: Vec<&str> = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|check| check["outcome"] == "failed")
        .map(|check| check["id"].as_str().unwrap())
        .collect();
    assert_eq!(failed, ["api.transport"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(web.keyless_requests("accepted"), PROBES);
}

/// `trawl-web` does not list the origin: `web.origin` fails and names the
/// setting to change.
#[test]
fn doctor_web_origin_rejected() {
    let web = Origin::trawl_web(|_| vec!["https://trawl.example.com".to_owned()]);
    let (output, report) = doctor(&web.url);
    assert_api_failed_independently(&report, "rejected");
    assert_eq!(
        outcome(&report, "web.transport"),
        ("complete".to_owned(), None, None)
    );
    assert_eq!(
        outcome(&report, "web.origin"),
        (
            "failed".to_owned(),
            Some("origin not in public_origins".to_owned()),
            None
        )
    );
    assert!(
        notes(&report)
            .iter()
            .all(|note| !note.contains("trawl-web"))
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(report["verdict"], "fail");
    assert_eq!(web.keyless_requests("rejected"), PROBES);
}

/// Something that answers the right statuses with other bodies is not
/// taken for `trawl-web`; one whose health answer is not `ok` is not
/// probed further.
#[test]
fn doctor_web_foreign_endpoint() {
    for (case, login) in [
        (
            "400 with another body",
            (400, r#"{"error":"bad request: api_key is required"}"#),
        ),
        (
            "400 with extra keys",
            (400, r#"{"error":"bad request","hint":"x"}"#),
        ),
        ("403 with another body", (403, r#"{"error":"forbidden"}"#)),
        ("403 as plain text", (403, "cross-origin request rejected")),
    ] {
        let web = Origin::stub((200, "ok"), login);
        let (output, report) = doctor(&web.url);
        assert_api_failed_independently(&report, case);
        assert_eq!(
            outcome(&report, "web.transport"),
            ("complete".to_owned(), None, None),
            "{case}"
        );
        assert_eq!(
            outcome(&report, "web.origin"),
            (
                "failed".to_owned(),
                Some("not a trawl-web login endpoint".to_owned()),
                None
            ),
            "{case}"
        );
        assert_eq!(output.status.code(), Some(1), "{case}");
        assert_eq!(web.keyless_requests(case), PROBES, "{case}");
    }

    let web = Origin::stub(
        (200, r#"{"status":"ok"}"#),
        (400, r#"{"error":"bad request"}"#),
    );
    let (output, report) = doctor(&web.url);
    assert_api_failed_independently(&report, "foreign health");
    assert_eq!(
        outcome(&report, "web.transport"),
        (
            "failed".to_owned(),
            Some("not a trawl-web health answer".to_owned()),
            None
        )
    );
    assert_eq!(
        outcome(&report, "web.origin"),
        (
            "not_sampled".to_owned(),
            Some("blocked".to_owned()),
            Some("web.transport".to_owned())
        )
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(web.keyless_requests("foreign health"), ["GET /healthz"]);
}
