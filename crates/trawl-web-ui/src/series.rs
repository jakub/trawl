// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Alignment of an aggregation result onto the axes the Visualization
//! tab draws (ADR-0038).
//!
//! [`align_series`] turns a `timechart` result into one x vector of
//! bucket instants and one y vector per series, every y the same length
//! as x, with a `None` wherever a (bucket, series) has no row. The roles
//! come from the executed query — the `by` fields are the group columns
//! and every other non-`_time` column is a metric — never from cell
//! types, the same way [`crate::categorical`] reads a `stats … by`. The
//! bucket width comes from the query too, through the [`Lane`] that
//! produced the result. [`cat_points`] is the ordinal-axis twin for a
//! categorical shape that module already detected.
//!
//! At the crate root rather than under `components/`, which is
//! wasm-gated, so the ladder is tested on native
//! `cargo test -p trawl-web-ui series`.
//!
//! On native, only the tests consume these items — `#[allow(dead_code)]`
//! at module scope silences the bin-crate dead-code warning, as in
//! `categorical.rs`. The allow is unconditional for now because the
//! Visualization component that consumes this module lands in the next
//! commit; once `components/chart.rs` calls [`align_series`], narrow it
//! back to `cfg_attr(not(target_arch = "wasm32"), allow(dead_code))` so
//! the wasm build catches anything the component stops using.

#![allow(dead_code)]

use std::collections::{BTreeSet, HashMap};

use trawl_api::display::value_to_string;
use trawl_api::value::{QueryResult, Value};
use trawl_core::ast::{PipeStage, TimechartStage};
use trawl_core::schema::{TIME, catalog_key};
use trawl_core::timechart::{query_span, resolve_span};

use crate::categorical::CatShape;
use crate::histogram::days_from_civil;

/// The most series a Line chart draws. Must be ≤ the palette in
/// `vendor/src/uplot.ts` (`readColors`, `lineDashes`): a seventh series
/// would wrap onto the first colour and dash and become
/// indistinguishable from it. `palette_covers_the_series_cap` pins the
/// two together.
pub const SERIES_CAP: usize = 6;
/// The most groups a Column or Bar chart draws.
pub const GROUP_CAP: usize = 20;
/// The most grid instants a Line chart lays out before refusing.
pub const MAX_INSTANTS: usize = 20_000;

/// A `timechart` result aligned on its bucket grid.
#[derive(Debug, Clone, PartialEq)]
pub struct SeriesSet {
    /// Epoch seconds, ascending, one per grid instant.
    pub xs: Vec<f64>,
    /// `(label, values)`; every `values` has `xs.len()` entries and a
    /// `None` is a gap, never a zero.
    pub series: Vec<(String, Vec<Option<f64>>)>,
    /// How many series the result held before [`SERIES_CAP`] applied.
    pub total_series: usize,
    /// The `by` fields in query order, for the caption; empty when the
    /// timechart is ungrouped.
    pub group_fields: Vec<String>,
}

/// Why a result is not drawn as lines. Every variant names the fix or
/// the place to look instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    Pivot,
    Top,
    Rare,
    GroupedTwoMetrics,
    NoTime,
    NoMetric,
    Empty,
    /// A metric cell that is not a number or a null; carries the cell as
    /// the table prints it.
    BadMetric(String),
    /// A `_time` cell in neither admitted form; carries the cell text.
    UnparsedTime(String),
    /// Two rows landed on one (bucket, series). `bucket` is the original
    /// `_time` text of the second row.
    DuplicateCell {
        bucket: String,
        series: String,
    },
    /// The grid would hold this many instants, more than
    /// [`MAX_INSTANTS`].
    GridTooLarge(usize),
}

impl Refusal {
    /// The sentence shown in place of the chart. The copy is asserted by
    /// the e2e suite; change it there too.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Pivot => {
                "Pivot results are not drawn as lines. Open Events for the table.".into()
            }
            Self::Top => "Top results are not drawn as lines. Open Events for the table.".into(),
            Self::Rare => "Rare results are not drawn as lines. Open Events for the table.".into(),
            Self::GroupedTwoMetrics => {
                "Grouped charts draw one metric. Chart one metric, or open Events.".into()
            }
            Self::NoTime => {
                "This result has no time axis. Choose Column for stats by, or open Events.".into()
            }
            Self::NoMetric => {
                "Visualization needs a metric column. Use timechart count(), or open Events.".into()
            }
            Self::Empty => "No rows to draw.".into(),
            Self::BadMetric(_) => {
                "Visualization draws numeric metrics. Open Events for these values.".into()
            }
            Self::UnparsedTime(cell) => format!(
                "A _time value could not be read as a timestamp: {cell}. Open Events for the exact rows."
            ),
            Self::DuplicateCell { bucket, series } => format!(
                "Two rows share the {bucket} bucket for {series}. Open Events for the exact rows."
            ),
            Self::GridTooLarge(n) => format!(
                "This result spans {n} time buckets; the chart draws up to 20,000. Use a larger span or a shorter range."
            ),
        }
    }

    /// Whether the refusal should be followed by a link to the Events
    /// tab. True for every variant today; the component asks here so the
    /// answer has one home when a variant without a table arrives.
    #[must_use]
    pub fn offers_events(&self) -> bool {
        match self {
            Self::Pivot
            | Self::Top
            | Self::Rare
            | Self::GroupedTwoMetrics
            | Self::NoTime
            | Self::NoMetric
            | Self::Empty
            | Self::BadMetric(_)
            | Self::UnparsedTime(_)
            | Self::DuplicateCell { .. }
            | Self::GridTooLarge(_) => true,
        }
    }
}

/// Parse a `_time` cell into epoch seconds, admitting exactly the two
/// forms ADR-0038 names and nothing else.
///
/// - `YYYY-MM-DD HH:MM:SS[.fff]`: the snapshot wire format. UTC wall
///   clock with no zone, because the web UI sends `timezone: None` and
///   the server defaults the offset to zero.
/// - `YYYY-MM-DDTHH:MM:SS[.fff]Z` or `…+00:00`: RFC 3339 with a zero
///   offset, which is what the live stream's chrono `to_rfc3339()`
///   writes (`+00:00`).
///
/// A non-zero offset, a `T` form with no zone, a space form with one, an
/// impossible date (`2026-02-30`), or any other text is `None`. The
/// fraction is parsed so that it is admitted, then truncated: the grid
/// is whole seconds.
///
/// Strict where [`crate::histogram::parse_bucket_ms`] is lenient. That
/// parser lays the service drawer's ingest grid and ignores zone and
/// fraction because it treats any timestamp as naive wall clock; this
/// one refuses, because a `_time` the chart cannot place exactly is a
/// point drawn in the wrong place, and the Line refusal ladder says so
/// instead ([`Refusal::UnparsedTime`]). Both share [`days_from_civil`].
#[must_use]
pub fn parse_instant_secs(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    // Fixed-width prefix: `YYYY-MM-DD?HH:MM:SS` is 19 bytes.
    if bytes.len() < 19 {
        return None;
    }
    let digits = |from: usize, len: usize| -> Option<i64> {
        let slice = &bytes[from..from + len];
        if !slice.iter().all(u8::is_ascii_digit) {
            return None;
        }
        Some(
            slice
                .iter()
                .fold(0_i64, |acc, digit| acc * 10 + i64::from(digit - b'0')),
        )
    };
    let year = digits(0, 4)?;
    if bytes[4] != b'-' {
        return None;
    }
    let month = digits(5, 2)?;
    if bytes[7] != b'-' {
        return None;
    }
    let day = digits(8, 2)?;
    let separator = bytes[10];
    if separator != b' ' && separator != b'T' {
        return None;
    }
    let hour = digits(11, 2)?;
    if bytes[13] != b':' {
        return None;
    }
    let minute = digits(14, 2)?;
    if bytes[16] != b':' {
        return None;
    }
    let second = digits(17, 2)?;

    // Optional fraction: a dot and at least one digit, then truncated.
    let mut rest = &bytes[19..];
    if let [b'.', tail @ ..] = rest {
        let count = tail.iter().take_while(|c| c.is_ascii_digit()).count();
        if count == 0 {
            return None;
        }
        rest = &tail[count..];
    }

    // The zone is decided by the separator: none after a space, a zero
    // offset after a `T`.
    let zone_ok = match separator {
        b' ' => rest.is_empty(),
        _ => rest == b"Z" || rest == b"+00:00",
    };
    if !zone_ok {
        return None;
    }

    if !(1..=12).contains(&month)
        || day < 1
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Days in a proleptic-Gregorian month; `month` is already known to be 1–12.
fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
            if leap { 29 } else { 28 }
        }
    }
}

/// Which execution produced a result, and so which bucket width it was
/// cut with.
///
/// The snapshot emitter resolves the automatic span from the query's
/// `last=` filter; the live compiler has no filter and always resolves as
/// if there were none (`resolve_span(span, None)`). A live
/// `last=4h | timechart count()` is therefore bucketed at one minute,
/// and aligning it on the five-minute grid the snapshot would use would
/// scatter its rows as off-lattice instants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    Snapshot,
    Live,
}

/// Align a `timechart` result on its bucket grid, or say why it cannot
/// be drawn as lines.
///
/// `query` is the DSL that produced `result`, never the text being
/// typed: the roles are read from its last `timechart` stage. The
/// output is a function of the row SET — any row order aligns
/// identically — because the grid is the sorted set of instants and the
/// legend is sorted by label.
///
/// Memory is bounded by the cap, not by the result: every group is read
/// into a sparse map first, and a dense `xs.len()` vector is allocated
/// only for the [`SERIES_CAP`] groups that survive ranking. Twenty
/// thousand rows in twenty thousand groups over twenty thousand instants
/// is twenty thousand small maps and six dense vectors, not 400 million
/// cells.
///
/// # Errors
///
/// A [`Refusal`] naming the rung of the ladder that stopped the draw.
#[allow(clippy::too_many_lines)]
pub fn align_series(query: &str, result: &QueryResult, lane: Lane) -> Result<SeriesSet, Refusal> {
    // A query that does not parse cannot have produced this result; it
    // has no time axis anyone can read off it.
    let ast = trawl_core::parser::parse(query).map_err(|_| Refusal::NoTime)?;
    let mut tc: Option<&TimechartStage> = None;
    for stage in &ast.pipeline {
        match &stage.node {
            PipeStage::Pivot(_) => return Err(Refusal::Pivot),
            PipeStage::Top(_) => return Err(Refusal::Top),
            PipeStage::Rare(_) => return Err(Refusal::Rare),
            PipeStage::Timechart(t) => tc = Some(t),
            _ => {}
        }
    }
    let tc = tc.ok_or(Refusal::NoTime)?;

    let names: Vec<String> = result
        .columns
        .iter()
        .map(|c| catalog_key(&c.name))
        .collect();
    let time_col = names
        .iter()
        .position(|n| n == TIME)
        .ok_or(Refusal::NoTime)?;

    // `by HOST` groups the catalog's `host`, and the response names the
    // column as the catalog does; fold both sides before they meet, as
    // `categorical::detect` does. A `by` field the response no longer
    // carries (projected away) is not a role anything can play.
    let mut group_cols: Vec<usize> = Vec::new();
    let mut group_fields: Vec<String> = Vec::new();
    for field in &tc.group_by {
        if let Some(ci) = names.iter().position(|n| *n == catalog_key(field)) {
            group_cols.push(ci);
            group_fields.push(field.clone());
        }
    }

    // Metrics: every column that is neither `_time` nor a group. Not the
    // names the stage declares — a later `rename` may have changed any
    // of them, and a metric that lost its declared name is still the
    // number the reader asked for.
    let metric_cols: Vec<usize> = (0..names.len())
        .filter(|ci| *ci != time_col && !group_cols.contains(ci))
        .collect();

    if metric_cols.is_empty() {
        return Err(Refusal::NoMetric);
    }
    if !group_cols.is_empty() && metric_cols.len() > 1 {
        return Err(Refusal::GroupedTwoMetrics);
    }
    if result.rows.is_empty() {
        return Err(Refusal::Empty);
    }

    // The width the lane's executor cut the buckets with (see `Lane`).
    let span = match lane {
        Lane::Snapshot => query_span(&ast).map_or(60, |d| d.to_seconds()),
        Lane::Live => resolve_span(tc.span, None).to_seconds(),
    }
    .max(1);
    let span = i64::try_from(span).unwrap_or(i64::MAX);

    // Pass 1: every `_time` cell, so that an unreadable one refuses
    // before any series is built.
    let mut instants: Vec<i64> = Vec::with_capacity(result.rows.len());
    for row in &result.rows {
        let cell = row.get(time_col).unwrap_or(&Value::Null);
        let text = match cell {
            Value::String(s) => s.as_str(),
            other => return Err(Refusal::UnparsedTime(value_to_string(other))),
        };
        instants
            .push(parse_instant_secs(text).ok_or_else(|| Refusal::UnparsedTime(text.to_owned()))?);
    }
    let min = *instants.iter().min().expect("rows are non-empty");
    let max = *instants.iter().max().expect("rows are non-empty");

    // Grid rule: every `span` from the earliest bucket to the latest,
    // PLUS any returned instant that does not sit on that lattice. The
    // regular steps are what make a missing bucket a visible gap; the
    // extra instants keep a row the server did return (an explicit `on`
    // column, an odd first bucket) from being dropped or snapped.
    let regular = usize::try_from((max - min) / span + 1).unwrap_or(usize::MAX);
    if regular > MAX_INSTANTS {
        return Err(Refusal::GridTooLarge(regular));
    }
    let mut grid: BTreeSet<i64> = instants.iter().copied().collect();
    let mut t = min;
    while t <= max {
        grid.insert(t);
        t = t.saturating_add(span);
        if t == i64::MAX {
            break;
        }
    }
    if grid.len() > MAX_INSTANTS {
        return Err(Refusal::GridTooLarge(grid.len()));
    }
    let xs: Vec<i64> = grid.into_iter().collect();
    let slot: HashMap<i64, usize> = xs.iter().enumerate().map(|(i, t)| (*t, i)).collect();

    // Pass 2: sparse fill. Ungrouped: one series per metric column, keyed
    // and named by the column. Grouped: one per distinct group TUPLE —
    // the key is the vector of rendered cells, never their joined label,
    // so `("a · b", "c")` and `("a", "b · c")` stay two series even
    // though they print alike. Every (slot, series) visit is recorded
    // whether or not the cell holds a number, so two null rows on one
    // bucket are a duplicate too.
    let mut series: Vec<Sparse> = Vec::new();
    let mut index: HashMap<Vec<String>, usize> = HashMap::new();
    for (row, instant) in result.rows.iter().zip(&instants) {
        let x = slot[instant];
        if group_cols.is_empty() {
            for ci in &metric_cols {
                let name = result.columns[*ci].name.clone();
                let si = series_index(&mut index, &mut series, vec![name.clone()], name);
                series[si].visit(x, row.get(*ci), || value_to_string(&row[time_col]))?;
            }
        } else {
            let key: Vec<String> = group_cols
                .iter()
                .map(|ci| value_to_string(row.get(*ci).unwrap_or(&Value::Null)))
                .collect();
            let label = key.join(" · ");
            let si = series_index(&mut index, &mut series, key, label);
            series[si].visit(x, row.get(metric_cols[0]), || {
                value_to_string(&row[time_col])
            })?;
        }
    }

    // Rank by total descending, ties by label, keep the cap, then order
    // the survivors by label so the legend is stable across refreshes.
    // Only the survivors are made dense.
    let total_series = series.len();
    series.sort_by(|a, b| {
        b.total
            .total_cmp(&a.total)
            .then_with(|| a.label.cmp(&b.label))
    });
    series.truncate(SERIES_CAP);
    series.sort_by(|a, b| a.label.cmp(&b.label));
    let series = series
        .into_iter()
        .map(|Sparse { label, points, .. }| (label, dense(points, xs.len())))
        .collect();

    #[allow(clippy::cast_precision_loss)]
    // epoch seconds fit in 2^53 for the next 285 million years
    let xs = xs.into_iter().map(|t| t as f64).collect();
    Ok(SeriesSet {
        xs,
        series,
        total_series,
        group_fields,
    })
}

/// One series before ranking: its visited slots and their values, and
/// the running total the ranking sorts on.
struct Sparse {
    label: String,
    /// Grid slot → value; a `None` value is a visited null.
    points: HashMap<usize, Option<f64>>,
    total: f64,
}

impl Sparse {
    /// Record the row at `slot`, refusing a second visit or a cell that
    /// is not a number.
    fn visit(
        &mut self,
        slot: usize,
        cell: Option<&Value>,
        bucket: impl Fn() -> String,
    ) -> Result<(), Refusal> {
        let value = match cell {
            // An absent cell (a short row) and an explicit null are both
            // "no measurement": a gap, never a zero — but still a visit.
            None | Some(Value::Null) => None,
            Some(v) => Some(numeric(v).ok_or_else(|| Refusal::BadMetric(value_to_string(v)))?),
        };
        if self.points.insert(slot, value).is_some() {
            return Err(Refusal::DuplicateCell {
                bucket: bucket(),
                series: self.label.clone(),
            });
        }
        if let Some(v) = value {
            self.total += v;
        }
        Ok(())
    }
}

/// The dense vector the chart draws from one series' sparse points, one
/// entry per grid instant.
fn dense(points: HashMap<usize, Option<f64>>, len: usize) -> Vec<Option<f64>> {
    #[cfg(test)]
    DENSE_ALLOCATIONS.with(|n| n.set(n.get() + 1));
    let mut values = vec![None; len];
    for (slot, value) in points {
        values[slot] = value;
    }
    values
}

// How many dense vectors `align_series` allocated on this thread; the
// bound-by-cap test reads it. Tests run one per thread, so a
// thread-local is not shared between them.
#[cfg(test)]
thread_local! {
    static DENSE_ALLOCATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The index of the series under `key`, creating it with `label` on
/// first sight.
fn series_index(
    index: &mut HashMap<Vec<String>, usize>,
    series: &mut Vec<Sparse>,
    key: Vec<String>,
    label: String,
) -> usize {
    *index.entry(key).or_insert_with(|| {
        series.push(Sparse {
            label,
            points: HashMap::new(),
            total: 0.0,
        });
        series.len() - 1
    })
}

/// A metric cell as an `f64`, or `None` when it is not a number.
///
/// An `i64` or `u64` past 2^53 loses its low bits here. The chart
/// positions a point a few hundred pixels tall, so the loss cannot move
/// it; the exact digits stay in the Events table.
#[allow(clippy::cast_precision_loss)]
fn numeric(value: &Value) -> Option<f64> {
    match value {
        Value::Integer(i) => Some(*i as f64),
        Value::UInt(u) => Some(*u as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

/// A categorical result on an ordinal axis: one label and one value per
/// group, largest first.
#[derive(Debug, Clone, PartialEq)]
pub struct CatPoints {
    /// Group cells as the table prints them.
    pub labels: Vec<String>,
    /// Metric cells; `None` is a null.
    pub values: Vec<Option<f64>>,
    /// How many groups the result held before [`GROUP_CAP`] applied.
    pub total_groups: usize,
}

/// The points a Column or Bar chart draws for a shape
/// [`crate::categorical::detect`] already admitted.
///
/// Every row is read — the detector ran over the whole fetched result,
/// so this is the whole result too — and the groups are ranked by value
/// descending with nulls last, ties by label, then cut at
/// [`GROUP_CAP`].
#[must_use]
pub fn cat_points(shape: &CatShape, result: &QueryResult) -> CatPoints {
    let mut points: Vec<(String, Option<f64>)> = result
        .rows
        .iter()
        .map(|row| {
            let label = value_to_string(row.get(shape.group).unwrap_or(&Value::Null));
            let value = row.get(shape.metric).and_then(numeric);
            (label, value)
        })
        .collect();
    points.sort_by(|(la, va), (lb, vb)| match (va, vb) {
        (Some(a), Some(b)) => b.total_cmp(a).then_with(|| la.cmp(lb)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => la.cmp(lb),
    });
    let total_groups = points.len();
    points.truncate(GROUP_CAP);
    let (labels, values) = points.into_iter().unzip();
    CatPoints {
        labels,
        values,
        total_groups,
    }
}

/// The line under a chart that drew fewer series or groups than the
/// result holds; `None` when nothing was left out.
///
/// `noun` is `"series"` for Line and `"groups"` for Column and Bar.
/// `narrow` is the `by` fields joined with `", "`, when there are any to
/// suggest narrowing. The copy is asserted by the e2e suite.
#[must_use]
pub fn caption(drawn: usize, total: usize, noun: &str, narrow: Option<&str>) -> Option<String> {
    if drawn >= total {
        return None;
    }
    let omitted = total - drawn;
    let line = if noun == "series" {
        format!("{drawn} of {total} series drawn; the {omitted} smallest by total are not.")
    } else {
        format!("{drawn} of {total} {noun} drawn; the {omitted} smallest are not.")
    };
    Some(match narrow {
        Some(fields) => format!("{line} Narrow {fields}, or open Events."),
        None => line,
    })
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss)] // fixture instants and u64::MAX, compared loosely
mod align_series_tests {
    use super::*;
    use trawl_api::value::Column;

    /// 2026-09-01T00:00:00Z.
    const T0: i64 = 1_788_220_800;

    fn result(names: &[&str], rows: Vec<Vec<Value>>) -> QueryResult {
        QueryResult {
            columns: names
                .iter()
                .map(|n| Column {
                    name: (*n).to_string(),
                })
                .collect(),
            rows,
        }
    }

    /// The snapshot lane, which most tests exercise.
    fn align(query: &str, result: &QueryResult) -> Result<SeriesSet, Refusal> {
        align_series(query, result, Lane::Snapshot)
    }

    fn s(text: &str) -> Value {
        Value::String(text.to_owned())
    }

    /// A snapshot-form `_time` `minutes` past T0.
    fn at(minutes: i64) -> Value {
        let secs = T0 + minutes * 60;
        s(&chrono::DateTime::from_timestamp(secs, 0)
            .unwrap()
            .format("%Y-%m-%d %H:%M:%S")
            .to_string())
    }

    fn series<'a>(set: &'a SeriesSet, label: &str) -> &'a [Option<f64>] {
        &set.series
            .iter()
            .find(|(l, _)| l == label)
            .unwrap_or_else(|| panic!("no series {label}: {:?}", set.series))
            .1
    }

    #[test]
    fn parses_both_admitted_forms() {
        assert_eq!(parse_instant_secs("2026-09-01 00:00:00"), Some(T0));
        assert_eq!(parse_instant_secs("2026-09-01T00:00:00Z"), Some(T0));
        assert_eq!(parse_instant_secs("2026-09-01T00:00:00+00:00"), Some(T0));
        // The live stream's exact spelling, chrono `to_rfc3339()`.
        let live = chrono::DateTime::from_timestamp(T0, 0)
            .unwrap()
            .to_rfc3339();
        assert_eq!(parse_instant_secs(&live), Some(T0), "{live}");
        // The epoch itself, and a date before it.
        assert_eq!(parse_instant_secs("1970-01-01 00:00:00"), Some(0));
        assert_eq!(parse_instant_secs("1969-12-31 23:59:59"), Some(-1));
    }

    #[test]
    fn parses_a_fraction() {
        assert_eq!(parse_instant_secs("2026-09-01 00:00:00.123"), Some(T0));
        assert_eq!(parse_instant_secs("2026-09-01T00:00:00.999Z"), Some(T0));
        assert_eq!(
            parse_instant_secs("2026-09-01T00:00:00.123456789+00:00"),
            Some(T0)
        );
        // A dot with no digits is not a fraction.
        assert_eq!(parse_instant_secs("2026-09-01 00:00:00."), None);
    }

    #[test]
    fn refuses_an_offset_zone() {
        assert_eq!(parse_instant_secs("2026-09-01T00:00:00+02:00"), None);
        assert_eq!(parse_instant_secs("2026-09-01T00:00:00-00:00"), None);
        assert_eq!(
            parse_instant_secs("2026-09-01T00:00:00"),
            None,
            "T form needs a zone"
        );
        assert_eq!(
            parse_instant_secs("2026-09-01 00:00:00Z"),
            None,
            "space form has none"
        );
        assert_eq!(
            parse_instant_secs(" 2026-09-01 00:00:00"),
            None,
            "no trimming"
        );
        assert_eq!(parse_instant_secs("2026-09-01 00:00:00 "), None);
        assert_eq!(parse_instant_secs("2026-09-01 00:00"), None);
        assert_eq!(parse_instant_secs("1788220800"), None);
    }

    #[test]
    fn refuses_an_impossible_date() {
        assert_eq!(parse_instant_secs("2026-13-40 00:00:00"), None);
        assert_eq!(parse_instant_secs("2026-02-30 00:00:00"), None);
        assert_eq!(
            parse_instant_secs("2026-02-29 00:00:00"),
            None,
            "2026 is not a leap year"
        );
        assert!(
            parse_instant_secs("2024-02-29 00:00:00").is_some(),
            "2024 is"
        );
        assert_eq!(
            parse_instant_secs("2100-02-29 00:00:00"),
            None,
            "2100 is not"
        );
        assert!(
            parse_instant_secs("2000-02-29 00:00:00").is_some(),
            "2000 is"
        );
        assert_eq!(parse_instant_secs("2026-04-31 00:00:00"), None);
        assert_eq!(parse_instant_secs("2026-00-01 00:00:00"), None);
        assert_eq!(parse_instant_secs("2026-01-00 00:00:00"), None);
        assert_eq!(parse_instant_secs("2026-01-01 24:00:00"), None);
        assert_eq!(parse_instant_secs("2026-01-01 00:60:00"), None);
        assert_eq!(parse_instant_secs("2026-01-01 00:00:60"), None);
    }

    const BY_HOST: &str = "* | timechart span=1m count() by host";

    #[test]
    fn aligns_disjoint_groups_with_nulls() {
        // `a` has minutes 0 and 2; `b` has minute 1 only.
        let rows = vec![
            vec![at(0), s("a"), Value::Integer(5)],
            vec![at(2), s("a"), Value::Integer(7)],
            vec![at(1), s("b"), Value::Integer(3)],
        ];
        let set = align(BY_HOST, &result(&["_time", "host", "count"], rows)).unwrap();
        assert_eq!(set.xs, vec![T0 as f64, (T0 + 60) as f64, (T0 + 120) as f64]);
        assert_eq!(series(&set, "a"), &[Some(5.0), None, Some(7.0)]);
        assert_eq!(series(&set, "b"), &[None, Some(3.0), None]);
        assert_eq!(set.total_series, 2);
        assert_eq!(set.group_fields, vec!["host".to_owned()]);
    }

    #[test]
    fn a_bucket_absent_for_every_group_is_a_null_column() {
        // Nobody has minute 1: the grid still holds it, as a gap for all.
        let rows = vec![
            vec![at(0), s("a"), Value::Integer(1)],
            vec![at(2), s("a"), Value::Integer(1)],
            vec![at(0), s("b"), Value::Integer(1)],
            vec![at(2), s("b"), Value::Integer(1)],
        ];
        let set = align(BY_HOST, &result(&["_time", "host", "count"], rows)).unwrap();
        assert_eq!(set.xs, vec![T0 as f64, (T0 + 60) as f64, (T0 + 120) as f64]);
        for (_, values) in &set.series {
            assert_eq!(values[1], None);
        }
    }

    #[test]
    fn an_explicit_null_cell_is_a_gap_not_a_zero() {
        let rows = vec![
            vec![at(0), s("a"), Value::Integer(4)],
            vec![at(1), s("a"), Value::Null],
            vec![at(2), s("a"), Value::Float(-1.5)],
        ];
        let set = align(BY_HOST, &result(&["_time", "host", "count"], rows)).unwrap();
        assert_eq!(series(&set, "a"), &[Some(4.0), None, Some(-1.5)]);
    }

    #[test]
    fn shuffled_rows_align_identically() {
        let rows = vec![
            vec![at(3), s("b"), Value::Integer(1)],
            vec![at(0), s("a"), Value::Integer(2)],
            vec![at(2), s("b"), Value::Integer(3)],
            vec![at(1), s("a"), Value::Integer(4)],
            vec![at(5), s("c"), Value::Integer(9)],
        ];
        let forward = align(BY_HOST, &result(&["_time", "host", "count"], rows.clone())).unwrap();
        let mut reversed = rows.clone();
        reversed.reverse();
        let backward = align(BY_HOST, &result(&["_time", "host", "count"], reversed)).unwrap();
        let mut rotated = rows;
        rotated.rotate_left(2);
        let turned = align(BY_HOST, &result(&["_time", "host", "count"], rotated)).unwrap();
        assert_eq!(forward, backward);
        assert_eq!(forward, turned);
        // Minute 4 is nobody's row and sits on the grid regardless.
        assert_eq!(forward.xs.len(), 6);
    }

    #[test]
    fn multi_field_labels_join_with_middot() {
        let query = "* | timechart span=1m count() by host, status";
        let rows = vec![
            vec![at(0), s("web"), Value::Integer(200), Value::Integer(9)],
            vec![at(0), s("web"), Value::Null, Value::Integer(1)],
            vec![at(0), s("db"), Value::Float(1.5), Value::Integer(2)],
        ];
        let set = align(query, &result(&["_time", "host", "status", "count"], rows)).unwrap();
        let labels: Vec<&str> = set.series.iter().map(|(l, _)| l.as_str()).collect();
        // As the table prints them: NULL for a null, two decimals for a
        // float; legend in label order.
        assert_eq!(labels, vec!["db · 1.50", "web · 200", "web · NULL"]);
        assert_eq!(
            set.group_fields,
            vec!["host".to_owned(), "status".to_owned()]
        );
    }

    #[test]
    fn refuses_a_duplicate_bucket_series_row() {
        let rows = vec![
            vec![at(0), s("a"), Value::Integer(1)],
            vec![at(0), s("a"), Value::Integer(2)],
        ];
        let err = align(BY_HOST, &result(&["_time", "host", "count"], rows)).unwrap_err();
        assert_eq!(
            err,
            Refusal::DuplicateCell {
                bucket: "2026-09-01 00:00:00".into(),
                series: "a".into(),
            }
        );
        assert_eq!(
            err.message(),
            "Two rows share the 2026-09-01 00:00:00 bucket for a. Open Events for the exact rows."
        );
        assert!(err.offers_events());
    }

    #[test]
    fn refuses_a_grid_over_twenty_thousand() {
        let query = "* | timechart span=1m count()";
        let far = |minutes: i64| {
            result(
                &["_time", "count"],
                vec![
                    vec![at(0), Value::Integer(1)],
                    vec![at(minutes), Value::Integer(1)],
                ],
            )
        };
        let ok = align(query, &far(19_999)).unwrap();
        assert_eq!(ok.xs.len(), 20_000);
        assert_eq!(ok.series[0].1.len(), 20_000);
        assert_eq!(ok.series[0].1.iter().flatten().count(), 2);

        let err = align(query, &far(20_000)).unwrap_err();
        assert_eq!(err, Refusal::GridTooLarge(20_001));
        assert_eq!(
            err.message(),
            "This result spans 20001 time buckets; the chart draws up to 20,000. Use a larger span or a shorter range."
        );
    }

    #[test]
    fn an_instant_off_the_lattice_is_kept() {
        // A row 30 seconds into the second minute: not on the 1m
        // lattice, still a grid instant, never snapped or dropped.
        let query = "* | timechart span=1m count()";
        let rows = vec![
            vec![at(0), Value::Integer(1)],
            vec![s("2026-09-01 00:01:30"), Value::Integer(2)],
            vec![at(2), Value::Integer(3)],
        ];
        let set = align(query, &result(&["_time", "count"], rows)).unwrap();
        assert_eq!(
            set.xs,
            vec![
                T0 as f64,
                (T0 + 60) as f64,
                (T0 + 90) as f64,
                (T0 + 120) as f64
            ]
        );
        assert_eq!(set.series[0].1, vec![Some(1.0), None, Some(2.0), Some(3.0)]);
    }

    #[test]
    fn ranks_series_by_total_and_caps_at_six() {
        // 14 groups g00..g13, group gNN totalling NN over two buckets;
        // g13 is the largest and g00 the smallest.
        let mut rows = Vec::new();
        for g in 0..14_i64 {
            rows.push(vec![at(0), s(&format!("g{g:02}")), Value::Integer(g)]);
            rows.push(vec![at(1), s(&format!("g{g:02}")), Value::Integer(0)]);
        }
        let set = align(BY_HOST, &result(&["_time", "host", "count"], rows)).unwrap();
        assert_eq!(set.total_series, 14);
        assert_eq!(set.series.len(), SERIES_CAP);
        let labels: Vec<&str> = set.series.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, vec!["g08", "g09", "g10", "g11", "g12", "g13"]);
        assert_eq!(
            caption(set.series.len(), set.total_series, "series", Some("host")),
            Some("6 of 14 series drawn; the 8 smallest by total are not. Narrow host, or open Events.".into())
        );
    }

    #[test]
    fn ranking_ties_break_by_label_and_legend_is_by_label() {
        // Seven series, all totalling 1: the six lowest labels survive.
        let rows: Vec<Vec<Value>> = ["z", "y", "x", "w", "v", "u", "t"]
            .iter()
            .map(|g| vec![at(0), s(g), Value::Integer(1)])
            .collect();
        let set = align(BY_HOST, &result(&["_time", "host", "count"], rows)).unwrap();
        let labels: Vec<&str> = set.series.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, vec!["t", "u", "v", "w", "x", "y"]);
    }

    #[test]
    fn ungrouped_metrics_are_one_series_each() {
        let query = "* | timechart span=1m count(), avg(bytes) as mean, max(bytes)";
        let rows = vec![
            vec![
                at(0),
                Value::Integer(3),
                Value::Float(1.5),
                Value::UInt(u64::MAX),
            ],
            vec![at(1), Value::Integer(4), Value::Null, Value::Integer(-2)],
        ];
        let set = align(
            query,
            &result(&["_time", "count", "mean", "max_bytes"], rows),
        )
        .unwrap();
        assert_eq!(set.total_series, 3);
        assert!(set.group_fields.is_empty());
        assert_eq!(series(&set, "count"), &[Some(3.0), Some(4.0)]);
        assert_eq!(series(&set, "mean"), &[Some(1.5), None]);
        let max = series(&set, "max_bytes");
        assert_eq!(max[1], Some(-2.0));
        assert!((max[0].unwrap() - u64::MAX as f64).abs() < 1.0);
        // A metric column the query did not declare (a later rename)
        // still draws: the fallback is every non-time column.
        let renamed = "* | timechart span=1m count() | rename count as n";
        let rows = vec![vec![at(0), Value::Integer(3)]];
        let set = align(renamed, &result(&["_time", "n"], rows)).unwrap();
        assert_eq!(set.series[0].0, "n");
    }

    #[test]
    fn grouped_two_metrics_refuses() {
        let query = "* | timechart span=1m count(), avg(bytes) by host";
        let rows = vec![vec![at(0), s("a"), Value::Integer(1), Value::Float(2.0)]];
        let err = align(
            query,
            &result(&["_time", "host", "count", "avg_bytes"], rows),
        )
        .unwrap_err();
        assert_eq!(err, Refusal::GroupedTwoMetrics);
        assert_eq!(
            err.message(),
            "Grouped charts draw one metric. Chart one metric, or open Events."
        );
    }

    #[test]
    fn pivot_top_rare_refuse() {
        let rows = vec![vec![at(0), Value::Integer(1)]];
        let r = result(&["_time", "count"], rows);
        let cases = [
            (
                "* | pivot count() on status by host",
                Refusal::Pivot,
                "Pivot",
            ),
            ("* | top 5 host", Refusal::Top, "Top"),
            ("* | rare 5 host", Refusal::Rare, "Rare"),
            // Anywhere in the pipeline, not only last.
            (
                "* | top 5 host | timechart span=1m count()",
                Refusal::Top,
                "Top",
            ),
        ];
        for (query, want, word) in cases {
            let err = align(query, &r).unwrap_err();
            assert_eq!(err, want, "{query}");
            assert_eq!(
                err.message(),
                format!("{word} results are not drawn as lines. Open Events for the table.")
            );
        }
    }

    #[test]
    fn no_time_axis_refuses_toward_column() {
        let rows = vec![vec![s("200"), Value::Integer(1)]];
        let err = align(
            "* | stats count() by status",
            &result(&["status", "count"], rows),
        )
        .unwrap_err();
        assert_eq!(err, Refusal::NoTime);
        assert_eq!(
            err.message(),
            "This result has no time axis. Choose Column for stats by, or open Events."
        );
        // A timechart whose `_time` was projected away has no axis either.
        let rows = vec![vec![Value::Integer(1)]];
        let err = align(
            "* | timechart span=1m count() | table count",
            &result(&["count"], rows),
        )
        .unwrap_err();
        assert_eq!(err, Refusal::NoTime);
    }

    #[test]
    fn the_remaining_rungs_refuse_with_their_sentences() {
        let query = "* | timechart span=1m count()";
        // No metric column at all.
        let err = align(query, &result(&["_time"], vec![vec![at(0)]])).unwrap_err();
        assert_eq!(err, Refusal::NoMetric);
        assert_eq!(
            err.message(),
            "Visualization needs a metric column. Use timechart count(), or open Events."
        );
        // No rows.
        let err = align(query, &result(&["_time", "count"], vec![])).unwrap_err();
        assert_eq!(err, Refusal::Empty);
        assert_eq!(err.message(), "No rows to draw.");
        // A `_time` cell in neither form.
        let rows = vec![vec![s("2026-09-01T00:00:00+02:00"), Value::Integer(1)]];
        let err = align(query, &result(&["_time", "count"], rows)).unwrap_err();
        assert_eq!(
            err,
            Refusal::UnparsedTime("2026-09-01T00:00:00+02:00".into())
        );
        assert_eq!(
            err.message(),
            "A _time value could not be read as a timestamp: 2026-09-01T00:00:00+02:00. Open Events for the exact rows."
        );
        // A metric that is text.
        let rows = vec![vec![at(0), s("many")]];
        let err = align(query, &result(&["_time", "count"], rows)).unwrap_err();
        assert_eq!(err, Refusal::BadMetric("many".into()));
        assert_eq!(
            err.message(),
            "Visualization draws numeric metrics. Open Events for these values."
        );
        assert!(err.offers_events());
    }

    #[test]
    fn the_span_comes_from_the_query_not_the_data() {
        // No explicit span, no `last=`: the resolver says one minute, so
        // two rows five minutes apart get four gaps between them.
        let rows = vec![
            vec![at(0), Value::Integer(1)],
            vec![at(5), Value::Integer(1)],
        ];
        let set = align(
            "* | timechart count()",
            &result(&["_time", "count"], rows.clone()),
        )
        .unwrap();
        assert_eq!(set.xs.len(), 6);
        // `last=7d` resolves to one hour, so the same two rows are two
        // off-lattice instants on a grid of one hour.
        let set = align(
            "last=7d | timechart count()",
            &result(&["_time", "count"], rows),
        )
        .unwrap();
        assert_eq!(set.xs.len(), 2);
    }

    #[test]
    fn cat_points_ranks_and_caps_at_twenty() {
        let query = "* | stats count() by status";
        // 25 groups; s24 the largest, s00 the smallest; one null value.
        let mut rows: Vec<Vec<Value>> = (0..25_i64)
            .map(|i| vec![s(&format!("s{i:02}")), Value::Integer(i)])
            .collect();
        rows.push(vec![s("nul"), Value::Null]);
        rows.reverse();
        let r = result(&["status", "count"], rows);
        let shape = crate::categorical::detect(query, &r).expect("a categorical shape");
        let points = cat_points(&shape, &r);
        assert_eq!(points.total_groups, 26);
        assert_eq!(points.labels.len(), GROUP_CAP);
        assert_eq!(points.values.len(), GROUP_CAP);
        assert_eq!(points.labels[0], "s24");
        assert_eq!(points.values[0], Some(24.0));
        assert_eq!(points.labels[19], "s05");
        assert!(
            !points.labels.contains(&"nul".to_owned()),
            "nulls rank last"
        );

        // Nulls last, ties by label, when everything fits.
        let r = result(
            &["status", "count"],
            vec![
                vec![s("b"), Value::Integer(1)],
                vec![s("n"), Value::Null],
                vec![s("a"), Value::Integer(1)],
            ],
        );
        let shape = crate::categorical::detect(query, &r).unwrap();
        let points = cat_points(&shape, &r);
        assert_eq!(points.labels, vec!["a", "b", "n"]);
        assert_eq!(points.values, vec![Some(1.0), Some(1.0), None]);
        assert_eq!(points.total_groups, 3);
    }

    #[test]
    fn caption_text_matches_the_contract() {
        assert_eq!(caption(6, 6, "series", Some("host")), None);
        assert_eq!(caption(3, 3, "groups", None), None);
        assert_eq!(
            caption(6, 14, "series", Some("host, service")).as_deref(),
            Some(
                "6 of 14 series drawn; the 8 smallest by total are not. Narrow host, service, or open Events."
            )
        );
        assert_eq!(
            caption(6, 7, "series", None).as_deref(),
            Some("6 of 7 series drawn; the 1 smallest by total are not.")
        );
        assert_eq!(
            caption(20, 26, "groups", None).as_deref(),
            Some("20 of 26 groups drawn; the 6 smallest are not.")
        );
    }

    #[test]
    fn dense_storage_is_bounded_by_the_cap() {
        // 5,000 distinct groups, one row each, spread over a grid of
        // 5,000 minutes. A dense-per-group layout would be 25 million
        // cells; the sparse pass keeps one point per group and only the
        // six survivors are made dense, which the allocation counter
        // observes directly.
        let rows: Vec<Vec<Value>> = (0..5_000_i64)
            .map(|g| vec![at(g), s(&format!("g{g:04}")), Value::Integer(g)])
            .collect();
        DENSE_ALLOCATIONS.with(|n| n.set(0));
        let set = align(BY_HOST, &result(&["_time", "host", "count"], rows)).unwrap();
        assert_eq!(set.total_series, 5_000);
        assert_eq!(set.series.len(), SERIES_CAP);
        assert_eq!(set.xs.len(), 5_000);
        assert_eq!(DENSE_ALLOCATIONS.with(std::cell::Cell::get), SERIES_CAP);
        // The largest survive: g4999 down to g4994.
        assert_eq!(set.series[0].0, "g4994");
        assert_eq!(set.series[5].0, "g4999");
        assert_eq!(set.series[5].1[4_999], Some(4_999.0));
    }

    #[test]
    fn distinct_tuples_with_colliding_labels_stay_distinct() {
        let query = "* | timechart span=1m count() by host, service";
        let cols = &["_time", "host", "service", "count"];
        // Same instant: two tuples, one printed label, NOT a duplicate.
        let rows = vec![
            vec![at(0), s("a · b"), s("c"), Value::Integer(1)],
            vec![at(0), s("a"), s("b · c"), Value::Integer(2)],
        ];
        let set = align(query, &result(cols, rows)).unwrap();
        assert_eq!(set.total_series, 2);
        assert_eq!(set.series.len(), 2);
        assert_eq!(set.series[0].0, "a · b · c");
        assert_eq!(set.series[1].0, "a · b · c");
        let mut firsts: Vec<Option<f64>> = set.series.iter().map(|(_, v)| v[0]).collect();
        firsts.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(firsts, vec![Some(1.0), Some(2.0)]);
        // Different instants: still two series, each with its own gap.
        let rows = vec![
            vec![at(0), s("a · b"), s("c"), Value::Integer(1)],
            vec![at(1), s("a"), s("b · c"), Value::Integer(2)],
        ];
        let set = align(query, &result(cols, rows)).unwrap();
        assert_eq!(set.total_series, 2);
        let mut values: Vec<&[Option<f64>]> =
            set.series.iter().map(|(_, v)| v.as_slice()).collect();
        values.sort_by(|a, b| a[0].partial_cmp(&b[0]).unwrap());
        assert_eq!(values, vec![&[None, Some(2.0)][..], &[Some(1.0), None][..]]);
    }

    #[test]
    fn live_lane_uses_the_no_filter_span() {
        let query = "last=4h | timechart count()";
        let rows = vec![
            vec![at(0), Value::Integer(1)],
            vec![at(2), Value::Integer(1)],
        ];
        // Live buckets at one minute whatever `last=` says: minute 1 is
        // a gap between the two rows.
        let live = align_series(
            query,
            &result(&["_time", "count"], rows.clone()),
            Lane::Live,
        )
        .unwrap();
        assert_eq!(live.xs.len(), 3);
        assert_eq!(live.series[0].1, vec![Some(1.0), None, Some(1.0)]);
        // The snapshot emitter resolves `last=4h` to five minutes, so the
        // same rows are two instants on a five-minute grid.
        let snap = align(query, &result(&["_time", "count"], rows)).unwrap();
        assert_eq!(snap.xs.len(), 2);
        assert_eq!(snap.series[0].1, vec![Some(1.0), Some(1.0)]);
    }

    #[test]
    fn a_renamed_metric_is_still_a_metric() {
        let query = "* | timechart span=1m count(), avg(bytes) | rename count as n";
        let rows = vec![vec![at(0), Value::Integer(3), Value::Float(1.5)]];
        let set = align(query, &result(&["_time", "n", "avg_bytes"], rows)).unwrap();
        assert_eq!(set.total_series, 2);
        assert_eq!(series(&set, "n"), &[Some(3.0)]);
        assert_eq!(series(&set, "avg_bytes"), &[Some(1.5)]);
    }

    #[test]
    fn a_renamed_second_metric_still_refuses_when_grouped() {
        let query = "* | timechart span=1m count(), avg(bytes) by host | rename avg_bytes as mean";
        let rows = vec![vec![at(0), s("a"), Value::Integer(3), Value::Float(1.5)]];
        let err = align(query, &result(&["_time", "host", "count", "mean"], rows)).unwrap_err();
        assert_eq!(err, Refusal::GroupedTwoMetrics);
    }

    #[test]
    fn a_null_row_still_counts_toward_a_duplicate() {
        let dup = Refusal::DuplicateCell {
            bucket: "2026-09-01 00:00:00".into(),
            series: "a".into(),
        };
        for pair in [
            (Value::Null, Value::Integer(1)),
            (Value::Integer(1), Value::Null),
            (Value::Null, Value::Null),
        ] {
            let rows = vec![
                vec![at(0), s("a"), pair.0.clone()],
                vec![at(0), s("a"), pair.1.clone()],
            ];
            let err = align(BY_HOST, &result(&["_time", "host", "count"], rows)).unwrap_err();
            assert_eq!(err, dup, "{pair:?}");
        }
    }

    /// The bridge's palette and dash list are the other half of
    /// `SERIES_CAP`: a seventh series would wrap onto the first colour.
    #[test]
    fn palette_covers_the_series_cap() {
        let ts = include_str!("../vendor/src/uplot.ts");

        // `const readColors = () => [ ... ];` — count the top-level
        // comma-separated entries of the array literal.
        let colors = list_after(ts, "const readColors = () => [");
        let color_entries = top_level_entries(colors);
        assert!(
            color_entries >= SERIES_CAP,
            "readColors holds {color_entries} colours, SERIES_CAP is {SERIES_CAP}"
        );

        // `const lineDashes = [ [..], [..], ... ];` — count the inner
        // arrays.
        let dashes = list_after(ts, "const lineDashes = [");
        let dash_entries = top_level_entries(dashes);
        assert!(
            dash_entries >= SERIES_CAP,
            "lineDashes holds {dash_entries} patterns, SERIES_CAP is {SERIES_CAP}"
        );
    }

    /// The text of the array literal that `opener` starts, up to its
    /// matching close bracket.
    fn list_after<'a>(source: &'a str, opener: &str) -> &'a str {
        let start = source
            .find(opener)
            .unwrap_or_else(|| panic!("uplot.ts no longer spells `{opener}`"))
            + opener.len();
        let mut depth = 1_usize;
        for (i, c) in source[start..].char_indices() {
            match c {
                '[' | '(' => depth += 1,
                ']' | ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return &source[start..start + i];
                    }
                }
                _ => {}
            }
        }
        panic!("unterminated list after `{opener}`");
    }

    /// Entries in a bracket list body, counting commas outside nested
    /// brackets and parentheses, ignoring a trailing comma.
    fn top_level_entries(body: &str) -> usize {
        let mut depth = 0_usize;
        let mut commas = 0_usize;
        for c in body.chars() {
            match c {
                '[' | '(' => depth += 1,
                ']' | ')' => depth -= 1,
                ',' if depth == 0 => commas += 1,
                _ => {}
            }
        }
        let trailing = body.trim_end().ends_with(',');
        if body.trim().is_empty() {
            0
        } else if trailing {
            commas
        } else {
            commas + 1
        }
    }
}
