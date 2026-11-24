// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The [`MAX_TREE_DEPTH`] check (ADR-0024) and the [`MAX_SEARCH_PRESCAN`]
//! count that runs before the grammar.
//!
//! Most later walks of a parsed query recurse once a level, and the
//! grammar builds a flat chain as deep as it is long. This walk is the one
//! that must not: it holds the nodes still to visit in a list on the heap,
//! so a 64 KiB chain costs it heap, not stack. It only reads the tree: it
//! never clones, formats, debug-prints or compares it, because the derived
//! `Clone`, `Debug` and `PartialEq` recurse and the checked formatter
//! parses its text again.

use std::ops::Range;

use super::{MAX_SEARCH_PRESCAN, MAX_TREE_DEPTH, ParseError};
use crate::ast::{
    AggExpr, DedupStage, DropStage, EventStatsStage, Expr, ExtractStage, FromSavedStage, LetStage,
    LimitStage, PipeStage, PivotStage, Query, RareStage, RenameStage, SampleStage, SearchStage,
    SearchToken, SortStage, Spanned, StatsStage, TableStage, TailStage, TimechartStage, TopStage,
    WhereStage,
};

const HINT_EXPRESSION: &str = "each operator, not, minus sign, call and in list is a level: \
     write equality tests on one field as field in (…), and split a long and across several \
     | where stages";

const HINT_SEARCH: &str = "each NOT and each pair of brackets is a level: use fewer nested \
     brackets or negations";

/// Which kind of tree was too deep, which picks the error's words.
#[derive(Debug, Clone, Copy)]
enum Tree {
    Search,
    Expression,
}

pub(super) fn tree_depth_error(query: &Query) -> Option<ParseError> {
    let Query { search, pipeline } = query;
    let SearchStage {
        groups,
        time_filter: _,
        earliest: _,
        latest: _,
        hoisted: _,
    } = search;
    if let Some(token) = groups
        .iter()
        .flatten()
        .find(|token| search_too_deep(&token.node))
    {
        return Some(refusal(Tree::Search, &token.span));
    }
    pipeline
        .iter()
        .find_map(|stage| deep_root(&stage.node))
        .map(|span| refusal(Tree::Expression, &span))
}

fn refusal(tree: Tree, span: &Range<usize>) -> ParseError {
    let (word, hint) = match tree {
        Tree::Search => ("search", HINT_SEARCH),
        Tree::Expression => ("expression", HINT_EXPRESSION),
    };
    ParseError {
        message: format!("{word} is deeper than {MAX_TREE_DEPTH} levels"),
        span: span.clone(),
        label: None,
        hint: Some(hint.to_owned()),
    }
}

/// Every stage and every field is named, with no wildcard and no `..`, so
/// a stage or a field that holds an expression does not compile until it
/// is counted here.
fn deep_root(stage: &PipeStage) -> Option<Range<usize>> {
    match stage {
        PipeStage::Where(WhereStage { condition }) => first_deep([condition]),
        PipeStage::Let(LetStage {
            assignments,
            keyword: _,
        }) => first_deep(assignments.iter().map(|(_, value)| value)),
        PipeStage::Stats(StatsStage {
            aggregations,
            group_by: _,
        })
        | PipeStage::EventStats(EventStatsStage {
            aggregations,
            group_by: _,
        })
        | PipeStage::Timechart(TimechartStage {
            on: _,
            span: _,
            aggregations,
            group_by: _,
        }) => first_deep(aggregations.iter().flat_map(arguments)),
        PipeStage::Pivot(PivotStage {
            aggregation,
            on_field: _,
            by: _,
        }) => first_deep(arguments(aggregation)),
        PipeStage::Sort(SortStage { fields: _ })
        | PipeStage::Limit(LimitStage {
            count: _,
            keyword: _,
        })
        | PipeStage::Table(TableStage {
            fields: _,
            keyword: _,
        })
        | PipeStage::Top(TopStage {
            count: _,
            field: _,
            by: _,
        })
        | PipeStage::Rare(RareStage {
            count: _,
            field: _,
            by: _,
        })
        | PipeStage::Drop(DropStage { fields: _ })
        | PipeStage::Extract(ExtractStage {
            mode: _,
            source_field: _,
            keyword: _,
        })
        | PipeStage::Dedup(DedupStage { fields: _ })
        | PipeStage::Tail(TailStage { count: _ })
        | PipeStage::Rename(RenameStage { renames: _ })
        | PipeStage::Sample(SampleStage { mode: _ })
        | PipeStage::FromSaved(FromSavedStage { name: _, run: _ }) => None,
    }
}

/// An aggregation's arguments, each a root: the aggregation call itself
/// is no expression, so it adds no level.
fn arguments(aggregation: &AggExpr) -> &[Spanned<Expr>] {
    let AggExpr {
        function: _,
        args,
        alias: _,
    } = aggregation;
    args
}

fn first_deep<'q>(roots: impl IntoIterator<Item = &'q Spanned<Expr>>) -> Option<Range<usize>> {
    roots
        .into_iter()
        .find(|root| expr_too_deep(&root.node))
        .map(|root| root.span.clone())
}

fn expr_too_deep(root: &Expr) -> bool {
    let mut pending = vec![(root, 0_usize)];
    while let Some((node, above)) = pending.pop() {
        let level = above + 1;
        match node {
            Expr::Binary { lhs, op: _, rhs } => {
                if level > MAX_TREE_DEPTH {
                    return true;
                }
                pending.extend([(&lhs.node, level), (&rhs.node, level)]);
            }
            Expr::Unary { op: _, operand } => {
                if level > MAX_TREE_DEPTH {
                    return true;
                }
                pending.push((&operand.node, level));
            }
            Expr::FunctionCall { name: _, args } => {
                if level > MAX_TREE_DEPTH {
                    return true;
                }
                pending.extend(args.iter().map(|arg| (&arg.node, level)));
            }
            Expr::InList { expr, list } => {
                if level > MAX_TREE_DEPTH {
                    return true;
                }
                pending.push((&expr.node, level));
                pending.extend(list.iter().map(|item| (&item.node, level)));
            }
            Expr::FieldRef(_) | Expr::Literal(_) => {}
        }
    }
    false
}

fn search_too_deep(root: &SearchToken) -> bool {
    let mut pending = vec![(root, 0_usize)];
    while let Some((token, above)) = pending.pop() {
        let level = above + 1;
        match token {
            SearchToken::Not(inner) => {
                if level > MAX_TREE_DEPTH {
                    return true;
                }
                pending.push((&inner.node, level));
            }
            SearchToken::Group(groups) => {
                if level > MAX_TREE_DEPTH {
                    return true;
                }
                pending.extend(groups.iter().flatten().map(|child| (&child.node, level)));
            }
            SearchToken::FieldFilter(_)
            | SearchToken::TextSearch(_)
            | SearchToken::TimeFilter(_)
            | SearchToken::QuotedSearch(_)
            | SearchToken::EarliestFilter(_)
            | SearchToken::LatestFilter(_) => {}
        }
    }
    false
}

pub(super) fn prescan_error(input: &str) -> Option<ParseError> {
    prescan(input, MAX_SEARCH_PRESCAN)
        .err()
        .map(|at| refusal(Tree::Search, &at))
}

struct Level {
    at: usize,
    bracket: bool,
}

/// An upper bound on how many levels deep the search grammar can recurse
/// in `input`, counted in one pass over its characters, with no lexing and
/// no recursion. It is the most levels open at once, or the span of the
/// bracket or `NOT` at which the count first passes `limit`.
///
/// The grammar opens a level at each `(` that starts a group and at each
/// `NOT` that negates (`search::search_token`), even in an alternative it
/// then backtracks out of. A group's level ends at its `)`, and a `NOT`'s
/// when the token it negates ends. Here every `(` opens a level, and so
/// does every `NOT` that the keyword rule admits, wherever it sits. A
/// level ends only where no reading of the text can keep it open:
///
/// - A `)` ends the newest open `(`, the levels above it and the `NOT`s
///   just before it, unless a `"`, backtick, `/` or `#` came after that
///   `(` or those `NOT`s. A `)` that the search grammar does not read as
///   a group's end is in the pipeline, where no search level is open, or
///   in a quoted string, a backticked name, a regex body or a comment.
///   Each of these starts with one of those four bytes, so such a `)`
///   ends only levels opened inside the same span, and the grammar opened
///   none there.
/// - `NOT`s with only whitespace after each end at the next character
///   that is not `(`, `#` or another `NOT`. The token they negate starts
///   there, and it is a word, a filter or a phrase, which opens no level.
///   A `#` can start a comment between a `NOT` and its token, so those
///   `NOT`s stay open.
///
/// The count can exceed the grammar's depth. Brackets and `NOT`s count in
/// quotes, names, regex bodies, comments and the pipeline, and a group or
/// call that holds one of the four bytes stays open.
pub(super) fn prescan(input: &str, limit: usize) -> Result<usize, Range<usize>> {
    let bytes = input.as_bytes();
    // Oldest first, so the offsets ascend.
    let mut open: Vec<Level> = Vec::new();
    // The index in `open` of each open `(`, oldest first.
    let mut brackets: Vec<usize> = Vec::new();
    // The `NOT`s on top of `open` that only whitespace has followed.
    let mut waiting = 0;
    // The offset of the latest `"`, backtick, `/` or `#`.
    let mut opener: Option<usize> = None;
    let mut peak = 0;
    let mut chars = input.char_indices();
    while let Some((at, c)) = chars.next() {
        if c.is_whitespace() {
            continue;
        }
        let not_word = bytes[at..].starts_with(b"NOT")
            && !bytes
                .get(at + 3)
                .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_');
        if not_word {
            open.push(Level { at, bracket: false });
            waiting += 1;
            if open.len() > limit {
                return Err(at..at + 3);
            }
            peak = peak.max(open.len());
            // past the `O` and the `T`
            chars.nth(1);
            continue;
        }
        if c == '(' || c == '#' {
            waiting = 0;
        } else {
            open.truncate(open.len() - waiting);
            waiting = 0;
        }
        match c {
            '(' => {
                brackets.push(open.len());
                open.push(Level { at, bracket: true });
                if open.len() > limit {
                    return Err(at..at + 1);
                }
                peak = peak.max(open.len());
            }
            ')' => {
                let after_opener = |level: &Level| opener.is_none_or(|o| level.at > o);
                if let Some(&newest) = brackets.last()
                    && after_opener(&open[newest])
                {
                    brackets.pop();
                    open.truncate(newest);
                    while open
                        .last()
                        .is_some_and(|level| !level.bracket && after_opener(level))
                    {
                        open.pop();
                    }
                }
            }
            '"' | '`' | '/' | '#' => opener = Some(at),
            _ => {}
        }
    }
    Ok(peak)
}

#[cfg(test)]
pub(crate) mod fixtures {
    use crate::ast::{BinaryOp, Expr, LiteralValue, PipeStage, Query, Spanned, UnaryOp};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Kind {
        Search,
        Expression,
    }

    #[derive(Debug)]
    pub(crate) struct Shape {
        pub(crate) name: &'static str,
        pub(crate) text: String,
        pub(crate) kind: Kind,
        /// The text a refusal of the shape spans: its deep root as the
        /// tree holds it. A `-` or `not` run spans only its innermost
        /// operand, because the grammar gives each unary node its
        /// operand's span.
        pub(crate) refused: String,
    }

    pub(crate) fn chain(n: usize) -> String {
        vec!["1"; n].join(" + ")
    }

    pub(crate) fn comparisons(n: usize, op: &str) -> String {
        (0..n)
            .map(|i| format!("a == {i}"))
            .collect::<Vec<_>>()
            .join(&format!(" {op} "))
    }

    fn calls(function: &str, n: usize, body: &str) -> String {
        format!(
            "{}{body}{}",
            format!("{function}(").repeat(n),
            ")".repeat(n)
        )
    }

    /// Sixteen of a call shape's levels are nested calls, the most the
    /// grammar's nesting bound admits.
    pub(crate) fn shapes(depth: usize) -> Vec<Shape> {
        assert!(depth > 16, "a call shape spends 16 levels on its calls");
        let d = depth;
        let expression = |name, prefix: &str, root: String, suffix: &str| Shape {
            name,
            text: format!("{prefix}{root}{suffix}"),
            kind: Kind::Expression,
            refused: root,
        };
        let search = |name, text: String| Shape {
            name,
            refused: text.clone(),
            text,
            kind: Kind::Search,
        };
        let not_and_brackets = format!(
            "{}{}a=1{}",
            if d % 2 == 1 { "NOT " } else { "" },
            "NOT (".repeat(d / 2),
            " )".repeat(d / 2)
        );
        vec![
            expression("plus chain", "* | where ", format!("{} > 0", chain(d)), ""),
            expression("or chain", "* | where ", comparisons(d, "or"), ""),
            expression("and chain", "* | where ", comparisons(d, "and"), ""),
            Shape {
                name: "minus run",
                text: format!("* | where {}a < 2", "-".repeat(d - 1)),
                kind: Kind::Expression,
                refused: "a < 2".to_owned(),
            },
            Shape {
                name: "not run",
                text: format!("* | where {}a == 1", "not ".repeat(d - 1)),
                kind: Kind::Expression,
                refused: "a == 1".to_owned(),
            },
            expression(
                "chain as in subject",
                "* | where ",
                format!("{} in (64)", chain(d)),
                "",
            ),
            expression(
                "chain as in element",
                "* | where ",
                format!("a in (1, {})", chain(d)),
                "",
            ),
            expression(
                "chain as call argument",
                "* | let x = ",
                calls("abs", 16, &chain(d - 15)),
                "",
            ),
            expression(
                "chain under pass-through calls",
                "* | let x = ",
                calls("coalesce", 16, &chain(d - 15)),
                "",
            ),
            expression(
                "pass-through calls in where",
                "* | where ",
                format!("{} > 0", calls("coalesce", 16, &chain(d - 16))),
                "",
            ),
            expression("let", "* | let x = ", chain(d + 1), ""),
            expression("stats", "* | stats sum(", chain(d + 1), ")"),
            expression("timechart", "* | timechart span=1m sum(", chain(d + 1), ")"),
            expression("eventstats", "* | eventstats sum(", chain(d + 1), ") as x"),
            expression("pivot", "* | pivot sum(", chain(d + 1), ") on a"),
            expression(
                "where after extract kv",
                "* | extract kv | where ",
                comparisons(d, "or"),
                "",
            ),
            search("search NOT run", format!("{}a=1", "NOT ".repeat(d))),
            search(
                "search brackets",
                format!("{}a=1{}", "(".repeat(d), " )".repeat(d)),
            ),
            search("search NOT and brackets", not_and_brackets),
        ]
    }

    fn at<T>(node: T) -> Spanned<T> {
        Spanned::new(node, 0..0)
    }

    pub(crate) fn int(n: i64) -> Spanned<Expr> {
        at(Expr::Literal(LiteralValue::Int(n)))
    }

    /// The leaves joined by `+` as the grammar folds them, left-deep: one
    /// level fewer than there are leaves.
    pub(crate) fn chain_of(leaves: impl IntoIterator<Item = Spanned<Expr>>) -> Spanned<Expr> {
        let mut leaves = leaves.into_iter();
        let first = leaves.next().expect("a chain has a term");
        leaves.fold(first, |lhs, rhs| {
            at(Expr::Binary {
                lhs: Box::new(lhs),
                op: BinaryOp::Add,
                rhs: Box::new(rhs),
            })
        })
    }

    pub(crate) fn flat_chain(terms: usize) -> Spanned<Expr> {
        chain_of(std::iter::repeat_with(|| int(1)).take(terms))
    }

    pub(crate) fn unary_run(op: UnaryOp, n: usize, operand: Spanned<Expr>) -> Spanned<Expr> {
        (0..n).fold(operand, |operand, _| {
            at(Expr::Unary {
                op,
                operand: Box::new(operand),
            })
        })
    }

    const MARKER: &str = "DEEP";

    /// `template` parsed, with the deep tree in place of the one field
    /// `DEEP` its last stage names.
    pub(crate) fn spliced(template: &str, deep: Spanned<Expr>) -> Query {
        let mut query = crate::parser::parse(template).expect("the template parses");
        let mut deep = Some(deep);
        let stage = query.pipeline.last_mut().expect("the template has a stage");
        let roots: Vec<&mut Spanned<Expr>> = match &mut stage.node {
            PipeStage::Where(stage) => vec![&mut stage.condition],
            PipeStage::Let(stage) => stage.assignments.iter_mut().map(|(_, v)| v).collect(),
            PipeStage::Stats(stage) => stage
                .aggregations
                .iter_mut()
                .flat_map(|a| &mut a.args)
                .collect(),
            other => panic!("a template ends in where, let or stats, not {other}"),
        };
        for root in roots {
            place(root, &mut deep);
        }
        assert!(deep.is_none(), "{template} names {MARKER} once");
        query
    }

    /// Puts `deep` where `expr` names the marker. The template is shallow,
    /// so this walk may recurse.
    fn place(expr: &mut Spanned<Expr>, deep: &mut Option<Spanned<Expr>>) {
        if matches!(&expr.node, Expr::FieldRef(name) if name == MARKER) {
            *expr = deep.take().expect("the template names the marker once");
            return;
        }
        match &mut expr.node {
            Expr::Binary { lhs, op: _, rhs } => {
                place(lhs, deep);
                place(rhs, deep);
            }
            Expr::Unary { op: _, operand } => place(operand, deep),
            Expr::FunctionCall { name: _, args } => {
                for arg in args {
                    place(arg, deep);
                }
            }
            Expr::InList { expr, list } => {
                place(expr, deep);
                for item in list {
                    place(item, deep);
                }
            }
            Expr::FieldRef(_) | Expr::Literal(_) => {}
        }
    }
}
