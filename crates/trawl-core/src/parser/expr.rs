// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Layer 4: expression parser with operator precedence.
//!
//! Recursive descent with `foldl`/`foldr` for 7 precedence levels.
//! Used in `where` clauses, aggregation arguments, and `in` lists.

use chumsky::prelude::*;

use std::cell::Cell;
use std::ops::Range;

use crate::ast::{BinaryOp, Expr, LiteralValue, Spanned, UnaryOp};
use crate::parser::comment::Spaced;
use crate::parser::primitives::{
    ParserExtra, ParserInput, field_name, keyword, literal, plain_name, quoted_string,
    regex_pattern, spanned,
};

/// How deeply one expression may nest before the grammar refuses it.
///
/// Every nesting level — a parenthesised sub-expression, a function
/// call's arguments, an `in (…)` list — is one more recursive descent
/// through the whole precedence chain, and each descent costs stack. The
/// input cap (`MAX_QUERY_LEN`) alone does not bound that: `a(a(a(…` fits
/// twenty thousand levels into 64 KiB of text, which is enough to walk a
/// thread off the end of its stack. trawld parses inside its axum
/// handlers, on a tokio worker whose stack is 2 MiB, so an unbounded
/// grammar is a remote crash a single query can spell.
///
/// How much a level costs is a property of the BUILD, not of the DSL: a
/// release build spends tens of bytes per level, while an unoptimized
/// build with full debuginfo spends tens of KILOBYTES (LLVM cannot merge
/// the stack slots of chumsky's enormous combinator types once every one
/// of them is a named debugger variable). That spread — three orders of
/// magnitude — is why an unbounded grammar cannot be argued safe from
/// one measurement, and it is how this bound came to be written: CI,
/// which builds exactly that way, took SIGSEGV on 300 nested parens
/// while the same corpus passed on a developer's
/// `debug = "line-tables-only"` build using a sixth of the stack it had.
///
/// So the number is set against the HUNGRIEST build, measured there and
/// not extrapolated: in an unoptimized full-debuginfo build a 2 MiB
/// stack carries nesting into the forties and dies in the sixties, so
/// sixteen is the bound — about a quarter of what that build survives,
/// and a rounding error in a release one.
///
/// Sixteen is still far past any query a person writes: `((a + b) * (c -
/// d))` is two, and the deepest expression in this repository's tests,
/// docs and UI-composed queries is three.
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
/// the SAME query would be refused.
struct DepthGuard;

impl Drop for DepthGuard {
    fn drop(&mut self) {
        DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Run `inner` one nesting level deeper, refusing past [`MAX_EXPR_DEPTH`].
///
/// The refusal is a parse error carrying the limit, not a silent
/// truncation and not a crash: an over-nested query is answered, in the
/// same shape as every other thing the grammar declines.
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

        // function_call: ident "(" args ")" — must try before bare field_ref.
        // The head is the UNQUOTED production: a function is not a field,
        // so `lower`(x) is a field reference, never a call (ADR-0013 §7).
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

        // --- precedence 5: additive `+` `-` ---
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

        // --- precedence 4: comparison ---
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
                    // matches with regex literal support
                    keyword("matches")
                        .spaced()
                        .ignore_then(choice((regex_literal, additive.clone())))
                        .map(|rhs| CmpRhs::Binary(BinaryOp::Matches, rhs)),
                    // like / ilike pattern matching
                    keyword("ilike")
                        .spaced()
                        .ignore_then(additive.clone())
                        .map(|rhs| CmpRhs::Binary(BinaryOp::ILike, rhs)),
                    keyword("like")
                        .spaced()
                        .ignore_then(additive.clone())
                        .map(|rhs| CmpRhs::Binary(BinaryOp::Like, rhs)),
                    // other comparison operators
                    cmp_op
                        .then(additive)
                        .map(|(op, rhs)| CmpRhs::Binary(op, rhs)),
                    // in list
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

        // --- precedence 3: unary `not` ---
        let not_expr = keyword("not")
            .spaced()
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
            keyword("and").spaced().ignore_then(not_expr).repeated(),
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
            keyword("or").spaced().ignore_then(and_expr).repeated(),
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
    InList(Vec<Spanned<Expr>>, Range<usize>),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_expr(input: &str) -> Spanned<Expr> {
        expr().parse(input).into_result().unwrap()
    }

    /// A backticked name is ALWAYS a field reference (ADR-0013 ruling 7):
    /// function names are not fields, so the call head takes the
    /// unquoted production and `` `lower`(x) `` does not parse as a call.
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
        // …and the whole-query door refuses it outright.
        assert!(crate::parser::parse("| let y = `lower`(x)").is_err());
    }

    /// A backticked field reference reaches expressions verbatim.
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
    fn test_in_list_span() {
        let input = "x in (1, 2)";
        let result = parse_expr(input);
        assert_eq!(result.span.start, 0);
        assert_eq!(result.span.end, input.len());
    }

    #[test]
    fn test_keyword_not_in_ident() {
        // "android" should parse as field_ref, not "and" + "roid"
        let result = parse_expr("android");
        assert_eq!(result.node, Expr::FieldRef("android".to_string()));
    }

    #[test]
    fn test_double_minus() {
        // `a - -b` should parse as Sub(a, Neg(b))
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
        // string literal RHS still works
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
    fn test_division_still_works() {
        // `/` as division must not be confused with regex delimiters
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
}
