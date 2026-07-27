// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! App namespace, role-name, and permission-string validation for the
//! multi-app substrate.
//!
//! These rules keep identifiers safe to embed in log fields, file paths,
//! and SQL string literals without escaping, and reject the
//! obviously-broken inputs at the application boundary. They mirror the
//! CHECK constraints on the `roles` / `role_permissions` /
//! `app_permissions` tables.

use crate::error::AuthError;

/// The app namespace that trawl uses for its own grants. Exists so
/// trawl-side code can avoid bare string literals when asking permission
/// questions (`verified.has_app_permission(TRAWL_APP, ...)`).
pub const TRAWL_APP: &str = "trawl";

/// Maximum length of an app namespace identifier, in bytes.
pub const MAX_APP_NAMESPACE_LEN: usize = 64;

/// Maximum length of a role name, in bytes.
pub const MAX_ROLE_NAME_LEN: usize = 64;

/// Maximum length of a permission string, in bytes.
pub const MAX_PERMISSION_LEN: usize = 64;

/// Validate an app namespace identifier.
///
/// Rules: lowercase ascii + digits + underscore, length 1..=64.
pub fn validate_app_namespace(app: &str) -> Result<(), AuthError> {
    if app.is_empty() {
        return Err(AuthError::InvalidApp("app namespace is empty".into()));
    }
    if app.len() > MAX_APP_NAMESPACE_LEN {
        return Err(AuthError::InvalidApp(format!(
            "app namespace exceeds {MAX_APP_NAMESPACE_LEN} bytes: {app}"
        )));
    }
    if !app
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err(AuthError::InvalidApp(format!(
            "app namespace must be lowercase ascii alphanum + underscore: {app}"
        )));
    }
    Ok(())
}

/// Validate a role name.
///
/// Rules: lowercase ascii + digits + underscore + hyphen, length 1..=64 —
/// the hyphen is required by the converted legacy names (`trawl-admin`,
/// `coastwatch-analyst`, …). Mirrors the `roles_name_format` CHECK.
pub fn validate_role_name(role: &str) -> Result<(), AuthError> {
    if role.is_empty() {
        return Err(AuthError::InvalidRole("role name is empty".into()));
    }
    if role.len() > MAX_ROLE_NAME_LEN {
        return Err(AuthError::InvalidRole(format!(
            "role name exceeds {MAX_ROLE_NAME_LEN} bytes: {role}"
        )));
    }
    if !role
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    {
        return Err(AuthError::InvalidRole(format!(
            "role name must be lowercase ascii alphanum + underscore + hyphen: {role}"
        )));
    }
    Ok(())
}

/// Validate a permission string.
///
/// Rules: same charset discipline as app namespaces — lowercase ascii +
/// digits + underscore, length 1..=64. Mirrors the
/// `role_permissions_permission_format` CHECK.
pub fn validate_permission(permission: &str) -> Result<(), AuthError> {
    if permission.is_empty() {
        return Err(AuthError::InvalidPermission(
            "permission string is empty".into(),
        ));
    }
    if permission.len() > MAX_PERMISSION_LEN {
        return Err(AuthError::InvalidPermission(format!(
            "permission string exceeds {MAX_PERMISSION_LEN} bytes: {permission}"
        )));
    }
    if !permission
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err(AuthError::InvalidPermission(format!(
            "permission string must be lowercase ascii alphanum + underscore: {permission}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_well_formed_namespaces() {
        for s in [
            "trawl",
            "coastwatch",
            "siem",
            "a_b_c_123",
            "x",
            "_underscore",
        ] {
            assert!(validate_app_namespace(s).is_ok(), "should accept {s}");
        }
    }

    #[test]
    fn rejects_bad_namespaces() {
        for s in ["", "Trawl", "trawl-app", "trawl.app", "trawl app", "💀"] {
            assert!(validate_app_namespace(s).is_err(), "should reject {s:?}");
        }
    }

    #[test]
    fn rejects_overlong_namespace() {
        let s = "a".repeat(MAX_APP_NAMESPACE_LEN + 1);
        assert!(validate_app_namespace(&s).is_err());
    }

    #[test]
    fn accepts_role_names_including_converted_legacy_shape() {
        for s in ["trawl-admin", "coastwatch-siem_consumer", "tier1", "a-b_c9"] {
            assert!(validate_role_name(s).is_ok(), "should accept {s}");
        }
    }

    #[test]
    fn rejects_bad_role_names() {
        for s in [
            "",
            "Role With Spaces",
            "ANALYST",
            "with\0nul",
            "with\nnewline",
            "rôle",
        ] {
            assert!(validate_role_name(s).is_err(), "should reject {s:?}");
        }
        assert!(validate_role_name(&"x".repeat(MAX_ROLE_NAME_LEN + 1)).is_err());
    }

    #[test]
    fn accepts_permission_strings() {
        for s in ["query", "schema_read", "ioc_exports_read", "p99"] {
            assert!(validate_permission(s).is_ok(), "should accept {s}");
        }
    }

    #[test]
    fn rejects_bad_permission_strings() {
        for s in ["", "Query", "server-manage", "a b", "trawl:query"] {
            assert!(validate_permission(s).is_err(), "should reject {s:?}");
        }
        assert!(validate_permission(&"x".repeat(MAX_PERMISSION_LEN + 1)).is_err());
    }
}
