// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a projecting stage NAMES — the one lane-neutral answer.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use crate::ast::{AggExpr, Expr, PipeStage, Spanned};
use crate::parser::suggest::quote_dsl_field;
use crate::schema::catalog_key;

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

fn innermost_field_name(arg: Option<&Spanned<Expr>>) -> Option<String> {
    match &arg?.node {
        Expr::FieldRef(name) => Some(name.clone()),
        Expr::FunctionCall { args, .. } => innermost_field_name(args.first()),
        Expr::Binary { lhs, .. } => innermost_field_name(Some(lhs)),
        Expr::Unary { operand, .. } => innermost_field_name(Some(operand)),
        _ => None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Producer {
    GroupKey,
    Aggregate,
    TimeBucket,
    MintedCount,
    Subject,
}

struct Output {
    key: String,
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

    fn label(&self, agg: Option<&AggExpr>) -> String {
        let name = label_name(&self.name);
        match self.producer {
            Producer::GroupKey => format!("the group key {name}"),
            Producer::Aggregate => match agg {
                Some(a) => format!("the aggregate `{}`", render_agg(a)),
                None => format!("the aggregate {name}"),
            },
            Producer::TimeBucket => format!("the `timechart` time bucket {name}"),
            Producer::MintedCount => format!("the {name} column the stage mints"),
            Producer::Subject => format!("the counted field {name}"),
        }
    }
}

/// A string literal argument admits ESC, BEL, U+202E and every other
/// [`sanitize::is_unsafe_display_char`], and the error messages this feeds
/// are 400 bodies rendered in a terminal, the TUI and a browser, so the
/// rendering is sanitised.
///
/// [`sanitize::is_unsafe_display_char`]: crate::sanitize::is_unsafe_display_char
fn render_agg(agg: &AggExpr) -> String {
    let mut out = String::new();
    crate::format::format_agg_expr(agg, &mut out);
    crate::sanitize::sanitize_display_text(&out)
}

fn label_name(name: &str) -> String {
    let dsl = quote_dsl_field(name).expect("projection names came from a parsed AST");
    if dsl.starts_with('`') {
        dsl
    } else {
        format!("`{dsl}`")
    }
}

/// Refuse a projecting stage that would mint two columns of one name
/// (ADR-0013 ruling 8).
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

fn check_frequency(keyword: &str, field: &str, by: &[String]) -> Result<(), String> {
    let mut outputs = vec![Output::new(field, Producer::Subject)];
    outputs.extend(group_outputs(by));
    outputs.push(Output::new("count", Producer::MintedCount));
    check_outputs(keyword, &outputs, &[])
}

/// A hash map, not a pairwise scan: a query is allowed thousands of group
/// keys, and a pairwise scan would be quadratic in request-controlled input.
fn check_outputs(
    keyword: &str,
    statics: &[Output],
    aggregations: &[AggExpr],
) -> Result<(), String> {
    let mut seen: HashMap<&str, (&Output, Option<&AggExpr>)> = HashMap::new();
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
        match seen.entry(output.key.as_str()) {
            Entry::Occupied(first) => {
                let (first, first_agg) = *first.get();
                return Err(collision_message(keyword, first, first_agg, output, agg));
            }
            Entry::Vacant(slot) => {
                slot.insert((output, agg));
            }
        }
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
    let name = label_name(&first.name);
    let remedy = remedy(keyword, first.producer, second.producer);
    format!(
        "`{keyword}` would project two columns named {name}: {} and {} — {remedy}",
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

    fn fixed_anchor() -> crate::context::EvalContext {
        crate::context::EvalContext::at(
            chrono::DateTime::parse_from_rfc3339("2026-08-24T12:00:00Z")
                .expect("literal is RFC 3339")
                .with_timezone(&chrono::Utc),
        )
    }

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

    #[test]
    fn default_walks_to_the_innermost_field() {
        assert_eq!(name("| stats avg(tonumber(rssi) * -1)"), "avg_rssi");
        assert_eq!(name("| stats max(length(message))"), "max_message");
    }

    #[test]
    fn a_literal_argument_has_no_field_to_name() {
        assert_eq!(name("| stats count(1)"), "count");
    }

    fn refusal(dsl: &str) -> String {
        let query = parser::parse(dsl).expect("dsl parses");
        check_projection(&query.pipeline[0].node).expect_err(&format!("{dsl} must be refused"))
    }

    #[test]
    fn a_collision_with_an_aggregate_demands_as() {
        let msg = refusal("* | stats count() by count");
        assert!(msg.contains("give one an explicit name with `as`"), "{msg}");
    }

    #[test]
    fn a_frequency_collision_offers_the_stats_rewrite() {
        let msg = refusal("* | top 5 count");
        assert!(
            msg.contains("`stats count() as <name> by <field>`"),
            "{msg}"
        );
        assert!(!msg.contains("give one an explicit name"), "{msg}");
    }

    #[test]
    fn a_bucket_collision_says_the_bucket_is_fixed() {
        let msg = refusal("* | timechart span=1h count() by _time");
        assert!(
            msg.contains("always projects its bucket as `_time`"),
            "{msg}"
        );
    }

    #[test]
    fn duplicate_group_keys_are_told_to_name_it_once() {
        let msg = refusal("* | pivot count() on status by host, Host");
        assert!(msg.contains("name it once"), "{msg}");
        assert!(msg.contains("`host`"), "{msg}");
    }

    #[test]
    fn the_collision_check_folds_names() {
        let msg = refusal("* | stats count() as total, sum(x) as Total");
        assert!(msg.contains("count() as total"), "{msg}");
        assert!(msg.contains("sum(x) as Total"), "{msg}");
    }

    #[test]
    fn a_refusal_renders_names_as_dsl_text() {
        let msg = refusal("* | eventstats avg(`response time`)");
        assert!(
            msg.contains("eventstats avg(`response time`) as <name>"),
            "{msg}"
        );
        parser::parse("* | eventstats avg(`response time`) as slow")
            .expect("the offered rewrite parses");

        let msg = refusal("* | pivot count() on status by `request id`, `Request ID`");
        assert!(msg.contains("named `request id`"), "{msg}");
    }

    #[test]
    fn a_refusal_never_echoes_control_characters() {
        for dsl in [
            "* | eventstats max(replace(message, \"\u{1b}]0;pwned\u{7}\", \"\"))",
            "* | stats count(\"\u{1b}[2Kevil\") as n, sum(x) as N",
            "* | stats count(\"\u{202e}drowssap\") as n, sum(x) as N",
        ] {
            let msg = refusal(dsl);
            assert!(
                !msg.chars().any(crate::sanitize::is_unsafe_display_char),
                "{msg:?}"
            );
            assert!(msg.contains(crate::sanitize::REPLACEMENT), "{msg:?}");
        }
    }

    #[test]
    fn pivot_checks_by_keys_only() {
        assert!(check_projection(&stage("* | pivot count() on status by status")).is_ok());
        assert!(check_projection(&stage("* | pivot count() on status")).is_ok());
    }

    #[test]
    fn a_very_wide_stage_is_checked_in_linear_time() {
        let keys: Vec<String> = (0..8000).map(|i| format!("k{i}")).collect();
        let wide = format!("* | stats count() as c by {}", keys.join(", "));
        assert!(check_projection(&stage(&wide)).is_ok());

        let dup = format!("* | stats count() as c by {}, K0", keys.join(", "));
        let msg = check_projection(&stage(&dup)).expect_err("the duplicate must be refused");
        assert!(msg.contains("name it once"), "{msg}");
    }

    fn stage(dsl: &str) -> PipeStage {
        parser::parse(dsl).expect("dsl parses").pipeline[0]
            .node
            .clone()
    }

    #[test]
    fn every_lane_projects_the_same_column_name() {
        for (dsl, expected, streams) in [
            ("* | stats avg(tonumber(rssi) * -1)", "avg_rssi", false),
            ("* | stats count() as total", "total", true),
            ("* | stats count(1)", "count", true),
            ("* | stats avg(duration) as slow", "slow", true),
            (
                "* | timechart span=1h max(length(message))",
                "max_message",
                false,
            ),
        ] {
            let query = parser::parse(dsl).expect("dsl parses");

            let sql = crate::emitter::emit(&query, "src", fixed_anchor())
                .expect("emit succeeds")
                .sql;
            assert!(
                sql.contains(&format!("AS \"{expected}\"")),
                "{dsl}: SQL lane must alias {expected}, got {sql}"
            );

            let plan = crate::stream::compile_stream_plan(
                &query.pipeline,
                &crate::pin_scope::PinScope::unpinned(),
                None,
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

    #[test]
    fn only_non_null_literal_count_uses_the_row_counter() {
        for dsl in [
            "* | stats count(null)",
            "* | stats avg(1)",
            "* | stats sum(1)",
        ] {
            let query = parser::parse(dsl).expect("dsl parses");
            assert!(
                crate::stream::compile_stream_plan(
                    &query.pipeline,
                    &crate::pin_scope::PinScope::unpinned(),
                    None,
                )
                .is_err(),
                "{dsl} must remain refused"
            );
        }
    }

    #[test]
    fn duplicate_capture_names_are_refused_in_both_lanes() {
        for dsl in [
            r#"* | extract "(?P<A>.)(?P<a>.)" from message"#,
            r#"* | extract "(?P<dur>.)(?P<DUR>.)" from message"#,
        ] {
            let query = parser::parse(dsl).expect("the parser does not read capture names");
            let sql = crate::emitter::validate_pipeline(&query.pipeline)
                .expect_err("the SQL lane must refuse")
                .to_string();
            let stream = crate::stream::compile_stream_plan(
                &query.pipeline,
                &crate::pin_scope::PinScope::unpinned(),
                None,
            )
            .expect_err("the stream lane must refuse")
            .to_string();
            assert_eq!(
                sql.strip_prefix("unsupported operation: ").unwrap_or(&sql),
                stream,
                "{dsl}: both lanes must carry one refusal sentence"
            );
            assert!(
                sql.contains("which name one column") && sql.contains("extract writes"),
                "{dsl}: {sql}"
            );
        }

        let dsl = r#"* | extract "(?P<ip>.)(?P<port>.)" from message"#;
        let query = parser::parse(dsl).expect("parses");
        crate::emitter::validate_pipeline(&query.pipeline).expect("SQL lane accepts");
        crate::stream::compile_stream_plan(
            &query.pipeline,
            &crate::pin_scope::PinScope::unpinned(),
            None,
        )
        .expect("stream lane accepts");
    }
}
