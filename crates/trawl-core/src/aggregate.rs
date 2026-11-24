// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The closed set of aggregate functions, and where one may stand
//! (ADR-0001, Amendment 2026-11-08).

use crate::ast::{AggExpr, Expr, PipeStage, Spanned};
use crate::emitter::EmitError;

/// The AST keeps the spelling the query wrote ([`crate::ast::AggExpr`]),
/// because an output's default name derives from it
/// ([`crate::projection::agg_output_name`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Aggregate {
    Count,
    Avg,
    Sum,
    Min,
    Max,
    DistinctCount,
    P50,
    P90,
    P95,
    P99,
    First,
    Last,
    Values,
    Median,
    Stddev,
}

impl Aggregate {
    pub const ALL: [Self; 15] = [
        Self::Count,
        Self::Avg,
        Self::Sum,
        Self::Min,
        Self::Max,
        Self::DistinctCount,
        Self::P50,
        Self::P90,
        Self::P95,
        Self::P99,
        Self::First,
        Self::Last,
        Self::Values,
        Self::Median,
        Self::Stddev,
    ];

    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "count" => Self::Count,
            "avg" => Self::Avg,
            "sum" => Self::Sum,
            "min" => Self::Min,
            "max" => Self::Max,
            "dc" | "distinct_count" => Self::DistinctCount,
            "p50" => Self::P50,
            "p90" => Self::P90,
            "p95" => Self::P95,
            "p99" => Self::P99,
            "first" => Self::First,
            "last" => Self::Last,
            "values" | "list" => Self::Values,
            "median" => Self::Median,
            "stddev" => Self::Stddev,
            _ => return None,
        })
    }

    #[must_use]
    pub const fn spellings(self) -> &'static [&'static str] {
        match self {
            Self::Count => &["count"],
            Self::Avg => &["avg"],
            Self::Sum => &["sum"],
            Self::Min => &["min"],
            Self::Max => &["max"],
            Self::DistinctCount => &["dc", "distinct_count"],
            Self::P50 => &["p50"],
            Self::P90 => &["p90"],
            Self::P95 => &["p95"],
            Self::P99 => &["p99"],
            Self::First => &["first"],
            Self::Last => &["last"],
            Self::Values => &["values", "list"],
            Self::Median => &["median"],
            Self::Stddev => &["stddev"],
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Position<'a> {
    /// Evaluated once per row: a `where` condition or a `let` value.
    Row,
    /// An aggregation slot's head, or an argument of a scalar call that
    /// stands in one: `coalesce(sum(x), 0)` aggregates `sum(x)`.
    Slot,
    /// Inside the operand of the named aggregate, at any depth.
    Operand(&'a str),
}

/// Refuse an aggregate call written where neither lane can run it.
/// The walk keeps its own stack, so a flat chain deeper than a worker's
/// stack cannot overflow it (ADR-0061).
///
/// # Errors
///
/// [`EmitError::InvalidAggregation`] naming the misplaced aggregate.
pub fn check_positions(stages: &[Spanned<PipeStage>]) -> Result<(), EmitError> {
    let mut stack: Vec<(&Expr, Position<'_>)> = Vec::new();
    for stage in stages {
        // Each stage's roots in reverse, so the first is visited first.
        match &stage.node {
            PipeStage::Where(w) => stack.push((&w.condition.node, Position::Row)),
            PipeStage::Let(l) => stack.extend(
                l.assignments
                    .iter()
                    .rev()
                    .map(|(_, expr)| (&expr.node, Position::Row)),
            ),
            PipeStage::Stats(s) => push_slots(&mut stack, &s.aggregations),
            PipeStage::Timechart(t) => push_slots(&mut stack, &t.aggregations),
            PipeStage::EventStats(e) => push_slots(&mut stack, &e.aggregations),
            PipeStage::Pivot(p) => push_slots(&mut stack, std::slice::from_ref(&p.aggregation)),
            PipeStage::Sort(_)
            | PipeStage::Limit(_)
            | PipeStage::Table(_)
            | PipeStage::Top(_)
            | PipeStage::Rare(_)
            | PipeStage::Drop(_)
            | PipeStage::Extract(_)
            | PipeStage::Dedup(_)
            | PipeStage::Tail(_)
            | PipeStage::Rename(_)
            | PipeStage::Sample(_)
            | PipeStage::FromSaved(_) => {}
        }
        while let Some((expr, position)) = stack.pop() {
            match expr {
                Expr::FunctionCall { name, args } => {
                    let inner = if Aggregate::from_name(name).is_some() {
                        match position {
                            Position::Row => {
                                return Err(EmitError::InvalidAggregation {
                                    message: format!(
                                        "{name}() is an aggregation; use it in stats, \
                                         timechart, eventstats or pivot"
                                    ),
                                });
                            }
                            Position::Operand(outer) => {
                                return Err(EmitError::InvalidAggregation {
                                    message: format!("{name}() cannot be nested inside {outer}()"),
                                });
                            }
                            Position::Slot => Position::Operand(name),
                        }
                    } else {
                        position
                    };
                    stack.extend(args.iter().rev().map(|arg| (&arg.node, inner)));
                }
                Expr::Binary { lhs, rhs, .. } => {
                    stack.push((&rhs.node, position));
                    stack.push((&lhs.node, position));
                }
                Expr::Unary { operand, .. } => stack.push((&operand.node, position)),
                Expr::InList { expr: target, list } => {
                    stack.extend(list.iter().rev().map(|item| (&item.node, position)));
                    stack.push((&target.node, position));
                }
                // A field named `count` is a field, never a call.
                Expr::FieldRef(_) | Expr::Literal(_) => {}
            }
        }
    }
    Ok(())
}

/// Push a stage's aggregation slots, last first. The head is not an
/// expression node of its own, so its arguments are pushed directly: an
/// aggregate head's as its operand, a scalar head's as the slot.
fn push_slots<'a>(stack: &mut Vec<(&'a Expr, Position<'a>)>, slots: &'a [AggExpr]) {
    for slot in slots.iter().rev() {
        let position = if Aggregate::from_name(&slot.function).is_some() {
            Position::Operand(&slot.function)
        } else {
            Position::Slot
        };
        stack.extend(slot.args.iter().rev().map(|arg| (&arg.node, position)));
    }
}

#[cfg(test)]
mod tests {
    use crate::ast::{BinaryOp, Expr, PipeStage, Spanned, WhereStage};

    #[test]
    fn flat_expression_validation_is_bounded() {
        const TERMS: usize = 100_000;
        let leaf = || Spanned::new(Expr::FieldRef("x".to_owned()), 0..1);
        std::thread::Builder::new()
            .stack_size(2 << 20)
            .spawn(move || {
                let mut chain = leaf();
                for _ in 1..TERMS {
                    chain = Spanned::new(
                        Expr::Binary {
                            lhs: Box::new(chain),
                            op: BinaryOp::Add,
                            rhs: Box::new(leaf()),
                        },
                        0..1,
                    );
                }
                let condition = Spanned::new(
                    Expr::Binary {
                        lhs: Box::new(chain),
                        op: BinaryOp::Gt,
                        rhs: Box::new(Spanned::new(
                            Expr::Literal(crate::ast::LiteralValue::Int(0)),
                            0..1,
                        )),
                    },
                    0..1,
                );
                let stages = vec![Spanned::new(
                    PipeStage::Where(WhereStage { condition }),
                    0..1,
                )];
                crate::emitter::validate_pipeline(&stages)
            })
            .expect("spawn the 2 MiB thread")
            .join()
            .expect("the chain builds, validates and drops on 2 MiB")
            .expect("a flat chain of plain field reads is valid");
    }
}
