// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The bind-time expansion budget (ADR-0024).
//!
//! `DuckDB` binds a SELECT list left to right, and an output that names an
//! EARLIER output of the same list is bound by copying that earlier
//! expression in and binding it again. The DSL text grows by one name; the
//! bound tree doubles. Chained, that is exponential preparation work for a
//! query that fits on one line, and the permit stays held for all of it.
//!
//! This module is the admission check both validation doors run
//! ([`crate::emitter::validate_pipeline`] and
//! [`crate::stream::compile_stream_plan`]). It is pure and
//! corpus-independent: it reads the AST and a table of numeric RENDERING
//! PROFILES, and it never emits SQL, parses SQL, consults the catalog or
//! opens a database. Corpus-independence is the point — the same DSL text
//! has to be admitted or refused identically at every door, including the
//! one that validates a saved query nobody has run yet.
//!
//! # The measure
//!
//! Every expression carries two numbers ([`Weight`]):
//!
//! - `w`, the EXPANDED node count — what the bound tree costs once every
//!   same-stage alias inside it has been substituted;
//! - `d`, the ADDITIONAL substitution that expansion caused — the part
//!   that would not exist if the query named no earlier output.
//!
//! A field or a literal is `w = 1, d = 0`. A reference to an earlier
//! output `T` of the SAME stage is `w = W(T), d = W(T) - 1`: the whole
//! target arrives in place of the one name that was written. A rendering
//! with fixed node overhead `c` and per-child copy counts `mᵢ` gives
//! `w = c + Σ mᵢ·wᵢ` and `d = Σ mᵢ·dᵢ`.
//!
//! `d` is PROPAGATED, never derived by subtracting one maximized total
//! from another: where a construct's shape depends on the field's catalog
//! pin, the check takes the componentwise maximum of `(w, d)` over every
//! supported pin, and two maxima can come from two different renderings.
//!
//! The per-stage sum of assignment `d`s, accumulated across the pipeline,
//! is what [`MAX_LATERAL_EXPANSION`] bounds. Arithmetic saturates, and a
//! saturated total is over the limit by construction — nothing subtracts
//! saturated numbers.
//!
//! # What the profiles are for
//!
//! Counting DSL nodes alone undercounts, because SQL translation multiplies
//! operands the DSL wrote once. `sev(x) in (1,3,5,…,23)` renders its
//! subject once per disjoint ladder range — twelve copies for that list —
//! so a chain of such assignments compounds twelve-fold per link while the
//! DSL grows linearly. [`profile`] states, per rendering, how many copies
//! of each child the emitter writes; the drift tests in
//! `tests/complexity_emission_bounds.rs` hold those numbers against the
//! real emitter. A profile may overcount fixed overhead. It may never omit
//! a child copy.
//!
//! # What it does not bound
//!
//! Multiplication that no alias reference feeds — `x in (1,…,1000)` over an
//! ordinary column — has `d = 0` and is bounded by the existing query
//! length and expression depth limits instead. Corpus width, file
//! discovery and pivot cardinality are database planning costs and sit
//! outside this measure entirely.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;

use crate::ast::{
    AggExpr, BinaryOp, Expr, ExtractMode, FilterOp, FloatLiteral, LiteralValue, PipeStage, Spanned,
};
use crate::compare::{self, CompareForm, PatternForm};
use crate::parser::suggest::quote_dsl_field;
use crate::projection::agg_output_name;
use crate::schema::{CanonicalType, catalog_key};
use crate::severity::Dialect;

// ---------------------------------------------------------------------------
// The limits
// ---------------------------------------------------------------------------

/// The most additional substitution one query may describe (ADR-0024).
///
/// Not configurable, deliberately: an operator cannot know what a number
/// here costs the binder, and a per-install limit would make one DSL text
/// admitted on one node and refused on another.
pub const MAX_LATERAL_EXPANSION: u64 = 512;

/// The most pipeline stages one query may carry (ADR-0024).
///
/// Counted over the WHOLE pipeline, including `from saved` and the stages
/// the executor later peels off to run in Rust — the search stage is not a
/// pipeline stage and carries no same-stage aliases.
pub const MAX_PIPELINE_STAGES: usize = 128;

/// The most contiguous ranges a `SEVERITY` set can render
/// ([`compare::severity_ranges`]): the twelve non-adjacent points the 1-24
/// ladder admits, plus the single representative every out-of-ladder point
/// collapses onto.
const MAX_SEVERITY_RUNS: usize = 13;

/// The node the `OVER (…)` wrapper adds to an `eventstats` output, before
/// its partition keys.
const EVENTSTATS_OVER_FIXED: u64 = 1;

// ---------------------------------------------------------------------------
// Weights and profiles
// ---------------------------------------------------------------------------

/// One expression's expanded weight and the additional substitution that
/// expansion caused.
///
/// Both halves saturate. A saturated `d` is over [`MAX_LATERAL_EXPANSION`]
/// on its own, so saturation needs no separate signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Weight {
    /// The expanded node count.
    pub w: u64,
    /// The additional substitution, propagated from the children.
    pub d: u64,
}

impl Weight {
    /// A field reference the stage does not define, a literal, a bound
    /// parameter: one node, nothing substituted.
    pub const LEAF: Self = Self { w: 1, d: 0 };

    /// A reference to an earlier output of the SAME stage, whose expanded
    /// weight is `target`: the target arrives whole in place of the name.
    #[must_use]
    pub fn substituted(target: u64) -> Self {
        Self {
            w: target,
            d: target.saturating_sub(1),
        }
    }

    /// The componentwise maximum — how the check combines two pin
    /// interpretations of one construct.
    ///
    /// Componentwise, and `d` carried rather than recomputed: the two
    /// maxima may describe different renderings, so any arithmetic across
    /// them (`w_max - w_unexpanded_max`) could name a rendering that does
    /// not exist and undercount the real one.
    #[must_use]
    pub fn max_with(self, other: Self) -> Self {
        Self {
            w: self.w.max(other.w),
            d: self.d.max(other.d),
        }
    }
}

/// How one rendering multiplies its children.
///
/// `fixed` is the node overhead translation adds on its own — calls,
/// casts, `CASE` scaffolding, inlined literals. `copies[i]` is how many
/// times child `i` lands in the emitted expression.
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

    /// `fixed` nodes over `n` children, each written once.
    fn uniform(fixed: u64, n: usize) -> Self {
        Self::new(fixed, vec![1; n])
    }

    /// Apply the profile to its children's weights.
    ///
    /// Total by construction: a child list shorter or longer than `copies`
    /// stops at the shorter of the two rather than indexing past either,
    /// because scoring must reach the emitter's own semantic refusal for a
    /// malformed call instead of panicking ahead of it.
    #[must_use]
    pub fn apply(&self, children: &[Weight]) -> Weight {
        let mut out = Weight {
            w: self.fixed,
            d: 0,
        };
        for (m, child) in self.copies.iter().zip(children) {
            out.w = out.w.saturating_add(m.saturating_mul(child.w));
            out.d = out.d.saturating_add(m.saturating_mul(child.d));
        }
        out
    }
}

// ---------------------------------------------------------------------------
// The rendering inventory
// ---------------------------------------------------------------------------

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
    /// A `SEVERITY` band reached through a MIXED `IN` list, where the
    /// per-element arm renders the band's own ranges. No single pin can
    /// produce that mixture, so this arm is defensive and takes the widest
    /// set the ladder can render.
    SeverityBand,
}

/// One DSL function's emitted shape, at the arity it was called with.
///
/// The variants are emission shapes, not names: everything that translates
/// to one wrapper node over each argument written once is [`Self::Plain`],
/// and a variant exists only where the emitter does something else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FunctionShape {
    /// One wrapper node over `n` arguments, each written once —
    /// `AVG(a)`, `IF(a, b, c)`, `COALESCE(a, …)`, `CASE WHEN … END`.
    Plain(usize),
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
    /// `sev(x[, dialect])`: the severity reading kernel, bound once, over
    /// the argument's canonical text form. The dialect argument is inlined
    /// text and never an operand.
    Sev { dialect: Dialect, argc: usize },
}

/// One `crate::conform` expression helper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConformShape {
    /// [`crate::conform::untyped_text`].
    UntypedText,
    /// [`crate::conform::decimal_reading`].
    DecimalReading,
    /// [`crate::conform::guarded_cast_in`] for one pin.
    GuardedCast(CanonicalType, Dialect),
    /// [`crate::conform::severity_reading_sql`] — the repeated-subject
    /// shape the conform rung emits.
    SeverityReading(Dialect),
    /// [`crate::conform::severity_reading_sql_bind_once`] — the shape
    /// `sev()` emits, whose subject may carry bound parameters.
    SeverityReadingBindOnce(Dialect),
    /// [`crate::conform::severity_token_text_sql`].
    SeverityTokenText,
}

/// Every construct whose emitted shape the check has to price.
///
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

/// The node overhead and child-copy counts one rendering contributes.
///
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
                PatternForm::Native => 0,
                // `CAST(target AS VARCHAR)`.
                PatternForm::BigIntText | PatternForm::BooleanText | PatternForm::DoubleText => 1,
                // `strftime(target, '<format>')`.
                PatternForm::Rfc3339Text => 2,
                PatternForm::SeverityText => severity_token_text_fixed(),
            };
            RenderProfile::new(target + 2, vec![1])
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
            //   TRY_CAST(t AS BIGINT) END)`.
            CanonicalType::BigInt => RenderProfile::new(6, vec![3]),
            // `(CASE WHEN CAST(TRY_CAST(t AS BOOLEAN) AS VARCHAR) = t THEN
            //   TRY_CAST(t AS BOOLEAN) END)`.
            CanonicalType::Boolean => RenderProfile::new(5, vec![3]),
            CanonicalType::Double => RenderProfile::new(1, vec![1]),
            // `TRY_CAST(TRY_CAST(t AS TIMESTAMPTZ) AS TIMESTAMP)`.
            CanonicalType::Timestamp => RenderProfile::new(2, vec![1]),
            CanonicalType::Severity => conform_profile(ConformShape::SeverityReading(dialect)),
        },
        ConformShape::SeverityReading(dialect) => {
            let case = reading_case_fixed(dialect);
            let subjects = reading_case_subjects(dialect);
            // `CAST(<case over trimmed(text)> AS BIGINT)`: the outer cast,
            // the case's own nodes, and one trim per writing of the
            // subject.
            RenderProfile::new(1 + case + subjects * TRIMMED_FIXED, vec![subjects])
        }
        ConformShape::SeverityReadingBindOnce(dialect) => {
            let case = reading_case_fixed(dialect);
            let subjects = reading_case_subjects(dialect);
            // `CAST(list_transform([trimmed(text)], _sev -> <case>)[1] AS
            // BIGINT)`: the cast, the transform, the list, the lambda and
            // its parameter, the index and its literal, one trim, the
            // case, and the lambda parameter once per writing.
            RenderProfile::new(7 + TRIMMED_FIXED + case + subjects, vec![1])
        }
        // `(CASE x WHEN 1 THEN 'trace' … WHEN 24 THEN 'fatal4' END)`.
        ConformShape::SeverityTokenText => RenderProfile::new(severity_token_text_fixed(), vec![1]),
    }
}

/// Nodes in `regexp_replace(regexp_replace(x, '…', ''), '…', '')` — the
/// whitespace trim both severity readings wrap their subject in.
const TRIMMED_FIXED: u64 = 6;

/// Nodes in `crate::conform`'s reading `CASE`, excluding the writings of
/// its subject.
///
/// Read off the severity tables rather than frozen, so a token added there
/// widens the profile instead of silently escaping it: the ASCII gate and
/// its literal, the `lower()` fold, the two `CASE` heads, two nodes per
/// arm, and the numeric arm the dialect chooses.
fn reading_case_fixed(dialect: Dialect) -> u64 {
    let arms = n_u64(severity_case_arms());
    let numeric = match dialect {
        // `(CASE WHEN cast BETWEEN 1 AND 24 THEN cast END)`: the `CASE`,
        // the ternary `BETWEEN`, its two bounds, and two casts.
        Dialect::Otel => 7,
        // `(CASE cast WHEN 0 THEN … WHEN 7 THEN … END)`, eight rungs.
        Dialect::Syslog => 18,
    };
    // Two `CASE` heads, `regexp_full_match` and its pattern, `lower()`,
    // and the numeric arm's own guard (`CASE`, match, pattern).
    5 + arms.saturating_mul(2) + 3 + numeric
}

/// How many times the reading `CASE` writes its (already trimmed) subject.
///
/// Five under `OTel` — the ASCII gate, the `lower()` fold, the digits
/// guard, and the numeric cast's two halves — and four under syslog, whose
/// numeric arm names the cast once.
fn reading_case_subjects(dialect: Dialect) -> u64 {
    match dialect {
        Dialect::Otel => 5,
        Dialect::Syslog => 4,
    }
}

/// The arms `crate::conform`'s reading `CASE` generates: every band token,
/// plus each `OTel` exact short name the token table does not already
/// carry.
fn severity_case_arms() -> usize {
    let table: Vec<&str> = crate::severity::token_entries().map(|(t, _)| t).collect();
    let extras = (1..=24u8)
        .filter(|n| crate::severity::otel_name(*n).is_some_and(|name| !table.contains(&name)))
        .count();
    table.len() + extras
}

/// Nodes in `crate::conform::severity_token_text_sql`: the `CASE` head and
/// two per ladder arm.
fn severity_token_text_fixed() -> u64 {
    1 + 24 * 2
}

// ---------------------------------------------------------------------------
// The refusal
// ---------------------------------------------------------------------------

/// Which limit a refusal names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    /// [`MAX_LATERAL_EXPANSION`].
    Lateral,
    /// [`MAX_PIPELINE_STAGES`], with the stage count the query carried.
    Stages(usize),
}

/// A query the expansion budget refuses, and the one sentence that says so.
///
/// One `Display` owner for both doors: the SQL lane carries it as
/// `EmitError::UnsupportedOperation` (which prefixes its own
/// `unsupported operation: `), the stream lane as
/// `StreamPlanError::TooComplex`, verbatim. The sentence names the stage
/// and the budget and no measured database node count — the profiles are
/// deliberately conservative, so a number here would be a claim the check
/// cannot make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComplexityRefusal {
    /// Zero-based index of the offending stage in the pipeline.
    pub stage_index: usize,
    /// The keyword the query spelled that stage with.
    pub stage: &'static str,
    /// The output whose scoring crossed the budget, rendered as DSL text.
    /// `None` where the stage has no output sequence, or where the name
    /// cannot be spelled in the DSL at all.
    pub target: Option<String>,
    /// Which limit was crossed.
    pub limit: Limit,
}

impl ComplexityRefusal {
    /// The advice that fits the stage that overflowed.
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

/// A name as it appears inside a refusal: its DSL spelling, sanitised,
/// always visually quoted.
///
/// [`quote_dsl_field`] declines a name the field grammar cannot spell
/// (empty, or carrying a control or invisible display character), and a
/// declined name is simply omitted — the stage alone still says where to
/// look, and a refusal an operator reads in a terminal must never carry
/// text the query author chose to hide a line with.
///
/// A name the bare production can spell is wrapped here so the message's
/// quoting is uniform, and a name that is ALREADY backticked is left as
/// the DSL must spell it rather than double-wrapped — the same rule
/// [`crate::projection`]'s collision messages use.
fn refusal_name(name: &str) -> Option<String> {
    let dsl = crate::sanitize::sanitize_display_text(&quote_dsl_field(name)?);
    Some(if dsl.starts_with('`') {
        dsl
    } else {
        format!("`{dsl}`")
    })
}

// ---------------------------------------------------------------------------
// The check
// ---------------------------------------------------------------------------

/// What one check visited — the evidence that the walk is linear in source
/// size rather than in expanded weight.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CheckStats {
    /// AST nodes entered, plus one per pin interpretation priced.
    pub visited_nodes: u64,
    /// Pipeline stages counted, against [`MAX_PIPELINE_STAGES`].
    pub stages: usize,
    /// The summed assignment delta, against [`MAX_LATERAL_EXPANSION`] —
    /// the headroom a query actually left, which is what the
    /// documentation fixtures record.
    pub lateral_delta: u64,
}

/// Admit a pipeline under the bind-time expansion budget (ADR-0024).
///
/// # Errors
///
/// [`ComplexityRefusal`] when the pipeline carries more than
/// [`MAX_PIPELINE_STAGES`] stages, or when its assignment deltas sum past
/// [`MAX_LATERAL_EXPANSION`].
pub fn check_pipeline_complexity(stages: &[Spanned<PipeStage>]) -> Result<(), ComplexityRefusal> {
    check_pipeline_complexity_with_stats(stages).0
}

/// [`check_pipeline_complexity`], with the walk's own accounting.
pub fn check_pipeline_complexity_with_stats(
    stages: &[Spanned<PipeStage>],
) -> (Result<(), ComplexityRefusal>, CheckStats) {
    let mut stats = CheckStats::default();

    // The stage cap first: a pipeline over it is refused for its length,
    // whatever its expressions cost.
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
///
/// Cleared at every stage, because a name defined in an earlier stage is
/// an incoming COLUMN by the time the next SELECT reads it, not a lateral
/// alias — one node, nothing substituted.
type Memo = HashMap<String, u64>;

/// Walk one stage's ordered outputs, charging each one's delta.
///
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
                memo.insert(catalog_key(target), weight.w);
            }
        }
        PipeStage::Stats(s) => {
            score_aggregations(index, keyword, &s.aggregations, 0, total, stats)?;
        }
        PipeStage::Timechart(t) => {
            score_aggregations(index, keyword, &t.aggregations, 0, total, stats)?;
        }
        PipeStage::EventStats(es) => {
            // Every output carries a mandatory `OVER (…)` wrapper.
            // `DuckDB` 1.5.5 happens to refuse a substituted nested window
            // before it binds the children, but validity here does not
            // depend on that refusal order staying put, so the outputs are
            // walked and charged like any other sequence.
            let window = EVENTSTATS_OVER_FIXED.saturating_add(n_u64(es.group_by.len()));
            score_aggregations(index, keyword, &es.aggregations, window, total, stats)?;
        }
        PipeStage::Pivot(p) => {
            // One USING expression and no output-alias sequence: its
            // delta is charged, and nothing in the stage can name it.
            let weight = score_agg(&p.aggregation, &Memo::new(), 0, stats);
            charge(index, keyword, None, weight.d, total)?;
        }
        PipeStage::Rename(r) => {
            // A rename substitutes a leaf, so its target carries one node
            // and no delta. Recorded all the same: the stage's own outputs
            // are what a same-stage reference would find.
            for (_, new) in &r.renames {
                memo.insert(catalog_key(new), 1);
            }
        }
        // No recursive output-alias sequence. Each arm is spelled out
        // rather than folded into a wildcard, so a stage that grows one
        // has to say so here.
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
        PipeStage::Where(_)
        | PipeStage::Sort(_)
        | PipeStage::Limit(_)
        | PipeStage::Table(_)
        | PipeStage::Top(_)
        | PipeStage::Rare(_)
        | PipeStage::Drop(_)
        | PipeStage::Dedup(_)
        | PipeStage::Tail(_)
        | PipeStage::Sample(_)
        | PipeStage::FromSaved(_) => {}
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
        memo.insert(catalog_key(&name), weight.w);
    }
    Ok(())
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

/// The keyword a stage was written with — the spelling the query used,
/// where the AST kept it.
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

// ---------------------------------------------------------------------------
// Expression scoring
// ---------------------------------------------------------------------------

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

/// Which rendering a call takes, at the arity it was written with.
///
/// A name outside the inventory takes [`Rendering::UnknownCall`], which is
/// benign by construction: the emitter and the stream compiler both refuse
/// an unknown function, and this walk only has to reach them.
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
        "sev" => FunctionShape::Sev {
            dialect: sev_dialect(args),
            argc: arity,
        },
        "count"
        | "avg"
        | "sum"
        | "min"
        | "max"
        | "dc"
        | "distinct_count"
        | "first"
        | "last"
        | "values"
        | "list"
        | "median"
        | "stddev"
        | "lower"
        | "upper"
        | "length"
        | "len"
        | "coalesce"
        | "if"
        | "replace"
        | "substr"
        | "trim"
        | "ltrim"
        | "rtrim"
        | "abs"
        | "ceil"
        | "ceiling"
        | "floor"
        | "round"
        | "typeof"
        | "tonumber"
        | "tostring"
        | "contains"
        | "startswith"
        | "endswith"
        | "concat"
        | "date_part"
        | "date_trunc"
        | "date_diff"
        | "strftime"
        | "strptime"
        | "case"
        | "json"
        | "json_extract"
        | "json_extract_string"
        | "json_valid"
        | "json_keys"
        | "json_array_length" => FunctionShape::Plain(arity),
        // `KNOWN_FUNCTIONS` grew a name this table does not price.
        // `every_known_function_has_a_profile` fails here, loudly, rather
        // than letting the new function score as something cheaper.
        _ => return Rendering::UnknownCall(arity),
    })
}

/// The dialect a `sev()` call reads its numerals in.
///
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

/// What a pin-aware comparison is comparing, as far as a corpus-blind
/// check can tell.
///
/// The classifier mirrors [`crate::pin_scope::PinScope::subject_pin`]'s
/// two admitted SHAPES without its catalog: a bare field reference is a
/// subject under EVERY pin (the check does not know which one applies, so
/// it prices them all), and a pin-declaring call carries its own answer.
#[derive(Debug, Clone, Copy)]
enum SubjectKind {
    /// A bare field reference: score under every supported pin.
    Bare,
    /// A call whose result type the function declares — `sev(level)`.
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

/// The pins one subject has to be priced under.
fn candidate_pins(kind: SubjectKind) -> Vec<Option<CanonicalType>> {
    match kind {
        SubjectKind::Declared(pin) => vec![Some(pin)],
        SubjectKind::Bare => std::iter::once(None)
            .chain(CanonicalType::ALL.into_iter().map(Some))
            .collect(),
    }
}

/// A bare literal operand, as [`compare::compare_form_bound`]'s door wants
/// it.
///
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

    if matches!(op, BinaryOp::Matches | BinaryOp::Like | BinaryOp::ILike) {
        // Only the LEFT operand of a pattern operator is a subject; the
        // right one is the pattern.
        let Some(kind) = subject_kind(lhs) else {
            return generic;
        };
        if !matches!(rhs.node, Expr::Literal(LiteralValue::String(_))) {
            return generic;
        }
        let mut best = generic;
        for pin in candidate_pins(kind) {
            stats.visited_nodes = stats.visited_nodes.saturating_add(1);
            let rendering = Rendering::Pattern(compare::pattern_form(pin));
            best = best.max_with(profile(&rendering).apply(&[left]));
        }
        return best;
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

/// The weight one resolved comparison form renders to over its subject.
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
        // An all-severity list is one merged range set: the subject once
        // per run.
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

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

    // ── the numeric boundary ──────────────────────────────────────────

    /// The exact 512/513 boundary, and the proof that the cap is on the
    /// LATERAL DELTA rather than on the total expanded weight: at 512
    /// references the delta is exactly the budget while `W` is more than
    /// twice it, and the query is admitted.
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
        let memo: Memo = [("x".to_string(), 2)].into_iter().collect();
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

    /// The same boundary written as DSL an operator could type, through
    /// `eval` rather than `let`, with the alias reached by a different
    /// case spelling.
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

    // ── the severity chain (ADR-0024's worked example) ─────────────────

    /// The chain the ADR records: a doubling seed, then eight assignments
    /// that each read the previous one through `sev(…) in (…)`.
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
                Expr::FieldRef(name) => memo
                    .get(&catalog_key(name))
                    .map_or(Weight::LEAF, |&t| Weight::substituted(t)),
                Expr::Unary { operand, .. } => {
                    let c = node(&operand.node, memo);
                    Weight { w: 1 + c.w, d: c.d }
                }
                Expr::Binary { lhs, rhs, .. } => {
                    let (l, r) = (node(&lhs.node, memo), node(&rhs.node, memo));
                    Weight {
                        w: 1 + l.w + r.w,
                        d: l.d + r.d,
                    }
                }
                Expr::FunctionCall { args, .. } => {
                    let mut out = Weight { w: 1, d: 0 };
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

    /// The reason ADR-0024 replaced that measure: the emitter writes
    /// twelve copies of the subject per assignment, so the chain is
    /// refused — at the SECOND link, long before the eighth.
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

    /// A twelve-point severity set writes its subject twelve times, read
    /// off the real range expansion rather than a frozen number.
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

    /// A deep chain is refused by traversing the SOURCE, never by building
    /// the expansion: the walk visits a bounded number of nodes, nothing
    /// overflows, and a debug build (where arithmetic overflow panics)
    /// finishes.
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

        // Every visit is either an AST node or one pin interpretation of a
        // construct, so the walk is linear in source size with a fixed
        // number of pin cases — not in the expanded weight, which is
        // astronomically larger.
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

    /// Saturation is over-limit, and nothing panics reaching it: a chain
    /// long enough to saturate `u64` is refused like any other.
    #[test]
    fn saturated_weights_are_over_limit() {
        let saturated = Weight {
            w: u64::MAX,
            d: u64::MAX,
        };
        let combined = profile(&Rendering::Binary).apply(&[saturated, saturated]);
        assert_eq!(combined.w, u64::MAX);
        assert_eq!(combined.d, u64::MAX);
        assert!(combined.d > MAX_LATERAL_EXPANSION);
    }

    // ── the stage cap ─────────────────────────────────────────────────

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

    /// The stage cap is checked BEFORE any expression, so a pipeline that
    /// is both too long and too expensive is refused for its length.
    #[test]
    fn the_stage_cap_wins_over_the_expansion_budget() {
        let mut dsl = severity_chain(8);
        dsl.push_str(&" | head 1".repeat(MAX_PIPELINE_STAGES));
        assert!(matches!(refusal(&dsl).limit, Limit::Stages(_)));
    }

    /// `from saved` counts like every other stage, so slicing it off
    /// later cannot buy a query one more stage.
    #[test]
    fn from_saved_counts_toward_the_stage_cap() {
        let dsl = format!("| from saved daily{}", " | head 1".repeat(128));
        assert!(matches!(refusal(&dsl).limit, Limit::Stages(129)));
    }

    // ── the alias table ───────────────────────────────────────────────

    /// The table resets at every stage: a name defined in one stage is an
    /// incoming COLUMN in the next, so reading it twice costs two leaves,
    /// not two expansions.
    #[test]
    fn the_alias_table_resets_between_stages() {
        let args = vec!["y"; 300].join(", ");
        let same_stage = format!("* | let x = coalesce({args}), z = x + x");
        assert!(matches!(refusal(&same_stage).limit, Limit::Lateral));

        let split = format!("* | let x = coalesce({args}) | let z = x + x");
        check(&split).expect("a split pipeline reads a finished column");
    }

    /// Names fold the way the catalog folds them, so a differently-cased
    /// reference is still the same output.
    #[test]
    fn alias_names_fold_ascii_case() {
        let args = vec!["a"; 513].join(", ");
        let refused = refusal(&format!("* | let A = abs(y), z = coalesce({args})"));
        assert_eq!(refused.target.as_deref(), Some("`z`"));

        let args = vec!["A"; 513].join(", ");
        let refused = refusal(&format!("* | let a = abs(y), z = coalesce({args})"));
        assert_eq!(refused.target.as_deref(), Some("`z`"));
    }

    /// A forward or self reference does not resolve through the table —
    /// it is a plain leaf, and the existing semantic refusal still owns
    /// that query.
    #[test]
    fn forward_and_self_references_score_as_leaves() {
        check("* | let a = b + b, b = 1").expect("a forward reference costs nothing here");
        check("* | let a = a + a").expect("a self reference costs nothing here");
    }

    // ── zero-delta shapes ─────────────────────────────────────────────

    /// An expression naming no earlier output contributes nothing, however
    /// large — a flat list, an independent assignment, a wide `stats`.
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

    /// Stages with no output-alias sequence charge nothing, whatever their
    /// expressions look like.
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

    // ── the aggregating stages ────────────────────────────────────────

    /// `stats` and `timechart` accept scalar heads, so they can express
    /// the same chains `let` can — under the names
    /// `projection::agg_output_name` derives, explicit or inferred.
    #[test]
    fn aggregating_stages_carry_their_own_alias_sequence() {
        let args = vec!["abs_y"; 513].join(", ");
        let refused = refusal(&format!("* | stats abs(y) as abs_y, coalesce({args}) as z"));
        assert_eq!(refused.stage, "stats");
        assert_eq!(refused.target.as_deref(), Some("`z`"));

        // The inferred name is the derivation every lane reads, so an
        // un-aliased head is reachable by the name it projects.
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

    /// The `eventstats` window wrapper is charged conservatively, so its
    /// outputs weigh at least as much as the plain `stats` ones.
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

    // ── hand-calculated profiles ──────────────────────────────────────

    #[test]
    fn hand_calculated_profiles() {
        assert_eq!(profile(&Rendering::Leaf), RenderProfile::new(1, vec![]));
        assert_eq!(
            profile(&Rendering::Binary),
            RenderProfile::new(1, vec![1, 1])
        );
        assert_eq!(profile(&Rendering::Unary), RenderProfile::new(1, vec![1]));
        // `(x IN (a, b, c))`: the `IN` node, the subject and three
        // elements.
        assert_eq!(
            profile(&Rendering::InListPlain(3)),
            RenderProfile::new(1, vec![1, 1, 1, 1])
        );
        // `x IN (?, ?, ?)`: the `IN` node and three parameters over one
        // writing of the subject.
        assert_eq!(
            profile(&Rendering::InListBound(3)),
            RenderProfile::new(4, vec![1])
        );
        // The VARCHAR pin's numeric equality names its subject twice.
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
        // `COUNT(*)` has no operands at all.
        assert_eq!(
            profile(&Rendering::Function(FunctionShape::CountStar)),
            RenderProfile::new(2, vec![])
        );
        // `(a IS NULL)` and `(a IS NOT NULL)`.
        assert_eq!(
            profile(&Rendering::Function(FunctionShape::IsNull)),
            RenderProfile::new(2, vec![1])
        );
        assert_eq!(
            profile(&Rendering::Function(FunctionShape::IsNotNull)),
            RenderProfile::new(3, vec![1])
        );
        // `ABS(a)`.
        assert_eq!(
            profile(&Rendering::Function(FunctionShape::Plain(1))),
            RenderProfile::new(1, vec![1])
        );
        // `ROUND(a, 2)`: the precision literal is inlined, never copied.
        assert_eq!(
            profile(&Rendering::Function(FunctionShape::RoundPrecision)),
            RenderProfile::new(2, vec![1, 0])
        );
        // `STRING_SPLIT(a, b)[i + 1]`.
        assert_eq!(
            profile(&Rendering::Function(FunctionShape::Split)),
            RenderProfile::new(3, vec![1, 1, 0])
        );
        // `CAST(? AS TIMESTAMP)` — no operands at all.
        assert_eq!(
            profile(&Rendering::Function(FunctionShape::Now)),
            RenderProfile::new(2, vec![])
        );
        // `json_extract_string(to_json(x), '$')`.
        assert_eq!(
            profile(&Rendering::Conform(ConformShape::UntypedText)),
            RenderProfile::new(3, vec![1])
        );
        // The BIGINT rung names its text three times.
        assert_eq!(
            profile(&Rendering::Conform(ConformShape::GuardedCast(
                CanonicalType::BigInt,
                Dialect::Otel
            ))),
            RenderProfile::new(6, vec![3])
        );
        // The VARCHAR rung IS the text.
        assert_eq!(
            profile(&Rendering::Conform(ConformShape::GuardedCast(
                CanonicalType::Varchar,
                Dialect::Otel
            ))),
            RenderProfile::new(0, vec![1])
        );
        // The ladder's token text: the `CASE` head and two nodes per arm.
        assert_eq!(
            profile(&Rendering::Conform(ConformShape::SeverityTokenText)),
            RenderProfile::new(49, vec![1])
        );
    }

    /// The reading `CASE` writes its subject five times under `OTel` and
    /// four under syslog — the counts `crate::conform` documents, read
    /// back off the profile rather than restated.
    #[test]
    fn the_severity_reading_profiles_match_their_documented_shapes() {
        let repeated = profile(&Rendering::Conform(ConformShape::SeverityReading(
            Dialect::Otel,
        )));
        assert_eq!(repeated.copies, vec![5]);
        let syslog = profile(&Rendering::Conform(ConformShape::SeverityReading(
            Dialect::Syslog,
        )));
        assert_eq!(syslog.copies, vec![4]);
        // The bind-once shape names its subject exactly once, which is why
        // `sev()` can carry bound parameters.
        let once = profile(&Rendering::Conform(ConformShape::SeverityReadingBindOnce(
            Dialect::Otel,
        )));
        assert_eq!(once.copies, vec![1]);
        // Twenty band tokens plus the eighteen exact names the table does
        // not already carry.
        assert_eq!(severity_case_arms(), 38);
    }

    /// The severity set can never render more runs than the ladder admits
    /// non-adjacent points, plus the one representative every out-of-ladder
    /// point collapses onto.
    #[test]
    fn the_severity_run_ceiling_is_thirteen() {
        let mut points: Vec<i64> = (1..=24).step_by(2).collect();
        points.push(-5);
        points.push(9_000);
        assert_eq!(compare::severity_ranges(&points).len(), MAX_SEVERITY_RUNS);
    }

    // ── the function inventory ────────────────────────────────────────

    /// Every function the DSL knows has a profile. A new entry in
    /// `KNOWN_FUNCTIONS` without one lands on the placeholder, and this
    /// test is what makes that loud.
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

    /// A name outside the inventory scores structurally and hands the
    /// query on: this check never pre-empts the emitter's own
    /// unknown-function refusal.
    #[test]
    fn an_unknown_call_scores_structurally() {
        let args = vec![Spanned::new(Expr::FieldRef("x".to_string()), 0..0)];
        assert_eq!(call_rendering("nosuchfn", &args), Rendering::UnknownCall(1));
        check("* | let a = nosuchfn(x)").expect("the emitter owns that refusal");
    }

    /// A severity token the rule table refuses is skipped as an
    /// interpretation, not turned into a complexity refusal — the lanes
    /// own that sentence.
    #[test]
    fn an_unknown_severity_token_is_not_a_complexity_refusal() {
        check(r#"* | where _severity == "wat""#).expect("the lanes own the vocabulary error");
    }

    // ── the message ───────────────────────────────────────────────────

    /// The sentence names the stage and the budget, offers the remedy that
    /// fits the stage, and claims no measured database node count.
    #[test]
    fn the_refusal_sentence_names_the_stage_the_budget_and_a_remedy() {
        let args = vec!["x"; 513].join(", ");
        let text = refusal(&format!("* | let x = abs(y), z = coalesce({args})")).to_string();
        assert!(text.contains("pipeline stage 1 (`let`)"), "{text}");
        assert!(text.contains("512"), "{text}");
        assert!(text.contains("`z`"), "{text}");
        assert!(text.contains("separate `| let` stages"), "{text}");
        // Not the parser's depth sentence, and no count of database nodes.
        assert!(!text.contains("nests deeper"), "{text}");
        assert!(!text.contains("nodes"), "{text}");

        let text = refusal(&format!("* | stats abs(y) as x, coalesce({args}) as z")).to_string();
        assert!(text.contains("(`stats`)"), "{text}");
        assert!(text.contains("expression of its own"), "{text}");
    }

    /// A name the field grammar cannot spell is omitted; the stage still
    /// says where to look.
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

    // ── the documented queries ────────────────────────────────────────

    /// Every query trawl DOCUMENTS stays inside the budget (ADR-0024).
    ///
    /// The caps are numbers picked against a binder's behaviour, not
    /// against the DSL's expressiveness, so the obligation runs the other
    /// way: a limit that refused an example the reference tells operators
    /// to type would be the wrong limit. This walks the fenced code in the
    /// DSL and CLI references, parses what parses, and admits it.
    ///
    /// What it skips, and why:
    ///
    /// - **fences in another language.** A `toml` block is configuration
    ///   and a `bash` block is shell; the DSL inside a `bash` block is
    ///   recovered from the quoted argument of `trawl query` /
    ///   `trawl validate`, which is where the CLI reference keeps it.
    /// - **lines the prose labels as errors.** The reference shows what a
    ///   refused query looks like. Those mostly fail to parse and drop out
    ///   on their own; the ones that parse are semantic refusals, not
    ///   budget ones, and stay in.
    /// - **anything that does not parse.** Placeholders
    ///   (`[search stage] | …`), sample output and prose fragments are not
    ///   queries.
    const DOCS: &[&str] = &[
        "docs/src/content/docs/reference/dsl.md",
        "docs/src/content/docs/reference/cli.md",
    ];

    /// The reference deliberately shows refused text. A line the prose marks
    /// that way is not a fixture.
    const NEGATIVE_MARKERS: &[&str] = &["error at the"];

    /// Pull every candidate query out of one markdown file's fenced blocks.
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
            // The CLI reference carries its DSL inside a quoted argument.
            return lines.iter().filter_map(|l| quoted_argument(l)).collect();
        }
        if !lang.is_empty() {
            // toml, json and friends are not queries.
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

    /// The first double-quoted argument on a shell line, if the command is one
    /// that takes DSL.
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

        // Observed maxima at the time of writing, on the two reference pages:
        //   documented queries admitted: 61
        //   longest pipeline:            5 stages   (of 128)
        //   largest lateral delta:       0          (of 512)
        //
        // Zero is not an accident. No documented example names an earlier
        // output of the same stage — the pattern the budget exists for does
        // not appear in the reference at all, which is why the caps can be
        // this low without touching anything trawl tells people to write.
        assert!(max_stages <= MAX_PIPELINE_STAGES / 4, "{max_stages} stages");
        assert!(
            max_delta <= MAX_LATERAL_EXPANSION / 4,
            "{max_delta} lateral delta, from: {widest}"
        );
    }

    /// A backticked name comes back out of the message exactly as the
    /// DSL has to spell it.
    #[test]
    fn a_quoted_target_renders_as_dsl_text() {
        let args = vec!["`a b`"; 513].join(", ");
        let refused = refusal(&format!("* | let `a b` = abs(y), `z z` = coalesce({args})"));
        assert_eq!(refused.target.as_deref(), Some("`z z`"));
        parser::parse(&format!("* | let {} = 1", refused.target.unwrap()))
            .expect("the name the message prints parses back");
    }
}
