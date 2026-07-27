// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shared auth substrate for fleet apps (ADR-0030).
//!
//! Provides the canonical app-agnostic auth types (`PrincipalKind`, `Role`,
//! `RolePermission`, `VerifiedKey`, `ApiKeyInfo`, `CreatedKey`) and, behind
//! the `keystore` default feature, a Postgres-backed `KeyStore` plus the
//! `flt_*` token module and embedded sqlx migrations. Roles are data-defined
//! cross-app permission bundles (ADR-0006): keys hold any number of roles
//! and effective permissions are the union.

pub mod error;
pub mod types;
pub mod validation;

#[cfg(feature = "keystore")]
pub mod cache;
#[cfg(feature = "keystore")]
pub mod migrations;
#[cfg(feature = "keystore")]
pub mod store;
#[cfg(feature = "keystore")]
pub mod token;

#[cfg(feature = "session")]
pub mod session;

#[cfg(feature = "axum")]
pub mod handlers;
#[cfg(feature = "axum")]
pub mod middleware;

#[cfg(feature = "keystore")]
pub use cache::{VerificationCache, VerificationCacheKey, VerificationCacheStats};
#[cfg(feature = "keystore")]
pub use migrations::MIGRATOR;
#[cfg(feature = "keystore")]
pub use store::KeyStore;

#[cfg(feature = "session")]
pub use session::{
    DEFAULT_COOKIE_NAME, DEFAULT_TTL_SECS, KEY_LEN, NONCE_LEN, OriginRejected, SessionConfig,
    SessionConfigBuilder, SessionError, SessionExpiry, SessionKey, SessionPayload,
    build_clear_cookie_header, build_session_cookie_header, check_origin, decrypt, encrypt,
    is_expired, origin_allowed, request_host,
};
// Re-export `cookie::SameSite` so consumers don't need to add `cookie` as a
// direct dep just to populate `SessionConfig.same_site`.
#[cfg(feature = "session")]
pub use cookie::SameSite;

#[cfg(feature = "axum")]
pub use handlers::{LoginRequest, LoginResponse, login, logout};
#[cfg(feature = "axum")]
pub use middleware::{
    BearerState, SessionState, require_bearer, require_bearer_only, require_session,
};

pub use error::AuthError;
pub use types::{ApiKeyInfo, CreatedKey, PrincipalKind, Role, RolePermission, VerifiedKey};
pub use validation::{
    MAX_APP_NAMESPACE_LEN, MAX_PERMISSION_LEN, MAX_ROLE_NAME_LEN, TRAWL_APP,
    validate_app_namespace, validate_permission, validate_role_name,
};
