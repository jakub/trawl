// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::BTreeMap;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::error::AuthError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PrincipalKind {
    Human,
    Service,
}

impl PrincipalKind {
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

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RolePermission {
    pub app: String,
    pub permission: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Role {
    pub id: i64,
    pub name: String,
    pub rate_rpm: Option<u32>,
    pub permissions: Vec<RolePermission>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiKeyInfo {
    pub id: i64,
    pub prefix: String,
    pub name: String,
    pub kind: PrincipalKind,
    pub roles: Vec<String>,
    pub active: bool,
    #[cfg(feature = "keystore")]
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[cfg(feature = "keystore")]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    #[cfg(feature = "keystore")]
    pub last_used: Option<chrono::DateTime<chrono::Utc>>,
    #[cfg(feature = "keystore")]
    pub revoked_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Clone)]
pub struct CreatedKey {
    pub info: ApiKeyInfo,
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

/// The union fields are private: [`VerifiedKey::from_roles`] is the single
/// place union semantics exist, so a hand-rolled construction can never
/// disagree with the substrate's resolution rules.
#[derive(Clone)]
pub struct VerifiedKey {
    pub id: i64,
    pub prefix: String,
    pub name: String,
    pub kind: PrincipalKind,
    roles: Vec<String>,
    permissions: BTreeMap<String, Vec<String>>,
    rate_rpm: Option<u32>,
}

#[allow(
    clippy::missing_fields_in_debug,
    reason = "sensitive identity and authorization fields are deliberately omitted"
)]
impl std::fmt::Debug for VerifiedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let permission_count: usize = self.permissions.values().map(Vec::len).sum();

        f.debug_struct("VerifiedKey")
            .field("id", &self.id)
            .field("prefix", &self.prefix)
            .field("kind", &self.kind)
            .field("role_count", &self.roles.len())
            .field("app_count", &self.permissions.len())
            .field("permission_count", &permission_count)
            .field("has_rate_override", &self.rate_rpm.is_some())
            .finish()
    }
}

impl VerifiedKey {
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

    pub fn roles(&self) -> &[String] {
        &self.roles
    }

    pub fn permissions_for(&self, app: &str) -> &[String] {
        self.permissions.get(app).map_or(&[], Vec::as_slice)
    }

    pub fn has_app_permission(&self, app: &str, permission: &str) -> bool {
        self.permissions
            .get(app)
            .is_some_and(|perms| perms.iter().any(|p| p == permission))
    }

    pub fn has_any_permission(&self, app: &str) -> bool {
        self.permissions.get(app).is_some_and(|p| !p.is_empty())
    }

    pub fn rate_rpm(&self) -> Option<u32> {
        self.rate_rpm
    }

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

    #[test]
    fn verified_key_debug_is_bounded_to_identity_and_aggregate_fields() {
        let rate_value = 4_000_000_007;
        let key = VerifiedKey::from_roles(
            42,
            "safe-prefix",
            "name-secret-sentinel",
            PrincipalKind::Service,
            vec![
                role(
                    "role-secret-one",
                    Some(rate_value),
                    &[
                        ("app-secret-one", "permission-secret-one"),
                        ("app-secret-one", "permission-secret-two"),
                    ],
                ),
                role(
                    "role-secret-two",
                    None,
                    &[("app-secret-two", "permission-secret-three")],
                ),
            ],
        );

        let debug = format!("{key:?}");
        assert_eq!(
            debug,
            "VerifiedKey { id: 42, prefix: \"safe-prefix\", kind: Service, role_count: 2, app_count: 2, permission_count: 3, has_rate_override: true }"
        );
        for secret in [
            "name-secret-sentinel",
            "role-secret-one",
            "role-secret-two",
            "app-secret-one",
            "app-secret-two",
            "permission-secret-one",
            "permission-secret-two",
            "permission-secret-three",
        ] {
            assert!(!debug.contains(secret), "debug leaked {secret}: {debug}");
        }
        assert!(
            !debug.contains(&rate_value.to_string()),
            "debug leaked the rate override value: {debug}"
        );
    }

    #[test]
    fn verified_key_debug_reports_empty_aggregates() {
        let debug = format!(
            "{:?}",
            VerifiedKey::from_roles(
                7,
                "empty-prefix",
                "empty-name-secret",
                PrincipalKind::Human,
                vec![],
            )
        );

        assert_eq!(
            debug,
            "VerifiedKey { id: 7, prefix: \"empty-prefix\", kind: Human, role_count: 0, app_count: 0, permission_count: 0, has_rate_override: false }"
        );
        assert!(!debug.contains("empty-name-secret"));
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
