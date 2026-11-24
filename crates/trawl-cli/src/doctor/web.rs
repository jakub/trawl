// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `web.*` checks: whether `--web-url` names a `trawl-web` that
//! accepts itself as a browser origin (ADR-0047).

use std::time::Duration;

use trawl_api::doctor::{Check, Outcome, reason};
use trawl_client::{ClientError, NetworkKind, OriginProbe, ProbeResponse, TlsTrust};

use super::api::{row, with_next, with_reason};
use super::resolve::CheckedUrl;

pub const WEB_TRANSPORT: &str = "web.transport";
pub const WEB_ORIGIN: &str = "web.origin";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

const ACCEPTED_BODY: &str = r#"{"error":{"code":"bad_request","message":"bad request"}}"#;
const REJECTED_BODY: &str =
    r#"{"error":{"code":"forbidden","message":"cross-origin request rejected"}}"#;

pub const ORIGIN_ACCEPTED_NOTE: &str = "web.origin shows that trawl-web accepts its own origin; \
                                        it does not show that trawl-web reaches trawld";

pub struct WebRun<'a> {
    url: &'a CheckedUrl,
    probe: Option<OriginProbe>,
    notes: Vec<String>,
}

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
            Ok(ProbeResponse { status: 429, .. }) => rate_limited(check),
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
        match recognize(answer.status, &answer.body) {
            Some(Answer::Accepted) => {
                self.notes.push(ORIGIN_ACCEPTED_NOTE.to_owned());
                Check {
                    outcome: Outcome::Complete,
                    detail: Some(format!("accepts Origin {origin}")),
                    ..check
                }
            }
            Some(Answer::Rejected) => with_next(
                with_reason(check, "origin not in public_origins"),
                format!(
                    "add {origin} to trawl-web's public_origins (the [web] public_origins list, \
                     or FLEET_SESSION_PUBLIC_ORIGINS), then restart trawl-web"
                ),
            ),
            None if answer.status == 429 => rate_limited(check),
            None => with_next(
                Check {
                    detail: Some(format!(
                        "POST /api/auth/login answered HTTP {}",
                        answer.status
                    )),
                    ..with_reason(check, "not a trawl-web login endpoint")
                },
                "check that --web-url names trawl-web itself, not trawld's API or another \
                 service",
            ),
        }
    }

    pub fn into_notes(self) -> Vec<String> {
        self.notes
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    Accepted,
    Rejected,
}

fn recognize(status: u16, body: &[u8]) -> Option<Answer> {
    let (answer, expected) = match status {
        400 => (Answer::Accepted, ACCEPTED_BODY),
        403 => (Answer::Rejected, REJECTED_BODY),
        _ => return None,
    };
    let body = serde_json::from_slice::<serde_json::Value>(body).ok()?;
    let expected: serde_json::Value =
        serde_json::from_str(expected).expect("the expected envelope is JSON");
    (body == expected).then_some(answer)
}

fn rate_limited(check: Check) -> Check {
    with_next(
        with_reason(
            Check {
                outcome: Outcome::NotSampled,
                ..check
            },
            reason::RATE_LIMITED,
        ),
        "wait a minute, then run trawl doctor again",
    )
}

fn network_failure(check: Check, e: &ClientError) -> Check {
    if let ClientError::TooLarge { .. } = e {
        return with_next(
            with_reason(check, "response too large"),
            "check that --web-url names trawl-web itself, not trawld's API or another service",
        );
    }
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
        Some(NetworkKind::BodyRead) => with_next(
            with_reason(check, "response body broken"),
            "run trawl doctor again; if it repeats, check for a proxy between here and \
             trawl-web that cuts answers short",
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
        let accepted = br#"{"error":{"code":"bad_request","message":"bad request"}}"#;
        let rejected =
            br#"{"error":{"code":"forbidden","message":"cross-origin request rejected"}}"#;
        assert_eq!(recognize(400, accepted), Some(Answer::Accepted));
        assert_eq!(
            recognize(
                400,
                br#" { "error" : { "message" : "bad request", "code" : "bad_request" } } "#
            ),
            Some(Answer::Accepted),
            "key order and JSON whitespace do not matter"
        );
        assert_eq!(recognize(403, rejected), Some(Answer::Rejected));

        // The right body under the other status is foreign.
        assert_eq!(recognize(403, accepted), None);
        assert_eq!(recognize(400, rejected), None);
        assert_eq!(recognize(401, accepted), None);
        assert_eq!(recognize(200, accepted), None);

        for (status, foreign) in [
            // The legacy bodies no trawl-web emits any more.
            (400, &br#"{"error":"bad request"}"#[..]),
            (403, br#"{"error":"cross-origin request rejected"}"#),
            (400, br#"{"error":{"code":"bad_request","message":"bad request: x"}}"#),
            (400, br#"{"error":{"code":"validation_error","message":"bad request"}}"#),
            (400, br#"{"error":{"code":"bad_request","message":"Bad Request"}}"#),
            (400, br#"{"error":{"code":"bad_request"}}"#),
            (400, br#"{"error":{"message":"bad request"}}"#),
            (
                400,
                br#"{"error":{"code":"bad_request","message":"bad request","details":[]}}"#,
            ),
            (
                400,
                br#"{"error":{"code":"bad_request","message":"bad request","details":[{"message":"x"}]}}"#,
            ),
            (
                400,
                br#"{"error":{"code":"bad_request","message":"bad request","conflict":{"expected":1,"current":2}}}"#,
            ),
            (
                400,
                br#"{"error":{"code":"bad_request","message":"bad request"},"hint":"x"}"#,
            ),
            (
                403,
                br#"{"error":{"code":"forbidden","message":"forbidden"}}"#,
            ),
            (
                403,
                br#"{"error":{"code":"auth_error","message":"cross-origin request rejected"}}"#,
            ),
            (
                403,
                br#"{"error":{"code":"forbidden","message":"cross-origin request rejected","request_id":"r"}}"#,
            ),
            (400, br#"["bad request"]"#),
            (400, b"bad request"),
            (403, b"cross-origin request rejected"),
            (400, b""),
        ] {
            assert_eq!(
                recognize(status, foreign),
                None,
                "{status} {}",
                String::from_utf8_lossy(foreign)
            );
        }
    }
}
