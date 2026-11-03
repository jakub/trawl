// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The canonical form of a parsed query (ADR-0061): what two parses must
//! share to mean the same thing.
//!
//! The canonical form is the AST with every span and the search stage's
//! `hoisted` list left out. Both record where something sat in the
//! source, not what the query asks. Nothing else is normalised: token
//! order, group nesting, scalar kind, operator, list order, float source
//! text, alias keywords, `Int` against `Neg`, and `TextSearch` against
//! `QuotedSearch` all count.
//!
//! [`same_query`] is the one owner of that rule. It compares two trees in
//! lockstep rather than building a span-free copy, and it walks with an
//! explicit stack: `MAX_EXPR_DEPTH` bounds nesting, not tree depth, so
//! `1+1+…` folds to a left-leaning tree as deep as the chain is long, and
//! derived `PartialEq` on it would recurse once per term.
//!
//! Every AST struct is destructured in full and every enum matched
//! without a wildcard arm, so a new field or variant fails to compile
//! here until someone decides whether it counts. Leaf types that hold no
//! span and no subtree are compared with their derived `PartialEq`.

use crate::ast::{
    AggExpr, DedupStage, DropStage, EventStatsStage, Expr, ExtractStage, FieldFilter,
    FromSavedStage, LetStage, LimitStage, LiteralValue, PipeStage, PivotStage, Query, QuotedSearch,
    RareStage, RenameStage, SampleStage, SearchStage, SearchToken, SortField, SortStage, Spanned,
    StatsStage, TableStage, TailStage, TextSearch, TimeFilter, TimechartStage, TopStage,
    WhereStage,
};

/// Whether two queries have the same canonical form: equal in everything
/// but spans and `SearchStage.hoisted`.
#[must_use]
pub fn same_query(a: &Query, b: &Query) -> bool {
    let mut walk = Walk::default();
    walk.query(a, b) && walk.drain()
}

/// One pair of subtrees the walk has still to compare.
enum Pair<'q> {
    Token(&'q SearchToken, &'q SearchToken),
    Expr(&'q Expr, &'q Expr),
}

/// The lockstep comparison. Shallow fields are compared as they are met;
/// subtrees are queued on `pending` and compared by [`Walk::drain`].
#[derive(Default)]
struct Walk<'q> {
    pending: Vec<Pair<'q>>,
}

/// The node of a spanned value, its span ignored.
fn node<T>(spanned: &Spanned<T>) -> &T {
    let Spanned { node, span: _ } = spanned;
    node
}

/// Two optional spanned values, their spans ignored.
fn same_optional<T>(
    a: Option<&Spanned<T>>,
    b: Option<&Spanned<T>>,
    same: impl Fn(&T, &T) -> bool,
) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => same(node(a), node(b)),
        (None, Some(_)) | (Some(_), None) => false,
    }
}

fn same_time_filter(a: &TimeFilter, b: &TimeFilter) -> bool {
    let TimeFilter { duration: da } = a;
    let TimeFilter { duration: db } = b;
    da == db
}

fn same_field_filter(a: &FieldFilter, b: &FieldFilter) -> bool {
    let FieldFilter {
        field: fa,
        op: oa,
        value: va,
    } = a;
    let FieldFilter {
        field: fb,
        op: ob,
        value: vb,
    } = b;
    fa == fb && oa == ob && va == vb
}

/// Two literals. A float compares its source token and the bits of its
/// value, so `1.0` and `1.00` differ, and a value equals itself even when
/// it is NaN.
fn same_literal(a: &LiteralValue, b: &LiteralValue) -> bool {
    match a {
        LiteralValue::String(sa) => matches!(b, LiteralValue::String(sb) if sa == sb),
        LiteralValue::Int(na) => matches!(b, LiteralValue::Int(nb) if na == nb),
        LiteralValue::Float(fa) => matches!(
            b,
            LiteralValue::Float(fb)
                if fa.text() == fb.text() && fa.value().to_bits() == fb.value().to_bits()
        ),
        LiteralValue::Bool(ba) => matches!(b, LiteralValue::Bool(bb) if ba == bb),
        LiteralValue::Null => matches!(b, LiteralValue::Null),
    }
}

fn same_sort_field(a: &SortField, b: &SortField) -> bool {
    let SortField {
        field: fa,
        direction: da,
    } = a;
    let SortField {
        field: fb,
        direction: db,
    } = b;
    fa == fb && da == db
}

impl<'q> Walk<'q> {
    fn query(&mut self, a: &'q Query, b: &'q Query) -> bool {
        let Query {
            search: sa,
            pipeline: pa,
        } = a;
        let Query {
            search: sb,
            pipeline: pb,
        } = b;
        self.search(sa, sb)
            && pa.len() == pb.len()
            && pa.iter().zip(pb).all(|(a, b)| self.stage(node(a), node(b)))
    }

    fn search(&mut self, a: &'q SearchStage, b: &'q SearchStage) -> bool {
        let SearchStage {
            groups: ga,
            time_filter: ta,
            earliest: ea,
            latest: la,
            hoisted: _,
        } = a;
        let SearchStage {
            groups: gb,
            time_filter: tb,
            earliest: eb,
            latest: lb,
            hoisted: _,
        } = b;
        same_optional(ta.as_ref(), tb.as_ref(), same_time_filter)
            && same_optional(ea.as_ref(), eb.as_ref(), String::eq)
            && same_optional(la.as_ref(), lb.as_ref(), String::eq)
            && self.groups(ga, gb)
    }

    /// OR-separated groups of AND-joined tokens: same shape, then each
    /// token pair queued.
    fn groups(
        &mut self,
        a: &'q [Vec<Spanned<SearchToken>>],
        b: &'q [Vec<Spanned<SearchToken>>],
    ) -> bool {
        if a.len() != b.len() || a.iter().zip(b).any(|(ga, gb)| ga.len() != gb.len()) {
            return false;
        }
        for (ta, tb) in a.iter().flatten().zip(b.iter().flatten()) {
            self.pending.push(Pair::Token(node(ta), node(tb)));
        }
        true
    }

    /// Two expression lists: same length, then each pair queued.
    fn exprs(&mut self, a: &'q [Spanned<Expr>], b: &'q [Spanned<Expr>]) -> bool {
        if a.len() != b.len() {
            return false;
        }
        for (ea, eb) in a.iter().zip(b) {
            self.pending.push(Pair::Expr(node(ea), node(eb)));
        }
        true
    }

    fn aggs(&mut self, a: &'q [AggExpr], b: &'q [AggExpr]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(a, b)| self.agg(a, b))
    }

    fn agg(&mut self, a: &'q AggExpr, b: &'q AggExpr) -> bool {
        let AggExpr {
            function: fa,
            args: xa,
            alias: aa,
        } = a;
        let AggExpr {
            function: fb,
            args: xb,
            alias: ab,
        } = b;
        fa == fb && aa == ab && self.exprs(xa, xb)
    }

    // one arm per stage, each destructured in full on both sides
    #[allow(clippy::too_many_lines)]
    fn stage(&mut self, a: &'q PipeStage, b: &'q PipeStage) -> bool {
        match a {
            PipeStage::Stats(StatsStage {
                aggregations: aa,
                group_by: ga,
            }) => {
                let PipeStage::Stats(StatsStage {
                    aggregations: ab,
                    group_by: gb,
                }) = b
                else {
                    return false;
                };
                ga == gb && self.aggs(aa, ab)
            }
            PipeStage::Where(WhereStage { condition: ca }) => {
                let PipeStage::Where(WhereStage { condition: cb }) = b else {
                    return false;
                };
                self.pending.push(Pair::Expr(node(ca), node(cb)));
                true
            }
            PipeStage::Sort(SortStage { fields: fa }) => {
                let PipeStage::Sort(SortStage { fields: fb }) = b else {
                    return false;
                };
                fa.len() == fb.len() && fa.iter().zip(fb).all(|(a, b)| same_sort_field(a, b))
            }
            PipeStage::Limit(LimitStage {
                count: ca,
                keyword: ka,
            }) => {
                let PipeStage::Limit(LimitStage {
                    count: cb,
                    keyword: kb,
                }) = b
                else {
                    return false;
                };
                ca == cb && ka == kb
            }
            PipeStage::Table(TableStage {
                fields: fa,
                keyword: ka,
            }) => {
                let PipeStage::Table(TableStage {
                    fields: fb,
                    keyword: kb,
                }) = b
                else {
                    return false;
                };
                fa == fb && ka == kb
            }
            PipeStage::Top(TopStage {
                count: ca,
                field: fa,
                by: ba,
            }) => {
                let PipeStage::Top(TopStage {
                    count: cb,
                    field: fb,
                    by: bb,
                }) = b
                else {
                    return false;
                };
                ca == cb && fa == fb && ba == bb
            }
            PipeStage::Rare(RareStage {
                count: ca,
                field: fa,
                by: ba,
            }) => {
                let PipeStage::Rare(RareStage {
                    count: cb,
                    field: fb,
                    by: bb,
                }) = b
                else {
                    return false;
                };
                ca == cb && fa == fb && ba == bb
            }
            PipeStage::Drop(DropStage { fields: fa }) => {
                let PipeStage::Drop(DropStage { fields: fb }) = b else {
                    return false;
                };
                fa == fb
            }
            PipeStage::Let(LetStage {
                assignments: aa,
                keyword: ka,
            }) => {
                let PipeStage::Let(LetStage {
                    assignments: ab,
                    keyword: kb,
                }) = b
                else {
                    return false;
                };
                if ka != kb || aa.len() != ab.len() {
                    return false;
                }
                for ((na, ea), (nb, eb)) in aa.iter().zip(ab) {
                    if na != nb {
                        return false;
                    }
                    self.pending.push(Pair::Expr(node(ea), node(eb)));
                }
                true
            }
            PipeStage::Extract(ExtractStage {
                mode: ma,
                source_field: sa,
                keyword: ka,
            }) => {
                let PipeStage::Extract(ExtractStage {
                    mode: mb,
                    source_field: sb,
                    keyword: kb,
                }) = b
                else {
                    return false;
                };
                ma == mb && sa == sb && ka == kb
            }
            PipeStage::Dedup(DedupStage { fields: fa }) => {
                let PipeStage::Dedup(DedupStage { fields: fb }) = b else {
                    return false;
                };
                fa == fb
            }
            PipeStage::Timechart(TimechartStage {
                on: oa,
                span: sa,
                aggregations: aa,
                group_by: ga,
            }) => {
                let PipeStage::Timechart(TimechartStage {
                    on: ob,
                    span: sb,
                    aggregations: ab,
                    group_by: gb,
                }) = b
                else {
                    return false;
                };
                oa == ob && sa == sb && ga == gb && self.aggs(aa, ab)
            }
            PipeStage::Pivot(PivotStage {
                aggregation: aa,
                on_field: oa,
                by: ba,
            }) => {
                let PipeStage::Pivot(PivotStage {
                    aggregation: ab,
                    on_field: ob,
                    by: bb,
                }) = b
                else {
                    return false;
                };
                oa == ob && ba == bb && self.agg(aa, ab)
            }
            PipeStage::Tail(TailStage { count: ca }) => {
                let PipeStage::Tail(TailStage { count: cb }) = b else {
                    return false;
                };
                ca == cb
            }
            PipeStage::Rename(RenameStage { renames: ra }) => {
                let PipeStage::Rename(RenameStage { renames: rb }) = b else {
                    return false;
                };
                ra == rb
            }
            PipeStage::Sample(SampleStage { mode: ma }) => {
                let PipeStage::Sample(SampleStage { mode: mb }) = b else {
                    return false;
                };
                ma == mb
            }
            PipeStage::EventStats(EventStatsStage {
                aggregations: aa,
                group_by: ga,
            }) => {
                let PipeStage::EventStats(EventStatsStage {
                    aggregations: ab,
                    group_by: gb,
                }) = b
                else {
                    return false;
                };
                ga == gb && self.aggs(aa, ab)
            }
            PipeStage::FromSaved(FromSavedStage { name: na, run: ra }) => {
                let PipeStage::FromSaved(FromSavedStage { name: nb, run: rb }) = b else {
                    return false;
                };
                na == nb && ra == rb
            }
        }
    }

    /// Compare every queued pair, stopping at the first difference.
    fn drain(&mut self) -> bool {
        while let Some(pair) = self.pending.pop() {
            let same = match pair {
                Pair::Token(a, b) => self.token(a, b),
                Pair::Expr(a, b) => self.expr(a, b),
            };
            if !same {
                return false;
            }
        }
        true
    }

    fn token(&mut self, a: &'q SearchToken, b: &'q SearchToken) -> bool {
        match a {
            SearchToken::FieldFilter(fa) => {
                matches!(b, SearchToken::FieldFilter(fb) if same_field_filter(fa, fb))
            }
            SearchToken::TextSearch(TextSearch {
                term: ta,
                negated: na,
            }) => matches!(
                b,
                SearchToken::TextSearch(TextSearch { term: tb, negated: nb })
                    if ta == tb && na == nb
            ),
            SearchToken::TimeFilter(fa) => {
                matches!(b, SearchToken::TimeFilter(fb) if same_time_filter(fa, fb))
            }
            SearchToken::QuotedSearch(QuotedSearch { phrase: pa }) => matches!(
                b,
                SearchToken::QuotedSearch(QuotedSearch { phrase: pb }) if pa == pb
            ),
            SearchToken::EarliestFilter(va) => {
                matches!(b, SearchToken::EarliestFilter(vb) if va == vb)
            }
            SearchToken::LatestFilter(va) => {
                matches!(b, SearchToken::LatestFilter(vb) if va == vb)
            }
            SearchToken::Not(ia) => {
                let SearchToken::Not(ib) = b else {
                    return false;
                };
                self.pending.push(Pair::Token(node(ia), node(ib)));
                true
            }
            SearchToken::Group(ga) => {
                let SearchToken::Group(gb) = b else {
                    return false;
                };
                self.groups(ga, gb)
            }
        }
    }

    fn expr(&mut self, a: &'q Expr, b: &'q Expr) -> bool {
        match a {
            Expr::Binary {
                lhs: la,
                op: oa,
                rhs: ra,
            } => {
                let Expr::Binary {
                    lhs: lb,
                    op: ob,
                    rhs: rb,
                } = b
                else {
                    return false;
                };
                if oa != ob {
                    return false;
                }
                self.pending.push(Pair::Expr(node(ra), node(rb)));
                self.pending.push(Pair::Expr(node(la), node(lb)));
                true
            }
            Expr::Unary {
                op: oa,
                operand: xa,
            } => {
                let Expr::Unary {
                    op: ob,
                    operand: xb,
                } = b
                else {
                    return false;
                };
                if oa != ob {
                    return false;
                }
                self.pending.push(Pair::Expr(node(xa), node(xb)));
                true
            }
            Expr::FunctionCall { name: na, args: xa } => {
                let Expr::FunctionCall { name: nb, args: xb } = b else {
                    return false;
                };
                na == nb && self.exprs(xa, xb)
            }
            Expr::FieldRef(fa) => matches!(b, Expr::FieldRef(fb) if fa == fb),
            Expr::Literal(va) => matches!(b, Expr::Literal(vb) if same_literal(va, vb)),
            Expr::InList { expr: ea, list: la } => {
                let Expr::InList { expr: eb, list: lb } = b else {
                    return false;
                };
                if !self.exprs(la, lb) {
                    return false;
                }
                self.pending.push(Pair::Expr(node(ea), node(eb)));
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::same_query;
    use crate::ast::Query;
    use crate::parser::parse;

    fn parsed(src: &str) -> Query {
        parse(src).unwrap_or_else(|e| panic!("{src:?} must parse: {e:?}"))
    }

    fn assert_same(a: &str, b: &str) {
        assert!(
            same_query(&parsed(a), &parsed(b)),
            "{a:?} and {b:?} must compare equal"
        );
    }

    fn assert_differ(a: &str, b: &str) {
        let (qa, qb) = (parsed(a), parsed(b));
        assert!(
            !same_query(&qa, &qb),
            "{a:?} and {b:?} must compare unequal"
        );
        assert!(
            !same_query(&qb, &qa),
            "{b:?} and {a:?} must compare unequal"
        );
        // each side still equals itself, so the inequality is the pair's
        assert!(same_query(&qa, &parsed(a)), "{a:?} must equal its reparse");
        assert!(same_query(&qb, &parsed(b)), "{b:?} must equal its reparse");
    }

    #[test]
    fn a_changed_predicate_differs() {
        assert_differ("a=1", "a=2");
        assert_differ("a=1", "a!=1");
        assert_differ("a=1", "a>=1");
        assert_differ("a=1", "b=1");
        assert_differ("* | where a > 1", "* | where a >= 1");
        assert_differ("* | where a > 1", "* | where a > 2");
        assert_differ("* | where a > 1", "* | where b > 1");
        assert_differ("* | stats count() by a", "* | stats count() by b");
        assert_differ("* | stats count() as n", "* | stats count()");
        assert_differ("* | stats count()", "* | stats sum(a)");
        assert_differ("* | let x = a + 1", "* | let x = a - 1");
        assert_differ("* | let x = a", "* | let y = a");
        assert_differ("* | sort a", "* | sort -a");
        assert_differ("* | where lower(a)", "* | where upper(a)");
    }

    #[test]
    fn a_scalar_kind_differs() {
        // Exact, Glob and Regex carrying the same characters
        assert_differ("a=x*", r#"a="x*""#);
        assert_differ("a=/x/", "a=x");
        assert_differ("a=/x*/", "a=x*");
        assert_differ("a=x*,y", r#"a="x*",y"#);
        // a negative literal against a negated positive one: the parser
        // reads `-1` as `Neg(Int(1))`, and a literal `Int(-1)` only after
        // a space, so `- -1` is `Neg(Int(-1))` and `--1` is
        // `Neg(Neg(Int(1)))`
        assert_differ("* | where a == - -1", "* | where a == --1");
        assert_differ("* | where a == - -1.5", "* | where a == --1.5");
        // an Int against a Float, a string against a number
        assert_differ("* | where a == 1", "* | where a == 1.0");
        assert_differ("* | where a == 1", r#"* | where a == "1""#);
        assert_differ("* | where a == true", r#"* | where a == "true""#);
        assert_differ("* | where a == null", r#"* | where a == "null""#);
        // a field reference against a string literal
        assert_differ("* | where a == b", r#"* | where a == "b""#);
        // a bare word against a quoted phrase
        assert_differ("foo", r#""foo""#);
    }

    #[test]
    fn a_negation_differs() {
        assert_differ("foo", "-foo");
        assert_differ("a=1", "NOT a=1");
        assert_differ("(a b)", "NOT (a b)");
        assert_differ("NOT a=1", "NOT NOT a=1");
        assert_differ("* | where a", "* | where not a");
        assert_differ("* | where a", "* | where -a");
    }

    #[test]
    fn a_time_slot_differs() {
        assert_differ("x last=1h", "x last=2h");
        assert_differ("x last=1h", "x");
        // the unit is kept as written, so equal seconds still differ
        assert_differ("last=60m", "last=1h");
        assert_differ(
            r#"earliest="2026-01-01T00:00:00Z""#,
            r#"latest="2026-01-01T00:00:00Z""#,
        );
        assert_differ(
            r#"earliest="2026-01-01T00:00:00Z""#,
            r#"earliest="2026-01-02T00:00:00Z""#,
        );
        assert_differ(r#"x latest="2026-01-01T00:00:00Z""#, "x");
        // a time clause against a field named like its keyword
        assert_differ("last=1h", "`last`=1h");
    }

    #[test]
    fn stage_order_differs() {
        assert_differ("* | head 1 | tail 1", "* | tail 1 | head 1");
        assert_differ("* | head 1", "* | head 1 | head 1");
        assert_differ("* | where a | where b", "* | where b | where a");
    }

    #[test]
    fn a_float_token_differs() {
        assert_differ("* | where a == 1.0", "* | where a == 1.00");
        assert_differ("* | where a == 1.5", "* | where a == 1.50");
    }

    #[test]
    fn bare_star_differs_from_quoted_star() {
        assert_differ("*", r#""*""#);
        assert_differ("* | head 1", r#""*" | head 1"#);
        assert_differ("a=*", r#"a="*""#);
    }

    #[test]
    fn order_nesting_and_spelling_count() {
        // token order and list order
        assert_differ("a b", "b a");
        assert_differ("a=1,2", "a=2,1");
        assert_differ("* | where a in (1, 2)", "* | where a in (2, 1)");
        assert_differ("* | table a, b", "* | table b, a");
        // group nesting, in the search stage and in expressions
        assert_differ("a b", "(a b)");
        assert_differ("a OR b", "(a OR b)");
        assert_differ("a b OR c", "a (b OR c)");
        assert_differ("* | where a + b * c", "* | where (a + b) * c");
        assert_differ("* | where (a == b) == c", "* | where a == (b == c)");
        // an alias keyword is what the author typed
        assert_differ("* | head 1", "* | limit 1");
        assert_differ("* | table a", "* | fields a");
        assert_differ("* | let x = 1", "* | eval x = 1");
    }

    #[test]
    fn a_repeated_last_equals_the_last() {
        assert_same("last=1h last=2h", "last=2h");
        assert_same("x last=1h last=2h", "x last=2h");
    }

    #[test]
    fn whitespace_comments_and_clause_position_are_ignored() {
        assert_same("a=1 b=2", "a=1   b=2");
        assert_same("a=1 b=2", "  a=1 # first\n b=2 # second\n");
        assert_same(
            "* | where a > 1 | head 2",
            "*\n| where a  >  1 # big\n|  head 2",
        );
        assert_same("* | where (a)", "* | where a");
        // where a time clause sat lives only in spans and `hoisted`
        assert_same("x last=1h", "last=1h x");
        assert_same("x OR y last=1h", "last=1h x OR y");
    }

    #[test]
    fn spans_and_hoisted_are_ignored() {
        let a = parsed("x last=1h y=2 | where a + 1 > 2 | stats count() by y");
        let mut b = a.clone();
        b.search.hoisted.clear();
        for token in b.search.groups.iter_mut().flatten() {
            token.span = 900..901;
        }
        if let Some(t) = b.search.time_filter.as_mut() {
            t.span = 7..70;
        }
        for stage in &mut b.pipeline {
            stage.span = 3..4;
            if let crate::ast::PipeStage::Where(w) = &mut stage.node {
                w.condition.span = 1..2;
            }
        }
        assert!(same_query(&a, &b));
        assert!(same_query(&b, &a));
    }

    /// The comparison walks with an explicit stack, so a flat chain far
    /// deeper than `MAX_EXPR_DEPTH` compares on a tokio worker's 2 MiB
    /// stack in a debug build. `1+1+…` folds to a left-leaning tree as
    /// deep as the chain is long.
    #[test]
    fn a_flat_chain_compares_on_a_worker_stack() {
        const TOKIO_WORKER_STACK: usize = 2 * 1024 * 1024;
        const TERMS: usize = 20_000;

        let chain = vec!["1"; TERMS].join("+");
        let mut changed = vec!["1"; TERMS];
        changed[0] = "2";
        let changed = changed.join("+");
        // the longest OR chain `MAX_QUERY_LEN` admits; the search stage
        // keeps it flat, so it costs width rather than depth
        let ors = vec!["a"; 12_000].join(" OR ");
        let corpus = [
            (
                format!("* | where {chain}"),
                format!("* | where {chain}"),
                true,
            ),
            (
                format!("* | where {chain}"),
                format!("* | where {changed}"),
                false,
            ),
            (
                format!("* | let x = {chain}"),
                format!("* | let x = {changed}"),
                false,
            ),
            (
                format!("* | stats sum({chain})"),
                format!("* | stats sum({chain})"),
                true,
            ),
            (ors.clone(), ors, true),
        ];
        let trees = std::thread::Builder::new()
            .stack_size(TOKIO_WORKER_STACK)
            .spawn(move || {
                corpus
                    .iter()
                    .map(|(a, b, want)| {
                        let (qa, qb) = (parsed(a), parsed(b));
                        assert_eq!(same_query(&qa, &qb), *want, "{a:.40}… vs {b:.40}…");
                        (qa, qb)
                    })
                    .collect::<Vec<_>>()
            })
            .expect("spawn")
            .join()
            .expect("the comparison must not overflow a 2 MiB stack");
        // Dropping a tree this deep recurses in its drop glue, which is
        // not the walk under test, so it gets a stack it fits in.
        std::thread::Builder::new()
            .stack_size(64 * TOKIO_WORKER_STACK)
            .spawn(move || drop(trees))
            .expect("spawn")
            .join()
            .expect("drop");
    }
}
