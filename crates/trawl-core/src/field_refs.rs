// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Which catalog keys a query binds (ADR-0011).

use std::collections::BTreeSet;

use crate::ast::{Expr, PipeStage, Query, SearchToken, Spanned};
use crate::schema;

#[must_use]
pub fn referenced_fields(query: &Query) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for token in query.search.all_tokens() {
        collect_token(&token.node, &mut out);
    }
    for stage in &query.pipeline {
        collect_stage(&stage.node, &mut out);
    }
    out
}

#[must_use]
pub fn referenced_fields_in(dsl: &str) -> BTreeSet<String> {
    crate::parser::parse(dsl).map_or_else(|_| BTreeSet::new(), |q| referenced_fields(&q))
}

#[must_use]
pub fn reads_text_implicitly(query: &Query) -> bool {
    query
        .search
        .all_tokens()
        .any(|token| token_reads_text(&token.node))
        || query
            .pipeline
            .iter()
            .any(|stage| matches!(&stage.node, PipeStage::Extract(e) if e.source_field.is_none()))
}

fn token_reads_text(token: &SearchToken) -> bool {
    match token {
        SearchToken::TextSearch(t) => t.term != "*",
        SearchToken::QuotedSearch(_) => true,
        SearchToken::Not(inner) => token_reads_text(&inner.node),
        SearchToken::Group(groups) => groups
            .iter()
            .flatten()
            .any(|token| token_reads_text(&token.node)),
        SearchToken::FieldFilter(_)
        | SearchToken::TimeFilter(_)
        | SearchToken::EarliestFilter(_)
        | SearchToken::LatestFilter(_) => false,
    }
}

fn key(name: &str) -> String {
    schema::catalog_key(name)
}

fn insert(name: &str, out: &mut BTreeSet<String>) {
    out.insert(key(name));
}

fn insert_all(names: &[String], out: &mut BTreeSet<String>) {
    for name in names {
        insert(name, out);
    }
}

fn collect_token(token: &SearchToken, out: &mut BTreeSet<String>) {
    match token {
        SearchToken::FieldFilter(f) => insert(&f.field, out),
        SearchToken::Not(inner) => collect_token(&inner.node, out),
        SearchToken::Group(groups) => {
            for token in groups.iter().flatten() {
                collect_token(&token.node, out);
            }
        }
        SearchToken::TextSearch(_)
        | SearchToken::QuotedSearch(_)
        | SearchToken::TimeFilter(_)
        | SearchToken::EarliestFilter(_)
        | SearchToken::LatestFilter(_) => {}
    }
}

fn collect_stage(stage: &PipeStage, out: &mut BTreeSet<String>) {
    // Exhaustive on purpose: a new stage has to state whether it binds
    // fields, rather than inherit "no" from a wildcard arm.
    match stage {
        PipeStage::Where(s) => collect_expr(&s.condition.node, out),
        PipeStage::Let(s) => {
            for (_, expr) in &s.assignments {
                collect_expr(&expr.node, out);
            }
        }
        PipeStage::Stats(s) => {
            insert_all(&s.group_by, out);
            collect_aggs(&s.aggregations, out);
        }
        PipeStage::EventStats(s) => {
            insert_all(&s.group_by, out);
            collect_aggs(&s.aggregations, out);
        }
        PipeStage::Timechart(s) => {
            if let Some(on) = s.on.as_deref().filter(|c| !crate::schema::is_event_time(c)) {
                insert(on, out);
            }
            insert_all(&s.group_by, out);
            collect_aggs(&s.aggregations, out);
        }
        PipeStage::Top(s) => {
            insert(&s.field, out);
            insert_all(&s.by, out);
        }
        PipeStage::Rare(s) => {
            insert(&s.field, out);
            insert_all(&s.by, out);
        }
        PipeStage::Pivot(s) => {
            insert(&s.on_field, out);
            insert_all(&s.by, out);
            collect_aggs(std::slice::from_ref(&s.aggregation), out);
        }
        PipeStage::Sort(s) => {
            for field in &s.fields {
                insert(&field.field, out);
            }
        }
        PipeStage::Dedup(s) => insert_all(&s.fields, out),
        PipeStage::Table(s) => insert_all(&s.fields, out),
        PipeStage::Rename(s) => {
            for (from, _) in &s.renames {
                insert(from, out);
            }
        }
        PipeStage::Extract(s) => {
            if let Some(field) = &s.source_field {
                insert(field, out);
            }
        }
        PipeStage::Drop(_)
        | PipeStage::Limit(_)
        | PipeStage::Tail(_)
        | PipeStage::Sample(_)
        | PipeStage::FromSaved(_) => {}
    }
}

fn collect_aggs(aggs: &[crate::ast::AggExpr], out: &mut BTreeSet<String>) {
    for agg in aggs {
        for arg in &agg.args {
            collect_expr(&arg.node, out);
        }
    }
}

fn collect_expr(expr: &Expr, out: &mut BTreeSet<String>) {
    match expr {
        Expr::FieldRef(name) => insert(name, out),
        Expr::Binary { lhs, rhs, .. } => {
            collect_expr(&lhs.node, out);
            collect_expr(&rhs.node, out);
        }
        Expr::Unary { operand, .. } => collect_expr(&operand.node, out),
        Expr::FunctionCall { args, .. } => collect_exprs(args, out),
        Expr::InList { expr, list } => {
            collect_expr(&expr.node, out);
            collect_exprs(list, out);
        }
        Expr::Literal(_) => {}
    }
}

fn collect_exprs(exprs: &[Spanned<Expr>], out: &mut BTreeSet<String>) {
    for expr in exprs {
        collect_expr(&expr.node, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs(dsl: &str) -> Vec<String> {
        referenced_fields_in(dsl).into_iter().collect()
    }

    #[test]
    fn every_binding_position_is_collected() {
        let cases: [(&str, &[&str]); 17] = [
            ("service=nginx", &["service"]),
            ("status>=400 host=web01", &["host", "status"]),
            ("NOT status=200", &["status"]),
            (
                "(status=500 OR path=/api/*) service=nginx",
                &["path", "service", "status"],
            ),
            ("* | where duration > 1 | table host", &["duration", "host"]),
            ("* | let ms = duration * 1000", &["duration"]),
            (
                "* | stats avg(duration) by service, host",
                &["duration", "host", "service"],
            ),
            (
                "* | eventstats avg(duration) as avg_duration by host",
                &["duration", "host"],
            ),
            ("* | timechart span=5m count() by service", &["service"]),
            ("* | top 10 host by service", &["host", "service"]),
            ("* | rare 5 status", &["status"]),
            (
                "* | pivot avg(duration) on status by host",
                &["duration", "host", "status"],
            ),
            ("* | sort -count, host | dedup host", &["count", "host"]),
            ("* | where duration > 1 | drop message", &["duration"]),
            ("* | rename service as svc", &["service"]),
            ("* | extract kv from raw_body", &["raw_body"]),
            ("* | where x in (1, 2) and not isnull(y)", &["x", "y"]),
        ];
        for (dsl, expect) in cases {
            assert_eq!(refs(dsl), expect, "{dsl}");
        }
    }

    #[test]
    fn text_search_and_time_bounds_bind_nothing() {
        for dsl in [
            "error",
            "\"connection refused\"",
            "-debug last=1h",
            "last=7d",
            "* | extract kv",
            "* | limit 5 | tail 2",
        ] {
            assert!(refs(dsl).is_empty(), "{dsl}: {:?}", refs(dsl));
        }
    }

    #[test]
    fn names_fold_and_never_alias() {
        assert_eq!(refs("timestamp>\"2026-01-01\""), vec!["timestamp"]);
        assert_eq!(refs("* | sort -@timestamp"), vec!["@timestamp"]);
        assert_eq!(refs("level=error"), vec!["level"]);
        assert_eq!(refs("* | where level == \"error\""), vec!["level"]);
        assert_eq!(refs("Status=200"), vec!["status"]);
    }

    #[test]
    fn an_unparseable_query_contributes_nothing() {
        assert!(refs("| where |||").is_empty());
        assert!(refs("").is_empty());
    }

    #[test]
    fn implicit_text_reads_are_text_search_and_default_extract() {
        let reads = |dsl: &str| reads_text_implicitly(&crate::parser::parse(dsl).expect("parses"));
        for dsl in [
            "error",
            "\"connection refused\"",
            "-debug",
            "NOT debug",
            "host=a OR boom",
            "(host=a OR (boom service=b))",
            "* | extract kv",
            r#"* | extract "(?P<w>x)""#,
        ] {
            assert!(reads(dsl), "{dsl}");
        }
        for dsl in [
            "*",
            "host=web1",
            "last=1h",
            "* | stats count()",
            r#"* | extract "(?P<w>x)" from body"#,
            "* | extract kv from body",
        ] {
            assert!(!reads(dsl), "{dsl}");
        }
    }

    #[test]
    fn a_computed_target_shadowing_a_field_is_over_collected() {
        assert_eq!(
            refs("* | let status = 1 | where status > 0"),
            vec!["status"],
            "the `where` reads the DERIVED status, but the walk is pin-blind"
        );
    }
}
