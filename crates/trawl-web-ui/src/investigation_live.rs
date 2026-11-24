// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! How the investigation board keeps itself current (ADR-0059).
//! Time is milliseconds on any clock the caller passes in.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use fleet_ui::Tone;
use trawl_api::investigations::KEEP_REASON_MAX_BYTES;
use trawl_api::{ErrorCode, ErrorResponse};

use crate::evidence_queue::wait_ms;

pub const POLL_MS: u32 = 5_000;

pub const BACKOFF_MAX_MS: u32 = 60_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connection {
    Checking,
    Live,
    Paused,
    Retrying,
    Stopped,
}

impl Connection {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reread {
    Done,
    Reading,
    Waiting,
    Stopped,
}

impl Reread {
    /// The read after a conflict whose mark is `mark`, when the board's
    /// newest accepted read is `accepted` and its state `connection`.
    #[must_use]
    pub fn of(mark: u64, accepted: u64, connection: Connection) -> Self {
        if accepted > mark {
            return Self::Done;
        }
        match connection {
            Connection::Paused => Self::Waiting,
            Connection::Stopped => Self::Stopped,
            Connection::Checking | Connection::Live | Connection::Retrying => Self::Reading,
        }
    }

    #[must_use]
    pub fn sentence(self) -> &'static str {
        match self {
            Self::Done => "The board has read it again",
            Self::Reading => "The board is reading it again",
            Self::Waiting => "The board reads it again when this tab is shown",
            Self::Stopped => "The board stopped reading before it read it again",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ReadEnd {
    Accepted,
    Failed,
    Throttled {
        /// From [`crate::evidence_queue::retry_after_ms`].
        retry_after_ms: f64,
    },
    /// The server refused the read for want of a grant, or no longer
    /// holds the investigation.
    Refused,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Liveness {
    connection: Connection,
    failures: u32,
    /// The instant a 429 asked the board not to read before.
    hold_until: Option<f64>,
    last_ok: Option<f64>,
}

impl Liveness {
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

    #[must_use]
    pub fn stopped(&self) -> bool {
        self.connection == Connection::Stopped
    }

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

    pub fn hidden(&mut self) {
        if !self.stopped() {
            self.connection = Connection::Paused;
        }
    }

    /// The tab shows at `now`: how long until the board reads, `Some(0)`
    /// for at once, `None` when it no longer reads.
    pub fn shown(&mut self, now: f64) -> Option<u32> {
        if self.stopped() {
            return None;
        }
        self.connection = if self.failures > 0 {
            Connection::Retrying
        } else {
            Connection::Checking
        };
        Some(self.hold_ms(now))
    }

    /// How long the pause a 429 asked for still holds the board's reads
    /// at `now`; zero when none runs. Only an accepted read ends it early.
    #[must_use]
    pub fn hold_ms(&self, now: f64) -> u32 {
        self.hold_until.map_or(0, |until| wait_ms(now, until))
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasonProblem {
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
    Unauthorized,
    Refused(String),
    /// No answer was read: the write may or may not have landed.
    Unknown,
}

/// A server error does not prove the write was refused: trawld can fail
/// after its commit, and a gateway error may stand for an answer lost on
/// the way. So every 5xx is unknown, except `request_limit_reached`, which
/// trawld answers at admission, before any handler (ADR-0054).
#[must_use]
pub fn write_refusal(status: u16, body: &str) -> WriteFailure {
    if status == 401 {
        return WriteFailure::Unauthorized;
    }
    let envelope = serde_json::from_str::<ErrorResponse>(body);
    if status >= 500 {
        return match envelope {
            Ok(ErrorResponse { error }) if error.code == ErrorCode::RequestLimitReached => {
                WriteFailure::Refused(error.message)
            }
            _ => WriteFailure::Unknown,
        };
    }
    match envelope {
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
        let mut short = live_board(0.0);
        short.ended(
            ReadEnd::Throttled {
                retry_after_ms: 1_000.0,
            },
            0.0,
        );
        assert_eq!(short.next_delay(0.0), Some(POLL_MS));
        live.hidden();
        assert_eq!(live.shown(11_000.0), Some(20_000));
        assert_eq!(live.connection(), Connection::Retrying);
        assert_eq!(live.shown(31_000.0), Some(0));
        live.ended(ReadEnd::Accepted, 31_500.0);
        assert_eq!(live.next_delay(31_500.0), Some(POLL_MS));
    }

    #[test]
    fn a_429s_pause_holds_through_hiding_and_failures_until_it_ends_or_a_read_lands() {
        let mut live = live_board(0.0);
        assert_eq!(live.hold_ms(0.0), 0);
        live.ended(
            ReadEnd::Throttled {
                retry_after_ms: 30_000.0,
            },
            5_000.0,
        );
        assert_eq!(live.hold_ms(5_000.0), 30_000);
        live.hidden();
        assert_eq!(live.hold_ms(15_000.0), 20_000);
        assert_eq!(live.shown(15_000.0), Some(20_000));
        live.hidden();
        assert_eq!(live.shown(34_000.0), Some(1_000));
        live.ended(ReadEnd::Failed, 20_000.0);
        assert_eq!(live.hold_ms(20_000.0), 15_000);
        assert_eq!(live.hold_ms(35_000.0), 0);
        assert_eq!(live.hold_ms(40_000.0), 0);
        live.ended(
            ReadEnd::Throttled {
                retry_after_ms: 30_000.0,
            },
            40_000.0,
        );
        live.ended(ReadEnd::Accepted, 41_000.0);
        assert_eq!(live.hold_ms(41_000.0), 0);
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

    #[test]
    fn reread_prefers_an_accepted_read_over_a_stop() {
        use Connection::{Checking, Live, Paused, Retrying, Stopped};
        let all = [Checking, Live, Paused, Retrying, Stopped];
        for connection in all {
            assert_eq!(Reread::of(4, 5, connection), Reread::Done, "{connection:?}");
        }
        for accepted in [3, 4] {
            for (connection, reread) in [
                (Checking, Reread::Reading),
                (Live, Reread::Reading),
                (Paused, Reread::Waiting),
                (Retrying, Reread::Reading),
                (Stopped, Reread::Stopped),
            ] {
                assert_eq!(
                    Reread::of(4, accepted, connection),
                    reread,
                    "{connection:?}"
                );
            }
        }
        assert_eq!(
            Reread::Stopped.sentence(),
            "The board stopped reading before it read it again"
        );
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
        assert_eq!(pick_anchor(&placed, 200.0), Some(3));
    }

    #[test]
    fn a_section_whose_entries_are_all_above_the_line_passes_to_the_next() {
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
        assert_eq!(keep_reason(&format!("  {ascii}  ")), Ok(ascii.as_str()));
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
        assert_eq!(write_refusal(502, "<html>"), WriteFailure::Unknown);
        assert_eq!(write_refusal(504, closed), WriteFailure::Unknown);
        assert_eq!(write_refusal(500, ""), WriteFailure::Unknown);
        assert_eq!(
            write_refusal(400, ""),
            WriteFailure::Refused("The server answered 400.".to_owned())
        );
    }

    #[test]
    fn a_server_error_is_unknown_even_with_trawlds_envelope() {
        for (status, code) in [
            (500, "internal_error"),
            (500, "execution_error"),
            (503, "service_unavailable"),
            (503, "corpus_recovering"),
            (504, "timeout"),
        ] {
            let body = format!(r#"{{"error":{{"code":"{code}","message":"stub"}}}}"#);
            assert_eq!(
                write_refusal(status, &body),
                WriteFailure::Unknown,
                "{code}"
            );
        }
        let limit = r#"{"error":{"code":"request_limit_reached","message":"trawld is at its HTTP request limit ([server] max_concurrent_requests); the request was not processed; retry later with backoff"}}"#;
        assert_eq!(
            write_refusal(503, limit),
            WriteFailure::Refused(
                "trawld is at its HTTP request limit ([server] max_concurrent_requests); the \
                 request was not processed; retry later with backoff"
                    .to_owned()
            )
        );
    }
}
