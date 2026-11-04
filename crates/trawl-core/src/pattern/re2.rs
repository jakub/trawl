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
//!
//! # The view
//!
//! RE2 reads the Perl classes `\w`, `\d` and `\s` and the word assertions
//! `\b` and `\B` as ASCII, and the `regex` crate reads them as Unicode. So
//! the same body compiles to a far larger program in Rust than in RE2 when
//! it holds them: `\w{20}` is about 1 MiB in Rust and 2 KiB in RE2. The
//! view ([`view`]) is the body with each of them written out in its ASCII
//! meaning, in syntax the `regex` crate reads the way RE2 reads the
//! original, so the size the `regex` crate compiles it to measures what
//! RE2 compiles.
//!
//! RE2's ASCII meanings, from the bundled `perl_groups.cc` and `parse.cc`:
//!
//! - `\d` is `[0-9]`, `\s` is `[\t\n\f\r ]` (no `\v`), and `\w` is
//!   `[0-9A-Za-z_]`. `\D`, `\S` and `\W` are their complements over all of
//!   Unicode, so they stay large classes with few ranges.
//! - Under `(?i)` RE2 case-folds a Perl class like any other class: `\w`
//!   gains the Kelvin sign and the long s, and `\W` loses them. The view
//!   writes the ASCII class where the Perl class stood, so the `regex`
//!   crate folds it under the same flags.
//! - `\b` and `\B` test ASCII word characters. The view writes each word
//!   assertion as `(?-u:…)`. A word assertion compiles to one state either
//!   way, so this keeps the meaning and does not change the size.
//!
//! Everything else that this module does not refuse reads the same in
//! both engines, or is syntax RE2 refuses, which `DuckDB` reports when the
//! query runs.

use regex_syntax::ast::{
    self, Ast, ClassBracketed, ClassPerl, ClassPerlKind, ClassSet, ClassSetItem, ClassSetRange,
    ClassSetUnion, Flag, Flags, FlagsItem, FlagsItemKind, Group, GroupKind, HexLiteralKind,
    Literal, LiteralKind, Span, Visitor,
};

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

    fn visit_class_set_item_pre(&mut self, item: &ClassSetItem) -> Result<(), Found> {
        match item {
            // A set item sits inside a bracketed class, so a bracketed
            // item is a class nested in one. A POSIX class is an `Ascii`
            // item, and an escaped `[` or `]` a `Literal`.
            ClassSetItem::Bracketed(_) => Err(Found),
            ClassSetItem::Empty(_)
            | ClassSetItem::Literal(_)
            | ClassSetItem::Range(_)
            | ClassSetItem::Ascii(_)
            | ClassSetItem::Unicode(_)
            | ClassSetItem::Perl(_)
            | ClassSetItem::Union(_) => Ok(()),
        }
    }

    fn visit_class_set_binary_op_pre(&mut self, _op: &ast::ClassSetBinaryOp) -> Result<(), Found> {
        Err(Found)
    }
}

/// The body `ast` parses from, as RE2 reads it, in syntax the `regex`
/// crate reads the same way ([module docs](self)).
///
/// The rewrite runs on a copy of the syntax tree and prints it back, as
/// [`super::batch_extract`] does. A body with none of the five escapes
/// prints back as its tree reads, which is the body itself unless it
/// holds `(?x)` whitespace or comments. It builds nothing.
pub(crate) fn view(ast: &Ast) -> String {
    let mut tree = ast.clone();
    // An explicit stack, as the syntax tree's own visitor keeps, so a
    // deeply nested body cannot overflow the thread's stack.
    let mut open: Vec<&mut Ast> = vec![&mut tree];
    while let Some(node) = open.pop() {
        if let Some(ascii) = ascii_node(node) {
            *node = ascii;
            continue;
        }
        match node {
            Ast::Group(group) => open.push(&mut group.ast),
            Ast::Repetition(repetition) => open.push(&mut repetition.ast),
            Ast::Alternation(alternation) => open.extend(alternation.asts.iter_mut()),
            Ast::Concat(concat) => open.extend(concat.asts.iter_mut()),
            Ast::ClassBracketed(class) => ascii_set(&mut class.kind),
            Ast::Empty(_)
            | Ast::Flags(_)
            | Ast::Literal(_)
            | Ast::Dot(_)
            | Ast::Assertion(_)
            | Ast::ClassUnicode(_)
            | Ast::ClassPerl(_) => {}
        }
    }
    let mut regex = String::new();
    // Printing into a `String` cannot fail.
    let _ = ast::print::Printer::new().print(&tree, &mut regex);
    regex
}

/// What a Perl class or a word assertion becomes in the view, or `None`
/// for any other node.
fn ascii_node(node: &Ast) -> Option<Ast> {
    match node {
        Ast::ClassPerl(perl) => Some(Ast::class_bracketed(ascii_class(perl))),
        Ast::Assertion(assertion) => {
            use ast::AssertionKind as K;
            match assertion.kind {
                K::WordBoundary
                | K::NotWordBoundary
                | K::WordBoundaryStart
                | K::WordBoundaryEnd
                | K::WordBoundaryStartAngle
                | K::WordBoundaryEndAngle
                | K::WordBoundaryStartHalf
                | K::WordBoundaryEndHalf => Some(ascii_assertion(assertion)),
                K::StartLine | K::EndLine | K::StartText | K::EndText => None,
            }
        }
        _ => None,
    }
}

/// `assertion` inside `(?-u:…)`, where the `regex` crate tests ASCII word
/// characters, as RE2 does.
fn ascii_assertion(assertion: &ast::Assertion) -> Ast {
    let span = assertion.span;
    let item = |kind| FlagsItem { span, kind };
    Ast::group(Group {
        span,
        kind: GroupKind::NonCapturing(Flags {
            span,
            items: vec![
                item(FlagsItemKind::Negation),
                item(FlagsItemKind::Flag(Flag::Unicode)),
            ],
        }),
        ast: Box::new(Ast::assertion(assertion.clone())),
    })
}

/// Rewrite every Perl class inside a bracketed class's set. A body the
/// ledger builds holds no nested class or set operation
/// ([`class_set_refusal`]), but the rewrite reaches them too, so the view
/// of any body is RE2's reading of its Perl classes.
fn ascii_set(set: &mut ClassSet) {
    enum Open<'a> {
        Set(&'a mut ClassSet),
        Item(&'a mut ClassSetItem),
    }
    let mut open = vec![Open::Set(set)];
    while let Some(next) = open.pop() {
        match next {
            Open::Set(ClassSet::Item(item)) => open.push(Open::Item(item)),
            Open::Set(ClassSet::BinaryOp(op)) => {
                open.push(Open::Set(&mut op.lhs));
                open.push(Open::Set(&mut op.rhs));
            }
            Open::Item(item) => match item {
                ClassSetItem::Perl(perl) => {
                    let ascii = ascii_class(perl);
                    *item = ClassSetItem::Bracketed(Box::new(ascii));
                }
                ClassSetItem::Bracketed(class) => open.push(Open::Set(&mut class.kind)),
                ClassSetItem::Union(union) => open.extend(union.items.iter_mut().map(Open::Item)),
                ClassSetItem::Empty(_)
                | ClassSetItem::Literal(_)
                | ClassSetItem::Range(_)
                | ClassSetItem::Ascii(_)
                | ClassSetItem::Unicode(_) => {}
            },
        }
    }
}

/// RE2's reading of a Perl class: its ASCII ranges, negated as written.
/// Each bound is a `\x` escape, which reads the same under `(?x)`.
fn ascii_class(perl: &ClassPerl) -> ClassBracketed {
    let ranges: &[(char, char)] = match perl.kind {
        ClassPerlKind::Digit => &[('0', '9')],
        ClassPerlKind::Space => &[('\t', '\n'), ('\x0C', '\r'), (' ', ' ')],
        ClassPerlKind::Word => &[('0', '9'), ('A', 'Z'), ('_', '_'), ('a', 'z')],
    };
    let span: Span = perl.span;
    let literal = |c| Literal {
        span,
        kind: LiteralKind::HexFixed(HexLiteralKind::X),
        c,
    };
    let items = ranges
        .iter()
        .map(|&(lo, hi)| {
            if lo == hi {
                ClassSetItem::Literal(literal(lo))
            } else {
                ClassSetItem::Range(ClassSetRange {
                    span,
                    start: literal(lo),
                    end: literal(hi),
                })
            }
        })
        .collect();
    ClassBracketed {
        span,
        negated: perl.negated,
        kind: ClassSet::union(ClassSetUnion { span, items }),
    }
}
