// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Layer 4: expression parser with operator precedence.

use chumsky::prelude::*;

use std::cell::Cell;
use std::ops::Range;

use crate::ast::{BinaryOp, Expr, LiteralValue, Spanned, UnaryOp};
use crate::parser::comment::Spaced;
use crate::parser::primitives::{
    ParserExtra, ParserInput, check_pattern, field_name, keyword, literal, plain_name,
    quoted_string, regex_pattern, spanned,
};
use crate::pattern::{PatternKind, literal_operand};

/// Every nesting level (a parenthesised sub-expression, a function
/// call's arguments, an `in (…)` list) costs stack, and the input cap
/// (`MAX_QUERY_LEN`) alone does not bound that.
pub(crate) const MAX_EXPR_DEPTH: usize = 16;

thread_local! {
    /// How many expression nesting levels the parse currently sits
    /// inside. A thread-local because chumsky's parsers are shared,
    /// immutable values with no state channel of their own, and one
    /// `parse` call never leaves the thread it started on.
    static DEPTH: Cell<usize> = const { Cell::new(0) };
}

/// Restores the nesting depth however the level is left — a match, a
/// failed alternative chumsky will backtrack out of, or a panic. Without
/// it a backtracked `(` would leak a level and later, legal nesting in
/// the same query would be refused.
struct DepthGuard;

impl Drop for DepthGuard {
    fn drop(&mut self) {
        DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

fn nested<'src, T: 'src>(
    inner: impl Parser<'src, ParserInput<'src>, T, ParserExtra<'src>> + Clone + 'src,
) -> impl Parser<'src, ParserInput<'src>, T, ParserExtra<'src>> + Clone {
    custom(move |inp| {
        let depth = DEPTH.with(Cell::get) + 1;
        if depth > MAX_EXPR_DEPTH {
            let before = inp.cursor();
            return Err(Rich::custom(
                inp.span_since(&before),
                format!("expression nests deeper than {MAX_EXPR_DEPTH} levels"),
            ));
        }
        DEPTH.with(|d| d.set(depth));
        let _guard = DepthGuard;
        inp.parse(inner.clone())
    })
}

#[allow(clippy::too_many_lines)]
pub(crate) fn expr<'src>()
-> impl Parser<'src, ParserInput<'src>, Spanned<Expr>, ParserExtra<'src>> + Clone {
    recursive(|expr| {
        let func_call = spanned(
            plain_name()
                .then_ignore(just('(').spaced())
                .then(nested(
                    expr.clone()
                        .separated_by(just(',').spaced())
                        .collect::<Vec<_>>(),
                ))
                .then_ignore(just(')').spaced())
                .map(|(name, args)| Expr::FunctionCall { name, args }),
        );

        let literal_expr = spanned(literal().map(Expr::Literal));

        let string_literal_expr =
            spanned(quoted_string().map(|s| Expr::Literal(LiteralValue::String(s))));

        let field_ref = spanned(field_name().map(Expr::FieldRef));

        let paren_expr = nested(expr.clone()).delimited_by(just('(').spaced(), just(')').spaced());

        // order matters: func_call before field_ref, literal before field_ref
        // .boxed() here to break up deeply nested generic types that overflow
        // the macOS linker's symbol name length limit
        let atom = choice((
            func_call,
            literal_expr,
            string_literal_expr,
            paren_expr,
            field_ref,
        ))
        .spaced()
        .labelled("expression")
        .boxed();

        let unary_neg = just('-').repeated().foldr(atom, |_op, operand| {
            let span = operand.span.clone();
            Spanned::new(
                Expr::Unary {
                    op: UnaryOp::Neg,
                    operand: Box::new(operand),
                },
                span,
            )
        });

        let mul_op = choice((
            just('*').to(BinaryOp::Mul),
            just('/').to(BinaryOp::Div),
            just('%').to(BinaryOp::Mod),
        ))
        .spaced();

        let multiplicative =
            unary_neg
                .clone()
                .foldl(mul_op.then(unary_neg).repeated(), |lhs, (op, rhs)| {
                    let span = lhs.span.start..rhs.span.end;
                    Spanned::new(
                        Expr::Binary {
                            lhs: Box::new(lhs),
                            op,
                            rhs: Box::new(rhs),
                        },
                        span,
                    )
                });

        let add_op = choice((just('+').to(BinaryOp::Add), just('-').to(BinaryOp::Sub))).spaced();

        let additive = multiplicative.clone().foldl(
            add_op.then(multiplicative).repeated(),
            |lhs, (op, rhs)| {
                let span = lhs.span.start..rhs.span.end;
                Spanned::new(
                    Expr::Binary {
                        lhs: Box::new(lhs),
                        op,
                        rhs: Box::new(rhs),
                    },
                    span,
                )
            },
        );

        let cmp_op = choice((
            just("==").to(BinaryOp::Eq),
            just("!=").to(BinaryOp::Ne),
            just(">=").to(BinaryOp::Gte),
            just(">").to(BinaryOp::Gt),
            just("<=").to(BinaryOp::Lte),
            just("<").to(BinaryOp::Lt),
        ))
        .spaced();

        // `matches` is handled separately so the RHS can accept `/regex/`
        // literals without conflicting with `/` as the division operator
        let regex_literal =
            spanned(regex_pattern().map(|s| Expr::Literal(LiteralValue::String(s))));

        let comparison = additive
            .clone()
            .then(
                choice((
                    keyword("matches")
                        .spaced()
                        .ignore_then(choice((
                            regex_literal,
                            pattern_operand(PatternKind::Regex, additive.clone()),
                        )))
                        .map(|rhs| CmpRhs::Binary(BinaryOp::Matches, rhs)),
                    keyword("ilike")
                        .spaced()
                        .ignore_then(pattern_operand(PatternKind::ILike, additive.clone()))
                        .map(|rhs| CmpRhs::Binary(BinaryOp::ILike, rhs)),
                    keyword("like")
                        .spaced()
                        .ignore_then(pattern_operand(PatternKind::Like, additive.clone()))
                        .map(|rhs| CmpRhs::Binary(BinaryOp::Like, rhs)),
                    cmp_op
                        .then(additive)
                        .map(|(op, rhs)| CmpRhs::Binary(op, rhs)),
                    keyword("in")
                        .spaced()
                        .ignore_then(
                            nested(
                                expr.clone()
                                    .separated_by(just(',').spaced())
                                    .collect::<Vec<_>>(),
                            )
                            .delimited_by(just('(').spaced(), just(')').spaced()),
                        )
                        .map_with(|list, e| {
                            let span = e.span();
                            CmpRhs::InList(list, span.start..span.end)
                        }),
                ))
                .or_not(),
            )
            .map(|(lhs, rhs)| match rhs {
                Some(CmpRhs::Binary(op, rhs)) => {
                    let span = lhs.span.start..rhs.span.end;
                    Spanned::new(
                        Expr::Binary {
                            lhs: Box::new(lhs),
                            op,
                            rhs: Box::new(rhs),
                        },
                        span,
                    )
                }
                Some(CmpRhs::InList(list, in_span)) => {
                    let span = lhs.span.start..in_span.end;
                    Spanned::new(
                        Expr::InList {
                            expr: Box::new(lhs),
                            list,
                        },
                        span,
                    )
                }
                None => lhs,
            })
            .boxed();

        let not_expr = keyword("not").or(keyword("NOT")).spaced().repeated().foldr(
            comparison,
            |_kw, operand| {
                let span = operand.span.clone();
                Spanned::new(
                    Expr::Unary {
                        op: UnaryOp::Not,
                        operand: Box::new(operand),
                    },
                    span,
                )
            },
        );

        let and_expr = not_expr.clone().foldl(
            keyword("and")
                .or(keyword("AND"))
                .spaced()
                .ignore_then(not_expr)
                .repeated(),
            |lhs, rhs| {
                let span = lhs.span.start..rhs.span.end;
                Spanned::new(
                    Expr::Binary {
                        lhs: Box::new(lhs),
                        op: BinaryOp::And,
                        rhs: Box::new(rhs),
                    },
                    span,
                )
            },
        );

        and_expr.clone().foldl(
            keyword("or")
                .or(keyword("OR"))
                .spaced()
                .ignore_then(and_expr)
                .repeated(),
            |lhs, rhs| {
                let span = lhs.span.start..rhs.span.end;
                Spanned::new(
                    Expr::Binary {
                        lhs: Box::new(lhs),
                        op: BinaryOp::Or,
                        rhs: Box::new(rhs),
                    },
                    span,
                )
            },
        )
    })
    .labelled("expression")
}

/// The right side of `matches`, `like` or `ilike` spelled as an
/// expression. It must be literal text ([`literal_operand`], ADR-0063).
///
/// A refusal is emitted over the operand's span, not returned: the
/// comparison still parses, so no other reading of the same text can win
/// with a worse error, and `into_result()` is still `Err`.
fn pattern_operand<'src>(
    kind: PatternKind,
    operand: impl Parser<'src, ParserInput<'src>, Spanned<Expr>, ParserExtra<'src>> + Clone,
) -> impl Parser<'src, ParserInput<'src>, Spanned<Expr>, ParserExtra<'src>> + Clone {
    operand.validate(move |rhs: Spanned<Expr>, extra, emitter| {
        let span = rhs.span.clone().into();
        match literal_operand(&rhs.node) {
            Ok(text) => {
                check_pattern(kind, text, span, extra.state(), emitter);
            }
            Err(error) => emitter.emit(Rich::custom(span, error.to_string())),
        }
        rhs
    })
}

#[derive(Debug)]
enum CmpRhs {
    Binary(BinaryOp, Spanned<Expr>),
    InList(Vec<Spanned<Expr>>, Range<usize>),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_expr(input: &str) -> Spanned<Expr> {
        expr().parse(input).into_result().unwrap()
    }

    #[test]
    fn backticked_name_is_never_a_function_call() {
        let parsed = expr().parse("`lower`(x)").into_result();
        match parsed {
            Err(_) => {} // trailing `(x)` is unconsumed — the loud answer
            Ok(e) => assert_eq!(
                e.node,
                Expr::FieldRef("lower".to_string()),
                "a backticked head must lex as a field, never a call"
            ),
        }
        assert!(crate::parser::parse("| let y = `lower`(x)").is_err());
    }

    #[test]
    fn backticked_field_reference_keeps_its_name() {
        assert_eq!(
            parse_expr("`http-status`").node,
            Expr::FieldRef("http-status".to_string())
        );
    }

    #[test]
    fn test_literal_int() {
        let result = parse_expr("42");
        assert_eq!(result.node, Expr::Literal(LiteralValue::Int(42)));
    }

    #[test]
    fn test_literal_float() {
        let result = parse_expr("3.25");
        assert_eq!(result.node, Expr::Literal(LiteralValue::Float(3.25.into())));
    }

    #[test]
    fn test_literal_string() {
        let result = parse_expr(r#""hello""#);
        assert_eq!(
            result.node,
            Expr::Literal(LiteralValue::String("hello".to_string()))
        );
    }

    #[test]
    fn test_literal_bool() {
        let result = parse_expr("true");
        assert_eq!(result.node, Expr::Literal(LiteralValue::Bool(true)));
    }

    #[test]
    fn test_field_ref() {
        let result = parse_expr("host");
        assert_eq!(result.node, Expr::FieldRef("host".to_string()));
    }

    #[test]
    fn test_dotted_field_ref() {
        let result = parse_expr("host.name");
        assert_eq!(result.node, Expr::FieldRef("host.name".to_string()));
    }

    #[test]
    fn test_function_call_no_args() {
        let result = parse_expr("count()");
        assert_eq!(
            result.node,
            Expr::FunctionCall {
                name: "count".to_string(),
                args: vec![],
            }
        );
    }

    #[test]
    fn test_function_call_with_arg() {
        let result = parse_expr("avg(duration)");
        match &result.node {
            Expr::FunctionCall { name, args } => {
                assert_eq!(name, "avg");
                assert_eq!(args.len(), 1);
                assert_eq!(args[0].node, Expr::FieldRef("duration".to_string()));
            }
            other => panic!("expected FunctionCall, got {other:?}"),
        }
    }

    #[test]
    fn test_binary_comparison() {
        let result = parse_expr("count > 10");
        match &result.node {
            Expr::Binary { lhs, op, rhs } => {
                assert_eq!(lhs.node, Expr::FieldRef("count".to_string()));
                assert_eq!(*op, BinaryOp::Gt);
                assert_eq!(rhs.node, Expr::Literal(LiteralValue::Int(10)));
            }
            other => panic!("expected Binary, got {other:?}"),
        }
    }

    #[test]
    fn test_arithmetic() {
        let result = parse_expr("a + b * c");
        match &result.node {
            Expr::Binary { lhs, op, rhs } => {
                assert_eq!(*op, BinaryOp::Add);
                assert_eq!(lhs.node, Expr::FieldRef("a".to_string()));
                match &rhs.node {
                    Expr::Binary { op: inner_op, .. } => assert_eq!(*inner_op, BinaryOp::Mul),
                    other => panic!("expected inner Binary, got {other:?}"),
                }
            }
            other => panic!("expected Binary, got {other:?}"),
        }
    }

    #[test]
    fn test_logical_and_or() {
        let result = parse_expr("a > 1 and b < 2 or c == 3");
        match &result.node {
            Expr::Binary { op, .. } => assert_eq!(*op, BinaryOp::Or),
            other => panic!("expected Binary Or, got {other:?}"),
        }
    }

    #[test]
    fn test_not() {
        let result = parse_expr("not active");
        match &result.node {
            Expr::Unary { op, operand } => {
                assert_eq!(*op, UnaryOp::Not);
                assert_eq!(operand.node, Expr::FieldRef("active".to_string()));
            }
            other => panic!("expected Unary Not, got {other:?}"),
        }
    }

    #[test]
    fn test_negation() {
        let result = parse_expr("-count");
        match &result.node {
            Expr::Unary { op, operand } => {
                assert_eq!(*op, UnaryOp::Neg);
                assert_eq!(operand.node, Expr::FieldRef("count".to_string()));
            }
            other => panic!("expected Unary Neg, got {other:?}"),
        }
    }

    #[test]
    fn test_parenthesized() {
        let result = parse_expr("(a + b) * c");
        match &result.node {
            Expr::Binary { op, .. } => assert_eq!(*op, BinaryOp::Mul),
            other => panic!("expected Binary Mul, got {other:?}"),
        }
    }

    #[test]
    fn test_in_list_span() {
        let input = "x in (1, 2)";
        let result = parse_expr(input);
        assert_eq!(result.span.start, 0);
        assert_eq!(result.span.end, input.len());
    }

    #[test]
    fn test_keyword_not_in_ident() {
        let result = parse_expr("android");
        assert_eq!(result.node, Expr::FieldRef("android".to_string()));
    }

    #[test]
    fn test_double_minus() {
        let result = parse_expr("a - -b");
        match &result.node {
            Expr::Binary { lhs, op, rhs } => {
                assert_eq!(*op, BinaryOp::Sub);
                assert_eq!(lhs.node, Expr::FieldRef("a".to_string()));
                assert!(matches!(
                    rhs.node,
                    Expr::Unary {
                        op: UnaryOp::Neg,
                        ..
                    }
                ));
            }
            other => panic!("expected Binary Sub, got {other:?}"),
        }
    }

    #[test]
    fn test_in_list_literals() {
        let result = parse_expr("x in (1, 2, 3)");
        match &result.node {
            Expr::InList { expr, list } => {
                assert_eq!(expr.node, Expr::FieldRef("x".to_string()));
                assert_eq!(list.len(), 3);
            }
            other => panic!("expected InList, got {other:?}"),
        }
    }

    #[test]
    fn test_in_list_field_refs() {
        let result = parse_expr("x in (y, z)");
        match &result.node {
            Expr::InList { expr, list } => {
                assert_eq!(expr.node, Expr::FieldRef("x".to_string()));
                assert_eq!(list.len(), 2);
                assert_eq!(list[0].node, Expr::FieldRef("y".to_string()));
                assert_eq!(list[1].node, Expr::FieldRef("z".to_string()));
            }
            other => panic!("expected InList, got {other:?}"),
        }
    }

    #[test]
    fn test_in_list_complex_exprs() {
        let result = parse_expr("x in (count(), y + 2)");
        match &result.node {
            Expr::InList { list, .. } => {
                assert_eq!(list.len(), 2);
                assert!(matches!(list[0].node, Expr::FunctionCall { .. }));
                assert!(matches!(list[1].node, Expr::Binary { .. }));
            }
            other => panic!("expected InList, got {other:?}"),
        }
    }

    #[test]
    fn test_matches_regex_literal() {
        let result = parse_expr("host matches /prod-.*/");
        match &result.node {
            Expr::Binary { lhs, op, rhs } => {
                assert_eq!(lhs.node, Expr::FieldRef("host".to_string()));
                assert_eq!(*op, BinaryOp::Matches);
                assert_eq!(
                    rhs.node,
                    Expr::Literal(LiteralValue::String("prod-.*".to_string()))
                );
            }
            other => panic!("expected Binary Matches, got {other:?}"),
        }
    }

    #[test]
    fn test_matches_string_literal() {
        let result = parse_expr(r#"host matches "pattern""#);
        match &result.node {
            Expr::Binary { lhs, op, rhs } => {
                assert_eq!(lhs.node, Expr::FieldRef("host".to_string()));
                assert_eq!(*op, BinaryOp::Matches);
                assert_eq!(
                    rhs.node,
                    Expr::Literal(LiteralValue::String("pattern".to_string()))
                );
            }
            other => panic!("expected Binary Matches, got {other:?}"),
        }
    }

    #[test]
    fn matches_regex_obeys_the_same_body_rule() {
        let result = parse_expr("host matches /err|warn/");
        match &result.node {
            Expr::Binary { op, rhs, .. } => {
                assert_eq!(*op, BinaryOp::Matches);
                assert_eq!(
                    rhs.node,
                    Expr::Literal(LiteralValue::String("err|warn".to_string()))
                );
            }
            other => panic!("expected Binary Matches, got {other:?}"),
        }
        for (dsl, message) in [
            (
                "* | where m matches /a | b/",
                crate::parser::primitives::MSG_REGEX_PIPE_STAGE,
            ),
            (
                "* | where m matches /a\nb/",
                crate::parser::primitives::MSG_REGEX_LINE_BREAK,
            ),
        ] {
            let errors = crate::parser::parse(dsl).expect_err(dsl);
            assert_eq!(errors.len(), 1, "{dsl:?}: {errors:?}");
            assert_eq!(errors[0].message, message, "{dsl:?}");
            assert_eq!(&dsl[errors[0].span.clone()][..1], "/", "{dsl:?}");
            assert!(dsl[errors[0].span.clone()].ends_with('/'), "{dsl:?}");
        }
    }

    #[test]
    fn test_division_still_works() {
        let result = parse_expr("x / 2 > 0");
        match &result.node {
            Expr::Binary { lhs, op, .. } => {
                assert_eq!(*op, BinaryOp::Gt);
                match &lhs.node {
                    Expr::Binary { op: inner_op, .. } => assert_eq!(*inner_op, BinaryOp::Div),
                    other => panic!("expected inner Binary Div, got {other:?}"),
                }
            }
            other => panic!("expected Binary Gt, got {other:?}"),
        }
    }

    #[test]
    fn test_nested_function_call() {
        let result = parse_expr("func(other_func(x))");
        match &result.node {
            Expr::FunctionCall { name, args } => {
                assert_eq!(name, "func");
                assert_eq!(args.len(), 1);
                match &args[0].node {
                    Expr::FunctionCall {
                        name: inner_name,
                        args: inner_args,
                    } => {
                        assert_eq!(inner_name, "other_func");
                        assert_eq!(inner_args.len(), 1);
                        assert_eq!(inner_args[0].node, Expr::FieldRef("x".to_string()));
                    }
                    other => panic!("expected inner FunctionCall, got {other:?}"),
                }
            }
            other => panic!("expected FunctionCall, got {other:?}"),
        }
    }

    fn stage_expr(query: &str) -> Expr {
        let parsed = crate::parser::parse(query).unwrap_or_else(|e| panic!("{query}: {e:?}"));
        match &parsed.pipeline[0].node {
            crate::ast::PipeStage::Where(w) => w.condition.node.clone(),
            crate::ast::PipeStage::Let(l) => l.assignments[0].1.node.clone(),
            other => panic!("{query}: expected where or let, got {other:?}"),
        }
    }

    fn expression_contexts(expr: &str) -> [String; 3] {
        [
            format!("| where {expr}"),
            format!("| let x = {expr}"),
            format!("| let x = case({expr}, 1, 0)"),
        ]
    }

    #[test]
    fn expression_boolean_case_matrix() {
        let spellings = [
            ("a == 1 OR b == 2", "a == 1 or b == 2"),
            ("a == 1 AND b == 2", "a == 1 and b == 2"),
            ("NOT a == 1", "not a == 1"),
            (
                "NOT a == 1 OR b == 2 AND c == 3",
                "not a == 1 or b == 2 and c == 3",
            ),
            (
                "not a == 1 OR b == 2 and c == 3",
                "not a == 1 or b == 2 and c == 3",
            ),
            ("NOT NOT a == 1", "not not a == 1"),
        ];
        for (spelled, lowercase) in spellings {
            for (query, reference) in expression_contexts(spelled)
                .into_iter()
                .zip(expression_contexts(lowercase))
            {
                let read = crate::parser::parse(&query)
                    .unwrap_or_else(|e| panic!("{query} must parse: {e:?}"));
                let expected = crate::parser::parse(&reference).unwrap();
                assert!(
                    crate::canonical_form::same_query(&read, &expected),
                    "{query} must read as {reference}"
                );
            }
        }

        let root = stage_expr("| where NOT a == 1 OR b == 2 AND c == 3");
        let Expr::Binary {
            lhs,
            op: BinaryOp::Or,
            rhs,
        } = &root
        else {
            panic!("the root must be `or`");
        };
        assert!(
            matches!(
                &lhs.node,
                Expr::Unary { op: UnaryOp::Not, operand }
                    if matches!(&operand.node, Expr::Binary { op: BinaryOp::Eq, lhs, .. }
                        if lhs.node == Expr::FieldRef("a".to_string()))
            ),
            "the left of `or` must be `not (a == 1)`, got {lhs:?}"
        );
        assert!(
            matches!(
                &rhs.node,
                Expr::Binary { op: BinaryOp::And, lhs, rhs }
                    if matches!(&lhs.node, Expr::Binary { op: BinaryOp::Eq, .. })
                        && matches!(&rhs.node, Expr::Binary { op: BinaryOp::Eq, .. })
            ),
            "the right of `or` must be `b == 2 and c == 3`, got {rhs:?}"
        );

        for mixed in ["a == 1 Or b == 2", "a == 1 And b == 2", "Not a == 1"] {
            for query in expression_contexts(mixed) {
                assert!(
                    crate::parser::parse(&query).is_err(),
                    "{query} must stay a parse error"
                );
            }
        }

        for (query, field) in [
            ("| where ORDER == 1", "ORDER"),
            ("| where ANDROID == 1", "ANDROID"),
            ("| where NOTE == 1", "NOTE"),
            ("| where AND.x == 1", "AND.x"),
            ("| where AND == 1", "AND"),
            ("| where OR == 1", "OR"),
        ] {
            let Expr::Binary { ref lhs, .. } = stage_expr(query) else {
                panic!("{query} must be a comparison");
            };
            assert_eq!(lhs.node, Expr::FieldRef(field.to_string()), "{query}");
        }
        let Expr::Binary { ref rhs, .. } = stage_expr("| where a == NOT") else {
            panic!("`a == NOT` must be a comparison");
        };
        assert_eq!(rhs.node, Expr::FieldRef("NOT".to_string()));
    }

    #[test]
    fn a_bare_not_field_is_read_as_the_operator() {
        for query in ["| where NOT == 1", "| where NOT.x == 1"] {
            assert!(
                crate::parser::parse(query).is_err(),
                "{query} must not parse"
            );
        }

        let not_of = |query: &str| match &mut stage_expr(query) {
            Expr::Unary {
                op: UnaryOp::Not,
                operand,
            } => std::mem::replace(&mut operand.node, Expr::Literal(LiteralValue::Null)),
            other => panic!("{query} must read as `not`, got {other:?}"),
        };
        assert_eq!(not_of("| where NOT(x)"), Expr::FieldRef("x".to_string()));
        assert!(
            matches!(
                not_of("| where NOT in (1, 2)"),
                Expr::FunctionCall { ref name, ref args } if name == "in" && args.len() == 2
            ),
            "`NOT in (1, 2)` must read as `not` over a call named `in`"
        );
        assert!(
            matches!(
                not_of("| let y = NOT - 1"),
                Expr::Unary { op: UnaryOp::Neg, ref operand }
                    if operand.node == Expr::Literal(LiteralValue::Int(1))
            ),
            "`NOT - 1` must read as `not (-1)`"
        );

        for (query, field) in [
            ("| where `NOT` == 1", "NOT"),
            ("| where `NOT.x` == 1", "NOT.x"),
        ] {
            let Expr::Binary { ref lhs, .. } = stage_expr(query) else {
                panic!("{query} must be a comparison");
            };
            assert_eq!(lhs.node, Expr::FieldRef(field.to_string()), "{query}");
        }
    }
}
