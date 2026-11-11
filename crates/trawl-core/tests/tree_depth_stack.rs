// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Every recursive walk a query reaches fits a quarter of a worker's
//! stack when its tree is 64 levels deep (#447).
//!
//! The walks over a parsed query stay recursive: the parser bounds how
//! deep a tree may be, and this test is the premise that bound rests on.
//! Each shape is exactly 64 levels deep, counted as `context.md`'s
//! **Tree depth** counts, and runs through every walk the batch and live
//! lanes reach, on one thread with a 512 KiB stack in the debug build: a
//! quarter of the 2 MiB a tokio worker or a blocking preparation job has.
//!
//! A step that refuses the query for what it means, as it does today,
//! has still walked the tree, so it counts as done. The refusals are
//! printed, so a run's log says which step refused which shape.

use std::fmt::Write as _;

use serde_json::json;
use trawl_core::ast::{AggExpr, Expr, PipeStage, Query, SearchToken, Spanned};
use trawl_core::complexity::check_pipeline_complexity;
use trawl_core::context::EvalContext;
use trawl_core::emitter::{self, validate_pipeline};
use trawl_core::field_refs::referenced_fields;
use trawl_core::filter::CompiledFilter;
use trawl_core::pattern::Preparer;
use trawl_core::pin_scope::PinScope;
use trawl_core::schema::FieldTypes;
use trawl_core::stream::{
    StreamPlan, accept_event, accept_event_into_aggregate, compile_stream_plan_with,
};

/// A quarter of a tokio worker's 2 MiB stack.
const QUARTER_STACK: usize = 512 * 1024;

/// The tree depth limit #447 sets.
const LIMIT: usize = 64;

/// Names the step about to run, so a stack overflow, which aborts the
/// process with no unwinding, still says which walk and shape it was.
fn phase(shape: &str, step: &str) {
    eprintln!("shape={shape} step={step}");
}

/// The only refusals a shape may meet: the streaming plan does not take
/// these stages. Any other refusal would skip the walks after it, so it
/// fails the test.
const STREAM_REFUSALS: [&str; 4] = ["stats", "timechart", "eventstats", "pivot"];

/// Records a step that refused the shape for what it means, and fails on
/// a refusal [`STREAM_REFUSALS`] does not name.
fn refused(shape: &str, step: &str, why: &dyn std::fmt::Display) {
    eprintln!("shape={shape} step={step} refused: {why}");
    assert!(
        step == "compile_stream_plan_with" && STREAM_REFUSALS.contains(&shape),
        "{shape}: {step} must not refuse: {why}"
    );
}

/// `n` terms joined by `+`: an operator chain `n - 1` levels deep.
fn chain(n: usize) -> String {
    vec!["1"; n].join(" + ")
}

/// `a == 0 <op> a == 1 …`: `n` comparisons, `n` levels deep.
fn comparisons(n: usize, op: &str) -> String {
    (0..n)
        .map(|i| format!("a == {i}"))
        .collect::<Vec<_>>()
        .join(&format!(" {op} "))
}

/// `body` wrapped in `n` nested calls to `function`.
fn calls(function: &str, n: usize, body: &str) -> String {
    format!(
        "{}{body}{}",
        format!("{function}(").repeat(n),
        ")".repeat(n)
    )
}

/// Every shape #447's first premise checkpoint names, each 64 deep.
fn shapes() -> Vec<(&'static str, String)> {
    vec![
        ("plus chain", format!("* | where {} > 0", chain(LIMIT))),
        (
            "or chain",
            format!("* | where {}", comparisons(LIMIT, "or")),
        ),
        (
            "and chain",
            format!("* | where {}", comparisons(LIMIT, "and")),
        ),
        (
            "minus run",
            // `-` repeats with no space between
            format!("* | where {}a < 2", "-".repeat(LIMIT - 1)),
        ),
        (
            "not run",
            format!("* | where {}a == 1", "not ".repeat(LIMIT - 1)),
        ),
        (
            "chain as in subject",
            format!("* | where {} in (64)", chain(LIMIT)),
        ),
        (
            "chain as in element",
            format!("* | where a in (1, {})", chain(LIMIT)),
        ),
        (
            "chain as call argument",
            format!("* | let x = {}", calls("abs", 16, &chain(LIMIT - 15))),
        ),
        (
            "chain under pass-through calls",
            format!("* | let x = {}", calls("coalesce", 16, &chain(LIMIT - 15))),
        ),
        (
            "pass-through calls in where",
            format!(
                "* | where {} > 0",
                calls("coalesce", 16, &chain(LIMIT - 16))
            ),
        ),
        ("let", format!("* | let x = {}", chain(LIMIT + 1))),
        ("stats", format!("* | stats sum({})", chain(LIMIT + 1))),
        (
            "timechart",
            format!("* | timechart span=1m sum({})", chain(LIMIT + 1)),
        ),
        (
            "eventstats",
            // named, so the stages that refuse an unnamed one walk it
            format!("* | eventstats sum({}) as x", chain(LIMIT + 1)),
        ),
        ("pivot", format!("* | pivot sum({}) on a", chain(LIMIT + 1))),
        (
            "where after extract kv",
            format!("* | extract kv | where {}", comparisons(LIMIT, "or")),
        ),
        ("search NOT run", format!("{}a=1", "NOT ".repeat(LIMIT))),
        (
            "search brackets",
            format!("{}a=1{}", "(".repeat(LIMIT), " )".repeat(LIMIT)),
        ),
        (
            "search NOT and brackets",
            format!("{}a=1{}", "NOT (".repeat(LIMIT / 2), " )".repeat(LIMIT / 2)),
        ),
    ]
}

/// The deepest root in `query`, counted as `context.md`'s **Tree depth**:
/// a field, value, search word, filter or time clause is 0, and each
/// operator, unary, call, `in` list, search `NOT` and bracket group adds
/// one to its deepest child. An aggregation's call is no level; each of
/// its arguments is a root.
fn tree_depth(query: &Query) -> usize {
    let search = query
        .search
        .groups
        .iter()
        .flatten()
        .map(|token| token_depth(&token.node));
    let stages = query
        .pipeline
        .iter()
        .flat_map(|stage| stage_roots(&stage.node));
    search
        .chain(stages.map(|root| expr_depth(&root.node)))
        .max()
        .unwrap_or(0)
}

fn stage_roots(stage: &PipeStage) -> Vec<&Spanned<Expr>> {
    fn arguments(aggregations: &[AggExpr]) -> Vec<&Spanned<Expr>> {
        aggregations.iter().flat_map(|agg| &agg.args).collect()
    }
    match stage {
        PipeStage::Where(stage) => vec![&stage.condition],
        PipeStage::Let(stage) => stage.assignments.iter().map(|(_, value)| value).collect(),
        PipeStage::Stats(stage) => arguments(&stage.aggregations),
        PipeStage::EventStats(stage) => arguments(&stage.aggregations),
        PipeStage::Timechart(stage) => arguments(&stage.aggregations),
        PipeStage::Pivot(stage) => arguments(std::slice::from_ref(&stage.aggregation)),
        _ => Vec::new(),
    }
}

fn expr_depth(expr: &Expr) -> usize {
    match expr {
        Expr::Binary { lhs, rhs, .. } => 1 + expr_depth(&lhs.node).max(expr_depth(&rhs.node)),
        Expr::Unary { operand, .. } => 1 + expr_depth(&operand.node),
        Expr::FunctionCall { args, .. } => {
            1 + args
                .iter()
                .map(|arg| expr_depth(&arg.node))
                .max()
                .unwrap_or(0)
        }
        Expr::InList { expr, list } => {
            1 + list
                .iter()
                .map(|item| expr_depth(&item.node))
                .chain([expr_depth(&expr.node)])
                .max()
                .unwrap_or(0)
        }
        Expr::FieldRef(_) | Expr::Literal(_) => 0,
    }
}

fn token_depth(token: &SearchToken) -> usize {
    match token {
        SearchToken::Not(inner) => 1 + token_depth(&inner.node),
        SearchToken::Group(groups) => {
            1 + groups
                .iter()
                .flatten()
                .map(|token| token_depth(&token.node))
                .max()
                .unwrap_or(0)
        }
        SearchToken::FieldFilter(_)
        | SearchToken::TextSearch(_)
        | SearchToken::TimeFilter(_)
        | SearchToken::QuotedSearch(_)
        | SearchToken::EarliestFilter(_)
        | SearchToken::LatestFilter(_) => 0,
    }
}

/// Runs every walk a query reaches, in the order a run reaches them.
fn walk(shape: &str, dsl: &str) {
    phase(shape, "parse");
    let query = trawl_core::parser::parse(dsl)
        .unwrap_or_else(|errors| panic!("{shape}: {dsl} must parse: {errors:?}"));

    phase(shape, "check_pipeline_complexity");
    if let Err(why) = check_pipeline_complexity(&query.pipeline) {
        refused(shape, "check_pipeline_complexity", &why);
    }

    phase(shape, "validate_pipeline");
    if let Err(why) = validate_pipeline(&query.pipeline) {
        refused(shape, "validate_pipeline", &why);
    }

    phase(shape, "referenced_fields");
    drop(referenced_fields(&query));

    phase(shape, "emit_with_pins");
    let emitted = emitter::emit_with_pins(
        &query,
        "/nonexistent/*.parquet",
        &FieldTypes::new(),
        &emitter::InputColumns::unprobed(),
        EvalContext::capture(),
    );
    match &emitted {
        Ok(emitted) => assert!(!emitted.sql.is_empty(), "{shape}"),
        Err(why) => refused(shape, "emit_with_pins", why),
    }

    phase(shape, "compile_stream_plan_with");
    let plan = compile_stream_plan_with(
        &query.pipeline,
        &PinScope::unpinned(),
        None,
        &mut Preparer::new(),
    );
    if let Err(why) = &plan {
        refused(shape, "compile_stream_plan_with", why);
    }

    phase(shape, "CompiledFilter::compile");
    let filter = CompiledFilter::compile(&query.search, &FieldTypes::new());
    if let Err(why) = &filter {
        refused(shape, "CompiledFilter::compile", why);
    }

    let event = json!({
        "_time": "2026-01-15T09:00:00Z",
        "a": 1,
        "message": "a=1",
    });
    let event = event.as_object().expect("an object");
    let ctx = EvalContext::capture();
    let mut plan = plan.ok();
    if let Ok(filter) = &filter {
        phase(shape, "match");
        if let Err(why) = filter.matches_at(event, &ctx) {
            refused(shape, "match", &why);
        }
        match plan.as_mut() {
            Some(StreamPlan::PassThrough(stages)) => {
                phase(shape, "accept_event");
                if let Err(why) = accept_event(filter, stages, event, &ctx) {
                    refused(shape, "accept_event", &why);
                }
            }
            Some(StreamPlan::Aggregate {
                pre_stages,
                aggregation,
                post_stages: _,
            }) => {
                phase(shape, "accept_event_into_aggregate");
                if let Err(why) =
                    accept_event_into_aggregate(filter, pre_stages, aggregation, event, &ctx)
                {
                    refused(shape, "accept_event_into_aggregate", &why);
                }
                phase(shape, "aggregation snapshot");
                drop(aggregation.snapshot());
            }
            None => {}
        }
    }

    phase(shape, "clone");
    let copy = query.clone();

    phase(shape, "eq");
    assert!(query == copy, "{shape}: a clone equals its query");

    phase(shape, "debug");
    let mut debug = String::new();
    write!(debug, "{query:?}").expect("formats");
    assert!(!debug.is_empty());

    phase(shape, "drop compiled");
    drop(filter);
    drop(plan);
    drop(emitted);

    phase(shape, "drop query");
    drop(query);
    phase(shape, "drop clone");
    drop(copy);
}

#[test]
fn walks_at_the_tree_depth_limit_fit_a_quarter_stack() {
    let shapes = shapes();
    // The depth is counted on this thread, with its own stack, so a
    // shape that is not 64 deep fails here and not as a shallow pass.
    for (shape, dsl) in &shapes {
        let query = trawl_core::parser::parse(dsl)
            .unwrap_or_else(|errors| panic!("{shape}: {dsl} must parse: {errors:?}"));
        assert_eq!(tree_depth(&query), LIMIT, "{shape}: {dsl}");
    }

    std::thread::Builder::new()
        .name("quarter-stack".to_owned())
        .stack_size(QUARTER_STACK)
        .spawn(move || {
            for (shape, dsl) in &shapes {
                walk(shape, dsl);
            }
        })
        .expect("spawn")
        .join()
        .expect("every walk must fit 512 KiB at depth 64");
}
