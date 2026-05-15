// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! trawl-auth: API key management and multi-app role-based authorization.
//!
//! `SQLite`-backed store for API keys with namespaced `(app, role)` grants.
//! Handles key creation, verification, revocation, and permission checking.

/// Error types for the authentication subsystem.
pub mod error;

/// App namespaces and `(app, role)` grant validation.
pub mod assignments;

/// API key data types — metadata, creation results, verified identity.
pub mod keys;

/// Role definitions and permission checking.
pub mod roles;

/// `SQLite`-backed key storage.
pub mod store;

/// `SQLite`-backed query history storage.
pub mod history;

/// `SQLite`-backed saved queries storage.
pub mod saved;

/// `SQLite`-backed scheduled query execution and report storage.
pub mod schedule;

/// Token generation, hashing, and verification.
pub mod token;

pub use assignments::{
    MAX_APP_NAMESPACE_LEN, PrincipalKind, RoleAssignment, TRAWL_APP, validate_app_namespace,
    validate_assignment, validate_role_name,
};
pub use error::AuthError;
pub use history::{HistoryEntry, HistoryPage, HistoryStore};
pub use keys::{ApiKeyInfo, CreatedKey, VerifiedKey};
pub use roles::{Permission, Role};
pub use saved::{SavedQuery, SavedQueryStore};
pub use schedule::{ReportRun, Schedule, ScheduleStore};
pub use store::KeyStore;
