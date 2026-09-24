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
//! Run now lives here too: whether a net offers it, what the form says
//! it will read, and how its toast states the window the server claimed.
//! The browser never computes that window (ADR-0018 as amended on
//! 2026-09-23); it only prints the bounds the response carries.
//!
//! Pure + ungated so its tests run natively; the callers are
//! wasm32-only.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use trawl_api::{ReportRunSummary, SavedQueryResponse, ScheduleResponse};

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
    /// Incorporate a newer server seed without replacing dirty fields.
    pub fn refresh_clean_from(&mut self, old: &Self, new: Self) {
        if self.mode == old.mode {
            self.mode = new.mode;
        }
        if self.span == old.span {
            self.span = new.span;
        }
        if self.lag == old.lag {
            self.lag = new.lag;
        }
    }

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

/// How a net's schedule reads in a list: its cadence and what each run
/// covers, in one phrase.
///
/// The window half is the wire's own vocabulary said in words — an
/// absent `window` is query mode, `since_last` is tiling, and anything
/// else is the span the server parses. Nothing here parses a duration,
/// so a span is quoted rather than described.
#[must_use]
pub fn cadence_sentence(schedule: Option<&ScheduleResponse>) -> String {
    let Some(schedule) = schedule else {
        return "No schedule".to_owned();
    };
    let covers = match schedule.window.as_deref() {
        None => "query text".to_owned(),
        Some("since_last") => "since last run".to_owned(),
        Some(span) => format!("fixed span {span}"),
    };
    format!("Every {} · {covers}", schedule.interval)
}

/// The one label for firing a schedule by hand, in the Nets row and the
/// net drawer header alike.
pub const RUN_NOW: &str = "Run now";

/// The success toast's link to the started run.
pub const VIEW_RUN: &str = "View run";

/// The refusal toast's title. Its detail is the server's own message.
pub const RUN_NOT_STARTED: &str = "Run not started";

/// What Run now reads for a `since_last` schedule, said under the window
/// choice.
pub const RUN_NOW_SINCE_LAST_LINE: &str =
    "Run now reads from where the last successful run stopped.";

/// What Run now reads for a fixed-span schedule, said under the window
/// choice.
pub const RUN_NOW_FIXED_LINE: &str =
    "Run now reads the span ending now; results can overlap earlier runs.";

/// Whether a net offers Run now: exactly when its SAVED state has a
/// schedule, in any mode and enabled or paused. A net with no schedule
/// has nothing to fire, and the server refuses it with a 400, so the
/// control would not work (ADR-0025).
#[must_use]
pub fn run_now_offered(saved: &SavedQueryResponse) -> bool {
    saved.schedule.is_some()
}

/// The line the schedule form shows about Run now for `mode`, or `None`
/// in query mode, where a manual run reads the saved text as a
/// scheduled one would.
#[must_use]
pub const fn run_now_form_line(mode: WindowMode) -> Option<&'static str> {
    match mode {
        WindowMode::Query => None,
        WindowMode::SinceLast => Some(RUN_NOW_SINCE_LAST_LINE),
        WindowMode::Fixed => Some(RUN_NOW_FIXED_LINE),
    }
}

/// The success toast's title for a started manual run: the window the
/// server claimed, as it claimed it, or plain "Run started" when the run
/// has no window (query mode).
///
/// Bounds print in UTC as `HH:MM`. Bounds on different days carry their
/// dates, and bounds inside one minute carry seconds (inside one second,
/// microseconds), so the text never reads as a backwards or empty window. A bound the browser cannot read
/// is quoted verbatim.
#[must_use]
pub fn run_started_toast(run: &ReportRunSummary) -> String {
    let (Some(start), Some(end)) = (run.window_start.as_deref(), run.window_end.as_deref()) else {
        return "Run started".to_owned();
    };
    let parse = |text: &str| {
        chrono::DateTime::parse_from_rfc3339(text)
            .ok()
            .map(|t| t.with_timezone(&chrono::Utc))
    };
    let (Some(from), Some(to)) = (parse(start), parse(end)) else {
        return format!("Run started · covers {start}–{end}");
    };
    let clock = if from.date_naive() == to.date_naive() {
        // The coarsest clock that tells the two bounds apart.
        ["%H:%M", "%H:%M:%S", "%H:%M:%S%.6f"]
            .into_iter()
            .find(|clock| from.format(clock).to_string() != to.format(clock).to_string())
            .unwrap_or("%H:%M:%S%.6f")
    } else {
        "%Y-%m-%d %H:%M"
    };
    format!(
        "Run started · covers {}–{} UTC",
        from.format(clock),
        to.format(clock)
    )
}

/// Noon UTC, in seconds from midnight: the anchor the worked example
/// counts back from. A run time rather than a run: the example says what
/// a schedule would read, and midnight-relative arithmetic keeps it
/// inside one day for every duration it will print.
const EXAMPLE_END_SECS: u64 = 12 * 3600;

/// Every duration the example can print resolves to seconds, or it is
/// not printed. `5m 15m 1h 6h 24h 1w` are the form's own presets.
const PRESET_SECS: &[(&str, u64)] = &[
    ("5m", 300),
    ("15m", 900),
    ("1h", 3600),
    ("6h", 21_600),
    ("24h", 86_400),
    ("1w", 604_800),
];

/// What the form says when a duration it was given is one the browser
/// cannot read.
const CUSTOM_NOTE: &str = "The server validates custom durations when you save.";

/// The plain-language reading of a schedule edit: what it will do, an
/// optional worked example, and an optional note about what could not be
/// worked out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleSentence {
    /// One sentence naming the cadence and what each run covers.
    pub headline: String,
    /// A concrete pair of bounds, present only when every duration in
    /// the draft resolved to seconds.
    pub example: Option<String>,
    /// Why there is no example, when the reason is custom text.
    pub note: Option<&'static str>,
}

/// The seconds `text` names, or `None` when the browser cannot know.
///
/// A preset spells its own duration. Otherwise the one thing the browser
/// knows is what the server already parsed: a string byte-equal to a
/// duration the server reported seconds for has those seconds, because a
/// duration's value is a function of its text. Anything else is custom
/// text, and the browser has no duration grammar to guess with.
fn secs_of(text: &str, saved_text: Option<&str>, saved_secs: Option<u64>) -> Option<u64> {
    let text = text.trim();
    if let Some((_, secs)) = PRESET_SECS.iter().find(|(preset, _)| *preset == text) {
        return Some(*secs);
    }
    match (saved_text, saved_secs) {
        (Some(saved_text), Some(saved_secs)) if saved_text.trim() == text => Some(saved_secs),
        _ => None,
    }
}

/// `secs_of` against both pairs the server reports: the interval and its
/// seconds, the lag and its seconds.
fn resolve(text: &str, saved: Option<&ScheduleResponse>) -> Option<u64> {
    secs_of(
        text,
        saved.map(|s| s.interval.as_str()),
        saved.map(|s| s.interval_secs),
    )
    .or_else(|| {
        secs_of(
            text,
            saved.and_then(|s| s.lag.as_deref()),
            saved.and_then(|s| s.lag_secs),
        )
    })
}

/// The lag in seconds. Blank is none, which the server applies as zero.
fn resolve_lag(text: &str, saved: Option<&ScheduleResponse>) -> Option<u64> {
    if text.trim().is_empty() {
        return Some(0);
    }
    resolve(text, saved)
}

/// `HH:MM`, or `HH:MM:SS` when the time carries seconds.
fn clock(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if s == 0 {
        format!("{h:02}:{m:02}")
    } else {
        format!("{h:02}:{m:02}:{s:02}")
    }
}

/// Whether `text` was offered and could not be read.
fn unresolved(text: &str, secs: Option<u64>) -> bool {
    secs.is_none() && !text.trim().is_empty()
}

/// What the schedule form's callout says about the edit in the draft.
///
/// The example is computed, never canned: a fixed "11:00 → 12:00" beside
/// a 5m interval would advertise a window the schedule will not read
/// (ADR-0025). So it exists only when every duration in the draft
/// resolves to seconds, and custom text yields the validation note
/// instead.
#[must_use]
pub fn schedule_sentence(
    interval: &str,
    saved: Option<&ScheduleResponse>,
    draft: &WindowDraft,
) -> ScheduleSentence {
    let interval = interval.trim();
    if interval.is_empty() {
        return ScheduleSentence {
            headline: "Choose how often to run.".to_owned(),
            example: None,
            note: None,
        };
    }
    let lag_secs = resolve_lag(&draft.lag, saved);
    match draft.mode {
        WindowMode::Query => ScheduleSentence {
            headline: format!("Every {interval}, run the saved text as written."),
            example: Some("No schedule-imposed time limit.".to_owned()),
            note: None,
        },
        WindowMode::SinceLast => {
            let example = lag_secs.filter(|lag| *lag <= EXAMPLE_END_SECS).map(|lag| {
                format!(
                    "Example: from the last covered point → {} UTC exclusive",
                    clock(EXAMPLE_END_SECS - lag)
                )
            });
            let note =
                (example.is_none() && unresolved(&draft.lag, lag_secs)).then_some(CUSTOM_NOTE);
            ScheduleSentence {
                headline: format!("Every {interval}, continue from the last covered point."),
                example,
                note,
            }
        }
        WindowMode::Fixed => {
            let span = draft.span.trim();
            if span.is_empty() {
                return ScheduleSentence {
                    headline: format!("Every {interval}, read a trailing span."),
                    example: None,
                    note: None,
                };
            }
            let span_secs = resolve(span, saved);
            let example = span_secs
                .zip(lag_secs)
                .filter(|(span, lag)| span + lag <= EXAMPLE_END_SECS)
                .map(|(span, lag)| {
                    let end = EXAMPLE_END_SECS - lag;
                    format!(
                        "Example: {} UTC inclusive → {} UTC exclusive",
                        clock(end - span),
                        clock(end)
                    )
                });
            let note = (example.is_none()
                && (unresolved(span, span_secs) || unresolved(&draft.lag, lag_secs)))
            .then_some(CUSTOM_NOTE);
            ScheduleSentence {
                headline: format!("Every {interval}, read the previous {span}."),
                example,
                note,
            }
        }
    }
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

    /// A net as the server lists it, with or without a schedule.
    fn net(schedule: Option<ScheduleResponse>) -> SavedQueryResponse {
        SavedQueryResponse {
            id: 7,
            name: "errors".to_owned(),
            query: "level=error".to_owned(),
            created_at: "2026-03-14T02:00:00Z".to_owned(),
            updated_at: "2026-03-14T02:00:00Z".to_owned(),
            schedule,
        }
    }

    /// A manual run as `POST .../run` answers it, with the window the
    /// server claimed, if any.
    fn started(window: Option<(&str, &str)>) -> ReportRunSummary {
        ReportRunSummary {
            id: 41,
            query: "level=error".to_owned(),
            status: "running".to_owned(),
            started_at: "2026-09-24T14:26:00.000000Z".to_owned(),
            finished_at: None,
            duration_ms: None,
            row_count: None,
            error_message: None,
            result_path: None,
            window_start: window.map(|(start, _)| start.to_owned()),
            window_end: window.map(|(_, end)| end.to_owned()),
            window_truncated: window.map(|_| false),
            window_kind: window.map(|_| "since_last".to_owned()),
            origin: Some("manual".to_owned()),
        }
    }

    /// Run now is offered for every saved schedule, whatever its mode or
    /// enabled state, and never for a net with no schedule to fire.
    #[test]
    fn run_now_is_offered_exactly_when_a_schedule_is_saved() {
        assert!(!run_now_offered(&net(None)));
        for window in [None, Some("since_last"), Some("15m")] {
            assert!(run_now_offered(&net(Some(schedule(window, None)))));
        }
        let mut paused = schedule(Some("since_last"), None);
        paused.enabled = false;
        assert!(run_now_offered(&net(Some(paused))));
    }

    /// The toast states the bounds the server claimed, in UTC, to the
    /// minute when the minute tells them apart.
    #[test]
    fn run_started_toast_names_the_claimed_window() {
        assert_eq!(
            run_started_toast(&started(Some((
                "2026-09-24T14:05:00.000000Z",
                "2026-09-24T14:21:00.000000Z"
            )))),
            "Run started · covers 14:05–14:21 UTC"
        );
    }

    /// Query mode claims no window, so the toast claims none either.
    #[test]
    fn run_started_toast_in_query_mode_names_no_window() {
        assert_eq!(run_started_toast(&started(None)), "Run started");
    }

    /// Bounds on two days carry their dates, or 23:55–00:10 would read
    /// as a window running backwards.
    #[test]
    fn run_started_toast_dates_a_window_across_midnight() {
        assert_eq!(
            run_started_toast(&started(Some((
                "2026-09-23T23:55:00.000000Z",
                "2026-09-24T00:10:00.000000Z"
            )))),
            "Run started · covers 2026-09-23 23:55–2026-09-24 00:10 UTC"
        );
    }

    /// Two bounds inside one minute print their seconds, or the window
    /// would read as empty.
    #[test]
    fn run_started_toast_prints_seconds_when_bounds_share_a_minute() {
        assert_eq!(
            run_started_toast(&started(Some((
                "2026-09-24T14:05:10.250000Z",
                "2026-09-24T14:05:50.000000Z"
            )))),
            "Run started · covers 14:05:10–14:05:50 UTC"
        );
    }

    /// Bounds inside one second print the microseconds the wire carries.
    #[test]
    fn run_started_toast_prints_fractions_when_bounds_share_a_second() {
        assert_eq!(
            run_started_toast(&started(Some((
                "2026-09-24T14:05:10.250000Z",
                "2026-09-24T14:05:10.750000Z"
            )))),
            "Run started · covers 14:05:10.250000–14:05:10.750000 UTC"
        );
    }

    /// Bounds the browser cannot read are quoted as the server sent
    /// them rather than dropped: the window is still the server's claim.
    #[test]
    fn run_started_toast_quotes_bounds_it_cannot_read() {
        assert_eq!(
            run_started_toast(&started(Some(("yesterday", "today")))),
            "Run started · covers yesterday–today"
        );
    }

    /// One line per windowed mode, and none for query mode, which reads
    /// the saved text whenever it runs.
    #[test]
    fn run_now_form_line_speaks_only_for_windowed_modes() {
        assert_eq!(run_now_form_line(WindowMode::Query), None);
        assert_eq!(
            run_now_form_line(WindowMode::SinceLast),
            Some("Run now reads from where the last successful run stopped.")
        );
        assert_eq!(
            run_now_form_line(WindowMode::Fixed),
            Some("Run now reads the span ending now; results can overlap earlier runs.")
        );
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

    /// The list's one-line reading of a schedule: no schedule at all,
    /// and each of the three window shapes.
    #[test]
    fn cadence_sentence_reads_every_window_shape() {
        assert_eq!(cadence_sentence(None), "No schedule");
        assert_eq!(
            cadence_sentence(Some(&schedule(None, None))),
            "Every 1h · query text"
        );
        assert_eq!(
            cadence_sentence(Some(&schedule(Some("since_last"), None))),
            "Every 1h · since last run"
        );
        assert_eq!(
            cadence_sentence(Some(&schedule(Some("15m"), None))),
            "Every 1h · fixed span 15m"
        );
    }

    /// A draft in one of the three modes, with whatever span and lag
    /// text the case is about.
    fn draft(mode: WindowMode, span: &str, lag: &str) -> WindowDraft {
        WindowDraft {
            mode,
            span: span.to_owned(),
            lag: lag.to_owned(),
        }
    }

    /// Nothing to say about a cadence that has not been chosen.
    #[test]
    fn schedule_sentence_asks_for_an_interval_first() {
        let said = schedule_sentence("  ", None, &draft(WindowMode::Fixed, "1h", ""));
        assert_eq!(said.headline, "Choose how often to run.");
        assert_eq!(said.example, None);
        assert_eq!(said.note, None);
    }

    /// Query mode reads the saved text, so the "example" is the absence
    /// of a bound rather than a pair of timestamps.
    #[test]
    fn schedule_sentence_query_mode_names_no_bound() {
        let said = schedule_sentence("1h", None, &draft(WindowMode::Query, "", "5m"));
        assert_eq!(said.headline, "Every 1h, run the saved text as written.");
        assert_eq!(
            said.example.as_deref(),
            Some("No schedule-imposed time limit.")
        );
        assert_eq!(said.note, None);
    }

    /// Tiling has one bound to print: the run time, moved back by a lag
    /// the browser could read.
    #[test]
    fn schedule_sentence_since_last_prints_the_end_bound() {
        let said = schedule_sentence("1h", None, &draft(WindowMode::SinceLast, "", "5m"));
        assert_eq!(
            said.headline,
            "Every 1h, continue from the last covered point."
        );
        assert_eq!(
            said.example.as_deref(),
            Some("Example: from the last covered point → 11:55 UTC exclusive")
        );
        assert_eq!(said.note, None);
    }

    /// A lag the browser has no grammar for yields the note instead of a
    /// guess.
    #[test]
    fn schedule_sentence_since_last_custom_lag_notes_the_server() {
        let said = schedule_sentence("1h", None, &draft(WindowMode::SinceLast, "", "90s"));
        assert_eq!(said.example, None);
        assert_eq!(said.note, Some(CUSTOM_NOTE));
    }

    /// The common case, both bounds printed: a blank lag is none.
    #[test]
    fn schedule_sentence_fixed_prints_both_bounds() {
        let said = schedule_sentence("1h", None, &draft(WindowMode::Fixed, "1h", ""));
        assert_eq!(said.headline, "Every 1h, read the previous 1h.");
        assert_eq!(
            said.example.as_deref(),
            Some("Example: 11:00 UTC inclusive → 12:00 UTC exclusive")
        );
        assert_eq!(said.note, None);
    }

    /// A lag moves BOTH bounds back, which is the whole reason the
    /// example is computed rather than canned.
    #[test]
    fn schedule_sentence_fixed_lag_moves_both_bounds() {
        let said = schedule_sentence("15m", None, &draft(WindowMode::Fixed, "15m", "5m"));
        assert_eq!(
            said.example.as_deref(),
            Some("Example: 11:40 UTC inclusive → 11:55 UTC exclusive")
        );
    }

    /// Seconds print only when a bound carries them, and a duration the
    /// server already parsed is one the browser can read back.
    #[test]
    fn schedule_sentence_fixed_prints_seconds_when_a_bound_has_them() {
        let mut saved = schedule(Some("90s"), None);
        saved.interval = "90s".to_owned();
        saved.interval_secs = 90;
        let said = schedule_sentence("90s", Some(&saved), &draft(WindowMode::Fixed, "90s", ""));
        assert_eq!(said.headline, "Every 90s, read the previous 90s.");
        assert_eq!(
            said.example.as_deref(),
            Some("Example: 11:58:30 UTC inclusive → 12:00 UTC exclusive")
        );
    }

    /// A span not yet chosen is not a refusal, so the callout says what
    /// the mode means and stops there.
    #[test]
    fn schedule_sentence_fixed_blank_span_says_only_the_shape() {
        let said = schedule_sentence("1h", None, &draft(WindowMode::Fixed, "  ", "5m"));
        assert_eq!(said.headline, "Every 1h, read a trailing span.");
        assert_eq!(said.example, None);
        assert_eq!(said.note, None);
    }

    /// Custom span text the browser cannot read: the headline still
    /// quotes it, the example does not exist, the note says who decides.
    #[test]
    fn schedule_sentence_fixed_custom_span_notes_the_server() {
        let said = schedule_sentence("1h", None, &draft(WindowMode::Fixed, "45m", ""));
        assert_eq!(said.headline, "Every 1h, read the previous 45m.");
        assert_eq!(said.example, None);
        assert_eq!(said.note, Some(CUSTOM_NOTE));
    }

    /// A span that resolves but runs past the example's own day gets no
    /// example and no note: nothing failed to be read.
    #[test]
    fn schedule_sentence_fixed_week_span_has_no_worked_example() {
        let said = schedule_sentence("1w", None, &draft(WindowMode::Fixed, "1w", ""));
        assert_eq!(said.headline, "Every 1w, read the previous 1w.");
        assert_eq!(said.example, None);
        assert_eq!(said.note, None);
    }
}

#[cfg(test)]
mod live_draft_tests {
    use super::*;

    #[test]
    fn refresh_updates_clean_fields_and_keeps_dirty_fields_across_successive_reads() {
        let old = WindowDraft {
            mode: WindowMode::Fixed,
            span: "1h".into(),
            lag: "2m".into(),
        };
        let mut draft = old.clone();
        draft.span = "3h".into();
        let next = WindowDraft {
            mode: WindowMode::Fixed,
            span: "2h".into(),
            lag: "5m".into(),
        };
        draft.refresh_clean_from(&old, next.clone());
        assert_eq!(draft.span, "3h");
        assert_eq!(draft.lag, "5m");
        let latest = WindowDraft {
            lag: "7m".into(),
            ..next.clone()
        };
        draft.refresh_clean_from(&next, latest);
        assert_eq!(draft.span, "3h");
        assert_eq!(draft.lag, "7m");
    }
}
