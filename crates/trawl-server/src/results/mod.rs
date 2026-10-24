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
//! agreement: once at boot, then on the scheduler's hourly retention tick, or
//! on a tick of its own every `ttl_secs` (1 s to 1 h) when
//! `[scheduler] enabled = false`.
//! [`charge`] admits each capture: per-key and installation-wide running
//! slots, and the binding quota reservations. [`service`] is the
//! capability itself, transport-free: capture, list, page and cell.
//! `routes` adapts it to the four HTTP routes. [`ResultsState`] is what
//! every part of the capability shares: the root, the metadata store, the
//! limits, the clock every lifecycle decision reads, the gate, and the
//! captures in flight between their file and their row.

use std::collections::HashSet;
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
    /// The captures between their file and their row.
    pub in_flight: Arc<InFlight>,
}

/// The ids of the captures whose file is published, or about to be, and
/// whose row's insert has not been decided (ADR-0057). The sweep never
/// unlinks the file of one, however long its insert takes
/// ([`sweep::sweep`]).
///
/// A capture enters before it publishes and leaves once its insert is
/// decided: committed, or failed with the file unlinked. A capture whose
/// insert failed stays while its fence waits on the first insert's row. One
/// whose fence failed too stays through its grace
/// ([`service::COMMIT_GRACE`]), so no sweep judges its file while the row
/// may still land; it leaves with the file kept, unlinked, or left for
/// boot and the sweep when its row cannot be read. A commit task dropped
/// before its insert is decided, as at shutdown, leaves with its file left
/// for them too. Boot reconciliation runs before any capture can enter.
#[derive(Debug, Default)]
pub struct InFlight(Mutex<HashSet<String>>);

impl InFlight {
    /// Hold `id` in flight until the returned registration drops.
    pub(crate) fn enter(self: &Arc<Self>, id: &str) -> Entered {
        self.0.lock().insert(id.to_owned());
        Entered {
            set: Arc::clone(self),
            id: id.to_owned(),
        }
    }

    /// The ids in flight now.
    #[must_use]
    pub fn snapshot(&self) -> HashSet<String> {
        self.0.lock().clone()
    }
}

/// One capture's place in [`InFlight`]; dropping it leaves.
#[derive(Debug)]
pub(crate) struct Entered {
    set: Arc<InFlight>,
    id: String,
}

impl Drop for Entered {
    fn drop(&mut self) {
        self.set.0.lock().remove(&self.id);
    }
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
    fn a_capture_is_in_flight_until_its_registration_drops() {
        let in_flight = Arc::new(InFlight::default());
        let first = in_flight.enter("a");
        let second = in_flight.enter("b");
        assert_eq!(
            in_flight.snapshot(),
            HashSet::from(["a".to_owned(), "b".to_owned()])
        );
        drop(first);
        assert_eq!(in_flight.snapshot(), HashSet::from(["b".to_owned()]));
        drop(second);
        assert!(in_flight.snapshot().is_empty());
    }

    #[test]
    #[should_panic(expected = "the system clock cannot be set")]
    fn the_system_clock_cannot_be_set() {
        ResultsClock::System.set(Utc::now());
    }
}
