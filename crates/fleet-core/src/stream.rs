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
    AggExpr, DedupStage, DropStage, ExtractMode, ExtractStage, LetStage, LimitStage, PipeStage,
    RenameStage, Spanned, TableStage, WhereStage,
};
use crate::emitter::map_field_name;
use crate::eval::eval_expr;

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
}

impl fmt::Display for StreamPlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedStage { stage, reason } => {
                write!(f, "{stage} is not supported in streaming mode: {reason}")
            }
            Self::InvalidRegex(msg) => write!(f, "invalid regex: {msg}"),
        }
    }
}

impl std::error::Error for StreamPlanError {}

/// Compile pipe stages into a streaming evaluation plan.
///
/// Rejects unsupported stages (sort, pivot, multiple aggregations)
/// with an error before the stream starts.
pub fn compile_stream_plan(pipeline: &[Spanned<PipeStage>]) -> Result<StreamPlan, StreamPlanError> {
    // Find the first aggregation stage index (if any).
    let agg_idx = pipeline.iter().position(|s| is_agg_stage(&s.node));

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
            pre_stages.push(compile_per_event_stage(spanned)?);
        }

        let aggregation = compile_aggregation(&pipeline[idx].node)?;

        let mut post_stages = Vec::new();
        for spanned in &pipeline[idx + 1..] {
            post_stages.push(compile_per_event_stage(spanned)?);
        }

        Ok(StreamPlan::Aggregate {
            pre_stages,
            aggregation,
            post_stages,
        })
    } else {
        let mut stages = Vec::new();
        for spanned in pipeline {
            stages.push(compile_per_event_stage(spanned)?);
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

fn compile_per_event_stage(spanned: &Spanned<PipeStage>) -> Result<CompiledStage, StreamPlanError> {
    match &spanned.node {
        PipeStage::Table(s) => Ok(compile_table(s)),
        PipeStage::Drop(s) => Ok(compile_drop(s)),
        PipeStage::Rename(s) => Ok(compile_rename(s)),
        PipeStage::Limit(s) => Ok(compile_limit(s)),
        PipeStage::Tail(s) => Ok(CompiledStage::Tail { count: s.count }),
        PipeStage::Where(s) => Ok(compile_where(s)),
        PipeStage::Let(s) => Ok(compile_let(s)),
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
    },
    /// Compute derived fields.
    Let {
        assignments: Vec<(String, Spanned<crate::ast::Expr>)>,
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
            Self::Let { assignments } => f
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
            .map(|f| map_field_name(f).to_string())
            .collect(),
    }
}

fn compile_drop(s: &DropStage) -> CompiledStage {
    CompiledStage::Drop {
        fields: s
            .fields
            .iter()
            .map(|f| map_field_name(f).to_string())
            .collect(),
    }
}

fn compile_rename(s: &RenameStage) -> CompiledStage {
    CompiledStage::Rename {
        renames: s
            .renames
            .iter()
            .map(|(from, to)| (map_field_name(from).to_string(), to.clone()))
            .collect(),
    }
}

fn compile_limit(s: &LimitStage) -> CompiledStage {
    CompiledStage::Limit {
        remaining: AtomicU64::new(s.count),
    }
}

fn compile_where(s: &WhereStage) -> CompiledStage {
    CompiledStage::Where {
        condition: s.condition.clone(),
    }
}

fn compile_let(s: &LetStage) -> CompiledStage {
    CompiledStage::Let {
        assignments: s.assignments.clone(),
    }
}

fn compile_extract(s: &ExtractStage) -> Result<CompiledStage, StreamPlanError> {
    let source_field = s
        .source_field
        .as_deref()
        .map_or("message", |f| map_field_name(f))
        .to_string();

    match &s.mode {
        ExtractMode::Regex(pattern) => {
            let regex = regex::Regex::new(pattern)
                .map_err(|e| StreamPlanError::InvalidRegex(e.to_string()))?;
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
            .map(|f| map_field_name(f).to_string())
            .collect(),
        seen: HashSet::new(),
        max_entries: 10_000,
    }
}

// ── stage application ──────────────────────────────────────────────

/// Apply a compiled stage to an event, mutating it in place.
///
/// Returns whether the event should pass through, be filtered, or
/// the stream is done.
pub fn apply_stage(stage: &mut CompiledStage, event: &mut Map<String, Value>) -> StageResult {
    match stage {
        CompiledStage::Table { fields } => {
            event.retain(|k, _| fields.contains(k));
            StageResult::Pass
        }

        CompiledStage::Drop { fields } => {
            for f in fields.iter() {
                event.remove(f);
            }
            StageResult::Pass
        }

        CompiledStage::Rename { renames } => {
            for (from, to) in renames {
                if let Some(v) = event.remove(from.as_str()) {
                    event.insert(to.clone(), v);
                }
            }
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

        CompiledStage::Where { condition } => {
            let result = eval_expr(condition, event);
            if result.is_truthy() {
                StageResult::Pass
            } else {
                StageResult::Filtered
            }
        }

        CompiledStage::Let { assignments } => {
            for (name, expr) in assignments {
                let result = eval_expr(expr, event);
                event.insert(name.clone(), Value::from(result));
            }
            StageResult::Pass
        }

        CompiledStage::ExtractRegex {
            regex,
            source_field,
        } => {
            if let Some(Value::String(text)) = event.get(source_field) {
                if let Some(caps) = regex.captures(text) {
                    let names: Vec<_> = regex
                        .capture_names()
                        .flatten()
                        .filter_map(|name| {
                            caps.name(name)
                                .map(|m| (name.to_string(), m.as_str().to_string()))
                        })
                        .collect();
                    for (name, value) in names {
                        event.insert(name, Value::String(value));
                    }
                }
            }
            StageResult::Pass
        }

        CompiledStage::ExtractKv {
            source_field,
            separator,
        } => {
            if let Some(Value::String(text)) = event.get(source_field) {
                let pairs = extract_key_value_pairs(text, *separator);
                for (k, v) in pairs {
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
        fields
            .iter()
            .map(|f| {
                let mapped = map_field_name(f);
                event.get(mapped).map_or_else(String::new, |v| match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
            })
            .collect()
    }
}

/// Coerce a string value from kv extraction into the most specific JSON type.
///
/// Tries integer, then float, then boolean, falling back to string.
pub fn coerce_kv_value(s: String) -> Value {
    if let Ok(i) = s.parse::<i64>() {
        return serde_json::Number::from(i).into();
    }
    if let Ok(f) = s.parse::<f64>() {
        if let Some(n) = serde_json::Number::from_f64(f) {
            return Value::Number(n);
        }
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

fn compile_agg_expr(agg: &AggExpr) -> CompiledAcc {
    let field = agg.args.first().and_then(|a| {
        if let crate::ast::Expr::FieldRef(name) = &a.node {
            Some(map_field_name(name).to_string())
        } else {
            None
        }
    });

    let alias = agg.alias.clone().unwrap_or_else(|| match &field {
        Some(f) => format!("{}_{f}", agg.function),
        None => agg.function.clone(),
    });

    let percentile = match agg.function.as_str() {
        "p50" => Some(0.5),
        "p90" => Some(0.9),
        "p95" => Some(0.95),
        "p99" => Some(0.99),
        _ => None,
    };

    CompiledAcc {
        function: agg.function.clone(),
        field,
        alias,
        percentile,
    }
}

fn compile_aggregation(stage: &PipeStage) -> Result<CompiledAggregation, StreamPlanError> {
    match stage {
        PipeStage::Stats(s) => {
            let accumulators: Vec<_> = s.aggregations.iter().map(compile_agg_expr).collect();
            let group_by: Vec<_> = s
                .group_by
                .iter()
                .map(|f| map_field_name(f).to_string())
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
                .map_or(60, crate::ast::FleetDuration::to_seconds);
            let accumulators: Vec<_> = s.aggregations.iter().map(compile_agg_expr).collect();
            let group_by: Vec<_> = s
                .group_by
                .iter()
                .map(|f| map_field_name(f).to_string())
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
            field: map_field_name(&s.field).to_string(),
            by: s.by.iter().map(|f| map_field_name(f).to_string()).collect(),
            counters: HashMap::new(),
        }),
        PipeStage::Rare(s) => Ok(CompiledAggregation::Rare {
            count: s.count,
            field: map_field_name(&s.field).to_string(),
            by: s.by.iter().map(|f| map_field_name(f).to_string()).collect(),
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
                let field_val = event
                    .get(field.as_str())
                    .map_or_else(String::new, |v| match v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    });
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
    group_by
        .iter()
        .map(|f| {
            event.get(f).map_or_else(String::new, |v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
        })
        .collect()
}

#[allow(clippy::cast_precision_loss, clippy::cast_possible_wrap)]
fn event_time_bucket(event: &Map<String, Value>, span_secs: u64) -> i64 {
    // Try to parse timestamp field as RFC3339
    if let Some(Value::String(ts)) = event.get("timestamp") {
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
            return dt.timestamp() / span_secs as i64;
        }
    }
    // Fallback: use current time
    chrono::Utc::now().timestamp() / span_secs as i64
}

fn extract_f64(event: &Map<String, Value>, field: &str) -> Option<f64> {
    event.get(field).and_then(|v| match v {
        Value::Number(n) => n.as_f64(),
        _ => None,
    })
}

fn feed_acc(acc: &CompiledAcc, state: &mut AccState, event: &Map<String, Value>) {
    match state {
        AccState::Count(n) => *n += 1,
        AccState::CountField { non_null } => {
            if let Some(field) = &acc.field {
                if event.get(field).is_some_and(|v| !v.is_null()) {
                    *non_null += 1;
                }
            }
        }
        AccState::Sum(total) => {
            if let Some(field) = &acc.field {
                if let Some(v) = extract_f64(event, field) {
                    *total += v;
                }
            }
        }
        AccState::Avg { sum, count } => {
            if let Some(field) = &acc.field {
                if let Some(v) = extract_f64(event, field) {
                    *sum += v;
                    *count += 1;
                }
            }
        }
        AccState::Min(current) => {
            if let Some(field) = &acc.field {
                if let Some(v) = extract_f64(event, field) {
                    *current = Some(current.map_or(v, |c| c.min(v)));
                }
            }
        }
        AccState::Max(current) => {
            if let Some(field) = &acc.field {
                if let Some(v) = extract_f64(event, field) {
                    *current = Some(current.map_or(v, |c| c.max(v)));
                }
            }
        }
        AccState::Dc(set) | AccState::Values(set) => {
            feed_acc_string_set(acc, set, event);
        }
        AccState::First(stored) => {
            if stored.is_none() {
                if let Some(field) = &acc.field {
                    if let Some(v) = event.get(field) {
                        *stored = Some(v.clone());
                    }
                }
            }
        }
        AccState::Last(stored) => {
            if let Some(field) = &acc.field {
                if let Some(v) = event.get(field) {
                    *stored = Some(v.clone());
                }
            }
        }
        AccState::Median(values) | AccState::Percentile { values, .. } => {
            feed_acc_f64_vec(acc, values, event, MAX_EXACT_VALUES);
        }
        AccState::Stddev(welford) => {
            if let Some(field) = &acc.field {
                if let Some(v) = extract_f64(event, field) {
                    welford.update(v);
                }
            }
        }
    }
}

fn feed_acc_string_set(acc: &CompiledAcc, set: &mut HashSet<String>, event: &Map<String, Value>) {
    if let Some(field) = &acc.field {
        if set.len() < MAX_DISTINCT {
            if let Some(v) = event.get(field) {
                if !v.is_null() {
                    set.insert(match v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    });
                }
            }
        }
    }
}

fn feed_acc_f64_vec(
    acc: &CompiledAcc,
    values: &mut Vec<f64>,
    event: &Map<String, Value>,
    max: usize,
) {
    if let Some(field) = &acc.field {
        if values.len() < max {
            if let Some(v) = extract_f64(event, field) {
                values.push(v);
            }
        }
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
    if sorted.len() % 2 == 0 {
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
        let err = compile_stream_plan(&pipeline).unwrap_err();
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
        let err = compile_stream_plan(&pipeline).unwrap_err();
        assert!(err.to_string().contains("pivot"));
    }

    #[test]
    fn accepts_empty_pipeline() {
        let plan = compile_stream_plan(&[]).unwrap();
        assert!(matches!(plan, StreamPlan::PassThrough(stages) if stages.is_empty()));
    }

    // ── tier 1: table ──────────────────────────────────────────────

    #[test]
    fn table_retains_specified_fields() {
        let mut stage = compile_table(&TableStage {
            fields: vec!["host".into(), "service".into()],
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
        });
        // The compiled stage preserves user-specified field order.
        let CompiledStage::Table { ref fields } = stage else {
            panic!("expected Table stage");
        };
        assert_eq!(
            fields,
            &["timestamp", "event_type", "target", "message"],
            "Table stage must store fields in user-specified order"
        );
    }

    #[test]
    fn table_maps_field_names() {
        let mut stage = compile_table(&TableStage {
            fields: vec!["_time".into(), "host".into()],
        });
        let mut ev = event(&json!({"timestamp": "2026-01-01", "host": "web-1", "message": "hi"}));
        assert_eq!(apply_stage(&mut stage, &mut ev), StageResult::Pass);
        assert!(ev.contains_key("timestamp"));
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
        let mut stage = compile_limit(&LimitStage { count: 3 });
        let mut ev = event(&json!({"i": 1}));

        assert_eq!(apply_stage(&mut stage, &mut ev), StageResult::Pass);
        assert_eq!(apply_stage(&mut stage, &mut ev), StageResult::Pass);
        assert_eq!(apply_stage(&mut stage, &mut ev), StageResult::Pass);
        assert_eq!(apply_stage(&mut stage, &mut ev), StageResult::Done);
    }

    #[test]
    fn limit_zero_is_done_immediately() {
        let mut stage = compile_limit(&LimitStage { count: 0 });
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
        let mut stage = compile_where(&WhereStage { condition });
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
        let mut stage = compile_where(&WhereStage { condition });
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
        let mut stage = compile_where(&WhereStage { condition });
        let mut ev = event(&json!({"host": "web-1"}));
        assert_eq!(apply_stage(&mut stage, &mut ev), StageResult::Filtered);
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
        let mut stage = compile_let(&LetStage { assignments });
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
        let mut stage = compile_let(&LetStage { assignments });
        let mut ev = event(&json!({"service": "nginx"}));
        apply_stage(&mut stage, &mut ev);
        assert_eq!(ev.get("svc").unwrap(), "NGINX");
    }

    // ── tier 2: extract regex ──────────────────────────────────────

    #[test]
    fn extract_regex_captures_named_groups() {
        let mut stage = compile_extract(&ExtractStage {
            mode: ExtractMode::Regex(r"(?P<ip>\d+\.\d+\.\d+\.\d+)".into()),
            source_field: Some("message".into()),
        })
        .unwrap();
        let mut ev = event(&json!({"message": "connection from 192.168.1.100 accepted"}));
        apply_stage(&mut stage, &mut ev);
        assert_eq!(ev.get("ip").unwrap(), "192.168.1.100");
    }

    #[test]
    fn extract_regex_no_match_is_noop() {
        let mut stage = compile_extract(&ExtractStage {
            mode: ExtractMode::Regex(r"(?P<ip>\d+\.\d+\.\d+\.\d+)".into()),
            source_field: Some("message".into()),
        })
        .unwrap();
        let mut ev = event(&json!({"message": "no ip here"}));
        let orig_len = ev.len();
        apply_stage(&mut stage, &mut ev);
        assert_eq!(ev.len(), orig_len);
    }

    #[test]
    fn extract_regex_default_field() {
        let mut stage = compile_extract(&ExtractStage {
            mode: ExtractMode::Regex(r"(?P<method>[A-Z]+) (?P<path>/[^ ]+)".into()),
            source_field: None,
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

    // ── multi-stage pipeline ───────────────────────────────────────

    #[test]
    fn pipeline_table_then_rename() {
        let pipeline = vec![
            span(PipeStage::Table(TableStage {
                fields: vec!["host".into(), "service".into()],
            })),
            span(PipeStage::Rename(RenameStage {
                renames: vec![("service".into(), "svc".into())],
            })),
        ];
        let plan = compile_stream_plan(&pipeline).unwrap();
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
            span(PipeStage::Limit(LimitStage { count: 2 })),
        ];
        let plan = compile_stream_plan(&pipeline).unwrap();
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
        let plan = compile_stream_plan(&pipeline).unwrap();
        assert!(matches!(plan, StreamPlan::Aggregate { .. }));
    }

    #[test]
    fn stats_with_pre_and_post_stages() {
        use crate::ast::TableStage;
        let pipeline = vec![
            span(PipeStage::Table(TableStage {
                fields: vec!["host".into(), "duration".into()],
            })),
            span(PipeStage::Stats(StatsStage {
                aggregations: vec![AggExpr {
                    function: "avg".into(),
                    args: vec![span(Expr::FieldRef("duration".into()))],
                    alias: None,
                }],
                group_by: vec!["host".into()],
            })),
            span(PipeStage::Limit(LimitStage { count: 10 })),
        ];
        let plan = compile_stream_plan(&pipeline).unwrap();
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
        let plan = compile_stream_plan(&pipeline).unwrap();
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
        let plan = compile_stream_plan(&pipeline).unwrap();
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
        let plan = compile_stream_plan(&pipeline).unwrap();
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
        let plan = compile_stream_plan(&pipeline).unwrap();
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
        let plan = compile_stream_plan(&pipeline).unwrap();
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
        let plan = compile_stream_plan(&pipeline).unwrap();
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
        let plan = compile_stream_plan(&pipeline).unwrap();
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
        let plan = compile_stream_plan(&pipeline).unwrap();
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
        let plan = compile_stream_plan(&pipeline).unwrap();
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
        let plan = compile_stream_plan(&pipeline).unwrap();
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
        let plan = compile_stream_plan(&pipeline).unwrap();
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
        let plan = compile_stream_plan(&pipeline).unwrap();
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
        let plan = compile_stream_plan(&pipeline).unwrap();
        let StreamPlan::Aggregate { aggregation, .. } = plan else {
            panic!("expected Aggregate");
        };

        let (_, rows) = aggregation.snapshot();
        assert!(rows.is_empty());
    }
}
