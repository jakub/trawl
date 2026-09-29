// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! TLS certificate loading and self-signed certificate generation.
//!
//! When no cert/key paths are configured, a self-signed certificate is
//! auto-generated to `{state_dir}/tls/` and persisted across restarts.
//! The state directory is typically the parent of the data directory
//! (e.g. `/var/lib/trawl/tls/` for the deb package).
//!
//! `trawld --doctor` judges the same material without acting on it (#269,
//! D12): [`TlsSource::of`] picks the pair, [`parse_pem_pair`] and
//! [`serving_config`] are the one path from PEM bytes to a served
//! certificate, and [`inspect_generated_pair`] reads the generated pair
//! through the same directory checks and reads a start makes, creating,
//! moving, and removing nothing. Boot calls each of them, so the doctor's
//! answer and boot's cannot drift.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rustls::ServerConfig;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::TlsAcceptor;
use trawl_config::{GENERATED_CERT_FILE, GENERATED_KEY_DIR, GENERATED_KEY_FILE, GENERATED_TLS_DIR};

/// Why a path inside trawld's generated TLS directories is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsafeReason {
    /// It is a symbolic link.
    Symlink,
    /// A generated directory is not a directory.
    NotDirectory,
    /// `cert.pem` or `key.pem` is not a regular file.
    NotRegular,
    /// A generated directory is owned by `owner`, not by trawld's `euid`.
    ForeignOwner {
        /// The directory's owner.
        owner: u32,
        /// trawld's effective uid.
        euid: u32,
    },
}

impl std::fmt::Display for UnsafeReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Symlink => f.write_str(SYMLINK_REFUSAL),
            Self::NotDirectory => f.write_str("it is not a directory"),
            Self::NotRegular => f.write_str("it is not a regular file"),
            Self::ForeignOwner { owner, euid } => {
                write!(f, "it is owned by uid {owner}, not by trawld's uid {euid}")
            }
        }
    }
}

/// TLS configuration errors.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("failed to read certificate {}: {source}", path.display())]
    ReadCert {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to read private key {}: {source}", path.display())]
    ReadKey {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("invalid PEM certificate: {0}")]
    InvalidCert(String),

    #[error("invalid PEM private key: {0}")]
    InvalidKey(String),

    #[error("certificate generation failed: {0}")]
    Generation(String),

    #[error("failed to write TLS files: {0}")]
    Write(std::io::Error),

    #[error("TLS configuration error: {0}")]
    Config(String),

    /// A path inside trawld's generated TLS directories is not what trawld
    /// made there. It is refused, never followed.
    #[error("refusing {}: {reason}", path.display())]
    Unsafe { path: PathBuf, reason: UnsafeReason },
}

/// Where the listener's certificate and key come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsSource<'a> {
    /// `[server] tls_cert_path` and `tls_key_path`: the operator's pair.
    Configured {
        /// The certificate chain's PEM file.
        cert: &'a Path,
        /// The private key's PEM file.
        key: &'a Path,
    },
    /// Neither is set: the pair trawld generates under its state directory.
    Generated,
}

impl<'a> TlsSource<'a> {
    /// The source that `cert_path` and `key_path` select.
    ///
    /// # Errors
    /// [`TlsError::Config`] when only one of them is set.
    pub fn of(cert_path: Option<&'a Path>, key_path: Option<&'a Path>) -> Result<Self, TlsError> {
        match (cert_path, key_path) {
            (Some(cert), Some(key)) => Ok(Self::Configured { cert, key }),
            (None, None) => Ok(Self::Generated),
            _ => Err(TlsError::Config(
                "both tls_cert_path and tls_key_path must be set, or neither".into(),
            )),
        }
    }
}

/// Build a `rustls` [`ServerConfig`] from user-provided cert/key paths,
/// or auto-generate a self-signed certificate if neither is set.
///
/// `state_dir` is the base directory for the auto-generated pair (the
/// certificate in `{state_dir}/tls/`, the key in `{state_dir}/tls-key/`).
/// Only consulted when both cert/key paths are `None`.
///
/// Returns `Err` if only one of cert/key is provided.
pub fn build_server_config(
    cert_path: Option<&Path>,
    key_path: Option<&Path>,
    state_dir: &Path,
) -> Result<(Arc<ServerConfig>, bool), TlsError> {
    let (cert_pem, key_pem, self_signed) = match TlsSource::of(cert_path, key_path)? {
        TlsSource::Configured { cert, key } => {
            let (c, k) = load_pem_files(cert, key)?;
            (c, k, false)
        }
        TlsSource::Generated => {
            let (c, k, generated) = load_or_generate_default(state_dir)?;
            (c, k, generated)
        }
    };

    log_cert_details(&cert_pem);

    let (certs, key) = parse_pem_pair(&cert_pem, &key_pem)?;
    let config = serving_config(certs, key)?;

    Ok((Arc::new(config), self_signed))
}

/// The certificate chain and the private key of a PEM pair, parsed the way
/// the listener's certificate is parsed.
///
/// A file with no PEM certificate in it parses to an empty chain, which
/// [`serving_config`] refuses.
///
/// # Errors
/// [`TlsError::InvalidCert`] or [`TlsError::InvalidKey`] when a file is
/// not PEM.
pub fn parse_pem_pair(
    cert_pem: &[u8],
    key_pem: &[u8],
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), TlsError> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| TlsError::InvalidCert(e.to_string()))?;

    let key =
        PrivateKeyDer::from_pem_slice(key_pem).map_err(|e| TlsError::InvalidKey(e.to_string()))?;
    Ok((certs, key))
}

/// The listener's `rustls` config for a parsed pair: TLS 1.2 and 1.3 from
/// the ring provider, and ALPN for HTTP/2 and HTTP/1.1.
///
/// # Errors
/// [`TlsError::Config`] when the chain is empty, or the key is not one
/// `rustls` can sign with or its public half is not the leaf's.
pub fn serving_config(
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<ServerConfig, TlsError> {
    let mut config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|e| TlsError::Config(e.to_string()))?
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|e| TlsError::Config(e.to_string()))?;

    // Support HTTP/2 and HTTP/1.1 via ALPN negotiation.
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    Ok(config)
}

/// Read cert and key PEM files from disk.
fn load_pem_files(cert_path: &Path, key_path: &Path) -> Result<(Vec<u8>, Vec<u8>), TlsError> {
    let cert = fs::read(cert_path).map_err(|e| TlsError::ReadCert {
        path: cert_path.to_owned(),
        source: e,
    })?;
    let key = fs::read(key_path).map_err(|e| TlsError::ReadKey {
        path: key_path.to_owned(),
        source: e,
    })?;
    Ok((cert, key))
}

/// Log certificate details (subject, issuer, validity period) for observability.
///
/// Parses the PEM-encoded certificate and emits a `tls_cert_details` event.
/// Failures are logged as warnings rather than propagated, since cert
/// details are informational — the actual TLS handshake will catch
/// genuinely broken certs.
fn log_cert_details(pem_bytes: &[u8]) {
    match x509_parser::pem::parse_x509_pem(pem_bytes) {
        Ok((_, pem)) => match pem.parse_x509() {
            Ok(cert) => {
                let subject = cert.subject().to_string();
                let issuer = cert.issuer().to_string();
                let not_before = cert
                    .validity()
                    .not_before
                    .to_rfc2822()
                    .unwrap_or_else(|e| format!("(invalid: {e})"));
                let not_after = cert
                    .validity()
                    .not_after
                    .to_rfc2822()
                    .unwrap_or_else(|e| format!("(invalid: {e})"));
                tracing::info!(
                    event_type = "tls_cert_details",
                    subject = %subject,
                    issuer = %issuer,
                    not_before = %not_before,
                    not_after = %not_after,
                    "TLS certificate details"
                );
            }
            Err(e) => {
                tracing::warn!(
                    event_type = "tls_cert_details",
                    error = %e,
                    "failed to parse X.509 certificate"
                );
            }
        },
        Err(e) => {
            tracing::warn!(
                event_type = "tls_cert_details",
                error = %e,
                "failed to parse PEM certificate"
            );
        }
    }
}

/// Load existing default certs or generate new self-signed ones.
///
/// `cert.pem` is also a published artifact: a `trawl-web` running as
/// another user pins it (ADR-0048), so it is world-readable and appears
/// only once complete. Generation removes any old `cert.pem` first, then
/// writes the key, and publishes `cert.pem` last, so a visible certificate
/// always has its own key.
///
/// The key lives in its own directory, `{state_dir}/tls-key/`, not beside
/// the certificate: the Helm chart's `trawl-web` sidecar mounts `tls/` to
/// pin `cert.pem`, and Kubernetes' fsGroup ownership walk makes every file
/// on the volume group-readable to that sidecar, so only keeping the key out
/// of the mount keeps it from the sidecar.
///
/// `tls/` is set to `0755` on every start: the proxy's uid must search it,
/// and no uid but trawld's may write where the proxy reads its pin.
///
/// Both directories are trawld's own: a symlink, a non-directory, or a
/// directory another uid owns is refused with [`TlsError::Unsafe`] (see
/// [`GeneratedDir`]), and so is a symlink or non-regular file at `cert.pem`
/// or `key.pem`. An existing key with extra hard links or another owner is
/// discarded and the pair regenerated (see [`key_exposure`]).
///
/// Returns `(cert_pem, key_pem, was_generated)`.
fn load_or_generate_default(state_dir: &Path) -> Result<(Vec<u8>, Vec<u8>, bool), TlsError> {
    fs::create_dir_all(state_dir).map_err(TlsError::Write)?;
    let tls_dir = GeneratedDir::open(&state_dir.join(GENERATED_TLS_DIR), Some(0o755))?;
    let key_dir = GeneratedDir::open(&state_dir.join(GENERATED_KEY_DIR), Some(0o700))?;
    let cert_path = tls_dir.path.join(GENERATED_CERT_FILE);
    let key_path = key_dir.path.join(GENERATED_KEY_FILE);

    // An older trawld wrote its key beside the certificate. No private key
    // stays in the directory the sidecar mounts; an old-layout pair then has
    // no key in `key_dir` and is regenerated below.
    if tls_dir
        .remove(GENERATED_KEY_FILE)
        .map_err(TlsError::Write)?
    {
        tracing::info!(
            event_type = "lifecycle",
            key = %tls_dir.path.join(GENERATED_KEY_FILE).display(),
            "removed a private key left in the certificate directory"
        );
    }

    // Both are read, so a symlink at either name is refused on every start,
    // not only on the one that finds the pair complete.
    let key = find_generated_key(Some(&key_dir), current_euid(), Reader::Start)?;
    if let KeyFound::Exposed(reason) = &key {
        tracing::warn!(
            event_type = "lifecycle",
            key = %key_path.display(),
            reason = %reason,
            "discarding a private key that may be readable outside trawld, generating a new pair"
        );
    }
    let cert = find_generated_cert(Some(&tls_dir), Reader::Start)?;
    if let GeneratedPair::Found { cert_pem, key_pem } = judge_generated_pair(cert, key) {
        tracing::info!(
            event_type = "lifecycle",
            cert = %cert_path.display(),
            key = %key_path.display(),
            "loading existing self-signed TLS certificate"
        );
        return Ok((cert_pem, key_pem, false));
    }

    tracing::info!(
        event_type = "lifecycle",
        "no TLS certificate found, generating self-signed certificate"
    );

    let subject_alt_names = vec![
        "localhost".to_owned(),
        "127.0.0.1".to_owned(),
        "::1".to_owned(),
    ];

    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(subject_alt_names)
            .map_err(|e| TlsError::Generation(e.to_string()))?;

    let cert_pem = cert.pem();
    let key_pem = signing_key.serialize_pem();

    // A certificate whose key was lost must go before the new key lands:
    // an interruption between the key write and the publication below would
    // otherwise leave the old certificate beside an unrelated key, and every
    // later start would load that pair and fail TLS. With it gone, an
    // interrupted generation leaves at most a key with no certificate, which
    // the next start regenerates. The directory sync makes the removal
    // durable before the new key exists.
    tls_dir
        .remove(GENERATED_CERT_FILE)
        .map_err(TlsError::Write)?;
    tls_dir.sync().map_err(TlsError::Write)?;

    // Write the private key with restricted permissions from the start
    // to avoid a TOCTOU window where the key is world-readable. A key left
    // by an interrupted generation is removed rather than truncated, so the
    // new key never inherits the old file's mode.
    {
        use std::io::Write;
        key_dir
            .remove(GENERATED_KEY_FILE)
            .map_err(TlsError::Write)?;
        let mut f = key_dir
            .create_new(GENERATED_KEY_FILE, 0o600)
            .map_err(TlsError::Write)?;
        f.write_all(key_pem.as_bytes()).map_err(TlsError::Write)?;
        f.sync_all().map_err(TlsError::Write)?;
    }

    publish_cert(&tls_dir, cert_pem.as_bytes()).map_err(TlsError::Write)?;

    tracing::info!(
        event_type = "lifecycle",
        cert = %cert_path.display(),
        key = %key_path.display(),
        "self-signed TLS certificate generated"
    );

    Ok((cert_pem.into_bytes(), key_pem.into_bytes(), true))
}

/// trawld's effective uid, which its generated directories and key must
/// belong to. `None` where the platform has no uid.
#[allow(clippy::unnecessary_wraps, reason = "there is no uid off unix")]
fn current_euid() -> Option<u32> {
    #[cfg(unix)]
    {
        Some(rustix::process::geteuid().as_raw())
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// How a generated file is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reader {
    /// As a start reads it: whole.
    Start,
    /// As `trawld --doctor` reads it (#269, D15): also `O_NOCTTY`, and at
    /// most this many bytes, past which the read fails with
    /// [`std::io::ErrorKind::FileTooLarge`].
    Bounded(u64),
}

/// What a start finds at `tls-key/key.pem`.
#[derive(Debug)]
enum KeyFound {
    /// No key.
    Absent,
    /// A key that may be readable outside trawld ([`key_exposure`]'s
    /// reason). A start discards it and generates a new pair.
    Exposed(String),
    /// A key a start loads, with the metadata of the file read.
    Read { pem: Vec<u8>, meta: fs::Metadata },
}

/// Read the generated key from `key_dir` (`None` when there is no such
/// directory) with `reader`, judged as a start judges it: by its directory
/// entry before it is opened, then by the opened file.
///
/// A key another uid owns (root `0600` after a restore) may be unreadable
/// to trawld, and the open would fail before its owner could be seen, so
/// the entry is judged first. That look is advisory; the check of the
/// opened file is the one that decides what is loaded. `euid` is the uid
/// the key must belong to; `None` judges its hard links only.
fn find_generated_key(
    key_dir: Option<&GeneratedDir>,
    euid: Option<u32>,
    reader: Reader,
) -> Result<KeyFound, TlsError> {
    let Some(key_dir) = key_dir else {
        return Ok(KeyFound::Absent);
    };
    let read_err = |path, source| TlsError::ReadKey { path, source };
    if let Some(reason) = key_dir.entry_exposure(GENERATED_KEY_FILE, euid, read_err)? {
        return Ok(KeyFound::Exposed(reason));
    }
    match key_dir.read(GENERATED_KEY_FILE, reader, read_err)? {
        None => Ok(KeyFound::Absent),
        Some((pem, meta)) => Ok(match metadata_exposure(&meta, euid) {
            Some(reason) => KeyFound::Exposed(reason),
            None => KeyFound::Read { pem, meta },
        }),
    }
}

/// Read the generated certificate from `tls_dir` (`None` when there is no
/// such directory) with `reader`, refusing what a start refuses.
fn find_generated_cert(
    tls_dir: Option<&GeneratedDir>,
    reader: Reader,
) -> Result<Option<Vec<u8>>, TlsError> {
    let Some(tls_dir) = tls_dir else {
        return Ok(None);
    };
    Ok(tls_dir
        .read(GENERATED_CERT_FILE, reader, |path, source| {
            TlsError::ReadCert { path, source }
        })?
        .map(|(pem, _)| pem))
}

/// What a start does with the generated pair it found.
#[derive(Clone, PartialEq, Eq)]
pub enum GeneratedPair {
    /// Both halves are there and the key is trawld's alone: the start loads
    /// them.
    Found {
        /// `tls/cert.pem`.
        cert_pem: Vec<u8>,
        /// `tls-key/key.pem`.
        key_pem: Vec<u8>,
    },
    /// The start generates a new pair.
    Regenerate(Regenerate),
}

/// `Debug` never shows the key.
impl std::fmt::Debug for GeneratedPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Found { cert_pem, key_pem } => f
                .debug_struct("Found")
                .field("cert_pem", &format_args!("{} bytes", cert_pem.len()))
                .field("key_pem", &format_args!("{} bytes", key_pem.len()))
                .finish(),
            Self::Regenerate(why) => f.debug_tuple("Regenerate").field(why).finish(),
        }
    }
}

/// Why a start generates a new pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Regenerate {
    /// There is neither a certificate nor a key.
    Absent,
    /// There is a key but no certificate.
    NoCertificate,
    /// There is a certificate but no key.
    NoKey,
    /// The key may be readable outside trawld, for this reason.
    KeyExposed(String),
}

/// A start loads a certificate and key it found, and generates a new pair
/// when either is missing or the key may be exposed.
fn judge_generated_pair(cert: Option<Vec<u8>>, key: KeyFound) -> GeneratedPair {
    match (cert, key) {
        (_, KeyFound::Exposed(reason)) => GeneratedPair::Regenerate(Regenerate::KeyExposed(reason)),
        (Some(cert_pem), KeyFound::Read { pem, .. }) => GeneratedPair::Found {
            cert_pem,
            key_pem: pem,
        },
        (None, KeyFound::Absent) => GeneratedPair::Regenerate(Regenerate::Absent),
        (None, KeyFound::Read { .. }) => GeneratedPair::Regenerate(Regenerate::NoCertificate),
        (Some(_), KeyFound::Absent) => GeneratedPair::Regenerate(Regenerate::NoKey),
    }
}

/// Whose the generated directories and key must be, for
/// [`inspect_generated_pair`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerRule {
    /// trawld runs as this uid: another owner is refused, or makes the key
    /// exposed, exactly as a start by this uid judges it.
    Enforced(u32),
    /// Not judged, as in a root run, which cannot know trawld's uid: an
    /// owner other than this uid is only recorded.
    Observed(u32),
}

/// What [`inspect_generated_pair`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedInspection {
    /// What a start does with the pair.
    pub pair: GeneratedPair,
    /// Under [`OwnerRule::Observed`]: a generated directory or the key
    /// belongs to another uid, which that rule does not judge.
    pub foreign_owner: bool,
}

/// Inspect the generated pair under `state_dir` as a start would find it,
/// and act on nothing: no directory is created, re-moded, or opened for
/// writing, and nothing is removed or renamed.
///
/// It makes a start's checks in a start's order: each directory is opened
/// without following a symlink and must be a directory of the right owner;
/// then the key is judged and read, then the certificate. A directory that
/// does not exist holds nothing, which a start creates. A read of the
/// certificate past `cert_max` bytes, or of the key past `key_max`, stops
/// with [`std::io::ErrorKind::FileTooLarge`].
///
/// The work is blocking; call it off the async runtime.
///
/// # Errors
/// What a start by `owner`'s uid would refuse ([`TlsError::Unsafe`]), and
/// failures to open a directory ([`TlsError::Write`]) or read a file
/// ([`TlsError::ReadKey`], [`TlsError::ReadCert`]).
pub fn inspect_generated_pair(
    state_dir: &Path,
    owner: OwnerRule,
    cert_max: u64,
    key_max: u64,
) -> Result<GeneratedInspection, TlsError> {
    let mut foreign_owner = false;
    let mut open = |path: PathBuf| -> Result<Option<GeneratedDir>, TlsError> {
        let dir = match GeneratedDir::open_existing(&path) {
            Err(TlsError::Write(e)) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            opened => opened?,
        };
        if let Some(owned_by) = dir.owner() {
            match owner {
                OwnerRule::Enforced(euid) => {
                    if let Some(reason) = foreign_owner_reason(owned_by, euid) {
                        return Err(unsafe_path(&path, reason));
                    }
                }
                OwnerRule::Observed(uid) => foreign_owner |= owned_by != uid,
            }
        }
        Ok(Some(dir))
    };
    let tls_dir = open(state_dir.join(GENERATED_TLS_DIR))?;
    let key_dir = open(state_dir.join(GENERATED_KEY_DIR))?;

    let judged_euid = match owner {
        OwnerRule::Enforced(euid) => Some(euid),
        OwnerRule::Observed(_) => None,
    };
    let key = find_generated_key(key_dir.as_ref(), judged_euid, Reader::Bounded(key_max))?;
    if let (OwnerRule::Observed(uid), KeyFound::Read { meta, .. }) = (owner, &key) {
        foreign_owner |= file_owner(meta).is_some_and(|owned_by| owned_by != uid);
    }
    let cert = find_generated_cert(tls_dir.as_ref(), Reader::Bounded(cert_max))?;
    Ok(GeneratedInspection {
        pair: judge_generated_pair(cert, key),
        foreign_owner,
    })
}

/// The owner of the file `meta` describes, where the platform has one.
#[allow(clippy::unnecessary_wraps, reason = "there is no owner off unix")]
fn file_owner(meta: &fs::Metadata) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(meta.uid())
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

/// Why a generated directory owned by `owner` is refused to a start as
/// `euid`, or `None` when it is that start's own.
const fn foreign_owner_reason(owner: u32, euid: u32) -> Option<UnsafeReason> {
    if owner == euid {
        None
    } else {
        Some(UnsafeReason::ForeignOwner { owner, euid })
    }
}

/// Why a generated key with `links` hard links, owned by uid `owner`, may be
/// readable through something other than `tls-key/key.pem` when trawld runs
/// as `euid`, or `None` when it is trawld's alone.
///
/// A second hard link, for one inside the `tls/` directory the sidecar
/// mounts, exposes the same inode under that name; a key another uid owns
/// was not written by this trawld. Either key is discarded rather than
/// refused: the pair is regenerated, so whatever still holds the old key
/// holds one whose certificate is no longer served, with no operator step.
///
/// The permission bits are not checked: Kubernetes' fsGroup walk adds group
/// bits to the key, and `tls-key/` itself is `0700` and outside the mount.
#[cfg(unix)]
fn key_exposure(links: u64, owner: u32, euid: u32) -> Option<String> {
    if links != 1 {
        Some(format!(
            "it has {links} hard links; another name may expose it"
        ))
    } else if owner != euid {
        Some(format!(
            "it is owned by uid {owner}, not by trawld's uid {euid}"
        ))
    } else {
        None
    }
}

/// [`key_exposure`] of the key file whose metadata is `meta`, for a start
/// as `euid`. `None` for `euid` judges the hard links only.
#[cfg(unix)]
fn metadata_exposure(meta: &fs::Metadata, euid: Option<u32>) -> Option<String> {
    use std::os::unix::fs::MetadataExt;

    key_exposure(meta.nlink(), meta.uid(), euid.unwrap_or_else(|| meta.uid()))
}

#[cfg(not(unix))]
fn metadata_exposure(_meta: &fs::Metadata, _euid: Option<u32>) -> Option<String> {
    None
}

/// Why a symlink inside the generated TLS directories is refused.
const SYMLINK_REFUSAL: &str = "it is a symbolic link; trawld does not follow one in the \
                               directories it generates its certificate and key into";

/// One of trawld's generated TLS directories, created if missing, opened
/// once, and checked.
///
/// On unix the directory is opened with `O_NOFOLLOW | O_DIRECTORY` and must
/// be owned by trawld's effective uid. Anyone who can write the state
/// directory could otherwise plant `tls-key -> tls` and have the private key
/// written into the directory the Helm sidecar mounts, or have `tls/` re-moded
/// `0700`. Every file operation after the check is relative to the open
/// handle (`openat`, `unlinkat`, `renameat`), so replacing the path with a
/// symlink after the check redirects nothing. A symlink at a file name inside
/// the directory is refused on open (`O_NOFOLLOW`) and replaced, never
/// followed, by unlink and rename.
#[derive(Debug)]
struct GeneratedDir {
    path: PathBuf,
    #[cfg(unix)]
    handle: fs::File,
    /// The directory's owner, read from the open handle.
    #[cfg(unix)]
    owner: u32,
}

impl GeneratedDir {
    /// Create `path` if missing and open it. `mode`, when given, is set
    /// through the checked handle, so neither the umask nor a directory left
    /// by an earlier start decides it.
    fn open(path: &Path, mode: Option<u32>) -> Result<Self, TlsError> {
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(mode);
        }
        match builder.create(path) {
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => {
                return Err(TlsError::Write(e));
            }
            _ => {}
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let dir = Self::open_existing(path)?;
            let euid = rustix::process::geteuid().as_raw();
            if let Some(reason) = foreign_owner_reason(dir.owner, euid) {
                return Err(unsafe_path(path, reason));
            }
            if let Some(mode) = mode {
                dir.handle
                    .set_permissions(fs::Permissions::from_mode(mode))
                    .map_err(TlsError::Write)?;
            }
            Ok(dir)
        }
        #[cfg(not(unix))]
        {
            let _ = mode;
            Self::open_existing(path)
        }
    }

    /// Open the directory at `path` as it is, creating and changing nothing.
    /// A symlink or anything but a directory is refused. Its owner is read
    /// from the open handle and not judged here. A missing directory is
    /// [`TlsError::Write`] with [`std::io::ErrorKind::NotFound`].
    fn open_existing(path: &Path) -> Result<Self, TlsError> {
        #[cfg(unix)]
        {
            use rustix::fs::{Mode, OFlags};
            use rustix::io::Errno;
            use std::os::unix::fs::MetadataExt;

            let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
            let handle = match rustix::fs::open(path, flags, Mode::empty()) {
                Ok(fd) => fs::File::from(fd),
                // Linux answers a symlink with ENOTDIR here, not ELOOP: the
                // open has already refused it, and the lstat only names why.
                Err(Errno::LOOP | Errno::NOTDIR) => {
                    let reason = if fs::symlink_metadata(path).is_ok_and(|m| m.is_symlink()) {
                        UnsafeReason::Symlink
                    } else {
                        UnsafeReason::NotDirectory
                    };
                    return Err(unsafe_path(path, reason));
                }
                Err(e) => return Err(TlsError::Write(e.into())),
            };
            let owner = handle.metadata().map_err(TlsError::Write)?.uid();
            Ok(Self {
                path: path.to_owned(),
                handle,
                owner,
            })
        }
        #[cfg(not(unix))]
        {
            if let Err(e) = fs::metadata(path) {
                return Err(TlsError::Write(e));
            }
            Ok(Self {
                path: path.to_owned(),
            })
        }
    }

    /// The directory's owner, where the platform has one.
    #[allow(clippy::unnecessary_wraps, reason = "there is no owner off unix")]
    fn owner(&self) -> Option<u32> {
        #[cfg(unix)]
        {
            Some(self.owner)
        }
        #[cfg(not(unix))]
        {
            None
        }
    }

    /// Read the file `name` with `reader`, and the metadata of the file
    /// read, or `None` when there is none. A symlink or anything but a
    /// regular file is refused; any other failure is reported through
    /// `err`.
    fn read(
        &self,
        name: &str,
        reader: Reader,
        err: impl FnOnce(PathBuf, std::io::Error) -> TlsError,
    ) -> Result<Option<(Vec<u8>, fs::Metadata)>, TlsError> {
        let path = self.path.join(name);
        #[cfg(unix)]
        {
            use rustix::fs::{Mode, OFlags};
            use rustix::io::Errno;
            use std::io::Read;

            // O_NONBLOCK keeps a FIFO at `name` from blocking the open before
            // the file type below can refuse it; a regular file ignores it.
            let mut flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
            if let Reader::Bounded(_) = reader {
                // A terminal at `name` does not become the doctor's.
                flags |= OFlags::NOCTTY;
            }
            let mut file = match rustix::fs::openat(&self.handle, name, flags, Mode::empty()) {
                Ok(fd) => fs::File::from(fd),
                Err(Errno::NOENT) => return Ok(None),
                Err(Errno::LOOP) => return Err(unsafe_path(&path, UnsafeReason::Symlink)),
                Err(e) => return Err(err(path, e.into())),
            };
            // fstat of the open handle: the metadata is the file's that is read.
            let meta = match file.metadata() {
                Ok(meta) => meta,
                Err(e) => return Err(err(path, e)),
            };
            if !meta.file_type().is_file() {
                return Err(unsafe_path(&path, UnsafeReason::NotRegular));
            }
            let bytes = match reader {
                Reader::Start => {
                    let mut bytes = Vec::new();
                    file.read_to_end(&mut bytes).map(|_| bytes)
                }
                Reader::Bounded(max) => read_at_most(&file, &meta, max),
            };
            match bytes {
                Ok(bytes) => Ok(Some((bytes, meta))),
                Err(e) => Err(err(path, e)),
            }
        }
        #[cfg(not(unix))]
        {
            use std::io::Read;

            let mut file = match fs::File::open(&path) {
                Ok(file) => file,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(err(path, e)),
            };
            let meta = match file.metadata() {
                Ok(meta) => meta,
                Err(e) => return Err(err(path, e)),
            };
            if !meta.file_type().is_file() {
                return Err(unsafe_path(&path, UnsafeReason::NotRegular));
            }
            let bytes = match reader {
                Reader::Start => {
                    let mut bytes = Vec::new();
                    file.read_to_end(&mut bytes).map(|_| bytes)
                }
                Reader::Bounded(max) => read_at_most(&file, &meta, max),
            };
            match bytes {
                Ok(bytes) => Ok(Some((bytes, meta))),
                Err(e) => Err(err(path, e)),
            }
        }
    }

    /// [`key_exposure`] of the file `name`, judged from its directory entry
    /// without opening it, so a file trawld cannot read is judged too, for a
    /// start as `euid` (`None`: the hard links only). `None` when there is
    /// no file or nothing exposes it. A symlink or anything but a regular
    /// file is refused, as [`read`](Self::read) refuses it; any other
    /// failure is reported through `err`.
    fn entry_exposure(
        &self,
        name: &str,
        euid: Option<u32>,
        err: impl FnOnce(PathBuf, std::io::Error) -> TlsError,
    ) -> Result<Option<String>, TlsError> {
        #[cfg(unix)]
        {
            use rustix::fs::{AtFlags, FileType};
            use rustix::io::Errno;

            let path = self.path.join(name);
            let stat = match rustix::fs::statat(&self.handle, name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) => stat,
                Err(Errno::NOENT) => return Ok(None),
                Err(e) => return Err(err(path, e.into())),
            };
            match FileType::from_raw_mode(stat.st_mode) {
                FileType::RegularFile => {}
                FileType::Symlink => return Err(unsafe_path(&path, UnsafeReason::Symlink)),
                _ => return Err(unsafe_path(&path, UnsafeReason::NotRegular)),
            }
            #[allow(
                clippy::useless_conversion,
                reason = "the link count is narrower than u64 on some targets"
            )]
            let links = u64::from(stat.st_nlink);
            Ok(key_exposure(
                links,
                stat.st_uid,
                euid.unwrap_or(stat.st_uid),
            ))
        }
        #[cfg(not(unix))]
        {
            let _ = (name, euid, err);
            Ok(None)
        }
    }

    /// Remove the file `name`; returns whether there was one. A symlink is
    /// removed itself, not its target.
    fn remove(&self, name: &str) -> std::io::Result<bool> {
        #[cfg(unix)]
        let removed = rustix::fs::unlinkat(&self.handle, name, rustix::fs::AtFlags::empty())
            .map_err(std::io::Error::from);
        #[cfg(not(unix))]
        let removed = fs::remove_file(self.path.join(name));
        match removed {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Create the file `name`, which must not exist, at `mode`. The file is
    /// created with no permissions and then set to `mode` through its handle,
    /// so the umask never decides the mode and no wider mode ever exists.
    fn create_new(&self, name: &str, mode: u32) -> std::io::Result<fs::File> {
        #[cfg(unix)]
        {
            use rustix::fs::{Mode, OFlags};
            use std::os::unix::fs::PermissionsExt;

            let flags =
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
            let file = fs::File::from(rustix::fs::openat(
                &self.handle,
                name,
                flags,
                Mode::empty(),
            )?);
            file.set_permissions(fs::Permissions::from_mode(mode))?;
            Ok(file)
        }
        #[cfg(not(unix))]
        {
            let _ = mode;
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(self.path.join(name))
        }
    }

    /// Rename `from` to `to` within the directory. An existing `to`, symlink
    /// or not, is replaced rather than followed.
    fn rename(&self, from: &str, to: &str) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            rustix::fs::renameat(&self.handle, from, &self.handle, to)?;
            Ok(())
        }
        #[cfg(not(unix))]
        fs::rename(self.path.join(from), self.path.join(to))
    }

    /// Make earlier removals and renames in the directory durable.
    fn sync(&self) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            self.handle.sync_all()
        }
        #[cfg(not(unix))]
        Ok(())
    }
}

fn unsafe_path(path: &Path, reason: UnsafeReason) -> TlsError {
    TlsError::Unsafe {
        path: path.to_owned(),
        reason,
    }
}

/// Read `file`, whose metadata is `meta`, to its end, or fail with
/// [`std::io::ErrorKind::FileTooLarge`] once it holds more than `max`
/// bytes: past the size `fstat` gave, or past `max` in what is read.
fn read_at_most(file: &fs::File, meta: &fs::Metadata, max: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read;

    if meta.len() > max {
        return Err(std::io::ErrorKind::FileTooLarge.into());
    }
    let mut bytes = Vec::new();
    file.take(max.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(std::io::ErrorKind::FileTooLarge.into());
    }
    Ok(bytes)
}

/// Publish `pem` as `cert.pem` in `tls_dir` so that a reader sees either no
/// file or the whole certificate, never a partial one.
///
/// Writes a sibling `cert.pem.tmp`, syncs it, then renames it over
/// `cert.pem` (rename within one directory is atomic). The temporary file is
/// created exclusively and set to `0644` explicitly, so neither a leftover
/// file from a crashed start nor a restrictive umask decides who can read
/// the published certificate. The `tempfile` crate is not used because it
/// creates files `0600`.
fn publish_cert(tls_dir: &GeneratedDir, pem: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let tmp_name = format!("{GENERATED_CERT_FILE}.tmp");

    // A crash between create and rename leaves the temporary file behind.
    tls_dir.remove(&tmp_name)?;

    let mut f = tls_dir.create_new(&tmp_name, 0o644)?;
    f.write_all(pem)?;
    f.sync_all()?;
    drop(f);
    tls_dir.rename(&tmp_name, GENERATED_CERT_FILE)
}

/// Background task that polls cert/key files for changes and sends a new
/// [`TlsAcceptor`] through the watch channel when they change.
///
/// Uses content-based comparison instead of mtime to avoid TOCTOU races
/// between the change check and file read.
///
/// Runs until the watch receiver is dropped (i.e. the server shuts down).
pub async fn cert_reload_task(
    cert_path: PathBuf,
    key_path: PathBuf,
    interval: Duration,
    tx: tokio::sync::watch::Sender<TlsAcceptor>,
) {
    // Seed with the current file contents so we only reload on actual changes.
    let mut last_cert = fs::read(&cert_path).ok();
    let mut last_key = fs::read(&key_path).ok();

    loop {
        tokio::time::sleep(interval).await;

        let current_cert = match fs::read(&cert_path) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::debug!(error = %e, "could not read cert file, skipping reload");
                continue;
            }
        };
        let current_key = match fs::read(&key_path) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::debug!(error = %e, "could not read key file, skipping reload");
                continue;
            }
        };

        if last_cert.as_deref() == Some(&current_cert) && last_key.as_deref() == Some(&current_key)
        {
            continue;
        }

        tracing::info!(
            event_type = "tls_reload",
            "TLS certificate files changed, reloading"
        );

        // state_dir is unused when both paths are Some, but required by the signature.
        match build_server_config(Some(&cert_path), Some(&key_path), Path::new("")) {
            Ok((config, _)) => {
                let acceptor = TlsAcceptor::from(config);
                if tx.send(acceptor).is_err() {
                    // Receiver dropped — server is shutting down.
                    break;
                }
                last_cert = Some(current_cert);
                last_key = Some(current_key);
                tracing::info!(
                    event_type = "tls_reload",
                    "TLS certificate reloaded successfully"
                );
            }
            Err(e) => {
                tracing::error!(event_type = "tls_reload_error", error = %e, "failed to reload TLS certificate, keeping current");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_self_signed_produces_valid_pem() {
        let tmp = tempfile::tempdir().unwrap();
        let cert_path = tmp.path().join("cert.pem");
        let key_path = tmp.path().join("key.pem");

        // A real PEM pair on disk: this test drives the operator-supplied
        // path branch, which loads the files instead of generating any.
        let san = vec!["localhost".to_owned(), "127.0.0.1".to_owned()];
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(san).unwrap();

        let cert_pem = cert.pem();
        let key_pem = signing_key.serialize_pem();

        fs::write(&cert_path, &cert_pem).unwrap();
        fs::write(&key_path, &key_pem).unwrap();

        // Verify we can load them back and build a rustls config.
        let (config, self_signed) =
            build_server_config(Some(&cert_path), Some(&key_path), tmp.path()).unwrap();
        assert!(!self_signed);
        assert_eq!(
            config.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );
    }

    #[test]
    fn rejects_mismatched_cert_key_config() {
        let tmp = tempfile::tempdir().unwrap();
        let cert_only = tmp.path().join("cert.pem");

        let result = build_server_config(Some(&cert_only), None, tmp.path());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("both"));
    }

    #[test]
    fn rejects_missing_cert_file() {
        let result = build_server_config(
            Some(Path::new("/nonexistent/cert.pem")),
            Some(Path::new("/nonexistent/key.pem")),
            Path::new("/tmp"),
        );
        assert!(result.is_err());
    }

    #[test]
    fn auto_generates_certs_in_state_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let (_, self_signed) = build_server_config(None, None, tmp.path()).unwrap();
        assert!(self_signed);

        // Verify the cert was written to {state_dir}/tls/ and the key to
        // {state_dir}/tls-key/.
        assert!(tmp.path().join("tls").join("cert.pem").exists());
        assert!(tmp.path().join("tls-key").join("key.pem").exists());
    }

    /// The generated certificate is published for a proxy running as another
    /// user: world-readable and complete, while the key stays owner-only. A
    /// temporary file left by a crashed start (here `0600` and garbage) must
    /// not leak its mode or bytes into the published certificate.
    #[cfg(unix)]
    #[test]
    fn generated_cert_is_world_readable_and_key_is_owner_only() {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let tmp = tempfile::tempdir().unwrap();
        let tls_dir = tmp.path().join(GENERATED_TLS_DIR);
        fs::create_dir_all(&tls_dir).unwrap();
        let leftover = tls_dir.join(format!("{GENERATED_CERT_FILE}.tmp"));
        {
            use std::io::Write;
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&leftover)
                .unwrap();
            f.write_all(b"half a certificate").unwrap();
        }

        let (_, self_signed) = build_server_config(None, None, tmp.path()).unwrap();
        assert!(self_signed);

        let cert_path = tls_dir.join(GENERATED_CERT_FILE);
        let key_dir = tmp.path().join(GENERATED_KEY_DIR);
        let key_path = key_dir.join(GENERATED_KEY_FILE);
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&cert_path), 0o644, "cert.pem is world-readable");
        assert_eq!(mode(&key_dir), 0o700, "the key directory is owner-only");
        assert_eq!(mode(&key_path), 0o600, "key.pem is owner-only");
        assert!(!leftover.exists(), "the temporary file was renamed away");
        assert_eq!(
            dir_entries(&tls_dir),
            [GENERATED_CERT_FILE],
            "a proxy mounting tls/ sees the certificate and nothing else"
        );

        let certs = CertificateDer::pem_slice_iter(&fs::read(&cert_path).unwrap())
            .collect::<Result<Vec<_>, _>>()
            .expect("cert.pem is PEM");
        assert_eq!(certs.len(), 1, "cert.pem holds one certificate");

        // The pair on disk is what a restart loads, so it must still serve.
        let (_, self_signed) = build_server_config(None, None, tmp.path()).unwrap();
        assert!(!self_signed, "the restart loads the published pair");
    }

    /// A proxy running as another uid pins `tls/cert.pem`, so it must be
    /// able to search `tls/`, and only trawld may write it. Whatever mode an
    /// earlier start, the umask, or a group ownership walk left, trawld sets
    /// the directory to `0755`.
    #[cfg(unix)]
    #[test]
    fn the_certificate_directory_is_0755_whatever_it_was() {
        use std::os::unix::fs::PermissionsExt;

        for before in [None, Some(0o700), Some(0o775), Some(0o777), Some(0o2775)] {
            let tmp = tempfile::tempdir().unwrap();
            let tls_dir = tmp.path().join(GENERATED_TLS_DIR);
            if let Some(mode) = before {
                fs::create_dir(&tls_dir).unwrap();
                fs::set_permissions(&tls_dir, fs::Permissions::from_mode(mode)).unwrap();
            }
            build_server_config(None, None, tmp.path()).unwrap();
            let mode = fs::metadata(&tls_dir).unwrap().permissions().mode() & 0o7777;
            let before = before.map_or_else(|| "absent".to_owned(), |m| format!("{m:o}"));
            assert_eq!(mode, 0o755, "tls/ was {before}");
        }
    }

    /// A certificate whose key is gone is regenerated, and a generation
    /// interrupted after the new key is written must not leave the old
    /// certificate beside it: the next start would load a pair that does not
    /// match and fail TLS on every start after. Publication is made to fail
    /// by a directory squatting on `cert.pem.tmp`.
    #[test]
    fn an_interrupted_generation_never_pairs_a_new_key_with_the_old_cert() {
        let tmp = tempfile::tempdir().unwrap();
        let tls_dir = tmp.path().join(GENERATED_TLS_DIR);
        fs::create_dir_all(&tls_dir).unwrap();
        let cert_path = tls_dir.join(GENERATED_CERT_FILE);
        let key_path = tmp.path().join(GENERATED_KEY_DIR).join(GENERATED_KEY_FILE);

        // Only a certificate from some other keypair survives.
        let rcgen::CertifiedKey { cert: stale, .. } =
            rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
        fs::write(&cert_path, stale.pem()).unwrap();

        let squatter = tls_dir.join(format!("{GENERATED_CERT_FILE}.tmp"));
        fs::create_dir(&squatter).unwrap();
        build_server_config(None, None, tmp.path())
            .expect_err("publication fails while cert.pem.tmp is a directory");
        assert!(
            key_path.exists(),
            "the interruption came after the key write"
        );
        assert!(
            !cert_path.exists(),
            "the stale certificate is gone before a new key is written"
        );

        fs::remove_dir(&squatter).unwrap();
        // rustls refuses a key whose public half is not the certificate's.
        let (_, self_signed) = build_server_config(None, None, tmp.path())
            .expect("the next start serves a matching pair");
        assert!(self_signed, "the next start regenerates");
        let (_, self_signed) =
            build_server_config(None, None, tmp.path()).expect("the regenerated pair loads");
        assert!(!self_signed, "the start after loads the published pair");
    }

    /// An older trawld kept its key at `tls/key.pem`, inside the directory a
    /// proxy mounts to pin the certificate. Any start that owns the
    /// generated pair deletes it: an old-layout pair has no key in the key
    /// directory, so it is regenerated, and a stray legacy key beside a
    /// current pair is removed without regenerating.
    #[test]
    fn a_legacy_key_in_the_certificate_directory_is_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let tls_dir = tmp.path().join(GENERATED_TLS_DIR);
        fs::create_dir_all(&tls_dir).unwrap();
        let legacy_key = tls_dir.join(GENERATED_KEY_FILE);

        // The old layout: a matching pair, both inside tls/.
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
        fs::write(tls_dir.join(GENERATED_CERT_FILE), cert.pem()).unwrap();
        fs::write(&legacy_key, signing_key.serialize_pem()).unwrap();

        let (_, self_signed) = build_server_config(None, None, tmp.path())
            .expect("an old-layout pair gives way to a new one");
        assert!(self_signed, "the old-layout pair is regenerated");
        assert!(!legacy_key.exists(), "the legacy key is gone");
        assert_eq!(dir_entries(&tls_dir), [GENERATED_CERT_FILE]);

        // A legacy key beside a current pair.
        fs::write(&legacy_key, signing_key.serialize_pem()).unwrap();
        let (_, self_signed) =
            build_server_config(None, None, tmp.path()).expect("the current pair loads");
        assert!(!self_signed, "the current pair is kept");
        assert!(!legacy_key.exists(), "the stray legacy key is gone");
    }

    /// A `tls-key` symlink planted at the key directory must not steer the
    /// private key anywhere: pointed at `tls/`, it would put the key in the
    /// directory the proxy sidecar mounts. Generation refuses the link,
    /// naming it, and writes nothing into its target.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_key_directory_is_refused() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let tmp = tempfile::tempdir().unwrap();
        let tls_dir = tmp.path().join(GENERATED_TLS_DIR);
        fs::create_dir_all(&tls_dir).unwrap();
        fs::set_permissions(&tls_dir, fs::Permissions::from_mode(0o755)).unwrap();
        let key_dir = tmp.path().join(GENERATED_KEY_DIR);
        symlink(&tls_dir, &key_dir).unwrap();

        let err = build_server_config(None, None, tmp.path())
            .expect_err("a symlinked key directory is refused");
        assert!(
            matches!(&err, TlsError::Unsafe { path, .. } if *path == key_dir),
            "the refusal names the key directory: {err}"
        );
        assert!(err.to_string().contains("symbolic link"), "{err}");
        assert!(dir_entries(&tls_dir).is_empty(), "nothing lands in tls/");
        let mode = fs::metadata(&tls_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "the link target is not re-moded");
    }

    /// The same holds for the certificate directory: a `tls` symlink is
    /// refused rather than followed, so no certificate or key is written
    /// into the directory it points at.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_certificate_directory_is_refused() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let elsewhere = tmp.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        let tls_dir = tmp.path().join(GENERATED_TLS_DIR);
        symlink(&elsewhere, &tls_dir).unwrap();

        let err = build_server_config(None, None, tmp.path())
            .expect_err("a symlinked certificate directory is refused");
        assert!(
            matches!(&err, TlsError::Unsafe { path, .. } if *path == tls_dir),
            "the refusal names the certificate directory: {err}"
        );
        assert!(err.to_string().contains("symbolic link"), "{err}");
        assert!(
            dir_entries(&elsewhere).is_empty(),
            "nothing lands in the target"
        );
    }

    /// A key directory that is not a directory at all is refused too.
    #[test]
    fn a_key_directory_that_is_a_file_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let key_dir = tmp.path().join(GENERATED_KEY_DIR);
        fs::write(&key_dir, b"not a directory").unwrap();

        let err = build_server_config(None, None, tmp.path())
            .expect_err("a key directory that is a file is refused");
        assert!(
            matches!(&err, TlsError::Unsafe { path, .. } if *path == key_dir),
            "the refusal names the key directory: {err}"
        );
        assert!(err.to_string().contains("not a directory"), "{err}");
    }

    /// A symlink at `tls-key/key.pem` or `tls/cert.pem` is refused whether
    /// or not a pair is already published, and its target is neither read
    /// as the key nor replaced.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_key_or_certificate_file_is_refused() {
        use std::os::unix::fs::symlink;

        for with_cert in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let tls_dir = tmp.path().join(GENERATED_TLS_DIR);
            let key_dir = tmp.path().join(GENERATED_KEY_DIR);
            fs::create_dir_all(&tls_dir).unwrap();
            fs::create_dir_all(&key_dir).unwrap();
            if with_cert {
                let rcgen::CertifiedKey { cert, .. } =
                    rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
                fs::write(tls_dir.join(GENERATED_CERT_FILE), cert.pem()).unwrap();
            }
            let victim = tmp.path().join("victim");
            fs::write(&victim, b"not yours").unwrap();
            let key_path = key_dir.join(GENERATED_KEY_FILE);
            symlink(&victim, &key_path).unwrap();

            let err = build_server_config(None, None, tmp.path())
                .expect_err("a symlinked key.pem is refused");
            assert!(
                matches!(&err, TlsError::Unsafe { path, .. } if *path == key_path),
                "the refusal names key.pem (with_cert={with_cert}): {err}"
            );
            assert_eq!(fs::read(&victim).unwrap(), b"not yours");
            assert!(
                fs::symlink_metadata(&key_path).unwrap().is_symlink(),
                "the link is left for the operator to inspect"
            );
        }

        let tmp = tempfile::tempdir().unwrap();
        let tls_dir = tmp.path().join(GENERATED_TLS_DIR);
        fs::create_dir_all(&tls_dir).unwrap();
        let victim = tmp.path().join("victim");
        fs::write(&victim, b"not yours").unwrap();
        let cert_path = tls_dir.join(GENERATED_CERT_FILE);
        symlink(&victim, &cert_path).unwrap();
        let err = build_server_config(None, None, tmp.path())
            .expect_err("a symlinked cert.pem is refused");
        assert!(
            matches!(&err, TlsError::Unsafe { path, .. } if *path == cert_path),
            "the refusal names cert.pem: {err}"
        );
        assert_eq!(fs::read(&victim).unwrap(), b"not yours");
    }

    /// A second link to the generated key, here one inside the `tls/`
    /// directory the proxy sidecar mounts, exposes the key through that
    /// name. The next start discards the key and generates a new pair, so
    /// the exposed inode holds only a key whose certificate is no longer
    /// served, and the start after loads the new pair.
    ///
    /// A key owned by another uid is discarded the same way. Planting one
    /// inside trawld's own `0700` key directory needs root, so
    /// [`key_exposure_names_extra_links_and_a_foreign_owner`] covers the
    /// owner rule and [`an_unreadable_generated_key_is_judged_before_it_is_opened`]
    /// the check that applies it before any open.
    #[cfg(unix)]
    #[test]
    fn a_generated_key_with_extra_links_is_regenerated() {
        use std::os::unix::fs::MetadataExt;

        let tmp = tempfile::tempdir().unwrap();
        build_server_config(None, None, tmp.path()).unwrap();
        let tls_dir = tmp.path().join(GENERATED_TLS_DIR);
        let key_path = tmp.path().join(GENERATED_KEY_DIR).join(GENERATED_KEY_FILE);
        let cert_path = tls_dir.join(GENERATED_CERT_FILE);
        let old_key = fs::read(&key_path).unwrap();
        let old_cert = fs::read(&cert_path).unwrap();
        let exposed = tls_dir.join("exposed.pem");
        fs::hard_link(&key_path, &exposed).unwrap();

        let (_, self_signed) = build_server_config(None, None, tmp.path())
            .expect("a linked key gives way to a new pair");
        assert!(self_signed, "the linked key is not loaded");
        let new_key = fs::read(&key_path).unwrap();
        assert_ne!(new_key, old_key, "the key was regenerated");
        assert_ne!(fs::read(&cert_path).unwrap(), old_cert, "so was the cert");
        assert_eq!(fs::metadata(&key_path).unwrap().nlink(), 1);
        for name in dir_entries(&tls_dir) {
            assert_ne!(
                fs::read(tls_dir.join(&name)).unwrap(),
                new_key,
                "tls/{name} does not hold the served key"
            );
        }

        let (_, self_signed) =
            build_server_config(None, None, tmp.path()).expect("the new pair loads");
        assert!(!self_signed, "the start after loads the new pair");
    }

    /// Which keys are trawld's alone: one link, owned by trawld's uid. A
    /// second link or another owner is a reason to regenerate, and the
    /// reason names what was found.
    #[cfg(unix)]
    #[test]
    fn key_exposure_names_extra_links_and_a_foreign_owner() {
        assert_eq!(key_exposure(1, 1000, 1000), None);
        let links = key_exposure(2, 1000, 1000).expect("a second link exposes the key");
        assert!(links.contains("2 hard links"), "{links}");
        let owner = key_exposure(1, 0, 1000).expect("another owner exposes the key");
        assert!(
            owner.contains("uid 0") && owner.contains("uid 1000"),
            "{owner}"
        );
    }

    /// A key trawld cannot read because another uid owns it (root `0600`
    /// after a restore) must be judged by its owner before any open: the
    /// open would fail with `EACCES` and stop the start. Staging another
    /// owner needs root, so this stages the same unreadable file with an
    /// extra link, which the same directory-entry check routes to
    /// regeneration. A self-owned key that is unreadable and trawld's alone
    /// is a start error naming the key.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_generated_key_is_judged_before_it_is_opened() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let tmp = tempfile::tempdir().unwrap();
        build_server_config(None, None, tmp.path()).unwrap();
        let key_path = tmp.path().join(GENERATED_KEY_DIR).join(GENERATED_KEY_FILE);
        let old_key = fs::read(&key_path).unwrap();
        fs::set_permissions(&key_path, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&key_path).is_ok() {
            // Root reads past the mode; there is nothing unreadable to stage.
            return;
        }

        // trawld's alone, but unreadable: refused, naming the key.
        let err = build_server_config(None, None, tmp.path())
            .expect_err("an unreadable key trawld owns stops the start");
        assert!(
            matches!(&err, TlsError::ReadKey { path, source }
                if *path == key_path && source.kind() == std::io::ErrorKind::PermissionDenied),
            "{err:?}"
        );

        // Exposed and unreadable: regenerated without being opened.
        let exposed = tmp.path().join("exposed.pem");
        fs::hard_link(&key_path, &exposed).unwrap();
        let (_, self_signed) = build_server_config(None, None, tmp.path())
            .expect("an exposed key gives way to a new pair even unread");
        assert!(self_signed, "the exposed key is not loaded");
        assert_ne!(
            fs::read(&key_path).unwrap(),
            old_key,
            "the key was regenerated"
        );
        assert_eq!(fs::metadata(&key_path).unwrap().nlink(), 1);
        let (_, self_signed) =
            build_server_config(None, None, tmp.path()).expect("the new pair loads");
        assert!(!self_signed, "the start after loads the new pair");
    }

    /// Something at `key.pem` that is not a regular file is not a key trawld
    /// wrote. It is refused by name, and a FIFO there does not hang the
    /// start on open.
    #[cfg(unix)]
    #[test]
    fn a_key_that_is_not_a_regular_file_is_refused() {
        use rustix::fs::{CWD, FileType, Mode};

        let tmp = tempfile::tempdir().unwrap();
        let key_dir = tmp.path().join(GENERATED_KEY_DIR);
        fs::create_dir_all(&key_dir).unwrap();
        let key_path = key_dir.join(GENERATED_KEY_FILE);
        rustix::fs::mknodat(CWD, &key_path, FileType::Fifo, Mode::RUSR | Mode::WUSR, 0).unwrap();

        let err =
            build_server_config(None, None, tmp.path()).expect_err("a FIFO at key.pem is refused");
        assert!(
            matches!(&err, TlsError::Unsafe { path, .. } if *path == key_path),
            "the refusal names key.pem: {err}"
        );
        assert!(err.to_string().contains("not a regular file"), "{err}");
    }

    /// Every entry under `dir` with its mode, size and modification time,
    /// sorted; empty when `dir` does not exist.
    #[cfg(unix)]
    fn tree(dir: &Path) -> Vec<(PathBuf, u32, u64, std::time::SystemTime)> {
        use std::os::unix::fs::MetadataExt;

        let mut entries = Vec::new();
        let mut pending = vec![dir.to_owned()];
        while let Some(at) = pending.pop() {
            let Ok(meta) = fs::symlink_metadata(&at) else {
                continue;
            };
            if meta.is_dir() {
                pending.extend(fs::read_dir(&at).unwrap().map(|e| e.unwrap().path()));
            }
            entries.push((at, meta.mode(), meta.len(), meta.modified().unwrap()));
        }
        entries.sort();
        entries
    }

    /// The doctor's read of the generated pair agrees with what a start
    /// does with it, state by state, and changes nothing: no directory is
    /// created or re-moded, and no file is removed or written.
    #[cfg(unix)]
    #[test]
    fn inspecting_the_generated_pair_agrees_with_boot_and_writes_nothing() {
        use std::os::unix::fs::PermissionsExt;

        let euid = rustix::process::geteuid().as_raw();
        let inspect = |state: &Path| {
            let before = tree(state);
            let found =
                inspect_generated_pair(state, OwnerRule::Enforced(euid), u64::MAX, u64::MAX);
            assert_eq!(
                tree(state),
                before,
                "the inspection changed the state directory"
            );
            found
        };
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let cert_path = state.join(GENERATED_TLS_DIR).join(GENERATED_CERT_FILE);
        let key_path = state.join(GENERATED_KEY_DIR).join(GENERATED_KEY_FILE);

        // Before the first start there is nothing, and nothing is created.
        let found = inspect(&state).unwrap();
        assert_eq!(found.pair, GeneratedPair::Regenerate(Regenerate::Absent));
        assert!(!state.exists());

        // After it, the inspection finds exactly the pair boot generated,
        // and leaves a mode boot would reset as it is.
        assert!(build_server_config(None, None, &state).unwrap().1);
        fs::set_permissions(
            state.join(GENERATED_TLS_DIR),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let found = inspect(&state).unwrap();
        assert_eq!(
            found.pair,
            GeneratedPair::Found {
                cert_pem: fs::read(&cert_path).unwrap(),
                key_pem: fs::read(&key_path).unwrap(),
            }
        );
        assert!(!found.foreign_owner);
        assert!(
            !build_server_config(None, None, &state).unwrap().1,
            "boot loads it"
        );

        // Each half missing: boot generates a new pair.
        fs::remove_file(&cert_path).unwrap();
        let found = inspect(&state).unwrap();
        assert_eq!(
            found.pair,
            GeneratedPair::Regenerate(Regenerate::NoCertificate)
        );
        assert!(build_server_config(None, None, &state).unwrap().1);
        fs::remove_file(&key_path).unwrap();
        let found = inspect(&state).unwrap();
        assert_eq!(found.pair, GeneratedPair::Regenerate(Regenerate::NoKey));
        assert!(build_server_config(None, None, &state).unwrap().1);

        // An exposed key: boot discards it and generates a new pair.
        let exposed = tmp.path().join("exposed.pem");
        fs::hard_link(&key_path, &exposed).unwrap();
        let found = inspect(&state).unwrap();
        assert!(
            matches!(&found.pair, GeneratedPair::Regenerate(Regenerate::KeyExposed(why)) if why.contains("2 hard links")),
            "{found:?}"
        );
        assert!(build_server_config(None, None, &state).unwrap().1);

        // A symlinked key directory: boot refuses it, and so does the
        // inspection, naming the same reason.
        let key_dir = state.join(GENERATED_KEY_DIR);
        let moved = tmp.path().join("moved-key-dir");
        fs::rename(&key_dir, &moved).unwrap();
        std::os::unix::fs::symlink(&moved, &key_dir).unwrap();
        let inspected = inspect(&state).expect_err("a symlinked key directory is refused");
        let booted = build_server_config(None, None, &state).expect_err("boot refuses it too");
        for err in [inspected, booted] {
            assert!(
                matches!(&err, TlsError::Unsafe { path, reason: UnsafeReason::Symlink } if *path == key_dir),
                "{err}"
            );
        }
    }

    /// Another owner is refused as boot refuses it when the uid is
    /// enforced, and only recorded when it is observed, as a root run
    /// observes it. A key past its read cap stops the read.
    #[cfg(unix)]
    #[test]
    fn the_owner_rule_and_the_read_cap_hold() {
        let euid = rustix::process::geteuid().as_raw();
        let other = euid.wrapping_add(1);
        let tmp = tempfile::tempdir().unwrap();
        build_server_config(None, None, tmp.path()).unwrap();

        let err =
            inspect_generated_pair(tmp.path(), OwnerRule::Enforced(other), u64::MAX, u64::MAX)
                .expect_err("a directory another uid owns is refused");
        assert!(
            matches!(
                &err,
                TlsError::Unsafe { reason: UnsafeReason::ForeignOwner { owner, euid: enforced }, .. }
                    if *owner == euid && *enforced == other
            ),
            "{err}"
        );
        assert!(
            err.to_string().contains(&format!("owned by uid {euid}")),
            "{err}"
        );

        let observed =
            inspect_generated_pair(tmp.path(), OwnerRule::Observed(other), u64::MAX, u64::MAX)
                .expect("an observed owner is not refused");
        assert!(observed.foreign_owner);
        assert!(
            matches!(observed.pair, GeneratedPair::Found { .. }),
            "{observed:?}"
        );
        let own = inspect_generated_pair(tmp.path(), OwnerRule::Observed(euid), u64::MAX, u64::MAX)
            .unwrap();
        assert!(!own.foreign_owner);

        let err = inspect_generated_pair(tmp.path(), OwnerRule::Enforced(euid), u64::MAX, 8)
            .expect_err("a key past the cap is not read");
        assert!(
            matches!(&err, TlsError::ReadKey { source, .. } if source.kind() == std::io::ErrorKind::FileTooLarge),
            "{err:?}"
        );
    }

    #[test]
    fn the_source_is_both_paths_or_neither() {
        let (cert, key) = (Path::new("/tls/cert.pem"), Path::new("/tls/key.pem"));
        assert_eq!(
            TlsSource::of(Some(cert), Some(key)).unwrap(),
            TlsSource::Configured { cert, key }
        );
        assert_eq!(TlsSource::of(None, None).unwrap(), TlsSource::Generated);
        for (cert, key) in [(Some(cert), None), (None, Some(key))] {
            let err = TlsSource::of(cert, key).unwrap_err();
            assert!(err.to_string().contains("both"), "{err}");
        }
    }

    /// Names of the entries in `dir`, sorted.
    fn dir_entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// The Debian package's trawl-web pins the certificate this module
    /// generates, so the packaged `[web] upstream_ca_path` must be the file
    /// trawld writes for the packaged `[data] path`.
    #[test]
    fn the_debian_config_pins_the_generated_certificate() {
        let config = trawl_config::Config::parse_toml(include_str!("../debian/trawld.toml"))
            .expect("the packaged trawld.toml parses");
        assert!(
            config.server.tls_cert_path.is_none() && config.server.tls_key_path.is_none(),
            "the pin assumes trawld generates its own certificate"
        );
        let pin = config
            .web
            .upstream_ca_path
            .as_deref()
            .expect("the packaged trawld.toml sets [web] upstream_ca_path");
        let generated = config
            .generated_cert_path()
            .expect("trawld generates a certificate for the packaged config");
        assert_eq!(pin, generated, "the pin is trawld's generated certificate");
        let relative = generated
            .strip_prefix(config.state_dir())
            .expect("the generated certificate lies under trawld's state directory");

        // Generate into a scratch state directory and read the pin from it.
        let tmp = tempfile::tempdir().unwrap();
        build_server_config(None, None, tmp.path()).unwrap();
        let pem = fs::read(tmp.path().join(relative)).expect("trawld wrote the pinned file");
        let certs = CertificateDer::pem_slice_iter(&pem)
            .collect::<Result<Vec<_>, _>>()
            .expect("the pinned file is PEM");
        assert_eq!(certs.len(), 1, "the pinned file holds the certificate");
        assert_eq!(relative, Path::new("tls").join(GENERATED_CERT_FILE));
    }
}
