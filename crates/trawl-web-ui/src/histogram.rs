// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pure-logic helpers for the histogram surfaces.
//!
//! Two independent jobs live here:
//!
//! - [`bucketize_series`] — the search-page strip. Splits an iterator of
//!   `(timestamp_seconds, is_error)` tuples into `n_buckets`
//!   evenly-spaced buckets between the observed min/max timestamp, and
//!   returns the bounds it used with them so the bars, the axis labels
//!   ([`axis_labels`]) and the accessible bucket table all read one
//!   geometry. Each bucket carries `(ok_count, err_count)`.
//! - [`align_buckets`] — the service drawer's ingest chart. Lays rows
//!   the server already aggregated (`timechart span=1h`) onto a fixed
//!   grid, so gaps in the data render as gaps instead of collapsing the
//!   chart to one bar per returned row.
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
/// axis labels, the accessible bucket table — measures from these two
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

/// The strip's three x-axis labels: the first bucket's start, the
/// midpoint, and the last bucket's end. Real timestamps off the data
/// that was bucketed — never `-15m` / `now` over an observed extent the
/// picker's range may not describe (ADR-0027, amended 2026-09-12).
#[must_use]
pub fn axis_labels(series: &Series) -> (String, String, String) {
    (
        bucket_time(series.min_secs, false),
        bucket_time(f64::midpoint(series.min_secs, series.max_secs), false),
        bucket_time(series.max_secs, true),
    )
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
/// Accepts the executor's `YYYY-MM-DD HH:MM:SS[.fff]` display format and
/// the RFC 3339 `T`-separated form; any trailing zone designator or
/// fractional part is ignored, since the grid resolution is whole
/// buckets. Returns `None` on anything it can't read, which the caller
/// treats as "fall back to unpositioned bars".
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

/// Days since the Unix epoch for a proleptic-Gregorian date.
/// Hinnant's `days_from_civil`.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
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

    #[test]
    fn axis_labels_are_the_first_start_the_midpoint_and_the_last_end() {
        // 2026-09-01T00:00:00Z .. +1h
        let start = 1_788_220_800.0;
        let series = bucketize_series(&[(start, false), (start + 3600.0, false)], 4).expect("s");
        let (left, mid, right) = axis_labels(&series);
        assert_eq!(left, "2026-09-01 00:00:00.000");
        assert_eq!(mid, "2026-09-01 00:30:00.000");
        assert_eq!(right, "2026-09-01 01:00:00.000");
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
