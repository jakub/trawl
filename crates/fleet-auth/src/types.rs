// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! App-agnostic auth types shared across fleet apps.

use std::str::FromStr;

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::error::AuthError;

/// Identity discriminator orthogonal to role assignments.
///
/// Distinguishes interactive principals (humans logging into a UI) from
/// non-interactive ones (services calling an API). No effect on authorization
/// by itself — apps may use it for richer audit context or to gate features
/// (e.g. require human consent for destructive ops).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PrincipalKind {
    /// Interactive principal — a person using the UI or CLI.
    Human,
    /// Non-interactive principal — a service or automation account.
    Service,
}

impl PrincipalKind {
    /// Wire / database representation.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Service => "service",
        }
    }
}

impl std::fmt::Display for PrincipalKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for PrincipalKind {
    type Err = AuthError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "human" => Ok(Self::Human),
            "service" => Ok(Self::Service),
            other => Err(AuthError::InvalidPrincipalKind(other.to_owned())),
        }
    }
}

/// A namespaced role grant on an API key.
///
/// `app` is an opaque app namespace (e.g. `"trawl"`, `"coastwatch"`). The role
/// string is interpreted by each app independently — fleet-auth does not
/// define a shared role vocabulary.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RoleAssignment {
    /// App namespace this grant applies to.
    pub app: String,
    /// App-defined role name (e.g. `"admin"` for trawl, `"siem_consumer"` for
    /// coastwatch).
    pub role: String,
}

/// Metadata about an API key, as stored in the database.
///
/// Never contains the hash or the plaintext token.
#[derive(Debug, Clone, Serialize)]
pub struct ApiKeyInfo {
    /// Database row id.
    pub id: i64,
    /// First 8 chars of the random token body (after the `flt_` prefix). Used
    /// for identification in admin tooling and audit logs.
    pub prefix: String,
    /// Human-readable label.
    pub name: String,
    /// Whether the underlying principal is interactive or not.
    pub kind: PrincipalKind,
    /// All `(app, role)` grants attached to this key, across every app.
    pub assignments: Vec<RoleAssignment>,
    /// Whether the key is active (`!revoked`).
    pub active: bool,
    /// Creation timestamp.
    #[cfg(feature = "keystore")]
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Optional expiration timestamp.
    #[cfg(feature = "keystore")]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Optional last-used timestamp.
    #[cfg(feature = "keystore")]
    pub last_used: Option<chrono::DateTime<chrono::Utc>>,
    /// Optional revocation timestamp.
    #[cfg(feature = "keystore")]
    pub revoked_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl ApiKeyInfo {
    /// Look up the role granted to this key in the given app namespace.
    pub fn role_for(&self, app: &str) -> Option<&str> {
        self.assignments
            .iter()
            .find(|a| a.app == app)
            .map(|a| a.role.as_str())
    }
}

/// The result of creating a new API key.
///
/// The plaintext token is included ONCE — it must be shown to the user
/// immediately and is never recoverable. Wrapped in [`Zeroizing`] so it is
/// cleared from memory on drop.
#[derive(Clone)]
pub struct CreatedKey {
    /// Key metadata.
    pub info: ApiKeyInfo,
    /// The full plaintext token.
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
///
/// Minimal struct for request handlers; carries no DB metadata to avoid
/// leaking liveness data through every authenticated response.
#[derive(Debug, Clone)]
pub struct VerifiedKey {
    /// Database row id.
    pub id: i64,
    /// The key prefix for identification.
    pub prefix: String,
    /// Human-readable name (as a UI convenience).
    pub name: String,
    /// Whether the principal is interactive or not.
    pub kind: PrincipalKind,
    /// All `(app, role)` grants attached to this key.
    pub assignments: Vec<RoleAssignment>,
}

impl VerifiedKey {
    /// Look up the role granted to this principal in the given app namespace.
    ///
    /// Returns `None` if the key has no grant in that app — callers should
    /// treat `None` as "not authorized for this app" rather than as a default.
    pub fn role_for(&self, app: &str) -> Option<&str> {
        self.assignments
            .iter()
            .find(|a| a.app == app)
            .map(|a| a.role.as_str())
    }

    /// Render this key's assignments as `"app1:role1,app2:role2"` for log
    /// fields. Delegates to [`format_assignments`].
    pub fn assignments_display(&self) -> String {
        format_assignments(&self.assignments)
    }
}

/// Render a slice of `RoleAssignment` as `"app1:role1,app2:role2"`, sorted
/// by app for deterministic log/tracing output. Returns `"none"` when empty
/// so log filters can match an explicit string instead of an absent field.
pub fn format_assignments(assignments: &[RoleAssignment]) -> String {
    if assignments.is_empty() {
        return "none".to_owned();
    }
    let mut sorted: Vec<&RoleAssignment> = assignments.iter().collect();
    sorted.sort_by(|a, b| a.app.cmp(&b.app));
    sorted
        .iter()
        .map(|a| format!("{}:{}", a.app, a.role))
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn principal_kind_roundtrips() {
        for kind in [PrincipalKind::Human, PrincipalKind::Service] {
            let s = kind.to_string();
            let parsed: PrincipalKind = s.parse().unwrap();
            assert_eq!(kind, parsed);
        }
    }

    #[test]
    fn principal_kind_rejects_unknown() {
        let err = "alien".parse::<PrincipalKind>().unwrap_err();
        assert!(matches!(err, AuthError::InvalidPrincipalKind(_)));
    }

    #[test]
    fn role_for_returns_grant_or_none() {
        let key = VerifiedKey {
            id: 1,
            prefix: "p".into(),
            name: "n".into(),
            kind: PrincipalKind::Human,
            assignments: vec![
                RoleAssignment {
                    app: "trawl".into(),
                    role: "admin".into(),
                },
                RoleAssignment {
                    app: "coastwatch".into(),
                    role: "siem_consumer".into(),
                },
            ],
        };
        assert_eq!(key.role_for("trawl"), Some("admin"));
        assert_eq!(key.role_for("coastwatch"), Some("siem_consumer"));
        assert_eq!(key.role_for("nope"), None);
    }

    #[test]
    fn assignments_display_sorted_with_empty_fallback() {
        // listed coastwatch first, expect trawl first after sort
        let key = VerifiedKey {
            id: 1,
            prefix: "p".into(),
            name: "n".into(),
            kind: PrincipalKind::Service,
            assignments: vec![
                RoleAssignment {
                    app: "coastwatch".into(),
                    role: "siem_consumer".into(),
                },
                RoleAssignment {
                    app: "trawl".into(),
                    role: "analyst".into(),
                },
            ],
        };
        assert_eq!(
            key.assignments_display(),
            "coastwatch:siem_consumer,trawl:analyst"
        );

        let empty = VerifiedKey {
            id: 2,
            prefix: "p".into(),
            name: "n".into(),
            kind: PrincipalKind::Human,
            assignments: vec![],
        };
        assert_eq!(empty.assignments_display(), "none");
    }

    #[cfg(feature = "keystore")]
    #[test]
    fn created_key_debug_redacts_plaintext() {
        let key = CreatedKey {
            info: ApiKeyInfo {
                id: 1,
                prefix: "abcd1234".into(),
                name: "test".into(),
                kind: PrincipalKind::Human,
                assignments: vec![],
                active: true,
                created_at: chrono::Utc::now(),
                expires_at: None,
                last_used: None,
                revoked_at: None,
            },
            plaintext_token: Zeroizing::new("flt_supersecrettoken12345".into()),
        };
        let debug = format!("{key:?}");
        assert!(debug.contains("<redacted>"), "expected redaction marker");
        assert!(!debug.contains("supersecret"), "must not leak plaintext");
    }
}
