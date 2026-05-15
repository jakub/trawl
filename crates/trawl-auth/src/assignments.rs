// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! App namespaces and `(app, role)` grant validation for the ADR-0021
//! multi-app substrate.
//!
//! The wire types [`PrincipalKind`] and [`RoleAssignment`] are owned by
//! `trawl-api` and re-exported here so callers that depend on `trawl-auth`
//! don't need a separate `trawl-api` dep just to construct a grant.

pub use trawl_api::{PrincipalKind, RoleAssignment};

use crate::error::AuthError;

/// The app namespace that the trawl server itself uses for its own grants.
///
/// Other apps (e.g. coastwatch) use their own namespace strings; this
/// constant exists so trawl-internal code can avoid bare string literals.
pub const TRAWL_APP: &str = "trawl";

/// Maximum length of an app namespace identifier, in bytes.
pub const MAX_APP_NAMESPACE_LEN: usize = 64;

/// Validate an app namespace identifier.
///
/// App namespaces are lowercase ascii alphanum + underscore, 1..=64 chars.
/// The intent is to keep them safe to embed in identifiers, file paths,
/// and SQL string literals without escaping.
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

/// Validate the role string within a grant.
///
/// Roles are app-defined, but we still want some hygiene: non-empty,
/// reasonable size, no embedded NULs / control chars.
pub fn validate_role_name(role: &str) -> Result<(), AuthError> {
    if role.is_empty() {
        return Err(AuthError::InvalidRole("role name is empty".into()));
    }
    if role.len() > 128 {
        return Err(AuthError::InvalidRole(format!(
            "role name exceeds 128 bytes: {role}"
        )));
    }
    if role.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err(AuthError::InvalidRole(
            "role name contains control characters".into(),
        ));
    }
    Ok(())
}

/// Validate a full `RoleAssignment` before persisting.
pub fn validate_assignment(assignment: &RoleAssignment) -> Result<(), AuthError> {
    validate_app_namespace(&assignment.app)?;
    validate_role_name(&assignment.role)?;
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
    fn accepts_role_names() {
        for s in ["admin", "siem_consumer", "Role With Spaces", "ANALYST"] {
            assert!(validate_role_name(s).is_ok(), "should accept {s}");
        }
    }

    #[test]
    fn rejects_bad_role_names() {
        assert!(validate_role_name("").is_err());
        assert!(validate_role_name("with\0nul").is_err());
        assert!(validate_role_name("with\nnewline").is_err());
    }

    #[test]
    fn validates_assignment() {
        let a = RoleAssignment {
            app: "trawl".into(),
            role: "admin".into(),
        };
        assert!(validate_assignment(&a).is_ok());

        let bad_app = RoleAssignment {
            app: "Trawl".into(),
            role: "admin".into(),
        };
        assert!(validate_assignment(&bad_app).is_err());
    }
}
