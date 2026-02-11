//! fleet-auth: API key management and role-based authorization.
//!
//! `SQLite`-backed store for API keys with three roles (admin, analyst, reader).
//! Handles key creation, verification, revocation, and permission checking.

/// Error types for the authentication subsystem.
pub mod error;

/// API key data types — metadata, creation results, verified identity.
pub mod keys;

/// Role definitions and permission checking.
pub mod roles;

pub use error::AuthError;
pub use keys::{ApiKeyInfo, CreatedKey, VerifiedKey};
pub use roles::{Permission, Role};
