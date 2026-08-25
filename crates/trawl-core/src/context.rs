// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The evaluation context: the ONE instant a unit of output reads
//! `now()` at (ADR-0017 §3).
//!
//! `now()` is one instant per unit of output, not one per call site. In
//! BATCH that unit is the logical query: the instant is captured here, in
//! Rust, and travels as a bound TIMESTAMP parameter into the SQL prefix
//! and as an [`EvalContext`] into the `rust_stages` tail behind
//! `extract kv`, so a `let` and a `where` in one statement cannot see
//! different clocks and the SQL and the tail agree by construction. (In
//! the LIVE lane the unit is one event, or one emitted aggregate
//! snapshot; the same type carries it.)
//!
//! # Why the truncation lives here
//!
//! [`chrono::Utc::now`] carries nanoseconds; `DuckDB`'s TIMESTAMP domain
//! is MICROSECONDS. An untruncated instant would compare one way (against
//! a nanosecond-precise Rust value) and render another (as the
//! microsecond value `DuckDB` stores), and the bound parameter — micros,
//! necessarily — would differ from the value the in-memory tail holds.
//! Truncating at construction makes that impossible: every reader of an
//! `EvalContext` sees an instant `DuckDB` can represent exactly. The
//! truncation is here and nowhere else, so there is no second rule to
//! drift from.

use chrono::{DateTime, SubsecRound as _, Utc};

/// The instant one unit of output evaluates `now()` at.
///
/// Deliberately NOT widened beyond the instant: a context that also
/// carried pins, a source or a row would make "which context" a question
/// with more than one answer, and the whole point is that there is
/// exactly one per unit of output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvalContext {
    /// Truncated to microseconds at construction — see the module note.
    now: DateTime<Utc>,
}

impl EvalContext {
    /// Capture the current instant: **the one `Utc::now()` in
    /// trawl-core**.
    ///
    /// Call this once per unit of output — at the head of a query lane,
    /// never inside an evaluation function. An evaluator reaching for the
    /// clock is exactly the per-call `now()` this type exists to remove.
    #[must_use]
    pub fn capture() -> Self {
        Self::at(Utc::now())
    }

    /// A context pinned to a fixed instant: tests, and INHERITANCE — a
    /// re-emission of the same logical query (the executor's hot-only
    /// fallback) adopts the original's anchor rather than sampling a
    /// second one.
    #[must_use]
    pub fn at(now: DateTime<Utc>) -> Self {
        Self {
            now: now.trunc_subsecs(6),
        }
    }

    /// The anchor as a zone-aware instant.
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

    /// The anchor as the in-memory evaluator's own value — the same
    /// instant the SQL lane binds, in the shape `eval` compares and
    /// renders.
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

    /// Nanosecond precision is truncated — not rounded — at
    /// construction, so the anchor is always a value `DuckDB`'s
    /// microsecond TIMESTAMP domain holds exactly.
    #[test]
    fn at_truncates_nanoseconds_to_microseconds() {
        let ctx = EvalContext::at(fixed("2026-08-24T10:11:12.123456789Z"));
        assert_eq!(
            ctx.now_utc(),
            fixed("2026-08-24T10:11:12.123456Z"),
            "sub-microsecond digits must be dropped, not rounded"
        );
    }

    /// The rounding direction matters: `.9999995` must NOT become the
    /// next microsecond, or the anchor could name an instant later than
    /// the one the clock reported.
    #[test]
    fn truncation_never_rounds_up() {
        let ctx = EvalContext::at(fixed("2026-08-24T10:11:12.999999999Z"));
        assert_eq!(ctx.now_utc(), fixed("2026-08-24T10:11:12.999999Z"));
    }

    /// `capture()` goes through the same door `at()` does, so a captured
    /// anchor carries no sub-microsecond digits either.
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

    /// The three accessors are three renderings of ONE instant — a
    /// reader that picks the wrong one still gets the same answer.
    #[test]
    fn accessors_agree_on_one_instant() {
        let ctx = EvalContext::at(fixed("2026-08-24T10:11:12.123456Z"));
        assert_eq!(ctx.now_timestamp(), ctx.now_utc().naive_utc());
        assert_eq!(
            ctx.now_value(),
            crate::eval::EvalValue::Timestamp(crate::compare::Instant::At(ctx.now_timestamp()))
        );
    }

    /// Two contexts at the same instant are equal — what lets the
    /// hot-only fallback ASSERT it inherited rather than re-sampled.
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
