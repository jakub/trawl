// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Token generation, hashing, and verification.
//!
//! Tokens use the `flt_` prefix followed by 32 bytes of base64url-encoded
//! random data (43 chars after the prefix). The first 8 chars of the
//! base64url body are the operational prefix. The indexed database lookup,
//! revocation, and one component of the verification cache key all use it.

use std::sync::LazyLock;

use base64ct::{Base64UrlUnpadded, Encoding as _};
use rand::RngCore as _;
use zeroize::Zeroizing;

use crate::error::AuthError;

/// Length of the random token body in bytes.
const TOKEN_BYTES: usize = 32;

/// String prefix prepended to every fleet API token.
const TOKEN_PREFIX: &str = "flt_";

/// Number of characters from the base64url body used as the operational
/// prefix. NOT a security boundary — randomness lives in the full 32 bytes.
const PREFIX_LENGTH: usize = 8;

/// A generated API token. Shown to the user exactly once at creation; the
/// plaintext is never persisted.
///
/// The `Debug` impl redacts `plaintext` — accidental `{:?}` logging of a
/// freshly-generated token would leak the one-time API key otherwise.
#[derive(Clone)]
pub struct GeneratedToken {
    /// Full token string: `flt_` + 43 base64url chars.
    pub plaintext: Zeroizing<String>,
    /// First 8 chars of the base64url body — the operational prefix used
    /// for identification and the indexed DB lookup.
    pub prefix: String,
}

impl std::fmt::Debug for GeneratedToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeneratedToken")
            .field("prefix", &self.prefix)
            .field("plaintext", &"<redacted>")
            .finish()
    }
}

/// Pre-computed dummy argon2id hash for timing equalization on prefix miss.
///
/// Generated once at process start so the `verify_token` call on the miss
/// path takes the same wall-clock as the hit path.
pub static DUMMY_HASH: LazyLock<String> =
    LazyLock::new(|| hash_token("dummy-timing-equalization").expect("failed to hash dummy"));

/// Generate a new cryptographically random API token.
pub fn generate_token() -> GeneratedToken {
    let mut bytes = [0u8; TOKEN_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut bytes);

    let encoded = Base64UrlUnpadded::encode_string(&bytes);
    let prefix = encoded[..PREFIX_LENGTH].to_owned();
    let plaintext = Zeroizing::new(format!("{TOKEN_PREFIX}{encoded}"));

    GeneratedToken { plaintext, prefix }
}

/// Extract the operational prefix from a full token string.
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
/// Returns the PHC-formatted hash string (algorithm + params + salt + hash).
///
/// Production params: 128 MiB memory, 3 iterations, 4 lanes — appropriate for
/// long-lived admin credentials. With the `fast-hash` feature: 1 MiB / 1 / 1
/// — used in tests so the suite doesn't burn ~200 ms per KDF.
pub fn hash_token(plaintext: &str) -> Result<String, AuthError> {
    use argon2::password_hash::SaltString;
    use argon2::{Algorithm, Argon2, Params, PasswordHasher as _, Version};

    let salt = SaltString::generate(&mut rand::rngs::OsRng);

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
/// Returns `true` if the token matches, `false` otherwise. Constant-time
/// comparison provided by argon2.
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
    use std::collections::HashSet;

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
    fn generated_token_prefix_matches_body() {
        let token = generate_token();
        let body = token.plaintext.strip_prefix("flt_").unwrap();
        assert_eq!(&body[..PREFIX_LENGTH], token.prefix);
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
    fn verify_wrong_token_returns_false() {
        let token = generate_token();
        let hash = hash_token(&token.plaintext).unwrap();
        let other = generate_token();
        assert!(!verify_token(&other.plaintext, &hash).unwrap());
    }

    #[test]
    fn verify_corrupted_hash_errors() {
        let result = verify_token("flt_whatever", "not-a-valid-phc-string");
        assert!(matches!(result, Err(AuthError::Hash(_))));
    }

    #[test]
    fn dummy_hash_exists_and_verifies_false_for_random_input() {
        // DUMMY_HASH must be a valid PHC string that always returns false for
        // non-matching plaintext — required for the `verify_key` timing
        // equalization path.
        let token = generate_token();
        let result = verify_token(&token.plaintext, &DUMMY_HASH);
        assert!(!result.unwrap());
    }

    #[test]
    fn generated_token_debug_redacts_plaintext() {
        let token = generate_token();
        let debug = format!("{token:?}");
        assert!(debug.contains("<redacted>"), "expected redaction marker");
        assert!(
            !debug.contains(&*token.plaintext.to_string()),
            "must not contain plaintext token"
        );
        assert!(
            debug.contains(&token.prefix),
            "prefix is non-secret and should appear in debug"
        );
    }

    #[test]
    fn ten_thousand_tokens_are_unique() {
        // Statistical sanity: a CSPRNG should never collide in 10k draws of
        // 256-bit tokens. Catches catastrophic RNG/base64/slicing regressions.
        // Not a security proof — that comes from the 256-bit entropy.
        let mut plaintexts = HashSet::with_capacity(10_000);
        let mut prefixes = HashSet::with_capacity(10_000);
        for _ in 0..10_000 {
            let t = generate_token();
            assert!(
                plaintexts.insert(t.plaintext.to_string()),
                "duplicate plaintext after {} draws — RNG regression",
                plaintexts.len()
            );
            // Prefixes are 48 bits (8 base64url chars), so collisions among
            // 10k draws are possible (~birthday-paradox at ~16M); we don't
            // assert prefix uniqueness, just count them for visibility.
            prefixes.insert(t.prefix);
        }
        assert_eq!(plaintexts.len(), 10_000);
        assert!(prefixes.len() > 9_990, "suspicious prefix collision rate");
    }
}
