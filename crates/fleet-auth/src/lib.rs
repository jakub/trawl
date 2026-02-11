//! fleet-auth: API key management and role-based authorization.
//!
//! SQLite-backed store for API keys with three roles (admin, analyst, reader).
//! Handles key creation, verification, revocation, and permission checking.

/// API key storage and lifecycle management.
pub mod keys;

/// Role definitions and permission checking.
pub mod roles;
