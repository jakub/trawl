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
//! This module applies those Rust stages to the SQL result set in the
//! order the query writes them (ADR-0064). A run of streamable stages
//! reuses the streaming engine's per-event transforms and aggregation
//! machinery; a `sort` or a `dedup` is a barrier that reads its whole
//! input before the next stage sees a row.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use chrono::{Offset as _, Utc};
use trawl_core::ast::{DedupStage, PipeStage, SortDirection, SortStage, Spanned, TrawlDuration};
use trawl_core::compare::Instant;
use trawl_core::context::EvalContext;
use trawl_core::emitter::EmitError;
use trawl_core::eval::{EvalValue, bind_event_key};
use trawl_core::pattern::{PatternError, Preparer};
use trawl_core::pin_scope::PinScope;
use trawl_core::row::{self, Row};
use trawl_core::schema::catalog_key;
use trawl_core::stream::{
    self, CompiledStage, OutputColumn, StageResult, StreamPlan, StreamPlanError,
};

use crate::cancel::CancelLatch;
use crate::error::EngineError;
use crate::value::{Column, ColumnType, QueryResult, TypeFold, land_u64};

/// Apply Rust pipeline stages to a SQL result set.
///
/// Compiles the stages into ordered steps ([`compile_tail`]), converts the
/// columnar `QueryResult` into typed pipeline rows, runs the steps in the
/// order the query writes them, and converts back. The result's columns
/// are the steps' output schema over the input's
/// ([`StreamPlan::output_schema`], folded across the steps), so a tail
/// that removes every row still answers with the columns it projects,
/// typed as far as the input says ([`tail_cell_type`]), and then any field
/// an `extract kv` found.
///
/// An input with no columns is the SQL read's answer for a source that
/// matched no file, and it is returned as it came, as the same query
/// without the tail returns it: there is no input schema to walk, and no
/// row for the tail to read. The tail is compiled first, so a stage it
/// refuses is refused over that input too.
///
/// `pins` is the pin scope stamped at the kv split
/// (`EmittedQuery::rust_stage_pins`), so the tail's `where`/`let` evaluate
/// under the same interpretation the SQL prefix used. The tail compiles in
/// one walk of that scope, which a `sort` or a `dedup` leaves unchanged.
///
/// `anchor` is the statement's `now()` instant (`EmittedQuery::anchor`,
/// ADR-0017 §3). Every row — pre-stage, accumulator feed, snapshot,
/// post-stage — evaluates under this one instant, in every step, because
/// the tail is part of one unit of output with the SQL prefix that bound
/// it. Nothing here samples a clock.
///
/// `last` is the query's `last=` window (`ast.search.time_filter`). A
/// `timechart` in the tail with no `span=` picks its automatic bucket
/// width from it, through the resolver the SQL emitter uses for the same
/// stage (ADR-0038), so `last=4h | extract kv | timechart count()` cuts
/// the five-minute buckets the same query without the kv split cuts in
/// SQL — and that the web chart lays its grid with — rather than the
/// live stream's one-minute default.
///
/// The tail's patterns are prepared through `preparer`, the query's one
/// metered ledger, so each is built and charged once, their charge is
/// held from the server-wide regex reservation, and it shares the query's
/// cap with any RE2 allowance the ledger holds for the SQL statement
/// (ADR-0063). The plan is local to this call, so the charge comes back
/// when it returns or unwinds: inside the executor's work, when the work
/// that matches with it ends, whatever the request's deadline did. A tail
/// whose patterns do not fit is [`EngineError::RegexResources`].
///
/// `cancel` is the lane's latch. The tail reads it before each step, row
/// and stage, and a ledger built over its stop ends a pattern build or
/// search the same way, so a cancelled tail stops at its next step with
/// [`EngineError::Cancelled`] (ADR-0063).
pub fn apply_rust_stages(
    result: QueryResult,
    stages: &[Spanned<PipeStage>],
    pins: &PinScope,
    anchor: EvalContext,
    last: Option<TrawlDuration>,
    preparer: &mut Preparer,
    cancel: &CancelLatch,
) -> Result<QueryResult, EngineError> {
    if stages.is_empty() {
        return Ok(result);
    }

    let mut steps = compile_tail(stages, pins, last, preparer)?;
    if result.columns.is_empty() {
        return Ok(result);
    }
    let input = result
        .columns
        .iter()
        .map(|column| OutputColumn::new(column.name.clone(), Some(tail_cell_type(&column.ty))))
        .collect();
    let schema = steps.iter().fold(input, |schema, step| match step {
        TailStep::Stream(plan) => plan.output_schema(schema),
        // A barrier orders or drops rows and leaves the columns alone.
        TailStep::Sort(_) | TailStep::Dedup(_) => schema,
    });

    let events = rows_to_events(&result, cancel)?;
    // The steps own copies of every cell, so the SQL result goes now
    // rather than live beside them through the whole tail.
    drop(result);
    let rows = run_steps(&mut steps, events, &anchor, cancel)?;

    rows_to_result_within(&rows, &schema, cancel)
}

/// The sentence a `tail` after `extract kv` is refused with: the kv tail
/// has no rule yet for which rows are last after a rename, an
/// aggregation or a dropped `_time` (ADR-0064).
const TAIL_REFUSAL: &str =
    "post-processing: tail is not supported after extract kv; use sort and head instead";

/// One step of the kv batch tail (ADR-0064). Each step takes the
/// previous step's rows by value and hands its own to the next.
enum TailStep {
    /// A run of streamable stages, compiled as the live lane compiles
    /// them. A `limit` that is spent ends only this run.
    Stream(StreamPlan),
    /// A `sort` barrier: it reads its whole input before the next step
    /// sees a row.
    Sort(SortStage),
    /// A `dedup` barrier, for the same reason: the newest row of a key
    /// can be the last row of the input.
    Dedup(DedupStage),
}

impl TailStep {
    /// Run this step over `rows`.
    fn run(
        &mut self,
        rows: impl IntoIterator<Item = Row>,
        anchor: &EvalContext,
        cancel: &CancelLatch,
    ) -> Result<Vec<Row>, EngineError> {
        match self {
            Self::Stream(StreamPlan::PassThrough(stages)) => {
                apply_pass_through(stages, rows, anchor, cancel)
            }
            Self::Stream(StreamPlan::Aggregate {
                pre_stages,
                aggregation,
                post_stages,
            }) => apply_aggregate(pre_stages, aggregation, post_stages, rows, anchor, cancel),
            Self::Sort(sort) => sort_step(rows.into_iter().collect(), sort, cancel),
            Self::Dedup(dedup) => dedup_step(rows.into_iter().collect(), dedup, cancel),
        }
    }
}

/// Whether `stage` is a barrier of the kv tail: a stage that reads its
/// whole input before the next stage sees a row.
fn is_barrier(stage: &PipeStage) -> bool {
    matches!(stage, PipeStage::Sort(_) | PipeStage::Dedup(_))
}

/// The kv tail's stages, compiled into the steps that run them in written
/// order (ADR-0064).
///
/// A `tail` is refused first, before anything is built: the kv tail has
/// no rule for it yet ([`TAIL_REFUSAL`]).
///
/// The barriers are taken out, and one call to the live lane's compiler
/// compiles every other stage. That one call is the whole tail's
/// validation, before any row runs: the expansion budget, projection
/// collisions, the stages the tail cannot run, and the one-aggregation
/// rule, which a barrier between two aggregations does not reset. It is
/// also one walk of the pin scope (a barrier leaves the scope unchanged,
/// `PinScope::advance`), one `last` span and one pass over the ledger. Its
/// plan is then cut back into steps at the barriers' positions
/// ([`cut_into_steps`]).
fn compile_tail(
    stages: &[Spanned<PipeStage>],
    pins: &PinScope,
    last: Option<TrawlDuration>,
    preparer: &mut Preparer,
) -> Result<Vec<TailStep>, EngineError> {
    if stages
        .iter()
        .any(|stage| matches!(stage.node, PipeStage::Tail(_)))
    {
        return Err(EngineError::Emit(EmitError::UnsupportedOperation {
            message: TAIL_REFUSAL.to_owned(),
        }));
    }
    let streamed: Vec<Spanned<PipeStage>> = stages
        .iter()
        .filter(|stage| !is_barrier(&stage.node))
        .cloned()
        .collect();
    let plan =
        stream::compile_stream_plan_with(&streamed, pins, last, preparer).map_err(plan_error)?;
    Ok(cut_into_steps(stages, plan))
}

/// The tail's answer for a plan the live lane's compiler refused.
fn plan_error(error: StreamPlanError) -> EngineError {
    match error {
        StreamPlanError::Pattern(PatternError::Resources(refusal)) => {
            EngineError::RegexResources(refusal)
        }
        // The ledger reads the lane's latch: a cancellation, never a
        // refusal of the query (ADR-0063).
        StreamPlanError::Cancelled | StreamPlanError::Pattern(PatternError::Cancelled) => {
            EngineError::Cancelled
        }
        e => EngineError::Emit(EmitError::UnsupportedOperation {
            message: format!("post-processing: {e}"),
        }),
    }
}

/// One compiled piece of a stream plan: a per-event stage, or the
/// aggregation.
enum Piece {
    Stage(CompiledStage),
    Aggregation(stream::CompiledAggregation),
}

/// Cut `plan`, compiled from `stages` with their barriers taken out, back
/// into steps at the barriers' positions.
///
/// The live compiler emits one piece per stage it is given, in order: a
/// [`CompiledStage`] for each per-event stage and the
/// [`stream::CompiledAggregation`] at the aggregation's place
/// (`stream::compile_stream_plan_with`). So the pieces pair one for one,
/// in order, with the stages that are not barriers, and a run between two
/// barriers holds exactly its own stages. A count that does not pair is
/// the server's own broken invariant, and the tail panics rather than run
/// a stage at another stage's position.
fn cut_into_steps(stages: &[Spanned<PipeStage>], plan: StreamPlan) -> Vec<TailStep> {
    let pieces: Vec<Piece> = match plan {
        StreamPlan::PassThrough(stages) => stages.into_iter().map(Piece::Stage).collect(),
        StreamPlan::Aggregate {
            pre_stages,
            aggregation,
            post_stages,
        } => pre_stages
            .into_iter()
            .map(Piece::Stage)
            .chain(std::iter::once(Piece::Aggregation(aggregation)))
            .chain(post_stages.into_iter().map(Piece::Stage))
            .collect(),
    };
    let streamed = stages
        .iter()
        .filter(|stage| !is_barrier(&stage.node))
        .count();
    assert_eq!(
        pieces.len(),
        streamed,
        "the stream compiler emits one piece per stage it compiles"
    );

    let mut pieces = pieces.into_iter();
    let mut steps = Vec::new();
    let mut run = Vec::new();
    for stage in stages {
        match &stage.node {
            PipeStage::Sort(sort) => {
                steps.extend(stream_step(std::mem::take(&mut run)));
                steps.push(TailStep::Sort(sort.clone()));
            }
            PipeStage::Dedup(dedup) => {
                steps.extend(stream_step(std::mem::take(&mut run)));
                steps.push(TailStep::Dedup(dedup.clone()));
            }
            _ => run.extend(pieces.next()),
        }
    }
    steps.extend(stream_step(run));
    steps
}

/// The stream step a run of pieces compiles to, or none for an empty run.
fn stream_step(run: Vec<Piece>) -> Option<TailStep> {
    if run.is_empty() {
        return None;
    }
    let mut pre_stages = Vec::new();
    let mut aggregation = None;
    let mut post_stages = Vec::new();
    for piece in run {
        match piece {
            Piece::Aggregation(compiled) => aggregation = Some(compiled),
            Piece::Stage(stage) if aggregation.is_none() => pre_stages.push(stage),
            Piece::Stage(stage) => post_stages.push(stage),
        }
    }
    Some(TailStep::Stream(match aggregation {
        Some(aggregation) => StreamPlan::Aggregate {
            pre_stages,
            aggregation,
            post_stages,
        },
        None => StreamPlan::PassThrough(pre_stages),
    }))
}

/// Run `steps` in order, the first over `rows` and each later one over
/// the rows the step before it kept.
///
/// The latch is read before each step and once after the last, so a stop
/// that lands as a step ends, or while a step has no row to read it over,
/// ends the query as [`EngineError::Cancelled`] rather than answering
/// (ADR-0063).
fn run_steps(
    steps: &mut [TailStep],
    rows: impl IntoIterator<Item = Row>,
    anchor: &EvalContext,
    cancel: &CancelLatch,
) -> Result<Vec<Row>, EngineError> {
    let mut steps = steps.iter_mut();
    cancel.check()?;
    let mut rows = match steps.next() {
        Some(first) => first.run(rows, anchor, cancel)?,
        None => rows.into_iter().collect(),
    };
    for step in steps {
        cancel.check()?;
        rows = step.run(rows, anchor, cancel)?;
    }
    cancel.check()?;
    Ok(rows)
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

/// Convert a columnar `QueryResult` into a vec of typed pipeline rows,
/// reading the lane's latch every [`TAIL_CANCEL_STRIDE`] rows: a result
/// can be a million rows, and the copy holds the lane's permit.
fn rows_to_events(result: &QueryResult, cancel: &CancelLatch) -> Result<Vec<Row>, EngineError> {
    let mut events = Vec::with_capacity(result.rows.len());
    for row in &result.rows {
        if events.len().is_multiple_of(TAIL_CANCEL_STRIDE) {
            cancel.check()?;
        }
        events.push(
            result
                .columns
                .iter()
                .zip(row.iter())
                .map(|(col, cell)| (col.name.clone(), cell_to_eval(cell)))
                .collect(),
        );
    }
    Ok(events)
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
    match rows_to_result_within(rows, schema, &CancelLatch::never()) {
        Ok(result) => result,
        Err(_) => unreachable!("a latch that never stops"),
    }
}

/// The answer's columns: the schema's, then each name the rows add, one
/// column per ASCII-folded name.
struct ResultColumns {
    names: Vec<String>,
    carried: Vec<Option<ColumnType>>,
    /// Each column by its ASCII-folded name.
    index: HashMap<String, usize>,
}

/// [`ResultColumns`] for `rows`. This pass reads every row too, so it
/// reads the lane's latch every [`TAIL_CANCEL_STRIDE`] rows, as the copy
/// after it does.
fn result_columns(
    rows: &[Row],
    schema: &[OutputColumn<ColumnType>],
    cancel: &CancelLatch,
) -> Result<ResultColumns, EngineError> {
    let mut columns = ResultColumns {
        names: Vec::new(),
        carried: Vec::new(),
        index: HashMap::new(),
    };
    let mut name = |name: &str, carries: Option<ColumnType>| {
        if let Entry::Vacant(slot) = columns.index.entry(catalog_key(name)) {
            slot.insert(columns.names.len());
            columns.names.push(name.to_owned());
            columns.carried.push(carries);
        }
    };
    for column in schema {
        name(&column.name, column.carries.clone());
    }
    for (at, row) in rows.iter().enumerate() {
        if at.is_multiple_of(TAIL_CANCEL_STRIDE) {
            cancel.check()?;
        }
        for key in row.keys() {
            name(key, None);
        }
    }
    Ok(columns)
}

/// [`rows_to_result`], reading the lane's latch every
/// [`TAIL_CANCEL_STRIDE`] rows: the batch tail's answer can be a million
/// rows, and the copy holds the lane's permit.
fn rows_to_result_within(
    rows: &[Row],
    schema: &[OutputColumn<ColumnType>],
    cancel: &CancelLatch,
) -> Result<QueryResult, EngineError> {
    let ResultColumns {
        names,
        carried,
        index,
    } = result_columns(rows, schema, cancel)?;

    let mut folds = vec![TypeFold::new(); names.len()];
    let mut out: Vec<Vec<crate::value::Value>> = Vec::with_capacity(rows.len());
    for row in rows {
        if out.len().is_multiple_of(TAIL_CANCEL_STRIDE) {
            cancel.check()?;
        }
        let mut cells: Vec<Option<&EvalValue>> = vec![None; names.len()];
        for (key, cell) in row {
            let at = index[&catalog_key(key)];
            if cells[at].is_none() || *key == names[at] {
                cells[at] = Some(cell);
            }
        }
        out.push(
            cells
                .into_iter()
                .zip(folds.iter_mut())
                .map(|(cell, fold)| {
                    let cell = cell.unwrap_or(&EvalValue::Null);
                    observe_cell(fold, cell);
                    eval_to_cell(cell)
                })
                .collect(),
        );
    }

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

    Ok(QueryResult { columns, rows: out })
}

/// The tail's answer for a stage that has no prepared pattern for an
/// operand it reads (ADR-0063). The plan compile prepared every pattern
/// the tail reads, so whatever the cause, the server broke its own
/// invariant and the query is not to blame:
/// [`EngineError::MissingPreparedPattern`], which names no pattern. The
/// cause is logged here, by its fixed sentence. The tail stops at the
/// first such row rather than read it as a non-match.
///
/// A search the lane's latch stopped ([`PatternError::Cancelled`]) is
/// [`EngineError::Cancelled`] instead: the query was cancelled, and no
/// invariant broke.
fn tail_pattern_error(error: &PatternError) -> EngineError {
    if matches!(error, PatternError::Cancelled) {
        return EngineError::Cancelled;
    }
    tracing::error!(
        event_type = "tail_missing_prepared_pattern",
        %error,
        "refusing a batch answer: a compiled tail stage has no prepared pattern"
    );
    EngineError::MissingPreparedPattern
}

/// Run pass-through stages on each event.
///
/// Refuses through [`tail_pattern_error`] when a stage lacks a prepared
/// pattern.
fn apply_pass_through(
    stages: &mut [CompiledStage],
    events: impl IntoIterator<Item = Row>,
    anchor: &EvalContext,
    cancel: &CancelLatch,
) -> Result<Vec<Row>, EngineError> {
    let mut output = Vec::new();
    'event: for mut event in events {
        cancel.check()?;
        for stage in stages.iter_mut() {
            cancel.check()?;
            match stream::apply_stage(stage, &mut event, anchor)
                .map_err(|error| tail_pattern_error(&error))?
            {
                StageResult::Pass => {}
                StageResult::Filtered => continue 'event,
                StageResult::Done => return Ok(output),
            }
        }
        output.push(event);
    }
    Ok(output)
}

/// Run an aggregation plan: pre-stages → accumulate → snapshot → post-stages.
///
/// Refuses with [`EngineError::AggregationCapped`] when the accumulators
/// dropped input at a cap: the live lane can report a partial answer on
/// its snapshot, but a batch answer cannot, so it gives none (ADR-0001).
/// Refuses through [`tail_pattern_error`] when a stage lacks a prepared
/// pattern.
fn apply_aggregate(
    pre_stages: &mut [CompiledStage],
    aggregation: &mut stream::CompiledAggregation,
    post_stages: &mut [CompiledStage],
    events: impl IntoIterator<Item = Row>,
    anchor: &EvalContext,
    cancel: &CancelLatch,
) -> Result<Vec<Row>, EngineError> {
    // Feed events through pre-stages into the aggregation.
    'event: for mut event in events {
        cancel.check()?;
        for stage in pre_stages.iter_mut() {
            cancel.check()?;
            match stream::apply_stage(stage, &mut event, anchor)
                .map_err(|error| tail_pattern_error(&error))?
            {
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
        cancel.check()?;
        for stage in post_stages.iter_mut() {
            cancel.check()?;
            match stream::apply_stage(stage, &mut row, anchor)
                .map_err(|error| tail_pattern_error(&error))?
            {
                StageResult::Pass => {}
                StageResult::Filtered => continue 'row,
                StageResult::Done => return Ok(output),
            }
        }
        output.push(row);
    }
    Ok(output)
}

/// A `sort` step: a stable sort of `rows` by the stage's fields, the
/// first field written the primary one.
///
/// Each field binds per row through [`bind_event_key`], as every other
/// stage of the tail binds a name, and a row without it reads NULL. A
/// NULL sorts after every value in both directions, as batch SQL puts it:
/// [`compare_cells`] puts it last, and `-` reverses only the comparison of
/// two values. Rows whose keys compare equal keep their input order, so of
/// several `sort` steps the last is the primary key and the earlier ones
/// break its ties.
///
/// The sort reads the lane's latch as it goes ([`sort_rows`]).
fn sort_step(
    rows: Vec<Row>,
    sort: &SortStage,
    cancel: &CancelLatch,
) -> Result<Vec<Row>, EngineError> {
    sort_rows(rows, cancel, |a, b| {
        for field in &sort.fields {
            let (x, y) = (sort_cell(a, &field.field), sort_cell(b, &field.field));
            let cmp = compare_cells(x, y);
            let cmp = match field.direction {
                SortDirection::Desc if is_value(x) && is_value(y) => cmp.reverse(),
                SortDirection::Asc | SortDirection::Desc => cmp,
            };
            if cmp.is_ne() {
                return cmp;
            }
        }
        std::cmp::Ordering::Equal
    })
}

/// The cell a sort key `name` reads in `row`, bound as `DuckDB` binds an
/// identifier.
fn sort_cell<'r>(row: &'r Row, name: &str) -> Option<&'r EvalValue> {
    bind_event_key(row, name).and_then(|key| row.get(key))
}

/// Whether a sort key read a value rather than a NULL or nothing.
fn is_value(cell: Option<&EvalValue>) -> bool {
    !matches!(cell, None | Some(EvalValue::Null))
}

/// A `dedup` step: for each key, the row with the newest `_time`, the
/// winners in their input order (ADR-0064).
///
/// The key is the live stage's ([`stream::dedup_key`]): the named fields'
/// cell text, where an absent field and a NULL are one key, or with no
/// field the whole row. Unlike the live stage, no key is ever evicted: a
/// batch answer has every row before the step runs.
///
/// A row's `_time` binds as every name in the tail binds
/// ([`bind_event_key`]) and is read as batch SQL's
/// `TRY_CAST(_time AS TIMESTAMP)` reads it ([`EvalValue::as_instant`]).
/// Only a strictly newer row replaces the one held for its key, so of
/// rows with the same time the earliest wins, and a row with no readable
/// time loses to any row with one. Identical rows have the same `_time`,
/// so a bare `dedup` keeps the first of them.
///
/// One pass picks each key's winner by its index, and a second moves the
/// winners out in input order, so no row is copied. Both read the lane's
/// latch every [`TAIL_CANCEL_STRIDE`] rows.
fn dedup_step(
    rows: Vec<Row>,
    dedup: &DedupStage,
    cancel: &CancelLatch,
) -> Result<Vec<Row>, EngineError> {
    let mut winners: HashMap<Vec<Option<String>>, (usize, Option<Instant>)> = HashMap::new();
    for (at, row) in rows.iter().enumerate() {
        if at.is_multiple_of(TAIL_CANCEL_STRIDE) {
            cancel.check()?;
        }
        let time = bind_event_key(row, trawl_core::schema::TIME)
            .and_then(|key| row.get(key))
            .and_then(EvalValue::as_instant);
        match winners.entry(stream::dedup_key(&dedup.fields, row)) {
            Entry::Vacant(slot) => {
                slot.insert((at, time));
            }
            Entry::Occupied(mut slot) => {
                if time > slot.get().1 {
                    slot.insert((at, time));
                }
            }
        }
    }

    let mut wins = vec![false; rows.len()];
    for (&(at, _), seen) in winners.values().zip(0_usize..) {
        if seen.is_multiple_of(TAIL_CANCEL_STRIDE) {
            cancel.check()?;
        }
        wins[at] = true;
    }
    let mut kept = Vec::with_capacity(winners.len());
    drop(winners);
    for (row, (at, wins)) in rows.into_iter().zip(wins.into_iter().enumerate()) {
        if at.is_multiple_of(TAIL_CANCEL_STRIDE) {
            cancel.check()?;
        }
        if wins {
            kept.push(row);
        }
    }
    Ok(kept)
}

/// How many rows a row pass of the batch tail takes between two reads
/// of the lane's latch: a sort step's run or merge, a dedup step's pass,
/// a copy in or out, or the render of a tracked timestamp column.
pub(crate) const TAIL_CANCEL_STRIDE: usize = 4096;

/// A stable sort of `rows` by `compare` that a cancellation stops within
/// [`TAIL_CANCEL_STRIDE`] rows of work.
///
/// One `sort_by` call cannot be left early, and a comparator that starts
/// answering `Equal` once the latch is set breaks the total order
/// `sort_by` may panic over. So the rows are sorted in runs of one stride,
/// each run reading the latch first, and the runs are merged pairwise,
/// reading it every stride of output. Ties keep the earlier run's row
/// first, so the sort is as stable as `sort_by`.
fn sort_rows(
    rows: Vec<Row>,
    cancel: &CancelLatch,
    mut compare: impl FnMut(&Row, &Row) -> std::cmp::Ordering,
) -> Result<Vec<Row>, EngineError> {
    let mut runs = Vec::with_capacity(rows.len().div_ceil(TAIL_CANCEL_STRIDE));
    let mut rows = rows.into_iter();
    loop {
        let mut run: Vec<Row> = rows.by_ref().take(TAIL_CANCEL_STRIDE).collect();
        if run.is_empty() {
            break;
        }
        cancel.check()?;
        run.sort_by(&mut compare);
        runs.push(run);
    }
    while runs.len() > 1 {
        let mut merged = Vec::with_capacity(runs.len().div_ceil(2));
        let mut pairs = runs.into_iter();
        while let Some(left) = pairs.next() {
            merged.push(match pairs.next() {
                Some(right) => merge_runs(left, right, cancel, &mut compare)?,
                None => left,
            });
        }
        runs = merged;
    }
    Ok(runs.pop().unwrap_or_default())
}

/// Merge two runs sorted by `compare` into one, `left`'s row first on a
/// tie, reading the latch every [`TAIL_CANCEL_STRIDE`] rows of output.
fn merge_runs(
    left: Vec<Row>,
    right: Vec<Row>,
    cancel: &CancelLatch,
    compare: &mut impl FnMut(&Row, &Row) -> std::cmp::Ordering,
) -> Result<Vec<Row>, EngineError> {
    let mut out = Vec::with_capacity(left.len() + right.len());
    let mut left = left.into_iter().peekable();
    let mut right = right.into_iter().peekable();
    while let (Some(l), Some(r)) = (left.peek(), right.peek()) {
        if out.len().is_multiple_of(TAIL_CANCEL_STRIDE) {
            cancel.check()?;
        }
        let right_first = compare(r, l) == std::cmp::Ordering::Less;
        out.extend(if right_first {
            right.next()
        } else {
            left.next()
        });
    }
    // At most one run is left, and the move reads the latch on the same
    // stride: that run can be most of the result.
    for row in left.chain(right) {
        if out.len().is_multiple_of(TAIL_CANCEL_STRIDE) {
            cancel.check()?;
        }
        out.push(row);
    }
    Ok(out)
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

    let sorted = apply_rust_stages(
        result,
        &tail,
        &PinScope::unpinned(),
        anchor,
        None,
        &mut ledger(),
        &CancelLatch::never(),
    )
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

    let err = apply_rust_stages(
        result,
        &tail,
        &PinScope::unpinned(),
        anchor,
        None,
        &mut ledger(),
        &CancelLatch::never(),
    )
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

/// A tail whose patterns need more regex memory than one query holds is
/// refused before it matches a row, typed as the capacity refusal it is,
/// with a sentence that quotes none of its patterns. Four bodies of the
/// `\w{20}` class charge past the budget at the fourth (ADR-0063).
#[cfg(test)]
#[test]
fn a_tail_over_the_regex_budget_is_a_typed_refusal() {
    let heavy = [r"\w{20}", r"(?i)\w{20}", r"\w{19}\w", r"\w\w{19}"];
    let stages = heavy.iter().fold(String::new(), |mut stages, body| {
        stages.push_str(" | where message matches /");
        stages.push_str(body);
        stages.push('/');
        stages
    });
    let tail = trawl_core::parser::parse(&format!("* | extract kv{stages}"))
        .expect("parsing meters no regex budget")
        .pipeline;
    let result = QueryResult {
        columns: vec![Column::new("message", ColumnType::String)],
        rows: Vec::new(),
    };
    let anchor = EvalContext::at(
        chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .expect("a valid RFC 3339 instant")
            .into(),
    );

    let err = apply_rust_stages(
        result,
        &tail,
        &PinScope::unpinned(),
        anchor,
        None,
        &mut ledger(),
        &CancelLatch::never(),
    )
    .expect_err("the fourth heavy pattern is over the budget");
    assert!(
        matches!(
            err,
            EngineError::RegexResources(trawl_core::pattern::RegexRefusal::Budget)
        ),
        "{err:?}"
    );
    let sentence = err.to_string();
    assert!(
        heavy.iter().all(|body| !sentence.contains(body)),
        "{sentence}"
    );
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

    let windowed = apply_rust_stages(
        rows(),
        tail,
        &PinScope::unpinned(),
        anchor(),
        last,
        &mut ledger(),
        &CancelLatch::never(),
    )
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
    let live = apply_rust_stages(
        rows(),
        tail,
        &PinScope::unpinned(),
        anchor(),
        None,
        &mut ledger(),
        &CancelLatch::never(),
    )
    .expect("the tail runs a timechart over a kv split");
    assert_eq!(live.rows.len(), 2, "{:?}", live.rows);
}

/// A fresh metered ledger for one tail, as a query lane opens one.
#[cfg(test)]
fn ledger() -> Preparer {
    Preparer::new()
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
            &mut ledger(),
            &CancelLatch::never(),
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
            &mut ledger(),
            &CancelLatch::never(),
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
            &mut ledger(),
            &CancelLatch::never(),
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
            &mut ledger(),
            &CancelLatch::never(),
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
            &mut ledger(),
            &CancelLatch::never(),
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
            &mut ledger(),
            &CancelLatch::never(),
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
            &mut ledger(),
            &CancelLatch::never(),
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
            &mut ledger(),
            &CancelLatch::never(),
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

    /// A tail stage whose table lacks the pattern it reads refuses the
    /// batch answer in every position of the tail as the server's own
    /// fault, [`EngineError::MissingPreparedPattern`], whose sentence names
    /// no pattern: a row is never read as a non-match, and the query is
    /// not blamed.
    #[test]
    fn missing_prepared_pattern_is_an_error() {
        use trawl_core::eval::PatternTable;
        use trawl_core::pin_scope::PinScope;

        let secret = "s3cr3t-[0-9]+";
        let unprepared = || {
            vec![CompiledStage::Where {
                condition: span(Expr::Binary {
                    lhs: Box::new(span(Expr::FieldRef("message".into()))),
                    op: BinaryOp::Matches,
                    rhs: Box::new(span(Expr::Literal(LiteralValue::String(secret.into())))),
                }),
                pins: PinScope::unpinned(),
                patterns: PatternTable::default(),
            }]
        };
        let events = || {
            rows_to_events(
                &make_result(
                    &["message"],
                    vec![vec![crate::value::Value::String("k=v".into())]],
                ),
                &CancelLatch::never(),
            )
            .expect("a latch that never stops")
        };
        let aggregation = || {
            let StreamPlan::Aggregate { aggregation, .. } = stream::compile_stream_plan(
                &trawl_core::parser::parse("* | stats count() by message")
                    .expect("dsl parses")
                    .pipeline,
                &PinScope::unpinned(),
                None,
            )
            .expect("plan compiles") else {
                panic!("an aggregate plan");
            };
            aggregation
        };
        let anchor = EvalContext::capture();

        let refusals = [
            apply_pass_through(&mut unprepared(), events(), &anchor, &CancelLatch::never())
                .map(|_| ()),
            apply_aggregate(
                &mut unprepared(),
                &mut aggregation(),
                &mut [],
                events(),
                &anchor,
                &CancelLatch::never(),
            )
            .map(|_| ()),
            apply_aggregate(
                &mut [],
                &mut aggregation(),
                &mut unprepared(),
                events(),
                &anchor,
                &CancelLatch::never(),
            )
            .map(|_| ()),
        ];
        for (position, refusal) in ["pass-through", "pre-stage", "post-stage"]
            .into_iter()
            .zip(refusals)
        {
            let Err(error @ EngineError::MissingPreparedPattern) = refusal else {
                panic!("{position}: {refusal:?}");
            };
            let message = error.to_string();
            assert!(!message.contains(secret), "{position}: {message}");
        }
    }

    /// A latch over a fresh flag, and the flag that sets it.
    fn tripwire() -> (std::sync::Arc<std::sync::atomic::AtomicBool>, CancelLatch) {
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        (std::sync::Arc::clone(&flag), CancelLatch::new(flag))
    }

    /// `dsl`'s pipeline compiled as the tail compiles it, its patterns
    /// prepared on a ledger that reads `cancel`'s stop.
    fn cancellable_plan(dsl: &str, cancel: &CancelLatch) -> StreamPlan {
        let tail = trawl_core::parser::parse(dsl).expect("dsl parses").pipeline;
        stream::compile_stream_plan_with(
            &tail,
            &PinScope::unpinned(),
            None,
            &mut ledger().with_stop(cancel.stop()),
        )
        .expect("plan compiles")
    }

    /// How many events a `head` stage of cap `count` has admitted, read
    /// off its live counter.
    fn admitted(stage: &CompiledStage) -> u64 {
        let CompiledStage::Limit { count, remaining } = stage else {
            panic!("a head stage: {stage:?}");
        };
        count - remaining.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The batch tail reads the latch between events and between stages
    /// (ADR-0063), so a cancellation stops it at its next step with
    /// [`EngineError::Cancelled`], never a pattern or emit error. A `head`
    /// after the `extract kv` counts the events that reached it.
    ///
    /// Pass-through: the latch trips as the input hands over event `k+1`,
    /// and only the first `k` events reached the `head`. Aggregate: the
    /// latch trips when the input runs out, between the pre-stages and the
    /// post-stage, and the post-stage `head` admits nothing. A pattern
    /// cancelled while the tail compiles or searches is the same answer.
    #[test]
    fn rust_tail_observes_cancellation() {
        use std::cell::Cell;
        use std::sync::atomic::Ordering;

        const N: usize = 10;
        const K: usize = 4;
        let anchor = EvalContext::capture();

        let (flag, cancel) = tripwire();
        let StreamPlan::PassThrough(mut stages) =
            cancellable_plan("* | extract kv | head 100", &cancel)
        else {
            panic!("a pass-through plan");
        };
        let handed = Cell::new(0_usize);
        let events = rows_to_events(&kv_messages(N), &CancelLatch::never())
            .expect("a latch that never stops")
            .into_iter()
            .inspect(|_| {
                if handed.get() == K {
                    flag.store(true, Ordering::SeqCst);
                }
                handed.set(handed.get() + 1);
            });
        let outcome = apply_pass_through(&mut stages, events, &anchor, &cancel);
        assert!(
            matches!(outcome, Err(EngineError::Cancelled)),
            "pass-through: {outcome:?}"
        );
        assert_eq!(admitted(&stages[1]), K as u64, "stopped before event k+1");
        assert_eq!(handed.get(), K + 1, "read no event past k+1");

        let (flag, cancel) = tripwire();
        let StreamPlan::Aggregate {
            mut pre_stages,
            mut aggregation,
            mut post_stages,
        } = cancellable_plan(
            "* | extract kv | head 100 | stats count() by k | head 5",
            &cancel,
        )
        else {
            panic!("an aggregate plan");
        };
        let events = rows_to_events(&kv_messages(N), &CancelLatch::never())
            .expect("a latch that never stops")
            .into_iter()
            .chain(std::iter::from_fn(|| {
                flag.store(true, Ordering::SeqCst);
                None
            }));
        let outcome = apply_aggregate(
            &mut pre_stages,
            &mut aggregation,
            &mut post_stages,
            events,
            &anchor,
            &cancel,
        );
        assert!(
            matches!(outcome, Err(EngineError::Cancelled)),
            "aggregate: {outcome:?}"
        );
        assert_eq!(admitted(&pre_stages[1]), N as u64, "the pre-stages ran");
        assert_eq!(admitted(&post_stages[0]), 0, "the post-stage never ran");

        let (flag, cancel) = tripwire();
        flag.store(true, Ordering::SeqCst);
        let tail = trawl_core::parser::parse(r#"* | extract kv | where k matches "^v""#)
            .expect("dsl parses")
            .pipeline;
        let compiled = apply_rust_stages(
            kv_messages(N),
            &tail,
            &PinScope::unpinned(),
            anchor,
            None,
            &mut ledger().with_stop(cancel.stop()),
            &cancel,
        );
        assert!(
            matches!(compiled, Err(EngineError::Cancelled)),
            "compile: {compiled:?}"
        );
        assert!(matches!(
            tail_pattern_error(&PatternError::Cancelled),
            EngineError::Cancelled
        ));
    }

    /// Rows carrying `n` = each of `values`.
    fn numbered(values: impl IntoIterator<Item = i64>) -> Vec<Row> {
        values
            .into_iter()
            .map(|n| Row::from([("n".to_owned(), EvalValue::Int(n))]))
            .collect()
    }

    /// A comparator by `n` that counts its calls in `calls` and sets
    /// `flag` on call `trip`.
    fn tripping_compare<'a>(
        calls: &'a std::cell::Cell<usize>,
        flag: &'a std::sync::atomic::AtomicBool,
        trip: usize,
    ) -> impl FnMut(&Row, &Row) -> std::cmp::Ordering + 'a {
        move |a, b| {
            calls.set(calls.get() + 1);
            if calls.get() == trip {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            compare_cells(a.get("n"), b.get("n"))
        }
    }

    /// `dsl`'s pipeline compiled into the tail's steps, its patterns
    /// prepared on a ledger that reads `cancel`'s stop.
    fn cancellable_steps(dsl: &str, cancel: &CancelLatch) -> Vec<TailStep> {
        let tail = trawl_core::parser::parse(dsl).expect("dsl parses").pipeline;
        compile_tail(
            &tail,
            &PinScope::unpinned(),
            None,
            &mut ledger().with_stop(cancel.stop()),
        )
        .expect("the tail compiles")
    }

    /// A step's shape: its kind and its compiled pieces in order, each
    /// `head` with its count.
    fn shape(step: &TailStep) -> String {
        let piece = |stage: &CompiledStage| match stage {
            CompiledStage::Limit { count, .. } => format!("head {count}"),
            other => format!("{other:?}")
                .split(|c: char| !c.is_alphanumeric())
                .next()
                .unwrap_or_default()
                .to_owned(),
        };
        let pieces =
            |stages: &[CompiledStage]| stages.iter().map(piece).collect::<Vec<_>>().join(", ");
        match step {
            TailStep::Stream(StreamPlan::PassThrough(stages)) => {
                format!("pass [{}]", pieces(stages))
            }
            TailStep::Stream(StreamPlan::Aggregate {
                pre_stages,
                post_stages,
                ..
            }) => format!(
                "pass [{}] aggregate [{}]",
                pieces(pre_stages),
                pieces(post_stages)
            ),
            TailStep::Sort(sort) => format!(
                "sort {}",
                sort.fields
                    .iter()
                    .map(|field| match field.direction {
                        SortDirection::Asc => field.field.clone(),
                        SortDirection::Desc => format!("-{}", field.field),
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            TailStep::Dedup(dedup) => format!("dedup {}", dedup.fields.join(", "))
                .trim_end()
                .to_owned(),
        }
    }

    /// The plan of the tail without its barriers is cut back at each
    /// barrier's position, so every compiled piece runs in its own stage's
    /// place: each `head` keeps its own count, the aggregation lands in
    /// the run that wrote it, a run between two barriers holds exactly its
    /// own stages, and two barriers in a row leave no empty run between
    /// them.
    #[test]
    fn the_cut_puts_each_piece_at_its_stage() {
        let cases: &[(&str, &[&str])] = &[
            ("* | extract kv | sort n", &["pass [ExtractKv]", "sort n"]),
            ("* | sort n | head 2", &["sort n", "pass [head 2]"]),
            (
                "* | extract kv | head 1 | sort n | head 2 | sort -k, n | head 3 | table k",
                &[
                    "pass [ExtractKv, head 1]",
                    "sort n",
                    "pass [head 2]",
                    "sort -k, n",
                    "pass [head 3, Table]",
                ],
            ),
            (
                "* | extract kv | sort n | sort -k | where n > 1",
                &["pass [ExtractKv]", "sort n", "sort -k", "pass [Where]"],
            ),
            (
                "* | extract kv | head 5 | dedup k, n | head 2 | dedup | let m = n",
                &[
                    "pass [ExtractKv, head 5]",
                    "dedup k, n",
                    "pass [head 2]",
                    "dedup",
                    "pass [Let]",
                ],
            ),
            (
                "* | extract kv | dedup k | stats count() by k | dedup count | sort k",
                &[
                    "pass [ExtractKv]",
                    "dedup k",
                    "pass [] aggregate []",
                    "dedup count",
                    "sort k",
                ],
            ),
            (
                "* | extract kv | where n > 1 | stats count() by k | sort -count | head 1",
                &[
                    "pass [ExtractKv, Where] aggregate []",
                    "sort -count",
                    "pass [head 1]",
                ],
            ),
            (
                "* | extract kv | sort n | head 4 | stats count() by k | head 3 \
                 | sort -count | rename k as key",
                &[
                    "pass [ExtractKv]",
                    "sort n",
                    "pass [head 4] aggregate [head 3]",
                    "sort -count",
                    "pass [Rename]",
                ],
            ),
        ];
        for (dsl, expected) in cases {
            let steps = cancellable_steps(dsl, &CancelLatch::never());
            let shapes: Vec<String> = steps.iter().map(shape).collect();
            assert_eq!(shapes, *expected, "{dsl}");
        }
    }

    /// A sort step reads the lane's latch (ADR-0063): a sort cancelled
    /// before it runs ends as [`EngineError::Cancelled`] rather than
    /// answering with sorted rows, and so does a tail with no rows to
    /// sort, which only the reads around the steps can see.
    #[test]
    fn sort_steps_observe_cancellation() {
        let anchor = EvalContext::capture();
        let mut steps = cancellable_steps("* | sort -n | sort k", &CancelLatch::never());
        let rows = numbered([3, 1, 2]);

        let sorted = run_steps(&mut steps, rows.clone(), &anchor, &CancelLatch::never())
            .expect("an uncancelled sort answers");
        let order: Vec<_> = sorted.iter().map(|row| row.get("n").cloned()).collect();
        assert_eq!(
            order,
            [3, 2, 1].map(|n| Some(EvalValue::Int(n))),
            "sorted by n descending"
        );

        let (flag, cancel) = tripwire();
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
        let TailStep::Sort(sort) = &steps[0] else {
            panic!("a sort step: {}", shape(&steps[0]));
        };
        let outcome = sort_step(rows, sort, &cancel);
        assert!(
            matches!(outcome, Err(EngineError::Cancelled)),
            "rows: {outcome:?}"
        );
        let outcome = run_steps(&mut steps, Vec::new(), &anchor, &cancel);
        assert!(
            matches!(outcome, Err(EngineError::Cancelled)),
            "no rows: {outcome:?}"
        );
    }

    /// A dedup step reads the lane's latch in its passes: a cancelled
    /// dedup over more than one stride of rows ends as
    /// [`EngineError::Cancelled`] rather than answering with its winners.
    #[test]
    fn a_dedup_step_observes_cancellation() {
        let steps = cancellable_steps("* | dedup n", &CancelLatch::never());
        let TailStep::Dedup(dedup) = &steps[0] else {
            panic!("a dedup step: {}", shape(&steps[0]));
        };
        let len = i64::try_from(2 * TAIL_CANCEL_STRIDE).expect("fits");
        let rows = numbered((0..len).map(|n| n % 3));

        let kept = dedup_step(rows.clone(), dedup, &CancelLatch::never())
            .expect("an uncancelled dedup answers");
        let order: Vec<_> = kept.iter().map(|row| row.get("n").cloned()).collect();
        assert_eq!(
            order,
            [0, 1, 2].map(|n| Some(EvalValue::Int(n))),
            "the first row of each key, in input order"
        );

        let (flag, cancel) = tripwire();
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
        let outcome = dedup_step(rows, dedup, &cancel);
        assert!(
            matches!(outcome, Err(EngineError::Cancelled)),
            "{:?}",
            outcome.map(|rows| rows.len())
        );
    }

    /// A stop that lands as one step's input runs out ends the tail as
    /// [`EngineError::Cancelled`] before the step after it runs, so the
    /// `head` after the sort admits nothing. In the second tail the
    /// `where` keeps no row, so the sort and the `head` have no row to
    /// read the latch over, and only the reads between and after the
    /// steps can see the stop.
    #[test]
    fn the_tail_reads_the_latch_between_steps() {
        use std::sync::atomic::Ordering;

        const N: usize = 10;
        let anchor = EvalContext::capture();
        for dsl in [
            "* | extract kv | sort n | head 5",
            "* | extract kv | where n < 0 | sort n | head 5",
        ] {
            let (flag, cancel) = tripwire();
            let mut steps = cancellable_steps(dsl, &cancel);
            let events = rows_to_events(&kv_messages(N), &CancelLatch::never())
                .expect("a latch that never stops")
                .into_iter()
                .chain(std::iter::from_fn(|| {
                    flag.store(true, Ordering::SeqCst);
                    None
                }));
            let outcome = run_steps(&mut steps, events, &anchor, &cancel);
            assert!(
                matches!(outcome, Err(EngineError::Cancelled)),
                "{dsl}: {outcome:?}"
            );
            let TailStep::Stream(StreamPlan::PassThrough(after)) = &steps[2] else {
                panic!("{dsl}: a head step: {}", shape(&steps[2]));
            };
            assert_eq!(admitted(&after[0]), 0, "{dsl}: the head never ran");
        }
    }

    /// Sorting in strides and merging them answers exactly what one
    /// stable `sort_by` answers, ties in arrival order included, for an
    /// input over several strides with a ragged last one.
    #[test]
    fn a_strided_sort_matches_a_stable_sort() {
        let len = 3 * TAIL_CANCEL_STRIDE + TAIL_CANCEL_STRIDE / 2;
        let rows: Vec<Row> = (0..len)
            .map(|i| {
                let i = i64::try_from(i).expect("fits");
                Row::from([
                    ("n".to_owned(), EvalValue::Int((i * 7_919) % 97)),
                    ("arrival".to_owned(), EvalValue::Int(i)),
                ])
            })
            .collect();
        let by_n = |a: &Row, b: &Row| compare_cells(a.get("n"), b.get("n"));
        let mut expected = rows.clone();
        expected.sort_by(by_n);
        let sorted = sort_rows(rows, &CancelLatch::never(), by_n).expect("not cancelled");
        assert_eq!(sorted, expected);
    }

    /// A latch set inside a sort's comparator ends the sort within one
    /// stride's worth of work: the run being sorted finishes, and the
    /// next run reads the latch. Unstopped, the sort of sixteen strides
    /// makes about twenty times as many comparisons as the bound.
    #[test]
    fn a_sort_stops_within_a_stride_of_its_cancellation() {
        let stride = TAIL_CANCEL_STRIDE;
        // A fixed scramble, so no run arrives already in order.
        let mut state = 0x2545_f491_u64;
        let values = std::iter::repeat_with(|| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            i64::try_from(state >> 33).expect("31 bits fit")
        });
        let rows = numbered(values.take(16 * stride));

        let (flag, cancel) = tripwire();
        let calls = std::cell::Cell::new(0);
        let outcome = sort_rows(rows, &cancel, tripping_compare(&calls, &flag, 10));
        assert!(
            matches!(outcome, Err(EngineError::Cancelled)),
            "{:?}",
            outcome.map(|rows| rows.len())
        );
        let bound = 2 * stride * stride.ilog2() as usize;
        assert!(calls.get() <= bound, "{} comparisons", calls.get());
    }

    /// A latch set inside a merge's comparator ends the merge at the next
    /// stride of output, before the rest of the runs is read.
    #[test]
    fn a_merge_stops_within_a_stride_of_its_cancellation() {
        let stride = TAIL_CANCEL_STRIDE;
        let half = i64::try_from(4 * stride).expect("fits");
        let evens = numbered((0..half).map(|n| 2 * n));
        let odds = numbered((0..half).map(|n| 2 * n + 1));

        let (flag, cancel) = tripwire();
        let calls = std::cell::Cell::new(0);
        let outcome = merge_runs(
            evens,
            odds,
            &cancel,
            &mut tripping_compare(&calls, &flag, 10),
        );
        assert!(
            matches!(outcome, Err(EngineError::Cancelled)),
            "{:?}",
            outcome.map(|rows| rows.len())
        );
        assert!(calls.get() <= stride, "{} comparisons", calls.get());
    }

    /// The copies into and out of the tail read the latch too: a
    /// cancelled lane does not copy a whole result on either side.
    #[test]
    fn the_tail_copies_observe_cancellation() {
        let (flag, cancel) = tripwire();
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
        let input = kv_messages(2 * TAIL_CANCEL_STRIDE);
        assert!(
            matches!(rows_to_events(&input, &cancel), Err(EngineError::Cancelled)),
            "the copy into the tail"
        );

        let rows = rows_to_events(&input, &CancelLatch::never()).expect("a latch that never stops");
        assert!(
            matches!(
                rows_to_result_within(&rows, &[], &cancel),
                Err(EngineError::Cancelled)
            ),
            "the copy out of the tail"
        );
        assert!(
            matches!(
                result_columns(&rows, &[], &cancel),
                Err(EngineError::Cancelled)
            ),
            "the names pass of the copy out"
        );
    }

    /// Runs that do not overlap end the comparisons early, and the rest
    /// of the other run is moved after them. That move reads the latch
    /// too, so a stop during the last comparison ends the merge.
    #[test]
    fn a_merge_drain_stops_at_its_cancellation() {
        let stride = TAIL_CANCEL_STRIDE;
        let low = i64::try_from(stride).expect("fits");
        let left = numbered(0..low);
        let right = numbered(low..5 * low);

        let (flag, cancel) = tripwire();
        let calls = std::cell::Cell::new(0);
        let outcome = merge_runs(
            left,
            right,
            &cancel,
            &mut tripping_compare(&calls, &flag, stride),
        );
        assert!(
            matches!(outcome, Err(EngineError::Cancelled)),
            "{:?}",
            outcome.map(|rows| rows.len())
        );
    }

    fn run_tail(dsl: &str, result: QueryResult) -> Result<QueryResult, EngineError> {
        let tail = trawl_core::parser::parse(dsl).expect("dsl parses").pipeline;
        apply_rust_stages(
            result,
            &tail,
            &PinScope::unpinned(),
            EvalContext::capture(),
            None,
            &mut ledger(),
            &CancelLatch::never(),
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
