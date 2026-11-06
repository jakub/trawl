// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pure-logic helpers for the histogram surfaces.
//!
//! Three independent jobs live here:
//!
//! - [`bucketize_series`] — the search-page strip. Splits an iterator of
//!   `(timestamp_seconds, is_error)` tuples into `n_buckets`
//!   evenly-spaced buckets between the observed min/max timestamp, and
//!   returns the bounds it used with them so the bars, the axis ticks
//!   ([`clock_ticks`]) and the accessible bucket table all read one
//!   geometry. Each bucket carries `(ok_count, err_count)`.
//! - [`align_buckets`] — the service drawer's ingest chart. Lays rows
//!   the server already aggregated (`timechart span=1h`) onto a fixed
//!   grid, so gaps in the data render as gaps instead of collapsing the
//!   chart to one bar per returned row.
//! - [`tip_shift`] — how far a strip bar's tooltip moves sideways so it
//!   stays inside the strip.
//!
//! The histogram component is intentionally tolerant: callers pass
//! whatever timestamps they can extract (parsed `_time` strings,
//! ingest receipt times, etc.). When no timestamps are available the
//! component renders a neutral placeholder rather than guessing.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bucket {
    pub ok: u32,
    pub err: u32,
}

/// Bucketed counts plus the time span they were laid out over.
///
/// `max_secs` is the span's end, not the largest observed timestamp:
/// for a page whose events all share one timestamp it is one second
/// past `min_secs`, the same widening bucket assignment uses. Every
/// surface that positions something on the strip — bar tooltips, the
/// axis ticks, the accessible bucket table — measures from these two
/// numbers, so none of them can disagree about where a bucket starts.
#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    pub buckets: Vec<Bucket>,
    pub min_secs: f64,
    pub max_secs: f64,
}

impl Series {
    /// Width of one bucket, in seconds.
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // bucket counts are small
    pub fn bucket_width(&self) -> f64 {
        (self.max_secs - self.min_secs) / self.buckets.len() as f64
    }
}

/// Lay `(timestamp_seconds, is_error)` events onto `n_buckets` even
/// buckets spanning the observed extent. `None` when there is nothing
/// to lay out.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)] // bucket arithmetic is bounded; precision loss past 2^52 events is acceptable
pub fn bucketize_series(events: &[(f64, bool)], n_buckets: usize) -> Option<Series> {
    if events.is_empty() || n_buckets == 0 {
        return None;
    }
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for &(t, _) in events {
        if t < min {
            min = t;
        }
        if t > max {
            max = t;
        }
    }
    // All same timestamp → degenerate range; widen by 1s so bucket
    // assignment doesn't divide by zero.
    let range = (max - min).max(1.0);
    let mut buckets = vec![Bucket { ok: 0, err: 0 }; n_buckets];
    let n = n_buckets as f64;
    for &(t, is_err) in events {
        let raw = (((t - min) / range) * n).floor();
        let idx = if raw <= 0.0 { 0 } else { raw as usize };
        let i = idx.min(n_buckets - 1);
        if is_err {
            buckets[i].err += 1;
        } else {
            buckets[i].ok += 1;
        }
    }
    Some(Series {
        buckets,
        min_secs: min,
        max_secs: min + range,
    })
}

/// One bucket bound as a UTC timestamp, rounded outward to the
/// millisecond: down for a start, up for an end, so a bucket's printed
/// interval always contains every event counted in it.
#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub fn bucket_time(seconds: f64, end: bool) -> String {
    let millis = seconds * 1000.0;
    let bound = if end { millis.ceil() } else { millis.floor() };
    chrono::DateTime::from_timestamp_millis(bound as i64).map_or_else(
        || seconds.to_string(),
        |dt| dt.format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
    )
}

/// Where an x-axis label sits against its tick: centred on it, or
/// starting or ending there when a centred label would cross the edge of
/// the bar box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickAlign {
    Center,
    Start,
    End,
}

/// One x-axis tick: a round UTC clock time inside the series' span.
#[derive(Debug, Clone, PartialEq)]
pub struct Tick {
    /// Position across the bar box, `0.0` at `min_secs`, `1.0` at
    /// `max_secs`.
    pub fraction: f64,
    pub label: String,
    pub align: TickAlign,
}

/// The most ticks the strip prints, however wide it is.
const MAX_TICKS: i64 = 6;
/// One character of an axis label, in pixels: Chivo Mono advances 0.6em,
/// and `.histo .xax` sets it at `--fs-tiny` (10px).
const LABEL_CHAR_PX: f64 = 6.0;
/// The least clear space between two neighbouring labels, in pixels.
const LABEL_GAP_PX: f64 = 12.0;
const DAY_MS: i64 = 86_400_000;
/// Tick steps below a day, in milliseconds. Each one divides the next
/// unit evenly, so every tick lands on a round clock time.
const STEPS_MS: [i64; 25] = [
    1, 2, 5, 10, 20, 50, 100, 200, 500, // sub-second
    1_000, 2_000, 5_000, 10_000, 15_000, 30_000, // seconds
    60_000, 120_000, 300_000, 600_000, 900_000, 1_800_000, // minutes
    3_600_000, 7_200_000, 21_600_000, 43_200_000, // hours
];

/// The strip's x-axis ticks for a bar box `width_px` wide: round UTC
/// clock times at their positions, never `-15m` / `now` over an observed
/// extent the picker's range may not describe (ADR-0027, amended
/// 2026-09-12). Every tick lies inside `[min_secs, max_secs]`.
///
/// The step is the smallest of 1, 2 and 5 ms up to 500 ms, then 1 s, 2 s,
/// 5 s, 10 s, 15 s, 30 s, 1 min, 2 min, 5 min, 10 min, 15 min, 30 min,
/// 1 h, 2 h, 6 h, 12 h, 1 d and whole multiples of days, that gives at
/// most six ticks whose labels fit the box without touching. A narrow
/// box therefore gets a coarser step, not overlapping labels. If even
/// two labels cannot fit, the coarsest step's first tick is printed
/// alone.
///
/// The first label carries the date, and a later one carries it only
/// where the day changes, so a same-day strip prints the date once. The
/// rest print `HH:MM`, `HH:MM:SS` below a one-minute step, and
/// `HH:MM:SS.mmm` below a one-second step. A day step prints dates only.
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
pub fn clock_ticks(series: &Series, width_px: f64) -> Vec<Tick> {
    let span = series.max_secs - series.min_secs;
    if !(span > 0.0 && width_px > 0.0) {
        return Vec::new();
    }
    let min_ms = (series.min_secs * 1000.0).ceil() as i64;
    let max_ms = (series.max_secs * 1000.0).floor() as i64;
    let day_steps = [1, 2, 5, 10, 20, 50, 100, 200, 500, 1000].map(|d| d * DAY_MS);
    let mut fallback: Option<Vec<Tick>> = None;
    for step in STEPS_MS.into_iter().chain(day_steps) {
        let first = min_ms.div_euclid(step) * step;
        let first = if first < min_ms { first + step } else { first };
        if first > max_ms {
            break;
        }
        let count = (max_ms - first) / step + 1;
        if count > MAX_TICKS {
            continue;
        }
        let ticks = place_ticks(series, width_px, first, step, count);
        if fits(&ticks, width_px) {
            return ticks;
        }
        fallback = Some(ticks);
    }
    fallback.map_or_else(Vec::new, |mut ticks| {
        ticks.truncate(1);
        ticks
    })
}

#[allow(clippy::cast_precision_loss)]
fn place_ticks(series: &Series, width_px: f64, first: i64, step: i64, count: i64) -> Vec<Tick> {
    let span = series.max_secs - series.min_secs;
    let mut prev_day: Option<String> = None;
    (0..count)
        .filter_map(|i| {
            let ms = first + step * i;
            let dt = chrono::DateTime::from_timestamp_millis(ms)?;
            let day = dt.format("%Y-%m-%d").to_string();
            let clock = if step >= DAY_MS {
                None
            } else if step >= 60_000 {
                Some(dt.format("%H:%M").to_string())
            } else if step >= 1_000 {
                Some(dt.format("%H:%M:%S").to_string())
            } else {
                Some(dt.format("%H:%M:%S%.3f").to_string())
            };
            let new_day = prev_day.as_deref() != Some(day.as_str());
            let label = match (clock, new_day) {
                (None, _) => day.clone(),
                (Some(clock), true) => format!("{day} {clock}"),
                (Some(clock), false) => clock,
            };
            prev_day = Some(day);
            let fraction = ((ms as f64 / 1000.0 - series.min_secs) / span).clamp(0.0, 1.0);
            let half = label_px(&label) / 2.0;
            let x = fraction * width_px;
            let align = if x - half < 0.0 {
                TickAlign::Start
            } else if x + half > width_px {
                TickAlign::End
            } else {
                TickAlign::Center
            };
            Some(Tick {
                fraction,
                label,
                align,
            })
        })
        .collect()
}

#[allow(clippy::cast_precision_loss)] // labels are a few dozen characters
fn label_px(label: &str) -> f64 {
    label.chars().count() as f64 * LABEL_CHAR_PX
}

/// The pixel span a tick's label covers in a box `width_px` wide.
fn label_box(tick: &Tick, width_px: f64) -> (f64, f64) {
    let x = tick.fraction * width_px;
    let w = label_px(&tick.label);
    match tick.align {
        TickAlign::Center => (x - w / 2.0, x + w / 2.0),
        TickAlign::Start => (x, x + w),
        TickAlign::End => (x - w, x),
    }
}

/// Every label inside the box, each clear of the next by the gap.
fn fits(ticks: &[Tick], width_px: f64) -> bool {
    let boxes: Vec<_> = ticks.iter().map(|t| label_box(t, width_px)).collect();
    boxes.iter().all(|&(l, r)| l >= 0.0 && r <= width_px)
        && boxes.windows(2).all(|w| w[0].1 + LABEL_GAP_PX <= w[1].0)
}

/// A bucket width ([`Series::bucket_width`]) in the one unit that keeps
/// it short: `"500ms"`, `"17.2s"`, `"≈4.4min"`, `"2h"`, `"≈1.5d"`.
///
/// The value keeps one decimal below 100 and none from 100 up. It is
/// marked `≈` when that rounding changed it. The unit is the smallest one
/// in which the rounded value stays under the next unit, so 59.97 s
/// reads `"≈1min"`, never `"60s"`. A width that rounds to zero
/// milliseconds reads `"<0.1ms"`, never `"0ms"`.
#[must_use]
pub fn bucket_width_label(seconds: f64) -> String {
    const UNITS: [(&str, f64, f64); 5] = [
        ("ms", 0.001, 1000.0),
        ("s", 1.0, 60.0),
        ("min", 60.0, 60.0),
        ("h", 3600.0, 24.0),
        ("d", 86_400.0, f64::INFINITY),
    ];
    for (unit, scale, next) in UNITS {
        let exact = seconds / scale;
        let shown = if exact < 100.0 {
            (exact * 10.0).round() / 10.0
        } else {
            exact.round()
        };
        if shown >= next {
            continue;
        }
        if shown <= 0.0 {
            return "<0.1ms".to_owned();
        }
        let approx = if (shown - exact).abs() > exact * 1e-9 {
            "≈"
        } else {
            ""
        };
        return if shown.fract() == 0.0 {
            format!("{approx}{shown:.0}{unit}")
        } else {
            format!("{approx}{shown:.1}{unit}")
        };
    }
    // Only a non-finite width gets here; Series never builds one.
    seconds.to_string()
}

/// One slot of a fixed-width timechart grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    /// Slot start, epoch milliseconds, aligned to a `slot_ms` boundary.
    pub start_ms: i64,
    pub count: u64,
}

/// Lay pre-aggregated `(bucket_start_ms, count)` rows onto a contiguous
/// grid of `n` slots of `slot_ms` each, ending at the slot holding the
/// newest row. Gaps come back as zero-count slots.
///
/// uPlot sizes bars from the spacing of the x values it is handed and
/// ends its axis just past the last one, so two populated hours out of
/// twenty-four arrive as two wide bars on an axis that stops at the
/// later of them. The zero slots are what make idle time read as idle.
///
/// Anchoring on the newest row rather than wall-clock now is
/// deliberate: `_time` arrives pre-shifted into trawld's configured
/// display timezone, which the browser doesn't know. Anchoring on the
/// data keeps every comparison inside one timezone, at the cost of the
/// grid ending at the last observed bucket instead of the current one.
///
/// Rows falling outside the window are dropped; rows sharing a slot are
/// summed, so unaligned input still lands somewhere sensible.
#[must_use]
pub fn align_buckets(rows: &[(i64, u64)], slot_ms: i64, n: usize) -> Vec<Slot> {
    if rows.is_empty() || n == 0 || slot_ms <= 0 {
        return Vec::new();
    }
    let floor = |t: i64| t.div_euclid(slot_ms) * slot_ms;

    let newest = floor(rows.iter().map(|&(t, _)| t).max().unwrap_or(0));
    // `n - 1` slots of history plus the newest one. Bail rather than wrap
    // on absurd inputs — a saturated grid would silently mis-place bars.
    let Some(first) = i64::try_from(n - 1)
        .ok()
        .and_then(|back| back.checked_mul(slot_ms))
        .and_then(|span| newest.checked_sub(span))
    else {
        return Vec::new();
    };

    let mut out: Vec<Slot> = Vec::with_capacity(n);
    let mut start = first;
    for _ in 0..n {
        out.push(Slot {
            start_ms: start,
            count: 0,
        });
        start = start.saturating_add(slot_ms);
    }

    for &(t, count) in rows {
        let Some(offset) = floor(t).checked_sub(first) else {
            continue;
        };
        if let Ok(idx) = usize::try_from(offset / slot_ms)
            && let Some(slot) = out.get_mut(idx)
        {
            slot.count = slot.count.saturating_add(count);
        }
    }
    out
}

/// Parse a trawl timestamp into epoch milliseconds, treating it as
/// naive wall-clock (no timezone shift applied).
///
/// Accepts the result wire's RFC 3339 `T`-separated form and the
/// space-separated `YYYY-MM-DD HH:MM:SS[.fff]` cast text; any trailing
/// zone designator or fractional part is ignored, since the grid
/// resolution is whole buckets. Returns `None` on anything it can't read, which the caller
/// treats as "fall back to unpositioned bars".
///
/// Deliberately lenient, unlike `series::parse_instant_secs`: the ingest
/// grid only needs the hour a row belongs to, so a zone or a fraction
/// can be dropped, whereas the Visualization chart must place a point
/// exactly and refuses any form ADR-0038 does not admit.
#[must_use]
pub fn parse_bucket_ms(s: &str) -> Option<i64> {
    let (date, rest) = s.trim().split_once(['T', ' '])?;

    let mut dp = date.split('-');
    let y: i64 = dp.next()?.parse().ok()?;
    let mo: i64 = dp.next()?.parse().ok()?;
    let d: i64 = dp.next()?.parse().ok()?;
    if dp.next().is_some() || !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }

    // Keep the leading HH[:MM[:SS]]; drop fraction and zone.
    let time: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == ':')
        .collect();
    let mut tp = time.split(':');
    let h: i64 = tp.next()?.parse().ok()?;
    let mi: i64 = tp.next().map_or(Ok(0), str::parse).ok()?;
    let sec: i64 = tp.next().map_or(Ok(0), str::parse).ok()?;
    if h > 23 || mi > 59 || sec > 60 {
        return None;
    }

    let days = days_from_civil(y, mo, d);
    Some((days * 86_400 + h * 3_600 + mi * 60 + sec) * 1_000)
}

/// How far a bar's tooltip moves sideways, in pixels, to lie inside the
/// strip.
///
/// The tip is centred on its bar: its natural left edge is
/// `bar_left + bar_width / 2 - tip_width / 2`, with every position
/// measured from the strip's left edge. The shift moves that edge into
/// `0..=strip_width - tip_width` and is zero when it is already there.
/// A tip wider than the strip cannot fit either way; it aligns to the
/// strip's left edge, so its start (the bucket's start time) stays
/// readable. The component caps a tip's width at the strip's and lets it
/// wrap, so in practice this case does not arise; the fallback keeps the
/// function total.
///
/// A measured clamp, not a rule for "the first or last few bars": the tip
/// width follows its label and the bar width follows the bucket count
/// and the viewport, so no fixed number of edge bars is right.
#[must_use]
pub fn tip_shift(bar_left: i32, bar_width: i32, tip_width: i32, strip_width: i32) -> i32 {
    let natural = bar_left + bar_width / 2 - tip_width / 2;
    let clamped = natural.min(strip_width - tip_width).max(0);
    clamped - natural
}

/// Days since the Unix epoch for a proleptic-Gregorian date.
/// Hinnant's `days_from_civil`.
///
/// Shared with the strict `_time` parser in `series::parse_instant_secs`,
/// which validates the date before calling this and refuses what
/// [`parse_bucket_ms`] above tolerates.
pub(crate) fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buckets(events: &[(f64, bool)], n: usize) -> Vec<Bucket> {
        bucketize_series(events, n).map_or_else(Vec::new, |s| s.buckets)
    }

    #[test]
    fn tip_shift_leaves_a_tip_that_fits_centred() {
        // Bar 500..520 centres a 100px tip at 460..560, inside 0..1000.
        assert_eq!(tip_shift(500, 20, 100, 1000), 0);
    }

    #[test]
    fn tip_shift_pushes_a_first_bar_tip_right_to_the_strip_edge() {
        // Natural left is 40 + 10 - 150 = -100; the tip moves 100px right.
        assert_eq!(tip_shift(40, 20, 300, 1000), 100);
    }

    #[test]
    fn tip_shift_pulls_a_last_bar_tip_left_to_the_strip_edge() {
        // Natural left is 966 + 10 - 150 = 826, so the right edge would be
        // 1126; the tip moves left until it ends at 1000.
        let shift = tip_shift(966, 20, 300, 1000);
        assert_eq!(shift, -126);
        assert_eq!(826 + shift + 300, 1000);
    }

    #[test]
    fn tip_shift_aligns_a_tip_wider_than_the_strip_to_its_left_edge() {
        // 500px of tip in a 400px strip: left edge at 0, wherever the bar is.
        for bar_left in [0, 190, 380] {
            let natural = bar_left + 10 - 250;
            assert_eq!(natural + tip_shift(bar_left, 20, 500, 400), 0, "{bar_left}");
        }
    }

    #[test]
    fn tip_shift_touching_either_edge_is_inside() {
        // Exactly at 0 and exactly ending at the strip width: no move.
        assert_eq!(tip_shift(40, 20, 100, 1000), 0);
        assert_eq!(tip_shift(940, 20, 100, 1000), 0);
    }

    #[test]
    fn empty_input_returns_no_series() {
        assert!(bucketize_series(&[], 10).is_none());
    }

    #[test]
    fn zero_buckets_returns_no_series() {
        let events = [(1.0, false), (2.0, true)];
        assert!(bucketize_series(&events, 0).is_none());
    }

    #[test]
    fn single_event_lands_in_first_bucket() {
        let events = [(5.0, false)];
        let out = buckets(&events, 4);
        assert_eq!(out.len(), 4);
        assert_eq!(out[0], Bucket { ok: 1, err: 0 });
        assert_eq!(out[1], Bucket { ok: 0, err: 0 });
    }

    #[test]
    fn distributes_across_buckets() {
        let events = [(0.0, false), (1.0, true), (2.0, false), (3.0, true)];
        let out = buckets(&events, 4);
        assert_eq!(out.len(), 4);
        let total_ok: u32 = out.iter().map(|b| b.ok).sum();
        let total_err: u32 = out.iter().map(|b| b.err).sum();
        assert_eq!(total_ok, 2);
        assert_eq!(total_err, 2);
    }

    #[test]
    fn last_event_lands_in_last_bucket() {
        let events = [(0.0, false), (10.0, true)];
        let out = buckets(&events, 5);
        assert_eq!(out[0].ok, 1);
        assert_eq!(out[4].err, 1);
    }

    #[test]
    fn series_bounds_are_the_observed_extent() {
        let series = bucketize_series(&[(0.0, false), (10.0, true)], 5).expect("series");
        assert!((series.min_secs - 0.0).abs() < f64::EPSILON);
        assert!((series.max_secs - 10.0).abs() < f64::EPSILON);
        assert!((series.bucket_width() - 2.0).abs() < f64::EPSILON);
    }

    /// One timestamp on every row is still a page worth drawing: the
    /// span widens by a second rather than collapsing to zero width.
    #[test]
    fn identical_timestamps_widen_to_one_second() {
        let series = bucketize_series(&[(100.0, false), (100.0, true)], 4).expect("series");
        assert!((series.max_secs - series.min_secs - 1.0).abs() < f64::EPSILON);
        assert_eq!(series.buckets[0], Bucket { ok: 1, err: 1 });
    }

    fn span(min_secs: f64, max_secs: f64) -> Series {
        Series {
            buckets: vec![Bucket { ok: 1, err: 0 }; 48],
            min_secs,
            max_secs,
        }
    }

    fn labels(ticks: &[Tick]) -> Vec<&str> {
        ticks.iter().map(|t| t.label.as_str()).collect()
    }

    /// 2026-09-01T00:00:00Z.
    const DAY0: f64 = 1_788_220_800.0;

    /// The capture fixture's shape: 05:57:01.565 .. 06:10:46.706. Two
    /// minutes would print seven ticks, so the step is five.
    #[test]
    fn clock_ticks_label_round_minutes_with_the_date_once() {
        let min = DAY0 + 5.0 * 3600.0 + 57.0 * 60.0 + 1.565;
        let series = span(min, DAY0 + 6.0 * 3600.0 + 10.0 * 60.0 + 46.706);
        let ticks = clock_ticks(&series, 1100.0);
        assert_eq!(labels(&ticks), ["2026-09-01 06:00", "06:05", "06:10"]);
        // Each tick sits at its own clock time across the span.
        let at = |secs: f64| (secs - min) / (series.max_secs - min);
        let expected = [6.0 * 3600.0, 6.0 * 3600.0 + 300.0, 6.0 * 3600.0 + 600.0];
        for (tick, secs) in ticks.iter().zip(expected) {
            assert!((tick.fraction - at(DAY0 + secs)).abs() < 1e-9);
        }
        assert!(ticks.iter().all(|t| t.align == TickAlign::Center));
    }

    #[test]
    fn clock_ticks_stay_inside_the_observed_extent() {
        for (min, max) in [
            (DAY0 + 0.4, DAY0 + 1.4),
            (DAY0 + 17.0, DAY0 + 3_617.0),
            (DAY0 - 86_400.0 * 3.3, DAY0),
            (DAY0 + 59.9, DAY0 + 60.1),
        ] {
            for width in [120.0, 450.0, 1100.0] {
                let ticks = clock_ticks(&span(min, max), width);
                assert!(!ticks.is_empty(), "{min}..{max} at {width}px");
                assert!(ticks.len() <= 6);
                assert!(ticks.iter().all(|t| (0.0..=1.0).contains(&t.fraction)));
                assert!(ticks.windows(2).all(|w| w[0].fraction < w[1].fraction));
            }
        }
    }

    /// The step is the smallest that fits: a one-hour span prints every
    /// 10 minutes on a wide strip and every 30 on a 200px one, where the
    /// dated first label would touch a 10- or 15-minute neighbour.
    #[test]
    fn clock_ticks_coarsen_the_step_to_fit_a_narrow_strip() {
        let series = span(DAY0 + 17.0, DAY0 + 3_617.0);
        assert_eq!(
            labels(&clock_ticks(&series, 1100.0)),
            [
                "2026-09-01 00:10",
                "00:20",
                "00:30",
                "00:40",
                "00:50",
                "01:00"
            ]
        );
        let narrow = clock_ticks(&series, 200.0);
        assert_eq!(labels(&narrow), ["2026-09-01 00:30", "01:00"]);
        assert!(fits(&narrow, 200.0));
    }

    #[test]
    fn clock_ticks_never_overlap_or_leave_the_box() {
        let series = span(
            DAY0 + 5.0 * 3600.0 + 57.0 * 60.0,
            DAY0 + 6.0 * 3600.0 + 11.0 * 60.0,
        );
        for width in [60.0, 150.0, 300.0, 450.0, 680.0, 1100.0, 1600.0] {
            let ticks = clock_ticks(&series, width);
            assert!(!ticks.is_empty());
            if ticks.len() > 1 {
                assert!(fits(&ticks, width), "{width}px: {:?}", labels(&ticks));
            }
        }
    }

    /// A label whose centre would cross the box edge starts or ends at
    /// its tick instead.
    #[test]
    fn clock_ticks_pin_edge_labels_to_their_tick() {
        // 00:00:00 is the very start and 00:00:10 the very end. A
        // two-second step would put 00:00:02 under the dated first label.
        let ticks = clock_ticks(&span(DAY0, DAY0 + 10.0), 600.0);
        assert_eq!(
            labels(&ticks),
            ["2026-09-01 00:00:00", "00:00:05", "00:00:10"]
        );
        let aligns: Vec<_> = ticks.iter().map(|t| t.align).collect();
        assert_eq!(
            aligns,
            [TickAlign::Start, TickAlign::Center, TickAlign::End]
        );
    }

    #[test]
    fn clock_ticks_keep_the_date_where_the_day_changes() {
        // 2026-09-01T21:10Z .. 09-02T02:50Z: hourly ticks cross midnight.
        let series = span(DAY0 + 21.0 * 3600.0 + 600.0, DAY0 + 26.0 * 3600.0 + 3000.0);
        assert_eq!(
            labels(&clock_ticks(&series, 1100.0)),
            [
                "2026-09-01 22:00",
                "23:00",
                "2026-09-02 00:00",
                "01:00",
                "02:00"
            ]
        );
    }

    #[test]
    fn clock_ticks_print_seconds_below_a_minute_step() {
        let series = span(DAY0 + 3.2, DAY0 + 47.9);
        assert_eq!(
            labels(&clock_ticks(&series, 1100.0)),
            ["2026-09-01 00:00:10", "00:00:20", "00:00:30", "00:00:40"]
        );
    }

    /// The narrowest strip `bucketize_series` builds is one second wide.
    #[test]
    fn clock_ticks_print_milliseconds_below_a_second_step() {
        let series = bucketize_series(&[(DAY0 + 0.25, false)], 4).expect("s");
        assert_eq!(
            labels(&clock_ticks(&series, 1100.0)),
            [
                "2026-09-01 00:00:00.400",
                "00:00:00.600",
                "00:00:00.800",
                "00:00:01.000",
                "00:00:01.200"
            ]
        );
    }

    #[test]
    fn clock_ticks_print_dates_alone_at_a_day_step() {
        let series = span(DAY0 - 86_400.0 * 3.3, DAY0 + 600.0);
        assert_eq!(
            labels(&clock_ticks(&series, 1100.0)),
            ["2026-08-29", "2026-08-30", "2026-08-31", "2026-09-01"]
        );
    }

    /// Too narrow for two labels: one tick alone, never none.
    #[test]
    fn clock_ticks_fall_back_to_one_label() {
        let ticks = clock_ticks(&span(DAY0 + 17.0, DAY0 + 3_617.0), 60.0);
        assert_eq!(labels(&ticks), ["2026-09-01 01:00"]);
    }

    #[test]
    fn clock_ticks_need_a_span_and_a_width() {
        assert!(clock_ticks(&span(DAY0, DAY0), 600.0).is_empty());
        assert!(clock_ticks(&span(DAY0, DAY0 + 60.0), 0.0).is_empty());
    }

    #[test]
    fn bucket_time_rounds_outward_to_milliseconds() {
        assert_eq!(
            bucket_time(1_788_220_800.000_4, false),
            "2026-09-01 00:00:00.000"
        );
        assert_eq!(
            bucket_time(1_788_220_800.000_4, true),
            "2026-09-01 00:00:00.001"
        );
    }

    // ── align_buckets ──────────────────────────────────────────────

    const HOUR: i64 = 3_600_000;

    #[test]
    fn bucket_width_label_keeps_an_exact_width_unmarked() {
        assert_eq!(bucket_width_label(0.5), "500ms");
        assert_eq!(bucket_width_label(1.0), "1s");
        assert_eq!(bucket_width_label(825.6 / 48.0), "17.2s");
        assert_eq!(bucket_width_label(7200.0), "2h");
    }

    #[test]
    fn bucket_width_label_marks_a_rounded_width() {
        assert_eq!(bucket_width_label(1.0 / 48.0), "≈20.8ms");
        assert_eq!(bucket_width_label(900.0 / 48.0), "≈18.8s");
        assert_eq!(bucket_width_label(15.0 * 60.0 / 47.0), "≈19.1s");
        assert_eq!(bucket_width_label(3.0 * 86_400.0 / 48.0), "1.5h");
        assert_eq!(bucket_width_label(30.0 * 86_400.0 / 48.0), "15h");
        assert_eq!(bucket_width_label(365.0 * 86_400.0 / 48.0), "≈7.6d");
    }

    #[test]
    fn bucket_width_label_steps_up_when_rounding_reaches_the_next_unit() {
        assert_eq!(bucket_width_label(59.97), "≈1min");
        assert_eq!(bucket_width_label(0.999_96), "≈1s");
        assert_eq!(bucket_width_label(23.99 * 3600.0), "≈1d");
    }

    #[test]
    fn bucket_width_label_never_prints_zero() {
        assert_eq!(bucket_width_label(0.000_01), "<0.1ms");
        assert_eq!(bucket_width_label(0.0), "<0.1ms");
        assert_eq!(bucket_width_label(0.000_1), "0.1ms");
    }

    #[test]
    fn bucket_width_label_drops_decimals_from_100_up() {
        assert_eq!(bucket_width_label(0.1234), "≈123ms");
        assert_eq!(bucket_width_label(150.0 * 86_400.0), "150d");
    }

    #[test]
    fn align_rejects_degenerate_input() {
        assert!(align_buckets(&[], HOUR, 24).is_empty());
        assert!(align_buckets(&[(0, 1)], HOUR, 0).is_empty());
        assert!(align_buckets(&[(0, 1)], 0, 24).is_empty());
        assert!(align_buckets(&[(0, 1)], -HOUR, 24).is_empty());
    }

    #[test]
    fn align_always_returns_a_full_grid() {
        // The reason this helper exists: two populated hours must render
        // as 24 slots, not 2 bars.
        let rows = [(100 * HOUR, 5), (103 * HOUR, 7)];
        let out = align_buckets(&rows, HOUR, 24);
        assert_eq!(out.len(), 24);
        assert_eq!(out.iter().map(|s| s.count).sum::<u64>(), 12);
        // Newest row anchors the last slot.
        assert_eq!(out[23].start_ms, 103 * HOUR);
        assert_eq!(out[23].count, 7);
        assert_eq!(out[20].count, 5);
        assert_eq!(out[21].count, 0);
    }

    #[test]
    fn align_slots_are_contiguous_and_ascending() {
        let out = align_buckets(&[(50 * HOUR, 1)], HOUR, 6);
        assert_eq!(out.len(), 6);
        for pair in out.windows(2) {
            assert_eq!(pair[1].start_ms - pair[0].start_ms, HOUR);
        }
    }

    #[test]
    fn align_drops_rows_older_than_the_window() {
        let rows = [(0, 99), (10 * HOUR, 3)];
        let out = align_buckets(&rows, HOUR, 4);
        assert_eq!(out.iter().map(|s| s.count).sum::<u64>(), 3);
    }

    #[test]
    fn align_floors_unaligned_rows_and_sums_collisions() {
        // Two rows inside the same hour fold into one slot.
        let rows = [(10 * HOUR + 60_000, 2), (10 * HOUR + 120_000, 3)];
        let out = align_buckets(&rows, HOUR, 2);
        assert_eq!(out[1].start_ms, 10 * HOUR);
        assert_eq!(out[1].count, 5);
    }

    // ── parse_bucket_ms ────────────────────────────────────────────

    #[test]
    fn parses_the_executor_display_format() {
        assert_eq!(parse_bucket_ms("1970-01-01 00:00:00"), Some(0));
        assert_eq!(parse_bucket_ms("1970-01-02 00:00:00"), Some(86_400_000));
        assert_eq!(parse_bucket_ms("1970-01-01 01:30:00"), Some(5_400_000));
    }

    #[test]
    fn parses_rfc3339_and_ignores_fraction_and_zone() {
        let plain = parse_bucket_ms("2026-07-28 13:00:00");
        assert_eq!(parse_bucket_ms("2026-07-28T13:00:00Z"), plain);
        assert_eq!(parse_bucket_ms("2026-07-28T13:00:00.123456Z"), plain);
        assert_eq!(parse_bucket_ms("2026-07-28T13:00:00+02:00"), plain);
    }

    #[test]
    fn parses_a_known_instant() {
        // 2026-07-28T00:00:00Z — cross-checked against the civil-days
        // formula's reference epoch.
        assert_eq!(
            parse_bucket_ms("2026-07-28 00:00:00"),
            Some(1_785_196_800_000)
        );
    }

    #[test]
    fn rejects_unparseable_timestamps() {
        for bad in [
            "",
            "not a timestamp",
            "2026-07-28",
            "2026-13-01 00:00:00",
            "2026-07-32 00:00:00",
            "2026-07-28 24:00:00",
            "2026-07-28 00:99:00",
            "2026-07-28-01 00:00:00",
        ] {
            assert_eq!(parse_bucket_ms(bad), None, "should reject {bad:?}");
        }
    }

    #[test]
    fn hours_within_a_day_are_distinct_and_ordered() {
        // The chart plots these on a real time axis, so intra-day
        // offsets have to be exact, not merely parseable.
        let midnight = parse_bucket_ms("2026-07-28 00:00:00").unwrap();
        let one_pm = parse_bucket_ms("2026-07-28 13:00:00").unwrap();
        let last_minute = parse_bucket_ms("2026-07-28 23:59:00").unwrap();
        assert_eq!(one_pm - midnight, 13 * 3_600_000);
        assert_eq!(last_minute - midnight, 23 * 3_600_000 + 59 * 60_000);
    }
}
