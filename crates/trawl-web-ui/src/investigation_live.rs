// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! How the investigation board keeps itself current (ADR-0059).
//!
//! While its tab is visible the board reads the entries changed since
//! its last read every [`POLL_MS`]. While the tab is hidden it reads
//! nothing; when the tab shows again it reads at once, then every
//! [`POLL_MS`] again. A read that fails is tried again after a delay that
//! doubles from [`POLL_MS`] up to [`BACKOFF_MAX_MS`], and a 429 waits at
//! least the `Retry-After` it carried. A read the server refuses for want
//! of a grant stops the board's reading for good.
//!
//! [`Liveness`] holds that schedule and what the page says about it: the
//! [`Connection`] state and when a read was last accepted. A read that
//! failed changes neither the board nor that instant. The page announces
//! to screen readers only a change of state that a person did not cause
//! by switching tabs ([`announcement`]) and how many entries a read
//! brought ([`new_entries_text`]), never each check.
//!
//! A change the reader did not ask for leaves what they read where it
//! was on screen: [`pick_anchor`] chooses the element that holds their
//! place, and the page scrolls by however far the change moved it.
//!
//! A person writes to the board too: a keep from the evidence viewer,
//! and a close or reopen from its header. [`keep_reason`] checks a
//! keep's reason against the bounds the server enforces before anything
//! is sent, and [`write_refusal`] reads what a refused write's answer
//! says ([`WriteFailure`]): an overwrite whose sequence moved, an
//! investigation closed meanwhile, or the server's own sentence. A write
//! whose answer never arrived is neither a success nor a refusal: its
//! outcome is unknown, and the page never sends it again by itself.
//!
//! Time is milliseconds on any clock the caller passes in, so the tests
//! drive it with a fake one. Pure and ungated so its tests run natively;
//! the page that drives it is wasm32-only.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use fleet_ui::Tone;
use trawl_api::investigations::KEEP_REASON_MAX_BYTES;
use trawl_api::{ErrorCode, ErrorResponse};

use crate::evidence_queue::wait_ms;

/// Between two reads of a visible board that is current.
pub const POLL_MS: u32 = 5_000;

/// The longest the board waits after reads failed in a row.
pub const BACKOFF_MAX_MS: u32 = 60_000;

/// What the page says about how current the board is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connection {
    /// No read has been accepted since the board opened or its tab
    /// showed again.
    Checking,
    /// The last read was accepted; the next one is [`POLL_MS`] away.
    Live,
    /// The tab is hidden, so the board reads nothing.
    Paused,
    /// The last read failed; the board tries again after a delay.
    Retrying,
    /// The server refused a read for want of a grant: the board reads no
    /// more on this visit.
    Stopped,
}

impl Connection {
    /// The state in words.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Checking => "Checking",
            Self::Live => "Live",
            Self::Paused => "Paused",
            Self::Retrying => "Retrying",
            Self::Stopped => "Stopped",
        }
    }

    /// The badge tone of the state.
    #[must_use]
    pub fn tone(self) -> Tone {
        match self {
            Self::Checking => Tone::Info,
            Self::Live => Tone::Success,
            Self::Paused => Tone::Neutral,
            Self::Retrying => Tone::Warn,
            Self::Stopped => Tone::Danger,
        }
    }
}

/// How a read of the board ended.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ReadEnd {
    /// The server answered the entries; the board merged them.
    Accepted,
    /// The read failed: the network, a decode or a server error.
    Failed,
    /// The server answered 429 and asked for this pause.
    Throttled {
        /// From [`crate::evidence_queue::retry_after_ms`].
        retry_after_ms: f64,
    },
    /// The server refused the read for want of a grant, or no longer
    /// holds the investigation.
    Refused,
}

/// The board's reading schedule and what the page says about it.
#[derive(Debug, Clone, PartialEq)]
pub struct Liveness {
    connection: Connection,
    /// Reads that failed in a row.
    failures: u32,
    /// The instant a 429 asked the board not to read before.
    hold_until: Option<f64>,
    /// When the last read was accepted.
    last_ok: Option<f64>,
}

impl Liveness {
    /// The schedule of a board that has not read yet, in a tab that is
    /// `visible` or not.
    #[must_use]
    pub fn new(visible: bool) -> Self {
        Self {
            connection: if visible {
                Connection::Checking
            } else {
                Connection::Paused
            },
            failures: 0,
            hold_until: None,
            last_ok: None,
        }
    }

    /// What the page says about how current the board is.
    #[must_use]
    pub fn connection(&self) -> Connection {
        self.connection
    }

    /// When the last read was accepted, on the caller's clock. A read
    /// that answered nothing new counts; a failed one does not.
    #[must_use]
    pub fn last_ok(&self) -> Option<f64> {
        self.last_ok
    }

    /// Whether the board still reads at all.
    #[must_use]
    pub fn stopped(&self) -> bool {
        self.connection == Connection::Stopped
    }

    /// A read ended at `now`.
    pub fn ended(&mut self, end: ReadEnd, now: f64) {
        if self.stopped() {
            return;
        }
        match end {
            ReadEnd::Accepted => {
                self.failures = 0;
                self.hold_until = None;
                self.last_ok = Some(now);
                self.connection = Connection::Live;
            }
            ReadEnd::Failed => {
                self.failures = self.failures.saturating_add(1);
                self.connection = Connection::Retrying;
            }
            ReadEnd::Throttled { retry_after_ms } => {
                self.failures = self.failures.saturating_add(1);
                self.hold_until = Some(now + retry_after_ms.max(0.0));
                self.connection = Connection::Retrying;
            }
            ReadEnd::Refused => self.connection = Connection::Stopped,
        }
    }

    /// The tab is hidden: the board reads nothing until it shows.
    pub fn hidden(&mut self) {
        if !self.stopped() {
            self.connection = Connection::Paused;
        }
    }

    /// The tab shows at `now`: how long until the board reads, `Some(0)`
    /// for at once, `None` when it no longer reads. A read waits only for
    /// a 429's pause that is still running.
    pub fn shown(&mut self, now: f64) -> Option<u32> {
        if self.stopped() {
            return None;
        }
        self.connection = if self.failures > 0 {
            Connection::Retrying
        } else {
            Connection::Checking
        };
        Some(self.hold_until.map_or(0, |until| wait_ms(now, until)))
    }

    /// How long after a read ended at `now` the next one is due, in a
    /// visible tab. `None` when the board no longer reads.
    #[must_use]
    pub fn next_delay(&self, now: f64) -> Option<u32> {
        if self.stopped() {
            return None;
        }
        let backoff = match self.failures {
            0 => POLL_MS,
            n => POLL_MS
                .saturating_mul(1u32 << (n - 1).min(16))
                .min(BACKOFF_MAX_MS),
        };
        Some(
            self.hold_until
                .map_or(backoff, |until| backoff.max(wait_ms(now, until))),
        )
    }
}

/// What a polite live region says when the state changes from `before`
/// to `after`, if anything. Pausing in a hidden tab and checking when it
/// shows again are the person's own doing and pass unannounced; a read
/// that failed, a board that stopped, and a recovery are not.
#[must_use]
pub fn announcement(before: Connection, after: Connection) -> Option<&'static str> {
    match (before, after) {
        (before, after) if before == after => None,
        (_, Connection::Retrying) => Some("Checking for new entries failed. Retrying."),
        (_, Connection::Stopped) => Some("The board stopped checking for new entries."),
        (Connection::Retrying, Connection::Live) => Some("The board is live again."),
        _ => None,
    }
}

/// How many entries a read brought, in words; `None` for none.
#[must_use]
pub fn new_entries_text(added: usize) -> Option<String> {
    match added {
        0 => None,
        1 => Some("1 new entry.".to_owned()),
        n => Some(format!("{n} new entries.")),
    }
}

/// One keyed element of the board as the page measured it: its top and
/// bottom edges in the viewport, and whether the keyed element after it
/// in document order lies inside it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placed {
    pub top: f64,
    pub bottom: f64,
    pub holds_next: bool,
}

/// Which of `placed`, in document order, holds the reader's place while
/// the board changes out of their hands: the first that still shows
/// below `line`, the scroller's top edge. A section that starts above the
/// line and holds entries is passed over for what follows it, since an
/// entry added inside it above the line would move what the reader sees
/// but not the section's top. An element with no height is hidden and
/// holds nothing.
#[must_use]
pub fn pick_anchor(placed: &[Placed], line: f64) -> Option<usize> {
    placed
        .iter()
        .position(|p| p.bottom > p.top && p.bottom > line && !(p.holds_next && p.top < line))
}

/// Why a keep's reason cannot be sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasonProblem {
    /// Nothing but whitespace.
    Empty,
    /// Longer than [`KEEP_REASON_MAX_BYTES`] bytes of UTF-8 once trimmed.
    TooLong {
        /// The trimmed reason's length in bytes.
        bytes: usize,
    },
    /// It holds a NUL, which the server cannot store.
    Nul,
}

impl ReasonProblem {
    /// What the form says under the field.
    #[must_use]
    pub fn message(self) -> String {
        match self {
            Self::Empty => "Say why you keep this row.".to_owned(),
            Self::TooLong { bytes } => {
                format!("The reason is {bytes} bytes; it can be at most {KEEP_REASON_MAX_BYTES}.")
            }
            Self::Nul => "The reason cannot hold a NUL character.".to_owned(),
        }
    }
}

/// The reason a keep sends: `text` trimmed, when the server would take
/// it. The bound is the server's, counted in bytes of UTF-8 as the server
/// counts it, never in characters.
///
/// # Errors
/// The [`ReasonProblem`] the server would refuse it for.
pub fn keep_reason(text: &str) -> Result<&str, ReasonProblem> {
    let reason = text.trim();
    if reason.is_empty() {
        Err(ReasonProblem::Empty)
    } else if reason.contains('\0') {
        Err(ReasonProblem::Nul)
    } else if reason.len() > KEEP_REASON_MAX_BYTES {
        Err(ReasonProblem::TooLong {
            bytes: reason.len(),
        })
    } else {
        Ok(reason)
    }
}

/// Why a write from the board did not land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteFailure {
    /// 409 `sequence_conflict`: the investigation changed after the board
    /// read the sequence the overwrite expected. Nothing was written.
    Stale {
        /// The sequence the write expected, and the one it found, when
        /// the server said.
        conflict: Option<(u64, u64)>,
    },
    /// 409 `investigation_closed`: it was closed when the write reached
    /// it. Nothing was written.
    Closed,
    /// The session is gone (401).
    Unauthorized,
    /// The server refused it, in its own words when it gave any.
    Refused(String),
    /// No answer was read: the write may or may not have landed.
    Unknown,
}

/// What a write's non-2xx answer, `status` with `body`, says. A gateway
/// failure, or a server error without trawld's envelope, means the answer
/// was lost on the way, not that the write was refused: its outcome is
/// unknown.
#[must_use]
pub fn write_refusal(status: u16, body: &str) -> WriteFailure {
    if status == 401 {
        return WriteFailure::Unauthorized;
    }
    if matches!(status, 502 | 504) {
        return WriteFailure::Unknown;
    }
    match serde_json::from_str::<ErrorResponse>(body) {
        Ok(ErrorResponse { error }) => match error.code {
            ErrorCode::SequenceConflict => WriteFailure::Stale {
                conflict: error.conflict.map(|c| (c.expected, c.current)),
            },
            ErrorCode::InvestigationClosed => WriteFailure::Closed,
            _ if status == 403 => WriteFailure::Refused(
                "You do not have permission to write to investigations.".to_owned(),
            ),
            _ => WriteFailure::Refused(error.message),
        },
        Err(_) if status >= 500 => WriteFailure::Unknown,
        Err(_) => WriteFailure::Refused(format!("The server answered {status}.")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_board(now: f64) -> Liveness {
        let mut live = Liveness::new(true);
        live.ended(ReadEnd::Accepted, now);
        live
    }

    #[test]
    fn a_visible_board_checks_until_its_first_read_lands_then_polls_every_five_seconds() {
        let mut live = Liveness::new(true);
        assert_eq!(live.connection(), Connection::Checking);
        assert_eq!(live.last_ok(), None);
        live.ended(ReadEnd::Accepted, 1_000.0);
        assert_eq!(live.connection(), Connection::Live);
        assert_eq!(live.last_ok(), Some(1_000.0));
        assert_eq!(live.next_delay(1_000.0), Some(POLL_MS));
        // An empty read is still a check.
        live.ended(ReadEnd::Accepted, 6_000.0);
        assert_eq!(live.last_ok(), Some(6_000.0));
    }

    #[test]
    fn a_hidden_tab_pauses_and_showing_it_reads_at_once() {
        let mut live = Liveness::new(false);
        assert_eq!(live.connection(), Connection::Paused);
        assert_eq!(live.shown(0.0), Some(0));
        assert_eq!(live.connection(), Connection::Checking);

        let mut live = live_board(0.0);
        live.hidden();
        assert_eq!(live.connection(), Connection::Paused);
        assert_eq!(live.last_ok(), Some(0.0));
        assert_eq!(live.shown(60_000.0), Some(0));
        assert_eq!(live.connection(), Connection::Checking);
    }

    #[test]
    fn failed_reads_back_off_from_five_to_sixty_seconds_and_keep_the_last_check() {
        let mut live = live_board(0.0);
        let mut delays = Vec::new();
        for _ in 0..6 {
            live.ended(ReadEnd::Failed, 10.0);
            assert_eq!(live.connection(), Connection::Retrying);
            delays.push(live.next_delay(10.0).unwrap());
        }
        assert_eq!(delays, vec![5_000, 10_000, 20_000, 40_000, 60_000, 60_000]);
        assert_eq!(live.last_ok(), Some(0.0));
        // Many more failures never overflow the doubling.
        for _ in 0..100 {
            live.ended(ReadEnd::Failed, 10.0);
        }
        assert_eq!(live.next_delay(10.0), Some(BACKOFF_MAX_MS));

        live.ended(ReadEnd::Accepted, 500.0);
        assert_eq!(live.connection(), Connection::Live);
        assert_eq!(live.next_delay(500.0), Some(POLL_MS));
        assert_eq!(live.last_ok(), Some(500.0));
    }

    #[test]
    fn a_429_waits_at_least_its_retry_after() {
        let mut live = live_board(0.0);
        live.ended(
            ReadEnd::Throttled {
                retry_after_ms: 30_000.0,
            },
            1_000.0,
        );
        assert_eq!(live.connection(), Connection::Retrying);
        assert_eq!(live.next_delay(1_000.0), Some(30_000));
        // A short Retry-After still waits the backoff.
        let mut short = live_board(0.0);
        short.ended(
            ReadEnd::Throttled {
                retry_after_ms: 1_000.0,
            },
            0.0,
        );
        assert_eq!(short.next_delay(0.0), Some(POLL_MS));
        // Showing the tab inside the pause waits out what is left of it,
        // and the state still says the last read failed.
        live.hidden();
        assert_eq!(live.shown(11_000.0), Some(20_000));
        assert_eq!(live.connection(), Connection::Retrying);
        // Past the pause it reads at once.
        assert_eq!(live.shown(31_000.0), Some(0));
        // An accepted read clears the pause.
        live.ended(ReadEnd::Accepted, 31_500.0);
        assert_eq!(live.next_delay(31_500.0), Some(POLL_MS));
    }

    #[test]
    fn a_refused_read_stops_the_board_for_good() {
        let mut live = live_board(0.0);
        live.ended(ReadEnd::Refused, 5_000.0);
        assert_eq!(live.connection(), Connection::Stopped);
        assert!(live.stopped());
        assert_eq!(live.next_delay(5_000.0), None);
        live.hidden();
        assert_eq!(live.shown(6_000.0), None);
        live.ended(ReadEnd::Accepted, 7_000.0);
        assert_eq!(live.connection(), Connection::Stopped);
        assert_eq!(live.last_ok(), Some(0.0));
    }

    #[test]
    fn only_failures_stops_and_recoveries_are_announced() {
        use Connection::{Checking, Live, Paused, Retrying, Stopped};
        assert_eq!(announcement(Checking, Live), None);
        assert_eq!(announcement(Live, Paused), None);
        assert_eq!(announcement(Paused, Checking), None);
        assert_eq!(announcement(Live, Live), None);
        assert_eq!(
            announcement(Live, Retrying),
            Some("Checking for new entries failed. Retrying.")
        );
        assert_eq!(announcement(Retrying, Retrying), None);
        assert_eq!(
            announcement(Retrying, Live),
            Some("The board is live again.")
        );
        assert_eq!(
            announcement(Live, Stopped),
            Some("The board stopped checking for new entries.")
        );
        assert_eq!(new_entries_text(0), None);
        assert_eq!(new_entries_text(1).as_deref(), Some("1 new entry."));
        assert_eq!(new_entries_text(3).as_deref(), Some("3 new entries."));
    }

    fn section(top: f64, bottom: f64) -> Placed {
        Placed {
            top,
            bottom,
            holds_next: true,
        }
    }

    fn entry(top: f64, bottom: f64) -> Placed {
        Placed {
            top,
            bottom,
            holds_next: false,
        }
    }

    #[test]
    fn the_entry_under_the_line_holds_the_place_not_its_section() {
        // Activity runs 100..400 with entries of 100; the line is inside
        // the second entry.
        let placed = [
            entry(0.0, 90.0),
            section(100.0, 400.0),
            entry(100.0, 200.0),
            entry(200.0, 300.0),
            entry(300.0, 400.0),
            section(400.0, 600.0),
            entry(420.0, 600.0),
        ];
        assert_eq!(pick_anchor(&placed, 250.0), Some(3));
        // On an entry's top edge, that entry.
        assert_eq!(pick_anchor(&placed, 200.0), Some(3));
    }

    #[test]
    fn a_section_whose_entries_are_all_above_the_line_passes_to_the_next() {
        // The line is in the activity's Load more, below its last entry.
        let placed = [
            section(0.0, 260.0),
            entry(0.0, 100.0),
            entry(100.0, 200.0),
            section(260.0, 500.0),
            entry(300.0, 500.0),
        ];
        assert_eq!(pick_anchor(&placed, 230.0), Some(3));
    }

    #[test]
    fn a_section_below_the_line_or_without_entries_holds_the_place_itself() {
        let placed = [section(300.0, 500.0), entry(330.0, 500.0)];
        assert_eq!(pick_anchor(&placed, 250.0), Some(0));
        // "No steps yet.": a section with nothing keyed inside it.
        let placed = [entry(0.0, 100.0), entry(100.0, 400.0)];
        assert_eq!(pick_anchor(&placed, 150.0), Some(1));
    }

    #[test]
    fn hidden_elements_and_everything_above_the_line_hold_nothing() {
        let placed = [entry(0.0, 100.0), entry(0.0, 0.0), entry(150.0, 150.0)];
        assert_eq!(pick_anchor(&placed, 120.0), None);
        assert_eq!(pick_anchor(&[], 0.0), None);
    }

    #[test]
    fn every_state_has_words_and_a_tone() {
        for (state, label) in [
            (Connection::Checking, "Checking"),
            (Connection::Live, "Live"),
            (Connection::Paused, "Paused"),
            (Connection::Retrying, "Retrying"),
            (Connection::Stopped, "Stopped"),
        ] {
            assert_eq!(state.label(), label);
        }
        assert_eq!(Connection::Live.tone(), Tone::Success);
        assert_eq!(Connection::Retrying.tone(), Tone::Warn);
    }

    #[test]
    fn a_reason_is_trimmed_and_must_say_something() {
        assert_eq!(keep_reason("  the first 502 \n"), Ok("the first 502"));
        for blank in ["", " ", "\t\n ", "\u{3000}"] {
            assert_eq!(keep_reason(blank), Err(ReasonProblem::Empty), "{blank:?}");
        }
        assert_eq!(keep_reason("a\0b"), Err(ReasonProblem::Nul));
    }

    #[test]
    fn a_reason_is_bounded_in_bytes_not_characters() {
        let ascii = "a".repeat(KEEP_REASON_MAX_BYTES);
        assert_eq!(keep_reason(&ascii), Ok(ascii.as_str()));
        assert_eq!(
            keep_reason(&format!("{ascii}b")),
            Err(ReasonProblem::TooLong { bytes: 1025 })
        );
        // Whitespace around it is not counted: it is not sent.
        assert_eq!(keep_reason(&format!("  {ascii}  ")), Ok(ascii.as_str()));
        // Four bytes each: 256 fit exactly, 257 do not, although 257
        // characters is far below 1024 characters.
        let wide = "\u{1F4A5}".repeat(256);
        assert_eq!(wide.len(), KEEP_REASON_MAX_BYTES);
        assert_eq!(keep_reason(&wide), Ok(wide.as_str()));
        let over = "\u{1F4A5}".repeat(257);
        assert_eq!(
            keep_reason(&over),
            Err(ReasonProblem::TooLong { bytes: 1028 })
        );
        assert_eq!(
            ReasonProblem::TooLong { bytes: 1028 }.message(),
            "The reason is 1028 bytes; it can be at most 1024."
        );
    }

    #[test]
    fn a_refused_write_reads_its_conflict_from_the_answer() {
        // The stub's 409 bodies, which the wire contract test pins to
        // trawld's.
        let stale = include_str!("../e2e/harness/wire/investigation-sequence-conflict.json");
        assert_eq!(
            write_refusal(409, stale),
            WriteFailure::Stale {
                conflict: Some((9, 10))
            }
        );
        let closed = include_str!("../e2e/harness/wire/investigation-closed.json");
        assert_eq!(write_refusal(409, closed), WriteFailure::Closed);
        let budget = r#"{"error":{"code":"evidence_budget_exceeded","message":"over budget"}}"#;
        assert_eq!(
            write_refusal(409, budget),
            WriteFailure::Refused("over budget".to_owned())
        );
        let forbidden = r#"{"error":{"code":"forbidden","message":"insufficient permissions"}}"#;
        assert_eq!(
            write_refusal(403, forbidden),
            WriteFailure::Refused(
                "You do not have permission to write to investigations.".to_owned()
            )
        );
        assert_eq!(write_refusal(401, ""), WriteFailure::Unauthorized);
        // A lost answer is not a refusal.
        assert_eq!(write_refusal(502, "<html>"), WriteFailure::Unknown);
        assert_eq!(write_refusal(504, closed), WriteFailure::Unknown);
        assert_eq!(write_refusal(500, ""), WriteFailure::Unknown);
        assert_eq!(
            write_refusal(400, ""),
            WriteFailure::Refused("The server answered 400.".to_owned())
        );
    }
}
