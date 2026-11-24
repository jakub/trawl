// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Error types for the postgres app-state store.
//!
//! Postgres failures are classified centrally by (SQLSTATE, constraint
//! name), never by message text. Every constraint in
//! `crates/trawl-server/migrations/` is named so this map stays exact.

use crate::report_window::WindowPolicyError;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("app-state store unavailable: {0}")]
    Unavailable(#[source] sqlx::Error),

    #[error("app-state store migration failed: {0}")]
    Migration(#[from] super::migrations::SchemaError),

    #[error(
        "another trawld instance already holds the app-state database advisory lock \
         (trawl is single-writer by design — stop the other instance or point \
         [storage] database_url at a dedicated database)"
    )]
    LockHeld,

    #[error("a saved query with this name already exists")]
    DuplicateName { name: String },

    #[error("a schedule already exists for saved query id={saved_query_id}")]
    ScheduleExists { saved_query_id: i64 },

    /// Investigation ids are text, so this is not a [`Self::NotFound`], whose id
    /// is numeric.
    #[error("investigation not found")]
    InvestigationNotFound,

    #[error("the investigation is at sequence {current}, not the expected {expected}")]
    SequenceConflict { expected: u64, current: u64 },

    #[error("the investigation is closed")]
    InvestigationClosed,

    /// Resource not found (or not owned by the caller).
    #[error("{resource} not found: id={id}")]
    NotFound { id: i64, resource: &'static str },

    #[error("validation failed: {0}")]
    Validation(String),

    #[error("invalid interval format: {input:?}")]
    InvalidInterval { input: String },

    #[error("schedule interval {secs}s is below minimum of 60s")]
    IntervalTooShort { secs: u64 },

    #[error(
        "duration {secs}s exceeds the maximum of {} seconds (10 years)",
        crate::store::MAX_DURATION_SECS
    )]
    DurationTooLong { secs: u64 },

    #[error(
        "lag {lag_secs}s needs a report window: without `window` the saved query owns \
         its own time clause and trawl shifts no bounds. Set window to \"since_last\" \
         or a duration, or drop lag"
    )]
    LagWithoutWindow { lag_secs: u64 },

    #[error("name must not be blank or contain control or invisible formatting characters")]
    InvalidName { name: String },

    #[error("a repin job is already running (one at a time, install-wide)")]
    RepinAlreadyRunning,

    /// Distinct from [`Self::Unavailable`]: a failure from BEFORE the commit can
    /// be settled by re-reading `field_types`, while this one cannot, because a
    /// read races the commit. The pin cache is reconciled by over-eviction instead.
    #[error(
        "the field-catalog pin purge could not confirm its commit; postgres may have \
         applied it anyway, so whether the pins were reclaimed is unknown"
    )]
    PurgeCommitUnknown,

    /// Postgres bounds each statement itself, but a connection that stops
    /// answering mid-statement is invisible to a database-side timeout. The purge
    /// holds the corpus gate, and every compaction batch queues behind that, so
    /// the wait is bounded here as well.
    #[error(
        "the field-catalog pin purge gave up waiting on the database before it \
         committed; nothing was reclaimed"
    )]
    PurgePrepareTimeout,

    #[error("the write ran out of time before it committed; nothing was written")]
    WaitExpired,

    /// The engine reads the pin from the in-process cache, so between that
    /// read and the claim a pin gc purge can have reclaimed the slot (or an
    /// earlier repin retyped it). Claiming anyway would leave the cutover
    /// updating a row that no longer exists.
    #[error("{}", repin_pin_message(field, expected, found.as_deref()))]
    RepinPinVanished {
        /// The field the claim named (catalog key).
        field: String,
        /// The pin the caller read, in catalog spelling.
        expected: &'static str,
        found: Option<String>,
    },
}

/// A vanished pin reads exactly as the engine's own unpinned-field refusal.
fn repin_pin_message(field: &str, expected: &str, found: Option<&str>) -> String {
    match found {
        None => format!("{field:?} is not a pinned field, so there is nothing to repin"),
        Some(actual) => format!(
            "{field:?} is pinned {actual}, not the {expected} this request was prepared \
             against; re-read the field's pin and retry"
        ),
    }
}

impl StoreError {
    /// The raw Display of `Unavailable`/`Migration` embeds pg diagnostics, and a
    /// `tracing` event on the persisted path lands in the retained
    /// `service=trawld` corpus, so events log this class instead.
    pub fn class(&self) -> &'static str {
        match self {
            // `Unavailable` absorbs every sqlx failure, so a flat label
            // would be constant at exactly the call sites that swallow the
            // error; the sqlx variant is a closed, content-free subtype.
            Self::Unavailable(e) => match e {
                sqlx::Error::PoolTimedOut => "unavailable_pool_timeout",
                sqlx::Error::PoolClosed => "unavailable_pool_closed",
                sqlx::Error::Io(_) => "unavailable_io",
                sqlx::Error::Tls(_) => "unavailable_tls",
                sqlx::Error::Database(_) => "unavailable_database",
                sqlx::Error::RowNotFound => "unavailable_row_not_found",
                sqlx::Error::ColumnNotFound(_)
                | sqlx::Error::ColumnDecode { .. }
                | sqlx::Error::ColumnIndexOutOfBounds { .. }
                | sqlx::Error::TypeNotFound { .. }
                | sqlx::Error::Decode(_) => "unavailable_decode",
                sqlx::Error::Configuration(_) => "unavailable_configuration",
                _ => "unavailable_other",
            },
            Self::Migration(_) => "migration",
            Self::LockHeld => "lock_held",
            Self::DuplicateName { .. } => "duplicate_name",
            Self::ScheduleExists { .. } => "schedule_exists",
            Self::NotFound { .. } => "not_found",
            Self::InvestigationNotFound => "investigation_not_found",
            Self::SequenceConflict { .. } => "sequence_conflict",
            Self::InvestigationClosed => "investigation_closed",
            Self::Validation(_) => "validation",
            Self::InvalidInterval { .. } => "invalid_interval",
            Self::IntervalTooShort { .. } => "interval_too_short",
            Self::DurationTooLong { .. } => "duration_too_long",
            Self::InvalidName { .. } => "invalid_name",
            Self::LagWithoutWindow { .. } => "lag_without_window",
            Self::RepinAlreadyRunning => "repin_already_running",
            Self::PurgeCommitUnknown => "purge_commit_unknown",
            Self::PurgePrepareTimeout => "purge_prepare_timeout",
            Self::WaitExpired => "wait_expired",
            Self::RepinPinVanished { .. } => "repin_pin_vanished",
        }
    }
}

/// The error of a store write that first had to prove a schedule window and
/// a saved query's text compatible (ADR-0018 rulings 7 and 12).
#[derive(Debug, thiserror::Error)]
pub enum WindowWriteError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Policy(#[from] WindowPolicyError),
}

impl From<sqlx::Error> for WindowWriteError {
    fn from(e: sqlx::Error) -> Self {
        Self::Store(StoreError::from(e))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PgViolation {
    SavedNameTaken,
    ScheduleTaken,
    RunAlreadyRunning,
    RepinAlreadyRunning,
    ResultIdTaken,
    UnknownInvestigation,
    ForeignKey,
    Check,
}

pub(crate) fn classify_violation(e: &sqlx::Error) -> Option<PgViolation> {
    let sqlx::Error::Database(db) = e else {
        return None;
    };
    match (db.code().as_deref(), db.constraint()) {
        (Some("23505"), Some("saved_queries_key_name_unique")) => Some(PgViolation::SavedNameTaken),
        (Some("23505"), Some("schedules_saved_query_unique")) => Some(PgViolation::ScheduleTaken),
        (Some("23505"), Some("report_runs_one_running")) => Some(PgViolation::RunAlreadyRunning),
        (Some("23505"), Some("repin_jobs_one_running")) => Some(PgViolation::RepinAlreadyRunning),
        (Some("23505"), Some("captured_results_pkey")) => Some(PgViolation::ResultIdTaken),
        (Some("23503"), Some("captured_results_investigation_fk")) => {
            Some(PgViolation::UnknownInvestigation)
        }
        (Some("23503"), _) => Some(PgViolation::ForeignKey),
        (Some("23514"), _) => Some(PgViolation::Check),
        _ => None,
    }
}

impl StoreError {
    pub(crate) fn violation(&self) -> Option<PgViolation> {
        match self {
            Self::Unavailable(e) => classify_violation(e),
            _ => None,
        }
    }
}

impl From<sqlx::Error> for StoreError {
    fn from(e: sqlx::Error) -> Self {
        Self::Unavailable(e)
    }
}
