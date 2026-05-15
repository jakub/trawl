// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! API key data types — metadata, creation results, verified identity.

use zeroize::Zeroizing;

use crate::assignments::{PrincipalKind, RoleAssignment, TRAWL_APP};
use crate::roles::{Permission, Role};

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
    /// Whether the underlying principal is interactive (human) or not (service).
    pub kind: PrincipalKind,
    /// All `(app, role)` grants attached to this key, across every app.
    pub assignments: Vec<RoleAssignment>,
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

impl ApiKeyInfo {
    /// Look up the role granted to this key in the given app namespace.
    pub fn role_for(&self, app: &str) -> Option<&str> {
        self.assignments
            .iter()
            .find(|a| a.app == app)
            .map(|a| a.role.as_str())
    }

    /// Parsed trawl-app role, if this key has a `("trawl", _)` grant.
    pub fn trawl_role(&self) -> Option<Role> {
        self.role_for(TRAWL_APP).and_then(|r| {
            if let Ok(role) = r.parse() {
                Some(role)
            } else {
                tracing::warn!(role = r, "unrecognized trawl role in assignment");
                None
            }
        })
    }
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
    /// Whether the underlying principal is interactive (human) or not (service).
    pub kind: PrincipalKind,
    /// All `(app, role)` grants attached to this key.
    pub assignments: Vec<RoleAssignment>,
}

impl VerifiedKey {
    /// Look up the role granted to this principal in the given app namespace.
    ///
    /// Returns `None` if the key has no grant in that app.
    pub fn role_for(&self, app: &str) -> Option<&str> {
        self.assignments
            .iter()
            .find(|a| a.app == app)
            .map(|a| a.role.as_str())
    }

    /// Parsed trawl-app role, if this key has a `("trawl", _)` grant.
    ///
    /// Returns `None` if the key has no trawl grant or the grant's role
    /// string is unrecognized by trawl-auth.
    pub fn trawl_role(&self) -> Option<Role> {
        self.role_for(TRAWL_APP).and_then(|r| {
            if let Ok(role) = r.parse() {
                Some(role)
            } else {
                tracing::warn!(role = r, "unrecognized trawl role in assignment");
                None
            }
        })
    }

    /// Check whether this key has the given trawl-app permission.
    ///
    /// Short-circuits to `false` for keys with no trawl-app grant.
    pub fn has_permission(&self, perm: Permission) -> bool {
        self.trawl_role().is_some_and(|r| r.has_permission(perm))
    }

    /// Render assignments as `"app1:role1,app2:role2"` for log fields.
    ///
    /// Deterministic (sorted by app). Returns `"none"` when empty so log
    /// filters can match an explicit string.
    pub fn assignments_display(&self) -> String {
        if self.assignments.is_empty() {
            return "none".to_owned();
        }
        self.assignments
            .iter()
            .map(|a| format!("{}:{}", a.app, a.role))
            .collect::<Vec<_>>()
            .join(",")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admin_trawl() -> Vec<RoleAssignment> {
        vec![RoleAssignment {
            app: TRAWL_APP.into(),
            role: "admin".into(),
        }]
    }

    #[test]
    fn created_key_debug_redacts_token() {
        let key = CreatedKey {
            info: ApiKeyInfo {
                id: 1,
                prefix: "abcd1234".into(),
                name: "test-key".into(),
                kind: PrincipalKind::Human,
                assignments: admin_trawl(),
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

    #[test]
    fn verified_key_has_permission_via_trawl_role() {
        let key = VerifiedKey {
            id: 1,
            prefix: "abcd1234".into(),
            name: "k".into(),
            kind: PrincipalKind::Human,
            assignments: admin_trawl(),
        };
        assert!(key.has_permission(Permission::Query));
        assert!(key.has_permission(Permission::ServerManage));
        assert_eq!(key.trawl_role(), Some(Role::Admin));
    }

    #[test]
    fn verified_key_without_trawl_grant_has_no_trawl_permissions() {
        let key = VerifiedKey {
            id: 2,
            prefix: "ef567890".into(),
            name: "siem-only".into(),
            kind: PrincipalKind::Service,
            assignments: vec![RoleAssignment {
                app: "coastwatch".into(),
                role: "siem_consumer".into(),
            }],
        };
        assert!(!key.has_permission(Permission::Query));
        assert_eq!(key.trawl_role(), None);
        assert_eq!(key.role_for("coastwatch"), Some("siem_consumer"));
    }

    #[test]
    fn assignments_display_formats_pairs() {
        let key = VerifiedKey {
            id: 3,
            prefix: "p".into(),
            name: "n".into(),
            kind: PrincipalKind::Service,
            assignments: vec![
                RoleAssignment {
                    app: "trawl".into(),
                    role: "analyst".into(),
                },
                RoleAssignment {
                    app: "coastwatch".into(),
                    role: "siem_consumer".into(),
                },
            ],
        };
        assert_eq!(
            key.assignments_display(),
            "trawl:analyst,coastwatch:siem_consumer"
        );

        let empty = VerifiedKey {
            id: 4,
            prefix: "p".into(),
            name: "n".into(),
            kind: PrincipalKind::Human,
            assignments: vec![],
        };
        assert_eq!(empty.assignments_display(), "none");
    }
}
