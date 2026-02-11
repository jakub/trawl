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

    let rcgen::CertifiedKey { cert, key_pair } =
        rcgen::generate_simple_self_signed(subject_alt_names)
            .map_err(|e| format!("certificate generation failed: {e}"))?;

    let cert_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();

    fs::create_dir_all(output_dir)
        .map_err(|e| format!("failed to create {}: {e}", output_dir.display()))?;

    let cert_path = output_dir.join("cert.pem");
    let key_path = output_dir.join("key.pem");

    fs::write(&cert_path, &cert_pem)
        .map_err(|e| format!("failed to write {}: {e}", cert_path.display()))?;
    fs::write(&key_path, &key_pem)
        .map_err(|e| format!("failed to write {}: {e}", key_path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("failed to set key permissions: {e}"))?;
    }

    println!("cert: {}", cert_path.display());
    println!("key:  {}", key_path.display());
    eprintln!("done — restart fleetd to pick up the new certificate");

    Ok(())
}
