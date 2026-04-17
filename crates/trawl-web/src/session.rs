// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Session cookie AEAD.
//!
//! Sessions are stored in a single `HttpOnly; Secure; SameSite=Strict` cookie,
//! encrypted with XChaCha20-Poly1305. The payload is small — just the bearer
//! token plus identity metadata fetched from trawld at login time. Tokens
//! never reach the browser's JavaScript environment; they're only decrypted
//! server-side when the proxy needs to forward a request to trawld.
//!
//! The encoding is `base64url(nonce || ciphertext || tag)`. A fresh 24-byte
//! nonce is drawn from the OS RNG on every encrypt. Tamper resistance and
//! confidentiality come from XChaCha20-Poly1305's AEAD construction.

use std::fs;
use std::path::Path;

use base64ct::{Base64UrlUnpadded, Encoding};
use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305, XNonce,
    aead::{Aead, OsRng, rand_core::RngCore},
};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

/// Cookie name used for the encrypted session. Lives here so every
/// module that reads, writes, or clears the cookie agrees on the
/// wire-level identifier.
pub const SESSION_COOKIE: &str = "trawl_session";

/// Build a `Set-Cookie` header value for a live session cookie.
///
/// Delegates to the `cookie` crate for attribute serialization rather
/// than hand-rolling a format string — belt-and-suspenders against
/// future bugs where a value character (e.g. `;` or `\r`) could inject
/// spurious directives into the header. Session values today are
/// base64url-safe, so this is purely defensive; code-review flagged it
/// as a cosmetic concern worth closing.
#[must_use]
pub fn build_session_cookie_header(
    name: &'static str,
    value: String,
    max_age_secs: u64,
    secure: bool,
) -> String {
    cookie::Cookie::build((name, value))
        .http_only(true)
        .same_site(cookie::SameSite::Strict)
        .path("/")
        .max_age(cookie::time::Duration::seconds(
            i64::try_from(max_age_secs).unwrap_or(i64::MAX),
        ))
        .secure(secure)
        .to_string()
}

/// Build a `Set-Cookie` header value that clears the named cookie.
///
/// Attributes match what the login handler sets on creation
/// (`HttpOnly`, `SameSite=Strict`, `Path=/`, optional `Secure`),
/// so browsers will accept the clear directive. `Max-Age=0` signals
/// immediate deletion. The optional `Secure` attribute mirrors the
/// original cookie — dev deployments with `allow_insecure_cookies =
/// true` set it to `false` to accept the cookie on plain HTTP.
#[must_use]
pub fn build_clear_cookie_header(name: &'static str, secure: bool) -> String {
    cookie::Cookie::build((name, ""))
        .http_only(true)
        .same_site(cookie::SameSite::Strict)
        .path("/")
        .max_age(cookie::time::Duration::ZERO)
        .secure(secure)
        .to_string()
}

/// Length of the symmetric AEAD key in bytes (XChaCha20-Poly1305).
pub const KEY_LEN: usize = 32;

/// Length of the `XChaCha20` nonce in bytes.
pub const NONCE_LEN: usize = 24;

/// Errors produced while creating or consuming session cookies.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("failed to read key file: {0}")]
    KeyFile(#[from] std::io::Error),

    #[error("key file must contain exactly {KEY_LEN} bytes, got {0}")]
    KeyLength(usize),

    #[error("key base64 decode failed")]
    KeyDecode,

    #[error("cookie base64 decode failed")]
    CookieDecode,

    #[error("cookie too short ({0} bytes) for nonce+tag framing")]
    CookieShort(usize),

    #[error("cookie AEAD verification failed (tampered or wrong key)")]
    Aead,

    #[error("cookie JSON payload malformed: {0}")]
    Json(#[from] serde_json::Error),

    #[error("session expired")]
    Expired,
}

/// A 32-byte XChaCha20-Poly1305 AEAD key used to protect session cookies.
///
/// Wraps `Zeroizing<[u8; KEY_LEN]>` so the key is scrubbed from memory when
/// dropped. Clone is intentionally unavailable — take a borrow instead.
pub struct SessionKey(Zeroizing<[u8; KEY_LEN]>);

impl SessionKey {
    /// Wrap an existing 32-byte buffer as a session key.
    #[must_use]
    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// Generate a fresh random key from the OS RNG.
    #[must_use]
    pub fn generate() -> Self {
        let mut bytes = [0u8; KEY_LEN];
        OsRng.fill_bytes(&mut bytes);
        Self::from_bytes(bytes)
    }

    /// Load a key from a base64 (url-safe, unpadded) string.
    ///
    /// # Errors
    /// Returns `KeyDecode` if the string isn't valid base64, or `KeyLength`
    /// if the decoded byte count isn't exactly [`KEY_LEN`].
    pub fn from_base64(s: &str) -> Result<Self, SessionError> {
        let decoded =
            Base64UrlUnpadded::decode_vec(s.trim()).map_err(|_| SessionError::KeyDecode)?;
        let bytes: [u8; KEY_LEN] = decoded
            .try_into()
            .map_err(|v: Vec<u8>| SessionError::KeyLength(v.len()))?;
        Ok(Self::from_bytes(bytes))
    }

    /// Load a key from a file. The file must contain exactly 32 bytes
    /// (raw, not base64). Trailing newlines are NOT trimmed — use
    /// `trawl-admin` to generate keys correctly.
    ///
    /// # Errors
    /// Returns `KeyFile` on IO failure or `KeyLength` if the file is the
    /// wrong size.
    pub fn from_file(path: &Path) -> Result<Self, SessionError> {
        let bytes = fs::read(path)?;
        let arr: [u8; KEY_LEN] = bytes
            .try_into()
            .map_err(|v: Vec<u8>| SessionError::KeyLength(v.len()))?;
        Ok(Self::from_bytes(arr))
    }
}

impl std::fmt::Debug for SessionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionKey")
            .field("len", &KEY_LEN)
            .finish_non_exhaustive()
    }
}

/// Plaintext payload stored inside an encrypted session cookie.
///
/// `token` is the bearer token that the proxy re-attaches to outbound
/// requests to trawld. It's wrapped in `Zeroizing` so it's cleared on drop.
#[derive(Serialize, Deserialize)]
pub struct SessionPayload {
    /// Bearer token to forward upstream.
    #[serde(with = "zeroizing_string")]
    pub token: Zeroizing<String>,

    /// Identity name from `/whoami`, surfaced in the UI's user menu.
    pub name: String,

    /// Role from `/whoami` (e.g. "admin", "analyst"), used for client-side
    /// gating of admin-only screens.
    pub role: String,

    /// Absolute unix-second expiry timestamp.
    pub exp: i64,
}

impl std::fmt::Debug for SessionPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionPayload")
            .field("name", &self.name)
            .field("role", &self.role)
            .field("exp", &self.exp)
            .finish_non_exhaustive()
    }
}

/// Encrypt a session payload into a base64url cookie value.
///
/// The output is `base64url(nonce || ciphertext || tag)` with no padding.
///
/// # Errors
/// Returns `Json` if serialization fails (effectively never for valid
/// payloads) or `Aead` if the AEAD primitive refuses to encrypt (also
/// effectively never — included for defensiveness).
pub fn encrypt(key: &SessionKey, payload: &SessionPayload) -> Result<String, SessionError> {
    let plaintext = serde_json::to_vec(payload)?;

    let cipher = XChaCha20Poly1305::new(key.0.as_ref().into());

    let mut nonce_bytes = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = XNonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_ref())
        .map_err(|_| SessionError::Aead)?;

    let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    Ok(Base64UrlUnpadded::encode_string(&out))
}

/// Decrypt a cookie value back into a [`SessionPayload`].
///
/// # Errors
/// - `CookieDecode`: the cookie isn't valid base64
/// - `CookieShort`: the cookie is too short to contain a nonce + tag
/// - `Aead`: AEAD verification failed (tampered ciphertext or wrong key)
/// - `Json`: ciphertext decrypted but plaintext wasn't valid payload JSON
///
/// This function does NOT enforce expiry — call [`is_expired`] separately.
pub fn decrypt(key: &SessionKey, cookie_value: &str) -> Result<SessionPayload, SessionError> {
    let bytes =
        Base64UrlUnpadded::decode_vec(cookie_value).map_err(|_| SessionError::CookieDecode)?;
    if bytes.len() < NONCE_LEN + 16 {
        return Err(SessionError::CookieShort(bytes.len()));
    }

    let (nonce_bytes, ciphertext) = bytes.split_at(NONCE_LEN);
    let nonce = XNonce::from_slice(nonce_bytes);

    let cipher = XChaCha20Poly1305::new(key.0.as_ref().into());
    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| SessionError::Aead)?;

    let payload: SessionPayload = serde_json::from_slice(&plaintext)?;
    Ok(payload)
}

/// Check whether a payload's `exp` is earlier than `now`.
///
/// Callers supply `now` explicitly so that tests aren't clock-dependent
/// and so the proxy can decide once-per-request what "now" means.
#[must_use]
pub fn is_expired(payload: &SessionPayload, now: i64) -> bool {
    payload.exp <= now
}

/// Serde adapter so `Zeroizing<String>` round-trips as a plain JSON string.
mod zeroizing_string {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use zeroize::Zeroizing;

    pub fn serialize<S: Serializer>(v: &Zeroizing<String>, s: S) -> Result<S::Ok, S::Error> {
        v.as_str().serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Zeroizing<String>, D::Error> {
        let s = String::deserialize(d)?;
        Ok(Zeroizing::new(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_payload() -> SessionPayload {
        SessionPayload {
            token: Zeroizing::new("flt_testtoken123".to_string()),
            name: "alice".to_string(),
            role: "analyst".to_string(),
            exp: 1_700_000_000,
        }
    }

    #[test]
    fn encrypt_decrypt_round_trip() {
        let key = SessionKey::generate();
        let payload = sample_payload();

        let cookie = encrypt(&key, &payload).unwrap();
        let decoded = decrypt(&key, &cookie).unwrap();

        assert_eq!(decoded.token.as_str(), "flt_testtoken123");
        assert_eq!(decoded.name, "alice");
        assert_eq!(decoded.role, "analyst");
        assert_eq!(decoded.exp, 1_700_000_000);
    }

    #[test]
    fn distinct_nonces_produce_distinct_ciphertexts() {
        let key = SessionKey::generate();
        let payload = sample_payload();

        let a = encrypt(&key, &payload).unwrap();
        let b = encrypt(&key, &payload).unwrap();
        assert_ne!(a, b, "fresh nonce on every encrypt");
    }

    #[test]
    fn tampered_cookie_fails_verification() {
        let key = SessionKey::generate();
        let cookie = encrypt(&key, &sample_payload()).unwrap();

        // flip a byte near the end (AEAD tag territory)
        let mut bytes = Base64UrlUnpadded::decode_vec(&cookie).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        let tampered = Base64UrlUnpadded::encode_string(&bytes);

        let err = decrypt(&key, &tampered).unwrap_err();
        assert!(matches!(err, SessionError::Aead));
    }

    #[test]
    fn wrong_key_fails_verification() {
        let key_a = SessionKey::generate();
        let key_b = SessionKey::generate();
        let cookie = encrypt(&key_a, &sample_payload()).unwrap();

        let err = decrypt(&key_b, &cookie).unwrap_err();
        assert!(matches!(err, SessionError::Aead));
    }

    #[test]
    fn truncated_cookie_rejected() {
        let key = SessionKey::generate();
        let cookie = encrypt(&key, &sample_payload()).unwrap();
        let mut bytes = Base64UrlUnpadded::decode_vec(&cookie).unwrap();
        bytes.truncate(NONCE_LEN); // leave nonce, strip ciphertext+tag
        let truncated = Base64UrlUnpadded::encode_string(&bytes);

        let err = decrypt(&key, &truncated).unwrap_err();
        assert!(matches!(err, SessionError::CookieShort(_)));
    }

    #[test]
    fn garbage_cookie_rejected() {
        let key = SessionKey::generate();
        let err = decrypt(&key, "not!!!base64!!!").unwrap_err();
        assert!(matches!(err, SessionError::CookieDecode));
    }

    #[test]
    fn expired_detected() {
        let payload = SessionPayload {
            exp: 100,
            ..sample_payload()
        };
        assert!(is_expired(&payload, 200));
        assert!(!is_expired(&payload, 50));
        // boundary: exp <= now is considered expired
        assert!(is_expired(&payload, 100));
    }

    #[test]
    fn key_from_file_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key.bin");

        let original = SessionKey::generate();
        std::fs::write(&path, original.0.as_ref()).unwrap();

        let loaded = SessionKey::from_file(&path).unwrap();

        let cookie = encrypt(&original, &sample_payload()).unwrap();
        let decoded = decrypt(&loaded, &cookie).unwrap();
        assert_eq!(decoded.name, "alice");
    }

    #[test]
    fn key_from_file_rejects_wrong_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key.bin");
        std::fs::write(&path, b"too short").unwrap();

        let err = SessionKey::from_file(&path).unwrap_err();
        assert!(matches!(err, SessionError::KeyLength(9)));
    }

    #[test]
    fn key_from_base64_round_trip() {
        let original = SessionKey::generate();
        let b64 = Base64UrlUnpadded::encode_string(original.0.as_ref());

        let loaded = SessionKey::from_base64(&b64).unwrap();

        let cookie = encrypt(&original, &sample_payload()).unwrap();
        let decoded = decrypt(&loaded, &cookie).unwrap();
        assert_eq!(decoded.name, "alice");
    }
}
