//! Layer 4: expression parser with operator precedence.
//!
//! Recursive descent with `foldl`/`foldr` for 7 precedence levels.
//! Used in `where` clauses, aggregation arguments, and `in` lists.

use chumsky::prelude::*;

use crate::ast::{BinaryOp, Expr, LiteralValue, Spanned, UnaryOp};
use crate::parser::primitives::{
    ParserExtra, ParserInput, field_name, keyword, literal, quoted_string, spanned,
};

/// Parse an expression with full operator precedence.
///
/// Precedence (low to high):
/// 1. `or`
/// 2. `and`
/// 3. `not` (unary)
/// 4. `==` `!=` `>` `>=` `<` `<=` `in` `matches` (comparison)
/// 5. `+` `-` (additive)
/// 6. `*` `/` `%` (multiplicative)
/// 7. `-` (unary negation)
#[allow(clippy::too_many_lines)]
pub(crate) fn expr<'src>()
-> impl Parser<'src, ParserInput<'src>, Spanned<Expr>, ParserExtra<'src>> + Clone {
    recursive(|expr| {
        // --- atoms ---

        // function_call: ident "(" args ")" — must try before bare field_ref
        let func_call = spanned(
            field_name()
                .then_ignore(just('(').padded())
                .then(
                    expr.clone()
                        .separated_by(just(',').padded())
                        .collect::<Vec<_>>(),
                )
                .then_ignore(just(')').padded())
                .map(|(name, args)| Expr::FunctionCall { name, args }),
        );

        let literal_expr = spanned(literal().map(Expr::Literal));

        let string_literal_expr =
            spanned(quoted_string().map(|s| Expr::Literal(LiteralValue::String(s))));

        let field_ref = spanned(field_name().map(Expr::FieldRef));

        let paren_expr = expr
            .clone()
            .delimited_by(just('(').padded(), just(')').padded());

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
        .padded()
        .labelled("expression")
        .boxed();

        // --- precedence 7: unary negation `-` ---
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

        // --- precedence 6: multiplicative `*` `/` `%` ---
        let mul_op = choice((
            just('*').to(BinaryOp::Mul),
            just('/').to(BinaryOp::Div),
            just('%').to(BinaryOp::Mod),
        ))
        .padded();

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

        // --- precedence 5: additive `+` `-` ---
        let add_op = choice((just('+').to(BinaryOp::Add), just('-').to(BinaryOp::Sub))).padded();

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

        // --- precedence 4: comparison ---
        let cmp_op = choice((
            just("==").to(BinaryOp::Eq),
            just("!=").to(BinaryOp::Ne),
            just(">=").to(BinaryOp::Gte),
            just(">").to(BinaryOp::Gt),
            just("<=").to(BinaryOp::Lte),
            just("<").to(BinaryOp::Lt),
            keyword("matches").to(BinaryOp::Matches),
        ))
        .padded();

        let comparison = additive
            .clone()
            .then(
                choice((
                    cmp_op
                        .then(additive)
                        .map(|(op, rhs)| CmpRhs::Binary(op, rhs)),
                    keyword("in")
                        .padded()
                        .ignore_then(
                            expr.clone()
                                .separated_by(just(',').padded())
                                .collect::<Vec<_>>()
                                .delimited_by(just('(').padded(), just(')').padded()),
                        )
                        .map(CmpRhs::InList),
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
                Some(CmpRhs::InList(list)) => {
                    let span = lhs.span.clone();
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

        // --- precedence 3: unary `not` ---
        let not_expr = keyword("not")
            .padded()
            .repeated()
            .foldr(comparison, |_kw, operand| {
                let span = operand.span.clone();
                Spanned::new(
                    Expr::Unary {
                        op: UnaryOp::Not,
                        operand: Box::new(operand),
                    },
                    span,
                )
            });

        // --- precedence 2: `and` ---
        let and_expr = not_expr.clone().foldl(
            keyword("and").padded().ignore_then(not_expr).repeated(),
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

        // --- precedence 1: `or` ---
        and_expr.clone().foldl(
            keyword("or").padded().ignore_then(and_expr).repeated(),
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

/// Helper enum for comparison right-hand side.
#[derive(Debug)]
enum CmpRhs {
    Binary(BinaryOp, Spanned<Expr>),
    InList(Vec<Spanned<Expr>>),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_expr(input: &str) -> Spanned<Expr> {
        expr().parse(input).into_result().unwrap()
    }

    #[test]
    fn test_literal_int() {
        let result = parse_expr("42");
        assert_eq!(result.node, Expr::Literal(LiteralValue::Int(42)));
    }

    #[test]
    fn test_literal_float() {
        let result = parse_expr("3.25");
        assert_eq!(result.node, Expr::Literal(LiteralValue::Float(3.25)));
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
        // should parse as a + (b * c) due to precedence
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
        // should parse as (a > 1 and b < 2) or (c == 3)
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
    fn test_keyword_not_in_ident() {
        // "android" should parse as field_ref, not "and" + "roid"
        let result = parse_expr("android");
        assert_eq!(result.node, Expr::FieldRef("android".to_string()));
    }
}
