//! Streaming pipeline compiler and executor.
//!
//! Compiles pipe stages from the AST into an in-memory evaluation plan
//! for the SSE streaming path. Two modes:
//!
//! - **pass-through**: events flow through per-event transforms (table,
//!   drop, rename, where, let, extract, dedup, limit, tail).
//! - **aggregation**: events feed into accumulators, periodic snapshots
//!   are emitted (stats, timechart, top, rare).

use std::collections::HashSet;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Map, Value};

use crate::ast::{
    DedupStage, DropStage, ExtractMode, ExtractStage, LetStage, LimitStage, PipeStage, RenameStage,
    Spanned, TableStage, WhereStage,
};
use crate::emitter::map_field_name;
use crate::eval::eval_expr;

// ── stream plan ────────────────────────────────────────────────────

/// A compiled streaming evaluation plan.
pub enum StreamPlan {
    /// Events flow through per-event transforms individually.
    PassThrough(Vec<CompiledStage>),
    // Aggregate variant will be added in phase 6.
}

impl fmt::Debug for StreamPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PassThrough(stages) => f.debug_tuple("PassThrough").field(&stages.len()).finish(),
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
    let mut stages = Vec::new();

    for spanned in pipeline {
        match &spanned.node {
            // tier 1: simple projection/mutation
            PipeStage::Table(s) => stages.push(compile_table(s)),
            PipeStage::Drop(s) => stages.push(compile_drop(s)),
            PipeStage::Rename(s) => stages.push(compile_rename(s)),
            PipeStage::Limit(s) => stages.push(compile_limit(s)),
            PipeStage::Tail(s) => stages.push(CompiledStage::Tail { count: s.count }),

            // tier 2: expression-dependent
            PipeStage::Where(s) => stages.push(compile_where(s)),
            PipeStage::Let(s) => stages.push(compile_let(s)),
            PipeStage::Extract(s) => stages.push(compile_extract(s)?),
            PipeStage::Dedup(s) => stages.push(compile_dedup(s)),

            // aggregation (will be handled in phase 6)
            PipeStage::Stats(_)
            | PipeStage::Timechart(_)
            | PipeStage::Top(_)
            | PipeStage::Rare(_) => {
                return Err(StreamPlanError::UnsupportedStage {
                    stage: stage_name(&spanned.node).to_string(),
                    reason: "aggregation in streaming mode is not yet supported".to_string(),
                });
            }

            // unsupported by design
            PipeStage::Sort(_) => {
                return Err(StreamPlanError::UnsupportedStage {
                    stage: "sort".to_string(),
                    reason: "contradicts real-time arrival order".to_string(),
                });
            }
            PipeStage::Pivot(_) => {
                return Err(StreamPlanError::UnsupportedStage {
                    stage: "pivot".to_string(),
                    reason: "dynamic column structure breaks progressive rendering".to_string(),
                });
            }
        }
    }

    Ok(StreamPlan::PassThrough(stages))
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
    /// Keep only the specified fields.
    Table { fields: HashSet<String> },
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
    ExtractKv { source_field: String },
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
            Self::ExtractKv { source_field } => f
                .debug_struct("ExtractKv")
                .field("source_field", source_field)
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
        ExtractMode::KeyValue => Ok(CompiledStage::ExtractKv { source_field }),
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

        CompiledStage::ExtractKv { source_field } => {
            if let Some(Value::String(text)) = event.get(source_field) {
                let pairs = extract_key_value_pairs(text);
                for (k, v) in pairs {
                    event.insert(k, Value::String(v));
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

/// Extract `key=value` pairs from a string.
///
/// Handles both `key=value` and `key="quoted value"` formats.
fn extract_key_value_pairs(text: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut chars = text.char_indices().peekable();

    while let Some((i, c)) = chars.peek().copied() {
        // skip non-alphanumeric/underscore chars
        if !c.is_alphanumeric() && c != '_' {
            chars.next();
            continue;
        }

        // try to find key=value
        let key_start = i;
        while let Some(&(_, c)) = chars.peek() {
            if c.is_alphanumeric() || c == '_' || c == '.' {
                chars.next();
            } else {
                break;
            }
        }

        let key_end = chars.peek().map_or(text.len(), |&(i, _)| i);

        // check for '='
        if chars.peek().is_some_and(|&(_, c)| c == '=') {
            chars.next(); // consume '='
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{
        BinaryOp, DedupStage, DropStage, Expr, ExtractMode, ExtractStage, LetStage, LimitStage,
        LiteralValue, PipeStage, RenameStage, TableStage, WhereStage,
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
            mode: ExtractMode::KeyValue,
            source_field: Some("message".into()),
        })
        .unwrap();
        let mut ev = event(&json!({"message": "user=alice status=200 path=/api"}));
        apply_stage(&mut stage, &mut ev);
        assert_eq!(ev.get("user").unwrap(), "alice");
        assert_eq!(ev.get("status").unwrap(), "200");
        assert_eq!(ev.get("path").unwrap(), "/api");
    }

    #[test]
    fn extract_kv_quoted_values() {
        let mut stage = compile_extract(&ExtractStage {
            mode: ExtractMode::KeyValue,
            source_field: Some("message".into()),
        })
        .unwrap();
        let mut ev = event(&json!({"message": r#"user="alice smith" action=login"#}));
        apply_stage(&mut stage, &mut ev);
        assert_eq!(ev.get("user").unwrap(), "alice smith");
        assert_eq!(ev.get("action").unwrap(), "login");
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
        let pairs = extract_key_value_pairs("user=alice status=200");
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
        let pairs = extract_key_value_pairs(r#"name="John Doe" age=30"#);
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
        let pairs = extract_key_value_pairs("INFO: user=alice action=login");
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
        let pairs = extract_key_value_pairs("");
        assert!(pairs.is_empty());
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
        let StreamPlan::PassThrough(mut stages) = plan;

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
        let StreamPlan::PassThrough(mut stages) = plan;

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
}
