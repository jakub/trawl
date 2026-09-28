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
//! which trawld always sends. An untrusted certificate, an `insecure`
//! trust, a plain `http` URL, a redirect, or any answer that is not trawl's
//! health body therefore never sees a key.
//!
//! Every outcome is decided from [`ClientError::network_kind`] or a
//! [`ClientError::Server`] status, never from an error's text, and no
//! error's `Display` or `Debug` reaches the report. Strings the server sent
//! pass through [`display_safe`] first.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use trawl_api::doctor::{Check, Outcome, reason};
use trawl_api::{HealthResponse, HealthStatus, WhoAmIResponse};
use trawl_client::{ClientError, NetworkKind, TlsTrust};

use super::display_safe;
use super::resolve::{Connection, Scheme};

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
pub const API_HEALTH_INVALID_KEY: &str = "api.health.invalid_key";

/// The deadline for each request the doctor sends.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// A check name the report may show: `[a-z0-9_]{1,64}`.
fn is_health_key(name: &str) -> bool {
    (1..=64).contains(&name.len())
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
    use trawl_api::HealthResponse;
    use trawl_client::{ClientError, HttpClient, TlsTrust};

    use super::super::resolve::{CheckedUrl, Connection, Key, Scheme};
    use super::REQUEST_TIMEOUT;

    /// The unkeyed health request and what came back. Only [`probe`]
    /// builds one, so a `Probe` that says an answer arrived is one that did.
    pub(super) struct Probe {
        /// Whether the request went out under the connection's own trust.
        /// A plain `http` URL is probed without TLS, so it is false there.
        under_configured_trust: bool,
        /// What the request returned. `Err` from building the client too.
        pub(super) result: Result<HealthResponse, ClientError>,
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
                result: api.health().await,
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
        answer: Result<HealthResponse, ClientError>,
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
    /// parsed as a trawl health body (a 200, or a 503 carrying one) with
    /// both a `checks` map and a `version`. Only [`VerifiedTls::into_api`]
    /// mints one.
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
    }

    impl<'a> VerifiedTls<'a> {
        /// The health answer, and with it the API witness when that answer
        /// is trawl's health body. A redirect, another status, a body that
        /// does not parse, or one without both `checks` and `version` comes
        /// back as the refusal, and no witness.
        pub(super) fn into_api(self) -> Result<(VerifiedApi<'a>, HealthResponse), NotTrawl> {
            let health = self.answer.map_err(NotTrawl::Answer)?;
            if health.checks.is_none() || health.version.is_none() {
                return Err(NotTrawl::Unsigned);
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

/// Whether `result` holds an HTTP answer, whatever its status.
fn answered<T>(result: &Result<T, ClientError>) -> bool {
    match result {
        Ok(_) | Err(ClientError::Server { .. } | ClientError::Parse(_)) => true,
        Err(e) => e.network_kind() == Some(NetworkKind::Redirect),
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

    /// `api.health` and its `api.health.<key>` rows, from the probe's
    /// answer. The runner calls it only after `api.tls` completed. A trawl
    /// health body, one that parses and carries both `checks` and
    /// `version`, mints the API witness, the only way `api.identity` can
    /// send the key; anything else fails here and blocks it.
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
        };
        let status = match health.status {
            HealthStatus::Ok => "ok",
            HealthStatus::Degraded => "degraded",
            HealthStatus::Unavailable => "unavailable",
        };
        let mut detail = format!("status: {status}");
        if let Some(version) = &health.version {
            detail.push_str("; server version ");
            detail.push_str(&display_safe(version));
            let cli = env!("CARGO_PKG_VERSION");
            if version != cli {
                self.notes.push(format!(
                    "this CLI is version {cli} and the server reports version {}",
                    display_safe(version)
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
            rows.extend(health_rows(checks));
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
                check.detail = Some(identity_detail(&who));
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
        let reason = match probe.result {
            Err(ClientError::InvalidCa(_)) => "the pinned CA holds no usable certificate",
            _ => "the HTTP client could not be built",
        };
        return with_next(
            with_reason(
                Check {
                    outcome: Outcome::Failed,
                    ..check
                },
                reason,
            ),
            "point ca_cert at a PEM file that holds the server's CA certificate",
        );
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
/// are not identifiers become one `api.health.invalid_key` row, and none of
/// them is echoed.
fn health_rows(checks: &HashMap<String, String>) -> Vec<Check> {
    let mut named = BTreeMap::new();
    let mut invalid = 0usize;
    for (name, value) in checks {
        if is_health_key(name) {
            named.insert(name.as_str(), value.as_str());
        } else {
            invalid += 1;
        }
    }
    let mut rows: Vec<Check> = named
        .into_iter()
        .map(|(name, value)| health_row(name, value))
        .collect();
    if invalid > 0 {
        rows.push(with_next(
            Check {
                detail: Some(format!("{invalid} such name(s), not shown")),
                ..with_reason(
                    row(API_HEALTH_INVALID_KEY, Outcome::Failed),
                    "the server reported a check name that is not [a-z0-9_]{1,64}",
                )
            },
            "check that the URL names trawld's API; its check names are plain identifiers",
        ));
    }
    rows
}

fn health_row(name: &str, value: &str) -> Check {
    let id = format!("{API_HEALTH_KEY_PREFIX}{name}");
    match value {
        "ok" => Check {
            detail: Some("reported ok".to_owned()),
            ..row(&id, Outcome::Complete)
        },
        "error" | "refusing" => with_next(
            with_reason(row(&id, Outcome::Failed), format!("reported {value}")),
            format!("read the server's log for why {name} reports {value}"),
        ),
        other if is_quotable_value(other) => with_next(
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

/// The key's name, kind and permissions, never its prefix.
fn identity_detail(who: &WhoAmIResponse) -> String {
    let permissions = if who.permissions.is_empty() {
        "none".to_owned()
    } else {
        who.permissions
            .iter()
            .map(|p| display_safe(p))
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "name: {}; kind: {}; permissions: {permissions}",
        display_safe(&who.name),
        who.kind.as_str()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_keys_and_values_follow_the_patterns() {
        assert!(is_health_key("duckdb"));
        assert!(is_health_key("ingest_capacity"));
        assert!(is_health_key(&"a".repeat(64)));
        assert!(!is_health_key(&"a".repeat(65)));
        assert!(!is_health_key(""));
        assert!(!is_health_key("Bad-Key"));
        assert!(!is_health_key("x\u{1b}[31m"));
        assert!(is_quotable_value("recovering"));
        assert!(!is_quotable_value(&"a".repeat(33)));
        assert!(!is_quotable_value("Weird Value!"));
    }

    #[test]
    fn health_rows_sort_and_map_values() {
        let checks = HashMap::from([
            ("duckdb".to_owned(), "error".to_owned()),
            ("corpus".to_owned(), "recovering".to_owned()),
            ("auth_db".to_owned(), "ok".to_owned()),
            ("ingest_capacity".to_owned(), "refusing".to_owned()),
            ("data_path".to_owned(), "Weird\u{1b}Value".to_owned()),
            ("Bad-Key".to_owned(), "ok".to_owned()),
            ("x\u{202e}y".to_owned(), "ok".to_owned()),
        ]);
        let rows = health_rows(&checks);
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
                    Outcome::Failed,
                    Some("reported recovering, a value this CLI does not know")
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
                    API_HEALTH_INVALID_KEY,
                    Outcome::Failed,
                    Some("the server reported a check name that is not [a-z0-9_]{1,64}")
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
        let detail = identity_detail(&who);
        assert_eq!(
            detail,
            "name: ops[2Jkey; kind: service; permissions: query, ingest"
        );
        assert!(!detail.contains("pfx12345"));
    }
}
