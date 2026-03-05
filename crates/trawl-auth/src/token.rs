//! Token generation, hashing, and verification.
//!
//! Tokens use the `flt_` prefix followed by 32 bytes of base64url-encoded
//! random data (43 chars). The first 8 chars of the base64url portion serve
//! as the "prefix" used for listing and revocation.

use base64ct::Base64UrlUnpadded;
use base64ct::Encoding as _;
use rand::RngCore as _;
use zeroize::Zeroizing;

use crate::error::AuthError;

/// Length of the random token body in bytes.
const TOKEN_BYTES: usize = 32;

/// The string prefix prepended to all trawl API tokens.
const TOKEN_PREFIX: &str = "flt_";

/// Number of characters from the base64url portion used as the key prefix.
const PREFIX_LENGTH: usize = 8;

/// A generated API token, shown to the user exactly once at creation time.
#[derive(Debug, Clone)]
pub struct GeneratedToken {
    /// The full token string: `flt_` + 32 bytes base64url.
    /// Wrapped in [`Zeroizing`] to clear from memory on drop.
    pub plaintext: Zeroizing<String>,
    /// The first 8 chars of the base64url portion, used for identification.
    pub prefix: String,
}

/// Generate a new cryptographically random API token.
pub fn generate_token() -> GeneratedToken {
    let mut bytes = [0u8; TOKEN_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut bytes);

    let encoded = Base64UrlUnpadded::encode_string(&bytes);
    let prefix = encoded[..PREFIX_LENGTH].to_owned();
    let plaintext = Zeroizing::new(format!("{TOKEN_PREFIX}{encoded}"));

    GeneratedToken { plaintext, prefix }
}

/// Extract the prefix from a full token string.
///
/// Returns `None` if the token doesn't start with `flt_` or is too short.
pub fn extract_prefix(token: &str) -> Option<&str> {
    let body = token.strip_prefix(TOKEN_PREFIX)?;
    if body.len() < PREFIX_LENGTH {
        return None;
    }
    Some(&body[..PREFIX_LENGTH])
}

/// Hash a plaintext token using argon2id.
///
/// Returns the PHC-formatted hash string (includes algorithm, params, salt, hash).
pub fn hash_token(plaintext: &str) -> Result<String, AuthError> {
    use argon2::password_hash::SaltString;
    use argon2::{Algorithm, Argon2, Params, PasswordHasher as _, Version};

    let salt = SaltString::generate(&mut rand::rngs::OsRng);
    // Production: 128 MiB memory, 3 iterations, 4 lanes — strong params for
    // long-lived admin credentials. Key creation is rare so cost is negligible.
    // Test (fast-hash): 1 MiB, 1 iteration, 1 lane — fast enough to verify
    // argon2 integration without burning CI time.
    #[cfg(feature = "fast-hash")]
    let params = Params::new(1024, 1, 1, None).expect("valid argon2 params");
    #[cfg(not(feature = "fast-hash"))]
    let params = Params::new(128 * 1024, 3, 4, None).expect("valid argon2 params");
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);

    argon2
        .hash_password(plaintext.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| AuthError::Hash(e.to_string()))
}

/// Verify a plaintext token against an argon2id PHC hash string.
///
/// Returns `true` if the token matches, `false` otherwise.
/// The comparison is constant-time (provided by argon2).
pub fn verify_token(plaintext: &str, hash: &str) -> Result<bool, AuthError> {
    use argon2::password_hash::PasswordHash;
    use argon2::{Argon2, PasswordVerifier as _};

    let parsed_hash =
        PasswordHash::new(hash).map_err(|e| AuthError::Hash(format!("invalid hash: {e}")))?;

    match Argon2::default().verify_password(plaintext.as_bytes(), &parsed_hash) {
        Ok(()) => Ok(true),
        Err(argon2::password_hash::Error::Password) => Ok(false),
        Err(e) => Err(AuthError::Hash(format!("verification failed: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_token_format() {
        let token = generate_token();
        assert!(
            token.plaintext.starts_with("flt_"),
            "token should start with flt_"
        );
        // flt_ (4) + base64url of 32 bytes (43) = 47 chars
        assert_eq!(token.plaintext.len(), 47);
        assert_eq!(token.prefix.len(), PREFIX_LENGTH);
    }

    #[test]
    fn generate_token_uniqueness() {
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a.plaintext, b.plaintext);
    }

    #[test]
    fn extract_prefix_valid() {
        let token = generate_token();
        let prefix = extract_prefix(&token.plaintext).unwrap();
        assert_eq!(prefix, token.prefix);
    }

    #[test]
    fn extract_prefix_missing_flt() {
        assert!(extract_prefix("sk_abcdefghijklmnop").is_none());
    }

    #[test]
    fn extract_prefix_too_short() {
        assert!(extract_prefix("flt_abc").is_none());
    }

    #[test]
    fn hash_and_verify_roundtrip() {
        let token = generate_token();
        let hash = hash_token(&token.plaintext).unwrap();
        assert!(verify_token(&token.plaintext, &hash).unwrap());
    }

    #[test]
    fn verify_wrong_token() {
        let token = generate_token();
        let hash = hash_token(&token.plaintext).unwrap();
        let other = generate_token();
        assert!(!verify_token(&other.plaintext, &hash).unwrap());
    }

    #[test]
    fn verify_corrupted_hash() {
        let result = verify_token("flt_whatever", "not-a-valid-phc-string");
        assert!(result.is_err());
    }
}
