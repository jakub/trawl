// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! One absolute deadline per unit of work (ADR-0024).

use std::future::Future;
use std::time::Duration;

use tokio::time::Instant;

/// The longest budget a deadline will represent. Adding an operator's
/// unclamped seconds to `now` can overflow the clock's representation;
/// a year is past every real query timeout and cannot.
const MAX_BUDGET: Duration = Duration::from_hours(365 * 24);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Deadline(Instant);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Expired;

impl std::fmt::Display for Expired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("deadline expired")
    }
}

impl std::error::Error for Expired {}

impl Deadline {
    #[must_use]
    pub fn after(budget: Duration) -> Self {
        Self(Instant::now() + budget.min(MAX_BUDGET))
    }

    #[must_use]
    pub fn instant(self) -> Instant {
        self.0
    }

    #[must_use]
    pub fn remaining(self) -> Duration {
        self.0.saturating_duration_since(Instant::now())
    }

    #[must_use]
    pub fn expired(self) -> bool {
        self.remaining().is_zero()
    }

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

        tokio::time::sleep(Duration::from_secs(4)).await;
        assert_eq!(deadline.remaining(), Duration::from_secs(6));

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

    #[tokio::test(start_paused = true)]
    async fn an_expired_deadline_grants_no_further_wait() {
        let deadline = Deadline::after(Duration::from_secs(1));
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(
            deadline.run(std::future::pending::<()>()).await,
            Err(Expired)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_unrepresentable_budget_is_clamped() {
        let clamped = Deadline::after(Duration::from_secs(u64::MAX));
        assert_eq!(clamped.remaining(), super::MAX_BUDGET);
    }
}
