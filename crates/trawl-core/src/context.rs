// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The evaluation context: the one instant a unit of output reads
//! `now()` at (ADR-0017 §3).
//!
//! [`chrono::Utc::now`] carries nanoseconds; `DuckDB`'s TIMESTAMP domain
//! is MICROSECONDS. Truncating at construction makes every reader of an
//! `EvalContext` see an instant `DuckDB` can represent exactly.

use chrono::{DateTime, SubsecRound as _, Utc};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvalContext {
    now: DateTime<Utc>,
}

impl EvalContext {
    #[must_use]
    pub fn capture() -> Self {
        Self::at(Utc::now())
    }

    #[must_use]
    pub fn at(now: DateTime<Utc>) -> Self {
        Self {
            now: now.trunc_subsecs(6),
        }
    }

    #[must_use]
    pub fn now_utc(&self) -> DateTime<Utc> {
        self.now
    }

    /// The anchor as the naive UTC wall clock the SQL parameter binds —
    /// `DuckDB` TIMESTAMP is zoneless, and every conforming connection
    /// runs with `TimeZone='UTC'`
    /// ([`crate::conform::SESSION_TIME_ZONE_SQL`]).
    #[must_use]
    pub fn now_timestamp(&self) -> chrono::NaiveDateTime {
        self.now.naive_utc()
    }

    #[must_use]
    pub fn now_value(&self) -> crate::eval::EvalValue {
        crate::eval::EvalValue::Timestamp(crate::compare::Instant::At(self.now_timestamp()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn at_truncates_nanoseconds_to_microseconds() {
        let ctx = EvalContext::at(fixed("2026-08-24T10:11:12.123456789Z"));
        assert_eq!(
            ctx.now_utc(),
            fixed("2026-08-24T10:11:12.123456Z"),
            "sub-microsecond digits must be dropped, not rounded"
        );
    }

    #[test]
    fn truncation_never_rounds_up() {
        let ctx = EvalContext::at(fixed("2026-08-24T10:11:12.999999999Z"));
        assert_eq!(ctx.now_utc(), fixed("2026-08-24T10:11:12.999999Z"));
    }

    #[test]
    fn capture_truncates_too() {
        let ctx = EvalContext::capture();
        assert_eq!(
            ctx.now_utc().timestamp_subsec_nanos() % 1_000,
            0,
            "a captured anchor must hold whole microseconds"
        );
        assert_eq!(
            ctx,
            EvalContext::at(ctx.now_utc()),
            "re-anchoring is stable"
        );
    }

    #[test]
    fn accessors_agree_on_one_instant() {
        let ctx = EvalContext::at(fixed("2026-08-24T10:11:12.123456Z"));
        assert_eq!(ctx.now_timestamp(), ctx.now_utc().naive_utc());
        assert_eq!(
            ctx.now_value(),
            crate::eval::EvalValue::Timestamp(crate::compare::Instant::At(ctx.now_timestamp()))
        );
    }

    #[test]
    fn equality_is_by_instant() {
        let at = fixed("2026-08-24T10:11:12.123456Z");
        assert_eq!(EvalContext::at(at), EvalContext::at(at));
        assert_ne!(
            EvalContext::at(at),
            EvalContext::at(at + chrono::Duration::microseconds(1))
        );
    }
}
