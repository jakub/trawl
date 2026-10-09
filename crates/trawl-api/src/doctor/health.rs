// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a doctor may show of trawld's health answer, and what each reported
//! value means (ADR-0047).
//!
//! `trawl doctor`, `trawld --doctor` and `trawl-web --doctor` all read
//! `GET /api/v1/health` and turn its `checks` map into one row per check
//! name. They classify names and values here, so the vantages cannot
//! disagree about which values pass. [`judge`] decides what a whole answer
//! is, from its HTTP status and its body, and hands back a typed [`Answer`]
//! that holds no text the server sent. Each binary builds its own rows from
//! that answer: the wording of a row's next action belongs to the vantage.
//!
//! A server's check names and values are remote text. A name is shown only
//! when [`is_health_key`] accepts it, and a value the doctor does not know is
//! quoted only when [`is_quotable_value`] accepts it. An [`Answer`] carries
//! neither: it names a check only by this crate's copy of a name in
//! [`HEALTH_CHECK_NAMES`], and a value only as a [`Reported`].

use std::collections::BTreeMap;

use super::Outcome;
use crate::{ErrorCode, ErrorResponse, HealthResponse, HealthStatus};

/// The health check that trawld reports while its corpus recovers after a
/// restart (ADR-0041). Only this key has values beyond `ok`, `error`, and
/// `refusing`.
pub const CORPUS_KEY: &str = "corpus";

/// The names of the checks `GET /api/v1/health` reports, in the order
/// trawld fills them. [`judge`] gives an answer's check a place of its own
/// only under one of these names (#269).
pub const HEALTH_CHECK_NAMES: [&str; 6] = [
    "duckdb",
    "auth_db",
    "storage_db",
    "data_path",
    "ingest_capacity",
    CORPUS_KEY,
];

/// The most bytes of a health answer's body a doctor reads. trawld's answer
/// is a few hundred.
pub const BODY_MAX: usize = 64 * 1024;

/// What every vantage shows for [`Answer::RequestLimit`], so the doctors
/// agree on what the refusal means.
pub const REQUEST_LIMIT_REFUSED: &str = "probe refused: trawld at its request limit";

/// A check name the report may show: `[a-z][a-z0-9_]{0,63}`.
#[must_use]
pub fn is_health_key(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name.as_bytes()[0].is_ascii_lowercase()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// A check value the report may quote: `[a-z0-9_]{1,32}`.
#[must_use]
pub fn is_quotable_value(value: &str) -> bool {
    (1..=32).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// What one reported health value means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueClass {
    /// `ok`: the server vouches for the check.
    Ok,
    /// The corpus is still recovering after a restart (`rollup_pending` or
    /// `restart_backlog` on [`CORPUS_KEY`]). The server cannot yet vouch
    /// for what it holds, so the doctor could not look.
    Recovering,
    /// `error` or `refusing`: the server reports the check failing.
    Refused,
    /// A value this doctor does not know. It cannot pass. Quote it only
    /// when [`is_quotable_value`] accepts it.
    Unknown,
}

impl ValueClass {
    /// The outcome a row with this value has.
    #[must_use]
    pub const fn outcome(self) -> Outcome {
        match self {
            Self::Ok => Outcome::Complete,
            Self::Recovering => Outcome::NotSampled,
            Self::Refused | Self::Unknown => Outcome::Failed,
        }
    }
}

/// Classify the value `value` that the health answer reports for the check
/// `name`.
#[must_use]
pub fn classify(name: &str, value: &str) -> ValueClass {
    Reported::of(name, value).class()
}

/// A value the health answer reported for one check, as this crate names
/// it. A value it does not know is [`Reported::Unknown`], and what the
/// server sent is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reported {
    /// `ok`.
    Ok,
    /// `rollup_pending` on [`CORPUS_KEY`].
    RollupPending,
    /// `restart_backlog` on [`CORPUS_KEY`].
    RestartBacklog,
    /// `error`.
    Error,
    /// `refusing`.
    Refusing,
    /// Any other value, or a corpus recovery value on another check.
    Unknown,
}

impl Reported {
    /// The value `value` that the health answer reports for the check
    /// `name`.
    #[must_use]
    pub fn of(name: &str, value: &str) -> Self {
        match value {
            "ok" => Self::Ok,
            "rollup_pending" if name == CORPUS_KEY => Self::RollupPending,
            "restart_backlog" if name == CORPUS_KEY => Self::RestartBacklog,
            "error" => Self::Error,
            "refusing" => Self::Refusing,
            _ => Self::Unknown,
        }
    }

    /// What the value means.
    #[must_use]
    pub const fn class(self) -> ValueClass {
        match self {
            Self::Ok => ValueClass::Ok,
            Self::RollupPending | Self::RestartBacklog => ValueClass::Recovering,
            Self::Error | Self::Refusing => ValueClass::Refused,
            Self::Unknown => ValueClass::Unknown,
        }
    }
}

/// Whether trawld's health endpoint sends the HTTP status `http`: 200 or
/// 503. Any other status is not its answer, whatever the body.
#[must_use]
pub const fn is_health_http_status(http: u16) -> bool {
    matches!(http, 200 | 503)
}

/// Whether the HTTP status `http` is the one trawld sends with a health
/// body whose `status` is `status`: 200 with `ok` or `degraded`, 503 with
/// `unavailable`.
#[must_use]
pub const fn status_agrees(http: u16, status: &HealthStatus) -> bool {
    matches!(
        (http, status),
        (200, HealthStatus::Ok | HealthStatus::Degraded) | (503, HealthStatus::Unavailable)
    )
}

/// What a doctor made of one health answer, from any vantage. It holds no
/// text the server sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// trawld's health body, under an HTTP status trawld sends with it.
    Health(Health),
    /// A 503 `corpus_recovering` refusal: trawld's corpus is still
    /// recovering after a restart and it cannot answer for it yet.
    Recovering,
    /// A 503 `request_limit_reached` refusal: trawld was at its count of
    /// requests in progress, or at its probe allowance, and refused the
    /// probe before any handler ran (ADR-0054). It says nothing about the
    /// checks, and it is not another service's answer: retry with backoff.
    RequestLimit,
    /// An HTTP status trawld's health endpoint never sends, whatever the
    /// body.
    Status(u16),
    /// A body larger than [`BODY_MAX`]. It was not read.
    TooLarge,
    /// A health body whose `status` trawld never sends with this HTTP
    /// status ([`status_agrees`]).
    Disagrees,
    /// A body that is neither trawld's health body nor its recovery
    /// refusal.
    Foreign,
}

impl Answer {
    /// The outcome of the health check itself. The per-check outcomes are
    /// the [`Health`]'s.
    #[must_use]
    pub fn outcome(&self) -> Outcome {
        match self {
            Self::Health(health) => health.outcome(),
            Self::Recovering | Self::RequestLimit | Self::TooLarge => Outcome::NotSampled,
            Self::Status(_) | Self::Disagrees | Self::Foreign => Outcome::Failed,
        }
    }
}

/// trawld's health body as a doctor may show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Health {
    http: u16,
    status: HealthStatus,
    versioned: bool,
    checks: Vec<KeyAnswer>,
    unknown_names: u32,
}

impl Health {
    /// The HTTP status the answer came with: 200 or 503.
    #[must_use]
    pub const fn http_status(&self) -> u16 {
        self.http
    }

    /// The body's `status`.
    #[must_use]
    pub const fn status(&self) -> &HealthStatus {
        &self.status
    }

    /// Whether the body carries a `version`, which trawld always sends.
    /// Its text is not kept.
    #[must_use]
    pub const fn has_version(&self) -> bool {
        self.versioned
    }

    /// One answer per reported check whose name is in
    /// [`HEALTH_CHECK_NAMES`], sorted by name.
    #[must_use]
    pub fn checks(&self) -> &[KeyAnswer] {
        &self.checks
    }

    /// How many reported check names are not in [`HEALTH_CHECK_NAMES`].
    /// None of them is kept: even an identifier-shaped name may be a
    /// secret.
    #[must_use]
    pub const fn unknown_names(&self) -> u32 {
        self.unknown_names
    }

    /// The outcome of the health check itself: an `unavailable` answer,
    /// which trawld sends only with a 503, fails whatever its checks say,
    /// even when it reports none or only `ok` ones.
    #[must_use]
    pub const fn outcome(&self) -> Outcome {
        match self.status {
            HealthStatus::Unavailable => Outcome::Failed,
            HealthStatus::Ok | HealthStatus::Degraded => Outcome::Complete,
        }
    }
}

/// One reported check a doctor gives a row of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyAnswer {
    name: &'static str,
    value: Reported,
}

impl KeyAnswer {
    /// The check's name: this crate's copy in [`HEALTH_CHECK_NAMES`], never
    /// the text that arrived.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// The value the check reported.
    #[must_use]
    pub const fn value(&self) -> Reported {
        self.value
    }

    /// The outcome of the check's row.
    #[must_use]
    pub const fn outcome(&self) -> Outcome {
        self.value.class().outcome()
    }
}

/// Judge a health answer that arrived whole with HTTP status `http` and
/// body `body`.
///
/// The status comes first: one trawld's health endpoint never sends is
/// [`Answer::Status`] whatever the body. A body past [`BODY_MAX`] is
/// [`Answer::TooLarge`] and not parsed. A 200 or 503 health body with a
/// `checks` map keeps one [`KeyAnswer`] per known check, so a 503 from a
/// failed `DuckDB` probe fails its own row, unless its `status` disagrees
/// with the HTTP status. A 503 `corpus_recovering` refusal is
/// [`Answer::Recovering`], as [`classify`] maps the corpus recovery values,
/// and a 503 `request_limit_reached` refusal is [`Answer::RequestLimit`].
/// Either code under any other status, like any other body, is not
/// trawld's health answer.
#[must_use]
pub fn judge(http: u16, body: &[u8]) -> Answer {
    if !is_health_http_status(http) {
        return Answer::Status(http);
    }
    if body.len() > BODY_MAX {
        return Answer::TooLarge;
    }
    if let Ok(health) = serde_json::from_slice::<HealthResponse>(body)
        && let Some(reported) = &health.checks
    {
        if !status_agrees(http, &health.status) {
            return Answer::Disagrees;
        }
        let mut named = BTreeMap::new();
        let mut unknown_names = 0_u32;
        for (name, value) in reported {
            match HEALTH_CHECK_NAMES.into_iter().find(|known| *known == name) {
                Some(known) => {
                    named.insert(known, Reported::of(known, value));
                }
                None => unknown_names = unknown_names.saturating_add(1),
            }
        }
        return Answer::Health(Health {
            http,
            versioned: health.version.is_some(),
            status: health.status,
            checks: named
                .into_iter()
                .map(|(name, value)| KeyAnswer { name, value })
                .collect(),
            unknown_names,
        });
    }
    if http == 503
        && let Ok(refusal) = serde_json::from_slice::<ErrorResponse>(body)
    {
        match refusal.error.code {
            ErrorCode::CorpusRecovering => return Answer::Recovering,
            ErrorCode::RequestLimitReached => return Answer::RequestLimit,
            _ => {}
        }
    }
    Answer::Foreign
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_values_classify_by_name_and_value() {
        let table = [
            ("duckdb", "ok", ValueClass::Ok, Outcome::Complete),
            ("duckdb", "error", ValueClass::Refused, Outcome::Failed),
            (
                "ingest_capacity",
                "refusing",
                ValueClass::Refused,
                Outcome::Failed,
            ),
            (
                "corpus",
                "rollup_pending",
                ValueClass::Recovering,
                Outcome::NotSampled,
            ),
            (
                "corpus",
                "restart_backlog",
                ValueClass::Recovering,
                Outcome::NotSampled,
            ),
            // Only the corpus check recovers; elsewhere the value is unknown.
            (
                "storage_db",
                "restart_backlog",
                ValueClass::Unknown,
                Outcome::Failed,
            ),
            ("wal", "recovering", ValueClass::Unknown, Outcome::Failed),
            ("corpus", "OK", ValueClass::Unknown, Outcome::Failed),
            ("corpus", "", ValueClass::Unknown, Outcome::Failed),
        ];
        for (name, value, class, outcome) in table {
            assert_eq!(classify(name, value), class, "{name}={value}");
            assert_eq!(class.outcome(), outcome, "{class:?}");
        }
    }

    fn health(
        http: u16,
        status: HealthStatus,
        versioned: bool,
        checks: &[(&'static str, Reported)],
        unknown_names: u32,
    ) -> Answer {
        Answer::Health(Health {
            http,
            status,
            versioned,
            checks: checks
                .iter()
                .map(|&(name, value)| KeyAnswer { name, value })
                .collect(),
            unknown_names,
        })
    }

    /// Every way a health answer can come back, from any vantage: trawld's
    /// body under each status it sends, its recovery refusal, and each way
    /// an answer is not trawld's. No name or value the server sent survives
    /// into the answer.
    #[test]
    #[expect(clippy::too_many_lines, reason = "one row per case")]
    fn health_answers_judge_status_and_body() {
        use HealthStatus::{Degraded, Ok as Healthy, Unavailable};
        use Reported::{Error, Ok, Refusing, RestartBacklog, RollupPending, Unknown};
        let oversized = format!(
            r#"{{"status":"ok","version":"x","checks":{{"duckdb":"{}"}}}}"#,
            "o".repeat(BODY_MAX)
        );
        let table: Vec<(u16, &[u8], Answer, Outcome)> = vec![
            (
                200,
                br#"{"status":"ok","version":"0.9.0","checks":{"duckdb":"ok","corpus":"ok"}}"#,
                health(200, Healthy, true, &[("corpus", Ok), ("duckdb", Ok)], 0),
                Outcome::Complete,
            ),
            (
                200,
                br#"{"status":"degraded","version":"0.9.0","checks":{
                    "duckdb":"ok","auth_db":"error","ingest_capacity":"refusing",
                    "corpus":"rollup_pending","data_path":"Weird Value!",
                    "storage_db":"restart_backlog"}}"#,
                health(
                    200,
                    Degraded,
                    true,
                    &[
                        ("auth_db", Error),
                        ("corpus", RollupPending),
                        ("data_path", Unknown),
                        ("duckdb", Ok),
                        ("ingest_capacity", Refusing),
                        // Only the corpus check recovers.
                        ("storage_db", Unknown),
                    ],
                    0,
                ),
                Outcome::Complete,
            ),
            // An unavailable answer fails the check itself, whatever its
            // checks say, and keeps them.
            (
                503,
                br#"{"status":"unavailable","version":"0.9.0","checks":{
                    "duckdb":"error","corpus":"restart_backlog"}}"#,
                health(
                    503,
                    Unavailable,
                    true,
                    &[("corpus", RestartBacklog), ("duckdb", Error)],
                    0,
                ),
                Outcome::Failed,
            ),
            (
                503,
                br#"{"status":"unavailable","checks":{}}"#,
                health(503, Unavailable, false, &[], 0),
                Outcome::Failed,
            ),
            // No version: still trawld's body shape; the vantage decides
            // whether an answer without one is trawld's.
            (
                200,
                br#"{"status":"ok","checks":{"duckdb":"ok"}}"#,
                health(200, Healthy, false, &[("duckdb", Ok)], 0),
                Outcome::Complete,
            ),
            // Names trawld does not report are counted, never kept.
            (
                200,
                br#"{"status":"ok","version":"x","checks":{"duckdb":"ok",
                    "Bad-Key":"ok","_invalid":"ok","private_secret":"ok",
                    "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08":"ok"}}"#,
                health(200, Healthy, true, &[("duckdb", Ok)], 4),
                Outcome::Complete,
            ),
            // The status and the body disagree.
            (
                200,
                br#"{"status":"unavailable","version":"x","checks":{}}"#,
                Answer::Disagrees,
                Outcome::Failed,
            ),
            (
                503,
                br#"{"status":"ok","version":"x","checks":{"duckdb":"ok"}}"#,
                Answer::Disagrees,
                Outcome::Failed,
            ),
            (
                503,
                br#"{"status":"degraded","version":"x","checks":{}}"#,
                Answer::Disagrees,
                Outcome::Failed,
            ),
            // No checks map: a status alone is anyone's answer.
            (
                200,
                br#"{"status":"ok","version":"x"}"#,
                Answer::Foreign,
                Outcome::Failed,
            ),
            // The recovery refusal, only as a 503.
            (
                503,
                br#"{"error":{"code":"corpus_recovering","message":"not yet"}}"#,
                Answer::Recovering,
                Outcome::NotSampled,
            ),
            (
                200,
                br#"{"error":{"code":"corpus_recovering","message":"not yet"}}"#,
                Answer::Foreign,
                Outcome::Failed,
            ),
            (
                503,
                br#"{"error":{"code":"internal_error","message":"x"}}"#,
                Answer::Foreign,
                Outcome::Failed,
            ),
            // Garbage.
            (200, b"<html>hello</html>", Answer::Foreign, Outcome::Failed),
            (503, b"", Answer::Foreign, Outcome::Failed),
            (200, &[0xff, 0xfe, 0x00], Answer::Foreign, Outcome::Failed),
            (
                200,
                br#"{"status":"sideways","checks":{}}"#,
                Answer::Foreign,
                Outcome::Failed,
            ),
            // A body past the cap is not parsed, even a well-formed one.
            (
                200,
                oversized.as_bytes(),
                Answer::TooLarge,
                Outcome::NotSampled,
            ),
            // A status trawld never sends decides alone, before the body.
            (
                404,
                br#"{"status":"ok","version":"x","checks":{}}"#,
                Answer::Status(404),
                Outcome::Failed,
            ),
            (502, b"bad gateway", Answer::Status(502), Outcome::Failed),
            (302, b"", Answer::Status(302), Outcome::Failed),
            (
                500,
                oversized.as_bytes(),
                Answer::Status(500),
                Outcome::Failed,
            ),
        ];
        for (http, body, answer, outcome) in table {
            let judged = judge(http, body);
            let shown = String::from_utf8_lossy(&body[..body.len().min(200)]);
            assert_eq!(judged, answer, "HTTP {http} {shown}");
            assert_eq!(judged.outcome(), outcome, "HTTP {http} {shown}");
            let debug = format!("{judged:?}");
            for hidden in ["Weird", "Bad-Key", "secret", "9f86d081", "0.9.0", "not yet"] {
                assert!(!debug.contains(hidden), "{hidden}: {debug}");
            }
        }

        // A body at the cap exactly is read.
        let padded = |len: usize| {
            let mut body = br#"{"status":"ok","version":"x","checks":{"duckdb":"ok"}}"#.to_vec();
            body.resize(len, b' ');
            body
        };
        assert_eq!(
            judge(200, &padded(BODY_MAX)),
            health(200, Healthy, true, &[("duckdb", Ok)], 0)
        );
        assert_eq!(judge(200, &padded(BODY_MAX + 1)), Answer::TooLarge);

        // Every per-check outcome follows the shared classifier.
        let Answer::Health(degraded) = judge(
            200,
            br#"{"status":"degraded","checks":{"duckdb":"ok","auth_db":"error",
                "corpus":"rollup_pending","wal":"x","data_path":"nope"}}"#,
        ) else {
            panic!("a degraded answer is trawld's");
        };
        let outcomes: Vec<_> = degraded
            .checks()
            .iter()
            .map(|check| (check.name(), check.outcome()))
            .collect();
        assert_eq!(
            outcomes,
            [
                ("auth_db", Outcome::Failed),
                ("corpus", Outcome::NotSampled),
                ("data_path", Outcome::Failed),
                ("duckdb", Outcome::Complete),
            ]
        );
        assert_eq!(degraded.unknown_names(), 1);
        assert_eq!(degraded.http_status(), 200);
        assert_eq!(degraded.status(), &Degraded);
        assert!(!degraded.has_version());
    }

    /// trawld's request-limit refusal, under the regular or the probe
    /// message, is a capacity refusal: not sampled, with no checks, and
    /// not a foreign answer. Only the 503 with the typed code is; the
    /// same code under another status, another code, and an unrelated
    /// body stay what they were (ADR-0054).
    #[test]
    fn health_request_limit_refusal_is_not_sampled_and_not_foreign() {
        let regular = br#"{"error":{"code":"request_limit_reached","message":"trawld is at its HTTP request limit ([server] max_concurrent_requests); the request was not processed; retry later with backoff"}}"#;
        let probe = br#"{"error":{"code":"request_limit_reached","message":"trawld is at its HTTP probe allowance; the request was not processed; retry later with backoff","details":[]}}"#;
        for body in [regular.as_slice(), probe.as_slice()] {
            let judged = judge(503, body);
            assert_eq!(judged, Answer::RequestLimit);
            assert_eq!(judged.outcome(), Outcome::NotSampled);
            let debug = format!("{judged:?}");
            assert!(!debug.contains("max_concurrent_requests"), "{debug}");
            assert!(!debug.contains("allowance"), "{debug}");
        }
        assert_eq!(
            REQUEST_LIMIT_REFUSED,
            "probe refused: trawld at its request limit"
        );
        assert_eq!(
            crate::doctor::reason::REQUEST_LIMIT_REACHED,
            "request_limit_reached"
        );

        // The code alone decides nothing: under a status trawld's health
        // endpoint does not send it with, the answer is what it was.
        assert_eq!(judge(200, regular), Answer::Foreign);
        assert_eq!(judge(429, regular), Answer::Status(429));
        assert_eq!(judge(500, regular), Answer::Status(500));
        // Another code, an unknown one, and an unrelated body are foreign.
        for body in [
            br#"{"error":{"code":"internal_error","message":"x"}}"#.as_slice(),
            br#"{"error":{"code":"request_limit","message":"x"}}"#,
            br#"{"error":"request_limit_reached"}"#,
            br#"{"code":"request_limit_reached"}"#,
            b"<html>503 Service Unavailable</html>",
        ] {
            assert_eq!(
                judge(503, body),
                Answer::Foreign,
                "{}",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[test]
    fn health_status_pairs_as_trawld_sends_it() {
        for (http, status, agrees) in [
            (200, HealthStatus::Ok, true),
            (200, HealthStatus::Degraded, true),
            (200, HealthStatus::Unavailable, false),
            (503, HealthStatus::Unavailable, true),
            (503, HealthStatus::Ok, false),
            (503, HealthStatus::Degraded, false),
            (500, HealthStatus::Unavailable, false),
            (204, HealthStatus::Ok, false),
        ] {
            assert_eq!(status_agrees(http, &status), agrees, "{http} {status:?}");
        }
        assert!(is_health_http_status(200) && is_health_http_status(503));
        assert!(!is_health_http_status(204) && !is_health_http_status(502));
        // Every name trawld reports is a key the report may show.
        assert!(HEALTH_CHECK_NAMES.into_iter().all(is_health_key));
    }

    #[test]
    fn health_names_and_values_follow_the_patterns() {
        assert!(is_health_key("duckdb"));
        assert!(is_health_key(&"a".repeat(64)));
        assert!(!is_health_key(&"a".repeat(65)));
        assert!(!is_health_key(""));
        assert!(!is_health_key("_invalid"));
        assert!(!is_health_key("0day"));
        assert!(!is_health_key("Bad-Key"));
        assert!(is_quotable_value("recovering"));
        assert!(is_quotable_value("0"));
        assert!(!is_quotable_value(""));
        assert!(!is_quotable_value(&"a".repeat(33)));
        assert!(!is_quotable_value("Weird Value!"));
    }
}
