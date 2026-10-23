// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Captured results (ADR-0057): a search executed once and kept as an
//! immutable, typed, expiring file that later reads page without
//! re-running anything.
//!
//! `bounds` decides what a capture executes: the one window it covers,
//! the text with that window spliced in, and whether the result is
//! event-shaped. [`root`] holds the results root, where each capture's
//! file is staged and published. [`sweep`] keeps the root and the rows in
//! agreement: once at boot, and on the scheduler's hourly retention tick.
//! [`charge`] admits each capture: per-key and installation-wide running
//! slots, and the binding quota reservations. [`service`] is the
//! capability itself, transport-free: capture, list, page and cell.
//! `routes` adapts it to the four HTTP routes. [`ResultsState`] is what
//! every part of the capability shares: the root, the metadata store, the
//! limits, the clock every lifecycle decision reads, and the gate.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;

use crate::config::CaptureConfig;
use crate::report_window::truncate_to_micros;
use crate::store::ResultsStore;

pub(crate) mod bounds;
pub mod charge;
pub mod root;
pub(crate) mod routes;
pub mod service;
pub mod sweep;

/// The clock every lifecycle decision about captured results reads:
/// expiry, tombstone age, quota usage, the sweep and boot reconciliation.
/// Each reading is passed to SQL as a parameter, never replaced by SQL
/// `now()`, so a test that moves this clock moves every decision with it.
///
/// Readings are truncated to the microsecond, `TIMESTAMPTZ`'s precision, so
/// an instant compared in Rust is the instant `PostgreSQL` stores.
#[derive(Debug, Clone)]
pub enum ResultsClock {
    /// The system's clock, as trawld runs.
    System,
    /// A clock that reads what it was last set to, shared by every clone,
    /// for tests.
    Fixed(Arc<Mutex<DateTime<Utc>>>),
}

impl ResultsClock {
    /// A fixed clock reading `at`.
    #[must_use]
    pub fn fixed(at: DateTime<Utc>) -> Self {
        Self::Fixed(Arc::new(Mutex::new(truncate_to_micros(at))))
    }

    /// The current instant, to the microsecond.
    #[must_use]
    pub fn now(&self) -> DateTime<Utc> {
        match self {
            Self::System => truncate_to_micros(Utc::now()),
            Self::Fixed(at) => *at.lock(),
        }
    }

    /// Move a fixed clock to `at`, for every clone of it.
    ///
    /// # Panics
    /// On [`Self::System`], which only the operating system moves.
    pub fn set(&self, at: DateTime<Utc>) {
        match self {
            Self::System => panic!("the system clock cannot be set"),
            Self::Fixed(now) => *now.lock() = truncate_to_micros(at),
        }
    }
}

/// What the captured-results capability shares across requests, the
/// scheduler's sweep and boot.
#[derive(Debug)]
pub struct ResultsState {
    /// The results root, held for the life of the process.
    pub root: Arc<root::ResultsRoot>,
    /// The metadata rows.
    pub store: ResultsStore,
    /// The `[capture]` limits.
    pub capture: CaptureConfig,
    /// The clock every lifecycle decision reads.
    pub clock: ResultsClock,
    /// Capture admission, built from the same limits, store, root and
    /// clock.
    pub gate: charge::CaptureGate,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fixed_clock_reads_what_it_was_set_to_in_every_clone() {
        let start = DateTime::parse_from_rfc3339("2026-10-23T10:00:00.123456789Z")
            .unwrap()
            .with_timezone(&Utc);
        let clock = ResultsClock::fixed(start);
        let clone = clock.clone();
        assert_eq!(clone.now(), truncate_to_micros(start));
        let later = start + chrono::TimeDelta::minutes(15);
        clock.set(later);
        assert_eq!(clone.now(), truncate_to_micros(later));
        assert_eq!(clone.now().timestamp_subsec_nanos() % 1_000, 0);
    }

    #[test]
    fn the_system_clock_reads_microseconds() {
        assert_eq!(
            ResultsClock::System.now().timestamp_subsec_nanos() % 1_000,
            0
        );
    }

    #[test]
    #[should_panic(expected = "the system clock cannot be set")]
    fn the_system_clock_cannot_be_set() {
        ResultsClock::System.set(Utc::now());
    }
}
