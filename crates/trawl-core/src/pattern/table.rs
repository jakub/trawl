// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-stage tables of prepared patterns for live evaluation (ADR-0063).
//!
//! A compiled live stage (a `where`, a `let`, a regex `extract`) owns the
//! patterns its expressions read. They are built when the plan compiles,
//! through the plan's one [`Preparer`], so a pattern several stages share
//! builds once. Evaluation only reads a table: a [`PatternTable`] behind a
//! shared reference cannot build anything, so no event pays for a build,
//! and an operand the table does not hold is [`PatternError::Missing`],
//! never a non-match.

use std::collections::HashMap;

use crate::ast::{Expr, Spanned};

use super::{Pattern, PatternError, PatternKind, Preparer, literal_operand};

/// The prepared patterns of one compiled stage, by kind and text.
#[derive(Debug, Clone, Default)]
pub struct PatternTable {
    entries: HashMap<PatternKind, HashMap<String, Pattern>>,
}

impl PatternTable {
    /// The table of every pattern operand in `exprs`: the right-hand side
    /// of each `matches`, `like` and `ilike`, wherever it sits in the tree.
    ///
    /// # Errors
    ///
    /// [`PatternError::Computed`] for an operand that is not literal text
    /// ([`literal_operand`]), and whatever `preparer` refuses for a pattern
    /// it builds.
    pub fn prepare<'e>(
        exprs: impl IntoIterator<Item = &'e Spanned<Expr>>,
        preparer: &mut Preparer,
    ) -> Result<Self, PatternError> {
        let mut table = Self::default();
        // Left to right, each operator before its operands, so the first
        // refusal reported is the first one written. An explicit stack, so
        // a deep hand-built tree cannot overflow this walk.
        let mut stack: Vec<&Expr> = exprs.into_iter().map(|expr| &expr.node).collect();
        stack.reverse();
        while let Some(expr) = stack.pop() {
            match expr {
                Expr::Binary { lhs, op, rhs } => {
                    if let Some(kind) = PatternKind::of_operator(*op) {
                        table.insert(kind, literal_operand(&rhs.node)?, preparer)?;
                    }
                    stack.push(&rhs.node);
                    stack.push(&lhs.node);
                }
                Expr::Unary { operand, .. } => stack.push(&operand.node),
                Expr::FunctionCall { args, .. } => {
                    stack.extend(args.iter().rev().map(|arg| &arg.node));
                }
                Expr::InList { expr: target, list } => {
                    stack.extend(list.iter().rev().map(|item| &item.node));
                    stack.push(&target.node);
                }
                Expr::Literal(_) | Expr::FieldRef(_) => {}
            }
        }
        Ok(table)
    }

    /// Build `text` as a `kind` pattern into this table, unless it holds
    /// it already.
    ///
    /// # Errors
    ///
    /// Whatever `preparer` refuses for the pattern.
    pub(crate) fn insert(
        &mut self,
        kind: PatternKind,
        text: &str,
        preparer: &mut Preparer,
    ) -> Result<(), PatternError> {
        if self.get(kind, text).is_ok() {
            return Ok(());
        }
        let pattern = preparer.build(kind, text)?;
        self.entries
            .entry(kind)
            .or_default()
            .insert(text.to_owned(), pattern);
        Ok(())
    }

    /// The pattern this table prepared for `text` written as `kind`.
    ///
    /// # Errors
    ///
    /// [`PatternError::Missing`] when the table holds no such pattern.
    pub fn get(&self, kind: PatternKind, text: &str) -> Result<&Pattern, PatternError> {
        self.entries
            .get(&kind)
            .and_then(|patterns| patterns.get(text))
            .ok_or(PatternError::Missing)
    }
}

/// A ledger for this crate's unit tests, which prepare tables and compile
/// stages one at a time.
#[cfg(test)]
pub(crate) fn test_ledger() -> Preparer {
    Preparer::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{BinaryOp, LiteralValue};
    use crate::pattern::thread_builds;

    fn span(node: Expr) -> Spanned<Expr> {
        Spanned { node, span: 0..0 }
    }

    fn text(s: &str) -> Spanned<Expr> {
        span(Expr::Literal(LiteralValue::String(s.into())))
    }

    fn field(name: &str) -> Spanned<Expr> {
        span(Expr::FieldRef(name.into()))
    }

    fn binary(lhs: Spanned<Expr>, op: BinaryOp, rhs: Spanned<Expr>) -> Spanned<Expr> {
        span(Expr::Binary {
            lhs: Box::new(lhs),
            op,
            rhs: Box::new(rhs),
        })
    }

    fn call(name: &str, args: Vec<Spanned<Expr>>) -> Spanned<Expr> {
        span(Expr::FunctionCall {
            name: name.into(),
            args,
        })
    }

    /// Every pattern operand is found, at any depth and under every
    /// operator that holds a subexpression, and keyed by its own kind.
    #[test]
    fn a_table_holds_every_pattern_operand_in_the_tree() {
        let nested = call(
            "if",
            vec![
                binary(field("host"), BinaryOp::Like, text("web-%")),
                span(Expr::Unary {
                    op: crate::ast::UnaryOp::Not,
                    operand: Box::new(binary(field("message"), BinaryOp::Matches, text("err"))),
                }),
                span(Expr::InList {
                    expr: Box::new(binary(field("user"), BinaryOp::ILike, text("ROOT"))),
                    list: vec![binary(field("a"), BinaryOp::Matches, text("^x"))],
                }),
            ],
        );
        let table = PatternTable::prepare([&nested], &mut test_ledger()).expect("prepares");

        assert!(table.get(PatternKind::Like, "web-%").is_ok());
        assert!(table.get(PatternKind::Regex, "err").is_ok());
        assert!(table.get(PatternKind::ILike, "ROOT").is_ok());
        assert!(table.get(PatternKind::Regex, "^x").is_ok());
        // The same text under another kind is another pattern.
        assert_eq!(
            table.get(PatternKind::Like, "err").err(),
            Some(PatternError::Missing)
        );
    }

    /// A lookup the table cannot answer is a typed error, not a
    /// non-match and not a build.
    #[test]
    fn a_pattern_the_table_does_not_hold_is_missing() {
        let table = PatternTable::default();
        let before = thread_builds();
        assert_eq!(
            table.get(PatternKind::Regex, "a").err(),
            Some(PatternError::Missing)
        );
        assert_eq!(thread_builds(), before, "a lookup builds nothing");
    }

    /// A computed operand is refused while the table is prepared, by the
    /// one literal-only rule.
    #[test]
    fn a_computed_operand_is_refused_at_preparation() {
        for rhs in [
            field("pattern"),
            call("lower", vec![text("A")]),
            span(Expr::Literal(LiteralValue::Null)),
        ] {
            let expr = binary(field("message"), BinaryOp::Matches, rhs);
            assert_eq!(
                PatternTable::prepare([&expr], &mut test_ledger()).err(),
                Some(PatternError::Computed),
                "{expr:?}"
            );
        }
    }

    /// One ledger behind several tables builds a shared pattern once, and
    /// a pattern repeated in one table is held once.
    #[test]
    fn tables_share_their_ledgers_builds() {
        let mut ledger = test_ledger();
        let condition = binary(field("message"), BinaryOp::Matches, text("err"));
        let both = binary(
            binary(field("message"), BinaryOp::Matches, text("err")),
            BinaryOp::And,
            binary(field("host"), BinaryOp::Like, text("web-%")),
        );
        let before = thread_builds();
        let first = PatternTable::prepare([&condition, &condition], &mut ledger).expect("prepares");
        let second = PatternTable::prepare([&both], &mut ledger).expect("prepares");
        assert_eq!(ledger.builds(), 2, "one build per unique (kind, text)");
        assert_eq!(thread_builds() - before, 2);
        assert!(first.get(PatternKind::Regex, "err").is_ok());
        assert!(second.get(PatternKind::Regex, "err").is_ok());
        assert!(second.get(PatternKind::Like, "web-%").is_ok());
    }

    /// A pattern the ledger refuses fails the preparation with the
    /// ledger's own cause.
    #[test]
    fn a_refused_build_fails_the_preparation() {
        let expr = binary(field("message"), BinaryOp::Matches, text("["));
        assert!(matches!(
            PatternTable::prepare([&expr], &mut test_ledger()),
            Err(PatternError::Syntax(_))
        ));
    }
}
