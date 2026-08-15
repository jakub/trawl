// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Which catalog keys a query BINDS (ADR-0011 slice C1).
//!
//! The incomplete-results notice on `/api/v1/query` intersects this set with
//! the degraded-field set: a query that filters on a field whose pin has
//! been shelving values is answering from data the pin destroyed, and that
//! is true whether or not the field appears in the output. A `where` on a
//! degraded field that projects it away is exactly the incomplete case, so
//! the walk collects every BOUND name — filters, pipeline expressions,
//! group-by keys, sort keys, projections — not the result columns.
//!
//! Deliberately NOT collected: bare-word and quoted text search, and the
//! time filters. A bare term binds `message` and `_raw`, both envelope
//! VARCHAR pins that cannot degrade in practice, and counting them would
//! badge every text query on an install with one bad `_raw`.
//!
//! Deliberately NOT pin-scope-aware either. A name a `let` or `stats`
//! INVENTS can enter the set; it is not a catalog pin, so intersecting with
//! the degraded set drops it. The one residual is a computed target that
//! SHADOWS a degraded field name (`| let status = … | where status > 400`),
//! which costs one false badge on a query that reuses the name — accepted
//! rather than defended, because the alternative is a second pin-scope walk
//! whose only consumer is a notice.

use std::collections::BTreeSet;

use crate::ast::{Expr, PipeStage, Query, SearchToken, Spanned};
use crate::schema;

/// Every catalog key `query` binds, in sorted order.
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

/// Parse `dsl` and collect its bound fields; an unparseable query
/// contributes nothing.
///
/// Total by design: the notice is only ever stamped on a query that already
/// EXECUTED, so a parse failure here means the caller and the executor
/// disagree — and a notice is not the place to surface that.
#[must_use]
pub fn referenced_fields_in(dsl: &str) -> BTreeSet<String> {
    crate::parser::parse(dsl).map_or_else(|_| BTreeSet::new(), |q| referenced_fields(&q))
}

/// The catalog key a DSL field reference names — the ASCII fold, and
/// nothing else. The DSL has zero aliases (ADR-0013 §6), so a reference
/// names the column it spells.
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
        // Whole-event search and the time bounds bind no named field the
        // notice should speak for (see the module doc).
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
            // RHS only — the targets are OUTPUTS, and a computed target is
            // not a catalog field however it is spelled.
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
        // Sources only: the target is a name the stage creates.
        PipeStage::Rename(s) => {
            for (from, _) in &s.renames {
                insert(from, out);
            }
        }
        // `None` reads `message`, which the module doc excludes for the same
        // reason bare-word search is excluded.
        PipeStage::Extract(s) => {
            if let Some(field) = &s.source_field {
                insert(field, out);
            }
        }
        // Stages that bind no field at all. `drop` is one of them on
        // purpose: it names a column to REMOVE, so nothing downstream can
        // depend on that column's values and naming it must not badge the
        // query.
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
            // The incomplete case the notice exists for: filtered on,
            // projected away.
            ("* | where duration > 1 | table host", &["duration", "host"]),
            ("* | let ms = duration * 1000", &["duration"]),
            (
                "* | stats avg(duration) by service, host",
                &["duration", "host", "service"],
            ),
            (
                "* | eventstats avg(duration) by host",
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
            // `drop` REMOVES a column: the answer cannot depend on its
            // values, so naming it there binds nothing.
            ("* | where duration > 1 | drop message", &["duration"]),
            ("* | rename service as svc", &["service"]),
            ("* | extract kv from raw_body", &["raw_body"]),
            ("* | where x in (1, 2) and not isnull(y)", &["x", "y"]),
        ];
        for (dsl, expect) in cases {
            assert_eq!(refs(dsl), expect, "{dsl}");
        }
    }

    /// Whole-event search binds `message`/`_raw` and every query has a time
    /// bound; badging on those would badge everything.
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

    /// DSL aliases name their physical column: the notice must speak the
    /// catalog's spelling, not the query's.
    #[test]
    fn names_fold_and_never_alias() {
        // Zero aliases (ADR-0013 §6): each name is its own catalog key.
        assert_eq!(refs("timestamp>\"2026-01-01\""), vec!["timestamp"]);
        assert_eq!(refs("* | sort -@timestamp"), vec!["@timestamp"]);
        // Zero aliases: `level` binds the sender's own `level` column.
        assert_eq!(refs("level=error"), vec!["level"]);
        assert_eq!(refs("* | where level == \"error\""), vec!["level"]);
        assert_eq!(refs("Status=200"), vec!["status"]);
    }

    /// A query that cannot be parsed contributes nothing — the notice rides
    /// on successful executions only.
    #[test]
    fn an_unparseable_query_contributes_nothing() {
        assert!(refs("| where |||").is_empty());
        assert!(refs("").is_empty());
    }

    /// Documented residual: a computed target shadowing a real field name
    /// is over-collected. Pinned here so the cost stays visible.
    #[test]
    fn a_computed_target_shadowing_a_field_is_over_collected() {
        assert_eq!(
            refs("* | let status = 1 | where status > 0"),
            vec!["status"],
            "the `where` reads the DERIVED status, but the walk is pin-blind"
        );
    }
}
