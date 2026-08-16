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

use serde_json::{Map, Value};

use crate::ast::{
    AggExpr, DedupStage, DropStage, Expr, ExtractMode, ExtractStage, LetStage, LimitStage,
    LiteralValue, PipeStage, RenameStage, Spanned, TableStage, WhereStage,
};
use crate::emitter::{
    format_literal_position, unit_literal_positions, validate_format_literal, validate_unit_literal,
};
use crate::eval::{bind_event_key, eval_expr_with_pins};
use crate::pin_scope::PinScope;
use crate::schema::catalog_key;

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
    /// A projecting stage that would write one column twice (ADR-0013
    /// ruling 8). Same reason as `ReservedName`: one shared check
    /// (`projection::check_projection_names`), two lanes, and this one
    /// never runs `emitter::validate_pipeline`.
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
            Self::InvalidComparison(msg) => write!(f, "invalid comparison: {msg}"),
            Self::ReservedName(msg) | Self::ProjectionCollision(msg) => {
                write!(f, "unsupported operation: {msg}")
            }
        }
    }
}

impl std::error::Error for StreamPlanError {}

/// Compile pipe stages into a streaming evaluation plan.
///
/// Rejects unsupported stages (sort, pivot, multiple aggregations)
/// with an error before the stream starts.
///
/// `pins` is the pin scope in force at the FIRST stage of `pipeline`
/// (ADR-0011 slice A′): the catalog snapshot's root for a whole-pipeline
/// SSE stream, or the scope stamped at the kv split for a `rust_stages`
/// batch tail. The compiler walks the SAME per-stage scope table the SQL
/// emitter consumes (`crate::pin_scope`), stamping each `where`/`let`
/// with the scope it must evaluate under — no default parameter, so
/// pin-blindness is always explicit at the call site
/// (`&PinScope::unpinned()`).
pub fn compile_stream_plan(
    pipeline: &[Spanned<PipeStage>],
    pins: &PinScope,
) -> Result<StreamPlan, StreamPlanError> {
    // Projection validation runs FIRST — before aggregation discovery and
    // before the unsupported-stage refusals — so a colliding `pivot` or
    // `eventstats` reports the collision this lane shares with the SQL one
    // rather than an "unsupported in streaming mode" that names a
    // different problem (ADR-0013 ruling 8: one check, one sentence).
    for stage in pipeline {
        crate::projection::check_projection_names(&stage.node)
            .map_err(|e| StreamPlanError::ProjectionCollision(e.to_string()))?;
    }

    // Find the first aggregation stage index (if any).
    let agg_idx = pipeline.iter().position(|s| is_agg_stage(&s.node));

    // The scope advances per stage — one walk, shared with the emitter.
    let mut scope = pins.clone();

    if let Some(idx) = agg_idx {
        // Check for a second aggregation stage (not supported).
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
    Limit { remaining: AtomicU64 },
    /// Pass through (ring buffer sizing is handled by the TUI).
    Tail { count: u64 },
    /// Filter events by condition.
    Where {
        condition: Spanned<crate::ast::Expr>,
        /// The pin scope in force at this stage (ADR-0011 slice A′).
        pins: PinScope,
    },
    /// Compute derived fields.
    Let {
        assignments: Vec<(String, Spanned<crate::ast::Expr>)>,
        /// The pin scope in force at this stage (ADR-0011 slice A′).
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
            Self::Limit { remaining } => f
                .debug_struct("Limit")
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

/// Walk an expression tree and validate date/time unit and format literals
/// plus `level` comparison tokens.
///
/// This replicates the checks `emit_expr` performs on the batch path so that an
/// unsupported unit, an invalid `strftime`/`strptime` format literal, or an
/// unknown severity token in `where level == "..."` is rejected at
/// `compile_stream_plan` time rather than silently evaluating to `Null` (or, in
/// batch, erroring) in live tail.
fn validate_expr(
    expr: &Spanned<crate::ast::Expr>,
    scope: &PinScope,
) -> Result<(), StreamPlanError> {
    match &expr.node {
        Expr::FunctionCall { name, args } => {
            let unit_positions = unit_literal_positions(name);
            for (idx, allowlist) in unit_positions {
                let arg = args.get(*idx);
                let raw = arg.and_then(|a| match &a.node {
                    Expr::Literal(LiteralValue::String(s)) => Some(s.as_str()),
                    _ => None,
                });
                // If arg exists check it; if it doesn't exist arity validation will
                // catch it elsewhere.
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
            // Recurse into all args.
            for arg in args {
                validate_expr(arg, scope)?;
            }
        }
        Expr::Binary { lhs, op, rhs } => {
            // The pinned rule table refuses an unknown severity value, and
            // it must refuse it HERE — the SQL emitter 400s on the same
            // shape, and eval (the only other consumer of this scope) has
            // no error channel at all.
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
/// table, for its ERROR alone — the compiled form is re-resolved per
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
    let resolved = match (&lhs.node, &rhs.node) {
        (Expr::FieldRef(name), other) | (other, Expr::FieldRef(name)) => {
            crate::eval::bare_literal_of(other).map(|lit| (name, lit))
        }
        _ => None,
    };
    let Some((name, literal)) = resolved else {
        return Ok(());
    };
    let Some(pin) = scope.pin_for(name) else {
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
    let Expr::FieldRef(name) = &target.node else {
        return Ok(());
    };
    let Some(pin) = scope.pin_for(name) else {
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
            // The `_` namespace is sealed at BOTH pipeline write positions
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
            // Two captures may not name one column: the SQL lane refuses
            // this in `emitter::validate_pipeline`, which this lane never
            // runs — the kv arm's precedent, one predicate, both doors.
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

// ── stage application ──────────────────────────────────────────────

/// The value a live field read sees, bound the way `DuckDB` binds a name.
///
/// Every read in this lane goes through here. `DuckDB` binds identifiers
/// case-insensitively, and since a projection may WRITE a mixed-case key
/// mid-pipeline (`| let A = 1` leaves the row carrying `A`), an exact-key
/// lookup downstream misses a column the batch query happily returns —
/// `| let A = 1 | stats count() by a` counted nothing while the SQL lane
/// answered 1. The doctrine already required this for every pipeline field
/// read (ADR-0011 slice A′); the aggregation machinery predated rows that
/// could carry a non-lowercase key at all.
fn event_value<'e>(event: &'e Map<String, Value>, name: &str) -> Option<&'e Value> {
    let key = bind_event_key(event, name)?;
    event.get(key)
}

/// The value a live field read sees, rendered as the grouping/dedup text.
fn event_text(event: &Map<String, Value>, name: &str) -> String {
    event_value(event, name).map_or_else(String::new, |v| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    })
}

/// Apply a `rename` stage in place, with the SQL's parallel semantics.
///
/// The batch lane emits one projection —
/// `COLUMNS(c -> fold(c) NOT IN (sources, targets)), src AS tgt, …` — so
/// every target reads the PRE-stage row: `rename a as b, b as c` gives `b` the
/// original `a` and `c` the original `b`, never the just-renamed value.
/// A source this event does not carry makes its target absent (the SQL
/// column would be NULL) rather than leaving the target's own stale
/// value behind — the same rule [`PinScope::advance`] applies to pins,
/// so value and pin can never come from different columns.
///
/// A projection write OWNS its folded name, exactly as `let` and
/// `eventstats` do: the emitted exclusion covers the TARGETS as well as
/// the sources, so `rename a as B` over a row that also carries `b`
/// leaves ONE column in either lane. This lane drops the fold-twin
/// ([`remove_folded_twins`]) before writing, which is the in-memory half
/// of that same rule — leave the twin behind and a later `B` reads the
/// stale column while the batch query reads the renamed one.
///
/// Each source binds to the row's OWN spelling ([`bind_event_key`]), for
/// the same reason [`PinScope::advance`] resolves its pin through
/// [`crate::schema::catalog_key`]: `DuckDB` binds the emitted
/// `"Status" AS "st"` to an ingest-folded `status` column
/// case-insensitively, so `rename Status as st` has to carry the value
/// across in this lane too — and the key REMOVED is the one that bound,
/// never the verbatim source.
fn apply_rename(renames: &[(String, String)], event: &mut Map<String, Value>) {
    let sources: Vec<Option<String>> = renames
        .iter()
        .map(|(from, _)| bind_event_key(event, from).map(str::to_owned))
        .collect();
    let resolved: Vec<(&str, Option<Value>)> = renames
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
        // The write OWNS its folded name, exactly as the emitted
        // projection now excludes it: `rename a as B` over a row that
        // also carries `b` must leave ONE key, or a later `B` reads the
        // twin here and the column DuckDB projected there.
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

/// Drop every key that ASCII-folds to `name` but is not spelled like it.
///
/// The in-memory half of "a projection write owns its folded name": the
/// SQL lanes exclude the twin through [`crate::emitter::fields`]'s folded
/// `COLUMNS` lambda, and a row that kept both spellings would answer a
/// later reference with whichever key it met first.
fn remove_folded_twins(event: &mut Map<String, Value>, name: &str) {
    let twins: Vec<String> = event
        .keys()
        .filter(|k| k.as_str() != name && catalog_key(k) == catalog_key(name))
        .cloned()
        .collect();
    for twin in twins {
        event.remove(&twin);
    }
}

/// Apply a `let` stage in place, with the SQL's column-then-alias
/// resolution.
///
/// The batch lane desugars the whole stage into ONE projection
/// (`COLUMNS(c -> c NOT IN (targets)), (expr) AS tgt, …`), and `DuckDB`
/// binds a name inside it the way it binds any name in a `SELECT` list:
/// an INPUT COLUMN wins, and only a name resolving to no input column
/// falls through to the LATERAL COLUMN ALIAS a sibling just defined. Both
/// halves are load-bearing, so this mirrors both:
///
/// - a target that SHADOWS a column the row carries never feeds its
///   siblings — `let a = 1, b = a` and `let a = a + 1, b = a` over a row
///   with an `a` both give `b` the ORIGINAL `a`; "carries" is `DuckDB`'s
///   own case-insensitive binding ([`bind_event_key`]), so `let A = 1,
///   b = A` shadows an `a` too;
/// - a target the row does NOT carry — the ordinary case, since `let`
///   usually names something new — IS the sibling's binding:
///   `let ms = 1000, total = ms * 2` gives `total = 2000`, matching
///   `/api/v1/query` (this lane is also the `rust_stages` batch tail
///   behind `extract kv`, where there is no SQL lane to fall back on).
///
/// Pins do NOT follow the alias: [`PinScope::advance`] resolves every
/// assignment's pin against the PRE-stage scope, so an alias-bound
/// sibling is unpinned — conservative, and identical in both lanes
/// because both consume that one walk.
///
/// The residual is the row-vs-relation gap: a column the CORPUS carries
/// but this row leaves absent (a sparse custom field) is a NULL column
/// read in batch, while the live lane, seeing no key, binds the alias.
fn apply_let(
    assignments: &[(String, Spanned<crate::ast::Expr>)],
    pins: &PinScope,
    event: &mut Map<String, Value>,
) {
    // Decided against the PRE-stage row, before any alias lands: these
    // targets name a real column, so they stay invisible to their
    // siblings and their new values are applied only at the end.
    // "Names a real column" is `DuckDB`'s own binding rule
    // ([`bind_event_key`]), not an exact key match: a target spelled
    // `Dur` shadows the row's `dur` exactly as a reference to it would
    // bind that column.
    let shadowing: Vec<bool> = assignments
        .iter()
        .map(|(name, _)| bind_event_key(event, name).is_some())
        .collect();
    let mut resolved: Vec<(&str, Value)> = Vec::with_capacity(assignments.len());
    for ((name, expr), shadows_column) in assignments.iter().zip(shadowing) {
        let value = Value::from(eval_expr_with_pins(expr, event, pins));
        if !shadows_column {
            // The lateral alias: a later sibling naming this target finds
            // no input column and reads what was just computed.
            event.insert(name.clone(), value.clone());
        }
        resolved.push((name.as_str(), value));
    }
    for (name, value) in resolved {
        // The batch lane's projection excludes every INPUT column whose
        // name ASCII-folds to a target's (`fields::columns_except`), so
        // `let A = 1` over a row carrying `a` leaves ONE column, spelled
        // as the target wrote it. Dropping the folded twin here is that
        // same rule: without it this lane emits both spellings and a
        // downstream reference binds to whichever it finds first.
        remove_folded_twins(event, name);
        event.insert(name.to_string(), value);
    }
}

/// Apply a regex `extract` in place: EVERY capture target is written on
/// EVERY event.
///
/// That is the emitted projection's semantic — it excludes each target and
/// writes `nullif(regexp_extract(…), '')` — so a row the pattern misses,
/// or an optional group that did not participate, gets the column with
/// NULL rather than keeping whatever was there. Executed batch output
/// settles the representation rather than a guess: the key comes back
/// PRESENT carrying null, never removed.
///
/// `nullif` is the whole expression, so an EMPTY match is NULL too: a
/// group that participated and captured nothing (`(?P<x>a*)` against
/// `bbb`) is indistinguishable from one that did not, and a chained
/// extract reading that column would otherwise carry the divergence
/// forward.
fn apply_extract_regex(regex: &regex::Regex, source_field: &str, event: &mut Map<String, Value>) {
    let text = match event_value(event, source_field) {
        Some(Value::String(text)) => Some(text.clone()),
        _ => None,
    };
    let caps = text.as_deref().and_then(|text| regex.captures(text));
    let written: Vec<(String, Value)> = regex
        .capture_names()
        .flatten()
        .map(|name| {
            let value = caps
                .as_ref()
                .and_then(|caps| caps.name(name))
                .filter(|m| !m.as_str().is_empty())
                .map_or(Value::Null, |m| Value::String(m.as_str().to_string()));
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
pub fn apply_stage(stage: &mut CompiledStage, event: &mut Map<String, Value>) -> StageResult {
    match stage {
        CompiledStage::Table { fields } => {
            // `SELECT "a"` binds a column named `A`, so the kept set is
            // decided by the same fold, not by exact spelling.
            let keep: Vec<String> = fields
                .iter()
                .filter_map(|f| bind_event_key(event, f).map(str::to_owned))
                .collect();
            event.retain(|k, _| keep.iter().any(|kept| kept == k));
            StageResult::Pass
        }

        CompiledStage::Drop { fields } => {
            // …and `* EXCLUDE ("a")` excludes an `A` column for the same
            // reason, so the key REMOVED is the one that bound.
            for f in fields.iter() {
                if let Some(key) = bind_event_key(event, f).map(str::to_owned) {
                    event.remove(&key);
                }
            }
            StageResult::Pass
        }

        CompiledStage::Rename { renames } => {
            apply_rename(renames, event);
            StageResult::Pass
        }

        CompiledStage::Limit { remaining } => {
            let prev = remaining.fetch_sub(1, Ordering::Relaxed);
            if prev == 0 {
                StageResult::Done
            } else if prev == 1 {
                // this is the last event — pass it but signal done next time
                StageResult::Pass
            } else {
                StageResult::Pass
            }
        }

        CompiledStage::Tail { .. } => {
            // tail is handled at the buffer level, just pass through
            StageResult::Pass
        }

        CompiledStage::Where { condition, pins } => {
            let result = eval_expr_with_pins(condition, event, pins);
            if result.is_truthy() {
                StageResult::Pass
            } else {
                StageResult::Filtered
            }
        }

        CompiledStage::Let { assignments, pins } => {
            apply_let(assignments, pins, event);
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
            if let Some(Value::String(text)) = event_value(event, source_field) {
                let pairs = extract_key_value_pairs(text, *separator);
                for (k, v) in pairs {
                    // The `_` namespace is sealed against LOG CONTENT too
                    // (ADR-0013 §1): a kv key is sender-controlled text, so
                    // an inserted `_severity`/`_time` would let a message
                    // body forge trawl's own verdict slots in both the SSE
                    // lane and the batch tail. The pair is dropped — the
                    // text stays findable in the source field and `_raw`.
                    if crate::schema::is_reserved_name(&k) {
                        continue;
                    }
                    // A kv write owns its folded name like every other
                    // projection write: a message carrying `Status=500`
                    // over a row that already has `status` must leave ONE
                    // key, or a later read binds whichever it meets first
                    // and the stale original wins. Twin removal also makes
                    // repeated keys within one message last-wins whatever
                    // their spelling, matching the exact-key rule the
                    // insert loop already had.
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
fn dedup_key(fields: &[String], event: &Map<String, Value>) -> Vec<String> {
    if fields.is_empty() {
        // dedup on entire event — use all values sorted by key
        let mut pairs: Vec<_> = event.iter().map(|(k, v)| format!("{k}={v}")).collect();
        pairs.sort();
        pairs
    } else {
        fields.iter().map(|f| event_text(event, f)).collect()
    }
}

/// Coerce a string value from kv extraction into the most specific JSON type.
///
/// Tries integer, then float, then boolean, falling back to string.
pub fn coerce_kv_value(s: String) -> Value {
    if let Ok(i) = s.parse::<i64>() {
        return serde_json::Number::from(i).into();
    }
    if let Ok(f) = s.parse::<f64>()
        && let Some(n) = serde_json::Number::from_f64(f)
    {
        return Value::Number(n);
    }
    match s.as_str() {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        _ => Value::String(s),
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

        // check for separator
        if chars.peek().is_some_and(|&(_, c)| c == separator) {
            chars.next(); // consume separator
            let key = &text[key_start..key_end];

            // parse value
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
    First(Option<Value>),
    Last(Option<Value>),
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

fn compile_agg_expr(agg: &AggExpr) -> Result<CompiledAcc, StreamPlanError> {
    let field = match agg.args.first().map(|a| &a.node) {
        Some(crate::ast::Expr::FieldRef(name)) => Some(name.clone()),
        None => None,
        // `count(<non-null literal>)` IS `count()` — SQL counts one row
        // per input row either way (probed: `COUNT(1)` = `COUNT(*)`), and
        // this lane's row counter answers exactly that. `count(null)` is
        // NOT: SQL counts nothing, and an accumulator that reads no key
        // cannot express "never increment", so it stays refused with
        // every other shape.
        Some(crate::ast::Expr::Literal(lit))
            if agg.function == "count" && !matches!(lit, crate::ast::LiteralValue::Null) =>
        {
            None
        }
        // An accumulator reads ONE event key; it cannot evaluate a wrapped
        // argument, and since the output-name rule became shared this lane
        // would name that column exactly as the batch lane does
        // (`avg_dur`) while never updating it — a stream showing null
        // where the equivalent query shows a value, with nothing in the
        // response to say so. Refusing is the same choice the lane makes
        // for every other shape it cannot evaluate.
        Some(_) => {
            return Err(StreamPlanError::UnsupportedStage {
                stage: format!("{}(...)", agg.function),
                reason: "an aggregation over a computed argument needs the full dataset; \
                         aggregate the field itself, or compute it in a preceding `let`"
                    .to_string(),
            });
        }
    };

    // the OUTPUT name is the shared rule (`projection`), so the column this
    // lane emits is the one the batch lane emits.
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
            let accumulators: Vec<_> = s
                .aggregations
                .iter()
                .map(compile_agg_expr)
                .collect::<Result<_, _>>()?;
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
            let accumulators: Vec<_> = s
                .aggregations
                .iter()
                .map(compile_agg_expr)
                .collect::<Result<_, _>>()?;
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
    pub fn feed_event(&mut self, event: &Map<String, Value>) {
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
                let bucket = event_time_bucket(event, *span_secs);
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
    /// Returns `(column_names, rows)` where each row is a `Map`.
    pub fn snapshot(&self) -> (Vec<String>, Vec<Map<String, Value>>) {
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
) -> (Vec<String>, Vec<Map<String, Value>>) {
    let mut columns: Vec<String> = group_by.to_vec();
    columns.extend(accumulators.iter().map(|a| a.alias.clone()));

    let mut rows = Vec::new();
    for (key, states) in groups {
        let mut row = Map::new();
        for (col, val) in group_by.iter().zip(key.iter()) {
            row.insert(col.clone(), Value::String(val.clone()));
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
) -> (Vec<String>, Vec<Map<String, Value>>) {
    let mut columns = vec!["_time".to_string()];
    columns.extend(group_by.iter().cloned());
    columns.extend(accumulators.iter().map(|a| a.alias.clone()));

    let mut rows = Vec::new();
    let mut sorted_buckets: Vec<_> = buckets.keys().copied().collect();
    sorted_buckets.sort_unstable();

    for bucket in sorted_buckets {
        if let Some(group_map) = buckets.get(&bucket) {
            for (key, states) in group_map {
                let mut row = Map::new();
                let ts = chrono::DateTime::from_timestamp(bucket * span_secs as i64, 0)
                    .map_or_else(|| bucket.to_string(), |dt| dt.to_rfc3339());
                row.insert("_time".to_string(), Value::String(ts));
                for (col, val) in group_by.iter().zip(key.iter()) {
                    row.insert(col.clone(), Value::String(val.clone()));
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
) -> (Vec<String>, Vec<Map<String, Value>>) {
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
            let mut row = Map::new();
            for (col, gv) in by.iter().zip(group_key.iter()) {
                row.insert(col.clone(), Value::String(gv.clone()));
            }
            row.insert(field.to_string(), Value::String(val.clone()));
            row.insert("count".to_string(), Value::from(*cnt));
            rows.push(row);
        }
    }
    (columns, rows)
}

fn make_group_key(group_by: &[String], event: &Map<String, Value>) -> GroupKey {
    group_by.iter().map(|f| event_text(event, f)).collect()
}

#[allow(clippy::cast_precision_loss, clippy::cast_possible_wrap)]
fn event_time_bucket(event: &Map<String, Value>, span_secs: u64) -> i64 {
    // Try to parse the _time field as RFC3339
    if let Some(Value::String(ts)) = event.get("_time")
        && let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts)
    {
        return dt.timestamp() / span_secs as i64;
    }
    // Fallback: use current time
    chrono::Utc::now().timestamp() / span_secs as i64
}

fn extract_f64(event: &Map<String, Value>, field: &str) -> Option<f64> {
    event_value(event, field).and_then(|v| match v {
        Value::Number(n) => n.as_f64(),
        _ => None,
    })
}

fn feed_acc(acc: &CompiledAcc, state: &mut AccState, event: &Map<String, Value>) {
    match state {
        AccState::Count(n) => *n += 1,
        AccState::CountField { non_null } => {
            if let Some(field) = &acc.field
                && event_value(event, field).is_some_and(|v| !v.is_null())
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
        AccState::Min(current) => {
            if let Some(field) = &acc.field
                && let Some(v) = extract_f64(event, field)
            {
                *current = Some(current.map_or(v, |c| c.min(v)));
            }
        }
        AccState::Max(current) => {
            if let Some(field) = &acc.field
                && let Some(v) = extract_f64(event, field)
            {
                *current = Some(current.map_or(v, |c| c.max(v)));
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

fn feed_acc_string_set(acc: &CompiledAcc, set: &mut HashSet<String>, event: &Map<String, Value>) {
    if let Some(field) = &acc.field
        && set.len() < MAX_DISTINCT
        && let Some(v) = event_value(event, field)
        && !v.is_null()
    {
        set.insert(match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        });
    }
}

fn feed_acc_f64_vec(
    acc: &CompiledAcc,
    values: &mut Vec<f64>,
    event: &Map<String, Value>,
    max: usize,
) {
    if let Some(field) = &acc.field
        && values.len() < max
        && let Some(v) = extract_f64(event, field)
    {
        values.push(v);
    }
}

#[allow(clippy::cast_precision_loss)]
fn snapshot_acc(state: &AccState) -> Value {
    match state {
        AccState::Count(n) => Value::from(*n),
        AccState::CountField { non_null } => Value::from(*non_null),
        AccState::Sum(total) => json_f64(*total),
        AccState::Avg { sum, count } => {
            if *count == 0 {
                Value::Null
            } else {
                json_f64(*sum / *count as f64)
            }
        }
        AccState::Min(v) | AccState::Max(v) => v.map_or(Value::Null, json_f64),
        AccState::Dc(set) => Value::from(set.len() as u64),
        AccState::First(v) | AccState::Last(v) => v.clone().unwrap_or(Value::Null),
        AccState::Values(set) => {
            let mut vals: Vec<_> = set.iter().cloned().collect();
            vals.sort();
            Value::Array(vals.into_iter().map(Value::String).collect())
        }
        AccState::Median(values) => snapshot_median(values),
        AccState::Stddev(welford) => welford.stddev().map_or(Value::Null, json_f64),
        AccState::Percentile { values, target } => snapshot_percentile(values, *target),
    }
}

fn snapshot_median(values: &[f64]) -> Value {
    if values.is_empty() {
        return Value::Null;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        json_f64(f64::midpoint(sorted[mid - 1], sorted[mid]))
    } else {
        json_f64(sorted[mid])
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn snapshot_percentile(values: &[f64], target: f64) -> Value {
    if values.is_empty() {
        return Value::Null;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = (target * (sorted.len() - 1) as f64).round() as usize;
    json_f64(sorted[idx.min(sorted.len() - 1)])
}

fn json_f64(v: f64) -> Value {
    serde_json::Number::from_f64(v).map_or(Value::Null, Value::Number)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{
        AggExpr, BinaryOp, DedupStage, DropStage, Expr, ExtractMode, ExtractStage, LetStage,
        LimitStage, LiteralValue, PipeStage, RareStage, RenameStage, StatsStage, TableStage,
        TopStage, WhereStage,
    };
    use serde_json::json;

    fn span<T>(node: T) -> Spanned<T> {
        Spanned { node, span: 0..0 }
    }

    fn event(pairs: &Value) -> Map<String, Value> {
        pairs.as_object().unwrap().clone()
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
        assert_eq!(apply_stage(&mut stage, &mut ev), StageResult::Pass);
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
        // The compiled stage preserves user-specified field order.
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
        assert_eq!(apply_stage(&mut stage, &mut ev), StageResult::Pass);
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
        assert_eq!(apply_stage(&mut stage, &mut ev), StageResult::Pass);
        assert_eq!(ev.len(), 1);
        assert!(ev.contains_key("host"));
    }

    #[test]
    fn drop_ignores_missing_fields() {
        let mut stage = compile_drop(&DropStage {
            fields: vec!["nonexistent".into()],
        });
        let mut ev = event(&json!({"host": "web-1"}));
        assert_eq!(apply_stage(&mut stage, &mut ev), StageResult::Pass);
        assert_eq!(ev.len(), 1);
    }

    // ── tier 1: rename ─────────────────────────────────────────────

    #[test]
    fn rename_renames_field() {
        let mut stage = compile_rename(&RenameStage {
            renames: vec![("service".into(), "svc".into())],
        });
        let mut ev = event(&json!({"service": "nginx", "host": "web-1"}));
        assert_eq!(apply_stage(&mut stage, &mut ev), StageResult::Pass);
        assert_eq!(ev.get("svc").unwrap(), "nginx");
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
        apply_stage(&mut stage, &mut ev);
        assert!(ev.contains_key("svc"));
        assert!(ev.contains_key("hostname"));
        assert!(!ev.contains_key("service"));
        assert!(!ev.contains_key("host"));
    }

    #[test]
    fn rename_chain_reads_the_pre_stage_event() {
        // SQL: `COLUMNS(c -> fold(c) NOT IN ('a','b','c')), a AS b, b AS c`
        // — `b` takes the original `a`, `c` the original `b`, never the
        // just-renamed one.
        let mut stage = compile_rename(&RenameStage {
            renames: vec![("a".into(), "b".into()), ("b".into(), "c".into())],
        });
        let mut ev = event(&json!({"a": 1, "b": 2}));
        apply_stage(&mut stage, &mut ev);
        assert_eq!(ev.get("b").unwrap(), 1);
        assert_eq!(ev.get("c").unwrap(), 2);
        assert!(!ev.contains_key("a"));
    }

    #[test]
    fn rename_swap_exchanges_values() {
        let mut stage = compile_rename(&RenameStage {
            renames: vec![("a".into(), "b".into()), ("b".into(), "a".into())],
        });
        let mut ev = event(&json!({"a": 1, "b": 2}));
        apply_stage(&mut stage, &mut ev);
        assert_eq!(ev.get("a").unwrap(), 2);
        assert_eq!(ev.get("b").unwrap(), 1);
    }

    #[test]
    fn rename_collision_on_one_target_takes_the_last_source() {
        let mut stage = compile_rename(&RenameStage {
            renames: vec![("a".into(), "x".into()), ("b".into(), "x".into())],
        });
        let mut ev = event(&json!({"a": 1, "b": 2}));
        apply_stage(&mut stage, &mut ev);
        assert_eq!(ev.get("x").unwrap(), 2);
        assert!(!ev.contains_key("a"));
        assert!(!ev.contains_key("b"));
    }

    #[test]
    fn rename_from_absent_source_scrubs_the_target() {
        // The SQL column exists corpus-wide even when THIS event lacks
        // it: the target becomes NULL, so it must not keep its own
        // pre-stage value.
        let mut stage = compile_rename(&RenameStage {
            renames: vec![("nonexistent".into(), "host".into())],
        });
        let mut ev = event(&json!({"host": "web-1"}));
        apply_stage(&mut stage, &mut ev);
        assert!(!ev.contains_key("host"));
    }

    #[test]
    fn rename_missing_field_is_noop() {
        let mut stage = compile_rename(&RenameStage {
            renames: vec![("nonexistent".into(), "alias".into())],
        });
        let mut ev = event(&json!({"host": "web-1"}));
        apply_stage(&mut stage, &mut ev);
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

        assert_eq!(apply_stage(&mut stage, &mut ev), StageResult::Pass);
        assert_eq!(apply_stage(&mut stage, &mut ev), StageResult::Pass);
        assert_eq!(apply_stage(&mut stage, &mut ev), StageResult::Pass);
        assert_eq!(apply_stage(&mut stage, &mut ev), StageResult::Done);
    }

    #[test]
    fn limit_zero_is_done_immediately() {
        let mut stage = compile_limit(&LimitStage {
            count: 0,
            keyword: "limit",
        });
        let mut ev = event(&json!({"i": 1}));
        assert_eq!(apply_stage(&mut stage, &mut ev), StageResult::Done);
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
        assert_eq!(apply_stage(&mut stage, &mut ev), StageResult::Pass);
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
        assert_eq!(apply_stage(&mut stage, &mut ev), StageResult::Filtered);
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
        assert_eq!(apply_stage(&mut stage, &mut ev), StageResult::Filtered);
    }

    /// `where level == "..."` is an ORDINARY comparison on the sender's
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
        assert_eq!(apply_stage(&mut stage, &mut gold), StageResult::Pass);
        let mut silver = event(&json!({"service": "game", "level": "silver"}));
        assert_eq!(apply_stage(&mut stage, &mut silver), StageResult::Filtered);
        // No `level` key is NULL, not a severity lookup.
        let mut none = event(&json!({"severity": 17}));
        assert_eq!(apply_stage(&mut stage, &mut none), StageResult::Filtered);
    }

    /// There is no severity vocabulary on a bare name any more, so
    /// nothing about `level` is rejected at compile time.
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

    /// The pipeline may not MINT a reserved name — the same predicate
    /// ingest strips by (ADR-0013 §5) — and BOTH doors refuse it: the
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

    /// The SEVERITY pin's closed vocabulary is enforced at COMPILE time
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

        // The vocabulary itself compiles, and an UNPINNED `severity` is
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
        apply_stage(&mut stage, &mut ev);
        assert_eq!(ev.get("duration_ms").unwrap(), 2000);
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
        apply_stage(&mut stage, &mut ev);
        assert_eq!(ev.get("svc").unwrap(), "NGINX");
    }

    #[test]
    fn let_sibling_binds_the_alias_when_the_row_has_no_such_column() {
        // `let ms = 1000, total = ms * 2` — the row carries no `ms`, so
        // DuckDB binds the LATERAL COLUMN ALIAS and the batch answers
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
        apply_stage(&mut stage, &mut ev);
        assert_eq!(ev.get("ms").unwrap(), 1000);
        assert_eq!(ev.get("total").unwrap(), 2000);
    }

    #[test]
    fn let_siblings_read_the_pre_stage_event() {
        // SQL: one projection, `(1) AS a, (a) AS b` — the row carries an
        // `a`, so the input COLUMN wins over the alias and `b` takes the
        // ORIGINAL `a`.
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
        apply_stage(&mut stage, &mut ev);
        assert_eq!(ev.get("a").unwrap(), 1);
        assert_eq!(ev.get("b").unwrap(), 5);
    }

    #[test]
    fn let_target_shadows_a_case_variant_column() {
        // `let A = 1, b = A` — DuckDB binds `A` to the input column `a`,
        // so the target shadows it and `b` reads the ORIGINAL 5. An
        // exact-key shadowing test would have made `A` a fresh alias and
        // handed `b` the 1.
        //
        // The shadowed column also LEAVES: the batch projection excludes
        // every input column folding to a target, so one column comes
        // back, spelled as the target wrote it.
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
        apply_stage(&mut stage, &mut ev);
        assert_eq!(ev.get("A").unwrap(), 1);
        assert!(
            !ev.contains_key("a"),
            "the folded twin must not survive beside the target: {ev:?}"
        );
        assert_eq!(ev.get("b").unwrap(), 5);
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
        apply_stage(&mut stage, &mut ev);
        assert_eq!(ev.get("a").unwrap(), 6);
        assert_eq!(ev.get("b").unwrap(), 5);
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
        apply_stage(&mut stage, &mut ev);
        assert_eq!(ev.get("ip").unwrap(), "192.168.1.100");
    }

    #[test]
    fn extract_kv_write_owns_its_folded_name() {
        // A kv key differing only in case from a column the row already
        // carries must REPLACE it, not sit beside it — a later read binds
        // one of the two and the stale original could win.
        let mut stage = compile_extract(&ExtractStage {
            mode: ExtractMode::KeyValue { separator: '=' },
            source_field: Some("message".into()),
            keyword: "extract",
        })
        .unwrap();
        let mut ev = event(&json!({"message": "Status=500", "status": 200}));
        apply_stage(&mut stage, &mut ev);
        assert_eq!(ev.get("Status"), Some(&Value::from(500)));
        assert!(!ev.contains_key("status"), "one key, not two: {ev:?}");

        // Repeated keys folding to one name inside a SINGLE message are
        // last-wins, matching the exact-key rule the insert loop always
        // had.
        let mut ev = event(&json!({"message": "dur=1 DUR=2 Dur=3"}));
        apply_stage(&mut stage, &mut ev);
        assert_eq!(ev.get("Dur"), Some(&Value::from(3)));
        assert_eq!(
            ev.keys().filter(|k| k.eq_ignore_ascii_case("dur")).count(),
            1,
            "{ev:?}"
        );
    }

    #[test]
    fn extract_regex_empty_participating_capture_is_null() {
        // The emitted expression is `nullif(regexp_extract(…), '')`, so a
        // group that PARTICIPATED and captured nothing is NULL in batch —
        // indistinguishable from one that did not participate.
        let mut stage = compile_extract(&ExtractStage {
            mode: ExtractMode::Regex(r"(?P<x>a*)".into()),
            source_field: Some("message".into()),
            keyword: "extract",
        })
        .unwrap();
        let mut ev = event(&json!({"message": "bbb"}));
        apply_stage(&mut stage, &mut ev);
        assert_eq!(ev.get("x"), Some(&Value::Null));

        // …and a genuine capture still lands.
        let mut ev = event(&json!({"message": "aab"}));
        apply_stage(&mut stage, &mut ev);
        assert_eq!(ev.get("x"), Some(&Value::from("aa")));
    }

    #[test]
    fn extract_regex_no_match_writes_null() {
        // The emitted projection excludes the target and writes
        // `nullif(regexp_extract(…), '')`, so a row the pattern misses
        // gets the column carrying NULL — executed batch output returns
        // the key PRESENT with null. This lane used to leave the row
        // untouched, which kept a pre-existing value the batch query
        // would have overwritten.
        let mut stage = compile_extract(&ExtractStage {
            mode: ExtractMode::Regex(r"(?P<ip>\d+\.\d+\.\d+\.\d+)".into()),
            source_field: Some("message".into()),
            keyword: "extract",
        })
        .unwrap();
        let mut ev = event(&json!({"message": "no ip here", "ip": "keep?"}));
        apply_stage(&mut stage, &mut ev);
        assert_eq!(ev.get("ip"), Some(&Value::Null));
        assert!(ev.contains_key("ip"), "the key is present, not removed");
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
        apply_stage(&mut stage, &mut ev);
        assert_eq!(ev.get("method").unwrap(), "GET");
        assert_eq!(ev.get("path").unwrap(), "/api/v1/users");
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
        apply_stage(&mut stage, &mut ev);
        assert_eq!(ev.get("user").unwrap(), "alice");
        assert_eq!(ev.get("status").unwrap(), 200); // coerced to int
        assert_eq!(ev.get("path").unwrap(), "/api");
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
        apply_stage(&mut stage, &mut ev);
        assert_eq!(ev.get("user").unwrap(), "alice smith");
        assert_eq!(ev.get("action").unwrap(), "login");
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
        apply_stage(&mut stage, &mut ev);
        assert_eq!(ev.get("user").unwrap(), "alice");
        assert_eq!(ev.get("status").unwrap(), 200);
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
        apply_stage(&mut stage, &mut ev);
        assert_eq!(ev.get("count").unwrap(), 42);
        assert_eq!(ev.get("rate").unwrap(), 1.5);
        assert_eq!(ev.get("flag").unwrap(), true);
        assert_eq!(ev.get("name").unwrap(), "hello");
        assert_eq!(ev.get("empty").unwrap(), false);
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

        assert_eq!(apply_stage(&mut stage, &mut ev1), StageResult::Pass);
        assert_eq!(apply_stage(&mut stage, &mut ev2), StageResult::Filtered);
        assert_eq!(apply_stage(&mut stage, &mut ev3), StageResult::Pass);
    }

    #[test]
    fn dedup_by_multiple_fields() {
        let mut stage = compile_dedup(&DedupStage {
            fields: vec!["host".into(), "service".into()],
        });

        let mut ev1 = event(&json!({"host": "web-1", "service": "nginx"}));
        let mut ev2 = event(&json!({"host": "web-1", "service": "nginx"}));
        let mut ev3 = event(&json!({"host": "web-1", "service": "postgres"}));

        assert_eq!(apply_stage(&mut stage, &mut ev1), StageResult::Pass);
        assert_eq!(apply_stage(&mut stage, &mut ev2), StageResult::Filtered);
        assert_eq!(apply_stage(&mut stage, &mut ev3), StageResult::Pass);
    }

    #[test]
    fn dedup_bare_deduplicates_full_event() {
        let mut stage = compile_dedup(&DedupStage { fields: vec![] });

        let mut ev1 = event(&json!({"host": "web-1", "level": "info"}));
        let mut ev2 = event(&json!({"host": "web-1", "level": "info"}));
        let mut ev3 = event(&json!({"host": "web-1", "level": "error"}));

        assert_eq!(apply_stage(&mut stage, &mut ev1), StageResult::Pass);
        assert_eq!(apply_stage(&mut stage, &mut ev2), StageResult::Filtered);
        assert_eq!(apply_stage(&mut stage, &mut ev3), StageResult::Pass);
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

        assert_eq!(apply_stage(&mut stage, &mut ev), StageResult::Pass);
        // Log content cannot forge trawl's verdict slots (ADR-0013 §1):
        // the reserved pairs are dropped, the ordinary one lands, and an
        // existing `_severity` keeps trawl's own value.
        assert_eq!(ev.get("a"), Some(&json!(1)));
        assert_eq!(ev.get("_severity"), Some(&json!(9)));
        assert!(ev.get("_time").is_none());
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
            let result = apply_stage(stage, &mut ev);
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
            result = apply_stage(stage, &mut ev);
            if result != StageResult::Pass {
                break;
            }
        }
        assert_eq!(result, StageResult::Pass);

        // event 2: filtered by where
        let mut ev = event(&json!({"status": 200}));
        result = StageResult::Pass;
        for stage in &mut stages {
            result = apply_stage(stage, &mut ev);
            if result != StageResult::Pass {
                break;
            }
        }
        assert_eq!(result, StageResult::Filtered);

        // event 3: passes where + last limit event
        let mut ev = event(&json!({"status": 404}));
        result = StageResult::Pass;
        for stage in &mut stages {
            result = apply_stage(stage, &mut ev);
            if result != StageResult::Pass {
                break;
            }
        }
        assert_eq!(result, StageResult::Pass);

        // event 4: limit exhausted
        let mut ev = event(&json!({"status": 503}));
        result = StageResult::Pass;
        for stage in &mut stages {
            result = apply_stage(stage, &mut ev);
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

        aggregation.feed_event(&event(&json!({"host": "a"})));
        aggregation.feed_event(&event(&json!({"host": "b"})));
        aggregation.feed_event(&event(&json!({"host": "c"})));

        let (columns, rows) = aggregation.snapshot();
        assert_eq!(columns, vec!["count"]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get("count").unwrap(), 3);
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

        aggregation.feed_event(&event(&json!({"host": "web-1"})));
        aggregation.feed_event(&event(&json!({"host": "web-2"})));
        aggregation.feed_event(&event(&json!({"host": "web-1"})));

        let (columns, rows) = aggregation.snapshot();
        assert_eq!(columns, vec!["host", "count"]);
        assert_eq!(rows.len(), 2);

        // Find the web-1 row
        let web1 = rows
            .iter()
            .find(|r| r.get("host").unwrap() == "web-1")
            .unwrap();
        assert_eq!(web1.get("count").unwrap(), 2);
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
            aggregation.feed_event(&event(&json!({"v": val})));
        }

        let (_, rows) = aggregation.snapshot();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        // Default alias is {function}_{field}
        assert_eq!(row.get("sum_v").unwrap(), 60.0);
        assert_eq!(row.get("avg_v").unwrap(), 20.0);
        assert_eq!(row.get("min_v").unwrap(), 10.0);
        assert_eq!(row.get("max_v").unwrap(), 30.0);
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

        aggregation.feed_event(&event(&json!({"svc": "nginx"})));
        aggregation.feed_event(&event(&json!({"svc": "postgres"})));
        aggregation.feed_event(&event(&json!({"svc": "nginx"}))); // duplicate

        let (_, rows) = aggregation.snapshot();
        let row = &rows[0];
        assert_eq!(row.get("dc_svc").unwrap(), 2);
        let vals = row.get("values_svc").unwrap().as_array().unwrap();
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

        aggregation.feed_event(&event(&json!({"msg": "alpha"})));
        aggregation.feed_event(&event(&json!({"msg": "beta"})));
        aggregation.feed_event(&event(&json!({"msg": "gamma"})));

        let (_, rows) = aggregation.snapshot();
        let row = &rows[0];
        assert_eq!(row.get("first_msg").unwrap(), "alpha");
        assert_eq!(row.get("last_msg").unwrap(), "gamma");
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
            aggregation.feed_event(&event(&json!({"v": val})));
        }
        let (_, rows) = aggregation.snapshot();
        assert_eq!(rows[0].get("median_v").unwrap(), 5.0);
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
            aggregation.feed_event(&event(&json!({"v": val})));
        }
        let (_, rows) = aggregation.snapshot();
        assert_eq!(rows[0].get("median_v").unwrap(), 4.0);
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
            aggregation.feed_event(&event(&json!({"v": val})));
        }
        let (_, rows) = aggregation.snapshot();
        let sd = rows[0].get("stddev_v").unwrap().as_f64().unwrap();
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

        aggregation.feed_event(&event(&json!({"v": 1})));
        aggregation.feed_event(&event(&json!({"v": null})));
        aggregation.feed_event(&event(&json!({"other": 3}))); // v missing
        aggregation.feed_event(&event(&json!({"v": 4})));

        let (_, rows) = aggregation.snapshot();
        assert_eq!(rows[0].get("count_v").unwrap(), 2);
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

        aggregation.feed_event(&event(&json!({"x": 1})));
        let (columns, rows) = aggregation.snapshot();
        assert_eq!(columns, vec!["total"]);
        assert_eq!(rows[0].get("total").unwrap(), 1);
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
            aggregation.feed_event(&event(&json!({"host": "web-1"})));
        }
        for _ in 0..3 {
            aggregation.feed_event(&event(&json!({"host": "web-2"})));
        }
        aggregation.feed_event(&event(&json!({"host": "web-3"})));

        let (columns, rows) = aggregation.snapshot();
        assert_eq!(columns, vec!["host", "count"]);
        assert_eq!(rows.len(), 2);
        // First row should be web-1 (most frequent)
        assert_eq!(rows[0].get("host").unwrap(), "web-1");
        assert_eq!(rows[0].get("count").unwrap(), 5);
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
            aggregation.feed_event(&event(&json!({"host": "web-1"})));
        }
        aggregation.feed_event(&event(&json!({"host": "web-2"})));

        let (_, rows) = aggregation.snapshot();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get("host").unwrap(), "web-2");
        assert_eq!(rows[0].get("count").unwrap(), 1);
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

    // ── M6: unit allowlist validation (streaming path) ─────────────

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
        // dow is in DATE_PART_UNITS but NOT in DATE_UNITS (date_trunc allowlist)
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
