// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! App-agnostic auth types shared across fleet apps.

use std::collections::BTreeMap;
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

/// One `(app, permission)` entry in a role's permission bundle.
///
/// `app` is an opaque app namespace (e.g. `"trawl"`, `"coastwatch"`); the
/// permission string is interpreted by the owning app's compile-time
/// `Permission` enum — roles are data, permissions are code (ADR-0006).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RolePermission {
    /// App namespace this permission belongs to.
    pub app: String,
    /// App-defined permission string (e.g. `"query"`, `"stories_read"`).
    pub permission: String,
}

/// A data-defined role: a named, cross-app bundle of permission strings
/// (ADR-0006). Keys hold any number of roles; effective permissions are the
/// union across them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Role {
    /// Database row id.
    pub id: i64,
    /// Unique role name (lowercase alnum + underscore + hyphen).
    pub name: String,
    /// Optional per-role rate ceiling (requests/minute). A key's effective
    /// RPM is the max across its roles; when set it OVERRIDES the route
    /// class's config default (never max'd or summed with it).
    pub rate_rpm: Option<u32>,
    /// The `(app, permission)` bundle this role grants.
    pub permissions: Vec<RolePermission>,
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
    /// Names of every role held by this key, sorted.
    pub roles: Vec<String>,
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
/// Carries role names (display/audit) plus the resolved per-app permission
/// unions (gates). The union fields are private: [`VerifiedKey::from_roles`]
/// is the single place union semantics exist, so a hand-rolled construction
/// can never disagree with the substrate's resolution rules.
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
    /// Sorted role names.
    roles: Vec<String>,
    /// Per-app permission union: app → sorted, deduped permission strings.
    permissions: BTreeMap<String, Vec<String>>,
    /// Max `rate_rpm` across the key's roles, if any role sets one.
    rate_rpm: Option<u32>,
}

impl VerifiedKey {
    /// Build a verified identity from the key row plus its resolved roles.
    ///
    /// The ONLY constructor — union semantics live here and nowhere else:
    /// role names are sorted, per-app permissions are the deduped union
    /// across roles, and `rate_rpm` is the max across roles that set it.
    pub fn from_roles(
        id: i64,
        prefix: impl Into<String>,
        name: impl Into<String>,
        kind: PrincipalKind,
        roles: Vec<Role>,
    ) -> Self {
        let mut names: Vec<String> = Vec::with_capacity(roles.len());
        let mut union: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut rate_rpm: Option<u32> = None;

        for role in roles {
            names.push(role.name);
            rate_rpm = rate_rpm.max(role.rate_rpm);
            for rp in role.permissions {
                union.entry(rp.app).or_default().push(rp.permission);
            }
        }
        names.sort_unstable();
        names.dedup();
        for perms in union.values_mut() {
            perms.sort_unstable();
            perms.dedup();
        }

        Self {
            id,
            prefix: prefix.into(),
            name: name.into(),
            kind,
            roles: names,
            permissions: union,
            rate_rpm,
        }
    }

    /// Sorted names of every role this key holds.
    pub fn roles(&self) -> &[String] {
        &self.roles
    }

    /// The resolved permission union for one app namespace (sorted, deduped).
    /// Empty slice when the key has no permissions in that app.
    pub fn permissions_for(&self, app: &str) -> &[String] {
        self.permissions.get(app).map_or(&[], Vec::as_slice)
    }

    /// Whether the resolved union grants `permission` in `app`.
    pub fn has_app_permission(&self, app: &str, permission: &str) -> bool {
        self.permissions
            .get(app)
            .is_some_and(|perms| perms.iter().any(|p| p == permission))
    }

    /// Whether the key holds at least one permission in `app` — the
    /// substrate-level "may enter this app at all" question
    /// (`require_session`'s namespace gate).
    pub fn has_any_permission(&self, app: &str) -> bool {
        self.permissions.get(app).is_some_and(|p| !p.is_empty())
    }

    /// Max `rate_rpm` across the key's roles, `None` when no role sets one.
    pub fn rate_rpm(&self) -> Option<u32> {
        self.rate_rpm
    }

    /// Render this key's role names as `"role-a,role-b"` (sorted) for log
    /// fields and display. Returns `"none"` when the key holds no roles so
    /// log filters can match an explicit string instead of an absent field.
    pub fn roles_display(&self) -> String {
        if self.roles.is_empty() {
            "none".to_owned()
        } else {
            self.roles.join(",")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role(name: &str, rate_rpm: Option<u32>, perms: &[(&str, &str)]) -> Role {
        Role {
            id: 0,
            name: name.into(),
            rate_rpm,
            permissions: perms
                .iter()
                .map(|(app, permission)| RolePermission {
                    app: (*app).into(),
                    permission: (*permission).into(),
                })
                .collect(),
        }
    }

    fn key_with(roles: Vec<Role>) -> VerifiedKey {
        VerifiedKey::from_roles(1, "abcd1234", "k", PrincipalKind::Human, roles)
    }

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
    fn from_roles_unions_overlapping_permissions() {
        // Two overlapping roles resolve the deduped union (AC2).
        let key = key_with(vec![
            role(
                "tier1",
                None,
                &[("trawl", "query"), ("trawl", "schema_read")],
            ),
            role("tier2", None, &[("trawl", "query"), ("trawl", "export")]),
        ]);
        assert_eq!(key.roles(), ["tier1", "tier2"]);
        assert_eq!(
            key.permissions_for("trawl"),
            ["export", "query", "schema_read"]
        );
        assert!(key.has_app_permission("trawl", "export"));
        assert!(!key.has_app_permission("trawl", "ingest"));
    }

    #[test]
    fn from_roles_cross_app_role_grants_in_both_namespaces() {
        // One role spanning two apps grants in both from a single link (AC2).
        let key = key_with(vec![role(
            "bridge",
            None,
            &[("trawl", "query"), ("coastwatch", "stories_read")],
        )]);
        assert!(key.has_app_permission("trawl", "query"));
        assert!(key.has_app_permission("coastwatch", "stories_read"));
        assert!(key.has_any_permission("trawl"));
        assert!(key.has_any_permission("coastwatch"));
        assert!(!key.has_any_permission("elsewhere"));
    }

    #[test]
    fn from_roles_rate_rpm_is_max_across_roles() {
        // AC5: max across roles; None when all null.
        assert_eq!(key_with(vec![]).rate_rpm(), None);
        assert_eq!(
            key_with(vec![role("a", None, &[]), role("b", None, &[])]).rate_rpm(),
            None
        );
        assert_eq!(
            key_with(vec![role("a", Some(60), &[]), role("b", None, &[])]).rate_rpm(),
            Some(60)
        );
        assert_eq!(
            key_with(vec![role("a", Some(60), &[]), role("b", Some(2000), &[])]).rate_rpm(),
            Some(2000)
        );
    }

    #[test]
    fn roles_display_sorted_with_empty_fallback() {
        let key = key_with(vec![role("zeta", None, &[]), role("alpha", None, &[])]);
        assert_eq!(key.roles_display(), "alpha,zeta");
        assert_eq!(key_with(vec![]).roles_display(), "none");
    }

    #[test]
    fn permissions_for_unknown_app_is_empty() {
        let key = key_with(vec![role("r", None, &[("trawl", "query")])]);
        assert!(key.permissions_for("coastwatch").is_empty());
        assert!(!key.has_any_permission("coastwatch"));
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
                roles: vec![],
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
