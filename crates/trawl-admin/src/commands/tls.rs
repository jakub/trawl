//! TLS certificate management commands.

use std::fs;
use std::path::Path;

/// Generate a self-signed TLS certificate and private key.
pub fn generate(output_dir: &Path, extra_san: &[String]) -> Result<(), String> {
    let mut subject_alt_names = vec![
        "localhost".to_owned(),
        "127.0.0.1".to_owned(),
        "::1".to_owned(),
    ];
    for san in extra_san {
        if !subject_alt_names.contains(san) {
            subject_alt_names.push(san.clone());
        }
    }

    eprintln!(
        "generating self-signed certificate (SAN: {})",
        subject_alt_names.join(", ")
    );

    // Explicit ECDSA P-256 key + 2-year validity for auditability.
    let key_pair = rcgen::KeyPair::generate().map_err(|e| format!("key generation failed: {e}"))?;

    let mut params = rcgen::CertificateParams::new(subject_alt_names)
        .map_err(|e| format!("certificate params failed: {e}"))?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "trawl self-signed");
    params.not_before = time::OffsetDateTime::now_utc();
    params.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(730);

    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| format!("certificate generation failed: {e}"))?;

    let cert_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();

    fs::create_dir_all(output_dir)
        .map_err(|e| format!("failed to create {}: {e}", output_dir.display()))?;

    let cert_path = output_dir.join("cert.pem");
    let key_path = output_dir.join("key.pem");

    fs::write(&cert_path, &cert_pem)
        .map_err(|e| format!("failed to write {}: {e}", cert_path.display()))?;

    // Write the private key with restricted permissions from the start to
    // avoid a TOCTOU window where the key is world-readable.
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let mut key_file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&key_path)
            .map_err(|e| format!("failed to create {}: {e}", key_path.display()))?;
        key_file
            .write_all(key_pem.as_bytes())
            .map_err(|e| format!("failed to write {}: {e}", key_path.display()))?;
    }

    #[cfg(not(unix))]
    {
        fs::write(&key_path, &key_pem)
            .map_err(|e| format!("failed to write {}: {e}", key_path.display()))?;
    }

    println!("cert: {}", cert_path.display());
    println!("key:  {}", key_path.display());
    eprintln!("done — restart trawld to pick up the new certificate");

    Ok(())
}
