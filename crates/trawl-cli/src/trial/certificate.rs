// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The trial's TLS certificate, generated in process.
//!
//! `up` generates one self-signed pair the first time it finds the trawld
//! volume without a certificate, and never again for that trial (ADR-0045).
//! The private key PEM lives in a buffer that is zeroed on drop, inside a
//! [`TrialPair`] whose `Debug` shows none of it. It leaves the process only
//! through [`super::secrets::put_file`]. rcgen and its crypto backend make
//! copies of their own while generating and encoding the key, and those are
//! not wiped.

use std::fmt;

use zeroize::Zeroizing;

use super::TrialError;

/// The names trawld answers to: `trawld` for trawl-web on the trial
/// network, and loopback for the host CLI. rcgen makes the two addresses
/// IP SANs.
pub const SUBJECT_ALT_NAMES: [&str; 4] = ["trawld", "localhost", "127.0.0.1", "::1"];

/// How long the certificate is valid. A trial is disposable, and `up`
/// never rotates its certificate.
pub const VALIDITY_DAYS: i64 = 730;

/// A generated certificate and its private key, both PEM.
pub struct TrialPair {
    cert_pem: String,
    key_pem: Zeroizing<String>,
}

impl TrialPair {
    /// Generate a self-signed ECDSA P-256 certificate for
    /// [`SUBJECT_ALT_NAMES`], valid from now for [`VALIDITY_DAYS`].
    pub fn generate() -> Result<Self, TrialError> {
        // rcgen's errors name what failed, never the key, but the reason
        // stays generic so no future variant can carry one.
        let failed = |_: rcgen::Error| TrialError::Certificate {
            reason: "it could not be generated",
        };
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).map_err(failed)?;
        let mut params =
            rcgen::CertificateParams::new(SUBJECT_ALT_NAMES.map(str::to_owned).to_vec())
                .map_err(failed)?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "trawl trial");
        let now = time::OffsetDateTime::now_utc();
        params.not_before = now;
        params.not_after = now + time::Duration::days(VALIDITY_DAYS);
        let cert = params.self_signed(&key).map_err(failed)?;
        Ok(Self {
            cert_pem: cert.pem(),
            key_pem: Zeroizing::new(key.serialize_pem()),
        })
    }

    /// The private key: PKCS#8, PEM.
    pub fn key_pem(&self) -> &str {
        &self.key_pem
    }

    /// The certificate, PEM.
    #[cfg(test)]
    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    /// The certificate, dropping the key, which is zeroed.
    pub fn into_cert_pem(self) -> String {
        self.cert_pem
    }
}

impl fmt::Debug for TrialPair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TrialPair { key_pem: <redacted>, .. }")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::net::IpAddr;
    use std::sync::Arc;

    use rustls_pki_types::pem::PemObject as _;
    use rustls_pki_types::{CertificateDer, PrivateKeyDer};
    use x509_parser::extensions::GeneralName;
    use x509_parser::oid_registry::{OID_EC_P256, OID_KEY_TYPE_EC_PUBLIC_KEY};

    use super::*;

    /// The generated pair is the certificate trawld serves and both
    /// clients pin: exactly the four names, P-256, a validity window of
    /// exactly 730 days that starts now, and a key that rustls loads with
    /// it. `Debug` shows no key byte.
    #[test]
    fn trial_certificate_policy() {
        let started = time::OffsetDateTime::now_utc().unix_timestamp();
        let pair = TrialPair::generate().unwrap();

        let (_, pem) = x509_parser::pem::parse_x509_pem(pair.cert_pem().as_bytes()).unwrap();
        assert_eq!(pem.label, "CERTIFICATE");
        let cert = pem.parse_x509().unwrap();

        let names = &cert
            .subject_alternative_name()
            .unwrap()
            .expect("a SAN extension")
            .value
            .general_names;
        assert_eq!(names.len(), 4, "{names:?}");
        let found: BTreeSet<String> = names
            .iter()
            .map(|name| match name {
                GeneralName::DNSName(dns) => format!("dns:{dns}"),
                GeneralName::IPAddress(bytes) => {
                    let ip = match bytes.len() {
                        4 => IpAddr::from(<[u8; 4]>::try_from(*bytes).unwrap()),
                        16 => IpAddr::from(<[u8; 16]>::try_from(*bytes).unwrap()),
                        n => panic!("an IP SAN of {n} bytes"),
                    };
                    format!("ip:{ip}")
                }
                other => panic!("unexpected SAN {other:?}"),
            })
            .collect();
        let expected: BTreeSet<String> = ["dns:trawld", "dns:localhost", "ip:127.0.0.1", "ip:::1"]
            .map(str::to_owned)
            .into();
        assert_eq!(found, expected);

        let validity = cert.validity();
        let not_before = validity.not_before.timestamp();
        let not_after = validity.not_after.timestamp();
        assert_eq!(not_after - not_before, VALIDITY_DAYS * 86_400);
        assert!(
            (not_before - started).abs() <= 120,
            "not_before {not_before}, now {started}"
        );

        let spki = cert.public_key();
        assert_eq!(spki.algorithm.algorithm, OID_KEY_TYPE_EC_PUBLIC_KEY);
        let curve = spki
            .algorithm
            .parameters
            .as_ref()
            .expect("a named curve")
            .as_oid()
            .unwrap();
        assert_eq!(curve, OID_EC_P256);
        let key = rcgen::KeyPair::from_pem(pair.key_pem()).unwrap();
        assert_eq!(key.algorithm(), &rcgen::PKCS_ECDSA_P256_SHA256);

        // trawld's load: rustls refuses a key that does not match the
        // certificate's public key.
        let chain = CertificateDer::pem_slice_iter(pair.cert_pem().as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(chain.len(), 1);
        let private = PrivateKeyDer::from_pem_slice(pair.key_pem().as_bytes()).unwrap();
        rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(chain, private)
        .expect("rustls loads the pair");

        let debug = format!("{pair:?}");
        assert!(debug.contains("<redacted>"), "{debug}");
        assert!(!debug.contains("PRIVATE KEY"), "{debug}");
        for line in pair.key_pem().lines().filter(|l| !l.starts_with("-----")) {
            assert!(!debug.contains(line), "{debug}");
        }
    }
}
