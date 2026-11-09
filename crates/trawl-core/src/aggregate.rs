// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The closed set of aggregate functions, and where one may stand
//! (ADR-0001, Amendment 2026-11-08).
//!
//! The SQL translator ([`crate::emitter`]) and the live accumulators
//! ([`crate::stream`]) both match [`Aggregate`] with no wildcard arm, so
//! an aggregate added here does not compile until both lanes implement
//! it. A name outside the set is not an aggregate, whatever slot it is
//! written in.
//!
//! [`check_positions`] refuses an aggregate written where neither lane
//! can run it. Both validation doors call it at the same point, so both
//! lanes answer one query with one sentence.

use crate::ast::{AggExpr, Expr, PipeStage, Spanned};
use crate::emitter::EmitError;

/// One aggregate function. Two have a second spelling: `dc` and
/// `distinct_count` are [`Self::DistinctCount`], `values` and `list` are
/// [`Self::Values`].
///
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
    /// Every aggregate, in declaration order.
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

    /// The aggregate a function name spells, matched exactly: `Count`
    /// is not `count`.
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

    /// Every name [`Self::from_name`] reads as this aggregate.
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

/// Where an expression stands, for [`check_positions`].
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

/// Refuse an aggregate call written where neither lane can run it: in a
/// per-row expression (`where`, `let`), at any depth, or inside another
/// aggregate's operand. A scalar call around an aggregate in an
/// aggregation slot aggregates it, so `stats coalesce(sum(x), 0)` stays
/// legal and `stats sum(coalesce(count(x), 0))` does not.
///
/// The walk keeps its own stack, so a flat chain deeper than a worker's
/// stack cannot overflow it (ADR-0061). Children are visited left to
/// right and stages in order, so the refusal reported is the first one
/// written.
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
            // Field names and settings only: no expression to stand in.
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

    /// Validation of a flat chain of 100,000 `+` terms in a `where`
    /// finishes on a 2 MiB thread.
    ///
    /// The parser cannot produce the chain (a query is at most 64 KiB),
    /// so the AST is built by hand. Building and dropping it happen on a
    /// thread with a large stack, because `Drop` of a nested `Box` chain
    /// recurses (#330). Only `validate_pipeline` runs on the small
    /// thread, and over a `where` it walks nothing but
    /// [`super::check_positions`]: the complexity budget and the
    /// projection check have nothing to do there.
    #[test]
    fn flat_expression_validation_is_bounded() {
        const TERMS: usize = 100_000;
        let leaf = || Spanned::new(Expr::FieldRef("x".to_owned()), 0..1);
        let big = std::thread::Builder::new()
            .stack_size(1 << 30)
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

                let (stages, verdict) = std::thread::Builder::new()
                    .stack_size(2 << 20)
                    .spawn(move || {
                        let verdict = crate::emitter::validate_pipeline(&stages);
                        (stages, verdict)
                    })
                    .expect("spawn the 2 MiB thread")
                    .join()
                    .expect("validation finishes on 2 MiB");
                drop(stages);
                verdict
            })
            .expect("spawn the big-stack thread");
        big.join()
            .expect("the big-stack thread builds and drops the chain")
            .expect("a flat chain of plain field reads is valid");
    }
}
