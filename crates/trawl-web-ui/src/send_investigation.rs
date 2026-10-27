// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What "Send to investigation" sends from Search (ADR-0057, ADR-0059).
//!
//! The Search page sends the search it ran, never the editor's draft: the
//! accepted response's own query state and its server start instant,
//! taken once when the dialog opens ([`SendSnapshot`]). The server runs
//! the capture itself, attached to the chosen investigation under the
//! session's key, so the server attests the evidence and the browser
//! only names it.
//!
//! [`plan`] reads that state as a capture request through the shared
//! codec ([`capture_parts`]): the range-free text and a half-open window
//! for a fixed absolute range, or the text alone when an absolute
//! `earliest=` and `latest=` in it close it. A clause in the text that
//! moves (`last=`, or a bound with no other end) is refused: the range
//! control cannot pin text, and rewriting the query is not the page's to
//! do, so the person removes the clause and picks the range instead.
//! A range that moves (a quick range, or an open end) is never sent: a
//! capture is immutable, so the page first pins it to the instant the
//! search ran ([`pin_range`], from the response's `execution.started_at`,
//! the server's clock), runs Search again on the pinned absolute range,
//! and sends that execution. A `from saved` search takes no bounds at all
//! and is not offered ([`offers_send`]).
//!
//! A capture is a write: an answer that never arrived is an unknown
//! outcome ([`SendFailure::Unknown`]), and nothing here sends it again.
//!
//! Pure and ungated so its tests run natively; the dialog that drives it
//! is wasm32-only.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use trawl_api::investigations::{
    InvestigationId, InvestigationList, InvestigationStatus, PURPOSE_MAX_BYTES,
};
use trawl_api::{ErrorCode, ErrorResponse};
use trawl_core::ast::PipeStage;

use crate::investigation_live::{WriteFailure, write_refusal};
use crate::query_merge::{
    CaptureParts, CaptureRefusal, Filter, PinRefusal, RangeSpec, capture_parts, pin_range,
};

/// The accepted execution the dialog sends, taken when it opens. Later
/// edits to the draft, or a later URL, never change it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendSnapshot {
    /// The URL's query text the response ran (`q`).
    pub base: String,
    /// The URL's filters the response ran.
    pub filters: Vec<Filter>,
    /// The URL's range the response ran.
    pub range: RangeSpec,
    /// When the server started the execution, as `execution.started_at`.
    pub started_at: String,
}

/// What the dialog can do with a [`SendSnapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendPlan {
    /// Send these parts now.
    Ready(CaptureParts),
    /// The range moves. Pinning it to the instant the search ran gives
    /// this absolute range, which Search runs before anything is sent.
    Pin(RangeSpec),
    /// Nothing can be sent, for this reason.
    Refused(String),
}

/// Why a search is not sent, as the dialog says it.
const EMPTY_QUERY: &str = "There is no query to send.";

/// The line for a range past the capture span limit, the limit in the
/// largest whole unit that spells it exactly.
#[must_use]
pub fn span_copy(max_span_secs: u64) -> String {
    let (count, unit) = [(86_400, "day"), (3_600, "hour"), (60, "minute")]
        .into_iter()
        .find(|(size, _)| max_span_secs.is_multiple_of(*size) && max_span_secs >= *size)
        .map_or((max_span_secs, "second"), |(size, unit)| {
            (max_span_secs / size, unit)
        });
    let plural = if count == 1 { "" } else { "s" };
    format!(
        "The range is longer than one capture may cover, {count} {unit}{plural} on this server. \
         Pick a shorter range, run the search, then send it."
    )
}
const UNREADABLE_RANGE: &str = "The search's range cannot be read, so it cannot be sent.";
const RANGE_OVERFLOW: &str = "The search's range ends past the year 9999, so it cannot be sent.";
const UNREADABLE_ANCHOR: &str =
    "The server did not say when this search ran, so its range cannot be pinned.";
const TEXT_MOVES: &str = "This query sets its own time range, and that range moves: it ends \
     when the capture runs, not when this search ran. A capture is fixed, so this search cannot \
     be sent. Remove last=, earliest= and latest= from the query, pick the range in the range \
     control, run the search, then send it.";

/// Read `snapshot` as what the dialog sends, under the capture span limit
/// the server advertises, `max_span_secs` (`None` when it could not be
/// read: then the server alone decides). A moving range is pinned first,
/// and a pin whose capture would be past the limit is not offered either.
#[must_use]
pub fn plan(snapshot: &SendSnapshot, max_span_secs: Option<u64>) -> SendPlan {
    let parts =
        |range: &RangeSpec| capture_parts(&snapshot.base, &snapshot.filters, range, max_span_secs);
    match parts(&snapshot.range) {
        Ok(parts) => SendPlan::Ready(parts),
        Err(CaptureRefusal::Moving) => match pin_range(&snapshot.range, &snapshot.started_at) {
            Ok(pinned) => match parts(&pinned) {
                Err(refusal @ CaptureRefusal::SpanTooLong { .. }) => refused(refusal),
                _ => SendPlan::Pin(pinned),
            },
            Err(PinRefusal::UnreadableAnchor) => SendPlan::Refused(UNREADABLE_ANCHOR.to_owned()),
            Err(PinRefusal::UnreadableRange) => SendPlan::Refused(UNREADABLE_RANGE.to_owned()),
        },
        Err(refusal) => refused(refusal),
    }
}

/// The dialog's sentence for a capture the codec refuses.
fn refused(refusal: CaptureRefusal) -> SendPlan {
    SendPlan::Refused(match refusal {
        CaptureRefusal::EmptyQuery => EMPTY_QUERY.to_owned(),
        CaptureRefusal::Moving | CaptureRefusal::UnreadableRange => UNREADABLE_RANGE.to_owned(),
        CaptureRefusal::RangeOverflow => RANGE_OVERFLOW.to_owned(),
        CaptureRefusal::TextMoves => TEXT_MOVES.to_owned(),
        CaptureRefusal::SpanTooLong { max_span_secs } => span_copy(max_span_secs),
    })
}

/// Whether Search offers to send the search that ran `effective`. A
/// `from saved` search reads a stored run, takes no bounds and no window
/// (ADR-0057), so the control is absent for it. Text the parser refuses
/// never reaches an accepted response, and is not offered either.
#[must_use]
pub fn offers_send(effective: &str) -> bool {
    trawl_core::parser::parse(effective).is_ok_and(|query| {
        !query
            .pipeline
            .iter()
            .any(|stage| matches!(stage.node, PipeStage::FromSaved(_)))
    })
}

/// Why a purpose is not sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PurposeProblem {
    /// Longer than the server takes, in bytes of UTF-8.
    TooLong { bytes: usize },
    /// It holds a NUL character, which the server cannot store.
    Nul,
}

impl PurposeProblem {
    /// The sentence the dialog shows under the field.
    #[must_use]
    pub fn copy(self) -> String {
        match self {
            Self::TooLong { bytes } => format!(
                "The purpose is {bytes} bytes. The server takes at most {PURPOSE_MAX_BYTES}."
            ),
            Self::Nul => "The purpose cannot hold a NUL character.".to_owned(),
        }
    }
}

/// The purpose a send states, `text` trimmed, when the server would take
/// it: `None` when it is blank. The bound is the server's, counted in
/// bytes of UTF-8 as the server counts it.
///
/// # Errors
/// The [`PurposeProblem`] the server would refuse it for.
pub fn purpose(text: &str) -> Result<Option<&str>, PurposeProblem> {
    let purpose = text.trim();
    if purpose.is_empty() {
        Ok(None)
    } else if purpose.contains('\0') {
        Err(PurposeProblem::Nul)
    } else if purpose.len() > PURPOSE_MAX_BYTES {
        Err(PurposeProblem::TooLong {
            bytes: purpose.len(),
        })
    } else {
        Ok(Some(purpose))
    }
}

/// One investigation the dialog offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub id: InvestigationId,
    pub title: String,
}

impl Choice {
    /// The first eight digits of the id, beside the title, so two
    /// investigations with one title stay apart.
    #[must_use]
    pub fn short_id(&self) -> &str {
        let id = self.id.as_str();
        id.get(..8).unwrap_or(id)
    }
}

/// The open investigations in `list`, in the server's order. A closed one
/// takes no attached work.
#[must_use]
pub fn open_choices(list: &InvestigationList) -> Vec<Choice> {
    list.investigations
        .iter()
        .filter(|summary| summary.status == InvestigationStatus::Open)
        .map(|summary| Choice {
            id: summary.id.clone(),
            title: summary.title.clone(),
        })
        .collect()
}

/// Why a send did not land.
#[derive(Debug, Clone, PartialEq)]
pub enum SendFailure {
    /// 409 `investigation_closed`: it was closed when the capture reached
    /// it. Nothing ran.
    Closed,
    /// 429: too many captures are running for the key, or its capture
    /// quota is full. Nothing ran; the server asks for this pause.
    Busy {
        message: String,
        retry_after_ms: f64,
    },
    /// The session is gone (401).
    Unauthorized,
    /// The server refused it, in its own words when it gave any.
    Refused(String),
    /// No answer was read: the capture may or may not have run.
    Unknown,
}

/// What a capture's non-2xx answer, `status` with `body`, says.
/// `retry_after_ms` is the pause a 429's `Retry-After` asks for. A lost
/// answer is classified as a board write's is ([`write_refusal`]).
#[must_use]
pub fn send_refusal(status: u16, body: &str, retry_after_ms: f64) -> SendFailure {
    if status == 429 {
        let message = match serde_json::from_str::<ErrorResponse>(body) {
            Ok(ErrorResponse { error }) if error.code != ErrorCode::InternalError => error.message,
            _ => "Too many captures are running.".to_owned(),
        };
        return SendFailure::Busy {
            message,
            retry_after_ms,
        };
    }
    match write_refusal(status, body) {
        WriteFailure::Closed => SendFailure::Closed,
        WriteFailure::Unauthorized => SendFailure::Unauthorized,
        WriteFailure::Unknown => SendFailure::Unknown,
        WriteFailure::Refused(message) => SendFailure::Refused(message),
        // A capture carries no expected sequence, so the server never
        // answers this one; its sentence is still the server's.
        WriteFailure::Stale { .. } => {
            SendFailure::Refused("The investigation changed. Nothing was sent.".to_owned())
        }
    }
}

/// The dialog's line for a busy answer, in whole seconds, never below one.
#[must_use]
pub fn busy_copy(message: &str, retry_after_ms: f64) -> String {
    // A pause is a few seconds at most; the cast cannot truncate.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let seconds = (retry_after_ms / 1000.0).ceil().max(1.0) as u64;
    let unit = if seconds == 1 { "second" } else { "seconds" };
    format!("{message} Nothing was sent. Try again in {seconds} {unit}.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query_merge::{CaptureWindow, FilterOp};

    fn snapshot(base: &str, range: RangeSpec) -> SendSnapshot {
        SendSnapshot {
            base: base.to_owned(),
            filters: vec![Filter {
                field: "host".into(),
                value: "web-01".into(),
                op: FilterOp::Include,
            }],
            range,
            started_at: "2026-10-26T12:34:56.789000Z".to_owned(),
        }
    }

    fn abs(from: &str, to: &str) -> RangeSpec {
        RangeSpec::Absolute {
            from: from.into(),
            to: to.into(),
        }
    }

    #[test]
    fn a_fixed_range_sends_the_range_free_text_and_a_half_open_window() {
        let plan = plan(
            &snapshot(
                "status>=500",
                abs("2026-10-26T12:00:00Z", "2026-10-26T12:30:00Z"),
            ),
            None,
        );
        assert_eq!(
            plan,
            SendPlan::Ready(CaptureParts {
                query: "host=\"web-01\" status>=500".into(),
                window: Some(CaptureWindow {
                    earliest: "2026-10-26T12:00:00.000000Z".into(),
                    latest: "2026-10-26T12:30:00.000001Z".into(),
                }),
            })
        );
    }

    /// The pin is anchored on the server's start instant, never the
    /// browser's clock, and is the whole-second hull of what ran.
    #[test]
    fn a_moving_range_pins_to_the_instant_the_search_ran() {
        assert_eq!(
            plan(&snapshot("status>=500", RangeSpec::Quick("15m")), None),
            SendPlan::Pin(abs("2026-10-26T12:19:56Z", "2026-10-26T12:34:57Z"))
        );
        assert_eq!(
            plan(&snapshot("*", abs("2026-10-26T12:00:00Z", "now")), None),
            SendPlan::Pin(abs("2026-10-26T12:00:00Z", "2026-10-26T12:34:57Z"))
        );
    }

    #[test]
    fn a_pinned_range_then_sends() {
        let mut pinned = snapshot("status>=500", RangeSpec::Quick("15m"));
        let SendPlan::Pin(range) = plan(&pinned, None) else {
            panic!("a pin");
        };
        pinned.range = range;
        let SendPlan::Ready(parts) = plan(&pinned, None) else {
            panic!("ready after the pin");
        };
        assert_eq!(
            parts.window,
            Some(CaptureWindow {
                earliest: "2026-10-26T12:19:56.000000Z".into(),
                latest: "2026-10-26T12:34:57.000001Z".into(),
            })
        );
    }

    /// A closed absolute clause in the text is fixed: it is sent as it
    /// is, with no window, whatever the range control says.
    #[test]
    fn a_closed_text_time_clause_sends_without_a_window_whatever_the_range() {
        let plan = plan(
            &snapshot(
                r#"earliest="2026-10-26T09:00:00Z" latest="2026-10-26T10:00:00Z" status>=500"#,
                RangeSpec::Quick("15m"),
            ),
            None,
        );
        let SendPlan::Ready(parts) = plan else {
            panic!("ready");
        };
        assert_eq!(parts.window, None);
    }

    /// Send never sends a moving range. A moving clause in the query text
    /// is not the range control's to pin, so nothing is offered: neither
    /// Send nor Pin and re-run, whatever the range is.
    #[test]
    fn a_moving_text_time_clause_is_refused() {
        for base in [
            "last=1h status>=500",
            r#"earliest="2026-10-26T09:00:00Z" status>=500"#,
        ] {
            for range in [
                RangeSpec::Quick("15m"),
                abs("2026-10-26T09:00:00Z", "2026-10-26T10:00:00Z"),
            ] {
                assert_eq!(
                    plan(&snapshot(base, range), None),
                    SendPlan::Refused(TEXT_MOVES.to_owned()),
                    "{base}"
                );
            }
        }
    }

    /// The server refuses a capture longer than its `max_span_secs`, so a
    /// range that would cover more is neither pinned nor sent: the 7-day
    /// preset pins to seven days and a microsecond. The range is never
    /// narrowed to fit, and an unknown limit refuses nothing.
    #[test]
    fn a_range_past_the_capture_span_limit_offers_nothing() {
        let week = Some(604_800);
        let refused = SendPlan::Refused(span_copy(604_800));
        assert_eq!(
            plan(&snapshot("status>=500", RangeSpec::Quick("7d")), week),
            refused
        );
        assert_eq!(
            plan(
                &snapshot(
                    "status>=500",
                    abs("2026-10-19T12:00:00Z", "2026-10-26T12:00:00Z")
                ),
                week
            ),
            refused
        );
        assert!(matches!(
            plan(&snapshot("status>=500", RangeSpec::Quick("24h")), week),
            SendPlan::Pin(_)
        ));
        assert!(matches!(
            plan(&snapshot("status>=500", RangeSpec::Quick("7d")), None),
            SendPlan::Pin(_)
        ));
        assert_eq!(
            span_copy(604_800),
            "The range is longer than one capture may cover, 7 days on this server. Pick a \
             shorter range, run the search, then send it."
        );
        assert!(span_copy(3_600).contains(", 1 hour on"));
        assert!(span_copy(90).contains(", 90 seconds on"));
    }

    #[test]
    fn an_unreadable_anchor_or_range_refuses() {
        let mut bad_anchor = snapshot("*", RangeSpec::Quick("15m"));
        bad_anchor.started_at = "yesterday".into();
        assert_eq!(
            plan(&bad_anchor, None),
            SendPlan::Refused(UNREADABLE_ANCHOR.to_owned())
        );
        assert_eq!(
            plan(&snapshot("*", abs("soon", "2026-10-26T12:00:00Z")), None),
            SendPlan::Refused(UNREADABLE_RANGE.to_owned())
        );
        assert_eq!(
            plan(
                &snapshot("  ", abs("2026-10-26T12:00:00Z", "2026-10-26T12:00:01Z")),
                None
            ),
            SendPlan::Refused(EMPTY_QUERY.to_owned())
        );
        assert_eq!(
            plan(
                &snapshot(
                    "*",
                    abs("9999-12-31T23:59:59Z", "9999-12-31T23:59:59.999999Z")
                ),
                None
            ),
            SendPlan::Refused(RANGE_OVERFLOW.to_owned())
        );
    }

    #[test]
    fn a_from_saved_search_is_not_offered() {
        assert!(offers_send("status>=500 | stats count() by host"));
        assert!(offers_send("*"));
        assert!(!offers_send("| from saved nightly"));
        assert!(!offers_send("| from saved nightly | stats count() by host"));
        // The words in a value are a value.
        assert!(offers_send("message=\"| from saved nightly\""));
        assert!(!offers_send(
            "service=nginx // filter by service\n| stats count()"
        ));
    }

    #[test]
    fn a_purpose_is_trimmed_bounded_in_bytes_and_free_of_nul() {
        assert_eq!(purpose("   "), Ok(None));
        assert_eq!(purpose("  why  "), Ok(Some("why")));
        let at_bound = "é".repeat(PURPOSE_MAX_BYTES / 2);
        assert_eq!(purpose(&at_bound), Ok(Some(at_bound.as_str())));
        // 513 characters, 1026 bytes: the server counts bytes.
        let over = "é".repeat(PURPOSE_MAX_BYTES / 2 + 1);
        assert_eq!(
            purpose(&over),
            Err(PurposeProblem::TooLong {
                bytes: PURPOSE_MAX_BYTES + 2
            })
        );
        assert_eq!(purpose("a\0b"), Err(PurposeProblem::Nul));
    }

    #[test]
    fn only_open_investigations_are_offered() {
        let list: InvestigationList =
            serde_json::from_str(include_str!("../e2e/harness/wire/investigations-list.json"))
                .expect("the list fixture");
        let choices = open_choices(&list);
        assert_eq!(
            choices
                .iter()
                .map(|c| (c.short_id(), c.title.as_str()))
                .collect::<Vec<_>>(),
            [
                ("5f0c1a2b", "502s on checkout since 09:00"),
                ("c0ffee00", "Nightly backup job slower than usual"),
            ]
        );
    }

    #[test]
    fn a_capture_answer_is_read_as_a_write() {
        let closed = r#"{"error":{"code":"investigation_closed","message":"closed"}}"#;
        assert_eq!(send_refusal(409, closed, 0.0), SendFailure::Closed);
        let busy = r#"{"error":{"code":"capture_busy","message":"too many captures are running; retry after the Retry-After interval"}}"#;
        assert_eq!(
            send_refusal(429, busy, 2000.0),
            SendFailure::Busy {
                message: "too many captures are running; retry after the Retry-After interval"
                    .into(),
                retry_after_ms: 2000.0,
            }
        );
        assert_eq!(
            send_refusal(429, "", 1000.0),
            SendFailure::Busy {
                message: "Too many captures are running.".into(),
                retry_after_ms: 1000.0,
            }
        );
        let parse = r#"{"error":{"code":"parse_error","message":"expected a value"}}"#;
        assert_eq!(
            send_refusal(400, parse, 0.0),
            SendFailure::Refused("expected a value".into())
        );
        assert_eq!(send_refusal(401, "", 0.0), SendFailure::Unauthorized);
        // A lost answer is not a refusal.
        assert_eq!(send_refusal(502, "", 0.0), SendFailure::Unknown);
        assert_eq!(send_refusal(500, "<html>", 0.0), SendFailure::Unknown);
    }

    #[test]
    fn a_busy_line_names_whole_seconds() {
        assert_eq!(
            busy_copy("Busy.", 1000.0),
            "Busy. Nothing was sent. Try again in 1 second."
        );
        assert_eq!(
            busy_copy("Busy.", 2500.0),
            "Busy. Nothing was sent. Try again in 3 seconds."
        );
        assert_eq!(
            busy_copy("Busy.", 0.0),
            "Busy. Nothing was sent. Try again in 1 second."
        );
    }
}
