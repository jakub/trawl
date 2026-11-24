// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Translation and repeat pricing over a regex syntax tree (ADR-0063).
//!
//! `regex_syntax::ast::visit` keeps its own heap stack, so a
//! deeply nested pattern cannot overflow the thread's stack here.
//!
//! Flags scope as the translator's `set_flags` scopes them: a group's flags
//! last until the group closes, and a bare `(?i)` lasts until its enclosing
//! group closes, across the alternation branches after it.

use std::convert::Infallible;

use regex_syntax::ast::{self, Ast, Visitor};

/// The translation budget of one query, in units.
pub(crate) const MAX_TRANSLATION_UNITS: u64 = 1 << 24;

/// The repeat budget of one pattern, in positions.
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

const PERL_WORD: Set = Set {
    ranges: 796,
    span: 144_667,
};
const PERL_DIGIT: Set = Set {
    ranges: 71,
    span: 760,
};
const PERL_SPACE: Set = Set {
    ranges: 10,
    span: 25,
};
const BYTE_WORD: Set = Set {
    ranges: 4,
    span: 63,
};
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Shape {
    pub(crate) units: u64,
    pub(crate) positions: u64,
    /// The named capture groups, in pattern order.
    pub(crate) captures: Vec<Capture>,
}

pub(crate) fn shape(ast: &Ast) -> Shape {
    let Ok(shape) = ast::visit(ast, Walk::new());
    shape
}

#[cfg_attr(
    target_pointer_width = "64",
    expect(
        clippy::result_large_err,
        reason = "the syntax error is regex-syntax's own, and a refused body is rare; \
                  on 32-bit targets it is small enough that the lint stays quiet"
    )
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

/// What a finished node translates to, as far as the alternation merge
/// cares: `Hir::alternation` unions class branches into one class, one at a
/// time, so each class branch costs a union with the branches before it.
/// When the walk cannot tell, it says `Class`, which only charges more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Translated {
    /// Matches only the empty string, so a concatenation drops it.
    Empty,
    Class(u64),
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Combine {
    Concat,
    Alternation,
    Single,
}

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

/// The visitor calls each node's pre and post hooks in matched pairs, so
/// every stack below has a top when a post hook pops it.
#[derive(Debug)]
struct Walk {
    flags: Flags,
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

    fn finished(&mut self, positions: u64, translated: Translated) {
        let Some(frame) = self.frames.last_mut() else {
            return;
        };
        let positions = match frame.combine {
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

    fn atom(&mut self, ranges: u64) {
        self.finished(1, Translated::Class(ranges));
    }

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

    fn property(&mut self, class: &ast::ClassUnicode) -> Set {
        self.pass(UNICODE_PROPERTY);
        self.fold_and_negate(UNICODE_PROPERTY, class.is_negated())
    }

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
            Ast::Empty(_) => self.finished(0, Translated::Empty),
            Ast::Flags(set) => {
                self.flags.apply(&set.flags);
                self.finished(0, Translated::Empty);
            }
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
                    ast::GroupKind::CaptureIndex(_) | ast::GroupKind::CaptureName { .. } => {
                        (1, Translated::Other)
                    }
                    ast::GroupKind::NonCapturing(_) => (0, inner.translated),
                };
                self.finished(inner.positions.saturating_add(own), translated);
            }
            Ast::Concat(_) => {
                let inner = self.close();
                self.finished(inner.positions, inner.translated);
            }
            Ast::Alternation(alternation) => {
                let inner = self.close();
                let splits =
                    u64::try_from(alternation.asts.len().saturating_sub(1)).unwrap_or(u64::MAX);
                self.finished(inner.positions.saturating_add(splits), inner.translated);
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

    pub(crate) fn unequal_alternation() -> String {
        let b3 = r"(?:(?:\pL){0,2}){0,2}";
        let alternation = format!(r"(?:\pL|(?:\pL)?|{b3}|(?:{b3})?){{0,2}}");
        let wrapped = (0..38).fold(alternation, |s, _| format!("(?:{s})?"));
        format!(r"\B{wrapped}\PL")
    }

    const SYSLOG: &str = r"(?i)^\w{3}\s+\d+\s+[\d:]+\s+(\S+)\s+(\w+)\[(\d+)\]";
    const EMAIL: &str = r"(?i)[a-z0-9._%+-]+@[a-z0-9.-]+\.[a-z]{2,}";
    const UUID: &str = r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}";

    /// Every regex body in `docs/src/content/docs/reference/dsl.md`.
    const DSL_MD_REGEXES: &[&str] = &[
        "timeout|refused",
        "(?i)timeout",
        "connection refused",
        "^a b",
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
        r"^\s+at ",
    ];

    pub(crate) fn calibration_cases() -> Vec<Case> {
        use Verdict::{Admitted, RefusedByRepeat, RefusedByTranslation};
        let mut cases = vec![
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
                "F7 unequal alternation under {0,2}",
                unequal_alternation(),
                1,
                RefusedByRepeat,
            ),
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
        for body in crate::pattern::charge::LOG_REGEXES {
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
        assert_eq!(units("(?i)[a-z]"), 1 + 16 + 26 + 6 * 26);
        assert_eq!(
            units(r"(?i)[\x00-\x{10FFFF}]"),
            1 + 16 + 0x11_0000 + 6 * 2938
        );
        assert_eq!(
            units(r"(?i)[^\x00-\x{10FFFE}]"),
            1 + 16 + 0x10_FFFF + 6 * 2938 + (1 + 2938)
        );
        assert_eq!(units(r"(?i)[\w]"), 2 * 796 + 16 * 796 + 144_667 + 6 * 2938);
    }

    #[test]
    fn a_negated_nested_class_widens_to_every_code_point() {
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
        assert_eq!(units(r"\w|(\d)"), 796 + 71 + 796);
        assert_eq!(units(r"\w|\d{2}"), 796 + 71 + 796);
        assert_eq!(units(r"\w|\d\d"), 796 + 2 * 71 + 796);
        assert_eq!(units(r"\w|^"), 796 + 796);
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
        ("Ast::Alternation", "a|b", 3),
        ("Ast::Alternation with an empty branch", "a|", 3),
        ("Ast::Alternation of empty branches", "|", 3),
        ("Ast::Concat", "ab", 2),
    ];

    #[test]
    fn every_node_kind_counts_its_positions() {
        for (node, body, charge) in NODE_CHARGES {
            assert_eq!(positions(body), *charge, "{node}: {body}");
        }
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
        assert_eq!(positions("a|"), 3);
        assert_eq!(positions("(?:|)"), 3);
        assert_eq!(positions("(?:||||)"), 5 + 4);
        assert_eq!(positions("(?:(?i)|a)"), 3, "a branch of flags is empty");
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
        assert_eq!(positions("ab|cde"), 2 + 3 + 1);
        assert_eq!(positions("(abc)(?:de)(?P<x>f)"), 8);
        assert_eq!(positions("(?:a|b)[abc](?:a|d)d"), 8);
    }

    #[test]
    fn an_alternation_counts_one_position_for_each_branch_after_the_first() {
        for (body, expect) in [
            ("a|b", 3),
            ("a|b|c", 5),
            ("(?:a|)", 3),
            ("(?:a|b){3}", 9),
            ("(?:a|b|c|d|e)", 9),
            ("(?:ab|c)", 4),
            ("((?:a|b))", 4),
            ("(?:a|b)?", 4),
            ("(?:a|(?:b|c))", 5),
        ] {
            assert_eq!(positions(body), expect, "{body}");
        }
        assert_eq!(positions(&super::calibration::unequal_alternation()), 70);
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
        assert_eq!(positions("(?:a|b)[abc]{1000}(?:a|d)d"), 1007);
    }

    #[test]
    fn an_unbounded_repeat_counts_its_lower_bound_plus_one() {
        assert_eq!(positions("a+"), 2);
        assert_eq!(positions("a{3,}"), 4);
        assert_eq!(positions("(?:ab)+"), 4);
        assert_eq!(positions("(?:a+)+"), 4);
        assert_eq!(positions("a*"), 2);
        assert_eq!(positions("a{0,}"), 2);
    }

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
