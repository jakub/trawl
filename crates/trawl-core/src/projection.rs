// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The names a projecting stage puts on its output row.
//!
//! This module exists because the aggregate output-name rule had FOUR
//! implementations and three of them disagreed with the one that actually
//! writes the column: the SQL emitter recursed into a nested argument
//! (`avg(lower(dur))` → `avg_dur`) while `eventstats`, the SSE lane and the
//! pin-scope walk each took the first argument only and fell back to the
//! bare function name (`avg`). Same syntax, same shape of stage, different
//! answer per lane — a live/batch divergence in `stats` today, and the
//! reason `eventstats` names a nested aggregation differently from `stats`.
//!
//! It sits beside [`crate::pin_scope`] rather than inside the emitter for
//! the same reason that one does: the rule is not about SQL, and both the
//! SQL lane and the stream lane must reach it without one importing the
//! other. A collision check computed from a rule the emitter does not use
//! would report collisions the emitter never creates and miss the ones it
//! does — so the rule has to be shared before it can be checked.

use crate::ast::{AggExpr, Expr, Spanned};

/// The first FIELD REFERENCE reachable from an aggregation's argument.
///
/// Recurses through wrapping expressions (function calls, binary ops, unary
/// ops) so `avg(tonumber(rssi) * -1)` names itself `avg_rssi` rather than
/// just `avg`. The walk is deliberately leftmost-first: it is a NAMING
/// heuristic, not an evaluation, and it has to be total over expressions no
/// lane can evaluate.
#[must_use]
pub fn agg_arg_field_name(arg: Option<&Spanned<Expr>>) -> Option<String> {
    let expr = &arg?.node;
    match expr {
        Expr::FieldRef(name) => Some(name.clone()),
        Expr::FunctionCall { args, .. } => agg_arg_field_name(args.first()),
        Expr::Binary { lhs, .. } => agg_arg_field_name(Some(lhs)),
        Expr::Unary { operand, .. } => agg_arg_field_name(Some(operand)),
        _ => None,
    }
}

/// The column an aggregation projects: its explicit `as` target, else
/// `func_arg` — or bare `func` when the argument names no field.
///
/// Raw, never SQL-quoted: the SQL lane quotes at emission, the stream lane
/// stores it as a JSON key, and a collision check compares it folded.
#[must_use]
pub fn agg_output_name(agg: &AggExpr) -> String {
    if let Some(alias) = &agg.alias {
        return alias.clone();
    }
    match agg_arg_field_name(agg.args.first()) {
        Some(arg) => format!("{}_{arg}", agg.function),
        None => agg.function.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::PipeStage;
    use crate::parser;

    /// The aggregations of the first pipe stage of `dsl`.
    fn aggs(dsl: &str) -> Vec<AggExpr> {
        let query = parser::parse(dsl).expect("parse should succeed");
        match &query.pipeline[0].node {
            PipeStage::Stats(s) => s.aggregations.clone(),
            PipeStage::EventStats(s) => s.aggregations.clone(),
            PipeStage::Timechart(s) => s.aggregations.clone(),
            PipeStage::Pivot(s) => vec![s.aggregation.clone()],
            other => panic!("not a projecting stage: {other:?}"),
        }
    }

    fn name_of(dsl: &str) -> String {
        agg_output_name(&aggs(dsl)[0])
    }

    #[test]
    fn no_argument_names_the_function() {
        assert_eq!(name_of("* | stats count()"), "count");
    }

    #[test]
    fn a_field_argument_names_function_and_field() {
        assert_eq!(name_of("* | stats avg(duration)"), "avg_duration");
        assert_eq!(name_of("* | stats dc(host)"), "dc_host");
    }

    /// The recursive half — the whole reason this is one rule and not four.
    #[test]
    fn a_wrapped_argument_still_finds_its_field() {
        assert_eq!(name_of("* | stats avg(lower(dur))"), "avg_dur");
        assert_eq!(name_of("* | stats sum(a + b)"), "sum_a");
        assert_eq!(name_of("* | stats avg(-x)"), "avg_x");
        assert_eq!(name_of("* | stats avg(tonumber(rssi) * -1)"), "avg_rssi");
        // …and an argument naming no field at all falls back to the function
        assert_eq!(name_of("* | stats avg(1 + 2)"), "avg");
    }

    #[test]
    fn an_explicit_alias_wins_over_everything() {
        assert_eq!(name_of("* | stats count() as total"), "total");
        assert_eq!(name_of("* | stats avg(lower(dur)) as d"), "d");
        // including the spellings only backticks can express
        assert_eq!(name_of("* | stats count() as `n rows`"), "n rows");
    }

    /// Every projecting stage reads the same rule — `eventstats` used to
    /// answer `avg` where `stats` answered `avg_dur`.
    #[test]
    fn every_projecting_stage_answers_alike() {
        for dsl in [
            "* | stats avg(lower(dur))",
            "* | eventstats avg(lower(dur))",
            "* | timechart span=5m avg(lower(dur))",
            "* | pivot avg(lower(dur)) on service",
        ] {
            assert_eq!(agg_output_name(&aggs(dsl)[0]), "avg_dur", "{dsl}");
        }
    }

    /// And every LANE: the column the SQL emitter writes, the alias the
    /// stream lane keys its snapshot rows by, and the name the pin scope
    /// kills are one string. This is the drift guard — it fails on every
    /// nested-argument row for the three implementations this module
    /// replaced.
    #[test]
    fn one_rule_in_every_lane() {
        use crate::pin_scope::PinScope;
        use crate::schema::{CanonicalType, FieldTypes, catalog_key};
        use crate::stream::{CompiledAggregation, StreamPlan, compile_stream_plan};

        for dsl in [
            "* | stats count()",
            "* | stats avg(dur)",
            "* | stats avg(lower(dur))",
            "* | stats sum(a + b)",
            "* | stats avg(-x)",
            "* | stats count() as t",
        ] {
            let query = parser::parse(dsl).expect("parse should succeed");
            let want = agg_output_name(&aggs(dsl)[0]);

            let sql = crate::emitter::emit(&query, "/data/**/*.parquet")
                .expect("emit should succeed")
                .sql;
            assert!(
                sql.contains(&format!("AS \"{want}\"")),
                "{dsl}: SQL must project {want:?}: {sql}"
            );

            let plan = compile_stream_plan(&query.pipeline, &PinScope::unpinned())
                .expect("plan should compile");
            match plan {
                StreamPlan::Aggregate {
                    aggregation: CompiledAggregation::Stats { accumulators, .. },
                    ..
                } => assert_eq!(accumulators[0].alias, want, "{dsl}: stream alias"),
                _ => panic!("{dsl}: expected a stats aggregation plan"),
            }

            // The pin scope must kill exactly the column the stage writes:
            // killing a different name leaves the real output column
            // wearing the pin of the field it was computed from.
            let mut root = FieldTypes::new();
            root.insert(&catalog_key(&want), CanonicalType::Varchar);
            let mut scope = PinScope::root(&root);
            for stage in &query.pipeline {
                scope.advance(&stage.node);
            }
            assert_eq!(scope.pin_for(&want), None, "{dsl}: pin scope");
        }
    }
}
