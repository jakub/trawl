// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The one owner of regex construction (ADR-0063).
//!
//! Every regex trawl builds from query text is built by a [`Preparer`]:
//! search-stage regexes and globs, `matches`, `like` and `ilike`
//! operands, and `extract` bodies. Each is a program of regex-automata's
//! meta engine, the engine under the `regex` crate, with one cache of its
//! own: no search draws on the engine's per-thread pool of caches.
//!
//! A clippy `disallowed-methods` rule bans the `regex` and
//! `regex-automata` builders everywhere else, and the meta searches that
//! use the pool. It cannot see a build through `FromStr` or `TryFrom`, so
//! `tests::construction_inventory` scans the sources for those spellings,
//! and lists where each ledger begins.
//!
//! `price` measures a pattern's translation and repeat cost from its
//! syntax tree before anything is built. `re2` holds what the batch
//! engine's RE2 reads differently from the live lane: it refuses the
//! class syntax the two engines would match differently, and writes the
//! view whose program the ledger caps. `charge` is the accounting: what a
//! prepared pattern charges against its query's regex budget, and the
//! lazy-DFA capacity it builds with. `memory` holds the server-wide
//! reservation every charge is held from while its pattern lives. `table`
//! holds the patterns a compiled live stage looks up.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::ops::Range;
use std::sync::{Arc, Mutex, MutexGuard};

use regex_automata::nfa::thompson;
use regex_automata::util::look::LookMatcher;
use regex_automata::util::syntax;
use regex_automata::{Input, MatchKind, meta};
use regex_syntax::hir::Hir;

use crate::ast::{BinaryOp, Expr, LiteralValue};
use charge::{BUILD_TRANSIENT, Charge, Profile, REGEX_QUERY_BUDGET};
use memory::{RegexHold, RegexReservation};

pub mod charge;
pub mod memory;
pub(crate) mod price;
pub(crate) mod re2;
pub(crate) mod table;

#[cfg(test)]
mod tests;

/// Maximum regex body length, in bytes, to prevent compilation-based
/// denial-of-service.
pub const MAX_REGEX_LEN: usize = 1024;

/// The most memory one compiled query regex may take, in bytes: the NFA
/// size limit every [`Preparer`] build checks a pattern's program
/// against, the measure of the `regex` crate's `size_limit`. A regex body's batch-engine view has a cap of its own,
/// [`MAX_BATCH_COMPILED_BYTES`].
///
/// [`MAX_REGEX_LEN`] does not bound compile work. `\w{64}` is six bytes,
/// and each count compiles another copy of a Unicode class, about 50 KiB
/// apiece. Compile time follows compiled size. The most compile work a
/// query may hold, [`crate::parser::MAX_QUERY_PATTERNS`] bodies that each
/// compile near this limit and near [`MAX_BATCH_COMPILED_BYTES`] in their
/// view, took about 1.2 s to admit on a desktop CPU
/// (`docs/evidence/issue-334/transcript.txt`). `\w{20}` and `\d{64}`
/// compile under it. `\d{200}` and `.{1000}` would too, but the repeat budget refuses
/// them before anything builds (`price::MAX_REPEAT_POSITIONS`).
pub const MAX_REGEX_COMPILED_BYTES: usize = 1 << 20;

/// The message a regex over [`MAX_REGEX_COMPILED_BYTES`] carries.
pub(crate) const MSG_REGEX_TOO_LARGE: &str = "regex pattern too large once compiled (max 1 MiB)";

/// The most a regex body's program may take once the batch engine
/// compiles it, in bytes, as the live lane's NFA size limit measures the
/// body's view (`re2`).
///
/// `DuckDB` runs every batch regex in RE2 under its default `max_mem` of
/// 8 MiB, and RE2 sizes its DFA from that budget. A program past the
/// budget fails the DFA's init, so every call runs the NFA instead, at
/// hundreds of milliseconds a call over one stored value where the DFA
/// takes a few. Measured on the view, that step lies between 628.7 and
/// 663.9 KiB for programs of large classes (ADR-0063), and this cap sits
/// under it.
///
/// The cap keeps RE2's DFA from failing at init. It does not bound one RE2
/// call once the DFA gives up on a busy subject: the cost transcript's
/// slowest admitted body took about 1.8 s a call over one 65 536-character
/// value. ADR-0063 records that as a measured envelope with no ceiling.
/// Its 0.5 s ceiling holds only for the meta engine, which runs the live
/// lane.
pub const MAX_BATCH_COMPILED_BYTES: usize = 512 << 10;

/// The message a regex body over [`MAX_BATCH_COMPILED_BYTES`] carries.
pub(crate) const MSG_BATCH_TOO_LARGE: &str =
    "regex pattern too large for the batch engine once compiled (max 512 KiB)";

/// How the message of a pattern over the repeat budget begins
/// ([`PatternError::Repeat`]). The counts follow it.
pub(crate) const MSG_REPEAT_BUDGET: &str = "pattern repeats too much";

/// How the message of a query over the translation budget begins
/// ([`PatternError::Translation`]). The counts follow it.
pub(crate) const MSG_TRANSLATION_BUDGET: &str = "the query's patterns cost too much to translate";

/// The message a regex body carries when it nests a class in a class or
/// uses a class set operator ([`PatternError::ClassSetSyntax`]).
pub(crate) const MSG_CLASS_SET_SYNTAX: &str =
    "nested character classes and the class set operators &&, -- and ~~ are not supported";

/// The message a computed `matches`, `like` or `ilike` operand carries
/// ([`PatternError::Computed`]).
pub(crate) const MSG_PATTERN_NOT_LITERAL: &str =
    "a pattern must be literal text in the query, not a field or a computed value";

/// The message of a query whose patterns charge more than its regex
/// budget ([`RegexRefusal::Budget`]).
pub(crate) const MSG_REGEX_BUDGET: &str =
    "the query's patterns need more regex memory than one query may hold";

/// The message of a query the server's regex reservation cannot hold
/// ([`RegexRefusal::Reservation`]).
pub(crate) const MSG_REGEX_RESERVATION: &str = "the server's regex memory is held by other queries";

/// What a pattern's text is written in, and so how it becomes a regex.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PatternKind {
    /// A regex body, used as written: a search-stage `/…/`, a `matches`
    /// operand or an `extract` body.
    Regex,
    /// A SQL `like` pattern: `%` and `_` are wildcards, the rest literal,
    /// and the whole value must match.
    Like,
    /// `like`, ignoring case.
    ILike,
    /// A `DuckDB` GLOB in a search value, matched whole and across
    /// newlines.
    Glob,
}

impl PatternKind {
    /// The kind of pattern an expression operator's right-hand side is, or
    /// `None` when the operator takes no pattern.
    #[must_use]
    pub fn of_operator(op: BinaryOp) -> Option<Self> {
        match op {
            BinaryOp::Matches => Some(Self::Regex),
            BinaryOp::Like => Some(Self::Like),
            BinaryOp::ILike => Some(Self::ILike),
            _ => None,
        }
    }

    /// What a diagnostic calls this kind of pattern.
    fn noun(self) -> &'static str {
        match self {
            Self::Regex => "regex pattern",
            Self::Like => "like pattern",
            Self::ILike => "ilike pattern",
            Self::Glob => "glob pattern",
        }
    }

    /// The regex `text` stands for when written as this kind.
    pub(crate) fn regex_source(self, text: &str) -> Cow<'_, str> {
        match self {
            Self::Regex => Cow::Borrowed(text),
            Self::Like => Cow::Owned(like_to_regex(text, false)),
            Self::ILike => Cow::Owned(like_to_regex(text, true)),
            Self::Glob => Cow::Owned(glob_to_regex(text)),
        }
    }
}

/// The pattern text of a `matches`, `like` or `ilike` right-hand side.
///
/// This is the one literal-only rule (ADR-0063): the grammar, the emitter
/// and the live pattern tables all ask it. A pattern is literal text in the
/// query, never a value computed per event, so it can be priced and built
/// once before anything runs.
///
/// # Errors
///
/// [`PatternError::Computed`] when `rhs` is anything but a string literal.
pub fn literal_operand(rhs: &Expr) -> Result<&str, PatternError> {
    match rhs {
        Expr::Literal(LiteralValue::String(text)) => Ok(text),
        _ => Err(PatternError::Computed),
    }
}

/// An `extract` body as the batch lane binds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BatchExtract {
    /// The body with every unnamed group made non-capturing. A group's
    /// kind never changes what a regex matches, so this matches what the
    /// body matches.
    pub(crate) regex: String,
    /// The named groups in pattern order, each with its index in
    /// [`regex`](Self::regex): its place among the named groups, from 1.
    pub(crate) captures: Vec<price::Capture>,
}

/// Rewrite an `extract` body for the batch lane.
///
/// The batch lane binds the body once, through `DuckDB`'s struct-form
/// `regexp_extract`, whose k-th field is the k-th capture group, named or
/// not. The batch copy has every unnamed group made non-capturing, so its
/// named groups are numbered 1 to n in order and named group k is field
/// k, whatever unnamed groups sit before or around it. The struct form
/// has no limit of 9 groups.
///
/// The rewrite runs on the syntax tree and prints it back, so a body with
/// no unnamed group binds as written. It builds nothing.
#[cfg_attr(
    target_pointer_width = "64",
    expect(
        clippy::result_large_err,
        reason = "the syntax error is regex-syntax's own, and a refused body is rare; \
                  on 32-bit targets it is small enough that the lint stays quiet"
    )
)]
pub(crate) fn batch_extract(body: &str) -> Result<BatchExtract, regex_syntax::ast::Error> {
    use regex_syntax::ast::{self, Ast};

    let mut tree = ast::parse::Parser::new().parse(body)?;
    // An explicit stack, as the syntax tree's own visitor keeps, so a
    // deeply nested body cannot overflow the thread's stack.
    let mut open: Vec<&mut Ast> = vec![&mut tree];
    while let Some(node) = open.pop() {
        match node {
            Ast::Group(group) => {
                match group.kind {
                    ast::GroupKind::CaptureIndex(_) => {
                        group.kind = ast::GroupKind::NonCapturing(ast::Flags {
                            span: group.span,
                            items: Vec::new(),
                        });
                    }
                    ast::GroupKind::CaptureName { .. } | ast::GroupKind::NonCapturing(_) => {}
                }
                open.push(&mut group.ast);
            }
            Ast::Repetition(repetition) => open.push(&mut repetition.ast),
            Ast::Alternation(alternation) => open.extend(alternation.asts.iter_mut()),
            Ast::Concat(concat) => open.extend(concat.asts.iter_mut()),
            Ast::Empty(_)
            | Ast::Flags(_)
            | Ast::Literal(_)
            | Ast::Dot(_)
            | Ast::Assertion(_)
            | Ast::ClassUnicode(_)
            | Ast::ClassPerl(_)
            | Ast::ClassBracketed(_) => {}
        }
    }
    let mut regex = String::new();
    // Printing into a `String` cannot fail.
    let _ = ast::print::Printer::new().print(&tree, &mut regex);
    // The indices are what the parser assigns to the text that binds.
    let captures = price::captures(&regex)?;
    Ok(BatchExtract { regex, captures })
}

/// A built pattern. Only a [`Preparer`] makes one.
///
/// A clone shares the program, its one cache and the reservation it holds.
/// Every search locks that cache, so no search draws on the meta engine's
/// own per-thread pool of caches. The last clone to drop returns the
/// pattern's charge to the server-wide reservation ([`memory`]).
#[derive(Clone)]
pub struct Pattern(Arc<Prepared>);

/// A compiled program, the cache every search of it uses, what the two
/// charge against the query's regex budget, and that charge held from the
/// server-wide reservation.
struct Prepared {
    regex: meta::Regex,
    cache: Mutex<meta::Cache>,
    charge: Charge,
    /// Dropped with the last clone, which returns the charge.
    #[expect(
        dead_code,
        reason = "held for its drop, which returns the charge to the reservation"
    )]
    hold: RegexHold,
}

impl fmt::Debug for Pattern {
    /// meta's own `Debug` prints the whole program, so this prints none
    /// of it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pattern").finish_non_exhaustive()
    }
}

impl Pattern {
    fn new(program: Program, hold: RegexHold) -> Self {
        let Program { regex, charge } = program;
        let cache = Mutex::new(regex.create_cache());
        Self(Arc::new(Prepared {
            regex,
            cache,
            charge,
            hold,
        }))
    }

    /// What this pattern charges against its query's regex budget, and
    /// the lazy-DFA capacity it built with. A clone charges nothing more:
    /// it shares the program and the cache.
    #[must_use]
    pub fn charge(&self) -> Charge {
        self.0.charge
    }

    /// Whether the pattern matches anywhere in `haystack`. A `like`,
    /// `ilike` or glob pattern is anchored, so it must match all of it.
    #[must_use]
    pub fn is_match(&self, haystack: &str) -> bool {
        let input = Input::new(haystack).earliest(true);
        self.0
            .regex
            .search_half_with(&mut self.cache(), &input)
            .is_some()
    }

    /// The capture groups of the leftmost match in `haystack`, if any.
    ///
    /// The owner never lends out its program. Code that held one could
    /// build another from it outside the owner and search it through the
    /// meta engine's pool.
    #[must_use]
    pub fn captures<'h>(&self, haystack: &'h str) -> Option<Captures<'h>> {
        let mut groups = self.0.regex.create_captures();
        self.0
            .regex
            .search_captures_with(&mut self.cache(), &Input::new(haystack), &mut groups);
        groups.is_match().then_some(Captures { haystack, groups })
    }

    /// The pattern's cache, locked. A search that panicked while it held
    /// the lock poisoned it and may have left the cache in any state, so
    /// the next search takes a fresh cache and clears the poison.
    fn cache(&self) -> MutexGuard<'_, meta::Cache> {
        self.0.cache.lock().unwrap_or_else(|poisoned| {
            let mut cache = poisoned.into_inner();
            *cache = self.0.regex.create_cache();
            self.0.cache.clear_poison();
            cache
        })
    }
}

/// The capture groups of one match, read by name.
#[derive(Debug)]
pub struct Captures<'h> {
    haystack: &'h str,
    groups: regex_automata::util::captures::Captures,
}

impl<'h> Captures<'h> {
    /// What the group `name` matched, or `None` when the pattern has no
    /// such group or the group took no part in the match.
    #[must_use]
    pub fn name(&self, name: &str) -> Option<Group<'h>> {
        let span = self.groups.get_group_by_name(name)?;
        Some(Group {
            haystack: self.haystack,
            range: span.range(),
        })
    }
}

/// The text one capture group matched.
#[derive(Debug, Clone)]
pub struct Group<'h> {
    haystack: &'h str,
    range: Range<usize>,
}

impl<'h> Group<'h> {
    /// The matched text.
    #[must_use]
    pub fn as_str(&self) -> &'h str {
        &self.haystack[self.range.clone()]
    }

    /// Where the matched text sits in the haystack, in bytes.
    #[must_use]
    pub fn range(&self) -> Range<usize> {
        self.range.clone()
    }
}

/// Why a pattern was refused or not built, one cause per refusal.
///
/// `Display` gives the cause in words. A syntax error displays
/// regex-syntax's own text, word for word what the `regex` crate's error
/// displays, so each lane frames it as it always has (`invalid regex: …`, `invalid regex in extract:
/// …`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatternError {
    /// The operand is computed, not literal text in the query.
    Computed,
    /// Not a regex: regex-syntax's message, as the `regex` crate words
    /// it, which names an unknown Unicode class too.
    Syntax(String),
    /// The pattern text is over [`MAX_REGEX_LEN`] bytes.
    Length { kind: PatternKind, bytes: usize },
    /// A regex body nests a class in a bracketed class or uses a class
    /// set operation, which RE2 reads differently (`re2`).
    ClassSetSyntax,
    /// The query's regexes cost more to translate than its budget.
    Translation { units: u64, limit: u64 },
    /// The pattern repeats to more positions than its budget.
    Repeat { positions: u64, limit: u64 },
    /// The query charges more patterns than its limit.
    FanOut { charges: usize, limit: usize },
    /// The pattern compiles past [`MAX_REGEX_COMPILED_BYTES`].
    CompiledSize,
    /// A regex body's program, as the batch engine reads it, compiles past
    /// [`MAX_BATCH_COMPILED_BYTES`].
    BatchCompiledSize,
    /// Not built: an earlier build in the same ledger failed for size,
    /// and a ledger builds nothing after that.
    Stopped,
    /// A compiled stage has no prepared pattern for its operand.
    Missing,
    /// The query's regex memory does not fit, in its budget or in the
    /// server's reservation. A capacity refusal, not a fault in the query
    /// text, so its sentence quotes no pattern. A ledger builds nothing
    /// after it, and a new pattern gets the same refusal.
    Resources(RegexRefusal),
}

/// Which regex memory bound refused a query (ADR-0063).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegexRefusal {
    /// Its prepared patterns charge more than [`REGEX_QUERY_BUDGET`],
    /// with one build transient, or it would hold more than
    /// [`memory::QUERY_RESERVATION_CAP`] of the reservation, its batch
    /// statement's RE2 allowance included. The query can never run.
    Budget,
    /// The server-wide regex reservation cannot hold its charge, the
    /// transient of its next build, or its batch statement's RE2
    /// allowance, beside what others hold now
    /// ([`memory::REGEX_RESERVATION_BYTES`]).
    Reservation,
}

impl fmt::Display for PatternError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Computed => f.write_str(MSG_PATTERN_NOT_LITERAL),
            Self::Syntax(message) => f.write_str(message),
            Self::Length { kind, bytes } => write!(
                f,
                "{} too long ({bytes} bytes, max {MAX_REGEX_LEN})",
                kind.noun()
            ),
            Self::ClassSetSyntax => f.write_str(MSG_CLASS_SET_SYNTAX),
            Self::Translation { units, limit } => {
                write!(f, "{MSG_TRANSLATION_BUDGET} ({units} units, max {limit})")
            }
            Self::Repeat { positions, limit } => {
                write!(
                    f,
                    "{MSG_REPEAT_BUDGET} ({positions} positions, max {limit})"
                )
            }
            Self::FanOut { charges, limit } => write!(
                f,
                "the query charges {charges} patterns, over the limit of {limit}"
            ),
            Self::CompiledSize => f.write_str(MSG_REGEX_TOO_LARGE),
            Self::BatchCompiledSize => f.write_str(MSG_BATCH_TOO_LARGE),
            Self::Stopped => f.write_str(
                "not built: an earlier pattern in this query compiled past the size limit",
            ),
            Self::Missing => {
                f.write_str("a compiled stage has no prepared pattern for its operand")
            }
            Self::Resources(RegexRefusal::Budget) => f.write_str(MSG_REGEX_BUDGET),
            Self::Resources(RegexRefusal::Reservation) => f.write_str(MSG_REGEX_RESERVATION),
        }
    }
}

impl std::error::Error for PatternError {}

/// A ledger of the patterns one preparation builds, and the only builder.
///
/// It builds each `(kind, text)` once and answers a repeat from what it
/// holds, a failure included. [`builds`](Self::builds) counts the builds
/// it ran. Once a build fails for size it builds nothing new and answers
/// [`PatternError::Stopped`], so a query of many patterns that each
/// compile past the limit costs one failed build, not one per pattern.
///
/// It meters the query's regex budget. Each pattern it builds charges
/// what [`charge`] computes, and while the charges and one
/// [`BUILD_TRANSIENT`] fit [`REGEX_QUERY_BUDGET`] the query is admitted.
/// The build that takes them past refuses with
/// [`RegexRefusal::Budget`] and stops the ledger. A pattern shared by
/// several stages, or by the filter and the plan, charges once.
///
/// It holds every charge from the server-wide reservation ([`memory`]).
/// Before a build it holds one [`BUILD_TRANSIENT`], and once the build is
/// in budget it resizes that hold to the pattern's charge and gives it to
/// the pattern, which returns it when its last clone drops. A hold that
/// does not fit refuses with [`RegexRefusal::Reservation`] and stops the
/// ledger, so no metered build and no match runs unreserved.
///
/// A batch query's ledger also holds the RE2 allowance of the statement
/// it runs ([`Self::hold_re2`]), until the ledger drops. What the ledger
/// holds, its patterns' charges, a build transient and the allowance,
/// stays within [`memory::QUERY_RESERVATION_CAP`], or the query is
/// refused with [`RegexRefusal::Budget`].
///
/// Open one ledger per preparation and keep it: a ledger opened per call
/// builds and charges again on every call. [`Preparer::check`] opens a
/// ledger that validates, keeps no pattern and holds nothing.
#[derive(Debug)]
pub struct Preparer {
    ledger: Ledger<Pattern>,
    /// What the patterns this ledger holds charge, in bytes.
    charged: usize,
    /// The most `charged` and one build transient may come to:
    /// [`REGEX_QUERY_BUDGET`], or less in a test.
    budget: usize,
    /// Where its holds come from: the process-wide reservation, or a
    /// test's own.
    reservation: &'static RegexReservation,
    /// The RE2 allowance of the batch statement this ledger's query runs,
    /// held until the ledger drops ([`Self::hold_re2`]).
    re2: Option<RegexHold>,
    /// The most the ledger may hold of the reservation at once:
    /// [`memory::QUERY_RESERVATION_CAP`], or less in a test.
    cap: u64,
}

impl Preparer {
    /// An empty metered ledger, for a preparation that matches. It has no
    /// `Default`, so each ledger is a `Preparer::new` call the
    /// construction inventory can see.
    #[must_use]
    #[expect(
        clippy::new_without_default,
        reason = "every ledger opens through a visible constructor call, which the construction inventory lists"
    )]
    pub fn new() -> Self {
        Self {
            ledger: Ledger::new(),
            charged: 0,
            budget: REGEX_QUERY_BUDGET,
            reservation: memory::reservation(),
            re2: None,
            cap: memory::QUERY_RESERVATION_CAP,
        }
    }

    /// An empty ledger that checks patterns and keeps none, for `parse`.
    ///
    /// It builds each pattern through the same build as [`Preparer::new`]'s
    /// ledger, so the two accept and refuse alike, and drops the program
    /// at once. It meters no budget and holds no reservation: a query is
    /// charged where it is prepared to match, not where it is validated.
    #[must_use]
    pub fn check() -> Checker {
        Checker {
            ledger: Ledger::new(),
        }
    }

    /// Build `text` as a `kind` pattern, or answer with what this ledger
    /// already built for it.
    ///
    /// A regex body's class set syntax is refused before anything builds,
    /// as the parser refuses it, so a plan built from a query no parser
    /// checked refuses it too. That refusal is no build.
    ///
    /// # Errors
    ///
    /// [`PatternError::Syntax`] when the regex does not parse or names an
    /// unknown class, [`PatternError::ClassSetSyntax`] for a regex body
    /// that nests a class or uses a class set operator,
    /// [`PatternError::CompiledSize`] when it compiles past
    /// [`MAX_REGEX_COMPILED_BYTES`], [`PatternError::BatchCompiledSize`]
    /// for a regex body whose batch-engine view compiles past
    /// [`MAX_BATCH_COMPILED_BYTES`], [`PatternError::Stopped`] for a new
    /// pattern once a build has failed for size, and
    /// [`PatternError::Resources`] with [`RegexRefusal::Budget`] when its
    /// charge takes the query past its budget, with
    /// [`RegexRefusal::Reservation`] when the reservation cannot hold its
    /// transient or its charge, and with the same refusal for every new
    /// pattern after either.
    pub fn build(&mut self, kind: PatternKind, text: &str) -> Result<Pattern, PatternError> {
        let (charged, budget, reservation) = (&mut self.charged, self.budget, self.reservation);
        // The rest of the query's cap, once its RE2 allowance and a build
        // transient are held beside the charges.
        let room = self
            .cap
            .saturating_sub(self.re2.as_ref().map_or(0, RegexHold::bytes))
            .saturating_sub(reservation_bytes(BUILD_TRANSIENT));
        let refused = PatternError::Resources(RegexRefusal::Reservation);
        self.ledger.build(
            kind,
            text,
            || {
                reservation
                    .try_hold(reservation_bytes(BUILD_TRANSIENT))
                    .ok_or_else(|| refused.clone())
            },
            |program, mut hold| {
                let total = program.charge.total();
                let after = charged.saturating_add(total);
                if after.saturating_add(BUILD_TRANSIENT) > budget || reservation_bytes(after) > room
                {
                    return Err(PatternError::Resources(RegexRefusal::Budget));
                }
                hold.resize(reservation_bytes(total))
                    .map_err(PatternError::Resources)?;
                *charged = after;
                let pattern = Pattern::new(program, hold);
                #[cfg(any(test, feature = "test-support"))]
                reservation.after_charge();
                Ok(pattern)
            },
        )
    }

    /// Hold `bytes` of RE2 allowance for the batch statement this
    /// ledger's query is about to bind ([`memory::RegexSites::re2_allowance`]),
    /// in place of any allowance it holds already, until the ledger drops.
    ///
    /// Call it before the statement binds, so `DuckDB` compiles no regex
    /// unreserved. Zero bytes hold nothing. A query that runs a second
    /// statement, such as the hot-only fallback, calls it again with that
    /// statement's allowance once the first statement has dropped.
    ///
    /// # Errors
    ///
    /// [`RegexRefusal::Budget`] when the allowance and the patterns'
    /// charges pass [`memory::QUERY_RESERVATION_CAP`], so the query can
    /// never run, and [`RegexRefusal::Reservation`] when the reservation
    /// cannot hold it beside what others hold now. The ledger keeps what
    /// it held before then.
    pub fn hold_re2(&mut self, bytes: u64) -> Result<(), RegexRefusal> {
        if bytes.saturating_add(reservation_bytes(self.charged)) > self.cap {
            return Err(RegexRefusal::Budget);
        }
        if bytes == 0 {
            self.re2 = None;
        } else if let Some(held) = &mut self.re2 {
            held.resize(bytes)?;
        } else {
            let held = self.reservation.try_hold(bytes);
            self.re2 = Some(held.ok_or(RegexRefusal::Reservation)?);
        }
        #[cfg(any(test, feature = "test-support"))]
        if bytes > 0 {
            self.reservation.after_charge();
        }
        Ok(())
    }

    /// The RE2 allowance this ledger holds, in bytes ([`Self::hold_re2`]).
    #[must_use]
    pub fn re2_held(&self) -> u64 {
        self.re2.as_ref().map_or(0, RegexHold::bytes)
    }

    /// How many builds this ledger ran, failed ones included.
    #[must_use]
    pub fn builds(&self) -> usize {
        self.ledger.builds
    }

    /// What the patterns this ledger holds charge, in bytes: the sum it
    /// meters against the budget, without the build transient.
    #[must_use]
    pub fn charged(&self) -> usize {
        self.charged
    }

    /// What `kind`'s regex for `text` compiles to, in bytes as the NFA
    /// size limit counts them: the smallest limit under which it builds.
    ///
    /// A measuring instrument for the cost harness
    /// (`crates/trawl-engine/tests/regex_cost.rs`), not a step of any
    /// preparation. It bisects, so it builds the regex about 22 times, and
    /// [`builds`](Self::builds) counts each. It remembers nothing, charges
    /// nothing, holds no reservation and ignores the ledger's stop.
    ///
    /// # Errors
    ///
    /// [`PatternError::Syntax`] when the regex does not build at any
    /// limit, and [`PatternError::CompiledSize`] when it compiles past
    /// [`MEASURE_LIMIT_BYTES`].
    pub fn compiled_bytes(&mut self, kind: PatternKind, text: &str) -> Result<usize, PatternError> {
        self.bisect(&kind.regex_source(text))
    }

    /// [`compiled_bytes`](Self::compiled_bytes) of a regex body's view as
    /// the batch engine reads it (`re2`): with `\w`, `\d`, `\s`, `\b` and
    /// `\B` in their ASCII meanings, what `DuckDB`'s RE2 compiles the body
    /// to, in the same measure.
    ///
    /// # Errors
    ///
    /// As [`compiled_bytes`](Self::compiled_bytes), and
    /// [`PatternError::Syntax`] when the body does not parse.
    pub fn batch_compiled_bytes(&mut self, text: &str) -> Result<usize, PatternError> {
        let ast = regex_syntax::ast::parse::Parser::new()
            .parse(text)
            .map_err(|e| PatternError::Syntax(e.to_string()))?;
        self.bisect(&re2::view(&ast))
    }

    /// The smallest size limit under which `source` builds, by bisection
    /// up to [`MEASURE_LIMIT_BYTES`].
    fn bisect(&mut self, source: &str) -> Result<usize, PatternError> {
        let mut fits = |limit| {
            self.ledger.count_build();
            compile(&parse_hir(source)?, limit).map(|_| ())
        };
        fits(MEASURE_LIMIT_BYTES)?;
        let (mut fails, mut builds) = (0, MEASURE_LIMIT_BYTES);
        while builds - fails > 1 {
            let mid = fails + (builds - fails) / 2;
            if fits(mid).is_ok() {
                builds = mid;
            } else {
                fails = mid;
            }
        }
        Ok(builds)
    }
}

/// A ledger that checks each pattern a query holds and keeps none
/// ([`Preparer::check`]). It has no method that hands out a [`Pattern`]:
/// what it builds, it drops.
#[derive(Debug)]
pub struct Checker {
    ledger: Ledger<()>,
}

impl Checker {
    /// Build `text` as a `kind` pattern and drop it, or answer with this
    /// ledger's verdict on it from before.
    ///
    /// # Errors
    ///
    /// As [`Preparer::build`], but never [`PatternError::Resources`].
    pub fn check(&mut self, kind: PatternKind, text: &str) -> Result<(), PatternError> {
        self.ledger
            .build(kind, text, || Ok(()), |_program, ()| Ok(()))
    }

    /// How many builds this ledger ran, failed ones included.
    #[must_use]
    pub fn builds(&self) -> usize {
        self.ledger.builds
    }
}

/// What both kinds of ledger share: the answer for each `(kind, text)`
/// it has seen, its build count, and its stop.
#[derive(Debug)]
struct Ledger<T> {
    built: HashMap<(PatternKind, String), Result<T, PatternError>>,
    builds: usize,
    /// The refusal a new pattern gets once a build has stopped the ledger,
    /// which remembers why it stopped.
    stopped: Option<PatternError>,
}

impl<T: Clone> Ledger<T> {
    fn new() -> Self {
        Self {
            built: HashMap::new(),
            builds: 0,
            stopped: None,
        }
    }

    /// Build `text` as a `kind` pattern and `keep` the program, or answer
    /// with what this ledger already holds for it. `reserve` runs before
    /// the build and may refuse it, so a refused reservation builds
    /// nothing. `keep` turns a built program and what `reserve` gave into
    /// what the ledger holds, or refuses it. A build that fails drops what
    /// `reserve` gave.
    fn build<R>(
        &mut self,
        kind: PatternKind,
        text: &str,
        reserve: impl FnOnce() -> Result<R, PatternError>,
        keep: impl FnOnce(Program, R) -> Result<T, PatternError>,
    ) -> Result<T, PatternError> {
        let key = (kind, text.to_owned());
        if let Some(done) = self.built.get(&key) {
            return done.clone();
        }
        // A regex body's view, as the batch engine reads it. A body that
        // does not parse has none, and falls through to the build, which
        // words its syntax error as the `regex` crate does.
        let mut view = None;
        if kind == PatternKind::Regex
            && let Ok(ast) = regex_syntax::ast::parse::Parser::new().parse(text)
        {
            if let Err(refused) = re2::class_set_refusal(&ast) {
                self.built.insert(key, Err(refused.clone()));
                return Err(refused);
            }
            view = Some(re2::view(&ast));
        }
        if let Some(stopped) = &self.stopped {
            return Err(stopped.clone());
        }
        let built = match reserve() {
            Ok(reserved) => {
                let built = prepare(&kind.regex_source(text), view.as_deref());
                self.count_build();
                built.and_then(|program| keep(program, reserved))
            }
            Err(refused) => Err(refused),
        };
        self.stopped = match &built {
            Err(PatternError::CompiledSize | PatternError::BatchCompiledSize) => {
                Some(PatternError::Stopped)
            }
            Err(resources @ PatternError::Resources(_)) => Some(resources.clone()),
            _ => None,
        };
        self.built.insert(key, built.clone());
        built
    }

    /// Count one build in this ledger and, under test support, on this
    /// thread.
    fn count_build(&mut self) {
        self.builds += 1;
        #[cfg(any(test, feature = "test-support"))]
        REGEX_BUILDS.with(|n| n.set(n.get() + 1));
    }
}

/// The largest compiled size [`Preparer::compiled_bytes`] measures: four
/// times [`MAX_REGEX_COMPILED_BYTES`], past every pattern the cost
/// harness calibrates against.
pub const MEASURE_LIMIT_BYTES: usize = 4 * MAX_REGEX_COMPILED_BYTES;

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    /// How many regexes any ledger has built on this thread.
    static REGEX_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many regexes any ledger has built on this thread, so a test can
/// show that some work built none. Take the difference across the work:
/// other work on the same thread counts too.
#[cfg(any(test, feature = "test-support"))]
#[must_use]
pub fn thread_builds() -> usize {
    REGEX_BUILDS.with(std::cell::Cell::get)
}

/// `bytes` as the reservation counts them. A size past `u64` cannot be
/// held, so it reads as the most there is, which no reservation holds.
fn reservation_bytes(bytes: usize) -> u64 {
    u64::try_from(bytes).unwrap_or(u64::MAX)
}

/// What one build makes: a program and what it charges.
struct Program {
    regex: meta::Regex,
    charge: Charge,
}

/// One build of a pattern: its program `source` under
/// [`MAX_REGEX_COMPILED_BYTES`], and, for a regex body, its batch-engine
/// `view` under [`MAX_BATCH_COMPILED_BYTES`].
///
/// A view that differs from the program compiles after it, so a program
/// over 1 MiB reports that limit. A view the same as the program is one
/// compile at the smaller limit, which checks both, and only a program
/// over it compiles again, to tell which limit it passed.
fn prepare(source: &str, view: Option<&str>) -> Result<Program, PatternError> {
    let hir = parse_hir(source)?;
    let (regex, profile) = match view {
        None => compile(&hir, MAX_REGEX_COMPILED_BYTES)?,
        Some(view) if view == source => match compile(&hir, MAX_BATCH_COMPILED_BYTES) {
            Err(PatternError::CompiledSize) => {
                compile(&hir, MAX_REGEX_COMPILED_BYTES)?;
                return Err(PatternError::BatchCompiledSize);
            }
            built => built?,
        },
        Some(view) => {
            let program = compile(&hir, MAX_REGEX_COMPILED_BYTES)?;
            match parse_hir(view).and_then(|view| compile(&view, MAX_BATCH_COMPILED_BYTES)) {
                Ok(_) => program,
                Err(PatternError::CompiledSize) => return Err(PatternError::BatchCompiledSize),
                Err(other) => return Err(other),
            }
        }
    };
    let charge = Charge::new(&profile, regex.memory_usage());
    Ok(Program { regex, charge })
}

/// `source` parsed as the `regex` crate's `Regex` parses it: every flag at
/// the value `RegexBuilder::new` leaves it, and UTF-8 on (regex 1.13.1,
/// `builders.rs:69-83`).
fn parse_hir(source: &str) -> Result<Hir, PatternError> {
    let syntax = syntax::Config::new()
        .case_insensitive(false)
        .multi_line(false)
        .dot_matches_new_line(false)
        .crlf(false)
        .line_terminator(b'\n')
        .swap_greed(false)
        .ignore_whitespace(false)
        .unicode(true)
        .utf8(true)
        .nest_limit(250)
        .octal(false);
    syntax::parse_with(source, &syntax).map_err(|e| PatternError::Syntax(e.to_string()))
}

/// The meta configuration every pattern builds under: the `regex` crate's
/// own, with the NFA size limit `limit` and the lazy-DFA cache capacity
/// `capacity`, a direction.
///
/// Two engines stay off: the full DFA, which another crate can unify in
/// through regex-automata's `dfa-build` feature and which would make one
/// pattern's memory differ by crate, and the bounded backtracker, whose
/// scratch has no bound the budget can charge (ADR-0063).
fn meta_config(limit: usize, capacity: usize) -> meta::Config {
    meta::Config::new()
        .match_kind(MatchKind::LeftmostFirst)
        .utf8_empty(true)
        .nfa_size_limit(Some(limit))
        .hybrid_cache_capacity(capacity)
        .dfa(false)
        .backtrack(false)
}

/// The configuration meta compiles its forward NFA under, from its own
/// (`meta::Config::to_thompson_config`, regex-automata 0.4.16
/// `meta/regex.rs:3282-3292`).
fn forward_nfa_config(config: &meta::Config) -> thompson::Config {
    let mut look = LookMatcher::new();
    look.set_line_terminator(config.get_line_terminator());
    thompson::Config::new()
        .utf8(config.get_utf8_empty())
        .reverse(false)
        .nfa_size_limit(config.get_nfa_size_limit())
        .shrink(false)
        .which_captures(config.get_which_captures())
        .look_matcher(look)
}

/// The one regex build site (ADR-0063): `hir` compiled to a meta program
/// under the size limit `limit`, with the profile its charge and its
/// lazy-DFA capacity come from. Every build that prepares a pattern runs
/// through [`prepare`].
///
/// It first compiles the forward NFA meta compiles, under the same limit,
/// reads its profile and drops it. A body over the limit fails there, as
/// it would in meta. The program then builds at the capacity the profile
/// sets ([`Profile::capacity`]). It compiles as
/// `regex::RegexBuilder::new(source).size_limit(limit).build()` did, so a
/// pattern matches, captures and fails with the messages it always has.
#[expect(
    clippy::disallowed_methods,
    reason = "the one regex construction site; every other build goes through a Preparer (ADR-0063)"
)]
fn compile(hir: &Hir, limit: usize) -> Result<(meta::Regex, Profile), PatternError> {
    let config = meta_config(limit, charge::CACHE_MAX);
    let profile = thompson::Compiler::new()
        .configure(forward_nfa_config(&config))
        .build_from_hir(hir)
        .map(|nfa| Profile::of(&nfa))
        .map_err(|e| match e.size_limit() {
            Some(_) => PatternError::CompiledSize,
            None => PatternError::Syntax(e.to_string()),
        })?;
    let regex = meta::Builder::new()
        .configure(config.hybrid_cache_capacity(profile.capacity()))
        .build_from_hir(hir)
        // As the regex crate maps a meta build error
        // (`Error::from_meta_build_error`, regex 1.13.1 `error.rs:35-53`).
        .map_err(|e| match (e.size_limit(), e.syntax_error()) {
            (Some(_), _) => PatternError::CompiledSize,
            (None, Some(syntax)) => PatternError::Syntax(syntax.to_string()),
            (None, None) => PatternError::Syntax(e.to_string()),
        })?;
    Ok((regex, profile))
}

/// Translate a SQL `like` pattern to an anchored regex: `%` matches any
/// sequence, `_` one character, and every regex metacharacter is
/// escaped. `case_insensitive` (`ilike`) prefixes `(?i)`.
fn like_to_regex(pattern: &str, case_insensitive: bool) -> String {
    let mut regex_str = String::from("^");
    for ch in pattern.chars() {
        match ch {
            '%' => regex_str.push_str(".*"),
            '_' => regex_str.push('.'),
            // escape regex metacharacters
            '.' | '^' | '$' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\\' => {
                regex_str.push('\\');
                regex_str.push(ch);
            }
            _ => regex_str.push(ch),
        }
    }
    regex_str.push('$');
    if case_insensitive {
        regex_str.insert_str(0, "(?i)");
    }
    regex_str
}

/// Convert a `DuckDB` GLOB pattern to an anchored, dot-all regex.
///
/// `DuckDB` GLOB is a full-match pattern (like SQL LIKE):
/// - `*` matches any sequence of characters, newlines included
/// - `?` matches any single character, a newline included
/// - `[...]` matches a character class (`[!...]` for negation)
/// - Everything else is literal (case-sensitive)
///
/// The newline rule is probed, not assumed: without `(?s)` a multiline
/// value matches `f=*` in batch and not live, and `f!=*` inverts that
/// (the complement matrix in `tests/filter_parity.rs` carries the row).
fn glob_to_regex(glob: &str) -> String {
    let mut regex = String::from("(?s)^");
    let mut chars = glob.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '*' => regex.push_str(".*"),
            '?' => regex.push('.'),
            '[' => {
                regex.push('[');
                // DuckDB uses `!` for negation in character classes.
                if chars.peek() == Some(&'!') {
                    chars.next();
                    regex.push('^');
                }
                // Copy until closing `]`.
                let mut first = true;
                loop {
                    match chars.next() {
                        Some(']') if !first => {
                            regex.push(']');
                            break;
                        }
                        Some(nc) => {
                            regex.push(nc);
                            first = false;
                        }
                        None => {
                            // Unterminated bracket — close it.
                            regex.push(']');
                            break;
                        }
                    }
                }
            }
            c if is_regex_meta(c) => {
                regex.push('\\');
                regex.push(c);
            }
            c => regex.push(c),
        }
    }

    regex.push('$');
    regex
}

/// Characters that have special meaning in regex and need escaping.
fn is_regex_meta(c: char) -> bool {
    matches!(
        c,
        '.' | '+' | '^' | '$' | '|' | '(' | ')' | '{' | '}' | '\\'
    )
}
