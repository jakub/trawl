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

/// The one place the identifier rules live: non-empty, within a byte
/// budget, and restricted to lowercase ascii alphanumerics + underscore
/// (optionally plus hyphen). Every public validator below is a thin
/// wrapper over this, so a charset or length tweak lands in one spot
/// instead of drifting across three copies — and stays checkable against
/// its Postgres CHECK-constraint twin.
///
/// `kind` names the identifier in error messages ("role name"), and
/// `ctor` picks the [`AuthError`] variant the caller's domain expects.
fn validate_identifier(
    value: &str,
    kind: &str,
    max_len: usize,
    allow_hyphen: bool,
    ctor: fn(String) -> AuthError,
) -> Result<(), AuthError> {
    if value.is_empty() {
        return Err(ctor(format!("{kind} is empty")));
    }
    if value.len() > max_len {
        return Err(ctor(format!("{kind} exceeds {max_len} bytes: {value}")));
    }
    if !value.bytes().all(|b| {
        b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || (allow_hyphen && b == b'-')
    }) {
        let hyphen = if allow_hyphen { " + hyphen" } else { "" };
        return Err(ctor(format!(
            "{kind} must be lowercase ascii alphanum + underscore{hyphen}: {value}"
        )));
    }
    Ok(())
}

/// Validate an app namespace identifier.
///
/// Rules: lowercase ascii + digits + underscore, length 1..=64. Mirrors
/// the `app_permissions_app_format` CHECK.
pub fn validate_app_namespace(app: &str) -> Result<(), AuthError> {
    validate_identifier(
        app,
        "app namespace",
        MAX_APP_NAMESPACE_LEN,
        false,
        AuthError::InvalidApp,
    )
}

/// Validate a role name.
///
/// Rules: lowercase ascii + digits + underscore + hyphen, length 1..=64.
/// The hyphen is the one charset difference from app namespaces and
/// permission strings, which reject it: role names are `<app>-<role>`
/// (`trawl-admin`, `coastwatch-analyst`). Mirrors the `roles_name_format`
/// CHECK.
pub fn validate_role_name(role: &str) -> Result<(), AuthError> {
    validate_identifier(
        role,
        "role name",
        MAX_ROLE_NAME_LEN,
        true,
        AuthError::InvalidRole,
    )
}

/// Validate a permission string.
///
/// Rules: same charset discipline as app namespaces — lowercase ascii +
/// digits + underscore, length 1..=64. Mirrors the
/// `role_permissions_permission_format` CHECK.
pub fn validate_permission(permission: &str) -> Result<(), AuthError> {
    validate_identifier(
        permission,
        "permission string",
        MAX_PERMISSION_LEN,
        false,
        AuthError::InvalidPermission,
    )
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
