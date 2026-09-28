// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `web.*` checks: whether `--web-url` names a `trawl-web` that
//! accepts itself as a browser origin (ADR-0047).
//!
//! Both requests go through an [`OriginProbe`], which holds no key and has
//! no way to send one, and which refuses redirects. TLS is verified against
//! system roots only: the web origin is what a browser opens, so a
//! certificate a browser would not trust is a finding, and a private CA on
//! the web origin is not supported. A plain `http` `--web-url` reaches here
//! only for a loopback host; [`super::resolve`] refuses any other.
//!
//! `web.origin` sends `POST /api/auth/login` with the origin's own `Origin`
//! header and an empty `api_key`. `trawl-web` checks the origin before the
//! key and never calls trawld for an empty key, so the answer says whether
//! the origin is in `public_origins` and nothing else. It does not show that
//! `trawl-web` reaches trawld.

use std::time::Duration;

use trawl_api::doctor::{Check, Outcome, reason};
use trawl_client::{ClientError, NetworkKind, OriginProbe, ProbeResponse, TlsTrust};

use super::api::{row, with_next, with_reason};
use super::resolve::CheckedUrl;

/// `web.transport`.
pub const WEB_TRANSPORT: &str = "web.transport";
/// `web.origin`.
pub const WEB_ORIGIN: &str = "web.origin";

/// The deadline for each request the doctor sends.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// What `trawl-web` answers an allowed origin with an empty key.
const ACCEPTED_ERROR: &str = "bad request";
/// What `trawl-web` answers an origin outside `public_origins`.
const REJECTED_ERROR: &str = "cross-origin request rejected";

/// The state the `web.*` steps share, in the order the runner calls them.
pub struct WebRun<'a> {
    url: &'a CheckedUrl,
    probe: Option<OriginProbe>,
    notes: Vec<String>,
}

/// Names the origin only.
impl std::fmt::Debug for WebRun<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebRun")
            .field("origin", &self.url.origin())
            .finish_non_exhaustive()
    }
}

impl<'a> WebRun<'a> {
    pub const fn new(url: &'a CheckedUrl) -> Self {
        Self {
            url,
            probe: None,
            notes: Vec::new(),
        }
    }

    /// `web.transport`: `GET /healthz` must answer 200 with the body `ok`.
    pub async fn transport(&mut self) -> Check {
        let check = row(WEB_TRANSPORT, Outcome::Failed);
        let Ok(probe) = OriginProbe::new(self.url.origin(), &TlsTrust::System, REQUEST_TIMEOUT)
        else {
            return with_reason(check, "the HTTP client could not be built");
        };
        let result = probe.healthz().await;
        self.probe = Some(probe);
        match result {
            Ok(ProbeResponse { status: 200, body }) if body == b"ok" => Check {
                outcome: Outcome::Complete,
                ..check
            },
            Ok(ProbeResponse { status, .. }) => with_next(
                Check {
                    detail: Some(format!("GET /healthz answered HTTP {status}")),
                    ..with_reason(check, "not a trawl-web health answer")
                },
                "check that --web-url names trawl-web itself, not trawld's API or another \
                 service",
            ),
            Err(e) => network_failure(check, &e),
        }
    }

    /// `web.origin`: the login probe, with the origin's own `Origin` header
    /// and an empty key. The runner calls it only after `web.transport`
    /// completed.
    pub async fn origin(&mut self) -> Check {
        let probe = self
            .probe
            .as_ref()
            .expect("web.origin runs only after web.transport completed");
        let origin = self.url.origin();
        let check = row(WEB_ORIGIN, Outcome::Failed);
        let answer = match probe.login_probe(origin).await {
            Ok(answer) => answer,
            Err(e) => return network_failure(check, &e),
        };
        match answer.status {
            400 if is_error_body(&answer.body, ACCEPTED_ERROR) => {
                self.notes.push(
                    "web.origin shows that trawl-web accepts its own origin; it does not show \
                     that trawl-web reaches trawld"
                        .to_owned(),
                );
                Check {
                    outcome: Outcome::Complete,
                    detail: Some(format!("accepts Origin {origin}")),
                    ..check
                }
            }
            403 if is_error_body(&answer.body, REJECTED_ERROR) => with_next(
                with_reason(check, "origin not in public_origins"),
                format!(
                    "add {origin} to trawl-web's public_origins (the [web] public_origins list, \
                     or FLEET_SESSION_PUBLIC_ORIGINS), then restart trawl-web"
                ),
            ),
            429 => with_next(
                with_reason(
                    Check {
                        outcome: Outcome::NotSampled,
                        ..check
                    },
                    reason::RATE_LIMITED,
                ),
                "wait a minute, then run trawl doctor again",
            ),
            status => with_next(
                Check {
                    detail: Some(format!("POST /api/auth/login answered HTTP {status}")),
                    ..with_reason(check, "not a trawl-web login endpoint")
                },
                "check that --web-url names trawl-web itself, not trawld's API or another \
                 service",
            ),
        }
    }

    /// Report-level notes gathered along the way.
    pub fn into_notes(self) -> Vec<String> {
        self.notes
    }
}

/// Whether `body` is exactly `{"error": expected}`: a JSON object with one
/// key, `error`, whose value is that string.
fn is_error_body(body: &[u8], expected: &str) -> bool {
    serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(body).is_ok_and(|map| {
        map.len() == 1 && map.get("error").and_then(serde_json::Value::as_str) == Some(expected)
    })
}

/// The outcome of a probe that got no complete HTTP answer, from the
/// error's kind alone.
fn network_failure(check: Check, e: &ClientError) -> Check {
    match e.network_kind() {
        Some(NetworkKind::UntrustedCertificate) => with_next(
            with_reason(check, "certificate not trusted under system roots"),
            "a browser origin is checked against system roots only, and a private CA on the web \
             origin is not supported; serve trawl-web with a certificate system roots trust",
        ),
        Some(NetworkKind::Connect) => with_next(
            with_reason(check, "connection failed"),
            "check that trawl-web is running and that --web-url's host and port reach it",
        ),
        Some(NetworkKind::Timeout | NetworkKind::BodyTimeout) => with_next(
            with_reason(
                Check {
                    outcome: Outcome::NotSampled,
                    ..check
                },
                reason::TIMED_OUT,
            ),
            "check that --web-url's host and port are reachable from here; no answer came \
             within 10 s",
        ),
        Some(NetworkKind::Redirect) => with_next(
            with_reason(check, "redirect refused"),
            "give --web-url the origin trawl-web serves, not an address that redirects",
        ),
        _ => with_next(
            with_reason(check, "the request failed"),
            "run trawl doctor again, and check the network path to trawl-web",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_bodies_match_exactly() {
        assert!(is_error_body(br#"{"error":"bad request"}"#, ACCEPTED_ERROR));
        assert!(is_error_body(
            br#" { "error" : "bad request" } "#,
            ACCEPTED_ERROR
        ));
        for foreign in [
            &br#"{"error":"bad request","detail":"x"}"#[..],
            br#"{"error":"bad request: api_key is required"}"#,
            br#"{"error":{"code":"bad_request","message":"bad request"}}"#,
            br#"["bad request"]"#,
            b"bad request",
            b"",
        ] {
            assert!(
                !is_error_body(foreign, ACCEPTED_ERROR),
                "{}",
                String::from_utf8_lossy(foreign)
            );
        }
        assert!(is_error_body(
            br#"{"error":"cross-origin request rejected"}"#,
            REJECTED_ERROR
        ));
        assert!(!is_error_body(
            br#"{"error":"bad request"}"#,
            REJECTED_ERROR
        ));
    }
}
