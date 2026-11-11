// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Every call a pipeline writes, checked once before either lane runs it
//! (ADR-0001, Amendment 2026-11-11).
//!
//! [`check_calls`] applies the one per-call rule
//! ([`crate::emitter::check_call`]: name, argument count, literal
//! positions) to every call in every stage, the stages after
//! `extract kv` included. Both validation doors call it right after
//! [`crate::aggregate::check_positions`], so a bad call gets one sentence
//! whichever lane would have run it.

use crate::ast::{AggExpr, Expr, PipeStage, Spanned};
use crate::emitter::{EmitError, check_call};

/// One node still to visit: an aggregation slot, whose head is a call
/// that is not an expression node of its own, or an expression.
enum Work<'a> {
    Slot(&'a AggExpr),
    Expr(&'a Expr),
}

/// Refuse the first call, in written order, that breaks its function's
/// rule.
///
/// Stages are walked in order, and within a stage its conditions,
/// assignments and aggregation slots in order. A call is checked before
/// its arguments, so `foo(bar(x))` reports `foo`. A field named like a
/// function (`count`, `lower`) is a field, never a call.
///
/// The walk keeps its own stack, so a flat chain deeper than a worker's
/// stack cannot overflow it (ADR-0061).
///
/// # Errors
///
/// The [`EmitError`] [`check_call`] gives the first bad call.
pub(crate) fn check_calls(stages: &[Spanned<PipeStage>]) -> Result<(), EmitError> {
    let mut stack: Vec<Work<'_>> = Vec::new();
    for stage in stages {
        // Each stage's roots in reverse, so the first is visited first.
        match &stage.node {
            PipeStage::Where(w) => stack.push(Work::Expr(&w.condition.node)),
            PipeStage::Let(l) => stack.extend(
                l.assignments
                    .iter()
                    .rev()
                    .map(|(_, expr)| Work::Expr(&expr.node)),
            ),
            PipeStage::Stats(s) => stack.extend(s.aggregations.iter().rev().map(Work::Slot)),
            PipeStage::Timechart(t) => stack.extend(t.aggregations.iter().rev().map(Work::Slot)),
            PipeStage::EventStats(e) => stack.extend(e.aggregations.iter().rev().map(Work::Slot)),
            PipeStage::Pivot(p) => stack.push(Work::Slot(&p.aggregation)),
            // Field names and settings only: no call to check. An
            // `extract kv` mints columns and nothing else, so the stages
            // after it are walked like any other.
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
        while let Some(work) = stack.pop() {
            match work {
                Work::Slot(slot) => {
                    check_call(&slot.function, &slot.args)?;
                    stack.extend(slot.args.iter().rev().map(|arg| Work::Expr(&arg.node)));
                }
                Work::Expr(expr) => match expr {
                    Expr::FunctionCall { name, args } => {
                        check_call(name, args)?;
                        stack.extend(args.iter().rev().map(|arg| Work::Expr(&arg.node)));
                    }
                    Expr::Binary { lhs, rhs, .. } => {
                        stack.push(Work::Expr(&rhs.node));
                        stack.push(Work::Expr(&lhs.node));
                    }
                    Expr::Unary { operand, .. } => stack.push(Work::Expr(&operand.node)),
                    Expr::InList { expr: target, list } => {
                        stack.extend(list.iter().rev().map(|item| Work::Expr(&item.node)));
                        stack.push(Work::Expr(&target.node));
                    }
                    Expr::FieldRef(_) | Expr::Literal(_) => {}
                },
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::ast::{BinaryOp, Expr, PipeStage, Spanned, WhereStage};
    use crate::emitter::EmitError;

    /// A flat chain of 100,000 `+` terms with one bad call at its bottom
    /// is refused on a 2 MiB thread.
    ///
    /// The parser cannot produce the chain (a query is at most 64 KiB),
    /// so the AST is built by hand. Building, checking and dropping it all
    /// happen on the small thread. The result is matched, never printed:
    /// the tree's derived `Debug` recurses.
    #[test]
    fn check_calls_walks_a_deep_chain_without_recursion() {
        const TERMS: usize = 100_000;
        let leaf = || Spanned::new(Expr::FieldRef("x".to_owned()), 0..1);
        let refused = std::thread::Builder::new()
            .stack_size(2 << 20)
            .spawn(move || {
                let mut chain = Spanned::new(
                    Expr::FunctionCall {
                        name: "nosuch".to_owned(),
                        args: vec![leaf()],
                    },
                    0..1,
                );
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
                let stages = vec![Spanned::new(
                    PipeStage::Where(WhereStage { condition: chain }),
                    0..1,
                )];
                matches!(
                    super::check_calls(&stages),
                    Err(EmitError::UnknownFunction { ref name, .. }) if name == "nosuch"
                )
            })
            .expect("spawn the 2 MiB thread")
            .join()
            .expect("the chain builds, is checked and drops on 2 MiB");
        assert!(
            refused,
            "the bad call at the bottom of the chain is refused"
        );
    }
}
