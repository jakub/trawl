// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The whole-result histogram (#197): every row of one execution counted
//! by `_time` into [`HISTOGRAM_BUCKETS`] half-open bins.
//!
//! The decision is split in two. The handler calls [`plan`] on the
//! admitted parse, before execution: a request that did not ask, an
//! aggregation, a `from saved` read and a `sample` stage are settled
//! there, in that order. The worker that ran the query calls [`count`] on
//! the final result, inside its permit and under the post phase, before
//! the page is cut from it: the result's columns and rows settle the rest.
//!
//! Bins are integer microseconds. `start` is the minimum usable `_time`
//! and the bin width is `ceil(max(1 s, max - min + 1 µs) / 48)`, so `end`
//! is past the maximum and every usable row lands in exactly one bin. The
//! arithmetic widens to `i128` before it subtracts, because the span of
//! two finite instants can exceed `i64::MAX`.

use chrono::{Offset, Utc};
use trawl_api::value::{ColumnType, QueryResult, Value};
use trawl_api::zone::{self, TIMESTAMP_INFINITY, TIMESTAMP_NEG_INFINITY};
use trawl_api::{
    EventHistogram, HISTOGRAM_BUCKETS, HISTOGRAM_MIN_SPAN_MICROS, HistogramBucket, HistogramReason,
};
use trawl_core::ast::{PipeStage, Query};
use trawl_engine::cancel::CancelLatch;
use trawl_engine::error::EngineError;
use trawl_engine::post_process::TAIL_CANCEL_STRIDE;

/// [`HISTOGRAM_BUCKETS`] in the bin arithmetic's type.
const BINS: i128 = 48;
const _: () = assert!(HISTOGRAM_BUCKETS == 48, "BINS restates the bin count");

/// The narrowest bin: the minimum span over [`BINS`], rounded up. 20,834 µs.
const MIN_WIDTH: i128 = (HISTOGRAM_MIN_SPAN_MICROS as i128 + BINS - 1) / BINS;

/// A `_time` is usable only below this instant, 1,000,032 µs short of the
/// infinity sentinel. A single instant spans [`MIN_WIDTH`] × 48 =
/// 1,000,032 µs, so the bound keeps every `end` finite, and it drops only
/// the last second or so before the year 294,247.
#[allow(clippy::cast_possible_truncation)] // 1,000,032 fits i64
const USABLE_BELOW: i64 = TIMESTAMP_INFINITY - (MIN_WIDTH * BINS) as i64;

/// What the handler decided before execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistogramPlan {
    /// The response carries no histogram field.
    Absent,
    /// The response says why it carries no counts, whatever the rows hold.
    Refuse(HistogramReason),
    /// The worker counts the result. `kept_some` is the answer's flag.
    Count { kept_some: bool },
}

/// The worker's half of [`HistogramPlan::Count`]: count the final result,
/// and answer `kept_some` with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistogramAsk {
    pub kept_some: bool,
}

/// Steps 1-4 of the eligibility rule, read off the admitted parse. The
/// first match wins: the flag is false, the query aggregates, the rows are
/// a stored run's (`from_saved`), the query samples. Otherwise the worker
/// counts, and the result holds only the rows a `head`, `limit`, `tail` or
/// `dedup` stage kept, which `kept_some` says.
pub fn plan(asked: bool, query: &Query, from_saved: bool) -> HistogramPlan {
    if !asked || query.has_aggregation() {
        return HistogramPlan::Absent;
    }
    if from_saved {
        return HistogramPlan::Refuse(HistogramReason::StoredResult);
    }
    let stages = || query.pipeline.iter().map(|stage| &stage.node);
    if stages().any(|stage| matches!(stage, PipeStage::Sample(_))) {
        return HistogramPlan::Refuse(HistogramReason::Sample);
    }
    HistogramPlan::Count {
        kept_some: stages().any(|stage| {
            matches!(
                stage,
                PipeStage::Limit(_) | PipeStage::Tail(_) | PipeStage::Dedup(_)
            )
        }),
    }
}

/// Count `result`, the final result of one execution, steps 5-7 of the
/// eligibility rule: no `_time` column typed `timestamp` answers
/// `no_time_column`, no row with a usable `_time` answers
/// `no_usable_time`, and otherwise the counts are ready.
///
/// Two passes over the rows and constant extra memory: the first finds the
/// bounds and the undated rows, the second fills the bins. Each pass reads
/// `cancel` at its first row and every [`TAIL_CANCEL_STRIDE`] rows after,
/// and once more when the pass ends, so a result with no usable time is
/// not answered past a cancel either.
///
/// # Errors
/// [`EngineError::Cancelled`] once `cancel` is latched.
pub fn count(
    result: &QueryResult,
    kept_some: bool,
    cancel: &CancelLatch,
) -> Result<EventHistogram, EngineError> {
    let unavailable = |reason| Ok(EventHistogram::Unavailable { reason });
    let Some(time) = result
        .columns
        .iter()
        .position(|c| c.name == trawl_core::schema::TIME && c.ty == ColumnType::Timestamp)
    else {
        return unavailable(HistogramReason::NoTimeColumn);
    };
    let severity = result
        .columns
        .iter()
        .position(|c| c.name == trawl_core::schema::SEVERITY);

    let mut bounds: Option<(i64, i64)> = None;
    let mut undated: u64 = 0;
    for (at, row) in result.rows.iter().enumerate() {
        stride(at, cancel)?;
        match usable(row.get(time)) {
            Some(t) => bounds = Some(bounds.map_or((t, t), |(lo, hi)| (lo.min(t), hi.max(t)))),
            None => undated += 1,
        }
    }
    if cancel.latched() {
        return Err(EngineError::Cancelled);
    }
    let Some((min, max)) = bounds else {
        return unavailable(HistogramReason::NoUsableTime);
    };

    let start = i128::from(min);
    let span = (i128::from(max) - start + 1).max(i128::from(HISTOGRAM_MIN_SPAN_MICROS));
    let width = (span + BINS - 1) / BINS;
    // `max < USABLE_BELOW`, and `end` is at most `max + 48` for a span of
    // a second or more and `min + 1,000,032` below it: finite either way.
    let end = i64::try_from(start + BINS * width)
        .ok()
        .filter(|&end| end < TIMESTAMP_INFINITY)
        .expect("the usability bound keeps `end` finite");

    let mut buckets = [HistogramBucket {
        events: 0,
        errors: 0,
    }; HISTOGRAM_BUCKETS];
    for (at, row) in result.rows.iter().enumerate() {
        stride(at, cancel)?;
        let Some(t) = usable(row.get(time)) else {
            continue;
        };
        // `min <= t <= max < end`, so the quotient is in `0..48`.
        let bin = usize::try_from((i128::from(t) - start) / width)
            .ok()
            .filter(|&bin| bin < HISTOGRAM_BUCKETS)
            .expect("a usable time between the bounds lands in a bin");
        buckets[bin].events += 1;
        if is_error(row, severity) {
            buckets[bin].errors += 1;
        }
    }
    if cancel.latched() {
        return Err(EngineError::Cancelled);
    }

    Ok(EventHistogram::Ready {
        start: zone::wire_timestamp(min, Utc.fix()),
        end: zone::wire_timestamp(end, Utc.fix()),
        buckets: buckets.to_vec(),
        undated,
        severity: severity.is_some(),
        kept_some,
    })
}

/// Read the latch at row 0 and every [`TAIL_CANCEL_STRIDE`] rows after.
fn stride(at: usize, cancel: &CancelLatch) -> Result<(), EngineError> {
    if at.is_multiple_of(TAIL_CANCEL_STRIDE) && cancel.latched() {
        return Err(EngineError::Cancelled);
    }
    Ok(())
}

/// A `_time` cell's instant, when it is text `wire_micros` reads, neither
/// infinity, and below [`USABLE_BELOW`].
fn usable(cell: Option<&Value>) -> Option<i64> {
    let Some(Value::String(text)) = cell else {
        return None;
    };
    zone::wire_micros(text)
        .filter(|&micros| micros != TIMESTAMP_NEG_INFINITY && micros < USABLE_BELOW)
}

/// Whether a row's `_severity` cell is an integer in the error band or
/// above, the rule the results grid applies.
fn is_error(row: &[Value], severity: Option<usize>) -> bool {
    matches!(
        severity.and_then(|at| row.get(at)),
        Some(Value::Integer(n)) if *n >= i64::from(trawl_core::severity::ERROR_BAND.0)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use trawl_api::value::Column;

    /// A UTC wire instant.
    fn wire(micros: i64) -> String {
        zone::wire_timestamp(micros, Utc.fix())
    }

    fn timestamp(micros: i64) -> Value {
        Value::String(wire(micros))
    }

    /// A result with a timestamp-typed `_time` column holding `cells`.
    fn timed(cells: Vec<Value>) -> QueryResult {
        QueryResult {
            columns: vec![Column::new("_time", ColumnType::Timestamp)],
            rows: cells.into_iter().map(|cell| vec![cell]).collect(),
        }
    }

    fn ready(result: &QueryResult) -> (i64, i64, Vec<HistogramBucket>, u64, bool) {
        match count(result, false, &CancelLatch::never()).expect("an unlatched walk") {
            EventHistogram::Ready {
                start,
                end,
                buckets,
                undated,
                severity,
                ..
            } => (
                zone::wire_micros(&start).expect("a wire start"),
                zone::wire_micros(&end).expect("a wire end"),
                buckets,
                undated,
                severity,
            ),
            EventHistogram::Unavailable { reason } => panic!("unavailable: {reason:?}"),
        }
    }

    fn reason(result: &QueryResult) -> HistogramReason {
        match count(result, false, &CancelLatch::never()).expect("an unlatched walk") {
            EventHistogram::Unavailable { reason } => reason,
            ready @ EventHistogram::Ready { .. } => panic!("ready: {ready:?}"),
        }
    }

    /// The bin width the rule gives for `min` and `max`, stated apart from
    /// the counter: `ceil(max(1 s, max - min + 1) / 48)`.
    fn expected_width(min: i64, max: i64) -> i128 {
        let span = (i128::from(max) - i128::from(min) + 1).max(1_000_000);
        span / 48 + i128::from(span % 48 != 0)
    }

    /// `min`, `max`, every inner edge between them and the microsecond
    /// on each side of it.
    fn edges_and_neighbours(min: i64, max: i64) -> Vec<i64> {
        let width = expected_width(min, max);
        let mut times = vec![min, max];
        for i in 1..48 {
            let edge = i128::from(min) + i * width;
            for t in [edge - 1, edge, edge + 1] {
                if t <= i128::from(max) {
                    times.push(i64::try_from(t).unwrap());
                }
            }
        }
        times
    }

    /// Count `times` and check the geometry against the rule: `start` is
    /// the minimum, the width is the rule's, `end` is 48 widths on and past
    /// the maximum, and each time lands in exactly one half-open bin, the
    /// one the counter put it in.
    fn assert_binned(label: &str, times: &[i64]) {
        let min = *times.iter().min().unwrap();
        let max = *times.iter().max().unwrap();
        let result = timed(times.iter().map(|&t| timestamp(t)).collect());
        let (start, end, buckets, undated, _) = ready(&result);
        let width = expected_width(min, max);
        assert_eq!(start, min, "{label}: start is the minimum");
        assert_eq!(
            i128::from(end),
            i128::from(min) + 48 * width,
            "{label}: end is 48 widths past start"
        );
        assert!(end > max, "{label}: end {end} is past max {max}");
        assert!(end < TIMESTAMP_INFINITY, "{label}: end is finite");
        assert_eq!(undated, 0, "{label}");

        let mut expected = [0_u64; 48];
        for &t in times {
            let holding: Vec<usize> = (0..48)
                .filter(|&i| {
                    let lo = i128::from(min) + i128::try_from(i).unwrap() * width;
                    lo <= i128::from(t) && i128::from(t) < lo + width
                })
                .collect();
            assert_eq!(
                holding.len(),
                1,
                "{label}: {t} lands in one bin: {holding:?}"
            );
            expected[holding[0]] += 1;
        }
        let counted: Vec<u64> = buckets.iter().map(|b| b.events).collect();
        assert_eq!(counted, expected, "{label}: the bins");
    }

    #[test]
    fn histogram_bins_are_half_open_integer_microseconds() {
        // A span of exactly 48 s: one-second bins, edges on whole seconds.
        assert_binned("whole seconds", &edges_and_neighbours(0, 47_999_999));
        // A span that does not divide by 48: the width rounds up.
        assert_binned(
            "a ragged span",
            &edges_and_neighbours(1_700_000_000_000_000, 1_700_000_100_000_005),
        );
        // Negative epochs, wholly before 1970 and across it.
        assert_binned(
            "before the epoch",
            &edges_and_neighbours(-86_400_000_000 - 7, -1),
        );
        assert_binned(
            "across the epoch",
            &edges_and_neighbours(-3_600_000_001, 3_599_999_999),
        );
        // The widest span the counter accepts: `max - min` overflows i64.
        assert_binned(
            "the full range",
            &edges_and_neighbours(TIMESTAMP_NEG_INFINITY + 1, USABLE_BELOW - 1),
        );
        // A span shorter than a second still spans one, from `min`.
        assert_binned("a short span", &edges_and_neighbours(5, 900_000));

        // One instant: the narrowest bin, 20,834 µs, and all rows in bin 0.
        for t in [0, -1, 1_731_234_567_890_123, USABLE_BELOW - 1] {
            let result = timed(vec![timestamp(t), timestamp(t)]);
            let (start, end, buckets, _, _) = ready(&result);
            assert_eq!(start, t);
            assert_eq!(i128::from(end) - i128::from(start), 48 * 20_834);
            assert_eq!(MIN_WIDTH, 20_834);
            assert_eq!(buckets[0].events, 2, "{t}");
            assert_eq!(buckets.iter().map(|b| b.events).sum::<u64>(), 2, "{t}");
        }

        // Row order does not move `start`: it is the minimum, not the
        // first row.
        let descending = timed(vec![
            timestamp(3_000_000),
            timestamp(2_000_000),
            timestamp(0),
        ]);
        let (start, _, buckets, _, _) = ready(&descending);
        assert_eq!(start, 0);
        assert_eq!(buckets[0].events, 1);
        assert_eq!(
            buckets[47].events, 1,
            "3 s lands in the last bin: the width is 62,501 µs"
        );
    }

    #[test]
    fn histogram_counts_undated_rows() {
        assert_eq!(USABLE_BELOW, TIMESTAMP_INFINITY - 1_000_032);
        let cutoff = wire(USABLE_BELOW);
        assert_eq!(
            zone::wire_micros(&cutoff),
            Some(TIMESTAMP_INFINITY - 1_000_032),
            "the cutoff's text reads back as the cutoff"
        );
        let result = timed(vec![
            timestamp(1_000_000),
            Value::Null,
            Value::String("not a time".to_owned()),
            Value::String("2026-02-30T00:00:00Z".to_owned()),
            Value::String("infinity".to_owned()),
            Value::String("-infinity".to_owned()),
            Value::String(cutoff),
            Value::Integer(1_000_000),
            // The microsecond below the cutoff is usable.
            timestamp(USABLE_BELOW - 1),
        ]);
        let (start, end, buckets, undated, _) = ready(&result);
        assert_eq!(undated, 7);
        assert_eq!(buckets.iter().map(|b| b.events).sum::<u64>(), 2);
        assert_eq!(start, 1_000_000);
        assert!(end > USABLE_BELOW - 1 && end < TIMESTAMP_INFINITY);
        assert!(
            EventHistogram::Ready {
                start: wire(start),
                end: wire(end),
                buckets,
                undated,
                severity: false,
                kept_some: false,
            }
            .consistent_with(result.rows.len())
        );

        // A short row has no `_time` cell, and counts as undated too.
        let short = QueryResult {
            columns: vec![
                Column::new("host", ColumnType::String),
                Column::new("_time", ColumnType::Timestamp),
            ],
            rows: vec![
                vec![Value::String("a".to_owned()), timestamp(0)],
                vec![Value::String("b".to_owned())],
            ],
        };
        let (_, _, _, undated, _) = ready(&short);
        assert_eq!(undated, 1);
    }

    #[test]
    fn histogram_reasons_follow_the_columns() {
        let with = |columns: Vec<Column>, rows: Vec<Vec<Value>>| QueryResult { columns, rows };
        let host = || Column::new("host", ColumnType::String);
        let readable = || timestamp(1_000_000);

        assert_eq!(
            reason(&with(vec![host()], vec![vec![readable()]])),
            HistogramReason::NoTimeColumn,
            "no `_time` column"
        );
        assert_eq!(
            reason(&with(
                vec![Column::new("_time", ColumnType::String)],
                vec![vec![readable()]]
            )),
            HistogramReason::NoTimeColumn,
            "a string-typed `_time`, though its text reads as instants"
        );
        assert_eq!(
            reason(&with(
                vec![Column::new("time", ColumnType::Timestamp)],
                vec![vec![readable()]]
            )),
            HistogramReason::NoTimeColumn,
            "a timestamp column under another name"
        );
        assert_eq!(
            reason(&with(
                vec![Column::new("_time", ColumnType::Date)],
                vec![vec![Value::String("2026-01-15".to_owned())]]
            )),
            HistogramReason::NoTimeColumn,
            "a date-typed `_time`"
        );
        assert_eq!(
            reason(&timed(vec![
                Value::Null,
                Value::String("infinity".to_owned())
            ])),
            HistogramReason::NoUsableTime,
            "every row undated"
        );
        assert_eq!(
            reason(&timed(Vec::new())),
            HistogramReason::NoUsableTime,
            "zero rows under a timestamp-typed `_time`"
        );
        assert_eq!(
            reason(&with(vec![host()], Vec::new())),
            HistogramReason::NoTimeColumn,
            "zero rows and no `_time`: the column rule comes first"
        );
        assert_eq!(
            reason(&with(
                vec![Column::new("_time", ColumnType::String)],
                Vec::new()
            )),
            HistogramReason::NoTimeColumn,
            "zero rows under a string-typed `_time`"
        );
        assert_eq!(
            reason(&QueryResult::empty()),
            HistogramReason::NoTimeColumn,
            "no columns at all"
        );
    }

    #[test]
    fn histogram_errors_read_only_severity() {
        let row = |sev: Value, bare: Value| vec![timestamp(0), sev, bare];
        let result = QueryResult {
            columns: vec![
                Column::new("_time", ColumnType::Timestamp),
                Column::new("_severity", ColumnType::Integer),
                Column::new("severity", ColumnType::Integer),
            ],
            rows: vec![
                row(Value::Integer(16), Value::Null),
                row(Value::Integer(17), Value::Null),
                row(Value::Integer(24), Value::Null),
                row(Value::Null, Value::Null),
                row(Value::Integer(9), Value::Integer(20)),
                row(Value::String("20".to_owned()), Value::Null),
                row(Value::Float(20.0), Value::Null),
            ],
        };
        let (_, _, buckets, _, severity) = ready(&result);
        assert!(severity);
        assert_eq!(buckets[0].events, 7);
        assert_eq!(
            buckets[0].errors, 2,
            "17 and 24 are errors; 16, null, a bare `severity` of 20, text and a float are not"
        );

        let without = QueryResult {
            columns: vec![
                Column::new("_time", ColumnType::Timestamp),
                Column::new("severity", ColumnType::Integer),
            ],
            rows: vec![
                vec![timestamp(0), Value::Integer(20)],
                vec![timestamp(5_000_000), Value::Integer(24)],
            ],
        };
        let (_, _, buckets, _, severity) = ready(&without);
        assert!(!severity, "no `_severity` column");
        assert!(buckets.iter().all(|b| b.errors == 0), "{buckets:?}");
        assert_eq!(buckets.iter().map(|b| b.events).sum::<u64>(), 2);

        let all_null = QueryResult {
            columns: vec![
                Column::new("_time", ColumnType::Timestamp),
                Column::new("_severity", ColumnType::Integer),
            ],
            rows: vec![vec![timestamp(0), Value::Null]],
        };
        let (_, _, buckets, _, severity) = ready(&all_null);
        assert!(severity, "an all-null `_severity` is still the column");
        assert_eq!(buckets[0].errors, 0);
    }

    #[test]
    fn histogram_payload_is_bounded() {
        // The widest bounds the counter writes: the oldest finite instant
        // and the latest usable one, both in the expanded-year spelling.
        let extremes = timed(vec![
            timestamp(TIMESTAMP_NEG_INFINITY + 1),
            timestamp(USABLE_BELOW - 1),
        ]);
        let EventHistogram::Ready {
            start,
            end,
            buckets,
            ..
        } = count(&extremes, false, &CancelLatch::never()).unwrap()
        else {
            panic!("the extremes are usable");
        };
        assert!(
            start.starts_with('-') && end.starts_with('+'),
            "{start} {end}"
        );
        assert_eq!(buckets.len(), 48);
        let widest = EventHistogram::Ready {
            start,
            end,
            buckets: vec![
                HistogramBucket {
                    events: u64::MAX,
                    errors: u64::MAX,
                };
                48
            ],
            undated: u64::MAX,
            severity: false,
            kept_some: false,
        };
        let bytes = serde_json::to_vec(&widest).unwrap().len();
        assert!(bytes <= 4_096, "{bytes} bytes");
    }

    #[test]
    fn histogram_walk_reads_the_latch_in_each_pass() {
        let latched = || {
            let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
            CancelLatch::new(flag)
        };
        // Already latched: the first row of the first pass stops the walk.
        let rows = timed(vec![timestamp(0); 3]);
        assert!(matches!(
            count(&rows, false, &latched()),
            Err(EngineError::Cancelled)
        ));
        // A reason settled by the columns walks nothing.
        assert!(matches!(
            count(&QueryResult::empty(), false, &latched()),
            Ok(EventHistogram::Unavailable {
                reason: HistogramReason::NoTimeColumn
            })
        ));
        // Zero rows under a typed `_time`: no stride read happens, and the
        // read that ends the pass still keeps `no_usable_time` from being
        // answered past a cancel.
        assert!(matches!(
            count(&timed(Vec::new()), false, &latched()),
            Err(EngineError::Cancelled)
        ));
    }

    /// The eligibility order on real parses: an aggregation beats a
    /// `from saved` read and a `sample`, a `from saved` read beats a
    /// `sample`, and only `head`, `limit`, `tail` and `dedup` set
    /// `kept_some`.
    #[test]
    fn histogram_plan_follows_the_eligibility_order() {
        let parse =
            |dsl: &str| trawl_core::parser::parse(dsl).unwrap_or_else(|e| panic!("{dsl}: {e:?}"));
        let count = |kept_some| HistogramPlan::Count { kept_some };
        let refuse = HistogramPlan::Refuse;
        let cases = [
            ("*", false, false, HistogramPlan::Absent),
            ("* | head 10", false, false, HistogramPlan::Absent),
            ("* | stats count()", true, false, HistogramPlan::Absent),
            ("* | timechart count()", true, false, HistogramPlan::Absent),
            ("* | top 5 host", true, false, HistogramPlan::Absent),
            ("* | rare 5 host", true, false, HistogramPlan::Absent),
            (
                "| from saved daily run=all | stats count()",
                true,
                true,
                HistogramPlan::Absent,
            ),
            (
                "* | sample 10 | stats count()",
                true,
                false,
                HistogramPlan::Absent,
            ),
            (
                "| from saved daily | sample 10% | stats count()",
                true,
                true,
                HistogramPlan::Absent,
            ),
            (
                "| from saved daily",
                true,
                true,
                refuse(HistogramReason::StoredResult),
            ),
            (
                "| from saved daily | sample 10",
                true,
                true,
                refuse(HistogramReason::StoredResult),
            ),
            (
                "* | sample 10",
                true,
                false,
                refuse(HistogramReason::Sample),
            ),
            (
                "* | sample 10%",
                true,
                false,
                refuse(HistogramReason::Sample),
            ),
            (
                "* | head 5 | sample 10%",
                true,
                false,
                refuse(HistogramReason::Sample),
            ),
            ("*", true, false, count(false)),
            ("* | head 10", true, false, count(true)),
            ("* | limit 10", true, false, count(true)),
            ("* | tail 5", true, false, count(true)),
            ("* | dedup host", true, false, count(true)),
            ("* | sort -_time", true, false, count(false)),
            (
                "* | eventstats count() as n by host",
                true,
                false,
                count(false),
            ),
            (
                "* | extract kv | fields _time, host",
                true,
                false,
                count(false),
            ),
        ];
        for (dsl, asked, from_saved, expected) in cases {
            assert_eq!(
                plan(asked, &parse(dsl), from_saved),
                expected,
                "{dsl} asked={asked} from_saved={from_saved}"
            );
        }
    }
}
