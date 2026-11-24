// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The server-wide regex reservation (ADR-0063).
//!
//! One process runs one server, so the reservation is a process-wide
//! static that every metered [`super::Preparer`] reaches with no handle
//! passed to it: SSE setup in trawl-server and the batch tail in
//! trawl-engine draw on the same bytes.

use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU64, Ordering};

use super::RegexRefusal;

const KIB: u64 = 1 << 10;
const MIB: u64 = 1 << 20;

/// The most regex memory every query in the process may hold at once, in
/// bytes: 1.25 GiB, against the charges [`super::charge`] computes and the
/// RE2 allowances of batch statements ([`RegexSites::re2_allowance`]).
pub const REGEX_RESERVATION_BYTES: u64 = 1280 * MIB;

/// The most one query may hold of the reservation at once, in bytes: its
/// patterns' charges and its batch statement's RE2 allowance together. A
/// query that needs more can never run, and is refused with
/// [`RegexRefusal::Budget`]. One query cannot hold the whole reservation,
/// so others still fit beside it.
pub const QUERY_RESERVATION_CAP: u64 = 512 * MIB;

/// What one regex site other than an extract's may take in `DuckDB`'s
/// RE2, on one thread, in bytes: 6.88 MiB, rounded up to a whole KiB
/// (`docs/evidence/issue-335/re2-memory.txt`, parts `SITE_SIZE` and
/// `ALLOWANCE`, "flat by kind").
pub const MATCH_SITE_RE2_BYTES: u64 = 7046 * KIB;

/// What one `regexp_extract` site may take in `DuckDB`'s RE2, on one
/// thread, in bytes: 13.38 MiB, rounded up to a whole KiB
/// (`docs/evidence/issue-335/re2-memory.txt`, parts `SITE_SIZE` and
/// `ALLOWANCE`, "flat by kind").
pub const EXTRACT_SITE_RE2_BYTES: u64 = 13702 * KIB;

/// What one regex call whose pattern the emitter writes may take in
/// `DuckDB`'s RE2, on one thread, in bytes: 9 KiB. Those patterns are
/// `crate::conform`'s four (`FIXED_REGEXES`)
/// (`docs/evidence/issue-335/re2-memory.txt`, part `FIXED_SITES`).
pub const FIXED_SITE_RE2_BYTES: u64 = 9 * KIB;

/// The regex calls one emitted statement makes `DuckDB` compile, by the
/// kind of RE2 program each one is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RegexSites {
    /// `regexp_extract` calls: one per `extract` stage, and the copies
    /// `DuckDB` makes of it.
    pub extract: u64,
    /// Every other call that is none of the emitter's measured regexes:
    /// `matches` operands, search `/regex/` terms, and their copies.
    pub matching: u64,
    /// Calls of one of the emitter's own regexes through its own function
    /// ([`crate::conform::FIXED_REGEXES`]): `sev()`'s and the hot
    /// branch's severity readings, and their copies.
    pub fixed: u64,
}

impl RegexSites {
    /// The RE2 memory a statement with these sites may take on `threads`
    /// database threads, in bytes: each site at its kind's figure
    /// ([`MATCH_SITE_RE2_BYTES`], [`EXTRACT_SITE_RE2_BYTES`],
    /// [`FIXED_SITE_RE2_BYTES`]), once on every thread. Saturates, so a
    /// sum past `u64::MAX` reads as the most there is, which no
    /// reservation holds.
    #[must_use]
    pub fn re2_allowance(self, threads: NonZeroU32) -> u64 {
        self.matching
            .saturating_mul(MATCH_SITE_RE2_BYTES)
            .saturating_add(self.extract.saturating_mul(EXTRACT_SITE_RE2_BYTES))
            .saturating_add(self.fixed.saturating_mul(FIXED_SITE_RE2_BYTES))
            .saturating_mul(u64::from(threads.get()))
    }
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

#[must_use]
pub fn reservation() -> &'static RegexReservation {
    &RESERVATION
}

/// A pool of regex memory, in bytes, that holds are taken from.
#[derive(Debug)]
pub struct RegexReservation {
    limit: AtomicU64,
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

    #[must_use]
    pub fn reserved(&self) -> u64 {
        self.reserved.load(Ordering::Acquire)
    }

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
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Hold `bytes` instead of what this hold keeps now.
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
    /// build's charge is held and before the ledger answers, and on every
    /// batch statement, after its RE2 allowance is held and before it
    /// binds. Dropping it opens the gate and removes it.
    #[derive(Debug)]
    pub struct ChargePause {
        reservation: &'static RegexReservation,
        gate: Arc<Gate>,
    }

    impl ChargePause {
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
        #[must_use]
        pub fn holds(&self) -> u64 {
            self.probe.holds.load(Ordering::SeqCst)
        }

        #[must_use]
        pub fn releases(&self) -> u64 {
            self.probe.releases.load(Ordering::SeqCst)
        }

        /// Park every metered build that holds from this reservation once
        /// its pattern's charge is held, and every batch statement once
        /// its RE2 allowance is held, before it binds, until the pause is
        /// released or dropped.
        ///
        /// A build or a statement parks on the thread that runs it, so
        /// pause only work that runs off the async reactor, such as the
        /// batch lane's.
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

    #[test]
    fn the_allowance_is_each_site_once_a_thread() {
        let four = NonZeroU32::new(4).expect("not zero");
        let five = RegexSites {
            extract: 0,
            matching: 5,
            fixed: 0,
        };
        assert_eq!(five.re2_allowance(four), 140_920 * KIB, "about 137.6 MiB");
        let parser = RegexSites {
            extract: 1,
            matching: 0,
            fixed: 0,
        };
        assert_eq!(parser.re2_allowance(four), 54_808 * KIB, "about 53.5 MiB");
        let both = RegexSites {
            extract: 1,
            matching: 5,
            fixed: 0,
        };
        assert_eq!(
            both.re2_allowance(four),
            five.re2_allowance(four) + parser.re2_allowance(four)
        );
        // `* | let s = sev(message)`: the 41 calls `DuckDB` binds.
        let sev = RegexSites {
            fixed: 41,
            ..RegexSites::default()
        };
        assert_eq!(sev.re2_allowance(four), 1476 * KIB, "about 1.4 MiB");
        let all = RegexSites { fixed: 41, ..both };
        assert_eq!(
            all.re2_allowance(four),
            both.re2_allowance(four) + sev.re2_allowance(four)
        );
        assert_eq!(RegexSites::default().re2_allowance(four), 0);
        let past = RegexSites {
            extract: u64::MAX,
            matching: 1,
            fixed: 0,
        };
        assert_eq!(past.re2_allowance(four), u64::MAX);
        let past = RegexSites {
            fixed: u64::MAX,
            ..RegexSites::default()
        };
        assert_eq!(past.re2_allowance(four), u64::MAX);
    }

    #[test]
    fn site_figures_round_the_model_up_to_a_kib() {
        let kib_over =
            |hundredths_mib: u64| (hundredths_mib * MIB).div_ceil(100).div_ceil(KIB) * KIB;
        assert_eq!(MATCH_SITE_RE2_BYTES, kib_over(688));
        assert_eq!(EXTRACT_SITE_RE2_BYTES, kib_over(1338));
    }

    #[test]
    fn the_fixed_site_figure_is_the_measured_one_with_its_margin() {
        let measured_tenths_kib: u64 = 66;
        let with_margin = (measured_tenths_kib * KIB * 125).div_ceil(10 * 100);
        assert_eq!(FIXED_SITE_RE2_BYTES, with_margin.div_ceil(KIB) * KIB);
        assert_eq!(FIXED_SITE_RE2_BYTES, 9 * KIB);
    }

    #[test]
    fn the_process_reservation_has_no_limit_under_unit_tests() {
        assert_eq!(reservation().limit(), u64::MAX);
    }
}
