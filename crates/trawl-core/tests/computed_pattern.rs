// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A `matches`, `like` or `ilike` pattern is literal text in the query
//! (ADR-0063), and each door a query passes through refuses a computed
//! one on its own: the parser, the SQL emitter, and the live plan
//! compile. A hand-built AST skips the parser, so the two doors after it
//! are checked over ASTs the parser never saw.

use trawl_core::ast::{BinaryOp, Expr, LiteralValue, PipeStage, Query, Spanned};
use trawl_core::context::EvalContext;
use trawl_core::emitter::{EmitError, emit};
use trawl_core::parser::{parse, parse_unbuilt};
use trawl_core::pattern::PatternError;
use trawl_core::pin_scope::PinScope;
use trawl_core::stream::{StreamPlanError, compile_stream_plan};

const PATTERN_NOT_LITERAL: &str = "a pattern must be literal text in the query";

const OPERATORS: [(BinaryOp, &str); 3] = [
    (BinaryOp::Matches, "matches"),
    (BinaryOp::Like, "like"),
    (BinaryOp::ILike, "ilike"),
];

/// Computed operands as the query spells them, and as an AST.
fn computed_operands() -> Vec<(&'static str, Expr)> {
    let text = |s: &str| Spanned::new(Expr::Literal(LiteralValue::String(s.to_owned())), 0..0);
    vec![
        ("field", Expr::FieldRef("field".to_owned())),
        (
            r#"lower("a")"#,
            Expr::FunctionCall {
                name: "lower".to_owned(),
                args: vec![text("a")],
            },
        ),
        (
            r#"concat("a", "%")"#,
            Expr::FunctionCall {
                name: "concat".to_owned(),
                args: vec![text("a"), text("%")],
            },
        ),
    ]
}

/// `query` with the pattern operand of its one `op` test, in a `where` or
/// `let` stage, replaced by `operand`.
fn with_operand(mut query: Query, op: BinaryOp, operand: &Expr) -> Query {
    let condition = match &mut query.pipeline[0].node {
        PipeStage::Where(stage) => &mut stage.condition,
        PipeStage::Let(stage) => &mut stage.assignments[0].1,
        other => panic!("not a where or let stage: {other:?}"),
    };
    let Expr::Binary { op: found, rhs, .. } = &mut condition.node else {
        panic!("not a pattern test: {condition:?}");
    };
    assert_eq!(*found, op);
    rhs.node = operand.clone();
    query
}

#[test]
fn computed_pattern_is_refused_by_every_door() {
    for (op, word) in OPERATORS {
        for (spelled, operand) in computed_operands() {
            for stage in [
                format!("where message {word} {spelled}"),
                format!("let hit = message {word} {spelled}"),
            ] {
                let dsl = format!("* | {stage}");
                let label = format!("{dsl:?}");

                // The parser, whether or not it goes on to build.
                for (door, result) in [
                    ("parse", parse(&dsl)),
                    ("parse_unbuilt", parse_unbuilt(&dsl)),
                ] {
                    let errors = result.expect_err(&format!("{door} {label}"));
                    assert_eq!(errors.len(), 1, "{door} {label}: {errors:?}");
                    let error = &errors[0];
                    assert!(
                        error.message.starts_with(PATTERN_NOT_LITERAL),
                        "{door} {label}: {error:?}"
                    );
                    assert_eq!(&dsl[error.span.clone()], spelled, "{door} {label}");
                    assert!(error.hint.is_some(), "{door} {label}");
                }

                // The same stage over a literal operand, which the parser
                // admits, with the computed operand put in by hand.
                let literal = dsl.replace(spelled, r#""a""#);
                let query = with_operand(
                    parse(&literal).unwrap_or_else(|e| panic!("{literal}: {e:?}")),
                    op,
                    &operand,
                );

                // The emitter.
                assert_eq!(
                    emit(&query, "/data/*.parquet", EvalContext::capture()).map(|_| ()),
                    Err(EmitError::ComputedPattern),
                    "emit {label}"
                );

                // The live plan compile.
                assert_eq!(
                    compile_stream_plan(&query.pipeline, &PinScope::unpinned(), None).err(),
                    Some(StreamPlanError::Pattern(PatternError::Computed)),
                    "live compile {label}"
                );
            }
        }
    }
}
