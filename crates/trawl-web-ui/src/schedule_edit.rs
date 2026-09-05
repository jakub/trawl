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

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

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
}
