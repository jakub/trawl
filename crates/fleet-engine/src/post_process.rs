//! Post-SQL Rust stage execution.
//!
//! When a query pipeline contains stages that can't be expressed as SQL
//! (e.g. `extract kv` with dynamic columns), the emitter splits the
//! pipeline: stages before the boundary run as `DuckDB` SQL, stages at
//! and after the boundary are collected into `EmittedQuery::rust_stages`.
//!
//! This module applies those Rust stages to the SQL result set, reusing
//! the streaming engine's per-event transforms and aggregation machinery.

use fleet_core::ast::{PipeStage, SortDirection, Spanned};
use fleet_core::stream::{self, CompiledStage, StageResult, StreamPlan};
use indexmap::IndexSet;
use serde_json::{Map, Value};

use crate::error::EngineError;
use crate::value::{Column, QueryResult};

/// Apply Rust pipeline stages to a SQL result set.
///
/// Converts the columnar `QueryResult` into JSON events, compiles a
/// stream plan from the given stages, runs it, and converts back.
pub fn apply_rust_stages(
    result: QueryResult,
    stages: &[Spanned<PipeStage>],
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

    let plan = stream::compile_stream_plan(&plan_stages).map_err(|e| {
        EngineError::Emit(fleet_core::emitter::EmitError::UnsupportedOperation {
            message: format!("post-processing: {e}"),
        })
    })?;

    let processed = match plan {
        StreamPlan::PassThrough(mut compiled_stages) => {
            apply_pass_through(&mut compiled_stages, events)
        }
        StreamPlan::Aggregate {
            mut pre_stages,
            mut aggregation,
            mut post_stages,
        } => apply_aggregate(&mut pre_stages, &mut aggregation, &mut post_stages, events),
    };

    // Apply deferred sort stages.
    let sorted = apply_sorts(processed, &sort_stages);

    Ok(events_to_result(&sorted))
}

/// Convert a columnar `QueryResult` into a vec of JSON object events.
fn rows_to_events(result: &QueryResult) -> Vec<Map<String, Value>> {
    result
        .rows
        .iter()
        .map(|row| {
            let mut event = Map::new();
            for (col, cell) in result.columns.iter().zip(row.iter()) {
                event.insert(col.name.clone(), cell_to_json(cell));
            }
            event
        })
        .collect()
}

/// Convert a `crate::value::Value` cell to `serde_json::Value`.
fn cell_to_json(cell: &crate::value::Value) -> Value {
    match cell {
        crate::value::Value::Null => Value::Null,
        crate::value::Value::Boolean(b) => Value::Bool(*b),
        crate::value::Value::Integer(i) => Value::from(*i),
        crate::value::Value::Float(f) => {
            serde_json::Number::from_f64(*f).map_or(Value::Null, Value::Number)
        }
        crate::value::Value::String(s) => Value::String(s.clone()),
        crate::value::Value::Array(arr) => Value::Array(arr.iter().map(cell_to_json).collect()),
    }
}

/// Convert JSON events back to a columnar `QueryResult`.
///
/// Discovers the column set across all events (preserving insertion order)
/// and fills missing keys with NULL.
fn events_to_result(events: &[Map<String, Value>]) -> QueryResult {
    // Discover columns across all events, preserving insertion order.
    let mut col_set = IndexSet::new();
    for event in events {
        for key in event.keys() {
            col_set.insert(key.clone());
        }
    }

    let columns: Vec<Column> = col_set
        .iter()
        .map(|name| Column { name: name.clone() })
        .collect();

    let rows: Vec<Vec<crate::value::Value>> = events
        .iter()
        .map(|event| {
            col_set
                .iter()
                .map(|col| json_to_cell(event.get(col).unwrap_or(&Value::Null)))
                .collect()
        })
        .collect();

    QueryResult { columns, rows }
}

/// Convert a `serde_json::Value` back to `crate::value::Value`.
fn json_to_cell(v: &Value) -> crate::value::Value {
    match v {
        Value::Null => crate::value::Value::Null,
        Value::Bool(b) => crate::value::Value::Boolean(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                crate::value::Value::Integer(i)
            } else {
                crate::value::Value::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        Value::String(s) => crate::value::Value::String(s.clone()),
        Value::Array(arr) => crate::value::Value::Array(arr.iter().map(json_to_cell).collect()),
        Value::Object(obj) => {
            // Stringify objects (shouldn't normally happen).
            crate::value::Value::String(Value::Object(obj.clone()).to_string())
        }
    }
}

/// Run pass-through stages on each event.
fn apply_pass_through(
    stages: &mut [CompiledStage],
    events: Vec<Map<String, Value>>,
) -> Vec<Map<String, Value>> {
    let mut output = Vec::new();
    'event: for mut event in events {
        for stage in stages.iter_mut() {
            match stream::apply_stage(stage, &mut event) {
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
fn apply_aggregate(
    pre_stages: &mut [CompiledStage],
    aggregation: &mut stream::CompiledAggregation,
    post_stages: &mut [CompiledStage],
    events: Vec<Map<String, Value>>,
) -> Vec<Map<String, Value>> {
    // Feed events through pre-stages into the aggregation.
    'event: for mut event in events {
        for stage in pre_stages.iter_mut() {
            match stream::apply_stage(stage, &mut event) {
                StageResult::Pass => {}
                StageResult::Filtered => continue 'event,
                StageResult::Done => break,
            }
        }
        aggregation.feed_event(&event);
    }

    // Snapshot the aggregation results.
    let (_columns, snapshot_rows) = aggregation.snapshot();

    // Apply post-stages to snapshot rows.
    let mut output = Vec::new();
    'row: for mut row in snapshot_rows {
        for stage in post_stages.iter_mut() {
            match stream::apply_stage(stage, &mut row) {
                StageResult::Pass => {}
                StageResult::Filtered => continue 'row,
                StageResult::Done => return output,
            }
        }
        output.push(row);
    }
    output
}

/// Apply deferred sort stages to the event list.
///
/// Sort is rejected by the streaming compiler (contradicts arrival order)
/// but is valid in batch post-processing.
fn apply_sorts(
    mut events: Vec<Map<String, Value>>,
    sort_stages: &[Spanned<PipeStage>],
) -> Vec<Map<String, Value>> {
    for stage in sort_stages {
        if let PipeStage::Sort(sort) = &stage.node {
            events.sort_by(|a, b| {
                for field in &sort.fields {
                    let key = fleet_core::emitter::map_field_name(&field.field);
                    let va = a.get(key);
                    let vb = b.get(key);
                    let cmp = compare_json_values(va, vb);
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

/// Compare two optional JSON values for sorting purposes.
///
/// NULLs sort last. Numbers compare numerically. Everything else compares
/// as strings.
fn compare_json_values(a: Option<&Value>, b: Option<&Value>) -> std::cmp::Ordering {
    match (a, b) {
        (None | Some(Value::Null), None | Some(Value::Null)) => std::cmp::Ordering::Equal,
        (None | Some(Value::Null), _) => std::cmp::Ordering::Greater, // NULLs last
        (_, None | Some(Value::Null)) => std::cmp::Ordering::Less,
        (Some(a), Some(b)) => {
            // Try numeric comparison first.
            if let (Some(na), Some(nb)) = (as_f64(a), as_f64(b)) {
                return na.partial_cmp(&nb).unwrap_or(std::cmp::Ordering::Equal);
            }
            // Fall back to string comparison.
            let sa = value_to_sort_string(a);
            let sb = value_to_sort_string(b);
            sa.cmp(&sb)
        }
    }
}

fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

fn value_to_sort_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_core::ast::*;

    fn span<T>(node: T) -> Spanned<T> {
        Spanned { node, span: 0..0 }
    }

    fn make_result(columns: &[&str], rows: Vec<Vec<crate::value::Value>>) -> QueryResult {
        QueryResult {
            columns: columns
                .iter()
                .map(|n| Column {
                    name: n.to_string(),
                })
                .collect(),
            rows,
        }
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
        }))];

        let out = apply_rust_stages(result, &stages).unwrap();
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
            })),
            span(PipeStage::Where(WhereStage {
                condition: span(Expr::Binary {
                    lhs: Box::new(span(Expr::FieldRef("status".into()))),
                    op: BinaryOp::Gte,
                    rhs: Box::new(span(Expr::Literal(LiteralValue::Int(400)))),
                }),
            })),
        ];

        let out = apply_rust_stages(result, &stages).unwrap();
        assert_eq!(out.rows.len(), 1);
        let col_idx = |name: &str| out.columns.iter().position(|c| c.name == name).unwrap();
        assert_eq!(
            out.rows[0][col_idx("method")],
            crate::value::Value::String("POST".into())
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

        let out = apply_rust_stages(result, &stages).unwrap();
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
            })),
            span(PipeStage::Sort(SortStage {
                fields: vec![SortField {
                    field: "score".into(),
                    direction: SortDirection::Asc,
                }],
            })),
        ];

        let out = apply_rust_stages(result, &stages).unwrap();
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
        }))];

        let out = apply_rust_stages(result, &stages).unwrap();
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
        }))];

        let out = apply_rust_stages(result, &stages).unwrap();
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
        }))];
        let out = apply_rust_stages(result, &stages).unwrap();
        assert!(out.is_empty());
    }
}
