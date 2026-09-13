// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a schedule edit carries back, and what it tells the operator.
//!
//! `PUT .../schedule` takes the schedule's whole shape, so an omitted
//! `window` does not mean "leave it alone", it means query mode. The
//! drawer's form therefore holds a whole [`WindowDraft`] rather than a
//! set of fields it may or may not send: the draft opens seeded from
//! what the server reported, and [`WindowDraft::to_request`] decides
//! from the chosen mode alone which halves travel. What the operator
//! reads is what the request carries, off the same value.
//!
//! Nothing here parses a duration or inspects the DSL. The browser has
//! no copy of either grammar, and inventing one would let the form
//! disagree with the server about what it just accepted. The refusals
//! this module does make ([`ScheduleEditError`]) are the ones a form can
//! make without a grammar: a chosen span left blank, a max-runs box
//! holding something that is not a number.
//!
//! Pure + ungated so its tests run natively; the callers are
//! wasm32-only.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use trawl_api::ScheduleResponse;

/// Which report window a schedule edit is about to send.
///
/// The ids are the wire's own: `window` is absent for query mode,
/// literally `since_last` for tiling, and any other string is a span the
/// server parses. Nothing here parses a duration — the browser has no
/// duration grammar and inventing one would let the form disagree with
/// the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowMode {
    /// The saved text runs as written; the request omits `window`.
    Query,
    /// Each run covers from the last covered point to the run time.
    SinceLast,
    /// Each run covers a trailing span measured from the run time.
    Fixed,
}

impl WindowMode {
    /// The control's option id, and the value the segmented strip
    /// round-trips through.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::SinceLast => "since_last",
            Self::Fixed => "fixed",
        }
    }

    /// The mode an option id names, or `None` for anything else.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "query" => Some(Self::Query),
            "since_last" => Some(Self::SinceLast),
            "fixed" => Some(Self::Fixed),
            _ => None,
        }
    }
}

/// What the form holds for the window half of a schedule edit.
///
/// The span and lag buffers survive a mode toggle, so an operator who
/// looks at query mode and comes back finds their typing intact. Which
/// of them travels is [`WindowDraft::to_request`]'s decision, taken from
/// `mode` alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowDraft {
    pub mode: WindowMode,
    pub span: String,
    pub lag: String,
}

/// A refusal the form makes on its own, before any request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleEditError {
    /// Fixed span was chosen with nothing in the span box.
    BlankFixedSpan,
    /// Max runs held something that is not a whole number.
    InvalidMaxRuns,
}

impl std::fmt::Display for ScheduleEditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::BlankFixedSpan => "Enter a span.",
            Self::InvalidMaxRuns => "Max runs must be a whole number of 1 or more, or blank.",
        })
    }
}

impl std::error::Error for ScheduleEditError {}

impl WindowDraft {
    /// Seed the draft from what the server reports, so an untouched form
    /// sends the pair back unchanged.
    ///
    /// A lag of zero seeds blank: zero is what the server applies when
    /// the field is absent, so showing `0s` would invite an operator to
    /// "keep" a value that means nothing.
    #[must_use]
    pub fn from_schedule(schedule: Option<&ScheduleResponse>) -> Self {
        let Some(schedule) = schedule else {
            return Self {
                mode: WindowMode::Query,
                span: String::new(),
                lag: String::new(),
            };
        };
        let (mode, span) = match schedule.window.as_deref() {
            None => (WindowMode::Query, String::new()),
            Some("since_last") => (WindowMode::SinceLast, String::new()),
            Some(span) => (WindowMode::Fixed, span.to_owned()),
        };
        let lag = match schedule.lag_secs {
            Some(secs) if secs > 0 => schedule.lag.clone().unwrap_or_default(),
            _ => String::new(),
        };
        Self { mode, span, lag }
    }

    /// The `(window, lag)` pair a PUT carries.
    ///
    /// Query mode sends neither field whatever the buffers hold: the PUT
    /// takes the schedule's whole shape, so an omitted `window` means
    /// query mode rather than "unchanged", and a lag with no window is a
    /// 400.
    ///
    /// # Errors
    /// [`ScheduleEditError::BlankFixedSpan`] when a fixed window has no
    /// span to send.
    pub fn to_request(&self) -> Result<(Option<String>, Option<String>), ScheduleEditError> {
        let window = match self.mode {
            WindowMode::Query => return Ok((None, None)),
            WindowMode::SinceLast => "since_last".to_owned(),
            WindowMode::Fixed => {
                let span = self.span.trim();
                if span.is_empty() {
                    return Err(ScheduleEditError::BlankFixedSpan);
                }
                span.to_owned()
            }
        };
        let lag = self.lag.trim();
        let lag = (!lag.is_empty()).then(|| lag.to_owned());
        Ok((Some(window), lag))
    }
}

/// The max-runs field as a request value.
///
/// Blank is unlimited. Anything that is not a whole number is refused
/// here rather than silently becoming unlimited, which is what a
/// discarded parse used to do.
///
/// # Errors
/// [`ScheduleEditError::InvalidMaxRuns`] when the text is neither blank
/// nor a `u64`. Zero travels: the server owns that judgement.
pub fn validate_max_runs(text: &str) -> Result<Option<u64>, ScheduleEditError> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    text.parse::<u64>()
        .map(Some)
        .map_err(|_| ScheduleEditError::InvalidMaxRuns)
}

/// The line a stored run's preview shows when the run stored more rows
/// than the response carries, or `None` when paging covers everything.
///
/// The preview pages the rows it was given; it never re-fetches. Saying
/// so is the difference between a pager that looks broken and one whose
/// limits are stated.
#[must_use]
pub fn preview_cap(row_count: Option<usize>, fetched: usize) -> Option<String> {
    let n = row_count?;
    (n > fetched).then(|| {
        format!("This run stored {n} rows; {fetched} were fetched. Paging covers the fetched rows.")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A schedule as the server reports it. Only the four fields this
    /// module reads carry anything.
    fn schedule(window: Option<&str>, lag: Option<(&str, u64)>) -> ScheduleResponse {
        ScheduleResponse {
            id: 1,
            saved_query_id: 7,
            interval: "1h".to_owned(),
            interval_secs: 3600,
            max_runs: None,
            enabled: true,
            created_at: "2026-03-14T02:00:00Z".to_owned(),
            updated_at: "2026-03-14T02:00:00Z".to_owned(),
            last_run: None,
            total_runs: 0,
            window: window.map(str::to_owned),
            lag: lag.map(|(text, _)| text.to_owned()),
            lag_secs: lag.map(|(_, secs)| secs),
            covered_through: None,
            next_fire_at: "2026-03-14T03:00:00Z".to_owned(),
        }
    }

    /// Every mode the strip can press is a mode the draft can hold, and
    /// an unknown id is refused rather than guessed at.
    #[test]
    fn window_mode_ids_round_trip() {
        for mode in [WindowMode::Query, WindowMode::SinceLast, WindowMode::Fixed] {
            assert_eq!(WindowMode::from_id(mode.id()), Some(mode));
        }
        assert_eq!(WindowMode::from_id("tiling"), None);
    }

    /// The form opens showing what the server holds, including the
    /// silence a zero lag earns.
    #[test]
    fn from_schedule_seeds_the_draft() {
        let none = WindowDraft::from_schedule(None);
        assert_eq!(none.mode, WindowMode::Query);
        assert_eq!(none.span, "");
        assert_eq!(none.lag, "");

        let query = WindowDraft::from_schedule(Some(&schedule(None, None)));
        assert_eq!(query.mode, WindowMode::Query);
        assert_eq!(query.span, "");
        assert_eq!(query.lag, "");

        let tiling =
            WindowDraft::from_schedule(Some(&schedule(Some("since_last"), Some(("5m", 300)))));
        assert_eq!(tiling.mode, WindowMode::SinceLast);
        assert_eq!(tiling.lag, "5m");

        let fixed = WindowDraft::from_schedule(Some(&schedule(Some("2h"), Some(("0s", 0)))));
        assert_eq!(fixed.mode, WindowMode::Fixed);
        assert_eq!(fixed.span, "2h");
        assert_eq!(fixed.lag, "");
    }

    /// The old pass-through guarantee, now a property of the draft: a
    /// form nobody touched puts the server's own pair back on the wire.
    #[test]
    fn an_untouched_draft_repeats_the_server_pair() {
        let tiling =
            WindowDraft::from_schedule(Some(&schedule(Some("since_last"), Some(("5m", 300)))));
        assert_eq!(
            tiling.to_request(),
            Ok((Some("since_last".to_owned()), Some("5m".to_owned())))
        );
        let fixed = WindowDraft::from_schedule(Some(&schedule(Some("2h"), Some(("5m", 300)))));
        assert_eq!(
            fixed.to_request(),
            Ok((Some("2h".to_owned()), Some("5m".to_owned())))
        );
    }

    /// Query mode drops both fields whatever the buffers still hold, and
    /// a windowed mode with a blank lag sends the window alone.
    #[test]
    fn query_mode_sends_neither_field() {
        let draft = WindowDraft {
            mode: WindowMode::Query,
            span: "2h".to_owned(),
            lag: "5m".to_owned(),
        };
        assert_eq!(draft.to_request(), Ok((None, None)));

        let tiling = WindowDraft {
            mode: WindowMode::SinceLast,
            span: "2h".to_owned(),
            lag: "   ".to_owned(),
        };
        assert_eq!(
            tiling.to_request(),
            Ok((Some("since_last".to_owned()), None))
        );
    }

    /// What the form refuses, it refuses before any request. Zero max
    /// runs is not one of those: the server owns that judgement.
    #[test]
    fn local_refusals_send_nothing() {
        let blank = WindowDraft {
            mode: WindowMode::Fixed,
            span: "  ".to_owned(),
            lag: "5m".to_owned(),
        };
        assert_eq!(blank.to_request(), Err(ScheduleEditError::BlankFixedSpan));

        assert_eq!(
            validate_max_runs("abc"),
            Err(ScheduleEditError::InvalidMaxRuns)
        );
        assert_eq!(validate_max_runs("0"), Ok(Some(0)));
        assert_eq!(validate_max_runs(""), Ok(None));
        assert_eq!(validate_max_runs(" 3 "), Ok(Some(3)));
    }

    /// The cap line appears only when rows were left behind, so an
    /// uncapped preview says nothing about fetching at all.
    #[test]
    fn preview_cap_names_the_gap_only_when_capped() {
        assert_eq!(
            preview_cap(Some(100), 45).as_deref(),
            Some("This run stored 100 rows; 45 were fetched. Paging covers the fetched rows.")
        );
        assert_eq!(preview_cap(Some(45), 45), None);
        assert_eq!(preview_cap(None, 45), None);
    }
}
