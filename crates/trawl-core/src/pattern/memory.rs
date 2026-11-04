// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The server-wide regex reservation (ADR-0063).
//!
//! One process runs one server, so the reservation is a process-wide
//! static that every metered [`super::Preparer`] reaches with no handle
//! passed to it: SSE setup in trawl-server and the batch tail in
//! trawl-engine draw on the same bytes.
//!
//! A metered build reserves before it builds and keeps what it reserved
//! for as long as the pattern lives. It holds one build transient while
//! it compiles, then resizes the hold to the pattern's charge and moves
//! the hold into the pattern, so the bytes return when the last clone of
//! the pattern drops. A request that has answered does not release them;
//! the work that holds the pattern does, when it ends.
//!
//! A hold that does not fit is refused at once. Nothing waits for bytes
//! to come back, so a full reservation answers a query with a capacity
//! refusal, first come, first served.

use std::sync::atomic::{AtomicU64, Ordering};

use super::RegexRefusal;

/// The most regex memory every prepared query in the process may hold at
/// once, in bytes, against the charges [`super::charge`] computes. A
/// fixed constant with no config key, like `MAX_LATERAL_EXPANSION`.
///
/// It holds eight queries at [`super::charge::REGEX_QUERY_BUDGET`]. An
/// ordinary query of a few ASCII patterns charges a few MiB, so many
/// times that many streams fit.
pub const REGEX_RESERVATION_BYTES: u64 = 256 << 20;

/// The regex-bearing parameters one emitted statement binds, by the kind
/// of RE2 program `DuckDB` compiles for each: every copy the emit-time
/// counter counts, lateral and pivot copies included
/// ([`crate::emitter::MAX_REGEX_PARAMS`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RegexSites {
    /// `regexp_extract` patterns, one per `extract` stage.
    pub extract: u64,
    /// Every other regex-bearing parameter: `matches` operands, search
    /// `/regex/` terms, and the copies `DuckDB` binds of them.
    pub matching: u64,
}

/// The limit the process-wide reservation starts with. trawl-core's own
/// unit tests run in parallel threads of one process, so there it has no
/// limit and no test sees another's holds refuse it.
const STARTING_LIMIT: u64 = if cfg!(test) {
    u64::MAX
} else {
    REGEX_RESERVATION_BYTES
};

static RESERVATION: RegexReservation = RegexReservation::new(STARTING_LIMIT);

/// The process-wide regex reservation every metered ledger holds from.
#[must_use]
pub fn reservation() -> &'static RegexReservation {
    &RESERVATION
}

/// A pool of regex memory, in bytes, that holds are taken from.
#[derive(Debug)]
pub struct RegexReservation {
    /// The most `reserved` may come to.
    limit: AtomicU64,
    /// What the live holds add up to.
    reserved: AtomicU64,
    #[cfg(any(test, feature = "test-support"))]
    probe: probe::Probe,
}

impl RegexReservation {
    pub(crate) const fn new(limit: u64) -> Self {
        Self {
            limit: AtomicU64::new(limit),
            reserved: AtomicU64::new(0),
            #[cfg(any(test, feature = "test-support"))]
            probe: probe::Probe::new(),
        }
    }

    /// What the live holds add up to, in bytes.
    #[must_use]
    pub fn reserved(&self) -> u64 {
        self.reserved.load(Ordering::Acquire)
    }

    /// The most the live holds may add up to, in bytes.
    #[must_use]
    pub fn limit(&self) -> u64 {
        self.limit.load(Ordering::Acquire)
    }

    /// Hold `bytes` of the reservation, or `None` at once when they do not
    /// fit beside what is already held.
    #[must_use]
    pub fn try_hold(&'static self, bytes: u64) -> Option<RegexHold> {
        self.grow(bytes).ok()?;
        #[cfg(any(test, feature = "test-support"))]
        self.probe.count_hold();
        Some(RegexHold {
            reservation: self,
            bytes,
        })
    }

    /// Add `bytes` to what is held, if the sum stays within the limit. A
    /// compare-and-swap loop with checked arithmetic, so two holds that
    /// race never pass the limit between them and a sum past `u64::MAX`
    /// is refused, not wrapped.
    fn grow(&self, bytes: u64) -> Result<(), RegexRefusal> {
        let limit = self.limit();
        self.reserved
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                held.checked_add(bytes).filter(|&after| after <= limit)
            })
            .map(|_| ())
            .map_err(|_| RegexRefusal::Reservation)
    }

    /// Return `bytes` to the reservation.
    fn shrink(&self, bytes: u64) {
        self.reserved.fetch_sub(bytes, Ordering::AcqRel);
    }
}

/// Bytes held from a [`RegexReservation`], returned when the hold drops.
///
/// It has no `Clone`, so each hold is returned exactly once. A pattern
/// owns its hold, and the clones of a pattern share it.
#[derive(Debug)]
pub struct RegexHold {
    reservation: &'static RegexReservation,
    bytes: u64,
}

impl RegexHold {
    /// The bytes this hold keeps from the reservation.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Hold `bytes` instead of what this hold keeps now.
    ///
    /// Shrinking always succeeds. Growing takes the difference from the
    /// reservation as [`RegexReservation::try_hold`] does.
    ///
    /// # Errors
    ///
    /// [`RegexRefusal::Reservation`] when the growth does not fit. The hold
    /// then keeps what it kept before.
    pub fn resize(&mut self, bytes: u64) -> Result<(), RegexRefusal> {
        if bytes > self.bytes {
            self.reservation.grow(bytes - self.bytes)?;
        } else {
            self.reservation.shrink(self.bytes - bytes);
        }
        self.bytes = bytes;
        Ok(())
    }
}

impl Drop for RegexHold {
    fn drop(&mut self) {
        self.reservation.shrink(self.bytes);
        #[cfg(any(test, feature = "test-support"))]
        self.reservation.probe.count_release();
    }
}

#[cfg(any(test, feature = "test-support"))]
pub use probe::ChargePause;

/// What a test reads off a reservation and how it holds a build: hold and
/// release counters, a gate a metered build passes once its charge is
/// held, and a limit an integration test can lower before anything holds.
#[cfg(any(test, feature = "test-support"))]
mod probe {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex, PoisonError};

    use super::RegexReservation;

    #[derive(Debug)]
    pub(super) struct Probe {
        holds: AtomicU64,
        releases: AtomicU64,
        gate: Mutex<Option<Arc<Gate>>>,
    }

    impl Probe {
        pub(super) const fn new() -> Self {
            Self {
                holds: AtomicU64::new(0),
                releases: AtomicU64::new(0),
                gate: Mutex::new(None),
            }
        }

        pub(super) fn count_hold(&self) {
            self.holds.fetch_add(1, Ordering::SeqCst);
        }

        pub(super) fn count_release(&self) {
            self.releases.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// The gate one [`ChargePause`] installs.
    #[derive(Debug)]
    struct Gate {
        open: Mutex<bool>,
        opened: Condvar,
        arrivals: AtomicUsize,
    }

    impl Gate {
        /// Count the arrival and wait for the gate to open, however long
        /// that takes. The pause's drop opens it, so a failing test still
        /// lets the build go.
        fn pass(&self) {
            self.arrivals.fetch_add(1, Ordering::SeqCst);
            let mut open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
            while !*open {
                open = self
                    .opened
                    .wait(open)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        }

        fn release(&self) {
            *self.open.lock().unwrap_or_else(PoisonError::into_inner) = true;
            self.opened.notify_all();
        }
    }

    /// A test's hold on every metered build of one reservation, after the
    /// build's charge is held and before the ledger answers. Dropping it
    /// opens the gate and removes it.
    #[derive(Debug)]
    pub struct ChargePause {
        reservation: &'static RegexReservation,
        gate: Arc<Gate>,
    }

    impl ChargePause {
        /// How many builds have reached the gate.
        #[must_use]
        pub fn arrivals(&self) -> usize {
            self.gate.arrivals.load(Ordering::SeqCst)
        }

        /// Let every parked build through, and every later one straight
        /// past.
        pub fn release(&self) {
            self.gate.release();
        }
    }

    impl Drop for ChargePause {
        fn drop(&mut self) {
            self.gate.release();
            let mut installed = self
                .reservation
                .probe
                .gate
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if installed
                .as_ref()
                .is_some_and(|gate| Arc::ptr_eq(gate, &self.gate))
            {
                *installed = None;
            }
        }
    }

    impl RegexReservation {
        /// How many holds this reservation has granted.
        #[must_use]
        pub fn holds(&self) -> u64 {
            self.probe.holds.load(Ordering::SeqCst)
        }

        /// How many holds have been returned to this reservation.
        #[must_use]
        pub fn releases(&self) -> u64 {
            self.probe.releases.load(Ordering::SeqCst)
        }

        /// Park every metered build that holds from this reservation once
        /// its pattern's charge is held, until the pause is released or
        /// dropped.
        ///
        /// A build parks on the thread that runs it, so pause only a build
        /// that runs off the async reactor, such as the batch tail's.
        #[must_use]
        pub fn pause_after_charge(&'static self) -> ChargePause {
            let gate = Arc::new(Gate {
                open: Mutex::new(false),
                opened: Condvar::new(),
                arrivals: AtomicUsize::new(0),
            });
            let displaced = self
                .probe
                .gate
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .replace(Arc::clone(&gate));
            if let Some(displaced) = displaced {
                displaced.release();
            }
            ChargePause {
                reservation: self,
                gate,
            }
        }

        /// The metered build's side: free unless a test paused it.
        pub(crate) fn after_charge(&self) {
            let gate = self
                .probe
                .gate
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            if let Some(gate) = gate {
                gate.pass();
            }
        }
    }

    /// Set the process-wide reservation's limit to `bytes`, for an
    /// integration test that runs a server in a process of its own.
    ///
    /// A setter, not an environment variable read at first use: the test
    /// that sets the limit names it in its own code, before it starts
    /// work that holds, and a process that inherited the variable from
    /// its parent cannot set it by accident. The test may build a pattern
    /// first, to size the limit from its charge, and drop it.
    ///
    /// # Panics
    ///
    /// When a hold is live, because a limit changed under live holds would
    /// make what they hold mean something else.
    #[cfg(feature = "test-support")]
    pub fn set_reservation_limit(bytes: u64) {
        let reservation = super::reservation();
        assert_eq!(
            reservation.reserved(),
            0,
            "the regex reservation's limit is set while nothing holds from it"
        );
        reservation.limit.store(bytes, Ordering::Release);
    }
}

#[cfg(feature = "test-support")]
pub use probe::set_reservation_limit;

/// A reservation of its own for a test, with `limit` bytes, so its holds,
/// counters and gate are no other test's.
#[cfg(test)]
pub(crate) fn private_reservation(limit: u64) -> &'static RegexReservation {
    Box::leak(Box::new(RegexReservation::new(limit)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hold within the limit is granted, and one that does not fit is
    /// refused at once, leaving what is held as it was.
    #[test]
    fn a_hold_that_does_not_fit_is_refused_at_once() {
        let pool = private_reservation(100);
        let first = pool.try_hold(60).expect("fits");
        assert_eq!(pool.reserved(), 60);
        assert!(pool.try_hold(41).is_none(), "61 more do not fit");
        assert_eq!(pool.reserved(), 60);
        let second = pool.try_hold(40).expect("the rest fits exactly");
        assert_eq!(pool.reserved(), 100);
        assert!(pool.try_hold(1).is_none());
        drop((first, second));
        assert_eq!(pool.reserved(), 0);
        assert_eq!((pool.holds(), pool.releases()), (2, 2));
    }

    /// A sum past `u64::MAX` is refused, never wrapped, on a new hold and
    /// on growth.
    #[test]
    fn holds_never_overflow() {
        let pool = private_reservation(u64::MAX);
        let mut held = pool.try_hold(10).expect("fits");
        assert!(pool.try_hold(u64::MAX).is_none());
        assert_eq!(held.resize(u64::MAX), Ok(()), "exactly the limit fits");
        assert!(pool.try_hold(1).is_none());
        assert_eq!(pool.reserved(), u64::MAX);
        drop(held);
        assert_eq!(pool.reserved(), 0);
    }

    /// Shrinking a hold always succeeds and returns the difference.
    /// Growing takes the difference while it fits, and a growth that does
    /// not fit is refused with the hold unchanged.
    #[test]
    fn resize_shrinks_freely_and_grows_within_the_limit() {
        let pool = private_reservation(100);
        let mut held = pool.try_hold(40).expect("fits");
        assert_eq!(held.resize(10), Ok(()));
        assert_eq!((held.bytes(), pool.reserved()), (10, 10));
        assert_eq!(held.resize(90), Ok(()));
        assert_eq!((held.bytes(), pool.reserved()), (90, 90));
        let other = pool.try_hold(10).expect("the rest fits");
        assert_eq!(held.resize(91), Err(RegexRefusal::Reservation));
        assert_eq!((held.bytes(), pool.reserved()), (90, 100));
        drop(other);
        drop(held);
        assert_eq!(pool.reserved(), 0);
        assert_eq!((pool.holds(), pool.releases()), (2, 2));
    }

    /// Holds raced from many threads never take the reservation past its
    /// limit, and every byte comes back.
    #[test]
    fn racing_holds_stay_within_the_limit() {
        let pool = private_reservation(1000);
        let peak = AtomicU64::new(0);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for i in 0..2000_u64 {
                        if let Some(mut held) = pool.try_hold(7 + i % 50) {
                            peak.fetch_max(pool.reserved(), Ordering::SeqCst);
                            let _ = held.resize(i % 300);
                            peak.fetch_max(pool.reserved(), Ordering::SeqCst);
                        }
                    }
                });
            }
        });
        assert!(peak.load(Ordering::SeqCst) <= 1000);
        assert!(peak.load(Ordering::SeqCst) > 0);
        assert_eq!(pool.reserved(), 0);
        assert_eq!(pool.holds(), pool.releases());
    }

    /// A paused reservation parks a build at the charge point until the
    /// pause is released, and a dropped pause lets every later build
    /// straight past.
    #[test]
    fn a_pause_parks_the_build_until_released() {
        let pool = private_reservation(u64::MAX);
        let pause = pool.pause_after_charge();
        let parked = std::thread::spawn(move || pool.after_charge());
        while pause.arrivals() == 0 {
            std::thread::yield_now();
        }
        assert!(!parked.is_finished(), "the build waits at the gate");
        pause.release();
        parked.join().expect("the build goes on");
        drop(pause);
        pool.after_charge();
    }

    /// trawl-core's own tests share the process-wide reservation across
    /// threads, so it has no limit there.
    #[test]
    fn the_process_reservation_has_no_limit_under_unit_tests() {
        assert_eq!(reservation().limit(), u64::MAX);
    }
}
