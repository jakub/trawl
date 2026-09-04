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
//!
//! The CSRF guard over that cookie lives here too. [`check_origin`] compares
//! a present `Origin` — whole: scheme, host and effective port — against the
//! app's configured [`PublicOrigins`], which [`SessionConfig`] requires
//! (ADR-0016). It used to compare the origin's host against the request's
//! `Host` header, which let `http://` forge against `https://`, let any
//! other port of the same name through, and made the answer depend on a
//! header the reverse proxy in front rewrites. No request header other than
//! `Origin` is read any more, and `crates/fleet-auth/tests/no_forwarded_trust.rs`
//! is the guard that keeps it that way.

use std::fs;
use std::path::Path;

use base64ct::{Base64, Base64Unpadded, Base64Url, Base64UrlUnpadded, Encoding};
use chacha20poly1305::aead::Aead;
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::origin::{
    Origin, PublicOrigins, PublicOriginsError, REASON_MULTIPLE_HEADERS, REASON_NON_UTF8,
    rejection_log_fields,
};
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

/// Canonical Fleet runtime environment variable carrying the shared session
/// AEAD key as base64.
///
/// Development orchestration injects this only into browser-facing processes
/// that mint or consume Fleet sessions. Application workers and API daemons
/// that do not handle the cookie must not receive it.
pub const ENV_SESSION_AEAD_KEY: &str = "FLEET_SESSION_AEAD_KEY";

/// Canonical Fleet runtime environment variable for the cookie `Domain=`
/// attribute.
///
/// An explicitly empty value means a host-only cookie (omit `Domain=`). An
/// absent variable leaves each application's production configuration
/// untouched.
pub const ENV_SESSION_COOKIE_DOMAIN: &str = "FLEET_SESSION_COOKIE_DOMAIN";

/// Canonical Fleet runtime environment variable for the cookie `Path=`
/// attribute.
///
/// Fleet sessions currently require `/`; consumers must reject other values
/// instead of silently issuing and clearing cookies with different scopes.
pub const ENV_SESSION_COOKIE_PATH: &str = "FLEET_SESSION_COOKIE_PATH";

/// Canonical Fleet runtime environment variable for the positive cookie
/// `Secure` flag. The only valid values are `true` and `false`.
pub const ENV_SESSION_COOKIE_SECURE: &str = "FLEET_SESSION_COOKIE_SECURE";

/// Canonical Fleet runtime environment variable carrying the deployment's
/// browser-visible origins as a comma-separated list, e.g.
/// `https://trawl.example.com,http://localhost:8090`.
///
/// This is the CSRF allowlist ADR-0016 compares a present `Origin` against,
/// so it is deliberately an override of the same shape as the other
/// `FLEET_SESSION_*` knobs rather than a dev-only side channel: development
/// orchestration knows the browser origin it just published (a magic-DNS
/// name and port, say) and configuration files do not. Entries are split on
/// `,` and handed to `Origin::parse` verbatim — no trimming, because a
/// space inside an entry means the operator wrote something this parser
/// will not guess at.
pub const ENV_SESSION_PUBLIC_ORIGINS: &str = "FLEET_SESSION_PUBLIC_ORIGINS";

/// Errors from parsing the `FLEET_SESSION_*` runtime environment.
#[derive(Debug, thiserror::Error)]
pub enum SessionRuntimeError {
    #[error("env var {name} is set but not valid UTF-8")]
    NotUnicode { name: &'static str },

    #[error("env var {name} holds an AEAD key that isn't valid base64 or wrong length")]
    InvalidKey { name: &'static str },

    #[error("env var {name} has an invalid Fleet session value: {reason}")]
    InvalidValue {
        name: &'static str,
        reason: &'static str,
    },

    /// One entry of the public-origin list did not parse. The index is the
    /// position in the comma-separated variable, so an operator staring at
    /// a long line knows which piece to fix.
    #[error("env var {name} entry {index} ({entry:?}) is not a valid origin: {source}")]
    InvalidOrigin {
        name: &'static str,
        index: usize,
        entry: String,
        #[source]
        source: crate::origin::OriginParseError,
    },

    /// Two entries name the same origin after normalization. Refused for
    /// the same reason the config list refuses it: the operator believes
    /// those two spellings differ, and the next spelling they add will be
    /// one that genuinely does.
    #[error("env var {name} lists entries {first} and {second} as the same origin")]
    DuplicateOrigin {
        name: &'static str,
        first: usize,
        second: usize,
    },
}

/// Cookie `Domain=` state from the runtime environment.
///
/// Deliberately three-valued so development orchestration can force a
/// host-only cookie without disturbing production configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeCookieDomain {
    /// Runtime variable absent: preserve application configuration.
    PreserveConfigured,
    /// Runtime variable present and empty: force a host-only cookie.
    HostOnly,
    /// Runtime variable supplies an explicit, validated `Domain=` value.
    Explicit(String),
}

/// Parsed common Fleet session runtime environment.
///
/// This is the single shared implementation of the `FLEET_SESSION_*`
/// contract; consuming applications map it onto their own configuration
/// rather than re-implementing the parsing and drifting.
#[derive(Debug)]
pub struct SessionRuntimeOverrides {
    pub key: Option<SessionKey>,
    pub domain: RuntimeCookieDomain,
    pub secure: Option<bool>,
    /// The CSRF allowlist from the environment (ADR-0016), `None` when the
    /// variable is absent. A present value REPLACES whatever the config
    /// file states — it never merges, because a merged allowlist would let
    /// a stale config entry keep authorizing an origin the operator thinks
    /// they moved away from. The consuming application is where that
    /// displacement gets its warning, next to the other `FLEET_SESSION_*`
    /// overrides it already warns about.
    pub public_origins: Option<PublicOrigins>,
}

impl SessionRuntimeOverrides {
    /// Read and validate the `FLEET_SESSION_*` variables from the process
    /// environment.
    ///
    /// # Errors
    /// Fails closed on any present-but-invalid value.
    pub fn from_process_env() -> Result<Self, SessionRuntimeError> {
        Self::parse(
            read_optional_env(ENV_SESSION_AEAD_KEY)?,
            read_optional_env(ENV_SESSION_COOKIE_DOMAIN)?,
            read_optional_env(ENV_SESSION_COOKIE_PATH)?,
            read_optional_env(ENV_SESSION_COOKIE_SECURE)?,
            read_optional_env(ENV_SESSION_PUBLIC_ORIGINS)?,
        )
    }

    /// Validate already-read variable values (`None` = absent).
    ///
    /// # Errors
    /// Fails closed on any present-but-invalid value.
    pub fn parse(
        key: Option<String>,
        domain: Option<String>,
        path: Option<String>,
        secure: Option<String>,
        public_origins: Option<String>,
    ) -> Result<Self, SessionRuntimeError> {
        let key = key
            .map(Zeroizing::new)
            .map(|raw| {
                SessionKey::from_base64(raw.as_str()).map_err(|_| SessionRuntimeError::InvalidKey {
                    name: ENV_SESSION_AEAD_KEY,
                })
            })
            .transpose()?;

        if let Some(path) = path
            && path != "/"
        {
            return Err(SessionRuntimeError::InvalidValue {
                name: ENV_SESSION_COOKIE_PATH,
                reason: "only the fleet-wide path `/` is supported",
            });
        }

        let secure = secure
            .map(|value| match value.as_str() {
                "true" => Ok(true),
                "false" => Ok(false),
                _ => Err(SessionRuntimeError::InvalidValue {
                    name: ENV_SESSION_COOKIE_SECURE,
                    reason: "expected exactly `true` or `false`",
                }),
            })
            .transpose()?;

        let domain = match domain {
            None => RuntimeCookieDomain::PreserveConfigured,
            Some(domain) if domain.is_empty() => RuntimeCookieDomain::HostOnly,
            Some(domain) => {
                validate_runtime_cookie_domain(&domain)?;
                RuntimeCookieDomain::Explicit(domain)
            }
        };

        let public_origins = public_origins
            .map(|raw| parse_env_public_origins(&raw))
            .transpose()?;

        Ok(Self {
            key,
            domain,
            secure,
            public_origins,
        })
    }
}

/// Read the comma-separated public-origin list from its environment
/// variable through the ONE origin parser (ADR-0016).
///
/// The entries reach [`Origin::parse`] exactly as written. Trimming would
/// be the parser quietly repairing input, and this parser refuses instead:
/// a config file and an environment variable that normalized differently
/// would be two allowlists wearing one name.
fn parse_env_public_origins(raw: &str) -> Result<PublicOrigins, SessionRuntimeError> {
    let name = ENV_SESSION_PUBLIC_ORIGINS;
    PublicOrigins::parse(raw.split(',')).map_err(|err| match err {
        PublicOriginsError::Entry {
            index,
            entry,
            source,
        } => SessionRuntimeError::InvalidOrigin {
            name,
            index,
            entry,
            source,
        },
        PublicOriginsError::Duplicate { first, second } => SessionRuntimeError::DuplicateOrigin {
            name,
            first,
            second,
        },
        // Unreachable: `split(',')` always yields at least one element, so
        // even an empty variable arrives as one empty entry and is refused
        // by `Origin::parse`. Mapped rather than unwrapped because a
        // panic here would be a startup crash on operator input.
        PublicOriginsError::Empty => SessionRuntimeError::InvalidValue {
            name,
            reason: "expected a comma-separated list of browser-visible origins",
        },
    })
}

fn read_optional_env(name: &'static str) -> Result<Option<String>, SessionRuntimeError> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(SessionRuntimeError::NotUnicode { name }),
    }
}

/// Validate a runtime-provided cookie domain before it can reach a response
/// header. A leading dot is permitted for parity with existing production
/// configuration; the remaining value must be a conventional ASCII DNS name.
fn validate_runtime_cookie_domain(domain: &str) -> Result<(), SessionRuntimeError> {
    let bare = domain.strip_prefix('.').unwrap_or(domain);
    let valid = !bare.is_empty()
        && bare.len() <= 253
        && !bare.ends_with('.')
        && bare.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && label
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && label
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
        });
    if valid {
        Ok(())
    } else {
        Err(SessionRuntimeError::InvalidValue {
            name: ENV_SESSION_COOKIE_DOMAIN,
            reason: "expected empty for host-only or an ASCII DNS domain",
        })
    }
}

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
        // Try url-safe first (what `to_base64url` emits), fall back through
        // the other three. All four decoders are constant-time and reject
        // non-alphabet bytes, so the cascade is safe.
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
/// Newtype around the raw second count so callers can't mix seconds with
/// milliseconds. One stray `.timestamp_millis()` would produce sessions
/// that live 1000× too long with no compile-time or runtime signal.
///
/// `#[serde(transparent)]` keeps the newtype invisible on the wire, so the
/// cookie payload's `exp` stays a bare integer.
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
/// App-agnostic per ADR-0030: authorization is not in the payload. Each app
/// resolves roles and permissions from the [`VerifiedKey`] returned by
/// `KeyStore::verify_key` at request time. `name` is here so the UI can
/// render the user's name on first paint without a verify round-trip.
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
    /// The browser-visible origins allowed to make cookie-authenticated
    /// requests (ADR-0016). Not an `Option`: a deployment that has not
    /// stated its own origin cannot be guarded, and the alternatives are
    /// both silent failures — an empty list that allows everything is a
    /// decorative guard, one that allows nothing breaks every browser with
    /// no log line to explain it. The type is non-empty by construction,
    /// so this field is the invariant rather than a value that needs one.
    pub(crate) public_origins: PublicOrigins,
}

impl SessionConfig {
    /// Start a builder with SSO-friendly defaults (SameSite=Lax, secure=true,
    /// 24h TTL, cookie name `"fleet_session"`, post-login redirect `"/"`).
    #[must_use]
    pub fn builder() -> SessionConfigBuilder {
        SessionConfigBuilder::default()
    }

    /// Convenience for the common "just need the required fields" case.
    /// Equivalent to
    /// `Self::builder().cookie_name(...).app_namespace(...).public_origins(...).build()`.
    ///
    /// `public_origins` is a positional argument rather than a builder-only
    /// knob because it is a trust boundary, not a preference: ADR-0016 has
    /// the web surface refuse to start without it, and a constructor that
    /// let a caller forget it would put that refusal back in the hands of
    /// whoever remembers to call the setter.
    ///
    /// # Errors
    /// As for [`SessionConfigBuilder::build`].
    pub fn new(
        cookie_name: impl Into<String>,
        app_namespace: impl Into<String>,
        public_origins: PublicOrigins,
    ) -> Result<Self, crate::AuthError> {
        Self::builder()
            .cookie_name(cookie_name)
            .app_namespace(app_namespace)
            .public_origins(public_origins)
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

    /// The origins a cookie-authenticated request may come from
    /// (ADR-0016). Feed it straight to [`check_origin`]; there is no
    /// second policy anywhere and nothing else in the request is
    /// consulted, least of all `Host` or a forwarding header.
    #[must_use]
    pub fn public_origins(&self) -> &PublicOrigins {
        &self.public_origins
    }

    /// Validate the configuration. The builder calls this in
    /// [`SessionConfigBuilder::build`]; [`SessionState::new`] also calls
    /// it defensively in case a future internal path constructs a
    /// `SessionConfig` without going through the builder.
    ///
    /// `public_origins` gets no check here on purpose: [`PublicOrigins`]
    /// cannot be built empty, so the invariant ADR-0016 cares about is
    /// already carried by the value. A re-check would be a branch no input
    /// can reach and no test can exercise.
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
/// `post_login_redirect = "/"`. The required setters are
/// [`Self::app_namespace`] (must pass [`validate_app_namespace`]) and
/// [`Self::public_origins`] (ADR-0016: there is no default browser origin
/// to guess, so `build()` refuses rather than guessing); [`Self::cookie_name`]
/// defaults but must be non-empty if set.
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
    public_origins: Option<PublicOrigins>,
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

    /// The deployment's browser-visible origins. Required — `build()`
    /// fails when it is unset (ADR-0016).
    ///
    /// Takes a parsed [`PublicOrigins`] rather than strings so the
    /// operator's list is validated where it is read (startup, with the
    /// config path in hand) instead of here, where a bad entry would
    /// surface as an unhelpful builder error.
    #[must_use]
    pub fn public_origins(mut self, v: PublicOrigins) -> Self {
        self.public_origins = Some(v);
        self
    }

    /// Build and validate.
    ///
    /// # Errors
    /// Returns [`crate::AuthError::InvalidApp`] when `public_origins` was
    /// never set, or when validation fails (empty `cookie_name`, invalid
    /// `post_login_redirect`, `same_site == None` without `secure`, or
    /// invalid `app_namespace`).
    pub fn build(self) -> Result<SessionConfig, crate::AuthError> {
        // Missing origins first: a config that cannot be guarded is not a
        // config with one bad field, and the message has to name the knob
        // an operator has to go and write.
        let public_origins = self.public_origins.ok_or_else(|| {
            crate::AuthError::InvalidApp(
                "public_origins is required: state the deployment's browser-visible \
                 origin(s), e.g. [\"https://trawl.example.com\"] (ADR-0016)"
                    .into(),
            )
        })?;
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
            public_origins,
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
/// `Domain`) must match what the original `Set-Cookie` had, because browsers
/// reject clear directives that don't match the issued attributes.
/// `Max-Age=0` signals immediate deletion.
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

    let cipher = XChaCha20Poly1305::new(
        <&chacha20poly1305::Key>::try_from(key.0.as_slice()).expect("SessionKey is 32 bytes"),
    );

    let mut nonce_bytes = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = XNonce::from(nonce_bytes);

    let ciphertext = cipher
        .encrypt(&nonce, plaintext.as_slice())
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
    let nonce = <&XNonce>::try_from(nonce_bytes).expect("split_at yields exactly NONCE_LEN bytes");

    let cipher = XChaCha20Poly1305::new(
        <&chacha20poly1305::Key>::try_from(key.0.as_slice()).expect("SessionKey is 32 bytes"),
    );
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

/// Whether a present `Origin` is one of the deployment's configured public
/// origins (ADR-0016).
///
/// The comparison is whole-origin: scheme, host and effective port, both
/// sides normalized by the one parser in [`crate::origin`]. The rule this
/// replaced compared the `Origin`'s host against the request's `Host`
/// header, which allowed `http://trawl.example` to forge against the
/// `https://` deployment of the same name, allowed any other port of that
/// name, and made a security verdict depend on a header the reverse proxy
/// in front rewrites. `Host` is not a security input here any more, and
/// neither is `Forwarded` or `X-Forwarded-*`: the verdict is a function of
/// the operator's configured list and the bytes the browser stamped, and
/// of nothing else.
///
/// - `origin` **absent** -> allowed. This is a browser CSRF control, not
///   client authentication: browsers always stamp cross-site POSTs, while
///   curl and scripted clients send no `Origin` and keep working.
/// - `origin` parses to a configured origin -> allowed.
/// - anything else -> rejected. That includes a sibling fleet app under
///   the shared cookie domain: `Domain=` decides where a browser sends the
///   cookie, never who may call these endpoints.
///
/// Prefer [`check_origin`], which also answers the two questions a bare
/// `Option<&str>` cannot represent (more than one `Origin` field, a value
/// that is not UTF-8) and emits the one rejection log.
#[must_use]
pub fn origin_allowed(origin: Option<&str>, allowed: &PublicOrigins) -> bool {
    match origin {
        None => true,
        Some(raw) => Origin::parse(raw).is_ok_and(|origin| allowed.contains(&origin)),
    }
}

/// Returned by [`check_origin`] when a request's `Origin` is rejected. The
/// rejection has already been logged with its `handler`/`reason`/`origin`
/// fields; each caller maps this marker onto its own error/response type
/// (403, no `Set-Cookie`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OriginRejected;

/// Guard a cookie-authenticated endpoint on the request's `Origin`
/// (ADR-0016).
///
/// Takes the whole [`http::HeaderMap`] rather than an `Option<&str>`
/// because two of the four verdicts are properties of the map and not of
/// any string: a request carrying two `Origin` fields has no single origin
/// to compare (and `HeaderMap::get` would quietly hand back the first
/// one), and a value that is not valid UTF-8 cannot be parsed at all. A
/// caller that extracted the header itself would decide both of those on
/// its own, which is the drift this function exists to prevent. It is also
/// why the parameter is the map and not a request: nothing else in the
/// request is consulted, so nothing else is asked for.
///
/// Verdicts, in order: no `Origin` -> `Ok`; two or more `Origin` fields
/// (even byte-identical ones) -> rejected; a non-UTF-8 value -> rejected;
/// otherwise parse and compare against `allowed`.
///
/// On rejection it emits exactly one `tracing::warn!` with bounded fields:
/// `handler` (the caller's own `&'static str` label), `reason` (from the
/// closed vocabulary in [`crate::origin`]) and `origin`, which is present
/// only when the header parsed and is then the normalized text, never the
/// raw bytes. An attacker chooses the header; they do not get to choose
/// what lands in the log, how long it is, or whether it contains a newline.
///
/// # Errors
///
/// Returns [`OriginRejected`] for every verdict above that is not `Ok`.
pub fn check_origin(
    headers: &http::HeaderMap,
    allowed: &PublicOrigins,
    handler: &'static str,
) -> Result<(), OriginRejected> {
    let mut values = headers.get_all(http::header::ORIGIN).iter();
    let Some(only) = values.next() else {
        return Ok(());
    };
    if values.next().is_some() {
        // A browser sends at most one `Origin`. Two fields mean something
        // between the browser and here appended one, so there is no single
        // origin to compare: refuse instead of picking a copy. Identical
        // copies are refused too, because "they matched, so it is fine"
        // is a rule about this request, not about the next one.
        return Err(reject(handler, REASON_MULTIPLE_HEADERS, None));
    }
    let Ok(text) = only.to_str() else {
        return Err(reject(handler, REASON_NON_UTF8, None));
    };
    if origin_allowed(Some(text), allowed) {
        return Ok(());
    }
    let (reason, origin) = rejection_log_fields(Some(text));
    Err(reject(handler, reason, origin.as_deref()))
}

/// Emit the one cross-origin rejection log line and return the marker.
///
/// Every rejection path goes through here so the message and its fields
/// cannot drift between the multi-header case, the encoding case and the
/// parse/allowlist cases. `origin` is an `Option`: `tracing` records
/// nothing for a `None` value, so a refusal with no safe text to print
/// leaves the field out of the line entirely rather than printing a
/// placeholder that looks like an origin.
fn reject(handler: &'static str, reason: &'static str, origin: Option<&str>) -> OriginRejected {
    tracing::warn!(
        handler,
        reason,
        origin,
        "auth: cross-origin request rejected"
    );
    OriginRejected
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

    #[test]
    fn fleet_session_runtime_environment_names_are_frozen() {
        assert_eq!(ENV_SESSION_AEAD_KEY, "FLEET_SESSION_AEAD_KEY");
        assert_eq!(ENV_SESSION_COOKIE_DOMAIN, "FLEET_SESSION_COOKIE_DOMAIN");
        assert_eq!(ENV_SESSION_COOKIE_PATH, "FLEET_SESSION_COOKIE_PATH");
        assert_eq!(ENV_SESSION_COOKIE_SECURE, "FLEET_SESSION_COOKIE_SECURE");
        assert_eq!(ENV_SESSION_PUBLIC_ORIGINS, "FLEET_SESSION_PUBLIC_ORIGINS");
    }

    #[test]
    fn runtime_overrides_parse_the_valid_shapes() {
        let absent = SessionRuntimeOverrides::parse(None, None, None, None, None).unwrap();
        assert!(absent.key.is_none());
        assert_eq!(absent.domain, RuntimeCookieDomain::PreserveConfigured);
        assert_eq!(absent.secure, None);

        let key = SessionKey::from_bytes([0x42; KEY_LEN]).to_base64url();
        let full = SessionRuntimeOverrides::parse(
            Some(key.to_string()),
            Some(String::new()),
            Some("/".into()),
            Some("true".into()),
            Some("https://trawl.example.com".into()),
        )
        .unwrap();
        assert_eq!(full.key.unwrap().to_base64url().as_str(), key.as_str());
        assert_eq!(full.domain, RuntimeCookieDomain::HostOnly);
        assert_eq!(full.secure, Some(true));
        assert_eq!(
            full.public_origins,
            Some(PublicOrigins::parse(["https://trawl.example.com"]).unwrap())
        );
        assert!(absent.public_origins.is_none());

        let explicit =
            SessionRuntimeOverrides::parse(None, Some(".fleet.example".into()), None, None, None)
                .unwrap();
        assert_eq!(
            explicit.domain,
            RuntimeCookieDomain::Explicit(".fleet.example".into())
        );
    }

    #[test]
    fn invalid_runtime_values_fail_closed() {
        let bad_key =
            SessionRuntimeOverrides::parse(Some("not-base64".into()), None, None, None, None)
                .unwrap_err();
        assert!(
            matches!(bad_key, SessionRuntimeError::InvalidKey { name } if name == ENV_SESSION_AEAD_KEY)
        );

        for path in ["", "/app", "//"] {
            let err = SessionRuntimeOverrides::parse(None, None, Some(path.into()), None, None)
                .unwrap_err();
            assert!(matches!(
                err,
                SessionRuntimeError::InvalidValue { name, .. } if name == ENV_SESSION_COOKIE_PATH
            ));
        }

        for secure in ["TRUE", "1", "", "yes"] {
            let err = SessionRuntimeOverrides::parse(None, None, None, Some(secure.into()), None)
                .unwrap_err();
            assert!(matches!(
                err,
                SessionRuntimeError::InvalidValue { name, .. } if name == ENV_SESSION_COOKIE_SECURE
            ));
        }

        for domain in [
            ".",
            "https://fleet.example",
            "-fleet.example",
            "fleet..example",
            "fleet.example:8444",
            "fleet.example\r\nx-injected: yes",
        ] {
            let err = SessionRuntimeOverrides::parse(None, Some(domain.into()), None, None, None)
                .unwrap_err();
            assert!(matches!(
                err,
                SessionRuntimeError::InvalidValue { name, .. } if name == ENV_SESSION_COOKIE_DOMAIN
            ));
        }
    }

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
        let expiry = SessionExpiry::from_unix_seconds(1_700_000_000);
        let json = serde_json::to_string(&expiry).unwrap();
        assert_eq!(json, "1700000000");
        let back: SessionExpiry = serde_json::from_str("1700000000").unwrap();
        assert_eq!(back, expiry);
    }

    #[test]
    fn payload_serde_drops_no_fields() {
        // A field without a serde default changes the JSON shape and stops
        // every already-issued cookie from parsing, so make that a
        // deliberate decision.
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
        // `.fleet.home.lan` and `fleet.home.lan` are semantically identical
        // per RFC 6265; the `cookie` crate normalises both to the dotless
        // form. Either input is acceptable; we assert on the normalised emit.
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
            .public_origins(test_origins())
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
        let err = SessionConfig::builder()
            .public_origins(test_origins())
            .build()
            .unwrap_err();
        assert!(matches!(err, crate::AuthError::InvalidApp(_)));
    }

    #[test]
    fn session_config_builder_requires_public_origins() {
        // ADR-0016's loud upgrade, at the type's own door: there is no
        // browser origin to default to, so a config that never states one
        // does not exist. The message has to name the knob, because the
        // operator reading it is looking for something to write down.
        let err = SessionConfig::builder()
            .app_namespace("trawl")
            .build()
            .unwrap_err();
        assert!(
            matches!(&err, crate::AuthError::InvalidApp(m) if m.contains("public_origins")),
            "got: {err:?}"
        );
    }

    #[test]
    fn session_config_new_validates_namespace() {
        let cfg = SessionConfig::new("fleet_session", "trawl", test_origins()).unwrap();
        assert_eq!(cfg.app_namespace(), "trawl");
        assert_eq!(cfg.public_origins(), &test_origins());

        let err = SessionConfig::new("fleet_session", "BAD!", test_origins()).unwrap_err();
        assert!(matches!(err, crate::AuthError::InvalidApp(_)));
    }

    #[test]
    fn session_config_rejects_empty_cookie_name() {
        let err = SessionConfig::builder()
            .cookie_name("")
            .app_namespace("trawl")
            .public_origins(test_origins())
            .build()
            .unwrap_err();
        assert!(matches!(err, crate::AuthError::InvalidApp(m) if m.contains("cookie_name")));
    }

    #[test]
    fn session_config_rejects_relative_redirect() {
        let err = SessionConfig::builder()
            .app_namespace("trawl")
            .public_origins(test_origins())
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
            .public_origins(test_origins())
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
                .public_origins(test_origins())
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
                .public_origins(test_origins())
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
            .public_origins(test_origins())
            .secure(false)
            .same_site(cookie::SameSite::None)
            .build()
            .unwrap_err();
        assert!(
            matches!(&err, crate::AuthError::InvalidApp(m) if m.contains("SameSite=None")),
            "got: {err:?}"
        );
    }

    /// The allowlist every `SessionConfig` fixture above is built with: a
    /// public HTTPS deployment, the packaged loopback bind, and that
    /// bind's IPv6 spelling. Three entries, so "allowed" cannot be an
    /// accident of a single-element list.
    ///
    /// The guard's own truth table and rejection-log contract run against
    /// the same three entries from
    /// `crates/fleet-auth/tests/origin_guard_truth_table.rs`, out of reach
    /// of the source scan in `no_forwarded_trust.rs`, which reads whole
    /// files and cannot tell a planted `X-Forwarded-Host` in a test from
    /// one the guard started trusting.
    fn test_origins() -> PublicOrigins {
        PublicOrigins::parse([
            "https://trawl.example.com",
            "http://localhost:8090",
            "http://[::1]:8090",
        ])
        .expect("valid test allowlist")
    }

    // -- the public-origin runtime override -----------------------------

    #[test]
    fn the_public_origins_override_parses_a_comma_separated_list() {
        let parsed = SessionRuntimeOverrides::parse(
            None,
            None,
            None,
            None,
            Some("https://trawl.example.com,http://localhost:8090".into()),
        )
        .unwrap()
        .public_origins
        .expect("present");
        assert_eq!(
            parsed,
            PublicOrigins::parse(["https://trawl.example.com", "http://localhost:8090"]).unwrap()
        );
    }

    #[test]
    fn a_bad_override_entry_names_the_variable_and_the_index() {
        // Entries are not trimmed: a space inside an entry is the operator
        // writing something this parser refuses to guess at, and guessing
        // is how a config list and an env list end up meaning different
        // things.
        for (raw, index) in [
            ("https://trawl.example.com, http://localhost:8090", 1),
            ("nonsense", 0),
            ("", 0),
            ("https://ok.example.com,", 1),
        ] {
            let err = SessionRuntimeOverrides::parse(None, None, None, None, Some(raw.into()))
                .unwrap_err();
            match err {
                SessionRuntimeError::InvalidOrigin {
                    name,
                    index: reported,
                    ..
                } => {
                    assert_eq!(name, ENV_SESSION_PUBLIC_ORIGINS);
                    assert_eq!(reported, index, "for {raw:?}");
                }
                other => panic!("{raw:?} should name the entry, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_duplicated_override_entry_is_refused() {
        let err = SessionRuntimeOverrides::parse(
            None,
            None,
            None,
            None,
            Some("https://trawl.example.com,https://trawl.example.com:443".into()),
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                SessionRuntimeError::DuplicateOrigin { name, first: 0, second: 1 }
                    if name == ENV_SESSION_PUBLIC_ORIGINS
            ),
            "got: {err:?}"
        );
    }

    #[test]
    fn session_config_no_domain_clears_domain_setter() {
        let cfg = SessionConfig::builder()
            .app_namespace("trawl")
            .public_origins(test_origins())
            .domain("fleet.home.lan")
            .no_domain()
            .build()
            .unwrap();
        assert!(cfg.domain().is_none());
    }
}
