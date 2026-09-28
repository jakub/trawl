// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `api.*` checks: transport, TLS, health, and identity (ADR-0047).
//!
//! One unkeyed `GET /api/v1/health` decides `api.transport`, `api.tls` and
//! `api.health`. It is sent under the connection's own trust and carries no
//! key. A key goes out only in `GET /api/v1/whoami`, and only through a
//! client built from a [`witness::VerifiedApi`]. That takes two steps:
//! `api.tls` mints a [`witness::VerifiedTls`] when the URL is `https`, the
//! trust verifies, and an HTTP answer arrived under that trust; `api.health`
//! turns it into a `VerifiedApi` only when that answer parsed as a trawl
//! health body carrying trawl's signature: a `checks` map and a `version`,
//! which trawld always sends, under the HTTP status trawld pairs with the
//! body's `status` (200 with `ok` or `degraded`, 503 with `unavailable`).
//! An untrusted certificate, an `insecure` trust, a plain `http` URL, a
//! redirect, or any answer that is not trawl's health body therefore never
//! sees a key.
//!
//! Every outcome is decided from [`ClientError::network_kind`] or a
//! [`ClientError::Server`] status, never from an error's text, and no
//! error's `Display` or `Debug` reaches the report. Strings the server sent
//! pass through [`display_safe`] first, which also redacts any run of the
//! selected key: a server that echoes the key, or its prefix, in a name, a
//! permission, a check name or value, or its version does not get it into
//! the report.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::time::Duration;

use trawl_api::doctor::{Check, Outcome, reason};
use trawl_api::{HealthResponse, HealthStatus, WhoAmIResponse};
use trawl_client::{ClientError, NetworkKind, TlsTrust};

use super::resolve::{Connection, Scheme};
use super::{display_safe, display_safe_within, holds_key};

/// `api.transport`.
pub const API_TRANSPORT: &str = "api.transport";
/// `api.tls`.
pub const API_TLS: &str = "api.tls";
/// `api.health`.
pub const API_HEALTH: &str = "api.health";
/// `api.identity`.
pub const API_IDENTITY: &str = "api.identity";
/// Prefix of the one row per check name the health answer reports.
pub const API_HEALTH_KEY_PREFIX: &str = "api.health.";
/// The one row that stands for every check name that is not an identifier.
/// Its last part starts with `_`, which no check name may, so it cannot
/// collide with a server's check.
pub const API_HEALTH_INVALID_KEY: &str = "api.health._invalid";

/// The deadline for each request the doctor sends.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// A check name the report may show: `[a-z][a-z0-9_]{0,63}`.
fn is_health_key(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name.as_bytes()[0].is_ascii_lowercase()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// A check value the report may quote: `[a-z0-9_]{1,32}`.
fn is_quotable_value(value: &str) -> bool {
    (1..=32).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

pub(super) fn row(id: &str, outcome: Outcome) -> Check {
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

pub(super) fn with_reason(mut check: Check, reason: impl Into<String>) -> Check {
    check.reason = Some(reason.into());
    check
}

pub(super) fn with_next(mut check: Check, next: impl Into<String>) -> Check {
    check.next_action = Some(next.into());
    check
}

/// The private witness and the only two constructors of an HTTP client
/// aimed at the API.
mod witness {
    use trawl_api::{HealthResponse, HealthStatus};
    use trawl_client::{ClientError, HttpClient, TlsTrust};

    use super::super::resolve::{CheckedUrl, Connection, Key, Scheme};
    use super::REQUEST_TIMEOUT;

    /// The unkeyed health request and what came back. Only [`probe`]
    /// builds one, so a `Probe` that says an answer arrived is one that did.
    pub(super) struct Probe {
        /// Whether the request went out under the connection's own trust.
        /// A plain `http` URL is probed without TLS, so it is false there.
        under_configured_trust: bool,
        /// What the request returned, with the HTTP status (200 or 503) of
        /// an answer. `Err` from building the client too.
        pub(super) result: Result<(u16, HealthResponse), ClientError>,
        /// The client could not be built; nothing was sent.
        pub(super) not_built: bool,
    }

    impl Probe {
        /// An HTTP answer arrived: a status line, whatever it said.
        pub(super) fn answered(&self) -> bool {
            super::answered(&self.result)
        }
    }

    /// Every client here is built on a [`CheckedUrl`], which cannot hold
    /// userinfo; the assertion restates that at the point of use.
    fn client(url: &CheckedUrl, token: &str, trust: &TlsTrust) -> Result<HttpClient, ClientError> {
        debug_assert!(
            !url.origin().contains('@'),
            "a CheckedUrl never carries userinfo"
        );
        HttpClient::with_trust_timeout(url.base(), token, trust, REQUEST_TIMEOUT)
    }

    /// Send the one unkeyed `GET /api/v1/health`.
    ///
    /// The client holds an empty token, and `health()` sends no
    /// `Authorization` header at all. A plain `http` URL involves no TLS,
    /// so it is probed with system trust: a pin would refuse to send it,
    /// and there is no certificate for any trust to judge.
    pub(super) async fn probe(connection: &Connection) -> Probe {
        let plain = connection.url.scheme() == Scheme::Http;
        let trust = if plain {
            &TlsTrust::System
        } else {
            &connection.trust
        };
        match client(&connection.url, "", trust) {
            Ok(api) => Probe {
                under_configured_trust: !plain,
                result: api.health_with_status().await,
                not_built: false,
            },
            Err(e) => Probe {
                under_configured_trust: !plain,
                result: Err(e),
                not_built: true,
            },
        }
    }

    /// An API whose certificate verified under the connection's trust, and
    /// the probe's answer observed under it. Only [`verify_tls`] mints one.
    /// It cannot build a keyed client; [`VerifiedTls::into_api`] decides
    /// whether the answer came from trawl.
    pub(super) struct VerifiedTls<'a> {
        url: &'a CheckedUrl,
        trust: &'a TlsTrust,
        answer: Result<(u16, HealthResponse), ClientError>,
    }

    /// Mint the TLS witness when, and only when, the URL is `https`, the
    /// trust verifies certificates, and the probe got an HTTP answer under
    /// that trust. The probe is consumed, so its answer is the one the
    /// witness carries.
    pub(super) fn verify_tls(connection: &Connection, probe: Probe) -> Option<VerifiedTls<'_>> {
        let verifies = matches!(connection.trust, TlsTrust::System | TlsTrust::PinnedCa(_));
        if connection.url.scheme() == Scheme::Https
            && verifies
            && probe.under_configured_trust
            && !probe.not_built
            && probe.answered()
        {
            Some(VerifiedTls {
                url: &connection.url,
                trust: &connection.trust,
                answer: probe.result,
            })
        } else {
            None
        }
    }

    /// An API that answered as trawl under verified TLS: its health answer
    /// parsed as a trawl health body with both a `checks` map and a
    /// `version`, under the HTTP status trawld sends with that body's
    /// `status`. Only [`VerifiedTls::into_api`] mints one.
    pub(super) struct VerifiedApi<'a> {
        url: &'a CheckedUrl,
        trust: &'a TlsTrust,
    }

    /// Why a health answer minted no [`VerifiedApi`].
    pub(super) enum NotTrawl {
        /// The request failed or its answer did not parse.
        Answer(ClientError),
        /// The body parsed, but lacks the `checks` map or the `version`
        /// that trawld always sends: a status alone is anyone's answer.
        Unsigned,
        /// The HTTP status and the body's `status` are not a pair trawld
        /// sends: 200 goes with `ok` or `degraded`, 503 with `unavailable`.
        Disagrees,
    }

    impl<'a> VerifiedTls<'a> {
        /// The health answer, and with it the API witness when that answer
        /// is trawl's health body. A redirect, another status, a body that
        /// does not parse, one without both `checks` and `version`, or one
        /// whose `status` disagrees with the HTTP status comes back as the
        /// refusal, and no witness.
        pub(super) fn into_api(self) -> Result<(VerifiedApi<'a>, HealthResponse), NotTrawl> {
            let (http_status, health) = self.answer.map_err(NotTrawl::Answer)?;
            if health.checks.is_none() || health.version.is_none() {
                return Err(NotTrawl::Unsigned);
            }
            if !matches!(
                (http_status, &health.status),
                (200, HealthStatus::Ok | HealthStatus::Degraded) | (503, HealthStatus::Unavailable)
            ) {
                return Err(NotTrawl::Disagrees);
            }
            Ok((
                VerifiedApi {
                    url: self.url,
                    trust: self.trust,
                },
                health,
            ))
        }
    }

    impl VerifiedApi<'_> {
        /// The only place a client that carries a key is built.
        pub(super) fn keyed_client(&self, key: &Key) -> Result<HttpClient, ClientError> {
            client(self.url, key.secret(), self.trust)
        }
    }
}

/// Whether `result` holds an HTTP answer, whatever its status. A body too
/// large to read, one that stalled after the headers, or one that broke
/// off after them still followed an answer's status line.
fn answered<T>(result: &Result<T, ClientError>) -> bool {
    match result {
        Ok(_)
        | Err(ClientError::Server { .. } | ClientError::Parse(_) | ClientError::TooLarge { .. }) => {
            true
        }
        Err(e) => matches!(
            e.network_kind(),
            Some(NetworkKind::Redirect | NetworkKind::BodyTimeout | NetworkKind::BodyRead)
        ),
    }
}

/// The trust mode, named as the report names it.
const fn trust_name(trust: &TlsTrust) -> &'static str {
    match trust {
        TlsTrust::System => "system roots",
        TlsTrust::PinnedCa(_) => "the pinned CA",
        TlsTrust::AcceptInvalid => "certificate verification off",
    }
}

/// The state the `api.*` steps share, in the order the runner calls them.
pub struct ApiRun<'a> {
    connection: &'a Connection,
    probe: Option<witness::Probe>,
    tls: Option<witness::VerifiedTls<'a>>,
    verified: Option<witness::VerifiedApi<'a>>,
    health: Option<HealthResponse>,
    identity: Option<WhoAmIResponse>,
    notes: Vec<String>,
}

/// Names the origin and how far the run got; a probe's error and the key
/// never reach it.
impl std::fmt::Debug for ApiRun<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiRun")
            .field("origin", &self.connection.url.origin())
            .field("probed", &self.probe.is_some())
            .field("tls_verified", &self.tls.is_some())
            .field("verified", &self.verified.is_some())
            .finish_non_exhaustive()
    }
}

impl<'a> ApiRun<'a> {
    pub fn new(connection: &'a Connection) -> Self {
        Self {
            connection,
            probe: None,
            tls: None,
            verified: None,
            health: None,
            identity: None,
            notes: Vec::new(),
        }
    }

    /// `api.transport`: send the health probe and say whether a connection
    /// opened.
    pub async fn transport(&mut self) -> Check {
        let probe = witness::probe(self.connection).await;
        let check = transport_row(&probe);
        self.probe = Some(probe);
        check
    }

    /// `api.tls`: judge the certificate from the probe, and mint the TLS
    /// witness when it verified.
    pub fn tls(&mut self) -> Check {
        let probe = self
            .probe
            .take()
            .expect("api.tls runs only after api.transport completed");
        let connection = self.connection;
        let mut check = row(API_TLS, Outcome::Failed);
        if connection.url.scheme() == Scheme::Http {
            return with_next(
                with_reason(check, "connection is not TLS"),
                "use an https URL for the API",
            );
        }
        if matches!(connection.trust, TlsTrust::AcceptInvalid) {
            return with_next(
                with_reason(check, "certificate not verified"),
                "turn insecure off (the --insecure flag or the profile's insecure) and set \
                 ca_cert to the server's CA in a CLI profile",
            );
        }
        if probe
            .result
            .as_ref()
            .err()
            .and_then(ClientError::network_kind)
            == Some(NetworkKind::UntrustedCertificate)
        {
            check.reason = Some(format!(
                "certificate not trusted under {}",
                trust_name(&connection.trust)
            ));
            let next = match connection.trust {
                TlsTrust::PinnedCa(_) => {
                    "point ca_cert at the CA that issued the server's certificate"
                }
                _ => {
                    "set ca_cert to the server's CA in a CLI profile, then run trawl doctor \
                     --profile NAME"
                }
            };
            return with_next(check, next);
        }
        match witness::verify_tls(connection, probe) {
            Some(tls) => {
                self.tls = Some(tls);
                check.outcome = Outcome::Complete;
                check.detail = Some(format!("verified under {}", trust_name(&connection.trust)));
                check
            }
            // Unreachable: api.transport completes only on an answer or an
            // untrusted certificate, and both are handled above.
            None => with_reason(check, "the certificate could not be judged"),
        }
    }

    /// The selected key's secret, for redaction.
    fn secret(&self) -> Option<&'a str> {
        self.connection
            .key
            .as_ref()
            .map(super::resolve::Key::secret)
    }

    /// `api.health` and its `api.health.<key>` rows, from the probe's
    /// answer. The runner calls it only after `api.tls` completed. A trawl
    /// health body, one that parses, carries both `checks` and `version`,
    /// and came under the HTTP status trawld sends with its `status`, mints
    /// the API witness, the only way `api.identity` can send the key;
    /// anything else fails here and blocks it.
    pub fn health(&mut self) -> Vec<Check> {
        let tls = self
            .tls
            .take()
            .expect("api.health reads only an answer observed under verified TLS");
        let check = row(API_HEALTH, Outcome::Failed);
        let health = match tls.into_api() {
            Ok((verified, health)) => {
                self.verified = Some(verified);
                health
            }
            Err(witness::NotTrawl::Answer(e)) => return vec![answer_failure(check, &e, "health")],
            Err(witness::NotTrawl::Unsigned) => {
                return vec![with_next(
                    with_reason(check, "not a trawl health answer"),
                    "check that the URL names trawld's API, not another service",
                )];
            }
            Err(witness::NotTrawl::Disagrees) => {
                return vec![with_next(
                    with_reason(check, "status and body disagree"),
                    "check that the URL names trawld's API, not a proxy or another service",
                )];
            }
        };
        let status = match health.status {
            HealthStatus::Ok => "ok",
            HealthStatus::Degraded => "degraded",
            HealthStatus::Unavailable => "unavailable",
        };
        let secret = self.secret();
        let mut detail = format!("status: {status}");
        if let Some(version) = &health.version {
            let shown = display_safe(version, secret);
            detail.push_str("; server version ");
            detail.push_str(&shown);
            let cli = env!("CARGO_PKG_VERSION");
            if version != cli {
                self.notes.push(format!(
                    "this CLI is version {cli} and the server reports version {shown}"
                ));
            }
        }
        let mut rows = vec![Check {
            outcome: Outcome::Complete,
            detail: Some(detail),
            ..check
        }];
        // The witness exists, so the answer carried a checks map.
        if let Some(checks) = &health.checks {
            rows.extend(health_rows(checks, secret));
        }
        self.health = Some(health);
        rows
    }

    /// `api.identity`: prove the key with `whoami`. The runner calls it
    /// only after `api.health` completed, which is when the API witness
    /// exists.
    pub async fn identity(&mut self) -> Check {
        let mut check = row(API_IDENTITY, Outcome::NotConfigured);
        let Some(key) = &self.connection.key else {
            return with_next(
                with_reason(check, "no key selected"),
                "to check a key, name it with --token-env or --token-file, or set token in the \
                 profile",
            );
        };
        check.source = Some(key.source().to_owned());
        let verified = self
            .verified
            .as_ref()
            .expect("api.identity runs only after api.health completed");
        let Ok(api) = verified.keyed_client(key) else {
            check.outcome = Outcome::Failed;
            return with_reason(check, "the HTTP client could not be built");
        };
        let result = api.whoami().await;
        match result {
            Ok(who) => {
                check.outcome = Outcome::Complete;
                check.detail = Some(identity_detail(&who, key.secret()));
                self.identity = Some(who);
                check
            }
            Err(ClientError::Server { status: 401, .. }) => {
                check.outcome = Outcome::Failed;
                with_next(
                    with_reason(check, "key rejected"),
                    "check the key: it may be revoked, expired, or mistyped",
                )
            }
            Err(ClientError::Server { status: 403, .. }) => {
                check.outcome = Outcome::Failed;
                with_next(
                    with_reason(check, "key has no permissions"),
                    "give the key a role that grants trawl permissions",
                )
            }
            // trawld's whoami answers exactly 200.
            Err(ClientError::Server { status, .. }) if (200..300).contains(&status) => {
                check.outcome = Outcome::Failed;
                check.detail = Some(format!("GET /api/v1/whoami answered HTTP {status}"));
                with_next(
                    with_reason(check, "unexpected status"),
                    "check that the URL names trawld's API, not a proxy or another service",
                )
            }
            Err(e) => answer_failure(check, &e, "whoami"),
        }
    }

    /// Report-level notes gathered along the way, plus the ones that need
    /// both health and identity.
    pub fn into_notes(mut self) -> Vec<String> {
        if let (Some(health), Some(who)) = (&self.health, &self.identity) {
            let capacity_ok = health
                .checks
                .as_ref()
                .and_then(|checks| checks.get("ingest_capacity"))
                .is_some_and(|value| value == "ok");
            if capacity_ok && !who.permissions.iter().any(|p| p == "ingest") {
                self.notes.push(
                    "ingest_capacity reports ok, but this key lacks the ingest permission, so it \
                     cannot send events"
                        .to_owned(),
                );
            }
        }
        self.notes
    }
}

/// `api.transport` from the probe.
fn transport_row(probe: &witness::Probe) -> Check {
    let check = row(API_TRANSPORT, Outcome::Complete);
    if probe.not_built {
        return not_built_row(check, probe.result.as_ref().err());
    }
    if probe.answered() {
        return check;
    }
    let kind = probe
        .result
        .as_ref()
        .err()
        .and_then(ClientError::network_kind);
    match kind {
        // The TCP connection opened and the handshake reached the
        // certificate; api.tls reports the rest.
        Some(NetworkKind::UntrustedCertificate) => check,
        Some(NetworkKind::Timeout) => with_next(
            with_reason(
                Check {
                    outcome: Outcome::NotSampled,
                    ..check
                },
                reason::TIMED_OUT,
            ),
            "check that the host and port are reachable from here; no answer came within 10 s",
        ),
        _ => with_next(
            with_reason(
                Check {
                    outcome: Outcome::Failed,
                    ..check
                },
                "connection failed",
            ),
            "check that trawld is running and that the URL's host and port reach its API",
        ),
    }
}

/// `api.transport` when the probe client could not be built. Only a pinned
/// CA that holds no usable certificate points at `ca_cert`; any other build
/// failure, including every one under `--url`'s system trust, does not.
fn not_built_row(check: Check, error: Option<&ClientError>) -> Check {
    let (reason, next) = match error {
        Some(ClientError::InvalidCa(_)) => (
            "the pinned CA holds no usable certificate",
            "point ca_cert at a PEM file that holds the server's CA certificate",
        ),
        _ => (
            "the HTTP client could not be built",
            "run trawl doctor again; if it fails the same way, report the TLS setup error",
        ),
    };
    with_next(
        with_reason(
            Check {
                outcome: Outcome::Failed,
                ..check
            },
            reason,
        ),
        next,
    )
}

/// The outcome of a request that got something other than the answer it
/// wanted. `what` names the endpoint in the reason.
fn answer_failure(mut check: Check, e: &ClientError, what: &str) -> Check {
    check.outcome = Outcome::Failed;
    match e {
        ClientError::Server { status: 429, .. } => {
            check.outcome = Outcome::NotSampled;
            with_next(
                with_reason(check, reason::RATE_LIMITED),
                "wait a minute, then run trawl doctor again",
            )
        }
        ClientError::Server { status, .. } if (300..400).contains(status) => with_next(
            with_reason(check, "redirect refused"),
            "point the URL at trawld's API itself, not at an address that redirects",
        ),
        ClientError::Server { status, .. } => with_next(
            with_reason(check, format!("HTTP {status} is not a {what} answer")),
            "check that the URL names trawld's API, not a proxy or another service, and read \
             the server's log",
        ),
        ClientError::Parse(_) => with_next(
            with_reason(check, format!("the answer is not a trawl {what} response")),
            "check that the URL names trawld's API, not another service",
        ),
        ClientError::TooLarge { .. } => with_next(
            with_reason(check, "response too large"),
            "check that the URL names trawld's API, not another service",
        ),
        _ => match e.network_kind() {
            Some(NetworkKind::Redirect) => with_next(
                with_reason(check, "redirect refused"),
                "point the URL at trawld's API itself, not at an address that redirects",
            ),
            Some(NetworkKind::Timeout) => {
                check.outcome = Outcome::NotSampled;
                with_next(
                    with_reason(check, reason::TIMED_OUT),
                    "run trawl doctor again; no answer came within 10 s",
                )
            }
            // The server answered, so transport and TLS stand; only this
            // answer could not be read.
            Some(NetworkKind::BodyTimeout) => {
                check.outcome = Outcome::NotSampled;
                with_next(
                    with_reason(check, reason::TIMED_OUT),
                    "run trawl doctor again; the answer began but did not finish within 10 s",
                )
            }
            // The server answered, so transport and TLS stand; this answer
            // broke off, which the doctor saw, so the check failed.
            Some(NetworkKind::BodyRead) => with_next(
                with_reason(check, "response body broken"),
                "run trawl doctor again; if it repeats, check for a proxy between here and the \
                 server that cuts answers short, and read the server's log",
            ),
            Some(NetworkKind::UntrustedCertificate) => with_next(
                with_reason(check, "certificate not trusted"),
                "the server's certificate changed during the run; run trawl doctor again",
            ),
            _ => with_next(
                with_reason(check, "the request failed"),
                "run trawl doctor again, and check the network path to the server",
            ),
        },
    }
}

/// One row per check name the server reports, sorted by name. Names that
/// are not identifiers, or that hold a run of the key, become one
/// `api.health._invalid` row, and none of them is echoed.
fn health_rows(checks: &HashMap<String, String>, secret: Option<&str>) -> Vec<Check> {
    let mut named = BTreeMap::new();
    let mut invalid = 0usize;
    for (name, value) in checks {
        if is_health_key(name) && !holds_key(name, secret) {
            named.insert(name.as_str(), value.as_str());
        } else {
            invalid += 1;
        }
    }
    let mut rows: Vec<Check> = named
        .into_iter()
        .map(|(name, value)| health_row(name, value, secret))
        .collect();
    if invalid > 0 {
        rows.push(with_next(
            Check {
                detail: Some(format!("{invalid} such name(s), not shown")),
                ..with_reason(
                    row(API_HEALTH_INVALID_KEY, Outcome::Failed),
                    "the server reported a check name that is not [a-z][a-z0-9_]{0,63}",
                )
            },
            "check that the URL names trawld's API; its check names are plain identifiers",
        ));
    }
    rows
}

/// The health check that trawld reports while its corpus recovers after a
/// restart (ADR-0041). Only this key has values beyond `ok`, `error`, and
/// `refusing`.
const CORPUS_KEY: &str = "corpus";

fn health_row(name: &str, value: &str, secret: Option<&str>) -> Check {
    let id = format!("{API_HEALTH_KEY_PREFIX}{name}");
    match value {
        "ok" => Check {
            detail: Some("reported ok".to_owned()),
            ..row(&id, Outcome::Complete)
        },
        // A recovering corpus is not a failure: the server cannot yet vouch
        // for what it holds, so the doctor could not look (ADR-0047).
        "rollup_pending" | "restart_backlog" if name == CORPUS_KEY => with_next(
            Check {
                detail: Some(format!("reported {value}")),
                ..with_reason(row(&id, Outcome::NotSampled), reason::RECOVERING)
            },
            "wait for trawld to finish recovery, then rerun",
        ),
        "error" | "refusing" => with_next(
            with_reason(row(&id, Outcome::Failed), format!("reported {value}")),
            format!("read the server's log for why {name} reports {value}"),
        ),
        other if is_quotable_value(other) && !holds_key(other, secret) => with_next(
            with_reason(
                row(&id, Outcome::Failed),
                format!("reported {other}, a value this CLI does not know"),
            ),
            format!("read the server's log for {name}, or update this CLI"),
        ),
        _ => with_next(
            with_reason(row(&id, Outcome::Failed), "unrecognized value"),
            format!("read the server's log for {name}"),
        ),
    }
}

/// The most characters of the key's name the report shows.
const NAME_MAX_CHARS: usize = 32;

/// The key's name, kind and permissions, never its prefix.
///
/// Only the name is the server's own text. `kind` is already one of
/// trawl's principal kinds: whoami's `kind` decodes only as `human` or
/// `service`, so any other value fails the whole answer. Permissions are
/// shown only when they are exactly one of trawl's permission names, in
/// canonical order and printed from that list; anything else is counted
/// and never shown. A key split into short pieces, one per permission,
/// therefore never reaches the report.
fn identity_detail(who: &WhoAmIResponse, secret: &str) -> String {
    let known: Vec<&str> = trawl_api::PERMISSION_NAMES
        .iter()
        .copied()
        .filter(|name| who.permissions.iter().any(|p| p == name))
        .collect();
    let unrecognized = who
        .permissions
        .iter()
        .filter(|p| !trawl_api::PERMISSION_NAMES.contains(&p.as_str()))
        .count();
    let mut permissions = if known.is_empty() {
        "none".to_owned()
    } else {
        known.join(", ")
    };
    if unrecognized > 0 {
        write!(permissions, "; {unrecognized} unrecognized").expect("writing to a String");
    }
    // The name is shown as the server sent it, stripped of control
    // characters, with every literal run of the key redacted, and capped
    // at NAME_MAX_CHARS. Redaction catches only the key's own characters:
    // a server that already holds the key could still echo it encoded
    // (base64, hex, reversed) in the name, and up to NAME_MAX_CHARS of
    // that encoding would reach the report. That residual risk is
    // accepted: such a server already has the key, and the cap bounds how
    // much of it one report can carry.
    format!(
        "name: {}; kind: {}; permissions: {permissions}",
        display_safe_within(&who.name, Some(secret), NAME_MAX_CHARS),
        who.kind.as_str()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_build_failure_names_ca_cert_only_for_a_bad_ca() {
        let bad_ca = ClientError::InvalidCa("no certificate".to_owned());
        let check = not_built_row(row(API_TRANSPORT, Outcome::Complete), Some(&bad_ca));
        assert_eq!(check.outcome, Outcome::Failed);
        assert_eq!(
            check.reason.as_deref(),
            Some("the pinned CA holds no usable certificate")
        );
        assert!(
            check
                .next_action
                .as_deref()
                .is_some_and(|n| n.contains("ca_cert")),
            "{check:?}"
        );

        for other in [
            ClientError::InvalidUrl("unsupported".to_owned()),
            ClientError::Parse("tls backend".to_owned()),
        ] {
            let check = not_built_row(row(API_TRANSPORT, Outcome::Complete), Some(&other));
            assert_eq!(check.outcome, Outcome::Failed, "{other:?}");
            assert_eq!(
                check.reason.as_deref(),
                Some("the HTTP client could not be built"),
                "{other:?}"
            );
            let next = check.next_action.as_deref().expect("a next action");
            assert!(!next.contains("ca_cert"), "{other:?}: {next}");
        }
    }

    #[test]
    fn health_keys_and_values_follow_the_patterns() {
        assert!(is_health_key("duckdb"));
        assert!(is_health_key("ingest_capacity"));
        assert!(is_health_key(&"a".repeat(64)));
        assert!(!is_health_key(&"a".repeat(65)));
        assert!(!is_health_key(""));
        assert!(!is_health_key("Bad-Key"));
        assert!(!is_health_key("x\u{1b}[31m"));
        assert!(!is_health_key("_invalid"));
        assert!(!is_health_key("0day"));
        assert!(is_health_key("a0_"));
        assert!(is_quotable_value("recovering"));
        assert!(!is_quotable_value(&"a".repeat(33)));
        assert!(!is_quotable_value("Weird Value!"));
    }

    #[test]
    fn health_rows_sort_and_map_values() {
        let checks = HashMap::from([
            ("duckdb".to_owned(), "error".to_owned()),
            ("wal".to_owned(), "recovering".to_owned()),
            ("corpus".to_owned(), "rollup_pending".to_owned()),
            ("storage_db".to_owned(), "restart_backlog".to_owned()),
            ("auth_db".to_owned(), "ok".to_owned()),
            ("ingest_capacity".to_owned(), "refusing".to_owned()),
            ("data_path".to_owned(), "Weird\u{1b}Value".to_owned()),
            ("Bad-Key".to_owned(), "ok".to_owned()),
            ("x\u{202e}y".to_owned(), "ok".to_owned()),
            ("_invalid".to_owned(), "ok".to_owned()),
        ]);
        let rows = health_rows(&checks, None);
        let summary: Vec<(&str, Outcome, Option<&str>)> = rows
            .iter()
            .map(|c| (c.id.as_str(), c.outcome, c.reason.as_deref()))
            .collect();
        assert_eq!(
            summary,
            [
                ("api.health.auth_db", Outcome::Complete, None),
                (
                    "api.health.corpus",
                    Outcome::NotSampled,
                    Some(reason::RECOVERING)
                ),
                (
                    "api.health.data_path",
                    Outcome::Failed,
                    Some("unrecognized value")
                ),
                ("api.health.duckdb", Outcome::Failed, Some("reported error")),
                (
                    "api.health.ingest_capacity",
                    Outcome::Failed,
                    Some("reported refusing")
                ),
                (
                    "api.health.storage_db",
                    Outcome::Failed,
                    Some("reported restart_backlog, a value this CLI does not know")
                ),
                (
                    "api.health.wal",
                    Outcome::Failed,
                    Some("reported recovering, a value this CLI does not know")
                ),
                (
                    API_HEALTH_INVALID_KEY,
                    Outcome::Failed,
                    Some("the server reported a check name that is not [a-z][a-z0-9_]{0,63}")
                ),
            ]
        );
        let all = format!("{rows:?}");
        assert!(!all.contains("Bad-Key") && !all.contains('\u{202e}') && !all.contains("Weird"));
    }

    #[test]
    fn identity_detail_never_shows_the_prefix() {
        let who = WhoAmIResponse {
            prefix: "pfx12345".to_owned(),
            name: "ops\u{1b}[2Jkey".to_owned(),
            kind: trawl_api::PrincipalKind::Service,
            roles: vec!["admin".to_owned()],
            permissions: vec!["query".to_owned(), "ingest".to_owned()],
        };
        let detail = identity_detail(&who, "flt_pfx12345restofthekey");
        assert_eq!(
            detail,
            "name: ops[2Jkey; kind: service; permissions: query, ingest"
        );
        assert!(!detail.contains("pfx12345"));
    }
}
