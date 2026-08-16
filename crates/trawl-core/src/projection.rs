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

use std::fmt;

use crate::ast::{AggExpr, Expr, PipeStage, Spanned};
use crate::schema::catalog_key;

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

/// How a projecting stage came to put a name on its output row. Drives the
/// remedy sentence — telling an operator to "use `as`" where the grammar
/// has no `as` to give is worse than no message at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameOrigin {
    /// A `by` key, or the subject field of `top`/`rare`.
    GroupKey,
    /// `timechart`'s implicit `_time` bucket.
    TimeBucket,
    /// `top`/`rare`'s implicit `count`.
    FrequencyCount,
    /// An aggregation with no `as`, named by [`agg_output_name`].
    AutoAlias { function: String },
    /// An aggregation's explicit `as` target.
    ExplicitAlias { function: String },
}

impl NameOrigin {
    /// How this producer reads in the collision message.
    fn describe(&self, name: &str) -> String {
        match self {
            Self::GroupKey => format!("the grouping field `{name}`"),
            Self::TimeBucket => "the time bucket `_time`".to_string(),
            Self::FrequencyCount => "the frequency column `count`".to_string(),
            Self::AutoAlias { function } => {
                format!("`{function}()`, which names its output `{name}`")
            }
            Self::ExplicitAlias { function } => format!("`{function}() as {name}`"),
        }
    }

    fn is_auto_alias(&self) -> bool {
        matches!(self, Self::AutoAlias { .. })
    }

    fn is_alias(&self) -> bool {
        matches!(self, Self::AutoAlias { .. } | Self::ExplicitAlias { .. })
    }
}

/// One name a stage puts on its output row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProducedName {
    pub name: String,
    pub origin: NameOrigin,
}

impl ProducedName {
    fn new(name: impl Into<String>, origin: NameOrigin) -> Self {
        Self {
            name: name.into(),
            origin,
        }
    }
}

/// Two producers, one column.
///
/// Carries both so the message can name them: "produces `count` twice" with
/// no second half leaves an operator hunting for the other half.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameCollision {
    /// The user's spelling of the stage keyword (`fields` vs `table`).
    pub stage: &'static str,
    /// The folded key the two names share.
    pub folded: String,
    pub first: ProducedName,
    pub second: ProducedName,
}

impl fmt::Display for NameCollision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "`{}` would produce the column `{}` twice — from {} and from {}",
            self.stage,
            self.first.name,
            self.first.origin.describe(&self.first.name),
            self.second.origin.describe(&self.second.name),
        )?;
        // Two spellings that fold together read as a puzzle otherwise:
        // `Host` and `host` are one DuckDB identifier.
        if self.first.name != self.second.name {
            write!(f, " (the two names fold to one column)")?;
        }
        write!(f, "; {}", self.remedy())
    }
}

impl std::error::Error for NameCollision {}

impl NameCollision {
    /// The half an operator acts on. Origin-driven, because `top` has no
    /// `as` clause to offer and `timechart` always writes `_time`.
    fn remedy(&self) -> String {
        let origins = [&self.first.origin, &self.second.origin];
        if origins.iter().any(|o| o.is_auto_alias()) {
            if self.stage == "eventstats" {
                return "give the aggregation an explicit name with `as` — `eventstats` \
                        overwrites a column of that name the way `let` does, so each \
                        output needs a name of its own"
                    .to_string();
            }
            return "give the aggregation an explicit name with `as`".to_string();
        }
        if origins.iter().any(|o| o.is_alias()) {
            // One side is already an explicit `as`, so the remedy is that
            // name — never "drop the duplicate", which would point at a
            // grouping field the query needs.
            return "give the aggregation's `as` a name nothing else in the stage produces"
                .to_string();
        }
        if origins.contains(&&NameOrigin::FrequencyCount) {
            return format!(
                "`{}` always produces `count`: rename the field with a preceding \
                 `| rename`, or use `stats`, where the output name is yours",
                self.stage
            );
        }
        if origins.contains(&&NameOrigin::TimeBucket) {
            return "`timechart` always produces `_time`: drop it from the `by` list".to_string();
        }
        "drop the duplicate".to_string()
    }
}

/// Every name `stage` projects, in emission order; empty for a stage that
/// projects nothing, so a caller may pass any stage.
///
/// The set is what the EMITTER writes, which is why it shares
/// [`agg_output_name`] with it. Two deliberate omissions:
///
/// - `pivot`'s value columns come from the ON field's DATA, so no static
///   check can see them; only the `by` keys are checkable. The aggregation
///   is NOT a producer either — trawl emits `PIVOT … USING <agg>` with the
///   alias dropped, so naming it here would report a collision the emitter
///   never creates.
/// - `eventstats` passes its input row through, so an alias equal to an
///   ordinary input column is the DOCUMENTED let-like overwrite, not a
///   collision. Its GROUP KEYS are a different matter and are produced
///   names like any other stage's: overwriting the column the stage
///   partitions BY destroys the grouping the row is grouped by.
#[must_use]
pub fn projected_names(stage: &PipeStage) -> Vec<ProducedName> {
    fn aggregations(aggs: &[AggExpr], out: &mut Vec<ProducedName>) {
        for agg in aggs {
            let function = agg.function.clone();
            let origin = if agg.alias.is_some() {
                NameOrigin::ExplicitAlias { function }
            } else {
                NameOrigin::AutoAlias { function }
            };
            out.push(ProducedName::new(agg_output_name(agg), origin));
        }
    }
    fn group_keys(keys: &[String], out: &mut Vec<ProducedName>) {
        out.extend(
            keys.iter()
                .map(|k| ProducedName::new(k.clone(), NameOrigin::GroupKey)),
        );
    }

    let mut out = Vec::new();
    match stage {
        PipeStage::Stats(s) => {
            group_keys(&s.group_by, &mut out);
            aggregations(&s.aggregations, &mut out);
        }
        PipeStage::Timechart(s) => {
            out.push(ProducedName::new(
                crate::schema::TIME,
                NameOrigin::TimeBucket,
            ));
            group_keys(&s.group_by, &mut out);
            aggregations(&s.aggregations, &mut out);
        }
        PipeStage::Top(s) => {
            group_keys(std::slice::from_ref(&s.field), &mut out);
            group_keys(&s.by, &mut out);
            out.push(ProducedName::new("count", NameOrigin::FrequencyCount));
        }
        PipeStage::Rare(s) => {
            group_keys(std::slice::from_ref(&s.field), &mut out);
            group_keys(&s.by, &mut out);
            out.push(ProducedName::new("count", NameOrigin::FrequencyCount));
        }
        PipeStage::Pivot(s) => group_keys(&s.by, &mut out),
        PipeStage::EventStats(s) => {
            group_keys(&s.group_by, &mut out);
            aggregations(&s.aggregations, &mut out);
        }
        _ => {}
    }
    out
}

/// The stage keyword a collision message names, in the user's own spelling.
fn stage_keyword(stage: &PipeStage) -> &'static str {
    match stage {
        PipeStage::Stats(_) => "stats",
        PipeStage::Timechart(_) => "timechart",
        PipeStage::Top(_) => "top",
        PipeStage::Rare(_) => "rare",
        PipeStage::Pivot(_) => "pivot",
        PipeStage::EventStats(_) => "eventstats",
        _ => "stage",
    }
}

/// Refuse a stage that would put two producers on one output column
/// (ADR-0013 ruling 8).
///
/// Called from `emitter::validate_pipeline` AND `stream::compile_stream_plan`
/// — the reserved-name-mint precedent, since the stream lane never runs the
/// emitter's validation and a duplicate column is not something SQL reports:
/// it silently emits two `AS` clauses and a later reference binds to
/// whichever the engine picks.
///
/// Names fold through [`catalog_key`] first, because that is what decides
/// whether two spellings are one column.
///
/// The collision is BOXED because it carries both producers and their
/// spellings — a 152-byte `Err` on the hot path of every stage validation
/// (`clippy::result_large_err`), where the error is the rare case.
///
/// # Errors
///
/// Returns the FIRST pair of produced names equal after folding.
pub fn check_projection_names(stage: &PipeStage) -> Result<(), Box<NameCollision>> {
    let produced = projected_names(stage);
    let mut seen: Vec<(String, ProducedName)> = Vec::with_capacity(produced.len());
    for name in produced {
        let folded = catalog_key(&name.name);
        if let Some((_, first)) = seen.iter().find(|(key, _)| *key == folded) {
            return Err(Box::new(NameCollision {
                stage: stage_keyword(stage),
                folded,
                first: first.clone(),
                second: name,
            }));
        }
        seen.push((folded, name));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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

    /// The shared naming rule made the two lanes agree on the column NAME
    /// for a wrapped argument, which is only an improvement if the live
    /// lane can actually fill it. It cannot — an accumulator reads one
    /// event key — so it refuses the shape instead of streaming a column
    /// that is always empty beside a batch answer that is not.
    #[test]
    fn a_wrapped_argument_is_refused_by_the_live_lane_not_silently_empty() {
        use crate::pin_scope::PinScope;
        use crate::stream::compile_stream_plan;

        for dsl in [
            "* | stats avg(lower(dur))",
            "* | stats sum(a + b) by host",
            "* | timechart span=5m avg(-x)",
        ] {
            let query = parser::parse(dsl).expect("parse should succeed");
            // batch still answers it
            crate::emitter::emit(&query, "/data/**/*.parquet")
                .unwrap_or_else(|e| panic!("{dsl} must still emit: {e}"));
            // …and the stream says why it cannot
            let err = compile_stream_plan(&query.pipeline, &PinScope::unpinned())
                .expect_err(&format!("{dsl}: the stream lane must refuse"))
                .to_string();
            assert!(
                err.contains("computed argument"),
                "{dsl}: the refusal must name the shape: {err}"
            );
        }

        // `count(<non-null literal>)` IS `count()` — the same aggregate
        // in both lanes — so it is NOT part of the refused class, and it
        // must still agree with batch. `count(null)` counts nothing in
        // SQL, which a row counter cannot express, so it stays refused.
        let query = parser::parse("* | stats count(1)").expect("parse should succeed");
        let plan = compile_stream_plan(&query.pipeline, &PinScope::unpinned())
            .expect("count(1) must still stream");
        match plan {
            crate::stream::StreamPlan::Aggregate {
                aggregation: crate::stream::CompiledAggregation::Stats { accumulators, .. },
                ..
            } => {
                assert_eq!(accumulators[0].alias, "count");
                assert!(
                    accumulators[0].field.is_none(),
                    "it counts rows, like count()"
                );
            }
            _ => panic!("expected a stats aggregation plan"),
        }
        for refused in [
            "* | stats count(null)",
            "* | stats avg(1)",
            "* | stats sum(1)",
        ] {
            let query = parser::parse(refused).expect("parse should succeed");
            assert!(
                compile_stream_plan(&query.pipeline, &PinScope::unpinned()).is_err(),
                "{refused} must stay refused"
            );
        }

        // the plain shapes still stream
        for dsl in [
            "* | stats avg(dur)",
            "* | stats count() by host",
            "* | stats p95(dur)",
            "* | stats count(dur)",
        ] {
            let query = parser::parse(dsl).expect("parse should succeed");
            compile_stream_plan(&query.pipeline, &PinScope::unpinned())
                .unwrap_or_else(|e| panic!("{dsl} must still compile: {e}"));
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

        // `streams` is false for a wrapped argument: the live lane refuses
        // that shape outright (see the test above), so it has no alias to
        // compare — the SQL and pin-scope halves still apply to it.
        for (dsl, streams) in [
            ("* | stats count()", true),
            ("* | stats avg(dur)", true),
            ("* | stats avg(lower(dur))", false),
            ("* | stats sum(a + b)", false),
            ("* | stats avg(-x)", false),
            ("* | stats count() as t", true),
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

            if streams {
                let plan = compile_stream_plan(&query.pipeline, &PinScope::unpinned())
                    .expect("plan should compile");
                match plan {
                    StreamPlan::Aggregate {
                        aggregation: CompiledAggregation::Stats { accumulators, .. },
                        ..
                    } => assert_eq!(accumulators[0].alias, want, "{dsl}: stream alias"),
                    _ => panic!("{dsl}: expected a stats aggregation plan"),
                }
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

    // ── the collision check (ADR-0013 ruling 8) ──────────────────────────

    fn first_stage(dsl: &str) -> PipeStage {
        let query = parser::parse(dsl).expect("parse should succeed");
        query.pipeline[0].node.clone()
    }

    fn collision(dsl: &str) -> Box<NameCollision> {
        check_projection_names(&first_stage(dsl))
            .expect_err(&format!("{dsl} must be refused as a collision"))
    }

    /// Every projecting stage, every way two producers can land on one
    /// column. The message names BOTH producers — half a message leaves an
    /// operator hunting for the other producer.
    #[test]
    fn a_duplicate_output_column_is_refused_in_every_projecting_stage() {
        let cases: &[(&str, &str, &str)] = &[
            // stats: a group key against an auto alias — ruling 8's example
            (
                "* | stats count() by count",
                "count",
                "give the aggregation an explicit name with `as`",
            ),
            // …two aggregations with no `by` at all still collide
            (
                "* | stats count(), count()",
                "count",
                "give the aggregation an explicit name with `as`",
            ),
            // …duplicate group keys, including case-only variants
            (
                "* | stats count() by host, host",
                "host",
                "drop the duplicate",
            ),
            // …the message names the FIRST spelling, then says they fold
            (
                "* | stats count() by Host, host",
                "Host",
                "drop the duplicate",
            ),
            // …two explicit aliases
            (
                "* | stats count() as n, avg(dur) as n",
                "n",
                "a name nothing else in the stage produces",
            ),
            // timechart always writes `_time`
            (
                "* | timechart span=5m count() by _time",
                "_time",
                "drop it from the `by` list",
            ),
            // top/rare always write `count`, and have no `as` to offer
            ("* | top 5 count", "count", "use `stats`"),
            ("* | rare 5 count", "count", "use `stats`"),
            ("* | top 5 host by host", "host", "drop the duplicate"),
            // pivot's static half
            (
                "* | pivot count() on status by host, host",
                "host",
                "drop the duplicate",
            ),
            // eventstats: declared outputs AND group keys, with the
            // overwrite remedy
            (
                "* | eventstats count(), count()",
                "count",
                "overwrites a column of that name the way `let` does",
            ),
            // …including a group KEY: overwriting the column the stage
            // partitions by destroys the grouping it grouped by
            (
                "* | eventstats count() as service by service",
                "service",
                "a name nothing else in the stage produces",
            ),
        ];

        for (dsl, column, remedy) in cases {
            let err = collision(dsl).to_string();
            assert!(
                err.contains(&format!("column `{column}`")),
                "{dsl}: must name the column: {err}"
            );
            assert!(
                err.contains(remedy),
                "{dsl}: remedy must fit the stage: {err}"
            );
            assert!(
                err.matches(" and from ").count() == 1,
                "{dsl}: must name both producers: {err}"
            );
        }
    }

    /// The shapes that are NOT collisions — an over-broad check is worse
    /// than none, since it refuses working queries.
    #[test]
    fn distinct_output_columns_are_accepted() {
        for dsl in [
            "* | stats count() by host",
            "* | stats count(), avg(dur)",
            "* | stats count() as n, count() as m",
            "* | timechart span=5m count() by service",
            "* | top 5 host by service",
            "* | eventstats count()",
            // an eventstats alias equal to an INPUT column is the
            // documented let-like overwrite, not a duplicate
            "* | eventstats count() as service",
            // pivot's value columns are runtime data, so only `by` is checked
            "* | pivot count() on status by host",
            "* | pivot count() as host on status by host",
        ] {
            check_projection_names(&first_stage(dsl))
                .unwrap_or_else(|e| panic!("{dsl} must be accepted: {e}"));
        }
    }

    /// Two capture groups may not name ONE column, and the regex is the
    /// one write list the PARSER never sees — so the refusal lives at the
    /// two places the pattern is compiled, in the same words the
    /// `let`/`rename` parse refusal uses, and both lanes say it alike.
    #[test]
    fn duplicate_capture_names_are_refused_in_both_lanes() {
        use crate::pin_scope::PinScope;
        use crate::stream::compile_stream_plan;

        for dsl in [
            r#"* | extract "(?P<A>.)(?P<a>.)" from message"#,
            r#"* | extract "(?P<dur>.)(?P<DUR>.)" from message"#,
        ] {
            let query = parser::parse(dsl).expect("the parser does not read capture names");
            let sql = crate::emitter::validate_pipeline(&query.pipeline)
                .expect_err(&format!("{dsl}: the SQL lane must refuse"))
                .to_string();
            let stream = compile_stream_plan(&query.pipeline, &PinScope::unpinned())
                .expect_err(&format!("{dsl}: the stream lane must refuse"))
                .to_string();
            assert_eq!(sql, stream, "{dsl}: both lanes must print one sentence");
            assert!(
                sql.contains("which name one column") && sql.contains("extract writes"),
                "{dsl}: {sql}"
            );
        }

        // …and a multi-capture extract naming DISTINCT columns still works
        // in both lanes.
        let dsl = r#"* | extract "(?P<ip>.)(?P<port>.)" from message"#;
        let query = parser::parse(dsl).expect("parses");
        crate::emitter::validate_pipeline(&query.pipeline).expect("SQL lane accepts");
        compile_stream_plan(&query.pipeline, &PinScope::unpinned()).expect("stream lane accepts");
        crate::emitter::emit(&query, "/data/**/*.parquet").expect("emits");
    }

    /// One check, two lanes, one sentence — the `rejects_minting_reserved_names`
    /// precedent. The stream lane never runs `validate_pipeline`, so a
    /// second implementation is exactly how the two would drift apart.
    #[test]
    fn collisions_are_refused_in_both_lanes_with_one_message() {
        use crate::pin_scope::PinScope;
        use crate::stream::compile_stream_plan;

        for dsl in [
            "* | stats count() by count",
            "* | stats count(), count()",
            "* | stats count() by Host, host",
            "* | timechart span=5m count() by _time",
            "* | top 5 count",
            "* | rare 5 count",
            // refused as UNSUPPORTED by the stream lane — the collision
            // check runs first, so both lanes still say the same thing
            "* | pivot count() on status by host, host",
            "* | eventstats count(), count()",
        ] {
            let query = parser::parse(dsl).expect("parse should succeed");
            let sql = crate::emitter::validate_pipeline(&query.pipeline)
                .expect_err(&format!("{dsl}: the SQL lane must refuse"))
                .to_string();
            let stream = compile_stream_plan(&query.pipeline, &PinScope::unpinned())
                .expect_err(&format!("{dsl}: the stream lane must refuse"))
                .to_string();
            assert_eq!(sql, stream, "{dsl}: both lanes must print one sentence");
        }
    }
}
