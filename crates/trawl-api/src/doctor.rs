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

pub mod health;

/// Version of the JSON report document. Bump it when the shape changes.
pub const REPORT_VERSION: u32 = 1;

/// One doctor run: what it checked, from where, and what it concluded.
///
/// The fields are private so the verdict always agrees with the checks:
/// [`Report::new`] is the only constructor and computes it, nothing can
/// change the checks afterwards, and deserializing recomputes the verdict
/// and refuses a document whose stated verdict disagrees with its checks.
///
/// A struct literal does not compile outside this module:
///
/// ```compile_fail
/// use trawl_api::doctor::{Report, Target, Vantage, Verdict};
/// let forged = Report {
///     version: 1,
///     vantage: Vantage::Client,
///     target: Target { origin: None, source: String::new() },
///     verdict: Verdict::Pass,
///     checks: Vec::new(),
///     notes: Vec::new(),
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ReportDocument")]
pub struct Report {
    /// Document version, always [`REPORT_VERSION`] when built here.
    version: u32,
    /// Which process ran the checks.
    vantage: Vantage,
    /// What the checks were aimed at.
    target: Target,
    /// Conclusion over every check, computed by [`Report::new`].
    verdict: Verdict,
    /// Every check, in the order the doctor ran them.
    checks: Vec<Check>,
    /// Report-level notes that never change an outcome, e.g. a version
    /// mismatch between client and server.
    notes: Vec<String>,
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

    /// Document version.
    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }

    /// Which process ran the checks.
    #[must_use]
    pub const fn vantage(&self) -> Vantage {
        self.vantage
    }

    /// What the checks were aimed at.
    #[must_use]
    pub const fn target(&self) -> &Target {
        &self.target
    }

    /// Conclusion over every check.
    #[must_use]
    pub const fn verdict(&self) -> Verdict {
        self.verdict
    }

    /// Every check, in the order the doctor ran them.
    #[must_use]
    pub fn checks(&self) -> &[Check] {
        &self.checks
    }

    /// Report-level notes.
    #[must_use]
    pub fn notes(&self) -> &[String] {
        &self.notes
    }
}

/// The JSON form of a [`Report`] as read, before its verdict is checked.
#[derive(Deserialize)]
struct ReportDocument {
    version: u32,
    vantage: Vantage,
    target: Target,
    verdict: Verdict,
    checks: Vec<Check>,
    notes: Vec<String>,
}

/// A report document whose stated verdict is not the one its checks give.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerdictMismatch {
    /// The verdict the document states.
    pub stated: Verdict,
    /// The verdict its checks give.
    pub computed: Verdict,
}

impl std::fmt::Display for VerdictMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the report states verdict {:?} but its checks give {:?}",
            self.stated, self.computed
        )
    }
}

impl std::error::Error for VerdictMismatch {}

impl TryFrom<ReportDocument> for Report {
    type Error = VerdictMismatch;

    fn try_from(document: ReportDocument) -> Result<Self, Self::Error> {
        let computed = Verdict::of(document.checks.iter().map(|check| &check.outcome));
        if computed != document.verdict {
            return Err(VerdictMismatch {
                stated: document.verdict,
                computed,
            });
        }
        Ok(Self {
            version: document.version,
            vantage: document.vantage,
            target: document.target,
            verdict: computed,
            checks: document.checks,
            notes: document.notes,
        })
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

/// Stable reason codes.
///
/// Most say why a check could not look (`not_sampled`). The ones whose
/// comment names another outcome say why a `complete` or `failed` check has
/// that outcome, where the reason is part of the contract.
pub mod reason {
    /// The target refused the probe for exceeding its rate limit.
    pub const RATE_LIMITED: &str = "rate_limited";
    /// trawld refused the probe because it was at its request limit: its
    /// count of requests in progress, or its probe allowance, was full
    /// (ADR-0054). Retry with backoff.
    pub const REQUEST_LIMIT_REACHED: &str = "request_limit_reached";
    /// The probe got no answer within its deadline.
    pub const TIMED_OUT: &str = "timed_out";
    /// The doctor's user lacks access to what the check reads.
    pub const PERMISSION_DENIED: &str = "permission_denied";
    /// A prerequisite did not complete; [`super::Check::blocked_by`] names it.
    pub const BLOCKED: &str = "blocked";
    /// The target is still recovering what it holds, such as trawld's
    /// corpus after a restart, and cannot answer for it yet.
    pub const RECOVERING: &str = "recovering";
    /// Run as root, which can read what the service user may not: an
    /// access check did not look. A check that waits on it still runs.
    pub const RAN_AS_ROOT: &str = "ran_as_root";
    /// Nothing accepted a connection at the listener's address, as when
    /// trawld is not running.
    pub const NOT_LISTENING: &str = "not_listening";
    /// A file the check reads changed while the doctor read it.
    pub const MATERIAL_CHANGED: &str = "material_changed";
    /// A migrator holds its lock while the schema is not current.
    pub const MIGRATION_IN_PROGRESS: &str = "migration_in_progress";
    /// The evidence neither proves nor refutes the assertion, such as an
    /// archive with no identity marker.
    pub const UNPROVEN: &str = "unproven";
    /// No certificate exists yet to compare the listener against.
    pub const NO_MATERIAL: &str = "no_material";
    /// A file the check reads exists but could not be read as one.
    pub const UNREADABLE: &str = "unreadable";
    /// A file the check reads is larger than the doctor reads.
    pub const TOO_LARGE: &str = "too_large";
    /// With `complete`: trawld's boot creates or brings this state current
    /// on its next start, and accepts it as it is.
    pub const WILL_INITIALIZE: &str = "will_initialize";
    /// With `complete`: a storage root other users may reach that trawld's
    /// boot closes to its owner on its next start. A prediction: boot's
    /// change is the authority.
    pub const WILL_TIGHTEN: &str = "will_tighten";
    /// With `complete`: a well-formed recovery marker that trawld's boot
    /// finishes on its next start.
    pub const PENDING_AT_NEXT_BOOT: &str = "pending_at_next_boot";
    /// With `complete`: trawld's writer lock is held by some session.
    pub const HELD: &str = "held";
    /// With `complete`: no session holds trawld's writer lock.
    pub const NOT_OBSERVED: &str = "not_observed";
    /// With `failed`: the data root belongs to another catalog than the
    /// app-state database's.
    pub const CATALOG_IDENTITY_MISMATCH: &str = "catalog_identity_mismatch";
    /// The database session did not take the doctor's read-only setting
    /// and timeouts, or a startup field would have kept it from taking
    /// them, so the doctor sent no other query.
    pub const SESSION_NOT_READ_ONLY: &str = "session_not_read_only";
    /// The database connection broke after it authenticated, or before the
    /// server answered the doctor's startup message.
    pub const CONNECTION_LOST: &str = "connection_lost";
    /// A read-only query erred, or its answer did not decode, after the
    /// connection authenticated.
    pub const QUERY_FAILED: &str = "query_failed";
    /// The database server sent a message the driver could not decode, so
    /// the doctor dropped the connection.
    pub const PROTOCOL_ERROR: &str = "protocol_error";
    /// The listener address resolves to several addresses and none of them
    /// proved it serves the certificate on disk: which one trawld binds is
    /// not known.
    pub const AMBIGUOUS_ADDRESS: &str = "ambiguous_address";
    /// An exchange broke off before it was whole, such as a TLS handshake
    /// or a health answer cut short, and what arrived proves nothing.
    pub const INTERRUPTED: &str = "interrupted";
    /// With `failed`: under `sslmode` `disable` or `allow`, the server a
    /// database URL names did not answer the doctor's startup message as a
    /// PostgreSQL server does: its first message was not of type `R`, `E`
    /// or `v`, or declared more than 8 KiB. The doctor did not connect.
    pub const NOT_POSTGRES: &str = "not_postgres";
    /// A certificate the configuration pins does not exist yet, as before
    /// trawld's first start writes it.
    pub const CA_NOT_PRESENT: &str = "ca_not_present";
    /// With `not_configured`: no key is selected, so each start makes a new
    /// one and every earlier session ends.
    pub const EPHEMERAL_EACH_START: &str = "ephemeral_each_start";
    /// With `failed`: the address was refused a connection, as when nothing
    /// listens there.
    pub const CONNECTION_REFUSED: &str = "connection_refused";
    /// With `failed`: the server's certificate did not verify under the
    /// trust the configuration selects.
    pub const CERTIFICATE_NOT_TRUSTED: &str = "certificate_not_trusted";
    /// With `failed`: the answer was a redirect, which the doctor does not
    /// follow.
    pub const REDIRECT_REFUSED: &str = "redirect_refused";
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
            assert_eq!(built.verdict(), *verdict, "Report::new over {outcomes:?}");
            assert_eq!(built.verdict().exit_code(), *code);
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

    /// A document whose stated verdict disagrees with its checks is
    /// refused; one that agrees round-trips unchanged.
    #[test]
    fn doctor_report_deserialize_checks_the_verdict() {
        let built = report(&[Outcome::Complete, Outcome::NotSampled]);
        assert_eq!(built.verdict(), Verdict::Incomplete);
        let encoded = serde_json::to_value(&built).unwrap();
        let decoded: Report = serde_json::from_value(encoded.clone()).unwrap();
        assert_eq!(decoded, built);

        for stated in ["pass", "fail"] {
            let mut forged = encoded.clone();
            forged["verdict"] = serde_json::Value::from(stated);
            let err = serde_json::from_value::<Report>(forged)
                .expect_err("a verdict that disagrees with the checks is refused");
            assert!(
                err.to_string().contains("but its checks give Incomplete"),
                "{err}"
            );
        }

        // An empty report passes, and must say so.
        let mut forged = serde_json::to_value(report(&[])).unwrap();
        forged["verdict"] = serde_json::Value::from("fail");
        assert!(serde_json::from_value::<Report>(forged).is_err());
    }
}
