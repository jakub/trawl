// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The report and exit contract every doctor shares (ADR-0047).
//!
//! `trawl doctor`, `trawld --doctor` and `trawl-web --doctor` each run
//! named checks from their own vantage and hand back one [`Report`]. This
//! module holds the shape of that report, the four check outcomes, and the
//! verdict and exit-status rule. It carries no rendering: each binary renders
//! the same [`Report`] as text or serializes it as the versioned JSON form.
//!
//! Optional fields serialize as `null`, never omitted, so a consumer of the
//! JSON form sees every key on every check.

use serde::{Deserialize, Serialize};

/// Version of the JSON report document. Bump it when the shape changes.
pub const REPORT_VERSION: u32 = 1;

/// One doctor run: what it checked, from where, and what it concluded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// Document version, always [`REPORT_VERSION`] when built here.
    pub version: u32,
    /// Which process ran the checks.
    pub vantage: Vantage,
    /// What the checks were aimed at.
    pub target: Target,
    /// Conclusion over every check, computed by [`Report::new`].
    pub verdict: Verdict,
    /// Every check, in the order the doctor ran them.
    pub checks: Vec<Check>,
    /// Report-level notes that never change an outcome, e.g. a version
    /// mismatch between client and server.
    pub notes: Vec<String>,
}

impl Report {
    /// Build a report and compute its verdict from the checks' outcomes.
    ///
    /// This is the only constructor, so a report's verdict always agrees
    /// with its checks.
    #[must_use]
    pub fn new(vantage: Vantage, target: Target, checks: Vec<Check>, notes: Vec<String>) -> Self {
        let verdict = Verdict::of(checks.iter().map(|check| &check.outcome));
        Self {
            version: REPORT_VERSION,
            vantage,
            target,
            verdict,
            checks,
            notes,
        }
    }
}

/// The process a doctor runs in. Each vantage checks only the configuration
/// it consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Vantage {
    /// `trawl doctor`, where a client runs.
    Client,
    /// `trawld --doctor`, on the server host.
    Server,
    /// `trawl-web --doctor`, where the web proxy runs.
    Web,
}

/// What a doctor run was aimed at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    /// The origin checked (`scheme://host:port`), when one resolved.
    pub origin: Option<String>,
    /// Where the target came from, named without values, e.g.
    /// "CLI profile `prod` in ~/.config/trawl/config.toml".
    pub source: String,
}

/// One named check and its outcome.
///
/// Every string here is a name or a sanitized observation. None carries a
/// key, a key prefix, a database URL, a certificate body, a response body or
/// driver error text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    /// Stable check id, e.g. `api.tls` or `api.health.duckdb`.
    pub id: String,
    /// What the doctor observed.
    pub outcome: Outcome,
    /// Why the outcome is what it is. For `not_sampled`, one of the stable
    /// codes in [`reason`].
    pub reason: Option<String>,
    /// Observed, sanitized facts, e.g. a key's name, kind and permissions.
    pub detail: Option<String>,
    /// The name of the setting or input the check read, never its value.
    pub source: Option<String>,
    /// Id of the failed prerequisite that kept this check from looking.
    pub blocked_by: Option<String>,
    /// What the operator should do next.
    pub next_action: Option<String>,
}

/// The four outcomes of a check (ADR-0047, vocabulary from ADR-0033).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The doctor observed the named assertion hold.
    Complete,
    /// The doctor observed evidence against it.
    Failed,
    /// The user did not select what the check needs, e.g. no key.
    NotConfigured,
    /// The doctor could not look; [`Check::reason`] says why.
    NotSampled,
}

/// Conclusion of a doctor run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Every check is `complete` or `not_configured`.
    Pass,
    /// At least one check is `failed`.
    Fail,
    /// No check failed, but at least one was `not_sampled`.
    Incomplete,
}

impl Verdict {
    /// Conclude over a set of outcomes. A failure outweighs a check that
    /// could not look, and an empty set passes.
    pub fn of<'a>(outcomes: impl IntoIterator<Item = &'a Outcome>) -> Self {
        let mut verdict = Self::Pass;
        for outcome in outcomes {
            match outcome {
                Outcome::Failed => return Self::Fail,
                Outcome::NotSampled => verdict = Self::Incomplete,
                Outcome::Complete | Outcome::NotConfigured => {}
            }
        }
        verdict
    }

    /// Process exit status: 0 pass, 1 fail, 3 incomplete. 2 stays the
    /// usage-error status.
    #[must_use]
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Pass => 0,
            Self::Fail => 1,
            Self::Incomplete => 3,
        }
    }
}

/// Stable reason codes for a check that could not look.
pub mod reason {
    /// The target refused the probe for exceeding its rate limit.
    pub const RATE_LIMITED: &str = "rate_limited";
    /// The probe got no answer within its deadline.
    pub const TIMED_OUT: &str = "timed_out";
    /// The doctor's user lacks access to what the check reads.
    pub const PERMISSION_DENIED: &str = "permission_denied";
    /// A prerequisite did not complete; [`super::Check::blocked_by`] names it.
    pub const BLOCKED: &str = "blocked";
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(id: &str, outcome: Outcome) -> Check {
        Check {
            id: id.to_owned(),
            outcome,
            reason: None,
            detail: None,
            source: None,
            blocked_by: None,
            next_action: None,
        }
    }

    fn report(outcomes: &[Outcome]) -> Report {
        let checks = outcomes
            .iter()
            .enumerate()
            .map(|(i, outcome)| check(&format!("check.{i}"), *outcome))
            .collect();
        Report::new(
            Vantage::Client,
            Target {
                origin: None,
                source: "--url flag".to_owned(),
            },
            checks,
            Vec::new(),
        )
    }

    #[test]
    fn doctor_verdict_exit_mapping() {
        use Outcome::{Complete, Failed, NotConfigured, NotSampled};
        let table: &[(&[Outcome], Verdict, u8)] = &[
            (&[], Verdict::Pass, 0),
            (&[Complete], Verdict::Pass, 0),
            (&[NotConfigured], Verdict::Pass, 0),
            (&[Complete, NotConfigured], Verdict::Pass, 0),
            (&[Failed], Verdict::Fail, 1),
            (&[Complete, Failed, NotConfigured], Verdict::Fail, 1),
            (&[NotSampled], Verdict::Incomplete, 3),
            (
                &[Complete, NotConfigured, NotSampled],
                Verdict::Incomplete,
                3,
            ),
            // A failure outweighs a check that could not look, in either order.
            (&[NotSampled, Failed], Verdict::Fail, 1),
            (&[Failed, NotSampled], Verdict::Fail, 1),
        ];
        for (outcomes, verdict, code) in table {
            assert_eq!(Verdict::of(outcomes.iter()), *verdict, "{outcomes:?}");
            assert_eq!(verdict.exit_code(), *code, "{verdict:?}");
            let built = report(outcomes);
            assert_eq!(built.verdict, *verdict, "Report::new over {outcomes:?}");
            assert_eq!(built.verdict.exit_code(), *code);
        }
    }

    #[test]
    fn doctor_report_serde_round_trip() {
        let blocked = Check {
            id: "api.identity".to_owned(),
            outcome: Outcome::NotSampled,
            reason: Some(reason::BLOCKED.to_owned()),
            detail: None,
            source: Some("key from --token-file".to_owned()),
            blocked_by: Some("api.tls".to_owned()),
            next_action: None,
        };
        let built = Report::new(
            Vantage::Client,
            Target {
                origin: None,
                source: "--url flag".to_owned(),
            },
            vec![
                check("connection.config", Outcome::Complete),
                check("api.tls", Outcome::Failed),
                check("web.transport", Outcome::NotConfigured),
                blocked,
            ],
            vec!["note".to_owned()],
        );
        let encoded = serde_json::to_value(&built).unwrap();
        assert_eq!(
            encoded,
            serde_json::json!({
                "version": REPORT_VERSION,
                "vantage": "client",
                "target": {"origin": null, "source": "--url flag"},
                "verdict": "fail",
                "checks": [
                    {"id": "connection.config", "outcome": "complete", "reason": null,
                     "detail": null, "source": null, "blocked_by": null, "next_action": null},
                    {"id": "api.tls", "outcome": "failed", "reason": null,
                     "detail": null, "source": null, "blocked_by": null, "next_action": null},
                    {"id": "web.transport", "outcome": "not_configured", "reason": null,
                     "detail": null, "source": null, "blocked_by": null, "next_action": null},
                    {"id": "api.identity", "outcome": "not_sampled", "reason": "blocked",
                     "detail": null, "source": "key from --token-file",
                     "blocked_by": "api.tls", "next_action": null},
                ],
                "notes": ["note"],
            })
        );
        let decoded: Report = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, built);

        for (vantage, name) in [
            (Vantage::Client, "client"),
            (Vantage::Server, "server"),
            (Vantage::Web, "web"),
        ] {
            assert_eq!(serde_json::to_value(vantage).unwrap(), name);
        }
        for (verdict, name) in [
            (Verdict::Pass, "pass"),
            (Verdict::Fail, "fail"),
            (Verdict::Incomplete, "incomplete"),
        ] {
            assert_eq!(serde_json::to_value(verdict).unwrap(), name);
        }
    }
}
