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

use crate::ast::{AggExpr, Expr, PipeStage, Spanned};
use crate::schema::catalog_key;

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

/// Which producer minted a projected column — the half of the message
/// that tells a user WHERE the name came from, and the input to the
/// remedy (only some producers can carry an `as`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Producer {
    GroupKey,
    Aggregate,
    /// `timechart`'s bucket, always projected as `_time`.
    TimeBucket,
    /// The frequency column `top`/`rare` desugar to.
    MintedCount,
    /// The field `top`/`rare` counts values of.
    Subject,
}

/// One statically-known output column of a projecting stage.
struct Output {
    /// The name folded through [`catalog_key`] — `Total` IS `total`.
    key: String,
    /// The verbatim name, for the message.
    name: String,
    producer: Producer,
}

impl Output {
    fn new(name: &str, producer: Producer) -> Self {
        Self {
            key: catalog_key(name),
            name: name.to_string(),
            producer,
        }
    }

    /// How the message refers to this column.
    fn label(&self, agg: Option<&AggExpr>) -> String {
        let name = &self.name;
        match self.producer {
            Producer::GroupKey => format!("the group key `{name}`"),
            Producer::Aggregate => match agg {
                Some(a) => format!("the aggregate `{}`", render_agg(a)),
                None => format!("the aggregate `{name}`"),
            },
            Producer::TimeBucket => format!("the `timechart` time bucket `{name}`"),
            Producer::MintedCount => format!("the `{name}` column the stage mints"),
            Producer::Subject => format!("the counted field `{name}`"),
        }
    }
}

/// Render an aggregate the way the query spells it, for the message:
/// `count()`, `avg(duration)`, `count() as total`. A computed argument
/// renders as `…` — the name is what matters, not the arithmetic.
fn render_agg(agg: &AggExpr) -> String {
    let args: Vec<&str> = agg
        .args
        .iter()
        .map(|a| match &a.node {
            Expr::FieldRef(name) => name.as_str(),
            _ => "…",
        })
        .collect();
    let call = format!("{}({})", agg.function, args.join(", "));
    match &agg.alias {
        Some(alias) => format!("{call} as {alias}"),
        None => call,
    }
}

/// Refuse a projecting stage that would mint two columns of one name
/// (ADR-0013 ruling 8).
///
/// The output-name set of every projecting stage is assembled here —
/// group keys, `timechart`'s implicit `_time` bucket, the `count` column
/// `top`/`rare` desugar to, and each aggregate's output name — folded
/// through [`catalog_key`], so `Total` and `total` are one column. The
/// error names BOTH producers and the way out; it is returned as a plain
/// string so that each lane can carry it in its own error type without
/// either lane owning the sentence.
///
/// Non-projecting stages are `Ok` — the caller can hand every stage over
/// without matching first.
///
/// `pivot`'s output set is its `by` keys ALONE: the `on` field's values
/// become columns `DuckDB` mints from the data at run time, and the `on`
/// field itself is consumed. `eventstats` projects `*` plus its aliases,
/// so an alias naming an incoming column overwrites it (documented,
/// `let`-like) — but it must HAVE an alias, because the live lane cannot
/// know a row's schema before the rows arrive.
///
/// # Errors
///
/// Returns the shared message when two producers name one column, or
/// when an `eventstats` aggregate has no explicit `as`.
pub fn check_projection(stage: &PipeStage) -> Result<(), String> {
    match stage {
        PipeStage::Stats(s) => check_outputs("stats", &group_outputs(&s.group_by), &s.aggregations),
        PipeStage::Timechart(t) => {
            let mut outputs = vec![Output::new(crate::schema::TIME, Producer::TimeBucket)];
            outputs.extend(group_outputs(&t.group_by));
            check_outputs("timechart", &outputs, &t.aggregations)
        }
        PipeStage::Top(t) => check_frequency("top", &t.field, &t.by),
        PipeStage::Rare(r) => check_frequency("rare", &r.field, &r.by),
        PipeStage::Pivot(p) => check_outputs("pivot", &group_outputs(&p.by), &[]),
        PipeStage::EventStats(es) => {
            for agg in &es.aggregations {
                if agg.alias.is_none() {
                    return Err(eventstats_alias_message(agg));
                }
            }
            check_outputs("eventstats", &[], &es.aggregations)
        }
        _ => Ok(()),
    }
}

fn group_outputs(group_by: &[String]) -> Vec<Output> {
    group_by
        .iter()
        .map(|f| Output::new(f, Producer::GroupKey))
        .collect()
}

/// `top`/`rare` desugar to `stats count() by <field>[, <by>]`, so their
/// static output set is the counted field, the group keys, and the
/// `count` column the desugar mints.
fn check_frequency(keyword: &str, field: &str, by: &[String]) -> Result<(), String> {
    let mut outputs = vec![Output::new(field, Producer::Subject)];
    outputs.extend(group_outputs(by));
    outputs.push(Output::new("count", Producer::MintedCount));
    check_outputs(keyword, &outputs, &[])
}

/// The one duplicate scan: static outputs first, then one output per
/// aggregate, in projection order.
fn check_outputs(
    keyword: &str,
    statics: &[Output],
    aggregations: &[AggExpr],
) -> Result<(), String> {
    let mut seen: Vec<(&Output, Option<&AggExpr>)> = Vec::new();
    let agg_outputs: Vec<Output> = aggregations
        .iter()
        .map(|agg| Output::new(&agg_output_name(agg), Producer::Aggregate))
        .collect();

    let all = statics.iter().map(|o| (o, None)).chain(
        agg_outputs
            .iter()
            .zip(aggregations)
            .map(|(o, a)| (o, Some(a))),
    );

    for (output, agg) in all {
        if let Some((first, first_agg)) = seen.iter().find(|(prev, _)| prev.key == output.key) {
            return Err(collision_message(keyword, first, *first_agg, output, agg));
        }
        seen.push((output, agg));
    }
    Ok(())
}

fn collision_message(
    keyword: &str,
    first: &Output,
    first_agg: Option<&AggExpr>,
    second: &Output,
    second_agg: Option<&AggExpr>,
) -> String {
    let name = &first.name;
    let remedy = remedy(keyword, first.producer, second.producer);
    format!(
        "`{keyword}` would project two columns named `{name}`: {} and {} — {remedy}",
        first.label(first_agg),
        second.label(second_agg),
    )
}

fn remedy(keyword: &str, first: Producer, second: Producer) -> String {
    let has = |p: Producer| first == p || second == p;
    if has(Producer::Aggregate) {
        "give one an explicit name with `as`".to_string()
    } else if has(Producer::MintedCount) {
        format!(
            "`{keyword}` always mints its own `count` column and cannot spell `as` — \
             write it as `stats count() as <name> by <field>` instead"
        )
    } else if has(Producer::TimeBucket) {
        "`timechart` always projects its bucket as `_time`, so `_time` cannot also be \
         a group key"
            .to_string()
    } else {
        "name it once".to_string()
    }
}

fn eventstats_alias_message(agg: &AggExpr) -> String {
    let call = render_agg(agg);
    format!(
        "`eventstats {call}` needs an explicit name: `eventstats` adds a column to every \
         row, and the live lane cannot know a row's schema before the rows arrive — write \
         `eventstats {call} as <name>`. An alias naming a column the rows already carry \
         overwrites it, the way `let` does."
    )
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

    // ── the collision check's remedies (ADR-0013 ruling 8) ────────────

    fn refusal(dsl: &str) -> String {
        let query = parser::parse(dsl).expect("dsl parses");
        check_projection(&query.pipeline[0].node).expect_err(&format!("{dsl} must be refused"))
    }

    /// An aggregate can carry a name, so the remedy is `as`.
    #[test]
    fn a_collision_with_an_aggregate_demands_as() {
        let msg = refusal("* | stats count() by count");
        assert!(msg.contains("give one an explicit name with `as`"), "{msg}");
    }

    /// `top`/`rare` cannot spell `as`, so the remedy is the rewrite.
    #[test]
    fn a_frequency_collision_offers_the_stats_rewrite() {
        let msg = refusal("* | top 5 count");
        assert!(
            msg.contains("`stats count() as <name> by <field>`"),
            "{msg}"
        );
        assert!(!msg.contains("give one an explicit name"), "{msg}");
    }

    /// `timechart`'s bucket is structural — the group key is what moves.
    #[test]
    fn a_bucket_collision_says_the_bucket_is_fixed() {
        let msg = refusal("* | timechart span=1h count() by _time");
        assert!(
            msg.contains("always projects its bucket as `_time`"),
            "{msg}"
        );
    }

    /// Two spellings of one group key: nothing to alias, just say it once.
    #[test]
    fn duplicate_group_keys_are_told_to_name_it_once() {
        let msg = refusal("* | pivot count() on status by host, Host");
        assert!(msg.contains("name it once"), "{msg}");
        assert!(msg.contains("`host`"), "{msg}");
    }

    /// The fold is the ONE key: `Total` and `total` are one column.
    #[test]
    fn the_collision_check_folds_names() {
        let msg = refusal("* | stats count() as total, sum(x) as Total");
        assert!(msg.contains("count() as total"), "{msg}");
        assert!(msg.contains("sum(x) as Total"), "{msg}");
    }

    /// `pivot`'s value columns come from the DATA, so only its `by` keys
    /// are statically known — the `on` field is consumed, not projected.
    #[test]
    fn pivot_checks_by_keys_only() {
        assert!(check_projection(&stage("* | pivot count() on status by status")).is_ok());
        assert!(check_projection(&stage("* | pivot count() on status")).is_ok());
    }

    fn stage(dsl: &str) -> PipeStage {
        parser::parse(dsl).expect("dsl parses").pipeline[0]
            .node
            .clone()
    }

    /// One derivation, every lane: the SQL emitter's `AS` alias, the
    /// stream compiler's accumulator column and the pin-scope walk's
    /// removal all read `agg_output_name`.
    ///
    /// The live lane accumulates a BARE field only, so a computed
    /// argument is REFUSED there rather than projected — agreeing on the
    /// name while answering NULL would be parity in name only.
    #[test]
    fn every_lane_projects_the_same_column_name() {
        for (dsl, expected, streams) in [
            ("* | stats avg(tonumber(rssi) * -1)", "avg_rssi", false),
            ("* | stats count() as total", "total", true),
            ("* | stats avg(duration) as slow", "slow", true),
            (
                "* | timechart span=1h max(length(message))",
                "max_message",
                false,
            ),
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
            );
            if streams {
                let Ok(crate::stream::StreamPlan::Aggregate { aggregation, .. }) = plan else {
                    panic!("{dsl} must compile to an aggregate plan");
                };
                let (columns, _) = aggregation.snapshot();
                assert!(
                    columns.iter().any(|c| c == expected),
                    "{dsl}: stream lane must project {expected}, got {columns:?}"
                );
            } else {
                assert!(
                    matches!(
                        plan,
                        Err(crate::stream::StreamPlanError::UnsupportedStage { .. })
                    ),
                    "{dsl}: stream lane cannot evaluate a computed argument, so it must refuse"
                );
            }

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
