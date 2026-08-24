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

use indexmap::IndexSet;
use trawl_core::ast::{PipeStage, SortDirection, Spanned};
use trawl_core::eval::{EvalValue, timestamp_to_duckdb_text};
use trawl_core::pin_scope::PinScope;
use trawl_core::row::{self, Row};
use trawl_core::stream::{self, CompiledStage, StageResult, StreamPlan};

use crate::error::EngineError;
use crate::value::{Column, QueryResult};

/// Apply Rust pipeline stages to a SQL result set.
///
/// Converts the columnar `QueryResult` into JSON events, compiles a
/// stream plan from the given stages, runs it, and converts back.
///
/// `pins` is the pin scope stamped at the kv split
/// (`EmittedQuery::rust_stage_pins`, ADR-0011 slice A′), so the tail's
/// `where`/`let` evaluate under the same interpretation the SQL prefix
/// used. Sort stages are partitioned out below without walking the scope
/// — they pass it through unchanged.
pub fn apply_rust_stages(
    result: QueryResult,
    stages: &[Spanned<PipeStage>],
    pins: &PinScope,
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

    let plan = stream::compile_stream_plan(&plan_stages, pins).map_err(|e| {
        EngineError::Emit(trawl_core::emitter::EmitError::UnsupportedOperation {
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

/// A result cell as a pipeline cell — the DIRECT bridge, variant for
/// variant.
///
/// It used to route through `serde_json`, which has no spelling for a
/// non-finite double and turned one into NULL on the way IN: a SQL
/// prefix computing `1.0/0` handed the `extract kv` tail a null, so the
/// tail counted, compared and printed something the same query without
/// the kv split never saw. Both directions are exhaustive with no `_`
/// arm, so a new variant on either side is a compile error rather than a
/// silent NULL.
fn cell_to_eval(cell: &crate::value::Value) -> EvalValue {
    match cell {
        crate::value::Value::Null => EvalValue::Null,
        crate::value::Value::Boolean(b) => EvalValue::Bool(*b),
        crate::value::Value::Integer(i) => EvalValue::Int(*i),
        crate::value::Value::Float(f) => EvalValue::Float(*f),
        crate::value::Value::String(s) => EvalValue::Str(s.clone()),
        crate::value::Value::Array(arr) => EvalValue::Array(arr.iter().map(cell_to_eval).collect()),
    }
}

/// A pipeline cell as a result cell.
///
/// A non-finite double SURVIVES as `Value::Float`; the ONE place it
/// becomes `null` is `trawl_api::value`'s serializer, at the wire, which
/// is also where the batch path has always nulled it. Adding a second
/// nulling site here is exactly the bug this bridge removes.
///
/// A `Timestamp` becomes the STRING `executor::extract_value` renders,
/// deliberately: the display-offset shift runs AFTER the tail and
/// re-parses that text, so a cell retyped here would silently stop
/// shifting (ADR-0011's unshifted-when-tail contract).
fn eval_to_cell(cell: &EvalValue) -> crate::value::Value {
    match cell {
        EvalValue::Null => crate::value::Value::Null,
        EvalValue::Bool(b) => crate::value::Value::Boolean(*b),
        EvalValue::Int(i) => crate::value::Value::Integer(*i),
        EvalValue::Float(f) => crate::value::Value::Float(*f),
        EvalValue::Str(s) => crate::value::Value::String(s.clone()),
        EvalValue::Timestamp(ts) => crate::value::Value::String(timestamp_to_duckdb_text(ts)),
        EvalValue::Array(arr) => crate::value::Value::Array(arr.iter().map(eval_to_cell).collect()),
    }
}

/// Convert JSON events back to a columnar `QueryResult`.
///
/// Discovers the column set across all events (preserving insertion order)
/// and fills missing keys with NULL.
fn events_to_result(events: &[Row]) -> QueryResult {
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
                .map(|col| eval_to_cell(event.get(col).unwrap_or(&EvalValue::Null)))
                .collect()
        })
        .collect();

    QueryResult { columns, rows }
}

/// Run pass-through stages on each event.
fn apply_pass_through(stages: &mut [CompiledStage], events: Vec<Row>) -> Vec<Row> {
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
    events: Vec<Row>,
) -> Vec<Row> {
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
/// NULLs sort last. Two numbers compare numerically, through the one
/// probed DOUBLE order (`compare::double_total_cmp`) rather than
/// `partial_cmp(…).unwrap_or(Equal)`, which left a NaN wherever arrival
/// order happened to put it. Everything else compares as the text the
/// cell shows (`row::cell_text`, the same renderer the live lane groups
/// by).
fn compare_cells(a: Option<&EvalValue>, b: Option<&EvalValue>) -> std::cmp::Ordering {
    match (a, b) {
        (None | Some(EvalValue::Null), None | Some(EvalValue::Null)) => std::cmp::Ordering::Equal,
        (None | Some(EvalValue::Null), _) => std::cmp::Ordering::Greater, // NULLs last
        (_, None | Some(EvalValue::Null)) => std::cmp::Ordering::Less,
        (Some(a), Some(b)) => {
            // Try numeric comparison first.
            if let (Some(na), Some(nb)) = (numeric_cell(a), numeric_cell(b)) {
                return trawl_core::compare::double_total_cmp(na, nb);
            }
            // Fall back to the cell's own text.
            row::cell_text(a).cmp(&row::cell_text(b))
        }
    }
}

/// The numeric reading a sort takes — numbers only, exactly as the JSON
/// form read only `Value::Number`.
#[allow(clippy::cast_precision_loss)]
fn numeric_cell(cell: &EvalValue) -> Option<f64> {
    match cell {
        EvalValue::Int(i) => Some(*i as f64),
        EvalValue::Float(f) => Some(*f),
        _ => None,
    }
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
            keyword: "extract",
        }))];

        let out = apply_rust_stages(result, &stages, &PinScope::unpinned()).unwrap();
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

        let out = apply_rust_stages(result, &stages, &PinScope::unpinned()).unwrap();
        assert_eq!(out.rows.len(), 1);
        let col_idx = |name: &str| out.columns.iter().position(|c| c.name == name).unwrap();
        assert_eq!(
            out.rows[0][col_idx("method")],
            crate::value::Value::String("POST".into())
        );
    }

    /// The kv tail is the ONLY lane behind `extract kv`: a `let` sibling
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

        let out = apply_rust_stages(result, &stages, &PinScope::unpinned()).unwrap();
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

        let out = apply_rust_stages(result, &stages, &PinScope::unpinned()).unwrap();
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

        let out = apply_rust_stages(result, &stages, &PinScope::unpinned()).unwrap();
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

        let out = apply_rust_stages(result, &stages, &PinScope::unpinned()).unwrap();
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

        let out = apply_rust_stages(result, &stages, &PinScope::unpinned()).unwrap();
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
        let out = apply_rust_stages(result, &stages, &PinScope::unpinned()).unwrap();
        assert!(out.is_empty());
    }
}
