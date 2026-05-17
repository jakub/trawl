// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Session cookie AEAD and per-app configuration (ADR-0030).
//!
//! Sessions are stored in a single AEAD-encrypted cookie. Cookie attributes
//! (`HttpOnly`, `Secure`, `SameSite`, `Domain`, `Path`, `Max-Age`) are owned
//! by [`SessionConfig`] so each consuming app picks its own namespace and
//! domain. The default `same_site` is [`cookie::SameSite::Lax`] to support
//! cross-subdomain SSO (ADR-0030 §"Shared session cookie"); apps that want
//! stricter posture set [`cookie::SameSite::Strict`] explicitly.
//!
//! The encoding is `base64url(nonce || ciphertext || tag)` with no padding.
//! A fresh 24-byte nonce is drawn from the OS RNG on every encrypt. Tamper
//! resistance and confidentiality come from XChaCha20-Poly1305.

use std::fs;
use std::path::Path;

use base64ct::{Base64, Base64Unpadded, Base64Url, Base64UrlUnpadded, Encoding};
use chacha20poly1305::aead::Aead;
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::validation::validate_app_namespace;

/// Length of the symmetric AEAD key in bytes (XChaCha20-Poly1305).
pub const KEY_LEN: usize = 32;

/// Length of the `XChaCha20` nonce in bytes.
pub const NONCE_LEN: usize = 24;

/// Default cookie name when [`SessionConfig::default`] is used.
///
/// The shared name across fleet apps is `fleet_session` so a single cookie
/// scoped to the parent domain provides SSO across subdomains.
pub const DEFAULT_COOKIE_NAME: &str = "fleet_session";

/// Default session TTL in seconds (24 hours).
pub const DEFAULT_TTL_SECS: u64 = 86_400;

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

    /// JSON serialise/deserialise failure. Carries the error as `String`
    /// rather than `serde_json::Error` so the public API doesn't leak the
    /// underlying codec — swapping to a different framing later wouldn't
    /// be a SemVer break.
    #[error("cookie JSON payload malformed: {0}")]
    Json(String),

    #[error("session expired")]
    Expired,
}

/// A 32-byte XChaCha20-Poly1305 AEAD key used to protect session cookies.
///
/// Wraps `Zeroizing<[u8; KEY_LEN]>` so the key is scrubbed from memory when
/// dropped. Clone is intentionally unavailable — share an `Arc<SessionKey>`
/// instead of duplicating the secret material.
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

    /// Load a key from a base64 string. Accepts all four common variants
    /// (standard or url-safe alphabet, with or without `=` padding) so keys
    /// produced by `openssl rand -base64`, `python3 -m base64`, and
    /// `fleet-admin generate-session-key` all work as drop-ins.
    ///
    /// # Errors
    /// Returns `KeyDecode` if the string isn't valid base64 in any of the
    /// four flavors, or `KeyLength` if the decoded byte count isn't exactly
    /// [`KEY_LEN`].
    pub fn from_base64(s: &str) -> Result<Self, SessionError> {
        let trimmed = s.trim();
        // Try url-safe first (the historical canonical form), fall back
        // through the other three. All four decoders are constant-time and
        // reject non-alphabet bytes, so the cascade is safe.
        let decoded = Base64UrlUnpadded::decode_vec(trimmed)
            .or_else(|_| Base64Url::decode_vec(trimmed))
            .or_else(|_| Base64Unpadded::decode_vec(trimmed))
            .or_else(|_| Base64::decode_vec(trimmed))
            .map_err(|_| SessionError::KeyDecode)?;
        let bytes: [u8; KEY_LEN] = decoded
            .try_into()
            .map_err(|v: Vec<u8>| SessionError::KeyLength(v.len()))?;
        Ok(Self::from_bytes(bytes))
    }

    /// Load a key from a file. The file must contain exactly 32 bytes (raw,
    /// not base64). Trailing newlines are NOT trimmed — generate keys with
    /// `fleet-admin generate-session-key`, which writes raw bytes.
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
/// App-agnostic per ADR-0030: `role` is *not* in the payload — each app
/// resolves its role from the [`VerifiedKey`] returned by
/// `KeyStore::verify_key` at request time. `name` stays so the UI can render
/// the user's name on first paint without a verify round-trip.
///
/// `token` is the bearer token that middleware re-verifies against
/// `KeyStore::verify_key`. Wrapped in [`Zeroizing`] so it's cleared on drop.
///
/// [`VerifiedKey`]: crate::types::VerifiedKey
#[derive(Serialize, Deserialize)]
pub struct SessionPayload {
    /// Bearer token to re-verify on each request.
    #[serde(with = "zeroizing_string")]
    pub token: Zeroizing<String>,

    /// Identity name (UI convenience, avoids a verify round-trip for first paint).
    pub name: String,

    /// Absolute unix-second expiry timestamp.
    pub exp: i64,
}

impl std::fmt::Debug for SessionPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionPayload")
            .field("name", &self.name)
            .field("exp", &self.exp)
            .finish_non_exhaustive()
    }
}

/// Per-app session configuration. Each consuming binary owns one and uses
/// the same instance for cookie writes, cookie reads, and the namespace
/// check in `RequireSession`.
///
/// `app_namespace` MUST be non-empty and pass [`validate_app_namespace`] —
/// [`SessionConfig::validate`] returns an error if not. Construct via
/// [`SessionConfig::new`] for fail-fast validation, or
/// [`SessionConfig::default`] followed by direct field mutation when wiring
/// from a config file.
///
/// Cloning is cheap — all fields are owned strings and copy types. The
/// expectation is to wrap in `Arc<SessionConfig>` at startup and share.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Cookie name (e.g. `"fleet_session"`).
    pub cookie_name: String,
    /// Optional `Domain=` attribute. `None` means no Domain attribute (the
    /// browser scopes to the exact origin, correct for localhost dev).
    pub domain: Option<String>,
    /// Session lifetime in seconds; sets cookie `Max-Age` and payload `exp`.
    pub ttl_secs: u64,
    /// `Secure` flag — set to `true` in production (HTTPS only).
    pub secure: bool,
    /// `SameSite` attribute. ADR-0030 mandates `Lax` for cross-subdomain SSO.
    pub same_site: cookie::SameSite,
    /// App namespace (e.g. `"trawl"`, `"coastwatch"`). Used to filter
    /// `VerifiedKey.assignments` in middleware and login handler.
    pub app_namespace: String,
    /// Path browsers redirect to after a successful login.
    pub post_login_redirect: String,
}

impl SessionConfig {
    /// Validate-and-construct.
    ///
    /// # Errors
    /// Returns [`crate::AuthError::InvalidApp`] when `cookie_name` is empty,
    /// `post_login_redirect` doesn't start with `/`, or `app_namespace` fails
    /// [`validate_app_namespace`].
    pub fn new(
        cookie_name: impl Into<String>,
        app_namespace: impl Into<String>,
    ) -> Result<Self, crate::AuthError> {
        let cfg = Self {
            cookie_name: cookie_name.into(),
            app_namespace: app_namespace.into(),
            ..Self::default()
        };
        cfg.validate()?;
        Ok(cfg)
    }

    /// Validate the configuration. Called by [`Self::new`]; also expose for
    /// callers that build via [`Self::default`] + field assignment.
    ///
    /// # Errors
    /// Returns [`crate::AuthError::InvalidApp`] when `cookie_name` is empty,
    /// `post_login_redirect` doesn't start with `/`, or `app_namespace`
    /// fails [`validate_app_namespace`].
    pub fn validate(&self) -> Result<(), crate::AuthError> {
        if self.cookie_name.is_empty() {
            return Err(crate::AuthError::InvalidApp(
                "cookie_name must not be empty".into(),
            ));
        }
        if !self.post_login_redirect.starts_with('/') {
            return Err(crate::AuthError::InvalidApp(format!(
                "post_login_redirect must start with '/', got: {}",
                self.post_login_redirect
            )));
        }
        validate_app_namespace(&self.app_namespace)?;
        Ok(())
    }
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            cookie_name: DEFAULT_COOKIE_NAME.to_owned(),
            domain: None,
            ttl_secs: DEFAULT_TTL_SECS,
            secure: true,
            // ADR-0030: Lax is required for parent-domain SSO. Strict would
            // silently break cross-subdomain navigation.
            same_site: cookie::SameSite::Lax,
            // app_namespace MUST be set explicitly — empty string fails
            // validate(), which is intentional.
            app_namespace: String::new(),
            post_login_redirect: "/".to_owned(),
        }
    }
}

/// Build a `Set-Cookie` header value for a live session cookie.
///
/// Delegates to the `cookie` crate for attribute serialization. The `domain`
/// argument controls the `Domain=` attribute: `Some(".fleet.home.lan")`
/// emits `Domain=.fleet.home.lan`, `None` omits the attribute entirely
/// (browser scopes to the exact origin — correct for `localhost`).
#[must_use]
pub fn build_session_cookie_header(
    name: &str,
    value: String,
    max_age_secs: u64,
    secure: bool,
    same_site: cookie::SameSite,
    domain: Option<&str>,
) -> String {
    let mut builder = cookie::Cookie::build((name.to_owned(), value))
        .http_only(true)
        .same_site(same_site)
        .path("/")
        .max_age(cookie::time::Duration::seconds(
            i64::try_from(max_age_secs).unwrap_or(i64::MAX),
        ))
        .secure(secure);
    if let Some(d) = domain {
        builder = builder.domain(d.to_owned());
    }
    builder.to_string()
}

/// Build a `Set-Cookie` header value that clears the named cookie.
///
/// Attributes (`HttpOnly`, `SameSite`, `Path`, optional `Secure`, optional
/// `Domain`) MUST match what the original `Set-Cookie` had — browsers reject
/// clear directives that don't match the issued attributes. `Max-Age=0`
/// signals immediate deletion.
#[must_use]
pub fn build_clear_cookie_header(
    name: &str,
    secure: bool,
    same_site: cookie::SameSite,
    domain: Option<&str>,
) -> String {
    let mut builder = cookie::Cookie::build((name.to_owned(), String::new()))
        .http_only(true)
        .same_site(same_site)
        .path("/")
        .max_age(cookie::time::Duration::ZERO)
        .secure(secure);
    if let Some(d) = domain {
        builder = builder.domain(d.to_owned());
    }
    builder.to_string()
}

/// Encrypt a session payload into a base64url cookie value.
///
/// Output is `base64url(nonce || ciphertext || tag)` with no padding.
///
/// # Errors
/// Returns `Json` if serialization fails (effectively never for valid
/// payloads) or `Aead` if the AEAD primitive refuses to encrypt (also
/// effectively never).
pub fn encrypt(key: &SessionKey, payload: &SessionPayload) -> Result<String, SessionError> {
    let plaintext = serde_json::to_vec(payload).map_err(|e| SessionError::Json(e.to_string()))?;

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

    let payload: SessionPayload =
        serde_json::from_slice(&plaintext).map_err(|e| SessionError::Json(e.to_string()))?;
    Ok(payload)
}

/// Check whether a payload's `exp` is earlier than or equal to `now`.
///
/// Callers supply `now` explicitly so that tests aren't clock-dependent and
/// so middleware can decide once-per-request what "now" means.
#[must_use]
pub fn is_expired(payload: &SessionPayload, now: i64) -> bool {
    payload.exp <= now
}

/// Serde adapter so `Zeroizing<String>` round-trips as a plain JSON string.
///
/// `pub(crate)` so other modules in the crate (notably `handlers::LoginRequest`)
/// can reuse the same `deserialize_with` adapter rather than duplicating it.
pub(crate) mod zeroizing_string {
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
        // boundary: exp == now is considered expired (inclusive)
        assert!(is_expired(&payload, 100));
    }

    #[test]
    fn payload_serde_drops_no_fields() {
        // SessionPayload has only {token, name, exp} — no `role`.
        // If a future change adds a field, the JSON shape changes and this
        // test prompts a deliberate decision about cookie back-compat.
        let payload = sample_payload();
        let json = serde_json::to_value(&payload).unwrap();
        let obj = json.as_object().expect("payload serialises as object");
        assert_eq!(
            obj.len(),
            3,
            "expected 3 fields, got {}: {obj:?}",
            obj.len()
        );
        assert!(obj.contains_key("token"));
        assert!(obj.contains_key("name"));
        assert!(obj.contains_key("exp"));
        assert!(
            !obj.contains_key("role"),
            "role must NOT be in payload (ADR-0030)"
        );
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

    #[test]
    fn key_from_base64_accepts_all_four_flavors() {
        let original = SessionKey::generate();
        let bytes = original.0.as_ref();

        for encoded in [
            Base64UrlUnpadded::encode_string(bytes),
            Base64Url::encode_string(bytes),
            Base64Unpadded::encode_string(bytes),
            Base64::encode_string(bytes),
        ] {
            let loaded = SessionKey::from_base64(&encoded)
                .unwrap_or_else(|e| panic!("decode failed for {encoded:?}: {e:?}"));
            let cookie = encrypt(&original, &sample_payload()).unwrap();
            let decoded = decrypt(&loaded, &cookie).unwrap();
            assert_eq!(decoded.name, "alice");
        }
    }

    #[test]
    fn session_cookie_header_includes_domain_when_set() {
        // `.fleet.home.lan` (legacy leading dot) and `fleet.home.lan` are
        // semantically identical per RFC 6265; the `cookie` crate normalises
        // both to the dotless form. Either input is acceptable; we assert on
        // the normalised emit.
        let header = build_session_cookie_header(
            "fleet_session",
            "abc".to_string(),
            3600,
            true,
            cookie::SameSite::Lax,
            Some(".fleet.home.lan"),
        );
        assert!(header.contains("Domain=fleet.home.lan"), "got: {header}");
        assert!(header.contains("SameSite=Lax"));
        assert!(header.contains("HttpOnly"));
        assert!(header.contains("Secure"));
        assert!(header.contains("Path=/"));
        assert!(header.contains("Max-Age=3600"));
    }

    #[test]
    fn session_cookie_header_omits_domain_when_none() {
        let header = build_session_cookie_header(
            "fleet_session",
            "abc".to_string(),
            3600,
            false,
            cookie::SameSite::Lax,
            None,
        );
        assert!(!header.to_lowercase().contains("domain="), "got: {header}");
        assert!(!header.contains("Secure"), "got: {header}");
    }

    #[test]
    fn session_cookie_header_respects_strict_same_site() {
        let header = build_session_cookie_header(
            "trawl_session",
            "x".to_string(),
            60,
            true,
            cookie::SameSite::Strict,
            None,
        );
        assert!(header.contains("SameSite=Strict"), "got: {header}");
    }

    #[test]
    fn clear_cookie_header_matches_session_attributes() {
        // Browsers reject clear directives that don't match the original
        // attribute set — same name/path/domain/samesite are mandatory.
        let header = build_clear_cookie_header(
            "fleet_session",
            true,
            cookie::SameSite::Lax,
            Some(".fleet.home.lan"),
        );
        assert!(header.starts_with("fleet_session=;"), "got: {header}");
        assert!(header.contains("Max-Age=0"));
        assert!(header.contains("Domain=fleet.home.lan"));
        assert!(header.contains("SameSite=Lax"));
        assert!(header.contains("Path=/"));
        assert!(header.contains("HttpOnly"));
        assert!(header.contains("Secure"));
    }

    #[test]
    fn session_config_default_is_sso_friendly() {
        let cfg = SessionConfig::default();
        assert_eq!(cfg.cookie_name, "fleet_session");
        assert!(cfg.domain.is_none());
        assert_eq!(cfg.ttl_secs, DEFAULT_TTL_SECS);
        assert!(cfg.secure);
        assert_eq!(cfg.same_site, cookie::SameSite::Lax);
        assert_eq!(cfg.post_login_redirect, "/");
        // app_namespace is empty by design — validate() must reject it so
        // callers can't accidentally ship a wide-open default.
        assert!(cfg.app_namespace.is_empty());
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn session_config_new_validates_namespace() {
        let cfg = SessionConfig::new("fleet_session", "trawl").unwrap();
        assert_eq!(cfg.app_namespace, "trawl");
        assert!(cfg.validate().is_ok());

        let err = SessionConfig::new("fleet_session", "BAD!").unwrap_err();
        assert!(matches!(err, crate::AuthError::InvalidApp(_)));
    }

    #[test]
    fn session_config_rejects_empty_cookie_name() {
        let cfg = SessionConfig {
            cookie_name: String::new(),
            app_namespace: "trawl".to_owned(),
            ..SessionConfig::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, crate::AuthError::InvalidApp(m) if m.contains("cookie_name")));
    }

    #[test]
    fn session_config_rejects_relative_redirect() {
        let cfg = SessionConfig {
            post_login_redirect: "home".to_owned(),
            app_namespace: "trawl".to_owned(),
            ..SessionConfig::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(
            matches!(err, crate::AuthError::InvalidApp(m) if m.contains("post_login_redirect"))
        );
    }
}
