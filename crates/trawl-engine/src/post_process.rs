// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Post-SQL Rust stage execution.
//!
//! When a query pipeline contains stages that can't be expressed as SQL
//! (e.g. `extract kv` with dynamic columns), the emitter splits the
//! pipeline: stages before the boundary run as `DuckDB` SQL, stages at
//! and after the boundary are collected into `EmittedQuery::rust_stages`.
//!
//! This module applies those Rust stages to the SQL result set, reusing
//! the streaming engine's per-event transforms and aggregation machinery.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use chrono::{Offset as _, Utc};
use trawl_core::ast::{PipeStage, SortDirection, Spanned, TrawlDuration};
use trawl_core::compare::Instant;
use trawl_core::context::EvalContext;
use trawl_core::eval::EvalValue;
use trawl_core::pin_scope::PinScope;
use trawl_core::row::{self, Row};
use trawl_core::schema::catalog_key;
use trawl_core::stream::{self, CompiledStage, OutputColumn, StageResult, StreamPlan};

use crate::error::EngineError;
use crate::value::{Column, ColumnType, QueryResult, TypeFold, land_u64};

/// Apply Rust pipeline stages to a SQL result set.
///
/// Converts the columnar `QueryResult` into typed pipeline rows, compiles a
/// stream plan from the given stages, runs it, and converts back. The
/// result's columns are the plan's output schema over the input's
/// ([`StreamPlan::output_schema`]), so a tail that removes every row still
/// answers with the columns it projects, typed as far as the input says
/// ([`tail_cell_type`]), and then any field an `extract kv` found.
///
/// An input with no columns is the SQL read's answer for a source that
/// matched no file, and it is returned as it came, as the same query
/// without the tail returns it: there is no input schema to walk, and no
/// row for the tail to read.
///
/// `pins` is the pin scope stamped at the kv split
/// (`EmittedQuery::rust_stage_pins`), so the tail's `where`/`let` evaluate
/// under the same interpretation the SQL prefix used. Sort stages are
/// partitioned out below without walking the scope; they pass it through
/// unchanged.
///
/// `anchor` is the statement's `now()` instant (`EmittedQuery::anchor`,
/// ADR-0017 §3). Every row — pre-stage, accumulator feed, snapshot,
/// post-stage — evaluates under this one instant, because the tail is
/// part of one unit of output with the SQL prefix that bound it. Nothing
/// here samples a clock.
///
/// `last` is the query's `last=` window (`ast.search.time_filter`). A
/// `timechart` in the tail with no `span=` picks its automatic bucket
/// width from it, through the resolver the SQL emitter uses for the same
/// stage (ADR-0038), so `last=4h | extract kv | timechart count()` cuts
/// the five-minute buckets the same query without the kv split cuts in
/// SQL — and that the web chart lays its grid with — rather than the
/// live stream's one-minute default.
pub fn apply_rust_stages(
    result: QueryResult,
    stages: &[Spanned<PipeStage>],
    pins: &PinScope,
    anchor: EvalContext,
    last: Option<TrawlDuration>,
) -> Result<QueryResult, EngineError> {
    if stages.is_empty() {
        return Ok(result);
    }

    let events = rows_to_events(&result);

    // Separate sort stages from the plan — the streaming compiler rejects
    // them (they contradict real-time arrival order) but they're valid in
    // batch post-processing.
    let (plan_stages, sort_stages): (Vec<_>, Vec<_>) = stages
        .iter()
        .cloned()
        .partition(|s| !matches!(s.node, PipeStage::Sort(_)));

    let plan = stream::compile_stream_plan(&plan_stages, pins, last).map_err(|e| {
        EngineError::Emit(trawl_core::emitter::EmitError::UnsupportedOperation {
            message: format!("post-processing: {e}"),
        })
    })?;
    if result.columns.is_empty() {
        return Ok(result);
    }
    // Sort stages order rows and leave the columns alone.
    let schema = plan.output_schema(
        result
            .columns
            .iter()
            .map(|column| OutputColumn::new(column.name.clone(), Some(tail_cell_type(&column.ty))))
            .collect(),
    );

    let processed = match plan {
        StreamPlan::PassThrough(mut compiled_stages) => {
            apply_pass_through(&mut compiled_stages, events, &anchor)
        }
        StreamPlan::Aggregate {
            mut pre_stages,
            mut aggregation,
            mut post_stages,
        } => apply_aggregate(
            &mut pre_stages,
            &mut aggregation,
            &mut post_stages,
            events,
            &anchor,
        )?,
    };

    // Apply deferred sort stages.
    let sorted = apply_sorts(processed, &sort_stages);

    Ok(rows_to_result(&sorted, &schema))
}

/// The type the tail's own cells give an input column of type `ty`.
///
/// The SQL read hands the tail DATE, TIME and TIMESTAMP cells as text
/// ([`crate::executor`] renders them before the tail reads them), so such
/// a column, or a list of them, passes through the tail as `string` cells;
/// every other type passes through as itself. Seeding the output schema
/// with these types keeps an empty answer typed as the same answer with
/// rows would be. A timestamp the tail passes through is typed back after
/// the tail by its lineage, rows or none (`render_tracked_timestamps`).
fn tail_cell_type(ty: &ColumnType) -> ColumnType {
    match ty {
        ColumnType::Date | ColumnType::Time | ColumnType::Timestamp => ColumnType::String,
        ColumnType::List { items } => ColumnType::list(tail_cell_type(items)),
        ColumnType::Boolean
        | ColumnType::Integer
        | ColumnType::Float
        | ColumnType::String
        | ColumnType::Mixed => ty.clone(),
    }
}

/// Convert a columnar `QueryResult` into a vec of typed pipeline rows.
fn rows_to_events(result: &QueryResult) -> Vec<Row> {
    result
        .rows
        .iter()
        .map(|row| {
            result
                .columns
                .iter()
                .zip(row.iter())
                .map(|(col, cell)| (col.name.clone(), cell_to_eval(cell)))
                .collect()
        })
        .collect()
}

/// A result cell as a pipeline cell — a direct bridge, variant for variant.
///
/// Deliberately not a `serde_json` hop: JSON has no spelling for a
/// non-finite double, so a SQL prefix computing `1.0/0` would hand the
/// `extract kv` tail a null and the tail would count, compare and print
/// something the same query without the kv split never sees. Both
/// directions are exhaustive with no `_` arm, so a new variant on either
/// side is a compile error rather than a silent NULL.
fn cell_to_eval(cell: &crate::value::Value) -> EvalValue {
    match cell {
        crate::value::Value::Null => EvalValue::Null,
        crate::value::Value::Boolean(b) => EvalValue::Bool(*b),
        crate::value::Value::Integer(i) => EvalValue::Int(*i),
        crate::value::Value::UInt(u) => EvalValue::UInt(*u),
        crate::value::Value::Float(f) => EvalValue::Float(*f),
        crate::value::Value::String(s) => EvalValue::Str(s.clone()),
        crate::value::Value::Array(arr) => EvalValue::Array(arr.iter().map(cell_to_eval).collect()),
    }
}

/// A pipeline cell as a result cell.
///
/// A non-finite double survives as `Value::Float`, and the wire spells it
/// as `NaN`, `Infinity` or `-Infinity` under its `float` column, the same
/// as the all-SQL path. Nulling it here would make the kv tail disagree
/// with the same query without the split.
///
/// A `Timestamp` the tail computed becomes the wire's RFC 3339 text in
/// UTC (`Z`), the spelling every `timestamp` cell has: it is the tail's own
/// instant, not a stored column the display offset applies to, so it says
/// which offset it is in. Its column is typed `timestamp`
/// ([`rows_to_result`] folds the pipeline cell, not this one).
fn eval_to_cell(cell: &EvalValue) -> crate::value::Value {
    match cell {
        EvalValue::Null => crate::value::Value::Null,
        EvalValue::Bool(b) => crate::value::Value::Boolean(*b),
        EvalValue::Int(i) => crate::value::Value::Integer(*i),
        // `Value::Integer` is signed, so a magnitude above `i64::MAX`
        // lands in `Value::UInt` instead — the rule `trawl_api::value`
        // owns for every decoder on this wire. A kv tail must not answer
        // with a rounded reading of a number the SQL prefix would have
        // spelled exactly.
        EvalValue::UInt(u) => land_u64(*u),
        EvalValue::Float(f) => crate::value::Value::Float(*f),
        EvalValue::Str(s) => crate::value::Value::String(s.clone()),
        EvalValue::Timestamp(instant) => crate::value::Value::String(match instant {
            Instant::NegInfinity => "-infinity".to_owned(),
            Instant::At(at) => {
                crate::executor::wire_timestamp(at.and_utc().timestamp_micros(), Utc.fix())
            }
            Instant::Infinity => "infinity".to_owned(),
        }),
        EvalValue::Array(arr) => crate::value::Value::Array(arr.iter().map(eval_to_cell).collect()),
    }
}

/// The unsigned bridge arm lands the number, not a rounded double, and
/// picks the narrower signed variant when that holds the value.
#[cfg(test)]
#[test]
fn unsigned_arm() {
    assert_eq!(
        eval_to_cell(&EvalValue::UInt(u64::MAX)),
        crate::value::Value::UInt(u64::MAX)
    );
    assert_eq!(
        eval_to_cell(&EvalValue::UInt(5)),
        crate::value::Value::Integer(5)
    );
}

/// Add one pipeline cell's type to a column's evidence.
///
/// The pipeline cell, not the result cell it becomes: an
/// [`EvalValue::Timestamp`] is text in the result, but its column is a
/// `timestamp` column.
fn observe_cell(fold: &mut TypeFold, cell: &EvalValue) {
    let ty = match cell {
        EvalValue::Null => return,
        EvalValue::Bool(_) => ColumnType::Boolean,
        EvalValue::Int(_) | EvalValue::UInt(_) => ColumnType::Integer,
        EvalValue::Float(_) => ColumnType::Float,
        EvalValue::Str(_) => ColumnType::String,
        EvalValue::Timestamp(_) => ColumnType::Timestamp,
        EvalValue::Array(elements) => {
            let mut items = TypeFold::new();
            for element in elements {
                observe_cell(&mut items, element);
            }
            fold.observe_list(items);
            return;
        }
    };
    fold.observe(&ty);
}

/// Convert pipeline rows to a typed, columnar [`QueryResult`].
///
/// The one builder for every result a Rust-side stage produces: the batch
/// tail behind `extract kv`, and the live lane's frames and snapshots.
/// The columns are `schema`'s, in its order, then every other key the rows
/// carry, in the order first seen; a missing key is NULL. The schema is
/// the stages' statically known output ([`StreamPlan::output_schema`]):
/// it orders the columns, which a [`Row`] keeps sorted, and it is the
/// whole answer when no row survived.
///
/// A row key lands in the column of its exact name, else in the one it
/// matches ASCII case-insensitively, so one name is one column: the stages
/// bind names that way, and a `table N` keeps the `n` an `extract kv`
/// produced under the row's own spelling. A row holding two case variants
/// fills the column from the exact one, else from the first in key order,
/// as a stage reading the name would.
///
/// Each column is typed from the cells it holds (ADR-0035, amended
/// 2026-10-22): one type of cell gives that type, several give `mixed`,
/// and list items fold the same way. The type its schema column carries
/// fills what the cells leave open ([`TypeFold::finish_with`]): a column
/// with no non-null cell takes it whole, and a list whose cells are all
/// empty or null takes its items, so a stored `BIGINT[]` with nothing in
/// it is still `list` of `integer` after the tail. A column that carries
/// nothing and shows nothing is `string`.
pub fn rows_to_result(rows: &[Row], schema: &[OutputColumn<ColumnType>]) -> QueryResult {
    let mut names: Vec<String> = Vec::new();
    let mut carried: Vec<Option<ColumnType>> = Vec::new();
    // Each column by its ASCII-folded name.
    let mut index: HashMap<String, usize> = HashMap::new();
    let known = schema
        .iter()
        .map(|column| (column.name.as_str(), column.carries.clone()));
    let seen = rows
        .iter()
        .flat_map(|row| row.keys().map(|key| (key.as_str(), None)));
    for (name, carries) in known.chain(seen) {
        if let Entry::Vacant(slot) = index.entry(catalog_key(name)) {
            slot.insert(names.len());
            names.push(name.to_owned());
            carried.push(carries);
        }
    }

    let mut folds = vec![TypeFold::new(); names.len()];
    let rows: Vec<Vec<crate::value::Value>> = rows
        .iter()
        .map(|row| {
            let mut cells: Vec<Option<&EvalValue>> = vec![None; names.len()];
            for (key, cell) in row {
                let at = index[&catalog_key(key)];
                if cells[at].is_none() || *key == names[at] {
                    cells[at] = Some(cell);
                }
            }
            cells
                .into_iter()
                .zip(folds.iter_mut())
                .map(|(cell, fold)| {
                    let cell = cell.unwrap_or(&EvalValue::Null);
                    observe_cell(fold, cell);
                    eval_to_cell(cell)
                })
                .collect()
        })
        .collect();

    let columns = names
        .into_iter()
        .zip(carried)
        .zip(folds)
        .map(|((name, carries), fold)| {
            Column::new(
                name,
                fold.finish_with(carries.as_ref())
                    .unwrap_or(ColumnType::String),
            )
        })
        .collect();

    QueryResult { columns, rows }
}

/// Run pass-through stages on each event.
fn apply_pass_through(
    stages: &mut [CompiledStage],
    events: Vec<Row>,
    anchor: &EvalContext,
) -> Vec<Row> {
    let mut output = Vec::new();
    'event: for mut event in events {
        for stage in stages.iter_mut() {
            match stream::apply_stage(stage, &mut event, anchor) {
                StageResult::Pass => {}
                StageResult::Filtered => continue 'event,
                StageResult::Done => return output,
            }
        }
        output.push(event);
    }
    output
}

/// Run an aggregation plan: pre-stages → accumulate → snapshot → post-stages.
///
/// Refuses with [`EngineError::AggregationCapped`] when the accumulators
/// dropped input at a cap: the live lane can report a partial answer on
/// its snapshot, but a batch answer cannot, so it gives none (ADR-0001).
fn apply_aggregate(
    pre_stages: &mut [CompiledStage],
    aggregation: &mut stream::CompiledAggregation,
    post_stages: &mut [CompiledStage],
    events: Vec<Row>,
    anchor: &EvalContext,
) -> Result<Vec<Row>, EngineError> {
    // Feed events through pre-stages into the aggregation.
    'event: for mut event in events {
        for stage in pre_stages.iter_mut() {
            match stream::apply_stage(stage, &mut event, anchor) {
                StageResult::Pass => {}
                StageResult::Filtered => continue 'event,
                // `Done` ends the input, not just this event's stage
                // run: breaking only the inner loop would feed the
                // accumulator the very event an exhausted `limit` just
                // refused, and then every event after it, so
                // `extract kv | limit 1 | stats count()` would count
                // every row. Exhaustion is sticky, so no later event
                // could be admitted anyway. Same rule as the post-stage
                // loop below.
                StageResult::Done => break 'event,
            }
        }
        // One statement anchor for every row, the pre-stages and the
        // accumulator feed alike (ADR-0017 §3).
        aggregation.feed_event(&event, anchor);
    }
    if let Some(&cap) = aggregation.capped().first() {
        return Err(EngineError::AggregationCapped(cap));
    }

    // Snapshot the aggregation results.
    let (_columns, snapshot_rows) = aggregation.snapshot();

    // Apply post-stages to snapshot rows.
    let mut output = Vec::new();
    'row: for mut row in snapshot_rows {
        for stage in post_stages.iter_mut() {
            match stream::apply_stage(stage, &mut row, anchor) {
                StageResult::Pass => {}
                StageResult::Filtered => continue 'row,
                StageResult::Done => return Ok(output),
            }
        }
        output.push(row);
    }
    Ok(output)
}

/// Apply deferred sort stages to the event list.
///
/// Sort is rejected by the streaming compiler (contradicts arrival order)
/// but is valid in batch post-processing.
fn apply_sorts(mut events: Vec<Row>, sort_stages: &[Spanned<PipeStage>]) -> Vec<Row> {
    for stage in sort_stages {
        if let PipeStage::Sort(sort) = &stage.node {
            events.sort_by(|a, b| {
                for field in &sort.fields {
                    let key = field.field.as_str();
                    let va = a.get(key);
                    let vb = b.get(key);
                    let cmp = compare_cells(va, vb);
                    let cmp = match field.direction {
                        SortDirection::Asc => cmp,
                        SortDirection::Desc => cmp.reverse(),
                    };
                    if cmp != std::cmp::Ordering::Equal {
                        return cmp;
                    }
                }
                std::cmp::Ordering::Equal
            });
        }
    }
    events
}

/// Compare two optional cells for sorting purposes.
///
/// NULLs sort last. Two numbers compare numerically and exactly: an
/// integer pair never becomes a pair of doubles, so `u64::MAX` and
/// `u64::MAX - 1` keep their order instead of rounding to the same
/// reading. Only a pair that already holds a DOUBLE reaches floating
/// point, and a DOUBLE pair goes through the probed DOUBLE order
/// (`compare::double_total_cmp`) rather than
/// `partial_cmp(…).unwrap_or(Equal)`, which would leave a NaN wherever
/// arrival order happened to put it. Everything else compares as the text
/// the cell shows (`row::cell_text`, the same renderer the live lane groups
/// by).
fn compare_cells(a: Option<&EvalValue>, b: Option<&EvalValue>) -> std::cmp::Ordering {
    match (a, b) {
        (None | Some(EvalValue::Null), None | Some(EvalValue::Null)) => std::cmp::Ordering::Equal,
        (None | Some(EvalValue::Null), _) => std::cmp::Ordering::Greater, // NULLs last
        (_, None | Some(EvalValue::Null)) => std::cmp::Ordering::Less,
        (Some(a), Some(b)) => {
            // Try numeric comparison first.
            if let Some(ordering) = compare_numeric(a, b) {
                return ordering;
            }
            // Fall back to the cell's own text.
            row::cell_text(a).cmp(&row::cell_text(b))
        }
    }
}

/// Order two numeric cells, or answer `None` when either side is not a
/// number.
///
/// These are the rules `result_actions::compare` already encodes for the
/// results table in `trawl-web-ui`, so a column sorted by the browser and
/// the same column sorted by `| sort` read the same way for every value the
/// JSON hop can carry (a non-finite DOUBLE lands as null there; only the
/// in-memory NaN sign rule differs, see `int_float`). Every integer is
/// a point on one line, so the signed and unsigned variants compare as the
/// numbers they are through `i128` — the narrowest type holding both
/// ranges — rather than by variant rank. Nothing but a pair that already
/// holds a DOUBLE is allowed near `f64`.
fn compare_numeric(a: &EvalValue, b: &EvalValue) -> Option<std::cmp::Ordering> {
    use EvalValue::{Float, Int, UInt};
    Some(match (a, b) {
        (Int(a), Int(b)) => a.cmp(b),
        (UInt(a), UInt(b)) => a.cmp(b),
        (Int(a), UInt(b)) => i128::from(*a).cmp(&i128::from(*b)),
        (UInt(a), Int(b)) => i128::from(*a).cmp(&i128::from(*b)),
        (Float(a), Float(b)) => trawl_core::compare::double_total_cmp(*a, *b),
        (Int(a), Float(b)) => int_float(*a, *b),
        (Float(a), Int(b)) => int_float(*b, *a).reverse(),
        (UInt(a), Float(b)) => uint_float(*a, *b),
        (Float(a), UInt(b)) => uint_float(*b, *a).reverse(),
        _ => return None,
    })
}

/// Order an `i64` against an `f64` without rounding the integer.
///
/// The shape is `result_actions::int_float`'s: bound the double against
/// the integer's range, truncate it, then refine on the fraction the
/// truncation dropped. NaN sorts last whatever its sign bit says, which is
/// where `compare::double_total_cmp` puts it for a DOUBLE pair; a
/// comparator that answered differently depending on which pairing it was
/// asked about would not be an ordering at all.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn int_float(i: i64, f: f64) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    if f.is_nan() || f >= 9_223_372_036_854_775_808.0 {
        return Ordering::Less;
    }
    if f < -9_223_372_036_854_775_808.0 {
        return Ordering::Greater;
    }
    let whole = f as i64;
    i.cmp(&whole)
        .then_with(|| (whole as f64).partial_cmp(&f).unwrap_or(Ordering::Equal))
}

/// The unsigned twin of [`int_float`], for the range above `i64::MAX`.
///
/// Same shape, same NaN placement, same truncate-then-refine tie-break;
/// only the bounds move, because a `u64` cannot be negative and reaches
/// twice as far up.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn uint_float(u: u64, f: f64) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    if f.is_nan() || f >= 18_446_744_073_709_551_616.0 {
        return Ordering::Less;
    }
    if f < 0.0 {
        return Ordering::Greater;
    }
    let whole = f as u64;
    u.cmp(&whole)
        .then_with(|| (whole as f64).partial_cmp(&f).unwrap_or(Ordering::Equal))
}

/// A `sort` in the Rust tail orders an oversized unsigned by value.
///
/// `u64::MAX` and `u64::MAX - 1` are the same `f64`, so a comparator that
/// read every number as a double called the two rows equal and left them
/// in whatever order they arrived in. The four rows here span both
/// integer variants and a DOUBLE, so they only come back in this order if
/// each pair took its own exact arm.
#[cfg(test)]
#[test]
fn rust_tail_sort_orders_large_unsigned_exactly() {
    use crate::value::Value;

    let tail = trawl_core::parser::parse("* | extract kv | sort bytes")
        .expect("dsl parses")
        .pipeline;
    let result = QueryResult {
        columns: vec![Column::new("bytes", ColumnType::Mixed)],
        rows: vec![
            vec![Value::UInt(u64::MAX)],
            vec![Value::UInt(u64::MAX - 1)],
            vec![Value::Integer(-1)],
            vec![Value::Float(1.5)],
        ],
    };
    let anchor = EvalContext::at(
        chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .expect("a valid RFC 3339 instant")
            .into(),
    );

    let sorted = apply_rust_stages(result, &tail, &PinScope::unpinned(), anchor, None)
        .expect("the tail runs a bare sort over a kv split");

    assert_eq!(
        sorted.rows,
        vec![
            vec![Value::Integer(-1)],
            vec![Value::Float(1.5)],
            vec![Value::UInt(u64::MAX - 1)],
            vec![Value::UInt(u64::MAX)],
        ],
        "the two neighbouring unsigned maxima must not collapse into one reading"
    );
}

/// The batch tail behind `extract kv` goes through the same streaming
/// compiler the live lane does, so a `timechart` naming a bucket column
/// is refused there too — and the refusal reaches the caller as the
/// 400-class `Emit` error, naming the lane and the column.
///
/// The other door into that compiler,
/// `compile_stream_plan`, is pinned by
/// `stream::timechart_on_refused_in_live_lane` in trawl-core, which
/// cannot reach this function: trawl-engine depends on that crate, not
/// the other way round.
#[cfg(test)]
#[test]
fn timechart_on_refused_in_batch_tail() {
    let tail = trawl_core::parser::parse("* | extract kv | timechart on hostname span=5m count()")
        .expect("dsl parses")
        .pipeline;
    let result = QueryResult {
        columns: vec![Column::new("_raw", ColumnType::String)],
        rows: Vec::new(),
    };
    let anchor = EvalContext::at(
        chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .expect("a valid RFC 3339 instant")
            .into(),
    );

    let err = apply_rust_stages(result, &tail, &PinScope::unpinned(), anchor, None)
        .expect_err("a bucket column other than _time has no meaning in the tail");
    match err {
        EngineError::Emit(trawl_core::emitter::EmitError::UnsupportedOperation { message }) => {
            // Pinned whole: a `contains` on one fragment hides both a
            // doubled clause and a run of stray spaces inside the
            // sentence.
            assert_eq!(
                message,
                "post-processing: timechart is not supported in streaming mode: \
                 bucketing on 'hostname' is not supported here; \
                 only _time can be bucketed live"
            );
        }
        other => panic!("expected the 400-class refusal, got {other:?}"),
    }
}

/// A `timechart` with no `span=` in the tail cuts its buckets from the
/// query's `last=` window, as the SQL emitter would for the same stage:
/// `last=4h` is the five-minute band, so two rows a minute apart share a
/// bucket that the live default of one minute would split.
#[cfg(test)]
#[test]
fn tail_timechart_span_follows_the_query_window() {
    use crate::value::Value;

    let query =
        trawl_core::parser::parse("last=4h | extract kv | timechart count()").expect("dsl parses");
    let tail = &query.pipeline;
    let last = query.search.time_filter.as_ref().map(|tf| tf.node.duration);
    let rows = || QueryResult {
        columns: vec![
            Column::new("_time", ColumnType::Timestamp),
            Column::new("message", ColumnType::String),
        ],
        rows: vec![
            vec![
                Value::String("2026-01-01T00:01:00+00:00".into()),
                Value::String("k=v".into()),
            ],
            vec![
                Value::String("2026-01-01T00:02:00+00:00".into()),
                Value::String("k=v".into()),
            ],
        ],
    };
    let anchor = || {
        EvalContext::at(
            chrono::DateTime::parse_from_rfc3339("2026-01-01T00:10:00Z")
                .expect("a valid RFC 3339 instant")
                .into(),
        )
    };

    let windowed = apply_rust_stages(rows(), tail, &PinScope::unpinned(), anchor(), last)
        .expect("the tail runs a timechart over a kv split");
    let time = |out: &QueryResult, row: usize| {
        let idx = out.columns.iter().position(|c| c.name == "_time").unwrap();
        out.rows[row][idx].clone()
    };
    assert_eq!(windowed.rows.len(), 1, "{:?}", windowed.rows);
    // The bucket is a TIMESTAMP, as the SQL lane's `time_bucket` is.
    assert_eq!(
        time(&windowed, 0),
        Value::String("2026-01-01T00:00:00.000000Z".into())
    );
    assert!(
        windowed
            .columns
            .contains(&Column::new("_time", ColumnType::Timestamp)),
        "{:?}",
        windowed.columns
    );

    // The live answer for the same stages: no window, one-minute buckets.
    let live = apply_rust_stages(rows(), tail, &PinScope::unpinned(), anchor(), None)
        .expect("the tail runs a timechart over a kv split");
    assert_eq!(live.rows.len(), 2, "{:?}", live.rows);
}

#[cfg(test)]
mod tests {
    use super::*;
    use trawl_core::ast::*;

    fn span<T>(node: T) -> Spanned<T> {
        Spanned { node, span: 0..0 }
    }

    fn make_result(columns: &[&str], rows: Vec<Vec<crate::value::Value>>) -> QueryResult {
        QueryResult {
            columns: columns
                .iter()
                .map(|n| Column::new(*n, ColumnType::String))
                .collect(),
            rows,
        }
    }

    /// The bridge is an identity on every cell shape a result can hold,
    /// specials included: a JSON hop would turn `inf` and `NaN` into NULL
    /// on the way in.
    #[test]
    fn the_bridge_round_trips_every_cell_shape() {
        let cells = [
            crate::value::Value::Null,
            crate::value::Value::Boolean(true),
            crate::value::Value::Boolean(false),
            crate::value::Value::Integer(i64::MIN),
            crate::value::Value::Integer(0),
            crate::value::Value::Integer(i64::MAX),
            crate::value::Value::Float(1.5),
            crate::value::Value::Float(-0.0),
            crate::value::Value::Float(f64::INFINITY),
            crate::value::Value::Float(f64::NEG_INFINITY),
            crate::value::Value::Float(f64::NAN),
            crate::value::Value::Float(-f64::NAN),
            crate::value::Value::String(String::new()),
            crate::value::Value::String("nginx".into()),
            crate::value::Value::Array(vec![
                crate::value::Value::Integer(1),
                crate::value::Value::Float(f64::INFINITY),
                crate::value::Value::String("x".into()),
            ]),
        ];
        for cell in cells {
            let round_tripped = eval_to_cell(&cell_to_eval(&cell));
            match (&cell, &round_tripped) {
                // NaN is never equal to itself, so the pair is compared
                // bitwise — sign bit included, since DuckDB renders it.
                (crate::value::Value::Float(a), crate::value::Value::Float(b)) => {
                    assert_eq!(a.to_bits(), b.to_bits(), "{cell:?}");
                }
                _ => assert_eq!(cell, round_tripped, "{cell:?}"),
            }
        }
    }

    /// A `Timestamp` cell — the one shape with no result-side twin — lands
    /// as the wire's RFC 3339 text in UTC, and an infinity as its word.
    #[test]
    fn a_timestamp_cell_leaves_the_bridge_as_wire_text() {
        let ts = chrono::NaiveDate::from_ymd_opt(2026, 1, 15)
            .unwrap()
            .and_hms_micro_opt(9, 0, 0, 120_000)
            .unwrap();
        for (instant, text) in [
            (
                trawl_core::compare::Instant::At(ts),
                "2026-01-15T09:00:00.120000Z",
            ),
            (trawl_core::compare::Instant::Infinity, "infinity"),
            (trawl_core::compare::Instant::NegInfinity, "-infinity"),
        ] {
            assert_eq!(
                eval_to_cell(&EvalValue::Timestamp(instant)),
                crate::value::Value::String(text.into())
            );
        }
    }

    /// The schema fixes the column order a sorted [`Row`] cannot, keeps a
    /// schema column no row carries (typed by what it carries, else
    /// `string`), and every other key follows it.
    #[test]
    fn the_schema_leads_the_column_order() {
        let row = |cells: &[(&str, EvalValue)]| -> Row {
            cells
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect()
        };
        let schema = [
            OutputColumn::new("host", None),
            OutputColumn::new("count", None),
            OutputColumn::new("gone", None),
            OutputColumn::new("rate", Some(ColumnType::Float)),
        ];
        let result = rows_to_result(
            &[row(&[
                ("count", EvalValue::UInt(u64::MAX)),
                ("host", EvalValue::Str("web-1".into())),
                ("extra", EvalValue::Bool(true)),
            ])],
            &schema,
        );
        assert_eq!(
            result.columns,
            vec![
                Column::new("host", ColumnType::String),
                Column::new("count", ColumnType::Integer),
                Column::new("gone", ColumnType::String),
                Column::new("rate", ColumnType::Float),
                Column::new("extra", ColumnType::Boolean),
            ]
        );
        assert_eq!(
            result.rows,
            vec![vec![
                crate::value::Value::String("web-1".into()),
                crate::value::Value::UInt(u64::MAX),
                crate::value::Value::Null,
                crate::value::Value::Null,
                crate::value::Value::Boolean(true),
            ]]
        );

        let empty = rows_to_result(&[], &schema);
        assert_eq!(
            empty.columns,
            vec![
                Column::new("host", ColumnType::String),
                Column::new("count", ColumnType::String),
                Column::new("gone", ColumnType::String),
                Column::new("rate", ColumnType::Float),
            ]
        );
        assert!(empty.rows.is_empty());
    }

    /// One name is one column: a row key lands in the column it names
    /// ASCII case-insensitively, under the spelling first listed, and a
    /// row holding both spellings fills it from the exact one.
    #[test]
    fn a_case_variant_key_lands_in_its_column() {
        let row = |cells: &[(&str, EvalValue)]| -> Row {
            cells
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect()
        };
        let result = rows_to_result(
            &[
                row(&[("n", EvalValue::Int(1))]),
                row(&[("N", EvalValue::Int(2)), ("Other", EvalValue::Int(3))]),
                row(&[("other", EvalValue::Int(4))]),
                row(&[("N", EvalValue::Int(5)), ("n", EvalValue::Int(6))]),
            ],
            &[OutputColumn::new("N", None)],
        );
        assert_eq!(
            result.columns,
            vec![
                Column::new("N", ColumnType::Integer),
                Column::new("Other", ColumnType::Integer),
            ]
        );
        let int = crate::value::Value::Integer;
        assert_eq!(
            result.rows,
            vec![
                vec![int(1), crate::value::Value::Null],
                vec![int(2), int(3)],
                vec![crate::value::Value::Null, int(4)],
                vec![int(5), crate::value::Value::Null],
            ]
        );
    }

    #[test]
    fn kv_extraction_on_result() {
        let result = make_result(
            &["message"],
            vec![
                vec![crate::value::Value::String("user=alice status=200".into())],
                vec![crate::value::Value::String("user=bob status=404".into())],
            ],
        );

        let stages = vec![span(PipeStage::Extract(ExtractStage {
            mode: ExtractMode::KeyValue { separator: '=' },
            source_field: None, // defaults to "message"
            keyword: "extract",
        }))];

        let out = apply_rust_stages(
            result,
            &stages,
            &PinScope::unpinned(),
            EvalContext::capture(),
            None,
        )
        .unwrap();
        assert_eq!(out.columns.len(), 3); // message, user, status
        assert_eq!(out.rows.len(), 2);

        // Check first row values.
        let col_idx = |name: &str| out.columns.iter().position(|c| c.name == name).unwrap();
        assert_eq!(
            out.rows[0][col_idx("user")],
            crate::value::Value::String("alice".into())
        );
        assert_eq!(
            out.rows[0][col_idx("status")],
            crate::value::Value::Integer(200)
        );
    }

    #[test]
    fn kv_with_where_filter() {
        let result = make_result(
            &["message"],
            vec![
                vec![crate::value::Value::String("method=GET status=200".into())],
                vec![crate::value::Value::String("method=POST status=500".into())],
                vec![crate::value::Value::String("method=GET status=201".into())],
            ],
        );

        let stages = vec![
            span(PipeStage::Extract(ExtractStage {
                mode: ExtractMode::KeyValue { separator: '=' },
                source_field: None,
                keyword: "extract",
            })),
            span(PipeStage::Where(WhereStage {
                condition: span(Expr::Binary {
                    lhs: Box::new(span(Expr::FieldRef("status".into()))),
                    op: BinaryOp::Gte,
                    rhs: Box::new(span(Expr::Literal(LiteralValue::Int(400)))),
                }),
            })),
        ];

        let out = apply_rust_stages(
            result,
            &stages,
            &PinScope::unpinned(),
            EvalContext::capture(),
            None,
        )
        .unwrap();
        assert_eq!(out.rows.len(), 1);
        let col_idx = |name: &str| out.columns.iter().position(|c| c.name == name).unwrap();
        assert_eq!(
            out.rows[0][col_idx("method")],
            crate::value::Value::String("POST".into())
        );
    }

    /// The kv tail is the only lane behind `extract kv`: a `let` sibling
    /// reference naming no column of the pre-stage row binds the sibling's
    /// value here exactly as `DuckDB`'s lateral column alias does in the
    /// SQL lane, so `| extract kv | let ms = 1000, total = ms * 2` cannot
    /// answer NULL where the same pipeline without `extract kv` answers
    /// 2000.
    #[test]
    fn kv_tail_let_sibling_binds_the_alias() {
        let result = make_result(
            &["message"],
            vec![vec![crate::value::Value::String("method=GET".into())]],
        );

        let stages = vec![
            span(PipeStage::Extract(ExtractStage {
                mode: ExtractMode::KeyValue { separator: '=' },
                source_field: None,
                keyword: "extract",
            })),
            span(PipeStage::Let(LetStage {
                assignments: vec![
                    ("ms".into(), span(Expr::Literal(LiteralValue::Int(1000)))),
                    (
                        "total".into(),
                        span(Expr::Binary {
                            lhs: Box::new(span(Expr::FieldRef("ms".into()))),
                            op: BinaryOp::Mul,
                            rhs: Box::new(span(Expr::Literal(LiteralValue::Int(2)))),
                        }),
                    ),
                ],
                keyword: "let",
            })),
        ];

        let out = apply_rust_stages(
            result,
            &stages,
            &PinScope::unpinned(),
            EvalContext::capture(),
            None,
        )
        .unwrap();
        let col_idx = |name: &str| out.columns.iter().position(|c| c.name == name).unwrap();
        assert_eq!(
            out.rows[0][col_idx("total")],
            crate::value::Value::Integer(2000)
        );
    }

    #[test]
    fn kv_with_stats() {
        let result = make_result(
            &["message"],
            vec![
                vec![crate::value::Value::String("method=GET status=200".into())],
                vec![crate::value::Value::String("method=POST status=500".into())],
                vec![crate::value::Value::String("method=GET status=201".into())],
            ],
        );

        let stages = vec![
            span(PipeStage::Extract(ExtractStage {
                mode: ExtractMode::KeyValue { separator: '=' },
                source_field: None,
                keyword: "extract",
            })),
            span(PipeStage::Stats(StatsStage {
                aggregations: vec![AggExpr {
                    function: "count".into(),
                    args: vec![],
                    alias: None,
                }],
                group_by: vec!["method".into()],
            })),
        ];

        let out = apply_rust_stages(
            result,
            &stages,
            &PinScope::unpinned(),
            EvalContext::capture(),
            None,
        )
        .unwrap();
        assert_eq!(out.rows.len(), 2);

        // Find GET row — should have count=2.
        let col_idx = |name: &str| out.columns.iter().position(|c| c.name == name).unwrap();
        let get_row = out
            .rows
            .iter()
            .find(|r| r[col_idx("method")] == crate::value::Value::String("GET".into()))
            .unwrap();
        assert_eq!(get_row[col_idx("count")], crate::value::Value::Integer(2));
    }

    #[test]
    fn kv_with_sort() {
        let result = make_result(
            &["message"],
            vec![
                vec![crate::value::Value::String("name=charlie score=30".into())],
                vec![crate::value::Value::String("name=alice score=10".into())],
                vec![crate::value::Value::String("name=bob score=20".into())],
            ],
        );

        let stages = vec![
            span(PipeStage::Extract(ExtractStage {
                mode: ExtractMode::KeyValue { separator: '=' },
                source_field: None,
                keyword: "extract",
            })),
            span(PipeStage::Sort(SortStage {
                fields: vec![SortField {
                    field: "score".into(),
                    direction: SortDirection::Asc,
                }],
            })),
        ];

        let out = apply_rust_stages(
            result,
            &stages,
            &PinScope::unpinned(),
            EvalContext::capture(),
            None,
        )
        .unwrap();
        let col_idx = |name: &str| out.columns.iter().position(|c| c.name == name).unwrap();
        let scores: Vec<_> = out.rows.iter().map(|r| &r[col_idx("score")]).collect();
        assert_eq!(
            scores,
            vec![
                &crate::value::Value::Integer(10),
                &crate::value::Value::Integer(20),
                &crate::value::Value::Integer(30),
            ]
        );
    }

    #[test]
    fn kv_custom_separator() {
        let result = make_result(
            &["message"],
            vec![vec![crate::value::Value::String(
                "user:alice status:200".into(),
            )]],
        );

        let stages = vec![span(PipeStage::Extract(ExtractStage {
            mode: ExtractMode::KeyValue { separator: ':' },
            source_field: None,
            keyword: "extract",
        }))];

        let out = apply_rust_stages(
            result,
            &stages,
            &PinScope::unpinned(),
            EvalContext::capture(),
            None,
        )
        .unwrap();
        let col_idx = |name: &str| out.columns.iter().position(|c| c.name == name).unwrap();
        assert_eq!(
            out.rows[0][col_idx("user")],
            crate::value::Value::String("alice".into())
        );
        assert_eq!(
            out.rows[0][col_idx("status")],
            crate::value::Value::Integer(200)
        );
    }

    #[test]
    fn type_coercion() {
        let result = make_result(
            &["message"],
            vec![vec![crate::value::Value::String(
                "count=42 rate=1.5 flag=true name=hello".into(),
            )]],
        );

        let stages = vec![span(PipeStage::Extract(ExtractStage {
            mode: ExtractMode::KeyValue { separator: '=' },
            source_field: None,
            keyword: "extract",
        }))];

        let out = apply_rust_stages(
            result,
            &stages,
            &PinScope::unpinned(),
            EvalContext::capture(),
            None,
        )
        .unwrap();
        let col_idx = |name: &str| out.columns.iter().position(|c| c.name == name).unwrap();
        assert_eq!(
            out.rows[0][col_idx("count")],
            crate::value::Value::Integer(42)
        );
        assert_eq!(
            out.rows[0][col_idx("rate")],
            crate::value::Value::Float(1.5)
        );
        assert_eq!(
            out.rows[0][col_idx("flag")],
            crate::value::Value::Boolean(true)
        );
        assert_eq!(
            out.rows[0][col_idx("name")],
            crate::value::Value::String("hello".into())
        );
    }

    #[test]
    fn empty_result_passthrough() {
        let result = QueryResult::empty();
        let stages = vec![span(PipeStage::Extract(ExtractStage {
            mode: ExtractMode::KeyValue { separator: '=' },
            source_field: None,
            keyword: "extract",
        }))];
        let out = apply_rust_stages(
            result,
            &stages,
            &PinScope::unpinned(),
            EvalContext::capture(),
            None,
        )
        .unwrap();
        assert!(out.is_empty());
    }

    /// `n` messages `k=v{i}` for the kv tail to split, one per row.
    fn kv_messages(n: usize) -> QueryResult {
        make_result(
            &["message"],
            (0..n)
                .map(|i| vec![crate::value::Value::String(format!("k=v{i} n={i}"))])
                .collect(),
        )
    }

    fn run_tail(dsl: &str, result: QueryResult) -> Result<QueryResult, EngineError> {
        let tail = trawl_core::parser::parse(dsl).expect("dsl parses").pipeline;
        apply_rust_stages(
            result,
            &tail,
            &PinScope::unpinned(),
            EvalContext::capture(),
            None,
        )
    }

    /// The batch tail feeds the live lane's accumulators, which stop
    /// taking input at their caps. A batch answer has no channel to say
    /// it is partial, so a tail that would drop input refuses as
    /// `result_too_large` and names the cap, instead of answering with
    /// counts that leave groups out (ADR-0001).
    #[test]
    fn kv_tail_over_group_cap_refuses() {
        let limit = trawl_core::stream::AggregationCap::Groups.limit();
        let err = run_tail(
            "* | extract kv | stats count() by k",
            kv_messages(limit + 1),
        )
        .expect_err("a tail past the group cap must not answer");
        assert!(
            matches!(
                err,
                EngineError::AggregationCapped(trawl_core::stream::AggregationCap::Groups)
            ),
            "{err:?}"
        );
        assert_eq!(
            err.to_string(),
            "result too large: the aggregation has more than 10000 groups; narrow the query"
        );
    }

    /// Exactly at a cap nothing is dropped, so the tail answers whole.
    #[test]
    fn kv_tail_at_group_cap_answers() {
        let limit = trawl_core::stream::AggregationCap::Groups.limit();
        let out = run_tail("* | extract kv | stats count() by k", kv_messages(limit))
            .expect("exactly at the cap drops nothing");
        assert_eq!(out.rows.len(), limit);
    }

    /// The per-group value caps refuse the same way, each naming itself.
    #[test]
    fn kv_tail_over_value_caps_refuses() {
        use trawl_core::stream::AggregationCap;

        let distinct = AggregationCap::DistinctValues.limit();
        let err = run_tail("* | extract kv | stats dc(k)", kv_messages(distinct + 1))
            .expect_err("a dc() past its cap must not answer");
        assert!(
            matches!(
                err,
                EngineError::AggregationCapped(AggregationCap::DistinctValues)
            ),
            "{err:?}"
        );
        assert_eq!(
            err.to_string(),
            "result too large: a dc() or values() has more than 10000 distinct values \
             in one group; narrow the query"
        );

        let exact = AggregationCap::ExactValues.limit();
        let err = run_tail("* | extract kv | stats median(n)", kv_messages(exact + 1))
            .expect_err("a median past its cap must not answer");
        assert!(
            matches!(
                err,
                EngineError::AggregationCapped(AggregationCap::ExactValues)
            ),
            "{err:?}"
        );
        assert_eq!(
            err.to_string(),
            "result too large: a median or percentile has more than 100000 values \
             in one group; narrow the query"
        );
    }
}
