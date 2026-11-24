// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The bind-time expansion budget (ADR-0024).
//!
//! [`profile`] states, per rendering, how many copies of each child the
//! emitter writes; the drift tests in `tests/complexity_emission_bounds.rs`
//! hold those numbers against the real emitter. A profile may overcount
//! fixed overhead. It may never omit a child copy.
//!
//! `DuckDB`'s parser rewrites a SIMPLE `CASE` — one with a subject at its
//! head — into one equality predicate per `WHEN` arm, at PARSE time, before
//! the binder ever sees a lateral alias. A SEARCHED `CASE` is not
//! rewritten. Text-level drift tests count the wrong side of the parser, so
//! the copy counts here are held against the PARSED tree by
//! `trawl-engine/tests/duckdb_probe.rs`.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;

use crate::ast::{
    AggExpr, BinaryOp, Expr, ExtractMode, FilterOp, FloatLiteral, LiteralValue, PipeStage,
    PivotStage, Spanned,
};
use crate::compare::{self, CompareForm, PatternForm};
use crate::parser::suggest::quote_dsl_field;
use crate::pattern::PatternKind;
use crate::projection::agg_output_name;
use crate::schema::{CanonicalType, catalog_key};
use crate::severity::Dialect;

/// The most additional substitution one query may describe (ADR-0024).
pub const MAX_LATERAL_EXPANSION: u64 = 512;

/// The most pipeline stages one query may carry (ADR-0024).
pub const MAX_PIPELINE_STAGES: usize = 128;

/// The most fields one `sort` or `dedup` stage may name (ADR-0024,
/// Amendment 2026-11-09).
pub const MAX_STAGE_FIELDS: usize = 64;

/// `DuckDB`'s default `pivot_filter_threshold`, which trawl never sets.
/// With at most this many pivot values, `DuckDB`'s binder writes one
/// filtered aggregate per value, each a whole copy of the aggregate, regex
/// patterns and all. `trawl-engine/tests/integration.rs` holds this value
/// against the setting the engine's connections run with.
pub const PIVOT_FILTER_THRESHOLD: u64 = 20;

/// The most contiguous ranges a `SEVERITY` set can render
/// ([`compare::severity_ranges`]): the twelve non-adjacent points the 1-24
/// ladder admits, plus the single representative every out-of-ladder point
/// collapses onto.
const MAX_SEVERITY_RUNS: usize = 13;

const EVENTSTATS_OVER_FIXED: u64 = 1;

/// Regex patterns bound as `DuckDB` `regexp_*` arguments (ADR-0063).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RegexLoad {
    pub copies: u64,
    pub bytes: u64,
}

impl RegexLoad {
    pub const NONE: Self = Self {
        copies: 0,
        bytes: 0,
    };

    #[must_use]
    pub fn of(pattern: &str) -> Self {
        Self {
            copies: 1,
            bytes: u64::try_from(pattern.len()).unwrap_or(u64::MAX),
        }
    }

    #[must_use]
    pub fn plus(self, other: Self) -> Self {
        Self {
            copies: self.copies.saturating_add(other.copies),
            bytes: self.bytes.saturating_add(other.bytes),
        }
    }

    #[must_use]
    pub fn times(self, times: u64) -> Self {
        Self {
            copies: self.copies.saturating_mul(times),
            bytes: self.bytes.saturating_mul(times),
        }
    }

    #[must_use]
    fn max_with(self, other: Self) -> Self {
        Self {
            copies: self.copies.max(other.copies),
            bytes: self.bytes.max(other.bytes),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Weight {
    pub w: u64,
    pub d: u64,
    /// The regex patterns the emitter writes for the expression, one bound
    /// parameter each.
    pub regex: RegexLoad,
    /// The further regex copies `DuckDB` binds: every pattern inside a
    /// substituted alias, and each extra copy of a written pattern that a
    /// rendering writes more than once.
    pub regex_copies: RegexLoad,
}

impl Weight {
    pub const LEAF: Self = Self {
        w: 1,
        d: 0,
        regex: RegexLoad::NONE,
        regex_copies: RegexLoad::NONE,
    };

    #[must_use]
    pub fn substituted(target: Self) -> Self {
        Self {
            w: target.w,
            d: target.w.saturating_sub(1),
            regex: RegexLoad::NONE,
            regex_copies: target.regex.plus(target.regex_copies),
        }
    }

    #[must_use]
    pub fn max_with(self, other: Self) -> Self {
        Self {
            w: self.w.max(other.w),
            d: self.d.max(other.d),
            regex: self.regex.max_with(other.regex),
            regex_copies: self.regex_copies.max_with(other.regex_copies),
        }
    }

    #[must_use]
    fn writing(self, pattern: RegexLoad) -> Self {
        Self {
            regex: self.regex.plus(pattern),
            ..self
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderProfile {
    /// Nodes the rendering contributes regardless of its children.
    pub fixed: u64,
    /// How many times each child is written, in argument order.
    pub copies: Vec<u64>,
}

impl RenderProfile {
    fn new(fixed: u64, copies: Vec<u64>) -> Self {
        Self { fixed, copies }
    }

    fn uniform(fixed: u64, n: usize) -> Self {
        Self::new(fixed, vec![1; n])
    }

    /// Total by construction: a child list shorter or longer than `copies`
    /// stops at the shorter of the two rather than indexing past either,
    /// because scoring must reach the emitter's own semantic refusal for a
    /// malformed call instead of panicking ahead of it.
    #[must_use]
    pub fn apply(&self, children: &[Weight]) -> Weight {
        let mut out = Weight {
            w: self.fixed,
            d: 0,
            regex: RegexLoad::NONE,
            regex_copies: RegexLoad::NONE,
        };
        for (m, child) in self.copies.iter().zip(children) {
            out.w = out.w.saturating_add(m.saturating_mul(child.w));
            out.d = out.d.saturating_add(m.saturating_mul(child.d));
            out.regex = out.regex.plus(child.regex);
            out.regex_copies = out
                .regex_copies
                .plus(child.regex_copies.times(*m))
                .plus(child.regex.times(m.saturating_sub(1)));
        }
        out
    }
}

/// A distinct type rather than a [`RenderProfile`] with one entry so the
/// composition below can read the copy count without an `Option`: a
/// defaulted `None` there would be a silently omitted child copy, which is
/// the one mistake the whole table may not make.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OneChild {
    fixed: u64,
    copies: u64,
}

impl OneChild {
    const fn new(fixed: u64, copies: u64) -> Self {
        Self { fixed, copies }
    }

    fn profile(self) -> RenderProfile {
        RenderProfile::new(self.fixed, vec![self.copies])
    }

    fn wrapped(self, extra: u64) -> Self {
        Self {
            fixed: self.fixed.saturating_add(extra),
            copies: self.copies,
        }
    }
}

/// A simple `CASE <this shape> WHEN v THEN r … END` over `arms` arms, as
/// `DuckDB`'s PARSER leaves it: `CASE WHEN subject = v THEN r …`, so each
/// arm costs one equality node, the arm's value, the arm's result, and a
/// WHOLE further copy of the subject expression.
fn simple_case(subject: OneChild, arms: u64) -> OneChild {
    OneChild {
        fixed: 2u64
            .saturating_add(arms.saturating_mul(3))
            .saturating_add(arms.saturating_mul(subject.fixed)),
        copies: arms.saturating_mul(subject.copies),
    }
}

/// One comparison clause's emitted shape
/// (`crate::emitter::compare::comparison_sql`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareKind {
    /// `subject OP ?` — one bound parameter. Every typed pin binds this
    /// way, as does an unpinned `Text`/`SeverityExact` literal.
    Bound,
    /// `TRY_CAST(subject AS DECIMAL(38,6)) OP TRY_CAST(? AS DECIMAL(38,6))`
    /// — the VARCHAR pin's ordered numeric comparison.
    NumericOnText,
    /// `(subject = ? OR COALESCE(dec(subject) = dec(?), FALSE))` — the
    /// VARCHAR pin's equality against a numeric literal, which names its
    /// subject twice.
    TextOrNumeric,
    /// A `SEVERITY` band reached through a MIXED `IN` list. No single pin can
    /// produce that mixture, so this arm is defensive and takes the widest
    /// set the ladder can render.
    SeverityBand,
}

/// One DSL function's emitted shape, at the arity it was called with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FunctionShape {
    /// One wrapper node over `n` arguments, each written once —
    /// `AVG(a)`, `IF(a, b, c)`, `COALESCE(a, …)`.
    Plain(usize),
    /// `case(c1, v1, …[, fallback])`: the searched `CASE`, whose fixed
    /// cost depends on the parity of its argument list.
    ///
    /// An odd arity writes the trailing `ELSE`. An even one writes none,
    /// and `DuckDB`'s parser fills in `ELSE NULL` before the binder ever
    /// sees the tree, so the node is there either way
    /// (`duckdb_probe::an_else_less_case_still_binds_an_else_node`).
    Case(usize),
    /// `COUNT(*)` — the row counter, whose star is a node of its own.
    CountStar,
    /// `(a IS NULL)`.
    IsNull,
    /// `(a IS NOT NULL)`.
    IsNotNull,
    /// `PERCENTILE_CONT(<p>) WITHIN GROUP (ORDER BY a)` — the aggregate
    /// and its inlined fraction.
    Percentile,
    /// `ROUND(a, <int>)` — the precision argument is inlined, never
    /// written as an operand.
    RoundPrecision,
    /// `STRING_SPLIT(a, b)[<int> + 1]` — the index argument is inlined.
    Split,
    /// `CAST(? AS TIMESTAMP)` — `now()` binds the query's anchor.
    Now,
    /// `F(TRY_CAST(a AS JSON)[, b])`: a json function over `n` arguments,
    /// each written once, whose document passes through a cast
    /// (ADR-0017, amendment of 2026-11-13).
    JsonGuarded(usize),
    /// `sev(x[, dialect])`: the severity reading kernel, bound once, over
    /// the argument's canonical text form. The dialect argument is inlined
    /// text and never an operand.
    Sev { dialect: Dialect, argc: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConformShape {
    UntypedText,
    DecimalReading,
    GuardedCast(CanonicalType, Dialect),
    /// [`crate::conform::severity_reading_sql`] — the repeated-subject
    /// shape the conform rung emits.
    SeverityReading(Dialect),
    /// [`crate::conform::severity_reading_sql_bind_once`] — the shape
    /// `sev()` emits, whose subject may carry bound parameters.
    SeverityReadingBindOnce(Dialect),
    SeverityTokenText,
}

/// [`profile`] is exhaustive over this enum with no wildcard arm, so a new
/// rendering fails to compile until it states its cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rendering {
    /// A field reference, a literal, or a bound parameter.
    Leaf,
    /// `(lhs OP rhs)` — the generic binary operator, both operands written
    /// once.
    Binary,
    /// `(NOT x)` / `(-x)`.
    Unary,
    /// `(x IN (e1, …, en))` — pin-blind list membership, every operand
    /// written once.
    InListPlain(usize),
    /// `subject IN (?, …)` over `n` bound elements — the non-expanding
    /// pinned list, which writes its subject once.
    InListBound(usize),
    /// The expanding pinned list: one predicate per element, OR'd.
    InListExpanded(Vec<CompareKind>),
    /// One comparison clause.
    Compare(CompareKind),
    /// A glob/regex/LIKE operator over the pin's canonical pattern text.
    Pattern(PatternForm),
    /// A `SEVERITY` set rendered as `runs` contiguous ranges, optionally
    /// negated — the subject is written once per run.
    SeverityRanges { runs: usize, negated: bool },
    /// One DSL function's translation.
    Function(FunctionShape),
    /// A call whose name no inventory entry claims. Structural and benign:
    /// it exists so the walk can finish and hand the query to the
    /// emitter's own unknown-function refusal, and
    /// `every_known_function_has_a_profile` proves nothing that emits
    /// successfully ever reaches it.
    UnknownCall(usize),
    /// One `crate::conform` expression helper.
    Conform(ConformShape),
}

/// Exhaustive, with no wildcard arm and no generic function fallback: a
/// rendering that has not stated its cost must fail to compile rather than
/// score as something cheaper.
#[must_use]
#[allow(clippy::match_same_arms)]
pub fn profile(rendering: &Rendering) -> RenderProfile {
    match rendering {
        Rendering::Leaf => RenderProfile::new(1, Vec::new()),
        // The operator node itself.
        Rendering::Binary => RenderProfile::uniform(1, 2),
        Rendering::Unary => RenderProfile::uniform(1, 1),
        // The `IN` node, plus the subject and every element once.
        Rendering::InListPlain(n) => RenderProfile::uniform(1, n + 1),
        // `subject IN (?, …)`: the `IN` node and one parameter per element.
        Rendering::InListBound(n) => RenderProfile::new(1 + n_u64(*n), vec![1]),
        Rendering::InListExpanded(elements) => {
            let joins = n_u64(elements.len()).saturating_sub(1);
            let mut fixed = joins;
            let mut copies = 0u64;
            for kind in elements {
                let element = profile(&Rendering::Compare(*kind));
                fixed = fixed.saturating_add(element.fixed);
                copies = copies.saturating_add(element.copies.first().copied().unwrap_or(0));
            }
            RenderProfile::new(fixed, vec![copies])
        }
        // `subject OP ?`: the operator and the parameter.
        Rendering::Compare(CompareKind::Bound) => RenderProfile::new(2, vec![1]),
        // Two casts, one parameter, the operator.
        Rendering::Compare(CompareKind::NumericOnText) => RenderProfile::new(4, vec![1]),
        // `(subject = ?, OR, COALESCE, FALSE, cast, =, cast, ?)` around two
        // writings of the subject.
        Rendering::Compare(CompareKind::TextOrNumeric) => RenderProfile::new(9, vec![2]),
        Rendering::Compare(CompareKind::SeverityBand) => profile(&Rendering::SeverityRanges {
            runs: MAX_SEVERITY_RUNS,
            negated: true,
        }),
        // `regexp_matches(target, ?)` / `(target LIKE ?)` over the pin's
        // pattern text.
        Rendering::Pattern(form) => {
            let target = match form {
                // The column itself; the emitter keeps this on the generic
                // path, whose operand count is the same.
                PatternForm::Native => OneChild::new(0, 1),
                // `CAST(target AS VARCHAR)`.
                PatternForm::BigIntText | PatternForm::BooleanText | PatternForm::DoubleText => {
                    OneChild::new(1, 1)
                }
                // `strftime(target, '<format>')`.
                PatternForm::Rfc3339Text => OneChild::new(2, 1),
                // The token table is a SIMPLE `CASE`, so the pattern's
                // target carries the whole subject once per ladder arm.
                PatternForm::SeverityText => severity_token_text(),
            };
            // The pattern operator and its bound parameter.
            target.wrapped(2).profile()
        }
        Rendering::SeverityRanges { runs, negated } => {
            let runs = n_u64(*runs);
            // Four nodes per range (`subject BETWEEN lo AND hi`; a
            // one-point run writes `= p`, two nodes, and is overcounted),
            // one OR between consecutive runs, and the `NOT` wrapper for
            // `!=`.
            let fixed = runs
                .saturating_mul(4)
                .saturating_add(runs.saturating_sub(1))
                .saturating_add(u64::from(*negated));
            RenderProfile::new(fixed, vec![runs])
        }
        Rendering::Function(shape) => function_profile(*shape),
        Rendering::UnknownCall(n) => RenderProfile::uniform(1, *n),
        Rendering::Conform(shape) => conform_profile(*shape),
    }
}

fn n_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

#[allow(clippy::match_same_arms)]
fn function_profile(shape: FunctionShape) -> RenderProfile {
    match shape {
        FunctionShape::Plain(n) => RenderProfile::uniform(1, n),
        // The `CASE` node, plus the `ELSE` the parser supplies when the
        // query wrote none.
        FunctionShape::Case(n) => RenderProfile::uniform(if n % 2 == 0 { 2 } else { 1 }, n),
        // `COUNT(*)`: the aggregate and the star.
        FunctionShape::CountStar => RenderProfile::new(2, Vec::new()),
        FunctionShape::IsNull => RenderProfile::new(2, vec![1]),
        FunctionShape::IsNotNull => RenderProfile::new(3, vec![1]),
        // The aggregate and its inlined percentile fraction.
        FunctionShape::Percentile => RenderProfile::new(2, vec![1]),
        // `ROUND(a, 2)`: the call and the inlined precision literal.
        FunctionShape::RoundPrecision => RenderProfile::new(2, vec![1, 0]),
        // `STRING_SPLIT(a, b)[i + 1]`: the call, the list index and the
        // inlined index literal.
        FunctionShape::Split => RenderProfile::new(3, vec![1, 1, 0]),
        // `CAST(? AS TIMESTAMP)`.
        FunctionShape::Now => RenderProfile::new(2, Vec::new()),
        // The call and the document's `TRY_CAST`.
        FunctionShape::JsonGuarded(n) => RenderProfile::uniform(2, n),
        FunctionShape::Sev { dialect, argc } => {
            let reading = conform_profile(ConformShape::SeverityReadingBindOnce(dialect));
            let text = conform_profile(ConformShape::UntypedText);
            // `sev()` reads its subject through the canonical text form,
            // and the reading writes that form once. A bare literal at
            // that position also carries its own `CAST(? AS T)`, one more
            // node than a column reference.
            let fixed = reading.fixed.saturating_add(text.fixed).saturating_add(1);
            let mut copies = vec![1];
            if argc > 1 {
                // The dialect token is inlined into the chosen expression,
                // never written as an operand.
                copies.push(0);
            }
            RenderProfile::new(fixed, copies)
        }
    }
}

fn conform_profile(shape: ConformShape) -> RenderProfile {
    match shape {
        // `json_extract_string(to_json(x), '$')`.
        ConformShape::UntypedText => RenderProfile::new(3, vec![1]),
        // `TRY_CAST(x AS DECIMAL(38,6))`.
        ConformShape::DecimalReading => RenderProfile::new(1, vec![1]),
        ConformShape::GuardedCast(pin, dialect) => match pin {
            // The text form IS the conform.
            CanonicalType::Varchar => RenderProfile::new(0, vec![1]),
            // `(CASE WHEN dec(t) = dec(TRY_CAST(t AS BIGINT)) THEN
            //   TRY_CAST(t AS BIGINT) END)` — SEARCHED, so the parser
            //   leaves it alone: the `CASE`, its NULL fallthrough, the
            //   equality, two decimal casts and two integer casts.
            CanonicalType::BigInt => RenderProfile::new(7, vec![3]),
            // `(CASE WHEN CAST(TRY_CAST(t AS BOOLEAN) AS VARCHAR) = t THEN
            //   TRY_CAST(t AS BOOLEAN) END)`, searched likewise.
            CanonicalType::Boolean => RenderProfile::new(6, vec![3]),
            CanonicalType::Double => RenderProfile::new(1, vec![1]),
            // `TRY_CAST(TRY_CAST(t AS TIMESTAMPTZ) AS TIMESTAMP)`.
            CanonicalType::Timestamp => RenderProfile::new(2, vec![1]),
            CanonicalType::Severity => conform_profile(ConformShape::SeverityReading(dialect)),
        },
        ConformShape::SeverityReading(dialect) => {
            let case = reading_case(dialect);
            // `CAST(<case over trimmed(text)> AS BIGINT)`: the outer cast,
            // the case's own nodes, and one trim per writing of the
            // subject.
            OneChild::new(
                1 + case.fixed + case.copies.saturating_mul(TRIMMED_FIXED),
                case.copies,
            )
            .profile()
        }
        ConformShape::SeverityReadingBindOnce(dialect) => {
            let case = reading_case(dialect);
            // `CAST(list_transform([trimmed(text)], _sev -> <case>)[1] AS
            // BIGINT)`: the cast, the transform, the list, the lambda and
            // its parameter, the index and its literal, one trim, the
            // case, and the LAMBDA-LOCAL parameter once per writing. The
            // subject itself lands once, inside the list literal, which is
            // the whole point of this shape — the parser's `CASE` rewrite
            // multiplies `_sev`, not the caller's expression.
            OneChild::new(7 + TRIMMED_FIXED + case.fixed + case.copies, 1).profile()
        }
        // `(CASE x WHEN 1 THEN 'trace' … WHEN 24 THEN 'fatal4' END)`.
        ConformShape::SeverityTokenText => severity_token_text().profile(),
    }
}

/// Nodes in `regexp_replace(regexp_replace(x, '…', ''), '…', '')` — the
/// whitespace trim both severity readings wrap their subject in.
const TRIMMED_FIXED: u64 = 6;

/// The arm count is read off the severity tables rather than frozen, so a
/// token added there widens the profile instead of silently escaping it.
///
/// The shape is a SIMPLE `CASE` whose head is itself an expression naming
/// the subject twice, so the parser's rewrite dominates everything else
/// here: thirty-eight arms times two writings is seventy-six copies of the
/// subject before the numeric fallthrough adds its own.
fn reading_case(dialect: Dialect) -> OneChild {
    // `(CASE WHEN regexp_full_match(t, '[A-Za-z0-9]+') THEN lower(t) END)`
    // — the `CASE`, its NULL fallthrough, the match call, its pattern and
    // the fold, over two writings of the subject.
    let gate = OneChild::new(5, 2);
    let arms = n_u64(severity_case_arms());
    let numeric = reading_numeric_arm(dialect);
    let case = simple_case(gate, arms);
    // The `ELSE` arm is written once and never repeated.
    OneChild::new(
        case.fixed.saturating_add(numeric.fixed),
        case.copies.saturating_add(numeric.copies),
    )
}

/// The reading's numeric fallthrough, over the same trimmed subject:
/// `(CASE WHEN regexp_full_match(t, '[+-]?[0-9]+') THEN <dialect> END)`.
fn reading_numeric_arm(dialect: Dialect) -> OneChild {
    // `TRY_CAST(t AS BIGINT)`.
    let cast = OneChild::new(1, 1);
    let numeric = match dialect {
        // `(CASE WHEN cast BETWEEN 1 AND 24 THEN cast END)`: searched, so
        // no rewrite — the `CASE`, its NULL fallthrough, the `BETWEEN`
        // and its two bounds, over two writings of the cast.
        Dialect::Otel => OneChild::new(5 + 2 * cast.fixed, cast.copies.saturating_mul(2)),
        // `(CASE cast WHEN 0 THEN … WHEN 7 THEN … END)`: SIMPLE, eight
        // rungs, so the cast lands eight times.
        Dialect::Syslog => simple_case(cast, 8),
    };
    // The digits guard: the `CASE`, its NULL fallthrough, the match call
    // and its pattern, over one further writing of the subject.
    OneChild::new(
        numeric.fixed.saturating_add(4),
        numeric.copies.saturating_add(1),
    )
}

fn severity_case_arms() -> usize {
    static ARMS: std::sync::LazyLock<usize> = std::sync::LazyLock::new(|| {
        let table: Vec<&str> = crate::severity::token_entries().map(|(t, _)| t).collect();
        let extras = (1..=24u8)
            .filter(|n| crate::severity::otel_name(*n).is_some_and(|name| !table.contains(&name)))
            .count();
        table.len() + extras
    });
    *ARMS
}

/// `crate::conform::severity_token_text_sql`: a SIMPLE `CASE` over
/// twenty-four ladder arms, which the parser rewrites into twenty-four
/// equalities — so the subject lands twenty-four times, not once.
fn severity_token_text() -> OneChild {
    simple_case(OneChild::new(0, 1), 24)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    Lateral,
    /// [`MAX_PIPELINE_STAGES`], with the stage count the query carried.
    Stages(usize),
    /// [`MAX_STAGE_FIELDS`], with the field count the stage wrote.
    Fields(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComplexityRefusal {
    /// Zero-based index of the offending stage in the pipeline.
    pub stage_index: usize,
    pub stage: &'static str,
    /// The output whose scoring crossed the budget, rendered as DSL text.
    /// `None` where the stage has no output sequence, or where the name
    /// cannot be spelled in the DSL at all.
    pub target: Option<String>,
    pub limit: Limit,
}

impl ComplexityRefusal {
    fn remedy(&self) -> &'static str {
        match self.stage {
            "let" | "eval" => {
                "split the dependent assignments across separate `| let` stages, so each one \
                 reads a finished column"
            }
            _ => {
                "give each output an expression of its own, and compute derived values in \
                 separate `| let` stages after this one"
            }
        }
    }
}

impl fmt::Display for ComplexityRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.limit {
            Limit::Stages(count) => write!(
                f,
                "this query has {count} pipeline stages, over the limit of \
                 {MAX_PIPELINE_STAGES}; shorten the pipeline, or save part of it and read it \
                 back with `from saved`"
            ),
            Limit::Fields(count) => write!(
                f,
                "pipeline stage {} (`{}`) names {count} fields, over the limit of \
                 {MAX_STAGE_FIELDS} for one `sort` or `dedup` stage",
                self.stage_index + 1,
                self.stage
            ),
            Limit::Lateral => {
                let stage = self.stage;
                let n = self.stage_index + 1;
                match &self.target {
                    Some(name) => write!(
                        f,
                        "pipeline stage {n} (`{stage}`) goes over this query's \
                         alias-expansion budget of {MAX_LATERAL_EXPANSION} at the output \
                         {name}: an output naming an earlier output of the same stage is \
                         written into it once for every place it is named, so the work \
                         multiplies — {}",
                        self.remedy()
                    ),
                    None => write!(
                        f,
                        "pipeline stage {n} (`{stage}`) goes over this query's \
                         alias-expansion budget of {MAX_LATERAL_EXPANSION}: an output \
                         naming an earlier output of the same stage is written into it once \
                         for every place it is named, so the work multiplies — {}",
                        self.remedy()
                    ),
                }
            }
        }
    }
}

impl std::error::Error for ComplexityRefusal {}

/// [`quote_dsl_field`] declines a name the field grammar cannot spell
/// (empty, or carrying a control or invisible display character), and a
/// declined name is simply omitted — the stage alone still says where to
/// look, and a refusal an operator reads in a terminal must never carry
/// text the query author chose to hide a line with.
fn refusal_name(name: &str) -> Option<String> {
    let dsl = crate::sanitize::sanitize_display_text(&quote_dsl_field(name)?);
    Some(if dsl.starts_with('`') {
        dsl
    } else {
        format!("`{dsl}`")
    })
}

/// What one check visited — the evidence that the walk is linear in source
/// size rather than in expanded weight.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CheckStats {
    /// AST nodes entered, plus one per pin interpretation priced.
    pub visited_nodes: u64,
    pub stages: usize,
    /// The summed assignment delta, against [`MAX_LATERAL_EXPANSION`].
    pub lateral_delta: u64,
}

/// Admit a pipeline under the bind-time expansion budget (ADR-0024).
pub fn check_pipeline_complexity(stages: &[Spanned<PipeStage>]) -> Result<(), ComplexityRefusal> {
    check_pipeline_complexity_with_stats(stages).0
}

pub fn check_pipeline_complexity_with_stats(
    stages: &[Spanned<PipeStage>],
) -> (Result<(), ComplexityRefusal>, CheckStats) {
    let mut stats = CheckStats::default();

    if stages.len() > MAX_PIPELINE_STAGES {
        return (
            Err(ComplexityRefusal {
                stage_index: MAX_PIPELINE_STAGES,
                stage: stages
                    .get(MAX_PIPELINE_STAGES)
                    .map_or("pipeline", |s| stage_keyword(&s.node)),
                target: None,
                limit: Limit::Stages(stages.len()),
            }),
            stats,
        );
    }

    stats.stages = stages.len();
    let mut total: u64 = 0;
    for (index, stage) in stages.iter().enumerate() {
        let outcome = score_stage(index, &stage.node, &mut total, &mut stats);
        stats.lateral_delta = total;
        if let Err(refusal) = outcome {
            return (Err(refusal), stats);
        }
    }
    (Ok(()), stats)
}

/// The per-stage alias table: an output's name, folded through
/// [`catalog_key`], against its expanded weight.
type Memo = HashMap<String, Weight>;

/// The match is exhaustive with no wildcard arm: a new stage variant must
/// state whether it has a recursive output-alias sequence before it can
/// compile, rather than defaulting to free.
#[allow(clippy::match_same_arms)]
fn score_stage(
    index: usize,
    stage: &PipeStage,
    total: &mut u64,
    stats: &mut CheckStats,
) -> Result<(), ComplexityRefusal> {
    let keyword = stage_keyword(stage);
    let mut memo = Memo::new();
    match stage {
        PipeStage::Let(l) => {
            for (target, expr) in &l.assignments {
                let weight = score_expr(expr, &memo, stats);
                charge(index, keyword, Some(target), weight.d, total)?;
                memo.insert(catalog_key(target), weight);
            }
        }
        PipeStage::Stats(s) => {
            score_aggregations(index, keyword, &s.aggregations, 0, total, stats)?;
        }
        PipeStage::Timechart(t) => {
            score_aggregations(index, keyword, &t.aggregations, 0, total, stats)?;
        }
        PipeStage::EventStats(es) => {
            let window = EVENTSTATS_OVER_FIXED.saturating_add(n_u64(es.group_by.len()));
            score_aggregations(index, keyword, &es.aggregations, window, total, stats)?;
        }
        PipeStage::Pivot(p) => {
            let weight = score_agg(&p.aggregation, &Memo::new(), 0, stats);
            charge(index, keyword, None, weight.d, total)?;
        }
        PipeStage::Rename(r) => {
            // Recorded all the same: the stage's own outputs are what a
            // same-stage reference would find.
            for (_, new) in &r.renames {
                memo.insert(catalog_key(new), Weight::LEAF);
            }
        }
        // Both arms are spelled out, and their bodies are identical on
        // purpose: neither extraction mode has a recursive output-alias
        // sequence, and merging them would let a third mode inherit that
        // answer without stating it.
        PipeStage::Extract(e) => match &e.mode {
            // One fixed source per capture group.
            ExtractMode::Regex(_) => {}
            // One fixed source for the whole stage.
            ExtractMode::KeyValue { .. } => {}
        },
        PipeStage::Sort(s) => check_field_list(index, keyword, s.fields.len())?,
        PipeStage::Dedup(d) => check_field_list(index, keyword, d.fields.len())?,
        PipeStage::Where(_)
        | PipeStage::Limit(_)
        | PipeStage::Table(_)
        | PipeStage::Top(_)
        | PipeStage::Rare(_)
        | PipeStage::Drop(_)
        | PipeStage::Tail(_)
        | PipeStage::Sample(_)
        | PipeStage::FromSaved(_) => {}
    }
    Ok(())
}

fn check_field_list(
    index: usize,
    keyword: &'static str,
    count: usize,
) -> Result<(), ComplexityRefusal> {
    if count > MAX_STAGE_FIELDS {
        return Err(ComplexityRefusal {
            stage_index: index,
            stage: keyword,
            target: None,
            limit: Limit::Fields(count),
        });
    }
    Ok(())
}

fn score_aggregations(
    index: usize,
    keyword: &'static str,
    aggregations: &[AggExpr],
    wrapper: u64,
    total: &mut u64,
    stats: &mut CheckStats,
) -> Result<(), ComplexityRefusal> {
    let mut memo = Memo::new();
    for agg in aggregations {
        let weight = score_agg(agg, &memo, wrapper, stats);
        let name = agg_output_name(agg);
        charge(index, keyword, Some(&name), weight.d, total)?;
        memo.insert(catalog_key(&name), weight);
    }
    Ok(())
}

/// The regex pattern copies `DuckDB` binds for one stage beyond the
/// patterns the emitter writes for it (ADR-0063).
#[must_use]
#[allow(clippy::match_same_arms)]
pub fn stage_regex_copies(stage: &PipeStage) -> RegexLoad {
    let mut stats = CheckStats::default();
    match stage {
        PipeStage::Let(l) => {
            let mut memo = Memo::new();
            let mut copies = RegexLoad::NONE;
            for (target, expr) in &l.assignments {
                let weight = score_expr(expr, &memo, &mut stats);
                copies = copies.plus(weight.regex_copies);
                memo.insert(catalog_key(target), weight);
            }
            copies
        }
        PipeStage::Stats(s) => aggregation_regex_copies(&s.aggregations, &mut stats),
        PipeStage::Timechart(t) => aggregation_regex_copies(&t.aggregations, &mut stats),
        PipeStage::EventStats(es) => aggregation_regex_copies(&es.aggregations, &mut stats),
        // The aggregate binds whole once per pivot value, up to
        // [`PIVOT_FILTER_THRESHOLD`] values: each written pattern for every
        // copy past the one the emitter counts, and each further copy
        // inside the aggregate for every copy.
        PipeStage::Pivot(p) => {
            let weight = score_agg(&p.aggregation, &Memo::new(), 0, &mut stats);
            weight
                .regex
                .times(PIVOT_FILTER_THRESHOLD - 1)
                .plus(weight.regex_copies.times(PIVOT_FILTER_THRESHOLD))
        }
        // No alias sequence, but a rendering may still repeat a pattern.
        PipeStage::Where(w) => score_expr(&w.condition, &Memo::new(), &mut stats).regex_copies,
        // The body binds once, as one struct-form `regexp_extract`
        // parameter, which the emitter writes and counts.
        PipeStage::Extract(_) => RegexLoad::NONE,
        PipeStage::Sort(_)
        | PipeStage::Limit(_)
        | PipeStage::Table(_)
        | PipeStage::Top(_)
        | PipeStage::Rare(_)
        | PipeStage::Drop(_)
        | PipeStage::Dedup(_)
        | PipeStage::Tail(_)
        | PipeStage::Rename(_)
        | PipeStage::Sample(_)
        | PipeStage::FromSaved(_) => RegexLoad::NONE,
    }
}

#[must_use]
pub fn pivot_aggregate_regex(pivot: &PivotStage) -> RegexLoad {
    let weight = score_agg(
        &pivot.aggregation,
        &Memo::new(),
        0,
        &mut CheckStats::default(),
    );
    weight.regex.plus(weight.regex_copies)
}

fn aggregation_regex_copies(aggregations: &[AggExpr], stats: &mut CheckStats) -> RegexLoad {
    let mut memo = Memo::new();
    let mut copies = RegexLoad::NONE;
    for agg in aggregations {
        let weight = score_agg(agg, &memo, 0, stats);
        copies = copies.plus(weight.regex_copies);
        memo.insert(catalog_key(&agg_output_name(agg)), weight);
    }
    copies
}

/// Add one output's delta to the running total, refusing at the FIRST
/// output that crosses the budget — so the message names the output an
/// operator has to change.
fn charge(
    index: usize,
    keyword: &'static str,
    target: Option<&str>,
    delta: u64,
    total: &mut u64,
) -> Result<(), ComplexityRefusal> {
    *total = total.saturating_add(delta);
    if *total > MAX_LATERAL_EXPANSION {
        return Err(ComplexityRefusal {
            stage_index: index,
            stage: keyword,
            target: target.and_then(refusal_name),
            limit: Limit::Lateral,
        });
    }
    Ok(())
}

fn stage_keyword(stage: &PipeStage) -> &'static str {
    match stage {
        PipeStage::Let(l) => l.keyword,
        PipeStage::Table(t) => t.keyword,
        PipeStage::Limit(l) => l.keyword,
        PipeStage::Extract(e) => e.keyword,
        PipeStage::Stats(_) => "stats",
        PipeStage::Where(_) => "where",
        PipeStage::Sort(_) => "sort",
        PipeStage::Top(_) => "top",
        PipeStage::Rare(_) => "rare",
        PipeStage::Drop(_) => "drop",
        PipeStage::Dedup(_) => "dedup",
        PipeStage::Timechart(_) => "timechart",
        PipeStage::Pivot(_) => "pivot",
        PipeStage::Tail(_) => "tail",
        PipeStage::Rename(_) => "rename",
        PipeStage::Sample(_) => "sample",
        PipeStage::EventStats(_) => "eventstats",
        PipeStage::FromSaved(_) => "from saved",
    }
}

fn score_agg(agg: &AggExpr, memo: &Memo, wrapper: u64, stats: &mut CheckStats) -> Weight {
    let children: Vec<Weight> = agg
        .args
        .iter()
        .map(|a| score_expr(a, memo, stats))
        .collect();
    let mut weight = profile(&call_rendering(&agg.function, &agg.args)).apply(&children);
    weight.w = weight.w.saturating_add(wrapper);
    weight
}

fn score_expr(expr: &Spanned<Expr>, memo: &Memo, stats: &mut CheckStats) -> Weight {
    stats.visited_nodes = stats.visited_nodes.saturating_add(1);
    match &expr.node {
        Expr::Literal(_) => Weight::LEAF,
        Expr::FieldRef(name) => memo
            .get(&catalog_key(name))
            .map_or(Weight::LEAF, |&target| Weight::substituted(target)),
        Expr::Unary { operand, .. } => {
            let child = score_expr(operand, memo, stats);
            profile(&Rendering::Unary).apply(&[child])
        }
        Expr::Binary { lhs, op, rhs } => score_binary(lhs, *op, rhs, memo, stats),
        Expr::FunctionCall { name, args } => {
            let children: Vec<Weight> = args.iter().map(|a| score_expr(a, memo, stats)).collect();
            profile(&call_rendering(name, args)).apply(&children)
        }
        Expr::InList { expr: target, list } => score_in_list(target, list, memo, stats),
    }
}

fn call_rendering(name: &str, args: &[Spanned<Expr>]) -> Rendering {
    let arity = args.len();
    let Some(known) = crate::parser::suggest::KNOWN_FUNCTIONS
        .iter()
        .find(|f| **f == name)
    else {
        return Rendering::UnknownCall(arity);
    };
    Rendering::Function(match *known {
        "count" if arity == 0 => FunctionShape::CountStar,
        "isnull" => FunctionShape::IsNull,
        "isnotnull" => FunctionShape::IsNotNull,
        "round" if arity >= 2 => FunctionShape::RoundPrecision,
        "split" => FunctionShape::Split,
        "now" => FunctionShape::Now,
        "p50" | "p90" | "p95" | "p99" => FunctionShape::Percentile,
        "case" => FunctionShape::Case(arity),
        "sev" => FunctionShape::Sev {
            dialect: sev_dialect(args),
            argc: arity,
        },
        "count" | "avg" | "sum" | "min" | "max" | "dc" | "distinct_count" | "first" | "last"
        | "values" | "list" | "median" | "stddev" | "lower" | "upper" | "length" | "len"
        | "coalesce" | "if" | "replace" | "substr" | "trim" | "ltrim" | "rtrim" | "abs"
        | "ceil" | "ceiling" | "floor" | "round" | "typeof" | "tonumber" | "tostring"
        | "contains" | "startswith" | "endswith" | "concat" | "date_part" | "date_trunc"
        | "date_diff" | "strftime" | "strptime" | "json_valid" => FunctionShape::Plain(arity),
        "json" | "json_extract" | "json_extract_string" | "json_keys" | "json_array_length" => {
            FunctionShape::JsonGuarded(arity)
        }
        // `KNOWN_FUNCTIONS` grew a name this table does not price.
        // `every_known_function_has_a_profile` fails here, loudly, rather
        // than letting the new function score as something cheaper.
        _ => return Rendering::UnknownCall(arity),
    })
}

/// Absent argument means `OTel`. An argument the emitter would refuse
/// (non-literal, or a token outside the vocabulary) takes the dialect with
/// the LARGER rendering, so an unreadable call is never priced below the
/// one the emitter might accept.
fn sev_dialect(args: &[Spanned<Expr>]) -> Dialect {
    match args.get(1) {
        None => Dialect::Otel,
        Some(arg) => match &arg.node {
            Expr::Literal(LiteralValue::String(token)) => {
                Dialect::from_token(token).unwrap_or(Dialect::Syslog)
            }
            _ => Dialect::Syslog,
        },
    }
}

/// The classifier mirrors [`crate::pin_scope::PinScope::subject_pin`]'s
/// two admitted SHAPES without its catalog: a bare field reference is a
/// subject under EVERY pin (the check does not know which one applies, so
/// it prices them all), and a pin-declaring call carries its own answer.
#[derive(Debug, Clone, Copy)]
enum SubjectKind {
    Bare,
    Declared(CanonicalType),
}

fn subject_kind(expr: &Spanned<Expr>) -> Option<SubjectKind> {
    match &expr.node {
        Expr::FieldRef(_) => Some(SubjectKind::Bare),
        Expr::FunctionCall { name, args } => {
            let pin = crate::emitter::function_result_pin(name)?;
            let (first, rest) = args.split_first()?;
            if !matches!(first.node, Expr::FieldRef(_)) {
                return None;
            }
            if !rest
                .iter()
                .all(|a| matches!(a.node, Expr::Literal(LiteralValue::String(_))))
            {
                return None;
            }
            Some(SubjectKind::Declared(pin))
        }
        _ => None,
    }
}

fn candidate_pins(kind: SubjectKind) -> Vec<Option<CanonicalType>> {
    match kind {
        SubjectKind::Declared(pin) => vec![Some(pin)],
        SubjectKind::Bare => std::iter::once(None)
            .chain(CanonicalType::ALL.into_iter().map(Some))
            .collect(),
    }
}

/// Mirrors the emitter's own fold of a negative numeric literal
/// (`crate::emitter::expr`'s `bare_literal`), which the parser leaves as
/// `Unary{Neg, Literal}`: the sign belongs to the literal the rule table
/// reads, and a shape this declines simply keeps the generic profile.
fn bare_literal(expr: &Expr) -> Option<Cow<'_, LiteralValue>> {
    match expr {
        Expr::Literal(lit) => Some(Cow::Borrowed(lit)),
        Expr::Unary {
            op: crate::ast::UnaryOp::Neg,
            operand,
        } => match &operand.node {
            Expr::Literal(LiteralValue::Int(n)) => {
                Some(Cow::Owned(LiteralValue::Int(n.checked_neg()?)))
            }
            Expr::Literal(LiteralValue::Float(f)) => Some(Cow::Owned(LiteralValue::Float(
                FloatLiteral::new(-f.value(), format!("-{}", f.text())),
            ))),
            _ => None,
        },
        _ => None,
    }
}

fn comparison_filter_op(op: BinaryOp) -> Option<FilterOp> {
    match op {
        BinaryOp::Eq => Some(FilterOp::Eq),
        BinaryOp::Ne => Some(FilterOp::Ne),
        BinaryOp::Gt => Some(FilterOp::Gt),
        BinaryOp::Gte => Some(FilterOp::Gte),
        BinaryOp::Lt => Some(FilterOp::Lt),
        BinaryOp::Lte => Some(FilterOp::Lte),
        _ => None,
    }
}

fn flip_filter_op(op: FilterOp) -> FilterOp {
    match op {
        FilterOp::Gt => FilterOp::Lt,
        FilterOp::Gte => FilterOp::Lte,
        FilterOp::Lt => FilterOp::Gt,
        FilterOp::Lte => FilterOp::Gte,
        other => other,
    }
}

fn score_binary(
    lhs: &Spanned<Expr>,
    op: BinaryOp,
    rhs: &Spanned<Expr>,
    memo: &Memo,
    stats: &mut CheckStats,
) -> Weight {
    // Children first, and once: the pin interpretations below combine
    // these weights instead of walking the operands again, which is what
    // keeps the walk linear in source size.
    let left = score_expr(lhs, memo, stats);
    let right = score_expr(rhs, memo, stats);
    let generic = profile(&Rendering::Binary).apply(&[left, right]);

    if let Some(pattern_kind) = PatternKind::of_operator(op) {
        // A literal regex binds as a parameter of its own. A `like` or
        // `ilike` pattern never reaches RE2, and a computed one is refused
        // before it is emitted.
        let pattern = match (pattern_kind, &rhs.node) {
            (PatternKind::Regex, Expr::Literal(LiteralValue::String(text))) => RegexLoad::of(text),
            _ => RegexLoad::NONE,
        };
        let Some(kind) = subject_kind(lhs) else {
            return generic.writing(pattern);
        };
        if !matches!(rhs.node, Expr::Literal(LiteralValue::String(_))) {
            return generic.writing(pattern);
        }
        let mut best = generic;
        for pin in candidate_pins(kind) {
            stats.visited_nodes = stats.visited_nodes.saturating_add(1);
            let rendering = Rendering::Pattern(compare::pattern_form(pin));
            best = best.max_with(profile(&rendering).apply(&[left]));
        }
        return best.writing(pattern);
    }

    let Some(filter_op) = comparison_filter_op(op) else {
        return generic;
    };

    // Both operand orders bind: `400 < status` IS `status > 400`.
    let resolved = match (subject_kind(lhs), subject_kind(rhs)) {
        (Some(kind), _) => bare_literal(&rhs.node).map(|lit| (kind, left, filter_op, lit)),
        (None, Some(kind)) => {
            bare_literal(&lhs.node).map(|lit| (kind, right, flip_filter_op(filter_op), lit))
        }
        (None, None) => None,
    };
    let Some((kind, subject, filter_op, literal)) = resolved else {
        return generic;
    };

    let mut best = generic;
    for pin in candidate_pins(kind) {
        stats.visited_nodes = stats.visited_nodes.saturating_add(1);
        // An interpretation the rule table refuses (an unknown severity
        // token) is skipped, never a refusal here: the emitter and the
        // stream compiler own that sentence, and this check must not
        // pre-empt it.
        let Ok(form) = compare::compare_form_bound(pin, filter_op, &literal) else {
            continue;
        };
        best = best.max_with(compare_weight(form.as_ref(), filter_op, subject));
    }
    best
}

fn compare_weight(form: Option<&CompareForm>, op: FilterOp, subject: Weight) -> Weight {
    match form {
        // `== null`, and the forms the rule table leaves literal-driven:
        // generic emission, subject and literal once each.
        None | Some(CompareForm::Native(_)) => {
            profile(&Rendering::Binary).apply(&[subject, Weight::LEAF])
        }
        Some(
            CompareForm::Conformed { .. } | CompareForm::Text(_) | CompareForm::SeverityExact(_),
        ) => profile(&Rendering::Compare(CompareKind::Bound)).apply(&[subject]),
        Some(CompareForm::NumericOnText(_)) => {
            profile(&Rendering::Compare(CompareKind::NumericOnText)).apply(&[subject])
        }
        Some(CompareForm::TextOrNumeric(_)) => {
            profile(&Rendering::Compare(CompareKind::TextOrNumeric)).apply(&[subject])
        }
        Some(band @ CompareForm::SeverityBand { .. }) => {
            let runs = severity_run_count(std::slice::from_ref(band));
            profile(&Rendering::SeverityRanges {
                runs,
                negated: op == FilterOp::Ne,
            })
            .apply(&[subject])
        }
    }
}

/// How many contiguous ranges a set of severity forms renders as — read
/// off [`compare::severity_ranges`], the one expansion the emitter uses,
/// rather than restated here.
fn severity_run_count(forms: &[CompareForm]) -> usize {
    compare::severity_points(forms).map_or(0, |points| compare::severity_ranges(&points).len())
}

fn score_in_list(
    target: &Spanned<Expr>,
    list: &[Spanned<Expr>],
    memo: &Memo,
    stats: &mut CheckStats,
) -> Weight {
    let subject = score_expr(target, memo, stats);
    let mut children = Vec::with_capacity(list.len() + 1);
    children.push(subject);
    children.extend(list.iter().map(|item| score_expr(item, memo, stats)));
    let mut best = profile(&Rendering::InListPlain(list.len())).apply(&children);

    let Some(kind) = subject_kind(target) else {
        return best;
    };
    // The pinned arm needs every element to be a bare literal; anything
    // else keeps the plain shape in the emitter too.
    let literals: Option<Vec<Cow<'_, LiteralValue>>> =
        list.iter().map(|item| bare_literal(&item.node)).collect();
    let Some(literals) = literals else {
        return best;
    };
    if literals.is_empty() {
        return best;
    }

    for pin in candidate_pins(kind) {
        stats.visited_nodes = stats.visited_nodes.saturating_add(1);
        // A null element, or a literal the pin refuses: the emitter falls
        // back to the plain shape, which is already priced.
        let forms: Option<Vec<CompareForm>> = literals
            .iter()
            .map(|literal| {
                compare::compare_form_bound(pin, FilterOp::Eq, literal)
                    .ok()
                    .flatten()
            })
            .collect();
        let Some(forms) = forms else { continue };
        best = best.max_with(in_list_weight(&forms, subject));
    }
    best
}

/// The weight a pinned `IN` list renders to over its subject, on the same
/// three-way split `crate::emitter::compare::in_list_sql` makes.
fn in_list_weight(forms: &[CompareForm], subject: Weight) -> Weight {
    let runs = severity_run_count(forms);
    if runs > 0 {
        return profile(&Rendering::SeverityRanges {
            runs,
            negated: false,
        })
        .apply(&[subject]);
    }
    let kinds: Vec<CompareKind> = forms.iter().map(element_kind).collect();
    if kinds
        .iter()
        .any(|k| matches!(k, CompareKind::TextOrNumeric | CompareKind::SeverityBand))
    {
        return profile(&Rendering::InListExpanded(kinds)).apply(&[subject]);
    }
    profile(&Rendering::InListBound(forms.len())).apply(&[subject])
}

fn element_kind(form: &CompareForm) -> CompareKind {
    match form {
        CompareForm::TextOrNumeric(_) => CompareKind::TextOrNumeric,
        CompareForm::SeverityBand { .. } => CompareKind::SeverityBand,
        // An ordered-only form cannot reach an IN element; it takes the
        // bound shape here, which is what the emitter writes for one.
        CompareForm::NumericOnText(_) => CompareKind::NumericOnText,
        CompareForm::Native(_)
        | CompareForm::Conformed { .. }
        | CompareForm::Text(_)
        | CompareForm::SeverityExact(_) => CompareKind::Bound,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::parser;

    fn pipeline(dsl: &str) -> Vec<Spanned<PipeStage>> {
        parser::parse(dsl).expect("dsl parses").pipeline
    }

    fn check(dsl: &str) -> Result<(), ComplexityRefusal> {
        check_pipeline_complexity(&pipeline(dsl))
    }

    fn refusal(dsl: &str) -> ComplexityRefusal {
        check(dsl).expect_err("this query must be refused")
    }

    #[test]
    fn the_boundary_is_the_delta_not_the_weight() {
        let coalesce = |n: usize| {
            let args = vec!["x"; n].join(", ");
            format!("* | let x = abs(y), z = coalesce({args})")
        };

        let admitted = pipeline(&coalesce(512));
        check_pipeline_complexity(&admitted).expect("a delta of exactly 512 is admitted");

        // `x` is `ABS(y)`: one call node over one leaf.
        let mut stats = CheckStats::default();
        let x = Weight {
            w: 2,
            ..Weight::LEAF
        };
        let memo: Memo = [("x".to_string(), x)].into_iter().collect();
        let PipeStage::Let(l) = &admitted[0].node else {
            panic!("first stage is a let")
        };
        let weight = score_expr(&l.assignments[1].1, &memo, &mut stats);
        assert_eq!(weight.d, 512, "512 references substitute 512 extra nodes");
        assert_eq!(weight.w, 1 + 512 * 2, "the expanded weight is far over 512");
        assert!(weight.w > MAX_LATERAL_EXPANSION);

        let refused = refusal(&coalesce(513));
        assert_eq!(refused.limit, Limit::Lateral);
        assert_eq!(refused.stage, "let");
        assert_eq!(refused.target.as_deref(), Some("`z`"));
    }

    #[test]
    fn an_else_less_case_is_priced_for_the_else_the_parser_adds() {
        assert_eq!(
            profile(&Rendering::Function(FunctionShape::Case(2))),
            RenderProfile::new(2, vec![1, 1])
        );
        assert_eq!(
            profile(&Rendering::Function(FunctionShape::Case(3))),
            RenderProfile::new(1, vec![1, 1, 1])
        );

        // `x` is four nodes, so each reference substitutes three.
        let reads = |n: usize| {
            let args = vec!["x"; n].join(", ");
            format!("* | let x = case(flag, status), z = coalesce({args})")
        };
        check(&reads(170)).expect("170 references substitute 510 nodes");
        let refused = refusal(&reads(256));
        assert_eq!(refused.limit, Limit::Lateral);
        assert_eq!(refused.target.as_deref(), Some("`z`"));
        assert_eq!(
            profile(&Rendering::Function(FunctionShape::Plain(2))),
            RenderProfile::new(1, vec![1, 1])
        );
    }

    #[test]
    fn a_constructible_boundary_case_refuses_one_reference_late() {
        let chain = |n: usize| {
            let args = vec!["A"; n].join(", ");
            format!("service=nginx | eval A = abs(y), z = coalesce({args})")
        };
        check(&chain(512)).expect("512 admitted");
        let refused = refusal(&chain(513));
        assert_eq!(refused.stage, "eval");
        assert_eq!(refused.target.as_deref(), Some("`z`"));
    }

    fn severity_chain(links: usize) -> String {
        use std::fmt::Write as _;
        let points = (0..12).map(|i| (i * 2 + 1).to_string()).collect::<Vec<_>>();
        let mut dsl = String::from("* | let a0 = _severity + _severity");
        for i in 1..=links {
            let _ = write!(dsl, ", a{i} = sev(a{}) in ({})", i - 1, points.join(","));
        }
        dsl
    }

    /// The ORIGINAL substituted-DSL-node measure, kept as a frozen
    /// negative oracle: it scores the eight-link chain at 408, under the
    /// budget, because it counts DSL nodes and misses the twelve subject
    /// copies the severity set renderer writes.
    fn original_score(stages: &[Spanned<PipeStage>]) -> u64 {
        fn node(expr: &Expr, memo: &HashMap<String, u64>) -> Weight {
            match expr {
                Expr::Literal(_) => Weight::LEAF,
                Expr::FieldRef(name) => memo.get(&catalog_key(name)).map_or(Weight::LEAF, |&t| {
                    Weight::substituted(Weight {
                        w: t,
                        ..Weight::LEAF
                    })
                }),
                Expr::Unary { operand, .. } => {
                    let c = node(&operand.node, memo);
                    Weight {
                        w: 1 + c.w,
                        d: c.d,
                        ..Weight::LEAF
                    }
                }
                Expr::Binary { lhs, rhs, .. } => {
                    let (l, r) = (node(&lhs.node, memo), node(&rhs.node, memo));
                    Weight {
                        w: 1 + l.w + r.w,
                        d: l.d + r.d,
                        ..Weight::LEAF
                    }
                }
                Expr::FunctionCall { args, .. } => {
                    let mut out = Weight::LEAF;
                    for a in args {
                        let c = node(&a.node, memo);
                        out.w += c.w;
                        out.d += c.d;
                    }
                    out
                }
                Expr::InList { expr, list } => {
                    let mut out = node(&expr.node, memo);
                    out.w += 1;
                    for item in list {
                        let c = node(&item.node, memo);
                        out.w += c.w;
                        out.d += c.d;
                    }
                    out
                }
            }
        }

        let mut total = 0;
        for stage in stages {
            let mut memo: HashMap<String, u64> = HashMap::new();
            if let PipeStage::Let(l) = &stage.node {
                for (target, expr) in &l.assignments {
                    let w = node(&expr.node, &memo);
                    total += w.d;
                    memo.insert(catalog_key(target), w.w);
                }
            }
        }
        total
    }

    #[test]
    fn the_original_measure_scores_the_chain_at_408_and_would_admit_it() {
        assert_eq!(original_score(&pipeline(&severity_chain(8))), 408);
        const { assert!(408 < MAX_LATERAL_EXPANSION) }
    }

    #[test]
    fn severity_chain_is_refused() {
        let refused = refusal(&severity_chain(8));
        assert_eq!(refused.limit, Limit::Lateral);
        assert_eq!(refused.stage, "let");
        assert_eq!(refused.target.as_deref(), Some("`a2`"));
        let text = refused.to_string();
        assert!(text.contains("512"), "{text}");
        assert!(text.contains("`let`"), "{text}");
    }

    #[test]
    fn a_disjoint_severity_set_renders_twelve_runs() {
        let forms: Vec<CompareForm> = (0..12)
            .map(|i| CompareForm::SeverityExact(i * 2 + 1))
            .collect();
        assert_eq!(severity_run_count(&forms), 12);
        let p = profile(&Rendering::SeverityRanges {
            runs: 12,
            negated: false,
        });
        assert_eq!(p.copies, vec![12]);
    }

    #[test]
    fn a_deep_chain_is_refused_without_expanding() {
        use std::fmt::Write as _;
        let mut dsl = String::from("* | let a0 = 1");
        for i in 1..90 {
            let _ = write!(dsl, ", a{i} = a{} + a{}", i - 1, i - 1);
        }
        let stages = pipeline(&dsl);
        let (result, stats) = check_pipeline_complexity_with_stats(&stages);
        let refused = result.expect_err("a 90-link doubling chain is refused");
        assert_eq!(refused.limit, Limit::Lateral);

        let source_nodes = count_ast_nodes(&stages);
        let cases = 1 + CanonicalType::ALL.len();
        assert!(
            stats.visited_nodes <= source_nodes * cases as u64,
            "{} visits over {source_nodes} source nodes",
            stats.visited_nodes
        );
        assert!(stats.visited_nodes < 10_000, "{}", stats.visited_nodes);
    }

    fn count_ast_nodes(stages: &[Spanned<PipeStage>]) -> u64 {
        fn expr_nodes(expr: &Expr) -> u64 {
            1 + match expr {
                Expr::Literal(_) | Expr::FieldRef(_) => 0,
                Expr::Unary { operand, .. } => expr_nodes(&operand.node),
                Expr::Binary { lhs, rhs, .. } => expr_nodes(&lhs.node) + expr_nodes(&rhs.node),
                Expr::FunctionCall { args, .. } => args.iter().map(|a| expr_nodes(&a.node)).sum(),
                Expr::InList { expr, list } => {
                    expr_nodes(&expr.node) + list.iter().map(|i| expr_nodes(&i.node)).sum::<u64>()
                }
            }
        }
        let mut total = 0;
        for stage in stages {
            match &stage.node {
                PipeStage::Let(l) => {
                    for (_, e) in &l.assignments {
                        total += expr_nodes(&e.node);
                    }
                }
                PipeStage::Stats(s) => {
                    for a in &s.aggregations {
                        total += 1 + a.args.iter().map(|x| expr_nodes(&x.node)).sum::<u64>();
                    }
                }
                _ => {}
            }
        }
        total
    }

    #[test]
    fn saturated_weights_are_over_limit() {
        let full = RegexLoad {
            copies: u64::MAX,
            bytes: u64::MAX,
        };
        let saturated = Weight {
            w: u64::MAX,
            d: u64::MAX,
            regex: full,
            regex_copies: full,
        };
        let combined = profile(&Rendering::Binary).apply(&[saturated, saturated]);
        assert_eq!(combined.w, u64::MAX);
        assert_eq!(combined.d, u64::MAX);
        assert!(combined.d > MAX_LATERAL_EXPANSION);
        assert_eq!(combined.regex, full);
        assert_eq!(combined.regex_copies, full);
    }

    #[test]
    fn stage_cap() {
        let stages = |n: usize| format!("*{}", " | head 1".repeat(n));
        check(&stages(MAX_PIPELINE_STAGES)).expect("128 stages are admitted");
        let refused = refusal(&stages(MAX_PIPELINE_STAGES + 1));
        assert_eq!(refused.limit, Limit::Stages(MAX_PIPELINE_STAGES + 1));
        let text = refused.to_string();
        assert!(text.contains("129 pipeline stages"), "{text}");
        assert!(text.contains("128"), "{text}");
    }

    #[test]
    fn the_stage_cap_wins_over_the_expansion_budget() {
        let mut dsl = severity_chain(8);
        dsl.push_str(&" | head 1".repeat(MAX_PIPELINE_STAGES));
        assert!(matches!(refusal(&dsl).limit, Limit::Stages(_)));
    }

    #[test]
    fn from_saved_counts_toward_the_stage_cap() {
        let dsl = format!("| from saved daily{}", " | head 1".repeat(128));
        assert!(matches!(refusal(&dsl).limit, Limit::Stages(129)));
    }

    fn field_list(count: usize, distinct: bool) -> String {
        (0..count)
            .map(|i| {
                if distinct {
                    format!("f{i}")
                } else {
                    "f".to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn sql_door(dsl: &str) -> Result<(), String> {
        crate::emitter::validate_pipeline(&pipeline(dsl)).map_err(|e| {
            e.to_string()
                .strip_prefix("unsupported operation: ")
                .unwrap_or_else(|| panic!("{dsl}: the emitter's prefix is missing"))
                .to_string()
        })
    }

    fn stream_door(dsl: &str) -> Result<(), crate::stream::StreamPlanError> {
        crate::stream::compile_stream_plan(
            &pipeline(dsl),
            &crate::pin_scope::PinScope::unpinned(),
            None,
        )
        .map(|_| ())
    }

    #[test]
    fn field_list_width_is_shared_admission() {
        use crate::stream::StreamPlanError;

        for distinct in [true, false] {
            for prefix in ["*", "* | extract kv"] {
                let n = if prefix == "*" { 1 } else { 2 };
                for stage in ["sort", "dedup"] {
                    let at = format!("{prefix} | {stage} {}", field_list(64, distinct));
                    check(&at).unwrap_or_else(|r| panic!("{at}: {r}"));
                    sql_door(&at).unwrap_or_else(|r| panic!("{at}: {r}"));
                    match stream_door(&at) {
                        Ok(()) => assert_eq!(stage, "dedup", "{at}: sort streamed"),
                        Err(StreamPlanError::UnsupportedStage { stage: s, .. }) => {
                            assert_eq!(s, "sort", "{at}");
                        }
                        Err(other) => panic!("{at}: {other}"),
                    }

                    let over = format!("{prefix} | {stage} {}", field_list(65, distinct));
                    let refused = refusal(&over);
                    assert_eq!(refused.limit, Limit::Fields(65), "{over}");
                    assert_eq!(refused.stage, stage, "{over}");
                    assert_eq!(refused.target, None, "{over}");
                    let sentence = refused.to_string();
                    assert_eq!(
                        sentence,
                        format!(
                            "pipeline stage {n} (`{stage}`) names 65 fields, over the limit \
                             of 64 for one `sort` or `dedup` stage"
                        )
                    );
                    assert_eq!(sql_door(&over), Err(sentence.clone()), "{over}");
                    match stream_door(&over) {
                        Err(StreamPlanError::TooComplex(text)) => {
                            assert_eq!(text, sentence, "{over}");
                        }
                        other => panic!("{over}: {other:?}"),
                    }
                }
            }
        }

        check("* | dedup").expect("bare dedup is admitted");
        sql_door("* | dedup").expect("bare dedup is admitted by the SQL door");
        stream_door("* | dedup").expect("bare dedup streams");

        let wide = format!("* | dedup {}", field_list(65, true));
        let long = format!("{wide}{}", " | head 1".repeat(MAX_PIPELINE_STAGES));
        assert!(matches!(refusal(&long).limit, Limit::Stages(129)));
        let lateral = format!(
            "* | let x = abs(y), z = coalesce({})",
            vec!["x"; 513].join(", ")
        );
        let lateral_first = format!("{lateral}{}", wide.trim_start_matches('*'));
        assert_eq!(refusal(&lateral_first).limit, Limit::Lateral);
        let wide_first = format!("{wide}{}", lateral.trim_start_matches('*'));
        assert_eq!(refusal(&wide_first).limit, Limit::Fields(65));
    }

    #[test]
    fn the_alias_table_resets_between_stages() {
        let args = vec!["y"; 300].join(", ");
        let same_stage = format!("* | let x = coalesce({args}), z = x + x");
        assert!(matches!(refusal(&same_stage).limit, Limit::Lateral));

        let split = format!("* | let x = coalesce({args}) | let z = x + x");
        check(&split).expect("a split pipeline reads a finished column");
    }

    #[test]
    fn alias_names_fold_ascii_case() {
        let args = vec!["a"; 513].join(", ");
        let refused = refusal(&format!("* | let A = abs(y), z = coalesce({args})"));
        assert_eq!(refused.target.as_deref(), Some("`z`"));

        let args = vec!["A"; 513].join(", ");
        let refused = refusal(&format!("* | let a = abs(y), z = coalesce({args})"));
        assert_eq!(refused.target.as_deref(), Some("`z`"));
    }

    #[test]
    fn forward_and_self_references_score_as_leaves() {
        check("* | let a = b + b, b = 1").expect("a forward reference costs nothing here");
        check("* | let a = a + a").expect("a self reference costs nothing here");
    }

    #[test]
    fn independent_expressions_and_flat_lists_have_no_delta() {
        let list = (0..2000)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        check(&format!("* | let a = status in ({list})")).expect("a flat list is free");

        let assignments = (0..500)
            .map(|i| format!("a{i} = abs(y)"))
            .collect::<Vec<_>>()
            .join(", ");
        check(&format!("* | let {assignments}")).expect("independent assignments are free");

        let aggs = (0..300)
            .map(|i| format!("avg(dur) as a{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        check(&format!("* | stats {aggs}")).expect("independent aggregates are free");
    }

    #[test]
    fn stages_without_an_output_sequence_charge_nothing() {
        for dsl in [
            "* | where status > 400 and dur < 10",
            "* | rename status as st, dur as d",
            r#"* | extract "(?P<ip>\d+)" from message"#,
            "* | extract kv",
            "* | sort -dur | table status | dedup host | tail 5",
            "* | top 5 status by host",
            "* | pivot count() on status by host",
        ] {
            check(dsl).unwrap_or_else(|e| panic!("{dsl}: {e}"));
        }
    }

    #[test]
    fn aggregating_stages_carry_their_own_alias_sequence() {
        let args = vec!["abs_y"; 513].join(", ");
        let refused = refusal(&format!("* | stats abs(y) as abs_y, coalesce({args}) as z"));
        assert_eq!(refused.stage, "stats");
        assert_eq!(refused.target.as_deref(), Some("`z`"));

        let args = vec!["abs_y"; 513].join(", ");
        let refused = refusal(&format!(
            "* | timechart span=1h abs(y), coalesce({args}) as z"
        ));
        assert_eq!(refused.stage, "timechart");

        let args = vec!["abs_y"; 513].join(", ");
        let refused = refusal(&format!(
            "* | eventstats abs(y) as abs_y, coalesce({args}) as z"
        ));
        assert_eq!(refused.stage, "eventstats");
    }

    #[test]
    fn eventstats_outputs_carry_their_window_wrapper() {
        let mut stats = CheckStats::default();
        let plain = pipeline("* | stats abs(y) as a");
        let windowed = pipeline("* | eventstats abs(y) as a by host, service");
        let (PipeStage::Stats(s), PipeStage::EventStats(es)) = (&plain[0].node, &windowed[0].node)
        else {
            panic!("stage kinds")
        };
        let bare = score_agg(&s.aggregations[0], &Memo::new(), 0, &mut stats);
        let over = score_agg(
            &es.aggregations[0],
            &Memo::new(),
            EVENTSTATS_OVER_FIXED + 2,
            &mut stats,
        );
        assert!(over.w > bare.w, "{over:?} vs {bare:?}");
    }

    #[test]
    fn hand_calculated_profiles() {
        assert_eq!(profile(&Rendering::Leaf), RenderProfile::new(1, vec![]));
        assert_eq!(
            profile(&Rendering::Binary),
            RenderProfile::new(1, vec![1, 1])
        );
        assert_eq!(profile(&Rendering::Unary), RenderProfile::new(1, vec![1]));
        assert_eq!(
            profile(&Rendering::InListPlain(3)),
            RenderProfile::new(1, vec![1, 1, 1, 1])
        );
        assert_eq!(
            profile(&Rendering::InListBound(3)),
            RenderProfile::new(4, vec![1])
        );
        assert_eq!(
            profile(&Rendering::Compare(CompareKind::TextOrNumeric)),
            RenderProfile::new(9, vec![2])
        );
        // Two `TextOrNumeric` elements and one `Text` element: 9 + 9 + 2
        // nodes plus two `OR`s, over five writings of the subject.
        assert_eq!(
            profile(&Rendering::InListExpanded(vec![
                CompareKind::TextOrNumeric,
                CompareKind::TextOrNumeric,
                CompareKind::Bound,
            ])),
            RenderProfile::new(22, vec![5])
        );
        // Three ranges: four nodes each, two `OR`s, one `NOT`.
        assert_eq!(
            profile(&Rendering::SeverityRanges {
                runs: 3,
                negated: true
            }),
            RenderProfile::new(15, vec![3])
        );
        assert_eq!(
            profile(&Rendering::Function(FunctionShape::CountStar)),
            RenderProfile::new(2, vec![])
        );
        assert_eq!(
            profile(&Rendering::Function(FunctionShape::IsNull)),
            RenderProfile::new(2, vec![1])
        );
        assert_eq!(
            profile(&Rendering::Function(FunctionShape::IsNotNull)),
            RenderProfile::new(3, vec![1])
        );
        assert_eq!(
            profile(&Rendering::Function(FunctionShape::Plain(1))),
            RenderProfile::new(1, vec![1])
        );
        assert_eq!(
            profile(&Rendering::Function(FunctionShape::RoundPrecision)),
            RenderProfile::new(2, vec![1, 0])
        );
        assert_eq!(
            profile(&Rendering::Function(FunctionShape::Split)),
            RenderProfile::new(3, vec![1, 1, 0])
        );
        assert_eq!(
            profile(&Rendering::Function(FunctionShape::Now)),
            RenderProfile::new(2, vec![])
        );
        assert_eq!(
            profile(&Rendering::Conform(ConformShape::UntypedText)),
            RenderProfile::new(3, vec![1])
        );
        assert_eq!(
            profile(&Rendering::Conform(ConformShape::GuardedCast(
                CanonicalType::BigInt,
                Dialect::Otel
            ))),
            RenderProfile::new(7, vec![3])
        );
        assert_eq!(
            profile(&Rendering::Conform(ConformShape::GuardedCast(
                CanonicalType::Varchar,
                Dialect::Otel
            ))),
            RenderProfile::new(0, vec![1])
        );
        // The ladder's token text, priced as the parser leaves it: the
        // `CASE`, its NULL fallthrough, and per arm one equality, the
        // ladder number and the token string — over twenty-four whole
        // copies of the subject.
        assert_eq!(
            profile(&Rendering::Conform(ConformShape::SeverityTokenText)),
            RenderProfile::new(2 + 24 * 3, vec![24])
        );
    }

    /// The nodes: one `CASE`, one fallthrough result, and three per arm
    /// (the equality, the ladder number, the token string) — 74, plus the
    /// 24 subject copies, which is exactly the 98-node tree
    /// `json_serialize_sql` reports in
    /// `trawl-engine/tests/duckdb_probe.rs`.
    #[test]
    fn the_token_text_kernel_is_priced_for_the_parser_rewrite() {
        let kernel = profile(&Rendering::Conform(ConformShape::SeverityTokenText));
        assert_eq!(kernel.fixed, 74);
        assert_eq!(kernel.copies, vec![24]);
        assert_eq!(kernel.fixed + kernel.copies[0], 98);

        let pattern = profile(&Rendering::Pattern(compare::PatternForm::SeverityText));
        assert_eq!(pattern, RenderProfile::new(76, vec![24]));

        for form in [
            compare::PatternForm::Native,
            compare::PatternForm::BigIntText,
            compare::PatternForm::BooleanText,
            compare::PatternForm::DoubleText,
            compare::PatternForm::Rfc3339Text,
        ] {
            assert_eq!(
                profile(&Rendering::Pattern(form)).copies,
                vec![1],
                "{form:?}"
            );
        }
    }

    #[test]
    fn severity_pattern_chain_is_refused() {
        let dsl = r#"* | let a = abs(_severity), b = sev(a) like "e%", c = sev(b) like "e%""#;
        let (verdict, stats) = check_pipeline_complexity_with_stats(&pipeline(dsl));
        let refused = verdict.expect_err("the chained severity patterns are refused");
        assert_eq!(refused.limit, Limit::Lateral);
        assert_eq!(refused.stage, "let");
        assert_eq!(refused.target.as_deref(), Some("`c`"));
        assert!(
            stats.lateral_delta > 100_000,
            "the chain costs {} substitutions",
            stats.lateral_delta
        );

        check(r#"* | let a = abs(_severity), b = sev(a) like "e%""#)
            .expect("one link is well inside the budget");
    }

    #[test]
    fn the_severity_reading_profiles_match_their_documented_shapes() {
        let repeated = profile(&Rendering::Conform(ConformShape::SeverityReading(
            Dialect::Otel,
        )));
        assert_eq!(repeated.copies, vec![79]);
        // Syslog costs MORE, not less: its numeric rung is a second simple
        // `CASE` with eight arms where `OTel`'s is a searched one.
        let syslog = profile(&Rendering::Conform(ConformShape::SeverityReading(
            Dialect::Syslog,
        )));
        assert_eq!(syslog.copies, vec![85]);
        let once = profile(&Rendering::Conform(ConformShape::SeverityReadingBindOnce(
            Dialect::Otel,
        )));
        assert_eq!(once.copies, vec![1]);
        assert!(once.fixed > repeated.copies[0], "{once:?}");
        // Twenty band tokens plus the eighteen exact names the table does
        // not already carry.
        assert_eq!(severity_case_arms(), 38);
    }

    #[test]
    fn the_severity_run_ceiling_is_thirteen() {
        let mut points: Vec<i64> = (1..=24).step_by(2).collect();
        points.push(-5);
        points.push(9_000);
        assert_eq!(compare::severity_ranges(&points).len(), MAX_SEVERITY_RUNS);
    }

    #[test]
    fn every_known_function_has_a_profile() {
        for &name in crate::parser::suggest::KNOWN_FUNCTIONS {
            let args: Vec<Spanned<Expr>> =
                vec![Spanned::new(Expr::FieldRef("x".to_string()), 0..0)];
            let rendering = call_rendering(name, &args);
            assert!(
                !matches!(rendering, Rendering::UnknownCall(_)),
                "{name} has no rendering profile"
            );
        }
    }

    #[test]
    fn an_unknown_call_scores_structurally() {
        let args = vec![Spanned::new(Expr::FieldRef("x".to_string()), 0..0)];
        assert_eq!(call_rendering("nosuchfn", &args), Rendering::UnknownCall(1));
        check("* | let a = nosuchfn(x)").expect("the emitter owns that refusal");
    }

    #[test]
    fn an_unknown_severity_token_is_not_a_complexity_refusal() {
        check(r#"* | where _severity == "wat""#).expect("the lanes own the vocabulary error");
    }

    #[test]
    fn the_refusal_sentence_names_the_stage_the_budget_and_a_remedy() {
        let args = vec!["x"; 513].join(", ");
        let text = refusal(&format!("* | let x = abs(y), z = coalesce({args})")).to_string();
        assert!(text.contains("pipeline stage 1 (`let`)"), "{text}");
        assert!(text.contains("512"), "{text}");
        assert!(text.contains("`z`"), "{text}");
        assert!(text.contains("separate `| let` stages"), "{text}");
        assert!(!text.contains("nests deeper"), "{text}");
        assert!(!text.contains("nodes"), "{text}");

        let text = refusal(&format!("* | stats abs(y) as x, coalesce({args}) as z")).to_string();
        assert!(text.contains("(`stats`)"), "{text}");
        assert!(text.contains("expression of its own"), "{text}");
    }

    #[test]
    fn an_unquotable_name_is_omitted_from_the_sentence() {
        assert_eq!(refusal_name("dur"), Some("`dur`".to_string()));
        assert_eq!(
            refusal_name("http-status"),
            Some("`http-status`".to_string())
        );
        assert_eq!(refusal_name("bad\u{1b}name"), None);

        let refusal = ComplexityRefusal {
            stage_index: 2,
            stage: "let",
            target: None,
            limit: Limit::Lateral,
        };
        let text = refusal.to_string();
        assert!(text.contains("pipeline stage 3 (`let`)"), "{text}");
        assert!(text.contains("512"), "{text}");
    }

    /// Every query trawl DOCUMENTS stays inside the budget (ADR-0024).
    const DOCS: &[&str] = &[
        "docs/src/content/docs/reference/dsl.md",
        "docs/src/content/docs/reference/cli.md",
    ];

    const NEGATIVE_MARKERS: &[&str] = &["error at the"];

    fn candidates(markdown: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut block: Vec<&str> = Vec::new();
        let mut lang = String::new();
        let mut inside = false;

        for line in markdown.lines() {
            if let Some(rest) = line.strip_prefix("```") {
                if inside {
                    out.extend(block_candidates(&lang, &block));
                    block.clear();
                    inside = false;
                } else {
                    lang = rest.trim().to_string();
                    inside = true;
                }
                continue;
            }
            if inside {
                block.push(line);
            }
        }
        out
    }

    fn block_candidates(lang: &str, block: &[&str]) -> Vec<String> {
        let lines: Vec<&str> = block
            .iter()
            .copied()
            .filter(|l| !l.trim().is_empty())
            .filter(|l| !NEGATIVE_MARKERS.iter().any(|m| l.contains(m)))
            .collect();
        if lines.is_empty() {
            return Vec::new();
        }

        if lang.eq_ignore_ascii_case("bash") {
            return lines.iter().filter_map(|l| quoted_argument(l)).collect();
        }
        if !lang.is_empty() {
            return Vec::new();
        }

        // An unlabelled block is either one multi-line query or a list of
        // one-line ones. Try the whole block first, so a wrapped pipeline is
        // measured as the pipeline it is.
        let joined = lines.join("\n");
        if parser::parse(&joined).is_ok() {
            return vec![joined];
        }
        lines.iter().map(|l| (*l).to_string()).collect()
    }

    fn quoted_argument(line: &str) -> Option<String> {
        if !line.contains("trawl query") && !line.contains("trawl validate") {
            return None;
        }
        let (_, rest) = line.split_once('"')?;
        let (arg, _) = rest.split_once('"')?;
        // The reference elides the query itself in some option examples.
        if arg.trim() == "..." {
            return None;
        }
        Some(arg.to_string())
    }

    #[test]
    fn docs_and_fixtures_stay_below_caps() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("the crate sits two levels under the repository root");

        let mut admitted = 0usize;
        let mut max_stages = 0usize;
        let mut max_delta = 0u64;
        let mut widest = String::new();

        for doc in DOCS {
            let path = root.join(doc);
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            for candidate in candidates(&text) {
                let Ok(query) = parser::parse(&candidate) else {
                    continue;
                };
                let (verdict, stats) = check_pipeline_complexity_with_stats(&query.pipeline);
                verdict.unwrap_or_else(|refusal| {
                    panic!(
                        "{doc} documents a query the budget refuses:\n  {candidate}\n  {refusal}"
                    )
                });
                admitted += 1;
                max_stages = max_stages.max(stats.stages);
                if stats.lateral_delta > max_delta {
                    max_delta = stats.lateral_delta;
                    widest = candidate.clone();
                }
            }
        }

        // The reference is not a corpus of one example; if the extraction ever
        // stops finding queries, this test would pass by looking at nothing.
        assert!(
            admitted >= 60,
            "only {admitted} documented queries were parsed — the extraction broke"
        );

        assert!(max_stages <= MAX_PIPELINE_STAGES / 4, "{max_stages} stages");
        assert!(
            max_delta <= MAX_LATERAL_EXPANSION / 4,
            "{max_delta} lateral delta, from: {widest}"
        );
    }

    #[test]
    fn a_quoted_target_renders_as_dsl_text() {
        let args = vec!["`a b`"; 513].join(", ");
        let refused = refusal(&format!("* | let `a b` = abs(y), `z z` = coalesce({args})"));
        assert_eq!(refused.target.as_deref(), Some("`z z`"));
        parser::parse(&format!("* | let {} = 1", refused.target.unwrap()))
            .expect("the name the message prints parses back");
    }

    fn regex_copies(dsl: &str) -> Vec<RegexLoad> {
        pipeline(dsl)
            .iter()
            .map(|stage| stage_regex_copies(&stage.node))
            .collect()
    }

    #[test]
    fn a_lateral_reference_copies_the_pattern_it_names() {
        assert_eq!(
            regex_copies(r#"* | let a = message matches "xy", b = a, c = b"#),
            vec![RegexLoad {
                copies: 2,
                bytes: 4
            }]
        );
        // a: written once. b: two copies of a. c: two copies of b's two.
        assert_eq!(
            regex_copies(r#"* | let a = message matches "x", b = a and a, c = b or b"#),
            vec![RegexLoad {
                copies: 6,
                bytes: 6
            }]
        );
        assert_eq!(
            regex_copies(r#"* | stats count(if(message matches "x", 1, null)) as n, max(n) as m"#),
            vec![RegexLoad {
                copies: 1,
                bytes: 1
            }]
        );
    }

    #[test]
    fn a_column_or_a_like_copies_no_regex() {
        assert_eq!(
            regex_copies(r#"* | let a = message matches "x" | let b = a"#),
            vec![RegexLoad::NONE, RegexLoad::NONE]
        );
        assert_eq!(
            regex_copies(r#"* | let a = message like "x%", b = a"#),
            vec![RegexLoad::NONE]
        );
        assert_eq!(
            regex_copies(r#"* | where message matches "x" | extract "(?P<a>x)(?P<b>y)""#),
            vec![RegexLoad::NONE, RegexLoad::NONE]
        );
    }

    #[test]
    fn a_rendering_that_repeats_its_subject_copies_the_pattern() {
        let points = (0..12).map(|i| (i * 2 + 1).to_string()).collect::<Vec<_>>();
        let runs = profile(&Rendering::SeverityRanges {
            runs: points.len(),
            negated: false,
        })
        .copies[0];
        assert_eq!(runs, 12, "a disjoint set writes its subject once per point");
        assert_eq!(
            regex_copies(&format!(
                r#"* | let a = "abc" matches "x", b = sev(a) in ({})"#,
                points.join(", ")
            )),
            vec![RegexLoad {
                copies: runs,
                bytes: runs
            }]
        );
    }

    #[test]
    fn a_pivot_aggregate_copies_its_patterns_per_filtered_value() {
        let worst = PIVOT_FILTER_THRESHOLD;
        assert_eq!(
            regex_copies(r#"* | pivot count(if(host matches "xy", 1, null)) on service by host"#),
            vec![RegexLoad {
                copies: worst - 1,
                bytes: 2 * (worst - 1)
            }]
        );
        assert_eq!(
            regex_copies(
                r#"* | pivot count(coalesce(if(host matches "a", 1, null), if(host matches "b", 1, null))) on service by host"#
            ),
            vec![RegexLoad {
                copies: 2 * (worst - 1),
                bytes: 2 * (worst - 1)
            }]
        );
        assert_eq!(
            regex_copies(r#"* | pivot count(if(host like "x%", 1, null)) on service by host"#),
            vec![RegexLoad::NONE]
        );
    }
}
