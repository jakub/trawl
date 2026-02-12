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
#[derive(Clone)]
pub struct CreatedKey {
    /// Key metadata.
    pub info: ApiKeyInfo,
    /// The full plaintext token — show to the user immediately, never store.
    /// Wrapped in [`Zeroizing`] to clear from memory on drop.
    pub plaintext_token: Zeroizing<String>,
}

impl std::fmt::Debug for CreatedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CreatedKey")
            .field("info", &self.info)
            .field("plaintext_token", &"<redacted>")
            .finish()
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn created_key_debug_redacts_token() {
        let key = CreatedKey {
            info: ApiKeyInfo {
                id: 1,
                prefix: "abcd1234".into(),
                name: "test-key".into(),
                role: Role::Admin,
                active: true,
                created_at: "2026-01-01T00:00:00Z".into(),
                expires_at: None,
                last_used: None,
                revoked_at: None,
            },
            plaintext_token: Zeroizing::new("flt_supersecrettoken12345".into()),
        };
        let debug = format!("{key:?}");
        assert!(
            debug.contains("<redacted>"),
            "should contain redaction marker"
        );
        assert!(
            !debug.contains("supersecret"),
            "must not contain actual token"
        );
    }
}
