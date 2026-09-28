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
//! path, `Origin`, whether it carried `Authorization` or `Cookie`, the
//! `api_key` its body held, and the raw text of its headers and body. The
//! doctor runs with a key selected (`--token-file` naming a canary), so a
//! probe has a key it could leak. Every test asserts that no request
//! carried a credential or the canary, and that each one is in
//! [`trawl_cli::doctor::REQUESTS`].
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
use trawl_cli::doctor::web::ORIGIN_ACCEPTED_NOTE;
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
    /// Every header name and value and the body, as text.
    raw: String,
}

/// The key the doctor is given. It must reach no web origin.
const CANARY: &str = "flt_canary00_doctorwebmustneversendthis";

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
    // The full request target, query included: a key carried in the query
    // must fail the exact `REQUESTS` match and show up in the canary check.
    let target = parts
        .uri
        .path_and_query()
        .map_or_else(|| parts.uri.path().to_owned(), ToString::to_string);
    let mut raw = String::new();
    raw.push_str(&target);
    raw.push('\n');
    for (name, value) in &parts.headers {
        raw.push_str(name.as_str());
        raw.push_str(": ");
        raw.push_str(&String::from_utf8_lossy(value.as_bytes()));
        raw.push('\n');
    }
    raw.push_str(&String::from_utf8_lossy(&bytes));
    log.lock().unwrap().push(Seen {
        method: parts.method.to_string(),
        path: target,
        origin: parts
            .headers
            .get(header::ORIGIN)
            .map(|value| value.to_str().unwrap_or("<not utf-8>").to_owned()),
        authorization: parts.headers.contains_key(header::AUTHORIZATION),
        cookie: parts.headers.contains_key(header::COOKIE),
        api_key,
        raw,
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
    fn stub(healthz: (u16, &str), login: (u16, &str)) -> Self {
        let answer = |(status, body): (u16, &str)| {
            let body = body.to_owned();
            move || {
                let body = body.clone();
                async move { (StatusCode::from_u16(status).unwrap(), body) }
            }
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
                !seen.raw.contains(CANARY),
                "{case}: the selected key reached {} {}",
                seen.method,
                seen.path
            );
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

/// Run `trawl doctor --url <closed> --token-file <canary> --web-url <web>
/// --format json` with no inherited `TRAWL_*` variables and a temp `HOME`
/// holding the [`CANARY`] key file.
fn doctor(web: &str) -> (Output, serde_json::Value) {
    let home = tempfile::tempdir().expect("tempdir");
    let key = home.path().join("key");
    std::fs::write(&key, format!("{CANARY}\n")).expect("key file");
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
            "--token-file",
            key.to_str().expect("utf-8 key path"),
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
    // The canary key was resolved: the probes had a key they could leak.
    assert_eq!(
        outcome(report, "connection.config"),
        ("complete".to_owned(), None, None),
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
            .any(|note| note == ORIGIN_ACCEPTED_NOTE),
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
        !notes(&report)
            .iter()
            .any(|note| note == ORIGIN_ACCEPTED_NOTE),
        "{report}"
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

/// A probe body past the cap is too large, never judged by its prefix. A
/// foreign endpoint whose body opens with trawl-web's exact answer, pads
/// it with JSON whitespace to the cap, and then goes on, is not taken for
/// `trawl-web`, for either answer `web.origin` recognizes.
#[test]
fn doctor_web_truncated_body_is_too_large() {
    for (case, status, signature) in [
        ("accepted signature", 400, r#"{"error":"bad request"}"#),
        (
            "rejected signature",
            403,
            r#"{"error":"cross-origin request rejected"}"#,
        ),
    ] {
        let mut body = signature.to_owned();
        body.push_str(&" ".repeat(trawl_client::OriginProbe::BODY_CAP - signature.len()));
        body.push_str("trailing garbage");
        let web = Origin::stub((200, "ok"), (status, &body));
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
                Some("response too large".to_owned()),
                None
            ),
            "{case}"
        );
        assert_eq!(output.status.code(), Some(1), "{case}");
        assert_eq!(web.keyless_requests(case), PROBES, "{case}");
    }
}

/// A plain loopback origin that answers `GET /healthz` with `200 ok`
/// unless `cut_healthz`, and cuts every other answer short: headers that
/// declare 100 bytes of body, 10 of them, then a closed connection. It
/// records each request it read as [`record`] does, headers and body
/// included, so [`Origin::keyless_requests`] applies to it.
fn cut_short_origin(cut_healthz: bool) -> Origin {
    use std::io::{Read as _, Write as _};
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let log = Log::default();
    let record = Arc::clone(&log);
    std::thread::spawn(move || {
        for tcp in listener.incoming() {
            let Ok(mut tcp) = tcp else { return };
            tcp.set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .ok();
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            let head_end = loop {
                if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
                match tcp.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => request.extend_from_slice(&buf[..n]),
                }
            };
            let head = String::from_utf8_lossy(&request[..head_end]).into_owned();
            let mut lines = head.lines();
            let mut parts = lines.next().unwrap_or_default().split(' ');
            let (method, path) = (
                parts.next().unwrap_or_default().to_owned(),
                parts.next().unwrap_or_default().to_owned(),
            );
            let headers: Vec<(String, String)> = lines
                .filter_map(|line| line.split_once(':'))
                .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
                .collect();
            let header = |name: &str| {
                headers
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, v)| v.clone())
            };
            let length = header("content-length").map_or(0, |v| v.parse::<usize>().unwrap());
            while request.len() < head_end + length {
                match tcp.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => request.extend_from_slice(&buf[..n]),
                }
            }
            let body = &request[head_end..head_end + length];
            let mut raw = String::new();
            for (name, value) in &headers {
                raw.push_str(name);
                raw.push_str(": ");
                raw.push_str(value);
                raw.push('\n');
            }
            raw.push_str(&String::from_utf8_lossy(body));
            let response: &[u8] = if path == "/healthz" && !cut_healthz {
                b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok"
            } else {
                b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 100\r\nconnection: close\r\n\r\n{\"error\":"
            };
            record.lock().unwrap().push(Seen {
                api_key: serde_json::from_slice::<serde_json::Value>(body)
                    .ok()
                    .and_then(|json| {
                        json.get("api_key")
                            .and_then(|v| v.as_str())
                            .map(str::to_owned)
                    }),
                origin: header("origin"),
                authorization: header("authorization").is_some(),
                cookie: header("cookie").is_some(),
                method,
                path,
                raw,
            });
            let _ = tcp.write_all(response);
            let _ = tcp.flush();
        }
    });
    Origin { url, log }
}

/// A web answer the origin cuts short after its headers is an observed
/// failure, not a failed connection and not a timeout: the check that
/// read it fails with `response body broken`. A broken `/healthz` fails
/// `web.transport` and blocks `web.origin`; a broken login answer after a
/// good `/healthz` fails `web.origin` alone.
#[test]
fn doctor_web_body_cut_short_is_broken() {
    let web = cut_short_origin(true);
    let (output, report) = doctor(&web.url);
    assert_api_failed_independently(&report, "healthz cut short");
    assert_eq!(
        outcome(&report, "web.transport"),
        (
            "failed".to_owned(),
            Some("response body broken".to_owned()),
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
    assert_eq!(web.keyless_requests("healthz cut short"), ["GET /healthz"]);

    let web = cut_short_origin(false);
    let (output, report) = doctor(&web.url);
    assert_api_failed_independently(&report, "login cut short");
    assert_eq!(
        outcome(&report, "web.transport"),
        ("complete".to_owned(), None, None)
    );
    assert_eq!(
        outcome(&report, "web.origin"),
        (
            "failed".to_owned(),
            Some("response body broken".to_owned()),
            None
        )
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(web.keyless_requests("login cut short"), PROBES);
}

/// `GET /healthz` answering 429 samples nothing: `web.transport` is
/// `not_sampled` with `rate_limited`, and `web.origin` is not sent.
#[test]
fn doctor_web_health_rate_limited() {
    let web = Origin::stub((429, "slow down"), (400, r#"{"error":"bad request"}"#));
    let (output, report) = doctor(&web.url);
    assert_api_failed_independently(&report, "rate limited");
    assert_eq!(
        outcome(&report, "web.transport"),
        (
            "not_sampled".to_owned(),
            Some("rate_limited".to_owned()),
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
    assert_eq!(web.keyless_requests("rate limited"), ["GET /healthz"]);
}
