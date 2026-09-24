// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Report-run / query-history status domain, and how a report run started.
//!
//! The store persists `status` as `TEXT` guarded by the
//! `report_runs_status_check` / `query_history_status_check` CHECK
//! constraints. [`RunStatus`] mirrors that domain in the type system so
//! write paths ([`super::schedule::ScheduleStore::finish_run`],
//! [`super::history::HistoryStore::record_query`]) can't emit an
//! out-of-domain string by construction; the DB CHECK stays as the backstop
//! against raw-SQL drift. `running` is report-run only — query history never
//! records it, but sharing one enum keeps the store boundary uniform and the
//! CHECK enforces the narrower history domain.

use std::str::FromStr;

/// Lifecycle status of a report run or query-history entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    /// A report run is in flight (report runs only; never query history).
    Running,
    /// Completed successfully.
    Success,
    /// Failed with an error.
    Error,
    /// Exceeded the configured timeout.
    Timeout,
}

impl RunStatus {
    /// The canonical lowercase spelling bound into SQL — the single source of
    /// truth for what lands in the `status` column.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Success => "success",
            Self::Error => "error",
            Self::Timeout => "timeout",
        }
    }
}

impl FromStr for RunStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "running" => Ok(Self::Running),
            "success" => Ok(Self::Success),
            "error" => Ok(Self::Error),
            "timeout" => Ok(Self::Timeout),
            other => Err(format!("unknown run status: {other}")),
        }
    }
}

/// How a report run started (ADR-0018 amended 2026-09-23).
///
/// Persisted in `report_runs.origin`, guarded by the `report_runs_origin`
/// CHECK. Every claim names one, so the run history can tell a boundary the
/// scheduler fired from a window an operator fired early, and a manual
/// run's successful finish knows to consume the fire cursor it overtook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOrigin {
    /// Claimed by the scheduler at a planned fire boundary.
    Scheduled,
    /// Fired by an operator through `POST /api/v1/saved/{id}/run`.
    Manual,
}

impl RunOrigin {
    /// The canonical spelling bound into SQL and written to the wire.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::Manual => "manual",
        }
    }
}

impl FromStr for RunOrigin {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "scheduled" => Ok(Self::Scheduled),
            "manual" => Ok(Self::Manual),
            other => Err(format!("unknown run origin: {other}")),
        }
    }
}

/// Decode a nullable `origin` column. NULL is a run claimed before origins
/// were recorded; an unreadable value is a decode error, for the reason
/// [`decode_status`] gives.
pub(crate) fn decode_origin(
    row: &sqlx::postgres::PgRow,
    col: &str,
) -> Result<Option<RunOrigin>, sqlx::Error> {
    use sqlx::Row as _;
    row.try_get::<Option<String>, _>(col)?
        .map(|s| s.parse().map_err(|e: String| sqlx::Error::Decode(e.into())))
        .transpose()
}

/// Decode a `status` column into [`RunStatus`], turning an out-of-domain
/// value (only reachable via raw-SQL drift past the CHECK) into a decode
/// error rather than a silent bad state.
pub(crate) fn decode_status(
    row: &sqlx::postgres::PgRow,
    col: &str,
) -> Result<RunStatus, sqlx::Error> {
    use sqlx::Row as _;
    row.try_get::<String, _>(col)?
        .parse()
        .map_err(|e: String| sqlx::Error::Decode(e.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_str_roundtrips_through_from_str() {
        for s in [
            RunStatus::Running,
            RunStatus::Success,
            RunStatus::Error,
            RunStatus::Timeout,
        ] {
            assert_eq!(s.as_str().parse::<RunStatus>(), Ok(s));
        }
    }

    #[test]
    fn from_str_rejects_unknown() {
        assert!("bogus".parse::<RunStatus>().is_err());
    }

    #[test]
    fn origin_as_str_roundtrips_through_from_str() {
        for o in [RunOrigin::Scheduled, RunOrigin::Manual] {
            assert_eq!(o.as_str().parse::<RunOrigin>(), Ok(o));
        }
        assert!("Manual".parse::<RunOrigin>().is_err());
    }

    /// The enum and the `report_runs_origin` CHECK are one vocabulary: an
    /// origin only the enum knows is a claim the database refuses.
    #[test]
    fn origin_vocabulary_matches_the_migration_check() {
        const SQL: &str = include_str!("../../migrations/20260924000001_report_run_origin.sql");
        let list = SQL
            .split_once("origin IN (")
            .expect("the origin CHECK's IN list")
            .1;
        let list = &list[..list.find(')').expect("the IN list closes")];
        let spelled: Vec<&str> = list
            .split(',')
            .map(|s| s.trim().trim_matches('\''))
            .collect();
        assert_eq!(
            spelled,
            [RunOrigin::Scheduled.as_str(), RunOrigin::Manual.as_str()]
        );
    }
}
