// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Report-run / query-history status domain, and how a report run started.
//!
//! `running` is report-run only — query history never records it; the
//! CHECK enforces the narrower history domain.

use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Running,
    Success,
    Error,
    Timeout,
}

impl RunStatus {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOrigin {
    Scheduled,
    Manual,
}

impl RunOrigin {
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
