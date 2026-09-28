// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! TLS certificate loading and self-signed certificate generation.
//!
//! When no cert/key paths are configured, a self-signed certificate is
//! auto-generated to `{state_dir}/tls/` and persisted across restarts.
//! The state directory is typically the parent of the data directory
//! (e.g. `/var/lib/trawl/tls/` for the deb package).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rustls::ServerConfig;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::TlsAcceptor;
use trawl_config::{GENERATED_CERT_FILE, GENERATED_KEY_FILE, GENERATED_TLS_DIR};

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
}

/// Build a `rustls` [`ServerConfig`] from user-provided cert/key paths,
/// or auto-generate a self-signed certificate if neither is set.
///
/// `state_dir` is the base directory for auto-generated certs (used as
/// `{state_dir}/tls/`). Only consulted when both cert/key paths are `None`.
///
/// Returns `Err` if only one of cert/key is provided.
pub fn build_server_config(
    cert_path: Option<&Path>,
    key_path: Option<&Path>,
    state_dir: &Path,
) -> Result<(Arc<ServerConfig>, bool), TlsError> {
    let (cert_pem, key_pem, self_signed) = match (cert_path, key_path) {
        (Some(cert), Some(key)) => {
            let (c, k) = load_pem_files(cert, key)?;
            (c, k, false)
        }
        (None, None) => {
            let tls_dir = state_dir.join(GENERATED_TLS_DIR);
            let (c, k, generated) = load_or_generate_default(&tls_dir)?;
            (c, k, generated)
        }
        _ => {
            return Err(TlsError::Config(
                "both tls_cert_path and tls_key_path must be set, or neither".into(),
            ));
        }
    };

    log_cert_details(&cert_pem);

    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&cert_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| TlsError::InvalidCert(e.to_string()))?;

    let key =
        PrivateKeyDer::from_pem_slice(&key_pem).map_err(|e| TlsError::InvalidKey(e.to_string()))?;

    let mut config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|e| TlsError::Config(e.to_string()))?
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|e| TlsError::Config(e.to_string()))?;

    // Support HTTP/2 and HTTP/1.1 via ALPN negotiation.
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    Ok((Arc::new(config), self_signed))
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
/// only once complete. The key is written first and `cert.pem` last, so a
/// visible certificate always has its key beside it.
///
/// Returns `(cert_pem, key_pem, was_generated)`.
fn load_or_generate_default(tls_dir: &Path) -> Result<(Vec<u8>, Vec<u8>, bool), TlsError> {
    let cert_path = tls_dir.join(GENERATED_CERT_FILE);
    let key_path = tls_dir.join(GENERATED_KEY_FILE);

    if cert_path.exists() && key_path.exists() {
        tracing::info!(
            event_type = "lifecycle",
            cert = %cert_path.display(),
            key = %key_path.display(),
            "loading existing self-signed TLS certificate"
        );
        let (c, k) = load_pem_files(&cert_path, &key_path)?;
        return Ok((c, k, false));
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

    // Persist so the cert is stable across daemon restarts.
    fs::create_dir_all(tls_dir).map_err(TlsError::Write)?;

    // Write the private key with restricted permissions from the start
    // to avoid a TOCTOU window where the key is world-readable.
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&key_path)
            .map_err(TlsError::Write)?;
        f.write_all(key_pem.as_bytes()).map_err(TlsError::Write)?;
    }
    #[cfg(not(unix))]
    {
        fs::write(&key_path, &key_pem).map_err(TlsError::Write)?;
    }

    publish_cert(&cert_path, cert_pem.as_bytes()).map_err(TlsError::Write)?;

    tracing::info!(
        event_type = "lifecycle",
        cert = %cert_path.display(),
        key = %key_path.display(),
        "self-signed TLS certificate generated"
    );

    Ok((cert_pem.into_bytes(), key_pem.into_bytes(), true))
}

/// Publish `pem` at `cert_path` so that a reader sees either no file or the
/// whole certificate, never a partial one.
///
/// Writes a sibling `cert.pem.tmp`, syncs it, then renames it over
/// `cert_path` (rename within one directory is atomic). On unix the
/// temporary file is created exclusively and set to `0644` explicitly, so
/// neither a leftover file from a crashed start nor a restrictive umask
/// decides who can read the published certificate. The `tempfile` crate is
/// not used because it creates files `0600`.
fn publish_cert(cert_path: &Path, pem: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let mut tmp_name = cert_path.file_name().unwrap_or_default().to_owned();
    tmp_name.push(".tmp");
    let tmp_path = cert_path.with_file_name(tmp_name);

    // A crash between create and rename leaves the temporary file behind.
    match fs::remove_file(&tmp_path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }

    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o644);
    }
    let mut f = options.open(&tmp_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // `mode` above is filtered through the umask; this is not.
        f.set_permissions(fs::Permissions::from_mode(0o644))?;
    }
    f.write_all(pem)?;
    f.sync_all()?;
    drop(f);
    fs::rename(&tmp_path, cert_path)
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

        // Verify certs were written to {state_dir}/tls/.
        let tls_dir = tmp.path().join("tls");
        assert!(tls_dir.join("cert.pem").exists());
        assert!(tls_dir.join("key.pem").exists());
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
        let key_path = tls_dir.join(GENERATED_KEY_FILE);
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&cert_path), 0o644, "cert.pem is world-readable");
        assert_eq!(mode(&key_path), 0o600, "key.pem is owner-only");
        assert!(!leftover.exists(), "the temporary file was renamed away");

        let certs = CertificateDer::pem_slice_iter(&fs::read(&cert_path).unwrap())
            .collect::<Result<Vec<_>, _>>()
            .expect("cert.pem is PEM");
        assert_eq!(certs.len(), 1, "cert.pem holds one certificate");

        // The pair on disk is what a restart loads, so it must still serve.
        let (_, self_signed) = build_server_config(None, None, tmp.path()).unwrap();
        assert!(!self_signed, "the restart loads the published pair");
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
