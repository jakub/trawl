// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Report-run / query-history status domain.
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
}
