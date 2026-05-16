// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared auth substrate for fleet apps (ADR-0030).
//!
//! Provides the canonical app-agnostic auth types (`PrincipalKind`,
//! `RoleAssignment`, `VerifiedKey`, `ApiKeyInfo`, `CreatedKey`) and, behind
//! the `keystore` default feature, a Postgres-backed `KeyStore` plus the
//! `flt_*` token module and embedded sqlx migrations.

pub mod error;
pub mod types;
pub mod validation;

#[cfg(feature = "keystore")]
pub mod token;

pub use error::AuthError;
pub use types::{ApiKeyInfo, CreatedKey, PrincipalKind, RoleAssignment, VerifiedKey};
pub use validation::{
    MAX_APP_NAMESPACE_LEN, TRAWL_APP, validate_app_namespace, validate_assignment,
    validate_role_name,
};
