// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! App namespace + grant validation for the multi-app substrate.
//!
//! These rules keep `(app, role)` strings safe to embed in identifiers,
//! file paths, and SQL string literals without escaping, and reject the
//! obviously-broken inputs (control chars, oversized strings) at the
//! application boundary.

use crate::error::AuthError;
use crate::types::RoleAssignment;

/// The app namespace that trawl uses for its own grants. Exists so
/// trawl-side code can avoid bare string literals when constructing
/// `RoleAssignment { app: TRAWL_APP.into(), role: ... }`.
pub const TRAWL_APP: &str = "trawl";

/// Maximum length of an app namespace identifier, in bytes.
pub const MAX_APP_NAMESPACE_LEN: usize = 64;

/// Maximum length of a role name, in bytes.
const MAX_ROLE_NAME_LEN: usize = 128;

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

/// Validate a role string within a grant.
///
/// Roles are app-defined, but we still apply some hygiene: non-empty,
/// reasonable size, no embedded NULs / ascii control characters.
pub fn validate_role_name(role: &str) -> Result<(), AuthError> {
    if role.is_empty() {
        return Err(AuthError::InvalidRole("role name is empty".into()));
    }
    if role.len() > MAX_ROLE_NAME_LEN {
        return Err(AuthError::InvalidRole(format!(
            "role name exceeds {MAX_ROLE_NAME_LEN} bytes: {role}"
        )));
    }
    if role.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err(AuthError::InvalidRole(
            "role name contains control characters".into(),
        ));
    }
    Ok(())
}

/// Validate a full grant assignment before persisting.
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
        assert!(validate_role_name(&"x".repeat(MAX_ROLE_NAME_LEN + 1)).is_err());
    }

    #[test]
    fn validates_assignment_chains_both_checks() {
        let ok = RoleAssignment {
            app: "trawl".into(),
            role: "admin".into(),
        };
        assert!(validate_assignment(&ok).is_ok());

        let bad_app = RoleAssignment {
            app: "Trawl".into(),
            role: "admin".into(),
        };
        assert!(matches!(
            validate_assignment(&bad_app),
            Err(AuthError::InvalidApp(_))
        ));

        let bad_role = RoleAssignment {
            app: "trawl".into(),
            role: "with\0nul".into(),
        };
        assert!(matches!(
            validate_assignment(&bad_role),
            Err(AuthError::InvalidRole(_))
        ));
    }
}
