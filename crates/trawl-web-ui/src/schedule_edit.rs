// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a schedule edit carries back, and what it tells the operator.
//!
//! The drawer's schedule form has inputs for interval, max runs and
//! enabled. It has none for the report window, because ADR-0018 put the
//! window on the schedule and the form for it is later work. That leaves a
//! trap: `PUT .../schedule` takes the schedule's whole shape, and an
//! omitted `window` does not mean "leave it alone", it means query mode.
//! An edit that dropped the field would retype a tiling schedule and its
//! next run would scan the corpus with no bounds at all, from one operator
//! flipping the Active toggle.
//!
//! So the edit sends the server's own copy straight back, and the form
//! prints what it is repeating. Both answers come from here, off the same
//! function, so what the operator reads is what the request carries.
//!
//! Pure + ungated so its tests run natively; the one caller is
//! wasm32-only.

// The draft below has no caller until the drawer's form is rewritten, and
// the native build has none at all.
#![allow(dead_code)]

use trawl_api::ScheduleResponse;

/// The `window` and `lag` a PUT has to repeat to leave a schedule's
/// coverage untouched.
///
/// `lag` rides the window and is repeated only when it is a real
/// allowance. A windowed schedule always reports `lag_secs`, and zero is
/// what the server applies when the field is absent, so sending `"0s"`
/// back would add a field that changes nothing. A query-mode schedule
/// answers `(None, None)`, which is also what the server refuses to accept
/// a lag beside.
pub fn preserved_window_and_lag(schedule: &ScheduleResponse) -> (Option<String>, Option<String>) {
    let Some(window) = schedule.window.clone() else {
        return (None, None);
    };
    let lag = match schedule.lag_secs {
        Some(secs) if secs > 0 => schedule.lag.clone(),
        _ => None,
    };
    (Some(window), lag)
}

/// The one read-only line the form shows for a windowed schedule, or
/// `None` when there is nothing to preserve.
///
/// Built from [`preserved_window_and_lag`] rather than from the response
/// again, so the line cannot name a lag the request drops or stay silent
/// about one it sends.
pub fn window_summary(schedule: &ScheduleResponse) -> Option<String> {
    let (window, lag) = preserved_window_and_lag(schedule);
    let window = window?;
    Some(match lag {
        Some(lag) => format!("window {window}, lag {lag}  (edit via TUI or API)"),
        None => format!("window {window}  (edit via TUI or API)"),
    })
}

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

    /// The whole point: an edit repeats the window it did not touch. Both
    /// modes travel, since a fixed window is destroyed by an omitted field
    /// exactly as a tiling one is.
    #[test]
    fn a_windowed_schedule_is_repeated_verbatim() {
        for mode in ["since_last", "2h"] {
            let (window, lag) = preserved_window_and_lag(&schedule(Some(mode), Some(("5m", 300))));
            assert_eq!(window.as_deref(), Some(mode));
            assert_eq!(lag.as_deref(), Some("5m"));
        }
    }

    /// Zero is the lag the server applies to a windowed schedule with no
    /// field, so repeating it would be a request field that changes
    /// nothing. The window still travels.
    #[test]
    fn a_zero_lag_is_left_off_and_the_window_still_travels() {
        let (window, lag) =
            preserved_window_and_lag(&schedule(Some("since_last"), Some(("0s", 0))));
        assert_eq!(window.as_deref(), Some("since_last"));
        assert_eq!(lag, None);
    }

    /// Query mode has nothing to preserve, and a lag with no window is a
    /// 400, so neither half may leak out of a schedule that has no window.
    #[test]
    fn query_mode_carries_neither_half() {
        assert_eq!(
            preserved_window_and_lag(&schedule(None, None)),
            (None, None)
        );
        // Defensive: a response that somehow paired a lag with no window
        // must not turn into a request the server refuses.
        assert_eq!(
            preserved_window_and_lag(&schedule(None, Some(("5m", 300)))),
            (None, None)
        );
    }

    /// The line names exactly what the request carries, including the
    /// silence about a zero lag.
    #[test]
    fn the_summary_names_what_the_request_sends() {
        assert_eq!(
            window_summary(&schedule(Some("since_last"), Some(("5m", 300)))).as_deref(),
            Some("window since_last, lag 5m  (edit via TUI or API)")
        );
        assert_eq!(
            window_summary(&schedule(Some("2h"), Some(("0s", 0)))).as_deref(),
            Some("window 2h  (edit via TUI or API)")
        );
        assert_eq!(window_summary(&schedule(None, None)), None);
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
