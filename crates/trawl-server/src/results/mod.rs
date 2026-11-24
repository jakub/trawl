// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Captured results (ADR-0057): a search executed once and kept as an
//! immutable, typed, expiring file that later reads page without
//! re-running anything.

use std::collections::HashSet;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;

use crate::config::{CaptureConfig, InvestigationConfig};
use crate::investigations::step::StepWriter;
use crate::report_window::truncate_to_micros;
use crate::store::ResultsStore;

pub(crate) mod bounds;
pub mod charge;
pub(crate) mod compare;
pub(crate) mod context;
pub mod root;
pub(crate) mod routes;
pub mod service;
pub(crate) mod sightings;
pub mod sweep;

/// Each reading is passed to SQL as a parameter, never replaced by SQL
/// `now()`, so a test that moves this clock moves every decision with it.
/// Readings are truncated to the microsecond, `TIMESTAMPTZ`'s precision, so
/// an instant compared in Rust is the instant `PostgreSQL` stores.
#[derive(Debug, Clone)]
pub enum ResultsClock {
    System,
    Fixed(Arc<Mutex<DateTime<Utc>>>),
}

impl ResultsClock {
    #[must_use]
    pub fn fixed(at: DateTime<Utc>) -> Self {
        Self::Fixed(Arc::new(Mutex::new(truncate_to_micros(at))))
    }

    #[must_use]
    pub fn now(&self) -> DateTime<Utc> {
        match self {
            Self::System => truncate_to_micros(Utc::now()),
            Self::Fixed(at) => *at.lock(),
        }
    }

    /// # Panics
    /// On [`Self::System`], which only the operating system moves.
    pub fn set(&self, at: DateTime<Utc>) {
        match self {
            Self::System => panic!("the system clock cannot be set"),
            Self::Fixed(now) => *now.lock() = truncate_to_micros(at),
        }
    }
}

#[derive(Debug)]
pub struct ResultsState {
    pub root: Arc<root::ResultsRoot>,
    pub store: ResultsStore,
    pub capture: CaptureConfig,
    pub investigation: InvestigationConfig,
    pub clock: ResultsClock,
    pub gate: charge::CaptureGate,
    pub in_flight: Arc<InFlight>,
    pub steps: StepWriter,
}

/// The ids of the captures whose file is published, or about to be, and
/// whose row's insert has not been decided (ADR-0057). The sweep never
/// unlinks the file of one, however long its insert takes
/// ([`sweep::sweep`]).
#[derive(Debug, Default)]
pub struct InFlight(Mutex<HashSet<String>>);

impl InFlight {
    pub(crate) fn enter(self: &Arc<Self>, id: &str) -> Entered {
        self.0.lock().insert(id.to_owned());
        Entered {
            set: Arc::clone(self),
            id: id.to_owned(),
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> HashSet<String> {
        self.0.lock().clone()
    }
}

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
pub(crate) mod logged {
    use std::sync::{Arc, Mutex};

    use crate::store::StoreError;

    pub(crate) fn logged(log: impl FnOnce()) -> String {
        #[derive(Clone, Default)]
        struct Sink(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Sink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let sink = Sink::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer({
                let sink = sink.clone();
                move || sink.clone()
            })
            .finish();
        tracing::subscriber::with_default(subscriber, log);
        String::from_utf8(sink.0.lock().unwrap().clone()).unwrap()
    }

    pub(crate) fn sentinel() -> StoreError {
        StoreError::Unavailable(sqlx::Error::Protocol("SENTINEL-backend-diagnostic".into()))
    }

    pub(crate) fn assert_class_only(logged: &str, event_type: &str, error: &StoreError) {
        assert!(
            logged.contains(&format!("event_type=\"{event_type}\"")),
            "{logged}"
        );
        assert!(
            logged.contains(&format!("error_class=\"{}\"", error.class())),
            "{logged}"
        );
        assert!(!logged.contains("SENTINEL"), "{logged}");
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
