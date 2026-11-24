// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `trawld --doctor` checks of the certificate and trawld's own listener.
//! The probe makes at most one `GET /api/v1/health` per address, over
//! HTTP/1.1 with no proxy, no redirect, no retry and no key, in one bounded
//! time for all addresses.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, DigitallySignedStruct, SignatureScheme};
use trawl_api::HealthStatus;
use trawl_api::doctor::health::{self, Health, KeyAnswer, Reported};
use trawl_api::doctor::{Outcome, reason};

use super::fsread::{self, Links, ReadFault};
use super::output::{HealthKey, Row, SelectedPath, Selection, Text};
use super::storage::{self, Access, RootSeen};
use super::{Ctx, Der, Runner, ServerCheck};
use crate::tls::{self, GeneratedPair, OwnerRule, Regenerate, TlsError, TlsSource, UnsafeReason};
use trawl_config::{GENERATED_KEY_DIR, GENERATED_TLS_DIR};

#[cfg(test)]
pub(super) const CHECKS: [ServerCheck; 3] = [
    ServerCheck::TlsMaterial,
    ServerCheck::ListenerIdentity,
    ServerCheck::ListenerHealth,
];

const CONNECT_DEADLINE: Duration = Duration::from_secs(5);

const PROBE_DEADLINE: Duration = Duration::from_secs(10);

const RESOLVE_DEADLINE: Duration = Duration::from_secs(5);

const HEALTH_BODY_MAX: usize = health::BODY_MAX;

const HEALTH_PATH: &str = "/api/v1/health";

pub(super) async fn run(ctx: &mut Ctx, runner: &mut Runner) {
    let source = MaterialSource::of(ctx);
    let before = check_material(ctx, runner, source.as_ref()).await;
    let answer = check_identity(ctx, runner, source.as_ref(), before).await;
    check_health(runner, answer).await;
}

#[derive(Debug)]
enum MaterialSource {
    Configured {
        cert: PathBuf,
        key: PathBuf,
        shown_cert: SelectedPath,
        shown_key: SelectedPath,
    },
    Generated {
        state_dir: PathBuf,
        owner: OwnerRule,
    },
}

impl MaterialSource {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    Cert,
    Key,
    Directory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    Read(Part, ReadFault),
    Refused(UnsafeReason),
}

#[derive(Clone, PartialEq, Eq)]
enum Material {
    Pair { cert: Vec<u8>, key: Vec<u8> },
    WillGenerate(Regenerate),
    Unread(Fault),
}

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

#[derive(Debug)]
struct Judged {
    leaf: Vec<u8>,
    /// Whole days until the leaf expires.
    days_left: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Misjudged {
    Content(&'static str),
    Dates(&'static str),
}

impl Misjudged {
    const fn reason(self) -> &'static str {
        match self {
            Self::Content(why) | Self::Dates(why) => why,
        }
    }
}

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
    } else if let (Material::WillGenerate(_), MaterialSource::Generated { state_dir, .. }) =
        (&observed.material, source)
        // A root run's access proves nothing about the service user's, and
        // its owner facet already says so.
        && !ctx.run_as.root
        && let Some(refused) = generation_row(state_dir.clone()).await
    {
        row = refused.source(source.text());
    }
    runner.record(gate, row);
    Some(observed)
}

/// When a start would generate a pair: the row that says it cannot create
/// or write what it generates into, or `None` when it can. The start
/// creates `tls/` and `tls-key/` under `state_dir`, creating `state_dir`
/// first when it is absent, and writes into both. Each directory that
/// exists is one the inspection opened as a directory this uid owns, and
/// the start re-modes it before it writes, so only a read-only filesystem
/// stops it there. Each absent one needs W and X on the nearest directory
/// above it that exists, reached as `server.data.root` reaches the data
/// root's.
async fn generation_row(state_dir: PathBuf) -> Option<Row> {
    let check = ServerCheck::TlsMaterial;
    let dirs = [GENERATED_TLS_DIR, GENERATED_KEY_DIR].map(|name| state_dir.join(name));
    let observe =
        tokio::task::spawn_blocking(move || dirs.map(|dir| storage::observe_created(&dir)));
    match tokio::time::timeout(fsread::READ_DEADLINE, observe).await {
        Err(_) => Some(Row::not_sampled(check, reason::TIMED_OUT)),
        Ok(Err(_)) => Some(Row::not_sampled(check, reason::UNREADABLE)),
        Ok(Ok(seen)) => seen.into_iter().find_map(generation_fault),
    }
}

fn generation_fault(seen: RootSeen) -> Option<Row> {
    let check = ServerCheck::TlsMaterial;
    let access = || {
        Text::new(
            "give the service user write access to trawld's state directory, the parent of \
             [data] path, or run as it",
        )
    };
    let point = || Text::new("point [data] path below a directory trawld can create");
    let row = match seen {
        RootSeen::Directory(Access::Granted | Access::NoWrite) => return None,
        seen if seen.holds() => return None,
        RootSeen::Absent(Some(Access::NoRead | Access::NoWrite)) => Row::failed(
            check,
            "the running user cannot create a generated TLS directory in the directory above it",
        )
        .next(access()),
        RootSeen::Directory(Access::ReadOnlyFs) | RootSeen::Absent(Some(Access::ReadOnlyFs)) => {
            Row::failed(
                check,
                "trawld's generated TLS directories are on a read-only filesystem",
            )
            .next(Text::new(
                "mount trawld's state directory read-write, or set [server] tls_cert_path and \
                 tls_key_path",
            ))
        }
        RootSeen::NotADirectory => Row::failed(check, refusal_reason(UnsafeReason::NotDirectory)),
        RootSeen::Dangling => Row::failed(check, refusal_reason(UnsafeReason::Symlink)),
        RootSeen::AncestorNotADirectory => Row::failed(
            check,
            "a parent of trawld's generated TLS directories is not a directory",
        )
        .next(point()),
        RootSeen::AncestorDangling => Row::failed(
            check,
            "a parent of trawld's generated TLS directories is a symlink to nothing",
        )
        .next(point()),
        RootSeen::Denied => Row::failed(
            check,
            "the running user cannot reach trawld's generated TLS directories",
        )
        .next(Text::new(
            "give the service user search access to every directory above trawld's state \
             directory",
        )),
        RootSeen::Vanished => Row::not_sampled(check, reason::MATERIAL_CHANGED).next(Text::new(
            "rerun once trawld's state directory stops changing",
        )),
        RootSeen::Directory(_) | RootSeen::Absent(_) | RootSeen::Unreadable => {
            Row::not_sampled(check, reason::UNREADABLE)
        }
    };
    Some(row)
}

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

fn changed_row(check: ServerCheck, source: &MaterialSource) -> Row {
    Row::not_sampled(check, reason::MATERIAL_CHANGED)
        .detail(Text::new(
            "the certificate or key changed while the doctor read them",
        ))
        .source(source.text())
        .next(Text::new("rerun once they stop changing"))
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Contact {
    Accepted,
    Refused(&'static str),
    TimedOut,
}

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

fn refusal(kind: io::ErrorKind) -> Option<&'static str> {
    if nothing_listens(kind) {
        Some(reason::NOT_LISTENING)
    } else if kind == io::ErrorKind::PermissionDenied {
        Some(reason::PERMISSION_DENIED)
    } else {
        None
    }
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Dial {
    addr: SocketAddr,
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Seen {
    leaf: Option<bool>,
    signature: Option<bool>,
}

impl Seen {
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
/// Nothing else in the chain, no host name, and no validity date is consulted:
/// the doctor asks whether this listener serves this certificate, not whether
/// a client would trust it.
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

/// No session resumption: every handshake verifies the certificate and a
/// signature.
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

#[derive(Debug)]
struct Probe {
    seen: Seen,
    answer: Option<Answer>,
}

type Answer = Result<reqwest::Response, reqwest::Error>;

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

#[derive(Debug)]
struct Attempt {
    row: Row,
    answer: Option<Answer>,
    not_listening: bool,
    /// This address settles the check whatever the others give: it proved
    /// the certificate, it accepted a connection when there is no material
    /// to compare, or the probe could not be built at all.
    decisive: bool,
}

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
                (Err(BodyFault::TooLarge), None) => (too_large_row(), Vec::new()),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyFault {
    TooLarge,
    TimedOut,
    Broken,
}

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

fn health_rows(status: u16, body: &[u8]) -> (Row, Vec<Row>) {
    let check = ServerCheck::ListenerHealth;
    match health::judge(status, body) {
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
        // A capacity refusal is trawld's own answer, not another
        // service's: the doctor could not look, and invents no checks.
        health::Answer::RequestLimit => (
            Row::not_sampled(check, reason::REQUEST_LIMIT_REACHED)
                .detail(Text::new(health::REQUEST_LIMIT_REFUSED))
                .next(Text::new(
                    "retry with backoff: rerun the doctor after trawld's requests in progress \
                     drain",
                )),
            Vec::new(),
        ),
        health::Answer::Status(status) => (status_failure(status), Vec::new()),
        health::Answer::TooLarge => (too_large_row(), Vec::new()),
        health::Answer::Disagrees => (
            Row::failed(check, "the health status and the HTTP status disagree"),
            Vec::new(),
        ),
        health::Answer::Foreign => (
            Row::failed(check, "the health answer is not trawld's health body").next(Text::new(
                "check that trawld, and not another service, listens at the configured address",
            )),
            Vec::new(),
        ),
    }
}

fn status_row(status: u16) -> Option<Row> {
    (!health::is_health_http_status(status)).then(|| status_failure(status))
}

fn status_failure(status: u16) -> Row {
    Row::failed(
        ServerCheck::ListenerHealth,
        "the health endpoint answered with a status trawld does not send",
    )
    .detail(Text::new("HTTP ").int(status))
    .next(Text::new(NOT_TRAWLD))
}

fn too_large_row() -> Row {
    Row::not_sampled(ServerCheck::ListenerHealth, reason::TOO_LARGE).detail(
        Text::new("the doctor reads at most ")
            .int(u32::try_from(HEALTH_BODY_MAX).unwrap_or(u32::MAX))
            .lit(" bytes of the health answer"),
    )
}

fn keyed_rows(health: &Health) -> Vec<Row> {
    let mut rows: Vec<Row> = health.checks().iter().map(key_row).collect();
    let invalid = health.unknown_names();
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

fn key_row(answer: &KeyAnswer) -> Row {
    let key = HealthKey::new(answer.name())
        .expect("the judge names only checks trawld's health endpoint reports");
    let row = |reason| Row::for_key(ServerCheck::ListenerHealth, key, answer.outcome(), reason);
    match answer.value() {
        Reported::Ok => row(None).detail(Text::new("reported ok")),
        Reported::Slow => row(None).detail(Text::new("reported slow")).next(
            Text::new("trawld is degraded, not down; read trawld's log if ")
                .key(&key)
                .lit(" stays slow"),
        ),
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
    use std::path::Path;
    use std::sync::Mutex;

    use rustls::pki_types::PrivateKeyDer;
    use rustls::server::{ClientHello, ResolvesServerCert};
    use rustls::sign::CertifiedKey;
    use rustls::{ServerConfig, SupportedProtocolVersion};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;

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

    #[derive(Debug, Default)]
    struct Hello {
        alpn: Option<Vec<Vec<u8>>>,
        sni: Option<String>,
        count: usize,
    }

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

    #[test]
    fn health_slow_engine_is_degraded_not_failed() {
        let body =
            br#"{"status":"degraded","version":"x","checks":{"duckdb":"slow","auth_db":"ok"}}"#;
        let (row, keyed) = health_rows(200, body);
        assert_eq!((row.outcome(), row.reason()), (Outcome::Complete, None));
        assert_eq!(
            summary(&keyed),
            [
                (
                    "server.listener.health.auth_db".to_owned(),
                    Outcome::Complete,
                    None
                ),
                (
                    "server.listener.health.duckdb".to_owned(),
                    Outcome::Complete,
                    None
                ),
            ]
        );
        let duckdb = keyed[1].clone().into_check();
        assert_eq!(duckdb.detail.as_deref(), Some("reported slow"));
        assert!(
            duckdb
                .next_action
                .is_some_and(|next| next.contains("degraded"))
        );
        let (disagrees, rows) = health_rows(
            503,
            br#"{"status":"degraded","version":"x","checks":{"duckdb":"slow"}}"#,
        );
        assert_eq!(disagrees.outcome(), Outcome::Failed);
        assert!(rows.is_empty());
    }

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

    /// Whether the running user writes a directory its mode forbids, as
    /// root or a holder of `CAP_DAC_OVERRIDE` does.
    #[cfg(unix)]
    fn writes_mode_0500(tmp: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt as _;
        let probe = tmp.join("probe");
        std::fs::create_dir(&probe).unwrap();
        std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o500)).unwrap();
        let privileged = std::fs::create_dir(probe.join("x")).is_ok();
        std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o700)).unwrap();
        privileged
    }

    /// One state of [`a_start_that_cannot_create_its_pair_fails`]: its
    /// name, the state directory, what to plant there, the directory to
    /// seal, and the reason an unprivileged run fails.
    type StateCase = (
        &'static str,
        &'static str,
        fn(&Path),
        &'static str,
        Option<&'static str>,
    );

    #[cfg(unix)]
    #[tokio::test]
    async fn a_start_that_cannot_create_its_pair_fails() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let privileged = writes_mode_0500(tmp.path());
        let create = Some(
            "the running user cannot create a generated TLS directory in the directory above it",
        );
        let set_mode = |path: &Path, mode: u32| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        let cases: [StateCase; 5] = [
            ("a writable state directory", "state", |_| {}, "", None),
            (
                "an unwritable state directory",
                "state",
                |state| std::fs::create_dir(state).unwrap(),
                "state",
                create,
            ),
            (
                "a state directory absent below an unwritable parent",
                "sealed/state",
                |state| std::fs::create_dir(state.parent().unwrap()).unwrap(),
                "sealed",
                create,
            ),
            (
                "unwritable generated directories in an unwritable state directory",
                "state",
                |state| {
                    for name in [GENERATED_TLS_DIR, GENERATED_KEY_DIR] {
                        std::fs::create_dir_all(state.join(name)).unwrap();
                        std::fs::set_permissions(
                            state.join(name),
                            std::fs::Permissions::from_mode(0o500),
                        )
                        .unwrap();
                    }
                },
                "state",
                None,
            ),
            (
                "a missing key directory in an unwritable state directory",
                "state",
                |state| std::fs::create_dir_all(state.join(GENERATED_TLS_DIR)).unwrap(),
                "state",
                create,
            ),
        ];
        for (name, state, plant, sealed, denied) in cases {
            let case = tempfile::tempdir().unwrap();
            let state = case.path().join(state);
            plant(&state);
            let sealed = case.path().join(sealed);
            if sealed != case.path() {
                set_mode(&sealed, 0o500);
            }
            let row = generation_row(state.clone()).await;
            let boot = tls::build_server_config(None, None, &state).is_ok();
            if sealed != case.path() {
                set_mode(&sealed, 0o700);
            }
            let expected = denied.filter(|_| !privileged);
            assert_eq!(
                row.as_ref().map(|row| (row.outcome(), row.reason())),
                expected.map(|why| (Outcome::Failed, Some(why))),
                "{name}"
            );
            assert_eq!(boot, expected.is_none(), "{name}: boot disagrees");
        }

        let file = tmp.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        let dangling = tmp.path().join("dangling");
        std::os::unix::fs::symlink(tmp.path().join("gone"), &dangling).unwrap();
        for (state, why) in [
            (
                file.clone(),
                "a parent of trawld's generated TLS directories is not a directory",
            ),
            (
                dangling.clone(),
                "a parent of trawld's generated TLS directories is a symlink to nothing",
            ),
        ] {
            let row = generation_row(state.clone()).await.expect("a refusal");
            assert_eq!((row.outcome(), row.reason()), (Outcome::Failed, Some(why)));
            assert!(tls::build_server_config(None, None, &state).is_err());
        }
    }
}
