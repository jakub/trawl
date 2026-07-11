// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! trawl-auth: `SQLite`-backed history, saved queries, and schedules.
//!
//! The API-key keystore (keys, roles, tokens, `(app, role)` grants) moved to
//! `fleet-auth`'s postgres store in ADR-0004 slice 1; the trawl permission
//! model now lives in `trawl-server`'s policy layer. What remains here is the
//! per-user query state trawld still keeps in `SQLite`: history, saved
//! queries, and their schedules.

/// Error types for the authentication subsystem.
pub mod error;

/// Legacy-db quarantine guard (ADR-0004).
pub mod guard;

/// `SQLite`-backed query history storage.
pub mod history;

/// `SQLite`-backed saved queries storage.
pub mod saved;

/// `SQLite`-backed scheduled query execution and report storage.
pub mod schedule;

pub use error::AuthError;
pub use guard::reject_legacy_keystore;
pub use history::{HistoryEntry, HistoryPage, HistoryStore};
pub use saved::{SavedQuery, SavedQueryStore};
pub use schedule::{ReportRun, Schedule, ScheduleStore};
