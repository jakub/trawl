// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A regex body as the batch engine reads it (ADR-0063).
//!
//! The live lane builds a regex body with the `regex` crate, and the batch
//! lane hands the same text to `DuckDB`'s RE2. The two dialects differ
//! (ADR-0001), and some of the differences change what a body costs as
//! well as what it matches. This module holds the rules that keep the
//! batch engine's program inside the bound the walk in `price` sets.
//!
//! # Class set syntax
//!
//! The `regex` crate reads a class nested in a bracketed class
//! (`[a[bc]]`) and the class set operators `&&`, `--` and `~~`. RE2 reads
//! none of them. Inside a class it reads `&`, `-` and `~` as characters
//! and joins every operand into one union, and it reads `[` as a
//! character, so the first `]` closes the class and the rest of a nested
//! class reads as more pattern after it. `[\pL&&a]` matches only `a` in
//! the live lane and any letter, or `&`, in the batch lane. One position
//! of the walk can be several classes in RE2, which compiles them into a
//! program the walk never priced. So a regex body that holds either form
//! is refused ([`class_set_refusal`]). Escaped characters (`[a\&&b]`,
//! `[\[]`) and POSIX classes (`[[:alpha:]]`) are neither, and both engines
//! read them alike.

use regex_syntax::ast::{self, Ast, Visitor};

use super::PatternError;

/// [`PatternError::ClassSetSyntax`] when `ast` holds a class nested in a
/// bracketed class, or a class set operation.
///
/// The syntax tree's own visitor keeps a heap stack, so a deeply nested
/// body cannot overflow the thread's stack here.
pub(crate) fn class_set_refusal(ast: &Ast) -> Result<(), PatternError> {
    match ast::visit(ast, ClassSetFinder) {
        Ok(()) => Ok(()),
        Err(Found) => Err(PatternError::ClassSetSyntax),
    }
}

/// Stops the walk at the first class set syntax it meets.
struct ClassSetFinder;

/// The walk met class set syntax.
struct Found;

impl Visitor for ClassSetFinder {
    type Output = ();
    type Err = Found;

    fn finish(self) -> Result<(), Found> {
        Ok(())
    }

    fn visit_pre(&mut self, _ast: &Ast) -> Result<(), Found> {
        Ok(())
    }

    fn visit_class_set_item_pre(&mut self, item: &ast::ClassSetItem) -> Result<(), Found> {
        match item {
            // A set item sits inside a bracketed class, so a bracketed
            // item is a class nested in one. A POSIX class is an `Ascii`
            // item, and an escaped `[` or `]` a `Literal`.
            ast::ClassSetItem::Bracketed(_) => Err(Found),
            ast::ClassSetItem::Empty(_)
            | ast::ClassSetItem::Literal(_)
            | ast::ClassSetItem::Range(_)
            | ast::ClassSetItem::Ascii(_)
            | ast::ClassSetItem::Unicode(_)
            | ast::ClassSetItem::Perl(_)
            | ast::ClassSetItem::Union(_) => Ok(()),
        }
    }

    fn visit_class_set_binary_op_pre(&mut self, _op: &ast::ClassSetBinaryOp) -> Result<(), Found> {
        Err(Found)
    }
}
