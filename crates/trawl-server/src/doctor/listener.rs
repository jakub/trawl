// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `trawld --doctor` checks of the certificate and trawld's own listener
//! (#269, D12 to D14).
//!
//! - `server.tls.material` reads the pair the configuration selects: an
//!   operator's pair through [`super::fsread`], trawld's generated pair
//!   through [`tls::inspect_generated_pair`], which makes a start's checks
//!   and changes nothing. It judges the pair with the functions boot serves
//!   it with ([`tls::parse_pem_pair`], [`tls::serving_config`]) and with one
//!   rule of the doctor's own: every certificate is within its validity
//!   dates. Nothing is generated.
//! - `server.listener.identity` dials the configured address, a wildcard
//!   mapped to loopback, and accepts only the leaf on disk, from a server
//!   that proves in the handshake that it holds the leaf's key
//!   ([`PinnedLeaf`]). It makes no claim about a host name. A host name is
//!   dialed at each address it resolves to, in order, until one proves it;
//!   when none does, which of them trawld binds is unknown. The material is
//!   read again after the probe, and a change means the comparison proves
//!   nothing. With no material yet, it only opens and closes a TCP
//!   connection.
//! - `server.listener.health` reads the health answer the identity probe
//!   received, at most [`HEALTH_BODY_MAX`] bytes, and maps each reported
//!   value through the classifier every doctor shares
//!   ([`trawl_api::doctor::health`]). An answer that breaks off is not
//!   classified, and no value is shown as the server sent it.
//!
//! The probe is one `GET /api/v1/health` per address over HTTP/1.1, with
//! no proxy, no redirect, no retry, no key, and one bounded time for all
//! addresses. The group fills [`Ctx::tls`] and [`Ctx::listener_refused`].

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, DigitallySignedStruct, SignatureScheme};
use trawl_api::doctor::health::{self, ValueClass};
use trawl_api::doctor::{Outcome, reason};
use trawl_api::{ErrorCode, ErrorResponse, HealthResponse, HealthStatus};

use super::fsread::{self, Links, ReadFault};
use super::output::{HealthKey, Row, SelectedPath, Selection, Text};
use super::{Ctx, Der, Runner, ServerCheck};
use crate::tls::{self, GeneratedPair, OwnerRule, Regenerate, TlsError, TlsSource, UnsafeReason};

/// This group's checks, in [`ServerCheck::ALL`] order; the graph test in
/// [`super`] reads this list.
#[cfg(test)]
pub(super) const CHECKS: [ServerCheck; 3] = [
    ServerCheck::TlsMaterial,
    ServerCheck::ListenerIdentity,
    ServerCheck::ListenerHealth,
];

/// How long a TCP connection to the listener may take to open.
const CONNECT_DEADLINE: Duration = Duration::from_secs(5);

/// How long the whole health probe may take: connection, handshake,
/// request, and body, at every address the listener address resolves to.
const PROBE_DEADLINE: Duration = Duration::from_secs(10);

/// How long a listener address that names a host may take to resolve.
const RESOLVE_DEADLINE: Duration = Duration::from_secs(5);

/// The most bytes of the health answer the doctor reads. trawld's answer is
/// a few hundred.
const HEALTH_BODY_MAX: usize = 64 * 1024;

/// The endpoint the probe reads.
const HEALTH_PATH: &str = "/api/v1/health";

/// Run this group's checks through `runner`.
pub(super) async fn run(ctx: &mut Ctx, runner: &mut Runner) {
    let source = MaterialSource::of(ctx);
    let before = check_material(ctx, runner, source.as_ref()).await;
    let answer = check_identity(ctx, runner, source.as_ref(), before).await;
    check_health(runner, answer).await;
}

// -- server.tls.material ---------------------------------------------------

/// Where the listener's certificate and key are read from.
#[derive(Debug)]
enum MaterialSource {
    /// `[server] tls_cert_path` and `tls_key_path`.
    Configured {
        cert: PathBuf,
        key: PathBuf,
        shown_cert: SelectedPath,
        shown_key: SelectedPath,
    },
    /// trawld's generated pair under `state_dir`, judged for `owner`.
    Generated {
        state_dir: PathBuf,
        owner: OwnerRule,
    },
}

impl MaterialSource {
    /// The source the configuration selects, as boot selects it. `None`
    /// when only one of the two paths is set, which the configuration's own
    /// validation refuses first.
    fn of(ctx: &Ctx) -> Option<Self> {
        let server = &ctx.config.server;
        match TlsSource::of(
            server.tls_cert_path.as_deref(),
            server.tls_key_path.as_deref(),
        ) {
            Err(_) => None,
            Ok(TlsSource::Configured { cert, key }) => Some(Self::Configured {
                cert: cert.to_owned(),
                key: key.to_owned(),
                shown_cert: SelectedPath::new(Selection::TlsCertPath, cert),
                shown_key: SelectedPath::new(Selection::TlsKeyPath, key),
            }),
            Ok(TlsSource::Generated) => Some(Self::Generated {
                state_dir: ctx.config.state_dir(),
                // A root run cannot know which uid trawld runs as, so it
                // records another owner rather than refusing it.
                owner: match ctx.run_as.uid {
                    Some(uid) if !ctx.run_as.root => OwnerRule::Enforced(uid),
                    _ => OwnerRule::Observed(0),
                },
            }),
        }
    }

    /// What the rows name as the input they read.
    fn text(&self) -> Text {
        match self {
            Self::Configured {
                shown_cert,
                shown_key,
                ..
            } => Text::new("[server] tls_cert_path ")
                .path(shown_cert)
                .lit(" and tls_key_path ")
                .path(shown_key),
            Self::Generated { .. } => Text::new(
                "the pair trawld generates in tls/ and tls-key/ under its state directory, \
                 the parent of [data] path",
            ),
        }
    }

    const fn generated(&self) -> bool {
        matches!(self, Self::Generated { .. })
    }
}

/// Which file a read fault is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    Cert,
    Key,
    /// One of the generated TLS directories.
    Directory,
}

/// Why the material was not read as a pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    /// A read gave no bytes.
    Read(Part, ReadFault),
    /// Boot refuses what is in its generated TLS directories.
    Refused(UnsafeReason),
}

/// What one read of the material found.
#[derive(Clone, PartialEq, Eq)]
enum Material {
    /// Both PEM files, as read.
    Pair { cert: Vec<u8>, key: Vec<u8> },
    /// No generated pair a start would load: it generates a new one.
    WillGenerate(Regenerate),
    /// Nothing that a start would serve was read.
    Unread(Fault),
}

/// One read of the material, compared as a whole before and after the
/// listener probe.
#[derive(Clone, PartialEq, Eq)]
struct Observed {
    material: Material,
    /// A generated directory or the generated key belongs to another uid,
    /// which a root run does not judge.
    foreign_owner: bool,
}

/// `Debug` never shows the key or the certificate.
impl std::fmt::Debug for Observed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let material = match &self.material {
            Material::Pair { cert, key } => {
                format!(
                    "Pair {{ cert: {} bytes, key: {} bytes }}",
                    cert.len(),
                    key.len()
                )
            }
            Material::WillGenerate(why) => format!("WillGenerate({why:?})"),
            Material::Unread(fault) => format!("Unread({fault:?})"),
        };
        f.debug_struct("Observed")
            .field("material", &format_args!("{material}"))
            .field("foreign_owner", &self.foreign_owner)
            .finish()
    }
}

/// Read the material `source` names, bounded in size and time.
async fn read_material(source: &MaterialSource) -> Observed {
    match source {
        MaterialSource::Configured { cert, key, .. } => {
            let cert = fsread::read(cert.clone(), fsread::cap::CERT, Links::Follow).await;
            let key = fsread::read(key.clone(), fsread::cap::KEY, Links::Follow).await;
            let material = match (cert, key) {
                (Err(fault), _) => Material::Unread(Fault::Read(Part::Cert, fault)),
                (_, Err(fault)) => Material::Unread(Fault::Read(Part::Key, fault)),
                (Ok(cert), Ok(key)) => Material::Pair { cert, key },
            };
            Observed {
                material,
                foreign_owner: false,
            }
        }
        MaterialSource::Generated { state_dir, owner } => {
            let (state_dir, owner) = (state_dir.clone(), *owner);
            let inspect = tokio::task::spawn_blocking(move || {
                tls::inspect_generated_pair(&state_dir, owner, fsread::cap::CERT, fsread::cap::KEY)
            });
            let unread = |fault| Observed {
                material: Material::Unread(fault),
                foreign_owner: false,
            };
            match tokio::time::timeout(fsread::READ_DEADLINE, inspect).await {
                Err(_) => unread(Fault::Read(Part::Directory, ReadFault::TimedOut)),
                Ok(Err(_)) => unread(Fault::Read(Part::Directory, ReadFault::Io)),
                Ok(Ok(Err(error))) => unread(generated_fault(&error)),
                Ok(Ok(Ok(inspection))) => Observed {
                    material: match inspection.pair {
                        GeneratedPair::Found { cert_pem, key_pem } => Material::Pair {
                            cert: cert_pem,
                            key: key_pem,
                        },
                        GeneratedPair::Regenerate(why) => Material::WillGenerate(why),
                    },
                    foreign_owner: inspection.foreign_owner,
                },
            }
        }
    }
}

/// The fault an inspection of the generated pair reports. Only the kind of
/// failure is kept: no path and no OS text.
fn generated_fault(error: &TlsError) -> Fault {
    let io_fault = |error: &io::Error| match error.kind() {
        io::ErrorKind::PermissionDenied => ReadFault::PermissionDenied,
        io::ErrorKind::FileTooLarge => ReadFault::TooLarge,
        io::ErrorKind::NotFound => ReadFault::Missing,
        _ => ReadFault::Io,
    };
    match error {
        TlsError::Unsafe { reason, .. } => Fault::Refused(*reason),
        TlsError::ReadKey { source, .. } => Fault::Read(Part::Key, io_fault(source)),
        TlsError::ReadCert { source, .. } => Fault::Read(Part::Cert, io_fault(source)),
        TlsError::Write(source) => Fault::Read(Part::Directory, io_fault(source)),
        _ => Fault::Read(Part::Directory, ReadFault::Io),
    }
}

/// A pair that parses, matches, and is in date.
#[derive(Debug)]
struct Judged {
    /// The leaf certificate's DER.
    leaf: Vec<u8>,
    /// Whole days until the leaf expires.
    days_left: i64,
}

/// Why a pair that was read is not one boot serves, or not one in date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Misjudged {
    /// Boot refuses it: it does not parse, or the key is not the leaf's.
    Content(&'static str),
    /// Boot serves it, but a certificate is outside its validity dates.
    Dates(&'static str),
}

impl Misjudged {
    const fn reason(self) -> &'static str {
        match self {
            Self::Content(why) | Self::Dates(why) => why,
        }
    }
}

/// Judge a PEM pair the way boot serves it, then by its validity dates.
fn judge_pair(cert_pem: &[u8], key_pem: &[u8]) -> Result<Judged, Misjudged> {
    use Misjudged::{Content, Dates};

    let (chain, key) = tls::parse_pem_pair(cert_pem, key_pem).map_err(|error| match error {
        TlsError::InvalidKey(_) => Content("the key file holds no PEM private key"),
        _ => Content("the certificate file is not valid PEM"),
    })?;
    let leaf = chain
        .first()
        .map(|leaf| leaf.as_ref().to_vec())
        .ok_or(Content("the certificate file holds no PEM certificate"))?;
    tls::serving_config(chain.clone(), key).map_err(|_| {
        Content("the private key does not belong to the certificate, or trawld cannot use it")
    })?;

    let now = x509_parser::time::ASN1Time::now();
    let mut days_left = 0;
    for (at, certificate) in chain.iter().enumerate() {
        let leaf_or =
            |leaf: &'static str, chained: &'static str| if at == 0 { leaf } else { chained };
        let Ok((_, parsed)) = x509_parser::parse_x509_certificate(certificate.as_ref()) else {
            return Err(Content(leaf_or(
                "the certificate does not parse as X.509",
                "a certificate in its chain does not parse as X.509",
            )));
        };
        let validity = parsed.validity();
        if now < validity.not_before {
            return Err(Dates(leaf_or(
                "the certificate is not valid yet",
                "a certificate in its chain is not valid yet",
            )));
        }
        if now > validity.not_after {
            return Err(Dates(leaf_or(
                "the certificate has expired",
                "a certificate in its chain has expired",
            )));
        }
        if at == 0 {
            days_left = (validity.not_after.timestamp() - now.timestamp()) / 86_400;
        }
    }
    Ok(Judged { leaf, days_left })
}

/// `server.tls.material`: read and judge the pair, and fill [`Ctx::tls`]
/// when it is one boot serves. Returns what was read, for the listener
/// check to compare after its probe.
async fn check_material(
    ctx: &mut Ctx,
    runner: &mut Runner,
    source: Option<&MaterialSource>,
) -> Option<Observed> {
    let check = ServerCheck::TlsMaterial;
    let gate = runner.gate(check)?;
    let Some(source) = source else {
        runner.record(
            gate,
            Row::failed(check, "only one of tls_cert_path and tls_key_path is set").next(
                Text::new("set both, or neither to let trawld generate a pair"),
            ),
        );
        return None;
    };
    let observed = read_material(source).await;
    let (mut row, leaf) = material_row(source, &observed);
    // A content failure is confirmed by a second read: a pair caught while
    // it is written fails to parse, and that is a change, not a fault.
    if row.outcome() == Outcome::Failed
        && matches!(observed.material, Material::Pair { .. })
        && read_material(source).await != observed
    {
        row = changed_row(check, source);
    } else if let Some(leaf) = leaf {
        ctx.tls.leaf = Some(Der(leaf));
    }
    runner.record(gate, row);
    Some(observed)
}

/// The row for what one read found, and the leaf when the pair is one boot
/// serves.
fn material_row(source: &MaterialSource, observed: &Observed) -> (Row, Option<Vec<u8>>) {
    let check = ServerCheck::TlsMaterial;
    let root_note = "; a generated directory or the key belongs to another uid, which a root run \
                     does not judge";
    let as_root = |detail: Text| {
        Row::not_sampled(check, reason::RAN_AS_ROOT)
            .detail(detail.lit(root_note))
            .next(Text::new(
                "rerun as the service user to check that trawld owns them",
            ))
    };
    let (row, leaf) = match &observed.material {
        Material::Unread(fault) => (fault_row(source, *fault), None),
        Material::WillGenerate(why) => {
            let detail = Text::new(match why {
                Regenerate::Absent => {
                    "no generated pair yet; trawld generates one on its next start"
                }
                Regenerate::NoCertificate => {
                    "the generated certificate is missing; trawld generates a new pair on its \
                     next start"
                }
                Regenerate::NoKey => {
                    "the generated key is missing; trawld generates a new pair on its next start"
                }
                Regenerate::KeyExposed(_) => {
                    "the generated key may be readable outside trawld; trawld discards it and \
                     generates a new pair on its next start"
                }
            });
            let row = if observed.foreign_owner {
                as_root(detail)
            } else {
                Row::complete_because(check, reason::WILL_INITIALIZE).detail(detail)
            };
            (row, None)
        }
        Material::Pair { cert, key } => match judge_pair(cert, key) {
            Err(why) => {
                let next = match (source.generated(), why) {
                    (true, _) => {
                        "remove tls/cert.pem under trawld's state directory; trawld then \
                         generates a new pair on its next start"
                    }
                    (false, Misjudged::Dates(_)) => {
                        "install a certificate that is valid now at tls_cert_path, with its key"
                    }
                    (false, Misjudged::Content(_)) => {
                        "install the certificate and its own private key at the configured paths"
                    }
                };
                (Row::failed(check, why.reason()).next(Text::new(next)), None)
            }
            Ok(judged) => {
                let detail = Text::new(
                    "the certificate and key parse and match; the certificate is valid for \
                     another ",
                )
                .int(judged.days_left)
                .lit(" days");
                let row = if observed.foreign_owner {
                    as_root(detail)
                } else {
                    Row::complete(check).detail(detail)
                };
                (row, Some(judged.leaf))
            }
        },
    };
    (row.source(source.text()), leaf)
}

/// The row for material that changed while the doctor read or compared it.
fn changed_row(check: ServerCheck, source: &MaterialSource) -> Row {
    Row::not_sampled(check, reason::MATERIAL_CHANGED)
        .detail(Text::new(
            "the certificate or key changed while the doctor read them",
        ))
        .source(source.text())
        .next(Text::new("rerun once they stop changing"))
}

/// The `server.tls.material` row for material that was not read as a pair.
fn fault_row(source: &MaterialSource, fault: Fault) -> Row {
    let check = ServerCheck::TlsMaterial;
    let row = match fault {
        Fault::Refused(UnsafeReason::OldKeyDirectory) => {
            Row::failed(check, refusal_reason(UnsafeReason::OldKeyDirectory)).next(Text::new(
                "remove the directory tls/key.pem under trawld's state directory",
            ))
        }
        Fault::Refused(why) => Row::failed(check, refusal_reason(why)).next(Text::new(
            "make tls/ and tls-key/ under trawld's state directory plain directories owned by \
             trawld's user, holding plain files, or remove them so trawld creates them",
        )),
        Fault::Read(part, read) => match (source.generated(), part, read) {
            (
                false,
                Part::Cert | Part::Key,
                ReadFault::Missing | ReadFault::NotRegular | ReadFault::SymlinkLoop,
            ) => Row::failed(check, configured_refusal(part, read)).next(Text::new(
                "set [server] tls_cert_path and tls_key_path to the pair trawld serves, or \
                     remove both to let trawld generate one",
            )),
            (_, _, ReadFault::PermissionDenied) => {
                Row::not_sampled(check, reason::PERMISSION_DENIED)
                    .detail(Text::new(match part {
                        Part::Cert => "this user may not read the certificate",
                        Part::Key => "this user may not read the private key",
                        Part::Directory => {
                            "this user may not open trawld's generated TLS directories"
                        }
                    }))
                    .next(Text::new(
                        "rerun as the service user, which trawld reads them as",
                    ))
            }
            (_, _, ReadFault::TooLarge) => Row::not_sampled(check, reason::TOO_LARGE).detail(
                Text::new("the doctor reads at most ")
                    .int(fsread::cap::CERT)
                    .lit(" bytes of each file"),
            ),
            (_, _, ReadFault::Changed) => return changed_row(check, source),
            (_, _, ReadFault::TimedOut) => Row::not_sampled(check, reason::TIMED_OUT)
                .detail(Text::new("reading the certificate and key did not finish")),
            _ => Row::not_sampled(check, reason::UNREADABLE),
        },
    };
    row.source(source.text())
}

/// Why boot refuses what is in its generated TLS directories.
const fn refusal_reason(why: UnsafeReason) -> &'static str {
    match why {
        UnsafeReason::Symlink => {
            "a symbolic link is in trawld's generated TLS directories, which boot refuses"
        }
        UnsafeReason::NotDirectory => {
            "a generated TLS directory is not a directory, which boot refuses"
        }
        UnsafeReason::NotRegular => {
            "the generated certificate or key is not a regular file, which boot refuses"
        }
        UnsafeReason::ForeignOwner { .. } => {
            "a generated TLS directory belongs to another uid than this one, which boot refuses"
        }
        UnsafeReason::OldKeyDirectory => {
            "a directory is at tls/key.pem, where an older trawld kept its key, and boot cannot remove it"
        }
    }
}

/// Why a configured certificate or key path is refused, for the read
/// faults that prove the path wrong: missing, not a regular file, or a
/// symlink loop.
const fn configured_refusal(part: Part, read: ReadFault) -> &'static str {
    match (part, read) {
        (Part::Key, ReadFault::Missing) => "the configured private key does not exist",
        (Part::Key, ReadFault::NotRegular) => "the configured private key is not a regular file",
        (Part::Key, _) => "the configured private key path is a symlink loop",
        (_, ReadFault::Missing) => "the configured certificate does not exist",
        (_, ReadFault::NotRegular) => "the configured certificate is not a regular file",
        _ => "the configured certificate path is a symlink loop",
    }
}

// -- server.listener.identity ----------------------------------------------

/// What the listener did with the doctor's connection, when it proved
/// nothing about its certificate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Contact {
    /// A TCP connection opened.
    Accepted,
    /// No connection opened; the reason code says why.
    Refused(&'static str),
    /// Nothing answered within the deadline.
    TimedOut,
}

/// Whether a connection error says nothing is there to accept it.
fn nothing_listens(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::NetworkUnreachable
            | io::ErrorKind::HostUnreachable
            | io::ErrorKind::AddrNotAvailable
            | io::ErrorKind::NetworkDown
    )
}

/// The reason code for a connection that did not open, when one fits.
fn refusal(kind: io::ErrorKind) -> Option<&'static str> {
    if nothing_listens(kind) {
        Some(reason::NOT_LISTENING)
    } else if kind == io::ErrorKind::PermissionDenied {
        Some(reason::PERMISSION_DENIED)
    } else {
        None
    }
}

/// Open a TCP connection to `addr` and close it at once, sending nothing,
/// within `budget` and at most [`CONNECT_DEADLINE`].
async fn tcp_contact(addr: SocketAddr, budget: Duration) -> Contact {
    let deadline = budget.min(CONNECT_DEADLINE);
    match tokio::time::timeout(deadline, tokio::net::TcpStream::connect(addr)).await {
        Err(_) => Contact::TimedOut,
        Ok(Ok(stream)) => {
            drop(stream);
            Contact::Accepted
        }
        Ok(Err(error)) if error.kind() == io::ErrorKind::TimedOut => Contact::TimedOut,
        Ok(Err(error)) => Contact::Refused(refusal(error.kind()).unwrap_or(reason::NOT_LISTENING)),
    }
}

/// The address the probe dials: the configured one, with a wildcard mapped
/// to loopback of its family. The flag says whether it was a wildcard.
fn dial_addr(addr: SocketAddr) -> (SocketAddr, bool) {
    if !addr.ip().is_unspecified() {
        return (addr, false);
    }
    let ip = match addr {
        SocketAddr::V4(_) => Ipv4Addr::LOCALHOST.into(),
        SocketAddr::V6(_) => Ipv6Addr::LOCALHOST.into(),
    };
    (SocketAddr::new(ip, addr.port()), true)
}

/// One address the probe dials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Dial {
    /// The address, a wildcard mapped to loopback by [`dial_addr`].
    addr: SocketAddr,
    /// Whether it was a wildcard.
    wildcard: bool,
}

/// The addresses the probe dials, in resolution order: the configured
/// socket address, or else every address `host:port` resolves to. Boot
/// binds the first of those it can bind, and which one that was is not
/// something the doctor can see, so it dials them all. Each is mapped by
/// [`dial_addr`], and a repeat is dialed once. The error is the row.
async fn listener_addrs(configured: &str) -> Result<Vec<Dial>, Row> {
    let check = ServerCheck::ListenerIdentity;
    let unresolved = || {
        Row::failed(
            check,
            "the listener address does not resolve to an address of this host",
        )
        .next(Text::new(
            "set [server] http_addr to host:port for an address of this host",
        ))
    };
    let resolved: Vec<SocketAddr> = if let Ok(addr) = configured.parse::<SocketAddr>() {
        vec![addr]
    } else {
        match tokio::time::timeout(
            RESOLVE_DEADLINE,
            tokio::net::lookup_host(configured.to_owned()),
        )
        .await
        {
            Err(_) => {
                return Err(Row::not_sampled(check, reason::TIMED_OUT)
                    .detail(Text::new("the listener address did not resolve in time")));
            }
            Ok(Err(_)) => return Err(unresolved()),
            Ok(Ok(addrs)) => addrs.collect(),
        }
    };
    let mut dials: Vec<Dial> = Vec::new();
    for addr in resolved {
        let (addr, wildcard) = dial_addr(addr);
        if !dials.iter().any(|dial| dial.addr == addr) {
            dials.push(Dial { addr, wildcard });
        }
    }
    if dials.is_empty() {
        Err(unresolved())
    } else {
        Ok(dials)
    }
}

/// What the probe's verifier saw.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Seen {
    /// Whether the server's leaf was the pinned one, once it sent one.
    leaf: Option<bool>,
    /// Whether every handshake signature verified, once one was checked.
    signature: Option<bool>,
}

impl Seen {
    /// The server sent the pinned leaf and proved it holds its key.
    const fn proven(self) -> bool {
        matches!(
            self,
            Self {
                leaf: Some(true),
                signature: Some(true)
            }
        )
    }
}

/// A `rustls` verifier that accepts exactly one certificate, the leaf on
/// disk, and only from a server that proves it holds the leaf's key.
///
/// The leaf is compared byte for byte with the pinned DER. Nothing else in
/// the chain, no host name, and no validity date is consulted: the doctor
/// asks whether this listener serves this certificate, not whether a client
/// would trust it. Every handshake signature is verified with the ring
/// provider's algorithms against the pinned leaf's key, through `rustls`'s
/// own [`rustls::crypto::verify_tls12_signature`] and
/// [`rustls::crypto::verify_tls13_signature`]; a server that presents the
/// leaf without its key cannot complete the handshake. What it saw is
/// recorded in [`Seen`] for the report.
struct PinnedLeaf {
    leaf: CertificateDer<'static>,
    algorithms: WebPkiSupportedAlgorithms,
    seen: Arc<Mutex<Seen>>,
}

/// The pinned certificate is never shown.
impl std::fmt::Debug for PinnedLeaf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PinnedLeaf(..)")
    }
}

impl PinnedLeaf {
    fn record(&self, update: impl FnOnce(&mut Seen)) {
        update(&mut self.seen.lock().unwrap_or_else(PoisonError::into_inner));
    }

    /// Verify one handshake signature with `verify`, only when `cert` is
    /// the pinned leaf, and record the result. A failure sticks.
    fn signature(
        &self,
        cert: &CertificateDer<'_>,
        verify: impl FnOnce() -> Result<HandshakeSignatureValid, rustls::Error>,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        // rustls passes the leaf `verify_server_cert` accepted; checking it
        // again costs one comparison and holds whatever calls this.
        let result = if cert.as_ref() == self.leaf.as_ref() {
            verify()
        } else {
            Err(not_pinned())
        };
        let verified = result.is_ok();
        self.record(|seen| seen.signature = Some(verified && seen.signature != Some(false)));
        result
    }
}

fn not_pinned() -> rustls::Error {
    rustls::Error::InvalidCertificate(CertificateError::ApplicationVerificationFailure)
}

impl ServerCertVerifier for PinnedLeaf {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let pinned = end_entity.as_ref() == self.leaf.as_ref();
        self.record(|seen| seen.leaf = Some(pinned));
        if pinned {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(not_pinned())
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.signature(cert, || {
            rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
        })
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.signature(cert, || {
            rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
        })
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// The probe's HTTP client: TLS through [`PinnedLeaf`] on the ring
/// provider, SNI off, ALPN `http/1.1` only, no session resumption (every
/// handshake verifies the certificate and a signature), no proxy, no
/// redirect, no retry, HTTP/1.1 only, and `budget` for the whole exchange,
/// at most [`CONNECT_DEADLINE`] of it to connect.
fn probe_client(
    leaf: &Der,
    seen: Arc<Mutex<Seen>>,
    budget: Duration,
) -> Result<reqwest::Client, ()> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(PinnedLeaf {
        leaf: CertificateDer::from(leaf.0.clone()),
        algorithms: provider.signature_verification_algorithms,
        seen,
    });
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(drop)?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    tls.enable_sni = false;
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    tls.resumption = rustls::client::Resumption::disabled();
    reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .https_only(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .http1_only()
        .pool_max_idle_per_host(0)
        .connect_timeout(budget.min(CONNECT_DEADLINE))
        .timeout(budget)
        .build()
        .map_err(drop)
}

/// What the probe got.
#[derive(Debug)]
struct Probe {
    seen: Seen,
    /// `None` when the client could not be built.
    answer: Option<Answer>,
}

/// The health request's result.
type Answer = Result<reqwest::Response, reqwest::Error>;

/// Send the one `GET /api/v1/health` to `addr`, accepting only `leaf`,
/// within `budget`, which the health body's read shares.
async fn probe(addr: SocketAddr, leaf: &Der, budget: Duration) -> Probe {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let answer = match probe_client(leaf, Arc::clone(&seen), budget) {
        Err(()) => None,
        Ok(client) => Some(
            client
                .get(format!("https://{addr}{HEALTH_PATH}"))
                .send()
                .await,
        ),
    };
    let seen = *seen.lock().unwrap_or_else(PoisonError::into_inner);
    Probe { seen, answer }
}

/// The kind of the first I/O error in `error`'s chain, or of the I/O
/// error it wraps when it is only an `Other` wrapper, as the TLS connector
/// makes around a handshake's error. `io::Error::source` skips the error a
/// wrapper holds, so the wrapped one is reached through `get_ref`.
fn io_kind(error: &reqwest::Error) -> Option<io::ErrorKind> {
    let mut source = std::error::Error::source(error);
    while let Some(inner) = source {
        if let Some(mut io) = inner.downcast_ref::<io::Error>() {
            while io.kind() == io::ErrorKind::Other
                && let Some(wrapped) = io
                    .get_ref()
                    .and_then(|wrapped| wrapped.downcast_ref::<io::Error>())
            {
                io = wrapped;
            }
            return Some(io.kind());
        }
        source = inner.source();
    }
    None
}

/// What dialing one address found.
#[derive(Debug)]
struct Attempt {
    /// The row this address alone gives.
    row: Row,
    /// The health answer, when this address proved its identity.
    answer: Option<Answer>,
    /// Nothing accepted a connection at this address.
    not_listening: bool,
    /// This address settles the check whatever the others give: it proved
    /// the certificate, it accepted a connection when there is no material
    /// to compare, or the probe could not be built at all.
    decisive: bool,
}

/// Dial `dial`, one of `count` addresses, within `budget`: a TCP contact
/// when there is no `leaf` to compare, else the pinned TLS probe.
async fn attempt(dial: Dial, count: usize, leaf: Option<&Der>, budget: Duration) -> Attempt {
    let check = ServerCheck::ListenerIdentity;
    let dialed = |text: Text| dialed_text(text, dial, count);
    let Some(leaf) = leaf else {
        let contact = tcp_contact(dial.addr, budget).await;
        let row = match contact {
            Contact::Accepted => Row::not_sampled(check, reason::NO_MATERIAL)
                .detail(dialed(Text::new(
                    "something accepts connections at the configured address, and there is no \
                     certificate on disk yet to compare it with",
                )))
                .next(Text::new(
                    "rerun once trawld has generated its certificate; if trawld is not running, \
                     find what holds the port",
                )),
            Contact::Refused(code) => contact_row(code, &dialed),
            Contact::TimedOut => timed_out_row(&dialed),
        };
        return Attempt {
            row,
            answer: None,
            not_listening: contact == Contact::Refused(reason::NOT_LISTENING),
            decisive: contact == Contact::Accepted,
        };
    };
    let Probe { seen, answer } = probe(dial.addr, leaf, budget).await;
    let Some(answer) = answer else {
        return Attempt {
            row: Row::failed(check, "the doctor could not build its TLS client"),
            answer: None,
            not_listening: false,
            decisive: true,
        };
    };
    let row = probe_row(seen, &answer, &dialed);
    let not_listening = matches!(
        &answer,
        Err(error) if !seen.proven() && io_kind(error).is_some_and(nothing_listens)
    );
    let proven = row.outcome() == Outcome::Complete;
    Attempt {
        row,
        answer: proven.then_some(answer),
        not_listening,
        decisive: proven,
    }
}

/// `text`, with what the report may say of how `dial`, one of `count`
/// addresses, was reached. No address is shown.
fn dialed_text(text: Text, dial: Dial, count: usize) -> Text {
    let text = if dial.wildcard {
        text.lit("; dialed on loopback for the wildcard address")
    } else {
        text
    };
    if count > 1 {
        text.lit("; at one of the ")
            .int(u32::try_from(count).unwrap_or(u32::MAX))
            .lit(" addresses the listener address resolves to")
    } else {
        text
    }
}

/// The identity row from the attempts at `count` addresses, the proven
/// address's health answer, and whether nothing listens at any address.
///
/// A decisive attempt settles it. One address gives what it found, so a
/// stable mismatch there fails. Of several addresses with no proof, none
/// accepting a connection is `not_listening`; anything else leaves unknown
/// which of them trawld binds, and a mismatch at one says nothing about
/// the others: `not_sampled`, [`reason::AMBIGUOUS_ADDRESS`].
fn settle(mut attempts: Vec<Attempt>, count: usize) -> (Row, Option<Answer>, bool) {
    let check = ServerCheck::ListenerIdentity;
    if let Some(at) = attempts.iter().position(|attempt| attempt.decisive) {
        let settled = attempts.swap_remove(at);
        return (settled.row, settled.answer, settled.not_listening);
    }
    if count == 1
        && let Some(only) = attempts.pop()
    {
        return (only.row, only.answer, only.not_listening);
    }
    let shown = u32::try_from(count).unwrap_or(u32::MAX);
    if attempts.iter().all(|attempt| attempt.not_listening) {
        let row = Row::not_sampled(check, reason::NOT_LISTENING)
            .detail(
                Text::new("nothing accepts connections at any of the ")
                    .int(shown)
                    .lit(" addresses the listener address resolves to"),
            )
            .next(Text::new("start trawld, then rerun"));
        return (row, None, true);
    }
    let row = Row::not_sampled(check, reason::AMBIGUOUS_ADDRESS)
        .detail(
            Text::new("the listener address resolves to ")
                .int(shown)
                .lit(
                    " addresses, and none of them proved it serves the certificate on disk; \
                     which one trawld binds is not known",
                ),
        )
        .next(Text::new(
            "set [server] http_addr to the one IP address and port trawld listens on",
        ));
    (row, None, false)
}

/// `server.listener.identity`: dial the listener and compare what it
/// serves with the material, then read the material again. A listener
/// address that resolves to several addresses is dialed at each in turn,
/// within one [`PROBE_DEADLINE`], until one proves it. Returns the probe's
/// answer when the listener proved its identity, for the health check.
async fn check_identity(
    ctx: &mut Ctx,
    runner: &mut Runner,
    source: Option<&MaterialSource>,
    before: Option<Observed>,
) -> Option<Answer> {
    let check = ServerCheck::ListenerIdentity;
    let gate = runner.gate(check)?;
    let source = source.expect("server.listener.identity runs only after the material was read");
    let before = before.expect("server.listener.identity runs only after the material was read");
    let from = if std::env::var("TRAWL_HTTP_ADDR").is_ok_and(|addr| !addr.is_empty()) {
        Text::new("TRAWL_HTTP_ADDR from the environment")
    } else {
        Text::new("[server] http_addr in ").path(&ctx.config_path)
    };

    let dials = match listener_addrs(&ctx.config.server.http_addr).await {
        Ok(dials) => dials,
        Err(row) => {
            runner.record(gate, row.source(from));
            return None;
        }
    };
    let leaf = ctx.tls.leaf.clone();
    let deadline = tokio::time::Instant::now() + PROBE_DEADLINE;
    let mut attempts = Vec::with_capacity(dials.len());
    for &dial in &dials {
        let budget = deadline.saturating_duration_since(tokio::time::Instant::now());
        let found = if budget.is_zero() {
            Attempt {
                row: timed_out_row(&|text| dialed_text(text, dial, dials.len())),
                answer: None,
                not_listening: false,
                decisive: false,
            }
        } else {
            attempt(dial, dials.len(), leaf.as_ref(), budget).await
        };
        let decisive = found.decisive;
        attempts.push(found);
        if decisive {
            break;
        }
    }
    let (row, answer, refused) = settle(attempts, dials.len());
    ctx.listener_refused = refused;

    // The comparison holds only for material that did not change meanwhile.
    if read_material(source).await != before {
        runner.record(
            gate,
            Row::not_sampled(check, reason::MATERIAL_CHANGED)
                .detail(Text::new(
                    "the certificate or key changed while the doctor compared them with the \
                     listener",
                ))
                .source(from)
                .next(Text::new("rerun once they stop changing")),
        );
        return None;
    }
    runner.record(gate, row.source(from));
    answer
}

/// The identity row for a connection that did not open.
fn contact_row(code: &'static str, dialed: &impl Fn(Text) -> Text) -> Row {
    let check = ServerCheck::ListenerIdentity;
    if code == reason::PERMISSION_DENIED {
        Row::not_sampled(check, code).detail(dialed(Text::new(
            "this user may not connect to the configured address",
        )))
    } else {
        Row::not_sampled(check, code)
            .detail(dialed(Text::new(
                "nothing accepts connections at the configured address",
            )))
            .next(Text::new("start trawld, then rerun"))
    }
}

fn timed_out_row(dialed: &impl Fn(Text) -> Text) -> Row {
    Row::not_sampled(ServerCheck::ListenerIdentity, reason::TIMED_OUT)
        .detail(dialed(Text::new("the listener did not answer in time")))
}

/// The identity row for a TLS probe, from what the verifier saw first and
/// the request's result second.
fn probe_row(seen: Seen, answer: &Answer, dialed: &impl Fn(Text) -> Text) -> Row {
    let check = ServerCheck::ListenerIdentity;
    let not_trawld =
        Text::new("check that trawld, and not another service, listens at the configured address");
    if seen.leaf == Some(false) {
        return Row::failed(
            check,
            "the listener serves another certificate than the one on disk",
        )
        .detail(dialed(Text::new("the served leaf differs from the file")))
        .next(Text::new(
            "restart trawld, or wait for its certificate reload, so it serves the \
                 certificate on disk; if trawld is not running, find what holds the port",
        ));
    }
    if seen.leaf == Some(true) && seen.signature == Some(false) {
        return Row::failed(
            check,
            "the listener presented the certificate on disk but did not prove it holds its key",
        )
        .detail(dialed(Text::new("a handshake signature did not verify")))
        .next(not_trawld);
    }
    if seen.proven() {
        return Row::complete(check).detail(dialed(Text::new(
            "the listener serves the certificate on disk and proved it holds its key",
        )));
    }
    match answer {
        // An answer without a verified leaf and signature cannot happen
        // with resumption off; it is never taken as proof.
        Ok(_) => Row::failed(
            check,
            "the listener's handshake did not verify its certificate",
        )
        .next(not_trawld),
        Err(error) if error.is_timeout() => timed_out_row(dialed),
        Err(error) => {
            let kind = io_kind(error);
            if let Some(code) = kind.and_then(refusal) {
                contact_row(code, dialed)
            } else if kind.is_some_and(cut_off) {
                Row::not_sampled(check, reason::INTERRUPTED)
                    .detail(dialed(Text::new(
                        "the connection ended before the TLS handshake was whole, with nothing \
                         that shows another protocol",
                    )))
                    .next(Text::new(
                        "rerun; if it keeps breaking off, read trawld's log",
                    ))
            } else {
                Row::failed(check, "the listener did not complete a TLS handshake")
                    .detail(dialed(Text::new("the connection opened")))
                    .next(not_trawld)
            }
        }
    }
}

/// Whether a connection error says the peer ended the connection, by a
/// close or a reset, before the exchange was whole. The TLS layer reports
/// bytes that are not TLS, and a peer's alert, as invalid data instead: a
/// cut-off handshake proves nothing about the listener, those do.
fn cut_off(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::UnexpectedEof
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
    )
}

// -- server.listener.health ------------------------------------------------

/// `server.listener.health` and its `server.listener.health.<key>` rows,
/// from the answer the identity probe received.
///
/// Only an answer the protocol proves whole is classified. One that breaks
/// off, before its head or inside its body, is `not_sampled`,
/// [`reason::INTERRUPTED`], with no per-key rows: what arrived may be a prefix of
/// anything. A status that arrived whole is still judged, since no body
/// makes a status trawld does not send its own.
async fn check_health(runner: &mut Runner, answer: Option<Answer>) {
    let check = ServerCheck::ListenerHealth;
    let Some(gate) = runner.gate(check) else {
        return;
    };
    let answer =
        answer.expect("server.listener.health reads only an answer from a proven listener");
    let interrupted = |detail: &'static str| {
        (
            Row::not_sampled(check, reason::INTERRUPTED)
                .detail(Text::new(detail))
                .next(Text::new(
                    "rerun; if it keeps breaking off, read trawld's log",
                )),
            Vec::new(),
        )
    };
    let (row, keyed) = match answer {
        Err(error) if error.is_timeout() => (
            Row::not_sampled(check, reason::TIMED_OUT)
                .detail(Text::new("the health answer did not arrive in time")),
            Vec::new(),
        ),
        Err(error) if not_http(&error) => (
            Row::failed(
                check,
                "the listener proved its certificate but answered with something other than HTTP",
            )
            .next(Text::new(NOT_TRAWLD)),
            Vec::new(),
        ),
        Err(_) => interrupted("the connection ended before the health answer's head was whole"),
        Ok(response) => {
            let status = response.status().as_u16();
            match (
                read_capped(response, HEALTH_BODY_MAX).await,
                status_row(status),
            ) {
                (Ok(body), _) => health_rows(status, &body),
                (Err(_), Some(row)) => (row, Vec::new()),
                (Err(BodyFault::TooLarge), None) => (
                    Row::not_sampled(check, reason::TOO_LARGE).detail(
                        Text::new("the doctor reads at most ")
                            .int(u32::try_from(HEALTH_BODY_MAX).unwrap_or(u32::MAX))
                            .lit(" bytes of the health answer"),
                    ),
                    Vec::new(),
                ),
                (Err(BodyFault::TimedOut), None) => (
                    Row::not_sampled(check, reason::TIMED_OUT)
                        .detail(Text::new("the health answer did not arrive in time")),
                    Vec::new(),
                ),
                (Err(BodyFault::Broken), None) => {
                    interrupted("the health answer's body broke off before it was whole")
                }
            }
        }
    };
    let row = row.source(Text::new("GET ").lit(HEALTH_PATH).lit(" on the listener"));
    runner.record_with(gate, row, keyed);
}

/// The next action for an answer that is not trawld's.
const NOT_TRAWLD: &str =
    "check that trawld, and not another service, listens at the configured address";

/// Whether the request failed because the listener's bytes are not an HTTP
/// response: evidence about the listener, not a failure to observe it.
fn not_http(error: &reqwest::Error) -> bool {
    let mut source = std::error::Error::source(error);
    while let Some(inner) = source {
        if inner
            .downcast_ref::<hyper::Error>()
            .is_some_and(hyper::Error::is_parse)
        {
            return true;
        }
        source = inner.source();
    }
    false
}

/// Why the health body was not read whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyFault {
    TooLarge,
    TimedOut,
    Broken,
}

/// The response body, read chunk by chunk, refused past `max` bytes.
async fn read_capped(mut response: reqwest::Response, max: usize) -> Result<Vec<u8>, BodyFault> {
    if response
        .content_length()
        .is_some_and(|length| length > max as u64)
    {
        return Err(BodyFault::TooLarge);
    }
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if body.len() + chunk.len() > max {
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

/// The health rows for a body that arrived whole with HTTP `status`.
///
/// A 200 or 503 health body keeps its rows, one per reported check, so a
/// 503 from a failed `DuckDB` probe fails its own row. An `unavailable`
/// answer, which trawld sends only with a 503, fails the check itself
/// whatever its checks say, even when it reports none or only `ok` ones. A
/// 503 `corpus_recovering` envelope is `recovering`, as the classifier maps
/// the corpus recovery values. Anything else is not trawld's health answer.
fn health_rows(status: u16, body: &[u8]) -> (Row, Vec<Row>) {
    let check = ServerCheck::ListenerHealth;
    if let Some(row) = status_row(status) {
        return (row, Vec::new());
    }
    if let Ok(health) = serde_json::from_slice::<HealthResponse>(body)
        && let Some(checks) = &health.checks
    {
        let agrees = matches!(
            (status, &health.status),
            (200, HealthStatus::Ok | HealthStatus::Degraded) | (503, HealthStatus::Unavailable)
        );
        if !agrees {
            return (
                Row::failed(check, "the health status and the HTTP status disagree"),
                Vec::new(),
            );
        }
        let shown = match health.status {
            HealthStatus::Ok => "ok",
            HealthStatus::Degraded => "degraded",
            HealthStatus::Unavailable => "unavailable",
        };
        let detail = Text::new("status: ").lit(shown).lit("; HTTP ").int(status);
        let row = if matches!(health.status, HealthStatus::Unavailable) {
            Row::failed(check, "trawld reports itself unavailable")
                .detail(detail)
                .next(Text::new(
                    "read the per-check rows and trawld's log for why it is unavailable",
                ))
        } else {
            Row::complete(check).detail(detail)
        };
        return (row, keyed_rows(checks));
    }
    if status == 503
        && let Ok(refusal) = serde_json::from_slice::<ErrorResponse>(body)
        && refusal.error.code == ErrorCode::CorpusRecovering
    {
        return (
            Row::not_sampled(check, reason::RECOVERING)
                .detail(Text::new(
                    "trawld answered corpus_recovering: its corpus is still recovering after a \
                     restart",
                ))
                .next(Text::new("wait for trawld to finish recovery, then rerun")),
            Vec::new(),
        );
    }
    (
        Row::failed(check, "the health answer is not trawld's health body").next(Text::new(
            "check that trawld, and not another service, listens at the configured address",
        )),
        Vec::new(),
    )
}

/// The failed health row for an HTTP `status` trawld's health endpoint
/// never sends, whatever the body; `None` for 200 and 503.
fn status_row(status: u16) -> Option<Row> {
    (!matches!(status, 200 | 503)).then(|| {
        Row::failed(
            ServerCheck::ListenerHealth,
            "the health endpoint answered with a status trawld does not send",
        )
        .detail(Text::new("HTTP ").int(status))
        .next(Text::new(NOT_TRAWLD))
    })
}

/// One row per reported check, sorted by name. Names trawld's health
/// endpoint does not report become one failed
/// `server.listener.health._invalid` row, and none of them is shown: the
/// report names only checks it knows, since even an identifier-shaped name
/// may be a secret.
fn keyed_rows(checks: &HashMap<String, String>) -> Vec<Row> {
    let mut named = BTreeMap::new();
    let mut invalid = 0_u32;
    for (name, value) in checks {
        match HealthKey::new(name) {
            Some(key) => {
                named.insert(name.as_str(), (key, value.as_str()));
            }
            None => invalid = invalid.saturating_add(1),
        }
    }
    let mut rows: Vec<Row> = named
        .into_iter()
        .map(|(name, (key, value))| key_row(name, &key, value))
        .collect();
    if invalid > 0 {
        rows.push(
            Row::for_key(
                ServerCheck::ListenerHealth,
                HealthKey::invalid(),
                Outcome::Failed,
                Some("trawld reported a check name this doctor does not know"),
            )
            .detail(Text::new("").int(invalid).lit(" such name(s), not shown"))
            .next(Text::new(
                "check that trawld, and not another service, listens at the configured address",
            )),
        );
    }
    rows
}

/// The row for the check `name`, shown as `key`, that reported `value`.
///
/// No value a server sends is shown as sent: a known value is named by a
/// literal of this doctor's, and a value it does not know is not shown at
/// all, since even an identifier-shaped value may be a secret.
fn key_row(name: &str, key: &HealthKey, value: &str) -> Row {
    let class = health::classify(name, value);
    let row = |reason| Row::for_key(ServerCheck::ListenerHealth, *key, class.outcome(), reason);
    match class {
        ValueClass::Ok => row(None).detail(Text::new("reported ok")),
        // A recovering corpus is not a failure: trawld cannot yet vouch for
        // what it holds, so the doctor could not look (ADR-0047).
        ValueClass::Recovering => row(Some(reason::RECOVERING))
            .detail(Text::new(match value {
                "rollup_pending" => "reported rollup_pending",
                "restart_backlog" => "reported restart_backlog",
                _ => "reported a recovery value",
            }))
            .next(Text::new("wait for trawld to finish recovery, then rerun")),
        ValueClass::Refused => row(Some(if value == "refusing" {
            "reported refusing"
        } else {
            "reported error"
        }))
        .next(
            Text::new("read trawld's log for why ")
                .key(key)
                .lit(" fails"),
        ),
        ValueClass::Unknown => row(Some("reported a value this doctor does not know"))
            .detail(Text::new("the value is not shown"))
            .next(Text::new("read trawld's log for ").key(key)),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use rustls::pki_types::PrivateKeyDer;
    use rustls::server::{ClientHello, ResolvesServerCert};
    use rustls::sign::CertifiedKey;
    use rustls::{ServerConfig, SupportedProtocolVersion};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;

    /// A fresh self-signed pair whose names are `names`.
    fn pair(names: &[&str]) -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
        let names: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(names).unwrap();
        (
            cert.der().clone(),
            PrivateKeyDer::Pkcs8(signing_key.serialize_der().into()),
        )
    }

    /// `cert` served with `key`, which need not be its own: `CertifiedKey::new`
    /// checks nothing, so a server can present a leaf it cannot sign for.
    fn certified(cert: CertificateDer<'static>, key: &PrivateKeyDer<'static>) -> Arc<CertifiedKey> {
        let signer = rustls::crypto::ring::default_provider()
            .key_provider
            .load_private_key(key.clone_key())
            .unwrap();
        Arc::new(CertifiedKey::new(vec![cert], signer))
    }

    /// What a server saw of the client's hello.
    #[derive(Debug, Default)]
    struct Hello {
        alpn: Option<Vec<Vec<u8>>>,
        sni: Option<String>,
        count: usize,
    }

    /// Serves one fixed key and records each client hello.
    #[derive(Debug)]
    struct Recording {
        key: Arc<CertifiedKey>,
        hello: Arc<Mutex<Hello>>,
    }

    impl ResolvesServerCert for Recording {
        fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
            let mut seen = self.hello.lock().unwrap();
            seen.alpn = hello
                .alpn()
                .map(|protocols| protocols.map(<[u8]>::to_vec).collect());
            seen.sni = hello.server_name().map(str::to_owned);
            seen.count += 1;
            Some(Arc::clone(&self.key))
        }
    }

    /// A TLS listener on loopback that serves `key` over `versions`,
    /// offering ALPN `h2` and `http/1.1` as trawld does, and answers every
    /// request with `response`.
    async fn listener(
        key: Arc<CertifiedKey>,
        versions: &[&'static SupportedProtocolVersion],
        response: &'static [u8],
    ) -> (SocketAddr, Arc<Mutex<Hello>>) {
        let hello = Arc::new(Mutex::new(Hello::default()));
        let mut config =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_protocol_versions(versions)
                .unwrap()
                .with_no_client_auth()
                .with_cert_resolver(Arc::new(Recording {
                    key,
                    hello: Arc::clone(&hello),
                }));
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = socket.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let mut request = Vec::new();
                    let mut buf = [0_u8; 1024];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        match tls.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => request.extend_from_slice(&buf[..n]),
                        }
                    }
                    let _ = tls.write_all(response).await;
                    let _ = tls.shutdown().await;
                });
            }
        });
        (addr, hello)
    }

    const HEALTHY: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
        content-length: 63\r\nconnection: close\r\n\r\n\
        {\"status\":\"ok\",\"checks\":{\"duckdb\":\"ok\"},\"version\":\"0.0.0-test\"}";

    const V12: &[&SupportedProtocolVersion] = &[&rustls::version::TLS12];
    const V13: &[&SupportedProtocolVersion] = &[&rustls::version::TLS13];

    /// reqwest runs the probe on the doctor's own `ClientConfig`, not one
    /// of its own: a self-signed certificate that names only a DNS host,
    /// dialed by IP, passes only through the pinned-leaf verifier, which
    /// records that it ran; the hello carries exactly ALPN `http/1.1` and
    /// no SNI. Over TLS 1.2 and 1.3, both of which the probe offers.
    #[tokio::test]
    async fn the_probe_runs_on_the_pinned_client_config() {
        for versions in [V12, V13] {
            let (cert, key) = pair(&["trawl.lab.example"]);
            let (addr, hello) = listener(certified(cert.clone(), &key), versions, HEALTHY).await;
            let Probe { seen, answer } =
                probe(addr, &Der(cert.as_ref().to_vec()), PROBE_DEADLINE).await;
            let response = answer
                .expect("the client builds")
                .expect("the probe is answered");
            assert_eq!(response.status(), 200);
            assert_eq!(
                seen,
                Seen {
                    leaf: Some(true),
                    signature: Some(true)
                }
            );
            let hello = hello.lock().unwrap();
            assert_eq!(hello.alpn, Some(vec![b"http/1.1".to_vec()]), "{versions:?}");
            assert_eq!(hello.sni, None);
            assert_eq!(hello.count, 1, "one connection, no retry");
        }
    }

    /// A listener serving any other certificate is refused at its leaf,
    /// before any signature is checked.
    #[tokio::test]
    async fn another_leaf_is_refused() {
        let (served, key) = pair(&["localhost"]);
        let (pinned, _) = pair(&["localhost"]);
        let (addr, _) = listener(certified(served, &key), V13, HEALTHY).await;
        let Probe { seen, answer } =
            probe(addr, &Der(pinned.as_ref().to_vec()), PROBE_DEADLINE).await;
        let answer = answer.expect("the client builds");
        assert!(answer.is_err(), "no answer from an unpinned leaf");
        assert_eq!(
            seen,
            Seen {
                leaf: Some(false),
                signature: None
            }
        );
        let row = probe_row(seen, &answer, &|text| text);
        assert_eq!(
            (row.outcome(), row.reason()),
            (
                Outcome::Failed,
                Some("the listener serves another certificate than the one on disk")
            )
        );
    }

    /// The pinned leaf alone proves nothing: a server that presents it but
    /// signs the handshake with another key fails signature verification,
    /// over TLS 1.2 and 1.3, and the probe never gets an answer.
    #[tokio::test]
    async fn the_pinned_leaf_without_its_key_is_refused() {
        for versions in [V12, V13] {
            let (cert, _) = pair(&["localhost"]);
            let (_, other_key) = pair(&["localhost"]);
            let (addr, _) = listener(certified(cert.clone(), &other_key), versions, HEALTHY).await;
            let Probe { seen, answer } =
                probe(addr, &Der(cert.as_ref().to_vec()), PROBE_DEADLINE).await;
            let answer = answer.expect("the client builds");
            assert!(answer.is_err(), "{versions:?}: no answer without the key");
            assert_eq!(
                seen,
                Seen {
                    leaf: Some(true),
                    signature: Some(false)
                },
                "{versions:?}"
            );
            let row = probe_row(seen, &answer, &|text| text);
            assert_eq!(row.outcome(), Outcome::Failed);
            assert_eq!(
                row.reason(),
                Some(
                    "the listener presented the certificate on disk but did not prove it holds \
                     its key"
                )
            );
        }
    }

    /// The verifier refuses a signature from any certificate but the pinned
    /// one, whatever called it, and a failed signature is never overwritten
    /// by a later good one.
    #[test]
    fn a_signature_by_another_certificate_is_refused() {
        let (pinned, _) = pair(&["localhost"]);
        let (other, _) = pair(&["localhost"]);
        let provider = rustls::crypto::ring::default_provider();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let verifier = PinnedLeaf {
            leaf: pinned,
            algorithms: provider.signature_verification_algorithms,
            seen: Arc::clone(&seen),
        };
        let called = std::cell::Cell::new(false);
        let result = verifier.signature(&other, || {
            called.set(true);
            Ok(HandshakeSignatureValid::assertion())
        });
        assert!(result.is_err());
        assert!(
            !called.get(),
            "no verification runs for another certificate"
        );
        let leaf = verifier.leaf.clone();
        assert!(
            verifier
                .signature(&leaf, || Ok(HandshakeSignatureValid::assertion()))
                .is_ok()
        );
        assert_eq!(
            seen.lock().unwrap().signature,
            Some(false),
            "a failure sticks"
        );
        assert_eq!(format!("{verifier:?}"), "PinnedLeaf(..)");
        assert!(!verifier.supported_verify_schemes().is_empty());
    }

    /// A port nothing listens on is `not_listening` through both paths: the
    /// TLS probe and the plain connection of a run with no material yet.
    /// A port something listens on is accepted, and nothing is sent to it.
    #[tokio::test]
    async fn a_closed_port_is_not_listening() {
        let closed = {
            let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            socket.local_addr().unwrap()
        };
        let (cert, _) = pair(&["localhost"]);
        let Probe { seen, answer } =
            probe(closed, &Der(cert.as_ref().to_vec()), PROBE_DEADLINE).await;
        let answer = answer.expect("the client builds");
        assert_eq!(seen, Seen::default(), "no handshake began");
        assert_eq!(
            answer.as_ref().err().and_then(io_kind),
            Some(io::ErrorKind::ConnectionRefused)
        );
        let row = probe_row(seen, &answer, &|text| text);
        assert_eq!(
            (row.outcome(), row.reason()),
            (Outcome::NotSampled, Some(reason::NOT_LISTENING))
        );
        assert_eq!(
            tcp_contact(closed, CONNECT_DEADLINE).await,
            Contact::Refused(reason::NOT_LISTENING)
        );

        let open = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = open.local_addr().unwrap();
        let received = tokio::spawn(async move {
            let (mut stream, _) = open.accept().await.unwrap();
            let mut bytes = Vec::new();
            stream.read_to_end(&mut bytes).await.unwrap();
            bytes
        });
        assert_eq!(tcp_contact(addr, CONNECT_DEADLINE).await, Contact::Accepted);
        assert!(received.await.unwrap().is_empty(), "nothing was sent");
    }

    /// Of the addresses a host name resolves to, any proof settles the
    /// identity. One address gives what it found, so its stable mismatch
    /// fails; several with no proof leave unknown which one trawld binds,
    /// unless nothing listens at any of them.
    #[test]
    fn several_addresses_settle_on_a_proof_or_stay_unknown() {
        use Outcome::{Complete, Failed, NotSampled};

        let check = ServerCheck::ListenerIdentity;
        let other = "the listener serves another certificate than the one on disk";
        let found = |row: Row, not_listening, decisive| Attempt {
            row,
            answer: None,
            not_listening,
            decisive,
        };
        let mismatch = || found(Row::failed(check, other), false, false);
        let refused = || found(Row::not_sampled(check, reason::NOT_LISTENING), true, false);
        let late = || found(Row::not_sampled(check, reason::TIMED_OUT), false, false);
        let proven = || found(Row::complete(check), false, true);
        let settled = |attempts, count| {
            let (row, _, refused) = settle(attempts, count);
            (row.outcome(), row.reason(), refused)
        };
        let ambiguous = (NotSampled, Some(reason::AMBIGUOUS_ADDRESS), false);

        assert_eq!(settled(vec![mismatch()], 1), (Failed, Some(other), false));
        assert_eq!(
            settled(vec![refused()], 1),
            (NotSampled, Some(reason::NOT_LISTENING), true)
        );
        assert_eq!(
            settled(vec![mismatch(), proven()], 2),
            (Complete, None, false)
        );
        assert_eq!(settled(vec![proven()], 2), (Complete, None, false));
        assert_eq!(settled(vec![mismatch(), mismatch()], 2), ambiguous);
        assert_eq!(settled(vec![mismatch(), refused()], 2), ambiguous);
        assert_eq!(settled(vec![refused(), late()], 2), ambiguous);
        assert_eq!(
            settled(vec![refused(), refused()], 2),
            (NotSampled, Some(reason::NOT_LISTENING), true)
        );
    }

    /// A socket address is dialed alone, a wildcard on loopback; a repeat
    /// in what a name resolves to is dialed once.
    #[tokio::test]
    async fn the_listener_address_is_dialed_as_resolved() {
        assert_eq!(
            listener_addrs("0.0.0.0:5514").await.unwrap(),
            [Dial {
                addr: "127.0.0.1:5514".parse().unwrap(),
                wildcard: true
            }]
        );
        let dials = listener_addrs("localhost:5514").await.unwrap();
        let unique: std::collections::HashSet<_> = dials.iter().map(|dial| dial.addr).collect();
        assert_eq!(dials.len(), unique.len(), "{dials:?}");
        assert!(
            dials
                .iter()
                .all(|dial| dial.addr.ip().is_loopback() && !dial.wildcard),
            "{dials:?}"
        );
    }

    #[test]
    fn a_wildcard_is_dialed_on_loopback() {
        let dial = |addr: &str| dial_addr(addr.parse().unwrap());
        assert_eq!(
            dial("0.0.0.0:5514"),
            ("127.0.0.1:5514".parse().unwrap(), true)
        );
        assert_eq!(dial("[::]:5514"), ("[::1]:5514".parse().unwrap(), true));
        assert_eq!(
            dial("10.1.2.3:5514"),
            ("10.1.2.3:5514".parse().unwrap(), false)
        );
        assert_eq!(dial("[::1]:443"), ("[::1]:443".parse().unwrap(), false));
    }

    /// A PEM pair as `rcgen` writes it, valid from `from` to `to` (years).
    fn pem_pair(from: i32, to: i32) -> (String, String) {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(vec!["localhost".to_owned()]).unwrap();
        params.not_before = rcgen::date_time_ymd(from, 1, 1);
        params.not_after = rcgen::date_time_ymd(to, 1, 1);
        (params.self_signed(&key).unwrap().pem(), key.serialize_pem())
    }

    #[test]
    fn a_pair_is_judged_as_boot_serves_it_and_by_its_dates() {
        let (cert, key) = pem_pair(2000, 3000);
        let judged = judge_pair(cert.as_bytes(), key.as_bytes()).expect("a good pair");
        assert!(judged.days_left > 300_000, "{judged:?}");
        let (_, other_key) = pem_pair(2000, 3000);
        let (expired, expired_key) = pem_pair(2000, 2001);
        let (future, future_key) = pem_pair(2999, 3000);
        let cases: [(&str, &str, Misjudged); 5] = [
            (
                &cert,
                &other_key,
                Misjudged::Content(
                    "the private key does not belong to the certificate, or trawld cannot use it",
                ),
            ),
            (
                &cert,
                "not a key",
                Misjudged::Content("the key file holds no PEM private key"),
            ),
            (
                "no certificate here",
                &key,
                Misjudged::Content("the certificate file holds no PEM certificate"),
            ),
            (
                &expired,
                &expired_key,
                Misjudged::Dates("the certificate has expired"),
            ),
            (
                &future,
                &future_key,
                Misjudged::Dates("the certificate is not valid yet"),
            ),
        ];
        for (cert, key, expected) in cases {
            assert_eq!(
                judge_pair(cert.as_bytes(), key.as_bytes()).unwrap_err(),
                expected
            );
        }
        // An expired certificate in the chain behind a valid leaf.
        let chain = format!("{cert}{expired}");
        assert_eq!(
            judge_pair(chain.as_bytes(), key.as_bytes()).unwrap_err(),
            Misjudged::Dates("a certificate in its chain has expired")
        );
    }

    fn summary(rows: &[Row]) -> Vec<(String, Outcome, Option<&'static str>)> {
        rows.iter()
            .map(|row| {
                let (outcome, reason) = (row.outcome(), row.reason());
                (row.clone().into_check().id, outcome, reason)
            })
            .collect()
    }

    /// Each reported value maps as the shared classifier says, and nothing
    /// a server sends is shown unless the report may show it.
    #[test]
    fn health_values_map_as_specified() {
        let body = br#"{"status":"degraded","version":"x","checks":{
            "duckdb":"ok","auth_db":"error","ingest_capacity":"refusing",
            "corpus":"rollup_pending","wal":"recovering","data_path":"Weird Value!",
            "storage_db":"private_secret","Bad-Key":"ok","_invalid":"ok",
            "private_secret":"ok",
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08":"ok"}}"#;
        let (row, keyed) = health_rows(200, body);
        assert_eq!((row.outcome(), row.reason()), (Outcome::Complete, None));
        let id = |key: &str| format!("server.listener.health.{key}");
        assert_eq!(
            summary(&keyed),
            [
                (id("auth_db"), Outcome::Failed, Some("reported error")),
                (id("corpus"), Outcome::NotSampled, Some(reason::RECOVERING)),
                (
                    id("data_path"),
                    Outcome::Failed,
                    Some("reported a value this doctor does not know")
                ),
                (id("duckdb"), Outcome::Complete, None),
                (
                    id("ingest_capacity"),
                    Outcome::Failed,
                    Some("reported refusing")
                ),
                (
                    id("storage_db"),
                    Outcome::Failed,
                    Some("reported a value this doctor does not know")
                ),
                (
                    id("_invalid"),
                    Outcome::Failed,
                    Some("trawld reported a check name this doctor does not know")
                ),
            ]
        );
        // No unknown value or name is shown, not even one shaped like an
        // identifier or a fingerprint.
        let shown = format!("{keyed:?}");
        for hidden in ["Weird", "Bad-Key", "secret", "wal", "9f86d081", "a08"] {
            assert!(!shown.contains(hidden), "{hidden}: {shown}");
        }
        let invalid = keyed.last().unwrap().clone().into_check();
        assert_eq!(invalid.detail.as_deref(), Some("5 such name(s), not shown"));
        for row in &keyed {
            let check = row.clone().into_check();
            if check.reason.as_deref() == Some("reported a value this doctor does not know") {
                assert_eq!(check.detail.as_deref(), Some("the value is not shown"));
            }
        }

        let recovering = br#"{"error":{"code":"corpus_recovering","message":"not yet"}}"#;
        let (row, keyed) = health_rows(503, recovering);
        assert_eq!(
            (row.outcome(), row.reason()),
            (Outcome::NotSampled, Some(reason::RECOVERING))
        );
        assert!(keyed.is_empty());

        for (status, body, reason) in [
            (
                200,
                &b"<html>hello</html>"[..],
                "the health answer is not trawld's health body",
            ),
            (
                503,
                &br#"{"error":{"code":"internal_error","message":"x"}}"#[..],
                "the health answer is not trawld's health body",
            ),
            (
                200,
                &br#"{"status":"unavailable","checks":{}}"#[..],
                "the health status and the HTTP status disagree",
            ),
            (
                404,
                &b"{}"[..],
                "the health endpoint answered with a status trawld does not send",
            ),
        ] {
            let (row, keyed) = health_rows(status, body);
            assert_eq!(
                (row.outcome(), row.reason()),
                (Outcome::Failed, Some(reason))
            );
            assert!(keyed.is_empty());
        }
    }

    /// An unavailable answer fails the check itself and keeps its rows,
    /// whatever they say: failing ones, none, or only ok ones.
    #[test]
    fn an_unavailable_answer_fails_whatever_its_checks_say() {
        let id = |key: &str| format!("server.listener.health.{key}");
        let unavailable = (Outcome::Failed, Some("trawld reports itself unavailable"));
        let (row, keyed) = health_rows(503, br#"{"status":"unavailable","checks":{}}"#);
        assert_eq!((row.outcome(), row.reason()), unavailable);
        assert!(keyed.is_empty());
        let (row, keyed) = health_rows(
            503,
            br#"{"status":"unavailable","checks":{"duckdb":"ok","auth_db":"ok"}}"#,
        );
        assert_eq!((row.outcome(), row.reason()), unavailable);
        assert_eq!(
            summary(&keyed),
            [
                (id("auth_db"), Outcome::Complete, None),
                (id("duckdb"), Outcome::Complete, None),
            ]
        );
        let (row, keyed) = health_rows(
            503,
            br#"{"status":"unavailable","checks":{"duckdb":"error","corpus":"restart_backlog"}}"#,
        );
        assert_eq!((row.outcome(), row.reason()), unavailable);
        assert_eq!(
            summary(&keyed),
            [
                (id("corpus"), Outcome::NotSampled, Some(reason::RECOVERING)),
                (id("duckdb"), Outcome::Failed, Some("reported error")),
            ]
        );
    }

    #[test]
    fn debug_never_shows_the_material() {
        let observed = Observed {
            material: Material::Pair {
                cert: b"private-cert".to_vec(),
                key: b"private-key".to_vec(),
            },
            foreign_owner: false,
        };
        let shown = format!("{observed:?}");
        assert!(!shown.contains("private"), "{shown}");
        assert!(shown.contains("12 bytes"), "{shown}");
    }
}
