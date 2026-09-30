// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The upstream: `proxy.upstream.trust` and `proxy.upstream.health`,
//! with its `proxy.upstream.health.<key>` rows.
//!
//! `proxy.upstream.trust` owns whatever error arrives in the upstream slot
//! of [`Ctx`]: a URL that is not https, carries a user name, password,
//! query or fragment, or names no host, a connect address that does not
//! fit it, and a pin path that is empty or not UTF-8. Each is a fixed
//! sentence, and no request is sent. With a pinned CA it reads the file
//! once through [`read::read_pin`], which expands `~` in the path on the
//! reading thread, and parses it with the production parser. A pin that
//! expands to an empty path fails as an empty pin does, as startup refuses
//! both alike. The row shows the path as written. The
//! roots go to the probe through [`Ctx::pinned_roots`], so the probe
//! trusts exactly what was checked, and the file is never read twice. A
//! pin that does not exist yet is `not_sampled`, reason `ca_not_present`:
//! trawld writes its generated certificate on its first start.
//!
//! `proxy.upstream.health` sends one anonymous `GET /api/v1/health` through
//! the production client rules ([`upstream_client`]): https only, no
//! redirect, no proxy, the connect address, the pinned roots only. The
//! doctor adds its own timeouts, no retry and no idle pool, so the probe is
//! one request on one connection. Under the platform roots, building that
//! client loads them, as startup's does: `SSL_CERT_FILE`, `SSL_CERT_DIR` or
//! the system store, read synchronously. The build runs on the blocking
//! pool under [`CLIENT_DEADLINE`], so a bundle on a stalled mount costs the
//! probe its answer, `timed_out`, and not the run. Trust still reports the
//! platform roots as the configured trust (D5): whether they load is the
//! probe's first step. The status is judged first: a redirect is
//! refused without reading where it points, and any status but 200 or 503
//! fails without reading the body. The body is read in chunks up to
//! [`BODY_MAX`] and judged by [`health::judge`], the judge every doctor
//! shares, and mapped to rows as `trawld --doctor` maps them.
//!
//! A request that fails is classified by walking its error's typed sources,
//! never by its text, which can quote the URL or a certificate name. No
//! error is ever formatted.

use std::error::Error as StdError;
use std::io;
use std::time::Duration;

use reqwest::Client;
use trawl_api::HealthStatus;
use trawl_api::doctor::health::{self, BODY_MAX, Health, KeyAnswer, Reported};
use trawl_api::doctor::{Outcome, reason};

use super::output::{HealthKey, Row, SelectedPath, Selection, Text};
use super::read::{self, PinFault, ReadFault};
use super::{Ctx, Runner, Unfinished, WebCheck, blocking_within};
use crate::config::{
    ConfigError, ConnectAddrError, ENV_UPSTREAM_CA_PATH, PinnedRoots, TrustSource, UpstreamPlan,
    UpstreamUrlError, pinned_roots,
};
use crate::upstream::upstream_client;

/// The checks this group runs, in [`WebCheck::ALL`] order.
pub(super) const CHECKS: [WebCheck; 2] = [WebCheck::UpstreamTrust, WebCheck::UpstreamHealth];

/// The path the probe asks, joined to the upstream URL as the proxy joins
/// every forwarded path.
const HEALTH_PATH: &str = "/api/v1/health";

/// How long the probe's connection, TCP and TLS, may take to open.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the whole probe may take, from dialing to the body's end.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// A backstop past [`REQUEST_TIMEOUT`], around the send and the body read
/// together, so no step of the probe can hold the run.
const PROBE_DEADLINE: Duration = Duration::from_secs(11);

/// How long building the probe's client may take: a file read's budget.
/// Under the platform roots the build reads them, from `SSL_CERT_FILE`,
/// `SSL_CERT_DIR` or the system store, and a stalled mount or a FIFO there
/// would hold it; [`PROBE_DEADLINE`] starts only once the client exists.
const CLIENT_DEADLINE: Duration = read::READ_DEADLINE;

/// Run the group's checks, each through the runner's gate.
pub(super) async fn run(runner: &mut Runner, ctx: &mut Ctx) {
    for check in CHECKS {
        let Some(gate) = runner.gate(check) else {
            continue;
        };
        match gate.check() {
            WebCheck::UpstreamTrust => {
                let row = check_trust(ctx).await;
                runner.record(gate, row);
            }
            WebCheck::UpstreamHealth => {
                let (row, keyed) = check_health(ctx).await;
                runner.record_with(gate, row, keyed);
            }
            other => unreachable!("{other:?} is not an upstream check"),
        }
    }
}

// -- proxy.upstream.trust ----------------------------------------------------

/// `proxy.upstream.trust`: the upstream slot resolved, and the trust it
/// names is usable. Fills [`Ctx::pinned_roots`] when a pinned CA read and
/// parsed.
async fn check_trust(ctx: &mut Ctx) -> Row {
    let check = WebCheck::UpstreamTrust;
    let plan = match &ctx.upstream {
        Ok(plan) => plan,
        Err(error) => return upstream_fault(error, &ctx.config_path),
    };
    let url_from = Text::new("upstream URL: ").setting(plan.url_from);
    let dials = |text: Text| {
        if plan.connect.is_some() {
            text.lit("; it dials upstream_connect_addr and verifies the URL's host name")
        } else {
            text
        }
    };
    match &plan.trust {
        TrustSource::System => Row::complete(check)
            .detail(dials(Text::new("trusts the platform roots")))
            .source(url_from),
        TrustSource::Pinned { path, from } => {
            // Shown as written; `~` expands on the reading thread.
            let shown = SelectedPath::new(Selection::UpstreamCaPath, path);
            let source = url_from.lit("; CA: ").setting(*from);
            let row = match read::read_pin(path.clone()).await {
                Ok(bytes) => match pinned_roots(&bytes) {
                    Ok(roots) => {
                        let count = u32::try_from(roots.certificates().len()).unwrap_or(u32::MAX);
                        ctx.pinned_roots = Some(roots);
                        Row::complete(check).detail(dials(
                            Text::new("trusts only the ")
                                .int(count)
                                .lit(" certificate(s) in ")
                                .path(&shown),
                        ))
                    }
                    Err(why) => Row::failed(check, why)
                        .detail(Text::new("read ").path(&shown))
                        .next(Text::new(
                            "point upstream_ca_path at trawld's CA certificate, in PEM",
                        )),
                },
                Err(fault) => pin_fault(fault, &shown, &ctx.config_path),
            };
            row.source(source)
        }
    }
}

/// The trust row for a pinned CA file the bounded reader did not read.
/// `config` is the `--config` path, which an empty pin's fix names.
fn pin_fault(fault: PinFault, shown: &SelectedPath, config: &SelectedPath) -> Row {
    let check = WebCheck::UpstreamTrust;
    let fix = || Text::new("point upstream_ca_path at trawld's CA certificate, a regular file");
    let fault = match fault {
        PinFault::Empty => {
            return empty_pin(config).detail(
                Text::new("")
                    .path(shown)
                    .lit(" expands to an empty path: this user's home directory is empty"),
            );
        }
        PinFault::Read(fault) => fault,
    };
    match fault {
        // trawld writes its generated certificate on its first start, which
        // may come after trawl-web's; the proxy starts and waits for it.
        ReadFault::Missing => Row::not_sampled(check, reason::CA_NOT_PRESENT)
            .detail(Text::new("nothing exists yet at ").path(shown))
            .next(Text::new("start trawld; it writes this certificate")),
        ReadFault::PermissionDenied => Row::not_sampled(check, reason::PERMISSION_DENIED)
            .detail(Text::new("this user may not read ").path(shown))
            .next(Text::new(
                "rerun as the service user, which can read the CA file",
            )),
        ReadFault::NotRegular => {
            Row::failed(check, "the CA path is not a regular file").next(fix())
        }
        ReadFault::SymlinkLoop => Row::failed(check, "the CA path is a symlink loop").next(fix()),
        ReadFault::TooLarge => Row::failed(check, "the CA file is too large")
            .detail(
                Text::new("the proxy reads at most ")
                    .int(read::cap::CA)
                    .lit(" bytes of it"),
            )
            .next(fix()),
        ReadFault::TimedOut => Row::not_sampled(check, reason::TIMED_OUT)
            .detail(Text::new("reading the CA file did not finish in time")),
        ReadFault::Io => Row::not_sampled(check, reason::UNREADABLE),
    }
}

/// The trust row for an upstream slot that did not resolve. Only the
/// variants that reach this slot have a sentence of their own; the text of
/// the error is never shown, since the rule the URL broke may be that it
/// holds a password.
fn upstream_fault(error: &ConfigError, config: &SelectedPath) -> Row {
    let check = WebCheck::UpstreamTrust;
    let fix = |what: &'static str| Text::new(what).path(config);
    match error {
        ConfigError::UpstreamUrl(url) => {
            let (why, next) = match url {
                UpstreamUrlError::Unparseable(_) => (
                    "the upstream URL does not parse",
                    "correct upstream_url, or the [server] http_addr it derives from, in ",
                ),
                UpstreamUrlError::Userinfo => (
                    "the upstream URL carries a user name or password",
                    "remove the part before `@` from the upstream URL in ",
                ),
                UpstreamUrlError::QueryOrFragment => (
                    "the upstream URL carries a query or fragment",
                    "remove the part from `?` or `#` from the upstream URL in ",
                ),
                UpstreamUrlError::NotHttps { .. } => (
                    "the upstream URL is not https",
                    "use an https upstream_url in ",
                ),
                UpstreamUrlError::NoHost => (
                    "the upstream URL names no host",
                    "correct upstream_url, or the [server] http_addr it derives from, in ",
                ),
            };
            Row::failed(check, why).next(fix(next))
        }
        ConfigError::UpstreamConnectAddr(addr) => {
            let row = match addr {
                ConnectAddrError::Malformed => {
                    Row::failed(check, "upstream_connect_addr is not an IP address and port")
                }
                ConnectAddrError::PortZero => {
                    Row::failed(check, "upstream_connect_addr has port 0")
                }
                ConnectAddrError::IpHost => Row::failed(
                    check,
                    "upstream_connect_addr needs an upstream URL whose host is a DNS name",
                ),
                ConnectAddrError::PortMismatch {
                    url_port,
                    addr_port,
                } => Row::failed(
                    check,
                    "upstream_connect_addr and the upstream URL name different ports",
                )
                .detail(
                    Text::new("the URL has port ")
                        .int(*url_port)
                        .lit(" and upstream_connect_addr has port ")
                        .int(*addr_port),
                ),
            };
            row.next(fix("correct upstream_connect_addr in "))
        }
        ConfigError::UpstreamCa { .. } => empty_pin(config),
        ConfigError::EnvUtf8 { .. } => Row::failed(
            check,
            "the upstream CA path from the environment is not UTF-8 text",
        )
        .next(
            Text::new("set ")
                .lit(ENV_UPSTREAM_CA_PATH)
                .lit(" to UTF-8 text, or unset it"),
        ),
        // Resolution puts none of these in the upstream slot.
        ConfigError::ReadFile { .. }
        | ConfigError::ParseFile { .. }
        | ConfigError::EnvMissing { .. }
        | ConfigError::EnvKey { .. }
        | ConfigError::KeyFile(_)
        | ConfigError::SessionEnvValue { .. }
        | ConfigError::PublicOrigins { .. }
        | ConfigError::SessionEnvOrigins(_) => {
            Row::failed(check, "the upstream settings do not resolve")
        }
    }
}

/// The trust row for an empty pin, as written or as `~` expanded it:
/// startup refuses both alike rather than fall back to the platform roots.
fn empty_pin(config: &SelectedPath) -> Row {
    Row::failed(WebCheck::UpstreamTrust, "upstream_ca_path is empty").next(
        Text::new(
            "name trawld's CA file, or remove upstream_ca_path to trust the platform roots, in ",
        )
        .path(config),
    )
}

// -- proxy.upstream.health ---------------------------------------------------

/// `proxy.upstream.health` and its `proxy.upstream.health.<key>` rows: one
/// anonymous request through the production client rules and the trust
/// `proxy.upstream.trust` checked.
async fn check_health(ctx: &Ctx) -> (Row, Vec<Row>) {
    // Trust completed, which it does only for a resolved upstream. The
    // slot's error is not formatted, even in a panic.
    let Ok(plan) = &ctx.upstream else {
        unreachable!("proxy.upstream.health runs only after the upstream resolved");
    };
    let pinned = match &plan.trust {
        TrustSource::System => None,
        TrustSource::Pinned { .. } => Some(
            ctx.pinned_roots
                .as_ref()
                .map(PinnedRoots::certificates)
                .expect("a pinned trust completed only with its roots parsed"),
        ),
    };
    let (row, keyed) = probe(plan, pinned).await;
    (
        row.source(Text::new("GET ").lit(HEALTH_PATH).lit(" on the upstream")),
        keyed,
    )
}

/// Send the probe and judge what came back.
async fn probe(plan: &UpstreamPlan, pinned: Option<&[reqwest::Certificate]>) -> (Row, Vec<Row>) {
    let check = WebCheck::UpstreamHealth;
    let client = match build_client(plan, pinned).await {
        Ok(client) => client,
        Err(row) => return (row, Vec::new()),
    };
    // `check_upstream_url` refused any credential before trust completed,
    // so the probe cannot carry one; this holds that line where the request
    // leaves.
    assert!(
        plan.checked_url.username().is_empty() && plan.checked_url.password().is_none(),
        "the probe carries no credential"
    );
    let url = format!("{}{HEALTH_PATH}", plan.url.trim_end_matches('/'));
    let exchange = async {
        let response = match client.get(url).send().await {
            Ok(response) => response,
            Err(error) => return (transport_row(&error), Vec::new()),
        };
        let status = response.status();
        if status.is_redirection() {
            // Dropped unread: where it points is not the doctor's to show,
            // and the proxy would not follow it either.
            return (
                Row::failed(check, reason::REDIRECT_REFUSED)
                    .detail(Text::new("HTTP ").int(status.as_u16()))
                    .next(Text::new(
                        "point upstream_url at trawld itself: the proxy follows no redirect",
                    )),
                Vec::new(),
            );
        }
        let status = status.as_u16();
        if !health::is_health_http_status(status) {
            return (status_failure(status), Vec::new());
        }
        match read_capped(response).await {
            Ok(body) => health_rows(status, &body),
            Err(BodyFault::TooLarge) => (too_large_row(), Vec::new()),
            Err(BodyFault::TimedOut) => (timed_out_row(), Vec::new()),
            Err(BodyFault::Broken) => (
                Row::not_sampled(check, reason::INTERRUPTED)
                    .detail(Text::new(
                        "the health answer's body broke off before it was whole",
                    ))
                    .next(Text::new(
                        "rerun; if it keeps breaking off, read trawld's log",
                    )),
                Vec::new(),
            ),
        }
    };
    tokio::time::timeout(PROBE_DEADLINE, exchange)
        .await
        .unwrap_or_else(|_| (timed_out_row(), Vec::new()))
}

/// The probe's client: the production rules ([`upstream_client`]) over
/// the doctor's timeouts, built on the blocking pool under
/// [`CLIENT_DEADLINE`]. Under the platform roots the build loads them
/// synchronously, as startup's does, so it must not run on the doctor's
/// one runtime thread unbounded.
async fn build_client(
    plan: &UpstreamPlan,
    pinned: Option<&[reqwest::Certificate]>,
) -> Result<Client, Row> {
    let check = WebCheck::UpstreamHealth;
    let pinned = pinned.map(<[reqwest::Certificate]>::to_vec);
    let connect = plan.connect.clone();
    let built = blocking_within(CLIENT_DEADLINE, move || {
        let builder = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .retry(reqwest::retry::never())
            .pool_max_idle_per_host(0);
        upstream_client(builder, pinned.as_deref(), connect.as_ref()).build()
    })
    .await;
    match built {
        Ok(Ok(client)) => Ok(client),
        Ok(Err(_)) => Err(Row::failed(
            check,
            "no upstream client can be built from this trust",
        )),
        Err(Unfinished::TimedOut) => {
            let row = Row::not_sampled(check, reason::TIMED_OUT).detail(Text::new(
                "loading the trusted roots for the probe did not finish in time",
            ));
            Err(match plan.trust {
                TrustSource::System => row.next(Text::new(
                    "check that the system certificate store, and SSL_CERT_FILE or \
                     SSL_CERT_DIR when set, can be read without waiting",
                )),
                TrustSource::Pinned { .. } => row,
            })
        }
        Err(Unfinished::Panicked) => Err(Row::not_sampled(check, reason::UNREADABLE)),
    }
}

fn timed_out_row() -> Row {
    Row::not_sampled(WebCheck::UpstreamHealth, reason::TIMED_OUT)
        .detail(Text::new("the health answer did not arrive in time"))
}

/// What a failed request's typed error sources show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transport {
    /// rustls refused the upstream's certificate.
    CertificateNotTrusted,
    /// A deadline passed.
    TimedOut,
    /// Nothing accepts connections at the upstream's address.
    Refused,
    /// The TLS handshake failed for another reason, or the upstream ended
    /// the connection during it.
    Handshake,
    /// The connection did not open for another reason.
    Connect,
    /// The connection opened, and the exchange failed after it.
    Exchange,
}

/// Classify `error` by walking its sources, into the error an `io::Error`
/// wraps as well: the TLS connector wraps a handshake's error in one, and
/// `io::Error::source` skips what a wrapper holds. Never by its text.
fn classify(error: &reqwest::Error) -> Transport {
    let mut certificate = false;
    let mut tls = false;
    let mut kinds = Vec::new();
    let mut next: Option<&(dyn StdError + 'static)> = Some(error);
    while let Some(err) = next {
        match err.downcast_ref::<rustls::Error>() {
            Some(rustls::Error::InvalidCertificate(_)) => certificate = true,
            Some(_) => tls = true,
            None => {}
        }
        next = match err.downcast_ref::<io::Error>() {
            Some(io) => {
                kinds.push(io.kind());
                io.get_ref().map(|inner| inner as _)
            }
            None => err.source(),
        };
    }
    if certificate {
        Transport::CertificateNotTrusted
    } else if error.is_timeout() || kinds.contains(&io::ErrorKind::TimedOut) {
        Transport::TimedOut
    } else if kinds.contains(&io::ErrorKind::ConnectionRefused) {
        Transport::Refused
    } else if tls || (error.is_connect() && kinds.iter().copied().any(cut_off)) {
        // Opening a TCP connection never reports the peer ending it, so an
        // end while connecting came during the handshake.
        Transport::Handshake
    } else if error.is_connect() {
        Transport::Connect
    } else {
        Transport::Exchange
    }
}

/// Whether an I/O error says the peer ended the connection, by a close or
/// a reset.
const fn cut_off(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::UnexpectedEof
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
    )
}

/// The health row for a request that got no HTTP answer.
fn transport_row(error: &reqwest::Error) -> Row {
    let check = WebCheck::UpstreamHealth;
    match classify(error) {
        Transport::CertificateNotTrusted => Row::failed(check, reason::CERTIFICATE_NOT_TRUSTED)
            .detail(Text::new(
                "the upstream's certificate does not verify against the trusted roots for the \
                 upstream URL's host",
            ))
            .next(Text::new(
                "make upstream_ca_path name the CA that issued trawld's certificate, and \
                 upstream_url name a host that certificate covers",
            )),
        Transport::TimedOut => timed_out_row(),
        Transport::Refused => Row::failed(check, reason::CONNECTION_REFUSED)
            .detail(Text::new(
                "nothing accepts connections at the upstream's address",
            ))
            .next(Text::new(
                "start trawld, or correct upstream_url or upstream_connect_addr",
            )),
        Transport::Handshake => {
            Row::failed(check, "the TLS handshake did not complete").next(Text::new(NOT_TRAWLD))
        }
        Transport::Connect => Row::failed(check, "the connection to the upstream did not open")
            .next(Text::new(
                "check that the upstream's address is reachable from this host",
            )),
        Transport::Exchange => Row::failed(
            check,
            "the upstream did not answer the health request with HTTP",
        )
        .next(Text::new(NOT_TRAWLD)),
    }
}

/// Why the health body was not read whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyFault {
    TooLarge,
    TimedOut,
    Broken,
}

/// The response body, read chunk by chunk and refused past [`BODY_MAX`]
/// bytes, before any of it is parsed.
async fn read_capped(mut response: reqwest::Response) -> Result<Vec<u8>, BodyFault> {
    if response
        .content_length()
        .is_some_and(|length| length > BODY_MAX as u64)
    {
        return Err(BodyFault::TooLarge);
    }
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if body.len() + chunk.len() > BODY_MAX {
                    return Err(BodyFault::TooLarge);
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => return Ok(body),
            Err(error) if error.is_timeout() => return Err(BodyFault::TimedOut),
            Err(_) => return Err(BodyFault::Broken),
        }
    }
}

/// The next action for an answer that is not trawld's.
const NOT_TRAWLD: &str = "check that trawld, and not another service, answers at the upstream URL";

/// The health rows for a body that arrived whole with HTTP `status`, as
/// the judge every doctor shares reads it ([`health::judge`]), mapped as
/// `trawld --doctor` maps its listener's answer.
///
/// One rule is the web doctor's own: a health body without a `version` is
/// not trawld's. trawld always sends one, `trawl doctor` requires it too,
/// and this doctor dials trawld directly, so no proxy in between can have
/// taken it away. A body that lacks it gets no per-check rows.
fn health_rows(status: u16, body: &[u8]) -> (Row, Vec<Row>) {
    let check = WebCheck::UpstreamHealth;
    let foreign = || {
        (
            Row::failed(check, "the health answer is not trawld's health body")
                .next(Text::new(NOT_TRAWLD)),
            Vec::new(),
        )
    };
    match health::judge(status, body) {
        health::Answer::Health(health) if !health.has_version() => foreign(),
        health::Answer::Health(health) => {
            let shown = match health.status() {
                HealthStatus::Ok => "ok",
                HealthStatus::Degraded => "degraded",
                HealthStatus::Unavailable => "unavailable",
            };
            let detail = Text::new("status: ").lit(shown).lit("; HTTP ").int(status);
            let row = if health.outcome() == Outcome::Failed {
                Row::failed(check, "trawld reports itself unavailable")
                    .detail(detail)
                    .next(Text::new(
                        "read the per-check rows and trawld's log for why it is unavailable",
                    ))
            } else {
                Row::complete(check).detail(detail)
            };
            (row, keyed_rows(&health))
        }
        health::Answer::Recovering => (
            Row::not_sampled(check, reason::RECOVERING)
                .detail(Text::new(
                    "trawld answered corpus_recovering: its corpus is still recovering after a \
                     restart",
                ))
                .next(Text::new("wait for trawld to finish recovery, then rerun")),
            Vec::new(),
        ),
        health::Answer::Status(status) => (status_failure(status), Vec::new()),
        health::Answer::TooLarge => (too_large_row(), Vec::new()),
        health::Answer::Disagrees => (
            Row::failed(check, "the health status and the HTTP status disagree")
                .next(Text::new(NOT_TRAWLD)),
            Vec::new(),
        ),
        health::Answer::Foreign => foreign(),
    }
}

/// The failed health row for an HTTP `status` trawld's health endpoint
/// never sends, such as a 502 from something between: the status is shown,
/// and nothing is said about TLS.
fn status_failure(status: u16) -> Row {
    Row::failed(
        WebCheck::UpstreamHealth,
        "the health endpoint answered with a status trawld does not send",
    )
    .detail(Text::new("HTTP ").int(status))
    .next(Text::new(NOT_TRAWLD))
}

/// The health row for an answer larger than the doctor reads.
fn too_large_row() -> Row {
    Row::not_sampled(WebCheck::UpstreamHealth, reason::TOO_LARGE).detail(
        Text::new("the doctor reads at most ")
            .int(u32::try_from(BODY_MAX).unwrap_or(u32::MAX))
            .lit(" bytes of the health answer"),
    )
}

/// One row per reported check, sorted by name. Names trawld's health
/// endpoint does not report become one failed
/// `proxy.upstream.health._invalid` row, and none of them is shown: even an
/// identifier-shaped name may be a secret.
fn keyed_rows(health: &Health) -> Vec<Row> {
    let mut rows: Vec<Row> = health.checks().iter().map(key_row).collect();
    let invalid = health.unknown_names();
    if invalid > 0 {
        rows.push(
            Row::for_key(
                WebCheck::UpstreamHealth,
                HealthKey::invalid(),
                Outcome::Failed,
                Some("trawld reported a check name this doctor does not know"),
            )
            .detail(Text::new("").int(invalid).lit(" such name(s), not shown"))
            .next(Text::new(NOT_TRAWLD)),
        );
    }
    rows
}

/// The row for one reported check. No value the upstream sends is shown
/// as sent: a known value is named by a literal of this doctor's, and a
/// value it does not know is not shown at all.
fn key_row(answer: &KeyAnswer) -> Row {
    let key = HealthKey::new(answer.name())
        .expect("the judge names only checks trawld's health endpoint reports");
    let row = |reason| Row::for_key(WebCheck::UpstreamHealth, key, answer.outcome(), reason);
    match answer.value() {
        Reported::Ok => row(None).detail(Text::new("reported ok")),
        // A recovering corpus is not a failure: trawld cannot yet vouch for
        // what it holds, so the doctor could not look (ADR-0047).
        recovering @ (Reported::RollupPending | Reported::RestartBacklog) => {
            row(Some(reason::RECOVERING))
                .detail(Text::new(if recovering == Reported::RollupPending {
                    "reported rollup_pending"
                } else {
                    "reported restart_backlog"
                }))
                .next(Text::new("wait for trawld to finish recovery, then rerun"))
        }
        refused @ (Reported::Error | Reported::Refusing) => {
            row(Some(if refused == Reported::Refusing {
                "reported refusing"
            } else {
                "reported error"
            }))
            .next(
                Text::new("read trawld's log for why ")
                    .key(&key)
                    .lit(" fails"),
            )
        }
        Reported::Unknown => row(Some("reported a value this doctor does not know"))
            .detail(Text::new("the value is not shown"))
            .next(Text::new("read trawld's log for ").key(&key)),
    }
}

#[cfg(test)]
mod tests {
    use trawl_api::doctor::Check;

    use super::*;

    fn checks((row, keyed): (Row, Vec<Row>)) -> Vec<Check> {
        std::iter::once(row)
            .chain(keyed)
            .map(Row::into_check)
            .collect()
    }

    fn summary(checks: &[Check]) -> Vec<(&str, Outcome, Option<&str>)> {
        checks
            .iter()
            .map(|c| (c.id.as_str(), c.outcome, c.reason.as_deref()))
            .collect()
    }

    /// trawld's answer keeps a row per known check, sorted; unknown names
    /// fold into one `_invalid` row that shows none of them; a body without
    /// `version` is not trawld's and has no rows.
    #[test]
    fn health_answers_map_to_rows() {
        let body = br#"{"status":"degraded","version":"1.0.0","checks":{
            "duckdb":"ok","ingest_capacity":"refusing","corpus":"rollup_pending",
            "private_secret_name":"ok","data_path":"private_secret_value"}}"#;
        let rows = checks(health_rows(200, body));
        assert_eq!(
            summary(&rows),
            [
                ("proxy.upstream.health", Outcome::Complete, None),
                (
                    "proxy.upstream.health.corpus",
                    Outcome::NotSampled,
                    Some(reason::RECOVERING)
                ),
                (
                    "proxy.upstream.health.data_path",
                    Outcome::Failed,
                    Some("reported a value this doctor does not know")
                ),
                ("proxy.upstream.health.duckdb", Outcome::Complete, None),
                (
                    "proxy.upstream.health.ingest_capacity",
                    Outcome::Failed,
                    Some("reported refusing")
                ),
                (
                    "proxy.upstream.health._invalid",
                    Outcome::Failed,
                    Some("trawld reported a check name this doctor does not know")
                ),
            ]
        );
        let shown = format!("{rows:?}");
        assert!(!shown.contains("private_secret"), "{shown}");

        let unversioned = br#"{"status":"ok","checks":{"duckdb":"ok"}}"#;
        assert_eq!(
            summary(&checks(health_rows(200, unversioned))),
            [(
                "proxy.upstream.health",
                Outcome::Failed,
                Some("the health answer is not trawld's health body")
            )]
        );

        let unavailable = br#"{"status":"unavailable","version":"1","checks":{"duckdb":"error"}}"#;
        assert_eq!(
            summary(&checks(health_rows(503, unavailable))),
            [
                (
                    "proxy.upstream.health",
                    Outcome::Failed,
                    Some("trawld reports itself unavailable")
                ),
                (
                    "proxy.upstream.health.duckdb",
                    Outcome::Failed,
                    Some("reported error")
                ),
            ]
        );
        let disagrees = br#"{"status":"ok","version":"1","checks":{}}"#;
        assert_eq!(
            summary(&checks(health_rows(503, disagrees)))[0].2,
            Some("the health status and the HTTP status disagree")
        );
    }

    /// A pin that `~` expands to an empty path fails as an empty pin in the
    /// slot does, with the same reason and fix, as startup refuses both
    /// alike; it is not a CA that trawld has yet to write.
    #[test]
    fn an_empty_expanded_pin_fails_as_an_empty_pin() {
        let config = SelectedPath::new(
            Selection::ConfigFlag,
            std::path::Path::new("/etc/trawl/trawld.toml"),
        );
        let shown = SelectedPath::new(Selection::UpstreamCaPath, std::path::Path::new("~"));
        let expanded = pin_fault(PinFault::Empty, &shown, &config).into_check();
        let in_slot = upstream_fault(
            &ConfigError::UpstreamCa {
                path: std::path::PathBuf::new(),
                reason: String::new(),
            },
            &config,
        )
        .into_check();
        assert_eq!(expanded.outcome, Outcome::Failed);
        assert_eq!(
            (expanded.outcome, expanded.reason.as_deref()),
            (in_slot.outcome, in_slot.reason.as_deref())
        );
        assert_eq!(
            expanded.reason.as_deref(),
            Some("upstream_ca_path is empty")
        );
        assert_eq!(expanded.next_action, in_slot.next_action);
        let missing = pin_fault(PinFault::Read(ReadFault::Missing), &shown, &config).into_check();
        assert_eq!(missing.reason.as_deref(), Some(reason::CA_NOT_PRESENT));
    }

    /// Every slot error names its rule in a fixed sentence and quotes
    /// nothing, not even the text an `Unparseable` carries.
    #[test]
    fn upstream_faults_quote_nothing() {
        let config = SelectedPath::new(
            Selection::ConfigFlag,
            std::path::Path::new("/etc/trawl/trawld.toml"),
        );
        let planted = "private-secret";
        let errors = [
            ConfigError::UpstreamUrl(UpstreamUrlError::Unparseable(planted.to_owned())),
            ConfigError::UpstreamUrl(UpstreamUrlError::Userinfo),
            ConfigError::UpstreamUrl(UpstreamUrlError::QueryOrFragment),
            ConfigError::UpstreamUrl(UpstreamUrlError::NotHttps {
                scheme: planted.to_owned(),
            }),
            ConfigError::UpstreamUrl(UpstreamUrlError::NoHost),
            ConfigError::UpstreamConnectAddr(ConnectAddrError::Malformed),
            ConfigError::UpstreamConnectAddr(ConnectAddrError::PortZero),
            ConfigError::UpstreamConnectAddr(ConnectAddrError::IpHost),
            ConfigError::UpstreamConnectAddr(ConnectAddrError::PortMismatch {
                url_port: 443,
                addr_port: 5514,
            }),
            ConfigError::UpstreamCa {
                path: planted.into(),
                reason: planted.to_owned(),
            },
            ConfigError::EnvUtf8 {
                name: planted.to_owned(),
            },
        ];
        for error in &errors {
            let check = upstream_fault(error, &config).into_check();
            assert_eq!(check.outcome, Outcome::Failed, "{error:?}");
            let shown = format!("{check:?}");
            assert!(!shown.contains(planted), "{shown}");
        }
    }
}
