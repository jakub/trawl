// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Translation and repeat pricing over a regex syntax tree (ADR-0063).
//!
//! [`shape`] walks a parsed [`Ast`] once and returns what translating it
//! to HIR costs, how many positions its counted repeats expand to, and its
//! named capture groups. It translates nothing and builds nothing, so
//! admission can price every pattern in a query before any of them
//! compiles. `regex_syntax::ast::visit` keeps its own heap stack, so a
//! deeply nested pattern cannot overflow the thread's stack here.
//!
//! # Units
//!
//! One unit is about 3 ns of `regex-syntax` translation on the reference
//! host, so [`MAX_TRANSLATION_UNITS`] is about 50 ms. The charges follow the
//! pinned translator, `regex-syntax`'s `hir/translate.rs`:
//!
//! | node | units |
//! |---|---|
//! | `(?i)` literal inside `FOLD_WINDOW` | `FOLDED_LITERAL` |
//! | `.` | `DOT_RANGES` |
//! | `\w`, `\d`, `\s` | their table's ranges, twice when negated, and no fold |
//! | `\p{…}`, known or not | `UNICODE_PROPERTY` ranges, a fold of every code point under `(?i)`, and its ranges again when negated |
//! | bracket item | the accumulator's ranges plus the item's own, for the union |
//! | bracket under `(?i)` | a fold of the accumulator |
//! | negated bracket or item | its ranges again; inside a bracket, it then covers every code point |
//! | set operation | both operands' ranges, and under `(?i)` a fold of each operand |
//! | alternation branch that is a class | the ranges of the class branches before it plus its own, for the merge |
//!
//! A fold of a class of at most `R` ranges over at most `S` code points
//! costs `FOLD_RANGE·R + FOLD_CODEPOINT·S + FOLD_MAPPED·min(S,
//! FOLD_MAPPED_MAX)`. The translator steps through every code point of a
//! range that meets the fold table, and pushes one range for each code point
//! that folds. Perl classes are already closed under case folding, so the
//! translator never folds them, and neither does the walk. A bracket that
//! holds one still folds its whole accumulator.
//!
//! Where the walk cannot know the translated shape, it charges the most the
//! shape could cost. The Unicode property tables are private to
//! `regex-syntax`, so every `\p{…}` class is priced at its largest possible
//! shape, and so is a property name the translator will refuse. A negated
//! item inside a bracket covers every code point, so an enclosing `(?i)`
//! bracket refolds the whole complement, as the translator does. A
//! class's range count is the sum of its items' counts, an upper bound on
//! what the translator's canonical class holds, and the same holds for
//! the class an alternation of classes merges into.
//!
//! Flags scope as the translator's `set_flags` scopes them: a group's flags
//! last until the group closes, and a bare `(?i)` lasts until its enclosing
//! group closes, across the alternation branches after it.
//!
//! # Positions
//!
//! A position is a step of the compiled program. Once both engines' lazy
//! DFAs give up, Rust's Pike VM and `RE2`'s NFA pay for every live step on
//! every byte of the subject, so every node that compiles to an
//! instruction counts at least one position:
//!
//! - an atom (a literal, a dot or a class) and an assertion (`^`, `$`,
//!   `\A`, `\z`, and every word boundary) count one;
//! - a capture group, named or not, counts one for its capture
//!   instructions, plus what it holds;
//! - an alternation branch counts what it holds, and at least one for its
//!   path out of the split;
//! - a repeat counts what its operand holds, and at least one for the
//!   split around it, times its upper bound. An unbounded repeat counts its
//!   lower bound plus one. The split is a node of its own, so a repeat
//!   counts at least one more than its operand: `x?`, `x*`, `x{0,1}` and
//!   `x{1}` are two positions, and each wrapper nested around another
//!   adds one. A count of two or more already multiplies past that, so
//!   `x{20}` is 20 and `x+` is 2.
//!
//! Concatenations and non-capturing groups add their children's counts
//! and nothing of their own. Flags, and empty nodes in a concatenation,
//! count zero: the translator drops them, and the cost harness
//! (`crates/trawl-engine/tests/regex_cost.rs`, its zero-charge family)
//! times a run of them inside a counted repeat at the cost of the repeat
//! without them. `(?x)` whitespace and comments are not nodes at all.
//!
//! Units and positions saturate at `u64::MAX`, so no pattern overflows them.

use std::convert::Infallible;

use regex_syntax::ast::{self, Ast, Visitor};

/// The translation budget of one query, in units (ADR-0063).
pub(crate) const MAX_TRANSLATION_UNITS: u64 = 1 << 24;

/// The repeat budget of one pattern, in positions (ADR-0063).
pub(crate) const MAX_REPEAT_POSITIONS: u64 = 64;

/// One range that the translator materialises, unions, negates or merges.
const RANGE: u64 = 1;
/// One range that a case fold visits: a search of the fold table, then its
/// share of sorting the folded class back into canonical order.
const FOLD_RANGE: u64 = 16;
/// One code point that a case fold steps through.
const FOLD_CODEPOINT: u64 = 1;
/// One code point that a case fold maps, pushing its peers as new ranges.
const FOLD_MAPPED: u64 = 6;
/// A `(?i)` literal inside [`FOLD_WINDOW`], which the translator folds into
/// a class of its own.
const FOLDED_LITERAL: u64 = 64;
/// The ranges of the class that `.` translates to.
const DOT_RANGES: u64 = 2;

/// The first and last code points with a simple case fold. The translator
/// folds a `(?i)` literal only when the fold table holds it, and every key
/// of the table lies in this window.
const FOLD_WINDOW: (u32, u32) = (0x41, 0x1_E943);
/// How many code points have a simple case fold.
const FOLD_MAPPED_MAX: u64 = 2938;
/// The most code points that one code point folds to, itself excluded.
const FOLD_PEERS_MAX: u64 = 3;
/// Every value a fold could step through in a Unicode class. The translator
/// steps through a range as integers, so surrogates count.
const CODEPOINTS: u64 = 0x11_0000;
/// Every value of a byte class, which `(?-u)` translates to.
const BYTES: u64 = 256;

/// `\w` in Unicode mode.
const PERL_WORD: Set = Set {
    ranges: 796,
    span: 144_667,
};
/// `\d` in Unicode mode.
const PERL_DIGIT: Set = Set {
    ranges: 71,
    span: 760,
};
/// `\s` in Unicode mode.
const PERL_SPACE: Set = Set {
    ranges: 10,
    span: 25,
};
/// `\w` under `(?-u)`.
const BYTE_WORD: Set = Set {
    ranges: 4,
    span: 63,
};
/// `\d` under `(?-u)`.
const BYTE_DIGIT: Set = Set {
    ranges: 1,
    span: 10,
};
/// `\s` under `(?-u)`, as the translator lists it before merging.
const BYTE_SPACE: Set = Set { ranges: 6, span: 6 };
/// The largest `[[:name:]]` class: `[[:space:]]` lists 6 ranges, and
/// `[[:ascii:]]` covers 128 code points.
const ASCII_CLASS: Set = Set {
    ranges: 6,
    span: 128,
};
/// The largest shape a `\p{…}` class can have: more ranges than any table
/// holds, over every code point.
const UNICODE_PROPERTY: Set = Set {
    ranges: 1024,
    span: CODEPOINTS,
};

/// A named capture group: its name and its absolute group index. The
/// `regex` crate and RE2 both number groups this way, counting unnamed
/// groups too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Capture {
    pub(crate) name: String,
    pub(crate) index: u32,
}

/// What one pattern costs before anything builds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Shape {
    /// The translation charge, in units.
    pub(crate) units: u64,
    /// The repetition-expanded position count.
    pub(crate) positions: u64,
    /// The named capture groups, in pattern order.
    pub(crate) captures: Vec<Capture>,
}

/// Price a parsed pattern: one walk over its syntax tree, which translates
/// nothing.
pub(crate) fn shape(ast: &Ast) -> Shape {
    let Ok(shape) = ast::visit(ast, Walk::new());
    shape
}

/// The named capture groups of `body`, read from its syntax tree. Parses
/// the syntax only, so it builds nothing and translates nothing.
#[expect(
    clippy::result_large_err,
    reason = "the syntax error is regex-syntax's own, and a refused body is rare"
)]
pub(crate) fn captures(body: &str) -> Result<Vec<Capture>, ast::Error> {
    let ast = ast::parse::Parser::new().parse(body)?;
    Ok(shape(&ast).captures)
}

/// An upper bound on one class: how many ranges it holds once canonical,
/// and how many values those ranges cover.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Set {
    ranges: u64,
    span: u64,
}

/// The two flags that change what translation costs.
#[derive(Debug, Clone, Copy)]
struct Flags {
    case_insensitive: bool,
    unicode: bool,
}

impl Flags {
    /// Apply a flag directive as the translator's `set_flags` does: each
    /// flag it names takes the new value, and every other flag keeps its
    /// value. A `-` turns off every flag after it.
    fn apply(&mut self, flags: &ast::Flags) {
        let mut enable = true;
        for item in &flags.items {
            match item.kind {
                ast::FlagsItemKind::Negation => enable = false,
                ast::FlagsItemKind::Flag(ast::Flag::CaseInsensitive) => {
                    self.case_insensitive = enable;
                }
                ast::FlagsItemKind::Flag(ast::Flag::Unicode) => self.unicode = enable,
                ast::FlagsItemKind::Flag(
                    ast::Flag::MultiLine
                    | ast::Flag::DotMatchesNewLine
                    | ast::Flag::SwapGreed
                    | ast::Flag::CRLF
                    | ast::Flag::IgnoreWhitespace,
                ) => {}
            }
        }
    }
}

/// What a finished node translates to, as far as the translator's
/// alternation merge cares. `Hir::alternation` unions branches that are
/// classes into one class, one branch at a time, so each class branch
/// costs a union with the branches merged before it.
///
/// When the walk cannot tell, it says `Class`, which only charges more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Translated {
    /// Matches only the empty string, so a concatenation drops it.
    Empty,
    /// A class, or a node the walk prices as one, of at most this many
    /// ranges.
    Class(u64),
    /// Anything else, such as a capture group or a literal string.
    Other,
}

/// How an open container combines what its children translate to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Combine {
    /// A concatenation, or the whole pattern: one class among empty nodes
    /// stays a class.
    Concat,
    /// An alternation: each class branch merges into the branches before it.
    Alternation,
    /// A group or a repeat, around its one child.
    Single,
}

/// One open container.
#[derive(Debug)]
struct Frame {
    combine: Combine,
    positions: u64,
    translated: Translated,
    /// For an alternation, the ranges its class branches merged so far.
    merged: u64,
}

impl Frame {
    fn new(combine: Combine) -> Self {
        Frame {
            combine,
            positions: 0,
            translated: match combine {
                Combine::Concat | Combine::Single => Translated::Empty,
                Combine::Alternation => Translated::Class(0),
            },
            merged: 0,
        }
    }
}

/// The walk's state. The visitor calls each node's pre and post hooks in
/// matched pairs, so every stack below has a top when a post hook pops it.
#[derive(Debug)]
struct Walk {
    flags: Flags,
    /// The flags to restore when each open group closes.
    saved: Vec<Flags>,
    /// One accumulator per open bracket and per open set-operation operand.
    classes: Vec<Set>,
    /// One frame per open container. The first is the whole pattern's.
    frames: Vec<Frame>,
    units: u64,
    captures: Vec<Capture>,
}

impl Walk {
    fn new() -> Self {
        Walk {
            flags: Flags {
                case_insensitive: false,
                unicode: true,
            },
            saved: Vec::new(),
            classes: Vec::new(),
            frames: vec![Frame::new(Combine::Concat)],
            units: 0,
            captures: Vec::new(),
        }
    }

    fn charge(&mut self, units: u64) {
        self.units = self.units.saturating_add(units);
    }

    /// Charge one pass over a class's ranges.
    fn pass(&mut self, set: Set) {
        self.charge(set.ranges.saturating_mul(RANGE));
    }

    fn open(&mut self, combine: Combine) {
        self.frames.push(Frame::new(combine));
    }

    fn close(&mut self) -> Frame {
        self.frames
            .pop()
            .unwrap_or_else(|| Frame::new(Combine::Single))
    }

    /// Hand a finished node's positions and translation to the innermost
    /// open container, charging an alternation's merge of a class branch.
    fn finished(&mut self, positions: u64, translated: Translated) {
        let Some(frame) = self.frames.last_mut() else {
            return;
        };
        let positions = match frame.combine {
            // Each branch is a path out of the alternation's split, which
            // the fallback engines follow at every step even when the
            // branch holds nothing.
            Combine::Alternation => positions.max(1),
            Combine::Concat | Combine::Single => positions,
        };
        frame.positions = frame.positions.saturating_add(positions);
        let mut merge = 0;
        frame.translated = match (frame.combine, frame.translated, translated) {
            (Combine::Concat, current, Translated::Empty) => current,
            (Combine::Single, _, child) | (Combine::Concat, Translated::Empty, child) => child,
            (Combine::Alternation, current, Translated::Class(ranges)) => {
                frame.merged = frame.merged.saturating_add(ranges);
                merge = frame.merged;
                match current {
                    Translated::Class(_) => Translated::Class(frame.merged),
                    Translated::Empty | Translated::Other => Translated::Other,
                }
            }
            (Combine::Concat, _, _)
            | (Combine::Alternation, _, Translated::Empty | Translated::Other) => Translated::Other,
        };
        self.charge(merge.saturating_mul(RANGE));
    }

    /// Finish an atom: one position, translated to a class of `ranges`.
    fn atom(&mut self, ranges: u64) {
        self.finished(1, Translated::Class(ranges));
    }

    /// How many values a class can cover under the current flags.
    fn universe(&self) -> u64 {
        if self.flags.unicode {
            CODEPOINTS
        } else {
            BYTES
        }
    }

    /// Charge a simple case fold of `set` and return the folded bound. The
    /// fold adds only code points that fold, so it grows a class by at most
    /// [`FOLD_MAPPED_MAX`] of them, each a range of its own.
    fn fold(&mut self, set: Set) -> Set {
        let mapped = set.span.min(FOLD_MAPPED_MAX);
        self.charge(
            FOLD_RANGE
                .saturating_mul(set.ranges)
                .saturating_add(FOLD_CODEPOINT.saturating_mul(set.span))
                .saturating_add(FOLD_MAPPED.saturating_mul(mapped)),
        );
        let added = set.span.saturating_mul(FOLD_PEERS_MAX).min(FOLD_MAPPED_MAX);
        Set {
            ranges: set.ranges.saturating_add(added),
            span: set.span.saturating_add(added).min(self.universe()),
        }
    }

    /// Charge a negation of `set` and return its complement's bound, which
    /// covers every value.
    fn complement(&mut self, set: Set) -> Set {
        self.pass(set);
        Set {
            ranges: set.ranges.saturating_add(1),
            span: self.universe(),
        }
    }

    /// The translator's fold-then-negate step: under `(?i)` it folds the
    /// positive side, and then it negates.
    fn fold_and_negate(&mut self, set: Set, negated: bool) -> Set {
        let set = if self.flags.case_insensitive {
            self.fold(set)
        } else {
            set
        };
        if negated { self.complement(set) } else { set }
    }

    /// A Perl class: its table's ranges, negated without a fold.
    fn perl(&mut self, class: &ast::ClassPerl) -> Set {
        let set = match (self.flags.unicode, &class.kind) {
            (true, ast::ClassPerlKind::Word) => PERL_WORD,
            (true, ast::ClassPerlKind::Digit) => PERL_DIGIT,
            (true, ast::ClassPerlKind::Space) => PERL_SPACE,
            (false, ast::ClassPerlKind::Word) => BYTE_WORD,
            (false, ast::ClassPerlKind::Digit) => BYTE_DIGIT,
            (false, ast::ClassPerlKind::Space) => BYTE_SPACE,
        };
        self.pass(set);
        if class.negated {
            self.complement(set)
        } else {
            set
        }
    }

    /// A `\p{…}` class at its largest shape, folded and then negated.
    fn property(&mut self, class: &ast::ClassUnicode) -> Set {
        self.pass(UNICODE_PROPERTY);
        self.fold_and_negate(UNICODE_PROPERTY, class.is_negated())
    }

    /// Union an item into the innermost open accumulator.
    fn union(&mut self, item: Set) {
        let universe = self.universe();
        let Some(acc) = self.classes.last_mut() else {
            return;
        };
        acc.ranges = acc.ranges.saturating_add(item.ranges);
        acc.span = acc.span.saturating_add(item.span).min(universe);
        let ranges = acc.ranges;
        self.charge(ranges.saturating_mul(RANGE));
    }
}

/// How many times a repeat can match its operand, with an unbounded repeat
/// counted as its lower bound plus one. `?` matches at most once, `*` is
/// `{0,}` and `+` is `{1,}`.
fn upper_bound(kind: &ast::RepetitionKind) -> u64 {
    match kind {
        ast::RepetitionKind::ZeroOrOne | ast::RepetitionKind::ZeroOrMore => 1,
        ast::RepetitionKind::OneOrMore => 2,
        ast::RepetitionKind::Range(
            ast::RepetitionRange::Exactly(max) | ast::RepetitionRange::Bounded(_, max),
        ) => u64::from(*max),
        ast::RepetitionKind::Range(ast::RepetitionRange::AtLeast(min)) => {
            u64::from(*min).saturating_add(1)
        }
    }
}

/// What a repeat translates to: `Hir::repetition` turns `{0}` into the
/// empty match and `{1}` into its operand. A repeat of an empty match is
/// empty or a repeat of nothing, and the walk calls both empty.
fn repeated(kind: &ast::RepetitionKind, operand: Translated) -> Translated {
    use ast::RepetitionRange::{Bounded, Exactly};
    match (kind, operand) {
        (_, Translated::Empty) | (ast::RepetitionKind::Range(Exactly(0) | Bounded(0, 0)), _) => {
            Translated::Empty
        }
        (ast::RepetitionKind::Range(Exactly(1) | Bounded(1, 1)), operand) => operand,
        _ => Translated::Other,
    }
}

fn in_fold_window(c: char) -> bool {
    (FOLD_WINDOW.0..=FOLD_WINDOW.1).contains(&u32::from(c))
}

impl Visitor for Walk {
    type Output = Shape;
    type Err = Infallible;

    fn finish(self) -> Result<Shape, Infallible> {
        Ok(Shape {
            units: self.units,
            positions: self.frames.first().map_or(0, |frame| frame.positions),
            captures: self.captures,
        })
    }

    fn visit_pre(&mut self, ast: &Ast) -> Result<(), Infallible> {
        match ast {
            Ast::Group(group) => {
                self.saved.push(self.flags);
                match &group.kind {
                    ast::GroupKind::CaptureName { name, .. } => self.captures.push(Capture {
                        name: name.name.clone(),
                        index: name.index,
                    }),
                    ast::GroupKind::NonCapturing(flags) => self.flags.apply(flags),
                    ast::GroupKind::CaptureIndex(_) => {}
                }
                self.open(Combine::Single);
            }
            Ast::Repetition(_) => self.open(Combine::Single),
            Ast::Concat(_) => self.open(Combine::Concat),
            Ast::Alternation(_) => self.open(Combine::Alternation),
            Ast::ClassBracketed(_) => self.classes.push(Set::default()),
            Ast::Empty(_)
            | Ast::Flags(_)
            | Ast::Literal(_)
            | Ast::Dot(_)
            | Ast::Assertion(_)
            | Ast::ClassUnicode(_)
            | Ast::ClassPerl(_) => {}
        }
        Ok(())
    }

    fn visit_post(&mut self, ast: &Ast) -> Result<(), Infallible> {
        match ast {
            // Free in a concatenation, which drops it: the cost harness's
            // zero-charge family. As an alternation branch or a repeat's
            // operand it still counts one, which its container charges.
            Ast::Empty(_) => self.finished(0, Translated::Empty),
            // Compiles to nothing: the cost harness's zero-charge family.
            Ast::Flags(set) => {
                self.flags.apply(&set.flags);
                self.finished(0, Translated::Empty);
            }
            // A look-around instruction, checked on every live step.
            Ast::Assertion(_) => self.finished(1, Translated::Other),
            Ast::Literal(literal) => {
                if self.flags.case_insensitive && in_fold_window(literal.c) {
                    self.charge(FOLDED_LITERAL);
                    self.atom(1 + FOLD_PEERS_MAX);
                } else {
                    self.atom(1);
                }
            }
            Ast::Dot(_) => {
                self.charge(DOT_RANGES.saturating_mul(RANGE));
                self.atom(DOT_RANGES);
            }
            Ast::ClassPerl(class) => {
                let set = self.perl(class);
                self.atom(set.ranges);
            }
            Ast::ClassUnicode(class) => {
                let set = self.property(class);
                self.atom(set.ranges);
            }
            Ast::ClassBracketed(class) => {
                let set = self.classes.pop().unwrap_or_default();
                let set = self.fold_and_negate(set, class.negated);
                self.atom(set.ranges);
            }
            Ast::Repetition(repetition) => {
                let inner = self.close();
                // A repeat compiles to a split around its operand, even an
                // operand that holds nothing, so the operand counts one.
                // The split is a node of its own, so the repeat counts at
                // least one more than its operand: otherwise wrappers that
                // repeat at most once, `(?:…)?` or `(?:…)*`, nest for free.
                let operand = inner.positions.max(1);
                self.finished(
                    operand
                        .saturating_mul(upper_bound(&repetition.op.kind))
                        .max(operand.saturating_add(1)),
                    repeated(&repetition.op.kind, inner.translated),
                );
            }
            Ast::Group(group) => {
                if let Some(flags) = self.saved.pop() {
                    self.flags = flags;
                }
                let inner = self.close();
                let (own, translated) = match &group.kind {
                    // Two capture instructions, which the fallback engines
                    // step through like any other.
                    ast::GroupKind::CaptureIndex(_) | ast::GroupKind::CaptureName { .. } => {
                        (1, Translated::Other)
                    }
                    // Compiles to its child alone: the cost harness's
                    // zero-charge family.
                    ast::GroupKind::NonCapturing(_) => (0, inner.translated),
                };
                self.finished(inner.positions.saturating_add(own), translated);
            }
            Ast::Concat(_) | Ast::Alternation(_) => {
                let inner = self.close();
                self.finished(inner.positions, inner.translated);
            }
        }
        Ok(())
    }

    fn visit_class_set_item_pre(&mut self, item: &ast::ClassSetItem) -> Result<(), Infallible> {
        if let ast::ClassSetItem::Bracketed(_) = item {
            self.classes.push(Set::default());
        }
        Ok(())
    }

    fn visit_class_set_item_post(&mut self, item: &ast::ClassSetItem) -> Result<(), Infallible> {
        let set = match item {
            ast::ClassSetItem::Empty(_) | ast::ClassSetItem::Union(_) => return Ok(()),
            ast::ClassSetItem::Literal(_) => Set { ranges: 1, span: 1 },
            ast::ClassSetItem::Range(range) => Set {
                ranges: 1,
                span: u64::from(u32::from(range.end.c))
                    .saturating_sub(u64::from(u32::from(range.start.c)))
                    .saturating_add(1),
            },
            ast::ClassSetItem::Ascii(class) => {
                self.pass(ASCII_CLASS);
                self.fold_and_negate(ASCII_CLASS, class.negated)
            }
            ast::ClassSetItem::Unicode(class) => self.property(class),
            ast::ClassSetItem::Perl(class) => self.perl(class),
            ast::ClassSetItem::Bracketed(class) => {
                let set = self.classes.pop().unwrap_or_default();
                self.fold_and_negate(set, class.negated)
            }
        };
        self.union(set);
        Ok(())
    }

    fn visit_class_set_binary_op_pre(
        &mut self,
        _op: &ast::ClassSetBinaryOp,
    ) -> Result<(), Infallible> {
        self.classes.push(Set::default());
        Ok(())
    }

    fn visit_class_set_binary_op_in(
        &mut self,
        _op: &ast::ClassSetBinaryOp,
    ) -> Result<(), Infallible> {
        self.classes.push(Set::default());
        Ok(())
    }

    fn visit_class_set_binary_op_post(
        &mut self,
        op: &ast::ClassSetBinaryOp,
    ) -> Result<(), Infallible> {
        let rhs = self.classes.pop().unwrap_or_default();
        let lhs = self.classes.pop().unwrap_or_default();
        let (lhs, rhs) = if self.flags.case_insensitive {
            (self.fold(lhs), self.fold(rhs))
        } else {
            (lhs, rhs)
        };
        let ranges = lhs.ranges.saturating_add(rhs.ranges);
        self.charge(ranges.saturating_mul(RANGE));
        let span = match op.kind {
            ast::ClassSetBinaryOpKind::Intersection => lhs.span.min(rhs.span),
            ast::ClassSetBinaryOpKind::Difference => lhs.span,
            ast::ClassSetBinaryOpKind::SymmetricDifference => {
                lhs.span.saturating_add(rhs.span).min(self.universe())
            }
        };
        self.union(Set { ranges, span });
        Ok(())
    }
}

/// The patterns ADR-0063 places on each side of the budgets, with the
/// verdict each must get. Timing them needs a clock, which no code under
/// `src/` may read (`tests/now_anchor_contract.rs`), so the timed report
/// that measured them is not in this crate.
#[cfg(test)]
pub(crate) mod calibration {
    use super::{MAX_REPEAT_POSITIONS, MAX_TRANSLATION_UNITS, Shape};

    /// Which budget, if either, refuses a query that holds `copies` of one
    /// pattern.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Verdict {
        Admitted,
        RefusedByTranslation,
        RefusedByRepeat,
    }

    pub(crate) fn verdict(shape: &Shape, copies: u64) -> Verdict {
        if shape.positions > MAX_REPEAT_POSITIONS {
            Verdict::RefusedByRepeat
        } else if shape.units.saturating_mul(copies) > MAX_TRANSLATION_UNITS {
            Verdict::RefusedByTranslation
        } else {
            Verdict::Admitted
        }
    }

    /// One calibration case: a query of `copies` of `body`, and the verdict
    /// ADR-0063 requires of it.
    pub(crate) struct Case {
        pub(crate) name: &'static str,
        pub(crate) body: String,
        pub(crate) copies: u64,
        pub(crate) expect: Verdict,
    }

    fn case(name: &'static str, body: impl Into<String>, copies: u64, expect: Verdict) -> Case {
        Case {
            name,
            body: body.into(),
            copies,
            expect,
        }
    }

    const SYSLOG: &str = r"(?i)^\w{3}\s+\d+\s+[\d:]+\s+(\S+)\s+(\w+)\[(\d+)\]";
    const EMAIL: &str = r"(?i)[a-z0-9._%+-]+@[a-z0-9.-]+\.[a-z]{2,}";
    const UUID: &str = r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}";

    /// The `log_regexes` corpus the parser holds under the compile limit.
    const LOG_REGEXES: &[&str] = &[
        r"\w{10}",
        r"\w+@\w+\.\w{2,10}",
        r"[\w.+-]+@[\w-]+\.[\w.]{2,10}",
        r"[A-Za-z0-9_]{3,30}",
        r"\d{1,3}(\.\d{1,3}){3}",
        r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})",
        r"([0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2}",
        UUID,
        r"(?i)(error|fail(ed|ure)?|exception|timeout|refused|denied)",
        r#"(?P<key>\w+)=(?P<val>"[^"]*"|\S+)"#,
        r"^(?P<ip>\d{1,3}(\.\d{1,3}){3}) - (?P<user>\S+) \[(?P<ts>[^\]]+)\] .(?P<method>[A-Z]+) (?P<path>\S+) HTTP\d\.\d. (?P<status>\d{3}) (?P<bytes>\d+|-)",
        r"(\w+\s+){5}\w+",
    ];

    /// Every regex body in `docs/src/content/docs/reference/dsl.md`.
    const DSL_MD_REGEXES: &[&str] = &[
        "timeout|refused",
        "(?i)timeout",
        "connection refused",
        "err|warn",
        "err| warn",
        "a,b",
        "c",
        "^true$",
        "^200$",
        r"^200\.0$",
        "re",
        "debug",
        "prod-.*",
        "a#b",
        "api",
        "timeout",
        "error.*",
        "a",
        r"(?P<ip>\d+\.\d+\.\d+\.\d+)",
        "(?P<code>[A-Z]+)",
        r"\w{20}",
        "[0-9a-f]{64}",
        r"\w{3,30}",
        r"[A-Za-z0-9_]{3,30}",
        r"\w+",
    ];

    /// The cases ADR-0063 names, and the ordinary patterns that must stay
    /// admitted.
    pub(crate) fn calibration_cases() -> Vec<Case> {
        use Verdict::{Admitted, RefusedByRepeat, RefusedByTranslation};
        let mut cases = vec![
            // A1 and A2 were measured at 145 and 170 classes a body. The
            // repeat budget now refuses those bodies, so each attack is as
            // many classes as it admits, and one body alone is over the
            // translation budget.
            case(
                "A1 (?i) + 64 \\p{Any}",
                format!("(?i){}", r"\p{Any}".repeat(64)),
                63,
                RefusedByTranslation,
            ),
            case(
                "A2 (?i) + 64 \\p{Ll}",
                format!("(?i){}", r"\p{Ll}".repeat(64)),
                63,
                RefusedByTranslation,
            ),
            // A3, 64 KiB of `\w`, no longer reaches the translation budget.
            // It was measured as 127 bodies of 256 `\w`, which the repeat
            // budget now refuses, like A3'. The pattern cap holds 256
            // bodies of 64, which price at about 13 M units. Each of them
            // compiles past the size limit, and construction stops at the
            // first.
            case("A3 64 \\w, 256 bodies", r"\w".repeat(64), 256, Admitted),
            case(
                "A3' 512 \\w, 63 bodies",
                r"\w".repeat(512),
                63,
                RefusedByRepeat,
            ),
            case(
                "A4 (?i) + 60 full-range brackets",
                format!("(?i){}", r"[\x00-\x{10FFFF}]".repeat(60)),
                63,
                RefusedByTranslation,
            ),
            case(
                "A5 (?i) + 40 [^\\x00-\\x{10FFFE}]",
                format!("(?i){}", r"[^\x00-\x{10FFFE}]".repeat(40)),
                63,
                RefusedByTranslation,
            ),
            case(
                "X8 (?i) + 64 [a[^b]]",
                format!("(?i){}", "[a[^b]]".repeat(64)),
                1,
                RefusedByTranslation,
            ),
            case("syslog", SYSLOG, 1, Admitted),
            case("syslog x256", SYSLOG, 256, Admitted),
            case("email x256", EMAIL, 256, Admitted),
            case(".{1000}", ".{1000}", 1, RefusedByRepeat),
            case("\\d{1000}", r"\d{1000}", 1, RefusedByRepeat),
            case(
                "(?:a|b)[abc]{1000}(?:a|d)d",
                "(?:a|b)[abc]{1000}(?:a|d)d",
                1,
                RefusedByRepeat,
            ),
            case("[A-Za-z]{1000}", "[A-Za-z]{1000}", 1, RefusedByRepeat),
            case(
                "(?:a|b)[abc]{30}(?:a|d)d",
                "(?:a|b)[abc]{30}(?:a|d)d",
                1,
                Admitted,
            ),
            case(
                "\\d{1,3}(\\.\\d{1,3}){3}",
                r"\d{1,3}(\.\d{1,3}){3}",
                1,
                Admitted,
            ),
            case("uuid", UUID, 1, Admitted),
            case("[0-9a-f]{64}", "[0-9a-f]{64}", 1, Admitted),
            case("64 literals", "a".repeat(64), 1, Admitted),
            case("65 literals", "a".repeat(65), 1, RefusedByRepeat),
            case(
                "worst admitted: (?i) + 14 full-range brackets",
                format!("(?i){}", r"[\x00-\x{10FFFF}]".repeat(14)),
                1,
                Admitted,
            ),
            case(
                "(?i) + 15 full-range brackets",
                format!("(?i){}", r"[\x00-\x{10FFFF}]".repeat(15)),
                1,
                RefusedByTranslation,
            ),
        ];
        for body in LOG_REGEXES {
            cases.push(case("log_regexes", *body, 1, Admitted));
        }
        for body in DSL_MD_REGEXES {
            cases.push(case("dsl.md", *body, 1, Admitted));
        }
        cases
    }
}

#[cfg(test)]
pub(crate) use tests::fold_site_inventory_check;

#[cfg(test)]
mod tests {
    use regex_syntax::hir::{self, ClassUnicode, ClassUnicodeRange};

    use super::*;

    fn price(body: &str) -> Shape {
        shape(
            &ast::parse::Parser::new()
                .parse(body)
                .expect("the body parses"),
        )
    }

    fn units(body: &str) -> u64 {
        price(body).units
    }

    fn positions(body: &str) -> u64 {
        price(body).positions
    }

    /// A fold of a `\p{…}` class's largest shape: `FOLD_RANGE·1024 +
    /// FOLD_CODEPOINT·0x110000 + FOLD_MAPPED·2938`.
    const PROPERTY_FOLD: u64 = 16 * 1024 + 0x11_0000 + 6 * 2938;

    #[test]
    fn a_case_insensitive_literal_inside_the_fold_window_costs_a_fold() {
        assert_eq!(units("abc"), 0);
        assert_eq!(units("(?i)abc"), 3 * FOLDED_LITERAL);
        assert_eq!(
            units("(?i)A"),
            FOLDED_LITERAL,
            "the window's first code point"
        );
        assert_eq!(units("(?i)@"), 0, "U+0040, just before the window");
        assert_eq!(units("(?i)1"), 0);
        assert_eq!(
            units(r"(?i)\x{1E943}"),
            FOLDED_LITERAL,
            "the window's last code point"
        );
        assert_eq!(units(r"(?i)\x{1E944}"), 0, "just past the window");
    }

    #[test]
    fn a_dot_costs_its_ranges() {
        assert_eq!(units("."), DOT_RANGES);
        assert_eq!(units("(?s)."), DOT_RANGES);
    }

    #[test]
    fn a_perl_class_costs_its_ranges_and_never_folds() {
        assert_eq!(units(r"\w"), 796);
        assert_eq!(units(r"\d"), 71);
        assert_eq!(units(r"\s"), 10);
        assert_eq!(units(r"\W"), 2 * 796, "a negation passes over them again");
        assert_eq!(units(r"(?i)\w"), units(r"\w"));
        assert_eq!(units(r"(?i)\W"), units(r"\W"));
        assert_eq!(units(r"(?-u)\w"), 4, "an ASCII class under (?-u)");
        assert_eq!(units(r"(?-u)\s"), 6);
    }

    #[test]
    fn a_property_class_costs_its_largest_shape() {
        assert_eq!(units(r"\pL"), 1024);
        assert_eq!(units(r"\p{Greek}"), 1024);
        assert_eq!(units(r"\PL"), 2 * 1024);
        assert_eq!(units(r"\p{gc!=L}"), units(r"\PL"), "!= negates");
        assert_eq!(units(r"(?i)\pL"), 1024 + PROPERTY_FOLD);
        // The negation after the fold passes over the folded bound: 1024
        // ranges and at most 2938 more.
        assert_eq!(units(r"(?i)\PL"), 1024 + PROPERTY_FOLD + 1024 + 2938);
    }

    #[test]
    fn an_unknown_property_name_is_charged_the_maximum() {
        for body in [r"\p{NoSuchProperty}", r"\p{Script=NoSuchScript}", r"\pQ"] {
            assert_eq!(units(body), units(r"\p{Any}"), "{body}");
            assert_eq!(
                units(&format!("(?i){body}")),
                1024 + PROPERTY_FOLD,
                "{body}"
            );
        }
    }

    #[test]
    fn a_bracket_item_costs_the_accumulator_and_its_own_ranges() {
        assert_eq!(units("[a]"), 1);
        assert_eq!(units("[a-z]"), 1);
        assert_eq!(units("[abc]"), 1 + 2 + 3);
        assert_eq!(units(r"[\w]"), 796 + 796, "materialise, then union");
        assert_eq!(units(r"[a\w]"), 1 + 796 + (1 + 796));
        assert_eq!(units("[[:alpha:]]"), 6 + 6);
        assert_eq!(
            units("[^a]"),
            1 + 1,
            "a negation passes over the ranges again"
        );
    }

    #[test]
    fn a_case_insensitive_bracket_folds_its_accumulator() {
        // fold(1 range, 26 code points) = 16 + 26 + 6·26.
        assert_eq!(units("(?i)[a-z]"), 1 + 16 + 26 + 6 * 26);
        // The translator steps through every code point of the range.
        assert_eq!(
            units(r"(?i)[\x00-\x{10FFFF}]"),
            1 + 16 + 0x11_0000 + 6 * 2938
        );
        // A negated bracket folds its positive side, then negates the
        // folded bound, so a complement of one code point pays in full.
        assert_eq!(
            units(r"(?i)[^\x00-\x{10FFFE}]"),
            1 + 16 + 0x10_FFFF + 6 * 2938 + (1 + 2938)
        );
        // The bracket folds a Perl class item, though the class alone never
        // folds.
        assert_eq!(units(r"(?i)[\w]"), 2 * 796 + 16 * 796 + 144_667 + 6 * 2938);
    }

    #[test]
    fn a_negated_nested_class_widens_to_every_code_point() {
        // [^b] folds {b} and negates it. Its complement covers every code
        // point, so the (?i) bracket around it steps through all of them.
        assert!(units("(?i)[a[^b]]") > 0x11_0000);
        assert!(units(r"(?i)[a\W]") > 0x11_0000);
        assert!(units("(?i)[a[:^alpha:]]") > 0x11_0000);
        assert!(
            units(r"(?i)[a\PL]") > 2 * 0x11_0000,
            "the item folds, then the bracket refolds"
        );
        assert!(units("[a[^b]]") < 100, "without (?i) nothing refolds");
        assert!(
            units("(?-u)(?i)[a[^b]]") < 2_000,
            "a byte class covers 256 values"
        );
    }

    #[test]
    fn a_set_operation_charges_both_operands() {
        // lhs \w 796 + 796, rhs \d 71 + 71, the operation 867, its union 867.
        assert_eq!(units(r"[\w&&\d]"), 2 * 796 + 2 * 71 + 2 * 867);
        // Under (?i) the operation folds each operand, so a negated operand
        // pays a fold of every code point on either side.
        assert!(units("(?i)[[^b]&&a-z]") > 0x11_0000);
        assert!(units("(?i)[a-z&&[^b]]") > 0x11_0000);
        assert!(units("(?i)[a-z--[^b]]") > 0x11_0000);
        assert!(
            units("(?i)[[^b]~~[^c]]") > 2 * 0x11_0000,
            "both operands fold"
        );
        assert!(units("[a-z&&[^b]]") < 100);
    }

    #[test]
    fn flags_scope_like_the_translator() {
        assert_eq!(
            units("(?i:a)b"),
            FOLDED_LITERAL,
            "a group's flags end with it"
        );
        assert_eq!(units("a(?i)b"), FOLDED_LITERAL);
        assert_eq!(units("(?i)a(?-i)b"), FOLDED_LITERAL);
        assert_eq!(
            units("((?i)a)b"),
            FOLDED_LITERAL,
            "a bare flag ends with its group"
        );
        // Two-literal branches are strings, not classes, so nothing merges.
        assert_eq!(
            units("(?i)ab|cd"),
            4 * FOLDED_LITERAL,
            "a bare flag crosses the branches after it"
        );
        assert_eq!(units("(?:(?i)ab|cd)ef"), 4 * FOLDED_LITERAL);
        assert_eq!(units("(?-i:(?i)a)b"), FOLDED_LITERAL);
        assert_eq!(
            units("(?ims-u)a"),
            FOLDED_LITERAL,
            "other flags change nothing"
        );
        assert_eq!(units(r"(?-u:\w)\w"), 4 + 796, "u scopes like i");
        assert_eq!(units(r"(?-u)(?u)\w"), 796);
    }

    #[test]
    fn an_alternation_of_classes_charges_each_merge() {
        // \w then \d: materialise both, then merge 796 and 796 + 71.
        assert_eq!(units(r"\w|\d"), 796 + 71 + 796 + 867);
        assert_eq!(
            units(r"(?:\w)|\d"),
            units(r"\w|\d"),
            "a plain group is its child"
        );
        assert_eq!(units(r"\w{1}|\d"), units(r"\w|\d"), "{{1}} is its operand");
        assert_eq!(
            units(r"(?:)\w|\d"),
            units(r"\w|\d"),
            "an empty node drops out"
        );
        assert_eq!(units("a|b"), 1 + 2, "single characters merge too");
        // A capture group, a repeat and a string are not classes, so only
        // the class branches merge.
        assert_eq!(units(r"\w|(\d)"), 796 + 71 + 796);
        assert_eq!(units(r"\w|\d{2}"), 796 + 71 + 796);
        assert_eq!(units(r"\w|\d\d"), 796 + 2 * 71 + 796);
        assert_eq!(units(r"\w|^"), 796 + 796);
        // An inner alternation merges into one class, which the outer one
        // merges again.
        assert_eq!(
            units(r"(?:\w|\d)|\s"),
            796 + 71 + 10 + (796 + 867) + (867 + 877)
        );
    }

    #[test]
    fn every_atom_counts_one_position() {
        for body in ["a", ".", r"\w", r"\pL", "[abc]", r"[\w\d]", r"\x{1F600}"] {
            assert_eq!(positions(body), 1, "{body}");
        }
        assert_eq!(positions("abc"), 3);
    }

    /// Each `Ast` variant and each `GroupKind`, with the positions it adds
    /// on its own. A node that compiles to an instruction counts at least
    /// one. Only flags and empty nodes in a concatenation count nothing.
    const NODE_CHARGES: &[(&str, &str, u64)] = &[
        ("Ast::Empty in a concatenation", "(?:)", 0),
        ("Ast::Flags", "(?i)", 0),
        ("Ast::Literal", "a", 1),
        ("Ast::Dot", ".", 1),
        ("Ast::Assertion", r"\B", 1),
        ("Ast::ClassUnicode", r"\pL", 1),
        ("Ast::ClassPerl", r"\d", 1),
        ("Ast::ClassBracketed", "[ab]", 1),
        ("Ast::Repetition", "a{3}", 3),
        ("Ast::Repetition at most once", "a?", 2),
        ("Ast::Repetition of nothing", "(?:)?", 2),
        ("GroupKind::CaptureIndex", "()", 1),
        ("GroupKind::CaptureIndex around an atom", "(a)", 2),
        ("GroupKind::CaptureName", "(?P<x>)", 1),
        ("GroupKind::CaptureName around an atom", "(?<y>a)", 2),
        ("GroupKind::NonCapturing", "(?i:)", 0),
        ("GroupKind::NonCapturing around an atom", "(?:a)", 1),
        ("Ast::Alternation", "a|b", 2),
        ("Ast::Alternation with an empty branch", "a|", 2),
        ("Ast::Alternation of empty branches", "|", 2),
        ("Ast::Concat", "ab", 2),
    ];

    #[test]
    fn every_node_kind_counts_its_positions() {
        for (node, body, charge) in NODE_CHARGES {
            assert_eq!(positions(body), *charge, "{node}: {body}");
        }
        // One pattern that holds every kind at once adds them up. Each
        // sits in a non-capturing group of its own, which adds nothing.
        let all = NODE_CHARGES
            .iter()
            .fold(String::new(), |all, (_, body, _)| all + "(?:" + body + ")");
        let sum: u64 = NODE_CHARGES.iter().map(|(_, _, charge)| charge).sum();
        assert_eq!(positions(&all), sum, "{all}");
    }

    #[test]
    fn every_assertion_counts_one_position() {
        for body in [
            "^",
            "$",
            r"\A",
            r"\z",
            r"\b",
            r"\B",
            r"\b{start}",
            r"\b{end}",
            r"\<",
            r"\>",
            r"\b{start-half}",
            r"\b{end-half}",
            "(?m)^",
            "(?m)$",
        ] {
            assert_eq!(positions(body), 1, "{body}");
        }
        assert_eq!(positions(r"^$\b\B\A\z"), 6);
        // A repeat multiplies an assertion like any atom.
        assert_eq!(positions(r"(?:\B[abc]){62}"), 124);
        assert_eq!(
            positions(&format!("𐐨(?:{}[𐐨𐐩𐐪]){{62}}𐐪", r"\B".repeat(8))),
            560
        );
    }

    #[test]
    fn every_capture_group_counts_one_position() {
        assert_eq!(positions("()()()"), 3);
        assert_eq!(positions("((((a))))"), 5);
        assert_eq!(positions("(?:(a)){3}"), 6);
        assert_eq!(
            positions(&format!(r"\B{}𐐨[𐐨𐐩𐐪]{{30}}𐐪", "()".repeat(490))),
            523
        );
    }

    #[test]
    fn an_empty_branch_or_repeat_operand_counts_one_position() {
        assert_eq!(positions("a|"), 2);
        assert_eq!(positions("(?:|)"), 2);
        assert_eq!(positions("(?:||||)"), 5);
        assert_eq!(positions("(?:(?i)|a)"), 2, "a branch of flags is empty");
        // The repeat around the empty operand counts one more.
        assert_eq!(positions("(?:)*"), 2);
        assert_eq!(positions("(?:)?"), 2);
        assert_eq!(positions("(?:){0,9}"), 9);
        assert_eq!(positions("(?:(?:)?[abc]){62}"), 186);
    }

    #[test]
    fn flags_whitespace_and_empty_groups_count_nothing() {
        for body in ["(?i)", "(?:)", "(?i:)", "(?i)(?-i)", "(?:(?:)(?:))", ""] {
            assert_eq!(positions(body), 0, "{body}");
        }
        assert_eq!(positions("(?x) a  b  # a comment"), 2);
        assert_eq!(positions("(?:(?:)(?i)(?-i)(?i:)[abc]){62}"), 62);
    }

    #[test]
    fn concatenation_and_alternation_add() {
        assert_eq!(positions("a|b|c"), 3);
        assert_eq!(positions("ab|cde"), 5);
        assert_eq!(positions("(abc)(?:de)(?P<x>f)"), 8);
        assert_eq!(positions("(?:a|b)[abc](?:a|d)d"), 6);
    }

    #[test]
    fn a_counted_repeat_multiplies_by_its_upper_bound() {
        assert_eq!(positions("a{5}"), 5);
        assert_eq!(positions("a{2,7}"), 7);
        assert_eq!(positions("a{2,7}?"), 7);
        assert_eq!(positions("(?:ab){3}"), 6);
        assert_eq!(positions("(?:a{2}b){3}"), 9);
        assert_eq!(positions(r"\d{1,3}(?:\.\d{1,3}){3}"), 15);
        assert_eq!(
            positions(r"\d{1,3}(\.\d{1,3}){3}"),
            18,
            "the group captures"
        );
        assert_eq!(positions("(?:a|b)[abc]{1000}(?:a|d)d"), 1005);
    }

    #[test]
    fn an_unbounded_repeat_counts_its_lower_bound_plus_one() {
        assert_eq!(positions("a+"), 2);
        assert_eq!(positions("a{3,}"), 4);
        assert_eq!(positions("(?:ab)+"), 4);
        assert_eq!(positions("(?:a+)+"), 4);
        // `*` is `{0,}`: one times its operand, and then one more for
        // the repeat itself.
        assert_eq!(positions("a*"), 2);
        assert_eq!(positions("a{0,}"), 2);
    }

    /// The split a repeat compiles to is a node of its own, so a repeat
    /// counts at least one more than its operand, whatever its bounds and
    /// greed. Wrappers that repeat at most once, or not at all, cannot
    /// nest for free: each adds one.
    #[test]
    fn a_repeat_counts_at_least_one_more_than_its_operand() {
        for body in [
            "a?", "a??", "a*", "a*?", "a+", "a+?", "a{0,1}", "a{0,1}?", "a{1}", "a{1,1}", "a{0}",
            "a{0,0}", "a{0,}", "a{1,}",
        ] {
            assert_eq!(positions(body), 2, "{body}");
        }
        assert_eq!(positions("(?:ab)?"), 3);
        assert_eq!(positions("(?:(?:(?:a)?)?)?"), 4);
        assert_eq!(positions("(?:(?:(?:a)*)*)*"), 4);
        assert_eq!(positions("(?:(?:(?:a)*?)??){0,1}"), 4);
        assert_eq!(positions("(?:(?:a)?){31}"), 62);
        assert_eq!(positions(r"\B(?:(?:(?:[abc])*)*){20}c"), 62);
        // A count of two or more already multiplies past one more.
        assert_eq!(positions("a{2}"), 2);
        assert_eq!(positions(r"\w{20}"), 20);
        assert_eq!(positions("[0-9a-f]{64}"), 64);
    }

    #[test]
    fn positions_and_units_saturate() {
        let max = u32::MAX;
        assert_eq!(
            positions(&format!("(?:(?:a{{{max}}}){{{max}}}){{{max}}}")),
            u64::MAX
        );
        assert_eq!(positions(&format!("a{{{max},}}")), u64::from(max) + 1);
        let mut walk = Walk::new();
        walk.charge(u64::MAX);
        walk.charge(1);
        assert_eq!(walk.units, u64::MAX);
        let mut walk = Walk::new();
        let folded = walk.fold(Set {
            ranges: u64::MAX,
            span: u64::MAX,
        });
        assert_eq!(walk.units, u64::MAX);
        assert_eq!(folded.ranges, u64::MAX);
        assert_eq!(folded.span, CODEPOINTS);
    }

    #[test]
    fn captures_carry_absolute_group_indices() {
        let named = |body: &str| -> Vec<(String, u32)> {
            captures(body)
                .expect("the body parses")
                .into_iter()
                .map(|c| (c.name, c.index))
                .collect()
        };
        let pairs = |list: &[(&str, u32)]| -> Vec<(String, u32)> {
            list.iter().map(|(n, i)| ((*n).to_string(), *i)).collect()
        };
        assert_eq!(
            named("(a|b)(?P<x>b)-(?P<y>c)"),
            pairs(&[("x", 2), ("y", 3)])
        );
        assert_eq!(
            named("(?P<a>(?P<b>x)(y))(?<c>z)"),
            pairs(&[("a", 1), ("b", 2), ("c", 4)])
        );
        assert_eq!(named("(a)(b)(?:c)"), pairs(&[]));
        let body = "(a)(?P<x>b)";
        assert_eq!(price(body).captures, captures(body).unwrap());
        assert!(captures("(").is_err());
        assert!(
            captures("(?P<x>a)(?P<x>b)").is_err(),
            "a duplicate name is a syntax error"
        );
    }

    /// The bound of a translated Unicode class.
    fn bound(class: &ClassUnicode) -> Set {
        Set {
            ranges: u64::try_from(class.ranges().len()).unwrap(),
            span: class
                .ranges()
                .iter()
                .map(|r| u64::from(u32::from(r.end()) - u32::from(r.start()) + 1))
                .sum(),
        }
    }

    /// What `body` translates to. The public API reaches the Unicode tables
    /// only through translation; the walk never translates.
    fn translated(body: &str) -> hir::Class {
        match regex_syntax::Parser::new()
            .parse(body)
            .expect("the body translates")
            .into_kind()
        {
            hir::HirKind::Class(class) => class,
            kind => panic!("{body} translates to {kind:?}"),
        }
    }

    fn unicode(body: &str) -> Set {
        match translated(body) {
            hir::Class::Unicode(class) => bound(&class),
            hir::Class::Bytes(_) => panic!("{body} is a byte class"),
        }
    }

    fn bytes(body: &str) -> Set {
        match translated(body) {
            hir::Class::Bytes(class) => Set {
                ranges: u64::try_from(class.ranges().len()).unwrap(),
                span: class
                    .ranges()
                    .iter()
                    .map(|r| u64::from(r.end() - r.start()) + 1)
                    .sum(),
            },
            hir::Class::Unicode(class) => bound(&class),
        }
    }

    /// Property classes with the most ranges among those `regex-syntax`
    /// knows, each held under [`UNICODE_PROPERTY`].
    const LARGE_PROPERTIES: &[&str] = &[
        "L",
        "Lu",
        "Ll",
        "Lo",
        "M",
        "Mn",
        "Mc",
        "N",
        "Nd",
        "No",
        "P",
        "Po",
        "S",
        "So",
        "Sm",
        "C",
        "Cn",
        "Cf",
        "Any",
        "Assigned",
        "Alphabetic",
        "ID_Start",
        "ID_Continue",
        "XID_Start",
        "XID_Continue",
        "Lowercase",
        "Uppercase",
        "Cased",
        "Case_Ignorable",
        "Changes_When_Casefolded",
        "Changes_When_Casemapped",
        "Changes_When_Lowercased",
        "Changes_When_Uppercased",
        "Changes_When_Titlecased",
        "Default_Ignorable_Code_Point",
        "Grapheme_Base",
        "Grapheme_Extend",
        "Diacritic",
        "Extender",
        "Math",
        "Emoji",
        "Extended_Pictographic",
        "Common",
        "Inherited",
        "Latin",
        "Han",
        "scx=Common",
        "scx=Inherited",
        "Age=V16_0",
        "gcb=Extend",
        "gcb=SpacingMark",
        "wb=ALetter",
        "wb=Extend",
        "wb=Format",
        "wb=Numeric",
        "sb=Lower",
        "sb=Upper",
        "sb=OLetter",
        "sb=Extend",
        "sb=Format",
        "sb=Close",
    ];

    /// The Unicode facts the charges use, re-derived through the public
    /// `regex-syntax` API, so a table bump fails here first.
    #[test]
    fn the_unicode_constants_match_the_pinned_tables() {
        assert_eq!(unicode(r"\w"), PERL_WORD);
        assert_eq!(unicode(r"\d"), PERL_DIGIT);
        assert_eq!(unicode(r"\s"), PERL_SPACE);
        for (body, most) in [
            (r"(?-u)\w", BYTE_WORD),
            (r"(?-u)\d", BYTE_DIGIT),
            (r"(?-u)\s", BYTE_SPACE),
        ] {
            let set = bytes(body);
            assert!(set.ranges <= most.ranges && set.span == most.span, "{body}");
        }
        for name in [
            "alnum", "alpha", "ascii", "blank", "cntrl", "digit", "graph", "lower", "print",
            "punct", "space", "upper", "word", "xdigit",
        ] {
            let set = unicode(&format!("[[:{name}:]]"));
            assert!(
                set.ranges <= ASCII_CLASS.ranges && set.span <= ASCII_CLASS.span,
                "[[:{name}:]]"
            );
        }
        let most = LARGE_PROPERTIES
            .iter()
            .map(|name| (unicode(&format!(r"\p{{{name}}}")).ranges, *name))
            .max()
            .unwrap();
        assert!(most.0 <= UNICODE_PROPERTY.ranges, "{most:?}");
        assert!(unicode(r"\p{Any}").span <= UNICODE_PROPERTY.span);
    }

    #[test]
    fn the_fold_constants_match_the_pinned_table() {
        let (mut first, mut last, mut mapped, mut peers) = (None, 0, 0, 0);
        for c in (0..=0x10_FFFF).filter_map(char::from_u32) {
            let mut class = ClassUnicode::new([ClassUnicodeRange::new(c, c)]);
            class.case_fold_simple();
            let others = bound(&class).span - 1;
            if others > 0 {
                first.get_or_insert(u32::from(c));
                last = u32::from(c);
                mapped += 1;
                peers = peers.max(others);
            }
        }
        assert_eq!((first, last), (Some(FOLD_WINDOW.0), FOLD_WINDOW.1));
        assert_eq!(mapped, FOLD_MAPPED_MAX);
        assert_eq!(peers, FOLD_PEERS_MAX);
    }

    #[test]
    fn fold_sites_are_charged() {
        super::fold_site_inventory_check();
    }

    /// The `regex-syntax` release whose translator the charges follow.
    const PINNED_REGEX_SYNTAX: &str = "0.8.11";

    /// FNV-1a 64 of that release's `src/hir/translate.rs`.
    const PINNED_TRANSLATE_RS: u64 = 0x3677_5f0f_d12a_f631;

    /// Which charge of the walk covers a case-fold site in the translator.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Charge {
        /// `visit_post` charges [`FOLDED_LITERAL`] for a `(?i)` literal
        /// inside [`FOLD_WINDOW`].
        Literal,
        /// `Walk::fold_and_negate` at a bracket's close, top-level or nested.
        Bracket,
        /// `Walk::property` folds a `\p{…}` class before it negates.
        Property,
        /// The `[[:name:]]` arm of `visit_class_set_item_post`.
        AsciiItem,
        /// `visit_class_set_binary_op_post` folds both operands.
        SetOperands,
        /// The translator's fold-then-negate helpers, which
        /// `Walk::fold_and_negate` mirrors. Their callers carry the charge.
        FoldThenNegate,
        /// Where the translator stores, reads or merges the `i` flag.
        /// `Flags::apply` scopes it the same way, and the owner never sets
        /// the builder's starting value.
        FlagScope,
    }

    /// Every case-fold site in the pinned `hir/translate.rs`, outside its
    /// tests: where it sits, the identifier that folds or reads the `i`
    /// flag, how many times it appears there, and the charge that covers it.
    const FOLD_SITES: &[(&str, &str, usize, Charge)] = &[
        (
            "fn case_insensitive",
            "case_insensitive",
            2,
            Charge::FlagScope,
        ),
        ("fn visit_post", "case_fold_char", 1, Charge::Literal),
        (
            "fn visit_post",
            "unicode_fold_and_negate",
            1,
            Charge::Bracket,
        ),
        ("fn visit_post", "bytes_fold_and_negate", 1, Charge::Bracket),
        (
            "fn visit_class_set_item_post",
            "unicode_fold_and_negate",
            1,
            Charge::Bracket,
        ),
        (
            "fn visit_class_set_item_post",
            "bytes_fold_and_negate",
            1,
            Charge::Bracket,
        ),
        (
            "fn visit_class_set_binary_op_post",
            "case_insensitive",
            2,
            Charge::SetOperands,
        ),
        (
            "fn visit_class_set_binary_op_post",
            "try_case_fold_simple",
            2,
            Charge::SetOperands,
        ),
        (
            "fn visit_class_set_binary_op_post",
            "case_fold_simple",
            2,
            Charge::SetOperands,
        ),
        ("fn case_fold_char", "case_insensitive", 1, Charge::Literal),
        ("fn case_fold_char", "SimpleCaseFolder", 1, Charge::Literal),
        (
            "fn case_fold_char",
            "try_case_fold_simple",
            1,
            Charge::Literal,
        ),
        ("fn case_fold_char", "case_fold_simple", 1, Charge::Literal),
        (
            "fn hir_unicode_class",
            "unicode_fold_and_negate",
            1,
            Charge::Property,
        ),
        (
            "fn hir_ascii_unicode_class",
            "unicode_fold_and_negate",
            1,
            Charge::AsciiItem,
        ),
        (
            "fn hir_ascii_byte_class",
            "bytes_fold_and_negate",
            1,
            Charge::AsciiItem,
        ),
        (
            "fn unicode_fold_and_negate",
            "case_insensitive",
            1,
            Charge::FoldThenNegate,
        ),
        (
            "fn unicode_fold_and_negate",
            "try_case_fold_simple",
            1,
            Charge::FoldThenNegate,
        ),
        (
            "fn bytes_fold_and_negate",
            "case_insensitive",
            1,
            Charge::FoldThenNegate,
        ),
        (
            "fn bytes_fold_and_negate",
            "case_fold_simple",
            1,
            Charge::FoldThenNegate,
        ),
        ("struct Flags", "case_insensitive", 1, Charge::FlagScope),
        ("fn from_ast", "case_insensitive", 1, Charge::FlagScope),
        ("fn merge", "case_insensitive", 3, Charge::FlagScope),
    ];

    /// Hold the translator's case-fold sites to [`FOLD_SITES`].
    ///
    /// Resolves `regex-syntax` through `cargo metadata`, as this crate
    /// depends on it, and reads that release's `src/hir/translate.rs`. Fails
    /// when the release, the file or its list of fold sites changes, so a
    /// `regex-syntax` bump stays red until someone reviews the walk's
    /// charges against the new translator and updates the pins.
    pub(crate) fn fold_site_inventory_check() {
        let (version, source) = translate_rs();
        assert_eq!(
            version, PINNED_REGEX_SYNTAX,
            "regex-syntax resolved to {version}: review pattern/price.rs's charges against \
             its translator, then update PINNED_REGEX_SYNTAX, PINNED_TRANSLATE_RS and \
             FOLD_SITES"
        );
        let expected: Vec<(String, String, usize)> = FOLD_SITES
            .iter()
            .map(|(at, token, count, _)| ((*at).to_string(), (*token).to_string(), *count))
            .collect();
        assert_eq!(
            fold_sites(&source),
            expected,
            "regex-syntax {version}'s translate.rs folds case at different sites: map each \
             one to the walk charge that covers it in FOLD_SITES"
        );
        let hash = fnv1a64(source.as_bytes());
        assert_eq!(
            hash, PINNED_TRANSLATE_RS,
            "regex-syntax {version}'s translate.rs changed ({hash:#x}): review it against \
             pattern/price.rs's charges, then update PINNED_TRANSLATE_RS"
        );
    }

    /// The resolved `regex-syntax` version this crate depends on, and the
    /// text of that release's `src/hir/translate.rs`.
    ///
    /// The resolve reads every locked crate's manifest, so on a bare checkout
    /// cargo fetches them first, as `trawl-mcp`'s dependency test does.
    fn translate_rs() -> (String, String) {
        use serde_json::Value;
        use std::path::Path;

        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
        let output = std::process::Command::new(cargo)
            .args(["metadata", "--format-version", "1", "--locked"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .expect("cargo metadata runs");
        assert!(
            output.status.success(),
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let metadata: Value = serde_json::from_slice(&output.stdout).expect("metadata is JSON");
        let packages = metadata["packages"].as_array().expect("packages");
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let this = packages
            .iter()
            .find(|p| p["manifest_path"].as_str().map(Path::new) == Some(manifest.as_path()))
            .expect("this crate is a workspace package");
        let node = metadata["resolve"]["nodes"]
            .as_array()
            .expect("resolve nodes")
            .iter()
            .find(|n| n["id"] == this["id"])
            .expect("this crate is resolved");
        let dep = node["deps"]
            .as_array()
            .expect("deps")
            .iter()
            .find(|d| d["name"] == "regex_syntax")
            .expect("this crate depends on regex-syntax");
        let package = packages
            .iter()
            .find(|p| p["id"] == dep["pkg"])
            .expect("regex-syntax is a package");
        assert_eq!(
            package["source"], "registry+https://github.com/rust-lang/crates.io-index",
            "regex-syntax comes from crates.io, not a patch or a path"
        );
        let version = package["version"].as_str().expect("version").to_string();
        let root = Path::new(package["manifest_path"].as_str().expect("manifest path"))
            .parent()
            .expect("the crate root");
        let source = std::fs::read_to_string(root.join("src/hir/translate.rs"))
            .expect("regex-syntax ships src/hir/translate.rs");
        (version, source)
    }

    /// The case-fold sites in `source`, outside its `mod tests`: every
    /// identifier that names a fold, and every use of the `i` flag, keyed by
    /// the function or struct it sits in and counted, in text order of first
    /// appearance. Comments are skipped, and so is the line that declares a
    /// function, since a declaration folds nothing.
    fn fold_sites(source: &str) -> Vec<(String, String, usize)> {
        let end = source
            .find("\n#[cfg(test)]\nmod tests {")
            .expect("translate.rs ends with its tests");
        let mut at = String::new();
        let mut sites: Vec<(String, String, usize)> = Vec::new();
        for line in source[..end].lines() {
            let code = line.split("//").next().unwrap_or("");
            let tokens: Vec<&str> = code
                .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .filter(|t| !t.is_empty())
                .collect();
            if let Some(i) = tokens.iter().position(|t| *t == "fn" || *t == "struct")
                && let Some(name) = tokens.get(i + 1)
            {
                at = format!("{} {name}", tokens[i]);
                continue;
            }
            for token in tokens {
                if !(token.to_ascii_lowercase().contains("fold") || token == "case_insensitive") {
                    continue;
                }
                match sites.iter_mut().find(|(a, t, _)| *a == at && t == token) {
                    Some(site) => site.2 += 1,
                    None => sites.push((at.clone(), token.to_string(), 1)),
                }
            }
        }
        sites
    }

    fn fnv1a64(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
    }

    #[test]
    fn the_measured_attacks_and_ordinary_patterns_land_on_their_side() {
        use super::calibration::{calibration_cases, verdict};

        for case in calibration_cases() {
            assert!(
                case.body.len() <= 1024,
                "{} is a {}-byte body",
                case.name,
                case.body.len()
            );
            let shape = price(&case.body);
            assert_eq!(
                verdict(&shape, case.copies),
                case.expect,
                "{} x{}: {} units and {} positions per body: {}",
                case.name,
                case.copies,
                shape.units,
                shape.positions,
                case.body
            );
        }
    }

    #[test]
    fn a_body_can_spend_exactly_the_translation_budget() {
        // 14 full-range folds, then one fold of S code points, which costs
        // 1 + 16 + S + 6·2938 = S + 17_645 units.
        let full = r"[\x00-\x{10FFFF}]".repeat(14);
        let rest = MAX_TRANSLATION_UNITS - 14 * 1_131_757 - 17_645;
        let at = format!("(?i){full}[\\x41-\\x{{{:X}}}]", 0x41 + rest - 1);
        let over = format!("(?i){full}[\\x41-\\x{{{:X}}}]", 0x41 + rest);
        assert_eq!(units(&at), MAX_TRANSLATION_UNITS, "{at}");
        assert_eq!(units(&over), MAX_TRANSLATION_UNITS + 1, "{over}");
    }
}
