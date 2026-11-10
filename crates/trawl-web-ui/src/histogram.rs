// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pure-logic helpers for the histogram surfaces.
//!
//! Three independent jobs live here:
//!
//! - The search-page strip. Its input is the whole-result count trawld
//!   answers beside the page ([`EventHistogram`]): 48 half-open bins in
//!   integer microseconds, from the first usable `_time` of the result to
//!   past its last. The browser never buckets rows itself. [`strip`]
//!   decides what the strip shows for a settled response, [`Series`]
//!   carries a validated count with its integer bounds, and the copy
//!   functions ([`caption`], [`reason_copy`], [`bar_tip`] and the key
//!   constants) build every string it prints. Counts stay `u64` from the
//!   wire to the tooltip and the scale ([`scale_top`]); only the CSS bar
//!   heights ([`pct`]) and the axis layout ([`clock_ticks`]) use `f64`.
//! - [`align_buckets`] — the service drawer's ingest chart. Lays rows
//!   the server already aggregated (`timechart span=1h`) onto a fixed
//!   grid, so gaps in the data render as gaps instead of collapsing the
//!   chart to one bar per returned row.
//! - [`tip_shift`] — how far a strip bar's tooltip moves sideways so it
//!   stays inside the strip.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use trawl_api::{EventHistogram, HistogramReason, zone};

use crate::service_card_fmt::format_exact;

/// One bin of the strip: its events that are not errors, and its errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bucket {
    pub ok: u64,
    pub err: u64,
}

impl Bucket {
    /// Every event in the bin. `ok` is `events - errors` of a validated
    /// bin, so the sum is the wire's `events` and cannot overflow.
    #[must_use]
    pub fn events(self) -> u64 {
        self.ok + self.err
    }
}

/// A validated whole-result count, laid out over the bounds trawld chose.
///
/// Bin `i` is `[start_micros + i × width_micros, start_micros + (i + 1) ×
/// width_micros)`. Every surface that positions something on the strip
/// (bar tooltips, the axis ticks) measures from these two integers, so
/// none of them can disagree about where a bucket starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Series {
    pub buckets: Vec<Bucket>,
    pub start_micros: i64,
    pub width_micros: i64,
}

impl Series {
    /// The strip for a ready histogram that [`EventHistogram::consistent_with`]
    /// accepts against the page's `total`. `None` for an unavailable one,
    /// and for a ready one that breaks an invariant.
    #[must_use]
    pub fn from_ready(histogram: &EventHistogram, total: usize) -> Option<Self> {
        let EventHistogram::Ready {
            start,
            end,
            buckets,
            ..
        } = histogram
        else {
            return None;
        };
        if !histogram.consistent_with(total) {
            return None;
        }
        let start = zone::wire_micros(start)?;
        let end = zone::wire_micros(end)?;
        let bins = i64::try_from(buckets.len()).ok()?;
        // Checked by `consistent_with`: finite bounds, start < end and a
        // span the bin count divides. The span can pass i64, the width not.
        let width = i64::try_from((i128::from(end) - i128::from(start)) / i128::from(bins)).ok()?;
        let buckets = buckets
            .iter()
            .map(|b| {
                Some(Bucket {
                    ok: b.events.checked_sub(b.errors)?,
                    err: b.errors,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            buckets,
            start_micros: start,
            width_micros: width,
        })
    }

    /// Bound `i` in microseconds: bucket `i`'s start, or for `i == len`
    /// the series' end. Every bound of a validated series is finite, so
    /// it fits i64.
    #[must_use]
    pub fn bound_micros(&self, i: usize) -> i64 {
        let i = i128::try_from(i).unwrap_or(i128::MAX);
        let bound = i128::from(self.start_micros) + i * i128::from(self.width_micros);
        i64::try_from(bound).unwrap_or(i64::MAX)
    }

    /// The series' start, in seconds, for the axis layout.
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // layout only; labels read the integers
    pub fn min_secs(&self) -> f64 {
        self.start_micros as f64 / 1e6
    }

    /// The series' end, in seconds, for the axis layout.
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // layout only; labels read the integers
    pub fn max_secs(&self) -> f64 {
        self.bound_micros(self.buckets.len()) as f64 / 1e6
    }

    /// The tallest bar, never below one.
    #[must_use]
    pub fn tallest(&self) -> u64 {
        self.buckets
            .iter()
            .map(|b| b.events())
            .max()
            .unwrap_or(1)
            .max(1)
    }

    /// Bucket `i`'s tooltip: its printed interval and its counts.
    #[must_use]
    pub fn tip(&self, i: usize) -> String {
        let b = self
            .buckets
            .get(i)
            .copied()
            .unwrap_or(Bucket { ok: 0, err: 0 });
        bar_tip(
            &bucket_time(self.bound_micros(i), false),
            &bucket_time(self.bound_micros(i + 1), true),
            b.events(),
            b.err,
        )
    }
}

/// What the strip shows for a settled, successful page response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Strip {
    /// The result is empty: the strip collapses, and the table below says
    /// the page is empty.
    Collapsed,
    /// The quiet `—`: no histogram came back, or a ready one broke an
    /// invariant.
    Dash,
    /// Why trawld did not count the result.
    Reason(&'static str),
    /// Bars.
    Drawn(Drawn),
}

/// A strip with bars, and the words around them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drawn {
    pub series: Series,
    pub caption: String,
    /// The result has a `_severity` column, so the key names both colours.
    pub severity: bool,
    /// The line under the key, when the query kept only some events.
    pub kept_some: Option<&'static str>,
}

/// What the strip shows for a settled, successful response whose
/// execution produced `total` rows. A pending or failed request never
/// gets here: the component shows the loading hint or the dash first.
///
/// In order: an empty result collapses the strip, whatever reason trawld
/// gave; no histogram is the dash; an unavailable one shows its reason; a
/// ready one that breaks an invariant is the dash; anything else draws.
#[must_use]
pub fn strip(total: usize, histogram: Option<&EventHistogram>) -> Strip {
    if total == 0 {
        return Strip::Collapsed;
    }
    match histogram {
        None => Strip::Dash,
        Some(EventHistogram::Unavailable { reason }) => Strip::Reason(reason_copy(*reason)),
        Some(
            ready @ EventHistogram::Ready {
                undated,
                severity,
                kept_some,
                ..
            },
        ) => match Series::from_ready(ready, total) {
            None => Strip::Dash,
            Some(series) => Strip::Drawn(Drawn {
                caption: caption(
                    u64::try_from(total).unwrap_or(u64::MAX),
                    *undated,
                    series.width_micros,
                ),
                series,
                severity: *severity,
                kept_some: kept_some.then_some(KEPT_SOME_LINE),
            }),
        },
    }
}

/// The swatch for an event that is not an error.
pub const OTHER_KEY: &str = "Other";
/// The swatch for an event at or above the error band.
pub const ERROR_KEY: &str = "Error or fatal";
/// The one swatch of a result with no `_severity` column.
pub const NO_SEVERITY_KEY: &str = "Events (no severity in this result)";
/// The line under the key when a `head`, `limit`, `tail` or `dedup`
/// stage kept only some events.
pub const KEPT_SOME_LINE: &str = "Only the events your query kept are counted.";

/// The caption over the bars: what they count, how wide one bin is, and
/// that the bins are UTC. `total` is every row of the result, `undated`
/// the rows without a usable time, and `width_micros` one bin's width.
#[must_use]
pub fn caption(total: u64, undated: u64, width_micros: i64) -> String {
    #[allow(clippy::cast_precision_loss)] // the label rounds to one decimal
    let width = bucket_width_label(width_micros as f64 / 1e6);
    let counted = if total == 1 {
        "1 event in this result".to_owned()
    } else {
        format!("All {} events in this result", format_exact(total))
    };
    let undated = if undated > 0 {
        format!(
            " · {} without a usable time, not drawn",
            format_exact(undated)
        )
    } else {
        String::new()
    };
    format!("{counted}{undated} · first to last event · {width} per bucket · UTC")
}

/// Why trawld did not count a result, in the strip's words.
#[must_use]
pub fn reason_copy(reason: HistogramReason) -> &'static str {
    match reason {
        HistogramReason::Sample => {
            "No histogram for a sampled result. Remove sample to count every event."
        }
        HistogramReason::StoredResult => "No histogram for rows read from a net's run.",
        HistogramReason::NoTimeColumn => "No histogram: this result has no _time timestamp.",
        HistogramReason::NoUsableTime => "No histogram: no event in this result has a usable time.",
    }
}

/// A bar's tooltip: `{start} – {end} · {n} events`, with ` · {e} errors`
/// when the bin holds any.
#[must_use]
pub fn bar_tip(start: &str, end: &str, events: u64, errors: u64) -> String {
    let errors = if errors > 0 {
        format!(" · {} errors", format_exact(errors))
    } else {
        String::new()
    };
    format!("{start} – {end} · {} events{errors}", format_exact(events))
}

/// The y scale's top: the tallest bar rounded up to an even count, so the
/// label on the midpoint gridline is a whole number of events. A tallest
/// bar that cannot round up without overflowing is the top itself.
#[must_use]
pub fn scale_top(tallest: u64) -> u64 {
    if tallest <= 1 {
        1
    } else {
        tallest.checked_add(tallest % 2).unwrap_or(tallest)
    }
}

/// A bar's CSS height, in percent of the scale's `top`.
#[must_use]
#[allow(clippy::cast_precision_loss)] // a CSS height needs no more than f64 holds
pub fn pct(n: u64, top: u64) -> f64 {
    if top == 0 {
        return 0.0;
    }
    (n as f64 / top as f64) * 100.0
}

/// One bucket bound as a UTC timestamp, rounded outward to the
/// millisecond: down for a start, up for an end, so a bucket's printed
/// interval always contains every event counted in it.
#[must_use]
pub fn bucket_time(micros: i64, end: bool) -> String {
    let floor = micros.div_euclid(1000);
    let millis = if end && micros.rem_euclid(1000) != 0 {
        floor + 1
    } else {
        floor
    };
    chrono::DateTime::from_timestamp_millis(millis).map_or_else(
        || format!("{micros}µs"),
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
    let span = series.max_secs() - series.min_secs();
    if !(span > 0.0 && width_px > 0.0) {
        return Vec::new();
    }
    let min_ms = (series.min_secs() * 1000.0).ceil() as i64;
    let max_ms = (series.max_secs() * 1000.0).floor() as i64;
    // 1, 2, 5, 10, 20, 50, ... days, so any span reaches a step with at
    // most six ticks; the sequence ends where a step would overflow.
    let day_steps = (0..)
        .map_while(|k| 10_i64.checked_pow(k))
        .flat_map(|p| [1, 2, 5].map(move |m| p.checked_mul(m * DAY_MS)))
        .map_while(|step| step);
    let mut fallback: Option<Vec<Tick>> = None;
    for step in STEPS_MS.into_iter().chain(day_steps) {
        // The wire's finite instants reach either end of i64
        // microseconds, so the extent may too: the first tick rounds up from the
        // minimum (a first tick past i64 lies past the maximum too), and
        // the count is taken in i128.
        let rem = min_ms.rem_euclid(step);
        let first = if rem == 0 {
            Some(min_ms)
        } else {
            min_ms.checked_add(step - rem)
        };
        let Some(first) = first.filter(|&first| first <= max_ms) else {
            break;
        };
        let count = (i128::from(max_ms) - i128::from(first)) / i128::from(step) + 1;
        let Ok(count) = i64::try_from(count) else {
            continue;
        };
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
    let span = series.max_secs() - series.min_secs();
    let mut prev_day: Option<String> = None;
    (0..count)
        .filter_map(|i| {
            // Every tick lies inside the extent, but `step * i` alone can
            // leave i64 when `first` is far below zero.
            let ms = i64::try_from(i128::from(first) + i128::from(step) * i128::from(i)).ok()?;
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
            let fraction = ((ms as f64 / 1000.0 - series.min_secs()) / span).clamp(0.0, 1.0);
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

/// A bucket width, in seconds, in the one unit that keeps
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
/// deliberate: it keeps every comparison on the rows' own clock rather
/// than the browser's, at the cost of the grid ending at the last
/// observed bucket instead of the current one.
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
/// naive wall-clock (the wire is UTC, so no shift is needed).
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
    use trawl_api::{HISTOGRAM_BUCKETS, HistogramBucket};

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

    // ── the strip's series and copy ────────────────────────────────

    /// 2026-09-01T00:00:00Z, in microseconds.
    const DAY0_MICROS: i64 = 1_788_220_800_000_000;

    fn wire(micros: i64) -> String {
        zone::wire_timestamp(micros, chrono::FixedOffset::east_opt(0).expect("UTC"))
    }

    /// A ready histogram from `start_micros` with bins `width_micros`
    /// wide. `filled` lists `(bin, events, errors)`; every other bin is
    /// empty.
    fn ready(
        start_micros: i64,
        width_micros: i64,
        filled: &[(usize, u64, u64)],
        undated: u64,
    ) -> EventHistogram {
        let mut buckets = vec![
            HistogramBucket {
                events: 0,
                errors: 0
            };
            HISTOGRAM_BUCKETS
        ];
        for &(i, events, errors) in filled {
            buckets[i] = HistogramBucket { events, errors };
        }
        EventHistogram::Ready {
            start: wire(start_micros),
            end: wire(start_micros + 48 * width_micros),
            buckets,
            undated,
            severity: true,
            kept_some: false,
        }
    }

    /// The browser draws trawld's bins as they came: `ok` is the bin's
    /// events less its errors, and no row is placed again.
    #[test]
    fn a_ready_histogram_becomes_a_series_of_its_own_bins() {
        let histogram = ready(
            DAY0_MICROS,
            75_000_000,
            &[(0, 3, 1), (20, 5, 0), (47, 2, 2)],
            0,
        );
        let series = Series::from_ready(&histogram, 10).expect("consistent");
        assert_eq!(series.buckets.len(), 48);
        assert_eq!(series.buckets[0], Bucket { ok: 2, err: 1 });
        assert_eq!(series.buckets[20], Bucket { ok: 5, err: 0 });
        assert_eq!(series.buckets[47], Bucket { ok: 0, err: 2 });
        assert_eq!(series.buckets[1], Bucket { ok: 0, err: 0 });
        assert_eq!(series.buckets.iter().map(|b| b.events()).sum::<u64>(), 10);
    }

    /// The bounds are the wire's integers: bin `i` starts at
    /// `start + i × width`, and the last bound is the wire's `end`.
    #[test]
    fn series_bounds_are_the_wire_bounds() {
        let series =
            Series::from_ready(&ready(DAY0_MICROS, 75_000_000, &[(0, 1, 0)], 0), 1).expect("s");
        assert_eq!(series.start_micros, DAY0_MICROS);
        assert_eq!(series.width_micros, 75_000_000);
        assert_eq!(series.bound_micros(1), DAY0_MICROS + 75_000_000);
        assert_eq!(series.bound_micros(48), DAY0_MICROS + 3_600_000_000);
        assert!((series.max_secs() - series.min_secs() - 3600.0).abs() < 1e-6);
    }

    /// One instant spans 48 bins of 20,834 µs, so the strip is 1.000032 s
    /// wide, and the tooltip prints the bin's interval rounded outward.
    #[test]
    fn a_one_instant_series_spans_its_minimum_width() {
        let series = Series::from_ready(&ready(DAY0_MICROS + 100_000, 20_834, &[(0, 1, 1)], 0), 1)
            .expect("one instant");
        assert_eq!(series.width_micros, 20_834);
        assert_eq!(series.bound_micros(48) - series.start_micros, 1_000_032);
        assert_eq!(
            series.tip(0),
            "2026-09-01 00:00:00.100 – 2026-09-01 00:00:00.121 · 1 events · 1 errors"
        );
        assert_eq!(
            series.tip(1),
            "2026-09-01 00:00:00.120 – 2026-09-01 00:00:00.142 · 0 events"
        );
    }

    /// A count the page's total does not account for, or one that breaks
    /// any other invariant, is no series; so is an unavailable one.
    #[test]
    fn an_unavailable_or_inconsistent_histogram_gives_no_series() {
        let good = ready(DAY0_MICROS, 75_000_000, &[(3, 4, 1)], 2);
        assert!(Series::from_ready(&good, 6).is_some());
        assert!(Series::from_ready(&good, 5).is_none(), "total short by one");
        assert!(Series::from_ready(&good, 7).is_none(), "total over by one");
        let unavailable = EventHistogram::Unavailable {
            reason: HistogramReason::Sample,
        };
        assert!(Series::from_ready(&unavailable, 6).is_none());
        let more_errors = ready(DAY0_MICROS, 75_000_000, &[(3, 4, 5)], 2);
        assert!(Series::from_ready(&more_errors, 6).is_none());
        let EventHistogram::Ready {
            start,
            end,
            mut buckets,
            undated,
            severity,
            kept_some,
        } = good
        else {
            unreachable!()
        };
        buckets.pop();
        let short = EventHistogram::Ready {
            start,
            end,
            buckets,
            undated,
            severity,
            kept_some,
        };
        assert!(Series::from_ready(&short, 6).is_none(), "47 bins");
    }

    /// The order the strip reads a settled response in: an empty result
    /// collapses whatever trawld said, then no histogram is the dash, a
    /// reason shows its copy, a broken count is the dash, else bars.
    #[test]
    fn the_strip_follows_its_render_order() {
        let good = ready(DAY0_MICROS, 75_000_000, &[(0, 2, 1)], 0);
        let no_time = EventHistogram::Unavailable {
            reason: HistogramReason::NoUsableTime,
        };
        assert_eq!(strip(0, Some(&good)), Strip::Collapsed);
        assert_eq!(strip(0, Some(&no_time)), Strip::Collapsed);
        assert_eq!(strip(0, None), Strip::Collapsed);
        assert_eq!(strip(2, None), Strip::Dash);
        assert_eq!(
            strip(2, Some(&no_time)),
            Strip::Reason("No histogram: no event in this result has a usable time.")
        );
        assert_eq!(strip(3, Some(&good)), Strip::Dash, "total breaks the count");
        let Strip::Drawn(drawn) = strip(2, Some(&good)) else {
            panic!("a consistent count draws");
        };
        assert_eq!(
            drawn.caption,
            "All 2 events in this result · first to last event · ≈1.3min per bucket · UTC"
        );
        assert!(drawn.severity);
        assert_eq!(drawn.kept_some, None);

        let EventHistogram::Ready {
            start,
            end,
            buckets,
            undated,
            ..
        } = good
        else {
            unreachable!()
        };
        let kept = EventHistogram::Ready {
            start,
            end,
            buckets: buckets
                .iter()
                .map(|b| HistogramBucket {
                    events: b.events,
                    errors: 0,
                })
                .collect(),
            undated,
            severity: false,
            kept_some: true,
        };
        let Strip::Drawn(drawn) = strip(2, Some(&kept)) else {
            panic!("a consistent count draws");
        };
        assert!(!drawn.severity);
        assert_eq!(drawn.kept_some, Some(KEPT_SOME_LINE));
    }

    /// Every string the strip prints, exactly as the spec writes it.
    #[test]
    fn histogram_copy_is_exact() {
        assert_eq!(
            caption(120, 0, 60_000_000),
            "All 120 events in this result · first to last event · 1min per bucket · UTC"
        );
        assert_eq!(
            caption(1_204, 7, 60_000_000),
            "All 1,204 events in this result · 7 without a usable time, not drawn \
             · first to last event · 1min per bucket · UTC"
        );
        assert_eq!(
            caption(1, 0, 20_834),
            "1 event in this result · first to last event · ≈20.8ms per bucket · UTC"
        );
        assert_eq!(
            KEPT_SOME_LINE,
            "Only the events your query kept are counted."
        );
        assert_eq!(OTHER_KEY, "Other");
        assert_eq!(ERROR_KEY, "Error or fatal");
        assert_eq!(NO_SEVERITY_KEY, "Events (no severity in this result)");
        assert_eq!(
            reason_copy(HistogramReason::Sample),
            "No histogram for a sampled result. Remove sample to count every event."
        );
        assert_eq!(
            reason_copy(HistogramReason::StoredResult),
            "No histogram for rows read from a net's run."
        );
        assert_eq!(
            reason_copy(HistogramReason::NoTimeColumn),
            "No histogram: this result has no _time timestamp."
        );
        assert_eq!(
            reason_copy(HistogramReason::NoUsableTime),
            "No histogram: no event in this result has a usable time."
        );
        assert_eq!(
            bar_tip("2026-09-01 00:00:00.000", "2026-09-01 00:01:00.000", 12, 0),
            "2026-09-01 00:00:00.000 – 2026-09-01 00:01:00.000 · 12 events"
        );
        assert_eq!(
            bar_tip("2026-09-01 00:00:00.000", "2026-09-01 00:01:00.000", 12, 3),
            "2026-09-01 00:00:00.000 – 2026-09-01 00:01:00.000 · 12 events · 3 errors"
        );
    }

    /// Counts past u32 reach the tooltip and the scale whole.
    #[test]
    fn series_keeps_u64_counts() {
        let past_u32 = u64::from(u32::MAX) + 1;
        let histogram = ready(DAY0_MICROS, 60_000_000, &[(0, past_u32, 1)], 0);
        let total = usize::try_from(past_u32).expect("a 64-bit host");
        let series = Series::from_ready(&histogram, total).expect("consistent");
        assert_eq!(
            series.buckets[0],
            Bucket {
                ok: past_u32 - 1,
                err: 1
            }
        );
        assert_eq!(series.tallest(), past_u32);
        assert!(
            series
                .tip(0)
                .ends_with(" · 4,294,967,296 events · 1 errors")
        );
        assert_eq!(scale_top(series.tallest()), past_u32);

        let histogram = ready(DAY0_MICROS, 60_000_000, &[(5, u64::MAX, u64::MAX - 1)], 0);
        let total = usize::try_from(u64::MAX).expect("a 64-bit host");
        let Strip::Drawn(drawn) = strip(total, Some(&histogram)) else {
            panic!("a consistent count draws");
        };
        assert_eq!(
            drawn.series.buckets[5],
            Bucket {
                ok: 1,
                err: u64::MAX - 1
            }
        );
        assert_eq!(drawn.series.tallest(), u64::MAX);
        assert!(
            drawn.series.tip(5).ends_with(
                " · 18,446,744,073,709,551,615 events · 18,446,744,073,709,551,614 errors"
            )
        );
        assert!(
            drawn
                .caption
                .starts_with("All 18,446,744,073,709,551,615 events in this result")
        );
    }

    /// The top rounds up to an even count when it can, and stays the
    /// tallest bar when rounding would overflow; it is never zero.
    #[test]
    fn scale_top_does_not_overflow() {
        assert_eq!(scale_top(0), 1);
        assert_eq!(scale_top(1), 1);
        assert_eq!(scale_top(2), 2);
        assert_eq!(scale_top(7), 8);
        let past_u32 = u64::from(u32::MAX) + 1;
        assert_eq!(scale_top(past_u32), past_u32);
        assert_eq!(scale_top(past_u32 + 1), past_u32 + 2);
        assert_eq!(scale_top(u64::MAX), u64::MAX);
        assert_eq!(scale_top(u64::MAX - 1), u64::MAX - 1);
        let top = scale_top(u64::MAX);
        assert!((pct(u64::MAX, top) - 100.0).abs() < 1e-9);
        assert!((pct(top / 2, top) - 50.0).abs() < 1e-9);
        assert!(pct(1, top) > 0.0);
        assert!((pct(0, top)).abs() < f64::EPSILON);
    }

    /// A 48-bin series over `min_secs..max_secs`, rounded to whole
    /// microseconds the way the wire carries them.
    #[allow(clippy::cast_possible_truncation)]
    fn span(min_secs: f64, max_secs: f64) -> Series {
        let start_micros = (min_secs * 1e6).round() as i64;
        let end_micros = (max_secs * 1e6).round() as i64;
        Series {
            buckets: vec![Bucket { ok: 1, err: 0 }; 48],
            start_micros,
            width_micros: (end_micros - start_micros) / 48,
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
        let at = |secs: f64| (secs - series.min_secs()) / (series.max_secs() - series.min_secs());
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
            // A sender's epoch-zero timestamp beside today's events.
            (0.0, DAY0),
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

    /// The wire's finite instants reach either end of i64 microseconds.
    /// Extents that wide still get an axis instead of an overflow.
    #[test]
    fn clock_ticks_survive_extents_at_the_i64_limit() {
        let far = i64::MAX - 1_000_033;
        for (start_micros, end_micros) in [(-far, far), (-far, 0), (0, far)] {
            let width_micros =
                i64::try_from((i128::from(end_micros) - i128::from(start_micros)) / 48)
                    .expect("a 48th of the finite range fits");
            let series = Series {
                buckets: vec![Bucket { ok: 1, err: 0 }; 48],
                start_micros,
                width_micros,
            };
            let (min, max) = (series.min_secs(), series.max_secs());
            for width in [120.0, 1100.0] {
                let ticks = clock_ticks(&series, width);
                assert!(!ticks.is_empty(), "{min}..{max} at {width}px");
                assert!(ticks.iter().all(|t| (0.0..=1.0).contains(&t.fraction)));
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
        // 00:00:00 is the very start and 00:00:10 is 80 ms from the end
        // (48 bins of 210 ms). A two-second step would put 00:00:02 under
        // the dated first label.
        let ticks = clock_ticks(&span(DAY0, DAY0 + 10.08), 600.0);
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

    /// The narrowest strip trawld counts is one instant: 48 bins of
    /// 20,834 µs, just over one second.
    #[test]
    fn clock_ticks_print_milliseconds_below_a_second_step() {
        let series = Series::from_ready(&ready(DAY0_MICROS + 250_000, 20_834, &[(0, 1, 0)], 0), 1)
            .expect("one instant");
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
            bucket_time(1_788_220_800_000_400, false),
            "2026-09-01 00:00:00.000"
        );
        assert_eq!(
            bucket_time(1_788_220_800_000_400, true),
            "2026-09-01 00:00:00.001"
        );
        // A bound already on a millisecond stays there either way.
        assert_eq!(
            bucket_time(1_788_220_800_100_000, true),
            "2026-09-01 00:00:00.100"
        );
        assert_eq!(bucket_time(-1, false), "1969-12-31 23:59:59.999");
        assert_eq!(bucket_time(-1, true), "1970-01-01 00:00:00.000");
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
