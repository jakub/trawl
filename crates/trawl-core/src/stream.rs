// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Streaming pipeline compiler and executor.
//!
//! Compiles pipe stages from the AST into an in-memory evaluation plan
//! for the SSE streaming path. Two modes:
//!
//! - **pass-through**: events flow through per-event transforms (table,
//!   drop, rename, where, let, extract, dedup, limit, tail).
//! - **aggregation**: events feed into accumulators, periodic snapshots
//!   are emitted (stats, timechart, top, rare).

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::ast::{
    AggExpr, DedupStage, DropStage, Expr, ExtractMode, ExtractStage, LetStage, LimitStage,
    LiteralValue, PipeStage, RenameStage, Spanned, TableStage, WhereStage,
};
use crate::context::EvalContext;
use crate::emitter::{
    format_literal_position, unit_literal_positions, validate_format_literal,
    validate_function_arity, validate_unit_literal,
};
use crate::eval::{EvalValue, bind_event_key, eval_expr_with_pins};
use crate::pin_scope::PinScope;
use crate::row::{self, Row};

// ── stream plan ────────────────────────────────────────────────────

/// A compiled streaming evaluation plan.
pub enum StreamPlan {
    /// Events flow through per-event transforms individually.
    PassThrough(Vec<CompiledStage>),
    /// Events feed into accumulators; periodic snapshots are emitted.
    Aggregate {
        pre_stages: Vec<CompiledStage>,
        aggregation: CompiledAggregation,
        post_stages: Vec<CompiledStage>,
    },
}

impl fmt::Debug for StreamPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PassThrough(stages) => f.debug_tuple("PassThrough").field(&stages.len()).finish(),
            Self::Aggregate { .. } => f.debug_struct("Aggregate").finish_non_exhaustive(),
        }
    }
}

/// Errors from compiling a stream plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamPlanError {
    /// A pipe stage is not supported in streaming mode.
    UnsupportedStage { stage: String, reason: String },
    /// A regex pattern failed to compile.
    InvalidRegex(String),
    /// A date/time unit argument is invalid (non-literal or not allowlisted).
    InvalidUnit(String),
    /// A `strftime`/`strptime` format string literal contains an invalid code.
    InvalidFormat(String),
    /// A function call the emitter's own tables refuse — an unknown name
    /// or the wrong argument count. This lane never runs
    /// `validate_pipeline`, so it mirrors the check itself: otherwise
    /// `let s = sev()` opens a live-looking stream that can only ever
    /// evaluate to NULL while `/api/v1/query` 400s on the same text.
    InvalidFunction(String),
    /// A pinned comparison the shared rule table refuses — an unknown
    /// severity value against a `SEVERITY`-pinned field (ADR-0013). The
    /// SQL emitter errors on exactly this shape, so the stream must too:
    /// eval has no error channel and would open a live-looking stream
    /// that can never match.
    InvalidComparison(String),
    /// A pipeline write position naming trawl's `_` namespace — an
    /// `extract` capture group, the one such name that is not already a
    /// parse error (ADR-0013 §5). The SQL lane refuses it in
    /// `emitter::validate_pipeline`, which this lane never runs.
    ReservedName(String),
    /// A projecting stage that would mint two columns of one name
    /// (ADR-0013 ruling 8). Same reason as `ReservedName`: the sentence
    /// comes from the one shared check in `crate::projection`, which the
    /// SQL lane reaches through `validate_pipeline` and this lane
    /// reaches itself.
    ProjectionCollision(String),
}

impl fmt::Display for StreamPlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedStage { stage, reason } => {
                write!(f, "{stage} is not supported in streaming mode: {reason}")
            }
            Self::InvalidRegex(msg) => write!(f, "invalid regex: {msg}"),
            Self::InvalidUnit(msg) => write!(f, "invalid date/time unit: {msg}"),
            Self::InvalidFormat(msg) => write!(f, "invalid date/time format: {msg}"),
            // Verbatim: the emitter (InvalidFunction) and the shared
            // projection check (ProjectionCollision) own their whole
            // sentence.
            Self::InvalidFunction(msg) | Self::ProjectionCollision(msg) => {
                write!(f, "{msg}")
            }
            Self::InvalidComparison(msg) => write!(f, "invalid comparison: {msg}"),
            Self::ReservedName(msg) => write!(f, "unsupported operation: {msg}"),
        }
    }
}

impl std::error::Error for StreamPlanError {}

/// Compile pipe stages into a streaming evaluation plan.
///
/// Rejects unsupported stages (sort, pivot, multiple aggregations)
/// with an error before the stream starts.
///
/// `pins` is the pin scope in force at the first stage of `pipeline`
/// (ADR-0011): the catalog snapshot's root for a whole-pipeline SSE
/// stream, or the scope stamped at the kv split for a `rust_stages`
/// batch tail. The compiler walks the same per-stage scope table the SQL
/// emitter consumes (`crate::pin_scope`), stamping each `where`/`let`
/// with the scope it must evaluate under. There is no default, so
/// pin-blindness is explicit at the call site (`&PinScope::unpinned()`).
pub fn compile_stream_plan(
    pipeline: &[Spanned<PipeStage>],
    pins: &PinScope,
) -> Result<StreamPlan, StreamPlanError> {
    // The shared projection-name check runs first, over every stage —
    // before the unsupported-stage and multi-aggregation refusals — so a
    // `pivot`/`eventstats` collision gets the semantic answer even where
    // the stage itself is not streamable (ADR-0013 ruling 8).
    for stage in pipeline {
        crate::projection::check_projection(&stage.node)
            .map_err(StreamPlanError::ProjectionCollision)?;
    }

    let agg_idx = pipeline.iter().position(|s| is_agg_stage(&s.node));

    // The scope advances per stage — one walk, shared with the emitter.
    let mut scope = pins.clone();

    if let Some(idx) = agg_idx {
        if pipeline[idx + 1..].iter().any(|s| is_agg_stage(&s.node)) {
            return Err(StreamPlanError::UnsupportedStage {
                stage: "multiple aggregations".to_string(),
                reason: "only one aggregation stage is supported in streaming mode".to_string(),
            });
        }

        let mut pre_stages = Vec::new();
        for spanned in &pipeline[..idx] {
            pre_stages.push(compile_per_event_stage(spanned, &scope)?);
            scope.advance(&spanned.node);
        }

        let aggregation = compile_aggregation(&pipeline[idx].node)?;
        scope.advance(&pipeline[idx].node);

        let mut post_stages = Vec::new();
        for spanned in &pipeline[idx + 1..] {
            post_stages.push(compile_per_event_stage(spanned, &scope)?);
            scope.advance(&spanned.node);
        }

        Ok(StreamPlan::Aggregate {
            pre_stages,
            aggregation,
            post_stages,
        })
    } else {
        let mut stages = Vec::new();
        for spanned in pipeline {
            stages.push(compile_per_event_stage(spanned, &scope)?);
            scope.advance(&spanned.node);
        }
        Ok(StreamPlan::PassThrough(stages))
    }
}

fn is_agg_stage(stage: &PipeStage) -> bool {
    matches!(
        stage,
        PipeStage::Stats(_) | PipeStage::Timechart(_) | PipeStage::Top(_) | PipeStage::Rare(_)
    )
}

fn compile_per_event_stage(
    spanned: &Spanned<PipeStage>,
    scope: &PinScope,
) -> Result<CompiledStage, StreamPlanError> {
    match &spanned.node {
        PipeStage::Table(s) => Ok(compile_table(s)),
        PipeStage::Drop(s) => Ok(compile_drop(s)),
        PipeStage::Rename(s) => Ok(compile_rename(s)),
        PipeStage::Limit(s) => Ok(compile_limit(s)),
        PipeStage::Tail(s) => Ok(CompiledStage::Tail { count: s.count }),
        PipeStage::Where(s) => compile_where(s, scope),
        PipeStage::Let(s) => compile_let(s, scope),
        PipeStage::Extract(s) => compile_extract(s),
        PipeStage::Dedup(s) => Ok(compile_dedup(s)),

        PipeStage::Sort(_) => Err(StreamPlanError::UnsupportedStage {
            stage: "sort".to_string(),
            reason: "contradicts real-time arrival order".to_string(),
        }),
        PipeStage::Pivot(_) => Err(StreamPlanError::UnsupportedStage {
            stage: "pivot".to_string(),
            reason: "dynamic column structure breaks progressive rendering".to_string(),
        }),

        PipeStage::Stats(_) | PipeStage::Timechart(_) | PipeStage::Top(_) | PipeStage::Rare(_) => {
            Err(StreamPlanError::UnsupportedStage {
                stage: stage_name(&spanned.node).to_string(),
                reason: "aggregation stages cannot appear as pre/post stages".to_string(),
            })
        }
        PipeStage::Sample(_) => Err(StreamPlanError::UnsupportedStage {
            stage: "sample".to_string(),
            reason: "statistical sampling requires the full dataset".to_string(),
        }),
        PipeStage::EventStats(_) => Err(StreamPlanError::UnsupportedStage {
            stage: "eventstats".to_string(),
            reason: "window functions require the full dataset".to_string(),
        }),
        PipeStage::FromSaved(_) => Err(StreamPlanError::UnsupportedStage {
            stage: "from".to_string(),
            reason: "saved query sources are not supported in streaming mode".to_string(),
        }),
    }
}

fn stage_name(stage: &PipeStage) -> &'static str {
    match stage {
        PipeStage::Stats(_) => "stats",
        PipeStage::Where(_) => "where",
        PipeStage::Sort(_) => "sort",
        PipeStage::Limit(_) => "limit",
        PipeStage::Table(_) => "table",
        PipeStage::Top(_) => "top",
        PipeStage::Rare(_) => "rare",
        PipeStage::Drop(_) => "drop",
        PipeStage::Let(_) => "let",
        PipeStage::Extract(_) => "extract",
        PipeStage::Dedup(_) => "dedup",
        PipeStage::Timechart(_) => "timechart",
        PipeStage::Pivot(_) => "pivot",
        PipeStage::Tail(_) => "tail",
        PipeStage::Rename(_) => "rename",
        PipeStage::Sample(_) => "sample",
        PipeStage::EventStats(_) => "eventstats",
        PipeStage::FromSaved(_) => "from",
    }
}

// ── compiled stages ────────────────────────────────────────────────

/// A single compiled per-event stage.
///
/// Cannot derive `Debug` due to `AtomicU64` and `regex::Regex` fields.
pub enum CompiledStage {
    /// Keep only the specified fields (ordered as the user wrote them).
    Table { fields: Vec<String> },
    /// Remove the specified fields.
    Drop { fields: HashSet<String> },
    /// Rename fields (from → to).
    Rename { renames: Vec<(String, String)> },
    /// Cap output to N events.
    ///
    /// `count` is the cap as written, kept beside the live counter
    /// because a post-aggregation limit is re-armed for every emitted
    /// snapshot ([`reset_limit`](CompiledStage::reset_limit)).
    Limit { count: u64, remaining: AtomicU64 },
    /// Pass through (ring buffer sizing is handled by the TUI).
    Tail { count: u64 },
    /// Filter events by condition.
    Where {
        condition: Spanned<crate::ast::Expr>,
        /// The pin scope in force at this stage (ADR-0011).
        pins: PinScope,
    },
    /// Compute derived fields.
    Let {
        assignments: Vec<(String, Spanned<crate::ast::Expr>)>,
        /// The pin scope in force at this stage (ADR-0011).
        pins: PinScope,
    },
    /// Extract fields via regex.
    ExtractRegex {
        regex: regex::Regex,
        source_field: String,
    },
    /// Extract key-value pairs.
    ExtractKv {
        source_field: String,
        separator: char,
    },
    /// Deduplicate by field values.
    Dedup {
        fields: Vec<String>,
        seen: HashSet<Vec<String>>,
        max_entries: usize,
    },
}

impl fmt::Debug for CompiledStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Table { fields } => f.debug_struct("Table").field("fields", fields).finish(),
            Self::Drop { fields } => f.debug_struct("Drop").field("fields", fields).finish(),
            Self::Rename { renames } => f.debug_struct("Rename").field("renames", renames).finish(),
            Self::Limit { count, remaining } => f
                .debug_struct("Limit")
                .field("count", count)
                .field("remaining", &remaining.load(Ordering::Relaxed))
                .finish(),
            Self::Tail { count } => f.debug_struct("Tail").field("count", count).finish(),
            Self::Where { .. } => f.debug_struct("Where").finish_non_exhaustive(),
            Self::Let { assignments, .. } => f
                .debug_struct("Let")
                .field("num_assignments", &assignments.len())
                .finish(),
            Self::ExtractRegex { source_field, .. } => f
                .debug_struct("ExtractRegex")
                .field("source_field", source_field)
                .finish_non_exhaustive(),
            Self::ExtractKv {
                source_field,
                separator,
            } => f
                .debug_struct("ExtractKv")
                .field("source_field", source_field)
                .field("separator", separator)
                .finish(),
            Self::Dedup {
                fields,
                max_entries,
                ..
            } => f
                .debug_struct("Dedup")
                .field("fields", fields)
                .field("max_entries", max_entries)
                .finish(),
        }
    }
}

impl CompiledStage {
    /// Re-arm a `limit`, and leave every other stage alone.
    ///
    /// The one caller is [`emit_snapshot`], because the two ends of an
    /// aggregate plan cap two different things (ADR-0001: batch is the
    /// contract, streaming the mirror):
    ///
    /// - a pre-aggregation `limit` caps the input set, which batch also
    ///   does exactly once, so its exhaustion stays sticky for the life
    ///   of the subscription;
    /// - a post-aggregation `limit` caps the result set, and an emitted
    ///   snapshot is the live rendering of the batch result set, so it
    ///   caps each snapshot as batch caps its one. Spending it for the
    ///   plan's life would leave the stream permanently silent while the
    ///   aggregation kept evolving, which mirrors nothing.
    ///
    /// Deliberately narrow: a `dedup` in the same position keeps its
    /// seen-set across snapshots, which is a separate question about
    /// what a live `dedup` means and is not answered here.
    fn reset_limit(&mut self) {
        if let Self::Limit { count, remaining } = self {
            remaining.store(*count, Ordering::Relaxed);
        }
    }
}

/// Result of applying a stage to an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageResult {
    /// Event passes through (possibly mutated).
    Pass,
    /// Event is filtered out.
    Filtered,
    /// Stream is done (e.g. limit reached).
    Done,
}

// ── the live lane's sampling boundaries (ADR-0017 §3) ──────────────

/// What one live event produced.
///
/// The live lane's unit of output is the event, so this is also the
/// unit an [`EvalContext`] covers: everything that reads `now()` on the
/// way from the bus to the wire — the search-stage window, a
/// `| where now() - _time < …`, a `| let age = now()` — reads the one
/// instant [`accept_event`] was handed.
#[derive(Debug, Clone, PartialEq)]
pub enum LiveOutcome {
    /// The event matched and survived every stage: the row to emit.
    Emit(Row),
    /// The filter rejected it, or a stage dropped it.
    Filtered,
    /// A `limit` stage ended the subscription. The event is not emitted
    /// (the stage that says `Done` has already refused it).
    Done,
}

/// The one per-event door of the live lane: match the search stage,
/// then run the pipeline stages, both under a single instant.
///
/// One-context-per-event is structural here: one `ctx` parameter, and no
/// clock read anywhere below it, so the filter's `last=` window and the
/// pipeline's `now()` cannot sample two instants and admit an event a
/// later stage then rejects for being too old.
///
/// The caller samples: one [`EvalContext::capture`] per event, at the
/// top of the loop. A subscription's clock therefore advances between
/// rows while each row stays internally frozen, which is what ADR-0017
/// §3 asks of live pass-through (per-batch sampling is rejected: a bus
/// batch is an upstream client's POST size, not a boundary the query
/// author can see).
pub fn accept_event(
    filter: &crate::filter::CompiledFilter,
    stages: &mut [CompiledStage],
    event: &serde_json::Map<String, serde_json::Value>,
    ctx: &EvalContext,
) -> LiveOutcome {
    // The search-stage filter reads the bus JSON directly — no
    // conversion on the firehose, only on the events that match.
    if !filter.matches_at(event, ctx) {
        return LiveOutcome::Filtered;
    }

    let mut row = row::from_json(event);
    for stage in stages.iter_mut() {
        match apply_stage(stage, &mut row, ctx) {
            StageResult::Pass => {}
            StageResult::Filtered => return LiveOutcome::Filtered,
            StageResult::Done => return LiveOutcome::Done,
        }
    }
    LiveOutcome::Emit(row)
}

/// The aggregate lane's per-event door: the same [`accept_event`] rule,
/// with the surviving row fed into the accumulators.
///
/// Returns whether the event reached the aggregation — the caller's
/// snapshot-threshold counter. A pre-stage `Done` drops the event and
/// does not end the subscription: an aggregate stream's output is the
/// snapshot, and a `limit` before the aggregation bounds what feeds it.
///
/// Both `now()` readers on this path — the filter window and any
/// `| where`/`| let` before the aggregation — plus the timechart
/// bucket's absent-`_time` fallback take the one `ctx` handed in, so a
/// fed event is bucketed at the instant it was admitted under.
pub fn accept_event_into_aggregate(
    filter: &crate::filter::CompiledFilter,
    pre_stages: &mut [CompiledStage],
    aggregation: &mut CompiledAggregation,
    event: &serde_json::Map<String, serde_json::Value>,
    ctx: &EvalContext,
) -> bool {
    match accept_event(filter, pre_stages, event, ctx) {
        LiveOutcome::Emit(row) => {
            aggregation.feed_event(&row, ctx);
            true
        }
        LiveOutcome::Filtered | LiveOutcome::Done => false,
    }
}

/// The instant one emitted aggregate snapshot reads `now()` at.
///
/// A distinct type, not a second `EvalContext` parameter, because the
/// two boundaries meet in one function: a snapshot's post-stage rows are
/// one unit of output together (ADR-0017 §3), while the events that fed
/// that snapshot each sampled their own instant, possibly seconds
/// earlier. Neither can be passed where the other is expected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotContext(EvalContext);

impl SnapshotContext {
    /// The snapshot's instant, sampled by the caller at the start of the
    /// snapshot attempt, before the rows are taken, so the sample point
    /// is deterministic even when the post-stages later drop every row.
    #[must_use]
    pub fn new(at: EvalContext) -> Self {
        Self(at)
    }
}

/// Take an aggregate snapshot and run its post-stages, every row under
/// the one snapshot instant.
///
/// Each pass re-arms the post-stages' `limit`s
/// ([`CompiledStage::reset_limit`]): a snapshot is the live rendering of
/// the batch result set, so `| stats … | limit N` caps every snapshot at
/// N, as batch caps its one result set. The plan's pre-stages are not
/// touched here and stay sticky — they cap the input.
///
/// Deliberately not shared with `post_process::apply_aggregate`, whose
/// `StageResult::Done` ends the whole result set: here it drops the
/// current row and the next row still gets its chance. Merging the two
/// loops would silently change one lane's row set. (That lane emits one
/// snapshot from a freshly compiled plan, so its counters start armed
/// and this re-arming would be a no-op there.)
pub fn emit_snapshot(
    aggregation: &CompiledAggregation,
    post_stages: &mut [CompiledStage],
    ctx: &SnapshotContext,
) -> (Vec<String>, Vec<Row>) {
    for stage in post_stages.iter_mut() {
        stage.reset_limit();
    }
    let (columns, rows) = aggregation.snapshot();
    let rows = rows
        .into_iter()
        .filter_map(|mut row| {
            for stage in post_stages.iter_mut() {
                match apply_stage(stage, &mut row, &ctx.0) {
                    StageResult::Pass => {}
                    StageResult::Filtered | StageResult::Done => return None,
                }
            }
            Some(row)
        })
        .collect();
    (columns, rows)
}

// ── stage compilation ──────────────────────────────────────────────

fn compile_table(s: &TableStage) -> CompiledStage {
    CompiledStage::Table {
        fields: s
            .fields
            .iter()
            .map(std::string::ToString::to_string)
            .collect(),
    }
}

fn compile_drop(s: &DropStage) -> CompiledStage {
    CompiledStage::Drop {
        fields: s
            .fields
            .iter()
            .map(std::string::ToString::to_string)
            .collect(),
    }
}

fn compile_rename(s: &RenameStage) -> CompiledStage {
    CompiledStage::Rename {
        renames: s
            .renames
            .iter()
            .map(|(from, to)| (from.clone(), to.clone()))
            .collect(),
    }
}

fn compile_limit(s: &LimitStage) -> CompiledStage {
    CompiledStage::Limit {
        count: s.count,
        remaining: AtomicU64::new(s.count),
    }
}

fn compile_where(s: &WhereStage, scope: &PinScope) -> Result<CompiledStage, StreamPlanError> {
    validate_expr(&s.condition, scope)?;
    Ok(CompiledStage::Where {
        condition: s.condition.clone(),
        pins: scope.clone(),
    })
}

fn compile_let(s: &LetStage, scope: &PinScope) -> Result<CompiledStage, StreamPlanError> {
    for (_, expr) in &s.assignments {
        validate_expr(expr, scope)?;
    }
    Ok(CompiledStage::Let {
        assignments: s.assignments.clone(),
        pins: scope.clone(),
    })
}

/// Walk an expression tree and validate function names and arity,
/// closed-vocabulary literals, and pinned comparisons.
///
/// These are the checks `emit_expr` performs on the batch path, so an
/// unsupported `date_part` unit, an unknown `sev()` dialect, an invalid
/// `strftime`/`strptime` format literal, or an unknown severity token
/// against a `SEVERITY`-pinned field is rejected at `compile_stream_plan`
/// time rather than silently evaluating to `Null` in live tail while
/// batch 400s on the same text.
fn validate_expr(
    expr: &Spanned<crate::ast::Expr>,
    scope: &PinScope,
) -> Result<(), StreamPlanError> {
    match &expr.node {
        Expr::FunctionCall { name, args } => {
            // Arity and name, from the emitter's single source of truth,
            // for every function and not just the ones with literal
            // positions: a wrong-arity call is a 400 in batch, and a
            // stream that silently nulls instead is the divergence this
            // check exists to prevent.
            validate_function_arity(name, args.len())
                .map_err(|e| StreamPlanError::InvalidFunction(e.to_string()))?;
            let unit_positions = unit_literal_positions(name);
            for (idx, allowlist) in unit_positions {
                let arg = args.get(*idx);
                let raw = arg.and_then(|a| match &a.node {
                    Expr::Literal(LiteralValue::String(s)) => Some(s.as_str()),
                    _ => None,
                });
                // An absent position is an optional argument (`sev(x)`
                // with no dialect); a wrong arity was refused above.
                if arg.is_some() {
                    validate_unit_literal(name, *idx, allowlist, raw)
                        .map_err(|e| StreamPlanError::InvalidUnit(e.to_string()))?;
                }
            }
            // strftime/strptime format literal: reject invalid codes up front. A
            // non-literal format keeps its runtime behaviour (eval nulls).
            if let Some(idx) = format_literal_position(name)
                && let Some(Expr::Literal(LiteralValue::String(s))) = args.get(idx).map(|a| &a.node)
            {
                validate_format_literal(name, s)
                    .map_err(|e| StreamPlanError::InvalidFormat(e.to_string()))?;
            }
            for arg in args {
                validate_expr(arg, scope)?;
            }
        }
        Expr::Binary { lhs, op, rhs } => {
            // The pinned rule table refuses an unknown severity value,
            // and it must refuse it here: the SQL emitter 400s on the
            // same shape, and eval (the only other consumer of this
            // scope) has no error channel at all.
            validate_pinned_comparison(lhs, *op, rhs, scope)?;
            validate_expr(lhs, scope)?;
            validate_expr(rhs, scope)?;
        }
        Expr::Unary { operand, .. } => validate_expr(operand, scope)?,
        Expr::InList { expr: target, list } => {
            validate_pinned_in_list(target, list, scope)?;
            validate_expr(target, scope)?;
            for item in list {
                validate_expr(item, scope)?;
            }
        }
        Expr::Literal(_) | Expr::FieldRef(_) => {}
    }
    Ok(())
}

/// Resolve a bare field-vs-literal comparison through the shared pin rule
/// table, for its error alone: the compiled form is re-resolved per
/// event by `crate::eval`, which reads the same scope.
fn validate_pinned_comparison(
    lhs: &Spanned<crate::ast::Expr>,
    op: crate::ast::BinaryOp,
    rhs: &Spanned<crate::ast::Expr>,
    scope: &PinScope,
) -> Result<(), StreamPlanError> {
    use crate::ast::BinaryOp;
    let filter_op = match op {
        BinaryOp::Eq => crate::ast::FilterOp::Eq,
        BinaryOp::Ne => crate::ast::FilterOp::Ne,
        BinaryOp::Gt => crate::ast::FilterOp::Gt,
        BinaryOp::Gte => crate::ast::FilterOp::Gte,
        BinaryOp::Lt => crate::ast::FilterOp::Lt,
        BinaryOp::Lte => crate::ast::FilterOp::Lte,
        _ => return Ok(()),
    };
    // The same classifier the emitter and eval consume, so a subject
    // that binds there binds here — and its refusal is the same sentence.
    let resolved = match (scope.subject_pin(lhs), scope.subject_pin(rhs)) {
        (Some((_, pin)), _) => crate::eval::bare_literal_of(&rhs.node).map(|lit| (pin, lit)),
        (None, Some((_, pin))) => crate::eval::bare_literal_of(&lhs.node).map(|lit| (pin, lit)),
        (None, None) => None,
    };
    let Some((pin, literal)) = resolved else {
        return Ok(());
    };
    crate::compare::compare_form_bound(Some(pin), filter_op, &literal)
        .map(|_| ())
        .map_err(|e| StreamPlanError::InvalidComparison(e.to_string()))
}

/// The IN-list half of [`validate_pinned_comparison`]: every element
/// binds through the equality rule, so every element can refuse.
fn validate_pinned_in_list(
    target: &Spanned<crate::ast::Expr>,
    list: &[Spanned<crate::ast::Expr>],
    scope: &PinScope,
) -> Result<(), StreamPlanError> {
    let Some((_, pin)) = scope.subject_pin(target) else {
        return Ok(());
    };
    for item in list {
        let Some(literal) = crate::eval::bare_literal_of(&item.node) else {
            continue;
        };
        crate::compare::compare_form_bound(Some(pin), crate::ast::FilterOp::Eq, &literal)
            .map_err(|e| StreamPlanError::InvalidComparison(e.to_string()))?;
    }
    Ok(())
}

fn compile_extract(s: &ExtractStage) -> Result<CompiledStage, StreamPlanError> {
    let source_field = s.source_field.as_deref().unwrap_or("message").to_string();

    match &s.mode {
        ExtractMode::Regex(pattern) => {
            let regex = regex::Regex::new(pattern)
                .map_err(|e| StreamPlanError::InvalidRegex(e.to_string()))?;
            // The `_` namespace is sealed at both pipeline write positions
            // (ADR-0013 §5), and this lane is a door of its own: SSE parses
            // and compiles straight to a stream plan, never through
            // `emitter::validate_pipeline`. Without this mirror of
            // `validate_extract`'s check, `extract "(?P<_severity>…)"` would
            // 400 in batch and stream happily live, letting a message body
            // overwrite trawl's own derived verdict slot.
            for name in regex.capture_names().flatten() {
                if crate::schema::is_reserved_name(name) {
                    return Err(StreamPlanError::ReservedName(
                        crate::schema::reserved_name_message("extract capture group", name),
                    ));
                }
            }
            if let Some(message) =
                crate::schema::duplicate_target_message(regex.capture_names().flatten(), "extract")
            {
                return Err(StreamPlanError::ProjectionCollision(message));
            }
            Ok(CompiledStage::ExtractRegex {
                regex,
                source_field,
            })
        }
        ExtractMode::KeyValue { separator } => Ok(CompiledStage::ExtractKv {
            source_field,
            separator: *separator,
        }),
    }
}

fn compile_dedup(s: &DedupStage) -> CompiledStage {
    CompiledStage::Dedup {
        fields: s
            .fields
            .iter()
            .map(std::string::ToString::to_string)
            .collect(),
        seen: HashSet::new(),
        max_entries: 10_000,
    }
}

/// Read a live field using `DuckDB`'s ASCII-insensitive identifier binding.
fn event_value<'e>(event: &'e Row, name: &str) -> Option<&'e EvalValue> {
    let key = bind_event_key(event, name)?;
    event.get(key)
}

/// A field's text for an identity or a display — [`row::cell_text`] for a
/// cell the row carries, and the empty string for one it does not (an
/// absent field is not a NULL one).
fn event_text(event: &Row, name: &str) -> String {
    event_value(event, name).map_or_else(String::new, row::cell_text)
}

// ── stage application ──────────────────────────────────────────────

/// Apply a `rename` stage in place, with the SQL's parallel semantics.
///
/// The batch lane emits `* EXCLUDE (sources), src AS tgt, …`, so every
/// target reads the pre-stage row: `rename a as b, b as c` gives `b` the
/// original `a` and `c` the original `b`, never the just-renamed value.
/// A source this event does not carry makes its target absent (the SQL
/// column would be NULL) rather than leaving the target's own stale
/// value behind — the same rule [`PinScope::advance`] applies to pins,
/// so value and pin can never come from different columns.
///
/// Each source binds to the row's own spelling ([`bind_event_key`]), for
/// the same reason [`PinScope::advance`] resolves its pin through
/// [`crate::schema::catalog_key`]: `DuckDB` binds the emitted
/// `"Status" AS "st"` to an ingest-folded `status` column
/// case-insensitively, so `rename Status as st` has to carry the value
/// across in this lane too, and the key removed is the one that bound,
/// never the verbatim source.
fn apply_rename(renames: &[(String, String)], event: &mut Row) {
    let sources: Vec<Option<String>> = renames
        .iter()
        .map(|(from, _)| bind_event_key(event, from).map(str::to_owned))
        .collect();
    let resolved: Vec<(&str, Option<EvalValue>)> = renames
        .iter()
        .zip(&sources)
        .map(|((_, to), source)| {
            (
                to.as_str(),
                source.as_ref().and_then(|key| event.get(key)).cloned(),
            )
        })
        .collect();
    for source in sources.iter().flatten() {
        event.remove(source.as_str());
    }
    for (to, value) in resolved {
        remove_folded_twins(event, to);
        match value {
            Some(v) => {
                event.insert(to.to_string(), v);
            }
            None => {
                event.remove(to);
            }
        }
    }
}

/// Remove other spellings of the column a projection is about to own.
fn remove_folded_twins(event: &mut Row, name: &str) {
    let folded = crate::schema::catalog_key(name);
    let twins: Vec<String> = event
        .keys()
        .filter(|key| key.as_str() != name && crate::schema::catalog_key(key) == folded)
        .cloned()
        .collect();
    for twin in twins {
        event.remove(&twin);
    }
}

/// Apply a `let` stage in place, with the SQL's column-then-alias
/// resolution.
///
/// The batch lane desugars the whole stage into one projection
/// (`COLUMNS(c -> c NOT IN (targets)), (expr) AS tgt, …`), and `DuckDB`
/// binds a name inside it the way it binds any name in a `SELECT` list:
/// an input column wins, and only a name resolving to no input column
/// falls through to the lateral column alias a sibling just defined. Both
/// halves are load-bearing, so this mirrors both:
///
/// - a target that shadows a column the row carries never feeds its
///   siblings — `let a = 1, b = a` and `let a = a + 1, b = a` over a row
///   with an `a` both give `b` the original `a`; "carries" is `DuckDB`'s
///   own case-insensitive binding ([`bind_event_key`]), so `let A = 1,
///   b = A` shadows an `a` too;
/// - a target the row does not carry — the ordinary case, since `let`
///   usually names something new — is the sibling's binding:
///   `let ms = 1000, total = ms * 2` gives `total = 2000`, matching
///   `/api/v1/query` (this lane is also the `rust_stages` batch tail
///   behind `extract kv`, where there is no SQL lane to fall back on).
///
/// Pins do not follow the alias: [`PinScope::advance`] resolves every
/// assignment's pin against the pre-stage scope, so an alias-bound
/// sibling is unpinned — conservative, and identical in both lanes
/// because both consume that one walk.
///
/// The residual is the row-vs-relation gap: a column the corpus carries
/// but this row leaves absent (a sparse custom field) is a NULL column
/// read in batch, while the live lane, seeing no key, binds the alias.
fn apply_let(
    assignments: &[(String, Spanned<crate::ast::Expr>)],
    pins: &PinScope,
    event: &mut Row,
    ctx: &EvalContext,
) {
    // Decided against the pre-stage row, before any alias lands: these
    // targets name a real column, so they stay invisible to their
    // siblings and their new values are applied only at the end.
    // "Names a real column" is DuckDB's own binding rule
    // (`bind_event_key`), not an exact key match: a target spelled
    // `Dur` shadows the row's `dur` exactly as a reference to it would
    // bind that column.
    let shadowing: Vec<bool> = assignments
        .iter()
        .map(|(name, _)| bind_event_key(event, name).is_some())
        .collect();
    let mut resolved: Vec<(&str, EvalValue)> = Vec::with_capacity(assignments.len());
    for ((name, expr), shadows_column) in assignments.iter().zip(shadowing) {
        // Stored as the evaluator produced it: a JSON round trip here
        // would turn `0/0` into NULL one stage before the query asks
        // about it (see `crate::row`).
        let value = eval_expr_with_pins(expr, event, pins, ctx);
        if !shadows_column {
            // The lateral alias: a later sibling naming this target finds
            // no input column and reads what was just computed.
            event.insert(name.clone(), value.clone());
        }
        resolved.push((name.as_str(), value));
    }
    for (name, value) in resolved {
        remove_folded_twins(event, name);
        event.insert(name.to_string(), value);
    }
}

/// The text an `extract` stage reads out of its source cell — the one
/// answer both modes use, so the regex and kv arms cannot disagree about
/// which cells an extraction reaches.
///
/// A string cell extracts, and so does a stage-computed instant, read as
/// its `CAST(… AS VARCHAR)` text ([`row::cell_text`] over
/// [`crate::compare::Instant::cast_text`]), so
/// `let t = strptime(…) | extract … from t` sees the timestamp text. A
/// number or a boolean is not text and extracts nothing.
///
/// The timestamp arm is the one place this lane answers where the SQL
/// lane cannot: `DuckDB` has no `regexp_extract(TIMESTAMP, …)` overload
/// and does not implicitly cast to VARCHAR, so the batch lane refuses
/// such a query outright (Binder Error, probed in `stage_parity`), and
/// behind `extract kv` there is no SQL lane at all — this code is the
/// batch tail.
fn extract_source_text(event: &Row, source_field: &str) -> Option<String> {
    match event_value(event, source_field)? {
        EvalValue::Str(text) => Some(text.clone()),
        cell @ EvalValue::Timestamp(_) => Some(row::cell_text(cell)),
        _ => None,
    }
}

/// Apply regex extraction with the same write-always, empty-is-NULL
/// semantics as the emitted `nullif(regexp_extract(...), '')` projection.
fn apply_extract_regex(regex: &regex::Regex, source_field: &str, event: &mut Row) {
    let text = extract_source_text(event, source_field);
    let captures = text.as_deref().and_then(|text| regex.captures(text));
    let written: Vec<(String, EvalValue)> = regex
        .capture_names()
        .flatten()
        .map(|name| {
            let value = captures
                .as_ref()
                .and_then(|captures| captures.name(name))
                .filter(|capture| !capture.as_str().is_empty())
                .map_or(EvalValue::Null, |capture| {
                    EvalValue::Str(capture.as_str().to_string())
                });
            (name.to_string(), value)
        })
        .collect();
    for (name, value) in written {
        remove_folded_twins(event, &name);
        event.insert(name, value);
    }
}

/// Apply a compiled stage to an event, mutating it in place.
///
/// Returns whether the event should pass through, be filtered, or
/// the stream is done.
///
/// `ctx` is the evaluation context this row is being processed under —
/// the instant its `now()` reads (ADR-0017 §3). The caller owns the
/// question of what a "unit of output" is: the batch tail behind
/// `extract kv` passes the statement's anchor for every row, while the
/// live lane samples per event.
pub fn apply_stage(stage: &mut CompiledStage, event: &mut Row, ctx: &EvalContext) -> StageResult {
    match stage {
        CompiledStage::Table { fields } => {
            let keep: Vec<String> = fields
                .iter()
                .filter_map(|field| bind_event_key(event, field).map(str::to_owned))
                .collect();
            event.retain(|key, _| keep.iter().any(|kept| kept == key));
            StageResult::Pass
        }

        CompiledStage::Drop { fields } => {
            for field in fields.iter() {
                if let Some(key) = bind_event_key(event, field).map(str::to_owned) {
                    event.remove(&key);
                }
            }
            StageResult::Pass
        }

        CompiledStage::Rename { renames } => {
            apply_rename(renames, event);
            StageResult::Pass
        }

        CompiledStage::Limit { remaining, .. } => {
            // Exhaustion is sticky, and `checked_sub` is what makes it
            // so: a plain `fetch_sub` on a zero counter wraps to
            // `u64::MAX`, which would say `Done` once and then let the
            // next event pass. The lanes that keep asking after a `Done`
            // — the aggregate feed, which runs for the life of the
            // subscription, and a snapshot's post-stages, which walk
            // every row — depend on it.
            if remaining
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
                    left.checked_sub(1)
                })
                .is_ok()
            {
                StageResult::Pass
            } else {
                StageResult::Done
            }
        }

        CompiledStage::Tail { .. } => {
            // tail is handled at the buffer level, just pass through
            StageResult::Pass
        }

        CompiledStage::Where { condition, pins } => {
            let result = eval_expr_with_pins(condition, event, pins, ctx);
            if result.is_truthy() {
                StageResult::Pass
            } else {
                StageResult::Filtered
            }
        }

        CompiledStage::Let { assignments, pins } => {
            apply_let(assignments, pins, event, ctx);
            StageResult::Pass
        }

        CompiledStage::ExtractRegex {
            regex,
            source_field,
        } => {
            apply_extract_regex(regex, source_field, event);
            StageResult::Pass
        }

        CompiledStage::ExtractKv {
            source_field,
            separator,
        } => {
            if let Some(text) = extract_source_text(event, source_field) {
                let pairs = extract_key_value_pairs(&text, *separator);
                for (k, v) in pairs {
                    // The `_` namespace is sealed against log content too
                    // (ADR-0013 §1): a kv key is sender-controlled text, so
                    // an inserted `_severity`/`_time` would let a message
                    // body forge trawl's own verdict slots in both the SSE
                    // lane and the batch tail. The pair is dropped — the
                    // text stays findable in the source field and `_raw`.
                    if crate::schema::is_reserved_name(&k) {
                        continue;
                    }
                    remove_folded_twins(event, &k);
                    event.insert(k, coerce_kv_value(v));
                }
            }
            StageResult::Pass
        }

        CompiledStage::Dedup {
            fields,
            seen,
            max_entries,
        } => {
            let key = dedup_key(fields, event);
            if seen.contains(&key) {
                StageResult::Filtered
            } else {
                if seen.len() >= *max_entries {
                    // evict by clearing (simple strategy — the set is bounded)
                    seen.clear();
                }
                seen.insert(key);
                StageResult::Pass
            }
        }
    }
}

/// Build a dedup key from field values. Empty fields → dedup on all values.
fn dedup_key(fields: &[String], event: &Row) -> Vec<String> {
    if fields.is_empty() {
        // dedup on entire event — every cell, kind-tagged so a string
        // and a number of the same text stay different rows
        // (`row::cell_key`).
        let mut pairs: Vec<_> = event
            .iter()
            .map(|(k, v)| format!("{k}={}", row::cell_key(v)))
            .collect();
        pairs.sort();
        pairs
    } else {
        fields
            .iter()
            .map(|field| event_text(event, field))
            .collect()
    }
}

/// Coerce a string value from kv extraction into the most specific cell
/// type.
///
/// Tries integer, then float, then boolean, falling back to string.
///
/// The finiteness guard is deliberate: a kv pair is sender text out of a
/// log line, with no SQL lane to agree with, so `x=inf` stays the string
/// `"inf"` it reads as rather than becoming an infinity the sender never
/// wrote.
pub fn coerce_kv_value(s: String) -> EvalValue {
    if let Ok(i) = s.parse::<i64>() {
        return EvalValue::Int(i);
    }
    if let Ok(f) = s.parse::<f64>()
        && f.is_finite()
    {
        return EvalValue::Float(f);
    }
    match s.as_str() {
        "true" => EvalValue::Bool(true),
        "false" => EvalValue::Bool(false),
        _ => EvalValue::Str(s),
    }
}

/// Extract key-value pairs from a string using the given separator.
///
/// Handles both `key<sep>value` and `key<sep>"quoted value"` formats.
pub fn extract_key_value_pairs(text: &str, separator: char) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut chars = text.char_indices().peekable();

    while let Some((i, c)) = chars.peek().copied() {
        // skip non-alphanumeric/underscore chars
        if !c.is_alphanumeric() && c != '_' {
            chars.next();
            continue;
        }

        // try to find key<sep>value
        let key_start = i;
        while let Some(&(_, c)) = chars.peek() {
            if c.is_alphanumeric() || c == '_' || c == '.' {
                chars.next();
            } else {
                break;
            }
        }

        let key_end = chars.peek().map_or(text.len(), |&(i, _)| i);

        if chars.peek().is_some_and(|&(_, c)| c == separator) {
            chars.next(); // consume separator
            let key = &text[key_start..key_end];

            if let Some(&(_, '"')) = chars.peek() {
                // quoted value
                chars.next(); // consume opening quote
                let val_start = chars.peek().map_or(text.len(), |&(i, _)| i);
                while let Some(&(_, c)) = chars.peek() {
                    if c == '"' {
                        break;
                    }
                    chars.next();
                }
                let val_end = chars.peek().map_or(text.len(), |&(i, _)| i);
                chars.next(); // consume closing quote
                pairs.push((key.to_string(), text[val_start..val_end].to_string()));
            } else {
                // unquoted value — ends at whitespace or comma
                let val_start = chars.peek().map_or(text.len(), |&(i, _)| i);
                while let Some(&(_, c)) = chars.peek() {
                    if c.is_whitespace() || c == ',' {
                        break;
                    }
                    chars.next();
                }
                let val_end = chars.peek().map_or(text.len(), |&(i, _)| i);
                if val_start < val_end {
                    pairs.push((key.to_string(), text[val_start..val_end].to_string()));
                }
            }
        }
    }

    pairs
}

// ── aggregation ────────────────────────────────────────────────────

/// Maximum number of groups before we stop tracking new ones.
const MAX_GROUPS: usize = 10_000;
/// Maximum values stored per `Dc`/`Values` accumulator.
const MAX_DISTINCT: usize = 10_000;
/// Maximum values stored per `Median`/`Percentile` accumulator.
const MAX_EXACT_VALUES: usize = 100_000;

/// A compiled aggregation stage for streaming evaluation.
pub enum CompiledAggregation {
    /// `| stats count(), avg(x) by host`
    Stats {
        accumulators: Vec<CompiledAcc>,
        group_by: Vec<String>,
        groups: HashMap<GroupKey, Vec<AccState>>,
    },
    /// `| timechart span=5m count() by service`
    Timechart {
        span_secs: u64,
        accumulators: Vec<CompiledAcc>,
        group_by: Vec<String>,
        buckets: HashMap<i64, HashMap<GroupKey, Vec<AccState>>>,
    },
    /// `| top 10 host by service`
    Top {
        count: u64,
        field: String,
        by: Vec<String>,
        counters: HashMap<GroupKey, HashMap<String, u64>>,
    },
    /// `| rare 5 status`
    Rare {
        count: u64,
        field: String,
        by: Vec<String>,
        counters: HashMap<GroupKey, HashMap<String, u64>>,
    },
}

impl fmt::Debug for CompiledAggregation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stats { group_by, .. } => f
                .debug_struct("Stats")
                .field("group_by", group_by)
                .finish_non_exhaustive(),
            Self::Timechart { span_secs, .. } => f
                .debug_struct("Timechart")
                .field("span_secs", span_secs)
                .finish_non_exhaustive(),
            Self::Top { count, field, .. } => f
                .debug_struct("Top")
                .field("count", count)
                .field("field", field)
                .finish_non_exhaustive(),
            Self::Rare { count, field, .. } => f
                .debug_struct("Rare")
                .field("count", count)
                .field("field", field)
                .finish_non_exhaustive(),
        }
    }
}

type GroupKey = Vec<String>;

/// A compiled accumulator descriptor (what function + which field).
#[derive(Debug, Clone)]
pub struct CompiledAcc {
    pub function: String,
    pub field: Option<String>,
    pub alias: String,
    /// For percentile functions, the target percentile (e.g. 0.95).
    pub percentile: Option<f64>,
}

/// Runtime state for a single accumulator instance.
#[derive(Debug, Clone)]
pub enum AccState {
    Count(u64),
    CountField { non_null: u64 },
    Sum(f64),
    Avg { sum: f64, count: u64 },
    Min(Option<f64>),
    Max(Option<f64>),
    Dc(HashSet<String>),
    First(Option<EvalValue>),
    Last(Option<EvalValue>),
    Values(HashSet<String>),
    Median(Vec<f64>),
    Stddev(WelfordState),
    Percentile { values: Vec<f64>, target: f64 },
}

/// Welford's online algorithm state for computing stddev.
#[derive(Debug, Clone)]
pub struct WelfordState {
    count: u64,
    mean: f64,
    m2: f64,
}

impl WelfordState {
    fn new() -> Self {
        Self {
            count: 0,
            mean: 0.0,
            m2: 0.0,
        }
    }

    #[allow(clippy::cast_precision_loss)]
    fn update(&mut self, x: f64) {
        self.count += 1;
        let delta = x - self.mean;
        self.mean += delta / self.count as f64;
        let delta2 = x - self.mean;
        self.m2 += delta * delta2;
    }

    #[allow(clippy::cast_precision_loss)]
    fn stddev(&self) -> Option<f64> {
        if self.count < 2 {
            None
        } else {
            Some((self.m2 / (self.count - 1) as f64).sqrt())
        }
    }
}

fn new_acc_state(acc: &CompiledAcc) -> AccState {
    match acc.function.as_str() {
        "count" => {
            if acc.field.is_some() {
                AccState::CountField { non_null: 0 }
            } else {
                AccState::Count(0)
            }
        }
        "sum" => AccState::Sum(0.0),
        "avg" => AccState::Avg { sum: 0.0, count: 0 },
        "min" => AccState::Min(None),
        "max" => AccState::Max(None),
        "dc" | "distinct_count" => AccState::Dc(HashSet::new()),
        "first" => AccState::First(None),
        "last" => AccState::Last(None),
        "values" | "list" => AccState::Values(HashSet::new()),
        "median" => AccState::Median(Vec::new()),
        "stddev" => AccState::Stddev(WelfordState::new()),
        "p50" => AccState::Percentile {
            values: Vec::new(),
            target: 0.5,
        },
        "p90" => AccState::Percentile {
            values: Vec::new(),
            target: 0.9,
        },
        "p95" => AccState::Percentile {
            values: Vec::new(),
            target: 0.95,
        },
        "p99" => AccState::Percentile {
            values: Vec::new(),
            target: 0.99,
        },
        _ => AccState::Count(0), // fallback
    }
}

fn compile_agg_expr(agg: &AggExpr, stage: &str) -> Result<CompiledAcc, StreamPlanError> {
    // The live accumulators read a bare field out of the event; they have
    // no expression evaluator. A computed argument would therefore feed
    // nothing and answer NULL under a column the SQL lane fills with a
    // real value, and since both lanes agree on the output name
    // (ADR-0013 ruling 8) that divergence would be invisible. Refuse it
    // instead.
    let field = match agg.args.first() {
        None => None,
        Some(a) => match &a.node {
            crate::ast::Expr::FieldRef(name) => Some(name.clone()),
            crate::ast::Expr::Literal(lit)
                if agg.function == "count" && !matches!(lit, crate::ast::LiteralValue::Null) =>
            {
                // SQL COUNT(non-null constant) is COUNT(*). The row-count
                // accumulator represents that exactly; COUNT(NULL) does not.
                None
            }
            _ => {
                return Err(StreamPlanError::UnsupportedStage {
                    stage: stage.to_string(),
                    reason: format!(
                        "{}(…) over a computed argument is not supported in streaming mode; \
                         aggregate a bare field",
                        agg.function
                    ),
                });
            }
        },
    };

    // The one output-name derivation, shared with the SQL emitter and
    // the pin-scope walk (ADR-0013 ruling 8): a computed argument names
    // its innermost field here exactly as it does in batch.
    let alias = crate::projection::agg_output_name(agg);

    let percentile = match agg.function.as_str() {
        "p50" => Some(0.5),
        "p90" => Some(0.9),
        "p95" => Some(0.95),
        "p99" => Some(0.99),
        _ => None,
    };

    Ok(CompiledAcc {
        function: agg.function.clone(),
        field,
        alias,
        percentile,
    })
}

fn compile_aggregation(stage: &PipeStage) -> Result<CompiledAggregation, StreamPlanError> {
    match stage {
        PipeStage::Stats(s) => {
            let accumulators = s
                .aggregations
                .iter()
                .map(|a| compile_agg_expr(a, "stats"))
                .collect::<Result<Vec<_>, _>>()?;
            let group_by: Vec<_> = s
                .group_by
                .iter()
                .map(std::string::ToString::to_string)
                .collect();
            Ok(CompiledAggregation::Stats {
                accumulators,
                group_by,
                groups: HashMap::new(),
            })
        }
        PipeStage::Timechart(s) => {
            let span_secs = s
                .span
                .as_ref()
                .map_or(60, crate::ast::TrawlDuration::to_seconds);
            let accumulators = s
                .aggregations
                .iter()
                .map(|a| compile_agg_expr(a, "timechart"))
                .collect::<Result<Vec<_>, _>>()?;
            let group_by: Vec<_> = s
                .group_by
                .iter()
                .map(std::string::ToString::to_string)
                .collect();
            Ok(CompiledAggregation::Timechart {
                span_secs,
                accumulators,
                group_by,
                buckets: HashMap::new(),
            })
        }
        PipeStage::Top(s) => Ok(CompiledAggregation::Top {
            count: s.count,
            field: s.field.clone(),
            by: s.by.iter().map(std::string::ToString::to_string).collect(),
            counters: HashMap::new(),
        }),
        PipeStage::Rare(s) => Ok(CompiledAggregation::Rare {
            count: s.count,
            field: s.field.clone(),
            by: s.by.iter().map(std::string::ToString::to_string).collect(),
            counters: HashMap::new(),
        }),
        _ => Err(StreamPlanError::UnsupportedStage {
            stage: "unknown".to_string(),
            reason: "not an aggregation stage".to_string(),
        }),
    }
}

impl CompiledAggregation {
    /// Feed an event into the aggregation accumulators.
    ///
    /// `ctx` is the event's own evaluation context (ADR-0017 §3): the
    /// timechart bucket falls back to `now()` when the row carries no
    /// readable `_time`, and that reading must be the instant the event
    /// was admitted under, not a fresh clock — a second read would put
    /// the event in a different span bucket than the one its own
    /// context names. In the batch tail behind `extract kv` this is the
    /// statement anchor, for every row alike.
    pub fn feed_event(&mut self, event: &Row, ctx: &EvalContext) {
        match self {
            Self::Stats {
                accumulators,
                group_by,
                groups,
            } => {
                let key = make_group_key(group_by, event);
                if groups.len() >= MAX_GROUPS && !groups.contains_key(&key) {
                    return;
                }
                let states = groups
                    .entry(key)
                    .or_insert_with(|| accumulators.iter().map(new_acc_state).collect());
                for (acc, state) in accumulators.iter().zip(states.iter_mut()) {
                    feed_acc(acc, state, event);
                }
            }
            Self::Timechart {
                span_secs,
                accumulators,
                group_by,
                buckets,
            } => {
                let bucket = event_time_bucket(event, *span_secs, ctx);
                let key = make_group_key(group_by, event);
                let group_map = buckets.entry(bucket).or_default();
                if group_map.len() >= MAX_GROUPS && !group_map.contains_key(&key) {
                    return;
                }
                let states = group_map
                    .entry(key)
                    .or_insert_with(|| accumulators.iter().map(new_acc_state).collect());
                for (acc, state) in accumulators.iter().zip(states.iter_mut()) {
                    feed_acc(acc, state, event);
                }
            }
            Self::Top {
                field,
                by,
                counters,
                ..
            }
            | Self::Rare {
                field,
                by,
                counters,
                ..
            } => {
                let group = make_group_key(by, event);
                let field_val = event_text(event, field);
                let counts = counters.entry(group).or_default();
                *counts.entry(field_val).or_insert(0) += 1;
            }
        }
    }

    /// Take a snapshot of the current aggregation state as rows.
    ///
    /// Returns `(column_names, rows)` where each row is a [`Row`].
    pub fn snapshot(&self) -> (Vec<String>, Vec<Row>) {
        match self {
            Self::Stats {
                accumulators,
                group_by,
                groups,
            } => snapshot_stats(accumulators, group_by, groups),
            Self::Timechart {
                accumulators,
                group_by,
                buckets,
                span_secs,
            } => snapshot_timechart(accumulators, group_by, buckets, *span_secs),
            Self::Top {
                count,
                field,
                by,
                counters,
            } => snapshot_frequency(counters, field, by, *count, true),
            Self::Rare {
                count,
                field,
                by,
                counters,
            } => snapshot_frequency(counters, field, by, *count, false),
        }
    }
}

fn snapshot_stats(
    accumulators: &[CompiledAcc],
    group_by: &[String],
    groups: &HashMap<GroupKey, Vec<AccState>>,
) -> (Vec<String>, Vec<Row>) {
    let mut columns: Vec<String> = group_by.to_vec();
    columns.extend(accumulators.iter().map(|a| a.alias.clone()));

    let mut rows = Vec::new();
    for (key, states) in groups {
        let mut row = Row::new();
        for (col, val) in group_by.iter().zip(key.iter()) {
            row.insert(col.clone(), EvalValue::Str(val.clone()));
        }
        for (acc, state) in accumulators.iter().zip(states.iter()) {
            row.insert(acc.alias.clone(), snapshot_acc(state));
        }
        rows.push(row);
    }
    (columns, rows)
}

#[allow(clippy::cast_possible_wrap)]
fn snapshot_timechart(
    accumulators: &[CompiledAcc],
    group_by: &[String],
    buckets: &HashMap<i64, HashMap<GroupKey, Vec<AccState>>>,
    span_secs: u64,
) -> (Vec<String>, Vec<Row>) {
    let mut columns = vec!["_time".to_string()];
    columns.extend(group_by.iter().cloned());
    columns.extend(accumulators.iter().map(|a| a.alias.clone()));

    let mut rows = Vec::new();
    let mut sorted_buckets: Vec<_> = buckets.keys().copied().collect();
    sorted_buckets.sort_unstable();

    for bucket in sorted_buckets {
        if let Some(group_map) = buckets.get(&bucket) {
            for (key, states) in group_map {
                let mut row = Row::new();
                let ts = chrono::DateTime::from_timestamp(bucket * span_secs as i64, 0)
                    .map_or_else(|| bucket.to_string(), |dt| dt.to_rfc3339());
                row.insert("_time".to_string(), EvalValue::Str(ts));
                for (col, val) in group_by.iter().zip(key.iter()) {
                    row.insert(col.clone(), EvalValue::Str(val.clone()));
                }
                for (acc, state) in accumulators.iter().zip(states.iter()) {
                    row.insert(acc.alias.clone(), snapshot_acc(state));
                }
                rows.push(row);
            }
        }
    }
    (columns, rows)
}

/// Snapshot helper for top/rare — identical except sort direction.
/// `descending=true` for top, `false` for rare.
#[allow(clippy::cast_possible_truncation)]
fn snapshot_frequency(
    counters: &HashMap<GroupKey, HashMap<String, u64>>,
    field: &str,
    by: &[String],
    count: u64,
    descending: bool,
) -> (Vec<String>, Vec<Row>) {
    let mut columns: Vec<String> = by.to_vec();
    columns.push(field.to_string());
    columns.push("count".to_string());

    let mut rows = Vec::new();
    for (group_key, counts) in counters {
        let mut sorted: Vec<_> = counts.iter().collect();
        if descending {
            sorted.sort_by(|a, b| b.1.cmp(a.1));
        } else {
            sorted.sort_by(|a, b| a.1.cmp(b.1));
        }
        for (val, cnt) in sorted.into_iter().take(count as usize) {
            let mut row = Row::new();
            for (col, gv) in by.iter().zip(group_key.iter()) {
                row.insert(col.clone(), EvalValue::Str(gv.clone()));
            }
            row.insert(field.to_string(), EvalValue::Str(val.clone()));
            row.insert("count".to_string(), row::count_value(*cnt));
            rows.push(row);
        }
    }
    (columns, rows)
}

fn make_group_key(group_by: &[String], event: &Row) -> GroupKey {
    group_by
        .iter()
        .map(|field| event_text(event, field))
        .collect()
}

/// The span bucket an event lands in.
///
/// The fallback for a row with no readable `_time` is `now()`, and
/// `now()` here is the event's context, the same instant its filter
/// window and its `| where` read. Sampling a clock of its own would
/// make a bucketless event land in a bucket nothing else in the query
/// can name, and across a span boundary that is a different row in the
/// snapshot.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_wrap)]
fn event_time_bucket(event: &Row, span_secs: u64, ctx: &EvalContext) -> i64 {
    // The exact `_time` key, not `bind_event_key` as the rest of this
    // lane uses: ingest folds every name to lowercase and the pipeline
    // cannot mint a reserved one, so a trawl-written row has no case
    // variant to bind.
    if let Some(EvalValue::Str(ts)) = event.get("_time")
        && let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts)
    {
        return dt.timestamp() / span_secs as i64;
    }
    // Fallback: the context's instant, never a fresh clock read.
    ctx.now_utc().timestamp() / span_secs as i64
}

/// The numeric reading an accumulator takes off a cell.
///
/// Numbers only, never a numeric-looking string: widening it would make
/// `sum(x)` start counting text the SQL lane does not.
#[allow(clippy::cast_precision_loss)]
fn extract_f64(event: &Row, field: &str) -> Option<f64> {
    event_value(event, field).and_then(|v| match v {
        EvalValue::Int(n) => Some(*n as f64),
        // A JSON number above `i64::MAX` feeds `sum`/`avg`/`min`/`max`
        // too, rounded to the nearest `f64`.
        EvalValue::UInt(n) => Some(*n as f64),
        EvalValue::Float(f) => Some(*f),
        _ => None,
    })
}

fn feed_acc(acc: &CompiledAcc, state: &mut AccState, event: &Row) {
    match state {
        AccState::Count(n) => *n += 1,
        AccState::CountField { non_null } => {
            if let Some(field) = &acc.field
                && event_value(event, field).is_some_and(|v| !matches!(v, EvalValue::Null))
            {
                *non_null += 1;
            }
        }
        AccState::Sum(total) => {
            if let Some(field) = &acc.field
                && let Some(v) = extract_f64(event, field)
            {
                *total += v;
            }
        }
        AccState::Avg { sum, count } => {
            if let Some(field) = &acc.field
                && let Some(v) = extract_f64(event, field)
            {
                *sum += v;
                *count += 1;
            }
        }
        // `f64::min`/`f64::max` ignore a NaN (`f64::max(NaN, 3.0)` is
        // 3.0), which would make `max(x)` skip the very value DuckDB
        // orders greatest. Both extremes go through the one probed order
        // instead (`crate::compare::double_total_cmp`).
        AccState::Min(current) => {
            if let Some(field) = &acc.field
                && let Some(v) = extract_f64(event, field)
            {
                *current = Some(current.map_or(v, |c| {
                    if crate::compare::double_total_cmp(v, c).is_lt() {
                        v
                    } else {
                        c
                    }
                }));
            }
        }
        AccState::Max(current) => {
            if let Some(field) = &acc.field
                && let Some(v) = extract_f64(event, field)
            {
                *current = Some(current.map_or(v, |c| {
                    if crate::compare::double_total_cmp(v, c).is_gt() {
                        v
                    } else {
                        c
                    }
                }));
            }
        }
        AccState::Dc(set) | AccState::Values(set) => {
            feed_acc_string_set(acc, set, event);
        }
        AccState::First(stored) => {
            if stored.is_none()
                && let Some(field) = &acc.field
                && let Some(v) = event_value(event, field)
            {
                *stored = Some(v.clone());
            }
        }
        AccState::Last(stored) => {
            if let Some(field) = &acc.field
                && let Some(v) = event_value(event, field)
            {
                *stored = Some(v.clone());
            }
        }
        AccState::Median(values) | AccState::Percentile { values, .. } => {
            feed_acc_f64_vec(acc, values, event, MAX_EXACT_VALUES);
        }
        AccState::Stddev(welford) => {
            if let Some(field) = &acc.field
                && let Some(v) = extract_f64(event, field)
            {
                welford.update(v);
            }
        }
    }
}

fn feed_acc_string_set(acc: &CompiledAcc, set: &mut HashSet<String>, event: &Row) {
    if let Some(field) = &acc.field
        && set.len() < MAX_DISTINCT
        && let Some(v) = event_value(event, field)
        && !matches!(v, EvalValue::Null)
    {
        set.insert(row::cell_text(v));
    }
}

fn feed_acc_f64_vec(acc: &CompiledAcc, values: &mut Vec<f64>, event: &Row, max: usize) {
    if let Some(field) = &acc.field
        && values.len() < max
        && let Some(v) = extract_f64(event, field)
    {
        values.push(v);
    }
}

/// An accumulator's value as a cell.
///
/// A computed double is stored as one, non-finite included: nulling a
/// value JSON cannot spell is a wire concern the wire door owns
/// ([`row::to_json`]). An empty accumulator is NULL.
#[allow(clippy::cast_precision_loss)]
fn snapshot_acc(state: &AccState) -> EvalValue {
    match state {
        AccState::Count(n) => row::count_value(*n),
        AccState::CountField { non_null } => row::count_value(*non_null),
        AccState::Sum(total) => EvalValue::Float(*total),
        AccState::Avg { sum, count } => {
            if *count == 0 {
                EvalValue::Null
            } else {
                EvalValue::Float(*sum / *count as f64)
            }
        }
        AccState::Min(v) | AccState::Max(v) => v.map_or(EvalValue::Null, EvalValue::Float),
        AccState::Dc(set) => row::count_value(set.len() as u64),
        AccState::First(v) | AccState::Last(v) => v.clone().unwrap_or(EvalValue::Null),
        AccState::Values(set) => {
            let mut vals: Vec<_> = set.iter().cloned().collect();
            vals.sort();
            EvalValue::Array(vals.into_iter().map(EvalValue::Str).collect())
        }
        AccState::Median(values) => snapshot_median(values),
        AccState::Stddev(welford) => welford.stddev().map_or(EvalValue::Null, EvalValue::Float),
        AccState::Percentile { values, target } => snapshot_percentile(values, *target),
    }
}

/// Order a sample for the positional aggregates.
///
/// The probed total order puts every NaN at the top, which is where
/// `DuckDB` sorts it. `partial_cmp(…).unwrap_or(Equal)` would leave a
/// NaN wherever it happened to sit and make `median(x)` depend on
/// arrival order.
fn sort_sample(values: &[f64]) -> Vec<f64> {
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| crate::compare::double_total_cmp(*a, *b));
    sorted
}

fn snapshot_median(values: &[f64]) -> EvalValue {
    if values.is_empty() {
        return EvalValue::Null;
    }
    let sorted = sort_sample(values);
    let mid = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        EvalValue::Float(f64::midpoint(sorted[mid - 1], sorted[mid]))
    } else {
        EvalValue::Float(sorted[mid])
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn snapshot_percentile(values: &[f64], target: f64) -> EvalValue {
    if values.is_empty() {
        return EvalValue::Null;
    }
    let sorted = sort_sample(values);
    let idx = (target * (sorted.len() - 1) as f64).round() as usize;
    EvalValue::Float(sorted[idx.min(sorted.len() - 1)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{
        AggExpr, BinaryOp, DedupStage, DropStage, Expr, ExtractMode, ExtractStage, LetStage,
        LimitStage, LiteralValue, PipeStage, RareStage, RenameStage, StatsStage, TableStage,
        TopStage, WhereStage,
    };
    use serde_json::Value;
    use serde_json::json;

    fn span<T>(node: T) -> Spanned<T> {
        Spanned { node, span: 0..0 }
    }

    /// The evaluation context these tests evaluate under.
    ///
    /// A fixed instant, not a capture: nothing below reaches for a
    /// clock, so neither does its fixture — the cases that care about
    /// `now()` name their own instant and assert against it.
    fn ctx() -> EvalContext {
        EvalContext::at(
            chrono::DateTime::parse_from_rfc3339("2026-08-24T12:00:00Z")
                .expect("literal is RFC 3339")
                .with_timezone(&chrono::Utc),
        )
    }

    fn event(pairs: &Value) -> Row {
        crate::row::from_json(pairs.as_object().unwrap())
    }

    /// A row's cell as JSON, so an assertion can compare it against a
    /// bare literal (`serde_json::Value` knows how to compare itself to
    /// a `&str`, an integer, a float…).
    fn cell(row: &Row, key: &str) -> Value {
        Value::from(row.get(key).cloned().expect("cell present"))
    }

    /// The same, for the assertions that distinguish "absent" from
    /// "present and null".
    fn cell_opt(row: &Row, key: &str) -> Option<Value> {
        row.get(key).cloned().map(Value::from)
    }

    // ── compile_stream_plan: rejection ─────────────────────────────

    #[test]
    fn rejects_sort() {
        let pipeline = vec![span(PipeStage::Sort(crate::ast::SortStage {
            fields: vec![crate::ast::SortField {
                field: "count".into(),
                direction: crate::ast::SortDirection::Desc,
            }],
        }))];
        let err = compile_stream_plan(&pipeline, &PinScope::unpinned()).unwrap_err();
        assert!(err.to_string().contains("sort"));
        assert!(err.to_string().contains("arrival order"));
    }

    #[test]
    fn rejects_pivot() {
        let pipeline = vec![span(PipeStage::Pivot(crate::ast::PivotStage {
            aggregation: crate::ast::AggExpr {
                function: "count".into(),
                args: vec![],
                alias: None,
            },
            on_field: "status".into(),
            by: vec![],
        }))];
        let err = compile_stream_plan(&pipeline, &PinScope::unpinned()).unwrap_err();
        assert!(err.to_string().contains("pivot"));
    }

    #[test]
    fn accepts_empty_pipeline() {
        let plan = compile_stream_plan(&[], &PinScope::unpinned()).unwrap();
        assert!(matches!(plan, StreamPlan::PassThrough(stages) if stages.is_empty()));
    }

    /// The timechart bucket reads the ingress `_time`, which is a wire
    /// string: JSON has no timestamp type, and no stage can mint one
    /// under that name (`_time` is reserved, so `let`/`rename` refuse
    /// it). A retype of this read would silently bucket every event at
    /// the current time through the fallback, which no assertion about
    /// counts would catch, so the bucket itself is asserted.
    #[test]
    fn the_time_bucket_reads_the_wire_string() {
        let ev = event(&json!({"_time": "2026-01-15T09:07:00Z", "service": "nginx"}));
        let span: u64 = 300; // five minutes
        let bucket = event_time_bucket(&ev, span, &ctx());
        let expected = chrono::DateTime::parse_from_rfc3339("2026-01-15T09:07:00Z")
            .unwrap()
            .timestamp()
            / i64::try_from(span).unwrap();
        assert_eq!(bucket, expected);

        // …and an event with no `_time` falls back to the context's
        // instant, a different bucket — the failure mode the assertion
        // above rules out for a real event.
        let bucketless = event(&json!({"service": "nginx"}));
        assert_ne!(event_time_bucket(&bucketless, span, &ctx()), expected);
    }

    // ── tier 1: table ──────────────────────────────────────────────

    #[test]
    fn table_retains_specified_fields() {
        let mut stage = compile_table(&TableStage {
            fields: vec!["host".into(), "service".into()],
            keyword: "table",
        });
        let mut ev = event(
            &json!({"host": "web-1", "service": "nginx", "message": "hello", "level": "info"}),
        );
        assert_eq!(apply_stage(&mut stage, &mut ev, &ctx()), StageResult::Pass);
        assert_eq!(ev.len(), 2);
        assert!(ev.contains_key("host"));
        assert!(ev.contains_key("service"));
    }

    #[test]
    fn table_stores_fields_in_user_order() {
        let stage = compile_table(&TableStage {
            fields: vec![
                "timestamp".into(),
                "event_type".into(),
                "target".into(),
                "message".into(),
            ],
            keyword: "table",
        });
        let CompiledStage::Table { ref fields } = stage else {
            panic!("expected Table stage");
        };
        assert_eq!(
            fields,
            &["timestamp", "event_type", "target", "message"],
            "Table stage must store fields verbatim, in user-specified order"
        );
    }

    #[test]
    fn table_maps_field_names() {
        let mut stage = compile_table(&TableStage {
            fields: vec!["_time".into(), "host".into()],
            keyword: "table",
        });
        let mut ev = event(&json!({"_time": "2026-01-01", "host": "web-1", "message": "hi"}));
        assert_eq!(apply_stage(&mut stage, &mut ev, &ctx()), StageResult::Pass);
        assert!(ev.contains_key("_time"));
        assert!(ev.contains_key("host"));
        assert!(!ev.contains_key("message"));
    }

    // ── tier 1: drop ───────────────────────────────────────────────

    #[test]
    fn drop_removes_specified_fields() {
        let mut stage = compile_drop(&DropStage {
            fields: vec!["message".into(), "raw".into()],
        });
        let mut ev = event(&json!({"host": "web-1", "message": "hello", "raw": "bytes"}));
        assert_eq!(apply_stage(&mut stage, &mut ev, &ctx()), StageResult::Pass);
        assert_eq!(ev.len(), 1);
        assert!(ev.contains_key("host"));
    }

    #[test]
    fn drop_ignores_missing_fields() {
        let mut stage = compile_drop(&DropStage {
            fields: vec!["nonexistent".into()],
        });
        let mut ev = event(&json!({"host": "web-1"}));
        assert_eq!(apply_stage(&mut stage, &mut ev, &ctx()), StageResult::Pass);
        assert_eq!(ev.len(), 1);
    }

    // ── tier 1: rename ─────────────────────────────────────────────

    #[test]
    fn rename_renames_field() {
        let mut stage = compile_rename(&RenameStage {
            renames: vec![("service".into(), "svc".into())],
        });
        let mut ev = event(&json!({"service": "nginx", "host": "web-1"}));
        assert_eq!(apply_stage(&mut stage, &mut ev, &ctx()), StageResult::Pass);
        assert_eq!(cell(&ev, "svc"), "nginx");
        assert!(!ev.contains_key("service"));
    }

    #[test]
    fn rename_multiple() {
        let mut stage = compile_rename(&RenameStage {
            renames: vec![
                ("service".into(), "svc".into()),
                ("host".into(), "hostname".into()),
            ],
        });
        let mut ev = event(&json!({"service": "nginx", "host": "web-1"}));
        apply_stage(&mut stage, &mut ev, &ctx());
        assert!(ev.contains_key("svc"));
        assert!(ev.contains_key("hostname"));
        assert!(!ev.contains_key("service"));
        assert!(!ev.contains_key("host"));
    }

    #[test]
    fn rename_chain_reads_the_pre_stage_event() {
        // SQL: `* EXCLUDE (a, b), a AS b, b AS c` — `b` takes the
        // original `a`, `c` the original `b`, never the just-renamed one.
        let mut stage = compile_rename(&RenameStage {
            renames: vec![("a".into(), "b".into()), ("b".into(), "c".into())],
        });
        let mut ev = event(&json!({"a": 1, "b": 2}));
        apply_stage(&mut stage, &mut ev, &ctx());
        assert_eq!(cell(&ev, "b"), 1);
        assert_eq!(cell(&ev, "c"), 2);
        assert!(!ev.contains_key("a"));
    }

    #[test]
    fn rename_swap_exchanges_values() {
        let mut stage = compile_rename(&RenameStage {
            renames: vec![("a".into(), "b".into()), ("b".into(), "a".into())],
        });
        let mut ev = event(&json!({"a": 1, "b": 2}));
        apply_stage(&mut stage, &mut ev, &ctx());
        assert_eq!(cell(&ev, "a"), 2);
        assert_eq!(cell(&ev, "b"), 1);
    }

    #[test]
    fn rename_collision_on_one_target_takes_the_last_source() {
        let mut stage = compile_rename(&RenameStage {
            renames: vec![("a".into(), "x".into()), ("b".into(), "x".into())],
        });
        let mut ev = event(&json!({"a": 1, "b": 2}));
        apply_stage(&mut stage, &mut ev, &ctx());
        assert_eq!(cell(&ev, "x"), 2);
        assert!(!ev.contains_key("a"));
        assert!(!ev.contains_key("b"));
    }

    #[test]
    fn rename_from_absent_source_scrubs_the_target() {
        // The SQL column exists corpus-wide even when this event lacks
        // it: the target becomes NULL, so it must not keep its own
        // pre-stage value.
        let mut stage = compile_rename(&RenameStage {
            renames: vec![("nonexistent".into(), "host".into())],
        });
        let mut ev = event(&json!({"host": "web-1"}));
        apply_stage(&mut stage, &mut ev, &ctx());
        assert!(!ev.contains_key("host"));
    }

    #[test]
    fn rename_missing_field_is_noop() {
        let mut stage = compile_rename(&RenameStage {
            renames: vec![("nonexistent".into(), "alias".into())],
        });
        let mut ev = event(&json!({"host": "web-1"}));
        apply_stage(&mut stage, &mut ev, &ctx());
        assert_eq!(ev.len(), 1);
        assert!(!ev.contains_key("alias"));
    }

    // ── tier 1: limit ──────────────────────────────────────────────

    #[test]
    fn limit_allows_n_events() {
        let mut stage = compile_limit(&LimitStage {
            count: 3,
            keyword: "limit",
        });
        let mut ev = event(&json!({"i": 1}));

        assert_eq!(apply_stage(&mut stage, &mut ev, &ctx()), StageResult::Pass);
        assert_eq!(apply_stage(&mut stage, &mut ev, &ctx()), StageResult::Pass);
        assert_eq!(apply_stage(&mut stage, &mut ev, &ctx()), StageResult::Pass);
        assert_eq!(apply_stage(&mut stage, &mut ev, &ctx()), StageResult::Done);
    }

    /// Exhaustion is sticky: once a `limit` has said `Done` it says it
    /// forever.
    ///
    /// Regression: a `fetch_sub` counter wraps at zero to `u64::MAX`, so
    /// the event after the first `Done` passes — which the lanes that
    /// keep asking (the aggregate feed, a snapshot's post-stages) turn
    /// into events admitted past the limit.
    #[test]
    fn limit_exhaustion_is_sticky() {
        let mut stage = compile_limit(&LimitStage {
            count: 2,
            keyword: "limit",
        });
        let mut ev = event(&json!({"i": 1}));

        assert_eq!(apply_stage(&mut stage, &mut ev, &ctx()), StageResult::Pass);
        assert_eq!(apply_stage(&mut stage, &mut ev, &ctx()), StageResult::Pass);
        for attempt in 0..5 {
            assert_eq!(
                apply_stage(&mut stage, &mut ev, &ctx()),
                StageResult::Done,
                "attempt {attempt} after exhaustion must still be Done"
            );
        }
    }

    #[test]
    fn limit_zero_is_done_immediately() {
        let mut stage = compile_limit(&LimitStage {
            count: 0,
            keyword: "limit",
        });
        let mut ev = event(&json!({"i": 1}));
        assert_eq!(apply_stage(&mut stage, &mut ev, &ctx()), StageResult::Done);
        assert_eq!(
            apply_stage(&mut stage, &mut ev, &ctx()),
            StageResult::Done,
            "a zero limit never becomes passable"
        );
    }

    // ── tier 2: where ──────────────────────────────────────────────

    #[test]
    fn where_passes_matching() {
        let condition = span(Expr::Binary {
            lhs: Box::new(span(Expr::FieldRef("status".into()))),
            op: BinaryOp::Gt,
            rhs: Box::new(span(Expr::Literal(LiteralValue::Int(400)))),
        });
        let mut stage = compile_where(&WhereStage { condition }, &PinScope::unpinned()).unwrap();
        let mut ev = event(&json!({"status": 500}));
        assert_eq!(apply_stage(&mut stage, &mut ev, &ctx()), StageResult::Pass);
    }

    #[test]
    fn where_filters_non_matching() {
        let condition = span(Expr::Binary {
            lhs: Box::new(span(Expr::FieldRef("status".into()))),
            op: BinaryOp::Gt,
            rhs: Box::new(span(Expr::Literal(LiteralValue::Int(400)))),
        });
        let mut stage = compile_where(&WhereStage { condition }, &PinScope::unpinned()).unwrap();
        let mut ev = event(&json!({"status": 200}));
        assert_eq!(
            apply_stage(&mut stage, &mut ev, &ctx()),
            StageResult::Filtered
        );
    }

    #[test]
    fn where_null_is_filtered() {
        let condition = span(Expr::Binary {
            lhs: Box::new(span(Expr::FieldRef("missing".into()))),
            op: BinaryOp::Gt,
            rhs: Box::new(span(Expr::Literal(LiteralValue::Int(0)))),
        });
        let mut stage = compile_where(&WhereStage { condition }, &PinScope::unpinned()).unwrap();
        let mut ev = event(&json!({"host": "web-1"}));
        assert_eq!(
            apply_stage(&mut stage, &mut ev, &ctx()),
            StageResult::Filtered
        );
    }

    /// `where level == "..."` is an ordinary comparison on the sender's
    /// own `level` column (ADR-0013 §6): zero aliases, so the stream
    /// reads the key it was given.
    fn level_where(op: BinaryOp, token: &str) -> Result<CompiledStage, StreamPlanError> {
        compile_where(
            &WhereStage {
                condition: span(Expr::Binary {
                    lhs: Box::new(span(Expr::FieldRef("level".into()))),
                    op,
                    rhs: Box::new(span(Expr::Literal(LiteralValue::String(token.into())))),
                }),
            },
            &PinScope::unpinned(),
        )
    }

    #[test]
    fn where_level_reads_the_senders_own_column() {
        let mut stage = level_where(BinaryOp::Eq, "gold").unwrap();
        // The game server's `level` field means what it says.
        let mut gold = event(&json!({"service": "game", "level": "gold"}));
        assert_eq!(
            apply_stage(&mut stage, &mut gold, &ctx()),
            StageResult::Pass
        );
        let mut silver = event(&json!({"service": "game", "level": "silver"}));
        assert_eq!(
            apply_stage(&mut stage, &mut silver, &ctx()),
            StageResult::Filtered
        );
        // No `level` key is NULL, not a severity lookup.
        let mut none = event(&json!({"severity": 17}));
        assert_eq!(
            apply_stage(&mut stage, &mut none, &ctx()),
            StageResult::Filtered
        );
    }

    /// A bare name carries no severity vocabulary, so nothing about
    /// `level` is rejected at compile time.
    #[test]
    fn level_is_an_ordinary_field_in_every_position() {
        for dsl in [
            r#"* | where level in ("error", "fatal")"#,
            r#"* | where "error" == level"#,
            "* | where isnull(level)",
            "* | where level matches /err.*/",
            r#"* | where level == "erro""#,
            "* | table level",
            "* | dedup level",
            "* | rename level as lvl",
            "* | let lvl = level",
            "* | where timestamp == \"2026-01-01\"",
            "* | table timestamp, @timestamp",
        ] {
            let pipeline = crate::parser::parse(dsl).expect("parses").pipeline;
            assert!(
                compile_stream_plan(&pipeline, &PinScope::unpinned()).is_ok(),
                "{dsl} must compile as ordinary field usage"
            );
        }
    }

    /// The pipeline may not mint a reserved name — the same predicate
    /// ingest strips by (ADR-0013 §5) — and both doors refuse it: the
    /// SQL lane through `validate_pipeline`, the SSE lane (which never
    /// runs it) through its own plan compilation. Otherwise a capture
    /// group named `_severity` would 400 in batch and stream live,
    /// letting log content overwrite trawl's derived verdict.
    #[test]
    fn rejects_minting_reserved_names() {
        for dsl in [
            "* | let _foo = 1",
            "* | eval _severity = 17",
            "* | rename service as _svc",
            r#"* | extract "(?P<_foo>.)" from message"#,
            r#"* | extract "(?P<_severity>\d+)" from message"#,
            // Quoting changes the lexing, never the policy (ADR-0013
            // ruling 7): a backticked write target is refused exactly as
            // the bare spelling is.
            "* | let `_foo` = 1",
            "* | rename service as `_svc`",
            "* | stats count() as `_total`",
        ] {
            let Ok(query) = crate::parser::parse(dsl) else {
                continue; // refused at the parser door
            };
            assert!(
                crate::emitter::validate_pipeline(&query.pipeline).is_err(),
                "{dsl} must be refused by the SQL lane"
            );
            assert!(
                compile_stream_plan(&query.pipeline, &PinScope::unpinned()).is_err(),
                "{dsl} must be refused by the stream lane"
            );
        }
    }

    /// The SEVERITY pin's closed vocabulary is enforced at compile time
    /// in the stream lane too (ADR-0013): eval has no error channel, so
    /// an unknown token would otherwise open a live-looking stream that
    /// can never match while `/api/v1/query` 400s on the same text.
    #[test]
    fn rejects_unknown_severity_values_under_a_severity_pin() {
        let mut ft = crate::schema::FieldTypes::new();
        ft.insert("_severity", crate::schema::CanonicalType::Severity);
        let scope = PinScope::root(&ft);

        for dsl in [
            r#"* | where _severity == "spicy""#,
            r#"* | where "spicy" == _severity"#,
            r#"* | where _severity >= "gold""#,
            r#"* | where _severity in ("error", "spicy")"#,
            r#"* | let hot = _severity == "spicy""#,
        ] {
            let pipeline = crate::parser::parse(dsl).expect("parses").pipeline;
            let err = compile_stream_plan(&pipeline, &scope).unwrap_err();
            assert!(
                matches!(err, StreamPlanError::InvalidComparison(_)),
                "{dsl}: expected InvalidComparison, got {err:?}"
            );
            assert!(
                err.to_string().contains("unknown severity value"),
                "{dsl}: {err}"
            );
        }

        // The vocabulary itself compiles, and an unpinned `severity` is
        // ordinary sender data with no vocabulary at all.
        for dsl in [
            r#"* | where _severity == "error""#,
            r#"* | where _severity >= "warn""#,
            "* | where _severity == 17",
            r#"* | where _severity == "error2""#,
            r#"* | where severity == "spicy""#,
        ] {
            let pipeline = crate::parser::parse(dsl).expect("parses").pipeline;
            assert!(
                compile_stream_plan(&pipeline, &scope).is_ok(),
                "{dsl} must compile"
            );
        }
    }

    /// `sev()`'s dialect is a closed vocabulary in the stream lane too:
    /// this lane never runs `validate_pipeline`, and eval has no error
    /// channel, so a dialect it cannot honour has to be refused where the
    /// plan is compiled — the same sentence the emitter gives.
    #[test]
    fn rejects_an_unknown_or_computed_sev_dialect() {
        let scope = PinScope::unpinned();
        for dsl in [
            r#"* | where sev(level, "rfc5424") >= 17"#,
            r#"* | let s = sev(level, "bogus")"#,
        ] {
            let pipeline = crate::parser::parse(dsl).expect("parses").pipeline;
            let err = compile_stream_plan(&pipeline, &scope).unwrap_err();
            assert!(
                err.to_string().contains("otel, syslog"),
                "{dsl}: {err} must name the vocabulary"
            );
        }
        let pipeline = crate::parser::parse("* | let s = sev(level, other)")
            .expect("parses")
            .pipeline;
        let err = compile_stream_plan(&pipeline, &scope).unwrap_err();
        assert!(
            err.to_string()
                .contains("must be a string literal dialect name"),
            "{err}"
        );
        // The vocabulary itself compiles, in either case and either arity.
        for dsl in [
            "* | let s = sev(level)",
            r#"* | let s = sev(level, "syslog")"#,
            r#"* | let s = sev(level, "OTEL")"#,
        ] {
            let pipeline = crate::parser::parse(dsl).expect("parses").pipeline;
            assert!(
                compile_stream_plan(&pipeline, &scope).is_ok(),
                "{dsl} must compile"
            );
        }
    }

    /// Arity is the emitter's table in both lanes, word for word: this
    /// lane never runs `validate_pipeline`, so without the mirror
    /// `/stream` accepts `sev()` and `sev(a, b, c)` — opening a
    /// live-looking stream that can only evaluate to NULL — while
    /// `/api/v1/query` 400s on the same text.
    #[test]
    fn rejects_a_wrong_arity_call_with_the_emitters_own_sentence() {
        let scope = PinScope::unpinned();
        for (dsl, sentence) in [
            ("* | let s = sev()", "sev() requires 1 to 2 arguments"),
            (
                r#"* | let s = sev(level, "otel", 1)"#,
                "sev() requires 1 to 2 arguments",
            ),
            (
                "* | where lower(a, b) == \"x\"",
                "lower() requires exactly one argument",
            ),
            ("* | let s = now(a)", "now() requires exactly 0 argument(s)"),
        ] {
            let pipeline = crate::parser::parse(dsl).expect("parses").pipeline;
            let err = compile_stream_plan(&pipeline, &scope)
                .unwrap_err()
                .to_string();
            assert!(err.contains(sentence), "{dsl}: {err}");
            // The batch lane's message for the same text, verbatim.
            let query = crate::parser::parse(dsl).expect("parses");
            let batch = crate::emitter::emit(&query, "/data/*.parquet", ctx())
                .expect_err("batch must refuse too")
                .to_string();
            assert!(batch.contains(sentence), "{dsl}: {batch}");
        }
        // An unknown function is refused here too, with the suggestion.
        let pipeline = crate::parser::parse("* | let s = sevv(level)")
            .expect("parses")
            .pipeline;
        let err = compile_stream_plan(&pipeline, &scope)
            .unwrap_err()
            .to_string();
        assert!(err.contains("sevv"), "{err}");
        assert!(err.contains("sev"), "the suggestion travels: {err}");
    }

    // ── tier 2: let ────────────────────────────────────────────────

    #[test]
    fn let_adds_computed_field() {
        let assignments = vec![(
            "duration_ms".into(),
            span(Expr::Binary {
                lhs: Box::new(span(Expr::FieldRef("duration".into()))),
                op: BinaryOp::Mul,
                rhs: Box::new(span(Expr::Literal(LiteralValue::Int(1000)))),
            }),
        )];
        let mut stage = compile_let(
            &LetStage {
                assignments,
                keyword: "let",
            },
            &PinScope::unpinned(),
        )
        .unwrap();
        let mut ev = event(&json!({"duration": 2}));
        apply_stage(&mut stage, &mut ev, &ctx());
        assert_eq!(cell(&ev, "duration_ms"), 2000);
    }

    #[test]
    fn let_with_function() {
        let assignments = vec![(
            "svc".into(),
            span(Expr::FunctionCall {
                name: "upper".into(),
                args: vec![span(Expr::FieldRef("service".into()))],
            }),
        )];
        let mut stage = compile_let(
            &LetStage {
                assignments,
                keyword: "let",
            },
            &PinScope::unpinned(),
        )
        .unwrap();
        let mut ev = event(&json!({"service": "nginx"}));
        apply_stage(&mut stage, &mut ev, &ctx());
        assert_eq!(cell(&ev, "svc"), "NGINX");
    }

    #[test]
    fn let_sibling_binds_the_alias_when_the_row_has_no_such_column() {
        // `let ms = 1000, total = ms * 2` — the row carries no `ms`, so
        // DuckDB binds the lateral column alias and the batch answers
        // 2000. This lane is also the `rust_stages` batch tail, so a NULL
        // here would be a silent wrong answer on /api/v1/query.
        let assignments = vec![
            ("ms".into(), span(Expr::Literal(LiteralValue::Int(1000)))),
            (
                "total".into(),
                span(Expr::Binary {
                    lhs: Box::new(span(Expr::FieldRef("ms".into()))),
                    op: BinaryOp::Mul,
                    rhs: Box::new(span(Expr::Literal(LiteralValue::Int(2)))),
                }),
            ),
        ];
        let mut stage = compile_let(
            &LetStage {
                assignments,
                keyword: "let",
            },
            &PinScope::unpinned(),
        )
        .unwrap();
        let mut ev = event(&json!({"service": "nginx"}));
        apply_stage(&mut stage, &mut ev, &ctx());
        assert_eq!(cell(&ev, "ms"), 1000);
        assert_eq!(cell(&ev, "total"), 2000);
    }

    #[test]
    fn let_siblings_read_the_pre_stage_event() {
        // SQL: one projection, `(1) AS a, (a) AS b` — the row carries an
        // `a`, so the input column wins over the alias and `b` takes the
        // original `a`.
        let assignments = vec![
            ("a".into(), span(Expr::Literal(LiteralValue::Int(1)))),
            ("b".into(), span(Expr::FieldRef("a".into()))),
        ];
        let mut stage = compile_let(
            &LetStage {
                assignments,
                keyword: "let",
            },
            &PinScope::unpinned(),
        )
        .unwrap();
        let mut ev = event(&json!({"a": 5}));
        apply_stage(&mut stage, &mut ev, &ctx());
        assert_eq!(cell(&ev, "a"), 1);
        assert_eq!(cell(&ev, "b"), 5);
    }

    #[test]
    fn let_target_shadows_a_case_variant_column() {
        // `let A = 1, b = A` — DuckDB binds `A` to the input column `a`,
        // so the target shadows it and `b` reads the original 5. An
        // exact-key shadowing test would have made `A` a fresh alias and
        // handed `b` the 1.
        let assignments = vec![
            ("A".into(), span(Expr::Literal(LiteralValue::Int(1)))),
            ("b".into(), span(Expr::FieldRef("A".into()))),
        ];
        let mut stage = compile_let(
            &LetStage {
                assignments,
                keyword: "let",
            },
            &PinScope::unpinned(),
        )
        .unwrap();
        let mut ev = event(&json!({"a": 5}));
        apply_stage(&mut stage, &mut ev, &ctx());
        assert_eq!(cell(&ev, "A"), 1);
        assert!(!ev.contains_key("a"));
        assert_eq!(cell(&ev, "b"), 5);
    }

    #[test]
    fn let_overwrite_does_not_feed_its_siblings() {
        // `let a = a + 1, b = a` — the overwrite lands on `a`, but `b`
        // still reads the pre-stage `a`.
        let assignments = vec![
            (
                "a".into(),
                span(Expr::Binary {
                    lhs: Box::new(span(Expr::FieldRef("a".into()))),
                    op: BinaryOp::Add,
                    rhs: Box::new(span(Expr::Literal(LiteralValue::Int(1)))),
                }),
            ),
            ("b".into(), span(Expr::FieldRef("a".into()))),
        ];
        let mut stage = compile_let(
            &LetStage {
                assignments,
                keyword: "let",
            },
            &PinScope::unpinned(),
        )
        .unwrap();
        let mut ev = event(&json!({"a": 5}));
        apply_stage(&mut stage, &mut ev, &ctx());
        assert_eq!(cell(&ev, "a"), 6);
        assert_eq!(cell(&ev, "b"), 5);
    }

    // ── tier 2: extract regex ──────────────────────────────────────

    #[test]
    fn extract_regex_captures_named_groups() {
        let mut stage = compile_extract(&ExtractStage {
            mode: ExtractMode::Regex(r"(?P<ip>\d+\.\d+\.\d+\.\d+)".into()),
            source_field: Some("message".into()),
            keyword: "extract",
        })
        .unwrap();
        let mut ev = event(&json!({"message": "connection from 192.168.1.100 accepted"}));
        apply_stage(&mut stage, &mut ev, &ctx());
        assert_eq!(cell(&ev, "ip"), "192.168.1.100");
    }

    #[test]
    fn extract_regex_no_match_writes_null() {
        let mut stage = compile_extract(&ExtractStage {
            mode: ExtractMode::Regex(r"(?P<ip>\d+\.\d+\.\d+\.\d+)".into()),
            source_field: Some("message".into()),
            keyword: "extract",
        })
        .unwrap();
        let mut ev = event(&json!({"message": "no ip here", "ip": "keep?"}));
        apply_stage(&mut stage, &mut ev, &ctx());
        assert_eq!(cell_opt(&ev, "ip"), Some(Value::Null));
        assert!(ev.contains_key("ip"));
    }

    #[test]
    fn extract_regex_empty_capture_is_null() {
        let mut stage = compile_extract(&ExtractStage {
            mode: ExtractMode::Regex(r"(?P<x>a*)".into()),
            source_field: Some("message".into()),
            keyword: "extract",
        })
        .unwrap();
        let mut ev = event(&json!({"message": "bbb"}));
        apply_stage(&mut stage, &mut ev, &ctx());
        assert_eq!(cell_opt(&ev, "x"), Some(Value::Null));

        let mut ev = event(&json!({"message": "aab"}));
        apply_stage(&mut stage, &mut ev, &ctx());
        assert_eq!(cell_opt(&ev, "x"), Some(Value::from("aa")));
    }

    #[test]
    fn extract_kv_write_owns_its_folded_name() {
        let mut stage = compile_extract(&ExtractStage {
            mode: ExtractMode::KeyValue { separator: '=' },
            source_field: Some("message".into()),
            keyword: "extract",
        })
        .unwrap();
        let mut ev = event(&json!({"message": "Status=500", "status": 200}));
        apply_stage(&mut stage, &mut ev, &ctx());
        assert_eq!(cell_opt(&ev, "Status"), Some(Value::from(500)));
        assert!(!ev.contains_key("status"));

        let mut ev = event(&json!({"message": "dur=1 DUR=2 Dur=3"}));
        apply_stage(&mut stage, &mut ev, &ctx());
        assert_eq!(cell_opt(&ev, "Dur"), Some(Value::from(3)));
        assert_eq!(
            ev.keys()
                .filter(|key| key.eq_ignore_ascii_case("dur"))
                .count(),
            1
        );
    }

    /// A stage-computed instant is extractable text in both modes (see
    /// [`extract_source_text`]).
    ///
    /// This lane is the whole answer for such a source: the batch SQL
    /// lane refuses `regexp_extract(TIMESTAMP, …)` outright (pinned in
    /// `stage_parity`), and behind `extract kv` there is no SQL lane at
    /// all — this code is the batch tail.
    #[test]
    fn extract_reads_a_computed_timestamp_as_its_cast_text() {
        let instant = crate::compare::Instant::At(
            chrono::NaiveDate::from_ymd_opt(2026, 1, 15)
                .unwrap()
                .and_hms_opt(9, 0, 0)
                .unwrap(),
        );
        // The cell's own cast text, `2026-01-15 09:00:00`.
        let text = row::cell_text(&EvalValue::Timestamp(instant));

        let mut stage = compile_extract(&ExtractStage {
            mode: ExtractMode::Regex(r"(?P<y>\d{4})".into()),
            source_field: Some("t".into()),
            keyword: "extract",
        })
        .unwrap();
        let mut ev = event(&json!({"service": "nginx"}));
        ev.insert("t".into(), EvalValue::Timestamp(instant));
        apply_stage(&mut stage, &mut ev, &ctx());
        assert_eq!(cell(&ev, "y"), "2026", "text was {text:?}");

        // The kv arm reads the same text through the same door.
        let mut stage = compile_extract(&ExtractStage {
            mode: ExtractMode::KeyValue { separator: ':' },
            source_field: Some("t".into()),
            keyword: "extract",
        })
        .unwrap();
        let mut ev = event(&json!({"service": "nginx"}));
        ev.insert("t".into(), EvalValue::Timestamp(instant));
        apply_stage(&mut stage, &mut ev, &ctx());
        assert_eq!(cell_opt(&ev, "09"), Some(json!("00:00")));
    }

    /// …and not one byte wider: neither arm extracts from a numeric or
    /// boolean cell.
    #[test]
    fn extract_still_skips_a_numeric_or_boolean_source() {
        let mut stage = compile_extract(&ExtractStage {
            mode: ExtractMode::Regex(r"(?P<d>\d+)".into()),
            source_field: Some("n".into()),
            keyword: "extract",
        })
        .unwrap();
        let mut ev = event(&json!({"n": 42}));
        apply_stage(&mut stage, &mut ev, &ctx());
        assert_eq!(cell_opt(&ev, "d"), Some(Value::Null));

        let mut stage = compile_extract(&ExtractStage {
            mode: ExtractMode::KeyValue { separator: '=' },
            source_field: Some("flag".into()),
            keyword: "extract",
        })
        .unwrap();
        let mut ev = event(&json!({"flag": true}));
        apply_stage(&mut stage, &mut ev, &ctx());
        assert_eq!(ev.keys().count(), 1, "nothing extracted: {ev:?}");
    }

    #[test]
    fn extract_regex_default_field() {
        let mut stage = compile_extract(&ExtractStage {
            mode: ExtractMode::Regex(r"(?P<method>[A-Z]+) (?P<path>/[^ ]+)".into()),
            source_field: None,
            keyword: "extract",
        })
        .unwrap();
        let mut ev = event(&json!({"message": "GET /api/v1/users HTTP/1.1"}));
        apply_stage(&mut stage, &mut ev, &ctx());
        assert_eq!(cell(&ev, "method"), "GET");
        assert_eq!(cell(&ev, "path"), "/api/v1/users");
    }

    #[test]
    fn extract_invalid_regex_errors() {
        let err = compile_extract(&ExtractStage {
            mode: ExtractMode::Regex(r"(?P<bad>[".into()),
            source_field: None,
            keyword: "extract",
        })
        .unwrap_err();
        assert!(matches!(err, StreamPlanError::InvalidRegex(_)));
    }

    // ── tier 2: extract kv ─────────────────────────────────────────

    #[test]
    fn extract_kv_basic() {
        let mut stage = compile_extract(&ExtractStage {
            mode: ExtractMode::KeyValue { separator: '=' },
            source_field: Some("message".into()),
            keyword: "extract",
        })
        .unwrap();
        let mut ev = event(&json!({"message": "user=alice status=200 path=/api"}));
        apply_stage(&mut stage, &mut ev, &ctx());
        assert_eq!(cell(&ev, "user"), "alice");
        assert_eq!(cell(&ev, "status"), 200); // coerced to int
        assert_eq!(cell(&ev, "path"), "/api");
    }

    #[test]
    fn extract_kv_quoted_values() {
        let mut stage = compile_extract(&ExtractStage {
            mode: ExtractMode::KeyValue { separator: '=' },
            source_field: Some("message".into()),
            keyword: "extract",
        })
        .unwrap();
        let mut ev = event(&json!({"message": r#"user="alice smith" action=login"#}));
        apply_stage(&mut stage, &mut ev, &ctx());
        assert_eq!(cell(&ev, "user"), "alice smith");
        assert_eq!(cell(&ev, "action"), "login");
    }

    #[test]
    fn extract_kv_custom_separator() {
        let mut stage = compile_extract(&ExtractStage {
            mode: ExtractMode::KeyValue { separator: ':' },
            source_field: Some("message".into()),
            keyword: "extract",
        })
        .unwrap();
        let mut ev = event(&json!({"message": "user:alice status:200"}));
        apply_stage(&mut stage, &mut ev, &ctx());
        assert_eq!(cell(&ev, "user"), "alice");
        assert_eq!(cell(&ev, "status"), 200);
    }

    #[test]
    fn extract_kv_type_coercion() {
        let mut stage = compile_extract(&ExtractStage {
            mode: ExtractMode::KeyValue { separator: '=' },
            source_field: Some("message".into()),
            keyword: "extract",
        })
        .unwrap();
        let mut ev =
            event(&json!({"message": "count=42 rate=1.5 flag=true name=hello empty=false"}));
        apply_stage(&mut stage, &mut ev, &ctx());
        assert_eq!(cell(&ev, "count"), 42);
        assert_eq!(cell(&ev, "rate"), 1.5);
        assert_eq!(cell(&ev, "flag"), true);
        assert_eq!(cell(&ev, "name"), "hello");
        assert_eq!(cell(&ev, "empty"), false);
    }

    // ── tier 2: dedup ──────────────────────────────────────────────

    #[test]
    fn dedup_by_field() {
        let mut stage = compile_dedup(&DedupStage {
            fields: vec!["host".into()],
        });

        let mut ev1 = event(&json!({"host": "web-1", "i": 1}));
        let mut ev2 = event(&json!({"host": "web-1", "i": 2}));
        let mut ev3 = event(&json!({"host": "web-2", "i": 3}));

        assert_eq!(apply_stage(&mut stage, &mut ev1, &ctx()), StageResult::Pass);
        assert_eq!(
            apply_stage(&mut stage, &mut ev2, &ctx()),
            StageResult::Filtered
        );
        assert_eq!(apply_stage(&mut stage, &mut ev3, &ctx()), StageResult::Pass);
    }

    #[test]
    fn dedup_by_multiple_fields() {
        let mut stage = compile_dedup(&DedupStage {
            fields: vec!["host".into(), "service".into()],
        });

        let mut ev1 = event(&json!({"host": "web-1", "service": "nginx"}));
        let mut ev2 = event(&json!({"host": "web-1", "service": "nginx"}));
        let mut ev3 = event(&json!({"host": "web-1", "service": "postgres"}));

        assert_eq!(apply_stage(&mut stage, &mut ev1, &ctx()), StageResult::Pass);
        assert_eq!(
            apply_stage(&mut stage, &mut ev2, &ctx()),
            StageResult::Filtered
        );
        assert_eq!(apply_stage(&mut stage, &mut ev3, &ctx()), StageResult::Pass);
    }

    #[test]
    fn dedup_bare_deduplicates_full_event() {
        let mut stage = compile_dedup(&DedupStage { fields: vec![] });

        let mut ev1 = event(&json!({"host": "web-1", "level": "info"}));
        let mut ev2 = event(&json!({"host": "web-1", "level": "info"}));
        let mut ev3 = event(&json!({"host": "web-1", "level": "error"}));

        assert_eq!(apply_stage(&mut stage, &mut ev1, &ctx()), StageResult::Pass);
        assert_eq!(
            apply_stage(&mut stage, &mut ev2, &ctx()),
            StageResult::Filtered
        );
        assert_eq!(apply_stage(&mut stage, &mut ev3, &ctx()), StageResult::Pass);
    }

    // ── extract_key_value_pairs ────────────────────────────────────

    #[test]
    fn kv_simple_pairs() {
        let pairs = extract_key_value_pairs("user=alice status=200", '=');
        assert_eq!(
            pairs,
            vec![
                ("user".into(), "alice".into()),
                ("status".into(), "200".into()),
            ]
        );
    }

    #[test]
    fn kv_quoted_value() {
        let pairs = extract_key_value_pairs(r#"name="John Doe" age=30"#, '=');
        assert_eq!(
            pairs,
            vec![
                ("name".into(), "John Doe".into()),
                ("age".into(), "30".into()),
            ]
        );
    }

    #[test]
    fn kv_with_prefix_text() {
        let pairs = extract_key_value_pairs("INFO: user=alice action=login", '=');
        assert_eq!(
            pairs,
            vec![
                ("user".into(), "alice".into()),
                ("action".into(), "login".into()),
            ]
        );
    }

    #[test]
    fn kv_empty_input() {
        let pairs = extract_key_value_pairs("", '=');
        assert!(pairs.is_empty());
    }

    #[test]
    fn kv_custom_separator() {
        let pairs = extract_key_value_pairs("user:alice status:200", ':');
        assert_eq!(
            pairs,
            vec![
                ("user".into(), "alice".into()),
                ("status".into(), "200".into()),
            ]
        );
    }

    #[test]
    fn kv_cannot_mint_reserved_names() {
        let mut stage = CompiledStage::ExtractKv {
            source_field: "message".into(),
            separator: '=',
        };
        let mut ev = event(&json!({
            "message": "_severity=17 _time=bogus a=1",
            "_severity": 9,
        }));

        assert_eq!(apply_stage(&mut stage, &mut ev, &ctx()), StageResult::Pass);
        // Log content cannot forge trawl's verdict slots (ADR-0013 §1):
        // the reserved pairs are dropped, the ordinary one lands, and an
        // existing `_severity` keeps trawl's own value.
        assert_eq!(cell_opt(&ev, "a"), Some(json!(1)));
        assert_eq!(cell_opt(&ev, "_severity"), Some(json!(9)));
        assert!(!ev.contains_key("_time"));
    }

    // ── multi-stage pipeline ───────────────────────────────────────

    #[test]
    fn pipeline_table_then_rename() {
        let pipeline = vec![
            span(PipeStage::Table(TableStage {
                fields: vec!["host".into(), "service".into()],
                keyword: "table",
            })),
            span(PipeStage::Rename(RenameStage {
                renames: vec![("service".into(), "svc".into())],
            })),
        ];
        let plan = compile_stream_plan(&pipeline, &PinScope::unpinned()).unwrap();
        let StreamPlan::PassThrough(mut stages) = plan else {
            panic!("expected PassThrough");
        };

        let mut ev = event(&json!({"host": "web-1", "service": "nginx", "message": "hi"}));
        for stage in &mut stages {
            let result = apply_stage(stage, &mut ev, &ctx());
            assert_eq!(result, StageResult::Pass);
        }
        assert_eq!(ev.len(), 2);
        assert!(ev.contains_key("host"));
        assert!(ev.contains_key("svc"));
    }

    #[test]
    fn pipeline_where_then_limit() {
        let pipeline = vec![
            span(PipeStage::Where(WhereStage {
                condition: span(Expr::Binary {
                    lhs: Box::new(span(Expr::FieldRef("status".into()))),
                    op: BinaryOp::Gte,
                    rhs: Box::new(span(Expr::Literal(LiteralValue::Int(400)))),
                }),
            })),
            span(PipeStage::Limit(LimitStage {
                count: 2,
                keyword: "limit",
            })),
        ];
        let plan = compile_stream_plan(&pipeline, &PinScope::unpinned()).unwrap();
        let StreamPlan::PassThrough(mut stages) = plan else {
            panic!("expected PassThrough");
        };

        // event 1: passes where + limit
        let mut ev = event(&json!({"status": 500}));
        let mut result = StageResult::Pass;
        for stage in &mut stages {
            result = apply_stage(stage, &mut ev, &ctx());
            if result != StageResult::Pass {
                break;
            }
        }
        assert_eq!(result, StageResult::Pass);

        // event 2: filtered by where
        let mut ev = event(&json!({"status": 200}));
        result = StageResult::Pass;
        for stage in &mut stages {
            result = apply_stage(stage, &mut ev, &ctx());
            if result != StageResult::Pass {
                break;
            }
        }
        assert_eq!(result, StageResult::Filtered);

        // event 3: passes where + last limit event
        let mut ev = event(&json!({"status": 404}));
        result = StageResult::Pass;
        for stage in &mut stages {
            result = apply_stage(stage, &mut ev, &ctx());
            if result != StageResult::Pass {
                break;
            }
        }
        assert_eq!(result, StageResult::Pass);

        // event 4: limit exhausted
        let mut ev = event(&json!({"status": 503}));
        result = StageResult::Pass;
        for stage in &mut stages {
            result = apply_stage(stage, &mut ev, &ctx());
            if result != StageResult::Pass {
                break;
            }
        }
        assert_eq!(result, StageResult::Done);
    }

    // ── aggregation: compile_stream_plan detection ───────────────

    #[test]
    fn stats_stage_produces_aggregate_plan() {
        let pipeline = vec![span(PipeStage::Stats(StatsStage {
            aggregations: vec![AggExpr {
                function: "count".into(),
                args: vec![],
                alias: None,
            }],
            group_by: vec![],
        }))];
        let plan = compile_stream_plan(&pipeline, &PinScope::unpinned()).unwrap();
        assert!(matches!(plan, StreamPlan::Aggregate { .. }));
    }

    #[test]
    fn stats_with_pre_and_post_stages() {
        use crate::ast::TableStage;
        let pipeline = vec![
            span(PipeStage::Table(TableStage {
                fields: vec!["host".into(), "duration".into()],
                keyword: "table",
            })),
            span(PipeStage::Stats(StatsStage {
                aggregations: vec![AggExpr {
                    function: "avg".into(),
                    args: vec![span(Expr::FieldRef("duration".into()))],
                    alias: None,
                }],
                group_by: vec!["host".into()],
            })),
            span(PipeStage::Limit(LimitStage {
                count: 10,
                keyword: "limit",
            })),
        ];
        let plan = compile_stream_plan(&pipeline, &PinScope::unpinned()).unwrap();
        let StreamPlan::Aggregate {
            pre_stages,
            post_stages,
            ..
        } = plan
        else {
            panic!("expected Aggregate");
        };
        assert_eq!(pre_stages.len(), 1); // Table
        assert_eq!(post_stages.len(), 1); // Limit
    }

    // ── aggregation: stats ──────────────────────────────────────

    #[test]
    fn stats_count_no_group() {
        let pipeline = vec![span(PipeStage::Stats(StatsStage {
            aggregations: vec![AggExpr {
                function: "count".into(),
                args: vec![],
                alias: None,
            }],
            group_by: vec![],
        }))];
        let plan = compile_stream_plan(&pipeline, &PinScope::unpinned()).unwrap();
        let StreamPlan::Aggregate {
            mut aggregation, ..
        } = plan
        else {
            panic!("expected Aggregate");
        };

        aggregation.feed_event(&event(&json!({"host": "a"})), &ctx());
        aggregation.feed_event(&event(&json!({"host": "b"})), &ctx());
        aggregation.feed_event(&event(&json!({"host": "c"})), &ctx());

        let (columns, rows) = aggregation.snapshot();
        assert_eq!(columns, vec!["count"]);
        assert_eq!(rows.len(), 1);
        assert_eq!(cell(&rows[0], "count"), 3);
    }

    #[test]
    fn stats_count_by_field() {
        let pipeline = vec![span(PipeStage::Stats(StatsStage {
            aggregations: vec![AggExpr {
                function: "count".into(),
                args: vec![],
                alias: None,
            }],
            group_by: vec!["host".into()],
        }))];
        let plan = compile_stream_plan(&pipeline, &PinScope::unpinned()).unwrap();
        let StreamPlan::Aggregate {
            mut aggregation, ..
        } = plan
        else {
            panic!("expected Aggregate");
        };

        aggregation.feed_event(&event(&json!({"host": "web-1"})), &ctx());
        aggregation.feed_event(&event(&json!({"host": "web-2"})), &ctx());
        aggregation.feed_event(&event(&json!({"host": "web-1"})), &ctx());

        let (columns, rows) = aggregation.snapshot();
        assert_eq!(columns, vec!["host", "count"]);
        assert_eq!(rows.len(), 2);

        // Find the web-1 row
        let web1 = rows.iter().find(|r| cell(r, "host") == "web-1").unwrap();
        assert_eq!(cell(web1, "count"), 2);
    }

    #[test]
    fn stats_sum_avg_min_max() {
        let pipeline = vec![span(PipeStage::Stats(StatsStage {
            aggregations: vec![
                AggExpr {
                    function: "sum".into(),
                    args: vec![span(Expr::FieldRef("v".into()))],
                    alias: None,
                },
                AggExpr {
                    function: "avg".into(),
                    args: vec![span(Expr::FieldRef("v".into()))],
                    alias: None,
                },
                AggExpr {
                    function: "min".into(),
                    args: vec![span(Expr::FieldRef("v".into()))],
                    alias: None,
                },
                AggExpr {
                    function: "max".into(),
                    args: vec![span(Expr::FieldRef("v".into()))],
                    alias: None,
                },
            ],
            group_by: vec![],
        }))];
        let plan = compile_stream_plan(&pipeline, &PinScope::unpinned()).unwrap();
        let StreamPlan::Aggregate {
            mut aggregation, ..
        } = plan
        else {
            panic!("expected Aggregate");
        };

        for val in [10, 20, 30] {
            aggregation.feed_event(&event(&json!({"v": val})), &ctx());
        }

        let (_, rows) = aggregation.snapshot();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        // Default alias is {function}_{field}
        assert_eq!(cell(row, "sum_v"), 60.0);
        assert_eq!(cell(row, "avg_v"), 20.0);
        assert_eq!(cell(row, "min_v"), 10.0);
        assert_eq!(cell(row, "max_v"), 30.0);
    }

    #[test]
    fn stats_dc_and_values() {
        let pipeline = vec![span(PipeStage::Stats(StatsStage {
            aggregations: vec![
                AggExpr {
                    function: "dc".into(),
                    args: vec![span(Expr::FieldRef("svc".into()))],
                    alias: None,
                },
                AggExpr {
                    function: "values".into(),
                    args: vec![span(Expr::FieldRef("svc".into()))],
                    alias: None,
                },
            ],
            group_by: vec![],
        }))];
        let plan = compile_stream_plan(&pipeline, &PinScope::unpinned()).unwrap();
        let StreamPlan::Aggregate {
            mut aggregation, ..
        } = plan
        else {
            panic!("expected Aggregate");
        };

        aggregation.feed_event(&event(&json!({"svc": "nginx"})), &ctx());
        aggregation.feed_event(&event(&json!({"svc": "postgres"})), &ctx());
        aggregation.feed_event(&event(&json!({"svc": "nginx"})), &ctx()); // duplicate

        let (_, rows) = aggregation.snapshot();
        let row = &rows[0];
        assert_eq!(cell(row, "dc_svc"), 2);
        let values_svc = cell(row, "values_svc");
        let vals = values_svc.as_array().unwrap();
        assert_eq!(vals.len(), 2);
    }

    #[test]
    fn stats_first_last() {
        let pipeline = vec![span(PipeStage::Stats(StatsStage {
            aggregations: vec![
                AggExpr {
                    function: "first".into(),
                    args: vec![span(Expr::FieldRef("msg".into()))],
                    alias: None,
                },
                AggExpr {
                    function: "last".into(),
                    args: vec![span(Expr::FieldRef("msg".into()))],
                    alias: None,
                },
            ],
            group_by: vec![],
        }))];
        let plan = compile_stream_plan(&pipeline, &PinScope::unpinned()).unwrap();
        let StreamPlan::Aggregate {
            mut aggregation, ..
        } = plan
        else {
            panic!("expected Aggregate");
        };

        aggregation.feed_event(&event(&json!({"msg": "alpha"})), &ctx());
        aggregation.feed_event(&event(&json!({"msg": "beta"})), &ctx());
        aggregation.feed_event(&event(&json!({"msg": "gamma"})), &ctx());

        let (_, rows) = aggregation.snapshot();
        let row = &rows[0];
        assert_eq!(cell(row, "first_msg"), "alpha");
        assert_eq!(cell(row, "last_msg"), "gamma");
    }

    #[test]
    fn stats_median() {
        let pipeline = vec![span(PipeStage::Stats(StatsStage {
            aggregations: vec![AggExpr {
                function: "median".into(),
                args: vec![span(Expr::FieldRef("v".into()))],
                alias: None,
            }],
            group_by: vec![],
        }))];
        let plan = compile_stream_plan(&pipeline, &PinScope::unpinned()).unwrap();
        let StreamPlan::Aggregate {
            mut aggregation, ..
        } = plan
        else {
            panic!("expected Aggregate");
        };

        // Odd count: median is middle value
        for val in [1, 3, 5, 7, 9] {
            aggregation.feed_event(&event(&json!({"v": val})), &ctx());
        }
        let (_, rows) = aggregation.snapshot();
        assert_eq!(cell(&rows[0], "median_v"), 5.0);
    }

    #[test]
    fn stats_median_even_count() {
        let pipeline = vec![span(PipeStage::Stats(StatsStage {
            aggregations: vec![AggExpr {
                function: "median".into(),
                args: vec![span(Expr::FieldRef("v".into()))],
                alias: None,
            }],
            group_by: vec![],
        }))];
        let plan = compile_stream_plan(&pipeline, &PinScope::unpinned()).unwrap();
        let StreamPlan::Aggregate {
            mut aggregation, ..
        } = plan
        else {
            panic!("expected Aggregate");
        };

        for val in [1, 3, 5, 7] {
            aggregation.feed_event(&event(&json!({"v": val})), &ctx());
        }
        let (_, rows) = aggregation.snapshot();
        assert_eq!(cell(&rows[0], "median_v"), 4.0);
    }

    #[test]
    fn stats_stddev() {
        let pipeline = vec![span(PipeStage::Stats(StatsStage {
            aggregations: vec![AggExpr {
                function: "stddev".into(),
                args: vec![span(Expr::FieldRef("v".into()))],
                alias: None,
            }],
            group_by: vec![],
        }))];
        let plan = compile_stream_plan(&pipeline, &PinScope::unpinned()).unwrap();
        let StreamPlan::Aggregate {
            mut aggregation, ..
        } = plan
        else {
            panic!("expected Aggregate");
        };

        // sample stddev of [2, 4, 4, 4, 5, 5, 7, 9]:
        // mean=5, Σ(x-μ)²=32, s=√(32/7)≈2.138
        for val in [2, 4, 4, 4, 5, 5, 7, 9] {
            aggregation.feed_event(&event(&json!({"v": val})), &ctx());
        }
        let (_, rows) = aggregation.snapshot();
        let sd = cell(&rows[0], "stddev_v").as_f64().unwrap();
        assert!((sd - 2.138).abs() < 0.01, "stddev was {sd}");
    }

    #[test]
    fn stats_count_field_ignores_nulls() {
        let pipeline = vec![span(PipeStage::Stats(StatsStage {
            aggregations: vec![AggExpr {
                function: "count".into(),
                args: vec![span(Expr::FieldRef("v".into()))],
                alias: None,
            }],
            group_by: vec![],
        }))];
        let plan = compile_stream_plan(&pipeline, &PinScope::unpinned()).unwrap();
        let StreamPlan::Aggregate {
            mut aggregation, ..
        } = plan
        else {
            panic!("expected Aggregate");
        };

        aggregation.feed_event(&event(&json!({"v": 1})), &ctx());
        aggregation.feed_event(&event(&json!({"v": null})), &ctx());
        aggregation.feed_event(&event(&json!({"other": 3})), &ctx()); // v missing
        aggregation.feed_event(&event(&json!({"v": 4})), &ctx());

        let (_, rows) = aggregation.snapshot();
        assert_eq!(cell(&rows[0], "count_v"), 2);
    }

    #[test]
    fn stats_custom_alias() {
        let pipeline = vec![span(PipeStage::Stats(StatsStage {
            aggregations: vec![AggExpr {
                function: "count".into(),
                args: vec![],
                alias: Some("total".into()),
            }],
            group_by: vec![],
        }))];
        let plan = compile_stream_plan(&pipeline, &PinScope::unpinned()).unwrap();
        let StreamPlan::Aggregate {
            mut aggregation, ..
        } = plan
        else {
            panic!("expected Aggregate");
        };

        aggregation.feed_event(&event(&json!({"x": 1})), &ctx());
        let (columns, rows) = aggregation.snapshot();
        assert_eq!(columns, vec!["total"]);
        assert_eq!(cell(&rows[0], "total"), 1);
    }

    // ── aggregation: top/rare ───────────────────────────────────

    #[test]
    fn top_counts_most_frequent() {
        let pipeline = vec![span(PipeStage::Top(TopStage {
            count: 2,
            field: "host".into(),
            by: vec![],
        }))];
        let plan = compile_stream_plan(&pipeline, &PinScope::unpinned()).unwrap();
        let StreamPlan::Aggregate {
            mut aggregation, ..
        } = plan
        else {
            panic!("expected Aggregate");
        };

        for _ in 0..5 {
            aggregation.feed_event(&event(&json!({"host": "web-1"})), &ctx());
        }
        for _ in 0..3 {
            aggregation.feed_event(&event(&json!({"host": "web-2"})), &ctx());
        }
        aggregation.feed_event(&event(&json!({"host": "web-3"})), &ctx());

        let (columns, rows) = aggregation.snapshot();
        assert_eq!(columns, vec!["host", "count"]);
        assert_eq!(rows.len(), 2);
        // First row should be web-1 (most frequent)
        assert_eq!(cell(&rows[0], "host"), "web-1");
        assert_eq!(cell(&rows[0], "count"), 5);
    }

    #[test]
    fn rare_counts_least_frequent() {
        let pipeline = vec![span(PipeStage::Rare(RareStage {
            count: 1,
            field: "host".into(),
            by: vec![],
        }))];
        let plan = compile_stream_plan(&pipeline, &PinScope::unpinned()).unwrap();
        let StreamPlan::Aggregate {
            mut aggregation, ..
        } = plan
        else {
            panic!("expected Aggregate");
        };

        for _ in 0..5 {
            aggregation.feed_event(&event(&json!({"host": "web-1"})), &ctx());
        }
        aggregation.feed_event(&event(&json!({"host": "web-2"})), &ctx());

        let (_, rows) = aggregation.snapshot();
        assert_eq!(rows.len(), 1);
        assert_eq!(cell(&rows[0], "host"), "web-2");
        assert_eq!(cell(&rows[0], "count"), 1);
    }

    #[test]
    fn stats_empty_snapshot_returns_empty() {
        let pipeline = vec![span(PipeStage::Stats(StatsStage {
            aggregations: vec![AggExpr {
                function: "count".into(),
                args: vec![],
                alias: None,
            }],
            group_by: vec!["host".into()],
        }))];
        let plan = compile_stream_plan(&pipeline, &PinScope::unpinned()).unwrap();
        let StreamPlan::Aggregate { aggregation, .. } = plan else {
            panic!("expected Aggregate");
        };

        let (_, rows) = aggregation.snapshot();
        assert!(rows.is_empty());
    }

    // ── unit allowlist validation (streaming path) ─────────────────

    fn make_date_part_stage(unit: &str) -> Spanned<PipeStage> {
        span(PipeStage::Let(LetStage {
            assignments: vec![(
                "h".into(),
                span(Expr::FunctionCall {
                    name: "date_part".into(),
                    args: vec![
                        span(Expr::Literal(LiteralValue::String(unit.to_string()))),
                        span(Expr::FieldRef("timestamp".into())),
                    ],
                }),
            )],
            keyword: "let",
        }))
    }

    fn make_date_trunc_stage(unit: &str) -> Spanned<PipeStage> {
        span(PipeStage::Let(LetStage {
            assignments: vec![(
                "d".into(),
                span(Expr::FunctionCall {
                    name: "date_trunc".into(),
                    args: vec![
                        span(Expr::Literal(LiteralValue::String(unit.to_string()))),
                        span(Expr::FieldRef("timestamp".into())),
                    ],
                }),
            )],
            keyword: "let",
        }))
    }

    #[test]
    fn stream_date_part_accepts_allowlisted_units() {
        for unit in [
            "year", "month", "day", "hour", "minute", "second", "dow", "doy", "epoch",
        ] {
            let pipeline = vec![make_date_part_stage(unit)];
            assert!(
                compile_stream_plan(&pipeline, &PinScope::unpinned()).is_ok(),
                "date_part should accept unit {unit:?} in streaming path"
            );
        }
    }

    #[test]
    fn stream_date_trunc_accepts_allowlisted_units() {
        for unit in [
            "year", "quarter", "month", "week", "day", "hour", "minute", "second",
        ] {
            let pipeline = vec![make_date_trunc_stage(unit)];
            assert!(
                compile_stream_plan(&pipeline, &PinScope::unpinned()).is_ok(),
                "date_trunc should accept unit {unit:?} in streaming path"
            );
        }
    }

    #[test]
    fn stream_date_part_rejects_unknown_unit() {
        let pipeline = vec![make_date_part_stage("nanosecond")];
        let err = compile_stream_plan(&pipeline, &PinScope::unpinned()).unwrap_err();
        assert!(
            matches!(err, StreamPlanError::InvalidUnit(ref msg) if msg.contains("nanosecond")),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn stream_date_trunc_rejects_dow() {
        // dow is in DATE_PART_UNITS but not in DATE_UNITS (date_trunc allowlist)
        let pipeline = vec![make_date_trunc_stage("dow")];
        let err = compile_stream_plan(&pipeline, &PinScope::unpinned()).unwrap_err();
        assert!(
            matches!(err, StreamPlanError::InvalidUnit(_)),
            "unexpected: {err}"
        );
    }

    #[test]
    fn stream_date_diff_rejects_non_literal_unit() {
        // Unit arg is a FieldRef, not a string literal — must be rejected
        let pipeline = vec![span(PipeStage::Let(LetStage {
            assignments: vec![(
                "age".into(),
                span(Expr::FunctionCall {
                    name: "date_diff".into(),
                    args: vec![
                        // non-literal unit: a field reference
                        span(Expr::FieldRef("unit_field".into())),
                        span(Expr::FieldRef("start".into())),
                        span(Expr::FieldRef("end".into())),
                    ],
                }),
            )],
            keyword: "let",
        }))];
        let err = compile_stream_plan(&pipeline, &PinScope::unpinned()).unwrap_err();
        assert!(
            matches!(err, StreamPlanError::InvalidUnit(ref msg) if msg.contains("literal")),
            "unexpected: {err}"
        );
    }

    #[test]
    fn stream_where_date_part_rejects_unknown_unit() {
        // validate_expr should also fire inside where conditions
        let condition = span(Expr::Binary {
            lhs: Box::new(span(Expr::FunctionCall {
                name: "date_part".into(),
                args: vec![
                    span(Expr::Literal(LiteralValue::String("century".to_string()))),
                    span(Expr::FieldRef("timestamp".into())),
                ],
            })),
            op: BinaryOp::Gt,
            rhs: Box::new(span(Expr::Literal(LiteralValue::Int(0)))),
        });
        let pipeline = vec![span(PipeStage::Where(WhereStage { condition }))];
        let err = compile_stream_plan(&pipeline, &PinScope::unpinned()).unwrap_err();
        assert!(matches!(err, StreamPlanError::InvalidUnit(_)));
    }

    // ── format-literal validation (streaming path) ──────────────────

    fn make_strftime_stage(fmt: &str) -> Spanned<PipeStage> {
        span(PipeStage::Let(LetStage {
            assignments: vec![(
                "s".into(),
                span(Expr::FunctionCall {
                    name: "strftime".into(),
                    args: vec![
                        span(Expr::FieldRef("timestamp".into())),
                        span(Expr::Literal(LiteralValue::String(fmt.to_string()))),
                    ],
                }),
            )],
            keyword: "let",
        }))
    }

    fn make_strptime_stage(fmt: &str) -> Spanned<PipeStage> {
        span(PipeStage::Let(LetStage {
            assignments: vec![(
                "t".into(),
                span(Expr::FunctionCall {
                    name: "strptime".into(),
                    args: vec![
                        span(Expr::FieldRef("message".into())),
                        span(Expr::Literal(LiteralValue::String(fmt.to_string()))),
                    ],
                }),
            )],
            keyword: "let",
        }))
    }

    #[test]
    fn stream_strftime_accepts_standard_format() {
        let pipeline = vec![make_strftime_stage("%Y-%m-%d %H:%M:%S")];
        assert!(compile_stream_plan(&pipeline, &PinScope::unpinned()).is_ok());
    }

    #[test]
    fn stream_strftime_rejects_invalid_format() {
        let pipeline = vec![make_strftime_stage("%Q")];
        let err = compile_stream_plan(&pipeline, &PinScope::unpinned()).unwrap_err();
        assert!(
            matches!(err, StreamPlanError::InvalidFormat(ref msg) if msg.contains("%Q")),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn stream_strptime_rejects_invalid_format() {
        let pipeline = vec![make_strptime_stage("%Q")];
        let err = compile_stream_plan(&pipeline, &PinScope::unpinned()).unwrap_err();
        assert!(matches!(err, StreamPlanError::InvalidFormat(_)), "{err}");
    }

    #[test]
    fn stream_where_strftime_rejects_invalid_format() {
        // validate_expr should also fire inside where conditions
        let condition = span(Expr::Binary {
            lhs: Box::new(span(Expr::FunctionCall {
                name: "strftime".into(),
                args: vec![
                    span(Expr::FieldRef("timestamp".into())),
                    span(Expr::Literal(LiteralValue::String("%Q".to_string()))),
                ],
            })),
            op: BinaryOp::Eq,
            rhs: Box::new(span(Expr::Literal(LiteralValue::String("x".to_string())))),
        });
        let pipeline = vec![span(PipeStage::Where(WhereStage { condition }))];
        let err = compile_stream_plan(&pipeline, &PinScope::unpinned()).unwrap_err();
        assert!(matches!(err, StreamPlanError::InvalidFormat(_)), "{err}");
    }
}
