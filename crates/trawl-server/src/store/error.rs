// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Error types for the postgres app-state store.
//!
//! Postgres failures are classified centrally by **(SQLSTATE, constraint
//! name)** — never by message text. Every constraint in
//! `crates/trawl-server/migrations/` is named so this map stays exact.

/// Errors from the app-state store (history, saved queries, schedules,
/// report runs).
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The database is unreachable or errored unexpectedly. Maps to 503 on
    /// the wire with a redacted body — pg diagnostics never leave the server.
    #[error("app-state store unavailable: {0}")]
    Unavailable(#[source] sqlx::Error),

    /// Boot-time migration failed.
    #[error("app-state store migration failed: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),

    /// Another trawld instance holds the app-state advisory lock.
    #[error(
        "another trawld instance already holds the app-state database advisory lock \
         (trawl is single-writer by design — stop the other instance or point \
         [storage] database_url at a dedicated database)"
    )]
    LockHeld,

    /// A saved query with this name already exists for this user (23505 on
    /// `saved_queries_key_name_unique`).
    #[error("a saved query named '{name}' already exists")]
    DuplicateName {
        /// The duplicate name.
        name: String,
    },

    /// A schedule already exists for this saved query (23505 on
    /// `schedules_saved_query_unique`).
    #[error("a schedule already exists for saved query id={saved_query_id}")]
    ScheduleExists {
        /// The saved query that already has a schedule.
        saved_query_id: i64,
    },

    /// Resource not found (or not owned by the caller).
    #[error("{resource} not found: id={id}")]
    NotFound {
        /// The resource ID.
        id: i64,
        /// The resource type.
        resource: &'static str,
    },

    /// Input rejected by a CHECK constraint or store-side validation (23514).
    #[error("validation failed: {0}")]
    Validation(String),

    /// The interval string is not a valid format (e.g. "5x", empty).
    #[error("invalid interval format: {input:?}")]
    InvalidInterval {
        /// The raw input string.
        input: String,
    },

    /// Schedule interval is below the minimum (60 seconds).
    #[error("schedule interval {secs}s is below minimum of 60s")]
    IntervalTooShort {
        /// The requested interval in seconds.
        secs: u64,
    },

    /// Saved query name contains invalid characters.
    #[error("invalid saved query name '{name}': must match [a-zA-Z0-9_-]+")]
    InvalidName {
        /// The rejected name.
        name: String,
    },
}

/// A named-constraint violation classified from a postgres error.
///
/// The `(SQLSTATE, constraint name)` pairs here are the single source of
/// truth for turning pg integrity errors into domain errors. Call sites
/// match on the variant and attach their context (names, ids).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PgViolation {
    /// 23505 on `saved_queries_key_name_unique`.
    SavedNameTaken,
    /// 23505 on `schedules_saved_query_unique`.
    ScheduleTaken,
    /// 23505 on `report_runs_one_running` — a run is already in flight.
    RunAlreadyRunning,
    /// 23503 — referenced row is gone (treat as not-found).
    ForeignKey,
    /// 23514 — a CHECK constraint rejected the value.
    Check,
}

/// Classify a sqlx error into a [`PgViolation`], if it is one.
pub(crate) fn classify_violation(e: &sqlx::Error) -> Option<PgViolation> {
    let sqlx::Error::Database(db) = e else {
        return None;
    };
    match (db.code().as_deref(), db.constraint()) {
        (Some("23505"), Some("saved_queries_key_name_unique")) => Some(PgViolation::SavedNameTaken),
        (Some("23505"), Some("schedules_saved_query_unique")) => Some(PgViolation::ScheduleTaken),
        (Some("23505"), Some("report_runs_one_running")) => Some(PgViolation::RunAlreadyRunning),
        (Some("23503"), _) => Some(PgViolation::ForeignKey),
        (Some("23514"), _) => Some(PgViolation::Check),
        _ => None,
    }
}

impl From<sqlx::Error> for StoreError {
    /// Default conversion for un-classified sqlx errors: the backend is
    /// unavailable or misbehaving. Call sites that expect constraint
    /// violations must run [`classify_violation`] BEFORE falling back here.
    fn from(e: sqlx::Error) -> Self {
        Self::Unavailable(e)
    }
}
