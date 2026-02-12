//! API key data types — metadata, creation results, verified identity.

use zeroize::Zeroizing;

use crate::roles::Role;

/// Metadata about an API key, as stored in the database.
/// NEVER contains the hash or the plaintext token.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ApiKeyInfo {
    /// Database row id.
    pub id: i64,
    /// First 8 chars of the token (after `flt_` prefix), used for identification.
    pub prefix: String,
    /// Human-readable label.
    pub name: String,
    /// The role granted by this key.
    pub role: Role,
    /// Whether the key is active (not revoked).
    pub active: bool,
    /// ISO 8601 UTC creation timestamp.
    pub created_at: String,
    /// ISO 8601 UTC expiration timestamp, if any.
    pub expires_at: Option<String>,
    /// ISO 8601 UTC last-used timestamp, if ever used.
    pub last_used: Option<String>,
    /// ISO 8601 UTC revocation timestamp, if revoked.
    pub revoked_at: Option<String>,
}

/// The result of creating a new API key.
/// The plaintext token is included ONCE — it is never stored or retrievable.
#[derive(Debug, Clone)]
pub struct CreatedKey {
    /// Key metadata.
    pub info: ApiKeyInfo,
    /// The full plaintext token — show to the user immediately, never store.
    /// Wrapped in [`Zeroizing`] to clear from memory on drop.
    pub plaintext_token: Zeroizing<String>,
}

/// A verified API key identity — the result of successful authentication.
/// Minimal struct for request handlers (no DB metadata leakage).
#[derive(Debug, Clone)]
pub struct VerifiedKey {
    /// Database row id.
    pub id: i64,
    /// The key prefix for identification.
    pub prefix: String,
    /// Human-readable name.
    pub name: String,
    /// The role granted by this key.
    pub role: Role,
}
