//! trawl-auth: API key management and role-based authorization.
//!
//! `SQLite`-backed store for API keys with three roles (admin, analyst, reader).
//! Handles key creation, verification, revocation, and permission checking.

/// Error types for the authentication subsystem.
pub mod error;

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

pub use error::AuthError;
pub use history::{HistoryEntry, HistoryPage, HistoryStore};
pub use keys::{ApiKeyInfo, CreatedKey, VerifiedKey};
pub use roles::{Permission, Role};
pub use saved::{SavedQuery, SavedQueryStore};
pub use schedule::{ReportRun, Schedule, ScheduleStore};
pub use store::KeyStore;
