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

/// Default cookie name used by [`SessionConfigBuilder`] when
/// [`SessionConfigBuilder::cookie_name`] is not set.
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
    /// be a semver-breaking change.
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
    /// not base64). Trailing newlines are NOT trimmed.
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

    /// Encode the key as base64url (no padding) — the format
    /// [`Self::from_base64`] reads back.
    ///
    /// Returns a [`Zeroizing`] string so the encoded form is scrubbed from
    /// memory when the binding goes out of scope. Print it immediately and
    /// move on; do not stash it in a long-lived `String`.
    pub fn to_base64url(&self) -> Zeroizing<String> {
        Zeroizing::new(Base64UrlUnpadded::encode_string(self.0.as_ref()))
    }
}

impl std::fmt::Debug for SessionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionKey")
            .field("len", &KEY_LEN)
            .finish_non_exhaustive()
    }
}

/// Absolute unix-second expiry timestamp.
///
/// Newtype around the raw second count so callers can't accidentally mix
/// seconds with milliseconds — the `coastwatch-web` consumer is the
/// second to use this module and one stray `.timestamp_millis()` would
/// produce sessions that live 1000× too long without any compile-time
/// or runtime signal.
///
/// `#[serde(transparent)]` keeps the on-the-wire shape a bare integer
/// so existing encrypted cookies still decrypt unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionExpiry(i64);

impl SessionExpiry {
    /// Construct from an absolute unix-second timestamp.
    #[must_use]
    pub const fn from_unix_seconds(secs: i64) -> Self {
        Self(secs)
    }

    /// `now + duration_secs`, saturating on overflow.
    #[must_use]
    pub const fn after_duration(now: i64, duration_secs: i64) -> Self {
        Self(now.saturating_add(duration_secs))
    }

    /// As a raw unix-second timestamp (for serialisation, comparisons,
    /// or tracing field formatting).
    #[must_use]
    pub const fn as_unix_seconds(self) -> i64 {
        self.0
    }

    /// Has this expiry passed at `now`?
    ///
    /// Uses `<` (strictly earlier) so the cookie is valid up to and
    /// *including* the `exp` second — see [`is_expired`] for the
    /// boundary rationale.
    #[must_use]
    pub const fn is_past(self, now: i64) -> bool {
        self.0 < now
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
    pub exp: SessionExpiry,
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
/// Construction goes through [`SessionConfig::builder`] (or
/// [`SessionConfig::new`] for the two-required-fields-only common case).
/// Both paths run [`SessionConfig::validate`] before handing back a
/// `SessionConfig`, so a value of this type is always known-valid.
///
/// Fields are `pub(crate)` and the struct is `#[non_exhaustive]` so
/// external callers can neither construct via struct literal nor mutate
/// post-construction — the only way to set a value is via the builder,
/// which keeps validation invariants enforced.
///
/// Cloning is cheap — all fields are owned strings and copy types. The
/// expectation is to wrap in `Arc<SessionConfig>` at startup and share.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SessionConfig {
    pub(crate) cookie_name: String,
    pub(crate) domain: Option<String>,
    pub(crate) ttl_secs: u64,
    pub(crate) secure: bool,
    pub(crate) same_site: cookie::SameSite,
    pub(crate) app_namespace: String,
    pub(crate) post_login_redirect: String,
}

impl SessionConfig {
    /// Start a builder with SSO-friendly defaults (SameSite=Lax, secure=true,
    /// 24h TTL, cookie name `"fleet_session"`, post-login redirect `"/"`).
    #[must_use]
    pub fn builder() -> SessionConfigBuilder {
        SessionConfigBuilder::default()
    }

    /// Convenience for the common "just need the two required fields"
    /// case. Equivalent to
    /// `Self::builder().cookie_name(...).app_namespace(...).build()`.
    ///
    /// # Errors
    /// As for [`SessionConfigBuilder::build`].
    pub fn new(
        cookie_name: impl Into<String>,
        app_namespace: impl Into<String>,
    ) -> Result<Self, crate::AuthError> {
        Self::builder()
            .cookie_name(cookie_name)
            .app_namespace(app_namespace)
            .build()
    }

    // -- read accessors --

    /// Cookie name (e.g. `"fleet_session"`).
    #[must_use]
    pub fn cookie_name(&self) -> &str {
        &self.cookie_name
    }

    /// Optional `Domain=` attribute. See [`SessionConfigBuilder::domain`]
    /// for the silent-misconfig warning.
    #[must_use]
    pub fn domain(&self) -> Option<&str> {
        self.domain.as_deref()
    }

    /// Session lifetime in seconds; sets cookie `Max-Age` and payload `exp`.
    #[must_use]
    pub const fn ttl_secs(&self) -> u64 {
        self.ttl_secs
    }

    /// `Secure` flag — `true` in production (HTTPS only).
    #[must_use]
    pub const fn secure(&self) -> bool {
        self.secure
    }

    /// `SameSite` attribute.
    #[must_use]
    pub const fn same_site(&self) -> cookie::SameSite {
        self.same_site
    }

    /// App namespace (e.g. `"trawl"`, `"coastwatch"`).
    #[must_use]
    pub fn app_namespace(&self) -> &str {
        &self.app_namespace
    }

    /// Path browsers redirect to after a successful login.
    #[must_use]
    pub fn post_login_redirect(&self) -> &str {
        &self.post_login_redirect
    }

    /// Validate the configuration. The builder calls this in
    /// [`SessionConfigBuilder::build`]; [`SessionState::new`] also calls
    /// it defensively in case a future internal path constructs a
    /// `SessionConfig` without going through the builder.
    ///
    /// # Errors
    /// Returns [`crate::AuthError::InvalidApp`] when `cookie_name` is empty,
    /// `post_login_redirect` doesn't start with `/` or is otherwise unsafe,
    /// `same_site == None` without `secure`, or `app_namespace` fails
    /// [`validate_app_namespace`].
    ///
    /// [`SessionState::new`]: crate::middleware::SessionState::new
    pub fn validate(&self) -> Result<(), crate::AuthError> {
        if self.cookie_name.is_empty() {
            return Err(crate::AuthError::InvalidApp(
                "cookie_name must not be empty".into(),
            ));
        }
        validate_redirect_path(&self.post_login_redirect)?;
        // SameSite=None requires Secure on modern browsers; without it the
        // cookie is silently treated as Lax (Chrome 80+) — a misconfig the
        // caller almost certainly didn't intend. Fail fast at construction.
        if self.same_site == cookie::SameSite::None && !self.secure {
            return Err(crate::AuthError::InvalidApp(
                "SameSite=None requires Secure (browsers reject otherwise)".into(),
            ));
        }
        validate_app_namespace(&self.app_namespace)?;
        Ok(())
    }
}

/// Builder for [`SessionConfig`].
///
/// SSO-friendly defaults: `cookie_name = "fleet_session"`, `secure = true`,
/// `same_site = Lax`, `ttl_secs = DEFAULT_TTL_SECS`, no `Domain`, and
/// `post_login_redirect = "/"`. The two required setters are
/// [`Self::cookie_name`] (must be non-empty) and [`Self::app_namespace`]
/// (must pass [`validate_app_namespace`]).
///
/// Validation runs in [`Self::build`], so unset/invalid fields surface as a
/// proper `Result` rather than a panic.
#[derive(Debug, Default, Clone)]
pub struct SessionConfigBuilder {
    cookie_name: Option<String>,
    domain: Option<String>,
    ttl_secs: Option<u64>,
    secure: Option<bool>,
    same_site: Option<cookie::SameSite>,
    app_namespace: Option<String>,
    post_login_redirect: Option<String>,
}

impl SessionConfigBuilder {
    /// Set the cookie name. Required; `build()` fails on empty.
    #[must_use]
    pub fn cookie_name(mut self, v: impl Into<String>) -> Self {
        self.cookie_name = Some(v.into());
        self
    }

    /// Set the `Domain=` cookie attribute.
    ///
    /// Mismatched values silently break: browsers accept the
    /// `Set-Cookie` but refuse to send the cookie back, producing
    /// "login succeeds, every subsequent request 401s, nothing in logs".
    /// Match the host the browser actually sees in the URL bar, or use
    /// [`Self::no_domain`] (the default) and let the browser scope to
    /// the origin.
    #[must_use]
    pub fn domain(mut self, v: impl Into<String>) -> Self {
        self.domain = Some(v.into());
        self
    }

    /// Explicitly clear any `Domain=` attribute previously set on the
    /// builder. Equivalent to leaving it unset; provided so a config-file
    /// loader can reset a value without calling [`Self::domain`] with an
    /// empty string.
    #[must_use]
    pub fn no_domain(mut self) -> Self {
        self.domain = None;
        self
    }

    /// Session lifetime in seconds.
    #[must_use]
    pub const fn ttl_secs(mut self, v: u64) -> Self {
        self.ttl_secs = Some(v);
        self
    }

    /// `Secure` cookie flag — set to `true` in production (HTTPS only).
    #[must_use]
    pub const fn secure(mut self, v: bool) -> Self {
        self.secure = Some(v);
        self
    }

    /// `SameSite` cookie attribute.
    #[must_use]
    pub const fn same_site(mut self, v: cookie::SameSite) -> Self {
        self.same_site = Some(v);
        self
    }

    /// App namespace, e.g. `"trawl"` or `"coastwatch"`. Required.
    #[must_use]
    pub fn app_namespace(mut self, v: impl Into<String>) -> Self {
        self.app_namespace = Some(v.into());
        self
    }

    /// Path browsers redirect to after a successful login. Must start
    /// with `/`, must not be protocol-relative.
    #[must_use]
    pub fn post_login_redirect(mut self, v: impl Into<String>) -> Self {
        self.post_login_redirect = Some(v.into());
        self
    }

    /// Build and validate.
    ///
    /// # Errors
    /// Returns [`crate::AuthError::InvalidApp`] when validation fails
    /// (empty `cookie_name`, invalid `post_login_redirect`,
    /// `same_site == None` without `secure`, or invalid `app_namespace`).
    pub fn build(self) -> Result<SessionConfig, crate::AuthError> {
        let cfg = SessionConfig {
            cookie_name: self
                .cookie_name
                .unwrap_or_else(|| DEFAULT_COOKIE_NAME.to_owned()),
            domain: self.domain,
            ttl_secs: self.ttl_secs.unwrap_or(DEFAULT_TTL_SECS),
            secure: self.secure.unwrap_or(true),
            // ADR-0030: Lax is required for parent-domain SSO. Strict would
            // silently break cross-subdomain navigation.
            same_site: self.same_site.unwrap_or(cookie::SameSite::Lax),
            app_namespace: self.app_namespace.unwrap_or_default(),
            post_login_redirect: self.post_login_redirect.unwrap_or_else(|| "/".to_owned()),
        };
        cfg.validate()?;
        Ok(cfg)
    }
}

/// Validate that a redirect path is safe to use in a `Location:` header.
///
/// Rejects:
/// - paths that don't start with `/` (relative — wrong shape for `Location`)
/// - protocol-relative `//host` paths (browser follows them as external)
/// - any byte that would CRLF-inject or break header framing
fn validate_redirect_path(path: &str) -> Result<(), crate::AuthError> {
    if !path.starts_with('/') {
        return Err(crate::AuthError::InvalidApp(format!(
            "post_login_redirect must start with '/', got: {path}"
        )));
    }
    if path.starts_with("//") {
        return Err(crate::AuthError::InvalidApp(format!(
            "post_login_redirect must not be protocol-relative, got: {path}"
        )));
    }
    // Browsers normalise `\` → `/` in URL parsing (WHATWG URL spec), so
    // `/\evil.com/path` is interpreted as a protocol-relative redirect to
    // evil.com. Close the gap.
    if path.starts_with("/\\") {
        return Err(crate::AuthError::InvalidApp(format!(
            "post_login_redirect must not be protocol-relative (backslash-normalised), got: {path}"
        )));
    }
    if path
        .bytes()
        .any(|b| b == b'\r' || b == b'\n' || b == 0 || b == 0x7f)
    {
        return Err(crate::AuthError::InvalidApp(
            "post_login_redirect contains control characters".into(),
        ));
    }
    Ok(())
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
    // Wrap the serialised plaintext in Zeroizing so the bearer token bytes
    // don't linger on the heap after the function returns (ADR-0030: treat
    // tokens as secrets, scrub on drop).
    let plaintext =
        Zeroizing::new(serde_json::to_vec(payload).map_err(|e| SessionError::Json(e.to_string()))?);

    let cipher = XChaCha20Poly1305::new(key.0.as_ref().into());

    let mut nonce_bytes = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = XNonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_slice())
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
    // Wrap decrypted plaintext in Zeroizing so the bearer token bytes are
    // wiped after we parse out the SessionPayload (whose .token is itself
    // Zeroizing<String> with the same posture).
    let plaintext = Zeroizing::new(
        cipher
            .decrypt(nonce, ciphertext)
            .map_err(|_| SessionError::Aead)?,
    );

    let payload: SessionPayload =
        serde_json::from_slice(&plaintext).map_err(|e| SessionError::Json(e.to_string()))?;
    Ok(payload)
}

/// Present-only Origin validation for login/logout endpoints (ADR-0004
/// slice 2).
///
/// The shared `fleet_session` cookie makes logout forgeable cross-site: a
/// forged POST to any fleet app's logout endpoint would clear the cookie
/// for every sibling app. This helper closes that hole while keeping
/// curl/scripted clients working:
///
/// - `origin` **absent** → allow. Browsers always send `Origin` on
///   cross-site POSTs, so the attack is blocked; non-browser clients
///   (which send no `Origin`) keep working.
/// - `origin` host equals the request `host` (ports stripped,
///   case-insensitive) → allow.
/// - Malformed `origin` (including the opaque `"null"` origin) → reject,
///   fail closed.
/// - Anything else → reject.
///
/// Sharing a parent-domain cookie is **not** an origin allowlist: a
/// sibling fleet app (`evil.fleet.example` posting to
/// `trawl.fleet.example/logout`) is a *different* origin and must be
/// rejected even though both sit under the cookie's `shared_domain`.
/// Otherwise any compromised sibling — or attacker-hosted content on one —
/// could auto-submit a form POST that clears `fleet_session` fleet-wide.
/// So origin validation is strictly same-host; the shared domain governs
/// only the cookie's `Domain=` attribute, never who may hit auth endpoints.
///
/// Pure string parsing — no request types — so both fleet-auth's own
/// handlers and thin proxies that only take the `session` feature call
/// the literally-same function instead of growing diverged copies.
///
/// Note: the exact-host arm trusts the request `Host` header. A reverse
/// proxy in front MUST forward the original `Host` or legitimate
/// same-origin requests will be rejected.
#[must_use]
pub fn origin_allowed(origin: Option<&str>, host: Option<&str>) -> bool {
    let Some(origin) = origin else {
        return true;
    };
    let Some(origin_host) = origin_host(origin) else {
        return false;
    };

    match host {
        Some(request_host) => origin_host.eq_ignore_ascii_case(strip_port(request_host)),
        None => false,
    }
}

/// Returned by [`check_origin`] when a request's `Origin` is rejected. The
/// rejection has already been logged with its `origin`/`host`/`handler`
/// fields; each caller maps this marker onto its own error/response type
/// (403, no `Set-Cookie`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OriginRejected;

/// Present-only Origin guard for state-changing auth endpoints, wrapping
/// [`origin_allowed`] with the canonical rejection log.
///
/// Callers pass the raw `Origin`/`Host` header values (already `Option<&str>`)
/// rather than a request type, so this stays usable from both fleet-auth's
/// own axum handlers and thin proxies that take only the `session` feature.
/// On rejection it emits the shared `tracing::warn!` — the log fields and
/// message live in **one** place so they can't drift between call sites — and
/// returns [`OriginRejected`]. `handler` labels the endpoint (`"login"` /
/// `"logout"`) in the log line.
///
/// # Errors
///
/// Returns [`OriginRejected`] when [`origin_allowed`] rejects the pair.
pub fn check_origin(
    origin: Option<&str>,
    host: Option<&str>,
    handler: &str,
) -> Result<(), OriginRejected> {
    if origin_allowed(origin, host) {
        return Ok(());
    }
    tracing::warn!(
        origin = origin.unwrap_or("<unparseable>"),
        host = host.unwrap_or("<none>"),
        handler,
        "auth: cross-origin request rejected"
    );
    Err(OriginRejected)
}

/// Extract the host component from an `Origin` header value
/// (`scheme "://" host [":" port]`). Returns `None` for anything that
/// doesn't parse as a serialized origin — including the opaque `"null"`
/// origin — so callers fail closed.
fn origin_host(origin: &str) -> Option<&str> {
    let (scheme, rest) = origin.split_once("://")?;
    if scheme.is_empty()
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return None;
    }
    // A serialized origin has no path, query, fragment, or userinfo.
    if rest.is_empty() || rest.contains(['/', '\\', '?', '#', '@']) {
        return None;
    }
    let host = strip_port(rest);
    if host.is_empty() { None } else { Some(host) }
}

/// Strip a trailing `:port` from a host, handling bracketed IPv6
/// literals (`[::1]:8080` → `::1`). Values without a valid numeric port
/// pass through unchanged.
fn strip_port(host: &str) -> &str {
    if let Some(rest) = host.strip_prefix('[') {
        // IPv6 literal: everything up to the closing bracket.
        if let Some(end) = rest.find(']') {
            return &rest[..end];
        }
        return host; // malformed — compare as-is, will simply not match
    }
    match host.rsplit_once(':') {
        Some((h, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => host,
    }
}

/// Check whether `now` is past a payload's `exp`.
///
/// Uses `<` (strictly earlier) so the cookie is valid up to and *including*
/// the `exp` second. With `<=` a session created at second `N` with
/// `ttl_secs = 1` would be rejected if the verifying request landed in the
/// same second — tight, but it does happen at scale.
///
/// Callers supply `now` explicitly so that tests aren't clock-dependent and
/// so middleware can decide once-per-request what "now" means.
#[must_use]
pub fn is_expired(payload: &SessionPayload, now: i64) -> bool {
    payload.exp.is_past(now)
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
            exp: SessionExpiry::from_unix_seconds(1_700_000_000),
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
        assert_eq!(decoded.exp.as_unix_seconds(), 1_700_000_000);
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
            exp: SessionExpiry::from_unix_seconds(100),
            ..sample_payload()
        };
        assert!(is_expired(&payload, 200));
        assert!(!is_expired(&payload, 50));
        // boundary: exp == now is NOT expired (`<` semantics — see is_expired
        // doc comment). The cookie is valid up to and including `exp`.
        assert!(!is_expired(&payload, 100));
        assert!(is_expired(&payload, 101));
    }

    #[test]
    fn session_expiry_after_duration_saturates() {
        // Headroom enough that i64::MAX is well past any realistic ttl,
        // but assert saturation explicitly so a future refactor can't
        // sneak in a panic on overflow.
        let expiry = SessionExpiry::after_duration(i64::MAX - 5, 100);
        assert_eq!(expiry.as_unix_seconds(), i64::MAX);
    }

    #[test]
    fn session_expiry_serde_is_bare_integer() {
        // #[serde(transparent)] keeps the wire format a bare i64, so
        // SessionPayload JSON shape stays back-compatible with cookies
        // written by earlier builds.
        let expiry = SessionExpiry::from_unix_seconds(1_700_000_000);
        let json = serde_json::to_string(&expiry).unwrap();
        assert_eq!(json, "1700000000");
        let back: SessionExpiry = serde_json::from_str("1700000000").unwrap();
        assert_eq!(back, expiry);
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
    fn to_base64url_roundtrips_through_from_base64() {
        let original = SessionKey::generate();
        let encoded = original.to_base64url();

        assert!(!encoded.contains('+'));
        assert!(!encoded.contains('/'));
        assert!(!encoded.contains('='));
        // 32 bytes -> ceil(32 * 4 / 3) -> 43 chars unpadded.
        assert_eq!(encoded.len(), 43);

        let loaded = SessionKey::from_base64(encoded.as_str()).unwrap();
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
    fn session_config_builder_emits_sso_friendly_defaults() {
        let cfg = SessionConfig::builder()
            .app_namespace("trawl")
            .build()
            .unwrap();
        assert_eq!(cfg.cookie_name(), "fleet_session");
        assert!(cfg.domain().is_none());
        assert_eq!(cfg.ttl_secs(), DEFAULT_TTL_SECS);
        assert!(cfg.secure());
        assert_eq!(cfg.same_site(), cookie::SameSite::Lax);
        assert_eq!(cfg.post_login_redirect(), "/");
        assert_eq!(cfg.app_namespace(), "trawl");
    }

    #[test]
    fn session_config_builder_rejects_empty_namespace() {
        // app_namespace defaults to empty — validate() must reject it so
        // callers can't accidentally ship a wide-open default.
        let err = SessionConfig::builder().build().unwrap_err();
        assert!(matches!(err, crate::AuthError::InvalidApp(_)));
    }

    #[test]
    fn session_config_new_validates_namespace() {
        let cfg = SessionConfig::new("fleet_session", "trawl").unwrap();
        assert_eq!(cfg.app_namespace(), "trawl");

        let err = SessionConfig::new("fleet_session", "BAD!").unwrap_err();
        assert!(matches!(err, crate::AuthError::InvalidApp(_)));
    }

    #[test]
    fn session_config_rejects_empty_cookie_name() {
        let err = SessionConfig::builder()
            .cookie_name("")
            .app_namespace("trawl")
            .build()
            .unwrap_err();
        assert!(matches!(err, crate::AuthError::InvalidApp(m) if m.contains("cookie_name")));
    }

    #[test]
    fn session_config_rejects_relative_redirect() {
        let err = SessionConfig::builder()
            .app_namespace("trawl")
            .post_login_redirect("home")
            .build()
            .unwrap_err();
        assert!(
            matches!(&err, crate::AuthError::InvalidApp(m) if m.contains("post_login_redirect"))
        );
    }

    #[test]
    fn session_config_rejects_protocol_relative_redirect() {
        let err = SessionConfig::builder()
            .app_namespace("trawl")
            .post_login_redirect("//evil.example.com/path")
            .build()
            .unwrap_err();
        assert!(matches!(err, crate::AuthError::InvalidApp(m) if m.contains("protocol-relative")));
    }

    #[test]
    fn session_config_rejects_backslash_protocol_relative_redirect() {
        // Chrome/Firefox normalise `\` → `/`, so /\evil.com is read as //evil.com.
        for bad in ["/\\evil.example.com/path", "/\\\\evil.example.com"] {
            let err = SessionConfig::builder()
                .app_namespace("trawl")
                .post_login_redirect(bad)
                .build()
                .unwrap_err();
            assert!(
                matches!(&err, crate::AuthError::InvalidApp(m) if m.contains("protocol-relative")),
                "expected protocol-relative rejection for {bad:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn session_config_rejects_crlf_in_redirect() {
        for bad in ["/ok\r\nX-Injected: 1", "/ok\nfoo", "/ok\0foo", "/ok\x7f"] {
            let err = SessionConfig::builder()
                .app_namespace("trawl")
                .post_login_redirect(bad)
                .build()
                .unwrap_err();
            assert!(
                matches!(&err, crate::AuthError::InvalidApp(m) if m.contains("control")),
                "expected control-char rejection for {bad:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn session_config_rejects_samesite_none_without_secure() {
        let err = SessionConfig::builder()
            .app_namespace("trawl")
            .secure(false)
            .same_site(cookie::SameSite::None)
            .build()
            .unwrap_err();
        assert!(
            matches!(&err, crate::AuthError::InvalidApp(m) if m.contains("SameSite=None")),
            "got: {err:?}"
        );
    }

    // -- origin_allowed truth table -------------------------------------

    #[test]
    fn origin_absent_is_allowed() {
        // curl / scripted logins / same-origin GET navigations don't send
        // Origin — the check is present-only by design.
        assert!(origin_allowed(None, Some("trawl.example.com")));
        assert!(origin_allowed(None, None));
    }

    #[test]
    fn origin_matching_request_host_is_allowed() {
        assert!(origin_allowed(
            Some("https://trawl.example.com"),
            Some("trawl.example.com")
        ));
        // ports are stripped on both sides
        assert!(origin_allowed(
            Some("https://trawl.example.com:8443"),
            Some("trawl.example.com:8443")
        ));
        assert!(origin_allowed(
            Some("http://localhost:8090"),
            Some("localhost:8090")
        ));
        // case-insensitive host comparison
        assert!(origin_allowed(
            Some("https://Trawl.Example.COM"),
            Some("trawl.example.com")
        ));
    }

    #[test]
    fn sibling_under_shared_domain_is_rejected() {
        // A parent-domain cookie is NOT an origin allowlist: a sibling
        // fleet app posting to trawl's auth endpoints is a *different*
        // origin and must be rejected, even though both live under the
        // same `shared_domain`. Otherwise a compromised (or
        // attacker-hosted) sibling could forge a logout that clears
        // `fleet_session` fleet-wide. This is the ADR-0004-slice-2
        // regression: origin validation stays strictly same-host.
        assert!(!origin_allowed(
            Some("https://evil.fleet.lab.ktle.net"),
            Some("trawl.fleet.lab.ktle.net")
        ));
        // even the bare parent domain is a different host
        assert!(!origin_allowed(
            Some("https://fleet.lab.ktle.net"),
            Some("trawl.fleet.lab.ktle.net")
        ));
    }

    #[test]
    fn origin_mismatch_is_rejected() {
        // strict same-host mode: any other host is rejected
        assert!(!origin_allowed(
            Some("https://evil.example.com"),
            Some("trawl.example.com")
        ));
        // no Host to match against → fail closed
        assert!(!origin_allowed(Some("https://trawl.example.com"), None));
        // suffix forgery: eviltrawl.example.com is NOT trawl.example.com
        assert!(!origin_allowed(
            Some("https://eviltrawl.example.com"),
            Some("trawl.example.com")
        ));
        // port-only mismatch on the exact-host arm still passes because
        // ports are stripped (Origin comparison is host-scoped here)
        assert!(origin_allowed(
            Some("https://trawl.example.com:9999"),
            Some("trawl.example.com:8443")
        ));
    }

    #[test]
    fn origin_malformed_is_rejected() {
        // opaque "null" origin (sandboxed iframe, data: URL) — fail closed
        assert!(!origin_allowed(Some("null"), Some("trawl.example.com")));
        // no scheme
        assert!(!origin_allowed(
            Some("trawl.example.com"),
            Some("trawl.example.com")
        ));
        // garbage
        assert!(!origin_allowed(Some("https://"), Some("trawl.example.com")));
        assert!(!origin_allowed(Some(""), Some("trawl.example.com")));
        // path smuggling: Origin never carries a path
        assert!(!origin_allowed(
            Some("https://evil.com/trawl.example.com"),
            Some("trawl.example.com")
        ));
        // userinfo smuggling
        assert!(!origin_allowed(
            Some("https://trawl.example.com@evil.com"),
            Some("trawl.example.com")
        ));
    }

    #[test]
    fn session_config_no_domain_clears_domain_setter() {
        let cfg = SessionConfig::builder()
            .app_namespace("trawl")
            .domain("fleet.home.lan")
            .no_domain()
            .build()
            .unwrap();
        assert!(cfg.domain().is_none());
    }
}
