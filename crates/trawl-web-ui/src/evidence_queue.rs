// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The investigation page's paced reads of the rows its findings cite
//! (ADR-0059).
//!
//! A finding cites up to 32 rows and a page shows every finding, so one
//! board can cite more rows than the reader's interactive rate limit
//! admits in a burst (100 a minute, one token back every 600 ms). The
//! page reads them through one [`EvidenceQueue`] instead of all at once:
//!
//! - at most [`MAX_IN_FLIGHT`] reads run at a time, the rest wait in the
//!   order they were asked for;
//! - a row already waiting or being read is not asked for again, so
//!   findings that cite the same row share one read;
//! - a 429 is not an answer about the row. It pauses the whole queue and
//!   puts the row back at the front, where it is read again once the
//!   pause ends. The pause is the `Retry-After` the server sent, in
//!   seconds or as a date ([`retry_after_ms`]), at least [`REFILL_MS`]
//!   and at most [`MAX_PAUSE_MS`], doubled for each throttled round in a
//!   row: a 429 once the last pause ended opens a round, one inside a
//!   running pause only extends it, and any answer ends the run;
//! - after [`MAX_AUTO_ROUNDS`] rounds in a row the queue stops instead of
//!   opening another: it reports [`Next::Stalled`] with how many rows
//!   wait, and nothing starts until the page calls
//!   [`EvidenceQueue::resume`];
//! - [`EvidenceQueue::cancel`] drops everything: the page is gone, so no
//!   waiting read starts and no answer still in flight lands.
//!
//! The queue only schedules. The page starts each read [`Next::Start`]
//! names, reports how it ended with [`EvidenceQueue::settled`] or
//! [`EvidenceQueue::throttled`], and arms one timer for each
//! [`Next::WakeAt`]. While the page's tab is hidden it asks the queue for
//! nothing, so every row waits, and asks again when the tab shows. Time
//! is milliseconds on any monotonic-enough clock the caller passes in,
//! so the tests drive it with a fake one.
//!
//! Pure and ungated so its tests run natively; the page that drives it
//! is wasm32-only.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use std::collections::{HashSet, VecDeque};
use std::hash::Hash;

/// Reads one board runs at a time.
pub const MAX_IN_FLIGHT: usize = 4;

/// The pause after a 429 that carried no usable `Retry-After`: the time
/// the interactive bucket takes to refill one token (60 s / 100).
pub const REFILL_MS: f64 = 600.0;

/// The longest one throttled round pauses, whatever the header asked.
pub const MAX_PAUSE_MS: f64 = 30_000.0;

/// Throttled rounds in a row the queue waits out by itself. With
/// trawld's `Retry-After: 1` that is 1 + 2 + 4 + 8 s, about 15 s of
/// automatic recovery before the page says the reads stopped.
pub const MAX_AUTO_ROUNDS: u32 = 4;

/// How one read ended, as the queue needs to know it. The board's own
/// reads of the investigation end the same way
/// ([`crate::investigation_live`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Paced<T> {
    /// The server answered the read, whatever it said.
    Answered(T),
    /// The server refused the read with 429; ask again after this many
    /// milliseconds.
    Throttled {
        /// From [`retry_after_ms`].
        retry_after_ms: f64,
    },
}

/// The pause a 429's `Retry-After` header asks for, in milliseconds.
///
/// Both forms RFC 9110 allows: whole seconds (what trawld sends), or an
/// HTTP date, which `parse_http_date` turns into unix milliseconds and
/// which pauses until then from `now_unix_ms`. A date already past is
/// one refill, [`REFILL_MS`], as is a missing header or anything
/// unreadable. The queue bounds what this returns ([`MAX_PAUSE_MS`]).
#[must_use]
pub fn retry_after_ms(
    header: Option<&str>,
    now_unix_ms: f64,
    parse_http_date: impl Fn(&str) -> Option<f64>,
) -> f64 {
    let Some(value) = header.map(str::trim) else {
        return REFILL_MS;
    };
    if let Ok(seconds) = value.parse::<u32>() {
        return f64::from(seconds) * 1000.0;
    }
    parse_http_date(value).map_or(REFILL_MS, |date| (date - now_unix_ms).max(REFILL_MS))
}

/// Whole milliseconds from `now` until `at` for a timer, at least zero
/// and at most `i32::MAX`: gloo-timers hands the delay to `setTimeout`
/// as a signed 32-bit number, so anything longer wraps negative and
/// fires at once. A wait longer than that fires early and the queue
/// arms again ([`Next::WakeAt`]); no pause is that long anyway.
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // clamped to i32's non-negative range first
pub fn wait_ms(now: f64, at: f64) -> u32 {
    (at - now).ceil().clamp(0.0, f64::from(i32::MAX)) as u32
}

/// The pause throttled round `round` (from 1) asks for when the server
/// asked for `server_pause`: at least one refill, so `Retry-After: 0`
/// still arms a timer, doubled each round, and never more than
/// [`MAX_PAUSE_MS`].
fn round_pause(server_pause: f64, round: u32) -> f64 {
    let doublings = round.saturating_sub(1).min(16);
    let base = server_pause.clamp(REFILL_MS, MAX_PAUSE_MS);
    (base * f64::from(1u32 << doublings)).min(MAX_PAUSE_MS)
}

/// What the page should do next.
#[derive(Debug, Clone, PartialEq)]
pub enum Next<K> {
    /// Start reading this row now.
    Start(K),
    /// The queue is paused with rows waiting: arm one timer for this
    /// instant and call [`EvidenceQueue::woke`] when it fires.
    WakeAt(f64),
    /// Nothing to start until a read ends, a row is asked for or an armed
    /// timer fires.
    Idle,
    /// [`MAX_AUTO_ROUNDS`] throttled rounds in a row: the queue stopped
    /// and arms no timer. Answered once for each count of rows waiting,
    /// then [`Next::Idle`] until [`EvidenceQueue::resume`].
    Stalled {
        /// The rows that will read once the queue resumes.
        waiting: usize,
    },
}

/// The rows one board still has to read.
#[derive(Debug)]
pub struct EvidenceQueue<K> {
    waiting: VecDeque<K>,
    in_flight: HashSet<K>,
    /// Until when a 429 paused the queue; nothing starts before it.
    paused_until: f64,
    /// The instant a timer is armed for, if one is.
    armed: Option<f64>,
    /// Throttled rounds in a row since the last answer or resume.
    rounds: u32,
    /// Stopped after [`MAX_AUTO_ROUNDS`] rounds, until [`Self::resume`].
    stalled: bool,
    /// The waiting count the last [`Next::Stalled`] reported.
    stall_reported: Option<usize>,
    cancelled: bool,
}

impl<K> Default for EvidenceQueue<K> {
    fn default() -> Self {
        Self {
            waiting: VecDeque::new(),
            in_flight: HashSet::new(),
            paused_until: 0.0,
            armed: None,
            rounds: 0,
            stalled: false,
            stall_reported: None,
            cancelled: false,
        }
    }
}

impl<K: Clone + Eq + Hash> EvidenceQueue<K> {
    /// An empty queue.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask for a row. `false` when it is already waiting or being read,
    /// or the queue was cancelled: nothing new will start for it.
    pub fn push(&mut self, key: K) -> bool {
        if self.cancelled || self.in_flight.contains(&key) || self.waiting.contains(&key) {
            return false;
        }
        self.waiting.push_back(key);
        true
    }

    /// The next thing to do at `now`. Call it until it answers something
    /// other than [`Next::Start`].
    pub fn next(&mut self, now: f64) -> Next<K> {
        if self.cancelled || self.waiting.is_empty() {
            return Next::Idle;
        }
        if self.stalled {
            let waiting = self.waiting.len();
            if self.stall_reported == Some(waiting) {
                return Next::Idle;
            }
            self.stall_reported = Some(waiting);
            return Next::Stalled { waiting };
        }
        if now < self.paused_until {
            return if self.armed.is_some() {
                Next::Idle
            } else {
                self.armed = Some(self.paused_until);
                Next::WakeAt(self.paused_until)
            };
        }
        if self.in_flight.len() >= MAX_IN_FLIGHT {
            return Next::Idle;
        }
        let Some(key) = self.waiting.pop_front() else {
            return Next::Idle;
        };
        self.in_flight.insert(key.clone());
        Next::Start(key)
    }

    /// A read the server answered: the run of throttled rounds ends.
    /// `false` when the queue no longer expects it (cancelled, or never
    /// started): the answer is dropped.
    pub fn settled(&mut self, key: &K) -> bool {
        if self.cancelled || !self.in_flight.remove(key) {
            return false;
        }
        self.rounds = 0;
        true
    }

    /// A read refused with 429 at `now`, whose `Retry-After` asked for
    /// `retry_after_ms`: put the row back at the front and pause the
    /// queue (a pause already running longer stands). A refusal once the
    /// last pause ended opens a round; the round past
    /// [`MAX_AUTO_ROUNDS`] stalls the queue instead of pausing it.
    /// `false` when the queue no longer expects it.
    pub fn throttled(&mut self, key: K, now: f64, retry_after_ms: f64) -> bool {
        if self.cancelled || !self.in_flight.remove(&key) {
            return false;
        }
        self.waiting.push_front(key);
        if self.stalled {
            return true;
        }
        if now >= self.paused_until {
            if self.rounds == MAX_AUTO_ROUNDS {
                self.stalled = true;
                return true;
            }
            self.rounds += 1;
        }
        self.paused_until = self
            .paused_until
            .max(now + round_pause(retry_after_ms, self.rounds));
        true
    }

    /// Start reading again after [`Next::Stalled`]: the run of throttled
    /// rounds starts over, and the next [`Self::next`] starts reads.
    pub fn resume(&mut self, now: f64) {
        self.stalled = false;
        self.stall_reported = None;
        self.rounds = 0;
        self.paused_until = now;
    }

    /// The armed timer fired. `false` when the queue was cancelled.
    pub fn woke(&mut self) -> bool {
        self.armed = None;
        !self.cancelled
    }

    /// The page is gone: nothing waiting starts, and nothing in flight is
    /// expected any more.
    pub fn cancel(&mut self) {
        self.cancelled = true;
        self.waiting.clear();
        self.in_flight.clear();
        self.armed = None;
    }
}

/// What the tests read of a queue's state.
#[cfg(test)]
impl<K: Clone + Eq + Hash> EvidenceQueue<K> {
    /// Whether [`cancel`](Self::cancel) ran.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    /// Reads running now.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    /// Rows waiting to start.
    #[must_use]
    pub fn waiting(&self) -> usize {
        self.waiting.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A fake fetcher and clock driving the queue the way the page does:
    /// start everything `next` allows, then end the oldest read, and
    /// when nothing runs and the queue waits on a timer, jump the clock
    /// to it.
    struct Harness {
        queue: EvidenceQueue<u32>,
        now: f64,
        running: VecDeque<u32>,
        /// Reads started per row.
        started: HashMap<u32, usize>,
        /// Rows answered, in order.
        answered: Vec<u32>,
        /// The next this-many reads answer 429.
        throttle: usize,
        retry_after: f64,
        timer: Option<f64>,
        max_in_flight: usize,
        /// When each read started.
        starts: Vec<(f64, u32)>,
        /// The waiting count the last [`Next::Stalled`] reported.
        stalled: Option<usize>,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                queue: EvidenceQueue::new(),
                now: 0.0,
                running: VecDeque::new(),
                started: HashMap::new(),
                answered: Vec::new(),
                throttle: 0,
                retry_after: 1000.0,
                timer: None,
                max_in_flight: 0,
                starts: Vec::new(),
                stalled: None,
            }
        }

        fn pump(&mut self) {
            loop {
                match self.queue.next(self.now) {
                    Next::Start(key) => {
                        *self.started.entry(key).or_default() += 1;
                        self.starts.push((self.now, key));
                        self.running.push_back(key);
                        self.max_in_flight = self.max_in_flight.max(self.queue.in_flight());
                    }
                    Next::WakeAt(at) => {
                        assert!(self.timer.is_none(), "one timer at a time");
                        self.timer = Some(at);
                        break;
                    }
                    Next::Idle => break,
                    Next::Stalled { waiting } => {
                        self.stalled = Some(waiting);
                        break;
                    }
                }
            }
        }

        fn want(&mut self, key: u32) {
            self.queue.push(key);
            self.pump();
        }

        /// End the oldest running read, or fire the timer when nothing
        /// runs. `false` once there is nothing left to do.
        fn step(&mut self) -> bool {
            self.now += 10.0;
            if let Some(key) = self.running.pop_front() {
                if self.throttle > 0 {
                    self.throttle -= 1;
                    assert!(self.queue.throttled(key, self.now, self.retry_after));
                } else {
                    assert!(self.queue.settled(&key));
                    self.answered.push(key);
                }
            } else if let Some(at) = self.timer.take() {
                self.now = self.now.max(at);
                assert!(self.queue.woke());
            } else {
                return false;
            }
            self.pump();
            true
        }

        fn run(&mut self) {
            let mut guard = 0;
            while self.step() {
                guard += 1;
                assert!(guard < 10_000, "the queue never drained");
            }
        }

        /// Step until the clock passes `until`, or nothing is left to do.
        fn run_until(&mut self, until: f64) {
            while self.now < until && self.step() {}
        }
    }

    #[test]
    fn at_most_four_reads_run_and_every_row_is_read_once() {
        let mut h = Harness::new();
        for key in 0..128 {
            h.want(key);
        }
        assert_eq!(h.queue.in_flight(), MAX_IN_FLIGHT);
        assert_eq!(h.queue.waiting(), 124);
        h.run();
        assert_eq!(h.max_in_flight, MAX_IN_FLIGHT);
        assert_eq!(
            h.answered,
            (0..128).collect::<Vec<_>>(),
            "first in, first out"
        );
        assert!(h.started.values().all(|&n| n == 1));
    }

    #[test]
    fn a_row_asked_for_twice_is_read_once() {
        let mut h = Harness::new();
        // Four findings citing the same three rows, as a board does.
        for _ in 0..4 {
            for key in 0..3 {
                h.want(key);
            }
        }
        // Asked again while being read: still one read.
        assert!(!h.queue.push(0));
        h.run();
        assert_eq!(h.answered, vec![0, 1, 2]);
        assert!(h.started.values().all(|&n| n == 1));
        // Once answered, the page asks again only on purpose (Retry or
        // Refresh after a failure), and then it is read again.
        assert!(h.queue.push(1));
    }

    #[test]
    fn a_429_pauses_the_whole_queue_and_reads_the_row_again_after_retry_after() {
        let mut h = Harness::new();
        h.throttle = 5;
        h.retry_after = 1000.0;
        for key in 0..20 {
            h.want(key);
        }
        // The first read ends with 429: nothing new may start until the
        // pause ends, though the three already running finish.
        assert!(h.step());
        let paused_at = h.now;
        assert_eq!(h.queue.in_flight(), 3);
        assert_eq!(h.queue.waiting(), 17);
        h.run();
        // Every row ends answered, none is lost or failed, and every row
        // past the first four started only after the first pause ended.
        let mut answered = h.answered.clone();
        answered.sort_unstable();
        assert_eq!(answered, (0..20).collect::<Vec<_>>());
        for (at, key) in &h.starts {
            if *key >= 4 {
                assert!(*at >= paused_at + 1000.0, "row {key} started at {at}");
            }
        }
        // The five refused reads were each asked again once.
        let again: usize = h.started.values().map(|n| n - 1).sum();
        assert_eq!(again, 5);
        assert_eq!(h.max_in_flight, MAX_IN_FLIGHT);
    }

    #[test]
    fn a_shorter_retry_after_does_not_cut_a_running_pause() {
        let mut q = EvidenceQueue::new();
        for key in 0..3 {
            q.push(key);
        }
        for key in 0..2 {
            assert_eq!(q.next(0.0), Next::Start(key));
        }
        assert!(q.throttled(0, 0.0, 5000.0));
        assert!(q.throttled(1, 10.0, 1000.0));
        assert_eq!(q.next(20.0), Next::WakeAt(5000.0));
        // One timer: asking again while it is armed starts nothing.
        assert_eq!(q.next(30.0), Next::Idle);
        assert!(q.woke());
        assert_eq!(
            q.next(5000.0),
            Next::Start(1),
            "refused rows go back to the front"
        );
        assert_eq!(q.next(5000.0), Next::Start(0));
        assert_eq!(q.next(5000.0), Next::Start(2));
    }

    #[test]
    fn a_timer_that_fires_early_arms_again() {
        let mut q = EvidenceQueue::new();
        q.push(1);
        q.push(2);
        assert_eq!(q.next(0.0), Next::Start(1));
        assert!(q.throttled(1, 0.0, 1000.0));
        assert_eq!(q.next(0.0), Next::WakeAt(1000.0));
        assert!(q.woke());
        assert_eq!(q.next(999.0), Next::WakeAt(1000.0));
    }

    /// The pause each of `rounds` consecutive throttled rounds asks for,
    /// one row read and refused per round, each 429 asking for
    /// `server_pause`.
    fn round_pauses(server_pause: f64, rounds: u32) -> Vec<f64> {
        let mut q = EvidenceQueue::new();
        q.push(0);
        let mut now = 0.0;
        let mut pauses = Vec::new();
        for _ in 0..rounds {
            assert_eq!(q.next(now), Next::Start(0));
            assert!(q.throttled(0, now, server_pause));
            let Next::WakeAt(at) = q.next(now) else {
                panic!("a throttled round arms a timer");
            };
            pauses.push(at - now);
            assert!(q.woke());
            now = at;
        }
        pauses
    }

    #[test]
    fn consecutive_throttled_rounds_double_the_pause() {
        // trawld's Retry-After: 1.
        assert_eq!(round_pauses(1000.0, 4), [1000.0, 2000.0, 4000.0, 8000.0]);
        // No usable header: one refill, doubled.
        assert_eq!(round_pauses(REFILL_MS, 4), [600.0, 1200.0, 2400.0, 4800.0]);
    }

    #[test]
    fn a_429_inside_a_running_pause_extends_it_and_opens_no_round() {
        let mut q = EvidenceQueue::new();
        q.push(0);
        q.push(1);
        assert_eq!(q.next(0.0), Next::Start(0));
        assert_eq!(q.next(0.0), Next::Start(1));
        // Round 1 pauses 1 s; the second 429 lands inside it and moves its
        // end to 500 + 1000, still at round 1's pause.
        assert!(q.throttled(0, 0.0, 1000.0));
        assert!(q.throttled(1, 500.0, 1000.0));
        assert_eq!(q.next(600.0), Next::WakeAt(1500.0));
        assert!(q.woke());
        assert_eq!(q.next(1500.0), Next::Start(1));
        // The next refusal opens round 2, not round 3.
        assert!(q.throttled(1, 1500.0, 1000.0));
        assert_eq!(q.next(1500.0), Next::WakeAt(3500.0));
    }

    #[test]
    fn a_settled_read_resets_the_rounds() {
        let mut q = EvidenceQueue::new();
        q.push(0);
        q.push(1);
        let mut now = 0.0;
        // Three rounds refusing row 0: 1 s, 2 s, 4 s.
        for pause in [1000.0, 2000.0, 4000.0] {
            assert_eq!(q.next(now), Next::Start(0));
            assert!(q.throttled(0, now, 1000.0));
            assert_eq!(q.next(now), Next::WakeAt(now + pause));
            assert!(q.woke());
            now += pause;
        }
        assert_eq!(q.next(now), Next::Start(0));
        assert_eq!(q.next(now), Next::Start(1));
        assert!(q.settled(&0));
        // The answer ended the run of refusals: the next 429 pauses for
        // the base again, not 8 s.
        assert!(q.throttled(1, now + 10.0, 1000.0));
        assert_eq!(q.next(now + 10.0), Next::WakeAt(now + 1010.0));
    }

    #[test]
    fn the_fifth_round_stalls_until_resume() {
        let mut q = EvidenceQueue::new();
        for key in 0..3 {
            q.push(key);
        }
        let mut now = 0.0;
        for _ in 0..MAX_AUTO_ROUNDS {
            for key in 0..3 {
                assert!(matches!(q.next(now), Next::Start(_)), "row {key}");
            }
            for key in 0..3 {
                assert!(q.throttled(key, now, 1000.0));
            }
            let Next::WakeAt(at) = q.next(now) else {
                panic!("rounds 1 to {MAX_AUTO_ROUNDS} arm a timer");
            };
            assert!(q.woke());
            now = at;
        }
        for _ in 0..3 {
            assert!(matches!(q.next(now), Next::Start(_)));
        }
        // Round five arms nothing: it says how many rows wait, once.
        assert!(q.throttled(0, now, 1000.0));
        assert_eq!(q.next(now), Next::Stalled { waiting: 1 });
        assert_eq!(q.next(now), Next::Idle);
        // Reads already in flight still end: an answer lands, a refusal
        // joins the rows waiting and the count is told again.
        assert!(q.settled(&1));
        assert!(q.throttled(2, now + 10.0, 1000.0));
        assert_eq!(q.next(now + 10.0), Next::Stalled { waiting: 2 });
        assert_eq!(q.next(now + 10.0), Next::Idle);
        // However long it waits, nothing starts by itself.
        assert_eq!(q.next(now + 3_600_000.0), Next::Idle);
        assert_eq!((q.in_flight(), q.waiting()), (0, 2));

        // Resume reads again, and the next refusal pauses for the base.
        let later = now + 3_600_000.0;
        q.resume(later);
        assert_eq!(q.next(later), Next::Start(2));
        assert_eq!(q.next(later), Next::Start(0));
        assert!(q.throttled(2, later, 1000.0));
        assert_eq!(q.next(later), Next::WakeAt(later + 1000.0));
    }

    #[test]
    fn retry_after_zero_still_pauses_one_refill() {
        let mut q = EvidenceQueue::new();
        q.push(0);
        q.push(1);
        assert_eq!(q.next(0.0), Next::Start(0));
        assert!(q.throttled(0, 0.0, 0.0));
        assert_eq!(q.next(0.0), Next::WakeAt(REFILL_MS), "never Start");
    }

    #[test]
    fn persistent_429s_on_five_rows_stop_after_four_rounds() {
        // Five rows, every read refused with trawld's Retry-After: 1, for
        // a minute.
        let mut h = Harness::new();
        h.throttle = usize::MAX;
        h.retry_after = 1000.0;
        for key in 0..5 {
            h.want(key);
        }
        h.run_until(60_000.0);
        let starts: usize = h.started.values().sum();
        assert!(
            starts <= 5 * (MAX_AUTO_ROUNDS as usize + 1),
            "{starts} starts"
        );
        // Four reads a round, rounds 1 to 4 and the round that stalls.
        assert_eq!(starts, 20);
        assert_eq!(h.stalled, Some(5));
        assert_eq!((h.queue.in_flight(), h.queue.waiting()), (0, 5));
        assert!(h.timer.is_none(), "a stalled queue arms nothing");
        // The last start was the stalling round's, about 15 s in; nothing
        // started after it.
        let last = h.starts.last().map(|(at, _)| *at).unwrap_or_default();
        assert!(last < 16_000.0, "last start at {last}");
    }

    #[test]
    fn cancel_starts_nothing_and_drops_every_answer_still_in_flight() {
        let mut q = EvidenceQueue::new();
        for key in 0..10 {
            q.push(key);
        }
        for key in 0..4 {
            assert_eq!(q.next(0.0), Next::Start(key));
        }
        q.cancel();
        assert!(q.is_cancelled());
        assert_eq!(q.next(0.0), Next::Idle);
        assert!(!q.settled(&0), "a late answer is not expected");
        assert!(!q.throttled(1, 0.0, 1000.0));
        assert!(!q.push(11));
        assert!(!q.woke());
        assert_eq!((q.in_flight(), q.waiting()), (0, 0));
    }

    /// An HTTP date one minute after the epoch.
    const MINUTE_PAST_EPOCH: &str = "Thu, 01 Jan 1970 00:01:00 GMT";

    /// A date parser that knows [`MINUTE_PAST_EPOCH`] and nothing else, as
    /// `Date.parse` answers it.
    fn epoch_dates(value: &str) -> Option<f64> {
        (value == MINUTE_PAST_EPOCH).then_some(60_000.0)
    }

    /// A parser that reads no date at all.
    fn no_dates(_: &str) -> Option<f64> {
        None
    }

    fn assert_ms(actual: f64, expected: f64, what: &str) {
        assert!((actual - expected).abs() < f64::EPSILON, "{what}: {actual}");
    }

    #[test]
    fn retry_after_reads_whole_seconds_and_falls_back_to_one_refill() {
        assert_ms(retry_after_ms(Some("1"), 0.0, no_dates), 1000.0, "1");
        assert_ms(retry_after_ms(Some(" 7 "), 0.0, no_dates), 7000.0, "7");
        for unreadable in [None, Some(""), Some("-1"), Some("1.5"), Some("soon")] {
            assert_ms(
                retry_after_ms(unreadable, 0.0, no_dates),
                REFILL_MS,
                &format!("{unreadable:?}"),
            );
        }
    }

    #[test]
    fn a_date_retry_after_pauses_until_that_date() {
        // At the epoch, a date one minute on is a minute's pause.
        assert_ms(
            retry_after_ms(Some(MINUTE_PAST_EPOCH), 0.0, epoch_dates),
            60_000.0,
            "a minute ahead",
        );
        // Ten seconds before it, ten seconds.
        assert_ms(
            retry_after_ms(Some(MINUTE_PAST_EPOCH), 50_000.0, epoch_dates),
            10_000.0,
            "ten seconds ahead",
        );
        // A date already past, or now, is one refill.
        for now in [60_000.0, 120_000.0] {
            assert_ms(
                retry_after_ms(Some(MINUTE_PAST_EPOCH), now, epoch_dates),
                REFILL_MS,
                "past",
            );
        }
        // A date the parser cannot read is one refill.
        assert_ms(
            retry_after_ms(Some(MINUTE_PAST_EPOCH), 0.0, no_dates),
            REFILL_MS,
            "unparsed",
        );
    }

    #[test]
    fn a_date_a_minute_ahead_pauses_the_queue_at_most_max_pause() {
        let mut q = EvidenceQueue::new();
        q.push(0);
        assert_eq!(q.next(0.0), Next::Start(0));
        let pause = retry_after_ms(Some(MINUTE_PAST_EPOCH), 0.0, epoch_dates);
        assert!(q.throttled(0, 0.0, pause));
        assert_eq!(q.next(0.0), Next::WakeAt(MAX_PAUSE_MS));
    }

    #[test]
    fn wait_ms_rounds_up_and_never_goes_negative() {
        assert_eq!(wait_ms(0.0, 1000.0), 1000);
        assert_eq!(wait_ms(0.0, 0.5), 1);
        assert_eq!(wait_ms(10.0, 0.0), 0);
    }

    #[test]
    fn wait_ms_stays_inside_a_signed_timer_delay() {
        let max = u32::try_from(i32::MAX).unwrap();
        assert_eq!(wait_ms(0.0, f64::MAX), max);
        assert_eq!(wait_ms(0.0, 1e12), max);
    }

    #[test]
    fn a_billion_second_retry_after_pauses_max_pause_on_a_bounded_timer() {
        // A billion seconds would be a timer gloo turns into
        // setTimeout(-1), which fires at once.
        let pause = retry_after_ms(Some("1000000000"), 0.0, no_dates);
        let mut q = EvidenceQueue::new();
        q.push(0);
        assert_eq!(q.next(0.0), Next::Start(0));
        assert!(q.throttled(0, 0.0, pause));
        let Next::WakeAt(at) = q.next(0.0) else {
            panic!("a throttled round arms a timer");
        };
        assert_ms(at, MAX_PAUSE_MS, "capped pause");
        assert!(i32::try_from(wait_ms(0.0, at)).is_ok());
        // And a timer chunk for any instant at all fits the signed range.
        assert!(i32::try_from(wait_ms(0.0, pause)).is_ok());
    }
}
