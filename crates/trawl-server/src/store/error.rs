// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Error types for the postgres app-state store.
//!
//! Postgres failures are classified centrally by (SQLSTATE, constraint
//! name), never by message text. Every constraint in
//! `crates/trawl-server/migrations/` is named so this map stays exact.

use crate::report_window::WindowPolicyError;

/// Errors from every app-state store facade.
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

    /// A duration exceeded [`crate::store::MAX_DURATION_SECS`]. Every
    /// duration the schedule grammar parses becomes date arithmetic, so the
    /// grammar bounds them all rather than each consumer proving itself
    /// total.
    #[error(
        "duration {secs}s exceeds the maximum of {} seconds (10 years)",
        crate::store::MAX_DURATION_SECS
    )]
    DurationTooLong {
        /// The requested duration in seconds.
        secs: u64,
    },

    /// A report-window `lag` was given on a schedule with no window
    /// (ADR-0018 ruling 6). The handler turns this into a 400.
    #[error(
        "lag {lag_secs}s needs a report window: without `window` the saved query owns \
         its own time clause and trawl shifts no bounds. Set window to \"since_last\" \
         or a duration, or drop lag"
    )]
    LagWithoutWindow {
        /// The requested lag in seconds.
        lag_secs: u64,
    },

    /// Saved query name contains invalid characters.
    #[error("invalid saved query name '{name}': must match [a-zA-Z0-9_-]+")]
    InvalidName {
        /// The rejected name.
        name: String,
    },

    /// A repin job is already running (23505 on `repin_jobs_one_running`)
    /// — one shadow rewrite at a time, install-wide.
    #[error("a repin job is already running (one at a time, install-wide)")]
    RepinAlreadyRunning,

    /// The pin purge reached its commit and could not confirm the outcome:
    /// either the commit outstayed its bound and was detached rather than
    /// cancelled, or it completed with an error postgres may have applied
    /// anyway.
    ///
    /// Distinct from [`Self::Unavailable`] because the caller must act
    /// differently: a failure from BEFORE the commit can be settled by
    /// re-reading `field_types`, while this one cannot. A detached commit
    /// is still running, and a failed one may have been made durable before
    /// the connection dropped, so a read races the commit either way and
    /// can answer with the state on either side of it. The pin cache is
    /// reconciled by over-eviction instead.
    #[error(
        "the field-catalog pin purge could not confirm its commit; postgres may have \
         applied it anyway, so whether the pins were reclaimed is unknown"
    )]
    PurgeCommitUnknown,

    /// The pin purge's pre-commit work ran past its client-side bound and
    /// was cancelled, so nothing was committed.
    ///
    /// Postgres bounds each of those statements itself, but a connection
    /// that stops answering mid-statement is invisible to a database-side
    /// timeout: the backend is fine and the client is waiting on a socket
    /// nobody will write to. The purge holds the corpus gate, and every
    /// compaction batch queues behind that, so the wait is bounded here as
    /// well. Cancelling before the commit is safe by construction — the
    /// dropped transaction rolls back — which is why this is an ordinary
    /// bounded failure and [`Self::PurgeCommitUnknown`] is not.
    #[error(
        "the field-catalog pin purge gave up waiting on the database before it \
         committed; nothing was reclaimed"
    )]
    PurgePrepareTimeout,

    /// The claim's `from` pin was not in `field_types` when the claim
    /// transaction looked, under the catalog lifecycle lock.
    ///
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
        /// The pin `field_types` actually holds, when it holds one.
        found: Option<String>,
    },
}

/// The sentence for [`StoreError::RepinPinVanished`].
///
/// A vanished pin reads exactly as the engine's own unpinned-field refusal,
/// because from the operator's side it is the same fact: the field is not
/// pinned, so there is nothing to repin. A pin that merely CHANGED gets its
/// own sentence, since retrying against the current pin is the remedy.
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
    /// A closed-set class label for log events, mirroring
    /// [`crate::error::ServerError::error_class`]'s rationale: the raw
    /// Display of `Unavailable`/`Migration` embeds pg diagnostics, and a
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
            Self::Validation(_) => "validation",
            Self::InvalidInterval { .. } => "invalid_interval",
            Self::IntervalTooShort { .. } => "interval_too_short",
            Self::DurationTooLong { .. } => "duration_too_long",
            Self::InvalidName { .. } => "invalid_name",
            Self::LagWithoutWindow { .. } => "lag_without_window",
            Self::RepinAlreadyRunning => "repin_already_running",
            Self::PurgeCommitUnknown => "purge_commit_unknown",
            Self::PurgePrepareTimeout => "purge_prepare_timeout",
            Self::RepinPinVanished { .. } => "repin_pin_vanished",
        }
    }
}

/// The error of a store write that first had to prove a schedule window and
/// a saved query's text compatible (ADR-0018 rulings 7 and 12).
///
/// Two write paths reach that rule from opposite sides —
/// [`crate::store::ScheduleStore::set_schedule_checked`] attaches a window
/// to standing text, [`crate::store::SavedQueryStore::update_checked`]
/// replaces the text under a standing window — and both can fail either as
/// a store fault or as a policy refusal. They stay apart because the wire
/// treatments differ: a store fault is redacted or mapped by SQLSTATE,
/// while the refusal is the operator's own two inputs and reaches the
/// client intact.
///
/// It is deliberately not a [`StoreError`] variant, for the reason
/// [`crate::store::DueClaimError`] gives: `ServerError` already maps
/// [`WindowPolicyError`] to a 400 that keeps its message, and a second
/// route to the same wire would be free to drift from the first.
#[derive(Debug, thiserror::Error)]
pub enum WindowWriteError {
    /// The app-state store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The window and the query text cannot both say what the report
    /// covers.
    #[error(transparent)]
    Policy(#[from] WindowPolicyError),
}

impl From<sqlx::Error> for WindowWriteError {
    fn from(e: sqlx::Error) -> Self {
        Self::Store(StoreError::from(e))
    }
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
    /// 23505 on `repin_jobs_one_running` — a repin job is already running.
    RepinAlreadyRunning,
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
        (Some("23505"), Some("repin_jobs_one_running")) => Some(PgViolation::RepinAlreadyRunning),
        (Some("23503"), _) => Some(PgViolation::ForeignKey),
        (Some("23514"), _) => Some(PgViolation::Check),
        _ => None,
    }
}

impl From<sqlx::Error> for StoreError {
    /// Default conversion for un-classified sqlx errors: the backend is
    /// unavailable or misbehaving. Call sites that expect constraint
    /// violations must run [`classify_violation`] before falling back here.
    fn from(e: sqlx::Error) -> Self {
        Self::Unavailable(e)
    }
}
