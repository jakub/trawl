// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A regex body as the batch engine reads it (ADR-0063).
//!
//! Under `(?i)` RE2 case-folds a Perl class like any other class: `\w`
//! gains the Kelvin sign and the long s, and `\W` loses them. The view
//! writes the ASCII class where the Perl class stood, so the Rust
//! engine folds it under the same flags.

use regex_syntax::ast::{
    self, Ast, ClassBracketed, ClassPerl, ClassPerlKind, ClassSet, ClassSetItem, ClassSetRange,
    ClassSetUnion, Flag, Flags, FlagsItem, FlagsItemKind, Group, GroupKind, HexLiteralKind,
    Literal, LiteralKind, Span, Visitor,
};

use super::PatternError;

/// The syntax tree's own visitor keeps a heap stack, so a deeply nested
/// body cannot overflow the thread's stack here.
pub(crate) fn class_set_refusal(ast: &Ast) -> Result<(), PatternError> {
    match ast::visit(ast, ClassSetFinder) {
        Ok(()) => Ok(()),
        Err(Found) => Err(PatternError::ClassSetSyntax),
    }
}

struct ClassSetFinder;

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

/// A body with none of the five escapes prints back as its tree reads,
/// which is the body itself unless it holds `(?x)` whitespace or comments.
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
