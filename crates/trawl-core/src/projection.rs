// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a projecting stage NAMES — the one lane-neutral answer.
//!
//! An aggregation stage mints columns: group keys, the implicit
//! `timechart` time bucket, the frequency count `top`/`rare` desugar to,
//! and one column per aggregate — named by its explicit `as` alias, else
//! by the `func`/`func_arg` default. Three lanes have to agree on those
//! names (the SQL emitter, the SSE stream compiler, and the pin-scope
//! walk both consume), so the derivation lives here, beside
//! [`crate::schema`] and [`crate::pin_scope`], and not inside any one
//! lane's emitter.

use crate::ast::{AggExpr, Expr, Spanned};

/// The column name an aggregate projects: its explicit `as` alias, else
/// the `func_arg`/`func` default the SQL lane has always emitted.
///
/// This is the ONE derivation. The SQL emitter quotes it, the stream
/// compiler stores it on the accumulator, and the pin-scope walk removes
/// it from the scope — so what a query names is what every lane writes.
#[must_use]
pub fn agg_output_name(agg: &AggExpr) -> String {
    if let Some(alias) = &agg.alias {
        return alias.clone();
    }
    match innermost_field_name(agg.args.first()) {
        Some(arg) => format!("{}_{arg}", agg.function),
        None => agg.function.clone(),
    }
}

/// Find the innermost field reference of an aggregation argument.
///
/// Recurses through wrapping expressions (function calls, binary ops,
/// unary ops), so `avg(tonumber(rssi) * -1)` names its column
/// `avg_rssi` rather than a bare `avg`.
fn innermost_field_name(arg: Option<&Spanned<Expr>>) -> Option<String> {
    match &arg?.node {
        Expr::FieldRef(name) => Some(name.clone()),
        Expr::FunctionCall { args, .. } => innermost_field_name(args.first()),
        Expr::Binary { lhs, .. } => innermost_field_name(Some(lhs)),
        Expr::Unary { operand, .. } => innermost_field_name(Some(operand)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::PipeStage;
    use crate::parser;

    fn aggs(dsl: &str) -> Vec<AggExpr> {
        let query = parser::parse(dsl).expect("dsl parses");
        match &query.pipeline[0].node {
            PipeStage::Stats(s) => s.aggregations.clone(),
            PipeStage::Timechart(s) => s.aggregations.clone(),
            PipeStage::EventStats(s) => s.aggregations.clone(),
            PipeStage::Pivot(s) => vec![s.aggregation.clone()],
            other => panic!("{dsl} is not an aggregating stage: {other:?}"),
        }
    }

    fn name(dsl: &str) -> String {
        agg_output_name(&aggs(dsl)[0])
    }

    #[test]
    fn explicit_alias_wins() {
        assert_eq!(name("| stats count() as total"), "total");
        assert_eq!(name("| stats avg(dur) as `mean dur`"), "mean dur");
    }

    #[test]
    fn default_is_func_or_func_arg() {
        assert_eq!(name("| stats count()"), "count");
        assert_eq!(name("| stats avg(duration)"), "avg_duration");
        assert_eq!(name("| stats dc(host)"), "dc_host");
    }

    /// The default walks to the INNERMOST field, so a computed argument
    /// still names the field it is about.
    #[test]
    fn default_walks_to_the_innermost_field() {
        assert_eq!(name("| stats avg(tonumber(rssi) * -1)"), "avg_rssi");
        assert_eq!(name("| stats max(length(message))"), "max_message");
    }

    #[test]
    fn a_literal_argument_has_no_field_to_name() {
        assert_eq!(name("| stats count(1)"), "count");
    }

    /// One derivation, every lane: the SQL emitter's `AS` alias, the
    /// stream compiler's accumulator column and the pin-scope walk's
    /// removal all read `agg_output_name`, so a computed-argument
    /// aggregate cannot be `avg_rssi` in batch and `avg` live.
    #[test]
    fn every_lane_projects_the_same_column_name() {
        for (dsl, expected) in [
            ("* | stats avg(tonumber(rssi) * -1)", "avg_rssi"),
            ("* | stats count() as total", "total"),
            ("* | timechart span=1h max(length(message))", "max_message"),
        ] {
            let query = parser::parse(dsl).expect("dsl parses");

            let sql = crate::emitter::emit(&query, "src")
                .expect("emit succeeds")
                .sql;
            assert!(
                sql.contains(&format!("AS \"{expected}\"")),
                "{dsl}: SQL lane must alias {expected}, got {sql}"
            );

            let plan = crate::stream::compile_stream_plan(
                &query.pipeline,
                &crate::pin_scope::PinScope::unpinned(),
            )
            .expect("stream plan compiles");
            let crate::stream::StreamPlan::Aggregate { aggregation, .. } = plan else {
                panic!("{dsl} must compile to an aggregate plan");
            };
            let (columns, _) = aggregation.snapshot();
            assert!(
                columns.iter().any(|c| c == expected),
                "{dsl}: stream lane must project {expected}, got {columns:?}"
            );

            let mut ft = crate::schema::FieldTypes::new();
            ft.insert(expected, crate::schema::CanonicalType::BigInt);
            let mut scope = crate::pin_scope::PinScope::root(&ft);
            for stage in &query.pipeline {
                scope.advance(&stage.node);
            }
            assert_eq!(
                scope.pin_for(expected),
                None,
                "{dsl}: the pin scope must scrub the aggregate's own output column"
            );
        }
    }
}
