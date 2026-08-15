// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! DSL query formatter — canonical pretty-printing of parsed queries.
//!
//! Takes a parsed AST and produces a consistently formatted DSL string.
//! One pipe stage per line, tight spacing in the search stage,
//! minimal parenthesization in expressions.

use std::fmt::Write;

use crate::ast::{
    AggExpr, BinaryOp, Expr, ExtractMode, FieldFilter, FilterOp, FilterValue, FromSavedStage,
    LiteralValue, PipeStage, Query, SampleMode, SavedRunSelector, SearchStage, SearchToken,
    SortDirection, UnaryOp,
};

/// Format a parsed query into canonical DSL form.
///
/// Output uses one pipe stage per line with `| ` prefix:
/// ```text
/// service=nginx _severity=error last=2h
/// | stats count() by host
/// | where count > 10
/// ```
#[must_use]
pub fn format_query(query: &Query) -> String {
    let mut out = String::new();
    format_search_stage(&query.search, &mut out);
    for stage in &query.pipeline {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("| ");
        format_pipe_stage(&stage.node, &mut out);
    }
    out
}

/// Convenience: parse and format in one step.
///
/// Returns `None` if the input fails to parse.
#[must_use]
pub fn reformat(input: &str) -> Option<String> {
    let query = crate::parser::parse(input).ok()?;
    Some(format_query(&query))
}

// ---------------------------------------------------------------------------
// Search stage
// ---------------------------------------------------------------------------

fn format_search_stage(search: &SearchStage, out: &mut String) {
    let has_groups = search.groups.iter().any(|g| !g.is_empty());

    if has_groups {
        for (i, group) in search.groups.iter().enumerate() {
            if i > 0 && !group.is_empty() {
                out.push_str(" OR ");
            }
            for (j, token) in group.iter().enumerate() {
                if j > 0 {
                    out.push(' ');
                }
                format_search_token(&token.node, out);
            }
        }
    }

    // Time filters go at the end of the search stage.
    if let Some(ref tf) = search.time_filter {
        if has_groups {
            out.push(' ');
        }
        let _ = write!(out, "last={}", tf.node.duration);
    }

    if let Some(ref earliest) = search.earliest {
        if !out.is_empty() {
            out.push(' ');
        }
        let _ = write!(out, "earliest=\"{}\"", earliest.node);
    }

    if let Some(ref latest) = search.latest {
        if !out.is_empty() {
            out.push(' ');
        }
        let _ = write!(out, "latest=\"{}\"", latest.node);
    }
}

fn format_search_token(token: &SearchToken, out: &mut String) {
    match token {
        SearchToken::FieldFilter(ff) => format_field_filter(ff, out),
        SearchToken::TextSearch(ts) => {
            if ts.negated {
                out.push('-');
            }
            // Bare words with special chars need quoting — but the parser only
            // produces TextSearch for simple bare words, so no quoting needed.
            out.push_str(&ts.term);
        }
        SearchToken::TimeFilter(tf) => {
            let _ = write!(out, "last={}", tf.duration);
        }
        SearchToken::QuotedSearch(qs) => {
            let _ = write!(out, "\"{}\"", qs.phrase);
        }
        SearchToken::EarliestFilter(val) => {
            let _ = write!(out, "earliest=\"{val}\"");
        }
        SearchToken::LatestFilter(val) => {
            let _ = write!(out, "latest=\"{val}\"");
        }
        SearchToken::Not(inner) => {
            out.push_str("NOT ");
            format_search_token(&inner.node, out);
        }
        SearchToken::Group(groups) => {
            out.push('(');
            for (i, group) in groups.iter().enumerate() {
                if i > 0 && !group.is_empty() {
                    out.push_str(" OR ");
                }
                for (j, token) in group.iter().enumerate() {
                    if j > 0 {
                        out.push(' ');
                    }
                    format_search_token(&token.node, out);
                }
            }
            out.push(')');
        }
    }
}

fn format_field_filter(ff: &FieldFilter, out: &mut String) {
    out.push_str(&ff.field);
    match ff.op {
        FilterOp::Glob | FilterOp::Regex => {
            // Glob and regex use `=` as the display operator, with the value
            // carrying the pattern syntax (glob wildcards or /regex/).
            out.push('=');
        }
        FilterOp::Eq => out.push('='),
        FilterOp::Ne => out.push_str("!="),
        FilterOp::Gt => out.push('>'),
        FilterOp::Gte => out.push_str(">="),
        FilterOp::Lt => out.push('<'),
        FilterOp::Lte => out.push_str("<="),
    }
    format_filter_value(&ff.value, ff.op, out);
}

/// Whether a filter value literal needs quoting in the formatted output.
fn needs_quoting(s: &str) -> bool {
    s.is_empty()
        || s.contains(' ')
        || s.contains('|')
        || s.contains('(')
        || s.contains(')')
        || s.contains(',')
        || s.contains('"')
}

fn format_filter_value(value: &FilterValue, op: FilterOp, out: &mut String) {
    match value {
        FilterValue::Literal(s) => {
            if op == FilterOp::Regex {
                let _ = write!(out, "/{s}/");
            } else if needs_quoting(s) {
                // Escape any embedded double quotes.
                let escaped = s.replace('"', "\\\"");
                let _ = write!(out, "\"{escaped}\"");
            } else {
                out.push_str(s);
            }
        }
        FilterValue::List(items) => {
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(item);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Pipe stages
// ---------------------------------------------------------------------------

fn format_pipe_stage(stage: &PipeStage, out: &mut String) {
    match stage {
        PipeStage::Stats(s) => format_stats("stats", &s.aggregations, &s.group_by, out),
        PipeStage::EventStats(s) => format_stats("eventstats", &s.aggregations, &s.group_by, out),
        PipeStage::Where(s) => {
            out.push_str("where ");
            format_expr(&s.condition.node, 0, ExprContext::Normal, out);
        }
        PipeStage::Sort(s) => format_sort(s, out),
        PipeStage::Limit(s) => {
            let _ = write!(out, "{} {}", s.keyword, s.count);
        }
        PipeStage::Tail(s) => {
            let _ = write!(out, "tail {}", s.count);
        }
        PipeStage::Table(s) => {
            let _ = write!(out, "{} {}", s.keyword, s.fields.join(", "));
        }
        PipeStage::Top(s) => format_top_rare("top", s.count, &s.field, &s.by, out),
        PipeStage::Rare(s) => format_top_rare("rare", s.count, &s.field, &s.by, out),
        PipeStage::Drop(s) => {
            let _ = write!(out, "drop {}", s.fields.join(", "));
        }
        PipeStage::Let(s) => format_let(s, out),
        PipeStage::Extract(s) => format_extract(s, out),
        PipeStage::Dedup(s) => format_dedup(s, out),
        PipeStage::Timechart(s) => format_timechart(s, out),
        PipeStage::Pivot(s) => format_pivot(s, out),
        PipeStage::Rename(s) => format_rename(s, out),
        PipeStage::Sample(s) => match s.mode {
            SampleMode::Percent(p) => {
                let _ = write!(out, "sample {p}%");
            }
            SampleMode::Count(n) => {
                let _ = write!(out, "sample {n}");
            }
        },
        PipeStage::FromSaved(s) => format_from_saved(s, out),
    }
}

fn format_sort(s: &crate::ast::SortStage, out: &mut String) {
    out.push_str("sort ");
    for (i, sf) in s.fields.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        if sf.direction == SortDirection::Desc {
            out.push('-');
        }
        out.push_str(&sf.field);
    }
}

fn format_top_rare(kw: &str, count: u64, field: &str, by: &[String], out: &mut String) {
    let _ = write!(out, "{kw} {count} {field}");
    if !by.is_empty() {
        let _ = write!(out, " by {}", by.join(", "));
    }
}

fn format_let(s: &crate::ast::LetStage, out: &mut String) {
    out.push_str(s.keyword);
    out.push(' ');
    for (i, (name, expr)) in s.assignments.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        let _ = write!(out, "{name} = ");
        format_expr(&expr.node, 0, ExprContext::Normal, out);
    }
}

fn format_dedup(s: &crate::ast::DedupStage, out: &mut String) {
    out.push_str("dedup");
    if !s.fields.is_empty() {
        let _ = write!(out, " {}", s.fields.join(", "));
    }
}

fn format_timechart(s: &crate::ast::TimechartStage, out: &mut String) {
    out.push_str("timechart");
    if let Some(span) = s.span {
        let _ = write!(out, " span={span}");
    }
    out.push(' ');
    format_agg_list(&s.aggregations, out);
    if !s.group_by.is_empty() {
        let _ = write!(out, " by {}", s.group_by.join(", "));
    }
}

fn format_pivot(s: &crate::ast::PivotStage, out: &mut String) {
    out.push_str("pivot ");
    format_agg_expr(&s.aggregation, out);
    let _ = write!(out, " on {}", s.on_field);
    if !s.by.is_empty() {
        let _ = write!(out, " by {}", s.by.join(", "));
    }
}

fn format_rename(s: &crate::ast::RenameStage, out: &mut String) {
    out.push_str("rename ");
    for (i, (old, new)) in s.renames.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        let _ = write!(out, "{old} as {new}");
    }
}

fn format_stats(keyword: &str, aggs: &[AggExpr], group_by: &[String], out: &mut String) {
    out.push_str(keyword);
    out.push(' ');
    format_agg_list(aggs, out);
    if !group_by.is_empty() {
        let _ = write!(out, " by {}", group_by.join(", "));
    }
}

fn format_agg_list(aggs: &[AggExpr], out: &mut String) {
    for (i, agg) in aggs.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        format_agg_expr(agg, out);
    }
}

fn format_agg_expr(agg: &AggExpr, out: &mut String) {
    out.push_str(&agg.function);
    out.push('(');
    for (i, arg) in agg.args.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        format_expr(&arg.node, 0, ExprContext::Normal, out);
    }
    out.push(')');
    if let Some(ref alias) = agg.alias {
        let _ = write!(out, " as {alias}");
    }
}

fn format_extract(s: &crate::ast::ExtractStage, out: &mut String) {
    out.push_str(s.keyword);
    match &s.mode {
        ExtractMode::Regex(pattern) => {
            let _ = write!(out, " \"{pattern}\"");
        }
        ExtractMode::KeyValue { separator } => {
            out.push_str(" kv");
            if *separator != '=' {
                let _ = write!(out, " sep=\"{separator}\"");
            }
        }
    }
    if let Some(ref field) = s.source_field {
        let _ = write!(out, " from {field}");
    }
}

fn format_from_saved(s: &FromSavedStage, out: &mut String) {
    // Name needs quoting if it contains spaces or special chars.
    if s.name.contains(' ') || s.name.contains('"') {
        let escaped = s.name.replace('"', "\\\"");
        let _ = write!(out, "from saved \"{escaped}\"");
    } else {
        let _ = write!(out, "from saved {}", s.name);
    }
    match s.run {
        SavedRunSelector::Latest => {} // default, don't emit
        SavedRunSelector::All => out.push_str(" run=all"),
        SavedRunSelector::Specific(id) => {
            let _ = write!(out, " run={id}");
        }
    }
}

// ---------------------------------------------------------------------------
// Expressions
// ---------------------------------------------------------------------------

/// Context for expression formatting — controls how certain literals render.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ExprContext {
    Normal,
    /// RHS of a `matches` operator — string literals render as `/regex/`.
    MatchesRhs,
}

/// Operator precedence (higher = binds tighter).
fn precedence(op: BinaryOp) -> u8 {
    match op {
        BinaryOp::Or => 1,
        BinaryOp::And => 2,
        BinaryOp::Eq
        | BinaryOp::Ne
        | BinaryOp::Gt
        | BinaryOp::Gte
        | BinaryOp::Lt
        | BinaryOp::Lte
        | BinaryOp::Matches
        | BinaryOp::Like
        | BinaryOp::ILike => 4,
        BinaryOp::Add | BinaryOp::Sub => 5,
        BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => 6,
    }
}

fn format_expr(expr: &Expr, parent_prec: u8, ctx: ExprContext, out: &mut String) {
    match expr {
        Expr::Binary { lhs, op, rhs } => {
            let prec = precedence(*op);
            let needs_parens = prec < parent_prec;
            if needs_parens {
                out.push('(');
            }
            format_expr(&lhs.node, prec, ExprContext::Normal, out);
            let _ = write!(out, " {op} ");
            // RHS context: if this is `matches`, the RHS is a regex literal
            let rhs_ctx = if *op == BinaryOp::Matches {
                ExprContext::MatchesRhs
            } else {
                ExprContext::Normal
            };
            // +1 for left-associativity: equal-precedence RHS needs parens
            format_expr(&rhs.node, prec + 1, rhs_ctx, out);
            if needs_parens {
                out.push(')');
            }
        }
        Expr::Unary { op, operand } => {
            let op_prec = match op {
                UnaryOp::Not => 3,
                UnaryOp::Neg => 7,
            };
            let needs_parens = op_prec < parent_prec;
            if needs_parens {
                out.push('(');
            }
            match op {
                UnaryOp::Not => {
                    out.push_str("not ");
                    format_expr(&operand.node, op_prec, ExprContext::Normal, out);
                }
                UnaryOp::Neg => {
                    out.push('-');
                    format_expr(&operand.node, op_prec, ExprContext::Normal, out);
                }
            }
            if needs_parens {
                out.push(')');
            }
        }
        Expr::FunctionCall { name, args } => {
            out.push_str(name);
            out.push('(');
            for (i, arg) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                format_expr(&arg.node, 0, ExprContext::Normal, out);
            }
            out.push(')');
        }
        Expr::FieldRef(name) => out.push_str(name),
        Expr::Literal(lit) => format_literal(lit, ctx, out),
        Expr::InList { expr, list } => {
            format_expr(&expr.node, 4, ExprContext::Normal, out);
            out.push_str(" in (");
            for (i, item) in list.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                format_expr(&item.node, 0, ExprContext::Normal, out);
            }
            out.push(')');
        }
    }
}

fn format_literal(lit: &LiteralValue, ctx: ExprContext, out: &mut String) {
    match lit {
        LiteralValue::String(s) => {
            if ctx == ExprContext::MatchesRhs {
                let _ = write!(out, "/{s}/");
            } else {
                let _ = write!(out, "\"{s}\"");
            }
        }
        LiteralValue::Int(n) => {
            let _ = write!(out, "{n}");
        }
        LiteralValue::Float(n) => {
            let _ = write!(out, "{n}");
        }
        LiteralValue::Bool(b) => {
            let _ = write!(out, "{b}");
        }
        LiteralValue::Null => out.push_str("null"),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse → format helper.
    fn fmt(input: &str) -> String {
        let query = crate::parser::parse(input).unwrap();
        format_query(&query)
    }

    // ── search stage ──────────────────────────────────────────────

    #[test]
    fn bare_star() {
        insta::assert_snapshot!(fmt("*"), @"*");
    }

    #[test]
    fn single_field_filter() {
        insta::assert_snapshot!(fmt("service=nginx"), @"service=nginx");
    }

    #[test]
    fn multiple_field_filters() {
        insta::assert_snapshot!(fmt("service=nginx   _severity=error   status>=400"), @"service=nginx _severity=error status>=400");
    }

    #[test]
    fn field_filter_with_list() {
        insta::assert_snapshot!(fmt("status=200,301,404"), @"status=200,301,404");
    }

    #[test]
    fn field_filter_with_glob() {
        insta::assert_snapshot!(fmt("path=/api/*"), @"path=/api/*");
    }

    #[test]
    fn field_filter_with_regex() {
        insta::assert_snapshot!(fmt("message=/error.*/"), @"message=/error.*/");
    }

    #[test]
    fn quoted_value_with_space() {
        insta::assert_snapshot!(fmt(r#"service="Activity Monitor""#), @r#"service="Activity Monitor""#);
    }

    #[test]
    fn text_search() {
        insta::assert_snapshot!(fmt("error"), @"error");
    }

    #[test]
    fn negated_text_search() {
        insta::assert_snapshot!(fmt("-debug"), @"-debug");
    }

    #[test]
    fn quoted_phrase() {
        insta::assert_snapshot!(fmt(r#""connection refused""#), @r#""connection refused""#);
    }

    #[test]
    fn time_filter_at_end() {
        insta::assert_snapshot!(fmt("service=nginx last=2h _severity=error"), @"service=nginx _severity=error last=2h");
    }

    #[test]
    fn time_filter_only() {
        insta::assert_snapshot!(fmt("last=7d"), @"last=7d");
    }

    #[test]
    fn or_groups_inline() {
        insta::assert_snapshot!(fmt("service=nginx OR service=apache"), @"service=nginx OR service=apache");
    }

    #[test]
    fn or_groups_with_time_filter() {
        insta::assert_snapshot!(fmt("service=nginx last=2h OR service=apache"), @"service=nginx OR service=apache last=2h");
    }

    #[test]
    fn earliest_latest() {
        insta::assert_snapshot!(
            fmt(r#"service=nginx earliest="2026-01-01" latest="2026-01-02""#),
            @r#"service=nginx earliest="2026-01-01" latest="2026-01-02""#
        );
    }

    #[test]
    fn not_token() {
        insta::assert_snapshot!(fmt("NOT service=nginx"), @"NOT service=nginx");
    }

    // ── pipe stages ───────────────────────────────────────────────

    #[test]
    fn full_pipeline() {
        insta::assert_snapshot!(
            fmt("service=nginx   _severity=error   last=2h  |stats count() by host|where count>10|sort -count|head 20"),
            @r"
        service=nginx _severity=error last=2h
        | stats count() by host
        | where count > 10
        | sort -count
        | head 20
        "
        );
    }

    #[test]
    fn stats_with_alias() {
        insta::assert_snapshot!(
            fmt("* | stats count() as total, avg(duration) as avg_dur by service"),
            @r"
        *
        | stats count() as total, avg(duration) as avg_dur by service
        "
        );
    }

    #[test]
    fn eventstats() {
        insta::assert_snapshot!(
            fmt("* | eventstats avg(duration) by service"),
            @r"
        *
        | eventstats avg(duration) by service
        "
        );
    }

    #[test]
    fn where_complex_expr() {
        insta::assert_snapshot!(
            fmt("* | where status >= 400 and count > 10 or host matches /prod-.*/"),
            @r"
        *
        | where status >= 400 and count > 10 or host matches /prod-.*/
        "
        );
    }

    #[test]
    fn sort_multi_field() {
        insta::assert_snapshot!(
            fmt("* | sort status, -count"),
            @r"
        *
        | sort status, -count
        "
        );
    }

    #[test]
    fn limit_and_head_preserve_keyword() {
        insta::assert_snapshot!(fmt("* | limit 20"), @r"
        *
        | limit 20
        ");
        insta::assert_snapshot!(fmt("* | head 20"), @r"
        *
        | head 20
        ");
    }

    #[test]
    fn tail_stage() {
        insta::assert_snapshot!(fmt("* | tail 5"), @r"
        *
        | tail 5
        ");
    }

    #[test]
    fn table_and_fields_preserve_keyword() {
        insta::assert_snapshot!(fmt("* | table host, service"), @r"
        *
        | table host, service
        ");
        insta::assert_snapshot!(fmt("* | fields host, service"), @r"
        *
        | fields host, service
        ");
    }

    #[test]
    fn top_with_by() {
        insta::assert_snapshot!(fmt("* | top 10 host by service"), @r"
        *
        | top 10 host by service
        ");
    }

    #[test]
    fn rare_basic() {
        insta::assert_snapshot!(fmt("* | rare 5 status"), @r"
        *
        | rare 5 status
        ");
    }

    #[test]
    fn drop_fields() {
        insta::assert_snapshot!(fmt("* | drop message, raw"), @r"
        *
        | drop message, raw
        ");
    }

    #[test]
    fn let_and_eval_preserve_keyword() {
        insta::assert_snapshot!(fmt("* | let duration_ms = duration * 1000"), @r"
        *
        | let duration_ms = duration * 1000
        ");
        insta::assert_snapshot!(fmt("* | eval status_class = status / 100"), @r"
        *
        | eval status_class = status / 100
        ");
    }

    #[test]
    fn extract_regex() {
        insta::assert_snapshot!(
            fmt(r#"* | extract "(?P<ip>\d+\.\d+\.\d+\.\d+)" from message"#),
            @r#"
        *
        | extract "(?P<ip>\d+\.\d+\.\d+\.\d+)" from message
        "#
        );
    }

    #[test]
    fn rex_preserves_keyword() {
        insta::assert_snapshot!(
            fmt(r#"* | rex "(?P<code>[A-Z]+)" from raw"#),
            @r#"
        *
        | rex "(?P<code>[A-Z]+)" from raw
        "#
        );
    }

    #[test]
    fn extract_kv() {
        insta::assert_snapshot!(fmt("* | extract kv"), @r"
        *
        | extract kv
        ");
    }

    #[test]
    fn extract_kv_with_sep() {
        insta::assert_snapshot!(fmt(r#"* | extract kv sep=":""#), @r#"
        *
        | extract kv sep=":"
        "#);
    }

    #[test]
    fn extract_kv_from_field() {
        insta::assert_snapshot!(fmt("* | extract kv from raw"), @r"
        *
        | extract kv from raw
        ");
    }

    #[test]
    fn dedup_bare() {
        insta::assert_snapshot!(fmt("* | dedup"), @r"
        *
        | dedup
        ");
    }

    #[test]
    fn dedup_with_fields() {
        insta::assert_snapshot!(fmt("* | dedup host, service"), @r"
        *
        | dedup host, service
        ");
    }

    #[test]
    fn timechart() {
        insta::assert_snapshot!(
            fmt("* | timechart span=5m count() by service"),
            @r"
        *
        | timechart span=5m count() by service
        "
        );
    }

    #[test]
    fn pivot() {
        insta::assert_snapshot!(
            fmt("* | pivot count() on status by host"),
            @r"
        *
        | pivot count() on status by host
        "
        );
    }

    #[test]
    fn rename() {
        insta::assert_snapshot!(
            fmt("* | rename service as svc, host as hostname"),
            @r"
        *
        | rename service as svc, host as hostname
        "
        );
    }

    #[test]
    fn sample_percent() {
        insta::assert_snapshot!(fmt("* | sample 10%"), @r"
        *
        | sample 10%
        ");
    }

    #[test]
    fn sample_count() {
        insta::assert_snapshot!(fmt("* | sample 1000"), @r"
        *
        | sample 1000
        ");
    }

    #[test]
    fn from_saved() {
        insta::assert_snapshot!(
            fmt("| from saved daily_report"),
            @"| from saved daily_report"
        );
    }

    #[test]
    fn from_saved_with_run() {
        insta::assert_snapshot!(
            fmt("| from saved daily_report run=all"),
            @"| from saved daily_report run=all"
        );
    }

    // ── expressions ───────────────────────────────────────────────

    #[test]
    fn expr_minimal_parens() {
        // a + b * c should NOT have parens (mul binds tighter)
        insta::assert_snapshot!(
            fmt("* | let x = a + b * c"),
            @r"
        *
        | let x = a + b * c
        "
        );
    }

    #[test]
    fn expr_needed_parens() {
        // (a + b) * c NEEDS parens
        insta::assert_snapshot!(
            fmt("* | let x = (a + b) * c"),
            @r"
        *
        | let x = (a + b) * c
        "
        );
    }

    #[test]
    fn expr_in_list() {
        insta::assert_snapshot!(
            fmt("* | where x in (1, 2, 3)"),
            @r"
        *
        | where x in (1, 2, 3)
        "
        );
    }

    #[test]
    fn expr_function_call() {
        insta::assert_snapshot!(
            fmt("* | let msg_len = if(isnotnull(message), length(message), 0)"),
            @r"
        *
        | let msg_len = if(isnotnull(message), length(message), 0)
        "
        );
    }

    #[test]
    fn expr_unary_not() {
        insta::assert_snapshot!(
            fmt("* | where not status == 200"),
            @r"
        *
        | where not status == 200
        "
        );
    }

    #[test]
    fn expr_unary_neg() {
        insta::assert_snapshot!(
            fmt("* | let x = -count"),
            @r"
        *
        | let x = -count
        "
        );
    }

    // ── round-trip / idempotency ──────────────────────────────────

    #[test]
    fn idempotent() {
        let input = "service=nginx   _severity=error last=2h  |stats count() by host|where count>10|sort -count|head 20";
        let first = fmt(input);
        let second = fmt(&first);
        assert_eq!(first, second, "formatter is not idempotent");
    }

    #[test]
    fn reformat_invalid_returns_none() {
        assert!(reformat("| bad_stage_that_doesnt_exist").is_none());
    }

    #[test]
    fn reformat_valid() {
        let result = reformat("service=nginx  |  head 10");
        assert_eq!(result.as_deref(), Some("service=nginx\n| head 10"));
    }

    // ── empty / edge cases ────────────────────────────────────────

    #[test]
    fn pipe_only_no_search() {
        insta::assert_snapshot!(fmt("| head 10"), @"| head 10");
    }

    #[test]
    fn multiple_let_assignments() {
        insta::assert_snapshot!(
            fmt("* | let a = lower(service), b = length(service)"),
            @r"
        *
        | let a = lower(service), b = length(service)
        "
        );
    }
}
