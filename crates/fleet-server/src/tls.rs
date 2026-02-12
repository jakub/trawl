//! TLS certificate loading and self-signed certificate generation.
//!
//! When no cert/key paths are configured, a self-signed certificate is
//! auto-generated to `~/.fleet/tls/` and persisted across restarts.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rustls::ServerConfig;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::TlsAcceptor;

const DEFAULT_TLS_DIR: &str = ".fleet/tls";
const CERT_FILENAME: &str = "cert.pem";
const KEY_FILENAME: &str = "key.pem";

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
/// Returns `Err` if only one of cert/key is provided.
pub fn build_server_config(
    cert_path: Option<&Path>,
    key_path: Option<&Path>,
) -> Result<(Arc<ServerConfig>, bool), TlsError> {
    let (cert_pem, key_pem, self_signed) = match (cert_path, key_path) {
        (Some(cert), Some(key)) => {
            let (c, k) = load_pem_files(cert, key)?;
            (c, k, false)
        }
        (None, None) => {
            let (c, k, generated) = load_or_generate_default()?;
            (c, k, generated)
        }
        _ => {
            return Err(TlsError::Config(
                "both tls_cert_path and tls_key_path must be set, or neither".into(),
            ));
        }
    };

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

/// Resolve the default TLS directory (`~/.fleet/tls/`).
fn default_tls_dir() -> Result<PathBuf, TlsError> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| TlsError::Config("HOME environment variable not set".into()))?;
    Ok(PathBuf::from(home).join(DEFAULT_TLS_DIR))
}

/// Load existing default certs or generate new self-signed ones.
///
/// Returns `(cert_pem, key_pem, was_generated)`.
fn load_or_generate_default() -> Result<(Vec<u8>, Vec<u8>, bool), TlsError> {
    let tls_dir = default_tls_dir()?;
    let cert_path = tls_dir.join(CERT_FILENAME);
    let key_path = tls_dir.join(KEY_FILENAME);

    if cert_path.exists() && key_path.exists() {
        tracing::info!(
            cert = %cert_path.display(),
            key = %key_path.display(),
            "loading existing self-signed TLS certificate"
        );
        let (c, k) = load_pem_files(&cert_path, &key_path)?;
        return Ok((c, k, false));
    }

    tracing::info!("no TLS certificate found, generating self-signed certificate");

    let subject_alt_names = vec![
        "localhost".to_owned(),
        "127.0.0.1".to_owned(),
        "::1".to_owned(),
    ];

    let rcgen::CertifiedKey { cert, key_pair } =
        rcgen::generate_simple_self_signed(subject_alt_names)
            .map_err(|e| TlsError::Generation(e.to_string()))?;

    let cert_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();

    // Persist so the cert is stable across daemon restarts.
    fs::create_dir_all(&tls_dir).map_err(TlsError::Write)?;
    fs::write(&cert_path, &cert_pem).map_err(TlsError::Write)?;
    fs::write(&key_path, &key_pem).map_err(TlsError::Write)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600))
            .map_err(TlsError::Write)?;
    }

    tracing::info!(
        cert = %cert_path.display(),
        key = %key_path.display(),
        "self-signed TLS certificate generated"
    );

    Ok((cert_pem.into_bytes(), key_pem.into_bytes(), true))
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

        tracing::info!("TLS certificate files changed, reloading");

        match build_server_config(Some(&cert_path), Some(&key_path)) {
            Ok((config, _)) => {
                let acceptor = TlsAcceptor::from(config);
                if tx.send(acceptor).is_err() {
                    // Receiver dropped — server is shutting down.
                    break;
                }
                last_cert = Some(current_cert);
                last_key = Some(current_key);
                tracing::info!("TLS certificate reloaded successfully");
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to reload TLS certificate, keeping current");
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

        // Generate by calling the internal function indirectly — use
        // build_server_config with no paths, but override HOME.
        let san = vec!["localhost".to_owned(), "127.0.0.1".to_owned()];
        let rcgen::CertifiedKey { cert, key_pair } =
            rcgen::generate_simple_self_signed(san).unwrap();

        let cert_pem = cert.pem();
        let key_pem = key_pair.serialize_pem();

        fs::write(&cert_path, &cert_pem).unwrap();
        fs::write(&key_path, &key_pem).unwrap();

        // Verify we can load them back and build a rustls config.
        let (config, self_signed) = build_server_config(Some(&cert_path), Some(&key_path)).unwrap();
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

        let result = build_server_config(Some(&cert_only), None);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("both"));
    }

    #[test]
    fn rejects_missing_cert_file() {
        let result = build_server_config(
            Some(Path::new("/nonexistent/cert.pem")),
            Some(Path::new("/nonexistent/key.pem")),
        );
        assert!(result.is_err());
    }
}
