// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! One absolute deadline per unit of work (ADR-0024).
//!
//! A per-phase `Duration` is a budget each phase spends in full: a query
//! could queue for the whole timeout, wait for publication for the whole
//! timeout again, and only then start running under a third copy of it.
//! An absolute instant cannot be spent twice. The handler stamps one at
//! authenticated entry and every wait below it asks how much is left.
//!
//! Built on [`tokio::time::Instant`], so a test can drive the whole
//! budget with `tokio::time::pause()` (or `#[tokio::test(start_paused =
//! true)]`) instead of sleeping: a queue wait proven with real seconds is
//! a slow test that proves less.
//!
//! This type says when the budget runs out and nothing else. What an
//! expiry MEANS — a timeout, or a capacity refusal for work that never
//! started — belongs to the lane that was waiting.

use std::future::Future;
use std::time::Duration;

use tokio::time::Instant;

/// The longest budget a deadline will represent. Adding an operator's
/// unclamped seconds to `now` can overflow the clock's representation;
/// a year is past every real query timeout and cannot.
const MAX_BUDGET: Duration = Duration::from_hours(365 * 24);

/// The instant a unit of work must be finished waiting by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Deadline(Instant);

/// The budget ran out before the future finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Expired;

impl std::fmt::Display for Expired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("deadline expired")
    }
}

impl std::error::Error for Expired {}

impl Deadline {
    /// A deadline `budget` from now, clamped to [`MAX_BUDGET`].
    #[must_use]
    pub fn after(budget: Duration) -> Self {
        Self(Instant::now() + budget.min(MAX_BUDGET))
    }

    /// The instant itself, for a wait that sleeps until it.
    #[must_use]
    pub fn instant(self) -> Instant {
        self.0
    }

    /// What is left of the budget: zero once the deadline has passed, so
    /// a wait handed this value fires immediately rather than waiting a
    /// second full timeout.
    #[must_use]
    pub fn remaining(self) -> Duration {
        self.0.saturating_duration_since(Instant::now())
    }

    /// Whether the budget is gone. `now == deadline` is expired.
    #[must_use]
    pub fn expired(self) -> bool {
        self.remaining().is_zero()
    }

    /// Await `f` within what is left of the budget.
    ///
    /// # Errors
    ///
    /// [`Expired`] when the deadline passes first. The future is dropped
    /// at that point, exactly as [`tokio::time::timeout`] drops it.
    pub async fn run<F: Future>(self, f: F) -> Result<F::Output, Expired> {
        tokio::time::timeout_at(self.0, f)
            .await
            .map_err(|_| Expired)
    }
}

#[cfg(test)]
mod tests {
    use super::{Deadline, Expired};
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn the_budget_is_spent_once_across_phases() {
        let deadline = Deadline::after(Duration::from_secs(10));

        // One phase waits four seconds...
        tokio::time::sleep(Duration::from_secs(4)).await;
        assert_eq!(deadline.remaining(), Duration::from_secs(6));

        // ...and the next one sees six, not another ten.
        tokio::time::sleep(Duration::from_secs(6)).await;
        assert!(deadline.expired());
        assert_eq!(deadline.remaining(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn run_returns_the_output_inside_the_budget() {
        let deadline = Deadline::after(Duration::from_secs(10));
        let out = deadline
            .run(async {
                tokio::time::sleep(Duration::from_secs(1)).await;
                7
            })
            .await;
        assert_eq!(out, Ok(7));
    }

    #[tokio::test(start_paused = true)]
    async fn run_expires_on_a_wait_past_the_budget() {
        let deadline = Deadline::after(Duration::from_secs(1));
        let out = deadline
            .run(async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                7
            })
            .await;
        assert_eq!(out, Err(Expired));
        assert!(deadline.expired());
    }

    /// A deadline already past does not hand the next wait a fresh
    /// budget: it fires immediately.
    #[tokio::test(start_paused = true)]
    async fn an_expired_deadline_grants_no_further_wait() {
        let deadline = Deadline::after(Duration::from_secs(1));
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(
            deadline.run(std::future::pending::<()>()).await,
            Err(Expired)
        );
    }

    #[test]
    fn an_unrepresentable_budget_is_clamped() {
        // The clamp is what keeps an operator's `timeout_secs` from
        // overflowing the clock; the value is a year, so any real query
        // budget is unaffected.
        let clamped = Deadline::after(Duration::from_secs(u64::MAX));
        let year = Deadline::after(super::MAX_BUDGET);
        assert!(clamped <= year, "a huge budget lands at the clamp");
    }
}
